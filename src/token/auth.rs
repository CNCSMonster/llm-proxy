//! Token authentication middleware
//!
//! Validates incoming requests against the token configuration.
//! Only applied to proxy routes, not admin routes.
//!
//! Flow (per spec §4.2):
//! 1. Extract key from Authorization or x-api-key header
//! 2. If no tokens configured → open mode, allow all
//! 3. Validate key exists → 401 if not
//! 4. Check expiration → 401 if expired
//! 5. Check quota windows → 429 if exhausted
//! 6. Check model access → 403 if not allowed
//! 7. Inject TokenContext into request extensions

#![allow(dead_code)] // TokenContext fields + with_usage_store consumed by handlers

use std::path::Path;
use std::sync::{Arc, RwLock};

use axum::extract::Request;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use super::config::TokensConfig;
use super::ratio::RatioCalculator;
use super::usage::TokenUsageStore;

/// Token context injected into request extensions after successful auth
#[derive(Debug, Clone)]
pub struct TokenContext {
    pub key: String,
    pub name: String,
    /// Allowed models (empty = allow all). Carried so handlers can enforce
    /// `allowed_models` without re-reading config under a lock.
    pub allowed_models: Vec<String>,
    /// Whether this token possesses administrative privileges.
    pub admin: bool,
}

impl TokenContext {
    pub fn key_hash(&self) -> String {
        crate::token::keygen::token_key_hash(&self.key)
    }
}

/// Per-request authorization outcome inserted into request extensions.
///
/// The middleware always inserts a `TokenGrant` (even in open mode), so
/// handlers can extract `Extension<TokenGrant>` unconditionally without
/// breaking the backward-compatible no-token path.
#[derive(Debug, Clone)]
pub enum TokenGrant {
    /// No tokens configured — open mode, all requests allowed.
    Open,
    /// A token authenticated successfully.
    Granted(TokenContext),
}

impl TokenGrant {
    pub fn token(&self) -> Option<&TokenContext> {
        match self {
            TokenGrant::Open => None,
            TokenGrant::Granted(ctx) => Some(ctx),
        }
    }

    /// Whether `model` may be used under this grant. Open mode allows all;
    /// a granted token allows all when its `allowed_models` is empty.
    pub fn model_allowed(&self, model: &str) -> bool {
        match self {
            TokenGrant::Open => true,
            TokenGrant::Granted(ctx) => {
                ctx.allowed_models.is_empty() || ctx.allowed_models.iter().any(|m| m == model)
            }
        }
    }

    /// Whether this grant possesses administrative privileges.
    /// Open mode defaults to true for backward compatibility with local dev.
    /// Granted tokens reflect their configured `admin` flag.
    pub fn is_admin(&self) -> bool {
        match self {
            TokenGrant::Open => true,
            TokenGrant::Granted(ctx) => ctx.admin,
        }
    }
}

/// Token authentication state shared across requests.
///
/// Held inside `AppState` so both the middleware (auth check) and
/// `record_usage` (quota charging) observe the same configuration and
/// SQLite-backed usage store.
#[derive(Clone)]
pub struct TokenAuthState {
    pub config: Arc<RwLock<TokensConfig>>,
    pub usage_store: Option<Arc<TokenUsageStore>>,
}

