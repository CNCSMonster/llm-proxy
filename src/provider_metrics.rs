//! Per-provider×model request metrics.
//!
//! In-memory counters, multi-window filtering, and sampling ring buffers,
//! exposed via `GET /admin/metrics` with snapshot persistence to SQLite.
//! Per ADR-038: `UsageRecord` is untouched — these are aggregate diagnostics,
//! not per-request properties.
//!
//! Design: bounded cost — O(1) recording per request (counters plus
//! fixed-capacity sampling); windows and percentiles derive at query time.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use dashmap::DashMap;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// Number of requests between ring-buffer samples.
/// 100-point buffer × 10 = ~1000 requests of history.
const SAMPLE_INTERVAL: u64 = 10;

/// Maximum samples kept per bucket.
const RING_CAPACITY: usize = 100;

/// Default retention period for persisted snapshots in days.
pub const DEFAULT_METRICS_RETENTION_DAYS: u32 = 7;

fn current_unix_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Ring Buffer ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct TimestampedSample {
    timestamp_sec: u64,
    value: f64,
}

/// Fixed-capacity ring buffer for latency/throughput samples with timestamps.
///
/// Written every `SAMPLE_INTERVAL` requests.
/// Query-side sort is O(n log n) on at most 100 elements (<1μs).
struct RingBuffer {
    data: Mutex<Vec<TimestampedSample>>,
    capacity: usize,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            data: Mutex::new(Vec::with_capacity(capacity)),
            capacity,
        }
    }

    fn push(&self, value: f64, now_sec: u64) {
        let mut data = self.data.lock().unwrap();
        if data.len() >= self.capacity {
            data.remove(0);
        }
        data.push(TimestampedSample {
            timestamp_sec: now_sec,
            value,
        });
    }

    fn sorted_snapshot(&self, min_timestamp_sec: Option<u64>) -> Vec<f64> {
        let data = self.data.lock().unwrap();
        let mut filtered: Vec<f64> = data
            .iter()
            .filter(|s| match min_timestamp_sec {
                Some(min_ts) => s.timestamp_sec >= min_ts,
                None => true,
            })
            .map(|s| s.value)
            .collect();
        filtered.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        filtered
    }
}

// ── Window Counters ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowKind {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "24h")]
    TwentyFourHours,
    #[serde(rename = "lifetime")]
    Lifetime,
}

impl WindowKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
            Self::TwentyFourHours => "24h",
            Self::Lifetime => "lifetime",
        }
    }

    pub fn duration_seconds(&self) -> Option<u64> {
        match self {
            Self::FiveMinutes => Some(300),
            Self::OneHour => Some(3600),
            Self::TwentyFourHours => Some(86400),
            Self::Lifetime => None,
        }
    }

    pub fn is_partial(&self, uptime_seconds: u64) -> bool {
        match self.duration_seconds() {
            Some(dur) => uptime_seconds < dur,
            None => false,
        }
    }
}

