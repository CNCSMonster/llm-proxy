use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use super::anthropic_responses::{anthropic_to_responses_request, responses_to_anthropic_response};
use super::shared::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeminiStreamChunk {
    pub text: String,
    pub reasoning_text: String,
    pub function_calls: Vec<GeminiFunctionCall>,
    /// (functionCall name → thought_signature) pairs collected from streaming
    /// chunks, in arrival order. Keys are NOT computed here: streaming
    /// functionCall args arrive in fragments, so a key built from a partial
    /// args payload would never match the complete args replayed next turn.
    /// The consumer computes the final key once it has assembled the full args.
    pub signature_pairs: Vec<(String, String)>,
    /// thought_signature values carried by thinking (thought:true) parts, in
    /// arrival order. These bind to the thinking block itself (Anthropic
    /// thinking signature), distinct from functionCall-bound signatures.
    pub thought_signatures: Vec<String>,
    pub prompt_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiFunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Build a stable key binding a thought_signature to its functionCall.
///
/// Prefers an explicit id (Claude tool_use path). Gemini-native calls carry no
/// id, so fall back to name + canonical args JSON (serde_json serializes
/// objects with sorted keys, so the key is order-independent). A partial
/// streaming args payload yields a key that simply won't match on injection —
/// the call is then left unsigned rather than mis-signed.
pub fn function_call_signature_key(call: &Value, name: &str, arguments: &str) -> String {
    if let Some(id) = call
        .get("id")
        .or_else(|| call.get("call_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        return format!("id:{id}");
    }
    signature_key_from_name_args(name, arguments)
}

/// Key from function name + full args. Used where no call Value is available
/// (streaming path after args are assembled). Canonicalizes args via
/// serde_json so key order is stable across turns.
pub fn signature_key_from_name_args(name: &str, arguments: &str) -> String {
    let canonical = serde_json::from_str::<Value>(arguments)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| arguments.to_string());
    format!("fn:{name}:{canonical}")
}

/// Whether the upstream model needs functionCall/functionResponse ids
/// (Anthropic-family conversion) versus Gemini-native semantics.
///
/// Decided by the native endpoint's declared `anthropic_family_models`
/// (glob patterns against upstream_model, globset semantics) — configured
/// per endpoint, not guessed from model names. Verified L1 (2026-08-02):
/// antigravity serves both claude-* and gpt-oss-* through the Anthropic
/// Messages conversion.
pub fn antigravity_needs_tool_call_ids(
    upstream_model: &str,
    anthropic_family_models: &[String],
) -> bool {
    crate::config::anthropic_family_glob_set(anthropic_family_models)
        .is_some_and(|set| set.is_match(upstream_model))
}

pub fn antigravity_stream_chunk(value: &Value) -> GeminiStreamChunk {
    let gemini = value.get("response").unwrap_or(value);
    let mut chunk = GeminiStreamChunk::default();
    for candidate in gemini
        .get("candidates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(reason) = candidate
            .get("finishReason")
            .or_else(|| candidate.get("finish_reason"))
            .and_then(Value::as_str)
        {
            chunk.finish_reason = Some(reason.to_string());
        }
        for part in candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    chunk.reasoning_text.push_str(text);
                    // Thinking block's own signature (Anthropic thinking
                    // signature), distinct from functionCall-bound signatures.
                    if let Some(sig) = part
                        .get("thoughtSignature")
                        .or_else(|| part.get("thought_signature"))
                        .and_then(Value::as_str)
                        .filter(|sig| !sig.is_empty())
                    {
                        chunk.thought_signatures.push(sig.to_string());
                    }
                } else {
                    chunk.text.push_str(text);
                }
            }
            if let Some(call) = part
                .get("functionCall")
                .or_else(|| part.get("function_call"))
            {
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let arguments = call
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
                    .to_string();
                chunk.function_calls.push(GeminiFunctionCall {
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
                // Bind the signature to the functionCall it arrived with. The
                // key is deferred to the consumer: streaming args are partial
                // here, so keying now would never match the complete args
                // replayed in the next turn.
                if let Some(sig) = part
                    .get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .and_then(Value::as_str)
                    .filter(|sig| !sig.is_empty())
                {
                    chunk.signature_pairs.push((name.clone(), sig.to_string()));
                }
            }
        }
    }
    if let Some(usage) = gemini
        .get("usageMetadata")
        .or_else(|| gemini.get("cpaUsageMetadata"))
    {
        chunk.prompt_tokens = usage
            .get("promptTokenCount")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        chunk.output_tokens = usage
            .get("candidatesTokenCount")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        chunk.total_tokens = usage
            .get("totalTokenCount")
            .and_then(Value::as_i64)
            .unwrap_or(chunk.prompt_tokens + chunk.output_tokens);
    }
    chunk
}

