//! Shell completion script generation and completion candidate listing.
//!
//! `llm-proxy completion <shell>` emits the static clap_complete script plus a
//! dynamic snippet that completes `--provider` / `--model` from the config.
//! The hidden `complete-candidates` subcommand feeds that snippet: it prints
//! one candidate per line, and the shell wrapper delegates back to the static
//! completion function for everything else.

use std::path::Path;

use clap::CommandFactory;

use super::types::{Cli, CompleteCandidatesKind, CompletionShell};

/// Bash: wrapper function that intercepts `--provider` / `--model` and
/// delegates everything else to the static completion function `_llm-proxy`
/// (name emitted by clap_complete for the `llm-proxy` binary).
///
/// 额外兜底：clap_complete 对含连字符的二进制名（llm-proxy）生成的 bash
/// 脚本中，子命令 case 标签与 cmd 推导不一致（bin_name 的 `-` 被替换为
/// `__subcmd__`，而 fn_name 的 `-` 被替换为 `__`），导致 usage 等子命令的
/// 静态选项补全永远无法命中。这里为 usage 补齐选项名及枚举值补全。
const BASH_DYNAMIC_SNIPPET: &str = r#"# --- llm-proxy: dynamic --provider/--model candidates (from config.toml) ---
_llm-proxy-dynamic() {
    local cur prev
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    case "${prev}" in
        --provider)
            COMPREPLY=($(compgen -W "$(llm-proxy complete-candidates provider 2>/dev/null)" -- "${cur}"))
            return 0
            ;;
        --model)
            COMPREPLY=($(compgen -W "$(llm-proxy complete-candidates model 2>/dev/null)" -- "${cur}"))
            return 0
            ;;
        --endpoint)
            COMPREPLY=($(compgen -W "openai-chat openai-responses anthropic" -- "${cur}"))
            return 0
            ;;
        --view)
            COMPREPLY=($(compgen -W "by-model by-provider by-endpoint by-hour by-day" -- "${cur}"))
            return 0
            ;;
    esac
    if [[ "${COMP_WORDS[1]}" == "usage" && ( "${cur}" == -* || ${COMP_CWORD} -eq 2 ) ]] ; then
        COMPREPLY=($(compgen -W "-p -h --period --provider --model --endpoint --view --json --help" -- "${cur}"))
        return 0
    fi
    _llm-proxy "$@"
}
if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _llm-proxy-dynamic -o nosort -o bashdefault -o default llm-proxy
else
    complete -F _llm-proxy-dynamic -o bashdefault -o default llm-proxy
fi
"#;

/// Zsh: same interception pattern. `$words` / `$CURRENT` are provided by the
/// completion system; everything else delegates to the static `_llm-proxy`.
const ZSH_DYNAMIC_SNIPPET: &str = r#"# --- llm-proxy: dynamic --provider/--model candidates (from config.toml) ---
_llm-proxy-dynamic() {
    local prev="${words[CURRENT-1]}"
    case "${prev}" in
        --provider)
            local -a candidates
            candidates=($(llm-proxy complete-candidates provider 2>/dev/null))
            _describe 'provider' candidates
            ;;
        --model)
            local -a candidates
            candidates=($(llm-proxy complete-candidates model 2>/dev/null))
            _describe 'model' candidates
            ;;
        *)
            _llm-proxy "$@"
            ;;
    esac
}
compdef _llm-proxy-dynamic llm-proxy
"#;

/// Generate the full completion script for a shell: the static
/// clap_complete script followed by the dynamic candidates snippet
/// (bash/zsh only; other shells keep the static script unchanged).
pub fn generate_completion_script(shell: CompletionShell) -> String {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    let generator: clap_complete::Shell = shell.into();
    let mut buf = Vec::new();
    clap_complete::generate(generator, &mut cmd, name, &mut buf);
    let static_script = String::from_utf8(buf).unwrap_or_default();
    let dynamic = match shell {
        CompletionShell::Bash => BASH_DYNAMIC_SNIPPET,
        CompletionShell::Zsh => ZSH_DYNAMIC_SNIPPET,
        CompletionShell::Fish | CompletionShell::PowerShell | CompletionShell::Elvish => "",
    };
    format!("{static_script}{dynamic}")
}

