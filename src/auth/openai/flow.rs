//! OpenAI 认证流程（device 授权码流 / token 获取 / 刷新）。
//! 原位：`auth/login.rs`（device 区）、`auth/token.rs`、`auth/refresh.rs`。
//! 可见性机械适配同 `schema.rs`；认证能力裁剪与本目录无关。

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::auth::login::write_oauth_account;
use crate::auth::refresh::{
    RefreshContext, RefreshedToken, post_refresh_form, refresh_account_with_urls_context,
    refreshed_token_from_json,
};
use crate::auth::storage::*;
use crate::auth::types::{AccountsFile, unix_secs};

use super::schema::*;

pub(in crate::auth) async fn login_openai_device_with_urls(
    config_path: &Path,
    path: &Path,
    account: &str,
    provider_id: &str,
    device_auth_url: &str,
    device_token_url: &str,
    oauth_token_url: &str,
) -> Result<()> {
    let device = request_openai_device_code(device_auth_url).await?;
    println!("OpenAI login device code: {}", device.user_code);
    println!("Open: https://auth.openai.com/codex/device");
    let interval = device.interval.max(2) + 3;
    let deadline = unix_secs() + device.expires_in.max(1);
    loop {
        if unix_secs() >= deadline {
            bail!("OpenAI device login expired; run provider login again");
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if let Some(poll) =
            poll_openai_device_token(device_token_url, &device.device_auth_id, &device.user_code)
                .await?
        {
            let token = exchange_openai_device_token(
                oauth_token_url,
                &poll.authorization_code,
                &poll.code_verifier,
            )
            .await?;
            let now = unix_secs() as i64;
            let entry = OpenaiAccount {
                account_label: account.to_string(),
                access_token: token.access_token,
                refresh_token: token
                    .refresh_token
                    .filter(|value| !value.is_empty())
                    .context("OpenAI OAuth server did not return a refresh token")?,
                expires_at_unix: token
                    .expires_in
                    .map(|seconds| now + seconds as i64)
                    .unwrap_or(now + 3600),
                updated_at_unix: now,
            };
            // server 运行 → 委托写（UDS + 更新 server 内存缓存）；
            // 否则本地持所有权锁 + 文件锁读改写
            write_oauth_account(
                config_path,
                path,
                account,
                provider_id,
                "openai",
                serde_json::to_value(&entry)?,
            )
            .await?;
            println!("logged in OAuth account={account} provider={provider_id}");
            return Ok(());
        }
    }
}

#[derive(Debug)]
pub(in crate::auth) struct DeviceCode {
    pub(in crate::auth) device_auth_id: String,
    pub(in crate::auth) user_code: String,
    pub(in crate::auth) expires_in: u64,
    pub(in crate::auth) interval: u64,
}

#[derive(Debug)]
pub(in crate::auth) struct DevicePoll {
    pub(in crate::auth) authorization_code: String,
    pub(in crate::auth) code_verifier: String,
}

pub(in crate::auth) async fn request_openai_device_code(url: &str) -> Result<DeviceCode> {
    let payload: Value = reqwest::Client::new()
        .post(url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({ "client_id": OPENAI_CLIENT_ID }))
        .send()
        .await
        .with_context(|| format!("OpenAI device code request failed for {url}"))?
        .error_for_status()?
        .json()
        .await
        .context("OpenAI device code response is not valid JSON")?;
    let device_auth_id = payload
        .get("device_auth_id")
        .or_else(|| payload.get("deviceAuthID"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .context("OpenAI device code response has no device_auth_id")?
        .to_string();
    let user_code = payload
        .get("user_code")
        .or_else(|| payload.get("usercode"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .context("OpenAI device code response has no user_code")?
        .to_string();
    Ok(DeviceCode {
        device_auth_id,
        user_code,
        expires_in: crate::auth::refresh::value_u64(&payload, "expires_in").unwrap_or(900),
        interval: crate::auth::refresh::value_u64(&payload, "interval").unwrap_or(5),
    })
}

pub(in crate::auth) async fn poll_openai_device_token(
    url: &str,
    device_auth_id: &str,
    user_code: &str,
) -> Result<Option<DevicePoll>> {
    let response = reqwest::Client::new()
        .post(url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        }))
        .send()
        .await
        .with_context(|| format!("OpenAI device token poll failed for {url}"))?;
    if response.status() == reqwest::StatusCode::FORBIDDEN
        || response.status() == reqwest::StatusCode::NOT_FOUND
    {
        return Ok(None);
    }
    let payload: Value = response.error_for_status()?.json().await?;
    Ok(Some(DevicePoll {
        authorization_code: payload
            .get("authorization_code")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .context("OpenAI device token response has no authorization_code")?
            .to_string(),
        code_verifier: payload
            .get("code_verifier")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .context("OpenAI device token response has no code_verifier")?
            .to_string(),
    }))
}

async fn exchange_openai_device_token(
    url: &str,
    authorization_code: &str,
    code_verifier: &str,
) -> Result<RefreshedToken> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", authorization_code),
        ("code_verifier", code_verifier),
        (
            "redirect_uri",
            "https://auth.openai.com/deviceauth/callback",
        ),
        ("client_id", OPENAI_CLIENT_ID),
    ];
    let payload = post_refresh_form(url, &params).await?;
    refreshed_token_from_json(payload)
}

/// 从 OAuth 账号集合取 OpenAI token（内存版，不读文件）。过期返回错误。
/// 供 server 转发热路径使用（内存缓存避免每次请求读文件）。
pub fn get_openai_token_from_accounts(
    accounts: &AccountsFile,
    account_id: &str,
    provider_id: &str,
) -> Result<String> {
    let account = accounts.openai.get(account_id).ok_or_else(|| {
        anyhow::anyhow!(
            "OpenAI account '{account_id}' not found\n\
             Run: llm-proxy provider login {provider_id}"
        )
    })?;
    if account.is_expired() {
        anyhow::bail!("OpenAI account '{account_id}' access token is expired");
    }
    Ok(account.access_token.clone())
}

/// 获取 OpenAI OAuth token（过期自动刷新）
#[allow(dead_code)] // 生产取数面全经 facade/registry；保留为单元测试直测载体（缺文件/缺账号/过期消息契约）
pub fn get_openai_token(path: &Path, account_id: &str, provider_id: &str) -> Result<String> {
    // 检查文件是否存在
    if !path.exists() {
        bail!(
            "Provider '{}' requires OAuth login\n\
             Run: llm-proxy provider login {}\n\
             Or use TUI: llm-proxy provider",
            provider_id,
            provider_id
        );
    }

    let accounts = load_oauth_accounts_with_recovery(path)?;

    let account = accounts.openai.get(account_id).ok_or_else(|| {
        anyhow::anyhow!(
            "OpenAI account '{}' not found\n\
                 Run: llm-proxy provider login {}",
            account_id,
            provider_id
        )
    })?;

    if !account.is_expired() {
        return Ok(account.access_token.clone());
    }

    // 过期，尝试自动刷新
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => tokio::task::block_in_place(|| {
            rt.block_on(async {
                refresh_account_with_urls_context(
                    path,
                    account_id,
                    "openai_oauth",
                    OPENAI_OAUTH_TOKEN_URL,
                    RefreshContext::new(provider_id, "token_auto_refresh"),
                )
                .await?;
                let accounts = load_oauth_accounts_with_recovery(path)?;
                let account = accounts.openai.get(account_id).ok_or_else(|| {
                    anyhow::anyhow!("OpenAI account '{}' disappeared after refresh", account_id)
                })?;
                Ok(account.access_token.clone())
            })
        }),
        Err(_) => bail!(
            "OpenAI account '{}' access token is expired and no runtime available for auto-refresh\n\
             Run: llm-proxy provider refresh {}",
            account_id,
            provider_id
        ),
    }
}

pub(in crate::auth) async fn refresh_openai_token(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<RefreshedToken> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", OPENAI_CLIENT_ID),
    ];
    let payload = super::super::refresh::post_refresh_form_with(client, token_url, &params).await?;
    refreshed_token_from_json(payload)
}
