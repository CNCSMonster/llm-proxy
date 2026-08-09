use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use toml_edit::DocumentMut;
use url::Url;

use crate::config::{
    AdapterKind, AuthConfig, Config, EndpointConfig, Protocol, ProviderConfig, ResolvedAuth,
};
use crate::{auth, catalog, service};

#[allow(dead_code)]
pub fn add_provider(
    config_path: &Path,
    provider_id: &str,
    api_key_env: Option<String>,
    no_api_key: bool,
    provider_type: Option<String>,
    endpoint_url: Option<String>,
) -> Result<()> {
    tokio::runtime::Runtime::new()
        .context("failed to create runtime for provider add")?
        .block_on(add_provider_with_models(
            config_path,
            provider_id,
            api_key_env,
            no_api_key,
            provider_type,
            endpoint_url,
            None,
            None,
        ))
}

pub use crate::core::CopyProviderResult;

pub fn copy_provider(
    config_path: &Path,
    source_id: &str,
    target_id: &str,
    api_key_env: Option<String>,
    no_api_key: bool,
) -> Result<CopyProviderResult> {
    let mut core = crate::core::CoreState::load(config_path)?;
    let result = core.copy_provider(source_id, target_id, api_key_env, no_api_key)?;

    println!("copied provider: {source_id} -> {target_id}");
    if result.requires_oauth_login {
        println!("OAuth provider copy requires login for new provider: {target_id}");
    }
    Ok(result)
}

pub fn remove_provider(config_path: &Path, provider_id: &str, force: bool) -> Result<()> {
    let mut core = crate::core::CoreState::load(config_path)?;

    // Check references — always reject if referenced (force only skips confirmation)
    let references = core.provider_references(provider_id);
    if !references.is_empty() {
        bail!(
            "provider {provider_id:?} is still referenced by model bindings: {}; remove them with `model provider remove` first",
            references.join(", ")
        );
    }

    let account = crate::auth::oauth_account_for_provider(core.config(), provider_id).ok();

    if !core.config().providers.contains_key(provider_id) {
        bail!("unknown provider {provider_id:?}");
    }

    // Interactive confirmation (stays in I/O layer)
    if !force {
        confirm_remove_provider(provider_id, account.as_deref())?;
    }

    // Delegate to CoreState — no force needed since references are already checked
    core.remove_provider_with_oauth(provider_id, false)?;

    println!("removed provider: {provider_id}");
    Ok(())
}

fn confirm_remove_provider(provider_id: &str, account: Option<&str>) -> Result<()> {
    if std::env::var("LLM_PROXY_NO_INTERACTIVE").is_ok() || !io::stdin().is_terminal() {
        bail!("provider remove requires --force in non-interactive terminals");
    }
    let account = account
        .map(|account| format!(" ({account})"))
        .unwrap_or_default();
    print!(
        "Remove provider \"{provider_id}\"{account}? This will remove provider config and matching OAuth credentials. [y/N] "
    );
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("failed to read confirmation")?;
    if matches!(line.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        bail!("provider removal cancelled");
    }
}

pub async fn reset_usage(config_path: &Path, provider_id: &str, force: bool) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let token = crate::usage::resolve_openai_token(&cfg, &auth::default_state_path(), provider_id)?;
    if !force {
        let usage = crate::usage::query_usage(&token).await?;
        confirm_reset_usage(&usage)?;
    }
    let result = crate::usage::consume_reset(&token).await?;
    println!("usage reset result for {provider_id}: {result:?}");
    Ok(())
}

fn confirm_reset_usage(usage: &crate::usage::UsageStatus) -> Result<()> {
    match usage.reset_confirm_level() {
        crate::usage::ResetConfirmLevel::None => Ok(()),
        crate::usage::ResetConfirmLevel::Confirm => {
            let before = usage
                .reset_credits_available
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let after = usage
                .reset_credits_available
                .map(|n| n.saturating_sub(1).to_string())
                .unwrap_or_else(|| "unknown".to_string());
            confirm_yes_no(&format!(
                "Consume 1 reset credit? ({before} -> {after}) [y/N] "
            ))
        }
        crate::usage::ResetConfirmLevel::ConfirmWarn => {
            let before = usage
                .reset_credits_available
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let after = usage
                .reset_credits_available
                .map(|n| n.saturating_sub(1).to_string())
                .unwrap_or_else(|| "unknown".to_string());
            confirm_yes_no(&format!(
                "Consume 1 reset credit? ({before} -> {after}) [y/N] "
            ))?;
            println!(
                "[WARNING] Usage is low ({}). This reset credit may be wasted.",
                usage_window_summary(usage)
            );
            confirm_exact_yes("Confirm anyway? (yes/N) ")
        }
    }
}

