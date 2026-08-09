//! 订阅额度查询（Quota Manager）。
//!
//! 遍历配置中 OAuth 认证的订阅类 provider（openai-sub / google-antigravity），
//! 调用对应客户端查询额度并标准化为 `QuotaInfo`。
//! 设计见 `docs/design/quota-query-feature.md`，调研见 `docs/research/quota-api-research.md`。

pub mod antigravity;
pub mod chatgpt;
pub mod types;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;

pub use types::QuotaInfo;

/// 查询所有订阅类 provider 的额度（从配置文件 + OAuth 账号文件读取）。
///
/// 单个 provider 失败不中断整体：错误打印到 stderr 后跳过；
/// 全部失败时返回聚合错误（不 panic）。
pub async fn fetch_quota(config_path: &Path) -> Result<Vec<QuotaInfo>> {
    let cfg = crate::config::Config::load(config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let accounts =
        crate::auth::load_oauth_accounts_with_recovery(&crate::auth::default_state_path())
            .context("failed to load OAuth accounts")?;
    fetch_quota_with_accounts(
        &cfg,
        &accounts,
        chatgpt::CHATGPT_BACKEND_URL,
        antigravity::ANTIGRAVITY_LOAD_CODE_ASSIST_URL,
    )
    .await
}

/// 给定配置、OAuth 账号集合与端点地址逐 provider 查询额度（可测试内层）。
///
/// 端点地址参数化以支持测试传入本地 mock 服务器（与 `src/auth/login.rs` 的
/// URL 参数化风格一致）。`chatgpt_backend` 为后端根地址（挂 `/wham/usage`），
/// `antigravity_url` 为 loadCodeAssist 完整地址。
pub async fn fetch_quota_with_accounts(
    cfg: &crate::config::Config,
    accounts: &crate::auth::OAuthAccounts,
    chatgpt_backend: &str,
    antigravity_url: &str,
) -> Result<Vec<QuotaInfo>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let mut infos = Vec::new();
    let mut errors = Vec::new();

    for (provider_id, provider) in &cfg.providers {
        // 仅处理 OAuth 认证的订阅类 provider；API key / 无认证 provider 跳过
        let is_oauth = matches!(
            provider.auth_config(provider_id),
            Ok(crate::config::AuthConfig::OpenaiOauth { .. })
                | Ok(crate::config::AuthConfig::AntigravityOauth { .. })
        );
        if !is_oauth {
            continue;
        }
        match fetch_one(
            &client,
            provider,
            accounts,
            provider_id,
            chatgpt_backend,
            antigravity_url,
        )
        .await
        {
            Ok(info) => infos.push(info),
            Err(e) => errors.push(format!("{provider_id}: {e:#}")),
        }
    }

    if infos.is_empty() && !errors.is_empty() {
        bail!(
            "quota query failed for all providers:\n{}",
            errors.join("\n")
        );
    }
    for error in &errors {
        eprintln!("⚠ quota query failed: {error}");
    }
    Ok(infos)
}

