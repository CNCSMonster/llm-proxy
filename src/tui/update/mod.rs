use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};

use super::model::{self, AppModel, NameOption, Screen};
use crate::tui_control::{GlobalControlKey, global_control_key};

const DOUBLE_ESC_MS: u64 = 500;

fn handle_global_control_key(app: &mut AppModel, key: KeyEvent) -> bool {
    match global_control_key(key) {
        Some(GlobalControlKey::Interrupt) => {
            app.screen = Screen::Quit;
            true
        }
        Some(GlobalControlKey::Eof) => {
            let editing_with_content = matches!(
                &app.screen,
                Screen::ProviderNaming(state) if state.editing && !state.input.is_empty()
            ) || matches!(
                &app.screen,
                Screen::CopyConfigure(state)
                    if (state.step == model::CopyConfigureStep::Name
                        && state.editing
                        && !state.input.is_empty())
                        || (state.step == model::CopyConfigureStep::Auth
                            && state.env_editing
                            && !state.env_input.is_empty())
            ) || matches!(
                &app.screen,
                Screen::CustomProviderEditor(state)
                    if state.mode == model::EditorMode::Edit
                        && !state.current_field().value.is_empty()
            ) || matches!(
                &app.screen,
                Screen::OAuthName(state) if state.input_active && !state.input.is_empty()
            ) || matches!(
                &app.screen,
                Screen::ModelDetail(state) if state.editing && !state.edit_buffer.is_empty()
            ) || matches!(
                &app.screen,
                Screen::ProtocolBindingEditor(state)
                    if state.editing_upstream && !state.upstream_input.is_empty()
            ) || matches!(
                &app.screen,
                Screen::ModelRename(state) if state.editing && !state.new_model_id.is_empty()
            );
            if editing_with_content {
                return true;
            }
            if matches!(
                &app.screen,
                Screen::ProviderNaming(state) if state.editing
            ) || matches!(
                &app.screen,
                Screen::CopyConfigure(state) if state.editing || state.env_editing
            ) || matches!(
                &app.screen,
                Screen::CustomProviderEditor(state) if state.mode == model::EditorMode::Edit
            ) || matches!(
                &app.screen,
                Screen::OAuthName(state) if state.input_active
            ) || matches!(
                &app.screen,
                Screen::ModelDetail(state) if state.editing
            ) || matches!(
                &app.screen,
                Screen::ProtocolBindingEditor(state) if state.editing_upstream
            ) || matches!(
                &app.screen,
                Screen::ModelRename(state) if state.editing
            ) {
                handle_esc(app);
            } else {
                app.screen = Screen::Quit;
            }
            true
        }
        Some(GlobalControlKey::Ignored) => true,
        Some(GlobalControlKey::Suspend) | None => false,
    }
}

