use std::path::Path;

use anyhow::{Result, bail};

use super::refresh::refresh_account_with_urls;
use super::storage::*;
use super::types::*;

/// 获取 Antigravity OAuth token（过期自动刷新）
pub fn get_antigravity_token(path: &Path, account_id: &str, provider_id: &str) -> Result<String> {
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

    let account = accounts.antigravity.get(account_id).ok_or_else(|| {
        anyhow::anyhow!(
            "Antigravity account '{}' not found\n\
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
                refresh_account_with_urls(
                    path,
                    account_id,
                    "antigravity_oauth",
                    "",
                    ANTIGRAVITY_TOKEN_URL,
                )
                .await?;
                let accounts = load_oauth_accounts_with_recovery(path)?;
                let account = accounts.antigravity.get(account_id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Antigravity account '{}' disappeared after refresh",
                        account_id
                    )
                })?;
                Ok(account.access_token.clone())
            })
        }),
        Err(_) => bail!(
            "Antigravity account '{}' access token is expired and no runtime available for auto-refresh\n\
             Run: llm-proxy provider refresh {}",
            account_id,
            provider_id
        ),
    }
}

/// 检查 OAuth 账号是否存在（内存版，区分 miss 与 expired 用）。
pub(crate) fn account_exists(accounts: &OAuthAccounts, account_id: &str, auth_type: &str) -> bool {
    match auth_type {
        "openai" | "openai_oauth" => accounts.openai.contains_key(account_id),
        "antigravity" | "antigravity_oauth" => accounts.antigravity.contains_key(account_id),
        _ => false,
    }
}

/// 从 OAuth 账号集合取 OpenAI token（内存版，不读文件）。过期返回错误。
/// 供 server 转发热路径使用（§15.2 阶段 4：内存缓存避免每次请求读文件）。
pub fn get_openai_token_from_accounts(
    accounts: &OAuthAccounts,
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

/// 从 OAuth 账号集合取 Antigravity token（内存版，不读文件）。过期返回错误。
pub fn get_antigravity_token_from_accounts(
    accounts: &OAuthAccounts,
    account_id: &str,
    provider_id: &str,
) -> Result<String> {
    let account = accounts.antigravity.get(account_id).ok_or_else(|| {
        anyhow::anyhow!(
            "Antigravity account '{account_id}' not found\n\
             Run: llm-proxy provider login {provider_id}"
        )
    })?;
    if account.is_expired() {
        anyhow::bail!("Antigravity account '{account_id}' access token is expired");
    }
    Ok(account.access_token.clone())
}

/// 刷新 OAuth token（server 内存版）：按 auth_type 调上游刷新，文件由
/// refresh_account_with_urls 落盘。调用方负责更新内存缓存。
pub async fn refresh_account_for_provider(
    path: &Path,
    account_id: &str,
    auth_type: &str,
) -> Result<()> {
    match auth_type {
        "openai_oauth" => {
            refresh_account_with_urls(path, account_id, "openai_oauth", OPENAI_OAUTH_TOKEN_URL, "")
                .await?;
        }
        "antigravity_oauth" => {
            refresh_account_with_urls(
                path,
                account_id,
                "antigravity_oauth",
                "",
                ANTIGRAVITY_TOKEN_URL,
            )
            .await?;
        }
        other => bail!("unknown oauth type: {other}"),
    }
    Ok(())
}

/// 获取 OpenAI OAuth token（过期自动刷新）
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
                refresh_account_with_urls(
                    path,
                    account_id,
                    "openai_oauth",
                    OPENAI_OAUTH_TOKEN_URL,
                    "",
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
