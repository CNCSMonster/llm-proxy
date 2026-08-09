//! Proxy submodule extracted from `proxy::mod`.

use super::*;
use anyhow::bail;
use async_stream::stream;
use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;
use std::convert::Infallible;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) async fn aggregate_responses_sse_to_value(
    state: &AppState,
    upstream: reqwest::Response,
) -> Result<Value> {
    use futures_util::StreamExt;
    let max_buffer_bytes = state.cfg.server.max_sse_buffer_bytes;
    let max_output_items = state.cfg.server.max_output_items;
    let mut upstream_stream = upstream.bytes_stream();
    let mut buffer = String::new();
    let mut response_id = String::new();
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    // Aggregated output items keyed by upstream output_index (preserves order).
    let mut items: BTreeMap<usize, Value> = BTreeMap::new();
    // item_id → output_index, for routing delta/done events to their item.
    let mut item_index: HashMap<String, usize> = HashMap::new();
    // Upstream final status: "completed" (default) or "incomplete" (truncated).
    let mut status = "completed".to_string();
    let mut incomplete_details: Option<Value> = None;
    // Upstream failure message when the stream reports response.failed / error.
    let mut failure: Option<String> = None;

    while let Some(chunk) = upstream_stream.next().await {
        let chunk = match chunk {
            Ok(bytes) => bytes,
            Err(_) => break,
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // 聚合器内存保护：缓冲超限视为上游异常
        if buffer.len() > max_buffer_bytes {
            bail!(
                "upstream SSE stream exceeds {} bytes buffer limit",
                max_buffer_bytes
            );
        }
        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].trim_end_matches('\r').to_string();
            buffer.drain(..=pos);
            if !line.starts_with("data:") {
                continue;
            }
            let data = line.trim_start_matches("data:").trim();
            if data == "[DONE]" {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(data) {
                let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
                match event_type {
                    "response.created" => {
                        if let Some(id) = value
                            .get("response")
                            .and_then(|r| r.get("id"))
                            .and_then(Value::as_str)
                        {
                            response_id = id.to_string();
                        }
                    }
                    "response.output_item.added" => {
                        // 聚合器内存保护：output item 数量上限
                        if items.len() >= max_output_items {
                            bail!(
                                "upstream response exceeds {} output items limit",
                                max_output_items
                            );
                        }
                        if let Some(item) = value.get("item").and_then(Value::as_object) {
                            let output_index = value
                                .get("output_index")
                                .and_then(Value::as_u64)
                                .unwrap_or(0)
                                as usize;
                            let item_id = item
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                                "function_call" => {
                                    let call_id = item
                                        .get("call_id")
                                        .and_then(Value::as_str)
                                        .unwrap_or(&item_id)
                                        .to_string();
                                    let name = item
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    items.insert(
                                        output_index,
                                        json!({
                                            "type": "function_call",
                                            "call_id": call_id,
                                            "name": name,
                                            "arguments": "",
                                        }),
                                    );
                                }
                                "message" => {
                                    let role = item
                                        .get("role")
                                        .and_then(Value::as_str)
                                        .unwrap_or("assistant");
                                    // 按 added item 的 content part 初始化（多 part 支持）；
                                    // output_text/text part 文本置空，其余 part 原样保留；
                                    // content 缺失或为空时兜底单 part。
                                    let content: Vec<Value> = item
                                        .get("content")
                                        .and_then(Value::as_array)
                                        .map(|parts| {
                                            parts
                                                .iter()
                                                .map(|part| {
                                                    let mut p = part.clone();
                                                    if matches!(
                                                        p.get("type").and_then(Value::as_str),
                                                        Some("output_text") | Some("text")
                                                    ) {
                                                        p["text"] = json!("");
                                                    }
                                                    p
                                                })
                                                .collect()
                                        })
                                        .filter(|parts: &Vec<Value>| !parts.is_empty())
                                        .unwrap_or_else(|| {
                                            vec![json!({"type": "output_text", "text": ""})]
                                        });
                                    items.insert(
                                        output_index,
                                        json!({
                                            "type": "message",
                                            "role": role,
                                            "content": content,
                                        }),
                                    );
                                }
                                "reasoning" => {
                                    // 聚合 reasoning item：下游 convert 层
                                    // （responses→anthropic）会把 summary 转成 thinking 块。
                                    items.insert(
                                        output_index,
                                        json!({
                                            "type": "reasoning",
                                            "id": item_id,
                                            "summary": [{
                                                "type": "summary_text",
                                                "text": "",
                                            }],
                                            "signature": "",
                                        }),
                                    );
                                }
                                _ => {}
                            }
                            if !item_id.is_empty() {
                                item_index.insert(item_id, output_index);
                            }
                        }
                    }
                    "response.output_text.delta" => {
                        if let Some(item_id) = value.get("item_id").and_then(Value::as_str)
                            && let Some(delta) = value.get("delta").and_then(Value::as_str)
                            && let Some(&index) = item_index.get(item_id)
                            && let Some(item) = items.get_mut(&index)
                        {
                            let part_index = value
                                .get("content_index")
                                .and_then(Value::as_u64)
                                .unwrap_or(0) as usize;
                            let content = item["content"].as_array_mut().unwrap();
                            // 防御：上游可能跳过 added 的 part 声明直接发 delta
                            while content.len() <= part_index {
                                content.push(json!({"type": "output_text", "text": ""}));
                            }
                            if let Value::String(text) = &mut content[part_index]["text"] {
                                text.push_str(delta);
                            }
                        }
                    }
                    "response.output_text.done" => {
                        // Symmetric to function_call_arguments.done: fall back to
                        // the complete text only when no delta was accumulated.
                        if let Some(item_id) = value.get("item_id").and_then(Value::as_str)
                            && let Some(text) = value.get("text").and_then(Value::as_str)
                            && let Some(&index) = item_index.get(item_id)
                            && let Some(item) = items.get_mut(&index)
                        {
                            let part_index = value
                                .get("content_index")
                                .and_then(Value::as_u64)
                                .unwrap_or(0) as usize;
                            if let Some(content) = item["content"].as_array_mut()
                                && part_index < content.len()
                                && let Value::String(cur) = &mut content[part_index]["text"]
                                && cur.is_empty()
                            {
                                cur.push_str(text);
                            }
                        }
                    }
                    "response.reasoning_summary_text.delta" => {
                        if let Some(item_id) = value.get("item_id").and_then(Value::as_str)
                            && let Some(delta) = value.get("delta").and_then(Value::as_str)
                            && let Some(&index) = item_index.get(item_id)
                            && let Some(item) = items.get_mut(&index)
                            && item.get("type").and_then(Value::as_str) == Some("reasoning")
                            && let Some(summary) =
                                item.get_mut("summary").and_then(Value::as_array_mut)
                            && !summary.is_empty()
                            && let Value::String(text) = &mut summary[0]["text"]
                        {
                            text.push_str(delta);
                        }
                    }
                    "response.reasoning_summary_text.done" => {
                        // 空时兜底（与 output_text.done 对称）
                        if let Some(item_id) = value.get("item_id").and_then(Value::as_str)
                            && let Some(text) = value.get("text").and_then(Value::as_str)
                            && let Some(&index) = item_index.get(item_id)
                            && let Some(item) = items.get_mut(&index)
                            && item.get("type").and_then(Value::as_str) == Some("reasoning")
                            && let Some(summary) =
                                item.get_mut("summary").and_then(Value::as_array_mut)
                            && !summary.is_empty()
                            && let Value::String(cur) = &mut summary[0]["text"]
                            && cur.is_empty()
                        {
                            cur.push_str(text);
                        }
                    }
                    "response.output_item.done" => {
                        // 兜底：若上游只发 item 级 done 而无字段级 delta/done，
                        // 用完整 item 填充空内容（text/arguments/summary/signature）。
                        if let Some(item) = value.get("item").and_then(Value::as_object)
                            && let Some(&index) = item
                                .get("id")
                                .and_then(Value::as_str)
                                .and_then(|id| item_index.get(id))
                            && let Some(target) = items.get_mut(&index)
                        {
                            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                                "message" => {
                                    if let (Some(done_parts), Some(cur_parts)) = (
                                        item.get("content").and_then(Value::as_array),
                                        target.get_mut("content").and_then(Value::as_array_mut),
                                    ) {
                                        for (i, part) in done_parts.iter().enumerate() {
                                            if i >= cur_parts.len() {
                                                cur_parts.push(part.clone());
                                                continue;
                                            }
                                            let cur_text = cur_parts[i]
                                                .get("text")
                                                .and_then(Value::as_str)
                                                .unwrap_or("");
                                            if cur_text.is_empty()
                                                && let Some(text) =
                                                    part.get("text").and_then(Value::as_str)
                                            {
                                                cur_parts[i]["text"] = json!(text);
                                            }
                                        }
                                    }
                                }
                                "function_call" => {
                                    if target["arguments"].as_str().unwrap_or("").is_empty()
                                        && let Some(args) =
                                            item.get("arguments").and_then(Value::as_str)
                                    {
                                        target["arguments"] = json!(args);
                                    }
                                }
                                "reasoning" => {
                                    if let (Some(done_summary), Some(cur_summary)) = (
                                        item.get("summary").and_then(Value::as_array),
                                        target.get_mut("summary").and_then(Value::as_array_mut),
                                    ) {
                                        for (i, part) in done_summary.iter().enumerate() {
                                            if i >= cur_summary.len() {
                                                cur_summary.push(part.clone());
                                                continue;
                                            }
                                            let cur_text = cur_summary[i]
                                                .get("text")
                                                .and_then(Value::as_str)
                                                .unwrap_or("");
                                            if cur_text.is_empty()
                                                && let Some(text) =
                                                    part.get("text").and_then(Value::as_str)
                                            {
                                                cur_summary[i]["text"] = json!(text);
                                            }
                                        }
                                    }
                                    if target
                                        .get("signature")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .is_empty()
                                        && let Some(sig) =
                                            item.get("signature").and_then(Value::as_str)
                                    {
                                        target["signature"] = json!(sig);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        if let Some(item_id) = value.get("item_id").and_then(Value::as_str)
                            && let Some(delta) = value.get("delta").and_then(Value::as_str)
                            && let Some(&index) = item_index.get(item_id)
                            && let Some(item) = items.get_mut(&index)
                            && let Value::String(args) = &mut item["arguments"]
                        {
                            args.push_str(delta);
                        }
                    }
                    "response.function_call_arguments.done" => {
                        // Prefix-protected takeover, aligned with CLIProxyAPI
                        // updateCodexFunctionCallArguments: the done event carries
                        // the complete arguments. If delta accumulation is empty,
                        // take the done value directly. If accumulation exists and
                        // done extends it (done starts_with accumulated), take done.
                        // If done does NOT extend the accumulation, the upstream
                        // event stream is inconsistent — surface it as an error
                        // instead of silently trusting either side.
                        if let Some(item_id) = value.get("item_id").and_then(Value::as_str)
                            && let Some(args) = value.get("arguments").and_then(Value::as_str)
                            && let Some(&index) = item_index.get(item_id)
                            && let Some(item) = items.get_mut(&index)
                            && item.get("type").and_then(Value::as_str) == Some("function_call")
                        {
                            let accumulated = item["arguments"].as_str().unwrap_or("");
                            if accumulated.is_empty() || args.starts_with(accumulated) {
                                item["arguments"] = json!(args);
                            } else {
                                failure = Some(format!(
                                    "function_call_arguments.done does not extend accumulated delta \
                                     (item {item_id})"
                                ));
                            }
                        }
                    }
                    "response.completed" => {
                        if let Some(usage) = value.get("response").and_then(|r| r.get("usage")) {
                            input_tokens = usage
                                .get("input_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            output_tokens = usage
                                .get("output_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                        }
                    }
                    "response.incomplete" => {
                        // Upstream truncated the output (max tokens, safety, etc.).
                        // Preserve the status so downstream conversion maps it to
                        // finish_reason=length / stop_reason=max_tokens instead of
                        // reporting a normal completion.
                        status = "incomplete".to_string();
                        incomplete_details = value
                            .get("response")
                            .and_then(|r| r.get("incomplete_details"))
                            .cloned();
                        // Usage may be present on the incomplete event too.
                        if let Some(usage) = value.get("response").and_then(|r| r.get("usage")) {
                            input_tokens = usage
                                .get("input_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            output_tokens = usage
                                .get("output_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                        }
                    }
                    "response.failed" => {
                        // Upstream reported a hard failure (content filter, server
                        // error, auth). Surface it instead of fabricating a
                        // completed response.
                        if let Some(msg) = value
                            .get("response")
                            .and_then(|r| r.get("error"))
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                        {
                            failure = Some(msg.to_string());
                        } else {
                            failure = Some("upstream stream reported response.failed".to_string());
                        }
                    }
                    "error" => {
                        if let Some(msg) = value.get("message").and_then(Value::as_str) {
                            failure = Some(msg.to_string());
                        } else {
                            failure = Some("upstream stream reported error".to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(failure_msg) = failure {
        bail!("upstream failed: {failure_msg}");
    }

    let mut output: Vec<Value> = items.into_values().collect();
    if output.is_empty() {
        output.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "",
            }],
        }));
    }

    let mut value = json!({
        "id": if response_id.is_empty() { format!("resp_{}", unix_millis()) } else { response_id },
        "object": "response",
        "created_at": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        "status": status,
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens
        }
    });
    if let Some(details) = incomplete_details {
        value["incomplete_details"] = details;
    }
    Ok(value)
}

/// Response-shaped wrapper around `aggregate_responses_sse_to_value`: records
/// usage and rewrites the model to the frontend model.
pub(super) async fn aggregate_responses_sse_to_json(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
) -> Response {
    let start_time = std::time::Instant::now();
    match aggregate_responses_sse_to_value(state, upstream).await {
        Ok(mut value) => {
            let latency_ms = start_time.elapsed().as_millis() as i64;
            record_usage_from_responses_value(
                state,
                &value,
                &frontend_model,
                &provider_id,
                &endpoint,
                Some(latency_ms),
            );
            value["model"] = json!(frontend_model);
            Json(value).into_response()
        }
        Err(err) => json_error(StatusCode::BAD_GATEWAY, &format!("upstream failure: {err}")),
    }
}

pub(super) async fn passthrough_sse(
    state: &AppState,
    direction: &'static str,
    upstream: reqwest::Response,
    provider_id: String,
    endpoint: String,
    frontend_model: String,
) -> Response {
    let mut upstream_stream = upstream.bytes_stream();
    let state_clone = state.clone();
    let stream = stream! {
        let mut buffer = String::new();
        let mut input_tokens: i64 = 0;
        let mut output_tokens: i64 = 0;
        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    state_clone.observe_stream_interruption(direction, &err.to_string());
                    yield Ok::<Bytes, Infallible>(Bytes::from(format!(
                        "event: error\ndata: {{\"error\":\"stream error: {err}\"}}\n\n"
                    )));
                    return;
                }
            };
            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);
            // Extract usage from SSE data lines while forwarding everything
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if line.starts_with("data:") {
                    let data = line.trim_start_matches("data:").trim();
                    if data != "[DONE]"
                        && let Ok(value) = serde_json::from_str::<Value>(data) {
                            // OpenAI Chat: usage in chunk directly
                            if let Some(usage) = value.get("usage") {
                                input_tokens = usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(input_tokens);
                                output_tokens = usage.get("completion_tokens").and_then(Value::as_i64).unwrap_or(output_tokens);
                            }
                            // Anthropic: usage in message_start (input) and message_delta (output)
                            if let Some(msg) = value.get("message").and_then(|m| m.get("usage")) {
                                input_tokens = msg.get("input_tokens").and_then(Value::as_i64).unwrap_or(input_tokens);
                            }
                            if value.get("type").and_then(Value::as_str) == Some("message_delta")
                                && let Some(usage) = value.get("usage") {
                                    output_tokens = usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(output_tokens);
                                }
                        }
                }
            }
            yield Ok(Bytes::from(text.into_owned()));
        }
        if !buffer.is_empty() {
            yield Ok(Bytes::from(buffer));
        }
        // Record usage at stream end
        if input_tokens > 0 || output_tokens > 0 {
            state_clone.record_usage(frontend_model, provider_id, endpoint, input_tokens, output_tokens, None);
        }
    };
    sse_response(Body::from_stream(stream))
}

pub(super) async fn responses_native_sse_rewrite_model(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
) -> Response {
    let mut upstream_stream = upstream.bytes_stream();
    let state_clone = state.clone();
    let body_stream = stream! {
        let mut buffer = String::new();
        let mut input_tokens: i64 = 0;
        let mut output_tokens: i64 = 0;
        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    yield Ok::<Bytes, Infallible>(responses_event("response.error", json!({
                        "type": "response.error",
                        "error": {"type": "upstream_error", "message": format!("stream disconnected: {err}")}
                    })));
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if !line.starts_with("data:") {
                    yield Ok(Bytes::from(format!("{line}\n")));
                    continue;
                }
                let data = line.trim_start_matches("data:").trim();
                if data == "[DONE]" {
                    yield Ok(Bytes::from("data: [DONE]\n"));
                    continue;
                }
                match serde_json::from_str::<Value>(data) {
                    Ok(mut value) => {
                        // Extract usage from response.completed
                        if value.get("type").and_then(Value::as_str) == Some("response.completed")
                            && let Some(usage) = value.get("response").and_then(|r| r.get("usage")) {
                                input_tokens = usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(input_tokens);
                                output_tokens = usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(output_tokens);
                            }
                        rewrite_model_fields(&mut value, &frontend_model);
                        yield Ok(Bytes::from(format!("data: {value}\n")));
                    }
                    Err(_) => yield Ok(Bytes::from(format!("{line}\n"))),
                }
            }
        }
        if !buffer.is_empty() {
            yield Ok(Bytes::from(buffer));
        }
        if input_tokens > 0 || output_tokens > 0 {
            state_clone.record_usage(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, None);
        }
    };
    sse_response(Body::from_stream(body_stream))
}

#[derive(Debug, Clone)]
struct StreamToolCallState {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
    output_index: usize,
}

pub(super) async fn antigravity_sse_to_responses_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
) -> Response {
    let now = unix_millis();
    let response_id = format!("resp_{now}");
    let message_id = format!("msg_{now}");
    let mut upstream_stream = upstream.bytes_stream();
    let state = state.clone();
    let body_stream = stream! {
        yield Ok::<Bytes, Infallible>(responses_event("response.created", json!({
            "type": "response.created",
            "response": {"id": response_id, "object": "response", "status": "in_progress", "model": frontend_model, "output": []}
        })));
        yield Ok(responses_event("response.output_item.added", json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"id": message_id, "type": "message", "role": "assistant", "status": "in_progress", "content": []}
        })));
        yield Ok(responses_event("response.content_part.added", json!({
            "type": "response.content_part.added",
            "item_id": message_id,
            "output_index": 0,
            "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []}
        })));
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut usage = json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0});
        let mut tool_calls: Vec<StreamToolCallState> = Vec::new();
        let mut next_output_index = 1usize;
        let mut collected_thought_sigs: Vec<(String, String)> = Vec::new();
        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    state.observe_stream_interruption("antigravity-to-responses", &err.to_string());
                    yield Ok(responses_event("response.error", json!({"type":"response.error","error":{"type":"upstream_error","message":format!("stream disconnected: {err}")}})));
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if !line.starts_with("data:") { continue; }
                let data = line.trim_start_matches("data:").trim();
                if data == "[DONE]" { continue; }
                let Ok(value) = serde_json::from_str::<Value>(data) else { continue; };
                let parsed = convert::antigravity_stream_chunk(&value);
                // Collect (name, signature) pairs from streaming chunks. Keys
                // are resolved after the stream, once full args are assembled.
                if !parsed.signature_pairs.is_empty() {
                    collected_thought_sigs.extend(parsed.signature_pairs.clone());
                }
                if parsed.total_tokens > 0 {
                    usage = json!({"input_tokens": parsed.prompt_tokens, "output_tokens": parsed.output_tokens, "total_tokens": parsed.total_tokens});
                }
                if !parsed.text.is_empty() {
                    full_text.push_str(&parsed.text);
                    yield Ok(responses_event("response.output_text.delta", json!({
                        "type": "response.output_text.delta",
                        "item_id": message_id,
                        "output_index": 0,
                        "content_index": 0,
                        "delta": parsed.text
                    })));
                }
                for call in parsed.function_calls {
                    // Each streaming functionCall chunk carries the COMPLETE
                    // args JSON (verified: antigravity emits full function
                    // calls, never cross-chunk argument fragments — see Go
                    // forward_responses_to_antigravity.go). So every chunk is
                    // an independent tool call; merging by name would corrupt
                    // same-named calls in one stream.
                    let item_id = format!("fc_{}", next_output_index);
                    let output_index = next_output_index;
                    next_output_index += 1;
                    tool_calls.push(StreamToolCallState {
                        id: item_id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        output_index,
                    });
                    let item = json!({
                        "id": item_id,
                        "type": "function_call",
                        "call_id": item_id,
                        "name": call.name,
                        "arguments": "",
                        "status": "in_progress"
                    });
                    yield Ok(responses_event("response.output_item.added", json!({"type":"response.output_item.added", "output_index": output_index, "item": item})));
                    if !call.arguments.is_empty() {
                        yield Ok(responses_event("response.function_call_arguments.delta", json!({"type":"response.function_call_arguments.delta", "item_id": item_id, "output_index": output_index, "delta": call.arguments})));
                    }
                }
            }
        }
        yield Ok(responses_event("response.output_text.done", json!({"type":"response.output_text.done", "item_id": message_id, "output_index": 0, "content_index": 0, "text": full_text})));
        let message_item = json!({"id": message_id, "type": "message", "role": "assistant", "status": "completed", "content": [{"type":"output_text", "text": full_text, "annotations": []}]});
        yield Ok(responses_event("response.output_item.done", json!({"type":"response.output_item.done", "output_index": 0, "item": message_item})));
        let mut output = vec![message_item];
        for call in &tool_calls {
            let item = json!({
                "id": call.id,
                "type": "function_call",
                "call_id": call.id,
                "name": call.name,
                "arguments": call.arguments,
                "status": "completed"
            });
            yield Ok(responses_event("response.function_call_arguments.done", json!({"type":"response.function_call_arguments.done", "item_id": call.id, "output_index": call.output_index, "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or("")})));
            yield Ok(responses_event("response.output_item.done", json!({"type":"response.output_item.done", "output_index": call.output_index, "item": item})));
            output.push(item);
        }
        // Resolve collected (name, signature) pairs against the fully
        // assembled tool_calls and cache them keyed by the complete function
        // name+args. Streaming args arrive in fragments, so keying here —
        // after all chunks are joined — is the only way to match what the
        // client will replay next turn.
        if !collected_thought_sigs.is_empty() && !tool_calls.is_empty() {
            let mut queue = state.thought_sig_queue.lock().unwrap();
            // Each collected signature belongs to the next matching tool_call
            // in arrival order; advance per-name so repeated calls pair up.
            let mut next_index: std::collections::HashMap<String, usize> = Default::default();
            for (name, signature) in collected_thought_sigs {
                let start = next_index.get(&name).copied().unwrap_or(0);
                let Some(slot) = tool_calls.iter().enumerate().skip(start).find(|(_, tc)| tc.name == name) else {
                    continue;
                };
                next_index.insert(name.clone(), slot.0 + 1);
                let key = crate::convert::signature_key_from_name_args(&name, &slot.1.arguments);
                queue.insert(key, signature);
            }
            if queue.len() > THOUGHT_SIGNATURE_MAP_MAX {
                queue.clear();
            }
        }
        yield Ok(responses_event("response.completed", json!({
            "type":"response.completed",
            "response":{"id": response_id, "object":"response", "status":"completed", "model": frontend_model, "output": output, "usage": usage}
        })));
        yield Ok(Bytes::from("data: [DONE]\n\n"));
        let inp = usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
        let out = usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
        if inp > 0 || out > 0 {
            state.record_usage(frontend_model.clone(), provider_id, endpoint, inp, out, None);
        }
    };
    sse_response(Body::from_stream(body_stream))
}

pub(super) async fn antigravity_sse_to_anthropic_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
) -> Response {
    let message_id = format!("msg_{}", unix_millis());
    let mut upstream_stream = upstream.bytes_stream();
    let state = state.clone();
    let body_stream = stream! {
        yield Ok::<Bytes, Infallible>(anthropic_event("message_start", json!({
            "type":"message_start",
            "message":{"id":message_id,"type":"message","role":"assistant","model":frontend_model,"content":[],"stop_reason":Value::Null,"stop_sequence":Value::Null,"usage":{"input_tokens":0,"output_tokens":0}}
        })));
        yield Ok(anthropic_event("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})));
        let mut buffer = String::new();
        let mut input_tokens = 0i64;
        let mut output_tokens = 0i64;
        let mut stop_reason = "end_turn".to_string();
        let mut text_open = true;
        let mut thinking_open = false;
        let mut current_index = 0i64;
        let mut tool_calls: Vec<StreamToolCallState> = Vec::new();
        let mut collected_thought_sigs: Vec<(String, String)> = Vec::new();
        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    state.observe_stream_interruption("antigravity-to-anthropic", &err.to_string());
                    yield Ok::<Bytes, Infallible>(anthropic_event("error", json!({"type":"error","error":{"type":"upstream_error","message":format!("stream disconnected: {err}")}})));
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if !line.starts_with("data:") { continue; }
                let data = line.trim_start_matches("data:").trim();
                if data == "[DONE]" { continue; }
                let Ok(value) = serde_json::from_str::<Value>(data) else { continue; };
                let parsed = convert::antigravity_stream_chunk(&value);
                if parsed.prompt_tokens > 0 { input_tokens = parsed.prompt_tokens; }
                if parsed.output_tokens > 0 { output_tokens = parsed.output_tokens; }
                if parsed.finish_reason.as_deref() == Some("MAX_TOKENS") { stop_reason = "max_tokens".to_string(); }
                // Collect (name, signature) pairs for next multi-turn request
                // (parity with Responses ingress streaming path).
                if !parsed.signature_pairs.is_empty() {
                    collected_thought_sigs.extend(parsed.signature_pairs.clone());
                }
                // Thinking content → Anthropic thinking block with its own
                // signature (parity with Go forward_anthropic_to_antigravity).
                if !parsed.reasoning_text.is_empty() {
                    if !thinking_open {
                        if text_open {
                            yield Ok(anthropic_event("content_block_stop", json!({"type":"content_block_stop","index":current_index})));
                            text_open = false;
                            current_index += 1;
                        }
                        let sig = parsed.thought_signatures.first().cloned().unwrap_or_default();
                        let cb = if sig.is_empty() {
                            json!({"type":"content_block_start","index":current_index,"content_block":{"type":"thinking","thinking":""}})
                        } else {
                            json!({"type":"content_block_start","index":current_index,"content_block":{"type":"thinking","thinking":"","signature":sig}})
                        };
                        yield Ok(anthropic_event("content_block_start", cb));
                        thinking_open = true;
                    }
                    yield Ok(anthropic_event("content_block_delta", json!({"type":"content_block_delta","index":current_index,"delta":{"type":"thinking_delta","thinking":parsed.reasoning_text}})));
                }
                if !parsed.text.is_empty() {
                    if thinking_open {
                        yield Ok(anthropic_event("content_block_stop", json!({"type":"content_block_stop","index":current_index})));
                        thinking_open = false;
                        current_index += 1;
                    }
                    yield Ok(anthropic_event("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":parsed.text}})));
                }
                for call in parsed.function_calls {
                    if thinking_open {
                        yield Ok(anthropic_event("content_block_stop", json!({"type":"content_block_stop","index":current_index})));
                        thinking_open = false;
                        current_index += 1;
                    }
                    if text_open {
                        yield Ok(anthropic_event("content_block_stop", json!({"type":"content_block_stop","index":current_index})));
                        text_open = false;
                        current_index += 1;
                    }
                    stop_reason = "tool_use".to_string();
                    // Each streaming functionCall chunk carries COMPLETE args
                    // (verified: antigravity never splits a call across chunks).
                    // Independent call per chunk — merging by name would corrupt
                    // same-named calls in one stream.
                    let index = current_index;
                    current_index += 1;
                    tool_calls.push(StreamToolCallState {
                        id: format!("toolu_{index}"),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        output_index: index as usize,
                    });
                    yield Ok(anthropic_event("content_block_start", json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":format!("toolu_{index}"),"name":call.name,"input":{}}})));
                    if !call.arguments.is_empty() {
                        yield Ok(anthropic_event("content_block_delta", json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":call.arguments}})));
                    }
                }
            }
        }
        if thinking_open {
            yield Ok(anthropic_event("content_block_stop", json!({"type":"content_block_stop","index":current_index})));
        }
        for call in &tool_calls {
            yield Ok(anthropic_event("content_block_stop", json!({"type":"content_block_stop","index":call.output_index})));
        }
        if text_open {
            yield Ok(anthropic_event("content_block_stop", json!({"type":"content_block_stop","index":0})));
        }
        // Enqueue collected thought_signature pairs for next multi-turn request
        // (parity with Responses ingress streaming path).
        if !collected_thought_sigs.is_empty() && !tool_calls.is_empty() {
            let mut queue = state.thought_sig_queue.lock().unwrap();
            let mut next_index: std::collections::HashMap<String, usize> = Default::default();
            for (name, signature) in collected_thought_sigs {
                let start = next_index.get(&name).copied().unwrap_or(0);
                let Some(slot) = tool_calls.iter().enumerate().skip(start).find(|(_, tc)| tc.name == name) else {
                    continue;
                };
                next_index.insert(name.clone(), slot.0 + 1);
                let key = crate::convert::signature_key_from_name_args(&name, &slot.1.arguments);
                queue.insert(key, signature);
            }
            if queue.len() > THOUGHT_SIGNATURE_MAP_MAX {
                queue.clear();
            }
        }
        yield Ok(anthropic_event("message_delta", json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":Value::Null},"usage":{"output_tokens":output_tokens}})));
        yield Ok(anthropic_event("message_stop", json!({"type":"message_stop"})));
        if input_tokens > 0 || output_tokens > 0 {
            state.record_usage(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, None);
        }
    };
    sse_response(Body::from_stream(body_stream))
}

pub(super) async fn responses_sse_to_chat_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
) -> Response {
    let mut upstream_stream = upstream.bytes_stream();
    let state = state.clone();
    let body_stream = stream! {
        let mut buffer = String::new();
        let mut finish_reason = "stop".to_string();
        let mut input_tokens = 0;
        let mut output_tokens = 0;
        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    state.observe_stream_interruption("responses-to-chat", &err.to_string());
                    yield Ok::<Bytes, Infallible>(Bytes::from(format!("event: error\ndata: {{\"error\":\"stream error: {err}\"}}\n\n")));
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if !line.starts_with("data:") { continue; }
                let data = line.trim_start_matches("data:").trim();
                if data == "[DONE]" { break; }
                let Ok(value) = serde_json::from_str::<Value>(data) else { continue; };
                match value.get("type").and_then(Value::as_str).unwrap_or("") {
                    "response.output_text.delta" => {
                        if let Some(text) = value.get("delta").and_then(Value::as_str)
                            && !text.is_empty() {
                                yield Ok(chat_event(json!({
                                    "id": "chatcmpl_llm_proxy",
                                    "object": "chat.completion.chunk",
                                    "created": 0,
                                    "model": frontend_model,
                                    "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": Value::Null}]
                                })));
                            }
                    }
                    "response.function_call_arguments.delta" => {
                        if let Some(arguments) = value.get("delta").and_then(Value::as_str)
                            && !arguments.is_empty() {
                                yield Ok(chat_event(json!({
                                    "id": "chatcmpl_llm_proxy",
                                    "object": "chat.completion.chunk",
                                    "created": 0,
                                    "model": frontend_model,
                                    "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": arguments}}]}, "finish_reason": Value::Null}]
                                })));
                            }
                    }
                    "response.output_item.added" => {
                        let item = value.get("item").cloned().unwrap_or_else(|| json!({}));
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            yield Ok(chat_event(json!({
                                "id": "chatcmpl_llm_proxy",
                                "object": "chat.completion.chunk",
                                "created": 0,
                                "model": frontend_model,
                                "choices": [{"index": 0, "delta": {"tool_calls": [{
                                    "index": 0,
                                    "id": item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or(""),
                                    "type": "function",
                                    "function": {"name": item.get("name").and_then(Value::as_str).unwrap_or(""), "arguments": ""}
                                }]}, "finish_reason": Value::Null}]
                            })));
                        }
                    }
                    "response.completed" => {
                        if let Some(usage) = value.get("response").and_then(|r| r.get("usage")) {
                            input_tokens = usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(input_tokens);
                            output_tokens = usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(output_tokens);
                        }
                    }
                    "response.incomplete" => finish_reason = "length".to_string(),
                    _ => {}
                }
            }
        }
        yield Ok(chat_event(json!({
            "id": "chatcmpl_llm_proxy",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": frontend_model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": input_tokens, "completion_tokens": output_tokens, "total_tokens": input_tokens + output_tokens}
        })));
        yield Ok(Bytes::from("data: [DONE]\n\n"));
        if input_tokens > 0 || output_tokens > 0 {
            state.record_usage(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, None);
        }
    };
    sse_response(Body::from_stream(body_stream))
}

