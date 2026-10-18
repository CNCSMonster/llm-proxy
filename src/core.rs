#![allow(dead_code)] // groundwork for Admin API
//! Core architecture layer — business logic separated from I/O.
//!
//! CoreState holds all application state (config, usage, oauth) and is the
//! single writer to persistent storage. CLI/TUI/Admin API all go through Core.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fs2::FileExt;

use crate::config::{AuthConfig, Config, Protocol, ProviderConfig};
use crate::usage_stats::{UsageRecord, UsageStore};

/// Core application state — owns all business data and persistence.
pub struct CoreState {
    config: Config,
    usage_store: Option<Arc<UsageStore>>,
    config_path: PathBuf,
    state_dirs: crate::service::StateDirs,
}

impl CoreState {
    /// Load core state from config file.
    pub fn load(config_path: &Path) -> Result<Self> {
        let config = Config::load(config_path)?;
        let config_dir = config_path.parent().unwrap_or(Path::new("."));
        let state_dirs = crate::service::StateDirs::resolve(config_dir);
        // usage 必须与 state_dirs 同源：走全局默认会给默认 identity 凭空建目录。
        let usage_store = config
            .server
            .as_ref()
            .map(|s| s.usage.clone())
            .and_then(|usage| UsageStore::new_with_state_dir(usage, &state_dirs.state_dir).ok())
            .map(Arc::new);
        Ok(Self {
            config,
            usage_store,
            config_path: config_path.to_path_buf(),
            state_dirs,
        })
    }

    /// Create CoreState from an existing Config.
    pub fn from_config(config: Config, config_path: PathBuf) -> Self {
        let config_dir = config_path.parent().unwrap_or(Path::new("."));
        let state_dirs = crate::service::StateDirs::resolve(config_dir);
        let usage_store = config
            .server
            .as_ref()
            .map(|s| s.usage.clone())
            .and_then(|usage| UsageStore::new_with_state_dir(usage, &state_dirs.state_dir).ok())
            .map(Arc::new);
        Self {
            config,
            usage_store,
            config_path,
            state_dirs,
        }
    }

    /// Create CoreState with an explicit runtime directory.
    ///
    /// Tests use this constructor to isolate the cross-process config lock
    /// without changing process-wide path environment variables.
    pub fn from_config_with_state_dirs(
        config: Config,
        config_path: PathBuf,
        state_dirs: crate::service::StateDirs,
    ) -> Self {
        let usage_store = config
            .server
            .as_ref()
            .map(|s| s.usage.clone())
            .and_then(|usage| UsageStore::new_with_state_dir(usage, &state_dirs.state_dir).ok())
            .map(Arc::new);
        Self {
            config,
            usage_store,
            config_path,
            state_dirs,
        }
    }

    /// Load core state, sharing an externally-created usage store（内存权威单一实例）。
    pub fn load_with_usage_store(
        config_path: &Path,
        usage_store: Option<Arc<UsageStore>>,
    ) -> Result<Self> {
        let mut core = Self::load(config_path)?;
        core.usage_store = usage_store;
        Ok(core)
    }

    /// Create CoreState from an existing Config, sharing an externally-created usage store.
    pub fn from_config_with_usage_store(
        config: Config,
        config_path: PathBuf,
        usage_store: Option<Arc<UsageStore>>,
    ) -> Self {
        let mut core = Self::from_config(config, config_path);
        core.usage_store = usage_store;
        core
    }

    /// Reload configuration from disk.
    pub fn reload_config(&mut self) -> Result<()> {
        self.config = Config::load(&self.config_path)?;
        Ok(())
    }

    /// Get a reference to the current config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Get a mutable reference to the config.
    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    pub(crate) fn replace_config_in_memory(&mut self, config: Config) {
        self.config = config;
    }

    /// Get the usage store if available.
    pub fn usage_store(&self) -> Option<&UsageStore> {
        self.usage_store.as_ref().map(|arc| arc.as_ref())
    }

    /// Get an owned clone of the shared usage store Arc if available.
    pub fn usage_store_arc(&self) -> Option<Arc<UsageStore>> {
        self.usage_store.clone()
    }

    #[cfg(test)]
    pub fn set_usage_store(&mut self, usage_store: Option<Arc<UsageStore>>) {
        self.usage_store = usage_store;
    }

    /// Get the config path.
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Get the per-instance state directories.
    pub fn state_dirs(&self) -> &crate::service::StateDirs {
        &self.state_dirs
    }

    /// Get the runtime directory (for cross-process locks and socket paths).
    pub fn runtime_dir(&self) -> &Path {
        &self.state_dirs.runtime_dir
    }

    /// Save config to disk.
    ///
    /// Acquires the cross-process `ConfigLock` (flock) so a concurrent CLI
    /// process writing config.toml through the same lock cannot lose updates.
    pub fn save_config(&self) -> Result<()> {
        self.with_config_file_lock(|| {
            crate::config_edit::write_full_config(&self.config_path, &self.config)
        })
    }

