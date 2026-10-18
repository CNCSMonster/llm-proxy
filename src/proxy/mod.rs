use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use axum::extract::Extension;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::Instrument;

use crate::config::{AdapterKind, Config, ExecutionPlan, Protocol};
use crate::usage_stats::{UsageRecord, UsageStore};
use crate::{convert, cooldown::CooldownStore, protection::BadRequestManager};

mod auth;
mod chat_responses_stream;
mod forward_anthropic;
mod forward_chat;
mod forward_responses;
mod request;
pub(crate) mod sse_aggregate;

use auth::*;
use forward_anthropic::*;
use forward_chat::*;
use forward_responses::*;
use request::*;
use sse_aggregate::*;

/// writer 任务在**关闭结论已产出之后**允许退出的宽限。
///
/// 结论通道与任务退出是两个独立信号；结论已定时任务理应毫秒级结束，超过这个宽限仍
/// 没退出，就只能报"不可判定"——继续无界等待等于把挂起伪装成正在关闭。
const CONFIG_WRITER_TASK_JOIN_BUDGET: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct AppState {
    generation: crate::runtime_config::GenerationReader,
    pub(crate) config_path: PathBuf,
    pub(crate) state_dirs: crate::service::StateDirs,
    #[cfg(test)]
    pub(crate) state_dir: PathBuf,
    config_writer: crate::runtime_config::ConfigWriterHandle,
    config_writer_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    fatal_config_state: crate::runtime_config::FatalConfigState,
    client: reqwest::Client,
    pub(crate) oauth_accounts_path: PathBuf,
    pub(crate) cooldowns: Arc<CooldownStore>,
    // OAuth 账号内存缓存（§15.2）：server 启动时加载，
    // 转发从内存取 token（不每次读文件）；login/refresh 委托更新缓存 + 落盘。
    // std::sync::RwLock：转发路径是同步 fn（resolve_plan_auth），刷新在
    // block_in_place 内同步更新缓存（短锁，可接受）。
    pub(crate) oauth_accounts: Arc<std::sync::RwLock<crate::auth::AccountsFile>>,
    // OAuth 刷新合并锁：并发请求同时刷新同一账号时串行化 + double-check
    // （§15.2：避免重复刷新触发上游限流）。
    // 用 tokio Mutex：guard 需跨 await 持有（刷新是 async），std MutexGuard 跨 await 会使 Future 不 Send。
    pub(crate) oauth_refresh_lock: Arc<tokio::sync::Mutex<()>>,
    oauth_token_urls: OAuthTokenUrls,
    bad_requests: Arc<BadRequestManager>,
    /// Per-token error-class cooldown: when a token
    /// accumulates repeated errors of the same class, ALL requests from
    /// that token are blocked for a cooldown period.
    pub(crate) error_cooldown: Arc<crate::protection::TokenErrorClassCooldown>,
    /// Per-account cumulative bad-request budget (daily + lifetime layers).
    /// When a token accumulates protectable upstream 4xx errors, both counters
    /// increment; exceeding either limit blocks all requests locally.
    account_budget: Arc<crate::protection::AccountBadBudgetManager>,
    stream_interruptions: Arc<std::sync::Mutex<BTreeMap<String, StreamInterruptionEntry>>>,
    frequency: Arc<tokio::sync::Mutex<FrequencyState>>,
    // RoundRobin cursor for provider selection.
    // Key: model_id, Value: cursor index for rotating candidates.
    round_robin_cursors: Arc<std::sync::Mutex<HashMap<String, usize>>>,
    usage_store: Option<Arc<UsageStore>>,
    // preflight/4xx 诊断记录（metadata-only 默认，opt-in full）。
    // 记录失败只 log，绝不影响请求路径。
    diagnostics: Option<Arc<crate::diagnostics::DiagnosticsStore>>,
    // 活跃 provider 记录（§12.3）：成功转发的 provider + 最近时间戳，
    // 作为"活的证据"供 status 跳过冗余探活。唯一状态源 ActiveProviderStore，
    // 与 ServerProbeState 共享同一 Arc（避免双状态源 bug）。
    pub(crate) active_providers: Arc<crate::probe_coordinator::ActiveProviderStore>,
    // 探活 singleflight（§19.6）：使用 ProbeCoordinator 实现真正的 Singleflight。
    pub(crate) probe_coordinator:
        Arc<crate::probe_coordinator::ProbeCoordinator<crate::probe_coordinator::ServerProbeState>>,
    // tokio::sync::Mutex (no poisoning) + spawn_blocking in admin handlers so
    // config file I/O never blocks a tokio worker thread.
    pub(crate) core: Arc<tokio::sync::Mutex<crate::core::CoreState>>,
    // Token-based access control state: loaded from tokens.toml,
    // shared by the auth middleware (check) and record_usage (quota charge).
    // pub(crate) so admin handlers can reload it after
    // server-side token mutations keep memory/disk consistent.
    pub(crate) token: crate::token::auth::TokenAuthState,
    /// Per-token rate limiter for the /v1/token/usage/history endpoint.
    usage_history_limiter: Arc<UsageHistoryRateLimiter>,
    /// Streaming vs non-streaming request metrics .
    /// 8 atomic counters: 2 categories × 4 dimensions. No persistence.
    streaming_metrics: Arc<crate::streaming_metrics::StreamingMetrics>,
    /// Per-provider×model request metrics.
    /// In-memory counters + sampling ring buffers. No persistence.
    provider_metrics: Arc<crate::provider_metrics::ProviderModelMetrics>,
    #[cfg(test)]
    test_dir: Option<Arc<tempfile::TempDir>>,
}

#[derive(Clone)]
struct OAuthTokenUrls {
    openai: String,
}

impl Default for OAuthTokenUrls {
    fn default() -> Self {
        Self {
            openai: crate::auth::OPENAI_OAUTH_TOKEN_URL.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct StreamInterruptionEntry {
    direction: String,
    count: u64,
    last_error: String,
    last_seen_unix: u64,
}

#[derive(Debug, Default)]
struct FrequencyState {
    attempts: HashMap<String, AttemptWindows>,
    buckets: HashMap<String, BurstBucket>,
}

/// Per-provider attempt bookkeeping for the frequency window.
///
/// `recent` holds attempts younger than 60s (drives `requests_per_minute`);
/// `older` holds attempts in [60s, 1h) (counted only toward
/// `requests_per_hour`). Both deques are sorted ascending, so each attempt is
/// moved at most once and popped at most once: window maintenance is
/// amortized O(1) per request instead of scanning the whole hour window.
#[derive(Debug, Default)]
struct AttemptWindows {
    recent: VecDeque<Instant>,
    older: VecDeque<Instant>,
}

impl AttemptWindows {
    fn prune(&mut self, now: Instant) {
        while self
            .recent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            if let Some(t) = self.recent.pop_front() {
                self.older.push_back(t);
            }
        }
        while self
            .older
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(3600))
        {
            self.older.pop_front();
        }
    }

    fn counts(&self) -> (u32, u32) {
        let recent = self.recent.len() as u32;
        (recent, recent + self.older.len() as u32)
    }
}

#[derive(Debug, Clone)]
struct BurstBucket {
    tokens: f64,
    last_refill: Instant,
}

#[derive(Debug)]
enum UpstreamSendFailure {
    Transport(reqwest::Error),
    /// Response headers never arrived within the TTFB budget. Surfaced as its
    /// own kind: streaming sends dispatch without a reqwest total timeout,
    /// so a header-stage deadline produces no reqwest error.
    TimedOut,
    FrequencyLimited,
}

impl std::fmt::Display for UpstreamSendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamSendFailure::Transport(err) => write!(f, "{err}"),
            UpstreamSendFailure::TimedOut => {
                write!(f, "upstream request timed out before response headers")
            }
            UpstreamSendFailure::FrequencyLimited => write!(f, "provider frequency limit exceeded"),
        }
    }
}

impl AppState {
    pub(crate) fn config(&self) -> Config {
        self.generation.load().config().clone()
    }

    /// Reads config and its revision from the *same* generation load so the
    /// value can be used as a commit base without a torn read.
    pub(crate) fn config_with_revision(&self) -> (Config, u64) {
        let generation = self.generation.load();
        (generation.config().clone(), generation.revision())
    }

    pub(crate) fn current_revision(&self) -> u64 {
        self.generation.load().revision()
    }

    pub(crate) fn fatal_config_state(&self) -> crate::runtime_config::FatalConfigState {
        self.fatal_config_state.clone()
    }

    /// Commits a whole-config candidate against the `base` revision it was
    /// derived from. A stale base is rejected with `CommitBaseConflict`
    /// instead of silently overwriting a newer generation.
    pub(crate) async fn commit_config(
        &self,
        cfg: Config,
        base: u64,
    ) -> Result<u64, crate::runtime_config::WriterError> {
        let (_, revision) = self
            .config_writer
            .commit(
                crate::runtime_config::ClientOperationKey(uuid::Uuid::new_v4().as_u128()),
                cfg,
                base,
            )
            .await?;
        Ok(revision)
    }

    pub(crate) async fn mutate_config(
        &self,
        mutation: crate::model::ConfigMutation,
    ) -> Result<String, crate::runtime_config::WriterError> {
        let (_, _, message) = self
            .config_writer
            .mutate(
                crate::runtime_config::ClientOperationKey(uuid::Uuid::new_v4().as_u128()),
                mutation,
            )
            .await?;
        Ok(message)
    }

    pub(crate) async fn reload_config(&self) -> Result<(), crate::runtime_config::WriterError> {
        self.config_writer
            .reload(crate::runtime_config::ClientOperationKey(
                uuid::Uuid::new_v4().as_u128(),
            ))
            .await?;
        // Token configuration is managed independently of the config writer (no UDS
        // delegation endpoint), but shares this reload trigger (§6).
        // Supports both tokens/ directory and legacy tokens.toml via load_auto().
        // A malformed token config keeps the previous good config and is surfaced
        // in the log rather than silently exposing the service.
        if let Err(err) = self.token.reload(&self.config_path) {
            tracing::error!(error = %err, "token configuration reload failed; keeping previous token config");
        } else if let Ok(tokens_cfg) = self.token.config.read() {
            self.cooldowns.reconcile(&tokens_cfg);
        }
        self.cooldowns.validate_against_config(&self.config());
        if let Some(store) = &self.usage_store {
            let cfg = self.config();
            let bindings = crate::usage_stats::extract_model_bindings(&cfg);
            let config_hash = crate::usage_stats::compute_bindings_hash(&bindings);
            if let Err(e) = store.sync_model_binding_revisions(&bindings, &config_hash) {
                tracing::warn!("Failed to sync model binding revisions on reload: {}", e);
            }
        }
        Ok(())
    }

    /// Complete a connect whose second write step was lost: an OAuth credential
    /// reached disk, the provider referencing it did not. Rebuild the provider
    /// from its catalog template and commit it.
    ///
    /// Runs only at the startup boundary, where this process cannot have an
    /// in-flight connect of its own. The direction is *completion*, never
    /// reclamation: a credential is the expensive half of the pair (only the
    /// user can obtain it) and is never deleted here.
    pub(crate) async fn recover_orphan_oauth_providers(&self) {
        let Ok(accounts) =
            crate::auth::load_oauth_accounts_with_recovery(&self.oauth_accounts_path)
        else {
            return;
        };
        let (mut cfg, base) = self.config_with_revision();
        let report = crate::auth::recover_orphan_oauth_accounts(&mut cfg, &accounts);
        if report.is_empty() {
            return;
        }

        for recovery in &report.recovered {
            tracing::warn!(
                provider = %recovery.provider,
                account = %recovery.account.account,
                "rebuilt provider from an OAuth credential left behind by an interrupted connect; \
                 the model selection is not recoverable — run `llm-proxy provider reselect {} --models <id>`",
                recovery.provider,
            );
        }
        for orphan in &report.unrecoverable {
            tracing::warn!(
                auth_type = orphan.auth_type,
                account = %orphan.account,
                "OAuth credential references no catalog provider, so nothing was rebuilt; \
                 add back the provider that uses this account key to keep using it, or remove \
                 the account from the OAuth credential file (`provider logout` needs an existing provider)",
            );
        }
        if report.recovered.is_empty() {
            return;
        }

        match self.commit_config(cfg, base).await {
            Ok(revision) => {
                tracing::info!(
                    revision,
                    "persisted providers recovered from orphan OAuth credentials"
                );
            }
            Err(err) => tracing::error!(
                error = ?err,
                "recovered providers were not persisted; the credentials stay on disk and this \
                 reconciliation retries at next startup",
            ),
        }
    }

