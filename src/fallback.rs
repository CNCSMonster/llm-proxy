//! Batch fallback logic for `provider fallback add`.
//!
//! Inserts a provider as a fallback for another provider across multiple
//! (model, endpoint) combinations. Both providers must belong to the same
//! product. Supports regex patterns in binding specifications.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{Config, Protocol, ProviderBinding};

/// Result of processing a single (model, endpoint) combination.
#[derive(Debug)]
pub enum FallbackBindingResult {
    Inserted {
        model_id: String,
        endpoint: String,
    },
    SkippedAlreadyInChain {
        model_id: String,
        endpoint: String,
    },
    SkippedRegexNoMatch {
        model_id: String,
        endpoint: String,
        reason: String,
    },
    Failed {
        model_id: String,
        endpoint: String,
        reason: String,
    },
}

/// Execute batch fallback: insert `provider_name` as fallback after
/// `target_name` for each (model, endpoint) combination in `binding_specs`.
///
/// Returns the per-combination results. The caller is responsible for
/// formatting output.
pub fn add_fallback(
    path: &Path,
    provider_name: &str,
    target_name: &str,
    binding_specs: &[String],
) -> Result<Vec<FallbackBindingResult>> {
    let mut cfg = Config::load(path)?;

    // Validate both providers exist
    let provider_config = cfg
        .providers
        .get(provider_name)
        .with_context(|| format!("unknown provider {provider_name:?}"))?
        .clone();
    let target_config = cfg
        .providers
        .get(target_name)
        .with_context(|| format!("unknown provider {target_name:?}"))?
        .clone();

    // Validate same product
    if provider_config.product != target_config.product {
        bail!(
            "provider {provider_name:?} (product={}) and target {target_name:?} (product={}) must belong to the same product",
            provider_config.product,
            target_config.product,
        );
    }

    // Validate not custom product
    if provider_config.is_custom_product() {
        bail!(
            "provider {provider_name:?} has product=\"custom\"; batch fallback requires a named product"
        );
    }

    // Validate provider has at least one client-protocol endpoint
    let provider_has_endpoint = Protocol::CLIENT_PROTOCOLS
        .iter()
        .any(|protocol| provider_config.endpoint(*protocol).is_some());
    if !provider_has_endpoint {
        bail!("provider {provider_name:?} has no client-protocol endpoints");
    }

    // Parse binding specs
    let parsed_specs: Vec<ParsedBindingSpec> = binding_specs
        .iter()
        .map(|spec| parse_binding_spec(spec))
        .collect::<Result<Vec<_>>>()?;

    let mut results = Vec::new();

    for spec in &parsed_specs {
        let is_explicit = !contains_regex_metacharacters(&spec.model_pattern)
            && !contains_regex_metacharacters(&spec.endpoint_pattern);

        // Find all matching (model_id, protocol) combinations
        let matches = find_matching_combinations(&cfg, spec);

        if matches.is_empty() {
            if is_explicit {
                let model_exists = cfg.models.contains_key(&spec.model_pattern);
                results.push(FallbackBindingResult::Failed {
                    model_id: spec.model_pattern.clone(),
                    endpoint: spec.endpoint_pattern.clone(),
                    reason: if !model_exists {
                        format!("model {:?} not found", spec.model_pattern)
                    } else {
                        format!(
                            "no endpoint {:?} found for model {:?}",
                            spec.endpoint_pattern, spec.model_pattern
                        )
                    },
                });
            }
            // Regex with no matches: silently skip (no output for this spec)
            continue;
        }

        for (model_id, protocol) in &matches {
            let endpoint_label = protocol_short_name(*protocol);
            let model = cfg.models.get_mut(model_id).unwrap();
            let bindings = provider_bindings_mut(model, *protocol);

            // Find target position
            let Some(target_pos) = bindings.iter().position(|b| b.name == target_name) else {
                if is_explicit {
                    results.push(FallbackBindingResult::Failed {
                        model_id: model_id.clone(),
                        endpoint: endpoint_label.to_string(),
                        reason: format!(
                            "target {target_name:?} not found in chain for {model_id}:{endpoint_label}"
                        ),
                    });
                } else {
                    results.push(FallbackBindingResult::SkippedRegexNoMatch {
                        model_id: model_id.clone(),
                        endpoint: endpoint_label.to_string(),
                        reason: format!("target {target_name:?} not in chain"),
                    });
                }
                continue;
            };

            // Check if provider already in chain (idempotent)
            if bindings.iter().any(|b| b.name == provider_name) {
                results.push(FallbackBindingResult::SkippedAlreadyInChain {
                    model_id: model_id.clone(),
                    endpoint: endpoint_label.to_string(),
                });
                continue;
            }

            // Copy upstream model from target binding
            let upstream_model = bindings[target_pos].model.clone();

            // Insert provider right after target
            bindings.insert(
                target_pos + 1,
                ProviderBinding {
                    name: provider_name.to_string(),
                    model: upstream_model,
                },
            );

            results.push(FallbackBindingResult::Inserted {
                model_id: model_id.clone(),
                endpoint: endpoint_label.to_string(),
            });
        }
    }

    // Check if any insertions were made; if so, save config
    let has_insertions = results
        .iter()
        .any(|r| matches!(r, FallbackBindingResult::Inserted { .. }));
    if has_insertions {
        // Acquire cross-process file lock (same as model.rs write path)
        let _lock = crate::core::ConfigLock::acquire(
            &crate::service::state_dir(),
            std::time::Duration::from_secs(5),
        )?;
        crate::config_edit::write_full_config(path, &cfg)?;
    }

    Ok(results)
}

