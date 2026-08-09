mod admin;
mod admin_client;
mod auth;
mod cache;
mod catalog;
mod cli;
mod clients;
mod codex;
mod config;
mod config_edit;
mod connect;
mod convert;
mod cooldown;
mod core;
mod doc;
mod fallback;
mod json_edit;
mod model;
mod ownership;
mod probe;
mod probe_coordinator;
mod protection;
mod proxy;
mod quota;
mod service;
mod status;
mod tui;
mod tui_usage;
mod usage;
mod usage_stats;
mod version;

use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;

use cli::*;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(config::default_config_path);

    match cli.command.unwrap_or(Command::Serve { frontend: false }) {
        Command::Init => {
            config::init_config(&config_path)?;
        }
        Command::Status { probe } => {
            let cfg = config::Config::load(&config_path)?;
            status::print_status(&config_path, &cfg, probe).await?;
        }
        Command::Connect(args) => {
            run_connect(&config_path, args).await?;
        }
        Command::Provider { select, command } => {
            run_provider_command(&config_path, select, command).await?;
        }
        Command::Model { command } => {
            run_model_command(&config_path, command).await?;
        }
        Command::Cooldown { command } => match command {
            CooldownCommand::List => cooldown::print_list(&cooldown::default_state_path())?,
            CooldownCommand::Clear { model, provider } => {
                // §15.2 5 步流程：委托优先 → 无 server try_lock → 本地写
                crate::ownership::with_cli_write_lock_or_delegate(
                    &config_path,
                    "llm-proxy cooldown clear",
                    || {
                        let model = model.clone();
                        let provider = provider.clone();
                        async move {
                            let removed = cooldown::clear(
                                &cooldown::default_state_path(),
                                model.as_deref(),
                                &provider,
                            )?;
                            if removed == 0 {
                                println!("No cooldown found for provider={provider}");
                            } else if let Some(model) = model {
                                println!("Cleared cooldown for model={model} provider={provider}");
                            } else {
                                println!("Cleared {removed} cooldown(s) for provider={provider}");
                            }
                            Ok(())
                        }
                    },
                    |server| {
                        let model = model.clone();
                        let provider = provider.clone();
                        Box::pin(async move {
                            let result = server.cooldown_clear(model.as_deref(), &provider).await?;
                            let removed = result
                                .get("data")
                                .and_then(|d| d.get("removed"))
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            if removed == 0 {
                                println!("No cooldown found for provider={provider}");
                            } else if let Some(model) = model {
                                println!("Cleared cooldown for model={model} provider={provider}");
                            } else {
                                println!("Cleared {removed} cooldown(s) for provider={provider}");
                            }
                            Ok(())
                        })
                    },
                )
                .await?;
            }
        },
        Command::Serve { frontend } => {
            let cfg = config::Config::load(&config_path)?;
            if frontend {
                service::run_foreground(cfg, &config_path).await?;
            } else {
                service::start_background(&config_path, &cfg)?;
            }
        }
        Command::Shutdown => {
            service::shutdown_background(&config_path)?;
        }
        Command::Restart => {
            let cfg = config::Config::load(&config_path)?;
            service::restart_background(&config_path, &cfg)?;
        }
        Command::Doc { list, raw, section } => {
            doc::print_doc(list, raw, section)?;
        }
        Command::Version => {
            version::print_version();
        }
        Command::Completion { shell } => {
            print!("{}", generate_completion_script(shell));
        }
        Command::CompleteCandidates { kind } => {
            print!("{}", complete_candidates(&config_path, kind).await);
        }
        Command::Launch { command } => match command {
            LaunchCommand::Codex {
                codex_home,
                dry_run,
            } => {
                let cfg = launch_config(&config_path, "codex").await?;
                codex::launch_codex(&cfg, codex_home, dry_run)?;
            }
            LaunchCommand::CodexDesktop {
                codex_home,
                dry_run,
            } => {
                let cfg = launch_config(&config_path, "codex-desktop").await?;
                codex::launch_codex(&cfg, codex_home, dry_run)?;
            }
            LaunchCommand::Pi { pi_home, dry_run } => {
                let cfg = launch_config(&config_path, "pi").await?;
                clients::launch_pi(&cfg, pi_home, dry_run)?;
            }
            LaunchCommand::QwenCode {
                model_id,
                qwen_home,
                dry_run,
            } => {
                let cfg = launch_config(&config_path, "qwen-code").await?;
                clients::launch_qwen_code(&cfg, model_id, qwen_home, dry_run)?;
            }
            LaunchCommand::ClaudeCode {
                model_id,
                claude_home,
                dry_run,
            } => {
                let cfg = launch_config(&config_path, "claude-code").await?;
                clients::launch_claude_code(&cfg, model_id, claude_home, dry_run)?;
            }
            LaunchCommand::ClaudeDesktop {
                claude_desktop_home,
                profile,
                dry_run,
            } => {
                let cfg = launch_config(&config_path, "claude-desktop").await?;
                clients::launch_claude_desktop(&cfg, claude_desktop_home, profile, dry_run)?;
            }
        },
        Command::Usage(args) => {
            // No arguments + TTY → TUI mode; otherwise → CLI mode (plain text)
            let has_filters = args.period.is_some()
                || args.provider.is_some()
                || args.model.is_some()
                || args.endpoint.is_some()
                || args.json;
            let is_tty = std::io::stdout().is_terminal();
            if !has_filters && is_tty {
                tui_usage::run().await?;
            } else {
                run_usage(&config_path, args).await?;
            }
        }
        Command::Quota(args) => {
            run_quota(&config_path, args).await?;
        }
    }

    Ok(())
}
