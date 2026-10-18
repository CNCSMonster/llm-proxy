use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
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

/// Chat `max_completion_tokens` is the current name of `max_tokens`; when both
/// are present the newer canonical name wins (registered in spec.md §6).
pub(super) fn chat_max_tokens(obj: &Map<String, Value>) -> Option<&Value> {
    [obj.get("max_completion_tokens"), obj.get("max_tokens")]
        .into_iter()
        .flatten()
        .find(|value| !value.is_null())
}

/// Proxy-generated response IDs must carry the target protocol's prefix and be
/// unique per response (spec.md §6 "Response ID and timestamp namespace
/// sub-contract"); `prefix` includes its separator (`chatcmpl-`, `resp_`, …).
pub(crate) fn new_scoped_id(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().as_simple())
}

/// Unix seconds at the moment of response synthesis; `created`/`created_at`
/// must never be a fabricated `0` (spec.md §6 rule 3).
pub(crate) fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// A placeholder event recorded when opt-in fills for missing history.
#[derive(Debug, Clone)]
pub struct PlaceholderEvent {
    /// "missing_function_call_output" or "orphan_reasoning_discarded"
    pub reason: &'static str,
    /// The call_id for missing function_call_output; None for orphan reasoning.
    pub call_id: Option<String>,
}

