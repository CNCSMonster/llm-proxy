use crossterm::event::{KeyCode, KeyEvent};

use super::model::{self, AppModel, NameOption, Screen, WarningOption};
use crate::tui::update::env::make_env_state;
use crate::tui::update::products::make_product_state;

pub(crate) fn handle_warning_keys(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let (choice, back_to_product) = if let Screen::WarningConfirm(ref s) = app.screen {
                (s.selected_option, s.back_to_product)
            } else {
                return;
            };
            match choice {
                WarningOption::Continue => {
                    // Retry with skip_probe = true, keeping pending_account intact
                    app.screen = Screen::Verifying(model::VerifyingState {
                        product_name: app
                            .chosen_product
                            .as_ref()
                            .map(|p| p.display_name.clone())
                            .unwrap_or_default(),
                        env_var: app.chosen_env_var.clone(),
                        models: app.chosen_models.clone(),
                        skip_probe: true,
                        progress_done: 0,
                        progress_total: 0,
                        progress_current_model: None,
                    });
                }
                WarningOption::Back => {
                    // Retain pending_account in AppModel and return to editing screen
                    if back_to_product {
                        app.screen = Screen::ProductSelection(make_product_state(&app.config_path));
                    } else if app.chosen_product.as_ref().is_some_and(|p| {
                        !crate::tui::update::models::get_model_templates(Some(&p.id)).is_empty()
                    }) {
                        let product = app.chosen_product.as_ref().unwrap();
                        let models =
                            crate::tui::update::models::get_model_templates(Some(&product.id));
                        let configured =
                            model::build_configured_set(&app.config_path, &product.id, &models);
                        let selected = app.chosen_models.iter().cloned().collect();
                        app.screen = Screen::ModelSelection(model::ModelSelectionState {
                            items: models,
                            cursor: 0,
                            filter: String::new(),
                            filter_active: false,
                            selected,
                            configured,
                            error: None,
                            product_name: product.display_name.clone(),
                            source: model::ModelSelectionSource::Connect,
                            is_openrouter: false,
                            is_degraded: false,
                            openrouter_cached_at: None,
                        });
                    } else {
                        app.screen = Screen::EnvVarSelection(make_env_state(app));
                    }
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::WarningConfirm(ref mut s) = app.screen {
                s.selected_option = WarningOption::Continue;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::WarningConfirm(ref mut s) = app.screen {
                s.selected_option = WarningOption::Back;
            }
        }
        _ => {}
    }
}

pub(crate) fn handle_overwrite_keys(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let choice = if let Screen::OAuthOverwrite(ref s) = app.screen {
                s.selected_option
            } else {
                return;
            };
            match choice {
                WarningOption::Continue => {
                    let name = app.oauth_provider_name.clone().unwrap_or_default();
                    // 空码 + 未轮询：主循环据此立即挂起 TUI 触发真实 CLI 登录
                    app.screen =
                        Screen::OAuthDeviceCode(model::OAuthDeviceCodeState::pending_login(name));
                }
                WarningOption::Back => {
                    app.screen = Screen::OAuthName(model::OAuthNameState {
                        recommended_name: "openai-subscription".to_string(),
                        selected_option: NameOption::Recommended,
                        input: String::new(),
                        input_active: false,
                        error: None,
                    });
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::OAuthOverwrite(ref mut s) = app.screen {
                s.selected_option = WarningOption::Continue;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::OAuthOverwrite(ref mut s) = app.screen {
                s.selected_option = WarningOption::Back;
            }
        }
        _ => {}
    }
}

pub(crate) fn handle_oauth_name_keys(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let state = if let Screen::OAuthName(ref s) = app.screen {
                (s.selected_option, s.recommended_name.clone())
            } else {
                return;
            };
            match state.0 {
                NameOption::Recommended => {
                    // 空码 + 未轮询：主循环据此立即挂起 TUI 触发真实 CLI 登录
                    app.screen = Screen::OAuthDeviceCode(
                        model::OAuthDeviceCodeState::pending_login(state.1),
                    );
                }
                NameOption::Custom => {
                    if let Screen::OAuthName(ref mut s) = app.screen {
                        s.input_active = true;
                    }
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::OAuthName(ref mut s) = app.screen {
                s.selected_option = NameOption::Recommended;
                s.input_active = false;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::OAuthName(ref mut s) = app.screen {
                s.selected_option = NameOption::Custom;
            }
        }
        _ => {}
    }

    // Handle text input for custom name
    if let Screen::OAuthName(ref mut s) = app.screen
        && s.selected_option == NameOption::Custom
        && s.input_active
    {
        match key.code {
            KeyCode::Char(ch) if ch.is_alphanumeric() || ch == '-' || ch == '_' => {
                s.input.push(ch);
            }
            KeyCode::Backspace => {
                s.input.pop();
            }
            _ => {}
        }
    }
}

pub(crate) fn handle_oauth_device_keys(app: &mut AppModel, _key: KeyEvent) {
    if let Screen::OAuthDeviceCode(ref mut s) = app.screen {
        match _key.code {
            // 失败后重试：清错并复位 polling，主循环据此重新挂起 TUI 触发 CLI 登录
            KeyCode::Char('r') | KeyCode::Enter => {
                if s.error.is_some() {
                    s.error = None;
                    s.polling = false;
                }
            }
            KeyCode::Char('s') => {
                app.screen = Screen::ProductSelection(make_product_state(&app.config_path));
            }
            _ => {}
        }
    }
}
