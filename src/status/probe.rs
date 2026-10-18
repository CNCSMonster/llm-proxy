use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::StatusCode;

use crate::config::{Config, Protocol, ReasoningLevelEntry};

use super::ProbeSelection;
use super::cache::{
    ProbeCacheEntry, ProbeFrequencyLimiter, StatusCache, read_cache, unix_now, write_cache,
};
use super::display::{
    AuthScope, print_auth, print_cooldowns, print_model_status, print_providers,
    print_runtime_state, print_service_state,
};
use super::format::{
    badge, cyan, dim, format_latency, heading, label, probe_key, protocol_label, red, section,
};

/// `llm-proxy status` 主入口。
///
/// 两种场景：
/// - Server 存活（本地/远程）→ 委托 Server（读实时状态 / 请求 Server 探活）
/// - Server 未启动（本地）→ 独立模式（读缓存 / CLI 自己探活）
///
/// 缓存更新时机：
/// - Server 可达：每次获取后更新本地缓存（作为不可达时的备份）
/// - 独立 probe 成功：更新缓存；只读缓存：不写
pub async fn print_status(
    config_path: &Path,
    cfg: &Config,
    probe: bool,
    selection: &ProbeSelection,
) -> Result<()> {
    let probe_config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let probe_dirs = crate::service::StateDirs::resolve(probe_config_dir);
    print_status_with_cache_path(
        config_path,
        cfg,
        probe,
        selection,
        super::cache::cache_path_in(&probe_dirs.state_dir),
        &probe_dirs.runtime_dir,
    )
    .await
}

/// 消费 Server 委托探测（经管理通道送达）的 SSE 事件流并逐项打印。
/// 收到 error 事件、流读取失败、或缺少 done 事件提前终止时返回 Err。
pub(super) async fn consume_probe_stream<S>(mut stream: S) -> Result<()>
where
    S: futures_util::Stream<Item = Result<crate::admin_client::SseEvent>> + Unpin,
{
    let mut count = 0usize;
    let mut done = false;
    loop {
        let next_item = match tokio::time::timeout(Duration::from_secs(60), stream.next()).await {
            Ok(item) => item,
            Err(_) => {
                anyhow::bail!("server probe stream timed out waiting for next event (idle > 60s)");
            }
        };
        let Some(event) = next_item else {
            break;
        };
        match event {
            Ok(event) if event.event == "probe_result" => {
                count += 1;
                let provider = event
                    .data
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let model = event
                    .data
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let protocol = event
                    .data
                    .get("protocol")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let ok = event
                    .data
                    .get("ok")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let latency = event
                    .data
                    .get("latency_ms")
                    .and_then(|v| v.as_u64())
                    .map(|v| format!(" {v}ms"))
                    .unwrap_or_default();
                let mark = if ok { "✓" } else { "✗" };
                println!("  [{count}] {mark} 探测 {provider} / {model} / {protocol}{latency}");
            }
            Ok(event) if event.event == "done" => {
                println!("  ✓ {count} 个探测完成");
                done = true;
                break;
            }
            Ok(event) if event.event == "error" => {
                let err_msg = event
                    .data
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                eprintln!("  ✗ Server 探活失败: {err_msg}");
                anyhow::bail!("server probe failed: {err_msg}");
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("  ✗ Server 探活流读取失败: {e}");
                return Err(e);
            }
        }
    }
    if !done {
        anyhow::bail!("server probe stream terminated unexpectedly before done event");
    }
    Ok(())
}

/// 拉取远端 `GET /admin/config/snapshot` 配置镜像。
///
/// 纯客户端（client-only）节点本地 `cfg` 的 providers/models 恒空，status 的
/// 元数据渲染（Providers/Models 树）若直接遍历本地配置必然为空；远端在线时
/// 改用该快照作为渲染源，与远端 `/admin/status` 回传的 probe cache 同源同代。
/// 快照走公开 HTTP listener，与 `/admin/ping` 同一套 Bearer 鉴权，不引入新的
/// 暴露面。响应携带的 `revision`（TUI 编辑器 CAS 用）不消费不展示，解析容忍
/// 其缺失。
#[derive(Debug)]
pub(super) struct RemoteConfigSnapshot {
    pub config: Config,
    pub view: crate::client_view::ClientView,
}

