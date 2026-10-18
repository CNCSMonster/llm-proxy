mod admin;
mod admin_client;
mod auth;
mod cache;
mod catalog;
mod cli;
mod client_view;
mod clients;
mod codex;
mod config;
mod config_edit;
mod config_render;
mod connect;
mod convert;
mod cooldown;
mod core;
mod diagnostics;
mod doc;
mod fallback;
mod http_client;
mod json_edit;
mod model;
mod ownership;
mod pattern;
mod preflight;
mod probe;
mod probe_coordinator;
mod protection;
mod provider_metrics;
mod provider_probe;
mod proxy;
mod runtime_config;
mod service;
mod status;
mod streaming_metrics;
mod terminal_session;
mod token;
mod tui;
mod tui_control;
mod tui_doc;
mod tui_usage;
mod usage;
mod usage_stats;
mod version;

use std::fs::OpenOptions;
use std::io::IsTerminal;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result};
use clap::Parser;

use cli::*;

/// panic backtrace 的日志上限（字节）。仅诊断开关下输出，见 [`panic_debug_enabled`]。
const PANIC_BACKTRACE_MAX_BYTES: usize = 4000;

/// `RUST_LOG` 未设置时的缺省 filter。
///
/// 为什么要按 target 分两级：噪音实测 100% 来自第三方——`status --probe` 在
/// `RUST_LOG=trace` 下产出 206 行，`hyper_util::client::legacy::pool` 等占满，
/// `llm_proxy::*` 占 **0 行**；而我方诊断埋点（如 `connect_probe_batch_start` /
/// `connect_probe_result`）是 `info!`，缺省 `ERROR` 会把它们一并滤掉，排查全靠猜。
///
/// 于是把「噪声明星」和「诊断刚需」拆开：第三方仍停在 `ERROR`（保住
/// 「不无差别抬默认」的取舍），只给自己的 `llm_proxy::*` 开到 `info`。副作用受控——
/// 我方 `info!` 在请求热路径（`src/proxy/request.rs`）为 0 处，不会每请求刷屏。
///
/// 显式设置 `RUST_LOG` 时完全以环境变量为准，行为与改前一致。
///
/// ⚠️ **target 匹配是纯字符串前缀**（`tracing-subscriber` 的 `directive.rs` 用
/// `meta.target().starts_with(target)`，非路径感知）：`llm_proxy=info` 因此也会匹配
/// 未来若出现的 `llm_proxy_something::…`。当前依赖树中无此类 target（全仓 30+ 个
/// 均以 `llm_proxy::` 开头），但引入同前缀依赖时需重新评估本行。
const DEFAULT_ENV_FILTER: &str = "error,llm_proxy=info";

/// 本次运行 tracing filter 的**纯函数**形态：给定 `RUST_LOG` 的取值产出 filter，
/// **不读环境变量**。
///
/// 环境读取只在 [`default_env_filter`] 发生一处，本函数让门禁测试能在**任何**
/// 运行环境下断言语义——原先直接调 `default_env_filter()`，在 `RUST_LOG=info/trace`
/// 下跑 `cargo test` 会失败。
fn default_env_filter_from(rust_log: Option<&std::ffi::OsStr>) -> tracing_subscriber::EnvFilter {
    match rust_log {
        // `EnvFilter::from_default_env()` 的展开形式，确保「已设 `RUST_LOG`」这一支
        // 与改前逐字节等价（含空串 → 补 ERROR 缺省指令的语义）。
        Some(value) => tracing_subscriber::EnvFilter::builder()
            .with_default_directive(tracing_subscriber::filter::LevelFilter::ERROR.into())
            .parse_lossy(value.to_string_lossy()),
        None => tracing_subscriber::EnvFilter::new(DEFAULT_ENV_FILTER),
    }
}

/// 解析本次运行的 tracing filter：`RUST_LOG` 已设则完全听环境变量，未设则用
/// [`DEFAULT_ENV_FILTER`]。两个日志初始化分支（TUI 落文件 / 非 TUI 落 stderr）共用。
fn default_env_filter() -> tracing_subscriber::EnvFilter {
    default_env_filter_from(std::env::var_os("RUST_LOG").as_deref())
}

