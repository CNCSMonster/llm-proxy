use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{AdapterKind, Config, ExecutionPlan, Protocol};
use crate::usage_stats::{UsageRecord, UsageStore};
use crate::{convert, cooldown::CooldownStore, protection::BadRequestManager};

mod auth;
mod forward_anthropic;
mod forward_antigravity;
mod forward_chat;
mod forward_responses;
mod request;
mod sse_aggregate;

use auth::*;
use forward_anthropic::*;
use forward_antigravity::*;
use forward_chat::*;
use forward_responses::*;
use request::*;
use sse_aggregate::*;

#[derive(Clone)]
pub(crate) struct AppState {
    cfg: Arc<Config>,
    client: reqwest::Client,
    pub(crate) cooldowns: Arc<CooldownStore>,
    // OAuth 账号内存缓存（§15.2 / 阶段 4）：server 启动时加载，
    // 转发从内存取 token（不每次读文件）；login/refresh 委托更新缓存 + 落盘。
    // std::sync::RwLock：转发路径是同步 fn（resolve_plan_auth），刷新在
    // block_in_place 内同步更新缓存（短锁，可接受）。
    pub(crate) oauth_accounts: Arc<std::sync::RwLock<crate::auth::OAuthAccounts>>,
    // OAuth 刷新合并锁：并发请求同时刷新同一账号时串行化 + double-check
    // （§15.2 阶段 4：避免重复刷新触发上游限流）。
    // 用 tokio Mutex：guard 需跨 await 持有（刷新是 async），std MutexGuard 跨 await 会使 Future 不 Send。
    pub(crate) oauth_refresh_lock: Arc<tokio::sync::Mutex<()>>,
    bad_requests: Arc<BadRequestManager>,
    stream_interruptions: Arc<std::sync::Mutex<BTreeMap<String, StreamInterruptionEntry>>>,
    frequency: Arc<tokio::sync::Mutex<FrequencyState>>,
    usage_store: Option<Arc<UsageStore>>,
    // 活跃 provider 记录（§12.3）：成功转发的 provider + 最近时间戳，
    // 作为"活的证据"供 status 跳过冗余探活。唯一状态源 ActiveProviderStore，
    // 与 ServerProbeState 共享同一 Arc（避免双状态源 bug）。
    pub(crate) active_providers: Arc<crate::probe_coordinator::ActiveProviderStore>,
    // 探活 singleflight（§19.6）：使用 ProbeCoordinator 实现真正的 Singleflight。
    pub(crate) probe_coordinator:
        Arc<crate::probe_coordinator::ProbeCoordinator<crate::probe_coordinator::ServerProbeState>>,
    // tokio::sync::Mutex (no poisoning) + spawn_blocking in admin handlers so
    // config file I/O never blocks a tokio worker thread.
    pub(crate) core: Arc<tokio::sync::Mutex<crate::core::CoreState>>,
    // thought_sig_queue caches thought_signatures from Gemini responses,
    // keyed by the functionCall they arrived with (id, or name+args for
    // Gemini-native calls). Injection matches by key — order-based caching
    // mispairs signatures across multi-turn tool calls.
    thought_sig_queue: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

/// Upper bound for the thought_signature replay map. A bounded map keeps
/// memory bounded and drops stale signatures from long-gone tool calls; the
/// next response repopulates it. Replay state is an optimization, not a
/// correctness requirement — losing it only means a functionCall goes
/// unsigned, never mis-signed.
const THOUGHT_SIGNATURE_MAP_MAX: usize = 200;

#[derive(Debug, Clone, Serialize)]
struct StreamInterruptionEntry {
    direction: String,
    count: u64,
    last_error: String,
    last_seen_unix: u64,
}

#[derive(Debug, Default)]
struct FrequencyState {
    attempts: HashMap<String, VecDeque<Instant>>,
    buckets: HashMap<String, BurstBucket>,
}

#[derive(Debug, Clone)]
struct BurstBucket {
    tokens: f64,
    last_refill: Instant,
}

#[derive(Debug)]
enum UpstreamSendFailure {
    Transport(reqwest::Error),
    FrequencyLimited,
}

impl std::fmt::Display for UpstreamSendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamSendFailure::Transport(err) => write!(f, "{err}"),
            UpstreamSendFailure::FrequencyLimited => write!(f, "provider frequency limit exceeded"),
        }
    }
}

