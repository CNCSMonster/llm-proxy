use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use super::shared::*;

pub fn anthropic_to_responses_request(mut req: Value, upstream_model: &str) -> Result<Value> {
    let obj = req
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Anthropic request must be a JSON object"))?;

    let mut out = Map::new();
    out.insert("model".to_string(), json!(upstream_model));
    copy_field(obj, &mut out, "stream", "stream");
    copy_field(obj, &mut out, "temperature", "temperature");
    copy_field(obj, &mut out, "top_p", "top_p");
    copy_field(obj, &mut out, "max_tokens", "max_output_tokens");
    if let Some(system) = obj.get("system") {
        let text = anthropic_system_to_text(system);
        if !text.is_empty() {
            out.insert("instructions".to_string(), json!(text));
        }
    }

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
        let mut content = Vec::new();
        // Anthropic content may be a plain string (shorthand for a single
        // text block) or an array of content blocks. Handle both — a string
        // content currently slips through as_array()==None and the message
        // would be silently dropped, yielding an empty upstream request.
        let content_parts: &[Value] = match msg_obj.get("content") {
            Some(Value::Array(parts)) => parts.as_slice(),
            Some(Value::String(text)) => {
                content.push(json!({
                    "type": if role == "assistant" { "output_text" } else { "input_text" },
                    "text": text,
                }));
                &[]
            }
            _ => &[],
        };
        for part in content_parts {
            let Some(part_obj) = part.as_object() else {
                content.push(json!({ "type": "input_text", "text": part.to_string() }));
                continue;
            };
            match part_obj.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => content.push(json!({
                    "type": if role == "assistant" { "output_text" } else { "input_text" },
                    "text": part_obj.get("text").and_then(Value::as_str).unwrap_or("")
                })),
                "image" => content.push(anthropic_image_to_responses_image(part)),
                "document" => content.push(anthropic_document_to_responses_file(part)),
                "tool_use" => input.push(json!({
                    "type": "function_call",
                    "call_id": part_obj.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                    "name": part_obj.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                    "arguments": part_obj.get("input").cloned().unwrap_or_else(|| json!({})).to_string()
                })),
                "tool_result" => input.push(json!({
                    "type": "function_call_output",
                    "call_id": part_obj.get("tool_use_id").and_then(Value::as_str).unwrap_or(""),
                    "output": extract_tool_output(part_obj.get("content"))
                })),
                _ => content.push(json!({
                    "type": if role == "assistant" { "output_text" } else { "input_text" },
                    "text": part.to_string()
                })),
            }
        }
        if !content.is_empty() {
            input.push(json!({
                "type": "message",
                "role": if role == "assistant" { "assistant" } else { "user" },
                "content": content
            }));
        }
    }
    out.insert("input".to_string(), Value::Array(input));

    if let Some(Value::Array(tools)) = obj.get("tools") {
        let converted: Vec<Value> = tools
            .iter()
            .filter_map(anthropic_tool_to_responses)
            .collect();
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }
    if let Some(choice) = obj.get("tool_choice") {
        out.insert(
            "tool_choice".to_string(),
            anthropic_tool_choice_to_responses(choice),
        );
    }
    Ok(Value::Object(out))
}

pub fn responses_to_anthropic_response(resp: Value, frontend_model: &str) -> Value {
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("msg_llm_proxy");
    let mut content = Vec::new();
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
                        content.push(json!({ "type": "text", "text": text }));
                    }
                }
            }
            "function_call" => content.push(json!({
                "type": "tool_use",
                "id": item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or("call_unknown"),
                "name": item.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                "input": item.get("arguments").and_then(Value::as_str).and_then(|args| serde_json::from_str::<Value>(args).ok()).unwrap_or_else(|| json!({}))
            })),
            "reasoning" => {
                let thinking = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<String>();
                if !thinking.is_empty() {
                    content.push(json!({"type": "thinking", "thinking": thinking, "signature": item.get("signature").and_then(Value::as_str).unwrap_or("")}));
                }
            }
            _ => {}
        }
    }
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": frontend_model,
        "content": content,
        "stop_reason": responses_status_to_anthropic_stop(resp.get("status"), &resp),
        "stop_sequence": Value::Null,
        "usage": anthropic_usage_from_responses(resp.get("usage"))
    })
}

pub(super) fn anthropic_image_to_responses_image(part: &Value) -> Value {
    json!({
        "type": "input_image",
        "image_url": anthropic_source_to_openai_url(
            part.get("source").and_then(Value::as_object),
            "image/png"
        )
    })
}

fn anthropic_tool_to_responses(tool: &Value) -> Option<Value> {
    let obj = tool.as_object()?;
    let name = obj.get("name")?.as_str()?;
    Some(json!({
        "type": "function",
        "name": name,
        "description": obj.get("description").cloned().unwrap_or_else(|| json!("")),
        "parameters": obj.get("input_schema").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} }))
    }))
}

