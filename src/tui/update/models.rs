use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::model::{self, AppModel, Screen, VerifyingState};
use crate::tui::update::utils::handle_filter_input;

pub(crate) fn handle_model_keys(app: &mut AppModel, key: KeyEvent) {
    // If filter is active, only handle filter keys, space, a, Enter
    {
        let filter_active = if let Screen::ModelSelection(ref s) = app.screen {
            s.filter_active
        } else {
            false
        };
        if filter_active {
            match key.code {
                KeyCode::Esc => {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        s.deactivate_filter();
                    }
                    return;
                }
                KeyCode::Enter => {
                    confirm_model_selection(app);
                    return;
                }
                KeyCode::Char(' ') => {
                    toggle_model_selection(app);
                    return;
                }
                KeyCode::Char('a') => {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        toggle_all_models(s);
                    }
                    return;
                }
                KeyCode::Up | KeyCode::Char('k')
                    if key.code == KeyCode::Up || key.modifiers.is_empty() =>
                {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        s.move_up();
                    }
                    return;
                }
                KeyCode::Down | KeyCode::Char('j')
                    if key.code == KeyCode::Down || key.modifiers.is_empty() =>
                {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        s.move_down();
                    }
                    return;
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        s.move_up();
                    }
                    return;
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        s.move_down();
                    }
                    return;
                }
                _ => {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        handle_filter_input(&mut s.filter, key);
                    }
                    return;
                }
            }
        }
    }

    // Normal mode
    match key.code {
        KeyCode::Enter => {
            confirm_model_selection(app);
        }
        KeyCode::Char(' ') => {
            toggle_model_selection(app);
        }
        KeyCode::Char('a') => {
            if let Screen::ModelSelection(ref mut s) = app.screen {
                toggle_all_models(s);
            }
        }
        KeyCode::Char('/') => {
            if let Screen::ModelSelection(ref mut s) = app.screen {
                s.activate_filter();
            }
        }
        KeyCode::Char('F') | KeyCode::Char('f') => {
            switch_to_fallback_config(app);
        }
        KeyCode::Char('c') => {
            enter_copy_model_confirm(app);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::ModelSelection(ref mut s) = app.screen {
                s.move_up();
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ModelSelection(ref mut s) = app.screen {
                s.move_down();
            }
        }
        _ => {}
    }
}

fn confirm_model_selection(app: &mut AppModel) {
    let (selected, product_name) = {
        if let Screen::ModelSelection(ref s) = app.screen {
            (s.selected.clone(), s.product_name.clone())
        } else {
            return;
        }
    };
    // Zero selection = skip: proceed with empty models (only create provider, no Model ID binding)
    let models: Vec<String> = selected.iter().cloned().collect();
    app.chosen_models = models.clone();
    app.screen = Screen::Verifying(VerifyingState {
        product_name,
        env_var: app.chosen_env_var.clone(),
        models,
        force_local: false,
    });
}

/// Switch from model selection to fallback config screen.
fn switch_to_fallback_config(app: &mut AppModel) {
    // Cache the current ModelSelectionState so selected/cursor survive round-trips.
    if let Screen::ModelSelection(ref state) = app.screen {
        app.cached_model_selection = Some(state.clone());
    }
    let fallback_state = super::env::build_fallback_state(app);
    app.screen = Screen::FallbackConfig(fallback_state);
}

fn toggle_model_selection(app: &mut AppModel) {
    let cursor = if let Screen::ModelSelection(ref s) = app.screen {
        s.cursor
    } else {
        return;
    };
    if let Screen::ModelSelection(ref mut s) = app.screen
        && let Some(item) = s.items.get(cursor)
    {
        if s.selected.contains(&item.id) {
            s.selected.remove(&item.id);
        } else {
            s.selected.insert(item.id.clone());
        }
    }
}

fn toggle_all_models(state: &mut super::model::ModelSelectionState) {
    if state.selected.len() == state.items.len() {
        state.selected.clear();
    } else {
        for item in &state.items {
            state.selected.insert(item.id.clone());
        }
    }
}

/// Enter the copy-model confirmation screen from the ModelSelection screen.
/// The currently highlighted model (by cursor) becomes the source.
fn enter_copy_model_confirm(app: &mut AppModel) {
    let (source_id, product_name) = if let Screen::ModelSelection(ref s) = app.screen {
        // Cache the current ModelSelectionState so it survives round-trips via Esc
        app.cached_model_selection = Some(s.clone());
        let item = match s.items.get(s.cursor) {
            Some(item) => item,
            None => return,
        };
        (item.id.clone(), s.product_name.clone())
    } else {
        return;
    };

    // Load config to generate a default new ID
    let config = crate::config::Config::load(&app.config_path).ok();
    let default_new_id = config
        .as_ref()
        .map(|cfg| model::generate_default_copy_id(&source_id, &cfg.models))
        .unwrap_or_else(|| format!("{}-1", source_id));

    app.screen = Screen::CopyModelConfirm(model::CopyModelConfirmState {
        source_model_id: source_id,
        default_new_id: default_new_id.clone(),
        new_id: default_new_id,
        cursor_pos: 0, // will be set after screen creation
        editing: true,
        validation: model::NameValidation::Available,
        error: None,
        product_name,
    });
    // Set cursor to end of the default ID
    if let Screen::CopyModelConfirm(ref mut s) = app.screen {
        s.cursor_pos = s.new_id.len();
    }
}

