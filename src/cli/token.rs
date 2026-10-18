//! `llm-proxy token` command handler (§5, delegation).
//!
//! Writes use delegation pattern: if server is alive, delegate via UDS;
//! otherwise, acquire ownership lock and write locally (§5.1).

use std::path::Path;

use anyhow::{Result, bail};

use crate::service;
use crate::token::config::TokenConfig;
use crate::token::manage::{self, CreateArgs, NewQuotaWindow};
use crate::token::usage::WindowUsage;

use super::types::{TokenCommand, TokenCreateArgs};

pub async fn run_token_command(config_path: &Path, command: TokenCommand) -> Result<()> {
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let dirs = service::StateDirs::resolve(config_dir);
    let state_dir = dirs.state_dir;
    match command {
        TokenCommand::Create(args) => {
            let create_args = build_create_args(args)?;
            // §15.2 五步流程：server 存活则委托，否则本地写
            let token = crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy token create",
                // Local write (server not alive)
                || {
                    let config_path = config_path.to_path_buf();
                    let args = create_args.clone();
                    // 上面的 ownership flock 只管跨进程这一半；`lock_token_store`
                    // 补上进程内这一半，它同时覆盖 server 自己的并发 handler
                    async move {
                        let token_store = manage::lock_token_store();
                        manage::create_token(&token_store, &config_path, args)
                    }
                },
                // Delegate to server
                |server| {
                    let args = create_args.clone();
                    Box::pin(async move {
                        let result = server
                            .token_create(
                                &args.name,
                                &args.models,
                                &args.windows,
                                args.expires.as_deref(),
                            )
                            .await?;
                        // Server responds with AdminApiResponse { status, data, ... };
                        // token fields live under "data". Decoding is strict: a
                        // missing or malformed field is an error, never a default.
                        created_token_from_response(&result)
                    })
                },
            )
            .await?;
            println!("Created token '{}'", token.name);
            println!("  key: {}", token.key);
            println!("  models: {}", describe_models(&token));
            println!("  quota: {}", describe_windows_flat(&token));
            if let Some(exp) = &token.expired_at {
                println!("  expires: {exp}");
            }
        }
        TokenCommand::List { json } => {
            let rows = manage::list(config_path, &state_dir)?;
            if json {
                let value: Vec<serde_json::Value> =
                    rows.iter().map(|(t, u)| token_json(t, u)).collect();
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if rows.is_empty() {
                println!("no tokens configured (open mode: all requests allowed)");
            } else {
                print_list_table(&rows);
            }
        }
        TokenCommand::Usage { key } => {
            let (token, usages) = manage::usage(config_path, &state_dir, &key)?;
            print_usage_detail(&token, &usages);
        }
        TokenCommand::Delete { key } => {
            // §15.2 五步流程：server 存活则委托，否则本地写
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy token delete",
                // Local write (server not alive)
                || {
                    let config_path = config_path.to_path_buf();
                    let state_dir = state_dir.to_path_buf();
                    let key = key.clone();
                    async move {
                        let token_store = manage::lock_token_store();
                        manage::delete_token(&token_store, &config_path, &state_dir, &key)
                    }
                },
                // Delegate to server
                |server| {
                    let key = key.clone();
                    Box::pin(async move {
                        server.token_delete(&key).await?;
                        Ok(())
                    })
                },
            )
            .await?;
            println!("Deleted token '{key}'");
        }
        TokenCommand::ResetUsage { key } => {
            // §15.2 五步流程：server 存活则委托，否则本地写
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy token reset-usage",
                // Local write (server not alive)
                || {
                    let config_path = config_path.to_path_buf();
                    let state_dir = state_dir.to_path_buf();
                    let key = key.clone();
                    async move { manage::reset_usage(&config_path, &state_dir, &key) }
                },
                // Delegate to server
                |server| {
                    let key = key.clone();
                    Box::pin(async move {
                        server.token_reset_usage(&key).await?;
                        Ok(())
                    })
                },
            )
            .await?;
            println!("Reset usage counters for token '{key}'");
        }
        TokenCommand::Rename { key, new_name } => {
            // §15.2 五步流程：server 存活则委托，否则本地写
            crate::ownership::with_cli_write_lock_or_delegate(
                config_path,
                "llm-proxy token rename",
                // Local write (server not alive)
                || {
                    let config_path = config_path.to_path_buf();
                    let state_dir = state_dir.to_path_buf();
                    let key = key.clone();
                    let new_name = new_name.clone();
                    async move {
                        let token_store = manage::lock_token_store();
                        manage::rename_token(
                            &token_store,
                            &config_path,
                            &state_dir,
                            &key,
                            &new_name,
                        )
                    }
                },
                // Delegate to server
                |server| {
                    let key = key.clone();
                    let new_name = new_name.clone();
                    Box::pin(async move {
                        server.token_rename(&key, &new_name).await?;
                        Ok(())
                    })
                },
            )
            .await?;
            println!("Renamed token '{key}' to '{new_name}'");
        }
        TokenCommand::Migrate => {
            manage::migrate_to_directory(config_path)?;
        }
    }
    Ok(())
}