fn anthropic_tool_choice_to_responses(choice: &Value) -> Value {
    match choice
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| choice.as_str())
    {
        Some("auto") => json!("auto"),
        Some("any") => json!("required"),
        Some("tool") => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({ "type": "function", "function": { "name": name } }))
            .unwrap_or_else(|| json!("auto")),
        _ => json!("auto"),
    }
}

fn responses_status_to_anthropic_stop(status: Option<&Value>, resp: &Value) -> Value {
    let has_tool = resp
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"));
    if has_tool {
        return json!("tool_use");
    }
    match status.and_then(Value::as_str).unwrap_or("completed") {
        "incomplete" => json!("max_tokens"),
        _ => json!("end_turn"),
    }
}

fn anthropic_usage_from_responses(usage: Option<&Value>) -> Value {
    let Some(usage) = usage else {
        return json!({ "input_tokens": 0, "output_tokens": 0 });
    };
    json!({
        "input_tokens": usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(0),
        "output_tokens": usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(0)
    })
}

pub fn responses_to_anthropic(mut req: Value, upstream_model: &str) -> Result<Value> {
    let obj = req
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Responses request must be a JSON object"))?;
    let mut out = Map::new();
    out.insert("model".to_string(), json!(upstream_model));
    copy_field(obj, &mut out, "stream", "stream");
    copy_field(obj, &mut out, "temperature", "temperature");
    copy_field(obj, &mut out, "top_p", "top_p");
    copy_field(obj, &mut out, "max_output_tokens", "max_tokens");
    if let Some(instructions) = obj.get("instructions").and_then(Value::as_str)
        && !instructions.is_empty()
    {
        out.insert("system".to_string(), json!(instructions));
    }
    let messages = match obj.get("input") {
        Some(Value::String(text)) => {
            vec![json!({"role":"user","content":[{"type":"text","text":text}]})]
        }
        Some(Value::Array(items)) => responses_items_to_anthropic_messages(items),
        Some(other) => bail!("unsupported Responses input shape: {other}"),
        None => Vec::new(),
    };
    out.insert("messages".to_string(), Value::Array(messages));
    if let Some(Value::Array(tools)) = obj.get("tools") {
        let converted: Vec<Value> = tools
            .iter()
            .filter_map(responses_tool_to_anthropic)
            .collect();
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }
    // Convert reasoning.effort → Anthropic thinking parameter.
    if let Some(reasoning) = obj.get("reasoning").and_then(Value::as_object)
        && let Some(effort) = reasoning.get("effort").and_then(Value::as_str)
    {
        let budget = match effort {
            "none" => 0,
            "low" => 1024,
            "medium" => 4096,
            "high" => 16384,
            _ => 8192, // "auto" or unknown → default
        };
        if budget > 0 {
            out.insert(
                "thinking".to_string(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
        }
    }
    Ok(Value::Object(out))
}

pub fn anthropic_to_responses(resp: Value, frontend_model: &str) -> Value {
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_llm_proxy");
    let mut output = Vec::new();
    let mut text = String::new();
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
            "text" => text.push_str(obj.get("text").and_then(Value::as_str).unwrap_or("")),
            "thinking" => {
                // Flush accumulated text first.
                if !text.is_empty() {
                    output.push(responses_output_message(&text));
                    text.clear();
                }
                let thinking_text = obj.get("thinking").and_then(Value::as_str).unwrap_or("");
                if !thinking_text.is_empty() {
                    let mut item = serde_json::Map::new();
                    item.insert("type".to_string(), json!("reasoning"));
                    item.insert(
                        "summary".to_string(),
                        json!([{"type": "summary_text", "text": thinking_text}]),
                    );
                    if let Some(sig) = obj.get("signature").and_then(Value::as_str)
                        && !sig.is_empty()
                    {
                        item.insert("signature".to_string(), json!(sig));
                    }
                    output.push(Value::Object(item));
                }
            }
            "tool_use" => {
                if !text.is_empty() {
                    output.push(responses_output_message(&text));
                    text.clear();
                }
                let call_id = obj
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown");
                output.push(json!({
                    "id": call_id,
                    "call_id": call_id,
                    "type": "function_call",
                    "name": obj.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                    "arguments": obj.get("input").cloned().unwrap_or_else(|| json!({})).to_string(),
                    "status": "completed"
                }));
            }
            _ => {}
        }
    }
    if !text.is_empty() {
        output.push(responses_output_message(&text));
    }
    json!({
        "id": id,
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": frontend_model,
        "output": output,
        "usage": responses_usage_from_anthropic(resp.get("usage")),
    })
}
