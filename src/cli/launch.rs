use crate::{admin_client, config};
use anyhow::{Context, Result, bail};
use regex::Regex;
use std::env::VarError;
use std::sync::LazyLock;

static SANITIZE_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (
            Regex::new(r"Bearer\s+[A-Za-z0-9_\-+/=.]+").expect("valid Bearer regex"),
            "Bearer [REDACTED]",
        ),
        (
            Regex::new(r"token=[A-Za-z0-9_\-+/=.]+").expect("valid token regex"),
            "token=[REDACTED]",
        ),
        (
            Regex::new(r"Authorization:\s*[^\r\n]+").expect("valid Authorization regex"),
            "Authorization: [REDACTED]",
        ),
        (
            Regex::new(r"([:=]\s*)[A-Za-z0-9+/=_\-]{32,}")
                .expect("valid high-entropy secret regex"),
            "${1}[REDACTED_SECRET]",
        ),
    ]
});

/// 通用响应文本脱敏工具函数
pub fn sanitize_error_snippet(body: &str) -> String {
    let pre_truncated: String = body.chars().take(500).collect();
    let mut clean = pre_truncated;

    for (re, repl) in SANITIZE_PATTERNS.iter() {
        clean = re.replace_all(&clean, *repl).to_string();
    }

    if clean.chars().count() > 150 {
        format!("{}...", clean.chars().take(150).collect::<String>())
    } else {
        clean
    }
}

/// 判断 host 是否为严格字面回环地址
pub fn is_loopback_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// 校验环境变量名是否符合合法标识符规范
pub fn is_valid_env_var_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 构造 launch 用 Config：
/// - 本地模式（config 有 providers/models）→ 直接读盘（现状）
/// - 远程模式（`[remote]` 或 providers/models 全空）→ HTTP(S) 直连远端
///   `/admin/client-config/{client}`，彻底旁路本地 UDS 探测与所有权锁。
pub async fn launch_config(
    config_path: &std::path::Path,
    client: &str,
    cli_filter: &crate::cli::types::LaunchFilterArgs,
) -> Result<config::Config> {
    let local = config::Config::load(config_path)?;
    let is_remote =
        local.remote.is_some() || (local.providers.is_empty() && local.models.is_empty());
    let mut cfg = if !is_remote {
        local.clone()
    } else if let Some(ref remote) = local.remote {
        launch_config_remote(remote, client).await?
    } else {
        match admin_client::detect_server(config_path).await {
            Ok(Some(server)) => {
                if let Err(e) = crate::ownership::ensure_same_config(&server, config_path) {
                    eprintln!("警告: 环境中存在另一份 config 的运行实例，视为当前配置未运行 ({e})");
                    anyhow::bail!(
                        "server unreachable and no local config for launch (remote mode); \
                         start the server or use a config with models"
                    );
                }
                let data = server.client_config(client).await?;
                config_from_client_data(&data, &server.base_url)?
            }
            Ok(None) => {
                anyhow::bail!(
                    "server unreachable and no local config for launch (remote mode); \
                     start the server or use a config with models"
                )
            }
            Err(e) => return Err(e),
        }
    };

    apply_launch_model_filter(&mut cfg, &local.launch, cli_filter)?;

    Ok(cfg)
}

