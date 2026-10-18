use crossterm::event::{KeyCode, KeyEvent};

use super::model::{AppModel, FallbackConfigState, FallbackFocus, FallbackSource, Screen};

/// Fallback 主内容区的可视行数：终端高度扣除外边框(2)、帮助栏(1)、标题(2)、副标题(1)、区头(1)、底栏(1)。
fn fallback_viewport(app_height: u16) -> usize {
    app_height.saturating_sub(8) as usize
}

/// 按键移动 / Tab 切换焦点 / 终端缩放后，重对齐主内容区滚动窗口。
fn sync_fallback_scroll(state: &mut FallbackConfigState, app_height: u16) {
    let cursor_line = state.active_cursor_line();
    super::model::follow_scroll(
        &mut state.scroll_offset,
        cursor_line,
        fallback_viewport(app_height),
    );
}

/// Handle keyboard input on the FallbackConfig screen.
pub(crate) fn handle_fallback_keys(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        // Tab switches focus between target provider list and options list
        KeyCode::Tab => {
            if let Screen::FallbackConfig(ref mut s) = app.screen {
                s.focus = match s.focus {
                    FallbackFocus::TargetProvider => FallbackFocus::Options,
                    FallbackFocus::Options => FallbackFocus::TargetProvider,
                };
                sync_fallback_scroll(s, app.height);
            }
        }

        // Enter confirms the fallback configuration
        KeyCode::Enter => {
            confirm_fallback_config(app);
        }

        // Space toggles the current option (only when options have focus)
        KeyCode::Char(' ') => {
            if let Screen::FallbackConfig(ref s) = app.screen
                && s.focus == FallbackFocus::Options
                && let Screen::FallbackConfig(ref mut s) = app.screen
            {
                s.toggle_option();
            }
        }

        // 'M' key switches to model selection screen
        KeyCode::Char('M') | KeyCode::Char('m') => {
            switch_to_model_selection(app);
        }

        // Navigation
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::FallbackConfig(ref mut s) = app.screen {
                s.move_up();
                sync_fallback_scroll(s, app.height);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::FallbackConfig(ref mut s) = app.screen {
                s.move_down();
                sync_fallback_scroll(s, app.height);
            }
        }

        _ => {}
    }
}