/// 查询单个 provider 的额度（token 获取 + 对应客户端调用）。
async fn fetch_one(
    client: &Client,
    provider: &crate::config::ProviderConfig,
    accounts: &crate::auth::OAuthAccounts,
    provider_id: &str,
    chatgpt_backend: &str,
    antigravity_url: &str,
) -> Result<QuotaInfo> {
    match provider.auth_config(provider_id)? {
        crate::config::AuthConfig::OpenaiOauth { account } => {
            let account_id = account.unwrap_or_else(|| provider_id.to_string());
            // 检查 token 是否过期，提供清晰的刷新提示
            if let Some(openai_account) = accounts.openai.get(&account_id)
                && openai_account.is_expired()
            {
                bail!(
                    "OpenAI token expired for account '{}'. Please run: llm-proxy provider refresh {}",
                    openai_account.account_label,
                    provider_id
                );
            }
            let token =
                crate::auth::get_openai_token_from_accounts(accounts, &account_id, provider_id)?;
            let chatgpt_account_id = chatgpt::chatgpt_account_id_from_jwt(&token)?;
            chatgpt::fetch_chatgpt_quota(
                client,
                chatgpt_backend,
                &token,
                &chatgpt_account_id,
                provider_id,
            )
            .await
        }
        crate::config::AuthConfig::AntigravityOauth { account } => {
            let account_id = account.unwrap_or_else(|| provider_id.to_string());
            // 检查 token 是否过期，提供清晰的刷新提示
            if let Some(ag_account) = accounts.antigravity.get(&account_id)
                && ag_account.is_expired()
            {
                bail!(
                    "Antigravity token expired for account '{}'. Please run: llm-proxy provider refresh {}",
                    ag_account.account_label,
                    provider_id
                );
            }
            let token = crate::auth::get_antigravity_token_from_accounts(
                accounts,
                &account_id,
                provider_id,
            )?;
            antigravity::fetch_antigravity_quota(client, antigravity_url, &token, provider_id).await
        }
        _ => bail!("provider {provider_id} is not OAuth-backed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AntigravityAccount, OAuthAccounts, OpenaiAccount};
    use crate::config::{AuthConfig, Config, ProviderConfig, ServerConfig};

    fn now_plus(seconds: i64) -> i64 {
        crate::quota::types::unix_secs() + seconds
    }

    fn test_openai_account(access_token: &str) -> OpenaiAccount {
        OpenaiAccount {
            account_label: "user@example.com".to_string(),
            access_token: access_token.to_string(),
            refresh_token: "refresh-token".to_string(),
            expires_at_unix: now_plus(3600),
            updated_at_unix: now_plus(-100),
        }
    }

    fn test_antigravity_account() -> AntigravityAccount {
        AntigravityAccount {
            account_label: "user@example.com".to_string(),
            project_id: "test-project-1".to_string(),
            access_token: "ag-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            expires_at_unix: now_plus(3600),
            updated_at_unix: now_plus(-100),
        }
    }

    /// 带 `chatgpt_account_id` claim 的假 JWT（与 chatgpt.rs 测试同构）。
    fn fake_chatgpt_jwt(account_id: &str) -> String {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        let header = serde_json::json!({"alg": "none", "typ": "JWT"});
        let payload = serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
        });
        let b64 = |v: &serde_json::Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
        format!("{}.{}.sig", b64(&header), b64(&payload))
    }

    fn config_with(providers: Vec<(&str, AuthConfig)>) -> Config {
        let providers = providers
            .into_iter()
            .map(|(id, auth)| {
                (
                    id.to_string(),
                    ProviderConfig {
                        auth: Some(auth),
                        ..ProviderConfig::default()
                    },
                )
            })
            .collect();
        Config {
            server: ServerConfig {
                listen: "127.0.0.1:0".to_string(),
                usage: Default::default(),
                max_sse_buffer_bytes: crate::config::default_max_sse_buffer_bytes(),
                max_output_items: crate::config::default_max_output_items(),
            },
            fallback: Default::default(),
            protection: Default::default(),
            status: Default::default(),
            providers,
            models: Default::default(),
        }
    }

    /// 一个 mock 服务器同时挂 wham/usage 与 loadCodeAssist 两个端点。
    async fn mock_quota_server() -> String {
        let app = axum::Router::new()
            .route(
                "/wham/usage",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "plan_type": "plus",
                        "rate_limit": {
                            "primary_window": {
                                "used_percent": 50,
                                "limit_window_seconds": 604800,
                                "reset_after_seconds": 316632,
                                "reset_at": 1786184630
                            }
                        }
                    }))
                }),
            )
            .route(
                "/load",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({
                        "allowedTiers": [
                            {"id": "standard-tier", "name": "Gemini Code Assist", "isDefault": true}
                        ]
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn fetches_all_oauth_providers() {
        let base = mock_quota_server().await;
        let cfg = config_with(vec![
            (
                "openai-sub",
                AuthConfig::OpenaiOauth {
                    account: Some("openai-subscription".to_string()),
                },
            ),
            (
                "google-antigravity",
                AuthConfig::AntigravityOauth {
                    account: Some("antigravity".to_string()),
                },
            ),
        ]);
        let mut accounts = OAuthAccounts::new();
        accounts.openai.insert(
            "openai-subscription".to_string(),
            test_openai_account(&fake_chatgpt_jwt("4ac60e19-abc")),
        );
        accounts
            .antigravity
            .insert("antigravity".to_string(), test_antigravity_account());

        let infos = fetch_quota_with_accounts(&cfg, &accounts, &base, &format!("{base}/load"))
            .await
            .expect("fetch");
        assert_eq!(infos.len(), 2);

        let openai = infos
            .iter()
            .find(|i| i.provider_id == "openai-sub")
            .expect("openai");
        assert_eq!(openai.plan_type.as_deref(), Some("plus"));
        assert_eq!(openai.used_percent, Some(50.0));
        assert_eq!(openai.limit_window_seconds, Some(604800));
        assert_eq!(openai.reset_at_unix, Some(1786184630));

        let ag = infos
            .iter()
            .find(|i| i.provider_id == "google-antigravity")
            .expect("antigravity");
        assert_eq!(ag.plan_type.as_deref(), Some("Gemini Code Assist"));
        assert_eq!(ag.used_percent, None);
    }

    #[tokio::test]
    async fn continues_on_single_provider_failure() {
        let base = mock_quota_server().await;
        // openai-sub 的账号未登录（accounts 里没有）→ 该 provider 失败；
        // google-antigravity 正常 → 返回 1 条结果。
        let cfg = config_with(vec![
            (
                "openai-sub",
                AuthConfig::OpenaiOauth {
                    account: Some("openai-subscription".to_string()),
                },
            ),
            (
                "google-antigravity",
                AuthConfig::AntigravityOauth {
                    account: Some("antigravity".to_string()),
                },
            ),
        ]);
        let mut accounts = OAuthAccounts::new();
        accounts
            .antigravity
            .insert("antigravity".to_string(), test_antigravity_account());

        let infos = fetch_quota_with_accounts(&cfg, &accounts, &base, &format!("{base}/load"))
            .await
            .expect("partial success");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].provider_id, "google-antigravity");
    }

    #[tokio::test]
    async fn errors_when_all_providers_fail() {
        let base = mock_quota_server().await;
        let cfg = config_with(vec![(
            "openai-sub",
            AuthConfig::OpenaiOauth {
                account: Some("openai-subscription".to_string()),
            },
        )]);
        // accounts 为空 → token 获取失败 → 全部失败 → Err
        let accounts = OAuthAccounts::new();
        let err = fetch_quota_with_accounts(&cfg, &accounts, &base, &format!("{base}/load"))
            .await
            .expect_err("all providers failed");
        assert!(
            err.to_string().contains("openai-sub"),
            "error should mention provider: {err}"
        );
    }

    #[tokio::test]
    async fn skips_non_oauth_providers() {
        let base = mock_quota_server().await;
        // 纯 API key provider + 无认证 provider：不发起任何请求，返回空列表
        let cfg = config_with(vec![
            (
                "deepseek",
                AuthConfig::ApiKeyEnv {
                    env: "DEEPSEEK_API_KEY".to_string(),
                },
            ),
            ("local", AuthConfig::None),
        ]);
        let accounts = OAuthAccounts::new();
        let infos = fetch_quota_with_accounts(&cfg, &accounts, &base, &format!("{base}/load"))
            .await
            .expect("empty ok");
        assert!(infos.is_empty());
    }

    #[tokio::test]
    async fn expired_token_returns_clear_error() {
        let base = mock_quota_server().await;
        let cfg = config_with(vec![(
            "openai-sub",
            AuthConfig::OpenaiOauth {
                account: Some("openai-subscription".to_string()),
            },
        )]);
        let mut accounts = OAuthAccounts::new();
        // 创建一个已过期的账号（expires_at_unix 在过去）
        accounts.openai.insert(
            "openai-subscription".to_string(),
            OpenaiAccount {
                account_label: "user@example.com".to_string(),
                access_token: fake_chatgpt_jwt("4ac60e19-abc"),
                refresh_token: "refresh-token".to_string(),
                expires_at_unix: now_plus(-3600), // 1 小时前过期
                updated_at_unix: now_plus(-7200),
            },
        );

        let err = fetch_quota_with_accounts(&cfg, &accounts, &base, &format!("{base}/load"))
            .await
            .expect_err("expired token should fail");
        assert!(
            err.to_string().contains("token expired")
                && err.to_string().contains("provider refresh"),
            "error should mention expiry and refresh command: {err}"
        );
    }
}
