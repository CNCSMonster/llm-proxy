//! Proxy submodule extracted from `proxy::mod`.

use super::*;

pub(super) async fn forward_responses_via_chat(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    chat_body: Value,
) -> Response {
    let stream_response = chat_body.get("stream").and_then(Value::as_bool) == Some(true);

    let req = state.client.post(&plan.native_url).json(&chat_body);
    let req = match apply_bearer_auth(state, plan, req, json_error).await {
        Ok(req) => req,
        Err(response) => return response,
    };

    let upstream = match send_upstream(state, plan, req).await {
        Ok(resp) => resp,
        Err(err) => return upstream_send_failure_response(err, responses_error),
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_responses(upstream).await;
    }

    if stream_response {
        chat_sse_to_responses_sse(
            state,
            upstream,
            frontend_model.to_string(),
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
        )
        .await
    } else {
        let start_time = std::time::Instant::now();
        let value = match upstream.json::<Value>().await {
            Ok(value) => value,
            Err(err) => {
                return responses_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = start_time.elapsed().as_millis() as i64;

        // Extract and record usage from chat response
        if let Some(usage) = value.get("usage") {
            let input_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let output_tokens = usage
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            state.record_usage(
                frontend_model.to_string(),
                plan.provider_id.clone(),
                "openai_responses".to_string(),
                input_tokens,
                output_tokens,
                Some(latency_ms),
            );
        }

        Json(convert::chat_to_responses(value, frontend_model)).into_response()
    }
}

pub(super) async fn forward_responses_native(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    mut body: Value,
) -> Response {
    let client_wants_stream = body.get("stream").and_then(Value::as_bool) == Some(true);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), json!(plan.upstream_model));
    }
    convert::apply_responses_egress_compat(&mut body, &plan.compat, plan.store);
    let req = state.client.post(&plan.native_url).json(&body);
    let req = match apply_bearer_auth(state, plan, req, json_error).await {
        Ok(req) => req,
        Err(response) => return response,
    };

    let upstream = match send_upstream(state, plan, req).await {
        Ok(resp) => resp,
        Err(err) => return upstream_send_failure_response(err, responses_error),
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_responses(upstream).await;
    }

    if client_wants_stream {
        responses_native_sse_rewrite_model(
            state,
            upstream,
            frontend_model.to_string(),
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
        )
        .await
    } else if plan.compat.effective_force_stream() {
        // upstream forces stream:true, aggregate SSE to JSON
        aggregate_responses_sse_to_json(
            state,
            upstream,
            frontend_model.to_string(),
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
        )
        .await
    } else {
        let start_time = std::time::Instant::now();
        let mut value = match upstream.json::<Value>().await {
            Ok(value) => value,
            Err(err) => {
                return responses_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = start_time.elapsed().as_millis() as i64;

        // Extract and record usage from responses API
        if let Some(usage) = value.get("response").and_then(|r| r.get("usage")) {
            let input_tokens = usage
                .get("input_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let output_tokens = usage
                .get("output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            state.record_usage(
                frontend_model.to_string(),
                plan.provider_id.clone(),
                "openai_responses".to_string(),
                input_tokens,
                output_tokens,
                Some(latency_ms),
            );
        }

        if let Some(obj) = value.as_object_mut() {
            obj.insert("model".to_string(), json!(frontend_model));
            if let Some(response) = obj.get_mut("response").and_then(Value::as_object_mut) {
                response.insert("model".to_string(), json!(frontend_model));
            }
        }
        Json(value).into_response()
    }
}

/// Aggregate SSE events from a Responses API stream into a single responses JSON value.
/// Used when the client wants non-streaming but the upstream forces streaming.
/// Collects message text, usage, and function_call items (tool calls must not be
/// lost: coding-agent clients rely on them).
///
/// Memory protection: the aggregator caps the SSE buffer and output item count
/// to prevent unbounded growth from a malicious or hung upstream. Limits are
/// configurable via `[server]` section in config.toml.
pub(super) async fn forward_responses_via_anthropic(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    anthropic_body: Value,
) -> Response {
    let req = state
        .client
        .post(&plan.native_url)
        .header("anthropic-version", "2023-06-01")
        .json(&anthropic_body);
    let req = match apply_anthropic_auth(state, plan, req, responses_error).await {
        Ok(req) => req,
        Err(response) => return response,
    };

    let upstream = match send_upstream(state, plan, req).await {
        Ok(resp) => resp,
        Err(err) => return upstream_send_failure_response(err, responses_error),
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_responses(upstream).await;
    }

    if anthropic_body.get("stream").and_then(Value::as_bool) == Some(true) {
        anthropic_sse_to_responses_sse(
            state,
            upstream,
            frontend_model.to_string(),
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
        )
        .await
    } else {
        let start_time = std::time::Instant::now();
        let value = match upstream.json::<Value>().await {
            Ok(value) => value,
            Err(err) => {
                return responses_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = start_time.elapsed().as_millis() as i64;
        if let Some(usage) = value.get("usage") {
            let input_tokens = usage
                .get("input_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let output_tokens = usage
                .get("output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            state.record_usage(
                frontend_model.to_string(),
                plan.provider_id.clone(),
                plan.frontend_protocol.field_name().to_string(),
                input_tokens,
                output_tokens,
                Some(latency_ms),
            );
        }
        Json(convert::anthropic_to_responses(value, frontend_model)).into_response()
    }
}
