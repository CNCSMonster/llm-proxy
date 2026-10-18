use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::model::{self, AppModel, ProbeConfirmState, Screen, VerifyingState};
use crate::tui::update::utils::handle_filter_input;

pub(crate) fn handle_model_keys(app: &mut AppModel, key: KeyEvent) {
    // If filter is active, only handle filter keys, space, a, Enter
    {
        let filter_active = if let Screen::ModelSelection(ref s) = app.screen {
            s.filter_active
        } else {
            false
        };
        if filter_active {
            match key.code {
                KeyCode::Esc => {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        s.deactivate_filter();
                    }
                    return;
                }
                KeyCode::Enter => {
                    confirm_model_selection(app);
                    return;
                }
                KeyCode::Char(' ') => {
                    toggle_model_selection(app);
                    return;
                }
                KeyCode::Char('a') => {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        toggle_all_models(s);
                    }
                    return;
                }
                KeyCode::Up | KeyCode::Char('k')
                    if key.code == KeyCode::Up || key.modifiers.is_empty() =>
                {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        s.move_up();
                    }
                    return;
                }
                KeyCode::Down | KeyCode::Char('j')
                    if key.code == KeyCode::Down || key.modifiers.is_empty() =>
                {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        s.move_down();
                    }
                    return;
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        s.move_up();
                    }
                    return;
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        s.move_down();
                    }
                    return;
                }
                _ => {
                    if let Screen::ModelSelection(ref mut s) = app.screen {
                        handle_filter_input(&mut s.filter, key);
                    }
                    return;
                }
            }
        }
    }

    // Normal mode
    match key.code {
        KeyCode::Enter => {
            confirm_model_selection(app);
        }
        KeyCode::Char(' ') => {
            toggle_model_selection(app);
        }
        KeyCode::Char('a') => {
            if let Screen::ModelSelection(ref mut s) = app.screen {
                toggle_all_models(s);
            }
        }
        KeyCode::Char('/') => {
            if let Screen::ModelSelection(ref mut s) = app.screen {
                s.activate_filter();
            }
        }
        KeyCode::Char('F') | KeyCode::Char('f') => {
            switch_to_fallback_config(app);
        }
        KeyCode::Char('c') => {
            enter_copy_model_confirm(app);
        }
        KeyCode::Char('r') => {
            if let Screen::ModelSelection(ref mut s) = app.screen
                && s.is_openrouter
            {
                let now = chrono::Utc::now().timestamp();
                if crate::connect::openrouter::check_refresh_debounce(
                    &mut app.openrouter_session,
                    now,
                ) {
                    app.openrouter_session.loading = true;
                    s.error = None;
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::ModelSelection(ref mut s) = app.screen {
                s.move_up();
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ModelSelection(ref mut s) = app.screen {
                s.move_down();
            }
        }
        _ => {}
    }
}

async fn apply_provider_reselection(
    config_path: &std::path::Path,
    runtime_dir: &std::path::Path,
    provider_name: &str,
    models: &[String],
) -> anyhow::Result<()> {
    crate::ownership::with_cli_write_lock_or_delegate_at(
        config_path,
        runtime_dir,
        "llm-proxy tui provider reselect",
        || async {
            crate::connect::reselect_provider_models_at(
                config_path,
                runtime_dir,
                provider_name,
                models,
            )
        },
        |server| {
            let provider_name = provider_name.to_string();
            let models = models.to_vec();
            Box::pin(async move {
                server
                    .reselect_provider(&provider_name, &models)
                    .await
                    .map(|_| ())
            })
        },
    )
    .await
}

/// 本地凭据就绪前置检查（**零网络**），在进入探测确认屏**之前**执行。
///
/// 先校验本地 env var 存在性，**再**询问是否花钱探测。
/// 只覆盖 `ProviderAuthType::ApiKey`（OAuth 无 env var，其就绪性由 `prepare_provider_candidate`
/// 的 `require_auth_env` 权威检查兜底，该检查同样先于任何网络探测）。
fn local_auth_precheck(app: &AppModel) -> std::result::Result<(), String> {
    let Some(product) = app.chosen_product.as_ref() else {
        return Ok(());
    };
    if !matches!(product.auth_type, model::ProviderAuthType::ApiKey) {
        return Ok(());
    }
    let Some(env) = app.chosen_env_var.as_deref().filter(|e| !e.is_empty()) else {
        return Ok(());
    };
    match std::env::var(env) {
        Ok(value) if !value.is_empty() => Ok(()),
        _ => Err(format!(
            "missing API key environment variable {env} — 请先 `export {env}=...` 再重试"
        )),
    }
}

fn confirm_model_selection(app: &mut AppModel) {
    let (selected, product_name, source, is_openrouter) = {
        if let Screen::ModelSelection(ref s) = app.screen {
            (
                s.selected.clone(),
                s.product_name.clone(),
                s.source.clone(),
                s.is_openrouter,
            )
        } else {
            return;
        }
    };
    let mut models: Vec<String> = selected.into_iter().collect();
    models.sort();

    if is_openrouter && models.is_empty() {
        if let Screen::ModelSelection(ref mut state) = app.screen {
            state.error =
                Some("0 selected — press Space to select models or Esc to return".to_string());
        }
        return;
    }

    if let model::ModelSelectionSource::ProviderReselect { provider_name } = source {
        if models.is_empty() {
            if let Screen::ModelSelection(ref mut state) = app.screen {
                state.error = Some("至少选择一个模型".to_string());
            }
            return;
        }
        let config_path = app.config_path.clone();
        let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
        let runtime_dir = crate::service::StateDirs::resolve(config_dir).runtime_dir;
        let result = crate::tui::run_async_sync(async {
            apply_provider_reselection(&config_path, &runtime_dir, &provider_name, &models).await
        });
        match result {
            Ok(()) => {
                super::provider_mgmt::return_to_provider_management_preserving_cursor(
                    app,
                    Some(&provider_name),
                );
                if let Screen::ProviderManagement(ref mut state) = app.screen {
                    state.error = Some(format!("✓ 已更新 {provider_name} 的模型"));
                }
            }
            Err(err) => {
                if let Screen::ModelSelection(ref mut state) = app.screen {
                    state.error = Some(format!("模型更新失败: {err}"));
                }
            }
        }
        return;
    }
    app.chosen_models = models.clone();
    // 无模型可探（如非 OpenRouter 的 0 勾选场景，意为「不绑定模型」）→ 无物可探，
    // 直接进 Verifying；`skip_probe=true` 让 verify 阶段不发任何请求。
    if models.is_empty() {
        enter_verifying(app, product_name, models, true);
        return;
    }
    // 时序铁律：**本地 env var 存在性检查 → 探测确认屏 → 网络探测**。
    // 凭据缺失必须在问用户"是否探测"之前就暴露，而不是等到探测失败。
    if let Err(msg) = local_auth_precheck(app) {
        if let Screen::ModelSelection(ref mut state) = app.screen {
            state.error = Some(msg);
        }
        return;
    }
    // 方案 A：模型确认后进入**探测确认屏**，默认高亮「探测」——
    // 用户连续按两次回车即走默认路径（探测）。SPEC 原本的 OpenRouter 特例
    // `skip_probe=true` 已废止：探测成本 = 已选模型 × 协议数（廉价），
    // 是否探测由用户在确认屏显式决定，不再由产品名硬编码。
    if let Screen::ModelSelection(ref state) = app.screen {
        app.cached_model_selection = Some(state.clone());
    }
    app.screen = Screen::ProbeConfirm(ProbeConfirmState {
        product_name,
        models,
        cursor: 0,
    });
}

/// 探测确认屏按键：Enter = 按当前高亮执行；N = 直接跳过探测；↑↓/j/k 换选项。
pub(crate) fn handle_probe_confirm_keys(app: &mut AppModel, key: KeyEvent) {
    let (product_name, models, probe) = match &app.screen {
        Screen::ProbeConfirm(s) => (s.product_name.clone(), s.models.clone(), s.probe_selected()),
        _ => return,
    };
    match key.code {
        KeyCode::Enter => {}
        KeyCode::Char('n') | KeyCode::Char('N') => {
            enter_verifying(app, product_name, models, true);
            return;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::ProbeConfirm(ref mut s) = app.screen {
                s.cursor = 0;
            }
            return;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::ProbeConfirm(ref mut s) = app.screen {
                s.cursor = 1;
            }
            return;
        }
        _ => return,
    }
    // Enter：按高亮项执行；本地 env var 存在性检查在更早的 prepare 阶段，
    // 这里只决定 skip_probe（是否花钱探活）。
    enter_verifying(app, product_name, models, !probe);
}

fn enter_verifying(
    app: &mut AppModel,
    product_name: String,
    models: Vec<String>,
    skip_probe: bool,
) {
    app.chosen_models = models.clone();
    app.screen = Screen::Verifying(VerifyingState {
        product_name,
        env_var: app.chosen_env_var.clone(),
        models,
        skip_probe,
        progress_done: 0,
        progress_total: 0,
        progress_current_model: None,
    });
}

/// Switch from model selection to fallback config screen.
fn switch_to_fallback_config(app: &mut AppModel) {
    // Cache the current ModelSelectionState so selected/cursor survive round-trips.
    if let Screen::ModelSelection(ref state) = app.screen {
        app.cached_model_selection = Some(state.clone());
    }
    let fallback_state = super::env::build_fallback_state(app);
    app.screen = Screen::FallbackConfig(fallback_state);
}

fn toggle_model_selection(app: &mut AppModel) {
    // 必须用与渲染层一致的过滤列表取下标：filter 激活时光标指向过滤后集合，
    // 直接 s.items.get(cursor) 会切换原数组里的另一条模型
    if let Screen::ModelSelection(ref mut s) = app.screen
        && let Some(item) = s.filtered_item().map(|item| item.id.clone())
    {
        if s.selected.contains(&item) {
            s.selected.remove(&item);
        } else {
            s.selected.insert(item);
        }
    }
}

fn toggle_all_models(state: &mut super::model::ModelSelectionState) {
    if state.selected.len() == state.items.len() {
        state.selected.clear();
    } else {
        for item in &state.items {
            state.selected.insert(item.id.clone());
        }
    }
}

/// Enter the copy-model confirmation screen from the ModelSelection screen.
/// The currently highlighted model (by cursor) becomes the source.
fn enter_copy_model_confirm(app: &mut AppModel) {
    let (source_id, product_name) = if let Screen::ModelSelection(ref s) = app.screen {
        // Cache the current ModelSelectionState so it survives round-trips via Esc
        app.cached_model_selection = Some(s.clone());
        let item = match s.items.get(s.cursor) {
            Some(item) => item,
            None => return,
        };
        (item.id.clone(), s.product_name.clone())
    } else {
        return;
    };

    // Load config to generate a default new ID
    let config = crate::config::Config::load(&app.config_path).ok();
    let default_new_id = config
        .as_ref()
        .map(|cfg| model::generate_default_copy_id(&source_id, &cfg.models))
        .unwrap_or_else(|| format!("{}-1", source_id));

    app.screen = Screen::CopyModelConfirm(model::CopyModelConfirmState {
        source_model_id: source_id,
        default_new_id: default_new_id.clone(),
        new_id: default_new_id,
        cursor_pos: 0, // will be set after screen creation
        editing: true,
        validation: model::NameValidation::Available,
        error: None,
        product_name,
    });
    // Set cursor to end of the default ID
    if let Screen::CopyModelConfirm(ref mut s) = app.screen {
        s.cursor_pos = s.new_id.len();
    }
}

/// Handle keyboard input for the CopyModelConfirm screen.
pub(crate) fn handle_copy_model_confirm_keys(app: &mut AppModel, key: KeyEvent) {
    let editing = if let Screen::CopyModelConfirm(ref s) = app.screen {
        s.editing
    } else {
        return;
    };

    if editing {
        handle_copy_editing_mode(app, key);
    } else {
        handle_copy_browse_mode(app, key);
    }
}

/// Browse mode: `e` enters editing, `Enter` confirms copy.
fn handle_copy_browse_mode(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Char('e') => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen {
                s.editing = true;
                s.cursor_pos = s.new_id.len();
            }
        }
        KeyCode::Enter => {
            confirm_copy_model(app);
        }
        _ => {}
    }
}

