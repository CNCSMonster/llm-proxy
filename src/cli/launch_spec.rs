use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Target operating system platform for config directory resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Windows,
    Linux,
}

impl Platform {
    /// Detect the platform of the current binary at runtime.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }
}

/// Single structured specification for a client's launch contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientLaunchSpec {
    pub client_name: &'static str,
    pub flag_name: &'static str,
    pub old_flags: &'static [&'static str],
    pub managed_files: &'static [&'static str],
    pub native_env: Option<&'static str>,
    pub shared_with: Option<&'static str>,
    pub probe_cmd: &'static str,
    pub probe_level: &'static str,
    pub default_dir_rule: &'static str,
    /// 隐式契约红线：在配置目录**之外**写文件（侧车文件）的规则。
    /// 空串表示无侧车文件——全部文件都在配置目录内部。
    pub sidecar_file_rule: &'static str,
}

pub const CODEX_SPEC: ClientLaunchSpec = ClientLaunchSpec {
    client_name: "codex",
    flag_name: "--codex-config-dir",
    old_flags: &["--codex-home"],
    managed_files: &["config.toml", "llm-proxy-<hash>-model-catalog.json"],
    native_env: Some("CODEX_HOME"),
    shared_with: None,
    probe_cmd: "CODEX_HOME=<dir> codex exec --skip-git-repo-check \"<最小请求>\"",
    probe_level: "探活",
    default_dir_rule: "$CODEX_HOME 若设置，否则 $HOME/.codex",
    sidecar_file_rule: "",
};

pub const CODEX_DESKTOP_SPEC: ClientLaunchSpec = ClientLaunchSpec {
    client_name: "codex-desktop",
    flag_name: "--codex-config-dir",
    old_flags: &["--codex-home"],
    managed_files: &["config.toml", "llm-proxy-<hash>-model-catalog.json"],
    native_env: Some("CODEX_HOME"),
    shared_with: Some("codex"),
    probe_cmd: "CODEX_HOME=<dir> codex exec --skip-git-repo-check \"<最小请求>\"",
    probe_level: "探活（复用 codex）",
    default_dir_rule: "$CODEX_HOME 若设置，否则 $HOME/.codex",
    sidecar_file_rule: "",
};

pub const PI_SPEC: ClientLaunchSpec = ClientLaunchSpec {
    client_name: "pi",
    flag_name: "--pi-config-dir",
    old_flags: &["--pi-home"],
    managed_files: &["models.json"],
    native_env: Some("PI_CODING_AGENT_DIR"),
    shared_with: None,
    probe_cmd: "PI_CODING_AGENT_DIR=<dir> pi -p --no-session --model <proxy/model> \"<最小请求>\"",
    probe_level: "探活",
    default_dir_rule: "$PI_CODING_AGENT_DIR 若设置，否则 $HOME/.pi/agent",
    sidecar_file_rule: "",
};

pub const QWEN_SPEC: ClientLaunchSpec = ClientLaunchSpec {
    client_name: "qwen-code",
    flag_name: "--qwen-config-dir",
    old_flags: &["--qwen-home"],
    managed_files: &["settings.json"],
    native_env: Some("QWEN_HOME"),
    shared_with: None,
    probe_cmd: "QWEN_HOME=<dir> qwen -m <model> -p \"<最小请求>\"",
    probe_level: "探活",
    default_dir_rule: "$QWEN_HOME 若设置，否则 $HOME/.qwen",
    sidecar_file_rule: "",
};

pub const CLAUDE_SPEC: ClientLaunchSpec = ClientLaunchSpec {
    client_name: "claude-code",
    flag_name: "--claude-config-dir",
    old_flags: &["--claude-home"],
    managed_files: &["settings.json"],
    native_env: Some("CLAUDE_CONFIG_DIR"),
    shared_with: None,
    probe_cmd: "CLAUDE_CONFIG_DIR=<dir> claude -p \"<最小请求>\"",
    probe_level: "探活",
    default_dir_rule: "$CLAUDE_CONFIG_DIR 若设置，否则 $HOME/.claude",
    // 唯一有侧车文件的客户端。官方语义（claude 2.1.293 实测）：
    // 设 CLAUDE_CONFIG_DIR=X → $X/.claude.json（内部）；不设 → $HOME/.claude.json（家目录根）。
    // 两条分支都不是「config_dir 的父目录」，禁止从路径形状反推——见 resolve_claude_code_layout。
    sidecar_file_rule: "<dir>/.claude.json（--claude-config-dir 给定或 $CLAUDE_CONFIG_DIR 已设时，在目录内部）；$HOME/.claude.json（全默认，家目录根）",
};