/// 把 payload 折成**单行**：控制字符（换行、`\r`、`ESC`、NUL…）一律换成空格。
///
/// 这一步不是美化。payload 可能带上游或用户可控的文本，换行能把一条记录伪造成两条
/// `event="…"` 日志；`ESC` 会把 tracing 的 ANSI 转义打乱，事后 grep `event="panic"`
/// 反而匹配不到（E2E 里就栽过这个假阴性）。
fn collapse_control_chars(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// 从 panic payload 取出**可以安全写进日志**的摘要。
///
/// 只认 `&str` / `String` 两种（`panic!` 与 `assert!` 系列都是这两种）。自定义 panic
/// 类型不能硬转 `Debug`——那等于把一个未知结构体整个灌进日志——只给一个占位标记。
/// 稳定版 std 也不提供 `dyn Any::type_name()`，所以这里连类型名都给不出：日志侧仍靠
/// `file`/`line`/`thread` 定位。
fn payload_summary(payload: &(dyn std::any::Any + Send + 'static)) -> String {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .map(panic_payload_summary)
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

/// 文本 payload ⇒ 脱敏摘要。脱敏与截断复用 OAuth 错误摘要那条**已审过的**流水线
/// （键名形态 + 裸 token 形态 + 300 字符截断），避免仓里出现第二份会漂移的脱敏规则。
fn panic_payload_summary(raw: &str) -> String {
    auth::oauth_error_log_summary(&collapse_control_chars(raw))
}

/// 诊断开关：`LLM_PROXY_PANIC_DEBUG=1` 时额外输出一条 backtrace。
///
/// backtrace 里是符号名、地址与本机路径，**不承诺脱敏**，所以默认关闭；只在本地排障时
/// 打开，且取得日志后不要直接上传或提交（见 `docs/user_guide/troubleshooting.md`）。
fn panic_debug_enabled() -> bool {
    std::env::var("LLM_PROXY_PANIC_DEBUG").is_ok_and(|value| value == "1")
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic| {
        let location = panic.location();
        let thread = std::thread::current();
        tracing::error!(
            target: "llm_proxy::service",
            event = "panic",
            thread = thread.name().unwrap_or("unnamed"),
            file = location.map(|location| location.file()).unwrap_or("unknown"),
            line = location.map(|location| location.line()).unwrap_or(0),
            payload = %payload_summary(panic.payload()),
            "llm-proxy encountered an unrecoverable error"
        );
        if panic_debug_enabled() {
            let backtrace = std::backtrace::Backtrace::force_capture();
            let one_line = collapse_control_chars(&backtrace.to_string());
            tracing::error!(
                target: "llm_proxy::service",
                event = "panic_backtrace",
                backtrace = %diagnostics::bounded_message(
                    &auth::redact_credential_shapes(&one_line),
                    PANIC_BACKTRACE_MAX_BYTES
                ),
                "panic backtrace enabled by LLM_PROXY_PANIC_DEBUG"
            );
        }
    }));
}

/// Check model list consistency between local config and remote server.
/// Fetches the remote model list and compares it with the local config.
async fn check_model_list_consistency(cfg: &config::Config) -> Result<()> {
    use serde_json::Value;

    cfg.validate()?;

    // Get the base URL (remote or local)
    let base_url = config::protocol_base_url(cfg, "");
    let model_list_url = format!("{}/admin/model/list", base_url.trim_end_matches('/'));

    println!("Checking model list consistency...");
    println!("Server URL: {base_url}");
    println!("Fetching model list from: {model_list_url}");

    // Build HTTP client with optional auth
    let client = reqwest::Client::new();
    let mut request = client.get(&model_list_url);

    // Add auth token if in remote token mode
    if let Some(auth_mode) = cfg.effective_remote_auth_mode()
        && matches!(auth_mode, config::RemoteAuthMode::Token)
        && let Some(token) = cfg.resolve_remote_auth_token()
    {
        request = request.bearer_auth(token);
    }

    let response = request
        .send()
        .await
        .context("failed to fetch model list from server")?;

    if !response.status().is_success() {
        anyhow::bail!("server returned error status: {}", response.status());
    }

    let remote_models: Value = response
        .json()
        .await
        .context("failed to parse model list response")?;

    // Extract model IDs from remote response
    let remote_model_ids: Vec<String> = remote_models
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
        .collect();

    // Get local model IDs from config
    let local_model_ids: Vec<String> = cfg.models.keys().cloned().collect();

    // Compare
    let remote_set: std::collections::HashSet<_> = remote_model_ids.iter().cloned().collect();
    let local_set: std::collections::HashSet<_> = local_model_ids.iter().cloned().collect();

    let new_models: Vec<_> = remote_set.difference(&local_set).collect();
    let removed_models: Vec<_> = local_set.difference(&remote_set).collect();

    println!("\nModel list comparison:");
    println!("  Local models: {}", local_model_ids.len());
    println!("  Remote models: {}", remote_model_ids.len());

    if new_models.is_empty() && removed_models.is_empty() {
        println!("\n✓ Model lists are consistent");
    } else {
        if !new_models.is_empty() {
            println!("\n⚠ New models on remote server ({}):", new_models.len());
            for model in new_models {
                println!("  + {model}");
            }
        }
        if !removed_models.is_empty() {
            println!(
                "\n⚠ Models removed from remote server ({}):",
                removed_models.len()
            );
            for model in removed_models {
                println!("  - {model}");
            }
        }
        println!("\nTo update local config, re-run: llm-proxy launch <client>");
    }

    Ok(())
}

// jemalloc：大块(≥128KB)分配不走 glibc 逐次 mmap/munmap，extent 复用暖页，
// 消除大 body 场景的缺页/TLB 抖动（实测 1MB 请求体中转开销 -33%）。
// Windows 无 jemalloc 支持，继续使用系统分配器。
#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Err(error) = validate_bare_command_terminal(
        cli.command.as_ref(),
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    ) {
        error.exit();
    }

    let tui_mode = matches!(
        cli.command.as_ref(),
        Some(Command::Connect(args)) if args.product.is_none()
    ) || matches!(
        cli.command.as_ref(),
        Some(Command::Provider {
            select: None,
            command: None
        })
    );
    if tui_mode {
        let tui_config_dir = cli
            .config_dir
            .clone()
            .unwrap_or_else(config::default_config_dir);
        let dirs = service::StateDirs::resolve(&tui_config_dir);
        let log_path = dirs.state_dir.join("llm-proxy-cli.log");
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log directory {}", parent.display()))?;
        }
        let mut log_options = OpenOptions::new();
        log_options.create(true).append(true);
        #[cfg(unix)]
        log_options.mode(0o600);
        let log = log_options
            .open(&log_path)
            .with_context(|| format!("failed to open log {}", log_path.display()))?;
        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            log.set_permissions(Permissions::from_mode(0o600))
                .with_context(|| format!("failed to secure log {}", log_path.display()))?;
        }
        tracing_subscriber::fmt()
            .with_env_filter(default_env_filter())
            .with_writer(log)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(default_env_filter())
            .init();
    }
    install_panic_hook();

    let config_dir = cli.config_dir.unwrap_or_else(config::default_config_dir);
    let config_path = config_dir.join("config.toml");

    // Top-level --dry-run: equivalent to `serve --dry-run`.
    // Reject --foreground at top level (was a conflicts_with on Serve).
    if cli.dry_run {
        if let Some(Command::Serve { frontend: true }) = cli.command.as_ref() {
            anyhow::bail!("the argument '--dry-run' cannot be used with '--foreground'");
        }
        let mut output = dry_run_check(&config_path)?;
        output.push_str(&preflight::runtime_preflight(&config_dir)?);
        println!("{output}");
        return Ok(());
    }

    match cli.command.unwrap_or(Command::Serve { frontend: false }) {
        Command::Init => {
            config::init_config(&config_path)?;
        }
        Command::Status {
            probe,
            provider,
            model,
        } => {
            let cfg = config::Config::load(&config_path)?;
            let selection = status::ProbeSelection { provider, model };
            status::print_status(&config_path, &cfg, probe, &selection).await?;
        }
        Command::Connect(args) => {
            run_connect(&config_path, args).await?;
        }
        Command::Provider { select, command } => {
            run_provider_command(&config_path, select, command).await?;
        }
        Command::Model { select, command } => {
            run_model_command(&config_path, select, command).await?;
        }
        Command::Cooldown { command } => {
            crate::cli::run_cooldown_command(&config_path, command).await?;
        }
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
            if !list && !raw && section.is_none() && std::io::stdout().is_terminal() {
                tui_doc::run()?;
            } else {
                doc::print_doc(list, raw, section)?;
            }
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
                codex_config_dir,
                codex_home: _,
                dry_run,
                check,
                filter,
            } => {
                let cfg = launch_config(&config_path, "codex", &filter).await?;
                if check {
                    check_model_list_consistency(&cfg).await?;
                } else {
                    // 传入 config_dir 以支持本地 tokens.toml 凭证注入
                    let config_dir = config_path.parent();
                    codex::launch_codex(&cfg, codex_config_dir, dry_run, config_dir)?;
                }
            }
            LaunchCommand::CodexDesktop {
                codex_config_dir,
                codex_home: _,
                dry_run,
                check,
                filter,
            } => {
                let cfg = launch_config(&config_path, "codex-desktop", &filter).await?;
                if check {
                    check_model_list_consistency(&cfg).await?;
                } else {
                    // 传入 config_dir 以支持本地 tokens.toml 凭证注入
                    let config_dir = config_path.parent();
                    codex::launch_codex(&cfg, codex_config_dir, dry_run, config_dir)?;
                }
            }
            LaunchCommand::Pi {
                pi_config_dir,
                pi_home: _,
                dry_run,
                check,
                filter,
            } => {
                let cfg = launch_config(&config_path, "pi", &filter).await?;
                if check {
                    check_model_list_consistency(&cfg).await?;
                } else {
                    clients::launch_pi(&cfg, pi_config_dir, dry_run)?;
                }
            }
            LaunchCommand::QwenCode {
                qwen_config_dir,
                qwen_home: _,
                dry_run,
                check,
                filter,
            } => {
                let cfg = launch_config(&config_path, "qwen-code", &filter).await?;
                if check {
                    check_model_list_consistency(&cfg).await?;
                } else {
                    clients::launch_qwen_code(&cfg, qwen_config_dir, dry_run)?;
                }
            }
            LaunchCommand::ClaudeCode {
                claude_config_dir,
                claude_home: _,
                dry_run,
                check,
                filter,
            } => {
                let cfg = launch_config(&config_path, "claude-code", &filter).await?;
                if check {
                    check_model_list_consistency(&cfg).await?;
                } else {
                    clients::launch_claude_code(&cfg, claude_config_dir, dry_run)?;
                }
            }
            LaunchCommand::ClaudeDesktop {
                claude_desktop_config_dir,
                claude_desktop_home: _,
                profile,
                dry_run,
                check,
                filter,
            } => {
                let cfg = launch_config(&config_path, "claude-desktop", &filter).await?;
                if check {
                    check_model_list_consistency(&cfg).await?;
                } else {
                    clients::launch_claude_desktop(
                        &cfg,
                        claude_desktop_config_dir,
                        profile,
                        dry_run,
                    )?;
                }
            }
            LaunchCommand::OpenCode {
                opencode_config_dir,
                opencode_home: _,
                dry_run,
                check,
                filter,
            } => {
                let cfg = launch_config(&config_path, "opencode", &filter).await?;
                if check {
                    check_model_list_consistency(&cfg).await?;
                } else {
                    clients::launch_opencode(&cfg, opencode_config_dir, dry_run)?;
                }
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
        Command::Token { command } => {
            run_token_command(&config_path, command).await?;
        }
    }

    Ok(())
}