/// Editing mode: accept character input, Backspace, Left/Right, Enter.
fn handle_copy_editing_mode(app: &mut AppModel, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            confirm_copy_model(app);
        }
        KeyCode::Esc => {
            // Exit editing mode, revert to default ID
            if let Screen::CopyModelConfirm(ref mut s) = app.screen {
                s.editing = false;
                s.new_id = s.default_new_id.clone();
                s.cursor_pos = 0;
                s.error = None;
                validate_copy_id(s, &app.config_path);
            }
        }
        KeyCode::Char(ch) if ch.is_alphanumeric() || ch == '-' || ch == '_' || ch == '.' => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen {
                if s.cursor_pos >= s.new_id.len() {
                    s.new_id.push(ch);
                } else {
                    s.new_id.insert(s.cursor_pos, ch);
                }
                s.cursor_pos += 1;
                validate_copy_id(s, &app.config_path);
            }
        }
        KeyCode::Backspace => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen
                && s.cursor_pos > 0
            {
                s.cursor_pos -= 1;
                s.new_id.remove(s.cursor_pos);
                validate_copy_id(s, &app.config_path);
            }
        }
        KeyCode::Left => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen {
                s.cursor_pos = s.cursor_pos.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen
                && s.cursor_pos < s.new_id.len()
            {
                s.cursor_pos += 1;
            }
        }
        _ => {}
    }
}