/// Format all fallback results for display.
#[allow(dead_code)] // used in tests; kept for future JSON/structured output
pub fn format_results(
    results: &[FallbackBindingResult],
    provider_name: &str,
    target_name: &str,
) -> String {
    let mut output = String::new();
    let mut inserted = 0;
    let mut skipped = 0;
    let mut failed = 0;

    for result in results {
        match result {
            FallbackBindingResult::Inserted { model_id, endpoint } => {
                output.push_str(&format!(
                    "  ✓ inserted {provider_name} after {target_name} for {model_id}:{endpoint}\n",
                ));
                inserted += 1;
            }
            FallbackBindingResult::SkippedAlreadyInChain { model_id, endpoint } => {
                output.push_str(&format!(
                    "  ✓ skipped {model_id}:{endpoint} (provider already in chain)\n",
                ));
                skipped += 1;
            }
            FallbackBindingResult::SkippedRegexNoMatch {
                model_id,
                endpoint,
                reason,
            } => {
                output.push_str(&format!("  ✓ skipped {model_id}:{endpoint} ({reason})\n",));
                skipped += 1;
            }
            FallbackBindingResult::Failed {
                model_id,
                endpoint,
                reason,
            } => {
                output.push_str(&format!("  ✗ error {model_id}:{endpoint}: {reason}\n",));
                failed += 1;
            }
        }
    }

    output.push_str(&format!(
        "\n{} inserted, {} skipped, {} failed\n",
        inserted, skipped, failed
    ));
    output
}

/// Print formatted results to stdout.
pub fn print_results(results: &[FallbackBindingResult], provider_name: &str, target_name: &str) {
    let mut inserted = 0;
    let mut skipped = 0;
    let mut failed = 0;

    for result in results {
        match result {
            FallbackBindingResult::Inserted { model_id, endpoint } => {
                println!(
                    "  ✓ inserted {provider_name} after {target_name} for {model_id}:{endpoint}"
                );
                inserted += 1;
            }
            FallbackBindingResult::SkippedAlreadyInChain { model_id, endpoint } => {
                println!("  ✓ skipped {model_id}:{endpoint} (provider already in chain)");
                skipped += 1;
            }
            FallbackBindingResult::SkippedRegexNoMatch {
                model_id,
                endpoint,
                reason,
            } => {
                println!("  ✓ skipped {model_id}:{endpoint} ({reason})");
                skipped += 1;
            }
            FallbackBindingResult::Failed {
                model_id,
                endpoint,
                reason,
            } => {
                println!("  ✗ error {model_id}:{endpoint}: {reason}");
                failed += 1;
            }
        }
    }

    println!();
    println!("{inserted} inserted, {skipped} skipped, {failed} failed");
}

