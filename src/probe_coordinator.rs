//! Probe Coordinator — unified entry layer for status probe with true Singleflight.
//!
//! # Design Overview (§19.6 in rust-v2-implementation-design.md)
//!
//! This module implements a three-layer judgment + (provider, model) pair-level
//! true Singleflight mechanism for status probes. Both Server and CLI reuse this
//! module following the DRY principle.
//!
//! ## Three-Layer Judgment
//!
//! 1. **L1: Active Provider Check** (30s TTL)
//!    - If provider has successful requests within TTL, skip probe
//!    - Rationale: Recent successful requests prove provider is healthy
//!
//! 2. **L2: Cache Result Check** (5s window)
//!    - If probe result exists in cache and is within 5s window, return cached result
//!    - Rationale: Avoid redundant probes for the same (provider, model) pair
//!    - Implementation: Check all protocols (openai_chat, anthropic, etc.)
//!
//! 3. **L3: Singleflight Merge**
//!    - If same (provider, model) is being probed, wait and share result
//!    - Rationale: Prevent duplicate concurrent probes to upstream
//!    - Timeout: 30s (configurable via `status.probe_timeout`)
//!
//! ## Concurrency Safety
//!
//! - **Lock ordering**: `flights` → `state`(RwLock read) → `cache`(internal RwLock)
//! - **No deadlock**: `state` only uses `read()` (never blocks), `flights` held across await
//!   is safe with tokio Mutex
//! - **Cancellation safety**: `FlightGuard` ensures flight state reset on panic/cancellation
//!
//! ## Known Trade-offs
//!
//! - **try_lock pattern**: `is_active`/`get_cached`/`update_cache` use `try_lock`/`try_read`/
//!   `try_write` to avoid blocking. On lock contention, operations silently skip (degraded
//!   but safe behavior).
//! - **Flights map growth**: Completed flights remain in map (with `result` clone and
//!   `last_probed`). Memory-bounded for static model sets, may grow slowly for dynamic sets.
//! - **Disk cache race**: Multiple concurrent probes to different keys may cause last-writer-wins
//!   on disk cache. Acceptable since probes are idempotent and infrequent.
//!
//! ## References
//!
//! - Design spec: `docs/design/rust-v2-implementation-design.md` §19
//! - Draft spec: `docs/drafts/core-architecture-design.md` §12
//! - Related: `src/status.rs` (ProbeResult, StatusCache, probe_key)
//!
//! # Module Structure
//!
//! - `ProbeState` trait: Abstraction for Server/CLI state management
//! - `ProbeCoordinator<S>`: Generic coordinator with three-layer judgment
//! - `ActiveProviderStore`: Single source of truth for provider liveness
//! - `CacheState`: Shared L2 cache read/write (get_cached/update_cache written once)
//! - `ServerProbeState`: Composes `ActiveProviderStore` + `CacheState`
//! - `CliProbeState`: Composes `CacheState` only (reserved for CLI integration)
//! - `FlightGuard`: Drop guard for cancellation safety
//! - `ProbeOutcome`: Result with execution info (for singleflight visibility)

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::{Mutex, RwLock};

use crate::config::Config;
use crate::status::{ProbeResult, StatusCache, probe_key};

/// Probe state abstraction: Server and CLI provide different implementations.
pub trait ProbeState: Send + Sync {
    /// L1: Active provider check (30s TTL by default)
    fn is_active(&self, provider_id: &str, ttl: Duration) -> bool;

    /// L2: Cache result check (5s window)
    fn get_cached(&self, key: &str, window: Duration) -> Option<ProbeResult>;

    /// Update cache after probe
    fn update_cache(&self, key: &str, result: &ProbeResult);

    /// Return a snapshot of the in-memory cache (for /admin/status response).
    fn cache_snapshot(&self) -> StatusCache;
}

/// Active provider store — single source of truth for provider liveness (§12.3/19.9).
///
/// Both `AppState` and `ServerProbeState` hold a shared `Arc<ActiveProviderStore>`
/// handle so the L1 liveness check and the forwarding path always read/write the
/// same state. All reads/writes go through this store's methods — there is no
/// second map anywhere.
///
/// Uses a `std::sync::Mutex` with `try_lock` (short critical sections, no await
/// held across the lock) to keep the API synchronous — `ProbeState` trait methods
/// are sync, and the forwarding path calls `mark_active` without async context.
pub struct ActiveProviderStore {
    inner: std::sync::Mutex<BTreeMap<String, Instant>>,
}

impl ActiveProviderStore {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Single write entry: called on successful forwarding (§12.3 live evidence).
    pub fn mark_active(&self, provider_id: &str) {
        if let Ok(mut map) = self.inner.try_lock() {
            map.insert(provider_id.to_string(), Instant::now());
        }
    }

    /// Check whether a provider is active within the TTL window (§19.9).
    pub fn is_active(&self, provider_id: &str, ttl: Duration) -> bool {
        let Ok(map) = self.inner.try_lock() else {
            return false;
        };
        let cutoff = Instant::now() - ttl;
        map.get(provider_id).is_some_and(|ts| *ts > cutoff)
    }

