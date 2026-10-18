const OAUTH_ERROR_SUMMARY_MAX_CHARS: usize = 300;

/// Redact potentially sensitive values from error_description.
/// Looks for patterns like `refresh_token=...`, `client_secret=...`, etc.
fn redact_sensitive_fields(text: &str) -> String {
    let authorization = regex::Regex::new(r"(?i)(authorization\s*:\s*)(?:bearer\s+)?[^\s,;]+")
        .expect("authorization redaction regex is valid");
    let bearer =
        regex::Regex::new(r"(?i)\bbearer\s+[^\s,;]+").expect("bearer redaction regex is valid");
    let mut result = bearer
        .replace_all(
            &authorization.replace_all(text, "${1}[REDACTED]"),
            "Bearer [REDACTED]",
        )
        .to_string();

    let sensitive_patterns = &[
        "refresh_token",
        "access_token",
        "client_secret",
        "api_key",
        "password",
        "token",
    ];

    for pattern in sensitive_patterns {
        // Match plain and JSON fields like `access_token=value`,
        // `access_token: value`, or `"access_token":"value"`.
        let re_pattern = format!(r#"(?i)[\"']?{pattern}[\"']?\s*[=:]\s*[\"']?[^\s,;&}}\"']+"#);
        if let Ok(re) = regex::Regex::new(&re_pattern) {
            result = re
                .replace_all(&result, format!("{pattern}=[REDACTED]"))
                .to_string();
        }
    }
    result
}

/// 按各家 token 的**固定前缀 + 足够长度**兜底脱敏（panic 日志需求）。
///
/// `redact_sensitive_fields` 只认 `refresh_token=…` / `"api_key":"…"` 这类**带键名**的
/// 形态；而 panic 文本、`unwrap()` 的 `Debug` 输出、上游自由文本里的裸 token 常常没有
/// 键名（例：``called `Result::unwrap()` on an `Err` value: "rt.1.AACb…"``），会整条漏出去。
/// 要求 ≥12 个字符体并用 `\b` 卡词首，避免把 `task-…` 这类普通词打没。
pub(crate) fn redact_credential_shapes(text: &str) -> String {
    const SHAPES: &[(&str, &str)] = &[
        ("openai-refresh", r"\brt\.[A-Za-z0-9._\-]{12,}"),
        ("google-access", r"\bya29\.[A-Za-z0-9._\-]{12,}"),
        ("jwt", r"\beyJ[A-Za-z0-9._\-]{12,}"),
        ("api-key", r"\bsk-[A-Za-z0-9_\-]{12,}"),
        ("github", r"\bgh[pousr]_[A-Za-z0-9]{12,}"),
        ("slack", r"\bxox[baprs]-[A-Za-z0-9\-]{12,}"),
        ("aws", r"\bAKIA[0-9A-Z]{12,}"),
    ];
    let mut result = text.to_string();
    for (name, pattern) in SHAPES {
        // 模式是字面量，编译失败属编码错误；这里必须 fail-loud，不能像
        // `redact_sensitive_fields` 那样 `if let Ok(_)` 静默跳过（静默跳过 = 漏脱敏）。
        let re = regex::Regex::new(pattern).expect("credential shape regex is valid");
        result = re
            .replace_all(&result, format!("[REDACTED:{name}]"))
            .to_string();
    }
    result
}

fn sanitize_oauth_error_body(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted = redact_sensitive_fields(&redact_credential_shapes(&compact));
    let truncated: String = redacted
        .chars()
        .take(OAUTH_ERROR_SUMMARY_MAX_CHARS)
        .collect();
    if redacted.chars().count() > OAUTH_ERROR_SUMMARY_MAX_CHARS {
        format!("{truncated}...")
    } else {
        truncated
    }
}

pub(crate) fn oauth_error_log_summary(text: &str) -> String {
    sanitize_oauth_error_body(text)
}

pub(super) fn oauth_error_summary(text: &str) -> String {
    let Ok(payload) = serde_json::from_str::<Value>(text) else {
        // Non-JSON body: don't output content to avoid leaking sensitive info
        return "non-JSON response body omitted".to_string();
    };

    let error = payload.get("error").and_then(Value::as_str);
    let description = payload.get("error_description").and_then(Value::as_str);
    match (error, description) {
        (Some(error), Some(description)) => {
            // Redact sensitive fields from error_description
            let redacted = redact_sensitive_fields(description);
            sanitize_oauth_error_body(&format!("error={error}; error_description={redacted}"))
        }
        (Some(error), None) => sanitize_oauth_error_body(&format!("error={error}")),
        (None, Some(description)) => {
            // Redact sensitive fields from error_description
            let redacted = redact_sensitive_fields(description);
            sanitize_oauth_error_body(&format!("error_description={redacted}"))
        }
        (None, None) => "response body omitted".to_string(),
    }
}

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::storage::*;
use super::types::*;
use crate::auth::openai::flow::refresh_openai_token;
use crate::auth::openai::schema::OPENAI_OAUTH_TOKEN_URL;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RefreshContext<'a> {
    pub provider_id: &'a str,
    pub trigger: &'a str,
}

