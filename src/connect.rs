use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::str::FromStr;

pub mod openrouter;

use anyhow::{Context, Result, bail};
use toml_edit::DocumentMut;
use url::Url;

use crate::config::{
    AdapterKind, AuthConfig, Config, EndpointConfig, Protocol, ProviderConfig, ResolvedAuth,
};
use crate::{auth, catalog, service};

/// 探针进度——通过 `tokio::sync::watch` 从 `verify_selected_models` 上抛给 TUI。
#[derive(Debug, Clone)]
pub struct ProbeProgress {
    #[allow(dead_code)]
    pub done: usize,
    #[allow(dead_code)]
    pub total: usize,
    #[allow(dead_code)]
    pub current_model: String,
}

/// TUI 探针进度发送端（watch channel sender）。
pub type ProbeProgressSender = tokio::sync::watch::Sender<ProbeProgress>;

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
            false,
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

    // Check references — reject when referenced unless force (which
    // removes the provider together with its model bindings, matching Admin API)
    let references = core.provider_references(provider_id);
    if !references.is_empty() && !force {
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

    // Delegate to CoreState with the same force semantics (removes model
    // bindings together with the provider when forced)
    core.remove_provider_with_oauth(provider_id, force)?;

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
    let token = crate::usage::resolve_openai_token(
        &cfg,
        &crate::auth::state_path_for_config(config_path),
        provider_id,
    )?;
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

fn ensure_interactive_terminal() -> Result<()> {
    ensure_interactive_terminal_state(io::stdin().is_terminal(), io::stdout().is_terminal())
}

fn ensure_interactive_terminal_state(
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
) -> Result<()> {
    if !stdin_is_terminal || !stdout_is_terminal {
        bail!("confirmation requires an interactive terminal; pass --force to skip confirmation");
    }
    Ok(())
}

fn confirm_yes_no(prompt: &str) -> Result<()> {
    ensure_interactive_terminal()?;
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
    ensure_interactive_terminal()?;
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

fn affected_catalog_models(provider_id: &str, selected_models: Option<&[String]>) -> Vec<String> {
    if let Some(selected_models) = selected_models {
        return selected_models.to_vec();
    }
    catalog::built_in_providers()
        .into_iter()
        .find(|entry| entry.id == provider_id)
        .and_then(|entry| {
            entry
                .model_templates
                .first()
                .map(|template| template.frontend_id.to_string())
        })
        .into_iter()
        .collect()
}

fn use_distinct_oauth_account(
    provider: &mut ProviderConfig,
    provider_id: &str,
    provider_name: &str,
) {
    if provider_name == provider_id {
        return;
    }
    if let Some(AuthConfig::OpenaiOauth { account }) = provider.auth.as_mut() {
        *account = Some(provider_name.to_string());
    }
}

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAddErrorKind {
    ProbeFailed,
    AuthNotReady,
    ConfigConflict,
    InvalidConfig,
    InternalError,
}

impl ProviderAddErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ProbeFailed => "probe_failed",
            Self::AuthNotReady => "auth_not_ready",
            Self::ConfigConflict => "config_conflict",
            Self::InvalidConfig => "invalid_config",
            Self::InternalError => "internal_error",
        }
    }

    pub fn status_code(&self) -> axum::http::StatusCode {
        match self {
            Self::ProbeFailed | Self::AuthNotReady => axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            Self::ConfigConflict => axum::http::StatusCode::CONFLICT,
            Self::InvalidConfig => axum::http::StatusCode::BAD_REQUEST,
            Self::InternalError => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAddError {
    pub kind: ProviderAddErrorKind,
    pub message: String,
}

impl std::fmt::Display for ProviderAddError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProviderAddError {}

impl ProviderAddError {
    pub fn new(kind: ProviderAddErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn probe_failed(message: impl Into<String>) -> Self {
        Self::new(ProviderAddErrorKind::ProbeFailed, message)
    }

    pub fn auth_not_ready(message: impl Into<String>) -> Self {
        Self::new(ProviderAddErrorKind::AuthNotReady, message)
    }

    #[allow(dead_code)]
    pub fn config_conflict(message: impl Into<String>) -> Self {
        Self::new(ProviderAddErrorKind::ConfigConflict, message)
    }

    pub fn invalid_config(message: impl Into<String>) -> Self {
        Self::new(ProviderAddErrorKind::InvalidConfig, message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(ProviderAddErrorKind::InternalError, message)
    }
}

/// Build and verify a provider candidate without reading or writing config.toml.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_provider_candidate(
    config_path: &Path,
    base: &Config,
    provider_id: &str,
    provider_name: Option<&str>,
    api_key_env: Option<String>,
    no_api_key: bool,
    provider_type: Option<String>,
    endpoint_url: Option<String>,
    selected_models: Option<&[String]>,
    skip_probe: bool,
    require_auth_env: bool,
    pending_auth: Option<&ResolvedAuth>,
    progress_tx: Option<&ProbeProgressSender>,
) -> std::result::Result<(Config, Vec<String>), ProviderAddError> {
    if api_key_env.is_some() && no_api_key {
        return Err(ProviderAddError::invalid_config(
            "--api-key-env and --no-api-key are mutually exclusive",
        ));
    }
    let is_custom = endpoint_url.is_some() || provider_type.is_some();
    if endpoint_url.is_some() != provider_type.is_some() {
        return Err(ProviderAddError::invalid_config(
            "custom providers require both --type and --endpoint-url",
        ));
    }
    let provider_name = provider_name.unwrap_or(provider_id);
    let mut cfg = base.clone();
    let is_openrouter = provider_id == "openrouter";
    // 不得硬编码 `let skip_probe = skip_probe || is_openrouter;` 覆盖探测开关：
    // 该写法会 **shadow 掉入参**，使探测确认屏对 OpenRouter 完全失效——
    // 无论用户在屏上选「探测」还是「跳过」，实际永远零请求。
    // 是否探测完全由调用方决定（TUI 确认屏 / CLI 参数）；
    // `is_openrouter` 仍保留给下方动态目录读取分支使用。
    let mut affected_models = affected_catalog_models(provider_id, selected_models);
    let mut provider = if let (Some(protocol), Some(endpoint_url)) = (provider_type, endpoint_url) {
        build_custom_provider(&protocol, &endpoint_url, api_key_env, no_api_key)
            .map_err(|e| ProviderAddError::invalid_config(e.to_string()))?
    } else {
        let mut entry = catalog::built_in_providers()
            .into_iter()
            .find(|entry| entry.id == provider_id)
            .ok_or_else(|| {
                ProviderAddError::invalid_config(format!("unknown provider {provider_id:?}"))
            })?;
        if no_api_key {
            entry.provider.api_key_env = None;
        } else if let Some(env) = api_key_env {
            entry.provider.api_key_env = Some(env);
        }
        entry.provider
    };
    use_distinct_oauth_account(&mut provider, provider_id, provider_name);
    cfg.providers.insert(provider_name.to_string(), provider);
    catalog::apply_catalog_model_defaults_with_name(
        &mut cfg,
        provider_id,
        provider_name,
        selected_models,
    )
    .map_err(|e| ProviderAddError::invalid_config(e.to_string()))?;

    if is_openrouter && let Some(selected) = selected_models {
        let config_dir = config_path.parent().unwrap_or(Path::new("."));
        let state_dir = crate::service::StateDirs::resolve(config_dir).state_dir;
        let now = chrono::Utc::now().timestamp();
        let cache_opt = openrouter::read_cache_file(&state_dir, now)
            .or_else(|| openrouter::read_cache_file(&crate::service::state_dir(), now));
        let cached_models = cache_opt.map(|c| c.models).unwrap_or_default();

        let mut raw_models = Vec::new();
        for id in selected {
            if let Some(m) = cached_models.iter().find(|m| &m.id == id) {
                raw_models.push(m.clone());
            } else {
                raw_models.push(crate::connect::openrouter::OpenRouterRawModel {
                    id: id.clone(),
                    name: Some(id.clone()),
                    context_length: Some(128_000),
                    top_provider: None,
                    architecture: None,
                    supported_parameters: vec!["tools".to_string()],
                    reasoning: None,
                });
            }
        }
        openrouter::apply_dynamic_models_to_config(&mut cfg, &raw_models)
            .map_err(|e| ProviderAddError::invalid_config(e.to_string()))?;

        let existing_bindings = openrouter::build_openrouter_existing_bindings(&cfg.models);
        let mut allocated_keys: std::collections::HashSet<String> =
            cfg.models.keys().cloned().collect();
        let mut resolved = Vec::new();
        for model_id in selected {
            let fid = openrouter::resolve_openrouter_frontend_id(
                model_id,
                &existing_bindings,
                &mut allocated_keys,
            );
            resolved.push(fid);
        }
        affected_models = resolved;
    }
    cfg.validate()
        .map_err(|e| ProviderAddError::invalid_config(e.to_string()))?;
    // 方案 A（ 探测确认）：两道检查**正交**——
    //  - `require_auth_env`：**本地** env var / OAuth 账号存在性，失败必须显式报给用户；
    //  - `skip_probe`：只控制**网络**有效性探测（花钱部分），由探测确认屏决定。
    // 跳过探测 ≠ 跳过本地凭据检查；显式 deferred（先写配置、凭据后补）用 require_auth_env=false。
    if require_auth_env && !is_custom && selected_models.is_some() {
        ensure_provider_auth_ready(config_path, &cfg, provider_name, pending_auth)
            .map_err(|e| ProviderAddError::auth_not_ready(e.to_string()))?;
    }
    // 探测必须使用**解析后的 frontend id**（`affected_models`），不能用原始入参：
    // OpenRouter 的入参是 upstream id（如 `anthropic/claude-sonnet-4`），而配置里
    // 落的是 `anthropic-claude-sonnet-4-or-lp`，喂原始 id 会报 "no configured binding"。
    // 其余产品二者等价——`affected_catalog_models` 对 Some 分支原样返回入参。
    // （该缺陷此前被 `skip_probe || is_openrouter` 强制跳过掩盖。）
    if !skip_probe && !is_custom && selected_models.is_some() {
        verify_selected_models(
            config_path,
            &cfg,
            provider_name,
            &affected_models,
            pending_auth,
            progress_tx,
        )
        .await
        .map_err(|e| ProviderAddError::probe_failed(e.to_string()))?;
    }
    Ok((cfg, affected_models))
}

/// Add a provider from the catalog (or custom) and bind default model templates.
///
/// `provider_id` — catalog product ID used for catalog lookup (e.g. `"deepseek"`).
/// `provider_name` — key written into config `[providers.<name>]` (e.g. `"deepseek-2"`).
///   When `None`, falls back to `provider_id` (normal CLI `provider add` flow).
///
/// Public backward-compatible entry point; `skip_probe` defaults to `false`.
/// Callers that need structured error classification or skip_probe should use
/// `add_provider_with_models_inner` directly.
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
    skip_probe: bool,
) -> Result<()> {
    add_provider_with_models_inner(
        config_path,
        provider_id,
        api_key_env,
        no_api_key,
        provider_type,
        endpoint_url,
        selected_models,
        provider_name,
        skip_probe,
        true, // require_auth_env: CLI 入口始终校验本地 env var
        None,
        None,
    )
    .await
    .map_err(Into::into)
}

/// Internal implementation with `skip_probe`, `pending_account` and structured `ProviderAddError`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn add_provider_with_models_inner(
    config_path: &Path,
    provider_id: &str,
    api_key_env: Option<String>,
    no_api_key: bool,
    provider_type: Option<String>,
    endpoint_url: Option<String>,
    selected_models: Option<&[String]>,
    provider_name: Option<&str>,
    skip_probe: bool,
    require_auth_env: bool,
    pending_account: Option<&crate::auth::PendingAccount>,
    progress_tx: Option<&ProbeProgressSender>,
) -> std::result::Result<(), ProviderAddError> {
    let provider_name = provider_name.unwrap_or(provider_id);
    let config_existed = config_path.exists();
    let base = if config_existed {
        Config::load(config_path).map_err(|e| ProviderAddError::invalid_config(e.to_string()))?
    } else {
        crate::config::default_deepseek_config()
    };
    let pending_auth = pending_account.map(|a| a.to_resolved_auth());
    if provider_id == "openrouter" {
        let config_dir = config_path.parent().unwrap_or(Path::new("."));
        let state_dir = crate::service::StateDirs::resolve(config_dir).state_dir;
        let now = chrono::Utc::now().timestamp();
        if openrouter::read_cached_catalog_if_valid(&state_dir, now).is_none() {
            let client = reqwest::Client::new();
            let url = "https://openrouter.ai/api/v1/models";
            let _ = openrouter::fetch_openrouter_catalog_online(state_dir, &client, url, now).await;
        }
    }
    let (cfg, affected_models) = prepare_provider_candidate(
        config_path,
        &base,
        provider_id,
        Some(provider_name),
        api_key_env,
        no_api_key,
        provider_type,
        endpoint_url,
        selected_models,
        skip_probe,
        require_auth_env,
        pending_auth.as_ref(),
        progress_tx,
    )
    .await?;

    let accounts_path = crate::auth::state_path_for_config(config_path);
    // the credential is the expensive half of this two-step
    // write (only the user can produce it via the OAuth flow), so a failed
    // config write keeps it and reports loudly. Startup reconciliation
    // (`AppState::recover_orphan_oauth_providers`) completes the lost step.
    let kept_credential_note = || match pending_account {
        Some(pending) => format!(
            " (the OAuth credential for account {:?} was kept on disk: the provider is rebuilt \
             from it at next startup, so no re-login is needed)",
            pending.account
        ),
        None => String::new(),
    };
    if let Some(pending) = pending_account
        && let Err(e) = crate::auth::storage::write_pending_account_locked(&accounts_path, pending)
    {
        return Err(ProviderAddError::internal_error(format!(
            "failed to save pending OAuth account: {e}"
        )));
    }

    if let Some(parent) = config_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Err(ProviderAddError::internal_error(format!(
            "failed to create {}: {e}{}",
            parent.display(),
            kept_credential_note()
        )));
    }

    if let Err(e) = write_config_after_connect(
        config_path,
        &cfg,
        provider_name,
        &affected_models,
        config_existed,
    ) {
        return Err(ProviderAddError::internal_error(format!(
            "{}{}",
            e,
            kept_credential_note()
        )));
    }

    maybe_inject_provider_env(config_path, &cfg, provider_name);
    println!("connected provider: {provider_name}");
    println!("config: {}", config_path.display());
    Ok(())
}