/// 应用客户端本地与命令行级联模型过滤。
///
/// 管道模型：
///   M0（服务端/本地基准模型集）
///     -> M1（应用 [launch] include_models / exclude_models 配置）
///     -> M2（应用 CLI --models / --exclude-models 参数）
///
/// 若过滤后 M2 为空，立即 fail-fast 报错（防止写入空配置破坏客户端已有配置）。
/// 若过滤后 M2 非空，级联裁剪未被任何保留模型引用的 Providers。
pub fn apply_launch_model_filter(
    cfg: &mut config::Config,
    config_filter: &config::LaunchConfig,
    cli_filter: &crate::cli::types::LaunchFilterArgs,
) -> Result<()> {
    let base_models: Vec<String> = cfg.models.keys().cloned().collect();
    if base_models.is_empty() {
        return Ok(());
    }

    let config_include: Vec<String> = config_filter
        .include_models
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let config_exclude: Vec<String> = config_filter
        .exclude_models
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut m1_models = base_models.clone();
    if !config_include.is_empty() {
        m1_models.retain(|m| config_include.contains(m));
    }
    if !config_exclude.is_empty() {
        m1_models.retain(|m| !config_exclude.contains(m));
    }

    let cli_include: Vec<String> = cli_filter
        .models
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let cli_exclude: Vec<String> = cli_filter
        .exclude_models
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut m2_models = m1_models.clone();
    if !cli_include.is_empty() {
        m2_models.retain(|m| cli_include.contains(m));
    }
    if !cli_exclude.is_empty() {
        m2_models.retain(|m| !cli_exclude.contains(m));
    }

    if m2_models.is_empty() {
        let mut msg = format!(
            "no models remain after applying launch model filters:\n  server/base models: [{}]",
            base_models.join(", ")
        );
        if !config_include.is_empty() || !config_exclude.is_empty() {
            msg.push_str(&format!(
                "\n  retained by [launch] config: [{}]",
                m1_models.join(", ")
            ));
        }
        if !cli_include.is_empty() || !cli_exclude.is_empty() {
            let mut parts = Vec::new();
            if !cli_include.is_empty() {
                parts.push(format!("include: [{}]", cli_include.join(", ")));
            }
            if !cli_exclude.is_empty() {
                parts.push(format!("exclude: [{}]", cli_exclude.join(", ")));
            }
            msg.push_str(&format!("\n  CLI filter: {}", parts.join("; ")));
        }
        msg.push_str("\n  final intersection: []");
        bail!("{msg}");
    }

    let m2_set: std::collections::BTreeSet<String> = m2_models.into_iter().collect();
    cfg.models.retain(|k, _| m2_set.contains(k));

    let mut needed_providers = std::collections::BTreeSet::new();
    for model in cfg.models.values() {
        for binding in &model.openai_chat_providers {
            needed_providers.insert(binding.name.clone());
        }
        for binding in &model.openai_responses_providers {
            needed_providers.insert(binding.name.clone());
        }
        for binding in &model.anthropic_providers {
            needed_providers.insert(binding.name.clone());
        }
    }
    for target in cfg.codex.auto_review_model.iter().chain(
        cfg.codex
            .model_overrides
            .values()
            .map(|override_config| &override_config.auto_review_model),
    ) {
        if let Some(target_model) = cfg.models.get(target) {
            for binding in &target_model.openai_responses_providers {
                needed_providers.insert(binding.name.clone());
            }
        }
    }
    if cfg.providers.contains_key("llm-proxy") {
        needed_providers.insert("llm-proxy".to_string());
    }
    cfg.providers.retain(|k, _| needed_providers.contains(k));

    cfg.codex.model_overrides.retain(|k, _| m2_set.contains(k));
    if let Some(ref auto) = cfg.codex.auto_review_model
        && !m2_set.contains(auto)
    {
        cfg.codex.auto_review_model = None;
    }

    Ok(())
}

