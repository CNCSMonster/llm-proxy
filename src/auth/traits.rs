//! auth 分层 trait 与类型化查询。
//!
//! 模块级 `allow(dead_code)` 已随分发接缝落地移除（原则保留：只压
//! unused、**不豁免** E0004/通配 lint/unreachable）；仅个别**分步契约载体**
//! 持注释级 `allow`（见各方法注——消费方接线即撤）。

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};

use super::types::AccountsFile;
use crate::config::{AuthConfig, ProviderConfig};

/// 运行时认证类型路由键（纯运行时数据，**不派生 serde**——不参与任何持久化）。
///
/// `AuthConfig` 有 3 变体、本枚举只有 2：`AuthConfig::None`（该 provider 无需认证注入）
/// **不是认证类型**，由 `TokenQuery::resolve` 的 `None` 臂承接为可达 `Err`
/// （两条 None 路径见 resolve 注）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthKind {
    ApiKeyEnv,
    OpenaiOauth,
}

impl AuthKind {
    /// 事件/日志 `auth_kind` 字段规范字符串——与 `oauth_refresh_*` 事件历史取值
    /// 逐字一致（消费方：source 刷新事件 + 写回校验标签，经 `ProviderAuth::KIND` 取得）。
    pub(crate) fn as_auth_kind_str(self) -> &'static str {
        match self {
            AuthKind::ApiKeyEnv => "api_key_env",
            AuthKind::OpenaiOauth => "openai_oauth",
        }
    }
}

/// 字段**私有（模块级可见性）**：非 `traits` 模块（及其子模块）内无法字面构造；
/// crate 内唯一构造入口 = `resolve`——不提供 `new`/`Default`、无 `pub(crate)` 字段，
/// `CredentialKey` 同样私有 → “kind/cred 非法组合”在类型层面不可表达。
#[derive(Debug)]
pub struct TokenQuery {
    kind: AuthKind,
    cred: CredentialKey,
    provider_id: String,
}

/// 私有凭证定位键；**按字段而非按 kind 的枚举**（避免新认证类型时出现平行闭集）。
#[derive(Debug)]
enum CredentialKey {
    Env(String),
    Account(String),
}

impl TokenQuery {
    /// 唯一构造：复用既有 `ProviderConfig::auth_config`（api_key_env/auth 合并与冲突
    /// bail 的既有语义单点）后，**单次穷尽 `match`（无 `_` 臂）产出
    /// `Result<(kind, cred)>`**——`None` 臂产出 `Err`：4→3 投影缺口以可达运行时 Err
    /// 表达（既非编译错误、也非“纯 `(kind, cred)` 投影”）。
    ///
    /// `None` 输入有两条**不同点、不同职责、不同结果语义**的路径：
    /// - **(i) proxy 分发路径**——`auth = None` 在 `proxy/auth.rs` 即走**透传成功路径**、
    ///   根本不调 facade/resolve（现状原样、行为零变化，此处不是错误路径）；
    /// - **(ii) 直接调用路径**（CLI/admin/新调用点直接 `resolve`）——本臂返回 `Err`，
    ///   对齐现状 `oauth_account_for_provider` 的 "not OAuth-backed"。
    ///
    /// L1 = (i) 的调用点选择（不产错、只不调用），L2 = (ii) 的函数内映射（出 Err）；
    /// **Err① 可达性由 (ii) 背书、不依赖 L1**。
    ///
    /// `Err` 三支可达语义输入（逐支单测钉死）：
    /// ① **直接调用路径**下 `auth = None`（proxy 分发路径不经本函数——见上 (i)）
    ///    → `not OAuth-backed`；
    /// ② `normalized_for_provider` 补全后 `account` 仍空（provider_id 空/纯空白）；
    /// ③ `ApiKeyEnv` 的 `env` **配置值本身**为空串（配置损坏）——运行时环境变量未设置
    ///    是另一回事，由 source 层 `NotFound` 承接，两者语义不同、各有测试。
    ///
    /// 本函数是**热路径**（每次取数第一步），非异常路径。签名校准：
    /// 需显式收 `provider_id`——它是 `providers` map 的键、不是 `ProviderConfig` 字段，
    /// 且 `auth_config(provider_id)` 的 account 补全同样依赖它。
    pub fn resolve(provider_id: &str, cfg: &ProviderConfig) -> Result<Self> {
        let auth = cfg.auth_config(provider_id)?;
        let (kind, cred) = match auth {
            AuthConfig::ApiKeyEnv { env } => {
                if env.trim().is_empty() {
                    bail!(
                        "api_key_env environment variable name is empty for provider {provider_id:?}"
                    );
                }
                (AuthKind::ApiKeyEnv, CredentialKey::Env(env))
            }
            AuthConfig::OpenaiOauth { account } => {
                let account = account
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        anyhow!("provider {provider_id:?} resolves to an empty OpenAI OAuth account name")
                    })?;
                (AuthKind::OpenaiOauth, CredentialKey::Account(account))
            }
            AuthConfig::None => bail!("provider {provider_id:?} is not OAuth-backed"),
        };
        Ok(Self {
            kind,
            cred,
            provider_id: provider_id.to_string(),
        })
    }

    pub fn kind(&self) -> AuthKind {
        self.kind
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub(crate) fn env(&self) -> Option<&str> {
        match &self.cred {
            CredentialKey::Env(env) => Some(env),
            CredentialKey::Account(_) => None,
        }
    }

    pub(crate) fn account(&self) -> Option<&str> {
        match &self.cred {
            CredentialKey::Account(account) => Some(account),
            CredentialKey::Env(_) => None,
        }
    }
}

