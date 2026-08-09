use anyhow::Result;
use serde_json::{Map, Value, json};

use super::shared::*;

pub fn chat_to_anthropic_request(mut req: Value, upstream_model: &str) -> Result<Value> {
    let obj = req
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Chat Completions request must be a JSON object"))?;
    let mut out = Map::new();
    out.insert("model".to_string(), json!(upstream_model));
    copy_field(obj, &mut out, "stream", "stream");
    copy_field(obj, &mut out, "temperature", "temperature");
    copy_field(obj, &mut out, "top_p", "top_p");
    copy_field(obj, &mut out, "max_tokens", "max_tokens");
    copy_field(obj, &mut out, "stop", "stop_sequences");

    let mut messages = Vec::new();
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
            let text = anthropic_content_to_text(msg_obj.get("content"));
            if !text.is_empty() {
                out.insert("system".to_string(), json!(text));
            }
            continue;
        }
        if role == "tool" {
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": msg_obj.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                    "content": anthropic_content_to_text(msg_obj.get("content"))
                }]
            }));
            continue;
        }
        messages.push(json!({
            "role": if role == "assistant" { "assistant" } else { "user" },
            "content": chat_message_content_to_anthropic(msg_obj)
        }));
    }
    out.insert("messages".to_string(), Value::Array(messages));

    if let Some(Value::Array(tools)) = obj.get("tools") {
        let converted: Vec<Value> = tools.iter().filter_map(chat_tool_to_anthropic).collect();
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }
    if let Some(choice) = obj.get("tool_choice") {
        out.insert(
            "tool_choice".to_string(),
            chat_tool_choice_to_anthropic(choice),
        );
    }
    Ok(Value::Object(out))
}

pub fn anthropic_to_chat_response(resp: Value, frontend_model: &str) -> Value {
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl_llm_proxy");
    let mut content_text = String::new();
    let mut tool_calls = Vec::new();
    for part in resp
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(obj) = part.as_object() else {
            continue;
        };
        match obj.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => content_text.push_str(obj.get("text").and_then(Value::as_str).unwrap_or("")),
            "tool_use" => {
                let id = obj
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown");
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": obj.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                        "arguments": obj.get("input").cloned().unwrap_or_else(|| json!({})).to_string()
                    }
                }));
            }
            _ => {}
        }
    }
    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert("content".to_string(), json!(content_text));
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    json!({
        "id": id,
        "object": "chat.completion",
        "created": 0,
        "model": frontend_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": anthropic_stop_reason_to_chat(resp.get("stop_reason"))
        }],
        "usage": chat_usage_from_anthropic(resp.get("usage"))
    })
}

pub fn anthropic_to_chat(mut req: Value, upstream_model: &str) -> Result<Value> {
    let obj = req
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Anthropic request must be a JSON object"))?;

    let mut out = Map::new();
    out.insert("model".to_string(), json!(upstream_model));
    copy_field(obj, &mut out, "stream", "stream");
    copy_field(obj, &mut out, "temperature", "temperature");
    copy_field(obj, &mut out, "top_p", "top_p");
    copy_field(obj, &mut out, "top_k", "top_k");
    copy_field(obj, &mut out, "max_tokens", "max_tokens");
    copy_field(obj, &mut out, "stop_sequences", "stop");

    let mut messages = Vec::new();
    if let Some(system) = obj.get("system") {
        let text = anthropic_system_to_text(system);
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }

    if let Some(Value::Array(items)) = obj.get("messages") {
        for item in items {
            let Some(msg) = item.as_object() else {
                continue;
            };
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
            match role {
                "assistant" => messages.push(anthropic_assistant_to_chat(msg)),
                "user" => append_anthropic_user_to_chat(msg, &mut messages),
                other => messages.push(json!({
                    "role": other,
                    "content": anthropic_content_to_chat(msg.get("content"))
                })),
            }
        }
    }
    out.insert("messages".to_string(), Value::Array(messages));

    if let Some(Value::Array(tools)) = obj.get("tools") {
        let converted: Vec<Value> = tools.iter().filter_map(anthropic_tool_to_chat).collect();
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }
    if let Some(choice) = obj.get("tool_choice") {
        out.insert(
            "tool_choice".to_string(),
            anthropic_tool_choice_to_chat(choice),
        );
    }

    if out.get("stream").and_then(Value::as_bool) == Some(true) {
        out.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }

    Ok(Value::Object(out))
}

pub fn chat_to_anthropic(resp: Value, frontend_model: &str) -> Value {
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("msg_llm_proxy");
    let choice = resp
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let message = choice.get("message").cloned().unwrap_or_else(|| json!({}));

    let mut content = Vec::new();
    let text = extract_chat_content_text(message.get("content"));
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
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
            let input = function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .and_then(|args| serde_json::from_str::<Value>(args).ok())
                .unwrap_or_else(|| json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": frontend_model,
        "content": content,
        "stop_reason": chat_finish_reason_to_anthropic(choice.get("finish_reason")),
        "stop_sequence": Value::Null,
        "usage": anthropic_usage(resp.get("usage")),
    })
}

