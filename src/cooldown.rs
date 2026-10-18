use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{CooldownKey, Protocol};

/// Manual `cooldown set` may last longer than automatic 24h caps.
pub const MANUAL_COOLDOWN_MAX: Duration = Duration::from_secs(30 * 24 * 60 * 60);
pub const DEFAULT_MANUAL_KIND: &str = "manual";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CooldownEntry {
    pub model: String,
    pub provider: String,
    pub protocol: Protocol,
    /// §3.1: token identity for (token × provider) 400 cooldowns.
    /// Absent (`None`) = provider-global entry; old state files without this
    /// key load as provider-global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Immutable token key hash for matching across renames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_key_hash: Option<String>,
    pub kind: String,
    pub reason: String,
    pub started_at_unix: u64,
    pub expires_at_unix: u64,
    /// Number of times incoming requests hit and were blocked/deflected by this cooldown.
    #[serde(default)]
    pub hit_count: u64,
    /// Timestamp (unix seconds) of the most recent hit, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_hit_unix: Option<u64>,
}

impl CooldownEntry {
    pub fn stable_id(&self) -> String {
        crate::config::cooldown_stable_id(
            &self.model,
            &self.provider,
            self.protocol,
            self.token.as_deref(),
            self.token_key_hash.as_deref(),
        )
    }

    /// Dual-path matching against a lookup key.
    ///
    /// - Matches model, provider, and protocol.
    /// - For provider-global keys (`key.token.is_none() && key.token_key_hash.is_none()`):
    ///   matches provider-global entries (`entry.token.is_none() && entry.token_key_hash.is_none()`).
    /// - For token-scoped keys:
    ///   * Path 1: if both have `token_key_hash`, hashes must match.
    ///   * Path 2: if either lacks `token_key_hash` (e.g. legacy entry), names must match.
    pub fn matches_key(&self, key: &CooldownKey) -> bool {
        if self.model != key.model_id
            || self.provider != key.provider_id
            || self.protocol != key.protocol
        {
            return false;
        }

        match (&key.token, &key.token_key_hash) {
            (None, None) => {
                // Provider-global key: only matches provider-global entry
                self.token.is_none() && self.token_key_hash.is_none()
            }
            _ => {
                // Token-scoped key: entry must also be token-scoped
                if self.token.is_none() && self.token_key_hash.is_none() {
                    return false;
                }
                // Path 1: if both have token_key_hash, they must match
                if let (Some(new_hash), Some(old_hash)) =
                    (&key.token_key_hash, &self.token_key_hash)
                {
                    return new_hash == old_hash;
                }
                // Path 2: fallback to token name match (legacy compatibility when at least one lacks hash)
                if let (Some(new_name), Some(old_name)) = (&key.token, &self.token) {
                    return new_name == old_name;
                }
                false
            }
        }
    }
}