    pub(crate) fn new(cfg: Config, config_path: PathBuf) -> Self {
        // `recover_orphan_oauth_providers` 按 config_path 提交配置，所以测试一旦绑到默认
        // 路径，跑测试就会把用户真实的 config.toml 覆盖成测试夹具（listen 也被改回夹具值）。
        #[cfg(test)]
        assert_ne!(
            config_path,
            crate::config::default_config_path(),
            "tests must not open the live user config path; pass an isolated temp config path"
        );
        let bad_request_cfg = cfg.protection.bad_request.clone();
        let oauth_accounts_path = crate::auth::state_path_for_config(&config_path);
        let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
        let dirs = crate::service::StateDirs::resolve(config_dir);

        // Initialize usage store（spec §12.6 单一实例：record 与 query 共享同一内存权威）
        let usage_store = cfg
            .server
            .as_ref()
            .map(|s| s.usage.clone())
            .and_then(|usage| UsageStore::new_with_state_dir(usage, &dirs.state_dir).ok())
            .map(Arc::new);

        if let Some(store) = &usage_store {
            let bindings = crate::usage_stats::extract_model_bindings(&cfg);
            let config_hash = crate::usage_stats::compute_bindings_hash(&bindings);
            if let Err(e) = store.sync_model_binding_revisions(&bindings, &config_hash) {
                tracing::warn!("Failed to sync model binding revisions: {}", e);
            }
        }

        // 诊断落盘（state_dir/diagnostics.jsonl，总量封顶）
        let diagnostics = crate::diagnostics::DiagnosticsStore::with_paths(
            cfg.diagnostics.clone(),
            dirs.state_dir.join("diagnostics.jsonl"),
        )
        .ok()
        .map(Arc::new);

        // Initialize CoreState from config path（必须与 CLI 传入的 --config 一致，
        #[cfg(test)]
        let core = if TEST_APP_STATE_HOOK
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
        {
            crate::core::CoreState::from_config_with_usage_store(
                cfg.clone(),
                config_path.clone(),
                usage_store.clone(),
            )
        } else {
            crate::core::CoreState::load_with_usage_store(&config_path, usage_store.clone())
                .unwrap_or_else(|_| {
                    crate::core::CoreState::from_config_with_usage_store(
                        cfg.clone(),
                        config_path.clone(),
                        usage_store.clone(),
                    )
                })
        };
        #[cfg(not(test))]
        let core = crate::core::CoreState::load_with_usage_store(&config_path, usage_store.clone())
            .unwrap_or_else(|_| {
                crate::core::CoreState::from_config_with_usage_store(
                    cfg.clone(),
                    config_path.clone(),
                    usage_store.clone(),
                )
            });

        // Single source of truth for provider liveness: shared with ServerProbeState
        let active_store = Arc::new(crate::probe_coordinator::ActiveProviderStore::new());
        // Token access-control state: tokens.toml sibling of
        // config_path + quota store on tokens.db in the shared state dir.
        let token = crate::token::auth::TokenAuthState::load(&config_path, &dirs.state_dir);
        let generation = crate::runtime_config::GenerationReader::new(core.config().clone());
        let runtime_dir = core.runtime_dir().to_path_buf();
        let core = Arc::new(tokio::sync::Mutex::new(core));
        let fatal_config_state = crate::runtime_config::FatalConfigState::default();
        #[cfg(test)]
        let app_state_dir = runtime_dir.clone();
        let (config_writer, config_writer_task) =
            crate::runtime_config::ConfigWriterHandle::start_for_current_runtime(
                uuid::Uuid::new_v4().as_u128(),
                256,
                generation.clone(),
                core.clone(),
                config_path.clone(),
                runtime_dir,
                fatal_config_state.clone(),
            );

        let cooldowns = {
            let tokens_cfg = token.config.read().ok();
            let store = CooldownStore::load(dirs.state_dir.join("cooldowns.json"))
                .unwrap_or_else(|_| CooldownStore::in_memory());
            if let Some(tokens) = tokens_cfg.as_ref() {
                store.reconcile(tokens);
            }
            store.validate_against_config(&cfg);
            Arc::new(store)
        };
        let this = Self {
            generation,
            config_path: config_path.clone(),
            state_dirs: dirs.clone(),
            #[cfg(test)]
            state_dir: app_state_dir,
            config_writer,
            config_writer_task: Arc::new(tokio::sync::Mutex::new(config_writer_task)),
            fatal_config_state,
            client: crate::http_client::build(&cfg),
            oauth_accounts_path: oauth_accounts_path.clone(),
            cooldowns: cooldowns.clone(),
            // OAuth 账号内存缓存：启动时加载（损坏时用空账号，转发时按需刷新）
            oauth_accounts: Arc::new(std::sync::RwLock::new(
                crate::auth::load_oauth_accounts_with_recovery(&oauth_accounts_path)
                    .unwrap_or_default(),
            )),
            oauth_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            oauth_token_urls: OAuthTokenUrls::default(),
            bad_requests: Arc::new(BadRequestManager::new(bad_request_cfg)),
            error_cooldown: Arc::new(crate::protection::TokenErrorClassCooldown::load(
                cfg.protection.error_cooldown.clone(),
                cfg.protection.local_reject.clone(),
                &dirs.state_dir.join("token-error-cooldowns.json"),
            )),
            account_budget: {
                let budget_db = dirs.state_dir.join("bad_request_budget.db");
                Arc::new(
                    crate::protection::AccountBadBudgetManager::open(
                        &budget_db,
                        cfg.protection.bad_request_budget.clone(),
                    )
                    .unwrap_or_else(|e| {
                        tracing::error!(error = %e, "failed to open bad-request budget store; using in-memory fallback");
                        crate::protection::AccountBadBudgetManager::in_memory_fallback(
                            cfg.protection.bad_request_budget.clone(),
                        )
                    }),
                )
            },
            stream_interruptions: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            frequency: Arc::new(tokio::sync::Mutex::new(FrequencyState::default())),
            round_robin_cursors: Arc::new(std::sync::Mutex::new(HashMap::new())),
            usage_store,
            diagnostics,
            active_providers: active_store.clone(),
            probe_coordinator: {
                let probe_state =
                    crate::probe_coordinator::ServerProbeState::with_cache_path_and_oauth_path(
                        active_store,
                        crate::status::cache_path_in(&dirs.state_dir),
                        oauth_accounts_path.clone(),
                    )
                    .with_cooldown_store(cooldowns.clone());
                Arc::new(crate::probe_coordinator::ProbeCoordinator::new(probe_state))
            },
            core,
            token,
            usage_history_limiter: Arc::new(UsageHistoryRateLimiter::new()),
            streaming_metrics: Arc::new(crate::streaming_metrics::StreamingMetrics::new()),
            provider_metrics: {
                let metrics_db_path = dirs.state_dir.join("metrics.db");
                let metrics_store = crate::provider_metrics::MetricsStore::open(&metrics_db_path)
                    .map(Arc::new)
                    .ok();
                match metrics_store {
                    Some(store) => Arc::new(
                        crate::provider_metrics::ProviderModelMetrics::with_store(store),
                    ),
                    None => Arc::new(crate::provider_metrics::ProviderModelMetrics::new()),
                }
            },
            #[cfg(test)]
            test_dir: None,
        };
        #[cfg(test)]
        if let Ok(mut guard) = TEST_APP_STATE_HOOK.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(this.clone());
        }
        this
    }

    #[cfg(test)]
    fn new_in_memory(cfg: Config) -> Self {
        Self::new_in_memory_for_management(cfg)
    }

    #[cfg(test)]
    pub(crate) fn new_in_memory_for_management(cfg: Config) -> Self {
        let test_dir = Arc::new(tempfile::tempdir().expect("create isolated app state directory"));
        let bad_request_cfg = cfg.protection.bad_request.clone();
        let config_path = test_dir.path().join("config.toml");
        let oauth_accounts_path = crate::auth::state_path_for_config(&config_path);
        let test_dirs = crate::service::StateDirs::for_test(test_dir.path());
        let core = crate::core::CoreState::from_config_with_state_dirs(
            cfg.clone(),
            config_path.clone(),
            test_dirs.clone(),
        );
        // Single source of truth for provider liveness: shared with ServerProbeState
        let active_store = Arc::new(crate::probe_coordinator::ActiveProviderStore::new());
        let generation = crate::runtime_config::GenerationReader::new(core.config().clone());
        let runtime_dir = core.runtime_dir().to_path_buf();
        let core = Arc::new(tokio::sync::Mutex::new(core));
        let fatal_config_state = crate::runtime_config::FatalConfigState::default();
        let diagnostics_jsonl_path = test_dirs.state_dir.join("diagnostics.jsonl");
        let cooldowns = Arc::new(CooldownStore::in_memory());
        let (config_writer, config_writer_task) =
            crate::runtime_config::ConfigWriterHandle::start_for_current_runtime(
                uuid::Uuid::new_v4().as_u128(),
                256,
                generation.clone(),
                core.clone(),
                config_path.clone(),
                runtime_dir.clone(),
                fatal_config_state.clone(),
            );
        Self {
            generation,
            config_path,
            state_dirs: test_dirs,
            state_dir: runtime_dir.clone(),
            config_writer,
            config_writer_task: Arc::new(tokio::sync::Mutex::new(config_writer_task)),
            fatal_config_state,
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            oauth_accounts_path: oauth_accounts_path.clone(),
            cooldowns: cooldowns.clone(),
            oauth_accounts: Arc::new(std::sync::RwLock::new(
                crate::auth::load_oauth_accounts_with_recovery(&oauth_accounts_path)
                    .unwrap_or_default(),
            )),
            oauth_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            oauth_token_urls: OAuthTokenUrls::default(),
            bad_requests: Arc::new(BadRequestManager::new(bad_request_cfg)),
            error_cooldown: Arc::new(crate::protection::TokenErrorClassCooldown::load(
                cfg.protection.error_cooldown.clone(),
                cfg.protection.local_reject.clone(),
                &runtime_dir.join("token-error-cooldowns.json"),
            )),
            account_budget: {
                let budget_db = runtime_dir.join("bad_request_budget.db");
                Arc::new(
                    crate::protection::AccountBadBudgetManager::open(
                        &budget_db,
                        cfg.protection.bad_request_budget.clone(),
                    )
                    .unwrap_or_else(|e| {
                        tracing::error!(error = %e, "failed to open bad-request budget store; using in-memory fallback");
                        crate::protection::AccountBadBudgetManager::in_memory_fallback(
                            cfg.protection.bad_request_budget.clone(),
                        )
                    }),
                )
            },
            stream_interruptions: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            frequency: Arc::new(tokio::sync::Mutex::new(FrequencyState::default())),
            round_robin_cursors: Arc::new(std::sync::Mutex::new(HashMap::new())),
            usage_store: None,
            diagnostics: crate::diagnostics::DiagnosticsStore::with_paths(
                cfg.diagnostics.clone(),
                diagnostics_jsonl_path,
            )
            .ok()
            .map(Arc::new),
            active_providers: active_store.clone(),
            probe_coordinator: {
                let probe_state =
                    crate::probe_coordinator::ServerProbeState::with_cache_path_and_oauth_path(
                        active_store,
                        test_dir.path().join("status-cache.json"),
                        oauth_accounts_path.clone(),
                    )
                    .with_cooldown_store(cooldowns.clone());
                Arc::new(crate::probe_coordinator::ProbeCoordinator::new(probe_state))
            },
            core,
            token: crate::token::auth::TokenAuthState::new(
                crate::token::config::TokensConfig::default(),
            ),
            usage_history_limiter: Arc::new(UsageHistoryRateLimiter::new()),
            streaming_metrics: Arc::new(crate::streaming_metrics::StreamingMetrics::new()),
            provider_metrics: Arc::new(crate::provider_metrics::ProviderModelMetrics::new()),
            test_dir: Some(test_dir),
        }
    }

    #[cfg(test)]
    pub(crate) fn use_oauth_token_urls_for_test(&mut self, openai: impl Into<String>) {
        self.oauth_token_urls = OAuthTokenUrls {
            openai: openai.into(),
        };
    }

    #[cfg(test)]
    pub(crate) fn set_usage_store_for_test(&mut self, store: Arc<UsageStore>) {
        let cfg = self.config();
        let bindings = crate::usage_stats::extract_model_bindings(&cfg);
        let config_hash = crate::usage_stats::compute_bindings_hash(&bindings);
        let _ = store.sync_model_binding_revisions(&bindings, &config_hash);
        if let Ok(mut core) = self.core.try_lock() {
            core.set_usage_store(Some(Arc::clone(&store)));
        }
        self.usage_store = Some(store);
    }

    pub(crate) fn diagnostics_store(&self) -> Option<&Arc<crate::diagnostics::DiagnosticsStore>> {
        self.diagnostics.as_ref()
    }

    /// Per-provider×model metrics snapshot for `/admin/metrics`.
    pub(crate) fn provider_metrics_snapshot(&self) -> crate::provider_metrics::MetricsSnapshot {
        self.provider_metrics.snapshot()
    }

    /// Per-provider×model metrics snapshot for a specific window.
    pub(crate) fn provider_metrics_snapshot_window(
        &self,
        window: crate::provider_metrics::WindowKind,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> crate::provider_metrics::MetricsSnapshot {
        self.provider_metrics
            .snapshot_window(window, provider, model)
    }

    /// Access metrics store if persistence is configured.
    pub(crate) fn provider_metrics_store(
        &self,
    ) -> Option<std::sync::Arc<crate::provider_metrics::MetricsStore>> {
        self.provider_metrics.store()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_metrics_store_for_test(
        &self,
        store: std::sync::Arc<crate::provider_metrics::MetricsStore>,
    ) {
        self.provider_metrics.set_store(store);
    }

    /// Rotate candidates using RoundRobin cursor.
    ///
    /// Returns a new vector with candidates rotated so that the next candidate
    /// to try is at index 0. The cursor is incremented atomically.
    ///
    /// Example: candidates = [A, B, C], cursor = 1 → returns [B, C, A]
    pub(crate) fn rotate_candidates_round_robin(
        &self,
        model_id: &str,
        candidates: Vec<(String, ExecutionPlan)>,
    ) -> Vec<(String, ExecutionPlan)> {
        if candidates.len() <= 1 {
            return candidates;
        }
        let mut cursors = self
            .round_robin_cursors
            .lock()
            .expect("round_robin_cursors lock poisoned");
        let cursor = cursors.entry(model_id.to_string()).or_insert(0);
        let offset = *cursor % candidates.len();
        *cursor = cursor.wrapping_add(1);
        drop(cursors);

        if offset == 0 {
            return candidates;
        }
        let mut rotated = candidates;
        rotated.rotate_left(offset);
        rotated
    }

    /// 关闭配置写入器，并把**可判定的关闭终态**返回给调用方。
    ///
    /// 两个阶段各自都有预算：结论通道可能永远不产出（协调任务被卡住），writer 任务
    /// 也可能永远不退出。任一阶段超时都按"不可判定"上报，禁止靠"没报错就是成功"推出
    /// 干净关闭。返回 `Err` 的调用方必须让进程以非 0 退出码收尾。
    pub(crate) async fn shutdown_config_writer(
        &self,
    ) -> Result<(), crate::runtime_config::WriterError> {
        let conclusion = self.config_writer.shutdown().await;
        if let Err(error) = &conclusion {
            if matches!(
                error,
                crate::runtime_config::WriterError::StopUndetermined
                    | crate::runtime_config::WriterError::MailboxClosed
            ) {
                // 「不可判定」必须是独立事件名，不能和干净关闭共用 service_stop 的语义。
                tracing::error!(
                    event = "stop_undetermined",
                    ?error,
                    "配置写入器关闭结论取不到，按失败处理（不得视为干净关闭）"
                );
            } else {
                tracing::error!(error = ?error, "failed to shut down config writer");
            }
        }
        let task = self.config_writer_task.lock().await.take();
        if let Some(task) = task {
            match tokio::time::timeout(CONFIG_WRITER_TASK_JOIN_BUDGET, task).await {
                Err(_elapsed) => {
                    tracing::error!(
                        event = "stop_undetermined",
                        budget_ms = CONFIG_WRITER_TASK_JOIN_BUDGET.as_millis() as u64,
                        "配置写入器任务在预算内未退出，关闭结论不可判定"
                    );
                    return Err(crate::runtime_config::WriterError::StopUndetermined);
                }
                Ok(Err(error)) => {
                    tracing::error!(error = ?error, "config writer task failed while shutting down");
                }
                Ok(Ok(_)) => {}
            }
        }
        conclusion
    }

    /// graceful shutdown 收尾——把 pending usage 记录落盘。
    ///
    /// `record` 只入队（内存权威），后台 `spawn_flush_task` 最长一个落盘周期
    /// （低频 180s）才写盘；关停序列缺这一步会让短会话整窗账目丢失（连
    /// 终态 `status` 一起）。flush 失败只记 error 不改变关闭结论：
    /// 丢账已经发生，重试无从谈起。
    pub(crate) fn flush_usage_on_shutdown(&self) {
        if let Some(store) = &self.usage_store
            && let Err(error) = store.flush_if_dirty()
        {
            tracing::error!(
                target: "llm_proxy::usage",
                event = "usage_shutdown_flush_failed",
                error = %error,
                "pending usage records were dropped during shutdown flush"
            );
        }
    }

    pub(crate) fn flush_metrics_on_shutdown(&self) {
        if let Some(store) = self.provider_metrics_store() {
            let snapshot = self.provider_metrics_snapshot();
            if !snapshot.metrics.is_empty()
                && let Err(error) = store.write_snapshot(&snapshot)
            {
                tracing::error!(
                    target: "llm_proxy::metrics",
                    event = "metrics_shutdown_flush_failed",
                    error = %error,
                    "failed to persist metrics snapshot during shutdown flush"
                );
            }
        }
    }

    // Signature fixed by §3.3/§4.3 (usage record + token attribution +
    // quota charge in one place); the per-request token is the 7th field.
    // : `is_streaming` and `response_bytes` drive the streaming
    // metrics counters (8 AtomicU64, no persistence).
    // : `ttfb_ms` captures time-to-first-byte for latency attribution.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    fn record_usage(
        &self,
        model: String,
        provider: String,
        endpoint: String,
        input_tokens: i64,
        output_tokens: i64,
        latency_ms: Option<i64>,
        token: Option<&crate::token::auth::TokenContext>,
        is_streaming: bool,
        response_bytes: u64,
        ttfb_ms: Option<u64>,
    ) {
        self.record_usage_with_status(
            model,
            provider,
            endpoint,
            input_tokens,
            output_tokens,
            latency_ms,
            token,
            Some("completed"),
            is_streaming,
            response_bytes,
            false,
            None,
            ttfb_ms,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_usage_with_target(
        &self,
        model: String,
        provider: String,
        endpoint: String,
        input_tokens: i64,
        output_tokens: i64,
        latency_ms: Option<i64>,
        token: Option<&crate::token::auth::TokenContext>,
        is_streaming: bool,
        response_bytes: u64,
        ttfb_ms: Option<u64>,
        routed_target: Option<String>,
    ) {
        self.record_usage_with_status_and_target(
            model,
            provider,
            endpoint,
            input_tokens,
            output_tokens,
            latency_ms,
            None,
            None,
            token,
            Some("completed"),
            is_streaming,
            response_bytes,
            false,
            None,
            ttfb_ms,
            routed_target,
        );
    }

    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    fn record_usage_with_status(
        &self,
        model: String,
        provider: String,
        endpoint: String,
        input_tokens: i64,
        output_tokens: i64,
        latency_ms: Option<i64>,
        token: Option<&crate::token::auth::TokenContext>,
        status: Option<&str>,
        is_streaming: bool,
        response_bytes: u64,
        truncated: bool,
        truncation_reason: Option<&str>,
        ttfb_ms: Option<u64>,
    ) {
        self.record_usage_with_status_and_target(
            model,
            provider,
            endpoint,
            input_tokens,
            output_tokens,
            latency_ms,
            None,
            None,
            token,
            status,
            is_streaming,
            response_bytes,
            truncated,
            truncation_reason,
            ttfb_ms,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_usage_with_status_and_target(
        &self,
        model: String,
        provider: String,
        endpoint: String,
        input_tokens: i64,
        output_tokens: i64,
        latency_ms: Option<i64>,
        cache_hit_tokens: Option<i64>,
        cache_miss_tokens: Option<i64>,
        token: Option<&crate::token::auth::TokenContext>,
        status: Option<&str>,
        is_streaming: bool,
        response_bytes: u64,
        truncated: bool,
        truncation_reason: Option<&str>,
        ttfb_ms: Option<u64>,
        routed_target: Option<String>,
    ) {
        // 成功转发 = 活跃证据（§12.3），供 status 跳过冗余探活
        if status.is_none() || status == Some("completed") {
            self.mark_provider_active(&provider);
        }
        // : per-provider×model metrics (counters + sampling ring buffers).
        // Recorded before UsageRecord::new() which moves `provider`.
        self.provider_metrics.record(
            &provider,
            &model,
            status,
            ttfb_ms,
            latency_ms,
            output_tokens,
        );
        if let Some(store) = &self.usage_store {
            let mut record = UsageRecord::new(
                model.clone(),
                provider,
                endpoint,
                input_tokens,
                output_tokens,
                latency_ms,
            )
            .with_token(token.map(|t| t.key.clone()), token.map(|t| t.name.clone()))
            .with_status(status.map(str::to_string))
            .with_truncation(truncated, truncation_reason.map(str::to_string))
            .with_ttfb(ttfb_ms);
            if let Some(target) = routed_target {
                record = record.with_cache_tokens(cache_hit_tokens, cache_miss_tokens);
                record = record.with_routed_target(target);
            }
            if let Err(e) = store.record(record) {
                tracing::warn!("Failed to record usage: {}", e);
            }
        }
        // Token quota accounting (§4.3): charge windows with the
        // ratio-weighted cost. Best-effort inside charge().
        if let Some(ctx) = token {
            self.token.charge(ctx, &model, input_tokens, output_tokens);
        }
        // : streaming metrics (8 atomic counters, tracing event).
        self.streaming_metrics
            .record(is_streaming, response_bytes, input_tokens, output_tokens);
    }

    /// Spend one request credit for `token` at the point of upstream dispatch.
    ///
    /// Called immediately before the first upstream send of a client request —
    /// the only point at which the proxy knows the request will actually be
    /// issued. Locally rejected requests never reach it, and it runs once per
    /// client request so internal fallback does not charge twice.
    /// Returns the `429` to answer with when a window is exhausted.
    ///
    /// The `model` parameter enables model-scoped quota groups :
    /// only the groups matching this model are checked and charged.
    fn reserve_token_request(
        &self,
        token: &crate::token::auth::TokenContext,
        model: &str,
    ) -> Option<Response> {
        // Limits come from configuration, usage comes from the store. A missing
        // usage row means "0 used" — it must never mean "no limit" — so anything
        // that leaves the limits, or the usage behind them, undetermined is
        // refused rather than waved through as unlimited.
        let grouped_windows = match self.token.config.read() {
            Ok(config) => {
                match config
                    .tokens
                    .iter()
                    .find(|candidate| candidate.key == token.key)
                {
                    Some(candidate) => {
                        crate::token::usage::grouped_windows_for_model(candidate, model)
                    }
                    None => {
                        return Some(
                            self.undetermined_quota(token, "token absent from configuration"),
                        );
                    }
                }
            }
            Err(_) => {
                return Some(self.undetermined_quota(token, "token configuration lock poisoned"));
            }
        };

        // An explicitly empty window set is the configured "unlimited" (§5.1).
        if grouped_windows.is_empty() {
            return None;
        }

        let Some(store) = self.token.usage_store.as_ref() else {
            return Some(self.undetermined_quota(token, "quota store unavailable"));
        };

        match store.reserve_request_slot_grouped(&token.key, &grouped_windows) {
            Ok(crate::token::usage::RequestSlot::Granted) => None,
            Ok(crate::token::usage::RequestSlot::Exhausted(windows)) => {
                tracing::debug!(name = %token.name, model = %model, "quota exhausted at dispatch");
                Some(crate::token::auth::quota_exhausted_response(&windows))
            }
            Err(error) => Some(self.undetermined_quota(token, &error.to_string())),
        }
    }

    /// Refuse a request whose quota cannot be determined.
    ///
    /// `503` rather than `429` keeps the failure honest: the caller's quota is
    /// not known to be exhausted, the proxy simply cannot honour the metering
    /// contract it promised — and letting the request through would hand out
    /// unmetered access.
    fn undetermined_quota(
        &self,
        token: &crate::token::auth::TokenContext,
        reason: &str,
    ) -> Response {
        tracing::error!(
            name = %token.name,
            reason,
            "quota could not be determined; refusing the request"
        );
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": {
                    "code": "quota_unavailable",
                    "message": "token quota could not be determined; refusing the request",
                    "type": "token_quota_unavailable",
                }
            })),
        )
            .into_response()
    }

    /// 记录一次成功转发，作为该 provider 的活跃证据（§12.3）。
    /// 统一走 ActiveProviderStore（唯一状态源，与 ServerProbeState 共享）。
    pub(crate) fn mark_provider_active(&self, provider_id: &str) {
        self.active_providers.mark_active(provider_id);
    }

    /// 返回 TTL 窗口内活跃的 provider id 列表（§12.3/12.9）。
    pub(crate) fn get_active_providers(&self, ttl: Duration) -> Vec<String> {
        self.active_providers.active_providers(ttl)
    }

    /// 判断 provider 在 TTL 窗口内是否活跃（§12.3）。
    #[cfg(test)]
    pub(crate) fn is_provider_active(&self, provider_id: &str, ttl: Duration) -> bool {
        self.active_providers.is_active(provider_id, ttl)
    }

    /// Polls for a frequency slot up to `queue_timeout_seconds`. Used inside an
    /// in-flight send sequence (retry pacing): the sequence head itself never
    /// waits — a failed head acquire surfaces `FrequencyLimited` to the
    /// candidate loop, which skips fast and owns the shared wait budget.
    async fn acquire_frequency(&self, plan: &ExecutionPlan) -> bool {
        let timeout =
            Duration::from_secs(plan.request_frequency.queue_timeout_seconds.unwrap_or(10));
        let deadline = Instant::now() + timeout;
        loop {
            if self.try_acquire_frequency_now(plan).await {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn try_acquire_frequency_now(&self, plan: &ExecutionPlan) -> bool {
        let now = Instant::now();
        let mut state = self.frequency.lock().await;
        let provider_id = plan.provider_id.clone();
        let (minute_count, hour_count) = {
            let windows = state.attempts.entry(provider_id.clone()).or_default();
            windows.prune(now);
            windows.counts()
        };
        let burst = plan.request_frequency.burst.unwrap_or(5).max(1) as f64;
        let minute_limit = plan
            .request_frequency
            .requests_per_minute
            .unwrap_or(60)
            .max(1);
        if minute_count >= minute_limit {
            return false;
        }
        if let Some(hour_limit) = plan.request_frequency.requests_per_hour
            && hour_count >= hour_limit
        {
            return false;
        }

        let bucket = state
            .buckets
            .entry(provider_id.clone())
            .or_insert(BurstBucket {
                tokens: burst,
                last_refill: now,
            });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        let refill_per_second = minute_limit as f64 / 60.0;
        bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(burst);
        bucket.last_refill = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        let windows = state.attempts.entry(provider_id).or_default();
        windows.recent.push_back(now);
        true
    }

    fn is_cooling_down(&self, key: &crate::config::CooldownKey) -> bool {
        self.cooldowns.is_cooling_down(key)
    }

    fn active_cooldown(
        &self,
        key: &crate::config::CooldownKey,
    ) -> Option<crate::cooldown::CooldownEntry> {
        self.cooldowns.active_entry(key)
    }

    fn set_cooldown(
        &self,
        key: &crate::config::CooldownKey,
        kind: &str,
        reason: &str,
        duration: Duration,
    ) {
        self.cooldowns.set(key, kind, reason, duration);
    }

    /// Tier-1: upstream 4xx on the egress path, with
    /// request features and Google `fieldViolations` when the upstream sent them.
    #[allow(clippy::too_many_arguments)] // one diagnostic needs plan + ingress context + upstream reply; mirrors the forward twins
    #[allow(dead_code)]
    fn record_upstream_client_error(
        &self,
        plan: &ExecutionPlan,
        frontend_model: &str,
        stream: bool,
        token: Option<&crate::token::auth::TokenContext>,
        body: &Value,
        status: StatusCode,
        raw_text: &str,
        message: &str,
    ) {
        // Error-class cooldown: record the error for the requesting
        // (token × inbound model) entry. Runs regardless of diagnostics being enabled.
        self.record_client_error_for_token(token, frontend_model, status, raw_text, message);

        let Some(store) = &self.diagnostics else {
            return;
        };
        let record = crate::diagnostics::DiagnosticRecord::new(
            crate::diagnostics::EVENT_UPSTREAM_CLIENT_ERROR,
            crate::diagnostics::RequestFeatures::from_opaque_body(body),
        );
        let record = if let Ok(error_json) = serde_json::from_str::<Value>(raw_text) {
            record.with_details(
                crate::diagnostics::extract_google_field_violations(&error_json),
                crate::diagnostics::extract_upstream_detail_types(&error_json),
            )
        } else {
            record
        };
        let mut record = self.fill_plan_fields(record, plan, frontend_model, stream, token);
        record.upstream_status = Some(status.as_u16());
        record.upstream_message = Some(crate::diagnostics::bounded_message(message, 2048));
        record.body_candidate = Some(body.to_string());
        store.record(record);
    }

    /// Record a local auth failure (OAuth refresh failure) that triggered fallback.
    /// The detailed error message is already in tracing logs (auth/refresh.rs);
    /// this record captures which provider/model/account failed and that fallback occurred.
    #[allow(dead_code)]
    fn record_local_auth_failure(
        &self,
        plan: &ExecutionPlan,
        frontend_model: &str,
        stream: bool,
        token: Option<&crate::token::auth::TokenContext>,
    ) {
        let Some(store) = &self.diagnostics else {
            return;
        };
        // Local auth failure has no request body to analyze (auth failed before egress);
        // use an empty opaque body for the RequestFeatures.
        let empty_body = serde_json::json!({});
        let record = crate::diagnostics::DiagnosticRecord::new(
            crate::diagnostics::EVENT_LOCAL_AUTH_FAILURE,
            crate::diagnostics::RequestFeatures::from_opaque_body(&empty_body),
        );
        let record = self.fill_plan_fields(record, plan, frontend_model, stream, token);
        store.record(record);
    }

    fn fill_plan_fields(
        &self,
        mut record: crate::diagnostics::DiagnosticRecord,
        plan: &ExecutionPlan,
        frontend_model: &str,
        stream: bool,
        token: Option<&crate::token::auth::TokenContext>,
    ) -> crate::diagnostics::DiagnosticRecord {
        record.provider = plan.provider_id.clone();
        record.upstream_model = plan.upstream_model.clone();
        record.frontend_model = frontend_model.to_string();
        record.protocol = plan.frontend_protocol.route_key().to_string();
        record.adapter = format!("{:?}", plan.adapter());
        record.stream = stream;
        record.token = token.map(|token| token.name.clone());
        record
    }

    fn bad_request_blocked(&self, fingerprint: &str) -> bool {
        self.bad_requests.check(fingerprint).blocked
    }

    fn observe_bad_request(&self, fingerprint: &str) {
        self.bad_requests.observe_client_error(fingerprint);
    }

    /// Check if a (token, inbound model) entry is in cooldown (error-class or local-reject).
    ///
    /// Every rejection leaves one INFO line — the block path used to fail
    /// silently, which made cooldown incidents invisible in the logs.
    fn check_token_error_cooldown(
        &self,
        token_name: &str,
        inbound_model: &str,
    ) -> crate::protection::BlockStatus {
        let status = self.error_cooldown.is_blocked(token_name, inbound_model);
        if status.blocked {
            tracing::warn!(
                token = %token_name,
                model = %inbound_model,
                remaining_seconds = status.remaining_seconds,
                reason = status.reason.as_deref().unwrap_or("error_class"),
                "token error-class cooldown active; request rejected"
            );
        }
        status
    }

    #[allow(dead_code)]
    pub(crate) fn token_error_cooldown_blocked(
        &self,
        token_name: &str,
        inbound_model: &str,
    ) -> bool {
        self.check_token_error_cooldown(token_name, inbound_model)
            .blocked
    }

    /// Record an error-class hit for a (token, inbound model) entry.
    fn record_token_error_class(&self, token_name: &str, inbound_model: &str, error_class: &str) {
        self.error_cooldown
            .record_error(token_name, inbound_model, error_class);
    }

    /// Record a local rejection for a (token, inbound model) entry.
    fn record_local_rejection_for_token(
        &self,
        token_name: &str,
        inbound_model: &str,
        reject_reason: &str,
    ) {
        self.error_cooldown
            .record_local_reject(token_name, inbound_model, reject_reason);
    }

    /// Check if a token is blocked by the cumulative bad-request budget.
    /// Returns the row state alongside the decision so blocked-path logs can
    /// self-describe (daily_date vs today is the decisive field when auditing
    /// why a lock happened).
    fn account_budget_blocked(
        &self,
        token_key: &str,
    ) -> (
        crate::protection::BudgetBlockReason,
        crate::protection::BudgetCheckDiagnostic,
    ) {
        self.account_budget.check_with_diagnostic(token_key)
    }

    /// Record a protectable upstream error for the per-account budget.
    fn record_account_budget_error(&self, token_key: &str, token_name: &str) {
        self.account_budget.record_error(token_key, token_name);
    }

    /// Snapshot of per-account budget state (for admin query).
    pub fn account_budget_snapshot(&self) -> Vec<crate::protection::AccountBudgetSnapshot> {
        self.account_budget.snapshot()
    }

    /// Reset the lifetime budget counter for a token (admin operation).
    pub fn account_budget_reset(&self, token_key: &str) -> anyhow::Result<()> {
        self.account_budget.reset_lifetime(token_key)
    }

    /// Record an upstream client error against the requesting (token, inbound
    /// model) entry.
    ///
    /// For Google-RPC-style bodies, 400 responses are further classified:
    /// rate-limit/quota errors (RESOURCE_EXHAUSTED, "rate limit", "quota", etc.)
    /// are NOT counted toward the bad-request budget — they are normal operational
    /// limits that should trigger short cooldown + fallback instead.
    pub(super) fn record_client_error_for_token(
        &self,
        token: Option<&crate::token::auth::TokenContext>,
        inbound_model: &str,
        status: StatusCode,
        raw_text: &str,
        message: &str,
    ) {
        if !status.is_client_error() {
            return;
        }
        if let Some(token_ctx) = token {
            // Failure-path observability: every upstream 4xx relayed to the
            // client leaves one structured line (truncated message, token
            // display name only — never the key). This fires for ALL 4xx,
            // counted or not — incident forensics rely on it.
            let token_display = if token_ctx.name.is_empty() {
                crate::protection::redact_token_key(&token_ctx.key)
            } else {
                token_ctx.name.clone()
            };
            let message_head: String = message.chars().take(300).collect();

            // Unified client-fault gate: BOTH protection ledgers (error-class
            // cooldown and bad-request budget) share this single predicate so
            // they can never drift apart. Credential failures (401/403),
            // operational rate limits (429, rate-limit-class 400s) and
            // provider-side account/catalog/network conditions (402 billing,
            // 404 model not deployed here, 408 timeout) are never the token's
            // fault — each has its own dedicated cooldown path and none of them
            // is evidence of a bad request. Counting is per client request, not
            // per candidate attempt: every status that survives this gate is
            // terminal in the candidate loop, so one failed request contributes
            // at most one count and an attempt rescued by fallback contributes
            // none.
            let rate_limit_400 = status == StatusCode::BAD_REQUEST
                && crate::protection::is_google_rpc_rate_limit_400(raw_text);
            let counted = crate::proxy::sse_aggregate::is_protectable_client_error(status)
                && !rate_limit_400
                && !crate::proxy::sse_aggregate::has_dedicated_cooldown_path(status);
            let exclude = if counted {
                "-"
            } else if rate_limit_400 {
                "rate_limit_400"
            } else {
                "dedicated_cooldown_path"
            };
            tracing::info!(
                token = %token_display,
                model = %inbound_model,
                status = status.as_u16(),
                message = %message_head,
                counted,
                exclude = %exclude,
                "upstream client error relayed"
            );

            if !counted {
                return;
            }
            let error_class = crate::protection::extract_error_class(
                message,
                &crate::diagnostics::extract_upstream_detail_types(
                    &serde_json::from_str::<Value>(raw_text).unwrap_or_default(),
                ),
            );
            self.record_token_error_class(&token_ctx.name, inbound_model, &error_class);
            self.record_account_budget_error(&token_ctx.key, &token_ctx.name);
        }
    }

    fn observe_stream_interruption(&self, direction: &str, err: &str) {
        // Counter update lives in a block so the MutexGuard is released *before*
        // anything is logged: tracing must never run while this map lock is held.
        let count = {
            let Ok(mut entries) = self.stream_interruptions.lock() else {
                return;
            };
            let entry =
                entries
                    .entry(direction.to_string())
                    .or_insert_with(|| StreamInterruptionEntry {
                        direction: direction.to_string(),
                        count: 0,
                        last_error: String::new(),
                        last_seen_unix: 0,
                    });
            entry.count += 1;
            entry.last_error = err.to_string();
            entry.last_seen_unix = (unix_millis() / 1000) as u64;
            entry.count
        };

        // Until now this in-memory counter was the *only* trace a
        // mid-stream upstream failure left — cleared on restart and visible only via
        // the management `/state` channel, which is why the issue's symptom read
        // "proxy error log has zero records".
        //
        // `error!` rather than `warn!`: keep the *event severity* — an upstream
        // stream break is a fault by definition, not a notice.
        //
        // 历史理由已变：这里原写「`EnvFilter::from_default_env()` 缺省停在
        // ERROR，写 warn 与没有这行对运维等价」。缺省 filter 现为 `error,llm_proxy=info`
        //（见 `main.rs::DEFAULT_ENV_FILTER`），我方 target 开到 info、warn 已可见——
        // 保留 `error!` 的依据因此是**事件本身是 error**，不再是「不写 error 就看不见」。
        // 断言 `level=ERROR` 不变，guarded by
        // `proxy::tests::stream_interruption_leaves_error_log_with_counter`.
        //
        // `error` is truncated: upstream error strings can carry full URLs and the log
        // line must not become a leak surface of its own.
        tracing::error!(
            event = "stream_interrupted",
            direction,
            count,
            error = %err.chars().take(300).collect::<String>(),
            "upstream stream interrupted; downstream received no protocol terminator"
        );
    }

    fn stream_interruption_snapshot(&self) -> Vec<StreamInterruptionEntry> {
        self.stream_interruptions
            .lock()
            .map(|entries| entries.values().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn management_state_json(&self) -> Value {
        json!({
            "bad_request_blocks": self.bad_requests.snapshot(),
            "stream_interruptions": self.stream_interruption_snapshot()
        })
    }
}

#[allow(dead_code)]
pub async fn serve(cfg: Config) -> Result<()> {
    let (_tx, rx) = tokio::sync::oneshot::channel();
    serve_with_shutdown(cfg, rx).await.map(|_| ())
}

pub async fn serve_with_shutdown(
    cfg: Config,
    shutdown_rx: tokio::sync::oneshot::Receiver<&'static str>,
) -> Result<&'static str> {
    serve_with_shutdown_at(cfg, crate::config::default_config_path(), shutdown_rx).await
}

/// [`serve_with_shutdown`] with an explicit config path.
///
/// The config writer persists to `config_path`, so tests must use this form with
/// a temp path; the default-path wrapper would let a test rewrite the live user
/// config.
pub(crate) async fn serve_with_shutdown_at(
    cfg: Config,
    config_path: PathBuf,
    shutdown_rx: tokio::sync::oneshot::Receiver<&'static str>,
) -> Result<&'static str> {
    let state = AppState::new(cfg, config_path);
    let result = serve_state_with_shutdown(state.clone(), shutdown_rx).await;
    state.flush_usage_on_shutdown();
    state.flush_metrics_on_shutdown();
    let writer_result = state.shutdown_config_writer().await;
    // 与 `service::run_foreground` 同口径：writer 没给出干净终态就不是成功关闭。
    let exit_reason = result?;
    writer_result.map_err(|error| anyhow::anyhow!("配置写入器未给出干净关闭结论（{error:?}）"))?;
    Ok(exit_reason)
}

pub(crate) async fn serve_state_with_shutdown(
    state: AppState,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<&'static str>,
) -> Result<&'static str> {
    let cfg = state.config();
    cfg.validate()?;
    // spec §12.6.3 智能落盘：后台任务按请求频率分档定期把内存 usage 落盘
    if let Some(store) = &state.usage_store {
        let _flush_handle =
            crate::usage_stats::UsageStore::spawn_flush_task(store.as_ref().clone());
    }
    // 定期快照持久化：后台任务定期把内存 metrics 快照落盘
    if let Some(store) = state.provider_metrics_store() {
        let metrics = state.provider_metrics.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.tick().await;
            let mut tick_count: u64 = 0;
            loop {
                interval.tick().await;
                tick_count += 1;
                let snapshot = metrics.snapshot();
                if !snapshot.metrics.is_empty()
                    && let Err(e) = store.write_snapshot(&snapshot)
                {
                    tracing::warn!("failed to persist metrics snapshot: {e}");
                }
                if tick_count.is_multiple_of(60)
                    && let Err(e) = store.cleanup_old_snapshots(
                        crate::provider_metrics::DEFAULT_METRICS_RETENTION_DAYS,
                    )
                {
                    tracing::warn!("failed to clean up old metrics snapshots: {e}");
                }
            }
        });
    }
    // OAuth 账号缺失/损坏不阻塞启动（非 OAuth provider 仍可用），降级为警告
    if let Err(err) = crate::auth::validate_oauth_on_startup(&cfg, &state.oauth_accounts_path) {
        tracing::warn!("OAuth accounts validation failed: {err}");
    }
    drop(cfg);
    // 启动边界补完"凭据已落盘、provider 未写入"的 connect
    state.recover_orphan_oauth_providers().await;
    let cfg = state.config();
    let server = cfg.require_server()?;
    let listen_addr = server.listen.clone();
    let listen = listen_addr
        .parse::<SocketAddr>()
        .with_context(|| format!("invalid listen address {listen_addr}"))?;
    let fatal_config_state = state.fatal_config_state();
    let app = router(state.clone());

    // : 先 bind 再打印——"listening" 必须意味着真正在监听，而不是"即将监听"。
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(err) => return Err(crate::service::format_bind_error(&listen_addr, &err)),
    };
    let bound_addr = listener.local_addr().unwrap_or(listen);
    tracing::info!(
        "llm-proxy listening on {bound_addr}; client configs are not auto-modified — point clients with 'llm-proxy launch <client>'"
    );
    println!("llm-proxy Rust v2 listening on http://{bound_addr}");
    let exit_reason = std::sync::Arc::new(std::sync::Mutex::new("shutdown_channel_closed"));
    let shutdown_exit_reason = exit_reason.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                source = &mut shutdown_rx => {
                    if let Ok(source) = source
                        && let Ok(mut exit_reason) = shutdown_exit_reason.lock()
                    { *exit_reason = source; }
                }
                _ = fatal_config_state.wait() => {
                    if let Ok(mut exit_reason) = shutdown_exit_reason.lock() { *exit_reason = "config_writer_fail_stop"; }
                }
            }
        })
        .await?;
    let final_reason = *exit_reason.lock().expect("exit reason lock poisoned");
    if state.fatal_config_state().is_failed() || final_reason == "config_writer_fail_stop" {
        anyhow::bail!("service stopped due to fatal config writer fail-stop");
    }
    Ok(final_reason)
}