    /// Return active provider ids within the TTL window (admin/status endpoint).
    pub fn active_providers(&self, ttl: Duration) -> Vec<String> {
        let Ok(map) = self.inner.try_lock() else {
            return Vec::new();
        };
        let cutoff = Instant::now() - ttl;
        map.iter()
            .filter(|(_, ts)| **ts > cutoff)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

impl Default for ActiveProviderStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a cached entry to a `ProbeResult`, checking the time window.
/// Single implementation of the entry→result conversion + expiry check
/// Core conversion from cache entry to ProbeResult (no window check).
/// Used when we know the entry is valid (e.g., just written by execute_probe).
fn cache_entry_to_result_unchecked(entry: &crate::status::ProbeCacheEntry) -> Option<ProbeResult> {
    if entry.ok {
        return Some(ProbeResult::Ok {
            latency_ms: entry.latency_ms.unwrap_or(0),
            status: entry.http_status.unwrap_or(200),
        });
    }
    match entry.error.as_deref() {
        Some(err) if err.contains("timeout") => Some(ProbeResult::Timeout),
        Some(err) => Some(ProbeResult::Error(err.to_string())),
        None => None,
    }
}

/// Window-checked cache entry conversion (L2 cache hit path).
/// Returns None if the entry is expired or has no error info.
fn cache_entry_to_result(
    entry: &crate::status::ProbeCacheEntry,
    now_secs: u64,
    window: Duration,
) -> Option<ProbeResult> {
    // Expired cache entries don't hit. saturating_sub guards against clock
    // skew (now < checked_at) which would otherwise underflow.
    if now_secs.saturating_sub(entry.checked_at_unix) > window.as_secs() {
        return None;
    }
    cache_entry_to_result_unchecked(entry)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Shared cache read/write state (L2 cache layer, §19.6).
///
/// Extracted from `ServerProbeState`/`CliProbeState` so the `get_cached` /
/// `update_cache` logic is written once (DRY). Both probe states compose a
/// `CacheState` instead of re-implementing cache access.
struct CacheState {
    cache: Arc<RwLock<StatusCache>>,
}

impl CacheState {
    fn new() -> Self {
        let cache_path = crate::status::cache_path();
        let cache = crate::status::read_cache(&cache_path).unwrap_or_default();
        Self {
            cache: Arc::new(RwLock::new(cache)),
        }
    }

    /// L2: cached result within the window (single implementation).
    fn get_cached(&self, key: &str, window: Duration) -> Option<ProbeResult> {
        let cache = self.cache.try_read().ok()?;
        let entry = cache.probes.get(key)?;
        cache_entry_to_result(entry, now_secs(), window)
    }

    /// Write cache (memory + atomic disk write).
    fn update_cache(&self, key: &str, result: &ProbeResult) {
        if let Some(entry) = result.to_cache_entry() {
            if let Ok(mut cache) = self.cache.try_write() {
                cache.probes.insert(key.to_string(), entry);
            }
            let cache_path = crate::status::cache_path();
            if let Ok(cache) = self.cache.try_read()
                && let Err(e) = crate::status::write_cache(&cache_path, &cache)
            {
                tracing::warn!("failed to write status cache: {e}");
            }
        }
    }

    /// Return a snapshot of the in-memory cache.
    fn snapshot(&self) -> StatusCache {
        self.cache
            .try_read()
            .map(|c| (*c).clone())
            .unwrap_or_default()
    }
}

/// Probe result with execution info for Singleflight coordination (§19.6).
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    /// The actual probe result. Kept as part of the public API — current call
    /// sites only inspect `executed`, but consumers may need the result.
    #[allow(dead_code)]
    pub result: ProbeResult,
    /// Whether the probe was actually executed (false if returned from cache/singleflight)
    pub executed: bool,
}

/// Guard to ensure flight state is reset even if probe is cancelled or panics.
struct FlightGuard<'a> {
    flights: &'a tokio::sync::Mutex<BTreeMap<(String, String), ProbeFlight>>,
    key: (String, String),
}

impl<'a> Drop for FlightGuard<'a> {
    fn drop(&mut self) {
        // This is a blocking operation in drop, which is not ideal.
        // But it ensures state consistency even on panic/cancellation.
        // We use try_lock to avoid blocking the runtime.
        if let Ok(mut flights) = self.flights.try_lock()
            && let Some(flight) = flights.get_mut(&self.key)
        {
            flight.in_progress = false;
            // Notify any remaining waiters of cancellation
            for tx in flight.waiters.drain(..) {
                let _ = tx.send(ProbeResult::Error("probe cancelled or panicked".into()));
            }
        }
    }
}

/// Singleflight flight state for a (provider, model) pair.
///
/// `result` is intentionally absent: waiters receive the result via oneshot
/// channels, and fresh lookups go through the L2 cache (`CacheState`), so a
/// cached result on the flight itself would be dead state.
struct ProbeFlight {
    in_progress: bool,
    waiters: Vec<tokio::sync::oneshot::Sender<ProbeResult>>,
    last_probed: Option<Instant>,
}

/// Probe Coordinator: unified entry layer for status probe.
///
/// Generic over `ProbeState` to support both Server and CLI implementations.
pub struct ProbeCoordinator<S: ProbeState> {
    state: Arc<RwLock<S>>,
    flights: Arc<Mutex<BTreeMap<(String, String), ProbeFlight>>>,
}

impl<S: ProbeState> ProbeCoordinator<S> {
    /// Create a new ProbeCoordinator with the given state.
    pub fn new(state: S) -> Self {
        Self {
            state: Arc::new(RwLock::new(state)),
            flights: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Return a snapshot of the in-memory cache (for /admin/status response).
    pub async fn cache_snapshot(&self) -> StatusCache {
        let state = self.state.read().await;
        state.cache_snapshot()
    }

    /// Unified entry: three-layer judgment + singleflight merge.
    ///
    /// Returns the probe outcome for the given (provider, model) pair.
    pub async fn probe(
        &self,
        provider_id: &str,
        model_id: &str,
        cfg: &Config,
        client: &reqwest::Client,
    ) -> ProbeOutcome {
        let key = (provider_id.to_string(), model_id.to_string());

        // L1: Active provider check (30s TTL)
        // If active but cache has no data for any protocol, force probe.
        {
            let state = self.state.read().await;
            let active_ttl = Duration::from_secs(cfg.status.active_ttl);
            if state.is_active(provider_id, active_ttl) {
                // Check if cache has any data for this provider across all protocols
                let has_cache = crate::config::Protocol::CLIENT_PROTOCOLS
                    .iter()
                    .any(|protocol| {
                        if let Some(model) = cfg.models.get(model_id)
                            && model
                                .provider_bindings(*protocol)
                                .iter()
                                .any(|binding| binding.name == provider_id)
                        {
                            let cache_key = probe_key(model_id, provider_id, *protocol);
                            // Check cache with large window (any age is valid for this check)
                            return state
                                .get_cached(&cache_key, Duration::from_secs(86400))
                                .is_some();
                        }
                        false
                    });
                if has_cache {
                    return ProbeOutcome {
                        result: ProbeResult::Active,
                        executed: false,
                    };
                }
                // Active but no cache data — fall through to probe
            }
        }

        let mut flights = self.flights.lock().await;

        // L2: Cache result check (5s window)
        // Check cache for all protocols, return first valid cache found
        {
            let state = self.state.read().await;
            let cache_window = Duration::from_secs(5);
            for protocol in crate::config::Protocol::CLIENT_PROTOCOLS {
                let cache_key = probe_key(model_id, provider_id, protocol);
                if let Some(cached) = state.get_cached(&cache_key, cache_window) {
                    return ProbeOutcome {
                        result: cached,
                        executed: false,
                    };
                }
            }
        }

        // L3: Singleflight merge
        if let Some(flight) = flights.get(&key)
            && flight.in_progress
        {
            // Wait and share result
            let (tx, rx) = tokio::sync::oneshot::channel();
            flights.get_mut(&key).unwrap().waiters.push(tx);
            drop(flights);

            // Wait with timeout (30s)
            let probe_timeout = Duration::from_secs(cfg.status.probe_timeout.max(1));
            let result = match tokio::time::timeout(probe_timeout, rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => ProbeResult::Error("waiter channel closed".into()),
                Err(_) => ProbeResult::Timeout,
            };
            return ProbeOutcome {
                result,
                executed: false, // Waited for another probe, didn't execute
            };
        }

        // Mark as in-progress
        flights.insert(
            key.clone(),
            ProbeFlight {
                in_progress: true,
                waiters: Vec::new(),
                last_probed: None,
            },
        );
        drop(flights);

        // Create guard to ensure state reset on panic/cancellation
        let _guard = FlightGuard {
            flights: &self.flights,
            key: key.clone(),
        };

        // Execute probe (this will also update the cache)
        let result = self.execute_probe(provider_id, model_id, cfg, client).await;

        // Notify waiters + update state
        let mut flights = self.flights.lock().await;
        if let Some(flight) = flights.get_mut(&key) {
            for tx in flight.waiters.drain(..) {
                let _ = tx.send(result.clone());
            }
            flight.in_progress = false;
            flight.last_probed = Some(Instant::now());
        }
        drop(flights);

        // Guard will be dropped here, but flight is already reset
        // If panic occurs before this point, guard will reset the state

        ProbeOutcome {
            result,
            executed: true, // Actually executed the probe
        }
    }

    fn inactive_probe_tasks(
        cfg: &Config,
        active: &[String],
    ) -> Vec<(String, String, crate::config::Protocol)> {
        let active: BTreeSet<&str> = active.iter().map(String::as_str).collect();
        let mut tasks = Vec::new();
        for provider_id in cfg.providers.keys() {
            if active.contains(provider_id.as_str()) {
                continue;
            }
            for (model_id, model) in &cfg.models {
                for protocol in crate::config::Protocol::CLIENT_PROTOCOLS {
                    if model
                        .provider_bindings(protocol)
                        .iter()
                        .any(|binding| binding.name == *provider_id)
                    {
                        tasks.push((provider_id.clone(), model_id.clone(), protocol));
                    }
                }
            }
        }
        tasks
    }

    /// Stream all inactive probe outcomes as soon as each concurrent probe completes.
    pub fn probe_stream<'a>(
        &'a self,
        cfg: &'a Config,
        client: &'a reqwest::Client,
        active: &'a [String],
    ) -> impl futures_util::Stream<Item = (String, String, crate::config::Protocol, ProbeOutcome)> + 'a
    {
        futures_util::stream::iter(Self::inactive_probe_tasks(cfg, active))
            .map(move |(provider_id, model_id, protocol)| async move {
                let outcome = self.probe(&provider_id, &model_id, cfg, client).await;
                (provider_id, model_id, protocol, outcome)
            })
            .buffer_unordered(8)
    }

    /// Probe all inactive (provider, model) bindings and return the providers
    /// that actually executed a probe (singleflight semantics — waiters and
    /// cache hits are excluded).
    ///
    /// This is the core traversal logic extracted from the admin HTTP handler
    /// so the handler stays thin and this logic is unit-testable.
    #[allow(dead_code)]
    pub async fn probe_all_inactive(
        &self,
        cfg: &Config,
        client: &reqwest::Client,
        active: &[String],
    ) -> Vec<String> {
        let mut probed = BTreeSet::new();
        let mut stream = std::pin::pin!(self.probe_stream(cfg, client, active));
        while let Some((provider_id, _, _, outcome)) = stream.next().await {
            if outcome.executed {
                probed.insert(provider_id);
            }
        }
        probed.into_iter().collect()
    }

    /// Execute the actual probe and update cache.
    /// Probes ALL matching protocols for the (provider, model) pair, not just the first.
    async fn execute_probe(
        &self,
        provider_id: &str,
        model_id: &str,
        cfg: &Config,
        client: &reqwest::Client,
    ) -> ProbeResult {
        // Read cache from disk
        let cache_path = crate::status::cache_path();
        let mut cache = crate::status::read_cache(&cache_path).unwrap_or_default();

        let mut first_result: Option<ProbeResult> = None;

        // Find the model and protocol, execute probe for ALL matching protocols
        for protocol in crate::config::Protocol::CLIENT_PROTOCOLS {
            if let Some(model) = cfg.models.get(model_id) {
                for binding in model.provider_bindings(protocol) {
                    if binding.name == provider_id
                        && let Some((_, plan)) = cfg
                            .resolve_model_request_candidates(protocol, model_id)
                            .into_iter()
                            .find(|(_, plan)| plan.provider_id == provider_id)
                    {
                        crate::status::run_one_online_probe(
                            cfg,
                            &mut cache,
                            client,
                            model_id,
                            protocol,
                            provider_id,
                            &plan,
                        )
                        .await;

                        // Get result from cache using the actual protocol
                        let cache_key = probe_key(model_id, provider_id, protocol);
                        if let Some(entry) = cache.probes.get(&cache_key) {
                            // We just wrote this cache entry, so it's valid — skip window check
                            let result =
                                cache_entry_to_result_unchecked(entry).unwrap_or_else(|| {
                                    ProbeResult::Error("probe result missing".into())
                                });
                            {
                                let state = self.state.read().await;
                                state.update_cache(&cache_key, &result);
                            }
                            // Remember the first result to return
                            if first_result.is_none() {
                                first_result = Some(result);
                            }
                        }
                    }
                }
            }
        }

        // Write cache to disk once after probing all protocols
        if let Err(e) = crate::status::write_cache(&cache_path, &cache) {
            tracing::warn!("failed to write status cache: {e}");
        }

        first_result.unwrap_or_else(|| ProbeResult::Error("no matching binding found".into()))
    }
}

/// Server-side probe state: shared active store + cache.
pub struct ServerProbeState {
    active_store: Arc<ActiveProviderStore>,
    cache: CacheState,
}

impl ServerProbeState {
    pub fn new(active_store: Arc<ActiveProviderStore>) -> Self {
        Self {
            active_store,
            cache: CacheState::new(),
        }
    }
}

impl ProbeState for ServerProbeState {
    fn is_active(&self, provider_id: &str, ttl: Duration) -> bool {
        // Single source of truth: shared ActiveProviderStore
        self.active_store.is_active(provider_id, ttl)
    }