/// Convert `--window/--quota-mode/--quota-limit` parallel vectors into windows.
fn build_create_args(args: TokenCreateArgs) -> Result<CreateArgs> {
    let n = args.window.len();
    if args.quota_mode.len() != n || args.quota_limit.len() != n {
        bail!(
            "--window, --quota-mode and --quota-limit counts must match (got {}/{}/{})",
            n,
            args.quota_mode.len(),
            args.quota_limit.len()
        );
    }
    let mut windows = Vec::with_capacity(n);
    for i in 0..n {
        let quota_mode = args.quota_mode[i].clone();
        if !crate::token::dimension::is_valid_mode(&quota_mode) {
            bail!(
                "--quota-mode must be 'tokens' or 'requests', got '{}' for window '{}'",
                quota_mode,
                args.window[i]
            );
        }
        windows.push(NewQuotaWindow {
            window: args.window[i].clone(),
            quota_mode,
            quota_limit: args.quota_limit[i],
        });
    }
    Ok(CreateArgs {
        name: args.name,
        models: args.models,
        windows,
        expires: args.expires,
        group_models: args.group_models,
    })
}

fn describe_models(token: &TokenConfig) -> String {
    if token.allowed_models.is_empty() {
        "ALL models".to_string()
    } else {
        token.allowed_models.join(", ")
    }
}