fn confirm_yes_no(prompt: &str) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("confirmation requires an interactive terminal; pass --force to skip confirmation");
    }
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if matches!(line.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        bail!("usage reset cancelled")
    }
}

fn confirm_exact_yes(prompt: &str) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("confirmation requires an interactive terminal; pass --force to skip confirmation");
    }
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if line.trim() == "yes" {
        Ok(())
    } else {
        bail!("usage reset cancelled")
    }
}

fn usage_window_summary(usage: &crate::usage::UsageStatus) -> String {
    let Some(rate) = &usage.rate_limit else {
        return "usage unknown".to_string();
    };
    let mut parts = Vec::new();
    if let Some(window) = &rate.primary_window {
        parts.push(format!("primary: {}%", window.used_percent));
    }
    if let Some(window) = &rate.secondary_window {
        parts.push(format!("secondary: {}%", window.used_percent));
    }
    if parts.is_empty() {
        "usage unknown".to_string()
    } else {
        parts.join(", ")
    }
}

/// Add a provider from the catalog (or custom) and bind default model templates.
///
/// `provider_id` — catalog product ID used for catalog lookup (e.g. `"deepseek"`).
/// `provider_name` — key written into config `[providers.<name>]` (e.g. `"deepseek-2"`).
///   When `None`, falls back to `provider_id` (normal CLI `provider add` flow).
#[allow(clippy::too_many_arguments)]
pub async fn add_provider_with_models(
    config_path: &Path,
    provider_id: &str,
    api_key_env: Option<String>,
    no_api_key: bool,
    provider_type: Option<String>,
    endpoint_url: Option<String>,
    selected_models: Option<&[String]>,
    provider_name: Option<&str>,
) -> Result<()> {
    let provider_name = provider_name.unwrap_or(provider_id);
    if api_key_env.is_some() && no_api_key {
        bail!("--api-key-env and --no-api-key are mutually exclusive");
    }
    let is_custom = endpoint_url.is_some() || provider_type.is_some();
    if endpoint_url.is_some() != provider_type.is_some() {
        bail!("custom providers require both --type and --endpoint-url");
    }
    let config_existed = config_path.exists();
    let mut cfg = if config_existed {
        Config::load(config_path)?
    } else {
        crate::config::default_deepseek_config()
    };
    let existing_models = cfg
        .models
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();

    let provider = if let (Some(protocol), Some(endpoint_url)) = (provider_type, endpoint_url) {
        build_custom_provider(&protocol, &endpoint_url, api_key_env, no_api_key)?
    } else {
        let mut entry = catalog::built_in_providers()
            .into_iter()
            .find(|entry| entry.id == provider_id)
            .with_context(|| {
                format!(
                    "unknown provider {provider_id:?}; use `llm-proxy provider list` to see configured providers, or provide --type and --endpoint-url for a custom provider"
                )
            })?;
        if no_api_key {
            entry.provider.api_key_env = None;
        } else if let Some(env) = api_key_env {
            entry.provider.api_key_env = Some(env);
        }
        entry.provider
    };

    cfg.providers.insert(provider_name.to_string(), provider);
    catalog::apply_catalog_model_defaults_with_name(
        &mut cfg,
        provider_id,
        provider_name,
        selected_models,
    )?;
    let inserted_models = cfg
        .models
        .keys()
        .filter(|model_id| !existing_models.contains(*model_id))
        .cloned()
        .collect::<Vec<_>>();
    cfg.validate()?;
    if !is_custom && let Some(models) = selected_models {
        ensure_provider_auth_ready(&cfg, provider_name)?;
        verify_selected_models(&cfg, provider_name, models).await?;
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    write_config_after_connect(
        config_path,
        &cfg,
        provider_name,
        &inserted_models,
        config_existed,
    )?;
    maybe_inject_provider_env(&cfg, provider_name);
    println!("connected provider: {provider_name}");
    println!("config: {}", config_path.display());
    Ok(())
}

fn ensure_provider_auth_ready(cfg: &Config, provider_id: &str) -> Result<()> {
    let provider = cfg
        .providers
        .get(provider_id)
        .with_context(|| format!("unknown provider {provider_id:?}"))?;
    match provider.auth_config(provider_id)? {
        AuthConfig::ApiKeyEnv { env } => {
            if std::env::var(&env)
                .ok()
                .filter(|value| !value.is_empty())
                .is_none()
            {
                bail!("missing API key environment variable {env} for provider {provider_id:?}");
            }
        }
        AuthConfig::OpenaiOauth { account } => {
            let account = account.unwrap_or_else(|| provider_id.to_string());
            auth::get_openai_token(&auth::default_state_path(), &account, provider_id)
                .with_context(|| format!("provider {provider_id:?} requires `llm-proxy provider login {provider_id}` before connect/add can verify models"))?;
        }
        AuthConfig::AntigravityOauth { account } => {
            let account = account.unwrap_or_else(|| provider_id.to_string());
            auth::get_antigravity_token(&auth::default_state_path(), &account, provider_id)
                .with_context(|| format!("provider {provider_id:?} requires `llm-proxy provider login {provider_id}` before connect/add can verify models"))?;
        }
        AuthConfig::None => {}
    }
    Ok(())
}

async fn verify_selected_models(
    cfg: &Config,
    provider_id: &str,
    selected_models: &[String],
) -> Result<()> {
    if selected_models.is_empty() {
        bail!("mature provider setup requires at least one selected model to verify");
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to create connectivity probe HTTP client")?;

    for model_id in selected_models {
        let (_frontend_id, plan) = cfg
            .resolve_model_request(Protocol::OpenaiChatCompletions, model_id)
            .or_else(|| cfg.resolve_model_request(Protocol::OpenaiResponses, model_id))
            .or_else(|| cfg.resolve_model_request(Protocol::Anthropic, model_id))
            .with_context(|| {
                format!("selected model {model_id:?} has no configured binding to verify")
            })?;
        if plan.provider_id != provider_id {
            bail!(
                "selected model {model_id:?} resolved to provider {:?}, expected {:?}",
                plan.provider_id,
                provider_id
            );
        }
        run_probe(&client, model_id, &plan).await?;
    }

    Ok(())
}

async fn run_probe(
    client: &reqwest::Client,
    model_id: &str,
    plan: &crate::config::ExecutionPlan,
) -> Result<()> {
    if plan.adapter != AdapterKind::Passthrough {
        // Probe the native upstream protocol directly. The goal is credential/model reachability,
        // while adapter correctness is covered by adapter/proxy tests and normal runtime paths.
    }

    let resolved_auth = match &plan.auth {
        AuthConfig::ApiKeyEnv { env } => {
            let key = std::env::var(env)
                .ok()
                .filter(|value| !value.is_empty())
                .with_context(|| {
                    format!("missing API key environment variable {env} for model {model_id}")
                })?;
            ResolvedAuth {
                token: Some(key),
                project_id: None,
            }
        }
        AuthConfig::OpenaiOauth { account } => {
            let account_id = account.as_deref().unwrap_or(&plan.provider_id);
            let token =
                auth::get_openai_token(&auth::default_state_path(), account_id, &plan.provider_id)?;
            ResolvedAuth {
                token: Some(token),
                project_id: None,
            }
        }
        AuthConfig::AntigravityOauth { account } => {
            let account_id = account.as_deref().unwrap_or(&plan.provider_id);
            let token = auth::get_antigravity_token(
                &auth::default_state_path(),
                account_id,
                &plan.provider_id,
            )?;
            // 从 accounts 中获取 project_id
            let (accounts, _skipped) = auth::load_oauth_accounts(&auth::default_state_path())?;
            let project_id = accounts
                .antigravity
                .get(account_id)
                .map(|a| Some(a.project_id.clone()))
                .unwrap_or(None);
            ResolvedAuth {
                token: Some(token),
                project_id,
            }
        }
        AuthConfig::None => ResolvedAuth {
            token: None,
            project_id: None,
        },
    };

    let body = crate::probe::probe_body_with_auth(plan, Some(&resolved_auth))?;
    let request =
        crate::probe::apply_protocol_headers(plan, client.post(&plan.native_url)).json(&body);
    let request = crate::probe::apply_auth_header(plan, request, resolved_auth.token);

    let response = request
        .send()
        .await
        .with_context(|| format!("connectivity probe failed for model {model_id}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!(
            "connectivity probe failed for model {model_id}: upstream returned {status}: {}",
            text.chars().take(500).collect::<String>()
        );
    }
    Ok(())
}

fn maybe_inject_provider_env(cfg: &Config, provider_id: &str) {
    let Some(provider) = cfg.providers.get(provider_id) else {
        return;
    };
    let Ok(crate::config::AuthConfig::ApiKeyEnv { env }) = provider.auth_config(provider_id) else {
        return;
    };
    let Some(value) = std::env::var(&env).ok().filter(|value| !value.is_empty()) else {
        return;
    };
    let mut vars = BTreeMap::new();
    vars.insert(env.clone(), value);
    if let Ok(applied) = service::inject_env(&vars)
        && !applied.is_empty()
    {
        println!(
            "updated running service environment: {}",
            applied.join(", ")
        );
    }
}

fn write_config_after_connect(
    config_path: &Path,
    cfg: &Config,
    provider_id: &str,
    inserted_models: &[String],
    config_existed: bool,
) -> Result<()> {
    let serialized = toml::to_string_pretty(cfg).context("failed to serialize config")?;
    if !config_existed {
        crate::config_edit::atomic_write(config_path, serialized.as_bytes())?;
        return Ok(());
    }

    let original = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let source = serialized
        .parse::<DocumentMut>()
        .context("failed to parse serialized config for round-trip update")?;

    doc["providers"][provider_id] = source["providers"][provider_id].clone();
    for model_id in inserted_models {
        doc["models"][model_id] = source["models"][model_id].clone();
    }

    let updated = doc.to_string();
    let roundtrip_cfg: Config =
        toml::from_str(&updated).context("round-trip connect update produced invalid TOML")?;
    roundtrip_cfg
        .validate()
        .context("round-trip connect update produced invalid config")?;
    crate::config_edit::atomic_write(config_path, updated.as_bytes())
}

/// Custom native provider: `--type PROTOCOL --endpoint-url URL` creates one
/// native endpoint whose `url` is stored as-is (design §12.1).
fn build_custom_provider(
    protocol_text: &str,
    endpoint_url: &str,
    api_key_env: Option<String>,
    no_api_key: bool,
) -> Result<ProviderConfig> {
    let protocol = parse_protocol(protocol_text)?;
    validate_endpoint_url(endpoint_url)?;
    let auth = if no_api_key { None } else { api_key_env };
    let mut provider = ProviderConfig {
        api_key_env: auth,
        ..ProviderConfig::default()
    };
    provider.set_endpoint(protocol, EndpointConfig::native(endpoint_url));
    Ok(provider)
}

fn parse_protocol(value: &str) -> Result<Protocol> {
    match value {
        "openai-chat" | "openai-chat-completions" | "chat" | "chat-completions" => {
            Ok(Protocol::OpenaiChatCompletions)
        }
        "openai-responses" | "responses" => Ok(Protocol::OpenaiResponses),
        "anthropic" | "anthropic-messages" => Ok(Protocol::Anthropic),
        _ => bail!(
            "unsupported provider --type {value:?}; expected openai-chat, openai-responses, or anthropic"
        ),
    }
}

fn validate_endpoint_url(endpoint_url: &str) -> Result<()> {
    let url = Url::from_str(endpoint_url)
        .with_context(|| format!("invalid endpoint URL {endpoint_url:?}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        bail!("endpoint URL must use http or https");
    }
    let path = url.path();
    if path == "/" || path.is_empty() {
        bail!("endpoint URL must include a concrete endpoint path");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("endpoint URL must not include query or fragment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use std::fs;

    use super::*;

    #[test]
    fn add_provider_writes_catalog_provider_to_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        add_provider(
            &path,
            "openai-payg",
            Some("CUSTOM_OPENAI_KEY".to_string()),
            false,
            None,
            None,
        )
        .expect("add provider");

        let cfg = Config::load(&path).expect("load");
        let provider = cfg.providers.get("openai-payg").expect("provider");
        assert_eq!(provider.api_key_env.as_deref(), Some("CUSTOM_OPENAI_KEY"));
        assert!(
            provider
                .endpoint(crate::config::Protocol::OpenaiResponses)
                .is_some()
        );
    }

    #[test]
    fn add_openai_payg_installs_gpt_model_bindings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        add_provider(
            &path,
            "openai-payg",
            Some("CUSTOM_OPENAI_KEY".to_string()),
            false,
            None,
            None,
        )
        .expect("add openai");

        let cfg = Config::load(&path).expect("load");
        let model = cfg.models.get("gpt-5.5-lp").expect("gpt model");
        assert_eq!(model.openai_chat_providers[0].name, "openai-payg");
        assert_eq!(model.openai_responses_providers[0].name, "openai-payg");
        assert_eq!(model.anthropic_providers[0].name, "openai-payg");
        assert!(
            cfg.resolve_model_request(Protocol::OpenaiResponses, "gpt-5.5-lp")
                .is_some()
        );
        assert!(
            cfg.resolve_model_request(Protocol::Anthropic, "gpt-5.5-lp")
                .is_some()
        );
    }

    #[test]
    fn add_ollama_installs_qwen_model_bindings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        add_provider(&path, "ollama", None, true, None, None).expect("add ollama");

        let cfg = Config::load(&path).expect("load");
        let provider = cfg.providers.get("ollama").expect("provider");
        assert_eq!(provider.api_key_env, None);
        let model = cfg.models.get("qwen3-27b-lp").expect("qwen model");
        assert_eq!(model.openai_chat_providers[0].name, "ollama");
        assert_eq!(model.openai_responses_providers[0].name, "ollama");
        assert_eq!(model.anthropic_providers[0].name, "ollama");
        assert_eq!(model.openai_chat_providers[0].model, "qwen3:27b");
        assert!(
            cfg.resolve_model_request(Protocol::Anthropic, "qwen3-27b-lp")
                .is_some()
        );
    }

    #[test]
    fn add_anthropic_installs_claude_anthropic_binding() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        add_provider(
            &path,
            "anthropic",
            Some("CUSTOM_ANTHROPIC_KEY".to_string()),
            false,
            None,
            None,
        )
        .expect("add anthropic");

        let cfg = Config::load(&path).expect("load");
        let provider = cfg.providers.get("anthropic").expect("provider");
        assert_eq!(
            provider.api_key_env.as_deref(),
            Some("CUSTOM_ANTHROPIC_KEY")
        );
        let model = cfg.models.get("claude-sonnet-lp").expect("claude model");
        assert_eq!(model.openai_chat_providers[0].name, "anthropic");
        assert_eq!(model.openai_responses_providers[0].name, "anthropic");
        assert_eq!(model.anthropic_providers[0].name, "anthropic");
        assert_eq!(model.anthropic_providers[0].model, "claude-sonnet-5");
        assert!(
            cfg.resolve_model_request(Protocol::OpenaiChatCompletions, "claude-sonnet-lp")
                .is_some()
        );
        assert!(
            cfg.resolve_model_request(Protocol::OpenaiResponses, "claude-sonnet-lp")
                .is_some()
        );
        assert!(
            cfg.resolve_model_request(Protocol::Anthropic, "claude-sonnet-lp")
                .is_some()
        );
    }

    #[test]
    fn add_expanded_catalog_provider_installs_default_model_bindings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        add_provider(
            &path,
            "kimi-platform-global",
            Some("MOONSHOT_API_KEY".to_string()),
            false,
            None,
            None,
        )
        .expect("add kimi");

        let cfg = Config::load(&path).expect("load");
        let provider = cfg.providers.get("kimi-platform-global").expect("provider");
        assert_eq!(
            provider.openai_chat.as_ref().and_then(|e| e.url.as_deref()),
            Some("https://api.moonshot.ai/v1/chat/completions")
        );
        let model = cfg.models.get("kimi-k3-lp").expect("kimi model");
        assert_eq!(model.openai_chat_providers[0].name, "kimi-platform-global");
        assert_eq!(
            model.openai_responses_providers[0].name,
            "kimi-platform-global"
        );
        assert_eq!(model.anthropic_providers[0].name, "kimi-platform-global");
        assert!(
            cfg.resolve_model_request(Protocol::Anthropic, "kimi-k3-lp")
                .is_some()
        );
    }

    #[test]
    fn add_provider_preserves_unmanaged_toml_comments() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        fs::write(
            &path,
            r#"# keep root comment
[server]
listen = "127.0.0.1:8989"

# keep provider comment
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.openai_chat]
url = "https://api.deepseek.com/chat/completions"

[providers.deepseek.anthropic]
derive_from = "openai_chat"

# keep model comment
[models.deepseek-v4-flash-lp]
context_window = 1000000
max_output_tokens = 384000
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]
anthropic_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]
"#,
        )
        .expect("write");

        add_provider(
            &path,
            "openai-payg",
            Some("OPENAI_API_KEY".to_string()),
            false,
            None,
            None,
        )
        .expect("add provider");

        let text = fs::read_to_string(&path).expect("read");
        assert!(text.contains("# keep root comment"));
        assert!(text.contains("# keep provider comment"));
        assert!(text.contains("# keep model comment"));
        assert!(text.contains("[providers.openai-payg]"));
        assert!(text.contains("gpt-5.5-lp"));
        Config::load(&path).expect("valid config after round-trip update");
    }

    #[test]
    fn add_custom_provider_stores_full_endpoint_url_as_is() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        add_provider(
            &path,
            "local-custom",
            Some("LOCAL_CUSTOM_KEY".to_string()),
            false,
            Some("openai-chat".to_string()),
            Some("http://127.0.0.1:11434/v1/chat/completions".to_string()),
        )
        .expect("add custom provider");

        let cfg = Config::load(&path).expect("load");
        let provider = cfg.providers.get("local-custom").expect("provider");
        assert_eq!(provider.api_key_env.as_deref(), Some("LOCAL_CUSTOM_KEY"));
        let endpoint = provider.openai_chat.as_ref().expect("chat endpoint");
        assert_eq!(
            endpoint.url.as_deref(),
            Some("http://127.0.0.1:11434/v1/chat/completions")
        );
        assert!(endpoint.derive_from.is_none());
    }

    #[test]
    fn provider_copy_and_remove_preserve_unmanaged_comments() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"# root comment