/// Validate the new model ID against existing models.
fn validate_copy_id(s: &mut model::CopyModelConfirmState, config_path: &std::path::Path) {
    let models = crate::config::Config::load(config_path)
        .map(|cfg| cfg.models)
        .unwrap_or_default();
    s.validation = if models.contains_key(&s.new_id) {
        model::NameValidation::Conflict
    } else {
        model::NameValidation::Available
    };
}

async fn apply_model_copy(
    config_path: &std::path::Path,
    runtime_dir: &std::path::Path,
    source_id: &str,
    new_id: &str,
) -> anyhow::Result<()> {
    crate::ownership::with_cli_write_lock_or_delegate_at(
        config_path,
        runtime_dir,
        "llm-proxy tui model copy",
        || async { copy_model_locally(config_path, runtime_dir, source_id, new_id) },
        |server| {
            let source_id = source_id.to_string();
            let new_id = new_id.to_string();
            Box::pin(async move {
                server
                    .model_add(&new_id, None, None, Some(&source_id), None)
                    .await
                    .map(|_| ())
            })
        },
    )
    .await
}

fn copy_model_locally(
    config_path: &std::path::Path,
    runtime_dir: &std::path::Path,
    source_id: &str,
    new_id: &str,
) -> anyhow::Result<()> {
    let _config_lock =
        crate::core::ConfigLock::acquire(runtime_dir, std::time::Duration::from_secs(5))?;
    let cfg = crate::config::Config::load(config_path)?;
    let (cfg, _) = crate::model::apply_mutation(
        cfg,
        crate::model::ConfigMutation::Add {
            model_id: new_id.to_string(),
            context_window: None,
            max_output_tokens: None,
            copy_from: Some(source_id.to_string()),
            routes: None,
        },
    )?;
    crate::config_edit::write_model(config_path, &cfg, new_id)
}

fn copy_success_message(new_id: &str) -> String {
    format!("✓ Model '{}' 已完整复制", new_id)
}

