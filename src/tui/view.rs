use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::model::{self, NameOption, Screen, WarningOption};
use super::widgets;

/// Main render function. Routes to per-screen renderers.
/// Only the outer TUI has a border; all inner screens are border-free.
pub fn render(frame: &mut Frame, app: &model::AppModel) {
    let area = frame.area();

    // Outer border for the entire TUI (the only border)
    let outer = Block::bordered()
        .border_style(Style::new().fg(Color::Cyan))
        .title_top(Line::from(Span::styled(
            " llm-proxy 配置向导 ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    // Layout: content (fill), separator (1), help (1)
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(0),
        Constraint::Length(1),
    ])
    .split(inner);

    // Content area — no borders, just plain rendering
    match &app.screen {
        Screen::ProviderManagement(s) => render_provider_management(frame, chunks[0], s),
        Screen::ProviderDetail(s) => render_provider_detail(frame, chunks[0], s),
        Screen::DeleteConfirm(s) => render_delete_confirm(frame, chunks[0], s),
        Screen::ResetUsageConfirm(s) => render_reset_usage_confirm(frame, chunks[0], s),
        Screen::ProductSelection(s) => render_product_selection(frame, chunks[0], s, app),
        Screen::EnvVarSelection(s) => render_env_var_selection(frame, chunks[0], s),
        Screen::ModelSelection(s) => render_model_selection(frame, chunks[0], s),
        Screen::CustomProviderEditor(s) => render_custom_provider_editor(frame, chunks[0], s),
        Screen::ProviderNaming(s) => render_provider_naming(frame, chunks[0], s),
        Screen::WarningConfirm(s) => render_warning_confirm(frame, chunks[0], s),
        Screen::Verifying(s) => render_verifying(frame, chunks[0], s),
        Screen::Done(s) => render_done(frame, chunks[0], s),
        Screen::OAuthName(s) => render_oauth_name(frame, chunks[0], s),
        Screen::OAuthDeviceCode(s) => render_oauth_device_code(frame, chunks[0], s),
        Screen::AntigravityLogin(s) => render_antigravity_login(frame, chunks[0], s),
        Screen::OAuthOverwrite(s) => render_oauth_overwrite(frame, chunks[0], s),
        Screen::FallbackConfig(s) => render_fallback_config(frame, chunks[0], s),
        Screen::CopyModelConfirm(s) => render_copy_model_confirm(frame, chunks[0], s),
        Screen::ModelRename(s) => render_model_rename(frame, chunks[0], s),
        Screen::Quit => {}
    }

    // Help bar at the bottom
    let help_text = app.help_text();
    let help = Paragraph::new(Line::from(Span::styled(
        help_text,
        Style::new().fg(Color::DarkGray),
    )))
    .alignment(ratatui::layout::Alignment::Right);
    frame.render_widget(help, chunks[2]);
}

// ── Section Title Helper ─────────────────────────────────────────────────

fn section_title(title: String, filter: Option<String>) -> Paragraph<'static> {
    let base = Span::styled(
        title,
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    );
    let line = if let Some(f) = filter {
        Line::from(vec![
            base,
            Span::styled(format!("  [search: {}]", f), Style::new().fg(Color::Yellow)),
        ])
    } else {
        Line::from(base)
    };
    Paragraph::new(line)
}

// ── Per-Screen Renderers ─────────────────────────────────────────────────

fn render_product_selection(
    frame: &mut Frame,
    area: Rect,
    state: &model::ProductSelectionState,
    _app: &model::AppModel,
) {
    let has_filter = state.filter_active && !state.filter.is_empty();
    let title = section_title(
        "Select Product".to_string(),
        if has_filter {
            Some(state.filter.clone())
        } else {
            None
        },
    );

    // Title + list layout
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(title, chunks[0]);

    let list = widgets::product_list(&state.items, state.cursor, &state.filter);
    let mut list_state = ratatui::widgets::ListState::default();

    let filtered_items = if state.filter.is_empty() {
        state.items.len()
    } else {
        let lower = state.filter.to_lowercase();
        state
            .items
            .iter()
            .filter(|i| {
                i.display_name.to_lowercase().contains(&lower)
                    || i.id.to_lowercase().contains(&lower)
            })
            .count()
    };
    if filtered_items > 0 {
        list_state.select(Some(state.cursor.min(filtered_items.saturating_sub(1))));
    }

    frame.render_stateful_widget(list, chunks[1], &mut list_state);
}