/// : provider-level wildcard entry (`model == "*"`) applies to any
/// provider-global lookup key of the same provider & protocol. Token-scoped
/// keys deliberately do NOT match: a wildcard is provider health, not
/// requester attribution, and routing already checks the provider-global key
/// for every candidate regardless of which token is asking.
fn provider_wildcard_matches(entry: &CooldownEntry, key: &CooldownKey) -> bool {
    entry.model == "*"
        && entry.provider == key.provider_id
        && entry.protocol == key.protocol
        && entry.token.is_none()
        && entry.token_key_hash.is_none()
        && key.token.is_none()
        && key.token_key_hash.is_none()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CooldownState {
    version: u32,
    #[serde(default)]
    entries: Vec<CooldownEntry>,
    /// : per-provider bad request counts (upstream 400s).
    /// Key is provider_id, value is the count of recent bad requests.
    #[serde(default)]
    bad_request_counts: BTreeMap<String, u32>,
}

#[derive(Debug)]
pub struct CooldownStore {
    path: Option<PathBuf>,
    entries: Mutex<BTreeMap<String, CooldownEntry>>,
    /// : per-provider bad request counts for tolerance tracking.
    bad_request_counts: Mutex<BTreeMap<String, u32>>,
}

impl CooldownStore {
    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: Mutex::new(BTreeMap::new()),
            bad_request_counts: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn load(path: PathBuf) -> Result<Self> {
        let state = read_state(&path)?;
        let mut map = BTreeMap::new();
        for entry in state.entries {
            map.insert(entry.stable_id(), entry);
        }
        Ok(Self {
            path: Some(path),
            entries: Mutex::new(map),
            bad_request_counts: Mutex::new(state.bad_request_counts),
        })
    }

    /// Pure state query — does NOT touch `hit_count`. Use this for routing
    /// decisions and multi-key checks (a request may check one entry via
    /// several key shapes; each must not inflate the counter).
    pub fn is_cooling_down(&self, key: &CooldownKey) -> bool {
        self.matches_active(key)
    }

    /// Pure query variant of `check_and_record_hit`: same three-way matching
    /// (stable_id, key match, provider wildcard) without touching `hit_count`
    /// or persisting.
    fn matches_active(&self, key: &CooldownKey) -> bool {
        let now = unix_secs();
        let entries = match self.entries.lock() {
            Ok(e) => e,
            Err(_) => return false,
        };
        let stable = key.stable_id();
        entries
            .get(&stable)
            .filter(|e| e.expires_at_unix > now)
            .is_some()
            || entries
                .values()
                .any(|e| e.expires_at_unix > now && e.matches_key(key))
            || entries
                .values()
                .any(|e| e.expires_at_unix > now && provider_wildcard_matches(e, key))
    }

    /// Explicit hit counter for a real interception event (candidate skipped
    /// because the entry is cooling). One record per interception keeps
    /// `hit_count` semantic equal to "requests turned away", not "times the
    /// status was queried".
    pub fn record_cooldown_hit(&self, key: &CooldownKey) {
        self.check_and_record_hit(key);
    }

    /// Checks if a key is currently cooling down. If active, records a hit
    /// (increments `hit_count` and updates `last_hit_unix`) and persists to disk.
    pub fn check_and_record_hit(&self, key: &CooldownKey) -> bool {
        let now = unix_secs();
        let mut entries = match self.entries.lock() {
            Ok(e) => e,
            Err(_) => return false,
        };

        // Fast path 1: direct stable_id match
        let stable = key.stable_id();
        if let Some(entry) = entries.get_mut(&stable).filter(|e| e.expires_at_unix > now) {
            entry.hit_count = entry.hit_count.saturating_add(1);
            entry.last_hit_unix = Some(now);
            let _ = self.persist_locked(&entries);
            return true;
        }

        // Dual-path fallback: search for an unexpired entry matching the key
        if let Some((_, entry)) = entries
            .iter_mut()
            .find(|(_, entry)| entry.expires_at_unix > now && entry.matches_key(key))
        {
            entry.hit_count = entry.hit_count.saturating_add(1);
            entry.last_hit_unix = Some(now);
            let _ = self.persist_locked(&entries);
            return true;
        }

        // : provider-level wildcard entry (model "*") blocks every
        // concrete (model, provider) key of that provider.
        if let Some((_, entry)) = entries
            .iter_mut()
            .find(|(_, entry)| entry.expires_at_unix > now && provider_wildcard_matches(entry, key))
        {
            entry.hit_count = entry.hit_count.saturating_add(1);
            entry.last_hit_unix = Some(now);
            let _ = self.persist_locked(&entries);
            return true;
        }

        false
    }

    pub fn active_entry(&self, key: &CooldownKey) -> Option<CooldownEntry> {
        let now = unix_secs();
        let entries = self.entries.lock().ok()?;

        // Fast path 1: direct stable_id match
        let stable = key.stable_id();
        if let Some(entry) = entries
            .get(&stable)
            .filter(|entry| entry.expires_at_unix > now)
        {
            return Some(entry.clone());
        }

        // Dual-path fallback: search for an unexpired entry matching the key
        entries
            .values()
            .find(|entry| {
                entry.expires_at_unix > now
                    && (entry.matches_key(key) || provider_wildcard_matches(entry, key))
            })
            .cloned()
    }

    /// : Increment the bad request count for a provider.
    /// Returns the new count after incrementing.
    pub fn increment_bad_request_count(&self, provider_id: &str) -> u32 {
        // Scope the counts guard so it is released before persist_locked
        // re-locks the same mutex — holding it across persist deadlocks.
        let new_count = {
            let mut counts = match self.bad_request_counts.lock() {
                Ok(c) => c,
                Err(_) => return 0,
            };
            let count = counts.entry(provider_id.to_string()).or_insert(0);
            *count += 1;
            *count
        };
        let entries = self.entries.lock().map(|e| e.clone()).unwrap_or_default();
        let _ = self.persist_locked(&entries);
        new_count
    }

    /// : Get the current bad request count for a provider.
    /// Production only increments/resets; reads happen in red-proof tests.
    #[allow(dead_code)]
    pub fn get_bad_request_count(&self, provider_id: &str) -> u32 {
        let counts = match self.bad_request_counts.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        counts.get(provider_id).copied().unwrap_or(0)
    }

    /// : Reset the bad request count for a provider (e.g., after cooldown is triggered).
    pub fn reset_bad_request_count(&self, provider_id: &str) {
        {
            let mut counts = match self.bad_request_counts.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            counts.remove(provider_id);
        }
        let entries = self.entries.lock().map(|e| e.clone()).unwrap_or_default();
        let _ = self.persist_locked(&entries);
    }

    /// Finds an active cooldown entry for a specific model binding (model + provider + protocol).
    ///
    /// Prefers provider-global cooldown entry if active; otherwise returns any active entry
    /// matching the binding with the latest expiration time.
    pub fn active_binding_cooldown(
        &self,
        model_id: &str,
        provider_id: &str,
        protocol: Protocol,
    ) -> Option<CooldownEntry> {
        let now = unix_secs();
        let entries = self.entries.lock().ok()?;
        let matching = entries.values().filter(|e| {
            e.expires_at_unix > now
                && e.provider == provider_id
                && e.protocol == protocol
 // : provider-level wildcard ("*") covers every model.
                && (e.model == model_id || e.model == "*")
        });
        let mut best: Option<CooldownEntry> = None;
        for entry in matching {
            if entry.token.is_none() && entry.token_key_hash.is_none() {
                return Some(entry.clone());
            }
            if best
                .as_ref()
                .map(|b| entry.expires_at_unix > b.expires_at_unix)
                .unwrap_or(true)
            {
                best = Some(entry.clone());
            }
        }
        best
    }

    /// Finds any active cooldown entry for a provider across all its models and protocols.
    ///
    /// Returns the entry with the latest expiration time, if any.
    #[allow(dead_code)]
    pub fn active_provider_cooldown(&self, provider_id: &str) -> Option<CooldownEntry> {
        let now = unix_secs();
        let entries = self.entries.lock().ok()?;
        entries
            .values()
            .filter(|e| e.expires_at_unix > now && e.provider == provider_id)
            .max_by_key(|e| e.expires_at_unix)
            .cloned()
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
            token: key.token.clone(),
            token_key_hash: key.token_key_hash.clone(),
            kind: kind.to_string(),
            reason: reason.to_string(),
            started_at_unix: now,
            expires_at_unix: now + duration.as_secs(),
            hit_count: 0,
            last_hit_unix: None,
        };
        if let Ok(mut entries) = self.entries.lock() {
            // Remove any existing entry matching this key (e.g. under an older stable_id before rename or legacy format)
            let old_keys: Vec<String> = entries
                .iter()
                .filter(|(_, e)| e.matches_key(key))
                .map(|(k, _)| k.clone())
                .collect();
            for old_k in old_keys {
                entries.remove(&old_k);
            }

            entries.insert(key.stable_id(), entry);
            let _ = self.persist_locked(&entries);
        }
    }

    /// Reconcile in-memory (and on-disk) cooldown entries against active tokens.
    ///
    /// - For entries with `token_key_hash`: update `token` (display name) to match current name in `tokens.toml`.
    /// - For legacy entries with `token` (name) but no `token_key_hash`: populate `token_key_hash` if a token with that name exists.
    /// - Re-indexes entries in the BTreeMap if stable_id changes.
    /// - Returns true if any entry was modified.
    pub fn reconcile(&self, tokens: &crate::token::config::TokensConfig) -> bool {
        let mut entries = match self.entries.lock() {
            Ok(e) => e,
            Err(_) => return false,
        };

        let mut hash_to_name = std::collections::HashMap::new();
        let mut name_to_hash = std::collections::HashMap::new();
        for t in &tokens.tokens {
            let hash = t.key_hash();
            hash_to_name.insert(hash.clone(), t.name.clone());
            name_to_hash.insert(t.name.clone(), hash);
        }

        let mut changed = false;
        let old_values: Vec<CooldownEntry> = entries.values().cloned().collect();
        let mut new_entries = BTreeMap::new();

        for mut entry in old_values {
            let mut entry_changed = false;
            if let Some(hash) = &entry.token_key_hash {
                if let Some(current_name) = hash_to_name.get(hash)
                    && entry.token.as_deref() != Some(current_name)
                {
                    entry.token = Some(current_name.clone());
                    entry_changed = true;
                }
            } else if let Some(name) = &entry.token
                && let Some(hash) = name_to_hash.get(name)
            {
                entry.token_key_hash = Some(hash.clone());
                entry_changed = true;
            }

            if entry_changed {
                changed = true;
            }
            new_entries.insert(entry.stable_id(), entry);
        }

        if changed {
            *entries = new_entries;
            let _ = self.persist_locked(&entries);
        }
        changed
    }

    fn persist_locked(&self, entries: &BTreeMap<String, CooldownEntry>) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let bad_request_counts = self
            .bad_request_counts
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default();
        write_state(
            path,
            entries.values().cloned().collect(),
            bad_request_counts,
        )
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

    /// Validate all active cooldown entries against loaded config.
    /// Emits WARN logs for entries referencing unknown models or providers, or models that do not bind the provider.
    pub fn validate_against_config(&self, cfg: &crate::config::Config) -> Vec<String> {
        let now = unix_secs();
        let entries = match self.entries.lock() {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        let mut warnings = Vec::new();
        for entry in entries.values() {
            if entry.expires_at_unix <= now {
                continue;
            }
            if let Some(zombie_reason) = check_entry_zombie(entry, cfg) {
                let msg = format!(
                    "cooldown entry for ({}, {}) references {zombie_reason}",
                    entry.provider, entry.model
                );
                tracing::warn!("{msg}");
                warnings.push(msg);
            }
        }
        warnings
    }
}

/// Returns None if the cooldown entry refers to a valid provider and model binding in `cfg`,
/// or Some(reason) if it is a zombie entry.
pub fn check_entry_zombie(entry: &CooldownEntry, cfg: &crate::config::Config) -> Option<String> {
    if !cfg.providers.contains_key(&entry.provider) {
        return Some(format!("unknown provider '{}'", entry.provider));
    }
    // : provider-level wildcard entry — only the provider matters.
    if entry.model == "*" {
        return None;
    }
    match cfg.models.get(&entry.model) {
        None => Some(format!("unknown model '{}'", entry.model)),
        Some(model_cfg) => {
            if !model_cfg.references_provider(&entry.provider) {
                Some(format!(
                    "model '{}' which does not bind provider '{}'",
                    entry.model, entry.provider
                ))
            } else {
                None
            }
        }
    }
}

/// Default-instance cooldown state path.
///
/// # 分类标注
///
/// 全局便捷函数——只可用于 CLI 单进程工具（如 `model info` 命令）。
/// Server 路径必须用 `state_dirs.state_dir.join("cooldowns.json")`。
pub fn default_state_path() -> PathBuf {
    crate::service::state_dir().join("cooldowns.json")
}

pub fn read_entries(path: &Path) -> Result<Vec<CooldownEntry>> {
    let state = read_state(path)?;
    let now = unix_secs();
    Ok(state
        .entries
        .into_iter()
        .filter(|entry| entry.expires_at_unix > now)
        .collect())
}

/// : Read the full cooldown state including bad request counts.
fn read_state(path: &Path) -> Result<CooldownState> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CooldownState::default());
        }
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
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