/// Perform the model copy using the same complete-copy semantics as CLI model add.
fn confirm_copy_model(app: &mut AppModel) {
    let (source_id, new_id, validation) = if let Screen::CopyModelConfirm(ref s) = app.screen {
        (
            s.source_model_id.clone(),
            s.new_id.clone(),
            s.validation.clone(),
        )
    } else {
        return;
    };

    // Block if name conflicts
    if validation == model::NameValidation::Conflict {
        if let Screen::CopyModelConfirm(ref mut s) = app.screen {
            s.editing = true;
            s.cursor_pos = s.new_id.len();
        }
        return;
    }

    // Block if new ID is empty
    if new_id.trim().is_empty() {
        if let Screen::CopyModelConfirm(ref mut s) = app.screen {
            s.editing = true;
            s.cursor_pos = 0;
        }
        return;
    }

    let config_path = app.config_path.clone();
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let runtime_dir = crate::service::StateDirs::resolve(config_dir).runtime_dir;

    let result = crate::tui::run_async_sync(async {
        apply_model_copy(&config_path, &runtime_dir, &source_id, &new_id).await
    });

    match result {
        Ok(()) => {
            // Success — show message on ProviderManagement panel
            let msg = copy_success_message(&new_id);
            return_to_model_selection_with_message(app, &msg);
        }
        Err(e) => {
            if let Screen::CopyModelConfirm(ref mut s) = app.screen {
                s.error = Some(format!("复制失败: {}", e));
            }
        }
    }
}

/// Return to the ModelSelection screen, showing a success message.
fn return_to_model_selection_with_message(app: &mut AppModel, message: &str) {
    // We can't easily show a message on ModelSelection (it doesn't have an error field
    // that's used for success). Instead, go back to the ProviderManagement screen
    // which does support success/error messages.
    let providers = model::load_configured_providers(&app.config_path);
    app.screen = Screen::ProviderManagement(model::ProviderManagementState {
        providers,
        cursor: 0,
        error: Some(message.to_string()),
        filter: String::new(),
        filter_active: false,
        scroll_offset: 0,
        quota_fetched_at: None,
    });
}