/// Remote mode: HTTP(S) GET `{remote.url}/admin/client-config/{client}` directly.
/// No UDS detection, no ownership check — the remote server is trusted over HTTP(S).
async fn launch_config_remote(
    remote: &config::RemoteConfig,
    client: &str,
) -> Result<config::Config> {
    let base_url = remote.url.trim_end_matches('/');
    let parsed_url =
        url::Url::parse(base_url).with_context(|| format!("invalid remote URL: {base_url}"))?;

    // 1. 传输通道安全：非回环地址无论何种模式一律强制使用 HTTPS
    if parsed_url.scheme() != "https" && !is_loopback_host(&parsed_url) {
        bail!(
            "Insecure remote URL '{}': Remote mode requires HTTPS for non-loopback endpoints to prevent exposure",
            remote.url
        );
    }

    let auth_mode = remote.auth_mode.unwrap_or(config::RemoteAuthMode::Token);

    // 2. 彻底禁用重定向，杜绝凭证泄漏风险
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build HTTP client for remote launch")?;

    let url = format!("{base_url}/admin/client-config/{client}");
    let mut req = http.get(&url);

    // 3. Token 严格校验与 Header 注入 (Fail-Closed)
    if auth_mode == config::RemoteAuthMode::Token {
        let env_name = remote.auth_token_env.trim();
        if !is_valid_env_var_name(env_name) {
            bail!(
                "Invalid remote auth_token_env '{}': must be a valid environment variable identifier",
                remote.auth_token_env
            );
        }
        let raw_token = match std::env::var(env_name) {
            Ok(v) => v,
            Err(VarError::NotPresent) => bail!(
                "Remote token mode requires environment variable '{}' to be set",
                env_name
            ),
            Err(VarError::NotUnicode(_)) => bail!(
                "Environment variable '{}' contains invalid unicode",
                env_name
            ),
        };
        let token = raw_token.trim_matches(|c: char| c.is_ascii_whitespace());
        if token.is_empty() {
            bail!(
                "Remote token mode requires environment variable '{}' to be non-empty",
                env_name
            );
        }
        if !token.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
            bail!("Invalid token format: contains illegal or non-printable ASCII characters");
        }
        let header_val = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .context("failed to construct Authorization header from token")?;
        req = req.header(reqwest::header::AUTHORIZATION, header_val);
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("remote server unreachable at {url}"))?;

    let status = resp.status();

    // 4. 重定向特异拦截（绝不读 body，结构性杜绝泄露）
    if status.is_redirection() {
        bail!(
            "remote server returned {status} redirect for {url} — redirection is disabled for security; check remote.url"
        );
    }

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let raw_msg = if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
            json.get("error")
                .and_then(|e| e.get("message").or_else(|| e.get("code")))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or(body)
        } else {
            body
        };
        let safe_msg = sanitize_error_snippet(&raw_msg);
        bail!("remote server returned {status} for client-config endpoint: {safe_msg}");
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .context("invalid JSON from remote client-config")?;

    // 5. 组装标准的纯 Remote 模式 Config
    remote_config_from_client_data(&data, remote, client)
}