impl AppState {
    pub(crate) fn new(cfg: Config, config_path: PathBuf) -> Self {
        let bad_request_cfg = cfg.protection.bad_request.clone();

        // Initialize usage store（§14 单一实例：record 与 query 共享同一内存权威）
        let usage_store = UsageStore::new(cfg.server.usage.clone()).ok().map(Arc::new);

        // Initialize CoreState from config path（必须与 CLI 传入的 --config 一致，
        // 否则 Server 侧的 status/usage 会操作错误的配置）
        let core = crate::core::CoreState::load_with_usage_store(&config_path, usage_store.clone())
            .unwrap_or_else(|_| {
                crate::core::CoreState::from_config_with_usage_store(
                    cfg.clone(),
                    config_path,
                    usage_store.clone(),
                )
            });

        // Single source of truth for provider liveness: shared with ServerProbeState
        let active_store = Arc::new(crate::probe_coordinator::ActiveProviderStore::new());

        Self {
            cfg: Arc::new(cfg),
            client: reqwest::Client::builder()
                .user_agent("llm-proxy/0.2.0")
                .build()
                .expect("reqwest client"),
            cooldowns: Arc::new(CooldownStore::load_default()),
            // OAuth 账号内存缓存：启动时加载（损坏时用空账号，转发时按需刷新）
            oauth_accounts: Arc::new(std::sync::RwLock::new(
                crate::auth::load_oauth_accounts_with_recovery(&crate::auth::default_state_path())
                    .unwrap_or_default(),
            )),
            oauth_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            bad_requests: Arc::new(BadRequestManager::new(bad_request_cfg)),
            stream_interruptions: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            frequency: Arc::new(tokio::sync::Mutex::new(FrequencyState::default())),
            usage_store,
            active_providers: active_store.clone(),
            probe_coordinator: {
                let probe_state = crate::probe_coordinator::ServerProbeState::new(active_store);
                Arc::new(crate::probe_coordinator::ProbeCoordinator::new(probe_state))
            },
            core: Arc::new(tokio::sync::Mutex::new(core)),
            thought_sig_queue: Arc::new(std::sync::Mutex::new(Default::default())),
        }
    }

    #[cfg(test)]
    fn new_in_memory(cfg: Config) -> Self {
        Self::new_in_memory_for_management(cfg)
    }

    #[cfg(test)]
    pub(crate) fn new_in_memory_for_management(cfg: Config) -> Self {
        let bad_request_cfg = cfg.protection.bad_request.clone();
        let config_path = std::path::PathBuf::from("/tmp/test-config.toml");
        let core = crate::core::CoreState::from_config(cfg.clone(), config_path);
        // Single source of truth for provider liveness: shared with ServerProbeState
        let active_store = Arc::new(crate::probe_coordinator::ActiveProviderStore::new());
        Self {
            cfg: Arc::new(cfg),
            client: reqwest::Client::new(),
            cooldowns: Arc::new(CooldownStore::in_memory()),
            oauth_accounts: Arc::new(std::sync::RwLock::new(
                crate::auth::load_oauth_accounts_with_recovery(&crate::auth::default_state_path())
                    .unwrap_or_default(),
            )),
            oauth_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            bad_requests: Arc::new(BadRequestManager::new(bad_request_cfg)),
            stream_interruptions: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            frequency: Arc::new(tokio::sync::Mutex::new(FrequencyState::default())),
            usage_store: None,
            active_providers: active_store.clone(),
            probe_coordinator: {
                let probe_state = crate::probe_coordinator::ServerProbeState::new(active_store);
                Arc::new(crate::probe_coordinator::ProbeCoordinator::new(probe_state))
            },
            core: Arc::new(tokio::sync::Mutex::new(core)),
            thought_sig_queue: Arc::new(std::sync::Mutex::new(Default::default())),
        }
    }

    fn record_usage(
        &self,
        model: String,
        provider: String,
        endpoint: String,
        input_tokens: i64,
        output_tokens: i64,
        latency_ms: Option<i64>,
    ) {
        // 成功转发 = 活跃证据（§12.3），供 status 跳过冗余探活
        self.mark_provider_active(&provider);
        if let Some(store) = &self.usage_store {
            let record = UsageRecord::new(
                model,
                provider,
                endpoint,
                input_tokens,
                output_tokens,
                latency_ms,
            );
            if let Err(e) = store.record(record) {
                tracing::warn!("Failed to record usage: {}", e);
            }
        }
    }

    /// 记录一次成功转发，作为该 provider 的活跃证据（§12.3）。
    /// 统一走 ActiveProviderStore（唯一状态源，与 ServerProbeState 共享）。
    pub(crate) fn mark_provider_active(&self, provider_id: &str) {
        self.active_providers.mark_active(provider_id);
    }

    /// 返回 TTL 窗口内活跃的 provider id 列表（§12.3/12.9）。
    pub(crate) fn get_active_providers(&self, ttl: Duration) -> Vec<String> {
        self.active_providers.active_providers(ttl)
    }

    /// 判断 provider 在 TTL 窗口内是否活跃（§12.3）。
    #[cfg(test)]
    pub(crate) fn is_provider_active(&self, provider_id: &str, ttl: Duration) -> bool {
        self.active_providers.is_active(provider_id, ttl)
    }