/// Unified provider connect entry point.
/// Routes via `with_cli_write_lock_or_delegate` to server delegation or local atomic write.
/// When server is detected, failures are NOT fallen back to local write (No-Fallback Invariant).
#[allow(dead_code, clippy::too_many_arguments)]
pub async fn connect_provider(
    config_path: &Path,
    provider_id: &str,
    api_key_env: Option<String>,
    no_api_key: bool,
    provider_type: Option<String>,
    endpoint_url: Option<String>,
    selected_models: Option<&[String]>,
    provider_name: Option<&str>,
    skip_probe: bool,
    pending_account: Option<&crate::auth::PendingAccount>,
    progress_tx: Option<ProbeProgressSender>,
) -> Result<()> {
    let provider_id = provider_id.to_string();
    let provider_name = provider_name.map(ToString::to_string);
    let selected_models_vec = selected_models.map(|m| m.to_vec());
    let pending_account_owned = pending_account.cloned();

    crate::ownership::with_cli_write_lock_or_delegate(
        config_path,
        "llm-proxy connect provider",
        || {
            let provider_id = provider_id.clone();
            let provider_name = provider_name.clone();
            let api_key_env = api_key_env.clone();
            let provider_type = provider_type.clone();
            let endpoint_url = endpoint_url.clone();
            let selected_models_vec = selected_models_vec.clone();
            let config_path = config_path.to_path_buf();
            let pending_account = pending_account_owned.clone();
            let progress_tx = progress_tx.clone();
            async move {
                add_provider_with_models_inner(
                    &config_path,
                    &provider_id,
                    api_key_env,
                    no_api_key,
                    provider_type,
                    endpoint_url,
                    selected_models_vec.as_deref(),
                    provider_name.as_deref(),
                    skip_probe,
                    true, // require_auth_env: TUI 路径始终校验本地 env var
                    pending_account.as_ref(),
                    progress_tx.as_ref(),
                )
                .await
                .map_err(Into::into)
            }
        },
        |server| {
            let provider_id = provider_id.clone();
            let provider_name = provider_name.clone();
            let api_key_env = api_key_env.clone();
            let provider_type = provider_type.clone();
            let endpoint_url = endpoint_url.clone();
            let selected_models_vec = selected_models_vec.clone();
            let pending_account = pending_account_owned.clone();
            Box::pin(async move {
                let result = server
                    .add_provider_with_options(
                        &provider_id,
                        provider_name.as_deref(),
                        api_key_env.as_deref(),
                        no_api_key,
                        provider_type.as_deref(),
                        endpoint_url.as_deref(),
                        selected_models_vec.as_deref(),
                        skip_probe,
                        pending_account.as_ref(),
                    )
                    .await?;
                if let Some(message) = result
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.as_str())
                {
                    println!("{message}");
                }
                Ok(())
            })
        },
    )
    .await
}

