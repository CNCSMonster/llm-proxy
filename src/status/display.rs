use std::path::{Path, PathBuf};

use crate::config::{AuthConfig, Config, Protocol, ProviderConfig};
use crate::{auth, cooldown, service};

use super::cache::{ProbeCacheEntry, StatusCache};
use super::format::{
    age_label, auth_label, badge, bold, dim, format_latency, green, label, protocol_label, section,
};

pub async fn print_provider_info(cfg: &Config, config_path: &Path, name: Option<&str>) {
    let accounts_path = auth::state_path_for_config(config_path);
    println!("{}", section("Providers"));
    for (id, provider) in &cfg.providers {
        if name.is_some_and(|wanted| wanted != id) {
            continue;
        }
        let (_state, auth) = provider_auth_summary(id, provider, &accounts_path);
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
            print_provider_usage_if_supported(cfg, &accounts_path, id).await;
        }
    }
}

/// Providers 行的凭证核验口径。
///
/// 纯客户端（client-only）节点以远端配置快照渲染时，provider 凭证由远程
/// Server 维护，本地 env / OAuth 账号库对这些 provider 没有意义——按本地
/// 口径核验必然全行误报 WARN（`state=missing` / `missing-login` /
/// `store-error`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthScope {
    /// 本地部署：读本地 env / 本地 OAuth 账号库。
    Local,
    /// 快照渲染（client-only）：凭证由远程 Server 维护，本地核验不适用。
    RemoteManaged,
}

pub(crate) fn print_providers(
    cfg: &Config,
    config_path: &Path,
    selection: &super::selection::ProbeSelection,
    auth_scope: AuthScope,
) {
    println!();
    println!("{}", section("Providers"));
    for line in provider_rows(cfg, config_path, selection, auth_scope) {
        println!("{line}");
    }
}

/// Providers 行（可断言的行生成版；空/无匹配情形由调用方的 section 头承接）。
pub(super) fn provider_rows(
    cfg: &Config,
    config_path: &Path,
    selection: &super::selection::ProbeSelection,
    auth_scope: AuthScope,
) -> Vec<String> {
    let accounts_path = auth::state_path_for_config(config_path);
    let mut rows = Vec::new();
    for (id, provider) in &cfg.providers {
        if !selection.matches_provider(id, cfg) {
            continue;
        }
        let (row_badge, auth_text) = match auth_scope {
            AuthScope::Local => {
                let (state, auth) = provider_auth_summary(id, provider, &accounts_path);
                (badge(state), auth_label(state, &auth))
            }
            AuthScope::RemoteManaged => match provider.auth_config(id) {
                Ok(AuthConfig::None) => (badge("OK"), green("auth=none")),
                _ => (
                    dim("N/A"),
                    dim("auth=remote-managed (凭证由远程 Server 维护，本地不适用)"),
                ),
            },
        };
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
        rows.push(format!(
            "  {row_badge} {:<16} {:<40} {}  {auth_text}",
            id, protocols, url
        ));
    }

    if rows.is_empty() {
        if let Some(ref p) = selection.provider {
            rows.push(format!("  No matching providers found: {p:?}"));
        } else if let Some(ref m) = selection.model {
            rows.push(format!(
                "  No matching providers found for model filter: {m:?}"
            ));
        }
    }
    rows
}

async fn print_provider_usage_if_supported(cfg: &Config, accounts_path: &Path, provider_id: &str) {
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
    let token = match crate::usage::resolve_openai_token(cfg, accounts_path, provider_id) {
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
        Ok(AuthConfig::None) => ("OK", "auth=none".to_string()),
        Err(err) => ("WARN", format!("auth=invalid error={err}")),
    }
}