fn append_anthropic_user_to_chat(msg: &Map<String, Value>, messages: &mut Vec<Value>) {
    let content = msg.get("content");
    if let Some(parts) = content.and_then(Value::as_array) {
        let mut normal_parts = Vec::new();
        for part in parts {
            let Some(obj) = part.as_object() else {
                continue;
            };
            match obj.get("type").and_then(Value::as_str).unwrap_or("") {
                "tool_result" => {
                    if !normal_parts.is_empty() {
                        messages.push(json!({ "role": "user", "content": normal_parts }));
                        normal_parts = Vec::new();
                    }
                    let call_id = obj.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": anthropic_content_to_text(obj.get("content"))
                    }));
                }
                _ => normal_parts.push(anthropic_block_to_chat_content(part)),
            }
        }
        if !normal_parts.is_empty() {
            let content = if normal_parts.len() == 1
                && normal_parts[0].get("type").and_then(Value::as_str) == Some("text")
            {
                normal_parts[0]
                    .get("text")
                    .cloned()
                    .unwrap_or_else(|| json!(""))
            } else {
                Value::Array(normal_parts)
            };
            messages.push(json!({ "role": "user", "content": content }));
        }
    } else {
        messages.push(json!({ "role": "user", "content": anthropic_content_to_chat(content) }));
    }
}

fn anthropic_assistant_to_chat(msg: &Map<String, Value>) -> Value {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    if let Some(parts) = msg.get("content").and_then(Value::as_array) {
        for part in parts {
            let Some(obj) = part.as_object() else {
                continue;
            };
            match obj.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    if let Some(text) = obj.get("text").and_then(Value::as_str) {
                        text_parts.push(text.to_string());
                    }
                }
                "tool_use" => {
                    let id = obj
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("call_unknown");
                    let name = obj.get("name").and_then(Value::as_str).unwrap_or("unknown");
                    let input = obj.get("input").cloned().unwrap_or_else(|| json!({}));
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": input.to_string() }
                    }));
                }
                _ => {}
            }
        }
    } else if let Some(text) = msg.get("content").and_then(Value::as_str) {
        text_parts.push(text.to_string());
    }
    let mut out = Map::new();
    out.insert("role".to_string(), json!("assistant"));
    out.insert("content".to_string(), json!(text_parts.join("")));
    if !tool_calls.is_empty() {
        out.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    Value::Object(out)
}

fn anthropic_content_to_chat(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(text)) => json!(text),
        Some(Value::Array(parts)) => {
            Value::Array(parts.iter().map(anthropic_block_to_chat_content).collect())
        }
        _ => json!(""),
    }
}

pub(super) fn anthropic_block_to_chat_content(part: &Value) -> Value {
    let Some(obj) = part.as_object() else {
        return json!({ "type": "text", "text": "" });
    };
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => {
            json!({ "type": "text", "text": obj.get("text").and_then(Value::as_str).unwrap_or("") })
        }
        "image" => json!({
            "type": "image_url",
            "image_url": {
                "url": anthropic_source_to_openai_url(obj.get("source").and_then(Value::as_object), "image/png")
            }
        }),
        "document" => json!({ "type": "file", "file": anthropic_document_to_responses_file(part) }),
        _ => json!({ "type": "text", "text": anthropic_content_to_text(Some(part)) }),
    }
}

fn anthropic_content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| {
                part.as_object()
                    .and_then(|obj| obj.get("text"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| part.to_string())
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn anthropic_tool_to_chat(tool: &Value) -> Option<Value> {
    let obj = tool.as_object()?;
    let name = obj.get("name")?.as_str()?;
    let description = obj.get("description").cloned().unwrap_or_else(|| json!(""));
    let parameters = obj
        .get("input_schema")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    Some(json!({
        "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters }
    }))
}

fn anthropic_tool_choice_to_chat(choice: &Value) -> Value {
    let Some(obj) = choice.as_object() else {
        return choice.clone();
    };
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        "auto" => json!("auto"),
        "any" => json!("required"),
        "tool" => obj
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({ "type": "function", "function": { "name": name } }))
            .unwrap_or_else(|| json!("auto")),
        _ => json!("auto"),
    }
}

fn chat_finish_reason_to_anthropic(value: Option<&Value>) -> Value {
    let reason = match value.and_then(Value::as_str).unwrap_or("stop") {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => "end_turn",
    };
    json!(reason)
}

fn anthropic_usage(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json!({ "input_tokens": input, "output_tokens": output })
}
