use crossterm::event::{KeyCode, KeyEvent};

use super::model::{self, AppModel, Screen};
use crate::config::Config;

/// Handle keyboard input on the ModelRename screen.
pub(crate) fn handle_model_rename_keys(app: &mut AppModel, key: KeyEvent) {
    let confirm_step = if let Screen::ModelRename(ref s) = app.screen {
        s.confirm_step
    } else {
        return;
    };

    if confirm_step == 0 {
        handle_step_input(app, key);
    } else {
        handle_step_confirm(app, key);
    }
}

/// Step 0: Enter the new model ID.
fn handle_step_input(app: &mut AppModel, key: KeyEvent) {
    let editing = if let Screen::ModelRename(ref s) = app.screen {
        s.editing
    } else {
        return;
    };

    if editing {
        handle_editing_mode(app, key);
    } else {
        handle_browse_mode(app, key);
    }
}

/// Browse mode: `e` enters editing, `Enter` proceeds to next step.
fn handle_browse_mode(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Char('e') => {
            if let Screen::ModelRename(ref mut s) = app.screen {
                s.editing = true;
                s.cursor_pos = s.new_model_id.len();
            }
        }
        KeyCode::Enter => {
            proceed_to_confirm(app);
        }
        _ => {}
    }
}

/// Editing mode: accept character input, Backspace, Left/Right, Enter.
fn handle_editing_mode(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            proceed_to_confirm(app);
        }
        KeyCode::Char(ch) if ch.is_alphanumeric() || ch == '-' || ch == '_' || ch == '.' => {
            if let Screen::ModelRename(ref mut s) = app.screen {
                if s.cursor_pos >= s.new_model_id.len() {
                    s.new_model_id.push(ch);
                } else {
                    s.new_model_id.insert(s.cursor_pos, ch);
                }
                s.cursor_pos += 1;
            }
        }
        KeyCode::Backspace => {
            if let Screen::ModelRename(ref mut s) = app.screen
                && s.cursor_pos > 0
            {
                s.cursor_pos -= 1;
                s.new_model_id.remove(s.cursor_pos);
            }
        }
        KeyCode::Left => {
            if let Screen::ModelRename(ref mut s) = app.screen {
                s.cursor_pos = s.cursor_pos.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if let Screen::ModelRename(ref mut s) = app.screen
                && s.cursor_pos < s.new_model_id.len()
            {
                s.cursor_pos += 1;
            }
        }
        _ => {}
    }
}

/// Proceed from step 0 to step 1 (confirmation).
fn proceed_to_confirm(app: &mut AppModel) {
    let new_model_id = if let Screen::ModelRename(ref s) = app.screen {
        s.new_model_id.trim().to_string()
    } else {
        return;
    };

    if new_model_id.is_empty() {
        if let Screen::ModelRename(ref mut s) = app.screen {
            s.error = Some("Model ID 不能为空".to_string());
            s.editing = true;
            s.cursor_pos = s.new_model_id.len();
        }
        return;
    }

    // Validate: new ID must not be the same as old
    let old_model_id = if let Screen::ModelRename(ref s) = app.screen {
        s.old_model_id.clone()
    } else {
        return;
    };

    if new_model_id == old_model_id {
        if let Screen::ModelRename(ref mut s) = app.screen {
            s.error = Some("新 Model ID 与原 ID 相同".to_string());
        }
        return;
    }

    // Validate: new ID must not already exist in config
    if let Screen::ModelRename(_) = app.screen {
        let config_path = app.config_path.clone();
        if let Ok(config) = Config::load(&config_path)
            && config.models.contains_key(&new_model_id)
        {
            if let Screen::ModelRename(ref mut s) = app.screen {
                s.error = Some(format!("Model ID '{}' 已存在", new_model_id));
                s.editing = true;
                s.cursor_pos = s.new_model_id.len();
            }
            return;
        }
    }

    // Move to confirmation step
    if let Screen::ModelRename(ref mut s) = app.screen {
        s.new_model_id = new_model_id;
        s.confirm_step = 1;
        s.editing = false;
        s.error = Some(format!(
            "重命名会导致客户端配置失效，确认将 '{}' 重命名为 '{}'?",
            s.old_model_id, s.new_model_id
        ));
    }
}

/// Step 1: Confirm or cancel the rename.
fn handle_step_confirm(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Char('y') => {
            perform_rename(app);
        }
        KeyCode::Char('n') | KeyCode::Esc => {
            // Cancel — go back to step 0
            if let Screen::ModelRename(ref mut s) = app.screen {
                s.confirm_step = 0;
                s.error = None;
            }
        }
        _ => {}
    }
}