pub(super) fn append_responses_items_as_chat_messages(
    items: &[Value],
    messages: &mut Vec<Value>,
    placeholder_enabled: bool,
    requires_reasoning: bool,
) -> Result<Vec<PlaceholderEvent>> {
    let mut pending_tool_calls: Vec<Value> = Vec::new();
    let mut pending_reasoning = String::new();
    let mut seen_call_ids = HashSet::new();
    let mut outstanding_call_ids: HashSet<&str> = HashSet::new();
    let mut events: Vec<PlaceholderEvent> = Vec::new();

    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("");

        match item_type {
            "reasoning" => {
                let text = responses_history_reasoning(obj)?;
                if text.is_empty() {
                    continue;
                }
                // A new reasoning segment ends the preceding consecutive call group.
                // Flush first so reasoning for that group cannot leak into the next one.
                if flush_pending_tool_calls(
                    messages,
                    &mut pending_tool_calls,
                    &mut pending_reasoning,
                    requires_reasoning,
                    placeholder_enabled,
                )? {
                    events.push(PlaceholderEvent {
                        reason: "missing_reasoning_content",
                        call_id: None,
                    });
                }
                pending_reasoning.push_str(&text);
            }
            "function_call" | "shell_call" | "local_shell_call" => {
                if pending_tool_calls.is_empty() && !outstanding_call_ids.is_empty() {
                    if placeholder_enabled {
                        // Generate placeholder outputs for outstanding call IDs
                        for call_id in outstanding_call_ids.iter() {
                            let placeholder_content = format!(
                                "[placeholder: function_call_output missing for call_id={call_id}]"
                            );
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": call_id,
                                "content": placeholder_content
                            }));
                            events.push(PlaceholderEvent {
                                reason: "missing_function_call_output",
                                call_id: Some(call_id.to_string()),
                            });
                            tracing::warn!(
                                                           event = "placeholder_used",
                                                           reason = "missing_function_call_output",
                                                           call_id = %call_id,
                            ": placeholder generated for missing function_call_output"
                                                       );
                        }
                        outstanding_call_ids.clear();
                    } else {
                        bail!(
                            "Responses history requires all tool outputs before a new call group"
                        );
                    }
                }
                // Keep the existing id-only compatibility, but never hide an invalid
                // explicit call_id behind the independent Responses item id.
                let id_field = if obj.contains_key("call_id") {
                    "call_id"
                } else {
                    "id"
                };
                let call_id = responses_history_identity(obj, id_field)?;
                if !seen_call_ids.insert(call_id) {
                    bail!("Responses history contains a duplicate call ID");
                }
                outstanding_call_ids.insert(call_id);
                let (name, arguments) = if item_type == "function_call" {
                    let name = responses_history_identity(obj, "name")?;
                    let arguments =
                        obj.get("arguments")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Responses function_call.arguments must be a string"
                                )
                            })?;
                    (name, arguments.to_owned())
                } else {
                    let action = obj
                        .get("action")
                        .filter(|value| value.is_object())
                        .ok_or_else(|| {
                            anyhow::anyhow!("Responses shell call.action must be an object")
                        })?;
                    ("shell", action.to_string())
                };
                pending_tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments
                    }
                }));
            }
            "function_call_output" | "shell_call_output" | "local_shell_call_output" => {
                // Orphan reasoning = reasoning with no pending tool calls to attach to.
                // If pending_tool_calls is non-empty, the reasoning will be attached to
                // the assistant message (legitimate). If empty, reasoning is orphan.
                if !pending_reasoning.is_empty() && pending_tool_calls.is_empty() {
                    if placeholder_enabled {
                        // Discard orphan reasoning, record warning
                        events.push(PlaceholderEvent {
                            reason: "orphan_reasoning_discarded",
                            call_id: None,
                        });
                        tracing::warn!(
                            event = "placeholder_used",
                            reason = "orphan_reasoning_discarded",
                            ": orphan reasoning discarded before tool output"
                        );
                        pending_reasoning.clear();
                    } else {
                        bail!("Responses history has orphan reasoning before a tool output");
                    }
                }
                if flush_pending_tool_calls(
                    messages,
                    &mut pending_tool_calls,
                    &mut pending_reasoning,
                    requires_reasoning,
                    placeholder_enabled,
                )? {
                    events.push(PlaceholderEvent {
                        reason: "missing_reasoning_content",
                        call_id: None,
                    });
                }
                let call_id = responses_history_identity(obj, "call_id")?;
                if !outstanding_call_ids.remove(call_id) {
                    bail!("Responses history tool output has no matching outstanding call ID");
                }
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
                let content = responses_content_to_chat(obj.get("content"))?;
                if role == "assistant"
                    && (is_empty_content(&content)
                        || content.as_array().is_some_and(|parts| {
                            parts
                                .iter()
                                .all(|part| part["type"] == "text" && part["text"] == "")
                        }))
                {
                    continue;
                }
                if flush_pending_tool_calls(
                    messages,
                    &mut pending_tool_calls,
                    &mut pending_reasoning,
                    requires_reasoning,
                    placeholder_enabled,
                )? {
                    events.push(PlaceholderEvent {
                        reason: "missing_reasoning_content",
                        call_id: None,
                    });
                }
                let mut message = json!({ "role": role, "content": content });
                if role == "assistant" {
                    if !outstanding_call_ids.is_empty() {
                        if placeholder_enabled {
                            // Generate placeholder outputs for outstanding call IDs
                            for call_id in outstanding_call_ids.iter() {
                                let placeholder_content = format!(
                                    "[placeholder: function_call_output missing for call_id={call_id}]"
                                );
                                messages.push(json!({
                                    "role": "tool",
                                    "tool_call_id": call_id,
                                    "content": placeholder_content
                                }));
                                events.push(PlaceholderEvent {
                                    reason: "missing_function_call_output",
                                    call_id: Some(call_id.to_string()),
                                });
                                tracing::warn!(
                                                                   event = "placeholder_used",
                                                                   reason = "missing_function_call_output",
                                                                   call_id = %call_id,
                                ": placeholder generated for missing function_call_output"
                                                               );
                            }
                            outstanding_call_ids.clear();
                        } else {
                            bail!(
                                "Responses history requires all tool outputs before an assistant message"
                            );
                        }
                    }
                    let has_text = content.as_str().is_some_and(|text| !text.is_empty())
                        || content.as_array().is_some_and(|parts| {
                            parts.iter().any(|part| {
                                part["type"] == "text"
                                    && part["text"].as_str().is_some_and(|text| !text.is_empty())
                            })
                        });
                    if has_text
                        && attach_pending_reasoning(
                            &mut message,
                            &mut pending_reasoning,
                            requires_reasoning,
                            placeholder_enabled,
                        )?
                    {
                        events.push(PlaceholderEvent {
                            reason: "missing_reasoning_content",
                            call_id: None,
                        });
                    }
                } else if !pending_reasoning.is_empty() {
                    if placeholder_enabled {
                        // Discard orphan reasoning, record warning
                        events.push(PlaceholderEvent {
                            reason: "orphan_reasoning_discarded",
                            call_id: None,
                        });
                        tracing::warn!(
                            event = "placeholder_used",
                            reason = "orphan_reasoning_discarded",
                            ": orphan reasoning discarded before non-assistant message"
                        );
                        pending_reasoning.clear();
                    } else {
                        bail!(
                            "Responses history has orphan reasoning before a non-assistant message"
                        );
                    }
                }
                messages.push(message);
            }
        }
    }

    if !outstanding_call_ids.is_empty() {
        if placeholder_enabled {
            // Flush any pending tool calls first (they haven't been flushed yet
            // since there were no outputs to trigger the flush)
            if flush_pending_tool_calls(
                messages,
                &mut pending_tool_calls,
                &mut pending_reasoning,
                requires_reasoning,
                placeholder_enabled,
            )? {
                events.push(PlaceholderEvent {
                    reason: "missing_reasoning_content",
                    call_id: None,
                });
            }
            // Generate placeholder outputs for outstanding call IDs at end of history
            for call_id in outstanding_call_ids.iter() {
                let placeholder_content =
                    format!("[placeholder: function_call_output missing for call_id={call_id}]");
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": placeholder_content
                }));
                events.push(PlaceholderEvent {
                    reason: "missing_function_call_output",
                    call_id: Some(call_id.to_string()),
                });
                tracing::warn!(
                                   event = "placeholder_used",
                                   reason = "missing_function_call_output",
                                   call_id = %call_id,
                ": placeholder generated for missing function_call_output at end of history"
                               );
            }
            outstanding_call_ids.clear();
        } else {
            bail!("Responses history is missing tool outputs");
        }
    }
    if !pending_reasoning.is_empty() {
        if placeholder_enabled {
            // Discard orphan reasoning at end of history
            events.push(PlaceholderEvent {
                reason: "orphan_reasoning_discarded",
                call_id: None,
            });
            tracing::warn!(
                event = "placeholder_used",
                reason = "orphan_reasoning_discarded",
                ": orphan reasoning discarded at end of history"
            );
            pending_reasoning.clear();
        } else {
            bail!("Responses history ends with orphan reasoning");
        }
    }
    Ok(events)
}