[server]
listen = "127.0.0.1:8989"

# source provider comment
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.openai_chat]
url = "https://api.deepseek.com/chat/completions"

# model comment
[models.model-a]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }]
"#,
        )
        .expect("write");

        copy_provider(
            &path,
            "deepseek",
            "deepseek-copy",
            Some("DEEPSEEK_COPY_KEY".to_string()),
            false,
        )
        .expect("copy");
        remove_provider(&path, "deepseek-copy", true).expect("remove");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# root comment"));
        assert!(text.contains("# source provider comment"));
        assert!(text.contains("# model comment"));
        assert!(!text.contains("[providers.deepseek-copy]"));
    }

    #[test]
    #[serial]
    fn provider_remove_without_force_requires_interactive_confirmation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");
        add_provider(
            &path,
            "local-custom",
            None,
            true,
            Some("openai-chat".to_string()),
            Some("http://127.0.0.1:11434/v1/chat/completions".to_string()),
        )
        .expect("add custom");

        // Set env var to simulate non-interactive terminal
        // SAFETY: test runs single-threaded, env var is only used by this test
        unsafe {
            std::env::set_var("LLM_PROXY_NO_INTERACTIVE", "1");
        }
        let err = remove_provider(&path, "local-custom", false).expect_err("needs confirmation");
        unsafe {
            std::env::remove_var("LLM_PROXY_NO_INTERACTIVE");
        }
        assert!(
            err.to_string().contains("requires --force") || err.to_string().contains("cancelled")
        );
    }

    #[test]
    #[serial]
    fn provider_remove_rejects_referenced_provider_and_force_removes_unref() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        let err = remove_provider(&path, "deepseek", true).expect_err("referenced");
        assert!(err.to_string().contains("still referenced"));

        add_provider(
            &path,
            "local-custom",
            None,
            true,
            Some("openai-chat".to_string()),
            Some("http://127.0.0.1:11434/v1/chat/completions".to_string()),
        )
        .expect("add custom");
        remove_provider(&path, "local-custom", true).expect("remove unreferenced");
        let cfg = Config::load(&path).expect("load");
        assert!(!cfg.providers.contains_key("local-custom"));
    }

    #[test]
    fn provider_copy_oauth_uses_target_account_and_requests_relogin() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"[server]
