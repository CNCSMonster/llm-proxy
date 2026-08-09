use crossterm::event::{KeyCode, KeyEvent};

use super::model;

pub(crate) fn handle_filter_input(filter: &mut String, key: KeyEvent) {
    match key.code {
        KeyCode::Char(ch) => {
            filter.push(ch);
        }
        KeyCode::Backspace => {
            filter.pop();
        }
        _ => {}
    }
}

pub(crate) fn scan_env_vars() -> Vec<model::EnvItem> {
    let mut items: Vec<model::EnvItem> = std::env::vars()
        .map(|(name, _)| model::EnvItem {
            name,
            is_skip: false,
            recommended: false,
        })
        .collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    items.push(model::EnvItem {
        name: String::new(),
        is_skip: true,
        recommended: false,
    });
    items
}