/// `serve --dry-run`: load and statically validate the
/// configuration, returning a desensitized summary. Shares Config::load's
/// parse/migrate/validate core (including retired-key rejection); performs no
/// listeners, state writes, snapshots, probes, or network access.
fn dry_run_check(config_path: &std::path::Path) -> Result<String> {
    let cfg = config::Config::load(config_path)?;
    let listen = cfg
        .server
        .as_ref()
        .map(|s| s.listen.as_str())
        .unwrap_or("not configured");
    Ok(format!(
        "configuration OK: {} provider(s), {} model(s), listen {}",
        cfg.providers.len(),
        cfg.models.len(),
        listen
    ))
}

#[cfg(test)]
mod dry_run_tests {
    use super::*;

    #[test]
    fn dry_run_check_reports_counts_without_side_effects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[server]
listen = "127.0.0.1:8989"

[providers.provider-a]
api_key_env = "PROVIDER_A_API_KEY"
[providers.provider-a.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.model-a]
context_window = 200000
max_output_tokens = 8192
openai_chat_providers = [{ name = "provider-a", model = "upstream-a" }]
"#,
        )
        .expect("write config");
        let summary = dry_run_check(&path).expect("valid config passes");
        assert!(summary.contains("1 provider(s)"), "{summary}");
        assert!(summary.contains("1 model(s)"), "{summary}");
        // Zero side effects: nothing but the config file exists.
        let entries: Vec<_> = std::fs::read_dir(temp.path())
            .expect("read dir")
            .collect::<Result<_, _>>()
            .expect("entries");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn dry_run_check_rejects_retired_enable_thinking_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[server]