pub(super) async fn chat_sse_to_responses_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
) -> Response {
    let now = unix_millis();
    let response_id = format!("resp_{now}");
    let message_id = format!("msg_{now}");
    let mut upstream_stream = upstream.bytes_stream();
    let state = state.clone();

    let body_stream = stream! {
        yield Ok::<Bytes, Infallible>(responses_event("response.created", json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "in_progress",
                "model": frontend_model,
                "output": []
            }
        })));
        yield Ok(responses_event("response.in_progress", json!({
            "type": "response.in_progress",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "in_progress",
                "model": frontend_model,
                "output": []
            }
        })));
        yield Ok(responses_event("response.output_item.added", json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "status": "in_progress",
                "content": []
            }
        })));
        yield Ok(responses_event("response.content_part.added", json!({
            "type": "response.content_part.added",
            "item_id": message_id,
            "output_index": 0,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": "",
                "annotations": []
            }
        })));

        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut usage = json!({ "input_tokens": 0, "output_tokens": 0, "total_tokens": 0 });
        let mut tool_calls: Vec<ToolCallAccumulator> = Vec::new();

        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    state.observe_stream_interruption("chat-to-responses", &err.to_string());
                    yield Ok(responses_event("response.error", json!({
                        "type": "response.error",
                        "error": {
                            "type": "upstream_error",
                            "message": format!("stream disconnected: {err}")
                        }
                    })));
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if !line.starts_with("data:") {
                    continue;
                }
                let data = line.trim_start_matches("data:").trim();
                if data == "[DONE]" {
                    break;
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(upstream_usage) = value.get("usage") {
                    usage = chat_usage_to_responses_usage(upstream_usage);
                }
                let choices = value.get("choices").and_then(Value::as_array).cloned().unwrap_or_default();
                for choice in choices {
                    let delta = choice.get("delta").cloned().unwrap_or_else(|| json!({}));
                    if let Some(text) = delta.get("content").and_then(Value::as_str)
                        && !text.is_empty() {
                            full_text.push_str(text);
                            yield Ok(responses_event("response.output_text.delta", json!({
                                "type": "response.output_text.delta",
                                "item_id": message_id,
                                "output_index": 0,
                                "content_index": 0,
                                "delta": text
                            })));
                        }
                    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                        accumulate_tool_calls(&mut tool_calls, calls);
                    }
                }
            }
        }

        yield Ok(responses_event("response.output_text.done", json!({
            "type": "response.output_text.done",
            "item_id": message_id,
            "output_index": 0,
            "content_index": 0,
            "text": full_text
        })));
        yield Ok(responses_event("response.content_part.done", json!({
            "type": "response.content_part.done",
            "item_id": message_id,
            "output_index": 0,
            "content_index": 0,
            "part": {
                "type": "output_text",
                "text": full_text
            }
        })));

        let message_item = json!({
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": full_text,
                "annotations": []
            }]
        });
        yield Ok(responses_event("response.output_item.done", json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": message_item
        })));

        let mut output = vec![message_item];
        for (idx, call) in tool_calls.iter().filter(|call| !call.id.is_empty()).enumerate() {
            let output_index = idx + 1;
            let item = json!({
                "id": call.id,
                "call_id": call.id,
                "type": "function_call",
                "name": call.name,
                "arguments": call.arguments,
                "status": "completed"
            });
            yield Ok(responses_event("response.output_item.added", json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": item
            })));
            yield Ok(responses_event("response.function_call_arguments.delta", json!({
                "type": "response.function_call_arguments.delta",
                "item_id": call.id,
                "output_index": output_index,
                "delta": call.arguments
            })));
            yield Ok(responses_event("response.function_call_arguments.done", json!({
                "type": "response.function_call_arguments.done",
                "item_id": call.id,
                "output_index": output_index,
                "arguments": call.arguments
            })));
            yield Ok(responses_event("response.output_item.done", json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item
            })));
            output.push(item);
        }

        if usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(0) == 0 && (!full_text.is_empty() || output.len() > 1) {
            usage = json!({ "input_tokens": 1, "output_tokens": 1, "total_tokens": 1 });
        }

        yield Ok(responses_event("response.completed", json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "completed",
                "model": frontend_model,
                "output": output,
                "usage": usage
            }
        })));
        let inp = usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
        let out = usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
        if inp > 0 || out > 0 {
            state.record_usage(frontend_model.clone(), provider_id, endpoint, inp, out, None);
        }
    };

    sse_response(Body::from_stream(body_stream))
}

