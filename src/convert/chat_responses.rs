use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use super::shared::*;

/// Convert Responses API request to Chat Completions format.
///
/// Note: `previous_response_id` is silently dropped (ADR-036).
/// This field requires `store=true` to work, but llm-proxy defaults to `store=false`.
/// Clients (Codex, OpenAI SDK) don't send this field by default.
///
/// Default behavior: strict rejection (placeholder_enabled=false).
pub fn responses_to_chat(req: Value, upstream_model: &str) -> Result<Value> {
    let (value, _events) = responses_to_chat_with_placeholder(req, upstream_model, false, false)?;
    Ok(value)
}

/// Convert Responses API request to Chat Completions format with
/// placeholder opt-in and reasoning requirement.
/// Returns the converted value and any placeholder events
/// that were generated (for diagnostics recording).
pub fn responses_to_chat_with_placeholder(
    mut req: Value,
    upstream_model: &str,
    placeholder_enabled: bool,
    requires_reasoning: bool,
) -> Result<(Value, Vec<PlaceholderEvent>)> {
    let obj = req
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Responses request must be a JSON object"))?;

    let mut out = Map::new();
    out.insert("model".to_string(), json!(upstream_model));

    copy_field(obj, &mut out, "stream", "stream");
    copy_field(obj, &mut out, "temperature", "temperature");
    copy_field(obj, &mut out, "top_p", "top_p");
    copy_field(obj, &mut out, "max_output_tokens", "max_tokens");
    copy_field(obj, &mut out, "parallel_tool_calls", "parallel_tool_calls");
    copy_field(obj, &mut out, "tool_choice", "tool_choice");

    if let Some(reasoning) = obj.get("reasoning").and_then(Value::as_object)
        && let Some(effort) = reasoning.get("effort")
    {
        out.insert("reasoning_effort".to_string(), effort.clone());
    }

    let mut messages = Vec::new();
    let mut events = Vec::new();
    if let Some(instructions) = obj.get("instructions").and_then(Value::as_str)
        && !instructions.is_empty()
    {
        messages.push(json!({ "role": "system", "content": instructions }));
    }

    match obj.get("input") {
        Some(Value::String(text)) => {
            messages.push(json!({ "role": "user", "content": text }));
        }
        Some(Value::Array(items)) => {
            events = append_responses_items_as_chat_messages(
                items,
                &mut messages,
                placeholder_enabled,
                requires_reasoning,
            )?;
        }
        Some(other) => bail!("unsupported Responses input shape: {other}"),
        None => {}
    }
    out.insert("messages".to_string(), Value::Array(messages));

    if let Some(Value::Array(tools)) = obj.get("tools") {
        let converted = convert_tools(tools);
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }

    if out.get("stream").and_then(Value::as_bool) == Some(true) {
        out.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }

    Ok((Value::Object(out), events))
}

