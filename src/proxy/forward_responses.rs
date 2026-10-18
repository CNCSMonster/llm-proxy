//! Proxy submodule extracted from `proxy::mod`.

use super::*;

pub(super) async fn forward_responses_via_chat(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    chat_body: Value,
    original_responses_req: Option<&Value>,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
) -> Response {
    let stream_response = chat_body.get("stream").and_then(Value::as_bool) == Some(true);

    let req = state.client.post(&plan.native_url).json(&chat_body);
    let req = match apply_bearer_auth(state, plan, req, json_error).await {
        Ok(req) => req,
        Err(response) => return response,
    };

    let upstream = match send_upstream(state, plan, req, stream_response).await {
        Ok(resp) => resp,
        Err(err) => {
            record_send_failure(
                state,
                frontend_model,
                plan,
                "openai_responses",
                token,
                request_start,
                &err,
            );
            return upstream_send_failure_response(err, responses_error);
        }
    };
    let ttfb_ms = if stream_response {
        None
    } else {
        Some(request_start.elapsed().as_millis() as u64)
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        record_request_failure(
            state,
            frontend_model,
            plan,
            "openai_responses",
            token,
            request_start,
            "upstream_error",
        );
        return upstream_error_responses(state, token, frontend_model, upstream).await;
    }

    if stream_response {
        chat_sse_to_responses_sse(
            state,
            upstream,
            frontend_model.to_string(),
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
            original_responses_req,
            token,
            request_start,
            plan.routed_target.clone(),
        )
        .await
    } else {
        let raw = match upstream.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                record_request_failure(
                    state,
                    frontend_model,
                    plan,
                    "openai_responses",
                    token,
                    request_start,
                    "invalid_json",
                );
                return responses_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        // Byte accounting reads the received wire bytes directly; serializing
        // the parsed value back to a Vec just to measure its length is a full
        // re-encode per request (cost item S7).
        let response_bytes = raw.len() as u64;
        let value = match serde_json::from_slice::<Value>(&raw) {
            Ok(value) => value,
            Err(err) => {
                record_request_failure(
                    state,
                    frontend_model,
                    plan,
                    "openai_responses",
                    token,
                    request_start,
                    "invalid_json",
                );
                return responses_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = request_start.elapsed().as_millis() as i64;

        // Extract and record usage from chat response:
        // preserve actual known counters internally without inventing zeros or total counts.
        // Internal total is local derived input+output; missing/invalid counters do not insert zeros.
        let correlation_id = value.get("id").and_then(Value::as_str).unwrap_or("unknown");
        if let Some(usage) = value.get("usage") {
            let (cache_hit, cache_miss) = crate::usage_stats::extract_cache_tokens(usage);
            let input_tokens = usage.get("prompt_tokens").and_then(Value::as_i64);
            let output_tokens = usage.get("completion_tokens").and_then(Value::as_i64);
            let valid_input = input_tokens.filter(|&v| v >= 0);
            let valid_output = output_tokens.filter(|&v| v >= 0);
            let sum = match (valid_input, valid_output) {
                (Some(i), Some(o)) => i.checked_add(o),
                _ => None,
            };

            if let (Some(input), Some(output), Some(_)) = (valid_input, valid_output, sum) {
                // : detect truncation from Chat finish_reason.
                let (truncated, truncation_reason) = value
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|c| c.first())
                    .and_then(|c| c.get("finish_reason"))
                    .and_then(Value::as_str)
                    .map(|r| match r {
                        "length" => (true, Some("max_tokens")),
                        "content_filter" => (true, Some("content_filter")),
                        _ => (false, None),
                    })
                    .unwrap_or((false, None));
                state.record_usage_with_status_and_target(
                    frontend_model.to_string(),
                    plan.provider_id.clone(),
                    "openai_responses".to_string(),
                    input,
                    output,
                    cache_hit,
                    cache_miss,
                    Some(latency_ms),
                    token,
                    Some("completed"),
                    false, // non-streaming
                    response_bytes,
                    truncated,
                    truncation_reason,
                    ttfb_ms,
                    plan.routed_target.clone(),
                );
            } else {
                let mut missing = Vec::new();
                if input_tokens.is_none() || valid_input.is_none() {
                    missing.push("prompt_tokens");
                }
                if output_tokens.is_none() || valid_output.is_none() {
                    missing.push("completion_tokens");
                }
                if valid_input.is_some() && valid_output.is_some() && sum.is_none() {
                    missing.push("total_tokens_overflow");
                }
                tracing::warn!(
                    target: "llm_proxy::proxy",
                    correlation_id = %correlation_id,
                    missing_fields = ?missing,
                    "upstream usage incomplete or invalid, omitted fabricated counts from internal storage"
                );
            }
        } else {
            tracing::warn!(
                target: "llm_proxy::proxy",
                correlation_id = %correlation_id,
                missing_fields = ?["usage"],
                "upstream usage absent, omitted fabricated counts from internal storage"
            );
        }

        Json(convert::chat_to_responses_with_context(
            value,
            frontend_model,
            original_responses_req,
        ))
        .into_response()
    }
}

pub(super) async fn forward_responses_native(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    exec_model: &str,
    mut body: Value,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
) -> Response {
    let client_wants_stream = body.get("stream").and_then(Value::as_bool) == Some(true);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), json!(plan.upstream_model));
    }
    apply_codex_service_tier(state, exec_model, &mut body);
    convert::apply_responses_egress_compat(&mut body, &plan.compat, plan.store);
    let req = state.client.post(&plan.native_url).json(&body);
    let req = match apply_bearer_auth(state, plan, req, json_error).await {
        Ok(req) => req,
        Err(response) => return response,
    };

    let upstream = match send_upstream(state, plan, req, client_wants_stream).await {
        Ok(resp) => resp,
        Err(err) => {
            record_send_failure(
                state,
                frontend_model,
                plan,
                "openai_responses",
                token,
                request_start,
                &err,
            );
            return upstream_send_failure_response(err, responses_error);
        }
    };
    let ttfb_ms = if client_wants_stream {
        None
    } else {
        Some(request_start.elapsed().as_millis() as u64)
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        record_request_failure(
            state,
            frontend_model,
            plan,
            "openai_responses",
            token,
            request_start,
            "upstream_error",
        );
        return upstream_error_responses(state, token, frontend_model, upstream).await;
    }

    if client_wants_stream {
        responses_native_sse_rewrite_model(
            state,
            upstream,
            frontend_model.to_string(),
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
            token,
            request_start,
            plan.routed_target.clone(),
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
            token,
            ttfb_ms,
            plan.routed_target.clone(),
            request_start,
        )
        .await
    } else {
        let raw = match upstream.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                record_request_failure(
                    state,
                    frontend_model,
                    plan,
                    "openai_responses",
                    token,
                    request_start,
                    "invalid_json",
                );
                return responses_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        // Wire-byte accounting without a measure-only re-encode (cost item S7).
        let response_bytes = raw.len() as u64;
        let mut value = match serde_json::from_slice::<Value>(&raw) {
            Ok(value) => value,
            Err(err) => {
                record_request_failure(
                    state,
                    frontend_model,
                    plan,
                    "openai_responses",
                    token,
                    request_start,
                    "invalid_json",
                );
                return responses_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = request_start.elapsed().as_millis() as i64;

        // Non-streaming passthrough bodies carry usage/status at the top level
        // (OpenAI responses shape); charge through the shared reader so this
        // path stays aligned with the chat entry. The SSE-style nested envelope
        // only exists inside stream events and never appears here.
        // response_bytes (wire length) is captured above the usage gate.
        record_usage_from_responses_value(
            state,
            &value,
            frontend_model,
            &plan.provider_id,
            plan.frontend_protocol.field_name(),
            Some(latency_ms),
            token,
            false, // non-streaming
            response_bytes,
            ttfb_ms,
            plan.routed_target.clone(),
        );
        // A missed charge must never be silent: log why nothing was recorded.
        match value.get("usage") {
            None => tracing::debug!(
                target: "llm_proxy::proxy",
                provider = %plan.provider_id,
                model = %frontend_model,
                "non-streaming responses body has no top-level usage field; usage not recorded"
            ),
            Some(usage)
                if usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    == 0
                    && usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        == 0 =>
            {
                tracing::debug!(
                    target: "llm_proxy::proxy",
                    provider = %plan.provider_id,
                    model = %frontend_model,
                    "non-streaming responses usage has zero token counts; usage not recorded"
                )
            }
            Some(_) => {}
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
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
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

    let wants_stream = anthropic_body.get("stream").and_then(Value::as_bool) == Some(true);
    let upstream = match send_upstream(state, plan, req, wants_stream).await {
        Ok(resp) => resp,
        Err(err) => {
            record_send_failure(
                state,
                frontend_model,
                plan,
                "openai_responses",
                token,
                request_start,
                &err,
            );
            return upstream_send_failure_response(err, responses_error);
        }
    };
    let ttfb_ms = if wants_stream {
        None
    } else {
        Some(request_start.elapsed().as_millis() as u64)
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        record_request_failure(
            state,
            frontend_model,
            plan,
            "openai_responses",
            token,
            request_start,
            "upstream_error",
        );
        return upstream_error_responses(state, token, frontend_model, upstream).await;
    }

    if wants_stream {
        anthropic_sse_to_responses_sse(
            state,
            upstream,
            frontend_model.to_string(),
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
            token,
            request_start,
            plan.routed_target.clone(),
        )
        .await
    } else {
        let raw = match upstream.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                record_request_failure(
                    state,
                    frontend_model,
                    plan,
                    "openai_responses",
                    token,
                    request_start,
                    "invalid_json",
                );
                return responses_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        // Wire-byte accounting without a measure-only re-encode (cost item S7).
        let response_bytes = raw.len() as u64;
        let value = match serde_json::from_slice::<Value>(&raw) {
            Ok(value) => value,
            Err(err) => {
                record_request_failure(
                    state,
                    frontend_model,
                    plan,
                    "openai_responses",
                    token,
                    request_start,
                    "invalid_json",
                );
                return responses_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = request_start.elapsed().as_millis() as i64;
        if let Some(usage) = value.get("usage") {
            let input_tokens = usage
                .get("input_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let output_tokens = usage
                .get("output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            // response_bytes (wire length) is captured above the usage gate.
            // : detect truncation from Anthropic stop_reason.
            let (truncated, truncation_reason) =
                match value.get("stop_reason").and_then(Value::as_str) {
                    Some("max_tokens") => (true, Some("max_tokens")),
                    _ => (false, None),
                };
            state.record_usage_with_status_and_target(
                frontend_model.to_string(),
                plan.provider_id.clone(),
                plan.frontend_protocol.field_name().to_string(),
                input_tokens,
                output_tokens,
                Some(latency_ms),
                None,
                None,
                token,
                Some("completed"),
                false, // non-streaming
                response_bytes,
                truncated,
                truncation_reason,
                ttfb_ms,
                plan.routed_target.clone(),
            );
        }
        Json(convert::anthropic_to_responses(value, frontend_model)).into_response()
    }
}
