use std::path::Path;

use anyhow::{Result, bail};

use super::refresh::{RefreshContext, refresh_account_with_urls_context};
use super::types::*;

/// 检查 OAuth 账号是否存在（内存版，区分 miss 与 expired 用）。
pub(crate) fn account_exists(accounts: &AccountsFile, account_id: &str, auth_type: &str) -> bool {
    match auth_type {
        "openai" | "openai_oauth" => accounts.openai.contains_key(account_id),
        _ => false,
    }
}

/// 刷新 OAuth token（server 内存版）：按 auth_type 调上游刷新，文件由
/// refresh_account_with_urls 落盘。调用方负责更新内存缓存。
pub(crate) async fn refresh_account_for_provider_with_context_and_urls(
    path: &Path,
    account_id: &str,
    auth_type: &str,
    openai_token_url: &str,
    context: RefreshContext<'_>,
) -> Result<()> {
    match auth_type {
        "openai_oauth" => {
            refresh_account_with_urls_context(
                path,
                account_id,
                "openai_oauth",
                openai_token_url,
                context,
            )
            .await?;
        }
        other => bail!("unknown oauth type: {other}"),
    }
    Ok(())
}