fn responses_history_identity<'a>(obj: &'a Map<String, Value>, field: &str) -> Result<&'a str> {
    obj.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Responses history {field} must be a non-empty string"))
}

fn responses_history_reasoning(obj: &Map<String, Value>) -> Result<String> {
    match obj.get("summary") {
        None | Some(Value::Null) => {}
        Some(Value::Array(parts)) if parts.is_empty() => {}
        _ => bail!("Responses reasoning summary must be empty for Chat conversion"),
    }
    match obj.get("encrypted_content") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) if text.is_empty() => {}
        _ => bail!("Responses reasoning encrypted_content must be empty for Chat conversion"),
    }
    let parts = match obj.get("content") {
        None | Some(Value::Null) => return Ok(String::new()),
        Some(Value::Array(parts)) => parts,
        _ => bail!("Responses reasoning content must be an array"),
    };
    let mut text = String::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("reasoning_text") {
            bail!("Responses reasoning content must contain reasoning_text parts");
        }
        let part_text = part
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Responses reasoning_text.text must be a string"))?;
        text.push_str(part_text);
    }
    Ok(text)
}

/// Attach pending reasoning to an assistant message, applying the
/// behavior matrix based on `requires_reasoning` and `placeholder_enabled`.
///
/// Returns `Ok(true)` when a placeholder was injected (for event recording).
///
/// - Non-empty pending → always attach (existing behavior). Returns `Ok(false)`.
/// - Empty pending + `!requires_reasoning` → omit field (existing behavior). Returns `Ok(false)`.
/// - Empty pending + `requires_reasoning` + `placeholder_enabled` → inject placeholder. Returns `Ok(true)`.
/// - Empty pending + `requires_reasoning` + `!placeholder_enabled` → bail with readable error.
fn attach_pending_reasoning(
    message: &mut Value,
    pending_reasoning: &mut String,
    requires_reasoning: bool,
    placeholder_enabled: bool,
) -> Result<bool> {
    if !pending_reasoning.is_empty() {
        message["reasoning_content"] = json!(std::mem::take(pending_reasoning));
        return Ok(false);
    }
    // pending_reasoning is empty
    if !requires_reasoning {
        // Upstream doesn't check — omit field, no error.
        return Ok(false);
    }
    // requires_reasoning is true
    if placeholder_enabled {
        // inject self-describing placeholder
        message["reasoning_content"] =
            json!("[placeholder: reasoning_content missing for assistant with tool_calls]");
        tracing::warn!(
            event = "placeholder_used",
            reason = "missing_reasoning_content",
            ": placeholder injected for missing reasoning_content on assistant with tool_calls"
        );
        return Ok(true);
    }
    // requires_reasoning=true, placeholder_enabled=false → local 400
    bail!(
        "reasoning_content required but missing in assistant history (provider requires reasoning_content on assistant messages)"
    )
}

