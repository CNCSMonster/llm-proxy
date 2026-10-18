//! Proxy submodule extracted from `proxy::mod`.

use super::*;
use crate::config::AuthConfig;

// Err 是 axum Response（≥128B，包含 body），作为错误类型返回是接口约定；
// 改动签名（如 Box 化）会波及所有调用方，且错误路径本身构造代价低，故 allow。
#[allow(clippy::result_large_err)]
pub(super) async fn apply_bearer_auth(
    state: &AppState,
    plan: &ExecutionPlan,
    req: reqwest::RequestBuilder,
    err: fn(StatusCode, &str) -> Response,
) -> Result<reqwest::RequestBuilder, Response> {
    match resolve_plan_token(state, plan, err).await? {
        Some(token) => Ok(req.bearer_auth(token)),
        None => Ok(req),
    }
}

#[allow(clippy::result_large_err)]
pub(super) async fn apply_anthropic_auth(
    state: &AppState,
    plan: &ExecutionPlan,
    req: reqwest::RequestBuilder,
    err: fn(StatusCode, &str) -> Response,
) -> Result<reqwest::RequestBuilder, Response> {
    match resolve_plan_token(state, plan, err).await? {
        Some(token) => match plan.auth {
            AuthConfig::ApiKeyEnv { .. } => Ok(req.header("x-api-key", token)),
            AuthConfig::OpenaiOauth { .. } => Ok(req.bearer_auth(token)),
            AuthConfig::None => Ok(req),
        },
        None => Ok(req),
    }
}
#[allow(clippy::result_large_err)]
pub(super) async fn resolve_plan_token(
    state: &AppState,
    plan: &ExecutionPlan,
    err: fn(StatusCode, &str) -> Response,
) -> Result<Option<String>, Response> {
    Ok(resolve_plan_auth(state, plan, err).await?.token)
}
#[allow(clippy::result_large_err)]
pub(super) async fn resolve_plan_auth(
    state: &AppState,
    plan: &ExecutionPlan,
    err: fn(StatusCode, &str) -> Response,
) -> Result<crate::config::ResolvedAuth, Response> {
    // OAuth provider：从内存缓存取 token（§15.2，避免每次请求读文件）
    match &plan.auth {
        crate::config::AuthConfig::OpenaiOauth { account } => {
            let account_id = account.clone().unwrap_or_else(|| plan.provider_id.clone());
            let (token, _project_id) =
                resolve_oauth_auth(state, &account_id, &plan.provider_id, "openai_oauth")
                    .await
                    .map_err(|e| {
                        err(
                            StatusCode::UNAUTHORIZED,
                            &format!(
                                "provider {:?} authentication is not ready: {e:#}",
                                plan.provider_id
                            ),
                        )
                    })?;
            Ok(crate::config::ResolvedAuth {
                token: Some(token),
                project_id: None,
            })
        }
        _ => state
            .config()
            .resolve_auth_at(&plan.provider_id, &state.oauth_accounts_path)
            .map_err(|e| {
                err(
                    StatusCode::UNAUTHORIZED,
                    &format!(
                        "provider {:?} authentication is not ready: {e:#}",
                        plan.provider_id
                    ),
                )
            }),
    }
}