    /// Inject cached thought_signatures into functionCall parts that lack them.
    /// Codex strips thought_signature from streaming output, so we cache and re-inject.
    ///
    /// Signatures are matched by the functionCall key (id for Claude-family
    /// calls, name+args for Gemini-native calls). A call whose signature is not
    /// in the cache is left unsigned — injecting a stale signature from an
    /// earlier turn is worse than injecting none.
    fn inject_thought_signatures(&self, body: &mut Value) {
        let sigs = self.thought_sig_queue.lock().unwrap();
        if sigs.is_empty() {
            return;
        }
        let Some(contents) = body
            .get_mut("request")
            .and_then(|r| r.get_mut("contents"))
            .and_then(Value::as_array_mut)
        else {
            return;
        };
        for content in contents.iter_mut() {
            let Some(parts) = content.get_mut("parts").and_then(Value::as_array_mut) else {
                continue;
            };
            for part in parts.iter_mut() {
                // Already has a thought_signature
                if part.get("thoughtSignature").is_some() || part.get("thought_signature").is_some()
                {
                    continue;
                }
                let Some(call) = part
                    .get("functionCall")
                    .or_else(|| part.get("function_call"))
                else {
                    continue;
                };
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let arguments = call
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
                    .to_string();
                // Try the canonical key first (id for Claude-family calls), then
                // fall back to the name+args key. Streaming collection always
                // keys by name+args (streaming chunk args are assembled later),
                // so a Claude-family call whose signature was collected from a
                // stream must still match here despite the id mismatch.
                let signature = {
                    let key = crate::convert::function_call_signature_key(call, &name, &arguments);
                    sigs.get(&key).cloned().or_else(|| {
                        let name_args_key =
                            crate::convert::signature_key_from_name_args(&name, &arguments);
                        sigs.get(&name_args_key).cloned()
                    })
                };
                let Some(signature) = signature else {
                    continue;
                };
                tracing::debug!("inject_thought_signatures: name={name} matched");
                part.as_object_mut()
                    .map(|obj| obj.insert("thoughtSignature".to_string(), json!(signature)));
            }
        }
    }

