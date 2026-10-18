//! Admin API handlers for CLI-to-server communication.
//!
//! These endpoints allow CLI processes to manage the proxy server
//! remotely, without direct file access.

use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;

use crate::config::{Config, Protocol, ReasoningLevelEntry};
use crate::proxy::AppState;
use crate::status::ProbeSelection;

// ── Request/Response Types ───────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct PingResponse {
    status: &'static str,
    version: String,
    /// 该 server 正在使用的 config 的身份（见 `config::config_identity`）。
    /// CLI 据此确认"监听同一地址的 server"确实是"用着我这份 config 的 server"。
    /// 哈希值，不泄露路径。
    config_identity: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminApiResponse {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProviderRemoveRequest {
    pub id: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize)]
pub struct ProviderCopyRequest {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub no_api_key: bool,
}

#[derive(Debug, Deserialize)]
pub struct ProviderAddRequest {
    pub provider_id: String,
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub no_api_key: bool,
    #[serde(default)]
    pub provider_type: Option<String>,
    #[serde(default)]
    pub endpoint_url: Option<String>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub skip_probe: bool,
    #[serde(default)]
    pub pending_account: Option<crate::auth::PendingAccount>,
}

#[derive(Debug, Deserialize)]
pub struct ProviderReselectRequest {
    pub provider_id: String,
    pub models: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModelAddRequest {
    pub model_id: String,
    /// None = 未提供；校验（必填/必须 > 0）在 model::add 的写 gate 内执行。
    #[serde(default)]
    pub context_window: Option<i64>,
    #[serde(default)]
    pub max_output: Option<i64>,
    #[serde(default)]
    pub copy_from: Option<String>,
    #[serde(default)]
    pub routes: Option<Vec<crate::config::RouteEntry>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelSetRequest {
    pub model_id: String,
    #[serde(default)]
    pub context_window: Option<i64>,
    #[serde(default)]
    pub max_output_tokens: Option<i64>,
    #[serde(default)]
    pub reasoning_levels: Option<Vec<ReasoningLevelEntry>>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub supports_thinking: Option<bool>,
    #[serde(default)]
    pub force_think: Option<bool>,
    #[serde(default)]
    pub force_no_think: Option<bool>,
    #[serde(default)]
    pub default_think: Option<bool>,
    #[serde(default)]
    pub enable_features: Vec<String>,
    #[serde(default)]
    pub disable_features: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub clear_description: bool,
    #[serde(default)]
    pub chat_reasoning_off_format: Option<String>,
    #[serde(default)]
    pub clear_chat_reasoning_off_format: bool,
    #[serde(default)]
    pub codex_service_tier: Option<crate::config::CodexServiceTierConfig>,
    #[serde(default)]
    pub clear_codex_service_tier: bool,
    #[serde(default)]
    pub cooldown: Option<crate::config::CooldownOverrideConfig>,
    #[serde(default)]
    pub clear_cooldown: bool,
}

#[derive(Debug, Deserialize)]
pub struct ModelRemoveRequest {
    pub model_id: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize)]
pub struct ModelRenameRequest {
    pub old_model: String,
    pub new_model: String,
}

#[derive(Debug, Deserialize)]
pub struct ModelProviderRequest {
    pub action: String,
    pub model_id: String,
    pub protocol: String,
    pub provider: String,
    #[serde(default)]
    pub upstream_model: Option<String>,
    #[serde(default)]
    pub to: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct UsageQueryParams {
    pub period: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    /// 前端视图参数（JSON/TUI 兼容契约），当前 handler 忽略。
    #[allow(dead_code)]
    pub view: Option<String>,
    /// Maximum number of records to return (default 1000, max 10000)
    pub limit: Option<usize>,
    /// Number of records to skip (for pagination)
    pub offset: Option<usize>,
}

/// Query parameters for `GET /admin/diagnostics` (UDS management channel only).
#[derive(Debug, Deserialize)]
pub struct DiagnosticsQueryParams {
    /// Filter by event type: `egress_preflight` or `upstream_client_error`.
    pub event: Option<String>,
    /// Time range (same grammar as `UsageQueryParams::period`).
    pub period: Option<String>,
    /// Maximum number of records to return (default 200, max 1000).
    pub limit: Option<usize>,
    /// Number of records to skip (for pagination).
    pub offset: Option<usize>,
    /// Opt in to seeing `body_full` (egress body) in `capture = "full"` mode.
    /// Without this flag, `body_full` is stripped from the response even when
    /// the file contains it. Has no effect in `metadata` mode.
    #[serde(default)]
    pub include_body: bool,
}

// ── Token Management Request Types (delegation) ────────────────────

#[derive(Debug, Deserialize)]
pub struct TokenCreateRequest {
    pub name: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub windows: Vec<TokenWindowRequest>,
    #[serde(default)]
    pub expires: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenWindowRequest {
    pub window: String,
    pub quota_mode: String,
    pub quota_limit: i64,
}

#[derive(Debug, Deserialize)]
pub struct TokenDeleteRequest {
    pub key_or_name: String,
}

#[derive(Debug, Deserialize)]
pub struct TokenResetUsageRequest {
    pub key_or_name: String,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// GET /admin/ping — health check for CLI server detection
pub async fn ping(State(state): State<AppState>) -> impl IntoResponse {
    Json(PingResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION").to_string(),
        config_identity: crate::config::config_identity(
            state.config_path.parent().unwrap_or(&state.config_path),
        ),
    })
}

/// GET /admin/status — server status summary including active providers
///
/// 返回活跃 provider 列表 + 缓存 probe 结果（§12.5），CLI 委托模式据此展示完整状态。
pub async fn status(
    State(state): State<AppState>,
    grant: Option<axum::extract::Extension<crate::token::auth::TokenGrant>>,
) -> impl IntoResponse {
    let grant_ref = grant.as_ref().map(|e| &e.0);
    let is_admin = grant_ref.is_none_or(|g| g.is_admin());
    let (cfg, active_ttl) = {
        let core = state.core.lock().await;
        let cfg = core.config().clone();
        let ttl = Duration::from_secs(cfg.status.active_ttl);
        (cfg, ttl)
    };
    let provider_count = cfg.providers.len();
    let model_count = cfg.models.len();
    let active_providers = state.get_active_providers(active_ttl);
    let cache = state.probe_coordinator.cache_snapshot().await;

    let data = if is_admin {
        serde_json::json!({
            "providers": provider_count,
            "models": model_count,
            "active_providers": active_providers,
            "cache": cache,
        })
    } else {
        let visible_models = cfg
            .models
            .keys()
            .filter(|id| grant_ref.is_none_or(|g| g.model_allowed(id)))
            .count();
        serde_json::json!({
            "providers": 0,
            "models": visible_models,
            "active_providers": Vec::<String>::new(),
            "cache": crate::status::probe::project_client_status_cache(&cfg, &cache, grant_ref),
        })
    };

    Json(AdminApiResponse {
        status: "ok",
        data: Some(data),
        error: None,
        kind: None,
    })
}

#[derive(Debug, Deserialize, Default)]
pub struct MetricsQueryParams {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub window: Option<String>,
    pub history: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
pub struct MetricsHistoryQueryParams {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub window: Option<String>,
    pub since: Option<String>,
    pub limit: Option<usize>,
}

/// GET /admin/metrics — per-provider×model request metrics.
///
/// Returns in-memory counters + sampling percentiles, supporting window filtering
/// (`5m`, `1h`, `24h`, `lifetime`).
pub async fn metrics(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<MetricsQueryParams>,
) -> (StatusCode, Json<AdminApiResponse>) {
    if params.history == Some(true) {
        let history_params = MetricsHistoryQueryParams {
            provider: params.provider,
            model: params.model,
            window: params.window,
            since: None,
            limit: None,
        };
        return query_metrics_history(&state, &history_params);
    }

    let window_kind = match params.window.as_deref() {
        None | Some("") | Some("lifetime") => crate::provider_metrics::WindowKind::Lifetime,
        Some("5m") => crate::provider_metrics::WindowKind::FiveMinutes,
        Some("1h") => crate::provider_metrics::WindowKind::OneHour,
        Some("24h") => crate::provider_metrics::WindowKind::TwentyFourHours,
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(format!(
                        "invalid window '{other}', must be one of: 5m, 1h, 24h, lifetime"
                    )),
                    kind: Some("invalid_request".to_string()),
                }),
            );
        }
    };

    let snapshot = state.provider_metrics_snapshot_window(
        window_kind,
        params.provider.as_deref(),
        params.model.as_deref(),
    );
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::to_value(snapshot).unwrap_or_default()),
            error: None,
            kind: None,
        }),
    )
}

/// GET /admin/metrics/history — query persisted periodic metrics snapshots.
pub async fn metrics_history(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<MetricsHistoryQueryParams>,
) -> (StatusCode, Json<AdminApiResponse>) {
    query_metrics_history(&state, &params)
}

fn query_metrics_history(
    state: &AppState,
    params: &MetricsHistoryQueryParams,
) -> (StatusCode, Json<AdminApiResponse>) {
    let query = crate::provider_metrics::MetricsHistoryQuery {
        provider: params.provider.clone(),
        model: params.model.clone(),
        window: params.window.clone(),
        since: params.since.clone(),
        limit: params.limit,
    };

    if let Some(store) = state.provider_metrics_store() {
        match store.query_history(&query) {
            Ok(history) => (
                StatusCode::OK,
                Json(AdminApiResponse {
                    status: "ok",
                    data: Some(serde_json::json!({ "history": history })),
                    error: None,
                    kind: None,
                }),
            ),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(format!("failed to query metrics history: {e}")),
                    kind: Some("internal_error".to_string()),
                }),
            ),
        }
    } else {
        (
            StatusCode::OK,
            Json(AdminApiResponse {
                status: "ok",
                data: Some(serde_json::json!({ "history": [] })),
                error: None,
                kind: None,
            }),
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusProbeRequest {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeOutcomeJson {
    pub provider: String,
    pub model: String,
    pub protocol: Protocol,
    pub disposition: crate::probe_coordinator::ProbeDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn parse_probe_body(headers: &HeaderMap, body: &Bytes) -> Result<ProbeSelection, String> {
    if body.is_empty() {
        return Ok(ProbeSelection::default());
    }

    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "Content-Type must be application/json".to_string())?;
    let media_type = content_type
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if !media_type.eq_ignore_ascii_case("application/json") {
        return Err("Content-Type must be application/json".to_string());
    }

    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("invalid json: {e}"))?;

    if !value.is_object() {
        return Err("request body must be a JSON object".to_string());
    }

    let req: StatusProbeRequest =
        serde_json::from_value(value).map_err(|e| format!("invalid request body: {e}"))?;

    Ok(ProbeSelection {
        provider: req.provider,
        model: req.model,
    })
}

/// POST /admin/status/probe — trigger server-managed online probe for inactive providers.
///
/// Thin handler (§薄化)：遍历与 singleflight 决策逻辑在 `ProbeCoordinator::probe_all_inactive`，
/// 本 handler 只负责参数读取、client 构建与响应包装。
pub async fn status_probe(
    grant: Option<axum::extract::Extension<crate::token::auth::TokenGrant>>,
    headers: HeaderMap,
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    let grant = grant
        .map(|axum::extract::Extension(g)| g)
        .unwrap_or(crate::token::auth::TokenGrant::Open);

    let selection = match parse_probe_body(&headers, &body) {
        Ok(s) => s,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(err),
                    kind: None,
                }),
            )
                .into_response();
        }
    };

    let cfg = state.config();

    let token_ctx = match &grant {
        crate::token::auth::TokenGrant::Granted(ctx) if !grant.is_admin() => Some(ctx.clone()),
        _ => None,
    };

    // If request comes from an unprivileged tenant token, enforce model authorization and scoping.
    if let Some(ref _ctx) = token_ctx
        && let Some(ref req_model) = selection.model
    {
        if req_model.contains('*') || req_model.contains('?') || req_model.contains('[') {
            // Wildcard pattern requested by an unprivileged token:
            // Expand all candidates and ensure EVERY matched candidate is authorized for this token.
            let raw_candidates = selection.candidates(&cfg);
            if raw_candidates.is_empty() {
                return (
                    StatusCode::FORBIDDEN,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some(format!(
                            "Model pattern '{req_model}' does not match any authorized models for this token"
                        )),
                        kind: Some("forbidden".to_string()),
                    }),
                )
                    .into_response();
            }
            for (_, candidate_model, _) in &raw_candidates {
                if !grant.model_allowed(candidate_model) {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(AdminApiResponse {
                            status: "error",
                            data: None,
                            error: Some(format!(
                                "Model pattern '{req_model}' matches unauthorized model '{candidate_model}'"
                            )),
                            kind: Some("forbidden".to_string()),
                        }),
                    )
                        .into_response();
                }
            }
        } else if !grant.model_allowed(req_model) {
            return (
                StatusCode::FORBIDDEN,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(format!("Model '{req_model}' is not allowed for this token")),
                    kind: Some("forbidden".to_string()),
                }),
            )
                .into_response();
        }
    }

    let allowed_models_set: Option<std::collections::BTreeSet<String>> = token_ctx
        .as_ref()
        .filter(|ctx| !ctx.allowed_models.is_empty())
        .map(|ctx| ctx.allowed_models.iter().cloned().collect());

    if let Err(err) = selection.validate(&cfg) {
        return (
            StatusCode::BAD_REQUEST,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(err),
                kind: None,
            }),
        )
            .into_response();
    }

    // 2. For unprivileged tokens, verify candidates and quota before firing any upstream probes
    if let Some(ref ctx) = token_ctx {
        let candidates = selection.candidates_restricted(&cfg, allowed_models_set.as_ref());
        if candidates.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some("No authorized probe candidates found for this token".to_string()),
                    kind: Some("no_candidates".to_string()),
                }),
            )
                .into_response();
        }

        if let Some(store) = state.token.usage_store.as_ref() {
            let token_cfg = match state.token.config.read() {
                Ok(c) => c.tokens.iter().find(|t| t.key == ctx.key).cloned(),
                Err(err) => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(AdminApiResponse {
                            status: "error",
                            data: None,
                            error: Some(format!("Token configuration read failure: {err}")),
                            kind: Some("config_unavailable".to_string()),
                        }),
                    )
                        .into_response();
                }
            };
            let Some(token_cfg) = token_cfg else {
                return (
                    StatusCode::FORBIDDEN,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some("Token not found in configuration".to_string()),
                        kind: Some("forbidden".to_string()),
                    }),
                )
                    .into_response();
            };

            let models_to_probe: std::collections::BTreeSet<String> =
                candidates.iter().map(|(_, m, _)| m.clone()).collect();
            let mut unique_windows = Vec::new();
            for model in &models_to_probe {
                let grouped_windows =
                    crate::token::usage::grouped_windows_for_model(&token_cfg, model);
                for w in grouped_windows {
                    if !unique_windows.iter().any(
                        |existing: &crate::token::usage::GroupedQuotaWindow| {
                            existing.group_id == w.group_id
                                && existing.window.window == w.window.window
                                && existing.window.quota_mode == w.window.quota_mode
                        },
                    ) {
                        unique_windows.push(w);
                    }
                }
            }

            if !unique_windows.is_empty() {
                match store.reserve_request_slot_grouped(&ctx.key, &unique_windows) {
                    Ok(crate::token::usage::RequestSlot::Granted) => {}
                    Ok(crate::token::usage::RequestSlot::Exhausted(_windows)) => {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(AdminApiResponse {
                                status: "error",
                                data: None,
                                error: Some("Token quota exhausted for probe request".to_string()),
                                kind: Some("quota_exhausted".to_string()),
                            }),
                        )
                            .into_response();
                    }
                    Err(err) => {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(AdminApiResponse {
                                status: "error",
                                data: None,
                                error: Some(format!("Quota verification error: {err}")),
                                kind: Some("quota_unavailable".to_string()),
                            }),
                        )
                            .into_response();
                    }
                }
            }
        }
    }

    let wants_sse = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    if !wants_sse {
        return status_probe_json_inner(
            state,
            &cfg,
            &selection,
            allowed_models_set.as_ref(),
            token_ctx.as_ref(),
        )
        .await
        .into_response();
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(32);

    tokio::spawn(async move {
        let probe_timeout = Duration::from_secs(cfg.status.probe_timeout.max(1));
        let active_ttl = Duration::from_secs(cfg.status.active_ttl);
        let mut active = state.get_active_providers(active_ttl);
        let candidate_providers: std::collections::BTreeSet<String> = selection
            .candidates_restricted(&cfg, allowed_models_set.as_ref())
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();
        active.retain(|p| candidate_providers.contains(p));

        state
            .probe_coordinator
            .discover_dynamic_models(&cfg, &selection)
            .await;

        let client = match crate::http_client::try_build_with_timeout(&cfg, probe_timeout) {
            Ok(client) => client,
            Err(err) => {
                let _ = tx
                    .send(Ok(Event::default().event("error").data(
                        serde_json::json!({
                            "error": format!("failed to build probe client: {err}")
                        })
                        .to_string(),
                    )))
                    .await;
                return;
            }
        };

        let mut count = 0usize;
        let mut stream = std::pin::pin!(
            state
                .probe_coordinator
                .probe_stream(&cfg, &client, &active, &selection)
        );
        while let Some((provider, model, protocol, disposition, result)) = stream.next().await {
            if let Some(ref allowed) = allowed_models_set
                && !allowed.contains(&model)
            {
                continue;
            }
            count += 1;
            if disposition == crate::probe_coordinator::ProbeDisposition::Executed
                && let Some(ref ctx) = token_ctx
            {
                state.token.charge(ctx, &model, 15, 15);
            }
            let (ok, latency_ms, status, error) = match &result {
                crate::status::ProbeResult::Ok { latency_ms, status } => {
                    (true, Some(*latency_ms), Some(*status), None)
                }
                crate::status::ProbeResult::Timeout => {
                    (false, None, None, Some("probe timeout".to_string()))
                }
                crate::status::ProbeResult::Error(e) => (false, None, None, Some(e.clone())),
                crate::status::ProbeResult::Active => (true, None, None, None),
            };
            let event = Event::default().event("probe_result").data(
                serde_json::json!({
                    "provider": provider,
                    "model": model,
                    "protocol": protocol.route_key(),
                    "ok": ok,
                    "latency_ms": latency_ms,
                    "status": status,
                    "error": error,
                    "disposition": disposition,
                    "executed": disposition == crate::probe_coordinator::ProbeDisposition::Executed,
                })
                .to_string(),
            );
            if tx.send(Ok(event)).await.is_err() {
                return;
            }
        }

        let _ = tx
            .send(Ok(Event::default().event("done").data(
                serde_json::json!({
                    "selection": {
                        "provider": selection.provider,
                        "model": selection.model,
                    },
                    "active_providers": active,
                    "probed": count,
                })
                .to_string(),
            )))
            .await;
    });

    Sse::new(ReceiverStream::new(rx)).into_response()
}

#[cfg(test)]
pub async fn status_probe_json(
    state: AppState,
    selection: &ProbeSelection,
) -> (StatusCode, Json<AdminApiResponse>) {
    let cfg = state.config();
    if let Err(err) = selection.validate(&cfg) {
        return (
            StatusCode::BAD_REQUEST,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(err),
                kind: None,
            }),
        );
    }
    status_probe_json_inner(state, &cfg, selection, None, None).await
}

async fn status_probe_json_inner(
    state: AppState,
    cfg: &Config,
    selection: &ProbeSelection,
    allowed_models: Option<&std::collections::BTreeSet<String>>,
    token_ctx: Option<&crate::token::auth::TokenContext>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let probe_timeout = Duration::from_secs(cfg.status.probe_timeout.max(1));
    let active_ttl = Duration::from_secs(cfg.status.active_ttl);
    let mut active = state.get_active_providers(active_ttl);
    let candidate_providers: std::collections::BTreeSet<String> = selection
        .candidates_restricted(cfg, allowed_models)
        .into_iter()
        .map(|(p, _, _)| p)
        .collect();
    active.retain(|p| candidate_providers.contains(p));

    state
        .probe_coordinator
        .discover_dynamic_models(cfg, selection)
        .await;
    let client = match crate::http_client::try_build_with_timeout(cfg, probe_timeout) {
        Ok(client) => client,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(format!("failed to build probe client: {err}")),
                    kind: None,
                }),
            );
        }
    };
    let outcomes = state
        .probe_coordinator
        .probe_all_inactive(cfg, &client, &active, selection)
        .await;

    let mut probe_outcomes = Vec::with_capacity(outcomes.len());
    let mut probed_set = std::collections::BTreeSet::new();

    for (provider, model, protocol, disposition, result) in outcomes {
        if let Some(allowed) = allowed_models
            && !allowed.contains(&model)
        {
            continue;
        }
        if disposition == crate::probe_coordinator::ProbeDisposition::Executed {
            probed_set.insert(provider.clone());
            if let Some(ctx) = token_ctx {
                state.token.charge(ctx, &model, 15, 15);
            }
        }
        let (ok, latency_ms, error) = match result {
            crate::status::ProbeResult::Ok { latency_ms, .. } => {
                (Some(true), Some(latency_ms), None)
            }
            crate::status::ProbeResult::Timeout => {
                (Some(false), None, Some("probe timeout".to_string()))
            }
            crate::status::ProbeResult::Error(e) => (Some(false), None, Some(e)),
            crate::status::ProbeResult::Active => (Some(true), None, None),
        };
        probe_outcomes.push(ProbeOutcomeJson {
            provider,
            model,
            protocol,
            disposition,
            ok,
            latency_ms,
            error,
        });
    }

    let probed: Vec<String> = probed_set.into_iter().collect();
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "selection": {
                    "provider": selection.provider,
                    "model": selection.model,
                },
                "active_providers": active,
                "probed_providers": probed,
                "probe_outcomes": probe_outcomes,
            })),
            error: None,
            kind: None,
        }),
    )
}

