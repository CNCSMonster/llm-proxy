use crossterm::event::{KeyCode, KeyEvent};

use super::model::{self, AppModel, Screen};
use crate::config::{Config, Protocol, ProviderBinding};
use crate::tui::model::{
    BindingRow, ModelDeleteConfirmState, ModelDetailField, ModelDetailKind, ModelDetailState,
    ModelManagementState, ModelRenameState, ProtocolBindingEditorState,
};

/// 按键移动 / 返回列表 / 终端缩放后，按当前终端高度重新对齐滚动窗口。
fn sync_model_list_scroll(state: &mut ModelManagementState, app_height: u16) {
    let viewport = model::model_list_viewport(app_height).saturating_sub(state.error_row_count());
    state.ensure_cursor_visible(viewport);
}

pub(crate) fn return_to_model_management_preserving_cursor(
    app: &mut AppModel,
    keep_model: Option<&str>,
) {
    let models = model::load_configured_models(&app.config_path);
    let cursor = keep_model
        .and_then(|id| models.iter().position(|item| item.id == id))
        .unwrap_or(0);
    let mut state = ModelManagementState {
        models,
        cursor,
        filter: String::new(),
        filter_active: false,
        error: None,
        scroll_offset: 0,
    };
    sync_model_list_scroll(&mut state, app.height);
    app.screen = Screen::ModelManagement(state);
}

pub fn handle_model_management_keys(app: &mut AppModel, key: KeyEvent) {
    if let Screen::ModelManagement(ref mut state) = app.screen
        && state.filter_active
    {
        match key.code {
            KeyCode::Esc => state.deactivate_filter(),
            KeyCode::Enter => state.filter_active = false,
            KeyCode::Backspace => {
                state.filter.pop();
                if state.filter.is_empty() {
                    state.filter_active = false;
                }
            }
            KeyCode::Char(c) => {
                state.filter.push(c);
                state.cursor = 0;
                state.scroll_offset = 0;
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Char('q') => app.screen = Screen::Quit,
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::ModelManagement(ref mut state) = app.screen {
                state.move_up();
                sync_model_list_scroll(state, app.height);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ModelManagement(ref mut state) = app.screen {
                state.move_down();
                sync_model_list_scroll(state, app.height);
            }
        }
        KeyCode::Char('/') => {
            if let Screen::ModelManagement(ref mut state) = app.screen {
                state.activate_filter();
            }
        }
        KeyCode::Enter | KeyCode::Char('l') => {
            if let Screen::ModelManagement(ref state) = app.screen
                && let Some(item) = state.selected_model()
            {
                let id = item.id.clone();
                app.screen = Screen::ModelDetail(model::build_model_detail(&app.config_path, &id));
            }
        }
        KeyCode::Char('a') | KeyCode::Char('+') => {
            app.screen = Screen::ModelDetail(model::build_add_model_detail());
        }
        KeyCode::Char('c') => {
            if let Screen::ModelManagement(ref state) = app.screen
                && let Some(item) = state.selected_model()
            {
                let source = item.id.clone();
                if item.route_count > 0 {
                    if let Screen::ModelManagement(ref mut state) = app.screen {
                        state.error =
                            Some("路由模型暂不支持在 TUI 中复制，请在配置文件中配置".to_string());
                    }
                    return;
                }
                match model::build_copy_model_detail(&app.config_path, &source) {
                    Some(detail) => app.screen = Screen::ModelDetail(detail),
                    None => {
                        if let Screen::ModelManagement(ref mut state) = app.screen {
                            state.error = Some(format!("无法复制 {source}"));
                        }
                    }
                }
            }
        }
        KeyCode::Char('d') | KeyCode::Char('-') => {
            if let Screen::ModelManagement(ref state) = app.screen
                && let Some(item) = state.selected_model()
            {
                app.screen = Screen::ModelDeleteConfirm(ModelDeleteConfirmState {
                    model_id: item.id.clone(),
                    error: None,
                });
            }
        }
        KeyCode::Char('r') => {
            let id = if let Screen::ModelManagement(ref state) = app.screen {
                state.selected_model().map(|item| item.id.clone())
            } else {
                None
            };
            if let Some(id) = id {
                enter_model_rename(app, &id);
            }
        }
        _ => {}
    }
}

pub fn handle_model_detail_keys(app: &mut AppModel, key: KeyEvent) {
    let editing = matches!(&app.screen, Screen::ModelDetail(s) if s.editing);
    if editing {
        handle_detail_editing(app, key);
        return;
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::ModelDetail(ref mut state) = app.screen {
                state.move_up();
                sync_model_detail_scroll(state, app.height);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ModelDetail(ref mut state) = app.screen {
                state.move_down();
                sync_model_detail_scroll(state, app.height);
            }
        }
        KeyCode::Char('s') => save_model_detail(app),
        KeyCode::Char('r') => {
            if let Screen::ModelDetail(ref state) = app.screen
                && matches!(state.kind, ModelDetailKind::Existing { .. })
            {
                let id = state.model_id.clone();
                enter_model_rename(app, &id);
            }
        }
        KeyCode::Enter => activate_detail_field(app),
        _ => {}
    }
}

pub fn handle_model_delete_confirm_keys(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => confirm_delete_model(app),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            let id = if let Screen::ModelDeleteConfirm(ref state) = app.screen {
                Some(state.model_id.clone())
            } else {
                None
            };
            return_to_model_management_preserving_cursor(app, id.as_deref());
        }
        _ => {}
    }
}

/// 按键移动 / 返回编辑器 / 终端缩放后，按当前终端高度重对齐内容滚动窗口。
fn sync_model_detail_scroll(state: &mut ModelDetailState, app_height: u16) {
    let viewport = model::detail_viewport(app_height).saturating_sub(state.error_row_count());
    let cursor_line = state.cursor_line_index();
    model::follow_scroll(&mut state.scroll_offset, cursor_line, viewport);
}

fn sync_protocol_binding_scroll(state: &mut ProtocolBindingEditorState, app_height: u16) {
    let viewport = model::detail_viewport(app_height).saturating_sub(state.error_row_count());
    let cursor_line = state.active_cursor_line();
    model::follow_scroll(&mut state.scroll_offset, cursor_line, viewport);
}