listen = "127.0.0.1:8989"

[models.model-a]
context_window = 1
max_output_tokens = 1
enable_thinking = false
"#,
        )
        .expect("write config");
        let err = dry_run_check(&path).expect_err("retired key rejected");
        // Config::load wraps parse errors in path context; the migration
        // guidance lives in the cause chain (anyhow `{:#}` / main's Debug
        // report both surface it).
        assert!(
            format!("{err:#}").contains("enable_thinking is retired"),
            "{err:#}"
        );
        // Rejection must not rewrite the config.
        let text = std::fs::read_to_string(&path).expect("read config");
        assert!(text.contains("enable_thinking = false"));
    }

    #[test]
    fn dry_run_check_rejects_strategy_contradictions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[server]
listen = "127.0.0.1:8989"

[models.model-a]
context_window = 1
max_output_tokens = 1
default_reasoning_level = "low"
force_think = true
force_no_think = true
"#,
        )
        .expect("write config");
        let err = dry_run_check(&path).expect_err("contradiction rejected");
        assert!(format!("{err:#}").contains("are both true"), "{err:#}");
    }
}

#[cfg(test)]
mod panic_log_tests {
    use super::*;

    /// 反向断言（本组测试最重要的一条）：脱敏**不得吃掉诊断信号**。
    /// 定性证据就是这句常量消息，若被摘要逻辑打没，日志照样不可归因。
    #[test]
    fn real_panic_message_survives_untouched() {
        let raw = "`async fn` resumed after completion";
        assert_eq!(panic_payload_summary(raw), raw);
    }

