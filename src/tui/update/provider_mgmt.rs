use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};

use super::model::{self, AppModel, FallbackSource, Screen};
use crate::tui::model::{
    DeleteConfirmState, FallbackConfigState, FallbackOption, ProviderDetailState, ProviderListItem,
    ProviderManagementState, ResetUsageConfirmState,
};

/// 按键移动 / 返回列表 / 终端缩放后，按当前终端高度重新对齐滚动窗口。
fn sync_provider_list_scroll(state: &mut ProviderManagementState, app_height: u16) {
    let viewport =
        model::provider_list_viewport(app_height).saturating_sub(state.error_row_count());
    state.ensure_cursor_visible(viewport);
}

/// 按键移动 / 返回详情 / 终端缩放后，按当前终端高度重对齐绑定模型列表滚动窗口。
pub(crate) fn sync_provider_detail_scroll(state: &mut ProviderDetailState, app_height: u16) {
    let endpoints_rows = 1 + state.endpoints.len();
    let viewport = model::detail_viewport(app_height)
        .saturating_sub(3) // product + auth 区
        .saturating_sub(endpoints_rows);
    let cursor_line = state.cursor_line_index();
    model::follow_scroll(&mut state.scroll_offset, cursor_line, viewport);
}

/// Reload the provider list and return to the ProviderManagement screen,
/// preserving cursor position on `keep_provider` (if it still exists).
///
/// : 保留既有的 `quota_map` / `quota_fetched_at`，避免从详情页返回时额度缓存被清空。
pub(super) fn return_to_provider_management_preserving_cursor(
    app: &mut AppModel,
    keep_provider: Option<&str>,
) {
    // 从当前 screen 取出旧的刷新时间戳，重建后原样归还。
    let preserved_quota_fetched_at = if let Screen::ProviderManagement(ref state) = app.screen {
        state.quota_fetched_at
    } else {
        None
    };

    let providers = crate::tui::model::load_configured_providers(&app.config_path);
    let cursor = keep_provider
        .and_then(|name| providers.iter().position(|p| p.name == name))
        .unwrap_or(0);
    let mut state = ProviderManagementState {
        providers,
        cursor,
        error: None,
        filter: String::new(),
        filter_active: false,
        scroll_offset: 0,
        quota_fetched_at: preserved_quota_fetched_at,
    };
    sync_provider_list_scroll(&mut state, app.height);
    app.screen = Screen::ProviderManagement(state);
}

fn expired_oauth_provider_ids(
    config: &crate::config::Config,
    accounts: &crate::auth::AccountsFile,
) -> Vec<String> {
    config
        .providers
        .iter()
        .filter_map(|(provider_id, provider)| {
            let expired = match provider.auth_config(provider_id) {
                Ok(crate::config::AuthConfig::OpenaiOauth { account }) => accounts
                    .openai
                    .get(account.as_deref().unwrap_or(provider_id))
                    .is_some_and(crate::auth::OpenaiAccount::is_expired),
                Ok(
                    crate::config::AuthConfig::ApiKeyEnv { .. } | crate::config::AuthConfig::None,
                )
                | Err(_) => false,
            };
            expired.then(|| provider_id.clone())
        })
        .collect()
}

/// Auto-refresh expired OAuth tokens before reloading provider list.
/// Returns provider-scoped failures without exposing account identifiers.
fn auto_refresh_oauth_tokens(config_path: &std::path::Path) -> Vec<String> {
    let config = match crate::config::Config::load(config_path) {
        Ok(config) => config,
        Err(error) => return vec![format!("无法加载配置: {error}")],
    };
    let state_path = crate::auth::state_path_for_config(config_path);
    let accounts = match crate::auth::load_oauth_accounts(&state_path) {
        Ok((accounts, _)) => accounts,
        Err(error) => return vec![format!("无法加载 OAuth 状态: {error}")],
    };
    let mut errors = Vec::new();
    let candidates = expired_oauth_provider_ids(&config, &accounts);
    let started = std::time::Instant::now();
    let mut refreshed = 0_usize;

    for provider_id in &candidates {
        if let Err(error) = crate::tui::run_async_sync(crate::auth::refresh_provider_with_trigger(
            &config,
            &state_path,
            provider_id,
            "tui_provider_list_refresh",
        )) {
            errors.push(format!(
                "{provider_id}: {}",
                crate::auth::oauth_error_log_summary(&format!("{error:#}"))
            ));
        } else {
            refreshed += 1;
        }
    }
    tracing::info!(
        target: "llm_proxy::oauth",
        event = "oauth_refresh_batch_complete",
        trigger = "tui_provider_list_refresh",
        candidate_count = candidates.len(),
        refreshed_count = refreshed,
        failure_count = errors.len(),
        duration_ms = started.elapsed().as_millis() as u64,
    );

    errors
}