pub(super) async fn responses_sse_to_anthropic_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
) -> Response {
    let message_id = format!("msg_{}", unix_millis());
    let mut upstream_stream = upstream.bytes_stream();
    let state = state.clone();
    let body_stream = stream! {
        yield Ok::<Bytes, Infallible>(anthropic_event("message_start", json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": frontend_model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        })));
        yield Ok(anthropic_event("content_block_start", json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        })));
        let mut buffer = String::new();
        let mut input_tokens = 0;
        let mut output_tokens = 0;
        let mut stop_reason = "end_turn".to_string();
        let mut current_index = 0i64;
        let mut text_open = true;
        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    state.observe_stream_interruption("responses-to-anthropic", &err.to_string());
                    yield Ok(anthropic_event("error", json!({
                        "type": "error",
                        "error": {"type": "upstream_error", "message": format!("stream disconnected: {err}")}
                    })));
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if !line.starts_with("data:") { continue; }
                let data = line.trim_start_matches("data:").trim();
                if data == "[DONE]" { break; }
                let Ok(value) = serde_json::from_str::<Value>(data) else { continue; };
                match value.get("type").and_then(Value::as_str).unwrap_or("") {
                    "response.output_text.delta" => {
                        if let Some(text) = value.get("delta").and_then(Value::as_str)
                            && !text.is_empty() {
                                yield Ok(anthropic_event("content_block_delta", json!({
                                    "type": "content_block_delta",
                                    "index": 0,
                                    "delta": {"type": "text_delta", "text": text}
                                })));
                            }
                    }
                    "response.output_item.added" => {
                        let item = value.get("item").cloned().unwrap_or_else(|| json!({}));
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            if text_open {
                                yield Ok(anthropic_event("content_block_stop", json!({"type": "content_block_stop", "index": current_index})));
                                text_open = false;
                                current_index += 1;
                            }
                            stop_reason = "tool_use".to_string();
                            yield Ok(anthropic_event("content_block_start", json!({
                                "type": "content_block_start",
                                "index": current_index,
                                "content_block": {
                                    "type": "tool_use",
                                    "id": item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or(""),
                                    "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
                                    "input": {}
                                }
                            })));
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        if let Some(partial) = value.get("delta").and_then(Value::as_str)
                            && !partial.is_empty() {
                                yield Ok(anthropic_event("content_block_delta", json!({
                                    "type": "content_block_delta",
                                    "index": current_index,
                                    "delta": {"type": "input_json_delta", "partial_json": partial}
                                })));
                            }
                    }
                    "response.output_item.done" => {
                        let item = value.get("item").cloned().unwrap_or_else(|| json!({}));
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            yield Ok(anthropic_event("content_block_stop", json!({"type": "content_block_stop", "index": current_index})));
                            current_index += 1;
                        }
                    }
                    "response.completed" => {
                        if let Some(usage) = value.get("response").and_then(|r| r.get("usage")) {
                            input_tokens = usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(input_tokens);
                            output_tokens = usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(output_tokens);
                        }
                    }
                    "response.incomplete" => stop_reason = "max_tokens".to_string(),
                    _ => {}
                }
            }
        }
        if text_open {
            yield Ok(anthropic_event("content_block_stop", json!({"type": "content_block_stop", "index": 0})));
        }
        yield Ok(anthropic_event("message_delta", json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
            "usage": {"output_tokens": output_tokens}
        })));
        yield Ok(anthropic_event("message_stop", json!({"type": "message_stop"})));
        if input_tokens > 0 || output_tokens > 0 {
            state.record_usage(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, None);
        }
    };
    sse_response(Body::from_stream(body_stream))
}