    /// 裸 token（无 `key=` 前缀）是 `redact_sensitive_fields` 的原有盲区，
    /// 也正是 `unwrap()` 的 `Debug` 输出最常见的形态。
    /// 夹具一律用 `EXAMPLE` 占位段：既满足"测试不得含真实凭据"的贡献规范，
    /// 也让仓内自建的凭据扫描器能把它和真值区分开。
    #[test]
    fn bare_credential_shapes_are_redacted() {
        let cases = [
            ("rt.1.EXAMPLE0EXAMPLE0EXAMPLE0", "openai-refresh"),
            ("ya29.a0EXAMPLE0EXAMPLE0EXAMPLE", "google-access"),
            ("eyJEXAMPLE0EXAMPLE0EXAMPLE0", "jwt"),
            ("sk-proj-EXAMPLE0EXAMPLE0EXAMPLE", "api-key"),
            ("ghp_EXAMPLE0EXAMPLE0EXAMPLE0", "github"),
            ("xoxb-EXAMPLE0-EXAMPLE0-EXAMPLE", "slack"),
            ("AKIAEXAMPLE0EXAMPLE0", "aws"),
        ];
        for (secret, kind) in cases {
            let raw = format!("called `Result::unwrap()` on an `Err` value: \"{secret}\"");
            let summary = panic_payload_summary(&raw);
            assert!(
                !summary.contains(&secret[..secret.len().min(20)]),
                "凭据原文漏进日志：{kind} ⇒ {summary}"
            );
            assert!(
                summary.contains(&format!("[REDACTED:{kind}]")),
                "未按形态识别 {kind}：{summary}"
            );
            // 诊断上下文必须保留：只删秘密，不删整条消息。
            assert!(
                summary.contains("called `Result::unwrap()`"),
                "脱敏顺带打丢了诊断上下文：{summary}"
            );
        }
    }

