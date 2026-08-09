use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{CooldownKey, Protocol};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CooldownEntry {
    pub model: String,
    pub provider: String,
    pub protocol: Protocol,
    pub kind: String,
    pub reason: String,
    pub started_at_unix: u64,
    pub expires_at_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CooldownState {
    version: u32,
    #[serde(default)]
    entries: Vec<CooldownEntry>,
}

#[derive(Debug)]
pub struct CooldownStore {
    path: Option<PathBuf>,
    entries: Mutex<BTreeMap<String, CooldownEntry>>,
}

impl CooldownStore {
    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn load_default() -> Self {
        Self::load(default_state_path()).unwrap_or_else(|_| Self::in_memory())
    }

    pub fn load(path: PathBuf) -> Result<Self> {
        let mut map = BTreeMap::new();
        for entry in read_entries(&path)? {
            map.insert(
                entry_key(&entry.model, &entry.provider, entry.protocol),
                entry,
            );
        }
        Ok(Self {
            path: Some(path),
            entries: Mutex::new(map),
        })
    }

    pub fn is_cooling_down(&self, key: &CooldownKey) -> bool {
        let now = unix_secs();
        let stable = key.stable_id();
        self.entries
            .lock()
            .map(|entries| {
                entries
                    .get(&stable)
                    .is_some_and(|entry| entry.expires_at_unix > now)
            })
            .unwrap_or(false)
    }

    pub fn set(&self, key: &CooldownKey, kind: &str, reason: &str, duration: Duration) {
        if duration.is_zero() {
            return;
        }
        let now = unix_secs();
        let entry = CooldownEntry {
            model: key.model_id.clone(),
            provider: key.provider_id.clone(),
            protocol: key.protocol,
            kind: kind.to_string(),
            reason: reason.to_string(),
            started_at_unix: now,
            expires_at_unix: now + duration.as_secs(),
        };
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(key.stable_id(), entry);
            let _ = self.persist_locked(&entries);
        }
    }

    fn persist_locked(&self, entries: &BTreeMap<String, CooldownEntry>) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        write_entries(path, entries.values().cloned().collect())
    }

    /// 清除匹配的冷却条目（server 内存级，供 delegation 的 clear 委托使用），
    /// 同步落盘。返回移除数量。
    pub fn clear_entries(&self, model: Option<&str>, provider: &str) -> usize {
        let mut entries = match self.entries.lock() {
            Ok(e) => e,
            Err(_) => return 0,
        };
        let before = entries.len();
        entries.retain(|_, entry| {
            if entry.provider != provider {
                return true;
            }
            if let Some(model) = model {
                entry.model != model
            } else {
                false
            }
        });
        let removed = before - entries.len();
        if removed > 0 {
            let _ = self.persist_locked(&entries);
        }
        removed
    }
}

pub fn default_state_path() -> PathBuf {
    crate::service::state_dir().join("cooldowns.json")
}

pub fn read_entries(path: &Path) -> Result<Vec<CooldownEntry>> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let state: CooldownState = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let now = unix_secs();
    Ok(state
        .entries
        .into_iter()
        .filter(|entry| entry.expires_at_unix > now)
        .collect())
}

pub fn clear(path: &Path, model: Option<&str>, provider: &str) -> Result<usize> {
    let entries = read_entries(path)?;
    let before = entries.len();
    let kept: Vec<_> = entries
        .into_iter()
        .filter(|entry| {
            if entry.provider != provider {
                return true;
            }
            if let Some(model) = model {
                entry.model != model
            } else {
                false
            }
        })
        .collect();
    let removed = before - kept.len();
    if removed > 0 {
        write_entries(path, kept)?;
    }
    Ok(removed)
}

pub fn write_entries(path: &Path, entries: Vec<CooldownEntry>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let state = CooldownState {
        version: 1,
        entries,
    };
    let data =
        serde_json::to_string_pretty(&state).context("failed to serialize cooldown state")? + "\n";
    fs::write(path, data).with_context(|| format!("failed to write {}", path.display()))
}

pub fn print_list(path: &Path) -> Result<()> {
    let entries = read_entries(path)?;
    if entries.is_empty() {
        println!("No active cooldowns");
        return Ok(());
    }
    for entry in entries {
        println!(
            "model={} provider={} protocol={} kind={} expires_at_unix={} reason={}",
            entry.model,
            entry.provider,
            entry.protocol.route_key(),
            entry.kind,
            entry.expires_at_unix,
            entry.reason
        );
    }
    Ok(())
}