pub fn handle_protocol_binding_editor_keys(app: &mut AppModel, key: KeyEvent) {
    handle_protocol_binding_editor_keys_inner(app, key);
    if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
        sync_protocol_binding_scroll(state, app.height);
    }
}

fn handle_protocol_binding_editor_keys_inner(app: &mut AppModel, key: KeyEvent) {
    let (adding, editing_upstream, remove_confirm) =
        if let Screen::ProtocolBindingEditor(ref state) = app.screen {
            (state.adding, state.editing_upstream, state.remove_confirm)
        } else {
            return;
        };

    if remove_confirm {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Screen::ProtocolBindingEditor(ref mut state) = app.screen
                    && state.cursor < state.rows.len()
                {
                    state.rows.remove(state.cursor);
                    if state.cursor > 0 && state.cursor >= state.rows.len() {
                        state.cursor -= 1;
                    }
                    state.remove_confirm = false;
                    state.error = None;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
                    state.remove_confirm = false;
                    state.error = None;
                }
            }
            _ => {}
        }
        return;
    }

    if editing_upstream {
        handle_upstream_editing(app, key);
        return;
    }

    if adding {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
                    state.candidate_cursor = state.candidate_cursor.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Screen::ProtocolBindingEditor(ref mut state) = app.screen
                    && state.candidate_cursor + 1 < state.candidates.len()
                {
                    state.candidate_cursor += 1;
                }
            }
            KeyCode::Enter => confirm_add_binding(app),
            KeyCode::Esc => {
                if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
                    state.adding = false;
                    state.error = None;
                }
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
                state.cursor = state.cursor.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ProtocolBindingEditor(ref mut state) = app.screen
                && state.cursor + 1 < state.rows.len()
            {
                state.cursor += 1;
            }
        }
        KeyCode::Char('a') => start_add_binding(app),
        KeyCode::Char('e') => start_edit_upstream(app),
        KeyCode::Char('K') => move_binding(app, -1),
        KeyCode::Char('J') => move_binding(app, 1),
        KeyCode::Char('d') | KeyCode::Char('-') => {
            if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
                if state.rows.is_empty() {
                    state.error = Some("没有可删除的绑定".to_string());
                } else {
                    state.remove_confirm = true;
                    state.error = Some("确认删除当前绑定？默认取消".to_string());
                }
            }
        }
        KeyCode::Char('s') => save_binding_editor(app),
        _ => {}
    }
}

fn enter_model_rename(app: &mut AppModel, model_id: &str) {
    app.screen = Screen::ModelRename(ModelRenameState {
        old_model_id: model_id.to_string(),
        new_model_id: model_id.to_string(),
        editing: false,
        cursor_pos: 0,
        confirm_step: 0,
        error: None,
        provider_name: String::new(),
        return_to_model_list: true,
    });
}

fn activate_detail_field(app: &mut AppModel) {
    let field = if let Screen::ModelDetail(ref state) = app.screen {
        state.selected_field()
    } else {
        return;
    };

    if let Screen::ModelDetail(ref state) = app.screen
        && state.is_routing_model
    {
        if field == ModelDetailField::Id && matches!(state.kind, ModelDetailKind::Existing { .. }) {
            enter_model_rename(app, &state.model_id.clone());
        } else if let Screen::ModelDetail(ref mut s) = app.screen {
            s.error = Some("路由模型的能力派生自目标模型，不可直接编辑".to_string());
        }
        return;
    }

    if field.is_binding() {
        let (kind, model_id, protocol) = if let Screen::ModelDetail(ref state) = app.screen {
            (
                state.kind.clone(),
                state.model_id.clone(),
                field.protocol().expect("binding field"),
            )
        } else {
            return;
        };
        if !matches!(kind, ModelDetailKind::Existing { .. }) {
            if let Screen::ModelDetail(ref mut state) = app.screen {
                state.error = Some("请先保存模型，再编辑协议绑定".to_string());
            }
            return;
        }
        match build_binding_editor(&app.config_path, &model_id, protocol) {
            Ok(editor) => app.screen = Screen::ProtocolBindingEditor(editor),
            Err(err) => {
                if let Screen::ModelDetail(ref mut state) = app.screen {
                    state.error = Some(err);
                }
            }
        }
        return;
    }

    let (existing_id, toggle_thinking, start_editing) = if let Screen::ModelDetail(ref state) =
        app.screen
    {
        if field == ModelDetailField::Id && matches!(state.kind, ModelDetailKind::Existing { .. }) {
            (Some(state.model_id.clone()), false, false)
        } else if field == ModelDetailField::Thinking {
            (None, true, false)
        } else {
            (None, false, true)
        }
    } else {
        return;
    };

    if let Some(id) = existing_id {
        enter_model_rename(app, &id);
        return;
    }

    if toggle_thinking {
        if let Screen::ModelDetail(ref mut state) = app.screen {
            state.thinking_enabled = !state.thinking_enabled;
        }
        if let Screen::ModelDetail(ref state) = app.screen
            && !state.is_draft()
        {
            apply_existing_scalar(app);
        }
        return;
    }

    if start_editing && let Screen::ModelDetail(ref mut state) = app.screen {
        state.editing = true;
        state.edit_buffer = current_field_value(state, field);
        state.edit_cursor = state.edit_buffer.len();
        state.error = None;
    }
}

fn current_field_value(state: &ModelDetailState, field: ModelDetailField) -> String {
    match field {
        ModelDetailField::Id => state.model_id.clone(),
        ModelDetailField::ContextWindow => state.context_window.clone(),
        ModelDetailField::MaxOutput => state.max_output_tokens.clone(),
        ModelDetailField::Features => state.features.clone(),
        ModelDetailField::Thinking
        | ModelDetailField::ChatBindings
        | ModelDetailField::ResponsesBindings
        | ModelDetailField::AnthropicBindings
        | ModelDetailField::Routes => String::new(),
    }
}

