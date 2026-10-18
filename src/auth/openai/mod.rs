//! OpenAI 认证实现专用目录。
//!
//! 迁移自 `auth/{login,token,refresh,types,storage}.rs`（现状函数边界、
//! 函数体零变化）；旧路径经原模块与 `auth/mod.rs` re-export 保持可达。
//! 本目录是公开能力，**不属认证能力裁剪范围**。

pub(super) mod flow;
pub(super) mod schema;
// ────────────────────────────────────────────────────────────────────────────
// 认证类型标记/会话持有对象 + 分层 trait impl。
// 角色分工：本对象 = 登录期
// 临时对象；取数期长驻对象 `OpenaiSource`，两者**不合并**。
// 接线前本节无调用方——逐项 `allow(dead_code)`。
// ────────────────────────────────────────────────────────────────────────────

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use super::framework::StorageHandle;
use super::refresh::apply_refreshed_openai;
use super::traits::{
    AccountGroup, AuthCtx, AuthKind, DeviceFlow, OAuthAccount, ProviderAuth, TokenQuery,
    TokenSource,
};
use super::types::AccountsFile;
use flow::{
    DeviceCode, DevicePoll, login_openai_device_with_urls, poll_openai_device_token,
    refresh_openai_token, request_openai_device_code,
};
use schema::{
    OPENAI_DEVICE_AUTH_URL, OPENAI_DEVICE_TOKEN_URL, OPENAI_OAUTH_TOKEN_URL, OpenaiAccount,
    validate_openai_account,
};

/// 登录期/会话持有对象（字段提供 flow 位置参数；静态方法不触字段）。
pub(in crate::auth) struct Openai {
    pub(in crate::auth) config_path: PathBuf,
    pub(in crate::auth) path: PathBuf,
    pub(in crate::auth) account: String,
    pub(in crate::auth) provider_id: String,
}

impl ProviderAuth for Openai {
    const KIND: AuthKind = AuthKind::OpenaiOauth;
    type Account = OpenaiAccount;

    fn group(container: &mut AccountsFile) -> &mut AccountGroup<Self> {
        &mut container.openai
    }

    fn validate_account(id: &str, raw: &Self::Account) -> Result<()> {
        validate_openai_account(id, raw)
    }

    fn is_expired(account: &Self::Account, ctx: &AuthCtx<'_>) -> bool {
        // 时钟经 ctx.now（同 anti，与 unix_secs() 同源系统时钟、语义等价）。
        account.expires_at_unix <= ctx.now.timestamp()
    }
}

impl OAuthAccount for Openai {
    fn refresh(ctx: &AuthCtx<'_>, account: &mut Self::Account) -> Result<()> {
        let token_url = ctx
            .upstream
            .token_url
            .as_deref()
            .ok_or_else(|| anyhow!("AuthCtx.upstream.token_url missing for OpenAI refresh"))?;
        // 同步签名内执行 HTTP 刷新：须在 Tokio 运行时上下文（无上下文 = 可达 Err）；
        // 接线从 async 侧进入时用 block_in_place/spawn_blocking（见 traits.rs 注）。
        let handle = tokio::runtime::Handle::try_current()
            .context("OAuth refresh requires a Tokio runtime context")?;
        // block_in_place + `ctx.http` 注入（同 anti）。
        let refreshed = tokio::task::block_in_place(|| {
            handle.block_on(refresh_openai_token(
                ctx.http,
                token_url,
                &account.refresh_token,
            ))
        })?;
        apply_refreshed_openai(account, refreshed, ctx.now.timestamp());
        Ok(())
    }
}

impl DeviceFlow for Openai {
    type DeviceCode = DeviceCode;
    type DevicePoll = DevicePoll;

    async fn request_device_code(&self, _ctx: &AuthCtx<'_>) -> Result<Self::DeviceCode> {
        // device 端点 = crate 常量（与 login.rs 生产一致——`ctx.upstream` 专供
        // token/refresh 路径，按现状校准）。
        request_openai_device_code(OPENAI_DEVICE_AUTH_URL).await
    }

    async fn poll_device_token(
        &self,
        _ctx: &AuthCtx<'_>,
        code: &Self::DeviceCode,
    ) -> Result<Option<Self::DevicePoll>> {
        poll_openai_device_token(
            OPENAI_DEVICE_TOKEN_URL,
            &code.device_auth_id,
            &code.user_code,
        )
        .await
    }