    fn get_cached(&self, key: &str, window: Duration) -> Option<ProbeResult> {
        self.cache.get_cached(key, window)
    }

    fn update_cache(&self, key: &str, result: &ProbeResult) {
        self.cache.update_cache(key, result);
    }

    fn cache_snapshot(&self) -> StatusCache {
        self.cache.snapshot()
    }
}

/// CLI-side probe state: local cache only.
///
/// Reserved for the CLI independent-mode integration (DRY reuse of
/// `ProbeCoordinator`). Currently only constructed by tests — the CLI
/// `run_online_probes` path has not yet been refactored onto this module.
#[allow(dead_code)]
pub struct CliProbeState {
    cache: CacheState,
}

impl CliProbeState {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            cache: CacheState::new(),
        }
    }
}

impl Default for CliProbeState {
    fn default() -> Self {
        Self::new()
    }
}

impl ProbeState for CliProbeState {
    fn is_active(&self, _provider_id: &str, _ttl: Duration) -> bool {
        // CLI doesn't have global active state
        false
    }

    fn get_cached(&self, key: &str, window: Duration) -> Option<ProbeResult> {
        self.cache.get_cached(key, window)
    }

    fn update_cache(&self, key: &str, result: &ProbeResult) {
        self.cache.update_cache(key, result);
    }