fn render_env_var_selection(frame: &mut Frame, area: Rect, state: &model::EnvVarSelectionState) {
    let has_filter = state.filter_active && !state.filter.is_empty();
    let title = section_title(
        format!("Select API Key — {}", state.product_name),
        if has_filter {
            Some(state.filter.clone())
        } else {
            None
        },
    );

    // Layout: title + list + optional warning
    let warning_height = if state.env_in_use_warning.is_some() {
        Constraint::Length(1)
    } else {
        Constraint::Length(0)
    };
    let chunks =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1), warning_height]).split(area);
    frame.render_widget(title, chunks[0]);

    let list = widgets::env_list(
        &state.items,
        state.cursor,
        &state.filter,
        &state.product_name,
    );
    let mut list_state = ratatui::widgets::ListState::default();
    if !state.items.is_empty() {
        list_state.select(Some(state.cursor.min(state.items.len().saturating_sub(1))));
    }
    frame.render_stateful_widget(list, chunks[1], &mut list_state);

    // Env-in-use warning (persistent, non-blinking)
    if let Some(ref warning) = state.env_in_use_warning {
        let warning_widget = Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(warning, Style::new().fg(Color::Yellow)),
            Span::raw("  (允许继续)"),
        ]));
        frame.render_widget(warning_widget, chunks[2]);
    }
}

fn render_model_selection(frame: &mut Frame, area: Rect, state: &model::ModelSelectionState) {
    let has_filter = state.filter_active && !state.filter.is_empty();
    let title = section_title(
        format!("Select Models — {}", state.product_name),
        if has_filter {
            Some(state.filter.clone())
        } else {
            None
        },
    );

    let selected_count = state.selected.len();
    let total = state.items.len();

    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(title, chunks[0]);

    let list = widgets::model_list(
        &state.items,
        state.cursor,
        &state.filter,
        &state.selected,
        &state.configured,
        &state.product_name,
    );
    let mut list_state = ratatui::widgets::ListState::default();
    if !state.items.is_empty() {
        list_state.select(Some(state.cursor.min(state.items.len().saturating_sub(1))));
    }
    frame.render_stateful_widget(list, chunks[1], &mut list_state);

    // Status bar
    let (status_text, status_style) = if selected_count > 0 {
        (
            format!(" {} of {} selected  ", selected_count, total),
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            " 0 selected — Enter 跳过模型选择  ".to_string(),
            Style::new().fg(Color::DarkGray),
        )
    };
    let status = Paragraph::new(Line::from(Span::styled(&status_text, status_style)))
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(status, chunks[2]);
}

fn render_custom_provider_editor(
    frame: &mut Frame,
    area: Rect,
    state: &model::CustomProviderEditorState,
) {
    use model::EditorMode;

    let title_text = match &state.purpose {
        model::EditorPurpose::Create => match &state.mode {
            EditorMode::Browse => "Custom Provider".to_string(),
            EditorMode::Edit => format!("Edit: {}", state.current_field().label),
        },
        model::EditorPurpose::Edit {
            original_name,
            auth_type: _,
        } => match &state.mode {
            EditorMode::Browse => format!("Edit Provider: {}", original_name),
            EditorMode::Edit => format!("Edit: {}", state.current_field().label),
        },
    };
    let title = section_title(title_text, None);

    let chunks = Layout::vertical([
        Constraint::Length(2), // title
        Constraint::Min(1),    // fields
        Constraint::Length(1), // hint
        Constraint::Length(1), // buttons
    ])
    .split(area);
    frame.render_widget(title, chunks[0]);

    match &state.mode {
        EditorMode::Browse => {
            use model::EditorFocus;

            let lines: Vec<Line> = state
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let is_editable = state.is_field_editable(f.field.clone());
                    let is_selected =
                        state.focus == EditorFocus::Fields && i == state.cursor && is_editable;
                    let value_display = if f.value.is_empty() {
                        format!("({})", f.placeholder)
                    } else {
                        f.value.clone()
                    };

                    // 不可编辑字段显示为只读样式
                    if !is_editable {
                        return Line::from(vec![
                            Span::styled("   ", Style::new()),
                            Span::styled(
                                format!("{:<16}", f.label),
                                Style::new().fg(Color::DarkGray),
                            ),
                            Span::styled(value_display, Style::new().fg(Color::DarkGray)),
                            Span::styled(" 🔒", Style::new().fg(Color::DarkGray)),
                        ]);
                    }

                    let status = if f.value.is_empty() {
                        if matches!(
                            f.field,
                            model::ProviderField::Name | model::ProviderField::ApiKeyEnv
                        ) {
                            Span::styled(" !", Style::new().fg(Color::Yellow))
                        } else {
                            Span::styled(" -", Style::new().fg(Color::DarkGray))
                        }
                    } else {
                        Span::styled(" ✓", Style::new().fg(Color::Green))
                    };

                    let (label_style, value_style) = if is_selected {
                        (
                            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                            Style::new().fg(Color::White),
                        )
                    } else {
                        (
                            Style::new().fg(Color::Gray),
                            Style::new().fg(Color::DarkGray),
                        )
                    };

                    Line::from(vec![
                        Span::styled(
                            if is_selected { " ▶ " } else { "   " },
                            if is_selected {
                                Style::new().fg(Color::Green)
                            } else {
                                Style::new()
                            },
                        ),
                        Span::styled(format!("{:<16}", f.label), label_style),
                        Span::styled(value_display, value_style),
                        status,
                    ])
                })
                .collect();

            frame.render_widget(Paragraph::new(lines), chunks[1]);

            // Hint
            let hint = "↑↓ Navigate  Enter Edit/Select  Esc Back";
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    hint,
                    Style::new().fg(Color::DarkGray),
                ))),
                chunks[2],
            );

            // Buttons
            let can_save = state.has_endpoint();
            let save_style = if state.focus == EditorFocus::Buttons && state.button_cursor == 0 {
                if can_save {
                    Style::new()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::DarkGray).bg(Color::DarkGray)
                }
            } else if can_save {
                Style::new().fg(Color::Green)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            let cancel_style = if state.focus == EditorFocus::Buttons && state.button_cursor == 1 {
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Red)
            };

            let buttons = Line::from(vec![
                Span::raw("  "),
                Span::styled(" Save & Connect ", save_style),
                Span::raw("  "),
                Span::styled(" Cancel ", cancel_style),
            ]);
            frame.render_widget(Paragraph::new(buttons), chunks[3]);
        }
        EditorMode::Edit => {
            let field = state.current_field();
            let display = if field.value.is_empty() {
                field.placeholder
            } else {
                &field.value
            };
            let cursor_pos = state.edit_cursor.min(display.len());

            let before = &display[..cursor_pos];
            let cursor_char = if cursor_pos < display.len() {
                &display[cursor_pos..cursor_pos + 1]
            } else {
                " "
            };
            let after = if cursor_pos + 1 < display.len() {
                &display[cursor_pos + 1..]
            } else {
                ""
            };

            let cursor_span = Span::styled(
                cursor_char,
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );

            let input_line = Line::from(vec![
                Span::raw("  > "),
                Span::raw(before),
                cursor_span,
                Span::raw(after),
            ]);

            let is_placeholder = field.value.is_empty();
            let lines = vec![
                Line::from(""),
                input_line,
                Line::from(""),
                if is_placeholder {
                    Line::from(Span::styled(
                        "  (placeholder — type to replace)",
                        Style::new().fg(Color::DarkGray),
                    ))
                } else {
                    Line::from(Span::styled(
                        "  Enter Confirm  Esc Cancel",
                        Style::new().fg(Color::DarkGray),
                    ))
                },
            ];

            frame.render_widget(Paragraph::new(lines), chunks[1]);

            // Show error if any
            if let Some(err) = &state.error {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!("  Error: {}", err),
                        Style::new().fg(Color::Red),
                    ))),
                    chunks[2],
                );
            }
        }
    }
}

