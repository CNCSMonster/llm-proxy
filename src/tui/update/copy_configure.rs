use crossterm::event::{KeyCode, KeyEvent};

use super::model::{self, AppModel, NameValidation, Screen};
use crate::tui::model::{CopyAuthKind, CopyConfigureState, CopyConfigureStep};

/// Handle keyboard input on the CopyConfigure screen.
///
/// Esc is handled globally in `handle_esc`.
pub(crate) fn handle_copy_configure_keys(app: &mut AppModel, key: KeyEvent) {
    let step = if let Screen::CopyConfigure(ref s) = app.screen {
        s.step
    } else {
        return;
    };

    match step {
        CopyConfigureStep::Name => handle_name_step(app, key),
        CopyConfigureStep::Auth => handle_auth_step(app, key),
    }
}

fn handle_name_step(app: &mut AppModel, key: KeyEvent) {
    let editing = if let Screen::CopyConfigure(ref s) = app.screen {
        s.editing
    } else {
        return;
    };

    if editing {
        handle_name_editing(app, key);
    } else {
        match key.code {
            KeyCode::Char('e') => {
                if let Screen::CopyConfigure(ref mut s) = app.screen {
                    s.editing = true;
                    s.cursor_pos = s.input.len();
                    s.error = None;
                }
            }
            KeyCode::Enter => advance_from_name(app),
            _ => {}
        }
    }
}

fn handle_name_editing(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            if let Screen::CopyConfigure(ref mut s) = app.screen {
                s.editing = false;
            }
            advance_from_name(app);
        }
        KeyCode::Char(ch) if ch.is_alphanumeric() || ch == '-' || ch == '_' => {
            if let Screen::CopyConfigure(ref mut s) = app.screen {
                if s.cursor_pos >= s.input.len() {
                    s.input.push(ch);
                } else {
                    s.input.insert(s.cursor_pos, ch);
                }
                s.cursor_pos += 1;
                validate_copy_name(s, &app.config_path);
                s.error = None;
            }
        }
        KeyCode::Backspace => {
            if let Screen::CopyConfigure(ref mut s) = app.screen
                && s.cursor_pos > 0
            {
                s.cursor_pos -= 1;
                s.input.remove(s.cursor_pos);
                validate_copy_name(s, &app.config_path);
                s.error = None;
            }
        }
        KeyCode::Left => {
            if let Screen::CopyConfigure(ref mut s) = app.screen {
                s.cursor_pos = s.cursor_pos.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if let Screen::CopyConfigure(ref mut s) = app.screen
                && s.cursor_pos < s.input.len()
            {
                s.cursor_pos += 1;
            }
        }
        _ => {}
    }
}

fn handle_auth_step(app: &mut AppModel, key: KeyEvent) {
    let (auth_kind, env_editing) = if let Screen::CopyConfigure(ref s) = app.screen {
        (s.auth_kind, s.env_editing)
    } else {
        return;
    };

    if env_editing {
        handle_env_editing(app, key);
        return;
    }

    match key.code {
        KeyCode::Char('e') if auth_kind == CopyAuthKind::ApiKey => {
            if let Screen::CopyConfigure(ref mut s) = app.screen {
                s.env_editing = true;
                s.env_cursor_pos = s.env_input.len();
                s.error = None;
            }
        }
        KeyCode::Char('s') if auth_kind == CopyAuthKind::ApiKey => {
            confirm_copy_provider(app, None, true);
        }
        KeyCode::Enter => match auth_kind {
            CopyAuthKind::ApiKey => {
                let env = if let Screen::CopyConfigure(ref s) = app.screen {
                    s.env_input.trim().to_string()
                } else {
                    return;
                };
                if env.is_empty() {
                    if let Screen::CopyConfigure(ref mut s) = app.screen {
                        s.error = Some("请输入环境变量名，或按 [s] 复制为无密钥".to_string());
                        s.env_editing = true;
                        s.env_cursor_pos = s.env_input.len();
                    }
                    return;
                }
                confirm_copy_provider(app, Some(env), false);
            }
            CopyAuthKind::None => confirm_copy_provider(app, None, false),
            CopyAuthKind::OpenaiOauth => {
                confirm_copy_provider(app, None, false);
            }
        },
        _ => {}
    }
}

fn handle_env_editing(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            if let Screen::CopyConfigure(ref mut s) = app.screen {
                s.env_editing = false;
            }
            handle_auth_step(
                app,
                KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
            );
        }
        KeyCode::Char(ch) if is_env_char(ch) => {
            if let Screen::CopyConfigure(ref mut s) = app.screen {
                if s.env_cursor_pos >= s.env_input.len() {
                    s.env_input.push(ch);
                } else {
                    s.env_input.insert(s.env_cursor_pos, ch);
                }
                s.env_cursor_pos += 1;
                s.error = None;
            }
        }
        KeyCode::Backspace => {
            if let Screen::CopyConfigure(ref mut s) = app.screen
                && s.env_cursor_pos > 0
            {
                s.env_cursor_pos -= 1;
                s.env_input.remove(s.env_cursor_pos);
                s.error = None;
            }
        }
        KeyCode::Left => {
            if let Screen::CopyConfigure(ref mut s) = app.screen {
                s.env_cursor_pos = s.env_cursor_pos.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if let Screen::CopyConfigure(ref mut s) = app.screen
                && s.env_cursor_pos < s.env_input.len()
            {
                s.env_cursor_pos += 1;
            }
        }
        _ => {}
    }
}