impl TokenAuthState {
    /// Build a state from an already-loaded config, without a usage store.
    /// Used by tests and the open-mode path.
    pub fn new(config: TokensConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            usage_store: None,
        }
    }

    /// Load tokens from the config directory (: prefers tokens/ directory
    /// over legacy tokens.toml) and open the quota usage store.
    ///
    /// # Security
    /// - No tokens configured → open mode (no tokens, no charging)
    /// - Configuration present but malformed → **panic** (refuse to start, per spec §12.3)
    ///
    /// Silently falling back to open mode on parse errors would expose the
    /// service to unauthenticated access — a security vulnerability.
    pub fn load(config_path: &Path, state_dir: &Path) -> Self {
        let config_dir = if config_path.is_dir() {
            config_path
        } else {
            config_path.parent().unwrap_or_else(|| Path::new("."))
        };

        // load_auto handles: tokens/ dir > tokens.toml > open mode
        let (config, source) = match TokensConfig::load_auto(config_dir) {
            Ok(result) => result,
            Err(err) => {
                // SECURITY: refuse to start with malformed token configuration.
                panic!(
                    "FATAL: token configuration has errors, refusing to start (security policy).\n\
                     Directory: {}\n\
                     Error: {}\n\n\
                     Fix the configuration or remove the tokens/ directory to enable open mode.",
                    config_dir.display(),
                    err
                );
            }
        };

        tracing::info!(source = %source, "token configuration loaded");

        let usage_store = TokenUsageStore::open(&state_dir.join("tokens.db"))
            .map_err(|err| {
                tracing::error!(error = %err, "token usage store unavailable, quota enforcement disabled")
            })
            .ok()
            .map(Arc::new);

        // converge the stored window rows to the configured set at
        // load time, so a window dropped from configuration stops enforcing.
        if let Some(store) = &usage_store {
            Self::reconcile_loaded_windows(store, &config);
        }

        Self {
            config: Arc::new(RwLock::new(config)),
            usage_store,
        }
    }

    /// Re-read token configuration into the shared config (called by the reload path).
    /// The Arc<RwLock<..>> is shared with the middleware clones, so the new
    /// tokens/quotas/ratios take effect for in-flight and future requests.
    /// On parse failure the previous config is kept and the error surfaced.
    /// Supports both tokens/ directory and legacy tokens.toml .
    pub fn reload(&self, config_path: &Path) -> Result<(), String> {
        let config_dir = if config_path.is_dir() {
            config_path
        } else {
            config_path.parent().unwrap_or_else(|| Path::new("."))
        };

        let (fresh, source) = TokensConfig::load_auto(config_dir).map_err(|e| e.to_string())?;
        tracing::info!(source = %source, "token configuration reloaded");
        // a reload is a configuration boundary, so the stored windows
        // converge here too — otherwise removing a window has no effect.
        if let Some(store) = &self.usage_store {
            Self::reconcile_loaded_windows(store, &fresh);
        }
        let mut guard = self
            .config
            .write()
            .map_err(|_| "token config lock poisoned".to_string())?;
        *guard = fresh;
        Ok(())
    }

    /// Converge stored quota state to the freshly loaded configuration: drop the
    /// windows a still-configured token no longer lists and the rows
    /// of tokens that are gone entirely. Window rows are only ever
    /// upserted, and they are charged and checked without filtering on the
    /// window, so without this a window removed from `tokens.toml` keeps
    /// rejecting requests.
    fn reconcile_loaded_windows(store: &TokenUsageStore, config: &TokensConfig) {
        for token in &config.tokens {
            match store.reconcile_all_windows(token) {
                Ok(0) => {}
                Ok(removed) => tracing::info!(
                    name = %token.name,
                    removed,
                    "dropped quota windows that are no longer configured"
                ),
                Err(error) => tracing::warn!(
                    name = %token.name,
                    error = %error,
                    "failed to reconcile quota windows"
                ),
            }
        }
        // Same boundary, token dimension: rows left behind by a `token delete`
        // whose second step failed. Skipped when nothing is
        // configured — an unreadable-but-present `tokens.toml` must not wipe the
        // counters of tokens that come back on the next reload, and open mode has
        // no quota state worth converging anyway.
        if config.tokens.is_empty() {
            return;
        }
        let configured: Vec<String> = config.tokens.iter().map(|t| t.key.clone()).collect();
        match store.purge_unconfigured_token_rows(&configured) {
            Ok(0) => {}
            Ok(removed) => tracing::info!(
                removed,
                "dropped quota rows for tokens that are no longer configured"
            ),
            Err(error) => tracing::warn!(
                error = %error,
                "failed to purge quota rows for removed tokens"
            ),
        }
    }

    /// Check if token authentication is enabled (has at least one token)
    pub fn is_enabled(&self) -> bool {
        self.config.read().map(|c| c.is_enabled()).unwrap_or(false)
    }

    /// Charge a completed request against the token's quota windows, applying
    /// the model ratio so `tokens`-mode windows accumulate a unified cost
    /// (`ceil((input + output × completion_ratio) × model_ratio)`), while
    /// `requests`-mode windows increment by 1 at admission.
    ///
    /// Model-scoped groups : only windows belonging to groups matching
    /// `model` are charged (AND semantics across matching groups; isolated from non-matching).
    /// Best-effort: storage errors are logged, never propagated to the response path.
    pub fn charge(&self, ctx: &TokenContext, model: &str, input_tokens: i64, output_tokens: i64) {
        let Some(store) = &self.usage_store else {
            return;
        };
        // Read the current config once; ratio resolution borrows it so a
        // reloaded config (new model_ratios) is honoured immediately.
        let (weighted, matching_windows, credits_per_1m) = {
            let config = match self.config.read() {
                Ok(c) => c,
                Err(_) => return,
            };
            let Some(token) = config.tokens.iter().find(|t| t.key == ctx.key) else {
                return;
            };

            // Ensure all token windows exist in store before charging
            let all_windows = crate::token::usage::all_grouped_windows(token);
            if let Err(err) = store.init_grouped_windows(&ctx.key, &all_windows) {
                // The charge below updates window rows; if they were not created
                // it silently updates nothing and the usage is lost. The response
                // is already out, so this cannot be reported to the caller — it
                // must at least be attributable here.
                tracing::error!(name = %ctx.name, error = %err, "token windows unavailable, usage not recorded");
                return;
            }

            let matching_windows = crate::token::usage::grouped_windows_for_model(token, model);
            let weighted =
                RatioCalculator::calculate_quota(&config, model, input_tokens, output_tokens);
            let credits_per_1m = config.credits_per_1m.unwrap_or(100.0);
            (weighted, matching_windows, credits_per_1m)
        };

        if matching_windows.is_empty() {
            return;
        }

        // Charge all matching quota windows (AND semantics)
        if let Err(err) =
            store.add_usage_grouped(&ctx.key, &matching_windows, weighted, 0, credits_per_1m)
        {
            tracing::warn!(name = %ctx.name, error = %err, "failed to charge token quota");
        }
    }
}