/// Parse a human-readable cooldown duration (`42h`, `1h30m`, `30m`, `7d`, `151200s`).
pub fn parse_manual_duration(input: &str) -> Result<Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("duration must not be empty");
    }
    let duration = parse_compound_duration(trimmed).with_context(|| {
        format!("invalid duration {input:?}; expected forms like 42h, 1h30m, 30m, 7d")
    })?;
    if duration.is_zero() {
        bail!("duration must be greater than 0");
    }
    if duration > MANUAL_COOLDOWN_MAX {
        bail!("duration must be at most 30d");
    }
    Ok(duration)
}

fn parse_compound_duration(input: &str) -> Result<Duration> {
    let mut total = Duration::ZERO;
    let mut number = String::new();
    let mut saw_unit = false;
    for ch in input.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        if number.is_empty() {
            bail!("duration segment is missing a number before {ch:?}");
        }
        let value: u64 = number.parse().context("duration number overflow")?;
        number.clear();
        saw_unit = true;
        total = total
            .checked_add(unit_duration(ch, value)?)
            .context("duration overflow")?;
    }
    if !number.is_empty() {
        bail!("duration number is missing a unit");
    }
    if !saw_unit {
        bail!("duration is missing a unit");
    }
    Ok(total)
}

fn unit_duration(unit: char, value: u64) -> Result<Duration> {
    let seconds = match unit {
        's' => value,
        'm' => value.checked_mul(60).context("duration overflow")?,
        'h' => value.checked_mul(60 * 60).context("duration overflow")?,
        'd' => value
            .checked_mul(24 * 60 * 60)
            .context("duration overflow")?,
        other => bail!("unsupported duration unit {other:?}; expected s, m, h, or d"),
    };
    Ok(Duration::from_secs(seconds))
}

pub fn parse_cooldown_protocol(value: &str) -> Result<Protocol> {
    match value {
        "openai-chat"
        | "openai-chat-completions"
        | "chat"
        | "chat-completions"
        | "chat_completions" => Ok(Protocol::OpenaiChatCompletions),
        "openai-responses" | "responses" => Ok(Protocol::OpenaiResponses),
        "anthropic" | "anthropic-messages" => Ok(Protocol::Anthropic),
        other => bail!(
            "unsupported --protocol {other:?}; expected openai-chat, openai-responses, or anthropic"
        ),
    }
}

pub fn protocols_for_cli(protocol: Option<&str>) -> Result<Vec<Protocol>> {
    match protocol {
        Some(value) => Ok(vec![parse_cooldown_protocol(value)?]),
        None => Ok(Protocol::CLIENT_PROTOCOLS.to_vec()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownSetResult {
    pub count: usize,
    pub model: String,
    pub provider: String,
    pub duration: Duration,
    pub kind: String,
    pub protocols: Vec<Protocol>,
}

/// Offline `cooldown set`: write `cooldowns.json` for one or all client protocols.
pub fn set_from_cli(
    path: &Path,
    provider: &str,
    model: &str,
    duration: &str,
    protocol: Option<&str>,
    kind: Option<&str>,
    reason: Option<&str>,
) -> Result<CooldownSetResult> {
    let duration = parse_manual_duration(duration)?;
    let protocols = protocols_for_cli(protocol)?;
    let kind = kind.unwrap_or(DEFAULT_MANUAL_KIND);
    let reason = reason.unwrap_or("");
    let store = CooldownStore::load(path.to_path_buf())?;
    apply_manual_set(&store, provider, model, duration, kind, reason, &protocols);
    Ok(CooldownSetResult {
        count: protocols.len(),
        model: model.to_string(),
        provider: provider.to_string(),
        duration,
        kind: kind.to_string(),
        protocols,
    })
}

pub fn apply_manual_set(
    store: &CooldownStore,
    provider: &str,
    model: &str,
    duration: Duration,
    kind: &str,
    reason: &str,
    protocols: &[Protocol],
) {
    for protocol in protocols {
        store.set(
            &CooldownKey {
                model_id: model.to_string(),
                provider_id: provider.to_string(),
                protocol: *protocol,
                token: None,
                token_key_hash: None,
            },
            kind,
            reason,
            duration,
        );
    }
}

pub fn format_set_message(result: &CooldownSetResult) -> String {
    let protocol_label = result
        .protocols
        .iter()
        .map(|protocol| protocol.route_key())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Set {} cooldown(s) for model={} provider={} duration={}s kind={} protocol=[{}]",
        result.count,
        result.model,
        result.provider,
        result.duration.as_secs(),
        result.kind,
        protocol_label
    )
}

pub fn write_entries(path: &Path, entries: Vec<CooldownEntry>) -> Result<()> {
    write_state(path, entries, BTreeMap::new())
}

/// : Write the full cooldown state including bad request counts.
pub fn write_state(
    path: &Path,
    entries: Vec<CooldownEntry>,
    bad_request_counts: BTreeMap<String, u32>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let state = CooldownState {
        version: 1,
        entries,
        bad_request_counts,
    };
    let data =
        serde_json::to_string_pretty(&state).context("failed to serialize cooldown state")? + "\n";
    fs::write(path, data).with_context(|| format!("failed to write {}", path.display()))
}

/// Update cooldown entries on disk when a token is renamed via CLI.
///
/// Updates `token` (display name) to `new_name` and populates `token_key_hash`.
/// Returns the number of entries updated.
pub fn rename_token_in_file(
    path: &Path,
    old_name: &str,
    new_name: &str,
    token_key: &str,
) -> Result<usize> {
    let entries = match read_entries(path) {
        Ok(e) => e,
        Err(err)
            if err.downcast_ref::<std::io::Error>().map(|e| e.kind())
                == Some(std::io::ErrorKind::NotFound) =>
        {
            return Ok(0);
        }
        Err(err) => return Err(err),
    };
    if entries.is_empty() {
        return Ok(0);
    }
    let hash = crate::token::keygen::token_key_hash(token_key);
    let mut changed = 0;
    let updated_entries: Vec<CooldownEntry> = entries
        .into_iter()
        .map(|mut entry| {
            let matches = entry.token_key_hash.as_deref() == Some(&hash)
                || entry.token.as_deref() == Some(old_name);
            if matches {
                entry.token = Some(new_name.to_string());
                entry.token_key_hash = Some(hash.clone());
                changed += 1;
            }
            entry
        })
        .collect();

    if changed > 0 {
        write_entries(path, updated_entries)?;
    }
    Ok(changed)
}

pub fn format_duration_short(secs: u64) -> String {
    if secs >= 86400 {
        let d = secs / 86400;
        let h = (secs % 86400) / 3600;
        if h > 0 {
            format!("{d}d {h}h")
        } else {
            format!("{d}d")
        }
    } else if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m > 0 {
            format!("{h}h {m}m")
        } else {
            format!("{h}h")
        }
    } else if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        if s > 0 {
            format!("{m}m {s}s")
        } else {
            format!("{m}m")
        }
    } else {
        format!("{secs}s")
    }
}