fn entry_key(model: &str, provider: &str, protocol: Protocol) -> String {
    format!("{model}:{provider}:{}", protocol.route_key())
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(model: &str, provider: &str, protocol: Protocol) -> CooldownKey {
        CooldownKey {
            model_id: model.into(),
            provider_id: provider.into(),
            protocol,
        }
    }

    fn active_entry(model: &str, provider: &str, protocol: Protocol) -> CooldownEntry {
        CooldownEntry {
            model: model.into(),
            provider: provider.into(),
            protocol,
            kind: "server_error".into(),
            reason: "500".into(),
            started_at_unix: unix_secs().saturating_sub(1),
            expires_at_unix: unix_secs() + 60,
        }
    }

    #[test]
    fn in_memory_set_marks_key_cooling_down() {
        let store = CooldownStore::in_memory();
        let key = key("m1", "p1", Protocol::OpenaiChatCompletions);

        assert!(!store.is_cooling_down(&key));
        store.set(&key, "rate_limit", "429", Duration::from_secs(30));
        assert!(store.is_cooling_down(&key));
    }

    #[test]
    fn zero_duration_set_does_not_create_cooldown() {
        let store = CooldownStore::in_memory();
        let key = key("m1", "p1", Protocol::OpenaiResponses);

        store.set(&key, "server_error", "500", Duration::ZERO);

        assert!(!store.is_cooling_down(&key));
        assert!(store.entries.lock().expect("lock").is_empty());
    }

    #[test]
    fn cooldown_key_includes_protocol() {
        let store = CooldownStore::in_memory();
        let chat_key = key("m1", "p1", Protocol::OpenaiChatCompletions);
        let responses_key = key("m1", "p1", Protocol::OpenaiResponses);

        store.set(&chat_key, "network", "timeout", Duration::from_secs(30));

        assert!(store.is_cooling_down(&chat_key));
        assert!(!store.is_cooling_down(&responses_key));
    }

    #[test]
    fn expired_entries_are_ignored_when_loaded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");
        let now = unix_secs();
        write_entries(
            &path,
            vec![CooldownEntry {
                model: "m1".into(),
                provider: "p1".into(),
                protocol: Protocol::Anthropic,
                kind: "rate_limit".into(),
                reason: "old 429".into(),
                started_at_unix: now.saturating_sub(120),
                expires_at_unix: now.saturating_sub(1),
            }],
        )
        .expect("write");

        assert!(read_entries(&path).expect("read").is_empty());
    }

    #[test]
    fn read_missing_state_file_returns_empty_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("missing").join("cooldowns.json");

        assert!(read_entries(&path).expect("read missing").is_empty());
    }

    #[test]
    fn load_persists_only_active_entries_and_matches_cooldown_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");
        write_entries(&path, vec![active_entry("m1", "p1", Protocol::Antigravity)]).expect("write");

        let store = CooldownStore::load(path).expect("load");

        assert!(store.is_cooling_down(&key("m1", "p1", Protocol::Antigravity)));
        assert!(!store.is_cooling_down(&key("m1", "p2", Protocol::Antigravity)));
    }

    #[test]
    fn clear_entries_by_provider_removes_all_matching_models_and_persists() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");
        write_entries(
            &path,
            vec![
                active_entry("m1", "p1", Protocol::OpenaiResponses),
                active_entry("m2", "p1", Protocol::OpenaiResponses),
                active_entry("m3", "p2", Protocol::OpenaiResponses),
            ],
        )
        .expect("write");
        let store = CooldownStore::load(path.clone()).expect("load");

        assert_eq!(store.clear_entries(None, "p1"), 2);

        let entries = read_entries(&path).expect("read");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].provider, "p2");
    }

    #[test]
    fn clear_entries_no_match_does_not_change_state() {
        let store = CooldownStore::in_memory();
        let key = key("m1", "p1", Protocol::OpenaiResponses);
        store.set(&key, "client_error", "400", Duration::from_secs(30));

        assert_eq!(store.clear_entries(Some("m1"), "missing-provider"), 0);
        assert!(store.is_cooling_down(&key));
    }

    #[test]
    fn clear_by_provider_and_model_updates_state_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");
        write_entries(
            &path,
            vec![
                CooldownEntry {
                    model: "m1".into(),
                    provider: "p1".into(),
                    protocol: Protocol::OpenaiResponses,
                    kind: "server_error".into(),
                    reason: "500".into(),
                    started_at_unix: 1,
                    expires_at_unix: unix_secs() + 60,
                },
                CooldownEntry {
                    model: "m2".into(),
                    provider: "p1".into(),
                    protocol: Protocol::OpenaiResponses,
                    kind: "server_error".into(),
                    reason: "500".into(),
                    started_at_unix: 1,
                    expires_at_unix: unix_secs() + 60,
                },
            ],
        )
        .expect("write");

        assert_eq!(clear(&path, Some("m1"), "p1").expect("clear"), 1);
        let entries = read_entries(&path).expect("read");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model, "m2");
    }

    // =========================================================================
    // Phase 1 P0: cooldown.rs file persistence tests
    // =========================================================================

    #[test]
    fn test_set_persists_to_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");
        // Create a file-backed store
        let store = CooldownStore::load(path.clone()).expect("load");
        let key = key("model-a", "provider-b", Protocol::OpenaiChatCompletions);

        // set() should persist to the file
        store.set(&key, "rate_limit", "429", Duration::from_secs(120));

        // Read back from disk independently
        let disk_entries = read_entries(&path).expect("read disk");
        assert_eq!(disk_entries.len(), 1);
        assert_eq!(disk_entries[0].model, "model-a");
        assert_eq!(disk_entries[0].provider, "provider-b");
        assert_eq!(disk_entries[0].kind, "rate_limit");
        assert_eq!(disk_entries[0].reason, "429");
    }

    #[test]
    fn test_read_entries_corrupted_file_returns_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");
        // Write invalid JSON to the file
        fs::write(&path, "{broken json content").expect("write corrupt");
        // read_entries should return an error (not silently empty)
        let result = read_entries(&path);
        assert!(result.is_err(), "corrupted file should produce error");
        // But load_default() should fall back to in-memory (no panic)
        // We test the error path here since load_default uses a different path
    }

    #[test]
    fn test_write_entries_creates_parent_dirs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nested_path = temp
            .path()
            .join("a")
            .join("b")
            .join("c")
            .join("cooldowns.json");
        // Parent directories don't exist yet
        assert!(!nested_path.parent().unwrap().exists());

        let entries = vec![CooldownEntry {
            model: "m".into(),
            provider: "p".into(),
            protocol: Protocol::Anthropic,
            kind: "network".into(),
            reason: "timeout".into(),
            started_at_unix: unix_secs(),
            expires_at_unix: unix_secs() + 60,
        }];
        write_entries(&nested_path, entries).expect("write should create parent dirs");

        // Verify file was created and readable
        assert!(nested_path.exists());
        let loaded = read_entries(&nested_path).expect("read");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].model, "m");
    }
}
