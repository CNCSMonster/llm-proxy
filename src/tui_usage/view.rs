//! Usage TUI rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use super::model::UsageTuiState;

/// Format a number with thousand separators.
fn format_number(n: i64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Render the usage TUI.
pub fn render(f: &mut Frame, state: &UsageTuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if state.detail_open {
            vec![
                Constraint::Length(3), // Header
                Constraint::Length(1), // Summary
                Constraint::Min(3),    // Table
                Constraint::Min(3),    // Detail
                Constraint::Length(1), // Help bar
            ]
        } else {
            vec![
                Constraint::Length(3), // Header
                Constraint::Length(1), // Summary
                Constraint::Min(5),    // Table
                Constraint::Length(1), // Help bar
            ]
        })
        .split(f.area());

    // Header
    render_header(f, state, chunks[0]);

    // Summary
    render_summary(f, state, chunks[1]);

    // Table
    render_table(f, state, chunks[2]);

    if state.detail_open {
        // Detail view
        render_detail(f, state, chunks[3]);
        render_help(f, chunks[4]);
    } else {
        // Help bar
        render_help(f, chunks[3]);
    }
}

fn render_header(f: &mut Frame, state: &UsageTuiState, area: Rect) {
    let filter_text = format!(
        "Filter: {} | {} | {}",
        state.filter_provider.as_deref().unwrap_or("All Providers"),
        "All Endpoints",
        state.period.as_str()
    );
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "Token Usage Statistics",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(state.view_mode.as_str(), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(Span::styled(
            filter_text,
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, area);
}

fn render_summary(f: &mut Frame, state: &UsageTuiState, area: Rect) {
    let summary = Line::from(vec![
        Span::raw("Total: "),
        Span::styled(
            format!("{} tokens", format_number(state.total_tokens)),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        Span::styled(
            format!("{} requests", format_number(state.total_requests)),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    f.render_widget(Paragraph::new(summary), area);
}

fn render_table(f: &mut Frame, state: &UsageTuiState, area: Rect) {
    let header_cells = ["Label", "Input", "Output", "Total", "Requests", "%"]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        });
    let header = Row::new(header_cells).height(1);

    let rows: Vec<Row> = state
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let pct = if state.total_tokens > 0 {
                (row.total_tokens as f64 / state.total_tokens as f64) * 100.0
            } else {
                0.0
            };

            let style = if i == state.cursor {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };

            Row::new(vec![
                Cell::from(row.label.clone()),
                Cell::from(format_number(row.input_tokens)),
                Cell::from(format_number(row.output_tokens)),
                Cell::from(format_number(row.total_tokens)),
                Cell::from(format_number(row.request_count)),
                Cell::from(format!("{:.1}%", pct)),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(state.view_mode.as_str()),
    );

    f.render_widget(table, area);
}

fn render_help(f: &mut Frame, area: Rect) {
    let help = Line::from(vec![
        Span::styled("[f]", Style::default().fg(Color::Yellow)),
        Span::raw(" Filter  "),
        Span::styled("[v]", Style::default().fg(Color::Yellow)),
        Span::raw(" View  "),
        Span::styled("[p]", Style::default().fg(Color::Yellow)),
        Span::raw(" Period  "),
        Span::styled("[Enter]", Style::default().fg(Color::Yellow)),
        Span::raw(" Details  "),
        Span::styled("[q]", Style::default().fg(Color::Yellow)),
        Span::raw(" Quit"),
    ]);
    f.render_widget(Paragraph::new(help), area);
}

/// Render the detail view for the currently selected row.
fn render_detail(f: &mut Frame, state: &UsageTuiState, area: Rect) {
    let label = state
        .rows
        .get(state.cursor)
        .map(|r| r.label.clone())
        .unwrap_or_default();
    let title = format!(
        "Details: {label} ({} request{})",
        state.detail_records.len(),
        if state.detail_records.len() == 1 {
            ""
        } else {
            "s"
        }
    );

    const MAX_DETAIL_ROWS: usize = 10;
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Time                   Provider             Model                  Endpoint           Input     Output    Latency",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    for r in state.detail_records.iter().take(MAX_DETAIL_ROWS) {
        let time = r
            .parsed_timestamp()
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| r.timestamp.clone());
        let latency = r
            .latency_ms
            .map(|l| format!("{l}ms"))
            .unwrap_or_else(|| "-".to_string());
        lines.push(Line::from(format!(
            "{time:<22} {:<21} {:<22} {:<18} {:<10} {:<10} {latency}",
            r.provider,
            r.model,
            r.endpoint,
            format_number(r.input_tokens),
            format_number(r.output_tokens),
        )));
    }
    if state.detail_records.len() > MAX_DETAIL_ROWS {
        lines.push(Line::from(Span::styled(
            format!(
                "… and {} more",
                state.detail_records.len() - MAX_DETAIL_ROWS
            ),
            Style::default().fg(Color::DarkGray),
        )));
    }

    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(para, area);
}