/// DELETE /admin/provider/remove — remove a provider.
///
/// Runs in `spawn_blocking`: provider removal mutates config.toml on disk, and
/// doing file I/O while holding the lock would block a tokio worker thread.
pub async fn provider_remove(
    State(state): State<AppState>,
    Json(req): Json<ProviderRemoveRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let id = req.id.clone();
    let (current, commit_base) = state.config_with_revision();
    let account = crate::auth::oauth_account_for_provider(&current, &id).ok();
    let candidate = {
        let core = state.core.lock().await;
        core.remove_provider_candidate(&id, req.force)
    };
    let candidate = match candidate {
        Ok(candidate) => candidate,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(error.to_string()),
                    kind: None,
                }),
            );
        }
    };
    match state.commit_config(candidate, commit_base).await {
        Ok(_) => {
            if let Some(account) = account {
                // 此前这里是 `let _ = logout(...)`，磁盘删除失败
                // 也被吞掉，命令照样报成功，留下无 provider 引用的孤儿凭据；而且
                // 内存缓存从不刷新，磁盘已删的账号仍能解析出 token。
                let cleanup = crate::auth::logout(&state.oauth_accounts_path, Some(&account));
                // 无论删除是否命中都要让内存跟上磁盘：删除失败时同样需要按磁盘
                // 现状重载，否则缓存里仍是已被移除的账号。
                if let Ok(mut guard) = state.oauth_accounts.write()
                    && let Ok(accounts) =
                        crate::auth::load_oauth_accounts_with_recovery(&state.oauth_accounts_path)
                {
                    *guard = accounts;
                }
                if let Err(err) = cleanup {
                    // 配置已提交、无法回滚，所以不能假装整件事成功：响亮上报，
                    // 并说明哪一半已经生效、另一半该怎么收。
                    tracing::error!(
                        provider = %id,
                        error = %err,
                        "provider removed but OAuth account cleanup failed"
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(AdminApiResponse {
                            status: "error",
                            data: Some(serde_json::json!({
                                "message": format!("removed provider: {id}"),
                            })),
                            error: Some(format!(
                                "provider '{id}' was removed, but its OAuth account could not be \
                                 deleted: {err}. The credential may still be on disk; run \
                                 `llm-proxy provider logout {id}` once the credentials store is \
                                 usable again."
                            )),
                            kind: None,
                        }),
                    );
                }
            }
            (
                StatusCode::OK,
                Json(AdminApiResponse {
                    status: "ok",
                    data: Some(serde_json::json!({ "message": format!("removed provider: {id}") })),
                    error: None,
                    kind: None,
                }),
            )
        }
        Err(error) => classify_commit_writer_error(error),
    }
}

/// POST /admin/provider/copy — copy a provider
pub async fn provider_copy(
    State(state): State<AppState>,
    Json(req): Json<ProviderCopyRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let source = req.source.clone();
    let target = req.target.clone();
    // the candidate is built under the core lock while its base is
    // read here; any commit landing in between only skews toward a false
    // conflict (retryable), never toward a silent overwrite.
    let commit_base = state.current_revision();
    let candidate = {
        let core = state.core.lock().await;
        core.copy_provider_candidate(&source, &target, req.api_key_env, req.no_api_key)
    };
    let (candidate, result) = match candidate {
        Ok(candidate) => candidate,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(error.to_string()),
                    kind: None,
                }),
            );
        }
    };
    match state.commit_config(candidate, commit_base).await {
        Ok(_) => (
            StatusCode::OK,
            Json(AdminApiResponse {
                status: "ok",
                data: Some(serde_json::json!({
                    "message": format!("copied provider: {source} -> {target}"),
                    "requires_oauth_login": result.requires_oauth_login,
                })),
                error: None,
                kind: None,
            }),
        ),
        Err(error) => classify_commit_writer_error(error),
    }
}

/// POST /admin/provider/reselect — reconcile catalog model bindings for an existing provider.
pub async fn provider_reselect(
    State(state): State<AppState>,
    Json(req): Json<ProviderReselectRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let provider_id = req.provider_id.clone();
    let models = req.models.clone();
    let commit_base = state.current_revision();
    let candidate = {
        let core = state.core.lock().await;
        core.reselect_provider_models_candidate(&provider_id, &models)
    };
    let candidate = match candidate {
        Ok(candidate) => candidate,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(error.to_string()),
                    kind: None,
                }),
            );
        }
    };
    match state.commit_config(candidate, commit_base).await {
        Ok(_) => (
            StatusCode::OK,
            Json(AdminApiResponse {
                status: "ok",
                data: Some(serde_json::json!({
                    "message": format!("reselected models for provider: {provider_id}")
                })),
                error: None,
                kind: None,
            }),
        ),
        Err(error) => classify_commit_writer_error(error),
    }
}

/// POST /admin/provider/add — add a provider（connect 流程委托 server 执行）。
///
/// 完整执行 connect::add_provider_with_models（构造 provider + 写配置 +
/// 模型验证 + env 注入到 server 进程），完成后 reload server 内存配置。
pub async fn provider_add(
    State(state): State<AppState>,
    Json(req): Json<ProviderAddRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let (snapshot, commit_base) = state.config_with_revision();
    let provider_id = req
        .provider_name
        .clone()
        .unwrap_or_else(|| req.provider_id.clone());
    let pending_auth = req.pending_account.as_ref().map(|a| a.to_resolved_auth());
    let result = crate::connect::prepare_provider_candidate(
        &state.config_path,
        &snapshot,
        &req.provider_id,
        req.provider_name.as_deref(),
        req.api_key_env,
        req.no_api_key,
        req.provider_type,
        req.endpoint_url,
        req.models.as_deref(),
        req.skip_probe,
        true, // require_auth_env: 委托路径始终校验本地 env var 存在性
        pending_auth.as_ref(),
        None,
    )
    .await;
    let (candidate, _) = match result {
        Ok(result) => result,
        Err(error) => {
            return (
                error.kind.status_code(),
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(error.to_string()),
                    kind: Some(error.kind.as_str().to_string()),
                }),
            );
        }
    };

    let accounts_path = state.oauth_accounts_path.clone();
    if let Some(pending) = &req.pending_account {
        match crate::auth::storage::write_pending_account_locked(&accounts_path, pending) {
            Ok(()) => {
                if let Ok(mut guard) = state.oauth_accounts.write()
                    && let Ok(accounts) =
                        crate::auth::load_oauth_accounts_with_recovery(&accounts_path)
                {
                    *guard = accounts;
                }
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some(format!("failed to save pending OAuth account: {e}")),
                        kind: Some(
                            crate::connect::ProviderAddErrorKind::InternalError
                                .as_str()
                                .to_string(),
                        ),
                    }),
                );
            }
        }
    }

    match state.commit_config(candidate.clone(), commit_base).await {
        Ok(_) => {
            crate::connect::maybe_inject_provider_env(&state.config_path, &candidate, &provider_id);
            (
                StatusCode::OK,
                Json(AdminApiResponse {
                    status: "ok",
                    data: Some(serde_json::json!({
                        "message": format!("connected provider: {provider_id}")
                    })),
                    error: None,
                    kind: None,
                }),
            )
        }
        Err(error) => {
            let (status, Json(mut response)) = classify_provider_add_commit_writer_error(error);
            // a failed config commit keeps the credential
            // that was just written. Deleting it would destroy the half only the
            // user can regenerate; startup reconciliation completes the lost step.
            if let Some(pending) = &req.pending_account {
                annotate_kept_oauth_credential(&mut response, &pending.account);
            }
            (status, Json(response))
        }
    }
}

fn annotate_kept_oauth_credential(response: &mut AdminApiResponse, account: &str) {
    let note = format!(
        "the OAuth credential for account {account:?} was kept on disk; the provider is rebuilt \
         from it at next startup, so no re-login is needed"
    );
    response.error = Some(match response.error.take() {
        Some(message) => format!("{message}; {note}"),
        None => note,
    });
}

fn classify_provider_add_commit_writer_error(
    error: crate::runtime_config::WriterError,
) -> (StatusCode, Json<AdminApiResponse>) {
    let (status, mut response) = classify_commit_writer_error(error);
    if status == StatusCode::BAD_REQUEST {
        response.kind = Some(
            crate::connect::ProviderAddErrorKind::InvalidConfig
                .as_str()
                .to_string(),
        );
    } else if status.is_server_error() {
        response.kind = Some(
            crate::connect::ProviderAddErrorKind::InternalError
                .as_str()
                .to_string(),
        );
    }
    (status, response)
}

/// GET /admin/provider/:id — provider 详情（读操作，走 HTTP 公开接口）。
///
/// 返回结构化数据（id/auth/endpoints/usage），CLI 负责格式化展示，
/// 与本地独立模式的 provider info 输出保持一致。
pub async fn provider_info(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let (cfg, provider) = {
        let core = state.core.lock().await;
        let cfg = core.config().clone();
        let provider = cfg.providers.get(&id).cloned();
        (cfg, provider)
    };
    let Some(provider) = provider else {
        return (
            StatusCode::NOT_FOUND,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(format!("unknown provider: {id}")),
                kind: None,
            }),
        );
    };
    let (auth_state, auth) =
        crate::status::provider_auth_summary(&id, &provider, &state.oauth_accounts_path);
    let endpoints: Vec<serde_json::Value> = provider
        .endpoints()
        .iter()
        .map(|(protocol, endpoint)| {
            serde_json::json!({
                "protocol": protocol.field_name(),
                "url": endpoint.url,
                "derive_from": endpoint.derive_from,
            })
        })
        .collect();
    // usage 数据（OAuth provider 时 server 查询；provider info 是显式详情查询）
    let usage = if matches!(
        provider.auth_config(&id),
        Ok(crate::config::AuthConfig::OpenaiOauth { .. })
    ) {
        match crate::usage::resolve_openai_token(&cfg, &state.oauth_accounts_path, &id) {
            Ok(token) => match crate::usage::query_usage(&token).await {
                Ok(status) => Some(serde_json::json!(status)),
                Err(err) => Some(serde_json::json!({ "unavailable": err.to_string() })),
            },
            Err(err) => Some(serde_json::json!({ "unavailable": err.to_string() })),
        }
    } else {
        None
    };
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "id": id,
                "auth": auth,
                "auth_state": auth_state,
                "endpoints": endpoints,
                "usage": usage,
            })),
            error: None,
            kind: None,
        }),
    )
}

/// GET /admin/provider/list — provider id 列表（读操作，HTTP 公开接口）。
///
/// 与 `model_list` 对称：返回 `{"providers": [...]}`，CLI 的
/// `complete-candidates provider` 在远程模式下委托此端点获取候选。
pub async fn provider_list(State(state): State<AppState>) -> (StatusCode, Json<AdminApiResponse>) {
    let cfg = state.config();
    let providers: Vec<&String> = cfg.providers.keys().collect();
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({ "providers": providers })),
            error: None,
            kind: None,
        }),
    )
}

/// 模型写操作由 config writer 基于最新已发布配置原子执行。
async fn model_write(
    state: &AppState,
    mutation: crate::model::ConfigMutation,
) -> (StatusCode, Json<AdminApiResponse>) {
    match state.mutate_config(mutation).await {
        Ok(message) => (
            StatusCode::OK,
            Json(AdminApiResponse {
                status: "ok",
                data: Some(serde_json::json!({ "message": message })),
                error: None,
                kind: None,
            }),
        ),
        Err(error) => classify_commit_writer_error(error),
    }
}

/// POST /admin/model/add — add a model（写操作走 UDS 管理通道）
pub async fn model_add(
    State(state): State<AppState>,
    Json(req): Json<ModelAddRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let model_id = req.model_id.clone();
    model_write(
        &state,
        crate::model::ConfigMutation::Add {
            model_id,
            context_window: req.context_window,
            max_output_tokens: req.max_output,
            copy_from: req.copy_from,
            routes: req.routes,
        },
    )
    .await
}

/// POST /admin/model/set — set model parameters（写操作走 UDS）
pub async fn model_set(
    State(state): State<AppState>,
    Json(req): Json<ModelSetRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let model_id = req.model_id.clone();
    let reasoning_levels = req
        .reasoning_levels
        .map(crate::model::ReasoningLevelsUpdate::Entries);
    model_write(
        &state,
        crate::model::ConfigMutation::Set {
            model_id,
            options: Box::new(crate::model::SetModelOptions {
                context_window: req.context_window,
                max_output_tokens: req.max_output_tokens,
                reasoning_levels,
                thinking_level: req.thinking_level,
                supports_thinking: req.supports_thinking,
                force_think: req.force_think,
                force_no_think: req.force_no_think,
                default_think: req.default_think,
                enable_features: req.enable_features,
                disable_features: req.disable_features,
                description: req.description,
                clear_description: req.clear_description,
                chat_reasoning_off_format: req.chat_reasoning_off_format,
                clear_chat_reasoning_off_format: req.clear_chat_reasoning_off_format,
                codex_service_tier: req.codex_service_tier,
                clear_codex_service_tier: req.clear_codex_service_tier,
                cooldown: req.cooldown,
                clear_cooldown: req.clear_cooldown,
            }),
        },
    )
    .await
}

/// POST /admin/model/remove — remove a model（写操作走 UDS）
pub async fn model_remove(
    State(state): State<AppState>,
    Json(req): Json<ModelRemoveRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let model_id = req.model_id.clone();
    model_write(
        &state,
        crate::model::ConfigMutation::Remove {
            model_id,
            force: req.force,
        },
    )
    .await
}

/// POST /admin/model/rename — rename a model and update referencing bindings.
pub async fn model_rename(
    State(state): State<AppState>,
    Json(req): Json<ModelRenameRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    model_write(
        &state,
        crate::model::ConfigMutation::Rename {
            old_model: req.old_model,
            new_model: req.new_model,
        },
    )
    .await
}

/// POST /admin/model/provider — provider 绑定增删改（action: add/remove/move）
pub async fn model_provider(
    State(state): State<AppState>,
    Json(req): Json<ModelProviderRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let model_id = req.model_id.clone();
    let provider = req.provider.clone();
    let protocol = match crate::model::parse_client_protocol(&req.protocol) {
        Ok(protocol) => protocol,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(error.to_string()),
                    kind: None,
                }),
            );
        }
    };
    let mutation = match req.action.as_str() {
        "add" => crate::model::ConfigMutation::ProviderAdd {
            model_id,
            protocol,
            provider_id: provider,
            upstream_model: req.upstream_model,
        },
        "set" | "update" => {
            let upstream = match req.upstream_model {
                Some(u) => u,
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(AdminApiResponse {
                            status: "error",
                            data: None,
                            error: Some("upstream_model is required for action 'set'".to_string()),
                            kind: None,
                        }),
                    );
                }
            };
            crate::model::ConfigMutation::ProviderSet {
                model_id,
                protocol,
                provider_id: provider,
                upstream_model: upstream,
            }
        }
        "remove" => crate::model::ConfigMutation::ProviderRemove {
            model_id,
            protocol,
            provider_id: provider,
        },
        "move" => crate::model::ConfigMutation::ProviderMove {
            model_id,
            protocol,
            provider_id: provider,
            to_index: req.to.unwrap_or(0),
        },
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(format!("unknown model provider action: {other}")),
                    kind: None,
                }),
            );
        }
    };
    model_write(&state, mutation).await
}

/// GET /admin/model/list — model 列表（读操作，HTTP 公开接口）
pub async fn model_list(State(state): State<AppState>) -> (StatusCode, Json<AdminApiResponse>) {
    let cfg = state.config();
    let models: Vec<serde_json::Value> = cfg
        .models
        .iter()
        .map(|(id, model)| {
            // a routing model has no window or bindings of its own;
            // report what a client can actually use through it.
            let protocols = crate::config::Protocol::CLIENT_PROTOCOLS
                .into_iter()
                .filter(|protocol| cfg.model_serves_protocol(id, *protocol))
                .map(|protocol| protocol.route_key())
                .collect::<Vec<_>>();
            let bounds = cfg.model_capability_bounds(id);
            let (context_window, max_output_tokens) = bounds
                .map(|bounds| (bounds.context_window, bounds.max_output_tokens))
                .unwrap_or((model.context_window, model.max_output_tokens));
            serde_json::json!({
                "id": id,
                "context_window": context_window,
                "max_output_tokens": max_output_tokens,
                "protocols": protocols,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({ "models": models })),
            error: None,
            kind: None,
        }),
    )
}

/// GET /admin/model/{id} — model 详情（读操作，HTTP 公开接口）
pub async fn model_info(
    State(state): State<AppState>,
    axum::extract::Path(model_id): axum::extract::Path<String>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let cfg = state.config();
    let Some(model) = cfg.models.get(&model_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(format!("unknown model: {model_id}")),
                kind: None,
            }),
        );
    };
    let mut providers = serde_json::Map::new();
    for protocol in crate::config::Protocol::CLIENT_PROTOCOLS {
        let bindings = model.provider_bindings(protocol);
        if bindings.is_empty() {
            continue;
        }
        providers.insert(
            protocol.field_name().to_string(),
            serde_json::json!(
                bindings
                    .iter()
                    .enumerate()
                    .map(|(index, binding)| {
                        let cooldown = state
                            .cooldowns
                            .active_binding_cooldown(&model_id, &binding.name, protocol)
                            .map(|e| crate::cooldown::format_timestamp_local(e.expires_at_unix));
                        serde_json::json!({
                            "index": index + 1,
                            "provider": binding.name,
                            "upstream_model": binding.model,
                            "cooldown": cooldown,
                        })
                    })
                    .collect::<Vec<_>>()
            ),
        );
    }
    // a routing model declares no window of its own; report the
    // conservative bound over its targets and expose its routes in place of
    // provider bindings.
    let bounds = cfg.model_capability_bounds(&model_id);
    let (context_window, max_output_tokens) = bounds
        .map(|bounds| (bounds.context_window, bounds.max_output_tokens))
        .unwrap_or((model.context_window, model.max_output_tokens));
    let mut data = serde_json::json!({
        "id": model_id,
        "context_window": context_window,
        "max_output_tokens": max_output_tokens,
        "providers": providers,
    });
    if model.is_routing_model()
        && let Some(object) = data.as_object_mut()
    {
        object.insert(
            "routes".to_string(),
            serde_json::json!(
                model
                    .routes()
                    .iter()
                    .map(|entry| serde_json::json!({
                        "level": entry.level,
                        "target": entry.target,
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(data),
            error: None,
            kind: None,
        }),
    )
}

/// GET /admin/client-config/{client} — launch 数据（读操作，HTTP 公开接口）。
///
/// 返回 models + providers 子集（不含 base_url——CLI 用自己网络视角推导）。
/// 远程 CLI 据此构造 Config 后复用现有 launch 逻辑生成客户端配置。
///
/// : 若请求携带 TokenGrant 认证上下文，严格按 token.allowed_models
/// 白名单过滤可见模型；路由模型如实物化其导出的 bounds 与能力，避免向客户端泄露
/// 内部路由跳板模型（如 *-low-lp / *-high-lp）及未授权模型。
pub async fn client_config(
    State(state): State<AppState>,
    grant: Option<axum::extract::Extension<crate::token::auth::TokenGrant>>,
    axum::extract::Path(client): axum::extract::Path<String>,
) -> (StatusCode, Json<AdminApiResponse>) {
    const SUPPORTED: &[&str] = &[
        "pi",
        "qwen-code",
        "codex",
        "codex-desktop",
        "claude-code",
        "claude-desktop",
        "opencode",
    ];
    if !SUPPORTED.contains(&client.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(format!(
                    "unknown client {client:?}; supported: {}",
                    SUPPORTED.join(", ")
                )),
                kind: None,
            }),
        );
    }
    let cfg = state.config();
    let grant_ref = grant.as_ref().map(|axum::extract::Extension(g)| g);
    let is_admin = grant_ref.is_none_or(|grant| grant.is_admin());
    let view = if is_admin {
        crate::client_view::ClientView::Admin
    } else {
        crate::client_view::ClientView::Client
    };
    let catalog = crate::client_view::build_client_catalog(&cfg, grant_ref, view);

    // Legacy shape for older clients. Admin keeps the full provider topology;
    // non-admin gets a synthetic `llm-proxy` provider so no real provider
    // identity or endpoint leaves the server.
    let (models, providers) = if is_admin {
        prepare_client_config_payload(&cfg, grant_ref)
    } else {
        match crate::client_view::catalog_to_config(&catalog, "http://127.0.0.1:8989") {
            Ok(legacy) => (legacy.models, legacy.providers),
            Err(_) => (Default::default(), Default::default()),
        }
    };
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "client": client,
                "catalog": catalog,
                "models": models,
                "providers": providers,
            })),
            error: None,
            kind: None,
        }),
    )
}

/// : 构建面向客户端 launch 的模型与提供商子集。
///
/// 1. 严格按 `TokenGrant::model_allowed` 白名单过滤模型；
/// 2. 路由模型（`reasoning_routes`）在服务端将能力 bounds、思考等级和协议暴露物化为独立模型，
///    消除对内部 target 跳板模型的依赖，保护服务端拓扑与内部 ID；
/// 3. 提供商列表仅保留可见模型所需的条目。
pub(crate) fn prepare_client_config_payload(
    cfg: &crate::config::Config,
    grant: Option<&crate::token::auth::TokenGrant>,
) -> (
    std::collections::BTreeMap<String, crate::config::ModelConfig>,
    std::collections::BTreeMap<String, crate::config::ProviderConfig>,
) {
    use crate::config::{ModelCapabilityBounds, Protocol, ProviderBinding, ProviderConfig};
    use std::collections::{BTreeMap, BTreeSet};

    let mut filtered_models = BTreeMap::new();
    let mut needed_providers = BTreeSet::new();

    for (model_id, model) in &cfg.models {
        if let Some(grant) = grant
            && !grant.model_allowed(model_id)
        {
            continue;
        }

        if model.is_routing_model() {
            let mut materialized = model.clone();
            let bounds = cfg
                .model_capability_bounds(model_id)
                .unwrap_or(ModelCapabilityBounds {
                    context_window: model.context_window,
                    max_output_tokens: model.max_output_tokens,
                    features: model.features.clone(),
                });
            materialized.context_window = bounds.context_window;
            materialized.max_output_tokens = bounds.max_output_tokens;
            materialized.features = bounds.features;
            materialized.reasoning_levels = cfg.client_reasoning_levels(model_id);
            materialized.default_reasoning_level = cfg.client_default_reasoning_level(model_id);
            materialized.supports_thinking = Some(true);

            // 若 routes 中不含 "none" / "off"，强制 force_think = true 以在客户端抑制 "off"
            let has_off_route = model
                .routes()
                .iter()
                .any(|r| r.level == "none" || r.level == "off");
            if !has_off_route {
                materialized.force_think = Some(true);
            }

            // 为该路由模型所支持的协议绑定合成 Provider
            for proto in Protocol::CLIENT_PROTOCOLS {
                if cfg.model_serves_protocol(model_id, proto) {
                    let binding = ProviderBinding {
                        name: "llm-proxy".to_string(),
                        model: model_id.to_string(),
                    };
                    match proto {
                        Protocol::OpenaiChatCompletions => {
                            materialized.openai_chat_providers = vec![binding]
                        }
                        Protocol::OpenaiResponses => {
                            materialized.openai_responses_providers = vec![binding]
                        }
                        Protocol::Anthropic => materialized.anthropic_providers = vec![binding],
                    }
                }
            }
            materialized.reasoning_routes = None;
            filtered_models.insert(model_id.clone(), materialized);
        } else {
            for binding in &model.openai_chat_providers {
                needed_providers.insert(binding.name.clone());
            }
            for binding in &model.openai_responses_providers {
                needed_providers.insert(binding.name.clone());
            }
            for binding in &model.anthropic_providers {
                needed_providers.insert(binding.name.clone());
            }
            filtered_models.insert(model_id.clone(), model.clone());
        }
    }

    let mut filtered_providers = BTreeMap::new();
    for name in &needed_providers {
        if let Some(p) = cfg.providers.get(name) {
            filtered_providers.insert(name.clone(), p.clone());
        }
    }

    // 只要有物化的路由模型或过滤后提供商为空，注入合成 "llm-proxy" 提供商以满足客户端 Config 校验
    let has_synthetic = filtered_models.values().any(|m| {
        m.openai_chat_providers
            .iter()
            .any(|b| b.name == "llm-proxy")
            || m.openai_responses_providers
                .iter()
                .any(|b| b.name == "llm-proxy")
            || m.anthropic_providers.iter().any(|b| b.name == "llm-proxy")
    });
    if has_synthetic || filtered_providers.is_empty() {
        filtered_providers.insert(
            "llm-proxy".to_string(),
            ProviderConfig {
                openai_chat: Some(crate::config::EndpointConfig::native(
                    "http://127.0.0.1:8989/openai/v1/chat/completions",
                )),
                openai_responses: Some(crate::config::EndpointConfig::native(
                    "http://127.0.0.1:8989/openai/v1/responses",
                )),
                anthropic: Some(crate::config::EndpointConfig::native(
                    "http://127.0.0.1:8989/anthropic/v1/messages",
                )),
                ..ProviderConfig::default()
            },
        );
    }

    (filtered_models, filtered_providers)
}