fn handle_detail_editing(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => commit_detail_edit(app),
        KeyCode::Esc => {
            if let Screen::ModelDetail(ref mut state) = app.screen {
                state.editing = false;
                state.edit_buffer.clear();
                state.error = None;
            }
        }
        KeyCode::Char(ch)
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ',' | ' ') =>
        {
            if let Screen::ModelDetail(ref mut state) = app.screen {
                if state.edit_cursor >= state.edit_buffer.len() {
                    state.edit_buffer.push(ch);
                } else {
                    state.edit_buffer.insert(state.edit_cursor, ch);
                }
                state.edit_cursor += 1;
                state.error = None;
            }
        }
        KeyCode::Backspace => {
            if let Screen::ModelDetail(ref mut state) = app.screen
                && state.edit_cursor > 0
            {
                state.edit_cursor -= 1;
                state.edit_buffer.remove(state.edit_cursor);
            }
        }
        KeyCode::Left => {
            if let Screen::ModelDetail(ref mut state) = app.screen {
                state.edit_cursor = state.edit_cursor.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if let Screen::ModelDetail(ref mut state) = app.screen
                && state.edit_cursor < state.edit_buffer.len()
            {
                state.edit_cursor += 1;
            }
        }
        _ => {}
    }
}

fn commit_detail_edit(app: &mut AppModel) {
    let field = if let Screen::ModelDetail(ref state) = app.screen {
        state.selected_field()
    } else {
        return;
    };
    let value = if let Screen::ModelDetail(ref state) = app.screen {
        state.edit_buffer.trim().to_string()
    } else {
        return;
    };

    if let Screen::ModelDetail(ref mut state) = app.screen {
        match field {
            ModelDetailField::Id => {
                if value.is_empty() {
                    state.error = Some("模型 ID 不能为空".to_string());
                    return;
                }
                state.model_id = value;
            }
            ModelDetailField::ContextWindow => {
                if value.parse::<i64>().ok().filter(|n| *n > 0).is_none() {
                    state.error = Some("context_window 必须是正整数".to_string());
                    return;
                }
                state.context_window = value;
            }
            ModelDetailField::MaxOutput => {
                if value.parse::<i64>().ok().filter(|n| *n > 0).is_none() {
                    state.error = Some("max_output_tokens 必须是正整数".to_string());
                    return;
                }
                state.max_output_tokens = value;
            }
            ModelDetailField::Features => state.features = normalize_csv(&value),
            _ => {}
        }
        state.editing = false;
        state.edit_buffer.clear();
        state.error = None;
    }

    if let Screen::ModelDetail(ref state) = app.screen
        && !state.is_draft()
        && field != ModelDetailField::Id
    {
        apply_existing_scalar(app);
    }
}

fn normalize_csv(value: &str) -> String {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

fn csv_parts(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn save_model_detail(app: &mut AppModel) {
    let state = if let Screen::ModelDetail(ref state) = app.screen {
        state.clone()
    } else {
        return;
    };
    if state.is_routing_model {
        if let Screen::ModelDetail(ref mut current) = app.screen {
            current.error = Some("路由模型为只读展示，不可在此保存修改".to_string());
        }
        return;
    }
    if !state.is_draft() {
        if let Screen::ModelDetail(ref mut current) = app.screen {
            current.error = Some("已保存的模型会即时写入标量字段".to_string());
        }
        return;
    }
    if state.model_id.trim().is_empty() {
        if let Screen::ModelDetail(ref mut current) = app.screen {
            current.error = Some("模型 ID 不能为空".to_string());
            current.cursor = 0;
            current.editing = true;
        }
        return;
    }
    let context_window = match state.context_window.parse::<i64>() {
        Ok(value) if value > 0 => value,
        _ => {
            if let Screen::ModelDetail(ref mut current) = app.screen {
                current.error = Some("context_window 必须是正整数".to_string());
            }
            return;
        }
    };
    let max_output = match state.max_output_tokens.parse::<i64>() {
        Ok(value) if value > 0 => value,
        _ => {
            if let Screen::ModelDetail(ref mut current) = app.screen {
                current.error = Some("max_output_tokens 必须是正整数".to_string());
            }
            return;
        }
    };

    let config_path = app.config_path.clone();
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let runtime_dir = crate::service::StateDirs::resolve(config_dir).runtime_dir;
    let result = crate::tui::run_async_sync(async {
        match &state.kind {
            ModelDetailKind::Add => {
                apply_model_add(
                    &config_path,
                    &runtime_dir,
                    &state.model_id,
                    Some(context_window),
                    Some(max_output),
                    None,
                )
                .await?;
            }
            ModelDetailKind::Copy { source_id } => {
                apply_model_add(
                    &config_path,
                    &runtime_dir,
                    &state.model_id,
                    None,
                    None,
                    Some(source_id.as_str()),
                )
                .await?;
            }
            ModelDetailKind::Existing { .. } => {}
        }
        apply_model_set(&config_path, &runtime_dir, &state).await
    });

    match result {
        Ok(()) => {
            return_to_model_management_preserving_cursor(app, Some(&state.model_id));
            if let Screen::ModelManagement(ref mut list) = app.screen {
                list.error = Some(format!("✓ 已保存 {}", state.model_id));
            }
        }
        Err(err) => {
            if let Screen::ModelDetail(ref mut current) = app.screen {
                current.error = Some(format!("保存失败: {err}"));
            }
        }
    }
}

fn apply_existing_scalar(app: &mut AppModel) {
    let state = if let Screen::ModelDetail(ref state) = app.screen {
        state.clone()
    } else {
        return;
    };
    if state.is_routing_model {
        if let Screen::ModelDetail(ref mut current) = app.screen {
            current.error = Some("路由模型的能力派生自目标模型，不可直接编辑".to_string());
        }
        return;
    }
    let config_path = app.config_path.clone();
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let runtime_dir = crate::service::StateDirs::resolve(config_dir).runtime_dir;
    let result = crate::tui::run_async_sync(async {
        apply_model_set(&config_path, &runtime_dir, &state).await
    });
    match result {
        Ok(()) => {
            if let Screen::ModelDetail(ref mut current) = app.screen {
                *current = model::build_model_detail(&app.config_path, &state.model_id);
                current.cursor = state.cursor;
                current.error = Some(format!("✓ 已更新 {}", state.model_id));
            }
        }
        Err(err) => {
            if let Screen::ModelDetail(ref mut current) = app.screen {
                current.error = Some(format!("更新失败: {err}"));
            }
        }
    }
}

fn confirm_delete_model(app: &mut AppModel) {
    let model_id = if let Screen::ModelDeleteConfirm(ref state) = app.screen {
        state.model_id.clone()
    } else {
        return;
    };
    let config_path = app.config_path.clone();
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let runtime_dir = crate::service::StateDirs::resolve(config_dir).runtime_dir;
    let result = crate::tui::run_async_sync(async {
        apply_model_remove(&config_path, &runtime_dir, &model_id).await
    });
    match result {
        Ok(()) => {
            return_to_model_management_preserving_cursor(app, None);
            if let Screen::ModelManagement(ref mut list) = app.screen {
                list.error = Some(format!("✓ 已删除 {model_id}"));
            }
        }
        Err(err) => {
            if let Screen::ModelDeleteConfirm(ref mut state) = app.screen {
                state.error = Some(format!("删除失败: {err}"));
            }
        }
    }
}

fn build_binding_editor(
    config_path: &std::path::Path,
    model_id: &str,
    protocol: Protocol,
) -> Result<ProtocolBindingEditorState, String> {
    let cfg = Config::load(config_path).map_err(|err| format!("加载配置失败: {err}"))?;
    let model = cfg
        .models
        .get(model_id)
        .ok_or_else(|| format!("unknown model {model_id:?}"))?;
    let rows = model
        .provider_bindings(protocol)
        .iter()
        .map(|binding| BindingRow {
            name: binding.name.clone(),
            upstream: binding.model.clone(),
            kind: endpoint_kind(&cfg, &binding.name, protocol),
        })
        .collect();
    Ok(ProtocolBindingEditorState {
        model_id: model_id.to_string(),
        protocol,
        rows,
        cursor: 0,
        adding: false,
        candidates: Vec::new(),
        candidate_cursor: 0,
        editing_upstream: false,
        upstream_input: String::new(),
        upstream_cursor: 0,
        remove_confirm: false,
        error: None,
        scroll_offset: 0,
    })
}

fn endpoint_kind(cfg: &Config, provider_id: &str, protocol: Protocol) -> String {
    cfg.providers
        .get(provider_id)
        .and_then(|provider| provider.endpoint(protocol))
        .map(|endpoint| {
            if endpoint.derive_from.is_some() {
                "derived"
            } else {
                "native"
            }
        })
        .unwrap_or("missing")
        .to_string()
}

fn start_add_binding(app: &mut AppModel) {
    let (model_id, protocol, used) = if let Screen::ProtocolBindingEditor(ref state) = app.screen {
        (
            state.model_id.clone(),
            state.protocol,
            state
                .rows
                .iter()
                .map(|row| row.name.clone())
                .collect::<Vec<_>>(),
        )
    } else {
        return;
    };
    let Ok(cfg) = Config::load(&app.config_path) else {
        if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
            state.error = Some("无法加载配置".to_string());
        }
        return;
    };
    let mut candidates: Vec<String> = cfg
        .providers
        .iter()
        .filter(|(id, provider)| {
            provider.endpoint(protocol).is_some() && !used.iter().any(|name| name == *id)
        })
        .map(|(id, _)| id.clone())
        .collect();
    candidates.sort();
    if candidates.is_empty() {
        if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
            state.error = Some(format!(
                "没有可添加的 {model_id} / {} provider",
                protocol.field_name()
            ));
        }
        return;
    }
    if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
        state.adding = true;
        state.candidates = candidates;
        state.candidate_cursor = 0;
        state.error = None;
    }
}