    /// Collect thought_signatures from an antigravity non-streaming response,
    /// keyed by their functionCall so later turns can re-inject the right one.
    fn collect_thought_signatures(&self, response: &Value) {
        // Antigravity non-streaming responses may be a single-element array or
        // an object — unwrap both to reach the Gemini response envelope.
        let gemini = match response {
            Value::Array(arr) => arr
                .first()
                .and_then(|e| e.get("response"))
                .unwrap_or(response),
            other => other.get("response").unwrap_or(other),
        };
        let candidates = gemini.get("candidates").and_then(Value::as_array);
        let Some(candidates) = candidates else {
            return;
        };
        let mut new_sigs = std::collections::HashMap::new();
        for candidate in candidates {
            let parts = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array);
            let Some(parts) = parts else { continue };
            for part in parts {
                let Some(call) = part
                    .get("functionCall")
                    .or_else(|| part.get("function_call"))
                else {
                    continue;
                };
                let Some(signature) = part
                    .get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                if signature.is_empty() {
                    continue;
                }
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let arguments = call
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
                    .to_string();
                // Store under BOTH keys: the id-keyed one (matching a replay
                // that keeps the upstream id) and the name+args key (matching
                // a replay from an Anthropic client, where tool_use round-trips
                // without the upstream id). Inject falls back across both.
                let key = crate::convert::function_call_signature_key(call, &name, &arguments);
                new_sigs.insert(key, signature.to_string());
                let name_args_key = crate::convert::signature_key_from_name_args(&name, &arguments);
                new_sigs.insert(name_args_key, signature.to_string());
            }
        }
        if !new_sigs.is_empty() {
            let keys: Vec<_> = new_sigs.keys().cloned().collect();
            let mut queue = self.thought_sig_queue.lock().unwrap();
            for (key, signature) in new_sigs {
                queue.insert(key, signature);
            }
            tracing::debug!(
                "collect_thought_signatures: queue_len={} keys={keys:?}",
                queue.len()
            );
            if queue.len() > THOUGHT_SIGNATURE_MAP_MAX {
                queue.clear();
            }
        }
    }

    async fn acquire_frequency(&self, plan: &ExecutionPlan) -> bool {
        let timeout =
            Duration::from_secs(plan.request_frequency.queue_timeout_seconds.unwrap_or(10));
        let deadline = Instant::now() + timeout;
        loop {
            if self.try_acquire_frequency_now(plan).await {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn try_acquire_frequency_now(&self, plan: &ExecutionPlan) -> bool {
        let now = Instant::now();
        let mut state = self.frequency.lock().await;
        let provider_id = plan.provider_id.clone();
        let (minute_count, hour_count) = {
            let attempts = state.attempts.entry(provider_id.clone()).or_default();
            while attempts
                .front()
                .is_some_and(|instant| now.duration_since(*instant) >= Duration::from_secs(3600))
            {
                attempts.pop_front();
            }
            let minute_count = attempts
                .iter()
                .filter(|instant| now.duration_since(**instant) < Duration::from_secs(60))
                .count() as u32;
            (minute_count, attempts.len() as u32)
        };
        let burst = plan.request_frequency.burst.unwrap_or(5).max(1) as f64;
        let minute_limit = plan
            .request_frequency
            .requests_per_minute
            .unwrap_or(60)
            .max(1);
        if minute_count >= minute_limit {
            return false;
        }
        if let Some(hour_limit) = plan.request_frequency.requests_per_hour
            && hour_count >= hour_limit
        {
            return false;
        }

        let bucket = state
            .buckets
            .entry(provider_id.clone())
            .or_insert(BurstBucket {
                tokens: burst,
                last_refill: now,
            });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        let refill_per_second = minute_limit as f64 / 60.0;
        bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(burst);
        bucket.last_refill = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        state
            .attempts
            .entry(provider_id)
            .or_default()
            .push_back(now);
        true
    }

    fn is_cooling_down(&self, key: &crate::config::CooldownKey) -> bool {
        self.cooldowns.is_cooling_down(key)
    }

    fn set_cooldown(
        &self,
        key: &crate::config::CooldownKey,
        kind: &str,
        reason: &str,
        duration: Duration,
    ) {
        self.cooldowns.set(key, kind, reason, duration);
    }

    fn bad_request_blocked(&self, fingerprint: &str) -> bool {
        self.bad_requests.check(fingerprint).blocked
    }

    fn observe_bad_request(&self, fingerprint: &str) {
        self.bad_requests.observe_client_error(fingerprint);
    }

    fn observe_stream_interruption(&self, direction: &str, err: &str) {
        let Ok(mut entries) = self.stream_interruptions.lock() else {
            return;
        };
        let entry =
            entries
                .entry(direction.to_string())
                .or_insert_with(|| StreamInterruptionEntry {
                    direction: direction.to_string(),
                    count: 0,
                    last_error: String::new(),
                    last_seen_unix: 0,
                });
        entry.count += 1;
        entry.last_error = err.to_string();
        entry.last_seen_unix = (unix_millis() / 1000) as u64;
    }

    fn stream_interruption_snapshot(&self) -> Vec<StreamInterruptionEntry> {
        self.stream_interruptions
            .lock()
            .map(|entries| entries.values().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn management_state_json(&self) -> Value {
        json!({
            "bad_request_blocks": self.bad_requests.snapshot(),
            "stream_interruptions": self.stream_interruption_snapshot()
        })
    }
}

#[allow(dead_code)]
pub async fn serve(cfg: Config) -> Result<()> {
    let (_tx, rx) = tokio::sync::oneshot::channel();
    serve_with_shutdown(cfg, rx).await
}

pub async fn serve_with_shutdown(
    cfg: Config,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let config_path = crate::config::default_config_path();
    let state = AppState::new(cfg, config_path);
    serve_state_with_shutdown(state, shutdown_rx).await
}

pub(crate) async fn serve_state_with_shutdown(
    state: AppState,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    state.cfg.validate()?;
    // §14.3 智能落盘：后台任务按请求频率分档定期把内存 usage 落盘
    if let Some(store) = &state.usage_store {
        let _flush_handle =
            crate::usage_stats::UsageStore::spawn_flush_task(store.as_ref().clone());
    }
    // OAuth 账号缺失/损坏不阻塞启动（非 OAuth provider 仍可用），降级为警告
    if let Err(err) =
        crate::auth::validate_oauth_on_startup(&state.cfg, &crate::auth::default_state_path())
    {
        tracing::warn!("OAuth accounts validation failed: {err}");
    }
    let listen = state
        .cfg
        .server
        .listen
        .parse::<SocketAddr>()
        .with_context(|| format!("invalid listen address {}", state.cfg.server.listen))?;
    let app = router(state);

    println!("llm-proxy Rust v2 listening on http://{listen}");
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::select! {
                _ = shutdown_rx => {},
                _ = tokio::signal::ctrl_c() => {},
            }
        })
        .await?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        // Proxy routes
        .route("/openai/v1/models", get(list_openai_models))
        .route("/responses/v1/models", get(list_responses_models))
        .route("/anthropic/v1/models", get(list_anthropic_models))
        .route("/claude-desktop/v1/models", get(list_anthropic_models))
        .route("/openai/v1/chat/completions", post(chat_completions))
        .route("/openai/v1/responses", post(responses))
        .route("/responses/v1/responses", post(responses))
        .route("/anthropic/v1/messages", post(anthropic_messages))
        .route("/claude-desktop/v1/messages", post(anthropic_messages))
        .route(
            "/claude-desktop/v1/messages/count_tokens",
            post(count_tokens),
        )
        // Admin API routes（写端点已迁至 UDS 管理通道，见 service::management_router）
        .route("/admin/ping", get(crate::admin::ping))
        .route("/admin/status", get(crate::admin::status))
        .route("/admin/status/probe", post(crate::admin::status_probe))
        .route("/admin/provider/list", get(crate::admin::provider_list))
        .route("/admin/provider/{id}", get(crate::admin::provider_info))
        .route("/admin/model/list", get(crate::admin::model_list))
        .route("/admin/model/{id}", get(crate::admin::model_info))
        .route(
            "/admin/client-config/{client}",
            get(crate::admin::client_config),
        )
        .route("/admin/usage", get(crate::admin::usage))
        .with_state(state)
}

async fn list_openai_models(State(state): State<AppState>) -> impl IntoResponse {
    let ids: Vec<&String> = state
        .cfg
        .models
        .iter()
        .filter(|(_, model)| {
            model.exposes_protocol(Protocol::OpenaiChatCompletions)
                || model.exposes_protocol(Protocol::OpenaiResponses)
        })
        .map(|(id, _)| id)
        .collect();
    Json(openai_model_list(ids))
}

async fn list_responses_models(State(state): State<AppState>) -> impl IntoResponse {
    let ids: Vec<&String> = state
        .cfg
        .models
        .iter()
        .filter(|(_, model)| model.exposes_protocol(Protocol::OpenaiResponses))
        .map(|(id, _)| id)
        .collect();
    Json(openai_model_list(ids))
}

async fn list_anthropic_models(State(state): State<AppState>) -> impl IntoResponse {
    let ids: Vec<&String> = state
        .cfg
        .models
        .iter()
        .filter(|(_, model)| model.exposes_protocol(Protocol::Anthropic))
        .map(|(id, _)| id)
        .collect();
    Json(anthropic_model_list(ids))
}

fn openai_model_list(ids: Vec<&String>) -> Value {
    let data: Vec<Value> = ids
        .into_iter()
        .map(|id| json!({ "id": id, "object": "model", "created": 0, "owned_by": "llm-proxy" }))
        .collect();
    json!({ "object": "list", "data": data })
}

fn anthropic_model_list(ids: Vec<&String>) -> Value {
    let data: Vec<Value> = ids
        .iter()
        .map(|id| json!({ "id": id, "type": "model", "display_name": id }))
        .collect();
    json!({
        "data": data,
        "has_more": false,
        "first_id": ids.first().map(|s| s.as_str()),
        "last_id": ids.last().map(|s| s.as_str())
    })
}

async fn count_tokens(Json(body): Json<Value>) -> impl IntoResponse {
    let mut text = String::new();
    collect_text_for_token_count(&body, &mut text);
    Json(json!({ "input_tokens": rough_token_count(&text) }))
}

fn collect_text_for_token_count(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push_str(s);
            out.push(' ');
        }
        Value::Array(items) => {
            for item in items {
                collect_text_for_token_count(item, out);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "text" | "content" | "system" | "input") {
                    collect_text_for_token_count(value, out);
                }
            }
        }
        _ => {}
    }
}

