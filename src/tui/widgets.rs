#![allow(dead_code)] // TUI widgets reserved for future screens
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use super::fuzzy;
use super::style;

/// Build a filterable product list widget.
pub fn product_list<'a>(
    items: &'a [super::model::ProductItem],
    _cursor: usize,
    filter: &str,
) -> List<'a> {
    let filtered: Vec<&super::model::ProductItem> = if filter.is_empty() {
        items.iter().collect()
    } else {
        items
            .iter()
            .filter(|i| {
                fuzzy::fuzzy_match(filter, &i.display_name) || fuzzy::fuzzy_match(filter, &i.id)
            })
            .collect()
    };

    let list_items: Vec<ListItem> = filtered
        .iter()
        .map(|item| {
            let name_style = if item.is_custom {
                Style::new().fg(Color::DarkGray)
            } else {
                Style::new().fg(Color::White)
            };

            let primary = Span::styled(format!(" {} ", item.display_name), name_style);

            // 添加认证状态
            let status_span = if let Some(ref status) = item.auth_status {
                let (status_text, status_style) = if status.contains("✓") {
                    (status.clone(), Style::new().fg(Color::Green))
                } else if status.contains("⚠") {
                    (status.clone(), Style::new().fg(Color::Yellow))
                } else if status.contains("✗") {
                    (status.clone(), Style::new().fg(Color::Red))
                } else {
                    (status.clone(), Style::new().fg(Color::DarkGray))
                };
                Span::styled(format!(" {} ", status_text), status_style)
            } else {
                Span::raw("")
            };

            let secondary = Span::styled(
                format!(
                    "{} · {} endpoints",
                    item.auth_type.label(),
                    item.endpoint_count
                ),
                Style::new().fg(Color::DarkGray),
            );
            ListItem::new(Line::from(vec![primary, status_span, secondary]))
        })
        .collect();

    List::new(list_items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ")
}

/// Build a filterable environment variable list widget.
pub fn env_list<'a>(
    items: &'a [super::model::EnvItem],
    _cursor: usize,
    filter: &str,
    _product_name: &str,
) -> List<'a> {
    let filtered: Vec<&super::model::EnvItem> = if filter.is_empty() {
        items.iter().collect()
    } else {
        items
            .iter()
            .filter(|i| {
                if i.is_skip {
                    false // skip item is never matched by filter
                } else {
                    fuzzy::fuzzy_match(filter, &i.name)
                }
            })
            .collect()
    };

    let list_items: Vec<ListItem> = filtered
        .iter()
        .map(|item| {
            if item.is_skip {
                ListItem::new(Line::from(Span::styled(
                    " (不使用 API Key)",
                    Style::new().fg(Color::DarkGray),
                )))
            } else if item.recommended {
                ListItem::new(Line::from(vec![
                    Span::styled(&item.name, Style::new().fg(Color::White)),
                    Span::styled(
                        "  you may want",
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::ITALIC),
                    ),
                ]))
            } else {
                ListItem::new(Line::from(Span::styled(
                    &item.name,
                    Style::new().fg(Color::White),
                )))
            }
        })
        .collect();

    List::new(list_items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ")
}

/// Build a multi-select model list widget.
pub fn model_list<'a>(
    items: &'a [super::model::ModelItem],
    _cursor: usize,
    filter: &str,
    selected: &std::collections::HashSet<String>,
    configured: &std::collections::HashSet<String>,
    _product_name: &str,
) -> List<'a> {
    let filtered: Vec<&super::model::ModelItem> = if filter.is_empty() {
        items.iter().collect()
    } else {
        items
            .iter()
            .filter(|i| {
                fuzzy::fuzzy_match(filter, &i.id) || fuzzy::fuzzy_match(filter, &i.display_name)
            })
            .collect()
    };

    let list_items: Vec<ListItem> = filtered
        .iter()
        .map(|item| {
            let (mark, marker_style) = if configured.contains(&item.id) {
                ("[★]", Style::new().fg(Color::Green))
            } else if selected.contains(&item.id) {
                ("[✓]", Style::new().fg(Color::Green))
            } else {
                ("[ ]", Style::new().fg(Color::DarkGray))
            };

            let (name, name_style) = if configured.contains(&item.id) {
                (
                    format!(" {} (已配置)", item.display_name),
                    Style::new().fg(Color::Green),
                )
            } else if selected.contains(&item.id) {
                (
                    format!(" {}", item.display_name),
                    Style::new().fg(Color::White),
                )
            } else {
                (
                    format!(" {}", item.display_name),
                    Style::new().fg(Color::Gray),
                )
            };

            let details = Span::styled(
                format!(
                    "   {}",
                    format_model_details(
                        item.context_window,
                        item.max_output_tokens,
                        item.supports_image
                    )
                ),
                Style::new().fg(Color::DarkGray),
            );

            ListItem::new(Line::from(vec![
                Span::styled(mark, marker_style),
                Span::styled(name, name_style),
                details,
            ]))
        })
        .collect();

    List::new(list_items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ")
}

/// Build a help bar.
pub fn help_bar(text: &str) -> Paragraph<'_> {
    Paragraph::new(Line::from(Span::styled(text, style::INSTRUCTION_STYLE)))
}

/// Build an error banner.
pub fn error_banner(message: &str) -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        format!("⚠️  {}", message),
        style::ERROR_STYLE,
    )))
}

fn format_model_details(context_window: i64, max_output: i64, supports_image: bool) -> String {
    format!(
        "ctx={}  max_out={}  image={}",
        format_tokens(context_window),
        format_tokens(max_output),
        if supports_image { "✓" } else { "✗" }
    )
}

fn format_tokens(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}
