use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::StatusCode;

use crate::config::{Config, Protocol};

use super::cache::{
    ProbeCacheEntry, ProbeFrequencyLimiter, StatusCache, read_cache, unix_now, write_cache,
};
use super::display::{
    print_auth, print_cooldowns, print_model_status, print_providers, print_runtime_state,
    print_service_state,
};
use super::format::{badge, cyan, dim, format_latency, heading, label, protocol_label, section};

/// `llm-proxy status` 主入口（§12 Status 命令重构）。
///
/// §12.2 两种场景：
/// - Server 存活（本地/远程）→ 委托 Server（读实时状态 / 请求 Server 探活）
/// - Server 未启动（本地）→ 独立模式（读缓存 / CLI 自己探活）
///
/// §12.11 缓存更新时机：
/// - Server 可达：每次获取后更新本地缓存（作为不可达时的备份）
/// - 独立 probe 成功：更新缓存；只读缓存：不写
pub async fn print_status(config_path: &Path, cfg: &Config, probe: bool) -> Result<()> {
    cfg.validate()?;
    let cache_path = super::cache::cache_path();
    let mut cache = read_cache(&cache_path).unwrap_or_default();
    // §13：Server 版本不兼容时 detect_server 返回 Err，报错退出
    let server = match crate::admin_client::detect_server(config_path).await {
        Ok(Some(conn)) => Some(conn),
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
    println!("{}  {}", label("Listen"), cfg.server.listen);
    print_service_state(config_path);
    println!(
        "{}   {}",
        label("Probe"),
        if probe {
            cyan("probe requested")
        } else {
            dim("cached only; use --probe for a real upstream probe")
        }
    );

    // §12.10 数据来源提示
    let mut server_mode = false;
    match &server {
        Some(conn) => {
            server_mode = true;
            if probe {
                // 委托 Server 探活（§12.5/12.6），SSE 流式显示每个完成项。
                match conn.status_probe_stream().await {
                    Ok(stream) => {
                        println!();
                        println!("{}", section("Online Probe (server)"));
                        let mut count = 0usize;
                        let mut stream = std::pin::pin!(stream);
                        while let Some(event) = stream.next().await {
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
                                    println!(
                                        "  [{count}] {mark} 探测 {provider} / {model} / {protocol}{latency}"
                                    );
                                }
                                Ok(event) if event.event == "done" => {
                                    println!("  ✓ {count} 个探测完成");
                                    break;
                                }
                                Ok(event) if event.event == "error" => {
                                    eprintln!(
                                        "  ✗ Server 探活失败: {}",
                                        event
                                            .data
                                            .get("error")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("unknown error")
                                    );
                                    break;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    eprintln!("  ✗ Server 探活流读取失败: {e}");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("  ✗ Server 探活失败: {e}");
                        eprintln!(
                            "  建议：检查 server 是否运行，或稍后重试。流式探测应在 30s 内完成；若仍超时请检查上游网络/API Key。"
                        );
                    }
                }
            } else {
                println!("  ℹ 数据来自 Server（实时）");
            }
        }
        None => {
            // Server 不可达：判定本地/远程模式
            let is_remote = cfg.providers.is_empty() && cfg.models.is_empty();

            if probe {
                if is_remote {
                    // 远程模式 + probe：尝试失败 → 回退缓存
                    println!("  ✗ 远程模式无法本地探测（Server 不可达）");
                    let has_cache = !cache.probes.is_empty() || !cache.dynamic_models.is_empty();
                    if has_cache {
                        println!("  ⚠ 回退到本地缓存（可能过时）");
                    } else {
                        println!("  ✗ 无缓存数据");
                    }
                } else {
                    // 本地模式 + probe：CLI 自己探活（§12.2 场景 2）
                    println!();
                    println!("{}", section("Online Probe"));
                    run_online_probes(cfg, &mut cache).await;
                    discover_dynamic_models(cfg, &mut cache).await;
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

    print_providers(cfg);
    print_auth(&crate::auth::default_state_path());
    print_cooldowns(&crate::cooldown::default_state_path());
    print_runtime_state();

    // §12.11：Server 可达 → 每次获取后更新本地缓存；只读缓存不写
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

    print_model_status(cfg, &cache);

    Ok(())
}

pub(super) async fn run_online_probes(cfg: &Config, cache: &mut StatusCache) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            println!("  client init failed: {err}");
            return;
        }
    };

    let mut limiter = ProbeFrequencyLimiter::default();
    for (model_id, model) in &cfg.models {
        for protocol in Protocol::CLIENT_PROTOCOLS {
            for binding in model.provider_bindings(protocol) {
                let Some((_id, plan)) = cfg
                    .resolve_model_request_candidates(protocol, model_id)
                    .into_iter()
                    .find(|(_, plan)| plan.provider_id == binding.name)
                else {
                    continue;
                };
                if !limiter.acquire(&plan).await {
                    let key = super::format::probe_key(model_id, &binding.name, protocol);
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
                run_one_online_probe(
                    cfg,
                    cache,
                    &client,
                    model_id,
                    protocol,
                    &binding.name,
                    &plan,
                )
                .await;
            }
        }
    }
}

/// 对单个 model/protocol/provider 绑定执行一次真实探活，结果写入 cache。
/// 同时被 CLI 独立模式（`status --probe`）与 Server 委托（`/admin/status/probe`）复用。
pub(crate) async fn run_one_online_probe(
    cfg: &Config,
    cache: &mut StatusCache,
    client: &reqwest::Client,
    model_id: &str,
    protocol: Protocol,
    provider_id: &str,
    plan: &crate::config::ExecutionPlan,
) {
    let key = super::format::probe_key(model_id, provider_id, protocol);
    let now = unix_now();
    let mut entry = ProbeCacheEntry {
        ok: false,
        checked_at_unix: now,
        latency_ms: None,
        http_status: None,
        error: None,
    };

    let resolved_auth = match cfg.resolve_auth(provider_id) {
        Ok(auth) => auth,
        Err(err) => {
            entry.error = Some(format!("auth unavailable: {err}"));
            println!(
                "  - {model_id} {} {provider_id}: auth unavailable",
                protocol_label(protocol.route_key())
            );
            cache.probes.insert(key, entry);
            return;
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
            return;
        }
    };

    let started = Instant::now();
    let request = crate::probe::apply_auth_header(
        plan,
        crate::probe::apply_protocol_headers(plan, client.post(&plan.native_url).json(&body)),
        resolved_auth.token,
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
}

async fn discover_dynamic_models(cfg: &Config, cache: &mut StatusCache) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            println!("  dynamic model discovery client init failed: {err}");
            return;
        }
    };

    for (provider_id, provider) in &cfg.providers {
        let result = if provider.antigravity.is_some() {
            discover_antigravity_models(&client, provider_id, provider, cfg).await
        } else {
            match provider_id.as_str() {
                "ollama" => discover_ollama_models(&client, provider_id, provider).await,
                "openrouter" => {
                    discover_openrouter_models(&client, provider_id, provider, cfg).await
                }
                _ => continue,
            }
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
        supported_reasoning_levels: Vec::new(),
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
) -> Result<Vec<super::cache::DynamicModelCacheEntry>> {
    let source_url = "https://openrouter.ai/api/v1/models".to_string();
    let mut req = client.get(&source_url);
    match cfg.auth_token(provider_id) {
        Ok(Some(token)) => {
            req = req.bearer_auth(token);
        }
        Ok(None) => {}
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

async fn discover_antigravity_models(
    client: &reqwest::Client,
    provider_id: &str,
    provider: &crate::config::ProviderConfig,
    cfg: &Config,
) -> Result<Vec<super::cache::DynamicModelCacheEntry>> {
    let auth = cfg.resolve_auth(provider_id)?;
    let token = auth
        .token
        .context("Antigravity model discovery requires OAuth access token")?;
    let base = antigravity_base_url(provider)
        .unwrap_or_else(|| "https://cloudcode-pa.googleapis.com".to_string());
    let source_url = format!(
        "{}/v1internal:fetchAvailableModels",
        base.trim_end_matches('/')
    );
    let mut body = serde_json::Map::new();
    if let Some(project_id) = auth.project_id.filter(|id| !id.is_empty()) {
        body.insert("project".to_string(), serde_json::json!(project_id));
    }
    let payload: serde_json::Value = client
        .post(&source_url)
        .bearer_auth(token)
        .header("User-Agent", "antigravity/cli/1.0.13")
        .json(&serde_json::Value::Object(body))
        .send()
        .await
        .context("POST Antigravity fetchAvailableModels failed")?
        .error_for_status()
        .context("POST Antigravity fetchAvailableModels returned error")?
        .json()
        .await
        .context("POST Antigravity fetchAvailableModels returned invalid JSON")?;
    Ok(antigravity_cache_rows(
        provider_id,
        &source_url,
        unix_now(),
        &payload,
    ))
}

pub(super) fn antigravity_cache_rows(
    provider_id: &str,
    source_url: &str,
    now: u64,
    payload: &serde_json::Value,
) -> Vec<super::cache::DynamicModelCacheEntry> {
    let web_search = string_array(payload.get("webSearchModelIds"))
        .into_iter()
        .map(|id| id.to_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    payload
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| antigravity_model_id(item).map(|id| (id, item)))
        .map(|(id, item)| {
            let mut features = Vec::new();
            if web_search.contains(&id.to_lowercase()) {
                features.push("web_search".to_string());
            }
            super::cache::DynamicModelCacheEntry {
                provider_id: provider_id.to_string(),
                source_url: source_url.to_string(),
                probed_at_unix: now,
                stale_after_unix: now + 24 * 60 * 60,
                model_id: id.clone(),
                display_name: item
                    .get("displayName")
                    .or_else(|| item.get("display_name"))
                    .or_else(|| item.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
                    .or(Some(id)),
                context_window: item
                    .get("contextWindow")
                    .or_else(|| item.get("context_window"))
                    .and_then(serde_json::Value::as_i64),
                max_output_tokens: item
                    .get("maxOutputTokens")
                    .or_else(|| item.get("max_output_tokens"))
                    .and_then(serde_json::Value::as_i64),
                features,
                supported_parameters: Vec::new(),
                supported_reasoning_levels: Vec::new(),
                default_reasoning_level: None,
            }
        })
        .collect()
}

pub(super) fn antigravity_model_id(item: &serde_json::Value) -> Option<String> {
    if let Some(id) = item.as_str() {
        return Some(id.to_string());
    }
    item.get("id")
        .or_else(|| item.get("model"))
        .or_else(|| item.get("name"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
}

fn antigravity_base_url(provider: &crate::config::ProviderConfig) -> Option<String> {
    let url = provider.antigravity.as_ref()?.url.as_ref()?;
    let parsed = url::Url::parse(url).ok()?;
    Some(parsed.origin().ascii_serialization())
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
    let supported_reasoning_levels = {
        let explicit = string_array(reasoning.and_then(|r| r.get("supported_efforts")));
        if !explicit.is_empty() {
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
        supported_reasoning_levels,
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