// ── Internal helpers ────────────────────────────────────────────────────────

#[derive(Debug)]
struct ParsedBindingSpec {
    model_pattern: String,
    endpoint_pattern: String,
}

fn parse_binding_spec(spec: &str) -> Result<ParsedBindingSpec> {
    let Some(colon_pos) = spec.find(':') else {
        bail!(
            "invalid binding spec {spec:?}: expected format model:endpoint (e.g. deepseek-v4-pro-lp:chat)"
        );
    };
    let model_pattern = &spec[..colon_pos];
    let endpoint_pattern = &spec[colon_pos + 1..];
    if model_pattern.is_empty() {
        bail!("invalid binding spec {spec:?}: model pattern is empty");
    }
    if endpoint_pattern.is_empty() {
        bail!("invalid binding spec {spec:?}: endpoint pattern is empty");
    }
    // Validate regex patterns
    regex::Regex::new(model_pattern)
        .with_context(|| format!("invalid model regex in binding spec {spec:?}"))?;
    regex::Regex::new(endpoint_pattern)
        .with_context(|| format!("invalid endpoint regex in binding spec {spec:?}"))?;
    Ok(ParsedBindingSpec {
        model_pattern: model_pattern.to_string(),
        endpoint_pattern: endpoint_pattern.to_string(),
    })
}

/// Returns true if the string contains regex metacharacters.
fn contains_regex_metacharacters(s: &str) -> bool {
    s.contains('.')
        || s.contains('*')
        || s.contains('+')
        || s.contains('?')
        || s.contains('[')
        || s.contains('(')
        || s.contains('{')
        || s.contains('|')
        || s.contains('^')
        || s.contains('$')
        || s.contains('\\')
}

/// Find all (model_id, protocol) combinations matching the binding spec.
fn find_matching_combinations(cfg: &Config, spec: &ParsedBindingSpec) -> Vec<(String, Protocol)> {
    let model_re = regex::Regex::new(&format!("^{}$", spec.model_pattern)).unwrap();
    let endpoint_re = regex::Regex::new(&format!("^{}$", spec.endpoint_pattern)).unwrap();

    let mut matches = Vec::new();

    for (model_id, model) in &cfg.models {
        if !model_re.is_match(model_id) {
            continue;
        }
        for &protocol in &Protocol::CLIENT_PROTOCOLS {
            let short_name = protocol_short_name(protocol);
            let field_name = protocol.field_name();
            let route_key = protocol.route_key();
            if endpoint_re.is_match(short_name)
                || endpoint_re.is_match(field_name)
                || endpoint_re.is_match(route_key)
            {
                let bindings = model.provider_bindings(protocol);
                if !bindings.is_empty() {
                    matches.push((model_id.clone(), protocol));
                }
            }
        }
    }

    matches
}

/// Map protocol to short name used in TUI and CLI binding specs.
fn protocol_short_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiChatCompletions => "chat",
        Protocol::OpenaiResponses => "responses",
        Protocol::Anthropic => "anthropic",
        Protocol::Antigravity => "antigravity",
    }
}

fn provider_bindings_mut(
    model: &mut crate::config::ModelConfig,
    protocol: Protocol,
) -> &mut Vec<ProviderBinding> {
    match protocol {
        Protocol::OpenaiChatCompletions => &mut model.openai_chat_providers,
        Protocol::OpenaiResponses => &mut model.openai_responses_providers,
        Protocol::Anthropic => &mut model.anthropic_providers,
        Protocol::Antigravity => unreachable!("antigravity is not a client protocol"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(&path, text).expect("write config");
        (temp, path)
    }

    /// Minimal config with two providers of the same product and one model.
    fn two_provider_config() -> &'static str {
        r#"
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[providers.deepseek-2]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY_2"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[models."deepseek-v4-flash-lp"]
context_window = 64000
max_output_tokens = 8192
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]
"#
    }

    /// Config with multiple endpoints (chat + responses).
    fn multi_endpoint_config() -> &'static str {
        r#"
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }
openai_responses = { url = "https://api.deepseek.com/v1/responses" }

[providers.deepseek-2]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY_2"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }
openai_responses = { url = "https://api.deepseek.com/v1/responses" }