pub fn format_relative_time(now: u64, hit_unix: Option<u64>) -> String {
    match hit_unix {
        None | Some(0) => "never".to_string(),
        Some(hit) => {
            let diff = now.saturating_sub(hit);
            if diff < 60 {
                format!("{diff}s ago")
            } else if diff < 3600 {
                format!("{}m ago", diff / 60)
            } else if diff < 86400 {
                let h = diff / 3600;
                let m = (diff % 3600) / 60;
                if m > 0 {
                    format!("{h}h {m}m ago")
                } else {
                    format!("{h}h ago")
                }
            } else {
                format!("{}d ago", diff / 86400)
            }
        }
    }
}

pub fn print_list(path: &Path, config: Option<&crate::config::Config>) -> Result<()> {
    let entries = read_entries(path)?;
    if entries.is_empty() {
        println!("No active cooldowns");
        return Ok(());
    }
    let now = unix_secs();

    println!(
        "{:<16}  {:<28}  {:<10}  {:<10}  {:<12}  {:<10}  {:<6}  STATUS",
        "PROVIDER", "MODEL", "PROTOCOL", "DURATION", "REMAINING", "LAST HIT", "HITS"
    );

    for entry in entries {
        let model_display = if let Some(token) = &entry.token {
            format!("{} [tok:{}]", entry.model, token)
        } else {
            entry.model.clone()
        };

        let duration_secs = entry.expires_at_unix.saturating_sub(entry.started_at_unix);
        let duration_str = format_duration_short(duration_secs);

        let rem_secs = entry.expires_at_unix.saturating_sub(now);
        let rem_str = format_duration_short(rem_secs);

        let last_hit_str = format_relative_time(now, entry.last_hit_unix);
        let hit_count_str = entry.hit_count.to_string();

        let status_str = match config {
            Some(cfg) => match check_entry_zombie(&entry, cfg) {
                Some(reason) => format!("[ZOMBIE: {reason}]"),
                None => "active".to_string(),
            },
            None => "active".to_string(),
        };

        println!(
            "{:<16}  {:<28}  {:<10}  {:<10}  {:<12}  {:<10}  {:<6}  {}",
            entry.provider,
            model_display,
            entry.protocol.route_key(),
            duration_str,
            rem_str,
            last_hit_str,
            hit_count_str,
            status_str,
        );
    }
    Ok(())
}

pub(crate) fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Format unix seconds into a local time string `%Y-%m-%d %H:%M`.
pub fn format_timestamp_local(unix: u64) -> String {
    chrono::DateTime::from_timestamp(unix as i64, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| unix.to_string())
}

/// Returns a formatted cooldown badge string for CLI display.
///
/// If stdout is a terminal, colors `[COOLDOWN until ...]` in red and `[OK]` in green.
pub fn cooldown_badge(cooldown: Option<&str>) -> String {
    use std::io::IsTerminal;
    cooldown_badge_with_color(cooldown, std::io::stdout().is_terminal())
}

/// Returns a formatted cooldown badge string, optionally ANSI colored.
pub fn cooldown_badge_with_color(cooldown: Option<&str>, color: bool) -> String {
    match cooldown {
        Some(until) => {
            let text = format!("[COOLDOWN until {until}]");
            if color {
                format!("\x1b[31m{text}\x1b[0m")
            } else {
                text
            }
        }
        None => {
            let text = "[OK]";
            if color {
                format!("\x1b[32m{text}\x1b[0m")
            } else {
                text.to_string()
            }
        }
    }
}

/// Finds an active cooldown entry for a specific model binding (model + provider + protocol) from a list of entries.
///
/// Prefers provider-global cooldown entry if active; otherwise returns any active entry
/// matching the binding with the latest expiration time.
pub fn find_binding_cooldown<'a>(
    entries: &'a [CooldownEntry],
    model_id: &str,
    provider_id: &str,
    protocol: Protocol,
    now: u64,
) -> Option<&'a CooldownEntry> {
    let matching = entries.iter().filter(|e| {
        e.expires_at_unix > now
            && e.model == model_id
            && e.provider == provider_id
            && e.protocol == protocol
    });
    let mut best: Option<&'a CooldownEntry> = None;
    for entry in matching {
        if entry.token.is_none() && entry.token_key_hash.is_none() {
            return Some(entry);
        }
        if best
            .map(|b| entry.expires_at_unix > b.expires_at_unix)
            .unwrap_or(true)
        {
            best = Some(entry);
        }
    }
    best
}