fn rough_token_count(text: &str) -> usize {
    text.split_whitespace().count().max(1)
}

async fn chat_completions(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let candidates = match resolve_request_candidates(
        &state,
        Protocol::OpenaiChatCompletions,
        &body,
        json_error,
    ) {
        Ok(candidates) => candidates,
        Err(response) => return *response,
    };
    let body = body_with_default_reasoning(&state, Protocol::OpenaiChatCompletions, body);
    let fingerprint = crate::protection::fingerprint(
        Protocol::OpenaiChatCompletions,
        body.get("model").and_then(Value::as_str),
        &body,
    );
    if state.bad_request_blocked(&fingerprint) {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "request temporarily blocked after repeated upstream bad requests",
        );
    }
    // §10.5.5: until the first byte is written downstream, streaming requests
    // follow the same cooldown/fallback policy as non-streaming ones.
    let all_cooling_down = candidates
        .iter()
        .all(|(frontend_model, plan)| state.is_cooling_down(&plan.cooldown_key(frontend_model)));
    let mut last_error: Option<Response> = None;
    let mut saw_client_error = false;

    for (frontend_model, plan) in candidates {
        let cooldown_key = plan.cooldown_key(&frontend_model);
        if !all_cooling_down && state.is_cooling_down(&cooldown_key) {
            continue;
        }
        let body = match body_with_mapped_reasoning(
            &state,
            Protocol::OpenaiChatCompletions,
            body.clone(),
            &frontend_model,
            &plan,
        ) {
            Ok(body) => body,
            Err(err) => return json_error(StatusCode::BAD_REQUEST, &err.to_string()),
        };
        let response = match plan.adapter() {
            AdapterKind::Passthrough => {
                let mut upstream_body = body.clone();
                if let Some(obj) = upstream_body.as_object_mut() {
                    obj.insert("model".to_string(), json!(plan.upstream_model));
                    if obj.get("stream").and_then(Value::as_bool) == Some(true) {
                        obj.insert(
                            "stream_options".to_string(),
                            json!({ "include_usage": true }),
                        );
                    }
                }
                forward_chat_request(&state, &plan, &frontend_model, upstream_body).await
            }
            AdapterKind::ChatCompletionsFromAnthropic => {
                let anthropic_body =
                    match convert::chat_to_anthropic_request(body.clone(), &plan.upstream_model) {
                        Ok(body) => body,
                        Err(err) => return json_error(StatusCode::BAD_REQUEST, &err.to_string()),
                    };
                forward_chat_via_anthropic(&state, &plan, &frontend_model, anthropic_body).await
            }
            AdapterKind::ChatCompletionsFromResponses => {
                let responses_body =
                    match convert::chat_to_responses_request(body.clone(), &plan.upstream_model) {
                        Ok(body) => body,
                        Err(err) => return json_error(StatusCode::BAD_REQUEST, &err.to_string()),
                    };
                forward_chat_via_responses(&state, &plan, &frontend_model, responses_body).await
            }
            other => json_error(
                StatusCode::BAD_REQUEST,
                &format!(
                    "adapter {other:?} is not valid for Chat ingress; check provider endpoint declarations"
                ),
            ),
        };
        let status = response.status();
        if is_local_frequency_limited_response(&response) {
            last_error = Some(response);
            continue;
        }
        saw_client_error |= status == StatusCode::BAD_REQUEST;
        if let Some(duration) =
            cooldown_duration_for_response(status, response.headers(), &state.cfg.fallback.cooldown)
        {
            state.set_cooldown(
                &cooldown_key,
                cooldown_kind_for_status(status),
                &status.to_string(),
                duration,
            );
            last_error = Some(response);
            continue;
        }
        return response;
    }

    // §10.5.8: the fingerprint block is fed only when every candidate failed
    // and at least one failure was a client error.
    if saw_client_error {
        state.observe_bad_request(&fingerprint);
    }
    last_error.unwrap_or_else(|| {
        json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "all providers are cooling down",
        )
    })
}