    /// Run a config-file write while holding the cross-process file lock.
    ///
    /// The server serializes writers in-process via `Mutex<CoreState>`, but a
    /// CLI process (connect/provider/model commands) serializes via flock. Both
    /// sides must hold the same flock before touching config.toml, otherwise a
    /// concurrent read-modify-write from the other side loses updates.
    fn with_config_file_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _lock = ConfigLock::acquire(&self.state_dirs.runtime_dir, Duration::from_secs(5))?;
        f()
    }

    /// 整体替换配置（TUI 编辑器保存的 server 侧执行）：
    /// 持锁写盘 + 更新内存，保持单一写者模型下的内存/盘一致。
    pub fn update_full_config(&mut self, cfg: &Config) -> Result<()> {
        cfg.validate()?;
        self.with_config_file_lock(|| {
            crate::config_edit::write_full_config(&self.config_path, cfg)
        })?;
        self.config = cfg.clone();
        Ok(())
    }

    /// Reconcile catalog model bindings without writing to disk.
    pub fn reselect_provider_models_candidate(
        &self,
        provider_id: &str,
        selected_models: &[String],
    ) -> Result<Config> {
        let mut updated = self.config.clone();
        crate::catalog::reselect_catalog_model_bindings(
            &mut updated,
            provider_id,
            selected_models,
        )?;
        updated.validate()?;
        Ok(updated)
    }

    /// Reconcile catalog model bindings for an existing provider while preserving
    /// all non-catalog model metadata and unrelated provider bindings.
    pub fn reselect_provider_models(
        &mut self,
        provider_id: &str,
        selected_models: &[String],
    ) -> Result<()> {
        let updated = self.reselect_provider_models_candidate(provider_id, selected_models)?;
        let mut before = self.config.clone();
        let affected_models = crate::catalog::reselect_catalog_model_bindings(
            &mut before,
            provider_id,
            selected_models,
        )?;
        self.with_config_file_lock(|| {
            crate::config_edit::write_models(&self.config_path, &updated, &affected_models)
        })?;
        self.config = updated;
        Ok(())
    }

    /// Build a provider removal candidate without writing to disk.
    pub fn remove_provider_candidate(&self, id: &str, force: bool) -> Result<Config> {
        let mut updated = self.config.clone();
        let references = self.provider_references(id);
        let review_references = updated.codex_review_provider_references(id);
        if !review_references.is_empty() {
            bail!(
                "cannot remove provider {id:?}; it provides Codex auto-review bindings: {}",
                review_references.join(", ")
            );
        }
        if !references.is_empty() && !force {
            bail!(
                "provider {id:?} is still referenced by model bindings: {}; \
                 remove them with `model provider remove` first",
                references.join(", ")
            );
        }
        if !updated.providers.contains_key(id) {
            bail!("unknown provider {id:?}");
        }
        updated.providers.remove(id);
        if force {
            for model in updated.models.values_mut() {
                model.openai_chat_providers.retain(|b| b.name != id);
                model.openai_responses_providers.retain(|b| b.name != id);
                model.anthropic_providers.retain(|b| b.name != id);
            }
        }
        updated.validate()?;
        Ok(updated)
    }

    /// Build a provider copy candidate without writing to disk.
    pub fn copy_provider_candidate(
        &self,
        source_id: &str,
        target_id: &str,
        api_key_env: Option<String>,
        no_api_key: bool,
    ) -> Result<(Config, CopyProviderResult)> {
        let mut updated = self.config.clone();
        if api_key_env.is_some() && no_api_key {
            bail!("--api-key-env and --no-api-key are mutually exclusive");
        }
        if updated.providers.contains_key(target_id) {
            bail!("provider {target_id:?} already exists");
        }
        let mut provider = updated
            .providers
            .get(source_id)
            .with_context(|| format!("unknown provider {source_id:?}"))?
            .clone();
        let source_auth = provider.auth_config(source_id)?;
        let mut requires_oauth_login = false;
        match source_auth {
            AuthConfig::ApiKeyEnv { .. } => {
                if no_api_key {
                    provider.api_key_env = None;
                    provider.auth = Some(AuthConfig::None);
                } else {
                    let env = api_key_env.with_context(|| {
                        format!(
                            "provider copy for API-key provider {source_id:?} requires --api-key-env ENV"
                        )
                    })?;
                    provider.api_key_env = Some(env);
                    provider.auth = None;
                }
            }
            AuthConfig::OpenaiOauth { .. } => {
                provider.api_key_env = None;
                provider.auth = Some(AuthConfig::OpenaiOauth {
                    account: Some(target_id.to_string()),
                });
                requires_oauth_login = true;
            }
            AuthConfig::None => {
                if api_key_env.is_some() {
                    bail!("cannot add --api-key-env when copying no-auth provider {source_id:?}");
                }
                provider.api_key_env = None;
                provider.auth = Some(AuthConfig::None);
            }
        }
        updated.providers.insert(target_id.to_string(), provider);
        updated.validate()?;
        Ok((
            updated,
            CopyProviderResult {
                requires_oauth_login,
            },
        ))
    }

    /// Remove a provider by ID. Returns error if still referenced by models.
    pub fn remove_provider(&mut self, id: &str) -> Result<()> {
        let review_references = self.config.codex_review_provider_references(id);
        if !review_references.is_empty() {
            bail!(
                "cannot remove provider {id:?}; it provides Codex auto-review bindings: {}",
                review_references.join(", ")
            );
        }
        // Check model bindings reference this provider
        let references: Vec<String> = self
            .config
            .models
            .iter()
            .filter(|(_, m)| {
                m.openai_chat_providers.iter().any(|b| b.name == id)
                    || m.openai_responses_providers.iter().any(|b| b.name == id)
                    || m.anthropic_providers.iter().any(|b| b.name == id)
            })
            .map(|(model_id, _)| model_id.clone())
            .collect();

        if !references.is_empty() {
            bail!(
                "provider {id:?} is still referenced by model bindings: {}; \
                 remove them with `model provider remove` first",
                references.join(", ")
            );
        }

        if !self.config.providers.contains_key(id) {
            bail!("unknown provider {id:?}");
        }

        self.config.providers.remove(id);
        self.save_config()?;
        Ok(())
    }

    /// Force-remove a provider even if referenced by models.
    pub fn force_remove_provider(&mut self, id: &str) -> Result<()> {
        let review_references = self.config.codex_review_provider_references(id);
        if !review_references.is_empty() {
            bail!(
                "cannot force-remove provider {id:?}; it provides Codex auto-review bindings: {}",
                review_references.join(", ")
            );
        }
        if !self.config.providers.contains_key(id) {
            bail!("unknown provider {id:?}");
        }

        self.config.providers.remove(id);

        // Remove bindings from all models
        for model in self.config.models.values_mut() {
            model.openai_chat_providers.retain(|b| b.name != id);
            model.openai_responses_providers.retain(|b| b.name != id);
            model.anthropic_providers.retain(|b| b.name != id);
        }

        self.save_config()?;
        Ok(())
    }

    /// Remove a provider and clean up associated OAuth accounts.
    /// This is the full removal that connect.rs uses — checks references,
    /// removes provider, and logs out OAuth accounts.
    pub fn remove_provider_with_oauth(&mut self, id: &str, force: bool) -> Result<()> {
        // Check references first
        let references = self.provider_references(id);
        let review_references = self.config.codex_review_provider_references(id);
        if !review_references.is_empty() {
            bail!(
                "cannot remove provider {id:?}; it provides Codex auto-review bindings: {}",
                review_references.join(", ")
            );
        }
        if !references.is_empty() && !force {
            bail!(
                "provider {id:?} is still referenced by model bindings: {}; \
                 remove them with `model provider remove` first",
                references.join(", ")
            );
        }

        // Find OAuth account before removing
        let account = crate::auth::oauth_account_for_provider(&self.config, id).ok();

        if !self.config.providers.contains_key(id) {
            bail!("unknown provider {id:?}");
        }

        self.config.providers.remove(id);

        // Force-remove model bindings if needed
        if force && !references.is_empty() {
            for model in self.config.models.values_mut() {
                model.openai_chat_providers.retain(|b| b.name != id);
                model.openai_responses_providers.retain(|b| b.name != id);
                model.anthropic_providers.retain(|b| b.name != id);
            }
        }

        // Use round-trip editing to preserve unmanaged comments
        self.with_config_file_lock(|| {
            crate::config_edit::remove_provider(&self.config_path, &self.config, id)
        })?;

        // Clean up OAuth account. standalone 孪生：这里没有
        // 进程内凭据缓存可刷，但同样不能用 `let _ =` 吞掉失败——凭据没删掉却向用户
        // 报成功，留下的就是"配置里 provider 已消失、磁盘上账号仍在"的孤儿凭据，
        // 与崩溃落在两步之间的终态相同，只是成因从 crash 变成静默吞错。
        // 配置已落盘、无法回滚，所以把失败连同"哪一半已经生效"一起上报。
        if let Some(account) = account {
            let accounts_path = crate::auth::state_path_for_config(&self.config_path);
            if let Err(err) = crate::auth::logout(&accounts_path, Some(&account)) {
                tracing::error!(
                    provider = %id,
                    error = %err,
                    "provider removed but OAuth account cleanup failed"
                );
                anyhow::bail!(
                    "provider '{id}' was removed, but its OAuth account could not be deleted: \
                     {err}. The credential may still be on disk; run `llm-proxy provider logout \
                     {id}` once the credentials store is usable again."
                );
            }
        }

        Ok(())
    }

    /// Check which model bindings reference a provider.
    pub fn provider_references(&self, provider_id: &str) -> Vec<String> {
        let mut refs = Vec::new();
        for (model_id, model) in &self.config.models {
            for protocol in Protocol::CLIENT_PROTOCOLS {
                for binding in model.provider_bindings(protocol) {
                    if binding.name == provider_id {
                        refs.push(format!("{model_id}.{}", protocol.field_name()));
                    }
                }
            }
        }
        refs
    }

    /// Copy a provider with auth handling.
    pub fn copy_provider(
        &mut self,
        source_id: &str,
        target_id: &str,
        api_key_env: Option<String>,
        no_api_key: bool,
    ) -> Result<CopyProviderResult> {
        if api_key_env.is_some() && no_api_key {
            bail!("--api-key-env and --no-api-key are mutually exclusive");
        }
        if self.config.providers.contains_key(target_id) {
            bail!("provider {target_id:?} already exists");
        }

        let mut provider = self
            .config
            .providers
            .get(source_id)
            .with_context(|| format!("unknown provider {source_id:?}"))?
            .clone();

        let source_auth = provider.auth_config(source_id)?;
        let mut requires_oauth_login = false;

        match source_auth {
            AuthConfig::ApiKeyEnv { .. } => {
                if no_api_key {
                    provider.api_key_env = None;
                    provider.auth = Some(AuthConfig::None);
                } else {
                    let env = api_key_env.with_context(|| {
                        format!(
                            "provider copy for API-key provider {source_id:?} requires --api-key-env ENV"
                        )
                    })?;
                    provider.api_key_env = Some(env);
                    provider.auth = None;
                }
            }
            AuthConfig::OpenaiOauth { .. } => {
                provider.api_key_env = None;
                provider.auth = Some(AuthConfig::OpenaiOauth {
                    account: Some(target_id.to_string()),
                });
                requires_oauth_login = true;
            }
            AuthConfig::None => {
                if api_key_env.is_some() {
                    bail!("cannot add --api-key-env when copying no-auth provider {source_id:?}");
                }
                provider.api_key_env = None;
                provider.auth = Some(AuthConfig::None);
            }
        }

        self.config
            .providers
            .insert(target_id.to_string(), provider);
        // Use round-trip editing to preserve unmanaged comments
        self.with_config_file_lock(|| {
            crate::config_edit::write_provider(&self.config_path, &self.config, target_id)
        })?;

        Ok(CopyProviderResult {
            requires_oauth_login,
        })
    }

    /// Add a provider to the config.
    pub fn add_provider(&mut self, provider_id: &str, provider: ProviderConfig) -> Result<()> {
        self.config
            .providers
            .insert(provider_id.to_string(), provider);
        self.save_config()?;
        Ok(())
    }

    /// Add a provider and apply catalog model defaults.
    pub fn add_provider_with_models(
        &mut self,
        provider_id: &str,
        provider: ProviderConfig,
        selected_models: Option<&[String]>,
    ) -> Result<Vec<String>> {
        let existing_models = self
            .config
            .models
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();

        self.config
            .providers
            .insert(provider_id.to_string(), provider);

        // Apply catalog model defaults
        crate::catalog::apply_catalog_model_defaults(
            &mut self.config,
            provider_id,
            selected_models,
        )?;

        let inserted_models = self
            .config
            .models
            .keys()
            .filter(|m| !existing_models.contains(*m))
            .cloned()
            .collect::<Vec<_>>();

        self.config.validate()?;
        self.save_config()?;

        Ok(inserted_models)
    }

    /// Structured provider info for listing.
    pub fn list_providers(&self) -> Vec<ProviderListItem> {
        let mut items = Vec::new();
        for entry in crate::catalog::built_in_providers() {
            let state = if self.config.providers.contains_key(entry.id) {
                ProviderState::Configured
            } else {
                ProviderState::Available
            };
            let protocols = entry
                .provider
                .endpoints()
                .iter()
                .map(|(p, _)| p.route_key())
                .collect::<Vec<_>>()
                .join(",");
            let auth = match entry.provider.auth_config(entry.id) {
                Ok(AuthConfig::ApiKeyEnv { env }) => env,
                Ok(AuthConfig::OpenaiOauth { .. }) => "openai_oauth".to_string(),
                Ok(AuthConfig::None) | Err(_) => "none".to_string(),
            };
            let url = entry
                .provider
                .endpoints()
                .iter()
                .find_map(|(_, ep)| ep.url.as_deref())
                .unwrap_or("derived-only")
                .to_string();
            items.push(ProviderListItem {
                id: entry.id.to_string(),
                state,
                auth,
                protocols,
                url,
            });
        }
        items
    }

    /// Record token usage for a request. Single entry point for all usage recording.
    pub fn record_usage(
        &self,
        model: String,
        provider: String,
        endpoint: String,
        input_tokens: i64,
        output_tokens: i64,
        latency_ms: Option<i64>,
    ) {
        if let Some(store) = &self.usage_store {
            let record = UsageRecord::new(
                model,
                provider,
                endpoint,
                input_tokens,
                output_tokens,
                latency_ms,
            );
            if let Err(e) = store.record(record) {
                tracing::warn!("Failed to record usage: {}", e);
            }
        }
    }

    /// Query usage records with optional filters.
    pub fn query_usage(
        &self,
        start: Option<chrono::DateTime<chrono::Utc>>,
        end: Option<chrono::DateTime<chrono::Utc>>,
        provider: Option<&str>,
        model: Option<&str>,
        endpoint: Option<&str>,
    ) -> Vec<UsageRecord> {
        match &self.usage_store {
            Some(store) => store.query(start, end, provider, model, endpoint),
            None => Vec::new(),
        }
    }

    /// Execute an admin command. Single dispatch point for all operations.
    pub fn apply(&mut self, cmd: AdminCommand) -> Result<AdminResponse> {
        match cmd {
            AdminCommand::ProviderRemove { id, force } => {
                if force {
                    self.force_remove_provider(&id)?;
                } else {
                    self.remove_provider(&id)?;
                }
                Ok(AdminResponse::Ok {
                    message: format!("provider {id} removed"),
                })
            }
            AdminCommand::UsageQuery {
                period: _,
                provider,
                model,
                endpoint,
            } => {
                // Period parsing is done by the caller (CLI layer)
                let records = self.query_usage(
                    None,
                    None,
                    provider.as_deref(),
                    model.as_deref(),
                    endpoint.as_deref(),
                );
                Ok(AdminResponse::UsageRecords(records))
            }
            AdminCommand::Status => Ok(AdminResponse::StatusInfo {
                providers: self.config.providers.len(),
                models: self.config.models.len(),
            }),
            AdminCommand::ConfigReload => {
                let new_config = Config::load(&self.config_path)?;
                self.config = new_config;
                Ok(AdminResponse::Ok {
                    message: "config reloaded".to_string(),
                })
            }
        }
    }
}

