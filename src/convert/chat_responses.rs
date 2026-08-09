use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use super::shared::*;

pub fn responses_to_chat(mut req: Value, upstream_model: &str) -> Result<Value> {
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
            append_responses_items_as_chat_messages(items, &mut messages)?;
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

    Ok(Value::Object(out))
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
    copy_field(obj, &mut out, "max_tokens", "max_output_tokens");
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
            "content": chat_content_to_responses_content(msg_obj.get("content"), role)
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
        .unwrap_or("chatcmpl_llm_proxy");
    let created = resp.get("created_at").and_then(Value::as_i64).unwrap_or(0);
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

fn chat_content_to_responses_content(content: Option<&Value>, role: &str) -> Value {
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    match content {
        Some(Value::String(text)) => json!([{ "type": text_type, "text": text }]),
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .map(|part| chat_part_to_responses_part(part, text_type))
                .collect(),
        ),
        Some(other) => json!([{ "type": text_type, "text": other.to_string() }]),
        None => json!([]),
    }
}

fn chat_part_to_responses_part(part: &Value, text_type: &str) -> Value {
    let Some(obj) = part.as_object() else {
        return json!({ "type": text_type, "text": part.to_string() });
    };
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => {
            json!({ "type": text_type, "text": obj.get("text").and_then(Value::as_str).unwrap_or("") })
        }
        "image_url" => {
            json!({ "type": "input_image", "image_url": obj.get("image_url").cloned().unwrap_or_else(|| json!({})) })
        }
        other => {
            let mut out = obj.clone();
            out.insert("type".to_string(), json!(other));
            Value::Object(out)
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

pub fn chat_to_responses(resp: Value, frontend_model: &str) -> Value {
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_llm_proxy");
    let created = resp.get("created").and_then(Value::as_i64).unwrap_or(0);
    let choice = resp
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let message = choice.get("message").cloned().unwrap_or_else(|| json!({}));

    let mut output = Vec::new();
    let text = extract_chat_content_text(message.get("content"));
    if !text.is_empty() {
        output.push(json!({
            "id": format!("msg_{created}"),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": text,
                "annotations": []
            }]
        }));
    }

    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("call_unknown");
            let function = call.get("function").and_then(Value::as_object);
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let arguments = function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");
            output.push(json!({
                "id": id,
                "call_id": id,
                "type": "function_call",
                "name": name,
                "arguments": arguments,
                "status": "completed"
            }));
        }
    }

    json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": "completed",
        "model": frontend_model,
        "output": output,
        "usage": responses_usage(resp.get("usage")),
    })
}
