//! Proxy submodule extracted from `proxy::mod`.

use super::*;
use crate::config::ModelConfig;
use anyhow::bail;

pub(super) fn resolve_request_candidates(
    state: &AppState,
    protocol: Protocol,
    body: &Value,
    err: fn(StatusCode, &str) -> Response,
) -> Result<Vec<(String, ExecutionPlan)>, Box<Response>> {
    let Some(model_id) = body.get("model").and_then(Value::as_str) else {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            "missing required \"model\" field",
        )));
    };
    let Some(model) = state.cfg.models.get(model_id) else {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &format!(
                "unknown model {model_id:?}; llm-proxy does not substitute a default model — fix the client model ID or declare the model in config"
            ),
        )));
    };
    if !model.exposes_protocol(protocol) {
        let supported = model
            .supported_protocols()
            .iter()
            .map(|p| p.route_key())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &format!(
                "model {model_id:?} does not support {} requests; supported protocols: {supported}",
                protocol.route_key()
            ),
        )));
    }
    if let Some(missing) = missing_capability(model, protocol, body) {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &format!(
                "model {model_id:?} does not declare the {missing} capability required by this request"
            ),
        )));
    }
    if let Some(level) = requested_reasoning_level(protocol, body)
        && !model.supported_reasoning_levels.is_empty()
        && !model
            .supported_reasoning_levels
            .iter()
            .any(|supported| supported == level)
    {
        return Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            &format!(
                "model {model_id:?} does not support reasoning level {level:?}; supported levels: {}",
                model.supported_reasoning_levels.join(", ")
            ),
        )));
    }
    Ok(state
        .cfg
        .resolve_model_request_candidates(protocol, model_id))
}

pub(super) fn missing_capability(
    model: &ModelConfig,
    protocol: Protocol,
    body: &Value,
) -> Option<&'static str> {
    let declares = |feature: &str| model.features.iter().any(|f| f == feature);
    if request_has_image(protocol, body) && !declares("image_input") {
        return Some("image_input");
    }
    if request_has_document(protocol, body) && !declares("document_input") {
        return Some("document_input");
    }
    None
}

pub(super) fn requested_reasoning_level(protocol: Protocol, body: &Value) -> Option<&str> {
    match protocol {
        Protocol::OpenaiChatCompletions => body.get("reasoning_effort").and_then(Value::as_str),
        Protocol::OpenaiResponses => body
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str),
        Protocol::Anthropic => body
            .get("thinking")
            .and_then(|thinking| thinking.get("effort").or_else(|| thinking.get("level")))
            .and_then(Value::as_str),
        Protocol::Antigravity => None,
    }
}

pub(super) fn body_with_default_reasoning(
    state: &AppState,
    protocol: Protocol,
    mut body: Value,
) -> Value {
    if requested_reasoning_level(protocol, &body).is_some() {
        return body;
    }
    let Some(model_id) = body.get("model").and_then(Value::as_str) else {
        return body;
    };
    if state
        .cfg
        .models
        .get(model_id)
        .and_then(|model| model.enable_thinking)
        == Some(false)
    {
        return body_without_reasoning(protocol, body);
    }
    let Some(default_level) = state
        .cfg
        .models
        .get(model_id)
        .and_then(|model| model.default_reasoning_level.as_deref())
    else {
        return body;
    };
    let Some(obj) = body.as_object_mut() else {
        return body;
    };
    match protocol {
        Protocol::OpenaiChatCompletions => {
            obj.insert("reasoning_effort".to_string(), json!(default_level));
        }
        Protocol::OpenaiResponses => {
            obj.insert("reasoning".to_string(), json!({"effort": default_level}));
        }
        Protocol::Anthropic => {
            obj.insert(
                "thinking".to_string(),
                json!({"type": "enabled", "effort": default_level}),
            );
        }
        Protocol::Antigravity => {}
    }
    body
}