/// Finds any active cooldown entry for a provider across all its models and protocols from a list of entries.
///
/// Returns the entry with the latest expiration time, if any.
pub fn find_provider_cooldown<'a>(
    entries: &'a [CooldownEntry],
    provider_id: &str,
    now: u64,
) -> Option<&'a CooldownEntry> {
    entries
        .iter()
        .filter(|e| e.expires_at_unix > now && e.provider == provider_id)
        .max_by_key(|e| e.expires_at_unix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(model: &str, provider: &str, protocol: Protocol) -> CooldownKey {
        CooldownKey {
            model_id: model.into(),
            provider_id: provider.into(),
            protocol,
            token: None,
            token_key_hash: None,
        }
    }

    fn active_entry(model: &str, provider: &str, protocol: Protocol) -> CooldownEntry {
        CooldownEntry {
            model: model.into(),
            provider: provider.into(),
            protocol,
            token: None,
            token_key_hash: None,
            kind: "server_error".into(),
            reason: "500".into(),
            started_at_unix: unix_secs().saturating_sub(1),
            expires_at_unix: unix_secs() + 60,
            hit_count: 0,
            last_hit_unix: None,
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

    // §3.1: (token × provider) 400 cooldowns must not alias the
    // provider-global (health-family) cooldown dimension.
    #[test]
    fn token_scoped_cooldown_isolates_requester_from_provider_and_other_tokens() {
        let store = CooldownStore::in_memory();
        let provider_key = key("m1", "p1", Protocol::OpenaiChatCompletions);
        let alice = CooldownKey {
            token: Some("alice".into()),
            ..provider_key.clone()
        };
        let bob = CooldownKey {
            token: Some("bob".into()),
            ..provider_key.clone()
        };

        store.set(&alice, "client_error", "400", Duration::from_secs(30));

        assert!(store.is_cooling_down(&alice));
        assert!(!store.is_cooling_down(&bob));
        assert!(!store.is_cooling_down(&provider_key));

        // `cooldown clear` is the operator override: it removes the token
        // entries of that provider too.
        assert_eq!(store.clear_entries(None, "p1"), 1);
        assert!(!store.is_cooling_down(&alice));
    }

    #[test]
    fn token_scoped_entries_persist_and_reload_under_their_own_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");
        let provider_key = key("m1", "p1", Protocol::OpenaiChatCompletions);
        let alice = CooldownKey {
            token: Some("alice".into()),
            ..provider_key.clone()
        };
        let store = CooldownStore::load(path.clone()).expect("load");
        store.set(&alice, "client_error", "400", Duration::from_secs(300));

        let disk_entries = read_entries(&path).expect("read disk");
        assert_eq!(disk_entries.len(), 1);
        assert_eq!(disk_entries[0].token.as_deref(), Some("alice"));

        let reloaded = CooldownStore::load(path).expect("reload");
        assert!(reloaded.is_cooling_down(&alice));
        assert!(!reloaded.is_cooling_down(&provider_key));
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
                token: None,
                token_key_hash: None,
                kind: "rate_limit".into(),
                reason: "old 429".into(),
                started_at_unix: now.saturating_sub(120),
                expires_at_unix: now.saturating_sub(1),
                hit_count: 0,
                last_hit_unix: None,
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
                    token: None,
                    token_key_hash: None,
                    kind: "server_error".into(),
                    reason: "500".into(),
                    started_at_unix: 1,
                    expires_at_unix: unix_secs() + 60,
                    hit_count: 0,
                    last_hit_unix: None,
                },
                CooldownEntry {
                    model: "m2".into(),
                    provider: "p1".into(),
                    protocol: Protocol::OpenaiResponses,
                    token: None,
                    token_key_hash: None,
                    kind: "server_error".into(),
                    reason: "500".into(),
                    started_at_unix: 1,
                    expires_at_unix: unix_secs() + 60,
                    hit_count: 0,
                    last_hit_unix: None,
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
    // cooldown.rs file persistence tests
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
            token: None,
            token_key_hash: None,
            kind: "network".into(),
            reason: "timeout".into(),
            started_at_unix: unix_secs(),
            expires_at_unix: unix_secs() + 60,
            hit_count: 0,
            last_hit_unix: None,
        }];
        write_entries(&nested_path, entries).expect("write should create parent dirs");

        // Verify file was created and readable
        assert!(nested_path.exists());
        let loaded = read_entries(&nested_path).expect("read");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].model, "m");
    }

    #[test]
    fn parse_manual_duration_accepts_human_readable_forms() {
        assert_eq!(
            parse_manual_duration("42h").expect("42h"),
            Duration::from_secs(42 * 60 * 60)
        );
        assert_eq!(
            parse_manual_duration("1h30m").expect("1h30m"),
            Duration::from_secs(90 * 60)
        );
        assert_eq!(
            parse_manual_duration("30m").expect("30m"),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            parse_manual_duration("7d").expect("7d"),
            Duration::from_secs(7 * 24 * 60 * 60)
        );
        assert_eq!(
            parse_manual_duration("151200s").expect("151200s"),
            Duration::from_secs(151200)
        );
        assert_eq!(
            parse_manual_duration("30d").expect("30d"),
            MANUAL_COOLDOWN_MAX
        );
    }

    #[test]
    fn parse_manual_duration_rejects_invalid_and_capped_values() {
        for input in ["", "abc", "42", "42x", "h30", "0s", "31d"] {
            assert!(
                parse_manual_duration(input).is_err(),
                "expected {input:?} to be rejected"
            );
        }
    }

    #[test]
    fn set_from_cli_writes_all_client_protocols_when_omitted() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");

        let result = set_from_cli(
            &path,
            "openai-sub",
            "gemini-3.8-flash-high-lp",
            "42h",
            None,
            None,
            Some("quota exhausted"),
        )
        .expect("set");

        assert_eq!(result.count, Protocol::CLIENT_PROTOCOLS.len());
        assert_eq!(result.kind, DEFAULT_MANUAL_KIND);
        let entries = read_entries(&path).expect("read");
        assert_eq!(entries.len(), Protocol::CLIENT_PROTOCOLS.len());
        for protocol in Protocol::CLIENT_PROTOCOLS {
            let entry = entries
                .iter()
                .find(|entry| entry.protocol == protocol)
                .expect("protocol entry");
            assert_eq!(entry.provider, "openai-sub");
            assert_eq!(entry.model, "gemini-3.8-flash-high-lp");
            assert_eq!(entry.kind, "manual");
            assert_eq!(entry.reason, "quota exhausted");
            assert!(entry.expires_at_unix > unix_secs() + 41 * 60 * 60);
        }
    }

    #[test]
    fn set_from_cli_then_clear_removes_manual_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");
        set_from_cli(
            &path,
            "p1",
            "m1",
            "30m",
            Some("anthropic"),
            Some("manual"),
            Some("quota"),
        )
        .expect("set");
        assert_eq!(read_entries(&path).expect("read").len(), 1);
        assert_eq!(clear(&path, Some("m1"), "p1").expect("clear"), 1);
        assert!(read_entries(&path).expect("read").is_empty());
    }

    // =========================================================================
    // Token rename cooldown reconciliation and hash matching tests
    // =========================================================================

    #[test]
    fn matches_key_matches_by_hash_across_name_change() {
        let token_key = "lp-secret-key-12345678901234567890";
        let hash = crate::token::keygen::token_key_hash(token_key);

        let entry = CooldownEntry {
            model: "m1".into(),
            provider: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("alice".into()),
            token_key_hash: Some(hash.clone()),
            kind: "client_error".into(),
            reason: "400".into(),
            started_at_unix: unix_secs(),
            expires_at_unix: unix_secs() + 60,
            hit_count: 0,
            last_hit_unix: None,
        };

        // Query with renamed token (different name, same hash)
        let query_renamed = CooldownKey {
            model_id: "m1".into(),
            provider_id: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("alice-v2".into()),
            token_key_hash: Some(hash.clone()),
        };
        assert!(entry.matches_key(&query_renamed));

        // Query with different token key (different hash, same old name)
        let query_other = CooldownKey {
            model_id: "m1".into(),
            provider_id: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("alice".into()),
            token_key_hash: Some("0123456789abcdef0123456789abcdef".into()),
        };
        assert!(!entry.matches_key(&query_other));
    }

    #[test]
    fn matches_key_legacy_fallback_by_name_when_hash_absent() {
        let entry = CooldownEntry {
            model: "m1".into(),
            provider: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("bob".into()),
            token_key_hash: None,
            kind: "client_error".into(),
            reason: "400".into(),
            started_at_unix: unix_secs(),
            expires_at_unix: unix_secs() + 60,
            hit_count: 0,
            last_hit_unix: None,
        };

        // Query key with both name and newly computed hash
        let query_key = CooldownKey {
            model_id: "m1".into(),
            provider_id: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("bob".into()),
            token_key_hash: Some("somehash".into()),
        };
        // Matches because legacy entry has no hash, falls back to name matching
        assert!(entry.matches_key(&query_key));

        let query_diff_name = CooldownKey {
            model_id: "m1".into(),
            provider_id: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("bob-v2".into()),
            token_key_hash: Some("somehash".into()),
        };
        assert!(!entry.matches_key(&query_diff_name));
    }

    #[test]
    fn store_reconcile_updates_renamed_token_and_populates_legacy_hashes() {
        let store = CooldownStore::in_memory();
        let key1 = "lp-key-one-12345678901234567890";
        let hash1 = crate::token::keygen::token_key_hash(key1);
        let key2 = "lp-key-two-12345678901234567890";
        let hash2 = crate::token::keygen::token_key_hash(key2);

        // Entry 1: token renamed from "alice" to "alice-v2" in tokens.toml
        let entry1 = CooldownEntry {
            model: "m1".into(),
            provider: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("alice".into()),
            token_key_hash: Some(hash1.clone()),
            kind: "client_error".into(),
            reason: "400".into(),
            started_at_unix: unix_secs(),
            expires_at_unix: unix_secs() + 60,
            hit_count: 0,
            last_hit_unix: None,
        };

        // Entry 2: legacy entry without hash for "bob"
        let entry2 = CooldownEntry {
            model: "m2".into(),
            provider: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("bob".into()),
            token_key_hash: None,
            kind: "client_error".into(),
            reason: "400".into(),
            started_at_unix: unix_secs(),
            expires_at_unix: unix_secs() + 60,
            hit_count: 0,
            last_hit_unix: None,
        };

        {
            let mut entries = store.entries.lock().unwrap();
            entries.insert(entry1.stable_id(), entry1);
            entries.insert(entry2.stable_id(), entry2);
        }

        let tokens_config = crate::token::config::TokensConfig {
            tokens: vec![
                crate::token::config::TokenConfig {
                    key: key1.into(),
                    name: "alice-v2".into(),
                    allowed_models: vec![],
                    quota_windows: vec![],
                    model_quota_groups: vec![],
                    expired_at: None,
                    bad_request_budget_daily_limit: None,
                    bad_request_budget_lifetime_limit: None,
                    admin: false,
                },
                crate::token::config::TokenConfig {
                    key: key2.into(),
                    name: "bob".into(),
                    allowed_models: vec![],
                    quota_windows: vec![],
                    model_quota_groups: vec![],
                    expired_at: None,
                    bad_request_budget_daily_limit: None,
                    bad_request_budget_lifetime_limit: None,
                    admin: false,
                },
            ],
            ..Default::default()
        };

        assert!(store.reconcile(&tokens_config));

        // Verify entry1 has its display name updated to alice-v2
        let query_renamed = CooldownKey {
            model_id: "m1".into(),
            provider_id: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("alice-v2".into()),
            token_key_hash: Some(hash1),
        };
        let active1 = store.active_entry(&query_renamed).expect("active1");
        assert_eq!(active1.token.as_deref(), Some("alice-v2"));

        // Verify entry2 has its hash populated with hash2
        let query_bob = CooldownKey {
            model_id: "m2".into(),
            provider_id: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("bob".into()),
            token_key_hash: Some(hash2.clone()),
        };
        let active2 = store.active_entry(&query_bob).expect("active2");
        assert_eq!(active2.token_key_hash.as_deref(), Some(hash2.as_str()));
    }

    #[test]
    fn rename_token_in_file_updates_disk_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");
        let key = "lp-secret-key-for-rename-test123";
        let hash = crate::token::keygen::token_key_hash(key);

        let entries = vec![CooldownEntry {
            model: "gemini-3.8-flash".into(),
            provider: "openai-sub".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("alice".into()),
            token_key_hash: Some(hash.clone()),
            kind: "client_error".into(),
            reason: "400".into(),
            started_at_unix: unix_secs(),
            expires_at_unix: unix_secs() + 300,
            hit_count: 0,
            last_hit_unix: None,
        }];
        write_entries(&path, entries).expect("write");

        let changed = rename_token_in_file(&path, "alice", "alice-renamed", key).expect("rename");
        assert_eq!(changed, 1);

        let reloaded = read_entries(&path).expect("reload");
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].token.as_deref(), Some("alice-renamed"));
        assert_eq!(reloaded[0].token_key_hash.as_deref(), Some(hash.as_str()));
    }

    #[test]
    fn cooldown_entry_backward_compatible_deserialization() {
        let json = r#"{
            "model": "m1",
            "provider": "p1",
            "protocol": "openai-chat-completions",
            "kind": "rate_limit",
            "reason": "429",
            "started_at_unix": 1000,
            "expires_at_unix": 2000
        }"#;
        let entry: CooldownEntry = serde_json::from_str(json).expect("deserialize legacy entry");
        assert_eq!(entry.hit_count, 0);
        assert_eq!(entry.last_hit_unix, None);

        let json_with_hits = r#"{
            "model": "m1",
            "provider": "p1",
            "protocol": "openai-chat-completions",
            "kind": "rate_limit",
            "reason": "429",
            "started_at_unix": 1000,
            "expires_at_unix": 2000,
            "hit_count": 5,
            "last_hit_unix": 1500
        }"#;
        let entry_hits: CooldownEntry =
            serde_json::from_str(json_with_hits).expect("deserialize entry with hits");
        assert_eq!(entry_hits.hit_count, 5);
        assert_eq!(entry_hits.last_hit_unix, Some(1500));

        let serialized = serde_json::to_string(&entry).expect("serialize entry");
        assert!(!serialized.contains("last_hit_unix"));
        assert!(serialized.contains(r#""hit_count":0"#));
    }

    #[test]
    fn check_and_record_hit_updates_hits_and_active_entry_is_readonly() {
        let store = CooldownStore::in_memory();
        let k = key("m1", "p1", Protocol::OpenaiChatCompletions);
        store.set(&k, "rate_limit", "429", Duration::from_secs(60));

        // active_entry is read-only
        let entry_initial = store.active_entry(&k).expect("entry exists");
        assert_eq!(entry_initial.hit_count, 0);
        assert_eq!(entry_initial.last_hit_unix, None);

        // check_and_record_hit increments
        assert!(store.check_and_record_hit(&k));
        let entry_after_first = store.active_entry(&k).expect("entry exists");
        assert_eq!(entry_after_first.hit_count, 1);
        assert!(entry_after_first.last_hit_unix.is_some());
        let first_hit_time = entry_after_first.last_hit_unix.unwrap();

        // second hit
        assert!(store.check_and_record_hit(&k));
        let entry_after_second = store.active_entry(&k).expect("entry exists");
        assert_eq!(entry_after_second.hit_count, 2);
        assert!(entry_after_second.last_hit_unix.unwrap() >= first_hit_time);

        // new semantics: is_cooling_down is a pure query (no counter side
        // effect); record_cooldown_hit is the explicit interception counter.
        assert!(store.is_cooling_down(&k));
        let entry_after_query = store.active_entry(&k).expect("entry exists");
        assert_eq!(
            entry_after_query.hit_count, 2,
            "pure query must not inflate hit_count"
        );
        store.record_cooldown_hit(&k);
        let entry_after_third = store.active_entry(&k).expect("entry exists");
        assert_eq!(entry_after_third.hit_count, 3);
    }

    /// hit_count 语义锁定：状态查询（多键检查）不得膨胀计数——一次请求
    /// 对同一冷却条目可经 token/plan/provider 多键形态检查多次，hit_count
    /// 只记录真实拦截事件（每拦截一次 +1）。
    #[test]
    fn hit_count_counts_interceptions_not_queries() {
        let k = key("m1", "p1", Protocol::OpenaiChatCompletions);
        let store = CooldownStore::in_memory();
        store.set(&k, "client_error", "400", Duration::from_secs(60));
        // 多键形态查询（模拟一次请求的多键检查）不得计数
        for _ in 0..5 {
            assert!(store.is_cooling_down(&k));
        }
        let entry = store.active_entry(&k).expect("entry exists");
        assert_eq!(
            entry.hit_count, 0,
            "pure queries must not inflate hit_count"
        );
        // 显式拦截记录：每次拦截 +1
        store.record_cooldown_hit(&k);
        store.record_cooldown_hit(&k);
        let entry = store.active_entry(&k).expect("entry exists");
        assert_eq!(entry.hit_count, 2, "one record per interception");
    }

    #[test]
    fn hits_persisted_to_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");
        let k = key("m1", "p1", Protocol::OpenaiChatCompletions);

        {
            let store = CooldownStore::load(path.clone()).expect("load store");
            store.set(&k, "rate_limit", "429", Duration::from_secs(60));
            store.record_cooldown_hit(&k);
            store.record_cooldown_hit(&k);
        }

        // Reload from disk and verify hit_count and last_hit_unix survived
        let reloaded = CooldownStore::load(path).expect("reload store");
        let entry = reloaded.active_entry(&k).expect("entry exists");
        assert_eq!(entry.hit_count, 2);
        assert!(entry.last_hit_unix.is_some());
    }

    #[test]
    fn relative_time_and_duration_formatting() {
        let now = 1_000_000;
        assert_eq!(format_relative_time(now, None), "never");
        assert_eq!(format_relative_time(now, Some(now + 10)), "0s ago");
        assert_eq!(format_relative_time(now, Some(now)), "0s ago");
        assert_eq!(format_relative_time(now, Some(now - 15)), "15s ago");
        assert_eq!(format_relative_time(now, Some(now - 60)), "1m ago");
        assert_eq!(format_relative_time(now, Some(now - 125)), "2m ago");
        assert_eq!(format_relative_time(now, Some(now - 3600)), "1h ago");
        assert_eq!(format_relative_time(now, Some(now - 7200)), "2h ago");
        assert_eq!(format_relative_time(now, Some(now - 86400)), "1d ago");
        assert_eq!(format_relative_time(now, Some(now - 172800)), "2d ago");

        assert_eq!(format_duration_short(0), "0s");
        assert_eq!(format_duration_short(45), "45s");
        assert_eq!(format_duration_short(60), "1m");
        assert_eq!(format_duration_short(75), "1m 15s");
        assert_eq!(format_duration_short(3600), "1h");
        assert_eq!(format_duration_short(3660), "1h 1m");
        assert_eq!(format_duration_short(86400), "1d");
        assert_eq!(format_duration_short(90060), "1d 1h");
    }

    #[test]
    fn zombie_and_startup_validation() {
        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.prov-a]
