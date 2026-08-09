mod style;

pub mod fuzzy;
pub mod model;
pub mod update;
pub mod view;
pub mod widgets;

use self::update::products::make_product_state;

use std::io;
use std::path::Path;

use anyhow::Result;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use self::model::{ConfigResult, EntryMode};

/// 同步上下文（TUI handle_key）中执行异步委托操作（C1 根治：
/// server 运行时 TUI 写配置委托 server，保持单一写者）。
pub fn run_async_sync<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

/// Main TUI entry point. Called from `connect` or `provider` CLI commands.
pub async fn run(config_path: &std::path::Path, entry: EntryMode) -> Result<()> {
    let config_path = config_path.to_path_buf();

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Initialize state
    let mut app = model::AppModel::new(config_path, entry);

    // Main loop
    let res = loop {
        // Render
        terminal.draw(|f| view::render(f, &app))?;

        // Check quit
        if matches!(app.screen, model::Screen::Quit) {
            break Ok(());
        }

        // Handle Verifying state — perform async write
        if let model::Screen::Verifying(ref v) = app.screen {
            let force_local = v.force_local;
            let product_id = app.chosen_product.as_ref().map(|p| p.id.clone());
            // Prefer user-chosen name from naming screen; fall back to product ID
            let provider_name = app
                .oauth_provider_name
                .clone()
                .or_else(|| app.chosen_product.as_ref().map(|p| p.id.clone()));
            let env_var = app.chosen_env_var.clone();
            let models = app.chosen_models.clone();
            let custom_type = app.custom_provider_type.clone();
            let custom_endpoint = app.custom_provider_endpoint.clone();
            let config_path = app.config_path.clone();

            let result = if force_local {
                // Force local write: skip server delegation, only use local flock
                write_provider_config_local(
                    &config_path,
                    product_id.as_deref(),
                    env_var.as_deref(),
                    &models,
                    custom_type.as_deref(),
                    custom_endpoint.as_deref(),
                    provider_name.as_deref(),
                )
                .await
            } else {
                // Normal flow: try server delegation first, fallback to local
                write_provider_config(
                    &config_path,
                    product_id.as_deref(),
                    env_var.as_deref(),
                    &models,
                    custom_type.as_deref(),
                    custom_endpoint.as_deref(),
                    provider_name.as_deref(),
                )
                .await
            };

            match result {
                Ok(results) => {
                    app.session_results = results.clone();
                    app.screen = model::Screen::Done(model::DoneState { results });
                }
                Err(e) => {
                    app.screen = model::Screen::WarningConfirm(model::WarningConfirmState {
                        message: e.to_string(),
                        selected_option: model::WarningOption::Continue,
                        back_to_product: false,
                        error: None,
                        force_local,
                    });
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
                    }
                    continue;
                }
            };
            // Restore terminal for CLI-style login (uses stdin/stdout)
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
            let result = crate::auth::login_provider(
                &app.config_path,
                &cfg,
                &crate::auth::default_state_path(),
                &provider_id,
            )
            .await;
            // Re-enter TUI mode
            enable_raw_mode()?;
            execute!(terminal.backend_mut(), EnterAlternateScreen)?;
            match result {
                Ok(()) => {
                    app.screen = model::Screen::ProductSelection(make_product_state());
                    continue;
                }
                Err(e) => {
                    if let model::Screen::OAuthDeviceCode(ref mut s) = app.screen {
                        s.error = Some(format!("Login failed: {e}"));
                    }
                    continue;
                }
            }
        }

        // Antigravity login：自包含流程——用户在 TUI 内粘贴 code 后按 Enter
        // （submitted=true），直接用进入屏幕时保存的 PKCE verifier 完成
        // OAuth exchange + 写账号，不退出 raw mode，不调用 CLI 交互式 login_provider。
        if let model::Screen::AntigravityLogin(ref state) = app.screen
            && state.submitted
            && !state.input.is_empty()
            && !state.verifier.is_empty()
            && state.error.is_none()
        {
            let provider_id = state.provider_name.clone();
            let code = state.input.clone();
            let verifier = state.verifier.clone();
            let cfg = match crate::config::Config::load(&app.config_path) {
                Ok(c) => c,
                Err(e) => {
                    if let model::Screen::AntigravityLogin(ref mut s) = app.screen {
                        s.error = Some(format!("Failed to load config: {e}"));
                        s.submitted = false;
                    }
                    continue;
                }
            };
            let result = crate::auth::login_antigravity_with_code(
                &app.config_path,
                &cfg,
                &crate::auth::default_state_path(),
                &provider_id,
                &verifier,
                &code,
            )
            .await;
            match result {
                Ok(()) => {
                    app.screen = model::Screen::ProductSelection(make_product_state());
                    continue;
                }
                Err(e) => {
                    if let model::Screen::AntigravityLogin(ref mut s) = app.screen {
                        s.error = Some(format!("Login failed: {e}"));
                        s.input.clear();
                        s.submitted = false;
                    }
                    continue;
                }
            }
        }

        // Block on next key event
        if let Ok(Event::Key(key)) = event::read() {
            update::handle_key(&mut app, key);
        }
    };

    // Cleanup
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    res
}