pub(crate) const GENERATION_FAILURE_HEADER: &str = "x-llm-proxy-upstream-generation-failed";
#[allow(dead_code)]
pub(crate) const GENERATION_FAILURE_CODE: &str = "upstream_generation_failed";

pub(crate) fn is_upstream_generation_failure_response(response: &Response) -> bool {
    response.headers().get(GENERATION_FAILURE_HEADER).is_some()
}

#[allow(dead_code)]
pub(crate) fn make_upstream_generation_failure_response(
    protocol: Protocol,
    request_id: &str,
    reason: &str,
) -> Response {
    let message = format!(
        "upstream generation failed ({reason}); request_id: {request_id}; check request parameters, retry, or consult docs/user_guide/troubleshooting.md"
    );
    let mut response = match protocol {
        Protocol::OpenaiChatCompletions => json_error_with_type_and_code(
            StatusCode::BAD_GATEWAY,
            &message,
            "server_error",
            GENERATION_FAILURE_CODE,
        ),
        Protocol::OpenaiResponses => {
            responses_error_with_code(StatusCode::BAD_GATEWAY, &message, GENERATION_FAILURE_CODE)
        }
        Protocol::Anthropic => {
            anthropic_error_with_code(StatusCode::BAD_GATEWAY, &message, GENERATION_FAILURE_CODE)
        }
    };
    response
        .headers_mut()
        .insert(GENERATION_FAILURE_HEADER, HeaderValue::from_static("true"));
    if let Ok(val) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-llm-proxy-request-id", val);
    }
    response
}

