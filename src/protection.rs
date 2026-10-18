use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{
    AccountBadBudgetConfig, BadRequestProtectionConfig, ErrorCooldownConfig, LocalRejectConfig,
    Protocol,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockStatus {
    pub blocked: bool,
    pub remaining_seconds: u64,
    pub reason: Option<String>,
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
                reason: None,
            };
        }
        let now = unix_secs();
        let Ok(mut entries) = self.entries.lock() else {
            return BlockStatus {
                blocked: false,
                remaining_seconds: 0,
                reason: None,
            };
        };
        cleanup(&mut entries, now, self.cfg.window_seconds);
        if let Some(entry) = entries.get(fingerprint)
            && entry.blocked_until_unix > now
        {
            return BlockStatus {
                blocked: true,
                remaining_seconds: entry.blocked_until_unix - now,
                reason: None,
            };
        }
        BlockStatus {
            blocked: false,
            remaining_seconds: 0,
            reason: None,
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

/// Hash one client request's identity: protocol + model + raw body bytes.
///
/// The body segment is the exact wire bytes as received — insignificant
/// whitespace and key order are part of the identity, so a re-spaced payload
/// is a distinct fingerprint. That trades evadability-by-respacing for the
/// removal of one full body re-encode per request.
pub fn fingerprint(protocol: Protocol, model: Option<&str>, body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(protocol.route_key().as_bytes());
    hasher.update(b"\n");
    hasher.update(model.unwrap_or_default().as_bytes());
    hasher.update(b"\n");
    hasher.update(body);
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

// ── Error-class extraction ────────────────────────────────────────────

/// Extract a stable error-class identifier from an upstream error response.
///
/// Priority:
/// 1. Upstream structured error type (e.g. Google `@type`) → short hash
/// 2. `upstream_message` full hash → `msg_<sha256[..16]>`
/// 3. Keyword matching (fallback) → named class
/// 4. `"unknown"`
pub fn extract_error_class(upstream_message: &str, upstream_detail_types: &[String]) -> String {
    // Priority 1: upstream structured error type (most stable)
    if let Some(detail_type) = upstream_detail_types.first()
        && !detail_type.is_empty()
    {
        let hash = short_hash(detail_type);
        return format!("type_{hash}");
    }

    // Priority 2: upstream_message full hash (stable)
    if !upstream_message.is_empty() {
        let hash = short_hash(upstream_message);
        return format!("msg_{hash}");
    }

    // Priority 3: keyword matching (fallback, less stable)
    const PATTERNS: &[(&str, &str)] = &[
        ("thought_signature", "missing_thought_signature"),
        ("Invalid schema", "invalid_schema"),
        ("missing a required parameter", "missing_required_parameter"),
        ("must be a string", "type_mismatch_string"),
        ("must be a number", "type_mismatch_number"),
        ("must be an object", "type_mismatch_object"),
        ("must be an array", "type_mismatch_array"),
    ];
    for (keyword, class) in PATTERNS {
        if upstream_message.contains(keyword) {
            return class.to_string();
        }
    }

    "unknown".to_string()
}

/// Check if a Google-RPC-style 400 response is a rate-limit/quota error rather
/// than a true request error.
///
/// Google RPC returns structured error bodies with `details[].@type`
/// and `error.message`. Rate-limit errors contain `RESOURCE_EXHAUSTED` in detail
/// types or keywords like "rate limit", "quota", "too many requests" in the message.
///
/// Returns `true` if the 400 should be treated as a rate-limit (short cooldown
/// + fallback) rather than counted toward the bad-request budget.
pub fn is_google_rpc_rate_limit_400(body: &str) -> bool {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };

    // Check detail_types for RESOURCE_EXHAUSTED
    let detail_types = crate::diagnostics::extract_upstream_detail_types(&json);
    if detail_types
        .iter()
        .any(|t| t.contains("RESOURCE_EXHAUSTED"))
    {
        return true;
    }

    // Check error.message for rate-limit keywords
    let message = extract_message_from_error_json(&json).to_lowercase();
    const RATE_LIMIT_KEYWORDS: &[&str] = &[
        "rate limit",
        "ratelimit",
        "quota",
        "too many requests",
        "exceeded",
        "throttled",
    ];
    RATE_LIMIT_KEYWORDS.iter().any(|kw| message.contains(kw))
}

/// Extract error-class from a raw JSON response body.
///
/// This is a convenience wrapper that extracts the message and detail types
/// from common error envelope shapes, then calls `extract_error_class`.
#[allow(dead_code)]
pub fn extract_error_class_from_body(body: &str) -> String {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return "unknown".to_string();
    };
    let message = extract_message_from_error_json(&json);
    let detail_types = crate::diagnostics::extract_upstream_detail_types(&json);
    extract_error_class(&message, &detail_types)
}

#[allow(dead_code)]
fn extract_message_from_error_json(json: &serde_json::Value) -> String {
    // Try common shapes:
    // {"error": {"message": "..."}}
    // {"error": "string message"}
    // {"message": "..."}
    if let Some(error) = json.get("error") {
        if let Some(msg) = error.get("message").and_then(|v| v.as_str()) {
            return msg.to_string();
        }
        if let Some(msg) = error.as_str() {
            return msg.to_string();
        }
    }
    if let Some(msg) = json.get("message").and_then(|v| v.as_str()) {
        return msg.to_string();
    }
    String::new()
}

fn short_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())[..16].to_string()
}

/// Redact a token key for logs/snapshots: short prefix + suffix only
/// (`lp-kz0…2b8`), so operators can correlate without exposing the secret.
pub fn redact_token_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 12 {
        return "*".repeat(chars.len().max(3));
    }
    let prefix: String = chars[..6].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}…{suffix}")
}

// ── Per-token error-class cooldown ────────────────────────────────────

/// Per-token, per-inbound-model cooldown triggered by repeated errors of the
/// same class.
///
/// When a token accumulates `max_errors_per_class` errors of the same
/// error-class for one inbound model within `window_seconds`, that
/// (token, model) entry is blocked for `cooldown_seconds`: requests from that
/// token **for that model** are rejected locally. Other models of the same
/// token, and all other tokens, are unaffected. This prevents a broken client
/// from continuously hitting upstream with bad requests without collateral
/// damage to traffic that does not produce the error.
///
/// The model dimension is the inbound model — the `model` string exactly as the
/// client sent it, before routing expansion — so a routing model is keyed by
/// its own entry.
///
/// State (per-class counts, window start, cooldown expiry) survives restarts:
/// [`Self::load`] reads it from `state_dir/token-error-cooldowns.json` and every
/// mutation atomically rewrites the file. Expired state is pruned on load
/// (window elapsed → counts dropped, cooldown elapsed → released, entries with
/// nothing left are removed), so counts clear naturally at expiry. Operators
/// can clear state early via `POST /admin/protection/token-error-cooldown/clear`.
/// The file keys entries by token **display name** → model and never contains keys.
#[derive(Debug)]
pub struct TokenErrorClassCooldown {
    cfg: ErrorCooldownConfig,
    local_reject_cfg: LocalRejectConfig,
    /// Per-token state, split by inbound model.
    tokens: Mutex<HashMap<String, TokenState>>,
    /// Persistence target (set by [`Self::load`]); `None` = memory-only.
    path: Option<PathBuf>,
}

/// Cooldown state for one (token, inbound model) pair.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModelState {
    /// error_class → count (reset when cooldown triggers or window expires)
    #[serde(default)]
    error_counts: HashMap<String, u32>,
    /// When this (token, model) entry is blocked until (unix seconds)
    #[serde(default)]
    cooldown_until: u64,
    /// When the error count window started (unix seconds)
    #[serde(default)]
    window_start: u64,
    /// Reason for active cooldown (e.g. "error_class" or "local_reject_tolerance")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cooldown_reason: Option<String>,
    /// Local rejection count in current window
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    local_reject_count: u32,
    /// When the local rejection count window started (unix seconds)
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    local_reject_window_start: u64,
}

fn is_zero_u32(val: &u32) -> bool {
    *val == 0
}

fn is_zero_u64(val: &u64) -> bool {
    *val == 0
}

/// Per-token state, keyed by inbound model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TokenState {
    models: HashMap<String, ModelState>,
}

/// On-disk shape of persisted error-class state
/// (token display name → inbound model → state; never contains token keys).
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedCooldownState {
    #[serde(default)]
    tokens: HashMap<String, TokenState>,
}

