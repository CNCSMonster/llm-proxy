use anyhow::Result;
use reqwest::RequestBuilder;
use serde_json::{Value, json};

use crate::config::{AuthConfig, ExecutionPlan, Protocol, ResolvedAuth};

/// 探活请求的统一输出上限：须高于默认思考预算（1024）与常见上游最小输出
/// 约束，避免思考类端点以「预算不低于上限 / 上限过小」为由拒绝探活，
/// 把健康模型误报成故障；后续可按 provider 能力自适应收敛。
const PROBE_MAX_OUTPUT_TOKENS: u32 = 16_384;

#[cfg(test)]
pub fn probe_body(plan: &ExecutionPlan) -> Result<Value> {
    probe_body_with_auth(plan, None)
}

pub fn probe_body_with_auth(plan: &ExecutionPlan, _auth: Option<&ResolvedAuth>) -> Result<Value> {
    match plan.source_protocol {
        Protocol::OpenaiChatCompletions => {
            // Use the provider's configured max_tokens field name
            // (default: "max_completion_tokens"; legacy models override via compat.max_tokens_field)
            let max_tokens_key = plan.compat.effective_max_tokens_field();
            let mut body = json!({
                "model": plan.upstream_model,
                "messages": [{"role": "user", "content": "Reply with exactly: pong"}],
                "stream": false
            });
            body[max_tokens_key] = json!(PROBE_MAX_OUTPUT_TOKENS);
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
                "max_output_tokens": PROBE_MAX_OUTPUT_TOKENS,
                "stream": false
            });
            // Reuse the runtime egress adaptation (ADR-017) so the
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
            "max_tokens": PROBE_MAX_OUTPUT_TOKENS,
            "stream": false
        })),
    }
}

pub const ANTHROPIC_USER_AGENT: &str = "anthropic-typescript/0.39.0";
pub const OPENAI_USER_AGENT: &str = "OpenAI/Python 1.65.0";

pub fn apply_protocol_headers(plan: &ExecutionPlan, request: RequestBuilder) -> RequestBuilder {
    // ADR-017: 统一遵循各协议官方客户端真实标准 UA，
    // 严禁使用自曝代理或探测器身份的 User-Agent（如 llm-proxy 或 connectivity-probe），
    // 避免触发风控或百炼 Coding Plan 等端点的 agent 检测（如 405 "only available for Coding Agents"）。
    match plan.source_protocol {
        Protocol::Anthropic => request
            .header("User-Agent", ANTHROPIC_USER_AGENT)
            .header("anthropic-version", "2023-06-01"),
        Protocol::OpenaiChatCompletions | Protocol::OpenaiResponses => {
            request.header("User-Agent", OPENAI_USER_AGENT)
        }
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
            routed_target: None,
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
        // default max_tokens_field is now "max_completion_tokens"
        assert!(
            probe_body(&plan(Protocol::OpenaiChatCompletions))
                .unwrap()
                .get("max_completion_tokens")
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
    }

    #[test]
    fn probe_max_tokens_stay_above_default_thinking_budget() {
        // 探活输出上限必须高于默认思考预算（1024）与常见上游最小输出约束，
        // 否则思考类端点会以「预算不低于上限 / 上限过小」为由拒绝探活请求，
        // 把健康模型误报成故障并可能触发上游 400 配额累计。
        const MIN_PROBE_CAP: u64 = 16_384;

        let chat = probe_body(&plan(Protocol::OpenaiChatCompletions)).unwrap();
        let cap = chat
            .get("max_completion_tokens")
            .or_else(|| chat.get("max_tokens"))
            .and_then(Value::as_u64);
        assert!(
            cap.is_some_and(|c| c >= MIN_PROBE_CAP),
            "chat probe cap must be >= {MIN_PROBE_CAP}, got {cap:?}"
        );

        let responses = probe_body(&plan(Protocol::OpenaiResponses)).unwrap();
        let cap = responses.get("max_output_tokens").and_then(Value::as_u64);
        assert!(
            cap.is_some_and(|c| c >= MIN_PROBE_CAP),
            "responses probe cap must be >= {MIN_PROBE_CAP}, got {cap:?}"
        );

        let anthropic = probe_body(&plan(Protocol::Anthropic)).unwrap();
        let cap = anthropic.get("max_tokens").and_then(Value::as_u64);
        assert!(
            cap.is_some_and(|c| c >= MIN_PROBE_CAP),
            "anthropic probe cap must be >= {MIN_PROBE_CAP}, got {cap:?}"
        );
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
    fn chat_probe_uses_legacy_max_tokens_when_explicitly_configured() {
        // backward compat — explicit "max_tokens" override still works
        let mut p = plan(Protocol::OpenaiChatCompletions);
        p.compat = CompatConfig {
            max_tokens_field: Some("max_tokens".to_string()),
            ..CompatConfig::default()
        };
        let body = probe_body(&p).unwrap();
        assert!(
            body.get("max_tokens").is_some(),
            "legacy max_tokens_field override must produce max_tokens key"
        );
        assert!(
            body.get("max_completion_tokens").is_none(),
            "must not emit both keys"
        );
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

    #[test]
    fn protocol_headers_use_official_clients_and_never_expose_probe() {
        let client = reqwest::Client::new();
        let protocols = [
            Protocol::Anthropic,
            Protocol::OpenaiChatCompletions,
            Protocol::OpenaiResponses,
        ];

        for proto in protocols {
            let p = plan(proto);
            let req = apply_protocol_headers(&p, client.post("http://127.0.0.1/test"))
                .build()
                .expect("build request");
            let ua = req
                .headers()
                .get(reqwest::header::USER_AGENT)
                .expect("User-Agent header must be present")
                .to_str()
                .expect("valid utf-8 User-Agent");

            // ADR-017: 绝不能出现自曝特征
            assert!(
                !ua.contains("llm-proxy"),
                "User-Agent for {proto:?} must never expose 'llm-proxy', got: {ua}"
            );
            assert!(
                !ua.contains("connectivity-probe"),
                "User-Agent for {proto:?} must never expose 'connectivity-probe', got: {ua}"
            );

            match proto {
                Protocol::Anthropic => {
                    assert_eq!(ua, ANTHROPIC_USER_AGENT);
                    let anthropic_ver = req
                        .headers()
                        .get("anthropic-version")
                        .expect("anthropic-version header must be present")
                        .to_str()
                        .unwrap();
                    assert_eq!(anthropic_ver, "2023-06-01");
                }
                Protocol::OpenaiChatCompletions | Protocol::OpenaiResponses => {
                    assert_eq!(ua, OPENAI_USER_AGENT);
                }
            }
        }
    }
}