pub(super) async fn fetch_remote_config_snapshot(
    client: &reqwest::Client,
    base_url: &str,
    token: Option<&str>,
) -> Result<RemoteConfigSnapshot> {
    let mut req = client.get(format!("{base_url}/admin/config/snapshot"));
    if let Some(token) = token {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let resp = req
        .send()
        .await
        .context("GET /admin/config/snapshot request failed")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET /admin/config/snapshot returned {status}");
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .context("config snapshot response is not valid JSON")?;
    let data = body
        .get("data")
        .context("config snapshot response has no data")?;
    let mut config_value = data
        .get("config")
        .cloned()
        .context("config snapshot response has no config")?;
    for name in strip_unsupported_providers(&mut config_value) {
        eprintln!("  ⚠ 远端 provider `{name}` 含本客户端不支持的配置，已在本地渲染中跳过");
    }
    let view: crate::client_view::ClientView = data
        .get("view")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or(crate::client_view::ClientView::Client);
    let config: Config =
        serde_json::from_value(config_value).context("failed to parse config snapshot")?;
    Ok(RemoteConfigSnapshot { config, view })
}

/// 远端快照可能携带本客户端不认识的 provider（例如更高版本服务端的扩展认证）。
/// client-only status 不应因单个 provider 解析失败而整体中止：逐个试解析，
/// 剔除失败的 provider，其余照常渲染。
pub(crate) fn strip_unsupported_providers(config: &mut serde_json::Value) -> Vec<String> {
    let Some(providers) = config.get_mut("providers").and_then(|v| v.as_object_mut()) else {
        return Vec::new();
    };
    let unsupported: Vec<String> = providers
        .iter()
        .filter(|(_, value)| {
            serde_json::from_value::<crate::config::ProviderConfig>((*value).clone()).is_err()
        })
        .map(|(name, _)| name.clone())
        .collect();
    for name in &unsupported {
        providers.remove(name);
    }
    unsupported
}

fn is_better_probe(current: &ProbeCacheEntry, candidate: &ProbeCacheEntry) -> bool {
    if candidate.ok != current.ok {
        candidate.ok
    } else {
        candidate.checked_at_unix > current.checked_at_unix
    }
}

/// Project the server's provider-keyed probe cache into the client view.
///
/// The client synthesizes a single `llm-proxy` provider, so its lookup key is
/// `(model, "llm-proxy", protocol)`. For each visible model/protocol we pick
/// the healthiest binding: any successful probe wins; ties go to the freshest
/// entry. Provider topology never leaves the server.
pub(crate) fn project_client_status_cache(
    cfg: &Config,
    cache: &StatusCache,
    grant: Option<&crate::token::auth::TokenGrant>,
) -> StatusCache {
    let mut projected = StatusCache::default();
    for (model_id, model) in &cfg.models {
        if let Some(grant) = grant
            && !grant.model_allowed(model_id)
        {
            continue;
        }
        for protocol in Protocol::CLIENT_PROTOCOLS {
            if !cfg.model_serves_protocol(model_id, protocol) {
                continue;
            }

            let mut best: Option<ProbeCacheEntry> = None;
            let mut consider = |entry: &ProbeCacheEntry| {
                if best
                    .as_ref()
                    .is_none_or(|current| is_better_probe(current, entry))
                {
                    best = Some(entry.clone());
                }
            };

            if model.is_routing_model() {
                for route in model.routes() {
                    let Some(target) = cfg.models.get(&route.target) else {
                        continue;
                    };
                    for binding in target.provider_bindings(protocol) {
                        let key = probe_key(&route.target, &binding.name, protocol);
                        if let Some(entry) = cache.probes.get(&key) {
                            consider(entry);
                        }
                    }
                }
            } else {
                for binding in model.provider_bindings(protocol) {
                    let key = probe_key(model_id, &binding.name, protocol);
                    if let Some(entry) = cache.probes.get(&key) {
                        consider(entry);
                    }
                }
            }

            if let Some(mut entry) = best {
                // Provider-specific errors may contain provider ids or upstream
                // URLs; the client view only needs availability.
                if entry.error.is_some() {
                    entry.error = Some("probe failed".to_string());
                }
                projected
                    .probes
                    .insert(probe_key(model_id, "llm-proxy", protocol), entry);
            }
        }
    }
    projected
}

pub async fn print_status_with_cache_path(
    config_path: &Path,
    cfg: &Config,
    probe: bool,
    selection: &ProbeSelection,
    cache_path: PathBuf,
    runtime_dir: &Path,
) -> Result<()> {
    cfg.validate()?;
    if let Some(ref p) = selection.provider {
        crate::pattern::PatternMatcher::new(p).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    if let Some(ref m) = selection.model {
        crate::pattern::PatternMatcher::new(m).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    let mut cache = read_cache(&cache_path).unwrap_or_default();
    // Selector-bearing delegated probes use their own feature-version gate after a
    // connectivity-only detection. All other status paths retain the generic compatibility gate.
    let server = match if probe && !selection.is_empty() {
        crate::admin_client::detect_server_for_probe_selection_at(config_path, runtime_dir).await
    } else {
        crate::admin_client::detect_server_at(config_path, runtime_dir).await
    } {
        Ok(Some(conn)) => {
            // 读委托加 config_identity 门禁，不匹配或未上报硬拒绝
            crate::ownership::ensure_same_config(&conn, config_path)?;
            Some(conn)
        }
        Ok(None) => None,
        Err(e) => anyhow::bail!("{e}"),
    };

    println!("{}", heading("llm-proxy status"));
    println!(
        "{}  {}  {}",
        label("Config"),
        badge("OK"),
        dim(&config_path.display().to_string())
    );
    if let Some(server) = cfg.server_config() {
        println!("{}  {}", label("Listen"), server.listen);
    } else if let Some(remote) = &cfg.remote {
        println!("{}  {}", label("Remote"), remote.url);
    }
    // Show state/runtime directories for operational visibility
    let status_config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let status_dirs = crate::service::StateDirs::resolve(status_config_dir);
    println!(
        "{}  {}",
        label("State"),
        dim(&status_dirs.state_dir.display().to_string())
    );
    println!(
        "{}  {}",
        label("Runtime"),
        dim(&status_dirs.runtime_dir.display().to_string())
    );
    if let Some(ref server) = cfg.server {
        print_service_state(config_path, &server.listen);
    } else if let Some(ref remote) = cfg.remote {
        println!("{}  {}", label("Remote"), cyan(&remote.url));
    } else {
        println!(
            "{}  {}",
            label("Config"),
            red("no server or remote configured")
        );
    }
    println!(
        "{}   {}",
        label("Probe"),
        if probe {
            cyan("probe requested")
        } else {
            dim("cached only; use --probe for a real upstream probe")
        }
    );

    // 数据来源提示
    // 远端在线时拉取配置镜像，供下方元数据渲染使用。
    let mut remote_render_cfg: Option<Config> = None;
    let mut remote_view: Option<crate::client_view::ClientView> = None;
    let mut server_mode = false;
    match &server {
        Some(conn) => {
            server_mode = true;
            if probe {
                // 委托 Server 探活，SSE 流式显示每个完成项。
                match conn.status_probe_stream(selection).await {
                    Ok(stream) => {
                        println!();
                        println!("{}", section("Online Probe (server)"));
                        println!("  Selection: {}", selection.describe());
                        consume_probe_stream(std::pin::pin!(stream)).await?;
                    }
                    Err(e) => {
                        eprintln!("  ✗ Server 探活失败: {e}");
                        eprintln!(
                            "  建议：检查 server 是否运行，或稍后重试。流式探测应在 30s 内完成；若仍超时请检查上游网络/API Key。"
                        );
                        anyhow::bail!("{e}");
                    }
                }
            } else {
                println!("  ℹ 数据来自 Server（实时）");
            }
        }
        None => {
            if let Some(remote) = &cfg.remote {
                // 远程模式下的 status 探测
                let base_url = remote.url.trim_end_matches('/');
                let token_opt = cfg.resolve_remote_auth_token();
                let client = match reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .no_proxy()
                    .build()
                {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("  ✗ 初始化 HTTP client 失败: {e}");
                        return Err(e.into());
                    }
                };

                let mut ping_req = client.get(format!("{base_url}/admin/ping"));
                if let Some(ref token) = token_opt {
                    ping_req =
                        ping_req.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
                }

                match ping_req.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        println!("  ✓ 远程 Server 在线 ({})", cyan(&remote.url));
                        if let Ok(body) = resp.json::<serde_json::Value>().await
                            && let Some(version) = body.get("version").and_then(|v| v.as_str())
                        {
                            println!("  ℹ 远程 Server 版本: {version}");
                        }

                        if probe {
                            println!();
                            println!("{}", section("Online Probe (remote)"));
                            println!("  Selection: {}", selection.describe());

                            let probe_client = match reqwest::Client::builder()
                                .connect_timeout(Duration::from_secs(10))
                                .read_timeout(Duration::from_secs(60))
                                .no_proxy()
                                .build()
                            {
                                Ok(c) => c,
                                Err(e) => {
                                    eprintln!("  ✗ 初始化流式探活 HTTP client 失败: {e}");
                                    return Err(e.into());
                                }
                            };

                            let mut probe_req = probe_client
                                .post(format!("{base_url}/admin/status/probe"))
                                .header(reqwest::header::ACCEPT, "text/event-stream");
                            if let Some(ref token) = token_opt {
                                probe_req = probe_req.header(
                                    reqwest::header::AUTHORIZATION,
                                    format!("Bearer {token}"),
                                );
                            }
                            if !selection.is_empty() {
                                probe_req = probe_req
                                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                                    .json(selection);
                            }
                            match probe_req.send().await {
                                Ok(probe_resp) if probe_resp.status().is_success() => {
                                    let is_sse = probe_resp
                                        .headers()
                                        .get(reqwest::header::CONTENT_TYPE)
                                        .and_then(|v| v.to_str().ok())
                                        .is_some_and(|v| v.contains("text/event-stream"));

                                    if is_sse {
                                        let stream =
                                            crate::admin_client::response_to_sse_stream(probe_resp);
                                        if let Err(e) =
                                            consume_probe_stream(std::pin::pin!(stream)).await
                                        {
                                            eprintln!("  ✗ 远程流式探活失败: {e}");
                                            return Err(e);
                                        }
                                    } else {
                                        let _ = probe_resp.bytes().await;
                                        println!("  ✓ 远程 Server 探活成功");
                                    }
                                }
                                Ok(probe_resp) => {
                                    let status = probe_resp.status();
                                    let bytes = probe_resp.bytes().await.unwrap_or_default();
                                    let bounded_len = bytes.len().min(65536);
                                    let text =
                                        String::from_utf8_lossy(&bytes[..bounded_len]).to_string();
                                    let msg = if let Ok(body) =
                                        serde_json::from_str::<serde_json::Value>(&text)
                                    {
                                        body.get("error")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string())
                                            .unwrap_or(text)
                                    } else if !text.trim().is_empty() {
                                        text
                                    } else {
                                        format!("status {}", status)
                                    };
                                    eprintln!("  ✗ 远程 Server 探活返回错误: {msg}");
                                    anyhow::bail!(
                                        "remote probe failed with status {status}: {msg}"
                                    );
                                }
                                Err(e) => {
                                    eprintln!("  ✗ 远程 Server 探活网络失败: {e}");
                                    return Err(e.into());
                                }
                            }
                        }

                        // 尝试从远程获取最新 status 状态与 cache
                        let mut status_req = client.get(format!("{base_url}/admin/status"));
                        if let Some(ref token) = token_opt {
                            status_req = status_req
                                .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
                        }
                        if let Ok(status_resp) = status_req.send().await
                            && status_resp.status().is_success()
                            && let Ok(status_json) = status_resp.json::<serde_json::Value>().await
                            && let Some(server_cache) = status_json
                                .get("data")
                                .and_then(|d| d.get("cache"))
                                .and_then(serde_json::Value::as_object)
                            && let Ok(parsed) = serde_json::from_value::<StatusCache>(
                                serde_json::Value::Object(server_cache.clone()),
                            )
                        {
                            cache = parsed;
                            if let Err(e) = write_cache(&cache_path, &cache) {
                                eprintln!("  ⚠ 本地缓存更新失败: {e}");
                            }
                            println!("  ℹ 数据来自远程 Server（实时）");
                        }

                        // 元数据渲染源：client-only 节点本地 providers/models
                        // 不承载流量，渲染源恒为远端配置镜像；快照失败即
                        // fail-fast（不渲染半截状态、不回退本地口径）。
                        match fetch_remote_config_snapshot(&client, base_url, token_opt.as_deref())
                            .await
                        {
                            Ok(snapshot) => {
                                println!("  ℹ 已加载远程配置快照");
                                remote_view = Some(snapshot.view);
                                remote_render_cfg = Some(snapshot.config);
                            }
                            Err(e) => {
                                eprintln!("  ⚠ 获取远程配置快照失败: {e}");
                                anyhow::bail!(
                                    "client-only status requires the remote config snapshot; aborting without partial render"
                                );
                            }
                        }
                    }
                    Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => {
                        println!("  ✗ 远程 Server 认证失败 (HTTP 401: Unauthorized)");
                        println!(
                            "    建议：检查 [remote] 中的 auth_token_env 对应的环境变量是否已设置且与远程服务匹配。"
                        );
                    }
                    Ok(resp) => {
                        println!("  ✗ 远程 Server 返回异常状态: {}", resp.status());
                    }
                    Err(e) => {
                        println!("  ✗ 远程 Server 不可达 ({e})");
                        let has_cache =
                            !cache.probes.is_empty() || !cache.dynamic_models.is_empty();
                        if has_cache {
                            println!("  ⚠ 回退到本地缓存（可能过时）");
                        } else {
                            println!("  ✗ 无缓存数据");
                        }
                    }
                }
            } else {
                // Server 不可达：判定本地/远程模式
                let is_remote = cfg.providers.is_empty() && cfg.models.is_empty();

                if probe {
                    if is_remote {
                        if !selection.is_empty() {
                            anyhow::bail!(
                                "cannot validate probe selection because the configured server is unavailable"
                            );
                        }
                        // 远程模式 + probe：尝试失败 → 回退缓存
                        println!("  ✗ 远程模式无法本地探测（Server 不可达）");
                        let has_cache =
                            !cache.probes.is_empty() || !cache.dynamic_models.is_empty();
                        if has_cache {
                            println!("  ⚠ 回退到本地缓存（可能过时）");
                        } else {
                            println!("  ✗ 无缓存数据");
                        }
                    } else {
                        // 本地模式 + probe：CLI 自己探活
                        if !selection.is_empty()
                            && let Err(err) = selection.validate(cfg)
                        {
                            anyhow::bail!("{err}");
                        }
                        println!();
                        println!("{}", section("Online Probe"));
                        println!("  Selection: {}", selection.describe());
                        run_online_probes(config_path, cfg, &mut cache, selection).await;
                        discover_dynamic_models(
                            cfg,
                            &mut cache,
                            selection,
                            &crate::auth::state_path_for_config(config_path),
                        )
                        .await;
                        if let Err(e) = write_cache(&cache_path, &cache) {
                            eprintln!("  ✗ 缓存写入失败: {e}");
                        }
                        println!("  ✓ 已执行本地探测");
                    }
                } else {
                    let has_cache = !cache.probes.is_empty() || !cache.dynamic_models.is_empty();
                    if has_cache {
                        println!("  ⚠ 数据来自本地缓存（Server 未启动，可能过时）");
                    } else {
                        println!("  ✗ Server 未启动，无缓存数据");
                    }
                }
            }
        }
    }

    let accounts_path = crate::auth::state_path_for_config(config_path);
    // 元数据渲染源选择：远端在线时渲染源恒为远端配置镜像（快照失败已在上方
    // fail-fast，不会到达此处）；远端不可达或本地 Server 场景使用本地配置
    // （本地 Server 与本地 cfg 同源）。运行态判定始终用本地 cfg：快照里的
    // [server] 属于远端，误传会把部署模式判回本地。
    let render_cfg: &Config = remote_render_cfg.as_ref().unwrap_or(cfg);
    let auth_scope = if remote_render_cfg.is_some() {
        AuthScope::RemoteManaged
    } else {
        AuthScope::Local
    };
    let client_view = matches!(remote_view, Some(crate::client_view::ClientView::Client));
    if !client_view {
        print_providers(render_cfg, config_path, selection, auth_scope);
        print_auth(&accounts_path, render_cfg, selection);
        let cooldown_state_dir = {
            let cd = config_path.parent().unwrap_or(std::path::Path::new("."));
            crate::service::StateDirs::resolve(cd).state_dir
        };
        print_cooldowns(&cooldown_state_dir.join("cooldowns.json"), selection);
    }
    print_runtime_state(config_path, cfg);

    // Server 可达 → 每次获取后更新本地缓存；只读缓存不写
    if server_mode {
        let conn = server.as_ref().expect("server_mode implies Some");
        match conn.status().await {
            Ok(status) => {
                if let Some(server_cache) = status
                    .get("data")
                    .and_then(|d| d.get("cache"))
                    .and_then(serde_json::Value::as_object)
                    && let Ok(parsed) = serde_json::from_value::<StatusCache>(
                        serde_json::Value::Object(server_cache.clone()),
                    )
                {
                    cache = parsed;
                    if let Err(e) = write_cache(&cache_path, &cache) {
                        eprintln!("  ⚠ 本地缓存更新失败: {e}");
                    }
                }
            }
            Err(e) => {
                // 设计决策②：检测到 Server 但调用失败 → 报错不回退（防内存态/盘分裂）
                anyhow::bail!("server status request failed: {e}");
            }
        }
    }

    print_model_status(render_cfg, &cache, selection, client_view);

    Ok(())
}

