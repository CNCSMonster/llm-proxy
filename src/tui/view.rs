use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
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

    // Layout: content (fill), separator (1), help (1 or 2 lines)
    let help_height = if matches!(
        app.screen,
        Screen::ProviderManagement(_) | Screen::ModelManagement(_)
    ) && inner.height >= 3
    {
        2
    } else {
        1
    };
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(0),
        Constraint::Length(help_height),
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
        Screen::ProbeConfirm(s) => render_probe_confirm(frame, chunks[0], s),
        Screen::CustomProviderEditor(s) => render_custom_provider_editor(frame, chunks[0], s),
        Screen::ProviderNaming(s) => render_provider_naming(frame, chunks[0], s),
        Screen::WarningConfirm(s) => render_warning_confirm(frame, chunks[0], s),
        Screen::Verifying(s) => render_verifying(frame, chunks[0], s),
        Screen::Done(s) => render_done(frame, chunks[0], s),
        Screen::OAuthName(s) => render_oauth_name(frame, chunks[0], s),
        Screen::OAuthDeviceCode(s) => render_oauth_device_code(frame, chunks[0], s),
        Screen::OAuthOverwrite(s) => render_oauth_overwrite(frame, chunks[0], s),
        Screen::FallbackConfig(s) => render_fallback_config(frame, chunks[0], s),
        Screen::CopyModelConfirm(s) => render_copy_model_confirm(frame, chunks[0], s),
        Screen::CopyConfigure(s) => render_copy_configure(frame, chunks[0], s),
        Screen::ModelRename(s) => render_model_rename(frame, chunks[0], s),
        Screen::ModelManagement(s) => render_model_management(frame, chunks[0], s),
        Screen::ModelDetail(s) => render_model_detail(frame, chunks[0], s),
        Screen::ModelDeleteConfirm(s) => render_model_delete_confirm(frame, chunks[0], s),
        Screen::ProtocolBindingEditor(s) => render_protocol_binding_editor(frame, chunks[0], s),
        Screen::Quit => {}
    }

    // Help bar at the bottom
    let help_text = app.help_text();
    let help =
        Paragraph::new(Text::from_iter(help_text.lines().map(|line| {
            Line::from(Span::styled(line, Style::new().fg(Color::Gray)))
        })))
        .alignment(ratatui::layout::Alignment::Left);
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

    let status_height = if state.error.is_some() {
        Constraint::Length(1)
    } else {
        Constraint::Length(0)
    };
    let chunks =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1), status_height]).split(area);
    frame.render_widget(title, chunks[0]);

    let list = widgets::product_list(&state.items, state.cursor, &state.filter);
    let mut list_state = ratatui::widgets::ListState::default();

    // 计数必须与 product_list 内部过滤谓词一致，否则高亮项会偏移
    let filtered_items = widgets::filtered_product_count(&state.items, &state.filter);
    if filtered_items > 0 {
        list_state.select(Some(state.cursor.min(filtered_items.saturating_sub(1))));
    }

    frame.render_stateful_widget(list, chunks[1], &mut list_state);

    if let Some(ref err) = state.error {
        let status_widget = Paragraph::new(Line::from(vec![Span::styled(
            format!("  {err}"),
            Style::new().fg(Color::Red),
        )]));
        frame.render_widget(status_widget, chunks[2]);
    }
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
    // 计数必须与 env_list 内部过滤谓词一致，否则高亮项会偏移
    let filtered_items = widgets::filtered_env_count(&state.items, &state.filter);
    if filtered_items > 0 {
        list_state.select(Some(state.cursor.min(filtered_items.saturating_sub(1))));
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
    let title_prefix = if state.is_openrouter && state.is_degraded {
        format!("⚠️  Select Models — {} (离线降级缓存)", state.product_name)
    } else {
        format!("Select Models — {}", state.product_name)
    };
    let title = section_title(
        title_prefix,
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
    // 计数必须与 model_list 内部过滤谓词一致，否则高亮项会偏移
    let filtered_items = widgets::filtered_model_count(&state.items, &state.filter);
    if filtered_items > 0 {
        list_state.select(Some(state.cursor.min(filtered_items.saturating_sub(1))));
    }
    frame.render_stateful_widget(list, chunks[1], &mut list_state);

    // Status bar：错误优先展示，避免操作失败（如"至少选择一个模型"）无任何反馈
    let (status_text, status_style) = if let Some(ref err) = state.error {
        (
            format!(" {err}  "),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if state.is_openrouter {
        if state.is_degraded {
            (
                format!(
                    " ⚠️  离线缓存 · {} 模型 (网络不可达) · [/] 过滤 [r] 重试  ",
                    total
                ),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )
        } else if selected_count > 0 {
            (
                format!(
                    " 已选择 {} / {} · [/] 过滤 [r] 刷新 [Space] 勾选 [Enter] 确认  ",
                    selected_count, total
                ),
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            )
        } else {
            (
                format!(
                    " OpenRouter · {} models · [/] 过滤 [r] 刷新 [Space] 勾选 [Enter] 确认  ",
                    total
                ),
                Style::new().fg(Color::Cyan),
            )
        }
    } else if selected_count > 0 {
        (
            format!(" {} of {} selected  ", selected_count, total),
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        )
    } else if matches!(
        state.source,
        model::ModelSelectionSource::ProviderReselect { .. }
    ) {
        (
            " 至少选择一个模型  ".to_string(),
            Style::new().fg(Color::Yellow),
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
            let input_line = hscroll_input_line(
                "  > ",
                display,
                cursor_pos,
                chunks[1].width as usize,
                InputLineStyle::plain(),
            );

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
        let input_line = hscroll_input_line(
            "  > ",
            &state.input,
            cursor_pos,
            chunks[2].width as usize,
            InputLineStyle::plain(),
        );
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

/// 探测确认屏（ 方案 A）：默认高亮「探测」，连按两次回车即走默认路径。
///
/// 位置：本地 env var 存在性检查**之后**、网络有效性探测**之前**。
fn render_probe_confirm(frame: &mut Frame, area: Rect, state: &model::ProbeConfirmState) {
    let title = Paragraph::new(Line::from(Span::styled(
        format!("🔍  探测可用性 — {}", state.product_name),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));

    let mut lines = vec![
        Line::from(""),
        Line::from("本地凭据校验已通过（env var / OAuth 账号存在）。"),
        Line::from("网络探测会向已选模型的 endpoint 实际发请求，用以确认上游可用。"),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "将探测 {} 个模型（约 {}~{} 次请求）",
                state.models.len(),
                state.models.len(),
                state.models.len() * 2
            ),
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];

    let options = ["探测（推荐）", "跳过探测"];
    for (i, opt) in options.iter().enumerate() {
        if state.cursor == i {
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
    // wrap：验证失败原因可能很长（含 URL/详情），窄终端下不再被静默截断
    frame.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
        chunks[1],
    );
}

fn render_verifying(frame: &mut Frame, area: Rect, state: &model::VerifyingState) {
    let title = section_title("Verifying".to_string(), None);
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(title, chunks[0]);

    let status_line = if state.progress_total > 0 {
        let current = state.progress_current_model.as_deref().unwrap_or("...");
        Line::from(Span::styled(
            format!(
                "⠋  探针验证中 ({}/{}): {}",
                state.progress_done, state.progress_total, current
            ),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))
    } else if state.skip_probe {
        Line::from(Span::styled(
            "⏳  写入配置中...",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            "⏳  Verifying connectivity...",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))
    };

    let lines = vec![
        Line::from(""),
        status_line,
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

    frame.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
        chunks[1],
    );
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
    if let Some(ref err) = state.error {
        lines.push(Line::from(Span::styled(
            format!("✗ {err}"),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "   按 [r/Enter] 重试登录",
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from("⏳  正在准备登录（将暂时退出 TUI 打开浏览器）…"));
    }

    frame.render_widget(Paragraph::new(lines), chunks[1]);
}

#[cfg_attr(not(test), allow(dead_code))]
fn wrap_ascii_url(url: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    url.as_bytes()
        .chunks(width)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect()
}

/// URL 展示行左右各留 2 空格边距，避免终端选中复制时把 TUI 边框字符一起粘进 URL。
#[cfg_attr(not(test), allow(dead_code))]
const URL_DISPLAY_MARGIN: usize = 4;

/// URL 展示行：按内容宽度留边折行并整体缩进，保证手动选中复制时 URL 两侧只有空白。
#[cfg_attr(not(test), allow(dead_code))]
fn url_display_lines(url: &str, content_width: usize) -> Vec<Line<'static>> {
    let wrap_width = content_width.saturating_sub(URL_DISPLAY_MARGIN);
    if wrap_width == 0 {
        return Vec::new();
    }
    wrap_ascii_url(url, wrap_width)
        .into_iter()
        .map(|part| Line::from(format!("  {part}")))
        .collect()
}

/// 把字节下标对齐到字符边界（截窗计算可能落在多字节字符中间）。
fn snap_to_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// 输入行外观：内容样式 + 竖条光标样式。随界面各自演化的部分全部参数化。
struct InputLineStyle {
    content: Style,
    cursor_style: Style,
}

impl InputLineStyle {
    /// 默认外观：竖条光标（用户审美偏好），黄色前景。
    fn plain() -> Self {
        Self {
            content: Style::new(),
            cursor_style: Style::new().fg(Color::Yellow),
        }
    }
}

/// 构造带横向滚动窗口的输入行：内容超出可用宽度时以光标为锚点截取可见片段，
/// 保证光标始终在可视区内（窄终端下长输入的尾部不再被静默裁掉，可盲打问题）。
/// 光标为竖条 ▌，插入在光标位置（光标处字符保留在竖条右侧）。
fn hscroll_input_line(
    prefix: &str,
    display: &str,
    cursor_pos: usize,
    width: usize,
    style: InputLineStyle,
) -> Line<'static> {
    let available = width.saturating_sub(prefix.len());
    if available == 0 {
        return Line::from(vec![Span::raw(prefix.to_string())]);
    }
    let byte_len = display.len();
    let cursor_pos = snap_to_char_boundary(display, cursor_pos.min(byte_len));

    let build = |start: usize, end: usize, left_ellipsis: bool, right_ellipsis: bool| {
        let start = snap_to_char_boundary(display, start);
        let end = snap_to_char_boundary(display, end).max(start);
        let before = display[start..cursor_pos].to_string();
        // 竖条占 1 列且不替换内容字符：截窗（右侧有省略号）时 after 右边界让出 1 列，
        // 全量渲染时不需要
        let content_end = if right_ellipsis {
            end.saturating_sub(1)
        } else {
            end
        };
        let after_end = snap_to_char_boundary(display, content_end);
        let after = display[cursor_pos.min(after_end)..after_end].to_string();
        let ellipsis_style = Style::new().fg(Color::DarkGray);
        let mut spans = vec![Span::raw(prefix.to_string())];
        if left_ellipsis {
            spans.push(Span::styled("…", ellipsis_style));
        }
        spans.push(Span::styled(before, style.content));
        spans.push(Span::styled("▌", style.cursor_style));
        spans.push(Span::styled(after, style.content));
        if right_ellipsis {
            spans.push(Span::styled("…", ellipsis_style));
        }
        spans
    };

    // 全量可放下（含竖条 1 列）：直接渲染
    if byte_len < available {
        return Line::from(build(0, byte_len, false, false));
    }

    // 截窗：两侧各留 1 列给省略号
    let window = available.saturating_sub(2).max(1);
    let (start, end, left_e, right_e) = if cursor_pos.saturating_sub(1) <= window / 2 {
        (0, window.min(byte_len), false, true)
    } else if byte_len.saturating_sub(cursor_pos) <= window / 2 + 1 {
        (byte_len.saturating_sub(window), byte_len, true, false)
    } else {
        let start = cursor_pos - window / 2;
        ((start), (start + window).min(byte_len), true, true)
    };
    Line::from(build(start, end, left_e, right_e))
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

// ── Copy Provider Configuration Screen ──────────────────────────────────

fn render_copy_configure(frame: &mut Frame, area: Rect, state: &model::CopyConfigureState) {
    use model::{CopyAuthKind, CopyConfigureStep, NameValidation};

    let title = section_title(format!("Copy Provider — {}", state.source_id), None);
    let chunks = Layout::vertical([
        Constraint::Length(2), // title
        Constraint::Length(3), // source info
        Constraint::Length(3), // name input
        Constraint::Length(1), // validation
        Constraint::Min(3),    // auth / note
        Constraint::Length(1), // error
    ])
    .split(area);

    frame.render_widget(title, chunks[0]);

    let source_info = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("  源 Provider: ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                &state.source_id,
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  [{}]", state.source_product),
                Style::new().fg(Color::Magenta),
            ),
        ]),
        Line::from(vec![
            Span::styled("  认证: ", Style::new().fg(Color::DarkGray)),
            Span::raw(&state.source_auth_label),
            Span::styled("  端点: ", Style::new().fg(Color::DarkGray)),
            Span::raw(if state.source_endpoints.is_empty() {
                "—"
            } else {
                &state.source_endpoints
            }),
        ]),
        Line::from(Span::styled(
            "  复制不创建模型绑定；OAuth token 也不会被复制。",
            Style::new().fg(Color::DarkGray),
        )),
    ]);
    frame.render_widget(source_info, chunks[1]);

    let name_line = if state.editing {
        let cursor_pos = state.cursor_pos.min(state.input.len());
        let input = hscroll_input_line(
            "  新名称: ",
            &state.input,
            cursor_pos,
            chunks[2].width as usize,
            InputLineStyle::plain(),
        );
        Line::from(input.spans)
    } else {
        Line::from(vec![
            Span::styled("  新名称: ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                if state.input.is_empty() {
                    "___"
                } else {
                    &state.input
                },
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ])
    };
    frame.render_widget(
        Paragraph::new(vec![Line::from(""), name_line, Line::from("")]),
        chunks[2],
    );

    let (validation_text, validation_style) = match &state.validation {
        NameValidation::Available => ("✓ 名字可用", Style::new().fg(Color::Green)),
        NameValidation::Conflict => ("✗ 名字已被占用", Style::new().fg(Color::Red)),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(validation_text, validation_style),
        ])),
        chunks[3],
    );

    let auth_lines = match (state.step, state.auth_kind) {
        (CopyConfigureStep::Name, _) => vec![Line::from(Span::styled(
            "  [Enter] 下一步：确认认证",
            Style::new().fg(Color::DarkGray),
        ))],
        (CopyConfigureStep::Auth, CopyAuthKind::ApiKey) => {
            let env_line = if state.env_editing {
                let cursor_pos = state.env_cursor_pos.min(state.env_input.len());
                let input = hscroll_input_line(
                    "  api_key_env: ",
                    &state.env_input,
                    cursor_pos,
                    chunks[4].width as usize,
                    InputLineStyle::plain(),
                );
                Line::from(input.spans)
            } else {
                Line::from(vec![
                    Span::styled("  api_key_env: ", Style::new().fg(Color::DarkGray)),
                    Span::styled(
                        if state.env_input.is_empty() {
                            "（未确认）"
                        } else {
                            &state.env_input
                        },
                        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                    ),
                ])
            };
            vec![
                env_line,
                Line::from(Span::styled(
                    "  必须显式确认环境变量名；工具不会根据 Provider ID 编造变量名。",
                    Style::new().fg(Color::Yellow),
                )),
                Line::from(Span::styled(
                    "  [e] 编辑  [s] 无密钥复制  [Enter] 确认复制",
                    Style::new().fg(Color::DarkGray),
                )),
            ]
        }
        (CopyConfigureStep::Auth, CopyAuthKind::None) => vec![
            Line::from(Span::styled(
                "  源 Provider 无认证，复制后仍为无认证。",
                Style::new().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  [Enter] 确认复制",
                Style::new().fg(Color::DarkGray),
            )),
        ],
        (CopyConfigureStep::Auth, _) => vec![
            Line::from(Span::styled(
                "  OAuth 只复制配置；新实例使用独立 account，需重新登录。",
                Style::new().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "  [Enter] 复制并进入登录",
                Style::new().fg(Color::DarkGray),
            )),
        ],
    };
    frame.render_widget(Paragraph::new(auth_lines), chunks[4]);

    if let Some(ref err) = state.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  Error: {err}"),
                Style::new().fg(Color::Red),
            ))),
            chunks[5],
        );
    }
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
        let input = hscroll_input_line(
            "  新 Model ID: ",
            &state.new_id,
            cursor_pos,
            chunks[2].width as usize,
            InputLineStyle::plain(),
        );
        let input_line = Line::from(input.spans);
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

    // Copy scope and source-model safety notice
    let warning = Paragraph::new(Line::from(Span::styled(
        "  ⚠ 将完整复制模型参数及 provider 绑定；源模型保持不变",
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

    // 滚动窗口：scroll_offset 由 update 层按键/resize 时维护，保证焦点光标行始终可见
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(state.scroll_offset)
        .take(chunks[3].height as usize)
        .collect();
    frame.render_widget(Paragraph::new(visible), chunks[3]);

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

        let cooldown_span = match &provider.cooldown {
            Some(until) => Span::styled(
                format!("  [COOLDOWN until {until}]"),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            None => Span::styled("  [OK]", Style::new().fg(Color::Green)),
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
            cooldown_span,
        ]));
    }

    if state.providers.is_empty() {
        lines.push(Line::from(Span::styled(
            "  暂无已配置的 Provider。按 [a] 添加。",
            Style::new().fg(Color::DarkGray),
        )));
        frame.render_widget(Paragraph::new(lines), chunks[1]);
        return;
    }
    if state.filter_active && filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  无匹配结果。按 [Esc] 清除过滤。",
            Style::new().fg(Color::DarkGray),
        )));
        frame.render_widget(Paragraph::new(lines), chunks[1]);
        return;
    }

    // 滚动窗口：error 消息固定渲染在列表底部，其余区域展示 scroll_offset 起的窗口，
    // 保证 cursor 始终可见（scroll_offset 由 update 层按键/resize 时维护）。
    let error_rows = state.error_row_count() as u16;
    let list_height = chunks[1].height.saturating_sub(error_rows) as usize;
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(state.scroll_offset)
        .take(list_height)
        .collect();
    frame.render_widget(Paragraph::new(visible), chunks[1]);

    // 显示错误/成功信息
    if let Some(ref error) = state.error {
        let color = if error.starts_with('✓') {
            Color::Green
        } else {
            Color::Red
        };
        let error_area = Rect {
            x: chunks[1].x,
            y: chunks[1].y + chunks[1].height - error_rows,
            width: chunks[1].width,
            height: error_rows,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {}", error),
                Style::new().fg(color),
            ))),
            error_area,
        );
    }
}

