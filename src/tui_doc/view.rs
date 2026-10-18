use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use super::model::DocTuiState;

pub fn render(frame: &mut Frame, state: &DocTuiState) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(frame.area());
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(20)])
        .split(areas[0]);

    render_sections(frame, state, panes[0]);
    render_document(frame, state, panes[1]);
    render_help(frame, areas[1]);
}

fn render_sections(frame: &mut Frame, state: &DocTuiState, area: Rect) {
    let items = state.section_names().enumerate().map(|(index, name)| {
        let style = if index == state.selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        ListItem::new(name).style(style)
    });
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Sections"));
    frame.render_widget(list, area);
}

fn render_document(frame: &mut Frame, state: &DocTuiState, area: Rect) {
    let document = Paragraph::new(state.selected_text())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(state.selected_name()),
        )
        .scroll((state.scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(document, area);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let help = Line::from(vec![
        Span::styled("[↑/k]", Style::default().fg(Color::Yellow)),
        Span::raw(" Previous  "),
        Span::styled("[↓/j]", Style::default().fg(Color::Yellow)),
        Span::raw(" Next  "),
        Span::styled("[PgUp/PgDn]", Style::default().fg(Color::Yellow)),
        Span::raw(" Scroll  "),
        Span::styled("[q/Esc]", Style::default().fg(Color::Yellow)),
        Span::raw(" Quit"),
    ]);
    frame.render_widget(Paragraph::new(help), area);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn render_shows_section_names_and_selected_document() {
        let state =
            DocTuiState::from_sections(&[("first", "first text"), ("second", "second text")]);
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| render(frame, &state))
            .expect("render");

        let buffer = terminal.backend().buffer();
        let output = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(output.contains("Sections"));
        assert!(output.contains("first"));
        assert!(output.contains("second"));
        assert!(output.contains("first text"));
    }
}
