//! Provider-agnostic client model catalog.
//!
//! The server projects its full configuration into this shape for client-only
//! consumers. Real provider ids, endpoints and auth metadata never cross the
//! boundary; clients synthesize a local `llm-proxy` provider from the model
//! metadata and keep using the existing launch/status code paths.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Config, ModelCapabilityBounds, Protocol, ReasoningLevelEntry};

/// Version of the client-facing catalog contract.
pub const CLIENT_VIEW_VERSION: u32 = 1;

/// Which view the server decided to return for the caller.
///
/// The client never sends a permission flag; the server derives this from the
/// authenticated token's admin bit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientView {
    #[default]
    Client,
    Admin,
}

/// Protocols a client can speak to llm-proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientProtocol {
    OpenaiChat,
    OpenaiResponses,
    Anthropic,
}

impl ClientProtocol {
    pub fn to_protocol(self) -> Protocol {
        match self {
            Self::OpenaiChat => Protocol::OpenaiChatCompletions,
            Self::OpenaiResponses => Protocol::OpenaiResponses,
            Self::Anthropic => Protocol::Anthropic,
        }
    }

    /// Provider endpoint field name in `ProviderConfig`.
    pub fn endpoint_field(self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai_chat",
            Self::OpenaiResponses => "openai_responses",
            Self::Anthropic => "anthropic",
        }
    }

    /// Model binding field name in `ModelConfig`.
    pub fn binding_field(self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai_chat_providers",
            Self::OpenaiResponses => "openai_responses_providers",
            Self::Anthropic => "anthropic_providers",
        }
    }

    /// Proxy endpoint path used by the synthesized local provider.
    pub fn endpoint_path(self) -> &'static str {
        match self {
            Self::OpenaiChat => "/openai/v1/chat/completions",
            Self::OpenaiResponses => "/openai/v1/responses",
            Self::Anthropic => "/anthropic/v1/messages",
        }
    }
}

/// Model metadata visible to both client-only consumers and server managers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientModel {
    pub context_window: i64,
    pub max_output_tokens: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<ReasoningLevelEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_think: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_no_think: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_think: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_reasoning_off_format: Option<String>,
    #[serde(default)]
    pub protocols: Vec<ClientProtocol>,
}