fn flush_pending_tool_calls(
    messages: &mut Vec<Value>,
    pending_tool_calls: &mut Vec<Value>,
    pending_reasoning: &mut String,
    requires_reasoning: bool,
    placeholder_enabled: bool,
) -> Result<bool> {
    if pending_tool_calls.is_empty() {
        return Ok(false);
    }
    let mut message = json!({
        "role": "assistant",
        "tool_calls": std::mem::take(pending_tool_calls)
    });
    let placeholder_injected = attach_pending_reasoning(
        &mut message,
        pending_reasoning,
        requires_reasoning,
        placeholder_enabled,
    )?;
    messages.push(message);
    Ok(placeholder_injected)
}

pub(super) fn responses_content_to_chat(content: Option<&Value>) -> Result<Value> {
    match content {
        Some(Value::String(text)) => Ok(json!(text)),
        Some(Value::Array(parts)) => {
            let converted: Vec<Value> = parts
                .iter()
                .filter_map(|part| match responses_part_to_chat_content(part) {
                    Ok(Some(part)) => Some(Ok(part)),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                })
                .collect::<Result<_>>()?;
            if converted.len() == 1
                && converted[0].get("type").and_then(Value::as_str) == Some("text")
            {
                Ok(converted[0]
                    .get("text")
                    .cloned()
                    .unwrap_or_else(|| json!("")))
            } else if converted.is_empty() {
                Ok(json!(""))
            } else {
                Ok(Value::Array(converted))
            }
        }
        _ => Ok(json!("")),
    }
}

fn responses_part_to_chat_content(part: &Value) -> Result<Option<Value>> {
    let obj = part
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Responses content part must be an object"))?;
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        "input_text" | "output_text" => Ok(Some(json!({
            "type": "text",
            "text": obj.get("text").and_then(Value::as_str).unwrap_or("")
        }))),
        "input_image" => match responses_image_url(obj)? {
            Some(url) => Ok(Some(
                json!({ "type": "image_url", "image_url": { "url": url } }),
            )),
            None => Ok(None),
        },
        "input_file" | "document" => Ok(None),
        other => bail!("unsupported Responses content part type: {other}"),
    }
}

