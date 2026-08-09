//! Proxy submodule extracted from `proxy::mod`.

use super::*;

pub(super) async fn forward_chat_request(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    body: Value,
) -> Response {
    let stream = body.get("stream").and_then(Value::as_bool) == Some(true);
    let req = state.client.post(&plan.native_url).json(&body);
    let req = match apply_bearer_auth(state, plan, req, json_error).await {
        Ok(req) => req,
        Err(response) => return response,
    };

    let upstream = match send_upstream(state, plan, req).await {
        Ok(resp) => resp,
        Err(err) => return upstream_send_failure_response(err, json_error),
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_json(upstream).await;
    }

    if stream {
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
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = start_time.elapsed().as_millis() as i64;

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
            state.record_usage(
                frontend_model.to_string(),
                plan.provider_id.clone(),
                "openai_chat".to_string(),
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

pub(super) async fn forward_chat_via_responses(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    mut responses_body: Value,
) -> Response {
    // Invariant: client_wants_stream must be recorded before egress adaptation
    // (force_stream overrides stream to true; the client's intent is what drives
    // the response shape).
    let client_wants_stream = responses_body.get("stream").and_then(Value::as_bool) == Some(true);
    convert::apply_responses_egress_compat(&mut responses_body, &plan.compat, plan.store);
    let req = state.client.post(&plan.native_url).json(&responses_body);
    let req = match apply_bearer_auth(state, plan, req, json_error).await {
        Ok(req) => req,
        Err(response) => return response,
    };

    let upstream = match send_upstream(state, plan, req).await {
        Ok(resp) => resp,
        Err(err) => return upstream_send_failure_response(err, json_error),
    };

    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_json(upstream).await;
    }

    if client_wants_stream {
        responses_sse_to_chat_sse(
            state,
            upstream,
            frontend_model.to_string(),
            plan.provider_id.clone(),
            plan.frontend_protocol.field_name().to_string(),
        )
        .await
    } else if plan.compat.effective_force_stream() {
        // Upstream forces stream:true while the client wants non-streaming:
        // aggregate the SSE stream to JSON, then run the standard JSON conversion.
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
                Json(convert::responses_to_chat_response(value, frontend_model)).into_response()
            }
            Err(err) => json_error(StatusCode::BAD_GATEWAY, &format!("upstream failure: {err}")),
        }
    } else {
        let start_time = std::time::Instant::now();
        let value = match upstream.json::<Value>().await {
            Ok(value) => value,
            Err(err) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("invalid upstream JSON: {err}"),
                );
            }
        };
        let latency_ms = start_time.elapsed().as_millis() as i64;
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
            state.record_usage(
                frontend_model.to_string(),
                plan.provider_id.clone(),
                plan.frontend_protocol.field_name().to_string(),
                input_tokens,
                output_tokens,
                Some(latency_ms),
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
    let upstream = match send_upstream(state, plan, req).await {
        Ok(resp) => resp,
        Err(err) => return upstream_send_failure_response(err, json_error),
    };
    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_json(upstream).await;
    }
    if anthropic_body.get("stream").and_then(Value::as_bool) == Some(true) {
        anthropic_sse_to_chat_sse(
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
                return json_error(
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
        Json(convert::anthropic_to_chat_response(value, frontend_model)).into_response()
    }
}
