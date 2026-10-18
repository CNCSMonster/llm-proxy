#![allow(dead_code)] // TUI widgets reserved for future screens
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use super::style;

/// 与 product_list 渲染集合一致的计数，供调用方做光标 clamp。
/// 实现收敛到 model::filtered_len + 共享谓词，状态层与渲染层唯一实现点。
pub fn filtered_product_count(items: &[super::model::ProductItem], filter: &str) -> usize {
    super::model::filtered_len(items, filter, super::model::product_filter_pred)
}

/// 与 env_list 渲染集合一致的计数（skip 项仅在无过滤时计入）。
pub fn filtered_env_count(items: &[super::model::EnvItem], filter: &str) -> usize {
    super::model::filtered_len(items, filter, super::model::env_filter_pred)
}

/// 与 model_list 渲染集合一致的计数。
pub fn filtered_model_count(items: &[super::model::ModelItem], filter: &str) -> usize {
    super::model::filtered_len(items, filter, super::model::model_filter_pred)
}

/// Build a filterable product list widget.
pub fn product_list<'a>(
    items: &'a [super::model::ProductItem],
    _cursor: usize,
    filter: &str,
) -> List<'a> {
    let filtered: Vec<&super::model::ProductItem> = items
        .iter()
        .filter(|i| super::model::filter_match(filter, *i, super::model::product_filter_pred))
        .collect();

    let list_items: Vec<ListItem> = filtered
        .iter()
        .map(|item| {
            let name_style = if item.is_custom {
                Style::new().fg(Color::DarkGray)
            } else {
                Style::new().fg(Color::White)
            };

            let primary = Span::styled(format!(" {} ", item.display_name), name_style);

            let secondary = Span::styled(
                format!(
                    "{} · {} endpoints",
                    item.auth_type.label(),
                    item.endpoint_count
                ),
                Style::new().fg(Color::DarkGray),
            );
            ListItem::new(Line::from(vec![primary, secondary]))
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
    let filtered: Vec<&super::model::EnvItem> = items
        .iter()
        .filter(|i| super::model::filter_match(filter, *i, super::model::env_filter_pred))
        .collect();

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
    let filtered: Vec<&super::model::ModelItem> = items
        .iter()
        .filter(|i| super::model::filter_match(filter, *i, super::model::model_filter_pred))
        .collect();

    let list_items: Vec<ListItem> = filtered
        .iter()
        .map(|item| {
            let is_selected = selected.contains(&item.id);
            let is_configured = configured.contains(&item.id);

            let (mark, marker_style) = if is_selected {
                ("[✓]", Style::new().fg(Color::Green))
            } else if is_configured {
                ("[★]", Style::new().fg(Color::Green))
            } else {
                ("[ ]", Style::new().fg(Color::DarkGray))
            };

            let (name, name_style) = if is_selected && is_configured {
                (
                    format!(" {} (已配置)", item.display_name),
                    Style::new().fg(Color::Green),
                )
            } else if is_selected {
                (
                    format!(" {}", item.display_name),
                    Style::new().fg(Color::White),
                )
            } else if is_configured {
                (
                    format!(" {} (已配置)", item.display_name),
                    Style::new().fg(Color::Green),
                )
            } else {
                (
                    format!(" {}", item.display_name),
                    Style::new().fg(Color::Gray),
                )
            };

            let details = Span::styled(
                format!("   {}", format_model_details(item)),
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

/// 模型行详情：上下文 / 最大输出 / 视觉 / 思考标识。
///
/// `None`（目录未声明）渲染为 `?`，不虚构默认值——与配置侧「宁缺毋假」一致。
fn format_model_details(item: &super::model::ModelItem) -> String {
    let ctx = item
        .context_window
        .map(format_tokens)
        .unwrap_or_else(|| "?".to_string());
    let max_out = item
        .max_output_tokens
        .map(format_tokens)
        .unwrap_or_else(|| "?".to_string());
    let mut line = format!(
        "ctx={}  max_out={}  image={}",
        ctx,
        max_out,
        if item.supports_image() { "✓" } else { "✗" }
    );
    if item.has_thinking() {
        line.push_str(" [thinking]");
    }
    line
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::{ModelItem, ProductItem, ProviderAuthType};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn product_list_widget_renders_clean_template_without_ready_or_runtime_health() {
        let items = vec![
            ProductItem {
                id: "deepseek".to_string(),
                display_name: "DeepSeek API".to_string(),
                auth_type: ProviderAuthType::ApiKey,
                endpoint_count: 1,
                product_kind: "payg".to_string(),
                is_custom: false,
            },
            ProductItem {
                id: "anthropic".to_string(),
                display_name: "Anthropic API".to_string(),
                auth_type: ProviderAuthType::ApiKey,
                endpoint_count: 1,
                product_kind: "subscription".to_string(),
                is_custom: false,
            },
            ProductItem {
                id: String::new(),
                display_name: "Custom provider…".to_string(),
                auth_type: ProviderAuthType::None,
                endpoint_count: 0,
                product_kind: "custom".to_string(),
                is_custom: true,
            },
        ];

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                let widget = product_list(&items, 0, "");
                frame.render_widget(widget, frame.area());
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("DeepSeek API"));
        assert!(rendered.contains("Anthropic API"));
        assert!(rendered.contains("Custom provider…"));
        assert!(!rendered.contains("Ready"));
        assert!(!rendered.contains("✓"));
        assert!(!rendered.contains("⚠"));
        assert!(!rendered.contains("✗"));
        assert!(!rendered.contains("Missing"));
        assert!(!rendered.contains("Expired"));
        assert!(!rendered.contains("Not logged in"));
    }

    fn make_item(features: &[&str], levels: &[&str]) -> ModelItem {
        ModelItem {
            id: "anthropic/claude-sonnet-4".to_string(),
            display_name: "Claude Sonnet 4".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            features: features.iter().map(|s| s.to_string()).collect(),
            reasoning_levels: levels.iter().map(|s| s.to_string()).collect(),
            default_reasoning_level: levels.first().map(|s| s.to_string()),
        }
    }

    /// SPEC：列表项必须展示思考标识 `[thinking]`。
    #[test]
    fn model_details_renders_thinking_badge_only_when_levels_declared() {
        assert!(
            format_model_details(&make_item(&["tools"], &["low", "high"])).contains("[thinking]")
        );
        // 无档位声明 → 不展示徽标（宁缺毋假，不猜思考能力）
        assert!(!format_model_details(&make_item(&["tools"], &[])).contains("[thinking]"));
    }

    /// SPEC：视觉能力来自 `input_modalities`/`image_input`；
    /// 目录未声明的窗口渲染为 `?`，不虚构默认值。
    #[test]
    fn model_details_renders_vision_flag_and_unknown_window_as_question_mark() {
        assert!(format_model_details(&make_item(&["image_input"], &[])).contains("image=✓"));
        assert!(!format_model_details(&make_item(&["tools"], &[])).contains("image=✓"));

        let mut unknown = make_item(&[], &[]);
        unknown.context_window = None;
        unknown.max_output_tokens = None;
        let rendered = format_model_details(&unknown);
        assert!(rendered.contains("ctx=?"), "{}", rendered);
        assert!(rendered.contains("max_out=?"), "{}", rendered);
    }
}
