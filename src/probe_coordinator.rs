//! Probe Coordinator — unified entry layer for status probe with true Singleflight.
//!
//! # Design Overview (§19.6 in spec.md)
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
//! - Design spec: `spec.md` §19
//! - Spec: `spec.md` §12
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::config::{Config, Protocol};
use crate::status::ProbeSelection;
use crate::status::{ProbeResult, StatusCache, probe_key};

/// Execution disposition for status probe outcome (§4/§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeDisposition {
    /// Actually executed upstream network request
    Executed,
    /// Failed local preflight before any upstream request was sent.
    NotExecuted,
    /// Skipped due to active provider evidence (L1)
    Active,
    /// Skipped due to short-term cache hit (L2)
    Cached,
    /// Merged into an in-flight probe waiter (L3)
    Coalesced,
}

/// Probe state abstraction: Server and CLI provide different implementations.
pub trait ProbeState: Send + Sync {
    /// L1: Active provider check (30s TTL by default)
    fn is_active(&self, provider_id: &str, ttl: Duration) -> bool;

    /// L2: Cache result check (5s window)
    fn get_cached(&self, key: &str, window: Duration) -> Option<ProbeResult>;

    /// Update cache after probe
    fn update_cache(&self, key: &str, result: &ProbeResult);

    /// Return the OAuth account store used by this probe state.
    fn oauth_accounts_path(&self) -> PathBuf;

    /// Return the injected cache path (spec §11.8 parameter homogeneity).
    fn cache_path(&self) -> PathBuf;

    /// Return a snapshot of the in-memory cache (for /admin/status response).
    fn cache_snapshot(&self) -> StatusCache;

    /// Merge successful dynamic model discovery results into the shared cache.
    async fn update_dynamic_models(
        &self,
        dynamic_models: BTreeMap<String, Vec<crate::status::DynamicModelCacheEntry>>,
    );

    /// : Return the cooldown store for probe send governance.
    /// Server path returns the shared store; CLI path returns None (no
    /// in-memory store — the CLI loads from disk directly).
    fn cooldown_store(&self) -> Option<Arc<crate::cooldown::CooldownStore>> {
        None
    }
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
    path: PathBuf,
}

impl CacheState {
    fn with_path(path: PathBuf) -> Self {
        let cache = crate::status::read_cache(&path).unwrap_or_default();
        Self {
            cache: Arc::new(RwLock::new(cache)),
            path,
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
            if let Ok(cache) = self.cache.try_read()
                && let Err(e) = crate::status::write_cache(&self.path, &cache)
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

    /// Return the injected cache path.
    fn path(&self) -> &Path {
        &self.path
    }

    /// Merge dynamic discovery rows and persist the shared cache.
    async fn update_dynamic_models(
        &self,
        dynamic_models: BTreeMap<String, Vec<crate::status::DynamicModelCacheEntry>>,
    ) {
        if dynamic_models.is_empty() {
            return;
        }
        {
            let mut cache = self.cache.write().await;
            cache.dynamic_models.extend(dynamic_models);
        }
        let cache = self.cache.read().await;
        if let Err(e) = crate::status::write_cache(&self.path, &cache) {
            tracing::warn!("failed to write dynamic model status cache: {e}");
        }
    }
}

/// Probe result with execution info for Singleflight coordination (§19.6, §5).
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    /// Per-protocol probe results for this (provider, model) pair.
    pub results: HashMap<Protocol, ProbeResult>,
    /// Per-protocol execution dispositions for this (provider, model) pair.
    pub dispositions: HashMap<Protocol, ProbeDisposition>,
    /// Aggregate disposition retained for existing pair-level callers.
    #[allow(dead_code)]
    pub disposition: ProbeDisposition,
    /// First / primary probe result for backward compatibility in callers/tests.
    #[allow(dead_code)]
    pub result: ProbeResult,
    /// Whether any protocol in the pair actually executed an upstream request.
    #[allow(dead_code)]
    pub executed: bool,
}

impl ProbeOutcome {
    pub fn new(results: HashMap<Protocol, ProbeResult>, disposition: ProbeDisposition) -> Self {
        let dispositions = results
            .keys()
            .map(|protocol| (*protocol, disposition))
            .collect();
        Self::with_dispositions(results, dispositions, disposition)
    }

