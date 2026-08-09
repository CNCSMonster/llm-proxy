use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::model::{self, AppModel, ProviderAuthType, Screen};
use crate::tui::update::utils::handle_filter_input;

fn select_product(app: &mut AppModel, item: super::model::ProductItem, product_name: String) {
    if item.is_custom {
        app.screen = Screen::CustomProviderEditor(model::CustomProviderEditorState::new());
        return;
    }

    // Check if this product already has a provider configured → show naming screen
    {
        let providers = crate::config::Config::load(&app.config_path)
            .map(|cfg| cfg.providers)
            .unwrap_or_default();
        if model::product_already_configured(&providers, &item.id) {
            app.screen = Screen::ProviderNaming(model::build_naming_state(&app.config_path, item));
            return;
        }
    }

    select_product_inner(app, item, product_name);
}

/// Continue the connect flow after the naming screen confirms a name.
/// This is the same logic as `select_product` but skips the naming screen check
/// (we already know the product is configured).
pub(crate) fn select_product_after_naming(
    app: &mut AppModel,
    item: super::model::ProductItem,
    product_name: String,
) {
    select_product_inner(app, item, product_name);
}

/// Core connect flow logic (shared between first-time and post-naming paths).
fn select_product_inner(app: &mut AppModel, item: super::model::ProductItem, product_name: String) {
    // OAuth provider：直接进入对应的 OAuth 登录屏幕
    match &item.auth_type {
        ProviderAuthType::OpenaiOauth | ProviderAuthType::AntigravityOauth => {
            app.chosen_product = Some(item.clone());
            let provider_name = app
                .oauth_provider_name
                .clone()
                .unwrap_or_else(|| item.id.clone());
            match &item.auth_type {
                ProviderAuthType::OpenaiOauth => {
                    app.screen = Screen::OAuthDeviceCode(model::OAuthDeviceCodeState {
                        provider_name,
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
                ProviderAuthType::AntigravityOauth => {
                    app.screen =
                        Screen::AntigravityLogin(model::AntigravityLoginState::new(provider_name));
                }
                _ => unreachable!(),
            }
            return;
        }
        // No auth required (e.g. Ollama): skip env var selection, go directly to model selection
        ProviderAuthType::None => {
            app.chosen_product = Some(item);
            app.chosen_env_var = None;
            let models = super::models::get_model_templates(
                app.chosen_product.as_ref().map(|p| p.id.as_str()),
            );
            if models.is_empty() {
                app.screen = Screen::Verifying(model::VerifyingState {
                    product_name,
                    env_var: None,
                    models: vec![],
                    force_local: false,
                });
            } else {
                let configured = if let Some(ref product) = app.chosen_product {
                    model::build_configured_set(&app.config_path, &product.id, &models)
                } else {
                    std::collections::HashSet::new()
                };
                app.screen = Screen::ModelSelection(model::ModelSelectionState {
                    items: models,
                    cursor: 0,
                    filter: String::new(),
                    filter_active: false,
                    selected: std::collections::HashSet::new(),
                    configured,
                    error: None,
                    product_name,
                });
            }
            return;
        }
        _ => {}
    }

    // API Key provider: 正常流程，选择环境变量
    app.chosen_product = Some(item);
    let env_state = super::env::make_env_state(app);
    app.screen = Screen::EnvVarSelection(env_state);
}
pub(crate) fn make_product_state() -> model::ProductSelectionState {
    model::ProductSelectionState {
        items: model::build_product_list(),
        cursor: 0,
        filter: String::new(),
        filter_active: false,
        error: None,
    }
}

pub(crate) fn handle_product_keys(app: &mut AppModel, key: KeyEvent) {
    // If filter is active, only handle filter keys and Enter
    {
        let filter_active = if let Screen::ProductSelection(ref s) = app.screen {
            s.filter_active
        } else {
            false
        };
        if filter_active {
            match key.code {
                KeyCode::Esc => {
                    if let Screen::ProductSelection(ref mut s) = app.screen {
                        s.deactivate_filter();
                    }
                    return;
                }
                KeyCode::Enter => {
                    // Confirm with filter active — select current item
                    let (item, product_name) = {
                        if let Screen::ProductSelection(ref s) = app.screen {
                            let fi = s.filtered_item();
                            let name = fi
                                .as_ref()
                                .map(|i| i.display_name.clone())
                                .unwrap_or_default();
                            (fi, name)
                        } else {
                            return;
                        }
                    };
                    if let Some(item) = item {
                        select_product(app, item.clone(), product_name);
                    }
                    return;
                }
                KeyCode::Up | KeyCode::Char('k')
                    if key.code == KeyCode::Up || key.modifiers.is_empty() =>
                {
                    if let Screen::ProductSelection(ref mut s) = app.screen {
                        s.move_up();
                    }
                    return;
                }
                KeyCode::Down | KeyCode::Char('j')
                    if key.code == KeyCode::Down || key.modifiers.is_empty() =>
                {
                    if let Screen::ProductSelection(ref mut s) = app.screen {
                        s.move_down();
                    }
                    return;
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Screen::ProductSelection(ref mut s) = app.screen {
                        s.move_up();
                    }
                    return;
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Screen::ProductSelection(ref mut s) = app.screen {
                        s.move_down();
                    }
                    return;
                }
                _ => {
                    if let Screen::ProductSelection(ref mut s) = app.screen {
                        handle_filter_input(&mut s.filter, key);
                    }
                    return;
                }
            }
        }
    }

    // Normal mode — only / toggles filter, others behave normally
    match key.code {
        KeyCode::Enter => {
            let (item, product_name) = {
                if let Screen::ProductSelection(ref s) = app.screen {
                    let fi = s.filtered_item();
                    let name = fi
                        .as_ref()
                        .map(|i| i.display_name.clone())
                        .unwrap_or_default();
                    (fi, name)
                } else {
                    return;
                }
            };
            if let Some(item) = item {
                select_product(app, item.clone(), product_name);
            }
        }
        KeyCode::Char('/') => {
            if let Screen::ProductSelection(ref mut s) = app.screen {
                s.activate_filter();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::ProductSelection(ref mut s) = app.screen {
                s.move_up();
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ProductSelection(ref mut s) = app.screen {
                s.move_down();
            }
        }
        _ => {}
    }
}
