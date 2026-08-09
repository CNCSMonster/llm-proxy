#![allow(dead_code)] // Query methods reserved for Admin API / TUI
//! Token usage statistics tracking.
//!
//! Three-tier storage architecture:
//! 1. Memory cache: recent records for fast queries
//! 2. JSON Lines file: human-readable recent data (2MB threshold)
//! 3. SQLite database: structured historical data (50MB max)

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::config::UsageConfig;
use crate::service::state_dir;

/// Maximum records to keep in memory
const DEFAULT_MEMORY_CACHE_SIZE: usize = 1000;

/// Default SQLite max size in bytes (50MB)
const DEFAULT_DB_MAX_SIZE_BYTES: u64 = 50 * 1024 * 1024;

/// 后台落盘任务的 tick 间隔（§14.3 智能落盘）。
const FLUSH_TICK_SECONDS: u64 = 15;

/// A single usage record
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Request latency in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<i64>,
}

impl UsageRecord {
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
            input_tokens,
            output_tokens,
            total_tokens,
            latency_ms,
        }
    }

    /// Parse timestamp as DateTime
    pub fn parsed_timestamp(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.timestamp)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Get date as NaiveDate
    pub fn date(&self) -> Option<NaiveDate> {
        self.parsed_timestamp().map(|dt| dt.date_naive())
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

/// Thread-safe usage statistics store
///
/// §14 Usage 数据源：**内存为权威数据源，磁盘为持久化备份**。
/// `record` 只写内存 + 置 dirty；后台 `spawn_flush_task` 按请求频率分档
/// 定期把 pending 记录落盘（JSONL + SQLite 迁移）。
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
    /// Configuration
    config: UsageConfig,
    /// Max memory cache size
    max_cache_size: usize,
}

