use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use sha2::Digest;

use super::refresh::{RefreshedToken, post_refresh_form, refreshed_token_from_json};
use super::storage::*;
use super::types::*;

/// 写入 OAuth 账号（阶段 5）：server 运行 → 委托 UDS（server 写文件 + 更新内存缓存）；
/// 否则本地持所有权锁 + 文件锁写入。
pub(crate) async fn write_oauth_account(
    config_path: &Path,
    path: &Path,
    account: &str,
    provider_id: &str,
    oauth_type: &str,
    entry: serde_json::Value,
) -> Result<()> {
    if let Some(server) = crate::admin_client::detect_server(config_path).await? {
        server
            .oauth_write(provider_id, oauth_type, account, &entry)
            .await?;
        return Ok(());
    }
    // 本地：所有权锁（§15.2 单一写者）+ 文件锁读改写
    crate::ownership::with_cli_write_lock(
        &format!("llm-proxy provider login {provider_id}"),
        || {
            with_locked_accounts(path, |accounts| {
                match oauth_type {
                    "openai" => {
                        let e: OpenaiAccount = serde_json::from_value(entry.clone())?;
                        accounts.openai.insert(account.to_string(), e);
                    }
                    "antigravity" => {
                        let e: AntigravityAccount = serde_json::from_value(entry.clone())?;
                        accounts.antigravity.insert(account.to_string(), e);
                    }
                    _ => bail!("unknown oauth type: {oauth_type}"),
                }
                save_oauth_accounts_locked(path, accounts)
            })
        },
    )
}

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
    match provider.auth_config(provider_id)? {
        crate::config::AuthConfig::OpenaiOauth { account } => {
            let account = account.unwrap_or_else(|| provider_id.to_string());
            login_openai_device_with_urls(
                config_path,
                path,
                &account,
                provider_id,
                OPENAI_DEVICE_AUTH_URL,
                OPENAI_DEVICE_TOKEN_URL,
                OPENAI_OAUTH_TOKEN_URL,
            )
            .await
        }
        crate::config::AuthConfig::AntigravityOauth { account } => {
            let account = account.unwrap_or_else(|| provider_id.to_string());
            login_antigravity_interactive_with_urls(
                config_path,
                path,
                &account,
                provider_id,
                ANTIGRAVITY_AUTH_URL,
                ANTIGRAVITY_TOKEN_URL,
                ANTIGRAVITY_USERINFO_URL,
                ANTIGRAVITY_LOAD_CODE_ASSIST_URL,
                ANTIGRAVITY_ONBOARD_USER_URL,
            )
            .await
        }
        crate::config::AuthConfig::ApiKeyEnv { .. } | crate::config::AuthConfig::None => {
            bail!("provider {provider_id:?} is not OAuth-backed")
        }
    }
}

