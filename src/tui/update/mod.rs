use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};

use super::model::{self, AppModel, NameOption, Screen};

const DOUBLE_ESC_MS: u64 = 500;

pub(crate) mod editor;
pub(crate) mod env;
pub(crate) mod fallback;
pub(crate) mod model_rename;
pub(crate) mod models;
pub(crate) mod naming;
pub(crate) mod oauth;
pub(crate) mod products;
pub(crate) mod provider_mgmt;
pub(crate) mod utils;

use products::make_product_state;

/// Main key handler. Checks global keys first, then dispatches per-screen.
pub fn handle_key(app: &mut AppModel, key: KeyEvent) {
    app.esc_hint_visible = false;

    // Global keys
    match key.code {
        KeyCode::Esc => {
            handle_esc(app);
            return;
        }
        KeyCode::Char('q') => {
            handle_q(app);
            return;
        }
        _ => {}
    }

    // Clone screen to avoid holding a borrow across mutations.
    let screen = app.screen.clone();
    match screen {
        Screen::ProviderManagement(_) => provider_mgmt::handle_provider_management_keys(app, key),
        Screen::ProviderDetail(_) => provider_mgmt::handle_provider_detail_keys(app, key),
        Screen::DeleteConfirm(_) => provider_mgmt::handle_delete_confirm_keys(app, key),
        Screen::ResetUsageConfirm(_) => provider_mgmt::handle_reset_usage_confirm_keys(app, key),
        Screen::ProductSelection(_) => products::handle_product_keys(app, key),
        Screen::EnvVarSelection(_) => env::handle_env_keys(app, key),
        Screen::ModelSelection(_) => models::handle_model_keys(app, key),
        Screen::CustomProviderEditor(_) => editor::handle_editor_input(app, key),
        Screen::ProviderNaming(_) => naming::handle_naming_keys(app, key),
        Screen::WarningConfirm(_) => oauth::handle_warning_keys(app, key),
        Screen::Verifying(_) => {}
        Screen::Done(_) => {
            if key.code == KeyCode::Enter {
                // If we entered from ProviderManagement, return there after adding
                if app.entry_mode == model::EntryMode::ProviderTui {
                    let providers = crate::tui::model::load_configured_providers(&app.config_path);
                    // Try to keep cursor on the same provider
                    let new_cursor = app
                        .oauth_provider_name
                        .as_ref()
                        .and_then(|name| providers.iter().position(|p| p.name == *name))
                        .unwrap_or(0);
                    app.screen = Screen::ProviderManagement(model::ProviderManagementState {
                        providers,
                        cursor: new_cursor,
                        error: None,
                        filter: String::new(),
                        filter_active: false,
                    });
                } else {
                    app.screen = Screen::ProductSelection(make_product_state());
                }
            }
        }
        Screen::OAuthName(_) => oauth::handle_oauth_name_keys(app, key),
        Screen::OAuthDeviceCode(_) => oauth::handle_oauth_device_keys(app, key),
        Screen::AntigravityLogin(_) => oauth::handle_antigravity_keys(app, key),
        Screen::OAuthOverwrite(_) => oauth::handle_overwrite_keys(app, key),
        Screen::FallbackConfig(_) => fallback::handle_fallback_keys(app, key),
        Screen::CopyModelConfirm(_) => models::handle_copy_model_confirm_keys(app, key),
        Screen::ModelRename(_) => model_rename::handle_model_rename_keys(app, key),
        Screen::Quit => {}
    }
}

