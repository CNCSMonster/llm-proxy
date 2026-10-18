//! ApiKey 认证类型——**仅实现 `TokenSource`**（ApiKey 不进 `ProviderAuth` 体系、
//! 无账号存储、无 `AccountsFile` 字段）。
//!
//! 分发接缝已接线（`ApiKeySource` 进 registry、facade apikey 臂）——
//! 模块级 `allow(dead_code)` 已撤（原则留档：只压 unused、不豁免穷尽/通配 lint）。

use anyhow::{Result, anyhow};

use super::traits::{AuthCtx, TokenQuery, TokenSource};

/// 环境变量凭证源：`TokenQuery::env()` 定位，无状态。
#[derive(Debug, Default, Clone, Copy)]
pub struct ApiKeySource;

impl TokenSource for ApiKeySource {
    fn peek_token(&self, q: &TokenQuery) -> Result<String> {
        read_env(q)
    }

    fn get_token_valid(&self, _ctx: &AuthCtx<'_>, q: &TokenQuery) -> Result<String> {
        // ApiKey 无缓存/无刷新：valid 与 peek 同语义（读环境变量）。
        read_env(q)
    }
}

/// 环境变量读取。两种失败语义（各有测试）：
/// - `TokenQuery` 不携带 env 名（kind 错配）→ 结构性 `Err`（resolve 单点保证下不应发生）；
/// - 变量未设置 → **`NotFound` 语义的 `Err`**（运行时缺失；与 resolve Err③ 的
///   「配置 env 名为空串」是两回事）。
fn read_env(q: &TokenQuery) -> Result<String> {
    let env = q.env().ok_or_else(|| {
        anyhow!(
            "TokenQuery for provider {:?} carries no environment variable name (kind mismatch)",
            q.provider_id()
        )
    })?;
    std::env::var(env).map_err(|_| {
        anyhow!(
            "missing API key environment variable {env} for provider {:?}",
            q.provider_id()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::traits::UpstreamUrls;
    use super::*;
    use crate::config::ProviderConfig;
    use serde_json::json;
    use serial_test::serial;

    fn query(env_field: serde_json::Value) -> TokenQuery {
        let cfg: ProviderConfig =
            serde_json::from_value(json!({"auth": env_field})).expect("provider config");
        TokenQuery::resolve("prov", &cfg).expect("resolve")
    }

    #[test]
    #[serial]
    fn peek_and_valid_agree_for_set_variable() {
        let key = "LLM_PROXY_APIKEY_PROBE";
        // SAFETY: test-only environment variable mutation under #[serial]
        // （仅本测试写该变量；唯一键，无其他写者）
        unsafe {
            std::env::set_var(key, "probe-value");
        }
        let q = query(json!({"type": "api_key_env", "env": key}));
        let source = ApiKeySource;
        let http = reqwest::Client::new();
        let ctx = AuthCtx {
            now: chrono::Utc::now(),
            http: &http,
            upstream: UpstreamUrls::none(),
        };
        assert_eq!(source.peek_token(&q).expect("peek"), "probe-value");
        assert_eq!(
            source.get_token_valid(&ctx, &q).expect("valid"),
            "probe-value"
        );
    }

    #[test]
    fn missing_variable_is_runtime_not_found() {
        let q = query(json!({"type": "api_key_env", "env": "LLM_PROXY_ABSENT_VAR"}));
        let source = ApiKeySource;
        let err = source.peek_token(&q).expect_err("unset var must fail");
        assert!(
            err.to_string()
                .contains("missing API key environment variable"),
            "unexpected error: {err}"
        );
    }
}
