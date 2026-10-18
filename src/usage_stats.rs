#![allow(dead_code)] // Query methods reserved for Admin API / TUI
//! Token usage statistics tracking.
//!
//! Storage architecture:
//! 1. Memory cache: recent records (max 1000) for fast queries and dirty tracking
//! 2. SQLite database: authoritative durable storage (50MB max) for all persisted records
//! 3. JSON Lines file: deprecated legacy format, zero-dependency, safe to delete

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::config::UsageConfig;

/// Maximum records to keep in memory
const DEFAULT_MEMORY_CACHE_SIZE: usize = 1000;

/// Default SQLite max size in bytes (50MB)
const DEFAULT_DB_MAX_SIZE_BYTES: u64 = 50 * 1024 * 1024;

/// 后台落盘任务的 tick 间隔（spec §12.6.3 智能落盘）。
const FLUSH_TICK_SECONDS: u64 = 15;

/// Normalize upstream prompt-cache fields across provider dialects:
/// DeepSeek (`prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`),
/// OpenAI (`prompt_tokens_details.cached_tokens`), Anthropic
/// (`cache_read_input_tokens` / `cache_creation_input_tokens`).
/// Returns (cache_hit_tokens, cache_miss_tokens) — None when the upstream
/// reports no cache data (fully backward compatible).
pub fn extract_cache_tokens(usage: &serde_json::Value) -> (Option<i64>, Option<i64>) {
    use serde_json::Value;
    let hit = usage
        .get("prompt_cache_hit_tokens")
        .and_then(Value::as_i64)
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(Value::as_i64)
        })
        .or_else(|| usage.get("cache_read_input_tokens").and_then(Value::as_i64));
    let miss = usage
        .get("prompt_cache_miss_tokens")
        .and_then(Value::as_i64)
        .or_else(|| {
            usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_i64)
        })
        .or_else(|| {
            let input = usage.get("prompt_tokens").and_then(Value::as_i64);
            match (input, hit) {
                (Some(i), Some(h)) => Some((i - h).max(0)),
                _ => None,
            }
        });
    (hit, miss)
}

/// A single usage record
///
/// ADR-038 四组划分：计量（tokens）+ 归属（model/provider/endpoint/token）+ 体验（latency/status）+ 判定（truncated/reason）。
/// `truncated` 与 `truncation_reason` 属"判定"组——请求固有的结果标记。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]

pub struct UsageRecord {
    /// Timestamp in ISO 8601 format
    pub timestamp: String,
    /// Model name used
    pub model: String,
    /// Provider name
    pub provider: String,
    /// Service endpoint (e.g., "openai_chat", "openai_responses", "anthropic")
    pub endpoint: String,
    /// Input tokens
    pub input_tokens: i64,
    /// Output tokens
    pub output_tokens: i64,
    /// Total tokens (input + output)
    pub total_tokens: i64,
    /// Prompt cache hit tokens (normalized from upstream cache fields:
    /// prompt_cache_hit_tokens / prompt_tokens_details.cached_tokens /
    /// cache_read_input_tokens) — None when upstream reports no cache data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_tokens: Option<i64>,
    /// Prompt cache miss tokens (prompt_cache_miss_tokens or input - hit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_miss_tokens: Option<i64>,
    /// End-to-end request latency in milliseconds, measured from the
    /// handler-entry clock (captured at the endpoint fn's first statement,
    /// after auth middleware and body extraction): non-streaming = response
    /// body fully read; streaming = stream end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<i64>,
    /// Token key that authorized this request (None in open mode)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_key: Option<String>,
    /// Human-readable token name for audit display
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_name: Option<String>,
    /// Terminal status of the request ("completed", "upstream_interrupted", "client_disconnected", "upstream_error")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Whether the output was truncated (ADR-038 判定组).
    #[serde(default)]
    pub truncated: bool,
    /// Truncation reason: "max_tokens", "content_filter", or "other".
    /// None when `truncated` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<String>,
    /// Time to first byte (ms): request sent → first upstream data received.
    /// Streaming: first SSE chunk arrival − request send time.
    /// Time to first byte in milliseconds from the same handler-entry clock
    /// as `latency_ms`: non-streaming = response headers arrival; streaming =
    /// first upstream chunk arrival.
    /// `None` when the measurement point was not reachable (e.g., auth failure before send).
    /// Belongs to the "experience" group alongside `latency_ms` (ADR-038).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_ms: Option<u64>,
    /// Leaf target model when the request went through Model Routing.
    /// `None` for regular (non-routed) models. Only the irreducible routing
    /// decision is persisted; the upstream model is derived via SCD Type 2
    /// dimension table `model_binding_revisions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_target: Option<String>,
}

impl UsageRecord {
    /// Attach normalized prompt-cache metrics (None = upstream reported none).
    pub fn with_cache_tokens(mut self, hit: Option<i64>, miss: Option<i64>) -> Self {
        self.cache_hit_tokens = hit;
        self.cache_miss_tokens = miss;
        self
    }

    pub fn new(
        model: String,
        provider: String,
        endpoint: String,
        input_tokens: i64,
        output_tokens: i64,
        latency_ms: Option<i64>,
    ) -> Self {
        let total_tokens = input_tokens + output_tokens;
        let timestamp = Utc::now().to_rfc3339();
        Self {
            timestamp,
            model,
            provider,
            endpoint,
            cache_hit_tokens: None,
            cache_miss_tokens: None,
            input_tokens,
            output_tokens,
            total_tokens,
            latency_ms,
            token_key: None,
            token_name: None,
            status: None,
            truncated: false,
            truncation_reason: None,
            ttfb_ms: None,
            routed_target: None,
        }
    }

    /// Attach the routing target when Model Routing was used.
    pub fn with_routed_target(mut self, target: impl Into<String>) -> Self {
        self.routed_target = Some(target.into());
        self
    }

    /// Override timestamp (useful for testing and replay).
    pub fn with_timestamp(mut self, ts: impl Into<String>) -> Self {
        self.timestamp = ts.into();
        self
    }

    /// Attach the authorizing token identity (§3.3).
    pub fn with_token(mut self, token_key: Option<String>, token_name: Option<String>) -> Self {
        self.token_key = token_key;
        self.token_name = token_name;
        self
    }

    /// Attach the terminal status of the request.
    pub fn with_status(mut self, status: Option<String>) -> Self {
        self.status = status;
        self
    }

    /// Attach truncation information (ADR-038 判定组).
    /// `reason` should be "max_tokens", "content_filter", or "other".
    pub fn with_truncation(mut self, truncated: bool, reason: Option<String>) -> Self {
        self.truncated = truncated;
        self.truncation_reason = if truncated { reason } else { None };
        self
    }

    /// Attach the time-to-first-byte measurement (ADR-038 "experience" group).
    pub fn with_ttfb(mut self, ttfb_ms: Option<u64>) -> Self {
        self.ttfb_ms = ttfb_ms;
        self
    }

    /// Parse timestamp as DateTime
    pub fn parsed_timestamp(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.timestamp)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Get date as NaiveDate
    pub fn date(&self) -> Option<NaiveDate> {
        // 本地时区日历日：与 history 分桶、CLI/TUI 日聚合同一口径
        self.parsed_timestamp()
            .map(|dt| dt.with_timezone(&Local).date_naive())
    }
}

impl UsageConfig {
    pub fn file_threshold_bytes(&self) -> u64 {
        (self.file_threshold_mb * 1024.0 * 1024.0) as u64
    }

    pub fn db_max_size_bytes(&self) -> u64 {
        (self.db_max_size_mb * 1024.0 * 1024.0) as u64
    }
}

#[cfg(test)]
type QueryCallback = Arc<dyn Fn() + Send + Sync>;

/// Thread-safe usage statistics store
///
/// spec §12.6 Usage 数据源设计：**内存为权威数据源，磁盘为持久化备份**。
/// `record` 只写内存 + 置 dirty；后台 `spawn_flush_task` 按请求频率分档
/// 定期把 pending 记录落盘（JSONL + SQLite 迁移）；优雅关停收尾显式 flush，
/// 崩溃/强杀仍最多丢一个落盘周期。
type CachedBindings = (Vec<(String, String, String)>, String);

#[derive(Clone)]
pub struct UsageStore {
    /// In-memory cache of recent records (权威数据源)
    memory_cache: Arc<Mutex<VecDeque<UsageRecord>>>,
    /// Records recorded since the last flush (尚未落盘的记录)
    pending_records: Arc<Mutex<VecDeque<UsageRecord>>>,
    /// Whether there is unsynced data on disk
    dirty: Arc<Mutex<bool>>,
    /// Timestamps of requests in the last 60s (sliding window for rate bucketing)
    recent_requests: Arc<Mutex<VecDeque<i64>>>,
    /// Last flush timestamp (rate-bucketed flush pacing)
    last_flush: Arc<Mutex<Instant>>,
    /// Serializes append + migration so a JSONL truncate-rewrite during
    /// `check_and_migrate` cannot race a concurrent `append_to_jsonl` and
    /// lose the newly appended records.
    io_lock: Arc<Mutex<()>>,
    /// Path to JSON Lines file
    jsonl_path: PathBuf,
    /// Path to SQLite database
    db_path: PathBuf,
    /// Cached model binding revisions for deferred syncing (preserves zero-I/O clean store contract)
    cached_bindings: Arc<Mutex<Option<CachedBindings>>>,
    /// Configuration
    config: UsageConfig,
    /// Max memory cache size
    max_cache_size: usize,
    #[cfg(test)]
    query_hook: Arc<Mutex<Option<QueryCallback>>>,
}

impl UsageStore {
    /// Create a new usage store with default paths (uses the default config instance).
    pub fn new(config: UsageConfig) -> Result<Self> {
        let state_dir = crate::service::state_dir();
        Self::new_with_state_dir(config, &state_dir)
    }

