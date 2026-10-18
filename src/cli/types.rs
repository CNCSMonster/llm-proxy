use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "llm-proxy")]
#[command(about = "Local LLM protocol gateway")]
#[command(version = crate::version::CLI_VERSION)]
pub struct Cli {
    /// Config directory path (contains config.toml, tokens.toml, etc.)
    #[arg(long = "config-dir", short = 'd')]
    pub config_dir: Option<PathBuf>,

    /// Load and statically validate the configuration, print desensitized
    /// diagnostics and runtime preflight checks, then exit — no listeners,
    /// state writes, or network. Equivalent to `serve --dry-run`.
    #[arg(long = "dry-run", global = true)]
    pub dry_run: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)] // CLI subcommands are parsed once on startup; boxing would add heap allocations
pub enum Command {
    Init,
    Status {
        #[arg(long)]
        probe: bool,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
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
    #[command(args_conflicts_with_subcommands = true)]
    Model {
        #[arg(long)]
        select: Option<String>,
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
    /// Generate client configuration files for supported AI coding clients.
    ///
    /// 所有 `--<client>-config-dir <dir>` 参数指向该客户端的配置目录本身（与该客户端原生隔离环境变量所指的目录一一对应；例外：claude-desktop 无原生 env，只承诺写侧平台路径），不是 `$HOME`。做隔离实验：指向空目录，再按映射表用对应环境变量启动客户端。
    Launch {
        #[command(subcommand)]
        command: LaunchCommand,
    },
    /// Token usage statistics
    Usage(UsageArgs),
    /// Manage API tokens for access control & quota
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
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

    /// Maximum number of records to return (default 1000, max 10000)
    #[arg(long, default_value = "1000", value_name = "LIMIT")]
    pub limit: usize,

    /// Number of records to skip (for pagination)
    #[arg(long, default_value = "0", value_name = "OFFSET")]
    pub offset: usize,
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
    /// 跳过网络可达性探测，直接写入配置
    /// （探测发现不可达模型时，交互终端会询问是否照常写入；本开关供非交互场景显式放行）
    #[arg(long = "skip-probe")]
    pub skip_probe: bool,
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
    /// Reconcile catalog model bindings for an existing catalog-backed provider
    Reselect {
        name: String,
        #[arg(long = "model", required = true)]
        models: Vec<String>,
    },
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
pub enum TokenCommand {
    /// Create a token, generate a key, write to tokens.toml
    Create(TokenCreateArgs),
    /// List all tokens with a usage summary
    List {
        /// Output raw JSON instead of table format
        #[arg(long)]
        json: bool,
    },
    /// Show detailed window usage for one token (by key or name)
    Usage {
        /// Token key or name
        key: String,
    },
    /// Delete a token (from tokens.toml + clear its quota state)
    Delete {
        /// Token key or name
        key: String,
    },
    /// Reset a token's usage counters
    ResetUsage {
        /// Token key or name
        key: String,
    },
    /// Rename a token (updates tokens.toml and synchronizes cooldown entries)
    Rename {
        /// Token key or name
        key: String,
        /// New token name
        new_name: String,
    },
    /// : Migrate tokens.toml to tokens/ directory (one file per token)
    Migrate,
}

/// Arguments for `token create`.
#[derive(Debug, Clone, Parser)]
pub struct TokenCreateArgs {
    /// Human-readable token name (unique)
    #[arg(long)]
    pub name: String,
    /// Allowed models (comma-separated). Omit to allow all models.
    #[arg(long, value_delimiter = ',')]
    pub models: Vec<String>,
    /// Quota window duration, repeatable in pairs with --quota-mode/--quota-limit
    /// (e.g. --window 5h --quota-mode requests --quota-limit 60).
    #[arg(long)]
    pub window: Vec<String>,
    /// Quota mode aligned with each --window: tokens | requests.
    #[arg(long = "quota-mode")]
    pub quota_mode: Vec<String>,
    /// Quota limit aligned with each --window (positive integer).
    #[arg(long = "quota-limit")]
    pub quota_limit: Vec<i64>,
    /// ISO 8601 expiration (e.g. 2026-12-31T23:59:59Z). Omit for never.
    #[arg(long)]
    pub expires: Option<String>,
    /// : When set, the --window/--quota-mode/--quota-limit flags define
    /// a model quota group for these models (comma-separated) instead of flat
    /// token-wide windows. Creates a model_quota_groups entry.
    #[arg(long = "group-models", value_delimiter = ',')]
    pub group_models: Vec<String>,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)] // CLI subcommands are parsed once on startup; boxing would add heap allocations
pub enum ModelCommand {
    List,
    Info {
        model: String,
    },
    Add {
        model: String,
        /// Required unless --copy-from/--from-discovery/--route; must be > 0 when provided.
        #[arg(
            long = "context-window",
            conflicts_with = "copy_from",
            conflicts_with = "route"
        )]
        context_window: Option<i64>,
        /// Required unless --copy-from/--from-discovery/--route; must be > 0 when provided.
        #[arg(
            long = "max-output",
            conflicts_with = "copy_from",
            conflicts_with = "route"
        )]
        max_output: Option<i64>,
        #[arg(
            long = "copy-from",
            conflicts_with = "from_discovery",
            conflicts_with = "context_window",
            conflicts_with = "max_output",
            conflicts_with = "upstream_model",
            conflicts_with = "route"
        )]
        copy_from: Option<String>,
        #[arg(
            long = "from-discovery",
            conflicts_with = "copy_from",
            conflicts_with = "route"
        )]
        from_discovery: Option<String>,
        #[arg(
            long = "upstream-model",
            requires = "from_discovery",
            conflicts_with = "copy_from"
        )]
        upstream_model: Option<String>,
        /// Add a routing entry (level=target). Repeatable. Makes the model a routing model.
        #[arg(long = "route", value_name = "LEVEL=TARGET")]
        route: Vec<String>,
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
        #[arg(long = "supports-thinking")]
        supports_thinking: Option<bool>,
        #[arg(long = "force-think")]
        force_think: Option<bool>,
        #[arg(long = "force-no-think")]
        force_no_think: Option<bool>,
        #[arg(long = "default-think")]
        default_think: Option<bool>,
        #[arg(long = "enable-feature")]
        enable_features: Vec<String>,
        #[arg(long = "disable-feature")]
        disable_features: Vec<String>,
        #[arg(long = "description")]
        description: Option<String>,
        #[arg(long = "clear-description")]
        clear_description: bool,
        #[arg(long = "chat-reasoning-off-format")]
        chat_reasoning_off_format: Option<String>,
        #[arg(long = "clear-chat-reasoning-off-format")]
        clear_chat_reasoning_off_format: bool,
        #[arg(long = "service-tier")]
        service_tier: Option<String>,
        #[arg(long = "default-tier", requires = "service_tier")]
        default_tier: bool,
        #[arg(
            long = "forced-tier",
            requires = "service_tier",
            conflicts_with = "default_tier"
        )]
        forced_tier: bool,
        #[arg(long = "clear-service-tier", conflicts_with = "service_tier")]
        clear_service_tier: bool,
        #[arg(long = "cooldown-network")]
        cooldown_network: Option<u64>,
        #[arg(long = "cooldown-server-error")]
        cooldown_server_error: Option<u64>,
        #[arg(long = "cooldown-rate-limit")]
        cooldown_rate_limit: Option<u64>,
        #[arg(long = "cooldown-model-unavailable")]
        cooldown_model_unavailable: Option<u64>,
        #[arg(long = "cooldown-client-error")]
        cooldown_client_error: Option<u64>,
        #[arg(long = "cooldown-auth-error")]
        cooldown_auth_error: Option<u64>,
        #[arg(long = "clear-cooldown")]
        clear_cooldown: bool,
    },
    Remove {
        model: String,
        #[arg(long)]
        force: bool,
    },
    Rename {
        old_model: String,
        new_model: String,
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
    Set {
        model: String,
        #[arg(long = "type", alias = "protocol")]
        provider_type: String,
        #[arg(long)]
        provider: String,
        #[arg(long = "upstream-model")]
        upstream_model: String,
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
    Set {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        duration: String,
        #[arg(long)]
        protocol: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    Clear {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Clone, clap::Args, Default, PartialEq, Eq)]
pub struct LaunchFilterArgs {
    /// Include only specified models in client configuration (comma-separated or repeated)
    #[arg(long = "models", alias = "include-models", value_delimiter = ',')]
    pub models: Vec<String>,

    /// Exclude specified models from client configuration (comma-separated or repeated)
    #[arg(long = "exclude-models", value_delimiter = ',')]
    pub exclude_models: Vec<String>,
}

/// Generate client configuration files for supported AI coding clients.
///
/// 所有 `--<client>-config-dir <dir>` 参数指向该客户端的配置目录本身（与该客户端原生隔离环境变量所指的目录一一对应；例外：claude-desktop 无原生 env，只承诺写侧平台路径），不是 `$HOME`。做隔离实验：指向空目录，再按映射表用对应环境变量启动客户端。
#[derive(Debug, Subcommand)]
pub enum LaunchCommand {
    Codex {
        #[arg(
            long = "codex-config-dir",
            help = "Target config directory (= CODEX_HOME; writes config.toml and model catalog; default: $CODEX_HOME or ~/.codex)"
        )]
        codex_config_dir: Option<PathBuf>,
        #[arg(
            long = "codex-home",
            hide = true,
            num_args = 0..=1,
            default_missing_value = "",
            value_parser = crate::cli::launch_spec::reject_codex_home
        )]
        codex_home: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Check if the local model list matches the remote server's current model list
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        filter: LaunchFilterArgs,
    },
    CodexDesktop {
        #[arg(
            long = "codex-config-dir",
            help = "Target config directory (= CODEX_HOME; shared with codex; default: $CODEX_HOME or ~/.codex)"
        )]
        codex_config_dir: Option<PathBuf>,
        #[arg(
            long = "codex-home",
            hide = true,
            num_args = 0..=1,
            default_missing_value = "",
            value_parser = crate::cli::launch_spec::reject_codex_home
        )]
        codex_home: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Check if the local model list matches the remote server's current model list
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        filter: LaunchFilterArgs,
    },
    Pi {
        #[arg(
            long = "pi-config-dir",
            help = "Target config directory (= PI_CODING_AGENT_DIR; writes models.json; default: $PI_CODING_AGENT_DIR or ~/.pi/agent)"
        )]
        pi_config_dir: Option<PathBuf>,
        #[arg(
            long = "pi-home",
            hide = true,
            num_args = 0..=1,
            default_missing_value = "",
            value_parser = crate::cli::launch_spec::reject_pi_home
        )]
        pi_home: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Check if the local model list matches the remote server's current model list
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        filter: LaunchFilterArgs,
    },
    QwenCode {
        #[arg(
            long = "qwen-config-dir",
            help = "Target config directory (= QWEN_HOME; writes settings.json; default: $QWEN_HOME or ~/.qwen)"
        )]
        qwen_config_dir: Option<PathBuf>,
        #[arg(
            long = "qwen-home",
            hide = true,
            num_args = 0..=1,
            default_missing_value = "",
            value_parser = crate::cli::launch_spec::reject_qwen_home
        )]
        qwen_home: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Check if the local model list matches the remote server's current model list
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        filter: LaunchFilterArgs,
    },
    ClaudeCode {
        #[arg(
            long = "claude-config-dir",
            help = "Target config directory (= CLAUDE_CONFIG_DIR; writes settings.json; default: $CLAUDE_CONFIG_DIR or ~/.claude)"
        )]
        claude_config_dir: Option<PathBuf>,
        #[arg(
            long = "claude-home",
            hide = true,
            num_args = 0..=1,
            default_missing_value = "",
            value_parser = crate::cli::launch_spec::reject_claude_home
        )]
        claude_home: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Check if the local model list matches the remote server's current model list
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        filter: LaunchFilterArgs,
    },
    ClaudeDesktop {
        #[arg(
            long = "claude-desktop-config-dir",
            help = "Target config directory (custom write target, best-effort; no native env, see platform paths; writes claude_desktop_config.json)"
        )]
        claude_desktop_config_dir: Option<PathBuf>,
        #[arg(
            long = "claude-desktop-home",
            hide = true,
            num_args = 0..=1,
            default_missing_value = "",
            value_parser = crate::cli::launch_spec::reject_claude_desktop_home
        )]
        claude_desktop_home: Option<String>,
        #[arg(long, default_value = "llm-proxy")]
        profile: String,
        #[arg(long)]
        dry_run: bool,
        /// Check if the local model list matches the remote server's current model list
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        filter: LaunchFilterArgs,
    },
    /// Generate OpenCode provider configuration (JSON/JSONC)
    #[command(name = "opencode", alias = "open-code")]
    OpenCode {
        #[arg(
            long = "opencode-config-dir",
            help = "Target config directory (= OPENCODE_CONFIG_DIR; writes opencode.json; default: $OPENCODE_CONFIG_DIR or XDG config)"
        )]
        opencode_config_dir: Option<PathBuf>,
        #[arg(
            long = "opencode-home",
            hide = true,
            num_args = 0..=1,
            default_missing_value = "",
            value_parser = crate::cli::launch_spec::reject_opencode_home
        )]
        opencode_home: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Check if the local model list matches the remote server's current model list
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        filter: LaunchFilterArgs,
    },
}