/// 从远端 client-config 数据直接构造纯 Remote 模式 Config（单次构造即符合契约）
pub fn remote_config_from_client_data(
    data: &serde_json::Value,
    remote: &config::RemoteConfig,
    client: &str,
) -> Result<config::Config> {
    let d = data
        .get("data")
        .context("missing 'data' field in remote client-config response")?;

    let resp_client = d
        .get("client")
        .and_then(|c| c.as_str())
        .context("missing or non-string 'client' field in remote client-config response")?;
    if resp_client != client {
        bail!(
            "remote client-config response client mismatch: expected '{client}', got '{resp_client}'"
        );
    }

    // Preferred shape: provider-agnostic model catalog. The client synthesizes
    // a local `llm-proxy` provider and never receives real provider topology.
    if let Some(catalog_value) = d.get("catalog") {
        let catalog: crate::client_view::ClientModelCatalog =
            serde_json::from_value(catalog_value.clone())
                .context("failed to parse client model catalog")?;
        let mut final_remote = remote.clone();
        if final_remote.auth_mode == Some(config::RemoteAuthMode::Open) {
            final_remote.auth_token_env = String::new();
        }
        let mut cfg = crate::client_view::catalog_to_config(&catalog, &remote.url)?;
        cfg.remote = Some(final_remote);
        return Ok(cfg);
    }

    let models_obj = d
        .get("models")
        .and_then(|m| m.as_object())
        .context("missing or non-object 'models' field in remote client-config response")?;
    if models_obj.is_empty() {
        bail!("'models' object in remote client-config response must not be empty");
    }

    let providers_obj = d
        .get("providers")
        .and_then(|p| p.as_object())
        .context("missing or non-object 'providers' field in remote client-config response")?;
    if providers_obj.is_empty() {
        bail!("'providers' object in remote client-config response must not be empty");
    }

    let mut final_remote = remote.clone();
    if final_remote.auth_mode == Some(config::RemoteAuthMode::Open) {
        final_remote.auth_token_env = String::new();
    }

    // 裁剪版客户端可能不认识私仓 server 的扩展 provider/auth。逐个试解析，
    // 跳过不支持的 provider，并移除引用它们的模型，避免整个 launch 失败。
    let mut providers_value = serde_json::json!({ "providers": providers_obj });
    let removed = crate::status::probe::strip_unsupported_providers(&mut providers_value);
    let providers_obj = providers_value["providers"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut models_obj = models_obj.clone();
    if !removed.is_empty() {
        let removed_set: std::collections::BTreeSet<&str> =
            removed.iter().map(String::as_str).collect();
        let to_remove: Vec<String> = models_obj
            .iter()
            .filter(|(_, model)| {
                [
                    "openai_chat_providers",
                    "openai_responses_providers",
                    "anthropic_providers",
                ]
                .iter()
                .any(|key| {
                    model
                        .get(*key)
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|bindings| {
                            bindings.iter().any(|binding| {
                                binding
                                    .get("name")
                                    .and_then(serde_json::Value::as_str)
                                    .is_some_and(|name| removed_set.contains(name))
                            })
                        })
                })
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in &to_remove {
            models_obj.remove(name);
        }
        for name in &removed {
            eprintln!("  ⚠ 远端 provider `{name}` 含本客户端不支持的配置，已在 launch 中跳过");
        }
        if !to_remove.is_empty() {
            eprintln!(
                "  ⚠ 已跳过 {} 个绑定到不支持 provider 的模型",
                to_remove.len()
            );
        }
    }
    if models_obj.is_empty() {
        bail!("no models remain after filtering unsupported providers");
    }

    let cfg: config::Config = serde_json::from_value(serde_json::json!({
        "server": null,
        "remote": final_remote,
        "models": models_obj,
        "providers": providers_obj,
    }))
    .context("failed to deserialize models and providers into remote config")?;

    Ok(cfg)
}

/// 从 client-config 响应构造 Config：models/providers 来自 server，
/// server.listen 用 CLI 视角的 base_url host:port（launch 地址推导正确）。
fn config_from_client_data(data: &serde_json::Value, base_url: &str) -> Result<config::Config> {
    let d = data.get("data").context("missing client-config data")?;
    let listen = url::Url::parse(base_url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|host| format!("{host}:{}", u.port().unwrap_or(8989)))
        })
        .unwrap_or_else(|| "127.0.0.1:8989".to_string());
    let cfg = serde_json::from_value(serde_json::json!({
        "server": { "listen": listen },
        "models": d.get("models").cloned().unwrap_or(serde_json::json!({})),
        "providers": d.get("providers").cloned().unwrap_or(serde_json::json!({})),
    }))
    .context("failed to build config from client-config data")?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_remote_config(dir: &std::path::Path, remote_url: &str) -> std::path::PathBuf {
        let cfg_path = dir.join("config.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        writeln!(
            f,
            r#"
[remote]
url = "{remote_url}"
auth_mode = "open"
"#
        )
        .unwrap();
        cfg_path
    }

    fn write_remote_token_config(
        dir: &std::path::Path,
        remote_url: &str,
        env_var: &str,
    ) -> std::path::PathBuf {
        let cfg_path = dir.join("config.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        writeln!(
            f,
            r#"
[remote]
url = "{remote_url}"
auth_token_env = "{env_var}"
"#
        )
        .unwrap();
        cfg_path
    }

    fn write_local_config(dir: &std::path::Path) -> std::path::PathBuf {
        let cfg_path = dir.join("config.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        writeln!(
            f,
            r#"
[server]
listen = "127.0.0.1:19900"

[providers.local]
api_key_env = "TEST_API_KEY"

[providers.local.openai_chat]
url = "https://api.example.com/v1/chat/completions"
"#
        )
        .unwrap();
        cfg_path
    }

    async fn spawn_mock_remote_server(
        client_config_body: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route(
            "/admin/client-config/{client}",
            axum::routing::get({
                let body = client_config_body.clone();
                move || {
                    let body = body.clone();
                    async move { axum::Json(body) }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base_url, handle)
    }

    async fn spawn_mock_remote_server_with_status(
        status_code: u16,
        body: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route(
            "/admin/client-config/{client}",
            axum::routing::get({
                let body = body.clone();
                move || {
                    let body = body.clone();
                    async move {
                        (
                            axum::http::StatusCode::from_u16(status_code).unwrap(),
                            axum::Json(body),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base_url, handle)
    }

    #[test]
    fn test_sanitize_error_snippet_patterns() {
        // 1. Bearer token
        let s1 = sanitize_error_snippet("failed with Bearer secret_token_12345");
        assert_eq!(s1, "failed with Bearer [REDACTED]");

        // 2. token= parameter
        let s2 = sanitize_error_snippet("error at url?token=abcdef1234567890");
        assert_eq!(s2, "error at url?token=[REDACTED]");

        // 3. Authorization header
        let s3 = sanitize_error_snippet("header: Authorization: Bearer eyJhbGciOiJIUzI1NiJ9");
        assert_eq!(s3, "header: Authorization: [REDACTED]");

        // 4. High-entropy secret in key-value context
        let s4 = sanitize_error_snippet("api_key: abcdef1234567890abcdef1234567890");
        assert_eq!(s4, "api_key: [REDACTED_SECRET]");

        // 5. Length truncation
        let long_str = "a".repeat(300);
        let truncated = sanitize_error_snippet(&long_str);
        assert!(truncated.ends_with("..."));
        assert!(truncated.len() <= 160);
    }

    #[test]
    fn test_is_loopback_host_resolution() {
        assert!(is_loopback_host(
            &url::Url::parse("http://127.0.0.1:8989").unwrap()
        ));
        assert!(is_loopback_host(
            &url::Url::parse("http://localhost:8989").unwrap()
        ));
        assert!(is_loopback_host(
            &url::Url::parse("http://[::1]:8989").unwrap()
        ));
        assert!(!is_loopback_host(
            &url::Url::parse("http://example.com:8989").unwrap()
        ));
        assert!(!is_loopback_host(
            &url::Url::parse("http://192.168.1.1:8989").unwrap()
        ));
    }

    #[test]
    fn test_is_valid_env_var_name_checks() {
        assert!(is_valid_env_var_name("API_KEY"));
        assert!(is_valid_env_var_name("_SECRET_1"));
        assert!(!is_valid_env_var_name(""));
        assert!(!is_valid_env_var_name("1_KEY"));
        assert!(!is_valid_env_var_name("MY KEY"));
        assert!(!is_valid_env_var_name("KEY-NAME"));
    }

    #[tokio::test]
    async fn remote_mode_fetches_client_config_via_http() {
        let mock_body = serde_json::json!({
            "status": "ok",
            "data": {
                "client": "codex",
                "models": { "m1": {} },
                "providers": { "p1": {} }
            }
        });
        let (base_url, _handle) = spawn_mock_remote_server(mock_body).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_remote_config(dir.path(), &base_url);

        let cfg = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap();
        assert!(cfg.server.is_none());
        assert!(cfg.remote.is_some());
        let remote = cfg.remote.unwrap();
        assert_eq!(remote.url, base_url);
    }

    #[tokio::test]
    async fn remote_mode_token_missing_fails_closed_before_network() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_remote_token_config(
            dir.path(),
            "http://127.0.0.1:8989",
            "LP_TEST_NON_EXISTENT_VAR",
        );

        let err = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("LP_TEST_NON_EXISTENT_VAR")
                && msg.contains("requires environment variable"),
            "expected fail-closed error before sending request, got: {msg}"
        );
    }

    #[tokio::test]
    async fn remote_mode_token_empty_or_whitespace_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let env_key = "LP_TEST_EMPTY_TOKEN_VAR";
        unsafe { std::env::set_var(env_key, "   ") };
        let cfg_path = write_remote_token_config(dir.path(), "http://127.0.0.1:8989", env_key);

        let err = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("to be non-empty"),
            "expected whitespace token to fail closed, got: {msg}"
        );
        unsafe { std::env::remove_var(env_key) };
    }

    #[tokio::test]
    async fn remote_mode_insecure_http_rejected_for_non_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_remote_config(dir.path(), "http://example.com/lp");

        let err = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("requires HTTPS for non-loopback endpoints"),
            "expected insecure HTTP rejection, got: {msg}"
        );
    }

    #[tokio::test]
    async fn remote_mode_redirection_rejected_without_reading_body() {
        let app = axum::Router::new().route(
            "/admin/client-config/{client}",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::MOVED_PERMANENTLY,
                    [("Location", "https://evil.com/leak")],
                    "sensitive response body that must not be read",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let _handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_remote_config(dir.path(), &base_url);

        let err = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("redirection is disabled for security"),
            "expected redirection rejection, got: {msg}"
        );
        assert!(
            !msg.contains("sensitive response body"),
            "3xx response body must not be reflected, got: {msg}"
        );
    }

    #[tokio::test]
    async fn remote_mode_auth_header_injected_and_remote_context_preserved() {
        let env_key = "LP_TEST_VALID_TOKEN_VAR";
        unsafe { std::env::set_var(env_key, "secret_key_12345") };

        let app = axum::Router::new().route(
            "/lp/admin/client-config/{client}",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
                if auth == Some("Bearer secret_key_12345") {
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "status": "ok",
                            "data": {
                                "client": "codex",
                                "models": { "gpt-4": {} },
                                "providers": { "openai": {} }
                            }
                        })),
                    )
                } else {
                    (
                        axum::http::StatusCode::UNAUTHORIZED,
                        axum::Json(serde_json::json!({
                            "error": { "message": "unauthorized request" }
                        })),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/lp");
        let _handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_remote_token_config(dir.path(), &base_url, env_key);

        let cfg = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap();
        assert!(cfg.server.is_none());
        assert!(cfg.remote.is_some());
        let remote = cfg.remote.unwrap();
        assert_eq!(remote.url, base_url);
        assert_eq!(remote.auth_token_env, env_key);

        unsafe { std::env::remove_var(env_key) };
    }

    #[tokio::test]
    async fn remote_mode_empty_models_or_providers_fails_closed() {
        let mock_body = serde_json::json!({
            "status": "ok",
            "data": {
                "client": "codex",
                "models": {},
                "providers": { "p1": {} }
            }
        });
        let (base_url, _handle) = spawn_mock_remote_server(mock_body).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_remote_config(dir.path(), &base_url);

        let err = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("must not be empty"),
            "expected empty models to fail closed, got: {msg}"
        );
    }

    #[tokio::test]
    async fn remote_mode_client_mismatch_fails_closed() {
        let mock_body = serde_json::json!({
            "status": "ok",
            "data": {
                "client": "pi",
                "models": { "m1": {} },
                "providers": { "p1": {} }
            }
        });
        let (base_url, _handle) = spawn_mock_remote_server(mock_body).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_remote_config(dir.path(), &base_url);

        let err = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("client mismatch"),
            "expected client mismatch to fail closed, got: {msg}"
        );
    }

    #[tokio::test]
    async fn remote_mode_open_negative_assertion() {
        let app = axum::Router::new().route(
            "/admin/client-config/{client}",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                assert!(
                    headers.get("authorization").is_none(),
                    "open mode must not send Authorization header"
                );
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "data": {
                        "client": "codex",
                        "models": { "m1": {} },
                        "providers": { "p1": {} }
                    }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let _handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_remote_config(dir.path(), &base_url);

        let cfg = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap();
        let remote = cfg.remote.unwrap();
        assert_eq!(remote.auth_token_env, "");
    }

    #[tokio::test]
    async fn remote_mode_unreachable_reports_remote_error() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_remote_config(dir.path(), "http://127.0.0.1:1");

        let err = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("remote server unreachable") || msg.contains("unreachable"),
            "expected remote-unreachable error, got: {msg}"
        );
        assert!(
            !msg.contains("start the server or use a config with models"),
            "remote error should not suggest starting local server, got: {msg}"
        );
    }

    #[tokio::test]
    async fn remote_mode_server_error_reports_status() {
        let err_body = serde_json::json!({"error": "client not supported"});
        let (base_url, _handle) = spawn_mock_remote_server_with_status(500, err_body).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_remote_config(dir.path(), &base_url);

        let err = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("500") || msg.contains("remote server returned"),
            "expected remote server error with status, got: {msg}"
        );
    }

    #[tokio::test]
    async fn local_mode_returns_config_directly() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_local_config(dir.path());

        let cfg = launch_config(&cfg_path, "codex", &Default::default())
            .await
            .unwrap();
        assert!(cfg.providers.contains_key("local"));
        assert!(cfg.remote.is_none());
    }

    #[test]
    fn test_launch_model_filter_cascade_intersection() {
        use crate::cli::types::LaunchFilterArgs;
        use crate::config::{Config, LaunchConfig, ModelConfig, ProviderBinding, ProviderConfig};

        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "models": {},
            "providers": {}
        }))
        .unwrap();

        // Server/Base models: A, B, C, D
        for (m, p) in [("A", "p_a"), ("B", "p_b"), ("C", "p_c"), ("D", "p_d")] {
            let mut mc: ModelConfig = serde_json::from_value(serde_json::json!({
                "context_window": 1000,
                "max_output_tokens": 1000
            }))
            .unwrap();
            mc.openai_chat_providers = vec![ProviderBinding {
                name: p.to_string(),
                model: m.to_string(),
            }];
            cfg.models.insert(m.to_string(), mc);
            cfg.providers
                .insert(p.to_string(), ProviderConfig::default());
        }

        // Config level filter: allows A, B, D
        let config_filter = LaunchConfig {
            include_models: vec!["A".to_string(), "B".to_string(), "D".to_string()],
            exclude_models: vec![],
        };

        // CLI level filter: allows A, C
        let cli_filter = LaunchFilterArgs {
            models: vec!["A".to_string(), "C".to_string()],
            exclude_models: vec![],
        };

        // Apply cascade intersection
        apply_launch_model_filter(&mut cfg, &config_filter, &cli_filter).unwrap();

        // Final intersection MUST be strictly {A}
        let remaining_models: Vec<String> = cfg.models.keys().cloned().collect();
        assert_eq!(remaining_models, vec!["A"]);

        // Providers p_b, p_c, p_d must be pruned, only p_a remains
        let remaining_providers: Vec<String> = cfg.providers.keys().cloned().collect();
        assert_eq!(remaining_providers, vec!["p_a"]);
    }

    #[test]
    fn test_launch_model_filter_exclusion_cascade() {
        use crate::cli::types::LaunchFilterArgs;
        use crate::config::{Config, LaunchConfig, ModelConfig};

        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "models": {},
            "providers": {}
        }))
        .unwrap();

        for m in ["A", "B", "C", "D"] {
            let mc: ModelConfig = serde_json::from_value(serde_json::json!({
                "context_window": 1000,
                "max_output_tokens": 1000
            }))
            .unwrap();
            cfg.models.insert(m.to_string(), mc);
        }

        // Config excludes D -> leaves A, B, C
        let config_filter = LaunchConfig {
            include_models: vec![],
            exclude_models: vec!["D".to_string()],
        };

        // CLI excludes B -> leaves A, C
        let cli_filter = LaunchFilterArgs {
            models: vec![],
            exclude_models: vec!["B".to_string()],
        };

        apply_launch_model_filter(&mut cfg, &config_filter, &cli_filter).unwrap();
        let remaining: Vec<String> = cfg.models.keys().cloned().collect();
        assert_eq!(remaining, vec!["A", "C"]);
    }

    #[test]
    fn test_launch_model_filter_empty_intersection_fails_fast_with_diagnostic() {
        use crate::cli::types::LaunchFilterArgs;
        use crate::config::{Config, LaunchConfig, ModelConfig};

        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "models": {},
            "providers": {}
        }))
        .unwrap();

        for m in ["A", "B"] {
            let mc: ModelConfig = serde_json::from_value(serde_json::json!({
                "context_window": 1000,
                "max_output_tokens": 1000
            }))
            .unwrap();
            cfg.models.insert(m.to_string(), mc);
        }

        // Config allows only A
        let config_filter = LaunchConfig {
            include_models: vec!["A".to_string()],
            exclude_models: vec![],
        };

        // CLI requests only B
        let cli_filter = LaunchFilterArgs {
            models: vec!["B".to_string()],
            exclude_models: vec![],
        };

        let err = apply_launch_model_filter(&mut cfg, &config_filter, &cli_filter).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("no models remain after applying launch model filters"));
        assert!(msg.contains("server/base models: [A, B]"));
        assert!(msg.contains("retained by [launch] config: [A]"));
        assert!(msg.contains("CLI filter: include: [B]"));
        assert!(msg.contains("final intersection: []"));
    }

    #[test]
    fn test_launch_model_filter_cli_flag_parsing_and_alias() {
        use crate::cli::types::{Cli, Command, LaunchCommand};
        use clap::Parser;

        // Comma-separated with alias --include-models
        let cli = Cli::try_parse_from([
            "llm-proxy",
            "launch",
            "pi",
            "--include-models",
            "m1,m2",
            "--exclude-models",
            "m3",
        ])
        .unwrap();

        match cli.command {
            Some(Command::Launch {
                command: LaunchCommand::Pi { filter, .. },
            }) => {
                assert_eq!(filter.models, vec!["m1", "m2"]);
                assert_eq!(filter.exclude_models, vec!["m3"]);
            }
            _ => panic!("unexpected command variant"),
        }

        // Standard --models flag repeated
        let cli2 = Cli::try_parse_from([
            "llm-proxy",
            "launch",
            "codex",
            "--models",
            "m1",
            "--models",
            "m2,m3",
        ])
        .unwrap();

        match cli2.command {
            Some(Command::Launch {
                command: LaunchCommand::Codex { filter, .. },
            }) => {
                assert_eq!(filter.models, vec!["m1", "m2", "m3"]);
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn remote_config_skips_unsupported_provider_and_its_models() {
        let data = serde_json::json!({
            "data": {
                "client": "qwen-code",
                "models": {
                    "plain": {
                        "context_window": 100,
                        "max_output_tokens": 10,
                        "openai_chat_providers": [{"name": "plain", "model": "m"}]
                    },
                    "extension-only": {
                        "context_window": 100,
                        "max_output_tokens": 10,
                        "openai_chat_providers": [{"name": "future-provider", "model": "m"}]
                    }
                },
                "providers": {
                    "plain": {
                        "product": "deepseek",
                        "auth": {"type": "api_key_env", "env": "K"},
                        "openai_chat": {"url": "https://example.invalid/v1/chat/completions"}
                    },
                    "future-provider": {
                        "product": "future-product",
                        "auth": {"type": "future_extension_oauth", "account": "x"},
                        "future_extension": {"url": "https://example.invalid/future"}
                    }
                }
            }
        });
        let remote = config::RemoteConfig {
            url: "https://example.invalid/lp".to_string(),
            auth_token_env: "K".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Token),
        };
        let cfg = remote_config_from_client_data(&data, &remote, "qwen-code")
            .expect("unsupported provider must not abort launch config");
        assert!(cfg.providers.contains_key("plain"));
        assert!(!cfg.providers.contains_key("future-provider"));
        assert!(cfg.models.contains_key("plain"));
        assert!(!cfg.models.contains_key("extension-only"));
    }

    #[test]
    fn remote_catalog_launches_provider_hidden_model() {
        let data = serde_json::json!({
            "data": {
                "client": "qwen-code",
                "catalog": {
                    "client_view": 1,
                    "view": "client",
                    "models": {
                        "vendor-model": {
                            "context_window": 200,
                            "max_output_tokens": 20,
                            "protocols": ["openai_chat", "anthropic"]
                        }
                    }
                }
            }
        });
        let remote = config::RemoteConfig {
            url: "https://example.invalid/lp".to_string(),
            auth_token_env: "K".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Token),
        };
        let cfg = remote_config_from_client_data(&data, &remote, "qwen-code")
            .expect("catalog response must build launch config");
        assert!(cfg.models.contains_key("vendor-model"));
        assert!(cfg.providers.contains_key("llm-proxy"));
        assert_eq!(
            cfg.models["vendor-model"].openai_chat_providers[0].name,
            "llm-proxy"
        );
        assert_eq!(
            cfg.remote.as_ref().expect("remote").url,
            "https://example.invalid/lp"
        );
    }
}