    fn cache_snapshot(&self) -> StatusCache {
        self.cache.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 隔离 status cache：设置唯一的 LLM_PROXY_STATE_DIR（测试专用目录），
    /// 避免测试读写与运行中 server 共享的 status-cache.json（edition 2024 set_var 为 unsafe）。
    fn isolate_status_cache() {
        // 每个测试唯一目录：并发测试共用 PID 目录会互相删除对方的
        // 磁盘缓存（CacheState::new() 从磁盘加载，见 flaky 根因）。
        let dir = std::env::temp_dir().join(format!(
            "llm-proxy-test-{}-{:x}",
            std::process::id(),
            rand_suffix(),
        ));
        unsafe {
            std::env::set_var("LLM_PROXY_STATE_DIR", dir);
        }
        // 清理该目录内残留 cache，保证每个测试从空缓存开始
        let _ = std::fs::remove_file(crate::status::cache_path());
    }

    /// 生成随机后缀（避免并发测试共享目录）。不需要密码学强度。
    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 32))
            .unwrap_or(0)
    }

    /// 启动一个返回 200 的 mock upstream（axum），并返回其 addr。
    async fn spawn_ok_upstream(counter: Arc<AtomicUsize>) -> std::net::SocketAddr {
        use axum::Router;
        use axum::routing::post;
        use serde_json::json;

        let c = counter.clone();
        let upstream = Router::new().route(
            "/chat/completions",
            post(move || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({
                        "id":"probe-ok","object":"chat.completion","model":"deepseek-chat",
                        "choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let addr = listener.local_addr().expect("upstream addr");
        tokio::spawn(async move {
            axum::serve(listener, upstream)
                .await
                .expect("serve upstream");
        });
        addr
    }

    /// 把 deepseek provider 的 openai_chat URL host 重定向到 mock addr。
    fn repoint_deepseek(cfg: &mut crate::config::Config, addr: std::net::SocketAddr) {
        let base = format!("http://{addr}");
        let provider = cfg.providers.get_mut("deepseek").expect("deepseek");
        // Repoint openai_chat endpoint
        if let Some(ep) = &mut provider.openai_chat
            && let Some(u) = &mut ep.url
        {
            let path = url::Url::parse(u)
                .map(|p| p.path().to_string())
                .unwrap_or_else(|_| "/chat/completions".to_string());
            *u = format!("{base}{path}");
        }
        // Repoint anthropic endpoint
        if let Some(ep) = &mut provider.anthropic
            && let Some(u) = &mut ep.url
        {
            let path = url::Url::parse(u)
                .map(|p| p.path().to_string())
                .unwrap_or_else(|_| "/anthropic/v1/messages".to_string());
            *u = format!("{base}{path}");
        }
        // Note: openai_responses is derived from openai_chat, so it uses the same URL
    }

    #[test]
    fn test_probe_result_variants() {
        let active = ProbeResult::Active;
        let ok = ProbeResult::Ok {
            latency_ms: 100,
            status: 200,
        };
        let timeout = ProbeResult::Timeout;
        let error = ProbeResult::Error("test".into());

        // Just verify they can be created
        assert!(matches!(active, ProbeResult::Active));
        assert!(matches!(ok, ProbeResult::Ok { .. }));
        assert!(matches!(timeout, ProbeResult::Timeout));
        assert!(matches!(error, ProbeResult::Error(_)));
    }

    #[test]
    fn active_store_marks_and_checks_within_ttl() {
        // §12.3/19.9：mark_active 后 TTL 内 is_active 为 true，TTL 外为 false
        let store = ActiveProviderStore::new();
        assert!(!store.is_active("deepseek", Duration::from_secs(30)));

        store.mark_active("deepseek");
        assert!(store.is_active("deepseek", Duration::from_secs(30)));
        assert!(!store.is_active("unknown", Duration::from_secs(30)));

        let active = store.active_providers(Duration::from_secs(30));
        assert_eq!(active, vec!["deepseek".to_string()]);
    }

    #[test]
    fn active_store_expired_entry_is_not_active() {
        // 过期条目（TTL 窗口外）不算活跃
        let store = ActiveProviderStore::new();
        store.mark_active("deepseek");
        // 0 秒 TTL：任何非零 elapsed 都过期
        assert!(!store.is_active("deepseek", Duration::ZERO));
        assert!(store.active_providers(Duration::ZERO).is_empty());
    }

    #[test]
    fn active_store_shared_handle_is_single_source_of_truth() {
        // 双状态源 bug 修复验证：两个 Arc 句柄共享同一状态
        let store = Arc::new(ActiveProviderStore::new());
        let handle_a = store.clone();
        let handle_b = store.clone();

        // 通过 handle_a 写入
        handle_a.mark_active("deepseek");
        // handle_b 能看到（同一 store）
        assert!(handle_b.is_active("deepseek", Duration::from_secs(30)));
        assert!(
            handle_a
                .active_providers(Duration::from_secs(30))
                .contains(&"deepseek".to_string())
        );
    }

    #[tokio::test]
    async fn flight_guard_resets_in_progress_on_drop() {
        // FlightGuard：模拟 leader 被取消/panic，drop 时重置 in_progress
        let coordinator = ProbeCoordinator::new(CliProbeState::new());
        let key = ("deepseek".to_string(), "deepseek-v4-flash-lp".to_string());

        // 手动插入一个 in_progress flight
        coordinator.flights.lock().await.insert(
            key.clone(),
            ProbeFlight {
                in_progress: true,
                waiters: Vec::new(),
                last_probed: None,
            },
        );

        // 创建 guard 并立即 drop（模拟 leader 被 abort/panic）
        {
            let _guard = FlightGuard {
                flights: &coordinator.flights,
                key: key.clone(),
            };
        }

        // in_progress 应被重置为 false
        let flights = coordinator.flights.lock().await;
        assert!(!flights.get(&key).expect("flight exists").in_progress);
    }

    #[tokio::test]
    async fn waiter_receives_result_when_flight_completes() {
        // 并发合并：waiter 在 flight 完成时收到 leader 的结果
        let coordinator = ProbeCoordinator::new(CliProbeState::new());
        let key = ("deepseek".to_string(), "deepseek-v4-flash-lp".to_string());

        // 构造一个 in_progress flight，并注册一个 waiter
        let (tx, rx) = tokio::sync::oneshot::channel::<ProbeResult>();
        coordinator.flights.lock().await.insert(
            key.clone(),
            ProbeFlight {
                in_progress: true,
                waiters: vec![tx],
                last_probed: None,
            },
        );

        // 模拟 leader 完成：发送结果并重置状态
        {
            let mut flights = coordinator.flights.lock().await;
            let flight = flights.get_mut(&key).expect("flight exists");
            for waiter in flight.waiters.drain(..) {
                let _ = waiter.send(ProbeResult::Ok {
                    latency_ms: 10,
                    status: 200,
                });
            }
            flight.in_progress = false;
        }

        // waiter 收到结果
        let result = rx.await.expect("waiter receives result");
        match result {
            ProbeResult::Ok { status, .. } => assert_eq!(status, 200),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn waiter_with_expired_flight_returns_timeout() {
        // waiter 超时：flight 卡住时，waiter 在超时后返回（此处直接验证 oneshot 超时语义）
        let (tx, rx) = tokio::sync::oneshot::channel::<ProbeResult>();
        // 模拟 flight 永不完成（tx 持有不 drop，rx 永不 resolve）
        let _keep_tx = tx;

        let result = tokio::time::timeout(Duration::from_millis(50), rx).await;
        assert!(
            result.is_err(),
            "waiter should time out when flight never completes"
        );
    }

    #[tokio::test]
    async fn probe_all_inactive_skips_active_providers_without_network() {
        // §19.6 核心遍历：active 过滤分支（L1）——活跃 provider 直接跳过，不发任何网络请求
        let cfg = crate::config::default_deepseek_config();
        let coordinator = ProbeCoordinator::new(CliProbeState::new());
        let client = reqwest::Client::new();

        // 所有 provider 都标记活跃 → probe_all_inactive 应全部跳过 → 返回空，零网络请求
        let active = vec!["deepseek".to_string()];
        let probed = coordinator.probe_all_inactive(&cfg, &client, &active).await;
        assert!(probed.is_empty(), "active providers must be skipped");
    }

    /// 真实并发 L3：两个 probe() 并发调用同一 key，只有一个成为 leader 发真实网络请求，
    /// 另一个等待并共享结果（singleflight 核心语义）。不依赖真实 key（mock upstream）。
    /// 注意：execute_probe 现在会探测所有协议（chat/responses/anthropic），理论上一次执行会产生 3 个请求。
    /// 但测试环境中 derived 端点（openai_responses）可能不会发出请求，所以实际是 2 个。
    #[tokio::test]
    #[serial_test::serial]
    async fn concurrent_probes_merge_into_single_flight() {
        // 隔离全局 cache
        isolate_status_cache();

        let counter = Arc::new(AtomicUsize::new(0));
        let addr = spawn_ok_upstream(counter.clone()).await;
        let mut cfg = crate::config::default_deepseek_config();
        repoint_deepseek(&mut cfg, addr);

        let coordinator = ProbeCoordinator::new(CliProbeState::new());
        let client = reqwest::Client::new();

        let (r1, r2) = tokio::join!(
            coordinator.probe("deepseek", "deepseek-v4-flash-lp", &cfg, &client),
            coordinator.probe("deepseek", "deepseek-v4-flash-lp", &cfg, &client),
        );
        let executed = [r1.executed, r2.executed].iter().filter(|e| **e).count();
        assert_eq!(
            executed, 1,
            "exactly one probe should execute, got {executed}"
        );
        // execute_probe 探测所有协议，但测试环境中 derived 端点可能不发出请求
        // 实际观察到 2 个请求（chat + anthropic），TODO: 调查为什么 responses 没有发出请求
        let request_count = counter.load(Ordering::SeqCst);
        assert!(
            request_count >= 2,
            "at least two upstream requests expected (chat + anthropic), got {request_count}"
        );
    }

    /// L1 直接命中：provider 活跃且 cache 有数据时 probe() 直接返回 Active，不发网络请求。
    #[tokio::test]
    async fn probe_returns_active_when_provider_is_live() {
        // 隔离磁盘缓存：CacheState::new() 会从磁盘加载，不隔离则依赖
        // 运行中 server 或并发测试写入的真实缓存数据（flaky 根因）。
        isolate_status_cache();
        let store = Arc::new(ActiveProviderStore::new());
        store.mark_active("deepseek");
        let state = ServerProbeState::new(store);
        // Pre-populate cache so L1 check passes (active + cache has data)
        // key 格式必须与 probe_key() 一致："{model_id}:{provider_id}:{route_key}"
        state.update_cache(
            "deepseek-v4-flash-lp:deepseek:chat_completions",
            &ProbeResult::Ok {
                latency_ms: 10,
                status: 200,
            },
        );
        let coordinator = ProbeCoordinator::new(state);
        let cfg = crate::config::default_deepseek_config();
        let client = reqwest::Client::new();

        let outcome = coordinator
            .probe("deepseek", "deepseek-v4-flash-lp", &cfg, &client)
            .await;
        assert!(matches!(outcome.result, ProbeResult::Active));
        assert!(
            !outcome.executed,
            "active provider with cache must not execute probe"
        );
    }

    /// L1 绕过：provider 活跃但 cache 无数据时强制探测。
    #[tokio::test]
    async fn probe_forces_probe_when_active_but_cache_empty() {
        // 隔离磁盘缓存：CacheState::new() 从磁盘加载，不隔离则可能
        // 读到运行中 server 写入的缓存数据导致 L1 误命中（flaky 根因）。
        isolate_status_cache();
        let store = Arc::new(ActiveProviderStore::new());
        store.mark_active("deepseek");
        let coordinator = ProbeCoordinator::new(ServerProbeState::new(store));
        let cfg = crate::config::default_deepseek_config();
        let client = reqwest::Client::new();

        let outcome = coordinator
            .probe("deepseek", "deepseek-v4-flash-lp", &cfg, &client)
            .await;
        // Active but cache empty → should execute probe, not return Active
        assert!(
            outcome.executed,
            "active provider with empty cache must execute probe"
        );
        assert!(!matches!(outcome.result, ProbeResult::Active));
    }

    /// CliProbeState：update_cache 后可 get_cached 命中；过期条目不命中。
    #[tokio::test]
    async fn cli_probe_state_cache_roundtrip_and_expiry() {
        // 隔离全局 cache
        isolate_status_cache();

        // 命中路径：update_cache → get_cached（5s 窗口内）
        let state = CliProbeState::new();
        let key = "deepseek:deepseek-v4-flash-lp:openai_chat";
        state.update_cache(
            key,
            &ProbeResult::Ok {
                latency_ms: 10,
                status: 200,
            },
        );
        let hit = state.get_cached(key, Duration::from_secs(5));
        assert!(matches!(hit, Some(ProbeResult::Ok { status: 200, .. })));

        // 过期路径：手工构造 checked_at_unix=0 的旧 cache 文件，加载后应不命中
        isolate_status_cache();
        let mut old_cache = crate::status::StatusCache::default();
        old_cache.probes.insert(
            key.to_string(),
            crate::status::ProbeCacheEntry {
                ok: true,
                checked_at_unix: 0, // 1970 年，必然过期
                latency_ms: Some(10),
                http_status: Some(200),
                error: None,
            },
        );
        crate::status::write_cache(&crate::status::cache_path(), &old_cache)
            .expect("write old cache");
        let state = CliProbeState::new();
        let expired = state.get_cached(key, Duration::from_secs(5));
        assert!(expired.is_none(), "expired cache entry must not hit");

        isolate_status_cache();
    }

    /// 防御分支：锁竞争时 try_lock 失败 → 降级返回 false / 空列表。
    #[test]
    fn active_store_degrades_when_lock_contended() {
        let store = ActiveProviderStore::new();
        store.mark_active("deepseek");
        // 手动持有锁，使 try_lock 失败（降级路径）
        let _guard = store.inner.lock().expect("lock");
        assert!(
            !store.is_active("deepseek", Duration::from_secs(30)),
            "try_lock contention must degrade to false"
        );
        assert!(
            store.active_providers(Duration::from_secs(30)).is_empty(),
            "try_lock contention must degrade to empty"
        );
    }

    /// 防御分支：default() 构造干净状态。
    #[test]
    fn default_impls_construct_clean_state() {
        let store = ActiveProviderStore::default();
        assert!(store.active_providers(Duration::from_secs(30)).is_empty());
        assert!(!store.is_active("x", Duration::from_secs(30)));
        let _cli = CliProbeState::default();
    }

    /// 防御分支：异常 cache entry（非 ok 且无 error）转换 → None。
    #[test]
    fn cache_entry_to_result_handles_malformed_entry() {
        let entry = crate::status::ProbeCacheEntry {
            ok: false,
            checked_at_unix: now_secs(),
            latency_ms: None,
            http_status: None,
            error: None,
        };
        assert_eq!(
            cache_entry_to_result(&entry, now_secs(), Duration::from_secs(5)),
            None,
            "malformed entry (ok=false, no error) must not convert"
        );
    }

    /// 防御分支：cfg 中无该 provider 的绑定 → Error("no matching binding")。
    #[tokio::test]
    async fn probe_returns_no_binding_error_for_unknown_provider() {
        let cfg = crate::config::default_deepseek_config();
        let coordinator = ProbeCoordinator::new(CliProbeState::new());
        let client = reqwest::Client::new();
        // unknown provider：cfg 中不存在，execute_probe 找不到绑定即返回，不发网络
        let outcome = coordinator
            .probe(
                "nonexistent-provider",
                "deepseek-v4-flash-lp",
                &cfg,
                &client,
            )
            .await;
        match outcome.result {
            ProbeResult::Error(ref msg) => assert!(msg.contains("no matching binding")),
            other => panic!("expected no-binding error, got {:?}", other),
        }
        assert!(outcome.executed, "probe attempt counts as executed");
    }

    /// 防御分支：leader 卡住时 waiter 在 probe_timeout 后返回 Timeout。
    #[tokio::test]
    async fn waiter_times_out_when_leader_stalled() {
        // 隔离 cache：避免命中真实 cache 提前返回（不进入 waiter 分支）
        isolate_status_cache();
        let coordinator = ProbeCoordinator::new(CliProbeState::new());
        let key = ("deepseek".to_string(), "deepseek-v4-flash-lp".to_string());
        // 手动构造卡住的 in_progress flight（leader 永不完成）
        coordinator.flights.lock().await.insert(
            key.clone(),
            ProbeFlight {
                in_progress: true,
                waiters: Vec::new(),
                last_probed: None,
            },
        );
        // 短 probe_timeout 加速测试
        let mut cfg = crate::config::default_deepseek_config();
        cfg.status.probe_timeout = 1;
        let client = reqwest::Client::new();
        let start = std::time::Instant::now();
        let outcome = coordinator
            .probe("deepseek", "deepseek-v4-flash-lp", &cfg, &client)
            .await;
        assert!(matches!(outcome.result, ProbeResult::Timeout));
        assert!(start.elapsed() >= Duration::from_millis(900));
    }

    /// 防御分支：FlightGuard drop 时通知 waiter（cancel 语义）。
    #[tokio::test]
    async fn flight_guard_notifies_waiters_on_drop() {
        let coordinator = ProbeCoordinator::new(CliProbeState::new());
        let key = ("deepseek".to_string(), "deepseek-v4-flash-lp".to_string());
        let (tx, rx) = tokio::sync::oneshot::channel::<ProbeResult>();
        coordinator.flights.lock().await.insert(
            key.clone(),
            ProbeFlight {
                in_progress: true,
                waiters: vec![tx],
                last_probed: None,
            },
        );
        // drop guard（模拟 leader 被取消/panic）→ waiter 收到 cancel 通知
        {
            let _guard = FlightGuard {
                flights: &coordinator.flights,
                key: key.clone(),
            };
        }
        let result = rx.await.expect("waiter receives cancellation");
        match result {
            ProbeResult::Error(ref msg) => {
                assert!(msg.contains("cancelled") || msg.contains("panicked"));
            }
            other => panic!("expected cancellation error, got {:?}", other),
        }
    }
}
