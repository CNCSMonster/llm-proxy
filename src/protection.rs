use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{BadRequestProtectionConfig, Protocol};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockStatus {
    pub blocked: bool,
    pub remaining_seconds: u64,
}

#[derive(Debug, Clone)]
struct Entry {
    first_seen_unix: u64,
    last_seen_unix: u64,
    error_count: u32,
    blocked_until_unix: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BadRequestSnapshotEntry {
    pub fingerprint: String,
    pub error_count: u32,
    pub blocked_until_unix: u64,
}

#[derive(Debug)]
pub struct BadRequestManager {
    cfg: BadRequestProtectionConfig,
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl BadRequestManager {
    pub fn new(cfg: BadRequestProtectionConfig) -> Self {
        Self {
            cfg,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    fn enabled(&self) -> bool {
        self.cfg.enabled
            && self.cfg.window_seconds > 0
            && self.cfg.max_errors > 0
            && self.cfg.block_seconds > 0
    }

    pub fn check(&self, fingerprint: &str) -> BlockStatus {
        if !self.enabled() || fingerprint.is_empty() {
            return BlockStatus {
                blocked: false,
                remaining_seconds: 0,
            };
        }
        let now = unix_secs();
        let Ok(mut entries) = self.entries.lock() else {
            return BlockStatus {
                blocked: false,
                remaining_seconds: 0,
            };
        };
        cleanup(&mut entries, now, self.cfg.window_seconds);
        if let Some(entry) = entries.get(fingerprint)
            && entry.blocked_until_unix > now
        {
            return BlockStatus {
                blocked: true,
                remaining_seconds: entry.blocked_until_unix - now,
            };
        }
        BlockStatus {
            blocked: false,
            remaining_seconds: 0,
        }
    }

    pub fn observe_client_error(&self, fingerprint: &str) {
        if !self.enabled() || fingerprint.is_empty() {
            return;
        }
        let now = unix_secs();
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        cleanup(&mut entries, now, self.cfg.window_seconds);
        let entry = entries.entry(fingerprint.to_string()).or_insert(Entry {
            first_seen_unix: now,
            last_seen_unix: now,
            error_count: 0,
            blocked_until_unix: 0,
        });
        if now.saturating_sub(entry.first_seen_unix) > self.cfg.window_seconds {
            entry.first_seen_unix = now;
            entry.error_count = 0;
        }
        entry.last_seen_unix = now;
        entry.error_count += 1;
        if entry.error_count >= self.cfg.max_errors {
            entry.blocked_until_unix = now + self.cfg.block_seconds;
        }
    }

    pub fn snapshot(&self) -> Vec<BadRequestSnapshotEntry> {
        let now = unix_secs();
        let Ok(mut entries) = self.entries.lock() else {
            return Vec::new();
        };
        cleanup(&mut entries, now, self.cfg.window_seconds);
        entries
            .iter()
            .map(|(fingerprint, entry)| BadRequestSnapshotEntry {
                fingerprint: fingerprint.clone(),
                error_count: entry.error_count,
                blocked_until_unix: entry.blocked_until_unix,
            })
            .collect()
    }
}

pub fn fingerprint(protocol: Protocol, model: Option<&str>, body: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(protocol.route_key().as_bytes());
    hasher.update(b"\n");
    hasher.update(model.unwrap_or_default().as_bytes());
    hasher.update(b"\n");
    hasher.update(body.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

fn cleanup(entries: &mut BTreeMap<String, Entry>, now: u64, window_seconds: u64) {
    entries.retain(|_, entry| {
        entry.blocked_until_unix > now || now.saturating_sub(entry.last_seen_unix) <= window_seconds
    });
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_configured_error_threshold() {
        let manager = BadRequestManager::new(BadRequestProtectionConfig {
            enabled: true,
            window_seconds: 60,
            max_errors: 2,
            block_seconds: 120,
        });
        assert!(!manager.check("fp").blocked);
        manager.observe_client_error("fp");
        assert!(!manager.check("fp").blocked);
        manager.observe_client_error("fp");
        assert!(manager.check("fp").blocked);
    }
}