pub(super) async fn chat_sse_to_anthropic_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
) -> Response {
    let message_id = format!("msg_{}", unix_millis());
    let mut upstream_stream = upstream.bytes_stream();
    let state = state.clone();
    let body_stream = stream! {
        yield Ok::<Bytes, Infallible>(anthropic_event("message_start", json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": frontend_model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            }
        })));
        yield Ok(anthropic_event("content_block_start", json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        })));

        let mut buffer = String::new();
        let mut input_tokens = 0;
        let mut output_tokens = 0;
        let mut stop_reason = "end_turn".to_string();
        // Block management for tool_use conversion (Go StreamOpenAIToAnthropic
        // parity): the text block is open at index 0; each new tool call
        // closes the open block and opens the next index.
        let mut current_block_index = 0i64;
        let mut text_block_open = true;
        let mut tool_block_open = false;

        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    state.observe_stream_interruption("chat-to-anthropic", &err.to_string());
                    yield Ok(anthropic_event("error", json!({
                        "type": "error",
                        "error": { "type": "upstream_error", "message": format!("stream disconnected: {err}") }
                    })));
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if !line.starts_with("data:") { continue; }
                let data = line.trim_start_matches("data:").trim();
                if data == "[DONE]" { break; }
                let Ok(value) = serde_json::from_str::<Value>(data) else { continue; };
                if let Some(usage) = value.get("usage") {
                    input_tokens = usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(input_tokens);
                    output_tokens = usage.get("completion_tokens").and_then(Value::as_i64).unwrap_or(output_tokens);
                }
                let choices = value.get("choices").and_then(Value::as_array).cloned().unwrap_or_default();
                for choice in choices {
                    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                        stop_reason = match reason {
                            "length" => "max_tokens",
                            "tool_calls" => "tool_use",
                            _ => "end_turn",
                        }.to_string();
                    }
                    let delta = choice.get("delta").cloned().unwrap_or_else(|| json!({}));
                    if let Some(text) = delta.get("content").and_then(Value::as_str)
                        && !text.is_empty() {
                            yield Ok(anthropic_event("content_block_delta", json!({
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": { "type": "text_delta", "text": text }
                            })));
                        }
                    let tool_calls = delta
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    for call in tool_calls {
                        let function = call.get("function").cloned().unwrap_or_else(|| json!({}));
                        let name = function.get("name").and_then(Value::as_str).unwrap_or("");
                        if !name.is_empty() {
                            if text_block_open || tool_block_open {
                                yield Ok(anthropic_event("content_block_stop", json!({
                                    "type": "content_block_stop",
                                    "index": current_block_index
                                })));
                                current_block_index += 1;
                            }
                            text_block_open = false;
                            tool_block_open = true;
                            yield Ok(anthropic_event("content_block_start", json!({
                                "type": "content_block_start",
                                "index": current_block_index,
                                "content_block": {
                                    "type": "tool_use",
                                    "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                                    "name": name,
                                    "input": {}
                                }
                            })));
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str)
                            && !arguments.is_empty() && tool_block_open {
                                yield Ok(anthropic_event("content_block_delta", json!({
                                    "type": "content_block_delta",
                                    "index": current_block_index,
                                    "delta": { "type": "input_json_delta", "partial_json": arguments }
                                })));
                            }
                    }
                }
            }
        }

        yield Ok(anthropic_event("content_block_stop", json!({
            "type": "content_block_stop",
            "index": current_block_index
        })));
        yield Ok(anthropic_event("message_delta", json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
            "usage": { "output_tokens": output_tokens }
        })));
        yield Ok(anthropic_event("message_stop", json!({ "type": "message_stop" })));
        if input_tokens > 0 || output_tokens > 0 {
            state.record_usage(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, None);
        }
    };
    sse_response(Body::from_stream(body_stream))
}