pub fn responses_to_antigravity_request(
    req: Value,
    upstream_model: &str,
    project_id: &str,
    anthropic_family_models: &[String],
) -> Result<Value> {
    let mut gemini = responses_to_gemini_request(req, upstream_model, anthropic_family_models)?;
    // Remove model from inner request — it belongs only in the top-level envelope
    if let Some(obj) = gemini.as_object_mut() {
        obj.remove("model");
    }
    Ok(json!({
        "project": project_id,
        "request": gemini,
        "model": upstream_model
    }))
}

pub fn anthropic_to_antigravity_request(
    req: Value,
    upstream_model: &str,
    project_id: &str,
    anthropic_family_models: &[String],
) -> Result<Value> {
    // Extract Anthropic thinking config BEFORE the chain, because the
    // intermediate Responses format does not carry thinking parameters.
    let thinking_config = req
        .get("thinking")
        .and_then(Value::as_object)
        .and_then(|t| {
            if t.get("type").and_then(Value::as_str) != Some("enabled") {
                return None;
            }
            let budget = t.get("budget_tokens").and_then(Value::as_i64).unwrap_or(-1);
            let mut tc = serde_json::Map::new();
            tc.insert("thinkingBudget".to_string(), json!(budget));
            if budget < 0 {
                tc.insert("includeThoughts".to_string(), json!(true));
            }
            Some(Value::Object(tc))
        });

    // Collect thinking parts from assistant messages BEFORE the chain drops them.
    // These need to be injected into Gemini contents for multi-turn context.
    let thinking_parts_per_assistant: Vec<Vec<Value>> = req
        .get("messages")
        .and_then(Value::as_array)
        .map(|msgs| {
            msgs.iter()
                .map(|msg| {
                    let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
                    if role != "assistant" {
                        return Vec::new();
                    }
                    msg.get("content")
                        .and_then(Value::as_array)
                        .map(|blocks| {
                            blocks
                                .iter()
                                .filter_map(|block| {
                                    let obj = block.as_object()?;
                                    if obj.get("type").and_then(Value::as_str) != Some("thinking") {
                                        return None;
                                    }
                                    let thinking =
                                        obj.get("thinking").and_then(Value::as_str).unwrap_or("");
                                    if thinking.is_empty() {
                                        return None;
                                    }
                                    let mut part = serde_json::Map::new();
                                    part.insert("text".to_string(), json!(thinking));
                                    part.insert("thought".to_string(), json!(true));
                                    if let Some(sig) = obj.get("signature").and_then(Value::as_str)
                                    {
                                        part.insert("thoughtSignature".to_string(), json!(sig));
                                    }
                                    Some(Value::Object(part))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();

    let responses = anthropic_to_responses_request(req, upstream_model)?;
    let mut result = responses_to_antigravity_request(
        responses,
        upstream_model,
        project_id,
        anthropic_family_models,
    )?;

    if let Some(request) = result.get_mut("request").and_then(Value::as_object_mut) {
        // Inject thinking config into generationConfig.
        if let Some(tc) = thinking_config {
            let gc = request
                .entry("generationConfig")
                .or_insert_with(|| json!({}));
            if let Some(gc_obj) = gc.as_object_mut() {
                gc_obj.insert("thinkingConfig".to_string(), tc);
            }
        }

        // Inject thinking parts into assistant (model) contents.
        if let Some(contents) = request.get_mut("contents").and_then(Value::as_array_mut) {
            let mut assistant_idx = 0;
            for content in contents.iter_mut() {
                let role = content
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user");
                if role != "model" {
                    continue;
                }
                if assistant_idx < thinking_parts_per_assistant.len() {
                    let thinking = &thinking_parts_per_assistant[assistant_idx];
                    if !thinking.is_empty()
                        && let Some(parts) = content.get_mut("parts").and_then(Value::as_array_mut)
                    {
                        // Remove serialized thinking blocks that leaked through
                        // the chain (anthropic_to_responses_request serializes
                        // unknown block types as JSON text).
                        parts.retain(|p| {
                            let text = p.get("text").and_then(Value::as_str).unwrap_or("");
                            // A leaked thinking block serializes as JSON with
                            // "type":"thinking" — filter it out.
                            !(text.contains("\"type\":\"thinking\"")
                                || text.contains("\"type\": \"thinking\""))
                        });
                        // Prepend thinking parts before text parts.
                        let mut new_parts = thinking.clone();
                        new_parts.append(parts);
                        *parts = new_parts;
                    }
                }
                assistant_idx += 1;
            }
        }
    }

    Ok(result)
}

pub fn antigravity_to_responses_response(resp: Value, frontend_model: &str) -> Value {
    // Antigravity non-streaming responses may be a single-element array or
    // a multi-element array (Claude models return text split across elements).
    // Merge all text parts from all elements into a single response.
    let gemini = if let Some(arr) = resp.as_array() {
        if arr.len() == 1 {
            let first = &arr[0];
            first
                .get("response")
                .cloned()
                .unwrap_or_else(|| first.clone())
        } else {
            let mut merged_text = String::new();
            let mut merged_calls: Vec<Value> = Vec::new();
            let mut last_usage = Value::Null;
            let mut model_version = String::new();
            for elem in arr {
                let response = elem
                    .get("response")
                    .cloned()
                    .unwrap_or_else(|| elem.clone());
                if let Some(candidates) = response.get("candidates").and_then(Value::as_array) {
                    for candidate in candidates {
                        if let Some(parts) = candidate
                            .get("content")
                            .and_then(|c| c.get("parts"))
                            .and_then(Value::as_array)
                        {
                            for part in parts {
                                if let Some(text) = part.get("text").and_then(Value::as_str) {
                                    merged_text.push_str(text);
                                }
                                if let Some(call) = part
                                    .get("functionCall")
                                    .or_else(|| part.get("function_call"))
                                {
                                    // Keep the ORIGINAL Gemini functionCall
                                    // shape (name/args) so that
                                    // gemini_to_responses_response can parse it
                                    // later — embedding a Responses-shaped
                                    // call here yields empty args.
                                    merged_calls.push(call.clone());
                                }
                            }
                        }
                    }
                }
                if let Some(usage) = response.get("usageMetadata") {
                    last_usage = usage.clone();
                }
                if let Some(mv) = response.get("modelVersion").and_then(Value::as_str) {
                    model_version = mv.to_string();
                }
            }
            let mut merged = serde_json::Map::new();
            let mut merged_parts: Vec<Value> = vec![json!({"text": merged_text})];
            for call in merged_calls {
                let mut part = serde_json::Map::new();
                part.insert("functionCall".to_string(), call);
                merged_parts.push(Value::Object(part));
            }
            merged.insert(
                "candidates".to_string(),
                json!([{
                    "content": {
                        "role": "model",
                        "parts": merged_parts
                    },
                    "finishReason": "STOP"
                }]),
            );
            if !last_usage.is_null() {
                merged.insert("usageMetadata".to_string(), last_usage);
            }
            if !model_version.is_empty() {
                merged.insert("modelVersion".to_string(), json!(model_version));
            }
            Value::Object(merged)
        }
    } else {
        resp.get("response").cloned().unwrap_or(resp)
    };
    gemini_to_responses_response(gemini, frontend_model)
}

pub fn antigravity_to_anthropic_response(resp: Value, frontend_model: &str) -> Value {
    responses_to_anthropic_response(
        antigravity_to_responses_response(resp, frontend_model),
        frontend_model,
    )
}

pub(super) fn responses_to_gemini_request(
    mut req: Value,
    upstream_model: &str,
    anthropic_family_models: &[String],
) -> Result<Value> {
    let obj = req
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Responses request must be a JSON object"))?;
    let mut out = Map::new();
    let mut contents = Vec::new();
    if let Some(instructions) = obj.get("instructions").and_then(Value::as_str)
        && !instructions.is_empty()
    {
        out.insert(
            "systemInstruction".to_string(),
            json!({"parts": [{"text": instructions}]}),
        );
    }
    match obj.get("input") {
        Some(Value::String(text)) => {
            contents.push(json!({"role": "user", "parts": [{"text": text}]}))
        }
        Some(Value::Array(items)) => append_responses_input_as_gemini_contents(
            items,
            &mut contents,
            upstream_model,
            anthropic_family_models,
        ),
        Some(other) => bail!("unsupported Responses input shape: {other}"),
        None => {}
    }
    out.insert("contents".to_string(), Value::Array(contents));
    if let Some(Value::Array(tools)) = obj.get("tools") {
        let declarations: Vec<Value> = tools
            .iter()
            .filter_map(responses_tool_to_gemini_declaration)
            .collect();
        if !declarations.is_empty() {
            out.insert(
                "tools".to_string(),
                json!([{ "functionDeclarations": declarations }]),
            );
        }
    }
    let mut generation_config = Map::new();
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("max_output_tokens", "maxOutputTokens"),
    ] {
        if let Some(value) = obj.get(from) {
            generation_config.insert(to.to_string(), value.clone());
        }
    }
    if let Some(reasoning) = obj.get("reasoning").and_then(Value::as_object)
        && let Some(effort) = reasoning.get("effort").and_then(Value::as_str)
    {
        // Map reasoning effort to thinkingBudget (integer), matching Go version.
        // Gemini API rejects raw strings like "none" for thinkingLevel.
        let budget = match effort {
            "none" => 0,
            "low" => 1024,
            "medium" => 4096,
            "high" => 16384,
            _ => -1, // "auto" or unknown → automatic
        };
        // budget=0 (effort=none): omit thinkingConfig entirely — Gemini models
        // that use level format reject thinkingBudget:0 as invalid.
        if budget != 0 {
            let mut tc = serde_json::Map::new();
            tc.insert("thinkingBudget".to_string(), json!(budget));
            if budget < 0 {
                tc.insert("includeThoughts".to_string(), json!(true));
            }
            generation_config.insert("thinkingConfig".to_string(), Value::Object(tc));
        }
    }
    if !generation_config.is_empty() {
        out.insert(
            "generationConfig".to_string(),
            Value::Object(generation_config),
        );
    }
    out.insert("model".to_string(), json!(upstream_model));
    Ok(Value::Object(out))
}

fn append_responses_input_as_gemini_contents(
    items: &[Value],
    contents: &mut Vec<Value>,
    upstream_model: &str,
    anthropic_family_models: &[String],
) {
    // Anthropic-family upstreams (antigravity converts Gemini → Anthropic
    // Messages for claude AND gpt-oss models) require functionCall /
    // functionResponse ids to map to tool_use.id / tool_result.tool_use_id.
    // Gemini-native upstreams reject ids (or tolerate them) but always require
    // a non-empty function_response.name — use the real function name.
    //
    // The family comes from the native endpoint's declared
    // anthropic_family_models (catalog/config), not from guessing the model name.
    let include_call_ids = antigravity_needs_tool_call_ids(upstream_model, anthropic_family_models);

    // Build call_id → function name mapping from function_call items, so
    // function_call_output can carry the real name instead of falling back to call_id.
    let mut call_name_map: std::collections::HashMap<&str, &str> = Default::default();
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("function_call")
            && let (Some(cid), Some(name)) = (
                item.get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str),
                item.get("name").and_then(Value::as_str),
            )
            && !cid.is_empty()
            && !name.is_empty()
        {
            call_name_map.insert(cid, name);
        }
    }

    for item in items {
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        match item_type {
            "message" => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let parts = responses_content_to_gemini_parts(item.get("content"));
                if !parts.is_empty() {
                    // Multi-turn tool-call histories yield adjacent same-role
                    // contents (assistant text + functionCall, functionResponse +
                    // next user message). Antigravity's Claude conversion requires
                    // strictly alternating user/model roles, so merge same-role
                    // neighbours into one content with combined parts.
                    push_or_merge_gemini_content(
                        contents,
                        if role == "assistant" { "model" } else { "user" },
                        parts,
                    );
                }
            }
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut call = serde_json::Map::new();
                if include_call_ids && !call_id.is_empty() {
                    call.insert("id".to_string(), json!(call_id));
                }
                call.insert(
                    "name".to_string(),
                    json!(
                        item.get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    ),
                );
                call.insert(
                    "args".to_string(),
                    item.get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| json!({})),
                );
                push_or_merge_gemini_content(
                    contents,
                    "model",
                    vec![json!({"functionCall": call})],
                );
            }
            "function_call_output" => {
                // Gemini's function_response.name must be non-empty and should be the
                // function name; fall back to call_id (then to "unknown").
                let call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                let name = if !name.is_empty() {
                    name
                } else if let Some(mapped) = call_name_map.get(call_id) {
                    mapped
                } else if !call_id.is_empty() {
                    call_id
                } else {
                    "unknown"
                };
                let mut fr = serde_json::Map::new();
                if include_call_ids && !call_id.is_empty() {
                    fr.insert("id".to_string(), json!(call_id));
                }
                fr.insert("name".to_string(), json!(name));
                fr.insert(
                    "response".to_string(),
                    json!({"output": item.get("output").and_then(Value::as_str).unwrap_or("")}),
                );
                push_or_merge_gemini_content(
                    contents,
                    "user",
                    vec![json!({"functionResponse": fr})],
                );
            }
            _ => {}
        }
    }
}

/// Push a Gemini content, merging into the previous one when the roles match.
///
/// Adjacent same-role contents appear in multi-turn tool-call histories:
/// `model(assistant text)` then `model(functionCall)`, or `user(functionResponse)`
/// then `user(next question)`. Antigravity's Claude conversion demands strictly
/// alternating roles, so same-role neighbours must be combined into one content
/// with concatenated parts. Gemini-native upstreams tolerate the merge too.
fn push_or_merge_gemini_content(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut()
        && last.get("role").and_then(Value::as_str).unwrap_or("") == role
        && let Some(last_parts) = last.get_mut("parts").and_then(Value::as_array_mut)
    {
        last_parts.extend(parts);
        return;
    }
    contents.push(json!({"role": role, "parts": parts}));
}

fn responses_content_to_gemini_parts(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => vec![json!({"text": text})],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(responses_part_to_gemini_part)
            .collect(),
        Some(value) => vec![json!({"text": value.to_string()})],
        None => Vec::new(),
    }
}

fn responses_part_to_gemini_part(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str).unwrap_or("") {
        "input_text" | "output_text" | "text" => {
            // Skip empty text parts — antigravity's Claude endpoint rejects
            // {"text": ""} when converting to Anthropic messages (text.text: Field required).
            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                return None;
            }
            Some(json!({"text": text}))
        }
        "input_image" => {
            let url = part
                .get("image_url")
                .and_then(Value::as_str)
                .or_else(|| {
                    part.get("image_url")
                        .and_then(|v| v.get("url"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("");
            data_url_to_gemini_inline_data(url).or_else(|| Some(json!({"text": "[image omitted]"})))
        }
        _ => Some(json!({"text": part.to_string()})),
    }
}

fn data_url_to_gemini_inline_data(url: &str) -> Option<Value> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some(json!({"inlineData": {"mimeType": media_type, "data": data}}))
}

fn responses_tool_to_gemini_declaration(tool: &Value) -> Option<Value> {
    let obj = tool.as_object()?;
    if obj.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    Some(json!({
        "name": obj.get("name").cloned().or_else(|| obj.get("function").and_then(|f| f.get("name")).cloned())?,
        "description": obj.get("description").cloned().or_else(|| obj.get("function").and_then(|f| f.get("description")).cloned()).unwrap_or_else(|| json!("")),
        "parameters": obj.get("parameters").cloned().or_else(|| obj.get("function").and_then(|f| f.get("parameters")).cloned()).unwrap_or_else(|| json!({"type":"object","properties":{}}))
    }))
}

fn gemini_to_responses_response(gemini: Value, frontend_model: &str) -> Value {
    let mut output = Vec::new();
    for candidate in gemini
        .get("candidates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut text = String::new();
        let mut reasoning_text = String::new();
        let mut reasoning_signature = String::new();
        for part in candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    reasoning_text.push_str(t);
                    // Carry the thinking block's own signature through to the
                    // Responses reasoning item so the Anthropic conversion can
                    // emit it as thinking.signature (multi-turn parity).
                    if let Some(sig) = part
                        .get("thoughtSignature")
                        .or_else(|| part.get("thought_signature"))
                        .and_then(Value::as_str)
                        .filter(|sig| !sig.is_empty())
                    {
                        reasoning_signature = sig.to_string();
                    }
                } else {
                    text.push_str(t);
                }
            }
            if let Some(call) = part
                .get("functionCall")
                .or_else(|| part.get("function_call"))
            {
                let fc_id = format!("fc_{}", uuid::Uuid::new_v4().as_simple());
                output.push(json!({
                    "id": fc_id,
                    "call_id": fc_id,
                    "type": "function_call",
                    "name": call.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                    "arguments": call.get("args").cloned().unwrap_or_else(|| json!({})).to_string(),
                    "status": "completed"
                }));
            }
        }
        if !reasoning_text.is_empty() {
            let mut reasoning = serde_json::Map::new();
            reasoning.insert("id".to_string(), json!("rs_llm_proxy"));
            reasoning.insert("type".to_string(), json!("reasoning"));
            reasoning.insert(
                "summary".to_string(),
                json!([{"type": "summary_text", "text": reasoning_text}]),
            );
            if !reasoning_signature.is_empty() {
                reasoning.insert("signature".to_string(), json!(reasoning_signature));
            }
            output.push(Value::Object(reasoning));
        }
        if !text.is_empty() {
            output.push(responses_output_message(&text));
        }
    }
    json!({
        "id": "resp_llm_proxy",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": frontend_model,
        "output": output,
        "usage": responses_usage_from_gemini(gemini.get("usageMetadata").or_else(|| gemini.get("cpaUsageMetadata")))
    })
}

fn responses_usage_from_gemini(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|u| u.get("promptTokenCount"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json!({"input_tokens": input, "output_tokens": output, "total_tokens": usage.and_then(|u| u.get("totalTokenCount")).and_then(Value::as_i64).unwrap_or(input + output)})
}