pub(crate) mod copy_configure;
pub(crate) mod editor;
pub(crate) mod env;
pub(crate) mod fallback;
pub(crate) mod model_mgmt;
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

    if handle_global_control_key(app, key) {
        return;
    }

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
        Screen::ProbeConfirm(_) => models::handle_probe_confirm_keys(app, key),
        Screen::CustomProviderEditor(_) => editor::handle_editor_input(app, key),
        Screen::ProviderNaming(_) => naming::handle_naming_keys(app, key),
        Screen::WarningConfirm(_) => oauth::handle_warning_keys(app, key),
        Screen::Verifying(_) => {}
        Screen::Done(_) => {
            if key.code == KeyCode::Enter {
                // If we entered from ProviderManagement, return there after adding
                if matches!(app.entry_mode, model::EntryMode::ProviderTui { .. }) {
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
                        scroll_offset: 0,
                        quota_fetched_at: None,
                    });
                } else {
                    app.screen = Screen::ProductSelection(make_product_state(&app.config_path));
                }
            }
        }
        Screen::OAuthName(_) => oauth::handle_oauth_name_keys(app, key),
        Screen::OAuthDeviceCode(_) => oauth::handle_oauth_device_keys(app, key),
        Screen::OAuthOverwrite(_) => oauth::handle_overwrite_keys(app, key),
        Screen::FallbackConfig(_) => fallback::handle_fallback_keys(app, key),
        Screen::CopyModelConfirm(_) => models::handle_copy_model_confirm_keys(app, key),
        Screen::CopyConfigure(_) => copy_configure::handle_copy_configure_keys(app, key),
        Screen::ModelRename(_) => model_rename::handle_model_rename_keys(app, key),
        Screen::ModelManagement(_) => model_mgmt::handle_model_management_keys(app, key),
        Screen::ModelDetail(_) => model_mgmt::handle_model_detail_keys(app, key),
        Screen::ModelDeleteConfirm(_) => model_mgmt::handle_model_delete_confirm_keys(app, key),
        Screen::ProtocolBindingEditor(_) => {
            model_mgmt::handle_protocol_binding_editor_keys(app, key)
        }
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
        app.screen = Screen::ProductSelection(make_product_state(&app.config_path));
        return;
    }

    // CopyConfigure: cancel current edit, then step, then return to the list.
    if let Screen::CopyConfigure(ref mut s) = app.screen {
        if s.env_editing {
            s.env_editing = false;
            s.env_input = s.source_api_key_env.clone().unwrap_or_default();
            s.env_cursor_pos = 0;
            s.error = None;
            return;
        }
        if s.editing {
            s.editing = false;
            s.input = s.default_name.clone();
            s.cursor_pos = 0;
            s.error = None;
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
        if s.step == model::CopyConfigureStep::Auth {
            s.step = model::CopyConfigureStep::Name;
            s.error = None;
            return;
        }
        let source = s.source_id.clone();
        provider_mgmt::return_to_provider_management_preserving_cursor(app, Some(&source));
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
                scroll_offset: 0,
                quota_fetched_at: None,
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
        // At step 0, not editing: return to the originating list/detail.
        if s.return_to_model_list {
            let model_id = s.old_model_id.clone();
            model_mgmt::return_to_model_management_preserving_cursor(app, Some(&model_id));
        } else {
            let provider_name = s.provider_name.clone();
            provider_mgmt::return_to_provider_management_preserving_cursor(
                app,
                Some(&provider_name),
            );
        }
        return;
    }

    if let Screen::ModelDetail(ref mut s) = app.screen {
        if s.editing {
            s.editing = false;
            s.edit_buffer.clear();
            s.error = None;
            return;
        }
        if s.is_draft() {
            model_mgmt::return_to_model_management_preserving_cursor(app, None);
        } else {
            let id = s.model_id.clone();
            model_mgmt::return_to_model_management_preserving_cursor(app, Some(&id));
        }
        return;
    }

    if let Screen::ProtocolBindingEditor(ref mut s) = app.screen {
        if s.editing_upstream {
            s.editing_upstream = false;
            s.error = None;
            return;
        }
        if s.adding {
            s.adding = false;
            s.error = None;
            return;
        }
        if s.remove_confirm {
            s.remove_confirm = false;
            s.error = None;
            return;
        }
        let model_id = s.model_id.clone();
        app.screen = Screen::ModelDetail(model::build_model_detail(&app.config_path, &model_id));
        return;
    }

    let now = Instant::now();
    if let Some(last) = app.last_esc
        && now.duration_since(last).as_millis() < DOUBLE_ESC_MS as u128
    {
        app.pending_account = None;
        app.screen = Screen::Quit;
        return;
    }
    app.last_esc = Some(now);
    app.esc_hint_visible = true;

    // Go back to parent screen — clone to avoid borrow
    // 探测确认屏的返回目标要提前取（返回目标是缓存的 ModelSelection）：
    // 它与 `match &app.screen` 的借用不能重叠。
    let probe_back = app
        .cached_model_selection
        .take()
        .map(Screen::ModelSelection);
    let new_screen = match &app.screen {
        Screen::ProviderManagement(_)
        | Screen::ModelManagement(_)
        | Screen::ProductSelection(_)
        | Screen::Quit => Screen::Quit,
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
        Screen::EnvVarSelection(_) => {
            Screen::ProductSelection(make_product_state(&app.config_path))
        }
        Screen::ModelSelection(state) => {
            if state.is_openrouter {
                if let Some(h) = app.openrouter_session.in_flight.take() {
                    h.abort();
                }
                app.openrouter_session.request_token += 1;
                app.openrouter_session.loading = false;
            }
            match &state.source {
                model::ModelSelectionSource::Connect => {
                    Screen::EnvVarSelection(env::make_env_state(app))
                }
                model::ModelSelectionSource::ProviderReselect { provider_name } => {
                    let provider_name = provider_name.clone();
                    provider_mgmt::return_to_provider_management_preserving_cursor(
                        app,
                        Some(&provider_name),
                    );
                    return;
                }
            }
        }
        Screen::ProbeConfirm(_) => {
            probe_back.unwrap_or_else(|| Screen::EnvVarSelection(env::make_env_state(app)))
        }
        Screen::CustomProviderEditor(_) => {
            Screen::ProductSelection(make_product_state(&app.config_path))
        }
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
                        skip_probe: false,
                        progress_done: 0,
                        progress_total: 0,
                        progress_current_model: None,
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
        Screen::Done(_) => Screen::ProductSelection(make_product_state(&app.config_path)),
        Screen::OAuthName(_) => Screen::ProductSelection(make_product_state(&app.config_path)),
        Screen::OAuthDeviceCode(_) => Screen::OAuthName(model::OAuthNameState {
            recommended_name: "openai-subscription".to_string(),
            selected_option: NameOption::Recommended,
            input: String::new(),
            input_active: false,
            error: None,
        }),
        Screen::OAuthOverwrite(_) => Screen::OAuthName(model::OAuthNameState {
            recommended_name: "openai-subscription".to_string(),
            selected_option: NameOption::Recommended,
            input: String::new(),
            input_active: false,
            error: None,
        }),
        // ProviderNaming / CopyConfigure / Model TUI editors are handled above
        Screen::ProviderNaming(_)
        | Screen::CopyConfigure(_)
        | Screen::ModelDetail(_)
        | Screen::ProtocolBindingEditor(_) => unreachable!(),
        Screen::ModelDeleteConfirm(state) => {
            let id = state.model_id.clone();
            model_mgmt::return_to_model_management_preserving_cursor(app, Some(&id));
            return;
        }
        Screen::CopyModelConfirm(_) | Screen::ModelRename(_) => Screen::Quit,
    };
    if matches!(new_screen, Screen::ProductSelection(_) | Screen::Quit) {
        app.pending_account = None;
    }
    app.screen = new_screen;
}