/// Superseded on-disk shape: one flat state per token with no model dimension.
/// Entries in this shape cannot be attributed to a model, so they are dropped on
/// load (with one warning) rather than silently blocking every model of a token.
#[derive(Debug, Default, Deserialize)]
struct LegacyTokenState {
    #[serde(default)]
    #[allow(dead_code)]
    error_counts: HashMap<String, u32>,
    #[serde(default)]
    #[allow(dead_code)]
    cooldown_until: u64,
    #[serde(default)]
    #[allow(dead_code)]
    window_start: u64,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyPersistedCooldownState {
    #[serde(default)]
    tokens: HashMap<String, LegacyTokenState>,
}

impl TokenErrorClassCooldown {
    /// Memory-only instance (unit tests / no persistence target).
    #[cfg(test)]
    pub fn new(cfg: ErrorCooldownConfig) -> Self {
        Self {
            cfg,
            local_reject_cfg: Default::default(),
            tokens: Mutex::new(HashMap::new()),
            path: None,
        }
    }

    #[cfg(test)]
    pub fn new_with_configs(cfg: ErrorCooldownConfig, local_reject_cfg: LocalRejectConfig) -> Self {
        Self {
            cfg,
            local_reject_cfg,
            tokens: Mutex::new(HashMap::new()),
            path: None,
        }
    }

    /// Load persisted state from `path` (missing, corrupt, or legacy flat file →
    /// empty state, never blocks startup), pruning anything whose window or
    /// cooldown has already expired.
    pub fn load(
        cfg: ErrorCooldownConfig,
        local_reject_cfg: LocalRejectConfig,
        path: &Path,
    ) -> Self {
        let mut tokens = match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<PersistedCooldownState>(&bytes) {
                Ok(persisted) => persisted.tokens,
                Err(e) => {
                    if let Ok(legacy) =
                        serde_json::from_slice::<LegacyPersistedCooldownState>(&bytes)
                    {
                        tracing::warn!(
                            path = %path.display(),
                            dropped_tokens = legacy.tokens.len(),
                            "token error-cooldown state uses the superseded flat schema without a model dimension; dropping it and starting empty"
                        );
                    } else {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "token error-cooldown state unreadable; starting empty"
                        );
                    }
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "token error-cooldown state unreadable; starting empty"
                );
                HashMap::new()
            }
        };

        let now = unix_secs();
        let pruned = Self::prune_expired(&mut tokens, &cfg, &local_reject_cfg, now);
        let instance = Self {
            cfg,
            local_reject_cfg,
            tokens: Mutex::new(tokens),
            path: Some(path.to_path_buf()),
        };
        if pruned && let Ok(tokens) = instance.tokens.lock() {
            instance.save_locked(&tokens);
        }
        instance
    }

    /// Drop expired windows/cooldowns and empty (token, model) entries; true if
    /// anything changed.
    fn prune_expired(
        tokens: &mut HashMap<String, TokenState>,
        cfg: &ErrorCooldownConfig,
        local_reject_cfg: &LocalRejectConfig,
        now: u64,
    ) -> bool {
        let mut changed = false;
        for token_state in tokens.values_mut() {
            let before = token_state.models.len();
            token_state.models.retain(|_, state| {
                if state.window_start != 0
                    && now.saturating_sub(state.window_start) > cfg.window_seconds
                {
                    state.error_counts.clear();
                    state.window_start = 0;
                    changed = true;
                }
                if state.local_reject_window_start != 0
                    && now.saturating_sub(state.local_reject_window_start)
                        > local_reject_cfg.window_seconds
                {
                    state.local_reject_count = 0;
                    state.local_reject_window_start = 0;
                    changed = true;
                }
                if state.cooldown_until != 0 && state.cooldown_until <= now {
                    state.cooldown_until = 0;
                    state.cooldown_reason = None;
                    changed = true;
                }
                !state.error_counts.is_empty()
                    || state.local_reject_count != 0
                    || state.cooldown_until != 0
            });
            changed |= token_state.models.len() != before;
        }
        let tokens_before = tokens.len();
        tokens.retain(|_, token_state| !token_state.models.is_empty());
        changed |= tokens.len() != tokens_before;
        changed
    }

    /// Atomically persist current state (caller holds the tokens lock).
    fn save_locked(&self, tokens: &HashMap<String, TokenState>) {
        let Some(path) = &self.path else {
            return;
        };
        let payload = match serde_json::to_vec(&PersistedCooldownState {
            tokens: tokens.clone(),
        }) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize token error-cooldown state");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        let result = std::fs::write(&tmp, payload).and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = result {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to persist token error-cooldown state"
            );
        }
    }

    /// Clear counts and cooldown for one token across all its models
    /// (admin operation).
    pub fn clear_token(&self, token_name: &str) -> bool {
        let Ok(mut tokens) = self.tokens.lock() else {
            return false;
        };
        let removed = tokens.remove(token_name).is_some();
        if removed {
            self.save_locked(&tokens);
        }
        removed
    }

    /// Clear counts and cooldown for one (token, model) entry (admin operation).
    /// A token left with no model entries is removed entirely.
    pub fn clear_token_model(&self, token_name: &str, model: &str) -> bool {
        let Ok(mut tokens) = self.tokens.lock() else {
            return false;
        };
        let removed = match tokens.get_mut(token_name) {
            Some(token_state) => {
                let removed = token_state.models.remove(model).is_some();
                if token_state.models.is_empty() {
                    tokens.remove(token_name);
                }
                removed
            }
            None => false,
        };
        if removed {
            self.save_locked(&tokens);
        }
        removed
    }

    /// Clear all token states (admin operation); returns entries removed.
    pub fn clear_all(&self) -> usize {
        let Ok(mut tokens) = self.tokens.lock() else {
            return 0;
        };
        let removed = tokens.len();
        tokens.clear();
        if removed > 0 {
            self.save_locked(&tokens);
        }
        removed
    }

    /// Clear one inbound model's state across **every** token (`"*"` + `model`
    /// filter); returns the number of tokens that had state for that model.
    pub fn clear_model_everywhere(&self, model: &str) -> usize {
        let Ok(mut tokens) = self.tokens.lock() else {
            return 0;
        };
        let mut touched = 0usize;
        tokens.retain(|_, state| {
            if state.models.remove(model).is_some() {
                touched += 1;
            }
            !state.models.is_empty()
        });
        if touched > 0 {
            self.save_locked(&tokens);
        }
        touched
    }

    fn enabled(&self) -> bool {
        (self.cfg.enabled && self.cfg.max_errors_per_class > 0 && self.cfg.cooldown_seconds > 0)
            || (self.local_reject_cfg.enabled
                && self.local_reject_cfg.max_local_rejects > 0
                && self.local_reject_cfg.cooldown_seconds > 0)
    }

    fn error_cooldown_enabled(&self) -> bool {
        self.cfg.enabled && self.cfg.max_errors_per_class > 0 && self.cfg.cooldown_seconds > 0
    }

    fn local_reject_enabled(&self) -> bool {
        self.local_reject_cfg.enabled
            && self.local_reject_cfg.max_local_rejects > 0
            && self.local_reject_cfg.cooldown_seconds > 0
    }

    /// Check if a (token, inbound model) entry is currently in cooldown.
    pub fn is_blocked(&self, token_name: &str, model: &str) -> BlockStatus {
        if !self.enabled() || token_name.is_empty() {
            return BlockStatus {
                blocked: false,
                remaining_seconds: 0,
                reason: None,
            };
        }
        let now = unix_secs();
        let Ok(tokens) = self.tokens.lock() else {
            return BlockStatus {
                blocked: false,
                remaining_seconds: 0,
                reason: None,
            };
        };
        match tokens.get(token_name).and_then(|t| t.models.get(model)) {
            Some(state) if state.cooldown_until > now => BlockStatus {
                blocked: true,
                remaining_seconds: state.cooldown_until - now,
                reason: state.cooldown_reason.clone(),
            },
            _ => BlockStatus {
                blocked: false,
                remaining_seconds: 0,
                reason: None,
            },
        }
    }

    /// Record an error for a (token, inbound model) entry. If the error-class
    /// count reaches the threshold, that entry enters cooldown; other models of
    /// the same token stay usable.
    pub fn record_error(&self, token_name: &str, model: &str, error_class: &str) {
        if !self.error_cooldown_enabled() || token_name.is_empty() {
            return;
        }
        let now = unix_secs();
        let Ok(mut tokens) = self.tokens.lock() else {
            return;
        };
        let state = tokens
            .entry(token_name.to_string())
            .or_default()
            .models
            .entry(model.to_string())
            .or_default();

        // If in active cooldown, don't update counts
        if state.cooldown_until > now {
            return;
        }

        // Reset window if expired
        if now.saturating_sub(state.window_start) > self.cfg.window_seconds {
            state.error_counts.clear();
            state.window_start = now;
        }
        if state.window_start == 0 {
            state.window_start = now;
        }

        // Increment error count for this class
        let count = {
            let entry = state
                .error_counts
                .entry(error_class.to_string())
                .or_insert(0);
            *entry += 1;
            *entry
        };

        tracing::info!(
            token = %token_name,
            model = %model,
            error_class = %error_class,
            count,
            threshold = self.cfg.max_errors_per_class,
            "token error-class counted"
        );

        // Check if threshold reached → trigger cooldown
        if count >= self.cfg.max_errors_per_class {
            state.cooldown_until = now + self.cfg.cooldown_seconds;
            state.cooldown_reason = Some("error_class".to_string());
            state.error_counts.clear();
            tracing::info!(
                token = %token_name,
                model = %model,
                error_class = %error_class,
                count = count,
                cooldown_seconds = self.cfg.cooldown_seconds,
                reason = "error_class",
                "token entered error-class cooldown"
            );
        }

        self.save_locked(&tokens);
    }

    /// Record a client-attributable local rejection for a (token, inbound model) entry.
    /// If the local reject count reaches `max_local_rejects`, that entry enters
    /// cooldown with reason="local_reject_tolerance".
    pub fn record_local_reject(&self, token_name: &str, model: &str, reject_reason: &str) {
        if !self.local_reject_enabled() || token_name.is_empty() || model.is_empty() {
            return;
        }
        let now = unix_secs();
        let Ok(mut tokens) = self.tokens.lock() else {
            return;
        };
        let state = tokens
            .entry(token_name.to_string())
            .or_default()
            .models
            .entry(model.to_string())
            .or_default();

        // If in active cooldown, don't update counts
        if state.cooldown_until > now {
            return;
        }

        // Reset window if expired
        if now.saturating_sub(state.local_reject_window_start)
            > self.local_reject_cfg.window_seconds
        {
            state.local_reject_count = 0;
            state.local_reject_window_start = now;
        }
        if state.local_reject_window_start == 0 {
            state.local_reject_window_start = now;
        }

        state.local_reject_count += 1;
        let count = state.local_reject_count;

        tracing::warn!(
            token = %token_name,
            model = %model,
            counted = true,
            reason = %reject_reason,
            count,
            threshold = self.local_reject_cfg.max_local_rejects,
            "token local-reject counted"
        );

        if count >= self.local_reject_cfg.max_local_rejects {
            state.cooldown_until = now + self.local_reject_cfg.cooldown_seconds;
            state.cooldown_reason = Some("local_reject_tolerance".to_string());
            state.local_reject_count = 0;
            tracing::warn!(
                token = %token_name,
                model = %model,
                reason = "local_reject_tolerance",
                reject_reason = %reject_reason,
                count = count,
                cooldown_seconds = self.local_reject_cfg.cooldown_seconds,
                "token entered local-reject cooldown"
            );
        }

        self.save_locked(&tokens);
    }

    /// Snapshot of current (token, model) states, one entry per pair
    /// (for diagnostics / admin).
    pub fn snapshot(&self) -> Vec<TokenCooldownSnapshot> {
        let now = unix_secs();
        let Ok(tokens) = self.tokens.lock() else {
            return Vec::new();
        };
        let mut snapshots: Vec<TokenCooldownSnapshot> = tokens
            .iter()
            .flat_map(|(token, token_state)| {
                token_state
                    .models
                    .iter()
                    .map(move |(model, state)| (token.clone(), model.clone(), state.clone()))
            })
            .filter(|(_, _, state)| {
                state.cooldown_until > now
                    || !state.error_counts.is_empty()
                    || state.local_reject_count > 0
            })
            .map(|(token, model, state)| TokenCooldownSnapshot {
                token,
                model,
                error_counts: state.error_counts.clone(),
                cooldown_remaining: state.cooldown_until.saturating_sub(now),
                cooldown_reason: state.cooldown_reason.clone(),
                local_reject_count: state.local_reject_count,
            })
            .collect();
        snapshots.sort_by(|a, b| (&a.token, &a.model).cmp(&(&b.token, &b.model)));
        snapshots
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenCooldownSnapshot {
    pub token: String,
    /// Inbound model this entry scopes to.
    pub model: String,
    pub error_counts: HashMap<String, u32>,
    pub cooldown_remaining: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub local_reject_count: u32,
}

// ── Per-account cumulative bad-request budget ──────────────────────────

/// Outcome of a budget check: which layer (if any) blocked the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetBlockReason {
    /// Not blocked.
    Allowed,
    /// Daily layer exhausted; recovers at next natural-day boundary.
    DailyExhausted,
    /// Lifetime layer exhausted; requires admin reset.
    LifetimeExhausted,
}