async fn request_trace(request: Request, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let request_id_header = HeaderValue::from_str(&request_id).ok();
    let span = tracing::info_span!(
        target: "llm_proxy::request",
        "proxy_request",
        request_id = %request_id,
        method = %method,
        path = %path,
        version = env!("CARGO_PKG_VERSION"),
    );
    async move {
        let mut response = next.run(request).await;
        if let Some(value) = request_id_header {
            response
                .headers_mut()
                .insert("x-llm-proxy-request-id", value);
        }
        response
    }
    .instrument(span)
    .await
}

/// Fixed guard sized for 8M-token text requests plus JSON and tool metadata.
const MODEL_PROXY_MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn router(state: AppState) -> Router {
    // Token authentication state is owned by AppState (loaded from tokens.toml
    // at startup); share a handle into the proxy-route middleware.
    let token_auth_state = state.token.clone();

    // Proxy routes (with token auth middleware)
    let proxy_routes = Router::new()
        .route("/openai/v1/models", get(list_openai_models))
        .route("/responses/v1/models", get(list_responses_models))
        .route("/anthropic/v1/models", get(list_anthropic_models))
        .route("/claude-desktop/v1/models", get(list_anthropic_models))
        .route("/openai/v1/chat/completions", post(chat_completions))
        .route("/openai/v1/responses", post(responses))
        .route("/responses/v1/responses", post(responses))
        .route("/anthropic/v1/messages", post(anthropic_messages))
        .route("/claude-desktop/v1/messages", post(anthropic_messages))
        .route(
            "/claude-desktop/v1/messages/count_tokens",
            post(count_tokens),
        )
        .route("/v1/token/usage", get(token_usage_handler))
        .route("/v1/token/usage/history", get(token_usage_history_handler))
        .route("/v1/token/models", get(token_models_handler))
        .layer(middleware::from_fn_with_state(
            token_auth_state.clone(),
            crate::token::auth::token_auth,
        ))
        .layer(DefaultBodyLimit::max(MODEL_PROXY_MAX_REQUEST_BODY_BYTES));

    // Admin routes on public HTTP listener with token auth and RBAC protection.
    // Privileged routes (mutating state or triggering real upstream probes) require admin role.
    // Read-only / tenant client inspection routes are accessible by any authenticated token.
    let admin_privileged_routes = Router::new()
        .route("/admin/provider/add", post(crate::admin::provider_add))
        .route(
            "/admin/provider/reselect",
            post(crate::admin::provider_reselect),
        )
        .route(
            "/admin/provider/remove",
            axum::routing::delete(crate::admin::provider_remove),
        )
        .route("/admin/provider/copy", post(crate::admin::provider_copy))
        .route("/admin/config/reload", post(crate::admin::config_reload))
        .route("/admin/config/update", post(crate::admin::config_update))
        .route("/admin/model/add", post(crate::admin::model_add))
        .route("/admin/model/set", post(crate::admin::model_set))
        .route("/admin/model/remove", post(crate::admin::model_remove))
        .route("/admin/model/rename", post(crate::admin::model_rename))
        .route("/admin/model/provider", post(crate::admin::model_provider))
        .route("/admin/cooldown/clear", post(crate::admin::cooldown_clear))
        .route("/admin/cooldown/set", post(crate::admin::cooldown_set))
        .route("/admin/oauth/write", post(crate::admin::oauth_write))
        .route("/admin/oauth/logout", post(crate::admin::oauth_logout))
        .route("/admin/token/create", post(crate::admin::token_create))
        .route("/admin/token/delete", post(crate::admin::token_delete))
        .route("/admin/token/rename", post(crate::admin::token_rename))
        .route(
            "/admin/token/reset-usage",
            post(crate::admin::token_reset_usage),
        )
        .route(
            "/admin/protection/bad-request-budget/reset",
            post(crate::admin::bad_request_budget_reset),
        )
        .route(
            "/admin/protection/token-error-cooldown/clear",
            post(crate::admin::token_error_cooldown_clear),
        )
        // Provider topology (ids, endpoints, auth metadata) is administrator-only.
        .route("/admin/provider/list", get(crate::admin::provider_list))
        .route("/admin/provider/{id}", get(crate::admin::provider_info))
        .layer(middleware::from_fn(crate::token::auth::require_admin));

    // Read-only metadata and self-probing routes exposed on the public HTTP listener.
    // NOTE: /admin/status/probe is placed here rather than in admin_privileged_routes to
    // support dual-role access control:
    // - Admin tokens & UDS get full unmetered cluster-wide probe capabilities.
    // - Unprivileged tokens are scoped to their allowed_models, pre-gated by quota windows,
    //   and metered honestly. Mutating endpoints remain strictly guarded by require_admin.
    // Provider topology (ids, endpoints, auth metadata) is administrator-only and lives in
    // admin_privileged_routes; model metadata remains readable by any authenticated token.
    let admin_routes = Router::new()
        .route("/admin/ping", get(crate::admin::ping))
        .route("/admin/status", get(crate::admin::status))
        .route("/admin/status/probe", post(crate::admin::status_probe))
        .route("/admin/model/list", get(crate::admin::model_list))
        .route("/admin/model/{id}", get(crate::admin::model_info))
        .route(
            "/admin/client-config/{client}",
            get(crate::admin::client_config),
        )
        .route("/admin/usage", get(crate::admin::usage))
        .route(
            "/admin/usage/attribution",
            get(crate::admin::usage_attribution),
        )
        .route("/admin/metrics", get(crate::admin::metrics))
        .route("/admin/metrics/history", get(crate::admin::metrics_history))
        // 公开 listener 用剥离 server.auth_token 的变体（UDS 管理路由保持完整）。
        .route(
            "/admin/config/snapshot",
            get(crate::admin::config_snapshot_http),
        )
        .route(
            "/admin/protection/bad-request-budget",
            get(crate::admin::bad_request_budget_status),
        )
        .route(
            "/admin/protection/token-error-cooldown",
            get(crate::admin::token_error_cooldown_status),
        )
        .merge(admin_privileged_routes)
        .layer(middleware::from_fn_with_state(
            token_auth_state,
            crate::token::auth::token_auth,
        ));

    // Expose unauthenticated, zero-leakage /health endpoint for external
    // liveness probes, k8s probes, and automated test readiness checks.
    Router::new()
        .route("/health", get(health_check))
        .merge(proxy_routes)
        .merge(admin_routes)
        .layer(middleware::from_fn(request_trace))
        .with_state(state)
}

async fn health_check() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn list_openai_models(
    State(state): State<AppState>,
    Extension(grant): Extension<crate::token::auth::TokenGrant>,
) -> impl IntoResponse {
    let cfg = state.config();
    let ids = cfg
        .models
        .iter()
        .filter(|(id, _)| {
            (cfg.model_serves_protocol(id, Protocol::OpenaiChatCompletions)
                || cfg.model_serves_protocol(id, Protocol::OpenaiResponses))
                && grant.model_allowed(id)
        })
        .map(|(id, _)| id.clone())
        .collect();
    Json(openai_model_list(ids))
}

async fn list_responses_models(
    State(state): State<AppState>,
    Extension(grant): Extension<crate::token::auth::TokenGrant>,
) -> impl IntoResponse {
    let cfg = state.config();
    let ids = cfg
        .models
        .iter()
        .filter(|(id, _)| {
            cfg.model_serves_protocol(id, Protocol::OpenaiResponses) && grant.model_allowed(id)
        })
        .map(|(id, _)| id.clone())
        .collect();
    Json(openai_model_list(ids))
}

async fn list_anthropic_models(
    State(state): State<AppState>,
    Extension(grant): Extension<crate::token::auth::TokenGrant>,
) -> impl IntoResponse {
    let cfg = state.config();
    let ids = cfg
        .models
        .iter()
        .filter(|(id, _)| {
            cfg.model_serves_protocol(id, Protocol::Anthropic) && grant.model_allowed(id)
        })
        .map(|(id, _)| id.clone())
        .collect();
    Json(anthropic_model_list(ids))
}

/// True when a tool-call id looks like an upstream conversation-local id that predates
/// proxy-side re-identification (`call_` + short hex, e.g. `call_5fd50cd7512b`), rather
/// than one of the proxy's own mints (`call_/fc_/gemini-call-/toolu_` + 32-hex uuid).
///
/// Such ids were handed to clients by pre-re-identification builds; their replay records
/// (if ever written) belong to an older key scheme and can no longer match, so a miss
/// tagged this way is expected legacy noise that sentinel coverage absorbs. Shape-based
/// heuristic only — same-shape ids written under an older key scheme are NOT caught.
#[allow(dead_code)]
fn suspected_legacy_upstream_id(id: &str) -> bool {
    id.strip_prefix("call_")
        .is_some_and(|rest| rest.len() != 32 || !rest.chars().all(|c| c.is_ascii_hexdigit()))
}

fn openai_model_list(ids: Vec<String>) -> Value {
    let data: Vec<Value> = ids
        .into_iter()
        .map(|id| json!({ "id": id, "object": "model", "created": 0, "owned_by": "llm-proxy" }))
        .collect();
    json!({ "object": "list", "data": data })
}

fn anthropic_model_list(ids: Vec<String>) -> Value {
    let data: Vec<Value> = ids
        .iter()
        .map(|id| json!({ "id": id, "type": "model", "display_name": id }))
        .collect();
    json!({
        "data": data,
        "has_more": false,
        "first_id": ids.first().map(String::as_str),
        "last_id": ids.last().map(String::as_str)
    })
}

/// GET /v1/token/usage — query the authenticated token's quota usage
async fn token_usage_handler(
    State(state): State<AppState>,
    Extension(grant): Extension<crate::token::auth::TokenGrant>,
) -> impl IntoResponse {
    use crate::token::auth::TokenGrant;

    // Extract token context from the grant
    let token_ctx = match &grant {
        TokenGrant::Granted(ctx) => ctx,
        TokenGrant::Open => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "token usage query requires token authentication"
                })),
            );
        }
    };

    // Read token config
    let config = match state.token.config.read() {
        Ok(cfg) => cfg,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "token config lock poisoned"})),
            );
        }
    };

    // Find the token
    let token = match config.tokens.iter().find(|t| t.key == token_ctx.key) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "token not found"})),
            );
        }
    };

    // Materialise the configured windows before reading them: limits live in the
    // configuration, usage lives in the store, so a token that has never been
    // used must still report `0 / limit` per window rather than an empty list.
    if let Some(store) = &state.token.usage_store
        && let Err(error) = store.init_token_windows(&token.key, &token.quota_windows)
    {
        tracing::warn!(error = %error, "failed to initialise token windows for display");
    }

    // Query usage
    let usage = match &state.token.usage_store {
        Some(store) => match store.get_token_usage(&token_ctx.key) {
            Ok(u) => u,
            Err(e) => {
                tracing::error!(error = %e, "failed to query token usage");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "failed to query usage"})),
                );
            }
        },
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "usage store unavailable"})),
            );
        }
    };

    // Build response
    let quota_windows: Vec<Value> = usage
        .iter()
        .map(|w| {
            let reset_at = w.window_start + w.window_seconds;
            let remaining = (w.quota_limit - w.used).max(0);
            json!({
                "window": w.window_key,
                "quota_mode": crate::token::dimension::label(&w.quota_mode),
                "quota_limit": w.quota_limit,
                "used": w.used,
                "remaining": remaining,
                "reset_at": reset_at
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "token_name": token.name,
            "allowed_models": token.allowed_models,
            "quota_windows": quota_windows
        })),
    )
}

// ── Usage History Rate Limiter ──────────────────────────────────────────────

/// Per-token rate limiter for the usage history endpoint.
/// 10 queries per minute per token, in-memory, restart resets counters.
struct UsageHistoryRateLimiter {
    entries: std::sync::Mutex<HashMap<String, RateLimiterEntry>>,
}

struct RateLimiterEntry {
    count: u32,
    window_start: Instant,
}

impl UsageHistoryRateLimiter {
    fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Check if the token is within the rate limit. Returns `true` if allowed.
    /// Also performs lazy cleanup of stale entries every 100th call.
    fn check(&self, token_key: &str) -> bool {
        let mut entries = self.entries.lock().unwrap();
        let now = Instant::now();

        // Lazy cleanup: remove stale entries periodically
        // (cheap since the map is small — bounded by unique tokens)
        if entries.len() > 50 {
            entries.retain(|_, entry| {
                now.duration_since(entry.window_start) < Duration::from_secs(600)
            });
        }

        let entry = entries
            .entry(token_key.to_string())
            .or_insert(RateLimiterEntry {
                count: 0,
                window_start: now,
            });

        // Reset window if 60s has elapsed
        if now.duration_since(entry.window_start) >= Duration::from_secs(60) {
            entry.count = 0;
            entry.window_start = now;
        }

        if entry.count >= 10 {
            return false;
        }

        entry.count += 1;
        true
    }
}

/// Query parameters for `GET /v1/token/usage/history`.
#[derive(Debug, Deserialize)]
struct UsageHistoryQuery {
    period: Option<String>,
    group_by: Option<String>,
    model: Option<String>,
}

/// Parse a period string into (start, end) as UTC DateTime.
/// Supports: today, 7d, 30d, 90d, YYYY-MM-DD:YYYY-MM-DD
/// Returns Err with a user-facing message on invalid input.
/// 指定日历日按**系统本地时区**的 00:00 换算为 UTC 时刻（DST 歧义/缺失时回退 UTC 午夜）。
fn local_day_start_utc(day: chrono::NaiveDate) -> chrono::DateTime<chrono::Utc> {
    let midnight = day.and_hms_opt(0, 0, 0).unwrap();
    midnight
        .and_local_timezone(chrono::Local)
        .single()
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| midnight.and_utc())
}

fn parse_history_period(
    period: &str,
) -> std::result::Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>), String> {
    let now = chrono::Utc::now();
    // 按天统计与窗口边界对齐系统本地时区的自然日（修复：UTC 日界导致本地凌晨用量归前一天）
    let today_start = local_day_start_utc(chrono::Local::now().date_naive());

    match period {
        "today" => Ok((today_start, now)),
        s if s.ends_with('d') => {
            let days: i64 = s
                .trim_end_matches('d')
                .parse()
                .map_err(|_| format!("invalid period: {s}"))?;
            if days <= 0 || days > 90 {
                return Err("period exceeds maximum (90d)".to_string());
            }
            let start = today_start - chrono::Duration::days(days);
            Ok((start, now))
        }
        s if s.contains(':') => {
            let parts: Vec<&str> = s.split(':').collect();
            if parts.len() != 2 {
                return Err(format!("invalid period: {s}"));
            }
            let start_date = chrono::NaiveDate::parse_from_str(parts[0], "%Y-%m-%d")
                .map_err(|_| format!("invalid period: {s}"))?;
            let end_date = chrono::NaiveDate::parse_from_str(parts[1], "%Y-%m-%d")
                .map_err(|_| format!("invalid period: {s}"))?;
            // Validate range doesn't exceed 90 days
            let span = end_date - start_date;
            if span.num_days() > 90 {
                return Err("period exceeds maximum (90d)".to_string());
            }
            if span.num_days() < 0 {
                return Err(format!("invalid period: {s}"));
            }
            let start = local_day_start_utc(start_date);
            let end = local_day_start_utc(end_date + chrono::Duration::days(1));
            Ok((start, end))
        }
        _ => Err(format!("invalid period: {period}")),
    }
}