fn describe_windows_flat(token: &TokenConfig) -> String {
    if token.quota_windows.is_empty() {
        "unlimited".to_string()
    } else {
        token
            .quota_windows
            .iter()
            .map(|w| {
                format!(
                    "{} {}={}",
                    w.window,
                    quota_mode_label(&w.quota_mode),
                    w.quota_limit
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn quota_mode_label(mode: &str) -> &str {
    crate::token::dimension::label(mode)
}

fn print_list_table(rows: &[(TokenConfig, Vec<WindowUsage>)]) {
    println!("{:<36}  {:<16}  {:<30}  EXPIRES", "KEY", "NAME", "WINDOWS");
    for (token, usages) in rows {
        let windows = if usages.is_empty() {
            "unlimited".to_string()
        } else {
            usages
                .iter()
                .map(|u| format!("{}:{}/{}", u.window_key, u.used, u.quota_limit))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let expires = token.expired_at.as_deref().unwrap_or("-");
        println!(
            "{:<36}  {:<16}  {:<30}  {}",
            token.key, token.name, windows, expires
        );
    }
}

fn print_usage_detail(token: &TokenConfig, usages: &[WindowUsage]) {
    println!("token: {} ({})", token.name, token.key);
    println!("  models: {}", describe_models(token));
    println!(
        "  expires: {}",
        token.expired_at.as_deref().unwrap_or("never")
    );
    if usages.is_empty() {
        println!("  quota: unlimited (no windows)");
        return;
    }
    println!("  quota windows:");
    for u in usages {
        let unit = crate::token::dimension::label(&u.quota_mode);
        let remaining = (u.quota_limit - u.used).max(0);
        println!(
            "    {:<10} {}/{} {}  (remaining: {remaining})",
            u.window_key, u.used, u.quota_limit, unit
        );
    }
}

fn token_json(token: &TokenConfig, usages: &[WindowUsage]) -> serde_json::Value {
    serde_json::json!({
        "key": token.key,
        "name": token.name,
        "allowed_models": token.allowed_models,
        "expired_at": token.expired_at,
        "quota_windows": token.quota_windows.iter().map(|w| serde_json::json!({
            "window": w.window,
            "quota_mode": crate::token::dimension::label(&w.quota_mode),
            "quota_limit": w.quota_limit,
        })).collect::<Vec<_>>(),
        "usage": usages.iter().map(|u| serde_json::json!({
            "window": u.window_key,
            "used": u.used,
            "quota_limit": u.quota_limit,
            "quota_mode": crate::token::dimension::label(&u.quota_mode),
        })).collect::<Vec<_>>(),
    })
}

/// Decode a `/admin/token/create` management response into a [`TokenConfig`].
///
/// The server replies with `AdminApiResponse { status, data, kind, error }` and
/// the token fields live under `data` (`f64a125` fixed a bug where the CLI read
/// them from the top level, which made every delegated create fail to parse).
///
/// Decoding is deliberately strict. The previous implementation used
/// `as_str().unwrap_or(...)` throughout, so a server whose response shape drifted
/// again would yield an empty key plus `quota: unlimited` and still exit 0 —
/// a caller would be told an unlimited token was created when a limited one was.
/// A missing or malformed field is now a hard error.
fn created_token_from_response(response: &serde_json::Value) -> Result<TokenConfig> {
    let data = response
        .get("data")
        .ok_or_else(|| anyhow::anyhow!("management response has no `data` object: {response}"))?;

    let key = data
        .get("key")
        .and_then(serde_json::Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| anyhow::anyhow!("management response has no non-empty `key`: {response}"))?
        .to_string();

    let name = data
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("management response has no `name`: {response}"))?
        .to_string();

    let models = data
        .get("models")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("management response has no `models` array: {response}"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(String::from)
                .ok_or_else(|| anyhow::anyhow!("`models` contains a non-string entry: {value}"))
        })
        .collect::<Result<Vec<String>>>()?;

    let windows = data
        .get("windows")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("management response has no `windows` array: {response}"))?
        .iter()
        .map(|window| {
            let key = window
                .get("window")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("quota window has no `window`: {window}"))?;
            let quota_mode = match window.get("quota_mode").and_then(serde_json::Value::as_str) {
                Some(mode) if crate::token::dimension::is_valid_mode(mode) => mode.to_string(),
                other => {
                    return Err(anyhow::anyhow!(
                        "quota window has an unknown `quota_mode` {other:?}: {window}"
                    ));
                }
            };
            let quota_limit = window
                .get("quota_limit")
                .and_then(serde_json::Value::as_i64)
                .ok_or_else(|| anyhow::anyhow!("quota window has no `quota_limit`: {window}"))?;
            Ok(crate::token::config::QuotaWindow {
                window: key.to_string(),
                quota_mode,
                quota_limit,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let expired_at = match data.get("expires") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) => Some(value.clone()),
        Some(other) => {
            return Err(anyhow::anyhow!(
                "`expires` is neither a string nor null: {other}"
            ));
        }
    };

    Ok(TokenConfig {
        key,
        name,
        allowed_models: models,
        quota_windows: windows,
        model_quota_groups: vec![],
        expired_at,
        bad_request_budget_daily_limit: None,
        bad_request_budget_lifetime_limit: None,
        admin: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_response() -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "data": {
                "key": "lp-testkey",
                "name": "alice",
                "models": ["gpt-4o"],
                "windows": [
                    {"window": "5h", "quota_mode": "requests", "quota_limit": 60}
                ],
                "expires": "2026-12-31T23:59:59Z"
            }
        })
    }

    #[test]
    fn decodes_the_data_nested_response() {
        // `f64a125` 的第三个 bug：server 把 token 字段放在 `data` 下，CLI 曾按
        // 顶层解析，导致委托调用报 "EOF while parsing a value"。
        let token = created_token_from_response(&full_response()).unwrap();
        assert_eq!(token.key, "lp-testkey");
        assert_eq!(token.name, "alice");
        assert_eq!(token.allowed_models, vec!["gpt-4o"]);
        assert_eq!(token.quota_windows.len(), 1);
        assert_eq!(token.quota_windows[0].quota_mode, "requests");
        assert_eq!(token.quota_windows[0].quota_limit, 60);
        assert_eq!(token.expired_at.as_deref(), Some("2026-12-31T23:59:59Z"));
    }

    #[test]
    fn rejects_a_response_whose_fields_are_not_under_data() {
        // 顶层形态（即 `f64a125` 之前的错误读法所假设的形状）必须报错，而不是
        // 静默产出空 key + "quota: unlimited"。
        let flat = serde_json::json!({
            "status": "ok",
            "key": "lp-testkey",
            "name": "alice",
            "models": [],
            "windows": []
        });
        let err = created_token_from_response(&flat).unwrap_err().to_string();
        assert!(err.contains("no `data` object"), "{err}");
    }

    #[test]
    fn rejects_missing_or_empty_key() {
        for data in [
            serde_json::json!({"status": "ok", "data": {"name": "alice", "models": [], "windows": []}}),
            serde_json::json!({"status": "ok", "data": {"key": "", "name": "alice", "models": [], "windows": []}}),
        ] {
            let err = created_token_from_response(&data).unwrap_err().to_string();
            assert!(err.contains("non-empty `key`"), "{err}");
        }
    }

    #[test]
    fn rejects_a_malformed_quota_window() {
        let unknown_mode = serde_json::json!({
            "status": "ok",
            "data": {
                "key": "lp-testkey",
                "name": "alice",
                "models": [],
                "windows": [{"window": "5h", "quota_mode": "bananas", "quota_limit": 1}]
            }
        });
        let err = created_token_from_response(&unknown_mode)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown `quota_mode`"), "{err}");

        let missing_limit = serde_json::json!({
            "status": "ok",
            "data": {
                "key": "lp-testkey",
                "name": "alice",
                "models": [],
                "windows": [{"window": "5h", "quota_mode": "tokens"}]
            }
        });
        let err = created_token_from_response(&missing_limit)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `quota_limit`"), "{err}");
    }

    #[test]
    fn accepts_absent_or_null_expires() {
        for data in [
            serde_json::json!({"status": "ok", "data": {"key": "k", "name": "n", "models": [], "windows": []}}),
            serde_json::json!({"status": "ok", "data": {"key": "k", "name": "n", "models": [], "windows": [], "expires": null}}),
        ] {
            let token = created_token_from_response(&data).unwrap();
            assert_eq!(token.expired_at, None);
        }
    }
}
