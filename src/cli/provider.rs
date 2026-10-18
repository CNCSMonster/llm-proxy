use super::types::*;
use crate::{admin_client, auth, cache, config, connect, status, tui};
use anyhow::{Context, Result};
use std::io::IsTerminal;

/// 交互判定（纯函数，便于测试）：与既有 `confirm_remove_provider` 同一范式——
/// 设了 `LLM_PROXY_NO_INTERACTIVE` 或 stdin/stdout 任一非 TTY 即视为非交互。
/// **默认（TTY 且未显式关闭）按有交互处理。**
fn cli_interactive_state(no_interactive_env: bool, stdin_tty: bool, stdout_tty: bool) -> bool {
    !no_interactive_env && stdin_tty && stdout_tty
}

fn cli_interactive() -> bool {
    cli_interactive_state(
        std::env::var("LLM_PROXY_NO_INTERACTIVE").is_ok(),
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    )
}

pub async fn run_connect(config_path: &std::path::Path, args: ProviderAddArgs) -> Result<()> {
    let Some(ref product) = args.product else {
        return tui::run(config_path, tui::model::EntryMode::Connect).await;
    };
    if args.models.is_empty() {
        anyhow::bail!("connect requires at least one --model for mature catalog products");
    }

    let result = connect_write(config_path, &args, product, args.skip_probe).await;

    // 探测失败不直接判死（默认按有交互）：
    //  - 交互终端 → 打印不可达清单并询问，确认后以 skip_probe=true 重跑写配置
    //    （等价 TUI 警告框的「继续」）；
    //  - 非交互终端 → 保留原错误退出，并提示用 --skip-probe 显式放行。
    if let Err(error) = &result {
        if args.skip_probe || !crate::admin_client::is_probe_failure(error) {
            return result;
        }
        if cli_interactive() {
            eprintln!("{error:#}");
            eprintln!();
            print!("是否照常写入配置（跳过网络探测）？[y/N] ");
            use std::io::Write;
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .context("failed to read confirmation")?;
            if matches!(line.trim(), "y" | "Y" | "yes" | "YES") {
                return connect_write(config_path, &args, product, true).await;
            }
            anyhow::bail!("连接已取消：存在不可达模型");
        }
        eprintln!(
            "提示：当前为非交互终端，无法询问是否放行。若要忽略探测失败照常写入，请加 --skip-probe。"
        );
    }
    result
}

/// 单次 connect 写入尝试（§15.2 5 步流程：委托优先 → 无 server try_lock → 本地写）。
/// `skip_probe` 同时透传到本地直连与服务端委托两条路径。
async fn connect_write(
    config_path: &std::path::Path,
    args: &ProviderAddArgs,
    product: &str,
    skip_probe: bool,
) -> Result<()> {
    crate::ownership::with_cli_write_lock_or_delegate(
        config_path,
        "llm-proxy connect",
        || {
            let api_key_env = args.api_key_env.clone();
            let no_api_key = args.no_api_key;
            let provider_type = args.provider_type.clone();
            let endpoint_url = args.endpoint_url.clone();
            let models = args.models.clone();
            let product = product.to_string();
            let config_path = config_path.to_path_buf();
            async move {
                connect::add_provider_with_models(
                    &config_path,
                    &product,
                    api_key_env,
                    no_api_key,
                    provider_type,
                    endpoint_url,
                    Some(&models),
                    None,
                    skip_probe,
                )
                .await
            }
        },
        |server| {
            let api_key_env = args.api_key_env.clone();
            let no_api_key = args.no_api_key;
            let provider_type = args.provider_type.clone();
            let endpoint_url = args.endpoint_url.clone();
            let models = args.models.clone();
            let product = product.to_string();
            Box::pin(async move {
                let result = server
                    .add_provider_with_options(
                        &product,
                        None,
                        api_key_env.as_deref(),
                        no_api_key,
                        provider_type.as_deref(),
                        endpoint_url.as_deref(),
                        Some(&models),
                        skip_probe,
                        None,
                    )
                    .await?;
                if let Some(message) = result
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.as_str())
                {
                    println!("{message}");
                }
                Ok(())
            })
        },
    )
    .await
}