fn render_provider_naming(frame: &mut Frame, area: Rect, state: &model::ProviderNamingState) {
    use model::NameValidation;

    let title = section_title(
        format!("Provider Name — {}", state.product_display_name),
        None,
    );

    let chunks = Layout::vertical([
        Constraint::Length(2), // title
        Constraint::Length(1), // subtitle
        Constraint::Length(3), // input display
        Constraint::Length(1), // validation
        Constraint::Min(1),    // spacer
    ])
    .split(area);

    frame.render_widget(title, chunks[0]);

    // Subtitle
    let subtitle = Paragraph::new(Line::from(Span::styled(
        " This product already has a provider. Choose a name for the new instance.",
        Style::new().fg(Color::DarkGray),
    )));
    frame.render_widget(subtitle, chunks[1]);

    // Input display
    if state.editing {
        // Editing mode: show input with cursor
        let cursor_pos = state.cursor_pos.min(state.input.len());
        let before = &state.input[..cursor_pos];
        let cursor_char = if cursor_pos < state.input.len() {
            &state.input[cursor_pos..cursor_pos + 1]
        } else {
            " "
        };
        let after = if cursor_pos + 1 < state.input.len() {
            &state.input[cursor_pos + 1..]
        } else {
            ""
        };

        let cursor_span = Span::styled(
            cursor_char,
            Style::new()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

        let input_line = Line::from(vec![
            Span::raw("  > "),
            Span::raw(before),
            cursor_span,
            Span::raw(after),
        ]);
        frame.render_widget(Paragraph::new(input_line), chunks[2]);
    } else {
        // Browse mode: show name as pre-filled, not actively editing
        let name_display = if state.input.is_empty() {
            "___".to_string()
        } else {
            state.input.clone()
        };

        let input_line = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                &name_display,
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(Paragraph::new(input_line), chunks[2]);
    }

    // Validation message
    let (validation_text, validation_style) = match &state.validation {
        NameValidation::Available => ("✓ 名字可用", Style::new().fg(Color::Green)),
        NameValidation::Conflict => ("✗ 名字已被占用", Style::new().fg(Color::Red)),
    };
    let validation = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(validation_text, validation_style),
    ]));
    frame.render_widget(validation, chunks[3]);
}