    /// 换行不得穿过脱敏层进入日志字段（否则一条 panic 可伪造成两条记录）。
    #[test]
    fn control_characters_cannot_forge_extra_log_lines() {
        let raw = "boom\n\tevent=\"fake\"\u{1b}[0m\u{0}done";
        let summary = panic_payload_summary(raw);
        assert!(
            !summary.chars().any(char::is_control),
            "摘要里仍有控制字符：{summary:?}"
        );
        assert!(summary.contains("boom") && summary.contains("done"));
    }

    /// 长度契约复用 OAuth 摘要那条：**按字符**截断（多字节中文不会把预算撑爆成字节数），
    /// 超长时补 3 个点的省略标记。
    #[test]
    fn summary_is_bounded() {
        let ascii = "x".repeat(5000);
        let summary = panic_payload_summary(&ascii);
        assert_eq!(summary.chars().count(), 303, "{summary}");

        let wide = "错".repeat(5000);
        let summary = panic_payload_summary(&wide);
        assert_eq!(summary.chars().count(), 303);
        assert!(summary.ends_with("..."), "{summary}");
    }

    /// 自定义 panic 类型：既不能漏内容（不转 `Debug`），也不给空字段。
    #[test]
    fn non_string_payload_falls_back_to_placeholder() {
        let boxy: Box<dyn std::any::Any + Send> = Box::new(42_u32);
        let summary = payload_summary(boxy.as_ref());
        assert_eq!(summary, "<non-string panic payload>");
        assert!(!summary.contains("42"), "{summary}");
    }
}

#[cfg(test)]
mod model_list_tests {
    use super::*;
    use serial_test::serial;

    #[tokio::test]
    async fn check_model_list_consistency_consistent() {
        let app = axum::Router::new().route(
            "/admin/model/list",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!([
                    {"id": "deepseek-chat"},
                    {"id": "deepseek-coder"}
                ]))
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock");
        });

        let mut cfg = config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: format!("http://{addr}"),
            auth_mode: Some(config::RemoteAuthMode::Open),
            auth_token_env: "LP_TEST_NOEXIST_1".into(),
        });

        let result = check_model_list_consistency(&cfg).await;
        assert!(result.is_ok(), "consistent model list should succeed");
    }

    #[tokio::test]
    async fn check_model_list_consistency_drift_detected() {
        let app = axum::Router::new().route(
            "/admin/model/list",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!([
                    {"id": "deepseek-chat"},
                    {"id": "new-model-on-remote"}
                ]))
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock");
        });

        let mut cfg = config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: format!("http://{addr}"),
            auth_mode: Some(config::RemoteAuthMode::Open),
            auth_token_env: "LP_TEST_NOEXIST_2".into(),
        });

        // Should succeed but print warnings (not an error, just informational)
        let result = check_model_list_consistency(&cfg).await;
        assert!(
            result.is_ok(),
            "drift detection should not error, just warn"
        );
    }

    #[tokio::test]
    #[serial]
    async fn check_model_list_consistency_with_bearer_auth() {
        use axum::http::HeaderMap;
        use std::sync::{Arc, Mutex};

        // SAFETY: unique env var name
        unsafe { std::env::set_var("LP_TEST_MODEL_CHECK_TOKEN", "lp-model-check-token") };

        let auth_header_received = Arc::new(Mutex::new(None::<String>));
        let auth_rec_clone = Arc::clone(&auth_header_received);

        let app = axum::Router::new().route(
            "/admin/model/list",
            axum::routing::get(move |headers: HeaderMap| {
                let auth_flag = Arc::clone(&auth_rec_clone);
                async move {
                    if let Some(auth) = headers.get("authorization") {
                        *auth_flag.lock().unwrap() = Some(auth.to_str().unwrap().to_string());
                    }
                    axum::Json(serde_json::json!([{"id": "deepseek-chat"}]))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock");
        });

        let mut cfg = config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: format!("http://{addr}"),
            auth_mode: Some(config::RemoteAuthMode::Token),
            auth_token_env: "LP_TEST_MODEL_CHECK_TOKEN".into(),
        });

        let result = check_model_list_consistency(&cfg).await;
        assert!(result.is_ok());

        let received = auth_header_received.lock().unwrap().clone();
        assert_eq!(
            received,
            Some("Bearer lp-model-check-token".to_string()),
            "model list check must send Bearer token in token mode"
        );

        unsafe { std::env::remove_var("LP_TEST_MODEL_CHECK_TOKEN") };
    }

    #[tokio::test]
    async fn check_model_list_consistency_unreachable_server() {
        let mut cfg = config::default_deepseek_config();
        cfg.server = None;
        cfg.remote = Some(config::RemoteConfig {
            url: "http://127.0.0.1:59998".to_string(),
            auth_mode: Some(config::RemoteAuthMode::Open),
            auth_token_env: "LP_TEST_NOEXIST_3".into(),
        });

        let result = check_model_list_consistency(&cfg).await;
        assert!(
            result.is_err(),
            "unreachable server should produce an error"
        );
    }
}