/// Collect completion candidates of a kind, preferring a running server.
///
/// Remote-first: if `detect_server` finds a compatible server (local or
/// container→host), delegate via HTTP public endpoints (`/admin/provider/list`
/// and `/admin/model/list`). Otherwise fall back to reading the local
/// config.toml.
///
/// Returns one candidate per line (trailing newline when non-empty). A
/// missing or unparseable config yields an empty string: completion helpers
/// must stay silent instead of failing loudly during interactive use. The
/// error is still reported on stderr (discarded by the shell snippets via
/// `2>/dev/null`).
pub async fn complete_candidates(config_path: &Path, kind: CompleteCandidatesKind) -> String {
    // 远程优先：有 server 时走 HTTP 公开接口委托
    // Ok(None): server 未运行；Err: 版本不兼容。补全场景静默回退本地 config。
    if let Ok(Some(server)) = crate::admin_client::detect_server(config_path).await {
        let candidates: Vec<String> = match kind {
            CompleteCandidatesKind::Provider => match server.list_providers().await {
                Ok(ids) => ids,
                Err(e) => {
                    eprintln!("failed to fetch provider candidates from server: {e:#}");
                    return String::new();
                }
            },
            CompleteCandidatesKind::Model => match server.model_list().await {
                Ok(value) => model_ids_from_json(&value),
                Err(e) => {
                    eprintln!("failed to fetch model candidates from server: {e:#}");
                    return String::new();
                }
            },
        };
        return join_candidates(&candidates);
    }
    let cfg = match crate::config::Config::load(config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("failed to load config for completion candidates: {e:#}");
            return String::new();
        }
    };
    let candidates: Vec<String> = match kind {
        CompleteCandidatesKind::Provider => cfg.providers.keys().cloned().collect(),
        CompleteCandidatesKind::Model => cfg.models.keys().cloned().collect(),
    };
    join_candidates(&candidates)
}

