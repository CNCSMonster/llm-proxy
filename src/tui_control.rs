use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlobalControlKey {
    Interrupt,
    Eof,
    Suspend,
    /// Consume release/repeat events so they cannot fall through as page input.
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TuiKeyAction {
    Continue,
    Quit,
    Suspend,
}

pub(crate) fn global_control_key(key: KeyEvent) -> Option<GlobalControlKey> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }

    let control = match key.code {
        KeyCode::Char('c') => GlobalControlKey::Interrupt,
        KeyCode::Char('d') => GlobalControlKey::Eof,
        KeyCode::Char('z') => GlobalControlKey::Suspend,
        _ => return None,
    };

    if key.kind == KeyEventKind::Press {
        Some(control)
    } else {
        Some(GlobalControlKey::Ignored)
    }
}

pub(crate) fn non_editing_tui_control_action(key: KeyEvent) -> Option<TuiKeyAction> {
    global_control_key(key).map(|control| match control {
        GlobalControlKey::Interrupt | GlobalControlKey::Eof => TuiKeyAction::Quit,
        GlobalControlKey::Suspend => TuiKeyAction::Suspend,
        GlobalControlKey::Ignored => TuiKeyAction::Continue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control_key(code: char, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Char(code), KeyModifiers::CONTROL, kind)
    }

    #[test]
    fn press_events_map_to_global_control_keys() {
        assert_eq!(
            global_control_key(control_key('c', KeyEventKind::Press)),
            Some(GlobalControlKey::Interrupt)
        );
        assert_eq!(
            global_control_key(control_key('d', KeyEventKind::Press)),
            Some(GlobalControlKey::Eof)
        );
        assert_eq!(
            global_control_key(control_key('z', KeyEventKind::Press)),
            Some(GlobalControlKey::Suspend)
        );
    }

    #[test]
    fn release_and_repeat_control_events_are_consumed_without_action() {
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            for code in ['c', 'd', 'z'] {
                assert_eq!(
                    global_control_key(control_key(code, kind)),
                    Some(GlobalControlKey::Ignored)
                );
            }
        }
    }

    #[test]
    fn ordinary_keys_are_not_global_control_keys() {
        assert_eq!(
            global_control_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            global_control_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            global_control_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            None
        );
    }
}