/// ctx 只含环境性依赖（时钟 + HTTP + 上游地址）——storage 不在其中
/// （上游地址同为环境性依赖，参数多则集中 ctx 注入）。
pub struct AuthCtx<'a> {
    /// 时钟注入（类型校准：crate 用 chrono，设计示例的 `OffsetDateTime` 换为 `DateTime<Utc>`）。
    pub now: DateTime<Utc>,
    /// HTTP 客户端注入（类型校准：`http_client` 是客户端**工厂模块**、无 `Client` 类型——
    /// 客户端类型 = `reqwest::Client`；转发载体内部自建客户端，本字段由接线路径消费）。
    pub http: &'a reqwest::Client,
    /// 上游地址：生产按 kind 从 `state.oauth_token_urls` 构造 OAuth 两 kind 的真实值；
    /// **ApiKeyEnv 构造 `UpstreamUrls::none()`**（两 None——不消费、无假串填充）；
    /// **测试注入假上游（mock 保护）**。
    pub upstream: UpstreamUrls,
}

/// 上游地址集（`token_url` 为 `Option`——ApiKey 显式 None，
/// OAuth impl 取 url = `ok_or(可达语义 Err)`，测试忘填 → 明确报错，无 panic、无不可达分支）。
///
/// 消费面：读 `upstream` 的只有 refresh/flow 方法；`is_expired`
/// 纯时间判断、不读 url；**`ApiKeySource` 不实现 `OAuthAccount`——refresh/flow 对
/// ApiKey 类型上不可达**；**`validate_account` 签名无 ctx（结构上不可能读 upstream）**；
/// `get_token_valid`/`peek_token` 语义上不读 url → `none()` 对 ApiKeyEnv 全流程安全。
pub struct UpstreamUrls {
    pub token_url: Option<String>,
    #[allow(dead_code)]
    pub userinfo_url: Option<String>,
}

impl UpstreamUrls {
    /// 无上游（ApiKeyEnv 场景；两 None，不填假串）。
    pub fn none() -> Self {
        Self {
            token_url: None,
            userinfo_url: None,
        }
    }
}

/// 账号分组：每个参与存储的 OAuth 认证类型（`AuthKind` 的 OAuth 变体）一个类型化聚合
/// 容器字段 = `AccountGroup<Self>`（实际形状 `HashMap<账号名, Account>`）；
/// **provider 维度不在字段里**——同类型多 provider 共用一个字段、经账号/查询键区分。
pub type AccountGroup<P> = HashMap<String, <P as ProviderAuth>::Account>;

