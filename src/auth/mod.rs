// 模块级 lint（属性由嵌套模块继承，覆盖 traits.rs
// 与 auth/ 全部子模块）：registry `source` 等分发 match 禁通配臂，落地"禁 `_` 臂"
// 不再是文字约定；`unreachable_patterns` rustc 默认 deny。
#![deny(clippy::wildcard_enum_match_arm)]
mod apikey;
mod framework;
mod login;
mod openai;
mod refresh;
mod status;
pub(crate) mod storage;
mod token;
mod traits;
mod types;

#[cfg(test)]
mod tests;

// 并发正确性断言共享 fixture
#[cfg(test)]
mod testutil;

#[cfg(test)]
mod refresh_concurrency_tests;

// 规范化 byte-equal roundtrip 测试
#[cfg(test)]
mod storage_roundtrip;

// Re-export all public API symbols so external callers keep working unchanged.
pub(crate) use openai::schema::OPENAI_OAUTH_TOKEN_URL;
pub use openai::schema::OpenaiAccount;
pub use types::{AccountsFile, PendingAccount};

#[allow(unused_imports)] // used by tests in other modules
pub use storage::{
    default_state_path, load_oauth_accounts, load_oauth_accounts_with_recovery, save_oauth_accounts,
};
pub(crate) use storage::{save_oauth_accounts_locked, state_path_for_config, with_locked_accounts};

#[allow(unused_imports)]
// 生产取数面已全数经 facade/registry；测试目标经此深路径直测 get_* 语义契约
pub use openai::flow::{get_openai_token, get_openai_token_from_accounts};
pub(crate) use token::{account_exists, refresh_account_for_provider_with_context_and_urls};

pub use status::{
    logout, logout_provider, oauth_account_for_provider, status_rows, validate_oauth_on_startup,
};
// Named by callers of the recovery report and by tests; reachable through
// `recover_orphan_oauth_accounts`.
#[allow(unused_imports)]
pub use status::{
    OAuthAccountRecovery, OAuthRecoveryReport, OrphanOAuthAccount, orphan_oauth_accounts,
    recover_orphan_oauth_accounts,
};

pub use refresh::refresh_provider;
pub(crate) use refresh::{
    RefreshContext, oauth_account_label, oauth_error_log_summary, redact_credential_shapes,
    refresh_provider_with_trigger,
};

// ─────────────────────────────────────────────────────────────────────────────
// 分发接缝——`AuthRegistry` + 取数 facade
//
// 配对闭合三项（人工必查、不满足即不通过）：
//   ① `AuthRegistry` trait 与 registry 实例可见性 `pub(in crate::auth)`——不出 auth 模块；
//   ② facade 签名**无 registry 参数**（registry 于 facade 内部按 path 构建、同源取数）；
//   ③ 两类对象分立——`ProviderAuth`/flow impl 者（`Openai`）≠
//      `TokenSource` impl 者（`OpenaiSource`/`ApiKeySource`）。
// ─────────────────────────────────────────────────────────────────────────────

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::ProviderConfig;
use apikey::ApiKeySource;
use openai::{Openai, OpenaiSource};
use traits::{AuthCtx, AuthKind, DeviceFlow, TokenQuery, TokenSource, UpstreamUrls};

/// 分发接缝（不变量一：每 `AuthKind` 恰好一个 source 实例——构造性保证）。
/// `source` 内部 = 对 `AuthKind` 的**穷尽 match**（禁通配臂，模块级
/// `#![deny(clippy::wildcard_enum_match_arm)]` 生效；rustc 侧 `unreachable_patterns`
/// 默认 deny）：加 kind 变体未注册 → 非穷尽编译错；删变体残留臂 → 不存在变体编译错。
/// 可见性 `pub(in crate::auth)`（不出 auth 模块）。
pub(in crate::auth) trait AuthRegistry {
    fn source(&self, kind: AuthKind) -> &dyn TokenSource;
}

/// registry 实例（2 kind → 2 source 静态构建；可见性同上——不出 auth 模块）。
pub(in crate::auth) struct Registry {
    api_key: ApiKeySource,
    openai: OpenaiSource,
}

impl Registry {
    /// **provider 维度由 facade 从配置解析**（同类新 provider = 纯配置零代码）；
    /// registry 只按 kind 键控（不变量一）。storage 路径按调用点传入构建。
    pub(in crate::auth) fn new(path: &Path) -> Self {
        Self {
            api_key: ApiKeySource,
            openai: OpenaiSource::new(path),
        }
    }
}

impl AuthRegistry for Registry {
    fn source(&self, kind: AuthKind) -> &dyn TokenSource {
        match kind {
            AuthKind::ApiKeyEnv => &self.api_key,
            AuthKind::OpenaiOauth => &self.openai,
        }
    }
}