pub fn chat_to_responses_request(mut req: Value, upstream_model: &str) -> Result<Value> {
    let obj = req
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Chat Completions request must be a JSON object"))?;

    let mut out = Map::new();
    out.insert("model".to_string(), json!(upstream_model));
    copy_field(obj, &mut out, "stream", "stream");
    copy_field(obj, &mut out, "temperature", "temperature");
    copy_field(obj, &mut out, "top_p", "top_p");
    if let Some(value) = chat_max_tokens(obj) {
        out.insert("max_output_tokens".to_string(), value.clone());
    }
    copy_field(obj, &mut out, "parallel_tool_calls", "parallel_tool_calls");
    copy_field(obj, &mut out, "tool_choice", "tool_choice");

    let mut input = Vec::new();
    for msg in obj
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(msg_obj) = msg.as_object() else {
            continue;
        };
        let role = msg_obj
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" {
            let text = extract_chat_content_text(msg_obj.get("content"));
            if !text.is_empty() {
                let existing = out
                    .get("instructions")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let merged = if existing.is_empty() {
                    text
                } else {
                    format!("{existing}\n{text}")
                };
                out.insert("instructions".to_string(), json!(merged));
            }
            continue;
        }
        if role == "tool" {
            input.push(json!({
                "type": "function_call_output",
                "call_id": msg_obj.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                "output": extract_chat_content_text(msg_obj.get("content"))
            }));
            continue;
        }
        input.push(json!({
            "type": "message",
            "role": if role == "assistant" { "assistant" } else { "user" },
            "content": chat_content_to_responses_content(msg_obj.get("content"), role)?
        }));
        if role == "assistant"
            && let Some(tool_calls) = msg_obj.get("tool_calls").and_then(Value::as_array)
        {
            for call in tool_calls {
                let function = call.get("function").and_then(Value::as_object);
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                    "name": function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("unknown"),
                    "arguments": function.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("{}")
                }));
            }
        }
    }
    out.insert("input".to_string(), Value::Array(input));

    if let Some(Value::Array(tools)) = obj.get("tools") {
        let converted = chat_tools_to_responses_tools(tools);
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }
    Ok(Value::Object(out))
}

fn chat_tools_to_responses_tools(tools: &[Value]) -> Vec<Value> {
    convert_tools(tools)
        .into_iter()
        .filter_map(|tool| {
            let function = tool.get("function")?.as_object()?;
            Some(json!({
                "type": "function",
                "name": function.get("name").cloned().unwrap_or_else(|| json!("unknown")),
                "description": function.get("description").cloned().unwrap_or_else(|| json!("")),
                "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} }))
            }))
        })
        .collect()
}

pub fn responses_to_chat_response(resp: Value, frontend_model: &str) -> Value {
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| new_scoped_id("chatcmpl-"));
    let created = resp
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_else(unix_now_secs);
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    for item in resp
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "message" => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("output_text") | Some("text")
                    ) && let Some(text) = part.get("text").and_then(Value::as_str)
                    {
                        content.push_str(text);
                    }
                }
            }
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown");
                tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": item.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                        "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or("{}")
                    }
                }));
            }
            _ => {}
        }
    }
    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert("content".to_string(), json!(content));
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": frontend_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": responses_status_to_chat_finish(resp.get("status"))
        }],
        "usage": chat_usage_from_responses(resp.get("usage"))
    })
}

fn chat_content_to_responses_content(content: Option<&Value>, role: &str) -> Result<Value> {
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    match content {
        Some(Value::String(text)) => Ok(json!([{ "type": text_type, "text": text }])),
        Some(Value::Array(parts)) => Ok(Value::Array(
            parts
                .iter()
                .map(|part| chat_part_to_responses_part(part, text_type))
                .collect::<Result<Vec<_>>>()?,
        )),
        Some(other) => Ok(json!([{ "type": text_type, "text": other.to_string() }])),
        None => Ok(json!([])),
    }
}

fn chat_part_to_responses_part(part: &Value, text_type: &str) -> Result<Value> {
    let Some(obj) = part.as_object() else {
        return Ok(json!({ "type": text_type, "text": part.to_string() }));
    };
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => Ok(
            json!({ "type": text_type, "text": obj.get("text").and_then(Value::as_str).unwrap_or("") }),
        ),
        "image_url" => {
            let image = obj.get("image_url").and_then(Value::as_object);
            let image_url = image
                .and_then(|image| image.get("url"))
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Chat image_url.url must be a non-empty string"))?;
            let mut output = Map::new();
            output.insert("type".to_string(), json!("input_image"));
            output.insert("image_url".to_string(), json!(image_url));
            if let Some(detail) = image.and_then(|image| image.get("detail")) {
                output.insert("detail".to_string(), detail.clone());
            }
            Ok(Value::Object(output))
        }
        other => {
            let mut out = obj.clone();
            out.insert("type".to_string(), json!(other));
            Ok(Value::Object(out))
        }
    }
}