fn render_warning_confirm(frame: &mut Frame, area: Rect, state: &model::WarningConfirmState) {
    let title = Paragraph::new(Line::from(Span::styled(
        "⚠️  Verification Failed",
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )));

    let mut lines = vec![
        Line::from(""),
        Line::from(state.message.as_str()),
        Line::from(""),
        Line::from(Span::styled(
            "Configuration can still be saved, but may not work until corrected.",
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];

    let options = ["Continue anyway", "Go back"];
    for (i, opt) in options.iter().enumerate() {
        let selected = (i == 0 && state.selected_option == WarningOption::Continue)
            || (i == 1 && state.selected_option == WarningOption::Back);
        if selected {
            lines.push(Line::from(vec![
                Span::styled(
                    "▶ ",
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    *opt,
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
            ]));
        } else {
            lines.push(Line::from(format!("  {}", opt)));
        }
    }

    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(title, chunks[0]);
    frame.render_widget(Paragraph::new(lines), chunks[1]);
}

fn render_verifying(frame: &mut Frame, area: Rect, state: &model::VerifyingState) {
    let title = section_title("Verifying".to_string(), None);
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(title, chunks[0]);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "⏳  Verifying connectivity...",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("  Provider:  {}", state.product_name)),
        Line::from(format!(
            "  API Key:   {}",
            state.env_var.as_deref().unwrap_or("(none)")
        )),
        Line::from(format!(
            "  Models:    {}",
            if state.models.is_empty() {
                "(auto)".to_string()
            } else {
                state.models.join(", ")
            }
        )),
    ];

    frame.render_widget(Paragraph::new(lines), chunks[1]);
}

fn render_done(frame: &mut Frame, area: Rect, state: &model::DoneState) {
    let title = Paragraph::new(Line::from(Span::styled(
        "✅  Configuration Complete",
        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
    )));

    let mut lines = vec![Line::from("")];
    for result in &state.results {
        let marker = if result.success { "✓" } else { "✗" };
        let s = if result.success {
            Style::new().fg(Color::Green)
        } else {
            Style::new().fg(Color::Red)
        };
        lines.push(Line::from(Span::styled(
            format!("  {}  {} — {}", marker, result.provider, result.message),
            s,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter — Add another    q — Quit",
        Style::new().fg(Color::DarkGray),
    )));

    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(title, chunks[0]);
    frame.render_widget(Paragraph::new(lines), chunks[1]);
}

fn render_oauth_name(frame: &mut Frame, area: Rect, state: &model::OAuthNameState) {
    let title = section_title("OAuth Provider Name".to_string(), None);
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(title, chunks[0]);

    let custom_val = if state.input.is_empty() {
        "___".to_string()
    } else {
        state.input.clone()
    };
    let options: [(String, &str, NameOption); 2] = [
        (
            format!("Recommended: {}", state.recommended_name),
            "",
            NameOption::Recommended,
        ),
        (format!("Custom:   {}", custom_val), "", NameOption::Custom),
    ];

    let mut lines = Vec::new();
    for (text, _, option) in &options {
        let is_selected = *option == state.selected_option;
        let prefix = if is_selected {
            Span::styled(
                "▶ ",
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        };
        let text_style = if is_selected {
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::White)
        };
        lines.push(Line::from(vec![
            prefix,
            Span::styled(text.as_str(), text_style),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), chunks[1]);
}

fn render_oauth_device_code(frame: &mut Frame, area: Rect, state: &model::OAuthDeviceCodeState) {
    let title = section_title(format!("OAuth Login — {}", state.provider_name), None);
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(title, chunks[0]);

    let mut lines = vec![
        Line::from(""),
        Line::from("1. Open browser and visit:"),
        Line::from(""),
        Line::from(Span::styled(
            format!("   {}", state.verification_url),
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("2. Enter this code:"),
        Line::from(""),
        Line::from(Span::styled(
            format!("   {}", state.device_code),
            Style::new()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if state.copied {
        lines.push(Line::from(Span::styled(
            "   ✅ Code copied to clipboard",
            Style::new().fg(Color::Green),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from("⏳  Waiting for authorization..."));

    frame.render_widget(Paragraph::new(lines), chunks[1]);
}

fn render_antigravity_login(frame: &mut Frame, area: Rect, state: &model::AntigravityLoginState) {
    let title = section_title(format!("Antigravity Login — {}", state.provider_name), None);
    let chunks = Layout::vertical([
        Constraint::Length(2), // title
        Constraint::Min(1),    // content
        Constraint::Length(1), // error
    ])
    .split(area);
    frame.render_widget(title, chunks[0]);

    let mut lines = vec![
        Line::from(""),
        Line::from("1. Open browser:"),
        Line::from(state.auth_url.as_str()),
        Line::from(""),
    ];
    if state.copied {
        lines.push(Line::from(Span::styled(
            "   ✅ URL copied to clipboard",
            Style::new().fg(Color::Green),
        )));
    }
    lines.push(Line::from("2. Login to Google and authorize"));
    lines.push(Line::from(""));
    lines.push(Line::from("3. Paste the authorization code:"));
    lines.push(Line::from(Span::styled(
        format!(
            "   {}",
            if state.input.is_empty() {
                "___"
            } else {
                &state.input
            }
        ),
        Style::new().bg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), chunks[1]);

    // Show error if any
    if let Some(err) = &state.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  Error: {}", err),
                Style::new().fg(Color::Red),
            ))),
            chunks[2],
        );
    }
}

fn render_oauth_overwrite(frame: &mut Frame, area: Rect, state: &model::OAuthOverwriteState) {
    let title = Paragraph::new(Line::from(Span::styled(
        "⚠️  Overwrite Warning",
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )));
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(title, chunks[0]);

    let mut lines = vec![
        Line::from(""),
        Line::from(format!(
            "Provider \"{}\" already exists.",
            state.provider_name
        )),
        Line::from(format!("Account: {}", state.account_email)),
        Line::from(""),
        Line::from("This will overwrite the existing login info."),
        Line::from(""),
    ];

    let options = ["Confirm overwrite", "Cancel"];
    for (i, opt) in options.iter().enumerate() {
        let selected = (i == 0 && state.selected_option == WarningOption::Continue)
            || (i == 1 && state.selected_option == WarningOption::Back);
        if selected {
            lines.push(Line::from(vec![
                Span::styled(
                    "▶ ",
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    *opt,
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
            ]));
        } else {
            lines.push(Line::from(format!("  {}", opt)));
        }
    }

    frame.render_widget(Paragraph::new(lines), chunks[1]);
}

// ── Copy Model Confirmation Screen ──────────────────────────────────────

fn render_copy_model_confirm(frame: &mut Frame, area: Rect, state: &model::CopyModelConfirmState) {
    use model::NameValidation;

    let title = section_title(format!("Copy Model — {}", state.source_model_id), None);

    let chunks = Layout::vertical([
        Constraint::Length(2), // title
        Constraint::Length(1), // source info
        Constraint::Length(3), // input display
        Constraint::Length(1), // validation
        Constraint::Length(1), // warning
        Constraint::Length(1), // error
        Constraint::Min(1),    // spacer
    ])
    .split(area);

    frame.render_widget(title, chunks[0]);

    // Source model info
    let source_info = Paragraph::new(Line::from(vec![
        Span::styled("  源模型: ", Style::new().fg(Color::DarkGray)),
        Span::styled(
            &state.source_model_id,
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  [{}]", state.product_name),
            Style::new().fg(Color::Magenta),
        ),
    ]));
    frame.render_widget(source_info, chunks[1]);

    // Input display
    if state.editing {
        // Editing mode: show input with cursor
        let cursor_pos = state.cursor_pos.min(state.new_id.len());
        let before = &state.new_id[..cursor_pos];
        let cursor_char = if cursor_pos < state.new_id.len() {
            &state.new_id[cursor_pos..cursor_pos + 1]
        } else {
            " "
        };
        let after = if cursor_pos + 1 < state.new_id.len() {
            &state.new_id[cursor_pos + 1..]
        } else {
            ""
        };

        let cursor_span = Span::styled(
            cursor_char,
            Style::new()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

        let input_line = Line::from(vec![
            Span::styled("  新 Model ID: ", Style::new().fg(Color::DarkGray)),
            Span::raw(before),
            cursor_span,
            Span::raw(after),
        ]);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                input_line,
                Line::from(Span::styled(
                    "  (a-z 0-9 - _ .   Enter 确认   Esc 取消编辑)",
                    Style::new().fg(Color::DarkGray),
                )),
            ]),
            chunks[2],
        );
    } else {
        // Browse mode: show name as pre-filled
        let name_display = if state.new_id.is_empty() {
            "___".to_string()
        } else {
            state.new_id.clone()
        };

        let input_line = Line::from(vec![
            Span::styled("  新 Model ID: ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                &name_display,
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                input_line,
                Line::from(Span::styled(
                    "  [e] 编辑 ID  [Enter] 确认复制",
                    Style::new().fg(Color::DarkGray),
                )),
            ]),
            chunks[2],
        );
    }

    // Validation message
    let (validation_text, validation_style) = match &state.validation {
        NameValidation::Available => ("✓ ID 可用", Style::new().fg(Color::Green)),
        NameValidation::Conflict => ("✗ ID 已被占用", Style::new().fg(Color::Red)),
    };
    let validation = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(validation_text, validation_style),
    ]));
    frame.render_widget(validation, chunks[3]);

    // Warning: no bindings copied
    let warning = Paragraph::new(Line::from(Span::styled(
        "  ⚠ 将创建新 Model ID，需手动添加 provider 绑定",
        Style::new().fg(Color::Yellow),
    )));
    frame.render_widget(warning, chunks[4]);

    // Error message
    if let Some(ref err) = state.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  Error: {}", err),
                Style::new().fg(Color::Red),
            ))),
            chunks[5],
        );
    }
}