// ── Provider Detail View ─────────────────────────────────────────────────

fn render_provider_detail(frame: &mut Frame, area: Rect, state: &model::ProviderDetailState) {
    let block = Block::default()
        .title(format!(" Provider › {} ", state.name))
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 配额分组渲染已随配额特性裁剪，认证区固定 3 行。
    let auth_height = 3;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(auth_height), // product + auth + quota
            Constraint::Min(5),              // endpoints
            Constraint::Min(4),              // models + compat (grows with model list)
        ])
        .split(inner);

    // Product + Auth section (± Quota)
    let auth_lines = vec![
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
    ];

    let auth_section = Paragraph::new(auth_lines);
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
    // 滚动窗口：scroll_offset 由 update 层按键/resize 时维护，保证光标行始终可见
    let visible: Vec<Line> = info_lines
        .into_iter()
        .skip(state.scroll_offset)
        .take(chunks[2].height as usize)
        .collect();
    frame.render_widget(Paragraph::new(visible), chunks[2]);
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

    // Model references：超出可视区时截断并给出明确指示，避免用户在看不到完整
    // 受影响模型清单的情况下做强制删除决策
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
        let available = (chunks[1].height as usize).saturating_sub(ref_lines.len() + 1);
        let shown = state.referencing_models.len().min(available);
        for model in state.referencing_models.iter().take(shown) {
            ref_lines.push(Line::from(format!("    • {}", model)));
        }
        let remaining = state.referencing_models.len() - shown;
        if remaining > 0 {
            ref_lines.push(Line::from(Span::styled(
                format!("    … 还有 {remaining} 个模型未显示"),
                Style::new().fg(Color::Yellow),
            )));
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
        // Show cursor（带横向滚动窗口，窄终端下长 ID 尾部不再被裁掉；
        // 该界面沿用竖条光标 ▌，外观差异通过 InputLineStyle 参数保留）
        let cursor_pos = state.cursor_pos.min(state.new_model_id.len());
        let input = hscroll_input_line(
            "  新 ID:    ",
            &state.new_model_id,
            cursor_pos,
            chunks[1].width as usize,
            InputLineStyle {
                content: new_id_style,
                cursor_style: Style::new().fg(Color::Yellow),
            },
        );
        vec![Line::from(""), input]
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

fn render_model_management(frame: &mut Frame, area: Rect, state: &model::ModelManagementState) {
    let block = Block::default()
        .title(" 已配置的 Model ")
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(5)])
        .split(inner);

    let header_text = if state.filter_active {
        format!("Model 管理面板  [search: {}]", state.filter)
    } else {
        "Model 管理面板".to_string()
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            header_text,
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );

    let mut lines: Vec<Line> = Vec::new();
    let filtered = state.filtered_models();
    for (i, item) in filtered.iter().enumerate() {
        let is_selected = i == state.cursor;
        let style = if is_selected {
            Style::new().fg(Color::White).bg(Color::DarkGray)
        } else {
            Style::new()
        };
        let prefix = if is_selected { "▶ " } else { "  " };
        // a routing model's children are its routes, not providers.
        let providers = if item.route_count > 0 {
            format!("{} routes", item.route_count)
        } else {
            format!("{} providers", item.provider_count)
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(
                format!("{:<28}", item.id),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{:>7}  {:>6}  ",
                    item.context_window, item.max_output_tokens
                ),
                Style::new().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{:<10}", item.thinking),
                Style::new().fg(Color::Cyan),
            ),
            Span::styled(
                format!("{:<20}", item.features),
                Style::new().fg(Color::Magenta),
            ),
            Span::styled(providers, Style::new().fg(Color::Blue)),
        ]));
    }
    if state.models.is_empty() {
        lines.push(Line::from(Span::styled(
            "  暂无已配置的 Model。按 [a] 添加。",
            Style::new().fg(Color::DarkGray),
        )));
        frame.render_widget(Paragraph::new(lines), chunks[1]);
        return;
    }
    if state.filter_active && filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  无匹配结果。按 [Esc] 清除过滤。",
            Style::new().fg(Color::DarkGray),
        )));
        frame.render_widget(Paragraph::new(lines), chunks[1]);
        return;
    }

    // 滚动窗口：error 消息固定渲染在列表底部，其余区域展示 scroll_offset 起的窗口，
    // 保证 cursor 始终可见（scroll_offset 由 update 层按键/resize 时维护）。
    let error_rows = state.error_row_count() as u16;
    let list_height = chunks[1].height.saturating_sub(error_rows) as usize;
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(state.scroll_offset)
        .take(list_height)
        .collect();
    frame.render_widget(Paragraph::new(visible), chunks[1]);

    if let Some(ref error) = state.error {
        let color = if error.starts_with('✓') {
            Color::Green
        } else {
            Color::Red
        };
        let error_area = Rect {
            x: chunks[1].x,
            y: chunks[1].y + chunks[1].height - error_rows,
            width: chunks[1].width,
            height: error_rows,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {error}"),
                Style::new().fg(color),
            ))),
            error_area,
        );
    }
}