fn responses_image_url(obj: &Map<String, Value>) -> Result<Option<&str>> {
    let Some(image_url) = obj.get("image_url") else {
        if obj.get("file_id").is_some() {
            return Ok(None);
        }
        bail!("Responses input_image must contain image_url or file_id")
    };
    match image_url {
        Value::String(url) if !url.is_empty() => Ok(Some(url)),
        Value::Object(image) => image
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(Some)
            .ok_or_else(|| {
                anyhow::anyhow!("Responses input_image.image_url.url must be a non-empty string")
            }),
        _ => bail!("Responses input_image.image_url must be a non-empty string or object with url"),
    }
}

pub(super) fn responses_items_to_anthropic_messages(items: &[Value]) -> Result<Vec<Value>> {
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
                    "content": responses_content_to_anthropic(obj.get("content"))?
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
    Ok(messages)
}

pub(super) fn responses_content_to_anthropic(content: Option<&Value>) -> Result<Value> {
    match content {
        Some(Value::String(text)) => Ok(json!([{ "type": "text", "text": text }])),
        Some(Value::Array(parts)) => Ok(Value::Array(
            parts
                .iter()
                .filter_map(|part| match responses_part_to_anthropic_content(part) {
                    Ok(Some(part)) => Some(Ok(part)),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                })
                .collect::<Result<Vec<_>>>()?,
        )),
        _ => Ok(json!([{ "type": "text", "text": "" }])),
    }
}

fn responses_part_to_anthropic_content(part: &Value) -> Result<Option<Value>> {
    let obj = part
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Responses content part must be an object"))?;
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        "input_text" | "output_text" => Ok(Some(json!({
            "type": "text",
            "text": obj.get("text").and_then(Value::as_str).unwrap_or("")
        }))),
        "input_image" => match responses_image_url(obj)? {
            Some(url) => Ok(Some(json!({
                "type": "image",
                "source": image_url_to_anthropic_source(url)
            }))),
            None => Ok(None),
        },
        "input_file" | "document" => responses_file_to_anthropic_document(obj).map(Some),
        other => bail!("unsupported Responses content part type: {other}"),
    }
}

pub(super) fn responses_output_message(text: &str) -> Value {
    json!({
        "id": new_scoped_id("msg_"),
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

pub(super) fn responses_file_to_anthropic_document(obj: &Map<String, Value>) -> Result<Value> {
    let title = obj
        .get("filename")
        .or_else(|| obj.get("name"))
        .cloned()
        .unwrap_or_else(|| json!("document"));
    if let Some(file_data) = obj
        .get("file_data")
        .and_then(Value::as_str)
        .filter(|file_data| !file_data.is_empty())
    {
        return Ok(json!({
            "type": "document",
            "source": file_data_to_anthropic_source(file_data)?,
            "title": title
        }));
    }
    if let Some(file_url) = obj
        .get("file_url")
        .and_then(Value::as_str)
        .filter(|file_url| !file_url.is_empty())
    {
        return Ok(json!({
            "type": "document",
            "source": { "type": "url", "url": file_url },
            "title": title
        }));
    }
    bail!("Responses input_file must contain a non-empty file_data or file_url")
}

pub(super) fn file_data_to_anthropic_source(file_data: &str) -> Result<Value> {
    if let Some(rest) = file_data.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').ok_or_else(|| {
            anyhow::anyhow!("Responses input_file.file_data must be a complete data URL")
        })?;
        let media_type = meta
            .split(';')
            .next()
            .filter(|media| !media.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("Responses input_file.file_data must include a MIME type")
            })?;
        if data.is_empty() {
            bail!("Responses input_file.file_data must have a non-empty payload")
        }
        return Ok(json!({ "type": "base64", "media_type": media_type, "data": data }));
    }
    Ok(json!({ "type": "base64", "media_type": "application/pdf", "data": file_data }))
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

