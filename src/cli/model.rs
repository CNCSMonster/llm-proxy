use super::types::*;
use crate::{admin_client, cache, config, model, tui};
use anyhow::Result;

pub async fn run_model_command(
    config_path: &std::path::Path,
    command: Option<ModelCommand>,
) -> Result<()> {
    match command {
        None => tui::run(config_path, tui::model::EntryMode::Connect).await,
        Some(ModelCommand::List) => {
            // Try server delegation first（读操作走 HTTP 公开接口）
            match admin_client::detect_server(config_path).await {
                Ok(Some(server)) => match server.model_list().await {
                    Ok(result) => {
                        let _ = cache::QueryCache::new().save("model-list", &result);
                        model::print_model_list_json(&result)?;
                        return Ok(());
                    }
                    Err(e) => {
                        eprintln!("Server error: {e}");
                        return Err(e);
                    }
                },
                Ok(None) => {
                    // 远程模式读缓存兜底
                    let is_remote = config::Config::load(config_path)
                        .map(|cfg| cfg.providers.is_empty() && cfg.models.is_empty())
                        .unwrap_or(false);
                    if is_remote {
                        let cache = cache::QueryCache::new();
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
                Ok(Some(server)) => match server.model_info(&model_id).await {
                    Ok(result) => {
                        let _ = cache::QueryCache::new()
                            .save(&format!("model-info:{model_id}"), &result);
                        model::print_model_info_json(&result)?;
                        return Ok(());
                    }
                    Err(e) => {
                        eprintln!("Server error: {e}");
                        return Err(e);
                    }
                },
                Ok(None) => {
                    let is_remote = config::Config::load(config_path)
                        .map(|cfg| cfg.providers.is_empty() && cfg.models.is_empty())
                        .unwrap_or(false);
                    if is_remote {
                        let cache = cache::QueryCache::new();
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
        }) => {
            if from_discovery.is_some() {
                // CLI 层提前校验（fail fast，不必等写路径）；gate 内仍兜底。
                model::validate_window_override(context_window, "--context-window")?;
                model::validate_window_override(max_output, "--max-output")?;
                // from-discovery：server 无对应端点，保持本地执行。
                // server 运行（持有所有权锁）时明确报错，要求独立模式。
                if admin_client::detect_server(config_path).await?.is_some() {
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
                return crate::ownership::with_cli_write_lock_async(
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
            // CLI 层提前校验（fail fast，不发起 UDS 委托就报错）；gate 内仍兜底。
            model::validate_add_windows(context_window, max_output, copy_from.as_deref())?;
            // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy model add",
                || {
                    let model_id = model_id.clone();
                    let copy_from = copy_from.clone();
                    let config_path = config_path.to_path_buf();
                    async move {
                        model::add(
                            &config_path,
                            &model_id,
                            context_window,
                            max_output,
                            copy_from.as_deref(),
                        )
                    }
                },
                |server| {
                    let model_id = model_id.clone();
                    let copy_from = copy_from.clone();
                    Box::pin(async move {
                        let result = server
                            .model_add(&model_id, context_window, max_output, copy_from.as_deref())
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
            enable_thinking,
            disable_thinking,
            enable_features,
            disable_features,
        }) => {
            // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy model set",
                || {
                    let model_id = model_id.clone();
                    let supported_thinking_levels = supported_thinking_levels.clone();
                    let thinking_level = thinking_level.clone();
                    let enable_features = enable_features.clone();
                    let disable_features = disable_features.clone();
                    let config_path = config_path.to_path_buf();
                    async move {
                        model::set(
                            &config_path,
                            &model_id,
                            model::SetModelOptions {
                                context_window,
                                max_output_tokens: max_output,
                                supported_reasoning_levels: (!supported_thinking_levels.is_empty())
                                    .then_some(supported_thinking_levels),
                                thinking_level,
                                enable_thinking: enable_thinking
                                    .then_some(true)
                                    .or_else(|| disable_thinking.then_some(false)),
                                enable_features,
                                disable_features,
                            },
                        )
                    }
                },
                |server| {
                    let model_id = model_id.clone();
                    let supported_thinking_levels = supported_thinking_levels.clone();
                    let thinking_level = thinking_level.clone();
                    let enable_features = enable_features.clone();
                    let disable_features = disable_features.clone();
                    Box::pin(async move {
                        let supported = (!supported_thinking_levels.is_empty())
                            .then_some(supported_thinking_levels);
                        let enable = enable_thinking
                            .then_some(true)
                            .or_else(|| disable_thinking.then_some(false));
                        let result = server
                            .model_set(
                                &model_id,
                                context_window,
                                max_output,
                                supported,
                                thinking_level,
                                enable,
                                enable_features,
                                disable_features,
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