/// Handle keyboard input for the CopyModelConfirm screen.
pub(crate) fn handle_copy_model_confirm_keys(app: &mut AppModel, key: KeyEvent) {
    let editing = if let Screen::CopyModelConfirm(ref s) = app.screen {
        s.editing
    } else {
        return;
    };

    if editing {
        handle_copy_editing_mode(app, key);
    } else {
        handle_copy_browse_mode(app, key);
    }
}

/// Browse mode: `e` enters editing, `Enter` confirms copy.
fn handle_copy_browse_mode(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Char('e') => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen {
                s.editing = true;
                s.cursor_pos = s.new_id.len();
            }
        }
        KeyCode::Enter => {
            confirm_copy_model(app);
        }
        _ => {}
    }
}

/// Editing mode: accept character input, Backspace, Left/Right, Enter.
fn handle_copy_editing_mode(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            confirm_copy_model(app);
        }
        KeyCode::Esc => {
            // Exit editing mode, revert to default ID
            if let Screen::CopyModelConfirm(ref mut s) = app.screen {
                s.editing = false;
                s.new_id = s.default_new_id.clone();
                s.cursor_pos = 0;
                s.error = None;
                validate_copy_id(s, &app.config_path);
            }
        }
        KeyCode::Char(ch) if ch.is_alphanumeric() || ch == '-' || ch == '_' || ch == '.' => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen {
                if s.cursor_pos >= s.new_id.len() {
                    s.new_id.push(ch);
                } else {
                    s.new_id.insert(s.cursor_pos, ch);
                }
                s.cursor_pos += 1;
                validate_copy_id(s, &app.config_path);
            }
        }
        KeyCode::Backspace => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen
                && s.cursor_pos > 0
            {
                s.cursor_pos -= 1;
                s.new_id.remove(s.cursor_pos);
                validate_copy_id(s, &app.config_path);
            }
        }
        KeyCode::Left => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen {
                s.cursor_pos = s.cursor_pos.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen
                && s.cursor_pos < s.new_id.len()
            {
                s.cursor_pos += 1;
            }
        }
        _ => {}
    }
}

/// Validate the new model ID against existing models.
fn validate_copy_id(s: &mut model::CopyModelConfirmState, config_path: &std::path::Path) {
    let models = crate::config::Config::load(config_path)
        .map(|cfg| cfg.models)
        .unwrap_or_default();
    s.validation = if models.contains_key(&s.new_id) {
        model::NameValidation::Conflict
    } else {
        model::NameValidation::Available
    };
}

/// Perform the model copy: clone source model fields (without bindings), write to config.
fn confirm_copy_model(app: &mut AppModel) {
    let (source_id, new_id, validation) = if let Screen::CopyModelConfirm(ref s) = app.screen {
        (
            s.source_model_id.clone(),
            s.new_id.clone(),
            s.validation.clone(),
        )
    } else {
        return;
    };

    // Block if name conflicts
    if validation == model::NameValidation::Conflict {
        if let Screen::CopyModelConfirm(ref mut s) = app.screen {
            s.editing = true;
            s.cursor_pos = s.new_id.len();
        }
        return;
    }

    // Block if new ID is empty
    if new_id.trim().is_empty() {
        if let Screen::CopyModelConfirm(ref mut s) = app.screen {
            s.editing = true;
            s.cursor_pos = 0;
        }
        return;
    }

    let config_path = app.config_path.clone();

    // Perform the copy using with_cli_write_lock_async (local lock, same as CLI model add)
    let result = crate::tui::run_async_sync(async {
        crate::ownership::with_cli_write_lock_async("llm-proxy tui model copy", async {
            let mut cfg = crate::config::Config::load(&config_path)?;
            let source = cfg
                .models
                .get(&source_id)
                .ok_or_else(|| anyhow::anyhow!("source model {:?} not found", source_id))?
                .clone();

            // Copy fields but NOT bindings
            let new_model = crate::config::ModelConfig {
                description: source.description,
                context_window: source.context_window,
                max_output_tokens: source.max_output_tokens,
                features: source.features,
                supported_reasoning_levels: source.supported_reasoning_levels,
                default_reasoning_level: source.default_reasoning_level,
                enable_thinking: source.enable_thinking,
                openai_chat_providers: Vec::new(),
                openai_responses_providers: Vec::new(),
                anthropic_providers: Vec::new(),
                reasoning_level_map: source.reasoning_level_map,
            };

            cfg.models.insert(new_id.clone(), new_model);
            crate::config_edit::write_model(&config_path, &cfg, &new_id)?;
            Ok::<(), anyhow::Error>(())
        })
        .await
    });

    match result {
        Ok(()) => {
            // Success — show message on ProviderManagement panel
            let msg = format!("✓ Model '{}' 已创建（需手动添加 provider 绑定）", new_id);
            return_to_model_selection_with_message(app, &msg);
        }
        Err(e) => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen {
                s.error = Some(format!("复制失败: {}", e));
            }
        }
    }
}