/// Extract token key from request headers
fn extract_key_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    // Try Authorization: Bearer <key>
    if let Some(auth) = headers.get(header::AUTHORIZATION)
        && let Ok(auth_str) = auth.to_str()
        && let Some(token) = auth_str.strip_prefix("Bearer ")
    {
        return Some(token.trim().to_string());
    }

    // Try x-api-key (Anthropic style)
    if let Some(api_key) = headers.get("x-api-key")
        && let Ok(key_str) = api_key.to_str()
    {
        return Some(key_str.trim().to_string());
    }

    None
}

/// Constant-time comparison of two keys
fn keys_match(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    a_bytes.ct_eq(b_bytes).into()
}

/// Token authentication middleware
pub async fn token_auth(State(state): State<TokenAuthState>, req: Request, next: Next) -> Response {
    // All auth decisions are synchronous and produce a grant or a rejection;
    // no non-Send guard is held across `next.run(req).await`.
    let grant = match authenticate(&state, req.headers()) {
        Ok(grant) => grant,
        Err(status) => return status.into_response(),
    };

    let mut req = req;
    req.extensions_mut().insert(grant);
    next.run(req).await
}

/// Build the structured `429` response for an exhausted quota.
///
/// Quota is spent at the point of upstream dispatch
/// ([`super::usage::TokenUsageStore::reserve_request_slot`]) rather than in this
/// middleware, so that locally rejected requests never consume quota and
/// concurrent requests cannot slip through a stale check. Callers on
/// the forwarding path use this to answer with the offending windows, so a
/// client can explain the failure instead of surfacing an opaque, body-less
/// `429`.
pub(crate) fn quota_exhausted_response(windows: &[super::usage::WindowUsage]) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(quota_exhausted_body(windows)),
    )
        .into_response()
}

/// The client-readable body of an exhausted-quota rejection.
pub(crate) fn quota_exhausted_body(windows: &[super::usage::WindowUsage]) -> serde_json::Value {
    let windows: Vec<serde_json::Value> = windows.iter().map(quota_window_body).collect();
    serde_json::json!({
        "error": {
            "code": "quota_exceeded",
            "message": "token quota exhausted",
            "type": "token_quota_exceeded",
            "quota_windows": windows,
        }
    })
}

/// Client-readable description of one exhausted quota window.
fn quota_window_body(usage: &super::usage::WindowUsage) -> serde_json::Value {
    serde_json::json!({
        "window": usage.window_key,
        "quota_mode": super::dimension::label(&usage.quota_mode),
        "quota_limit": usage.quota_limit,
        "used": usage.used,
        "remaining": 0,
        "reset_at": usage.window_start + usage.window_seconds,
    })
}