/// GET /v1/token/usage/history — query the authenticated token's usage history
async fn token_usage_history_handler(
    State(state): State<AppState>,
    Extension(grant): Extension<crate::token::auth::TokenGrant>,
    axum::extract::Query(params): axum::extract::Query<UsageHistoryQuery>,
) -> impl IntoResponse {
    use crate::token::auth::TokenGrant;
    use crate::usage_stats::HistoryGroupBy;

    // 1. Authenticate — must be a granted token (not open mode)
    let token_ctx = match &grant {
        TokenGrant::Granted(ctx) => ctx,
        TokenGrant::Open => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "status": "error",
                    "error": "usage history requires token authentication"
                })),
            );
        }
    };

    // 2. Rate limiting (per-token, 10/min)
    if !state.usage_history_limiter.check(&token_ctx.key) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "status": "error",
                "error": "rate limit exceeded (10 queries per minute)"
            })),
        );
    }

    // 3. Parse group_by parameter
    let group_by = match params.group_by.as_deref() {
        Some("hour") => HistoryGroupBy::Hour,
        Some("day") | None => HistoryGroupBy::Day,
        Some("model") => HistoryGroupBy::Model,
        Some("day,model") => HistoryGroupBy::DayModel,
        Some("hour,model") => HistoryGroupBy::HourModel,
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "status": "error",
                    "error": format!("invalid group_by: {other}")
                })),
            );
        }
    };

    // 4. Parse period parameter (default: 7d)
    let period_str = params.period.as_deref().unwrap_or("7d");
    let (start, end) = match parse_history_period(period_str) {
        Ok(range) => range,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "status": "error",
                    "error": msg
                })),
            );
        }
    };

    // 5. Check auto-downgrade: 90d + hour = 2160 groups > 1000 max
    // Only applies to pure Hour grouping; Model and composite groups are naturally bounded.
    let effective_group_by = {
        let span_hours = (end - start).num_hours();
        if group_by == HistoryGroupBy::Hour && span_hours > 1000 {
            HistoryGroupBy::Day
        } else {
            group_by
        }
    };
    let auto_downgraded = effective_group_by != group_by;

    let group_by_str = match effective_group_by {
        HistoryGroupBy::Day => "day",
        HistoryGroupBy::Hour => "hour",
        HistoryGroupBy::Model => "model",
        HistoryGroupBy::DayModel => "day,model",
        HistoryGroupBy::HourModel => "hour,model",
    };

    let ingress_models: std::collections::HashSet<String> =
        state.config().models.keys().cloned().collect();

    let model_filter = params
        .model
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    if let Some(target_model) = model_filter
        && !ingress_models.contains(target_model)
    {
        // Whitelist defense: unknown model returns empty groups to prevent topology probing
        return (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "data": {
                    "period": period_str,
                    "group_by": group_by_str,
                    "auto_downgraded": false,
                    "token_name": token_ctx.name,
                    "summary": {
                        "requests": 0,
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "total_tokens": 0,
                    },
                    "groups": []
                }
            })),
        );
    }

    // 6. Query the usage store
    let core = state.core.lock().await;
    let store = match core.usage_store_arc() {
        Some(s) => s,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status": "error", "error": "usage store unavailable"})),
            );
        }
    };

    let result = match store.query_history_with_filter(
        &token_ctx.key,
        start,
        end,
        effective_group_by,
        model_filter,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "usage history query failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"status": "error", "error": "query failed"})),
            );
        }
    };

    // 7. Build response with whitelist defense (ADR-038)

    let mut consolidated: Vec<crate::usage_stats::HistoryGroup> = Vec::new();
    for g in result.groups {
        let key = if effective_group_by == HistoryGroupBy::Model {
            if ingress_models.contains(&g.key) {
                g.key
            } else {
                "other".to_string()
            }
        } else {
            g.key
        };
        let model = g.model.map(|m| {
            if ingress_models.contains(&m) {
                m
            } else {
                "other".to_string()
            }
        });
        if let Some(existing) = consolidated
            .iter_mut()
            .find(|x| x.key == key && x.model == model)
        {
            existing.requests += g.requests;
            existing.input_tokens += g.input_tokens;
            existing.output_tokens += g.output_tokens;
            existing.total_tokens += g.total_tokens;
        } else {
            consolidated.push(crate::usage_stats::HistoryGroup {
                key,
                model,
                requests: g.requests,
                input_tokens: g.input_tokens,
                output_tokens: g.output_tokens,
                total_tokens: g.total_tokens,
            });
        }
    }

    let groups: Vec<Value> = consolidated
        .iter()
        .map(|g| {
            let mut obj = serde_json::Map::new();
            obj.insert("key".to_string(), json!(g.key));
            if let Some(model) = &g.model {
                obj.insert("model".to_string(), json!(model));
            }
            obj.insert("requests".to_string(), json!(g.requests));
            obj.insert("input_tokens".to_string(), json!(g.input_tokens));
            obj.insert("output_tokens".to_string(), json!(g.output_tokens));
            obj.insert("total_tokens".to_string(), json!(g.total_tokens));
            Value::Object(obj)
        })
        .collect();

    let mut data = json!({
        "token_name": token_ctx.name,
        "period": period_str,
        "group_by": group_by_str,
        "auto_downgraded": auto_downgraded,
        "summary": {
            "requests": result.summary.requests,
            "input_tokens": result.summary.input_tokens,
            "output_tokens": result.summary.output_tokens,
            "total_tokens": result.summary.total_tokens
        },
        "groups": groups
    });

    if auto_downgraded {
        data["warning"] = json!("auto-downgraded to day grouping: result exceeded 1000 groups");
    }

    (
        StatusCode::OK,
        Json(json!({ "status": "ok", "data": data })),
    )
}

/// Response payload for `GET /v1/token/models`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenModelsResponse {
    pub token_name: Option<String>,
    pub models: Vec<TokenModelItem>,
}

/// Capability facts and bounds for a single model permitted by the caller's token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenModelItem {
    pub id: String,
    pub protocols: Vec<String>,
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub features: Vec<String>,
    pub features_declared: bool,
    pub reasoning_levels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_level: Option<String>,
    pub routing: bool,
    pub bounds: String,
}

/// GET /v1/token/models — query the models and derived capability bounds allowed for the authenticated token
async fn token_models_handler(
    State(state): State<AppState>,
    Extension(grant): Extension<crate::token::auth::TokenGrant>,
) -> impl IntoResponse {
    let cfg = state.config();
    let token_name = grant.token().map(|c| c.name.clone());

    let mut models = Vec::new();
    for (id, model) in &cfg.models {
        if !grant.model_allowed(id) {
            continue;
        }

        // 1. protocols: client protocols served by this model
        let mut protocols = Vec::new();
        for proto in crate::config::Protocol::CLIENT_PROTOCOLS {
            if cfg.model_serves_protocol(id, proto) {
                protocols.push(match proto {
                    crate::config::Protocol::OpenaiChatCompletions => "openai-chat".to_string(),
                    crate::config::Protocol::OpenaiResponses => "openai-responses".to_string(),
                    crate::config::Protocol::Anthropic => "anthropic".to_string(),
                });
            }
        }

        // 2. capability bounds: derived lower bound for routing models, or model's own declaration
        let bounds = cfg.model_capability_bounds(id);
        let context_window = bounds
            .as_ref()
            .map(|b| b.context_window)
            .unwrap_or(model.context_window);
        let max_output_tokens = bounds
            .as_ref()
            .map(|b| b.max_output_tokens)
            .unwrap_or(model.max_output_tokens);
        let features = bounds
            .map(|b| b.features)
            .unwrap_or_else(|| model.features.clone());

        // 3. features_declared: true if the model (or all its reachable targets) explicitly declared any features
        let features_declared = cfg.model_features_declared(id);

        // 4. reasoning_levels: active level names without leaking internal api_value mapping
        let reasoning_levels = cfg
            .client_reasoning_levels(id)
            .map(|entries| entries.into_iter().map(|e| e.level().to_string()).collect())
            .unwrap_or_default();

        let default_reasoning_level = cfg.client_default_reasoning_level(id);
        let routing = model.is_routing_model();
        let bounds_type = "derived".to_string();

        models.push(TokenModelItem {
            id: id.clone(),
            protocols,
            context_window,
            max_output_tokens,
            features,
            features_declared,
            reasoning_levels,
            default_reasoning_level,
            routing,
            bounds: bounds_type,
        });
    }

    models.sort_by(|a, b| a.id.cmp(&b.id));

    (
        StatusCode::OK,
        Json(TokenModelsResponse { token_name, models }),
    )
}

async fn count_tokens(Json(body): Json<Value>) -> impl IntoResponse {
    let mut text = String::new();
    collect_text_for_token_count(&body, &mut text);
    Json(json!({ "input_tokens": rough_token_count(&text) }))
}

fn collect_text_for_token_count(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push_str(s);
            out.push(' ');
        }
        Value::Array(items) => {
            for item in items {
                collect_text_for_token_count(item, out);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "text" | "content" | "system" | "input") {
                    collect_text_for_token_count(value, out);
                }
            }
        }
        _ => {}
    }
}

fn rough_token_count(text: &str) -> usize {
    text.split_whitespace().count().max(1)
}

/// Enforce `allowed_models` for a token-authenticated request.
///
/// Returns `Some(403)` when the requested model is outside the grant. Open
/// mode (no tokens configured) and empty `allowed_models` allow all (spec §3.1).
/// The requested model is read from the JSON `model` field; a request without a
/// usable model string is deferred to the existing model-resolution errors.
fn check_token_model_access(
    grant: &crate::token::auth::TokenGrant,
    body: &Value,
) -> Option<Response> {
    if matches!(grant, crate::token::auth::TokenGrant::Open) {
        return None;
    }
    let model = body.get("model").and_then(Value::as_str)?;
    if grant.model_allowed(model) {
        None
    } else {
        Some(json_error(
            StatusCode::FORBIDDEN,
            &format!("token is not allowed to use model '{model}'"),
        ))
    }
}