/// Confirm fallback config and execute the batch fallback or proceed to Verifying.
fn confirm_fallback_config(app: &mut AppModel) {
    // Extract source before mutating app
    let source = match &app.screen {
        Screen::FallbackConfig(s) => s.source.clone(),
        _ => return,
    };

    match source {
        FallbackSource::ConnectFlow => {
            // Connect flow: check if user selected anything
            let has_selection = if let Screen::FallbackConfig(ref s) = app.screen {
                !s.options.is_empty() && s.options.iter().any(|opt| opt.selected)
            } else {
                false
            };

            if !has_selection {
                // Zero selection = skip fallback config, proceed to Verifying
                let product_name = app
                    .chosen_product
                    .as_ref()
                    .map(|p| p.display_name.clone())
                    .unwrap_or_default();
                let env_var = app.chosen_env_var.clone();
                app.screen = Screen::Verifying(super::model::VerifyingState {
                    product_name,
                    env_var,
                    models: vec![],
                    skip_probe: false,
                    progress_done: 0,
                    progress_total: 0,
                    progress_current_model: None,
                });
                return;
            }

            // Proceed to Verifying with selected model IDs and endpoint filter
            let product_name = app
                .chosen_product
                .as_ref()
                .map(|p| p.display_name.clone())
                .unwrap_or_default();
            let env_var = app.chosen_env_var.clone();

            let selected_options: Vec<(&str, &str)> =
                if let Screen::FallbackConfig(ref s) = app.screen {
                    s.options
                        .iter()
                        .filter(|opt| opt.selected)
                        .map(|opt| (opt.model_id.as_str(), opt.endpoint.as_str()))
                        .collect()
                } else {
                    vec![]
                };

            // Build endpoint filter: model_id → set of protocol labels
            let mut filter_map: std::collections::HashMap<
                String,
                std::collections::HashSet<String>,
            > = std::collections::HashMap::new();
            let mut model_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (model_id, endpoint) in &selected_options {
                filter_map
                    .entry(model_id.to_string())
                    .or_default()
                    .insert(endpoint.to_string());
                model_ids.insert(model_id.to_string());
            }
            let selected_models: Vec<String> = model_ids.into_iter().collect();
            app.endpoint_filter = if filter_map.is_empty() {
                None
            } else {
                Some(filter_map)
            };
            app.chosen_models = selected_models.clone();

            // Store fallback target and binding specs for post-creation chain insertion
            let (fallback_target, fallback_bindings) =
                if let Screen::FallbackConfig(ref s) = app.screen {
                    let target = s.selected_target().map(|t| t.to_string());
                    let specs: Vec<String> = s
                        .selected_options()
                        .iter()
                        .map(|opt| format!("{}:{}", opt.model_id, opt.endpoint))
                        .collect();
                    (target, specs)
                } else {
                    (None, vec![])
                };
            app.fallback_target = fallback_target;
            app.fallback_bindings = fallback_bindings;

            app.screen = Screen::Verifying(super::model::VerifyingState {
                product_name,
                env_var,
                models: selected_models,
                skip_probe: false,
                progress_done: 0,
                progress_total: 0,
                progress_current_model: None,
            });
        }
        FallbackSource::ProviderManagement { ref provider_name } => {
            // Provider management: call batch fallback logic
            let (target, binding_specs, options_empty) = match &app.screen {
                Screen::FallbackConfig(s) => {
                    let target = s.selected_target().map(|t| t.to_string());
                    let specs: Vec<String> = s
                        .selected_options()
                        .iter()
                        .map(|opt| format!("{}:{}", opt.model_id, opt.endpoint))
                        .collect();
                    (target, specs, s.options.is_empty())
                }
                _ => return,
            };

            let provider_name = provider_name.clone();

            // No options available at all → skip silently (like Esc), return to provider list
            if options_empty {
                let name = provider_name.clone();
                super::provider_mgmt::return_to_provider_management_preserving_cursor(
                    app,
                    Some(&name),
                );
                return;
            }

            // Validate: need a target and at least one selection
            let Some(target_name) = target else {
                if let Screen::FallbackConfig(ref mut s) = app.screen {
                    if s.target_providers.is_empty() {
                        s.error = Some("没有同产品的其他 Provider 可作为目标".to_string());
                    } else {
                        s.error = Some("请先选择一个目标 Provider".to_string());
                    }
                }
                return;
            };

            if binding_specs.is_empty() {
                if let Screen::FallbackConfig(ref mut s) = app.screen {
                    s.error = Some("请至少选择一个 (model, endpoint) 组合".to_string());
                }
                return;
            }

            // Execute batch fallback
            let config_path = app.config_path.clone();
            let result = crate::tui::run_async_sync(async {
                crate::ownership::with_cli_write_lock_or_delegate(
                    &config_path,
                    "llm-proxy tui fallback add",
                    // Local write
                    || {
                        let config_path = config_path.clone();
                        let provider_name = provider_name.clone();
                        let target_name = target_name.clone();
                        let binding_specs = binding_specs.clone();
                        async move {
                            crate::fallback::add_fallback(
                                &config_path,
                                &provider_name,
                                &target_name,
                                &binding_specs,
                            )
                        }
                    },
                    // Delegate: server running, send via UDS
                    |server| {
                        // For TUI fallback, we use the local write path since
                        // the admin protocol doesn't yet have a fallback RPC.
                        // Fall through to local write.
                        let config_path = config_path.clone();
                        let provider_name = provider_name.clone();
                        let target_name = target_name.clone();
                        let binding_specs = binding_specs.clone();
                        Box::pin(async move {
                            let _ = server; // unused for now
                            crate::fallback::add_fallback(
                                &config_path,
                                &provider_name,
                                &target_name,
                                &binding_specs,
                            )
                        })
                    },
                )
                .await
            });

            match result {
                Ok(results) => {
                    // Build summary for display
                    let inserted = results
                        .iter()
                        .filter(|r| {
                            matches!(r, crate::fallback::FallbackBindingResult::Inserted { .. })
                        })
                        .count();
                    let skipped = results
                        .iter()
                        .filter(|r| {
                            matches!(
                                r,
                                crate::fallback::FallbackBindingResult::SkippedAlreadyInChain { .. }
                                    | crate::fallback::FallbackBindingResult::SkippedRegexNoMatch { .. }
                            )
                        })
                        .count();
                    let failed = results
                        .iter()
                        .filter(|r| {
                            matches!(r, crate::fallback::FallbackBindingResult::Failed { .. })
                        })
                        .count();

                    // Store results for display, then return to provider management
                    let _summary = format!(
                        "Fallback 完成: {} inserted, {} skipped, {} failed",
                        inserted, skipped, failed
                    );

                    // Return to provider management, preserving cursor on the fallback provider
                    super::provider_mgmt::return_to_provider_management_preserving_cursor(
                        app,
                        Some(&provider_name),
                    );
                }
                Err(e) => {
                    // Show error on the fallback config screen
                    if let Screen::FallbackConfig(ref mut s) = app.screen {
                        s.error = Some(format!("执行失败: {}", e));
                    }
                }
            }
        }
    }
}

/// Switch from fallback config to model selection screen.
fn switch_to_model_selection(app: &mut AppModel) {
    // Try to get product_id from chosen_product or from the current provider's product field
    let product_id = app
        .chosen_product
        .as_ref()
        .map(|p| p.id.clone())
        .or_else(|| {
            // If chosen_product is not set (e.g., from ProviderManagement), infer from provider
            if let Screen::FallbackConfig(ref s) = app.screen {
                s.target_providers.first().and_then(|name| {
                    crate::config::Config::load(&app.config_path)
                        .ok()
                        .and_then(|cfg| cfg.providers.get(name).map(|p| p.product.clone()))
                })
            } else {
                None
            }
        });
    let product_name = app
        .chosen_product
        .as_ref()
        .map(|p| p.display_name.clone())
        .unwrap_or_default();

    let models = super::models::get_model_templates(product_id.as_deref());
    if models.is_empty() {
        // No model templates available, stay on fallback or go to verifying
        if let Screen::FallbackConfig(ref mut s) = app.screen {
            s.error = Some("该产品暂无可选模型模板".to_string());
        }
        return;
    }

    let configured = product_id
        .as_deref()
        .map(|pid| super::model::build_configured_set(&app.config_path, pid, &models))
        .unwrap_or_default();

    // Restore cached selected/cursor/filter if available (survives round-trip).
    let (selected, cursor, filter, filter_active) =
        if let Some(cached) = app.cached_model_selection.take() {
            (
                cached.selected,
                cached.cursor,
                cached.filter,
                cached.filter_active,
            )
        } else {
            (std::collections::HashSet::new(), 0, String::new(), false)
        };

    app.screen = Screen::ModelSelection(super::model::ModelSelectionState {
        items: models,
        cursor,
        filter,
        filter_active,
        selected,
        configured,
        error: None,
        product_name,
        source: super::model::ModelSelectionSource::Connect,
        is_openrouter: false,
        is_degraded: false,
        openrouter_cached_at: None,
    });
}