// ── Admin Command Model ───────────────────────────────────────────────────────

/// Unified command model — CLI/TUI/Admin API all construct these.
#[derive(Debug, Clone)]
pub enum AdminCommand {
    // Provider management
    ProviderRemove {
        id: String,
        force: bool,
    },

    // Usage
    UsageQuery {
        period: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        endpoint: Option<String>,
    },

    // Status
    Status,
    ConfigReload,
}

/// Unified response from Core command execution.
#[derive(Debug)]
pub enum AdminResponse {
    Ok { message: String },
    UsageRecords(Vec<UsageRecord>),
    StatusInfo { providers: usize, models: usize },
}

// ── Provider Management Types ────────────────────────────────────────────────

/// Result of copying a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyProviderResult {
    pub requires_oauth_login: bool,
}

/// Structured provider info for listing.
#[derive(Debug, Clone)]
pub struct ProviderListItem {
    pub id: String,
    pub state: ProviderState,
    pub auth: String,
    pub protocols: String,
    pub url: String,
}

/// Whether a catalog provider is configured or just available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderState {
    Configured,
    Available,
}

// ── File Locking ──────────────────────────────────────────────────────────────

/// Advisory file lock for cross-process synchronization.
/// Uses flock(2) via fs2 crate. Lock is released on drop.
pub struct ConfigLock {
    file: File,
}

