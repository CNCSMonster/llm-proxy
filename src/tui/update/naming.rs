use crossterm::event::{KeyCode, KeyEvent};

use super::model::{AppModel, NameValidation, Screen};
use super::products::select_product_after_naming;
use crate::tui::model::is_provider_name_taken;

/// Handle keyboard input on the ProviderNaming screen.
///
/// Note: Esc is handled globally in `handle_esc` (editing → exit editing;
/// browse → back to product selection). This function only handles non-Esc keys.
pub(crate) fn handle_naming_keys(app: &mut AppModel, key: KeyEvent) {
    // Extract editing state first to avoid borrow issues
    let editing = if let Screen::ProviderNaming(ref s) = app.screen {
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

/// Browse mode: `e` enters editing, `Enter` confirms.
fn handle_browse_mode(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Char('e') => {
            if let Screen::ProviderNaming(ref mut s) = app.screen {
                s.editing = true;
                s.cursor_pos = s.input.len();
            }
        }
        KeyCode::Enter => {
            confirm_naming(app);
        }
        _ => {}
    }
}

/// Editing mode: accept character input, Backspace, Left/Right, Enter.
/// Esc is handled by the global handle_esc.
fn handle_editing_mode(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            confirm_naming(app);
        }
        KeyCode::Char(ch) if ch.is_alphanumeric() || ch == '-' || ch == '_' => {
            if let Screen::ProviderNaming(ref mut s) = app.screen {
                // Insert character at cursor position
                if s.cursor_pos >= s.input.len() {
                    s.input.push(ch);
                } else {
                    s.input.insert(s.cursor_pos, ch);
                }
                s.cursor_pos += 1;
                // Real-time validation
                validate_name(s, &app.config_path);
            }
        }
        KeyCode::Backspace => {
            if let Screen::ProviderNaming(ref mut s) = app.screen
                && s.cursor_pos > 0
            {
                s.cursor_pos -= 1;
                s.input.remove(s.cursor_pos);
                validate_name(s, &app.config_path);
            }
        }
        KeyCode::Left => {
            if let Screen::ProviderNaming(ref mut s) = app.screen {
                s.cursor_pos = s.cursor_pos.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if let Screen::ProviderNaming(ref mut s) = app.screen
                && s.cursor_pos < s.input.len()
            {
                s.cursor_pos += 1;
            }
        }
        _ => {}
    }
}

/// Re-validate the current input against the config.
fn validate_name(s: &mut super::model::ProviderNamingState, config_path: &std::path::Path) {
    let providers = crate::config::Config::load(config_path)
        .map(|cfg| cfg.providers)
        .unwrap_or_default();
    s.validation = if is_provider_name_taken(&providers, &s.input) {
        NameValidation::Conflict
    } else {
        NameValidation::Available
    };
}

/// Confirm the naming: save the chosen name and proceed to the next screen.
fn confirm_naming(app: &mut AppModel) {
    // Extract needed data
    let (input, validation, product_item) = if let Screen::ProviderNaming(ref s) = app.screen {
        (
            s.input.clone(),
            s.validation.clone(),
            s.product_item.clone(),
        )
    } else {
        return;
    };

    // Block confirmation if name conflicts
    if validation == NameValidation::Conflict {
        // Cannot proceed — name is taken. Enter editing mode so user can fix it.
        if let Screen::ProviderNaming(ref mut s) = app.screen {
            s.editing = true;
            s.cursor_pos = s.input.len();
        }
        return;
    }

    // Empty name check
    if input.trim().is_empty() {
        if let Screen::ProviderNaming(ref mut s) = app.screen {
            s.editing = true;
            s.cursor_pos = s.input.len();
        }
        return;
    }

    // Save the chosen provider name
    app.oauth_provider_name = Some(input);
    app.chosen_product = Some(product_item.clone());
    // Mark as repeat connect so env → FallbackConfig routing kicks in
    app.is_repeat_connect = true;

    // Proceed to the next step in the connect flow (env selection, etc.)
    let product_name = product_item.display_name.clone();
    select_product_after_naming(app, product_item, product_name);
}