/// Write provider config by calling the connect module.
/// Uses `with_cli_write_lock_or_delegate` for proper server delegation with retry.
///
/// `product_id` — catalog product ID for catalog lookup (e.g. `"deepseek"`).
/// `provider_name` — key written into config `[providers.<name>]` (e.g. `"deepseek-2"`).
async fn write_provider_config(
    config_path: &Path,
    product_id: Option<&str>,
    env_var: Option<&str>,
    models: &[String],
    custom_type: Option<&str>,
    custom_endpoint: Option<&str>,
    provider_name: Option<&str>,
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

    let api_key_env: Option<&str> = match env_var {
        Some(v) if !v.is_empty() => Some(v),
        _ => None,
    };
    let no_api_key = env_var.is_none() || env_var == Some("");
    let config_path = config_path.to_path_buf();
    let product_id_owned = product_id.to_string();
    let env_var_owned = env_var.map(|s| s.to_string());
    let api_key_env_owned = api_key_env.map(|s| s.to_string());
    let custom_type_owned = custom_type.map(|s| s.to_string());
    let custom_endpoint_owned = custom_endpoint.map(|s| s.to_string());
    let provider_name_owned = provider_name.map(|s| s.to_string());

    // Use the full 5-step delegation flow (detect → delegate | lock → write | retry on HeldByServer)
    crate::ownership::with_cli_write_lock_or_delegate(
        &config_path,
        "llm-proxy tui provider add",
        // Local write: no server, acquire lock and write directly
        || {
            let config_path = config_path.clone();
            let product_id = product_id_owned.clone();
            let env_var = env_var_owned.clone();
            let custom_type = custom_type_owned.clone();
            let custom_endpoint = custom_endpoint_owned.clone();
            let model_list = model_list.clone();
            let provider_name = provider_name_owned.clone();
            async move {
                let models_opt = if model_list.is_empty() {
                    None
                } else {
                    Some(model_list.as_slice())
                };
                crate::connect::add_provider_with_models(
                    &config_path,
                    &product_id,
                    env_var.filter(|v| !v.is_empty()).map(|v| v.to_string()),
                    no_api_key,
                    custom_type,
                    custom_endpoint,
                    models_opt,
                    provider_name.as_deref(),
                )
                .await
            }
        },
        // Delegate: server running, send via UDS
        |server| {
            let product_id = product_id_owned.clone();
            let env_var = api_key_env_owned.clone();
            let custom_type = custom_type_owned.clone();
            let custom_endpoint = custom_endpoint_owned.clone();
            let model_list = model_list.clone();
            let provider_name = provider_name_owned.clone();
            Box::pin(async move {
                let models_opt = if model_list.is_empty() {
                    None
                } else {
                    Some(model_list.as_slice())
                };
                // Server-side uses product_id as provider_id for catalog lookup;
                // the user-chosen provider_name is not yet propagated to the admin
                // endpoint, so fall back to product_id for the server path.
                // TODO: propagate provider_name to the server admin endpoint
                let server_id = provider_name.as_deref().unwrap_or(&product_id);
                server
                    .add_provider(
                        server_id,
                        env_var.as_deref(),
                        no_api_key,
                        custom_type.as_deref(),
                        custom_endpoint.as_deref(),
                        models_opt,
                    )
                    .await
                    .map(|_| ())
            })
        },
    )
    .await?;

    let display_name = provider_name.unwrap_or(product_id);
    Ok(vec![ConfigResult {
        provider: display_name.to_string(),
        success: true,
        message: format!("Configured ({} models)", models_count),
    }])
}

/// Write provider config locally only (skip server delegation).
/// Used when user selects "Continue anyway" after delegation failure.
async fn write_provider_config_local(
    config_path: &Path,
    product_id: Option<&str>,
    env_var: Option<&str>,
    models: &[String],
    custom_type: Option<&str>,
    custom_endpoint: Option<&str>,
    provider_name: Option<&str>,
) -> Result<Vec<ConfigResult>> {
    let product_id = match product_id {
        Some(id) if !id.is_empty() => id,
        _ => {
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

    // Only local write, no server delegation
    crate::ownership::with_cli_write_lock_async(
        "llm-proxy tui provider add (force local)",
        crate::connect::add_provider_with_models(
            config_path,
            product_id,
            env_var.filter(|v| !v.is_empty()).map(|v| v.to_string()),
            env_var.is_none() || env_var == Some(""),
            custom_type.map(|t| t.to_string()),
            custom_endpoint.map(|u| u.to_string()),
            models_opt,
            provider_name,
        ),
    )
    .await?;

    let display_name = provider_name.unwrap_or(product_id);
    Ok(vec![ConfigResult {
        provider: display_name.to_string(),
        success: true,
        message: format!("Configured locally ({} models)", models_count),
    }])
}
