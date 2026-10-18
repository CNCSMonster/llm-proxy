//! Proxy submodule extracted from `proxy::mod`.

use super::*;

pub(super) async fn forward_chat_request(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    body: Value,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
) -> Response {
    let stream = body.get("stream").and_then(Value::as_bool) == Some(true);
    let req = state.client.post(&plan.native_url).json(&body);
    let req = match apply_bearer_auth(state, plan, req, json_error).await {
        Ok(req) => req,
        Err(response) => return response,
    };

    let upstream = match send_upstream(state, plan, req, stream).await {
        Ok(resp) => resp,
        Err(err) => {
            record_send_failure(
                state,
                frontend_model,
                plan,
                "openai_chat",
                token,
                request_start,
                &err,
            );
            return upstream_send_failure_response(err, json_error);
        }
    };
    // Non-streaming TTFB = headers arrival − request send (streaming TTFB
    // is measured at first SSE chunk inside the relay).
    let ttfb_ms = if stream {
        None
    } else {
        Some(request_start.elapsed().as_millis() as u64)
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        record_request_failure(
            state,
            frontend_model,
            plan,
            "openai_chat",
            token,
            request_start,
            "upstream_error",
        );
        return upstream_error_json(state, token, frontend_model, upstream).await;
    }

    if stream {
        passthrough_sse(
            state,
            "passthrough",
            upstream,
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
            frontend_model.to_string(),
            token,
            request_start,
            plan.routed_target.clone(),
        )
        .await
    } else {
        let mut value = match upstream.json::<Value>().await {
            Ok(value) => value,
            Err(err) => {
                record_request_failure(
                    state,
                    frontend_model,
                    plan,
                    "openai_chat",
                    token,
                    request_start,
                    "invalid_json",
                );
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = request_start.elapsed().as_millis() as i64;

        // Extract and record usage
        if let Some(usage) = value.get("usage") {
            let input_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let output_tokens = usage
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let (cache_hit, cache_miss) = crate::usage_stats::extract_cache_tokens(usage);
            let response_bytes = serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(0) as u64;
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
                "openai_chat".to_string(),
                input_tokens,
                output_tokens,
                Some(latency_ms),
                cache_hit,
                cache_miss,
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

        if let Some(obj) = value.as_object_mut() {
            obj.insert("model".to_string(), json!(frontend_model));
        }
        Json(value).into_response()
    }
}

pub(super) async fn forward_chat_via_responses(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    exec_model: &str,
    mut responses_body: Value,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
) -> Response {
    // Invariant: client_wants_stream must be recorded before egress adaptation
    // (force_stream overrides stream to true; the client's intent is what drives
    // the response shape).
    let client_wants_stream = responses_body.get("stream").and_then(Value::as_bool) == Some(true);
    apply_codex_service_tier(state, exec_model, &mut responses_body);
    convert::apply_responses_egress_compat(&mut responses_body, &plan.compat, plan.store);
    let req = state.client.post(&plan.native_url).json(&responses_body);
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
                "openai_chat",
                token,
                request_start,
                &err,
            );
            return upstream_send_failure_response(err, json_error);
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
            "openai_chat",
            token,
            request_start,
            "upstream_error",
        );
        return upstream_error_json(state, token, frontend_model, upstream).await;
    }

    if client_wants_stream {
        responses_sse_to_chat_sse(
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
        // Upstream forces stream:true while the client wants non-streaming:
        // aggregate the SSE stream to JSON, then run the standard JSON conversion.
        match aggregate_responses_sse_to_value(state, upstream).await {
            Ok(value) => {
                let response_bytes =
                    serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(0) as u64;
                record_usage_from_responses_value(
                    state,
                    &value,
                    frontend_model,
                    &plan.provider_id,
                    plan.frontend_protocol.field_name(),
                    Some(request_start.elapsed().as_millis() as i64),
                    token,
                    false, // client received aggregated JSON (non-streaming experience)
                    response_bytes,
                    ttfb_ms,
                    plan.routed_target.clone(),
                );
                Json(convert::responses_to_chat_response(value, frontend_model)).into_response()
            }
            Err(err) => {
                record_request_failure(
                    state,
                    frontend_model,
                    plan,
                    plan.frontend_protocol.field_name(),
                    token,
                    request_start,
                    "invalid_json",
                );
                json_error(StatusCode::BAD_GATEWAY, &format!("upstream failure: {err}"))
            }
        }
    } else {
        let value = match upstream.json::<Value>().await {
            Ok(value) => value,
            Err(err) => {
                record_request_failure(
                    state,
                    frontend_model,
                    plan,
                    "openai_chat",
                    token,
                    request_start,
                    "invalid_json",
                );
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = request_start.elapsed().as_millis() as i64;
        // Extract usage from Responses-native response (input_tokens/output_tokens)
        if let Some(usage) = value.get("usage") {
            let input_tokens = usage
                .get("input_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let output_tokens = usage
                .get("output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let (cache_hit, cache_miss) = crate::usage_stats::extract_cache_tokens(usage);
            let response_bytes = serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(0) as u64;
            // : detect truncation from Responses status.
            let (truncated, truncation_reason) =
                if value.get("status").and_then(Value::as_str) == Some("incomplete") {
                    let reason = value
                        .get("incomplete_details")
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        .map(|r| match r {
                            "max_output_tokens" => "max_tokens",
                            _ => "other",
                        });
                    (true, reason)
                } else {
                    (false, None)
                };
            state.record_usage_with_status_and_target(
                frontend_model.to_string(),
                plan.provider_id.clone(),
                plan.frontend_protocol.field_name().to_string(),
                input_tokens,
                output_tokens,
                Some(latency_ms),
                cache_hit,
                cache_miss,
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
        Json(convert::responses_to_chat_response(value, frontend_model)).into_response()
    }
}

pub(super) async fn forward_chat_via_anthropic(
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
    let req = match apply_anthropic_auth(state, plan, req, json_error).await {
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
                "openai_chat",
                token,
                request_start,
                &err,
            );
            return upstream_send_failure_response(err, json_error);
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
            "openai_chat",
            token,
            request_start,
            "upstream_error",
        );
        return upstream_error_json(state, token, frontend_model, upstream).await;
    }
    if wants_stream {
        anthropic_sse_to_chat_sse(
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
        let value = match upstream.json::<Value>().await {
            Ok(value) => value,
            Err(err) => {
                record_request_failure(
                    state,
                    frontend_model,
                    plan,
                    "openai_chat",
                    token,
                    request_start,
                    "invalid_json",
                );
                return json_error(
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
            let (cache_hit, cache_miss) = crate::usage_stats::extract_cache_tokens(usage);
            let response_bytes = serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(0) as u64;
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
                cache_hit,
                cache_miss,
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
        Json(convert::anthropic_to_chat_response(value, frontend_model)).into_response()
    }
}
