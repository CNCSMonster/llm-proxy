//! Token window usage tracking with SQLite persistence
//!
//! This module manages the `token_window_usage` table which tracks quota consumption
//! per token per time window. Key features:
//!
//! - **Lazy reset**: When a window expires, `used` is reset to 0 on next access
//! - **Atomic updates**: The quota admission step runs inside one `BEGIN IMMEDIATE`
//!   transaction so its read and its write cannot be interleaved by another writer
//! - **Direct-write strategy**: Quota data is written directly to SQLite (not memory-cached)
//!   because quota decisions require real-time persistence to prevent over-allocation
//! - **Group-scoped windows** : Model quota groups allow the same
//!   (window, quota_mode) pair to exist for different model scopes. The group_id
//!   is encoded into the storage key to keep the schema stable.

#![allow(dead_code)] // Used in later stages

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::config::{QuotaWindow, TokenConfig};

/// Current Unix timestamp in seconds
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// How long a quota write waits for another connection's write transaction
/// before giving up (SQLite `busy_timeout`, milliseconds).
///
/// Every quota writer opens its own connection to `tokens.db` — the server
/// request path, the admin handler, and each CLI invocation — so more than one
/// process can hold the same database. Without a timeout a colliding write
/// returns `SQLITE_BUSY` immediately, which on the admission path means a
/// refused request that has nothing to do with the caller's quota. The write
/// transactions here are a handful of indexed statements, so waiting is both
/// cheap and bounded by this value; the alternative (fail fast) would turn
/// transient contention into an unexplainable `503`.
const QUOTA_WRITE_TIMEOUT_MS: u32 = 1000;

/// Unit separator used to encode group_id into the storage key.
/// Chosen because it never appears in duration strings or model names.
const GROUP_KEY_SEP: char = '\x1f';

/// Build the composite storage key for a window row.
///
/// Legacy (no group): `"5h"`
/// Grouped: `"5h\x1fdeepseek-v3,mimo"` (group_id is sorted, comma-joined models)
fn composite_window_key(window: &str, group_id: &str) -> String {
    if group_id.is_empty() {
        window.to_string()
    } else {
        format!("{window}{GROUP_KEY_SEP}{group_id}")
    }
}

/// Parse a composite window key back into (window_duration, group_id).
fn parse_composite_window_key(key: &str) -> (&str, &str) {
    match key.split_once(GROUP_KEY_SEP) {
        Some((window, group_id)) => (window, group_id),
        None => (key, ""),
    }
}

/// A quota window annotated with its group identity .
///
/// When a token uses `model_quota_groups`, each group produces one or more
/// `GroupedQuotaWindow` values. The `group_id` is the canonical (sorted,
/// comma-joined) model list of the group — empty for the legacy flat path.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GroupedQuotaWindow {
    pub window: QuotaWindow,
    /// Canonical group identifier (sorted models joined by comma).
    /// Empty string for legacy (non-grouped) windows.
    pub group_id: String,
}

/// Token window usage record
#[derive(Debug, Clone)]
#[allow(dead_code)] // Used in later stages
pub struct WindowUsage {
    pub token_key: String,
    pub window_key: String,
    pub window_seconds: i64,
    pub quota_mode: String,
    pub quota_limit: i64,
    pub used: i64,
    pub window_start: i64,
    /// Group this window belongs to (empty for legacy).
    pub group_id: String,
}

/// Build grouped windows from a token config for a specific model.
///
/// If the token has `model_quota_groups`, returns windows from all groups
/// that match the model (AND semantics), each annotated with its group_id.
/// Otherwise, returns the legacy flat `quota_windows` with empty group_id.
#[allow(dead_code)]
pub fn grouped_windows_for_model(token: &TokenConfig, model: &str) -> Vec<GroupedQuotaWindow> {
    if !token.model_quota_groups.is_empty() {
        token
            .model_quota_groups
            .iter()
            .filter(|g| g.matches_model(model))
            .flat_map(|g| {
                let group_id = g.canonical_id();
                g.quota_windows.iter().map(move |w| GroupedQuotaWindow {
                    window: w.clone(),
                    group_id: group_id.clone(),
                })
            })
            .collect()
    } else {
        token
            .quota_windows
            .iter()
            .map(|w| GroupedQuotaWindow {
                window: w.clone(),
                group_id: String::new(),
            })
            .collect()
    }
}

/// Build all grouped windows from a token config (for initialization).
#[allow(dead_code)]
pub fn all_grouped_windows(token: &TokenConfig) -> Vec<GroupedQuotaWindow> {
    if !token.model_quota_groups.is_empty() {
        token
            .model_quota_groups
            .iter()
            .flat_map(|g| {
                let group_id = g.canonical_id();
                g.quota_windows.iter().map(move |w| GroupedQuotaWindow {
                    window: w.clone(),
                    group_id: group_id.clone(),
                })
            })
            .collect()
    } else {
        token
            .quota_windows
            .iter()
            .map(|w| GroupedQuotaWindow {
                window: w.clone(),
                group_id: String::new(),
            })
            .collect()
    }
}

/// Outcome of spending one request credit against a token's `requests` windows.
#[derive(Debug)]
pub enum RequestSlot {
    /// Every `requests`-mode window had room; all of them were incremented.
    Granted,
    /// At least one window was already at its limit, so **nothing** was
    /// incremented. Carries the windows that blocked the request.
    Exhausted(Vec<WindowUsage>),
}

/// Token usage store backed by SQLite.
///
/// Thread-safe: `Arc<Mutex<Connection>>` allows sharing across axum handlers.
/// SQLite serializes writes internally; the Mutex guards the connection handle.
#[derive(Clone)]
pub struct TokenUsageStore {
    conn: Arc<Mutex<Connection>>,
}

