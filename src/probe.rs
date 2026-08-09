use anyhow::Result;
use reqwest::RequestBuilder;
use serde_json::{Value, json};

use crate::config::{AuthConfig, ExecutionPlan, Protocol, ResolvedAuth};

#[cfg(test)]
pub fn probe_body(plan: &ExecutionPlan) -> Result<Value> {
    probe_body_with_auth(plan, None)
}

pub fn probe_body_with_auth(plan: &ExecutionPlan, auth: Option<&ResolvedAuth>) -> Result<Value> {
    match plan.source_protocol {
        Protocol::OpenaiChatCompletions => {
            // Use the provider's configured max_tokens field name (e.g., "max_completion_tokens" for MiMo)
            let max_tokens_key = plan
                .compat
                .max_tokens_field
                .as_deref()
                .unwrap_or("max_tokens");
            let mut body = json!({
                "model": plan.upstream_model,
                "messages": [{"role": "user", "content": "Reply with exactly: pong"}],
                "stream": false
            });
            body[max_tokens_key] = json!(16);
            Ok(body)
        }
        Protocol::OpenaiResponses => {
            let mut body = json!({
                "model": plan.upstream_model,
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Reply with exactly: pong"}]
                }],
                "max_output_tokens": 16,
                "stream": false
            });
            // Reuse the runtime egress adaptation (design §5.1 / ADR-017) so the
            // probe body matches what the proxy actually sends upstream.
            crate::convert::apply_responses_egress_compat(&mut body, &plan.compat, plan.store);

            // Apply adapter conversion for derived endpoints (ADR-017 compliance)
            // If the provider uses an adapter (e.g., ChatCompletionsFromResponses),
            // convert the Responses body to the target protocol format.
            match plan.adapter() {
                crate::config::AdapterKind::Passthrough => {
                    // Native Responses endpoint, no conversion needed
                    Ok(body)
                }
                crate::config::AdapterKind::ChatCompletionsFromResponses => {
                    // Convert Responses → Chat Completions format
                    crate::convert::responses_to_chat(body, &plan.upstream_model)
                }
                crate::config::AdapterKind::AnthropicFromResponses => {
                    // Convert Responses → Anthropic format
                    crate::convert::responses_to_anthropic(body, &plan.upstream_model)
                }
                // Other adapters are not valid for OpenaiResponses source protocol
                _ => Err(anyhow::anyhow!(
                    "probe: adapter {:?} is not valid for OpenaiResponses source protocol",
                    plan.adapter()
                )),
            }
        }
        Protocol::Anthropic => Ok(json!({
            "model": plan.upstream_model,
            "messages": [{"role": "user", "content": "Reply with exactly: pong"}],
            "max_tokens": 16,
            "stream": false
        })),
        Protocol::Antigravity => {
            let project_id = auth
                .and_then(|auth| auth.project_id.as_deref())
                .filter(|project_id| !project_id.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "connectivity probe for Antigravity requires OAuth project_id; run `llm-proxy provider login {}`",
                        plan.provider_id
                    )
                })?;
            Ok(json!({
                "project": project_id,
                "model": plan.upstream_model,
                "request": {
                    "contents": [{
                        "role": "user",
                        "parts": [{"text": "Reply with exactly: pong"}]
                    }],
                    "generationConfig": {
                        "maxOutputTokens": 16
                    }
                }
            }))
        }
    }
}

pub fn apply_protocol_headers(plan: &ExecutionPlan, request: RequestBuilder) -> RequestBuilder {
    // 设置中性 User-Agent 以避免触发 Coding Plan 等端点的 agent 检测
    // (如百炼 Coding Plan 返回 405 "only available for Coding Agents" 当使用 reqwest 默认 UA)
    let request = match plan.source_protocol {
        Protocol::Antigravity => request.header("User-Agent", "antigravity/hub/2.2.1 darwin/arm64"),
        _ => request.header("User-Agent", "llm-proxy/0.2.0 (connectivity-probe)"),
    };
    match plan.source_protocol {
        Protocol::Anthropic => request.header("anthropic-version", "2023-06-01"),
        _ => request,
    }
}

