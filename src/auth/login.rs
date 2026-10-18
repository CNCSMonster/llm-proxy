use std::path::Path;

use anyhow::{Result, bail};

use super::storage::*;
use super::types::*;

/// 写入 OAuth 账号：server 运行 → 委托 UDS（server 写文件 + 更新内存缓存）；
/// 否则本地持所有权锁 + 文件锁写入。
pub(crate) async fn write_oauth_account(
    config_path: &Path,
    path: &Path,
    account: &str,
    provider_id: &str,
    oauth_type: &str,
    entry: serde_json::Value,
) -> Result<()> {
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let dirs = crate::service::StateDirs::resolve(config_dir);
    write_oauth_account_at(
        config_path,
        path,
        &dirs.runtime_dir,
        account,
        provider_id,
        oauth_type,
        entry,
    )
    .await
}

pub(super) async fn write_oauth_account_at(
    config_path: &Path,
    path: &Path,
    runtime_dir: &Path,
    account: &str,
    provider_id: &str,
    oauth_type: &str,
    entry: serde_json::Value,
) -> Result<()> {
    if let Some(server) = crate::admin_client::detect_server(config_path).await? {
        crate::ownership::ensure_same_config(&server, config_path)?;
        tracing::info!(
            target: "llm_proxy::oauth",
            event = "oauth_credential_persistence_start",
            mode = "server",
            provider = %provider_id,
            account = %super::refresh::oauth_account_label(account),
        );
        let result = server
            .oauth_write(provider_id, oauth_type, account, &entry)
            .await;
        match &result {
            Ok(_) => tracing::info!(
                target: "llm_proxy::oauth",
                event = "oauth_credential_persistence_success",
                mode = "server",
                provider = %provider_id,
                account = %super::refresh::oauth_account_label(account),
            ),
            Err(_) => tracing::warn!(
                target: "llm_proxy::oauth",
                event = "oauth_credential_persistence_failure",
                mode = "server",
                provider = %provider_id,
                account = %super::refresh::oauth_account_label(account),
            ),
        }
        return result.map(|_| ());
    }
    tracing::info!(
        target: "llm_proxy::oauth",
        event = "oauth_credential_persistence_start",
        mode = "local",
        provider = %provider_id,
        account = %super::refresh::oauth_account_label(account),
    );
    // 本地：所有权锁（单一写者）+ 文件锁读改写
    let result = crate::ownership::with_cli_write_lock_at(
        runtime_dir,
        &format!("llm-proxy provider login {provider_id}"),
        || {
            with_locked_accounts(path, |accounts| {
                match oauth_type {
                    "openai" => {
                        let e: OpenaiAccount = serde_json::from_value(entry.clone())?;
                        accounts.openai.insert(account.to_string(), e);
                    }
                    _ => bail!("unknown oauth type: {oauth_type}"),
                }
                save_oauth_accounts_locked(path, accounts)
            })
        },
    );
    match &result {
        Ok(()) => tracing::info!(
            target: "llm_proxy::oauth",
            event = "oauth_credential_persistence_success",
            mode = "local",
            provider = %provider_id,
            account = %super::refresh::oauth_account_label(account),
        ),
        Err(_) => tracing::warn!(
            target: "llm_proxy::oauth",
            event = "oauth_credential_persistence_failure",
            mode = "local",
            provider = %provider_id,
            account = %super::refresh::oauth_account_label(account),
        ),
    }
    result
}