pub(super) async fn anthropic_sse_to_chat_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
) -> Response {
    let mut upstream_stream = upstream.bytes_stream();
    let state = state.clone();
    let body_stream = stream! {
        let mut buffer = String::new();
        let mut input_tokens = 0;
        let mut output_tokens = 0;
        let mut finish_reason = "stop".to_string();
        // Sequential chat tool_call index for streamed tool_use blocks.
        let mut next_tool_index = 0i64;
        let mut current_tool_index = 0i64;
        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    state.observe_stream_interruption("anthropic-to-chat", &err.to_string());
                    yield Ok::<Bytes, Infallible>(Bytes::from(format!("event: error\ndata: {{\"error\":\"stream error: {err}\"}}\n\n")));
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if !line.starts_with("data:") { continue; }
                let data = line.trim_start_matches("data:").trim();
                let Ok(value) = serde_json::from_str::<Value>(data) else { continue; };
                match value.get("type").and_then(Value::as_str).unwrap_or("") {
                    "content_block_start" => {
                        let block = value.get("content_block").cloned().unwrap_or_else(|| json!({}));
                        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                            current_tool_index = next_tool_index;
                            next_tool_index += 1;
                            yield Ok(chat_event(json!({
                                "id": "chatcmpl_llm_proxy",
                                "object": "chat.completion.chunk",
                                "created": 0,
                                "model": frontend_model,
                                "choices": [{"index": 0, "delta": {"tool_calls": [{
                                    "index": current_tool_index,
                                    "id": block.get("id").and_then(Value::as_str).unwrap_or(""),
                                    "type": "function",
                                    "function": {
                                        "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
                                        "arguments": ""
                                    }
                                }]}, "finish_reason": Value::Null}]
                            })));
                        }
                    }
                    "content_block_delta" => {
                        let delta = value.get("delta").cloned().unwrap_or_else(|| json!({}));
                        match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                            // Go parity: thinking streams surface as content.
                            "text_delta" | "thinking_delta" => {
                                let text = delta
                                    .get("text")
                                    .or_else(|| delta.get("thinking"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if !text.is_empty() {
                                    yield Ok(chat_event(json!({
                                        "id": "chatcmpl_llm_proxy",
                                        "object": "chat.completion.chunk",
                                        "created": 0,
                                        "model": frontend_model,
                                        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": Value::Null}]
                                    })));
                                }
                            }
                            "input_json_delta" => {
                                if let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                                    && !partial.is_empty() {
                                        yield Ok(chat_event(json!({
                                            "id": "chatcmpl_llm_proxy",
                                            "object": "chat.completion.chunk",
                                            "created": 0,
                                            "model": frontend_model,
                                            "choices": [{"index": 0, "delta": {"tool_calls": [{
                                                "index": current_tool_index,
                                                "function": {"arguments": partial}
                                            }]}, "finish_reason": Value::Null}]
                                        })));
                                    }
                            }
                            _ => {}
                        }
                    }
                    "message_delta" => {
                        if let Some(usage) = value.get("usage") {
                            output_tokens = usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(output_tokens);
                        }
                        if let Some(reason) = value.get("delta").and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                            finish_reason = match reason {
                                "max_tokens" => "length",
                                "tool_use" => "tool_calls",
                                _ => "stop",
                            }.to_string();
                        }
                    }
                    "message_start" => {
                        if let Some(usage) = value.get("message").and_then(|m| m.get("usage")) {
                            input_tokens = usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(input_tokens);
                        }
                    }
                    _ => {}
                }
            }
        }
        yield Ok(chat_event(json!({
            "id": "chatcmpl_llm_proxy",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": frontend_model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": input_tokens, "completion_tokens": output_tokens, "total_tokens": input_tokens + output_tokens}
        })));
        yield Ok(Bytes::from("data: [DONE]\n\n"));
        if input_tokens > 0 || output_tokens > 0 {
            state.record_usage(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, None);
        }
    };
    sse_response(Body::from_stream(body_stream))
}