    /// Create a new usage store with an explicit state directory.
    pub fn new_with_state_dir(config: UsageConfig, state_dir: &Path) -> Result<Self> {
        let state_dir = state_dir.to_path_buf();
        // 0700: keep token usage state owner-only.
        // recursive(true) matches create_dir_all semantics (no-op if exists).
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&state_dir)
                .context("failed to create state directory")?;
        }
        #[cfg(not(unix))]
        {
            fs::create_dir_all(&state_dir).context("failed to create state directory")?;
        }

        let jsonl_path = state_dir.join("usage.jsonl");
        let db_path = state_dir.join("usage.db");

        let store = Self {
            memory_cache: Arc::new(Mutex::new(VecDeque::new())),
            pending_records: Arc::new(Mutex::new(VecDeque::new())),
            dirty: Arc::new(Mutex::new(false)),
            recent_requests: Arc::new(Mutex::new(VecDeque::new())),
            last_flush: Arc::new(Mutex::new(Instant::now())),
            io_lock: Arc::new(Mutex::new(())),
            jsonl_path,
            db_path,
            cached_bindings: Arc::new(Mutex::new(None)),
            config,
            max_cache_size: DEFAULT_MEMORY_CACHE_SIZE,
            #[cfg(test)]
            query_hook: Arc::new(Mutex::new(None)),
        };

        // Drain legacy usage.jsonl into SQLite if present (2026-09-27 决策反转，原 No-Op N2 作废；实现启动自动 drain 收编存量)
        if let Err(e) = store.drain_legacy_jsonl_to_sqlite() {
            tracing::warn!(
                target: "llm_proxy::usage_stats",
                error = %e,
                "failed to drain legacy jsonl to sqlite"
            );
        }

        // Load existing data into memory cache
        store.load_into_memory()?;

        Ok(store)
    }

    /// Create a usage store with custom paths (for testing)
    #[cfg(test)]
    pub fn with_paths(config: UsageConfig, jsonl_path: PathBuf, db_path: PathBuf) -> Result<Self> {
        let store = Self {
            memory_cache: Arc::new(Mutex::new(VecDeque::new())),
            pending_records: Arc::new(Mutex::new(VecDeque::new())),
            dirty: Arc::new(Mutex::new(false)),
            recent_requests: Arc::new(Mutex::new(VecDeque::new())),
            last_flush: Arc::new(Mutex::new(Instant::now())),
            io_lock: Arc::new(Mutex::new(())),
            jsonl_path,
            db_path,
            cached_bindings: Arc::new(Mutex::new(None)),
            config,
            max_cache_size: DEFAULT_MEMORY_CACHE_SIZE,
            query_hook: Arc::new(Mutex::new(None)),
        };
        if let Err(e) = store.drain_legacy_jsonl_to_sqlite() {
            tracing::warn!(
                target: "llm_proxy::usage_stats",
                error = %e,
                "failed to drain legacy jsonl to sqlite"
            );
        }
        store.load_into_memory()?;
        Ok(store)
    }

    /// Record a new usage entry.
    ///
    /// spec §12.6.1 内存权威：只写入内存 + pending 队列并置 dirty，**不直接落盘**。
    /// 磁盘同步由后台 `spawn_flush_task` 按频率分档负责；优雅关停由
    /// `AppState::flush_usage_on_shutdown` 收尾。
    pub fn record(&self, record: UsageRecord) -> Result<()> {
        // 1. 内存权威缓存
        {
            let mut cache = self.memory_cache.lock().unwrap();
            cache.push_back(record.clone());
            // Trim if over capacity
            while cache.len() > self.max_cache_size {
                cache.pop_front();
            }
        }
        // 2. pending 队列（本次 flush 周期内未落盘的记录）
        {
            let mut pending = self.pending_records.lock().unwrap();
            pending.push_back(record);
            // 防御：flush 长时间未跑时防止无限增长（上限 = 内存上限 2 倍）
            while pending.len() > self.max_cache_size * 2 {
                pending.pop_front();
            }
        }
        // 3. 标记 dirty + 更新 60s 请求窗口
        self.on_request();
        Ok(())
    }

    /// 记录一次请求时间戳并置 dirty（spec §12.6.3 频率统计滑动窗口）。
    fn on_request(&self) {
        *self.dirty.lock().unwrap() = true;
        let now = Utc::now().timestamp();
        let mut recent = self.recent_requests.lock().unwrap();
        recent.push_back(now);
        while let Some(&old) = recent.front() {
            if now - old > 60 {
                recent.pop_front();
            } else {
                break;
            }
        }
    }

    /// 是否有未落盘数据（spec §12.6.3 智能落盘状态机）。
    pub fn is_dirty(&self) -> bool {
        *self.dirty.lock().unwrap()
    }

    /// 按最近 60 秒请求频率计算落盘间隔（spec §12.6.3 频率分档）：
    /// 0 条 → 不主动落盘（dirty=false 时零 I/O）；1-10 → 180s；11-100 → 45s；>100 → 15s。
    pub fn calculate_interval(&self) -> Duration {
        let rpm = self.recent_requests.lock().unwrap().len() as u32;
        match rpm {
            0 => Duration::from_secs(180),
            1..=10 => Duration::from_secs(180),
            11..=100 => Duration::from_secs(45),
            _ => Duration::from_secs(15),
        }
    }

    /// 将 pending 记录落盘（直写 SQLite），成功/无 pending 后清 dirty。
    pub fn flush_if_dirty(&self) -> Result<()> {
        if !self.is_dirty() {
            return Ok(());
        }
        let _io = self
            .io_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let pending: Vec<UsageRecord> = {
            let mut p = self.pending_records.lock().unwrap();
            p.drain(..).collect()
        };
        if pending.is_empty() {
            *self.dirty.lock().unwrap() = false;
            *self.last_flush.lock().unwrap() = Instant::now();
            return Ok(());
        }

        if let Err(e) = self.insert_to_sqlite(&pending) {
            // 将未落盘成功的切片回塞至 pending_records 队首（保序契约）
            let mut p = self.pending_records.lock().unwrap();
            for record in pending.into_iter().rev() {
                p.push_front(record);
            }
            while p.len() > self.max_cache_size * 2 {
                p.pop_front();
            }
            return Err(e);
        }

        // SQLite 超额清理（超过 50MB 上限时自动清理最旧记录）
        if let Err(e) = self.check_and_cleanup_sqlite() {
            tracing::warn!(
                target: "llm_proxy::usage_stats",
                error = %e,
                "usage sqlite cleanup failed"
            );
        }

        *self.dirty.lock().unwrap() = false;
        *self.last_flush.lock().unwrap() = Instant::now();
        Ok(())
    }

    /// 启动后台智能落盘任务（spec §12.6.3）：每 15s tick 检查 dirty，
    /// 距上次落盘超过当前频率分档间隔才真正写盘。
    pub fn spawn_flush_task(store: UsageStore) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(FLUSH_TICK_SECONDS));
            loop {
                ticker.tick().await;
                if !store.is_dirty() {
                    continue;
                }
                let interval = store.calculate_interval();
                let last = *store.last_flush.lock().unwrap();
                if flush_due(last, interval, Instant::now())
                    && let Err(e) = store.flush_if_dirty()
                {
                    tracing::warn!("usage flush failed: {e}");
                }
            }
        })
    }

    /// Initialize SQLite database
    fn init_sqlite(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                model TEXT NOT NULL,
                provider TEXT NOT NULL,
                endpoint TEXT NOT NULL DEFAULT '',
                input_tokens INTEGER NOT NULL,
                cache_hit_tokens INTEGER,
                cache_miss_tokens INTEGER,
                output_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL,
                latency_ms INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_timestamp ON usage_records(timestamp);
            CREATE INDEX IF NOT EXISTS idx_model ON usage_records(model);
            CREATE INDEX IF NOT EXISTS idx_provider ON usage_records(provider);
            CREATE INDEX IF NOT EXISTS idx_endpoint ON usage_records(endpoint);",
        )?;

        // §3.3: token attribution columns. SQLite has no
        // `ADD COLUMN IF NOT EXISTS`, so tolerate the duplicate-column error
        // for databases created before these columns existed.
        for col in [
            "token_key TEXT",
            "token_name TEXT",
            "status TEXT",
            "truncated BOOLEAN NOT NULL DEFAULT 0",
            "truncation_reason TEXT",
            "ttfb_ms INTEGER",
            "routed_target TEXT",
            "cache_hit_tokens INTEGER",
            "cache_miss_tokens INTEGER",
        ] {
            match conn.execute_batch(&format!("ALTER TABLE usage_records ADD COLUMN {col};")) {
                Ok(()) => {}
                Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                    if msg.contains("duplicate column name") => {}
                Err(e) => return Err(e.into()),
            }
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_usage_records_token_key ON usage_records(token_key);
             CREATE INDEX IF NOT EXISTS idx_usage_records_token_ts_model ON usage_records(token_key, timestamp, model);",
        )?;

        // SCD Type 2 dimension table: tracks model→provider→upstream_model binding
        // revisions over time. Written only at startup and config reload (diff-based).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS model_binding_revisions (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                effective_model TEXT NOT NULL,
                provider        TEXT NOT NULL,
                upstream_model  TEXT NOT NULL,
                valid_from      TEXT NOT NULL,
                valid_to        TEXT,
                config_hash     TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_mbr_lookup
                ON model_binding_revisions(effective_model, provider, valid_from);",
        )?;

        Ok(conn)
    }

    /// Insert records into SQLite.
    ///
    /// The whole batch runs in one transaction: a crash mid-insert leaves no
    /// partial rows, so a record either stays in JSONL (transaction rolled
    /// back) or is fully in SQLite (committed) — never half-inserted.
    fn insert_to_sqlite(&self, records: &[UsageRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let mut conn = self.init_sqlite()?;

        let pending_bindings = self.cached_bindings.lock().unwrap().clone();
        if let Some((bindings, config_hash)) = pending_bindings {
            let _ = self.apply_model_binding_revisions(&mut conn, &bindings, &config_hash);
        }

        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
"INSERT INTO usage_records (timestamp, model, provider, endpoint, input_tokens, output_tokens, total_tokens, cache_hit_tokens, cache_miss_tokens, latency_ms, token_key, token_name, status, truncated, truncation_reason, ttfb_ms, routed_target)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"
            )?;

            for record in records {
                stmt.execute(params![
                    record.timestamp,
                    record.model,
                    record.provider,
                    record.endpoint,
                    record.input_tokens,
                    record.output_tokens,
                    record.total_tokens,
                    record.cache_hit_tokens,
                    record.cache_miss_tokens,
                    record.latency_ms,
                    record.token_key,
                    record.token_name,
                    record.status,
                    record.truncated,
                    record.truncation_reason,
                    record.ttfb_ms,
                    record.routed_target,
                ])?;
            }
        }
        tx.commit()?;

        Ok(())
    }

    /// Check SQLite size and cleanup if needed
    fn check_and_cleanup_sqlite(&self) -> Result<()> {
        let file_size = fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0);

        let max_size = self.config.db_max_size_bytes();
        if file_size <= max_size {
            return Ok(());
        }

        // Delete oldest records until under limit
        let conn = self.init_sqlite()?;

        // Delete oldest 10% of records
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM usage_records", [], |row| row.get(0))?;

        let delete_count = (count as f64 * 0.1) as i64;
        if delete_count > 0 {
            conn.execute(
                "DELETE FROM usage_records WHERE id IN (SELECT id FROM usage_records ORDER BY timestamp ASC LIMIT ?1)",
                params![delete_count],
            )?;
        }

        // Vacuum to reclaim space
        conn.execute_batch("VACUUM")?;

        Ok(())
    }

    /// 启动时一次性迁移存量 `usage.jsonl` 到 SQLite（去重），
    /// 确保半迁时代或未及落盘的历史记录全量找回（2026-09-27 决策反转，
    /// 原 No-Op N2「不写启动 drain」作废；实现启动自动 drain + 改名 `.migrated`）。
    fn drain_legacy_jsonl_to_sqlite(&self) -> Result<()> {
        if !self.jsonl_path.exists() {
            return Ok(());
        }
        let metadata = match fs::metadata(&self.jsonl_path) {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };
        if metadata.len() == 0 {
            return Ok(());
        }

        let file = fs::File::open(&self.jsonl_path)?;
        let reader = io::BufReader::new(file);
        let mut legacy_records = Vec::new();

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<UsageRecord>(trimmed) {
                legacy_records.push(record);
            }
        }

        if legacy_records.is_empty() {
            let migrated_path = self.jsonl_path.with_extension("jsonl.migrated");
            let _ = fs::rename(&self.jsonl_path, &migrated_path);
            return Ok(());
        }

        let mut conn = self.init_sqlite()?;

        // 读取已有 (timestamp, model, provider, endpoint, total_tokens) 用于去重
        let mut existing_keys = std::collections::HashSet::new();
        {
            let mut stmt = conn.prepare(
                "SELECT timestamp, model, provider, endpoint, total_tokens FROM usage_records",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?;
            for r in rows.flatten() {
                existing_keys.insert(r);
            }
        }

        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
"INSERT INTO usage_records (timestamp, model, provider, endpoint, input_tokens, output_tokens, total_tokens, cache_hit_tokens, cache_miss_tokens, latency_ms, token_key, token_name, status, truncated, truncation_reason, ttfb_ms, routed_target)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            )?;

            for record in legacy_records {
                let key = (
                    record.timestamp.clone(),
                    record.model.clone(),
                    record.provider.clone(),
                    record.endpoint.clone(),
                    record.total_tokens,
                );
                if !existing_keys.contains(&key) {
                    stmt.execute(params![
                        record.timestamp,
                        record.model,
                        record.provider,
                        record.endpoint,
                        record.input_tokens,
                        record.output_tokens,
                        record.total_tokens,
                        record.cache_hit_tokens,
                        record.cache_miss_tokens,
                        record.latency_ms,
                        record.token_key,
                        record.token_name,
                        record.status,
                        record.truncated,
                        record.truncation_reason,
                        record.ttfb_ms,
                        record.routed_target,
                    ])?;
                    existing_keys.insert(key);
                }
            }
        }
        tx.commit()?;

        // 迁移完成，将 usage.jsonl 重命名为 usage.jsonl.migrated
        let migrated_path = self.jsonl_path.with_extension("jsonl.migrated");
        let _ = fs::rename(&self.jsonl_path, &migrated_path);

        Ok(())
    }

    /// Load existing data into memory cache from SQLite (authoritative persistent store).
    /// JSONL is deprecated and no longer loaded on startup.
    fn load_into_memory(&self) -> Result<()> {
        let mut all_records = Vec::new();

        // Load from SQLite (historical records). Use `init_sqlite()` rather than
        // a raw open so the token_key/token_name ALTER TABLE migration runs for
        // databases created before those columns existed — otherwise a
        // pre-migration DB would fail the SELECT and silently disable the store.
        if self.db_path.exists()
            && let Ok(conn) = self.init_sqlite()
        {
            let mut stmt = conn.prepare(
"SELECT timestamp, model, provider, endpoint, input_tokens, output_tokens, total_tokens, cache_hit_tokens, cache_miss_tokens, latency_ms, token_key, token_name, status, truncated, truncation_reason, ttfb_ms, routed_target
                 FROM usage_records ORDER BY timestamp DESC LIMIT ?1"
            )?;

            let records = stmt.query_map(params![self.max_cache_size as i64], |row| {
                Ok(UsageRecord {
                    timestamp: row.get(0)?,
                    model: row.get(1)?,
                    provider: row.get(2)?,
                    endpoint: row.get(3)?,
                    input_tokens: row.get(4)?,
                    output_tokens: row.get(5)?,
                    total_tokens: row.get(6)?,
                    cache_hit_tokens: row.get(7)?,
                    cache_miss_tokens: row.get(8)?,
                    latency_ms: row.get(9)?,
                    token_key: row.get(10)?,
                    token_name: row.get(11)?,
                    status: row.get(12)?,
                    truncated: row.get::<_, bool>(13).unwrap_or(false),
                    truncation_reason: row.get(14)?,
                    ttfb_ms: row.get(15)?,
                    routed_target: row.get(16)?,
                })
            })?;

            for r in records.flatten() {
                all_records.push(r);
            }
        }

        all_records.reverse(); // oldest first in deque

        let mut cache = self.memory_cache.lock().unwrap();
        *cache = all_records.into();

        Ok(())
    }

    /// Get all records from memory cache
    pub fn get_all(&self) -> Vec<UsageRecord> {
        self.memory_cache.lock().unwrap().iter().cloned().collect()
    }

    /// Collect all matching records from memory cache and SQLite, sorted descending and deduplicated.
    /// Ensures memory cache (unflushed records) and SQLite (durable SSOT) are merged into one complete set.
    fn query_all_records(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        provider: Option<&str>,
        model: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Vec<UsageRecord>> {
        // 每次查询前先执行 flush，将内存中 pending 记录直接落库 SQLite，
        // 保证无论是 server /admin/usage、CLI 还是 TUI，查询点对齐统一的 durable 全集。
        let _ = self.flush_if_dirty();

        #[cfg(test)]
        {
            let hook = {
                let guard = self.query_hook.lock().unwrap();
                guard.clone()
            };
            if let Some(hook) = hook {
                hook();
            }
        }

        // Collect all matching records from memory cache
        let mut all_records: Vec<UsageRecord> = self
            .memory_cache
            .lock()
            .unwrap()
            .iter()
            .filter(|r| {
                if let Some(dt) = r.parsed_timestamp() {
                    if let Some(start) = start
                        && dt < start
                    {
                        return false;
                    }
                    if let Some(end) = end
                        && dt > end
                    {
                        return false;
                    }
                    if let Some(p) = provider
                        && r.provider != p
                    {
                        return false;
                    }
                    if let Some(m) = model
                        && r.model != m
                    {
                        return false;
                    }
                    if let Some(e) = endpoint
                        && r.endpoint != e
                    {
                        return false;
                    }
                    true
                } else {
                    false
                }
            })
            .cloned()
            .collect();

        // Also query SQLite for historical records
        if self.db_path.exists()
            && let Ok(conn) = self.init_sqlite()
        {
            let mut conditions = Vec::new();
            let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

            if let Some(start) = start {
                conditions.push(format!("timestamp >= ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(start.to_rfc3339()));
            }
            if let Some(end) = end {
                conditions.push(format!("timestamp <= ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(end.to_rfc3339()));
            }
            if let Some(provider) = provider {
                conditions.push(format!("provider = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(provider.to_string()));
            }
            if let Some(model) = model {
                conditions.push(format!("model = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(model.to_string()));
            }
            if let Some(endpoint) = endpoint {
                conditions.push(format!("endpoint = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(endpoint.to_string()));
            }

            let where_clause = if conditions.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", conditions.join(" AND "))
            };

            let query = format!(
"SELECT timestamp, model, provider, endpoint, input_tokens, output_tokens, total_tokens, cache_hit_tokens, cache_miss_tokens, latency_ms, token_key, token_name, status, truncated, truncation_reason, ttfb_ms, routed_target
                 FROM usage_records {} ORDER BY timestamp DESC",
                where_clause
            );

            let mut stmt = conn.prepare(&query)?;
            let records = stmt.query_map(rusqlite::params_from_iter(params_vec.iter()), |row| {
                Ok(UsageRecord {
                    timestamp: row.get(0)?,
                    model: row.get(1)?,
                    provider: row.get(2)?,
                    endpoint: row.get(3)?,
                    input_tokens: row.get(4)?,
                    output_tokens: row.get(5)?,
                    total_tokens: row.get(6)?,
                    cache_hit_tokens: row.get(7)?,
                    cache_miss_tokens: row.get(8)?,
                    latency_ms: row.get(9)?,
                    token_key: row.get(10)?,
                    token_name: row.get(11)?,
                    status: row.get(12)?,
                    truncated: row.get::<_, bool>(13).unwrap_or(false),
                    truncation_reason: row.get(14)?,
                    ttfb_ms: row.get(15)?,
                    routed_target: row.get(16)?,
                })
            })?;

            for r in records.flatten() {
                all_records.push(r);
            }
        }

        // Sort by timestamp descending (with secondary keys to ensure identical items are adjacent) and deduplicate
        all_records.sort_by(|a, b| {
            b.timestamp
                .cmp(&a.timestamp)
                .then_with(|| b.model.cmp(&a.model))
                .then_with(|| b.provider.cmp(&a.provider))
                .then_with(|| b.endpoint.cmp(&a.endpoint))
                .then_with(|| b.total_tokens.cmp(&a.total_tokens))
        });
        all_records.dedup_by(|a, b| a == b);

        Ok(all_records)
    }

    /// Get records filtered by time range (merges memory cache and SQLite)
    pub fn get_by_period(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> Vec<UsageRecord> {
        self.query(start, end, None, None, None)
    }

    /// Get records filtered by provider (merges memory cache and SQLite)
    pub fn get_by_provider(&self, provider: &str) -> Vec<UsageRecord> {
        self.query(None, None, Some(provider), None, None)
    }

    /// Get records filtered by model (merges memory cache and SQLite)
    pub fn get_by_model(&self, model: &str) -> Vec<UsageRecord> {
        self.query(None, None, None, Some(model), None)
    }

    /// Get records filtered by endpoint (merges memory cache and SQLite)
    pub fn get_by_endpoint(&self, endpoint: &str) -> Vec<UsageRecord> {
        self.query(None, None, None, None, Some(endpoint))
    }

    /// Query usage records with optional time range and attribute filters.
    /// Merges memory cache and SQLite to ensure full dataset coverage.
    pub fn query(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        provider: Option<&str>,
        model: Option<&str>,
        endpoint: Option<&str>,
    ) -> Vec<UsageRecord> {
        self.query_all_records(start, end, provider, model, endpoint)
            .unwrap_or_default()
    }

    /// Query usage records with pagination directly from SQLite and memory cache.
    /// Returns (records, total_count) where total_count is the count before pagination.
    /// Merges results from memory cache (recent unflushed records) and SQLite (historical records).
    #[allow(clippy::too_many_arguments)]
    pub fn query_with_pagination(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        provider: Option<&str>,
        model: Option<&str>,
        endpoint: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<UsageRecord>, usize)> {
        let all_records = self.query_all_records(start, end, provider, model, endpoint)?;
        let total = all_records.len();

        // Cap limit at 10000 to prevent memory issues
        let limit = limit.min(10000);

        // Apply pagination
        let paginated: Vec<UsageRecord> =
            all_records.into_iter().skip(offset).take(limit).collect();

        Ok((paginated, total))
    }

    #[cfg(test)]
    pub fn set_query_hook<F>(&self, hook: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.query_hook.lock().unwrap() = Some(Arc::new(hook));
    }

    /// Get paths for diagnostics
    pub fn jsonl_path(&self) -> &PathBuf {
        &self.jsonl_path
    }

    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }

    /// Aggregated usage group for a single time bucket or model bucket.
    pub fn query_history(
        &self,
        token_key: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        group_by: HistoryGroupBy,
    ) -> Result<HistoryQueryResult> {
        self.query_history_with_filter(token_key, start, end, group_by, None)
    }

    /// Aggregated usage group with optional model filtering.
    pub fn query_history_with_filter(
        &self,
        token_key: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        group_by: HistoryGroupBy,
        model_filter: Option<&str>,
    ) -> Result<HistoryQueryResult> {
        // Flush pending records to SQLite before querying
        let _ = self.flush_if_dirty();

        let conn = self.init_sqlite()?;
        let start_str = start.to_rfc3339();
        let end_str = end.to_rfc3339();

        let mut groups: Vec<HistoryGroup> = Vec::new();

        match group_by {
            HistoryGroupBy::Day => {
                let sql = if model_filter.is_some() {
                    "SELECT strftime('%Y-%m-%d', timestamp, 'localtime') AS time_key,
                            COUNT(*) AS requests,
                            COALESCE(SUM(input_tokens), 0) AS input_tokens,
                            COALESCE(SUM(output_tokens), 0) AS output_tokens,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                     FROM usage_records
                     WHERE token_key = ?1 AND timestamp >= ?2 AND timestamp < ?3 AND model = ?4
                     GROUP BY time_key
                     ORDER BY time_key DESC"
                } else {
                    "SELECT strftime('%Y-%m-%d', timestamp, 'localtime') AS time_key,
                            COUNT(*) AS requests,
                            COALESCE(SUM(input_tokens), 0) AS input_tokens,
                            COALESCE(SUM(output_tokens), 0) AS output_tokens,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                     FROM usage_records
                     WHERE token_key = ?1 AND timestamp >= ?2 AND timestamp < ?3
                     GROUP BY time_key
                     ORDER BY time_key DESC"
                };
                let mut stmt = conn.prepare(sql)?;
                let map_fn = |row: &rusqlite::Row| {
                    Ok(HistoryGroup {
                        key: row.get(0)?,
                        model: None,
                        requests: row.get(1)?,
                        input_tokens: row.get(2)?,
                        output_tokens: row.get(3)?,
                        total_tokens: row.get(4)?,
                    })
                };
                if let Some(m) = model_filter {
                    for row in stmt.query_map(params![token_key, start_str, end_str, m], map_fn)? {
                        groups.push(row?);
                    }
                } else {
                    for row in stmt.query_map(params![token_key, start_str, end_str], map_fn)? {
                        groups.push(row?);
                    }
                }
            }
            HistoryGroupBy::Hour => {
                let sql = if model_filter.is_some() {
                    "SELECT strftime('%Y-%m-%dT%H', timestamp, 'localtime') AS time_key,
                            COUNT(*) AS requests,
                            COALESCE(SUM(input_tokens), 0) AS input_tokens,
                            COALESCE(SUM(output_tokens), 0) AS output_tokens,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                     FROM usage_records
                     WHERE token_key = ?1 AND timestamp >= ?2 AND timestamp < ?3 AND model = ?4
                     GROUP BY time_key
                     ORDER BY time_key DESC"
                } else {
                    "SELECT strftime('%Y-%m-%dT%H', timestamp, 'localtime') AS time_key,
                            COUNT(*) AS requests,
                            COALESCE(SUM(input_tokens), 0) AS input_tokens,
                            COALESCE(SUM(output_tokens), 0) AS output_tokens,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                     FROM usage_records
                     WHERE token_key = ?1 AND timestamp >= ?2 AND timestamp < ?3
                     GROUP BY time_key
                     ORDER BY time_key DESC"
                };
                let mut stmt = conn.prepare(sql)?;
                let map_fn = |row: &rusqlite::Row| {
                    Ok(HistoryGroup {
                        key: row.get(0)?,
                        model: None,
                        requests: row.get(1)?,
                        input_tokens: row.get(2)?,
                        output_tokens: row.get(3)?,
                        total_tokens: row.get(4)?,
                    })
                };
                if let Some(m) = model_filter {
                    for row in stmt.query_map(params![token_key, start_str, end_str, m], map_fn)? {
                        groups.push(row?);
                    }
                } else {
                    for row in stmt.query_map(params![token_key, start_str, end_str], map_fn)? {
                        groups.push(row?);
                    }
                }
            }
            HistoryGroupBy::Model => {
                let sql = if model_filter.is_some() {
                    "SELECT model AS key,
                            COUNT(*) AS requests,
                            COALESCE(SUM(input_tokens), 0) AS input_tokens,
                            COALESCE(SUM(output_tokens), 0) AS output_tokens,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                     FROM usage_records
                     WHERE token_key = ?1 AND timestamp >= ?2 AND timestamp < ?3 AND model = ?4
                     GROUP BY model
                     ORDER BY total_tokens DESC, requests DESC, key ASC"
                } else {
                    "SELECT model AS key,
                            COUNT(*) AS requests,
                            COALESCE(SUM(input_tokens), 0) AS input_tokens,
                            COALESCE(SUM(output_tokens), 0) AS output_tokens,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                     FROM usage_records
                     WHERE token_key = ?1 AND timestamp >= ?2 AND timestamp < ?3
                     GROUP BY model
                     ORDER BY total_tokens DESC, requests DESC, key ASC"
                };
                let mut stmt = conn.prepare(sql)?;
                let map_fn = |row: &rusqlite::Row| {
                    let key: String = row.get(0)?;
                    Ok(HistoryGroup {
                        key: key.clone(),
                        model: Some(key),
                        requests: row.get(1)?,
                        input_tokens: row.get(2)?,
                        output_tokens: row.get(3)?,
                        total_tokens: row.get(4)?,
                    })
                };
                if let Some(m) = model_filter {
                    for row in stmt.query_map(params![token_key, start_str, end_str, m], map_fn)? {
                        groups.push(row?);
                    }
                } else {
                    for row in stmt.query_map(params![token_key, start_str, end_str], map_fn)? {
                        groups.push(row?);
                    }
                }
            }
            HistoryGroupBy::DayModel => {
                let sql = if model_filter.is_some() {
                    "SELECT strftime('%Y-%m-%d', timestamp, 'localtime') AS time_key,
                            model AS model_key,
                            COUNT(*) AS requests,
                            COALESCE(SUM(input_tokens), 0) AS input_tokens,
                            COALESCE(SUM(output_tokens), 0) AS output_tokens,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                     FROM usage_records
                     WHERE token_key = ?1 AND timestamp >= ?2 AND timestamp < ?3 AND model = ?4
                     GROUP BY time_key, model_key
                     ORDER BY time_key DESC, total_tokens DESC, model_key ASC"
                } else {
                    "SELECT strftime('%Y-%m-%d', timestamp, 'localtime') AS time_key,
                            model AS model_key,
                            COUNT(*) AS requests,
                            COALESCE(SUM(input_tokens), 0) AS input_tokens,
                            COALESCE(SUM(output_tokens), 0) AS output_tokens,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                     FROM usage_records
                     WHERE token_key = ?1 AND timestamp >= ?2 AND timestamp < ?3
                     GROUP BY time_key, model_key
                     ORDER BY time_key DESC, total_tokens DESC, model_key ASC"
                };
                let mut stmt = conn.prepare(sql)?;
                let map_fn = |row: &rusqlite::Row| {
                    Ok(HistoryGroup {
                        key: row.get(0)?,
                        model: Some(row.get(1)?),
                        requests: row.get(2)?,
                        input_tokens: row.get(3)?,
                        output_tokens: row.get(4)?,
                        total_tokens: row.get(5)?,
                    })
                };
                if let Some(m) = model_filter {
                    for row in stmt.query_map(params![token_key, start_str, end_str, m], map_fn)? {
                        groups.push(row?);
                    }
                } else {
                    for row in stmt.query_map(params![token_key, start_str, end_str], map_fn)? {
                        groups.push(row?);
                    }
                }
            }
            HistoryGroupBy::HourModel => {
                let sql = if model_filter.is_some() {
                    "SELECT strftime('%Y-%m-%dT%H', timestamp, 'localtime') AS time_key,
                            model AS model_key,
                            COUNT(*) AS requests,
                            COALESCE(SUM(input_tokens), 0) AS input_tokens,
                            COALESCE(SUM(output_tokens), 0) AS output_tokens,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                     FROM usage_records
                     WHERE token_key = ?1 AND timestamp >= ?2 AND timestamp < ?3 AND model = ?4
                     GROUP BY time_key, model_key
                     ORDER BY time_key DESC, total_tokens DESC, model_key ASC"
                } else {
                    "SELECT strftime('%Y-%m-%dT%H', timestamp, 'localtime') AS time_key,
                            model AS model_key,
                            COUNT(*) AS requests,
                            COALESCE(SUM(input_tokens), 0) AS input_tokens,
                            COALESCE(SUM(output_tokens), 0) AS output_tokens,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                     FROM usage_records
                     WHERE token_key = ?1 AND timestamp >= ?2 AND timestamp < ?3
                     GROUP BY time_key, model_key
                     ORDER BY time_key DESC, total_tokens DESC, model_key ASC"
                };
                let mut stmt = conn.prepare(sql)?;
                let map_fn = |row: &rusqlite::Row| {
                    Ok(HistoryGroup {
                        key: row.get(0)?,
                        model: Some(row.get(1)?),
                        requests: row.get(2)?,
                        input_tokens: row.get(3)?,
                        output_tokens: row.get(4)?,
                        total_tokens: row.get(5)?,
                    })
                };
                if let Some(m) = model_filter {
                    for row in stmt.query_map(params![token_key, start_str, end_str, m], map_fn)? {
                        groups.push(row?);
                    }
                } else {
                    for row in stmt.query_map(params![token_key, start_str, end_str], map_fn)? {
                        groups.push(row?);
                    }
                }
            }
        }

        let summary = HistorySummary {
            requests: groups.iter().map(|g| g.requests).sum(),
            input_tokens: groups.iter().map(|g| g.input_tokens).sum(),
            output_tokens: groups.iter().map(|g| g.output_tokens).sum(),
            total_tokens: groups.iter().map(|g| g.total_tokens).sum(),
        };

        Ok(HistoryQueryResult { summary, groups })
    }

    /// SCD Type 2 maintenance: compare current model bindings against the
    /// dimension table's current-effective rows and write revisions for any
    /// diffs. Called at startup and after config reload.
    pub fn sync_model_binding_revisions(
        &self,
        bindings: &[(String, String, String)],
        config_hash: &str,
    ) -> Result<()> {
        {
            let mut cached = self.cached_bindings.lock().unwrap();
            *cached = Some((bindings.to_vec(), config_hash.to_string()));
        }

        // If SQLite file does not exist yet (clean store), do not touch disk to preserve
        // zero-I/O clean store contract .
        if !self.db_path.exists() {
            return Ok(());
        }

        let mut conn = self.init_sqlite()?;
        self.apply_model_binding_revisions(&mut conn, bindings, config_hash)
    }

    fn apply_model_binding_revisions(
        &self,
        conn: &mut Connection,
        bindings: &[(String, String, String)],
        config_hash: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();

        // Collect current-effective rows (valid_to IS NULL)
        let mut current: HashMap<(String, String), (String, String)> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT effective_model, provider, upstream_model, config_hash
                 FROM model_binding_revisions WHERE valid_to IS NULL",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (em, prov, um, hash) = row?;
                current.insert((em.clone(), prov.clone()), (um, hash));
            }
        }

        let new_set: HashMap<(String, String), String> = bindings
            .iter()
            .map(|(em, prov, um)| ((em.clone(), prov.clone()), um.clone()))
            .collect();

        let tx = conn.transaction()?;

        // Close rows that are no longer current or whose upstream_model changed
        for ((em, prov), (old_um, _)) in &current {
            match new_set.get(&(em.clone(), prov.clone())) {
                Some(new_um) if new_um == old_um => {} // unchanged
                _ => {
                    tx.execute(
                        "UPDATE model_binding_revisions SET valid_to = ?1
                         WHERE effective_model = ?2 AND provider = ?3 AND valid_to IS NULL",
                        params![now, em, prov],
                    )?;
                }
            }
        }

        let valid_from = if current.is_empty() {
            "1970-01-01T00:00:00Z"
        } else {
            &now
        };

        // Insert new rows for added or changed bindings
        for ((em, prov), new_um) in &new_set {
            let changed = match current.get(&(em.clone(), prov.clone())) {
                Some((old_um, _)) => old_um != new_um,
                None => true,
            };
            if changed {
                tx.execute(
                    "INSERT INTO model_binding_revisions (effective_model, provider, upstream_model, valid_from, valid_to, config_hash)
                     VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                    params![em, prov, new_um, valid_from, config_hash],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Admin attribution query: joins usage_records with model_binding_revisions
    /// to resolve the physical upstream_model at the time of each record.
    pub fn query_attribution(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<AttributionGroup>> {
        let _ = self.flush_if_dirty();
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        let conn = self.init_sqlite()?;

        let mut stmt = conn.prepare(
            "SELECT COALESCE(b.upstream_model, '(unresolved)') AS upstream_model,
                    r.provider,
                    COALESCE(r.routed_target, r.model) AS effective_model,
                    COUNT(*) AS requests,
                    COALESCE(SUM(r.total_tokens), 0) AS total_tokens
             FROM usage_records r
             LEFT JOIN model_binding_revisions b
               ON b.effective_model = COALESCE(r.routed_target, r.model)
              AND b.provider = r.provider
              AND r.timestamp >= b.valid_from
              AND (b.valid_to IS NULL OR r.timestamp < b.valid_to)
             WHERE r.timestamp >= ?1 AND r.timestamp < ?2
             GROUP BY upstream_model, r.provider, effective_model
             ORDER BY total_tokens DESC",
        )?;

        let rows = stmt.query_map(params![start.to_rfc3339(), end.to_rfc3339()], |row| {
            Ok(AttributionGroup {
                upstream_model: row.get(0)?,
                provider: row.get(1)?,
                effective_model: row.get(2)?,
                requests: row.get(3)?,
                total_tokens: row.get(4)?,
            })
        })?;

        let mut groups = Vec::new();
        for row in rows {
            groups.push(row?);
        }
        Ok(groups)
    }
}

/// Admin attribution group: resolved upstream model + provider + effective model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributionGroup {
    pub upstream_model: String,
    pub provider: String,
    pub effective_model: String,
    pub requests: i64,
    pub total_tokens: i64,
}

/// Extract physical model bindings `(effective_model, provider, upstream_model)` from Config.
/// Model Routing entries are excluded as their physical bindings reside on leaf models.
pub fn extract_model_bindings(config: &crate::config::Config) -> Vec<(String, String, String)> {
    let mut bindings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (model_id, model_cfg) in &config.models {
        if model_cfg.reasoning_routes.is_some() {
            continue;
        }
        for binding in model_cfg
            .openai_chat_providers
            .iter()
            .chain(&model_cfg.openai_responses_providers)
            .chain(&model_cfg.anthropic_providers)
        {
            let item = (
                model_id.clone(),
                binding.name.clone(),
                binding.model.clone(),
            );
            if seen.insert(item.clone()) {
                bindings.push(item);
            }
        }
    }
    bindings.sort();
    bindings
}

/// Compute a deterministic hash for a set of model bindings.
pub fn compute_bindings_hash(bindings: &[(String, String, String)]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for (m, p, u) in bindings {
        hasher.update(m.as_bytes());
        hasher.update(b":");
        hasher.update(p.as_bytes());
        hasher.update(b":");
        hasher.update(u.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

/// Time aggregation granularity for usage history queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryGroupBy {
    Day,
    Hour,
    Model,
    DayModel,
    HourModel,
}

/// A single group in a history query result.
/// For time-based grouping: `key` is the time bucket, `model` is None.
/// For model-based grouping: `key` is the model name, `model` is the model name.
/// For composite grouping: `key` is the time bucket, `model` is the model name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryGroup {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

/// Aggregated summary across all groups.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorySummary {
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

/// Full result of a history query: summary + per-bucket groups.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryQueryResult {
    pub summary: HistorySummary,
    pub groups: Vec<HistoryGroup>,
}

/// spec §12.6.3 落盘决策：距上次落盘超过当前频率分档间隔才真正写盘。
fn flush_due(last_flush: Instant, interval: Duration, now: Instant) -> bool {
    now.duration_since(last_flush) >= interval
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_config() -> UsageConfig {
        UsageConfig {
            file_threshold_mb: 0.001, // 1KB for testing
            migration_ratio: 0.5,
            db_max_size_mb: 0.01, // 10KB for testing
        }
    }

    #[test]
    fn test_usage_record_creation() {
        let record = UsageRecord::new(
            "gpt-4".to_string(),
            "openai".to_string(),
            "openai_chat".to_string(),
            100,
            50,
            Some(1000),
        );

        assert_eq!(record.model, "gpt-4");
        assert_eq!(record.provider, "openai");
        assert_eq!(record.endpoint, "openai_chat");
        assert_eq!(record.input_tokens, 100);
        assert_eq!(record.output_tokens, 50);
        assert_eq!(record.total_tokens, 150);
        assert_eq!(record.latency_ms, Some(1000));
        assert!(!record.timestamp.is_empty());
    }

    #[test]
    fn test_usage_store_record_and_retrieve() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");

        let store =
            UsageStore::with_paths(test_config(), jsonl_path.clone(), db_path.clone()).unwrap();

        // Record some entries
        for i in 0..5 {
            let record = UsageRecord::new(
                format!("model-{}", i),
                "test-provider".to_string(),
                "openai_chat".to_string(),
                100 * i,
                50 * i,
                None,
            );
            store.record(record).unwrap();
        }

        // Verify all records are in memory (spec §12.6.1 内存权威)
        let all = store.get_all();
        assert_eq!(all.len(), 5);

        // spec §12.6.3：record 不直接落盘（dirty=true），flush 后写入 SQLite
        assert!(store.is_dirty());
        assert!(!db_path.exists(), "record should not flush synchronously");
        store.flush_if_dirty().unwrap();
        assert!(!store.is_dirty());
        assert!(db_path.exists(), "db must exist after flush");
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 10, 0)
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(records.len(), 5);
    }

    #[test]
    fn test_usage_store_filter_by_provider() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");

        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        store
            .record(UsageRecord::new(
                "gpt-4".into(),
                "openai".into(),
                "openai_chat".into(),
                100,
                50,
                None,
            ))
            .unwrap();
        store
            .record(UsageRecord::new(
                "claude".into(),
                "anthropic".into(),
                "anthropic".into(),
                200,
                100,
                None,
            ))
            .unwrap();
        store
            .record(UsageRecord::new(
                "gpt-4".into(),
                "openai".into(),
                "openai_chat".into(),
                150,
                75,
                None,
            ))
            .unwrap();

        let openai_records = store.get_by_provider("openai");
        assert_eq!(openai_records.len(), 2);

        let anthropic_records = store.get_by_provider("anthropic");
        assert_eq!(anthropic_records.len(), 1);
    }

    #[test]
    fn test_usage_store_filter_by_model() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");

        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        store
            .record(UsageRecord::new(
                "gpt-4".into(),
                "openai".into(),
                "openai_chat".into(),
                100,
                50,
                None,
            ))
            .unwrap();
        store
            .record(UsageRecord::new(
                "claude".into(),
                "anthropic".into(),
                "anthropic".into(),
                200,
                100,
                None,
            ))
            .unwrap();
        store
            .record(UsageRecord::new(
                "gpt-4".into(),
                "openrouter".into(),
                "openai_chat".into(),
                150,
                75,
                None,
            ))
            .unwrap();

        let gpt4_records = store.get_by_model("gpt-4");
        assert_eq!(gpt4_records.len(), 2);
    }

    #[test]
    fn test_usage_store_filter_by_endpoint() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");

        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        store
            .record(UsageRecord::new(
                "gpt-4".into(),
                "openai".into(),
                "openai_chat".into(),
                100,
                50,
                None,
            ))
            .unwrap();
        store
            .record(UsageRecord::new(
                "gpt-4".into(),
                "openai".into(),
                "openai_responses".into(),
                200,
                100,
                None,
            ))
            .unwrap();
        store
            .record(UsageRecord::new(
                "claude".into(),
                "anthropic".into(),
                "anthropic".into(),
                150,
                75,
                None,
            ))
            .unwrap();

        let chat_records = store.get_by_endpoint("openai_chat");
        assert_eq!(chat_records.len(), 1);

        let responses_records = store.get_by_endpoint("openai_responses");
        assert_eq!(responses_records.len(), 1);
    }

    #[test]
    fn test_jsonl_serialization() {
        let record = UsageRecord::new(
            "gpt-4".to_string(),
            "openai".to_string(),
            "openai_chat".to_string(),
            1234,
            567,
            Some(2345),
        );

        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"model\":\"gpt-4\""));
        assert!(json.contains("\"input_tokens\":1234"));
        assert!(json.contains("\"output_tokens\":567"));
        assert!(json.contains("\"total_tokens\":1801"));
        assert!(json.contains("\"latency_ms\":2345"));

        // Deserialize back
        let parsed: UsageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.model, "gpt-4");
        assert_eq!(parsed.total_tokens, 1801);
    }

    #[test]
    fn test_config_defaults() {
        let config = UsageConfig::default();
        assert_eq!(config.file_threshold_mb, 2.0);
        assert_eq!(config.migration_ratio, 0.5);
        assert_eq!(config.db_max_size_mb, 50.0);
        assert_eq!(config.file_threshold_bytes(), 2 * 1024 * 1024);
        assert_eq!(config.db_max_size_bytes(), 50 * 1024 * 1024);
    }

    #[test]
    fn concurrent_records_are_not_lost_during_flush() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let config = UsageConfig {
            file_threshold_mb: 0.001,
            migration_ratio: 0.5,
            db_max_size_mb: 1.0,
        };
        let store = UsageStore::with_paths(config, jsonl_path.clone(), db_path.clone()).unwrap();

        let mut handles = Vec::new();
        for thread_id in 0..8u32 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                for seq in 0..25u32 {
                    store
                        .record(UsageRecord::new(
                            format!("model-{thread_id}"),
                            "test".to_string(),
                            "openai_chat".to_string(),
                            seq as i64,
                            seq as i64,
                            None,
                        ))
                        .unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        // Memory cache keeps all 200 records (200 < 1000 cache cap).
        assert_eq!(
            store.get_all().len(),
            200,
            "no records lost in memory cache"
        );

        // flush 直接写入 SQLite，全集保存在 SQLite 中
        store.flush_if_dirty().unwrap();
        let db_rows: i64 = {
            let conn = Connection::open(&db_path).unwrap();
            conn.query_row("SELECT COUNT(*) FROM usage_records", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(db_rows, 200, "all 200 records directly persisted to sqlite");
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 500, 0)
            .unwrap();
        assert_eq!(total, 200);
        assert_eq!(records.len(), 200);
    }

    #[test]
    fn load_into_memory_ignores_legacy_jsonl_and_loads_from_sqlite() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path.clone(), db_path).unwrap();

        let db_record = UsageRecord::new(
            "sqlite-model".to_string(),
            "sqlite-provider".to_string(),
            "openai_chat".to_string(),
            100,
            50,
            Some(10),
        );

        // 写入一条记录到 SQLite
        store
            .insert_to_sqlite(std::slice::from_ref(&db_record))
            .unwrap();

        // 写入一条遗留孤儿记录到 JSONL
        let legacy_record = UsageRecord::new(
            "legacy-jsonl-model".to_string(),
            "legacy-provider".to_string(),
            "openai_chat".to_string(),
            200,
            100,
            Some(20),
        );
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&legacy_record).unwrap()),
        )
        .unwrap();

        // 重新执行 load_into_memory，断言只从 SQLite 加载，完全忽略 JSONL
        store.load_into_memory().unwrap();

        let loaded = store.get_all();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].model, "sqlite-model");

        // 查询接口也不读 JSONL
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 10, 0)
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(records[0].model, "sqlite-model");

        // 删除 JSONL 文件对查询完全无影响 (N15)
        fs::remove_file(&jsonl_path).unwrap();
        let (records_after_del, total_after_del) = store
            .query_with_pagination(None, None, None, None, None, 10, 0)
            .unwrap();
        assert_eq!(total_after_del, 1);
        assert_eq!(records_after_del[0].model, "sqlite-model");
    }

    #[test]
    fn smart_flush_pacing_frequency_buckets() {
        // spec §12.6.3 频率分档：0 → 180s；1-10 → 180s；11-100 → 45s；>100 → 15s
        let temp = tempdir().unwrap();
        let store = UsageStore::with_paths(
            test_config(),
            temp.path().join("u.jsonl"),
            temp.path().join("u.db"),
        )
        .unwrap();
        assert_eq!(store.calculate_interval(), Duration::from_secs(180));

        // 累计 10 条（1-10 档）→ 180s
        for _ in 0..10 {
            store
                .record(UsageRecord::new(
                    "m".into(),
                    "p".into(),
                    "e".into(),
                    1,
                    1,
                    None,
                ))
                .unwrap();
        }
        assert_eq!(store.calculate_interval(), Duration::from_secs(180));

        // 累计 100 条（11-100 档）→ 45s
        for _ in 0..90 {
            store
                .record(UsageRecord::new(
                    "m".into(),
                    "p".into(),
                    "e".into(),
                    1,
                    1,
                    None,
                ))
                .unwrap();
        }
        assert_eq!(store.calculate_interval(), Duration::from_secs(45));

        // 累计 120 条（>100 档）→ 15s
        for _ in 0..20 {
            store
                .record(UsageRecord::new(
                    "m".into(),
                    "p".into(),
                    "e".into(),
                    1,
                    1,
                    None,
                ))
                .unwrap();
        }
        assert_eq!(store.calculate_interval(), Duration::from_secs(15));
    }

    #[test]
    fn flush_due_decision_boundary() {
        // spec §12.6.3：未到间隔不落盘，到达/超过间隔落盘
        let now = Instant::now();
        assert!(!flush_due(
            now,
            Duration::from_secs(45),
            now + Duration::from_secs(44)
        ));
        assert!(flush_due(
            now,
            Duration::from_secs(45),
            now + Duration::from_secs(45)
        ));
        assert!(flush_due(
            now,
            Duration::from_secs(45),
            now + Duration::from_secs(46)
        ));
    }

    #[test]
    fn flush_is_noop_when_clean() {
        // spec §12.6.3：dirty=false 时 flush 不写盘（零 I/O）
        let temp = tempdir().unwrap();
        let store = UsageStore::with_paths(
            test_config(),
            temp.path().join("u.jsonl"),
            temp.path().join("u.db"),
        )
        .unwrap();
        store.flush_if_dirty().unwrap();
        assert!(!store.db_path.exists(), "clean store must not touch disk");
        assert!(!store.is_dirty());
    }

    #[test]
    fn flush_persists_only_pending_records() {
        // spec §12.6：flush 落盘 pending 到 SQLite；再次 record + flush 只追加新记录（无重复）
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("u.jsonl");
        let db_path = temp.path().join("u.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path.clone()).unwrap();

        store
            .record(UsageRecord::new(
                "m1".into(),
                "p".into(),
                "e".into(),
                1,
                1,
                None,
            ))
            .unwrap();
        store.flush_if_dirty().unwrap();
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 10, 0)
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(records[0].model, "m1");

        store
            .record(UsageRecord::new(
                "m2".into(),
                "p".into(),
                "e".into(),
                2,
                2,
                None,
            ))
            .unwrap();
        store.flush_if_dirty().unwrap();
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 10, 0)
            .unwrap();
        assert_eq!(total, 2);
        assert_eq!(records[0].model, "m2");
        assert_eq!(records[1].model, "m1");

        let db_rows: i64 = {
            let conn = Connection::open(&db_path).unwrap();
            conn.query_row("SELECT COUNT(*) FROM usage_records", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(db_rows, 2, "sqlite must hold exactly 2 records");
    }

    #[test]
    fn query_filters_records_correctly() {
        let temp = tempdir().unwrap();
        let store = UsageStore::with_paths(
            test_config(),
            temp.path().join("u.jsonl"),
            temp.path().join("u.db"),
        )
        .unwrap();

        store
            .record(UsageRecord::new(
                "m1".into(),
                "p1".into(),
                "chat".into(),
                10,
                20,
                None,
            ))
            .unwrap();
        store
            .record(UsageRecord::new(
                "m2".into(),
                "p1".into(),
                "responses".into(),
                30,
                40,
                None,
            ))
            .unwrap();
        store
            .record(UsageRecord::new(
                "m1".into(),
                "p2".into(),
                "chat".into(),
                50,
                60,
                None,
            ))
            .unwrap();

        assert_eq!(store.query(None, None, Some("p1"), None, None).len(), 2);
        assert_eq!(store.query(None, None, None, Some("m1"), None).len(), 2);
        assert_eq!(store.query(None, None, None, None, Some("chat")).len(), 2);
        let matched = store.query(None, None, Some("p1"), Some("m1"), Some("chat"));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].input_tokens, 10);
        assert_eq!(matched[0].output_tokens, 20);

        let none_matched = store.query(None, None, Some("nonexistent"), None, None);
        assert!(none_matched.is_empty());
    }

    #[test]
    fn test_query_with_pagination() {
        let dir = tempdir().unwrap();
        let jsonl_path = dir.path().join("usage.jsonl");
        let db_path = dir.path().join("usage.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        // Record 5 records
        for i in 0..5 {
            store
                .record(UsageRecord::new(
                    format!("model-{}", i),
                    "provider-1".into(),
                    "chat".into(),
                    i * 10,
                    i * 20,
                    None,
                ))
                .unwrap();
        }

        // Test basic pagination
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 2, 0)
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(records.len(), 2);

        // Test offset
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 2, 2)
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(records.len(), 2);

        // Test last page
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 2, 4)
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(records.len(), 1);

        // Test beyond total
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 2, 10)
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(records.len(), 0);

        // Test with filter
        let (records, total) = store
            .query_with_pagination(None, None, Some("provider-1"), None, None, 10, 0)
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(records.len(), 5);

        // Test limit cap at 10000
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 20000, 0)
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(records.len(), 5); // Should return all 5, not 10000
    }

    #[test]
    fn test_flush_converges_dirty_when_pending_is_empty() {
        // 当 pending 为空但 dirty 为 true 时，flush_if_dirty 必须将 dirty 收敛为 false
        let temp = tempdir().unwrap();
        let store = UsageStore::with_paths(
            test_config(),
            temp.path().join("u.jsonl"),
            temp.path().join("u.db"),
        )
        .unwrap();

        *store.dirty.lock().unwrap() = true;
        assert!(store.is_dirty());

        store.flush_if_dirty().unwrap();
        assert!(
            !store.is_dirty(),
            "dirty flag must converge to false even when pending is empty"
        );
    }

    #[test]
    fn test_flush_requeues_unwritten_records_on_io_failure() {
        // 当 SQLite 写入遇到 I/O 错误时，未落盘记录必须保序回塞 pending 队首，不能丢失
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("u.jsonl");
        let db_path = temp.path().join("u.db");

        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path.clone()).unwrap();

        // 将 db_path 建为一个目录，使 SQLite 打开/写入必然报错
        fs::create_dir(&db_path).unwrap();

        store
            .record(UsageRecord::new(
                "m1".into(),
                "p".into(),
                "e".into(),
                10,
                10,
                None,
            ))
            .unwrap();
        store
            .record(UsageRecord::new(
                "m2".into(),
                "p".into(),
                "e".into(),
                20,
                20,
                None,
            ))
            .unwrap();

        assert_eq!(store.pending_records.lock().unwrap().len(), 2);
        assert!(store.is_dirty());

        // 首次 flush 应失败
        let res = store.flush_if_dirty();
        assert!(res.is_err(), "flush must fail when db_path is a directory");

        // 核心断言：未写入的记录必须完整保留在 pending_records，dirty 必须保持 true
        assert_eq!(
            store.pending_records.lock().unwrap().len(),
            2,
            "pending records must be retained on write failure, not drained and lost"
        );
        assert!(store.is_dirty(), "dirty must remain true on write failure");

        // 在故障期间又有新记录到达
        store
            .record(UsageRecord::new(
                "m3".into(),
                "p".into(),
                "e".into(),
                30,
                30,
                None,
            ))
            .unwrap();
        assert_eq!(store.pending_records.lock().unwrap().len(), 3);

        // 解除故障：删除阻碍写入的目录
        fs::remove_dir(&db_path).unwrap();

        // 再次 flush 应成功
        store.flush_if_dirty().unwrap();
        assert_eq!(store.pending_records.lock().unwrap().len(), 0);
        assert!(!store.is_dirty());

        // 核验 SQLite 中的记录顺序与数量（m1, m2, m3 保持严格保序）
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 10, 0)
            .unwrap();
        assert_eq!(total, 3);
        // 按时间倒序返回：最新 m3，随后 m2，最旧 m1
        assert_eq!(records[0].model, "m3");
        assert_eq!(records[1].model, "m2");
        assert_eq!(records[2].model, "m1");
    }

    #[test]
    fn flush_persists_directly_to_sqlite_and_blind_spot_eliminated() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store =
            UsageStore::with_paths(UsageConfig::default(), jsonl_path.clone(), db_path.clone())
                .unwrap();

        // 写入 1200 条记录（超过默认内存缓存 1000 限制）
        for i in 0..1200 {
            store
                .record(UsageRecord::new(
                    format!("model-{i:04}"),
                    "test-provider".to_string(),
                    "openai_chat".to_string(),
                    10,
                    10,
                    None,
                ))
                .unwrap();
        }

        // flush 前：内存保留最新的 1000 条，pending 有 1200 条
        assert_eq!(store.get_all().len(), 1000);

        // 执行 flush_if_dirty 将所有 pending 直接落入 SQLite
        store.flush_if_dirty().unwrap();
        assert!(!store.is_dirty());

        // 验证 SQLite 中含有全部 1200 条记录
        let conn = Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_records", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1200, "all 1200 records must be present in SQLite");

        // 核心验收：query_with_pagination 查出 total 必须是 1200，绝非被截断的 1000！
        let (paginated, total) = store
            .query_with_pagination(None, None, None, None, None, 10, 0)
            .unwrap();
        assert_eq!(
            total, 1200,
            "total must reflect the full set of 1200 records"
        );
        assert_eq!(paginated.len(), 10);

        // 验证全量 query 和 get_by_period (TUI usage 口径) 均返回全集 1200
        assert_eq!(store.query(None, None, None, None, None).len(), 1200);
        assert_eq!(store.get_by_period(None, None).len(), 1200);

        // 验证分页警告触发条件：total (1200) > paginated.len() (10) 成立
        assert!(total > paginated.len());

        // 验证被内存驱逐的最旧 200 条记录（model-0000 到 model-0199）依然在查询全集末尾可查
        let (tail_records, _) = store
            .query_with_pagination(None, None, None, None, None, 10, 1190)
            .unwrap();
        assert_eq!(tail_records.len(), 10);
        assert_eq!(tail_records.last().unwrap().model, "model-0000");
    }

    #[test]
    fn restart_preserves_all_flushed_records_without_jsonl() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");

        {
            let store =
                UsageStore::with_paths(UsageConfig::default(), jsonl_path.clone(), db_path.clone())
                    .unwrap();
            for i in 0..1200 {
                store
                    .record(UsageRecord::new(
                        format!("model-{i:04}"),
                        "test-provider".to_string(),
                        "openai_chat".to_string(),
                        5,
                        5,
                        None,
                    ))
                    .unwrap();
            }
            store.flush_if_dirty().unwrap();
        }

        // 验证新架构对 usage.jsonl 零依赖：若磁盘存在任何 jsonl，删除它
        if jsonl_path.exists() {
            fs::remove_file(&jsonl_path).unwrap();
        }

        // 模拟进程重启
        let reopened = UsageStore::with_paths(UsageConfig::default(), jsonl_path, db_path).unwrap();

        // 重启加载：内存缓存仅从 SQLite 加载最新的 1000 条
        assert_eq!(reopened.get_all().len(), 1000);

        // 但查询接口通过 内存∪SQLite 保证全量 1200 条无损可查
        let (records, total) = reopened
            .query_with_pagination(None, None, None, None, None, 2000, 0)
            .unwrap();
        assert_eq!(total, 1200);
        assert_eq!(records.len(), 1200);

        // TUI usage 的 get_by_period 也返回全量 1200 条
        assert_eq!(reopened.get_by_period(None, None).len(), 1200);
    }

    #[test]
    fn drain_legacy_jsonl_deduplicates_and_recovers_missing_records() {
        use std::io::Write;
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");

        // 构造存量 usage.jsonl，写入 1106 条记录（模拟 09-26 单日 1106 条 jsonl、0 条 db 的生产场景）
        // 其中前 100 条先手动写入 SQLite 模拟部分半迁移历史，其余 1006 条为丢失存量
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS usage_records (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp TEXT NOT NULL,
                    model TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    endpoint TEXT NOT NULL,
                    input_tokens INTEGER NOT NULL,
                cache_hit_tokens INTEGER,
                cache_miss_tokens INTEGER,
                    output_tokens INTEGER NOT NULL,
                    total_tokens INTEGER NOT NULL,
                    latency_ms INTEGER
                );",
            )
            .unwrap();

            let mut jsonl_file = fs::File::create(&jsonl_path).unwrap();
            for i in 0..1106 {
                let rec = UsageRecord {
                    cache_hit_tokens: None,
                    cache_miss_tokens: None,
                    timestamp: format!("2026-09-26T12:{:02}:{:02}Z", (i / 60) % 60, i % 60),
                    model: if i < 254 {
                        "ljz-special-model".to_string()
                    } else {
                        format!("model-{i:04}")
                    },
                    provider: "test-provider".to_string(),
                    endpoint: "openai_chat".to_string(),
                    input_tokens: 10,
                    output_tokens: 10,
                    total_tokens: 20,
                    latency_ms: Some(100),
                    token_key: None,
                    token_name: None,
                    status: None,
                    truncated: false,
                    truncation_reason: None,
                    ttfb_ms: None,
                    routed_target: None,
                };
                let line = serde_json::to_string(&rec).unwrap();
                writeln!(jsonl_file, "{}", line).unwrap();

                // 前 100 条同时提前插入 SQLite，测试去重逻辑
                if i < 100 {
                    conn.execute(
                        "INSERT INTO usage_records (timestamp, model, provider, endpoint, input_tokens, output_tokens, total_tokens, latency_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            rec.timestamp,
                            rec.model,
                            rec.provider,
                            rec.endpoint,
                            rec.input_tokens,
                            rec.output_tokens,
                            rec.total_tokens,
                            rec.latency_ms,
                        ],
                    )
                    .unwrap();
                }
            }
            jsonl_file.flush().unwrap();
        }

        // 启动/初始化 UsageStore
        let store =
            UsageStore::with_paths(UsageConfig::default(), jsonl_path.clone(), db_path.clone())
                .unwrap();

        // 1. 验证存量 jsonl 被重命名为 usage.jsonl.migrated，不再保留未迁移的 usage.jsonl
        assert!(
            !jsonl_path.exists(),
            "original usage.jsonl should be drained and renamed"
        );
        let migrated_path = temp.path().join("usage.jsonl.migrated");
        assert!(migrated_path.exists(), "usage.jsonl.migrated must exist");

        // 2. 验证 SQLite 中全量包含 1106 条（100 条已存在未重复插入，1006 条存量被全部补齐恢复）
        let conn = Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_records", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 1106,
            "all 1106 records must be present in SQLite without duplication"
        );

        // 3. 验证用户 ljz 的 254 条全量可查，不再全部丢失！
        let (ljz_records, ljz_total) = store
            .query_with_pagination(None, None, None, Some("ljz-special-model"), None, 10000, 0)
            .unwrap();
        assert_eq!(
            ljz_total, 254,
            "ljz 254 records must be completely restored"
        );
        assert_eq!(ljz_records.len(), 254);

        // 4. 验证总数查询全集无截断（CLI --limit 10000 返回 1106，而非截断的 957）
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 10000, 0)
            .unwrap();
        assert_eq!(total, 1106, "total must be exactly 1106");
        assert_eq!(records.len(), 1106);
    }

    #[test]
    fn query_flushes_dirty_in_memory_records_before_returning() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store =
            UsageStore::with_paths(UsageConfig::default(), jsonl_path, db_path.clone()).unwrap();

        // 录入 50 条记录，此时不显式调用 flush_if_dirty
        for i in 0..50 {
            store
                .record(UsageRecord::new(
                    format!("model-{i}"),
                    "prov".into(),
                    "chat".into(),
                    1,
                    1,
                    None,
                ))
                .unwrap();
        }
        assert!(store.is_dirty());

        // 执行查询：query_with_pagination / query / get_by_period 会在查询前触发 flush_if_dirty
        let (records, total) = store
            .query_with_pagination(None, None, None, None, None, 100, 0)
            .unwrap();
        assert_eq!(total, 50);
        assert_eq!(records.len(), 50);
        assert!(
            !store.is_dirty(),
            "store dirty flag should be cleared after query auto-flush"
        );

        // 确认 SQLite 已落盘
        let conn = Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_records", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 50);
    }

    #[test]
    fn test_query_history_basic() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        // Insert records with token_key for two different tokens.
        // 固定正午 UTC 锚点：以真实 now 相对布点时，UTC 午夜~05:00 运行会让
        // 同一组记录横跨两个 UTC 日，day 分组断言随运行时刻翻转。
        let base = DateTime::parse_from_rfc3339("2026-06-15T12:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        for i in 0..5 {
            let mut record = UsageRecord::new(
                "gpt-4".into(),
                "openai".into(),
                "openai_chat".into(),
                100 * (i + 1),
                50 * (i + 1),
                None,
            );
            record.token_key = Some("token-a".into());
            record.token_name = Some("Alice".into());
            // Spread across different hours
            record.timestamp = (base - chrono::Duration::hours(i)).to_rfc3339();
            store.record(record).unwrap();
        }
        for i in 0..3 {
            let mut record = UsageRecord::new(
                "claude".into(),
                "anthropic".into(),
                "anthropic".into(),
                200 * (i + 1),
                100 * (i + 1),
                None,
            );
            record.token_key = Some("token-b".into());
            record.token_name = Some("Bob".into());
            record.timestamp = (base - chrono::Duration::hours(i)).to_rfc3339();
            store.record(record).unwrap();
        }
        store.flush_if_dirty().unwrap();

        // Query for token-a: should see 5 records, not token-b's
        let start = base - chrono::Duration::days(1);
        let end = base + chrono::Duration::hours(1);
        let result = store
            .query_history("token-a", start, end, HistoryGroupBy::Day)
            .unwrap();
        assert_eq!(result.summary.requests, 5);
        // token-b data must not leak
        assert_eq!(result.groups.len(), 1); // all same day

        // Query for token-b: should see 3 records
        let result = store
            .query_history("token-b", start, end, HistoryGroupBy::Day)
            .unwrap();
        assert_eq!(result.summary.requests, 3);

        // Query with hour grouping
        let result = store
            .query_history("token-a", start, end, HistoryGroupBy::Hour)
            .unwrap();
        assert!(!result.groups.is_empty());
        // Key format for hour: "YYYY-MM-DDTHH"
        for g in &result.groups {
            assert!(g.key.contains('T'), "hour key should contain T: {}", g.key);
        }
    }

    #[test]
    fn test_query_history_data_isolation() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        let now = Utc::now();
        let mut record_a = UsageRecord::new(
            "gpt-4".into(),
            "openai".into(),
            "openai_chat".into(),
            1000,
            500,
            None,
        );
        record_a.token_key = Some("secret-key-a".into());
        record_a.timestamp = now.to_rfc3339();
        store.record(record_a).unwrap();

        let mut record_b = UsageRecord::new(
            "claude".into(),
            "anthropic".into(),
            "anthropic".into(),
            2000,
            1000,
            None,
        );
        record_b.token_key = Some("secret-key-b".into());
        record_b.timestamp = now.to_rfc3339();
        store.record(record_b).unwrap();
        store.flush_if_dirty().unwrap();

        let start = now - chrono::Duration::days(1);
        let end = now + chrono::Duration::hours(1);

        // token A only sees its own data
        let result_a = store
            .query_history("secret-key-a", start, end, HistoryGroupBy::Day)
            .unwrap();
        assert_eq!(result_a.summary.requests, 1);
        assert_eq!(result_a.summary.input_tokens, 1000);

        // token B only sees its own data
        let result_b = store
            .query_history("secret-key-b", start, end, HistoryGroupBy::Day)
            .unwrap();
        assert_eq!(result_b.summary.requests, 1);
        assert_eq!(result_b.summary.input_tokens, 2000);

        // unknown token sees nothing
        let result_c = store
            .query_history("unknown-key", start, end, HistoryGroupBy::Day)
            .unwrap();
        assert_eq!(result_c.summary.requests, 0);
        assert!(result_c.groups.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn state_dir_created_with_restrictive_permissions() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("nested").join("state");
        let config = UsageConfig::default();
        let _store = UsageStore::new_with_state_dir(config, &state_dir).unwrap();
        let meta = std::fs::metadata(&state_dir).unwrap();
        let mode = meta.mode() & 0o777;
        assert_eq!(
            mode & 0o022,
            0,
            "state dir must not be group/other writable (got {:o})",
            mode
        );
    }

    #[test]
    #[cfg(unix)]
    fn existing_legacy_775_dir_tightened_to_700() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("legacy-state");
        std::fs::create_dir(&state_dir).unwrap();
        // Simulate legacy bare create_dir_all result (umask=0002 → 0775)
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(std::fs::metadata(&state_dir).unwrap().mode() & 0o777, 0o775);
        // ensure_private_state_dir should tighten it to 0700
        crate::service::ensure_private_state_dir(&state_dir).unwrap();
        let mode = std::fs::metadata(&state_dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700, "legacy 0775 dir must be tightened to 0700");
    }

    // ── : TTFB attribution tests ──────────────────────────────

    #[test]
    fn ttfb_with_builder_attaches_value() {
        let record = UsageRecord::new("m".into(), "p".into(), "e".into(), 1, 2, Some(500))
            .with_ttfb(Some(123));
        assert_eq!(record.ttfb_ms, Some(123));
        // latency_ms must remain untouched
        assert_eq!(record.latency_ms, Some(500));
    }

    #[test]
    fn ttfb_defaults_to_none() {
        let record = UsageRecord::new("m".into(), "p".into(), "e".into(), 1, 2, None);
        assert_eq!(record.ttfb_ms, None);
    }

    #[test]
    fn ttfb_jsonl_serialization_roundtrip() {
        let record = UsageRecord::new("m".into(), "p".into(), "e".into(), 10, 20, Some(300))
            .with_ttfb(Some(42));
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            json.contains("\"ttfb_ms\":42"),
            "ttfb_ms must appear in JSONL: {json}"
        );
        let parsed: UsageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ttfb_ms, Some(42));
    }

    #[test]
    fn ttfb_jsonl_omits_none() {
        let record = UsageRecord::new("m".into(), "p".into(), "e".into(), 10, 20, Some(300));
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            !json.contains("ttfb_ms"),
            "None ttfb_ms must be omitted from JSONL: {json}"
        );
    }

    #[test]
    fn ttfb_sqlite_persistence_roundtrip() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        let record = UsageRecord::new(
            "ttfb-model".into(),
            "ttfb-provider".into(),
            "openai_chat".into(),
            100,
            50,
            Some(1000),
        )
        .with_ttfb(Some(250));
        store.record(record).unwrap();
        store.flush_if_dirty().unwrap();

        let results = store.query(None, None, Some("ttfb-provider"), Some("ttfb-model"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].ttfb_ms, Some(250));
        // latency_ms must not be corrupted by ttfb_ms
        assert_eq!(results[0].latency_ms, Some(1000));
    }

    #[test]
    fn ttfb_none_sqlite_persistence_roundtrip() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        let record = UsageRecord::new("no-ttfb".into(), "p".into(), "e".into(), 1, 2, Some(100));
        assert_eq!(record.ttfb_ms, None);
        store.record(record).unwrap();
        store.flush_if_dirty().unwrap();

        let results = store.query(None, None, Some("p"), Some("no-ttfb"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].ttfb_ms, None);
    }

    // =========================================================================
    // : Routed target, Model grouping, SCD Type 2 attribution tests
    // =========================================================================

    #[test]
    fn routed_target_sqlite_persistence_roundtrip() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        let record = UsageRecord::new(
            "gemini-3.8-flash-lp".into(),
            "google".into(),
            "openai_chat".into(),
            100,
            50,
            Some(1000),
        )
        .with_routed_target("gemini-3.8-flash-high-lp");

        store.record(record).unwrap();
        store.flush_if_dirty().unwrap();

        let results = store.query(
            None,
            None,
            Some("google"),
            Some("gemini-3.8-flash-lp"),
            None,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].routed_target.as_deref(),
            Some("gemini-3.8-flash-high-lp")
        );
    }

    #[test]
    fn legacy_db_migration_adds_routed_target_and_scd_table() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("usage.db");

        // Create a legacy schema database without routed_target and without model_binding_revisions
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE usage_records (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp TEXT NOT NULL,
                    model TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    endpoint TEXT NOT NULL,
                    input_tokens INTEGER NOT NULL,
                cache_hit_tokens INTEGER,
                cache_miss_tokens INTEGER,
                    output_tokens INTEGER NOT NULL,
                    total_tokens INTEGER NOT NULL,
                    latency_ms INTEGER,
                    token_key TEXT,
                    token_name TEXT,
                    status TEXT,
                    truncated BOOLEAN NOT NULL DEFAULT 0,
                    truncation_reason TEXT,
                    ttfb_ms INTEGER
                );
                INSERT INTO usage_records (timestamp, model, provider, endpoint, input_tokens, output_tokens, total_tokens, latency_ms)
                VALUES ('2026-10-01T12:00:00Z', 'legacy-model', 'prov', 'chat', 10, 20, 30, 100);",
            )
            .unwrap();
        }

        // Initialize store on the existing legacy database
        let jsonl_path = temp.path().join("usage.jsonl");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path.clone()).unwrap();

        // 1. Existing data should load cleanly with routed_target == None
        let results = store.query(None, None, None, Some("legacy-model"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].routed_target, None);

        // 2. New column routed_target exists and can be written to
        let new_rec = UsageRecord::new("m2".into(), "p".into(), "e".into(), 1, 1, Some(10))
            .with_routed_target("target-leaf");
        store.record(new_rec).unwrap();
        store.flush_if_dirty().unwrap();

        let results2 = store.query(None, None, None, Some("m2"), None);
        assert_eq!(results2.len(), 1);
        assert_eq!(results2[0].routed_target.as_deref(), Some("target-leaf"));

        // 3. Verify model_binding_revisions table and composite index were created
        let conn = Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='model_binding_revisions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        let idx_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_usage_records_token_ts_model'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx_count, 1);
    }

    #[test]
    fn history_day_bucket_uses_local_calendar_day() {
        let temp = tempdir().unwrap();
        let store = UsageStore::with_paths(
            test_config(),
            temp.path().join("usage.jsonl"),
            temp.path().join("usage.db"),
        )
        .unwrap();

        // 本地今天 00:30 的记录：若按 UTC 分桶/过滤，此刻对应昨天 16:30Z，
        // 会被错归前一天或被“本地今天”的窗口（旧实现起点为 UTC 00:00）滤掉。
        let local_today = chrono::Local::now().date_naive();
        let local_ts = local_today
            .and_hms_opt(0, 30, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .single()
            .expect("00:30 must not be ambiguous");
        let ts = local_ts.with_timezone(&chrono::Utc).to_rfc3339();
        store
            .record(
                UsageRecord::new(
                    "model-a".into(),
                    "p1".into(),
                    "chat".into(),
                    10,
                    5,
                    Some(200),
                )
                .with_timestamp(&ts)
                .with_token(Some("tok_local".into()), Some("T".into())),
            )
            .unwrap();

        // 窗口 = 本地今天 00:00 → 明天 00:00
        let start = local_today
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .single()
            .unwrap()
            .with_timezone(&chrono::Utc);
        let end = start + chrono::Duration::days(1);
        let res = store
            .query_history("tok_local", start, end, HistoryGroupBy::Day)
            .unwrap();
        assert_eq!(
            res.groups.len(),
            1,
            "本地今天的记录必须落在本地今天的窗口内: {:?}",
            res.groups
        );
        assert_eq!(
            res.groups[0].key,
            local_today.format("%Y-%m-%d").to_string()
        );
    }

    #[test]
    fn query_history_group_by_model_and_composite() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        let t1 = "2026-10-01T10:00:00Z";
        let t2 = "2026-10-01T11:00:00Z";
        let t3 = "2026-10-02T10:00:00Z";

        // Record for token A: model1 (100 in, 50 out) at t1, model1 at t2, model2 at t3
        store
            .record(
                UsageRecord::new(
                    "model-alpha".into(),
                    "p1".into(),
                    "chat".into(),
                    100,
                    50,
                    Some(200),
                )
                .with_timestamp(t1)
                .with_token(Some("tok_a".into()), Some("Token A".into())),
            )
            .unwrap();
        store
            .record(
                UsageRecord::new(
                    "model-alpha".into(),
                    "p1".into(),
                    "chat".into(),
                    200,
                    100,
                    Some(200),
                )
                .with_timestamp(t2)
                .with_token(Some("tok_a".into()), Some("Token A".into())),
            )
            .unwrap();
        store
            .record(
                UsageRecord::new(
                    "model-beta".into(),
                    "p2".into(),
                    "chat".into(),
                    300,
                    150,
                    Some(200),
                )
                .with_timestamp(t3)
                .with_token(Some("tok_a".into()), Some("Token A".into())),
            )
            .unwrap();

        // Record for token B (isolation check): model-alpha
        store
            .record(
                UsageRecord::new(
                    "model-alpha".into(),
                    "p1".into(),
                    "chat".into(),
                    1000,
                    500,
                    Some(200),
                )
                .with_timestamp(t1)
                .with_token(Some("tok_b".into()), Some("Token B".into())),
            )
            .unwrap();

        store.flush_if_dirty().unwrap();

        let start = DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-10-03T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        // 1. Group by model for Token A
        let res_model = store
            .query_history("tok_a", start, end, HistoryGroupBy::Model)
            .unwrap();
        assert_eq!(res_model.summary.requests, 3);
        assert_eq!(res_model.summary.input_tokens, 600);
        assert_eq!(res_model.summary.output_tokens, 300);
        assert_eq!(res_model.summary.total_tokens, 900);
        assert_eq!(res_model.groups.len(), 2);
        // Sorted by total_tokens DESC: model-alpha (450) and model-beta (450)
        let alpha_group = res_model
            .groups
            .iter()
            .find(|g| g.key == "model-alpha")
            .unwrap();
        assert_eq!(alpha_group.requests, 2);
        assert_eq!(alpha_group.model.as_deref(), Some("model-alpha"));
        assert_eq!(alpha_group.total_tokens, 450);

        let beta_group = res_model
            .groups
            .iter()
            .find(|g| g.key == "model-beta")
            .unwrap();
        assert_eq!(beta_group.requests, 1);
        assert_eq!(beta_group.model.as_deref(), Some("model-beta"));
        assert_eq!(beta_group.total_tokens, 450);

        // 2. Composite group by Day,Model for Token A
        let res_day_model = store
            .query_history("tok_a", start, end, HistoryGroupBy::DayModel)
            .unwrap();
        assert_eq!(res_day_model.groups.len(), 2);
        let g1 = res_day_model
            .groups
            .iter()
            .find(|g| g.key == "2026-10-01" && g.model.as_deref() == Some("model-alpha"))
            .unwrap();
        assert_eq!(g1.requests, 2);

        let g2 = res_day_model
            .groups
            .iter()
            .find(|g| g.key == "2026-10-02" && g.model.as_deref() == Some("model-beta"))
            .unwrap();
        assert_eq!(g2.requests, 1);

        // 3. Test model_filter on Day grouping for Token A
        let res_filtered = store
            .query_history_with_filter(
                "tok_a",
                start,
                end,
                HistoryGroupBy::Day,
                Some("model-alpha"),
            )
            .unwrap();
        assert_eq!(res_filtered.summary.requests, 2);
        assert_eq!(res_filtered.summary.total_tokens, 450);
        assert_eq!(res_filtered.groups.len(), 1);
        assert_eq!(res_filtered.groups[0].key, "2026-10-01");
        assert_eq!(res_filtered.groups[0].requests, 2);

        // Filter for non-existent model returns 0
        let res_nonexistent = store
            .query_history_with_filter(
                "tok_a",
                start,
                end,
                HistoryGroupBy::Day,
                Some("model-nonexistent"),
            )
            .unwrap();
        assert_eq!(res_nonexistent.summary.requests, 0);
        assert!(res_nonexistent.groups.is_empty());

        // 3. Composite group by Hour,Model for Token A
        let res_hour_model = store
            .query_history("tok_a", start, end, HistoryGroupBy::HourModel)
            .unwrap();
        assert_eq!(res_hour_model.groups.len(), 3);

        // 4. Token B isolation check: token A query should not see Token B's 1500 tokens
        assert_eq!(res_model.summary.total_tokens, 900);
    }

    #[test]
    fn scd_type_2_binding_revisions_sync_and_query_attribution() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        // initial bindings
        let bindings_v1 = vec![
            (
                "gemini-flash".into(),
                "google".into(),
                "gemini-1.5-flash-001".into(),
            ),
            (
                "target-high".into(),
                "google".into(),
                "gemini-exp-1114".into(),
            ),
        ];
        store
            .sync_model_binding_revisions(&bindings_v1, "hash_v1")
            .unwrap();

        let t1 = Utc::now();
        // Record regular model call under v1
        store
            .record(
                UsageRecord::new(
                    "gemini-flash".into(),
                    "google".into(),
                    "chat".into(),
                    10,
                    20,
                    Some(50),
                )
                .with_timestamp(t1.to_rfc3339()),
            )
            .unwrap();
        // Record routed model call under v1 (routed_target points to target-high)
        store
            .record(
                UsageRecord::new(
                    "routed-flash".into(),
                    "google".into(),
                    "chat".into(),
                    30,
                    40,
                    Some(50),
                )
                .with_routed_target("target-high")
                .with_timestamp(t1.to_rfc3339()),
            )
            .unwrap();
        store.flush_if_dirty().unwrap();

        // Brief delay so timestamp strictly advances
        std::thread::sleep(std::time::Duration::from_millis(50));

        // config reloaded with upstream model upgrade
        let bindings_v2 = vec![
            (
                "gemini-flash".into(),
                "google".into(),
                "gemini-2.0-flash-001".into(),
            ),
            (
                "target-high".into(),
                "google".into(),
                "gemini-2.5-pro".into(),
            ),
        ];
        store
            .sync_model_binding_revisions(&bindings_v2, "hash_v2")
            .unwrap();

        let t2 = Utc::now();
        // Record regular model call under v2
        store
            .record(
                UsageRecord::new(
                    "gemini-flash".into(),
                    "google".into(),
                    "chat".into(),
                    50,
                    60,
                    Some(50),
                )
                .with_timestamp(t2.to_rfc3339()),
            )
            .unwrap();
        store.flush_if_dirty().unwrap();

        let start = t1 - chrono::Duration::hours(1);
        let end = t2 + chrono::Duration::hours(1);

        let attribution = store.query_attribution(start, end).unwrap();

        // Assert that historical records did NOT drift!
        // 1. gemini-1.5-flash-001 has 1 request (t1)
        let v1_flash = attribution
            .iter()
            .find(|a| a.upstream_model == "gemini-1.5-flash-001")
            .expect("should find v1 flash attribution");
        assert_eq!(v1_flash.requests, 1);
        assert_eq!(v1_flash.total_tokens, 30);
        assert_eq!(v1_flash.effective_model, "gemini-flash");

        // 2. gemini-exp-1114 has 1 request (routed target from t1)
        let v1_routed = attribution
            .iter()
            .find(|a| a.upstream_model == "gemini-exp-1114")
            .expect("should find v1 routed attribution");
        assert_eq!(v1_routed.requests, 1);
        assert_eq!(v1_routed.total_tokens, 70);
        assert_eq!(v1_routed.effective_model, "target-high");

        // 3. gemini-2.0-flash-001 has 1 request (t2)
        let v2_flash = attribution
            .iter()
            .find(|a| a.upstream_model == "gemini-2.0-flash-001")
            .expect("should find v2 flash attribution");
        assert_eq!(v2_flash.requests, 1);
        assert_eq!(v2_flash.total_tokens, 110);
        assert_eq!(v2_flash.effective_model, "gemini-flash");
    }
}

