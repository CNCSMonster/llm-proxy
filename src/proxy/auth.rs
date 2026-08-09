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
            AuthConfig::OpenaiOauth { .. } | AuthConfig::AntigravityOauth { .. } => {
                Ok(req.bearer_auth(token))
            }
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
    // OAuth provider：从内存缓存取 token（§15.2 阶段 4，避免每次请求读文件）
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
        crate::config::AuthConfig::AntigravityOauth { account } => {
            let account_id = account.clone().unwrap_or_else(|| plan.provider_id.clone());
            let (token, project_id) =
                resolve_oauth_auth(state, &account_id, &plan.provider_id, "antigravity_oauth")
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
                project_id,
            })
        }
        _ => state.cfg.resolve_auth(&plan.provider_id).map_err(|e| {
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
    crate::auth::refresh_account_for_provider(
        &crate::auth::default_state_path(),
        &account_id,
        &auth_type,
    )
    .await?;
    // 重新加载 → 更新内存缓存
    let accounts =
        crate::auth::load_oauth_accounts_with_recovery(&crate::auth::default_state_path())?;
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
    accounts: &crate::auth::OAuthAccounts,
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
        _ => {
            let token =
                crate::auth::get_antigravity_token_from_accounts(accounts, account_id, provider_id)
                    .ok()?;
            let project_id = accounts
                .antigravity
                .get(account_id)
                .map(|a| a.project_id.clone());
            Some((token, project_id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AntigravityAccount, OAuthAccounts, OpenaiAccount};
    use crate::config::{
        AdapterKind, AuthConfig, CompatConfig, Config, FallbackConfig, Protocol, ProviderConfig,
        RequestFrequencyConfig, ServerConfig, StatusConfig,
    };
    use serial_test::serial;
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;
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
            server: ServerConfig {
                listen: "127.0.0.1:0".into(),
                usage: Default::default(),
                max_sse_buffer_bytes: 1,
                max_output_items: 1,
            },
            fallback: FallbackConfig::default(),
            protection: Default::default(),
            status: StatusConfig::default(),
            providers,
            models: BTreeMap::new(),
        }
    }

    fn state(cfg: Config, accounts: OAuthAccounts) -> AppState {
        let active = Arc::new(crate::probe_coordinator::ActiveProviderStore::default());
        AppState {
            cfg: Arc::new(cfg.clone()),
            client: reqwest::Client::new(),
            cooldowns: Arc::new(CooldownStore::in_memory()),
            oauth_accounts: Arc::new(RwLock::new(accounts)),
            oauth_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            bad_requests: Arc::new(BadRequestManager::new(Default::default())),
            stream_interruptions: Arc::new(Mutex::new(BTreeMap::new())),
            frequency: Arc::new(tokio::sync::Mutex::new(FrequencyState::default())),
            usage_store: None,
            active_providers: active.clone(),
            probe_coordinator: Arc::new(crate::probe_coordinator::ProbeCoordinator::new(
                crate::probe_coordinator::ServerProbeState::new(active.clone()),
            )),
            core: Arc::new(tokio::sync::Mutex::new(
                crate::core::CoreState::from_config(
                    (*Arc::new(cfg.clone())).clone(),
                    PathBuf::from("/tmp/llm-proxy-test.toml"),
                ),
            )),
            thought_sig_queue: Arc::new(Mutex::new(HashMap::new())),
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
    fn anti(token: &str, project: &str, exp: i64) -> AntigravityAccount {
        AntigravityAccount {
            account_label: "ag".into(),
            project_id: project.into(),
            access_token: token.into(),
            refresh_token: "refresh-antigravity-token-long".into(),
            expires_at_unix: exp,
            updated_at_unix: 1,
        }
    }

    #[tokio::test]
    #[serial]
    async fn none_auth_resolves_no_token() {
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            OAuthAccounts::new(),
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
            OAuthAccounts::new(),
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
            OAuthAccounts::new(),
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
            OAuthAccounts::new(),
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
            OAuthAccounts::new(),
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
        let mut a = OAuthAccounts::new();
        a.openai.insert("acct".into(), openai("tok", future()));
        assert_eq!(
            oauth_token_from_guard(&a, "acct", "prov", "openai_oauth"),
            Some(("tok".into(), None))
        );
    }
    #[test]
    fn oauth_token_from_guard_openai_missing() {
        assert_eq!(
            oauth_token_from_guard(&OAuthAccounts::new(), "acct", "prov", "openai_oauth"),
            None
        );
    }
    #[test]
    fn oauth_token_from_guard_openai_expired() {
        let mut a = OAuthAccounts::new();
        a.openai.insert("acct".into(), openai("tok", past()));
        assert_eq!(
            oauth_token_from_guard(&a, "acct", "prov", "openai_oauth"),
            None
        );
    }
    #[test]
    fn oauth_token_from_guard_antigravity_valid_with_project() {
        let mut a = OAuthAccounts::new();
        a.antigravity
            .insert("acct".into(), anti("tok", "proj", future()));
        assert_eq!(
            oauth_token_from_guard(&a, "acct", "prov", "antigravity_oauth"),
            Some(("tok".into(), Some("proj".into())))
        );
    }
    #[test]
    fn oauth_token_from_guard_antigravity_missing() {
        assert_eq!(
            oauth_token_from_guard(&OAuthAccounts::new(), "acct", "prov", "antigravity_oauth"),
            None
        );
    }
    #[test]
    fn oauth_token_from_guard_unknown_type_uses_antigravity_path() {
        let mut a = OAuthAccounts::new();
        a.antigravity
            .insert("acct".into(), anti("tok", "proj", future()));
        assert_eq!(
            oauth_token_from_guard(&a, "acct", "prov", "weird"),
            Some(("tok".into(), Some("proj".into())))
        );
    }

    #[tokio::test]
    async fn resolve_plan_auth_openai_uses_explicit_account() {
        let mut a = OAuthAccounts::new();
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
        let mut a = OAuthAccounts::new();
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
            OAuthAccounts::new(),
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
    async fn resolve_plan_auth_antigravity_returns_project_id() {
        let mut a = OAuthAccounts::new();
        a.antigravity
            .insert("acct".into(), anti("tok", "proj", future()));
        let s = state(cfg_with_provider("p", AuthConfig::None), a);
        let got = resolve_plan_auth(
            &s,
            &plan(
                "p",
                AuthConfig::AntigravityOauth {
                    account: Some("acct".into()),
                },
            ),
            err,
        )
        .await
        .unwrap();
        assert_eq!(got.token, Some("tok".into()));
        assert_eq!(got.project_id, Some("proj".into()));
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
            OAuthAccounts::new(),
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
            OAuthAccounts::new(),
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
        let mut a = OAuthAccounts::new();
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
            OAuthAccounts::new(),
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
            OAuthAccounts::new(),
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
            OAuthAccounts::new(),
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
            OAuthAccounts::new(),
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

    #[tokio::test]
    async fn apply_anthropic_auth_antigravity_oauth_uses_bearer_not_x_api_key() {
        let mut a = OAuthAccounts::new();
        a.antigravity
            .insert("p".into(), anti("agtok", "proj", future()));
        let s = state(cfg_with_provider("p", AuthConfig::None), a);
        let req = apply_anthropic_auth(
            &s,
            &plan("p", AuthConfig::AntigravityOauth { account: None }),
            s.client.get("http://example.invalid"),
            err,
        )
        .await
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(
            req.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer agtok"
        );
        assert!(req.headers().get("x-api-key").is_none());
    }

    #[test]
    fn oauth_token_from_guard_antigravity_expired() {
        let mut a = OAuthAccounts::new();
        a.antigravity
            .insert("acct".into(), anti("tok", "proj", past()));
        assert_eq!(
            oauth_token_from_guard(&a, "acct", "prov", "antigravity_oauth"),
            None
        );
    }

    #[test]
    fn oauth_token_from_guard_openai_type_does_not_use_antigravity_account() {
        let mut a = OAuthAccounts::new();
        a.antigravity
            .insert("acct".into(), anti("tok", "proj", future()));
        assert_eq!(
            oauth_token_from_guard(&a, "acct", "prov", "openai_oauth"),
            None
        );
    }

    #[tokio::test]
    async fn resolve_plan_auth_antigravity_defaults_account_to_provider() {
        let mut a = OAuthAccounts::new();
        a.antigravity
            .insert("p".into(), anti("tok", "proj", future()));
        let s = state(cfg_with_provider("p", AuthConfig::None), a);
        let got = resolve_plan_auth(
            &s,
            &plan("p", AuthConfig::AntigravityOauth { account: None }),
            err,
        )
        .await
        .unwrap();
        assert_eq!(got.token, Some("tok".into()));
        assert_eq!(got.project_id, Some("proj".into()));
    }

    #[tokio::test]
    async fn resolve_plan_auth_antigravity_missing_account_is_unauthorized() {
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            OAuthAccounts::new(),
        );
        assert!(
            resolve_plan_auth(
                &s,
                &plan(
                    "p",
                    AuthConfig::AntigravityOauth {
                        account: Some("nope".into())
                    }
                ),
                err
            )
            .await
            .is_err()
        );
    }

    // =========================================================================
    // Phase 1 P0: proxy/auth.rs OAuth path tests
    // =========================================================================

    #[tokio::test]
    async fn test_resolve_plan_auth_openai_oauth_success() {
        // Verify resolve_plan_auth returns correct ResolvedAuth for openai_oauth
        let mut accounts = OAuthAccounts::new();
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
            OAuthAccounts::new(),
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
    async fn test_resolve_plan_auth_antigravity_oauth_with_project() {
        // Verify antigravity OAuth returns both token and project_id
        let mut accounts = OAuthAccounts::new();
        accounts
            .antigravity
            .insert("agacct".into(), anti("ag-tok", "proj-123", future()));
        let s = state(cfg_with_provider("p", AuthConfig::None), accounts);
        let resolved = resolve_plan_auth(
            &s,
            &plan(
                "p",
                AuthConfig::AntigravityOauth {
                    account: Some("agacct".into()),
                },
            ),
            err,
        )
        .await
        .unwrap();
        assert_eq!(resolved.token, Some("ag-tok".into()));
        assert_eq!(resolved.project_id, Some("proj-123".into()));
    }

    #[tokio::test]
    async fn test_apply_anthropic_auth_openai_oauth_bearer() {
        // Verify apply_anthropic_auth uses Bearer for openai_oauth with explicit account
        let mut accounts = OAuthAccounts::new();
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
    async fn test_apply_anthropic_auth_antigravity_oauth_bearer() {
        // Verify apply_anthropic_auth uses Bearer for antigravity_oauth with explicit account
        let mut accounts = OAuthAccounts::new();
        accounts
            .antigravity
            .insert("explicit".into(), anti("ag-bearer-tok", "proj", future()));
        let s = state(cfg_with_provider("p", AuthConfig::None), accounts);
        let req = apply_anthropic_auth(
            &s,
            &plan(
                "p",
                AuthConfig::AntigravityOauth {
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
            "Bearer ag-bearer-tok"
        );
        assert!(req.headers().get("x-api-key").is_none());
    }

    #[tokio::test]
    async fn test_resolve_plan_auth_default_branch_missing_provider() {
        // Default branch (ApiKeyEnv) with missing provider returns 401
        let s = state(
            cfg_with_provider("p", AuthConfig::None),
            OAuthAccounts::new(),
        );
        let result = resolve_plan_auth(&s, &plan("nonexistent", AuthConfig::None), err).await;
        assert!(result.is_err());
        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_resolve_plan_auth_antigravity_defaults_to_provider_id() {
        // When account is None, should default to provider_id for antigravity
        let mut accounts = OAuthAccounts::new();
        accounts
            .antigravity
            .insert("myprovider".into(), anti("tok-ag", "proj-ag", future()));
        let s = state(cfg_with_provider("myprovider", AuthConfig::None), accounts);
        let resolved = resolve_plan_auth(
            &s,
            &plan("myprovider", AuthConfig::AntigravityOauth { account: None }),
            err,
        )
        .await
        .unwrap();
        assert_eq!(resolved.token, Some("tok-ag".into()));
        assert_eq!(resolved.project_id, Some("proj-ag".into()));
    }
}