async fn login_openai_device_with_urls(
    config_path: &Path,
    path: &Path,
    account: &str,
    provider_id: &str,
    device_auth_url: &str,
    device_token_url: &str,
    oauth_token_url: &str,
) -> Result<()> {
    let device = request_openai_device_code(device_auth_url).await?;
    println!("OpenAI login device code: {}", device.user_code);
    println!("Open: https://auth.openai.com/codex/device");
    let interval = device.interval.max(2) + 3;
    let deadline = unix_secs() + device.expires_in.max(1);
    loop {
        if unix_secs() >= deadline {
            bail!("OpenAI device login expired; run provider login again");
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if let Some(poll) =
            poll_openai_device_token(device_token_url, &device.device_auth_id, &device.user_code)
                .await?
        {
            let token = exchange_openai_device_token(
                oauth_token_url,
                &poll.authorization_code,
                &poll.code_verifier,
            )
            .await?;
            let now = unix_secs() as i64;
            let entry = OpenaiAccount {
                account_label: account.to_string(),
                access_token: token.access_token,
                refresh_token: token
                    .refresh_token
                    .filter(|value| !value.is_empty())
                    .context("OpenAI OAuth server did not return a refresh token")?,
                expires_at_unix: token
                    .expires_in
                    .map(|seconds| now + seconds as i64)
                    .unwrap_or(now + 3600),
                updated_at_unix: now,
            };
            // 阶段 5：server 运行 → 委托写（UDS + 更新 server 内存缓存）；
            // 否则本地持所有权锁 + 文件锁读改写
            write_oauth_account(
                config_path,
                path,
                account,
                provider_id,
                "openai",
                serde_json::to_value(&entry)?,
            )
            .await?;
            println!("logged in OAuth account={account} provider={provider_id}");
            return Ok(());
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn login_antigravity_interactive_with_urls(
    config_path: &Path,
    path: &Path,
    account: &str,
    provider_id: &str,
    auth_url: &str,
    token_url: &str,
    userinfo_url: &str,
    load_code_assist_url: &str,
    onboard_user_url: &str,
) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("Antigravity login requires an interactive terminal to paste the authorization code");
    }
    let verifier = generate_pkce_code_verifier()?;
    let challenge = pkce_challenge(&verifier);
    let state = generate_state()?;
    // 当前 Antigravity CLI 登录流程要求用户手动粘贴 authorization code，
    // 不启动本地 redirect callback server，因此无法从 redirect URI 中读取并校验 state。
    // state 仍发送给授权端点，作为 OAuth URL 的随机 nonce，并为未来 loopback callback
    // 模式预留。若改为自动接收 redirect，必须验证返回 state 与此处 state 完全一致。
    let url = build_antigravity_auth_url(auth_url, &state, &challenge)?;
    println!("Open this URL in your browser and authorize Antigravity:");
    println!("{url}");
    print!("Paste authorization code: ");
    io::stdout().flush()?;
    let mut code = String::new();
    io::stdin().read_line(&mut code)?;
    finish_antigravity_login(
        config_path,
        path,
        account,
        provider_id,
        token_url,
        userinfo_url,
        load_code_assist_url,
        onboard_user_url,
        &verifier,
        code.trim(),
    )
    .await?;
    println!("logged in OAuth account={account} provider={provider_id}");
    Ok(())
}

/// 生成 Antigravity 授权 URL 及其对应 PKCE code_verifier。
/// TUI 进入 AntigravityLogin 屏幕时调用：展示 URL 给用户，保存 verifier，
/// 待用户粘贴 code 后用它完成 exchange（自包含登录，不依赖 CLI stdin/stdout）。
pub(crate) fn generate_antigravity_auth_url() -> Result<(String, String)> {
    let verifier = generate_pkce_code_verifier()?;
    let challenge = pkce_challenge(&verifier);
    let state = generate_state()?;
    let url = build_antigravity_auth_url(ANTIGRAVITY_AUTH_URL, &state, &challenge)?;
    Ok((url, verifier))
}

/// 非交互式 Antigravity 登录：TUI 已捕获授权 code，用预生成的 verifier 完成
/// OAuth exchange + 账号写入，不读取 stdin/stdout（与 login_provider 的 CLI
/// 交互流程区分，供 TUI 主循环直接调用）。
pub(crate) async fn login_antigravity_with_code(
    config_path: &Path,
    cfg: &crate::config::Config,
    path: &Path,
    provider_id: &str,
    verifier: &str,
    code: &str,
) -> Result<()> {
    let provider = cfg
        .providers
        .get(provider_id)
        .with_context(|| format!("unknown provider {provider_id:?}"))?;
    let account = match provider.auth_config(provider_id)? {
        crate::config::AuthConfig::AntigravityOauth { account } => {
            account.unwrap_or_else(|| provider_id.to_string())
        }
        _ => bail!("provider {provider_id:?} is not Antigravity OAuth-backed"),
    };
    finish_antigravity_login(
        config_path,
        path,
        &account,
        provider_id,
        ANTIGRAVITY_TOKEN_URL,
        ANTIGRAVITY_USERINFO_URL,
        ANTIGRAVITY_LOAD_CODE_ASSIST_URL,
        ANTIGRAVITY_ONBOARD_USER_URL,
        verifier,
        code.trim(),
    )
    .await?;
    tracing::info!("logged in OAuth account={account} provider={provider_id}");
    Ok(())
}

/// Antigravity exchange + 账号写入公共逻辑：被 CLI 交互流程与 TUI 非交互流程复用。
/// 阶段 5：server 运行 → 委托写（UDS + 更新 server 内存缓存）；
/// 否则本地持所有权锁 + 文件锁读改写。
#[allow(clippy::too_many_arguments)]
async fn finish_antigravity_login(
    config_path: &Path,
    path: &Path,
    account: &str,
    provider_id: &str,
    token_url: &str,
    userinfo_url: &str,
    load_code_assist_url: &str,
    onboard_user_url: &str,
    verifier: &str,
    code: &str,
) -> Result<()> {
    let token = exchange_antigravity_code(token_url, code, verifier).await?;
    let email = match fetch_antigravity_userinfo(userinfo_url, &token.access_token).await {
        Ok(email) => Some(email),
        Err(err) => {
            tracing::warn!(
                "Antigravity userinfo lookup failed; continuing without account email: {err:#}"
            );
            None
        }
    };
    let project_id =
        fetch_antigravity_project_id(load_code_assist_url, onboard_user_url, &token.access_token)
            .await
            .context("failed to determine Antigravity Google Cloud project ID; relogin required")?;
    let now = unix_secs() as i64;
    let entry = AntigravityAccount {
        account_label: email.unwrap_or_else(|| account.to_string()),
        project_id,
        access_token: token.access_token,
        refresh_token: token
            .refresh_token
            .filter(|value| !value.is_empty())
            .context("Antigravity OAuth server did not return a refresh token")?,
        expires_at_unix: token
            .expires_in
            .map(|seconds| now + seconds as i64)
            .unwrap_or(now + 3600),
        updated_at_unix: now,
    };
    write_oauth_account(
        config_path,
        path,
        account,
        provider_id,
        "antigravity",
        serde_json::to_value(&entry)?,
    )
    .await?;
    Ok(())
}

pub(super) fn build_antigravity_auth_url(
    auth_url: &str,
    state: &str,
    code_challenge: &str,
) -> Result<String> {
    let mut url = url::Url::parse(auth_url).context("invalid Antigravity auth URL")?;
    url.query_pairs_mut()
        .append_pair("client_id", ANTIGRAVITY_CLIENT_ID)
        .append_pair("redirect_uri", ANTIGRAVITY_REDIRECT_URI)
        .append_pair("response_type", "code")
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("scope", &antigravity_scopes().join(" "));
    Ok(url.to_string())
}

pub(super) fn antigravity_scopes() -> Vec<&'static str> {
    vec![
        "https://www.googleapis.com/auth/cloud-platform",
        "https://www.googleapis.com/auth/userinfo.email",
        "https://www.googleapis.com/auth/userinfo.profile",
        "https://www.googleapis.com/auth/cclog",
        "https://www.googleapis.com/auth/experimentsandconfigs",
    ]
}

pub(super) fn generate_pkce_code_verifier() -> Result<String> {
    let bytes = random_bytes(64)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn generate_state() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes(32)?))
}