async fn responses(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let candidates =
        match resolve_request_candidates(&state, Protocol::OpenaiResponses, &body, responses_error)
        {
            Ok(candidates) => candidates,
            Err(response) => return *response,
        };
    let body = body_with_default_reasoning(&state, Protocol::OpenaiResponses, body);
    let fingerprint = crate::protection::fingerprint(
        Protocol::OpenaiResponses,
        body.get("model").and_then(Value::as_str),
        &body,
    );
    if state.bad_request_blocked(&fingerprint) {
        return responses_error(
            StatusCode::TOO_MANY_REQUESTS,
            "request temporarily blocked after repeated upstream bad requests",
        );
    }
    // §10.5.5: until the first byte is written downstream, streaming requests
    // follow the same cooldown/fallback policy as non-streaming ones.
    let all_cooling_down = candidates
        .iter()
        .all(|(frontend_model, plan)| state.is_cooling_down(&plan.cooldown_key(frontend_model)));
    let mut last_error: Option<Response> = None;
    let mut saw_client_error = false;

    for (frontend_model, plan) in candidates {
        let cooldown_key = plan.cooldown_key(&frontend_model);
        if !all_cooling_down && state.is_cooling_down(&cooldown_key) {
            continue;
        }
        let body = match body_with_mapped_reasoning(
            &state,
            Protocol::OpenaiResponses,
            body.clone(),
            &frontend_model,
            &plan,
        ) {
            Ok(body) => body,
            Err(err) => return responses_error(StatusCode::BAD_REQUEST, &err.to_string()),
        };
        let response = match plan.adapter() {
            AdapterKind::ResponsesFromChatCompletions => {
                let chat_body = match convert::responses_to_chat(body.clone(), &plan.upstream_model)
                {
                    Ok(body) => body,
                    Err(err) => return responses_error(StatusCode::BAD_REQUEST, &err.to_string()),
                };
                forward_responses_via_chat(&state, &plan, &frontend_model, chat_body).await
            }
            AdapterKind::ResponsesFromAnthropic => {
                let anthropic_body =
                    match convert::responses_to_anthropic(body.clone(), &plan.upstream_model) {
                        Ok(body) => body,
                        Err(err) => {
                            return responses_error(StatusCode::BAD_REQUEST, &err.to_string());
                        }
                    };
                forward_responses_via_anthropic(&state, &plan, &frontend_model, anthropic_body)
                    .await
            }
            AdapterKind::ResponsesFromAntigravity => {
                let auth = match resolve_plan_auth(&state, &plan, responses_error).await {
                    Ok(auth) => auth,
                    Err(response) => return response,
                };
                let Some(project_id) = auth.project_id.as_deref() else {
                    return responses_error(
                        StatusCode::UNAUTHORIZED,
                        "provider requires Antigravity project_id; run `llm-proxy provider login antigravity`",
                    );
                };
                let stream_response = body.get("stream").and_then(Value::as_bool) == Some(true);
                let antigravity_body = match convert::responses_to_antigravity_request(
                    body.clone(),
                    &plan.upstream_model,
                    project_id,
                    &plan.anthropic_family_models,
                ) {
                    Ok(body) => body,
                    Err(err) => return responses_error(StatusCode::BAD_REQUEST, &err.to_string()),
                };
                forward_responses_via_antigravity(
                    &state,
                    &plan,
                    &frontend_model,
                    antigravity_body,
                    stream_response,
                    auth.token,
                )
                .await
            }
            AdapterKind::Passthrough => {
                forward_responses_native(&state, &plan, &frontend_model, body.clone()).await
            }
            other => responses_error(
                StatusCode::BAD_REQUEST,
                &format!(
                    "adapter {other:?} is not valid for Responses ingress; check provider endpoint declarations"
                ),
            ),
        };
        let status = response.status();
        if is_local_frequency_limited_response(&response) {
            last_error = Some(response);
            continue;
        }
        saw_client_error |= status == StatusCode::BAD_REQUEST;
        if let Some(duration) =
            cooldown_duration_for_response(status, response.headers(), &state.cfg.fallback.cooldown)
        {
            state.set_cooldown(
                &cooldown_key,
                cooldown_kind_for_status(status),
                &status.to_string(),
                duration,
            );
            last_error = Some(response);
            continue;
        }
        return response;
    }

    // §10.5.8: the fingerprint block is fed only when every candidate failed
    // and at least one failure was a client error.
    if saw_client_error {
        state.observe_bad_request(&fingerprint);
    }
    last_error.unwrap_or_else(|| {
        responses_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "all providers are cooling down",
        )
    })
}