fn responses_status_to_chat_finish(status: Option<&Value>) -> Value {
    match status.and_then(Value::as_str).unwrap_or("completed") {
        "incomplete" => json!("length"),
        _ => json!("stop"),
    }
}

fn chat_usage_from_responses(usage: Option<&Value>) -> Value {
    let Some(usage) = usage else {
        return json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 });
    };
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(input + output)
    })
}

#[cfg(test)]
pub fn chat_to_responses(resp: Value, frontend_model: &str) -> Value {
    chat_to_responses_with_context(resp, frontend_model, None)
}

fn failed_response(
    id: &str,
    created: i64,
    frontend_model: &str,
    message: &str,
    request_context: Option<&Value>,
) -> Value {
    let mut resp_obj = Map::new();
    resp_obj.insert("id".to_string(), json!(id));
    resp_obj.insert("object".to_string(), json!("response"));
    resp_obj.insert("created_at".to_string(), json!(created));
    resp_obj.insert("status".to_string(), json!("failed"));
    resp_obj.insert("incomplete_details".to_string(), Value::Null);
    resp_obj.insert("model".to_string(), json!(frontend_model));
    resp_obj.insert("output".to_string(), json!([]));
    resp_obj.insert(
        "error".to_string(),
        json!({
            "code": "server_error",
            "message": message
        }),
    );
    apply_request_context_defaults(&mut resp_obj, request_context);
    Value::Object(resp_obj)
}