impl std::fmt::Debug for ConfigLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigLock").finish_non_exhaustive()
    }
}

impl ConfigLock {
    /// Acquire an exclusive lock, waiting up to `timeout`.
    pub fn acquire(state_dir: &Path, timeout: Duration) -> Result<Self> {
        crate::service::ensure_private_state_dir(state_dir)?;
        let lock_path = state_dir.join("config.lock");
        let file = File::create(&lock_path)
            .with_context(|| format!("failed to create lock file {}", lock_path.display()))?;
        let start = Instant::now();
        loop {
            if file.try_lock_exclusive().is_ok() {
                return Ok(Self { file });
            }
            if start.elapsed() > timeout {
                bail!(
                    "another process is holding the config lock (timeout: {}s)",
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        self.file.unlock().ok();
    }
}

// ── Server Detection ──────────────────────────────────────────────────────────

/// Information about a running server process.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerInfo {
    pub pid: u32,
    pub listen: String,
    pub started_at: String,
}

/// Detect if a local server is running for the given config.
/// Returns ServerInfo if server is running and responsive.
pub fn detect_local_server(state_dir: &Path, listen: &str) -> Option<ServerInfo> {
    let pid_path = state_dir.join("llm-proxy.pid");

    // 1. Read PID file
    let pid_str = fs::read_to_string(&pid_path).ok()?;
    let pid: u32 = pid_str.trim().parse().ok()?;

    // 2. Check process is alive
    if !process_alive(pid) {
        // Stale PID file — clean up
        let _ = fs::remove_file(&pid_path);
        return None;
    }

    // 3. Try admin ping
    let info = ServerInfo {
        pid,
        listen: listen.to_string(),
        started_at: String::new(),
    };

    // Quick HTTP check — if server responds, it's alive
    // We use a simple TCP connect check rather than full HTTP
    let addr = listen.parse::<std::net::SocketAddr>().ok()?;
    let timeout = Duration::from_millis(500);
    if std::net::TcpStream::connect_timeout(&addr, timeout).is_ok() {
        Some(info)
    } else {
        None
    }
}

/// Check if a process is alive by sending signal 0.
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle == 0 as _ {
                return false;
            }
            CloseHandle(handle);
            true
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
}

// ── Standalone Execution ──────────────────────────────────────────────────────

/// Execute a config mutation in standalone mode (no server running).
/// Acquires file lock, loads config, runs the mutation, saves, releases lock.
pub fn with_locked_config<T>(
    config_path: &Path,
    state_dir: &Path,
    f: impl FnOnce(&mut CoreState) -> Result<T>,
) -> Result<T> {
    let _lock = ConfigLock::acquire(state_dir, Duration::from_secs(5))?;
    let mut core = CoreState::load(config_path)?;
    let result = f(&mut core)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn config_lock_acquire_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let lock1 = ConfigLock::acquire(dir.path(), Duration::from_secs(1)).unwrap();
        drop(lock1);
        let lock2 = ConfigLock::acquire(dir.path(), Duration::from_secs(1)).unwrap();
        drop(lock2);
    }