// ── Fallback Config Screen ──────────────────────────────────────────────

fn render_fallback_config(frame: &mut Frame, area: Rect, state: &model::FallbackConfigState) {
    use model::FallbackFocus;

    let title = section_title("Fallback 配置".to_string(), None);

    let chunks = Layout::vertical([
        Constraint::Length(2), // title
        Constraint::Length(1), // subtitle
        Constraint::Length(1), // section: target providers header
        Constraint::Min(3),    // target providers list + options
        Constraint::Length(1), // error / hint
    ])
    .split(area);

    frame.render_widget(title, chunks[0]);

    // Subtitle
    let subtitle = Paragraph::new(Line::from(Span::styled(
        "  为其他同产品 Provider 提供 fallback 兜底（可选，Esc 跳过）",
        Style::new().fg(Color::DarkGray),
    )));
    frame.render_widget(subtitle, chunks[1]);

    // Target providers section header
    let target_header_style = if state.focus == FallbackFocus::TargetProvider {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    let target_header = Paragraph::new(Line::from(vec![
        Span::styled("  目标 Provider（被兜底的）", target_header_style),
        Span::raw("  "),
        Span::styled("[Tab] 切换区域", Style::new().fg(Color::DarkGray)),
    ]));
    frame.render_widget(target_header, chunks[2]);

    // Target providers + options in the main area
    let mut lines: Vec<Line> = Vec::new();

    // Target providers list
    if state.target_providers.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (无同产品的其他 Provider)",
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        for (i, name) in state.target_providers.iter().enumerate() {
            let is_selected =
                state.focus == FallbackFocus::TargetProvider && i == state.target_cursor;
            let prefix = if is_selected { "  ▶ " } else { "    " };
            let style = if is_selected {
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(
                format!("{}{}", prefix, name),
                style,
            )));
        }
    }

    // Separator
    lines.push(Line::from(""));

    // Options section header
    let options_header_style = if state.focus == FallbackFocus::Options {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    lines.push(Line::from(Span::styled(
        "  (model, endpoint) 组合：",
        options_header_style,
    )));

    // Options list
    if state.options.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (该产品暂无可选模型模板)",
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        for (i, opt) in state.options.iter().enumerate() {
            let is_cursor = state.focus == FallbackFocus::Options && i == state.option_cursor;
            let mark = if opt.selected { "[✓]" } else { "[ ]" };
            let mark_style = if opt.selected {
                Style::new().fg(Color::Green)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            let name_style = if is_cursor {
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
            } else if opt.selected {
                Style::new().fg(Color::White)
            } else {
                Style::new().fg(Color::Gray)
            };
            let prefix = if is_cursor { "  ▶ " } else { "    " };
            lines.push(Line::from(vec![
                Span::styled(
                    prefix,
                    if is_cursor {
                        Style::new().fg(Color::Green)
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(format!("{} ", mark), mark_style),
                Span::styled(format!("{} ", opt.model_display_name), name_style),
                Span::styled(format!("[{}]", opt.endpoint), Style::new().fg(Color::Blue)),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), chunks[3]);

    // Bottom hint / error
    let bottom_text = if let Some(ref err) = state.error {
        Line::from(Span::styled(
            format!("  Error: {}", err),
            Style::new().fg(Color::Red),
        ))
    } else {
        let selected_count = state.options.iter().filter(|o| o.selected).count();
        Line::from(Span::styled(
            format!(
                "  已选 {} 项  |  [Enter] 确认  [M] 模型选择  [Esc] 跳过",
                selected_count
            ),
            Style::new().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(bottom_text), chunks[4]);
}

// ── Provider Management Panel ────────────────────────────────────────────

fn render_provider_management(
    frame: &mut Frame,
    area: Rect,
    state: &model::ProviderManagementState,
) {
    let block = Block::default()
        .title(" 已配置的 Provider ")
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(5),    // provider list
        ])
        .split(inner);

    // Header
    let header_text = if state.filter_active {
        format!("Provider 管理面板  [search: {}]", state.filter)
    } else {
        "Provider 管理面板".to_string()
    };
    let header = Paragraph::new(Line::from(Span::styled(
        header_text,
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(header, chunks[0]);

    // Provider list (filtered)
    let mut lines: Vec<Line> = Vec::new();
    let filtered = state.filtered_providers();
    for (i, provider) in filtered.iter().enumerate() {
        let is_selected = i == state.cursor;
        let status_icon = match provider.status {
            model::ProviderStatus::Ok => Span::styled("✓", Style::new().fg(Color::Green)),
            model::ProviderStatus::Warning => Span::styled("⚠", Style::new().fg(Color::Yellow)),
            model::ProviderStatus::Error => Span::styled("✗", Style::new().fg(Color::Red)),
        };

        let protocols = provider.protocols.join(" · ");
        let prefix = if is_selected { "▶ " } else { "  " };

        let style = if is_selected {
            Style::new().fg(Color::White).bg(Color::DarkGray)
        } else {
            Style::new()
        };

        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(
                format!("{:<24}", provider.name),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<20}", provider.product),
                Style::new().fg(Color::Magenta),
            ),
            Span::styled(
                format!("{:<16}", provider.auth_type),
                Style::new().fg(Color::Cyan),
            ),
            status_icon,
            Span::raw("  "),
            Span::styled(protocols, Style::new().fg(Color::Blue)),
        ]));
    }

    if state.providers.is_empty() {
        lines.push(Line::from(Span::styled(
            "  暂无已配置的 Provider。按 [a] 添加。",
            Style::new().fg(Color::DarkGray),
        )));
    } else if state.filter_active && filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  无匹配结果。按 [Esc] 清除过滤。",
            Style::new().fg(Color::DarkGray),
        )));
    }

    // 显示错误/成功信息
    if let Some(ref error) = state.error {
        let color = if error.starts_with('✓') {
            Color::Green
        } else {
            Color::Red
        };
        lines.push(Line::from(Span::styled(
            format!("  {}", error),
            Style::new().fg(color),
        )));
    }

    frame.render_widget(Paragraph::new(lines), chunks[1]);
}