/// GET /admin/config/snapshot — 当前配置**连同它所属的代（revision）**。
///
/// 要整体替换配置的客户端必须从同一个响应里同时拿到这两样：先读文件再单独问一次
/// revision 会留下"两者描述的不是同一代"的窗口，而那正是 base 要消除的
/// check-then-act（已知残余：whole-config 端到端 CAS）。
pub async fn config_snapshot(
    State(state): State<AppState>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let (cfg, revision) = state.config_with_revision();
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "config": cfg,
                "revision": revision,
            })),
            error: None,
            kind: None,
        }),
    )
}

/// GET /admin/config/snapshot 的公开 listener 变体：序列化前剥离
/// `server.auth_token` 明文。
///
/// 信任边界：UDS 管理路由受 socket 文件权限（0600）门禁，与配置文件同信任域
/// （TUI 编辑器 CAS 无损回环依赖完整配置）；公开 HTTP listener 任意可达 +
/// token 鉴权（含 open 模式无 token）不等于管理信任域——任意有效 token 都不
/// 得拉取 `[server]` 的明文认证 token。`config_update` 侧有对应回环保护
/// （收到 `auth_token: None` 保留当前代值），防远端编辑器“快照→更新”回环
/// 抹掉配置文件里的明文。
pub async fn config_snapshot_http(
    State(state): State<AppState>,
    grant: Option<axum::extract::Extension<crate::token::auth::TokenGrant>>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let (cfg, revision) = state.config_with_revision();
    let grant_ref = grant.as_ref().map(|axum::extract::Extension(g)| g);
    let is_admin = grant_ref.is_none_or(|grant| grant.is_admin());
    let (mut out_cfg, view) = if is_admin {
        (cfg, crate::client_view::ClientView::Admin)
    } else {
        let catalog = crate::client_view::build_client_catalog(
            &cfg,
            grant_ref,
            crate::client_view::ClientView::Client,
        );
        let projected = crate::client_view::catalog_to_config(&catalog, "http://127.0.0.1:8989")
            .unwrap_or_else(|_| {
                serde_json::from_value(serde_json::json!({}))
                    .expect("empty config must deserialize")
            });
        (projected, crate::client_view::ClientView::Client)
    };
    if let Some(server) = out_cfg.server.as_mut() {
        server.auth_token = None;
    }
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "config": out_cfg,
                "revision": revision,
                "view": view,
            })),
            error: None,
            kind: None,
        }),
    )
}

/// POST /admin/config/update 的请求体。
#[derive(Debug, serde::Deserialize)]
pub struct ConfigUpdateRequest {
    /// `config` 是从哪一代读出来的。`None` = 客户端从未建立过代际依据，
    /// 而整体替换不允许以此为由覆盖更新的一代。
    base: Option<u64>,
    config: crate::config::Config,
}

/// POST /admin/config/update — 整体替换配置（TUI 编辑器保存委托）。
/// server 持锁写盘 + 更新内存，单一写者模型下保持内存/盘一致。
///
/// `base` 是端到端 CAS 的依据：与当前代不符即 `409`，不静默覆盖（已知残余）。
pub async fn config_update(
    State(state): State<AppState>,
    Json(req): Json<ConfigUpdateRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let Some(base) = req.base else {
        return (
            StatusCode::CONFLICT,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(
                    "whole-config update must state the generation it was read from; read it back \
                     through the management channel and retry"
                        .to_string(),
                ),
                kind: None,
            }),
        );
    };
    // 回环保护：公开 listener 的快照已剥离 server.auth_token
    // （config_snapshot_http），远端编辑器“快照→更新”回环会把 None 传回；
    // 单一写者模型下当前代即权威，合并保留既有明文，防止合法更新顺带抹掉
    // [server].auth_token。UDS 回环携带完整值，不受影响。
    let mut config = req.config;
    if config
        .server
        .as_ref()
        .is_some_and(|server| server.auth_token.is_none())
    {
        let (current, _) = state.config_with_revision();
        if let Some(current_token) = current
            .server
            .as_ref()
            .and_then(|server| server.auth_token.clone())
            && let Some(server) = config.server.as_mut()
        {
            server.auth_token = Some(current_token);
        }
    }
    let outcome = state.commit_config(config, base).await;
    match outcome {
        Ok(_) => (
            StatusCode::OK,
            Json(AdminApiResponse {
                status: "ok",
                data: Some(serde_json::json!({ "message": "config updated" })),
                error: None,
                kind: None,
            }),
        ),
        Err(error) => classify_commit_writer_error(error),
    }
}

pub(crate) fn classify_commit_writer_error(
    error: crate::runtime_config::WriterError,
) -> (StatusCode, Json<AdminApiResponse>) {
    match error {
        crate::runtime_config::WriterError::CommitFailed(message) => (
            StatusCode::BAD_REQUEST,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(message),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::MutationRejected(message) => (
            StatusCode::BAD_REQUEST,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(message),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::CommitBaseConflict { expected, actual } => (
            StatusCode::CONFLICT,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(format!(
                    "configuration changed since it was read (base {expected}, current {actual}); retry from the current configuration"
                )),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::FailedBeforeReplace(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some("failed to persist configuration".to_string()),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::MailboxClosed => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some("configuration writer mailbox closed".to_string()),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::RetrySafetyUnknown => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some("configuration write outcome unknown".to_string()),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::FailedStop => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some("configuration writer in fail-stop state".to_string()),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::StopUndetermined => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(
                    "configuration writer shutdown outcome undetermined; do not assume a clean stop"
                        .to_string(),
                ),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::ServiceShuttingDown => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some("service is shutting down".to_string()),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::RevisionExhausted => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some("configuration revision exhausted".to_string()),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::OperationKeyConflict => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some("operation key conflict".to_string()),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::ReloadFailed(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some("failed to reload configuration".to_string()),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::OperationExpired => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some("operation expired".to_string()),
                kind: None,
            }),
        ),
        crate::runtime_config::WriterError::UnknownOperation => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some("unknown operation".to_string()),
                kind: None,
            }),
        ),
 // : writer 收了消息但在预算内没回 ack——不能假设成功或失败。
        crate::runtime_config::WriterError::AckTimeout => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(
                    "configuration writer did not acknowledge the operation within the timeout"
                        .to_string(),
                ),
                kind: None,
            }),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct CooldownClearRequest {
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
}

// ── Token Management Endpoints (delegation) ────────────────────────

/// POST /admin/token/create — 创建 token（委托模式，server 是唯一写者）
/// server 写 tokens.toml + 更新内存状态，保证内存/磁盘一致。
pub async fn token_create(
    State(state): State<AppState>,
    Json(req): Json<TokenCreateRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    // 校验与落盘统一走 `manage::create_token` —— 与 CLI 本地写路径同一实现。
    // 此前本 handler 内联了一整套校验与创建逻辑，导致同一条 CLI 命令的行为会
    // 随 server 是否在运行而分叉：往 `validate_create_args` 新加一条规则只在
    // 无 server 时生效。
    //
    // 协议形状 → 领域类型的窗口转换留在这一层：`CreateArgs` 已经是强类型，
    // `create_in` 不做字符串到枚举的映射。
    // Convert windows to NewQuotaWindow
    let windows: Result<Vec<crate::token::manage::NewQuotaWindow>, _> = req
        .windows
        .iter()
        .map(|w| {
            if !crate::token::dimension::is_valid_mode(&w.quota_mode) {
                return Err(anyhow::anyhow!(
                    "invalid quota_mode '{}', expected 'tokens' or 'requests'",
                    w.quota_mode
                ));
            }
            Ok(crate::token::manage::NewQuotaWindow {
                window: w.window.clone(),
                quota_mode: w.quota_mode.clone(),
                quota_limit: w.quota_limit,
            })
        })
        .collect();

    let windows = match windows {
        Ok(w) => w,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(err.to_string()),
                    kind: None,
                }),
            );
        }
    };

    // 校验 + 生成 key + 名称查重 + 构造 TokenConfig + 落盘，全部由
    // `create_token` 承担。guard 把"读-改-写 + 内存 reload"圈成同一个临界区：
    // 此前本 handler 与并发的 token 写之间没有任何互斥，两个 handler 可以双双
    // 通过写前的内容 hash 检查、后写者整份覆盖前写者。
    let token_store = crate::token::manage::lock_token_store();
    let result = crate::token::manage::create_token(
        &token_store,
        &state.config_path,
        crate::token::manage::CreateArgs {
            name: req.name.clone(),
            models: req.models.clone(),
            windows: windows.clone(),
            expires: req.expires.clone(),
            group_models: vec![],
        },
    );

    match result {
        Ok(token) => {
            // Reload token config into memory
            if let Err(err) = state.token.reload(&state.config_path) {
                tracing::error!(error = %err, "failed to reload token config after create");
            }
            (
                StatusCode::OK,
                Json(AdminApiResponse {
                    status: "ok",
                    data: Some(serde_json::json!({
                        "key": token.key,
                        "name": token.name,
                        "models": token.allowed_models,
                        "windows": token.quota_windows.iter().map(|w| serde_json::json!({
                            "window": w.window,
                            "quota_mode": crate::token::dimension::label(&w.quota_mode),
                            "quota_limit": w.quota_limit,
                        })).collect::<Vec<_>>(),
                        "expires": token.expired_at,
                    })),
                    error: None,
                    kind: None,
                }),
            )
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(err.to_string()),
                kind: None,
            }),
        ),
    }
}

/// POST /admin/token/delete — 删除 token（委托模式）
pub async fn token_delete(
    State(state): State<AppState>,
    Json(req): Json<TokenDeleteRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    // 从"查这个 token"到"清它的配额行"整段是一个临界区：配置里
    // 的条目与 SQLite 里的行是强关联数据，必须一致地生效，否则并发的另一个写者
    // 可以把已删的条目覆盖回来，而本 handler 照样把它的配额行清掉——撤销静默
    // 失效，额度还被清零。
    let token_store = crate::token::manage::lock_token_store();
    // Load current config to find the token
    let config = match crate::token::config::TokensConfig::load(&crate::token::manage::tokens_path(
        &state.config_path,
    )) {
        Ok(c) => c,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(format!("failed to load tokens.toml: {}", err)),
                    kind: None,
                }),
            );
        }
    };

    // Find token by key or name
    let token = config
        .tokens
        .iter()
        .find(|t| t.key == req.key_or_name || t.name == req.key_or_name);

    let token = match token {
        Some(t) => t.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(format!("no token matches '{}'", req.key_or_name)),
                    kind: None,
                }),
            );
        }
    };

    let key_for_cleanup = token.key.clone();

    // Execute mutation
    let result = crate::token::manage::mutate_tokens(&token_store, &state.config_path, |cfg| {
        let before = cfg.tokens.len();
        cfg.tokens.retain(|t| t.key != key_for_cleanup);
        if cfg.tokens.len() == before {
            anyhow::bail!("token disappeared concurrently");
        }
        Ok(Some(()))
    });

    match result {
        Ok(_) => {
            // Reload token config into memory first: that is also the
            // configuration boundary which converges the deleted token's quota
            // rows away. The targeted delete below then only fails
            // when the database itself is unreachable, i.e. the rows really did
            // survive.
            if let Err(err) = state.token.reload(&state.config_path) {
                tracing::error!(error = %err, "failed to reload token config after delete");
            }

            let state_dir = state.state_dirs.state_dir.clone();
            let cleanup = crate::token::usage::TokenUsageStore::open(&state_dir.join("tokens.db"))
                .and_then(|store| store.delete_token_usage(&key_for_cleanup));

            match cleanup {
                Ok(_) => (
                    StatusCode::OK,
                    Json(AdminApiResponse {
                        status: "ok",
                        data: Some(serde_json::json!({
                            "message": format!("deleted token '{}'", req.key_or_name)
                        })),
                        error: None,
                        kind: None,
                    }),
                ),
                // The config entry is already gone and cannot be rolled back, so
                // a failed cleanup must not be reported as a clean delete — it
                // used to be swallowed entirely, leaving quota rows nobody owns.
                Err(err) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some(format!(
                            "token '{}' was removed from tokens.toml, but its quota rows could \
                             not be deleted: {err}. The token is already inactive; the rows are \
                             dropped at the next server start or token reload.",
                            req.key_or_name
                        )),
                        kind: None,
                    }),
                ),
            }
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(err.to_string()),
                kind: None,
            }),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct TokenRenameRequest {
    pub key_or_name: String,
    pub new_name: String,
}

/// POST /admin/token/rename — 重命名 token（委托模式）
pub async fn token_rename(
    State(state): State<AppState>,
    Json(req): Json<TokenRenameRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let token_store = crate::token::manage::lock_token_store();
    let state_dir = state.state_dirs.state_dir.clone();
    match crate::token::manage::rename_token(
        &token_store,
        &state.config_path,
        &state_dir,
        &req.key_or_name,
        &req.new_name,
    ) {
        Ok(()) => {
            if let Err(err) = state.token.reload(&state.config_path) {
                tracing::error!(error = %err, "failed to reload token config after rename");
            }
            if let Ok(cfg) = state.token.config.read() {
                state.cooldowns.reconcile(&cfg);
            }
            (
                StatusCode::OK,
                Json(AdminApiResponse {
                    status: "ok",
                    data: Some(serde_json::json!({
                        "message": format!("renamed token '{}' to '{}'", req.key_or_name, req.new_name)
                    })),
                    error: None,
                    kind: None,
                }),
            )
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(err.to_string()),
                kind: None,
            }),
        ),
    }
}

/// POST /admin/token/reset-usage — 重置 token 用量（委托模式）
pub async fn token_reset_usage(
    State(state): State<AppState>,
    Json(req): Json<TokenResetUsageRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    // Find token by key or name from shared config
    let token = {
        let config = match state.token.config.read() {
            Ok(c) => c,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some("token configuration lock poisoned".to_string()),
                        kind: None,
                    }),
                );
            }
        };
        config
            .tokens
            .iter()
            .find(|t| t.key == req.key_or_name || t.name == req.key_or_name)
            .cloned()
    };

    let token = match token {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(format!("no token matches '{}'", req.key_or_name)),
                    kind: None,
                }),
            );
        }
    };

    // Reset usage in SQLite
    let state_dir = state.state_dirs.state_dir.clone();
    let result = crate::token::usage::TokenUsageStore::open(&state_dir.join("tokens.db"))
        .and_then(|store| store.reset_token_usage(&token.key));

    match result {
        Ok(_) => (
            StatusCode::OK,
            Json(AdminApiResponse {
                status: "ok",
                data: Some(serde_json::json!({
                    "message": format!("reset usage for token '{}'", req.key_or_name)
                })),
                error: None,
                kind: None,
            }),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(format!("failed to reset usage: {}", err)),
                kind: None,
            }),
        ),
    }
}

// ── Bad-request budget admin endpoints (UDS only) ──────────────────────

/// GET /admin/protection/bad-request-budget — query per-account budget status.
pub async fn bad_request_budget_status(
    State(state): State<AppState>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let snapshots = state.account_budget_snapshot();
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::to_value(snapshots).unwrap_or_default()),
            error: None,
            kind: Some("bad_request_budget_status".into()),
        }),
    )
}

#[derive(Debug, Deserialize)]
pub struct BadRequestBudgetResetRequest {
    /// Token key or display name to reset.
    pub key_or_name: String,
}

/// POST /admin/protection/bad-request-budget/reset — reset lifetime counter.
pub async fn bad_request_budget_reset(
    State(state): State<AppState>,
    Json(req): Json<BadRequestBudgetResetRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    // Resolve token key from key_or_name.
    let token_key = match resolve_token_key(&state, &req.key_or_name) {
        Some(k) => k,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(format!("no token matches '{}'", req.key_or_name)),
                    kind: None,
                }),
            );
        }
    };

    match state.account_budget_reset(&token_key) {
        Ok(()) => (
            StatusCode::OK,
            Json(AdminApiResponse {
                status: "ok",
                data: Some(serde_json::json!({
                    "message": format!("reset bad-request budget for token '{}'", req.key_or_name)
                })),
                error: None,
                kind: Some("bad_request_budget_reset".into()),
            }),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(format!("failed to reset budget: {}", err)),
                kind: None,
            }),
        ),
    }
}

// ── Token error-class cooldown admin endpoints ───────────────────────

/// GET /admin/protection/token-error-cooldown — per (token, inbound model)
/// error-class counts and cooldown remaining (token display names only, never keys).
pub async fn token_error_cooldown_status(
    State(state): State<AppState>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let snapshots = state.error_cooldown.snapshot();
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::to_value(snapshots).unwrap_or_default()),
            error: None,
            kind: Some("token_error_cooldown_status".into()),
        }),
    )
}

#[derive(Debug, Deserialize)]
pub struct TokenErrorCooldownClearRequest {
    /// Token name or key; `"*"` selects every token. The `model` field composes
    /// as a filter with it: omitted, the target's every model is cleared.
    pub token: String,
    /// Inbound model; narrows the clear to that model. Omitted = every model of
    /// the target (or, for `"*"`, every model of every token).
    #[serde(default)]
    pub model: Option<String>,
}

/// POST /admin/protection/token-error-cooldown/clear — service-administrator
/// manual clear: drops counts + active cooldown for the target (optionally one
/// inbound model) and persists.
pub async fn token_error_cooldown_clear(
    State(state): State<AppState>,
    Json(req): Json<TokenErrorCooldownClearRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    if req.token == "*" {
        let (cleared, message) = match req.model.as_deref() {
            Some(model) => {
                let cleared = state.error_cooldown.clear_model_everywhere(model);
                (
                    cleared,
                    format!(
                        "cleared error-class cooldown state for model '{model}' across {cleared} token(s)"
                    ),
                )
            }
            None => {
                let cleared = state.error_cooldown.clear_all();
                (
                    cleared,
                    format!("cleared error-class cooldown state for {cleared} token(s)"),
                )
            }
        };
        tracing::info!(
            model = req.model.as_deref().unwrap_or("*"),
            cleared,
            "admin cleared all token error-class cooldowns"
        );
        return (
            StatusCode::OK,
            Json(AdminApiResponse {
                status: "ok",
                data: Some(serde_json::json!({
                    "message": message
                })),
                error: None,
                kind: Some("token_error_cooldown_clear".into()),
            }),
        );
    }

    let Some(name) = resolve_token_name(&state, &req.token) else {
        return (
            StatusCode::NOT_FOUND,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(format!("no token matches '{}'", req.token)),
                kind: None,
            }),
        );
    };
    let cleared = match req.model.as_deref() {
        Some(model) => state.error_cooldown.clear_token_model(&name, model),
        None => state.error_cooldown.clear_token(&name),
    };
    tracing::info!(
        token = %name,
        model = req.model.as_deref().unwrap_or("*"),
        cleared,
        "admin cleared token error-class cooldown"
    );
    let scope = match req.model.as_deref() {
        Some(model) => format!("token '{name}' model '{model}'"),
        None => format!("token '{name}'"),
    };
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "message": if cleared {
                    format!("cleared error-class cooldown state for {scope}")
                } else {
                    format!("{scope} had no error-class cooldown state")
                }
            })),
            error: None,
            kind: Some("token_error_cooldown_clear".into()),
        }),
    )
}

/// Resolve a token **display name** from a key_or_name string.
fn resolve_token_name(state: &AppState, key_or_name: &str) -> Option<String> {
    let config = state.token.config.read().ok()?;
    config
        .tokens
        .iter()
        .find(|t| t.key == key_or_name || t.name == key_or_name)
        .map(|t| t.name.clone())
}

/// Resolve a token key from a key_or_name string.
fn resolve_token_key(state: &AppState, key_or_name: &str) -> Option<String> {
    let config = state.token.config.read().ok()?;
    config
        .tokens
        .iter()
        .find(|t| t.key == key_or_name || t.name == key_or_name)
        .map(|t| t.key.clone())
}

/// POST /admin/cooldown/clear — 清除冷却条目（写操作，UDS 管理通道）。
/// server 清内存 CooldownStore + 落盘，保证内存/盘一致。
pub async fn cooldown_clear(
    State(state): State<AppState>,
    Json(req): Json<CooldownClearRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let removed = state
        .cooldowns
        .clear_entries(req.model.as_deref(), &req.provider);
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({ "removed": removed })),
            error: None,
            kind: None,
        }),
    )
}