pub const OPENCODE_SPEC: ClientLaunchSpec = ClientLaunchSpec {
    client_name: "opencode",
    flag_name: "--opencode-config-dir",
    old_flags: &["--opencode-home"],
    managed_files: &["opencode.json"],
    native_env: Some("OPENCODE_CONFIG_DIR"),
    shared_with: None,
    probe_cmd: "OPENCODE_CONFIG_DIR=<dir> opencode run \"<最小请求>\"",
    probe_level: "探活",
    default_dir_rule: "$OPENCODE_CONFIG_DIR 若设置，否则 ${XDG_CONFIG_HOME:-$HOME/.config}/opencode",
    sidecar_file_rule: "",
};

pub const CLAUDE_DESKTOP_SPEC: ClientLaunchSpec = ClientLaunchSpec {
    client_name: "claude-desktop",
    flag_name: "--claude-desktop-config-dir",
    old_flags: &["--claude-desktop-home"],
    managed_files: &["claude_desktop_config.json"],
    native_env: None,
    shared_with: None,
    probe_cmd: "",
    probe_level: "仅写侧",
    default_dir_rule: "macOS ~/Library/Application Support/Claude/；Windows %APPDATA%\\Claude\\；Linux 未指定报错",
    sidecar_file_rule: "",
};

pub const ALL_SPECS: &[ClientLaunchSpec] = &[
    CODEX_SPEC,
    CODEX_DESKTOP_SPEC,
    PI_SPEC,
    QWEN_SPEC,
    CLAUDE_SPEC,
    OPENCODE_SPEC,
    CLAUDE_DESKTOP_SPEC,
];

impl ClientLaunchSpec {
    pub fn for_client(name: &str) -> Option<&'static ClientLaunchSpec> {
        ALL_SPECS.iter().find(|s| s.client_name == name)
    }

    /// Rejection error message for deprecated `--*-home` flags.
    pub fn old_flag_rejection_message(&self, old_flag: &str) -> String {
        if let Some(env) = self.native_env {
            format!(
                "{old_flag} was renamed to {} (the client's config directory, = {env})",
                self.flag_name
            )
        } else {
            format!(
                "{old_flag} was renamed to {} (the client's config directory; no native env, see platform paths)",
                self.flag_name
            )
        }
    }

    /// Pure function: resolve default config directory given platform, env snapshot, and home directory.
    /// Can be tested for macOS, Windows, and Linux branches on any host operating system.
    pub fn resolve_default_dir(
        &self,
        platform: Platform,
        env_snapshot: &HashMap<String, String>,
        home_dir: &Path,
    ) -> Result<PathBuf, String> {
        match self.client_name {
            "claude-desktop" => match platform {
                Platform::MacOs => Ok(home_dir
                    .join("Library")
                    .join("Application Support")
                    .join("Claude")),
                Platform::Windows => {
                    if let Some(appdata) = env_snapshot.get("APPDATA").filter(|s| !s.is_empty()) {
                        Ok(PathBuf::from(appdata).join("Claude"))
                    } else {
                        Ok(home_dir.join("AppData").join("Roaming").join("Claude"))
                    }
                }
                Platform::Linux => Err(
                    "Linux has no official Claude Desktop configuration directory; specify a custom target directory with --claude-desktop-config-dir <dir>".to_string(),
                ),
            },
            "opencode" => {
                if let Some(dir) = env_snapshot.get("OPENCODE_CONFIG_DIR").filter(|s| !s.is_empty()) {
                    Ok(PathBuf::from(dir))
                } else if let Some(xdg) = env_snapshot.get("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
                    Ok(PathBuf::from(xdg).join("opencode"))
                } else {
                    Ok(home_dir.join(".config").join("opencode"))
                }
            }
            "pi" => {
                if let Some(dir) = env_snapshot.get("PI_CODING_AGENT_DIR").filter(|s| !s.is_empty()) {
                    Ok(PathBuf::from(dir))
                } else {
                    Ok(home_dir.join(".pi").join("agent"))
                }
            }
            "codex" | "codex-desktop" => {
                if let Some(dir) = env_snapshot.get("CODEX_HOME").filter(|s| !s.is_empty()) {
                    Ok(PathBuf::from(dir))
                } else {
                    Ok(home_dir.join(".codex"))
                }
            }
            "qwen-code" => {
                if let Some(dir) = env_snapshot.get("QWEN_HOME").filter(|s| !s.is_empty()) {
                    Ok(PathBuf::from(dir))
                } else {
                    Ok(home_dir.join(".qwen"))
                }
            }
            "claude-code" => {
                if let Some(dir) = env_snapshot.get("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty()) {
                    Ok(PathBuf::from(dir))
                } else {
                    Ok(home_dir.join(".claude"))
                }
            }
            other => Err(format!("unknown client {other}")),
        }
    }
}