/// 把四个探针事件跑过给定 filter，返回实际落下的日志文本。
///
/// 抽成辅助函数是为了让两个门禁测试共用同一套发事件逻辑：一条测缺省分支、一条测
/// 「已设 `RUST_LOG`」分支。**必须传 filter，不得在内部调 `default_env_filter()`**——
/// 后者读进程级环境变量，测试若依赖它，`RUST_LOG=info/trace cargo test` 时会失败。
#[cfg(test)]
fn rendered_log_with_filter(filter: tracing_subscriber::EnvFilter) -> String {
    use tracing_subscriber::prelude::*;

    #[derive(Clone)]
    struct SharedWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
        type Writer = SharedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(SharedWriter(std::sync::Arc::clone(&buffer)))
        .with_filter(filter);
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));

    tracing::info!(
        target: "llm_proxy::connect",
        "own info must be visible"
    );
    tracing::info!(
        target: "hyper_util::client::legacy::pool",
        "third party info must be visible"
    );
    tracing::warn!(
        target: "hyper_util::client::legacy::pool",
        "third party warn must be visible"
    );
    tracing::error!(
        target: "hyper_util::client::legacy::pool",
        "third party error must be visible"
    );
    drop(_guard);

    String::from_utf8_lossy(&buffer.lock().expect("log buffer lock")).to_string()
}

/// 门禁：`RUST_LOG` 未设时，缺省 filter 必须
/// ① 放行我方 `llm_proxy::*` 的 `info!`（诊断默认留痕）
/// ② 挡住第三方 target 的 `info!` 与 `warn!`（噪声明星，保住「不无差别抬默认」的取舍）
/// ③ 放行第三方 `error!`
///
/// **本测试不读环境变量**（直接调纯函数 `default_env_filter_from(None)`），否则
/// `RUST_LOG=info/trace cargo test` 会失败。
///
/// 回归基线方式：把 `DEFAULT_ENV_FILTER` 改成 `"error"`，断言 ① 即失败。
#[test]
fn default_filter_surfaces_own_info_but_keeps_third_party_quiet() {
    let out = rendered_log_with_filter(default_env_filter_from(None));

    assert!(
        out.contains("own info must be visible"),
        "缺省 filter 必须放行我方 info —— 否则探测/连接诊断默认仍不可见: {out}"
    );
    assert!(
        !out.contains("third party info must be visible"),
        "缺省 filter 不得放行第三方 info —— 噪音会淹掉日志: {out}"
    );
    assert!(
        !out.contains("third party warn must be visible"),
        "第三方必须仍停在 ERROR —— 保住「不无差别抬默认」的取舍: {out}"
    );
    assert!(
        out.contains("third party error must be visible"),
        "第三方 error 必须照常可见: {out}"
    );
}