/// 从 `/admin/model/list` 响应中提取 model id 列表。
fn model_ids_from_json(value: &serde_json::Value) -> Vec<String> {
    value
        .get("data")
        .and_then(|d| d.get("models"))
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// 检查候选项是否只包含安全字符（字母、数字、下划线、连字符、点）。
///
/// 防止 shell 注入：provider/model id 会拼接到 shell 补全脚本中，
/// 如果包含空格、分号、反引号等特殊字符，可能导致命令注入或拆词错误。
fn is_safe_candidate(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn join_candidates(candidates: &[String]) -> String {
    let mut safe: Vec<&String> = candidates.iter().filter(|c| is_safe_candidate(c)).collect();
    safe.sort();
    let mut out = safe
        .into_iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const MINIMAL_CONFIG: &str = r#"
[server]
listen = "127.0.0.1:1"

[providers.alpha]
[providers.alpha.openai_chat]
url = "https://example.test/v1/chat/completions"

[providers.beta]
[providers.beta.anthropic]
url = "https://example.test/v1/messages"

[models.model-a]
context_window = 100000
max_output_tokens = 8000
openai_chat_providers = [
    { name = "alpha", model = "alpha-upstream" }
]

[models.model-b]
context_window = 100000
max_output_tokens = 8000
anthropic_providers = [
    { name = "beta", model = "beta-upstream" }
]
"#;

    fn write_config(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).expect("create config");
        f.write_all(body.as_bytes()).expect("write config");
        path
    }

    /// Spawn a mock admin server with the standard completion endpoints.
    /// Returns the bound address; the server runs on a background task.
    async fn spawn_completion_mock_server(
        providers: Vec<&str>,
        models: Vec<&str>,
    ) -> std::net::SocketAddr {
        let provider_body = serde_json::json!({
            "status": "ok",
            "data": { "providers": providers },
        });
        let model_body = serde_json::json!({
            "status": "ok",
            "data": {
                "models": models
                    .iter()
                    .map(|id| serde_json::json!({ "id": id, "context_window": 1000, "max_output_tokens": 100, "protocols": [] }))
                    .collect::<Vec<_>>(),
            },
        });
        let ping_body = serde_json::json!({ "status": "ok", "version": "0.2.5" });
        let app = axum::Router::new()
            .route(
                "/admin/ping",
                axum::routing::get({
                    let body = ping_body.clone();
                    move || {
                        let body = body.clone();
                        async move { axum::Json(body) }
                    }
                }),
            )
            .route(
                "/admin/provider/list",
                axum::routing::get({
                    let body = provider_body.clone();
                    move || {
                        let body = body.clone();
                        async move { axum::Json(body) }
                    }
                }),
            )
            .route(
                "/admin/model/list",
                axum::routing::get({
                    let body = model_body.clone();
                    move || {
                        let body = body.clone();
                        async move { axum::Json(body) }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock admin server");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock admin");
        });
        addr
    }

    #[tokio::test]
    async fn complete_candidates_lists_providers_and_models() {
        // listen = 127.0.0.1:1 (不可达) → detect_server 返回 Ok(None) → 回退本地 config
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(&dir, MINIMAL_CONFIG);
        assert_eq!(
            complete_candidates(&path, CompleteCandidatesKind::Provider).await,
            "alpha\nbeta\n"
        );
        assert_eq!(
            complete_candidates(&path, CompleteCandidatesKind::Model).await,
            "model-a\nmodel-b\n"
        );
    }

    #[tokio::test]
    async fn complete_candidates_missing_config_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        assert_eq!(
            complete_candidates(&path, CompleteCandidatesKind::Provider).await,
            ""
        );
        assert_eq!(
            complete_candidates(&path, CompleteCandidatesKind::Model).await,
            ""
        );
    }

    #[tokio::test]
    async fn complete_candidates_prefers_remote_server_for_providers() {
        // 远程 server 返回 remote-p1/remote-p2，本地 config 有 alpha/beta
        // → 远程优先：应返回远程候选
        let addr = spawn_completion_mock_server(vec!["remote-p1", "remote-p2"], vec![]).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let config_body = MINIMAL_CONFIG.replace("127.0.0.1:1", &addr.to_string());
        let path = write_config(&dir, &config_body);
        assert_eq!(
            complete_candidates(&path, CompleteCandidatesKind::Provider).await,
            "remote-p1\nremote-p2\n"
        );
    }

    #[tokio::test]
    async fn complete_candidates_prefers_remote_server_for_models() {
        let addr = spawn_completion_mock_server(vec![], vec!["remote-m1", "remote-m2"]).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let config_body = MINIMAL_CONFIG.replace("127.0.0.1:1", &addr.to_string());
        let path = write_config(&dir, &config_body);
        assert_eq!(
            complete_candidates(&path, CompleteCandidatesKind::Model).await,
            "remote-m1\nremote-m2\n"
        );
    }

    #[test]
    fn completion_script_bash_includes_dynamic_snippet() {
        let script = generate_completion_script(CompletionShell::Bash);
        assert!(script.contains("complete-candidates provider"));
        assert!(script.contains("complete-candidates model"));
        assert!(script.contains("_llm-proxy-dynamic"));
        assert!(script.contains("complete -F _llm-proxy-dynamic"));
        // 静态脚本仍在（委托目标存在）
        assert!(script.contains("_llm-proxy()"));
    }

    #[test]
    fn completion_script_zsh_includes_dynamic_snippet() {
        let script = generate_completion_script(CompletionShell::Zsh);
        assert!(script.contains("complete-candidates provider"));
        assert!(script.contains("complete-candidates model"));
        assert!(script.contains("_llm-proxy-dynamic"));
        assert!(script.contains("compdef _llm-proxy-dynamic llm-proxy"));
        // 静态脚本仍在
        assert!(script.contains("#compdef llm-proxy"));
    }

    #[test]
    fn completion_script_fish_has_no_dynamic_snippet() {
        let script = generate_completion_script(CompletionShell::Fish);
        // fish 不追加动态片段（bash/zsh 专属），静态脚本保持原样
        assert!(!script.contains("_llm-proxy-dynamic"));
        assert!(!script.contains("complete-candidates provider"));
    }

    #[test]
    fn is_safe_candidate_filters_special_chars() {
        assert!(is_safe_candidate("provider-name"));
        assert!(is_safe_candidate("model_v1.0"));
        assert!(is_safe_candidate("openai123"));
        assert!(!is_safe_candidate("provider; rm -rf /"));
        assert!(!is_safe_candidate("model`whoami`"));
        assert!(!is_safe_candidate("name with space"));
        assert!(!is_safe_candidate("test$(command)"));
        assert!(!is_safe_candidate("back\\slash"));
    }

    #[test]
    fn join_candidates_filters_and_sorts() {
        let candidates = vec![
            "safe-name".to_string(),
            "bad;name".to_string(),
            "another_safe.one".to_string(),
            "model`cmd`".to_string(),
            "zebra".to_string(),
            "alpha".to_string(),
        ];
        let result = join_candidates(&candidates);
        // 只保留安全候选，且按字母排序
        assert_eq!(result, "alpha\nanother_safe.one\nsafe-name\nzebra\n");
    }
}