// ── Provider Detail View ─────────────────────────────────────────────────

fn render_provider_detail(frame: &mut Frame, area: Rect, state: &model::ProviderDetailState) {
    let block = Block::default()
        .title(format!(" Provider › {} ", state.name))
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // product + auth
            Constraint::Min(5),    // endpoints
            Constraint::Min(4),    // models + compat (grows with model list)
        ])
        .split(inner);

    // Product + Auth section
    let auth_section = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("产品: ", Style::new().fg(Color::DarkGray)),
            Span::styled(&state.product, Style::new().fg(Color::Magenta)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("认证: ", Style::new().fg(Color::DarkGray)),
            Span::styled(&state.auth_description, Style::new().fg(Color::Cyan)),
            Span::raw(" "),
            Span::styled(&state.auth_status, Style::new().fg(Color::Green)),
        ]),
    ]);
    frame.render_widget(auth_section, chunks[0]);

    // Endpoints section
    let mut endpoint_lines = vec![Line::from(Span::styled(
        "Endpoints",
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::UNDERLINED),
    ))];
    for ep in &state.endpoints {
        let kind_style = if ep.kind == "native" {
            Style::new().fg(Color::Green)
        } else {
            Style::new().fg(Color::Blue)
        };
        endpoint_lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<12}", ep.protocol),
                Style::new().fg(Color::Blue),
            ),
            Span::styled(format!("{:<50}", ep.url), Style::new().fg(Color::DarkGray)),
            Span::styled(&ep.kind, kind_style),
        ]));
    }
    frame.render_widget(Paragraph::new(endpoint_lines), chunks[1]);

    // Models + Compat section
    let mut info_lines = vec![Line::from(Span::styled(
        "绑定模型",
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::UNDERLINED),
    ))];
    if state.bound_models.is_empty() {
        info_lines.push(Line::from("  (无)"));
    } else {
        for (i, model_id) in state.bound_models.iter().enumerate() {
            let is_cursor = i == state.model_cursor;
            let prefix = if is_cursor { "  ▶ " } else { "    " };
            let name_style = if is_cursor {
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            info_lines.push(Line::from(vec![
                Span::styled(
                    prefix,
                    if is_cursor {
                        Style::new().fg(Color::Green)
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(model_id.clone(), name_style),
            ]));
        }
    }
    info_lines.push(Line::from(""));
    if !state.compat_info.is_empty() {
        info_lines.push(Line::from(Span::styled(
            "Compat",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::UNDERLINED),
        )));
        for info in &state.compat_info {
            info_lines.push(Line::from(format!("  {}", info)));
        }
    }
    frame.render_widget(Paragraph::new(info_lines), chunks[2]);
}

// ── Delete Confirm Dialog ────────────────────────────────────────────────

fn render_delete_confirm(frame: &mut Frame, area: Rect, state: &model::DeleteConfirmState) {
    let block = Block::default()
        .title(" 确认删除 ")
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Red));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),                                         // question
            Constraint::Min(3),                                            // model references
            Constraint::Length(1),                                         // force mode indicator
            Constraint::Length(if state.error.is_some() { 1 } else { 0 }), // error
            Constraint::Length(1),                                         // help bar
        ])
        .split(inner);

    // Question
    let question = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("确定要删除 provider "),
            Span::styled(&state.provider_name, Style::new().fg(Color::Magenta)),
            Span::raw(" 吗？"),
        ]),
    ]);
    frame.render_widget(question, chunks[0]);

    // Model references
    let mut ref_lines = vec![Line::from("")];
    if state.referencing_models.is_empty() {
        ref_lines.push(Line::from(Span::styled(
            "  该 provider 没有被任何模型引用。",
            Style::new().fg(Color::Green),
        )));
    } else {
        ref_lines.push(Line::from(Span::styled(
            "  ⚠ 该 provider 被以下模型引用：",
            Style::new().fg(Color::Yellow),
        )));
        for model in &state.referencing_models {
            ref_lines.push(Line::from(format!("    • {}", model)));
        }
    }
    frame.render_widget(Paragraph::new(ref_lines), chunks[1]);

    // Force mode indicator
    let force_text = if state.force_mode {
        Line::from(Span::styled(
            "  ⚠ 强制模式：将同时删除模型绑定",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else if !state.referencing_models.is_empty() {
        Line::from(Span::styled(
            "  按 [f] 启用强制删除（同时删除模型绑定）",
            Style::new().fg(Color::DarkGray),
        ))
    } else {
        Line::from("")
    };
    frame.render_widget(Paragraph::new(vec![force_text]), chunks[2]);

    // Error message
    if let Some(ref err) = state.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  ✗ {}", err),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ))),
            chunks[3],
        );
    }

    // Help bar
    let help = Line::from(vec![
        Span::styled("[y]", Style::new().fg(Color::Yellow)),
        Span::raw(" 确认  "),
        Span::styled("[n/Esc]", Style::new().fg(Color::Yellow)),
        Span::raw(" 取消  "),
        Span::styled("[f]", Style::new().fg(Color::Yellow)),
        Span::raw(" 强制模式"),
    ]);
    frame.render_widget(Paragraph::new(help), chunks[4]);
}

