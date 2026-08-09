use super::types::*;
use crate::{admin_client, auth, cache, config, connect, status, tui};
use anyhow::Result;

pub async fn run_connect(config_path: &std::path::Path, args: ProviderAddArgs) -> Result<()> {
    let Some(ref product) = args.product else {
        return tui::run(config_path, tui::model::EntryMode::Connect).await;
    };
    if args.models.is_empty() {
        anyhow::bail!("connect requires at least one --model for mature catalog products");
    }
    // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
    crate::ownership::with_cli_write_lock_or_delegate(
        config_path,
        "llm-proxy connect",
        || {
            let api_key_env = args.api_key_env.clone();
            let no_api_key = args.no_api_key;
            let provider_type = args.provider_type.clone();
            let endpoint_url = args.endpoint_url.clone();
            let models = args.models.clone();
            let product = product.clone();
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
            let product = product.clone();
            Box::pin(async move {
                let result = server
                    .add_provider(
                        &product,
                        api_key_env.as_deref(),
                        no_api_key,
                        provider_type.as_deref(),
                        endpoint_url.as_deref(),
                        Some(&models),
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
    if let Some(name) = select {
        if command.is_some() {
            anyhow::bail!("provider --select cannot be combined with a provider subcommand");
        }
        let cfg = config::Config::load(config_path)?;
        status::print_provider_info(&cfg, Some(&name)).await;
        return Ok(());
    }
    match command {
        None => tui::run(config_path, tui::model::EntryMode::ProviderTui).await,
        Some(ProviderCommand::List { json }) => {
            if json {
                // JSON mode: try server delegation first, fall back to local config
                match admin_client::detect_server(config_path).await {
                    Ok(Some(server)) => {
                        if let Ok(status) = server.status().await {
                            println!("{}", serde_json::to_string_pretty(&status)?);
                            return Ok(());
                        }
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
                            &auth::default_state_path(),
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
                status::print_providers(&cfg);
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
        Some(ProviderCommand::Info { name }) => {
            // Try server delegation first（读操作走 HTTP 公开接口）
            match admin_client::detect_server(config_path).await {
                Ok(Some(server)) => {
                    if let Some(id) = name.as_deref() {
                        match server.provider_info(id).await {
                            Ok(result) => {
                                // 写缓存：远程模式 server 不可达时兜底
                                let _ = cache::QueryCache::new()
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
                        let cache = cache::QueryCache::new();
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
            status::print_provider_info(&cfg, name.as_deref()).await;
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
                                &auth::default_state_path(),
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
                                &auth::default_state_path(),
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
            let cfg = config::Config::load(config_path)?;
            let removed = auth::logout_provider(&cfg, &auth::default_state_path(), &name)?;
            if removed == 0 {
                println!("No OAuth credential found for provider={name}");
            } else {
                println!("Logged out provider={name}");
            }
            Ok(())
        }
        Some(ProviderCommand::Relogin { name }) => {
            let cfg = config::Config::load(config_path)?;
            let _ = auth::logout_provider(&cfg, &auth::default_state_path(), &name)?;
            auth::login_provider(config_path, &cfg, &auth::default_state_path(), &name).await
        }
        Some(ProviderCommand::Refresh { name }) => {
            let cfg = config::Config::load(config_path)?;
            auth::refresh_provider(&cfg, &auth::default_state_path(), &name).await
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