async fn anthropic_messages(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let candidates =
        match resolve_request_candidates(&state, Protocol::Anthropic, &body, anthropic_error) {
            Ok(candidates) => candidates,
            Err(response) => return *response,
        };
    let body = body_with_default_reasoning(&state, Protocol::Anthropic, body);
    let fingerprint = crate::protection::fingerprint(
        Protocol::Anthropic,
        body.get("model").and_then(Value::as_str),
        &body,
    );
    if state.bad_request_blocked(&fingerprint) {
        return anthropic_error(
            StatusCode::TOO_MANY_REQUESTS,
            "request temporarily blocked after repeated upstream bad requests",
        );
    }
    // §10.5.5: until the first byte is written downstream, streaming requests
    // follow the same cooldown/fallback policy as non-streaming ones.
    let all_cooling_down = candidates
        .iter()
        .all(|(frontend_model, plan)| state.is_cooling_down(&plan.cooldown_key(frontend_model)));
    let mut last_error: Option<Response> = None;
    let mut saw_client_error = false;

    for (frontend_model, plan) in candidates {
        let cooldown_key = plan.cooldown_key(&frontend_model);
        if !all_cooling_down && state.is_cooling_down(&cooldown_key) {
            continue;
        }
        let body = match body_with_mapped_reasoning(
            &state,
            Protocol::Anthropic,
            body.clone(),
            &frontend_model,
            &plan,
        ) {
            Ok(body) => body,
            Err(err) => return anthropic_error(StatusCode::BAD_REQUEST, &err.to_string()),
        };
        let response = match plan.adapter() {
            AdapterKind::AnthropicFromChatCompletions => {
                let chat_body = match convert::anthropic_to_chat(body.clone(), &plan.upstream_model)
                {
                    Ok(body) => body,
                    Err(err) => return anthropic_error(StatusCode::BAD_REQUEST, &err.to_string()),
                };
                forward_anthropic_via_chat(&state, &plan, &frontend_model, chat_body).await
            }
            AdapterKind::AnthropicFromResponses => {
                let responses_body = match convert::anthropic_to_responses_request(
                    body.clone(),
                    &plan.upstream_model,
                ) {
                    Ok(body) => body,
                    Err(err) => return anthropic_error(StatusCode::BAD_REQUEST, &err.to_string()),
                };
                forward_anthropic_via_responses(&state, &plan, &frontend_model, responses_body)
                    .await
            }
            AdapterKind::AnthropicFromAntigravity => {
                let auth = match resolve_plan_auth(&state, &plan, anthropic_error).await {
                    Ok(auth) => auth,
                    Err(response) => return response,
                };
                let Some(project_id) = auth.project_id.as_deref() else {
                    return anthropic_error(
                        StatusCode::UNAUTHORIZED,
                        "provider requires Antigravity project_id; run `llm-proxy provider login antigravity`",
                    );
                };
                let stream_response = body.get("stream").and_then(Value::as_bool) == Some(true);
                let antigravity_body = match convert::anthropic_to_antigravity_request(
                    body.clone(),
                    &plan.upstream_model,
                    project_id,
                    &plan.anthropic_family_models,
                ) {
                    Ok(body) => body,
                    Err(err) => return anthropic_error(StatusCode::BAD_REQUEST, &err.to_string()),
                };
                forward_anthropic_via_antigravity(
                    &state,
                    &plan,
                    &frontend_model,
                    antigravity_body,
                    stream_response,
                    auth.token,
                )
                .await
            }
            AdapterKind::Passthrough => {
                forward_anthropic_native(&state, &plan, &frontend_model, body.clone()).await
            }
            other => anthropic_error(
                StatusCode::BAD_REQUEST,
                &format!(
                    "adapter {other:?} is not valid for Anthropic ingress; check provider endpoint declarations"
                ),
            ),
        };
        let status = response.status();
        if is_local_frequency_limited_response(&response) {
            last_error = Some(response);
            continue;
        }
        saw_client_error |= status == StatusCode::BAD_REQUEST;
        if let Some(duration) =
            cooldown_duration_for_response(status, response.headers(), &state.cfg.fallback.cooldown)
        {
            state.set_cooldown(
                &cooldown_key,
                cooldown_kind_for_status(status),
                &status.to_string(),
                duration,
            );
            last_error = Some(response);
            continue;
        }
        return response;
    }

    // §10.5.8: the fingerprint block is fed only when every candidate failed
    // and at least one failure was a client error.
    if saw_client_error {
        state.observe_bad_request(&fingerprint);
    }
    last_error.unwrap_or_else(|| {
        anthropic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "all providers are cooling down",
        )
    })
}

