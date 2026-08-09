//! Proxy submodule extracted from `proxy::mod`.

use super::*;

pub(super) async fn forward_responses_via_antigravity(
    state: &AppState,
    plan: &crate::config::ExecutionPlan,
    frontend_model: &str,
    antigravity_body: Value,
    stream_response: bool,
    token: Option<String>,
) -> Response {
    // Inject cached thought_signatures into functionCall parts.
    let mut body = antigravity_body;
    state.inject_thought_signatures(&mut body);

    // Use ?alt=sse for streaming to get proper SSE format from antigravity API.
    let url = if stream_response && !plan.native_url.contains("alt=sse") {
        format!("{}?alt=sse", plan.native_url)
    } else {
        plan.native_url.clone()
    };
    let mut req = state
        .client
        .post(&url)
        .header("User-Agent", "antigravity/hub/2.2.1 darwin/arm64")
        .json(&body);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let upstream = match send_upstream(state, plan, req).await {
        Ok(resp) => resp,
        Err(err) => return upstream_send_failure_response(err, responses_error),
    };
    if upstream.status().is_client_error() || upstream.status().is_server_error() {
        return upstream_error_responses(upstream).await;
    }
    if stream_response {
        antigravity_sse_to_responses_sse(
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
        // Collect thought_signatures for next multi-turn request.
        state.collect_thought_signatures(&value);
        Json(convert::antigravity_to_responses_response(
            value,
            frontend_model,
        ))
        .into_response()
    }
}