listen = "127.0.0.1:8989"

[providers.openai-subscription]
auth = { type = "openai_oauth", account = "openai-subscription" }

[providers.openai-subscription.openai_responses]
url = "https://chatgpt.com/backend-api/codex/responses"
"#,
        )
        .expect("write");

        let result = copy_provider(
            &path,
            "openai-subscription",
            "openai-subscription-2",
            None,
            false,
        )
        .expect("copy oauth");

        assert!(result.requires_oauth_login);
        let cfg = Config::load(&path).expect("load");
        assert_eq!(
            cfg.providers["openai-subscription-2"].auth,
            Some(crate::config::AuthConfig::OpenaiOauth {
                account: Some("openai-subscription-2".to_string())
            })
        );
    }

    #[test]
    fn provider_copy_requires_api_key_reconfirmation_and_copies_no_bindings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        let err = copy_provider(&path, "deepseek", "deepseek-copy", None, false)
            .expect_err("requires env");
        assert!(err.to_string().contains("requires --api-key-env"));

        copy_provider(
            &path,
            "deepseek",
            "deepseek-copy",
            Some("DEEPSEEK_COPY_KEY".to_string()),
            false,
        )
        .expect("copy");
        let cfg = Config::load(&path).expect("load");
        assert_eq!(
            cfg.providers["deepseek-copy"].api_key_env.as_deref(),
            Some("DEEPSEEK_COPY_KEY")
        );
        assert!(cfg.models.values().all(|model| {
            Protocol::CLIENT_PROTOCOLS.into_iter().all(|protocol| {
                model
                    .provider_bindings(protocol)
                    .iter()
                    .all(|binding| binding.name != "deepseek-copy")
            })
        }));
    }

    #[tokio::test]
    async fn mature_provider_with_selected_model_verifies_before_writing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");
        let before = fs::read_to_string(&path).expect("read before");
        let missing_env = "LLM_PROXY_TEST_MISSING_KEY_DO_NOT_SET";

        let err = add_provider_with_models(
            &path,
            "openai-payg",
            Some(missing_env.to_string()),
            false,
            None,
            None,
            Some(&["gpt-5.5-lp".to_string()]),
            None,
        )
        .await
        .expect_err("missing env should fail before write");

        assert!(err.to_string().contains(missing_env));
        let after = fs::read_to_string(&path).expect("read after");
        assert_eq!(after, before);
    }

    #[test]
    fn custom_provider_requires_endpoint_path() {
        let err = build_custom_provider("openai-chat", "https://example.com", None, true)
            .expect_err("path required");
        assert!(err.to_string().contains("concrete endpoint path"));
    }

    // =========================================================================
    // Phase 1 P0: parse_protocol + validate_endpoint_url pure function tests
    // =========================================================================

    #[test]
    fn test_parse_protocol_openai_chat_variants() {
        // All openai-chat aliases should resolve to OpenaiChatCompletions
        for alias in &[
            "openai-chat",
            "openai-chat-completions",
            "chat",
            "chat-completions",
        ] {
            let result = parse_protocol(alias).unwrap_or_else(|_| panic!("should parse {alias}"));
            assert_eq!(result, Protocol::OpenaiChatCompletions);
        }
        // Also verify openai-responses and anthropic aliases
        assert_eq!(
            parse_protocol("openai-responses").unwrap(),
            Protocol::OpenaiResponses
        );
        assert_eq!(
            parse_protocol("responses").unwrap(),
            Protocol::OpenaiResponses
        );
        assert_eq!(parse_protocol("anthropic").unwrap(), Protocol::Anthropic);
        assert_eq!(
            parse_protocol("anthropic-messages").unwrap(),
            Protocol::Anthropic
        );
    }

    #[test]
    fn test_parse_protocol_unsupported_returns_error() {
        for bad in &["grpc", "", "OPENAI-CHAT", "Chat", "openai_chat"] {
            let err = parse_protocol(bad).expect_err(&format!("should reject {bad:?}"));
            assert!(
                err.to_string().contains("unsupported provider --type"),
                "error for {bad:?} should mention unsupported: {err}"
            );
        }
    }

    #[test]
    fn test_validate_endpoint_url_valid_https() {
        // Valid HTTPS URL with path should succeed
        assert!(validate_endpoint_url("https://api.example.com/v1/chat/completions").is_ok());
        // Valid HTTP URL (for local dev) should also succeed
        assert!(validate_endpoint_url("http://127.0.0.1:11434/v1/chat/completions").is_ok());
    }

    #[test]
    fn test_validate_endpoint_url_no_path_rejected() {
        // URL without a concrete path should be rejected
        for bad in &["https://example.com", "https://example.com/"] {
            let err = validate_endpoint_url(bad).expect_err(&format!("should reject {bad:?}"));
            assert!(
                err.to_string().contains("concrete endpoint path"),
                "error for {bad:?} should mention path: {err}"
            );
        }
        // Non-HTTP scheme should also be rejected
        let err = validate_endpoint_url("ftp://example.com/path").expect_err("ftp rejected");
        assert!(err.to_string().contains("http or https"));
        // Query/fragment should be rejected
        let err =
            validate_endpoint_url("https://example.com/v1?key=abc").expect_err("query rejected");
        assert!(err.to_string().contains("query or fragment"));
    }

    // =========================================================================
    // Phase 1 P0: usage_window_summary + confirm_yes_no tests (from §1.5)
    // =========================================================================

    #[test]
    fn test_usage_window_summary_active_window() {
        let usage = crate::usage::UsageStatus {
            plan_type: "plus".to_string(),
            rate_limit: Some(crate::usage::RateLimitInfo {
                allowed: Some(true),
                limit_reached: false,
                primary_window: Some(crate::usage::WindowSnapshot {
                    used_percent: 75,
                    reset_after_seconds: Some(3600),
                }),
                secondary_window: Some(crate::usage::WindowSnapshot {
                    used_percent: 40,
                    reset_after_seconds: Some(86400),
                }),
            }),
            reset_credits_available: Some(5),
        };
        let summary = usage_window_summary(&usage);
        assert!(summary.contains("primary: 75%"), "got: {summary}");
        assert!(summary.contains("secondary: 40%"), "got: {summary}");
    }

    #[test]
    fn test_usage_window_summary_expired_window() {
        // No rate limit info → "usage unknown"
        let usage = crate::usage::UsageStatus {
            plan_type: "unknown".to_string(),
            rate_limit: None,
            reset_credits_available: None,
        };
        assert_eq!(usage_window_summary(&usage), "usage unknown");

        // Rate limit exists but both windows are None → "usage unknown"
        let usage = crate::usage::UsageStatus {
            plan_type: "free".to_string(),
            rate_limit: Some(crate::usage::RateLimitInfo {
                allowed: Some(false),
                limit_reached: true,
                primary_window: None,
                secondary_window: None,
            }),
            reset_credits_available: None,
        };
        assert_eq!(usage_window_summary(&usage), "usage unknown");
    }

    #[test]
    fn test_confirm_yes_no_yes_input() {
        // In test environment, stdin/stdout are not terminals,
        // so confirm_yes_no should bail with "interactive terminal" message
        let result = confirm_yes_no("Confirm? [y/N] ");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("interactive terminal"),
            "should reject non-terminal input"
        );
    }
}