    pub fn with_dispositions(
        results: HashMap<Protocol, ProbeResult>,
        dispositions: HashMap<Protocol, ProbeDisposition>,
        disposition: ProbeDisposition,
    ) -> Self {
        let first_result = Protocol::CLIENT_PROTOCOLS
            .iter()
            .find_map(|p| results.get(p))
            .cloned()
            .or_else(|| results.values().next().cloned())
            .unwrap_or_else(|| ProbeResult::Error("no matching binding found".into()));
        let executed = dispositions
            .values()
            .any(|value| *value == ProbeDisposition::Executed);
        Self {
            results,
            dispositions,
            disposition,
            result: first_result,
            executed,
        }
    }
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
            let mut err_map = HashMap::new();
            for protocol in Protocol::CLIENT_PROTOCOLS {
                err_map.insert(
                    protocol,
                    ProbeResult::Error("probe cancelled or panicked".into()),
                );
            }
            // Notify any remaining waiters of cancellation
            for tx in flight.waiters.drain(..) {
                let _ = tx.send(err_map.clone());
            }
        }
    }
}

/// Singleflight flight state for a (provider, model) pair.
///
/// Waiters receive per-protocol results via oneshot channels.
struct ProbeFlight {
    in_progress: bool,
    waiters: Vec<tokio::sync::oneshot::Sender<HashMap<Protocol, ProbeResult>>>,
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