pub(crate) fn oauth_provider_summary(
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
    let (state, label, expires, email, duplicate_of) = match expected_kind {
        "openai_oauth" => match accounts.openai.get(account) {
            Some(acc) => {
                let state = if acc.is_expired() {
                    "expired"
                } else {
                    "authenticated"
                };
                let label = acc.account_label.clone();
                let expires = acc.expires_at_unix.to_string();
                (state, label, expires, None::<String>, None::<String>)
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

    let status = if state == "authenticated" && duplicate_of.is_none() {
        "OK"
    } else {
        "WARN"
    };
    let mut line = format!(
        "auth={expected_kind} account={account} state={} label={} expires_at_unix={}",
        state, label, expires
    );
    if let Some(ref em) = email {
        line.push_str(&format!(" email={em}"));
    }
    if let Some(ref dup) = duplicate_of {
        line.push_str(&format!(
            " [duplicate_email: conflict with account '{dup}', quota shared - remove duplicate account]"
        ));
    }
    line.push_str(" token=***");
    // When the token is expired, append a hint — but it must not read as an order
    // to refresh manually: forwarding requests auto-refresh through
    // `proxy::auth::resolve_oauth_auth` (context `proxy_expired_token`), so the
    // manual command is only the remedy for a *failed* refresh. The 2026-08-06 UX
    // report asked for the refresh command here and it was added without this
    // caveat, which is what kept sending readers to `provider refresh` for a state
    // that heals itself.
    if state == "expired" {
        let refresh_provider = match expected_kind {
            "openai_oauth" => account,
            _ => account,
        };
        line.push_str(&format!(
            " → expires_at stale; next request auto-refreshes — run: llm-proxy provider refresh {refresh_provider} only if refresh fails"
        ));
    }
    (status, line)
}

pub(super) fn print_auth(path: &Path, cfg: &Config, selection: &super::selection::ProbeSelection) {
    println!();
    println!("{}", section("OAuth Credentials"));
    for line in auth_lines_with_selection(path, Some(cfg), selection) {
        println!("{line}");
    }
}

#[allow(dead_code)]
pub(super) fn auth_lines(path: &Path) -> Vec<String> {
    auth_lines_with_selection(path, None, &super::selection::ProbeSelection::default())
}

pub(super) fn auth_lines_with_selection(
    path: &Path,
    cfg: Option<&Config>,
    selection: &super::selection::ProbeSelection,
) -> Vec<String> {
    let (rows, skipped) = auth::status_rows(path).unwrap_or_default();
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            if let Some(cfg) = cfg {
                selection.matches_provider(&row.provider, cfg)
            } else if let Some(ref p_pat) = selection.provider {
                if let Ok(m) = crate::pattern::PatternMatcher::new(p_pat) {
                    m.is_match(&row.provider)
                } else {
                    row.provider == *p_pat
                }
            } else {
                true
            }
        })
        .collect();
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
                    " → expires_at stale; next request auto-refreshes — run: llm-proxy provider refresh {} only if refresh fails",
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

pub(super) fn print_cooldowns(path: &Path, selection: &super::selection::ProbeSelection) {
    println!();
    println!("{}", section("Cooldowns"));
    for line in cooldown_lines_with_selection(path, selection) {
        println!("{line}");
    }
}

#[allow(dead_code)]
pub(super) fn cooldown_lines(path: &Path) -> Vec<String> {
    cooldown_lines_with_selection(path, &super::selection::ProbeSelection::default())
}

pub(super) fn cooldown_lines_with_selection(
    path: &Path,
    selection: &super::selection::ProbeSelection,
) -> Vec<String> {
    let entries = cooldown::read_entries(path).unwrap_or_default();
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| selection.matches_cooldown(&entry.provider, &entry.model))
        .collect();
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

pub(super) fn print_model_status(
    cfg: &Config,
    cache: &StatusCache,
    selection: &super::selection::ProbeSelection,
    client_view: bool,
) {
    println!();
    println!("{}", section("Models"));

    let matching_model_ids: Vec<_> = cfg
        .models
        .keys()
        .filter(|model_id| selection.matches_model(model_id, cfg))
        .cloned()
        .collect();

    if matching_model_ids.is_empty() {
        if let Some(ref m) = selection.model {
            println!("  No matching models found: {m:?}");
        } else if let Some(ref p) = selection.provider {
            println!("  No matching models found for provider: {p:?}");
        }
        return;
    }

    let model_count = matching_model_ids.len();

    for (idx, model_id) in matching_model_ids.iter().enumerate() {
        let model = &cfg.models[model_id];
        let is_last_model = idx == model_count - 1;
        let model_prefix = if is_last_model {
            "└──"
        } else {
            "├──"
        };

        // a routing model declares no window of its own; what a client
        // can actually use is derived from its targets, so show that instead of 0.
        let (display_ctx, display_max_out) = cfg
            .model_capability_bounds(model_id)
            .map(|bounds| (bounds.context_window, bounds.max_output_tokens))
            .unwrap_or((model.context_window, model.max_output_tokens));
        println!(
            "  {} {}  {}  {}",
            model_prefix,
            bold(model_id),
            dim(&format!("ctx={}", format_context_window(display_ctx))),
            dim(&format!(
                "max_out={}",
                format_context_window(display_max_out)
            ))
        );

        // a routing model's children are its routes, not provider
        // bindings. Reporting it as "unbound" would hide a working model.
        if model.is_routing_model() {
            let routes: Vec<_> = model
                .routes()
                .iter()
                .filter(|route| {
                    if let Some(ref _p_pat) = selection.provider {
                        if let Some(target_cfg) = cfg.models.get(&route.target) {
                            Protocol::CLIENT_PROTOCOLS.iter().any(|proto| {
                                target_cfg
                                    .provider_bindings(*proto)
                                    .iter()
                                    .any(|b| selection.matches_binding_provider(&b.name))
                            })
                        } else {
                            false
                        }
                    } else {
                        true
                    }
                })
                .collect();

            let route_count = routes.len();
            for (route_idx, entry) in routes.iter().enumerate() {
                let is_last_route = route_idx == route_count - 1;
                let route_prefix = match (is_last_model, is_last_route) {
                    (true, true) => "    └──",
                    (true, false) => "    ├──",
                    (false, true) => "│   └──",
                    (false, false) => "│   ├──",
                };
                println!(
                    "  {} {} level={} target={}",
                    route_prefix,
                    dim("route"),
                    entry.level,
                    entry.target
                );
            }
            continue;
        }

        let bindings: Vec<_> = Protocol::CLIENT_PROTOCOLS
            .iter()
            .flat_map(|protocol| {
                model
                    .provider_bindings(*protocol)
                    .iter()
                    .filter(|binding| selection.matches_binding_provider(&binding.name))
                    .map(move |binding| (protocol, binding))
            })
            .collect();

        let binding_count = bindings.len();
        if binding_count == 0 {
            println!("{}", unbound_model_status_line(is_last_model));
            continue;
        }

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

            let route_note = if client_view {
                String::new()
            } else if endpoint.url.is_some() {
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

pub(super) fn unbound_model_status_line(is_last_model: bool) -> String {
    let warning_prefix = if is_last_model {
        "    └──"
    } else {
        "│   └──"
    };
    format!(
        "  {} {} {}",
        warning_prefix,
        badge("WARN"),
        unbound_model_warning()
    )
}

pub(super) fn unbound_model_warning() -> &'static str {
    "no configured provider bindings; not selected for probe"
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

/// 本 config 注入身份的 runtime dir。status 的存活判定必须与 `-d` 启动的实例同源：
/// 走全局默认会拿另一个 identity 分片的 pid/socket 当自己的（同实例被判 not running，
/// 正在服务的进程还会被当成"残留"建议 kill）。
fn config_runtime_dir(config_path: &Path) -> PathBuf {
    crate::service::StateDirs::resolve(config_path.parent().unwrap_or(Path::new("."))).runtime_dir
}

/// 存活判定入口（回归基线测试的注入点：pid 文件只写进注入分片时必须读得到）。
pub(super) fn running_service_pid(config_path: &Path) -> Option<u32> {
    service::running_pid_in(&config_runtime_dir(config_path))
}

pub(super) fn print_service_state(config_path: &Path, listen: &str) {
    let pid_path = service::pid_path_in(&config_runtime_dir(config_path));
    let is_running = running_service_pid(config_path).is_some();
    let state = if is_running {
        green("● running")
    } else {
        dim("○ not running")
    };
    let suffix = if pid_path.exists() {
        dim(&format!(
            "; pid_file={}; socket={}",
            pid_path.display(),
            service::socket_path_in(&config_runtime_dir(config_path)).display()
        ))
    } else {
        String::new()
    };
    println!("{} {}{}", label("Service"), state, suffix);

    // 当服务 not running 时，交叉探测 listen 端口占用情况
    if !is_running && let Some(occ) = service::inspect_port_occupant(listen) {
        let port = listen
            .parse::<std::net::SocketAddr>()
            .map(|a| a.port().to_string())
            .unwrap_or_else(|_| listen.split(':').next_back().unwrap_or("8989").to_string());
        if let Some(pid) = occ.pid {
            println!(
                "  {} listen 端口 {} 被占用: PID {} ({})",
                badge("WARN"),
                listen,
                pid,
                occ.summary()
            );
            if occ.is_llm_proxy {
                println!(
                    "        {} 疑似残留的 llm-proxy 实例；若非由当前配置管理，请手动结束: kill {}",
                    dim("↳"),
                    pid
                );
            } else {
                println!(
                    "        {} 端口被外部进程占用；请修改 config.toml 中的 [server] listen 端口或执行: kill {}",
                    dim("↳"),
                    pid
                );
            }
        } else {
            println!(
                "  {} listen 端口 {} 已被占用（未能获取占用进程 PID）",
                badge("WARN"),
                listen
            );
            println!(
                "        {} 请执行 `ss -tlnp | grep {}` 或 `lsof -i :{}` 查看占用者并手动释放端口",
                dim("↳"),
                port,
                port
            );
        }
    }
}

/// Runtime State 行（可断言的行生成版）。
///
/// 部署模式感知——纯 Remote 客户端节点（无 `[server]`、仅 `[remote]`）
/// 本地不运行后台进程，盲连本地 Unix Socket 必然误报 `service not running`；
/// 该模式跳过本地存活探测，明确标注客户端模式。本地存活判定必须与 `-d`
/// 启动的实例同源，故仍走注入式 `*_in()` 变体。
pub(super) fn runtime_state_lines(config_path: &Path, cfg: &Config) -> Vec<String> {
    if cfg.server.is_none() && cfg.remote.is_some() {
        return vec![format!(
            "  {} client-only (remote) — 本地无后台进程",
            dim("ℹ")
        )];
    }
    match service::management_state_in(&config_runtime_dir(config_path)) {
        Ok(value) => vec![format!("  {} {}", badge("OK"), value)],
        Err(_) => vec![format!("  {} service not running", badge("WARN"))],
    }
}

pub(super) fn print_runtime_state(config_path: &Path, cfg: &Config) {
    println!();
    println!("{}", section("Runtime State"));
    for line in runtime_state_lines(config_path, cfg) {
        println!("{line}");
    }
}