/// Pure auth decision: inspect headers against the token state and return the
/// `TokenGrant` to inject, or a status to reject with.
///
/// Kept free of `Request`/`Next` so the 401/expire/open paths are directly
/// testable with a synthetic `HeaderMap` (no router/tower harness needed).
/// Model-access (`allowed_models`) is NOT checked here — it needs the parsed
/// request body, so handlers call `TokenGrant::model_allowed` (spec §4.2).
///
/// Quota is deliberately NOT checked here. A credit is spent at the point of
/// upstream dispatch (`reserve_request_slot`), which is the only place that
/// knows a request will actually be sent: it keeps locally rejected requests
/// from consuming quota, and it makes the check-and-charge atomic so concurrent
/// requests cannot slip through together.
fn authenticate(
    state: &TokenAuthState,
    headers: &axum::http::HeaderMap,
) -> Result<TokenGrant, StatusCode> {
    if !state.is_enabled() {
        return Ok(TokenGrant::Open);
    }

    let key = extract_key_from_headers(headers).ok_or_else(|| {
        tracing::debug!("missing token key");
        StatusCode::UNAUTHORIZED
    })?;

    // Find token (constant-time) + check expiration.
    let ctx = {
        let config = state
            .config
            .read()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let token = match config.tokens.iter().find(|t| keys_match(&t.key, &key)) {
            Some(t) => t.clone(),
            None => {
                tracing::debug!(key = %mask_key(&key), "invalid token");
                return Err(StatusCode::UNAUTHORIZED);
            }
        };
        drop(config);

        if let Some(ref expired_at) = token.expired_at
            && let Ok(expired) = chrono::DateTime::parse_from_rfc3339(expired_at)
            && chrono::Utc::now() > expired
        {
            tracing::debug!(name = %token.name, "token expired");
            return Err(StatusCode::UNAUTHORIZED);
        }

        TokenContext {
            key: token.key.clone(),
            name: token.name.clone(),
            allowed_models: token.allowed_models.clone(),
            admin: token.admin,
        }
    };

    Ok(TokenGrant::Granted(ctx))
}

/// Mask key for logging (show only first 8 chars)
fn mask_key(key: &str) -> String {
    if key.len() > 8 {
        format!("{}...", &key[..8])
    } else {
        "***".to_string()
    }
}