/// Return to the ModelSelection screen, showing a success message.
fn return_to_model_selection_with_message(app: &mut AppModel, message: &str) {
    // We can't easily show a message on ModelSelection (it doesn't have an error field
    // that's used for success). Instead, go back to the ProviderManagement screen
    // which does support success/error messages.
    let providers = model::load_configured_providers(&app.config_path);
    app.screen = Screen::ProviderManagement(model::ProviderManagementState {
        providers,
        cursor: 0,
        error: Some(message.to_string()),
        filter: String::new(),
        filter_active: false,
    });
}

pub(crate) fn get_model_templates(provider_id: Option<&str>) -> Vec<model::ModelItem> {
    let provider_id = match provider_id {
        Some(id) => id,
        None => return vec![],
    };

    match provider_id {
        "deepseek" => vec![
            model::ModelItem {
                id: "deepseek-v4-flash-lp".to_string(),
                display_name: "deepseek-v4-flash".to_string(),
                context_window: 1_000_000,
                max_output_tokens: 393_216,
                supports_image: false,
            },
            model::ModelItem {
                id: "deepseek-v4-pro-lp".to_string(),
                display_name: "deepseek-v4-pro".to_string(),
                context_window: 1_000_000,
                max_output_tokens: 393_216,
                supports_image: false,
            },
        ],
        "bailian-coding-plan-cn" | "bailian-payg-cn" | "bailian-payg-us" => vec![
            model::ModelItem {
                id: "qwen3.7-plus-bailian-lp".to_string(),
                display_name: "qwen3.7-plus".to_string(),
                context_window: 1_000_000,
                max_output_tokens: 65_536,
                supports_image: true,
            },
            model::ModelItem {
                id: "qwen3.6-plus-bailian-lp".to_string(),
                display_name: "qwen3.6-plus".to_string(),
                context_window: 1_000_000,
                max_output_tokens: 65_536,
                supports_image: true,
            },
            model::ModelItem {
                id: "qwen3.5-plus-bailian-lp".to_string(),
                display_name: "qwen3.5-plus".to_string(),
                context_window: 1_000_000,
                max_output_tokens: 65_536,
                supports_image: true,
            },
            model::ModelItem {
                id: "kimi-k2.5-bailian-lp".to_string(),
                display_name: "kimi-k2.5".to_string(),
                context_window: 262_144,
                max_output_tokens: 98_304,
                supports_image: true,
            },
            model::ModelItem {
                id: "glm-5-bailian-lp".to_string(),
                display_name: "glm-5".to_string(),
                context_window: 202_752,
                max_output_tokens: 2_000_000,
                supports_image: false,
            },
            model::ModelItem {
                id: "minimax-m2.5-bailian-lp".to_string(),
                display_name: "MiniMax-M2.5".to_string(),
                context_window: 196_608,
                max_output_tokens: 32_768,
                supports_image: false,
            },
            model::ModelItem {
                id: "qwen3-max-2026-01-23-bailian-lp".to_string(),
                display_name: "qwen3-max-2026-01-23".to_string(),
                context_window: 262_144,
                max_output_tokens: 32_768,
                supports_image: false,
            },
            model::ModelItem {
                id: "qwen3-coder-next-bailian-lp".to_string(),
                display_name: "qwen3-coder-next".to_string(),
                context_window: 262_144,
                max_output_tokens: 65_536,
                supports_image: false,
            },
            model::ModelItem {
                id: "qwen3-coder-plus-bailian-lp".to_string(),
                display_name: "qwen3-coder-plus".to_string(),
                context_window: 1_000_000,
                max_output_tokens: 65_536,
                supports_image: false,
            },
            model::ModelItem {
                id: "glm-4.7-bailian-lp".to_string(),
                display_name: "glm-4.7".to_string(),
                context_window: 202_752,
                max_output_tokens: 16_384,
                supports_image: false,
            },
            model::ModelItem {
                id: "qwen3.6-flash-bailian-lp".to_string(),
                display_name: "qwen3.6-flash".to_string(),
                context_window: 1_000_000,
                max_output_tokens: 65_536,
                supports_image: false,
            },
        ],
        "mimo-payg" | "mimo-token-plan-cn" | "mimo-token-plan-sgp" | "mimo-token-plan-ams" => {
            vec![model::ModelItem {
                id: "mimo-v2.5-pro-lp".to_string(),
                display_name: "mimo-v2.5-pro".to_string(),
                context_window: 1_048_576,
                max_output_tokens: 131_072,
                supports_image: false,
            }]
        }
        "zhipu-payg-cn" | "zhipu-coding-cn" => vec![model::ModelItem {
            id: "glm-5.2-zhipu-cn-lp".to_string(),
            display_name: "glm-5.2".to_string(),
            context_window: 128_000,
            max_output_tokens: 32_768,
            supports_image: false,
        }],
        "kimi-platform-global" | "kimi-platform-cn" => vec![model::ModelItem {
            id: "kimi-k2.6-lp".to_string(),
            display_name: "kimi-k2.6".to_string(),
            context_window: 256_000,
            max_output_tokens: 32_768,
            supports_image: false,
        }],
        "stepfun-payg" | "stepfun-step-plan" => vec![model::ModelItem {
            id: "step-3.7-flash-lp".to_string(),
            display_name: "step-3.7-flash".to_string(),
            context_window: 256_000,
            max_output_tokens: 32_768,
            supports_image: false,
        }],
        "openai-payg" => vec![model::ModelItem {
            id: "gpt-5.5-lp".to_string(),
            display_name: "gpt-5.5".to_string(),
            context_window: 400_000,
            max_output_tokens: 128_000,
            supports_image: false,
        }],
        "anthropic" => vec![model::ModelItem {
            id: "claude-sonnet-lp".to_string(),
            display_name: "claude-sonnet-4-8".to_string(),
            context_window: 200_000,
            max_output_tokens: 64_000,
            supports_image: false,
        }],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::{
        EntryMode, ModelItem, ModelSelectionState, ProductItem, ProviderAuthType, Screen,
    };
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn make_test_app() -> AppModel {
        let mut app = AppModel::new(PathBuf::from("/dev/null"), EntryMode::Connect);
        app.chosen_product = Some(ProductItem {
            id: "deepseek".to_string(),
            display_name: "DeepSeek".to_string(),
            auth_type: ProviderAuthType::ApiKey,
            endpoint_count: 1,
            product_kind: "payg".to_string(),
            is_custom: false,
            auth_status: None,
        });
        app.chosen_env_var = Some("DEEPSEEK_API_KEY".to_string());
        app.screen = Screen::ModelSelection(ModelSelectionState {
            items: vec![
                ModelItem {
                    id: "deepseek-v4-flash-lp".to_string(),
                    display_name: "deepseek-v4-flash".to_string(),
                    context_window: 1_000_000,
                    max_output_tokens: 393_216,
                    supports_image: false,
                },
                ModelItem {
                    id: "deepseek-v4-pro-lp".to_string(),
                    display_name: "deepseek-v4-pro".to_string(),
                    context_window: 1_000_000,
                    max_output_tokens: 393_216,
                    supports_image: false,
                },
            ],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            selected: HashSet::new(),
            configured: HashSet::new(),
            error: None,
            product_name: "DeepSeek".to_string(),
        });
        app
    }

    fn plain_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn confirm_model_selection_zero_selection_skips_to_verifying() {
        let mut app = make_test_app();
        // No models selected (selected set is empty)
        handle_model_keys(&mut app, plain_key(KeyCode::Enter));
        match &app.screen {
            Screen::Verifying(v) => {
                assert!(
                    v.models.is_empty(),
                    "zero selection should produce empty models"
                );
                assert_eq!(v.product_name, "DeepSeek");
            }
            other => panic!("expected Verifying screen, got {:?}", other),
        }
    }

    #[test]
    fn f_key_navigates_to_fallback_config() {
        let mut app = make_test_app();
        handle_model_keys(&mut app, plain_key(KeyCode::Char('F')));
        assert!(
            matches!(&app.screen, Screen::FallbackConfig(_)),
            "pressing F on ModelSelection should navigate to FallbackConfig"
        );
    }

    #[test]
    fn lowercase_f_key_also_navigates_to_fallback_config() {
        let mut app = make_test_app();
        handle_model_keys(&mut app, plain_key(KeyCode::Char('f')));
        assert!(
            matches!(&app.screen, Screen::FallbackConfig(_)),
            "pressing f on ModelSelection should also navigate to FallbackConfig"
        );
    }
}