fn is_env_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn validate_copy_name(s: &mut CopyConfigureState, config_path: &std::path::Path) {
    let providers = crate::config::Config::load(config_path)
        .map(|cfg| cfg.providers)
        .unwrap_or_default();
    s.validation = if crate::tui::model::is_provider_name_taken(&providers, &s.input) {
        NameValidation::Conflict
    } else {
        NameValidation::Available
    };
}

fn advance_from_name(app: &mut AppModel) {
    let (input, validation, auth_kind) = if let Screen::CopyConfigure(ref s) = app.screen {
        (s.input.clone(), s.validation.clone(), s.auth_kind)
    } else {
        return;
    };

    if input.trim().is_empty() || validation == NameValidation::Conflict {
        if let Screen::CopyConfigure(ref mut s) = app.screen {
            s.editing = true;
            s.cursor_pos = s.input.len();
            s.error = if input.trim().is_empty() {
                Some("新 Provider 名称不能为空".to_string())
            } else {
                Some("名字已被占用".to_string())
            };
        }
        return;
    }

    if let Screen::CopyConfigure(ref mut s) = app.screen {
        s.step = CopyConfigureStep::Auth;
        s.editing = false;
        s.error = None;
        if auth_kind == CopyAuthKind::ApiKey {
            s.env_editing = false;
            s.env_cursor_pos = s.env_input.len();
        }
    }
}

fn confirm_copy_provider(app: &mut AppModel, api_key_env: Option<String>, no_api_key: bool) {
    let (source_id, target_id) = if let Screen::CopyConfigure(ref s) = app.screen {
        (s.source_id.clone(), s.input.trim().to_string())
    } else {
        return;
    };

    let config_path = app.config_path.clone();
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let runtime_dir = crate::service::StateDirs::resolve(config_dir).runtime_dir;
    let result = crate::tui::run_async_sync(async {
        apply_provider_copy(
            &config_path,
            &runtime_dir,
            &source_id,
            &target_id,
            api_key_env,
            no_api_key,
        )
        .await
    });

    match result {
        Ok(requires_oauth_login) => {
            if requires_oauth_login {
                start_oauth_login_after_copy(app, &target_id);
            } else {
                super::provider_mgmt::return_to_provider_management_preserving_cursor(
                    app,
                    Some(&target_id),
                );
                if let Screen::ProviderManagement(ref mut state) = app.screen {
                    state.error = Some(format!("✓ 已复制为 {target_id}"));
                }
            }
        }
        Err(err) => {
            if let Screen::CopyConfigure(ref mut s) = app.screen {
                s.error = Some(format!("复制失败: {err}"));
            }
        }
    }
}

pub(crate) async fn apply_provider_copy(
    config_path: &std::path::Path,
    runtime_dir: &std::path::Path,
    source_id: &str,
    target_id: &str,
    api_key_env: Option<String>,
    no_api_key: bool,
) -> anyhow::Result<bool> {
    crate::ownership::with_cli_write_lock_or_delegate_at(
        config_path,
        runtime_dir,
        "llm-proxy tui provider copy",
        || {
            let config_path = config_path.to_path_buf();
            let runtime_dir = runtime_dir.to_path_buf();
            let source_id = source_id.to_string();
            let target_id = target_id.to_string();
            let api_key_env = api_key_env.clone();
            async move {
                let config = crate::config::Config::load(&config_path)?;
                let state_dirs = crate::service::StateDirs::for_test(&runtime_dir);
                let mut core = crate::core::CoreState::from_config_with_state_dirs(
                    config,
                    config_path,
                    state_dirs,
                );
                let result = core.copy_provider(&source_id, &target_id, api_key_env, no_api_key)?;
                Ok(result.requires_oauth_login)
            }
        },
        |server| {
            let source_id = source_id.to_string();
            let target_id = target_id.to_string();
            let api_key_env = api_key_env.clone();
            Box::pin(async move {
                let result = server
                    .copy_provider(&source_id, &target_id, api_key_env.as_deref(), no_api_key)
                    .await?;
                let requires_oauth_login = result
                    .get("data")
                    .and_then(|d| d.get("requires_oauth_login"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(requires_oauth_login)
            })
        },
    )
    .await
}

fn start_oauth_login_after_copy(app: &mut AppModel, provider_name: &str) {
    app.oauth_provider_name = Some(provider_name.to_string());
    if let Ok(config) = crate::config::Config::load(&app.config_path)
        && let Some(pc) = config.providers.get(provider_name)
        && let Ok(crate::config::AuthConfig::OpenaiOauth { .. }) = pc.auth_config(provider_name)
    {
        app.screen = Screen::OAuthDeviceCode(model::OAuthDeviceCodeState::pending_login(
            provider_name.to_string(),
        ));
        return;
    }
    super::provider_mgmt::return_to_provider_management_preserving_cursor(app, Some(provider_name));
    if let Screen::ProviderManagement(ref mut state) = app.screen {
        state.error = Some(format!("✓ 已复制为 {provider_name}，请登录"));
    }
}
