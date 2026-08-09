//! Proxy submodule extracted from `proxy::mod`.

use super::*;

pub(super) async fn forward_anthropic_via_antigravity(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    mut antigravity_body: Value,
    stream_response: bool,
    token: Option<String>,
) -> Response {
    // Re-inject cached thought_signatures into functionCall parts of the
    // antigravity request (Anthropic ingress multi-turn parity with the
    // Responses ingress path). The body shape matches: antigravity envelope
    // with request.contents.
    state.inject_thought_signatures(&mut antigravity_body);
    let url = if stream_response && !plan.native_url.contains("alt=sse") {
        format!("{}?alt=sse", plan.native_url)
    } else {
        plan.native_url.clone()
    };
    let mut req = state
        .client
        .post(&url)
        .header("User-Agent", "antigravity/hub/2.2.1 darwin/arm64")
        .json(&antigravity_body);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let upstream = match send_upstream(state, plan, req).await {
        Ok(resp) => resp,
        Err(err) => return upstream_send_failure_response(err, anthropic_error),
    };
    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_anthropic(upstream).await;
    }
    if stream_response {
        antigravity_sse_to_anthropic_sse(
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
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = start_time.elapsed().as_millis() as i64;
        if let Some(usage) = value
            .get("usageMetadata")
            .or_else(|| value.get("cpaUsageMetadata"))
        {
            let input_tokens = usage
                .get("promptTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let output_tokens = usage
                .get("candidatesTokenCount")
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
        // Collect functionCall thought_signatures for the next multi-turn
        // request (parity with the Responses ingress non-streaming path).
        state.collect_thought_signatures(&value);
        Json(convert::antigravity_to_anthropic_response(
            value,
            frontend_model,
        ))
        .into_response()
    }
}

pub(super) async fn forward_anthropic_via_responses(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    mut responses_body: Value,
) -> Response {
    // Invariant: client_wants_stream must be recorded before egress adaptation.
    let client_wants_stream = responses_body.get("stream").and_then(Value::as_bool) == Some(true);
    convert::apply_responses_egress_compat(&mut responses_body, &plan.compat, plan.store);
    let req = state.client.post(&plan.native_url).json(&responses_body);
    let req = match apply_bearer_auth(state, plan, req, anthropic_error).await {
        Ok(req) => req,
        Err(response) => return response,
    };

    let upstream = match send_upstream(state, plan, req).await {
        Ok(resp) => resp,
        Err(err) => return upstream_send_failure_response(err, anthropic_error),
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_anthropic(upstream).await;
    }

    if client_wants_stream {
        responses_sse_to_anthropic_sse(
            state,
            upstream,
            frontend_model.to_string(),
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
        )
        .await
    } else if plan.compat.effective_force_stream() {
        match aggregate_responses_sse_to_value(state, upstream).await {
            Ok(value) => {
                record_usage_from_responses_value(
                    state,
                    &value,
                    frontend_model,
                    &plan.provider_id,
                    plan.frontend_protocol.field_name(),
                    None,
                );
                Json(convert::responses_to_anthropic_response(
                    value,
                    frontend_model,
                ))
                .into_response()
            }
            Err(err) => {
                anthropic_error(StatusCode::BAD_GATEWAY, &format!("upstream failure: {err}"))
            }
        }
    } else {
        let value = match upstream.json::<Value>().await {
            Ok(value) => value,
            Err(err) => {
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        Json(convert::responses_to_anthropic_response(
            value,
            frontend_model,
        ))
        .into_response()
    }
}

pub(super) async fn forward_anthropic_via_chat(
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
        Err(err) => return upstream_send_failure_response(err, anthropic_error),
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_anthropic(upstream).await;
    }

    if stream_response {
        chat_sse_to_anthropic_sse(
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
                return anthropic_error(
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
                "anthropic".to_string(),
                input_tokens,
                output_tokens,
                Some(latency_ms),
            );
        }

        Json(convert::chat_to_anthropic(value, frontend_model)).into_response()
    }
}

pub(super) async fn forward_anthropic_native(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    mut body: Value,
) -> Response {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), json!(plan.upstream_model));
    }
    let stream_response = body.get("stream").and_then(Value::as_bool) == Some(true);

    let req = state
        .client
        .post(&plan.native_url)
        .header("anthropic-version", "2023-06-01")
        .json(&body);
    let req = match apply_anthropic_auth(state, plan, req, anthropic_error).await {
        Ok(req) => req,
        Err(response) => return response,
    };

    let upstream = match send_upstream(state, plan, req).await {
        Ok(resp) => resp,
        Err(err) => return upstream_send_failure_response(err, anthropic_error),
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_anthropic(upstream).await;
    }

    if stream_response {
        passthrough_sse(
            state,
            "passthrough",
            upstream,
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
            frontend_model.to_string(),
        )
        .await
    } else {
        let start_time = std::time::Instant::now();
        let mut value = match upstream.json::<Value>().await {
            Ok(value) => value,
            Err(err) => {
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = start_time.elapsed().as_millis() as i64;

        // Extract and record usage from anthropic response
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
                "anthropic".to_string(),
                input_tokens,
                output_tokens,
                Some(latency_ms),
            );
        }

        if let Some(obj) = value.as_object_mut() {
            obj.insert("model".to_string(), json!(frontend_model));
        }
        Json(value).into_response()
    }
}