/// 缓存字段多协议归一锁定：DeepSeek / OpenAI / Anthropic 三种方言 +
/// 无缓存字段（None 向下兼容）。
#[cfg(test)]
mod cache_extract_tests {
    use super::extract_cache_tokens;

    #[test]
    fn normalizes_deepseek_openai_anthropic_cache_fields() {
        // DeepSeek
        let (h, m) = extract_cache_tokens(&serde_json::json!(
            {"prompt_tokens": 100, "prompt_cache_hit_tokens": 60, "prompt_cache_miss_tokens": 40}
        ));
        assert_eq!((h, m), (Some(60), Some(40)));
        // OpenAI (prompt_tokens_details.cached_tokens)
        let (h, m) = extract_cache_tokens(&serde_json::json!(
            {"prompt_tokens": 100, "prompt_tokens_details": {"cached_tokens": 60}}
        ));
        assert_eq!(
            (h, m),
            (Some(60), Some(40)),
            "miss derived from input - hit"
        );
        // Anthropic
        let (h, m) = extract_cache_tokens(&serde_json::json!(
            {"prompt_tokens": 100, "cache_read_input_tokens": 60, "cache_creation_input_tokens": 40}
        ));
        assert_eq!((h, m), (Some(60), Some(40)));
        // 无缓存字段：向下兼容（None 不影响计费）
        let (h, m) = extract_cache_tokens(&serde_json::json!({"prompt_tokens": 100}));
        assert_eq!((h, m), (None, None));
    }
}