[models."deepseek-v4-flash-lp"]
context_window = 64000
max_output_tokens = 8192
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]
openai_responses_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]

[models."deepseek-v4-pro-lp"]
context_window = 64000
max_output_tokens = 8192
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-pro" }]
openai_responses_providers = [{ name = "deepseek", model = "deepseek-v4-pro" }]
"#
    }

    #[test]
    fn parse_binding_spec_valid() {
        let spec = parse_binding_spec("deepseek-v4-pro-lp:chat").unwrap();
        assert_eq!(spec.model_pattern, "deepseek-v4-pro-lp");
        assert_eq!(spec.endpoint_pattern, "chat");
    }

    #[test]
    fn parse_binding_spec_regex() {
        let spec = parse_binding_spec("deepseek-v4-pro:.*").unwrap();
        assert_eq!(spec.model_pattern, "deepseek-v4-pro");
        assert_eq!(spec.endpoint_pattern, ".*");
    }

    #[test]
    fn parse_binding_spec_no_colon() {
        assert!(parse_binding_spec("deepseek-v4-pro-lp").is_err());
    }

    #[test]
    fn parse_binding_spec_empty_model() {
        assert!(parse_binding_spec(":chat").is_err());
    }

    #[test]
    fn parse_binding_spec_empty_endpoint() {
        assert!(parse_binding_spec("model:").is_err());
    }

    #[test]
    fn parse_binding_spec_invalid_regex() {
        assert!(parse_binding_spec("[invalid:chat").is_err());
        assert!(parse_binding_spec("model:[invalid").is_err());
    }

    #[test]
    fn contains_regex_metacharacters_detects_patterns() {
        assert!(!contains_regex_metacharacters("deepseek-v4-pro-lp"));
        assert!(!contains_regex_metacharacters("chat"));
        assert!(contains_regex_metacharacters(".*"));
        assert!(contains_regex_metacharacters("deepseek.*"));
        assert!(contains_regex_metacharacters("chat|responses"));
        assert!(contains_regex_metacharacters("chat+"));
        assert!(contains_regex_metacharacters("chat?"));
    }

    #[test]
    fn protocol_short_name_mapping() {
        assert_eq!(protocol_short_name(Protocol::OpenaiChatCompletions), "chat");
        assert_eq!(protocol_short_name(Protocol::OpenaiResponses), "responses");
        assert_eq!(protocol_short_name(Protocol::Anthropic), "anthropic");
    }

    #[test]
    fn add_fallback_basic_insertion() {
        let (_temp, path) = write_config(two_provider_config());
        let results = add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &["deepseek-v4-flash-lp:chat".to_string()],
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], FallbackBindingResult::Inserted { .. }));

        // Verify config was updated
        let cfg = Config::load(&path).unwrap();
        let model = cfg.models.get("deepseek-v4-flash-lp").unwrap();
        assert_eq!(model.openai_chat_providers.len(), 2);
        assert_eq!(model.openai_chat_providers[0].name, "deepseek");
        assert_eq!(model.openai_chat_providers[1].name, "deepseek-2");
        assert_eq!(model.openai_chat_providers[1].model, "deepseek-v4-flash");
    }

    #[test]
    fn add_fallback_idempotent_skip() {
        let (_temp, path) = write_config(two_provider_config());
        // First insertion
        add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &["deepseek-v4-flash-lp:chat".to_string()],
        )
        .unwrap();
        // Second insertion: should skip (already in chain)
        let results = add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &["deepseek-v4-flash-lp:chat".to_string()],
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            FallbackBindingResult::SkippedAlreadyInChain { .. }
        ));
    }

    #[test]
    fn add_fallback_regex_matches_all_endpoints() {
        let (_temp, path) = write_config(multi_endpoint_config());
        let results = add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &["deepseek-v4-flash-lp:.*".to_string()],
        )
        .unwrap();

        // Should match both chat and responses endpoints
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|r| matches!(r, FallbackBindingResult::Inserted { .. }))
        );

        // Verify config
        let cfg = Config::load(&path).unwrap();
        let model = cfg.models.get("deepseek-v4-flash-lp").unwrap();
        assert_eq!(model.openai_chat_providers.len(), 2);
        assert_eq!(model.openai_responses_providers.len(), 2);
    }

    #[test]
    fn add_fallback_regex_model_pattern() {
        let (_temp, path) = write_config(multi_endpoint_config());
        let results = add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &["deepseek-v4-.*-lp:chat".to_string()],
        )
        .unwrap();

        // Should match both models' chat endpoints
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|r| matches!(r, FallbackBindingResult::Inserted { .. }))
        );
    }

    #[test]
    fn add_fallback_explicit_missing_model_errors() {
        let (_temp, path) = write_config(two_provider_config());
        let results = add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &["nonexistent-model:chat".to_string()],
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], FallbackBindingResult::Failed { .. }));
    }

    #[test]
    fn add_fallback_explicit_missing_target_errors() {
        let config = r#"
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[providers.deepseek-2]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY_2"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[models."deepseek-v4-flash-lp"]
context_window = 64000
max_output_tokens = 8192
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]
"#;
        let (_temp, path) = write_config(config);
        // Target "deepseek-2" is not in the chain (only "deepseek" is)
        let results = add_fallback(
            &path,
            "deepseek",
            "deepseek-2",
            &["deepseek-v4-flash-lp:chat".to_string()],
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            FallbackBindingResult::Failed { ref reason, .. } if reason.contains("not found in chain")
        ));
    }

    #[test]
    fn add_fallback_regex_no_match_skips_silently() {
        let (_temp, path) = write_config(two_provider_config());
        let results = add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &["nonexistent-.*:chat".to_string()],
        )
        .unwrap();

        // Regex with no matches: no results (silently skipped)
        assert!(results.is_empty());
    }

    #[test]
    fn add_fallback_regex_target_not_in_chain_skips() {
        let config = r#"
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[providers.deepseek-2]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY_2"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[providers.deepseek-3]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY_3"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[models."deepseek-v4-flash-lp"]
context_window = 64000
max_output_tokens = 8192
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]
"#;
        let (_temp, path) = write_config(config);
        // Regex spec, target "deepseek-2" not in chain → skip with reason
        // provider = deepseek-3 (the one we'd add), target = deepseek-2 (not in chain)
        let results = add_fallback(
            &path,
            "deepseek-3",
            "deepseek-2",
            &["deepseek-v4-.*:chat".to_string()],
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            FallbackBindingResult::SkippedRegexNoMatch { .. }
        ));
    }

    #[test]
    fn add_fallback_different_products_rejected() {
        let config = r#"
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[providers.kimi]
product = "kimi"
api_key_env = "KIMI_API_KEY"
openai_chat = { url = "https://api.kimi.com/v1/chat/completions" }

[models."deepseek-v4-flash-lp"]
context_window = 64000
max_output_tokens = 8192
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]
"#;
        let (_temp, path) = write_config(config);
        let err = add_fallback(
            &path,
            "kimi",
            "deepseek",
            &["deepseek-v4-flash-lp:chat".to_string()],
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("same product"));
    }

    #[test]
    fn add_fallback_custom_product_rejected() {
        let config = r#"
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[providers.custom-provider]
api_key_env = "CUSTOM_KEY"
openai_chat = { url = "http://localhost:11434/v1/chat/completions" }

[models."deepseek-v4-flash-lp"]
context_window = 64000
max_output_tokens = 8192
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]
"#;
        let (_temp, path) = write_config(config);
        let err = add_fallback(
            &path,
            "custom-provider",
            "deepseek",
            &["deepseek-v4-flash-lp:chat".to_string()],
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("custom"));
    }

    #[test]
    fn add_fallback_unknown_provider_rejected() {
        let (_temp, path) = write_config(two_provider_config());
        let err = add_fallback(
            &path,
            "nonexistent",
            "deepseek",
            &["deepseek-v4-flash-lp:chat".to_string()],
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("unknown provider"));
    }

    #[test]
    fn add_fallback_unknown_target_rejected() {
        let (_temp, path) = write_config(two_provider_config());
        let err = add_fallback(
            &path,
            "deepseek-2",
            "nonexistent",
            &["deepseek-v4-flash-lp:chat".to_string()],
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("unknown provider"));
    }

    #[test]
    fn add_fallback_inserts_after_target_not_at_end() {
        // Verify insertion position: right after target, not at the end
        let config = r#"
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[providers.deepseek-2]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY_2"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[providers.deepseek-3]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY_3"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[models."deepseek-v4-flash-lp"]
context_window = 64000
max_output_tokens = 8192
openai_chat_providers = [
    { name = "deepseek", model = "deepseek-v4-flash" },
    { name = "deepseek-3", model = "deepseek-v4-flash" },
]
"#;
        let (_temp, path) = write_config(config);
        add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &["deepseek-v4-flash-lp:chat".to_string()],
        )
        .unwrap();

        let cfg = Config::load(&path).unwrap();
        let model = cfg.models.get("deepseek-v4-flash-lp").unwrap();
        // deepseek-2 should be inserted right after deepseek (index 1), before deepseek-3 (index 2)
        assert_eq!(model.openai_chat_providers.len(), 3);
        assert_eq!(model.openai_chat_providers[0].name, "deepseek");
        assert_eq!(model.openai_chat_providers[1].name, "deepseek-2");
        assert_eq!(model.openai_chat_providers[2].name, "deepseek-3");
    }

    #[test]
    fn add_fallback_multiple_bindings_in_one_call() {
        let (_temp, path) = write_config(multi_endpoint_config());
        let results = add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &[
                "deepseek-v4-flash-lp:chat".to_string(),
                "deepseek-v4-pro-lp:responses".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|r| matches!(r, FallbackBindingResult::Inserted { .. }))
        );
    }

    #[test]
    fn add_fallback_mixed_results() {
        let (_temp, path) = write_config(multi_endpoint_config());
        // First insertion for one binding
        add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &["deepseek-v4-flash-lp:chat".to_string()],
        )
        .unwrap();

        // Now do a batch: one already-in-chain, one new, one nonexistent
        let results = add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &[
                "deepseek-v4-flash-lp:chat".to_string(), // already in chain → skip
                "deepseek-v4-pro-lp:responses".to_string(), // new → insert
                "nonexistent:chat".to_string(),          // explicit missing → error
            ],
        )
        .unwrap();

        assert_eq!(results.len(), 3);
        assert!(matches!(
            results[0],
            FallbackBindingResult::SkippedAlreadyInChain { .. }
        ));
        assert!(matches!(results[1], FallbackBindingResult::Inserted { .. }));
        assert!(matches!(results[2], FallbackBindingResult::Failed { .. }));
    }

    #[test]
    fn print_results_format() {
        let results = vec![
            FallbackBindingResult::Inserted {
                model_id: "model-a".to_string(),
                endpoint: "chat".to_string(),
            },
            FallbackBindingResult::SkippedAlreadyInChain {
                model_id: "model-b".to_string(),
                endpoint: "responses".to_string(),
            },
            FallbackBindingResult::Failed {
                model_id: "model-c".to_string(),
                endpoint: "chat".to_string(),
                reason: "target not found".to_string(),
            },
        ];

        let output = format_results(&results, "deepseek-2", "deepseek");
        assert!(output.contains("✓"));
        assert!(output.contains("✗"));
        assert!(output.contains("1 inserted"));
        assert!(output.contains("1 skipped"));
        assert!(output.contains("1 failed"));
        assert!(output.contains("deepseek-2 after deepseek"));
    }

    #[test]
    fn find_matching_combinations_respects_field_name_aliases() {
        // Endpoint regex "openai_chat" should also match the "chat" short name
        let config = r#"
[server]
listen = "127.0.0.1:8989"

[providers.deepseek]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[providers.deepseek-2]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY_2"
openai_chat = { url = "https://api.deepseek.com/v1/chat/completions" }

[models."deepseek-v4-flash-lp"]
context_window = 64000
max_output_tokens = 8192
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]
"#;
        let (_temp, path) = write_config(config);
        // Use "openai_chat" as endpoint pattern (field_name alias)
        let results = add_fallback(
            &path,
            "deepseek-2",
            "deepseek",
            &["deepseek-v4-flash-lp:openai_chat".to_string()],
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], FallbackBindingResult::Inserted { .. }));
    }
}
