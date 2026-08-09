//! Token Usage TUI — interactive usage statistics viewer.
//!
//! Entry point: `llm-proxy usage` (no arguments)

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod model;
mod view;

use model::UsageTuiState;

/// Run the Usage TUI.
pub async fn run() -> Result<()> {
    // Setup terminal
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = UsageTuiState::new();

    // Main event loop
    loop {
        // Draw
        terminal.draw(|f| view::render(f, &state))?;

        // Handle input
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && handle_key(&mut state, key)
        {
            break; // quit
        }
    }

    // Restore terminal
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    Ok(())
}

/// Handle a key event. Returns true to quit.
fn handle_key(state: &mut UsageTuiState, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Up | KeyCode::Char('k') => state.move_up(),
        KeyCode::Down | KeyCode::Char('j') => state.move_down(),
        KeyCode::Char('v') => state.cycle_view(),
        KeyCode::Char('p') => state.cycle_period(),
        KeyCode::Char('f') => state.toggle_filter(),
        KeyCode::Char('J') => state.toggle_json(),
        KeyCode::Enter => state.show_details(),
        _ => {}
    }
    false
}
