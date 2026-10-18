mod style;

pub mod fuzzy;
pub mod model;
pub mod update;
pub mod view;
pub mod widgets;

use self::update::products::make_product_state;

use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::terminal_session::TerminalSession;
use crate::tui_control::{GlobalControlKey, global_control_key};

use self::model::{ConfigResult, EntryMode};

fn log_oauth_tui_failure(flow: &str, provider_id: &str, error: &anyhow::Error) {
    tracing::error!(
        target: "llm_proxy::oauth",
        event = "oauth_tui_failure",
        flow,
        provider = %provider_id,
        error = %crate::auth::oauth_error_log_summary(&format!("{error:#}")),
        "OAuth operation failed in TUI"
    );
}

/// 同步上下文（TUI handle_key）中执行异步委托操作：
/// server 运行时 TUI 写配置委托 server，保持单一写者。
pub fn run_async_sync<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

/// Main TUI entry point. Called from `connect` or `provider` CLI commands.
pub async fn run(config_path: &std::path::Path, entry: EntryMode) -> Result<()> {
    let config_path = config_path.to_path_buf();

    let mut session = TerminalSession::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // Initialize state
    let mut app = model::AppModel::new(config_path, entry);
    let mut verifying_task: Option<tokio::task::JoinHandle<Result<Vec<model::ConfigResult>>>> =
        None;
    let mut verifying_progress_rx: Option<
        tokio::sync::watch::Receiver<crate::connect::ProbeProgress>,
    > = None;
    let mut openrouter_task: Option<
        tokio::task::JoinHandle<(
            u64,
            Result<Vec<crate::connect::openrouter::OpenRouterRawModel>>,
        )>,
    > = None;

    // Main loop
    loop {
        // OpenRouter 异步动态模型拉取
        if app.openrouter_session.loading && openrouter_task.is_none() {
            app.openrouter_session.request_token += 1;
            let request_token = app.openrouter_session.request_token;
            let config_dir = app
                .config_path
                .parent()
                .unwrap_or(std::path::Path::new("."));
            let state_dir = crate::service::StateDirs::resolve(config_dir).state_dir;
            let now = chrono::Utc::now().timestamp();
            let client = reqwest::Client::new();
            let url = "https://openrouter.ai/api/v1/models".to_string();

            let handle = tokio::spawn(async move {
                let res = crate::connect::openrouter::fetch_openrouter_catalog_online(
                    state_dir, &client, &url, now,
                )
                .await;
                (request_token, res)
            });
            app.openrouter_session.in_flight = Some(handle.abort_handle());
            openrouter_task = Some(handle);
        }

        if let Some(task) = openrouter_task.as_ref()
            && task.is_finished()
        {
            let task = openrouter_task.take().expect("openrouter task present");
            app.openrouter_session.in_flight = None;
            if let Ok((token, result)) = task.await
                && token == app.openrouter_session.request_token
            {
                app.openrouter_session.loading = false;
                let now = chrono::Utc::now().timestamp();
                match result {
                    Ok(raw_models) => {
                        let items =
                            crate::connect::openrouter::convert_raw_models_to_items(&raw_models);
                        if let model::Screen::ModelSelection(ref mut s) = app.screen {
                            s.items = items;
                            s.is_degraded = false;
                            s.openrouter_cached_at = Some(now);
                            s.error = None;
                        }
                    }
                    Err(err) => {
                        let config_dir = app
                            .config_path
                            .parent()
                            .unwrap_or(std::path::Path::new("."));
                        let state_dir = crate::service::StateDirs::resolve(config_dir).state_dir;
                        let cache_opt =
                            crate::connect::openrouter::read_cache_file(&state_dir, now);

                        if let model::Screen::ModelSelection(ref mut s) = app.screen {
                            if !s.items.is_empty() {
                                s.error =
                                    Some(format!("Failed to refresh OpenRouter models: {err}"));
                            } else if let Some(cache) = cache_opt {
                                s.items = crate::connect::openrouter::convert_raw_models_to_items(
                                    &cache.models,
                                );
                                s.is_degraded = true;
                                s.openrouter_cached_at = Some(cache.fetched_at);
                                s.error = None;
                                app.openrouter_session.last_failure = Some((
                                    crate::connect::openrouter::FailureKind::NetworkOrTimeout,
                                    now,
                                ));
                            } else {
                                s.error = Some(format!(
                                    "Network unreachable: {err} (press [r] to retry or [Esc] to return)"
                                ));
                                app.openrouter_session.last_failure = Some((
                                    crate::connect::openrouter::FailureKind::NetworkOrTimeout,
                                    now,
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Render
        terminal.draw(|f| view::render(f, &app))?;

        // Check quit
        if matches!(app.screen, model::Screen::Quit) {
            break Ok(());
        }

        // Handle Verifying state — spawn task on first entry, poll progress thereafter
        if let model::Screen::Verifying(ref v) = app.screen {
            let skip_probe = v.skip_probe;

            // Spawn the write task on first entry
            if verifying_task.is_none() {
                let product_id = app.chosen_product.as_ref().map(|p| p.id.clone());
                let provider_name = app
                    .oauth_provider_name
                    .clone()
                    .or_else(|| app.chosen_product.as_ref().map(|p| p.id.clone()));
                let env_var = app.chosen_env_var.clone();
                let models = app.chosen_models.clone();
                let custom_type = app.custom_provider_type.clone();
                let custom_endpoint = app.custom_provider_endpoint.clone();
                let config_path = app.config_path.clone();
                let pending_account = app.pending_account.clone();

                let (progress_tx, progress_rx) =
                    tokio::sync::watch::channel(crate::connect::ProbeProgress {
                        done: 0,
                        total: models.len(),
                        current_model: models.first().cloned().unwrap_or_default(),
                    });

                let handle = tokio::spawn(async move {
                    write_provider_config(
                        &config_path,
                        product_id.as_deref(),
                        env_var.as_deref(),
                        &models,
                        custom_type.as_deref(),
                        custom_endpoint.as_deref(),
                        provider_name.as_deref(),
                        skip_probe,
                        pending_account.as_ref(),
                        Some(progress_tx),
                    )
                    .await
                });
                verifying_task = Some(handle);
                verifying_progress_rx = Some(progress_rx);
            }

            // Poll progress updates
            if let Some(ref rx) = verifying_progress_rx {
                let progress = rx.borrow().clone();
                if let model::Screen::Verifying(ref mut v) = app.screen {
                    v.progress_done = progress.done;
                    v.progress_total = progress.total;
                    v.progress_current_model = Some(progress.current_model);
                }
            }

            // Check if task is done
            let task_done = if verifying_task.as_ref().is_some_and(|h| h.is_finished()) {
                verifying_task.take().map(|h| {
                    // is_finished() == true → await returns immediately
                    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(h))
                })
            } else {
                None
            };

            if let Some(join_result) = task_done {
                verifying_task = None;
                verifying_progress_rx = None;

                let product_id = app.chosen_product.as_ref().map(|p| p.id.clone());
                let provider_name = app
                    .oauth_provider_name
                    .clone()
                    .or_else(|| app.chosen_product.as_ref().map(|p| p.id.clone()));
                let config_path = app.config_path.clone();
                let endpoint_filter = app.endpoint_filter.clone();
                let fallback_target = app.fallback_target.clone();
                let fallback_bindings = app.fallback_bindings.clone();

                let result = join_result.unwrap_or_else(|e| Err(e.into()));
                match result {
                    Ok(results) => {
                        app.pending_account = None;
                        let pname = provider_name.as_deref().unwrap_or("unknown");
                        if let Some(ref filter) = endpoint_filter
                            && let Err(e) = apply_endpoint_filter(&config_path, pname, filter)
                        {
                            tracing::warn!("endpoint filter post-processing failed: {e}");
                        }
                        if let Some(ref target) = fallback_target
                            && !fallback_bindings.is_empty()
                            && let Err(e) = crate::fallback::add_fallback(
                                &config_path,
                                pname,
                                target,
                                &fallback_bindings,
                            )
                        {
                            tracing::warn!("fallback chain insertion failed: {e}");
                        }
                        app.endpoint_filter = None;
                        app.fallback_target = None;
                        app.fallback_bindings = Vec::new();
                        app.session_results = results.clone();
                        app.screen = model::Screen::Done(model::DoneState { results });
                    }
                    Err(e) => {
                        if crate::admin_client::is_recoverable_warning(&e) {
                            app.screen =
                                model::Screen::WarningConfirm(model::WarningConfirmState {
                                    message: e.to_string(),
                                    selected_option: model::WarningOption::Continue,
                                    back_to_product: false,
                                    error: None,
                                });
                        } else {
                            app.pending_account = None;
                            let display_name = provider_name
                                .or(product_id)
                                .unwrap_or_else(|| "provider".to_string());
                            let results = vec![model::ConfigResult {
                                provider: display_name,
                                success: false,
                                message: format!("Failed to configure: {e:#}"),
                            }];
                            app.session_results = results.clone();
                            app.screen = model::Screen::Done(model::DoneState { results });
                        }
                    }
                }
            }
            continue;
        }

        // OAuth device code login: temporarily leave raw mode for CLI interaction
        if let model::Screen::OAuthDeviceCode(ref state) = app.screen
            && !state.polling
            && state.device_code.is_empty()
        {
            let provider_id = state.provider_name.clone();
            let cfg = match crate::config::Config::load(&app.config_path) {
                Ok(c) => c,
                Err(e) => {
                    if let model::Screen::OAuthDeviceCode(ref mut s) = app.screen {
                        s.error = Some(format!("Failed to load config: {e}"));
                        // 阻止主循环立即重试（同登录失败分支）
                        s.polling = true;
                    }
                    continue;
                }
            };
            session.suspend()?;
            let result = crate::auth::login_provider(
                &app.config_path,
                &cfg,
                &crate::auth::state_path_for_config(&app.config_path),
                &provider_id,
            )
            .await;
            session.resume()?;
            match result {
                Ok(()) => {
                    app.screen =
                        model::Screen::ProductSelection(make_product_state(&app.config_path));
                    continue;
                }
                Err(e) => {
                    log_oauth_tui_failure("openai_device_login", &provider_id, &e);
                    if let model::Screen::OAuthDeviceCode(ref mut s) = app.screen {
                        s.error = Some(format!("Login failed: {e}"));
                        // 置 polling 阻止主循环立即重试（否则会 suspend/resume 死循环）；
                        // 用户按 [r/Enter] 清错后才会重新触发登录
                        s.polling = true;
                    }
                    continue;
                }
            }
        }

        // Block on next key event
        if event::poll(Duration::from_millis(50))?
            && let Ok(event) = event::read()
        {
            match event {
                Event::Key(key) => match global_control_key(key) {
                    Some(GlobalControlKey::Suspend) => {
                        session.suspend_for_resume()?;
                        terminal.clear()?;
                    }
                    Some(GlobalControlKey::Ignored) => {}
                    _ => update::handle_key(&mut app, key),
                },
                Event::Resize(width, height) => {
                    app.width = width;
                    app.height = height;
                    if let model::Screen::ModelManagement(ref mut state) = app.screen {
                        let viewport = model::model_list_viewport(height)
                            .saturating_sub(state.error_row_count());
                        state.ensure_cursor_visible(viewport);
                    }
                    if let model::Screen::ProviderManagement(ref mut state) = app.screen {
                        let viewport = model::provider_list_viewport(height)
                            .saturating_sub(state.error_row_count());
                        state.ensure_cursor_visible(viewport);
                    }
                    if let model::Screen::ModelDetail(ref mut state) = app.screen {
                        let viewport =
                            model::detail_viewport(height).saturating_sub(state.error_row_count());
                        let cursor_line = state.cursor_line_index();
                        model::follow_scroll(&mut state.scroll_offset, cursor_line, viewport);
                    }
                    if let model::Screen::ProtocolBindingEditor(ref mut state) = app.screen {
                        let viewport =
                            model::detail_viewport(height).saturating_sub(state.error_row_count());
                        let cursor_line = state.active_cursor_line();
                        model::follow_scroll(&mut state.scroll_offset, cursor_line, viewport);
                    }
                    if let model::Screen::ProviderDetail(ref mut state) = app.screen {
                        update::provider_mgmt::sync_provider_detail_scroll(state, height);
                    }
                    if let model::Screen::FallbackConfig(ref mut state) = app.screen {
                        let viewport = height.saturating_sub(8) as usize;
                        let cursor_line = state.active_cursor_line();
                        model::follow_scroll(&mut state.scroll_offset, cursor_line, viewport);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Post-process config to remove the newly-created provider's bindings for
/// unselected endpoints.
///
/// After `write_provider_config` creates a provider with catalog defaults (all
/// protocol endpoints), this function removes only the new provider's bindings
/// for protocols the user did not select in FallbackConfig.  Bindings belonging
/// to other providers are preserved.
fn apply_endpoint_filter(
    config_path: &Path,
    provider_name: &str,
    filter: &std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> Result<()> {
    use crate::config::Protocol;

    let mut cfg = crate::config::Config::load(config_path)?;

    for (model_id, allowed_endpoints) in filter {
        let Some(model_config) = cfg.models.get_mut(model_id) else {
            continue;
        };
        for protocol in Protocol::CLIENT_PROTOCOLS {
            let label = match protocol {
                Protocol::OpenaiChatCompletions => "chat",
                Protocol::OpenaiResponses => "responses",
                Protocol::Anthropic => "anthropic",
            };
            if !allowed_endpoints.contains(label) {
                let bindings = match protocol {
                    Protocol::OpenaiChatCompletions => &mut model_config.openai_chat_providers,
                    Protocol::OpenaiResponses => &mut model_config.openai_responses_providers,
                    Protocol::Anthropic => &mut model_config.anthropic_providers,
                };
                bindings.retain(|b| b.name != provider_name);
            }
        }
    }

    let serialized = crate::config_edit::canonical_format(&cfg)?;
    crate::config_edit::atomic_write(config_path, serialized.as_bytes())?;
    Ok(())
}

/// Write provider config by calling the connect module.
/// Routes via `with_cli_write_lock_or_delegate` to server delegation or local atomic write.
/// When server is detected, failures are NOT fallen back to local write (No-Fallback Invariant).
///
/// `product_id` — catalog product ID for catalog lookup (e.g. `"deepseek"`).
/// `provider_name` — key written into config `[providers.<name>]` (e.g. `"deepseek-2"`).
#[allow(clippy::too_many_arguments)]
async fn write_provider_config(
    config_path: &Path,
    product_id: Option<&str>,
    env_var: Option<&str>,
    models: &[String],
    custom_type: Option<&str>,
    custom_endpoint: Option<&str>,
    provider_name: Option<&str>,
    skip_probe: bool,
    pending_account: Option<&crate::auth::PendingAccount>,
    progress_tx: Option<crate::connect::ProbeProgressSender>,
) -> Result<Vec<ConfigResult>> {
    let product_id = match product_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            // Custom provider without a product — need to add via custom flow
            let provider_name = custom_type.unwrap_or("custom");
            anyhow::bail!("Custom provider not yet fully supported: {}", provider_name);
        }
    };

    let model_list: Vec<String> = models.to_vec();
    let models_count = model_list.len();
    let models_opt = if model_list.is_empty() {
        None
    } else {
        Some(model_list.as_slice())
    };

    let no_api_key = env_var.is_none() || env_var == Some("");
    let api_key_env = env_var.filter(|v| !v.is_empty()).map(|v| v.to_string());

    crate::connect::connect_provider(
        config_path,
        product_id,
        api_key_env,
        no_api_key,
        custom_type.map(|t| t.to_string()),
        custom_endpoint.map(|u| u.to_string()),
        models_opt,
        provider_name,
        skip_probe,
        pending_account,
        progress_tx,
    )
    .await?;

    let display_name = provider_name.unwrap_or(product_id);
    Ok(vec![ConfigResult {
        provider: display_name.to_string(),
        success: true,
        message: format!("Configured ({} models)", models_count),
    }])
}