/// Execute the actual model rename operation.
fn perform_rename(app: &mut AppModel) {
    let (old_model_id, new_model_id, provider_name, return_to_model_list) =
        if let Screen::ModelRename(ref s) = app.screen {
            (
                s.old_model_id.clone(),
                s.new_model_id.clone(),
                s.provider_name.clone(),
                s.return_to_model_list,
            )
        } else {
            return;
        };

    let config_path = app.config_path.clone();

    // Load config and perform the rename BEFORE the delegation call (editor
    // pattern). Body and generation come from one read so the replace can be
    // checked against what was actually edited (CAS).
    let config_path_for_load = config_path.clone();
    let config_dir_for_load = config_path_for_load
        .parent()
        .unwrap_or(std::path::Path::new("."));
    let runtime_dir_for_load = crate::service::StateDirs::resolve(config_dir_for_load).runtime_dir;
    let (mut config, base) = match crate::tui::run_async_sync(async move {
        crate::admin_client::load_config_for_replace(&config_path_for_load, &runtime_dir_for_load)
            .await
    }) {
        Ok(result) => result,
        Err(e) => {
            if let Screen::ModelRename(ref mut s) = app.screen {
                s.error = Some(format!("加载配置失败: {}", e));
            }
            return;
        }
    };

    if let Err(e) = crate::model::rename_model_in_config(&mut config, &old_model_id, &new_model_id)
    {
        if let Screen::ModelRename(ref mut s) = app.screen {
            s.error = Some(format!("重命名失败: {}", e));
            s.confirm_step = 0;
        }
        return;
    }

    // Use the full 5-step delegation flow (detect → delegate | lock → write | retry on HeldByServer)
    let result = crate::tui::run_async_sync(async {
        crate::ownership::with_cli_write_lock_or_delegate(
            &config_path,
            "llm-proxy tui model rename",
            // Local write: no server, acquire lock and write directly
            || {
                let config_path = config_path.clone();
                let config = config.clone();
                async move {
                    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
                    let runtime_dir = crate::service::StateDirs::resolve(config_dir).runtime_dir;
                    let _lock = crate::core::ConfigLock::acquire(
                        &runtime_dir,
                        std::time::Duration::from_secs(5),
                    )?;
                    crate::config_edit::write_full_config(&config_path, &config)
                }
            },
            // Delegate: server running, send via UDS
            |server| {
                let config = config.clone();
                Box::pin(async move { server.config_update(&config, base).await.map(|_| ()) })
            },
        )
        .await
    });

    match result {
        Ok(_) => {
            if return_to_model_list {
                super::model_mgmt::return_to_model_management_preserving_cursor(
                    app,
                    Some(&new_model_id),
                );
                if let Screen::ModelManagement(ref mut state) = app.screen {
                    state.error = Some(format!("✓ 已重命名: {old_model_id} → {new_model_id}"));
                }
                return;
            }
            // Success — return to ProviderDetail with the renamed model
            let provider_name_clone = provider_name.clone();
            // Reload the provider detail with updated models
            if let Ok(config) = Config::load(&app.config_path)
                && let Some(provider_config) = config.providers.get(&provider_name)
            {
                let mut bound_models = Vec::new();
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
                        bound_models.push(model_id.clone());
                    }
                }

                // Find the cursor position for the renamed model
                let model_cursor = bound_models
                    .iter()
                    .position(|m| m == &new_model_id)
                    .unwrap_or(0);

                let auth_description = match &provider_config.auth {
                    Some(crate::config::AuthConfig::ApiKeyEnv { .. }) => "ApiKey".to_string(),
                    Some(crate::config::AuthConfig::OpenaiOauth { .. }) => {
                        "OpenAI OAuth".to_string()
                    }
                    Some(crate::config::AuthConfig::None) | None => "None".to_string(),
                };
                let auth_status = "✓ 已配置".to_string();

                app.screen = Screen::ProviderDetail(model::ProviderDetailState {
                    name: provider_name_clone,
                    product: provider_config.product.clone(),
                    auth_description,
                    auth_status,
                    endpoints: Vec::new(), // Will be populated on next visit
                    bound_models,
                    compat_info: Vec::new(),
                    model_cursor,
                    scroll_offset: 0,
                    quota_fetched_at: None,
                });
            }

            // Show success message
            if let Screen::ProviderDetail(ref mut state) = app.screen {
                state.compat_info = vec![format!(
                    "✓ 已重命名: {} → {} (请同步更新客户端配置中的 model ID)",
                    old_model_id_display(&old_model_id),
                    &new_model_id
                )];
            }
        }
        Err(e) => {
            // Error — stay on ModelRename screen and show error
            if let Screen::ModelRename(ref mut s) = app.screen {
                s.error = Some(format!("重命名失败: {}", e));
                s.confirm_step = 0;
            }
        }
    }
}