/// Middleware guard that requires the caller to hold admin privileges.
///
/// In open mode (`TokenGrant::Open`), admin privileges are granted by default.
/// When tokens are configured, only tokens with `admin = true` are allowed.
/// Unprivileged tokens receive a 403 Forbidden response.
pub async fn require_admin(
    axum::extract::Extension(grant): axum::extract::Extension<TokenGrant>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !grant.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "status": "error",
                "error": "Admin privilege required for this endpoint"
            })),
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::super::config::{QuotaWindow, TokenConfig};
    use super::*;

    #[test]
    fn test_mask_key() {
        assert_eq!(mask_key("lp-abcdef1234567890"), "lp-abcde...");
        assert_eq!(mask_key("short"), "***");
    }

    #[test]
    fn test_keys_match() {
        assert!(keys_match("lp-abc123", "lp-abc123"));
        assert!(!keys_match("lp-abc123", "lp-abc124"));
        assert!(!keys_match("lp-abc123", "lp-abc1234")); // different length
    }

    #[test]
    fn test_extract_key_bearer() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("Authorization", "Bearer lp-test123".parse().unwrap());
        assert_eq!(
            extract_key_from_headers(&headers),
            Some("lp-test123".to_string())
        );
    }

    #[test]
    fn test_extract_key_x_api_key() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-api-key", "lp-test456".parse().unwrap());
        assert_eq!(
            extract_key_from_headers(&headers),
            Some("lp-test456".to_string())
        );
    }

    #[test]
    fn test_extract_key_missing() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(extract_key_from_headers(&headers), None);
    }

    // ---- end-to-end auth decision through the real store -------------------

    fn auth_headers(key: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert("Authorization", format!("Bearer {key}").parse().unwrap());
        h
    }

    fn token(key: &str, name: &str, allowed: Vec<String>) -> TokenConfig {
        TokenConfig {
            key: key.to_string(),
            name: name.to_string(),
            allowed_models: allowed,
            quota_windows: vec![],
            model_quota_groups: vec![],
            expired_at: None,
            bad_request_budget_daily_limit: None,
            bad_request_budget_lifetime_limit: None,
            admin: false,
        }
    }

    #[test]
    fn authenticate_open_mode_when_no_tokens() {
        let state = TokenAuthState::new(TokensConfig::default());
        let grant = authenticate(&state, &axum::http::HeaderMap::new()).unwrap();
        assert!(matches!(grant, TokenGrant::Open));
    }

    #[test]
    fn authenticate_valid_key_grants_context() {
        let cfg = TokensConfig {
            tokens: vec![token(
                "lp-validkey1234567890abcdefgh",
                "alice",
                vec!["gpt-4o".into()],
            )],
            ..Default::default()
        };
        let state = TokenAuthState::new(cfg);
        let grant = authenticate(&state, &auth_headers("lp-validkey1234567890abcdefgh")).unwrap();
        let ctx = grant.token().expect("granted");
        assert_eq!(ctx.name, "alice");
        assert_eq!(ctx.allowed_models, vec!["gpt-4o"]);
    }

    #[test]
    fn authenticate_missing_key_401_when_enabled() {
        let cfg = TokensConfig {
            tokens: vec![token("lp-x1234567890abcdefghij1234567", "a", vec![])],
            ..Default::default()
        };
        let state = TokenAuthState::new(cfg);
        let err = authenticate(&state, &axum::http::HeaderMap::new()).unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn authenticate_invalid_key_401() {
        let cfg = TokensConfig {
            tokens: vec![token("lp-x1234567890abcdefghij1234567", "a", vec![])],
            ..Default::default()
        };
        let state = TokenAuthState::new(cfg);
        let err =
            authenticate(&state, &auth_headers("lp-wrong000000000000000000000000")).unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn authenticate_expired_key_401() {
        let mut t = token("lp-x1234567890abcdefghij1234567", "a", vec![]);
        t.expired_at = Some("2000-01-01T00:00:00Z".to_string());
        let cfg = TokensConfig {
            tokens: vec![t],
            ..Default::default()
        };
        let state = TokenAuthState::new(cfg);
        let err =
            authenticate(&state, &auth_headers("lp-x1234567890abcdefghij1234567")).unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    /// A request credit is spent at admission: a caller at its limit is refused,
    /// the window does not move, and the rejection names the window that
    /// blocked it.
    #[test]
    fn reserve_request_slot_spends_at_admission_and_explains_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenUsageStore::open(&dir.path().join("tokens.db")).unwrap();
        let windows = vec![QuotaWindow {
            window: "1h".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: 2,
        }];
        let key = "lp-x1234567890abcdefghij1234567";

        for expected in 1..=2 {
            assert!(matches!(
                store.reserve_request_slot(key, &windows).unwrap(),
                crate::token::usage::RequestSlot::Granted
            ));
            assert_eq!(store.get_token_usage(key).unwrap()[0].used, expected);
        }

        let exhausted = match store.reserve_request_slot(key, &windows).unwrap() {
            crate::token::usage::RequestSlot::Exhausted(windows) => windows,
            crate::token::usage::RequestSlot::Granted => panic!("third request must be refused"),
        };
        // All-or-nothing: a refused request spends nothing.
        assert_eq!(store.get_token_usage(key).unwrap()[0].used, 2);

        let body = quota_exhausted_body(&exhausted);
        let reported = body["error"]["quota_windows"]
            .as_array()
            .expect("quota_windows");
        assert_eq!(reported.len(), 1, "only the exhausted window is reported");
        assert_eq!(reported[0]["window"], "1h");
        assert_eq!(reported[0]["used"], 2);
        assert_eq!(reported[0]["quota_limit"], 2);
        assert_eq!(reported[0]["remaining"], 0);
        assert!(reported[0]["reset_at"].as_i64().is_some());
    }

    /// Concurrent admissions share one window; the atomic check-and-increment is
    /// what keeps `used <= quota_limit` a hard invariant.
    #[test]
    fn concurrent_admissions_never_exceed_the_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(TokenUsageStore::open(&dir.path().join("tokens.db")).unwrap());
        let windows = Arc::new(vec![QuotaWindow {
            window: "10s".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: 10,
        }]);
        let key = "lp-shared0000000000000000000000";
        let granted = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..40 {
                let store = Arc::clone(&store);
                let windows = Arc::clone(&windows);
                let granted = Arc::clone(&granted);
                scope.spawn(move || {
                    if matches!(
                        store.reserve_request_slot(key, &windows).unwrap(),
                        crate::token::usage::RequestSlot::Granted
                    ) {
                        granted.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });

        assert_eq!(granted.load(Ordering::SeqCst), 10);
        assert_eq!(store.get_token_usage(key).unwrap()[0].used, 10);
    }

    /// A window removed from the configuration stops enforcing: its row is
    /// dropped, while the counters of windows that remain are preserved
    #[test]
    fn reconcile_drops_windows_the_configuration_no_longer_lists() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenUsageStore::open(&dir.path().join("tokens.db")).unwrap();
        let window = |window: &str, limit: i64| QuotaWindow {
            window: window.to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: limit,
        };
        let key = "lp-reconcile000000000000000000";

        let with_minute = vec![window("1m", 10), window("5h", 100)];
        assert!(matches!(
            store.reserve_request_slot(key, &with_minute).unwrap(),
            crate::token::usage::RequestSlot::Granted
        ));

        // Dropping "1m" must remove its row — otherwise it keeps being charged
        // and keeps rejecting requests even though it is no longer configured.
        let without_minute = vec![window("5h", 100)];
        assert_eq!(store.reconcile_windows(key, &without_minute).unwrap(), 1);

        let usage = store.get_token_usage(key).unwrap();
        assert_eq!(usage.len(), 1, "only the configured window remains");
        assert_eq!(usage[0].window_key, "5h");
        assert_eq!(usage[0].used, 1, "the remaining window keeps its counter");
    }

    fn request_window() -> Vec<QuotaWindow> {
        vec![QuotaWindow {
            window: "1h".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: 10,
        }]
    }

    /// 回归基线：`token delete` 的清理第二步失败留下的幽灵配额行，必须在
    /// 配置边界（启动/reload）被收敛掉。修复前 `load` 只收敛**窗口**维度，
    /// 已删除 token 的行永远无人回收。
    #[test]
    fn load_drops_quota_rows_of_tokens_that_are_no_longer_configured() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let ghost = "lp-ghost000000000000000000000000";
        let live = "lp-live00000000000000000000000000";
        let windows = request_window();
        {
            let store = TokenUsageStore::open(&dir.path().join("tokens.db")).unwrap();
            store.reserve_request_slot(ghost, &windows).unwrap();
            store.reserve_request_slot(live, &windows).unwrap();
        }
        // 仍在配置的 token 必须把它那扇窗**配置上**，否则窗口收敛会
        // 先合法地删掉它，测试就分不清是谁干的了。
        std::fs::write(
            dir.path().join("tokens.toml"),
            format!(
                "[[tokens]]\nkey = \"{live}\"\nname = \"live\"\nallowed_models = []\n\n\
                 [[tokens.quota_windows]]\nwindow = \"1h\"\nquota_mode = \"requests\"\nquota_limit = 10\n"
            ),
        )
        .unwrap();

        let state = TokenAuthState::load(&config_path, dir.path());
        let store = state
            .usage_store
            .as_ref()
            .expect("load must open the quota store");

        assert!(
            store.get_token_usage(ghost).unwrap().is_empty(),
            "行属于已不存在的 token，必须被回收"
        );
        assert_eq!(
            store.get_token_usage(live).unwrap()[0].used,
            1,
            "仍在配置的 token 不得被顺带清零"
        );
    }

    /// 空配置（open 模式或 `tokens.toml` 暂时读不到）**不**触发回收：
    /// 否则一次瞬时的文件缺失就会把所有 token 的窗口计数清零，等于静默重放配额。
    #[test]
    fn empty_configuration_does_not_purge_quota_rows() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let key = "lp-survivor00000000000000000000";
        let windows = request_window();
        {
            let store = TokenUsageStore::open(&dir.path().join("tokens.db")).unwrap();
            store.reserve_request_slot(key, &windows).unwrap();
        }

        let state = TokenAuthState::load(&config_path, dir.path());
        assert!(!state.is_enabled(), "no tokens.toml means open mode");
        let store = state.usage_store.as_ref().expect("store opened");
        assert_eq!(
            store.get_token_usage(key).unwrap()[0].used,
            1,
            "open 模式不得抹掉仍在磁盘上的配额计数"
        );
    }

    #[test]
    fn test_model_allowed_empty_list() {
        let grant = TokenGrant::Granted(TokenContext {
            key: "lp-test".to_string(),
            name: "test".to_string(),
            allowed_models: vec![],
            admin: false,
        });
        // Empty allowed_models = allow all
        assert!(grant.model_allowed("any-model"));
    }

    #[test]
    fn test_model_allowed_with_list() {
        let grant = TokenGrant::Granted(TokenContext {
            key: "lp-test".to_string(),
            name: "test".to_string(),
            allowed_models: vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()],
            admin: false,
        });
        assert!(grant.model_allowed("gpt-4o"));
        assert!(!grant.model_allowed("claude-sonnet"));
    }

    #[test]
    fn test_model_allowed_open_mode() {
        let grant = TokenGrant::Open;
        assert!(grant.model_allowed("anything"));
        assert!(grant.token().is_none());
    }

    #[test]
    fn reload_picks_up_new_tokens_and_ratios() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        // Start from an empty/open state.
        let state = TokenAuthState::new(TokensConfig::default());
        assert!(!state.is_enabled());

        std::fs::write(
            dir.path().join("tokens.toml"),
            r#"
[defaults]
model_ratio = 2.0

[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "reloaded"
allowed_models = ["gpt-4o"]
"#,
        )
        .unwrap();

        state.reload(&config_path).expect("reload ok");
        assert!(state.is_enabled());
        // Ratio now reflects the reloaded config (charge() reads shared config).
        let calc_ratio = RatioCalculator::resolve(&state.config.read().unwrap(), "gpt-4o");
        assert_eq!(calc_ratio.model_ratio, 2.0);
    }

    #[test]
    fn reload_keeps_previous_config_on_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            dir.path().join("tokens.toml"),
            "[[tokens]]\nkey = \"lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11\"\nname = \"good\"\nallowed_models = []\n",
        )
        .unwrap();
        let state = TokenAuthState::new(TokensConfig::default());
        state.reload(&config_path).unwrap();
        assert!(state.is_enabled());

        // Corrupt the file; reload must error and retain the good config.
        std::fs::write(dir.path().join("tokens.toml"), "[[tokens]]\nkey = 123\n").unwrap();
        assert!(state.reload(&config_path).is_err());
        assert!(
            state.is_enabled(),
            "previous good config must survive a failed reload"
        );
    }

    // ---- security: malformed tokens.toml must refuse to start ----

    #[test]
    fn load_absent_file_returns_open_mode() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        // tokens.toml does not exist → open mode (no panic)
        let state = TokenAuthState::load(&config_path, dir.path());
        assert!(!state.is_enabled(), "absent tokens.toml → open mode");
    }

    #[test]
    #[should_panic(expected = "FATAL: token configuration has errors")]
    fn load_malformed_file_panics() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        // tokens.toml exists but is malformed → must panic (security policy)
        std::fs::write(dir.path().join("tokens.toml"), "[[tokens]]\nkey = 123\n").unwrap();
        let _state = TokenAuthState::load(&config_path, dir.path());
    }

    // ---- : load_auto & grouped charge tests ----

    #[test]
    fn load_loads_from_tokens_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        std::fs::write(
            tokens_dir.join("alice.toml"),
            r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "alice"
allowed_models = ["gpt-4o"]
"#,
        )
        .unwrap();

        let state = TokenAuthState::load(&config_path, dir.path());
        assert!(state.is_enabled());
        let cfg = state.config.read().unwrap();
        assert_eq!(cfg.tokens.len(), 1);
        assert_eq!(cfg.tokens[0].name, "alice");
    }

    #[test]
    fn load_loads_from_legacy_tokens_toml() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            dir.path().join("tokens.toml"),
            r#"