async fn chat_completions(
    State(state): State<AppState>,
    Extension(grant): Extension<crate::token::auth::TokenGrant>,
    JsonWithBytes {
        value: mut body,
        bytes: raw_body,
    }: JsonWithBytes,
) -> Response {
    let ingress_start = std::time::Instant::now();
    if let Some(err) = check_token_model_access(&grant, &body) {
        return err;
    }
    // resolve the routing chain before normal candidate resolution.
    // The resolved candidates carry the *target* model, which governs execution
    // (cooldown identity, thinking policy, replay identity). `model_id` carries
    // the client-visible identity and is used for the downstream response body,
    // usage attribution, and client-facing error text.
    let routing_model_id = match resolve_routing(
        &state,
        Protocol::OpenaiChatCompletions,
        &mut body,
        json_error,
    ) {
        Ok(r) => r,
        Err(response) => {
            if let Some(token_ctx) = grant.token() {
                let model = body
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                state.record_local_rejection_for_token(&token_ctx.name, model, "ingress_routing");
            }
            return *response;
        }
    };
    let model_id = client_visible_model_id(routing_model_id.as_deref(), &body);
    if let Some(token_ctx) = grant.token() {
        let cd_status = state.check_token_error_cooldown(&token_ctx.name, &model_id);
        if cd_status.blocked {
            let msg = match cd_status.reason.as_deref() {
                Some("local_reject_tolerance") => {
                    "token blocked after repeated bad requests; wait for cooldown to expire"
                }
                _ => "token blocked after repeated upstream errors; wait for cooldown to expire",
            };
            return json_error(StatusCode::TOO_MANY_REQUESTS, msg);
        }
    }
    let candidates = match resolve_request_candidates(
        &state,
        Protocol::OpenaiChatCompletions,
        &body,
        &model_id,
        json_error,
    ) {
        Ok(candidates) => candidates,
        Err(response) => {
            if let Some(token_ctx) = grant.token() {
                state.record_local_rejection_for_token(
                    &token_ctx.name,
                    &model_id,
                    "ingress_format",
                );
            }
            return *response;
        }
    };
    // RoundRobin rotation for multi-provider load balancing.
    let candidates = state.rotate_candidates_round_robin(&model_id, candidates);
    let thinking_intent = extract_thinking_intent(Protocol::OpenaiChatCompletions, &body);
    let fingerprint = crate::protection::fingerprint(
        Protocol::OpenaiChatCompletions,
        body.get("model").and_then(Value::as_str),
        &raw_body,
    );
    if state.bad_request_blocked(&fingerprint) {
        if let Some(token_ctx) = grant.token() {
            state.record_local_rejection_for_token(&token_ctx.name, &model_id, "fingerprint_block");
        }
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "request temporarily blocked after repeated upstream bad requests",
        );
    }
    // §10.5.5: until the first byte is written downstream, streaming requests
    // follow the same cooldown/fallback policy as non-streaming ones.
    let token = grant.token();
    let skip_cooling =
        match candidate_cooldown_gate(&state, &candidates, &model_id, json_error, token) {
            Ok(skip_cooling) => skip_cooling,
            Err(response) => return *response,
        };
    let mut last_error: Option<Response> = None;
    let mut saw_client_error = false;
    // One credit per client request: the first iteration that reaches dispatch
    // spends it, so internal fallback does not charge twice.
    let mut reserved = false;
    // Frequency-limited candidates are re-scanned in a last-ditch pass under
    // one shared queue_timeout budget (candidate order, 25ms between passes);
    // the fast pass itself never waits on an individual candidate.
    let mut freq_limited: Vec<(String, ExecutionPlan)> = Vec::new();
    let last_ditch_budget = Duration::from_secs(
        candidates
            .iter()
            .map(|(_, p)| p.request_frequency.queue_timeout_seconds.unwrap_or(10))
            .max()
            .unwrap_or(10),
    );
    let mut last_ditch: Option<Instant> = None;

    'passes: loop {
        // This pass's re-scan set: rebuilt from scratch so a candidate that
        // regains capacity and produces a real upstream result (5xx/401/
        // transport ...) leaves the set instead of being replayed every pass
        // until the shared budget expires.
        let mut freq_limited_next: Vec<(String, ExecutionPlan)> = Vec::new();
        let replay: Vec<(String, ExecutionPlan)> = match &last_ditch {
            None => candidates.clone(),
            Some(_) => freq_limited.clone(),
        };
        for (exec_model, mut plan) in replay {
            // When routing occurred, record the leaf target for usage attribution
            if routing_model_id.is_some() {
                plan.routed_target = Some(exec_model.clone());
            }
            let cooldown_key = plan.cooldown_key(&exec_model);
            if skip_cooling && state.is_cooling_down(&cooldown_key) {
                state.cooldowns.record_cooldown_hit(&cooldown_key);
                continue;
            }
            if token_candidate_cooling(&state, &plan, &exec_model, token) {
                last_error = Some(json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    &format!(
                        "provider {:?} is cooling down for this token after a rejected request; retry later",
                        plan.provider_id
                    ),
                ));
                continue;
            }
            // Per-account cumulative bad-request budget: skip this candidate when
            // the token's daily or lifetime layer is exhausted, allowing fallback
            // to other candidates .
            if let Some(token_ctx) = token {
                let (budget_reason, budget_diag) = state.account_budget_blocked(&token_ctx.key);
                match budget_reason {
                    crate::protection::BudgetBlockReason::DailyExhausted => {
                        let msg =
                        crate::protection::AccountBadBudgetManager::format_daily_recovery_message(
                            &token_ctx.key,
                        );
                        tracing::info!(
                            event = "account_budget_blocked",
                            reason = "daily",
                            token = %token_ctx.name,
                            provider = %plan.provider_id,
                            daily_date = %budget_diag.daily_date,
                            today = %budget_diag.today,
                            daily_count = budget_diag.daily_count,
                            lifetime_count = budget_diag.lifetime_count,
                            "account bad-request budget exhausted; skipping candidate, fallback continues"
                        );
                        last_error = Some(json_error(StatusCode::TOO_MANY_REQUESTS, &msg));
                        continue;
                    }
                    crate::protection::BudgetBlockReason::LifetimeExhausted => {
                        tracing::info!(
                            event = "account_budget_blocked",
                            reason = "lifetime",
                            token = %token_ctx.name,
                            provider = %plan.provider_id,
                            daily_date = %budget_diag.daily_date,
                            today = %budget_diag.today,
                            daily_count = budget_diag.daily_count,
                            lifetime_count = budget_diag.lifetime_count,
                            "account lifetime bad-request budget exhausted; skipping candidate, admin reset required"
                        );
                        last_error = Some(json_error(
                            StatusCode::TOO_MANY_REQUESTS,
                            "account triggered lifetime bad-request quota; admin reset required",
                        ));
                        continue;
                    }
                    crate::protection::BudgetBlockReason::Allowed => {}
                }
            }
            let (body, thinking_disabled) = match prepare_candidate_thinking_body(
                &state,
                Protocol::OpenaiChatCompletions,
                &body,
                &exec_model,
                &plan,
                &thinking_intent,
            ) {
                Ok(prepared) => prepared,
                Err(crate::proxy::request::ThinkingError::Client(err)) => {
                    return json_error(StatusCode::BAD_REQUEST, &err);
                }
                Err(crate::proxy::request::ThinkingError::ConfigAdapter(err)) => {
                    return json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("configuration error: {err}"),
                    );
                }
            };
            // Spend the request credit here — immediately before the request can go
            // out. Cooldown skips and validation failures never reach this line, so
            // they consume nothing; the check and the increment happen in one
            // transaction, so concurrent requests cannot overshoot.
            if !reserved {
                if let Some(token) = grant.token()
                    && let Some(response) = state.reserve_token_request(token, &plan.upstream_model)
                {
                    return response;
                }
                reserved = true;
            }
            let response = match plan.adapter() {
                AdapterKind::Passthrough => {
                    let mut upstream_body =
                        apply_disabled_thinking_egress(body.clone(), &plan, thinking_disabled);
                    if let Some(obj) = upstream_body.as_object_mut() {
                        obj.insert("model".to_string(), json!(plan.upstream_model));
                        if obj.get("stream").and_then(Value::as_bool) == Some(true) {
                            obj.insert(
                                "stream_options".to_string(),
                                json!({ "include_usage": true }),
                            );
                        }
                    }
                    forward_chat_request(
                        &state,
                        &plan,
                        &model_id,
                        upstream_body,
                        grant.token(),
                        ingress_start,
                    )
                    .await
                }
                AdapterKind::ChatCompletionsFromAnthropic => {
                    let anthropic_body = match convert::chat_to_anthropic_request(
                        body.clone(),
                        &plan.upstream_model,
                    ) {
                        Ok(body) => body,
                        // Per-candidate conversion failure is the shape × this
                        // protocol pair's property: keep falling back.
                        Err(err) => {
                            last_error =
                                Some(json_error(StatusCode::BAD_REQUEST, &err.to_string()));
                            continue;
                        }
                    };
                    forward_chat_via_anthropic(
                        &state,
                        &plan,
                        &model_id,
                        anthropic_body,
                        grant.token(),
                        ingress_start,
                    )
                    .await
                }
                AdapterKind::ChatCompletionsFromResponses => {
                    let responses_body = match convert::chat_to_responses_request(
                        body.clone(),
                        &plan.upstream_model,
                    ) {
                        Ok(body) => body,
                        Err(err) => {
                            last_error =
                                Some(json_error(StatusCode::BAD_REQUEST, &err.to_string()));
                            continue;
                        }
                    };
                    forward_chat_via_responses(
                        &state,
                        &plan,
                        &model_id,
                        &exec_model,
                        responses_body,
                        grant.token(),
                        ingress_start,
                    )
                    .await
                }
                other => json_error(
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "adapter {other:?} is not valid for Chat ingress; check provider endpoint declarations"
                    ),
                ),
            };
            let status = response.status();
            if is_upstream_generation_failure_response(&response) {
                return response;
            }
            if is_local_frequency_limited_response(&response) {
                freq_limited_next.push((exec_model.clone(), plan.clone()));
                last_error = Some(response);
                continue;
            }
            if is_local_rejection_response(&response) {
                // Our own rejection (e.g. an unrecoverable function-call history) is a
                // property of the request, not of this account: never cool a provider for
                // it and never count it. It must NOT abort the chain either — a later
                // candidate whose endpoint does not enforce the same shape (signature
                // requirements are per-candidate) may still serve the conversation, the
                // same way prep Reject keeps `continue`. The rejection is kept as the
                // last error, so if every candidate fails the client still gets the
                // local 400 instead of a blanket 429.
                last_error = Some(response);
                continue;
            }
            if is_protectable_client_error(status) && !has_dedicated_cooldown_path(status) {
                // 400 and the other generic 4xx (405/413/422 etc.) are request-level
                // errors — retrying on another provider would send the same bad body
                // upstream. Feed the fingerprint immediately and return without fallback.
                // Only 402/404/408 have a dedicated cooldown path and keep their
                // cooldown+fallback behavior.
                set_bad_request_cooldown(
                    &state,
                    &plan,
                    &exec_model,
                    &cooldown_key,
                    &response,
                    grant.token(),
                );
                state.observe_bad_request(&fingerprint);
                return response;
            }
            saw_client_error |= status == StatusCode::BAD_REQUEST;
            if should_fallback_without_cooldown(status) {
                last_error = Some(response);
                continue;
            }
            let cooldown = state
                .config()
                .effective_cooldown(&exec_model, &plan.provider_id);
            if let Some(duration) = resolve_candidate_cooldown_duration(
                &state,
                &plan,
                status,
                response.headers(),
                &cooldown,
            )
            .await
            {
                set_response_cooldown(
                    &state,
                    &plan,
                    &exec_model,
                    &cooldown_key,
                    status,
                    duration,
                    grant.token(),
                );
                last_error = Some(response);
                continue;
            }
            if skip_cooling {
                tracing::info!(
                    model = %model_id,
                    provider = %plan.provider_id,
                    "request succeeded via a healthy provider while others are cooling"
                );
            }
            return response;
        }
        // This pass exhausted every candidate in `replay` without a success.
        // Shrink the re-scan set to what is still frequency-limited this pass:
        // a candidate that recovered, sent, and failed for a non-frequency
        // reason (or cooled into a silent skip) is done — keep its real
        // upstream result as `last_error` and stop once nobody remains.
        freq_limited = freq_limited_next;
        match last_ditch {
            None => {
                if freq_limited.is_empty() || last_ditch_budget.is_zero() {
                    break 'passes;
                }
                last_ditch = Some(Instant::now() + last_ditch_budget);
            }
            Some(deadline) => {
                if freq_limited.is_empty() || Instant::now() >= deadline {
                    break 'passes;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }

    // §10.5.8: the fingerprint block is fed only when every candidate failed
    // and at least one failure was a client error.
    if saw_client_error {
        state.observe_bad_request(&fingerprint);
    }
    let final_response =
        last_error.unwrap_or_else(|| empty_candidates_response(json_error, &model_id));
    if is_local_rejection_response(&final_response)
        && let Some(token_ctx) = grant.token()
    {
        state.record_local_rejection_for_token(&token_ctx.name, &model_id, "known_precheck");
    }
    final_response
}

fn validate_responses_tools_shapes(req: &Value) -> Result<(), String> {
    fn has_name(value: &Value) -> bool {
        value
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.is_empty())
    }

    fn validate_function(tool: &Value, path: &str) -> Result<(), String> {
        if tool.get("type").and_then(Value::as_str) != Some("function") || !has_name(tool) {
            return Err(format!(
                "invalid request: {path} requires type \"function\" and a non-empty string name"
            ));
        }
        if let Some(parameters) = tool.get("parameters")
            && !parameters.is_null()
            && !parameters.is_object()
        {
            return Err(format!(
                "invalid request: {path}.parameters must be an object or null"
            ));
        }
        if let Some(strict) = tool.get("strict")
            && !strict.is_null()
            && !strict.is_boolean()
        {
            return Err(format!(
                "invalid request: {path}.strict must be a boolean or null"
            ));
        }
        Ok(())
    }

    if let Some(tools) = req.get("tools") {
        let Some(tools_arr) = tools.as_array() else {
            return Err("invalid request: \"tools\" must be an array".to_string());
        };
        for (idx, tool) in tools_arr.iter().enumerate() {
            if !tool.is_object() {
                return Err(format!(
                    "invalid request: tool at index {idx} must be an object"
                ));
            }
            let path = format!("tools[{idx}]");
            match tool.get("type").and_then(Value::as_str) {
                Some("function") => validate_function(tool, &path)?,
                // Preserve the existing convert_tools shell/namespace adapters.
                Some("shell" | "local_shell") => {}
                Some("namespace") => {
                    if !has_name(tool) {
                        return Err(format!(
                            "invalid request: {path}.name must be a non-empty string"
                        ));
                    }
                    let children = tool
                        .get("tools")
                        .and_then(Value::as_array)
                        .ok_or_else(|| format!("invalid request: {path}.tools must be an array"))?;
                    for (child_idx, child) in children.iter().enumerate() {
                        validate_function(child, &format!("{path}.tools[{child_idx}]"))?;
                    }
                }
                // Lossy conversion (conversion contract): other OpenAI-native
                // tool types (custom/web_search/...) cannot be represented in
                // Chat; convert_tools drops them instead of failing the request.
                Some(other) if !other.is_empty() => {
                    tracing::warn!(
                        tool_index = idx,
                        tool_type = other,
                        "tool type unsupported by Chat conversion will be dropped"
                    );
                }
                _ => {
                    return Err(format!(
                        "invalid request: {path}.type must be a non-empty string"
                    ));
                }
            }
        }
    }
    if let Some(tool_choice) = req.get("tool_choice") {
        match tool_choice {
            Value::String(s) if s == "auto" || s == "none" || s == "required" => {}
            Value::Object(_)
                if tool_choice.get("type").and_then(Value::as_str) == Some("function")
                    && has_name(tool_choice) => {}
            _ => {
                return Err(
                    "invalid request: \"tool_choice\" must be \"auto\", \"none\", \"required\", or a function selector with type \"function\" and a non-empty string name"
                        .to_string(),
                );
            }
        }
    }
    if let Some(parallel) = req.get("parallel_tool_calls")
        && !parallel.is_boolean()
    {
        return Err("invalid request: \"parallel_tool_calls\" must be a boolean".to_string());
    }
    Ok(())
}

async fn responses(
    State(state): State<AppState>,
    Extension(grant): Extension<crate::token::auth::TokenGrant>,
    JsonWithBytes {
        value: mut body,
        bytes: raw_body,
    }: JsonWithBytes,
) -> Response {
    let ingress_start = std::time::Instant::now();
    if let Some(err) = check_token_model_access(&grant, &body) {
        return err;
    }
    // resolve routing model chain.
    let routing_model_id = match resolve_routing(
        &state,
        Protocol::OpenaiResponses,
        &mut body,
        responses_error,
    ) {
        Ok(r) => r,
        Err(response) => {
            if let Some(token_ctx) = grant.token() {
                let model = body
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                state.record_local_rejection_for_token(&token_ctx.name, model, "ingress_routing");
            }
            return *response;
        }
    };
    let model_id = client_visible_model_id(routing_model_id.as_deref(), &body);
    if let Some(token_ctx) = grant.token() {
        let cd_status = state.check_token_error_cooldown(&token_ctx.name, &model_id);
        if cd_status.blocked {
            let msg = match cd_status.reason.as_deref() {
                Some("local_reject_tolerance") => {
                    "token blocked after repeated bad requests; wait for cooldown to expire"
                }
                _ => "token blocked after repeated upstream errors; wait for cooldown to expire",
            };
            return responses_error(StatusCode::TOO_MANY_REQUESTS, msg);
        }
    }
    let candidates = match resolve_request_candidates(
        &state,
        Protocol::OpenaiResponses,
        &body,
        &model_id,
        responses_error,
    ) {
        Ok(candidates) => candidates,
        Err(response) => {
            if let Some(token_ctx) = grant.token() {
                state.record_local_rejection_for_token(
                    &token_ctx.name,
                    &model_id,
                    "ingress_format",
                );
            }
            return *response;
        }
    };
    // RoundRobin rotation for multi-provider load balancing.
    let candidates = state.rotate_candidates_round_robin(&model_id, candidates);
    let thinking_intent = extract_thinking_intent(Protocol::OpenaiResponses, &body);
    let fingerprint = crate::protection::fingerprint(
        Protocol::OpenaiResponses,
        body.get("model").and_then(Value::as_str),
        &raw_body,
    );
    if state.bad_request_blocked(&fingerprint) {
        if let Some(token_ctx) = grant.token() {
            state.record_local_rejection_for_token(&token_ctx.name, &model_id, "fingerprint_block");
        }
        return responses_error(
            StatusCode::TOO_MANY_REQUESTS,
            "request temporarily blocked after repeated upstream bad requests",
        );
    }
    // §10.5.5: until the first byte is written downstream, streaming requests
    // follow the same cooldown/fallback policy as non-streaming ones.
    let token = grant.token();
    let skip_cooling =
        match candidate_cooldown_gate(&state, &candidates, &model_id, responses_error, token) {
            Ok(skip_cooling) => skip_cooling,
            Err(response) => return *response,
        };
    let mut last_error: Option<Response> = None;
    let mut saw_client_error = false;
    // One credit per client request: the first iteration that reaches dispatch
    // spends it, so internal fallback does not charge twice.
    let mut reserved = false;
    // Frequency-limited candidates are re-scanned in a last-ditch pass under
    // one shared queue_timeout budget (candidate order, 25ms between passes);
    // the fast pass itself never waits on an individual candidate.
    let mut freq_limited: Vec<(String, ExecutionPlan)> = Vec::new();
    let last_ditch_budget = Duration::from_secs(
        candidates
            .iter()
            .map(|(_, p)| p.request_frequency.queue_timeout_seconds.unwrap_or(10))
            .max()
            .unwrap_or(10),
    );
    let mut last_ditch: Option<Instant> = None;

    'passes: loop {
        // This pass's re-scan set: rebuilt from scratch so a candidate that
        // regains capacity and produces a real upstream result (5xx/401/
        // transport ...) leaves the set instead of being replayed every pass
        // until the shared budget expires.
        let mut freq_limited_next: Vec<(String, ExecutionPlan)> = Vec::new();
        let replay: Vec<(String, ExecutionPlan)> = match &last_ditch {
            None => candidates.clone(),
            Some(_) => freq_limited.clone(),
        };
        for (exec_model, mut plan) in replay {
            // When routing occurred, record the leaf target for usage attribution
            if routing_model_id.is_some() {
                plan.routed_target = Some(exec_model.clone());
            }
            let cooldown_key = plan.cooldown_key(&exec_model);
            if skip_cooling && state.is_cooling_down(&cooldown_key) {
                state.cooldowns.record_cooldown_hit(&cooldown_key);
                continue;
            }
            if token_candidate_cooling(&state, &plan, &exec_model, token) {
                last_error = Some(responses_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    &format!(
                        "provider {:?} is cooling down for this token after a rejected request; retry later",
                        plan.provider_id
                    ),
                ));
                continue;
            }
            // Per-account cumulative bad-request budget: skip this candidate when
            // the token's daily or lifetime layer is exhausted, allowing fallback
            // to other candidates .
            if let Some(token_ctx) = token {
                let (budget_reason, budget_diag) = state.account_budget_blocked(&token_ctx.key);
                match budget_reason {
                    crate::protection::BudgetBlockReason::DailyExhausted => {
                        tracing::info!(
                            event = "account_budget_blocked",
                            reason = "daily",
                            token = %token_ctx.name,
                            provider = %plan.provider_id,
                            daily_date = %budget_diag.daily_date,
                            today = %budget_diag.today,
                            daily_count = budget_diag.daily_count,
                            lifetime_count = budget_diag.lifetime_count,
                            "account bad-request budget exhausted; skipping candidate, fallback continues"
                        );
                        last_error = Some(responses_error(
                        StatusCode::TOO_MANY_REQUESTS,
                        &crate::protection::AccountBadBudgetManager::format_daily_recovery_message(
                            &token_ctx.key,
                        ),
                    ));
                        continue;
                    }
                    crate::protection::BudgetBlockReason::LifetimeExhausted => {
                        tracing::info!(
                            event = "account_budget_blocked",
                            reason = "lifetime",
                            token = %token_ctx.name,
                            provider = %plan.provider_id,
                            daily_date = %budget_diag.daily_date,
                            today = %budget_diag.today,
                            daily_count = budget_diag.daily_count,
                            lifetime_count = budget_diag.lifetime_count,
                            "account lifetime bad-request budget exhausted; skipping candidate, admin reset required"
                        );
                        last_error = Some(responses_error(
                            StatusCode::TOO_MANY_REQUESTS,
                            "account triggered lifetime bad-request quota; admin reset required",
                        ));
                        continue;
                    }
                    crate::protection::BudgetBlockReason::Allowed => {}
                }
            }
            // The unprepared client body is consumed only by the Chat adapter
            // (tool-shape validation plus response-side placeholder work). Clone it
            // lazily per candidate so Passthrough/Anthropic candidates
            // skip the deep clone entirely (cost item S2).
            let original_responses_req =
                (plan.adapter() == AdapterKind::ResponsesFromChatCompletions).then(|| body.clone());
            let (body, thinking_disabled) = match prepare_candidate_thinking_body(
                &state,
                Protocol::OpenaiResponses,
                &body,
                &exec_model,
                &plan,
                &thinking_intent,
            ) {
                Ok(prepared) => prepared,
                Err(crate::proxy::request::ThinkingError::Client(err)) => {
                    return responses_error(StatusCode::BAD_REQUEST, &err);
                }
                Err(crate::proxy::request::ThinkingError::ConfigAdapter(err)) => {
                    return responses_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("configuration error: {err}"),
                    );
                }
            };
            // Spend the request credit here — immediately before the request can go
            // out. Cooldown skips and validation failures never reach this line, so
            // they consume nothing; the check and the increment happen in one
            // transaction, so concurrent requests cannot overshoot.
            if !reserved {
                if let Some(token) = grant.token()
                    && let Some(response) = state.reserve_token_request(token, &plan.upstream_model)
                {
                    return response;
                }
                reserved = true;
            }
            let response = match plan.adapter() {
                AdapterKind::ResponsesFromChatCompletions => {
                    let original = original_responses_req
                        .as_ref()
                        .expect("original request body cloned for Chat adapter dispatch");
                    if let Err(err) = validate_responses_tools_shapes(original) {
                        if let Some(token_ctx) = grant.token() {
                            state.record_local_rejection_for_token(
                                &token_ctx.name,
                                &model_id,
                                "ingress_format",
                            );
                        }
                        return responses_error(StatusCode::BAD_REQUEST, &err);
                    }
                    let placeholder_enabled =
                        state.config().protection.placeholder_on_missing_history;
                    let requires_reasoning = plan
                        .compat
                        .effective_requires_reasoning_content_on_assistant_messages();
                    let chat_body = match convert::responses_to_chat_with_placeholder(
                        body,
                        &plan.upstream_model,
                        placeholder_enabled,
                        requires_reasoning,
                    ) {
                        Ok((body, events)) => {
                            // Record placeholder events to diagnostics
                            if !events.is_empty()
                                && let Some(diagnostics) = &state.diagnostics
                            {
                                for event in &events {
                                    let mut record = crate::diagnostics::DiagnosticRecord::new(
                                        "placeholder_used",
                                        crate::diagnostics::RequestFeatures::default(),
                                    );
                                    record.provider = plan.provider_id.clone();
                                    record.upstream_model = plan.upstream_model.clone();
                                    record.frontend_model = model_id.clone();
                                    if let Some(call_id) = &event.call_id {
                                        record.violations.push(json!({
                                            "reason": event.reason,
                                            "call_id": call_id
                                        }));
                                    } else {
                                        record.violations.push(json!({
                                            "reason": event.reason
                                        }));
                                    }
                                    diagnostics.record(record);
                                }
                            }
                            apply_disabled_thinking_egress(body, &plan, thinking_disabled)
                        }
                        // Per-candidate conversion failure keeps falling back.
                        Err(err) => {
                            last_error =
                                Some(responses_error(StatusCode::BAD_REQUEST, &err.to_string()));
                            continue;
                        }
                    };
                    forward_responses_via_chat(
                        &state,
                        &plan,
                        &model_id,
                        chat_body,
                        Some(original),
                        grant.token(),
                        ingress_start,
                    )
                    .await
                }
                AdapterKind::ResponsesFromAnthropic => {
                    let anthropic_body =
                        match convert::responses_to_anthropic(body, &plan.upstream_model) {
                            Ok(body) => body,
                            Err(err) => {
                                last_error = Some(responses_error(
                                    StatusCode::BAD_REQUEST,
                                    &err.to_string(),
                                ));
                                continue;
                            }
                        };
                    forward_responses_via_anthropic(
                        &state,
                        &plan,
                        &model_id,
                        anthropic_body,
                        grant.token(),
                        ingress_start,
                    )
                    .await
                }
                AdapterKind::Passthrough => {
                    let upstream_body =
                        apply_disabled_thinking_egress(body, &plan, thinking_disabled);
                    let upstream_body = strip_vendor_signatures(upstream_body);
                    forward_responses_native(
                        &state,
                        &plan,
                        &model_id,
                        &exec_model,
                        upstream_body,
                        grant.token(),
                        ingress_start,
                    )
                    .await
                }
                other => responses_error(
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "adapter {other:?} is not valid for Responses ingress; check provider endpoint declarations"
                    ),
                ),
            };
            let status = response.status();
            if is_upstream_generation_failure_response(&response) {
                return response;
            }
            if is_local_frequency_limited_response(&response) {
                freq_limited_next.push((exec_model.clone(), plan.clone()));
                last_error = Some(response);
                continue;
            }
            if is_local_rejection_response(&response) {
                // Our own rejection (e.g. an unrecoverable function-call history) is a
                // property of the request, not of this account: never cool a provider for
                // it and never count it. It must NOT abort the chain either — a later
                // candidate whose endpoint does not enforce the same shape (signature
                // requirements are per-candidate) may still serve the conversation, the
                // same way prep Reject keeps `continue`. The rejection is kept as the
                // last error, so if every candidate fails the client still gets the
                // local 400 instead of a blanket 429.
                last_error = Some(response);
                continue;
            }
            if is_protectable_client_error(status) && !has_dedicated_cooldown_path(status) {
                // 400 and the other generic 4xx (405/413/422 etc.) are request-level
                // errors — retrying on another provider would send the same bad body
                // upstream. Feed the fingerprint immediately and return without fallback.
                // Only 402/404/408 have a dedicated cooldown path and keep their
                // cooldown+fallback behavior.
                set_bad_request_cooldown(
                    &state,
                    &plan,
                    &exec_model,
                    &cooldown_key,
                    &response,
                    grant.token(),
                );
                state.observe_bad_request(&fingerprint);
                return response;
            }
            saw_client_error |= status == StatusCode::BAD_REQUEST;
            if should_fallback_without_cooldown(status) {
                last_error = Some(response);
                continue;
            }
            let cooldown = state
                .config()
                .effective_cooldown(&exec_model, &plan.provider_id);
            if let Some(duration) = resolve_candidate_cooldown_duration(
                &state,
                &plan,
                status,
                response.headers(),
                &cooldown,
            )
            .await
            {
                set_response_cooldown(
                    &state,
                    &plan,
                    &exec_model,
                    &cooldown_key,
                    status,
                    duration,
                    grant.token(),
                );
                last_error = Some(response);
                continue;
            }
            if skip_cooling {
                tracing::info!(
                    model = %model_id,
                    provider = %plan.provider_id,
                    "request succeeded via a healthy provider while others are cooling"
                );
            }
            return response;
        }
        // This pass exhausted every candidate in `replay` without a success.
        // Shrink the re-scan set to what is still frequency-limited this pass:
        // a candidate that recovered, sent, and failed for a non-frequency
        // reason (or cooled into a silent skip) is done — keep its real
        // upstream result as `last_error` and stop once nobody remains.
        freq_limited = freq_limited_next;
        match last_ditch {
            None => {
                if freq_limited.is_empty() || last_ditch_budget.is_zero() {
                    break 'passes;
                }
                last_ditch = Some(Instant::now() + last_ditch_budget);
            }
            Some(deadline) => {
                if freq_limited.is_empty() || Instant::now() >= deadline {
                    break 'passes;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }

    // §10.5.8: the fingerprint block is fed only when every candidate failed
    // and at least one failure was a client error.
    if saw_client_error {
        state.observe_bad_request(&fingerprint);
    }
    let final_response =
        last_error.unwrap_or_else(|| empty_candidates_response(responses_error, &model_id));
    if is_local_rejection_response(&final_response)
        && let Some(token_ctx) = grant.token()
    {
        state.record_local_rejection_for_token(&token_ctx.name, &model_id, "known_precheck");
    }
    final_response
}

async fn anthropic_messages(
    State(state): State<AppState>,
    Extension(grant): Extension<crate::token::auth::TokenGrant>,
    JsonWithBytes {
        value: mut body,
        bytes: raw_body,
    }: JsonWithBytes,
) -> Response {
    let ingress_start = std::time::Instant::now();
    if let Some(err) = check_token_model_access(&grant, &body) {
        return err;
    }
    // resolve routing model chain.
    let routing_model_id =
        match resolve_routing(&state, Protocol::Anthropic, &mut body, anthropic_error) {
            Ok(r) => r,
            Err(response) => {
                if let Some(token_ctx) = grant.token() {
                    let model = body
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    state.record_local_rejection_for_token(
                        &token_ctx.name,
                        model,
                        "ingress_routing",
                    );
                }
                return *response;
            }
        };
    let model_id = client_visible_model_id(routing_model_id.as_deref(), &body);
    if let Some(token_ctx) = grant.token() {
        let cd_status = state.check_token_error_cooldown(&token_ctx.name, &model_id);
        if cd_status.blocked {
            let msg = match cd_status.reason.as_deref() {
                Some("local_reject_tolerance") => {
                    "token blocked after repeated bad requests; wait for cooldown to expire"
                }
                _ => "token blocked after repeated upstream errors; wait for cooldown to expire",
            };
            return anthropic_error(StatusCode::TOO_MANY_REQUESTS, msg);
        }
    }
    let candidates = match resolve_request_candidates(
        &state,
        Protocol::Anthropic,
        &body,
        &model_id,
        anthropic_error,
    ) {
        Ok(candidates) => candidates,
        Err(response) => {
            if let Some(token_ctx) = grant.token() {
                state.record_local_rejection_for_token(
                    &token_ctx.name,
                    &model_id,
                    "ingress_format",
                );
            }
            return *response;
        }
    };
    // RoundRobin rotation for multi-provider load balancing.
    let candidates = state.rotate_candidates_round_robin(&model_id, candidates);
    let thinking_intent = extract_thinking_intent(Protocol::Anthropic, &body);
    let fingerprint = crate::protection::fingerprint(
        Protocol::Anthropic,
        body.get("model").and_then(Value::as_str),
        &raw_body,
    );
    if state.bad_request_blocked(&fingerprint) {
        if let Some(token_ctx) = grant.token() {
            state.record_local_rejection_for_token(&token_ctx.name, &model_id, "fingerprint_block");
        }
        return anthropic_error(
            StatusCode::TOO_MANY_REQUESTS,
            "request temporarily blocked after repeated upstream bad requests",
        );
    }
    // §10.5.5: until the first byte is written downstream, streaming requests
    // follow the same cooldown/fallback policy as non-streaming ones.
    let token = grant.token();
    let skip_cooling =
        match candidate_cooldown_gate(&state, &candidates, &model_id, anthropic_error, token) {
            Ok(skip_cooling) => skip_cooling,
            Err(response) => return *response,
        };
    let mut last_error: Option<Response> = None;
    let mut saw_client_error = false;
    // One credit per client request: the first iteration that reaches dispatch
    // spends it, so internal fallback does not charge twice.
    let mut reserved = false;
    // Frequency-limited candidates are re-scanned in a last-ditch pass under
    // one shared queue_timeout budget (candidate order, 25ms between passes);
    // the fast pass itself never waits on an individual candidate.
    let mut freq_limited: Vec<(String, ExecutionPlan)> = Vec::new();
    let last_ditch_budget = Duration::from_secs(
        candidates
            .iter()
            .map(|(_, p)| p.request_frequency.queue_timeout_seconds.unwrap_or(10))
            .max()
            .unwrap_or(10),
    );
    let mut last_ditch: Option<Instant> = None;

    'passes: loop {
        // This pass's re-scan set: rebuilt from scratch so a candidate that
        // regains capacity and produces a real upstream result (5xx/401/
        // transport ...) leaves the set instead of being replayed every pass
        // until the shared budget expires.
        let mut freq_limited_next: Vec<(String, ExecutionPlan)> = Vec::new();
        let replay: Vec<(String, ExecutionPlan)> = match &last_ditch {
            None => candidates.clone(),
            Some(_) => freq_limited.clone(),
        };
        for (exec_model, mut plan) in replay {
            // When routing occurred, record the leaf target for usage attribution
            if routing_model_id.is_some() {
                plan.routed_target = Some(exec_model.clone());
            }
            let cooldown_key = plan.cooldown_key(&exec_model);
            if skip_cooling && state.is_cooling_down(&cooldown_key) {
                state.cooldowns.record_cooldown_hit(&cooldown_key);
                continue;
            }
            if token_candidate_cooling(&state, &plan, &exec_model, token) {
                last_error = Some(anthropic_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    &format!(
                        "provider {:?} is cooling down for this token after a rejected request; retry later",
                        plan.provider_id
                    ),
                ));
                continue;
            }
            // Per-account cumulative bad-request budget: skip this candidate when
            // the token's daily or lifetime layer is exhausted, allowing fallback
            // to other candidates .
            if let Some(token_ctx) = token {
                let (budget_reason, budget_diag) = state.account_budget_blocked(&token_ctx.key);
                match budget_reason {
                    crate::protection::BudgetBlockReason::DailyExhausted => {
                        tracing::info!(
                            event = "account_budget_blocked",
                            reason = "daily",
                            token = %token_ctx.name,
                            provider = %plan.provider_id,
                            daily_date = %budget_diag.daily_date,
                            today = %budget_diag.today,
                            daily_count = budget_diag.daily_count,
                            lifetime_count = budget_diag.lifetime_count,
                            "account bad-request budget exhausted; skipping candidate, fallback continues"
                        );
                        last_error = Some(anthropic_error(
                        StatusCode::TOO_MANY_REQUESTS,
                        &crate::protection::AccountBadBudgetManager::format_daily_recovery_message(
                            &token_ctx.key,
                        ),
                    ));
                        continue;
                    }
                    crate::protection::BudgetBlockReason::LifetimeExhausted => {
                        tracing::info!(
                            event = "account_budget_blocked",
                            reason = "lifetime",
                            token = %token_ctx.name,
                            provider = %plan.provider_id,
                            daily_date = %budget_diag.daily_date,
                            today = %budget_diag.today,
                            daily_count = budget_diag.daily_count,
                            lifetime_count = budget_diag.lifetime_count,
                            "account lifetime bad-request budget exhausted; skipping candidate, admin reset required"
                        );
                        last_error = Some(anthropic_error(
                            StatusCode::TOO_MANY_REQUESTS,
                            "account triggered lifetime bad-request quota; admin reset required",
                        ));
                        continue;
                    }
                    crate::protection::BudgetBlockReason::Allowed => {}
                }
            }
            let (body, thinking_disabled) = match prepare_candidate_thinking_body(
                &state,
                Protocol::Anthropic,
                &body,
                &exec_model,
                &plan,
                &thinking_intent,
            ) {
                Ok(prepared) => prepared,
                Err(crate::proxy::request::ThinkingError::Client(err)) => {
                    return anthropic_error(StatusCode::BAD_REQUEST, &err);
                }
                Err(crate::proxy::request::ThinkingError::ConfigAdapter(err)) => {
                    return anthropic_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("configuration error: {err}"),
                    );
                }
            };
            // Spend the request credit here — immediately before the request can go
            // out. Cooldown skips and validation failures never reach this line, so
            // they consume nothing; the check and the increment happen in one
            // transaction, so concurrent requests cannot overshoot.
            if !reserved {
                if let Some(token) = grant.token()
                    && let Some(response) = state.reserve_token_request(token, &plan.upstream_model)
                {
                    return response;
                }
                reserved = true;
            }
            let response = match plan.adapter() {
                AdapterKind::AnthropicFromChatCompletions => {
                    // Dispatch arms are mutually exclusive and the prepared body is a
                    // per-candidate local consumed by this arm only — move it in (S3b).
                    let chat_body = match convert::anthropic_to_chat(body, &plan.upstream_model) {
                        Ok(body) => apply_disabled_thinking_egress(body, &plan, thinking_disabled),
                        // Per-candidate conversion failure keeps falling back.
                        Err(err) => {
                            last_error =
                                Some(anthropic_error(StatusCode::BAD_REQUEST, &err.to_string()));
                            continue;
                        }
                    };
                    forward_anthropic_via_chat(
                        &state,
                        &plan,
                        &model_id,
                        chat_body,
                        grant.token(),
                        ingress_start,
                    )
                    .await
                }
                AdapterKind::AnthropicFromResponses => {
                    let responses_body =
                        match convert::anthropic_to_responses_request(body, &plan.upstream_model) {
                            Ok(body) => body,
                            Err(err) => {
                                last_error = Some(anthropic_error(
                                    StatusCode::BAD_REQUEST,
                                    &err.to_string(),
                                ));
                                continue;
                            }
                        };
                    forward_anthropic_via_responses(
                        &state,
                        &plan,
                        &model_id,
                        &exec_model,
                        responses_body,
                        grant.token(),
                        ingress_start,
                    )
                    .await
                }
                AdapterKind::Passthrough => {
                    let upstream_body =
                        apply_disabled_thinking_egress(body, &plan, thinking_disabled);
                    let upstream_body = strip_vendor_signatures(upstream_body);
                    forward_anthropic_native(
                        &state,
                        &plan,
                        &model_id,
                        upstream_body,
                        grant.token(),
                        ingress_start,
                    )
                    .await
                }
                other => anthropic_error(
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "adapter {other:?} is not valid for Anthropic ingress; check provider endpoint declarations"
                    ),
                ),
            };
            let status = response.status();
            if is_upstream_generation_failure_response(&response) {
                return response;
            }
            if is_local_frequency_limited_response(&response) {
                freq_limited_next.push((exec_model.clone(), plan.clone()));
                last_error = Some(response);
                continue;
            }
            if is_local_rejection_response(&response) {
                // Our own rejection (e.g. an unrecoverable function-call history) is a
                // property of the request, not of this account: never cool a provider for
                // it and never count it. It must NOT abort the chain either — a later
                // candidate whose endpoint does not enforce the same shape (signature
                // requirements are per-candidate) may still serve the conversation, the
                // same way prep Reject keeps `continue`. The rejection is kept as the
                // last error, so if every candidate fails the client still gets the
                // local 400 instead of a blanket 429.
                last_error = Some(response);
                continue;
            }
            if is_protectable_client_error(status) && !has_dedicated_cooldown_path(status) {
                // 400 and the other generic 4xx (405/413/422 etc.) are request-level
                // errors — retrying on another provider would send the same bad body
                // upstream. Feed the fingerprint immediately and return without fallback.
                // Only 402/404/408 have a dedicated cooldown path and keep their
                // cooldown+fallback behavior.
                set_bad_request_cooldown(
                    &state,
                    &plan,
                    &exec_model,
                    &cooldown_key,
                    &response,
                    grant.token(),
                );
                state.observe_bad_request(&fingerprint);
                return response;
            }
            saw_client_error |= status == StatusCode::BAD_REQUEST;
            if should_fallback_without_cooldown(status) {
                last_error = Some(response);
                continue;
            }
            let cooldown = state
                .config()
                .effective_cooldown(&exec_model, &plan.provider_id);
            if let Some(duration) = resolve_candidate_cooldown_duration(
                &state,
                &plan,
                status,
                response.headers(),
                &cooldown,
            )
            .await
            {
                set_response_cooldown(
                    &state,
                    &plan,
                    &exec_model,
                    &cooldown_key,
                    status,
                    duration,
                    grant.token(),
                );
                last_error = Some(response);
                continue;
            }
            if skip_cooling {
                tracing::info!(
                    model = %model_id,
                    provider = %plan.provider_id,
                    "request succeeded via a healthy provider while others are cooling"
                );
            }
            return response;
        }
        // This pass exhausted every candidate in `replay` without a success.
        // Shrink the re-scan set to what is still frequency-limited this pass:
        // a candidate that recovered, sent, and failed for a non-frequency
        // reason (or cooled into a silent skip) is done — keep its real
        // upstream result as `last_error` and stop once nobody remains.
        freq_limited = freq_limited_next;
        match last_ditch {
            None => {
                if freq_limited.is_empty() || last_ditch_budget.is_zero() {
                    break 'passes;
                }
                last_ditch = Some(Instant::now() + last_ditch_budget);
            }
            Some(deadline) => {
                if freq_limited.is_empty() || Instant::now() >= deadline {
                    break 'passes;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }

    // §10.5.8: the fingerprint block is fed only when every candidate failed
    // and at least one failure was a client error.
    if saw_client_error {
        state.observe_bad_request(&fingerprint);
    }
    let final_response =
        last_error.unwrap_or_else(|| empty_candidates_response(anthropic_error, &model_id));
    if is_local_rejection_response(&final_response)
        && let Some(token_ctx) = grant.token()
    {
        state.record_local_rejection_for_token(&token_ctx.name, &model_id, "known_precheck");
    }
    final_response
}

fn request_model_id(body: &Value) -> &str {
    body.get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

/// The client-visible model identity: the routing model the client requested when
/// one was resolved, otherwise the requested model itself. Used for response
/// bodies, usage attribution, and client-facing error text so a client never sees
/// an internal target it did not ask for.
fn client_visible_model_id(routing_model_id: Option<&str>, body: &Value) -> String {
    match routing_model_id {
        Some(id) => id.to_string(),
        None => request_model_id(body).to_string(),
    }
}

/// Empty candidate list is a configuration problem, not a cooldown.
fn empty_candidates_response(error: fn(StatusCode, &str) -> Response, model_id: &str) -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!(
            "model {model_id:?} has no usable provider binding; check provider names, endpoint declarations, and auth configuration"
        ),
    )
}

/// §3.1: the short-circuit branch for request-level client errors is terminal
/// for the candidate chain, but a real upstream 400 still scopes its
/// `client_error_seconds` cooldown to the requesting (token × provider) pair
/// before the chain returns.
fn set_bad_request_cooldown(
    state: &AppState,
    plan: &ExecutionPlan,
    exec_model: &str,
    cooldown_key: &crate::config::CooldownKey,
    response: &Response,
    token: Option<&crate::token::auth::TokenContext>,
) {
    let status = response.status();
    if status != StatusCode::BAD_REQUEST {
        return;
    }
    let cooldown = state
        .config()
        .effective_cooldown(exec_model, &plan.provider_id);
    if let Some(duration) = cooldown_duration_for_response(status, response.headers(), &cooldown) {
        set_response_cooldown(
            state,
            plan,
            exec_model,
            cooldown_key,
            status,
            duration,
            token,
        );
    }
}

#[allow(clippy::collapsible_if)]
async fn resolve_candidate_cooldown_duration(
    _state: &AppState,
    plan: &ExecutionPlan,
    status: StatusCode,
    headers: &HeaderMap,
    cooldown: &crate::config::FallbackCooldownConfig,
) -> Option<Duration> {
    if status == StatusCode::TOO_MANY_REQUESTS
        && let Some(duration) = crate::proxy::sse_aggregate::retry_after_duration(headers)
    {
        tracing::info!(
            event = "cooldown_duration_resolved",
            path = "retry_after",
            provider = %plan.provider_id,
            duration_secs = duration.as_secs(),
        );
        return Some(duration);
    }

    if let Some(duration) =
        crate::proxy::sse_aggregate::cooldown_duration_for_status(status, cooldown)
    {
        tracing::info!(
            event = "cooldown_duration_resolved",
            path = "default",
            provider = %plan.provider_id,
            duration_secs = duration.as_secs(),
        );
        Some(duration)
    } else {
        None
    }
}

/// §3.1: a real upstream 400 is attributed to the requesting
/// (token × provider) pair so the provider keeps serving everyone else;
/// health-family statuses (429/5xx) keep the provider-granular key. Open mode
/// (no token) has no requester to attribute to — provider granularity stays.
fn set_response_cooldown(
    state: &AppState,
    plan: &ExecutionPlan,
    exec_model: &str,
    provider_key: &crate::config::CooldownKey,
    status: StatusCode,
    duration: Duration,
    token: Option<&crate::token::auth::TokenContext>,
) {
    let key = if status == StatusCode::BAD_REQUEST {
        token.map_or_else(
            || provider_key.clone(),
            |token| plan.token_cooldown_key(exec_model, &token.name, Some(&token.key_hash())),
        )
    } else {
        provider_key.clone()
    };
    state.set_cooldown(
        &key,
        cooldown_kind_for_status(status),
        &status.to_string(),
        duration,
    );
}

/// §3.1: a token-scoped (upstream-400) cooldown is requester
/// attribution, not provider health — it skips the candidate regardless of
/// `skip_cooling` and never feeds `saw_client_error`.
fn token_candidate_cooling(
    state: &AppState,
    plan: &ExecutionPlan,
    exec_model: &str,
    token: Option<&crate::token::auth::TokenContext>,
) -> bool {
    token.is_some_and(|t| {
        let key = plan.token_cooldown_key(exec_model, &t.name, Some(&t.key_hash()));
        let cooling = state.is_cooling_down(&key);
        if cooling {
            state.cooldowns.record_cooldown_hit(&key);
        }
        cooling
    })
}

fn remaining_cooldown_seconds(entry: &crate::cooldown::CooldownEntry) -> u64 {
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    entry.expires_at_unix.saturating_sub(now).max(1)
}

fn persistent_all_cooling_response(
    state: &AppState,
    candidates: &[(String, ExecutionPlan)],
    model_id: &str,
    error: fn(StatusCode, &str) -> Response,
    token: Option<&crate::token::auth::TokenContext>,
) -> Response {
    let mut details = Vec::new();
    let mut retry_after_secs = u64::MAX;
    for (frontend_model, plan) in candidates {
        let Some(entry) = state
            .active_cooldown(&plan.cooldown_key(frontend_model))
            .or_else(|| {
                token.and_then(|t| {
                    state.active_cooldown(&plan.token_cooldown_key(
                        frontend_model,
                        &t.name,
                        Some(&t.key_hash()),
                    ))
                })
            })
        else {
            continue;
        };
        let remaining = remaining_cooldown_seconds(&entry);
        retry_after_secs = retry_after_secs.min(remaining);
        details.push(format!(
            "{}: {}s remaining ({}/{})",
            plan.provider_id, remaining, entry.kind, entry.reason
        ));
    }
    let retry_after_secs = if retry_after_secs == u64::MAX {
        1
    } else {
        retry_after_secs
    };
    let message = format!(
        "model {model_id:?} has all providers cooling down for persistent failures: {}",
        details.join("; ")
    );
    with_retry_after(
        error(StatusCode::SERVICE_UNAVAILABLE, &message),
        Some(
            HeaderValue::from_str(&retry_after_secs.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("1")),
        ),
    )
}

/// Returns whether cooling candidates should be skipped, or a local 503 when
/// every candidate is in a persistent cooldown. Transient all-cooling still
/// probes (ADR-005). An empty candidate list is a configuration error.
/// §3.1: token-scoped cooldowns are checked too — when every
/// candidate is cooling *for this token*, the requester sees the same
/// all-cooling/Retry-After shape, but no other user of those providers does.
fn candidate_cooldown_gate(
    state: &AppState,
    candidates: &[(String, ExecutionPlan)],
    model_id: &str,
    error: fn(StatusCode, &str) -> Response,
    token: Option<&crate::token::auth::TokenContext>,
) -> Result<bool, Box<Response>> {
    if candidates.is_empty() {
        return Err(Box::new(empty_candidates_response(error, model_id)));
    }
    if let Some(token) = token
        && candidates.iter().all(|(frontend_model, plan)| {
            state.is_cooling_down(&plan.token_cooldown_key(
                frontend_model,
                &token.name,
                Some(&token.key_hash()),
            ))
        })
    {
        return Err(Box::new(persistent_all_cooling_response(
            state,
            candidates,
            model_id,
            error,
            Some(token),
        )));
    }
    let active: Vec<_> = candidates
        .iter()
        .filter_map(|(frontend_model, plan)| {
            state.active_cooldown(&plan.cooldown_key(frontend_model))
        })
        .collect();
    if active.len() != candidates.len() {
        return Ok(true);
    }
    if active
        .iter()
        .all(|entry| !cooldown_kind_is_transient(&entry.kind))
    {
        for (frontend_model, plan) in candidates {
            state.is_cooling_down(&plan.cooldown_key(frontend_model));
        }
        return Err(Box::new(persistent_all_cooling_response(
            state, candidates, model_id, error, None,
        )));
    }
    Ok(false)
}

/// Request-time model resolution: a missing/unknown model ID and
/// a known model without a binding for the request protocol are explicit
/// client-protocol-shaped 4xx errors — never a silent default substitution.
/// Request-content capability filtering follows the trust contract: requests
/// are only rejected locally when the model explicitly does not declare the
/// matching feature.
async fn send_upstream(
    state: &AppState,
    plan: &ExecutionPlan,
    req: reqwest::RequestBuilder,
    stream: bool,
) -> Result<reqwest::Response, UpstreamSendFailure> {
    // §10.5.5: until the first byte is written downstream, network errors and
    // HTTP 5xx are retried uniformly for streaming and non-streaming requests.
    let cfg = state.config();
    let transport_attempts = cfg.fallback.max_retries.saturating_add(1).max(1);
    let server_error_attempts = cfg.fallback.server_error_retries.saturating_add(1).max(1);
    let retry_interval = Duration::from_millis(cfg.fallback.server_error_retry_interval_ms);
    let timeout = request_timeout(&cfg);
    drop(cfg);
    let mut last_err = None;
    let mut last_server_error: Option<reqwest::Response> = None;
    let mut transport_attempt = 0u32;
    let mut server_error_attempt = 0u32;
    let mut sequence_head = true;
    loop {
        // Sequence head: fail fast with zero wait so the candidate loop can
        // skip to the next bound provider; once a send attempt has actually
        // been made, slot acquisition is retry pacing and keeps waiting for
        // the sequence to finish instead of restarting it.
        let acquired = if sequence_head {
            sequence_head = false;
            state.try_acquire_frequency_now(plan).await
        } else {
            state.acquire_frequency(plan).await
        };
        if !acquired {
            return Err(UpstreamSendFailure::FrequencyLimited);
        }
        let Some(candidate) = req.try_clone() else {
            break;
        };
        match send_with_timeout(candidate, timeout, stream).await {
            Ok(resp) if should_retry_server_error(resp.status()) => {
                last_server_error = Some(resp);
                server_error_attempt = server_error_attempt.saturating_add(1);
                if server_error_attempt >= server_error_attempts {
                    break;
                }
                if !retry_interval.is_zero() {
                    tokio::time::sleep(retry_interval).await;
                }
            }
            Ok(resp) => return Ok(resp),
            Err(err) => {
                last_err = Some(err);
                transport_attempt = transport_attempt.saturating_add(1);
                if transport_attempt >= transport_attempts {
                    break;
                }
            }
        }
    }
    if let Some(resp) = last_server_error {
        return Ok(resp);
    }
    match last_err {
        Some(err) => Err(err),
        None => {
            // The loop broke before any attempt (req not cloneable), so this is
            // still the sequence head of an unsent candidate: fail fast exactly
            // like the head acquire above and let the candidate loop decide.
            if !state.try_acquire_frequency_now(plan).await {
                return Err(UpstreamSendFailure::FrequencyLimited);
            }
            send_with_timeout(req, timeout, stream).await
        }
    }
}

/// Timeout application splits by request shape. Non-streaming
/// requests keep the reqwest total timeout over the whole transaction; streaming
/// requests bound only the response-header (TTFB) wait — the body may
/// legitimately stream far past any request-level budget (spec.md §10.5.5:
/// "streaming requests have no overall timeout").
async fn send_with_timeout(
    req: reqwest::RequestBuilder,
    timeout: Duration,
    stream: bool,
) -> Result<reqwest::Response, UpstreamSendFailure> {
    if stream {
        match tokio::time::timeout(timeout, req.send()).await {
            Ok(result) => result.map_err(UpstreamSendFailure::Transport),
            Err(_elapsed) => Err(UpstreamSendFailure::TimedOut),
        }
    } else {
        req.timeout(timeout)
            .send()
            .await
            .map_err(UpstreamSendFailure::Transport)
    }
}

fn upstream_send_failure_response(
    err: UpstreamSendFailure,
    error: fn(StatusCode, &str) -> Response,
) -> Response {
    match err {
        UpstreamSendFailure::FrequencyLimited => {
            let mut response = error(
                StatusCode::TOO_MANY_REQUESTS,
                "provider frequency limit exceeded before contacting upstream",
            );
            response.headers_mut().insert(
                "x-llm-proxy-local-frequency-limit",
                HeaderValue::from_static("1"),
            );
            response
        }
        UpstreamSendFailure::Transport(err) => error(
            StatusCode::BAD_GATEWAY,
            // {:#} 展开 source 链：连接拒绝/超时/DNS 失败对用户可区分。
            &format!("upstream request failed: {:#}", anyhow::Error::new(err)),
        ),
        UpstreamSendFailure::TimedOut => error(
            StatusCode::BAD_GATEWAY,
            "upstream request failed: timed out before response headers",
        ),
    }
}

fn is_local_frequency_limited_response(response: &Response) -> bool {
    response
        .headers()
        .get("x-llm-proxy-local-frequency-limit")
        .is_some()
}

/// marker for responses the proxy rejected on its own (e.g. an unrecoverable
/// function-call history). Such a verdict is a property of the request, never of the
/// account that happened to serve it: it must not feed cooldowns or budget counts.
/// The candidate chain keeps going (a later candidate may not enforce the same
/// shape); the marker rides the stored rejection so that when every candidate fails
/// the client still gets this local verdict rather than a blanket 429.
pub(super) const LOCAL_REJECTION_HEADER: &str = "x-llm-proxy-local-rejection";

fn is_local_rejection_response(response: &Response) -> bool {
    response.headers().get(LOCAL_REJECTION_HEADER).is_some()
}

fn request_timeout(cfg: &Config) -> Duration {
    let seconds = cfg
        .fallback
        .timeout_seconds
        .min(cfg.fallback.max_timeout_seconds)
        .max(1);
    Duration::from_secs(seconds)
}

#[allow(clippy::too_many_arguments)]
fn record_usage_from_responses_value(
    state: &AppState,
    value: &Value,
    frontend_model: &str,
    provider_id: &str,
    endpoint: &str,
    latency_ms: Option<i64>,
    token: Option<&crate::token::auth::TokenContext>,
    is_streaming: bool,
    response_bytes: u64,
    ttfb_ms: Option<u64>,
    routed_target: Option<String>,
) {
    let input_tokens = value
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = value
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if input_tokens > 0 || output_tokens > 0 {
        // : detect truncation from Responses status.
        let (truncated, truncation_reason) =
            if value.get("status").and_then(Value::as_str) == Some("incomplete") {
                let reason = value
                    .get("incomplete_details")
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str)
                    .map(|r| match r {
                        "max_output_tokens" => "max_tokens",
                        _ => "other",
                    });
                (true, reason)
            } else {
                (false, None)
            };
        state.record_usage_with_status_and_target(
            frontend_model.to_string(),
            provider_id.to_string(),
            endpoint.to_string(),
            input_tokens as i64,
            output_tokens as i64,
            latency_ms,
            None,
            None,
            token,
            Some("completed"),
            is_streaming,
            response_bytes,
            truncated,
            truncation_reason,
            ttfb_ms,
            routed_target,
        );
    }
}