fn render_model_detail(frame: &mut Frame, area: Rect, state: &model::ModelDetailState) {
    use model::ModelDetailField;

    let title = match &state.kind {
        model::ModelDetailKind::Existing { .. } => {
            if state.is_routing_model {
                format!(" Model › {} [Routing Model - Read Only] ", state.model_id)
            } else {
                format!(" Model › {} ", state.model_id)
            }
        }
        model::ModelDetailKind::Add => " 新增 Model ".to_string(),
        model::ModelDetailKind::Copy { source_id } => format!(" 复制 Model ← {source_id} "),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = Vec::new();
    for (i, field) in state.fields().iter().enumerate() {
        let selected = i == state.cursor;
        let prefix = if selected { "▶ " } else { "  " };
        let value = if selected
            && state.editing
            && !field.is_binding()
            && *field != ModelDetailField::Thinking
            && *field != ModelDetailField::Routes
        {
            format!(
                "{}▌{}",
                &state.edit_buffer[..state.edit_cursor.min(state.edit_buffer.len())],
                &state.edit_buffer[state.edit_cursor.min(state.edit_buffer.len())..]
            )
        } else {
            match field {
                ModelDetailField::Id => state.model_id.clone(),
                ModelDetailField::ContextWindow => {
                    if state.is_routing_model {
                        format!("{} (derived)", state.context_window)
                    } else {
                        state.context_window.clone()
                    }
                }
                ModelDetailField::MaxOutput => {
                    if state.is_routing_model {
                        format!("{} (derived)", state.max_output_tokens)
                    } else {
                        state.max_output_tokens.clone()
                    }
                }
                ModelDetailField::Thinking => {
                    if state.is_routing_model {
                        if !state.supported_reasoning_levels.is_empty() {
                            if !state.default_reasoning_level.is_empty() {
                                format!(
                                    "levels: {} (default: {})",
                                    state.supported_reasoning_levels, state.default_reasoning_level
                                )
                            } else {
                                format!("levels: {}", state.supported_reasoning_levels)
                            }
                        } else if state.thinking_enabled {
                            "on".to_string()
                        } else {
                            "off".to_string()
                        }
                    } else if state.thinking_enabled {
                        if state.default_reasoning_level.is_empty() {
                            "on".to_string()
                        } else {
                            state.default_reasoning_level.clone()
                        }
                    } else {
                        "off".to_string()
                    }
                }
                ModelDetailField::Features => {
                    if state.features.is_empty() {
                        "(none)".to_string()
                    } else if state.is_routing_model {
                        format!("{} (derived)", state.features)
                    } else {
                        state.features.clone()
                    }
                }
                ModelDetailField::ChatBindings => format!("{} bindings", state.chat_bindings.len()),
                ModelDetailField::ResponsesBindings => {
                    format!("{} bindings", state.responses_bindings.len())
                }
                ModelDetailField::AnthropicBindings => {
                    format!("{} bindings", state.anthropic_bindings.len())
                }
                ModelDetailField::Routes => {
                    format!("{} routes", state.routes.len())
                }
            }
        };
        let style = if selected {
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(
                format!("{:<26}", field.label()),
                Style::new().fg(Color::DarkGray),
            ),
            Span::styled(value, style),
        ]));
        if field.is_binding() {
            let bindings = match field {
                ModelDetailField::ChatBindings => &state.chat_bindings,
                ModelDetailField::ResponsesBindings => &state.responses_bindings,
                ModelDetailField::AnthropicBindings => &state.anthropic_bindings,
                _ => continue,
            };
            for (index, binding) in bindings.iter().enumerate() {
                let mut spans = vec![Span::styled(
                    format!(
                        "      [{:>2}] {}  upstream: {}",
                        index + 1,
                        binding.name,
                        binding.upstream
                    ),
                    Style::new().fg(Color::Blue),
                )];
                match &binding.cooldown {
                    Some(until) => {
                        spans.push(Span::styled(
                            format!("  [COOLDOWN until {until}]"),
                            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ));
                    }
                    None => {
                        spans.push(Span::styled("  [OK]", Style::new().fg(Color::Green)));
                    }
                }
                lines.push(Line::from(spans));
            }
        } else if *field == ModelDetailField::Routes {
            for (index, route) in state.routes.iter().enumerate() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "      [{:>2}] level: {:<8} → target: {}",
                        index + 1,
                        route.level,
                        route.target
                    ),
                    Style::new().fg(Color::Cyan),
                )));
            }
        }
    }
    // 滚动窗口：error 固定底部（空行 + 消息行），其余区域展示 scroll_offset 起的窗口，
    // 保证当前字段行始终可见（scroll_offset 由 update 层按键/resize 时维护）。
    let error_rows = state.error_row_count() as u16;
    let list_height = inner.height.saturating_sub(error_rows) as usize;
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(state.scroll_offset)
        .take(list_height)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);

    if let Some(ref error) = state.error {
        let color = if error.starts_with('✓') {
            Color::Green
        } else {
            Color::Red
        };
        let error_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - error_rows,
            width: inner.width,
            height: error_rows,
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(format!("  {error}"), Style::new().fg(color))),
            ]),
            error_area,
        );
    }
}