pub(super) async fn run_online_probes(
    config_path: &Path,
    cfg: &Config,
    cache: &mut StatusCache,
    selection: &ProbeSelection,
) {
    let client = match crate::http_client::try_build_with_timeout(cfg, Duration::from_secs(30)) {
        Ok(client) => client,
        Err(err) => {
            println!("  client init failed: {err}");
            return;
        }
    };

    let candidates = selection.candidates(cfg);
    let candidate_set: HashSet<(&str, &str, Protocol)> = candidates
        .iter()
        .map(|(p, m, proto)| (p.as_str(), m.as_str(), *proto))
        .collect();

    // load cooldown store from disk for the CLI probe path so
    // probes respect cooldowns set by the forwarding path (and vice versa).
    let cooldown_store_dir = {
        let d = config_path.parent().unwrap_or(std::path::Path::new("."));
        crate::service::StateDirs::resolve(d).state_dir
    };
    let cooldown_store =
        crate::cooldown::CooldownStore::load(cooldown_store_dir.join("cooldowns.json")).ok();

    let mut limiter = ProbeFrequencyLimiter::default();
    for (model_id, model) in &cfg.models {
        for protocol in Protocol::CLIENT_PROTOCOLS {
            for binding in model.provider_bindings(protocol) {
                if !candidate_set.contains(&(&binding.name[..], model_id.as_str(), protocol)) {
                    continue;
                }
                let Some((_id, plan)) = cfg
                    .resolve_model_request_candidates(protocol, model_id)
                    .into_iter()
                    .find(|(_, plan)| plan.provider_id == binding.name)
                else {
                    continue;
                };
                let key = super::format::probe_key(model_id, &binding.name, protocol);
                if !limiter.acquire(&plan).await {
                    cache.probes.insert(
                        key,
                        ProbeCacheEntry {
                            ok: false,
                            checked_at_unix: unix_now(),
                            latency_ms: None,
                            http_status: None,
                            error: Some(
                                "probe deferred by provider request_frequency queue timeout"
                                    .to_string(),
                            ),
                        },
                    );
                    println!(
                        "  {} {model_id} {} {}  deferred by request_frequency",
                        badge("WARN"),
                        protocol_label(protocol.route_key()),
                        binding.name
                    );
                    continue;
                }
                let cooldown_cfg = cfg.effective_cooldown(model_id, &binding.name);
                run_one_online_probe(
                    config_path,
                    cfg,
                    cache,
                    &client,
                    model_id,
                    protocol,
                    &binding.name,
                    &plan,
                    cooldown_store.as_ref(),
                    &cooldown_cfg,
                )
                .await;

                // 探测之间引入平滑间隔与抖动，避免秒级连续请求轰炸
                let interval_ms = cfg.status.probe_interval_ms;
                let jitter_ms = cfg.status.probe_interval_jitter_ms;
                if interval_ms > 0 || jitter_ms > 0 {
                    let jitter = if jitter_ms > 0 {
                        use std::time::SystemTime;
                        let nanos = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .map(|d| d.subsec_nanos() as u64)
                            .unwrap_or(0);
                        nanos % (jitter_ms + 1)
                    } else {
                        0
                    };
                    tokio::time::sleep(Duration::from_millis(interval_ms + jitter)).await;
                }
            }
        }
    }
}