#[allow(dead_code)]
fn upstream_replay_error_log_summary(text: &str) -> String {
    let normalized = text.to_ascii_lowercase();
    let error_kind = if normalized.contains("thoughtsignature")
        || normalized.contains("thought_signature")
        || normalized.contains("thought signature")
    {
        "replay_signature_rejected"
    } else if normalized.contains("function")
        && (normalized.contains("does not match")
            || normalized.contains("mismatch")
            || normalized.contains("missing")
            || normalized.contains("not found"))
    {
        "tool_call_mismatch"
    } else if normalized.contains("context window")
        || normalized.contains("context length")
        || normalized.contains("token limit")
    {
        "context_limit"
    } else {
        "unclassified_upstream_error"
    };
    format!(
        "{error_kind}; upstream error body omitted ({} chars)",
        text.chars().count()
    )
}

/// Response-shaped wrapper around `aggregate_responses_sse_to_value`: records
/// usage and rewrites the model to the frontend model.
#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_replay_summary {
    use super::*;

    #[test]
    fn test_upstream_replay_error_log_summary_classifies_without_leaking_sensitive_data() {
        let sensitive_input = r#"{
            "error": {
                "message": "Invalid thoughtSignature: sig_abc_12345",
                "token": "access_token:ya29.secret_bearer_token"
            },
            "user_tool_arguments": {
                "file_path": "/home/user/secret.txt",
                "query": "select secret_data from vault"
            }
        }"#;

        let summary = upstream_replay_error_log_summary(sensitive_input);
        let expected = format!(
            "replay_signature_rejected; upstream error body omitted ({} chars)",
            sensitive_input.chars().count()
        );

        assert_eq!(summary, expected);
        assert!(!summary.contains("thoughtSignature"));
        assert!(!summary.contains("sig_abc_12345"));
        assert!(!summary.contains("access_token"));
        assert!(!summary.contains("ya29.secret_bearer_token"));
        assert!(!summary.contains("user_tool_arguments"));
        assert!(!summary.contains("file_path"));
        assert!(!summary.contains("/home/user/secret.txt"));
        assert!(!summary.contains("query"));
        assert!(!summary.contains("select secret_data from vault"));
    }

    #[test]
    fn test_upstream_replay_error_log_summary_uses_safe_fixed_categories() {
        let cases = [
            (
                "Function response does not match call",
                "tool_call_mismatch",
            ),
            ("Request exceeds context window", "context_limit"),
            ("opaque upstream failure", "unclassified_upstream_error"),
        ];

        for (message, expected_kind) in cases {
            let summary = upstream_replay_error_log_summary(message);
            assert_eq!(
                summary,
                format!(
                    "{expected_kind}; upstream error body omitted ({} chars)",
                    message.chars().count()
                )
            );
            assert!(!summary.contains(message));
        }
    }
}