/// 账号存储侧（oauth_accounts.json；ApiKey 不参与）。
pub trait ProviderAuth: Send + Sync {
    // 结构载体；消费方：source 刷新事件 `auth_kind` 与写回校验标签。
    const KIND: AuthKind;
    type Account: Serialize + DeserializeOwned + Send + Sync;

    /// 投影到本类型在共享容器 `AccountsFile`（`auth/types.rs`）中的类型化字段。
    fn group(container: &mut AccountsFile) -> &mut AccountGroup<Self>;

    /// 校验单条已反序列化条目。签名校准：显式收 `id`（map 键）——
    /// 现有校验函数 `validate_*_account(id, acc)` 都需要键（`raw` 单参放不下）。
    /// 消费方：`framework::with_transaction` 写回前对**变化条目**校验
    /// （失败仅告警不阻断——行为保持约束，见 framework 注）。
    fn validate_account(id: &str, raw: &Self::Account) -> Result<()>;

    fn is_expired(account: &Self::Account, ctx: &AuthCtx<'_>) -> bool;
}

pub trait OAuthAccount: ProviderAuth {
    /// **同步**签名。实现内部经 `tokio::runtime::Handle::block_on`
    /// 执行 HTTP 刷新——**接线必须以 `block_in_place`/`spawn_blocking` 语义从
    /// async 侧进入**（`Handle::block_on` 在 async 上下文内的约束以测试钉死）；
    /// 无 Tokio 运行时上下文 → `Err`（可达语义，非 panic）。
    /// 注意：anti 的 email 回填需要整容器（查重），**不在本方法内**——保留在
    /// `refresh.rs` 文件级事务（接线时按闭包可见域决定归属）。
    fn refresh(ctx: &AuthCtx<'_>, account: &mut Self::Account) -> Result<()>;
}

/// 授权码流（OAuth authorization-code flow）。**AFIT（async fn in trait，静态分发、不进 `dyn`）**——
/// 「async/progress 不走 trait object」；各方法**全转发现有函数**。
pub trait AuthorizationCodeFlow: OAuthAccount {
    /// 现状对位 attempt（1:1 不合并）；接线对位校核。
    type Pending;
    /// = 进度回调发送器（现状函数收 `Option<…>`）。
    type Progress;

    /// 同步（现状 `generate_…_attempt` 为同步）。
    #[allow(dead_code)]
    fn build_login(&self, ctx: &AuthCtx<'_>) -> Result<Self::Pending>;

    /// AFIT（现状对位函数为 async、带 progress）。
    #[allow(dead_code)]
    async fn finish_login(
        &self,
        ctx: &AuthCtx<'_>,
        pending: &Self::Pending,
        code: &str,
        progress: Option<Self::Progress>,
    ) -> Result<Self::Account>;

    /// AFIT（现状对位函数为 async、带 progress）。
    #[allow(dead_code)] // TUI Connect 消费 `LoginResult.pending_account`（后续
    // onboard 挂起流程），与本契约 `Account` 返回形状不同——trait 化会改 TUI 可见行为
    // （零破坏约束）→ 生产面保持 auth 内部门面 flow 的对位函数；
    // 本方法 = 分步契约载体，接上等价消费方即撤 allow。
    async fn exchange_pending(
        &self,
        ctx: &AuthCtx<'_>,
        pending: &Self::Pending,
        progress: Option<Self::Progress>,
    ) -> Result<Self::Account>;
}

/// 设备码流（openai）。AFIT 同上，全转发现有函数。
pub trait DeviceFlow: OAuthAccount {
    type DeviceCode;
    type DevicePoll;