fn handle_q(app: &mut AppModel) {
    app.pending_account = None;
    let should_quit = matches!(
        &app.screen,
        Screen::ProductSelection(_) | Screen::ProviderManagement(_) | Screen::ModelManagement(_)
    ) || matches!(app.entry_mode, model::EntryMode::ModelTui { .. });
    if should_quit {
        app.screen = Screen::Quit;
    } else {
        app.screen = Screen::ProductSelection(make_product_state(&app.config_path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::EntryMode;
    use crossterm::event::{KeyEventKind, KeyModifiers};

    fn app_with_screen(screen: Screen) -> AppModel {
        let mut app = AppModel::new(
            std::path::PathBuf::from("/nonexistent/config.toml"),
            EntryMode::Connect,
        );
        app.screen = screen;
        app
    }

    #[test]
    fn ctrl_c_quits_from_any_screen() {
        let config_path = std::path::PathBuf::from("/nonexistent/config.toml");
        let mut app = app_with_screen(Screen::ProductSelection(make_product_state(&config_path)));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(matches!(app.screen, Screen::Quit));
    }

    #[test]
    fn ctrl_d_quits_when_not_editing() {
        let config_path = std::path::PathBuf::from("/nonexistent/config.toml");
        let mut app = app_with_screen(Screen::ProductSelection(make_product_state(&config_path)));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert!(matches!(app.screen, Screen::Quit));
    }

    #[test]
    fn control_key_release_and_repeat_do_not_trigger_page_actions() {
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            let detail = crate::tui::model::ProviderDetailState {
                name: "deepseek".to_string(),
                product: "deepseek".to_string(),
                auth_description: "API Key".to_string(),
                auth_status: "✓ 已配置".to_string(),
                endpoints: vec![],
                bound_models: vec![],
                compat_info: vec![],
                model_cursor: 0,
                scroll_offset: 0,
                quota_fetched_at: None,
            };
            let mut app = app_with_screen(Screen::ProviderDetail(detail));

            handle_key(
                &mut app,
                KeyEvent::new_with_kind(KeyCode::Char('d'), KeyModifiers::CONTROL, kind),
            );

            assert!(matches!(app.screen, Screen::ProviderDetail(_)));
        }
    }

    #[test]
    fn esc_from_detail_and_model_reselection_returns_to_provider_list_with_cursor() {
        let config_path = tempfile::NamedTempFile::new().expect("temp config");
        std::fs::write(
            config_path.path(),
            r#"
[server]
listen = "127.0.0.1:8989"

[providers.alpha]
product = "deepseek"
openai_chat = { url = "https://example.test/alpha" }
api_key_env = "ALPHA_API_KEY"

[providers.beta]
product = "deepseek"
openai_chat = { url = "https://example.test/beta" }
api_key_env = "BETA_API_KEY"
"#,
        )
        .expect("write config");
        let detail = crate::tui::model::ProviderDetailState {
            name: "beta".to_string(),
            product: "deepseek".to_string(),
            auth_description: "API Key".to_string(),
            auth_status: "✓ 已配置".to_string(),
            endpoints: vec![],
            bound_models: vec![],
            compat_info: vec![],
            model_cursor: 0,
            scroll_offset: 0,
            quota_fetched_at: None,
        };
        let mut app = AppModel::new(
            config_path.path().to_path_buf(),
            EntryMode::ProviderTui { select: None },
        );
        app.screen = Screen::ProviderDetail(detail);

        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        let Screen::ProviderManagement(state) = app.screen else {
            panic!("expected provider management");
        };
        assert_eq!(
            state.selected_provider().map(|provider| &provider.name),
            Some(&"beta".to_string())
        );

        let detail = crate::tui::model::ProviderDetailState {
            name: "beta".to_string(),
            product: "deepseek".to_string(),
            auth_description: "API Key".to_string(),
            auth_status: "✓ 已配置".to_string(),
            endpoints: vec![],
            bound_models: vec![],
            compat_info: vec![],
            model_cursor: 0,
            scroll_offset: 0,
            quota_fetched_at: None,
        };
        let mut app = AppModel::new(
            config_path.path().to_path_buf(),
            EntryMode::ProviderTui { select: None },
        );
        app.screen = Screen::ProviderDetail(detail);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        );
        assert!(matches!(app.screen, Screen::ModelSelection(_)));

        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        let Screen::ProviderManagement(state) = app.screen else {
            panic!("expected provider management");
        };
        assert_eq!(
            state.selected_provider().map(|provider| &provider.name),
            Some(&"beta".to_string())
        );
    }

    #[test]
    fn q_from_model_tui_detail_quits_instead_of_connect() {
        let mut app = AppModel::new(
            std::path::PathBuf::from("/nonexistent/config.toml"),
            EntryMode::ModelTui { select: None },
        );
        app.screen = Screen::ModelDetail(crate::tui::model::build_add_model_detail());
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert!(
            matches!(app.screen, Screen::Quit),
            "bare model TUI must not fall back to connect: {:?}",
            std::mem::discriminant(&app.screen)
        );
    }
}
