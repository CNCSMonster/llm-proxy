use crossterm::event::{KeyCode, KeyEvent};

use super::model::{self, AppModel, Screen};
use crate::tui::update::products::make_product_state;

pub(crate) fn handle_editor_input(app: &mut AppModel, key: KeyEvent) {
    use model::{EditorButton, EditorFocus, EditorMode, ProviderField};

    let Screen::CustomProviderEditor(ref mut state) = app.screen else {
        return;
    };

    match state.mode {
        EditorMode::Browse => {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    match state.focus {
                        EditorFocus::Fields => {
                            // 向上移动，跳过不可编辑字段
                            loop {
                                if state.cursor == 0 {
                                    break;
                                }
                                state.cursor -= 1;
                                if state.is_field_editable(state.fields[state.cursor].field.clone())
                                {
                                    break;
                                }
                            }
                        }
                        EditorFocus::Buttons => {
                            state.focus = EditorFocus::Fields;
                            // 移动到最后一个可编辑字段
                            while state.cursor > 0
                                && !state
                                    .is_field_editable(state.fields[state.cursor].field.clone())
                            {
                                state.cursor -= 1;
                            }
                        }
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    match state.focus {
                        EditorFocus::Fields => {
                            let max = state.fields.len().saturating_sub(1);
                            // 向下移动，跳过不可编辑字段
                            loop {
                                if state.cursor >= max {
                                    // 移动到按钮
                                    state.focus = EditorFocus::Buttons;
                                    break;
                                }
                                state.cursor += 1;
                                if state.is_field_editable(state.fields[state.cursor].field.clone())
                                {
                                    break;
                                }
                            }
                        }
                        EditorFocus::Buttons => {
                            // Already at bottom, do nothing
                        }
                    }
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    if state.focus == EditorFocus::Buttons {
                        state.button_cursor = 0;
                    }
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    if state.focus == EditorFocus::Buttons {
                        state.button_cursor = 1;
                    }
                }
                KeyCode::Enter => {
                    match state.focus {
                        EditorFocus::Fields => {
                            state.mode = EditorMode::Edit;
                            state.edit_cursor = state.current_field().value.len();
                            state.error = None;
                        }
                        EditorFocus::Buttons => {
                            let button = if state.button_cursor == 0 {
                                EditorButton::Save
                            } else {
                                EditorButton::Cancel
                            };
                            match button {
                                EditorButton::Save => {
                                    if state.has_endpoint()
                                        || matches!(
                                            state.purpose,
                                            model::EditorPurpose::Edit { .. }
                                        )
                                    {
                                        // Extract values before calling execute_save to avoid borrow conflict
                                        let name = state
                                            .fields
                                            .iter()
                                            .find(|f| f.field == ProviderField::Name)
                                            .map(|f| f.value.clone())
                                            .unwrap_or_default();
                                        let api_key_env = state
                                            .fields
                                            .iter()
                                            .find(|f| f.field == ProviderField::ApiKeyEnv)
                                            .map(|f| f.value.clone())
                                            .filter(|v| !v.is_empty());
                                        let openai_chat_url = state
                                            .fields
                                            .iter()
                                            .find(|f| f.field == ProviderField::OpenAiChat)
                                            .map(|f| f.value.clone())
                                            .filter(|v| !v.is_empty());
                                        let openai_responses_url = state
                                            .fields
                                            .iter()
                                            .find(|f| f.field == ProviderField::OpenAiResponses)
                                            .map(|f| f.value.clone())
                                            .filter(|v| !v.is_empty());
                                        let anthropic_url = state
                                            .fields
                                            .iter()
                                            .find(|f| f.field == ProviderField::Anthropic)
                                            .map(|f| f.value.clone())
                                            .filter(|v| !v.is_empty());

                                        // 提取 purpose 信息以避免 borrow conflict
                                        let is_edit = matches!(
                                            state.purpose,
                                            model::EditorPurpose::Edit { .. }
                                        );
                                        let original_name = if let model::EditorPurpose::Edit {
                                            original_name,
                                            ..
                                        } = &state.purpose
                                        {
                                            Some(original_name.clone())
                                        } else {
                                            None
                                        };

                                        // 根据模式执行不同的保存逻辑
                                        if is_edit {
                                            execute_edit_save(
                                                app,
                                                original_name.unwrap(),
                                                name,
                                                api_key_env,
                                                openai_chat_url,
                                                openai_responses_url,
                                                anthropic_url,
                                            );
                                        } else {
                                            execute_save_from_values(
                                                app,
                                                name,
                                                api_key_env,
                                                openai_chat_url,
                                                openai_responses_url,
                                                anthropic_url,
                                            );
                                        }
                                    } else {
                                        state.error =
                                            Some("Configure at least one endpoint".to_string());
                                    }
                                }
                                EditorButton::Cancel => {
                                    // 根据模式返回不同的屏幕
                                    match &state.purpose {
                                        model::EditorPurpose::Create => {
                                            app.screen =
                                                Screen::ProductSelection(make_product_state());
                                        }
                                        model::EditorPurpose::Edit { original_name, .. } => {
                                            let name = original_name.clone();
                                            // 返回到 Provider 管理面板，保持光标在当前编辑的 provider
                                            super::provider_mgmt::return_to_provider_management_preserving_cursor(
                                                app,
                                                Some(&name),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                KeyCode::Esc => {
                    // Esc = Cancel (根据模式返回不同屏幕)
                    match &state.purpose {
                        model::EditorPurpose::Create => {
                            app.screen = Screen::ProductSelection(make_product_state());
                        }
                        model::EditorPurpose::Edit { original_name, .. } => {
                            let name = original_name.clone();
                            // 返回到 Provider 管理面板，保持光标在当前编辑的 provider
                            super::provider_mgmt::return_to_provider_management_preserving_cursor(
                                app,
                                Some(&name),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        EditorMode::Edit => match key.code {
            KeyCode::Esc => {
                state.mode = EditorMode::Browse;
                state.error = None;
            }
            KeyCode::Enter => {
                state.mode = EditorMode::Browse;
            }
            KeyCode::Backspace => {
                if state.edit_cursor > 0 {
                    state.fields[state.cursor]
                        .value
                        .remove(state.edit_cursor - 1);
                    state.edit_cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if state.edit_cursor < state.fields[state.cursor].value.len() {
                    state.fields[state.cursor].value.remove(state.edit_cursor);
                }
            }
            KeyCode::Left => {
                state.edit_cursor = state.edit_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = state.fields[state.cursor].value.len();
                if state.edit_cursor < len {
                    state.edit_cursor += 1;
                }
            }
            KeyCode::Home => {
                state.edit_cursor = 0;
            }
            KeyCode::End => {
                state.edit_cursor = state.fields[state.cursor].value.len();
            }
            KeyCode::Char(c) => {
                state.fields[state.cursor]
                    .value
                    .insert(state.edit_cursor, c);
                state.edit_cursor += 1;
            }
            _ => {}
        },
    }
}

fn execute_save_from_values(
    app: &mut AppModel,
    name: String,
    api_key_env: Option<String>,
    openai_chat_url: Option<String>,
    openai_responses_url: Option<String>,
    anthropic_url: Option<String>,
) {
    app.custom_provider_name = Some(name);
    app.custom_provider_endpoint = openai_chat_url.or(openai_responses_url).or(anthropic_url);
    // TODO: store all endpoints in app state, not just the first one
    app.chosen_env_var = api_key_env;

    app.screen = Screen::Verifying(model::VerifyingState {
        product_name: app.custom_provider_name.clone().unwrap_or_default(),
        env_var: app.chosen_env_var.clone(),
        models: Vec::new(),
        force_local: false,
    });
}

/// 执行编辑保存：更新已有 provider 的配置
fn execute_edit_save(
    app: &mut AppModel,
    original_name: String,
    new_name: String,
    api_key_env: Option<String>,
    openai_chat_url: Option<String>,
    openai_responses_url: Option<String>,
    anthropic_url: Option<String>,
) {
    use crate::config::Config;

    // 加载配置
    let mut config = match Config::load(&app.config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            if let Screen::CustomProviderEditor(ref mut state) = app.screen {
                state.error = Some(format!("Failed to load config: {}", e));
            }
            return;
        }
    };

    // 检查新名称是否与其他 provider 冲突
    if new_name != original_name && config.providers.contains_key(&new_name) {
        if let Screen::CustomProviderEditor(ref mut state) = app.screen {
            state.error = Some(format!("Provider name '{}' already exists", new_name));
        }
        return;
    }

    // 获取原始 provider 配置
    let provider = match config.providers.get(&original_name) {
        Some(p) => p.clone(),
        None => {
            if let Screen::CustomProviderEditor(ref mut state) = app.screen {
                state.error = Some(format!("Provider '{}' not found", original_name));
            }
            return;
        }
    };

    // 更新 provider 配置
    let mut updated_provider = provider;

    // 更新 API key env
    if let Some(env) = api_key_env {
        updated_provider.api_key_env = Some(env);
    }

    // 更新 endpoints
    if let Some(url) = openai_chat_url
        && let Some(ref mut ep) = updated_provider.openai_chat
    {
        ep.url = Some(url);
    }
    if let Some(url) = openai_responses_url
        && let Some(ref mut ep) = updated_provider.openai_responses
    {
        ep.url = Some(url);
    }
    if let Some(url) = anthropic_url
        && let Some(ref mut ep) = updated_provider.anthropic
    {
        ep.url = Some(url);
    }

    // 如果名称变了，需要重命名
    if new_name != original_name {
        // 移除旧的
        config.providers.remove(&original_name);
        // 添加新的
        config.providers.insert(new_name.clone(), updated_provider);

        // 更新所有模型绑定中的 provider 名称
        for model in config.models.values_mut() {
            for binding in &mut model.openai_chat_providers {
                if binding.name == original_name {
                    binding.name = new_name.clone();
                }
            }
            for binding in &mut model.openai_responses_providers {
                if binding.name == original_name {
                    binding.name = new_name.clone();
                }
            }
            for binding in &mut model.anthropic_providers {
                if binding.name == original_name {
                    binding.name = new_name.clone();
                }
            }
        }
    } else {
        // 名称没变，直接更新
        config.providers.insert(original_name, updated_provider);
    }

    // 保存配置（使用完整 5 步委托流程：detect → delegate | lock → write | retry on HeldByServer）
    let config_path = app.config_path.clone();
    let result = crate::tui::run_async_sync(async {
        crate::ownership::with_cli_write_lock_or_delegate(
            &config_path,
            "llm-proxy tui provider save",
            // Local write: no server, acquire lock and write directly
            || {
                let config_path = config_path.clone();
                let config = config.clone();
                async move {
                    let _lock = crate::core::ConfigLock::acquire(
                        &crate::service::state_dir(),
                        std::time::Duration::from_secs(5),
                    )?;
                    crate::config_edit::write_full_config(&config_path, &config)
                }
            },
            // Delegate: server running, send via UDS
            |server| {
                let config = config.clone();
                Box::pin(async move { server.config_update(&config).await.map(|_| ()) })
            },
        )
        .await
    });
    if let Err(e) = result {
        if let Screen::CustomProviderEditor(ref mut state) = app.screen {
            state.error = Some(format!("Failed to save config: {}", e));
        }
        return;
    }

    // 返回到 Provider 管理面板，保持光标在编辑后的 provider（如果改名则用新名称）
    super::provider_mgmt::return_to_provider_management_preserving_cursor(app, Some(&new_name));
}