pub fn chat_to_responses_with_context(
    resp: Value,
    frontend_model: &str,
    request_context: Option<&Value>,
) -> Value {
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| new_scoped_id("resp_"));
    let id: &str = &id;
    let created = resp
        .get("created")
        .and_then(Value::as_i64)
        .unwrap_or_else(unix_now_secs);

    if let Some(err) = resp.get("error")
        && (!err.is_null() && err != &json!({}))
    {
        return failed_response(
            id,
            created,
            frontend_model,
            "Upstream service error",
            request_context,
        );
    }

    let choices = match resp.get("choices").and_then(Value::as_array) {
        Some(c) if !c.is_empty() => c,
        _ => {
            return failed_response(
                id,
                created,
                frontend_model,
                "Missing choices in upstream response",
                request_context,
            );
        }
    };

    let choice = match choices[0].as_object() {
        Some(obj) => obj,
        None => {
            return failed_response(
                id,
                created,
                frontend_model,
                "Malformed choice in upstream response",
                request_context,
            );
        }
    };

    let message = match choice.get("message").and_then(Value::as_object) {
        Some(m) => m,
        None => {
            return failed_response(
                id,
                created,
                frontend_model,
                "Malformed message in upstream response",
                request_context,
            );
        }
    };

    let finish_reason = choice.get("finish_reason").and_then(Value::as_str);

    let valid_finish = matches!(
        finish_reason,
        Some("stop") | Some("tool_calls") | Some("length") | Some("content_filter")
    );

    if !valid_finish {
        return failed_response(
            id,
            created,
            frontend_model,
            "Missing or invalid finish reason from upstream",
            request_context,
        );
    }
    let finish_reason = finish_reason.unwrap();
    let is_truncated = finish_reason == "length" || finish_reason == "content_filter";
    let item_status = if is_truncated {
        "incomplete"
    } else {
        "completed"
    };

    let mut output = Vec::new();

    if let Some(reasoning_val) = message.get("reasoning_content")
        && !reasoning_val.is_null()
    {
        let Some(reasoning) = reasoning_val.as_str() else {
            return failed_response(
                id,
                created,
                frontend_model,
                "Malformed reasoning_content in upstream response",
                request_context,
            );
        };
        if !reasoning.is_empty() {
            let rs_id = format!("rs_{}", uuid::Uuid::new_v4().as_simple());
            output.push(json!({
                "id": rs_id,
                "type": "reasoning",
                "status": item_status,
                "content": [{
                    "type": "reasoning_text",
                    "text": reasoning
                }],
                "summary": []
            }));
        }
    }

    let text = extract_chat_content_text(message.get("content"));
    if !text.is_empty() {
        let msg_id = format!("msg_{}", uuid::Uuid::new_v4().as_simple());
        output.push(json!({
            "id": msg_id,
            "type": "message",
            "role": "assistant",
            "status": item_status,
            "content": [{
                "type": "output_text",
                "text": text,
                "annotations": []
            }]
        }));
    }

    if let Some(tool_calls_val) = message.get("tool_calls") {
        let tool_calls = match tool_calls_val.as_array() {
            Some(arr) => arr,
            None => {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Malformed tool_calls array in upstream response",
                    request_context,
                );
            }
        };

        let mut seen_call_ids = std::collections::HashSet::new();
        for call in tool_calls {
            let Some(call_obj) = call.as_object() else {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Malformed tool call object in upstream response",
                    request_context,
                );
            };

            let Some(call_id) = call_obj.get("id").and_then(Value::as_str) else {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Tool call missing id in upstream response",
                    request_context,
                );
            };
            if call_id.is_empty() {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Tool call has empty id in upstream response",
                    request_context,
                );
            }
            if !seen_call_ids.insert(call_id) {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Duplicate tool call id in upstream response",
                    request_context,
                );
            }

            let Some(call_type) = call_obj.get("type").and_then(Value::as_str) else {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Invalid tool call type in upstream response",
                    request_context,
                );
            };
            if call_type != "function" {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Unsupported tool call type in upstream response",
                    request_context,
                );
            }

            let Some(function_obj) = call_obj.get("function").and_then(Value::as_object) else {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Tool call missing function object in upstream response",
                    request_context,
                );
            };

            let Some(name) = function_obj.get("name").and_then(Value::as_str) else {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Tool call function missing name in upstream response",
                    request_context,
                );
            };
            if name.is_empty() {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Tool call function has empty name in upstream response",
                    request_context,
                );
            }

            let Some(arguments) = function_obj.get("arguments").and_then(Value::as_str) else {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Tool call function missing arguments in upstream response",
                    request_context,
                );
            };

            let is_valid_json = serde_json::from_str::<Value>(arguments).is_ok();
            if !is_valid_json && !is_truncated {
                return failed_response(
                    id,
                    created,
                    frontend_model,
                    "Tool call function arguments is not valid JSON",
                    request_context,
                );
            }

            let item_id = format!("fc_{}", uuid::Uuid::new_v4().as_simple());
            output.push(json!({
                "id": item_id,
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
                "status": item_status
            }));
        }
    }

    let (status, incomplete_details) = match finish_reason {
        "stop" | "tool_calls" => ("completed", None),
        "length" => ("incomplete", Some(json!({ "reason": "max_output_tokens" }))),
        "content_filter" => ("incomplete", Some(json!({ "reason": "content_filter" }))),
        _ => unreachable!(),
    };

    let mut resp_obj = Map::new();
    resp_obj.insert("id".to_string(), json!(id));
    resp_obj.insert("object".to_string(), json!("response"));
    resp_obj.insert("created_at".to_string(), json!(created));
    resp_obj.insert("status".to_string(), json!(status));
    resp_obj.insert("model".to_string(), json!(frontend_model));
    resp_obj.insert("output".to_string(), Value::Array(output));
    resp_obj.insert("error".to_string(), Value::Null);
    resp_obj.insert(
        "incomplete_details".to_string(),
        incomplete_details.unwrap_or(Value::Null),
    );

    match map_responses_usage(resp.get("usage")) {
        Ok(usage) => {
            resp_obj.insert("usage".to_string(), usage);
        }
        Err(issues) => tracing::warn!(
            target: "llm_proxy::convert",
            correlation_id = %id,
            missing_fields = ?issues.missing_fields,
            invalid_fields = ?issues.invalid_fields,
            "upstream usage incomplete or invalid, omitted client usage without fabricating counts"
        ),
    }

    apply_request_context_defaults(&mut resp_obj, request_context);

    Value::Object(resp_obj)
}