fn default_client_view_version() -> u32 {
    CLIENT_VIEW_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientModelCatalog {
    #[serde(default = "default_client_view_version")]
    pub client_view: u32,
    #[serde(default)]
    pub view: ClientView,
    #[serde(default)]
    pub models: BTreeMap<String, ClientModel>,
}

impl Default for ClientModelCatalog {
    fn default() -> Self {
        Self {
            client_view: CLIENT_VIEW_VERSION,
            view: ClientView::Client,
            models: BTreeMap::new(),
        }
    }
}

/// Synthesize a local `Config` from the provider-agnostic catalog.
///
/// The generated config contains exactly one provider (`llm-proxy`) whose
/// endpoints point back at the proxy's `base_url`, plus per-model bindings to
/// that provider. Existing launch/status code can then consume it unchanged.
pub fn catalog_to_config(catalog: &ClientModelCatalog, base_url: &str) -> Result<Config> {
    if catalog.models.is_empty() {
        bail!("client model catalog is empty");
    }

    let mut used_protocols = BTreeSet::new();
    for model in catalog.models.values() {
        used_protocols.extend(model.protocols.iter().copied());
    }
    if used_protocols.is_empty() {
        bail!("client model catalog has no protocol bindings");
    }

    let base = base_url.trim_end_matches('/');
    let mut provider = serde_json::Map::new();
    provider.insert("product".to_string(), serde_json::json!("custom"));
    for protocol in &used_protocols {
        provider.insert(
            protocol.endpoint_field().to_string(),
            serde_json::json!({ "url": format!("{base}{}", protocol.endpoint_path()) }),
        );
    }

    let mut models = serde_json::Map::new();
    for (id, model) in &catalog.models {
        let mut entry = serde_json::Map::new();
        entry.insert(
            "context_window".to_string(),
            serde_json::json!(model.context_window),
        );
        entry.insert(
            "max_output_tokens".to_string(),
            serde_json::json!(model.max_output_tokens),
        );
        if !model.features.is_empty() {
            entry.insert("features".to_string(), serde_json::json!(model.features));
        }
        if let Some(levels) = &model.reasoning_levels {
            entry.insert("reasoning_levels".to_string(), serde_json::json!(levels));
        }
        if let Some(value) = &model.default_reasoning_level {
            entry.insert(
                "default_reasoning_level".to_string(),
                serde_json::json!(value),
            );
        }
        if let Some(value) = model.supports_thinking {
            entry.insert("supports_thinking".to_string(), serde_json::json!(value));
        }
        if let Some(value) = model.force_think {
            entry.insert("force_think".to_string(), serde_json::json!(value));
        }
        if let Some(value) = model.force_no_think {
            entry.insert("force_no_think".to_string(), serde_json::json!(value));
        }
        if let Some(value) = model.default_think {
            entry.insert("default_think".to_string(), serde_json::json!(value));
        }
        if let Some(value) = &model.chat_reasoning_off_format {
            entry.insert(
                "chat_reasoning_off_format".to_string(),
                serde_json::json!(value),
            );
        }

        for protocol in &model.protocols {
            let binding = serde_json::json!({ "name": "llm-proxy", "model": id });
            entry
                .entry(protocol.binding_field().to_string())
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .expect("binding field is always an array")
                .push(binding);
        }
        models.insert(id.clone(), serde_json::Value::Object(entry));
    }

    let value = serde_json::json!({
        "providers": { "llm-proxy": serde_json::Value::Object(provider) },
        "models": serde_json::Value::Object(models),
    });
    let cfg: Config = serde_json::from_value(value)
        .context("failed to synthesize config from client model catalog")?;
    cfg.validate()?;
    Ok(cfg)
}

/// Project a full server config into the provider-agnostic client catalog.
///
/// `grant` filters models by `allowed_models`; `view` records the server's
/// role decision for the caller. Routing models are materialized with their
/// conservative capability bounds and route-level vocabulary, mirroring the
/// existing `/admin/client-config` behavior.
pub fn build_client_catalog(
    cfg: &Config,
    grant: Option<&crate::token::auth::TokenGrant>,
    view: ClientView,
) -> ClientModelCatalog {
    let mut models = BTreeMap::new();
    for (id, model) in &cfg.models {
        if let Some(grant) = grant
            && !grant.model_allowed(id)
        {
            continue;
        }

        let bounds = cfg
            .model_capability_bounds(id)
            .unwrap_or_else(|| ModelCapabilityBounds {
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
                features: model.features.clone(),
            });

        let protocols: Vec<ClientProtocol> = [
            ClientProtocol::OpenaiChat,
            ClientProtocol::OpenaiResponses,
            ClientProtocol::Anthropic,
        ]
        .into_iter()
        .filter(|protocol| cfg.model_serves_protocol(id, protocol.to_protocol()))
        .collect();
        if protocols.is_empty() {
            continue;
        }

        let (supports_thinking, force_think) = if model.is_routing_model() {
            let has_off_route = model
                .routes()
                .iter()
                .any(|route| route.level == "none" || route.level == "off");
            (
                Some(true),
                if has_off_route {
                    model.force_think
                } else {
                    Some(true)
                },
            )
        } else {
            (model.supports_thinking, model.force_think)
        };

        models.insert(
            id.clone(),
            ClientModel {
                context_window: bounds.context_window,
                max_output_tokens: bounds.max_output_tokens,
                features: bounds.features,
                reasoning_levels: cfg.client_reasoning_levels(id),
                default_reasoning_level: cfg.client_default_reasoning_level(id),
                supports_thinking,
                force_think,
                force_no_think: model.force_no_think,
                default_think: model.default_think,
                chat_reasoning_off_format: model.chat_reasoning_off_format.clone(),
                protocols,
            },
        );
    }

    ClientModelCatalog {
        client_view: CLIENT_VIEW_VERSION,
        view,
        models,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_catalog() -> ClientModelCatalog {
        ClientModelCatalog {
            client_view: CLIENT_VIEW_VERSION,
            view: ClientView::Client,
            models: BTreeMap::from([(
                "vendor-model".to_string(),
                ClientModel {
                    context_window: 200,
                    max_output_tokens: 20,
                    features: vec!["tool_call".to_string()],
                    reasoning_levels: Some(vec![
                        ReasoningLevelEntry::Simple("low".to_string()),
                        ReasoningLevelEntry::Simple("high".to_string()),
                    ]),
                    default_reasoning_level: Some("low".to_string()),
                    supports_thinking: Some(true),
                    force_think: None,
                    force_no_think: None,
                    default_think: None,
                    chat_reasoning_off_format: None,
                    protocols: vec![ClientProtocol::OpenaiChat, ClientProtocol::Anthropic],
                },
            )]),
        }
    }

    #[test]
    fn catalog_serialization_has_no_provider_fields() {
        let text = serde_json::to_string(&sample_catalog()).expect("serialize catalog");
        assert!(text.contains("openai_chat"));
        assert!(text.contains("anthropic"));
        assert!(!text.contains("providers"));
        assert!(!text.contains("endpoint"));
        assert!(!text.contains("\"auth\""));
    }

    #[test]
    fn catalog_serde_roundtrip() {
        let catalog = sample_catalog();
        let value = serde_json::to_value(&catalog).expect("to value");
        let back: ClientModelCatalog = serde_json::from_value(value).expect("from value");
        assert_eq!(back.client_view, catalog.client_view);
        assert_eq!(back.view, catalog.view);
        assert_eq!(back.models.len(), 1);
        let model = back.models.get("vendor-model").expect("model");
        assert_eq!(model.context_window, 200);
        assert_eq!(model.protocols.len(), 2);
    }

    #[test]
    fn catalog_to_config_synthesizes_llm_proxy_provider() {
        let cfg = catalog_to_config(&sample_catalog(), "https://example.test/lp")
            .expect("synthesize config");

        assert_eq!(cfg.providers.len(), 1);
        let provider = cfg.providers.get("llm-proxy").expect("synthetic provider");
        assert!(provider.openai_chat.is_some());
        assert!(provider.anthropic.is_some());
        assert!(provider.openai_responses.is_none());

        let model = cfg.models.get("vendor-model").expect("model");
        assert_eq!(model.context_window, 200);
        assert_eq!(model.max_output_tokens, 20);
        assert_eq!(model.openai_chat_providers.len(), 1);
        assert_eq!(model.openai_chat_providers[0].name, "llm-proxy");
        assert_eq!(model.openai_chat_providers[0].model, "vendor-model");
        assert_eq!(model.anthropic_providers.len(), 1);
        assert_eq!(model.anthropic_providers[0].name, "llm-proxy");
        assert!(model.openai_responses_providers.is_empty());
        assert_eq!(model.default_reasoning_level.as_deref(), Some("low"));
    }

    #[test]
    fn empty_catalog_is_rejected() {
        let err = catalog_to_config(&ClientModelCatalog::default(), "http://127.0.0.1:8989")
            .expect_err("empty catalog must fail");
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn build_catalog_projects_models_and_applies_allowed_models() {
        let cfg = crate::config::default_deepseek_config();
        let catalog = build_client_catalog(&cfg, None, ClientView::Client);
        assert_eq!(catalog.view, ClientView::Client);
        let model = catalog
            .models
            .get("deepseek-v4-flash-lp")
            .expect("default model projected");
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_output_tokens, 393_216);
        assert!(model.protocols.contains(&ClientProtocol::OpenaiChat));
        assert!(model.protocols.contains(&ClientProtocol::Anthropic));

        let text = serde_json::to_string(&catalog).expect("serialize catalog");
        assert!(!text.contains("\"providers\""));
        assert!(!text.contains("\"endpoint\""));
        assert!(!text.contains("\"auth\""));

        let grant = crate::token::auth::TokenGrant::Granted(crate::token::auth::TokenContext {
            key: "lp-test".to_string(),
            name: "test".to_string(),
            allowed_models: vec!["deepseek-v4-pro-lp".to_string()],
            admin: false,
        });
        let filtered = build_client_catalog(&cfg, Some(&grant), ClientView::Client);
        assert_eq!(filtered.models.len(), 1);
        assert!(filtered.models.contains_key("deepseek-v4-pro-lp"));
    }
}