pub(super) async fn anthropic_sse_to_responses_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
) -> Response {
    let now = unix_millis();
    let response_id = format!("resp_{now}");
    let message_id = format!("msg_{now}");
    let mut upstream_stream = upstream.bytes_stream();
    let state = state.clone();
    let body_stream = stream! {
        yield Ok::<Bytes, Infallible>(responses_event("response.created", json!({
            "type": "response.created",
            "response": {"id": response_id, "object": "response", "status": "in_progress", "model": frontend_model, "output": []}
        })));
        yield Ok(responses_event("response.output_item.added", json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"id": message_id, "type": "message", "role": "assistant", "status": "in_progress", "content": []}
        })));
        yield Ok(responses_event("response.content_part.added", json!({
            "type": "response.content_part.added",
            "item_id": message_id,
            "output_index": 0,
            "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []}
        })));
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut input_tokens = 0;
        let mut output_tokens = 0;
        // Tool-use streaming state: function_call items start at output_index 1
        // (the text message occupies index 0).
        let mut next_output_index = 1i64;
        let mut current_tool: Option<(String, i64, String, String, String)> = None;
        let mut completed_tools: Vec<Value> = Vec::new();
        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    state.observe_stream_interruption("anthropic-to-responses", &err.to_string());
                    yield Ok(responses_event("response.error", json!({
                        "type": "response.error",
                        "error": {"type": "upstream_error", "message": format!("stream disconnected: {err}")}
                    })));
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                if !line.starts_with("data:") { continue; }
                let data = line.trim_start_matches("data:").trim();
                let Ok(value) = serde_json::from_str::<Value>(data) else { continue; };
                match value.get("type").and_then(Value::as_str).unwrap_or("") {
                    "content_block_start" => {
                        let block = value.get("content_block").cloned().unwrap_or_else(|| json!({}));
                        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                            let call_id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let item_id = format!("fc_{call_id}");
                            let output_index = next_output_index;
                            next_output_index += 1;
                            current_tool =
                                Some((item_id.clone(), output_index, call_id, name, String::new()));
                            let (item_call_id, item_name) =
                                (current_tool.as_ref().unwrap().2.clone(), current_tool.as_ref().unwrap().3.clone());
                            yield Ok(responses_event("response.output_item.added", json!({
                                "type": "response.output_item.added",
                                "output_index": output_index,
                                "item": {
                                    "id": item_id,
                                    "type": "function_call",
                                    "call_id": item_call_id,
                                    "name": item_name,
                                    "arguments": "",
                                    "status": "in_progress"
                                }
                            })));
                        }
                    }
                    "content_block_delta" => {
                        let delta = value.get("delta").cloned().unwrap_or_else(|| json!({}));
                        match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text_delta" => {
                                if let Some(text) = delta.get("text").and_then(Value::as_str)
                                    && !text.is_empty() {
                                        full_text.push_str(text);
                                        yield Ok(responses_event("response.output_text.delta", json!({
                                            "type": "response.output_text.delta",
                                            "item_id": message_id,
                                            "output_index": 0,
                                            "content_index": 0,
                                            "delta": text
                                        })));
                                    }
                            }
                            "input_json_delta" => {
                                let mut emit = None;
                                if let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                                    && !partial.is_empty()
                                        && let Some(tool) = current_tool.as_mut() {
                                            tool.4.push_str(partial);
                                            emit = Some((tool.0.clone(), tool.1, partial.to_string()));
                                        }
                                if let Some((item_id, output_index, partial)) = emit {
                                    yield Ok(responses_event("response.function_call_arguments.delta", json!({
                                        "type": "response.function_call_arguments.delta",
                                        "item_id": item_id,
                                        "output_index": output_index,
                                        "delta": partial
                                    })));
                                }
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        if let Some((item_id, output_index, call_id, name, arguments)) =
                            current_tool.take()
                        {
                            yield Ok(responses_event("response.function_call_arguments.done", json!({
                                "type": "response.function_call_arguments.done",
                                "item_id": item_id,
                                "output_index": output_index,
                                "arguments": arguments
                            })));
                            let item = json!({
                                "id": item_id,
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments,
                                "status": "completed"
                            });
                            yield Ok(responses_event("response.output_item.done", json!({
                                "type": "response.output_item.done",
                                "output_index": output_index,
                                "item": item
                            })));
                            completed_tools.push(item);
                        }
                    }
                    "message_start" => {
                        if let Some(usage) = value.get("message").and_then(|m| m.get("usage")) {
                            input_tokens = usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(input_tokens);
                        }
                    }
                    "message_delta" => {
                        if let Some(usage) = value.get("usage") {
                            output_tokens = usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(output_tokens);
                        }
                    }
                    _ => {}
                }
            }
        }
        yield Ok(responses_event("response.output_text.done", json!({
            "type": "response.output_text.done", "item_id": message_id, "output_index": 0, "content_index": 0, "text": full_text
        })));
        let message_item = json!({
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": full_text, "annotations": []}]
        });
        yield Ok(responses_event("response.output_item.done", json!({
            "type": "response.output_item.done", "output_index": 0, "item": message_item
        })));
        let mut output_items = vec![message_item];
        output_items.extend(completed_tools.iter().cloned());
        yield Ok(responses_event("response.completed", json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "object": "response",
                "status": "completed",
                "model": frontend_model,
                "output": output_items,
                "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens, "total_tokens": input_tokens + output_tokens}
            }
        })));
        if input_tokens > 0 || output_tokens > 0 {
            state.record_usage(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, None);
        }
    };
    sse_response(Body::from_stream(body_stream))
}