fn render_model_delete_confirm(
    frame: &mut Frame,
    area: Rect,
    state: &model::ModelDeleteConfirmState,
) {
    let block = Block::default()
        .title(" 确认删除 Model ")
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Red));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("确定要删除模型 "),
            Span::styled(
                &state.model_id,
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ？"),
        ]),
        Line::from(Span::styled(
            "  不会删除任何 Provider 或凭据。",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    if let Some(ref error) = state.error {
        lines.push(Line::from(Span::styled(
            format!("  {error}"),
            Style::new().fg(Color::Red),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_protocol_binding_editor(
    frame: &mut Frame,
    area: Rect,
    state: &model::ProtocolBindingEditorState,
) {
    let block = Block::default()
        .title(format!(
            " {} bindings › {} ",
            state.protocol.field_name(),
            state.model_id
        ))
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = Vec::new();
    if state.rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (无绑定。按 [a] 添加已配置 Provider)",
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        for (i, row) in state.rows.iter().enumerate() {
            let selected = i == state.cursor && !state.adding;
            let prefix = if selected { "› " } else { "  " };
            let style = if selected {
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            let upstream = if selected && state.editing_upstream {
                format!("{}▌", state.upstream_input)
            } else {
                row.upstream.clone()
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{}[{:>2}] {} ({})  upstream: {}",
                    prefix,
                    i + 1,
                    row.name,
                    row.kind,
                    upstream
                ),
                style,
            )));
        }
    }
    if state.adding {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  选择要添加的 Provider：",
            Style::new().fg(Color::Cyan),
        )));
        for (i, name) in state.candidates.iter().enumerate() {
            let selected = i == state.candidate_cursor;
            lines.push(Line::from(Span::styled(
                format!("{}{}", if selected { "  ▶ " } else { "    " }, name),
                if selected {
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                },
            )));
        }
    }
    // 滚动窗口：error 固定底部（空行 + 消息行），其余区域展示 scroll_offset 起的窗口，
    // 保证活动光标行（rows 区或 adding 时的候选区）始终可见。
    let error_rows = state.error_row_count() as u16;
    let list_height = inner.height.saturating_sub(error_rows) as usize;
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(state.scroll_offset)
        .take(list_height)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);

    if let Some(ref error) = state.error {
        let color = if error.starts_with('✓') {
            Color::Green
        } else {
            Color::Yellow
        };
        let error_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - error_rows,
            width: inner.width,
            height: error_rows,
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(format!("  {error}"), Style::new().fg(color))),
            ]),
            error_area,
        );
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::{
        InputLineStyle, URL_DISPLAY_MARGIN, hscroll_input_line, url_display_lines, wrap_ascii_url,
    };
    use ratatui::style::{Color, Style};

    #[test]
    fn url_wrap_preserves_bytes_and_width() {
        let url = "https://example.test/oauth-callback?code_challenge=abcdef";
        let lines = wrap_ascii_url(url, 7);
        assert_eq!(lines.concat(), url);
        assert!(lines.iter().all(|line| line.len() <= 7));
    }

    #[test]
    fn zero_width_url_wrap_renders_no_url_characters() {
        assert!(wrap_ascii_url("https://example.test", 0).is_empty());
    }

    #[test]
    fn short_width_wrap_has_one_column_lines() {
        let lines = wrap_ascii_url("abc", 1);
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn url_display_lines_indent_and_keep_margin_from_borders() {
        let url = "https://example.test/oauth-callback?code_challenge=abcdef";
        let lines = url_display_lines(url, 20);
        assert!(!lines.is_empty());
        for line in &lines {
            // 每行 2 空格缩进 + 折行内容，总长不超过内容宽度
            assert!(line.spans[0].content.starts_with("  "));
            assert!(line.width() <= 20);
        }
        // 拼接（去缩进）仍是完整 URL
        let stitched: String = lines
            .iter()
            .map(|line| line.spans[0].content.trim_start().to_string())
            .collect();
        assert_eq!(stitched, url);
    }

    #[test]
    fn url_display_lines_render_nothing_when_width_fits_only_the_margin() {
        assert!(url_display_lines("https://example.test", URL_DISPLAY_MARGIN).is_empty());
        assert!(url_display_lines("https://example.test", 0).is_empty());
    }

    #[test]
    fn hscroll_input_line_keeps_full_content_when_it_fits() {
        let line = hscroll_input_line("> ", "abc", 1, 20, InputLineStyle::plain());
        // 前缀 2 + 内容 3 + 竖条 1
        assert_eq!(line.width(), 6);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "> a▌bc");
    }

    #[test]
    fn hscroll_input_line_anchors_window_around_cursor_when_overflow() {
        let display = "abcdefghijklmnopqrstuvwxyz";
        // 光标在正中：两侧都应出现省略号，窗口总宽不超过可用宽度
        let line = hscroll_input_line("", display, 13, 10, InputLineStyle::plain());
        assert!(line.width() <= 10);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with('…'));
        assert!(text.ends_with('…'));
        assert!(text.contains('n')); // 光标字符可见
        // 拼接去掉省略号和竖条光标后必须是原串的连续子串（字符不丢不重）
        let core: String = text.chars().filter(|c| *c != '…' && *c != '▌').collect();
        assert!(display.contains(&core));
    }

    #[test]
    fn hscroll_input_line_right_anchors_when_cursor_at_end() {
        let display = "abcdefghijklmnopqrstuvwxyz";
        let line = hscroll_input_line("", display, display.len(), 10, InputLineStyle::plain());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with('…'));
        assert!(!text.ends_with('…'));
        // 竖条光标绘制在末尾：行尾是 "z▌"，尾部内容 z 可见
        assert!(text.ends_with("z▌"));
    }

    #[test]
    fn hscroll_input_line_handles_multibyte_characters() {
        let display = "很长的中文输入内容需要被截断处理";
        let line = hscroll_input_line("> ", display, display.len(), 12, InputLineStyle::plain());
        assert!(line.width() <= 12);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('…'));
    }

    #[test]
    fn hscroll_input_line_bar_cursor_keeps_char_after_cursor() {
        // 竖条插入光标位置，光标处字符保留在竖条右侧
        let line = hscroll_input_line(
            "> ",
            "abcde",
            2,
            20,
            InputLineStyle {
                content: Style::new().fg(Color::Yellow),
                cursor_style: Style::new().fg(Color::Yellow),
            },
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "> ab▌cde");
    }
}