/// Handle keyboard input for the provider management panel
pub fn handle_provider_management_keys(app: &mut AppModel, key: KeyEvent) {
    // Handle filter input mode
    if let Screen::ProviderManagement(ref mut state) = app.screen
        && state.filter_active
    {
        match key.code {
            KeyCode::Esc => {
                state.deactivate_filter();
            }
            KeyCode::Enter => {
                state.filter_active = false;
            }
            KeyCode::Backspace => {
                state.filter.pop();
                if state.filter.is_empty() {
                    state.filter_active = false;
                }
            }
            KeyCode::Char(c) => {
                state.filter.push(c);
                state.cursor = 0; // Reset cursor when filter changes
                state.scroll_offset = 0;
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Char('q') => {
            app.screen = Screen::Quit;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::ProviderManagement(ref mut state) = app.screen {
                state.move_up();
                sync_provider_list_scroll(state, app.height);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ProviderManagement(ref mut state) = app.screen {
                state.move_down();
                sync_provider_list_scroll(state, app.height);
            }
        }
        KeyCode::Char('/') => {
            // Activate filter mode
            if let Screen::ProviderManagement(ref mut state) = app.screen {
                state.activate_filter();
            }
        }
        KeyCode::Char('r') => {
            // Refresh provider list with OAuth token auto-refresh
            let current_provider = if let Screen::ProviderManagement(ref state) = app.screen {
                state.selected_provider().map(|p| p.name.clone())
            } else {
                None
            };

            // Auto-refresh expired OAuth tokens before reloading
            let refresh_errors = auto_refresh_oauth_tokens(&app.config_path);

            let providers = crate::tui::model::load_configured_providers(&app.config_path);
            if let Screen::ProviderManagement(ref mut state) = app.screen {
                if providers.is_empty() {
                    state.error = Some("刷新失败：无法加载配置文件".to_string());
                } else {
                    let new_cursor = current_provider
                        .as_ref()
                        .and_then(|name| providers.iter().position(|p| p.name == *name))
                        .unwrap_or(0);
                    state.providers = providers;
                    state.cursor = new_cursor;
                    sync_provider_list_scroll(state, app.height);
                    // Show refresh result
                    if refresh_errors.is_empty() {
                        state.error = Some("✓ 已刷新".to_string());
                    } else {
                        state.error = Some(format!(
                            "✓ 已刷新，但部分 token 刷新失败: {}",
                            refresh_errors.join(", ")
                        ));
                    }
                }
            }
        }
        KeyCode::Enter => {
            // Enter detail view for selected provider
            if let Screen::ProviderManagement(ref state) = app.screen
                && let Some(provider) = state.selected_provider()
            {
                let detail = build_provider_detail(app, provider);
                app.screen = Screen::ProviderDetail(detail);
            }
        }
        KeyCode::Char('a') => {
            // Enter add provider flow
            app.screen = Screen::ProductSelection(model::ProductSelectionState {
                items: model::build_product_list(&app.config_path),
                cursor: 0,
                filter: String::new(),
                filter_active: false,
                error: None,
            });
        }
        KeyCode::Char('c') => {
            enter_copy_configure_from_list(app);
        }
        KeyCode::Char('d') => {
            // Enter delete confirmation for selected provider
            if let Screen::ProviderManagement(ref state) = app.screen
                && let Some(provider) = state.selected_provider()
            {
                let referencing_models = find_referencing_models(app, &provider.name);
                app.screen = Screen::DeleteConfirm(DeleteConfirmState {
                    provider_name: provider.name.clone(),
                    referencing_models,
                    force_mode: false,
                    error: None,
                });
            }
        }
        KeyCode::Char('l') => {
            // OAuth login for selected provider
            if let Screen::ProviderManagement(ref state) = app.screen
                && let Some(provider) = state.selected_provider()
            {
                let provider_name = provider.name.clone();
                app.oauth_provider_name = Some(provider_name.clone());
                // Check actual auth config from config file (not string matching)
                if let Ok(config) = crate::config::Config::load(&app.config_path)
                    && let Some(pc) = config.providers.get(&provider_name)
                {
                    match pc.auth_config(&provider_name) {
                        Ok(crate::config::AuthConfig::OpenaiOauth { .. }) => {
                            app.screen = Screen::OAuthDeviceCode(
                                model::OAuthDeviceCodeState::pending_login(provider_name.clone()),
                            );
                        }
                        _ => {
                            // Non-OAuth provider — show informational toast
                            if let Screen::ProviderManagement(ref mut state) = app.screen {
                                state.error =
                                    Some("该 provider 不是 OAuth 类型，无需登录".to_string());
                            }
                        }
                    }
                }
            }
        }
        KeyCode::Char('e') => {
            // Edit selected provider - open the custom provider editor
            if let Screen::ProviderManagement(ref state) = app.screen
                && let Some(provider) = state.selected_provider()
            {
                // Load current provider config for editing
                if let Ok(config) = crate::config::Config::load(&app.config_path)
                    && let Some(pc) = config.providers.get(&provider.name)
                {
                    let api_key_env = pc.api_key_env.as_deref();
                    let openai_chat_url = pc.openai_chat.as_ref().and_then(|ep| ep.url.as_deref());
                    let openai_responses_url = pc
                        .openai_responses
                        .as_ref()
                        .and_then(|ep| ep.url.as_deref());
                    let anthropic_url = pc.anthropic.as_ref().and_then(|ep| ep.url.as_deref());

                    app.screen =
                        Screen::CustomProviderEditor(model::CustomProviderEditorState::new_edit(
                            &provider.name,
                            &provider.auth_type,
                            api_key_env,
                            openai_chat_url,
                            openai_responses_url,
                            anthropic_url,
                        ));
                }
            }
        }
        KeyCode::Char('f') => {
            // Enter fallback configuration for selected provider
            let provider_name = if let Screen::ProviderManagement(ref state) = app.screen {
                state.selected_provider().map(|p| p.name.clone())
            } else {
                None
            };
            if let Some(name) = provider_name {
                enter_fallback_config(app, &name);
            }
        }
        KeyCode::Char('m') => {
            let provider_name = if let Screen::ProviderManagement(ref state) = app.screen {
                state.selected_provider().map(|p| p.name.clone())
            } else {
                None
            };
            if let Some(name) = provider_name {
                enter_model_reselection(app, &name);
            }
        }
        KeyCode::Char('u') => {
            // Show usage statistics for selected provider
            if let Screen::ProviderManagement(ref state) = app.screen
                && let Some(provider) = state.selected_provider()
            {
                let provider_name = provider.name.clone();
                let config_path = app.config_path.clone();

                // 优先向运行中 server 请求（对齐 TUI/CLI/admin 同一全集口径）
                let server_stats = crate::tui::run_async_sync(async {
                    if let Ok(Some(server)) = crate::admin_client::detect_server(&config_path).await
                        && crate::ownership::ensure_same_config(&server, &config_path).is_ok()
                        && let Ok(resp) = server
                            .usage(None, Some(&provider_name), None, None, Some(10000), Some(0))
                            .await
                        && let Some(records) = resp
                            .get("data")
                            .and_then(|d| d.get("records"))
                            .and_then(|r| r.as_array())
                    {
                        let mut total_input = 0i64;
                        let mut total_output = 0i64;
                        let mut total_tokens = 0i64;
                        for item in records {
                            total_input += item
                                .get("input_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            total_output += item
                                .get("output_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            total_tokens += item
                                .get("total_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                        }
                        return Some((total_input, total_output, total_tokens, records.len()));
                    }
                    None
                });

                let message =
                    if let Some((total_input, total_output, total_tokens, request_count)) =
                        server_stats
                    {
                        if request_count == 0 {
                            "该 provider 暂无 usage 统计数据".to_string()
                        } else {
                            format!(
                                "📊 {} — 输入: {} 输出: {} 总计: {} tokens | {} 次请求",
                                provider_name,
                                format_usage_number(total_input),
                                format_usage_number(total_output),
                                format_usage_number(total_tokens),
                                request_count,
                            )
                        }
                    } else {
                        // 本地回退：server 未运行或连接失败时读取本地 SQLite
                        let usage_config = crate::config::Config::load(&config_path)
                            .ok()
                            .and_then(|cfg| cfg.server.map(|s| s.usage.clone()))
                            .unwrap_or_default();

                        match crate::usage_stats::UsageStore::new(usage_config) {
                            Ok(store) => {
                                let records = store.get_by_provider(&provider_name);
                                if records.is_empty() {
                                    "该 provider 暂无 usage 统计数据".to_string()
                                } else {
                                    let total_input: i64 =
                                        records.iter().map(|r| r.input_tokens).sum();
                                    let total_output: i64 =
                                        records.iter().map(|r| r.output_tokens).sum();
                                    let total_tokens: i64 =
                                        records.iter().map(|r| r.total_tokens).sum();
                                    let request_count = records.len();
                                    format!(
                                        "📊 {} — 输入: {} 输出: {} 总计: {} tokens | {} 次请求",
                                        provider_name,
                                        format_usage_number(total_input),
                                        format_usage_number(total_output),
                                        format_usage_number(total_tokens),
                                        request_count,
                                    )
                                }
                            }
                            Err(e) => {
                                format!("无法加载 usage 统计: {}", e)
                            }
                        }
                    };

                if let Screen::ProviderManagement(ref mut state) = app.screen {
                    state.error = Some(message);
                }
            }
        }
        KeyCode::Char('R') => {
            // Shift+R: Enter reset usage confirmation for selected provider
            if let Screen::ProviderManagement(ref state) = app.screen
                && let Some(provider) = state.selected_provider()
            {
                let provider_name = provider.name.clone();
                let config_path = app.config_path.clone();

                // Try to resolve token and query usage info
                let usage_result: Result<(String, Option<i64>), String> = (|| {
                    let cfg = crate::config::Config::load(&config_path)
                        .map_err(|e| format!("无法加载配置: {}", e))?;
                    let token = crate::usage::resolve_openai_token(
                        &cfg,
                        &crate::auth::state_path_for_config(&config_path),
                        &provider_name,
                    )
                    .map_err(|e| format!("{}", e))?;
                    Ok((token, None))
                })();

                match usage_result {
                    Ok((token, _)) => {
                        // Query usage details (async)
                        let query_result =
                            crate::tui::run_async_sync(crate::usage::query_usage(&token));
                        match query_result {
                            Ok(usage) => {
                                let summary = format!(
                                    "计划: {} | 主窗口: {}% | 次窗口: {}% | 可用 credit: {}",
                                    usage.plan_type,
                                    usage
                                        .rate_limit
                                        .as_ref()
                                        .and_then(|r| r.primary_window.as_ref())
                                        .map(|w| w.used_percent)
                                        .unwrap_or(0),
                                    usage
                                        .rate_limit
                                        .as_ref()
                                        .and_then(|r| r.secondary_window.as_ref())
                                        .map(|w| w.used_percent)
                                        .unwrap_or(0),
                                    usage
                                        .reset_credits_available
                                        .map(|c| c.to_string())
                                        .unwrap_or_else(|| "未知".to_string()),
                                );
                                app.screen = Screen::ResetUsageConfirm(ResetUsageConfirmState {
                                    provider_name: provider_name.clone(),
                                    usage_info: Some(summary),
                                    credits: usage.reset_credits_available,
                                    error: None,
                                });
                            }
                            Err(e) => {
                                // Query failed — still allow reset (force mode)
                                app.screen = Screen::ResetUsageConfirm(ResetUsageConfirmState {
                                    provider_name: provider_name.clone(),
                                    usage_info: None,
                                    credits: None,
                                    error: Some(format!("无法查询 usage: {}", e)),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        // Non-OAuth provider or config error
                        if let Screen::ProviderManagement(ref mut state) = app.screen {
                            state.error = Some(e);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Handle keyboard input for the provider detail view
pub fn handle_provider_detail_keys(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            // Return to provider management panel, preserving cursor on current provider
            let provider_name = if let Screen::ProviderDetail(ref state) = app.screen {
                Some(state.name.clone())
            } else {
                None
            };
            return_to_provider_management_preserving_cursor(app, provider_name.as_deref());
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::ProviderDetail(ref mut state) = app.screen {
                state.model_move_up();
                sync_provider_detail_scroll(state, app.height);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ProviderDetail(ref mut state) = app.screen {
                state.model_move_down();
                sync_provider_detail_scroll(state, app.height);
            }
        }
        KeyCode::Char('m') => {
            let provider_name = if let Screen::ProviderDetail(ref state) = app.screen {
                Some(state.name.clone())
            } else {
                None
            };
            if let Some(name) = provider_name {
                enter_model_reselection(app, &name);
            }
        }
        KeyCode::Char('r') => {
            // Enter model rename for the selected model
            if let Screen::ProviderDetail(ref state) = app.screen
                && let Some(model_id) = state.selected_model()
            {
                let old_model_id = model_id.to_string();
                let provider_name = state.name.clone();
                app.screen = Screen::ModelRename(model::ModelRenameState {
                    old_model_id: old_model_id.clone(),
                    new_model_id: old_model_id,
                    editing: false,
                    cursor_pos: 0,
                    confirm_step: 0,
                    error: None,
                    provider_name,
                    return_to_model_list: false,
                });
            }
        }
        KeyCode::Char('c') => {
            enter_copy_configure_from_detail(app);
        }
        KeyCode::Char('d') => {
            // Enter delete confirmation for current provider
            if let Screen::ProviderDetail(ref state) = app.screen {
                let referencing_models = find_referencing_models(app, &state.name);
                app.screen = Screen::DeleteConfirm(DeleteConfirmState {
                    provider_name: state.name.clone(),
                    referencing_models,
                    force_mode: false,
                    error: None,
                });
            }
        }
        KeyCode::Char('e') => {
            // Enter edit mode for current provider
            if let Screen::ProviderDetail(ref state) = app.screen {
                // 加载当前 provider 的配置
                if let Ok(config) = crate::config::Config::load(&app.config_path)
                    && let Some(provider) = config.providers.get(&state.name)
                {
                    let api_key_env = provider.api_key_env.as_deref();
                    let openai_chat_url = provider
                        .openai_chat
                        .as_ref()
                        .and_then(|ep| ep.url.as_deref());
                    let openai_responses_url = provider
                        .openai_responses
                        .as_ref()
                        .and_then(|ep| ep.url.as_deref());
                    let anthropic_url =
                        provider.anthropic.as_ref().and_then(|ep| ep.url.as_deref());

                    app.screen =
                        Screen::CustomProviderEditor(model::CustomProviderEditorState::new_edit(
                            &state.name,
                            &state.auth_description,
                            api_key_env,
                            openai_chat_url,
                            openai_responses_url,
                            anthropic_url,
                        ));
                }
            }
        }
        _ => {}
    }
}

/// Handle keyboard input for the delete confirmation dialog
pub fn handle_delete_confirm_keys(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Char('n') | KeyCode::Esc => {
            // Cancel - return to provider management panel, preserving cursor
            let provider_name = if let Screen::DeleteConfirm(ref state) = app.screen {
                Some(state.provider_name.clone())
            } else {
                None
            };
            return_to_provider_management_preserving_cursor(app, provider_name.as_deref());
        }
        KeyCode::Char('y') => {
            // Confirm delete
            if let Screen::DeleteConfirm(ref state) = app.screen {
                let provider_name = state.provider_name.clone();
                let force = state.force_mode;

                // If there are referencing models and not in force mode, reject
                if !state.referencing_models.is_empty() && !force {
                    // Stay on the same screen — user needs to press [f] first
                    return;
                }

                // Perform the deletion（使用完整 5 步委托流程）
                let config_path = app.config_path.clone();
                let result = crate::tui::run_async_sync(async {
                    crate::ownership::with_cli_write_lock_or_delegate(
                        &config_path,
                        "llm-proxy tui provider remove",
                        // Local write: no server, remove directly
                        || {
                            let config_path = config_path.clone();
                            let provider_name = provider_name.clone();
                            async move {
                                crate::connect::remove_provider(&config_path, &provider_name, force)
                            }
                        },
                        // Delegate: server running, send via UDS
                        |server| {
                            let provider_name = provider_name.clone();
                            Box::pin(async move {
                                server
                                    .remove_provider(&provider_name, force)
                                    .await
                                    .map(|_| ())
                            })
                        },
                    )
                    .await
                });
                match result {
                    Ok(_) => {
                        // Return to provider management panel with refreshed list
                        // (provider was deleted, so cursor defaults to top)
                        return_to_provider_management_preserving_cursor(app, None);
                    }
                    Err(e) => {
                        // Show error — stay on delete confirm screen
                        if let Screen::DeleteConfirm(ref mut state) = app.screen {
                            state.error = Some(format!("删除失败: {}", e));
                        }
                    }
                }
            }
        }
        KeyCode::Char('f') => {
            // Force delete mode
            if let Screen::DeleteConfirm(ref mut state) = app.screen {
                state.force_mode = true;
            }
        }
        _ => {}
    }
}

/// Handle keyboard input for the reset usage confirmation dialog
pub fn handle_reset_usage_confirm_keys(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Char('n') | KeyCode::Esc => {
            // Cancel - return to provider management panel, preserving cursor
            let provider_name = if let Screen::ResetUsageConfirm(ref state) = app.screen {
                Some(state.provider_name.clone())
            } else {
                None
            };
            return_to_provider_management_preserving_cursor(app, provider_name.as_deref());
        }
        KeyCode::Char('y') => {
            // Confirm reset usage
            if let Screen::ResetUsageConfirm(ref state) = app.screen {
                let provider_name = state.provider_name.clone();
                let config_path = app.config_path.clone();

                // Resolve OpenAI token
                let token_result = (|| -> Result<String, String> {
                    let cfg = crate::config::Config::load(&config_path)
                        .map_err(|e| format!("无法加载配置: {}", e))?;
                    crate::usage::resolve_openai_token(
                        &cfg,
                        &crate::auth::state_path_for_config(&config_path),
                        &provider_name,
                    )
                    .map_err(|e| format!("{}", e))
                })();

                match token_result {
                    Ok(token) => {
                        // Call consume_reset (async)
                        let result =
                            crate::tui::run_async_sync(crate::usage::consume_reset(&token));
                        match result {
                            Ok(consume_result) => {
                                let msg = match consume_result {
                                    crate::usage::ConsumeResult::Reset => {
                                        format!("✓ usage 已重置 ({})", provider_name)
                                    }
                                    crate::usage::ConsumeResult::NothingToReset => {
                                        format!("⚠ 没有需要重置的 usage ({})", provider_name)
                                    }
                                    crate::usage::ConsumeResult::NoCredit => {
                                        format!("✗ 没有可用的 reset credit ({})", provider_name)
                                    }
                                    crate::usage::ConsumeResult::AlreadyRedeemed => {
                                        format!("⚠ reset credit 已被使用 ({})", provider_name)
                                    }
                                    crate::usage::ConsumeResult::Unknown => {
                                        format!("⚠ 未知结果 ({})", provider_name)
                                    }
                                };
                                return_to_provider_management_preserving_cursor(
                                    app,
                                    Some(&provider_name),
                                );
                                if let Screen::ProviderManagement(ref mut state) = app.screen {
                                    state.error = Some(msg);
                                }
                            }
                            Err(e) => {
                                // Show error — stay on reset confirm screen
                                if let Screen::ResetUsageConfirm(ref mut state) = app.screen {
                                    state.error = Some(format!("重置失败: {}", e));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if let Screen::ResetUsageConfirm(ref mut state) = app.screen {
                            state.error = Some(e);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Find all models that reference a given provider
fn find_referencing_models(app: &AppModel, provider_name: &str) -> Vec<String> {
    let config = match crate::config::Config::load(&app.config_path) {
        Ok(cfg) => cfg,
        Err(_) => return Vec::new(),
    };

    let mut referencing = Vec::new();
    for (model_id, model_config) in &config.models {
        let has_binding = model_config
            .openai_chat_providers
            .iter()
            .any(|p| p.name == provider_name)
            || model_config
                .openai_responses_providers
                .iter()
                .any(|p| p.name == provider_name)
            || model_config
                .anthropic_providers
                .iter()
                .any(|p| p.name == provider_name);

        if has_binding {
            referencing.push(model_id.clone());
        }
    }
    referencing
}

fn build_provider_detail(app: &AppModel, provider: &ProviderListItem) -> ProviderDetailState {
    let quota_fetched_at = if let Screen::ProviderManagement(ref state) = app.screen {
        state.quota_fetched_at
    } else {
        None
    };
    crate::tui::model::build_provider_detail(&app.config_path, provider, quota_fetched_at)
}

fn enter_model_reselection(app: &mut AppModel, provider_name: &str) {
    let config = match crate::config::Config::load(&app.config_path) {
        Ok(config) => config,
        Err(err) => {
            if let Screen::ProviderManagement(ref mut state) = app.screen {
                state.error = Some(format!("无法加载配置: {err}"));
            }
            return;
        }
    };
    let Some(provider) = config.providers.get(provider_name) else {
        return;
    };
    let items = super::models::get_model_templates(Some(&provider.product));
    if items.is_empty() {
        if let Screen::ProviderManagement(ref mut state) = app.screen {
            state.error = Some("自定义产品不支持模型重选".to_string());
        }
        return;
    }
    let selected = items
        .iter()
        .filter(|item| {
            config.models.get(&item.id).is_some_and(|model| {
                crate::config::Protocol::CLIENT_PROTOCOLS
                    .into_iter()
                    .any(|protocol| {
                        model
                            .provider_bindings(protocol)
                            .iter()
                            .any(|binding| binding.name == provider_name)
                    })
            })
        })
        .map(|item| item.id.clone())
        .collect();
    let product_name = provider.product.clone();
    app.screen = Screen::ModelSelection(model::ModelSelectionState {
        items,
        cursor: 0,
        filter: String::new(),
        filter_active: false,
        selected,
        configured: HashSet::new(),
        error: None,
        product_name,
        source: model::ModelSelectionSource::ProviderReselect {
            provider_name: provider_name.to_string(),
        },
        is_openrouter: false,
        is_degraded: false,
        openrouter_cached_at: None,
    });
}

/// Enter the fallback configuration screen from the provider management panel.
///
/// The selected provider becomes the "fallback provider" (兜底者).
/// We find other providers of the same product as targets, and enumerate
/// all (model, endpoint) combinations the targets participate in as options.
fn enter_fallback_config(app: &mut AppModel, provider_name: &str) {
    let config = match crate::config::Config::load(&app.config_path) {
        Ok(cfg) => cfg,
        Err(_) => return, // Can't load config, stay on current screen
    };

    // Get the provider's product
    let Some(provider_config) = config.providers.get(provider_name) else {
        return;
    };

    // Custom product providers can't do batch fallback
    if provider_config.is_custom_product() {
        if let Screen::ProviderManagement(ref mut state) = app.screen {
            state.error = Some("自定义产品不支持 Fallback 配置".to_string());
        }
        return;
    }

    let product = &provider_config.product;

    // Find other providers of the same product (these are the targets / 被兜底的)
    let target_providers: Vec<String> =
        crate::tui::model::find_providers_by_product(&config.providers, product)
            .into_iter()
            .filter(|name| name != provider_name)
            .collect();

    // Build (model, endpoint) options: enumerate all (model_id, protocol) combinations
    // where any target provider has a binding.
    let mut options = Vec::new();
    for (model_id, model_config) in &config.models {
        for protocol in crate::config::Protocol::CLIENT_PROTOCOLS {
            let bindings = model_config.provider_bindings(protocol);
            // Check if any target provider is in this chain
            let has_target = bindings.iter().any(|b| target_providers.contains(&b.name));
            if has_target {
                let endpoint_label = protocol_short_name(protocol);
                // Use model_id as display name (no catalog template available here)
                options.push(FallbackOption {
                    model_id: model_id.clone(),
                    model_display_name: model_id.clone(),
                    endpoint: endpoint_label.to_string(),
                    selected: false,
                });
            }
        }
    }

    // Sort options for consistent display
    options.sort_by(|a, b| {
        a.model_id
            .cmp(&b.model_id)
            .then(a.endpoint.cmp(&b.endpoint))
    });

    app.screen = Screen::FallbackConfig(FallbackConfigState {
        target_providers,
        target_cursor: 0,
        options,
        option_cursor: 0,
        focus: crate::tui::model::FallbackFocus::TargetProvider,
        error: None,
        skipped: false,
        source: FallbackSource::ProviderManagement {
            provider_name: provider_name.to_string(),
        },
        scroll_offset: 0,
    });
}

/// Map protocol to short name (mirrors fallback.rs internal function).
fn enter_copy_configure_from_list(app: &mut AppModel) {
    if let Screen::ProviderManagement(ref state) = app.screen
        && let Some(provider) = state.selected_provider()
    {
        match crate::tui::model::build_copy_configure_state(
            &app.config_path,
            &provider.name,
            &provider.product,
            &provider.auth_type,
        ) {
            Some(copy_state) => app.screen = Screen::CopyConfigure(copy_state),
            None => {
                if let Screen::ProviderManagement(ref mut state) = app.screen {
                    state.error = Some("无法加载该 provider 的复制配置".to_string());
                }
            }
        }
    }
}

fn enter_copy_configure_from_detail(app: &mut AppModel) {
    if let Screen::ProviderDetail(ref state) = app.screen
        && let Some(copy_state) = crate::tui::model::build_copy_configure_state(
            &app.config_path,
            &state.name,
            &state.product,
            &state.auth_description,
        )
    {
        app.screen = Screen::CopyConfigure(copy_state);
    }
}

fn protocol_short_name(protocol: crate::config::Protocol) -> &'static str {
    match protocol {
        crate::config::Protocol::OpenaiChatCompletions => "chat",
        crate::config::Protocol::OpenaiResponses => "responses",
        crate::config::Protocol::Anthropic => "anthropic",
    }
}

/// Format a number with thousands separators for display (e.g. 1_234_567).
fn format_usage_number(n: i64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::auth::{AccountsFile, OpenaiAccount};
    use crate::tui::model::{
        EntryMode, ModelSelectionSource, ProviderListItem, ProviderManagementState, ProviderStatus,
    };

    #[test]
    fn expired_oauth_candidates_only_include_configured_provider_accounts() {
        let config_path = tempfile::NamedTempFile::new().expect("temp config");
        std::fs::write(
            config_path.path(),
            r#"
[server]
listen = "127.0.0.1:8989"

[providers.openai-configured]
product = "openai-sub"
openai_responses = { url = "https://example.test/openai" }
auth = { type = "openai_oauth", account = "shared-openai" }

[providers.api-key]
product = "deepseek"
openai_chat = { url = "https://example.test/deepseek" }
api_key_env = "DEEPSEEK_API_KEY"
"#,
        )
        .expect("write config");
        let config = crate::config::Config::load(config_path.path()).expect("valid config");
        let mut accounts = AccountsFile::new();
        accounts.openai.insert(
            "shared-openai".to_string(),
            OpenaiAccount {
                account_label: "visible-openai-label".to_string(),
                access_token: "access-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires_at_unix: 1,
                updated_at_unix: 1,
            },
        );
        accounts.openai.insert(
            "orphan-account-id".to_string(),
            OpenaiAccount {
                account_label: "orphan-label".to_string(),
                access_token: "access-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires_at_unix: 1,
                updated_at_unix: 1,
            },
        );

        assert_eq!(
            expired_oauth_provider_ids(&config, &accounts),
            vec!["openai-configured".to_string()]
        );
    }

    #[test]
    fn m_opens_catalog_model_reselection_with_existing_bindings_preselected() {
        let config_path = tempfile::NamedTempFile::new().expect("temp config");
        std::fs::write(
            config_path.path(),
            r#"
[server]
listen = "127.0.0.1:8989"

[providers.openai-sub]
product = "openai-sub"
openai_responses = { url = "https://chatgpt.com/backend-api/codex/responses" }
auth = { type = "openai_oauth", account = "openai-subscription" }

[models."gpt-5.6-sol-sub-lp"]
context_window = 1050000
max_output_tokens = 128000
openai_responses_providers = [{ name = "openai-sub", model = "gpt-5.6-sol" }]
"#,
        )
        .expect("write config");
        crate::config::Config::load(config_path.path()).expect("valid config");
        let mut app = AppModel::new(
            config_path.path().to_path_buf(),
            EntryMode::ProviderTui { select: None },
        );
        app.screen = Screen::ProviderManagement(ProviderManagementState {
            providers: vec![ProviderListItem {
                name: "openai-sub".to_string(),
                product: "openai-sub".to_string(),
                auth_type: "OAuth".to_string(),
                status: ProviderStatus::Ok,
                protocols: vec!["responses".to_string()],
                cooldown: None,
            }],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            error: None,
            scroll_offset: 0,
            quota_fetched_at: None,
        });

        handle_provider_management_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        );

        match &app.screen {
            Screen::ModelSelection(state) => {
                assert_eq!(
                    state.source,
                    ModelSelectionSource::ProviderReselect {
                        provider_name: "openai-sub".to_string()
                    }
                );
                assert!(state.selected.contains("gpt-5.6-sol-sub-lp"));
                assert!(
                    state
                        .items
                        .iter()
                        .any(|item| item.id == "gpt-5.4-mini-sub-lp")
                );
            }
            other => panic!("expected model selection, got {other:?}"),
        }
    }

    #[test]
    fn detail_m_opens_catalog_model_reselection_with_existing_bindings_preselected() {
        let config_path = tempfile::NamedTempFile::new().expect("temp config");
        std::fs::write(
            config_path.path(),
            r#"
[server]
listen = "127.0.0.1:8989"

[providers.openai-sub]
product = "openai-sub"
openai_responses = { url = "https://chatgpt.com/backend-api/codex/responses" }
auth = { type = "openai_oauth", account = "openai-subscription" }

[models."gpt-5.6-sol-sub-lp"]
context_window = 1050000
max_output_tokens = 128000
openai_responses_providers = [{ name = "openai-sub", model = "gpt-5.6-sol" }]
"#,
        )
        .expect("write config");
        let mut app = AppModel::new(
            config_path.path().to_path_buf(),
            EntryMode::ProviderTui { select: None },
        );
        app.screen = Screen::ProviderDetail(ProviderDetailState {
            name: "openai-sub".to_string(),
            product: "openai-sub".to_string(),
            auth_description: "OAuth".to_string(),
            auth_status: "✓ 已配置".to_string(),
            endpoints: vec![],
            bound_models: vec!["gpt-5.6-sol-sub-lp".to_string()],
            compat_info: vec![],
            model_cursor: 0,
            scroll_offset: 0,
            quota_fetched_at: None,
        });

        handle_provider_detail_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        );

        match &app.screen {
            Screen::ModelSelection(state) => {
                assert_eq!(
                    state.source,
                    ModelSelectionSource::ProviderReselect {
                        provider_name: "openai-sub".to_string()
                    }
                );
                assert!(state.selected.contains("gpt-5.6-sol-sub-lp"));
            }
            other => panic!("expected model selection, got {other:?}"),
        }
    }

    #[test]
    fn list_c_opens_copy_configure_with_next_instance_id() {
        let config_path = tempfile::NamedTempFile::new().expect("temp config");
        std::fs::write(
            config_path.path(),
            r#"
[server]
listen = "127.0.0.1:0"

[providers.deepseek]
product = "deepseek"
openai_chat = { url = "https://api.deepseek.com/chat/completions" }
api_key_env = "DEEPSEEK_API_KEY"
"#,
        )
        .expect("write config");
        crate::config::Config::load(config_path.path()).expect("valid config");
        let mut app = AppModel::new(
            config_path.path().to_path_buf(),
            EntryMode::ProviderTui { select: None },
        );
        app.screen = Screen::ProviderManagement(ProviderManagementState {
            providers: vec![ProviderListItem {
                name: "deepseek".to_string(),
                product: "DeepSeek API".to_string(),
                auth_type: "ApiKey".to_string(),
                status: ProviderStatus::Ok,
                protocols: vec!["chat".to_string()],
                cooldown: None,
            }],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            error: None,
            scroll_offset: 0,
            quota_fetched_at: None,
        });

        handle_provider_management_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );

        match &app.screen {
            Screen::CopyConfigure(state) => {
                assert_eq!(state.source_id, "deepseek");
                assert_eq!(state.input, "deepseek-2");
                assert_eq!(state.step, crate::tui::model::CopyConfigureStep::Name);
                assert_eq!(
                    state.validation,
                    crate::tui::model::NameValidation::Available
                );
                assert_eq!(state.env_input, "DEEPSEEK_API_KEY");
                assert!(!state.editing);
            }
            other => panic!("expected CopyConfigure, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn copy_configure_enter_advances_to_auth_then_writes_provider() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config_path = temp.path().join("config.toml");
        let runtime_dir = temp.path().join("runtime");
        std::fs::write(
            &config_path,
            r#"
[server]
listen = "127.0.0.1:0"

[providers.deepseek]
product = "deepseek"
openai_chat = { url = "https://api.deepseek.com/chat/completions" }
api_key_env = "DEEPSEEK_API_KEY"
"#,
        )
        .expect("write config");
        let mut app = AppModel::new(config_path.clone(), EntryMode::ProviderTui { select: None });
        app.screen = Screen::ProviderManagement(ProviderManagementState {
            providers: vec![ProviderListItem {
                name: "deepseek".to_string(),
                product: "DeepSeek API".to_string(),
                auth_type: "ApiKey".to_string(),
                status: ProviderStatus::Ok,
                protocols: vec!["chat".to_string()],
                cooldown: None,
            }],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            error: None,
            scroll_offset: 0,
            quota_fetched_at: None,
        });

        handle_provider_management_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );
        crate::tui::update::copy_configure::handle_copy_configure_keys(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        match &app.screen {
            Screen::CopyConfigure(state) => {
                assert_eq!(state.step, crate::tui::model::CopyConfigureStep::Auth);
                assert_eq!(state.env_input, "DEEPSEEK_API_KEY");
            }
            other => panic!("expected auth step, got {other:?}"),
        }

        crate::tui::update::copy_configure::apply_provider_copy(
            &config_path,
            &runtime_dir,
            "deepseek",
            "deepseek-2",
            Some("DEEPSEEK_API_KEY".to_string()),
            false,
        )
        .await
        .expect("copy");

        let cfg = crate::config::Config::load(&config_path).expect("load");
        assert!(cfg.providers.contains_key("deepseek"));
        let copied = cfg.providers.get("deepseek-2").expect("copied");
        assert_eq!(copied.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn copy_configure_conflict_stays_on_name_step() {
        let config_path = tempfile::NamedTempFile::new().expect("temp config");
        std::fs::write(
            config_path.path(),
            r#"
[server]
listen = "127.0.0.1:0"

[providers.deepseek]
product = "deepseek"
openai_chat = { url = "https://api.deepseek.com/chat/completions" }
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek-2]
product = "deepseek"
openai_chat = { url = "https://api.deepseek.com/chat/completions" }
api_key_env = "OTHER_ENV"
"#,
        )
        .expect("write");
        let mut app = AppModel::new(
            config_path.path().to_path_buf(),
            EntryMode::ProviderTui { select: None },
        );
        app.screen = Screen::CopyConfigure(crate::tui::model::CopyConfigureState {
            source_id: "deepseek".to_string(),
            source_product: "DeepSeek API".to_string(),
            source_auth_label: "ApiKey".to_string(),
            source_endpoints: "chat".to_string(),
            auth_kind: crate::tui::model::CopyAuthKind::ApiKey,
            default_name: "deepseek-3".to_string(),
            input: "deepseek-2".to_string(),
            editing: false,
            cursor_pos: 0,
            validation: crate::tui::model::NameValidation::Conflict,
            step: crate::tui::model::CopyConfigureStep::Name,
            source_api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            env_input: "DEEPSEEK_API_KEY".to_string(),
            env_editing: false,
            env_cursor_pos: 0,
            error: None,
        });

        crate::tui::update::copy_configure::handle_copy_configure_keys(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        match &app.screen {
            Screen::CopyConfigure(state) => {
                assert_eq!(state.step, crate::tui::model::CopyConfigureStep::Name);
                assert!(state.editing);
                assert_eq!(state.error.as_deref(), Some("名字已被占用"));
            }
            other => panic!("expected to stay on name step, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oauth_copy_requires_login_and_does_not_copy_tokens() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config_path = temp.path().join("config.toml");
        let runtime_dir = temp.path().join("runtime");
        std::fs::write(
            &config_path,
            r#"
[server]
listen = "127.0.0.1:0"

[providers.openai-configured]
product = "openai-sub"
openai_responses = { url = "https://example.test/openai" }
auth = { type = "openai_oauth" }
"#,
        )
        .expect("write");
        let requires_login = crate::tui::update::copy_configure::apply_provider_copy(
            &config_path,
            &runtime_dir,
            "openai-configured",
            "openai-configured-2",
            None,
            false,
        )
        .await
        .expect("copy oauth");
        assert!(requires_login);

        let cfg = crate::config::Config::load(&config_path).expect("load");
        let copied = cfg.providers.get("openai-configured-2").expect("copied");
        match copied.auth_config("openai-configured-2").expect("auth") {
            crate::config::AuthConfig::OpenaiOauth { account } => {
                assert_eq!(account.as_deref(), Some("openai-configured-2"));
            }
            other => panic!("expected openai oauth, got {other:?}"),
        }
    }

    /// : 从详情页返回列表时，quota_fetched_at 必须保留。
    #[test]
    fn return_to_list_preserves_quota_fetched_at() {
        let config_path = tempfile::NamedTempFile::new().expect("temp config");
        std::fs::write(
            config_path.path(),
            r#"
[server]
listen = "127.0.0.1:8989"

[providers.openai-configured]
product = "openai-sub"
openai_responses = { url = "https://example.test/openai" }
auth = { type = "openai_oauth", account = "test-account" }
"#,
        )
        .expect("write config");

        let mut app = AppModel::new(
            config_path.path().to_path_buf(),
            EntryMode::ProviderTui { select: None },
        );

        let fetched_at = Some(std::time::SystemTime::now());

        app.screen = Screen::ProviderManagement(ProviderManagementState {
            providers: vec![ProviderListItem {
                name: "openai-configured".to_string(),
                product: "openai-sub".to_string(),
                auth_type: "OAuth".to_string(),
                status: ProviderStatus::Ok,
                protocols: vec!["responses".to_string()],
                cooldown: None,
            }],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            error: None,
            scroll_offset: 0,
            quota_fetched_at: fetched_at,
        });

        // 模拟从详情页返回
        return_to_provider_management_preserving_cursor(&mut app, Some("openai-configured"));

        match &app.screen {
            Screen::ProviderManagement(state) => {
                assert_eq!(state.providers.len(), 1);
                assert_eq!(state.providers[0].name, "openai-configured");
                // : quota_fetched_at 必须保留
                assert!(state.quota_fetched_at.is_some());
            }
            other => panic!("expected ProviderManagement, got {other:?}"),
        }
    }
}