/// 生产上游映射（2 kind → 1 类 url；`ApiKeyEnv` = `UpstreamUrls::none()`——两 None、
/// 无假串填充；openai 生产流程不用 userinfo → `None`）。
fn production_upstream(kind: AuthKind) -> UpstreamUrls {
    match kind {
        AuthKind::ApiKeyEnv => UpstreamUrls::none(),
        AuthKind::OpenaiOauth => UpstreamUrls {
            token_url: Some(OPENAI_OAUTH_TOKEN_URL.to_string()),
            userinfo_url: None,
        },
    }
}

/// 取数 facade——**唯一取数路径**：`resolve` → `source(q.kind())` → source
/// （facade 同源取值）。签名无 registry 参数。
///
/// L1（两条 None 路径之 (i)）：`auth = None` 的调用点在**调用前分类分流**
/// （透传/跳过），不进本函数；直接误入则 `resolve` 的 `None` 臂出 `Err`（L2）。
///
/// `ctx` 于内部按生产默认构造（`Utc::now` + 同工厂 60s 客户端 + kind→url 生产映射）；
/// 测试/mock 走 [`get_token_valid_inject`] 显式注入（上游注入口径）。
pub(crate) fn get_token_valid(
    provider_id: &str,
    cfg: &ProviderConfig,
    path: &Path,
) -> Result<String> {
    let q = TokenQuery::resolve(provider_id, cfg)?;
    let client =
        crate::http_client::try_build_from_env_with_timeout(std::time::Duration::from_secs(60))
            .context("failed to build HTTP client")?;
    let ctx = AuthCtx {
        now: chrono::Utc::now(),
        http: &client,
        upstream: production_upstream(q.kind()),
    };
    let registry = Registry::new(path);
    registry.source(q.kind()).get_token_valid(&ctx, &q)
}

/// 注入入口（测试/显式 ctx：上游地址可指 testserver——mock 保护）。
/// 配对闭合：registry 仍内部构建（两者均无 registry 参数）。
#[cfg_attr(not(test), allow(dead_code))] // 测试/注入专用入口（生产走 get_token_valid）
pub(crate) fn get_token_valid_inject(
    ctx: &AuthCtx<'_>,
    provider_id: &str,
    cfg: &ProviderConfig,
    path: &Path,
) -> Result<String> {
    let q = TokenQuery::resolve(provider_id, cfg)?;
    let registry = Registry::new(path);
    registry.source(q.kind()).get_token_valid(ctx, &q)
}

/// 无锁 peek facade（admin 展示类；不进事务、不校验新鲜度——peek 语义）。
#[cfg_attr(not(test), allow(dead_code))] // 无自然生产消费方（quota 取数休眠、
// 展示面读全量条目）——保留 admin/CLI 能力面 + 测试注入；接上生产面即撤
pub(crate) fn peek_token(provider_id: &str, cfg: &ProviderConfig, path: &Path) -> Result<String> {
    let q = TokenQuery::resolve(provider_id, cfg)?;
    let registry = Registry::new(path);
    registry.source(q.kind()).peek_token(&q)
}

// facade/registry 接线测试
#[cfg(test)]
mod capability_methods_tests;
#[cfg(test)]
mod registry_wiring_tests;
#[cfg(test)]
mod residual_config_tests;

/// **login facade**——`AuthConfig` 的穷尽 match（第二载体：
/// 加变体 → E0004；async/progress 不走 trait object——AFIT 静态分发经 holder 方法）。
/// 自 `login.rs` 移至本模块（registry 与 login facade 均在 auth/mod.rs
/// 静态构建）；openai 臂经 `Openai::device_login`（整流程转发、端点常量一致）。
pub async fn login_provider(
    config_path: &Path,
    cfg: &crate::config::Config,
    path: &Path,
    provider_id: &str,
) -> Result<()> {
    let provider = cfg
        .providers
        .get(provider_id)
        .with_context(|| format!("unknown provider {provider_id:?}"))?;
    // login 路径不消费 ctx.upstream/http（build/finish/device 全走常量与内部客户端）——
    // ctx 仅满足 trait 形参；构造每登录一次。
    let client = reqwest::Client::new();
    let ctx = AuthCtx {
        now: chrono::Utc::now(),
        http: &client,
        upstream: UpstreamUrls::none(),
    };
    match provider.auth_config(provider_id)? {
        crate::config::AuthConfig::OpenaiOauth { account } => {
            let account = account.unwrap_or_else(|| provider_id.to_string());
            let flow = Openai {
                config_path: config_path.to_path_buf(),
                path: path.to_path_buf(),
                account,
                provider_id: provider_id.to_string(),
            };
            let _entry = flow.device_login(&ctx).await?;
            Ok(())
        }
        crate::config::AuthConfig::ApiKeyEnv { .. } | crate::config::AuthConfig::None => {
            bail!("provider {provider_id:?} is not OAuth-backed")
        }
    }
}