impl LaunchCommand {
    #[allow(dead_code)]
    pub fn filter_args(&self) -> &LaunchFilterArgs {
        match self {
            Self::Codex { filter, .. }
            | Self::CodexDesktop { filter, .. }
            | Self::Pi { filter, .. }
            | Self::QwenCode { filter, .. }
            | Self::ClaudeCode { filter, .. }
            | Self::ClaudeDesktop { filter, .. }
            | Self::OpenCode { filter, .. } => filter,
        }
    }
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
    fn launch_opencode_command_parses_with_name_and_alias() {
        let cli = Cli::try_parse_from(["llm-proxy", "launch", "opencode"])
            .expect("parse launch opencode");
        assert!(matches!(
            cli.command,
            Some(Command::Launch {
                command: LaunchCommand::OpenCode { .. }
            })
        ));

        let cli_alias = Cli::try_parse_from(["llm-proxy", "launch", "open-code"])
            .expect("parse launch open-code");
        assert!(matches!(
            cli_alias.command,
            Some(Command::Launch {
                command: LaunchCommand::OpenCode { .. }
            })
        ));
    }

    #[test]
    fn top_level_dry_run_parses_without_subcommand() {
        let cli = Cli::try_parse_from(["llm-proxy", "--dry-run"]).expect("parse top-level dry-run");
        assert!(cli.dry_run);
        assert!(cli.command.is_none());
    }