#[cfg(test)]
static TEST_APP_STATE_HOOK: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<AppState>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static TEST_STATE_HOOK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests_fail_stop {
    use super::*;

    #[tokio::test]
    async fn gateway_terminates_with_error_on_fatal_config_fail_stop() {
        let mut cfg = crate::config::default_deepseek_config();
        if let Some(ref mut server) = cfg.server {
            server.listen = "127.0.0.1:0".to_string();
        }
        let state = AppState::new_in_memory_for_management(cfg);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let fatal = state.fatal_config_state();
        let server_task = tokio::spawn(serve_state_with_shutdown(state.clone(), shutdown_rx));

        // Signal fatal config fail-stop
        fatal.fail();

        let result = server_task.await.expect("server task join must complete");
        assert!(
            result.is_err(),
            "fatal fail-stop must cause serve_state_with_shutdown to return an error"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("fail-stop"),
            "error message should indicate fail-stop: {err_msg}"
        );
        // 本用例只验证 serve 侧的 fail-stop 传播；writer 关闭结论的三态由
        // `runtime_config` 与 `service` 的针对性用例断言。
        let _writer_result = state.shutdown_config_writer().await;
    }

    #[tokio::test]
    async fn serve_with_shutdown_normal_shutdown_awaits_config_writer_shutdown() {
        let _lock = TEST_STATE_HOOK_LOCK.lock().await;
        let (state_tx, state_rx) = tokio::sync::oneshot::channel();
        {
            let mut guard = TEST_APP_STATE_HOOK.lock().expect("lock hook");
            *guard = Some(state_tx);
        }

        let mut cfg = crate::config::default_deepseek_config();
        if let Some(ref mut server) = cfg.server {
            server.listen = "127.0.0.1:0".to_string();
        }
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let isolated = tempfile::tempdir().expect("isolated config dir");

        let server_task = tokio::spawn(serve_with_shutdown_at(
            cfg.clone(),
            isolated.path().join("config.toml"),
            shutdown_rx,
        ));
        let state = state_rx
            .await
            .expect("captured AppState from serve_with_shutdown");

        shutdown_tx
            .send("test_normal_shutdown")
            .expect("send shutdown signal");
        let result = server_task.await.expect("server task join must complete");
        assert_eq!(
            result.expect("serve_with_shutdown must succeed"),
            "test_normal_shutdown"
        );

        let write_result = state.commit_config(cfg, 0).await;
        assert_eq!(
            write_result,
            Err(crate::runtime_config::WriterError::ServiceShuttingDown),
            "writer operation on captured state must be rejected because config writer was shut down"
        );
        assert!(
            state.config_writer_task.lock().await.is_none(),
            "config_writer_task join handle must have been taken and joined"
        );
    }

    #[tokio::test]
    async fn serve_with_shutdown_fatal_fail_stop_awaits_config_writer_shutdown() {
        let _lock = TEST_STATE_HOOK_LOCK.lock().await;
        let (state_tx, state_rx) = tokio::sync::oneshot::channel();
        {
            let mut guard = TEST_APP_STATE_HOOK.lock().expect("lock hook");
            *guard = Some(state_tx);
        }

        let mut cfg = crate::config::default_deepseek_config();
        if let Some(ref mut server) = cfg.server {
            server.listen = "127.0.0.1:0".to_string();
        }
        let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let isolated = tempfile::tempdir().expect("isolated config dir");

        let server_task = tokio::spawn(serve_with_shutdown_at(
            cfg.clone(),
            isolated.path().join("config.toml"),
            shutdown_rx,
        ));
        let state = state_rx
            .await
            .expect("captured AppState from serve_with_shutdown");
        let fatal = state.fatal_config_state();

        assert!(
            state.config_writer_task.lock().await.is_some(),
            "config_writer_task must be active before triggering fatal fail-stop"
        );

        // Signal fatal config fail-stop
        fatal.fail();

        let result = server_task.await.expect("server task join must complete");
        assert!(
            result.is_err(),
            "fatal fail-stop must cause serve_with_shutdown to return an error"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("fail-stop"),
            "error message should indicate fail-stop: {err_msg}"
        );
        assert!(
            state.config_writer_task.lock().await.is_none(),
            "config_writer_task join handle must have been taken and joined"
        );

        let write_result = state.commit_config(cfg, 0).await;
        assert_eq!(
            write_result,
            Err(crate::runtime_config::WriterError::ServiceShuttingDown),
            "writer operation on captured state must be rejected because config writer was shut down"
        );
    }

    /// 回归闸门：任何测试把 AppState 绑到用户真实 config 路径上都必须立刻炸掉，
    /// 否则 `recover_orphan_oauth_providers` 会把用户配置覆盖成测试夹具。
    #[test]
    #[should_panic(expected = "tests must not open the live user config path")]
    fn app_state_new_rejects_the_live_user_config_path() {
        let _ = AppState::new(
            crate::config::default_deepseek_config(),
            crate::config::default_config_path(),
        );
    }

    // ── Usage History: parse_history_period ─────────────────────────────

    #[test]
    fn parse_history_period_today_starts_at_local_midnight() {
        let (start, _end) = super::parse_history_period("today").unwrap();
        let local_midnight = chrono::Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .single()
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            start, local_midnight,
            "today 窗口起点必须是系统本地时区的 00:00（而非 UTC 00:00）"
        );
    }

    #[test]
    fn parse_history_period_today() {
        let result = super::parse_history_period("today");
        assert!(result.is_ok());
        let (start, end) = result.unwrap();
        assert!(start <= end);
    }

    #[test]
    fn parse_history_period_7d() {
        let result = super::parse_history_period("7d");
        assert!(result.is_ok());
        let (start, end) = result.unwrap();
        let span = end - start;
        assert!(span.num_days() >= 6 && span.num_days() <= 8);
    }

    #[test]
    fn parse_history_period_90d() {
        let result = super::parse_history_period("90d");
        assert!(result.is_ok());
    }

    #[test]
    fn parse_history_period_exceeds_90d() {
        let result = super::parse_history_period("91d");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds maximum"));
    }

    #[test]
    fn parse_history_period_invalid() {
        assert!(super::parse_history_period("abc").is_err());
        assert!(super::parse_history_period("").is_err());
    }

    #[test]
    fn parse_history_period_date_range() {
        let result = super::parse_history_period("2026-09-01:2026-09-22");
        assert!(result.is_ok());
        let (start, end) = result.unwrap();
        assert!(start < end);
    }

    #[test]
    fn parse_history_period_date_range_exceeds_90d() {
        let result = super::parse_history_period("2026-01-01:2026-09-22");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds maximum"));
    }

    // ── Usage History: Rate Limiter ─────────────────────────────────────

    #[test]
    fn rate_limiter_allows_10_per_minute() {
        let limiter = super::UsageHistoryRateLimiter::new();
        for _ in 0..10 {
            assert!(limiter.check("token-1"));
        }
        // 11th should be rejected
        assert!(!limiter.check("token-1"));
    }

    #[test]
    fn rate_limiter_is_per_token() {
        let limiter = super::UsageHistoryRateLimiter::new();
        // Exhaust token-a
        for _ in 0..10 {
            assert!(limiter.check("token-a"));
        }
        assert!(!limiter.check("token-a"));
        // token-b should still be allowed
        assert!(limiter.check("token-b"));
    }
}
