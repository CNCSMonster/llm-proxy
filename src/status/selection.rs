use serde::{Deserialize, Serialize};

use crate::config::{Config, Protocol};
use crate::pattern::PatternMatcher;

/// Target selector for status probing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSelection {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl ProbeSelection {
    /// Returns true if both `provider` and `model` are `None`.
    pub fn is_empty(&self) -> bool {
        self.provider.is_none() && self.model.is_none()
    }

    /// Returns candidate bindings `(provider_id, frontend_model_id, protocol)`
    /// filtered by this selection. Supports exact match and glob wildcards (`*`, `?`, `[...]`).
    /// An empty selection returns all candidate bindings.
    pub fn candidates(&self, cfg: &Config) -> Vec<(String, String, Protocol)> {
        self.candidates_restricted(cfg, None)
    }

    /// Candidate bindings restricted by an optional allowed_models whitelist.
    pub fn candidates_restricted(
        &self,
        cfg: &Config,
        allowed_models: Option<&std::collections::BTreeSet<String>>,
    ) -> Vec<(String, String, Protocol)> {
        let mut candidates = Vec::new();
        let model_matcher = self
            .model
            .as_deref()
            .and_then(|m| PatternMatcher::new(m).ok());
        let provider_matcher = self
            .provider
            .as_deref()
            .and_then(|p| PatternMatcher::new(p).ok());

        for (model_id, model_config) in &cfg.models {
            if let Some(allowed) = allowed_models
                && !allowed.contains(model_id)
            {
                continue;
            }
            if let Some(ref matcher) = model_matcher {
                if !matcher.is_match(model_id) {
                    continue;
                }
            } else if let Some(ref target_model) = self.model
                && model_id != target_model
            {
                continue;
            }
            for protocol in Protocol::CLIENT_PROTOCOLS {
                for binding in model_config.provider_bindings(protocol) {
                    if let Some(ref matcher) = provider_matcher {
                        if !matcher.is_match(&binding.name) {
                            continue;
                        }
                    } else if let Some(ref target_provider) = self.provider
                        && &binding.name != target_provider
                    {
                        continue;
                    }
                    candidates.push((binding.name.clone(), model_id.clone(), protocol));
                }
            }
        }
        candidates
    }

    /// Validates this selection against the authoritative config.
    ///
    /// - Empty selection always succeeds (`Ok(())`).
    /// - If pattern syntax is invalid, returns `Err` naming the syntax error.
    /// - If `provider` is specified but has no matches in `cfg.providers`, returns `Err`.
    /// - If `model` is specified but has no matches in `cfg.models`, returns `Err`.
    /// - If both match (or either matches and the other None) but candidates is empty, returns `Err`
    ///   identifying that matched objects have no candidates/intersection.
    pub fn validate(&self, cfg: &Config) -> Result<(), String> {
        if self.is_empty() {
            return Ok(());
        }

        let p_matcher = if let Some(ref provider) = self.provider {
            let m = PatternMatcher::new(provider)?;
            let matched = cfg.providers.keys().any(|k| m.is_match(k));
            if !matched {
                if m.is_glob() {
                    return Err(format!("no provider matches pattern '{provider}'"));
                } else {
                    return Err(format!("unknown provider: '{provider}'"));
                }
            }
            Some(m)
        } else {
            None
        };

        let m_matcher = if let Some(ref model) = self.model {
            let m = PatternMatcher::new(model)?;
            let matched = cfg.models.keys().any(|k| m.is_match(k));
            if !matched {
                if m.is_glob() {
                    return Err(format!("no model matches pattern '{model}'"));
                } else {
                    return Err(format!("unknown model: '{model}'"));
                }
            }
            Some(m)
        } else {
            None
        };

        let candidates = self.candidates(cfg);
        if candidates.is_empty() {
            match (&self.provider, &self.model) {
                (Some(provider), Some(model)) => {
                    return Err(format!(
                        "no probe candidates found for intersection of provider '{provider}' and model '{model}'"
                    ));
                }
                (Some(provider), None) => {
                    if p_matcher.as_ref().map(|m| m.is_glob()).unwrap_or(false) {
                        return Err(format!(
                            "no configured bindings found for providers matching '{provider}'"
                        ));
                    } else {
                        return Err(format!(
                            "provider '{provider}' is known but has no configured bindings"
                        ));
                    }
                }
                (None, Some(model)) => {
                    if m_matcher.as_ref().map(|m| m.is_glob()).unwrap_or(false) {
                        return Err(format!(
                            "no configured bindings found for models matching '{model}'"
                        ));
                    } else {
                        return Err(format!(
                            "model '{model}' is known but has no configured bindings"
                        ));
                    }
                }
                (None, None) => unreachable!("is_empty handled above"),
            }
        }

        Ok(())
    }

    /// Returns true if `provider_id` matches the selection criteria given `cfg`.
    pub fn matches_provider(&self, provider_id: &str, cfg: &Config) -> bool {
        if self.is_empty() {
            return true;
        }

        if let Some(ref p_pat) = self.provider {
            if let Ok(m) = PatternMatcher::new(p_pat) {
                if !m.is_match(provider_id) {
                    return false;
                }
            } else if provider_id != p_pat {
                return false;
            }
        }

        if let Some(ref m_pat) = self.model {
            let m_matcher = PatternMatcher::new(m_pat).ok();
            let mut used_by_matching_model = false;
            for (m_id, m_cfg) in &cfg.models {
                let model_matches = match &m_matcher {
                    Some(matcher) => matcher.is_match(m_id),
                    None => m_id == m_pat,
                };
                if !model_matches {
                    continue;
                }
                // Direct bindings
                let has_binding = Protocol::CLIENT_PROTOCOLS.iter().any(|proto| {
                    m_cfg
                        .provider_bindings(*proto)
                        .iter()
                        .any(|b| b.name == provider_id)
                });
                if has_binding {
                    used_by_matching_model = true;
                    break;
                }
                // Routing model targets
                if m_cfg.is_routing_model() {
                    for route in m_cfg.routes() {
                        if let Some(target_cfg) = cfg.models.get(&route.target) {
                            let target_has_binding =
                                Protocol::CLIENT_PROTOCOLS.iter().any(|proto| {
                                    target_cfg
                                        .provider_bindings(*proto)
                                        .iter()
                                        .any(|b| b.name == provider_id)
                                });
                            if target_has_binding {
                                used_by_matching_model = true;
                                break;
                            }
                        }
                    }
                    if used_by_matching_model {
                        break;
                    }
                }
            }
            if !used_by_matching_model {
                return false;
            }
        }

        true
    }

    /// Returns true if `model_id` matches the selection criteria given `cfg`.
    pub fn matches_model(&self, model_id: &str, cfg: &Config) -> bool {
        if self.is_empty() {
            return true;
        }

        if let Some(ref m_pat) = self.model {
            if let Ok(m) = PatternMatcher::new(m_pat) {
                if !m.is_match(model_id) {
                    return false;
                }
            } else if model_id != m_pat {
                return false;
            }
        }

        if let Some(ref p_pat) = self.provider {
            let p_matcher = PatternMatcher::new(p_pat).ok();
            let Some(m_cfg) = cfg.models.get(model_id) else {
                return false;
            };

            let matches_p = |p_name: &str| -> bool {
                match &p_matcher {
                    Some(m) => m.is_match(p_name),
                    None => p_name == p_pat,
                }
            };

            let has_matching_binding = Protocol::CLIENT_PROTOCOLS.iter().any(|proto| {
                m_cfg
                    .provider_bindings(*proto)
                    .iter()
                    .any(|b| matches_p(&b.name))
            });

            if has_matching_binding {
                return true;
            }

            if m_cfg.is_routing_model() {
                for route in m_cfg.routes() {
                    if let Some(target_cfg) = cfg.models.get(&route.target) {
                        let target_has_binding = Protocol::CLIENT_PROTOCOLS.iter().any(|proto| {
                            target_cfg
                                .provider_bindings(*proto)
                                .iter()
                                .any(|b| matches_p(&b.name))
                        });
                        if target_has_binding {
                            return true;
                        }
                    }
                }
            }

            return false;
        }

        true
    }

    /// Returns true if a specific provider binding matches `self.provider`.
    pub fn matches_binding_provider(&self, provider_name: &str) -> bool {
        if let Some(ref p_pat) = self.provider {
            if let Ok(m) = PatternMatcher::new(p_pat) {
                m.is_match(provider_name)
            } else {
                provider_name == p_pat
            }
        } else {
            true
        }
    }

    /// Returns true if a cooldown entry matches the selection.
    pub fn matches_cooldown(&self, provider: &str, model: &str) -> bool {
        if let Some(ref p_pat) = self.provider {
            let matches_p = if let Ok(m) = PatternMatcher::new(p_pat) {
                m.is_match(provider)
            } else {
                provider == p_pat
            };
            if !matches_p {
                return false;
            }
        }

        if let Some(ref m_pat) = self.model {
            let matches_m = if let Ok(m) = PatternMatcher::new(m_pat) {
                m.is_match(model)
            } else {
                model == m_pat
            };
            if !matches_m {
                return false;
            }
        }

        true
    }

    /// Returns a human-readable description of the selection.
    /// Empty -> "all"; otherwise e.g. "provider=deepseek, model=deepseek-v4-lp" (only showing present fields).
    pub fn describe(&self) -> String {
        match (&self.provider, &self.model) {
            (None, None) => "all".to_string(),
            (Some(provider), None) => format!("provider={provider}"),
            (None, Some(model)) => format!("model={model}"),
            (Some(provider), Some(model)) => format!("provider={provider}, model={model}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> Config {
        let toml_str = r#"
[server]
listen = "127.0.0.1:8989"

[providers.p1]
product = "p1"

[providers.p2]
product = "p2"

[providers.p_lonely]
product = "p_lonely"

[models.m1]
context_window = 100000
max_output_tokens = 4096
openai_chat_providers = [{ name = "p1", model = "upstream-1-chat" }]
anthropic_providers = [{ name = "p1", model = "upstream-1-claude" }]

[models.m2]
context_window = 100000
max_output_tokens = 4096
openai_chat_providers = [{ name = "p2", model = "upstream-2" }]

[models.m_empty]
context_window = 100000
max_output_tokens = 4096
"#;
        toml::from_str(toml_str).expect("parse sample config")
    }

    #[test]
    fn test_is_empty() {
        assert!(ProbeSelection::default().is_empty());
        assert!(
            !ProbeSelection {
                provider: Some("p1".into()),
                model: None,
            }
            .is_empty()
        );
        assert!(
            !ProbeSelection {
                provider: None,
                model: Some("m1".into()),
            }
            .is_empty()
        );
        assert!(
            !ProbeSelection {
                provider: Some("p1".into()),
                model: Some("m1".into()),
            }
            .is_empty()
        );
    }

    #[test]
    fn test_describe() {
        assert_eq!(ProbeSelection::default().describe(), "all");
        assert_eq!(
            ProbeSelection {
                provider: Some("deepseek".into()),
                model: None,
            }
            .describe(),
            "provider=deepseek"
        );
        assert_eq!(
            ProbeSelection {
                provider: None,
                model: Some("deepseek-v4-lp".into()),
            }
            .describe(),
            "model=deepseek-v4-lp"
        );
        assert_eq!(
            ProbeSelection {
                provider: Some("deepseek".into()),
                model: Some("deepseek-v4-lp".into()),
            }
            .describe(),
            "provider=deepseek, model=deepseek-v4-lp"
        );
    }

    #[test]
    fn test_serde() {
        let sel = ProbeSelection {
            provider: Some("p1".into()),
            model: None,
        };
        let json = serde_json::to_string(&sel).expect("serialize");
        let parsed: ProbeSelection = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(sel, parsed);
    }

    #[test]
    fn test_candidates_all() {
        let cfg = sample_config();
        let sel = ProbeSelection::default();
        let candidates = sel.candidates(&cfg);
        assert_eq!(candidates.len(), 3);
        assert_eq!(
            candidates,
            vec![
                (
                    "p1".to_string(),
                    "m1".to_string(),
                    Protocol::OpenaiChatCompletions
                ),
                ("p1".to_string(), "m1".to_string(), Protocol::Anthropic),
                (
                    "p2".to_string(),
                    "m2".to_string(),
                    Protocol::OpenaiChatCompletions
                ),
            ]
        );
        assert!(sel.validate(&cfg).is_ok());
    }

    #[test]
    fn test_candidates_only_provider() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: Some("p1".into()),
            model: None,
        };
        let candidates = sel.candidates(&cfg);
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates,
            vec![
                (
                    "p1".to_string(),
                    "m1".to_string(),
                    Protocol::OpenaiChatCompletions
                ),
                ("p1".to_string(), "m1".to_string(), Protocol::Anthropic),
            ]
        );
        assert!(sel.validate(&cfg).is_ok());
    }

    #[test]
    fn test_candidates_only_model() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: None,
            model: Some("m1".into()),
        };
        let candidates = sel.candidates(&cfg);
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates,
            vec![
                (
                    "p1".to_string(),
                    "m1".to_string(),
                    Protocol::OpenaiChatCompletions
                ),
                ("p1".to_string(), "m1".to_string(), Protocol::Anthropic),
            ]
        );
        assert!(sel.validate(&cfg).is_ok());
    }

    #[test]
    fn test_candidates_intersection() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: Some("p1".into()),
            model: Some("m1".into()),
        };
        let candidates = sel.candidates(&cfg);
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates,
            vec![
                (
                    "p1".to_string(),
                    "m1".to_string(),
                    Protocol::OpenaiChatCompletions
                ),
                ("p1".to_string(), "m1".to_string(), Protocol::Anthropic),
            ]
        );
        assert!(sel.validate(&cfg).is_ok());
    }

    #[test]
    fn test_candidates_glob_model() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: None,
            model: Some("m*".into()),
        };
        let candidates = sel.candidates(&cfg);
        assert_eq!(candidates.len(), 3);
        assert!(sel.validate(&cfg).is_ok());

        let sel_single = ProbeSelection {
            provider: None,
            model: Some("m?".into()),
        };
        let candidates_single = sel_single.candidates(&cfg);
        assert_eq!(candidates_single.len(), 3);
        assert!(sel_single.validate(&cfg).is_ok());
    }

    #[test]
    fn test_candidates_glob_provider() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: Some("p*".into()),
            model: None,
        };
        let candidates = sel.candidates(&cfg);
        assert_eq!(candidates.len(), 3);
        assert!(sel.validate(&cfg).is_ok());
    }

    #[test]
    fn test_validate_unknown_provider() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: Some("unknown_prov".into()),
            model: None,
        };
        let err = sel.validate(&cfg).unwrap_err();
        assert!(
            err.contains("unknown provider") && err.contains("unknown_prov"),
            "err: {err}"
        );
    }

    #[test]
    fn test_validate_glob_no_matching_provider() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: Some("unknown-*".into()),
            model: None,
        };
        let err = sel.validate(&cfg).unwrap_err();
        assert!(
            err.contains("no provider matches pattern 'unknown-*'"),
            "err: {err}"
        );
    }

    #[test]
    fn test_validate_unknown_model() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: None,
            model: Some("unknown_mod".into()),
        };
        let err = sel.validate(&cfg).unwrap_err();
        assert!(
            err.contains("unknown model") && err.contains("unknown_mod"),
            "err: {err}"
        );
    }

    #[test]
    fn test_validate_glob_no_matching_model() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: None,
            model: Some("unknown-*".into()),
        };
        let err = sel.validate(&cfg).unwrap_err();
        assert!(
            err.contains("no model matches pattern 'unknown-*'"),
            "err: {err}"
        );
    }

    #[test]
    fn test_validate_invalid_glob_syntax() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: None,
            model: Some("m[".into()),
        };
        let err = sel.validate(&cfg).unwrap_err();
        assert!(err.contains("invalid glob pattern"), "err: {err}");
    }

    #[test]
    fn test_validate_known_provider_no_bindings() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: Some("p_lonely".into()),
            model: None,
        };
        let err = sel.validate(&cfg).unwrap_err();
        assert!(
            err.contains("p_lonely") && err.contains("no configured bindings"),
            "err: {err}"
        );
        assert!(
            !err.contains("unknown"),
            "err should not say unknown: {err}"
        );
    }

    #[test]
    fn test_validate_known_model_no_bindings() {
        let cfg = sample_config();
        let sel = ProbeSelection {
            provider: None,
            model: Some("m_empty".into()),
        };
        let err = sel.validate(&cfg).unwrap_err();
        assert!(
            err.contains("m_empty") && err.contains("no configured bindings"),
            "err: {err}"
        );
        assert!(
            !err.contains("unknown"),
            "err should not say unknown: {err}"
        );
    }

    #[test]
    fn test_validate_known_intersection_empty() {
        let cfg = sample_config();
        // p2 exists, m1 exists, but m1 only binds to p1
        let sel = ProbeSelection {
            provider: Some("p2".into()),
            model: Some("m1".into()),
        };
        let err = sel.validate(&cfg).unwrap_err();
        assert!(
            err.contains("p2") && err.contains("m1") && err.contains("intersection"),
            "err: {err}"
        );
        assert!(
            !err.contains("unknown"),
            "err should not say unknown: {err}"
        );
    }

    #[test]
    fn test_matches_helpers() {
        let cfg = sample_config();
        let sel_m1 = ProbeSelection {
            provider: None,
            model: Some("m1".into()),
        };
        assert!(sel_m1.matches_model("m1", &cfg));
        assert!(!sel_m1.matches_model("m2", &cfg));
        assert!(sel_m1.matches_provider("p1", &cfg));
        assert!(!sel_m1.matches_provider("p2", &cfg));
        assert!(sel_m1.matches_cooldown("p1", "m1"));
        assert!(!sel_m1.matches_cooldown("p1", "m2"));

        let sel_p2 = ProbeSelection {
            provider: Some("p2".into()),
            model: None,
        };
        assert!(!sel_p2.matches_model("m1", &cfg));
        assert!(sel_p2.matches_model("m2", &cfg));
        assert!(!sel_p2.matches_provider("p1", &cfg));
        assert!(sel_p2.matches_provider("p2", &cfg));
        assert!(sel_p2.matches_binding_provider("p2"));
        assert!(!sel_p2.matches_binding_provider("p1"));
    }
}