fn confirm_add_binding(app: &mut AppModel) {
    if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
        let Some(name) = state.candidates.get(state.candidate_cursor).cloned() else {
            return;
        };
        let default_upstream = state
            .rows
            .first()
            .map(|row| row.upstream.clone())
            .unwrap_or_else(|| state.model_id.clone());
        let kind = Config::load(&app.config_path)
            .ok()
            .map(|cfg| endpoint_kind(&cfg, &name, state.protocol))
            .unwrap_or_else(|| "native".to_string());
        state.rows.push(BindingRow {
            name,
            upstream: default_upstream,
            kind,
        });
        state.cursor = state.rows.len() - 1;
        state.adding = false;
        state.candidates.clear();
        state.error = None;
    }
}

fn start_edit_upstream(app: &mut AppModel) {
    if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
        let Some(row) = state.rows.get(state.cursor) else {
            state.error = Some("没有可编辑的绑定".to_string());
            return;
        };
        state.editing_upstream = true;
        state.upstream_input = row.upstream.clone();
        state.upstream_cursor = state.upstream_input.len();
        state.error = None;
    }
}

fn handle_upstream_editing(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
                let value = state.upstream_input.trim().to_string();
                if value.is_empty() {
                    state.error = Some("upstream model 不能为空".to_string());
                    return;
                }
                if let Some(row) = state.rows.get_mut(state.cursor) {
                    row.upstream = value;
                }
                state.editing_upstream = false;
                state.error = None;
            }
        }
        KeyCode::Esc => {
            if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
                state.editing_upstream = false;
                state.error = None;
            }
        }
        KeyCode::Char(ch) if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/') => {
            if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
                if state.upstream_cursor >= state.upstream_input.len() {
                    state.upstream_input.push(ch);
                } else {
                    state.upstream_input.insert(state.upstream_cursor, ch);
                }
                state.upstream_cursor += 1;
            }
        }
        KeyCode::Backspace => {
            if let Screen::ProtocolBindingEditor(ref mut state) = app.screen
                && state.upstream_cursor > 0
            {
                state.upstream_cursor -= 1;
                state.upstream_input.remove(state.upstream_cursor);
            }
        }
        KeyCode::Left => {
            if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
                state.upstream_cursor = state.upstream_cursor.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if let Screen::ProtocolBindingEditor(ref mut state) = app.screen
                && state.upstream_cursor < state.upstream_input.len()
            {
                state.upstream_cursor += 1;
            }
        }
        _ => {}
    }
}

fn move_binding(app: &mut AppModel, delta: isize) {
    if let Screen::ProtocolBindingEditor(ref mut state) = app.screen {
        if state.rows.len() < 2 {
            return;
        }
        let from = state.cursor;
        let to = from.saturating_add_signed(delta).min(state.rows.len() - 1);
        if from == to {
            return;
        }
        state.rows.swap(from, to);
        state.cursor = to;
    }
}