impl<'a> RefreshContext<'a> {
    pub(crate) const fn new(provider_id: &'a str, trigger: &'a str) -> Self {
        Self {
            provider_id,
            trigger,
        }
    }
}

pub async fn refresh_provider(
    cfg: &crate::config::Config,
    path: &Path,
    provider_id: &str,
) -> Result<()> {
    refresh_provider_with_trigger(cfg, path, provider_id, "cli_provider_refresh").await
}

pub(crate) async fn refresh_provider_with_trigger(
    cfg: &crate::config::Config,
    path: &Path,
    provider_id: &str,
    trigger: &str,
) -> Result<()> {
    let provider = cfg
        .providers
        .get(provider_id)
        .with_context(|| format!("unknown provider {provider_id:?}"))?;
    let (account, expected_kind) = match provider.auth_config(provider_id)? {
        crate::config::AuthConfig::OpenaiOauth { account } => (
            account.unwrap_or_else(|| provider_id.to_string()),
            "openai_oauth",
        ),
        crate::config::AuthConfig::ApiKeyEnv { .. } | crate::config::AuthConfig::None => {
            bail!("provider {provider_id:?} is not OAuth-backed")
        }
    };
    refresh_account_with_urls_context(
        path,
        &account,
        expected_kind,
        OPENAI_OAUTH_TOKEN_URL,
        RefreshContext::new(provider_id, trigger),
    )
    .await?;
    println!("refreshed OAuth provider={provider_id}");
    Ok(())
}

#[cfg(test)]
pub(super) async fn refresh_account_with_urls(
    path: &Path,
    account: &str,
    expected_kind: &str,
    openai_token_url: &str,
) -> Result<()> {
    refresh_account_with_urls_context(
        path,
        account,
        expected_kind,
        openai_token_url,
        RefreshContext::new("unknown", "test_direct"),
    )
    .await
}

pub(super) async fn refresh_account_with_urls_context(
    path: &Path,
    account: &str,
    expected_kind: &str,
    openai_token_url: &str,
    context: RefreshContext<'_>,
) -> Result<()> {
    refresh_account_with_urls_context_and_userinfo(
        path,
        account,
        expected_kind,
        openai_token_url,
        context,
    )
    .await
}