/// 从内存 OAuth 缓存取 token + project_id（同一快照）。
/// - 账号不存在 → 报错（请 login），不刷新
/// - 过期 → 刷新（全局刷新锁合并并发 + double-check；刷新后更新缓存 + 文件）
pub(super) async fn resolve_oauth_auth(
    state: &AppState,
    account_id: &str,
    provider_id: &str,
    auth_type: &str,
) -> anyhow::Result<(String, Option<String>)> {
    // 1. 读内存缓存（同一快照取 token + project_id）
    let initial = {
        let guard = state
            .oauth_accounts
            .read()
            .map_err(|_| anyhow::anyhow!("oauth cache lock poisoned"))?;
        oauth_token_from_guard(&guard, account_id, provider_id, auth_type)
    };
    if let Some(found) = initial {
        return Ok(found);
    }
    // 2. 账号不存在 vs 过期：不存在直接报错（请 login），不触发刷新
    {
        let guard = state
            .oauth_accounts
            .read()
            .map_err(|_| anyhow::anyhow!("oauth cache lock poisoned"))?;
        if !crate::auth::account_exists(&guard, account_id, auth_type) {
            anyhow::bail!(
                "OAuth account '{account_id}' not found; run: llm-proxy provider login {provider_id}"
            );
        }
    }
    // 3. 过期 → 刷新（刷新锁合并并发 + 锁内 double-check）
    let _refresh_guard = state.oauth_refresh_lock.lock().await;
    // double-check：等待锁期间其他请求可能已刷新
    {
        let guard = state
            .oauth_accounts
            .read()
            .map_err(|_| anyhow::anyhow!("oauth cache lock poisoned"))?;
        if let Some(found) = oauth_token_from_guard(&guard, account_id, provider_id, auth_type) {
            return Ok(found);
        }
    }
    // 真的需要刷新（纯 async，无需 block_in_place）
    let state = state.clone();
    let account_id = account_id.to_string();
    let provider_id = provider_id.to_string();
    let auth_type = auth_type.to_string();
    crate::auth::refresh_account_for_provider_with_context_and_urls(
        &state.oauth_accounts_path,
        &account_id,
        &auth_type,
        &state.oauth_token_urls.openai,
        crate::auth::RefreshContext::new(&provider_id, "proxy_expired_token"),
    )
    .await?;
    // 重新加载 → 更新内存缓存
    let accounts = crate::auth::load_oauth_accounts_with_recovery(&state.oauth_accounts_path)?;
    *state
        .oauth_accounts
        .write()
        .map_err(|_| anyhow::anyhow!("oauth cache lock poisoned"))? = accounts.clone();
    // 返回刷新后的 token + project_id（同一快照）
    oauth_token_from_guard(&accounts, &account_id, &provider_id, &auth_type)
        .ok_or_else(|| anyhow::anyhow!("OAuth account '{account_id}' disappeared after refresh"))
}