#[derive(Default)]
pub(crate) struct UsageValidationIssues {
    pub(crate) missing_fields: Vec<&'static str>,
    pub(crate) invalid_fields: Vec<&'static str>,
}

impl UsageValidationIssues {
    fn object<'a>(
        &mut self,
        value: Option<&'a Value>,
        field: &'static str,
    ) -> Option<&'a Map<String, Value>> {
        match value {
            None => self.missing_fields.push(field),
            Some(Value::Object(object)) => return Some(object),
            Some(_) => self.invalid_fields.push(field),
        }
        None
    }

    fn counter(&mut self, value: Option<&Value>, field: &'static str) -> Option<i64> {
        match value {
            None => self.missing_fields.push(field),
            Some(value) => match value.as_i64().filter(|count| *count >= 0) {
                Some(count) => return Some(count),
                None => self.invalid_fields.push(field),
            },
        }
        None
    }
}

pub(crate) fn map_responses_usage(
    usage_val: Option<&Value>,
) -> std::result::Result<Value, UsageValidationIssues> {
    // ResponseUsage requires all three base counts, both detail objects, and
    // their counters (including cache_write_tokens). Validate once; never fill
    // gaps with zeros or the proxy's separately approved local accounting sum.
    let mut issues = UsageValidationIssues::default();
    let Some(usage) = issues.object(usage_val, "usage") else {
        return Err(issues);
    };
    let input = issues.counter(usage.get("prompt_tokens"), "prompt_tokens");
    let output = issues.counter(usage.get("completion_tokens"), "completion_tokens");
    let total = issues.counter(usage.get("total_tokens"), "total_tokens");
    let input_details = issues
        .object(usage.get("prompt_tokens_details"), "prompt_tokens_details")
        .map(|details| {
            let cache_write = issues.counter(
                details.get("cache_write_tokens"),
                "prompt_tokens_details.cache_write_tokens",
            );
            let cached = issues.counter(
                details.get("cached_tokens"),
                "prompt_tokens_details.cached_tokens",
            );
            json!({"cache_write_tokens": cache_write, "cached_tokens": cached})
        });
    let output_details = issues
        .object(
            usage.get("completion_tokens_details"),
            "completion_tokens_details",
        )
        .map(|details| {
            let reasoning = issues.counter(
                details.get("reasoning_tokens"),
                "completion_tokens_details.reasoning_tokens",
            );
            json!({"reasoning_tokens": reasoning})
        });
    if !issues.missing_fields.is_empty() || !issues.invalid_fields.is_empty() {
        return Err(issues);
    }
    Ok(json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": total,
        "input_tokens_details": input_details,
        "output_tokens_details": output_details
    }))
}

pub(crate) fn apply_request_context_defaults(
    resp_obj: &mut Map<String, Value>,
    request_context: Option<&Value>,
) {
    let req = request_context.and_then(Value::as_object);
    let tools = req
        .and_then(|req| req.get("tools"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let default_choice = if tools.as_array().is_some_and(|tools| !tools.is_empty()) {
        "auto"
    } else {
        "none"
    };
    // These envelope fields are required even when no tools were requested.
    // Defaults depend on the request, never on generated function_call items.
    resp_obj.insert(
        "tool_choice".to_string(),
        req.and_then(|req| req.get("tool_choice"))
            .cloned()
            .unwrap_or_else(|| json!(default_choice)),
    );
    resp_obj.insert(
        "parallel_tool_calls".to_string(),
        req.and_then(|req| req.get("parallel_tool_calls"))
            .cloned()
            .unwrap_or(Value::Bool(true)),
    );
    resp_obj.insert("tools".to_string(), tools);
}