pub fn reselect_provider_models(
    config_path: &Path,
    provider_id: &str,
    selected_models: &[String],
) -> Result<()> {
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let dirs = crate::service::StateDirs::resolve(config_dir);
    reselect_provider_models_at(config_path, &dirs.runtime_dir, provider_id, selected_models)
}

pub(crate) fn reselect_provider_models_at(
    config_path: &Path,
    runtime_dir: &Path,
    provider_id: &str,
    selected_models: &[String],
) -> Result<()> {
    let _lock = crate::core::ConfigLock::acquire(runtime_dir, std::time::Duration::from_secs(5))?;
    let mut cfg = Config::load(config_path)?;
    let affected_models =
        catalog::reselect_catalog_model_bindings(&mut cfg, provider_id, selected_models)?;
    crate::config_edit::write_models(config_path, &cfg, &affected_models)?;
    println!(
        "reselected {} model template(s) for provider: {provider_id}",
        selected_models.len()
    );
    Ok(())
}

fn ensure_provider_auth_ready(
    config_path: &Path,
    cfg: &Config,
    provider_id: &str,
    pending_auth: Option<&ResolvedAuth>,
) -> Result<()> {
    if let Some(auth) = pending_auth
        && auth.token.is_some()
    {
        return Ok(());
    }
    let provider = cfg
        .providers
        .get(provider_id)
        .with_context(|| format!("unknown provider {provider_id:?}"))?;
    let accounts_path = crate::auth::state_path_for_config(config_path);
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
        AuthConfig::OpenaiOauth { .. } => {
            // OAuth 取数统一走分发面（kind 分派收进 facade 内的
            // resolve→source；本臂只保留 connect 特有的上下文文案）。
            auth::get_token_valid(provider_id, provider, &accounts_path).with_context(|| {
                format!(
                    "provider {provider_id:?} requires `llm-proxy provider login {provider_id}` before connect/add can verify models"
                )
            })?;
        }
        AuthConfig::None => {}
    }
    Ok(())
}