fn handle_esc(app: &mut AppModel) {
    // ProviderNaming: if editing, exit editing mode (revert to default);
    // if not editing, go back to product selection.
    if let Screen::ProviderNaming(ref mut s) = app.screen {
        if s.editing {
            s.editing = false;
            s.input = s.default_name.clone();
            s.cursor_pos = 0;
            let providers = crate::config::Config::load(&app.config_path)
                .map(|cfg| cfg.providers)
                .unwrap_or_default();
            s.validation = if crate::tui::model::is_provider_name_taken(&providers, &s.input) {
                model::NameValidation::Conflict
            } else {
                model::NameValidation::Available
            };
            return;
        }
        // Not editing: fall through to go back to product selection
        app.screen = Screen::ProductSelection(make_product_state());
        return;
    }

    // CopyModelConfirm: if editing, exit editing mode (revert to default);
    // if not editing, go back to ModelSelection (via cached state) or ProviderManagement.
    if let Screen::CopyModelConfirm(ref mut s) = app.screen {
        if s.editing {
            s.editing = false;
            s.new_id = s.default_new_id.clone();
            s.cursor_pos = 0;
            s.error = None;
            return;
        }
        // Not editing: restore cached ModelSelection or fall back to ProviderManagement
        if let Some(cached) = app.cached_model_selection.take() {
            app.screen = Screen::ModelSelection(cached);
        } else {
            let providers = model::load_configured_providers(&app.config_path);
            app.screen = Screen::ProviderManagement(model::ProviderManagementState {
                providers,
                cursor: 0,
                error: None,
                filter: String::new(),
                filter_active: false,
            });
        }
        return;
    }

    // ModelRename: if editing, exit editing mode; if confirming, go back to step 0;
    // if at step 0 and not editing, return to ProviderDetail.
    if let Screen::ModelRename(ref mut s) = app.screen {
        if s.editing {
            s.editing = false;
            s.cursor_pos = 0;
            return;
        }
        if s.confirm_step == 1 {
            s.confirm_step = 0;
            s.error = None;
            return;
        }
        // At step 0, not editing: return to ProviderDetail
        let provider_name = s.provider_name.clone();
        provider_mgmt::return_to_provider_management_preserving_cursor(app, Some(&provider_name));
        return;
    }

    let now = Instant::now();
    if let Some(last) = app.last_esc
        && now.duration_since(last).as_millis() < DOUBLE_ESC_MS as u128
    {
        app.screen = Screen::Quit;
        return;
    }
    app.last_esc = Some(now);
    app.esc_hint_visible = true;

    // Go back to parent screen — clone to avoid borrow
    let new_screen = match &app.screen {
        Screen::ProviderManagement(_) | Screen::ProductSelection(_) | Screen::Quit => Screen::Quit,
        Screen::ProviderDetail(state) => {
            // Return to provider management panel, preserving cursor on current provider
            let name = state.name.clone();
            let _ = state;
            provider_mgmt::return_to_provider_management_preserving_cursor(app, Some(&name));
            return;
        }
        Screen::DeleteConfirm(state) => {
            // Return to provider management panel, preserving cursor on the provider
            let name = state.provider_name.clone();
            let _ = state;
            provider_mgmt::return_to_provider_management_preserving_cursor(app, Some(&name));
            return;
        }
        Screen::ResetUsageConfirm(state) => {
            // Return to provider management panel, preserving cursor on the provider
            let name = state.provider_name.clone();
            let _ = state;
            provider_mgmt::return_to_provider_management_preserving_cursor(app, Some(&name));
            return;
        }
        Screen::EnvVarSelection(_) => Screen::ProductSelection(make_product_state()),
        Screen::ModelSelection(_) => Screen::EnvVarSelection(env::make_env_state(app)),
        Screen::CustomProviderEditor(_) => Screen::ProductSelection(make_product_state()),
        Screen::WarningConfirm(_) | Screen::Verifying(_) => {
            Screen::EnvVarSelection(env::make_env_state(app))
        }
        Screen::FallbackConfig(s) => {
            match s.source {
                model::FallbackSource::ConnectFlow => {
                    // Esc on fallback config from connect flow: skip fallback and proceed to verifying
                    let product_name = app
                        .chosen_product
                        .as_ref()
                        .map(|p| p.display_name.clone())
                        .unwrap_or_default();
                    app.screen = Screen::Verifying(model::VerifyingState {
                        product_name,
                        env_var: app.chosen_env_var.clone(),
                        models: vec![],
                        force_local: false,
                    });
                    return; // early return since we already set app.screen
                }
                model::FallbackSource::ProviderManagement { ref provider_name } => {
                    // Esc from provider management: return to provider list, preserving cursor
                    let name = provider_name.clone();
                    provider_mgmt::return_to_provider_management_preserving_cursor(
                        app,
                        Some(&name),
                    );
                    return;
                }
            }
        }
        Screen::Done(_) => Screen::ProductSelection(make_product_state()),
        Screen::OAuthName(_) => Screen::ProductSelection(make_product_state()),
        Screen::OAuthDeviceCode(_) | Screen::AntigravityLogin(_) => {
            Screen::OAuthName(model::OAuthNameState {
                recommended_name: "openai-subscription".to_string(),
                selected_option: NameOption::Recommended,
                input: String::new(),
                input_active: false,
                error: None,
            })
        }
        Screen::OAuthOverwrite(_) => Screen::OAuthName(model::OAuthNameState {
            recommended_name: "openai-subscription".to_string(),
            selected_option: NameOption::Recommended,
            input: String::new(),
            input_active: false,
            error: None,
        }),
        // ProviderNaming is handled by the early return above
        Screen::ProviderNaming(_) => unreachable!(),
        Screen::CopyModelConfirm(_) | Screen::ModelRename(_) => Screen::Quit,
    };
    app.screen = new_screen;
}

fn handle_q(app: &mut AppModel) {
    let should_quit = matches!(
        &app.screen,
        Screen::ProductSelection(_) | Screen::ProviderManagement(_)
    );
    if should_quit {
        app.screen = Screen::Quit;
    } else {
        app.screen = Screen::ProductSelection(make_product_state());
    }
}