api_key_env = "PROV_A_KEY"

[providers.prov-b]
api_key_env = "PROV_B_KEY"

[models.mod-a]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "prov-a", model = "mod-a" }]
"#;
        let cfg: crate::config::Config = toml::from_str(toml_str).expect("parse test config");

        // 1. Valid entry
        let valid = active_entry("mod-a", "prov-a", Protocol::OpenaiChatCompletions);
        assert_eq!(check_entry_zombie(&valid, &cfg), None);

        // 2. Unknown provider
        let unk_prov = active_entry("mod-a", "prov-ghost", Protocol::OpenaiChatCompletions);
        assert_eq!(
            check_entry_zombie(&unk_prov, &cfg),
            Some("unknown provider 'prov-ghost'".to_string())
        );

        // 3. Unknown model
        let unk_mod = active_entry("mod-ghost", "prov-a", Protocol::OpenaiChatCompletions);
        assert_eq!(
            check_entry_zombie(&unk_mod, &cfg),
            Some("unknown model 'mod-ghost'".to_string())
        );

        // 4. Model exists, provider exists, but model does not bind provider
        let unbound = active_entry("mod-a", "prov-b", Protocol::OpenaiChatCompletions);
        assert_eq!(
            check_entry_zombie(&unbound, &cfg),
            Some("model 'mod-a' which does not bind provider 'prov-b'".to_string())
        );

        // CooldownStore validation warnings
        let store = CooldownStore::in_memory();
        store
            .entries
            .lock()
            .unwrap()
            .insert(valid.stable_id(), valid);
        store
            .entries
            .lock()
            .unwrap()
            .insert(unk_prov.stable_id(), unk_prov);
        store
            .entries
            .lock()
            .unwrap()
            .insert(unbound.stable_id(), unbound);

        let warnings = store.validate_against_config(&cfg);
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().any(|w| w.contains("unknown provider")));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("does not bind provider"))
        );
    }

    #[test]
    fn print_list_table_format() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("cooldowns.json");

        // Empty case
        assert!(print_list(&path, None).is_ok());

        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.openai]
