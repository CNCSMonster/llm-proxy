const OAUTH_ERROR_SUMMARY_MAX_CHARS: usize = 300;

/// Redact potentially sensitive values from error_description.
/// Looks for patterns like `refresh_token=...`, `client_secret=...`, etc.
fn redact_sensitive_fields(text: &str) -> String {
    let sensitive_patterns = &[
        "refresh_token",
        "access_token",
        "client_secret",
        "api_key",
        "password",
        "token",
    ];

    let mut result = text.to_string();
    for pattern in sensitive_patterns {
        // Match patterns like `refresh_token=value` or `refresh_token: value`
        let re_pattern = format!(r"(?i){pattern}[=:]\s*[^\s,;}}]+");
        if let Ok(re) = regex::Regex::new(&re_pattern) {
            result = re
                .replace_all(&result, format!("{pattern}=[REDACTED]"))
                .to_string();
        }
    }
    result
}

fn sanitize_oauth_error_body(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted = redact_sensitive_fields(&compact);
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

use super::storage::*;
use super::types::*;

pub async fn refresh_provider(
    cfg: &crate::config::Config,
    path: &Path,
    provider_id: &str,
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
        crate::config::AuthConfig::AntigravityOauth { account } => (
            account.unwrap_or_else(|| provider_id.to_string()),
            "antigravity_oauth",
        ),
        crate::config::AuthConfig::ApiKeyEnv { .. } | crate::config::AuthConfig::None => {
            bail!("provider {provider_id:?} is not OAuth-backed")
        }
    };
    refresh_account_with_urls(
        path,
        &account,
        expected_kind,
        OPENAI_OAUTH_TOKEN_URL,
        ANTIGRAVITY_TOKEN_URL,
    )
    .await?;
    println!("refreshed OAuth account={account} provider={provider_id}");
    Ok(())
}

pub(super) async fn refresh_account_with_urls(
    path: &Path,
    account: &str,
    expected_kind: &str,
    openai_token_url: &str,
    antigravity_token_url: &str,
) -> Result<()> {
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
                    return Ok(());
                }
            }
            let refreshed = {
                let entry = accounts.openai.get(account).expect("entry checked above");
                refresh_openai_token(openai_token_url, &entry.refresh_token).await?
            };
            let entry = accounts
                .openai
                .get_mut(account)
                .expect("entry checked above");
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
        "antigravity_oauth" => {
            {
                let entry = accounts
                    .antigravity
                    .get(account)
                    .with_context(not_logged_in)?;
                if just_refreshed(entry.updated_at_unix, entry.is_expired()) {
                    return Ok(());
                }
            }
            let refreshed = {
                let entry = accounts
                    .antigravity
                    .get(account)
                    .expect("entry checked above");
                refresh_antigravity_token(antigravity_token_url, &entry.refresh_token).await?
            };
            let entry = accounts
                .antigravity
                .get_mut(account)
                .expect("entry checked above");
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
        other => bail!("unsupported OAuth account kind {other:?}"),
    }
    save_oauth_accounts_locked(path, &accounts)
}

#[derive(Debug)]
pub(super) struct RefreshedToken {
    pub(super) access_token: String,
    pub(super) refresh_token: Option<String>,
    pub(super) expires_in: Option<u64>,
}

async fn refresh_openai_token(token_url: &str, refresh_token: &str) -> Result<RefreshedToken> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", OPENAI_CLIENT_ID),
    ];
    let payload = post_refresh_form(token_url, &params).await?;
    refreshed_token_from_json(payload)
}

async fn refresh_antigravity_token(token_url: &str, refresh_token: &str) -> Result<RefreshedToken> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", ANTIGRAVITY_CLIENT_ID),
        ("client_secret", ANTIGRAVITY_CLIENT_SECRET),
    ];
    let payload = post_refresh_form(token_url, &params).await?;
    refreshed_token_from_json(payload)
}

pub(super) async fn post_refresh_form(token_url: &str, params: &[(&str, &str)]) -> Result<Value> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("failed to build HTTP client")?
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
