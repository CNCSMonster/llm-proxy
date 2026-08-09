use anyhow::Result;
use serde_json::{Map, Value, json};

// anthropic_system_to_text moved here from chat_anthropic because both
// anthropic_responses and chat_anthropic call it.
pub(super) fn anthropic_system_to_text(system: &Value) -> String {
    match system {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.as_object()?
                    .get("text")?
                    .as_str()
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(super) fn copy_field(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    from: &str,
    to: &str,
) {
    if let Some(value) = input.get(from)
        && !value.is_null()
    {
        out.insert(to.to_string(), value.clone());
    }
}

pub(super) fn append_responses_items_as_chat_messages(
    items: &[Value],
    messages: &mut Vec<Value>,
) -> Result<()> {
    let mut pending_tool_calls: Vec<Value> = Vec::new();

    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("");

        match item_type {
            "function_call" => {
                let call_id = obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown");
                let name = obj.get("name").and_then(Value::as_str).unwrap_or("unknown");
                let arguments = normalize_arguments(obj.get("arguments"));
                pending_tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments
                    }
                }));
            }
            "shell_call" | "local_shell_call" => {
                let call_id = obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown");
                let arguments = obj
                    .get("action")
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                pending_tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": "shell",
                        "arguments": arguments
                    }
                }));
            }
            "function_call_output" | "shell_call_output" | "local_shell_call_output" => {
                flush_pending_tool_calls(messages, &mut pending_tool_calls);
                let call_id = obj.get("call_id").and_then(Value::as_str).unwrap_or("");
                let output = extract_tool_output(obj.get("output"));
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output
                }));
            }
            _ => {
                let role = obj
                    .get("role")
                    .and_then(Value::as_str)
                    .map(|role| if role == "developer" { "system" } else { role })
                    .unwrap_or("user");
                let content = responses_content_to_chat(obj.get("content"));
                if role == "assistant" && is_empty_content(&content) {
                    continue;
                }
                flush_pending_tool_calls(messages, &mut pending_tool_calls);
                messages.push(json!({ "role": role, "content": content }));
            }
        }
    }

    flush_pending_tool_calls(messages, &mut pending_tool_calls);
    Ok(())
}

pub(super) fn flush_pending_tool_calls(
    messages: &mut Vec<Value>,
    pending_tool_calls: &mut Vec<Value>,
) {
    if pending_tool_calls.is_empty() {
        return;
    }
    messages.push(json!({
        "role": "assistant",
        "tool_calls": std::mem::take(pending_tool_calls)
    }));
}

pub(super) fn responses_content_to_chat(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(text)) => json!(text),
        Some(Value::Array(parts)) => {
            let converted: Vec<Value> = parts
                .iter()
                .filter_map(|part| {
                    let obj = part.as_object()?;
                    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
                        "input_text" | "output_text" => obj
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|text| json!({ "type": "text", "text": text })),
                        "input_image" => obj
                            .get("image_url")
                            .and_then(Value::as_str)
                            .map(|url| json!({ "type": "image_url", "image_url": { "url": url } })),
                        _ => None,
                    }
                })
                .collect();
            if converted.len() == 1
                && converted[0].get("type").and_then(Value::as_str) == Some("text")
            {
                converted[0]
                    .get("text")
                    .cloned()
                    .unwrap_or_else(|| json!(""))
            } else if converted.is_empty() {
                json!("")
            } else {
                Value::Array(converted)
            }
        }
        _ => json!(""),
    }
}

pub(super) fn responses_items_to_anthropic_messages(items: &[Value]) -> Vec<Value> {
    let mut messages = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        match obj.get("type").and_then(Value::as_str).unwrap_or("") {
            "message" => {
                let role = obj.get("role").and_then(Value::as_str).unwrap_or("user");
                messages.push(json!({
                    "role": if role == "assistant" { "assistant" } else { "user" },
                    "content": responses_content_to_anthropic(obj.get("content"))
                }));
            }
            "function_call" => messages.push(json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": obj.get("call_id").or_else(|| obj.get("id")).and_then(Value::as_str).unwrap_or("call_unknown"),
                    "name": obj.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                    "input": normalize_arguments_value(obj.get("arguments"))
                }]
            })),
            "function_call_output" | "shell_call_output" | "local_shell_call_output" => {
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": obj.get("call_id").and_then(Value::as_str).unwrap_or(""),
                        "content": extract_tool_output(obj.get("output"))
                    }]
                }));
            }
            _ => {}
        }
    }
    messages
}