// ── Reset Usage Confirm Dialog ──────────────────────────────────────────

fn render_reset_usage_confirm(
    frame: &mut Frame,
    area: Rect,
    state: &model::ResetUsageConfirmState,
) {
    let block = Block::default()
        .title(" 确认重置 Usage ")
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let error_height = if state.error.is_some() { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),            // question
            Constraint::Length(3),            // usage info
            Constraint::Length(1),            // credits warning
            Constraint::Length(error_height), // error
            Constraint::Length(1),            // help bar
        ])
        .split(inner);

    // Question
    let question = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("确定要重置 "),
            Span::styled(&state.provider_name, Style::new().fg(Color::Magenta)),
            Span::raw(" 的 usage 统计吗？"),
        ]),
    ]);
    frame.render_widget(question, chunks[0]);

    // Usage info
    let mut info_lines = vec![Line::from("")];
    if let Some(ref info) = state.usage_info {
        info_lines.push(Line::from(Span::styled(
            format!("  {}", info),
            Style::new().fg(Color::Cyan),
        )));
    } else {
        info_lines.push(Line::from(Span::styled(
            "  (无法获取 usage 详情，将执行强制重置)",
            Style::new().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(info_lines), chunks[1]);

    // Credits warning
    let credits_text = match state.credits {
        Some(c) if c <= 1 => Line::from(Span::styled(
            format!("  ⚠ 剩余 {} 个 credit，请谨慎操作", c),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Some(c) => Line::from(Span::styled(
            format!("  剩余 {} 个 credit", c),
            Style::new().fg(Color::DarkGray),
        )),
        None => Line::from(""),
    };
    frame.render_widget(Paragraph::new(vec![credits_text]), chunks[2]);

    // Error message
    if let Some(ref err) = state.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  ✗ {}", err),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ))),
            chunks[3],
        );
    }

    // Help bar
    let help = Line::from(vec![
        Span::styled("[y]", Style::new().fg(Color::Yellow)),
        Span::raw(" 确认重置  "),
        Span::styled("[n/Esc]", Style::new().fg(Color::Yellow)),
        Span::raw(" 取消"),
    ]);
    frame.render_widget(Paragraph::new(help), chunks[4]);
}

fn render_model_rename(frame: &mut Frame, area: Rect, state: &model::ModelRenameState) {
    use ratatui::widgets::Block;

    let block = Block::default()
        .title(" 重命名 Model ")
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),                                           // old model ID
            Constraint::Length(2),                                           // new model ID input
            Constraint::Length(if state.confirm_step == 1 { 3 } else { 0 }), // confirmation prompt
            Constraint::Length(if state.error.is_some() { 2 } else { 0 }),   // error
            Constraint::Min(1),                                              // spacer
        ])
        .split(inner);

    // Old model ID (read-only)
    let old_id_line = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  原始 ID: "),
            Span::styled(&state.old_model_id, Style::new().fg(Color::DarkGray)),
        ]),
    ];
    frame.render_widget(Paragraph::new(old_id_line), chunks[0]);

    // New model ID input
    let new_id_style = if state.editing {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new()
    };
    let new_id_display = if state.editing {
        // Show cursor
        let before = &state.new_model_id[..state.cursor_pos.min(state.new_model_id.len())];
        let after = &state.new_model_id[state.cursor_pos.min(state.new_model_id.len())..];
        vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  新 ID:    "),
                Span::styled(before, new_id_style),
                Span::styled("▌", Style::new().fg(Color::Yellow)),
                Span::styled(after, new_id_style),
            ]),
        ]
    } else {
        vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  新 ID:    "),
                Span::styled(&state.new_model_id, new_id_style),
            ]),
        ]
    };
    frame.render_widget(Paragraph::new(new_id_display), chunks[1]);

    // Confirmation prompt (step 1)
    if state.confirm_step == 1 {
        let confirm_lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  ⚠ 重命名会导致客户端配置失效",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("  [y]", Style::new().fg(Color::Yellow)),
                Span::raw(" 确认  "),
                Span::styled("[n/Esc]", Style::new().fg(Color::Yellow)),
                Span::raw(" 取消"),
            ]),
        ];
        frame.render_widget(Paragraph::new(confirm_lines), chunks[2]);
    }

    // Error
    if let Some(ref err) = state.error {
        let err_lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  {}", err),
                if state.confirm_step == 1 {
                    Style::new().fg(Color::Yellow)
                } else {
                    Style::new().fg(Color::Red)
                },
            )),
        ];
        frame.render_widget(Paragraph::new(err_lines), chunks[3]);
    }
}