#[derive(Debug, Default)]
pub(super) struct ToolCallAccumulator {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
}

pub(super) fn accumulate_tool_calls(acc: &mut Vec<ToolCallAccumulator>, calls: &[Value]) {
    for call in calls {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        while acc.len() <= index {
            acc.push(ToolCallAccumulator::default());
        }
        let entry = &mut acc[index];
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            entry.id = id.to_string();
        }
        if let Some(function) = call.get("function").and_then(Value::as_object) {
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                entry.name = name.to_string();
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                entry.arguments.push_str(arguments);
            }
        }
    }
}

pub(super) fn responses_event(event: &str, data: Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

pub(super) fn anthropic_event(event: &str, data: Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

pub(super) fn chat_event(data: Value) -> Bytes {
    Bytes::from(format!("data: {data}\n\n"))
}

pub(super) fn rewrite_model_fields(value: &mut Value, frontend_model: &str) {
    match value {
        Value::Object(obj) => {
            if obj.get("model").is_some() {
                obj.insert("model".to_string(), json!(frontend_model));
            }
            for child in obj.values_mut() {
                rewrite_model_fields(child, frontend_model);
            }
        }
        Value::Array(items) => {
            for child in items {
                rewrite_model_fields(child, frontend_model);
            }
        }
        _ => {}
    }
}

pub(super) fn sse_response(body: Body) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    (headers, body).into_response()
}

pub(super) async fn upstream_error_json(upstream: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let retry_after = upstream.headers().get(header::RETRY_AFTER).cloned();
    let text = upstream.text().await.unwrap_or_default();
    let message = flatten_upstream_error_message(&text);
    with_retry_after(json_error(status, &message), retry_after)
}

pub(super) async fn upstream_error_anthropic(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let retry_after = upstream.headers().get(header::RETRY_AFTER).cloned();
    let text = upstream.text().await.unwrap_or_default();
    let message = flatten_upstream_error_message(&text);
    with_retry_after(anthropic_error(status, &message), retry_after)
}

pub(super) async fn upstream_error_responses(upstream: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let retry_after = upstream.headers().get(header::RETRY_AFTER).cloned();
    let text = upstream.text().await.unwrap_or_default();
    let message = flatten_upstream_error_message(&text);
    with_retry_after(responses_error(status, &message), retry_after)
}

/// Try to extract a human-readable error message from upstream error JSON.
///
/// Upstream providers sometimes double-wrap errors (the proxy's own error
/// envelope gets serialised into the `message` field of another envelope),
/// producing `{"error":{"message":"{\"error\":{\"message\":\"...\"}}"}}`.
/// This function recursively unwraps such nesting so the client sees a
/// clean, flat message.  If the text is not valid JSON or has no recognisable
/// error structure the original text is returned unchanged.
pub(super) fn flatten_upstream_error_message(text: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(text) else {
        return text.to_string();
    };
    // Walk up to 4 levels of nesting — more than enough for any real case.
    for _ in 0..4 {
        let next = extract_inner_error_message(&value);
        match next {
            Some(msg) => {
                // If the extracted message is itself JSON, keep unwrapping.
                if let Ok(inner) = serde_json::from_str::<Value>(&msg) {
                    value = inner;
                    continue;
                }
                return msg;
            }
            None => break,
        }
    }
    // Could not extract a clean message — fall back to the original text.
    text.to_string()
}

/// Try to pull a `message` string out of common error envelope shapes:
///   {"error": {"message": "..."}}
///   {"error": {"message": "...", "type": "..."}}
///   {"message": "..."}
fn extract_inner_error_message(value: &Value) -> Option<String> {
    // Shape 1: {"error": {"message": "..."}}
    if let Some(error) = value.get("error")
        && let Some(msg) = error.get("message").and_then(Value::as_str)
    {
        return Some(msg.to_string());
    }
    // Shape 2: top-level {"message": "..."} (e.g. already unwrapped once)
    if let Some(msg) = value.get("message").and_then(Value::as_str) {
        return Some(msg.to_string());
    }
    None
}

pub(super) fn with_retry_after(
    mut response: Response,
    retry_after: Option<HeaderValue>,
) -> Response {
    if let Some(value) = retry_after {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

pub(super) fn anthropic_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": { "type": anthropic_error_type(status), "message": message }
        })),
    )
        .into_response()
}

