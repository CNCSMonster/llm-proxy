use super::types::*;
use crate::{admin_client, cache, config, model, tui};
use anyhow::Result;

pub async fn run_model_command(
    config_path: &std::path::Path,
    select: Option<String>,
    command: Option<ModelCommand>,
) -> Result<()> {
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let dirs = crate::service::StateDirs::resolve(config_dir);
    if let Some(name) = select {
        if command.is_some() {
            anyhow::bail!("model --select cannot be combined with a model subcommand");
        }
        let cfg = config::Config::load(config_path)?;
        return tui::run(config_path, model_select_entry(&cfg, name)?).await;
    }
    match command {
        None => {
            tui::run(
                config_path,
                tui::model::EntryMode::ModelTui { select: None },
            )
            .await
        }
        Some(ModelCommand::List) => {
            // Try server delegation first（读操作走 HTTP 公开接口）
            match admin_client::detect_server(config_path).await {
                Ok(Some(server)) => {
                    crate::ownership::ensure_same_config(&server, config_path)?;
                    match server.model_list().await {
                        Ok(result) => {
                            let _ = cache::QueryCache::new_in(&dirs.state_dir)
                                .save("model-list", &result);
                            model::print_model_list_json(&result)?;
                            return Ok(());
                        }
                        Err(e) => {
                            eprintln!("Server error: {e}");
                            return Err(e);
                        }
                    }
                }
                Ok(None) => {
                    // 远程模式读缓存兜底
                    let is_remote = config::Config::load(config_path)
                        .map(|cfg| cfg.providers.is_empty() && cfg.models.is_empty())
                        .unwrap_or(false);
                    if is_remote {
                        let cache = cache::QueryCache::new_in(&dirs.state_dir);
                        if let Some((cached_at, result)) = cache.load("model-list") {
                            println!(
                                "ℹ 从缓存获取（{}，可能过期）",
                                cache::QueryCache::format_cached_at(cached_at)
                            );
                            model::print_model_list_json(&result)?;
                            return Ok(());
                        }
                        anyhow::bail!("server unreachable and no cached model list");
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    return Err(e);
                }
            }
            // Fallback: local mode
            let cfg = config::Config::load(config_path)?;
            model::list(&cfg);
            Ok(())
        }
        Some(ModelCommand::Info { model: model_id }) => {
            // Try server delegation first（读操作走 HTTP 公开接口）
            match admin_client::detect_server(config_path).await {
                Ok(Some(server)) => {
                    crate::ownership::ensure_same_config(&server, config_path)?;
                    match server.model_info(&model_id).await {
                        Ok(result) => {
                            let _ = cache::QueryCache::new_in(&dirs.state_dir)
                                .save(&format!("model-info:{model_id}"), &result);
                            model::print_model_info_json(&result)?;
                            return Ok(());
                        }
                        Err(e) => {
                            eprintln!("Server error: {e}");
                            return Err(e);
                        }
                    }
                }
                Ok(None) => {
                    let is_remote = config::Config::load(config_path)
                        .map(|cfg| cfg.providers.is_empty() && cfg.models.is_empty())
                        .unwrap_or(false);
                    if is_remote {
                        let cache = cache::QueryCache::new_in(&dirs.state_dir);
                        if let Some((cached_at, result)) =
                            cache.load(&format!("model-info:{model_id}"))
                        {
                            println!(
                                "ℹ 从缓存获取（{}，可能过期）",
                                cache::QueryCache::format_cached_at(cached_at)
                            );
                            model::print_model_info_json(&result)?;
                            return Ok(());
                        }
                        anyhow::bail!("server unreachable and no cached model info for {model_id}");
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    return Err(e);
                }
            }
            // Fallback: local mode
            let cfg = config::Config::load(config_path)?;
            model::info(&cfg, &model_id)
        }
        Some(ModelCommand::Add {
            model: model_id,
            context_window,
            max_output,
            copy_from,
            from_discovery,
            upstream_model,
            route,
        }) => {
            if from_discovery.is_some() {
                // CLI 层提前校验（fail fast，不必等写路径）；gate 内仍兜底。
                model::validate_window_override(context_window, "--context-window")?;
                model::validate_window_override(max_output, "--max-output")?;
                // from-discovery：server 无对应端点，保持本地执行。
                // server 运行（持有所有权锁）时明确报错，要求独立模式。
                if let Some(conn) = admin_client::detect_server(config_path).await? {
                    crate::ownership::ensure_same_config(&conn, config_path)?;
                    anyhow::bail!(
                        "model add --from-discovery requires standalone mode (no server running); \
                         server has no from-discovery endpoint"
                    );
                }
                // 本地发现模式（含网络调用），加所有权锁
                let model_id = model_id.clone();
                let from_discovery = from_discovery.clone();
                let upstream_model = upstream_model.clone();
                let config_path = config_path.to_path_buf();
                let config_path_for_lock = config_path.clone();
                return crate::ownership::with_cli_write_lock_async(
                    &config_path_for_lock,
                    "llm-proxy model add --from-discovery",
                    async move {
                        model::add_from_discovery(
                            &config_path,
                            &model_id,
                            from_discovery.as_deref().expect("checked"),
                            upstream_model.as_deref(),
                            context_window,
                            max_output,
                        )
                    },
                )
                .await;
            }
            if copy_from.is_some() && upstream_model.is_some() {
                anyhow::bail!(
                    "--copy-from cannot be combined with --upstream-model; \
                     configure providers separately after adding"
                );
            }
            // Parse --route LEVEL=TARGET entries
            let routes = model::parse_route_entries(&route)?;
            // CLI 层提前校验（fail fast，不发起 UDS 委托就报错）；gate 内仍兜底。
            model::validate_add_windows(
                context_window,
                max_output,
                copy_from.as_deref(),
                !routes.is_empty(),
            )?;
            // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy model add",
                || {
                    let model_id = model_id.clone();
                    let copy_from = copy_from.clone();
                    let config_path = config_path.to_path_buf();
                    let routes = routes.clone();
                    async move {
                        model::add(
                            &config_path,
                            &model_id,
                            context_window,
                            max_output,
                            copy_from.as_deref(),
                            if routes.is_empty() {
                                None
                            } else {
                                Some(routes)
                            },
                        )
                    }
                },
                |server| {
                    let model_id = model_id.clone();
                    let copy_from = copy_from.clone();
                    let routes = routes.clone();
                    Box::pin(async move {
                        let result = server
                            .model_add(
                                &model_id,
                                context_window,
                                max_output,
                                copy_from.as_deref(),
                                if routes.is_empty() {
                                    None
                                } else {
                                    Some(routes)
                                },
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
        Some(ModelCommand::Set {
            model: model_id,
            context_window,
            max_output,
            thinking_level,
            supported_thinking_levels,
            supports_thinking,
            force_think,
            force_no_think,
            default_think,
            enable_features,
            disable_features,
            description,
            clear_description,
            chat_reasoning_off_format,
            clear_chat_reasoning_off_format,
            service_tier,
            default_tier,
            forced_tier,
            clear_service_tier,
            cooldown_network,
            cooldown_server_error,
            cooldown_rate_limit,
            cooldown_model_unavailable,
            cooldown_client_error,
            cooldown_auth_error,
            clear_cooldown,
        }) => {
            let codex_service_tier =
                service_tier.map(|tier| crate::config::CodexServiceTierConfig {
                    tiers: vec![tier.clone()],
                    default_tier: default_tier.then_some(tier.clone()),
                    forced_tier: forced_tier.then_some(tier),
                });
            let has_cooldown = cooldown_network.is_some()
                || cooldown_server_error.is_some()
                || cooldown_rate_limit.is_some()
                || cooldown_model_unavailable.is_some()
                || cooldown_client_error.is_some()
                || cooldown_auth_error.is_some();
            let cooldown = has_cooldown.then_some(crate::config::CooldownOverrideConfig {
                network_seconds: cooldown_network,
                server_error_seconds: cooldown_server_error,
                rate_limit_seconds: cooldown_rate_limit,
                model_unavailable_seconds: cooldown_model_unavailable,
                client_error_seconds: cooldown_client_error,
                auth_error_seconds: cooldown_auth_error,
            });
            let reasoning_levels = (!supported_thinking_levels.is_empty()).then_some(
                crate::model::ReasoningLevelsUpdate::Names(supported_thinking_levels),
            );
            let options = model::SetModelOptions {
                context_window,
                max_output_tokens: max_output,
                reasoning_levels,
                thinking_level,
                supports_thinking,
                force_think,
                force_no_think,
                default_think,
                enable_features,
                disable_features,
                description,
                clear_description,
                chat_reasoning_off_format,
                clear_chat_reasoning_off_format,
                codex_service_tier,
                clear_codex_service_tier: clear_service_tier,
                cooldown,
                clear_cooldown,
            };
            // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy model set",
                || {
                    let model_id = model_id.clone();
                    let options = options.clone();
                    let config_path = config_path.to_path_buf();
                    async move { model::set(&config_path, &model_id, options) }
                },
                |server| {
                    let model_id = model_id.clone();
                    let options = options.clone();
                    Box::pin(async move {
                        let result = server.model_set(&model_id, &options).await?;
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
        Some(ModelCommand::Rename {
            old_model,
            new_model,
        }) => {
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy model rename",
                || {
                    let old_model = old_model.clone();
                    let new_model = new_model.clone();
                    let config_path = config_path.to_path_buf();
                    async move { model::rename(&config_path, &old_model, &new_model) }
                },
                |server| {
                    let old_model = old_model.clone();
                    let new_model = new_model.clone();
                    Box::pin(async move {
                        let result = server.model_rename(&old_model, &new_model).await?;
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
        Some(ModelCommand::Remove {
            model: model_id,
            force,
        }) => {
            // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy model remove",
                || {
                    let model_id = model_id.clone();
                    let config_path = config_path.to_path_buf();
                    async move { model::remove(&config_path, &model_id, force) }
                },
                |server| {
                    let model_id = model_id.clone();
                    Box::pin(async move {
                        let result = server.model_remove(&model_id, force).await?;
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
        Some(ModelCommand::Provider { command }) => match command {
            ModelProviderCommand::Add {
                model: model_id,
                provider_type,
                provider,
                upstream_model,
            } => {
                // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
                crate::ownership::with_cli_write_lock_or_delegate(
                    config_path,
                    "llm-proxy model provider add",
                    || {
                        let model_id = model_id.clone();
                        let provider_type = provider_type.clone();
                        let provider = provider.clone();
                        let upstream_model = upstream_model.clone();
                        let config_path = config_path.to_path_buf();
                        async move {
                            let protocol = model::parse_client_protocol(&provider_type)?;
                            model::provider_add(
                                &config_path,
                                &model_id,
                                protocol,
                                &provider,
                                upstream_model,
                            )
                        }
                    },
                    |server| {
                        let model_id = model_id.clone();
                        let provider_type = provider_type.clone();
                        let provider = provider.clone();
                        let upstream_model = upstream_model.clone();
                        Box::pin(async move {
                            let result = server
                                .model_provider(
                                    "add",
                                    &model_id,
                                    &provider_type,
                                    &provider,
                                    upstream_model.as_deref(),
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
            ModelProviderCommand::Set {
                model: model_id,
                provider_type,
                provider,
                upstream_model,
            } => {
                // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
                crate::ownership::with_cli_write_lock_or_delegate(
                    config_path,
                    "llm-proxy model provider set",
                    || {
                        let model_id = model_id.clone();
                        let provider_type = provider_type.clone();
                        let provider = provider.clone();
                        let upstream_model = upstream_model.clone();
                        let config_path = config_path.to_path_buf();
                        async move {
                            let protocol = model::parse_client_protocol(&provider_type)?;
                            model::provider_set(
                                &config_path,
                                &model_id,
                                protocol,
                                &provider,
                                &upstream_model,
                            )
                        }
                    },
                    |server| {
                        let model_id = model_id.clone();
                        let provider_type = provider_type.clone();
                        let provider = provider.clone();
                        let upstream_model = upstream_model.clone();
                        Box::pin(async move {
                            let result = server
                                .model_provider(
                                    "set",
                                    &model_id,
                                    &provider_type,
                                    &provider,
                                    Some(&upstream_model),
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
            ModelProviderCommand::Remove {
                model: model_id,
                provider_type,
                provider,
            } => {
                // §15.2 5 步流程
                crate::ownership::with_cli_write_lock_or_delegate(
                    config_path,
                    "llm-proxy model provider remove",
                    || {
                        let model_id = model_id.clone();
                        let provider_type = provider_type.clone();
                        let provider = provider.clone();
                        let config_path = config_path.to_path_buf();
                        async move {
                            let protocol = model::parse_client_protocol(&provider_type)?;
                            model::provider_remove(&config_path, &model_id, protocol, &provider)
                        }
                    },
                    |server| {
                        let model_id = model_id.clone();
                        let provider_type = provider_type.clone();
                        let provider = provider.clone();
                        Box::pin(async move {
                            let result = server
                                .model_provider(
                                    "remove",
                                    &model_id,
                                    &provider_type,
                                    &provider,
                                    None,
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
            ModelProviderCommand::Move {
                model: model_id,
                provider_type,
                provider,
                to,
            } => {
                // §15.2 5 步流程
                crate::ownership::with_cli_write_lock_or_delegate(
                    config_path,
                    "llm-proxy model provider move",
                    || {
                        let model_id = model_id.clone();
                        let provider_type = provider_type.clone();
                        let provider = provider.clone();
                        let config_path = config_path.to_path_buf();
                        async move {
                            let protocol = model::parse_client_protocol(&provider_type)?;
                            model::provider_move(&config_path, &model_id, protocol, &provider, to)
                        }
                    },
                    |server| {
                        let model_id = model_id.clone();
                        let provider_type = provider_type.clone();
                        let provider = provider.clone();
                        Box::pin(async move {
                            let result = server
                                .model_provider(
                                    "move",
                                    &model_id,
                                    &provider_type,
                                    &provider,
                                    None,
                                    Some(to),
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
        },
    }
}

fn model_select_entry(cfg: &config::Config, name: String) -> Result<tui::model::EntryMode> {
    if !cfg.models.contains_key(&name) {
        anyhow::bail!("unknown model {name:?}");
    }
    Ok(tui::model::EntryMode::ModelTui { select: Some(name) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_select_entry_rejects_unknown_name() {
        let cfg = config::default_deepseek_config();
        let err = model_select_entry(&cfg, "missing".into())
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown model"));
        assert!(err.contains("missing"));
    }

    #[test]
    fn model_select_entry_targets_known_model() {
        let cfg = config::default_deepseek_config();
        let model_id = cfg.models.keys().next().expect("default config has models");
        match model_select_entry(&cfg, model_id.clone()).unwrap() {
            tui::model::EntryMode::ModelTui { select } => {
                assert_eq!(select.as_deref(), Some(model_id.as_str()));
            }
            other => panic!("unexpected entry mode: {other:?}"),
        }
    }
}