/// 对单个 model/protocol/provider 绑定执行一次真实探活，结果写入 cache。
/// 同时被 CLI 独立模式（`status --probe`）与 Server 委托（`/admin/status/probe`）复用。
///
/// # Probe send governance
///
/// `cooldown_store` enables two governance checks that align the probe path
/// with the forwarding path:
/// 1. **Pre-send cooldown check**: if the (provider, model) is already in
///    cooldown, skip the upstream request and report COOLDOWN status.
/// 2. **Post-response cooldown writeback**: on 429 / 5xx, write a cooldown
///    entry so the isolation window is extended.
///
/// Both are no-ops when `cooldown_store` is `None` (backward compatibility
/// for callers that don't have access to the cooldown store).
///
/// ## frequency accounting
/// The single send point below (marked `frequency accounting hook`)
/// is the instrumentable location where shared rate-limit group accounting
/// will be plugged in later.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_one_online_probe(
    config_path: &Path,
    cfg: &Config,
    cache: &mut StatusCache,
    client: &reqwest::Client,
    model_id: &str,
    protocol: Protocol,
    provider_id: &str,
    plan: &crate::config::ExecutionPlan,
    cooldown_store: Option<&crate::cooldown::CooldownStore>,
    cooldown_cfg: &crate::config::FallbackCooldownConfig,
) -> bool {
    let key = super::format::probe_key(model_id, provider_id, protocol);
    let now = unix_now();
    let mut entry = ProbeCacheEntry {
        ok: false,
        checked_at_unix: now,
        latency_ms: None,
        http_status: None,
        error: None,
    };

    // 判据 1: pre-send cooldown check.
    // If the forwarding path has already placed this (provider, model) in
    // cooldown, the probe must not send upstream — report COOLDOWN instead.
    let cooldown_key = plan.cooldown_key(model_id);
    if let Some(store) = cooldown_store
        && store.is_cooling_down(&cooldown_key)
    {
        let remaining = store
            .active_entry(&cooldown_key)
            .map(|e| e.expires_at_unix.saturating_sub(now))
            .unwrap_or(0);
        entry.error = Some(format!(
            "probe skipped: provider in cooldown ({remaining}s remaining)"
        ));
        println!(
            "  {} {model_id} {} {provider_id}  probe skipped (cooldown, {remaining}s remaining)",
            badge("SKIP"),
            protocol_label(protocol.route_key())
        );
        cache.probes.insert(key, entry);
        return false;
    }

    let accounts_path = crate::auth::state_path_for_config(config_path);
    let resolved_auth = match cfg.resolve_auth_at(provider_id, &accounts_path) {
        Ok(auth) => auth,
        Err(err) => {
            entry.error = Some(format!("auth unavailable: {err}"));
            println!(
                "  - {model_id} {} {provider_id}: auth unavailable",
                protocol_label(protocol.route_key())
            );
            cache.probes.insert(key, entry);
            return false;
        }
    };

    let body = match crate::probe::probe_body_with_auth(plan, Some(&resolved_auth)) {
        Ok(body) => body,
        Err(err) => {
            entry.error = Some(err.to_string());
            println!(
                "  - {model_id} {} {provider_id}: probe body unsupported",
                protocol_label(protocol.route_key())
            );
            cache.probes.insert(key, entry);
            return false;
        }
    };

    let started = Instant::now();
    // frequency accounting hook — all probe sends go through this
    // call site. Shared rate-limit group accounting plugs in here.
    let request = crate::probe::apply_auth_header(
        plan,
        crate::probe::apply_protocol_headers(
            plan,
            client.post(plan.native_url.clone()).json(&body),
        ),
        resolved_auth.token.clone(),
    );
    let result = request.send().await;
    entry.latency_ms = Some(started.elapsed().as_millis().try_into().unwrap_or(u64::MAX));

    match result {
        Ok(resp) => {
            let status = resp.status();
            entry.http_status = Some(status.as_u16());
            if is_success(status) {
                entry.ok = true;
                let latency_str = entry
                    .latency_ms
                    .map(format_latency)
                    .unwrap_or_else(|| dim("?ms").to_string());
                println!(
                    "  {} {model_id} {} {provider_id}  {}",
                    badge("OK"),
                    protocol_label(protocol.route_key()),
                    latency_str
                );
            } else {
                let mut detail = format!("http {status}");
                // 解析响应体中的错误类型，提供更具体的提示
                if (status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::FORBIDDEN)
                    && let Ok(body) = resp.text().await
                    && let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
                    && let Some(typ) = json["error"]["type"].as_str()
                {
                    match typ {
                        "exceeded_current_quota_error" => {
                            detail.push_str(" - 账户余额不足/已暂停，请充值")
                        }
                        "access_terminated_error" => detail.push_str(" - 配额已用完，请等待重置"),
                        "rate_limit_exceeded" => detail.push_str(" - 请求过于频繁，稍后重试"),
                        _ => {}
                    }
                }
                entry.error = Some(detail.clone());
                println!(
                    "  {} {model_id} {} {provider_id}  {detail}",
                    badge("FAIL"),
                    protocol_label(protocol.route_key())
                );

                // 判据 2: post-response cooldown writeback.
                // Match the forwarding path's governance: 429 and health-family
                // 5xx write a cooldown so the isolation window is extended.
                if let Some(store) = cooldown_store
                    && let Some(duration) = probe_cooldown_duration_for_status(status, cooldown_cfg)
                {
                    let kind = probe_cooldown_kind_for_status(status);
                    store.set(&cooldown_key, kind, &format!("probe {status}"), duration);
                }

                // upstream 400 → per-provider abnormal request counter.
                // When count reaches tolerance → provider-level cooldown.
                // Local 400 (from llm-proxy pre-check) never reaches here because
                // probes always send to upstream; local 400 only occurs in the
                // forwarding path (handled by ).
                if status == StatusCode::BAD_REQUEST
                    && let Some(store) = cooldown_store
                {
                    let count = store.increment_bad_request_count(provider_id);
                    let tolerance = cfg
                        .providers
                        .get(provider_id)
                        .map(|p| p.bad_request_tolerance)
                        .unwrap_or(2);
                    if count >= tolerance {
                        let duration = Duration::from_secs(
                            cooldown_cfg.client_error_seconds.min(24 * 60 * 60),
                        );
                        // Provider-level cooldown: write for all client protocols
                        // with wildcard model_id so all models under this provider
                        // are affected.
                        for proto in Protocol::CLIENT_PROTOCOLS {
                            let provider_key = crate::config::CooldownKey {
                                model_id: "*".to_string(),
                                provider_id: provider_id.to_string(),
                                protocol: proto,
                                token: None,
                                token_key_hash: None,
                            };
                            store.set(
                                &provider_key,
                                "bad_request_tolerance",
                                &format!("probe 400 tolerance reached ({count}/{tolerance})"),
                                duration,
                            );
                        }
                        // Reset counter after triggering cooldown
                        store.reset_bad_request_count(provider_id);
                    }
                }
            }
        }
        Err(err) => {
            entry.error = Some(err.to_string());
            println!(
                "  {} {model_id} {} {provider_id}  request failed",
                badge("FAIL"),
                protocol_label(protocol.route_key())
            );
        }
    }
    cache.probes.insert(key, entry);
    true
}