pub async fn run_provider_command(
    config_path: &std::path::Path,
    select: Option<String>,
    command: Option<ProviderCommand>,
) -> Result<()> {
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let dirs = crate::service::StateDirs::resolve(config_dir);
    if let Some(name) = select {
        if command.is_some() {
            anyhow::bail!("provider --select cannot be combined with a provider subcommand");
        }
        let cfg = config::Config::load(config_path)?;
        return tui::run(config_path, provider_select_entry(&cfg, name)?).await;
    }
    match command {
        None => {
            tui::run(
                config_path,
                tui::model::EntryMode::ProviderTui { select: None },
            )
            .await
        }
        Some(ProviderCommand::List { json }) => {
            if json {
                // JSON mode: try server delegation first, fall back to local config
                match admin_client::detect_server(config_path).await {
                    Ok(Some(server)) => {
                        crate::ownership::ensure_same_config(&server, config_path)?;
                        let status = server.status().await?;
                        println!("{}", serde_json::to_string_pretty(&status)?);
                        return Ok(());
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("{e}");
                        return Err(e);
                    }
                }
                // Local fallback: output config as JSON
                let cfg = config::Config::load(config_path)?;
                let providers: Vec<serde_json::Value> = cfg
                    .providers
                    .iter()
                    .map(|(id, provider)| {
                        let protocols: Vec<String> = provider
                            .endpoints()
                            .iter()
                            .map(|(protocol, endpoint)| {
                                if endpoint.url.is_some() {
                                    protocol.route_key().to_string()
                                } else {
                                    format!("{}(derived)", protocol.route_key())
                                }
                            })
                            .collect();
                        let url = provider
                            .endpoints()
                            .iter()
                            .find_map(|(_, endpoint)| endpoint.url.as_deref())
                            .unwrap_or("-");
                        let (state, auth) = status::provider_auth_summary(
                            id,
                            provider,
                            &auth::state_path_for_config(config_path),
                        );
                        serde_json::json!({
                            "id": id,
                            "state": state,
                            "protocols": protocols,
                            "url": url,
                            "auth": auth,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&providers)?);
            } else {
                // Table mode: load config and display formatted table
                let cfg = config::Config::load(config_path)?;
                status::print_providers(
                    &cfg,
                    config_path,
                    &status::ProbeSelection::default(),
                    status::AuthScope::Local,
                );
            }
            Ok(())
        }
        Some(ProviderCommand::Add(args)) => {
            let Some(ref product) = args.product else {
                return tui::run(config_path, tui::model::EntryMode::Connect).await;
            };
            let is_custom = args.provider_type.is_some() || args.endpoint_url.is_some();
            if !is_custom && args.models.is_empty() {
                anyhow::bail!(
                    "provider add for mature catalog products requires at least one --model"
                );
            }
            // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写；
            // 未获取锁且 holder=server 时重试 detect_server 并委托
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy provider add",
                || {
                    let api_key_env = args.api_key_env.clone();
                    let no_api_key = args.no_api_key;
                    let provider_type = args.provider_type.clone();
                    let endpoint_url = args.endpoint_url.clone();
                    let models = args.models.clone();
                    let product = product.clone();
                    async move {
                        connect::add_provider_with_models(
                            config_path,
                            &product,
                            api_key_env,
                            no_api_key,
                            provider_type,
                            endpoint_url,
                            Some(&models),
                            None,
                            args.skip_probe,
                        )
                        .await
                    }
                },
                |server| {
                    let args = args.clone();
                    let product = product.clone();
                    Box::pin(async move {
                        let result = server
                            .add_provider(
                                &product,
                                None,
                                args.api_key_env.as_deref(),
                                args.no_api_key,
                                args.provider_type.as_deref(),
                                args.endpoint_url.as_deref(),
                                Some(&args.models),
                            )
                            .await?;
                        if let Some(message) = result
                            .get("data")
                            .and_then(|d| d.get("message"))
                            .and_then(|m| m.as_str())
                        {
                            println!("{message}");
                        }
                        Ok(())
                    })
                },
            )
            .await
        }
        Some(ProviderCommand::Reselect { name, models }) => {
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy provider reselect",
                || {
                    let name = name.clone();
                    let models = models.clone();
                    let config_path = config_path.to_path_buf();
                    async move { connect::reselect_provider_models(&config_path, &name, &models) }
                },
                |server| {
                    let name = name.clone();
                    let models = models.clone();
                    Box::pin(async move {
                        let result = server.reselect_provider(&name, &models).await?;
                        if let Some(message) = result
                            .get("data")
                            .and_then(|data| data.get("message"))
                            .and_then(|message| message.as_str())
                        {
                            println!("{message}");
                        }
                        Ok(())
                    })
                },
            )
            .await
        }
        Some(ProviderCommand::Info { name }) => {
            // Try server delegation first（读操作走 HTTP 公开接口）
            match admin_client::detect_server(config_path).await {
                Ok(Some(server)) => {
                    crate::ownership::ensure_same_config(&server, config_path)?;
                    if let Some(id) = name.as_deref() {
                        match server.provider_info(id).await {
                            Ok(result) => {
                                // 写缓存：远程模式 server 不可达时兜底
                                let _ = cache::QueryCache::new_in(&dirs.state_dir)
                                    .save(&format!("provider-info:{id}"), &result);
                                if let Some(data) = result.get("data") {
                                    status::print_provider_info_json(data);
                                    return Ok(());
                                }
                                anyhow::bail!("server returned no data");
                            }
                            Err(e) => {
                                eprintln!("Server error: {e}");
                                return Err(e);
                            }
                        }
                    }
                    // 无 name：列出全部（委托走 status 数据）
                    if let Ok(status) = server.status().await {
                        println!("Server: {}", serde_json::to_string_pretty(&status)?);
                        return Ok(());
                    }
                }
                Ok(None) => {
                    // 无法委托：远程模式（config 极简，无本地 provider）读缓存兜底
                    let is_remote = config::Config::load(config_path)
                        .map(|cfg| cfg.providers.is_empty() && cfg.models.is_empty())
                        .unwrap_or(false);
                    if is_remote && let Some(id) = name.as_deref() {
                        let cache = cache::QueryCache::new_in(&dirs.state_dir);
                        if let Some((cached_at, result)) =
                            cache.load(&format!("provider-info:{id}"))
                        {
                            println!(
                                "ℹ 从缓存获取（{}，可能过期）",
                                cache::QueryCache::format_cached_at(cached_at)
                            );
                            if let Some(data) = result.get("data") {
                                status::print_provider_info_json(data);
                                return Ok(());
                            }
                        }
                        anyhow::bail!("server unreachable and no cached provider info for {id}");
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    return Err(e);
                }
            }
            // Fallback: local mode
            let cfg = config::Config::load(config_path)?;
            status::print_provider_info(&cfg, config_path, name.as_deref()).await;
            Ok(())
        }
        Some(ProviderCommand::Copy {
            source,
            name,
            api_key_env,
            no_api_key,
        }) => {
            // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy provider copy",
                || {
                    let source = source.clone();
                    let name = name.clone();
                    let api_key_env = api_key_env.clone();
                    async move {
                        let result = connect::copy_provider(
                            config_path,
                            &source,
                            &name,
                            api_key_env,
                            no_api_key,
                        )?;
                        if result.requires_oauth_login {
                            let cfg = config::Config::load(config_path)?;
                            auth::login_provider(
                                config_path,
                                &cfg,
                                &auth::state_path_for_config(config_path),
                                &name,
                            )
                            .await?;
                        }
                        Ok(())
                    }
                },
                |server| {
                    let source = source.clone();
                    let name = name.clone();
                    let api_key_env = api_key_env.clone();
                    let config_path = config_path.to_path_buf();
                    Box::pin(async move {
                        let result = server
                            .copy_provider(&source, &name, api_key_env.as_deref(), no_api_key)
                            .await?;
                        let data = result.get("data");
                        if let Some(message) =
                            data.and_then(|d| d.get("message")).and_then(|m| m.as_str())
                        {
                            println!("{message}");
                        }
                        let requires_oauth_login = data
                            .and_then(|d| d.get("requires_oauth_login"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if requires_oauth_login {
                            let cfg = config::Config::load(&config_path)?;
                            auth::login_provider(
                                &config_path,
                                &cfg,
                                &auth::state_path_for_config(&config_path),
                                &name,
                            )
                            .await?;
                        }
                        Ok(())
                    })
                },
            )
            .await
        }
        Some(ProviderCommand::Remove { name, force }) => {
            // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy provider remove",
                || {
                    let name = name.clone();
                    async move { connect::remove_provider(config_path, &name, force) }
                },
                |server| {
                    let name = name.clone();
                    Box::pin(async move {
                        let result = server.remove_provider(&name, force).await?;
                        println!(
                            "{}",
                            result
                                .get("data")
                                .and_then(|d| d.get("message"))
                                .and_then(|m| m.as_str())
                                .unwrap_or("removed")
                        );
                        Ok(())
                    })
                },
            )
            .await
        }
        Some(ProviderCommand::ResetUsage { name, force }) => {
            connect::reset_usage(config_path, &name, force).await
        }
        Some(ProviderCommand::Login { name }) => {
            tui::run(config_path, tui::model::EntryMode::OAuthLogin(name)).await
        }
        Some(ProviderCommand::Logout { name }) => {
            // 统一走委托入口，server 存活时由它刷新内存凭据缓存。
            let message = logout_provider_delegating(config_path, &name).await?;
            println!("{message}");
            Ok(())
        }
        Some(ProviderCommand::Relogin { name }) => {
            // 登出半段与 `provider logout` 共用同一委托入口。此前它直接
            // `auth::logout_provider` 写盘，server 存活时其内存缓存不被刷新，于是
            // "已登出仍可用"的窗口在 relogin 未完成（设备码中断/失败）时暴露。
            // 登录半段的凭据写入本就自行委托（`auth::login_provider` →
            // `write_oauth_account_at`），无需再包一层。
            let cfg = config::Config::load(config_path)?;
            let message = logout_provider_delegating(config_path, &name).await?;
            println!("{message}");
            auth::login_provider(
                config_path,
                &cfg,
                &auth::state_path_for_config(config_path),
                &name,
            )
            .await
        }
        Some(ProviderCommand::Refresh { name }) => {
            let cfg = config::Config::load(config_path)?;
            auth::refresh_provider(&cfg, &auth::state_path_for_config(config_path), &name).await
        }
        Some(ProviderCommand::Fallback { command }) => {
            match command {
                FallbackCommand::Add(args) => {
                    crate::ownership::with_cli_write_lock_or_delegate(
                    config_path,
                    "llm-proxy provider fallback add",
                    || {
                        let provider = args.provider.clone();
                        let target = args.target.clone();
                        let bindings = args.bindings.clone();
                        let config_path = config_path.to_path_buf();
                        async move {
                            let results = crate::fallback::add_fallback(
                                &config_path,
                                &provider,
                                &target,
                                &bindings,
                            )?;
                            crate::fallback::print_results(&results, &provider, &target);
                            let has_failure = results.iter().any(|r| matches!(r, crate::fallback::FallbackBindingResult::Failed { .. }));
                            if has_failure {
                                std::process::exit(1);
                            }
                            Ok(())
                        }
                    },
                    |_server| {
                        // TODO: server delegation not yet implemented for fallback
                        Box::pin(async move {
                            anyhow::bail!(
                                "provider fallback add is not yet supported via server delegation; \
                                 stop the server and retry"
                            )
                        })
                    },
                )
                .await
                }
            }
        }
    }
}

/// 登出一个 provider 的 OAuth 凭据 —— CLI 侧的**唯一**登出入口。
///
/// server 存活时必须走管理通道委托：`POST /admin/oauth/logout` 在删盘凭据之后还会
/// 刷新 server 进程内的 `oauth_accounts` 缓存。本地直写只改磁盘，而
/// `resolve_oauth_auth` 优先读内存缓存，于是留下"已登出仍可用"的窗口。
///
/// 修 `provider logout` 时漏掉了 `provider relogin` 这个同语义的
/// 第二调用点；收敛到这一个函数后，两条命令共用同一接线，不再各自漂移。
async fn logout_provider_delegating(config_path: &std::path::Path, name: &str) -> Result<String> {
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let dirs = crate::service::StateDirs::resolve(config_dir);
    logout_provider_delegating_in(config_path, &dirs.runtime_dir, name).await
}

/// [`logout_provider_delegating`] 的可注入 runtime dir 形态，供测试把委托目标与所有权锁
/// 都关在临时目录里（否则 mock 出的 server 会把写打到真实实例的 socket 上）。
async fn logout_provider_delegating_in(
    config_path: &std::path::Path,
    runtime_dir: &std::path::Path,
    name: &str,
) -> Result<String> {
    crate::ownership::with_cli_write_lock_or_delegate_at(
        config_path,
        runtime_dir,
        "llm-proxy provider logout",
        || {
            let name = name.to_string();
            let config_path = config_path.to_path_buf();
            async move {
                let cfg = config::Config::load(&config_path)?;
                let removed =
                    auth::logout_provider(&cfg, &auth::state_path_for_config(&config_path), &name)?;
                Ok(if removed == 0 {
                    format!("No OAuth credential found for provider={name}")
                } else {
                    format!("Logged out provider={name}")
                })
            }
        },
        |server| {
            let name = name.to_string();
            Box::pin(async move {
                let result = server.oauth_logout(&name).await?;
                Ok(result
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("logged out")
                    .to_string())
            })
        },
    )
    .await
}

fn provider_select_entry(cfg: &config::Config, name: String) -> Result<tui::model::EntryMode> {
    if !cfg.providers.contains_key(&name) {
        anyhow::bail!("unknown provider {name:?}");
    }
    Ok(tui::model::EntryMode::ProviderTui { select: Some(name) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn provider_select_entry_rejects_unknown_name() {
        let cfg = config::default_deepseek_config();
        let err = provider_select_entry(&cfg, "missing".into())
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown provider"));
        assert!(err.contains("missing"));
    }

    #[test]
    fn provider_select_entry_targets_known_provider() {
        let cfg = config::default_deepseek_config();
        match provider_select_entry(&cfg, "deepseek".into()).unwrap() {
            tui::model::EntryMode::ProviderTui { select } => {
                assert_eq!(select.as_deref(), Some("deepseek"));
            }
            other => panic!("unexpected entry mode: {other:?}"),
        }
    }

    // ── CLI 登出半段必须经管理通道委托 ─────────────────────────────
    //
    // 三个 mock 一起构成回归基线：`/admin/ping`（发现 server + identity 门禁）
    // 与原始 UDS 管理通道（写操作落点）。修复前 `provider relogin` 直接
    // `auth::logout_provider` 写盘，既不通 UDS 也不刷新 server 缓存。

    /// 写一份带 openai OAuth provider 的 config，并在同目录放好凭据文件。
    /// 凭据内容是"本地直写会改动它"的探针，因此委托测试可以断言磁盘未被触碰。
    fn write_oauth_provider_fixture(config_path: &std::path::Path, listen: &str) {
        std::fs::write(
            config_path,
            format!(
                "[server]\nlisten = \"{listen}\"\n\n\
                 [providers.ag1]\n\
                 auth = {{ type = \"openai_oauth\", account = \"ag1\" }}\n\
                 [providers.ag1.openai_chat]\nurl = \"https://example.test/v1\"\n"
            ),
        )
        .expect("write config");
        write_accounts(&auth::state_path_for_config(config_path));
    }

    fn write_accounts(path: &std::path::Path) {
        // 字段必须通过 `validate_openai_account`，否则加载时被当成无效账号跳过，
        // fixture 就成了"凭据本来就不存在"的假场景。
        std::fs::write(
            path,
            serde_json::json!({
                "version": 1,
                "openai": {
                    "ag1": {
                        "account_label": "tester@example.test",
                        "access_token": "access-token-fixture-value",
                        "refresh_token": "refresh-token-fixture-value",
                        "expires_at_unix": 4_000_000_000_i64,
                        "updated_at_unix": 1_700_000_000_i64,
                    }
                }
            })
            .to_string(),
        )
        .expect("write accounts");
    }

    fn account_present(path: &std::path::Path) -> bool {
        std::fs::read_to_string(path)
            .map(|text| text.contains("\"ag1\""))
            .unwrap_or(false)
    }

    /// UDS 管理通道 mock：同时提供 `/admin/ping`（用于 detect_server 探活）
    /// 和 `/admin/oauth/logout`（写操作端点）。detect_server 改走 UDS，
    /// 因此 ping 也必须挂在 UDS 上。
    #[cfg(unix)]
    #[allow(dead_code)]
    async fn spawn_mock_mgmt_uds(
        runtime_dir: &std::path::Path,
        seen: Arc<Mutex<Vec<String>>>,
    ) -> std::path::PathBuf {
        spawn_mock_mgmt_uds_with_identity(
            runtime_dir,
            seen,
            env!("CARGO_PKG_VERSION").to_string(),
            None,
        )
        .await
    }

    /// 带 config_identity 的 UDS mock（用于 identity 门禁测试）。
    #[cfg(unix)]
    async fn spawn_mock_mgmt_uds_with_identity(
        runtime_dir: &std::path::Path,
        seen: Arc<Mutex<Vec<String>>>,
        version: String,
        config_identity: Option<String>,
    ) -> std::path::PathBuf {
        let sock = crate::service::socket_path_in(runtime_dir);
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind mock mgmt uds");
        let app = axum::Router::new()
            .route(
                "/admin/ping",
                axum::routing::get(move || {
                    let version = version.clone();
                    let identity = config_identity.clone();
                    async move {
                        let mut body = serde_json::json!({
                            "status": "ok",
                            "version": version,
                        });
                        if let Some(id) = identity {
                            body["config_identity"] = serde_json::Value::String(id);
                        }
                        axum::Json(body)
                    }
                }),
            )
            .route(
                "/admin/oauth/logout",
                axum::routing::post(move |body: String| {
                    let seen = seen.clone();
                    async move {
                        if let Ok(mut guard) = seen.lock() {
                            guard.push(body);
                        }
                        axum::Json(serde_json::json!({
                            "status": "ok",
                            "data": { "message": "logged out via management channel" },
                            "error": null,
                        }))
                    }
                }),
            );
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let service = hyper_util::service::TowerToHyperService::new(
                    app.clone().into_service::<hyper::body::Incoming>(),
                );
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        });
        sock
    }

    fn runtime_dir_of(dir: &std::path::Path) -> std::path::PathBuf {
        let runtime_dir = dir.join("run");
        std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        runtime_dir
    }

    /// 回归基线（server 存活）：登出走 UDS，磁盘凭据**不**由 CLI 直写。
    /// 修复前的 `provider relogin` 在此断言下双重失败——UDS 上收不到任何请求，
    /// 且 `oauth_accounts.json` 被本地删掉，而 server 缓存仍持着旧凭据。
    #[tokio::test]
    #[cfg(unix)]
    async fn logout_delegates_to_management_channel_when_server_is_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let runtime_dir = runtime_dir_of(dir.path());
        // detect_server 改走 UDS，不再需要 TCP mock ping
        write_oauth_provider_fixture(&config_path, "127.0.0.1:0");
        let identity = config::config_identity(config_path.parent().unwrap_or(&config_path));
        assert!(
            account_present(&auth::state_path_for_config(&config_path)),
            "fixture must start with the credential on disk"
        );

        let uds_requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let _sock = spawn_mock_mgmt_uds_with_identity(
            &runtime_dir,
            uds_requests.clone(),
            env!("CARGO_PKG_VERSION").to_string(),
            Some(identity),
        )
        .await;

        let message = logout_provider_delegating_in(&config_path, &runtime_dir, "ag1")
            .await
            .expect("delegated logout must succeed");

        assert_eq!(message, "logged out via management channel");
        let requests = uds_requests.lock().expect("uds mock lock");
        assert_eq!(
            requests.as_slice(),
            ["{\"provider_id\":\"ag1\"}"],
            "写操作必须落在管理通道的登出端点上"
        );
        assert!(
            account_present(&auth::state_path_for_config(&config_path)),
            "server 存活时 CLI 不得直写凭据文件——那会绕过 server 的内存缓存刷新"
        );
    }

    /// server 用的是**另一份** config 时必须硬失败，既不委托也不回落本地写
    /// （identity 门禁 + spec §11.8「严禁本地降级」在登出入口同样成立）。
    #[tokio::test]
    #[cfg(unix)]
    async fn logout_refuses_to_delegate_to_foreign_config_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let runtime_dir = runtime_dir_of(dir.path());
        // detect_server 改走 UDS
        write_oauth_provider_fixture(&config_path, "127.0.0.1:0");
        let accounts_path = auth::state_path_for_config(&config_path);
        let uds_requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let _sock = spawn_mock_mgmt_uds_with_identity(
            &runtime_dir,
            uds_requests,
            env!("CARGO_PKG_VERSION").to_string(),
            Some("not-this-config".to_string()),
        )
        .await;

        let err = logout_provider_delegating_in(&config_path, &runtime_dir, "ag1")
            .await
            .expect_err("foreign config identity must be rejected");
        assert!(err.to_string().contains("拒绝委托"), "got: {err}");
        assert!(
            account_present(&accounts_path),
            "被拒绝的委托不得顺手本地删凭据"
        );
    }

    /// 无 server 时仍须本地删凭据并如实报告（委托优先不等于本地模式失效）。
    #[tokio::test]
    async fn logout_writes_locally_when_no_server_is_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let runtime_dir = runtime_dir_of(dir.path());
        write_oauth_provider_fixture(&config_path, "127.0.0.1:1");
        let accounts_path = auth::state_path_for_config(&config_path);

        let message = logout_provider_delegating_in(&config_path, &runtime_dir, "ag1")
            .await
            .expect("standalone logout");

        assert_eq!(message, "Logged out provider=ag1");
        assert!(
            !account_present(&accounts_path),
            "本地模式必须真的删掉凭据：{accounts_path:?}"
        );
    }

    /// 默认即有交互：TTY 且未设 LLM_PROXY_NO_INTERACTIVE → 询问用户。
    #[test]
    fn cli_interactive_defaults_to_true_on_tty() {
        assert!(cli_interactive_state(false, true, true));
        // 任一端非 TTY（管道/重定向）→ 非交互
        assert!(!cli_interactive_state(false, false, true));
        assert!(!cli_interactive_state(false, true, false));
        // 显式关闭优先于 TTY 检测
        assert!(!cli_interactive_state(true, true, true));
    }

    /// `is_probe_failure` 只认「模型连不上」，凭据缺失不得走"放行写配置"分支。
    #[test]
    fn probe_failure_classification_excludes_auth_not_ready() {
        let probe: anyhow::Error = connect::ProviderAddError::probe_failed("boom").into();
        assert!(crate::admin_client::is_probe_failure(&probe));
        assert!(crate::admin_client::is_recoverable_warning(&probe));

        let auth: anyhow::Error = connect::ProviderAddError::auth_not_ready("missing env").into();
        assert!(
            !crate::admin_client::is_probe_failure(&auth),
            "凭据缺失不是探测失败，不得当作可放行的连不上"
        );

        let invalid: anyhow::Error = connect::ProviderAddError::invalid_config("bad").into();
        assert!(!crate::admin_client::is_probe_failure(&invalid));

        // 服务端委托路径回传的错误（AdminClientError kind = probe_failed）
        let delegated = anyhow::Error::new(crate::admin_client::AdminClientError {
            status: 500,
            kind: Some("probe_failed".to_string()),
            message: "boom".to_string(),
        });
        assert!(crate::admin_client::is_probe_failure(&delegated));
    }
}
