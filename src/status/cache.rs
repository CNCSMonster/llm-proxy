use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Probe result enum for Singleflight coordination (§19.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// Provider is active (has successful requests within TTL)
    Active,
    /// Probe succeeded
    Ok { latency_ms: u64, status: u16 },
    /// Probe timed out
    Timeout,
    /// Probe failed with error
    Error(String),
}

impl ProbeResult {
    /// Convert to ProbeCacheEntry for cache storage.
    pub fn to_cache_entry(&self) -> Option<ProbeCacheEntry> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        match self {
            ProbeResult::Active => None, // Active providers don't need cache entry
            ProbeResult::Ok { latency_ms, status } => Some(ProbeCacheEntry {
                ok: true,
                checked_at_unix: now,
                latency_ms: Some(*latency_ms),
                http_status: Some(*status),
                error: None,
            }),
            ProbeResult::Timeout => Some(ProbeCacheEntry {
                ok: false,
                checked_at_unix: now,
                latency_ms: None,
                http_status: None,
                error: Some("probe timeout".into()),
            }),
            ProbeResult::Error(msg) => Some(ProbeCacheEntry {
                ok: false,
                checked_at_unix: now,
                latency_ms: None,
                http_status: None,
                error: Some(msg.clone()),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StatusCache {
    #[serde(default)]
    pub probes: BTreeMap<String, ProbeCacheEntry>,
    #[serde(default)]
    pub dynamic_models: BTreeMap<String, Vec<DynamicModelCacheEntry>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DynamicModelCacheEntry {
    pub provider_id: String,
    pub source_url: String,
    pub probed_at_unix: u64,
    pub stale_after_unix: u64,
    pub model_id: String,
    pub display_name: Option<String>,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub features: Vec<String>,
    pub supported_parameters: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_reasoning_levels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_level: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProbeCacheEntry {
    pub ok: bool,
    pub checked_at_unix: u64,
    pub latency_ms: Option<u64>,
    pub http_status: Option<u16>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct ProbeFrequencyLimiter {
    next_allowed: BTreeMap<String, Instant>,
}

impl ProbeFrequencyLimiter {
    pub(crate) async fn acquire(&mut self, plan: &crate::config::ExecutionPlan) -> bool {
        let provider_id = plan.provider_id.clone();
        let rpm = plan
            .request_frequency
            .requests_per_minute
            .unwrap_or(60)
            .max(1);
        let interval = Duration::from_secs_f64(60.0 / rpm as f64);
        let now = Instant::now();
        let allowed_at = self.next_allowed.get(&provider_id).copied().unwrap_or(now);
        let wait = allowed_at.saturating_duration_since(now);
        let queue_timeout =
            Duration::from_secs(plan.request_frequency.queue_timeout_seconds.unwrap_or(10));
        if wait > queue_timeout {
            return false;
        }
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        self.next_allowed
            .insert(provider_id, Instant::now() + interval);
        true
    }
}

pub fn read_cache(path: &Path) -> Result<StatusCache> {
    let data =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn write_cache(path: &Path, cache: &StatusCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let data =
        serde_json::to_string_pretty(cache).context("failed to serialize status cache")? + "\n";
    crate::config_edit::atomic_write(path, data.as_bytes())
}

pub fn cache_path() -> PathBuf {
    // 测试/隔离场景可通过 LLM_PROXY_STATE_DIR 覆盖 state 目录，
    // 避免测试读写与运行中 server 共享的 status-cache.json。
    let dir = std::env::var("LLM_PROXY_STATE_DIR")
        .map(PathBuf::from)
        .ok()
        .or_else(dirs::state_dir)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join("llm-proxy").join("status-cache.json")
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