/// Display helper for old model ID (truncated if too long).
fn old_model_id_display(id: &str) -> &str {
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelConfig, ProviderBinding};
    use std::collections::BTreeMap;

    fn make_test_config() -> Config {
        let mut config = Config {
            server: Some(crate::config::ServerConfig {
                listen: "127.0.0.1:8080".to_string(),
                usage: Default::default(),
                max_sse_buffer_bytes: 64 * 1024 * 1024,
                max_output_items: 4096,
                proxy: None,
                require_auth: None,
                auth_token: None,
                auth_token_env: None,
                management_port: None,
            }),
            remote: None,
            fallback: Default::default(),
            protection: Default::default(),
            diagnostics: Default::default(),
            status: Default::default(),
            codex: Default::default(),
            launch: Default::default(),
            providers: BTreeMap::new(),
            models: BTreeMap::new(),
        };

        // Add a model with bindings
        let mut model = ModelConfig {
            description: None,
            context_window: 100_000,
            max_output_tokens: 32_000,
            features: vec![],
            reasoning_levels: None,
            supported_reasoning_levels: vec![],
            cooldown: None,
            reasoning_routes: None,
            default_reasoning_level: None,
            supports_thinking: None,
            force_think: None,
            force_no_think: None,
            default_think: None,
            enable_thinking: None,
            chat_reasoning_off_format: None,
            codex_service_tier: None,
            openai_chat_providers: vec![ProviderBinding {
                name: "deepseek".to_string(),
                model: "deepseek-chat".to_string(),
            }],
            openai_responses_providers: vec![ProviderBinding {
                name: "deepseek".to_string(),
                model: "deepseek-chat".to_string(),
            }],
            anthropic_providers: vec![],
            reasoning_level_map: None,
        };
        config.models.insert("my-model".to_string(), model.clone());

        // Add another model with a binding referencing "my-model" as upstream
        model.openai_chat_providers = vec![ProviderBinding {
            name: "other-provider".to_string(),
            model: "my-model".to_string(),
        }];
        model.openai_responses_providers = vec![];
        config.models.insert("referencing-model".to_string(), model);

        config.codex.auto_review_model = Some("my-model".to_string());
        config.codex.model_overrides.insert(
            "my-model".to_string(),
            crate::config::CodexModelOverride {
                auto_review_model: "review-target".to_string(),
            },
        );
        config.codex.model_overrides.insert(
            "other-coding".to_string(),
            crate::config::CodexModelOverride {
                auto_review_model: "my-model".to_string(),
            },
        );

        config
    }

    #[test]
    fn rename_model_moves_entry_and_updates_bindings() {
        let mut config = make_test_config();

        let result = crate::model::rename_model_in_config(&mut config, "my-model", "renamed-model");
        assert!(result.is_ok());

        // Old key should be gone
        assert!(!config.models.contains_key("my-model"));
        // New key should exist
        assert!(config.models.contains_key("renamed-model"));

        // Provider bindings in renamed-model should be unchanged (they reference "deepseek-chat")
        let renamed = config.models.get("renamed-model").unwrap();
        assert_eq!(renamed.openai_chat_providers[0].model, "deepseek-chat");

        // Provider bindings in referencing-model should be updated
        let referencing = config.models.get("referencing-model").unwrap();
        assert_eq!(referencing.openai_chat_providers[0].model, "renamed-model");

        assert_eq!(
            config.codex.auto_review_model.as_deref(),
            Some("renamed-model")
        );
        assert!(!config.codex.model_overrides.contains_key("my-model"));
        assert_eq!(
            config.codex.model_overrides["renamed-model"].auto_review_model,
            "review-target"
        );
        assert_eq!(
            config.codex.model_overrides["other-coding"].auto_review_model,
            "renamed-model"
        );
    }

    #[test]
    fn rename_model_fails_if_old_not_found() {
        let mut config = make_test_config();
        let result = crate::model::rename_model_in_config(&mut config, "nonexistent", "new-name");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown model"));
    }

    #[test]
    fn rename_model_fails_if_new_already_exists() {
        let mut config = make_test_config();
        let result =
            crate::model::rename_model_in_config(&mut config, "my-model", "referencing-model");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
        // Original should be restored
        assert!(config.models.contains_key("my-model"));
    }
}
