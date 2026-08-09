use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "llm-proxy")]
#[command(about = "Local LLM protocol gateway")]
#[command(version)]
pub struct Cli {
    #[arg(long, short)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Init,
    Status {
        #[arg(long)]
        probe: bool,
    },
    Cooldown {
        #[command(subcommand)]
        command: CooldownCommand,
    },
    Connect(ProviderAddArgs),
    #[command(args_conflicts_with_subcommands = true)]
    Provider {
        #[arg(long)]
        select: Option<String>,
        #[command(subcommand)]
        command: Option<ProviderCommand>,
    },
    Model {
        #[command(subcommand)]
        command: Option<ModelCommand>,
    },
    Serve {
        #[arg(long = "foreground", visible_alias = "frontend")]
        frontend: bool,
    },
    Shutdown,
    Restart,
    Doc {
        #[arg(long)]
        list: bool,
        #[arg(long)]
        raw: bool,
        #[arg(long)]
        section: Option<String>,
    },
    Version,
    Completion {
        shell: CompletionShell,
    },
    Launch {
        #[command(subcommand)]
        command: LaunchCommand,
    },
    /// Token usage statistics
    Usage(UsageArgs),
    /// Subscription quota for OAuth providers
    Quota(QuotaArgs),
    /// 内部命令：输出补全候选（隐藏，供 shell 补全脚本调用）
    #[command(hide = true)]
    CompleteCandidates {
        /// candidates type: provider | model
        #[arg(value_enum)]
        kind: CompleteCandidatesKind,
    },
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum CompleteCandidatesKind {
    Provider,
    Model,
}

/// Subscription quota query arguments
#[derive(Debug, clap::Args)]
pub struct QuotaArgs {
    /// Force refresh (bypass cache); cache not yet implemented — this flag is currently a no-op
    #[arg(long)]
    pub refresh: bool,
}

/// Token usage statistics arguments
#[derive(Debug, clap::Args)]
pub struct UsageArgs {
    /// Time period (e.g., today, 7d, 2026-03-12:2026-03-20)
    #[arg(short, long, value_name = "PERIOD")]
    pub period: Option<String>,

    /// Provider filter
    #[arg(long, value_name = "PROVIDER")]
    pub provider: Option<String>,

    /// Model filter
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Endpoint filter (openai_chat, openai_responses, anthropic)
    #[arg(long, value_enum, value_name = "ENDPOINT")]
    pub endpoint: Option<EndpointArg>,

    /// View mode
    #[arg(long, value_enum, default_value_t = ViewArg::ByModel, value_name = "VIEW")]
    pub view: ViewArg,

    /// JSON output format
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, clap::ValueEnum)]
// variant 名保持 By* 前缀：clap ValueEnum 默认按 kebab-case 派生 CLI 值（by-model 等），
// 重命名会改变已有 CLI 参数值，属于对外行为变更，故保留并 allow。
#[allow(clippy::enum_variant_names)]
pub enum ViewArg {
    ByModel,
    ByProvider,
    ByEndpoint,
    ByHour,
    ByDay,
}

impl ViewArg {
    pub fn as_str(&self) -> &'static str {
        match self {
            ViewArg::ByModel => "by-model",
            ViewArg::ByProvider => "by-provider",
            ViewArg::ByEndpoint => "by-endpoint",
            ViewArg::ByHour => "by-hour",
            ViewArg::ByDay => "by-day",
        }
    }
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum EndpointArg {
    OpenaiChat,
    OpenaiResponses,
    Anthropic,
}

impl EndpointArg {
    pub fn as_str(&self) -> &'static str {
        match self {
            EndpointArg::OpenaiChat => "openai_chat",
            EndpointArg::OpenaiResponses => "openai_responses",
            EndpointArg::Anthropic => "anthropic",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Elvish,
}

impl From<CompletionShell> for clap_complete::Shell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => clap_complete::Shell::Bash,
            CompletionShell::Zsh => clap_complete::Shell::Zsh,
            CompletionShell::Fish => clap_complete::Shell::Fish,
            CompletionShell::PowerShell => clap_complete::Shell::PowerShell,
            CompletionShell::Elvish => clap_complete::Shell::Elvish,
        }
    }
}