pub(super) fn strict_anthropic_image_source_to_openai_url(
    source: Option<&Map<String, Value>>,
) -> Result<String> {
    let source =
        source.ok_or_else(|| anyhow::anyhow!("Anthropic image source must be an object"))?;
    match source.get("type").and_then(Value::as_str) {
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                anyhow::anyhow!("Anthropic image source.url must be a non-empty string")
            }),
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .filter(|media_type| !media_type.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("Anthropic image source.media_type must be a non-empty string")
                })?;
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .filter(|data| !data.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("Anthropic image source.data must be a non-empty string")
                })?;
            Ok(format!("data:{media_type};base64,{data}"))
        }
        Some(other) => bail!("unsupported Anthropic image source type: {other}"),
        None => bail!("Anthropic image source.type must be a string"),
    }
}

pub(super) fn validate_anthropic_message_images(messages: Option<&Value>) -> Result<()> {
    for message in messages.and_then(Value::as_array).into_iter().flatten() {
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("image") {
                strict_anthropic_image_source_to_openai_url(
                    part.get("source").and_then(Value::as_object),
                )?;
            }
        }
    }
    Ok(())
}

pub(super) fn validate_anthropic_message_documents(messages: Option<&Value>) -> Result<()> {
    for message in messages.and_then(Value::as_array).into_iter().flatten() {
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("document") {
                strict_anthropic_document_source(part.get("source").and_then(Value::as_object))?;
            }
        }
    }
    Ok(())
}

pub(super) fn validate_anthropic_message_documents_for_chat(
    messages: Option<&Value>,
) -> Result<()> {
    for message in messages.and_then(Value::as_array).into_iter().flatten() {
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        if parts
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) == Some("document"))
        {
            bail!("Anthropic document content is unsupported for Chat Completions")
        }
    }
    Ok(())
}

fn strict_anthropic_document_source(source: Option<&Map<String, Value>>) -> Result<()> {
    let source =
        source.ok_or_else(|| anyhow::anyhow!("Anthropic document source must be an object"))?;
    match source.get("type").and_then(Value::as_str) {
        Some("url") => {
            source
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("Anthropic document source.url must be a non-empty string")
                })?;
        }
        Some("base64") => {
            source
                .get("media_type")
                .and_then(Value::as_str)
                .filter(|media_type| !media_type.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Anthropic document source.media_type must be a non-empty string"
                    )
                })?;
            source
                .get("data")
                .and_then(Value::as_str)
                .filter(|data| !data.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("Anthropic document source.data must be a non-empty string")
                })?;
        }
        Some(other) => bail!("unsupported Anthropic document source type: {other}"),
        None => bail!("Anthropic document source.type must be a string"),
    }
    Ok(())
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

pub(super) fn chat_message_content_to_anthropic(msg: &Map<String, Value>) -> Result<Value> {
    let mut content = match msg.get("content") {
        Some(Value::String(text)) => json!([{ "type": "text", "text": text }]),
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .map(chat_part_to_anthropic_content)
                .collect::<Result<Vec<_>>>()?,
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
    Ok(content)
}

fn chat_part_to_anthropic_content(part: &Value) -> Result<Value> {
    let obj = part
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Chat content part must be an object"))?;
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => Ok(json!({
            "type": "text",
            "text": obj.get("text").and_then(Value::as_str).unwrap_or("")
        })),
        "image_url" => {
            let url = obj
                .get("image_url")
                .and_then(Value::as_object)
                .and_then(|image| image.get("url"))
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Chat image_url.url must be a non-empty string"))?;
            Ok(json!({
                "type": "image",
                "source": image_url_to_anthropic_source(url)
            }))
        }
        other => bail!("unsupported Chat content part type: {other}"),
    }
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
                } else {
                    let n = obj.get("name").and_then(Value::as_str)?;
                    let a = normalize_arguments(obj.get("arguments"));
                    (n, a)
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

/// Adapt an outbound Responses body to the target native endpoint's compat
/// declaration. Single source of truth for
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
    // store injection priority:
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