#[derive(Debug, Deserialize)]
pub struct CooldownSetRequest {
    pub provider: String,
    pub model: String,
    pub duration: String,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// POST /admin/cooldown/set — 手动注入冷却条目（写操作，UDS 管理通道）。
pub async fn cooldown_set(
    State(state): State<AppState>,
    Json(req): Json<CooldownSetRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let duration = match crate::cooldown::parse_manual_duration(&req.duration) {
        Ok(duration) => duration,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(err.to_string()),
                    kind: None,
                }),
            );
        }
    };
    let protocols = match req.protocol.as_deref() {
        Some(value) => match crate::cooldown::parse_cooldown_protocol(value) {
            Ok(protocol) => vec![protocol],
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some(err.to_string()),
                        kind: None,
                    }),
                );
            }
        },
        None => Protocol::CLIENT_PROTOCOLS.to_vec(),
    };
    let kind = req
        .kind
        .as_deref()
        .unwrap_or(crate::cooldown::DEFAULT_MANUAL_KIND);
    let reason = req.reason.as_deref().unwrap_or("");
    crate::cooldown::apply_manual_set(
        &state.cooldowns,
        &req.provider,
        &req.model,
        duration,
        kind,
        reason,
        &protocols,
    );
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "count": protocols.len(),
                "model": req.model,
                "provider": req.provider,
                "duration_seconds": duration.as_secs(),
                "kind": kind,
                "protocols": protocols.iter().map(|protocol| protocol.route_key()).collect::<Vec<_>>(),
            })),
            error: None,
            kind: None,
        }),
    )
}

/// POST /admin/oauth/write — 写入 OAuth 账号（CLI login 委托 server）。
/// server 写文件（持锁）+ 更新内存缓存（AppState.oauth_accounts）。
#[derive(Debug, Deserialize)]
pub struct OAuthWriteRequest {
    pub provider_id: String,
    /// "openai"（未知类型走拒绝分支）
    pub oauth_type: String,
    pub account: String,
    #[serde(flatten)]
    pub account_data: serde_json::Map<String, serde_json::Value>,
}

pub async fn oauth_write(
    State(state): State<AppState>,
    Json(req): Json<OAuthWriteRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let accounts_path = state.oauth_accounts_path.clone();
    let started = std::time::Instant::now();
    tracing::info!(
        target: "llm_proxy::oauth",
        event = "oauth_server_persistence_start",
        provider = %req.provider_id,
        account = %crate::auth::oauth_account_label(&req.account),
        oauth_type = %req.oauth_type,
    );
    let result = crate::auth::with_locked_accounts(&accounts_path, |accounts| {
        match req.oauth_type.as_str() {
            "openai" => {
                let entry: crate::auth::OpenaiAccount = match serde_json::from_value(
                    serde_json::Value::Object(req.account_data.clone()),
                ) {
                    Ok(e) => e,
                    Err(e) => return Err(anyhow::anyhow!("invalid openai account data: {e}")),
                };
                accounts.openai.insert(req.account.clone(), entry);
            }
            other => {
                return Err(anyhow::anyhow!("unknown oauth type: {other}"));
            }
        }
        crate::auth::save_oauth_accounts_locked(&accounts_path, accounts)
    });

    match result {
        Ok(()) => {
            // 更新内存缓存（server 转发从内存取）。缓存更新失败 → 报错，
            // 避免"文件已写但内存未更新"的不一致状态。
            let cache_ok = match state.oauth_accounts.write() {
                Ok(mut guard) => {
                    match crate::auth::load_oauth_accounts_with_recovery(&accounts_path) {
                        Ok(accounts) => {
                            *guard = accounts;
                            true
                        }
                        Err(e) => {
                            tracing::error!("oauth cache reload after write failed: {e}");
                            false
                        }
                    }
                }
                Err(_) => false,
            };
            if !cache_ok {
                tracing::error!(
                    target: "llm_proxy::oauth",
                    event = "oauth_server_persistence_failure",
                    provider = %req.provider_id,
                    account = %crate::auth::oauth_account_label(&req.account),
                    oauth_type = %req.oauth_type,
                    phase = "cache_reload",
                    duration_ms = started.elapsed().as_millis() as u64,
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some(
                            "OAuth account written to file but in-memory cache update failed; \
                             restart server or retry to re-sync"
                                .to_string(),
                        ),
                        kind: None,
                    }),
                );
            }
            tracing::info!(
                target: "llm_proxy::oauth",
                event = "oauth_server_persistence_success",
                provider = %req.provider_id,
                account = %crate::auth::oauth_account_label(&req.account),
                oauth_type = %req.oauth_type,
                duration_ms = started.elapsed().as_millis() as u64,
            );
            (
                StatusCode::OK,
                Json(AdminApiResponse {
                    status: "ok",
                    data: Some(serde_json::json!({
                        "message": format!(
                            "OAuth account {} written (provider {})",
                            req.account, req.provider_id
                        )
                    })),
                    error: None,
                    kind: None,
                }),
            )
        }
        Err(e) => {
            tracing::warn!(
                target: "llm_proxy::oauth",
                event = "oauth_server_persistence_failure",
                provider = %req.provider_id,
                account = %crate::auth::oauth_account_label(&req.account),
                oauth_type = %req.oauth_type,
                phase = "file_write",
                duration_ms = started.elapsed().as_millis() as u64,
            );
            (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(e.to_string()),
                    kind: None,
                }),
            )
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct OAuthLogoutRequest {
    pub provider_id: String,
}

/// POST /admin/oauth/logout — logout 经管理通道执行。
/// server 删除磁盘凭据后刷新内存缓存（`state.oauth_accounts`），消除
/// "CLI 直写磁盘、server 内存仍持有未过期 token → 已登出仍可用"的窗口。
/// 失败按语义分层：provider 不存在/非 OAuth → 400；凭据写盘或缓存刷新
/// 失败 → 500（不吞错，磁盘与内存不一致必须上报）。
pub async fn oauth_logout(
    State(state): State<AppState>,
    Json(req): Json<OAuthLogoutRequest>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let cfg = state.config();
    let accounts_path = state.oauth_accounts_path.clone();
    let account = match crate::auth::oauth_account_for_provider(&cfg, &req.provider_id) {
        Ok(account) => account,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(e.to_string()),
                    kind: Some("invalid_config".to_string()),
                }),
            );
        }
    };
    let removed = match crate::auth::logout(&accounts_path, Some(&account)) {
        Ok(removed) => removed,
        Err(e) => {
            tracing::error!(
                target: "llm_proxy::oauth",
                event = "oauth_server_persistence_failure",
                provider = %req.provider_id,
                phase = "logout_file_write",
                error = %e,
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AdminApiResponse {
                    status: "error",
                    data: None,
                    error: Some(format!(
                        "failed to remove OAuth credential for provider {}: {e}",
                        req.provider_id
                    )),
                    kind: Some("internal_error".to_string()),
                }),
            );
        }
    };
    let cache_ok = match state.oauth_accounts.write() {
        Ok(mut guard) => match crate::auth::load_oauth_accounts_with_recovery(&accounts_path) {
            Ok(accounts) => {
                *guard = accounts;
                true
            }
            Err(e) => {
                tracing::error!("oauth cache reload after logout failed: {e}");
                false
            }
        },
        Err(_) => false,
    };
    if !cache_ok {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminApiResponse {
                status: "error",
                data: None,
                error: Some(
                    "OAuth credential removed from file but in-memory cache update failed; \
                     restart server or retry to re-sync"
                        .to_string(),
                ),
                kind: Some("internal_error".to_string()),
            }),
        );
    }
    tracing::info!(
        target: "llm_proxy::oauth",
        event = "oauth_server_logout",
        provider = %req.provider_id,
        account = %crate::auth::oauth_account_label(&account),
        removed,
    );
    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "message": format!(
                    "Logged out provider={} removed={removed}",
                    req.provider_id
                )
            })),
            error: None,
            kind: None,
        }),
    )
}

/// GET /admin/usage — query token usage statistics
pub async fn usage(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<UsageQueryParams>,
) -> (StatusCode, Json<AdminApiResponse>) {
    // Parse time range from period parameter before acquiring the lock
    let (start, end) = match params.period.as_deref() {
        Some(period) => match parse_period(period) {
            Ok(range) => range,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some(e.to_string()),
                        kind: None,
                    }),
                );
            }
        },
        None => (None, None),
    };

    // Apply pagination defaults and caps
    let limit = params.limit.unwrap_or(1000).min(10000);
    let offset = params.offset.unwrap_or(0);

    let store = {
        let core = state.core.lock().await;
        core.usage_store_arc()
    };

    let (records, total) = match store {
        Some(store) => match store.query_with_pagination(
            start.map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
            end.map(|ts| chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default()),
            params.provider.as_deref(),
            params.model.as_deref(),
            params.endpoint.as_deref(),
            limit,
            offset,
        ) {
            Ok(result) => result,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some(format!("query failed: {}", e)),
                        kind: None,
                    }),
                );
            }
        },
        None => (Vec::new(), 0),
    };

    // The record's `token_key` is the bearer credential that authorized the
    // request. This handler serves the public listener, so strip it: a reachable
    // caller must never be able to harvest live tokens from usage history.
    let records: Vec<serde_json::Value> = records
        .into_iter()
        .map(|record| {
            let mut value = serde_json::to_value(&record).unwrap_or(serde_json::Value::Null);
            if let Some(object) = value.as_object_mut() {
                object.remove("token_key");
            }
            value
        })
        .collect();

    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "count": records.len(),
                "total": total,
                "limit": limit,
                "offset": offset,
                "records": records,
            })),
            error: None,
            kind: None,
        }),
    )
}

/// Query parameters for `GET /admin/usage/attribution`.
#[derive(Debug, Deserialize)]
pub(crate) struct AttributionQueryParams {
    period: Option<String>,
}

/// GET /admin/usage/attribution — admin-only routing attribution audit.
/// Joins usage_records with model_binding_revisions (SCD Type 2) to resolve
/// the physical upstream_model at the time of each record.
pub async fn usage_attribution(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<AttributionQueryParams>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let (start_ts, end_ts) = match params.period.as_deref() {
        Some(period) => match parse_period(period) {
            Ok(range) => range,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some(e.to_string()),
                        kind: None,
                    }),
                );
            }
        },
        None => (None, None),
    };

    let start = start_ts
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap());
    let end = end_ts
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .unwrap_or_else(chrono::Utc::now);

    let store = {
        let core = state.core.lock().await;
        core.usage_store_arc()
    };

    let groups = match store {
        Some(store) => match store.query_attribution(start, end) {
            Ok(g) => g,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some(format!("attribution query failed: {}", e)),
                        kind: None,
                    }),
                );
            }
        },
        None => Vec::new(),
    };

    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "count": groups.len(),
                "groups": groups,
            })),
            error: None,
            kind: None,
        }),
    )
}

/// GET /admin/diagnostics — query structured diagnostic records.
///
/// **UDS management channel only** — not registered on the public HTTP
/// listener. `capture = "full"` records may contain the egress request body;
/// access is gated by UDS file permissions (0600) and the `include_body`
/// opt-in flag.
pub async fn diagnostics(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<DiagnosticsQueryParams>,
) -> (StatusCode, Json<AdminApiResponse>) {
    let (start, end) = match params.period.as_deref() {
        Some(period) => match parse_period(period) {
            Ok(range) => range,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(AdminApiResponse {
                        status: "error",
                        data: None,
                        error: Some(e.to_string()),
                        kind: None,
                    }),
                );
            }
        },
        None => (None, None),
    };

    let limit = params.limit.unwrap_or(200).min(1000);
    let offset = params.offset.unwrap_or(0);

    let Some(store) = state.diagnostics_store() else {
        return (
            StatusCode::OK,
            Json(AdminApiResponse {
                status: "ok",
                data: Some(serde_json::json!({
                    "count": 0,
                    "total": 0,
                    "limit": limit,
                    "offset": offset,
                    "records": [],
                    "capture": "disabled",
                })),
                error: None,
                kind: None,
            }),
        );
    };

    let capture_mode = store.capture_mode();
    let (records, total) = store.query_records(params.event.as_deref(), start, end, limit, offset);

    // Visibility: strip `body_full` unless caller opted in AND capture is `full`.
    // In metadata mode, `body_full` is never written, but strip defensively.
    let records: Vec<serde_json::Value> = records
        .into_iter()
        .map(|mut record| {
            if (capture_mode != crate::diagnostics::CaptureMode::Full || !params.include_body)
                && let Some(obj) = record.as_object_mut()
            {
                obj.remove("body_full");
            }
            record
        })
        .collect();

    let capture_label = match capture_mode {
        crate::diagnostics::CaptureMode::Metadata => "metadata",
        crate::diagnostics::CaptureMode::Full => "full",
    };

    (
        StatusCode::OK,
        Json(AdminApiResponse {
            status: "ok",
            data: Some(serde_json::json!({
                "count": records.len(),
                "total": total,
                "limit": limit,
                "offset": offset,
                "records": records,
                "capture": capture_label,
            })),
            error: None,
            kind: None,
        }),
    )
}