#[derive(Debug, Clone, Parser)]
pub struct ProviderAddArgs {
    pub product: Option<String>,
    #[arg(long = "api-key-env")]
    pub api_key_env: Option<String>,
    #[arg(long)]
    pub no_api_key: bool,
    #[arg(long = "type")]
    pub provider_type: Option<String>,
    #[arg(long = "endpoint-url")]
    pub endpoint_url: Option<String>,
    #[arg(long = "model")]
    pub models: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// List configured providers (table format by default)
    List {
        /// Output raw JSON instead of table format
        #[arg(long)]
        json: bool,
    },
    Add(ProviderAddArgs),
    Info {
        name: Option<String>,
    },
    Login {
        name: String,
    },
    Copy {
        source: String,
        name: String,
        #[arg(long = "api-key-env")]
        api_key_env: Option<String>,
        #[arg(long)]
        no_api_key: bool,
    },
    Remove {
        name: String,
        #[arg(long)]
        force: bool,
    },
    ResetUsage {
        name: String,
        #[arg(long)]
        force: bool,
    },
    Logout {
        name: String,
    },
    Relogin {
        name: String,
    },
    Refresh {
        name: String,
    },
    /// Batch fallback configuration (insert provider as fallback for another)
    Fallback {
        #[command(subcommand)]
        command: FallbackCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum FallbackCommand {
    /// Insert a provider as fallback after a target provider for specified (model, endpoint) combinations
    Add(ProviderFallbackAddArgs),
}

/// Arguments for `provider fallback add`.
#[derive(Debug, Clone, Parser)]
pub struct ProviderFallbackAddArgs {
    /// Provider that provides the fallback (the fallback provider)
    #[arg(long)]
    pub provider: String,
    /// Target provider being backed up (the provider to add fallback for)
    #[arg(long)]
    pub target: String,
    /// Binding specifications in model:endpoint format (e.g. deepseek-v4-pro-lp:chat).
    /// Supports regex (e.g. deepseek-v4-.*:.*). Required, at least one.
    #[arg(long, required = true)]
    pub bindings: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    List,
    Info {
        model: String,
    },
    Add {
        model: String,
        /// Required unless --copy-from/--from-discovery; must be > 0 when provided.
        #[arg(long = "context-window")]
        context_window: Option<i64>,
        /// Required unless --copy-from/--from-discovery; must be > 0 when provided.
        #[arg(long = "max-output")]
        max_output: Option<i64>,
        #[arg(long = "copy-from", conflicts_with = "from_discovery")]
        copy_from: Option<String>,
        #[arg(long = "from-discovery", conflicts_with = "copy_from")]
        from_discovery: Option<String>,
        #[arg(long = "upstream-model", requires = "from_discovery")]
        upstream_model: Option<String>,
    },
    Set {
        model: String,
        #[arg(long = "context-window")]
        context_window: Option<i64>,
        #[arg(long = "max-output")]
        max_output: Option<i64>,
        #[arg(long = "thinking-level")]
        thinking_level: Option<String>,
        #[arg(long = "supported-thinking-level")]
        supported_thinking_levels: Vec<String>,
        #[arg(long = "enable-thinking", conflicts_with = "disable_thinking")]
        enable_thinking: bool,
        #[arg(long = "disable-thinking")]
        disable_thinking: bool,
        #[arg(long = "enable-feature")]
        enable_features: Vec<String>,
        #[arg(long = "disable-feature")]
        disable_features: Vec<String>,
    },
    Remove {
        model: String,
        #[arg(long)]
        force: bool,
    },
    Provider {
        #[command(subcommand)]
        command: ModelProviderCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ModelProviderCommand {
    Add {
        model: String,
        #[arg(long = "type", alias = "protocol")]
        provider_type: String,
        #[arg(long)]
        provider: String,
        #[arg(long = "upstream-model")]
        upstream_model: Option<String>,
    },
    Remove {
        model: String,
        #[arg(long = "type", alias = "protocol")]
        provider_type: String,
        #[arg(long)]
        provider: String,
    },
    Move {
        model: String,
        #[arg(long = "type", alias = "protocol")]
        provider_type: String,
        #[arg(long)]
        provider: String,
        #[arg(long = "to")]
        to: usize,
    },
}

#[derive(Debug, Subcommand)]
pub enum CooldownCommand {
    List,
    Clear {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        provider: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum LaunchCommand {
    Codex {
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    CodexDesktop {
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    Pi {
        #[arg(long)]
        pi_home: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    QwenCode {
        model_id: Option<String>,
        #[arg(long)]
        qwen_home: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    ClaudeCode {
        model_id: Option<String>,
        #[arg(long)]
        claude_home: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    ClaudeDesktop {
        #[arg(long = "claude-desktop-home")]
        claude_desktop_home: Option<PathBuf>,
        #[arg(long, default_value = "llm-proxy")]
        profile: String,
        #[arg(long)]
        dry_run: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_migrate_and_auth_commands_are_not_available() {
        assert!(Cli::try_parse_from(["llm-proxy", "migrate"]).is_err());
        assert!(Cli::try_parse_from(["llm-proxy", "auth"]).is_err());
    }

    #[test]
    fn connect_uses_positional_product_and_model_flags() {
        match Cli::try_parse_from([
            "llm-proxy",
            "connect",
            "deepseek",
            "--api-key-env",
            "DEEPSEEK_API_KEY",
            "--model",
            "deepseek-v4-pro-lp",
        ])
        .expect("parse connect")
        .command
        .expect("command")
        {
            Command::Connect(args) => {
                assert_eq!(args.product.as_deref(), Some("deepseek"));
                assert_eq!(args.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
                assert_eq!(args.models, vec!["deepseek-v4-pro-lp"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn provider_select_parses_as_tui_detail_convenience() {
        match Cli::try_parse_from(["llm-proxy", "provider", "--select", "deepseek"])
            .expect("parse provider select")
            .command
            .expect("command")
        {
            Command::Provider { select, command } => {
                assert_eq!(select.as_deref(), Some("deepseek"));
                assert!(command.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn provider_copy_remove_and_reset_usage_parse() {
        assert!(matches!(
            Cli::try_parse_from([
                "llm-proxy",
                "provider",
                "copy",
                "deepseek",
                "deepseek-copy",
                "--api-key-env",
                "DEEPSEEK_COPY_KEY",
            ])
            .expect("parse copy")
            .command,
            Some(Command::Provider {
                select: None,
                command: Some(ProviderCommand::Copy { .. })
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["llm-proxy", "provider", "remove", "deepseek", "--force"])
                .expect("parse remove")
                .command,
            Some(Command::Provider {
                select: None,
                command: Some(ProviderCommand::Remove { force: true, .. })
            })
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "llm-proxy",
                "provider",
                "reset-usage",
                "openai-subscription"
            ])
            .expect("parse reset")
            .command,
            Some(Command::Provider {
                select: None,
                command: Some(ProviderCommand::ResetUsage { .. })
            })
        ));
    }

    #[test]
    fn model_add_from_discovery_parses_provider_and_upstream_model() {
        match Cli::try_parse_from([
            "llm-proxy",
            "model",
            "add",
            "qwen3-27b-local",
            "--from-discovery",
            "ollama",
            "--upstream-model",
            "qwen3:27b",
            "--max-output",
            "4096",
        ])
        .expect("parse model add from discovery")
        .command
        .expect("command")
        {
            Command::Model {
                command:
                    Some(ModelCommand::Add {
                        model,
                        from_discovery,
                        upstream_model,
                        max_output,
                        ..
                    }),
            } => {
                assert_eq!(model, "qwen3-27b-local");
                assert_eq!(from_discovery.as_deref(), Some("ollama"));
                assert_eq!(upstream_model.as_deref(), Some("qwen3:27b"));
                assert_eq!(max_output, Some(4096));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn model_set_parses_parameter_thinking_and_feature_flags() {
        match Cli::try_parse_from([
            "llm-proxy",
            "model",
            "set",
            "model-a",
            "--context-window",
            "200000",
            "--max-output",
            "8192",
            "--thinking-level",
            "high",
            "--supported-thinking-level",
            "low",
            "--supported-thinking-level",
            "high",
            "--disable-thinking",
            "--enable-feature",
            "image_input",
            "--disable-feature",
            "document_input",
        ])
        .expect("parse model set")
        .command
        .expect("command")
        {
            Command::Model {
                command:
                    Some(ModelCommand::Set {
                        model,
                        context_window,
                        max_output,
                        thinking_level,
                        supported_thinking_levels,
                        disable_thinking,
                        enable_features,
                        disable_features,
                        ..
                    }),
            } => {
                assert_eq!(model, "model-a");
                assert_eq!(context_window, Some(200000));
                assert_eq!(max_output, Some(8192));
                assert_eq!(thinking_level.as_deref(), Some("high"));
                assert_eq!(supported_thinking_levels, vec!["low", "high"]);
                assert!(disable_thinking);
                assert_eq!(enable_features, vec!["image_input"]);
                assert_eq!(disable_features, vec!["document_input"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn codex_desktop_launch_parses_like_codex_launch() {
        match Cli::try_parse_from([
            "llm-proxy",
            "launch",
            "codex-desktop",
            "--codex-home",
            "/tmp/codex-desktop",
            "--dry-run",
        ])
        .expect("parse codex desktop launch")
        .command
        .expect("command")
        {
            Command::Launch {
                command:
                    LaunchCommand::CodexDesktop {
                        codex_home,
                        dry_run,
                    },
            } => {
                assert_eq!(codex_home, Some(PathBuf::from("/tmp/codex-desktop")));
                assert!(dry_run);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn model_provider_add_parses_protocol_binding_args() {
        match Cli::try_parse_from([
            "llm-proxy",
            "model",
            "provider",
            "add",
            "model-a",
            "--type",
            "anthropic",
            "--provider",
            "deepseek",
            "--upstream-model",
            "deepseek-v4-flash",
        ])
        .expect("parse model provider add")
        .command
        .expect("command")
        {
            Command::Model {
                command:
                    Some(ModelCommand::Provider {
                        command:
                            ModelProviderCommand::Add {
                                model,
                                provider_type,
                                provider,
                                upstream_model,
                            },
                    }),
            } => {
                assert_eq!(model, "model-a");
                assert_eq!(provider_type, "anthropic");
                assert_eq!(provider, "deepseek");
                assert_eq!(upstream_model.as_deref(), Some("deepseek-v4-flash"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn model_provider_add_accepts_protocol_alias_for_type() {
        // --protocol should work as an alias for --type in all three subcommands.
        for (subcmd, extra_args) in [
            ("add", vec!["--provider", "deepseek"] as Vec<&str>),
            ("remove", vec!["--provider", "deepseek"]),
            ("move", vec!["--provider", "deepseek", "--to", "1"]),
        ] {
            let mut args = vec![
                "llm-proxy",
                "model",
                "provider",
                subcmd,
                "model-a",
                "--protocol",
                "chat",
            ];
            args.extend_from_slice(&extra_args);
            Cli::try_parse_from(&args)
                .unwrap_or_else(|e| panic!("--protocol alias failed for `{subcmd}`: {e}"));
        }
    }

    #[test]
    fn provider_add_supports_custom_provider_without_model() {
        match Cli::try_parse_from([
            "llm-proxy",
            "provider",
            "add",
            "local-custom",
            "--type",
            "openai-chat",
            "--endpoint-url",
            "http://127.0.0.1:11434/v1/chat/completions",
            "--no-api-key",
        ])
        .expect("parse provider add")
        .command
        .expect("command")
        {
            Command::Provider {
                select: None,
                command: Some(ProviderCommand::Add(args)),
            } => {
                assert_eq!(args.product.as_deref(), Some("local-custom"));
                assert_eq!(args.provider_type.as_deref(), Some("openai-chat"));
                assert!(args.models.is_empty());
                assert!(args.no_api_key);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn status_probe_is_primary_flag_and_refresh_removed() {
        match Cli::try_parse_from(["llm-proxy", "status", "--probe"])
            .expect("parse --probe")
            .command
            .expect("command")
        {
            Command::Status { probe } => assert!(probe),
            other => panic!("unexpected command: {other:?}"),
        }
        // §12.4 不保留 --refresh（全新设计，无历史包袱）；必须拒绝解析
        assert!(
            Cli::try_parse_from(["llm-proxy", "status", "--refresh"]).is_err(),
            "--refresh should be rejected after removal"
        );
    }

    #[test]
    fn status_help_shows_probe_not_refresh_as_primary_flag() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let status = cmd
            .find_subcommand_mut("status")
            .expect("status subcommand");
        let mut help = Vec::new();
        status.write_long_help(&mut help).expect("help");
        let help = String::from_utf8(help).expect("utf8");
        assert!(help.contains("--probe"));
        assert!(!help.contains("--refresh"));
    }

    #[test]
    fn quota_parses_with_and_without_refresh() {
        match Cli::try_parse_from(["llm-proxy", "quota"])
            .expect("parse quota")
            .command
            .expect("command")
        {
            Command::Quota(args) => assert!(!args.refresh),
            other => panic!("unexpected command: {other:?}"),
        }
        match Cli::try_parse_from(["llm-proxy", "quota", "--refresh"])
            .expect("parse quota --refresh")
            .command
            .expect("command")
        {
            Command::Quota(args) => assert!(args.refresh),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn complete_candidates_parses_provider_and_model_kinds() {
        match Cli::try_parse_from(["llm-proxy", "complete-candidates", "provider"])
            .expect("parse provider candidates")
            .command
            .expect("command")
        {
            Command::CompleteCandidates { kind } => {
                assert!(matches!(kind, CompleteCandidatesKind::Provider));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        match Cli::try_parse_from(["llm-proxy", "complete-candidates", "model"])
            .expect("parse model candidates")
            .command
            .expect("command")
        {
            Command::CompleteCandidates { kind } => {
                assert!(matches!(kind, CompleteCandidatesKind::Model));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn complete_candidates_is_hidden_from_help_but_still_parseable() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let mut help = Vec::new();
        cmd.write_long_help(&mut help).expect("help");
        let help = String::from_utf8(help).expect("utf8");
        assert!(
            !help.contains("complete-candidates"),
            "hidden command must not appear in --help"
        );
        // 隐藏命令不影响解析
        assert!(Cli::try_parse_from(["llm-proxy", "complete-candidates", "provider"]).is_ok());
    }

    #[test]
    fn provider_fallback_add_parses_correctly() {
        match Cli::try_parse_from([
            "llm-proxy",
            "provider",
            "fallback",
            "add",
            "--provider",
            "deepseek-3",
            "--target",
            "deepseek",
            "--bindings",
            "deepseek-v4-pro-lp:responses",
            "--bindings",
            "deepseek-v4-pro-lp:anthropic",
        ])
        .expect("parse provider fallback add")
        .command
        .expect("command")
        {
            Command::Provider {
                select: None,
                command:
                    Some(ProviderCommand::Fallback {
                        command: FallbackCommand::Add(args),
                    }),
            } => {
                assert_eq!(args.provider, "deepseek-3");
                assert_eq!(args.target, "deepseek");
                assert_eq!(
                    args.bindings,
                    vec![
                        "deepseek-v4-pro-lp:responses",
                        "deepseek-v4-pro-lp:anthropic"
                    ]
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn provider_fallback_add_requires_bindings() {
        // Missing --bindings should fail
        assert!(
            Cli::try_parse_from([
                "llm-proxy",
                "provider",
                "fallback",
                "add",
                "--provider",
                "deepseek-3",
                "--target",
                "deepseek",
            ])
            .is_err(),
            "provider fallback add without --bindings should fail"
        );
    }

    #[test]
    fn provider_fallback_add_supports_regex_bindings() {
        match Cli::try_parse_from([
            "llm-proxy",
            "provider",
            "fallback",
            "add",
            "--provider",
            "deepseek-2",
            "--target",
            "deepseek",
            "--bindings",
            "deepseek-v4-.*:.*",
        ])
        .expect("parse regex bindings")
        .command
        .expect("command")
        {
            Command::Provider {
                command:
                    Some(ProviderCommand::Fallback {
                        command: FallbackCommand::Add(args),
                    }),
                ..
            } => {
                assert_eq!(args.bindings, vec!["deepseek-v4-.*:.*"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
