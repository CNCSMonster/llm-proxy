use std::path::Path;

use anyhow::{Context, Result, bail};

use super::storage::*;
use super::types::*;

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
            .filter(|(_, p)| matches!(&p.auth, Some(crate::config::AuthConfig::OpenaiOauth { .. })))
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
        if let Some(crate::config::AuthConfig::OpenaiOauth { account }) = &provider.auth {
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
    }

    Ok(())
}

/// An OAuth credential that exists on disk but is referenced by no provider in
/// the current configuration.
///
/// This is the residue of the connect two-step write (credential first, config
/// second): the expensive half survived and the cheap half
/// did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanOAuthAccount {
    pub auth_type: &'static str,
    pub account: String,
}

/// A provider rebuilt at startup from a catalog template and an orphan credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthAccountRecovery {
    pub account: OrphanOAuthAccount,
    pub provider: String,
}

/// What startup reconciliation rebuilt, and what it refused to invent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OAuthRecoveryReport {
    pub recovered: Vec<OAuthAccountRecovery>,
    pub unrecoverable: Vec<OrphanOAuthAccount>,
}

impl OAuthRecoveryReport {
    pub fn is_empty(&self) -> bool {
        self.recovered.is_empty() && self.unrecoverable.is_empty()
    }
}

/// The reverse of `validate_oauth_on_startup`'s provider→account check: stored
/// credentials no provider references.
pub fn orphan_oauth_accounts(
    config: &crate::config::Config,
    accounts: &AccountsFile,
) -> Vec<OrphanOAuthAccount> {
    let referenced: std::collections::BTreeSet<(&'static str, String)> = config
        .providers
        .iter()
        .filter_map(|(name, provider)| match &provider.auth {
            Some(crate::config::AuthConfig::OpenaiOauth { account }) => {
                Some(("openai", account.as_deref().unwrap_or(name).to_string()))
            }
            _ => None,
        })
        .collect();

    let mut orphans = Vec::new();
    for account in accounts.openai.keys() {
        if !referenced.contains(&("openai", account.clone())) {
            orphans.push(OrphanOAuthAccount {
                auth_type: "openai",
                account: account.clone(),
            });
        }
    }
    orphans.sort_unstable_by(|a, b| (a.auth_type, &a.account).cmp(&(b.auth_type, &b.account)));
    orphans
}

/// The catalog provider whose stored OAuth account key is exactly this one.
///
/// The account key is **not** the provider name: connecting under a custom name
/// rewrites the key (`connect.rs::use_distinct_oauth_account`), so this lookup —
/// not a string guess — is what decides whether a credential can be rebuilt.
fn catalog_entry_for_account(
    auth_type: &str,
    account: &str,
) -> Option<crate::catalog::ProviderCatalogEntry> {
    crate::catalog::built_in_providers()
        .into_iter()
        .find(|entry| match &entry.provider.auth {
            Some(crate::config::AuthConfig::OpenaiOauth { account: key }) => {
                auth_type == "openai" && key.as_deref() == Some(account)
            }
            _ => false,
        })
}

/// Rebuild providers for orphaned credentials, in place on `config`.
///
/// Only the provider is rebuilt: the model selection is unrecoverable, because
/// the catalog can offer a candidate list but not the user's choice. The caller
/// must therefore point at `provider reselect`. Credentials whose account key
/// matches no catalog template (hand-written providers) are reported instead of
/// being invented under a guessed name.
pub fn recover_orphan_oauth_accounts(
    config: &mut crate::config::Config,
    accounts: &AccountsFile,
) -> OAuthRecoveryReport {
    let mut report = OAuthRecoveryReport::default();
    let mut candidate = config.clone();

    for orphan in orphan_oauth_accounts(config, accounts) {
        let Some(entry) = catalog_entry_for_account(orphan.auth_type, &orphan.account) else {
            report.unrecoverable.push(orphan);
            continue;
        };
        // A provider already occupying the catalog name was written on purpose;
        // reconciliation never overwrites it.
        if candidate.providers.contains_key(entry.id) {
            report.unrecoverable.push(orphan);
            continue;
        }
        candidate
            .providers
            .insert(entry.id.to_string(), entry.provider.clone());
        report.recovered.push(OAuthAccountRecovery {
            provider: entry.id.to_string(),
            account: orphan,
        });
    }

    if report.recovered.is_empty() {
        return report;
    }
    match candidate.validate() {
        Ok(()) => {
            *config = candidate;
            report
        }
        Err(err) => {
            tracing::error!(
                error = %err,
                "orphan OAuth accounts were not recovered: the rebuilt configuration is invalid"
            );
            report
                .unrecoverable
                .extend(report.recovered.drain(..).map(|recovery| recovery.account));
            report
        }
    }
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
        crate::config::AuthConfig::OpenaiOauth { account } => {
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
            usize::from(accounts.openai.remove(account).is_some())
        } else {
            let n = accounts.openai.len();
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