/// Helper to resolve the default configuration directory under the current environment.
pub fn resolve_current_default_dir(client_name: &str) -> Result<PathBuf, String> {
    let spec = ClientLaunchSpec::for_client(client_name)
        .ok_or_else(|| format!("unknown client {client_name}"))?;
    let platform = Platform::current();
    let env_snapshot = std::env::vars().collect();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    spec.resolve_default_dir(platform, &env_snapshot, &home)
}

/// claude-code 的目录布局——配置目录与侧车 onboarding 文件各在哪。
///
/// 官方语义（claude 2.1.293 实测，2026-10-13）：设 `CLAUDE_CONFIG_DIR=X` 时
/// `.claude.json` 在 **`X/` 内部**；不设时在 **`$HOME/` 根**（与 `~/.claude/` 同级）。
/// 两条分支都不是「`config_dir` 的父目录」——本函数即为消除旧的
/// `config_dir.parent()` 路径形状反推（隐式契约红线）。
///
/// 语义唯一来源是「客户端启动时带不带 `CLAUDE_CONFIG_DIR`」，而非路径形状：
/// `--claude-config-dir X` 在语义上等价于给客户端设 `CLAUDE_CONFIG_DIR=X`
/// （见 `CLAUDE_SPEC.native_env` / `probe_cmd`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeLayout {
    /// `settings.json` 所在目录。
    pub config_dir: PathBuf,
    /// 侧车 onboarding 文件 `hasCompletedOnboarding` 所在路径。
    pub onboarding_file: PathBuf,
}

/// 纯函数版本：由显式覆盖、环境快照与家目录确定布局，供单测直接驱动。
pub fn resolve_claude_code_layout(
    override_dir: Option<PathBuf>,
    env_snapshot: &HashMap<String, String>,
    home_dir: &Path,
) -> Result<ClaudeLayout, String> {
    let spec = ClientLaunchSpec::for_client("claude-code")
        .ok_or_else(|| "unknown client claude-code".to_string())?;
    let env_key = spec.native_env.unwrap_or("CLAUDE_CONFIG_DIR");
    match override_dir {
        // ① 显式 --claude-config-dir ⟺ 客户端带 CLAUDE_CONFIG_DIR=<dir> 启动 → 侧车文件在目录内部
        Some(dir) => Ok(ClaudeLayout {
            onboarding_file: dir.join(".claude.json"),
            config_dir: dir,
        }),
        None => {
            let config_dir =
                spec.resolve_default_dir(Platform::current(), env_snapshot, home_dir)?;
            let onboarding_file = match env_snapshot.get(env_key).filter(|v| !v.is_empty()) {
                // ② 用户已设 $CLAUDE_CONFIG_DIR ⟺ 客户端同样带它启动 → 侧车文件在目录内部
                Some(_) => config_dir.join(".claude.json"),
                // ③ 全默认 ⟺ 客户端不带 env → 侧车文件在家目录根
                None => home_dir.join(".claude.json"),
            };
            Ok(ClaudeLayout {
                config_dir,
                onboarding_file,
            })
        }
    }
}

/// 读真实进程环境的入口，供 `launch_claude_code` 使用。
pub fn resolve_current_claude_layout(
    override_dir: Option<PathBuf>,
) -> Result<ClaudeLayout, String> {
    let env_snapshot = std::env::vars().collect();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    resolve_claude_code_layout(override_dir, &env_snapshot, &home)
}

/// Generic rejection helper for Clap value parser.
pub fn reject_old_flag(client_name: &str, old_flag: &str) -> Result<String, String> {
    let spec = ClientLaunchSpec::for_client(client_name)
        .ok_or_else(|| format!("unknown client {client_name}"))?;
    Err(spec.old_flag_rejection_message(old_flag))
}