    /// → `request_openai_device_code`（device 端点 = crate 常量，与 `login.rs`
    /// 生产调用一致；`ctx.upstream` 专供 token/refresh 路径——按现状校准）。
    #[allow(dead_code)] // `device_login` 为唯一生产端到端消费（整流程封装，
    // 语义含 request/poll 等价步骤）；分步消费方（TUI 分步屏）未接入——契约载体保留。
    async fn request_device_code(&self, ctx: &AuthCtx<'_>) -> Result<Self::DeviceCode>;

    /// → `poll_openai_device_token`（device 端点常量同上）。
    #[allow(dead_code)] // 同上（分步契约载体）
    async fn poll_device_token(
        &self,
        ctx: &AuthCtx<'_>,
        code: &Self::DeviceCode,
    ) -> Result<Option<Self::DevicePoll>>;

    /// → `login_openai_device_with_urls`（self 替代 config_path/path 位置参数）。
    async fn device_login(&self, ctx: &AuthCtx<'_>) -> Result<Self::Account>;
}

/// 取数侧（**同步、对象安全**——`&dyn TokenSource` 分发面；async 工作在
/// 实现内部以 block_on 承接，见 `OAuthAccount::refresh` 注）。
pub trait TokenSource: Send + Sync {
    /// 只读，无锁。
    #[allow(dead_code)] // 无自然生产消费方（quota 取数模块休眠无调用方、
    // 展示面读全量条目而非单 token）——保留 admin/CLI 能力面与测试注入；接上生产面即撤。
    fn peek_token(&self, q: &TokenQuery) -> Result<String>;
    /// 事务内读→判→刷→写。
    fn get_token_valid(&self, ctx: &AuthCtx<'_>, q: &TokenQuery) -> Result<String>;
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use serde_json::json;

    fn provider(raw: serde_json::Value) -> ProviderConfig {
        serde_json::from_value(raw).expect("ProviderConfig must deserialize from test JSON")
    }

    /// 逐 AuthConfig 变体单测断言（L2 保证）。
    #[test]
    fn resolve_api_key_env_variant() {
        let cfg = provider(json!({"auth": {"type": "api_key_env", "env": "MY_KEY"}}));
        let q = TokenQuery::resolve("p1", &cfg).expect("ApiKeyEnv resolves");
        assert_eq!(q.kind(), AuthKind::ApiKeyEnv);
        assert_eq!(q.env(), Some("MY_KEY"));
        assert_eq!(q.account(), None);
        assert_eq!(q.provider_id(), "p1");
    }

    /// (Some(env), None) 顶层 api_key_env 折叠路径（auth_config 既有语义）。
    #[test]
    fn resolve_top_level_api_key_env_folds_to_apikey_kind() {
        let cfg = provider(json!({"api_key_env": "TOP_KEY"}));
        let q = TokenQuery::resolve("p1", &cfg).expect("api_key_env folds");
        assert_eq!(q.kind(), AuthKind::ApiKeyEnv);
        assert_eq!(q.env(), Some("TOP_KEY"));
    }

    #[test]
    fn resolve_openai_missing_account_normalizes_to_provider_id() {
        let cfg = provider(json!({"auth": {"type": "openai_oauth"}}));
        let q = TokenQuery::resolve("prov-x", &cfg).expect("normalized account fills");
        assert_eq!(q.kind(), AuthKind::OpenaiOauth);
        assert_eq!(q.account(), Some("prov-x"));
        assert_eq!(q.env(), None);
    }

    /// Err①：直接调用路径下 auth = None → "not OAuth-backed"（对齐现状
    /// oauth_account_for_provider 文案；proxy 分发路径不经本函数）。
    #[test]
    fn resolve_auth_none_errors_not_oauth_backed() {
        let cfg = provider(json!({"auth": {"type": "none"}}));
        let err = TokenQuery::resolve("p1", &cfg).expect_err("None must not resolve");
        assert!(
            err.to_string().contains("not OAuth-backed"),
            "unexpected error: {err}"
        );
    }

    /// Err③：env 配置值本身为空串（配置损坏）——与运行时环境变量未设置
    ///（source 层 NotFound）语义不同。
    #[test]
    fn resolve_empty_env_name_errors() {
        let cfg = provider(json!({"auth": {"type": "api_key_env", "env": ""}}));
        let err = TokenQuery::resolve("p1", &cfg).expect_err("empty env must not resolve");
        assert!(
            err.to_string().contains("is empty"),
            "unexpected error: {err}"
        );
    }

    /// Err②：normalized 补全后 account 仍空（provider_id 空 → account 空串）。
    #[test]
    fn resolve_empty_account_after_normalization_errors() {
        let cfg = provider(json!({"auth": {"type": "openai_oauth"}}));
        let err = TokenQuery::resolve("", &cfg).expect_err("empty provider_id must not resolve");
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }
}
