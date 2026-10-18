//! Interactive browser for the embedded user documentation.

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

use model::DocTuiState;

pub fn run() -> Result<()> {
    let mut session = TerminalSession::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    run_event_loop(&mut terminal, &mut session)
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session: &mut TerminalSession,
) -> Result<()> {
    let mut state = DocTuiState::new(crate::doc::sections());
    let size = terminal.size()?;
    state.set_viewport(size.width, size.height);
    loop {
        terminal.draw(|frame| view::render(frame, &state))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => match handle_key(&mut state, key) {
                    TuiKeyAction::Continue => {}
                    TuiKeyAction::Quit => return Ok(()),
                    TuiKeyAction::Suspend => {
                        session.suspend_for_resume()?;
                        terminal.clear()?;
                    }
                },
                // wrap 折行随宽度变化，resize 后必须按新视口重 clamp，
                // 否则窄终端下文末内容可能永远滚不到（或滚动值越界）。
                Event::Resize(width, height) => state.set_viewport(width, height),
                _ => {}
            }
        }
    }
}

fn handle_key(state: &mut DocTuiState, key: KeyEvent) -> TuiKeyAction {
    if let Some(action) = non_editing_tui_control_action(key) {
        return action;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => TuiKeyAction::Quit,
        KeyCode::Up | KeyCode::Char('k') => {
            state.previous_section();
            TuiKeyAction::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.next_section();
            TuiKeyAction::Continue
        }
        KeyCode::PageUp => {
            state.scroll_up(10);
            TuiKeyAction::Continue
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            state.scroll_down(10);
            TuiKeyAction::Continue
        }
        _ => TuiKeyAction::Continue,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    #[test]
    fn navigation_keys_change_section_and_scroll() {
        let mut state = DocTuiState::from_sections(&[("one", "a\nb"), ("two", "c\nd")]);
        // 小视口：文档区可视 1 行，2 行文档最多滚到 1
        state.set_viewport(80, 4);

        assert_eq!(
            handle_key(&mut state, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            TuiKeyAction::Continue
        );
        assert_eq!(state.selected, 1);
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)
            ),
            TuiKeyAction::Continue
        );
        assert_eq!(state.scroll, 1);
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)
            ),
            TuiKeyAction::Continue
        );
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn q_and_escape_request_quit() {
        let mut state = DocTuiState::from_sections(&[("one", "text")]);
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

    #[test]
    fn ctrl_c_and_ctrl_d_request_quit() {
        let mut state = DocTuiState::from_sections(&[("one", "text")]);

        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            TuiKeyAction::Quit
        );
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
            ),
            TuiKeyAction::Quit
        );
    }

    #[test]
    fn ctrl_z_requests_suspend() {
        let mut state = DocTuiState::from_sections(&[("one", "text")]);

        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)
            ),
            TuiKeyAction::Suspend
        );
    }
}