pub fn apply_auth_header(
    plan: &ExecutionPlan,
    request: RequestBuilder,
    token: Option<String>,
) -> RequestBuilder {
    let Some(token) = token else {
        return request;
    };
    match auth_header_style(plan) {
        AuthHeaderStyle::AnthropicApiKey => request.header("x-api-key", token),
        AuthHeaderStyle::Bearer => request.bearer_auth(token),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthHeaderStyle {
    AnthropicApiKey,
    Bearer,
}

fn auth_header_style(plan: &ExecutionPlan) -> AuthHeaderStyle {
    match (&plan.source_protocol, &plan.auth) {
        (Protocol::Anthropic, AuthConfig::ApiKeyEnv { .. }) => AuthHeaderStyle::AnthropicApiKey,
        _ => AuthHeaderStyle::Bearer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AdapterKind, AuthConfig, CompatConfig, RequestFrequencyConfig};

    fn plan(source_protocol: Protocol) -> ExecutionPlan {
        ExecutionPlan {
            frontend_protocol: source_protocol,
            provider_id: "p".to_string(),
            upstream_model: "upstream".to_string(),
            source_protocol,
            adapter: AdapterKind::Passthrough,
            native_url: "http://127.0.0.1:1".to_string(),
            auth: AuthConfig::None,
            compat: CompatConfig::default(),
            anthropic_family_models: Vec::new(),
            store: None,
            request_frequency: RequestFrequencyConfig::default(),
        }
    }

    fn plan_with_auth(source_protocol: Protocol, auth: AuthConfig) -> ExecutionPlan {
        ExecutionPlan {
            auth,
            ..plan(source_protocol)
        }
    }

    #[test]
    fn probe_body_uses_protocol_specific_token_field() {
        assert!(
            probe_body(&plan(Protocol::OpenaiChatCompletions))
                .unwrap()
                .get("max_tokens")
                .is_some()
        );
        let responses_body = probe_body(&plan(Protocol::OpenaiResponses)).unwrap();
        assert!(
            responses_body
                .get("input")
                .and_then(Value::as_array)
                .is_some(),
            "responses probe input must be a list (egress adaptation)"
        );
        assert_eq!(
            responses_body.get("store").and_then(Value::as_bool),
            Some(false)
        );
        // default compat keeps max_output_tokens (no strip configured); the
        // strip behavior is covered by responses_probe_respects_force_stream_compat.
        assert!(responses_body.get("max_output_tokens").is_some());
        assert!(
            probe_body(&plan(Protocol::Anthropic))
                .unwrap()
                .get("max_tokens")
                .is_some()
        );
        assert!(probe_body(&plan(Protocol::Antigravity)).is_err());
        let auth = ResolvedAuth {
            token: Some("token".to_string()),
            project_id: Some("project".to_string()),
        };
        let body = probe_body_with_auth(&plan(Protocol::Antigravity), Some(&auth)).unwrap();
        assert_eq!(body["project"], "project");
        assert_eq!(body["request"]["generationConfig"]["maxOutputTokens"], 16);
    }

    #[test]
    fn responses_probe_respects_force_stream_compat() {
        let mut p = plan(Protocol::OpenaiResponses);
        p.compat = CompatConfig {
            force_stream: Some(true),
            strip_max_output_tokens: Some(true),
            ..CompatConfig::default()
        };
        let body = probe_body(&p).unwrap();
        assert_eq!(body.get("stream").and_then(Value::as_bool), Some(true));
        assert_eq!(body.get("store").and_then(Value::as_bool), Some(false));
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn auth_header_style_matches_runtime_anthropic_auth_rules() {
        assert_eq!(
            auth_header_style(&plan_with_auth(
                Protocol::Anthropic,
                AuthConfig::ApiKeyEnv {
                    env: "ANTHROPIC_API_KEY".to_string()
                }
            )),
            AuthHeaderStyle::AnthropicApiKey
        );
        assert_eq!(
            auth_header_style(&plan_with_auth(
                Protocol::Anthropic,
                AuthConfig::OpenaiOauth { account: None }
            )),
            AuthHeaderStyle::Bearer
        );
        assert_eq!(
            auth_header_style(&plan_with_auth(
                Protocol::OpenaiResponses,
                AuthConfig::ApiKeyEnv {
                    env: "OPENAI_API_KEY".to_string()
                }
            )),
            AuthHeaderStyle::Bearer
        );
    }
}