fn save_binding_editor(app: &mut AppModel) {
    let state = if let Screen::ProtocolBindingEditor(ref state) = app.screen {
        state.clone()
    } else {
        return;
    };
    let config_path = app.config_path.clone();
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let runtime_dir = crate::service::StateDirs::resolve(config_dir).runtime_dir;
    let result = crate::tui::run_async_sync(async {
        apply_binding_list(&config_path, &runtime_dir, &state).await
    });
    match result {
        Ok(()) => {
            let model_id = state.model_id;
            app.screen =
                Screen::ModelDetail(model::build_model_detail(&app.config_path, &model_id));
            if let Screen::ModelDetail(ref mut detail) = app.screen {
                detail.error = Some(format!("✓ 已更新 {model_id} 绑定"));
            }
        }
        Err(err) => {
            if let Screen::ProtocolBindingEditor(ref mut current) = app.screen {
                current.error = Some(format!("保存失败: {err}"));
            }
        }
    }
}

async fn apply_model_add(
    config_path: &std::path::Path,
    runtime_dir: &std::path::Path,
    model_id: &str,
    context_window: Option<i64>,
    max_output: Option<i64>,
    copy_from: Option<&str>,
) -> anyhow::Result<()> {
    crate::ownership::with_cli_write_lock_or_delegate_at(
        config_path,
        runtime_dir,
        "llm-proxy tui model add",
        || {
            let config_path = config_path.to_path_buf();
            let model_id = model_id.to_string();
            let copy_from = copy_from.map(ToOwned::to_owned);
            async move {
                crate::model::add(
                    &config_path,
                    &model_id,
                    context_window,
                    max_output,
                    copy_from.as_deref(),
                    None,
                )
            }
        },
        |server| {
            let model_id = model_id.to_string();
            let copy_from = copy_from.map(ToOwned::to_owned);
            Box::pin(async move {
                server
                    .model_add(
                        &model_id,
                        context_window,
                        max_output,
                        copy_from.as_deref(),
                        None,
                    )
                    .await
                    .map(|_| ())
            })
        },
    )
    .await
}

async fn apply_model_set(
    config_path: &std::path::Path,
    runtime_dir: &std::path::Path,
    state: &ModelDetailState,
) -> anyhow::Result<()> {
    if let Ok(cfg) = crate::config::Config::load(config_path)
        && cfg
            .models
            .get(&state.model_id)
            .is_some_and(|m| m.is_routing_model())
    {
        anyhow::bail!(
            "cannot set execution fields on routing model {:?}",
            state.model_id
        );
    }
    let context_window = state.context_window.parse::<i64>()?;
    let max_output = state.max_output_tokens.parse::<i64>()?;
    let desired_features = csv_parts(&state.features);
    let known_features = [
        "image_input",
        "audio_input",
        "video_input",
        "document_input",
        "tools",
        "tool_call_reasoning",
    ];
    let disable_features: Vec<String> = known_features
        .into_iter()
        .filter(|feature| !desired_features.iter().any(|wanted| wanted == feature))
        .map(ToOwned::to_owned)
        .collect();
    let features = desired_features;
    let supported = csv_parts(&state.supported_reasoning_levels);
    let thinking_enabled = state.thinking_enabled;
    let thinking_level = if thinking_enabled && !state.default_reasoning_level.is_empty() {
        Some(state.default_reasoning_level.clone())
    } else {
        None
    };
    crate::ownership::with_cli_write_lock_or_delegate_at(
        config_path,
        runtime_dir,
        "llm-proxy tui model set",
        || {
            let config_path = config_path.to_path_buf();
            let model_id = state.model_id.clone();
            let features = features.clone();
            let disable_features = disable_features.clone();
            let supported = supported.clone();
            let thinking_level = thinking_level.clone();
            async move {
                crate::model::set(
                    &config_path,
                    &model_id,
                    crate::model::SetModelOptions {
                        context_window: Some(context_window),
                        max_output_tokens: Some(max_output),
                        reasoning_levels: (!supported.is_empty())
                            .then_some(crate::model::ReasoningLevelsUpdate::Names(supported)),
                        thinking_level,
                        supports_thinking: Some(thinking_enabled),
                        force_think: None,
                        force_no_think: Some(!thinking_enabled),
                        default_think: Some(thinking_enabled),
                        enable_features: features,
                        disable_features,
                        ..Default::default()
                    },
                )
            }
        },
        |server| {
            let model_id = state.model_id.clone();
            let features = features.clone();
            let disable_features = disable_features.clone();
            let supported = supported.clone();
            let thinking_level = thinking_level.clone();
            Box::pin(async move {
                let reasoning_levels = (!supported.is_empty()).then(|| {
                    crate::model::ReasoningLevelsUpdate::Entries(
                        supported
                            .into_iter()
                            .map(|level| crate::config::ReasoningLevelEntry::new(level, None))
                            .collect(),
                    )
                });
                server
                    .model_set(
                        &model_id,
                        &crate::model::SetModelOptions {
                            context_window: Some(context_window),
                            max_output_tokens: Some(max_output),
                            reasoning_levels,
                            thinking_level,
                            supports_thinking: Some(thinking_enabled),
                            force_think: None,
                            force_no_think: Some(!thinking_enabled),
                            default_think: Some(thinking_enabled),
                            enable_features: features,
                            disable_features,
                            ..Default::default()
                        },
                    )
                    .await
                    .map(|_| ())
            })
        },
    )
    .await
}

async fn apply_model_remove(
    config_path: &std::path::Path,
    runtime_dir: &std::path::Path,
    model_id: &str,
) -> anyhow::Result<()> {
    crate::ownership::with_cli_write_lock_or_delegate_at(
        config_path,
        runtime_dir,
        "llm-proxy tui model remove",
        || {
            let config_path = config_path.to_path_buf();
            let model_id = model_id.to_string();
            async move { crate::model::remove(&config_path, &model_id, true) }
        },
        |server| {
            let model_id = model_id.to_string();
            Box::pin(async move { server.model_remove(&model_id, true).await.map(|_| ()) })
        },
    )
    .await
}