    #[test]
    fn dry_run_as_global_flag_works_with_serve_subcommand() {
        let cli = Cli::try_parse_from(["llm-proxy", "serve", "--dry-run"])
            .expect("parse serve --dry-run");
        assert!(cli.dry_run);
        assert!(matches!(
            cli.command,
            Some(Command::Serve { frontend: false })
        ));
    }

    #[test]
    fn dry_run_before_subcommand_also_works() {
        let cli = Cli::try_parse_from(["llm-proxy", "--dry-run", "serve"])
            .expect("parse --dry-run before subcommand");
        assert!(cli.dry_run);
        assert!(matches!(
            cli.command,
            Some(Command::Serve { frontend: false })
        ));
    }

    #[test]
    fn cooldown_set_requires_provider_and_duration_model_is_optional() {
        assert!(Cli::try_parse_from(["llm-proxy", "cooldown", "set"]).is_err());
        assert!(Cli::try_parse_from(["llm-proxy", "cooldown", "set", "--provider", "p1"]).is_err());
        assert!(Cli::try_parse_from(["llm-proxy", "cooldown", "set", "--duration", "5m"]).is_err());

        // Model omitted: valid for provider-level cooldown
        match Cli::try_parse_from([
            "llm-proxy",
            "cooldown",
            "set",
            "--provider",
            "ubt-bailian",
            "--duration",
            "720h",
        ])
        .expect("parse cooldown set without model")
        .command
        .expect("command")
        {
            Command::Cooldown {
                command:
                    CooldownCommand::Set {
                        provider,
                        model,
                        duration,
                        protocol,
                        kind,
                        reason,
                        dry_run,
                        yes,
                    },
            } => {
                assert_eq!(provider, "ubt-bailian");
                assert_eq!(model, None);
                assert_eq!(duration, "720h");
                assert_eq!(protocol, None);
                assert_eq!(kind, None);
                assert_eq!(reason, None);
                assert!(!dry_run);
                assert!(!yes);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // All flags provided including --dry-run and -y
        match Cli::try_parse_from([
            "llm-proxy",
            "cooldown",
            "set",
            "--provider",
            "openai-sub",
            "--model",
            "gemini-3.8-flash-high-lp",
            "--duration",
            "42h",
            "--protocol",
            "anthropic",
            "--kind",
            "manual",
            "--reason",
            "quota exhausted",
            "--dry-run",
            "-y",
        ])
        .expect("parse cooldown set")
        .command
        .expect("command")
        {
            Command::Cooldown {
                command:
                    CooldownCommand::Set {
                        provider,
                        model,
                        duration,
                        protocol,
                        kind,
                        reason,
                        dry_run,
                        yes,
                    },
            } => {
                assert_eq!(provider, "openai-sub");
                assert_eq!(model.as_deref(), Some("gemini-3.8-flash-high-lp"));
                assert_eq!(duration, "42h");
                assert_eq!(protocol.as_deref(), Some("anthropic"));
                assert_eq!(kind.as_deref(), Some("manual"));
                assert_eq!(reason.as_deref(), Some("quota exhausted"));
                assert!(dry_run);
                assert!(yes);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cooldown_clear_parses_flags_and_supports_dry_run_and_yes() {
        match Cli::try_parse_from([
            "llm-proxy",
            "cooldown",
            "clear",
            "--provider",
            "ubt-bailian",
            "--model",
            "qwen-*",
            "--dry-run",
            "--yes",
        ])
        .expect("parse cooldown clear")
        .command
        .expect("command")
        {
            Command::Cooldown {
                command:
                    CooldownCommand::Clear {
                        provider,
                        model,
                        dry_run,
                        yes,
                    },
            } => {
                assert_eq!(provider, "ubt-bailian");
                assert_eq!(model.as_deref(), Some("qwen-*"));
                assert!(dry_run);
                assert!(yes);
            }
            other => panic!("unexpected command: {other:?}"),
        }
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
                select: None,
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
    fn model_select_parses_target_id() {
        match Cli::try_parse_from(["llm-proxy", "model", "--select", "deepseek-v4-pro-lp"])
            .expect("parse model --select")
            .command
            .expect("command")
        {
            Command::Model {
                select,
                command: None,
            } => {
                assert_eq!(select.as_deref(), Some("deepseek-v4-pro-lp"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["llm-proxy", "model", "--select", "x", "list"]).is_err(),
            "model --select cannot combine with a subcommand"
        );
    }

    #[test]
    fn model_rename_parses_old_and_new_ids() {
        match Cli::try_parse_from(["llm-proxy", "model", "rename", "my-model", "renamed-model"])
            .expect("parse model rename")
            .command
            .expect("command")
        {
            Command::Model {
                select: None,
                command:
                    Some(ModelCommand::Rename {
                        old_model,
                        new_model,
                    }),
            } => {
                assert_eq!(old_model, "my-model");
                assert_eq!(new_model, "renamed-model");
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(Cli::try_parse_from(["llm-proxy", "model", "rename", "only-old"]).is_err());
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
            "--force-no-think",
            "true",
            "--supports-thinking",
            "true",
            "--enable-feature",
            "image_input",
            "--enable-feature",
            "audio_input",
            "--disable-feature",
            "document_input",
            "--disable-feature",
            "video_input",
        ])
        .expect("parse model set")
        .command
        .expect("command")
        {
            Command::Model {
                select: None,
                command:
                    Some(ModelCommand::Set {
                        model,
                        context_window,
                        max_output,
                        thinking_level,
                        supported_thinking_levels,
                        supports_thinking,
                        force_no_think,
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
                assert_eq!(supports_thinking, Some(true));
                assert_eq!(force_no_think, Some(true));
                assert_eq!(enable_features, vec!["image_input", "audio_input"]);
                assert_eq!(disable_features, vec!["document_input", "video_input"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn model_set_parses_new_c1_fields_and_clear_flags() {
        // Test setting new fields
        match Cli::try_parse_from([
            "llm-proxy",
            "model",
            "set",
            "m1",
            "--description",
            "Production Qwen model",
            "--chat-reasoning-off-format",
            "enable-thinking",
            "--service-tier",
            "priority",
            "--default-tier",
            "--cooldown-network",
            "10",
            "--cooldown-server-error",
            "20",
            "--cooldown-rate-limit",
            "30",
            "--cooldown-model-unavailable",
            "40",
            "--cooldown-client-error",
            "50",
            "--cooldown-auth-error",
            "60",
        ])
        .expect("parse model set new fields")
        .command
        .expect("command")
        {
            Command::Model {
                select: None,
                command:
                    Some(ModelCommand::Set {
                        model,
                        description,
                        clear_description,
                        chat_reasoning_off_format,
                        clear_chat_reasoning_off_format,
                        service_tier,
                        default_tier,
                        forced_tier,
                        clear_service_tier,
                        cooldown_network,
                        cooldown_server_error,
                        cooldown_rate_limit,
                        cooldown_model_unavailable,
                        cooldown_client_error,
                        cooldown_auth_error,
                        clear_cooldown,
                        ..
                    }),
            } => {
                assert_eq!(model, "m1");
                assert_eq!(description.as_deref(), Some("Production Qwen model"));
                assert!(!clear_description);
                assert_eq!(
                    chat_reasoning_off_format.as_deref(),
                    Some("enable-thinking")
                );
                assert!(!clear_chat_reasoning_off_format);
                assert_eq!(service_tier.as_deref(), Some("priority"));
                assert!(default_tier);
                assert!(!forced_tier);
                assert!(!clear_service_tier);
                assert_eq!(cooldown_network, Some(10));
                assert_eq!(cooldown_server_error, Some(20));
                assert_eq!(cooldown_rate_limit, Some(30));
                assert_eq!(cooldown_model_unavailable, Some(40));
                assert_eq!(cooldown_client_error, Some(50));
                assert_eq!(cooldown_auth_error, Some(60));
                assert!(!clear_cooldown);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Test clear flags
        match Cli::try_parse_from([
            "llm-proxy",
            "model",
            "set",
            "m1",
            "--clear-description",
            "--clear-chat-reasoning-off-format",
            "--clear-service-tier",
            "--clear-cooldown",
        ])
        .expect("parse model set clear flags")
        .command
        .expect("command")
        {
            Command::Model {
                select: None,
                command:
                    Some(ModelCommand::Set {
                        model,
                        clear_description,
                        clear_chat_reasoning_off_format,
                        clear_service_tier,
                        clear_cooldown,
                        ..
                    }),
            } => {
                assert_eq!(model, "m1");
                assert!(clear_description);
                assert!(clear_chat_reasoning_off_format);
                assert!(clear_service_tier);
                assert!(clear_cooldown);
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
            "--codex-config-dir",
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
                        codex_config_dir,
                        codex_home: _,
                        dry_run,
                        check,
                        ..
                    },
            } => {
                assert_eq!(codex_config_dir, Some(PathBuf::from("/tmp/codex-desktop")));
                assert!(dry_run);
                assert!(!check);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Old flag rejection
        let err = Cli::try_parse_from([
            "llm-proxy",
            "launch",
            "codex-desktop",
            "--codex-home",
            "/tmp/codex-desktop",
        ])
        .unwrap_err();
        assert!(
            err.to_string().contains("--codex-config-dir"),
            "error should guide to --codex-config-dir: {err}"
        );
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
                select: None,
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
            Command::Status { probe, .. } => assert!(probe),
            other => panic!("unexpected command: {other:?}"),
        }
        // §12.4 不保留 --refresh（全新设计，无历史包袱）；必须拒绝解析
        assert!(
            Cli::try_parse_from(["llm-proxy", "status", "--refresh"]).is_err(),
            "--refresh should be rejected after removal"
        );
    }

    #[test]
    fn status_probe_selector_combinations_parse_success() {
        // Combination 1: probe only
        match Cli::try_parse_from(["llm-proxy", "status", "--probe"])
            .expect("parse --probe")
            .command
            .expect("command")
        {
            Command::Status {
                probe,
                provider,
                model,
            } => {
                assert!(probe);
                assert_eq!(provider, None);
                assert_eq!(model, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Combination 2: probe + provider
        match Cli::try_parse_from(["llm-proxy", "status", "--probe", "--provider", "deepseek"])
            .expect("parse --probe --provider")
            .command
            .expect("command")
        {
            Command::Status {
                probe,
                provider,
                model,
            } => {
                assert!(probe);
                assert_eq!(provider.as_deref(), Some("deepseek"));
                assert_eq!(model, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Combination 3: probe + model
        match Cli::try_parse_from([
            "llm-proxy",
            "status",
            "--probe",
            "--model",
            "deepseek-v4-lp",
        ])
        .expect("parse --probe --model")
        .command
        .expect("command")
        {
            Command::Status {
                probe,
                provider,
                model,
            } => {
                assert!(probe);
                assert_eq!(provider, None);
                assert_eq!(model.as_deref(), Some("deepseek-v4-lp"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Combination 4: probe + provider + model
        match Cli::try_parse_from([
            "llm-proxy",
            "status",
            "--probe",
            "--provider",
            "deepseek",
            "--model",
            "deepseek-v4-lp",
        ])
        .expect("parse --probe --provider --model")
        .command
        .expect("command")
        {
            Command::Status {
                probe,
                provider,
                model,
            } => {
                assert!(probe);
                assert_eq!(provider.as_deref(), Some("deepseek"));
                assert_eq!(model.as_deref(), Some("deepseek-v4-lp"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn status_selector_without_probe_flag_succeeds() {
        let cmd = Cli::try_parse_from(["llm-proxy", "status", "--provider", "deepseek"])
            .expect("--provider without --probe should parse successfully")
            .command
            .expect("command");
        match cmd {
            Command::Status {
                probe,
                provider,
                model,
            } => {
                assert!(!probe);
                assert_eq!(provider.as_deref(), Some("deepseek"));
                assert_eq!(model, None);
            }
            _ => panic!("expected Status command"),
        }

        let cmd = Cli::try_parse_from(["llm-proxy", "status", "--model", "deepseek-v4-lp"])
            .expect("--model without --probe should parse successfully")
            .command
            .expect("command");
        match cmd {
            Command::Status {
                probe,
                provider,
                model,
            } => {
                assert!(!probe);
                assert_eq!(provider, None);
                assert_eq!(model.as_deref(), Some("deepseek-v4-lp"));
            }
            _ => panic!("expected Status command"),
        }
    }

    #[test]
    fn status_selector_duplicate_flag_rejected() {
        assert!(
            Cli::try_parse_from([
                "llm-proxy",
                "status",
                "--probe",
                "--provider",
                "p1",
                "--provider",
                "p2",
            ])
            .is_err(),
            "duplicate --provider must fail to parse"
        );
        assert!(
            Cli::try_parse_from([
                "llm-proxy",
                "status",
                "--probe",
                "--model",
                "m1",
                "--model",
                "m2",
            ])
            .is_err(),
            "duplicate --model must fail to parse"
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

    #[test]
    fn model_add_copy_from_conflicts_with_window_and_upstream_flags() {
        // --copy-from with --upstream-model must fail parse
        assert!(
            Cli::try_parse_from([
                "llm-proxy",
                "model",
                "add",
                "m1",
                "--copy-from",
                "src",
                "--upstream-model",
                "up",
            ])
            .is_err(),
            "--copy-from combined with --upstream-model must fail to parse"
        );

        // --copy-from with --context-window must fail parse
        assert!(
            Cli::try_parse_from([
                "llm-proxy",
                "model",
                "add",
                "m1",
                "--copy-from",
                "src",
                "--context-window",
                "1000",
            ])
            .is_err(),
            "--copy-from combined with --context-window must fail to parse"
        );

        // --copy-from with --max-output must fail parse
        assert!(
            Cli::try_parse_from([
                "llm-proxy",
                "model",
                "add",
                "m1",
                "--copy-from",
                "src",
                "--max-output",
                "500",
            ])
            .is_err(),
            "--copy-from combined with --max-output must fail to parse"
        );

        // --upstream-model alone (without --from-discovery) must fail parse
        assert!(
            Cli::try_parse_from([
                "llm-proxy",
                "model",
                "add",
                "m1",
                "--upstream-model",
                "up",
                "--context-window",
                "1000",
                "--max-output",
                "500",
            ])
            .is_err(),
            "--upstream-model without --from-discovery must fail to parse"
        );

        // --copy-from alone parses successfully
        assert!(
            Cli::try_parse_from(["llm-proxy", "model", "add", "m1", "--copy-from", "src"]).is_ok(),
            "--copy-from alone must parse successfully"
        );

        // --from-discovery with --upstream-model parses successfully
        assert!(
            Cli::try_parse_from([
                "llm-proxy",
                "model",
                "add",
                "m1",
                "--from-discovery",
                "ollama",
                "--upstream-model",
                "up",
            ])
            .is_ok(),
            "--from-discovery with --upstream-model must parse successfully"
        );
    }

    #[test]
    fn model_provider_set_parses_arguments() {
        match Cli::try_parse_from([
            "llm-proxy",
            "model",
            "provider",
            "set",
            "my-model",
            "--type",
            "openai-chat",
            "--provider",
            "deepseek",
            "--upstream-model",
            "deepseek-v4-flash",
        ])
        .expect("parse model provider set")
        .command
        .expect("command")
        {
            Command::Model {
                select: None,
                command:
                    Some(ModelCommand::Provider {
                        command:
                            ModelProviderCommand::Set {
                                model,
                                provider_type,
                                provider,
                                upstream_model,
                            },
                    }),
            } => {
                assert_eq!(model, "my-model");
                assert_eq!(provider_type, "openai-chat");
                assert_eq!(provider, "deepseek");
                assert_eq!(upstream_model, "deepseek-v4-flash");
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Missing --upstream-model must fail parse
        assert!(
            Cli::try_parse_from([
                "llm-proxy",
                "model",
                "provider",
                "set",
                "my-model",
                "--type",
                "openai-chat",
                "--provider",
                "deepseek",
            ])
            .is_err(),
            "model provider set without --upstream-model must fail"
        );
    }
}