pub(super) fn responses_content_to_anthropic(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(text)) => json!([{ "type": "text", "text": text }]),
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .filter_map(|part| {
                    let obj = part.as_object()?;
                    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
                        "input_text" | "output_text" => obj
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|text| json!({ "type": "text", "text": text })),
                        "input_image" => obj.get("image_url").and_then(Value::as_str).map(|url| {
                            json!({
                                "type": "image",
                                "source": image_url_to_anthropic_source(url)
                            })
                        }),
                        "input_file" | "document" => responses_file_to_anthropic_document(obj),
                        _ => None,
                    }
                })
                .collect(),
        ),
        _ => json!([{ "type": "text", "text": "" }]),
    }
}

pub(super) fn responses_output_message(text: &str) -> Value {
    json!({
        "id": "msg_llm_proxy",
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{
            "type": "output_text",
            "text": text,
            "annotations": []
        }]
    })
}

pub(super) fn responses_tool_to_anthropic(tool: &Value) -> Option<Value> {
    let obj = tool.as_object()?;
    if obj.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    let function = obj
        .get("function")
        .and_then(Value::as_object)
        .unwrap_or(obj);
    let name = function.get("name")?.as_str()?;
    Some(json!({
        "name": name,
        "description": function.get("description").cloned().unwrap_or_else(|| json!("")),
        "input_schema": function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }))
    }))
}

/// Anthropic image source from an OpenAI-style image URL. Data URIs carry
/// their media type; plain http(s) URLs map to Anthropic's URL source.
/// The adapter intentionally does not perform local network fetches; URL
/// fetch policy belongs to the upstream provider/client contract.
pub(super) fn image_url_to_anthropic_source(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').unwrap_or(("", rest));
        let media_type = meta
            .split(';')
            .next()
            .filter(|media| !media.is_empty())
            .unwrap_or("image/png");
        return json!({
            "type": "base64",
            "media_type": media_type,
            "data": data
        });
    }
    json!({ "type": "url", "url": url })
}

pub(super) fn responses_file_to_anthropic_document(obj: &Map<String, Value>) -> Option<Value> {
    if let Some(file_data) = obj.get("file_data").and_then(Value::as_str) {
        return Some(json!({
            "type": "document",
            "source": file_data_to_anthropic_source(file_data),
            "title": obj.get("filename").or_else(|| obj.get("name")).cloned().unwrap_or_else(|| json!("document"))
        }));
    }
    if let Some(file_url) = obj.get("file_url").and_then(Value::as_str) {
        return Some(json!({
            "type": "document",
            "source": { "type": "url", "url": file_url },
            "title": obj.get("filename").or_else(|| obj.get("name")).cloned().unwrap_or_else(|| json!("document"))
        }));
    }
    None
}

pub(super) fn file_data_to_anthropic_source(file_data: &str) -> Value {
    if let Some(rest) = file_data.strip_prefix("data:") {
        let (meta, data) = rest
            .split_once(',')
            .unwrap_or(("application/pdf;base64", rest));
        let media_type = meta
            .split(';')
            .next()
            .filter(|media| !media.is_empty())
            .unwrap_or("application/pdf");
        return json!({ "type": "base64", "media_type": media_type, "data": data });
    }
    json!({ "type": "base64", "media_type": "application/pdf", "data": file_data })
}

