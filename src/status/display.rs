use std::path::Path;

use crate::config::{AuthConfig, Config, Protocol, ProviderConfig};
use crate::{auth, cooldown, service};

use super::cache::{ProbeCacheEntry, StatusCache};
use super::format::{
    age_label, auth_label, badge, bold, dim, format_latency, green, label, protocol_label, section,
};

pub async fn print_provider_info(cfg: &Config, name: Option<&str>) {
    println!("{}", section("Providers"));
    for (id, provider) in &cfg.providers {
        if name.is_some_and(|wanted| wanted != id) {
            continue;
        }
        let (_state, auth) = provider_auth_summary(id, provider, &auth::default_state_path());
        println!("  {}", id);
        println!("    {}", auth);
        for (protocol, endpoint) in provider.endpoints() {
            let target = endpoint
                .url
                .as_deref()
                .map(|url| format!("url={url}"))
                .or_else(|| {
                    endpoint
                        .derive_from
                        .as_deref()
                        .map(|from| format!("derive_from={from}"))
                })
                .unwrap_or_else(|| "invalid endpoint".to_string());
            println!("    {}: {}", protocol.field_name(), target);
        }
        if name.is_some() {
            print_provider_usage_if_supported(cfg, id).await;
        }
    }
}

pub(crate) fn print_providers(cfg: &Config) {
    println!();
    println!("{}", section("Providers"));
    for (id, provider) in &cfg.providers {
        let (state, auth) = provider_auth_summary(id, provider, &auth::default_state_path());
        let protocols = provider
            .endpoints()
            .iter()
            .map(|(protocol, endpoint)| {
                if endpoint.url.is_some() {
                    protocol.route_key().to_string()
                } else {
                    format!("{}(derived)", protocol.route_key())
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        let url = provider
            .endpoints()
            .iter()
            .find_map(|(_, endpoint)| endpoint.url.as_deref())
            .unwrap_or("-");
        println!(
            "  {} {:<16} {:<40} {}  {}",
            badge(state),
            id,
            protocols,
            url,
            auth_label(state, &auth)
        );
    }
}

async fn print_provider_usage_if_supported(cfg: &Config, provider_id: &str) {
    let Some(provider) = cfg.providers.get(provider_id) else {
        return;
    };
    let Ok(auth) = provider.auth_config(provider_id) else {
        return;
    };
    if !matches!(auth, AuthConfig::OpenaiOauth { .. }) {
        return;
    }
    println!("    usage:");
    let token =
        match crate::usage::resolve_openai_token(cfg, &auth::default_state_path(), provider_id) {
            Ok(token) => token,
            Err(err) => {
                println!("      unavailable: {err}");
                return;
            }
        };
    match crate::usage::query_usage(&token).await {
        Ok(usage) => {
            println!("      plan: {}", usage.plan_type);
            if let Some(rate) = usage.rate_limit {
                println!("      limit_reached: {}", rate.limit_reached);
                if let Some(window) = rate.primary_window {
                    println!(
                        "      primary_window: used={}%, reset_after_seconds={}",
                        window.used_percent,
                        window
                            .reset_after_seconds
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                    );
                }
                if let Some(window) = rate.secondary_window {
                    println!(
                        "      secondary_window: used={}%, reset_after_seconds={}",
                        window.used_percent,
                        window
                            .reset_after_seconds
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                    );
                }
            }
            if let Some(credits) = usage.reset_credits_available {
                println!("      reset_credits: {credits}");
            }
        }
        Err(err) => println!("      unavailable: {err}"),
    }
}

/// 委托模式：格式化 server 返回的 provider 详情（与本地 print_provider_info 输出一致）。
pub fn print_provider_info_json(data: &serde_json::Value) {
    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let auth = data.get("auth").and_then(|v| v.as_str()).unwrap_or("");
    println!("Providers");
    println!("  {}", id);
    println!("    {}", auth);
    if let Some(endpoints) = data.get("endpoints").and_then(|v| v.as_array()) {
        for ep in endpoints {
            let protocol = ep.get("protocol").and_then(|v| v.as_str()).unwrap_or("?");
            let url = ep.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let derive = ep.get("derive_from").and_then(|v| v.as_str()).unwrap_or("");
            if !url.is_empty() {
                println!("    {protocol}: url={url}");
            } else if !derive.is_empty() {
                println!("    {protocol}: derive_from={derive}");
            } else {
                println!("    {protocol}: invalid endpoint");
            }
        }
    }
    if let Some(usage) = data.get("usage").filter(|u| !u.is_null()) {
        println!("    usage:");
        if let Some(unavailable) = usage.get("unavailable").and_then(|v| v.as_str()) {
            println!("      unavailable: {unavailable}");
            return;
        }
        if let Some(plan) = usage.get("plan_type").and_then(|v| v.as_str()) {
            println!("      plan: {plan}");
        }
        if let Some(rl) = usage.get("rate_limit") {
            if let Some(reached) = rl.get("limit_reached").and_then(|v| v.as_bool()) {
                println!("      limit_reached: {reached}");
            }
            for (label, key) in [
                ("primary_window", "primary"),
                ("secondary_window", "secondary"),
            ] {
                if let Some(window) = rl.get(label) {
                    let used = window
                        .get("used_percent")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let reset = window
                        .get("reset_after_seconds")
                        .and_then(|v| v.as_i64())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    println!("      {key}_window: used={used}%, reset_after_seconds={reset}");
                }
            }
        }
        if let Some(credits) = usage.get("reset_credits_available") {
            println!("      reset_credits: {credits}");
        }
    }
}

pub(crate) fn provider_auth_summary(
    provider_id: &str,
    provider: &ProviderConfig,
    store_path: &Path,
) -> (&'static str, String) {
    match provider.auth_config(provider_id) {
        Ok(AuthConfig::ApiKeyEnv { env }) => {
            if std::env::var(&env).is_ok_and(|value| !value.is_empty()) {
                ("OK", format!("api_key_env={env} state=set"))
            } else {
                ("WARN", format!("api_key_env={env} state=missing"))
            }
        }
        Ok(AuthConfig::OpenaiOauth { account }) => oauth_provider_summary(
            account.as_deref().unwrap_or(provider_id),
            "openai_oauth",
            store_path,
        ),
        Ok(AuthConfig::AntigravityOauth { account }) => oauth_provider_summary(
            account.as_deref().unwrap_or(provider_id),
            "antigravity_oauth",
            store_path,
        ),
        Ok(AuthConfig::None) => ("OK", "auth=none".to_string()),
        Err(err) => ("WARN", format!("auth=invalid error={err}")),
    }
}

fn oauth_provider_summary(
    account: &str,
    expected_kind: &str,
    store_path: &Path,
) -> (&'static str, String) {
    let (accounts, skipped) = match auth::load_oauth_accounts(store_path) {
        Ok((accounts, skipped)) => (accounts, skipped),
        Err(err) => {
            return (
                "WARN",
                format!("auth={expected_kind} account={account} state=store-error error={err}"),
            );
        }
    };

    // 检查账号是否在跳过列表中（已被加载时淘汰的无效账号）
    if let Some(s) = skipped.iter().find(|s| s.account_id == account) {
        return (
            "WARN",
            format!(
                "auth={expected_kind} account={account} state=skipped reason=\"{}\"",
                s.reason
            ),
        );
    }

    // 根据类型查找账号
    let (state, label, expires) = match expected_kind {
        "openai_oauth" => match accounts.openai.get(account) {
            Some(acc) => {
                let state = if acc.is_expired() {
                    "expired"
                } else {
                    "authenticated"
                };
                let label = acc.account_label.clone();
                let expires = acc.expires_at_unix.to_string();
                (state, label, expires)
            }
            None => {
                return (
                    "WARN",
                    format!("auth={expected_kind} account={account} state=missing-login"),
                );
            }
        },
        "antigravity_oauth" => match accounts.antigravity.get(account) {
            Some(acc) => {
                let state = if acc.is_expired() {
                    "expired"
                } else {
                    "authenticated"
                };
                let label = acc.account_label.clone();
                let expires = acc.expires_at_unix.to_string();
                (state, label, expires)
            }
            None => {
                return (
                    "WARN",
                    format!("auth={expected_kind} account={account} state=missing-login"),
                );
            }
        },
        _ => {
            return (
                "WARN",
                format!("auth={expected_kind} account={account} state=unsupported-kind"),
            );
        }
    };

    let status = if state == "authenticated" {
        "OK"
    } else {
        "WARN"
    };
    let mut line = format!(
        "auth={expected_kind} account={account} state={} label={} expires_at_unix={} token=***",
        state, label, expires
    );
    // When the token is expired, append a refresh hint so the user knows
    // which command to run without having to consult the docs.
    if state == "expired" {
        let refresh_provider = match expected_kind {
            "openai_oauth" => account,
            "antigravity_oauth" => {
                // antigravity accounts are keyed by account id but the
                // refresh command uses the provider id (which may differ).
                // The caller passes expected_kind="antigravity_oauth" and
                // account is already the account id; the refresh CLI
                // accepts either form.
                account
            }
            _ => account,
        };
        line.push_str(&format!(
            " → run: llm-proxy provider refresh {refresh_provider}"
        ));
    }
    (status, line)
}

pub(super) fn print_auth(path: &Path) {
    println!();
    println!("{}", section("OAuth Credentials"));
    for line in auth_lines(path) {
        println!("{line}");
    }
}

pub(super) fn auth_lines(path: &Path) -> Vec<String> {
    let (rows, skipped) = auth::status_rows(path).unwrap_or_default();
    if rows.is_empty() && skipped.is_empty() {
        return vec![format!("  {}", dim("none"))];
    }
    let mut lines: Vec<String> = rows
        .into_iter()
        .map(|row| {
            let state = if row.state == "authenticated" {
                "OK"
            } else if row.state == "expired" {
                "WARN"
            } else {
                "MISS"
            };
            let account = row.account_label.as_deref().unwrap_or("unknown");
            let expires = row
                .expires_at_unix
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let mut line = format!(
                "  {} provider={} auth_type={} state={} account={} expires_at_unix={} token=***",
                badge(state),
                row.provider,
                row.auth_type,
                row.state,
                account,
                expires
            );
            if row.state == "expired" {
                line.push_str(&format!(
                    " → run: llm-proxy provider refresh {}",
                    row.provider
                ));
            }
            line
        })
        .collect();
    if !skipped.is_empty() {
        lines.push(format!(
            "  {} {}",
            badge("WARN"),
            bold("Skipped Accounts (invalid, run login to fix):")
        ));
        for s in skipped {
            lines.push(format!(
                "    {}/{}  reason: {}",
                s.account_type, s.account_id, s.reason
            ));
        }
    }
    lines
}

pub(super) fn print_cooldowns(path: &Path) {
    println!();
    println!("{}", section("Cooldowns"));
    for line in cooldown_lines(path) {
        println!("{line}");
    }
}

pub(super) fn cooldown_lines(path: &Path) -> Vec<String> {
    let entries = cooldown::read_entries(path).unwrap_or_default();
    if entries.is_empty() {
        return vec![format!("  {}", dim("none"))];
    }
    entries
        .into_iter()
        .map(|entry| {
            format!(
                "  {} model={} provider={} protocol={} kind={} {} reason={}",
                badge("WARN"),
                entry.model,
                entry.provider,
                entry.protocol.route_key(),
                entry.kind,
                dim(&format!(
                    "expires_in={}s",
                    entry
                        .expires_at_unix
                        .saturating_sub(super::cache::unix_now())
                )),
                entry.reason
            )
        })
        .collect()
}

pub(super) fn print_model_status(cfg: &Config, cache: &StatusCache) {
    println!();
    println!("{}", section("Models"));
    let model_count = cfg.models.len();
    let model_ids: Vec<_> = cfg.models.keys().cloned().collect();

    for (idx, model_id) in model_ids.iter().enumerate() {
        let model = &cfg.models[model_id];
        let is_last_model = idx == model_count - 1;
        let model_prefix = if is_last_model {
            "└──"
        } else {
            "├──"
        };

        println!(
            "  {} {}  {}  {}",
            model_prefix,
            bold(model_id),
            dim(&format!(
                "ctx={}",
                format_context_window(model.context_window)
            )),
            dim(&format!(
                "max_out={}",
                format_context_window(model.max_output_tokens)
            ))
        );

        let bindings: Vec<_> = Protocol::CLIENT_PROTOCOLS
            .iter()
            .flat_map(|protocol| {
                model
                    .provider_bindings(*protocol)
                    .iter()
                    .map(move |binding| (protocol, binding))
            })
            .collect();

        let binding_count = bindings.len();
        for (binding_idx, (protocol, binding)) in bindings.iter().enumerate() {
            let is_last_binding = binding_idx == binding_count - 1;
            // 如果是最后一个模型，子节点不需要父级的竖线
            let binding_prefix = if is_last_model {
                if is_last_binding {
                    "    └──"
                } else {
                    "    ├──"
                }
            } else {
                if is_last_binding {
                    "│   └──"
                } else {
                    "│   ├──"
                }
            };

            let Some(provider) = cfg.providers.get(&binding.name) else {
                continue;
            };
            let Some(endpoint) = provider.endpoint(**protocol) else {
                continue;
            };

            let route_note = if endpoint.url.is_some() {
                format!(
                    "native provider={} upstream={}",
                    binding.name, binding.model
                )
            } else {
                format!(
                    "derived from {} provider={} upstream={}",
                    endpoint.derive_from.as_deref().unwrap_or("?"),
                    binding.name,
                    binding.model
                )
            };

            let key = super::format::probe_key(model_id, &binding.name, **protocol);
            let cached = cache.probes.get(&key);
            let (state, detail) = cache_detail_with_latency(cached);

            println!(
                "  {} {} {:<10} {}  {}",
                binding_prefix,
                badge(state),
                protocol_label(protocol.route_key()),
                detail,
                dim(&route_note)
            );
        }
    }
}

pub(super) fn format_context_window(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

pub(super) fn cache_detail_with_latency(entry: Option<&ProbeCacheEntry>) -> (&'static str, String) {
    match entry {
        Some(entry) if entry.ok => {
            let latency_str = entry
                .latency_ms
                .map(format_latency)
                .unwrap_or_else(|| dim("?ms").to_string());
            (
                "OK",
                format!(
                    "cached ok, {}, {}",
                    age_label(entry.checked_at_unix),
                    latency_str
                ),
            )
        }
        Some(entry) => (
            "FAIL",
            format!(
                "cached failed, {}, status={}, {}",
                age_label(entry.checked_at_unix),
                entry
                    .http_status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                entry.error.as_deref().unwrap_or("unknown")
            ),
        ),
        None => ("MISS", "no cached probe".to_string()),
    }
}

#[allow(dead_code)] // Reserved for enhanced status display
pub(super) fn cache_detail(entry: Option<&ProbeCacheEntry>) -> (&'static str, String) {
    match entry {
        Some(entry) if entry.ok => (
            "OK",
            format!(
                "cached ok, {}, {}ms",
                age_label(entry.checked_at_unix),
                entry.latency_ms.unwrap_or_default()
            ),
        ),
        Some(entry) => (
            "FAIL",
            format!(
                "cached failed, {}, status={}, {}",
                age_label(entry.checked_at_unix),
                entry
                    .http_status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                entry.error.as_deref().unwrap_or("unknown")
            ),
        ),
        None => ("MISS", "no cached probe".to_string()),
    }
}

pub(super) fn print_service_state(_config_path: &Path) {
    let pid_path = service::pid_path();
    let state = if service::running_pid().is_some() {
        green("● running")
    } else {
        dim("○ not running")
    };
    let suffix = if pid_path.exists() {
        dim(&format!(
            "; pid_file={}; socket={}",
            pid_path.display(),
            service::socket_path().display()
        ))
    } else {
        String::new()
    };
    println!("{} {}{}", label("Service"), state, suffix);
}

pub(super) fn print_runtime_state() {
    println!();
    println!("{}", section("Runtime State"));
    match service::management_state() {
        Ok(value) => println!("  {} {}", badge("OK"), value),
        Err(_) => println!("  {} service not running", badge("WARN")),
    }
}