/// 从 OAuth 账号集合取 (token, Option<project_id>)（同一快照）。
/// 账号不存在或过期返回 None。
pub(super) fn oauth_token_from_guard(
    accounts: &crate::auth::AccountsFile,
    account_id: &str,
    provider_id: &str,
    auth_type: &str,
) -> Option<(String, Option<String>)> {
    match auth_type {
        "openai_oauth" => {
            let token =
                crate::auth::get_openai_token_from_accounts(accounts, account_id, provider_id)
                    .ok()?;
            Some((token, None))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AccountsFile, OpenaiAccount};
    use crate::config::{
        AdapterKind, AuthConfig, CompatConfig, Config, FallbackConfig, Protocol, ProviderConfig,
        RequestFrequencyConfig, ServerConfig, StatusConfig,
    };
    use serial_test::serial;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, RwLock};

    fn err(status: StatusCode, msg: &str) -> Response {
        (status, msg.to_string()).into_response()
    }

    fn cfg_with_provider(provider_id: &str, auth: AuthConfig) -> Config {
        let mut providers = BTreeMap::new();
        providers.insert(
            provider_id.to_string(),
            ProviderConfig {
                auth: Some(auth),
                ..Default::default()
            },
        );
        Config {
            server: Some(ServerConfig {
                listen: "127.0.0.1:0".into(),
                usage: Default::default(),
                max_sse_buffer_bytes: 1,
                max_output_items: 1,
                proxy: None,
                require_auth: None,
                auth_token: None,
                auth_token_env: None,
                management_port: None,
            }),
            remote: None,
            fallback: FallbackConfig::default(),
            protection: Default::default(),
            diagnostics: Default::default(),
            status: StatusConfig::default(),
            codex: crate::config::CodexConfig::default(),
            launch: crate::config::LaunchConfig::default(),
            providers,
            models: BTreeMap::new(),
        }
    }

    fn state(cfg: Config, accounts: AccountsFile) -> AppState {
        let active = Arc::new(crate::probe_coordinator::ActiveProviderStore::default());
        let test_dir = Arc::new(tempfile::tempdir().expect("isolated auth fixture"));
        let config_path = test_dir.path().join("config.toml");
        let test_dirs = crate::service::StateDirs::for_test(test_dir.path());
        let state_dir = test_dirs.runtime_dir.clone();
        let generation = crate::runtime_config::GenerationReader::new(cfg.clone());
        let core = Arc::new(tokio::sync::Mutex::new(
            crate::core::CoreState::from_config_with_state_dirs(
                cfg.clone(),
                config_path.clone(),
                test_dirs.clone(),
            ),
        ));
        let fatal_config_state = crate::runtime_config::FatalConfigState::default();
        let (config_writer, config_writer_task) = crate::runtime_config::ConfigWriterHandle::start(
            uuid::Uuid::new_v4().as_u128(),
            256,
            generation.clone(),
            core.clone(),
            config_path.clone(),
            state_dir.clone(),
            fatal_config_state.clone(),
        );
        AppState {
            generation,
            config_path,
            state_dirs: test_dirs.clone(),
            state_dir,
            config_writer,
            config_writer_task: Arc::new(tokio::sync::Mutex::new(Some(config_writer_task))),
            fatal_config_state,
            client: reqwest::Client::new(),
            oauth_accounts_path: test_dir.path().join("oauth_accounts.json"),
            cooldowns: Arc::new(CooldownStore::in_memory()),
            oauth_accounts: Arc::new(RwLock::new(accounts)),
            oauth_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            oauth_token_urls: OAuthTokenUrls::default(),
            bad_requests: Arc::new(BadRequestManager::new(Default::default())),
            error_cooldown: Arc::new(crate::protection::TokenErrorClassCooldown::new(
                Default::default(),
            )),
            account_budget: Arc::new(
                crate::protection::AccountBadBudgetManager::in_memory_fallback(Default::default()),
            ),
            stream_interruptions: Arc::new(Mutex::new(BTreeMap::new())),
            frequency: Arc::new(tokio::sync::Mutex::new(FrequencyState::default())),
            round_robin_cursors: Arc::new(std::sync::Mutex::new(HashMap::new())),
            usage_store: None,
            diagnostics: None,
            active_providers: active.clone(),
            probe_coordinator: Arc::new(crate::probe_coordinator::ProbeCoordinator::new(
                crate::probe_coordinator::ServerProbeState::with_cache_path_and_oauth_path(
                    active.clone(),
                    crate::status::cache_path_in(&test_dirs.state_dir),
                    test_dir.path().join("oauth_accounts.json"),
                )
                .with_cooldown_store(Arc::new(CooldownStore::in_memory())),
            )),
            core,
            token: crate::token::auth::TokenAuthState::new(
                crate::token::config::TokensConfig::default(),
            ),
            usage_history_limiter: Arc::new(super::UsageHistoryRateLimiter::new()),
            streaming_metrics: Arc::new(crate::streaming_metrics::StreamingMetrics::new()),
            provider_metrics: Arc::new(crate::provider_metrics::ProviderModelMetrics::new()),
            test_dir: Some(test_dir),
        }
    }

    fn plan(provider_id: &str, auth: AuthConfig) -> ExecutionPlan {
        ExecutionPlan {
            frontend_protocol: Protocol::OpenaiResponses,
            provider_id: provider_id.into(),
            upstream_model: "m".into(),
            source_protocol: Protocol::OpenaiResponses,
            adapter: AdapterKind::Passthrough,
            native_url: "http://upstream.test/v1/responses".into(),
            auth,
            compat: CompatConfig::default(),
            anthropic_family_models: vec![],
            store: None,
            request_frequency: RequestFrequencyConfig::default(),
            routed_target: None,
        }
    }

    fn future() -> i64 {
        4_102_444_800
    }
    fn past() -> i64 {
        1
    }
    fn openai(token: &str, exp: i64) -> OpenaiAccount {
        OpenaiAccount {
            account_label: "oa".into(),
            access_token: token.into(),
            refresh_token: "refresh-openai-token-long".into(),
            expires_at_unix: exp,
            updated_at_unix: 1,
        }
    }
    #[tokio::test]
    #[serial]
    async fn none_auth_resolves_no_token() {
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            AccountsFile::new(),
        );
        assert_eq!(
            resolve_plan_token(&s, &plan("p", AuthConfig::None), err)
                .await
                .unwrap(),
            None
        );
    }
    #[tokio::test]
    #[serial]
    async fn missing_provider_returns_unauthorized() {
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            AccountsFile::new(),
        );
        assert!(
            resolve_plan_auth(&s, &plan("missing", AuthConfig::None), err)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    #[serial]
    async fn api_key_env_missing_is_ok_none() {
        let s = state(
            cfg_with_provider(
                "p",
                AuthConfig::ApiKeyEnv {
                    env: "AUTH_TEST_ABSENT".into(),
                },
            ),
            AccountsFile::new(),
        );
        unsafe {
            std::env::remove_var("AUTH_TEST_ABSENT");
        }
        assert_eq!(
            resolve_plan_token(
                &s,
                &plan(
                    "p",
                    AuthConfig::ApiKeyEnv {
                        env: "AUTH_TEST_ABSENT".into()
                    }
                ),
                err
            )
            .await
            .unwrap(),
            None
        );
    }
    #[tokio::test]
    #[serial]
    async fn api_key_env_empty_is_none() {
        unsafe {
            std::env::set_var("AUTH_TEST_EMPTY", "");
        }
        let s = state(
            cfg_with_provider(
                "p",
                AuthConfig::ApiKeyEnv {
                    env: "AUTH_TEST_EMPTY".into(),
                },
            ),
            AccountsFile::new(),
        );
        assert_eq!(
            resolve_plan_token(
                &s,
                &plan(
                    "p",
                    AuthConfig::ApiKeyEnv {
                        env: "AUTH_TEST_EMPTY".into()
                    }
                ),
                err
            )
            .await
            .unwrap(),
            None
        );
    }
    #[tokio::test]
    #[serial]
    async fn api_key_env_present_returns_token() {
        unsafe {
            std::env::set_var("AUTH_TEST_PRESENT", "secret");
        }
        let s = state(
            cfg_with_provider(
                "p",
                AuthConfig::ApiKeyEnv {
                    env: "AUTH_TEST_PRESENT".into(),
                },
            ),
            AccountsFile::new(),
        );
        assert_eq!(
            resolve_plan_token(
                &s,
                &plan(
                    "p",
                    AuthConfig::ApiKeyEnv {
                        env: "AUTH_TEST_PRESENT".into()
                    }
                ),
                err
            )
            .await
            .unwrap(),
            Some("secret".into())
        );
    }

    #[test]
    fn oauth_token_from_guard_openai_valid() {
        let mut a = AccountsFile::new();
        a.openai.insert("acct".into(), openai("tok", future()));
        assert_eq!(
            oauth_token_from_guard(&a, "acct", "prov", "openai_oauth"),
            Some(("tok".into(), None))
        );
    }
    #[test]
    fn oauth_token_from_guard_openai_missing() {
        assert_eq!(
            oauth_token_from_guard(&AccountsFile::new(), "acct", "prov", "openai_oauth"),
            None
        );
    }
    #[test]
    fn oauth_token_from_guard_openai_expired() {
        let mut a = AccountsFile::new();
        a.openai.insert("acct".into(), openai("tok", past()));
        assert_eq!(
            oauth_token_from_guard(&a, "acct", "prov", "openai_oauth"),
            None
        );
    }
    #[tokio::test]
    async fn resolve_plan_auth_openai_uses_explicit_account() {
        let mut a = AccountsFile::new();
        a.openai.insert("acct".into(), openai("tok", future()));
        let s = state(cfg_with_provider("p", AuthConfig::None), a);
        assert_eq!(
            resolve_plan_auth(
                &s,
                &plan(
                    "p",
                    AuthConfig::OpenaiOauth {
                        account: Some("acct".into())
                    }
                ),
                err
            )
            .await
            .unwrap()
            .token,
            Some("tok".into())
        );
    }
    #[tokio::test]
    async fn resolve_plan_auth_openai_defaults_account_to_provider() {
        let mut a = AccountsFile::new();
        a.openai.insert("p".into(), openai("tok", future()));
        let s = state(cfg_with_provider("p", AuthConfig::None), a);
        assert_eq!(
            resolve_plan_token(
                &s,
                &plan("p", AuthConfig::OpenaiOauth { account: None }),
                err
            )
            .await
            .unwrap(),
            Some("tok".into())
        );
    }
    #[tokio::test]
    async fn resolve_plan_auth_oauth_missing_account_is_unauthorized() {
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            AccountsFile::new(),
        );
        assert!(
            resolve_plan_auth(
                &s,
                &plan(
                    "p",
                    AuthConfig::OpenaiOauth {
                        account: Some("nope".into())
                    }
                ),
                err
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    #[serial]
    async fn apply_bearer_auth_adds_authorization_header() {
        unsafe {
            std::env::set_var("AUTH_TEST_BEARER", "bear");
        }
        let s = state(
            cfg_with_provider(
                "p",
                AuthConfig::ApiKeyEnv {
                    env: "AUTH_TEST_BEARER".into(),
                },
            ),
            AccountsFile::new(),
        );
        let req = apply_bearer_auth(
            &s,
            &plan(
                "p",
                AuthConfig::ApiKeyEnv {
                    env: "AUTH_TEST_BEARER".into(),
                },
            ),
            s.client.get("http://example.invalid"),
            err,
        )
        .await
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(
            req.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer bear"
        );
    }
    #[tokio::test]
    #[serial]
    async fn apply_anthropic_auth_api_key_uses_x_api_key() {
        unsafe {
            std::env::set_var("AUTH_TEST_ANTH", "akey");
        }
        let s = state(
            cfg_with_provider(
                "p",
                AuthConfig::ApiKeyEnv {
                    env: "AUTH_TEST_ANTH".into(),
                },
            ),
            AccountsFile::new(),
        );
        let req = apply_anthropic_auth(
            &s,
            &plan(
                "p",
                AuthConfig::ApiKeyEnv {
                    env: "AUTH_TEST_ANTH".into(),
                },
            ),
            s.client.get("http://example.invalid"),
            err,
        )
        .await
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(req.headers().get("x-api-key").unwrap(), "akey");
        assert!(req.headers().get(header::AUTHORIZATION).is_none());
    }
    #[tokio::test]
    async fn apply_anthropic_auth_oauth_uses_bearer() {
        let mut a = AccountsFile::new();
        a.openai.insert("p".into(), openai("otok", future()));
        let s = state(cfg_with_provider("p", AuthConfig::None), a);
        let req = apply_anthropic_auth(
            &s,
            &plan("p", AuthConfig::OpenaiOauth { account: None }),
            s.client.get("http://example.invalid"),
            err,
        )
        .await
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(
            req.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer otok"
        );
    }
    #[tokio::test]
    async fn apply_anthropic_auth_none_leaves_headers_empty() {
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            AccountsFile::new(),
        );
        let req = apply_anthropic_auth(
            &s,
            &plan("p", AuthConfig::None),
            s.client.get("http://example.invalid"),
            err,
        )
        .await
        .unwrap()
        .build()
        .unwrap();
        assert!(req.headers().get(header::AUTHORIZATION).is_none());
        assert!(req.headers().get("x-api-key").is_none());
    }
    #[tokio::test]
    #[serial]
    async fn apply_bearer_auth_none_does_not_add_authorization_header() {
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            AccountsFile::new(),
        );
        let req = apply_bearer_auth(
            &s,
            &plan("p", AuthConfig::None),
            s.client.get("http://example.invalid"),
            err,
        )
        .await
        .unwrap()
        .build()
        .unwrap();
        assert!(req.headers().get(header::AUTHORIZATION).is_none());
    }

    #[tokio::test]
    #[serial]
    async fn apply_bearer_auth_missing_provider_returns_response_error() {
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            AccountsFile::new(),
        );
        let got = apply_bearer_auth(
            &s,
            &plan("missing", AuthConfig::None),
            s.client.get("http://example.invalid"),
            err,
        )
        .await;
        assert!(got.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn apply_anthropic_auth_missing_api_key_does_not_add_headers() {
        unsafe {
            std::env::remove_var("AUTH_TEST_ANTH_ABSENT");
        }
        let s = state(
            cfg_with_provider(
                "p",
                AuthConfig::ApiKeyEnv {
                    env: "AUTH_TEST_ANTH_ABSENT".into(),
                },
            ),
            AccountsFile::new(),
        );
        let req = apply_anthropic_auth(
            &s,
            &plan(
                "p",
                AuthConfig::ApiKeyEnv {
                    env: "AUTH_TEST_ANTH_ABSENT".into(),
                },
            ),
            s.client.get("http://example.invalid"),
            err,
        )
        .await
        .unwrap()
        .build()
        .unwrap();
        assert!(req.headers().get(header::AUTHORIZATION).is_none());
        assert!(req.headers().get("x-api-key").is_none());
    }
    // =========================================================================
    // proxy/auth.rs OAuth path tests
    // =========================================================================

    #[tokio::test]
    async fn test_resolve_plan_auth_openai_oauth_success() {
        // Verify resolve_plan_auth returns correct ResolvedAuth for openai_oauth
        let mut accounts = AccountsFile::new();
        accounts
            .openai
            .insert("myacct".into(), openai("access-tok-123", future()));
        let s = state(cfg_with_provider("p", AuthConfig::None), accounts);
        let resolved = resolve_plan_auth(
            &s,
            &plan(
                "p",
                AuthConfig::OpenaiOauth {
                    account: Some("myacct".into()),
                },
            ),
            err,
        )
        .await
        .unwrap();
        assert_eq!(resolved.token, Some("access-tok-123".into()));
        // OpenAI OAuth should not have project_id
        assert_eq!(resolved.project_id, None);
    }

    #[tokio::test]
    async fn test_resolve_plan_auth_openai_oauth_missing_account() {
        // Verify 401 error when OAuth account doesn't exist
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            AccountsFile::new(),
        );
        let result = resolve_plan_auth(
            &s,
            &plan(
                "p",
                AuthConfig::OpenaiOauth {
                    account: Some("nonexistent".into()),
                },
            ),
            err,
        )
        .await;
        assert!(result.is_err());
        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    #[tokio::test]
    async fn test_apply_anthropic_auth_openai_oauth_bearer() {
        // Verify apply_anthropic_auth uses Bearer for openai_oauth with explicit account
        let mut accounts = AccountsFile::new();
        accounts
            .openai
            .insert("explicit".into(), openai("oa-bearer-tok", future()));
        let s = state(cfg_with_provider("p", AuthConfig::None), accounts);
        let req = apply_anthropic_auth(
            &s,
            &plan(
                "p",
                AuthConfig::OpenaiOauth {
                    account: Some("explicit".into()),
                },
            ),
            s.client.get("http://example.invalid"),
            err,
        )
        .await
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(
            req.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer oa-bearer-tok"
        );
        // Must NOT have x-api-key header for OAuth
        assert!(req.headers().get("x-api-key").is_none());
    }
    #[tokio::test]
    async fn test_resolve_plan_auth_default_branch_missing_provider() {
        // Default branch (ApiKeyEnv) with missing provider returns 401
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            AccountsFile::new(),
        );
        let result = resolve_plan_auth(&s, &plan("nonexistent", AuthConfig::None), err).await;
        assert!(result.is_err());
        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