/// Cooldown duration for probe responses — mirrors the forwarding path's
/// `cooldown_duration_for_status` but only for the health-family statuses
/// that probes should trigger cooldown on (429, 5xx). Probes do NOT write
/// cooldown for 400/401/403/402/404/408 — those are request-level errors
/// that don't indicate provider health issues.
fn probe_cooldown_duration_for_status(
    status: StatusCode,
    cooldown_cfg: &crate::config::FallbackCooldownConfig,
) -> Option<Duration> {
    let seconds = if status == StatusCode::TOO_MANY_REQUESTS {
        cooldown_cfg.rate_limit_seconds
    } else if status.is_server_error() {
        cooldown_cfg.server_error_seconds
    } else {
        0
    };
    if seconds == 0 {
        None
    } else {
        Some(Duration::from_secs(seconds.min(24 * 60 * 60)))
    }
}

/// Cooldown kind for probe responses — subset of the forwarding path's
/// `cooldown_kind_for_status`, covering only the statuses probes write back.
fn probe_cooldown_kind_for_status(status: StatusCode) -> &'static str {
    if status == StatusCode::TOO_MANY_REQUESTS {
        "rate_limit"
    } else if status.is_server_error() {
        "server_error"
    } else {
        "unknown"
    }
}

pub(crate) async fn discover_dynamic_models(
    cfg: &Config,
    cache: &mut StatusCache,
    selection: &ProbeSelection,
    oauth_accounts_path: &Path,
) {
    let client = match crate::http_client::try_build_with_timeout(cfg, Duration::from_secs(30)) {
        Ok(client) => client,
        Err(err) => {
            println!("  dynamic model discovery client init failed: {err}");
            return;
        }
    };

    let target_providers: Option<HashSet<String>> = if selection.is_empty() {
        None
    } else {
        let candidates = selection.candidates(cfg);
        Some(
            candidates
                .into_iter()
                .map(|(provider_id, _, _)| provider_id)
                .collect(),
        )
    };

    for (provider_id, provider) in &cfg.providers {
        if let Some(ref targets) = target_providers
            && !targets.contains(provider_id)
        {
            continue;
        }
        let result = match provider_id.as_str() {
            "ollama" => discover_ollama_models(&client, provider_id, provider).await,
            "openrouter" => {
                discover_openrouter_models(&client, provider_id, provider, cfg, oauth_accounts_path)
                    .await
            }
            _ => continue,
        };
        match result {
            Ok(rows) => {
                println!("  {} dynamic models for {}", rows.len(), provider_id);
                cache.dynamic_models.insert(provider_id.clone(), rows);
            }
            Err(err) => {
                println!("  dynamic model discovery for {provider_id} failed: {err}");
            }
        }
    }
}