async fn verify_selected_models(
    config_path: &Path,
    cfg: &Config,
    provider_id: &str,
    selected_models: &[String],
    pending_auth: Option<&ResolvedAuth>,
    progress_tx: Option<&ProbeProgressSender>,
) -> Result<()> {
    let accounts_path = crate::auth::state_path_for_config(config_path);
    if selected_models.is_empty() {
        bail!("mature provider setup requires at least one selected model to verify");
    }

    let client =
        crate::http_client::try_build_with_timeout(cfg, std::time::Duration::from_secs(30))
            .context("failed to create connectivity probe HTTP client")?;

    let batch_started = std::time::Instant::now();
    tracing::info!(
        target: "llm_proxy::connect",
        event = "connect_probe_batch_start",
        provider = %provider_id,
        model_count = selected_models.len(),
        "verifying the selected models against their upstream endpoints"
    );

    let mut verified_models = 0_usize;
    let total = selected_models.len();
    // 单个模型连不上**不得中断整批**：继续探测后续模型，
    // 最后一次性汇总。上游账号/数据策略类失败（如 OpenRouter 的 ZDR 404）属于
    // 「已知不可达」而非配置错误——逐条带给用户，由警告框的「继续」放行写配置
    // （`ProviderAddErrorKind::ProbeFailed` 已被 `is_recoverable_warning` 识别）。
    let mut failures: Vec<String> = Vec::new();
    for model_id in selected_models {
        if let Some(tx) = progress_tx {
            let _ = tx.send(ProbeProgress {
                done: verified_models,
                total,
                current_model: model_id.clone(),
            });
        }
        let (_frontend_id, plan) = match cfg
            .resolve_model_request(Protocol::OpenaiChatCompletions, model_id)
            .or_else(|| cfg.resolve_model_request(Protocol::OpenaiResponses, model_id))
            .or_else(|| cfg.resolve_model_request(Protocol::Anthropic, model_id))
        {
            Some(pair) => pair,
            None => {
                let reason = "selected model has no configured binding to verify".to_string();
                tracing::warn!(
                    target: "llm_proxy::connect",
                    event = "connect_probe_result",
                    provider = %provider_id,
                    model = %model_id,
                    outcome = "error",
                    error = %reason,
                    "cannot resolve binding for probe; skipping to next model"
                );
                failures.push(format!("  - {model_id}: {reason}"));
                continue;
            }
        };
        if plan.provider_id != provider_id {
            failures.push(format!(
                "  - {model_id}: resolved to provider {:?}, expected {:?}",
                plan.provider_id, provider_id
            ));
            continue;
        }

        // Probe wall time per model. This is the only record of how long the connect
        // verification stage spent on each model; the client timeout (30s) above is the
        // per-model budget, so a batch of N models can legitimately take N x 30s.
        let model_started = std::time::Instant::now();
        let outcome = run_probe(&client, &accounts_path, model_id, &plan, pending_auth, cfg).await;
        let duration_ms = model_started.elapsed().as_millis() as u64;
        match &outcome {
            Ok(()) => {
                tracing::info!(
                    target: "llm_proxy::connect",
                    event = "connect_probe_result",
                    provider = %provider_id,
                    model = %model_id,
                    upstream_model = %plan.upstream_model,
                    protocol = %plan.source_protocol.field_name(),
                    outcome = "ok",
                    duration_ms,
                    "connectivity probe succeeded"
                );
                verified_models += 1;
            }
            Err(error) => {
                let reason = format!("{error:#}");
                tracing::warn!(
                    target: "llm_proxy::connect",
                    event = "connect_probe_result",
                    provider = %provider_id,
                    model = %model_id,
                    upstream_model = %plan.upstream_model,
                    protocol = %plan.source_protocol.field_name(),
                    outcome = "error",
                    duration_ms,
                    error = %reason,
                    "connectivity probe failed; continuing with remaining models"
                );
                failures.push(format!("  - {model_id}: {reason}"));
            }
        }
    }

    if !failures.is_empty() {
        tracing::warn!(
            target: "llm_proxy::connect",
            event = "connect_probe_batch_complete",
            provider = %provider_id,
            verified_models,
            failed = failures.len(),
            total,
            duration_ms = batch_started.elapsed().as_millis() as u64,
            "probe batch finished with unreachable models"
        );
        bail!(
            "{} / {} 个模型当前不可达（其余 {} 个已通过探测）：\n{}\n\n选择「继续」可照常写入配置，这些模型稍后仍可重试。",
            failures.len(),
            total,
            verified_models,
            failures.join("\n")
        );
    }
    tracing::info!(
        target: "llm_proxy::connect",
        event = "connect_probe_batch_complete",
        provider = %provider_id,
        verified_models,
        duration_ms = batch_started.elapsed().as_millis() as u64,
        "all selected models verified"
    );

    Ok(())
}

async fn run_probe(
    client: &reqwest::Client,
    accounts_path: &Path,
    model_id: &str,
    plan: &crate::config::ExecutionPlan,
    pending_auth: Option<&ResolvedAuth>,
    cfg: &crate::config::Config,
) -> Result<()> {
    if plan.adapter != AdapterKind::Passthrough {
        // Probe the native upstream protocol directly. The goal is credential/model reachability,
        // while adapter correctness is covered by adapter/proxy tests and normal runtime paths.
    }

    let resolved_auth = if let Some(auth) = pending_auth {
        auth.clone()
    } else {
        let provider_cfg = cfg
            .providers
            .get(&plan.provider_id)
            .with_context(|| format!("unknown provider {:?}", plan.provider_id))?;
        match &plan.auth {
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
            AuthConfig::OpenaiOauth { .. } => {
                // probe 取数面走 facade（账号归一化/取数/刷新同源；
                // 分类已在 plan.auth 上、L1 分层不动）。
                let token = auth::get_token_valid(&plan.provider_id, provider_cfg, accounts_path)?;
                ResolvedAuth {
                    token: Some(token),
                    project_id: None,
                }
            }
            AuthConfig::None => ResolvedAuth {
                token: None,
                project_id: None,
            },
        }
    };

    let body = crate::probe::probe_body_with_auth(plan, Some(&resolved_auth))?;
    let request =
        crate::probe::apply_protocol_headers(plan, client.post(&plan.native_url)).json(&body);
    let request = crate::probe::apply_auth_header(plan, request, resolved_auth.token);

    let request_started = std::time::Instant::now();
    let response = request
        .send()
        .await
        .with_context(|| format!("connectivity probe failed for model {model_id}"))?;
    let ttfb_ms = request_started.elapsed().as_millis() as u64;
    let status = response.status();
    if !status.is_success() {
        let body_started = std::time::Instant::now();
        let text = response.text().await.unwrap_or_default();
        tracing::debug!(
            target: "llm_proxy::connect",
            event = "connect_probe_timing",
            model = %model_id,
            status = %status,
            ttfb_ms,
            body_ms = body_started.elapsed().as_millis() as u64,
            body_bytes = text.len(),
            "connectivity probe phases (the response body is read on the failure path only)"
        );
        bail!(
            "connectivity probe failed for model {model_id}: upstream returned {status}: {}",
            text.chars().take(500).collect::<String>()
        );
    }
    tracing::debug!(
        target: "llm_proxy::connect",
        event = "connect_probe_timing",
        model = %model_id,
        status = %status,
        ttfb_ms,
        body_read = false,
        "connectivity probe phases (a successful probe returns as soon as the response headers arrive)"
    );
    Ok(())
}

