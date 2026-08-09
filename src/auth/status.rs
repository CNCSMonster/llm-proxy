use std::path::Path;

use anyhow::{Context, Result, bail};

use super::storage::*;
use super::types::*;

/// Provider 认证状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAuthStatus {
    Ready,
    NotLoggedIn,
    Expired,
    MissingKey(String),
}

/// 获取 provider 认证状态
pub fn get_provider_auth_status(
    provider: &crate::config::ProviderConfig,
    provider_id: &str,
    accounts: &OAuthAccounts,
) -> ProviderAuthStatus {
    match &provider.auth {
        Some(crate::config::AuthConfig::ApiKeyEnv { env }) => {
            if std::env::var(env).is_ok() {
                ProviderAuthStatus::Ready
            } else {
                ProviderAuthStatus::MissingKey(env.clone())
            }
        }
        Some(crate::config::AuthConfig::AntigravityOauth { account }) => {
            let default_id = provider_id.to_string();
            let account_id = account.as_ref().unwrap_or(&default_id);
            match accounts.antigravity.get(account_id) {
                Some(acc) if acc.is_expired() => ProviderAuthStatus::Expired,
                Some(_) => ProviderAuthStatus::Ready,
                None => ProviderAuthStatus::NotLoggedIn,
            }
        }
        Some(crate::config::AuthConfig::OpenaiOauth { account }) => {
            let default_id = provider_id.to_string();
            let account_id = account.as_ref().unwrap_or(&default_id);
            match accounts.openai.get(account_id) {
                Some(acc) if acc.is_expired() => ProviderAuthStatus::Expired,
                Some(_) => ProviderAuthStatus::Ready,
                None => ProviderAuthStatus::NotLoggedIn,
            }
        }
        Some(crate::config::AuthConfig::None) | None => ProviderAuthStatus::Ready,
    }
}

/// 启动时验证 OAuth 账号
pub fn validate_oauth_on_startup(
    config: &crate::config::Config,
    accounts_path: &Path,
) -> Result<()> {
    // 检查文件是否存在
    if !accounts_path.exists() {
        // 检查是否有 OAuth provider
        let oauth_providers: Vec<_> = config
            .providers
            .iter()
            .filter(|(_, p)| {
                matches!(
                    &p.auth,
                    Some(crate::config::AuthConfig::AntigravityOauth { .. })
                        | Some(crate::config::AuthConfig::OpenaiOauth { .. })
                )
            })
            .collect();

        if !oauth_providers.is_empty() {
            tracing::warn!(
                "OAuth accounts file not found: {}\n\
                 Found {} OAuth provider(s) that require login:\n\
                 {}\n\
                 Run: llm-proxy provider login <provider>\n\
                 Or use TUI: llm-proxy provider",
                accounts_path.display(),
                oauth_providers.len(),
                oauth_providers
                    .iter()
                    .map(|(id, _)| format!("  - {}", id))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }

        return Ok(()); // 允许启动
    }

    // 文件存在，验证每个 OAuth provider 的账号引用
    let accounts = load_oauth_accounts_with_recovery(accounts_path)?;

    for (provider_id, provider) in &config.providers {
        match &provider.auth {
            Some(crate::config::AuthConfig::AntigravityOauth { account }) => {
                let account_id = account.as_ref().unwrap_or(provider_id);
                if !accounts.antigravity.contains_key(account_id) {
                    tracing::warn!(
                        "Provider '{}' references non-existent antigravity account '{}'\n\
                         Run: llm-proxy provider login {}",
                        provider_id,
                        account_id,
                        provider_id
                    );
                }
            }
            Some(crate::config::AuthConfig::OpenaiOauth { account }) => {
                let account_id = account.as_ref().unwrap_or(provider_id);
                if !accounts.openai.contains_key(account_id) {
                    tracing::warn!(
                        "Provider '{}' references non-existent openai account '{}'\n\
                         Run: llm-proxy provider login {}",
                        provider_id,
                        account_id,
                        provider_id
                    );
                }
            }
            _ => {}
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStatusRow {
    pub provider: String,
    pub auth_type: String,
    pub state: String,
    pub account_label: Option<String>,
    pub expires_at_unix: Option<u64>,
}

pub fn logout_provider(
    cfg: &crate::config::Config,
    path: &Path,
    provider_id: &str,
) -> Result<usize> {
    let account = oauth_account_for_provider(cfg, provider_id)?;
    logout(path, Some(&account))
}

pub fn oauth_account_for_provider(
    cfg: &crate::config::Config,
    provider_id: &str,
) -> Result<String> {
    let provider = cfg
        .providers
        .get(provider_id)
        .with_context(|| format!("unknown provider {provider_id:?}"))?;
    match provider.auth_config(provider_id)? {
        crate::config::AuthConfig::OpenaiOauth { account }
        | crate::config::AuthConfig::AntigravityOauth { account } => {
            Ok(account.unwrap_or_else(|| provider_id.to_string()))
        }
        crate::config::AuthConfig::ApiKeyEnv { .. } | crate::config::AuthConfig::None => {
            bail!("provider {provider_id:?} is not OAuth-backed")
        }
    }
}

pub fn logout(path: &Path, account: Option<&str>) -> Result<usize> {
    with_locked_accounts(path, |accounts| {
        let removed = if let Some(account) = account {
            usize::from(accounts.antigravity.remove(account).is_some())
                + usize::from(accounts.openai.remove(account).is_some())
        } else {
            let n = accounts.antigravity.len() + accounts.openai.len();
            accounts.antigravity.clear();
            accounts.openai.clear();
            n
        };
        if removed > 0 {
            save_oauth_accounts_locked(path, accounts)?;
        }
        Ok(removed)
    })
}

pub fn status_rows(path: &Path) -> Result<(Vec<AuthStatusRow>, Vec<SkippedAccount>)> {
    let (accounts, skipped) = load_oauth_accounts(path)?;
    let mut rows = Vec::new();
    for (account, entry) in &accounts.antigravity {
        rows.push(AuthStatusRow {
            provider: account.clone(),
            auth_type: "antigravity_oauth".to_string(),
            state: if entry.is_expired() {
                "expired"
            } else {
                "authenticated"
            }
            .to_string(),
            account_label: Some(entry.account_label.clone()),
            expires_at_unix: Some(entry.expires_at_unix as u64),
        });
    }
    for (account, entry) in &accounts.openai {
        rows.push(AuthStatusRow {
            provider: account.clone(),
            auth_type: "openai_oauth".to_string(),
            state: if entry.is_expired() {
                "expired"
            } else {
                "authenticated"
            }
            .to_string(),
            account_label: Some(entry.account_label.clone()),
            expires_at_unix: Some(entry.expires_at_unix as u64),
        });
    }
    Ok((rows, skipped))
}