/// Anthropic's documented error type taxonomy — Claude clients branch on it.
pub(super) fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "invalid_request_error",
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        _ if status.is_server_error() => "api_error",
        _ => "api_error",
    }
}

pub(super) fn error_code(status: StatusCode) -> String {
    status
        .canonical_reason()
        .unwrap_or("error")
        .to_lowercase()
        .replace(' ', "_")
}

pub(super) fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "proxy_error",
                "code": error_code(status)
            }
        })),
    )
        .into_response()
}

pub(super) fn responses_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": {
                "type": error_code(status),
                "message": message
            }
        })),
    )
        .into_response()
}

pub(super) fn chat_usage_to_responses_usage(usage: &Value) -> Value {
    let input = usage
        .get("prompt_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(input + output);
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": total
    })
}

pub(super) fn cooldown_kind_for_status(status: StatusCode) -> &'static str {
    if status == StatusCode::TOO_MANY_REQUESTS {
        "rate_limit"
    } else if status == StatusCode::NOT_FOUND {
        "model_unavailable"
    } else if status.is_server_error() {
        "server_error"
    } else {
        "client_error"
    }
}

pub(super) fn cooldown_duration_for_response(
    status: StatusCode,
    headers: &HeaderMap,
    cooldown: &crate::config::FallbackCooldownConfig,
) -> Option<Duration> {
    if status == StatusCode::TOO_MANY_REQUESTS
        && let Some(duration) = retry_after_duration(headers)
    {
        return Some(duration);
    }
    cooldown_duration_for_status(status, cooldown)
}

pub(super) fn retry_after_duration(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return capped_nonzero_duration(seconds);
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    let seconds = retry_at.duration_since(SystemTime::now()).ok()?.as_secs();
    capped_nonzero_duration(seconds)
}

pub(super) fn capped_nonzero_duration(seconds: u64) -> Option<Duration> {
    if seconds == 0 {
        None
    } else {
        Some(Duration::from_secs(seconds.min(24 * 60 * 60)))
    }
}

pub(super) fn cooldown_duration_for_status(
    status: StatusCode,
    cooldown: &crate::config::FallbackCooldownConfig,
) -> Option<Duration> {
    let seconds = if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        0
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        cooldown.rate_limit_seconds
    } else if status == StatusCode::NOT_FOUND {
        cooldown.model_unavailable_seconds
    } else if status.is_server_error() {
        cooldown.server_error_seconds
    } else if status == StatusCode::REQUEST_TIMEOUT {
        cooldown.network_seconds
    } else if status == StatusCode::BAD_REQUEST {
        cooldown.client_error_seconds
    } else {
        0
    };
    if seconds == 0 {
        None
    } else {
        Some(Duration::from_secs(seconds.min(24 * 60 * 60)))
    }
}

pub(super) fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