pub(crate) fn maybe_inject_provider_env(config_path: &Path, cfg: &Config, provider_id: &str) {
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
    // 注入目标必须与该 config 同一 identity 分片：走全局默认会把 env 注进别的实例（或空跑）。
    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    let runtime_dir = crate::service::StateDirs::resolve(config_dir).runtime_dir;
    if let Ok(applied) = service::inject_env_in(&runtime_dir, &vars)
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
    affected_models: &[String],
    config_existed: bool,
) -> Result<()> {
    let serialized = crate::config_edit::canonical_format(cfg)?;
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

    if !doc.get("providers").is_some_and(|p| p.is_table()) {
        doc.insert("providers", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    doc["providers"][provider_id] = source["providers"][provider_id].clone();

    if !doc.get("models").is_some_and(|m| m.is_table()) {
        doc.insert("models", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    for model_id in affected_models {
        if doc["models"].get(model_id).is_some() {
            for field in [
                "openai_chat_providers",
                "openai_responses_providers",
                "anthropic_providers",
            ] {
                if let Some(source_field) = source["models"][model_id].get(field) {
                    doc["models"][model_id][field] = source_field.clone();
                }
            }
        } else {
            doc["models"][model_id] = source["models"][model_id].clone();
        }
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
/// native endpoint whose `url` is stored as-is.
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

    /// 判据 2：connect 生成的 provider（catalog 与 custom 两条路径）
    /// 都携带 catalog 内置的异常请求容忍量默认值 2。
    #[test]
    fn add_provider_carries_bad_request_tolerance_default() {
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
        .expect("add catalog provider");
        add_provider(
            &path,
            "custom-p1",
            Some("CUSTOM_P1_KEY".to_string()),
            false,
            Some("chat".to_string()),
            Some("https://example.com/v1/chat/completions".to_string()),
        )
        .expect("add custom provider");

        let cfg = Config::load(&path).expect("load");
        assert_eq!(
            cfg.providers["openai-payg"].bad_request_tolerance, 2,
            "catalog provider must carry the built-in tolerance default"
        );
        assert_eq!(
            cfg.providers["custom-p1"].bad_request_tolerance, 2,
            "custom provider must carry the built-in tolerance default"
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
    fn provider_copy_and_remove_canonical_output_strips_comments() {
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
        // : canonical formatter strips hand-written comments
        assert!(!text.contains("# root comment"));
        assert!(!text.contains("# source provider comment"));
        assert!(!text.contains("# model comment"));
        // Semantic correctness: removed provider is gone, original data survives
        assert!(!text.contains("[providers.deepseek-copy]"));
        assert!(text.contains("[providers.deepseek]"));
        assert!(text.contains("[models.model-a]"));
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
    fn provider_remove_rejects_referenced_provider_without_force_and_removes_unref() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        let err = remove_provider(&path, "deepseek", false).expect_err("referenced");
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
    #[serial]
    fn force_remove_referenced_provider_cleans_model_bindings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        let cfg = Config::load(&path).expect("load");
        let referenced = |cfg: &Config| {
            cfg.models.values().any(|m| {
                m.openai_chat_providers
                    .iter()
                    .chain(m.openai_responses_providers.iter())
                    .chain(m.anthropic_providers.iter())
                    .any(|b| b.name == "deepseek")
            })
        };
        assert!(referenced(&cfg), "precondition: deepseek is referenced");

        remove_provider(&path, "deepseek", true).expect("force remove referenced provider");

        let cfg = Config::load(&path).expect("load");
        assert!(!cfg.providers.contains_key("deepseek"));
        assert!(!referenced(&cfg), "force remove must clean model bindings");
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

        let err = add_provider_with_models_inner(
            &path,
            "openai-payg",
            Some(missing_env.to_string()),
            false,
            None,
            None,
            Some(&["gpt-5.5-lp".to_string()]),
            None,
            false,
            true, // require_auth_env: 缺 env 必须在写配置前失败
            None,
            None,
        )
        .await
        .expect_err("missing env should fail before write");

        assert_eq!(err.kind, ProviderAddErrorKind::AuthNotReady);
        assert!(err.to_string().contains(missing_env));
        let after = fs::read_to_string(&path).expect("read after");
        assert_eq!(after, before);
    }

    /// 回归：OpenRouter **不得**被硬编码强制跳过探测。
    ///
    /// 旧实现 `let skip_probe = skip_probe || is_openrouter;` 会 shadow 掉入参，
    /// 使探测确认屏对 OpenRouter 完全失效——无论用户在屏上选「探测」还是「跳过」，
    /// 实际永远零网络请求。
    ///
    /// 这里把 endpoint 指向必然拒绝连接的 `127.0.0.1:1`：
    /// 探测真的执行 → 必然 `ProbeFailed`；旧实现（强制跳过）→ `Ok`，本断言即失败。
    /// 只连本地闭端口，不依赖外网。
    #[tokio::test]
    async fn openrouter_does_not_force_skip_probe() {
        let env = "LLM_PROXY_TEST_OPENROUTER_PROBE_GUARD_ENV";
        unsafe { std::env::set_var(env, "lp-test-token") };

        let mut base = crate::config::default_deepseek_config();
        let mut provider = ProviderConfig {
            api_key_env: Some(env.to_string()),
            ..ProviderConfig::default()
        };
        provider.set_endpoint(
            Protocol::OpenaiChatCompletions,
            EndpointConfig::native("http://127.0.0.1:1/v1/chat/completions"),
        );
        base.providers.insert("openrouter".to_string(), provider);

        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        let selected = ["anthropic/claude-sonnet-4".to_string()];

        let err = prepare_provider_candidate(
            &config_path,
            &base,
            "openrouter",
            None,
            Some(env.to_string()),
            false,
            None,
            None,
            Some(&selected),
            false, // skip_probe = false：显式要求探测
            true,  // require_auth_env：本地 env 已设置，不会先在这步失败
            None,
            None,
        )
        .await
        .expect_err("OpenRouter 探测必须真的执行（端口不可达 → 必然失败）");

        assert_eq!(
            err.kind,
            ProviderAddErrorKind::ProbeFailed,
            "期望 ProbeFailed，实际: {err}"
        );
    }

    /// **单个模型连不上不得中断整批探测**。
    ///
    /// 把 endpoint 指向必然拒绝的 `127.0.0.1:1`，两个模型都会失败；
    /// 断言错误消息**同时包含两个模型**——若沿用旧的 `outcome?` 早退，
    /// 消息只会含第一个模型，本断言即失败。
    /// 另断言该错误可被 `is_recoverable_warning` 识别，从而在 TUI 弹出
    /// 警告框、由「继续」放行写配置（支持"连不上也照常接入"）。
    #[tokio::test]
    async fn probe_batch_continues_after_individual_failure() {
        let env = "LLM_PROXY_TEST_PROBE_BATCH_ENV";
        unsafe { std::env::set_var(env, "lp-test-token") };

        let mut base = crate::config::default_deepseek_config();
        let mut provider = ProviderConfig {
            api_key_env: Some(env.to_string()),
            ..ProviderConfig::default()
        };
        provider.set_endpoint(
            Protocol::OpenaiChatCompletions,
            EndpointConfig::native("http://127.0.0.1:1/v1/chat/completions"),
        );
        base.providers.insert("openrouter".to_string(), provider);

        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        let selected = [
            "anthropic/claude-sonnet-4".to_string(),
            "stepfun/step-5-preview".to_string(),
        ];

        let err = prepare_provider_candidate(
            &config_path,
            &base,
            "openrouter",
            None,
            Some(env.to_string()),
            false,
            None,
            None,
            Some(&selected),
            false,
            true,
            None,
            None,
        )
        .await
        .expect_err("两个模型端点都不可达，必然失败");

        assert_eq!(err.kind, ProviderAddErrorKind::ProbeFailed, "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("claude-sonnet-4") && msg.contains("stepfun-step-5-preview"),
            "必须逐条列出**所有**不可达模型（证明没有在第一个失败处早退），实际: {msg}"
        );
        assert!(msg.contains("2 / 2"), "须给出 N/M 汇总计数，实际: {msg}");

        // 该错误必须能触发 TUI 警告框（Continue → 跳过探测照常写配置）
        let anyhow_err: anyhow::Error = err.into();
        assert!(
            crate::admin_client::is_recoverable_warning(&anyhow_err),
            "ProbeFailed 必须是可恢复警告，否则用户无法放行接入"
        );
    }

    #[tokio::test]
    async fn mature_provider_with_selected_model_allows_deferred_env_when_skip_probe_true() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");
        let missing_env = "LLM_PROXY_TEST_MISSING_KEY_DEFERRED_CONFIG";

        let result = add_provider_with_models_inner(
            &path,
            "openai-payg",
            Some(missing_env.to_string()),
            false,
            None,
            None,
            Some(&["gpt-5.5-lp".to_string()]),
            None,
            true,  // skip_probe: true allows deferred config
            false, // require_auth_env: deferred 配置——先写配置、凭据后补
            None,
            None,
        )
        .await;

        assert!(
            result.is_ok(),
            "skip_probe should allow deferred config: {:?}",
            result.err()
        );
        let cfg = Config::load(&path).expect("load config");
        assert!(cfg.providers.contains_key("openai-payg"));
    }

    #[tokio::test]
    async fn skip_probe_does_not_bypass_schema_or_reference_validation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).expect("init");

        // 1. Mutually exclusive flags must still fail with invalid_config even when skip_probe = true
        let err1 = add_provider_with_models_inner(
            &path,
            "openai-payg",
            Some("SOME_KEY".to_string()),
            true, // mutually exclusive with api_key_env
            None,
            None,
            Some(&["gpt-5.5-lp".to_string()]),
            None,
            true,
            true, // require_auth_env
            None,
            None,
        )
        .await
        .expect_err("mutually exclusive flags must fail");
        assert_eq!(err1.kind, ProviderAddErrorKind::InvalidConfig);

        // 2. Custom provider missing endpoint_url must fail with invalid_config
        let err2 = add_provider_with_models_inner(
            &path,
            "custom-test",
            None,
            true,
            Some("openai-chat".to_string()),
            None, // missing endpoint_url
            None,
            None,
            true,
            true, // require_auth_env
            None,
            None,
        )
        .await
        .expect_err("custom provider without endpoint_url must fail");
        assert_eq!(err2.kind, ProviderAddErrorKind::InvalidConfig);

        // 3. Unknown catalog provider must fail with invalid_config
        let err3 = add_provider_with_models_inner(
            &path,
            "nonexistent-catalog-product-xyz",
            None,
            true,
            None,
            None,
            None,
            None,
            true,
            true, // require_auth_env
            None,
            None,
        )
        .await
        .expect_err("unknown catalog provider must fail");
        assert_eq!(err3.kind, ProviderAddErrorKind::InvalidConfig);
    }

    #[test]
    fn custom_provider_requires_endpoint_path() {
        let err = build_custom_provider("openai-chat", "https://example.com", None, true)
            .expect_err("path required");
        assert!(err.to_string().contains("concrete endpoint path"));
    }

    // =========================================================================
    // parse_protocol + validate_endpoint_url pure function tests
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
    // usage_window_summary + confirm_yes_no tests
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
    fn test_confirm_yes_no_rejects_non_terminal_input() {
        // In test environment, stdin/stdout are not terminals,
        // so confirm_yes_no should bail before reading stdin.
        let result = ensure_interactive_terminal_state(false, false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("interactive terminal"),
            "should reject non-terminal input"
        );
    }

    #[tokio::test]
    async fn local_named_provider_persists_binding_to_existing_model() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config_path = temp.path().join("config.toml");
        let base = crate::config::default_deepseek_config();
        let (first, _) = prepare_provider_candidate(
            &config_path,
            &base,
            "openai-sub",
            None,
            None,
            true,
            None,
            None,
            None,
            false,
            true, // require_auth_env
            None,
            None,
        )
        .await
        .expect("first provider candidate");
        crate::config_edit::atomic_write(
            &config_path,
            toml::to_string_pretty(&first)
                .expect("serialize first config")
                .as_bytes(),
        )
        .expect("write first config");

        let (second, affected_models) = prepare_provider_candidate(
            &config_path,
            &first,
            "openai-sub",
            Some("openai-sub-2"),
            None,
            true,
            None,
            None,
            None,
            false,
            true, // require_auth_env
            None,
            None,
        )
        .await
        .expect("second provider candidate");
        write_config_after_connect(
            &config_path,
            &second,
            "openai-sub-2",
            &affected_models,
            true,
        )
        .expect("persist second provider");

        let persisted = Config::load(&config_path).expect("reload config");
        let model_id = affected_models.first().expect("affected model");
        let model = persisted.models.get(model_id).expect("persisted model");
        assert!(
            model
                .openai_chat_providers
                .iter()
                .any(|binding| binding.name == "openai-sub-2")
        );
    }

    #[test]
    fn write_config_preserves_comments_on_affected_model_tables() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config_path = temp.path().join("config.toml");
        let mut cfg = crate::config::default_deepseek_config();

        let extra_provider = crate::catalog::deepseek().provider;
        cfg.providers
            .insert("deepseek-2".to_string(), extra_provider);
        cfg.models
            .get_mut("deepseek-v4-flash-lp")
            .unwrap()
            .openai_chat_providers
            .push(crate::config::ProviderBinding {
                name: "deepseek-2".to_string(),
                model: "deepseek-v4-flash".to_string(),
            });

        let serialized = toml::to_string_pretty(&cfg).expect("serialize");
        crate::config_edit::atomic_write(&config_path, serialized.as_bytes())
            .expect("write initial config");

        let original = std::fs::read_to_string(&config_path).expect("read back");
        let modified = original.replacen(
            "[models.deepseek-v4-flash-lp]",
            "# important comment\n[models.deepseek-v4-flash-lp]",
            1,
        );
        std::fs::write(&config_path, modified.as_bytes()).expect("write with comment");

        write_config_after_connect(
            &config_path,
            &cfg,
            "deepseek-2",
            &["deepseek-v4-flash-lp".to_string()],
            true,
        )
        .expect("persist updated config");

        let final_content = std::fs::read_to_string(&config_path).expect("read final");
        assert!(
            final_content.contains("# important comment"),
            "comment on model table must survive round-trip update"
        );

        let reloaded = crate::config::Config::load(&config_path).expect("reload config");
        let model = reloaded.models.get("deepseek-v4-flash-lp").expect("model");
        assert!(
            model
                .openai_chat_providers
                .iter()
                .any(|b| b.name == "deepseek-2"),
            "new binding must be persisted"
        );
    }

    async fn start_mock_ping_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/admin/ping",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({"version": "0.0.0"}))
                }),
            );
            axum::serve(listener, app).await.ok();
        });
        (format!("{addr}"), handle)
    }

    /// No-Fallback Invariant for `connect_provider`: when server is detected via HTTP ping
    /// but delegation fails, the local config file must remain untouched.
    ///
    /// Uses an unknown provider_id so the real server (if running) returns an error,
    /// and no UDS socket (if absent) also causes an error. Either way, delegation fails
    /// and local_write must NOT run.
    #[tokio::test]
    async fn connect_provider_server_delegation_failure_does_not_write_local() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        crate::config::init_config(&config_path).expect("init config");

        let (listen_addr, _server_handle) = start_mock_ping_server().await;

        let mut cfg = crate::config::Config::load(&config_path).unwrap();
        cfg.server.as_mut().unwrap().listen = listen_addr;
        crate::config_edit::write_full_config(&config_path, &cfg).unwrap();
        let config_before = std::fs::read_to_string(&config_path).expect("read before");

        // Use mutually exclusive flags to guarantee the server rejects the request,
        // ensuring delegation fails regardless of whether a real server is running.
        let result = connect_provider(
            &config_path,
            "deepseek",
            Some("SOME_KEY".into()),
            true, // mutually exclusive with api_key_env → invalid_config
            None,
            None,
            None,
            None,
            false,
            None,
            None,
        )
        .await;

        assert!(
            result.is_err(),
            "delegation should fail with invalid config"
        );
        let config_after = std::fs::read_to_string(&config_path).expect("read after");
        assert_eq!(
            config_before, config_after,
            "config must not be modified when server delegation fails"
        );
    }

    /// Standalone twin of the admin handler contract: when
    /// only the second step of the connect write fails, the credential stays on
    /// disk and the error says so, because that credential is the input startup
    /// reconciliation needs to finish the job without a new OAuth flow.
    #[tokio::test]
    async fn standalone_connect_keeps_the_credential_when_the_config_write_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        // Break only the second step: `atomic_write` stages through
        // `config.toml.tmp`, so occupying that name with a directory makes the
        // config write fail after the credential write has already succeeded.
        std::fs::create_dir(config_path.with_extension("toml.tmp")).expect("occupy staging path");

        let pending = crate::auth::PendingAccount {
            auth_type: "openai".into(),
            account: "openai-subscription".into(),
            access_token: "mock-access-token-value-1234567890".into(),
            refresh_token: Some("mock-refresh-token-value-1234567890".into()),
            project_id: None,
            expires_at_unix: Some(2_000_000_000),
            email: None,
        };
        let error = add_provider_with_models_inner(
            &config_path,
            "openai-sub",
            None,
            true,
            None,
            None,
            None,
            None,
            true,
            true, // require_auth_env
            Some(&pending),
            None,
        )
        .await
        .expect_err("the config write must fail");

        assert_eq!(error.kind, ProviderAddErrorKind::InternalError);
        assert!(
            error.message.contains("kept on disk"),
            "failure must report that the credential was kept: {error}"
        );
        let accounts_path = crate::auth::state_path_for_config(&config_path);
        let accounts =
            crate::auth::load_oauth_accounts_with_recovery(&accounts_path).expect("load accounts");
        assert!(
            accounts.openai.contains_key("openai-subscription"),
            "credential must survive a failed config write"
        );

        // The surviving credential is enough: startup rebuilds the provider.
        let mut cfg: Config = toml::from_str(
            r#"
[server]
listen = "127.0.0.1:8989"
"#,
        )
        .expect("server-only config");
        let report = crate::auth::recover_orphan_oauth_accounts(&mut cfg, &accounts);
        assert_eq!(
            report
                .recovered
                .first()
                .map(|entry| entry.provider.as_str()),
            Some("openai-sub"),
            "{report:?}"
        );
    }

    // ── connect 验证探针的耗时可观测性 ────────────────────────────────────────
    //
    // 缺口：`verify_selected_models` / `run_probe` 原先**一条日志都没有**（本文件里
    // `Instant` 与 `tracing::` 均为 0 处），于是「验证阶段卡很久」既无法定位到具体
    // 模型，也无法区分「上游/网络慢」与「探针本身卡住」。下面两条用例把事件名、级别
    // 与字段钉住：删掉或降级日志、把 duration 写成常数，都会变红。

    /// 捕获本测试线程的日志事件（`event=` + 其余字段串接）。
    ///
    /// 做法与 `service.rs::ServiceLogCapture`、`proxy::tests` 同款：除 `set_default`
    /// 外还要再注册一个 `tracing::Dispatch`，否则 tracing-core 的单 dispatch 快路径会把
    /// 并发测试线程的"无订阅者" interest 缓存下来。不过滤 level/target，只认 `event=`。
    struct ProbeLogCapture {
        _guard: tracing::subscriber::DefaultGuard,
        _registration: tracing::Dispatch,
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ProbeLogCapture {
        fn start() -> Self {
            Self::build("trace")
        }

        /// 用**真实缺省 filter** 启动，用于验证真实探测跑在缺省日志级别下时
        /// info 埋点能穿透、debug 埋点仍被挡（见
        /// `probe_info_events_penetrate_default_filter_but_debug_does_not`）。
        fn start_with_default_filter() -> Self {
            Self::build(crate::DEFAULT_ENV_FILTER)
        }

        fn build(filter: &str) -> Self {
            use tracing_subscriber::prelude::*;
            let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let _registration = tracing::Dispatch::new(tracing_subscriber::registry());
            let subscriber = tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new(filter))
                .with(ProbeEventLayer(std::sync::Arc::clone(&events)));
            Self {
                _guard: tracing::subscriber::set_default(subscriber),
                _registration,
                events,
            }
        }

        fn lines(&self) -> Vec<String> {
            self.events.lock().expect("capture lock").clone()
        }

        fn find(&self, name: &str) -> Option<String> {
            let needle = format!("event={name}");
            self.lines().into_iter().find(|line| line.contains(&needle))
        }
    }

    struct ProbeEventLayer(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    #[derive(Default)]
    struct ProbeEventFields(std::collections::BTreeMap<String, String>);

    impl tracing::field::Visit for ProbeEventFields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let rendered = format!("{value:?}");
            // 字符串字面量按 Debug 渲染会带引号；去掉引号后，断言写的就是运维在日志里
            // 实际看到的文本。
            let unquoted = rendered
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(&rendered);
            self.0
                .insert(field.name().to_string(), unquoted.to_string());
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ProbeEventLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = ProbeEventFields::default();
            event.record(&mut fields);
            if !fields.0.contains_key("event") {
                return;
            }
            let mut line = format!(
                "event={} level={}",
                fields.0.remove("event").unwrap_or_default(),
                event.metadata().level()
            );
            for (name, value) in fields.0 {
                line.push_str(&format!(" {name}={value}"));
            }
            self.0.lock().expect("capture lock").push(line);
        }
    }

    /// 从捕获行里取数值字段（形如 `duration_ms=251`）。
    fn field_u64(line: &str, name: &str) -> u64 {
        let needle = format!("{name}=");
        let value = line
            .split_whitespace()
            .find_map(|part| part.strip_prefix(&needle))
            .unwrap_or_else(|| panic!("{name} missing in: {line}"));
        value
            .parse()
            .unwrap_or_else(|error| panic!("{name} is not a number ({value:?}): {error}"))
    }

    /// mock 上游：`/chat/completions` 延迟 `delay` 后返回 `status`。
    async fn start_mock_upstream(
        delay: std::time::Duration,
        status: axum::http::StatusCode,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock upstream");
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/chat/completions",
                axum::routing::post(move || async move {
                    tokio::time::sleep(delay).await;
                    (status, axum::Json(serde_json::json!({"ok": true})))
                }),
            );
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}/chat/completions"), handle)
    }

    fn probe_config(upstream_url: &str) -> Config {
        let mut cfg = crate::config::default_deepseek_config();
        cfg.providers
            .get_mut("deepseek")
            .expect("deepseek provider")
            .set_endpoint(
                Protocol::OpenaiChatCompletions,
                EndpointConfig::native(upstream_url),
            );
        cfg
    }

    fn probe_auth() -> ResolvedAuth {
        ResolvedAuth {
            token: Some("test-token".to_string()),
            project_id: None,
        }
    }

    /// 成功路径：批次开始/结束 + 每个模型的结果 + 阶段拆分都要有，且 `duration_ms`
    /// 必须反映真实等待（mock 延迟 250ms），不能是写死的 0。
    #[tokio::test]
    async fn connect_probe_logs_per_model_duration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        crate::config::init_config(&config_path).expect("init config");

        let (url, _upstream) = start_mock_upstream(
            std::time::Duration::from_millis(250),
            axum::http::StatusCode::OK,
        )
        .await;
        let cfg = probe_config(&url);
        let auth = probe_auth();
        let models = ["deepseek-v4-flash-lp".to_string()];

        let capture = ProbeLogCapture::start();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            verify_selected_models(&config_path, &cfg, "deepseek", &models, Some(&auth), None),
        )
        .await
        .expect("probe must finish well within 10s")
        .expect("probe against the mock upstream must succeed");

        let batch = capture
            .find("connect_probe_batch_start")
            .expect("batch start event");
        assert!(batch.contains("model_count=1"), "{batch}");
        assert!(batch.contains("provider=deepseek"), "{batch}");

        let result = capture
            .find("connect_probe_result")
            .expect("per-model probe result event");
        assert!(result.contains("outcome=ok"), "{result}");
        assert!(result.contains("model=deepseek-v4-flash-lp"), "{result}");
        assert!(result.contains("protocol=openai_chat"), "{result}");
        let duration_ms = field_u64(&result, "duration_ms");
        assert!(
            duration_ms >= 200,
            "the mock upstream delayed 250ms, so duration_ms must show it: {result}"
        );
        assert!(
            duration_ms < 10_000,
            "duration_ms must be a real measurement, not a placeholder: {result}"
        );

        let timing = capture
            .find("connect_probe_timing")
            .expect("phase split event");
        assert!(timing.contains("ttfb_ms="), "{timing}");

        let done = capture
            .find("connect_probe_batch_complete")
            .expect("batch complete event");
        assert!(done.contains("verified_models=1"), "{done}");
    }

    /// E2E：真实探测跑在**真实缺省 filter**（`DEFAULT_ENV_FILTER`）下——
    /// info 埋点必须穿透（否则探测在默认日志级别下零留痕），debug 埋点必须仍被挡。
    ///
    /// 这条是 `main.rs::default_filter_surfaces_own_info_but_keeps_third_party_quiet`
    /// （filter 契约层）与 `connect_probe_logs_per_model_duration`（事件发射层）之间的
    /// 拼接证据：光知道 filter 放行、或光知道事件会发，都不等于缺省下真的看得见。
    #[tokio::test]
    async fn probe_info_events_penetrate_default_filter_but_debug_does_not() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        crate::config::init_config(&config_path).expect("init config");

        let (url, _upstream) = start_mock_upstream(
            std::time::Duration::from_millis(50),
            axum::http::StatusCode::OK,
        )
        .await;
        let cfg = probe_config(&url);
        let auth = probe_auth();
        let models = ["deepseek-v4-flash-lp".to_string()];

        let capture = ProbeLogCapture::start_with_default_filter();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            verify_selected_models(&config_path, &cfg, "deepseek", &models, Some(&auth), None),
        )
        .await
        .expect("probe must finish well within 10s")
        .expect("probe against the mock upstream must succeed");

        capture
            .find("connect_probe_batch_start")
            .expect("缺省 filter 必须放行 info 的批次开始 —— 否则排查时无法判断探测是否发生过");
        capture
            .find("connect_probe_result")
            .expect("缺省 filter 必须放行 info 的单模型结果");
        capture
            .find("connect_probe_batch_complete")
            .expect("缺省 filter 必须放行 info 的批次完成");
        assert!(
            capture.find("connect_probe_timing").is_none(),
            "debug 埋点必须仍被缺省 filter 挡住 —— 否则缺省日志被阶段拆分刷屏"
        );
    }

    /// 失败路径：同样要留下耗时与原因，且级别必须默认可见性成立（warn）。
    #[tokio::test]
    async fn connect_probe_logs_failure_with_duration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        crate::config::init_config(&config_path).expect("init config");

        let (url, _upstream) = start_mock_upstream(
            std::time::Duration::from_millis(50),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
        let cfg = probe_config(&url);
        let auth = probe_auth();
        let models = ["deepseek-v4-flash-lp".to_string()];

        let capture = ProbeLogCapture::start();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            verify_selected_models(&config_path, &cfg, "deepseek", &models, Some(&auth), None),
        )
        .await
        .expect("probe must finish well within 10s")
        .expect_err("an upstream 500 must fail the probe");

        let result = capture
            .find("connect_probe_result")
            .expect("per-model probe result event");
        assert!(result.contains("outcome=error"), "{result}");
        assert!(
            result.contains("level=WARN"),
            "a failed probe must be recorded at warn level: {result}"
        );
        assert!(field_u64(&result, "duration_ms") >= 40, "{result}");

        let timing = capture
            .find("connect_probe_timing")
            .expect("phase split event on the failure path");
        assert!(timing.contains("body_ms="), "{timing}");
        assert!(
            error.to_string().contains("upstream returned 500"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn test_connect_openrouter_on_minimal_config_without_providers_or_models_table() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:8999\"\n")
            .expect("write minimal config");

        let result = add_provider_with_models_inner(
            &config_path,
            "openrouter",
            Some("TEST_KEY".to_string()),
            false,
            None,
            None,
            Some(&["anthropic/claude-sonnet-4".to_string()]),
            None,
            true,
            false, // require_auth_env: 只验证配置写入，TEST_KEY 未导出
            None,
            None,
        )
        .await;

        assert!(result.is_ok(), "must succeed: {:?}", result.err());
        let cfg = Config::load(&config_path).expect("load config");
        assert!(cfg.providers.contains_key("openrouter"));
        let model = cfg
            .models
            .get("anthropic-claude-sonnet-4-or-lp")
            .expect("model generated");
        assert_eq!(
            model.openai_chat_providers[0].model,
            "anthropic/claude-sonnet-4"
        );
    }
}
