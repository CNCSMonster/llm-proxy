//! Proxy submodule extracted from `proxy::mod`.

use super::*;
use anyhow::bail;
use async_stream::stream;
use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;
use std::convert::Infallible;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) async fn aggregate_responses_sse_to_value(
    state: &AppState,
    upstream: reqwest::Response,
) -> Result<Value> {
    use futures_util::StreamExt;
    let cfg = state.config();
    let max_buffer_bytes = cfg
        .server_config()
        .map(|s| s.max_sse_buffer_bytes)
        .unwrap_or(64 * 1024 * 1024);
    let max_output_items = cfg
        .server_config()
        .map(|s| s.max_output_items)
        .unwrap_or(4096);
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
#[allow(clippy::too_many_arguments)]
pub(super) async fn aggregate_responses_sse_to_json(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
    token: Option<&crate::token::auth::TokenContext>,
    ttfb_ms: Option<u64>,
    routed_target: Option<String>,

    request_start: std::time::Instant,
) -> Response {
    match aggregate_responses_sse_to_value(state, upstream).await {
        Ok(mut value) => {
            let latency_ms = request_start.elapsed().as_millis() as i64;
            record_usage_from_responses_value(
                state,
                &value,
                &frontend_model,
                &provider_id,
                &endpoint,
                Some(latency_ms),
                token,
                true,
                0,
                ttfb_ms,
                routed_target,
            );
            value["model"] = json!(frontend_model);
            Json(value).into_response()
        }
        Err(err) => {
            let mut guard = StreamUsageGuard::new_with_target(
                state.clone(),
                frontend_model.to_string(),
                provider_id.clone(),
                endpoint.clone(),
                token.cloned(),
                routed_target.clone(),
                request_start,
            );
            guard.terminate("invalid_json");
            json_error(StatusCode::BAD_GATEWAY, &format!("upstream failure: {err}"))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn passthrough_sse(
    state: &AppState,
    direction: &'static str,
    upstream: reqwest::Response,
    provider_id: String,
    endpoint: String,
    frontend_model: String,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
    routed_target: Option<String>,
) -> Response {
    let token_owned = token.cloned();
    let mut upstream_stream = upstream.bytes_stream();
    let state_clone = state.clone();
    let stream = stream! {
           let mut buffer = String::new();
           let mut input_tokens: i64 = 0;
           let mut output_tokens: i64 = 0;
    // : capture TTFB at first upstream chunk.
           let mut ttfb_ms: Option<u64> = None;
           while let Some(chunk) = upstream_stream.next().await {
               if ttfb_ms.is_none() {
                   ttfb_ms = Some(request_start.elapsed().as_millis() as u64);
               }
               let chunk = match chunk {
                   Ok(bytes) => bytes,
                   Err(err) => {
                       state_clone.observe_stream_interruption(direction, &err.to_string());
                       // 非 guard 系 error 补记：流中断也留 usage 行（ingress latency/ttfb+已抽 token）。
                    // 语义：token 计数=已解析帧的累计值（触发错误的当前帧未计数）；
                    // truncated=false/None——中断（连接断）≠截断（max_tokens/内容截断）。
                       state_clone.record_usage_with_status_and_target(frontend_model.clone(), provider_id.clone(), endpoint.clone(), input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("upstream_interrupted"), true, 0, false, None, ttfb_ms, routed_target.clone());
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
               state_clone.record_usage_with_target(frontend_model, provider_id, endpoint, input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), token_owned.as_ref(), true, 0, ttfb_ms, routed_target);
           }
       };
    sse_response(Body::from_stream(stream))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn responses_native_sse_rewrite_model(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
    routed_target: Option<String>,
) -> Response {
    let token_owned = token.cloned();
    let mut upstream_stream = upstream.bytes_stream();
    let state_clone = state.clone();
    let body_stream = stream! {
        let mut buffer = String::new();
        let mut input_tokens: i64 = 0;
        let mut output_tokens: i64 = 0;
        let mut ttfb_ms: Option<u64> = None;
        while let Some(chunk) = upstream_stream.next().await {
            if ttfb_ms.is_none() {
                ttfb_ms = Some(request_start.elapsed().as_millis() as u64);
            }
            let chunk = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    // This egress was the only one of the eight that
                    // left *no* trace of a mid-stream failure — it neither logged nor
                    // bumped the counter, so the direction never appeared in `/state`.
                    state_clone
                        .observe_stream_interruption("responses-to-responses", &err.to_string());
                    // 非 guard 系 error 补记：流中断也留 usage 行（ingress latency/ttfb+已抽 token）
                    state_clone.record_usage_with_status_and_target(frontend_model.clone(), provider_id.clone(), endpoint.clone(), input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("upstream_interrupted"), true, 0, false, None, ttfb_ms, routed_target.clone());
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
            state_clone.record_usage_with_target(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), token_owned.as_ref(), true, 0, ttfb_ms, routed_target);
        }
    };
    sse_response(Body::from_stream(body_stream))
}

/// Usage-ledger terminal-status bookkeeping for streaming
/// egress. Records exactly once with an explicit `status`; when the generator
/// is dropped before any terminal branch ran (downstream client disconnected),
/// `Drop` records `client_disconnected` so complete and truncated streams are
/// never ambiguous in `usage.jsonl` again.
pub(crate) struct StreamUsageGuard {
    state: AppState,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) endpoint: String,
    pub(crate) token: Option<crate::token::auth::TokenContext>,
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) terminal: bool,
    /// whether the output was truncated.
    pub(crate) truncated: bool,
    /// truncation reason ("max_tokens", "content_filter", "other").
    pub(crate) truncation_reason: Option<String>,
    /// time-to-first-byte, captured at the first upstream chunk.
    pub(crate) ttfb_ms: Option<u64>,
    /// request-entry clock for end-to-end latency at stream end.
    pub(crate) request_start: std::time::Instant,
    pub(crate) routed_target: Option<String>,
}

impl StreamUsageGuard {
    #[allow(dead_code)]
    pub(crate) fn new(
        state: AppState,
        model: String,
        provider: String,
        endpoint: String,
        token: Option<crate::token::auth::TokenContext>,
        request_start: std::time::Instant,
    ) -> Self {
        Self::new_with_target(state, model, provider, endpoint, token, None, request_start)
    }

    pub(crate) fn new_with_target(
        state: AppState,
        model: String,
        provider: String,
        endpoint: String,
        token: Option<crate::token::auth::TokenContext>,
        routed_target: Option<String>,
        request_start: std::time::Instant,
    ) -> Self {
        Self {
            state,
            model,
            provider,
            endpoint,
            token,
            input_tokens: 0,
            output_tokens: 0,
            terminal: false,
            truncated: false,
            truncation_reason: None,
            ttfb_ms: None,
            request_start,
            routed_target,
        }
    }

    /// Marks a known stream outcome and records usage. `completed` keeps the
    /// historical no-row-for-zero-usage behavior; interrupted outcomes always
    /// record so the truncation itself is visible.
    pub(crate) fn terminate(&mut self, status: &'static str) {
        self.terminal = true;
        if status == "completed" && self.input_tokens == 0 && self.output_tokens == 0 {
            return;
        }
        self.record(status);
    }

    fn record(&self, status: &str) {
        self.state.record_usage_with_status_and_target(
            self.model.clone(),
            self.provider.clone(),
            self.endpoint.clone(),
            self.input_tokens,
            self.output_tokens,
            Some(self.request_start.elapsed().as_millis() as i64),
            None,
            None,
            self.token.as_ref(),
            Some(status),
            true,
            0,
            self.truncated,
            self.truncation_reason.as_deref(),
            self.ttfb_ms,
            self.routed_target.clone(),
        );
    }
}

/// Records a request that failed after leaving the local pipeline — upstream
/// send failure, error status from upstream, or a malformed upstream body.
/// Emits a zero-token usage row carrying the failure status and end-to-end
/// latency measured from the handler-entry clock, so failed requests stay
/// visible in usage history. Auth rejections and local conversion errors
/// (request never left the process) are intentionally not recorded.
pub(super) fn record_request_failure(
    state: &AppState,
    frontend_model: &str,
    plan: &crate::config::ExecutionPlan,
    endpoint: &str,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
    failure_status: &'static str,
) {
    let mut guard = StreamUsageGuard::new_with_target(
        state.clone(),
        frontend_model.to_string(),
        plan.provider_id.clone(),
        endpoint.to_string(),
        token.cloned(),
        plan.routed_target.clone(),
        request_start,
    );
    guard.terminate(failure_status);
}

/// Send-failure variant: skips requests the local frequency limiter rejected
/// before they ever left the process (those stay unrecorded, matching the
/// documented boundary of `record_request_failure`).
pub(super) fn record_send_failure(
    state: &AppState,
    frontend_model: &str,
    plan: &crate::config::ExecutionPlan,
    endpoint: &str,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
    err: &UpstreamSendFailure,
) {
    if matches!(err, UpstreamSendFailure::FrequencyLimited) {
        return;
    }
    record_request_failure(
        state,
        frontend_model,
        plan,
        endpoint,
        token,
        request_start,
        "send_failed",
    );
}

impl Drop for StreamUsageGuard {
    fn drop(&mut self) {
        if !self.terminal {
            // Generator dropped at an await point mid-stream: the downstream
            // consumer went away before any terminal branch ran.
            self.record("client_disconnected");
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn responses_sse_to_chat_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
    routed_target: Option<String>,
) -> Response {
    let token_owned = token.cloned();
    let mut upstream_stream = upstream.bytes_stream();
    let state = state.clone();
    let body_stream = stream! {
           let mut buffer = String::new();
           let mut finish_reason = "stop".to_string();
           let mut input_tokens = 0;
           let mut output_tokens = 0;
           let mut tool_call_indices = HashMap::new();
           let mut next_tool_call_index = 0;
           // Synthetic Chat chunks carry one generated id/timestamp for the whole
           // stream (spec.md §6 ID namespace rules).
           let chat_id = convert::new_scoped_id("chatcmpl-");
           let chat_created = convert::unix_now_secs();
           let mut ttfb_ms: Option<u64> = None;
           while let Some(chunk) = upstream_stream.next().await {
               let chunk = match chunk {
                   Ok(bytes) => {
                       if ttfb_ms.is_none() {
                           ttfb_ms = Some(request_start.elapsed().as_millis() as u64);
                       }
                       bytes
                   }
                   Err(err) => {
                       state.observe_stream_interruption("responses-to-chat", &err.to_string());
                    // 非 guard 系 error 补记：流中断也留 usage 行（ingress latency/ttfb+已抽 token）
                    state.record_usage_with_status_and_target(frontend_model.clone(), provider_id.clone(), endpoint.clone(), input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("upstream_interrupted"), true, 0, false, None, ttfb_ms, routed_target.clone());
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
                                       "id": chat_id.clone(),
                                       "object": "chat.completion.chunk",
                                       "created": chat_created,
                                       "model": frontend_model,
                                       "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": Value::Null}]
                                   })));
                               }
                       }
                       "response.function_call_arguments.delta" => {
                           if let Some(item_id) = value.get("item_id").and_then(Value::as_str)
                               && let Some(&tool_call_index) = tool_call_indices.get(item_id)
                               && let Some(arguments) = value.get("delta").and_then(Value::as_str)
                               && !arguments.is_empty() {
                                   yield Ok(chat_event(json!({
                                       "id": chat_id.clone(),
                                       "object": "chat.completion.chunk",
                                       "created": chat_created,
                                       "model": frontend_model,
                                       "choices": [{"index": 0, "delta": {"tool_calls": [{"index": tool_call_index, "function": {"arguments": arguments}}]}, "finish_reason": Value::Null}]
                                   })));
                               }
                       }
                       "response.output_item.added" => {
                           let item = value.get("item").cloned().unwrap_or_else(|| json!({}));
                           if item.get("type").and_then(Value::as_str) == Some("function_call") {
                               let Some(item_id) = item.get("id").and_then(Value::as_str) else {
                                   continue;
                               };
                               let tool_call_index = next_tool_call_index;
                               next_tool_call_index += 1;
                               tool_call_indices.insert(item_id.to_string(), tool_call_index);
                               yield Ok(chat_event(json!({
                                   "id": chat_id.clone(),
                                   "object": "chat.completion.chunk",
                                   "created": chat_created,
                                   "model": frontend_model,
                                   "choices": [{"index": 0, "delta": {"tool_calls": [{
                                       "index": tool_call_index,
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
               "id": chat_id.clone(),
               "object": "chat.completion.chunk",
               "created": chat_created,
               "model": frontend_model,
               "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
               "usage": {"prompt_tokens": input_tokens, "completion_tokens": output_tokens, "total_tokens": input_tokens + output_tokens}
           })));
           yield Ok(Bytes::from("data: [DONE]\n\n"));
           if input_tokens > 0 || output_tokens > 0 {
    // : detect truncation from finish_reason.
               let (truncated, truncation_reason) = match finish_reason.as_str() {
                   "length" => (true, Some("max_tokens")),
                   "content_filter" => (true, Some("content_filter")),
                   _ => (false, None),
               };
               state.record_usage_with_status_and_target(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("completed"), true, 0, truncated, truncation_reason, ttfb_ms, routed_target);
           }
       };
    sse_response(Body::from_stream(body_stream))
}

pub(super) use super::chat_responses_stream::chat_sse_to_responses_sse;

#[allow(clippy::too_many_arguments)]
pub(super) async fn responses_sse_to_anthropic_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
    routed_target: Option<String>,
) -> Response {
    let token_owned = token.cloned();
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
           let mut next_content_block_index = 1i64;
           let mut text_open = true;
           let mut tool_call_blocks = HashMap::new();
           let mut ttfb_ms: Option<u64> = None;
           while let Some(chunk) = upstream_stream.next().await {
               let chunk = match chunk {
                   Ok(bytes) => {
                       if ttfb_ms.is_none() {
                           ttfb_ms = Some(request_start.elapsed().as_millis() as u64);
                       }
                       bytes
                   }
                   Err(err) => {
                       state.observe_stream_interruption("responses-to-anthropic", &err.to_string());
                    // 非 guard 系 error 补记：流中断也留 usage 行（ingress latency/ttfb+已抽 token）
                    state.record_usage_with_status_and_target(frontend_model.clone(), provider_id.clone(), endpoint.clone(), input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("upstream_interrupted"), true, 0, false, None, ttfb_ms, routed_target.clone());
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
                               let Some(item_id) = item.get("id").and_then(Value::as_str) else {
                                   continue;
                               };
                               if text_open {
                                   yield Ok(anthropic_event("content_block_stop", json!({"type": "content_block_stop", "index": 0})));
                                   text_open = false;
                               }
                               let content_block_index = next_content_block_index;
                               next_content_block_index += 1;
                               tool_call_blocks.insert(item_id.to_string(), content_block_index);
                               stop_reason = "tool_use".to_string();
                               yield Ok(anthropic_event("content_block_start", json!({
                                   "type": "content_block_start",
                                   "index": content_block_index,
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
                           if let Some(item_id) = value.get("item_id").and_then(Value::as_str)
                               && let Some(&content_block_index) = tool_call_blocks.get(item_id)
                               && let Some(partial) = value.get("delta").and_then(Value::as_str)
                               && !partial.is_empty() {
                                   yield Ok(anthropic_event("content_block_delta", json!({
                                       "type": "content_block_delta",
                                       "index": content_block_index,
                                       "delta": {"type": "input_json_delta", "partial_json": partial}
                                   })));
                               }
                       }
                       "response.output_item.done" => {
                           let item = value.get("item").cloned().unwrap_or_else(|| json!({}));
                           if item.get("type").and_then(Value::as_str) == Some("function_call")
                               && let Some(item_id) = item.get("id").and_then(Value::as_str)
                               && let Some(content_block_index) = tool_call_blocks.remove(item_id) {
                                   yield Ok(anthropic_event("content_block_stop", json!({"type": "content_block_stop", "index": content_block_index})));
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
    // : detect truncation from stop_reason.
               let (truncated, truncation_reason) = match stop_reason.as_str() {
                   "max_tokens" => (true, Some("max_tokens")),
                   _ => (false, None),
               };
               state.record_usage_with_status_and_target(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("completed"), true, 0, truncated, truncation_reason, ttfb_ms, routed_target);
           }
       };
    sse_response(Body::from_stream(body_stream))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn chat_sse_to_anthropic_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
    routed_target: Option<String>,
) -> Response {
    let token_owned = token.cloned();
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
           let mut next_content_block_index = 1i64;
           let mut text_block_open = true;
           let mut tool_call_blocks = HashMap::new();
           let mut open_tool_blocks = Vec::new();
           let mut ttfb_ms: Option<u64> = None;

           while let Some(chunk) = upstream_stream.next().await {
               let chunk = match chunk {
                   Ok(bytes) => {
                       if ttfb_ms.is_none() {
                           ttfb_ms = Some(request_start.elapsed().as_millis() as u64);
                       }
                       bytes
                   }
                   Err(err) => {
                       state.observe_stream_interruption("chat-to-anthropic", &err.to_string());
                    // 非 guard 系 error 补记：流中断也留 usage 行（ingress latency/ttfb+已抽 token）
                    state.record_usage_with_status_and_target(frontend_model.clone(), provider_id.clone(), endpoint.clone(), input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("upstream_interrupted"), true, 0, false, None, ttfb_ms, routed_target.clone());
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
                           let Some(tool_call_index) = call.get("index").and_then(Value::as_i64) else {
                               continue;
                           };
                           let function = call.get("function").cloned().unwrap_or_else(|| json!({}));
                           let name = function.get("name").and_then(Value::as_str).unwrap_or("");
                           if !name.is_empty() && !tool_call_blocks.contains_key(&tool_call_index) {
                               if text_block_open {
                                   yield Ok(anthropic_event("content_block_stop", json!({
                                       "type": "content_block_stop",
                                       "index": 0
                                   })));
                                   text_block_open = false;
                               }
                               let content_block_index = next_content_block_index;
                               next_content_block_index += 1;
                               tool_call_blocks.insert(tool_call_index, content_block_index);
                               open_tool_blocks.push(content_block_index);
                               yield Ok(anthropic_event("content_block_start", json!({
                                   "type": "content_block_start",
                                   "index": content_block_index,
                                   "content_block": {
                                       "type": "tool_use",
                                       "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                                       "name": name,
                                       "input": {}
                                   }
                               })));
                           }
                           if let Some(&content_block_index) = tool_call_blocks.get(&tool_call_index)
                               && let Some(arguments) = function.get("arguments").and_then(Value::as_str)
                               && !arguments.is_empty() {
                                   yield Ok(anthropic_event("content_block_delta", json!({
                                       "type": "content_block_delta",
                                       "index": content_block_index,
                                       "delta": { "type": "input_json_delta", "partial_json": arguments }
                                   })));
                               }
                       }
                   }
               }
           }

           if text_block_open {
               yield Ok(anthropic_event("content_block_stop", json!({
                   "type": "content_block_stop",
                   "index": 0
               })));
           }
           for content_block_index in open_tool_blocks {
               yield Ok(anthropic_event("content_block_stop", json!({
                   "type": "content_block_stop",
                   "index": content_block_index
               })));
           }
           yield Ok(anthropic_event("message_delta", json!({
               "type": "message_delta",
               "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
               "usage": { "output_tokens": output_tokens }
           })));
           yield Ok(anthropic_event("message_stop", json!({ "type": "message_stop" })));
           if input_tokens > 0 || output_tokens > 0 {
    // : detect truncation from stop_reason.
               let (truncated, truncation_reason) = match stop_reason.as_str() {
                   "max_tokens" => (true, Some("max_tokens")),
                   _ => (false, None),
               };
               state.record_usage_with_status_and_target(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("completed"), true, 0, truncated, truncation_reason, ttfb_ms, routed_target);
           }
       };
    sse_response(Body::from_stream(body_stream))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn anthropic_sse_to_chat_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
    routed_target: Option<String>,
) -> Response {
    let token_owned = token.cloned();
    let mut upstream_stream = upstream.bytes_stream();
    let state = state.clone();
    let body_stream = stream! {
           let mut buffer = String::new();
           let mut input_tokens = 0;
           let mut output_tokens = 0;
           let mut finish_reason = "stop".to_string();
           let mut next_tool_index = 0i64;
           let mut tool_call_indices = HashMap::new();
           let chat_id = convert::new_scoped_id("chatcmpl-");
           let chat_created = convert::unix_now_secs();
           let mut ttfb_ms: Option<u64> = None;
           while let Some(chunk) = upstream_stream.next().await {
               let chunk = match chunk {
                   Ok(bytes) => {
                       if ttfb_ms.is_none() {
                           ttfb_ms = Some(request_start.elapsed().as_millis() as u64);
                       }
                       bytes
                   }
                   Err(err) => {
                       state.observe_stream_interruption("anthropic-to-chat", &err.to_string());
                    // 非 guard 系 error 补记：流中断也留 usage 行（ingress latency/ttfb+已抽 token）
                    state.record_usage_with_status_and_target(frontend_model.clone(), provider_id.clone(), endpoint.clone(), input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("upstream_interrupted"), true, 0, false, None, ttfb_ms, routed_target.clone());
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
                               let Some(content_block_index) = value.get("index").and_then(Value::as_i64) else {
                                   continue;
                               };
                               let tool_call_index = next_tool_index;
                               next_tool_index += 1;
                               tool_call_indices.insert(content_block_index, tool_call_index);
                               yield Ok(chat_event(json!({
                                   "id": chat_id.clone(),
                                   "object": "chat.completion.chunk",
                                   "created": chat_created,
                                   "model": frontend_model,
                                   "choices": [{"index": 0, "delta": {"tool_calls": [{
                                       "index": tool_call_index,
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
                                           "id": chat_id.clone(),
                                           "object": "chat.completion.chunk",
                                           "created": chat_created,
                                           "model": frontend_model,
                                           "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": Value::Null}]
                                       })));
                                   }
                               }
                               "input_json_delta" => {
                                   if let Some(content_block_index) = value.get("index").and_then(Value::as_i64)
                                       && let Some(&tool_call_index) = tool_call_indices.get(&content_block_index)
                                       && let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                                       && !partial.is_empty() {
                                           yield Ok(chat_event(json!({
                                               "id": chat_id.clone(),
                                               "object": "chat.completion.chunk",
                                               "created": chat_created,
                                               "model": frontend_model,
                                               "choices": [{"index": 0, "delta": {"tool_calls": [{
                                                   "index": tool_call_index,
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
               "id": chat_id.clone(),
               "object": "chat.completion.chunk",
               "created": chat_created,
               "model": frontend_model,
               "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
               "usage": {"prompt_tokens": input_tokens, "completion_tokens": output_tokens, "total_tokens": input_tokens + output_tokens}
           })));
           yield Ok(Bytes::from("data: [DONE]\n\n"));
           if input_tokens > 0 || output_tokens > 0 {
    // : detect truncation from finish_reason.
               let (truncated, truncation_reason) = match finish_reason.as_str() {
                   "length" => (true, Some("max_tokens")),
                   "content_filter" => (true, Some("content_filter")),
                   _ => (false, None),
               };
               state.record_usage_with_status_and_target(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("completed"), true, 0, truncated, truncation_reason, ttfb_ms, routed_target);
           }
       };
    sse_response(Body::from_stream(body_stream))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn anthropic_sse_to_responses_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
    routed_target: Option<String>,
) -> Response {
    let token_owned = token.cloned();
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
           let mut stop_reason: Option<String> = None;
           // Tool-use streaming state: function_call items start at output_index 1
           // (the text message occupies index 0).
           let mut next_output_index = 1i64;
           let mut tool_calls = HashMap::new();
           let mut completed_tools: Vec<Value> = Vec::new();
           let mut ttfb_ms: Option<u64> = None;
           while let Some(chunk) = upstream_stream.next().await {
               let chunk = match chunk {
                   Ok(bytes) => {
                       if ttfb_ms.is_none() {
                           ttfb_ms = Some(request_start.elapsed().as_millis() as u64);
                       }
                       bytes
                   }
                   Err(err) => {
                       state.observe_stream_interruption("anthropic-to-responses", &err.to_string());
                    // 非 guard 系 error 补记：流中断也留 usage 行（ingress latency/ttfb+已抽 token）
                    state.record_usage_with_status_and_target(frontend_model.clone(), provider_id.clone(), endpoint.clone(), input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("upstream_interrupted"), true, 0, false, None, ttfb_ms, routed_target.clone());
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
                               let Some(content_block_index) = value.get("index").and_then(Value::as_i64) else {
                                   continue;
                               };
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
                               tool_calls.insert(
                                   content_block_index,
                                   (item_id.clone(), output_index, call_id.clone(), name.clone(), String::new()),
                               );
                               yield Ok(responses_event("response.output_item.added", json!({
                                   "type": "response.output_item.added",
                                   "output_index": output_index,
                                   "item": {
                                       "id": item_id,
                                       "type": "function_call",
                                       "call_id": call_id,
                                       "name": name,
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
                                   if let Some(content_block_index) = value.get("index").and_then(Value::as_i64)
                                       && let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                                       && !partial.is_empty()
                                       && let Some(tool) = tool_calls.get_mut(&content_block_index) {
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
                           if let Some(content_block_index) = value.get("index").and_then(Value::as_i64)
                               && let Some((item_id, output_index, call_id, name, arguments)) =
                                   tool_calls.remove(&content_block_index)
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
                           if let Some(reason) = value
                               .get("delta")
                               .and_then(|d| d.get("stop_reason"))
                               .and_then(Value::as_str)
                           {
                               stop_reason = Some(reason.to_string());
                           }
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
           let (status, incomplete_details) =
               convert::anthropic_stop_reason_to_responses_status(stop_reason.as_deref());
           let item_status = if status == "incomplete" { "incomplete" } else { "completed" };
           let message_item = json!({
               "id": message_id,
               "type": "message",
               "role": "assistant",
               "status": item_status,
               "content": [{"type": "output_text", "text": full_text, "annotations": []}]
           });
           yield Ok(responses_event("response.output_item.done", json!({
               "type": "response.output_item.done", "output_index": 0, "item": message_item
           })));
           let mut output_items = vec![message_item];
           for tool in completed_tools.iter_mut() {
               if status == "incomplete" && let Some(obj) = tool.as_object_mut() {
                   obj.insert("status".to_string(), json!("incomplete"));
               }
               output_items.push(tool.clone());
           }
           let terminal = if status == "incomplete" { "response.incomplete" } else { "response.completed" };
           yield Ok(responses_event(terminal, json!({
               "type": terminal,
               "response": {
                   "id": response_id,
                   "object": "response",
                   "status": status,
                   "model": frontend_model,
                   "output": output_items,
                   "incomplete_details": incomplete_details.unwrap_or(Value::Null),
                   "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens, "total_tokens": input_tokens + output_tokens}
               }
           })));
           if input_tokens > 0 || output_tokens > 0 {
    // : detect truncation from status.
               let (truncated, truncation_reason) = if status == "incomplete" {
                   let reason = match stop_reason.as_deref() {
                       Some("max_tokens") => Some("max_tokens"),
                       _ => Some("other"),
                   };
                   (true, reason)
               } else {
                   (false, None)
               };
               state.record_usage_with_status_and_target(frontend_model.clone(), provider_id, endpoint, input_tokens, output_tokens, Some(request_start.elapsed().as_millis() as i64), None, None, token_owned.as_ref(), Some("completed"), true, 0, truncated, truncation_reason, ttfb_ms, routed_target);
           }
       };
    sse_response(Body::from_stream(body_stream))
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

pub(super) async fn upstream_error_json(
    state: &AppState,
    token: Option<&crate::token::auth::TokenContext>,
    inbound_model: &str,
    upstream: reqwest::Response,
) -> Response {
    let (status, retry_after, message, text) = upstream_error_details_raw(upstream).await;
    state.record_client_error_for_token(token, inbound_model, status, &text, &message);
    with_retry_after(json_error(status, &message), retry_after)
}

/// Returns upstream error details including status, retry-after header,
/// flattened message, and the bounded raw error text for diagnostics and
/// error-class classification.
pub(super) async fn upstream_error_details_raw(
    upstream: reqwest::Response,
) -> (StatusCode, Option<HeaderValue>, String, String) {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let retry_after = upstream.headers().get(header::RETRY_AFTER).cloned();
    let text = upstream.text().await.unwrap_or_default();
    let message = flatten_upstream_error_message(&text);
    (status, retry_after, message, text)
}

pub(super) async fn upstream_error_anthropic(
    state: &AppState,
    token: Option<&crate::token::auth::TokenContext>,
    inbound_model: &str,
    upstream: reqwest::Response,
) -> Response {
    let (status, retry_after, message, text) = upstream_error_details_raw(upstream).await;
    state.record_client_error_for_token(token, inbound_model, status, &text, &message);
    with_retry_after(anthropic_error(status, &message), retry_after)
}

pub(super) async fn upstream_error_responses(
    state: &AppState,
    token: Option<&crate::token::auth::TokenContext>,
    inbound_model: &str,
    upstream: reqwest::Response,
) -> Response {
    let (status, retry_after, message, text) = upstream_error_details_raw(upstream).await;
    state.record_client_error_for_token(token, inbound_model, status, &text, &message);
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

pub(super) fn anthropic_error_with_code(status: StatusCode, message: &str, code: &str) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": { "type": anthropic_error_type(status), "message": message, "code": code }
        })),
    )
        .into_response()
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

pub(super) fn json_error_with_type_and_code(
    status: StatusCode,
    message: &str,
    err_type: &str,
    code: &str,
) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": err_type,
                "code": code
            }
        })),
    )
        .into_response()
}

pub(super) fn json_error_with_code(status: StatusCode, message: &str, code: &str) -> Response {
    json_error_with_type_and_code(status, message, "proxy_error", code)
}

pub(super) fn json_error(status: StatusCode, message: &str) -> Response {
    json_error_with_code(status, message, &error_code(status))
}

pub(super) fn responses_error_with_code(status: StatusCode, message: &str, code: &str) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": {
                "type": error_code(status),
                "message": message,
                "code": code
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

#[allow(dead_code)]
pub(crate) fn sanitize_finish_reason(reason: &str) -> String {
    const MAX_BYTES: usize = 64;
    let mut clean = String::new();
    for c in reason.chars() {
        if clean.len() >= MAX_BYTES {
            break;
        }
        if c.is_alphanumeric() || c == '_' || c == '-' {
            clean.push(c);
        } else if !clean.ends_with('_') {
            clean.push('_');
        }
    }
    clean.trim_matches('_').to_string()
}

#[allow(dead_code)]
pub(crate) fn digest_finish_message(message: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(message.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("len={},sha256={}", message.len(), hash)
}

#[allow(dead_code)]
pub(crate) fn compute_tools_digest(tools: Option<&[Value]>) -> (usize, String) {
    use sha2::{Digest, Sha256};
    let tools = match tools {
        Some(t) => t,
        None => return (0, "none".to_string()),
    };
    if tools.is_empty() {
        return (0, "none".to_string());
    }
    let count = tools.len();
    let mut hasher = Sha256::new();
    for tool in tools {
        hasher.update(tool.to_string().as_bytes());
    }
    let hash = hex::encode(hasher.finalize());
    (count, format!("sha256:{hash}"))
}

pub(super) fn cooldown_kind_for_status(status: StatusCode) -> &'static str {
    if status == StatusCode::TOO_MANY_REQUESTS {
        "rate_limit"
    } else if status == StatusCode::NOT_FOUND {
        "model_unavailable"
    } else if status == StatusCode::PAYMENT_REQUIRED {
        "auth_error"
    } else if status == StatusCode::REQUEST_TIMEOUT {
        "network"
    } else if status.is_server_error() {
        "server_error"
    } else {
        "client_error"
    }
}

pub(super) fn cooldown_kind_is_transient(kind: &str) -> bool {
    matches!(kind, "network" | "server_error" | "rate_limit")
}

pub(super) fn should_fallback_without_cooldown(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
}

/// 4xx client errors that should feed the protection fingerprint.
///
/// Excludes statuses with dedicated handling paths:
/// - 401/403: auth cooldown path (`should_fallback_without_cooldown`)
/// - 429: rate-limit cooldown path (`retry_after_duration`)
pub(super) fn is_protectable_client_error(status: StatusCode) -> bool {
    status.is_client_error()
        && status != StatusCode::UNAUTHORIZED
        && status != StatusCode::FORBIDDEN
        && status != StatusCode::TOO_MANY_REQUESTS
}

/// Statuses that have a dedicated cooldown+fallback path in `cooldown_duration_for_status`.
///
/// These should NOT be intercepted by the early-return because they
/// have their own cooldown behavior that must be preserved.
///
/// They are also exempt from both protection ledgers: a rescue by a later
/// candidate must not leave a count behind, and billing / model-catalog /
/// network failures are not the client's fault.
pub(super) fn has_dedicated_cooldown_path(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::PAYMENT_REQUIRED     // 402 → auth_error_seconds
        | StatusCode::NOT_FOUND           // 404 → model_unavailable_seconds
        | StatusCode::REQUEST_TIMEOUT // 408 → network_seconds
    )
}

pub(super) fn should_retry_server_error(status: StatusCode) -> bool {
    status.is_server_error()
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

pub(crate) fn capped_nonzero_duration(seconds: u64) -> Option<Duration> {
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
    } else if status == StatusCode::PAYMENT_REQUIRED {
        cooldown.auth_error_seconds
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

#[cfg(test)]
mod sse_aggregate_tests {
    use super::*;

    #[test]
    fn protectable_client_error_includes_4xx_except_dedicated_paths() {
        // 4xx that should feed the protection fingerprint
        assert!(is_protectable_client_error(StatusCode::BAD_REQUEST)); // 400
        assert!(is_protectable_client_error(StatusCode::METHOD_NOT_ALLOWED)); // 405
        assert!(is_protectable_client_error(
            StatusCode::from_u16(413).unwrap()
        )); // 413
        assert!(is_protectable_client_error(
            StatusCode::UNPROCESSABLE_ENTITY
        )); // 422
        assert!(is_protectable_client_error(StatusCode::CONFLICT)); // 409
        assert!(is_protectable_client_error(StatusCode::GONE)); // 410
        assert!(is_protectable_client_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        )); // 415

        // 4xx with dedicated handling paths — must NOT be protectable
        assert!(!is_protectable_client_error(StatusCode::UNAUTHORIZED)); // 401 → auth
        assert!(!is_protectable_client_error(StatusCode::FORBIDDEN)); // 403 → auth
        assert!(!is_protectable_client_error(StatusCode::TOO_MANY_REQUESTS)); // 429 → rate_limit

        // Non-4xx must not be protectable
        assert!(!is_protectable_client_error(StatusCode::OK)); // 200
        assert!(!is_protectable_client_error(
            StatusCode::INTERNAL_SERVER_ERROR
        )); // 500
        assert!(!is_protectable_client_error(StatusCode::BAD_GATEWAY)); // 502
        assert!(!is_protectable_client_error(
            StatusCode::SERVICE_UNAVAILABLE
        )); // 503
    }

    #[test]
    fn cooldown_duration_for_status_unchanged_for_generic_4xx() {
        let cooldown = crate::config::FallbackCooldownConfig::default();

        // Generic 4xx (405/413/422) do NOT get cooldown — they are request-level
        // errors where fallback would send the same bad body to another provider.
        // Instead they feed the fingerprint inline and return immediately.
        assert!(
            cooldown_duration_for_status(StatusCode::METHOD_NOT_ALLOWED, &cooldown).is_none(),
            "405 should not get cooldown (request-level error)"
        );
        assert!(
            cooldown_duration_for_status(StatusCode::UNPROCESSABLE_ENTITY, &cooldown).is_none(),
            "422 should not get cooldown (request-level error)"
        );
        assert!(
            cooldown_duration_for_status(StatusCode::from_u16(413).unwrap(), &cooldown).is_none(),
            "413 should not get cooldown (request-level error)"
        );

        // 400 still gets client_error cooldown (existing behavior preserved)
        assert!(
            cooldown_duration_for_status(StatusCode::BAD_REQUEST, &cooldown).is_some(),
            "400 should still get client_error cooldown"
        );

        // 401/403 still return None (dedicated path, no cooldown)
        assert!(
            cooldown_duration_for_status(StatusCode::UNAUTHORIZED, &cooldown).is_none(),
            "401 should not get cooldown from status"
        );
        assert!(
            cooldown_duration_for_status(StatusCode::FORBIDDEN, &cooldown).is_none(),
            "403 should not get cooldown from status"
        );

        // 5xx still uses server_error branch
        assert!(
            cooldown_duration_for_status(StatusCode::INTERNAL_SERVER_ERROR, &cooldown).is_some(),
            "500 should get server error cooldown"
        );
    }

    #[test]
    fn has_dedicated_cooldown_path_covers_existing_statuses() {
        // 400 no longer has a dedicated path — it feeds fingerprint + returns
        assert!(!has_dedicated_cooldown_path(StatusCode::BAD_REQUEST)); // 400
        assert!(has_dedicated_cooldown_path(StatusCode::PAYMENT_REQUIRED)); // 402
        assert!(has_dedicated_cooldown_path(StatusCode::NOT_FOUND)); // 404
        assert!(has_dedicated_cooldown_path(StatusCode::REQUEST_TIMEOUT)); // 408

        // Generic 4xx must NOT have dedicated path
        assert!(!has_dedicated_cooldown_path(StatusCode::METHOD_NOT_ALLOWED)); // 405
        assert!(!has_dedicated_cooldown_path(
            StatusCode::UNPROCESSABLE_ENTITY
        )); // 422
        assert!(!has_dedicated_cooldown_path(
            StatusCode::from_u16(413).unwrap()
        )); // 413
    }

    /// 变异测试：如果判据缩回仅 400，generic 4xx 测试必须失败。
    /// 这个测试通过显式检查 405/422 来确保判据不会被缩窄。
    #[test]
    fn mutation_guard_narrowing_to_400_only_would_fail() {
        let narrow = |s: StatusCode| s == StatusCode::BAD_REQUEST;
        // These would pass under the narrow predicate (correctly returning false)
        // but the real function must return true for them:
        assert!(is_protectable_client_error(StatusCode::METHOD_NOT_ALLOWED));
        assert!(!narrow(StatusCode::METHOD_NOT_ALLOWED)); // narrow would say false
        assert!(is_protectable_client_error(
            StatusCode::UNPROCESSABLE_ENTITY
        ));
        assert!(!narrow(StatusCode::UNPROCESSABLE_ENTITY)); // narrow would say false
    }
}