async fn apply_binding_list(
    config_path: &std::path::Path,
    runtime_dir: &std::path::Path,
    state: &ProtocolBindingEditorState,
) -> anyhow::Result<()> {
    crate::ownership::with_cli_write_lock_or_delegate_at(
        config_path,
        runtime_dir,
        "llm-proxy tui model bindings",
        || {
            let config_path = config_path.to_path_buf();
            let runtime_dir = runtime_dir.to_path_buf();
            let state = state.clone();
            async move { write_binding_list_locally(&config_path, &runtime_dir, &state) }
        },
        |server| {
            let state = state.clone();
            Box::pin(async move {
                // Read body and generation from one snapshot — editing a file read
                // taken before the base was captured is a silent overwrite.
                let (mut cfg, base) = server.config_snapshot().await?;
                replace_bindings(&mut cfg, &state)?;
                server.config_update(&cfg, Some(base)).await.map(|_| ())
            })
        },
    )
    .await
}

fn write_binding_list_locally(
    config_path: &std::path::Path,
    runtime_dir: &std::path::Path,
    state: &ProtocolBindingEditorState,
) -> anyhow::Result<()> {
    let mut cfg = Config::load(config_path)?;
    replace_bindings(&mut cfg, state)?;
    let _lock = crate::core::ConfigLock::acquire(runtime_dir, std::time::Duration::from_secs(5))?;
    crate::config_edit::write_model(config_path, &cfg, &state.model_id)
}