#[cfg(test)]
mod copy_model_confirm_view_tests {
    use super::render_copy_model_confirm;
    use crate::tui::model::{CopyModelConfirmState, NameValidation};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn warning_describes_complete_copy_and_preserved_source() {
        let state = CopyModelConfirmState {
            source_model_id: "source-model".to_string(),
            default_new_id: "copied-model".to_string(),
            new_id: "copied-model".to_string(),
            cursor_pos: 0,
            editing: false,
            validation: NameValidation::Available,
            error: None,
            product_name: "test-product".to_string(),
        };
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| render_copy_model_confirm(frame, frame.area(), &state))
            .expect("draw");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .filter(|symbol| *symbol != " ")
            .collect::<String>();

        assert!(rendered.contains("将完整复制模型参数及provider绑定；源模型保持不变"));
        assert!(!rendered.contains("需手动添加provider绑定"));
    }
}

#[cfg(test)]
mod model_management_view_tests {
    use super::{
        render_delete_confirm, render_model_detail, render_model_management,
        render_model_selection, render_probe_confirm, render_product_selection,
        render_provider_management,
    };
    use crate::tui::model::{
        AppModel, DeleteConfirmState, ModelDetailKind, ModelDetailState, ModelListItem,
        ModelManagementState, ModelSelectionSource, ModelSelectionState, ProductSelectionState,
        ProviderListItem, ProviderManagementState, ProviderStatus,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered(draw: impl FnOnce(&mut ratatui::Frame)) -> String {
        let backend = TestBackend::new(120, 16);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(draw).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn product_selection_renders_error_in_status_bar() {
        let state = ProductSelectionState {
            items: vec![],
            cursor: 0,
            filter: "openrotuer".to_string(),
            filter_active: true,
            error: Some("no product matches \"openrotuer\"".to_string()),
        };
        let app = AppModel::new(
            std::path::PathBuf::from("config.toml"),
            crate::tui::model::EntryMode::Connect,
        );
        let rendered =
            rendered(|frame| render_product_selection(frame, frame.area(), &state, &app));
        assert!(rendered.contains("no product matches \"openrotuer\""));
    }

    #[test]
    fn management_list_shows_model_columns() {
        let state = ModelManagementState {
            models: vec![ModelListItem {
                id: "deepseek-v4-pro-lp".into(),
                context_window: 100000,
                max_output_tokens: 4096,
                thinking: "off".into(),
                features: "tools".into(),
                provider_count: 1,
                route_count: 0,
            }],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            error: None,
            scroll_offset: 0,
        };
        let rendered = rendered(|frame| render_model_management(frame, frame.area(), &state));
        assert!(rendered.contains("deepseek-v4-pro-lp"));
        assert!(rendered.contains("tools"));
        assert!(rendered.contains("1 providers"));
    }

    fn make_models(count: usize) -> Vec<ModelListItem> {
        (0..count)
            .map(|i| ModelListItem {
                id: format!("model-{i:02}"),
                context_window: 100000,
                max_output_tokens: 4096,
                thinking: "off".into(),
                features: "tools".into(),
                provider_count: 1,
                route_count: 0,
            })
            .collect()
    }

    fn make_providers(count: usize) -> Vec<ProviderListItem> {
        (0..count)
            .map(|i| ProviderListItem {
                name: format!("provider-{i:02}"),
                product: "DeepSeek API".into(),
                auth_type: "ApiKey".into(),
                status: ProviderStatus::Ok,
                protocols: vec!["chat".to_string()],
                cooldown: None,
            })
            .collect()
    }

    #[test]
    fn delete_confirm_truncates_referencing_models_with_indicator() {
        // 引用模型多于可视区时必须截断并给出"还有 N 个未显示"指示，
        // 避免用户在看不到完整受影响清单的情况下做强制删除决策
        let state = DeleteConfirmState {
            provider_name: "shared-upstream".to_string(),
            referencing_models: (0..30).map(|i| format!("model-{i:02}")).collect(),
            force_mode: false,
            error: None,
        };
        let rendered = rendered(|frame| render_delete_confirm(frame, frame.area(), &state));
        assert!(rendered.contains("model-00"));
        // TestBackend 单元格流中宽字符（CJK）带空白填充格，压缩空格后再断言中文
        let squashed: String = rendered.chars().filter(|c| *c != ' ').collect();
        assert!(squashed.contains("还有23个模型未显示"));
        assert!(!rendered.contains("model-29"));
    }

    #[test]
    fn model_selection_renders_error_in_status_bar() {
        let state = ModelSelectionState {
            items: vec![],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            selected: Default::default(),
            configured: Default::default(),
            error: Some("至少选择一个模型".to_string()),
            product_name: "test".to_string(),
            source: ModelSelectionSource::Connect,
            is_openrouter: false,
            is_degraded: false,
            openrouter_cached_at: None,
        };
        let rendered = rendered(|frame| render_model_selection(frame, frame.area(), &state));
        // TestBackend 单元格流中宽字符（CJK）带空白填充格，压缩空格后再断言中文
        let squashed: String = rendered.chars().filter(|c| *c != ' ').collect();
        assert!(squashed.contains("至少选择一个模型"));
    }

    #[test]
    fn provider_scrolled_window_keeps_cursor_visible_and_hides_rows_outside() {
        // 测试渲染器高 16：内边框-2、表头-1，列表区 13 行；窗口从 offset=6 开始，
        // 应展示 provider-06..provider-18，offset 之前与窗口之后的行不可见。
        let state = ProviderManagementState {
            providers: make_providers(20),
            cursor: 12,
            filter: String::new(),
            filter_active: false,
            error: None,
            scroll_offset: 6,
            quota_fetched_at: None,
        };
        let rendered = rendered(|frame| render_provider_management(frame, frame.area(), &state));
        assert!(!rendered.contains("provider-05"));
        assert!(rendered.contains("provider-06"));
        assert!(rendered.contains("provider-12"));
        assert!(rendered.contains("provider-18"));
        assert!(!rendered.contains("provider-19"));
    }

    #[test]
    fn scrolled_window_keeps_cursor_visible_and_hides_rows_outside() {
        // 测试渲染器高 16：内边框-2、表头-1，列表区 13 行；窗口从 offset=6 开始，
        // 应展示 model-06..model-18，offset 之前与窗口之后的行不可见。
        let state = ModelManagementState {
            models: make_models(20),
            cursor: 12,
            filter: String::new(),
            filter_active: false,
            error: None,
            scroll_offset: 6,
        };
        let rendered = rendered(|frame| render_model_management(frame, frame.area(), &state));
        assert!(!rendered.contains("model-05"));
        assert!(rendered.contains("model-06"));
        assert!(rendered.contains("model-12"));
        assert!(rendered.contains("model-18"));
        assert!(!rendered.contains("model-19"));
    }

    #[test]
    fn error_message_stays_visible_below_the_scrolled_list() {
        let state = ModelManagementState {
            models: make_models(20),
            cursor: 12,
            filter: String::new(),
            filter_active: false,
            error: Some("some failure".to_string()),
            scroll_offset: 6,
        };
        let rendered = rendered(|frame| render_model_management(frame, frame.area(), &state));
        assert!(rendered.contains("some failure"));
        // error 行固定占底部一行，列表窗口缩为 12 行，model-18 被挤出可视区。
        assert!(rendered.contains("model-17"));
        assert!(!rendered.contains("model-18"));
    }

    #[test]
    fn detail_lists_protocol_binding_counts() {
        let state = ModelDetailState {
            kind: ModelDetailKind::Existing {
                original_id: "m".into(),
            },
            model_id: "m".into(),
            context_window: "1000".into(),
            max_output_tokens: "100".into(),
            thinking_enabled: false,
            default_reasoning_level: String::new(),
            supported_reasoning_levels: String::new(),
            features: String::new(),
            chat_bindings: vec![crate::tui::model::ModelBindingPreview {
                name: "deepseek".into(),
                upstream: "src".into(),
                cooldown: None,
            }],
            responses_bindings: Vec::new(),
            anthropic_bindings: Vec::new(),
            routes: Vec::new(),
            is_routing_model: false,
            cursor: 0,
            editing: false,
            edit_buffer: String::new(),
            edit_cursor: 0,
            error: None,
            scroll_offset: 0,
        };
        let rendered = rendered(|frame| render_model_detail(frame, frame.area(), &state));
        assert!(rendered.contains("openai_chat_providers"));
        assert!(rendered.contains("1 bindings"));
        assert!(rendered.contains("deepseek"));
    }

    #[test]
    fn detail_renders_binding_cooldown_badges() {
        let state = ModelDetailState {
            kind: ModelDetailKind::Existing {
                original_id: "m".into(),
            },
            model_id: "m".into(),
            context_window: "1000".into(),
            max_output_tokens: "100".into(),
            thinking_enabled: false,
            default_reasoning_level: String::new(),
            supported_reasoning_levels: String::new(),
            features: String::new(),
            chat_bindings: vec![
                crate::tui::model::ModelBindingPreview {
                    name: "prov-cooling".into(),
                    upstream: "src".into(),
                    cooldown: Some("2026-10-01 12:00".into()),
                },
                crate::tui::model::ModelBindingPreview {
                    name: "prov-ok".into(),
                    upstream: "src2".into(),
                    cooldown: None,
                },
            ],
            responses_bindings: Vec::new(),
            anthropic_bindings: Vec::new(),
            routes: Vec::new(),
            is_routing_model: false,
            cursor: 0,
            editing: false,
            edit_buffer: String::new(),
            edit_cursor: 0,
            error: None,
            scroll_offset: 0,
        };
        let rendered = rendered(|frame| render_model_detail(frame, frame.area(), &state));
        assert!(rendered.contains("[COOLDOWN until 2026-10-01 12:00]"));
        assert!(rendered.contains("[OK]"));
    }

    #[test]
    fn provider_management_renders_cooldown_status() {
        let state = ProviderManagementState {
            providers: vec![
                ProviderListItem {
                    name: "cooling-prov".into(),
                    product: "Custom".into(),
                    auth_type: "ApiKey".into(),
                    status: ProviderStatus::Ok,
                    protocols: vec!["chat".into()],
                    cooldown: Some("2026-10-01 12:00".into()),
                },
                ProviderListItem {
                    name: "ok-prov".into(),
                    product: "Custom".into(),
                    auth_type: "ApiKey".into(),
                    status: ProviderStatus::Ok,
                    protocols: vec!["chat".into()],
                    cooldown: None,
                },
            ],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            error: None,
            scroll_offset: 0,
            quota_fetched_at: None,
        };
        let rendered = rendered(|frame| render_provider_management(frame, frame.area(), &state));
        assert!(rendered.contains("[COOLDOWN until 2026-10-01 12:00]"));
        assert!(rendered.contains("[OK]"));
    }

    #[test]
    fn detail_renders_routing_model_derived_fields_and_routes() {
        let state = ModelDetailState {
            kind: ModelDetailKind::Existing {
                original_id: "router-lp".into(),
            },
            model_id: "router-lp".into(),
            context_window: "1000".into(),
            max_output_tokens: "100".into(),
            thinking_enabled: true,
            default_reasoning_level: "low".into(),
            supported_reasoning_levels: "low,high".into(),
            features: "image_input,tools".into(),
            chat_bindings: Vec::new(),
            responses_bindings: Vec::new(),
            anthropic_bindings: Vec::new(),
            routes: vec![
                crate::tui::model::ModelRoutePreview {
                    level: "low".into(),
                    target: "target-low".into(),
                },
                crate::tui::model::ModelRoutePreview {
                    level: "high".into(),
                    target: "target-high".into(),
                },
            ],
            is_routing_model: true,
            cursor: 0,
            editing: false,
            edit_buffer: String::new(),
            edit_cursor: 0,
            error: None,
            scroll_offset: 0,
        };
        let rendered = rendered(|frame| render_model_detail(frame, frame.area(), &state));
        assert!(rendered.contains("Routing Model - Read Only"));
        assert!(rendered.contains("1000 (derived)"));
        assert!(rendered.contains("100 (derived)"));
        assert!(rendered.contains("levels: low,high (default: low)"));
        assert!(rendered.contains("image_input,tools (derived)"));
        assert!(rendered.contains("reasoning_routes"));
        assert!(rendered.contains("2 routes"));
        assert!(rendered.contains("target-low"));
        assert!(rendered.contains("target-high"));
        assert!(!rendered.contains("openai_chat_providers"));
    }

    /// SPEC：模型列表须展示思考标识 `[thinking]`（能力来自 OpenRouter 缓存
    /// `supported_reasoning` / `reasoning_levels`，非虚构）。
    #[test]
    fn openrouter_model_selection_renders_degraded_warning_and_key_hints() {
        let state = ModelSelectionState {
            items: vec![crate::tui::model::ModelItem {
                id: "anthropic/claude-sonnet-4".to_string(),
                display_name: "Claude Sonnet 4".to_string(),
                context_window: Some(200_000),
                max_output_tokens: Some(64_000),
                features: vec!["image_input".to_string(), "tools".to_string()],
                reasoning_levels: vec!["low".to_string(), "high".to_string()],
                default_reasoning_level: Some("low".to_string()),
            }],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            selected: std::collections::HashSet::new(),
            configured: std::collections::HashSet::new(),
            error: None,
            product_name: "OpenRouter".to_string(),
            source: ModelSelectionSource::Connect,
            is_openrouter: true,
            is_degraded: true,
            openrouter_cached_at: Some(1_700_000_000),
        };
        let rendered = rendered(|frame| render_model_selection(frame, frame.area(), &state));
        let squashed: String = rendered.chars().filter(|c| *c != ' ').collect();
        assert!(squashed.contains("离线降级缓存"));
        assert!(squashed.contains("网络不可达"));
        assert!(squashed.contains("[r]重试"));
        assert!(rendered.contains("Claude Sonnet 4"));
        assert!(rendered.contains("ctx=200K"));
        assert!(rendered.contains("image=✓"));
        // SPEC §模型列表：思考标识
        assert!(rendered.contains("[thinking]"));
    }

    /// 探测确认屏默认高亮「探测」，并给出探测成本估算。
    #[test]
    fn probe_confirm_defaults_to_probing_with_cost_estimate() {
        let state = crate::tui::model::ProbeConfirmState {
            product_name: "OpenRouter".to_string(),
            models: vec![
                "anthropic/claude-sonnet-4".to_string(),
                "openai/gpt-5.5".to_string(),
            ],
            cursor: 0,
        };
        let rendered = rendered(|frame| render_probe_confirm(frame, frame.area(), &state));
        assert!(state.probe_selected(), "cursor 0 必须选中探测");
        // 双宽字符在渲染串里带填充空格，先压掉空白再断言
        let squashed: String = rendered.chars().filter(|c| *c != ' ').collect();
        assert!(
            squashed.contains("▶探测（推荐）"),
            "默认高亮必须落在「探测」上，实际: {squashed}"
        );
        assert!(!squashed.contains("▶跳过探测"), "默认不得高亮跳过");
        // 成本估算：2 个模型 → 约 2~4 次请求
        assert!(squashed.contains("2个模型"), "须给出探测成本估算");
        assert!(squashed.contains("2~4次请求"), "须给出请求次数估算");
        assert!(rendered.contains("OpenRouter"));
    }

    /// 目录未声明思考能力的模型不得显示思考标识（宁缺毋假）。
    #[test]
    fn openrouter_model_without_reasoning_levels_has_no_thinking_badge() {
        let state = ModelSelectionState {
            items: vec![crate::tui::model::ModelItem {
                id: "openai/gpt-oss".to_string(),
                display_name: "GPT-OSS".to_string(),
                context_window: None,
                max_output_tokens: None,
                features: vec![],
                reasoning_levels: vec![],
                default_reasoning_level: None,
            }],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            selected: std::collections::HashSet::new(),
            configured: std::collections::HashSet::new(),
            error: None,
            product_name: "OpenRouter".to_string(),
            source: ModelSelectionSource::Connect,
            is_openrouter: true,
            is_degraded: false,
            openrouter_cached_at: Some(1_700_000_000),
        };
        let rendered = rendered(|frame| render_model_selection(frame, frame.area(), &state));
        assert!(!rendered.contains("[thinking]"), "无声明不得显示思考标识");
        assert!(rendered.contains("ctx=?"), "未声明窗口渲染 ? 不虚构默认值");
    }
}

#[cfg(test)]
mod provider_detail_view_tests {
    use super::render_provider_detail;
    use crate::tui::model::ProviderDetailState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered_tall(draw: impl FnOnce(&mut ratatui::Frame), height: u16) -> String {
        let backend = TestBackend::new(120, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(draw).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn no_quota_groups_renders_compact_auth() {
        let state = ProviderDetailState {
            name: "test-provider".into(),
            product: "Test Product".into(),
            auth_description: "API Key".into(),
            auth_status: "已配置".into(),
            endpoints: vec![],
            bound_models: vec![],
            compat_info: vec![],
            model_cursor: 0,
            scroll_offset: 0,
            quota_fetched_at: None,
        };
        // Should not panic with a small area; auth_height = 3
        let rendered = rendered_tall(
            |frame| render_provider_detail(frame, frame.area(), &state),
            20,
        );
        assert!(rendered.contains("Test Product"));
        assert!(!rendered.contains("Quota"));
    }
}