pub(super) fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("failed to generate random bytes: {e}"))?;
    Ok(bytes)
}

pub(super) fn pkce_challenge(verifier: &str) -> String {
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

pub(super) async fn exchange_antigravity_code(
    token_url: &str,
    code: &str,
    code_verifier: &str,
) -> Result<RefreshedToken> {
    if code.trim().is_empty() {
        bail!("authorization code must not be empty");
    }
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", ANTIGRAVITY_REDIRECT_URI),
        ("client_id", ANTIGRAVITY_CLIENT_ID),
        ("client_secret", ANTIGRAVITY_CLIENT_SECRET),
        ("code_verifier", code_verifier),
    ];
    let payload = post_refresh_form(token_url, &params).await?;
    let token = refreshed_token_from_json(payload)?;
    if token
        .refresh_token
        .as_deref()
        .unwrap_or_default()
        .is_empty()
    {
        bail!("Antigravity token response has no refresh_token");
    }
    Ok(token)
}

const ANTIGRAVITY_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

fn antigravity_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(ANTIGRAVITY_HTTP_TIMEOUT)
        .build()
        .context("failed to build Antigravity HTTP client")
}

async fn fetch_antigravity_userinfo(userinfo_url: &str, access_token: &str) -> Result<String> {
    let payload: Value = antigravity_http_client()?
        .get(userinfo_url)
        .header("Accept", "application/json")
        .bearer_auth(access_token)
        .send()
        .await
        .with_context(|| format!("Antigravity userinfo request failed for {userinfo_url}"))?
        .error_for_status()?
        .json()
        .await
        .context("Antigravity userinfo response is not valid JSON")?;
    payload
        .get("email")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .map(ToString::to_string)
        .context("Antigravity userinfo response has no email")
}

pub(super) async fn fetch_antigravity_project_id(
    load_code_assist_url: &str,
    onboard_user_url: &str,
    access_token: &str,
) -> Result<String> {
    match fetch_project_id_from_load_code_assist(load_code_assist_url, access_token).await {
        Ok(Some(project_id)) => Ok(project_id),
        Ok(None) => fetch_project_id_from_onboard_user(onboard_user_url, access_token)
            .await
            .with_context(|| {
                format!(
                    "Antigravity loadCodeAssist response at {load_code_assist_url} had no project id; onboardUser fallback failed"
                )
            }),
        Err(err) => Err(err).with_context(|| {
            format!(
                "Antigravity loadCodeAssist request failed for {load_code_assist_url}; not falling back to onboardUser"
            )
        }),
    }
}