pub(super) async fn discover_ollama_models(
    client: &reqwest::Client,
    provider_id: &str,
    provider: &crate::config::ProviderConfig,
) -> Result<Vec<super::cache::DynamicModelCacheEntry>> {
    let base =
        ollama_base_url(provider).context("ollama provider has no native OpenAI chat URL")?;
    let source_url = format!("{}/api/tags", base.trim_end_matches('/'));
    let payload: serde_json::Value = client
        .get(&source_url)
        .send()
        .await
        .context("GET /api/tags failed")?
        .error_for_status()
        .context("GET /api/tags returned error")?
        .json()
        .await
        .context("GET /api/tags returned invalid JSON")?;
    let now = unix_now();
    let mut rows = Vec::new();
    for name in payload
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("name").and_then(serde_json::Value::as_str))
    {
        let show = fetch_ollama_show(client, &base, name).await.ok();
        rows.push(ollama_cache_row(
            provider_id,
            &source_url,
            now,
            name,
            show.as_ref(),
        ));
    }
    Ok(rows)
}

async fn fetch_ollama_show(
    client: &reqwest::Client,
    base: &str,
    model: &str,
) -> Result<serde_json::Value> {
    client
        .post(format!("{}/api/show", base.trim_end_matches('/')))
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .with_context(|| format!("POST /api/show failed for {model}"))?
        .error_for_status()
        .with_context(|| format!("POST /api/show returned error for {model}"))?
        .json()
        .await
        .with_context(|| format!("POST /api/show returned invalid JSON for {model}"))
}