fn replace_bindings(cfg: &mut Config, state: &ProtocolBindingEditorState) -> anyhow::Result<()> {
    if !cfg.models.contains_key(&state.model_id) {
        anyhow::bail!("unknown model {}", state.model_id);
    }
    let mut bindings = Vec::with_capacity(state.rows.len());
    for row in &state.rows {
        let provider = cfg
            .providers
            .get(&row.name)
            .ok_or_else(|| anyhow::anyhow!("unknown provider {}", row.name))?;
        if provider.endpoint(state.protocol).is_none() {
            anyhow::bail!(
                "provider {} does not declare {} endpoint",
                row.name,
                state.protocol.field_name()
            );
        }
        if row.upstream.trim().is_empty() {
            anyhow::bail!("upstream model cannot be empty");
        }
        bindings.push(ProviderBinding {
            name: row.name.clone(),
            model: row.upstream.clone(),
        });
    }
    let model = cfg.models.get_mut(&state.model_id).expect("model exists");
    match state.protocol {
        Protocol::OpenaiChatCompletions => model.openai_chat_providers = bindings,
        Protocol::OpenaiResponses => model.openai_responses_providers = bindings,
        Protocol::Anthropic => model.anthropic_providers = bindings,
    }
    cfg.validate()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::{EntryMode, ModelListItem};
    use crossterm::event::KeyModifiers;

    fn write_config(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("config.toml");
        std::fs::write(&path, contents).expect("write config");
        (temp, path)
    }

    fn sample_config() -> &'static str {
        r#"
[server]
listen = "127.0.0.1:0"

[providers.deepseek]
product = "deepseek"
openai_chat = { url = "https://api.deepseek.com/chat/completions" }
openai_responses = { url = "https://api.deepseek.com/responses" }
api_key_env = "DEEPSEEK_API_KEY"

[models.deepseek-v4-pro-lp]
context_window = 100000
max_output_tokens = 4096
features = ["tools"]
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-pro" }]
"#
    }

    #[test]
    fn bare_model_entry_opens_management_list() {
        let (_temp, path) = write_config(sample_config());
        let app = AppModel::new(path, EntryMode::ModelTui { select: None });
        match app.screen {
            Screen::ModelManagement(state) => {
                assert_eq!(state.models.len(), 1);
                assert_eq!(state.models[0].id, "deepseek-v4-pro-lp");
            }
            other => panic!("expected model list, got {other:?}"),
        }
    }

    #[test]
    fn model_select_opens_detail() {
        let (_temp, path) = write_config(sample_config());
        let app = AppModel::new(
            path,
            EntryMode::ModelTui {
                select: Some("deepseek-v4-pro-lp".into()),
            },
        );
        match app.screen {
            Screen::ModelDetail(state) => {
                assert_eq!(state.model_id, "deepseek-v4-pro-lp");
                assert_eq!(state.context_window, "100000");
                assert_eq!(state.chat_bindings.len(), 1);
            }
            other => panic!("expected model detail, got {other:?}"),
        }
    }

    #[test]
    fn list_enter_opens_detail_and_a_opens_add_draft() {
        let (_temp, path) = write_config(sample_config());
        let mut app = AppModel::new(path, EntryMode::ModelTui { select: None });
        handle_model_management_keys(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.screen, Screen::ModelDetail(_)));
        return_to_model_management_preserving_cursor(&mut app, Some("deepseek-v4-pro-lp"));
        handle_model_management_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        match app.screen {
            Screen::ModelDetail(state) => {
                assert!(matches!(state.kind, ModelDetailKind::Add));
                assert!(state.editing);
            }
            other => panic!("expected add draft, got {other:?}"),
        }
    }

    #[test]
    fn copy_opens_prefilled_draft_without_writing() {
        let (_temp, path) = write_config(sample_config());
        let mut app = AppModel::new(path.clone(), EntryMode::ModelTui { select: None });
        handle_model_management_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );
        match &app.screen {
            Screen::ModelDetail(state) => {
                assert_eq!(state.model_id, "deepseek-v4-pro-lp-1");
                assert!(matches!(
                    state.kind,
                    ModelDetailKind::Copy { ref source_id } if source_id == "deepseek-v4-pro-lp"
                ));
            }
            other => panic!("expected copy draft, got {other:?}"),
        }
        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.models.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_draft_save_writes_model() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("config.toml");
        let runtime = temp.path().join("runtime");
        std::fs::write(&path, sample_config()).expect("write");
        apply_model_add(
            &path,
            &runtime,
            "custom-model",
            Some(200000),
            Some(8192),
            None,
        )
        .await
        .expect("add");
        let cfg = Config::load(&path).expect("load");
        let model = cfg.models.get("custom-model").expect("created");
        assert_eq!(model.context_window, 200000);
        assert_eq!(model.max_output_tokens, 8192);
        assert!(model.openai_chat_providers.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn copy_keeps_source_and_bindings() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("config.toml");
        let runtime = temp.path().join("runtime");
        std::fs::write(&path, sample_config()).expect("write");
        apply_model_add(
            &path,
            &runtime,
            "deepseek-v4-pro-lp-1",
            None,
            None,
            Some("deepseek-v4-pro-lp"),
        )
        .await
        .expect("copy");
        let cfg = Config::load(&path).expect("load");
        let source = cfg.models.get("deepseek-v4-pro-lp").expect("source");
        let copied = cfg.models.get("deepseek-v4-pro-lp-1").expect("copied");
        assert_eq!(source.openai_chat_providers.len(), 1);
        assert_eq!(copied.openai_chat_providers.len(), 1);
        assert_eq!(
            copied.openai_chat_providers[0].name,
            source.openai_chat_providers[0].name
        );
        assert_eq!(
            copied.openai_chat_providers[0].model,
            source.openai_chat_providers[0].model
        );
        assert_eq!(copied.context_window, source.context_window);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn binding_save_replaces_protocol_list() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("config.toml");
        let runtime = temp.path().join("runtime");
        std::fs::write(
            &path,
            r#"
[server]
listen = "127.0.0.1:0"

[providers.deepseek]
product = "deepseek"
openai_chat = { url = "https://api.deepseek.com/chat/completions" }

[providers.deepseek-2]
product = "deepseek"
openai_chat = { url = "https://api.deepseek.com/chat/completions" }

[models.m]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "deepseek", model = "src" }]
"#,
        )
        .expect("write");
        let state = ProtocolBindingEditorState {
            model_id: "m".to_string(),
            protocol: Protocol::OpenaiChatCompletions,
            rows: vec![
                BindingRow {
                    name: "deepseek-2".to_string(),
                    upstream: "other".to_string(),
                    kind: "native".to_string(),
                },
                BindingRow {
                    name: "deepseek".to_string(),
                    upstream: "src".to_string(),
                    kind: "native".to_string(),
                },
            ],
            cursor: 0,
            adding: false,
            candidates: Vec::new(),
            candidate_cursor: 0,
            editing_upstream: false,
            upstream_input: String::new(),
            upstream_cursor: 0,
            remove_confirm: false,
            error: None,
            scroll_offset: 0,
        };
        apply_binding_list(&path, &runtime, &state)
            .await
            .expect("save bindings");
        let cfg = Config::load(&path).expect("load");
        let model = cfg.models.get("m").expect("model");
        assert_eq!(model.openai_chat_providers[0].name, "deepseek-2");
        assert_eq!(model.openai_chat_providers[0].model, "other");
        assert_eq!(model.openai_chat_providers[1].name, "deepseek");
    }

    #[test]
    fn unknown_select_stays_on_list() {
        let (_temp, path) = write_config(sample_config());
        let app = AppModel::new(
            path,
            EntryMode::ModelTui {
                select: Some("missing".into()),
            },
        );
        match app.screen {
            Screen::ModelManagement(state) => {
                assert!(
                    state
                        .error
                        .as_deref()
                        .is_some_and(|err| err.contains("unknown model"))
                );
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn list_item_filter_matches_id() {
        let state = ModelManagementState {
            models: vec![ModelListItem {
                id: "claude-sonnet-lp".into(),
                context_window: 1,
                max_output_tokens: 1,
                thinking: "medium".into(),
                features: "image_input".into(),
                provider_count: 1,
                route_count: 0,
            }],
            cursor: 0,
            filter: "sonnet".into(),
            filter_active: true,
            error: None,
            scroll_offset: 0,
        };
        assert_eq!(state.filtered_count(), 1);
    }

    #[test]
    fn draft_cannot_open_binding_editor() {
        let (_temp, path) = write_config(sample_config());
        let mut app = AppModel::new(path, EntryMode::ModelTui { select: None });
        handle_model_management_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        if let Screen::ModelDetail(ref mut state) = app.screen {
            state.editing = false;
            state.cursor = ModelDetailField::ALL
                .iter()
                .position(|field| *field == ModelDetailField::ChatBindings)
                .expect("chat bindings field");
        }
        handle_model_detail_keys(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match app.screen {
            Screen::ModelDetail(state) => {
                assert!(matches!(state.kind, ModelDetailKind::Add));
                assert!(
                    state
                        .error
                        .as_deref()
                        .is_some_and(|err| err.contains("请先保存"))
                );
            }
            other => panic!("expected add draft, got {other:?}"),
        }
    }

    #[test]
    fn existing_binding_enter_opens_editor() {
        let (_temp, path) = write_config(sample_config());
        let mut app = AppModel::new(
            path,
            EntryMode::ModelTui {
                select: Some("deepseek-v4-pro-lp".into()),
            },
        );
        if let Screen::ModelDetail(ref mut state) = app.screen {
            state.cursor = ModelDetailField::ALL
                .iter()
                .position(|field| *field == ModelDetailField::ChatBindings)
                .expect("chat bindings field");
        }
        handle_model_detail_keys(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match app.screen {
            Screen::ProtocolBindingEditor(state) => {
                assert_eq!(state.model_id, "deepseek-v4-pro-lp");
                assert_eq!(state.protocol, Protocol::OpenaiChatCompletions);
                assert_eq!(state.rows.len(), 1);
                assert_eq!(state.rows[0].name, "deepseek");
            }
            other => panic!("expected binding editor, got {other:?}"),
        }
    }

    #[test]
    fn binding_editor_reorders_without_writing() {
        let (_temp, path) = write_config(sample_config());
        let mut app = AppModel::new(path.clone(), EntryMode::ModelTui { select: None });
        app.screen = Screen::ProtocolBindingEditor(ProtocolBindingEditorState {
            model_id: "deepseek-v4-pro-lp".into(),
            protocol: Protocol::OpenaiChatCompletions,
            rows: vec![
                BindingRow {
                    name: "deepseek".into(),
                    upstream: "src".into(),
                    kind: "native".into(),
                },
                BindingRow {
                    name: "deepseek-2".into(),
                    upstream: "other".into(),
                    kind: "native".into(),
                },
            ],
            cursor: 0,
            adding: false,
            candidates: Vec::new(),
            candidate_cursor: 0,
            scroll_offset: 0,
            editing_upstream: false,
            upstream_input: String::new(),
            upstream_cursor: 0,
            remove_confirm: false,
            error: None,
        });
        handle_protocol_binding_editor_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE),
        );
        match &app.screen {
            Screen::ProtocolBindingEditor(state) => {
                assert_eq!(state.rows[0].name, "deepseek-2");
                assert_eq!(state.rows[1].name, "deepseek");
                assert_eq!(state.cursor, 1);
            }
            other => panic!("expected binding editor, got {other:?}"),
        }
        let cfg = Config::load(&path).expect("load");
        assert_eq!(
            cfg.models["deepseek-v4-pro-lp"].openai_chat_providers[0].name,
            "deepseek"
        );
    }

    #[test]
    fn delete_confirm_n_returns_to_list() {
        let (_temp, path) = write_config(sample_config());
        let mut app = AppModel::new(path.clone(), EntryMode::ModelTui { select: None });
        handle_model_management_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );
        assert!(matches!(app.screen, Screen::ModelDeleteConfirm(_)));
        handle_model_delete_confirm_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        );
        match app.screen {
            Screen::ModelManagement(state) => {
                assert_eq!(state.models.len(), 1);
                assert_eq!(state.models[0].id, "deepseek-v4-pro-lp");
            }
            other => panic!("expected list, got {other:?}"),
        }
        assert!(
            Config::load(&path)
                .expect("load")
                .models
                .contains_key("deepseek-v4-pro-lp")
        );
    }

    #[test]
    fn list_q_quits_and_help_covers_layers() {
        let (_temp, path) = write_config(sample_config());
        let mut app = AppModel::new(path, EntryMode::ModelTui { select: None });
        assert!(app.help_text().contains("[a] Add"));
        handle_model_management_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert!(matches!(app.screen, Screen::Quit));

        let mut add = AppModel::new(
            std::path::PathBuf::from("/nonexistent/config.toml"),
            EntryMode::ModelTui { select: None },
        );
        add.screen = Screen::ModelDetail(model::build_add_model_detail());
        if let Screen::ModelDetail(ref mut state) = add.screen {
            state.editing = false;
        }
        assert!(add.help_text().contains("[s] Save"));
    }

    fn routing_sample_config() -> &'static str {
        r#"
[server]
listen = "127.0.0.1:0"

[providers.mock]
api_key_env = "MOCK_API_KEY"
openai_chat = { url = "http://127.0.0.1:18991/chat/completions" }

[models.target-low]
context_window = 1000
max_output_tokens = 100
default_reasoning_level = "low"
reasoning_levels = [{ level = "low", api_value = "1000" }]
openai_chat_providers = [{ name = "mock", model = "upstream-low" }]

[models.target-high]
context_window = 1000
max_output_tokens = 100
default_reasoning_level = "high"
reasoning_levels = [{ level = "high", api_value = "-1" }]
openai_chat_providers = [{ name = "mock", model = "upstream-high" }]

[models.router-lp]
reasoning_routes = [
    { level = "low", target = "target-low" },
    { level = "high", target = "target-high" },
]
"#
    }

    #[test]
    fn routing_model_detail_read_only_and_blocks_edit_save() {
        let (_temp, path) = write_config(routing_sample_config());
        let mut app = AppModel::new(
            path.clone(),
            EntryMode::ModelTui {
                select: Some("router-lp".into()),
            },
        );
        match app.screen {
            Screen::ModelDetail(ref state) => {
                assert!(state.is_routing_model);
                assert_eq!(state.context_window, "1000");
                assert_eq!(state.max_output_tokens, "100");
                assert_eq!(state.routes.len(), 2);
            }
            ref other => panic!("expected model detail, got {other:?}"),
        }

        // Help text indicates read-only scroll, not edit
        assert!(
            app.help_text()
                .contains("[↑/↓] Field  [r] Rename  [Esc] Back")
        );
        assert!(!app.help_text().contains("[Enter] Edit"));

        // Moving down to ContextWindow (index 1) and pressing Enter
        handle_model_detail_keys(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        handle_model_detail_keys(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        if let Screen::ModelDetail(ref state) = app.screen {
            assert!(!state.editing);
            assert!(
                state
                    .error
                    .as_deref()
                    .is_some_and(|err| err.contains("路由模型的能力派生自目标模型，不可直接编辑"))
            );
        } else {
            panic!("expected ModelDetail screen");
        }

        // Pressing 's' (Save) on routing model
        handle_model_detail_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
        );
        if let Screen::ModelDetail(ref state) = app.screen {
            assert!(
                state
                    .error
                    .as_deref()
                    .is_some_and(|err| err.contains("路由模型为只读展示，不可在此保存修改"))
            );
        }

        // Verify config is unchanged and valid, router context_window remains 0
        let loaded = Config::load(&path).expect("load config");
        loaded.validate().expect("validate");
        assert_eq!(loaded.models["router-lp"].context_window, 0);
        assert_eq!(loaded.models["router-lp"].max_output_tokens, 0);
        assert!(loaded.models["router-lp"].features.is_empty());
    }

    #[test]
    fn routing_model_copy_blocked_on_management_list() {
        let (_temp, path) = write_config(routing_sample_config());
        let mut app = AppModel::new(path, EntryMode::ModelTui { select: None });
        if let Screen::ModelManagement(ref mut state) = app.screen {
            state.cursor = state
                .models
                .iter()
                .position(|m| m.id == "router-lp")
                .expect("router-lp in list");
        }
        handle_model_management_keys(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );
        if let Screen::ModelManagement(ref state) = app.screen {
            assert!(
                state
                    .error
                    .as_deref()
                    .is_some_and(|err| err.contains("路由模型暂不支持在 TUI 中复制"))
            );
        } else {
            panic!("expected ModelManagement screen");
        }
    }

    #[tokio::test]
    async fn apply_model_set_bails_on_routing_model_test() {
        let (_temp, path) = write_config(routing_sample_config());
        let runtime = tempfile::tempdir().expect("tempdir");
        let detail = model::build_model_detail(&path, "router-lp");
        let err = apply_model_set(&path, runtime.path(), &detail)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot set execution fields on routing model")
        );
    }
}