api_key_env = "OPENAI_API_KEY"

[models.gpt-4]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "openai", model = "gpt-4" }]
"#;
        let cfg: crate::config::Config = toml::from_str(toml_str).expect("parse test config");

        let mut entry1 = active_entry("gpt-4", "openai", Protocol::OpenaiChatCompletions);
        entry1.hit_count = 3;
        entry1.last_hit_unix = Some(unix_secs().saturating_sub(5));

        let mut entry2 = active_entry("unknown-model", "openai", Protocol::OpenaiChatCompletions);
        entry2.hit_count = 0;
        entry2.last_hit_unix = None;

        write_entries(&path, vec![entry1, entry2]).expect("write");

        assert!(print_list(&path, Some(&cfg)).is_ok());
    }

    #[test]
    fn cooldown_formatting_and_badge() {
        let ts = 1759320000; // 2025-10-01T12:00:00Z approx
        let formatted = format_timestamp_local(ts);
        assert!(formatted.contains("-"));
        assert!(formatted.contains(":"));

        let ok_plain = cooldown_badge_with_color(None, false);
        assert_eq!(ok_plain, "[OK]");

        let ok_colored = cooldown_badge_with_color(None, true);
        assert_eq!(ok_colored, "\x1b[32m[OK]\x1b[0m");

        let cd_plain = cooldown_badge_with_color(Some("2026-10-01 12:00"), false);
        assert_eq!(cd_plain, "[COOLDOWN until 2026-10-01 12:00]");

        let cd_colored = cooldown_badge_with_color(Some("2026-10-01 12:00"), true);
        assert_eq!(
            cd_colored,
            "\x1b[31m[COOLDOWN until 2026-10-01 12:00]\x1b[0m"
        );
    }

    #[test]
    fn binding_and_provider_cooldown_lookup() {
        let now = 1_000_000;
        let mut entry1 = active_entry("model-a", "prov-1", Protocol::OpenaiChatCompletions);
        entry1.expires_at_unix = now + 3600;
        entry1.token = None;
        entry1.token_key_hash = None;

        let mut entry2 = active_entry("model-b", "prov-1", Protocol::Anthropic);
        entry2.expires_at_unix = now + 7200;
        entry2.token = Some("tok1".into());
        entry2.token_key_hash = Some("hash1".into());

        let entries = vec![entry1.clone(), entry2.clone()];

        // Binding lookup
        let found = find_binding_cooldown(
            &entries,
            "model-a",
            "prov-1",
            Protocol::OpenaiChatCompletions,
            now,
        );
        assert!(found.is_some());
        assert_eq!(found.unwrap().expires_at_unix, now + 3600);

        let not_found =
            find_binding_cooldown(&entries, "model-a", "prov-1", Protocol::Anthropic, now);
        assert!(not_found.is_none());

        // Provider lookup
        let prov_cd = find_provider_cooldown(&entries, "prov-1", now);
        assert!(prov_cd.is_some());
        assert_eq!(prov_cd.unwrap().expires_at_unix, now + 7200);

        let prov_none = find_provider_cooldown(&entries, "prov-other", now);
        assert!(prov_none.is_none());

        // CooldownStore lookup
        let store = CooldownStore::in_memory();
        store.set(
            &CooldownKey {
                model_id: "model-a".into(),
                provider_id: "prov-1".into(),
                protocol: Protocol::OpenaiChatCompletions,
                token: None,
                token_key_hash: None,
            },
            "rate_limit",
            "429",
            Duration::from_secs(3600),
        );

        let active_b =
            store.active_binding_cooldown("model-a", "prov-1", Protocol::OpenaiChatCompletions);
        assert!(active_b.is_some());

        let active_p = store.active_provider_cooldown("prov-1");
        assert!(active_p.is_some());

        let active_p_other = store.active_provider_cooldown("prov-other");
        assert!(active_p_other.is_none());
    }

    // =========================================================================
    // : provider-level wildcard cooldown + bad request tolerance
    // =========================================================================

    /// 判据 3 red proof：provider 级通配冷却（model "*"）必须阻断该 provider
    /// 下任意具体 model 的路由查询（is_cooling_down / active_entry /
    /// active_binding_cooldown 三条查询路径）。
    #[test]
    fn wildcard_cooldown_blocks_every_model_of_provider() {
        let store = CooldownStore::in_memory();
        store.set(
            &key("*", "p1", Protocol::OpenaiChatCompletions),
            "bad_request_tolerance",
            "probe 400 tolerance reached (2/2)",
            Duration::from_secs(300),
        );

        // 同 provider 同协议的任意具体 model 都被阻断
        assert!(
            store.is_cooling_down(&key("m1", "p1", Protocol::OpenaiChatCompletions)),
            "wildcard must block concrete model m1"
        );
        assert!(
            store.is_cooling_down(&key("m2", "p1", Protocol::OpenaiChatCompletions)),
            "wildcard must block concrete model m2"
        );
        // 路由 gate 的 active_cooldown 查询路径
        assert!(
            store
                .active_entry(&key("m1", "p1", Protocol::OpenaiChatCompletions))
                .is_some()
        );
        // status 展示的 active_binding_cooldown 查询路径
        assert!(
            store
                .active_binding_cooldown("m1", "p1", Protocol::OpenaiChatCompletions)
                .is_some()
        );
        // 其他 provider 不受影响
        assert!(!store.is_cooling_down(&key("m1", "p2", Protocol::OpenaiChatCompletions)));
        // 其他协议不受影响（写入时按 CLIENT_PROTOCOLS 全协议展开，这里 p1 只写了 chat）
        assert!(!store.is_cooling_down(&key("m1", "p1", Protocol::OpenaiResponses)));
    }

    /// §3.1 维度隔离：通配是 provider 健康维度，token-scoped 查询不得命中它
    /// （token 维度归 /§3.1 归因逻辑管；路由对每个候选都会查
    /// provider-global key，所以阻断对所有 token 生效）。
    #[test]
    fn wildcard_does_not_alias_token_scoped_dimension() {
        let store = CooldownStore::in_memory();
        store.set(
            &key("*", "p1", Protocol::OpenaiChatCompletions),
            "bad_request_tolerance",
            "probe 400 tolerance reached (2/2)",
            Duration::from_secs(300),
        );
        let token_key = CooldownKey {
            model_id: "m1".into(),
            provider_id: "p1".into(),
            protocol: Protocol::OpenaiChatCompletions,
            token: Some("t1".into()),
            token_key_hash: Some("hash1".into()),
        };
        assert!(!store.is_cooling_down(&token_key));
    }

    /// 写入具体 model 的冷却（如 429 回写）不得清掉 provider 级通配条目。
    #[test]
    fn wildcard_survives_concrete_cooldown_write() {
        let store = CooldownStore::in_memory();
        store.set(
            &key("*", "p1", Protocol::OpenaiChatCompletions),
            "bad_request_tolerance",
            "probe 400 tolerance reached (2/2)",
            Duration::from_secs(300),
        );
        store.set(
            &key("m1", "p1", Protocol::OpenaiChatCompletions),
            "server_error",
            "500",
            Duration::from_secs(60),
        );
        // m2 只可能因通配条目仍在而被阻断
        assert!(store.is_cooling_down(&key("m2", "p1", Protocol::OpenaiChatCompletions)));
    }

    /// 异常请求计数必须跨进程持久化：计数随 cooldowns.json 落盘并可重载；
    /// reset 后重载为 0。
    #[test]
    fn bad_request_counts_persist_across_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cooldowns.json");
        {
            let store = CooldownStore::load(path.clone()).expect("load");
            assert_eq!(store.increment_bad_request_count("p1"), 1);
            assert_eq!(store.increment_bad_request_count("p1"), 2);
            assert_eq!(store.increment_bad_request_count("p2"), 1);
        }
        let reloaded = CooldownStore::load(path.clone()).expect("reload");
        assert_eq!(reloaded.get_bad_request_count("p1"), 2);
        assert_eq!(reloaded.get_bad_request_count("p2"), 1);
        reloaded.reset_bad_request_count("p1");
        let again = CooldownStore::load(path).expect("reload2");
        assert_eq!(again.get_bad_request_count("p1"), 0);
        assert_eq!(again.get_bad_request_count("p2"), 1);
    }

    /// 通配条目通过僵尸校验（model "*" 不做 model 绑定检查，只查 provider 存在）。
    #[test]
    fn wildcard_entry_is_not_a_zombie() {
        let cfg = crate::config::default_deepseek_config();
        let ok = active_entry("*", "deepseek", Protocol::OpenaiChatCompletions);
        assert_eq!(check_entry_zombie(&ok, &cfg), None);
        let bad_provider = active_entry("*", "no-such-provider", Protocol::OpenaiChatCompletions);
        assert!(check_entry_zombie(&bad_provider, &cfg).is_some());
    }
}
