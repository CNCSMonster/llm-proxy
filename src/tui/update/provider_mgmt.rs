use crossterm::event::{KeyCode, KeyEvent};

use super::model::{self, AppModel, FallbackSource, Screen};
use crate::tui::model::{
    DeleteConfirmState, EndpointInfo, FallbackConfigState, FallbackOption, ProviderDetailState,
    ProviderListItem, ProviderManagementState, ProviderStatus, ResetUsageConfirmState,
};

/// Reload the provider list and return to the ProviderManagement screen,
/// preserving cursor position on `keep_provider` (if it still exists).
pub(super) fn return_to_provider_management_preserving_cursor(
    app: &mut AppModel,
    keep_provider: Option<&str>,
) {
    let providers = crate::tui::model::load_configured_providers(&app.config_path);
    let cursor = keep_provider
        .and_then(|name| providers.iter().position(|p| p.name == name))
        .unwrap_or(0);
    app.screen = Screen::ProviderManagement(ProviderManagementState {
        providers,
        cursor,
        error: None,
        filter: String::new(),
        filter_active: false,
    });
}

/// Auto-refresh expired OAuth tokens before reloading provider list.
/// Returns a list of provider names that failed to refresh.
fn auto_refresh_oauth_tokens(config_path: &std::path::Path) -> Vec<String> {
    let mut errors = Vec::new();

    // Load OAuth accounts to check token status
    let state_path = crate::auth::default_state_path();
    let Ok((accounts, _)) = crate::auth::load_oauth_accounts(&state_path) else {
        return errors; // Can't load OAuth state, skip refresh
    };

    // Check OpenAI accounts
    for (account_id, entry) in &accounts.openai {
        if entry.is_expired() {
            match crate::tui::run_async_sync(crate::auth::refresh_account_for_provider(
                config_path,
                "openai_oauth",
                account_id,
            )) {
                Ok(_) => { /* Refresh succeeded */ }
                Err(e) => {
                    errors.push(format!("OpenAI({}): {}", account_id, e));
                }
            }
        }
    }

    // Check Antigravity accounts
    for (account_id, entry) in &accounts.antigravity {
        if entry.is_expired() {
            match crate::tui::run_async_sync(crate::auth::refresh_account_for_provider(
                config_path,
                "antigravity_oauth",
                account_id,
            )) {
                Ok(_) => { /* Refresh succeeded */ }
                Err(e) => {
                    errors.push(format!("Antigravity({}): {}", account_id, e));
                }
            }
        }
    }

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
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ProviderManagement(ref mut state) = app.screen {
                state.move_down();
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
                items: model::build_product_list(),
                cursor: 0,
                filter: String::new(),
                filter_active: false,
                error: None,
            });
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
                // Check actual auth config from config file (not string matching)
                if let Ok(config) = crate::config::Config::load(&app.config_path)
                    && let Some(pc) = config.providers.get(&provider_name)
                {
                    match pc.auth_config(&provider_name) {
                        Ok(crate::config::AuthConfig::OpenaiOauth { .. }) => {
                            app.screen = Screen::OAuthDeviceCode(model::OAuthDeviceCodeState {
                                provider_name: provider_name.clone(),
                                device_code: String::new(),
                                verification_url: String::new(),
                                expires_at: std::time::Instant::now()
                                    + std::time::Duration::from_secs(15 * 60),
                                copied: false,
                                polling: false,
                                retry_count: 0,
                                last_error: None,
                                error: None,
                            });
                        }
                        Ok(crate::config::AuthConfig::AntigravityOauth { .. }) => {
                            app.screen = Screen::AntigravityLogin(
                                model::AntigravityLoginState::new(provider_name),
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
        KeyCode::Char('u') => {
            // Show usage statistics for selected provider
            if let Screen::ProviderManagement(ref state) = app.screen
                && let Some(provider) = state.selected_provider()
            {
                let provider_name = provider.name.clone();
                let config_path = app.config_path.clone();

                // Load usage config and create store
                let usage_config = crate::config::Config::load(&config_path)
                    .map(|cfg| cfg.server.usage.clone())
                    .unwrap_or_default();

                let message = match crate::usage_stats::UsageStore::new(usage_config) {
                    Ok(store) => {
                        let records = store.get_by_provider(&provider_name);
                        if records.is_empty() {
                            "该 provider 暂无 usage 统计数据".to_string()
                        } else {
                            let total_input: i64 = records.iter().map(|r| r.input_tokens).sum();
                            let total_output: i64 = records.iter().map(|r| r.output_tokens).sum();
                            let total_tokens: i64 = records.iter().map(|r| r.total_tokens).sum();
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
                        &crate::auth::default_state_path(),
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
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ProviderDetail(ref mut state) = app.screen {
                state.model_move_down();
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
                });
            }
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
                        &crate::auth::default_state_path(),
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

/// Build provider detail state from a provider list item
fn build_provider_detail(app: &AppModel, provider: &ProviderListItem) -> ProviderDetailState {
    // Load the actual provider config
    let config = crate::config::Config::load(&app.config_path).ok();

    let mut endpoints = Vec::new();
    let mut bound_models = Vec::new();
    let mut compat_info = Vec::new();

    if let Some(ref cfg) = config {
        if let Some(pc) = cfg.providers.get(&provider.name) {
            // Collect endpoints
            if let Some(ref ep) = pc.openai_chat {
                endpoints.push(EndpointInfo {
                    protocol: "chat".to_string(),
                    url: ep.url.clone().unwrap_or_default(),
                    kind: if ep.derive_from.is_some() {
                        "derived".to_string()
                    } else {
                        "native".to_string()
                    },
                });
            }
            if let Some(ref ep) = pc.openai_responses {
                endpoints.push(EndpointInfo {
                    protocol: "responses".to_string(),
                    url: ep
                        .url
                        .clone()
                        .unwrap_or_else(|| ep.derive_from.clone().unwrap_or_default()),
                    kind: if ep.derive_from.is_some() {
                        "derived".to_string()
                    } else {
                        "native".to_string()
                    },
                });
            }
            if let Some(ref ep) = pc.anthropic {
                endpoints.push(EndpointInfo {
                    protocol: "anthropic".to_string(),
                    url: ep
                        .url
                        .clone()
                        .unwrap_or_else(|| ep.derive_from.clone().unwrap_or_default()),
                    kind: if ep.derive_from.is_some() {
                        "derived".to_string()
                    } else {
                        "native".to_string()
                    },
                });
            }
            if let Some(ref ep) = pc.antigravity {
                endpoints.push(EndpointInfo {
                    protocol: "antigravity".to_string(),
                    url: ep
                        .url
                        .clone()
                        .unwrap_or_else(|| ep.derive_from.clone().unwrap_or_default()),
                    kind: if ep.derive_from.is_some() {
                        "derived".to_string()
                    } else {
                        "native".to_string()
                    },
                });
            }

            // Collect compat info
            if let Some(ref ep) = pc.openai_chat
                && let Some(ref compat) = ep.compat
            {
                if let Some(ref tf) = compat.thinking_format {
                    compat_info.push(format!("thinking_format: {}", tf));
                }
                if let Some(sre) = compat.supports_reasoning_effort {
                    compat_info.push(format!("supports_reasoning_effort: {}", sre));
                }
                if let Some(ref mtf) = compat.max_tokens_field {
                    compat_info.push(format!("max_tokens_field: {}", mtf));
                }
            }
        }

        // Collect bound models
        for (model_id, model_config) in &cfg.models {
            // Check if this model references the provider
            let has_binding = model_config
                .openai_chat_providers
                .iter()
                .any(|p| p.name == provider.name)
                || model_config
                    .openai_responses_providers
                    .iter()
                    .any(|p| p.name == provider.name)
                || model_config
                    .anthropic_providers
                    .iter()
                    .any(|p| p.name == provider.name);

            if has_binding {
                bound_models.push(model_id.clone());
            }
        }
    }

    let auth_description = provider.auth_type.clone();
    let auth_status = match provider.status {
        ProviderStatus::Ok => "✓ 已配置".to_string(),
        ProviderStatus::Warning => "⚠ 需要登录".to_string(),
        ProviderStatus::Error => "✗ 错误".to_string(),
    };

    ProviderDetailState {
        name: provider.name.clone(),
        product: provider.product.clone(),
        auth_description,
        auth_status,
        endpoints,
        bound_models,
        compat_info,
        model_cursor: 0,
    }
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
    });
}

/// Map protocol to short name (mirrors fallback.rs internal function).
fn protocol_short_name(protocol: crate::config::Protocol) -> &'static str {
    match protocol {
        crate::config::Protocol::OpenaiChatCompletions => "chat",
        crate::config::Protocol::OpenaiResponses => "responses",
        crate::config::Protocol::Anthropic => "anthropic",
        crate::config::Protocol::Antigravity => "antigravity",
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