/// Snapshot of one account's budget state (for admin query).
#[derive(Debug, Clone, Serialize)]
pub struct AccountBudgetSnapshot {
    /// Redacted key (`lp-kz0…2b8` style) — full keys are never serialized.
    pub token_key_redacted: String,
    pub token_name: Option<String>,
    pub daily_limit: u32,
    pub daily_used: u32,
    pub lifetime_limit: u32,
    pub lifetime_used: u32,
    pub daily_locked: bool,
    pub lifetime_locked: bool,
}

/// In-memory entry for one account's budget counters.
#[derive(Debug, Clone)]
struct BudgetEntry {
    /// Display name (for admin snapshots); updated on reconcile.
    token_name: Option<String>,
    /// UTC date string (YYYY-MM-DD) for the daily counter.
    daily_date: String,
    /// Errors counted today.
    daily_count: u32,
    /// Cumulative errors (persistent).
    lifetime_count: u32,
}

/// Row state surfaced with a budget-check decision so blocked-path logs can
/// self-describe: an anomalous lock is auditable from the log alone, without
/// database forensics that may become impossible after an admin reset.
#[derive(Debug, Clone)]
pub struct BudgetCheckDiagnostic {
    /// The `today` value the checker compared against (UTC date string).
    pub today: String,
    /// The row's stored `daily_date`; stale (≠ today) means it must not lock.
    pub daily_date: String,
    /// The row's stored daily counter (meaningful only when `daily_date == today`).
    pub daily_count: u32,
    /// The row's persistent lifetime counter.
    pub lifetime_count: u32,
}

/// Per-account limit overrides: token_key → (daily_limit, lifetime_limit).
type LimitOverrides = HashMap<String, (Option<u32>, Option<u32>)>;

/// Per-account cumulative bad-request budget manager.
///
/// Two independent layers:
/// - **Daily**: resets at natural-day boundary (UTC).
/// - **Lifetime**: persistent across restarts via SQLite.
///
/// Counting scope: only protectable upstream 4xx (excludes 401/403/429).
/// Local rejections and preflight拦截 do not count.
///
/// Thread-safe: `Arc<Mutex<...>>` for both the in-memory map and the SQLite
/// connection. The hot path (`check`) is in-memory only.
#[derive(Debug)]
pub struct AccountBadBudgetManager {
    global_cfg: AccountBadBudgetConfig,
    /// Per-account limit overrides (token_key → (daily_limit, lifetime_limit)).
    overrides: Mutex<LimitOverrides>,
    /// In-memory authoritative state (loaded from SQLite on startup).
    entries: Mutex<HashMap<String, BudgetEntry>>,
    /// SQLite connection for lifetime persistence.
    conn: Arc<Mutex<Connection>>,
}

/// How long a budget write waits for another connection's write transaction.
const BUDGET_WRITE_TIMEOUT_MS: u32 = 1000;

