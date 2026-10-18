//! Token Usage TUI — interactive usage statistics viewer.
//!
//! Entry point: `llm-proxy usage` (no arguments)

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::terminal_session::TerminalSession;
use crate::tui_control::{TuiKeyAction, non_editing_tui_control_action};

mod model;
mod view;

use model::UsageTuiState;

/// Run the Usage TUI.
pub async fn run() -> Result<()> {
    let mut session = TerminalSession::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = UsageTuiState::new();

    loop {
        terminal.draw(|f| view::render(f, &state))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            match handle_key(&mut state, key) {
                TuiKeyAction::Continue => {}
                TuiKeyAction::Quit => return Ok(()),
                TuiKeyAction::Suspend => {
                    session.suspend_for_resume()?;
                    terminal.clear()?;
                }
            }
        }
    }
}

fn handle_key(state: &mut UsageTuiState, key: KeyEvent) -> TuiKeyAction {
    if let Some(action) = non_editing_tui_control_action(key) {
        return action;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return TuiKeyAction::Quit,
        KeyCode::Up | KeyCode::Char('k') => state.move_up(),
        KeyCode::Down | KeyCode::Char('j') => state.move_down(),
        KeyCode::Char('v') => state.cycle_view(),
        KeyCode::Char('p') => state.cycle_period(),
        KeyCode::Char('f') => state.toggle_filter(),
        KeyCode::Char('J') => state.toggle_json(),
        KeyCode::Enter => state.show_details(),
        _ => {}
    }
    TuiKeyAction::Continue
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;

    fn empty_state() -> UsageTuiState {
        UsageTuiState {
            view_mode: model::ViewMode::ByModel,
            period: model::Period::Last7Days,
            rows: Vec::new(),
            cursor: 0,
            total_tokens: 0,
            total_requests: 0,
            load_error: None,
            filter_provider: None,
            filter_model: None,
            show_json: false,
            all_records: Vec::new(),
            detail_records: Vec::new(),
            detail_open: false,
        }
    }

    #[test]
    fn ctrl_c_requests_quit() {
        let mut state = empty_state();

        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            TuiKeyAction::Quit
        );
    }

    #[test]
    fn ctrl_d_quits_with_applied_non_editing_filter_without_clearing_it() {
        let mut state = empty_state();
        state.filter_provider = Some("deepseek".to_string());

        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
            ),
            TuiKeyAction::Quit
        );
        assert_eq!(state.filter_provider.as_deref(), Some("deepseek"));
    }

    #[test]
    fn ctrl_z_requests_suspend() {
        let mut state = empty_state();

        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)
            ),
            TuiKeyAction::Suspend
        );
    }

    #[test]
    fn ordinary_quit_and_navigation_keys_keep_existing_behavior() {
        let mut state = empty_state();
        state.rows = vec![
            model::UsageRow {
                label: "first".to_string(),
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                request_count: 0,
            },
            model::UsageRow {
                label: "second".to_string(),
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                request_count: 0,
            },
        ];

        assert_eq!(
            handle_key(&mut state, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            TuiKeyAction::Continue
        );
        assert_eq!(state.cursor, 1);
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            ),
            TuiKeyAction::Quit
        );
        assert_eq!(
            handle_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TuiKeyAction::Quit
        );
    }
}