/// 门禁：一旦显式设了 `RUST_LOG`，行为必须与改前**逐字节等价**——
/// 包括空串（`RUST_LOG=""` 仍要回落到 ERROR 缺省，而不是变成全关）。
///
/// 这一条守的是「不动别人的决策」：本单只改缺省分支，显式分支一行都不能变。
#[test]
fn explicit_rust_log_still_overrides_default_filter() {
    let info =
        rendered_log_with_filter(default_env_filter_from(Some(std::ffi::OsStr::new("info"))));
    assert!(
        info.contains("third party info must be visible"),
        "RUST_LOG=info 必须完全听环境变量（含第三方 info 放行），否则显式设值不再有效: {info}"
    );
    assert!(
        info.contains("own info must be visible"),
        "RUST_LOG=info 下我方 info 当然可见: {info}"
    );

    let empty = rendered_log_with_filter(default_env_filter_from(Some(std::ffi::OsStr::new(""))));
    assert!(
        !empty.contains("own info must be visible"),
        "RUST_LOG=（空串）必须回落到 ERROR 缺省，与改前一致 —— 不能变成全关也不能变成放开: {empty}"
    );
    assert!(
        empty.contains("third party error must be visible"),
        "RUST_LOG=（空串）下 error 仍须可见: {empty}"
    );
}

/// 静态门禁：仓库文本文件不得出现内部编号形态（词干 + 可选分隔符 + 数字）。
///
/// 命中判定（ASCII 大小写不敏感）：`issue` 词干左邻不得是字母/数字（防 `tissue-5`
/// 一类词内片段误报），词干后接可选单个 `-`/`_`/空格/`#` 分隔再紧跟数字；
/// 字母占位（非数字）不算命中。
///
/// 扫描面 = `git ls-files`（已跟踪）+ `--others --exclude-standard`（未跟踪非忽略）
/// 的全部文本文件，非增量——存量已清零，任何命中都是回归；非 UTF-8（二进制）跳过。
/// git 不可用（发布包、非仓库环境）时直接跳过，不制造假失败。
#[test]
fn issue_number_does_not_leak_into_scope_files() {
    use std::process::Command;

    let git_available = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !git_available {
        eprintln!("git 不可用，跳过编号外溢门禁");
        return;
    }

    let root = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("git rev-parse");
    if !root.status.success() {
        eprintln!("不在 git 仓库内，跳过编号外溢门禁");
        return;
    }
    let root_path = String::from_utf8_lossy(&root.stdout).trim().to_string();

    let mut files: Vec<String> = Vec::new();
    for args in [
        &["ls-files", "-z"][..],
        &["ls-files", "--others", "--exclude-standard", "-z"][..],
    ] {
        let out = Command::new("git")
            .args(args)
            .current_dir(&root_path)
            .output()
            .expect("git ls-files");
        assert!(out.status.success(), "git ls-files 必须成功: {args:?}");
        files.extend(
            String::from_utf8_lossy(&out.stdout)
                .split('\0')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    files.sort();
    files.dedup();

    // 单行命中判定：`issue` 词干（ASCII 大小写不敏感）+ 可选单个分隔符 + 数字；
    // 词干左邻是字母/数字时视为词内片段，不算命中。
    fn internal_number_hit(line: &str) -> Option<usize> {
        let lower = line.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let mut from = 0;
        while let Some(rel) = lower[from..].find("issue") {
            let idx = from + rel;
            let left_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
            if left_ok {
                let mut k = idx + 5;
                if matches!(bytes.get(k), Some(b'-' | b'_' | b' ' | b'#')) {
                    k += 1;
                }
                if bytes.get(k).is_some_and(|b| b.is_ascii_digit()) {
                    return Some(idx);
                }
            }
            from = idx + 5;
        }
        None
    }

    let mut violations: Vec<String> = Vec::new();
    for file in &files {
        let Ok(bytes) = std::fs::read(std::path::Path::new(&root_path).join(file)) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if let Some(col) = internal_number_hit(line) {
                violations.push(format!(
                    "{file}:{}: 列 {} 起: {}",
                    i + 1,
                    col + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "内部编号形态不得出现在仓库文本文件中（全仓零容忍）:\n{}",
        violations.join("\n")
    );
}