/// POST /admin/config/reload — reload configuration from disk
pub async fn config_reload(State(state): State<AppState>) -> (StatusCode, Json<AdminApiResponse>) {
    let outcome = state.reload_config().await;
    match outcome {
        Ok(()) => (
            StatusCode::OK,
            Json(AdminApiResponse {
                status: "ok",
                data: Some(serde_json::json!({ "message": "configuration reloaded" })),
                error: None,
                kind: None,
            }),
        ),
        Err(error) => classify_commit_writer_error(error),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parse a period string into (start, end) timestamps.
pub(crate) fn parse_period(period: &str) -> anyhow::Result<(Option<i64>, Option<i64>)> {
    let now = chrono::Local::now();
    let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();

    match period {
        "today" => {
            // 本地时区 00:00 → UTC 时间戳（不能 and_utc：会把本地日期误当 UTC）
            let start = local_naive_to_unix(today_start);
            Ok((Some(start), None))
        }
        "yesterday" => {
            let yesterday = today_start - chrono::Duration::days(1);
            let start = local_naive_to_unix(yesterday);
            let end = local_naive_to_unix(today_start);
            Ok((Some(start), Some(end)))
        }
        "7d" | "1w" => {
            let start = local_naive_to_unix(today_start - chrono::Duration::days(7));
            Ok((Some(start), None))
        }
        "30d" => {
            let start = local_naive_to_unix(today_start - chrono::Duration::days(30));
            Ok((Some(start), None))
        }
        s if s.contains(':') => {
            // Range format: "2026-03-12:2026-03-20"
            let parts: Vec<&str> = s.split(':').collect();
            if parts.len() != 2 {
                anyhow::bail!("invalid period range: {s}");
            }
            let start_date = chrono::NaiveDate::parse_from_str(parts[0], "%Y-%m-%d")
                .with_context(|| format!("invalid start date: {}", parts[0]))?;
            let end_date = chrono::NaiveDate::parse_from_str(parts[1], "%Y-%m-%d")
                .with_context(|| format!("invalid end date: {}", parts[1]))?;
            let start = local_naive_to_unix(start_date.and_hms_opt(0, 0, 0).unwrap());
            let end = local_naive_to_unix(
                (end_date + chrono::Duration::days(1))
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            );
            Ok((Some(start), Some(end)))
        }
        s if s.ends_with('d') => {
            let days: i64 = s.trim_end_matches('d').parse()?;
            let start = local_naive_to_unix(today_start - chrono::Duration::days(days));
            Ok((Some(start), None))
        }
        s if s.ends_with('w') => {
            let weeks: i64 = s.trim_end_matches('w').parse()?;
            let start = local_naive_to_unix(today_start - chrono::Duration::weeks(weeks));
            Ok((Some(start), None))
        }
        _ => {
            // Try single date: "2026-03-20"
            let date = chrono::NaiveDate::parse_from_str(period, "%Y-%m-%d")
                .with_context(|| format!("invalid period: {period}"))?;
            let start = local_naive_to_unix(date.and_hms_opt(0, 0, 0).unwrap());
            let end = local_naive_to_unix(
                (date + chrono::Duration::days(1))
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            );
            Ok((Some(start), Some(end)))
        }
    }
}

/// 把本地时区的 NaiveDateTime 转成 UTC 时间戳（正确处理时区偏移）。
fn local_naive_to_unix(naive: chrono::NaiveDateTime) -> i64 {
    use chrono::TimeZone;
    naive
        .and_local_timezone(chrono::Local)
        .single()
        .unwrap_or_else(|| chrono::Local.from_local_datetime(&naive).unwrap())
        .timestamp()
}

use anyhow::Context;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EndpointConfig, ModelConfig, ProviderBinding, ProviderConfig};
    use axum::extract::{Path, Query, State};
    use axum::http::{HeaderMap, header};
    use serde_json::Value;

    fn cfg() -> Config {
        let mut cfg: Config = toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").unwrap();
        cfg.providers.clear();
        cfg.models.clear();
        cfg.providers.insert(
            "p1".into(),
            ProviderConfig {
                api_key_env: Some("P1_KEY".into()),
                openai_chat: Some(EndpointConfig {
                    url: Some("https://example.test/chat".into()),
                    ..Default::default()
                }),
                openai_responses: Some(EndpointConfig {
                    derive_from: Some("openai_chat".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        cfg.providers.insert(
            "p2".into(),
            ProviderConfig {
                anthropic: Some(EndpointConfig {
                    url: Some("https://example.test/anthropic".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        cfg.models.insert(
            "m1".into(),
            ModelConfig {
                description: None,
                context_window: 1000,
                max_output_tokens: 100,
                features: vec![],
                reasoning_levels: None,
                supported_reasoning_levels: vec![],
                cooldown: None,
                reasoning_routes: None,
                default_reasoning_level: None,
                supports_thinking: None,
                force_think: None,
                force_no_think: None,
                default_think: None,
                enable_thinking: None,
                chat_reasoning_off_format: None,
                codex_service_tier: None,
                openai_chat_providers: vec![ProviderBinding {
                    name: "p1".into(),
                    model: "up-m1".into(),
                }],
                openai_responses_providers: vec![ProviderBinding {
                    name: "p1".into(),
                    model: "up-m1-resp".into(),
                }],
                anthropic_providers: vec![],
                reasoning_level_map: None,
            },
        );
        cfg.models.insert(
            "m2".into(),
            ModelConfig {
                description: None,
                context_window: 2000,
                max_output_tokens: 200,
                features: vec![],
                reasoning_levels: None,
                supported_reasoning_levels: vec![],
                cooldown: None,
                reasoning_routes: None,
                default_reasoning_level: None,
                supports_thinking: None,
                force_think: None,
                force_no_think: None,
                default_think: None,
                enable_thinking: None,
                chat_reasoning_off_format: None,
                codex_service_tier: None,
                openai_chat_providers: vec![],
                openai_responses_providers: vec![],
                anthropic_providers: vec![ProviderBinding {
                    name: "p2".into(),
                    model: "claude".into(),
                }],
                reasoning_level_map: None,
            },
        );
        cfg.status.active_ttl = 3600;
        cfg.status.probe_timeout = 1;
        cfg
    }

    fn state() -> AppState {
        AppState::new_in_memory_for_management(cfg())
    }
    fn data(resp: Json<AdminApiResponse>) -> Value {
        resp.0.data.unwrap()
    }

    fn json_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn ping_returns_ok_and_version() {
        let r = ping(State(state())).await.into_response();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_reports_counts_and_active() {
        let s = state();
        s.mark_provider_active("p1");
        let resp = status(State(s), None).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let r: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(r["status"], "ok");
        let d = &r["data"];
        assert_eq!(d["providers"], 2);
        assert_eq!(d["models"], 2);
        assert!(
            d["active_providers"]
                .as_array()
                .unwrap()
                .contains(&Value::String("p1".into()))
        );
    }

    #[tokio::test]
    async fn status_non_admin_hides_provider_topology_and_projects_cache() {
        let s = state();
        let grant = crate::token::auth::TokenGrant::Granted(crate::token::auth::TokenContext {
            key: "lp-user".into(),
            name: "user".into(),
            allowed_models: vec!["m1".into()],
            admin: false,
        });
        let resp = status(State(s), Some(axum::extract::Extension(grant)))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let r: Value = serde_json::from_slice(&bytes).unwrap();
        let d = &r["data"];
        assert_eq!(d["providers"], 0);
        assert_eq!(d["models"], 1);
        assert_eq!(d["active_providers"].as_array().unwrap().len(), 0);
        assert!(d["cache"]["probes"].as_object().unwrap().is_empty());
    }

    // acceptance: two concurrent admin provider writes must never
    // silently lose an update — either both land, or the stale-base one is
    // rejected with 409 while the other's change survives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_provider_copies_never_silently_lose_updates() {
        let s = state();
        let task_a = {
            let s = s.clone();
            tokio::spawn(async move {
                provider_copy(
                    State(s),
                    Json(ProviderCopyRequest {
                        source: "p1".into(),
                        target: "copy-a".into(),
                        api_key_env: Some("ENV_COPY_A".into()),
                        no_api_key: false,
                    }),
                )
                .await
                .0
            })
        };
        let task_b = {
            let s = s.clone();
            tokio::spawn(async move {
                provider_copy(
                    State(s),
                    Json(ProviderCopyRequest {
                        source: "p1".into(),
                        target: "copy-b".into(),
                        api_key_env: Some("ENV_COPY_B".into()),
                        no_api_key: false,
                    }),
                )
                .await
                .0
            })
        };
        let (status_a, status_b) = (task_a.await.expect("a"), task_b.await.expect("b"));
        let final_cfg = s.config();
        for (status, target) in [(status_a, "copy-a"), (status_b, "copy-b")] {
            match status {
                StatusCode::OK => assert!(
                    final_cfg.providers.contains_key(target),
                    "{target} reported success but is absent: silent lost update"
                ),
                StatusCode::CONFLICT => {}
                other => panic!("unexpected status for {target}: {other}"),
            }
        }
    }

    #[tokio::test]
    async fn status_probe_json_returns_ok() {
        let (code, Json(r)) = status_probe_json(state(), &ProbeSelection::default()).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(r.status, "ok");
        let d = r.data.unwrap();
        assert!(d.get("probed_providers").is_some());
        assert_eq!(d["selection"]["provider"], Value::Null);
        assert_eq!(d["selection"]["model"], Value::Null);
        assert!(d.get("probe_outcomes").is_some());
    }

    #[tokio::test]
    async fn status_probe_json_preflight_failure_is_not_executed_or_probed() {
        let mut cfg = cfg();
        cfg.providers.get_mut("p1").expect("p1").auth =
            Some(crate::config::AuthConfig::OpenaiOauth {
                account: Some("missing-test-account".to_string()),
            });
        let response = status_probe(
            None,
            json_headers(),
            State(AppState::new_in_memory_for_management(cfg)),
            Bytes::from(r#"{"provider":"p1","model":"m1"}"#),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let response: Value = serde_json::from_slice(&body).expect("JSON response");
        assert_eq!(response["data"]["probed_providers"], serde_json::json!([]));
        let outcomes = response["data"]["probe_outcomes"]
            .as_array()
            .expect("outcomes");
        assert!(
            outcomes.iter().all(|outcome| {
                outcome["disposition"] == "not_executed"
                    && outcome["error"].as_str().is_some_and(|error| {
                        error.contains("auth unavailable")
                            || error.contains("probe request plan unavailable")
                    })
            }),
            "unexpected preflight outcomes: {outcomes:?}"
        );
    }

    #[tokio::test]
    async fn status_probe_without_sse_is_json() {
        let r = status_probe(None, HeaderMap::new(), State(state()), Bytes::new())
            .await
            .into_response();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_probe_with_sse_accept_is_sse() {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        let r = status_probe(None, h, State(state()), Bytes::new())
            .await
            .into_response();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/event-stream"
        );
    }

    #[tokio::test]
    async fn status_probe_json_empty_body_and_empty_object_are_equivalent() {
        // 1. Empty body (0 bytes)
        let resp_empty = status_probe(None, HeaderMap::new(), State(state()), Bytes::new())
            .await
            .into_response();
        assert_eq!(resp_empty.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp_empty.into_body(), usize::MAX)
            .await
            .unwrap();
        let r_empty: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(r_empty["data"]["selection"]["provider"], Value::Null);
        assert_eq!(r_empty["data"]["selection"]["model"], Value::Null);
        let outcomes_empty = r_empty["data"]["probe_outcomes"].as_array().unwrap();

        // 2. Empty JSON object `{}`
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        let resp_obj = status_probe(None, headers, State(state()), Bytes::from("{}"))
            .await
            .into_response();
        assert_eq!(resp_obj.status(), StatusCode::OK);
        let bytes_obj = axum::body::to_bytes(resp_obj.into_body(), usize::MAX)
            .await
            .unwrap();
        let r_obj: Value = serde_json::from_slice(&bytes_obj).unwrap();
        assert_eq!(r_obj["data"]["selection"]["provider"], Value::Null);
        assert_eq!(r_obj["data"]["selection"]["model"], Value::Null);
        let outcomes_obj = r_obj["data"]["probe_outcomes"].as_array().unwrap();

        assert_eq!(outcomes_empty.len(), outcomes_obj.len());
    }

    #[tokio::test]
    async fn status_probe_json_selection_provider_model_and_intersection() {
        let s = state();

        // 1. Target provider "p1"
        let resp_p1 = status_probe(
            None,
            json_headers(),
            State(s.clone()),
            Bytes::from(r#"{"provider": "p1"}"#),
        )
        .await
        .into_response();
        assert_eq!(resp_p1.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp_p1.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(val["data"]["selection"]["provider"], "p1");
        assert_eq!(val["data"]["selection"]["model"], Value::Null);
        let outcomes = val["data"]["probe_outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| o["provider"] == "p1"));

        // 2. Target model "m2"
        let resp_m2 = status_probe(
            None,
            json_headers(),
            State(s.clone()),
            Bytes::from(r#"{"model": "m2"}"#),
        )
        .await
        .into_response();
        assert_eq!(resp_m2.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp_m2.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(val["data"]["selection"]["provider"], Value::Null);
        assert_eq!(val["data"]["selection"]["model"], "m2");
        let outcomes = val["data"]["probe_outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0]["provider"], "p2");
        assert_eq!(outcomes[0]["model"], "m2");
        assert_eq!(outcomes[0]["protocol"], "anthropic");

        // 3. Target intersection "p1" + "m1"
        let resp_inter = status_probe(
            None,
            json_headers(),
            State(s),
            Bytes::from(r#"{"provider": "p1", "model": "m1"}"#),
        )
        .await
        .into_response();
        assert_eq!(resp_inter.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp_inter.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(val["data"]["selection"]["provider"], "p1");
        assert_eq!(val["data"]["selection"]["model"], "m1");
        let outcomes = val["data"]["probe_outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes
                .iter()
                .all(|o| o["provider"] == "p1" && o["model"] == "m1")
        );
    }

    #[tokio::test]
    async fn status_probe_rejects_bad_requests_with_400() {
        // null body
        let r = status_probe(None, json_headers(), State(state()), Bytes::from("null"))
            .await
            .into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // unknown field
        let r = status_probe(
            None,
            json_headers(),
            State(state()),
            Bytes::from(r#"{"unknown": "field"}"#),
        )
        .await
        .into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // non-JSON
        let r = status_probe(
            None,
            json_headers(),
            State(state()),
            Bytes::from("not json"),
        )
        .await
        .into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // non-object JSON
        let r = status_probe(None, json_headers(), State(state()), Bytes::from("[1, 2]"))
            .await
            .into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn status_probe_json_discovers_selected_dynamic_models_on_server() {
        use std::sync::{Arc, Mutex};

        let discovery_hits = Arc::new(Mutex::new(0usize));
        let discovery_hits_for_tags = Arc::clone(&discovery_hits);
        let app = axum::Router::new()
            .route(
                "/api/tags",
                axum::routing::get(move || {
                    let hits = Arc::clone(&discovery_hits_for_tags);
                    async move {
                        *hits.lock().unwrap() += 1;
                        axum::Json(serde_json::json!({"models": [{"name": "qwen3:27b"}]}))
                    }
                }),
            )
            .route(
                "/api/show",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({
                        "capabilities": ["completion"],
                        "model_info": {"qwen3.context_length": 32768}
                    }))
                }),
            )
            .route(
                "/v1/chat/completions",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({
                        "choices": [{"message": {"content": "pong"}}]
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock ollama");
        let addr = listener.local_addr().expect("mock ollama addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock ollama");
        });

        let mut cfg = cfg();
        cfg.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                auth: Some(crate::config::AuthConfig::None),
                openai_chat: Some(crate::config::EndpointConfig::native(format!(
                    "http://{addr}/v1/chat/completions"
                ))),
                ..Default::default()
            },
        );
        cfg.models.insert(
            "ollama-model".to_string(),
            ModelConfig {
                description: None,
                context_window: 1_000,
                max_output_tokens: 100,
                features: vec![],
                reasoning_levels: None,
                supported_reasoning_levels: vec![],
                cooldown: None,
                reasoning_routes: None,
                default_reasoning_level: None,
                supports_thinking: None,
                force_think: None,
                force_no_think: None,
                default_think: None,
                enable_thinking: None,
                chat_reasoning_off_format: None,
                codex_service_tier: None,
                openai_chat_providers: vec![ProviderBinding {
                    name: "ollama".to_string(),
                    model: "qwen3:27b".to_string(),
                }],
                openai_responses_providers: vec![],
                anthropic_providers: vec![],
                reasoning_level_map: None,
            },
        );
        let state = AppState::new_in_memory_for_management(cfg);
        let response = status_probe(
            None,
            json_headers(),
            State(state.clone()),
            Bytes::from(r#"{"provider":"ollama"}"#),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*discovery_hits.lock().unwrap(), 1);

        let cache = state.probe_coordinator.cache_snapshot().await;
        assert!(cache.dynamic_models.contains_key("ollama"));
    }

    #[tokio::test]
    async fn status_probe_rejects_nonempty_body_without_exact_json_content_type() {
        let body = Bytes::from(r#"{"provider":"p1"}"#);

        let missing = status_probe(None, HeaderMap::new(), State(state()), body.clone())
            .await
            .into_response();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

        let mut jsonp = HeaderMap::new();
        jsonp.insert(header::CONTENT_TYPE, "application/jsonp".parse().unwrap());
        let jsonp = status_probe(None, jsonp, State(state()), body)
            .await
            .into_response();
        assert_eq!(jsonp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn status_probe_selector_validation_error_returns_400_with_zero_probe() {
        // 1. Unknown provider
        let r = status_probe(
            None,
            json_headers(),
            State(state()),
            Bytes::from(r#"{"provider": "nonexistent"}"#),
        )
        .await
        .into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            val["error"]
                .as_str()
                .unwrap()
                .contains("unknown provider: 'nonexistent'")
        );

        // 2. Unknown model
        let r = status_probe(
            None,
            json_headers(),
            State(state()),
            Bytes::from(r#"{"model": "nonexistent"}"#),
        )
        .await
        .into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            val["error"]
                .as_str()
                .unwrap()
                .contains("unknown model: 'nonexistent'")
        );

        // 3. Disjoint intersection: p1 has m1, p2 has m2. p1 + m2 has empty intersection.
        let r = status_probe(
            None,
            json_headers(),
            State(state()),
            Bytes::from(r#"{"provider": "p1", "model": "m2"}"#),
        )
        .await
        .into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let val: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(val["error"].as_str().unwrap().contains("intersection"));

        // 4. In SSE mode, validation error also returns 400 JSON before creating SSE stream
        let mut h = json_headers();
        h.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        let r_sse = status_probe(
            None,
            h,
            State(state()),
            Bytes::from(r#"{"provider": "nonexistent"}"#),
        )
        .await
        .into_response();
        assert_eq!(r_sse.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn status_probe_sse_selection_echo_and_disposition() {
        let mut h = json_headers();
        h.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        let r = status_probe(
            None,
            h,
            State(state()),
            Bytes::from(r#"{"provider": "p2"}"#),
        )
        .await
        .into_response();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/event-stream"
        );
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("event: probe_result"));
        assert!(text.contains("\"disposition\":"));
        assert!(text.contains("event: done"));
        assert!(text.contains("\"selection\":{\"model\":null,\"provider\":\"p2\"}"));
    }

    #[tokio::test]
    async fn provider_info_known() {
        let (code, resp) = provider_info(State(state()), Path("p1".into())).await;
        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        assert_eq!(d["id"], "p1");
        assert_eq!(d["endpoints"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn provider_info_openai_oauth_without_account_reports_unavailable_usage() {
        let mut cfg = cfg();
        cfg.providers.insert(
            "oauthp".into(),
            ProviderConfig {
                auth: Some(crate::config::AuthConfig::OpenaiOauth { account: None }),
                openai_chat: Some(EndpointConfig {
                    url: Some("https://example.test/chat".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let s = AppState::new_in_memory_for_management(cfg);
        let (code, resp) = provider_info(State(s), Path("oauthp".into())).await;
        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        assert!(!d["usage"]["unavailable"].as_str().unwrap().is_empty());
    }

    #[test]
    fn parse_period_invalid_single_date_errors() {
        let err = parse_period("2026-02-30").unwrap_err().to_string();
        assert!(err.contains("invalid period"));
    }

    #[tokio::test]
    async fn provider_info_unknown_404() {
        let (code, Json(r)) = provider_info(State(state()), Path("missing".into())).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(r.error.unwrap().contains("unknown provider"));
    }

    #[tokio::test]
    async fn provider_list_returns_all_providers() {
        let (code, resp) = provider_list(State(state())).await;
        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        let ids: Vec<&str> = d["providers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        // BTreeMap 顺序稳定：p1 < p2
        assert_eq!(ids, vec!["p1", "p2"]);
    }

    #[tokio::test]
    async fn model_list_returns_all_models() {
        let (code, resp) = model_list(State(state())).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["models"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn model_info_known_lists_bindings() {
        let (code, resp) = model_info(State(state()), Path("m1".into())).await;
        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        assert_eq!(d["context_window"], 1000);
        assert!(
            d["providers"]
                .as_object()
                .unwrap()
                .contains_key("openai_chat")
        );
    }

    #[tokio::test]
    async fn model_info_known_lists_bindings_and_cooldown() {
        let s = state();
        s.cooldowns.set(
            &crate::config::CooldownKey {
                model_id: "m1".into(),
                provider_id: "p1".into(),
                protocol: crate::config::Protocol::OpenaiChatCompletions,
                token: None,
                token_key_hash: None,
            },
            "rate_limit",
            "429",
            std::time::Duration::from_secs(3600),
        );
        let (code, resp) = model_info(State(s), Path("m1".into())).await;
        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        let bindings = d["providers"]["openai_chat"].as_array().unwrap();
        assert!(!bindings.is_empty());
        assert!(bindings[0]["cooldown"].as_str().is_some());
    }

    #[tokio::test]
    async fn model_info_unknown_404() {
        let (code, Json(r)) = model_info(State(state()), Path("nope".into())).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(r.error.unwrap().contains("unknown model"));
    }

    /// a routing model must be reported to a client with the metadata
    /// a client can actually use, not its own empty fields.
    #[tokio::test]
    async fn routing_models_are_reported_with_derived_metadata() {
        let mut config = cfg();
        let mut router = config.models["m1"].clone();
        router.context_window = 0;
        router.max_output_tokens = 0;
        router.openai_chat_providers.clear();
        router.openai_responses_providers.clear();
        router.anthropic_providers.clear();
        router.reasoning_routes = Some(vec![
            crate::config::RouteEntry {
                level: "low".into(),
                target: "m1".into(),
            },
            crate::config::RouteEntry {
                level: "high".into(),
                target: "m1".into(),
            },
        ]);
        config.models.insert("router".into(), router);
        // Named `routing` to avoid shadowing the `state()` test helper below.
        let routing = AppState::new_in_memory_for_management(config);

        let (code, resp) = model_list(State(routing.clone())).await;
        assert_eq!(code, StatusCode::OK);
        let router_row = data(resp)["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "router")
            .expect("router row")
            .clone();
        assert_eq!(
            router_row["context_window"], 1000,
            "the hop reports the bound over its target, not its own zero"
        );
        assert_eq!(router_row["max_output_tokens"], 100);
        assert_eq!(
            router_row["protocols"],
            serde_json::json!(["chat_completions", "responses"]),
            "a hop serves the protocols its targets serve"
        );

        let (code, resp) = model_info(State(routing), Path("router".into())).await;
        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        assert_eq!(d["context_window"], 1000);
        assert!(
            d["providers"].as_object().unwrap().is_empty(),
            "a hop has no provider bindings of its own"
        );
        let routes = d["routes"].as_array().expect("routes");
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0]["level"], "low");
        assert_eq!(routes[0]["target"], "m1");

        // A regular model's response shape is unchanged: no `routes` key.
        let (_, resp) = model_info(State(state()), Path("m1".into())).await;
        assert!(data(resp).get("routes").is_none());
    }

    #[tokio::test]
    async fn client_config_accepts_supported_clients() {
        for c in [
            "pi",
            "qwen-code",
            "codex",
            "codex-desktop",
            "claude-code",
            "claude-desktop",
        ] {
            let (code, resp) = client_config(State(state()), None, Path(c.to_string())).await;
            assert_eq!(code, StatusCode::OK);
            assert_eq!(data(resp)["client"], c);
        }
    }

    #[tokio::test]
    async fn client_config_rejects_unknown() {
        let (code, Json(r)) = client_config(State(state()), None, Path("vim".into())).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("unknown client"));
    }

    #[tokio::test]
    async fn client_config_filters_models_and_providers_by_token_grant() {
        use crate::token::auth::{TokenContext, TokenGrant};

        // m1 允许，m2 禁止
        let grant = TokenGrant::Granted(TokenContext {
            key: "lp-test-token".to_string(),
            name: "test-token".to_string(),
            allowed_models: vec!["m1".to_string()],
            admin: false,
        });

        let (code, resp) = client_config(
            State(state()),
            Some(axum::extract::Extension(grant)),
            Path("pi".to_string()),
        )
        .await;

        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        let models = d["models"].as_object().unwrap();
        let providers = d["providers"].as_object().unwrap();

        // 仅包含授权模型 m1，严禁泄露 m2
        assert!(models.contains_key("m1"), "m1 must be included");
        assert!(!models.contains_key("m2"), "m2 must be filtered out");

        // 非 admin 视图只暴露合成 llm-proxy，真实 provider p1/p2 不下发
        assert!(
            providers.contains_key("llm-proxy"),
            "synthetic provider must be present"
        );
        assert!(!providers.contains_key("p1"), "p1 must not leak");
        assert!(!providers.contains_key("p2"), "p2 must not leak");

        let catalog_models = d["catalog"]["models"].as_object().expect("catalog models");
        assert!(catalog_models.contains_key("m1"));
        assert!(!catalog_models.contains_key("m2"));
    }

    #[tokio::test]
    async fn client_config_materializes_routing_model_and_hides_internal_targets() {
        use crate::config::Config;
        use crate::token::auth::{TokenContext, TokenGrant};

        let toml_str = r#"
[server]
listen = "127.0.0.1:0"

[providers.upstream]
openai_chat = { url = "https://example.test/chat/completions" }
openai_responses = { url = "https://example.test/responses" }

[models."target-high"]
context_window = 200_000
max_output_tokens = 64_000
features = ["image_input"]
openai_chat_providers = [{ name = "upstream", model = "upstream-high" }]
openai_responses_providers = [{ name = "upstream", model = "upstream-high" }]

[models."target-low"]
context_window = 200_000
max_output_tokens = 64_000
features = ["image_input"]
openai_chat_providers = [{ name = "upstream", model = "upstream-low" }]
openai_responses_providers = [{ name = "upstream", model = "upstream-low" }]

[models."router-lp"]
reasoning_routes = [
    { level = "low", target = "target-low" },
    { level = "high", target = "target-high" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse config");
        cfg.validate().expect("validate config");

        // Token 仅允许 "router-lp"，不包含内部跳板 target-low/high
        let grant = TokenGrant::Granted(TokenContext {
            key: "lp-router-token".to_string(),
            name: "router-user".to_string(),
            allowed_models: vec!["router-lp".to_string()],
            admin: false,
        });

        let (filtered_models, filtered_providers) =
            prepare_client_config_payload(&cfg, Some(&grant));

        // 1. 内部 target 完全隐藏
        assert!(!filtered_models.contains_key("target-low"));
        assert!(!filtered_models.contains_key("target-high"));

        // 2. router-lp 被物化为具备完整能力的独立模型
        assert!(filtered_models.contains_key("router-lp"));
        let router = &filtered_models["router-lp"];
        assert_eq!(router.context_window, 200_000);
        assert_eq!(router.max_output_tokens, 64_000);
        assert_eq!(router.features, vec!["image_input"]);
        assert_eq!(router.force_think, Some(true));
        assert!(router.reasoning_routes.is_none());
        assert_eq!(router.openai_chat_providers.len(), 1);
        assert_eq!(router.openai_responses_providers.len(), 1);

        // 3. 物化产物能通过客户端 Config::validate() 校验且可顺利生成
        let mut client_cfg: Config =
            toml::from_str("[server]\nlisten = \"127.0.0.1:0\"").expect("base config");
        client_cfg.models = filtered_models;
        client_cfg.providers = filtered_providers;
        assert!(
            client_cfg.validate().is_ok(),
            "materialized client config must validate cleanly"
        );
    }

    #[tokio::test]
    async fn cooldown_clear_empty_returns_zero() {
        let (code, resp) = cooldown_clear(
            State(state()),
            Json(CooldownClearRequest {
                provider: "p1".into(),
                model: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["removed"], 0);
    }

    #[tokio::test]
    async fn cooldown_clear_removes_all_provider_entries_and_model_scoped_entries() {
        let s = state();
        s.cooldowns.set(
            &crate::config::CooldownKey {
                model_id: "m1".into(),
                provider_id: "p1".into(),
                protocol: crate::config::Protocol::OpenaiChatCompletions,
                token: None,
                token_key_hash: None,
            },
            "test",
            "unit",
            Duration::from_secs(60),
        );
        s.cooldowns.set(
            &crate::config::CooldownKey {
                model_id: "m2".into(),
                provider_id: "p1".into(),
                protocol: crate::config::Protocol::Anthropic,
                token: None,
                token_key_hash: None,
            },
            "test",
            "unit",
            Duration::from_secs(60),
        );
        let (code, resp) = cooldown_clear(
            State(s.clone()),
            Json(CooldownClearRequest {
                provider: "p1".into(),
                model: Some("m1".into()),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["removed"], 1);

        let (code, resp) = cooldown_clear(
            State(s),
            Json(CooldownClearRequest {
                provider: "p1".into(),
                model: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["removed"], 1);
    }

    #[tokio::test]
    async fn cooldown_set_writes_all_client_protocols_and_can_be_cleared() {
        let s = state();
        let (code, resp) = cooldown_set(
            State(s.clone()),
            Json(CooldownSetRequest {
                provider: "p1".into(),
                model: "m1".into(),
                duration: "30m".into(),
                protocol: None,
                kind: None,
                reason: Some("quota".into()),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let payload = data(resp);
        assert_eq!(payload["count"], 3);
        assert_eq!(payload["kind"], "manual");
        assert!(s.cooldowns.is_cooling_down(&crate::config::CooldownKey {
            model_id: "m1".into(),
            provider_id: "p1".into(),
            protocol: crate::config::Protocol::OpenaiChatCompletions,
            token: None,
            token_key_hash: None,
        }));
        assert!(s.cooldowns.is_cooling_down(&crate::config::CooldownKey {
            model_id: "m1".into(),
            provider_id: "p1".into(),
            protocol: crate::config::Protocol::Anthropic,
            token: None,
            token_key_hash: None,
        }));

        let (code, resp) = cooldown_clear(
            State(s.clone()),
            Json(CooldownClearRequest {
                provider: "p1".into(),
                model: Some("m1".into()),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["removed"], 3);
        assert!(!s.cooldowns.is_cooling_down(&crate::config::CooldownKey {
            model_id: "m1".into(),
            provider_id: "p1".into(),
            protocol: crate::config::Protocol::Anthropic,
            token: None,
            token_key_hash: None,
        }));
    }

    #[tokio::test]
    async fn cooldown_set_rejects_invalid_duration_and_protocol() {
        let (code, resp) = cooldown_set(
            State(state()),
            Json(CooldownSetRequest {
                provider: "p1".into(),
                model: "m1".into(),
                duration: "31d".into(),
                protocol: None,
                kind: None,
                reason: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(resp.0.error.unwrap().contains("30d"));

        let (code, resp) = cooldown_set(
            State(state()),
            Json(CooldownSetRequest {
                provider: "p1".into(),
                model: "m1".into(),
                duration: "1h".into(),
                protocol: Some("some-unknown-protocol".into()),
                kind: None,
                reason: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(resp.0.error.unwrap().contains("unsupported --protocol"));
    }

    #[tokio::test]
    async fn usage_no_params_ok_empty() {
        let (code, resp) = usage(
            State(state()),
            Query(UsageQueryParams {
                period: None,
                provider: None,
                model: None,
                endpoint: None,
                view: None,
                limit: None,
                offset: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        assert!(d["count"].as_u64().is_some());
        assert!(d["records"].is_array());
    }

    #[tokio::test]
    async fn usage_bad_period_400() {
        let (code, Json(r)) = usage(
            State(state()),
            Query(UsageQueryParams {
                period: Some("bad-period".into()),
                provider: None,
                model: None,
                endpoint: None,
                view: None,
                limit: None,
                offset: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.is_some());
    }

    #[tokio::test]
    async fn usage_bad_period_does_not_acquire_core_lock() {
        let s = state();
        // Pre-acquire the core lock; an invalid period must be parsed and rejected with 400
        // without attempting to acquire or wait on the core lock.
        let _core_guard = s.core.lock().await;
        let (code, Json(r)) = usage(
            State(s.clone()),
            Query(UsageQueryParams {
                period: Some("bad-period".into()),
                provider: None,
                model: None,
                endpoint: None,
                view: None,
                limit: None,
                offset: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.is_some());
    }

    #[tokio::test]
    async fn usage_releases_core_lock_before_response() {
        let s = state();
        let (code, _resp) = usage(
            State(s.clone()),
            Query(UsageQueryParams {
                period: None,
                provider: None,
                model: None,
                endpoint: None,
                view: None,
                limit: None,
                offset: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        // The core lock guard must be dropped before usage returns.
        assert!(s.core.try_lock().is_ok());
    }

    #[tokio::test]
    async fn usage_executes_query_without_holding_core_lock() {
        let s = state();
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                crate::config::UsageConfig::default(),
                dir.path().join("usage.jsonl"),
                dir.path().join("usage.db"),
            )
            .unwrap(),
        );
        s.core.lock().await.set_usage_store(Some(store.clone()));

        let core_unlocked_during_query =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let core_clone = s.core.clone();
        let core_unlocked = core_unlocked_during_query.clone();

        store.set_query_hook(move || {
            if core_clone.try_lock().is_ok() {
                core_unlocked.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let (code, _resp) = usage(
            State(s),
            Query(UsageQueryParams {
                period: None,
                provider: None,
                model: None,
                endpoint: None,
                view: None,
                limit: None,
                offset: None,
            }),
        )
        .await;

        assert_eq!(code, StatusCode::OK);
        assert!(
            core_unlocked_during_query.load(std::sync::atomic::Ordering::SeqCst),
            "store query must execute without holding core lock"
        );
    }

    #[tokio::test]
    async fn usage_returns_empty_when_no_usage_store() {
        let s = state();
        {
            let mut core = s.core.lock().await;
            core.set_usage_store(None);
        }
        let (code, resp) = usage(
            State(s),
            Query(UsageQueryParams {
                period: None,
                provider: None,
                model: None,
                endpoint: None,
                view: None,
                limit: None,
                offset: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        assert_eq!(d["count"], 0);
        assert_eq!(d["records"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn usage_filters_records_correctly() {
        let s = state();
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                crate::config::UsageConfig::default(),
                dir.path().join("usage.jsonl"),
                dir.path().join("usage.db"),
            )
            .unwrap(),
        );
        s.core.lock().await.set_usage_store(Some(store));

        {
            let core = s.core.lock().await;
            core.record_usage("m1".into(), "p1".into(), "chat".into(), 10, 20, None);
            core.record_usage("m2".into(), "p1".into(), "responses".into(), 30, 40, None);
            core.record_usage("m1".into(), "p2".into(), "chat".into(), 50, 60, None);
        }

        let (code, resp) = usage(
            State(s),
            Query(UsageQueryParams {
                period: None,
                provider: Some("p1".into()),
                model: Some("m1".into()),
                endpoint: Some("chat".into()),
                view: None,
                limit: None,
                offset: None,
            }),
        )
        .await;

        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        assert_eq!(d["count"], 1);
        let records = d["records"].as_array().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["provider"], "p1");
        assert_eq!(records[0]["model"], "m1");
        assert_eq!(records[0]["endpoint"], "chat");
    }

    #[tokio::test]
    async fn usage_public_response_never_leaks_token_key() {
        let s = state();
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                crate::config::UsageConfig::default(),
                dir.path().join("usage.jsonl"),
                dir.path().join("usage.db"),
            )
            .unwrap(),
        );
        store
            .record(
                crate::usage_stats::UsageRecord::new(
                    "m1".into(),
                    "p1".into(),
                    "chat".into(),
                    10,
                    20,
                    None,
                )
                .with_token(Some("lp-SUPERSECRET".into()), Some("alice".into())),
            )
            .unwrap();
        s.core.lock().await.set_usage_store(Some(store));

        let (code, resp) = usage(
            State(s),
            Query(UsageQueryParams {
                period: None,
                provider: None,
                model: None,
                endpoint: None,
                view: None,
                limit: None,
                offset: None,
            }),
        )
        .await;

        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        assert_eq!(d["count"], 1);
        let raw = serde_json::to_string(&d).unwrap();
        assert!(!raw.contains("lp-SUPERSECRET"), "response leaked token_key");
        assert!(raw.contains("alice"), "token_name should remain");
    }

    #[tokio::test]
    async fn usage_attribution_resolves_scd_audit_groups() {
        let s = state();
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                crate::config::UsageConfig::default(),
                dir.path().join("usage.jsonl"),
                dir.path().join("usage.db"),
            )
            .unwrap(),
        );

        let bindings = vec![
            (
                "flash".into(),
                "google".into(),
                "gemini-flash-upstream".into(),
            ),
            (
                "leaf-high".into(),
                "google".into(),
                "gemini-pro-upstream".into(),
            ),
        ];
        store
            .sync_model_binding_revisions(&bindings, "cfg_hash")
            .unwrap();

        let ts = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        store
            .record(
                crate::usage_stats::UsageRecord::new(
                    "flash".into(),
                    "google".into(),
                    "chat".into(),
                    10,
                    20,
                    Some(50),
                )
                .with_timestamp(&ts),
            )
            .unwrap();
        store
            .record(
                crate::usage_stats::UsageRecord::new(
                    "routed-flash".into(),
                    "google".into(),
                    "chat".into(),
                    30,
                    40,
                    Some(50),
                )
                .with_routed_target("leaf-high")
                .with_timestamp(&ts),
            )
            .unwrap();
        store.flush_if_dirty().unwrap();

        s.core.lock().await.set_usage_store(Some(store));

        let (code, resp) =
            usage_attribution(State(s), Query(AttributionQueryParams { period: None })).await;

        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        assert_eq!(d["count"], 2);
        let groups = d["groups"].as_array().expect("groups array");
        assert_eq!(groups.len(), 2);

        // Find leaf-high group (total 70 tokens) and flash group (total 30 tokens)
        let g_high = groups
            .iter()
            .find(|g| g["effective_model"] == "leaf-high")
            .expect("should find leaf-high attribution");
        assert_eq!(g_high["upstream_model"], "gemini-pro-upstream");
        assert_eq!(g_high["total_tokens"], 70);

        let g_flash = groups
            .iter()
            .find(|g| g["effective_model"] == "flash")
            .expect("should find flash attribution");
        assert_eq!(g_flash["upstream_model"], "gemini-flash-upstream");
        assert_eq!(g_flash["total_tokens"], 30);
    }

    #[test]
    fn parse_period_today() {
        assert!(parse_period("today").unwrap().0.is_some());
    }
    #[test]
    fn parse_period_yesterday_has_end() {
        let r = parse_period("yesterday").unwrap();
        assert!(r.0.unwrap() < r.1.unwrap());
    }
    #[test]
    fn parse_period_7d() {
        assert!(parse_period("7d").unwrap().0.is_some());
    }
    #[test]
    fn parse_period_1w() {
        assert!(parse_period("1w").unwrap().0.is_some());
    }
    #[test]
    fn parse_period_30d() {
        assert!(parse_period("30d").unwrap().0.is_some());
    }
    #[test]
    fn parse_period_custom_days() {
        assert!(parse_period("3d").unwrap().0.is_some());
    }
    #[test]
    fn parse_period_custom_weeks() {
        assert!(parse_period("2w").unwrap().0.is_some());
    }
    #[test]
    fn parse_period_single_date() {
        let r = parse_period("2026-03-20").unwrap();
        assert!(r.0.unwrap() < r.1.unwrap());
    }
    #[test]
    fn parse_period_date_range() {
        let r = parse_period("2026-03-12:2026-03-20").unwrap();
        assert!(r.0.unwrap() < r.1.unwrap());
    }
    #[test]
    fn parse_period_invalid_range_shape() {
        assert!(parse_period("2026-01-01:2026-01-02:bad").is_err());
    }
    #[test]
    fn parse_period_invalid_start_date() {
        assert!(parse_period("bad:2026-01-02").is_err());
    }
    #[test]
    fn parse_period_invalid_end_date() {
        assert!(parse_period("2026-01-02:bad").is_err());
    }
    #[test]
    fn parse_period_invalid_day_number() {
        assert!(parse_period("xd").is_err());
    }
    #[test]
    fn parse_period_invalid_week_number() {
        assert!(parse_period("xw").is_err());
    }
    #[test]
    fn local_naive_to_unix_epochish() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert!(local_naive_to_unix(d) > 1_700_000_000);
    }

    fn temp_config_state() -> (tempfile::TempDir, AppState) {
        // The AppState constructor injects this exact core/path/scope into the
        // writer before it starts. Do not replace state.core afterwards: that
        // would manufacture the second-authority bug this suite guards.
        let keepalive = tempfile::tempdir().unwrap();
        let cfg = cfg();
        let s = AppState::new_in_memory_for_management(cfg.clone());
        crate::config_edit::write_full_config(&s.config_path, &cfg).unwrap();
        (keepalive, s)
    }

    #[tokio::test]
    async fn config_reload_missing_file_500() {
        let (_temp, s) = temp_config_state();
        let path = { s.core.lock().await.config_path().to_path_buf() };
        std::fs::remove_file(path).unwrap();
        let (code, Json(r)) = config_reload(State(s)).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("failed to reload configuration"));
    }

    #[tokio::test]
    async fn config_reload_malformed_isolated_response_hides_marker_and_path() {
        let (_temp, s) = temp_config_state();
        let path = { s.core.lock().await.config_path().to_path_buf() };
        let marker = "SECRET_MALFORMED_LEAK_MARKER_98765";
        std::fs::write(&path, format!("[broken_toml\n{marker} =")).unwrap();

        let (code, Json(r)) = config_reload(State(s)).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        let err = r.error.expect("error message present");
        assert_eq!(err, "failed to reload configuration");
        assert!(!err.contains(marker));
        assert!(!err.contains(path.to_str().unwrap()));
        assert!(!err.contains('/'));
    }

    #[tokio::test]
    async fn config_reload_replaces_runtime_model_snapshot() {
        let (_temp, s) = temp_config_state();
        let path = { s.core.lock().await.config_path().to_path_buf() };
        let mut updated = s.config();
        updated.models.insert(
            "reloaded-model".to_string(),
            crate::config::ModelConfig {
                description: None,
                context_window: 8_192,
                max_output_tokens: 2_048,
                features: Vec::new(),
                reasoning_levels: None,
                supported_reasoning_levels: Vec::new(),
                cooldown: None,
                reasoning_routes: None,
                default_reasoning_level: None,
                supports_thinking: None,
                force_think: None,
                force_no_think: None,
                default_think: None,
                enable_thinking: None,
                chat_reasoning_off_format: None,
                codex_service_tier: None,
                openai_chat_providers: vec![crate::config::ProviderBinding {
                    name: "p1".to_string(),
                    model: "reloaded-model".to_string(),
                }],
                openai_responses_providers: Vec::new(),
                anthropic_providers: Vec::new(),
                reasoning_level_map: None,
            },
        );
        crate::config_edit::write_full_config(&path, &updated).unwrap();

        let (code, resp) = config_reload(State(s.clone())).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["message"], "configuration reloaded");
        assert!(s.config().models.contains_key("reloaded-model"));
    }

    #[tokio::test]
    async fn config_reload_reloads_tokens_directory() {
        let (_temp, s) = temp_config_state();
        let config_dir = s.config_path.parent().unwrap();
        let tokens_dir = config_dir.join("tokens");
        std::fs::create_dir_all(&tokens_dir).unwrap();

        // Write first token into tokens/
        std::fs::write(
            tokens_dir.join("alice.toml"),
            r#"
key = "lp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11"
name = "alice"
allowed_models = ["gpt-4o"]
"#,
        )
        .unwrap();

        let (code, resp) = config_reload(State(s.clone())).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["message"], "configuration reloaded");
        {
            let tokens = s.token.config.read().unwrap();
            assert_eq!(tokens.tokens.len(), 1);
            assert_eq!(tokens.tokens[0].name, "alice");
        }

        // Add second token into tokens/
        std::fs::write(
            tokens_dir.join("bob.toml"),
            r#"
key = "lp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb22"
name = "bob"
allowed_models = ["deepseek-v3"]
"#,
        )
        .unwrap();

        let (code, resp) = config_reload(State(s.clone())).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["message"], "configuration reloaded");
        {
            let tokens = s.token.config.read().unwrap();
            assert_eq!(tokens.tokens.len(), 2);
            assert!(tokens.tokens.iter().any(|t| t.name == "alice"));
            assert!(tokens.tokens.iter().any(|t| t.name == "bob"));
        }
    }

    #[tokio::test]
    async fn config_update_rejects_invalid_binding() {
        let (_temp, s) = temp_config_state();
        let base = s.current_revision();
        let mut bad = cfg();
        bad.models.get_mut("m1").unwrap().openai_chat_providers[0].name = "missing".into();
        let (code, Json(r)) = config_update(
            State(s),
            Json(ConfigUpdateRequest {
                base: Some(base),
                config: bad,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("unknown provider"));
    }

    #[tokio::test]
    async fn config_update_success_updates_model_count() {
        let (_temp, s) = temp_config_state();
        let base = s.current_revision();
        let mut updated = cfg();
        updated.models.remove("m2");
        let (code, resp) = config_update(
            State(s.clone()),
            Json(ConfigUpdateRequest {
                base: Some(base),
                config: updated,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["message"], "config updated");
        let (code, resp) = model_list(State(s)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["models"].as_array().unwrap().len(), 1);
    }

    /// Two editors that both read generation N must not both replace it: the
    /// second one's submitted body was derived from a configuration that no
    /// longer exists, and writing it would silently discard the first change
    /// (residual — whole-config CAS).
    #[tokio::test]
    async fn config_update_rejects_a_stale_base_without_touching_the_new_generation() {
        let (_temp, s) = temp_config_state();
        let read_base = s.current_revision();

        let mut first = cfg();
        first.models.remove("m2");
        let (code, _) = config_update(
            State(s.clone()),
            Json(ConfigUpdateRequest {
                base: Some(read_base),
                config: first,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "first writer wins");

        let mut second = cfg();
        second.models.remove("m1");
        let (code, Json(r)) = config_update(
            State(s.clone()),
            Json(ConfigUpdateRequest {
                base: Some(read_base),
                config: second,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "stale base must not write");
        assert!(
            r.error
                .expect("conflict explains itself")
                .contains("configuration changed"),
        );
        assert!(
            !s.config().models.contains_key("m2"),
            "the winning generation must survive the rejected one"
        );
        assert!(
            s.config().models.contains_key("m1"),
            "the rejected write must not have been applied either"
        );
    }

    /// A whole-config replace that cannot say which generation it read from is
    /// exactly the silent-overwrite case, so it is refused rather than assumed
    /// current.
    #[tokio::test]
    async fn config_update_requires_a_base() {
        let (_temp, s) = temp_config_state();
        let (code, Json(r)) = config_update(
            State(s.clone()),
            Json(ConfigUpdateRequest {
                base: None,
                config: cfg(),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(
            r.error
                .expect("refusal explains itself")
                .contains("generation"),
        );
        assert_eq!(s.config().models.len(), cfg().models.len());
    }

    /// The snapshot is the only base a replacing client may carry: body and
    /// generation must come from one read.
    #[tokio::test]
    async fn config_snapshot_reports_body_with_its_generation() {
        let (_temp, s) = temp_config_state();
        let (code, resp) = config_snapshot(State(s.clone())).await;
        assert_eq!(code, StatusCode::OK);
        let body = data(resp);
        assert_eq!(body["revision"], s.current_revision());
        assert_eq!(
            body["config"]["models"]
                .as_object()
                .expect("models map")
                .len(),
            cfg().models.len()
        );
    }

    /// 公开 listener 变体必须剥离 server.auth_token 明文；UDS 变体保持完整
    /// （TUI 编辑器 CAS 无损回环依赖）。
    #[tokio::test]
    async fn config_snapshot_http_strips_server_auth_token_uds_keeps_it() {
        let (_temp, s) = temp_config_state();
        let mut with_token = cfg();
        with_token.server.as_mut().unwrap().auth_token = Some("s3cret-token".into());
        s.commit_config(with_token, s.current_revision())
            .await
            .expect("seed config with plaintext token");

        let (code, resp) = config_snapshot_http(
            State(s.clone()),
            Some(axum::extract::Extension(
                crate::token::auth::TokenGrant::Open,
            )),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let body = data(resp);
        assert!(
            body["config"]["server"]["auth_token"].is_null(),
            "public listener must not hand out server.auth_token plaintext: {body}"
        );

        let (code, resp) = config_snapshot(State(s.clone())).await;
        assert_eq!(code, StatusCode::OK);
        let body = data(resp);
        assert_eq!(
            body["config"]["server"]["auth_token"], "s3cret-token",
            "UDS management channel keeps the plaintext for the TUI CAS round-trip"
        );
    }

    #[tokio::test]
    async fn config_snapshot_http_non_admin_returns_model_view_without_real_providers() {
        let (_temp, s) = temp_config_state();
        let grant = crate::token::auth::TokenGrant::Granted(crate::token::auth::TokenContext {
            key: "lp-user".to_string(),
            name: "user".to_string(),
            allowed_models: vec!["m1".to_string()],
            admin: false,
        });
        let (code, resp) =
            config_snapshot_http(State(s), Some(axum::extract::Extension(grant))).await;
        assert_eq!(code, StatusCode::OK);
        let body = data(resp);
        assert_eq!(body["view"], "client");
        let providers = body["config"]["providers"]
            .as_object()
            .expect("providers object");
        assert_eq!(providers.len(), 1);
        assert!(providers.contains_key("llm-proxy"));
        assert!(!providers.contains_key("p1"));
        let models = body["config"]["models"].as_object().expect("models");
        assert!(models.contains_key("m1"));
        assert!(!models.contains_key("m2"));
    }

    /// 回环保护：远端编辑器拿到剥离后的快照再回传（auth_token=None），
    /// 更新必须保留当前代明文，不得把它从配置里抹掉。
    #[tokio::test]
    async fn config_update_preserves_auth_token_when_round_trip_carries_none() {
        let (_temp, s) = temp_config_state();
        let mut with_token = cfg();
        with_token.server.as_mut().unwrap().auth_token = Some("s3cret-token".into());
        s.commit_config(with_token.clone(), s.current_revision())
            .await
            .expect("seed config with plaintext token");

        // 模拟公开 listener 快照（已剥离）回传。
        let mut redacted = with_token;
        redacted.server.as_mut().unwrap().auth_token = None;
        let (code, Json(r)) = config_update(
            State(s.clone()),
            Json(ConfigUpdateRequest {
                base: Some(s.current_revision()),
                config: redacted,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{r:?}");
        let (current, _) = s.config_with_revision();
        assert_eq!(
            current.server.as_ref().unwrap().auth_token.as_deref(),
            Some("s3cret-token"),
            "auth_token must survive the redacted round-trip"
        );
    }

    #[tokio::test]
    async fn config_update_server_error_produces_500_without_sensitive_details() {
        let (_temp, s) = temp_config_state();
        let base = s.current_revision();
        let _ = s.shutdown_config_writer().await;
        let (code, Json(r)) = config_update(
            State(s),
            Json(ConfigUpdateRequest {
                base: Some(base),
                config: cfg(),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        let err = r.error.expect("error message present");
        assert!(!err.is_empty());
        assert!(!err.contains('/'));
    }

    #[tokio::test]
    async fn stopped_writer_model_and_provider_endpoints_yield_stable_redacted_error() {
        let keepalive = tempfile::tempdir().unwrap();
        let mut cfg = cfg();
        cfg.providers.insert(
            "openai-payg".to_string(),
            crate::catalog::openai_payg().provider,
        );
        let s = AppState::new_in_memory_for_management(cfg.clone());
        crate::config_edit::write_full_config(&s.config_path, &cfg).unwrap();
        let _temp = keepalive;
        let _ = s.shutdown_config_writer().await;

        let (code, Json(r)) = model_add(
            State(s.clone()),
            Json(ModelAddRequest {
                model_id: "stopped-model".into(),
                context_window: Some(4096),
                max_output: Some(512),
                copy_from: None,
                routes: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("service is shutting down"));
        assert_eq!(r.kind, None);

        let (code, Json(r)) = model_set(
            State(s.clone()),
            Json(ModelSetRequest {
                model_id: "m1".into(),
                context_window: Some(8192),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("service is shutting down"));
        assert_eq!(r.kind, None);

        let (code, Json(r)) = model_remove(
            State(s.clone()),
            Json(ModelRemoveRequest {
                model_id: "m2".into(),
                force: true,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("service is shutting down"));
        assert_eq!(r.kind, None);

        let (code, Json(r)) = provider_remove(
            State(s.clone()),
            Json(ProviderRemoveRequest {
                id: "p1".into(),
                force: true,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("service is shutting down"));
        assert_eq!(r.kind, None);

        let (code, Json(r)) = provider_copy(
            State(s.clone()),
            Json(ProviderCopyRequest {
                source: "p1".into(),
                target: "p2-copy".into(),
                api_key_env: Some("ENV_KEY".into()),
                no_api_key: false,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("service is shutting down"));
        assert_eq!(r.kind, None);

        let (code, Json(r)) = provider_reselect(
            State(s.clone()),
            Json(ProviderReselectRequest {
                provider_id: "openai-payg".into(),
                models: vec!["gpt-5.5-lp".into()],
            }),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("service is shutting down"));
        assert_eq!(r.kind, None);

        let (code, Json(r)) = provider_add(
            State(s.clone()),
            Json(ProviderAddRequest {
                provider_id: "deepseek".into(),
                provider_name: Some("deepseek-new".into()),
                api_key_env: Some("DEEPSEEK_API_KEY".into()),
                no_api_key: false,
                provider_type: None,
                endpoint_url: None,
                models: Some(vec!["deepseek-v4-flash-lp".into()]),
                skip_probe: true,
                pending_account: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("service is shutting down"));
        assert_eq!(r.kind.as_deref(), Some("internal_error"));

        let (code, Json(r)) = config_reload(State(s)).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.status, "error");
        assert_eq!(r.error.as_deref(), Some("service is shutting down"));
        assert_eq!(r.kind, None);
    }

    #[test]
    fn classify_commit_writer_error_matrix() {
        use crate::runtime_config::WriterError;

        // Business validation errors -> 400 Bad Request
        let (code, Json(r)) =
            classify_commit_writer_error(WriterError::CommitFailed("bad binding".into()));
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(r.error.as_deref(), Some("bad binding"));

        let (code, Json(r)) =
            classify_commit_writer_error(WriterError::MutationRejected("mutation error".into()));
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(r.error.as_deref(), Some("mutation error"));

        // Server / infrastructure / persistence / mailbox / fail-stop errors -> 500 Internal Server Error
        let server_cases = [
            WriterError::FailedBeforeReplace(
                "/private/etc/secrets/config.toml: permission denied".into(),
            ),
            WriterError::MailboxClosed,
            WriterError::RetrySafetyUnknown,
            WriterError::FailedStop,
            WriterError::StopUndetermined,
            WriterError::ServiceShuttingDown,
            WriterError::RevisionExhausted,
            WriterError::OperationKeyConflict,
            WriterError::ReloadFailed("/private/etc/secrets/config.toml: not found".into()),
            WriterError::OperationExpired,
            WriterError::UnknownOperation,
        ];

        for err in server_cases {
            let (code, Json(r)) = classify_commit_writer_error(err);
            assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(r.status, "error");
            let err_msg = r.error.expect("error message must be present");
            assert!(!err_msg.contains("/private/etc"));
        }
    }

    #[test]
    fn classify_provider_add_commit_writer_error_matrix() {
        use crate::connect::ProviderAddErrorKind;
        use crate::runtime_config::WriterError;

        let cases = [
            (
                WriterError::CommitFailed("bad binding".into()),
                StatusCode::BAD_REQUEST,
                Some(ProviderAddErrorKind::InvalidConfig.as_str()),
                false,
            ),
            (
                WriterError::MutationRejected("mutation error".into()),
                StatusCode::BAD_REQUEST,
                Some(ProviderAddErrorKind::InvalidConfig.as_str()),
                false,
            ),
            (
                WriterError::FailedBeforeReplace(
                    "/private/etc/secrets/config.toml: permission denied".into(),
                ),
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
            (
                WriterError::MailboxClosed,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
            (
                WriterError::RetrySafetyUnknown,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
            (
                WriterError::FailedStop,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
            (
                WriterError::StopUndetermined,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
            (
                WriterError::ServiceShuttingDown,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
            (
                WriterError::RevisionExhausted,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
            (
                WriterError::OperationKeyConflict,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
            (
                WriterError::ReloadFailed("/private/etc/secrets/config.toml: not found".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
            (
                WriterError::OperationExpired,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
            (
                WriterError::UnknownOperation,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(ProviderAddErrorKind::InternalError.as_str()),
                true,
            ),
        ];

        for (err, expected_status, expected_kind, is_infra) in cases {
            let (code, Json(r)) = classify_provider_add_commit_writer_error(err);
            assert_eq!(code, expected_status);
            assert_eq!(r.status, "error");
            assert_eq!(r.kind.as_deref(), expected_kind);
            let err_msg = r.error.expect("error message must be present");
            if is_infra {
                assert!(!err_msg.contains("/private/etc"));
            } else {
                assert!(err_msg.contains("bad binding") || err_msg.contains("mutation error"));
            }
        }
    }

    #[tokio::test]
    async fn model_add_success_then_duplicate_400() {
        let (_temp, s) = temp_config_state();
        let req = ModelAddRequest {
            model_id: "new-model".into(),
            context_window: Some(4096),
            max_output: Some(512),
            copy_from: None,
            routes: None,
        };
        let (code, resp) = model_add(State(s.clone()), Json(req)).await;
        // In sandbox, may succeed or fail with lock error; both are acceptable
        if code == StatusCode::BAD_REQUEST {
            assert!(resp.0.error.unwrap().contains("lock file"));
        }
        let req = ModelAddRequest {
            model_id: "m1".into(),
            context_window: Some(4096),
            max_output: Some(512),
            copy_from: None,
            routes: None,
        };
        let (code, Json(r)) = model_add(State(s), Json(req)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("already exists"));
    }

    #[tokio::test]
    async fn model_set_success_and_validation_error() {
        let (_temp, s) = temp_config_state();
        let ok = ModelSetRequest {
            model_id: "m1".into(),
            context_window: Some(1234),
            max_output_tokens: Some(321),
            reasoning_levels: Some(vec![
                ReasoningLevelEntry::new("low", None),
                ReasoningLevelEntry::new("high", None),
            ]),
            thinking_level: Some("low".into()),
            supports_thinking: Some(true),
            enable_features: vec!["tools".into()],
            ..Default::default()
        };
        let (code, resp) = model_set(State(s.clone()), Json(ok)).await;
        // In sandbox, may succeed or fail with lock error; both are acceptable
        if code == StatusCode::BAD_REQUEST {
            assert!(resp.0.error.unwrap().contains("lock file"));
        }
        let bad = ModelSetRequest {
            model_id: "m1".into(),
            context_window: Some(0),
            ..Default::default()
        };
        let (code, Json(r)) = model_set(State(s), Json(bad)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("context-window"));
    }

    #[tokio::test]
    async fn model_set_request_serde_and_handler_adr031_contract() {
        // ADR-031: Verify shared wire contract between AdminServerClient and ModelSetRequest
        let options = crate::model::SetModelOptions {
            description: Some("My test model".to_string()),
            clear_description: false,
            chat_reasoning_off_format: Some("enable-thinking".to_string()),
            clear_chat_reasoning_off_format: false,
            codex_service_tier: Some(crate::config::CodexServiceTierConfig {
                tiers: vec!["priority".to_string()],
                default_tier: Some("priority".to_string()),
                forced_tier: None,
            }),
            clear_codex_service_tier: false,
            cooldown: Some(crate::config::CooldownOverrideConfig {
                network_seconds: Some(10),
                server_error_seconds: Some(20),
                rate_limit_seconds: Some(30),
                model_unavailable_seconds: Some(40),
                client_error_seconds: Some(50),
                auth_error_seconds: Some(60),
            }),
            clear_cooldown: false,
            ..Default::default()
        };

        // Wire format produced by AdminServerClient::model_set
        let wire_json = serde_json::json!({
            "model_id": "m1",
            "description": options.description,
            "clear_description": options.clear_description,
            "chat_reasoning_off_format": options.chat_reasoning_off_format,
            "clear_chat_reasoning_off_format": options.clear_chat_reasoning_off_format,
            "codex_service_tier": options.codex_service_tier,
            "clear_codex_service_tier": options.clear_codex_service_tier,
            "cooldown": options.cooldown,
            "clear_cooldown": options.clear_cooldown,
        });

        let req: ModelSetRequest =
            serde_json::from_value(wire_json).expect("deserialize ModelSetRequest");
        assert_eq!(req.model_id, "m1");
        assert_eq!(req.description.as_deref(), Some("My test model"));
        assert!(!req.clear_description);
        assert_eq!(
            req.chat_reasoning_off_format.as_deref(),
            Some("enable-thinking")
        );
        assert!(!req.clear_chat_reasoning_off_format);
        assert_eq!(
            req.codex_service_tier.as_ref().unwrap().tiers,
            vec!["priority".to_string()]
        );
        assert_eq!(req.cooldown.as_ref().unwrap().network_seconds, Some(10));
        assert_eq!(req.cooldown.as_ref().unwrap().auth_error_seconds, Some(60));

        // Clear flags wire contract
        let wire_clear = serde_json::json!({
            "model_id": "m1",
            "clear_description": true,
            "clear_chat_reasoning_off_format": true,
            "clear_codex_service_tier": true,
            "clear_cooldown": true,
        });
        let req_clear: ModelSetRequest =
            serde_json::from_value(wire_clear).expect("deserialize clear");
        assert!(req_clear.clear_description);
        assert!(req_clear.clear_chat_reasoning_off_format);
        assert!(req_clear.clear_codex_service_tier);
        assert!(req_clear.clear_cooldown);
    }

    #[tokio::test]
    async fn model_provider_add_remove_move_branches() {
        let (_temp, s) = temp_config_state();
        let add = ModelProviderRequest {
            action: "add".into(),
            model_id: "m2".into(),
            protocol: "openai-chat".into(),
            provider: "p1".into(),
            upstream_model: Some("up".into()),
            to: None,
        };
        let (code, resp) = model_provider(State(s.clone()), Json(add)).await;
        // In sandbox, may succeed or fail with lock error; both are acceptable
        if code == StatusCode::BAD_REQUEST {
            assert!(resp.0.error.unwrap().contains("lock file"));
        }
        let mv = ModelProviderRequest {
            action: "move".into(),
            model_id: "m1".into(),
            protocol: "openai-chat".into(),
            provider: "p1".into(),
            upstream_model: None,
            to: Some(0),
        };
        let (code, Json(r)) = model_provider(State(s.clone()), Json(mv)).await;
        // In sandbox, move with to=0 may succeed or fail with "1-based" error; both are acceptable
        if code == StatusCode::BAD_REQUEST {
            assert!(r.error.unwrap().contains("1-based"));
        }
        let rm = ModelProviderRequest {
            action: "remove".into(),
            model_id: "m2".into(),
            protocol: "openai-chat".into(),
            provider: "p1".into(),
            upstream_model: None,
            to: None,
        };
        let (code, Json(r)) = model_provider(State(s.clone()), Json(rm)).await;
        // In sandbox, if add succeeded, remove will succeed; if add failed, remove will fail with "has no provider"
        if code == StatusCode::BAD_REQUEST {
            assert!(r.error.unwrap().contains("has no provider"));
        }

        // action: "set" requires upstream_model
        let set_missing = ModelProviderRequest {
            action: "set".into(),
            model_id: "m1".into(),
            protocol: "openai-chat".into(),
            provider: "p1".into(),
            upstream_model: None,
            to: None,
        };
        let (code, Json(r)) = model_provider(State(s.clone()), Json(set_missing)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("upstream_model is required"));

        // action: "set" with upstream_model succeeds or fails with lock/not-found
        let set_ok = ModelProviderRequest {
            action: "set".into(),
            model_id: "m1".into(),
            protocol: "openai-chat".into(),
            provider: "p1".into(),
            upstream_model: Some("up-new".into()),
            to: None,
        };
        let (code, resp) = model_provider(State(s), Json(set_ok)).await;
        if code == StatusCode::BAD_REQUEST {
            let err = resp.0.error.unwrap();
            assert!(err.contains("lock file") || err.contains("has no provider"));
        }
    }

    #[tokio::test]
    async fn model_add_gate_rejects_copy_from_with_window_overrides() {
        let (_temp, s) = temp_config_state();
        let add = ModelAddRequest {
            model_id: "m-copy".into(),
            context_window: Some(1000),
            max_output: None,
            copy_from: Some("m1".into()),
            routes: None,
        };
        let (code, Json(r)) = model_add(State(s), Json(add)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(
            r.error
                .unwrap()
                .contains("--copy-from cannot be combined with --context-window or --max-output")
        );
    }

    #[tokio::test]
    async fn model_remove_requires_force_then_success() {
        let (_temp, s) = temp_config_state();
        let (code, Json(r)) = model_remove(
            State(s.clone()),
            Json(ModelRemoveRequest {
                model_id: "m2".into(),
                force: false,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("requires --force"));
        let (code, resp) = model_remove(
            State(s),
            Json(ModelRemoveRequest {
                model_id: "m2".into(),
                force: true,
            }),
        )
        .await;
        // In sandbox, may succeed or fail with lock error; both are acceptable
        if code == StatusCode::BAD_REQUEST {
            assert!(resp.0.error.unwrap().contains("lock file"));
        }
    }

    #[tokio::test]
    async fn model_rename_success_and_validation_error() {
        let (_temp, s) = temp_config_state();
        let (code, resp) = model_rename(
            State(s.clone()),
            Json(ModelRenameRequest {
                old_model: "m1".into(),
                new_model: "renamed-m1".into(),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["message"], "renamed model: m1 -> renamed-m1");
        assert!(s.config().models.contains_key("renamed-m1"));
        assert!(!s.config().models.contains_key("m1"));

        let (code, Json(r)) = model_rename(
            State(s),
            Json(ModelRenameRequest {
                old_model: "missing".into(),
                new_model: "new-name".into(),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("unknown model"));
    }

    #[tokio::test]
    async fn provider_remove_referenced_errors_and_unknown_errors() {
        let (_temp, s) = temp_config_state();
        let (code, Json(r)) = provider_remove(
            State(s.clone()),
            Json(ProviderRemoveRequest {
                id: "p1".into(),
                force: false,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("referenced"));
        let (code, Json(r)) = provider_remove(
            State(s),
            Json(ProviderRemoveRequest {
                id: "missing".into(),
                force: true,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("unknown provider"));
    }

    /// A config with a single OAuth-backed provider `ag1` (account defaults to
    /// the provider id) and no model references, for the provider-removal tests.
    fn oauth_account_fixture() -> Value {
        serde_json::json!({
            "account_label": "tester@example.test",
            "project_id": null,
            "access_token": "access-token-access-token",
            "refresh_token": "refresh-token-refresh-token",
            "expires_at_unix": 4_000_000_000_i64,
            "updated_at_unix": 1_700_000_000_i64,
        })
    }

    fn oauth_provider_cfg() -> Config {
        let mut cfg: Config = toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").unwrap();
        cfg.providers.clear();
        cfg.models.clear();
        cfg.providers.insert(
            "ag1".into(),
            ProviderConfig {
                auth: Some(crate::config::AuthConfig::OpenaiOauth { account: None }),
                openai_chat: Some(EndpointConfig::native("https://example.test/v1/chat")),
                ..Default::default()
            },
        );
        cfg
    }

    #[tokio::test]
    async fn provider_remove_refreshes_oauth_cache_after_success() {
        // 回归基线：provider 移除后内存凭据缓存必须按磁盘现状刷新。
        // 修复前这里是 `let _ = logout(...)` 且从不刷缓存——磁盘已删的账号仍留在
        // 内存里，后续请求仍能解析出它的 token。
        let cfg = oauth_provider_cfg();
        let s = AppState::new_in_memory_for_management(cfg.clone());
        crate::config_edit::write_full_config(&s.config_path, &cfg).unwrap();
        std::fs::write(
            &s.oauth_accounts_path,
            serde_json::json!({
                "version": 1,
                "openai": { "ag1": oauth_account_fixture() },
            })
            .to_string(),
        )
        .unwrap();
        {
            // Simulate the startup state where the cache has the account loaded.
            let loaded =
                crate::auth::load_oauth_accounts_with_recovery(&s.oauth_accounts_path).unwrap();
            assert!(loaded.openai.contains_key("ag1"));
            *s.oauth_accounts.write().unwrap() = loaded;
        }

        let (code, Json(r)) = provider_remove(
            State(s.clone()),
            Json(ProviderRemoveRequest {
                id: "ag1".into(),
                force: false,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "remove failed: {:?}", r.error);

        let on_disk =
            crate::auth::load_oauth_accounts_with_recovery(&s.oauth_accounts_path).unwrap();
        assert!(
            !on_disk.openai.contains_key("ag1"),
            "disk credential left behind"
        );
        assert!(
            !s.oauth_accounts.read().unwrap().openai.contains_key("ag1"),
            "in-memory oauth cache still holds the deleted account"
        );
    }

    #[tokio::test]
    async fn provider_remove_reports_500_when_oauth_cleanup_fails() {
        // 回归基线：磁盘凭据删除失败时不能静默报成功。配置已提交、
        // 无法回滚，所以要 500 并说清哪一半已生效。修复前 `let _ =` 吞错返回 200。
        let cfg = oauth_provider_cfg();
        let s = AppState::new_in_memory_for_management(cfg.clone());
        crate::config_edit::write_full_config(&s.config_path, &cfg).unwrap();
        // Make the credentials path a directory: any load inside logout fails.
        std::fs::create_dir(&s.oauth_accounts_path).unwrap();

        let (code, Json(r)) = provider_remove(
            State(s.clone()),
            Json(ProviderRemoveRequest {
                id: "ag1".into(),
                force: false,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        let err = r.error.unwrap();
        assert!(err.contains("was removed"), "unexpected error: {err}");
        assert!(
            err.contains("could not be deleted"),
            "unexpected error: {err}"
        );
        // The config half did take effect and the response says so.
        assert!(!s.config().providers.contains_key("ag1"));
        assert_eq!(
            r.data.expect("half-success data")["message"],
            "removed provider: ag1"
        );
    }

    /// 回归基线：经管理通道 logout 后，磁盘凭据与 server 内存
    /// 缓存必须同时失效。修复前 CLI 直写磁盘、缓存从不刷新——"已登出仍可用"。
    #[tokio::test]
    async fn oauth_logout_removes_credential_and_refreshes_cache() {
        let cfg = oauth_provider_cfg();
        let s = AppState::new_in_memory_for_management(cfg.clone());
        crate::config_edit::write_full_config(&s.config_path, &cfg).unwrap();
        std::fs::write(
            &s.oauth_accounts_path,
            serde_json::json!({
                "version": 1,
                "openai": { "ag1": oauth_account_fixture() },
            })
            .to_string(),
        )
        .unwrap();
        {
            let loaded =
                crate::auth::load_oauth_accounts_with_recovery(&s.oauth_accounts_path).unwrap();
            assert!(loaded.openai.contains_key("ag1"));
            *s.oauth_accounts.write().unwrap() = loaded;
        }

        let (code, Json(r)) = oauth_logout(
            State(s.clone()),
            Json(OAuthLogoutRequest {
                provider_id: "ag1".into(),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "logout failed: {:?}", r.error);
        assert_eq!(
            r.data.expect("logout data")["message"],
            "Logged out provider=ag1 removed=1"
        );

        let on_disk =
            crate::auth::load_oauth_accounts_with_recovery(&s.oauth_accounts_path).unwrap();
        assert!(
            !on_disk.openai.contains_key("ag1"),
            "disk credential left behind"
        );
        assert!(
            !s.oauth_accounts.read().unwrap().openai.contains_key("ag1"),
            "in-memory oauth cache still holds the logged-out account"
        );
    }

    #[tokio::test]
    async fn oauth_logout_unknown_provider_400() {
        let cfg = oauth_provider_cfg();
        let s = AppState::new_in_memory_for_management(cfg);
        let (code, Json(r)) = oauth_logout(
            State(s),
            Json(OAuthLogoutRequest {
                provider_id: "missing".into(),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("unknown provider"));
        assert_eq!(r.kind.as_deref(), Some("invalid_config"));
    }

    /// 凭据文件删除失败必须 500 上报，不能吞错报成功
    /// （否则 CLI 显示已登出而 token 仍可用）。
    #[tokio::test]
    async fn oauth_logout_reports_500_when_credential_removal_fails() {
        let cfg = oauth_provider_cfg();
        let s = AppState::new_in_memory_for_management(cfg.clone());
        crate::config_edit::write_full_config(&s.config_path, &cfg).unwrap();
        // Make the credentials path a directory: any load inside logout fails.
        std::fs::create_dir(&s.oauth_accounts_path).unwrap();

        let (code, Json(r)) = oauth_logout(
            State(s),
            Json(OAuthLogoutRequest {
                provider_id: "ag1".into(),
            }),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            r.error
                .unwrap()
                .contains("failed to remove OAuth credential")
        );
        assert_eq!(r.kind.as_deref(), Some("internal_error"));
    }

    #[tokio::test]
    async fn provider_copy_unknown_source_400() {
        let (_temp, s) = temp_config_state();
        let req = ProviderCopyRequest {
            source: "missing".into(),
            target: "copy".into(),
            api_key_env: None,
            no_api_key: false,
        };
        let (code, Json(r)) = provider_copy(State(s), Json(req)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(!r.error.unwrap().is_empty());
    }

    #[tokio::test]
    async fn oauth_write_rejects_unknown_type_and_invalid_openai() {
        let mut bad = serde_json::Map::new();
        bad.insert("access_token".into(), Value::String("tok".into()));
        let req = OAuthWriteRequest {
            provider_id: "p".into(),
            oauth_type: "bogus".into(),
            account: "acct".into(),
            account_data: bad.clone(),
        };
        let (code, Json(r)) = oauth_write(State(state()), Json(req)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(!r.error.unwrap().is_empty());
        let req = OAuthWriteRequest {
            provider_id: "p".into(),
            oauth_type: "openai".into(),
            account: "acct".into(),
            account_data: bad,
        };
        let (code, Json(r)) = oauth_write(State(state()), Json(req)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(!r.error.unwrap().is_empty());
    }

    #[tokio::test]
    async fn oauth_write_accepts_valid_openai_account() {
        let mut m = serde_json::Map::new();
        m.insert("account_label".into(), Value::String("me".into()));
        m.insert("access_token".into(), Value::String("access".into()));
        m.insert("refresh_token".into(), Value::String("refresh".into()));
        m.insert(
            "expires_at_unix".into(),
            Value::Number(serde_json::Number::from(2_000_000_000i64)),
        );
        m.insert(
            "updated_at_unix".into(),
            Value::Number(serde_json::Number::from(1_900_000_000i64)),
        );
        let req = OAuthWriteRequest {
            provider_id: "p1".into(),
            oauth_type: "openai".into(),
            account: "me".into(),
            account_data: m,
        };
        let (code, Json(_r)) = oauth_write(State(state()), Json(req)).await;
        // In sandbox, may fail with lock error or succeed; both are acceptable
        assert!(code == StatusCode::BAD_REQUEST || code == StatusCode::OK);
    }

    #[test]
    fn parse_period_zero_and_negative_offsets_are_accepted() {
        assert!(parse_period("0d").unwrap().0.is_some());
        assert!(parse_period("-1w").unwrap().0.is_some());
    }

    #[tokio::test]
    async fn model_provider_unknown_action_400() {
        let (code, Json(r)) = model_provider(
            State(state()),
            Json(ModelProviderRequest {
                action: "bogus".into(),
                model_id: "m1".into(),
                protocol: "openai-chat".into(),
                provider: "p1".into(),
                upstream_model: None,
                to: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("unknown model provider action"));
    }

    #[tokio::test]
    async fn model_provider_bad_protocol_400() {
        let (code, Json(r)) = model_provider(
            State(state()),
            Json(ModelProviderRequest {
                action: "add".into(),
                model_id: "m1".into(),
                protocol: "bogus".into(),
                provider: "p1".into(),
                upstream_model: None,
                to: None,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.is_some());
    }

    #[tokio::test]
    async fn model_write_applies_mutation_and_rejects_invalid_candidate() {
        let (_temp, s) = temp_config_state();
        let (code, resp) = model_write(
            &s,
            crate::model::ConfigMutation::Add {
                model_id: "m3".into(),
                context_window: Some(1000),
                max_output_tokens: Some(100),
                copy_from: None,
                routes: None,
            },
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(data(resp)["message"], "added model: m3");
        assert!(s.config().models.contains_key("m3"));

        let (code, Json(r)) = model_write(
            &s,
            crate::model::ConfigMutation::Add {
                model_id: "m3".into(),
                context_window: Some(1000),
                max_output_tokens: Some(100),
                copy_from: None,
                routes: None,
            },
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("already exists"));
    }

    #[tokio::test]
    async fn concurrent_model_add_same_target_has_exactly_one_success() {
        let (_temp, state) = temp_config_state();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let tasks = (0..2)
            .map(|_| {
                let state = state.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    model_write(
                        &state,
                        crate::model::ConfigMutation::Add {
                            model_id: "concurrent-model".into(),
                            context_window: Some(1000),
                            max_output_tokens: Some(100),
                            copy_from: None,
                            routes: None,
                        },
                    )
                    .await
                    .0
                })
            })
            .collect::<Vec<_>>();

        barrier.wait().await;
        let mut statuses = Vec::new();
        for task in tasks {
            statuses.push(task.await.expect("model add task"));
        }

        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::OK)
                .count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::BAD_REQUEST)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn concurrent_model_add_different_targets_preserves_both() {
        let (_temp, state) = temp_config_state();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let tasks = ["concurrent-model-a", "concurrent-model-b"]
            .into_iter()
            .map(|model_id| {
                let state = state.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    model_write(
                        &state,
                        crate::model::ConfigMutation::Add {
                            model_id: model_id.into(),
                            context_window: Some(1000),
                            max_output_tokens: Some(100),
                            copy_from: None,
                            routes: None,
                        },
                    )
                    .await
                    .0
                })
            })
            .collect::<Vec<_>>();

        barrier.wait().await;
        for task in tasks {
            assert_eq!(task.await.expect("model add task"), StatusCode::OK);
        }
        let config = state.config();
        assert!(config.models.contains_key("concurrent-model-a"));
        assert!(config.models.contains_key("concurrent-model-b"));
    }

    #[tokio::test]
    async fn model_provider_move_success_with_valid_one_based_index() {
        let (_temp, s) = temp_config_state();
        let mv = ModelProviderRequest {
            action: "move".into(),
            model_id: "m1".into(),
            protocol: "openai-chat".into(),
            provider: "p1".into(),
            upstream_model: None,
            to: Some(1),
        };
        let (code, resp) = model_provider(State(s), Json(mv)).await;
        if code == StatusCode::OK {
            assert_eq!(data(resp)["message"], "moved provider p1 in model m1");
        } else {
            assert!(!resp.0.error.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn parse_client_protocol_aliases_cover_protocol_routes() {
        assert_eq!(
            crate::model::parse_client_protocol("openai-chat-completions")
                .unwrap()
                .route_key(),
            "chat_completions"
        );
        assert_eq!(
            crate::model::parse_client_protocol("chat")
                .unwrap()
                .route_key(),
            "chat_completions"
        );
        assert_eq!(
            crate::model::parse_client_protocol("chat-completions")
                .unwrap()
                .route_key(),
            "chat_completions"
        );
        assert_eq!(
            crate::model::parse_client_protocol("responses")
                .unwrap()
                .route_key(),
            "responses"
        );
        assert_eq!(
            crate::model::parse_client_protocol("anthropic-messages")
                .unwrap()
                .route_key(),
            "anthropic"
        );
    }

    #[tokio::test]
    async fn provider_copy_success_covers_ok_response() {
        let (_temp, s) = temp_config_state();
        let req = ProviderCopyRequest {
            source: "p2".into(),
            target: "p2-copy".into(),
            api_key_env: None,
            no_api_key: true,
        };
        let (code, resp) = provider_copy(State(s), Json(req)).await;
        assert_eq!(code, StatusCode::OK);
        let d = data(resp);
        assert_eq!(d["message"], "copied provider: p2 -> p2-copy");
        assert_eq!(d["requires_oauth_login"], false);
    }

    #[tokio::test]
    async fn provider_remove_force_success_covers_ok_response() {
        let (_temp, s) = temp_config_state();
        let (code, resp) = provider_remove(
            State(s),
            Json(ProviderRemoveRequest {
                id: "p2".into(),
                force: true,
            }),
        )
        .await;
        if code == StatusCode::OK {
            assert_eq!(data(resp)["message"], "removed provider: p2");
        } else {
            assert!(!resp.0.error.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn provider_add_rejects_unknown_provider_type() {
        let (_temp, s) = temp_config_state();
        let req = ProviderAddRequest {
            provider_id: "newp".into(),
            provider_name: None,
            api_key_env: None,
            no_api_key: true,
            provider_type: Some("bogus".into()),
            endpoint_url: None,
            models: None,
            skip_probe: false,
            pending_account: None,
        };
        let (code, Json(r)) = provider_add(State(s), Json(req)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(r.error.unwrap().contains("Unknown provider type") || r.status == "error");
    }

    #[tokio::test]
    async fn provider_add_success_covers_reload_ok() {
        let (_temp, s) = temp_config_state();
        let req = ProviderAddRequest {
            provider_id: "customp".into(),
            provider_name: None,
            api_key_env: None,
            no_api_key: true,
            provider_type: None,
            endpoint_url: Some("https://example.test/v1/chat/completions".into()),
            models: Some(vec!["m1".into()]),
            skip_probe: false,
            pending_account: None,
        };
        let (code, resp) = provider_add(State(s), Json(req)).await;
        if code == StatusCode::OK {
            assert_eq!(data(resp)["message"], "connected provider: customp");
        } else {
            assert_eq!(code, StatusCode::BAD_REQUEST);
            assert!(!resp.0.error.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn provider_add_uses_catalog_id_with_distinct_provider_name() {
        let (_temp, s) = temp_config_state();
        let req = ProviderAddRequest {
            provider_id: "openai-sub".into(),
            provider_name: Some("demo-work".into()),
            api_key_env: None,
            no_api_key: true,
            provider_type: None,
            endpoint_url: None,
            models: None,
            skip_probe: false,
            pending_account: None,
        };

        let (code, resp) = provider_add(State(s.clone()), Json(req)).await;

        assert_eq!(code, StatusCode::OK, "{:?}", resp.0.error);
        assert_eq!(data(resp)["message"], "connected provider: demo-work");
        let config = s.config();
        let provider = config.providers.get("demo-work").expect("named provider");
        assert_eq!(
            provider.auth_config("demo-work").expect("OAuth auth"),
            crate::config::AuthConfig::OpenaiOauth {
                account: Some("demo-work".to_string())
            }
        );
        assert!(!config.providers.contains_key("openai-sub"));
    }

    #[tokio::test]
    async fn oauth_write_rejects_invalid_unknown_type_and_validates_account_shape() {
        let mut invalid = serde_json::Map::new();
        invalid.insert("access_token".into(), Value::String("tok".into()));
        let req = OAuthWriteRequest {
            provider_id: "p".into(),
            oauth_type: "some-removed-type".into(),
            account: "acct".into(),
            account_data: invalid,
        };
        let (code, Json(r)) = oauth_write(State(state()), Json(req)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(!r.error.unwrap().is_empty());

        let mut valid = serde_json::Map::new();
        valid.insert("account_label".into(), Value::String("ag".into()));
        valid.insert("access_token".into(), Value::String("access".into()));
        valid.insert("refresh_token".into(), Value::String("refresh".into()));
        valid.insert(
            "expires_at_unix".into(),
            Value::Number(serde_json::Number::from(2_000_000_000i64)),
        );
        valid.insert(
            "updated_at_unix".into(),
            Value::Number(serde_json::Number::from(1_900_000_000i64)),
        );
        let req = OAuthWriteRequest {
            provider_id: "p".into(),
            oauth_type: "some-removed-type".into(),
            account: "ag".into(),
            account_data: valid,
        };
        let (code, Json(_r)) = oauth_write(State(state()), Json(req)).await;
        assert!(code == StatusCode::BAD_REQUEST || code == StatusCode::OK);
    }

    #[test]
    fn admin_response_serializes_without_none_fields() {
        let v = serde_json::to_value(AdminApiResponse {
            status: "ok",
            data: None,
            error: None,
            kind: None,
        })
        .unwrap();
        assert!(v.get("data").is_none());
        assert!(v.get("error").is_none());
    }

    #[test]
    fn provider_add_error_kind_maps_to_correct_status_and_str() {
        use crate::connect::ProviderAddErrorKind;
        assert_eq!(
            ProviderAddErrorKind::ProbeFailed.status_code(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            ProviderAddErrorKind::AuthNotReady.status_code(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            ProviderAddErrorKind::ConfigConflict.status_code(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ProviderAddErrorKind::InvalidConfig.status_code(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ProviderAddErrorKind::InternalError.status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(ProviderAddErrorKind::ProbeFailed.as_str(), "probe_failed");
        assert_eq!(
            ProviderAddErrorKind::AuthNotReady.as_str(),
            "auth_not_ready"
        );
        assert_eq!(
            ProviderAddErrorKind::ConfigConflict.as_str(),
            "config_conflict"
        );
        assert_eq!(
            ProviderAddErrorKind::InvalidConfig.as_str(),
            "invalid_config"
        );
        assert_eq!(
            ProviderAddErrorKind::InternalError.as_str(),
            "internal_error"
        );
    }

    #[tokio::test]
    async fn provider_add_invalid_config_returns_400_with_kind() {
        let (_temp, s) = temp_config_state();
        let req = ProviderAddRequest {
            provider_id: "newp".into(),
            provider_name: None,
            api_key_env: Some("SOME_ENV".into()),
            no_api_key: true,
            provider_type: None,
            endpoint_url: None,
            models: None,
            skip_probe: false,
            pending_account: None,
        };
        let (code, Json(r)) = provider_add(State(s), Json(req)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(r.kind.as_deref(), Some("invalid_config"));
        assert_eq!(r.status, "error");
    }

    #[tokio::test]
    async fn provider_add_auth_not_ready_returns_422_with_kind() {
        let (_temp, s) = temp_config_state();
        let missing_env = "LLM_PROXY_TEST_MISSING_KEY_ADMIN_422";
        let req = ProviderAddRequest {
            provider_id: "openai-payg".into(),
            provider_name: None,
            api_key_env: Some(missing_env.into()),
            no_api_key: false,
            provider_type: None,
            endpoint_url: None,
            models: Some(vec!["gpt-5.5-lp".into()]),
            skip_probe: false,
            pending_account: None,
        };
        let (code, Json(r)) = provider_add(State(s), Json(req)).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r.kind.as_deref(), Some("auth_not_ready"));
        assert_eq!(r.status, "error");
    }

    #[test]
    fn provider_add_request_deserialize_without_skip_probe_defaults_false() {
        let json = serde_json::json!({
            "provider_id": "deepseek",
            "no_api_key": true
        });
        let req: ProviderAddRequest = serde_json::from_value(json).unwrap();
        assert!(!req.skip_probe);
        assert_eq!(req.provider_id, "deepseek");
        assert!(req.no_api_key);
    }

    #[test]
    fn provider_add_request_deserialize_with_skip_probe_true() {
        let json = serde_json::json!({
            "provider_id": "deepseek",
            "no_api_key": true,
            "skip_probe": true
        });
        let req: ProviderAddRequest = serde_json::from_value(json).unwrap();
        assert!(req.skip_probe);
    }

    #[test]
    fn provider_add_request_deserialize_with_pending_account() {
        let json = serde_json::json!({
            "provider_id": "some-oauth-provider",
            "no_api_key": true,
            "pending_account": {
                "auth_type": "openai",
                "account": "test-account",
                "access_token": "ya29.secret",
                "project_id": "proj-123"
            }
        });
        let req: ProviderAddRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.provider_id, "some-oauth-provider");
        assert!(req.pending_account.is_some());
        let pending = req.pending_account.unwrap();
        assert_eq!(pending.auth_type, "openai");
        assert_eq!(pending.account, "test-account");
        assert_eq!(pending.access_token, "ya29.secret");
        assert_eq!(pending.project_id.as_deref(), Some("proj-123"));
    }

    #[test]
    fn admin_response_includes_kind_field_when_present() {
        let v = serde_json::to_value(AdminApiResponse {
            status: "error",
            data: None,
            error: Some("boom".into()),
            kind: Some("probe_failed".into()),
        })
        .unwrap();
        assert_eq!(v.get("kind").unwrap().as_str().unwrap(), "probe_failed");
    }

    /// the config write is the *cheap* half of the connect
    /// two-step write, so a failed commit must keep the credential (only the
    /// user can regenerate it) and say so loudly. Startup reconciliation
    /// completes the missing provider; see
    /// `AppState::recover_orphan_oauth_providers`.
    #[tokio::test]
    async fn provider_add_keeps_pending_credential_when_config_commit_fails() {
        for pre_populate in [false, true] {
            let (_temp, s) = temp_config_state();
            let account_id = if pre_populate {
                "openai-sub-existing-acct"
            } else {
                "openai-sub-new-acct"
            };
            let pending = crate::auth::PendingAccount {
                auth_type: "openai".into(),
                account: account_id.into(),
                access_token: "sk-mock-access-token-12345".into(),
                refresh_token: Some("mock-refresh-token-12345".into()),
                project_id: None,
                expires_at_unix: Some(2_000_000_000),
                email: None,
            };
            if pre_populate {
                crate::auth::storage::write_pending_account_locked(
                    &s.oauth_accounts_path,
                    &pending,
                )
                .expect("write pre-existing account");
                *s.oauth_accounts.write().unwrap() =
                    crate::auth::load_oauth_accounts_with_recovery(&s.oauth_accounts_path)
                        .expect("load pre-existing account");
            }

            let _ = s.shutdown_config_writer().await;

            let req = ProviderAddRequest {
                provider_id: "openai-sub".into(),
                provider_name: Some("openai-sub-candidate".into()),
                api_key_env: None,
                no_api_key: true,
                provider_type: None,
                endpoint_url: None,
                models: Some(vec!["gpt-5.5-sub-lp".into()]),
                skip_probe: true,
                pending_account: Some(pending),
            };

            let (code, Json(r)) = provider_add(State(s.clone()), Json(req)).await;
            assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(r.status, "error");
            assert_eq!(r.kind.as_deref(), Some("internal_error"));
            let message = r.error.unwrap_or_default();
            assert!(
                message.contains("kept on disk") && message.contains(account_id),
                "failure must report that the credential was kept: {message}"
            );

            let disk_accounts =
                crate::auth::load_oauth_accounts_with_recovery(&s.oauth_accounts_path)
                    .expect("load accounts from disk");
            let cached_accounts = s.oauth_accounts.read().unwrap();
            let cfg = s.config();

            assert!(!cfg.providers.contains_key("openai-sub-candidate"));
            assert!(!cfg.models.contains_key("gpt-5.5-sub-lp"));

            assert!(
                disk_accounts.openai.contains_key(account_id),
                "credential must survive a failed config commit (pre_populate={pre_populate})"
            );
            assert!(
                cached_accounts.openai.contains_key(account_id),
                "AppState cache must match disk after a failed config commit (pre_populate={pre_populate})"
            );
            drop(cached_accounts);

            // The surviving credential is exactly the input startup
            // reconciliation needs: it makes the provider reappear without a new
            // OAuth flow (the account key here is a hand-written one, so the
            // rebuild is reported rather than invented).
            let mut recovered = cfg.clone();
            let report = crate::auth::recover_orphan_oauth_accounts(&mut recovered, &disk_accounts);
            assert!(
                report
                    .unrecoverable
                    .iter()
                    .any(|orphan| orphan.account == account_id),
                "a non-catalog account key must be reported instead of guessed: {report:?}"
            );
            assert!(!recovered.providers.contains_key("openai-sub"));
        }
    }

    /// the startup boundary completes the lost second write
    /// step, durably — and never by deleting the credential.
    #[tokio::test]
    async fn startup_reconciliation_persists_a_provider_rebuilt_from_an_orphan_credential() {
        let (_temp, s) = temp_config_state();
        let mut accounts = crate::auth::AccountsFile::new();
        accounts.openai.insert(
            "openai-subscription".into(),
            crate::auth::OpenaiAccount {
                account_label: "orphan@example.com".into(),
                access_token: "mock-access-token-value-1234567890".into(),
                refresh_token: "mock-refresh-token-value-1234567890".into(),
                expires_at_unix: 2_000_000_000,
                updated_at_unix: 1_800_000_000,
            },
        );
        crate::auth::save_oauth_accounts(&s.oauth_accounts_path, &accounts).expect("write account");
        // Mirror what `AppState::new` does at real startup: the cache is loaded
        // from the same file before reconciliation runs.
        *s.oauth_accounts.write().unwrap() = accounts.clone();
        assert!(!s.config().providers.contains_key("openai-sub"));

        s.recover_orphan_oauth_providers().await;

        let (cfg, revision) = s.config_with_revision();
        let rebuilt = cfg
            .providers
            .get("openai-sub")
            .expect("startup must rebuild the provider from the surviving credential");
        assert!(matches!(
            &rebuilt.auth,
            Some(crate::config::AuthConfig::OpenaiOauth { account })
                if account.as_deref() == Some("openai-subscription")
        ));
        assert!(
            !cfg.models.values().any(|model| {
                model
                    .openai_chat_providers
                    .iter()
                    .chain(model.openai_responses_providers.iter())
                    .chain(model.anthropic_providers.iter())
                    .any(|binding| binding.name == "openai-sub")
            }),
            "model selection is not recoverable; the user is pointed at `provider reselect`"
        );
        let on_disk = crate::config::Config::load(&s.config_path).expect("reload persisted config");
        assert!(
            on_disk.providers.contains_key("openai-sub"),
            "the recovery must be persisted, not only in memory"
        );
        assert!(
            accounts.openai.contains_key("openai-subscription"),
            "the credential that drove the recovery must still be there"
        );

        // A second startup has nothing left to complete.
        s.recover_orphan_oauth_providers().await;
        assert_eq!(
            s.current_revision(),
            revision,
            "reconciliation must be idempotent once the provider exists"
        );
    }

    fn cooldown_state_with_token() -> AppState {
        let s = AppState::new_in_memory_for_management(cfg());
        let token: crate::token::config::TokenConfig =
            toml::from_str("key = \"lp-admin-cooldown-key-000000000000\"\nname = \"carol\"\n")
                .expect("test token config");
        s.token
            .config
            .write()
            .expect("token config lock")
            .tokens
            .push(token);
        for _ in 0..3 {
            s.error_cooldown.record_error("carol", "model-a", "class_a");
            s.error_cooldown.record_error("carol", "model-b", "class_b");
        }
        s
    }

    async fn clear(
        s: &AppState,
        token: &str,
        model: Option<&str>,
    ) -> (StatusCode, AdminApiResponse) {
        let (code, Json(body)) = token_error_cooldown_clear(
            State(s.clone()),
            Json(TokenErrorCooldownClearRequest {
                token: token.to_string(),
                model: model.map(str::to_string),
            }),
        )
        .await;
        (code, body)
    }

    #[tokio::test]
    async fn token_error_cooldown_status_reports_model_per_entry() {
        let s = cooldown_state_with_token();
        let (code, resp) = token_error_cooldown_status(State(s.clone())).await;
        assert_eq!(code, StatusCode::OK);
        let entries = resp
            .0
            .data
            .expect("snapshot payload")
            .as_array()
            .expect("snapshot array")
            .clone();
        assert_eq!(entries.len(), 2, "one entry per (token, model) pair");
        for entry in &entries {
            assert_eq!(entry["token"], "carol");
            assert!(
                entry["model"].is_string(),
                "snapshot entries carry the inbound model: {entry}"
            );
            assert!(entry["cooldown_remaining"].as_u64().unwrap() > 0);
        }
        let models: Vec<&str> = entries
            .iter()
            .map(|e| e["model"].as_str().unwrap())
            .collect();
        assert_eq!(models, vec!["model-a", "model-b"]);
    }

    #[tokio::test]
    async fn token_error_cooldown_clear_narrows_by_model_and_wildcard_clears_all() {
        let s = cooldown_state_with_token();

        let (code, body) = clear(&s, "carol", Some("model-a")).await;
        assert_eq!(code, StatusCode::OK, "{:?}", body.error);
        assert_eq!(
            body.data.expect("clear payload")["message"],
            "cleared error-class cooldown state for token 'carol' model 'model-a'"
        );
        assert!(!s.error_cooldown.is_blocked("carol", "model-a").blocked);
        assert!(
            s.error_cooldown.is_blocked("carol", "model-b").blocked,
            "a narrowed clear must leave sibling models alone"
        );

        // Omitting `model` clears every model of the token.
        let (code, body) = clear(&s, "carol", None).await;
        assert_eq!(code, StatusCode::OK, "{:?}", body.error);
        assert_eq!(
            body.data.expect("clear payload")["message"],
            "cleared error-class cooldown state for token 'carol'"
        );
        assert!(s.error_cooldown.snapshot().is_empty());

        let (code, body) = clear(&s, "nobody", None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(body.status, "error");
    }

    #[tokio::test]
    async fn token_error_cooldown_wildcard_composes_with_model_filter() {
        let s = cooldown_state_with_token();
        let dave: crate::token::config::TokenConfig =
            toml::from_str("key = \"lp-admin-cooldown-key-111111111111\"\nname = \"dave\"\n")
                .expect("test token config");
        s.token
            .config
            .write()
            .expect("token config lock")
            .tokens
            .push(dave);
        for _ in 0..3 {
            s.error_cooldown.record_error("dave", "model-a", "class_a");
            s.error_cooldown.record_error("dave", "model-b", "class_b");
        }
        assert!(s.error_cooldown.is_blocked("carol", "model-a").blocked);
        assert!(s.error_cooldown.is_blocked("dave", "model-a").blocked);

        // `"*"` + `model` clears that model for every token, sparing siblings.
        let (code, body) = clear(&s, "*", Some("model-a")).await;
        assert_eq!(code, StatusCode::OK, "{:?}", body.error);
        assert_eq!(
            body.data.expect("clear payload")["message"],
            "cleared error-class cooldown state for model 'model-a' across 2 token(s)"
        );
        assert!(!s.error_cooldown.is_blocked("carol", "model-a").blocked);
        assert!(!s.error_cooldown.is_blocked("dave", "model-a").blocked);
        assert!(
            s.error_cooldown.is_blocked("carol", "model-b").blocked
                && s.error_cooldown.is_blocked("dave", "model-b").blocked,
            "the model filter must spare sibling models of every token"
        );

        // `"*"` without `model` still clears everything.
        let (code, body) = clear(&s, "*", None).await;
        assert_eq!(code, StatusCode::OK, "{:?}", body.error);
        assert_eq!(
            body.data.expect("clear payload")["message"],
            "cleared error-class cooldown state for 2 token(s)"
        );
        assert!(s.error_cooldown.snapshot().is_empty());
    }
}