pub(super) fn body_with_mapped_reasoning(
    state: &AppState,
    protocol: Protocol,
    mut body: Value,
    model_id: &str,
    plan: &ExecutionPlan,
) -> Result<Value> {
    if effective_enable_thinking(state, model_id, &plan.provider_id) == Some(false) {
        return Ok(body_without_reasoning(protocol, body));
    }
    let Some(level) = requested_reasoning_level(protocol, &body).map(ToOwned::to_owned) else {
        return Ok(body);
    };
    let Some(api_value) = mapped_reasoning_api_value(state, model_id, &plan.provider_id, &level)?
    else {
        bail!(
            "model {model_id:?} reasoning level {level:?} disables upstream thinking for provider {:?}",
            plan.provider_id
        );
    };
    set_reasoning_level(protocol, &mut body, &api_value);
    Ok(body)
}

pub(super) fn effective_enable_thinking(
    state: &AppState,
    model_id: &str,
    provider_id: &str,
) -> Option<bool> {
    state
        .cfg
        .models
        .get(model_id)
        .and_then(|model| model.enable_thinking)
        .or_else(|| {
            state
                .cfg
                .providers
                .get(provider_id)
                .and_then(|provider| provider.enable_thinking)
        })
}

pub(super) fn mapped_reasoning_api_value(
    state: &AppState,
    model_id: &str,
    provider_id: &str,
    level: &str,
) -> Result<Option<String>> {
    if let Some(mapping) = state
        .cfg
        .models
        .get(model_id)
        .and_then(|model| model.reasoning_level_map.as_deref())
        .and_then(|mappings| mappings.iter().find(|mapping| mapping.level == level))
    {
        return Ok(mapping.api_value.clone());
    }
    if let Some(mapping) = state
        .cfg
        .providers
        .get(provider_id)
        .and_then(|provider| provider.reasoning_level_map.as_deref())
        .and_then(|mappings| mappings.iter().find(|mapping| mapping.level == level))
    {
        return Ok(mapping.api_value.clone());
    }
    Ok(Some(level.to_string()))
}

pub(super) fn set_reasoning_level(protocol: Protocol, body: &mut Value, api_value: &str) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    match protocol {
        Protocol::OpenaiChatCompletions => {
            obj.insert("reasoning_effort".to_string(), json!(api_value));
        }
        Protocol::OpenaiResponses => {
            obj.entry("reasoning")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .map(|reasoning| reasoning.insert("effort".to_string(), json!(api_value)));
        }
        Protocol::Anthropic => {
            obj.entry("thinking")
                .or_insert_with(|| json!({"type": "enabled"}))
                .as_object_mut()
                .map(|thinking| thinking.insert("effort".to_string(), json!(api_value)));
        }
        Protocol::Antigravity => {}
    }
}

pub(super) fn body_without_reasoning(protocol: Protocol, mut body: Value) -> Value {
    let Some(obj) = body.as_object_mut() else {
        return body;
    };
    match protocol {
        Protocol::OpenaiChatCompletions => {
            obj.remove("reasoning_effort");
        }
        Protocol::OpenaiResponses => {
            obj.remove("reasoning");
        }
        Protocol::Anthropic => {
            obj.remove("thinking");
        }
        Protocol::Antigravity => {}
    }
    body
}

pub(super) fn request_has_image(protocol: Protocol, body: &Value) -> bool {
    content_part_types(protocol, body)
        .any(|kind| matches!(kind, "image_url" | "input_image" | "image"))
}

pub(super) fn request_has_document(protocol: Protocol, body: &Value) -> bool {
    content_part_types(protocol, body)
        .any(|kind| matches!(kind, "file" | "input_file" | "document"))
}

pub(super) fn content_part_types<'a>(
    protocol: Protocol,
    body: &'a Value,
) -> Box<dyn Iterator<Item = &'a str> + 'a> {
    match protocol {
        Protocol::OpenaiChatCompletions | Protocol::Anthropic => {
            let messages = body
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten();
            Box::new(messages.flat_map(|message| {
                message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.get("type").and_then(Value::as_str))
            }))
        }
        Protocol::OpenaiResponses => {
            let items = body
                .get("input")
                .and_then(Value::as_array)
                .into_iter()
                .flatten();
            Box::new(items.flat_map(|item| {
                item.get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.get("type").and_then(Value::as_str))
            }))
        }
        Protocol::Antigravity => Box::new(std::iter::empty()),
    }
}