/// Request-time model resolution (design §5): a missing/unknown model ID and
/// a known model without a binding for the request protocol are explicit
/// client-protocol-shaped 4xx errors — never a silent default substitution.
/// Request-content capability filtering follows the trust contract: requests
/// are only rejected locally when the model explicitly does not declare the
/// matching feature.
async fn send_upstream(
    state: &AppState,
    plan: &ExecutionPlan,
    req: reqwest::RequestBuilder,
) -> Result<reqwest::Response, UpstreamSendFailure> {
    // §10.5.5: until the first byte is written downstream, network errors are
    // retried uniformly for streaming and non-streaming requests.
    let attempts = state.cfg.fallback.max_retries.saturating_add(1).max(1);
    let timeout = request_timeout(&state.cfg);
    let mut last_err = None;
    for attempt in 0..attempts {
        if !state.acquire_frequency(plan).await {
            return Err(UpstreamSendFailure::FrequencyLimited);
        }
        let Some(candidate) = req.try_clone() else {
            break;
        };
        match candidate.timeout(timeout).send().await {
            Ok(resp) => return Ok(resp),
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 >= attempts {
                    break;
                }
            }
        }
    }
    match last_err {
        Some(err) => Err(UpstreamSendFailure::Transport(err)),
        None => {
            if !state.acquire_frequency(plan).await {
                return Err(UpstreamSendFailure::FrequencyLimited);
            }
            req.timeout(timeout)
                .send()
                .await
                .map_err(UpstreamSendFailure::Transport)
        }
    }
}

fn upstream_send_failure_response(
    err: UpstreamSendFailure,
    error: fn(StatusCode, &str) -> Response,
) -> Response {
    match err {
        UpstreamSendFailure::FrequencyLimited => {
            let mut response = error(
                StatusCode::TOO_MANY_REQUESTS,
                "provider frequency limit exceeded before contacting upstream",
            );
            response.headers_mut().insert(
                "x-llm-proxy-local-frequency-limit",
                HeaderValue::from_static("1"),
            );
            response
        }
        UpstreamSendFailure::Transport(err) => error(
            StatusCode::BAD_GATEWAY,
            // {:#} 展开 source 链：连接拒绝/超时/DNS 失败对用户可区分。
            &format!("upstream request failed: {:#}", anyhow::Error::new(err)),
        ),
    }
}

fn is_local_frequency_limited_response(response: &Response) -> bool {
    response
        .headers()
        .get("x-llm-proxy-local-frequency-limit")
        .is_some()
}

fn request_timeout(cfg: &Config) -> Duration {
    let seconds = cfg
        .fallback
        .timeout_seconds
        .min(cfg.fallback.max_timeout_seconds)
        .max(1);
    Duration::from_secs(seconds)
}

fn record_usage_from_responses_value(
    state: &AppState,
    value: &Value,
    frontend_model: &str,
    provider_id: &str,
    endpoint: &str,
    latency_ms: Option<i64>,
) {
    let input_tokens = value
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = value
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if input_tokens > 0 || output_tokens > 0 {
        state.record_usage(
            frontend_model.to_string(),
            provider_id.to_string(),
            endpoint.to_string(),
            input_tokens as i64,
            output_tokens as i64,
            latency_ms,
        );
    }
}

/// Response-shaped wrapper around `aggregate_responses_sse_to_value`: records
/// usage and rewrites the model to the frontend model.
#[cfg(test)]
mod tests;