impl TokenUsageStore {
    /// Open or create the token usage store at the given database path.
    ///
    /// This database holds quota runtime state only (`token_window_usage`); the
    /// usage records and replay store live in separate files. Contention on it
    /// therefore comes exclusively from the quota writers themselves.
    pub fn open(db_path: &Path) -> Result<Self> {
        // Ensure the parent directory exists (fresh state dir / isolated runs).
        // 0700: keep quota state owner-only.
        if let Some(parent) = db_path.parent() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .with_context(|| {
                        format!(
                            "failed to create token usage database dir: {}",
                            parent.display()
                        )
                    })?;
            }
            #[cfg(not(unix))]
            {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "failed to create token usage database dir: {}",
                        parent.display()
                    )
                })?;
            }
        }
        let conn = Connection::open(db_path).with_context(|| {
            format!("failed to open token usage database: {}", db_path.display())
        })?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("failed to set WAL mode")?;

        conn.pragma_update(None, "busy_timeout", QUOTA_WRITE_TIMEOUT_MS)
            .context("failed to set busy_timeout")?;

        // Create table if not exists
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS token_window_usage (
                token_key      TEXT NOT NULL,
                group_id       TEXT NOT NULL DEFAULT '',
                window_key     TEXT NOT NULL,
                window_seconds INTEGER NOT NULL,
                quota_mode     TEXT NOT NULL,
                quota_limit    INTEGER NOT NULL,
                used           INTEGER DEFAULT 0,
                window_start   INTEGER NOT NULL,
                PRIMARY KEY (token_key, group_id, window_key, quota_mode)
            );
            "#,
        )
        .context("failed to create token_window_usage table")?;

        // Migrate from old schema if needed
        Self::migrate_schema_if_needed(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Acquire the connection lock. Recovers from poisoning so a panic in one
    /// handler does not permanently disable the store.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        match self.conn.lock() {
            Ok(guard) => Ok(guard),
            Err(poisoned) => Ok(poisoned.into_inner()),
        }
    }

    /// Migrate from old schema if needed.
    /// Handles migration from:
    /// - pre- schema: PRIMARY KEY (token_key, window_key)
    /// - schema: PRIMARY KEY (token_key, window_key, quota_mode) without group_id
    fn migrate_schema_if_needed(conn: &Connection) -> Result<()> {
        let table_sql: Option<String> = conn
            .prepare(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='token_window_usage'",
            )?
            .query_row([], |row| row.get(0))
            .optional()?;

        let Some(sql) = table_sql else {
            return Ok(());
        };

        if !sql.contains("group_id") {
            tracing::info!(
                "migrating token_window_usage schema to include group_id in PRIMARY KEY"
            );
            conn.execute_batch(
                r#"
                BEGIN TRANSACTION;
                CREATE TABLE token_window_usage_new (
                    token_key      TEXT NOT NULL,
                    group_id       TEXT NOT NULL DEFAULT '',
                    window_key     TEXT NOT NULL,
                    window_seconds INTEGER NOT NULL,
                    quota_mode     TEXT NOT NULL,
                    quota_limit    INTEGER NOT NULL,
                    used           INTEGER DEFAULT 0,
                    window_start   INTEGER NOT NULL,
                    PRIMARY KEY (token_key, group_id, window_key, quota_mode)
                );
                "#,
            )?;

            let has_quota_mode = sql.contains("quota_mode");
            if has_quota_mode {
                let mut select_stmt = conn.prepare(
                    "SELECT token_key, window_key, window_seconds, quota_mode, quota_limit, used, window_start FROM token_window_usage",
                )?;
                let mut insert_stmt = conn.prepare(
                    "INSERT OR IGNORE INTO token_window_usage_new (token_key, group_id, window_key, window_seconds, quota_mode, quota_limit, used, window_start)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                let rows = select_stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                })?;
                for r in rows {
                    let (
                        token_key,
                        raw_window_key,
                        window_seconds,
                        quota_mode,
                        quota_limit,
                        used,
                        window_start,
                    ) = r?;
                    let (window_key, group_id) = parse_composite_window_key(&raw_window_key);
                    insert_stmt.execute(params![
                        token_key,
                        group_id,
                        window_key,
                        window_seconds,
                        quota_mode,
                        quota_limit,
                        used,
                        window_start,
                    ])?;
                }
            } else {
                let mut select_stmt = conn.prepare(
                    "SELECT token_key, window_key, window_seconds, quota_limit, used, window_start FROM token_window_usage",
                )?;
                let mut insert_stmt = conn.prepare(
                    "INSERT OR IGNORE INTO token_window_usage_new (token_key, group_id, window_key, window_seconds, quota_mode, quota_limit, used, window_start)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                let rows = select_stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })?;
                for r in rows {
                    let (
                        token_key,
                        raw_window_key,
                        window_seconds,
                        quota_limit,
                        used,
                        window_start,
                    ) = r?;
                    let (window_key, group_id) = parse_composite_window_key(&raw_window_key);
                    insert_stmt.execute(params![
                        token_key,
                        group_id,
                        window_key,
                        window_seconds,
                        "requests",
                        quota_limit,
                        used,
                        window_start,
                    ])?;
                }
            }

            conn.execute_batch(
                r#"
                DROP TABLE token_window_usage;
                ALTER TABLE token_window_usage_new RENAME TO token_window_usage;
                COMMIT;
                "#,
            )
            .context("failed to migrate token_window_usage schema")?;
        }

        Ok(())
    }

    /// Initialize or reset a token's window usage based on its quota configuration.
    /// Called when a token is first seen or when its configuration changes.
    /// Legacy path: no group scoping (group_id = "").
    pub fn init_token_windows(&self, token_key: &str, windows: &[QuotaWindow]) -> Result<()> {
        let grouped: Vec<GroupedQuotaWindow> = windows
            .iter()
            .map(|w| GroupedQuotaWindow {
                window: w.clone(),
                group_id: String::new(),
            })
            .collect();
        self.init_grouped_windows(token_key, &grouped)
    }

    /// Initialize grouped quota windows .
    pub fn init_grouped_windows(
        &self,
        token_key: &str,
        windows: &[GroupedQuotaWindow],
    ) -> Result<()> {
        let conn = self.lock()?;
        Self::upsert_grouped_window_rows(&conn, token_key, windows, now_unix())
    }

    /// Upsert one row per configured window on `conn`.
    fn upsert_window_rows(
        conn: &Connection,
        token_key: &str,
        windows: &[QuotaWindow],
        now: i64,
    ) -> Result<()> {
        let grouped: Vec<GroupedQuotaWindow> = windows
            .iter()
            .map(|w| GroupedQuotaWindow {
                window: w.clone(),
                group_id: String::new(),
            })
            .collect();
        Self::upsert_grouped_window_rows(conn, token_key, &grouped, now)
    }

    /// Upsert grouped window rows. The composite primary key (token_key, group_id, window_key, quota_mode)
    /// ensures that different groups with the same (window, quota_mode) get separate rows.
    fn upsert_grouped_window_rows(
        conn: &Connection,
        token_key: &str,
        windows: &[GroupedQuotaWindow],
        now: i64,
    ) -> Result<()> {
        for gw in windows {
            let window_seconds = super::config::parse_duration(&gw.window.window)
                .with_context(|| format!("invalid window duration: {}", gw.window.window))?;

            conn.execute(
                r#"
                INSERT INTO token_window_usage (token_key, group_id, window_key, window_seconds, quota_mode, quota_limit, used, window_start)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)
                ON CONFLICT(token_key, group_id, window_key, quota_mode) DO UPDATE SET
                    window_seconds = ?4,
                    quota_limit = ?6
                "#,
                params![
                    token_key,
                    gw.group_id,
                    gw.window.window,
                    window_seconds,
                    gw.window.quota_mode,
                    gw.window.quota_limit,
                    now,
                ],
            ).with_context(|| format!("failed to init window {} (group '{}') for token {}", gw.window.window, gw.group_id, token_key))?;
        }

        Ok(())
    }

    /// Add usage to a token's `tokens`-mode windows after a successful request.
    /// Legacy path: charges all tokens-mode windows for `token_key`.
    pub fn add_usage(&self, token_key: &str, input_tokens: i64, output_tokens: i64) -> Result<()> {
        let now = now_unix();
        let total_tokens = input_tokens.saturating_add(output_tokens);
        let conn = self.lock()?;

        conn.execute(
            r#"
            UPDATE token_window_usage
            SET window_start = CASE
                    WHEN quota_mode = 'tokens' AND ?2 > window_start + window_seconds THEN ?2
                    ELSE window_start
                END,
                used = CASE
                    WHEN quota_mode <> 'tokens' THEN used
                    WHEN ?2 > window_start + window_seconds THEN ?1
                    ELSE used + ?1
                END
            WHERE token_key = ?3
            "#,
            params![total_tokens, now, token_key],
        )?;

        Ok(())
    }

    /// Add usage to a token's quota windows for specific matching groups .
    /// AND semantics: all matching groups are charged simultaneously, while non-matching
    /// groups are isolated and untouched.
    ///
    /// `credits_per_1m` is the conversion rate for `"credits"`-mode windows
    /// . Ignored when no credits windows are present.
    pub fn add_usage_grouped(
        &self,
        token_key: &str,
        windows: &[GroupedQuotaWindow],
        input_tokens: i64,
        output_tokens: i64,
        credits_per_1m: f64,
    ) -> Result<()> {
        if windows.is_empty() {
            return Ok(());
        }
        let now = now_unix();
        let conn = self.lock()?;

        for gw in windows {
            let amount = match gw.window.quota_mode.as_str() {
                "credits" => super::dimension::credits_charge(input_tokens, credits_per_1m),
                other => {
                    match super::dimension::charge_amount(other, input_tokens, output_tokens) {
                        Some(n) => n,
                        None => continue,
                    }
                }
            };
            conn.execute(
                r#"
                UPDATE token_window_usage
                SET window_start = CASE
                        WHEN ?2 > window_start + window_seconds THEN ?2
                        ELSE window_start
                    END,
                    used = CASE
                        WHEN ?2 > window_start + window_seconds THEN ?1
                        ELSE used + ?1
                    END
                WHERE token_key = ?3 AND group_id = ?4 AND window_key = ?5 AND quota_mode = ?6
                "#,
                params![
                    amount,
                    now,
                    token_key,
                    gw.group_id,
                    gw.window.window,
                    gw.window.quota_mode
                ],
            )?;
        }

        Ok(())
    }

    /// Legacy single-group/flat-window admission entry point.
    pub fn reserve_request_slot(
        &self,
        token_key: &str,
        windows: &[QuotaWindow],
    ) -> Result<RequestSlot> {
        let grouped: Vec<GroupedQuotaWindow> = windows
            .iter()
            .map(|w| GroupedQuotaWindow {
                window: w.clone(),
                group_id: String::new(),
            })
            .collect();
        self.reserve_request_slot_grouped(token_key, &grouped)
    }

    /// Group-aware version of [`Self::reserve_request_slot`] .
    ///
    /// Only checks and charges the windows belonging to the specified groups.
    /// This is the AND-semantics entry point: when a request's model matches
    /// multiple groups, all their windows are checked atomically.
    pub fn reserve_request_slot_grouped(
        &self,
        token_key: &str,
        windows: &[GroupedQuotaWindow],
    ) -> Result<RequestSlot> {
        if windows.is_empty() {
            return Ok(RequestSlot::Granted);
        }

        let now = now_unix();
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to open quota admission transaction")?;

        // Upsert the grouped window rows
        Self::upsert_grouped_window_rows(&tx, token_key, windows, now)?;

        // Expire windows that have elapsed
        tx.execute(
            r#"
            UPDATE token_window_usage
            SET used = 0, window_start = ?2
            WHERE token_key = ?1 AND ?2 > window_start + window_seconds
            "#,
            params![token_key, now],
        )?;

        // Check exhaustion only for the windows matching this model's groups
        let mut exhausted = Vec::new();
        for gw in windows {
            let row = tx.query_row(
                "SELECT window_seconds, quota_limit, used, window_start
                 FROM token_window_usage
                 WHERE token_key = ?1 AND group_id = ?2 AND window_key = ?3 AND quota_mode = ?4",
                params![
                    token_key,
                    gw.group_id,
                    gw.window.window,
                    gw.window.quota_mode
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            );
            if let Ok((window_seconds, quota_limit, used, window_start)) = row {
                let usage = WindowUsage {
                    token_key: token_key.to_string(),
                    window_key: gw.window.window.clone(),
                    window_seconds,
                    quota_mode: gw.window.quota_mode.clone(),
                    quota_limit,
                    used,
                    window_start,
                    group_id: gw.group_id.clone(),
                };
                if usage.used >= usage.quota_limit {
                    exhausted.push(usage);
                }
            }
        }

        if exhausted.is_empty() {
            // Increment windows that require admission-time charging
            for gw in windows {
                if let Some(inc) = super::dimension::reserve_increment(&gw.window.quota_mode) {
                    tx.execute(
                        "UPDATE token_window_usage SET used = used + ?4
                         WHERE token_key = ?1 AND group_id = ?2 AND window_key = ?3 AND quota_mode = ?5",
                        params![token_key, gw.group_id, gw.window.window, inc, gw.window.quota_mode],
                    )?;
                }
            }
        }

        tx.commit().context("failed to commit quota admission")?;

        Ok(if exhausted.is_empty() {
            RequestSlot::Granted
        } else {
            RequestSlot::Exhausted(exhausted)
        })
    }

    /// Drop the window rows the current configuration no longer lists.
    pub fn reconcile_windows(&self, token_key: &str, windows: &[QuotaWindow]) -> Result<usize> {
        let grouped: Vec<GroupedQuotaWindow> = windows
            .iter()
            .map(|w| GroupedQuotaWindow {
                window: w.clone(),
                group_id: String::new(),
            })
            .collect();
        self.reconcile_grouped_windows(token_key, &grouped)
    }

    /// Reconcile stored windows against grouped windows .
    pub fn reconcile_grouped_windows(
        &self,
        token_key: &str,
        windows: &[GroupedQuotaWindow],
    ) -> Result<usize> {
        let conn = self.lock()?;
        let stored: Vec<(String, String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT group_id, window_key, quota_mode FROM token_window_usage WHERE token_key = ?1",
            )?;
            let rows = stmt.query_map(params![token_key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<Result<_, _>>()?
        };

        let configured: Vec<(String, String, String)> = windows
            .iter()
            .map(|w| {
                (
                    w.group_id.clone(),
                    w.window.window.clone(),
                    w.window.quota_mode.clone(),
                )
            })
            .collect();

        let stale: Vec<(String, String, String)> = stored
            .into_iter()
            .filter(|(gid, wk, qm)| {
                !configured
                    .iter()
                    .any(|(cgid, cwk, cqm)| cgid == gid && cwk == wk && cqm == qm)
            })
            .collect();

        for (gid, wk, qm) in &stale {
            conn.execute(
                "DELETE FROM token_window_usage WHERE token_key = ?1 AND group_id = ?2 AND window_key = ?3 AND quota_mode = ?4",
                params![token_key, gid, wk, qm],
            )?;
        }

        Ok(stale.len())
    }

    /// : Reconcile stored windows against all configured token windows (both flat and grouped).
    pub fn reconcile_all_windows(&self, token: &TokenConfig) -> Result<usize> {
        let all_windows = all_grouped_windows(token);
        self.reconcile_grouped_windows(&token.key, &all_windows)
    }

    /// Drop every usage row belonging to a token the configuration no longer
    /// lists — the token dimension of the same convergence applied to
    /// windows.
    ///
    /// `token delete` removes the entry from `tokens.toml` and then best-effort
    /// deletes the rows; when that second step fails (store unopenable, SQLite
    /// busy) the rows survive with nothing left to point at them. They are inert
    /// — every read filters on a configured key — but "storage converges to the
    /// configuration at load boundaries" is the invariant that makes such leaks
    /// survivable at all, and a stale row keeps a deleted token's key occupying
    /// quota state forever.
    ///
    /// Call this at configuration-load boundaries (startup, reload) only, like
    /// [`Self::reconcile_windows`]. Returns how many rows went away.
    pub fn purge_unconfigured_token_rows(&self, configured_keys: &[String]) -> Result<usize> {
        let conn = self.lock()?;
        let stored: Vec<String> = {
            let mut stmt = conn.prepare("SELECT DISTINCT token_key FROM token_window_usage")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };

        let orphans: Vec<String> = stored
            .into_iter()
            .filter(|key| !configured_keys.iter().any(|c| c == key))
            .collect();

        for key in &orphans {
            conn.execute(
                "DELETE FROM token_window_usage WHERE token_key = ?1",
                params![key],
            )?;
        }

        Ok(orphans.len())
    }

    /// Get all usage records for a token (for CLI display).
    pub fn get_token_usage(&self, token_key: &str) -> Result<Vec<WindowUsage>> {
        let now = now_unix();
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT group_id, window_key, window_seconds, quota_mode, quota_limit, used, window_start
             FROM token_window_usage WHERE token_key = ?1",
        )?;

        let rows = stmt.query_map(params![token_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;

        let mut usages = Vec::new();
        for row in rows {
            let (
                group_id,
                window_key,
                window_seconds,
                quota_mode_str,
                quota_limit,
                used,
                window_start,
            ) = row?;

            let quota_mode = quota_mode_str;

            // Apply lazy reset for display
            let effective_used = if now > window_start + window_seconds {
                0
            } else {
                used
            };

            usages.push(WindowUsage {
                token_key: token_key.to_string(),
                window_key,
                window_seconds,
                quota_mode,
                quota_limit,
                used: effective_used,
                window_start,
                group_id,
            });
        }

        Ok(usages)
    }

    /// Delete all usage records for a token (called when token is deleted).
    pub fn delete_token_usage(&self, token_key: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM token_window_usage WHERE token_key = ?1",
            params![token_key],
        )?;
        Ok(())
    }

    /// Reset all usage counters for a token (manual reset via CLI).
    pub fn reset_token_usage(&self, token_key: &str) -> Result<()> {
        let now = now_unix();
        let conn = self.lock()?;
        conn.execute(
            "UPDATE token_window_usage SET used = 0, window_start = ?1 WHERE token_key = ?2",
            params![now, token_key],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::ModelQuotaGroup;
    use super::*;
    use tempfile::tempdir;

    fn test_store() -> TokenUsageStore {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        TokenUsageStore::open(&db_path).unwrap()
    }

    #[test]
    fn test_open_creates_table() {
        let store = test_store();
        // Verify table exists by querying it
        let count: i64 = store
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM token_window_usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_init_token_windows_starts_under_budget() {
        let store = test_store();
        let windows = vec![QuotaWindow {
            window: "1h".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: 10,
        }];

        store
            .init_token_windows("lp-test1234567890", &windows)
            .unwrap();

        let usages = store.get_token_usage("lp-test1234567890").unwrap();
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].used, 0);
        assert_eq!(usages[0].quota_limit, 10);

        // A freshly initialised window admits the first request.
        assert!(matches!(
            store
                .reserve_request_slot("lp-test1234567890", &windows)
                .unwrap(),
            RequestSlot::Granted
        ));
    }

    #[test]
    fn test_add_usage_and_quota_exhaustion() {
        let store = test_store();
        let windows = vec![QuotaWindow {
            window: "1h".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: 2,
        }];
        store
            .init_token_windows("lp-test1234567890", &windows)
            .unwrap();

        // A finished request no longer charges a `requests` window — the credit
        // is spent at admission — so add_usage must leave it alone.
        store.add_usage("lp-test1234567890", 0, 0).unwrap();
        assert_eq!(
            store.get_token_usage("lp-test1234567890").unwrap()[0].used,
            0
        );

        // Admission is what spends the credit, and it stops at the limit.
        for expected in 1..=2 {
            assert!(matches!(
                store
                    .reserve_request_slot("lp-test1234567890", &windows)
                    .unwrap(),
                RequestSlot::Granted
            ));
            assert_eq!(
                store.get_token_usage("lp-test1234567890").unwrap()[0].used,
                expected
            );
        }
        assert!(matches!(
            store
                .reserve_request_slot("lp-test1234567890", &windows)
                .unwrap(),
            RequestSlot::Exhausted(_)
        ));
    }

    #[test]
    fn test_token_mode_counts_tokens() {
        let store = test_store();
        let windows = vec![QuotaWindow {
            window: "1d".to_string(),
            quota_mode: "tokens".to_string(),
            quota_limit: 1000,
        }];

        store
            .init_token_windows("lp-test1234567890", &windows)
            .unwrap();

        // Add 500 input + 300 output = 800 tokens
        store.add_usage("lp-test1234567890", 500, 300).unwrap();
        assert_eq!(
            store.get_token_usage("lp-test1234567890").unwrap()[0].used,
            800
        );

        // Under budget: admission passes, and a `tokens` window is never
        // incremented at the gate (the credit is the token cost, not a count).
        assert!(matches!(
            store
                .reserve_request_slot("lp-test1234567890", &windows)
                .unwrap(),
            RequestSlot::Granted
        ));
        assert_eq!(
            store.get_token_usage("lp-test1234567890").unwrap()[0].used,
            800
        );

        // Add 300 more tokens (total 1100 > 1000 limit) — the budget is now
        // spent, so admission refuses and names the `tokens` window (spec §5.1).
        store.add_usage("lp-test1234567890", 200, 100).unwrap();
        let binding = match store
            .reserve_request_slot("lp-test1234567890", &windows)
            .unwrap()
        {
            RequestSlot::Exhausted(windows) => windows,
            RequestSlot::Granted => panic!("tokens window over budget must refuse"),
        };
        assert_eq!(binding.len(), 1);
        assert_eq!(binding[0].quota_mode, "tokens");
        assert_eq!(binding[0].used, 1100);
    }

    #[test]
    fn test_requests_and_tokens_windows_both_gate() {
        // The atomic reservation is requests-only, but a tokens window
        // coexisting on the same token must still refuse the request (§5.1).
        let store = test_store();
        let windows = vec![
            QuotaWindow {
                window: "1h".to_string(),
                quota_mode: "requests".to_string(),
                quota_limit: 5,
            },
            QuotaWindow {
                window: "1d".to_string(),
                quota_mode: "tokens".to_string(),
                quota_limit: 100,
            },
        ];
        store
            .init_token_windows("lp-test1234567890", &windows)
            .unwrap();

        // Spend the tokens budget; the requests window is untouched (used=0).
        store.add_usage("lp-test1234567890", 60, 60).unwrap();

        // Admission refuses on the tokens window even though the requests
        // window has room — and, all-or-nothing, it must NOT spend the
        // requests credit for a request it refused.
        let binding = match store
            .reserve_request_slot("lp-test1234567890", &windows)
            .unwrap()
        {
            RequestSlot::Exhausted(windows) => windows,
            RequestSlot::Granted => panic!("a spent tokens window must gate the request"),
        };
        assert_eq!(binding[0].quota_mode, "tokens");

        let usages = store.get_token_usage("lp-test1234567890").unwrap();
        let requests = usages.iter().find(|u| u.window_key == "1h").unwrap();
        assert_eq!(requests.used, 0, "refused request must not spend a credit");
    }

    #[test]
    fn test_delete_token_usage() {
        let store = test_store();
        let windows = vec![QuotaWindow {
            window: "1h".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: 10,
        }];

        store
            .init_token_windows("lp-test1234567890", &windows)
            .unwrap();
        store.add_usage("lp-test1234567890", 0, 0).unwrap();

        store.delete_token_usage("lp-test1234567890").unwrap();

        let usages = store.get_token_usage("lp-test1234567890").unwrap();
        assert!(usages.is_empty());
    }

    #[test]
    fn test_reset_token_usage() {
        let store = test_store();
        let windows = vec![QuotaWindow {
            window: "1h".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: 10,
        }];

        store
            .init_token_windows("lp-test1234567890", &windows)
            .unwrap();
        // Credits are spent at admission, so drive the counter the
        // way the proxy does.
        store
            .reserve_request_slot("lp-test1234567890", &windows)
            .unwrap();
        store
            .reserve_request_slot("lp-test1234567890", &windows)
            .unwrap();

        let usages = store.get_token_usage("lp-test1234567890").unwrap();
        assert_eq!(usages[0].used, 2);

        store.reset_token_usage("lp-test1234567890").unwrap();

        let usages = store.get_token_usage("lp-test1234567890").unwrap();
        assert_eq!(usages[0].used, 0);
    }

    #[test]
    fn test_multiple_windows() {
        let store = test_store();
        let windows = vec![
            QuotaWindow {
                window: "1h".to_string(),
                quota_mode: "requests".to_string(),
                quota_limit: 5,
            },
            QuotaWindow {
                window: "1d".to_string(),
                quota_mode: "tokens".to_string(),
                quota_limit: 10000,
            },
        ];

        store
            .init_token_windows("lp-test1234567890", &windows)
            .unwrap();

        let usages = store.get_token_usage("lp-test1234567890").unwrap();
        assert_eq!(usages.len(), 2);

        // Find each window
        let hourly = usages.iter().find(|u| u.window_key == "1h").unwrap();
        let daily = usages.iter().find(|u| u.window_key == "1d").unwrap();

        assert_eq!(hourly.quota_mode, "requests");
        assert_eq!(hourly.quota_limit, 5);
        assert_eq!(daily.quota_mode, "tokens");
        assert_eq!(daily.quota_limit, 10000);
    }

    #[test]
    fn test_same_window_duration_different_quota_mode() {
        // Same window duration (e.g., "1w") with different quota_mode should not conflict.
        // This tests the composite PRIMARY KEY (token_key, window_key, quota_mode).
        let store = test_store();
        let windows = vec![
            QuotaWindow {
                window: "1w".to_string(),
                quota_mode: "requests".to_string(),
                quota_limit: 8000,
            },
            QuotaWindow {
                window: "1w".to_string(),
                quota_mode: "tokens".to_string(),
                quota_limit: 1000000000,
            },
        ];

        store
            .init_token_windows("lp-test1234567890", &windows)
            .unwrap();

        let usages = store.get_token_usage("lp-test1234567890").unwrap();
        assert_eq!(usages.len(), 2, "both windows should exist independently");

        // Verify both windows are present with correct limits
        let requests = usages.iter().find(|u| u.quota_mode == "requests").unwrap();
        let tokens = usages.iter().find(|u| u.quota_mode == "tokens").unwrap();

        assert_eq!(requests.window_key, "1w");
        assert_eq!(requests.quota_limit, 8000);
        assert_eq!(tokens.window_key, "1w");
        assert_eq!(tokens.quota_limit, 1000000000);

        // Spend some requests quota
        store
            .reserve_request_slot("lp-test1234567890", &windows)
            .unwrap();

        // Spend some tokens quota
        store.add_usage("lp-test1234567890", 500, 500).unwrap();

        let usages = store.get_token_usage("lp-test1234567890").unwrap();
        let requests = usages.iter().find(|u| u.quota_mode == "requests").unwrap();
        let tokens = usages.iter().find(|u| u.quota_mode == "tokens").unwrap();

        assert_eq!(requests.used, 1, "requests window should have 1 used");
        assert_eq!(tokens.used, 1000, "tokens window should have 1000 used");
    }

    /// Rows belonging to a token the configuration no longer lists are dropped,
    /// while a still-configured token keeps its counter.
    #[test]
    fn purge_unconfigured_token_rows_drops_rows_of_removed_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenUsageStore::open(&dir.path().join("tokens.db")).unwrap();
        let windows = vec![QuotaWindow {
            window: "1h".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: 10,
        }];
        let kept = "lp-kept00000000000000000000000000";
        let removed = "lp-removed0000000000000000000000";

        for key in [kept, removed] {
            store.reserve_request_slot(key, &windows).unwrap();
        }

        assert_eq!(
            store
                .purge_unconfigured_token_rows(&[kept.to_string()])
                .unwrap(),
            1,
            "only the unconfigured token's rows go away"
        );
        assert!(
            store.get_token_usage(removed).unwrap().is_empty(),
            "ghost rows must not survive the boundary"
        );
        assert_eq!(store.get_token_usage(kept).unwrap()[0].used, 1);
    }

    /// 回归基线（fail-open）：一扇窗建行失败时，修复前错误被 `let _ =` 吞掉，
    /// 于是那扇窗在库里根本不存在，SELECT 只看到另一扇尚有余量的窗 ⇒ 请求被
    /// `Granted`，同时已插入的那半行留在库里（三条语句各自 autocommit，没有东西
    /// 把它们绑成一个整体）。"配额检查失败"与"这个 token 没有配额"不得塌缩成同一个结果。
    #[test]
    fn admission_fails_closed_when_a_configured_window_cannot_be_created() {
        let dir = tempdir().unwrap();
        let store = TokenUsageStore::open(&dir.path().join("tokens.db")).unwrap();
        let key = "lp-failopen00000000000000000000";
        let windows = vec![
            QuotaWindow {
                window: "1h".to_string(),
                quota_mode: "requests".to_string(),
                quota_limit: 10,
            },
            // 无法解析的时长代表"这扇窗建不出行"的任意成因（SQL 错误、BUSY、…）
            QuotaWindow {
                window: "bogus".to_string(),
                quota_mode: "tokens".to_string(),
                quota_limit: 5,
            },
        ];

        let err = store
            .reserve_request_slot(key, &windows)
            .expect_err("建不出窗口行时既不得放行，也不得当成没有配额");
        assert!(
            err.to_string().contains("bogus"),
            "错误必须指明是哪扇窗无法评估：{err}"
        );
        assert!(
            store.get_token_usage(key).unwrap().is_empty(),
            "失败的准入不得留下半个事务的残留行"
        );
    }

    /// 配额库的每个写者都各自开连接（server 请求路径、admin handler、每次 CLI 调用），
    /// 因此跨连接冲突是常态而非异常：连接必须等待而不是立刻 `SQLITE_BUSY`。
    #[test]
    fn store_connections_wait_for_a_concurrent_writer() {
        let dir = tempdir().unwrap();
        let store = TokenUsageStore::open(&dir.path().join("tokens.db")).unwrap();
        let timeout_ms: i64 = store
            .lock()
            .unwrap()
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            timeout_ms,
            i64::from(QUOTA_WRITE_TIMEOUT_MS),
            "没有 busy_timeout，配额写会在别人的写事务上立刻失败"
        );
    }

    /// 回归基线（跨连接硬不变量）：`used <= quota_limit` 不能只靠进程内的
    /// `Mutex<Connection>`。多个各持连接的写入者竞争同一份 `tokens.db`（等价于
    /// server 与 CLI 并发，或两个实例共享 state dir）时，放行数必须恰好等于最终
    /// `used`，且不得超过限额。
    ///
    /// 只断言安全方向：某个写入者若耗尽 `busy_timeout`，它会拿到 `Err`（fail-closed
    /// 的 `503`），于是放行数低于限额——那是正确行为，不是回归。
    #[test]
    fn concurrent_connections_never_over_admit() {
        const LIMIT: i64 = 50;
        const THREADS: usize = 6;
        const PER_THREAD: usize = 40;

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("tokens.db");
        let windows = vec![QuotaWindow {
            window: "1h".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: LIMIT,
        }];
        let key = "lp-race000000000000000000000000";
        let granted = std::sync::atomic::AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let store = TokenUsageStore::open(&db_path).unwrap();
                let windows = &windows;
                let granted = &granted;
                scope.spawn(move || {
                    let store = store;
                    for _ in 0..PER_THREAD {
                        if matches!(
                            store.reserve_request_slot(key, windows),
                            Ok(RequestSlot::Granted)
                        ) {
                            granted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        let used = TokenUsageStore::open(&db_path)
            .unwrap()
            .get_token_usage(key)
            .unwrap()
            .into_iter()
            .map(|u| u.used)
            .sum::<i64>();
        assert_eq!(
            used,
            i64::try_from(granted.load(std::sync::atomic::Ordering::Relaxed)).unwrap(),
            "每次放行必须恰好记一次"
        );
        assert!(
            used <= LIMIT,
            "并发准入把 used 推过了限额：{used} > {LIMIT}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn token_usage_dir_created_with_restrictive_permissions() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("nested").join("tokens.db");
        let _store = super::TokenUsageStore::open(&db_path).unwrap();
        let parent = db_path.parent().unwrap();
        let meta = std::fs::metadata(parent).unwrap();
        let mode = meta.mode() & 0o777;
        assert_eq!(
            mode & 0o022,
            0,
            "token usage dir must not be group/other writable (got {:o})",
            mode
        );
    }

    // ── : Composite key encoding ─────────────────────────

    #[test]
    fn composite_key_with_group() {
        let w = QuotaWindow {
            window: "5h".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: 10,
        };
        let key = super::composite_window_key(&w.window, "deepseek-v3,mimo");
        assert_eq!(key, "5h\x1fdeepseek-v3,mimo");
        let (win, gid) = super::parse_composite_window_key(&key);
        assert_eq!(win, "5h");
        assert_eq!(gid, "deepseek-v3,mimo");
    }

    #[test]
    fn composite_key_empty_group_is_legacy() {
        let w = QuotaWindow {
            window: "1d".to_string(),
            quota_mode: "tokens".to_string(),
            quota_limit: 10,
        };
        let key = super::composite_window_key(&w.window, "");
        assert_eq!(key, "1d");
        let (win, gid) = super::parse_composite_window_key(&key);
        assert_eq!(win, "1d");
        assert_eq!(gid, "");
    }

    #[test]
    fn parse_legacy_key_returns_empty_group() {
        let (win, gid) = super::parse_composite_window_key("5h");
        assert_eq!(win, "5h");
        assert_eq!(gid, "");
    }

    // ── : grouped_windows_for_model ──────────────────────

    #[test]
    fn grouped_windows_for_model_legacy_only() {
        let token = TokenConfig {
            key: "k".into(),
            name: "n".into(),
            allowed_models: vec!["gpt-4o".into()],
            quota_windows: vec![QuotaWindow {
                window: "5h".into(),
                quota_mode: "requests".to_string(),
                quota_limit: 10,
            }],
            model_quota_groups: vec![],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        };
        let gw = super::grouped_windows_for_model(&token, "gpt-4o");
        assert_eq!(gw.len(), 1);
        assert_eq!(gw[0].group_id, "");
        assert_eq!(gw[0].window.window, "5h");
    }

    #[test]
    fn grouped_windows_for_model_matching_group() {
        let token = TokenConfig {
            key: "k".into(),
            name: "n".into(),
            allowed_models: vec!["gpt-4o".into()],
            quota_windows: vec![],
            model_quota_groups: vec![
                ModelQuotaGroup {
                    models: vec!["gpt-4o".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "1h".into(),
                        quota_mode: "requests".to_string(),
                        quota_limit: 10,
                    }],
                },
                ModelQuotaGroup {
                    models: vec!["deepseek-v3".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "5h".into(),
                        quota_mode: "tokens".to_string(),
                        quota_limit: 1000,
                    }],
                },
            ],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        };
        let gw = super::grouped_windows_for_model(&token, "gpt-4o");
        assert_eq!(gw.len(), 1);
        assert_eq!(gw[0].group_id, "gpt-4o");
        assert_eq!(gw[0].window.window, "1h");
    }

    #[test]
    fn grouped_windows_for_model_multiple_groups_match() {
        let token = TokenConfig {
            key: "k".into(),
            name: "n".into(),
            allowed_models: vec!["gpt-4o".into()],
            quota_windows: vec![],
            model_quota_groups: vec![
                ModelQuotaGroup {
                    models: vec!["gpt-4o".into(), "deepseek-v3".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "1h".into(),
                        quota_mode: "requests".to_string(),
                        quota_limit: 10,
                    }],
                },
                ModelQuotaGroup {
                    models: vec!["gpt-4o".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "5h".into(),
                        quota_mode: "tokens".to_string(),
                        quota_limit: 1000,
                    }],
                },
            ],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        };
        let gw = super::grouped_windows_for_model(&token, "gpt-4o");
        assert_eq!(gw.len(), 2);
        // canonical_id sorts models alphabetically
        assert_eq!(gw[0].group_id, "deepseek-v3,gpt-4o");
        assert_eq!(gw[1].group_id, "gpt-4o");
    }

    #[test]
    fn grouped_windows_for_model_no_match() {
        let token = TokenConfig {
            key: "k".into(),
            name: "n".into(),
            allowed_models: vec!["gpt-4o".into()],
            quota_windows: vec![],
            model_quota_groups: vec![ModelQuotaGroup {
                models: vec!["deepseek-v3".into()],
                quota_windows: vec![QuotaWindow {
                    window: "5h".into(),
                    quota_mode: "requests".to_string(),
                    quota_limit: 10,
                }],
            }],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        };
        let gw = super::grouped_windows_for_model(&token, "gpt-4o");
        assert!(gw.is_empty());
    }

    // ── : reserve_request_slot_grouped ───────────────────

    #[test]
    fn reserve_grouped_only_checks_matching_group() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let store = TokenUsageStore::open(&db_path).unwrap();
        let token = TokenConfig {
            key: "k".into(),
            name: "n".into(),
            allowed_models: vec!["gpt-4o".into()],
            quota_windows: vec![],
            model_quota_groups: vec![
                ModelQuotaGroup {
                    models: vec!["gpt-4o".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "1h".into(),
                        quota_mode: "requests".to_string(),
                        quota_limit: 3,
                    }],
                },
                ModelQuotaGroup {
                    models: vec!["deepseek-v3".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "1h".into(),
                        quota_mode: "requests".to_string(),
                        quota_limit: 3,
                    }],
                },
            ],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        };
        // Exhaust gpt-4o group's limit
        for _ in 0..3 {
            let gw = super::grouped_windows_for_model(&token, "gpt-4o");
            let slot = store.reserve_request_slot_grouped("k", &gw).unwrap();
            assert!(matches!(slot, RequestSlot::Granted));
        }
        // gpt-4o group should be exhausted
        let gw = super::grouped_windows_for_model(&token, "gpt-4o");
        let slot = store.reserve_request_slot_grouped("k", &gw).unwrap();
        assert!(matches!(slot, RequestSlot::Exhausted(_)));
        // deepseek-v3 group should still have capacity
        let gw = super::grouped_windows_for_model(&token, "deepseek-v3");
        let slot = store.reserve_request_slot_grouped("k", &gw).unwrap();
        assert!(
            matches!(slot, RequestSlot::Granted),
            "deepseek-v3 group should be independent"
        );
    }

    #[test]
    fn reserve_grouped_legacy_still_works() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let store = TokenUsageStore::open(&db_path).unwrap();
        let token = TokenConfig {
            key: "k".into(),
            name: "n".into(),
            allowed_models: vec!["gpt-4o".into()],
            quota_windows: vec![QuotaWindow {
                window: "1h".into(),
                quota_mode: "requests".to_string(),
                quota_limit: 3,
            }],
            model_quota_groups: vec![],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        };
        for _ in 0..3 {
            let gw = super::grouped_windows_for_model(&token, "gpt-4o");
            let slot = store.reserve_request_slot_grouped("k", &gw).unwrap();
            assert!(matches!(slot, RequestSlot::Granted));
        }
        let gw = super::grouped_windows_for_model(&token, "gpt-4o");
        let slot = store.reserve_request_slot_grouped("k", &gw).unwrap();
        assert!(matches!(slot, RequestSlot::Exhausted(_)));
    }

    #[test]
    fn get_token_usage_parses_grouped_rows() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let store = TokenUsageStore::open(&db_path).unwrap();
        let token = TokenConfig {
            key: "k".into(),
            name: "n".into(),
            allowed_models: vec!["gpt-4o".into()],
            quota_windows: vec![],
            model_quota_groups: vec![
                ModelQuotaGroup {
                    models: vec!["gpt-4o".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "1h".into(),
                        quota_mode: "requests".to_string(),
                        quota_limit: 10,
                    }],
                },
                ModelQuotaGroup {
                    models: vec!["deepseek-v3".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "5h".into(),
                        quota_mode: "tokens".to_string(),
                        quota_limit: 1000,
                    }],
                },
            ],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        };
        // Initialize all grouped windows first
        let all_gw = super::all_grouped_windows(&token);
        store.init_grouped_windows("k", &all_gw).unwrap();

        // Make a request for gpt-4o
        let gw = super::grouped_windows_for_model(&token, "gpt-4o");
        let slot = store.reserve_request_slot_grouped("k", &gw).unwrap();
        assert!(matches!(slot, RequestSlot::Granted));

        let usage = store.get_token_usage("k").unwrap();
        // Should have 2 windows (one per group)
        assert_eq!(usage.len(), 2);
        // Find the gpt-4o group window
        let gpt_window = usage
            .iter()
            .find(|w| w.group_id == "gpt-4o")
            .expect("should have gpt-4o group window");
        assert_eq!(gpt_window.used, 1);
        // Find the deepseek-v3 group window
        let ds_window = usage
            .iter()
            .find(|w| w.group_id == "deepseek-v3")
            .expect("should have deepseek-v3 group window");
        assert_eq!(ds_window.used, 0);
    }

    // ── : SQLite Schema Migration & AND Semantics Tests ───

    #[test]
    fn test_schema_migration_from_composite_primary_key_without_group_id() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let now = now_unix();

        // Manually create the old schema (: PRIMARY KEY (token_key, window_key, quota_mode) without group_id)
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE token_window_usage (
                    token_key      TEXT NOT NULL,
                    window_key     TEXT NOT NULL,
                    window_seconds INTEGER NOT NULL,
                    quota_mode     TEXT NOT NULL,
                    quota_limit    INTEGER NOT NULL,
                    used           INTEGER DEFAULT 0,
                    window_start   INTEGER NOT NULL,
                    PRIMARY KEY (token_key, window_key, quota_mode)
                );
                "#,
            )
            .unwrap();
            conn.execute(
                "INSERT INTO token_window_usage VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params!["k1", "1h", 3600, "requests", 10, 3, now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO token_window_usage VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    "k1",
                    "5h\x1fdeepseek-v3,mimo",
                    18000,
                    "tokens",
                    10000000,
                    500,
                    now
                ],
            )
            .unwrap();
        }

        // Open with TokenUsageStore, which triggers migrate_schema_if_needed
        let store = TokenUsageStore::open(&db_path).unwrap();

        // Verify table schema now has group_id in PRIMARY KEY
        let sql: String = store
            .lock()
            .unwrap()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='token_window_usage'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("group_id"),
            "table must contain group_id column"
        );
        assert!(
            sql.contains("PRIMARY KEY (token_key, group_id, window_key, quota_mode)"),
            "table must have updated composite PRIMARY KEY"
        );

        // Verify data was preserved and parsed correctly
        let usages = store.get_token_usage("k1").unwrap();
        assert_eq!(usages.len(), 2);

        let legacy_w = usages
            .iter()
            .find(|u| u.window_key == "1h")
            .expect("should find 1h window");
        assert_eq!(legacy_w.group_id, "");
        assert_eq!(legacy_w.used, 3);
        assert_eq!(legacy_w.quota_mode, "requests");

        let grouped_w = usages
            .iter()
            .find(|u| u.window_key == "5h")
            .expect("should find 5h window");
        assert_eq!(grouped_w.group_id, "deepseek-v3,mimo");
        assert_eq!(grouped_w.used, 500);
        assert_eq!(grouped_w.quota_mode, "tokens");
    }

    #[test]
    fn test_schema_migration_from_pre_schema() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let now = now_unix();

        // Manually create the pre- schema: PRIMARY KEY (token_key, window_key)
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE token_window_usage (
                    token_key      TEXT NOT NULL,
                    window_key     TEXT NOT NULL,
                    window_seconds INTEGER NOT NULL,
                    quota_limit    INTEGER NOT NULL,
                    used           INTEGER DEFAULT 0,
                    window_start   INTEGER NOT NULL,
                    PRIMARY KEY (token_key, window_key)
                );
                "#,
            )
            .unwrap();
            conn.execute(
                "INSERT INTO token_window_usage VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!["k2", "1d", 86400, 50, 5, now],
            )
            .unwrap();
        }

        let store = TokenUsageStore::open(&db_path).unwrap();
        let usages = store.get_token_usage("k2").unwrap();
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].group_id, "");
        assert_eq!(usages[0].window_key, "1d");
        assert_eq!(usages[0].quota_mode, "requests");
        assert_eq!(usages[0].used, 5);
    }

    #[test]
    fn test_add_usage_grouped_and_semantics_and_isolation() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let store = TokenUsageStore::open(&db_path).unwrap();

        let token = TokenConfig {
            key: "k_iso".into(),
            name: "iso".into(),
            allowed_models: vec!["deepseek-v3".into(), "mimo".into(), "gpt-4o".into()],
            quota_windows: vec![],
            model_quota_groups: vec![
                ModelQuotaGroup {
                    models: vec!["deepseek-v3".into(), "mimo".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "5h".into(),
                        quota_mode: "tokens".to_string(),
                        quota_limit: 10_000,
                    }],
                },
                ModelQuotaGroup {
                    models: vec!["deepseek-v3".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "5h".into(),
                        quota_mode: "tokens".to_string(),
                        quota_limit: 5_000,
                    }],
                },
                ModelQuotaGroup {
                    models: vec!["gpt-4o".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "5h".into(),
                        quota_mode: "tokens".to_string(),
                        quota_limit: 8_000,
                    }],
                },
            ],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        };

        // Initialize all windows
        let all_gw = super::all_grouped_windows(&token);
        store.init_grouped_windows("k_iso", &all_gw).unwrap();

        // Charge for deepseek-v3: matches Group 1 (deepseek-v3,mimo) AND Group 2 (deepseek-v3)
        let matching_ds = super::grouped_windows_for_model(&token, "deepseek-v3");
        assert_eq!(matching_ds.len(), 2);

        store
            .add_usage_grouped("k_iso", &matching_ds, 1000, 200, 100.0)
            .unwrap();

        let usages = store.get_token_usage("k_iso").unwrap();
        let g1 = usages
            .iter()
            .find(|u| u.group_id == "deepseek-v3,mimo")
            .unwrap();
        assert_eq!(g1.used, 1200, "Group 1 must be charged");

        let g2 = usages.iter().find(|u| u.group_id == "deepseek-v3").unwrap();
        assert_eq!(g2.used, 1200, "Group 2 must be charged");

        let g3 = usages.iter().find(|u| u.group_id == "gpt-4o").unwrap();
        assert_eq!(g3.used, 0, "Group 3 must remain untouched (isolation)");
    }

    #[test]
    fn test_reserve_request_slot_grouped_and_semantics_multiple_matching_groups() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let store = TokenUsageStore::open(&db_path).unwrap();

        // Token with overlapping groups:
        // Group 1: models = ["deepseek-v3", "mimo"], limit = 10 requests
        // Group 2: models = ["deepseek-v3"], limit = 2 requests
        // Group 3: models = ["gpt-4o"], limit = 5 requests
        let token = TokenConfig {
            key: "k_and".into(),
            name: "and_test".into(),
            allowed_models: vec!["deepseek-v3".into(), "mimo".into(), "gpt-4o".into()],
            quota_windows: vec![],
            model_quota_groups: vec![
                ModelQuotaGroup {
                    models: vec!["deepseek-v3".into(), "mimo".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "1h".into(),
                        quota_mode: "requests".to_string(),
                        quota_limit: 10,
                    }],
                },
                ModelQuotaGroup {
                    models: vec!["deepseek-v3".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "1h".into(),
                        quota_mode: "requests".to_string(),
                        quota_limit: 2,
                    }],
                },
                ModelQuotaGroup {
                    models: vec!["gpt-4o".into()],
                    quota_windows: vec![QuotaWindow {
                        window: "1h".into(),
                        quota_mode: "requests".to_string(),
                        quota_limit: 5,
                    }],
                },
            ],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        };

        // Initialize all windows
        let all_gw = super::all_grouped_windows(&token);
        store.init_grouped_windows("k_and", &all_gw).unwrap();

        // 1st request for deepseek-v3: hits Group 1 AND Group 2
        let ds_gw = super::grouped_windows_for_model(&token, "deepseek-v3");
        assert_eq!(ds_gw.len(), 2);
        assert!(matches!(
            store.reserve_request_slot_grouped("k_and", &ds_gw).unwrap(),
            RequestSlot::Granted
        ));

        // 2nd request for deepseek-v3: hits Group 1 AND Group 2
        assert!(matches!(
            store.reserve_request_slot_grouped("k_and", &ds_gw).unwrap(),
            RequestSlot::Granted
        ));

        // Verify used counts: Group 1 used = 2, Group 2 used = 2, Group 3 used = 0
        let usages = store.get_token_usage("k_and").unwrap();
        let g1 = usages
            .iter()
            .find(|u| u.group_id == "deepseek-v3,mimo")
            .unwrap();
        assert_eq!(g1.used, 2);
        let g2 = usages.iter().find(|u| u.group_id == "deepseek-v3").unwrap();
        assert_eq!(g2.used, 2);
        let g3 = usages.iter().find(|u| u.group_id == "gpt-4o").unwrap();
        assert_eq!(g3.used, 0);

        // 3rd request for deepseek-v3: Group 2 is at capacity (2/2) -> Exhausted!
        let slot = store.reserve_request_slot_grouped("k_and", &ds_gw).unwrap();
        match slot {
            RequestSlot::Exhausted(ex) => {
                assert_eq!(ex.len(), 1);
                assert_eq!(ex[0].group_id, "deepseek-v3");
            }
            RequestSlot::Granted => panic!("expected exhausted due to group 2"),
        }

        // Ensure Group 1 was NOT incremented on the failed request (used remains 2)
        let usages = store.get_token_usage("k_and").unwrap();
        let g1 = usages
            .iter()
            .find(|u| u.group_id == "deepseek-v3,mimo")
            .unwrap();
        assert_eq!(
            g1.used, 2,
            "Group 1 must not increment when admission is exhausted"
        );

        // Request for mimo: only matches Group 1 (limit 10, current used 2) -> Granted!
        let mimo_gw = super::grouped_windows_for_model(&token, "mimo");
        assert_eq!(mimo_gw.len(), 1);
        assert!(matches!(
            store
                .reserve_request_slot_grouped("k_and", &mimo_gw)
                .unwrap(),
            RequestSlot::Granted
        ));
        let usages = store.get_token_usage("k_and").unwrap();
        let g1 = usages
            .iter()
            .find(|u| u.group_id == "deepseek-v3,mimo")
            .unwrap();
        assert_eq!(g1.used, 3);

        // Request for gpt-4o: matches Group 3 (isolated) -> Granted!
        let gpt_gw = super::grouped_windows_for_model(&token, "gpt-4o");
        assert_eq!(gpt_gw.len(), 1);
        assert!(matches!(
            store
                .reserve_request_slot_grouped("k_and", &gpt_gw)
                .unwrap(),
            RequestSlot::Granted
        ));
        let usages = store.get_token_usage("k_and").unwrap();
        let g3 = usages.iter().find(|u| u.group_id == "gpt-4o").unwrap();
        assert_eq!(g3.used, 1);
    }

    // ── : SQLite stores the original string ───────────────

    /// The `quota_mode` column in SQLite must store the exact string from the
    /// configuration — not a re-serialised enum variant.
    #[test]
    fn sqlite_stores_original_quota_mode_string() {
        let store = test_store();
        let windows = vec![
            QuotaWindow {
                window: "1h".to_string(),
                quota_mode: "tokens".to_string(),
                quota_limit: 1000,
            },
            QuotaWindow {
                window: "1d".to_string(),
                quota_mode: "requests".to_string(),
                quota_limit: 50,
            },
        ];
        store
            .init_token_windows("lp-sqlite-mode-test", &windows)
            .unwrap();

        // Read the raw SQLite rows to verify the stored strings
        let conn = store.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT quota_mode FROM token_window_usage WHERE token_key = ?1 ORDER BY window_key")
            .unwrap();
        let modes: Vec<String> = stmt
            .query_map(params!["lp-sqlite-mode-test"], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(modes, vec!["requests", "tokens"]);
    }

    // ── : credits dimension ──────────────────────────────────

    /// Helper: create a token config with specified quota windows in a single group.
    fn token_with_windows(
        windows: Vec<(&str, &str, i64)>, // (window, mode, limit)
    ) -> super::super::config::TokenConfig {
        use super::super::config::{ModelQuotaGroup, QuotaWindow, TokenConfig};
        let quota_windows: Vec<QuotaWindow> = windows
            .into_iter()
            .map(|(w, m, l)| QuotaWindow {
                window: w.to_string(),
                quota_mode: m.to_string(),
                quota_limit: l,
            })
            .collect();
        TokenConfig {
            key: "lp-credits-test-token1".into(),
            name: "credits-test".into(),
            allowed_models: vec![],
            quota_windows: vec![],
            model_quota_groups: vec![ModelQuotaGroup {
                models: vec![], // matches all models
                quota_windows,
            }],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        }
    }

    #[test]
    fn credits_dimension_charges_correctly() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let store = super::TokenUsageStore::open(&db_path).unwrap();

        let token = token_with_windows(vec![("1h", "credits", 1000)]);
        let all_gw = super::all_grouped_windows(&token);
        store
            .init_grouped_windows("lp-credits-test-token1", &all_gw)
            .unwrap();

        let matching = super::grouped_windows_for_model(&token, "gpt-4o");
        assert_eq!(matching.len(), 1);

        // 500K weighted tokens × rate 100.0 = 50 credits
        store
            .add_usage_grouped("lp-credits-test-token1", &matching, 500_000, 0, 100.0)
            .unwrap();

        let usages = store.get_token_usage("lp-credits-test-token1").unwrap();
        let credits_window = usages.iter().find(|u| u.quota_mode == "credits").unwrap();
        assert_eq!(credits_window.used, 50);
    }

    #[test]
    fn credits_and_tokens_windows_independently_counted() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let store = super::TokenUsageStore::open(&db_path).unwrap();

        let token = token_with_windows(vec![("1h", "tokens", 100000), ("1h", "credits", 1000)]);
        let all_gw = super::all_grouped_windows(&token);
        store
            .init_grouped_windows("lp-credits-test-token1", &all_gw)
            .unwrap();

        let matching = super::grouped_windows_for_model(&token, "gpt-4o");
        assert_eq!(matching.len(), 2);

        // 1000 weighted tokens: tokens window += 1000, credits window += ceil(1000/1M × 100) = 1
        store
            .add_usage_grouped("lp-credits-test-token1", &matching, 1000, 0, 100.0)
            .unwrap();

        let usages = store.get_token_usage("lp-credits-test-token1").unwrap();
        let tokens_window = usages.iter().find(|u| u.quota_mode == "tokens").unwrap();
        let credits_window = usages.iter().find(|u| u.quota_mode == "credits").unwrap();
        assert_eq!(
            tokens_window.used, 1000,
            "tokens window should count weighted tokens"
        );
        assert_eq!(
            credits_window.used, 1,
            "credits window should convert via rate"
        );
    }

    #[test]
    fn credits_multi_window_composition() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let store = super::TokenUsageStore::open(&db_path).unwrap();

        let token = token_with_windows(vec![("1h", "credits", 500), ("24h", "credits", 5000)]);
        let all_gw = super::all_grouped_windows(&token);
        store
            .init_grouped_windows("lp-credits-test-token1", &all_gw)
            .unwrap();

        let matching = super::grouped_windows_for_model(&token, "gpt-4o");
        assert_eq!(matching.len(), 2);

        // 1M weighted tokens × rate 100.0 = 100 credits
        store
            .add_usage_grouped("lp-credits-test-token1", &matching, 1_000_000, 0, 100.0)
            .unwrap();

        let usages = store.get_token_usage("lp-credits-test-token1").unwrap();
        let hourly = usages
            .iter()
            .find(|u| u.quota_mode == "credits" && u.window_key == "1h")
            .unwrap();
        let daily = usages
            .iter()
            .find(|u| u.quota_mode == "credits" && u.window_key == "24h")
            .unwrap();
        assert_eq!(hourly.used, 100, "hourly credits window charged");
        assert_eq!(
            daily.used, 100,
            "daily credits window charged independently"
        );
    }

    #[test]
    fn credits_zero_tokens_zero_credits() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let store = super::TokenUsageStore::open(&db_path).unwrap();

        let token = token_with_windows(vec![("1h", "credits", 1000)]);
        let all_gw = super::all_grouped_windows(&token);
        store
            .init_grouped_windows("lp-credits-test-token1", &all_gw)
            .unwrap();

        let matching = super::grouped_windows_for_model(&token, "gpt-4o");
        store
            .add_usage_grouped("lp-credits-test-token1", &matching, 0, 0, 100.0)
            .unwrap();

        let usages = store.get_token_usage("lp-credits-test-token1").unwrap();
        let credits_window = usages.iter().find(|u| u.quota_mode == "credits").unwrap();
        assert_eq!(credits_window.used, 0);
    }

    #[test]
    fn credits_custom_rate() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("tokens.db");
        let store = super::TokenUsageStore::open(&db_path).unwrap();

        let token = token_with_windows(vec![("1h", "credits", 10000)]);
        let all_gw = super::all_grouped_windows(&token);
        store
            .init_grouped_windows("lp-credits-test-token1", &all_gw)
            .unwrap();

        let matching = super::grouped_windows_for_model(&token, "gpt-4o");

        // 1M weighted tokens × rate 500.0 = 500 credits
        store
            .add_usage_grouped("lp-credits-test-token1", &matching, 1_000_000, 0, 500.0)
            .unwrap();

        let usages = store.get_token_usage("lp-credits-test-token1").unwrap();
        let credits_window = usages.iter().find(|u| u.quota_mode == "credits").unwrap();
        assert_eq!(credits_window.used, 500);
    }
}