    async fn device_login(&self, _ctx: &AuthCtx<'_>) -> Result<Self::Account> {
        // → login_openai_device_with_urls（self 替代 config_path/path 位置参数，
        // 端点常量与生产一致）。
        login_openai_device_with_urls(
            &self.config_path,
            &self.path,
            &self.account,
            &self.provider_id,
            OPENAI_DEVICE_AUTH_URL,
            OPENAI_DEVICE_TOKEN_URL,
            OPENAI_OAUTH_TOKEN_URL,
        )
        .await?;
        read_back_openai(&self.path, &self.account)
    }
}

/// 登录载体返回 `()`，trait 契约要求 `Account`——回读刚持久化的条目
/// （接线点：若载体改为直接返回 entry 则移除）。
fn read_back_openai(path: &Path, account: &str) -> Result<OpenaiAccount> {
    let (accounts, _skipped) = crate::auth::load_oauth_accounts(path)?;
    accounts
        .openai
        .get(account)
        .cloned()
        .with_context(|| format!("OAuth account {account:?} not found after login"))
}

/// 取数期长驻对象（角色分工 ②，同 anti——分立不合并）。
pub(in crate::auth) struct OpenaiSource {
    storage: StorageHandle,
}

impl OpenaiSource {
    pub(in crate::auth) fn new(path: &Path) -> Self {
        Self {
            storage: StorageHandle::new(path),
        }
    }
}

impl TokenSource for OpenaiSource {
    /// 无锁快照读（peek 语义同 anti：不校验新鲜度）。
    fn peek_token(&self, q: &TokenQuery) -> Result<String> {
        let account = source_account(q)?;
        ensure_login_backing(self.storage.path(), q.provider_id())?;
        let (mut accounts, _skipped) = crate::auth::load_oauth_accounts(self.storage.path())?;
        let group = <Openai as ProviderAuth>::group(&mut accounts);
        group
            .get(account)
            .map(|entry| entry.access_token.clone())
            .with_context(|| {
                format!(
                    "OpenAI account '{account}' not found\n\
                     Run: llm-proxy provider login {}",
                    q.provider_id()
                )
            })
    }

    /// 事务内读→判→刷→写（openai 无 email 回填）。
    fn get_token_valid(&self, ctx: &AuthCtx<'_>, q: &TokenQuery) -> Result<String> {
        let account = source_account(q)?;
        ensure_login_backing(self.storage.path(), q.provider_id())?;
        self.storage.with_transaction::<Openai, _>(|group| {
            let expired = {
                let entry = group.get(account).with_context(|| {
                    format!(
                        "OpenAI account '{account}' not found\n\
                         Run: llm-proxy provider login {}",
                        q.provider_id()
                    )
                })?;
                Openai::is_expired(entry, ctx)
            };
            if !expired {
                return Ok(group
                    .get(account)
                    .expect("entry checked above")
                    .access_token
                    .clone());
            }
            let refresh_id = uuid::Uuid::new_v4().to_string();
            let started = std::time::Instant::now();
            super::refresh::log_source_refresh_event(
                "start",
                &refresh_id,
                q.provider_id(),
                <Openai as ProviderAuth>::KIND.as_auth_kind_str(),
                account,
                started,
                None,
            );
            let refreshed = {
                let entry = group.get_mut(account).expect("entry checked above");
                Openai::refresh(ctx, entry)
            };
            match &refreshed {
                Ok(()) => super::refresh::log_source_refresh_event(
                    "success",
                    &refresh_id,
                    q.provider_id(),
                    <Openai as ProviderAuth>::KIND.as_auth_kind_str(),
                    account,
                    started,
                    None,
                ),
                Err(err) => super::refresh::log_source_refresh_event(
                    "failure",
                    &refresh_id,
                    q.provider_id(),
                    <Openai as ProviderAuth>::KIND.as_auth_kind_str(),
                    account,
                    started,
                    Some(&format!("{err:#}")),
                ),
            }
            refreshed?;
            Ok(group
                .get(account)
                .expect("entry checked above")
                .access_token
                .clone())
        })
    }
}

pub(super) fn source_account(q: &super::traits::TokenQuery) -> Result<&str> {
    q.account().ok_or_else(|| {
        anyhow!(
            "TokenQuery for provider {:?} carries no account name (kind mismatch)",
            q.provider_id()
        )
    })
}

pub(super) fn ensure_login_backing(path: &Path, provider_id: &str) -> Result<()> {
    if !path.exists() {
        bail!(
            "Provider '{provider_id}' requires OAuth login\n\
             Run: llm-proxy provider login {provider_id}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