pub fn reject_codex_home(_: &str) -> Result<String, String> {
    reject_old_flag("codex", "--codex-home")
}

pub fn reject_pi_home(_: &str) -> Result<String, String> {
    reject_old_flag("pi", "--pi-home")
}

pub fn reject_qwen_home(_: &str) -> Result<String, String> {
    reject_old_flag("qwen-code", "--qwen-home")
}

pub fn reject_claude_home(_: &str) -> Result<String, String> {
    reject_old_flag("claude-code", "--claude-home")
}

pub fn reject_claude_desktop_home(_: &str) -> Result<String, String> {
    reject_old_flag("claude-desktop", "--claude-desktop-home")
}

pub fn reject_opencode_home(_: &str) -> Result<String, String> {
    reject_old_flag("opencode", "--opencode-home")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_branches_resolution() {
        let home = Path::new("/custom/home");
        let empty_env = HashMap::new();

        // 1. claude-desktop: macOS, Windows, Linux
        let desktop_spec = ClientLaunchSpec::for_client("claude-desktop").unwrap();
        assert_eq!(
            desktop_spec
                .resolve_default_dir(Platform::MacOs, &empty_env, home)
                .unwrap(),
            home.join("Library/Application Support/Claude")
        );

        let mut win_env = HashMap::new();
        win_env.insert("APPDATA".to_string(), "/custom/AppData/Roaming".to_string());
        assert_eq!(
            desktop_spec
                .resolve_default_dir(Platform::Windows, &win_env, home)
                .unwrap(),
            PathBuf::from("/custom/AppData/Roaming/Claude")
        );

        // Linux without explicit dir must error
        let linux_err = desktop_spec
            .resolve_default_dir(Platform::Linux, &empty_env, home)
            .unwrap_err();
        assert!(linux_err.contains("Linux has no official Claude Desktop configuration directory"));

        // 2. opencode: OPENCODE_CONFIG_DIR > XDG_CONFIG_HOME > $HOME/.config/opencode
        let opencode_spec = ClientLaunchSpec::for_client("opencode").unwrap();
        assert_eq!(
            opencode_spec
                .resolve_default_dir(Platform::Linux, &empty_env, home)
                .unwrap(),
            home.join(".config/opencode")
        );

        let mut xdg_env = HashMap::new();
        xdg_env.insert("XDG_CONFIG_HOME".to_string(), "/custom/xdg".to_string());
        assert_eq!(
            opencode_spec
                .resolve_default_dir(Platform::Linux, &xdg_env, home)
                .unwrap(),
            PathBuf::from("/custom/xdg/opencode")
        );

        let mut opencode_env = HashMap::new();
        opencode_env.insert(
            "OPENCODE_CONFIG_DIR".to_string(),
            "/custom/opencode".to_string(),
        );
        assert_eq!(
            opencode_spec
                .resolve_default_dir(Platform::Linux, &opencode_env, home)
                .unwrap(),
            PathBuf::from("/custom/opencode")
        );

        // 3. pi: PI_CODING_AGENT_DIR > $HOME/.pi/agent
        let pi_spec = ClientLaunchSpec::for_client("pi").unwrap();
        assert_eq!(
            pi_spec
                .resolve_default_dir(Platform::Linux, &empty_env, home)
                .unwrap(),
            home.join(".pi/agent")
        );
        let mut pi_env = HashMap::new();
        pi_env.insert(
            "PI_CODING_AGENT_DIR".to_string(),
            "/custom/pi/agent".to_string(),
        );
        assert_eq!(
            pi_spec
                .resolve_default_dir(Platform::Linux, &pi_env, home)
                .unwrap(),
            PathBuf::from("/custom/pi/agent")
        );

        // 4. codex: CODEX_HOME > $HOME/.codex
        let codex_spec = ClientLaunchSpec::for_client("codex").unwrap();
        assert_eq!(
            codex_spec
                .resolve_default_dir(Platform::Linux, &empty_env, home)
                .unwrap(),
            home.join(".codex")
        );
        let mut codex_env = HashMap::new();
        codex_env.insert("CODEX_HOME".to_string(), "/custom/codex".to_string());
        assert_eq!(
            codex_spec
                .resolve_default_dir(Platform::Linux, &codex_env, home)
                .unwrap(),
            PathBuf::from("/custom/codex")
        );

        // 5. qwen-code: QWEN_HOME > $HOME/.qwen
        let qwen_spec = ClientLaunchSpec::for_client("qwen-code").unwrap();
        assert_eq!(
            qwen_spec
                .resolve_default_dir(Platform::Linux, &empty_env, home)
                .unwrap(),
            home.join(".qwen")
        );
        let mut qwen_env = HashMap::new();
        qwen_env.insert("QWEN_HOME".to_string(), "/custom/qwen".to_string());
        assert_eq!(
            qwen_spec
                .resolve_default_dir(Platform::Linux, &qwen_env, home)
                .unwrap(),
            PathBuf::from("/custom/qwen")
        );

        // 6. claude-code: CLAUDE_CONFIG_DIR > $HOME/.claude
        let claude_spec = ClientLaunchSpec::for_client("claude-code").unwrap();
        assert_eq!(
            claude_spec
                .resolve_default_dir(Platform::Linux, &empty_env, home)
                .unwrap(),
            home.join(".claude")
        );
        let mut claude_env = HashMap::new();
        claude_env.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/custom/claude".to_string(),
        );
        assert_eq!(
            claude_spec
                .resolve_default_dir(Platform::Linux, &claude_env, home)
                .unwrap(),
            PathBuf::from("/custom/claude")
        );
    }

    #[test]
    fn test_old_flags_rejection_messages() {
        for spec in ALL_SPECS {
            for old_flag in spec.old_flags {
                let msg = spec.old_flag_rejection_message(old_flag);
                assert!(
                    msg.contains(old_flag),
                    "{msg} should contain old flag {old_flag}"
                );
                assert!(
                    msg.contains(spec.flag_name),
                    "{msg} should contain new flag {}",
                    spec.flag_name
                );
                if let Some(env) = spec.native_env {
                    assert!(msg.contains(env), "{msg} should contain env {env}");
                } else {
                    assert!(
                        msg.contains("no native env"),
                        "{msg} should mention no native env"
                    );
                }
            }
        }
    }

    #[test]
    fn test_launch_help_output_consistency() {
        use clap::CommandFactory;
        let mut cmd = crate::cli::Cli::command();
        cmd.build();
        let launch_cmd = cmd
            .find_subcommand_mut("launch")
            .expect("launch subcommand");
        let help_str = launch_cmd.render_long_help().to_string();

        assert!(
            help_str.contains("所有 `--<client>-config-dir <dir>` 参数指向该客户端的配置目录本身"),
            "help should contain standard unified mouth sentence"
        );
        assert!(
            help_str.contains("不是 `$HOME`"),
            "help should clarify not $HOME"
        );

        for spec in ALL_SPECS {
            let mut cmd_inner = crate::cli::Cli::command();
            let launch_inner = cmd_inner.find_subcommand_mut("launch").expect("launch");
            let sub_cmd = launch_inner
                .find_subcommand_mut(spec.client_name)
                .unwrap_or_else(|| panic!("subcommand {}", spec.client_name));
            let sub_help = sub_cmd.render_long_help().to_string();
            assert!(
                sub_help.contains(spec.flag_name),
                "launch {} help should document flag {}",
                spec.client_name,
                spec.flag_name
            );
            if let Some(env) = spec.native_env {
                assert!(
                    sub_help.contains(env),
                    "launch {} help should document native env {env}",
                    spec.client_name
                );
            }
        }
    }

    #[test]
    fn test_old_flags_parsing_rejected_parameterized() {
        use clap::Parser;
        for spec in ALL_SPECS {
            for old_flag in spec.old_flags {
                let res = crate::cli::Cli::try_parse_from([
                    "llm-proxy",
                    "launch",
                    spec.client_name,
                    old_flag,
                    "/tmp/dummy-dir",
                ]);
                assert!(
                    res.is_err(),
                    "expected rejection for {old_flag} on client {}",
                    spec.client_name
                );
                let err_msg = res.unwrap_err().to_string();
                assert!(
                    err_msg.contains(spec.flag_name),
                    "error message '{err_msg}' should point to new flag {}",
                    spec.flag_name
                );
                assert!(
                    err_msg.contains(old_flag),
                    "error message '{err_msg}' should mention old flag {old_flag}"
                );
            }
        }
    }

    #[test]
    fn test_doc_grep_consistency() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let doc_path = manifest_dir.join("docs/user_guide/cli-guide.md");
        if doc_path.exists() {
            let doc_content = std::fs::read_to_string(&doc_path).expect("read cli-guide.md");
            for spec in ALL_SPECS {
                assert!(
                    doc_content.contains(spec.flag_name),
                    "doc {} should contain flag {}",
                    doc_path.display(),
                    spec.flag_name
                );
                // 隐式契约红线：凡登记了侧车文件规则的客户端，
                // 用户手册必须写明该侧车文件的位置与语义——把「代码登记了 spec
                // 但手册遗漏」从人工纪律变成机器门禁。
                if !spec.sidecar_file_rule.is_empty() {
                    let sidecar = ".claude.json";
                    assert!(
                        doc_content.contains(sidecar),
                        "doc {} declares sidecar rule for {} but never mentions {} \
                         (机制文档义务)",
                        doc_path.display(),
                        spec.client_name,
                        sidecar
                    );
                }
            }
        }
    }

    // ── ：claude-code 目录布局三分支 ─────────────────────────────
    //
    // 官方语义实测（claude 2.1.293，2026-10-13）：设 CLAUDE_CONFIG_DIR=X 时
    // `.claude.json` 在 $X/ 内部；不设时在 $HOME/ 根。两条分支都不是父目录。

    fn empty_env() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn claude_layout_default_puts_onboarding_in_home_root() {
        let home = Path::new("/custom/home");
        let l = resolve_claude_code_layout(None, &empty_env(), home).expect("layout");
        assert_eq!(l.config_dir, PathBuf::from("/custom/home/.claude"));
        assert_eq!(
            l.onboarding_file,
            PathBuf::from("/custom/home/.claude.json")
        );
    }

    #[test]
    fn claude_layout_env_config_dir_puts_onboarding_inside_dir() {
        let home = Path::new("/custom/home");
        let mut env = empty_env();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), "/exp/ccfg".to_string());
        let l = resolve_claude_code_layout(None, &env, home).expect("layout");
        assert_eq!(l.config_dir, PathBuf::from("/exp/ccfg"));
        assert_eq!(l.onboarding_file, PathBuf::from("/exp/ccfg/.claude.json"));
    }

    /// 核心回归基线：显式 override 时侧车文件必须在**目录内部**。
    /// 旧实现 `config_dir.parent()/.claude.json` 会得到 /exp/.claude.json（逃逸）。
    #[test]
    fn claude_layout_explicit_override_puts_onboarding_inside_dir() {
        let home = Path::new("/custom/home");
        let l =
            resolve_claude_code_layout(Some(PathBuf::from("/isolated/claude")), &empty_env(), home)
                .expect("layout");
        assert_eq!(l.config_dir, PathBuf::from("/isolated/claude"));
        assert_eq!(
            l.onboarding_file,
            PathBuf::from("/isolated/claude/.claude.json"),
            "侧车文件必须在 override 目录内部，不得逃逸到父目录"
        );
    }

    #[test]
    fn claude_layout_explicit_override_wins_over_env() {
        let home = Path::new("/custom/home");
        let mut env = empty_env();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), "/from/env".to_string());
        let l = resolve_claude_code_layout(Some(PathBuf::from("/from/flag")), &env, home)
            .expect("layout");
        assert_eq!(l.config_dir, PathBuf::from("/from/flag"));
        assert_eq!(l.onboarding_file, PathBuf::from("/from/flag/.claude.json"));
    }

    #[test]
    fn claude_layout_empty_env_counts_as_unset() {
        let home = Path::new("/custom/home");
        let mut env = empty_env();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), String::new());
        let l = resolve_claude_code_layout(None, &env, home).expect("layout");
        assert_eq!(
            l.onboarding_file,
            PathBuf::from("/custom/home/.claude.json")
        );
    }

    /// 隐式契约红线：侧车文件规则必须在 spec 中显式登记（审计门禁）。
    #[test]
    fn only_claude_code_declares_a_sidecar_file_rule() {
        for spec in ALL_SPECS {
            if spec.client_name == "claude-code" {
                assert!(
                    !spec.sidecar_file_rule.is_empty(),
                    "claude-code 有侧车 .claude.json，必须登记规则"
                );
            } else {
                assert_eq!(
                    spec.sidecar_file_rule, "",
                    "{} 不应声明侧车文件（已审计：均只往目录内写单文件）",
                    spec.client_name
                );
            }
        }
    }
}