impl std::str::FromStr for WindowKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "5m" => Ok(Self::FiveMinutes),
            "1h" => Ok(Self::OneHour),
            "24h" => Ok(Self::TwentyFourHours),
            "lifetime" => Ok(Self::Lifetime),
            other => Err(format!(
                "invalid window '{other}', must be one of: 5m, 1h, 24h, lifetime"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct CounterSlot {
    start_time_sec: Option<u64>,
    total: u64,
    completed: u64,
    no_response: u64,
    upstream_error: u64,
    client_disconnected: u64,
}

impl CounterSlot {
    fn new(start_time_sec: u64) -> Self {
        Self {
            start_time_sec: Some(start_time_sec),
            total: 0,
            completed: 0,
            no_response: 0,
            upstream_error: 0,
            client_disconnected: 0,
        }
    }

    fn add(&mut self, status: Option<&str>) {
        self.total += 1;
        match status {
            Some("completed") => self.completed += 1,
            Some("upstream_error")
            | Some("unclassified_upstream_error")
            | Some("generation_failed") => {
                self.upstream_error += 1;
            }
            Some("upstream_interrupted") => self.no_response += 1,
            Some("client_disconnected") => self.client_disconnected += 1,
            _ => self.no_response += 1,
        }
    }
}

/// Circular bucketed array for sliding window request counts.
struct WindowCounters {
    window_duration_sec: u64,
    slot_duration_sec: u64,
    num_slots: usize,
    slots: Mutex<Vec<CounterSlot>>,
}

impl WindowCounters {
    fn new(window_duration_sec: u64, slot_duration_sec: u64) -> Self {
        let num_slots = (window_duration_sec / slot_duration_sec) as usize;
        let mut slots = Vec::with_capacity(num_slots);
        for _ in 0..num_slots {
            slots.push(CounterSlot::default());
        }
        Self {
            window_duration_sec,
            slot_duration_sec,
            num_slots,
            slots: Mutex::new(slots),
        }
    }

    fn record(&self, status: Option<&str>, now_sec: u64) {
        let slot_start = (now_sec / self.slot_duration_sec) * self.slot_duration_sec;
        let slot_idx = ((slot_start / self.slot_duration_sec) % self.num_slots as u64) as usize;
        let mut slots = self.slots.lock().unwrap();
        let slot = &mut slots[slot_idx];
        if slot.start_time_sec != Some(slot_start) {
            *slot = CounterSlot::new(slot_start);
        }
        slot.add(status);
    }

    fn query(&self, now_sec: u64) -> RequestCounts {
        let min_time_sec = now_sec.saturating_sub(self.window_duration_sec);
        let slots = self.slots.lock().unwrap();
        let mut total = 0u64;
        let mut completed = 0u64;
        let mut no_response = 0u64;
        let mut upstream_error = 0u64;
        let mut client_disconnected = 0u64;

        for slot in slots.iter() {
            if let Some(start_time) = slot.start_time_sec
                && start_time >= min_time_sec
                && start_time <= now_sec
            {
                total += slot.total;
                completed += slot.completed;
                no_response += slot.no_response;
                upstream_error += slot.upstream_error;
                client_disconnected += slot.client_disconnected;
            }
        }

        RequestCounts {
            total,
            completed,
            no_response,
            upstream_error,
            client_disconnected,
        }
    }
}

// ── Metric Bucket ────────────────────────────────────────────────────────────

/// Per-(provider, model) counters and sampling buffers.
struct MetricBucket {
    lifetime_total: AtomicU64,
    lifetime_completed: AtomicU64,
    lifetime_no_response: AtomicU64,
    lifetime_upstream_error: AtomicU64,
    lifetime_client_disconnected: AtomicU64,

    window_5m: WindowCounters,
    window_1h: WindowCounters,
    window_24h: WindowCounters,

    ttft_samples: RingBuffer,
    total_ms_samples: RingBuffer,
    tps_samples: RingBuffer,
    sample_counter: AtomicU64,
}

impl MetricBucket {
    fn new() -> Self {
        Self {
            lifetime_total: AtomicU64::new(0),
            lifetime_completed: AtomicU64::new(0),
            lifetime_no_response: AtomicU64::new(0),
            lifetime_upstream_error: AtomicU64::new(0),
            lifetime_client_disconnected: AtomicU64::new(0),
            window_5m: WindowCounters::new(300, 10),
            window_1h: WindowCounters::new(3600, 60),
            window_24h: WindowCounters::new(86400, 900),
            ttft_samples: RingBuffer::new(RING_CAPACITY),
            total_ms_samples: RingBuffer::new(RING_CAPACITY),
            tps_samples: RingBuffer::new(RING_CAPACITY),
            sample_counter: AtomicU64::new(0),
        }
    }

    fn record(
        &self,
        status: Option<&str>,
        ttfb_ms: Option<u64>,
        latency_ms: Option<i64>,
        output_tokens: i64,
        now_sec: u64,
    ) {
        self.lifetime_total.fetch_add(1, Ordering::Relaxed);

        // Update lifetime atomic counters.
        match status {
            Some("completed") => {
                self.lifetime_completed.fetch_add(1, Ordering::Relaxed);
            }
            Some("upstream_error") | Some("unclassified_upstream_error") => {
                self.lifetime_upstream_error.fetch_add(1, Ordering::Relaxed);
            }
            Some("upstream_interrupted") => {
                self.lifetime_no_response.fetch_add(1, Ordering::Relaxed);
            }
            Some("client_disconnected") => {
                self.lifetime_client_disconnected
                    .fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.lifetime_no_response.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Update multi-window counters.
        self.window_5m.record(status, now_sec);
        self.window_1h.record(status, now_sec);
        self.window_24h.record(status, now_sec);

        // Sampling — ring buffers written every SAMPLE_INTERVAL requests.
        let count = self.sample_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_multiple_of(SAMPLE_INTERVAL) {
            if let Some(ttfb) = ttfb_ms {
                self.ttft_samples.push(ttfb as f64, now_sec);
            }
            if let Some(latency) = latency_ms
                && latency >= 0
            {
                self.total_ms_samples.push(latency as f64, now_sec);
            }
            if output_tokens > 0
                && let (Some(latency), Some(ttfb)) = (latency_ms, ttfb_ms)
            {
                let gen_ms = latency.saturating_sub(ttfb as i64);
                if gen_ms > 0 {
                    let tps = output_tokens as f64 / gen_ms as f64 * 1000.0;
                    self.tps_samples.push(tps, now_sec);
                }
            }
        }
    }

    fn query_counts(&self, window: WindowKind, now_sec: u64) -> RequestCounts {
        match window {
            WindowKind::Lifetime => RequestCounts {
                total: self.lifetime_total.load(Ordering::Relaxed),
                completed: self.lifetime_completed.load(Ordering::Relaxed),
                no_response: self.lifetime_no_response.load(Ordering::Relaxed),
                upstream_error: self.lifetime_upstream_error.load(Ordering::Relaxed),
                client_disconnected: self.lifetime_client_disconnected.load(Ordering::Relaxed),
            },
            WindowKind::FiveMinutes => self.window_5m.query(now_sec),
            WindowKind::OneHour => self.window_1h.query(now_sec),
            WindowKind::TwentyFourHours => self.window_24h.query(now_sec),
        }
    }
}

// ── Snapshot Types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketSnapshot {
    pub provider: String,
    pub model: String,
    pub window: String,
    pub is_partial: bool,
    pub requests: RequestCounts,
    pub availability: AvailabilityStats,
    pub latency: LatencyStats,
    pub throughput: ThroughputStats,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestCounts {
    pub total: u64,
    pub completed: u64,
    pub no_response: u64,
    pub upstream_error: u64,
    pub client_disconnected: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailabilityStats {
    pub rate: f64,
    pub numerator: u64,
    pub denominator: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyStats {
    pub ttft_ms: Percentiles,
    pub total_ms: Percentiles,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThroughputStats {
    pub generation_tps: Percentiles,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Percentiles {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub sample_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub metrics: Vec<BucketSnapshot>,
    pub uptime_seconds: u64,
    pub generated_at: String,
    pub window: String,
    pub is_partial: bool,
}

// ── Persistence Types & Store ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalSnapshotEntry {
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    pub window: String,
    pub is_partial: bool,
    pub requests: RequestCounts,
    pub availability: AvailabilityStats,
    pub latency: LatencyStats,
    pub throughput: ThroughputStats,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MetricsHistoryQuery {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub window: Option<String>,
    pub since: Option<String>,
    pub limit: Option<usize>,
}

/// SQLite persistence store for periodic metrics snapshots.
pub struct MetricsStore {
    db_path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl MetricsStore {
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let _ = std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent);
            }
            #[cfg(not(unix))]
            {
                let _ = std::fs::create_dir_all(parent);
            }
        }

        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open metrics database: {}", db_path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("failed to set WAL mode for metrics db")?;

        conn.pragma_update(None, "busy_timeout", 5000)
            .context("failed to set busy_timeout for metrics db")?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS metrics_snapshots (
                id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp            TEXT NOT NULL,
                provider             TEXT NOT NULL,
                model                TEXT NOT NULL,
                window               TEXT NOT NULL,
                is_partial           INTEGER NOT NULL,
                total_requests       INTEGER NOT NULL,
                completed            INTEGER NOT NULL,
                no_response          INTEGER NOT NULL,
                upstream_error       INTEGER NOT NULL,
                client_disconnected  INTEGER NOT NULL,
                availability_rate    REAL NOT NULL,
                ttft_p50_ms          REAL NOT NULL,
                ttft_p95_ms          REAL NOT NULL,
                ttft_p99_ms          REAL NOT NULL,
                total_p50_ms         REAL NOT NULL,
                total_p95_ms         REAL NOT NULL,
                total_p99_ms         REAL NOT NULL,
                avg_tps              REAL NOT NULL,
                snapshot_json        TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_metrics_snapshots_ts ON metrics_snapshots(timestamp);
            CREATE INDEX IF NOT EXISTS idx_metrics_snapshots_prov_model ON metrics_snapshots(provider, model);
            "#,
        )
        .context("failed to create metrics_snapshots table")?;

        Ok(Self {
            db_path,
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    #[allow(dead_code)]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn write_snapshot(&self, snapshot: &MetricsSnapshot) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                INSERT INTO metrics_snapshots (
                    timestamp, provider, model, window, is_partial,
                    total_requests, completed, no_response, upstream_error,
                    client_disconnected, availability_rate,
                    ttft_p50_ms, ttft_p95_ms, ttft_p99_ms,
                    total_p50_ms, total_p95_ms, total_p99_ms,
                    avg_tps, snapshot_json
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                "#,
            )?;

            for bucket in &snapshot.metrics {
                let json = serde_json::to_string(bucket).unwrap_or_default();
                stmt.execute(params![
                    snapshot.generated_at,
                    bucket.provider,
                    bucket.model,
                    bucket.window,
                    if bucket.is_partial { 1 } else { 0 },
                    bucket.requests.total,
                    bucket.requests.completed,
                    bucket.requests.no_response,
                    bucket.requests.upstream_error,
                    bucket.requests.client_disconnected,
                    bucket.availability.rate,
                    bucket.latency.ttft_ms.p50,
                    bucket.latency.ttft_ms.p95,
                    bucket.latency.ttft_ms.p99,
                    bucket.latency.total_ms.p50,
                    bucket.latency.total_ms.p95,
                    bucket.latency.total_ms.p99,
                    bucket.throughput.generation_tps.p50,
                    json,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn query_history(
        &self,
        query: &MetricsHistoryQuery,
    ) -> Result<Vec<HistoricalSnapshotEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT timestamp, provider, model, window, is_partial, snapshot_json FROM metrics_snapshots WHERE 1=1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(p) = &query.provider {
            sql.push_str(" AND provider = ?");
            params_vec.push(Box::new(p.clone()));
        }
        if let Some(m) = &query.model {
            sql.push_str(" AND model = ?");
            params_vec.push(Box::new(m.clone()));
        }
        if let Some(since) = &query.since {
            sql.push_str(" AND timestamp >= ?");
            params_vec.push(Box::new(since.clone()));
        } else if let Some(w) = &query.window {
            if let Ok(kind) = w.parse::<WindowKind>() {
                if let Some(dur_sec) = kind.duration_seconds() {
                    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(dur_sec as i64);
                    sql.push_str(" AND timestamp >= ?");
                    params_vec.push(Box::new(cutoff.to_rfc3339()));
                }
            } else {
                sql.push_str(" AND window = ?");
                params_vec.push(Box::new(w.clone()));
            }
        }

        sql.push_str(" ORDER BY timestamp DESC, id DESC");

        let limit = query.limit.unwrap_or(100).min(1000);
        sql.push_str(" LIMIT ?");
        params_vec.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            let timestamp: String = row.get(0)?;
            let provider: String = row.get(1)?;
            let model: String = row.get(2)?;
            let window: String = row.get(3)?;
            let is_partial_int: i32 = row.get(4)?;
            let json: String = row.get(5)?;
            Ok((
                timestamp,
                provider,
                model,
                window,
                is_partial_int != 0,
                json,
            ))
        })?;

        let mut result = Vec::new();
        for item in rows {
            let (timestamp, provider, model, window, is_partial, json) = item?;
            if let Ok(bucket) = serde_json::from_str::<BucketSnapshot>(&json) {
                result.push(HistoricalSnapshotEntry {
                    timestamp,
                    provider: bucket.provider,
                    model: bucket.model,
                    window: bucket.window,
                    is_partial: bucket.is_partial,
                    requests: bucket.requests,
                    availability: bucket.availability,
                    latency: bucket.latency,
                    throughput: bucket.throughput,
                });
            } else {
                result.push(HistoricalSnapshotEntry {
                    timestamp,
                    provider,
                    model,
                    window,
                    is_partial,
                    requests: RequestCounts::default(),
                    availability: AvailabilityStats {
                        rate: 0.0,
                        numerator: 0,
                        denominator: 0,
                    },
                    latency: LatencyStats {
                        ttft_ms: Percentiles {
                            p50: 0.0,
                            p95: 0.0,
                            p99: 0.0,
                            sample_count: 0,
                        },
                        total_ms: Percentiles {
                            p50: 0.0,
                            p95: 0.0,
                            p99: 0.0,
                            sample_count: 0,
                        },
                    },
                    throughput: ThroughputStats {
                        generation_tps: Percentiles {
                            p50: 0.0,
                            p95: 0.0,
                            p99: 0.0,
                            sample_count: 0,
                        },
                    },
                });
            }
        }
        Ok(result)
    }

    pub fn cleanup_old_snapshots(&self, retention_days: u32) -> Result<usize> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days as i64);
        let cutoff_str = cutoff.to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM metrics_snapshots WHERE timestamp < ?1",
            params![cutoff_str],
        )?;
        Ok(deleted)
    }
}

// ── ProviderModelMetrics ─────────────────────────────────────────────────────

/// Top-level container: DashMap of (provider, model) → MetricBucket.
///
/// Cheap to clone (DashMap is Arc-backed). Held in AppState as `Arc<ProviderModelMetrics>`.
pub struct ProviderModelMetrics {
    buckets: DashMap<(String, String), MetricBucket>,
    start_time: Instant,
    store: Mutex<Option<Arc<MetricsStore>>>,
}

impl Default for ProviderModelMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderModelMetrics {
    pub fn new() -> Self {
        Self {
            buckets: DashMap::new(),
            start_time: Instant::now(),
            store: Mutex::new(None),
        }
    }

    pub fn with_store(store: Arc<MetricsStore>) -> Self {
        Self {
            buckets: DashMap::new(),
            start_time: Instant::now(),
            store: Mutex::new(Some(store)),
        }
    }

    pub fn store(&self) -> Option<Arc<MetricsStore>> {
        self.store.lock().unwrap().clone()
    }

    #[allow(dead_code)]
    pub fn set_store(&self, store: Arc<MetricsStore>) {
        *self.store.lock().unwrap() = Some(store);
    }

    /// Record a single request. Called from `AppState::record_usage_with_status`.
    pub fn record(
        &self,
        provider: &str,
        model: &str,
        status: Option<&str>,
        ttfb_ms: Option<u64>,
        latency_ms: Option<i64>,
        output_tokens: i64,
    ) {
        self.record_at(
            provider,
            model,
            status,
            ttfb_ms,
            latency_ms,
            output_tokens,
            current_unix_sec(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_at(
        &self,
        provider: &str,
        model: &str,
        status: Option<&str>,
        ttfb_ms: Option<u64>,
        latency_ms: Option<i64>,
        output_tokens: i64,
        now_sec: u64,
    ) {
        let bucket = self
            .buckets
            .entry((provider.to_string(), model.to_string()))
            .or_insert_with(MetricBucket::new);
        bucket.record(status, ttfb_ms, latency_ms, output_tokens, now_sec);
    }

    /// Take a point-in-time snapshot for the `/admin/metrics` response.
    pub fn snapshot(&self) -> MetricsSnapshot {
        self.snapshot_window(WindowKind::Lifetime, None, None)
    }

    /// Take a snapshot for a specific time window and optional provider/model filter.
    pub fn snapshot_window(
        &self,
        window: WindowKind,
        provider_filter: Option<&str>,
        model_filter: Option<&str>,
    ) -> MetricsSnapshot {
        self.snapshot_window_at(window, provider_filter, model_filter, current_unix_sec())
    }

    pub(crate) fn snapshot_window_at(
        &self,
        window: WindowKind,
        provider_filter: Option<&str>,
        model_filter: Option<&str>,
        now_sec: u64,
    ) -> MetricsSnapshot {
        let uptime_seconds = self.start_time.elapsed().as_secs();
        let is_partial = window.is_partial(uptime_seconds);
        let min_sample_ts = window
            .duration_seconds()
            .map(|dur| now_sec.saturating_sub(dur));

        let mut metrics = Vec::new();

        for entry in self.buckets.iter() {
            let ((provider, model), bucket) = entry.pair();
            if let Some(pf) = provider_filter
                && provider != pf
            {
                continue;
            }
            if let Some(mf) = model_filter
                && model != mf
            {
                continue;
            }

            let counts = bucket.query_counts(window, now_sec);

            // Filter zero-request buckets for the requested window.
            if counts.total == 0 {
                continue;
            }

            // Availability: available = completed + upstream_error
            //               unavailable = no_response
            //               client_disconnected excluded from both.
            let available = counts.completed + counts.upstream_error;
            let denominator = available + counts.no_response;
            let rate = if denominator > 0 {
                available as f64 / denominator as f64
            } else {
                0.0
            };

            let ttft_sorted = bucket.ttft_samples.sorted_snapshot(min_sample_ts);
            let total_sorted = bucket.total_ms_samples.sorted_snapshot(min_sample_ts);
            let tps_sorted = bucket.tps_samples.sorted_snapshot(min_sample_ts);

            metrics.push(BucketSnapshot {
                provider: provider.clone(),
                model: model.clone(),
                window: window.as_str().to_string(),
                is_partial,
                requests: counts,
                availability: AvailabilityStats {
                    rate,
                    numerator: available,
                    denominator,
                },
                latency: LatencyStats {
                    ttft_ms: compute_percentiles(&ttft_sorted),
                    total_ms: compute_percentiles(&total_sorted),
                },
                throughput: ThroughputStats {
                    generation_tps: compute_percentiles(&tps_sorted),
                },
            });
        }

        // Sort by (provider, model) for deterministic output.
        metrics.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.model.cmp(&b.model)));

        MetricsSnapshot {
            metrics,
            uptime_seconds,
            generated_at: chrono::Utc::now().to_rfc3339(),
            window: window.as_str().to_string(),
            is_partial,
        }
    }
}

fn compute_percentiles(sorted: &[f64]) -> Percentiles {
    if sorted.is_empty() {
        return Percentiles {
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            sample_count: 0,
        };
    }
    Percentiles {
        p50: percentile(sorted, 0.50),
        p95: percentile(sorted, 0.95),
        p99: percentile(sorted, 0.99),
        sample_count: sorted.len(),
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let idx = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_slot_maps_generation_failed_to_upstream_error() {
        let mut slot = CounterSlot::default();
        slot.add(Some("generation_failed"));
        assert_eq!(slot.upstream_error, 1);
        assert_eq!(slot.completed, 0);
        assert_eq!(slot.total, 1);
    }

    #[test]
    fn new_metrics_are_empty() {
        let m = ProviderModelMetrics::new();
        let s = m.snapshot();
        assert!(s.metrics.is_empty());
        assert!(s.uptime_seconds < 2);
        assert_eq!(s.window, "lifetime");
        assert!(!s.is_partial);
    }

    #[test]
    fn record_completed_request() {
        let m = ProviderModelMetrics::new();
        m.record(
            "openai",
            "gpt-4o",
            Some("completed"),
            Some(200),
            Some(1500),
            500,
        );
        let s = m.snapshot();
        assert_eq!(s.metrics.len(), 1);
        let b = &s.metrics[0];
        assert_eq!(b.provider, "openai");
        assert_eq!(b.model, "gpt-4o");
        assert_eq!(b.window, "lifetime");
        assert!(!b.is_partial);
        assert_eq!(b.requests.total, 1);
        assert_eq!(b.requests.completed, 1);
        assert_eq!(b.requests.upstream_error, 0);
        assert_eq!(b.requests.no_response, 0);
        assert_eq!(b.availability.numerator, 1);
        assert_eq!(b.availability.denominator, 1);
        assert!((b.availability.rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn upstream_error_counts_as_available() {
        let m = ProviderModelMetrics::new();
        m.record("openai", "gpt-4o", Some("upstream_error"), None, None, 0);
        let s = m.snapshot();
        let b = &s.metrics[0];
        assert_eq!(b.requests.upstream_error, 1);
        assert_eq!(b.availability.numerator, 1);
        assert_eq!(b.availability.denominator, 1);
    }

    #[test]
    fn unclassified_upstream_error_counts_as_available() {
        let m = ProviderModelMetrics::new();
        m.record(
            "openai",
            "gpt-4o",
            Some("unclassified_upstream_error"),
            None,
            None,
            0,
        );
        let s = m.snapshot();
        let b = &s.metrics[0];
        assert_eq!(b.requests.upstream_error, 1);
        assert_eq!(b.availability.numerator, 1);
    }

    #[test]
    fn upstream_interrupted_counts_as_unavailable() {
        let m = ProviderModelMetrics::new();
        m.record(
            "openai",
            "gpt-4o",
            Some("upstream_interrupted"),
            None,
            None,
            0,
        );
        let s = m.snapshot();
        let b = &s.metrics[0];
        assert_eq!(b.requests.no_response, 1);
        assert_eq!(b.availability.numerator, 0);
        assert_eq!(b.availability.denominator, 1);
        assert!((b.availability.rate - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn client_disconnected_excluded_from_denominator() {
        let m = ProviderModelMetrics::new();
        m.record(
            "openai",
            "gpt-4o",
            Some("client_disconnected"),
            None,
            None,
            0,
        );
        let s = m.snapshot();
        let b = &s.metrics[0];
        assert_eq!(b.requests.client_disconnected, 1);
        assert_eq!(b.availability.numerator, 0);
        assert_eq!(b.availability.denominator, 0);
    }

    #[test]
    fn availability_rate_mixed() {
        let m = ProviderModelMetrics::new();
        for _ in 0..8 {
            m.record("openai", "gpt-4o", Some("completed"), None, None, 0);
        }
        m.record("openai", "gpt-4o", Some("upstream_error"), None, None, 0);
        m.record(
            "openai",
            "gpt-4o",
            Some("upstream_interrupted"),
            None,
            None,
            0,
        );
        let s = m.snapshot();
        let b = &s.metrics[0];
        assert_eq!(b.availability.numerator, 9);
        assert_eq!(b.availability.denominator, 10);
        assert!((b.availability.rate - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn sampling_throttle_skips_ring_buffer_writes() {
        let m = ProviderModelMetrics::new();
        for i in 0..9 {
            m.record(
                "openai",
                "gpt-4o",
                Some("completed"),
                Some(100 + i as u64),
                Some(1000 + i as i64),
                200,
            );
        }
        let s = m.snapshot();
        let b = &s.metrics[0];
        assert_eq!(b.latency.ttft_ms.sample_count, 0);
        assert_eq!(b.latency.total_ms.sample_count, 0);
        assert_eq!(b.throughput.generation_tps.sample_count, 0);

        m.record(
            "openai",
            "gpt-4o",
            Some("completed"),
            Some(200),
            Some(2000),
            500,
        );
        let s = m.snapshot();
        let b = &s.metrics[0];
        assert_eq!(b.latency.ttft_ms.sample_count, 1);
        assert_eq!(b.latency.total_ms.sample_count, 1);
        assert!((b.latency.ttft_ms.p50 - 200.0).abs() < f64::EPSILON);
        assert!((b.latency.total_ms.p50 - 2000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn multiple_provider_model_buckets() {
        let m = ProviderModelMetrics::new();
        m.record("openai", "gpt-4o", Some("completed"), None, None, 0);
        m.record("anthropic", "claude-3", Some("completed"), None, None, 0);
        m.record(
            "anthropic",
            "claude-3",
            Some("upstream_error"),
            None,
            None,
            0,
        );
        let s = m.snapshot();
        assert_eq!(s.metrics.len(), 2);
        assert_eq!(s.metrics[0].provider, "anthropic");
        assert_eq!(s.metrics[0].requests.total, 2);
        assert_eq!(s.metrics[1].provider, "openai");
        assert_eq!(s.metrics[1].requests.total, 1);
    }

    #[test]
    fn zero_output_tokens_excluded_from_tps() {
        let m = ProviderModelMetrics::new();
        for _ in 0..10 {
            m.record(
                "openai",
                "gpt-4o",
                Some("completed"),
                Some(100),
                Some(1000),
                0,
            );
        }
        let s = m.snapshot();
        let b = &s.metrics[0];
        assert_eq!(b.latency.ttft_ms.sample_count, 1);
        assert_eq!(b.latency.total_ms.sample_count, 1);
        assert_eq!(b.throughput.generation_tps.sample_count, 0);
    }

    #[test]
    fn percentile_calculation() {
        let sorted = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        assert!((percentile(&sorted, 0.50) - 30.0).abs() < f64::EPSILON);
        assert!((percentile(&sorted, 0.0) - 10.0).abs() < f64::EPSILON);
        assert!((percentile(&sorted, 1.0) - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_percentiles() {
        let p = compute_percentiles(&[]);
        assert_eq!(p.sample_count, 0);
        assert!((p.p50 - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn total_ms_percentile_computation() {
        let m = ProviderModelMetrics::new();
        // 50 requests with different latencies (interval=10 => 5 samples)
        for i in 1..=50 {
            m.record(
                "openai",
                "gpt-4o",
                Some("completed"),
                Some(100),
                Some(i as i64 * 100),
                50,
            );
        }
        let s = m.snapshot();
        let b = &s.metrics[0];
        assert_eq!(b.latency.total_ms.sample_count, 5);
        // Samples recorded at i = 10, 20, 30, 40, 50: latencies 1000, 2000, 3000, 4000, 5000
        assert!((b.latency.total_ms.p50 - 3000.0).abs() < f64::EPSILON);
        assert!((b.latency.total_ms.p95 - 5000.0).abs() < f64::EPSILON);
        assert!((b.latency.total_ms.p99 - 5000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn window_filtering_and_aging() {
        let m = ProviderModelMetrics::new();

        // 5 requests at t = 100
        for _ in 0..5 {
            m.record_at(
                "openai",
                "gpt-4o",
                Some("completed"),
                Some(150),
                Some(800),
                100,
                100,
            );
        }

        // 3 requests at t = 600
        for _ in 0..3 {
            m.record_at(
                "openai",
                "gpt-4o",
                Some("completed"),
                Some(250),
                Some(1200),
                100,
                600,
            );
        }

        // Query at now = 610:
        // 5m window duration is 300s -> min_ts = 310.
        // t = 100 is older than 310, so aged out from 5m window!
        let s_5m = m.snapshot_window_at(WindowKind::FiveMinutes, None, None, 610);
        assert_eq!(s_5m.metrics.len(), 1);
        assert_eq!(s_5m.metrics[0].requests.total, 3);
        assert_eq!(s_5m.metrics[0].window, "5m");

        // 1h window duration is 3600s -> min_ts = 0.
        // Both t = 100 and t = 600 are included in 1h window!
        let s_1h = m.snapshot_window_at(WindowKind::OneHour, None, None, 610);
        assert_eq!(s_1h.metrics.len(), 1);
        assert_eq!(s_1h.metrics[0].requests.total, 8);
        assert_eq!(s_1h.metrics[0].window, "1h");

        // Lifetime includes all
        let s_life = m.snapshot_window_at(WindowKind::Lifetime, None, None, 610);
        assert_eq!(s_life.metrics.len(), 1);
        assert_eq!(s_life.metrics[0].requests.total, 8);
        assert_eq!(s_life.metrics[0].window, "lifetime");
    }

    #[test]
    fn window_stale_slot_excluded_across_ring_wrap() {
        let m = ProviderModelMetrics::new();

        // 陈旧批：t = 100。跨越远超 5m 环回周期后回查，该槽位不得复活。
        for _ in 0..5 {
            m.record_at(
                "openai",
                "gpt-4o",
                Some("completed"),
                Some(150),
                Some(800),
                100,
                100,
            );
        }

        // 新鲜批：t = 99950（落在 5m 窗口内）
        for _ in 0..3 {
            m.record_at(
                "openai",
                "gpt-4o",
                Some("completed"),
                Some(250),
                Some(1200),
                100,
                99950,
            );
        }

        // 回查 5m 窗口：min_ts = 99700。陈旧槽位（start_time=100）必须被
        // 绝对时间戳过滤排除——若泄漏则 total 会是 8 而非 3。
        let s = m.snapshot_window_at(WindowKind::FiveMinutes, None, None, 100_000);
        assert_eq!(s.metrics.len(), 1);
        assert_eq!(s.metrics[0].requests.total, 3);
        assert_eq!(s.metrics[0].window, "5m");

        // lifetime 口径不受窗口过滤影响，两批都保留。
        let s_life = m.snapshot_window_at(WindowKind::Lifetime, None, None, 100_000);
        assert_eq!(s_life.metrics.len(), 1);
        assert_eq!(s_life.metrics[0].requests.total, 8);
    }

    #[test]
    fn window_kind_parsing_and_partial_flag() {
        assert_eq!("5m".parse::<WindowKind>().unwrap(), WindowKind::FiveMinutes);
        assert_eq!("1h".parse::<WindowKind>().unwrap(), WindowKind::OneHour);
        assert_eq!(
            "24h".parse::<WindowKind>().unwrap(),
            WindowKind::TwentyFourHours
        );
        assert_eq!(
            "lifetime".parse::<WindowKind>().unwrap(),
            WindowKind::Lifetime
        );
        assert!("invalid".parse::<WindowKind>().is_err());

        // Partial window checks
        assert!(WindowKind::FiveMinutes.is_partial(200));
        assert!(!WindowKind::FiveMinutes.is_partial(300));
        assert!(!WindowKind::FiveMinutes.is_partial(500));

        assert!(WindowKind::OneHour.is_partial(3599));
        assert!(!WindowKind::OneHour.is_partial(3600));

        assert!(WindowKind::TwentyFourHours.is_partial(80000));
        assert!(!WindowKind::TwentyFourHours.is_partial(86400));

        assert!(!WindowKind::Lifetime.is_partial(10));
    }

    #[test]
    fn provider_and_model_filtering() {
        let m = ProviderModelMetrics::new();
        m.record("openai", "gpt-4o", Some("completed"), None, None, 0);
        m.record("anthropic", "claude-3", Some("completed"), None, None, 0);

        let s_openai = m.snapshot_window(WindowKind::Lifetime, Some("openai"), None);
        assert_eq!(s_openai.metrics.len(), 1);
        assert_eq!(s_openai.metrics[0].provider, "openai");

        let s_model = m.snapshot_window(WindowKind::Lifetime, None, Some("claude-3"));
        assert_eq!(s_model.metrics.len(), 1);
        assert_eq!(s_model.metrics[0].model, "claude-3");

        let s_none = m.snapshot_window(WindowKind::Lifetime, Some("google"), None);
        assert!(s_none.metrics.is_empty());
    }

    #[test]
    fn metrics_store_write_and_query_history() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("metrics.db");
        let store = MetricsStore::open(&db_path).unwrap();

        let m = ProviderModelMetrics::with_store(Arc::new(store));
        m.record(
            "openai",
            "gpt-4o",
            Some("completed"),
            Some(150),
            Some(1200),
            100,
        );
        m.record(
            "anthropic",
            "claude-3",
            Some("completed"),
            Some(200),
            Some(1800),
            200,
        );

        let snap = m.snapshot();
        assert_eq!(snap.metrics.len(), 2);

        let store_ref = m.store().unwrap();
        store_ref.write_snapshot(&snap).unwrap();

        let history = store_ref
            .query_history(&MetricsHistoryQuery::default())
            .unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].window, "lifetime");

        // Filter by provider
        let q_openai = MetricsHistoryQuery {
            provider: Some("openai".to_string()),
            ..Default::default()
        };
        let h_openai = store_ref.query_history(&q_openai).unwrap();
        assert_eq!(h_openai.len(), 1);
        assert_eq!(h_openai[0].provider, "openai");
        assert_eq!(h_openai[0].model, "gpt-4o");
    }

    #[test]
    fn metrics_store_restart_persistence_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("metrics.db");

        // First session: open, record, snapshot, write to disk
        {
            let store = MetricsStore::open(&db_path).unwrap();
            let m = ProviderModelMetrics::with_store(Arc::new(store));
            m.record(
                "openai",
                "gpt-4o",
                Some("completed"),
                Some(100),
                Some(1000),
                50,
            );
            let snap = m.snapshot();
            m.store().unwrap().write_snapshot(&snap).unwrap();
        }

        // Second session (simulated restart): open store on same path
        let reopened_store = MetricsStore::open(&db_path).unwrap();
        let history = reopened_store
            .query_history(&MetricsHistoryQuery::default())
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].provider, "openai");
        assert_eq!(history[0].model, "gpt-4o");
        assert_eq!(history[0].requests.total, 1);
        assert_eq!(history[0].requests.completed, 1);
    }

    #[test]
    fn metrics_store_cleanup_retention() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("metrics.db");
        let store = MetricsStore::open(&db_path).unwrap();

        let mut old_snap = ProviderModelMetrics::new().snapshot();
        old_snap.generated_at = "2020-01-01T00:00:00Z".to_string();
        old_snap.metrics.push(BucketSnapshot {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            window: "lifetime".to_string(),
            is_partial: false,
            requests: RequestCounts {
                total: 1,
                completed: 1,
                no_response: 0,
                upstream_error: 0,
                client_disconnected: 0,
            },
            availability: AvailabilityStats {
                rate: 1.0,
                numerator: 1,
                denominator: 1,
            },
            latency: LatencyStats {
                ttft_ms: Percentiles {
                    p50: 100.0,
                    p95: 100.0,
                    p99: 100.0,
                    sample_count: 1,
                },
                total_ms: Percentiles {
                    p50: 1000.0,
                    p95: 1000.0,
                    p99: 1000.0,
                    sample_count: 1,
                },
            },
            throughput: ThroughputStats {
                generation_tps: Percentiles {
                    p50: 50.0,
                    p95: 50.0,
                    p99: 50.0,
                    sample_count: 1,
                },
            },
        });
        store.write_snapshot(&old_snap).unwrap();

        let current_snap = MetricsSnapshot {
            metrics: old_snap.metrics.clone(),
            uptime_seconds: 10,
            generated_at: chrono::Utc::now().to_rfc3339(),
            window: "lifetime".to_string(),
            is_partial: false,
        };
        store.write_snapshot(&current_snap).unwrap();

        assert_eq!(
            store
                .query_history(&MetricsHistoryQuery::default())
                .unwrap()
                .len(),
            2
        );

        // Cleanup snapshots older than 7 days
        let deleted = store.cleanup_old_snapshots(7).unwrap();
        assert_eq!(deleted, 1);

        let remaining = store
            .query_history(&MetricsHistoryQuery::default())
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].timestamp, current_snap.generated_at);
    }
}