async fn fetch_project_id_from_load_code_assist(
    url: &str,
    access_token: &str,
) -> Result<Option<String>> {
    let payload = post_antigravity_json(
        url,
        access_token,
        "antigravity/cli/1.0.13",
        serde_json::json!({"metadata": {"ideType": "ANTIGRAVITY", "pluginType": "GEMINI"}}),
    )
    .await?;
    Ok(payload
        .get("cloudaicompanionProject")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string))
}

async fn fetch_project_id_from_onboard_user(url: &str, access_token: &str) -> Result<String> {
    let payload = post_antigravity_json(
        url,
        access_token,
        "google-api-nodejs-client/10.3.0",
        serde_json::json!({
            "tier_id": "free-tier",
            "metadata": {"ideType": "ANTIGRAVITY", "pluginType": "GEMINI"}
        }),
    )
    .await?;
    payload
        .get("response")
        .and_then(|v| v.get("cloudaicompanionProject"))
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
        .context("Antigravity onboardUser response has no project id")
}

async fn post_antigravity_json(
    url: &str,
    access_token: &str,
    user_agent: &str,
    body: Value,
) -> Result<Value> {
    antigravity_http_client()?
        .post(url)
        .header("Accept", "application/json")
        .bearer_auth(access_token)
        .header("User-Agent", user_agent)
        .header("X-Goog-Api-Client", "gl-node/22.21.1")
        .json(&body)
        .send()
        .await
        .with_context(|| format!("Antigravity backend request failed for {url}"))?
        .error_for_status()?
        .json()
        .await
        .context("Antigravity backend response is not valid JSON")
}

#[derive(Debug)]
pub(super) struct DeviceCode {
    pub(super) device_auth_id: String,
    pub(super) user_code: String,
    pub(super) expires_in: u64,
    pub(super) interval: u64,
}

#[derive(Debug)]
pub(super) struct DevicePoll {
    pub(super) authorization_code: String,
    pub(super) code_verifier: String,
}

pub(super) async fn request_openai_device_code(url: &str) -> Result<DeviceCode> {
    let payload: Value = reqwest::Client::new()
        .post(url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({ "client_id": OPENAI_CLIENT_ID }))
        .send()
        .await
        .with_context(|| format!("OpenAI device code request failed for {url}"))?
        .error_for_status()?
        .json()
        .await
        .context("OpenAI device code response is not valid JSON")?;
    let device_auth_id = payload
        .get("device_auth_id")
        .or_else(|| payload.get("deviceAuthID"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .context("OpenAI device code response has no device_auth_id")?
        .to_string();
    let user_code = payload
        .get("user_code")
        .or_else(|| payload.get("usercode"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .context("OpenAI device code response has no user_code")?
        .to_string();
    Ok(DeviceCode {
        device_auth_id,
        user_code,
        expires_in: super::refresh::value_u64(&payload, "expires_in").unwrap_or(900),
        interval: super::refresh::value_u64(&payload, "interval").unwrap_or(5),
    })
}

pub(super) async fn poll_openai_device_token(
    url: &str,
    device_auth_id: &str,
    user_code: &str,
) -> Result<Option<DevicePoll>> {
    let response = reqwest::Client::new()
        .post(url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        }))
        .send()
        .await
        .with_context(|| format!("OpenAI device token poll failed for {url}"))?;
    if response.status() == reqwest::StatusCode::FORBIDDEN
        || response.status() == reqwest::StatusCode::NOT_FOUND
    {
        return Ok(None);
    }
    let payload: Value = response.error_for_status()?.json().await?;
    Ok(Some(DevicePoll {
        authorization_code: payload
            .get("authorization_code")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .context("OpenAI device token response has no authorization_code")?
            .to_string(),
        code_verifier: payload
            .get("code_verifier")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .context("OpenAI device token response has no code_verifier")?
            .to_string(),
    }))
}

async fn exchange_openai_device_token(
    url: &str,
    authorization_code: &str,
    code_verifier: &str,
) -> Result<RefreshedToken> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", authorization_code),
        ("code_verifier", code_verifier),
        (
            "redirect_uri",
            "https://auth.openai.com/deviceauth/callback",
        ),
        ("client_id", OPENAI_CLIENT_ID),
    ];
    let payload = post_refresh_form(url, &params).await?;
    refreshed_token_from_json(payload)
}