pub(super) fn ollama_cache_row(
    provider_id: &str,
    source_url: &str,
    now: u64,
    name: &str,
    show: Option<&serde_json::Value>,
) -> super::cache::DynamicModelCacheEntry {
    let context_window = show.and_then(ollama_context_window);
    let features = show
        .and_then(|value| value.get("capabilities"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(ollama_capability_feature)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    super::cache::DynamicModelCacheEntry {
        provider_id: provider_id.to_string(),
        source_url: source_url.to_string(),
        probed_at_unix: now,
        stale_after_unix: now + 24 * 60 * 60,
        model_id: name.to_string(),
        display_name: Some(name.to_string()),
        context_window,
        max_output_tokens: None,
        features,
        supported_parameters: Vec::new(),
        reasoning_levels: None,
        default_reasoning_level: None,
    }
}

pub(super) fn ollama_context_window(show: &serde_json::Value) -> Option<i64> {
    show.get("model_info")
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| {
            obj.iter()
                .find(|(key, _)| key.ends_with(".context_length"))
                .and_then(|(_, value)| value.as_i64())
        })
        .or_else(|| {
            show.get("parameters")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_ollama_num_ctx)
        })
}

pub(super) fn parse_ollama_num_ctx(parameters: &str) -> Option<i64> {
    parameters.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("num_ctx"), Some(value)) => value.parse().ok(),
            _ => None,
        }
    })
}