pub(super) fn anthropic_source_to_openai_url(
    source: Option<&Map<String, Value>>,
    default_media_type: &str,
) -> String {
    let Some(source) = source else {
        return String::new();
    };
    if source.get("type").and_then(Value::as_str) == Some("url") {
        return source
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    let media_type = source
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or(default_media_type);
    let data = source.get("data").and_then(Value::as_str).unwrap_or("");
    format!("data:{media_type};base64,{data}")
}

pub(super) fn anthropic_document_to_responses_file(part: &Value) -> Value {
    let source = part.get("source").and_then(Value::as_object);
    let filename = part
        .get("title")
        .or_else(|| part.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("document");
    if source.and_then(|s| s.get("type")).and_then(Value::as_str) == Some("url") {
        return json!({
            "type": "input_file",
            "filename": filename,
            "file_url": source.and_then(|s| s.get("url")).and_then(Value::as_str).unwrap_or("")
        });
    }
    json!({
        "type": "input_file",
        "filename": filename,
        "file_data": anthropic_source_to_openai_url(source, "application/pdf")
    })
}

/// Go `normalizeToolInputObject` parity: tool input/arguments normalize to a
/// JSON object; missing, null, empty, unparsable, or non-object values become
/// `{}` because strict providers reject anything else.
pub(super) fn normalize_tool_input_object(value: Option<&Value>) -> Value {
    let parsed = match value {
        None | Some(Value::Null) => return json!({}),
        Some(Value::String(text)) => {
            if text.is_empty() {
                return json!({});
            }
            match serde_json::from_str(text) {
                Ok(parsed) => parsed,
                Err(_) => return json!({}),
            }
        }
        Some(value) => value.clone(),
    };
    if parsed.is_object() {
        parsed
    } else {
        json!({})
    }
}

pub(super) fn normalize_arguments_value(value: Option<&Value>) -> Value {
    normalize_tool_input_object(value)
}

pub(super) fn responses_usage_from_anthropic(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json!({ "input_tokens": input, "output_tokens": output, "total_tokens": input + output })
}

pub(super) fn chat_message_content_to_anthropic(msg: &Map<String, Value>) -> Value {
    let mut content = match msg.get("content") {
        Some(Value::String(text)) => json!([{ "type": "text", "text": text }]),
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .filter_map(|part| {
                    let obj = part.as_object()?;
                    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text" => obj
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|text| json!({ "type": "text", "text": text })),
                        "image_url" => obj
                            .get("image_url")
                            .and_then(|v| v.get("url"))
                            .and_then(Value::as_str)
                            .map(|url| {
                                json!({
                                    "type": "image",
                                    "source": image_url_to_anthropic_source(url)
                                })
                            }),
                        _ => None,
                    }
                })
                .collect(),
        ),
        _ => json!([]),
    };
    if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
        let mut parts = content.as_array().cloned().unwrap_or_default();
        for call in tool_calls {
            let function = call.get("function").and_then(Value::as_object);
            parts.push(json!({
                "type": "tool_use",
                "id": call.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                "name": function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("unknown"),
                "input": normalize_arguments_value(function.and_then(|f| f.get("arguments")))
            }));
        }
        content = Value::Array(parts);
    }
    content
}

pub(super) fn chat_tool_to_anthropic(tool: &Value) -> Option<Value> {
    responses_tool_to_anthropic(tool)
}

pub(super) fn chat_tool_choice_to_anthropic(choice: &Value) -> Value {
    responses_tool_choice_to_anthropic(choice)
}

pub(super) fn responses_tool_choice_to_anthropic(choice: &Value) -> Value {
    match choice.as_str() {
        Some("auto") => json!({ "type": "auto" }),
        Some("required") => json!({ "type": "any" }),
        Some("none") => json!({ "type": "auto" }),
        _ => choice
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .map(|name| json!({ "type": "tool", "name": name }))
            .unwrap_or_else(|| json!({ "type": "auto" })),
    }
}

pub(super) fn anthropic_stop_reason_to_chat(reason: Option<&Value>) -> Value {
    let reason = match reason.and_then(Value::as_str).unwrap_or("end_turn") {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        _ => "stop",
    };
    json!(reason)
}

pub(super) fn chat_usage_from_anthropic(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json!({ "prompt_tokens": input, "completion_tokens": output, "total_tokens": input + output })
}

pub(super) fn convert_tools(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for tool in tools {
        let Some(obj) = tool.as_object() else {
            continue;
        };
        match obj.get("type").and_then(Value::as_str).unwrap_or("") {
            "function" => {
                if let Some(converted) = convert_function_tool(None, obj) {
                    out.push(converted);
                }
            }
            "shell" | "local_shell" => {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": "shell",
                        "description": "Execute shell commands in the local terminal.",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "command": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "workdir": { "type": "string" }
                            },
                            "required": ["command"]
                        }
                    }
                }));
            }
            "namespace" => {
                let namespace = obj.get("name").and_then(Value::as_str);
                if let Some(children) = obj.get("tools").and_then(Value::as_array) {
                    for child in children {
                        if let Some(child_obj) = child.as_object()
                            && let Some(converted) = convert_function_tool(namespace, child_obj)
                        {
                            out.push(converted);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub(super) fn convert_function_tool(
    namespace: Option<&str>,
    obj: &Map<String, Value>,
) -> Option<Value> {
    let func = obj.get("function").and_then(Value::as_object);
    let raw_name = obj
        .get("name")
        .or_else(|| func.and_then(|f| f.get("name")))
        .and_then(Value::as_str)?;
    let name = namespace
        .map(|ns| format!("{ns}__{raw_name}"))
        .unwrap_or_else(|| raw_name.to_string());
    let description = obj
        .get("description")
        .or_else(|| func.and_then(|f| f.get("description")))
        .cloned()
        .unwrap_or_else(|| json!(""));
    let parameters = obj
        .get("parameters")
        .or_else(|| func.and_then(|f| f.get("parameters")))
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));

    Some(json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters
        }
    }))
}