pub(super) async fn refresh_account_with_urls_context_and_userinfo(
    path: &Path,
    account: &str,
    expected_kind: &str,
    openai_token_url: &str,
    context: RefreshContext<'_>,
) -> Result<()> {
    let refresh_id = uuid::Uuid::new_v4().to_string();
    let started = std::time::Instant::now();
    let target_url = openai_token_url;
    tracing::info!(
        target: "llm_proxy::oauth",
        event = "oauth_refresh_start",
        refresh_id = %refresh_id,
        provider_id = context.provider_id,
        auth_kind = expected_kind,
        account_label = %oauth_account_label(account),
        trigger = context.trigger,
        phase = "start",
        target_host = %oauth_target_host(target_url),
        version = env!("CARGO_PKG_VERSION"),
    );
    let result =
        refresh_account_with_urls_inner(path, account, expected_kind, openai_token_url).await;
    match &result {
        Ok(RefreshOutcome::Performed) => tracing::info!(
            target: "llm_proxy::oauth",
            event = "oauth_refresh_success",
            refresh_id = %refresh_id,
            provider_id = context.provider_id,
            auth_kind = expected_kind,
            account_label = %oauth_account_label(account),
            trigger = context.trigger,
            phase = "success",
            duration_ms = started.elapsed().as_millis() as u64,
        ),
        Ok(RefreshOutcome::SkippedRecentlyRefreshed) => tracing::info!(
            target: "llm_proxy::oauth",
            event = "oauth_refresh_skip",
            refresh_id = %refresh_id,
            provider_id = context.provider_id,
            auth_kind = expected_kind,
            account_label = %oauth_account_label(account),
            trigger = context.trigger,
            phase = "skip",
            reason = "recently_refreshed",
            duration_ms = started.elapsed().as_millis() as u64,
        ),
        Err(error) => {
            let error_text = format!("{error:#}");
            tracing::warn!(
                target: "llm_proxy::oauth",
                event = "oauth_refresh_failure",
                refresh_id = %refresh_id,
                provider_id = context.provider_id,
                auth_kind = expected_kind,
                account_label = %oauth_account_label(account),
                trigger = context.trigger,
                phase = "failure",
                duration_ms = started.elapsed().as_millis() as u64,
                error_kind = oauth_refresh_error_kind(&error_text),
                retryable = oauth_refresh_retryable(&error_text),
                http_status = ?oauth_refresh_http_status(&error_text),
                oauth_error_code = ?oauth_refresh_error_code(&error_text),
                error_summary = %oauth_error_log_summary(&error_text),
            );
        }
    }
    result
        .map(|_| ())
        .with_context(|| format!("OAuth refresh failed; refresh_id={refresh_id}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshOutcome {
    Performed,
    SkippedRecentlyRefreshed,
}

/// 把刷新响应写回 OpenAI 账号条目——复用
/// `refresh_account_with_urls_inner` 的 openai 分支字段更新段（逻辑不变），
/// 供 `OAuthAccount::refresh` 转发复用（消除 trait 路径与文件级事务的重复实现）。
pub(super) fn apply_refreshed_openai(
    entry: &mut crate::auth::OpenaiAccount,
    refreshed: RefreshedToken,
    now: i64,
) {
    entry.access_token = refreshed.access_token;
    if let Some(new_refresh_token) = refreshed.refresh_token
        && !new_refresh_token.is_empty()
    {
        entry.refresh_token = new_refresh_token;
    }
    entry.expires_at_unix = refreshed
        .expires_in
        .map(|seconds| now + seconds as i64)
        .unwrap_or(now + 3600);
    entry.updated_at_unix = now;
}

async fn refresh_account_with_urls_inner(
    path: &Path,
    account: &str,
    expected_kind: &str,
    openai_token_url: &str,
) -> Result<RefreshOutcome> {
    // 整个"读-刷新-写"事务持锁：
    // 1. 防止与并发 login/logout 互相覆盖
    // 2. refresh token 是轮换式的，并发刷新同一账号会导致旧 token 作废、新 token 对被覆盖
    // 锁内 load 保证读到最新状态（其他进程已完成刷新则直接复用，不重复调用刷新 API）
    validate_path_safety(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let lock_file = acquire_lock(path)?;
    let _guard = scopeguard::guard(lock_file, |file| {
        let _ = file.unlock();
    });
    let (mut accounts, _skipped) = load_oauth_accounts(path)?;
    let now = unix_secs() as i64;
    let not_logged_in =
        || format!("OAuth account {account:?} is not logged in; run `llm-proxy provider login`");
    // 仅当"未过期且 60 秒内刚被刷新过"才跳过——识别并发进程刚完成的刷新；
    // 用户手动 refresh 未过期 token（updated_at 较旧）仍会强制执行
    let just_refreshed = |entry_updated: i64, expired: bool| !expired && now - entry_updated < 60;
    match expected_kind {
        "openai_oauth" => {
            {
                let entry = accounts.openai.get(account).with_context(not_logged_in)?;
                if just_refreshed(entry.updated_at_unix, entry.is_expired()) {
                    return Ok(RefreshOutcome::SkippedRecentlyRefreshed);
                }
            }
            let refreshed = {
                let entry = accounts.openai.get(account).expect("entry checked above");
                let client =
                    crate::http_client::try_build_from_env_with_timeout(Duration::from_secs(60))
                        .context("failed to build HTTP client")?;
                refresh_openai_token(&client, openai_token_url, &entry.refresh_token).await?
            };
            let entry = accounts
                .openai
                .get_mut(account)
                .expect("entry checked above");
            apply_refreshed_openai(entry, refreshed, now);
        }
        other => bail!("unsupported OAuth account kind {other:?}"),
    }
    save_oauth_accounts_locked(path, &accounts)?;
    Ok(RefreshOutcome::Performed)
}

pub(crate) fn oauth_account_label(account: &str) -> String {
    let digest = Sha256::digest(account.as_bytes());
    format!("account-{}", hex::encode(&digest[..6]))
}

fn oauth_target_host(target_url: &str) -> String {
    url::Url::parse(target_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "invalid-url".to_string())
}

fn oauth_refresh_error_kind(error: &str) -> &'static str {
    if error.contains("HTTP ") {
        "http_error"
    } else if error.contains("timed out") || error.contains("timeout") {
        "timeout"
    } else if error.contains("request failed") || error.contains("connect") {
        "transport_error"
    } else if error.contains("valid JSON") || error.contains("response has no") {
        "invalid_response"
    } else {
        "local_error"
    }
}

fn oauth_refresh_retryable(error: &str) -> bool {
    !(error.contains("HTTP 400")
        || error.contains("HTTP 401")
        || error.contains("HTTP 403")
        || error.contains("invalid_grant")
        || error.contains("unsupported OAuth account kind")
        || error.contains("is not logged in"))
}

fn oauth_refresh_http_status(error: &str) -> Option<u16> {
    regex::Regex::new(r"HTTP (\d{3})")
        .ok()?
        .captures(error)?
        .get(1)?
        .as_str()
        .parse()
        .ok()
}

fn oauth_refresh_error_code(error: &str) -> Option<String> {
    let summary = oauth_error_log_summary(error);
    regex::Regex::new(r"(?:^|[;\s])error=([A-Za-z0-9_-]+)")
        .ok()?
        .captures(&summary)?
        .get(1)
        .map(|capture| capture.as_str().to_string())
}

#[derive(Debug)]
pub(super) struct RefreshedToken {
    pub(super) access_token: String,
    pub(super) refresh_token: Option<String>,
    pub(super) expires_in: Option<u64>,
}

pub(super) async fn post_refresh_form(token_url: &str, params: &[(&str, &str)]) -> Result<Value> {
    let client = crate::http_client::try_build_from_env_with_timeout(Duration::from_secs(60))
        .context("failed to build HTTP client")?;
    post_refresh_form_with(&client, token_url, params).await
}

/// 客户端注入版（`AuthCtx.http` 消费面）——生产调用点传与上者同一
/// 工厂/超时构造的客户端，行为零变化。
pub(super) async fn post_refresh_form_with(
    client: &reqwest::Client,
    token_url: &str,
    params: &[(&str, &str)],
) -> Result<Value> {
    let response = client
        .post(token_url)
        .header("Accept", "application/json")
        .form(params)
        .send()
        .await
        .with_context(|| format!("OAuth refresh request failed for {token_url}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("OAuth refresh response body read failed")?;
    if !status.is_success() {
        let summary = oauth_error_summary(&text);
        bail!("OAuth refresh failed (HTTP {status}): {summary}");
    }
    serde_json::from_str(&text).context("OAuth refresh response is not valid JSON")
}

/// source（`with_transaction` 闭包）刷新路径复用文件级事务的**同名事件**
/// （oauth_refresh_{start,success,failure}）——两条刷新路径 ops 日志口径一致。
/// trigger 恒为 `token_auto_refresh`（与 `get_*_token` 自动刷新调用点一致）。
pub(super) fn log_source_refresh_event(
    phase: &'static str,
    refresh_id: &str,
    provider_id: &str,
    auth_kind: &str,
    account: &str,
    started: std::time::Instant,
    error_text: Option<&str>,
) {
    let account_label = oauth_account_label(account);
    match phase {
        "start" => tracing::info!(
            target: "llm_proxy::oauth",
            event = "oauth_refresh_start",
            refresh_id = %refresh_id,
            provider_id,
            auth_kind,
            account_label = %account_label,
            trigger = "token_auto_refresh",
            phase = "start"
        ),
        "success" => tracing::info!(
            target: "llm_proxy::oauth",
            event = "oauth_refresh_success",
            refresh_id = %refresh_id,
            provider_id,
            auth_kind,
            account_label = %account_label,
            trigger = "token_auto_refresh",
            phase = "success",
            duration_ms = started.elapsed().as_millis() as u64
        ),
        _ => {
            let text = error_text.unwrap_or_default();
            tracing::warn!(
                target: "llm_proxy::oauth",
                event = "oauth_refresh_failure",
                refresh_id = %refresh_id,
                provider_id,
                auth_kind,
                account_label = %account_label,
                trigger = "token_auto_refresh",
                phase = "failure",
                duration_ms = started.elapsed().as_millis() as u64,
                error_kind = oauth_refresh_error_kind(text),
                retryable = oauth_refresh_retryable(text),
                http_status = ?oauth_refresh_http_status(text)
            );
        }
    }
}

pub(super) fn refreshed_token_from_json(payload: Value) -> Result<RefreshedToken> {
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .context("OAuth refresh response has no access_token")?
        .to_string();
    Ok(RefreshedToken {
        access_token,
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        expires_in: payload.get("expires_in").and_then(Value::as_u64),
    })
}

pub(super) fn value_u64(payload: &Value, field: &str) -> Option<u64> {
    payload
        .get(field)
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

#[cfg(test)]
mod observability_tests {
    use super::*;

    #[test]
    fn oauth_error_log_summary_preserves_cause_and_redacts_credentials() {
        let summary = oauth_error_log_summary(
            "invalid account data: missing account label; access_token=secret-value; client_secret: another-secret",
        );
        assert!(summary.contains("missing account label"));
        assert!(!summary.contains("secret-value"));
        assert!(!summary.contains("another-secret"));
        assert!(summary.contains("access_token=[REDACTED]"));
        assert!(summary.contains("client_secret=[REDACTED]"));
    }

    #[test]
    fn oauth_error_log_summary_truncates_long_errors() {
        let summary = oauth_error_log_summary(&"x".repeat(OAUTH_ERROR_SUMMARY_MAX_CHARS + 10));
        assert_eq!(summary.chars().count(), OAUTH_ERROR_SUMMARY_MAX_CHARS + 3);
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn oauth_error_log_summary_redacts_bearer_authorization_and_query_credentials() {
        let summary = oauth_error_log_summary(
            "Authorization: Bearer bearer-secret https://oauth.example/token?access_token=query-secret&client_secret=client-secret JSON: {\"access_token\":\"json-secret\",\"refresh_token\":\"json-refresh\"}",
        );
        assert!(!summary.contains("bearer-secret"));
        assert!(!summary.contains("query-secret"));
        assert!(!summary.contains("client-secret"));
        assert!(!summary.contains("json-secret"));
        assert!(!summary.contains("json-refresh"));
        assert!(summary.contains("Authorization: [REDACTED]"));
        assert!(summary.contains("access_token=[REDACTED]"));
        assert!(summary.contains("client_secret=[REDACTED]"));
    }
    #[test]
    fn refresh_observability_labels_are_redacted_and_classified() {
        let label = oauth_account_label("some-account");
        assert!(label.starts_with("account-"));
        assert_eq!(label.len(), "account-".len() + 12);
        assert_eq!(
            oauth_target_host("https://oauth2.example/token"),
            "oauth2.example"
        );
        assert_eq!(oauth_target_host("not-a-url"), "invalid-url");
        assert_eq!(
            oauth_refresh_error_kind("OAuth refresh failed (HTTP 400)"),
            "http_error"
        );
        assert_eq!(
            oauth_refresh_error_kind("OAuth refresh request failed: timeout"),
            "timeout"
        );
        assert_eq!(
            oauth_refresh_error_kind("OAuth refresh response is not valid JSON"),
            "invalid_response"
        );
        assert_eq!(
            oauth_refresh_http_status("OAuth refresh failed (HTTP 401 Unauthorized)"),
            Some(401)
        );
        assert_eq!(
            oauth_refresh_error_code(
                "OAuth refresh failed (HTTP 400): error=invalid_grant; error_description=expired"
            ),
            Some("invalid_grant".to_string())
        );
        assert!(!oauth_refresh_retryable(
            "OAuth refresh failed (HTTP 400): error=invalid_grant"
        ));
        assert!(oauth_refresh_retryable(
            "OAuth refresh request failed: timeout"
        ));
    }
}