impl UsageStore {
    /// Create a new usage store with default paths
    pub fn new(config: UsageConfig) -> Result<Self> {
        let state_dir = state_dir();
        fs::create_dir_all(&state_dir).context("failed to create state directory")?;

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
            config,
            max_cache_size: DEFAULT_MEMORY_CACHE_SIZE,
        };

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
            config,
            max_cache_size: DEFAULT_MEMORY_CACHE_SIZE,
        };
        store.load_into_memory()?;
        Ok(store)
    }

    /// Record a new usage entry.
    ///
    /// §14.1 内存权威：只写入内存 + pending 队列并置 dirty，**不直接落盘**。
    /// 磁盘同步由后台 `spawn_flush_task` 按频率分档负责。
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

    /// 记录一次请求时间戳并置 dirty（§14.3 频率统计滑动窗口）。
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

    /// 是否有未落盘数据（§14.3 智能落盘状态机）。
    pub fn is_dirty(&self) -> bool {
        *self.dirty.lock().unwrap()
    }

    /// 按最近 60 秒请求频率计算落盘间隔（§14.3 频率分档）：
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

    /// 将 pending 记录落盘（JSONL append + 迁移），成功/无 pending 后清 dirty。
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
            return Ok(());
        }

        for record in &pending {
            self.append_to_jsonl(record)?;
        }
        // 迁移逻辑仍在落盘路径上（阈值判断由文件大小驱动）
        self.check_and_migrate()?;

        *self.dirty.lock().unwrap() = false;
        *self.last_flush.lock().unwrap() = Instant::now();
        Ok(())
    }

    /// 启动后台智能落盘任务（§14.3）：每 15s tick 检查 dirty，
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

    /// Append a record to the JSON Lines file
    fn append_to_jsonl(&self, record: &UsageRecord) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.jsonl_path)
            .context("failed to open usage.jsonl")?;

        let line = serde_json::to_string(record)?;
        writeln!(file, "{}", line)?;
        Ok(())
    }

    /// Check file size and migrate if needed
    fn check_and_migrate(&self) -> Result<()> {
        let file_size = fs::metadata(&self.jsonl_path).map(|m| m.len()).unwrap_or(0);

        let threshold = self.config.file_threshold_bytes();
        if file_size <= threshold {
            return Ok(());
        }

        // Read all records from file
        let records = self.read_jsonl_records()?;
        if records.is_empty() {
            return Ok(());
        }

        // Calculate how many to migrate
        let migrate_count = (records.len() as f64 * self.config.migration_ratio) as usize;
        if migrate_count == 0 {
            return Ok(());
        }

        // Split records
        let to_migrate: Vec<_> = records.iter().take(migrate_count).cloned().collect();
        let to_keep: Vec<_> = records.into_iter().skip(migrate_count).collect();

        // Migrate to SQLite
        self.insert_to_sqlite(&to_migrate)?;

        // Rewrite JSONL file with remaining records
        self.rewrite_jsonl(&to_keep)?;

        // Check SQLite size and cleanup if needed
        self.check_and_cleanup_sqlite()?;

        Ok(())
    }

    /// Read all records from JSON Lines file
    fn read_jsonl_records(&self) -> Result<Vec<UsageRecord>> {
        let file = match fs::File::open(&self.jsonl_path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let reader = io::BufReader::new(file);
        let mut records = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(record) => records.push(record),
                Err(_) => continue, // Skip malformed lines
            }
        }

        Ok(records)
    }

    /// Rewrite JSON Lines file with given records
    fn rewrite_jsonl(&self, records: &[UsageRecord]) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.jsonl_path)?;

        for record in records {
            let line = serde_json::to_string(record)?;
            writeln!(file, "{}", line)?;
        }

        Ok(())
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
                output_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL,
                latency_ms INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_timestamp ON usage_records(timestamp);
            CREATE INDEX IF NOT EXISTS idx_model ON usage_records(model);
            CREATE INDEX IF NOT EXISTS idx_provider ON usage_records(provider);
            CREATE INDEX IF NOT EXISTS idx_endpoint ON usage_records(endpoint);",
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
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO usage_records (timestamp, model, provider, endpoint, input_tokens, output_tokens, total_tokens, latency_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
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
                    record.latency_ms,
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

    /// Load existing data into memory cache
    fn load_into_memory(&self) -> Result<()> {
        let mut all_records = Vec::new();

        // Load from SQLite first (older records)
        if self.db_path.exists()
            && let Ok(conn) = Connection::open(&self.db_path)
        {
            let mut stmt = conn.prepare(
                    "SELECT timestamp, model, provider, endpoint, input_tokens, output_tokens, total_tokens, latency_ms
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
                    latency_ms: row.get(7)?,
                })
            })?;

            for r in records.flatten() {
                all_records.push(r);
            }
        }

        // Load from JSONL (newer records)
        let jsonl_records = self.read_jsonl_records()?;
        all_records.extend(jsonl_records);

        // Sort by timestamp and keep most recent
        all_records.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        // Deduplicate identical records: a crash between the SQLite insert
        // (committed) and the JSONL rewrite leaves the same record in both
        // stores. Identical records are adjacent after the timestamp sort.
        all_records.dedup_by(|a, b| serde_json::to_string(a).ok() == serde_json::to_string(b).ok());
        all_records.truncate(self.max_cache_size);
        all_records.reverse(); // oldest first in deque

        let mut cache = self.memory_cache.lock().unwrap();
        *cache = all_records.into();

        Ok(())
    }

    /// Get all records from memory cache
    pub fn get_all(&self) -> Vec<UsageRecord> {
        self.memory_cache.lock().unwrap().iter().cloned().collect()
    }

    /// Get records filtered by time range
    pub fn get_by_period(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> Vec<UsageRecord> {
        self.memory_cache
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
                    true
                } else {
                    false
                }
            })
            .cloned()
            .collect()
    }

    /// Get records filtered by provider
    pub fn get_by_provider(&self, provider: &str) -> Vec<UsageRecord> {
        self.memory_cache
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.provider == provider)
            .cloned()
            .collect()
    }

    /// Get records filtered by model
    pub fn get_by_model(&self, model: &str) -> Vec<UsageRecord> {
        self.memory_cache
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.model == model)
            .cloned()
            .collect()
    }

    /// Get records filtered by endpoint
    pub fn get_by_endpoint(&self, endpoint: &str) -> Vec<UsageRecord> {
        self.memory_cache
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.endpoint == endpoint)
            .cloned()
            .collect()
    }

    /// Get paths for diagnostics
    pub fn jsonl_path(&self) -> &PathBuf {
        &self.jsonl_path
    }

    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }
}