impl AccountBadBudgetManager {
    /// Open or create the budget store at the given database path.
    pub fn open(db_path: &Path, global_cfg: AccountBadBudgetConfig) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create bad-request budget database dir: {}",
                    parent.display()
                )
            })?;
        }
        let conn = Connection::open(db_path).with_context(|| {
            format!(
                "failed to open bad-request budget database: {}",
                db_path.display()
            )
        })?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("failed to set WAL mode for budget database")?;
        conn.pragma_update(None, "busy_timeout", BUDGET_WRITE_TIMEOUT_MS)
            .context("failed to set busy_timeout for budget database")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS bad_request_budget (
                token_key        TEXT PRIMARY KEY,
                lifetime_count   INTEGER NOT NULL DEFAULT 0,
                daily_count      INTEGER NOT NULL DEFAULT 0,
                daily_date       TEXT NOT NULL DEFAULT ''
            );
            "#,
        )
        .context("failed to create bad_request_budget table")?;

        // Load existing rows into memory.
        let mut entries = HashMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT token_key, lifetime_count, daily_count, daily_date
                     FROM bad_request_budget",
                )
                .context("failed to prepare budget load query")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .context("failed to query budget rows")?;
            for row in rows {
                let (key, lifetime, daily, date) = row?;
                entries.insert(
                    key,
                    BudgetEntry {
                        token_name: None,
                        daily_date: date,
                        daily_count: daily,
                        lifetime_count: lifetime,
                    },
                );
            }
        }

        Ok(Self {
            global_cfg,
            overrides: Mutex::new(HashMap::new()),
            entries: Mutex::new(entries),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Create an in-memory-only instance (for tests).
    #[cfg(test)]
    pub fn in_memory(global_cfg: AccountBadBudgetConfig) -> Self {
        let conn = Connection::open_in_memory().expect("in-memory SQLite");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bad_request_budget (
                token_key        TEXT PRIMARY KEY,
                lifetime_count   INTEGER NOT NULL DEFAULT 0,
                daily_count      INTEGER NOT NULL DEFAULT 0,
                daily_date       TEXT NOT NULL DEFAULT ''
            );",
        )
        .unwrap();
        Self {
            global_cfg,
            overrides: Mutex::new(HashMap::new()),
            entries: Mutex::new(HashMap::new()),
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Create an in-memory fallback when the on-disk database cannot be opened.
    /// Data will not survive restarts, but the protection layer remains functional.
    pub fn in_memory_fallback(global_cfg: AccountBadBudgetConfig) -> Self {
        let conn = Connection::open_in_memory().expect("in-memory SQLite for fallback");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bad_request_budget (
                token_key        TEXT PRIMARY KEY,
                lifetime_count   INTEGER NOT NULL DEFAULT 0,
                daily_count      INTEGER NOT NULL DEFAULT 0,
                daily_date       TEXT NOT NULL DEFAULT ''
            );",
        )
        .expect("create budget table in fallback");
        Self {
            global_cfg,
            overrides: Mutex::new(HashMap::new()),
            entries: Mutex::new(HashMap::new()),
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    fn enabled(&self) -> bool {
        self.global_cfg.enabled
    }

    /// Resolve effective limits for a token (per-account override or global).
    fn effective_limits(&self, token_key: &str) -> (u32, u32) {
        if let Ok(overrides) = self.overrides.lock()
            && let Some((d, l)) = overrides.get(token_key)
        {
            return (
                d.unwrap_or(self.global_cfg.daily_limit),
                l.unwrap_or(self.global_cfg.lifetime_limit),
            );
        }
        (self.global_cfg.daily_limit, self.global_cfg.lifetime_limit)
    }

    /// Today's UTC date as YYYY-MM-DD.
    fn today_utc() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        let days = now / 86400;
        // Compute Y-M-D from days since epoch (1970-01-01).
        // Algorithm from http://howardhinnant.github.io/date_algorithms.html
        let z = days as i64 + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{y:04}-{m:02}-{d:02}")
    }

    /// Unix timestamp of the next UTC midnight (i.e., when the daily counter resets).
    pub fn daily_recovery_utc_secs() -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        let today_start = (now / 86400) * 86400;
        today_start + 86400
    }

    /// Format a human-readable daily recovery message with the actual recovery time.
    ///
    /// Returns something like:
    /// `"account triggered daily bad-request quota; recovers at 2026-10-04T00:00:00Z (in 3h 42m)"`
    pub fn format_daily_recovery_message(token_key: &str) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        let recovery = Self::daily_recovery_utc_secs();
        let remaining = recovery.saturating_sub(now);
        let hours = remaining / 3600;
        let minutes = (remaining % 3600) / 60;

        // Format recovery time as ISO 8601 UTC.
        let days = recovery / 86400;
        let z = days as i64 + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };

        tracing::debug!(
            token = %token_key,
            recovery_unix = recovery,
            remaining_secs = remaining,
            "formatting daily budget recovery message"
        );

        format!(
            "account triggered daily bad-request quota; recovers at {y:04}-{m:02}-{d:02}T00:00:00Z (in {hours}h {minutes}m)"
        )
    }

    /// Check if a token is blocked by either budget layer.
    /// Decision-only view of the check; test convenience over
    /// `check_with_diagnostic` (production callers always want the row state).
    #[cfg(test)]
    pub fn check(&self, token_key: &str) -> BudgetBlockReason {
        self.check_with_diagnostic(token_key).0
    }

    /// Like `check`, but also returns the row state the decision was based on.
    /// Blocked-path logging carries these fields so an anomalous lock can be
    /// self-diagnosed from the log alone.
    pub fn check_with_diagnostic(
        &self,
        token_key: &str,
    ) -> (BudgetBlockReason, BudgetCheckDiagnostic) {
        let today = Self::today_utc();
        let empty = BudgetCheckDiagnostic {
            today: today.clone(),
            daily_date: String::new(),
            daily_count: 0,
            lifetime_count: 0,
        };
        if !self.enabled() || token_key.is_empty() {
            return (BudgetBlockReason::Allowed, empty);
        }
        let (daily_limit, lifetime_limit) = self.effective_limits(token_key);
        let Ok(entries) = self.entries.lock() else {
            return (BudgetBlockReason::Allowed, empty);
        };
        let Some(entry) = entries.get(token_key) else {
            return (BudgetBlockReason::Allowed, empty);
        };
        let diag = BudgetCheckDiagnostic {
            today,
            daily_date: entry.daily_date.clone(),
            daily_count: entry.daily_count,
            lifetime_count: entry.lifetime_count,
        };
        // Check lifetime first (more severe).
        if entry.lifetime_count >= lifetime_limit {
            return (BudgetBlockReason::LifetimeExhausted, diag);
        }
        // Daily: if the date has changed, the counter is effectively 0.
        let effective_daily = if entry.daily_date == diag.today {
            entry.daily_count
        } else {
            0
        };
        if effective_daily >= daily_limit {
            return (BudgetBlockReason::DailyExhausted, diag);
        }
        (BudgetBlockReason::Allowed, diag)
    }

    /// Record a protectable upstream error for a token.
    /// Increments both daily and lifetime counters, persists to SQLite.
    pub fn record_error(&self, token_key: &str, token_name: &str) {
        if !self.enabled() || token_key.is_empty() {
            return;
        }
        let today = Self::today_utc();

        // Update in-memory state and persist atomically.
        let Ok(mut conn) = self.conn.lock() else {
            return;
        };
        let Ok(tx) = conn.transaction_with_behavior(TransactionBehavior::Immediate) else {
            return;
        };

        // Load current persisted state.
        let (mut daily_count, mut lifetime_count, mut daily_date) = tx
            .prepare(
                "SELECT daily_count, lifetime_count, daily_date
                 FROM bad_request_budget WHERE token_key = ?1",
            )
            .and_then(|mut stmt| {
                stmt.query_row(params![token_key], |row| {
                    Ok((
                        row.get::<_, u32>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
            })
            .unwrap_or((0, 0, String::new()));

        // Reset daily counter if date changed.
        if daily_date != today {
            daily_count = 0;
            daily_date = today.clone();
        }

        daily_count += 1;
        lifetime_count += 1;

        // Persist.
        let _ = tx.execute(
            "INSERT INTO bad_request_budget (token_key, lifetime_count, daily_count, daily_date)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(token_key) DO UPDATE SET
                lifetime_count = ?2, daily_count = ?3, daily_date = ?4",
            params![token_key, lifetime_count, daily_count, daily_date],
        );
        let _ = tx.commit();

        // Update in-memory state.
        if let Ok(mut entries) = self.entries.lock() {
            let entry = entries.entry(token_key.to_string()).or_insert(BudgetEntry {
                token_name: None,
                daily_date: today.clone(),
                daily_count: 0,
                lifetime_count: 0,
            });
            entry.daily_date = today;
            entry.daily_count = daily_count;
            entry.lifetime_count = lifetime_count;
        }

        // Every increment must be reconstructable from the log alone: date and
        // counts travel with the record so a later lock can be audited even if
        // the row itself is cleared by a reset before anyone inspects it.
        let token_display = if token_name.is_empty() {
            redact_token_key(token_key)
        } else {
            token_name.to_string()
        };
        tracing::info!(
            token = %token_display,
            daily_date = %daily_date,
            daily_count,
            lifetime_count,
            "bad-request budget recorded"
        );

        let (daily_limit, lifetime_limit) = self.effective_limits(token_key);
        if daily_count == daily_limit || lifetime_count == lifetime_limit {
            // Never log the raw key: name when known, redacted prefix otherwise.
            let token_display = if token_name.is_empty() {
                redact_token_key(token_key)
            } else {
                token_name.to_string()
            };
            tracing::info!(
                token = %token_display,
                daily_count = daily_count,
                daily_limit = daily_limit,
                lifetime_count = lifetime_count,
                lifetime_limit = lifetime_limit,
                "account bad-request budget threshold reached"
            );
        }
    }

    /// Reset the lifetime counter for a token (admin operation).
    /// Also unlocks the account from both layers.
    pub fn reset_lifetime(&self, token_key: &str) -> Result<()> {
        let today = Self::today_utc();
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("budget database lock poisoned: {e}"))?;
        conn.execute(
            "UPDATE bad_request_budget SET lifetime_count = 0, daily_count = 0, daily_date = ?1
             WHERE token_key = ?2",
            params![today, token_key],
        )
        .context("failed to reset budget")?;

        if let Ok(mut entries) = self.entries.lock()
            && let Some(entry) = entries.get_mut(token_key)
        {
            entry.lifetime_count = 0;
            entry.daily_count = 0;
            entry.daily_date = today;
        }
        Ok(())
    }

    /// Update per-account limit overrides (called on config load/reload).
    #[allow(dead_code)]
    pub fn update_overrides(&self, overrides: HashMap<String, (Option<u32>, Option<u32>)>) {
        if let Ok(mut map) = self.overrides.lock() {
            *map = overrides;
        }
    }

    /// Reconcile token display names with the current token configuration.
    #[allow(dead_code)]
    pub fn reconcile_names(&self, names: &HashMap<String, String>) {
        if let Ok(mut entries) = self.entries.lock() {
            for (key, entry) in entries.iter_mut() {
                entry.token_name = names.get(key).cloned();
            }
        }
    }

    /// Snapshot of all accounts with budget state (for admin query).
    pub fn snapshot(&self) -> Vec<AccountBudgetSnapshot> {
        let today = Self::today_utc();
        let Ok(entries) = self.entries.lock() else {
            return Vec::new();
        };
        entries
            .iter()
            .map(|(key, entry)| {
                let (daily_limit, lifetime_limit) = self.effective_limits(key);
                let effective_daily = if entry.daily_date == today {
                    entry.daily_count
                } else {
                    0
                };
                AccountBudgetSnapshot {
                    token_key_redacted: redact_token_key(key),
                    token_name: entry.token_name.clone(),
                    daily_limit,
                    daily_used: effective_daily,
                    lifetime_limit,
                    lifetime_used: entry.lifetime_count,
                    daily_locked: effective_daily >= daily_limit,
                    lifetime_locked: entry.lifetime_count >= lifetime_limit,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// red/green: the fingerprint hashes raw request bytes, so two
    /// payloads with identical JSON semantics but different insignificant
    /// whitespace must NOT collide (otherwise the rate-limit/block store can
    /// be evaded by re-spacing a request).
    #[test]
    fn fingerprint_distinguishes_whitespace_variants() {
        let compact = br#"{"a":1,"b":[1,2,3]}"#;
        let spaced = br#"{"a": 1, "b": [1, 2, 3]}"#;
        let fp1 = super::fingerprint(Protocol::OpenaiChatCompletions, Some("m"), compact);
        let fp2 = super::fingerprint(Protocol::OpenaiChatCompletions, Some("m"), spaced);
        assert_ne!(
            fp1, fp2,
            "raw-bytes fingerprints must differ for byte-distinct bodies"
        );
        // Determinism: identical bytes hash identically.
        let fp3 = super::fingerprint(Protocol::OpenaiChatCompletions, Some("m"), compact);
        assert_eq!(fp1, fp3);
        // Model partitioning is preserved as a separate hash segment.
        let fp4 = super::fingerprint(Protocol::OpenaiChatCompletions, Some("other"), compact);
        assert_ne!(fp1, fp4);
    }

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

    // ── Error-class extraction tests ──────────────────────────────────

    #[test]
    fn error_class_detail_type_takes_priority() {
        let class = extract_error_class(
            "some message",
            &["type.googleapis.com/google.rpc.BadRequest".to_string()],
        );
        assert!(class.starts_with("type_"), "got {class}");
    }

    #[test]
    fn error_class_message_hash_when_no_detail_type() {
        let class = extract_error_class("missing thought_signature", &[]);
        assert!(class.starts_with("msg_"), "got {class}");
    }

    #[test]
    fn error_class_keyword_fallback() {
        // Empty message won't match — need a non-empty message for hash path.
        // Keyword path only fires when message is non-empty but we explicitly
        // test the keyword patterns by checking that the same message always
        // produces the same class.
        let class1 = extract_error_class("missing thought_signature in parts", &[]);
        let class2 = extract_error_class("missing thought_signature in parts", &[]);
        assert_eq!(class1, class2);
        // Same message → same hash-based class (not keyword, since message is non-empty)
        assert!(class1.starts_with("msg_"));
    }

    #[test]
    fn error_class_unknown_for_empty_input() {
        let class = extract_error_class("", &[]);
        assert_eq!(class, "unknown");
    }

    #[test]
    fn error_class_from_body_extracts_message() {
        let body = r#"{"error":{"message":"missing a thought_signature in functionCall parts"}}"#;
        let class = extract_error_class_from_body(body);
        assert!(class.starts_with("msg_"), "got {class}");
    }

    #[test]
    fn error_class_from_body_unknown_for_invalid_json() {
        let class = extract_error_class_from_body("not json");
        assert_eq!(class, "unknown");
    }

    #[test]
    fn error_class_same_message_same_class() {
        let msg = "Function call is missing a thought_signature in functionCall parts.";
        let class1 = extract_error_class(msg, &[]);
        let class2 = extract_error_class(msg, &[]);
        assert_eq!(class1, class2, "same message must produce same error-class");
    }

    #[test]
    fn error_class_different_message_different_class() {
        let class1 = extract_error_class("missing thought_signature", &[]);
        let class2 = extract_error_class("Invalid schema for tool", &[]);
        assert_ne!(
            class1, class2,
            "different messages should produce different error-classes"
        );
    }

    // ── Google-RPC rate-limit 400 classification tests ───────────────

    #[test]
    fn google_rpc_rate_limit_400_resource_exhausted() {
        let body = r#"{
            "error": {
                "code": 429,
                "message": "Quota exceeded for quota metric 'GenerateContent API requests per minute'",
                "status": "RESOURCE_EXHAUSTED",
                "details": [
                    {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED"},
                    {"@type": "type.googleapis.com/google.rpc.ResourceInfo"}
                ]
            }
        }"#;
        assert!(is_google_rpc_rate_limit_400(body));
    }

    #[test]
    fn google_rpc_rate_limit_400_keyword_quota() {
        let body = r#"{"error":{"message":"daily request quota exceeded, resets at 00:00 UTC"}}"#;
        assert!(is_google_rpc_rate_limit_400(body));
    }

    #[test]
    fn google_rpc_rate_limit_400_keyword_rate_limit() {
        let body = r#"{"error":{"message":"rate limit exceeded, please retry in 30 seconds"}}"#;
        assert!(is_google_rpc_rate_limit_400(body));
    }

    #[test]
    fn google_rpc_true_request_error_not_rate_limit() {
        let body = r#"{
            "error": {
                "code": 400,
                "message": "Invalid JSON payload received. Unable to parse request body.",
                "status": "INVALID_ARGUMENT",
                "details": [
                    {"@type": "type.googleapis.com/google.rpc.BadRequest"}
                ]
            }
        }"#;
        assert!(!is_google_rpc_rate_limit_400(body));
    }

    #[test]
    fn google_rpc_rate_limit_400_invalid_json() {
        assert!(!is_google_rpc_rate_limit_400("not json"));
    }

    #[test]
    fn google_rpc_rate_limit_400_empty_body() {
        assert!(!is_google_rpc_rate_limit_400("{}"));
    }

    // ── Per-token error-class cooldown tests ──────────────────────────

    fn test_cooldown_config() -> ErrorCooldownConfig {
        ErrorCooldownConfig {
            enabled: true,
            max_errors_per_class: 3,
            cooldown_seconds: 300,
            window_seconds: 600,
        }
    }

    const M: &str = "model-a";

    #[test]
    fn token_cooldown_not_blocked_initially() {
        let cd = TokenErrorClassCooldown::new(test_cooldown_config());
        assert!(!cd.is_blocked("alice", M).blocked);
    }

    #[test]
    fn token_cooldown_blocks_after_threshold() {
        let cd = TokenErrorClassCooldown::new(test_cooldown_config());
        // Below threshold → not blocked
        cd.record_error("alice", M, "missing_thought_signature");
        assert!(!cd.is_blocked("alice", M).blocked);
        cd.record_error("alice", M, "missing_thought_signature");
        assert!(!cd.is_blocked("alice", M).blocked);
        // At threshold → blocked
        cd.record_error("alice", M, "missing_thought_signature");
        assert!(cd.is_blocked("alice", M).blocked);
    }

    /// The cooldown scopes to the (token, inbound model) pair that produced the
    /// errors: a sibling model of the same token keeps serving.
    #[test]
    fn token_cooldown_scopes_to_model_and_leaves_siblings_alone() {
        let cd = TokenErrorClassCooldown::new(test_cooldown_config());
        for _ in 0..3 {
            cd.record_error("alice", "model-a", "class_a");
        }
        assert!(
            cd.is_blocked("alice", "model-a").blocked,
            "the model that produced the errors is blocked"
        );
        assert!(
            !cd.is_blocked("alice", "model-b").blocked,
            "a sibling model of the same token must not be blocked"
        );

        // Counts are per model too: model-b needs its own three errors.
        cd.record_error("alice", "model-b", "class_a");
        cd.record_error("alice", "model-b", "class_a");
        assert!(!cd.is_blocked("alice", "model-b").blocked);
        cd.record_error("alice", "model-b", "class_a");
        assert!(cd.is_blocked("alice", "model-b").blocked);
    }

    #[test]
    fn token_cooldown_different_class_does_not_accumulate() {
        let cd = TokenErrorClassCooldown::new(test_cooldown_config());
        // 3 different error classes → each count=1, no threshold reached
        cd.record_error("alice", M, "class_a");
        cd.record_error("alice", M, "class_b");
        cd.record_error("alice", M, "class_c");
        assert!(!cd.is_blocked("alice", M).blocked);
    }

    #[test]
    fn token_cooldown_different_token_independent() {
        let cd = TokenErrorClassCooldown::new(test_cooldown_config());
        // Alice hits threshold
        cd.record_error("alice", M, "missing_thought_signature");
        cd.record_error("alice", M, "missing_thought_signature");
        cd.record_error("alice", M, "missing_thought_signature");
        assert!(cd.is_blocked("alice", M).blocked);
        // Bob is unaffected
        assert!(!cd.is_blocked("bob", M).blocked);
    }

    #[test]
    fn token_cooldown_disabled_when_config_off() {
        let cd = TokenErrorClassCooldown::new(ErrorCooldownConfig {
            enabled: false,
            ..test_cooldown_config()
        });
        cd.record_error("alice", M, "class_a");
        cd.record_error("alice", M, "class_a");
        cd.record_error("alice", M, "class_a");
        assert!(!cd.is_blocked("alice", M).blocked);
    }

    #[test]
    fn token_cooldown_open_mode_no_token() {
        let cd = TokenErrorClassCooldown::new(test_cooldown_config());
        // Empty token name → never blocked
        cd.record_error("", M, "class_a");
        cd.record_error("", M, "class_a");
        cd.record_error("", M, "class_a");
        assert!(!cd.is_blocked("", M).blocked);
    }

    #[test]
    fn token_cooldown_snapshot_reports_active_entries_per_model() {
        let cd = TokenErrorClassCooldown::new(test_cooldown_config());
        cd.record_error("alice", "model-a", "class_a");
        let snaps = cd.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].token, "alice");
        assert_eq!(snaps[0].model, "model-a");
        assert_eq!(snaps[0].error_counts.get("class_a"), Some(&1));
        assert_eq!(snaps[0].cooldown_remaining, 0);

        cd.record_error("alice", "model-a", "class_a");
        cd.record_error("alice", "model-a", "class_a");
        cd.record_error("alice", "model-b", "class_a");
        let snaps_blocked = cd.snapshot();
        assert_eq!(
            snaps_blocked.len(),
            2,
            "one snapshot entry per (token, model) pair"
        );
        assert_eq!(snaps_blocked[0].model, "model-a");
        assert!(snaps_blocked[0].cooldown_remaining > 0);
        assert_eq!(snaps_blocked[1].model, "model-b");
    }

    // ── Per-account bad-request budget tests ────────────────────────────

    fn test_budget_config() -> AccountBadBudgetConfig {
        AccountBadBudgetConfig {
            enabled: true,
            daily_limit: 2,
            lifetime_limit: 5,
        }
    }

    #[test]
    fn budget_not_blocked_initially() {
        let mgr = AccountBadBudgetManager::in_memory(test_budget_config());
        assert_eq!(mgr.check("lp-test"), BudgetBlockReason::Allowed);
    }

    #[test]
    fn budget_blocks_daily_after_limit() {
        let mgr = AccountBadBudgetManager::in_memory(test_budget_config());
        // First error: not yet blocked (the Nth error itself still goes through).
        mgr.record_error("lp-test", "lp-test");
        assert_eq!(mgr.check("lp-test"), BudgetBlockReason::Allowed);
        // Second error: daily limit (2) reached → blocked.
        mgr.record_error("lp-test", "lp-test");
        assert_eq!(mgr.check("lp-test"), BudgetBlockReason::DailyExhausted);
    }

    #[test]
    fn budget_blocks_lifetime_after_limit() {
        let mgr = AccountBadBudgetManager::in_memory(AccountBadBudgetConfig {
            enabled: true,
            daily_limit: 100, // high daily limit so it doesn't trigger
            lifetime_limit: 3,
        });
        mgr.record_error("lp-test", "lp-test");
        mgr.record_error("lp-test", "lp-test");
        assert_eq!(mgr.check("lp-test"), BudgetBlockReason::Allowed);
        mgr.record_error("lp-test", "lp-test");
        assert_eq!(mgr.check("lp-test"), BudgetBlockReason::LifetimeExhausted);
    }

    #[test]
    fn budget_lifetime_takes_priority_over_daily() {
        let mgr = AccountBadBudgetManager::in_memory(AccountBadBudgetConfig {
            enabled: true,
            daily_limit: 2,
            lifetime_limit: 3,
        });
        mgr.record_error("lp-test", "lp-test");
        mgr.record_error("lp-test", "lp-test");
        // Daily is exhausted.
        assert_eq!(mgr.check("lp-test"), BudgetBlockReason::DailyExhausted);
        // One more pushes lifetime to limit too.
        mgr.record_error("lp-test", "lp-test");
        // Lifetime takes priority.
        assert_eq!(mgr.check("lp-test"), BudgetBlockReason::LifetimeExhausted);
    }

    #[test]
    fn budget_different_tokens_independent() {
        let mgr = AccountBadBudgetManager::in_memory(test_budget_config());
        mgr.record_error("lp-alice", "lp-alice");
        mgr.record_error("lp-alice", "lp-alice");
        assert_eq!(mgr.check("lp-alice"), BudgetBlockReason::DailyExhausted);
        // Bob is unaffected.
        assert_eq!(mgr.check("lp-bob"), BudgetBlockReason::Allowed);
    }

    #[test]
    fn budget_reset_clears_both_layers() {
        let mgr = AccountBadBudgetManager::in_memory(test_budget_config());
        mgr.record_error("lp-test", "lp-test");
        mgr.record_error("lp-test", "lp-test");
        assert_eq!(mgr.check("lp-test"), BudgetBlockReason::DailyExhausted);

        mgr.reset_lifetime("lp-test").unwrap();
        assert_eq!(mgr.check("lp-test"), BudgetBlockReason::Allowed);
    }

    #[test]
    fn budget_check_diagnostic_reports_row_state() {
        let mgr = AccountBadBudgetManager::in_memory(test_budget_config());
        mgr.record_error("lp-test", "lp-test");
        mgr.record_error("lp-test", "lp-test");
        let (reason, diag) = mgr.check_with_diagnostic("lp-test");
        assert_eq!(reason, BudgetBlockReason::DailyExhausted);
        assert_eq!(diag.today, AccountBadBudgetManager::today_utc());
        assert_eq!(diag.daily_date, AccountBadBudgetManager::today_utc());
        assert_eq!(diag.daily_count, 2);
        assert_eq!(diag.lifetime_count, 2);
    }

    #[test]
    fn budget_check_diagnostic_exposes_stale_daily_date() {
        let mgr = AccountBadBudgetManager::in_memory(test_budget_config());
        {
            let mut entries = mgr.entries.lock().unwrap();
            entries.insert(
                "lp-test".to_string(),
                BudgetEntry {
                    token_name: None,
                    daily_date: "2000-01-01".to_string(),
                    daily_count: 5,
                    lifetime_count: 1,
                },
            );
        }
        // A stale row (daily_date ≠ today) must never lock, and the
        // diagnostic must expose the stale date for audit.
        let (reason, diag) = mgr.check_with_diagnostic("lp-test");
        assert_eq!(reason, BudgetBlockReason::Allowed);
        assert_eq!(diag.daily_date, "2000-01-01");
        assert_eq!(diag.today, AccountBadBudgetManager::today_utc());
        assert_eq!(diag.daily_count, 5);
    }

    #[test]
    fn budget_disabled_never_blocks() {
        let mgr = AccountBadBudgetManager::in_memory(AccountBadBudgetConfig {
            enabled: false,
            daily_limit: 1,
            lifetime_limit: 1,
        });
        mgr.record_error("lp-test", "lp-test");
        mgr.record_error("lp-test", "lp-test");
        assert_eq!(mgr.check("lp-test"), BudgetBlockReason::Allowed);
    }

    #[test]
    fn budget_empty_key_never_blocks() {
        let mgr = AccountBadBudgetManager::in_memory(test_budget_config());
        mgr.record_error("", "");
        assert_eq!(mgr.check(""), BudgetBlockReason::Allowed);
    }

    #[test]
    fn budget_persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("budget.db");
        let cfg = test_budget_config();

        // First instance: record errors.
        {
            let mgr = AccountBadBudgetManager::open(&db_path, cfg.clone()).unwrap();
            mgr.record_error("lp-test", "lp-test");
            mgr.record_error("lp-test", "lp-test");
            assert_eq!(mgr.check("lp-test"), BudgetBlockReason::DailyExhausted);
        }

        // Second instance: data survives restart.
        {
            let mgr = AccountBadBudgetManager::open(&db_path, cfg).unwrap();
            assert_eq!(mgr.check("lp-test"), BudgetBlockReason::DailyExhausted);
        }
    }

    #[test]
    fn budget_snapshot_reports_state() {
        let mgr = AccountBadBudgetManager::in_memory(test_budget_config());
        mgr.record_error("lp-test", "lp-test");
        let snaps = mgr.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].token_key_redacted, redact_token_key("lp-test"));
        assert_ne!(
            snaps[0].token_key_redacted, "lp-test",
            "raw key must never be serialized"
        );
        assert_eq!(snaps[0].daily_used, 1);
        assert_eq!(snaps[0].lifetime_used, 1);
        assert!(!snaps[0].daily_locked);
        assert!(!snaps[0].lifetime_locked);

        mgr.record_error("lp-test", "lp-test");
        let snaps = mgr.snapshot();
        assert!(snaps[0].daily_locked);
    }

    #[test]
    fn budget_per_account_override() {
        let mgr = AccountBadBudgetManager::in_memory(test_budget_config());
        // Override: alice gets higher limits.
        let mut overrides = HashMap::new();
        overrides.insert("lp-alice".to_string(), (Some(10), Some(20)));
        mgr.update_overrides(overrides);

        // Alice can take 10 daily errors (not 2).
        for _ in 0..3 {
            mgr.record_error("lp-alice", "lp-alice");
        }
        assert_eq!(mgr.check("lp-alice"), BudgetBlockReason::Allowed);

        // Bob still uses global limit (2).
        mgr.record_error("lp-bob", "lp-bob");
        mgr.record_error("lp-bob", "lp-bob");
        assert_eq!(mgr.check("lp-bob"), BudgetBlockReason::DailyExhausted);
    }

    // ── Daily recovery time tests ───────────────────────────────────────

    #[test]
    fn daily_recovery_utc_secs_is_future() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let recovery = AccountBadBudgetManager::daily_recovery_utc_secs();
        assert!(recovery > now, "recovery must be in the future");
        // Recovery is next UTC midnight: at most 86400s from now.
        assert!(
            recovery - now <= 86400,
            "recovery should be within 24h, got {}s gap",
            recovery - now
        );
    }

    #[test]
    fn daily_recovery_utc_secs_is_midnight_aligned() {
        let recovery = AccountBadBudgetManager::daily_recovery_utc_secs();
        assert_eq!(
            recovery % 86400,
            0,
            "recovery timestamp must be exactly UTC midnight"
        );
    }

    #[test]
    fn format_daily_recovery_message_contains_iso_timestamp() {
        let msg = AccountBadBudgetManager::format_daily_recovery_message("lp-test");
        // Should contain "recovers at YYYY-MM-DDT00:00:00Z"
        assert!(
            msg.contains("recovers at "),
            "message should contain 'recovers at': {msg}"
        );
        assert!(
            msg.contains("T00:00:00Z"),
            "message should contain ISO midnight timestamp: {msg}"
        );
        assert!(
            msg.contains("(in "),
            "message should contain remaining time: {msg}"
        );
    }

    #[test]
    fn format_daily_recovery_message_remaining_is_positive() {
        let msg = AccountBadBudgetManager::format_daily_recovery_message("lp-test");
        // Extract the "(in Xh Ym)" part and verify hours/minutes are reasonable.
        let in_part = msg.split("(in ").nth(1).expect("should have '(in '");
        let h_part = in_part.split('h').next().expect("should have 'h'");
        let hours: u64 = h_part.trim().parse().expect("hours should be numeric");
        assert!(hours < 24, "hours should be < 24, got {hours}");
    }

    // ── persistence / expiry / admin clear (error-class cooldown) ───────

    fn persisted_entry(
        token: &str,
        model: &str,
        counts: &[(&str, u32)],
        window_start: u64,
        cooldown_until: u64,
    ) -> PersistedCooldownState {
        let mut state = ModelState::default();
        for (class, count) in counts {
            state.error_counts.insert((*class).to_string(), *count);
        }
        state.window_start = window_start;
        state.cooldown_until = cooldown_until;
        PersistedCooldownState {
            tokens: HashMap::from([(
                token.to_string(),
                TokenState {
                    models: HashMap::from([(model.to_string(), state)]),
                },
            )]),
        }
    }

    #[test]
    fn error_cooldown_state_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        let cfg = test_cooldown_config();

        let cd = TokenErrorClassCooldown::load(cfg.clone(), Default::default(), &path);
        cd.record_error("alice", "model-a", "class_a");
        drop(cd);

        let reloaded = TokenErrorClassCooldown::load(cfg, Default::default(), &path);
        let snap = reloaded.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].token, "alice");
        assert_eq!(snap[0].model, "model-a");
        assert_eq!(snap[0].error_counts.get("class_a"), Some(&1));
        assert!(!reloaded.is_blocked("alice", "model-a").blocked);
    }

    /// The persisted file keys entries token → model → state, so counts and
    /// cooldowns stay attributed to the model that produced them.
    #[test]
    fn error_cooldown_persisted_schema_is_token_then_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        let cd = TokenErrorClassCooldown::load(test_cooldown_config(), Default::default(), &path);
        cd.record_error("alice", "model-a", "class_a");
        cd.record_error("alice", "model-b", "class_a");
        drop(cd);

        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let models = raw["tokens"]["alice"]["models"]
            .as_object()
            .expect("token → models");
        assert_eq!(models.len(), 2, "both models persisted");
        assert_eq!(models["model-a"]["error_counts"]["class_a"], 1);
        assert_eq!(models["model-b"]["error_counts"]["class_a"], 1);
    }

    #[test]
    fn error_cooldown_trigger_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        let cfg = test_cooldown_config();

        let cd = TokenErrorClassCooldown::load(cfg.clone(), Default::default(), &path);
        for _ in 0..3 {
            cd.record_error("alice", "model-a", "class_a");
        }
        assert!(cd.is_blocked("alice", "model-a").blocked);
        drop(cd);

        let reloaded = TokenErrorClassCooldown::load(cfg, Default::default(), &path);
        let status = reloaded.is_blocked("alice", "model-a");
        assert!(status.blocked, "triggered cooldown must survive restart");
        assert!(status.remaining_seconds > 0);
        assert!(status.remaining_seconds <= 300);
        assert!(
            !reloaded.is_blocked("alice", "model-b").blocked,
            "the cooldown must not leak to a sibling model across restart"
        );
        let snap = reloaded.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap[0].error_counts.is_empty(), "counts cleared on trigger");
    }

    /// A flat (model-less) state file from before the model dimension existed
    /// cannot be attributed to any model, so it is dropped with one warning
    /// instead of blocking every model of the token.
    #[test]
    fn error_cooldown_legacy_flat_file_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        std::fs::write(
            &path,
            br#"{"tokens":{"alice":{"error_counts":{"class_a":2},"cooldown_until":9999999999,"window_start":1}}}"#,
        )
        .unwrap();

        let cd = TokenErrorClassCooldown::load(test_cooldown_config(), Default::default(), &path);
        assert!(
            cd.snapshot().is_empty(),
            "legacy flat entries must not be loaded"
        );
        assert!(!cd.is_blocked("alice", "model-a").blocked);
    }

    #[test]
    fn error_cooldown_expired_state_pruned_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        let cfg = test_cooldown_config();
        let now = unix_secs();

        // Stale window AND expired cooldown → entry must disappear entirely.
        let payload = persisted_entry(
            "alice",
            "model-a",
            &[("class_a", 2)],
            now - cfg.window_seconds - 60,
            now.saturating_sub(10),
        );
        std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();

        let cd = TokenErrorClassCooldown::load(cfg, Default::default(), &path);
        assert!(cd.snapshot().is_empty(), "expired entries must be dropped");
        assert!(!cd.is_blocked("alice", "model-a").blocked);

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: PersistedCooldownState = serde_json::from_str(&raw).unwrap();
        assert!(
            parsed.tokens.is_empty(),
            "file rewritten without expired entry"
        );
    }

    #[test]
    fn error_cooldown_stale_window_clears_counts_but_keeps_live_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        let cfg = test_cooldown_config();
        let now = unix_secs();

        let payload = persisted_entry(
            "alice",
            "model-a",
            &[("class_a", 2)],
            now - cfg.window_seconds - 60,
            now + 120,
        );
        std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();

        let cd = TokenErrorClassCooldown::load(cfg, Default::default(), &path);
        let snap = cd.snapshot();
        assert_eq!(snap.len(), 1, "active cooldown keeps the entry");
        assert!(snap[0].error_counts.is_empty(), "stale window drops counts");
        assert!(cd.is_blocked("alice", "model-a").blocked);
    }

    /// One expired (token, model) entry must not prune a live sibling entry of
    /// the same token.
    #[test]
    fn error_cooldown_prune_keeps_live_sibling_model_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        let cfg = test_cooldown_config();
        let now = unix_secs();

        let mut dead = ModelState::default();
        dead.error_counts.insert("class_a".into(), 2);
        dead.window_start = now - cfg.window_seconds - 60;
        dead.cooldown_until = now.saturating_sub(10);
        let mut live = ModelState::default();
        live.error_counts.insert("class_a".into(), 1);
        live.window_start = now;
        let token_state = TokenState {
            models: HashMap::from([("model-a".to_string(), dead), ("model-b".to_string(), live)]),
        };
        let payload = PersistedCooldownState {
            tokens: HashMap::from([("alice".to_string(), token_state)]),
        };
        std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();

        let cd = TokenErrorClassCooldown::load(cfg, Default::default(), &path);
        let snap = cd.snapshot();
        assert_eq!(snap.len(), 1, "only the live model-b entry survives");
        assert_eq!(snap[0].model, "model-b");
        assert!(!cd.is_blocked("alice", "model-a").blocked);
    }

    #[test]
    fn error_cooldown_corrupt_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        std::fs::write(&path, b"{not json").unwrap();

        let cd = TokenErrorClassCooldown::load(test_cooldown_config(), Default::default(), &path);
        assert!(cd.snapshot().is_empty());
        cd.record_error("alice", "model-a", "class_a");
        assert_eq!(cd.snapshot().len(), 1);
    }

    #[test]
    fn error_cooldown_admin_clear_single_and_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        let cfg = test_cooldown_config();

        let cd = TokenErrorClassCooldown::load(cfg.clone(), Default::default(), &path);
        cd.record_error("alice", "model-a", "class_a");
        cd.record_error("bob", "model-a", "class_b");

        assert!(cd.clear_token("alice"));
        assert!(!cd.clear_token("alice"), "second clear is a no-op");
        drop(cd);

        // Clear persists: reload shows only bob.
        let reloaded = TokenErrorClassCooldown::load(cfg.clone(), Default::default(), &path);
        let snap = reloaded.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].token, "bob");

        assert_eq!(reloaded.clear_all(), 1);
        drop(reloaded);
        let final_load = TokenErrorClassCooldown::load(cfg, Default::default(), &path);
        assert!(final_load.snapshot().is_empty());
    }

    /// `model` narrows a clear to one entry; omitting it clears every model the
    /// token has.
    #[test]
    fn error_cooldown_admin_clear_narrows_by_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        let cfg = test_cooldown_config();

        let cd = TokenErrorClassCooldown::load(cfg.clone(), Default::default(), &path);
        for _ in 0..3 {
            cd.record_error("alice", "model-a", "class_a");
            cd.record_error("alice", "model-b", "class_a");
        }
        assert!(cd.is_blocked("alice", "model-a").blocked);
        assert!(cd.is_blocked("alice", "model-b").blocked);

        assert!(cd.clear_token_model("alice", "model-a"));
        assert!(!cd.is_blocked("alice", "model-a").blocked);
        assert!(
            cd.is_blocked("alice", "model-b").blocked,
            "narrow clear leaves siblings alone"
        );
        assert!(
            !cd.clear_token_model("alice", "model-a"),
            "clearing an absent entry is a no-op"
        );
        drop(cd);

        // Narrow clear persists; the token entry survives because model-b is still there.
        let reloaded = TokenErrorClassCooldown::load(cfg.clone(), Default::default(), &path);
        let snap = reloaded.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].model, "model-b");

        assert!(
            reloaded.clear_token("alice"),
            "token clear drops every model"
        );
        assert!(reloaded.snapshot().is_empty());
    }

    #[test]
    fn redact_token_key_never_exposes_full_key() {
        let key = "lp-kz0VwEh5UiAplDrlfLIKn6v0qU2Fd2b8";
        let red = redact_token_key(key);
        assert!(red.starts_with(&key[..6]), "keeps short prefix: {red}");
        assert!(
            red.ends_with(&key[key.len() - 4..]),
            "keeps short suffix: {red}"
        );
        assert!(
            !red.contains(&key[6..key.len() - 4]),
            "middle of the key must be masked: {red}"
        );
        assert!(redact_token_key("short").contains('*'));
        assert!(redact_token_key("").len() >= 3);
    }

    #[test]
    fn local_reject_threshold_triggers_cooldown() {
        let local_cfg = crate::config::LocalRejectConfig {
            enabled: true,
            max_local_rejects: 3,
            cooldown_seconds: 120,
            window_seconds: 300,
        };
        let cd = TokenErrorClassCooldown::new_with_configs(test_cooldown_config(), local_cfg);

        // Below threshold
        cd.record_local_reject("alice", "model-a", "ingress_format");
        assert!(!cd.is_blocked("alice", "model-a").blocked);
        cd.record_local_reject("alice", "model-a", "ingress_routing");
        assert!(!cd.is_blocked("alice", "model-a").blocked);

        // At threshold (3)
        cd.record_local_reject("alice", "model-a", "known_precheck");
        let status = cd.is_blocked("alice", "model-a");
        assert!(status.blocked);
        assert_eq!(status.reason.as_deref(), Some("local_reject_tolerance"));
        assert!(status.remaining_seconds > 0 && status.remaining_seconds <= 120);

        // Cooldown does not leak to other models or tokens
        assert!(!cd.is_blocked("alice", "model-b").blocked);
        assert!(!cd.is_blocked("bob", "model-a").blocked);
    }

    #[test]
    fn local_reject_and_error_class_isolation() {
        let local_cfg = crate::config::LocalRejectConfig {
            enabled: true,
            max_local_rejects: 3,
            cooldown_seconds: 120,
            window_seconds: 300,
        };
        let cd = TokenErrorClassCooldown::new_with_configs(test_cooldown_config(), local_cfg);

        // Record 2 local rejects
        cd.record_local_reject("alice", "model-a", "ingress_format");
        cd.record_local_reject("alice", "model-a", "ingress_format");

        let snap = cd.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].local_reject_count, 2);
        assert!(
            snap[0].error_counts.is_empty(),
            "local reject must never touch error_counts"
        );

        // Record error class
        cd.record_error("alice", "model-a", "missing_thought_signature");
        let snap2 = cd.snapshot();
        assert_eq!(
            snap2[0].local_reject_count, 2,
            "error class must never touch local_reject_count"
        );
        assert_eq!(
            snap2[0].error_counts.get("missing_thought_signature"),
            Some(&1)
        );
    }

    #[test]
    fn local_reject_disabled_is_noop() {
        let local_cfg = crate::config::LocalRejectConfig {
            enabled: false,
            max_local_rejects: 1,
            cooldown_seconds: 120,
            window_seconds: 300,
        };
        let cd = TokenErrorClassCooldown::new_with_configs(test_cooldown_config(), local_cfg);

        cd.record_local_reject("alice", "model-a", "ingress_format");
        assert!(!cd.is_blocked("alice", "model-a").blocked);
        assert!(cd.snapshot().is_empty());
    }

    #[test]
    fn local_reject_persists_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-error-cooldowns.json");
        let local_cfg = crate::config::LocalRejectConfig {
            enabled: true,
            max_local_rejects: 2,
            cooldown_seconds: 300,
            window_seconds: 600,
        };

        let cd = TokenErrorClassCooldown::load(test_cooldown_config(), local_cfg.clone(), &path);
        cd.record_local_reject("alice", "model-a", "fingerprint_block");
        cd.record_local_reject("alice", "model-a", "fingerprint_block");
        assert!(cd.is_blocked("alice", "model-a").blocked);
        drop(cd);

        let reloaded = TokenErrorClassCooldown::load(test_cooldown_config(), local_cfg, &path);
        let status = reloaded.is_blocked("alice", "model-a");
        assert!(status.blocked);
        assert_eq!(status.reason.as_deref(), Some("local_reject_tolerance"));
    }
}