/// Go `normalizeToolArguments` parity: the Chat `function.arguments` string
/// must always encode a JSON object.
pub(super) fn normalize_arguments(value: Option<&Value>) -> String {
    normalize_tool_input_object(value).to_string()
}

pub(super) fn extract_tool_output(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.as_object()
                    .and_then(|obj| obj.get("stdout").or_else(|| obj.get("text")))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

pub(super) fn is_empty_content(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

pub(crate) fn extract_text_from_content(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                if let Some(text) = part.as_str() {
                    return Some(text.to_string());
                }
                let obj = part.as_object()?;
                obj.get("text")
                    .or_else(|| obj.get("input_text"))
                    .or_else(|| obj.get("output_text"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(Value::Object(obj)) => obj
            .get("text")
            .or_else(|| obj.get("input_text"))
            .or_else(|| obj.get("output_text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

#[allow(dead_code)] // used by tests
pub(crate) fn normalize_tool_calls(calls: &[Value]) -> Vec<Value> {
    calls
        .iter()
        .filter_map(|call| {
            let obj = call.as_object()?;
            let id = obj
                .get("id")
                .or_else(|| obj.get("call_id"))
                .and_then(Value::as_str)
                .unwrap_or("call_unknown");

            let (name, arguments) =
                if let Some(func) = obj.get("function").and_then(Value::as_object) {
                    let n = func
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let a = normalize_arguments(func.get("arguments"));
                    (n, a)
                } else if let Some(n) = obj.get("name").and_then(Value::as_str) {
                    let a = normalize_arguments(obj.get("arguments"));
                    (n, a)
                } else {
                    return None;
                };

            Some(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments,
                }
            }))
        })
        .collect()
}

#[allow(dead_code)] // used by tests
pub(crate) fn map_reasoning_effort(effort: &str) -> Option<i32> {
    match effort {
        "none" => Some(0),
        "low" => Some(1024),
        "medium" => Some(4096),
        "high" => Some(16384),
        "auto" => Some(-1),
        _ => None,
    }
}

pub(super) fn extract_chat_content_text(value: Option<&Value>) -> String {
    extract_text_from_content(value)
}

pub(super) fn responses_usage(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let total = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(input + output);
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": total
    })
}

/// Adapt an outbound Responses body to the target native endpoint's compat
/// declaration (design §5.1 Layer 0b egress review). Single source of truth for
/// upstream quirks; applied by every path that sends to a Responses native
/// endpoint (conversion, passthrough, probe).
pub(crate) fn apply_responses_egress_compat(
    body: &mut Value,
    compat: &crate::config::CompatConfig,
    store: Option<bool>,
) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    // Standard Responses accepts arrays; ChatGPT Codex backend requires a list.
    if let Some(input) = obj.get("input").cloned()
        && input.is_string()
    {
        obj.insert(
            "input".to_string(),
            json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": input}]}]),
        );
    }
    // store injection priority (research §3.1a):
    //   compat must_not_store > endpoint store > client passthrough > default false.
    if compat.effective_must_not_store() {
        obj.insert("store".to_string(), json!(false));
    } else if let Some(store_val) = store {
        obj.insert("store".to_string(), json!(store_val));
    } else if !obj.contains_key("store") {
        // Upstream omitting store means stored; proxy defaults to not storing.
        obj.insert("store".to_string(), json!(false));
    }
    if compat.effective_force_stream() {
        obj.insert("stream".to_string(), json!(true));
    }
    if compat.effective_strip_max_output_tokens() {
        obj.remove("max_output_tokens");
    }
}