pub(super) fn ollama_capability_feature(capability: &str) -> String {
    match capability {
        "vision" => "image_input".to_string(),
        "tools" => "tools".to_string(),
        other => other.to_string(),
    }
}

async fn discover_openrouter_models(
    client: &reqwest::Client,
    provider_id: &str,
    provider: &crate::config::ProviderConfig,
    cfg: &Config,
    oauth_accounts_path: &Path,
) -> Result<Vec<super::cache::DynamicModelCacheEntry>> {
    let source_url = "https://openrouter.ai/api/v1/models".to_string();
    let mut req = client.get(&source_url);
    match cfg.resolve_auth_at(provider_id, oauth_accounts_path) {
        Ok(auth) => {
            if let Some(token) = auth.token {
                req = req.bearer_auth(token);
            }
        }
        Err(_) if provider.api_key_env.is_some() => {
            // OpenRouter model list can be public, so absence of a key should not
            // prevent explicit discovery. Authenticated accounts may see richer data.
        }
        Err(err) => return Err(err).context("OpenRouter auth resolution failed"),
    }
    let payload: serde_json::Value = req
        .send()
        .await
        .context("GET OpenRouter models failed")?
        .error_for_status()
        .context("GET OpenRouter models returned error")?
        .json()
        .await
        .context("GET OpenRouter models returned invalid JSON")?;
    let now = unix_now();
    let rows = payload
        .get("data")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id").and_then(serde_json::Value::as_str)?;
            let context_window = item
                .get("context_length")
                .and_then(serde_json::Value::as_i64);
            let max_output_tokens = item
                .get("top_provider")
                .and_then(|v| v.get("max_completion_tokens"))
                .and_then(serde_json::Value::as_i64);
            Some(openrouter_cache_row(
                provider_id,
                &source_url,
                now,
                id,
                item,
                context_window,
                max_output_tokens,
            ))
        })
        .collect();
    Ok(rows)
}

pub(super) fn openrouter_cache_row(
    provider_id: &str,
    source_url: &str,
    now: u64,
    id: &str,
    item: &serde_json::Value,
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
) -> super::cache::DynamicModelCacheEntry {
    let supported_parameters = string_array(item.get("supported_parameters"));
    let input_modalities = item
        .get("architecture")
        .and_then(|v| v.get("input_modalities"));
    let mut features = std::collections::BTreeSet::new();
    if supported_parameters
        .iter()
        .any(|p| p == "tools" || p == "tool_choice")
    {
        features.insert("tools".to_string());
    }
    if supported_parameters.iter().any(|p| p == "response_format") {
        features.insert("structured_output".to_string());
    }
    for modality in string_array(input_modalities) {
        match modality.as_str() {
            "image" => {
                features.insert("image_input".to_string());
            }
            "file" => {
                features.insert("document_input".to_string());
            }
            _ => {}
        }
    }
    let reasoning = item.get("reasoning");
    let reasoning_levels = {
        let explicit = string_array(reasoning.and_then(|r| r.get("supported_efforts")));
        let levels: Vec<String> = if !explicit.is_empty() {
            explicit
        } else {
            if supported_parameters
                .iter()
                .any(|p| p == "reasoning" || p == "reasoning_effort")
            {
                vec![
                    "minimal".to_string(),
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                    "xhigh".to_string(),
                    "max".to_string(),
                ]
            } else {
                Vec::new()
            }
        };
        if levels.is_empty() {
            None
        } else {
            Some(
                levels
                    .into_iter()
                    .map(|level| ReasoningLevelEntry::new(level, None))
                    .collect(),
            )
        }
    };
    let default_reasoning_level = reasoning
        .and_then(|r| r.get("default_effort"))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    super::cache::DynamicModelCacheEntry {
        provider_id: provider_id.to_string(),
        source_url: source_url.to_string(),
        probed_at_unix: now,
        stale_after_unix: now + 24 * 60 * 60,
        model_id: id.to_string(),
        display_name: item
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        context_window,
        max_output_tokens,
        features: features.into_iter().collect(),
        supported_parameters,
        reasoning_levels,
        default_reasoning_level,
    }
}

pub(super) fn string_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn ollama_base_url(provider: &crate::config::ProviderConfig) -> Option<String> {
    let url = provider.openai_chat.as_ref()?.url.as_ref()?;
    let parsed = url::Url::parse(url).ok()?;
    let origin = parsed.origin().ascii_serialization();
    Some(origin)
}

fn is_success(status: StatusCode) -> bool {
    status.is_success()
}