/// §14.3 落盘决策：距上次落盘超过当前频率分档间隔才真正写盘。
fn flush_due(last_flush: Instant, interval: Duration, now: Instant) -> bool {
    now.duration_since(last_flush) >= interval
}

/// Get the default usage store path
pub fn usage_state_dir() -> PathBuf {
    state_dir()
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

        // Verify all records are in memory (§14.1 内存权威)
        let all = store.get_all();
        assert_eq!(all.len(), 5);

        // §14.3：record 不直接落盘（dirty=true），flush 后才写 JSONL
        assert!(store.is_dirty());
        assert!(
            !jsonl_path.exists(),
            "record should not flush synchronously"
        );
        store.flush_if_dirty().unwrap();
        assert!(!store.is_dirty());
        let content = fs::read_to_string(&jsonl_path).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 5);
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
    fn concurrent_records_are_not_lost_during_migration() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        // Small file threshold forces frequent truncate-rewrite migrations;
        // db limit is generous so SQLite cleanup never deletes records here.
        let config = UsageConfig {
            file_threshold_mb: 0.001,
            migration_ratio: 0.5,
            db_max_size_mb: 1.0,
        };
        let store = UsageStore::with_paths(config, jsonl_path.clone(), db_path.clone()).unwrap();

        // io_lock must keep every record while migrations rewrite JSONL.
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

        // §14.3：flush 后再核对 JSONL + SQLite 总数——迁移把记录拆分到两处而不丢失任何一条。
        store.flush_if_dirty().unwrap();
        let jsonl_lines = fs::read_to_string(&jsonl_path)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        let db_rows: i64 = {
            let conn = Connection::open(&db_path).unwrap();
            conn.query_row("SELECT COUNT(*) FROM usage_records", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(
            jsonl_lines + db_rows as usize,
            200,
            "no records lost across jsonl + sqlite"
        );
    }

    #[test]
    fn load_into_memory_deduplicates_records_present_in_both_stores() {
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("usage.jsonl");
        let db_path = temp.path().join("usage.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path, db_path).unwrap();

        let record = UsageRecord::new(
            "dup-model".to_string(),
            "dup-provider".to_string(),
            "openai_chat".to_string(),
            100,
            50,
            Some(10),
        );

        // Simulate a crash between the SQLite insert (committed) and the JSONL
        // rewrite: the same record ends up in both stores.
        store
            .insert_to_sqlite(std::slice::from_ref(&record))
            .unwrap();
        store.append_to_jsonl(&record).unwrap();

        store.load_into_memory().unwrap();

        let duplicates = store
            .get_all()
            .iter()
            .filter(|r| r.model == "dup-model")
            .count();
        assert_eq!(
            duplicates, 1,
            "record duplicated across stores must be deduplicated"
        );
    }

    #[test]
    fn smart_flush_pacing_frequency_buckets() {
        // §14.3 频率分档：0 → 180s；1-10 → 180s；11-100 → 45s；>100 → 15s
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
        // §14.3：未到间隔不落盘，到达/超过间隔落盘
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
        // §14.3：dirty=false 时 flush 不写盘（零 I/O）
        let temp = tempdir().unwrap();
        let store = UsageStore::with_paths(
            test_config(),
            temp.path().join("u.jsonl"),
            temp.path().join("u.db"),
        )
        .unwrap();
        store.flush_if_dirty().unwrap();
        assert!(
            !store.jsonl_path.exists(),
            "clean store must not touch disk"
        );
        assert!(!store.is_dirty());
    }

    #[test]
    fn flush_persists_only_pending_records() {
        // §14：flush 落盘 pending；再次 record + flush 只追加新记录（无重复）
        let temp = tempdir().unwrap();
        let jsonl_path = temp.path().join("u.jsonl");
        let db_path = temp.path().join("u.db");
        let store = UsageStore::with_paths(test_config(), jsonl_path.clone(), db_path).unwrap();

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
        let after_first = fs::read_to_string(&jsonl_path).unwrap();
        assert_eq!(after_first.lines().count(), 1);

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
        let after_second = fs::read_to_string(&jsonl_path).unwrap();
        assert_eq!(
            after_second.lines().count(),
            2,
            "second flush must append only the new pending record"
        );
    }
}