pub(crate) fn get_model_templates(product_id: Option<&str>) -> Vec<model::ModelItem> {
    let Some(product_id) = product_id else {
        return Vec::new();
    };
    crate::catalog::built_in_providers()
        .into_iter()
        .find(|entry| entry.id == product_id)
        .map(|entry| {
            entry
                .model_templates
                .into_iter()
                .map(|template| {
                    let reasoning_levels: Vec<String> = template
                        .reasoning_level_map
                        .map(|map| map.iter().map(|(k, _)| k.to_string()).collect())
                        .unwrap_or_default();
                    // 孤儿 default（有默认档但无档位表）不落盘，与配置侧宁缺毋假一致
                    let default_reasoning_level = if reasoning_levels.is_empty() {
                        None
                    } else {
                        template.default_reasoning_level.map(|s| s.to_string())
                    };
                    model::ModelItem {
                        id: template.frontend_id.to_string(),
                        display_name: template.upstream_model.to_string(),
                        context_window: Some(template.context_window),
                        max_output_tokens: Some(template.max_output_tokens),
                        features: template.features.iter().map(|s| s.to_string()).collect(),
                        reasoning_levels,
                        default_reasoning_level,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        CodexServiceTierConfig, Config, EndpointConfig, ModelConfig, ProviderBinding,
        ProviderConfig, ReasoningLevelEntry,
    };
    use crate::tui::model::{
        CopyModelConfirmState, EntryMode, ModelItem, ModelSelectionState, NameValidation,
        ProductItem, ProviderAuthType, Screen,
    };
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn make_test_app() -> AppModel {
        let mut app = AppModel::new(PathBuf::from("/dev/null"), EntryMode::Connect);
        app.chosen_product = Some(ProductItem {
            id: "deepseek".to_string(),
            display_name: "DeepSeek".to_string(),
            auth_type: ProviderAuthType::ApiKey,
            endpoint_count: 1,
            product_kind: "payg".to_string(),
            is_custom: false,
        });
        app.chosen_env_var = Some("DEEPSEEK_API_KEY".to_string());
        app.screen = Screen::ModelSelection(ModelSelectionState {
            items: vec![
                ModelItem {
                    id: "deepseek-v4-flash-lp".to_string(),
                    display_name: "deepseek-v4-flash".to_string(),
                    context_window: Some(1_000_000),
                    max_output_tokens: Some(393_216),
                    features: vec![],
                    reasoning_levels: vec![],
                    default_reasoning_level: None,
                },
                ModelItem {
                    id: "deepseek-v4-pro-lp".to_string(),
                    display_name: "deepseek-v4-pro".to_string(),
                    context_window: Some(1_000_000),
                    max_output_tokens: Some(393_216),
                    features: vec![],
                    reasoning_levels: vec![],
                    default_reasoning_level: None,
                },
            ],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            selected: HashSet::new(),
            configured: HashSet::new(),
            error: None,
            product_name: "DeepSeek".to_string(),
            source: model::ModelSelectionSource::Connect,
            is_openrouter: false,
            is_degraded: false,
            openrouter_cached_at: None,
        });
        app
    }

    fn plain_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn copy_test_config() -> Config {
        let mut cfg: Config =
            toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").expect("base config");
        cfg.providers.clear();
        cfg.models.clear();
        for provider_id in ["p1", "p2"] {
            cfg.providers.insert(
                provider_id.to_string(),
                ProviderConfig {
                    openai_chat: Some(EndpointConfig {
                        url: Some(format!("https://{provider_id}.example.test/chat")),
                        ..Default::default()
                    }),
                    openai_responses: Some(EndpointConfig {
                        url: Some(format!("https://{provider_id}.example.test/responses")),
                        ..Default::default()
                    }),
                    anthropic: Some(EndpointConfig {
                        url: Some(format!("https://{provider_id}.example.test/anthropic")),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            );
        }
        cfg.models.insert(
            "source".to_string(),
            ModelConfig {
                description: Some("copy every model field".to_string()),
                context_window: 123_456,
                max_output_tokens: 7_890,
                features: vec!["tools".to_string(), "image_input".to_string()],
                default_reasoning_level: Some("high".to_string()),
                supports_thinking: Some(true),
                force_think: None,
                force_no_think: None,
                default_think: None,
                enable_thinking: None,
                chat_reasoning_off_format: None,
                codex_service_tier: Some(CodexServiceTierConfig {
                    tiers: vec!["priority".to_string()],
                    default_tier: Some("priority".to_string()),
                    forced_tier: None,
                }),
                openai_chat_providers: vec![
                    ProviderBinding {
                        name: "p2".to_string(),
                        model: "source-chat-secondary".to_string(),
                    },
                    ProviderBinding {
                        name: "p1".to_string(),
                        model: "source-chat-primary".to_string(),
                    },
                ],
                openai_responses_providers: vec![
                    ProviderBinding {
                        name: "p1".to_string(),
                        model: "source-responses-primary".to_string(),
                    },
                    ProviderBinding {
                        name: "p2".to_string(),
                        model: "source-responses-secondary".to_string(),
                    },
                ],
                anthropic_providers: vec![
                    ProviderBinding {
                        name: "p2".to_string(),
                        model: "source-anthropic-secondary".to_string(),
                    },
                    ProviderBinding {
                        name: "p1".to_string(),
                        model: "source-anthropic-primary".to_string(),
                    },
                ],
                reasoning_levels: Some(vec![
                    ReasoningLevelEntry::new("low", Some("1024".to_string())),
                    ReasoningLevelEntry::new("high", Some("8192".to_string())),
                ]),
                reasoning_level_map: None,
                supported_reasoning_levels: Vec::new(),
                cooldown: None,
                reasoning_routes: None,
            },
        );
        cfg
    }

    fn copy_confirm_app(config_path: PathBuf, source: &str, target: &str) -> AppModel {
        let mut app = AppModel::new(config_path, EntryMode::ProviderTui { select: None });
        app.screen = Screen::CopyModelConfirm(CopyModelConfirmState {
            source_model_id: source.to_string(),
            default_new_id: target.to_string(),
            new_id: target.to_string(),
            cursor_pos: target.len(),
            editing: false,
            validation: NameValidation::Available,
            error: None,
            product_name: "test".to_string(),
        });
        app
    }

    fn binding_pairs(bindings: &[ProviderBinding]) -> Vec<(&str, &str)> {
        bindings
            .iter()
            .map(|binding| (binding.name.as_str(), binding.model.as_str()))
            .collect()
    }

    fn copy_with_temp_runtime(
        config_path: &std::path::Path,
        runtime_dir: &std::path::Path,
        source_id: &str,
        new_id: &str,
    ) -> anyhow::Result<()> {
        crate::ownership::with_cli_write_lock_at(
            runtime_dir,
            "llm-proxy tui model copy test",
            || copy_model_locally(config_path, runtime_dir, source_id, new_id),
        )
    }

    #[test]
    fn model_templates_project_every_catalog_entry() {
        for entry in crate::catalog::built_in_providers() {
            let items = get_model_templates(Some(entry.id));
            assert_eq!(
                items.len(),
                entry.model_templates.len(),
                "{} template count",
                entry.id
            );
            for (item, template) in items.iter().zip(&entry.model_templates) {
                assert_eq!(item.id, template.frontend_id, "{} frontend ID", entry.id);
                assert_eq!(
                    item.display_name, template.upstream_model,
                    "{} display name",
                    entry.id
                );
                assert_eq!(
                    item.context_window,
                    Some(template.context_window),
                    "{} context window",
                    entry.id
                );
                assert_eq!(
                    item.max_output_tokens,
                    Some(template.max_output_tokens),
                    "{} max output",
                    entry.id
                );
                assert_eq!(
                    item.supports_image(),
                    template.features.contains(&"image_input"),
                    "{} image capability",
                    entry.id
                );
                // 思考档位：有映射表 → 有档位且默认档命中；无映射表 → 档位空且默认档为 None
                let expected_levels: Vec<String> = template
                    .reasoning_level_map
                    .map(|m| m.iter().map(|(k, _)| k.to_string()).collect())
                    .unwrap_or_default();
                assert_eq!(
                    item.reasoning_levels, expected_levels,
                    "{} levels",
                    entry.id
                );
                if expected_levels.is_empty() {
                    assert_eq!(
                        item.default_reasoning_level, None,
                        "{} orphan default must be dropped",
                        entry.id
                    );
                } else {
                    assert_eq!(
                        item.default_reasoning_level.as_deref(),
                        template.default_reasoning_level,
                        "{} default level",
                        entry.id
                    );
                }
            }
        }
    }

    #[test]
    fn confirm_model_selection_zero_selection_skips_to_verifying() {
        let mut app = make_test_app();
        // No models selected (selected set is empty)
        handle_model_keys(&mut app, plain_key(KeyCode::Enter));
        match &app.screen {
            Screen::Verifying(v) => {
                assert!(
                    v.models.is_empty(),
                    "zero selection should produce empty models"
                );
                assert_eq!(v.product_name, "DeepSeek");
            }
            other => panic!("expected Verifying screen, got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reselect_confirmation_updates_only_selected_provider_bindings() {
        let config_path = tempfile::NamedTempFile::new().expect("temp config");
        std::fs::write(
            config_path.path(),
            r#"
[server]
listen = "127.0.0.1:0"

[providers.deepseek]
product = "deepseek"
openai_chat = { url = "https://api.deepseek.com/chat/completions" }
openai_responses = { derive_from = "openai_chat" }
anthropic = { url = "https://api.deepseek.com/anthropic/v1/messages" }

[providers.fallback]
product = "custom"
openai_chat = { url = "https://example.test/chat/completions" }

[models.deepseek-v4-flash-lp]
context_window = 1000000
max_output_tokens = 393216
openai_chat_providers = [{ name = "deepseek", model = "deepseek-v4-flash" }, { name = "fallback", model = "fallback-chat" }]
"#,
        )
        .expect("write config");
        let runtime_dir = tempfile::tempdir().expect("runtime dir");
        apply_provider_reselection(
            config_path.path(),
            runtime_dir.path(),
            "deepseek",
            &["deepseek-v4-pro-lp".to_string()],
        )
        .await
        .expect("reselect models");

        let config = crate::config::Config::load(config_path.path()).expect("updated config");
        let flash = &config.models["deepseek-v4-flash-lp"];
        assert!(
            !flash
                .openai_chat_providers
                .iter()
                .any(|binding| binding.name == "deepseek")
        );
        assert!(
            flash
                .openai_chat_providers
                .iter()
                .any(|binding| binding.name == "fallback")
        );
        let pro = &config.models["deepseek-v4-pro-lp"];
        assert!(
            pro.openai_chat_providers
                .iter()
                .any(|binding| binding.name == "deepseek")
        );
    }

    #[test]
    fn f_key_navigates_to_fallback_config() {
        let mut app = make_test_app();
        handle_model_keys(&mut app, plain_key(KeyCode::Char('F')));
        assert!(
            matches!(&app.screen, Screen::FallbackConfig(_)),
            "pressing F on ModelSelection should navigate to FallbackConfig"
        );
    }

    #[test]
    fn lowercase_f_key_also_navigates_to_fallback_config() {
        let mut app = make_test_app();
        handle_model_keys(&mut app, plain_key(KeyCode::Char('f')));
        assert!(
            matches!(&app.screen, Screen::FallbackConfig(_)),
            "pressing f on ModelSelection should also navigate to FallbackConfig"
        );
    }

    #[test]
    fn copy_model_clones_every_field_and_binding_order_without_mutating_source() {
        let temp = tempfile::tempdir().expect("temp state");
        let config_path = temp.path().join("config.toml");
        let runtime_dir = temp.path().join("runtime");
        let before = copy_test_config();
        crate::config_edit::write_full_config(&config_path, &before).expect("write config");
        let source_before = toml::Value::try_from(&before.models["source"]).expect("serialize");

        copy_with_temp_runtime(&config_path, &runtime_dir, "source", "copied").expect("copy model");

        let after = Config::load(&config_path).expect("load copied config");
        let source_after = toml::Value::try_from(&after.models["source"]).expect("serialize");
        let copied = after.models.get("copied").expect("copied model");
        assert_eq!(
            toml::Value::try_from(copied).expect("serialize"),
            source_before,
            "the complete ModelConfig must be cloned"
        );
        assert_eq!(
            source_after, source_before,
            "source model must remain unchanged"
        );
        assert_eq!(
            binding_pairs(&copied.openai_chat_providers),
            vec![
                ("p2", "source-chat-secondary"),
                ("p1", "source-chat-primary")
            ]
        );
        assert_eq!(
            binding_pairs(&copied.openai_responses_providers),
            vec![
                ("p1", "source-responses-primary"),
                ("p2", "source-responses-secondary")
            ]
        );
        assert_eq!(
            binding_pairs(&copied.anthropic_providers),
            vec![
                ("p2", "source-anthropic-secondary"),
                ("p1", "source-anthropic-primary")
            ]
        );
        let message = copy_success_message("copied");
        assert!(!message.contains("手动添加 provider 绑定"));
    }

    #[test]
    fn cancelling_copy_leaves_config_unchanged() {
        let temp = tempfile::tempdir().expect("temp state");
        let config_path = temp.path().join("config.toml");
        crate::config_edit::write_full_config(&config_path, &copy_test_config())
            .expect("write config");
        let before = std::fs::read(&config_path).expect("read before");
        let mut app = copy_confirm_app(config_path.clone(), "source", "copied");

        crate::tui::update::handle_key(&mut app, plain_key(KeyCode::Esc));

        assert!(matches!(app.screen, Screen::ProviderManagement(_)));
        assert_eq!(std::fs::read(config_path).expect("read after"), before);
    }

    #[test]
    fn stale_name_validation_cannot_overwrite_target_created_before_locked_copy() {
        let temp = tempfile::tempdir().expect("temp state");
        let config_path = temp.path().join("config.toml");
        let runtime_dir = temp.path().join("runtime");
        let mut cfg = copy_test_config();
        let mut concurrent_target = cfg.models["source"].clone();
        concurrent_target.description = Some("concurrently added target".to_string());
        cfg.models
            .insert("copied".to_string(), concurrent_target.clone());
        crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");
        let before = std::fs::read(&config_path).expect("read before");

        let error = copy_with_temp_runtime(&config_path, &runtime_dir, "source", "copied")
            .expect_err("conflict must fail")
            .to_string();

        let after = Config::load(&config_path).expect("load config");
        assert_eq!(
            after.models["copied"].description,
            concurrent_target.description
        );
        assert_eq!(std::fs::read(&config_path).expect("read after"), before);
        assert!(error.contains("already exists"));
    }

    #[test]
    fn missing_source_copy_fails_without_creating_partial_target() {
        let temp = tempfile::tempdir().expect("temp state");
        let config_path = temp.path().join("config.toml");
        let runtime_dir = temp.path().join("runtime");
        crate::config_edit::write_full_config(&config_path, &copy_test_config())
            .expect("write config");
        let before = std::fs::read(&config_path).expect("read before");

        let error = copy_with_temp_runtime(&config_path, &runtime_dir, "missing", "copied")
            .expect_err("missing source must fail")
            .to_string();

        let after = Config::load(&config_path).expect("load config");
        assert!(!after.models.contains_key("copied"));
        assert_eq!(std::fs::read(&config_path).expect("read after"), before);
        assert!(error.contains("missing"));
    }

    #[test]
    fn test_openrouter_zero_selection_blocks_confirm() {
        let mut app = make_test_app();
        app.screen = Screen::ModelSelection(model::ModelSelectionState {
            items: vec![model::ModelItem {
                id: "test/m1".to_string(),
                display_name: "m1".to_string(),
                context_window: Some(100_000),
                max_output_tokens: Some(4096),
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
            source: model::ModelSelectionSource::Connect,
            is_openrouter: true,
            is_degraded: false,
            openrouter_cached_at: Some(1_700_000_000),
        });

        // 零勾选状态下按回车确认
        handle_model_keys(&mut app, plain_key(KeyCode::Enter));

        // 界面仍然停留在 ModelSelection，且设置了提示信息
        if let Screen::ModelSelection(ref s) = app.screen {
            assert!(
                s.error
                    .as_deref()
                    .unwrap_or("")
                    .contains("0 selected — press Space to select models or Esc to return")
            );
        } else {
            panic!("must remain on ModelSelection screen when 0 selected");
        }
    }
    /// 0 勾选拦截 → 空格勾选 → 回车进入**探测确认屏**，且默认高亮 = 探测。
    #[test]
    fn openrouter_tui_enter_after_selection_lands_on_probe_confirm() {
        // 本地 env 前置检查要求凭据存在；测试自设，保证不依赖外部 shell 环境
        unsafe { std::env::set_var("OPENROUTER_API_KEY", "lp-test-key-for-probe-confirm") };
        let temp = tempfile::tempdir().expect("temp state");
        let config_path = temp.path().join("config.toml");
        let mut app = AppModel::new(config_path, model::EntryMode::Connect);
        app.chosen_product = Some(model::ProductItem {
            id: "openrouter".to_string(),
            display_name: "OpenRouter".to_string(),
            auth_type: model::ProviderAuthType::ApiKey,
            endpoint_count: 2,
            product_kind: "payg".to_string(),
            is_custom: false,
        });
        app.chosen_env_var = Some("OPENROUTER_API_KEY".to_string());
        app.screen = Screen::ModelSelection(model::ModelSelectionState {
            items: vec![ModelItem {
                id: "stepfun/step-5-preview".to_string(),
                display_name: "StepFun Step 5 Preview".to_string(),
                context_window: Some(1_000_000),
                max_output_tokens: Some(64_000),
                features: vec!["image_input".to_string()],
                reasoning_levels: vec!["low".to_string(), "high".to_string()],
                default_reasoning_level: Some("low".to_string()),
            }],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            selected: HashSet::new(),
            configured: HashSet::new(),
            error: None,
            product_name: "OpenRouter".to_string(),
            source: model::ModelSelectionSource::Connect,
            is_openrouter: true,
            is_degraded: false,
            openrouter_cached_at: Some(1_700_000_000),
        });

        // 1. 0 勾选回车被拦截
        handle_model_keys(&mut app, plain_key(KeyCode::Enter));
        match &app.screen {
            Screen::ModelSelection(s) => assert!(
                s.error
                    .as_deref()
                    .unwrap_or("")
                    .contains("0 selected — press Space to select models"),
                "0 selected error must be shown: {:?}",
                s.error
            ),
            other => panic!("expected ModelSelection, got {:?}", other),
        }

        // 2. 空格勾选
        handle_model_keys(&mut app, plain_key(KeyCode::Char(' ')));
        if let Screen::ModelSelection(ref s) = app.screen {
            assert!(s.selected.contains("stepfun/step-5-preview"));
        }

        // 3. 回车 → 探测确认屏，且默认高亮 = 探测（cursor 0）
        handle_model_keys(&mut app, plain_key(KeyCode::Enter));
        match &app.screen {
            Screen::ProbeConfirm(p) => {
                assert_eq!(p.models, vec!["stepfun/step-5-preview"]);
                assert_eq!(p.cursor, 0, "默认高亮必须是「探测」");
                assert!(p.probe_selected(), "cursor 0 = 选中探测");
            }
            other => panic!("expected ProbeConfirm, got {:?}", other),
        }
    }

    /// 探测确认屏：默认 Enter = 探测（skip_probe=false）；N = 跳过（skip_probe=true）。
    /// 时序铁律：凭据缺失必须在**进入探测确认屏之前**就被拦截并提示，
    /// 而不是等到探测失败才暴露。
    #[test]
    fn missing_env_var_blocks_before_probe_confirm_screen() {
        let missing = "LLM_PROXY_TEST_MISSING_ENV_BLOCK_PROBE_CONFIRM";
        unsafe { std::env::remove_var(missing) };

        let temp = tempfile::tempdir().expect("temp state");
        let config_path = temp.path().join("config.toml");
        let mut app = AppModel::new(config_path, model::EntryMode::Connect);
        app.chosen_product = Some(model::ProductItem {
            id: "openrouter".to_string(),
            display_name: "OpenRouter".to_string(),
            auth_type: model::ProviderAuthType::ApiKey,
            endpoint_count: 2,
            product_kind: "payg".to_string(),
            is_custom: false,
        });
        app.chosen_env_var = Some(missing.to_string());
        app.screen = Screen::ModelSelection(model::ModelSelectionState {
            items: vec![ModelItem {
                id: "anthropic/claude-sonnet-4".to_string(),
                display_name: "Claude Sonnet 4".to_string(),
                context_window: Some(200_000),
                max_output_tokens: Some(64_000),
                features: vec![],
                reasoning_levels: vec![],
                default_reasoning_level: None,
            }],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            selected: HashSet::from(["anthropic/claude-sonnet-4".to_string()]),
            configured: HashSet::new(),
            error: None,
            product_name: "OpenRouter".to_string(),
            source: model::ModelSelectionSource::Connect,
            is_openrouter: true,
            is_degraded: false,
            openrouter_cached_at: Some(1_700_000_000),
        });

        handle_model_keys(&mut app, plain_key(KeyCode::Enter));

        match &app.screen {
            Screen::ModelSelection(s) => {
                let err = s.error.as_deref().unwrap_or("");
                assert!(
                    err.contains(missing),
                    "错误须点名缺失的 env var，实际: {err:?}"
                );
            }
            other => panic!("凭据缺失必须停留在 ModelSelection，实际: {:?}", other),
        }
    }

    #[test]
    fn openrouter_probe_confirm_enter_probes_and_n_skips() {
        let make_probe = |app: &mut AppModel| {
            app.screen = Screen::ProbeConfirm(model::ProbeConfirmState {
                product_name: "OpenRouter".to_string(),
                models: vec!["anthropic/claude-sonnet-4".to_string()],
                cursor: 0,
            });
        };

        // 默认（cursor=0）按 Enter → 探测
        let mut app = make_test_app();
        make_probe(&mut app);
        handle_probe_confirm_keys(&mut app, plain_key(KeyCode::Enter));
        match &app.screen {
            Screen::Verifying(v) => {
                assert_eq!(v.models, vec!["anthropic/claude-sonnet-4"]);
                assert!(!v.skip_probe, "默认必须探测");
            }
            other => panic!("expected Verifying, got {:?}", other),
        }

        // 光标移到「跳过」后按 N → 不探测
        let mut app = make_test_app();
        make_probe(&mut app);
        handle_probe_confirm_keys(&mut app, plain_key(KeyCode::Char('n')));
        match &app.screen {
            Screen::Verifying(v) => assert!(v.skip_probe, "N 必须跳过探测"),
            other => panic!("expected Verifying, got {:?}", other),
        }
    }

    #[test]
    fn openrouter_tui_r_key_debounce_in_model_selection() {
        let temp = tempfile::tempdir().expect("temp state");
        let config_path = temp.path().join("config.toml");
        let mut app = AppModel::new(config_path, model::EntryMode::Connect);
        app.screen = Screen::ModelSelection(model::ModelSelectionState {
            items: vec![],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            selected: HashSet::new(),
            configured: HashSet::new(),
            error: Some("old error".to_string()),
            product_name: "OpenRouter".to_string(),
            source: model::ModelSelectionSource::Connect,
            is_openrouter: true,
            is_degraded: false,
            openrouter_cached_at: Some(1_700_000_000),
        });

        // 第一次按 r：触发刷新，loading=true，error 清空
        handle_model_keys(&mut app, plain_key(KeyCode::Char('r')));
        assert!(app.openrouter_session.loading);
        if let Screen::ModelSelection(ref s) = app.screen {
            assert_eq!(s.error, None);
        }

        // 立即重置 loading，再按 r：处于 3s 防抖窗口内不得再次触发
        app.openrouter_session.loading = false;
        handle_model_keys(&mut app, plain_key(KeyCode::Char('r')));
        assert!(
            !app.openrouter_session.loading,
            "debounced r key must not trigger refresh within 3s"
        );
    }
}