[[tokens]]
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "bob"
allowed_models = ["gpt-4o"]
"#,
        )
        .unwrap();

        let state = TokenAuthState::load(&config_path, dir.path());
        assert!(state.is_enabled());
        let cfg = state.config.read().unwrap();
        assert_eq!(cfg.tokens.len(), 1);
        assert_eq!(cfg.tokens[0].name, "bob");
    }

    #[test]
    #[should_panic(expected = "FATAL: token configuration has errors")]
    fn load_panics_on_conflict_both_dir_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();
        std::fs::write(
            tokens_dir.join("alice.toml"),
            "key = \"lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11\"\nname = \"alice\"\nallowed_models = []\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("tokens.toml"),
            "[[tokens]]\nkey = \"lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22\"\nname = \"bob\"\nallowed_models = []\n",
        )
        .unwrap();

        let _state = TokenAuthState::load(&config_path, dir.path());
    }

    #[test]
    fn reload_supports_tokens_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        std::fs::write(
            tokens_dir.join("alice.toml"),
            r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "alice"
allowed_models = ["gpt-4o"]
"#,
        )
        .unwrap();

        let state = TokenAuthState::load(&config_path, dir.path());
        assert_eq!(state.config.read().unwrap().tokens.len(), 1);

        // Add bob to tokens/
        std::fs::write(
            tokens_dir.join("bob.toml"),
            r#"
key = "lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22"
name = "bob"
allowed_models = ["claude-sonnet-4"]
"#,
        )
        .unwrap();

        state.reload(&config_path).expect("reload ok");
        let cfg = state.config.read().unwrap();
        assert_eq!(cfg.tokens.len(), 2);
        assert!(cfg.tokens.iter().any(|t| t.name == "bob"));
    }

    #[test]
    fn reload_tokens_directory_reconciles_quota_windows_and_removes_deleted_token() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        let alice_key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11";
        let bob_key = "lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22";

        std::fs::write(
            tokens_dir.join("alice.toml"),
            format!(
                r#"
key = "{alice_key}"
name = "alice"
allowed_models = ["gpt-4o"]

[[model_quota_groups]]
models = ["gpt-4o"]

  [[model_quota_groups.quota_windows]]
  window = "1h"
  quota_mode = "requests"
  quota_limit = 10
"#
            ),
        )
        .unwrap();

        std::fs::write(
            tokens_dir.join("bob.toml"),
            format!(
                r#"
key = "{bob_key}"
name = "bob"
allowed_models = []

[[model_quota_groups]]

  [[model_quota_groups.quota_windows]]
  window = "1h"
  quota_mode = "requests"
  quota_limit = 10
"#
            ),
        )
        .unwrap();

        let state = TokenAuthState::load(&config_path, dir.path());
        let store = state.usage_store.as_ref().unwrap();

        // Exercise admission for both alice and bob
        {
            let alice_gw = crate::token::usage::grouped_windows_for_model(
                state
                    .config
                    .read()
                    .unwrap()
                    .tokens
                    .iter()
                    .find(|t| t.key == alice_key)
                    .unwrap(),
                "gpt-4o",
            );
            store
                .reserve_request_slot_grouped(alice_key, &alice_gw)
                .unwrap();

            let bob_gw = crate::token::usage::grouped_windows_for_model(
                state
                    .config
                    .read()
                    .unwrap()
                    .tokens
                    .iter()
                    .find(|t| t.key == bob_key)
                    .unwrap(),
                "any-model",
            );
            store
                .reserve_request_slot_grouped(bob_key, &bob_gw)
                .unwrap();
        }

        assert_eq!(store.get_token_usage(alice_key).unwrap().len(), 1);
        assert_eq!(store.get_token_usage(bob_key).unwrap().len(), 1);

        // Operator deletes bob.toml
        std::fs::remove_file(tokens_dir.join("bob.toml")).unwrap();

        // Trigger manual reload
        state.reload(&config_path).expect("reload ok");

        // Bob's token is gone from memory
        assert_eq!(state.config.read().unwrap().tokens.len(), 1);
        assert!(
            !state
                .config
                .read()
                .unwrap()
                .tokens
                .iter()
                .any(|t| t.key == bob_key)
        );

        // Bob's quota rows were purged at the reload boundary
        assert!(
            store.get_token_usage(bob_key).unwrap().is_empty(),
            "deleted token's quota rows must be purged on reload"
        );

        // Alice's quota row is preserved
        assert_eq!(store.get_token_usage(alice_key).unwrap().len(), 1);
        assert_eq!(store.get_token_usage(alice_key).unwrap()[0].used, 1);
    }

    #[test]
    fn charge_applies_and_semantics_and_isolation_across_groups() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let tokens_dir = dir.path().join("tokens");
        std::fs::create_dir(&tokens_dir).unwrap();

        std::fs::write(
            tokens_dir.join("multi.toml"),
            r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "multi"
allowed_models = ["deepseek-v3", "mimo", "gpt-4o"]

[[model_quota_groups]]
models = ["deepseek-v3", "mimo"]

  [[model_quota_groups.quota_windows]]
  window = "5h"
  quota_mode = "tokens"
  quota_limit = 100000

[[model_quota_groups]]
models = ["deepseek-v3"]

  [[model_quota_groups.quota_windows]]
  window = "5h"
  quota_mode = "tokens"
  quota_limit = 50000

[[model_quota_groups]]
models = ["gpt-4o"]

  [[model_quota_groups.quota_windows]]
  window = "5h"
  quota_mode = "tokens"
  quota_limit = 80000
"#,
        )
        .unwrap();

        let state = TokenAuthState::load(&config_path, dir.path());
        let store = state.usage_store.as_ref().unwrap();

        let ctx = TokenContext {
            key: "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11".into(),
            name: "multi".into(),
            allowed_models: vec!["deepseek-v3".into(), "mimo".into(), "gpt-4o".into()],
            admin: false,
        };

        // Charge for deepseek-v3: matches Group 1 (deepseek-v3,mimo) and Group 2 (deepseek-v3)
        state.charge(&ctx, "deepseek-v3", 500, 500);

        let usages = store.get_token_usage(&ctx.key).unwrap();
        let g1 = usages
            .iter()
            .find(|u| u.group_id == "deepseek-v3,mimo")
            .expect("group 1 found");
        assert_eq!(g1.used, 1000, "Group 1 should be charged");

        let g2 = usages
            .iter()
            .find(|u| u.group_id == "deepseek-v3")
            .expect("group 2 found");
        assert_eq!(g2.used, 1000, "Group 2 should be charged");

        let g3 = usages
            .iter()
            .find(|u| u.group_id == "gpt-4o")
            .expect("group 3 found");
        assert_eq!(g3.used, 0, "Group 3 must remain untouched (isolated)");
    }
}