    #[test]
    fn config_lock_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let _lock1 = ConfigLock::acquire(dir.path(), Duration::from_secs(1)).unwrap();
        let result = ConfigLock::acquire(dir.path(), Duration::from_millis(100));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("config lock"));
    }

    #[test]
    fn detect_no_server() {
        let dir = tempfile::tempdir().unwrap();
        let result = detect_local_server(dir.path(), "127.0.0.1:9999");
        assert!(result.is_none());
    }

    fn write_test_config(dir: &Path) -> PathBuf {
        let config_path = dir.join("config.toml");
        let config = crate::config::default_deepseek_config();
        crate::config_edit::write_full_config(&config_path, &config).unwrap();
        config_path
    }

    #[test]
    fn core_state_load_and_accessors() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let core = CoreState::load(&config_path).unwrap();
        assert!(core.config().providers.contains_key("deepseek"));
        assert_eq!(core.config().models.len(), 2);
        assert_eq!(core.config_path(), config_path);
    }

    #[test]
    fn core_apply_provider_remove() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();

        let cmd = AdminCommand::ProviderRemove {
            id: "deepseek".to_string(),
            force: false,
        };
        let result = core.apply(cmd);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("still referenced"));

        let cmd = AdminCommand::ProviderRemove {
            id: "deepseek".to_string(),
            force: true,
        };
        let result = core.apply(cmd).unwrap();
        match result {
            AdminResponse::Ok { message } => assert!(message.contains("removed")),
            _ => panic!("expected Ok response"),
        }
        assert!(!core.config().providers.contains_key("deepseek"));
    }

    #[test]
    fn core_apply_status() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();

        let cmd = AdminCommand::Status;
        let result = core.apply(cmd).unwrap();
        match result {
            AdminResponse::StatusInfo { providers, models } => {
                assert_eq!(providers, 1);
                assert_eq!(models, 2);
            }
            _ => panic!("expected StatusInfo response"),
        }
    }

    #[test]
    fn core_usage_store_uses_config_identity_shard_not_global_default() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let core = CoreState::load(&config_path).unwrap();
        let store = core.usage_store().expect("usage store");
        let shard = crate::service::StateDirs::resolve(dir.path())
            .state_dir
            .join("usage.db");
        assert_eq!(
            store.db_path(),
            &shard,
            "usage store must follow the config's own identity shard"
        );
        let global = crate::service::state_dir().join("usage.db");
        assert_ne!(
            store.db_path(),
            &global,
            "global default shard would mint an empty identity dir on every injected-config run"
        );
    }

    #[test]
    fn core_usage_store_available() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let core = CoreState::load(&config_path).unwrap();
        assert!(
            core.usage_store().is_some(),
            "usage store should be available"
        );

        core.record_usage(
            "test-model".to_string(),
            "test-provider".to_string(),
            "test_endpoint".to_string(),
            100,
            50,
            None,
        );
        let _records = core.query_usage(None, None, None, None, None);
    }

    #[test]
    fn with_locked_config_works() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let state_dir = dir.path();

        let result = with_locked_config(&config_path, state_dir, |core| {
            Ok(core.config().providers.len())
        })
        .unwrap();
        assert_eq!(result, 1);
    }

    #[test]
    fn core_copy_provider() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();

        let result = core
            .copy_provider(
                "deepseek",
                "deepseek-copy",
                Some("MY_KEY".to_string()),
                false,
            )
            .unwrap();
        assert!(!result.requires_oauth_login);
        assert!(core.config().providers.contains_key("deepseek-copy"));
        assert_eq!(
            core.config()
                .providers
                .get("deepseek-copy")
                .unwrap()
                .api_key_env
                .as_deref(),
            Some("MY_KEY")
        );
    }

    #[test]
    fn core_copy_provider_rejects_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();

        let result = core.copy_provider("deepseek", "deepseek", None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn core_provider_references() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let core = CoreState::load(&config_path).unwrap();

        let refs = core.provider_references("deepseek");
        assert!(!refs.is_empty(), "deepseek should be referenced by models");
        assert!(refs.iter().any(|r| r.contains("deepseek-v4-flash-lp")));

        let refs = core.provider_references("nonexistent");
        assert!(refs.is_empty());
    }

    #[test]
    fn core_list_providers() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let core = CoreState::load(&config_path).unwrap();

        let providers = core.list_providers();
        assert!(!providers.is_empty());

        let deepseek = providers.iter().find(|p| p.id == "deepseek").unwrap();
        assert_eq!(deepseek.state, ProviderState::Configured);

        let openai = providers.iter().find(|p| p.id == "openai-payg").unwrap();
        assert_eq!(openai.state, ProviderState::Available);
    }

    #[test]
    fn core_add_provider() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();

        let mut provider = ProviderConfig {
            api_key_env: Some("TEST_KEY".to_string()),
            ..ProviderConfig::default()
        };
        provider.set_endpoint(
            crate::config::Protocol::OpenaiChatCompletions,
            crate::config::EndpointConfig::native("https://example.com/v1/chat/completions"),
        );
        core.add_provider("test-provider", provider).unwrap();
        assert!(core.config().providers.contains_key("test-provider"));
    }

    #[test]
    fn core_from_config_accessors_and_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let config = crate::config::default_deepseek_config();
        let state_dirs = crate::service::StateDirs::for_test(runtime_dir.path());
        let mut core =
            CoreState::from_config_with_state_dirs(config, config_path.clone(), state_dirs);

        assert_eq!(core.config_path(), config_path);
        assert_eq!(core.runtime_dir(), runtime_dir.path().join("runtime"));
        core.config_mut().server.as_mut().unwrap().listen = "127.0.0.1:19090".to_string();
        assert_eq!(
            core.config().server.as_ref().unwrap().listen,
            "127.0.0.1:19090"
        );
    }

    #[test]
    fn core_from_config_with_external_usage_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::default_deepseek_config();
        let usage = crate::usage_stats::UsageStore::with_paths(
            config.server.as_ref().unwrap().usage.clone(),
            dir.path().join("usage.jsonl"),
            dir.path().join("usage.db"),
        )
        .unwrap();
        let core = CoreState::from_config_with_usage_store(
            config,
            dir.path().join("config.toml"),
            Some(Arc::new(usage)),
        );

        core.record_usage("m".into(), "p".into(), "e".into(), 3, 4, Some(12));
        let records = core.query_usage(None, None, Some("p"), Some("m"), Some("e"));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].total_tokens, 7);
        assert_eq!(records[0].latency_ms, Some(12));
    }

    #[test]
    fn core_query_usage_returns_empty_without_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::default_deepseek_config();
        let core =
            CoreState::from_config_with_usage_store(config, dir.path().join("config.toml"), None);

        core.record_usage("m".into(), "p".into(), "e".into(), 1, 2, None);
        assert!(core.usage_store().is_none());
        assert!(core.query_usage(None, None, None, None, None).is_empty());
    }

    #[test]
    fn core_usage_store_arc_getter() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::default_deepseek_config();
        let without_store = CoreState::from_config_with_usage_store(
            config.clone(),
            dir.path().join("config1.toml"),
            None,
        );
        assert!(without_store.usage_store_arc().is_none());

        let usage = Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                config.server.as_ref().unwrap().usage.clone(),
                dir.path().join("usage.jsonl"),
                dir.path().join("usage.db"),
            )
            .unwrap(),
        );
        let with_store = CoreState::from_config_with_usage_store(
            config,
            dir.path().join("config2.toml"),
            Some(usage),
        );
        assert!(with_store.usage_store_arc().is_some());
    }

    #[test]
    fn core_query_usage_filters_independently() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::default_deepseek_config();
        let usage = crate::usage_stats::UsageStore::with_paths(
            config.server.as_ref().unwrap().usage.clone(),
            dir.path().join("usage.jsonl"),
            dir.path().join("usage.db"),
        )
        .unwrap();
        let core = CoreState::from_config_with_usage_store(
            config,
            dir.path().join("config.toml"),
            Some(Arc::new(usage)),
        );

        core.record_usage("m1".into(), "p1".into(), "chat".into(), 1, 1, None);
        core.record_usage("m2".into(), "p1".into(), "responses".into(), 2, 2, None);
        core.record_usage("m1".into(), "p2".into(), "chat".into(), 3, 3, None);

        assert_eq!(
            core.query_usage(None, None, Some("p1"), None, None).len(),
            2
        );
        assert_eq!(
            core.query_usage(None, None, None, Some("m1"), None).len(),
            2
        );
        assert_eq!(
            core.query_usage(None, None, None, None, Some("chat")).len(),
            2
        );
        let only = core.query_usage(None, None, Some("p1"), Some("m1"), Some("chat"));
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].input_tokens, 1);
    }

    #[test]
    fn core_apply_usage_query_filters() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::default_deepseek_config();
        let usage = crate::usage_stats::UsageStore::with_paths(
            config.server.as_ref().unwrap().usage.clone(),
            dir.path().join("usage.jsonl"),
            dir.path().join("usage.db"),
        )
        .unwrap();
        let mut core = CoreState::from_config_with_usage_store(
            config,
            dir.path().join("config.toml"),
            Some(Arc::new(usage)),
        );
        core.record_usage("keep".into(), "p".into(), "chat".into(), 1, 2, None);
        core.record_usage("drop".into(), "p".into(), "chat".into(), 1, 2, None);

        let result = core
            .apply(AdminCommand::UsageQuery {
                period: Some("ignored-by-core".to_string()),
                provider: Some("p".to_string()),
                model: Some("keep".to_string()),
                endpoint: Some("chat".to_string()),
            })
            .unwrap();
        match result {
            AdminResponse::UsageRecords(records) => {
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].model, "keep");
            }
            _ => panic!("expected UsageRecords"),
        }
    }

    #[test]
    fn core_update_full_config_validates_before_mutating_memory() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();
        let original_len = core.config().providers.len();
        let mut invalid = core.config().clone();
        invalid.providers.clear();

        let err = core.update_full_config(&invalid).unwrap_err().to_string();
        assert!(err.contains("unknown provider") || err.contains("provider"));
        assert_eq!(core.config().providers.len(), original_len);
    }

    #[test]
    fn core_reload_config_refreshes_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();
        let mut changed = core.config().clone();
        changed.server.as_mut().unwrap().listen = "127.0.0.1:19191".to_string();
        crate::config_edit::write_full_config(&config_path, &changed).unwrap();

        core.reload_config().unwrap();
        assert_eq!(
            core.config().server.as_ref().unwrap().listen,
            "127.0.0.1:19191"
        );
    }

    #[test]
    fn core_apply_config_reload_reports_ok_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();
        let mut changed = core.config().clone();
        changed.server.as_mut().unwrap().listen = "127.0.0.1:19192".to_string();
        crate::config_edit::write_full_config(&config_path, &changed).unwrap();

        let result = core.apply(AdminCommand::ConfigReload).unwrap();
        match result {
            AdminResponse::Ok { message } => assert_eq!(message, "config reloaded"),
            _ => panic!("expected Ok"),
        }
        assert_eq!(
            core.config().server.as_ref().unwrap().listen,
            "127.0.0.1:19192"
        );
    }

    #[test]
    fn core_force_remove_provider_cleans_all_protocol_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();

        core.force_remove_provider("deepseek").unwrap();
        assert!(!core.config().providers.contains_key("deepseek"));
        for model in core.config().models.values() {
            assert!(
                model
                    .openai_chat_providers
                    .iter()
                    .all(|b| b.name != "deepseek")
            );
            assert!(
                model
                    .openai_responses_providers
                    .iter()
                    .all(|b| b.name != "deepseek")
            );
            assert!(
                model
                    .anthropic_providers
                    .iter()
                    .all(|b| b.name != "deepseek")
            );
        }
    }

    #[test]
    fn core_force_remove_provider_preserves_codex_review_reference() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();
        let mut cfg = core.config().clone();
        cfg.codex.auto_review_model = Some("deepseek-v4-flash-lp".to_string());
        core.update_full_config(&cfg).unwrap();

        let err = core
            .force_remove_provider("deepseek")
            .expect_err("review provider removal must be blocked")
            .to_string();
        assert!(err.contains("Codex auto-review bindings"));
        assert!(core.config().providers.contains_key("deepseek"));
    }

    #[test]
    fn core_remove_provider_unknown_errors() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();

        let err = core.remove_provider("missing").unwrap_err().to_string();
        assert!(err.contains("unknown provider"));
        let err = core
            .force_remove_provider("missing")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown provider"));
    }

    #[test]
    fn core_remove_provider_with_oauth_reports_cleanup_failure() {
        // standalone 孪生回归基线：`remove_provider_with_oauth` 不能
        // 用 `let _ =` 吞掉凭据删除失败——配置已落盘无法回滚，必须把失败连同
        // "哪一半已生效"一起上报。修复前该场景静默返回 Ok。
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut cfg: crate::config::Config = toml::from_str(
            "[server]\nlisten = \"127.0.0.1:0\"\n\n\
             [providers.ag1]\n\
             auth = { type = \"openai_oauth\", account = \"ag1\" }\n\
             [providers.ag1.openai_chat]\nurl = \"https://example.test/v1/chat\"\n",
        )
        .unwrap();
        cfg.models.clear();
        crate::config_edit::write_full_config(&config_path, &cfg).unwrap();
        // Make the credentials path a directory: any load inside logout fails.
        std::fs::create_dir(crate::auth::state_path_for_config(&config_path)).unwrap();

        let mut core = CoreState::load(&config_path).unwrap();
        let err = core
            .remove_provider_with_oauth("ag1", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("was removed"), "unexpected error: {err}");
        assert!(
            err.contains("could not be deleted"),
            "unexpected error: {err}"
        );
        // The config half did take effect and cannot be rolled back.
        assert!(!core.config().providers.contains_key("ag1"));
    }

    #[test]
    fn core_copy_provider_no_api_key_sets_no_auth() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();

        let result = core
            .copy_provider("deepseek", "deepseek-noauth", None, true)
            .unwrap();
        assert!(!result.requires_oauth_login);
        let provider = core.config().providers.get("deepseek-noauth").unwrap();
        assert!(provider.api_key_env.is_none());
        assert_eq!(provider.auth, Some(AuthConfig::None));
    }

    #[test]
    fn core_copy_provider_rejects_conflicting_auth_options() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let mut core = CoreState::load(&config_path).unwrap();

        let err = core
            .copy_provider("deepseek", "copy", Some("KEY".to_string()), true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mutually exclusive"));
        assert!(!core.config().providers.contains_key("copy"));
    }

    #[test]
    fn config_lock_creates_missing_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("missing").join("state");
        let lock = ConfigLock::acquire(&nested, Duration::from_secs(1)).unwrap();
        assert!(nested.join("config.lock").exists());
        drop(lock);
    }

    #[test]
    fn detect_local_server_removes_stale_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("llm-proxy.pid");
        fs::write(&pid_path, "999999999").unwrap();

        assert!(detect_local_server(dir.path(), "127.0.0.1:9").is_none());
        assert!(!pid_path.exists());
    }

    #[test]
    fn detect_local_server_ignores_invalid_pid_and_listen() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("llm-proxy.pid"), "not-a-pid").unwrap();
        assert!(detect_local_server(dir.path(), "not-an-addr").is_none());
    }

    // =========================================================================
    // core.rs simple function tests
    // =========================================================================

    #[test]
    fn test_load_with_usage_store_shares_store() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        // Create an external usage store
        let external_store = Arc::new(
            UsageStore::with_paths(
                crate::config::default_deepseek_config()
                    .server
                    .as_ref()
                    .unwrap()
                    .usage
                    .clone(),
                dir.path().join("external-usage.jsonl"),
                dir.path().join("external-usage.db"),
            )
            .unwrap(),
        );
        // Load with the external store
        let core =
            CoreState::load_with_usage_store(&config_path, Some(Arc::clone(&external_store)))
                .unwrap();
        // The core should use the external store, not create its own
        assert!(core.usage_store().is_some());
        // Record via core, verify via external store
        core.record_usage("m".into(), "p".into(), "e".into(), 5, 10, None);
        let records = external_store.get_by_period(None, None);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].input_tokens, 5);
    }

    #[test]
    fn test_from_config_with_usage_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::default_deepseek_config();
        let external_store = Arc::new(
            UsageStore::with_paths(
                config.server.as_ref().unwrap().usage.clone(),
                dir.path().join("shared.jsonl"),
                dir.path().join("shared.db"),
            )
            .unwrap(),
        );
        let core = CoreState::from_config_with_usage_store(
            config,
            dir.path().join("config.toml"),
            Some(Arc::clone(&external_store)),
        );
        // Verify the store is shared
        assert!(core.usage_store().is_some());
        core.record_usage(
            "test-m".into(),
            "test-p".into(),
            "test-e".into(),
            3,
            7,
            Some(42),
        );
        let records = core.query_usage(None, None, Some("test-p"), None, None);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].total_tokens, 10);
        assert_eq!(records[0].latency_ms, Some(42));
    }

    #[test]
    fn test_config_mut_returns_mutable_ref() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::default_deepseek_config();
        let original_listen = config.server.as_ref().unwrap().listen.clone();
        let mut core = CoreState::from_config(config, dir.path().join("config.toml"));
        // Verify config_mut returns a mutable reference we can modify
        let new_listen = "127.0.0.1:29999".to_string();
        core.config_mut().server.as_mut().unwrap().listen = new_listen.clone();
        assert_eq!(core.config().server.as_ref().unwrap().listen, new_listen);
        assert_ne!(
            core.config().server.as_ref().unwrap().listen,
            original_listen
        );
    }
}