    /// Discover dynamic models for the selected Provider set and merge successful rows into
    /// this coordinator's shared cache.
    pub async fn discover_dynamic_models(&self, cfg: &Config, selection: &ProbeSelection) {
        let oauth_accounts_path = self.state.read().await.oauth_accounts_path();
        let mut cache = StatusCache::default();
        crate::status::discover_dynamic_models(cfg, &mut cache, selection, &oauth_accounts_path)
            .await;
        self.state
            .read()
            .await
            .update_dynamic_models(cache.dynamic_models)
            .await;
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

        let matching_protocols: Vec<Protocol> = Protocol::CLIENT_PROTOCOLS
            .into_iter()
            .filter(|&proto| {
                cfg.models.get(model_id).is_some_and(|m| {
                    m.provider_bindings(proto)
                        .iter()
                        .any(|b| b.name == provider_id)
                })
            })
            .collect();

        // L1: Active provider check (30s TTL)
        // If active but cache has no data for any protocol, force probe.
        {
            let state = self.state.read().await;
            let active_ttl = Duration::from_secs(cfg.status.active_ttl);
            if state.is_active(provider_id, active_ttl) {
                // Check if cache has any data for this provider across all protocols
                let has_cache = Protocol::CLIENT_PROTOCOLS.iter().any(|protocol| {
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
                    let mut results = HashMap::new();
                    for &proto in &matching_protocols {
                        results.insert(proto, ProbeResult::Active);
                    }
                    return ProbeOutcome::new(results, ProbeDisposition::Active);
                }
                // Active but no cache data — fall through to probe
            }
        }

        let mut flights = self.flights.lock().await;

        // L2: Cache result check (5s window)
        // If all matching protocols have valid cache within window, return cached
        if !matching_protocols.is_empty() {
            let state = self.state.read().await;
            let cache_window = Duration::from_secs(5);
            let mut cached_map = HashMap::new();
            for &proto in &matching_protocols {
                let cache_key = probe_key(model_id, provider_id, proto);
                if let Some(cached) = state.get_cached(&cache_key, cache_window) {
                    cached_map.insert(proto, cached);
                }
            }
            if cached_map.len() == matching_protocols.len() {
                return ProbeOutcome::new(cached_map, ProbeDisposition::Cached);
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
            let results = match tokio::time::timeout(probe_timeout, rx).await {
                Ok(Ok(results)) => results,
                Ok(Err(_)) => {
                    let mut map = HashMap::new();
                    for &proto in &matching_protocols {
                        map.insert(proto, ProbeResult::Error("waiter channel closed".into()));
                    }
                    map
                }
                Err(_) => {
                    let mut map = HashMap::new();
                    for &proto in &matching_protocols {
                        map.insert(proto, ProbeResult::Timeout);
                    }
                    map
                }
            };
            return ProbeOutcome::new(results, ProbeDisposition::Coalesced);
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
        let outcome = self.execute_probe(provider_id, model_id, cfg, client).await;

        // Notify waiters + update state
        let mut flights = self.flights.lock().await;
        if let Some(flight) = flights.get_mut(&key) {
            for tx in flight.waiters.drain(..) {
                let _ = tx.send(outcome.results.clone());
            }
            flight.in_progress = false;
            flight.last_probed = Some(Instant::now());
        }
        drop(flights);

        // Guard will be dropped here, but flight is already reset
        // If panic occurs before this point, guard will reset the state

        outcome
    }

    /// Stream all selected binding outcomes as soon as each pair probe completes.
    ///
    /// Active bindings are emitted without an upstream request. The remaining bindings
    /// are grouped by `(provider, model)`, so this batch invokes the pair-level
    /// singleflight exactly once before mapping its protocol-specific results back to
    /// every selected binding.
    pub fn probe_stream<'a>(
        &'a self,
        cfg: &'a Config,
        client: &'a reqwest::Client,
        active: &'a [String],
        selection: &'a ProbeSelection,
    ) -> impl futures_util::Stream<Item = (String, String, Protocol, ProbeDisposition, ProbeResult)> + 'a
    {
        let active: BTreeSet<&str> = active.iter().map(String::as_str).collect();
        let mut inactive_pairs: BTreeMap<(String, String), Vec<Protocol>> = BTreeMap::new();
        let mut active_outcomes = Vec::new();

        for (provider_id, model_id, protocol) in selection.candidates(cfg) {
            if active.contains(provider_id.as_str()) {
                active_outcomes.push((
                    provider_id,
                    model_id,
                    protocol,
                    ProbeDisposition::Active,
                    ProbeResult::Active,
                ));
            } else {
                inactive_pairs
                    .entry((provider_id, model_id))
                    .or_default()
                    .push(protocol);
            }
        }

        let concurrency = cfg.status.probe_concurrency.max(1);
        let interval_ms = cfg.status.probe_interval_ms;
        let jitter_ms = cfg.status.probe_interval_jitter_ms;

        let inactive = futures_util::stream::iter(inactive_pairs).map(
            move |((provider_id, model_id), protocols)| async move {
                if interval_ms > 0 || jitter_ms > 0 {
                    let jitter = if jitter_ms > 0 {
                        use std::time::SystemTime;
                        let nanos = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .map(|d| d.subsec_nanos() as u64)
                            .unwrap_or(0);
                        nanos % (jitter_ms + 1)
                    } else {
                        0
                    };
                    tokio::time::sleep(Duration::from_millis(interval_ms + jitter)).await;
                }
                let outcome = self.probe(&provider_id, &model_id, cfg, client).await;
                protocols
                    .into_iter()
                    .map(|protocol| {
                        let disposition = outcome
                            .dispositions
                            .get(&protocol)
                            .copied()
                            .unwrap_or(ProbeDisposition::NotExecuted);
                        let result = outcome.results.get(&protocol).cloned().unwrap_or_else(|| {
                            ProbeResult::Error(
                                "probe result missing for selected protocol".to_string(),
                            )
                        });
                        (
                            provider_id.clone(),
                            model_id.clone(),
                            protocol,
                            disposition,
                            result,
                        )
                    })
                    .collect::<Vec<_>>()
            },
        );

        futures_util::stream::iter(active_outcomes).chain(
            inactive
                .buffer_unordered(concurrency)
                .flat_map(futures_util::stream::iter),
        )
    }

    /// Probe all selected bindings and return per-binding outcomes
    /// (provider, model, protocol, disposition, result).
    ///
    /// Only providers with disposition == Executed actually executed a probe.
    #[allow(dead_code)]
    pub async fn probe_all_inactive(
        &self,
        cfg: &Config,
        client: &reqwest::Client,
        active: &[String],
        selection: &ProbeSelection,
    ) -> Vec<(String, String, Protocol, ProbeDisposition, ProbeResult)> {
        let mut stream = std::pin::pin!(self.probe_stream(cfg, client, active, selection));
        let mut outcomes = Vec::new();
        while let Some(item) = stream.next().await {
            outcomes.push(item);
        }
        outcomes
    }

    /// Execute the actual probe and update cache.
    /// Probes ALL matching protocols for the (provider, model) pair, not just the first.
    async fn execute_probe(
        &self,
        provider_id: &str,
        model_id: &str,
        cfg: &Config,
        client: &reqwest::Client,
    ) -> ProbeOutcome {
        // Read cache from disk — use injected path, not global (spec §11.8)
        let cache_path = {
            let state = self.state.read().await;
            state.cache_path()
        };
        let mut cache = crate::status::read_cache(&cache_path).unwrap_or_default();
        let mut results = HashMap::new();
        let mut dispositions = HashMap::new();

        // Find the model and protocol, execute probe for ALL matching protocols
        for protocol in Protocol::CLIENT_PROTOCOLS {
            if let Some(model) = cfg.models.get(model_id) {
                for binding in model.provider_bindings(protocol) {
                    if binding.name != provider_id {
                        continue;
                    }

                    let Some((_, plan)) = cfg
                        .resolve_model_request_candidates(protocol, model_id)
                        .into_iter()
                        .find(|(_, plan)| plan.provider_id == provider_id)
                    else {
                        results.insert(
                            protocol,
                            ProbeResult::Error(
                                "probe request plan unavailable for selected binding".into(),
                            ),
                        );
                        dispositions.insert(protocol, ProbeDisposition::NotExecuted);
                        continue;
                    };

                    let oauth_accounts_path = {
                        let state = self.state.read().await;
                        state.oauth_accounts_path()
                    };
                    // : read cooldown store from state for probe governance.
                    let cooldown_store = {
                        let state = self.state.read().await;
                        state.cooldown_store()
                    };
                    let cooldown_cfg = cfg.effective_cooldown(model_id, provider_id);
                    let sent = crate::status::run_one_online_probe(
                        &oauth_accounts_path,
                        cfg,
                        &mut cache,
                        client,
                        model_id,
                        protocol,
                        provider_id,
                        &plan,
                        cooldown_store.as_ref().map(|s| s.as_ref()),
                        &cooldown_cfg,
                    )
                    .await;
                    let disposition = if sent {
                        ProbeDisposition::Executed
                    } else {
                        ProbeDisposition::NotExecuted
                    };

                    // Get result from cache using the actual protocol
                    let cache_key = probe_key(model_id, provider_id, protocol);
                    if let Some(entry) = cache.probes.get(&cache_key) {
                        // We just wrote this cache entry, so it's valid — skip window check
                        let result = cache_entry_to_result_unchecked(entry)
                            .unwrap_or_else(|| ProbeResult::Error("probe result missing".into()));
                        {
                            let state = self.state.read().await;
                            state.update_cache(&cache_key, &result);
                        }
                        results.insert(protocol, result);
                        dispositions.insert(protocol, disposition);
                    } else {
                        results.insert(
                            protocol,
                            ProbeResult::Error("probe result missing in cache".into()),
                        );
                        dispositions.insert(protocol, ProbeDisposition::NotExecuted);
                    }
                }
            }
        }

        // Write cache to disk once after probing all protocols
        if let Err(e) = crate::status::write_cache(&cache_path, &cache) {
            tracing::warn!("failed to write status cache: {e}");
        }

        if results.is_empty() {
            results.insert(
                Protocol::OpenaiChatCompletions,
                ProbeResult::Error("no matching binding found".into()),
            );
            dispositions.insert(
                Protocol::OpenaiChatCompletions,
                ProbeDisposition::NotExecuted,
            );
        }

        let aggregate_disposition = if dispositions
            .values()
            .any(|value| *value == ProbeDisposition::Executed)
        {
            ProbeDisposition::Executed
        } else {
            ProbeDisposition::NotExecuted
        };
        ProbeOutcome::with_dispositions(results, dispositions, aggregate_disposition)
    }
}

/// Server-side probe state: shared active store + cache.
pub struct ServerProbeState {
    active_store: Arc<ActiveProviderStore>,
    cache: CacheState,
    oauth_accounts_path: PathBuf,
    /// : shared cooldown store for probe send governance.
    cooldown_store: Option<Arc<crate::cooldown::CooldownStore>>,
}

impl ServerProbeState {
    pub fn with_cache_path_and_oauth_path(
        active_store: Arc<ActiveProviderStore>,
        cache_path: PathBuf,
        oauth_accounts_path: PathBuf,
    ) -> Self {
        Self {
            active_store,
            cache: CacheState::with_path(cache_path),
            oauth_accounts_path,
            cooldown_store: None,
        }
    }

    /// : attach the shared cooldown store so probes can check/write
    /// cooldowns (same store the forwarding path uses).
    pub fn with_cooldown_store(mut self, store: Arc<crate::cooldown::CooldownStore>) -> Self {
        self.cooldown_store = Some(store);
        self
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

    fn oauth_accounts_path(&self) -> PathBuf {
        self.oauth_accounts_path.clone()
    }

    fn cache_path(&self) -> PathBuf {
        self.cache.path().to_path_buf()
    }

    fn cache_snapshot(&self) -> StatusCache {
        self.cache.snapshot()
    }

    async fn update_dynamic_models(
        &self,
        dynamic_models: BTreeMap<String, Vec<crate::status::DynamicModelCacheEntry>>,
    ) {
        self.cache.update_dynamic_models(dynamic_models).await;
    }

    fn cooldown_store(&self) -> Option<Arc<crate::cooldown::CooldownStore>> {
        self.cooldown_store.clone()
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
        Self::with_cache_path(crate::status::cache_path())
    }

    pub fn with_cache_path(cache_path: PathBuf) -> Self {
        Self {
            cache: CacheState::with_path(cache_path),
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

    fn oauth_accounts_path(&self) -> PathBuf {
        crate::auth::default_state_path()
    }

    fn cache_path(&self) -> PathBuf {
        self.cache.path().to_path_buf()
    }

    fn cache_snapshot(&self) -> StatusCache {
        self.cache.snapshot()
    }

    async fn update_dynamic_models(
        &self,
        dynamic_models: BTreeMap<String, Vec<crate::status::DynamicModelCacheEntry>>,
    ) {
        self.cache.update_dynamic_models(dynamic_models).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_cache_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("cache tempdir");
        let path = dir.path().join("status-cache.json");
        (dir, path)
    }

    fn cli_probe_state() -> (tempfile::TempDir, CliProbeState) {
        let (dir, path) = temp_cache_path();
        (dir, CliProbeState::with_cache_path(path))
    }

    /// 启动一个返回 200 的 mock upstream（axum），并返回其 addr。
    async fn spawn_ok_upstream(counter: Arc<AtomicUsize>) -> std::net::SocketAddr {
        use axum::Router;
        use axum::routing::post;
        use serde_json::json;

        let c1 = counter.clone();
        let c2 = counter.clone();
        let c3 = counter.clone();
        let upstream = Router::new()
            .route(
                "/chat/completions",
                post(move || {
                    let c = c1.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({
                            "id":"probe-ok","object":"chat.completion","model":"deepseek-chat",
                            "choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],
                            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                        }))
                    }
                }),
            )
            .route(
                "/v1/responses",
                post(move || {
                    let c = c2.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({
                            "id":"probe-ok","object":"responses/response","status":"completed",
                            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"pong"}]}]
                        }))
                    }
                }),
            )
            .route(
                "/anthropic/v1/messages",
                post(move || {
                    let c = c3.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({
                            "id":"probe-ok","type":"message","role":"assistant",
                            "content":[{"type":"text","text":"pong"}],
                            "model":"deepseek-anthropic","stop_reason":"end_turn"
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
        // Repoint openai_responses endpoint
        if let Some(ep) = &mut provider.openai_responses
            && let Some(u) = &mut ep.url
        {
            let path = url::Url::parse(u)
                .map(|p| p.path().to_string())
                .unwrap_or_else(|_| "/v1/responses".to_string());
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
        let (tx, rx) = tokio::sync::oneshot::channel::<HashMap<Protocol, ProbeResult>>();
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
            let mut res_map = HashMap::new();
            res_map.insert(
                Protocol::OpenaiChatCompletions,
                ProbeResult::Ok {
                    latency_ms: 10,
                    status: 200,
                },
            );
            for waiter in flight.waiters.drain(..) {
                let _ = waiter.send(res_map.clone());
            }
            flight.in_progress = false;
        }

        // waiter 收到结果
        let results = rx.await.expect("waiter receives result");
        match results.get(&Protocol::OpenaiChatCompletions) {
            Some(ProbeResult::Ok { status, .. }) => assert_eq!(*status, 200),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn waiter_with_expired_flight_returns_timeout() {
        // waiter 超时：flight 卡住时，waiter 在超时后返回（此处直接验证 oneshot 超时语义）
        let (tx, rx) = tokio::sync::oneshot::channel::<HashMap<Protocol, ProbeResult>>();
        // 模拟 flight 永不完成（tx 持有不 drop，rx 永不 resolve）
        let _keep_tx = tx;

        let result = tokio::time::timeout(Duration::from_millis(50), rx).await;
        assert!(
            result.is_err(),
            "waiter should time out when flight never completes"
        );
    }

    #[tokio::test]
    async fn probe_all_inactive_reports_active_bindings_without_network() {
        // Active providers must still produce one outcome per selected binding.
        let cfg = crate::config::default_deepseek_config();
        let coordinator = ProbeCoordinator::new(CliProbeState::new());
        let client = reqwest::Client::new();
        let selection = ProbeSelection {
            provider: Some("deepseek".to_string()),
            model: Some("deepseek-v4-flash-lp".to_string()),
        };

        let outcomes = coordinator
            .probe_all_inactive(&cfg, &client, &["deepseek".to_string()], &selection)
            .await;

        let candidates = selection.candidates(&cfg);
        assert_eq!(outcomes.len(), candidates.len());
        assert!(outcomes.iter().all(|(_, _, _, disposition, result)| {
            *disposition == ProbeDisposition::Active && matches!(result, ProbeResult::Active)
        }));
    }

    /// 真实并发 L3：两个 probe() 并发调用同一 key，只有一个成为 leader 发真实网络请求，
    /// 另一个等待并共享结果（singleflight 核心语义）。不依赖真实 key（mock upstream）。
    /// 注意：execute_probe 现在会探测所有协议（chat/responses/anthropic），一次执行会产生 3 个请求。
    #[tokio::test]
    async fn concurrent_probes_merge_into_single_flight() {
        let (_cache_dir, state) = cli_probe_state();
        let counter = Arc::new(AtomicUsize::new(0));
        let addr = spawn_ok_upstream(counter.clone()).await;
        let mut cfg = crate::config::default_deepseek_config();
        repoint_deepseek(&mut cfg, addr);

        let coordinator = ProbeCoordinator::new(state);
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
        // execute_probe probes all protocols: chat + responses + anthropic = 3 requests
        let request_count = counter.load(Ordering::SeqCst);
        assert!(
            request_count >= 3,
            "at least three upstream requests expected (chat + responses + anthropic), got {request_count}"
        );
    }

    /// L1 直接命中：provider 活跃且 cache 有数据时 probe() 直接返回 Active，不发网络请求。
    #[tokio::test]
    async fn probe_returns_active_when_provider_is_live() {
        let (_cache_dir, cache_path) = temp_cache_path();
        let store = Arc::new(ActiveProviderStore::new());
        store.mark_active("deepseek");
        let state = ServerProbeState::with_cache_path_and_oauth_path(
            store,
            cache_path,
            crate::auth::default_state_path(),
        );
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
        let (_cache_dir, cache_path) = temp_cache_path();
        let store = Arc::new(ActiveProviderStore::new());
        store.mark_active("deepseek");
        let coordinator = ProbeCoordinator::new(ServerProbeState::with_cache_path_and_oauth_path(
            store,
            cache_path,
            crate::auth::default_state_path(),
        ));
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
        let (_cache_dir, cache_path) = temp_cache_path();
        let key = "deepseek:deepseek-v4-flash-lp:openai_chat";

        let state = CliProbeState::with_cache_path(cache_path.clone());
        state.update_cache(
            key,
            &ProbeResult::Ok {
                latency_ms: 10,
                status: 200,
            },
        );
        let hit = state.get_cached(key, Duration::from_secs(5));
        assert!(matches!(hit, Some(ProbeResult::Ok { status: 200, .. })));

        let mut old_cache = crate::status::StatusCache::default();
        old_cache.probes.insert(
            key.to_string(),
            crate::status::ProbeCacheEntry {
                ok: true,
                checked_at_unix: 0,
                latency_ms: Some(10),
                http_status: Some(200),
                error: None,
            },
        );
        crate::status::write_cache(&cache_path, &old_cache).expect("write old cache");
        let state = CliProbeState::with_cache_path(cache_path);
        let expired = state.get_cached(key, Duration::from_secs(5));
        assert!(expired.is_none(), "expired cache entry must not hit");
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
        assert_eq!(outcome.disposition, ProbeDisposition::NotExecuted);
        assert!(
            !outcome.executed,
            "a missing binding must not be reported as an upstream execution"
        );
    }

    /// 防御分支：leader 卡住时 waiter 在 probe_timeout 后返回 Timeout。
    #[tokio::test]
    async fn waiter_times_out_when_leader_stalled() {
        let (_cache_dir, state) = cli_probe_state();
        let coordinator = ProbeCoordinator::new(state);
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
        let (tx, rx) = tokio::sync::oneshot::channel::<HashMap<Protocol, ProbeResult>>();
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
        let results = rx.await.expect("waiter receives cancellation");
        let result = results
            .get(&Protocol::OpenaiChatCompletions)
            .expect("has result");
        match result {
            ProbeResult::Error(msg) => {
                assert!(msg.contains("cancelled") || msg.contains("panicked"));
            }
            other => panic!("expected cancellation error, got {:?}", other),
        }
    }

    /// §5: 同 pair 两个 protocol 时 per-protocol 结果一一对应。
    #[tokio::test]
    async fn same_pair_different_protocols_produce_distinct_results_in_stream() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use serde_json::json;

        // Upstream returns 200 for /chat/completions, and 500 for /anthropic/v1/messages
        let app = Router::new()
            .route(
                "/chat/completions",
                post(|| async {
                    axum::Json(json!({
                        "id": "probe-ok",
                        "object": "chat.completion",
                        "model": "deepseek-chat",
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    }))
                }),
            )
            .route(
                "/anthropic/v1/messages",
                post(|| async {
                    (StatusCode::INTERNAL_SERVER_ERROR, "upstream anthropic failed")
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let mut cfg = crate::config::default_deepseek_config();
        repoint_deepseek(&mut cfg, addr);

        let (_cache_dir, state) = cli_probe_state();
        let coordinator = ProbeCoordinator::new(state);
        let client = reqwest::Client::new();

        // Target only deepseek-v4-flash-lp
        let sel = ProbeSelection {
            provider: Some("deepseek".to_string()),
            model: Some("deepseek-v4-flash-lp".to_string()),
        };

        let outcomes = coordinator
            .probe_all_inactive(&cfg, &client, &[], &sel)
            .await;

        let chat_outcome = outcomes
            .iter()
            .find(|(_, _, proto, _, _)| *proto == Protocol::OpenaiChatCompletions)
            .expect("chat outcome exists");
        let anthropic_outcome = outcomes
            .iter()
            .find(|(_, _, proto, _, _)| *proto == Protocol::Anthropic)
            .expect("anthropic outcome exists");

        // The two protocols must have distinct, authentic per-protocol results
        match &chat_outcome.4 {
            ProbeResult::Ok { status, .. } => assert_eq!(*status, 200),
            other => panic!("expected chat Ok, got {:?}", other),
        }
        match &anthropic_outcome.4 {
            ProbeResult::Error(msg) => {
                assert!(
                    msg.contains("500") || msg.contains("failed") || msg.contains("error"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected anthropic Error, got {:?}", other),
        }
        assert_eq!(
            chat_outcome.3,
            ProbeDisposition::Executed,
            "the batch must execute this pair once rather than self-coalescing"
        );
        assert_eq!(
            anthropic_outcome.3,
            ProbeDisposition::Executed,
            "all selected bindings of the same executed pair share its disposition"
        );

        // Also verify that direct probe() call returns per-protocol results map
        let direct_outcome = coordinator
            .probe("deepseek", "deepseek-v4-flash-lp", &cfg, &client)
            .await;
        assert!(matches!(
            direct_outcome.results.get(&Protocol::OpenaiChatCompletions),
            Some(ProbeResult::Ok { .. })
        ));
        assert!(matches!(
            direct_outcome.results.get(&Protocol::Anthropic),
            Some(ProbeResult::Error(_))
        ));
    }

    /// §5: selection 过滤后未选中候选零请求（mock upstream 请求计数）。
    #[tokio::test]
    async fn selection_filters_unselected_candidates_zero_upstream_requests() {
        let counter_deepseek = Arc::new(AtomicUsize::new(0));
        let addr = spawn_ok_upstream(counter_deepseek.clone()).await;

        let mut cfg = crate::config::default_deepseek_config();
        repoint_deepseek(&mut cfg, addr);

        // Add a second provider "other" with its own endpoint and counter
        let counter_other = Arc::new(AtomicUsize::new(0));
        let c_other = counter_other.clone();
        use axum::Router;
        use axum::routing::post;
        let other_app = Router::new().route(
            "/chat/completions",
            post(move || {
                let c = c_other.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "id": "probe-other",
                        "object": "chat.completion",
                        "model": "other-chat",
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"}, "finish_reason": "stop"}]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let other_addr = listener.local_addr().expect("other addr");
        tokio::spawn(async move {
            axum::serve(listener, other_app).await.expect("serve");
        });

        let mut other_provider = crate::catalog::deepseek().provider;
        if let Some(ep) = &mut other_provider.openai_chat {
            ep.url = Some(format!("http://{other_addr}/chat/completions"));
        }
        cfg.providers.insert("other".to_string(), other_provider);

        // Add a model for "other"
        let binding = crate::config::ProviderBinding {
            name: "other".to_string(),
            model: "other-model".to_string(),
        };
        cfg.models.insert(
            "model-other".to_string(),
            crate::config::ModelConfig {
                description: None,
                context_window: 1000,
                max_output_tokens: 1000,
                features: vec![],
                reasoning_levels: None,
                supported_reasoning_levels: vec![],
                cooldown: None,
                reasoning_routes: None,
                default_reasoning_level: None,
                supports_thinking: None,
                force_think: None,
                force_no_think: None,
                default_think: None,
                enable_thinking: None,
                chat_reasoning_off_format: None,
                codex_service_tier: None,
                openai_chat_providers: vec![binding],
                openai_responses_providers: vec![],
                anthropic_providers: vec![],
                reasoning_level_map: None,
            },
        );

        let (_cache_dir, state) = cli_probe_state();
        let coordinator = ProbeCoordinator::new(state);
        let client = reqwest::Client::new();

        // Select ONLY deepseek
        let sel = ProbeSelection {
            provider: Some("deepseek".to_string()),
            model: None,
        };

        let _ = coordinator
            .probe_all_inactive(&cfg, &client, &[], &sel)
            .await;

        assert!(
            counter_deepseek.load(Ordering::SeqCst) > 0,
            "selected provider must execute requests"
        );
        assert_eq!(
            counter_other.load(Ordering::SeqCst),
            0,
            "unselected provider must produce zero upstream requests"
        );
    }

    /// §4: disposition 各分支覆盖（executed/cached/coalesced/active）。
    #[tokio::test]
    async fn disposition_branches_coverage() {
        // Serde roundtrip for ProbeDisposition
        for (disp, expected_str) in [
            (ProbeDisposition::Executed, "\"executed\""),
            (ProbeDisposition::NotExecuted, "\"not_executed\""),
            (ProbeDisposition::Active, "\"active\""),
            (ProbeDisposition::Cached, "\"cached\""),
            (ProbeDisposition::Coalesced, "\"coalesced\""),
        ] {
            assert_eq!(serde_json::to_string(&disp).unwrap(), expected_str);
            let parsed: ProbeDisposition = serde_json::from_str(expected_str).unwrap();
            assert_eq!(parsed, disp);
        }

        let (_cache_dir, state) = cli_probe_state();
        let counter = Arc::new(AtomicUsize::new(0));
        let addr = spawn_ok_upstream(counter.clone()).await;
        let mut cfg = crate::config::default_deepseek_config();
        repoint_deepseek(&mut cfg, addr);

        let coordinator = ProbeCoordinator::new(state);
        let client = reqwest::Client::new();

        // 1. Executed: fresh probe executes
        let r1 = coordinator
            .probe("deepseek", "deepseek-v4-flash-lp", &cfg, &client)
            .await;
        assert_eq!(r1.disposition, ProbeDisposition::Executed);
        assert!(r1.executed);

        // 2. Cached: immediate repeat probe hits cache
        let r2 = coordinator
            .probe("deepseek", "deepseek-v4-flash-lp", &cfg, &client)
            .await;
        assert_eq!(r2.disposition, ProbeDisposition::Cached);
        assert!(!r2.executed);

        // 3. Coalesced: concurrent probe on new key merges waiter
        let (_cache_dir2, state2) = cli_probe_state();
        let coord2 = ProbeCoordinator::new(state2);
        let (c1, c2) = tokio::join!(
            coord2.probe("deepseek", "deepseek-v4-pro-lp", &cfg, &client),
            coord2.probe("deepseek", "deepseek-v4-pro-lp", &cfg, &client),
        );
        let disps = [c1.disposition, c2.disposition];
        assert!(
            disps.contains(&ProbeDisposition::Executed),
            "one flight must be Executed"
        );
        assert!(
            disps.contains(&ProbeDisposition::Coalesced),
            "the other flight must be Coalesced"
        );

        // 4. Active: active store has provider + cache has data
        let (_cache_dir3, cache_path3) = temp_cache_path();
        let store = Arc::new(ActiveProviderStore::new());
        store.mark_active("deepseek");
        let state3 = ServerProbeState::with_cache_path_and_oauth_path(
            store,
            cache_path3,
            crate::auth::default_state_path(),
        );
        state3.update_cache(
            "deepseek-v4-flash-lp:deepseek:chat_completions",
            &ProbeResult::Ok {
                latency_ms: 10,
                status: 200,
            },
        );
        let coord3 = ProbeCoordinator::new(state3);
        let r_active = coord3
            .probe("deepseek", "deepseek-v4-flash-lp", &cfg, &client)
            .await;
        assert_eq!(r_active.disposition, ProbeDisposition::Active);
        assert!(!r_active.executed);
    }

    #[tokio::test]
    async fn probe_stream_respects_concurrency_and_interval_pacing() {
        let (_cache_dir, state) = cli_probe_state();
        let counter = Arc::new(AtomicUsize::new(0));
        let addr = spawn_ok_upstream(counter.clone()).await;
        let mut cfg = crate::config::default_deepseek_config();
        repoint_deepseek(&mut cfg, addr);

        // 设置确定的平滑间隔 50ms，零抖动
        cfg.status.probe_concurrency = 2;
        cfg.status.probe_interval_ms = 50;
        cfg.status.probe_interval_jitter_ms = 0;

        let coordinator = ProbeCoordinator::new(state);
        let client = reqwest::Client::new();
        let active = Vec::new();
        let selection = ProbeSelection::default();

        let start = std::time::Instant::now();
        let stream = coordinator.probe_stream(&cfg, &client, &active, &selection);
        let outcomes: Vec<_> = stream.collect().await;
        let elapsed = start.elapsed();

        assert!(!outcomes.is_empty());
        // 至少经过了一个 interval_ms (50ms) 的平滑等待
        assert!(
            elapsed >= Duration::from_millis(45),
            "probe_stream must respect configured interval pacing (>=45ms), took: {elapsed:?}"
        );
    }
}
