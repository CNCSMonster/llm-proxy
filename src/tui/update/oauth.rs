use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};

use super::model::{self, AppModel, ConfigResult, NameOption, Screen, WarningOption};
use crate::tui::update::env::make_env_state;
use crate::tui::update::products::make_product_state;

pub(crate) fn handle_warning_keys(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let (choice, back_to_product, force_local) =
                if let Screen::WarningConfirm(ref s) = app.screen {
                    (s.selected_option, s.back_to_product, s.force_local)
                } else {
                    return;
                };
            match choice {
                WarningOption::Continue => {
                    // If force_local is already true, we've already tried force write and failed.
                    // In that case, just go to Done with error state to break the infinite loop.
                    if force_local {
                        // Preserve the original error message for debuggability
                        let error_msg = if let Screen::WarningConfirm(ref s) = app.screen {
                            s.message.clone()
                        } else {
                            "Config save failed after retry.".to_string()
                        };
                        app.screen = Screen::Done(model::DoneState {
                            results: vec![ConfigResult {
                                provider: app
                                    .chosen_product
                                    .as_ref()
                                    .map(|p| p.id.clone())
                                    .unwrap_or_default(),
                                success: false,
                                message: error_msg,
                            }],
                        });
                    } else {
                        // First time: try force local write (skip server delegation)
                        app.screen = Screen::Verifying(model::VerifyingState {
                            product_name: app
                                .chosen_product
                                .as_ref()
                                .map(|p| p.display_name.clone())
                                .unwrap_or_default(),
                            env_var: app.chosen_env_var.clone(),
                            models: app.chosen_models.clone(),
                            force_local: true,
                        });
                    }
                }
                WarningOption::Back => {
                    if back_to_product {
                        app.screen = Screen::ProductSelection(make_product_state());
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
                    app.screen = Screen::OAuthDeviceCode(model::OAuthDeviceCodeState {
                        provider_name: name,
                        device_code: "XXXX-YYYY".to_string(),
                        verification_url: "https://device.login.openai.com/".to_string(),
                        expires_at: Instant::now() + std::time::Duration::from_secs(15 * 60),
                        copied: false,
                        polling: true,
                        retry_count: 0,
                        last_error: None,
                        error: None,
                    });
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
                    app.screen = Screen::OAuthDeviceCode(model::OAuthDeviceCodeState {
                        provider_name: state.1,
                        device_code: "XXXX-YYYY".to_string(),
                        verification_url: "https://device.login.openai.com/".to_string(),
                        expires_at: Instant::now() + std::time::Duration::from_secs(15 * 60),
                        copied: false,
                        polling: true,
                        retry_count: 0,
                        last_error: None,
                        error: None,
                    });
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
            KeyCode::Char('c') => {
                s.copied = true;
            }
            KeyCode::Char('s') => {
                app.screen = Screen::ProductSelection(make_product_state());
            }
            _ => {}
        }
    }
}

pub(crate) fn handle_antigravity_keys(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            // 提交 code：主循环检测到 submitted 后用保存的 verifier 完成非交互式 exchange
            if let Screen::AntigravityLogin(ref mut s) = app.screen {
                if s.input.trim().is_empty() {
                    s.error = Some("Authorization code cannot be empty".to_string());
                    s.submitted = false;
                } else {
                    s.submitted = true;
                }
            }
        }
        KeyCode::Char('c') => {
            if let Screen::AntigravityLogin(ref mut s) = app.screen {
                s.copied = true;
            }
        }
        KeyCode::Char(ch) => {
            if let Screen::AntigravityLogin(ref mut s) = app.screen {
                s.input.push(ch);
                // 重新输入时清除上次失败的 error，允许重试
                s.error = None;
                s.submitted = false;
            }
        }
        KeyCode::Backspace => {
            if let Screen::AntigravityLogin(ref mut s) = app.screen {
                s.input.pop();
                s.error = None;
                s.submitted = false;
            }
        }
        _ => {}
    }
}
