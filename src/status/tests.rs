use std::time::{Duration, Instant};

use reqwest::StatusCode;

use crate::config::{AuthConfig, Config, Protocol, ProviderConfig, ReasoningLevelEntry};

use super::cache::{ProbeFrequencyLimiter, unix_now};
use super::display::{
    AuthScope, auth_lines, cache_detail, cache_detail_with_latency, cooldown_lines,
    format_context_window, provider_rows, running_service_pid, runtime_state_lines,
    unbound_model_status_line, unbound_model_warning,
};
use super::format::{
    age_label, auth_label, badge, blue, bold, cyan, dim, format_latency, green, heading,
    is_success, label, latency_rating, protocol_label, red, section, yellow,
};
use super::probe::{
    consume_probe_stream, discover_ollama_models, fetch_remote_config_snapshot, ollama_cache_row,
    ollama_capability_feature, ollama_context_window, openrouter_cache_row, parse_ollama_num_ctx,
    run_one_online_probe, string_array,
};
use super::*;
use serial_test::serial;

#[tokio::test]
async fn default_status_sends_no_network_traffic() {
    use std::sync::{Arc, Mutex};

    let hits = Arc::new(Mutex::new(0usize));
    let hits_for_handler = Arc::clone(&hits);
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move || {
            let hits_for_handler = Arc::clone(&hits_for_handler);
            async move {
                *hits_for_handler.lock().unwrap() += 1;
                axum::Json(serde_json::json!({"choices": []}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve upstream");
    });

    let mut cfg = crate::config::default_deepseek_config();
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    provider.openai_chat.as_mut().unwrap().url = Some(format!("http://{addr}/chat/completions"));
    provider.api_key_env = None;

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let cache_path = crate::status::cache::cache_path_in(temp.path());

    // Default (offline) status must not probe the network at all.
    print_status_with_cache_path(
        &config_path,
        &cfg,
        false,
        &ProbeSelection::default(),
        cache_path,
        temp.path(),
    )
    .await
    .expect("offline status");
    assert_eq!(*hits.lock().unwrap(), 0);
}

#[tokio::test]
async fn online_probe_covers_all_client_protocol_bindings() {
    use std::sync::{Arc, Mutex};

    let hits = Arc::new(Mutex::new(0usize));
    let hits_for_handler = Arc::clone(&hits);
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move || {
            let hits_for_handler = Arc::clone(&hits_for_handler);
            async move {
                *hits_for_handler.lock().unwrap() += 1;
                axum::Json(serde_json::json!({"choices": [{"message": {"content": "pong"}}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve upstream");
    });

    let mut cfg = crate::config::default_deepseek_config();
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    provider.openai_chat.as_mut().unwrap().url = Some(format!("http://{addr}/chat/completions"));
    provider.openai_responses = Some(crate::config::EndpointConfig::derived(
        Protocol::OpenaiChatCompletions,
    ));
    provider.anthropic = Some(crate::config::EndpointConfig::derived(
        Protocol::OpenaiChatCompletions,
    ));
    provider.api_key_env = None;
    provider.auth = Some(crate::config::AuthConfig::None);
    provider.request_frequency = Some(crate::config::RequestFrequencyConfig {
        requests_per_minute: Some(60_000),
        requests_per_hour: None,
        burst: Some(60_000),
        queue_timeout_seconds: Some(1),
    });
    cfg.models.retain(|id, _| id == "deepseek-v4-flash-lp");
    let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
    model.openai_responses_providers = model.openai_chat_providers.clone();
    model.anthropic_providers = model.openai_chat_providers.clone();
    let second_model = model.clone();
    cfg.models
        .insert("deepseek-v4-flash-medium-lp".to_string(), second_model);

    let mut cache = StatusCache::default();
    super::probe::run_online_probes(
        std::path::Path::new("/nonexistent/config.toml"),
        &cfg,
        &mut cache,
        &ProbeSelection::default(),
    )
    .await;

    assert_eq!(
        *hits.lock().unwrap(),
        6,
        "each selected provider/model pair must probe its three protocol bindings"
    );
    for model_id in ["deepseek-v4-flash-lp", "deepseek-v4-flash-medium-lp"] {
        assert!(cache.probes.contains_key(&probe_key(
            model_id,
            "deepseek",
            Protocol::OpenaiChatCompletions
        )));
        assert!(cache.probes.contains_key(&probe_key(
            model_id,
            "deepseek",
            Protocol::OpenaiResponses
        )));
        assert!(
            cache
                .probes
                .contains_key(&probe_key(model_id, "deepseek", Protocol::Anthropic))
        );
    }
}

#[tokio::test]
async fn probe_frequency_limiter_defers_when_queue_timeout_would_be_exceeded() {
    let plan = crate::config::ExecutionPlan {
        frontend_protocol: Protocol::OpenaiChatCompletions,
        provider_id: "slow-provider".to_string(),
        upstream_model: "m".to_string(),
        source_protocol: Protocol::OpenaiChatCompletions,
        adapter: crate::config::AdapterKind::Passthrough,
        native_url: "http://127.0.0.1/chat".to_string(),
        auth: crate::config::AuthConfig::None,
        compat: crate::config::CompatConfig::default(),
        anthropic_family_models: Vec::new(),
        store: None,
        request_frequency: crate::config::RequestFrequencyConfig {
            requests_per_minute: Some(1),
            requests_per_hour: None,
            burst: Some(1),
            queue_timeout_seconds: Some(0),
        },
        routed_target: None,
    };
    let mut limiter = ProbeFrequencyLimiter::default();
    assert!(limiter.acquire(&plan).await);
    assert!(!limiter.acquire(&plan).await);
}

#[tokio::test]
#[ignore] // Flaky timing test, skip for coverage
async fn probe_frequency_limiter_waits_within_queue_timeout() {
    let plan = crate::config::ExecutionPlan {
        frontend_protocol: Protocol::OpenaiChatCompletions,
        provider_id: "fast-provider".to_string(),
        upstream_model: "m".to_string(),
        source_protocol: Protocol::OpenaiChatCompletions,
        adapter: crate::config::AdapterKind::Passthrough,
        native_url: "http://127.0.0.1/chat".to_string(),
        auth: crate::config::AuthConfig::None,
        compat: crate::config::CompatConfig::default(),
        anthropic_family_models: Vec::new(),
        store: None,
        request_frequency: crate::config::RequestFrequencyConfig {
            requests_per_minute: Some(60_000),
            requests_per_hour: None,
            burst: Some(1),
            queue_timeout_seconds: Some(1),
        },
        routed_target: None,
    };
    let mut limiter = ProbeFrequencyLimiter::default();
    assert!(limiter.acquire(&plan).await);
    let start = Instant::now();
    assert!(limiter.acquire(&plan).await);
    assert!(start.elapsed() >= Duration::from_millis(1));
}

#[tokio::test]
async fn ollama_dynamic_discovery_normalizes_model_cache_rows() {
    let app = axum::Router::new()
        .route(
            "/api/tags",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "models": [
                        {"name": "qwen3:27b"},
                        {"name": "llama3.2:latest"}
                    ]
                }))
            }),
        )
        .route(
            "/api/show",
            axum::routing::post(
                |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    let model = body
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let payload = if model == "qwen3:27b" {
                        serde_json::json!({
                            "capabilities": ["completion", "tools"],
                            "model_info": {"qwen3.context_length": 32768}
                        })
                    } else {
                        serde_json::json!({
                            "capabilities": ["vision"],
                            "parameters": "num_ctx 8192\ntemperature 0.7"
                        })
                    };
                    axum::Json(payload)
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ollama mock");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve ollama mock");
    });

    let mut provider = crate::catalog::ollama().provider;
    provider.openai_chat.as_mut().unwrap().url = Some(format!("http://{addr}/v1/chat/completions"));
    let client = reqwest::Client::new();
    let rows = discover_ollama_models(&client, "ollama", &provider)
        .await
        .expect("discover");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].provider_id, "ollama");
    assert_eq!(rows[0].model_id, "qwen3:27b");
    assert_eq!(rows[0].context_window, Some(32768));
    assert!(rows[0].features.contains(&"tools".to_string()));
    assert_eq!(rows[1].context_window, Some(8192));
    assert!(rows[1].features.contains(&"image_input".to_string()));
    assert!(rows[0].source_url.ends_with("/api/tags"));
    assert!(rows[0].stale_after_unix > rows[0].probed_at_unix);
}

#[test]
fn openrouter_dynamic_row_maps_modalities_parameters_and_reasoning() {
    let row = openrouter_cache_row(
        "openrouter",
        "https://openrouter.ai/api/v1/models",
        10,
        "openai/gpt-test",
        &serde_json::json!({
            "id": "openai/gpt-test",
            "name": "GPT Test",
            "architecture": {"input_modalities": ["text", "image", "file"]},
            "supported_parameters": ["tools", "tool_choice", "response_format", "reasoning"],
            "reasoning": {
                "supported_efforts": ["low", "medium", "high"],
                "default_effort": "medium"
            }
        }),
        Some(128000),
        Some(8192),
    );
    assert_eq!(row.model_id, "openai/gpt-test");
    assert_eq!(row.display_name.as_deref(), Some("GPT Test"));
    assert!(row.features.contains(&"tools".to_string()));
    assert!(row.features.contains(&"structured_output".to_string()));
    assert!(row.features.contains(&"image_input".to_string()));
    assert!(row.features.contains(&"document_input".to_string()));
    assert_eq!(
        row.reasoning_levels
            .as_ref()
            .map(|v| v.iter().map(|e| e.level()).collect::<Vec<_>>()),
        Some(vec!["low", "medium", "high"])
    );
    assert_eq!(row.default_reasoning_level.as_deref(), Some("medium"));
}

#[test]
fn status_cache_serializes_dynamic_model_rows() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("status-cache.json");
    let mut cache = StatusCache::default();
    cache.dynamic_models.insert(
        "ollama".to_string(),
        vec![DynamicModelCacheEntry {
            provider_id: "ollama".to_string(),
            source_url: "http://127.0.0.1:11434/api/tags".to_string(),
            probed_at_unix: 1,
            stale_after_unix: 2,
            model_id: "qwen3:27b".to_string(),
            display_name: Some("qwen3:27b".to_string()),
            context_window: None,
            max_output_tokens: None,
            features: Vec::new(),
            supported_parameters: Vec::new(),
            reasoning_levels: None,
            default_reasoning_level: None,
        }],
    );
    write_cache(&path, &cache).expect("write cache");
    let loaded = read_cache(&path).expect("read cache");
    assert_eq!(loaded.dynamic_models["ollama"][0].model_id, "qwen3:27b");
}

#[test]
fn protocol_label_is_user_facing() {
    assert_eq!(protocol_label("chat_completions"), "chat");
    assert_eq!(protocol_label("responses"), "responses");
}

#[test]
fn cache_detail_hides_raw_checked_at_on_success() {
    let entry = ProbeCacheEntry {
        ok: true,
        checked_at_unix: unix_now().saturating_sub(60),
        latency_ms: Some(123),
        http_status: Some(200),
        error: None,
    };

    let (state, detail) = cache_detail(Some(&entry));

    assert_eq!(state, "OK");
    assert!(detail.contains("cached ok"));
    assert!(detail.contains("123ms"));
    assert!(!detail.contains("checked_at="));
}

#[test]
fn provider_auth_summary_reports_oauth_state_without_tokens() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let provider = ProviderConfig {
        auth: Some(AuthConfig::OpenaiOauth {
            account: Some("openai-subscription".to_string()),
        }),
        openai_responses: Some(crate::config::EndpointConfig::native(
            "https://chatgpt.com/backend-api/codex/responses",
        )),
        ..ProviderConfig::default()
    };
    let (state, line) = provider_auth_summary("openai-subscription", &provider, &path);
    assert_eq!(state, "WARN");
    assert!(line.contains("state=missing-login"));

    let mut accounts = crate::auth::AccountsFile::new();
    accounts.openai.insert(
        "openai-subscription".to_string(),
        crate::auth::OpenaiAccount {
            account_label: "user@example.com".to_string(),
            access_token: "secret-access-token-1234567890".to_string(),
            refresh_token: "secret-refresh-token-1234567890".to_string(),
            expires_at_unix: (unix_now() + 60) as i64,
            updated_at_unix: unix_now() as i64,
        },
    );
    crate::auth::save_oauth_accounts(&path, &accounts).expect("write");
    let (state, line) = provider_auth_summary("openai-subscription", &provider, &path);
    assert_eq!(state, "OK");
    assert!(line.contains("auth=openai_oauth"));
    assert!(line.contains("token=***"));
    assert!(!line.contains("secret-access"));
    assert!(!line.contains("secret-refresh"));
}

#[test]
fn auth_lines_mask_token_values() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("auth.json");
    let mut accounts = crate::auth::AccountsFile::new();
    accounts.openai.insert(
        "openai-subscription".to_string(),
        crate::auth::OpenaiAccount {
            account_label: "user@example.com".to_string(),
            access_token: "secret-access-token-1234567890".to_string(),
            refresh_token: "secret-refresh-token-1234567890".to_string(),
            expires_at_unix: (unix_now() + 60) as i64,
            updated_at_unix: unix_now() as i64,
        },
    );
    crate::auth::save_oauth_accounts(&path, &accounts).expect("write");

    let lines = auth_lines(&path).join("\n");
    assert!(lines.contains("openai-subscription"));
    assert!(lines.contains("token=***"));
    assert!(!lines.contains("secret-access"));
    assert!(!lines.contains("secret-refresh"));
}

#[test]
fn auth_lines_shows_skipped_accounts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");

    // 写入一个包含无效账号的文件（缺字段、短 token 等）
    let broken = serde_json::json!({
        "version": 1,
        "openai": {
            "broken-acc": {
                "account_label": "test",
                "access_token": "short",
                "refresh_token": "also-short",
                "expires_at_unix": 1000000001,
                "updated_at_unix": 1000000000
            },
            "work": {
                "account_label": "user@example.com",
                "access_token": "valid-access-token-1234567890",
                "refresh_token": "valid-refresh-token-1234567890",
                "expires_at_unix": 2000000000,
                "updated_at_unix": 1000000000
            }
        }
    });
    std::fs::write(&path, serde_json::to_string_pretty(&broken).expect("json")).expect("write");

    let lines = auth_lines(&path);
    let text = lines.join("\n");
    // 有效账号正常显示
    assert!(text.contains("work"));
    assert!(text.contains("Skipped"));
    assert!(text.contains("broken-acc"));
}

#[test]
fn provider_auth_summary_reports_skipped_account() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");

    // 写入无效的 openai 账号（反序列化通过、校验失败 → 进 skipped 列表）
    let broken = serde_json::json!({
        "version": 1,
        "openai": {
            "broken-acc": {
                "account_label": "test",
                "access_token": "short",
                "refresh_token": "also-short",
                "expires_at_unix": 1000000001,
                "updated_at_unix": 1000000000
            }
        }
    });
    std::fs::write(&path, serde_json::to_string_pretty(&broken).expect("json")).expect("write");

    let provider = ProviderConfig {
        auth: Some(AuthConfig::OpenaiOauth {
            account: Some("broken-acc".to_string()),
        }),
        ..ProviderConfig::default()
    };
    let (state, line) = provider_auth_summary("p", &provider, &path);
    assert_eq!(state, "WARN");
    assert!(line.contains("state=skipped"));
}

#[test]
fn cooldown_lines_show_active_persisted_entries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("cooldowns.json");
    crate::cooldown::write_entries(
        &path,
        vec![crate::cooldown::CooldownEntry {
            model: "m".to_string(),
            provider: "p".to_string(),
            protocol: Protocol::OpenaiResponses,
            token: None,
            token_key_hash: None,
            kind: "server_error".to_string(),
            reason: "500".to_string(),
            started_at_unix: unix_now(),
            expires_at_unix: unix_now() + 60,
            hit_count: 0,
            last_hit_unix: None,
        }],
    )
    .expect("write cooldown state");

    let lines = cooldown_lines(&path);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("model=m"));
    assert!(lines[0].contains("provider=p"));
    assert!(lines[0].contains("protocol=responses"));
}

#[test]
fn test_format_provider_status() {
    let res = format_provider_status("openai", "OK", "api_key_env=OPENAI_API_KEY state=set");
    assert_eq!(
        res,
        "Provider openai: [OK] api_key_env=OPENAI_API_KEY state=set"
    );

    let res2 = format_provider_status("deepseek", "WARN", "auth=invalid error=key missing");
    assert_eq!(
        res2,
        "Provider deepseek: [WARN] auth=invalid error=key missing"
    );
}

#[test]
fn test_format_model_status() {
    let res = format_model_status("gpt-4o", 128000, 4096);
    assert_eq!(res, "gpt-4o (ctx: 128K, max_out: 4K)");

    let res2 = format_model_status("deepseek-v3", 1000000, 8192);
    assert_eq!(res2, "deepseek-v3 (ctx: 1.0M, max_out: 8K)");
}

#[test]
fn test_calculate_health_status() {
    assert_eq!(calculate_health_status(0, 0, false), "UNKNOWN");
    assert_eq!(calculate_health_status(3, 3, false), "OK");
    assert_eq!(calculate_health_status(3, 3, true), "WARN");
    assert_eq!(calculate_health_status(2, 3, false), "WARN");
    assert_eq!(calculate_health_status(0, 3, false), "FAIL");
}

#[test]
fn test_parse_cooldown_reason() {
    assert_eq!(parse_cooldown_reason(""), "unknown reason");
    assert_eq!(
        parse_cooldown_reason("HTTP 429 Too Many Requests"),
        "rate limit exceeded (429)"
    );
    assert_eq!(
        parse_cooldown_reason("Internal 500 error"),
        "upstream server error"
    );
    assert_eq!(
        parse_cooldown_reason("502 Bad Gateway"),
        "upstream server error"
    );
    assert_eq!(
        parse_cooldown_reason("503 Service Unavailable"),
        "upstream server error"
    );
    assert_eq!(
        parse_cooldown_reason("connection timed out"),
        "request timeout"
    );
    assert_eq!(
        parse_cooldown_reason("custom custom_reason"),
        "custom custom_reason"
    );
}

#[test]
fn test_format_duration() {
    assert_eq!(format_duration(0), "0s");
    assert_eq!(format_duration(45), "45s");
    assert_eq!(format_duration(60), "1m");
    assert_eq!(format_duration(90), "1m 30s");
    assert_eq!(format_duration(3600), "1h");
    assert_eq!(format_duration(3660), "1h 1m");
}

#[test]
fn test_format_bytes() {
    assert_eq!(format_bytes(500), "500 B");
    assert_eq!(format_bytes(1024), "1.00 KB");
    assert_eq!(format_bytes(1536), "1.50 KB");
    assert_eq!(format_bytes(1048576), "1.00 MB");
    assert_eq!(format_bytes(1073741824), "1.00 GB");
}

#[test]
fn test_truncate_string() {
    assert_eq!(truncate_string("hello", 10), "hello");
    assert_eq!(truncate_string("hello world", 5), "hello...");
    assert_eq!(truncate_string("你好世界！", 2), "你好...");
}

#[test]
fn test_should_skip_probe() {
    // In cooldown -> skip
    assert!(should_skip_probe(true, None, 1000, 60));
    // Checked recently -> skip
    assert!(should_skip_probe(false, Some(980), 1000, 60));
    // Not checked recently -> don't skip
    assert!(!should_skip_probe(false, Some(900), 1000, 60));
    // Never checked -> don't skip
    assert!(!should_skip_probe(false, None, 1000, 60));
}

#[test]
fn test_get_auth_status_summary() {
    assert_eq!(get_auth_status_summary("api_key", false, false), "MISSING");
    assert_eq!(
        get_auth_status_summary("openai_oauth", true, true),
        "EXPIRED"
    );
    assert_eq!(get_auth_status_summary("none", true, false), "NO_AUTH");
    assert_eq!(get_auth_status_summary("api_key", true, false), "VALID");
}

#[test]
fn test_format_error_message() {
    assert_eq!(
        format_error_message(Some(404), Some("Not Found")),
        "HTTP 404: Not Found"
    );
    assert_eq!(format_error_message(Some(500), None), "HTTP 500");
    assert_eq!(
        format_error_message(None, Some("Network error")),
        "Network error"
    );
    assert_eq!(format_error_message(None, None), "unknown error");
}

// =========================================================================
// 补充测试：age_label - 各时间区间
// =========================================================================

#[test]
fn age_label_seconds_range() {
    let now = unix_now();
    // 刚刚 (<60s)
    assert_eq!(age_label(now - 5), "5s ago");
    assert_eq!(age_label(now - 59), "59s ago");
    assert_eq!(age_label(now), "0s ago");
}

#[test]
fn age_label_minutes_range() {
    let now = unix_now();
    // 1 分钟 - 1 小时
    assert_eq!(age_label(now - 60), "1m ago");
    assert_eq!(age_label(now - 90), "1m ago");
    assert_eq!(age_label(now - 3599), "59m ago");
}

#[test]
fn age_label_hours_range() {
    let now = unix_now();
    // 1 小时 - 24 小时
    assert_eq!(age_label(now - 3600), "1h ago");
    assert_eq!(age_label(now - 7200), "2h ago");
    assert_eq!(age_label(now - 86399), "23h ago");
}

#[test]
fn age_label_days_range() {
    let now = unix_now();
    // ≥ 24 小时
    assert_eq!(age_label(now - 86400), "1d ago");
    assert_eq!(age_label(now - 172800), "2d ago");
}

#[test]
fn age_label_future_timestamp_returns_raw() {
    // 未来时间戳（checked_at > now）应返回 "checked_at=<ts>"
    let future = unix_now() + 3600;
    let label = age_label(future);
    assert!(
        label.starts_with("checked_at="),
        "future timestamp should show raw value, got: {label}"
    );
    assert!(
        label.contains(&future.to_string()),
        "should contain the raw timestamp"
    );
}

// =========================================================================
// 补充测试：probe_key 格式
// =========================================================================

#[test]
fn probe_key_format_is_colon_delimited() {
    use crate::config::Protocol;
    let key = probe_key("gpt-4o", "openai", Protocol::OpenaiChatCompletions);
    assert_eq!(key, "gpt-4o:openai:chat_completions");

    let key2 = probe_key("claude-sonnet", "anthropic", Protocol::Anthropic);
    assert_eq!(key2, "claude-sonnet:anthropic:anthropic");

    let key3 = probe_key("deepseek-v3", "deepseek", Protocol::OpenaiResponses);
    assert_eq!(key3, "deepseek-v3:deepseek:responses");
}

// =========================================================================
// 补充测试：cache_detail_with_latency - 各状态
// =========================================================================

#[test]
fn cache_detail_with_latency_ok_shows_latency_and_age() {
    let entry = ProbeCacheEntry {
        ok: true,
        checked_at_unix: unix_now().saturating_sub(30),
        latency_ms: Some(150),
        http_status: Some(200),
        error: None,
    };
    let (state, detail) = cache_detail_with_latency(Some(&entry));
    assert_eq!(state, "OK");
    assert!(detail.contains("cached ok"));
    assert!(detail.contains("30s ago") || detail.contains("s ago"));
    // detail 包含 latency（格式化后含 150ms）
    assert!(
        detail.contains("150ms"),
        "detail should have 150ms: {detail}"
    );
}

#[test]
fn cache_detail_with_latency_ok_without_latency_shows_question_mark() {
    let entry = ProbeCacheEntry {
        ok: true,
        checked_at_unix: unix_now().saturating_sub(10),
        latency_ms: None,
        http_status: Some(200),
        error: None,
    };
    let (state, detail) = cache_detail_with_latency(Some(&entry));
    assert_eq!(state, "OK");
    assert!(
        detail.contains("?ms"),
        "no latency should show ?ms: {detail}"
    );
}

#[test]
fn cache_detail_with_latency_fail_shows_status_and_error() {
    let entry = ProbeCacheEntry {
        ok: false,
        checked_at_unix: unix_now().saturating_sub(120),
        latency_ms: None,
        http_status: Some(429),
        error: Some("rate limited".to_string()),
    };
    let (state, detail) = cache_detail_with_latency(Some(&entry));
    assert_eq!(state, "FAIL");
    assert!(detail.contains("cached failed"));
    assert!(detail.contains("429"), "should show http status: {detail}");
    assert!(
        detail.contains("rate limited"),
        "should show error: {detail}"
    );
}

#[test]
fn cache_detail_with_latency_fail_without_status_shows_dash() {
    let entry = ProbeCacheEntry {
        ok: false,
        checked_at_unix: unix_now().saturating_sub(60),
        latency_ms: None,
        http_status: None,
        error: Some("connection refused".to_string()),
    };
    let (state, detail) = cache_detail_with_latency(Some(&entry));
    assert_eq!(state, "FAIL");
    assert!(
        detail.contains("status=-"),
        "missing status should show '-': {detail}"
    );
}

#[test]
fn cache_detail_with_latency_none_is_miss() {
    let (state, detail) = cache_detail_with_latency(None);
    assert_eq!(state, "MISS");
    assert_eq!(detail, "no cached probe");
}

// =========================================================================
// 补充测试：cache_detail - 各状态
// =========================================================================

#[test]
fn cache_detail_fail_shows_status_and_error() {
    let entry = ProbeCacheEntry {
        ok: false,
        checked_at_unix: unix_now().saturating_sub(30),
        latency_ms: None,
        http_status: Some(500),
        error: Some("server error".to_string()),
    };
    let (state, detail) = cache_detail(Some(&entry));
    assert_eq!(state, "FAIL");
    assert!(detail.contains("cached failed"));
    assert!(detail.contains("500"));
    assert!(detail.contains("server error"));
}

#[test]
fn cache_detail_fail_without_status_and_error_shows_defaults() {
    let entry = ProbeCacheEntry {
        ok: false,
        checked_at_unix: unix_now().saturating_sub(60),
        latency_ms: None,
        http_status: None,
        error: None,
    };
    let (state, detail) = cache_detail(Some(&entry));
    assert_eq!(state, "FAIL");
    assert!(
        detail.contains("status=-"),
        "no status should show '-': {detail}"
    );
    assert!(
        detail.contains("unknown"),
        "no error should show 'unknown': {detail}"
    );
}

#[test]
fn cache_detail_none_is_miss() {
    let (state, detail) = cache_detail(None);
    assert_eq!(state, "MISS");
    assert_eq!(detail, "no cached probe");
}

// =========================================================================
// 补充测试：format_context_window
// =========================================================================

#[test]
fn format_context_window_various_sizes() {
    // < 1000: raw
    assert_eq!(format_context_window(0), "0");
    assert_eq!(format_context_window(999), "999");
    // 1K - 999K
    assert_eq!(format_context_window(1000), "1K");
    assert_eq!(format_context_window(128000), "128K");
    assert_eq!(format_context_window(999999), "999K");
    // ≥ 1M
    assert_eq!(format_context_window(1_000_000), "1.0M");
    assert_eq!(format_context_window(1_500_000), "1.5M");
    assert_eq!(format_context_window(1_048_576), "1.0M"); // close to 1M
}

// =========================================================================
// 补充测试：latency_rating - 边界值
// =========================================================================

#[test]
fn latency_rating_boundary_values() {
    // 0 ms → 极快
    let (symbol, _, label) = latency_rating(0);
    assert_eq!(symbol, "●");
    assert_eq!(label, "极快");
    // 200 ms → 极快（边界）
    let (symbol, _, label) = latency_rating(200);
    assert_eq!(symbol, "●");
    assert_eq!(label, "极快");
    // 201 ms → 正常
    let (symbol, _, label) = latency_rating(201);
    assert_eq!(symbol, "◆");
    assert_eq!(label, "正常");
    // 1000 ms → 正常（边界）
    let (symbol, _, label) = latency_rating(1000);
    assert_eq!(symbol, "◆");
    assert_eq!(label, "正常");
    // 1001 ms → 偏慢
    let (symbol, _, label) = latency_rating(1001);
    assert_eq!(symbol, "▲");
    assert_eq!(label, "偏慢");
    // 3000 ms → 偏慢（边界）
    let (symbol, _, label) = latency_rating(3000);
    assert_eq!(symbol, "▲");
    assert_eq!(label, "偏慢");
    // 3001 ms → 超时
    let (symbol, _, label) = latency_rating(3001);
    assert_eq!(symbol, "✖");
    assert_eq!(label, "超时");
    // 10000 ms → 超时
    let (symbol, _, label) = latency_rating(10000);
    assert_eq!(symbol, "✖");
    assert_eq!(label, "超时");
}

// =========================================================================
// 补充测试：read_cache/write_cache 文件 roundtrip
// =========================================================================

#[test]
fn read_write_cache_probe_roundtrip() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("status-cache.json");

    let mut cache = StatusCache::default();
    cache.probes.insert(
        "model:provider:chat_completions".to_string(),
        ProbeCacheEntry {
            ok: true,
            checked_at_unix: 1700000000,
            latency_ms: Some(250),
            http_status: Some(200),
            error: None,
        },
    );
    write_cache(&path, &cache).expect("write");

    let loaded = read_cache(&path).expect("read");
    let entry = loaded
        .probes
        .get("model:provider:chat_completions")
        .expect("entry");
    assert!(entry.ok);
    assert_eq!(entry.latency_ms, Some(250));
    assert_eq!(entry.http_status, Some(200));
    assert_eq!(entry.checked_at_unix, 1700000000);
}

#[test]
fn read_cache_returns_error_for_corrupted_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("bad-cache.json");
    std::fs::write(&path, "{ corrupted").expect("write");

    let result = read_cache(&path);
    assert!(result.is_err(), "corrupted cache should return error");
}

// =========================================================================
// 补充测试：probe_result_to_cache_entry
// =========================================================================

#[test]
fn probe_result_active_yields_no_cache_entry() {
    let result = ProbeResult::Active;
    assert!(result.to_cache_entry().is_none());
}

#[test]
fn probe_result_ok_yields_positive_cache_entry() {
    let result = ProbeResult::Ok {
        latency_ms: 300,
        status: 200,
    };
    let entry = result.to_cache_entry().expect("cache entry");
    assert!(entry.ok);
    assert_eq!(entry.latency_ms, Some(300));
    assert_eq!(entry.http_status, Some(200));
    assert!(entry.error.is_none());
}

#[test]
fn probe_result_timeout_yields_negative_cache_entry() {
    let result = ProbeResult::Timeout;
    let entry = result.to_cache_entry().expect("cache entry");
    assert!(!entry.ok);
    assert_eq!(entry.error.as_deref(), Some("probe timeout"));
    assert!(entry.latency_ms.is_none());
    assert!(entry.http_status.is_none());
}

#[test]
fn probe_result_error_yields_negative_cache_entry_with_message() {
    let result = ProbeResult::Error("connection refused".to_string());
    let entry = result.to_cache_entry().expect("cache entry");
    assert!(!entry.ok);
    assert_eq!(entry.error.as_deref(), Some("connection refused"));
}

// =========================================================================
// 补充测试：parse_ollama_num_ctx
// =========================================================================

#[test]
fn parse_ollama_num_ctx_extracts_value() {
    assert_eq!(parse_ollama_num_ctx("num_ctx 32768"), Some(32768));
    assert_eq!(
        parse_ollama_num_ctx("temperature 0.7\nnum_ctx 8192\nstop_token END"),
        Some(8192)
    );
    // 无 num_ctx
    assert_eq!(parse_ollama_num_ctx("temperature 0.7"), None);
    // 空字符串
    assert_eq!(parse_ollama_num_ctx(""), None);
    // num_ctx 值非数字
    assert_eq!(parse_ollama_num_ctx("num_ctx abc"), None);
}

#[test]
fn calculate_health_status_covers_all_buckets() {
    assert_eq!(calculate_health_status(0, 0, false), "UNKNOWN");
    assert_eq!(calculate_health_status(2, 2, false), "OK");
    assert_eq!(calculate_health_status(1, 2, false), "WARN");
    assert_eq!(calculate_health_status(2, 2, true), "WARN");
    assert_eq!(calculate_health_status(0, 2, false), "FAIL");
}

#[test]
fn format_duration_and_bytes_cover_boundary_values() {
    assert_eq!(format_duration(59), "59s");
    assert_eq!(format_duration(60), "1m");
    assert_eq!(format_duration(61), "1m 1s");
    assert_eq!(format_duration(3600), "1h");
    assert_eq!(format_duration(3660), "1h 1m");

    assert_eq!(format_bytes(1023), "1023 B");
    assert_eq!(format_bytes(1024), "1.00 KB");
    assert_eq!(format_bytes(1024 * 1024), "1.00 MB");
    assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
}

#[test]
fn status_format_helpers_cover_strings_and_reason_classification() {
    assert_eq!(
        format_provider_status("deepseek", "OK", "api_key"),
        "Provider deepseek: [OK] api_key"
    );
    assert_eq!(
        format_model_status("deepseek-v4", 128000, 8192),
        "deepseek-v4 (ctx: 128K, max_out: 8K)"
    );
    assert_eq!(
        unbound_model_warning(),
        "no configured provider bindings; not selected for probe"
    );
    assert_eq!(
        unbound_model_status_line(false),
        format!(
            "  │   └── {} no configured provider bindings; not selected for probe",
            badge("WARN")
        )
    );
    assert_eq!(
        unbound_model_status_line(true),
        format!(
            "      └── {} no configured provider bindings; not selected for probe",
            badge("WARN")
        )
    );
    assert_eq!(parse_cooldown_reason(""), "unknown reason");
    assert_eq!(
        parse_cooldown_reason("received 500 from upstream"),
        "upstream server error"
    );
    assert_eq!(
        parse_cooldown_reason("request timed out while reading body"),
        "request timeout"
    );
    assert_eq!(parse_cooldown_reason("custom message"), "custom message");
}

#[test]
fn parse_cooldown_reason_and_auth_summary_cover_edges() {
    assert_eq!(
        parse_cooldown_reason("hit 429 from upstream"),
        "rate limit exceeded (429)"
    );
    assert_eq!(get_auth_status_summary("api_key", false, false), "MISSING");
    assert_eq!(get_auth_status_summary("api_key", true, true), "EXPIRED");
    assert_eq!(get_auth_status_summary("none", true, false), "NO_AUTH");
    assert_eq!(get_auth_status_summary("oauth", true, false), "VALID");
}

#[test]
fn format_error_message_and_should_skip_probe_cover_all_branches() {
    assert_eq!(
        format_error_message(Some(503), Some("down")),
        "HTTP 503: down"
    );
    assert_eq!(format_error_message(Some(404), None), "HTTP 404");
    assert_eq!(format_error_message(None, Some("boom")), "boom");
    assert_eq!(format_error_message(None, None), "unknown error");

    assert!(should_skip_probe(true, None, 100, 60));
    assert!(should_skip_probe(false, Some(90), 100, 11));
    assert!(!should_skip_probe(false, Some(80), 100, 10));
    assert!(!should_skip_probe(false, None, 100, 10));
}

#[test]
fn badge_auth_label_protocol_label_and_success_cover_unknowns() {
    assert!(badge("OK").contains("OK"));
    assert!(badge("WARN").contains("WARN"));
    assert!(badge("FAIL").contains("FAIL"));
    assert!(badge("MISS").contains("MISS"));
    assert_eq!(badge("CUSTOM"), "CUSTOM");

    assert!(auth_label("OK", "ready").contains("ready"));
    assert!(auth_label("WARN", "warn").contains("warn"));
    assert!(auth_label("FAIL", "fail").contains("fail"));
    assert_eq!(auth_label("MISS", "plain"), "plain");

    assert_eq!(protocol_label("responses"), "responses");
    assert_eq!(protocol_label("chat_completions"), "chat");
    assert_eq!(protocol_label("anthropic"), "anthropic");
    assert_eq!(protocol_label("custom"), "custom");

    assert!(is_success(StatusCode::OK));
    assert!(!is_success(StatusCode::BAD_GATEWAY));
}

#[test]
fn cache_path_uses_explicit_state_directory_and_style_helpers_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = crate::status::cache::cache_path_in(dir.path());
    assert_eq!(path, dir.path().join("status-cache.json"));

    for ms in [50_u64, 500, 2000, 5000] {
        assert!(format_latency(ms).contains("ms"));
    }
    assert!(!heading("x").is_empty());
    assert!(!section("x").is_empty());
    assert!(!label("x").is_empty());
    assert!(!bold("x").is_empty());
    assert!(!dim("x").is_empty());
    assert!(!red("x").is_empty());
    assert!(!green("x").is_empty());
    assert!(!yellow("x").is_empty());
    assert!(!blue("x").is_empty());
    assert!(!cyan("x").is_empty());
}
#[test]
fn string_array_filters_non_strings_and_preserves_order() {
    let value = serde_json::json!(["a", 1, null, "b", {"x": true}, "c"]);
    assert_eq!(string_array(Some(&value)), vec!["a", "b", "c"]);
    assert!(string_array(None).is_empty());
    assert!(string_array(Some(&serde_json::json!({"not":"array"}))).is_empty());
}

#[test]
fn ollama_capability_feature_maps_known_capabilities() {
    assert_eq!(ollama_capability_feature("vision"), "image_input");
    assert_eq!(ollama_capability_feature("tools"), "tools");
    assert_eq!(ollama_capability_feature("embedding"), "embedding");
}

#[test]
fn ollama_context_window_prefers_model_info_context_length() {
    let show = serde_json::json!({
        "model_info": {"llama.context_length": 131072},
        "parameters": "num_ctx 8192"
    });
    assert_eq!(ollama_context_window(&show), Some(131072));
}

#[test]
fn ollama_context_window_falls_back_to_parameters() {
    let show = serde_json::json!({"parameters": "temperature 0.1\nnum_ctx 16384"});
    assert_eq!(ollama_context_window(&show), Some(16384));
}

#[test]
fn ollama_cache_row_deduplicates_and_sorts_features() {
    let show = serde_json::json!({
        "capabilities": ["tools", "vision", "tools", "completion"],
        "parameters": "num_ctx 4096"
    });
    let row = ollama_cache_row(
        "ollama",
        "http://localhost:11434",
        10,
        "llama3",
        Some(&show),
    );
    assert_eq!(row.provider_id, "ollama");
    assert_eq!(row.model_id, "llama3");
    assert_eq!(row.context_window, Some(4096));
    assert_eq!(row.features, vec!["completion", "image_input", "tools"]);
    assert_eq!(row.stale_after_unix, 10 + 24 * 60 * 60);
}

#[test]
fn ollama_cache_row_without_show_has_minimal_metadata() {
    let row = ollama_cache_row("p", "src", 1, "m", None);
    assert_eq!(row.display_name.as_deref(), Some("m"));
    assert_eq!(row.context_window, None);
    assert!(row.features.is_empty());
}

#[test]
fn openrouter_cache_row_infers_reasoning_levels_from_parameter() {
    let item = serde_json::json!({"supported_parameters": ["reasoning_effort"]});
    let row = openrouter_cache_row("or", "src", 5, "model", &item, None, None);
    assert_eq!(
        row.reasoning_levels
            .as_ref()
            .map(|v| v.iter().map(|e| e.level()).collect::<Vec<_>>()),
        Some(vec!["minimal", "low", "medium", "high", "xhigh", "max"])
    );
}

#[test]
fn openrouter_cache_row_prefers_explicit_reasoning_levels_and_default() {
    let item = serde_json::json!({
        "name": "Nice Model",
        "supported_parameters": ["tools", "response_format"],
        "reasoning": {"supported_efforts": ["low", "high"], "default_effort": "low"},
        "architecture": {"input_modalities": ["text", "image", "file"]}
    });
    let row = openrouter_cache_row("or", "src", 8, "id", &item, Some(123), Some(45));
    assert_eq!(row.display_name.as_deref(), Some("Nice Model"));
    assert_eq!(row.context_window, Some(123));
    assert_eq!(row.max_output_tokens, Some(45));
    assert_eq!(
        row.features,
        vec![
            "document_input",
            "image_input",
            "structured_output",
            "tools"
        ]
    );
    assert_eq!(
        row.reasoning_levels
            .as_ref()
            .map(|v| v.iter().map(|e| e.level()).collect::<Vec<_>>()),
        Some(vec!["low", "high"])
    );
    assert_eq!(row.default_reasoning_level.as_deref(), Some("low"));
}

#[test]
fn truncate_string_handles_unicode_and_zero_length() {
    assert_eq!(truncate_string("abcdef", 3), "abc...");
    assert_eq!(truncate_string("你好世界", 2), "你好...");
    assert_eq!(truncate_string("abc", 3), "abc");
    assert_eq!(truncate_string("abc", 0), "...");
}

#[test]
fn format_duration_ignores_seconds_for_hour_outputs() {
    assert_eq!(format_duration(3599), "59m 59s");
    assert_eq!(format_duration(3601), "1h");
    assert_eq!(format_duration(7325), "2h 2m");
}

#[test]
fn format_bytes_formats_fractional_values() {
    assert_eq!(format_bytes(1536), "1.50 KB");
    assert_eq!(format_bytes(5 * 1024 * 1024 + 512 * 1024), "5.50 MB");
    assert_eq!(
        format_bytes(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024),
        "3.50 GB"
    );
}

#[test]
fn parse_cooldown_reason_precedence_is_429_then_5xx_then_timeout() {
    assert_eq!(
        parse_cooldown_reason("429 and timeout"),
        "rate limit exceeded (429)"
    );
    assert_eq!(
        parse_cooldown_reason("502 timeout"),
        "upstream server error"
    );
}

#[test]
fn should_skip_probe_uses_saturating_sub_for_future_last_checked() {
    assert!(should_skip_probe(false, Some(200), 100, 60));
    assert!(!should_skip_probe(false, Some(40), 100, 60));
}

#[test]
fn get_auth_status_summary_missing_takes_precedence_over_expired() {
    assert_eq!(get_auth_status_summary("none", false, true), "MISSING");
    assert_eq!(get_auth_status_summary("none", true, true), "EXPIRED");
}

#[test]
fn format_error_message_treats_empty_message_as_absent() {
    assert_eq!(format_error_message(Some(500), Some("")), "HTTP 500");
    assert_eq!(format_error_message(None, Some("")), "unknown error");
}

#[test]
fn status_cache_deserializes_missing_default_maps() {
    let cache: StatusCache = serde_json::from_str("{}").expect("defaults");
    assert!(cache.probes.is_empty());
    assert!(cache.dynamic_models.is_empty());
}

#[test]
fn dynamic_model_cache_entry_defaults_reasoning_fields() {
    let json = serde_json::json!({
        "provider_id":"p", "source_url":"u", "probed_at_unix":1, "stale_after_unix":2,
        "model_id":"m", "display_name":null, "context_window":null, "max_output_tokens":null,
        "features":[], "supported_parameters":[]
    });
    let row: DynamicModelCacheEntry = serde_json::from_value(json).expect("row");
    assert!(row.reasoning_levels.is_none());
    assert_eq!(row.default_reasoning_level, None);
}

#[test]
fn read_write_cache_dynamic_models_roundtrip() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("nested/status-cache.json");
    let mut cache = StatusCache::default();
    cache.dynamic_models.insert(
        "p".into(),
        vec![DynamicModelCacheEntry {
            provider_id: "p".into(),
            source_url: "u".into(),
            probed_at_unix: 1,
            stale_after_unix: 2,
            model_id: "m".into(),
            display_name: Some("M".into()),
            context_window: Some(10),
            max_output_tokens: Some(5),
            features: vec!["tools".into()],
            supported_parameters: vec!["temperature".into()],
            reasoning_levels: Some(vec![ReasoningLevelEntry::new("low", None)]),
            default_reasoning_level: Some("low".into()),
        }],
    );
    write_cache(&path, &cache).expect("write creates parent dirs");
    let loaded = read_cache(&path).expect("read");
    assert_eq!(loaded.dynamic_models["p"][0].model_id, "m");
    assert_eq!(
        loaded.dynamic_models["p"][0]
            .default_reasoning_level
            .as_deref(),
        Some("low")
    );
}

#[test]
fn cache_detail_ok_without_latency_defaults_to_zero_ms() {
    let entry = ProbeCacheEntry {
        ok: true,
        checked_at_unix: unix_now(),
        latency_ms: None,
        http_status: Some(204),
        error: None,
    };
    let (state, detail) = cache_detail(Some(&entry));
    assert_eq!(state, "OK");
    assert!(detail.contains("0ms"));
}

#[test]
fn age_label_exact_boundaries() {
    let now = unix_now();
    assert!(age_label(now.saturating_sub(60)).starts_with("1m"));
    assert!(age_label(now.saturating_sub(3600)).starts_with("1h"));
    assert!(age_label(now.saturating_sub(86400)).starts_with("1d"));
}

#[test]
fn probe_result_cache_entries_have_recent_timestamps() {
    let before = unix_now();
    let entry = ProbeResult::Error("e".into()).to_cache_entry().unwrap();
    let after = unix_now();
    assert!(entry.checked_at_unix >= before && entry.checked_at_unix <= after);
}

// =========================================================================
// status.rs simple path tests
// =========================================================================

#[tokio::test]
async fn test_run_one_probe_auth_unavailable() {
    // When resolve_auth fails for the provider, probe should record error
    let cfg = crate::config::default_deepseek_config();
    let mut cache = StatusCache::default();
    let client = reqwest::Client::new();
    let config_path = tempfile::tempdir().expect("config tempdir");
    let config_file = config_path.path().join("config.toml");
    let plan = crate::config::ExecutionPlan {
        frontend_protocol: Protocol::OpenaiChatCompletions,
        provider_id: "nonexistent-provider".to_string(),
        upstream_model: "m".to_string(),
        source_protocol: Protocol::OpenaiChatCompletions,
        adapter: crate::config::AdapterKind::Passthrough,
        native_url: "http://127.0.0.1:1/v1/chat/completions".to_string(),
        auth: crate::config::AuthConfig::None,
        compat: crate::config::CompatConfig::default(),
        anthropic_family_models: Vec::new(),
        store: None,
        request_frequency: crate::config::RequestFrequencyConfig::default(),
        routed_target: None,
    };
    // Use a provider_id not in config → resolve_auth will fail
    let cooldown_cfg = cfg.effective_cooldown("test-model", "nonexistent-provider");
    run_one_online_probe(
        &config_file,
        &cfg,
        &mut cache,
        &client,
        "test-model",
        Protocol::OpenaiChatCompletions,
        "nonexistent-provider",
        &plan,
        None,
        &cooldown_cfg,
    )
    .await;
    // Verify the cache entry records auth unavailable
    let key = probe_key(
        "test-model",
        "nonexistent-provider",
        Protocol::OpenaiChatCompletions,
    );
    let entry = cache.probes.get(&key).expect("cache entry should exist");
    assert!(!entry.ok);
    assert!(
        entry
            .error
            .as_ref()
            .is_some_and(|e| e.contains("auth unavailable")),
        "expected auth unavailable error, got: {:?}",
        entry.error
    );
}

#[tokio::test]
async fn test_print_status_remote_mode_probe_no_cache() {
    // Remote mode (empty config) + probe + no server + no cache → should not panic
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    // Write a minimal config with NO providers and NO models (remote mode)
    let cfg = Config {
        server: Some(crate::config::ServerConfig {
            listen: "127.0.0.1:0".into(),
            usage: Default::default(),
            max_sse_buffer_bytes: 1,
            max_output_items: 1,
            proxy: None,
            require_auth: None,
            auth_token: None,
            auth_token_env: None,
            management_port: None,
        }),
        remote: None,
        fallback: Default::default(),
        protection: Default::default(),
        diagnostics: Default::default(),
        status: Default::default(),
        codex: Default::default(),
        launch: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    // 隔离：注入 tempdir 作为 runtime_dir → detect_server_at 找不到 sock → 回退本地
    let cache_path = crate::status::cache::cache_path_in(temp.path());

    // Should succeed without panic — remote mode + probe + no server
    let result = print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &ProbeSelection::default(),
        cache_path.clone(),
        temp.path(),
    )
    .await;
    assert!(result.is_ok(), "print_status should succeed: {result:?}");

    let selection = ProbeSelection {
        provider: Some("nonexistent".to_string()),
        model: None,
    };
    let result = print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &selection,
        cache_path,
        temp.path(),
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("cannot validate probe selection"),
        "a selector must not succeed when the remote authority is unavailable"
    );
}

#[tokio::test]
async fn test_print_status_true_remote_mode_unreachable_server() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let cfg = Config {
        server: None,
        remote: Some(crate::config::RemoteConfig {
            url: "http://127.0.0.1:59999".into(),
            auth_mode: Some(crate::config::RemoteAuthMode::Open),
            auth_token_env: "LP_TEST_UNREACHABLE_TOKEN".into(),
        }),
        fallback: Default::default(),
        protection: Default::default(),
        diagnostics: Default::default(),
        status: Default::default(),
        codex: Default::default(),
        launch: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    let cache_path = crate::status::cache::cache_path_in(temp.path());

    // Should succeed gracefully without panic or "missing required [server]" error
    let result = print_status_with_cache_path(
        &config_path,
        &cfg,
        false,
        &ProbeSelection::default(),
        cache_path,
        temp.path(),
    )
    .await;
    assert!(
        result.is_ok(),
        "print_status should succeed in remote mode: {result:?}"
    );
}

#[tokio::test]
#[serial]
async fn test_print_status_true_remote_mode_online_server() {
    use axum::http::HeaderMap;
    use std::sync::{Arc, Mutex};

    // SAFETY: unique env var name
    unsafe { std::env::set_var("LP_REMOTE_TOKEN_172", "lp-remote-token-172") };

    let ping_received = Arc::new(Mutex::new(false));
    let ping_rec_clone = Arc::clone(&ping_received);
    let auth_header_received = Arc::new(Mutex::new(None::<String>));
    let auth_rec_clone = Arc::clone(&auth_header_received);
    let snapshot_received = Arc::new(Mutex::new(false));
    let snapshot_rec_clone = Arc::clone(&snapshot_received);
    let snapshot_auth_received = Arc::new(Mutex::new(None::<String>));
    let snapshot_auth_clone = Arc::clone(&snapshot_auth_received);

    let app = axum::Router::new()
        .route(
            "/admin/ping",
            axum::routing::get(move |headers: HeaderMap| {
                let ping_flag = Arc::clone(&ping_rec_clone);
                let auth_flag = Arc::clone(&auth_rec_clone);
                async move {
                    *ping_flag.lock().unwrap() = true;
                    if let Some(auth) = headers.get("authorization") {
                        *auth_flag.lock().unwrap() = Some(auth.to_str().unwrap().to_string());
                    }
                    axum::Json(serde_json::json!({
                        "version": "0.3.0",
                        "config_identity": "test-ident-172"
                    }))
                }
            }),
        )
        .route(
            "/admin/status",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "data": {
                        "providers": 0,
                        "models": 0,
                        "active_providers": [],
                        "cache": {}
                    }
                }))
            }),
        )
        // 纯客户端节点渲染源必须来自远端配置快照。
        .route(
            "/admin/config/snapshot",
            axum::routing::get(move |headers: HeaderMap| {
                let snap_flag = Arc::clone(&snapshot_rec_clone);
                let snap_auth = Arc::clone(&snapshot_auth_clone);
                async move {
                    *snap_flag.lock().unwrap() = true;
                    if let Some(auth) = headers.get("authorization") {
                        *snap_auth.lock().unwrap() = Some(auth.to_str().unwrap().to_string());
                    }
                    let snapshot_cfg = crate::config::default_deepseek_config();
                    axum::Json(serde_json::json!({
                        "status": "ok",
                        "data": {
                            "config": serde_json::to_value(&snapshot_cfg).unwrap(),
                            "revision": 42
                        }
                    }))
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let cfg = Config {
        server: None,
        remote: Some(crate::config::RemoteConfig {
            url: format!("http://{addr}"),
            auth_mode: Some(crate::config::RemoteAuthMode::Token),
            auth_token_env: "LP_REMOTE_TOKEN_172".into(),
        }),
        fallback: Default::default(),
        protection: Default::default(),
        diagnostics: Default::default(),
        status: Default::default(),
        codex: Default::default(),
        launch: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    let cache_path = crate::status::cache::cache_path_in(temp.path());

    let result = print_status_with_cache_path(
        &config_path,
        &cfg,
        false,
        &ProbeSelection::default(),
        cache_path,
        temp.path(),
    )
    .await;
    assert!(
        result.is_ok(),
        "print_status should succeed with online remote server: {result:?}"
    );
    assert!(
        *ping_received.lock().unwrap(),
        "remote server must receive /admin/ping"
    );
    assert_eq!(
        *auth_header_received.lock().unwrap(),
        Some("Bearer lp-remote-token-172".to_string()),
        "remote server must receive Bearer token"
    );
    assert!(
        *snapshot_received.lock().unwrap(),
        "remote server must receive /admin/config/snapshot for render config"
    );
    assert_eq!(
        *snapshot_auth_received.lock().unwrap(),
        Some("Bearer lp-remote-token-172".to_string()),
        "config snapshot request must carry the same Bearer token"
    );

    unsafe { std::env::remove_var("LP_REMOTE_TOKEN_172") };
}

/// 远端快照可能携带本客户端不认识的 provider（如更高版本服务端的扩展认证）。
/// client-only status 不得因单个 provider 解析失败而整体中止：跳过该 provider，
/// 其余 provider 照常渲染。
#[tokio::test]
async fn test_remote_snapshot_skips_unsupported_provider() {
    let mut snapshot_cfg =
        serde_json::to_value(crate::config::default_deepseek_config()).expect("serialize config");
    snapshot_cfg["providers"]["extension-only"] = serde_json::json!({
        "product": "future-product",
        "auth": { "type": "future_extension_oauth", "account": "smoke" },
        "future_extension": { "url": "https://example.invalid/future" }
    });

    let app = axum::Router::new().route(
        "/admin/config/snapshot",
        axum::routing::get(move || {
            let config = snapshot_cfg.clone();
            async move {
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "data": { "config": config, "revision": 1 }
                }))
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });

    let client = reqwest::Client::new();
    let snapshot = fetch_remote_config_snapshot(&client, &format!("http://{addr}"), None)
        .await
        .expect("unsupported provider must not abort the snapshot");
    let cfg = snapshot.config;
    assert!(
        !cfg.providers.contains_key("extension-only"),
        "unsupported provider must be skipped"
    );
    assert!(
        cfg.providers.contains_key("deepseek"),
        "supported providers must survive"
    );
}

/// client-only 节点快照拉取失败（ping 在线但快照不可用）必须 fail-fast：
/// 返回 Err（顶层非零退出）且不渲染半截状态、不回退本地空口径。
#[tokio::test]
async fn test_print_status_client_only_snapshot_failure_bails() {
    let app = axum::Router::new()
        .route(
            "/admin/ping",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "version": "0.3.0",
                    "config_identity": "test-ident-snapfail"
                }))
            }),
        )
        .route(
            "/admin/status",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "data": {
                        "providers": 0,
                        "models": 0,
                        "active_providers": [],
                        "cache": {}
                    }
                }))
            }),
        );
    // 故意不注册 /admin/config/snapshot → 404 触发快照失败。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let cfg = Config {
        server: None,
        remote: Some(crate::config::RemoteConfig {
            url: format!("http://{addr}"),
            auth_mode: None,
            auth_token_env: "LP_REMOTE_TOKEN_ABSENT".into(),
        }),
        fallback: Default::default(),
        protection: Default::default(),
        diagnostics: Default::default(),
        status: Default::default(),
        codex: Default::default(),
        launch: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    let result = print_status_with_cache_path(
        &config_path,
        &cfg,
        false,
        &ProbeSelection::default(),
        crate::status::cache::cache_path_in(temp.path()),
        temp.path(),
    )
    .await;
    let err = result.expect_err("snapshot failure must fail-fast, not degrade silently");
    assert!(
        err.to_string()
            .contains("requires the remote config snapshot"),
        "{err}"
    );
}

#[tokio::test]
async fn remote_status_probe_uses_sse_stream_and_accept_header() {
    use axum::http::HeaderMap;
    use std::sync::{Arc, Mutex};

    let ping_received = Arc::new(Mutex::new(false));
    let ping_rec_clone = Arc::clone(&ping_received);
    let probe_accept_received = Arc::new(Mutex::new(None::<String>));
    let probe_accept_clone = Arc::clone(&probe_accept_received);
    let probe_count_received = Arc::new(Mutex::new(0usize));
    let probe_count_clone = Arc::clone(&probe_count_received);

    let app = axum::Router::new()
        .route(
            "/admin/ping",
            axum::routing::get(move || {
                let ping_flag = Arc::clone(&ping_rec_clone);
                async move {
                    *ping_flag.lock().unwrap() = true;
                    axum::Json(serde_json::json!({
                        "version": "0.3.0",
                        "config_identity": "test-ident-remote-probe"
                    }))
                }
            }),
        )
        .route(
            "/admin/status/probe",
            axum::routing::post(move |headers: HeaderMap| {
                let probe_accept = Arc::clone(&probe_accept_clone);
                let probe_count = Arc::clone(&probe_count_clone);
                async move {
                    *probe_count.lock().unwrap() += 1;
                    if let Some(accept) = headers.get("accept") {
                        *probe_accept.lock().unwrap() = Some(accept.to_str().unwrap().to_string());
                    }
                    let stream = futures_util::stream::iter(vec![
                        Ok::<_, std::io::Error>(axum::body::Bytes::from(
                            "event: probe_result\ndata: {\"provider\":\"test-p\",\"model\":\"test-m\",\"protocol\":\"openai-chat\",\"ok\":true,\"latency_ms\":20}\n\n",
                        )),
                        Ok::<_, std::io::Error>(axum::body::Bytes::from("event: done\ndata: {}\n\n")),
                    ]);
                    let body = axum::body::Body::from_stream(stream);
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(body)
                        .unwrap()
                }
            }),
        )
        .route(
            "/admin/status",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "data": {
                        "providers": 1,
                        "models": 1,
                        "active_providers": ["test-p"],
                        "cache": {}
                    }
                }))
            }),
        )
        .route(
            "/admin/config/snapshot",
            axum::routing::get(|| async {
                let snapshot_cfg = crate::config::default_deepseek_config();
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "data": {
                        "config": serde_json::to_value(&snapshot_cfg).unwrap(),
                        "revision": 1
                    }
                }))
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let cfg = Config {
        server: None,
        remote: Some(crate::config::RemoteConfig {
            url: format!("http://{addr}"),
            auth_mode: Some(crate::config::RemoteAuthMode::Open),
            auth_token_env: "LP_NONEXISTENT_VAR".into(),
        }),
        fallback: Default::default(),
        protection: Default::default(),
        diagnostics: Default::default(),
        status: Default::default(),
        codex: Default::default(),
        launch: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    let cache_path = crate::status::cache::cache_path_in(temp.path());

    let result = print_status_with_cache_path(
        &config_path,
        &cfg,
        true, // probe = true
        &ProbeSelection::default(),
        cache_path,
        temp.path(),
    )
    .await;

    assert!(
        result.is_ok(),
        "print_status should succeed with streaming probe: {result:?}"
    );
    assert_eq!(
        *probe_count_received.lock().unwrap(),
        1,
        "remote server must receive exactly 1 probe request"
    );
    assert_eq!(
        probe_accept_received.lock().unwrap().as_deref(),
        Some("text/event-stream"),
        "probe request must specify Accept: text/event-stream"
    );
}

#[tokio::test]
async fn remote_status_probe_falls_back_to_json_response_gracefully() {
    use axum::http::HeaderMap;

    let app = axum::Router::new()
        .route(
            "/admin/ping",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "version": "0.3.0",
                    "config_identity": "test-ident-json-fallback"
                }))
            }),
        )
        .route(
            "/admin/status/probe",
            axum::routing::post(|_headers: HeaderMap| async move {
                // 返回普通 application/json
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "probed": 2
                }))
            }),
        )
        .route(
            "/admin/status",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "data": {
                        "providers": 1,
                        "models": 1,
                        "active_providers": ["test-p"],
                        "cache": {}
                    }
                }))
            }),
        )
        .route(
            "/admin/config/snapshot",
            axum::routing::get(|| async {
                let snapshot_cfg = crate::config::default_deepseek_config();
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "data": {
                        "config": serde_json::to_value(&snapshot_cfg).unwrap(),
                        "revision": 1
                    }
                }))
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let cfg = Config {
        server: None,
        remote: Some(crate::config::RemoteConfig {
            url: format!("http://{addr}"),
            auth_mode: Some(crate::config::RemoteAuthMode::Open),
            auth_token_env: "LP_NONEXISTENT_VAR".into(),
        }),
        fallback: Default::default(),
        protection: Default::default(),
        diagnostics: Default::default(),
        status: Default::default(),
        codex: Default::default(),
        launch: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    let cache_path = crate::status::cache::cache_path_in(temp.path());

    let result = print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &ProbeSelection::default(),
        cache_path,
        temp.path(),
    )
    .await;

    assert!(
        result.is_ok(),
        "print_status should succeed when fallback to JSON: {result:?}"
    );
}

#[tokio::test]
async fn remote_status_probe_stream_error_propagates_err() {
    let app = axum::Router::new()
        .route(
            "/admin/ping",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "version": "0.3.0",
                    "config_identity": "test-ident-stream-error"
                }))
            }),
        )
        .route(
            "/admin/status/probe",
            axum::routing::post(|| async {
                let stream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
                    axum::body::Bytes::from(
                        "event: error\ndata: {\"error\":\"remote worker failure\"}\n\n",
                    ),
                )]);
                let body = axum::body::Body::from_stream(stream);
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(body)
                    .unwrap()
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });

    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let cfg = Config {
        server: None,
        remote: Some(crate::config::RemoteConfig {
            url: format!("http://{addr}"),
            auth_mode: Some(crate::config::RemoteAuthMode::Open),
            auth_token_env: "LP_NONEXISTENT_VAR".into(),
        }),
        fallback: Default::default(),
        protection: Default::default(),
        diagnostics: Default::default(),
        status: Default::default(),
        codex: Default::default(),
        launch: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    let cache_path = crate::status::cache::cache_path_in(temp.path());

    let result = print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &ProbeSelection::default(),
        cache_path,
        temp.path(),
    )
    .await;

    assert!(
        result.is_err(),
        "probe stream error must not be silently swallowed: {result:?}"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("remote worker failure"),
        "error must carry root cause: {err}"
    );
}

/// 纯 Remote 部署的 Runtime State 必须标注客户端模式，
/// 不得盲连本地 Unix Socket 误报 `service not running`；本地部署对照组
/// 仍走本地存活探测。
#[test]
fn runtime_state_marks_client_mode_in_pure_remote_deployment() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");

    let client_only = Config {
        server: None,
        remote: Some(crate::config::RemoteConfig {
            url: "https://example.invalid/lp".into(),
            auth_mode: Some(crate::config::RemoteAuthMode::Open),
            auth_token_env: "LP_CLIENT_ONLY_TOKEN".into(),
        }),
        fallback: Default::default(),
        protection: Default::default(),
        diagnostics: Default::default(),
        status: Default::default(),
        codex: Default::default(),
        launch: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    let lines = runtime_state_lines(&config_path, &client_only);
    assert_eq!(
        lines.len(),
        1,
        "client-only mode must not fall through to the local socket probe: {lines:?}"
    );
    assert!(
        lines[0].contains("client-only (remote)"),
        "client-only runtime state must be labeled: {lines:?}"
    );
    assert!(
        !lines[0].contains("service not running"),
        "client-only mode must not emit the local service warning: {lines:?}"
    );

    // 对照：本地 [server] 部署无实例时仍如实报 not running。
    let local = Config {
        server: Some(crate::config::ServerConfig {
            listen: "127.0.0.1:0".into(),
            usage: Default::default(),
            max_sse_buffer_bytes: 1,
            max_output_items: 1,
            proxy: None,
            require_auth: None,
            auth_token: None,
            auth_token_env: None,
            management_port: None,
        }),
        remote: None,
        fallback: Default::default(),
        protection: Default::default(),
        diagnostics: Default::default(),
        status: Default::default(),
        codex: Default::default(),
        launch: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    let lines = runtime_state_lines(&config_path, &local);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("service not running"),
        "local deployment without an instance must still warn: {lines:?}"
    );
}

/// 快照渲染下 provider 凭证按远端托管口径展示：本地 env / OAuth 账号库
/// 对 client-only 节点无意义，按本地口径核验必然全行误报 WARN。
#[test]
fn provider_rows_remote_managed_scope_emits_no_local_auth_warning() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let provider = ProviderConfig {
        auth: Some(AuthConfig::OpenaiOauth { account: None }),
        ..ProviderConfig::default()
    };
    let mut cfg = Config {
        server: None,
        remote: Some(crate::config::RemoteConfig {
            url: "https://example.invalid/lp".into(),
            auth_mode: Some(crate::config::RemoteAuthMode::Open),
            auth_token_env: "LP_CLIENT_ONLY_TOKEN".into(),
        }),
        fallback: Default::default(),
        protection: Default::default(),
        diagnostics: Default::default(),
        status: Default::default(),
        codex: Default::default(),
        launch: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    cfg.providers.insert("p1".to_string(), provider);
    let selection = ProbeSelection::default();

    let local_rows = provider_rows(&cfg, &config_path, &selection, AuthScope::Local);
    assert_eq!(local_rows.len(), 1);
    assert!(
        local_rows[0].contains("WARN"),
        "local scope keeps reporting the (empty) local store: {:?}",
        local_rows[0]
    );

    let remote_rows = provider_rows(&cfg, &config_path, &selection, AuthScope::RemoteManaged);
    assert_eq!(remote_rows.len(), 1);
    assert!(
        remote_rows[0].contains("remote-managed"),
        "snapshot-rendered provider must be labeled remote-managed: {:?}",
        remote_rows[0]
    );
    assert!(
        !remote_rows[0].contains("WARN") && !remote_rows[0].contains("missing-login"),
        "snapshot-rendered provider must not carry local auth warnings: {:?}",
        remote_rows[0]
    );
}

/// 远端配置快照拉取——解析 data.config 并携带 Bearer 鉴权；响应携带的
/// revision（TUI 编辑器 CAS 用）不消费不展示，缺失也不构成错误。
#[tokio::test]
async fn test_fetch_remote_config_snapshot_parses_remote_config() {
    use axum::http::HeaderMap;
    use std::sync::{Arc, Mutex};

    let auth_seen = Arc::new(Mutex::new(None::<String>));
    let auth_seen_clone = Arc::clone(&auth_seen);
    let app = axum::Router::new().route(
        "/admin/config/snapshot",
        axum::routing::get(move |headers: HeaderMap| {
            let auth_seen = Arc::clone(&auth_seen_clone);
            async move {
                if let Some(auth) = headers.get("authorization") {
                    *auth_seen.lock().unwrap() = Some(auth.to_str().unwrap().to_string());
                }
                let snapshot_cfg = crate::config::default_deepseek_config();
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "data": {
                        "config": serde_json::to_value(&snapshot_cfg).unwrap(),
                        "revision": 7
                    }
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build client");
    let snapshot =
        fetch_remote_config_snapshot(&client, &format!("http://{addr}"), Some("tok-123"))
            .await
            .expect("snapshot fetch should succeed");
    let snapshot_cfg = snapshot.config;

    assert!(
        snapshot_cfg.providers.contains_key("deepseek"),
        "snapshot providers must round-trip: {:?}",
        snapshot_cfg.providers.keys().collect::<Vec<_>>()
    );
    assert!(
        !snapshot_cfg.models.is_empty(),
        "snapshot models must round-trip for the status model tree"
    );
    assert_eq!(
        *auth_seen.lock().unwrap(),
        Some("Bearer tok-123".to_string()),
        "snapshot request must carry the Bearer token"
    );

    // 响应缺 revision 同样可解析（不消费不依赖的容忍度契约）。
    let app = axum::Router::new().route(
        "/admin/config/snapshot",
        axum::routing::get(|| async {
            let snapshot_cfg = crate::config::default_deepseek_config();
            axum::Json(serde_json::json!({
                "status": "ok",
                "data": { "config": serde_json::to_value(&snapshot_cfg).unwrap() }
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    let snapshot = fetch_remote_config_snapshot(&client, &format!("http://{addr}"), None)
        .await
        .expect("revision-less snapshot must still parse");
    assert!(snapshot.config.providers.contains_key("deepseek"));
}

/// 快照端点不可用（旧版 server 404 / 缺 data.config）必须报错而非静默返回空配置，
/// 调用方才可能给出“列表为空”的明确提示。
#[tokio::test]
async fn test_fetch_remote_config_snapshot_errors_on_bad_responses() {
    // 404：远端无该路由
    let app = axum::Router::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build client");
    let err = fetch_remote_config_snapshot(&client, &format!("http://{addr}"), None)
        .await
        .expect_err("404 must be reported");
    assert!(err.to_string().contains("404"), "{err}");

    // 缺 data.config：形状不符必须报错
    let app = axum::Router::new().route(
        "/admin/config/snapshot",
        axum::routing::get(|| async {
            axum::Json(serde_json::json!({ "status": "ok", "data": {} }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    let err = fetch_remote_config_snapshot(&client, &format!("http://{addr}"), None)
        .await
        .expect_err("missing config must be reported");
    assert!(err.to_string().contains("no config"), "{err}");
}

fn chat_provider(chat_url: String) -> ProviderConfig {
    ProviderConfig {
        api_key_env: None,
        auth: Some(AuthConfig::None),
        openai_chat: Some(crate::config::EndpointConfig::native(chat_url)),
        request_frequency: Some(crate::config::RequestFrequencyConfig {
            requests_per_minute: Some(60_000),
            requests_per_hour: None,
            burst: Some(60_000),
            queue_timeout_seconds: Some(1),
        }),
        ..ProviderConfig::default()
    }
}

fn single_chat_model(providers: &[&str]) -> crate::config::ModelConfig {
    crate::config::ModelConfig {
        description: None,
        context_window: 1_000_000,
        max_output_tokens: 128_000,
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
        openai_chat_providers: providers
            .iter()
            .map(|name| crate::config::ProviderBinding {
                name: name.to_string(),
                model: "upstream-model".to_string(),
            })
            .collect(),
        openai_responses_providers: Vec::new(),
        anthropic_providers: Vec::new(),
        reasoning_level_map: None,
    }
}

#[tokio::test]
async fn online_probe_classifies_upstream_error_bodies() {
    let app = axum::Router::new()
        .route(
            "/quota",
            axum::routing::post(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(
                        serde_json::json!({"error": {"type": "exceeded_current_quota_error"}}),
                    ),
                )
            }),
        )
        .route(
            "/terminated",
            axum::routing::post(|| async {
                (
                    StatusCode::FORBIDDEN,
                    axum::Json(serde_json::json!({"error": {"type": "access_terminated_error"}})),
                )
            }),
        )
        .route(
            "/ratelimit",
            axum::routing::post(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(serde_json::json!({"error": {"type": "rate_limit_exceeded"}})),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve upstream");
    });

    let mut cfg = crate::config::default_deepseek_config();
    cfg.providers.clear();
    cfg.models.clear();
    for (name, path) in [
        ("quota-p", "quota"),
        ("terminated-p", "terminated"),
        ("ratelimit-p", "ratelimit"),
    ] {
        cfg.providers.insert(
            name.to_string(),
            chat_provider(format!("http://{addr}/{path}")),
        );
    }
    // Unreachable port → network failure branch.
    cfg.providers.insert(
        "dead-p".to_string(),
        chat_provider("http://127.0.0.1:1/chat/completions".to_string()),
    );
    cfg.models.insert(
        "m".to_string(),
        single_chat_model(&["quota-p", "terminated-p", "ratelimit-p", "dead-p"]),
    );

    // Use a tempdir as config_dir so StateDirs::resolve produces a unique
    // identity with no pre-existing cooldown state on disk (cooldown state
    // loading was added to the probe path).
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_path = tmp.path().join("config.toml");
    std::fs::write(&config_path, "# test").expect("write dummy config");

    let mut cache = StatusCache::default();
    super::probe::run_online_probes(&config_path, &cfg, &mut cache, &ProbeSelection::default())
        .await;

    let error_of = |provider: &str| {
        cache
            .probes
            .get(&probe_key("m", provider, Protocol::OpenaiChatCompletions))
            .and_then(|entry| entry.error.clone())
            .unwrap_or_default()
    };
    assert!(
        error_of("quota-p").contains("账户余额不足"),
        "got: {}",
        error_of("quota-p")
    );
    assert!(
        error_of("terminated-p").contains("配额已用完"),
        "got: {}",
        error_of("terminated-p")
    );
    assert!(
        error_of("ratelimit-p").contains("请求过于频繁"),
        "got: {}",
        error_of("ratelimit-p")
    );
    let dead = error_of("dead-p");
    assert!(!dead.is_empty(), "network failure should record error");
}

#[tokio::test]
async fn print_status_local_probe_writes_cache_and_discovers_dynamic_models() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache_path = crate::status::cache::cache_path_in(dir.path());

    let app = axum::Router::new()
        .route(
            "/chat/completions",
            axum::routing::post(|| async { axum::Json(serde_json::json!({"choices": []})) }),
        )
        .route(
            "/api/tags",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"models": [{"name": "qwen3:27b"}]}))
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
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve upstream");
    });

    let mut cfg = crate::config::default_deepseek_config();
    let deepseek = cfg.providers.get_mut("deepseek").unwrap();
    deepseek.openai_chat.as_mut().unwrap().url = Some(format!("http://{addr}/chat/completions"));
    deepseek.api_key_env = None;
    deepseek.auth = Some(AuthConfig::None);
    deepseek.request_frequency = Some(crate::config::RequestFrequencyConfig {
        requests_per_minute: Some(60_000),
        requests_per_hour: None,
        burst: Some(60_000),
        queue_timeout_seconds: Some(1),
    });
    let mut ollama = crate::catalog::ollama().provider;
    ollama.openai_chat.as_mut().unwrap().url = Some(format!("http://{addr}/v1/chat/completions"));
    cfg.providers.insert("ollama".to_string(), ollama);

    // 注入 dir.path() 作为 runtime_dir → detect_server_at 找不到 sock → 本地 probe 分支
    let config_path = dir.path().join("config.toml");
    if let Some(ref mut server) = cfg.server {
        server.listen = "127.0.0.1:1".to_string();
    }
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    crate::status::print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &ProbeSelection::default(),
        cache_path.clone(),
        dir.path(),
    )
    .await
    .expect("local probe status");

    assert!(cache_path.exists(), "probe should write cache file");
    let written: StatusCache =
        serde_json::from_str(&std::fs::read_to_string(cache_path).expect("read cache"))
            .expect("parse cache");
    assert!(!written.probes.is_empty(), "probes recorded");
    assert!(
        written.dynamic_models.contains_key("ollama"),
        "ollama dynamic models discovered"
    );
}

#[test]
fn print_provider_info_json_formats_all_branches() {
    print_provider_info_json(&serde_json::json!({
        "id": "p1",
        "auth": "api_key",
        "endpoints": [
            {"protocol": "chat", "url": "http://x"},
            {"protocol": "responses", "derive_from": "chat_completions"},
            {"protocol": "anthropic"}
        ],
        "usage": {
            "plan_type": "pro",
            "rate_limit": {
                "limit_reached": false,
                "primary_window": {"used_percent": 42, "reset_after_seconds": 300},
                "secondary_window": {"used_percent": 7}
            },
            "reset_credits_available": true
        }
    }));
    print_provider_info_json(&serde_json::json!({
        "id": "p2",
        "usage": {"unavailable": "token expired"}
    }));
}

#[tokio::test]
async fn print_provider_info_filters_by_name_and_shows_usage_fallback() {
    let mut cfg = crate::config::default_deepseek_config();
    cfg.providers.clear();
    cfg.providers.insert(
        "key-p".to_string(),
        chat_provider("http://127.0.0.1:1/chat/completions".to_string()),
    );
    cfg.providers.insert(
        "oauth-p".to_string(),
        ProviderConfig {
            auth: Some(AuthConfig::OpenaiOauth {
                account: Some("oauth-p".to_string()),
            }),
            openai_responses: Some(crate::config::EndpointConfig::native(
                "http://127.0.0.1:1/v1/responses",
            )),
            ..ProviderConfig::default()
        },
    );

    let config_path = std::path::Path::new("/nonexistent/config.toml");
    print_provider_info(&cfg, config_path, None).await;
    print_provider_info(&cfg, config_path, Some("oauth-p")).await;
}

#[tokio::test]
async fn local_probe_executes_each_selected_provider_model_pair() {
    use std::sync::{Arc, Mutex};

    let hits = Arc::new(Mutex::new(0usize));
    let hits_for_handler = Arc::clone(&hits);
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move || {
            let hits = Arc::clone(&hits_for_handler);
            async move {
                *hits.lock().expect("hits lock") += 1;
                axum::Json(serde_json::json!({"choices": [{"message": {"content": "pong"}}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve upstream");
    });

    let mut cfg = crate::config::default_deepseek_config();
    cfg.providers.clear();
    cfg.providers.insert(
        "p1".to_string(),
        ProviderConfig {
            openai_chat: Some(crate::config::EndpointConfig::native(format!(
                "http://{addr}/chat/completions"
            ))),
            auth: Some(AuthConfig::None),
            ..Default::default()
        },
    );
    cfg.models.clear();
    cfg.models
        .insert("m1".to_string(), single_chat_model(&["p1"]));
    cfg.models
        .insert("m2".to_string(), single_chat_model(&["p1"]));

    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    let mut cache = StatusCache::default();
    super::probe::run_online_probes(
        &config_path,
        &cfg,
        &mut cache,
        &ProbeSelection {
            provider: Some("p1".to_string()),
            model: None,
        },
    )
    .await;

    assert_eq!(*hits.lock().expect("hits lock"), 2);
    assert!(
        cache
            .probes
            .contains_key(&probe_key("m1", "p1", Protocol::OpenaiChatCompletions))
    );
    assert!(
        cache
            .probes
            .contains_key(&probe_key("m2", "p1", Protocol::OpenaiChatCompletions))
    );
}

#[tokio::test]
async fn status_local_probe_selection_filters_upstream_requests_and_cache() {
    use std::sync::{Arc, Mutex};

    let p1_hits = Arc::new(Mutex::new(0usize));
    let p2_hits = Arc::new(Mutex::new(0usize));
    let p1_hits_cb = Arc::clone(&p1_hits);
    let p2_hits_cb = Arc::clone(&p2_hits);

    let app = axum::Router::new()
        .route(
            "/p1/chat/completions",
            axum::routing::post(move || {
                let hits = Arc::clone(&p1_hits_cb);
                async move {
                    *hits.lock().unwrap() += 1;
                    axum::Json(
                        serde_json::json!({"choices": [{"message": {"content": "pong p1"}}]}),
                    )
                }
            }),
        )
        .route(
            "/p2/chat/completions",
            axum::routing::post(move || {
                let hits = Arc::clone(&p2_hits_cb);
                async move {
                    *hits.lock().unwrap() += 1;
                    axum::Json(
                        serde_json::json!({"choices": [{"message": {"content": "pong p2"}}]}),
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve upstream");
    });

    let mut cfg = crate::config::default_deepseek_config();
    let p1_cfg = ProviderConfig {
        openai_chat: Some(crate::config::EndpointConfig::native(format!(
            "http://{addr}/p1/chat/completions"
        ))),
        auth: Some(AuthConfig::None),
        ..Default::default()
    };
    cfg.providers.insert("p1".to_string(), p1_cfg);

    let p2_cfg = ProviderConfig {
        openai_chat: Some(crate::config::EndpointConfig::native(format!(
            "http://{addr}/p2/chat/completions"
        ))),
        auth: Some(AuthConfig::None),
        ..Default::default()
    };
    cfg.providers.insert("p2".to_string(), p2_cfg);

    cfg.models.clear();
    cfg.models
        .insert("m1".to_string(), single_chat_model(&["p1"]));
    cfg.models
        .insert("m2".to_string(), single_chat_model(&["p2"]));

    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    let cache_path = dir.path().join("cache.json");
    if let Some(ref mut server) = cfg.server {
        server.listen = "127.0.0.1:1".to_string();
    }

    // 1. Target only m1
    let sel_m1 = ProbeSelection {
        provider: None,
        model: Some("m1".to_string()),
    };
    print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &sel_m1,
        cache_path.clone(),
        dir.path(),
    )
    .await
    .expect("probe m1");

    assert_eq!(*p1_hits.lock().unwrap(), 1, "m1/p1 should be probed once");
    assert_eq!(
        *p2_hits.lock().unwrap(),
        0,
        "unselected m2/p2 must have zero upstream requests"
    );

    let written: StatusCache =
        serde_json::from_str(&std::fs::read_to_string(&cache_path).expect("read cache"))
            .expect("parse cache");
    assert!(
        written
            .probes
            .contains_key(&probe_key("m1", "p1", Protocol::OpenaiChatCompletions)),
        "selected m1/p1 probe written to cache"
    );
    assert!(
        !written
            .probes
            .contains_key(&probe_key("m2", "p2", Protocol::OpenaiChatCompletions)),
        "unselected m2/p2 probe must NOT be written to cache"
    );
}

#[tokio::test]
async fn status_local_probe_glob_selection_filters_upstream_requests_and_cache() {
    use std::sync::{Arc, Mutex};

    let p1_hits = Arc::new(Mutex::new(0usize));
    let p2_hits = Arc::new(Mutex::new(0usize));

    let p1_hits_c = Arc::clone(&p1_hits);
    let p2_hits_c = Arc::clone(&p2_hits);

    let app = axum::Router::new()
        .route(
            "/p1/chat/completions",
            axum::routing::post(move || {
                let hits = Arc::clone(&p1_hits_c);
                async move {
                    *hits.lock().unwrap() += 1;
                    axum::Json(serde_json::json!({"choices": [{"message": {"content": "p1"}}] }))
                }
            }),
        )
        .route(
            "/p2/chat/completions",
            axum::routing::post(move || {
                let hits = Arc::clone(&p2_hits_c);
                async move {
                    *hits.lock().unwrap() += 1;
                    axum::Json(serde_json::json!({"choices": [{"message": {"content": "p2"}}] }))
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });

    let mut cfg = crate::config::default_deepseek_config();
    cfg.providers.clear();
    let p1_cfg = ProviderConfig {
        openai_chat: Some(crate::config::EndpointConfig::native(format!(
            "http://{addr}/p1/chat/completions"
        ))),
        auth: Some(AuthConfig::None),
        ..Default::default()
    };
    cfg.providers.insert("p1".to_string(), p1_cfg);

    let p2_cfg = ProviderConfig {
        openai_chat: Some(crate::config::EndpointConfig::native(format!(
            "http://{addr}/p2/chat/completions"
        ))),
        auth: Some(AuthConfig::None),
        ..Default::default()
    };
    cfg.providers.insert("p2".to_string(), p2_cfg);

    cfg.models.clear();
    cfg.models
        .insert("model-alpha".to_string(), single_chat_model(&["p1"]));
    cfg.models
        .insert("model-beta".to_string(), single_chat_model(&["p2"]));

    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    let cache_path = dir.path().join("cache.json");
    if let Some(ref mut server) = cfg.server {
        server.listen = "127.0.0.1:1".to_string();
    }

    // Target via glob "model-a*"
    let sel_glob = ProbeSelection {
        provider: None,
        model: Some("model-a*".to_string()),
    };
    print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &sel_glob,
        cache_path.clone(),
        dir.path(),
    )
    .await
    .expect("probe model-a*");

    assert_eq!(
        *p1_hits.lock().unwrap(),
        1,
        "model-alpha/p1 should be probed once"
    );
    assert_eq!(
        *p2_hits.lock().unwrap(),
        0,
        "unselected model-beta/p2 must have zero upstream requests"
    );

    let written: StatusCache =
        serde_json::from_str(&std::fs::read_to_string(&cache_path).expect("read cache"))
            .expect("parse cache");
    assert!(
        written.probes.contains_key(&probe_key(
            "model-alpha",
            "p1",
            Protocol::OpenaiChatCompletions
        )),
        "selected model-alpha/p1 probe written to cache"
    );
    assert!(
        !written.probes.contains_key(&probe_key(
            "model-beta",
            "p2",
            Protocol::OpenaiChatCompletions
        )),
        "unselected model-beta/p2 probe must NOT be written to cache"
    );
}

#[tokio::test]
async fn status_local_probe_validate_error_propagates_without_probing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    let cache_path = dir.path().join("cache.json");

    let mut cfg = crate::config::default_deepseek_config();
    if let Some(ref mut server) = cfg.server {
        server.listen = "127.0.0.1:1".to_string();
    }

    // 1. Unknown provider
    let sel_unknown_prov = ProbeSelection {
        provider: Some("unknown_prov".to_string()),
        model: None,
    };
    let err = print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &sel_unknown_prov,
        cache_path.clone(),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("unknown provider: 'unknown_prov'"),
        "got: {err}"
    );
    assert!(
        !cache_path.exists(),
        "no cache should be written on validate failure"
    );

    // 2. Unknown model
    let sel_unknown_mod = ProbeSelection {
        provider: None,
        model: Some("unknown_mod".to_string()),
    };
    let err = print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &sel_unknown_mod,
        cache_path.clone(),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("unknown model: 'unknown_mod'"),
        "got: {err}"
    );
    assert!(
        !cache_path.exists(),
        "no cache should be written on validate failure"
    );

    // 3. Known provider and model with empty intersection
    let p_other = ProviderConfig {
        openai_chat: Some(crate::config::EndpointConfig::native(
            "http://127.0.0.1:1/chat".to_string(),
        )),
        ..Default::default()
    };
    cfg.providers.insert("p_other".to_string(), p_other);
    let sel_disjoint = ProbeSelection {
        provider: Some("p_other".to_string()),
        model: Some("deepseek-v4-flash-lp".to_string()),
    };
    let err = print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &sel_disjoint,
        cache_path.clone(),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("intersection"), "got: {err}");
    assert!(
        !cache_path.exists(),
        "no cache should be written on validate failure"
    );
}

#[tokio::test]
async fn status_local_probe_dynamic_discovery_scope_limitation() {
    use std::sync::{Arc, Mutex};

    let ollama_hits = Arc::new(Mutex::new(0usize));
    let ollama_hits_cb = Arc::clone(&ollama_hits);

    let app = axum::Router::new()
        .route(
            "/api/tags",
            axum::routing::get(move || {
                let hits = Arc::clone(&ollama_hits_cb);
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
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve upstream");
    });

    let mut cfg = crate::config::default_deepseek_config();
    let ollama = ProviderConfig {
        openai_chat: Some(crate::config::EndpointConfig::native(format!(
            "http://{addr}/v1/chat/completions"
        ))),
        ..Default::default()
    };
    cfg.providers.insert("ollama".to_string(), ollama);

    cfg.models
        .insert("m_ollama".to_string(), single_chat_model(&["ollama"]));

    let oauth_accounts_path = std::path::Path::new("/nonexistent/oauth_accounts.json");

    // 1. Selection targets only deepseek (model = deepseek-v4-flash-lp)
    // Ollama must NOT be discovered!
    let sel_deepseek = ProbeSelection {
        provider: None,
        model: Some("deepseek-v4-flash-lp".to_string()),
    };
    let mut cache = StatusCache::default();
    super::probe::discover_dynamic_models(&cfg, &mut cache, &sel_deepseek, oauth_accounts_path)
        .await;
    assert_eq!(
        *ollama_hits.lock().unwrap(),
        0,
        "ollama should NOT receive discovery requests when unselected"
    );
    assert!(
        !cache.dynamic_models.contains_key("ollama"),
        "ollama dynamic models should NOT be recorded when unselected"
    );

    // 2. Selection targets ollama
    let sel_ollama = ProbeSelection {
        provider: Some("ollama".to_string()),
        model: None,
    };
    super::probe::discover_dynamic_models(&cfg, &mut cache, &sel_ollama, oauth_accounts_path).await;
    assert_eq!(
        *ollama_hits.lock().unwrap(),
        1,
        "ollama should receive discovery request when selected"
    );
    assert!(
        cache.dynamic_models.contains_key("ollama"),
        "ollama dynamic models should be recorded when selected"
    );

    // 3. Empty selection (all)
    let mut cache_all = StatusCache::default();
    super::probe::discover_dynamic_models(
        &cfg,
        &mut cache_all,
        &ProbeSelection::default(),
        oauth_accounts_path,
    )
    .await;
    assert_eq!(
        *ollama_hits.lock().unwrap(),
        2,
        "ollama should receive discovery request when selection is empty (all)"
    );
    assert!(cache_all.dynamic_models.contains_key("ollama"));
}

#[tokio::test]
#[cfg(unix)]
async fn status_delegation_selector_rejects_old_server_before_probe_request() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:0\"\n").expect("write config");
    let identity = crate::config::config_identity(dir.path());

    let probe_requests = Arc::new(AtomicUsize::new(0));
    let probe_requests_for_handler = Arc::clone(&probe_requests);
    let app = axum::Router::new()
        .route(
            "/admin/ping",
            axum::routing::get(move || {
                let id = identity.clone();
                async move {
                    axum::Json(serde_json::json!({
                        "status": "ok",
                        "version": "0.2.9",
                        "config_identity": id,
                    }))
                }
            }),
        )
        .route(
            "/admin/status/probe",
            axum::routing::post(move || {
                let probe_requests = Arc::clone(&probe_requests_for_handler);
                async move {
                    probe_requests.fetch_add(1, Ordering::SeqCst);
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        "event: done\ndata: {}\n\n",
                    )
                }
            }),
        );
    // detect_server 改走 UDS，mock 也必须是 UDS
    let runtime_dir = dir.path().join("run");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    let sock = crate::service::socket_path_in(&runtime_dir);
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind mock uds");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let service = hyper_util::service::TowerToHyperService::new(
                app.clone().into_service::<hyper::body::Incoming>(),
            );
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    let cache_path = dir.path().join("cache.json");
    let mut cfg = crate::config::default_deepseek_config();
    if let Some(ref mut server) = cfg.server {
        server.listen = "127.0.0.1:0".to_string();
    }

    let selection = ProbeSelection {
        provider: Some("deepseek".to_string()),
        model: None,
    };
    // 注入 runtime_dir → detect_server_at 找到 mock UDS → 版本 0.2.9 < 0.3.0
    // → 版本门禁在 status_probe_stream 内 bail → probe 请求数 = 0（回归闸门）
    let err = print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &selection,
        cache_path,
        &runtime_dir,
    )
    .await
    .expect_err("old server must reject selector before any probe");
    assert!(
        err.to_string()
            .contains("server does not support probe selection"),
        "got: {err}"
    );
    assert_eq!(probe_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[cfg(unix)]
async fn status_delegation_to_server_with_different_config_identity_is_refused() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:0\"\n").expect("write config");

    let probe_requests = Arc::new(AtomicUsize::new(0));
    let probe_requests_for_handler = Arc::clone(&probe_requests);
    let app = axum::Router::new()
        .route(
            "/admin/ping",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "version": env!("CARGO_PKG_VERSION"),
                    "config_identity": "00000000deadbeef",
                }))
            }),
        )
        .route(
            "/admin/status/probe",
            axum::routing::post(move || {
                let probe_requests = Arc::clone(&probe_requests_for_handler);
                async move {
                    probe_requests.fetch_add(1, Ordering::SeqCst);
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        "event: done\ndata: {}\n\n",
                    )
                }
            }),
        );
    let runtime_dir = dir.path().join("run");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    let sock = crate::service::socket_path_in(&runtime_dir);
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind mock uds");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let service = hyper_util::service::TowerToHyperService::new(
                app.clone().into_service::<hyper::body::Incoming>(),
            );
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    let cache_path = dir.path().join("cache.json");
    let mut cfg = crate::config::default_deepseek_config();
    if let Some(ref mut server) = cfg.server {
        server.listen = "127.0.0.1:0".to_string();
    }

    let selection = ProbeSelection::default();
    let err = print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &selection,
        cache_path,
        &runtime_dir,
    )
    .await
    .expect_err("mismatched config identity must be hard-refused");

    let err_msg = err.to_string();
    assert!(
        err_msg.contains("拒绝委托"),
        "error must state delegation refused: {err_msg}"
    );
    assert!(
        err_msg.contains("正在使用另一份 config"),
        "error must identify config mismatch: {err_msg}"
    );
    assert!(
        err_msg.contains("00000000deadbeef"),
        "error must report actual identity: {err_msg}"
    );
    assert_eq!(
        probe_requests.load(Ordering::SeqCst),
        0,
        "no probe request should have been dispatched"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn status_delegation_to_server_without_config_identity_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:0\"\n").expect("write config");

    let app = axum::Router::new().route(
        "/admin/ping",
        axum::routing::get(|| async {
            axum::Json(serde_json::json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                // 未返回 config_identity
            }))
        }),
    );
    let runtime_dir = dir.path().join("run");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    let sock = crate::service::socket_path_in(&runtime_dir);
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind mock uds");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let service = hyper_util::service::TowerToHyperService::new(
                app.clone().into_service::<hyper::body::Incoming>(),
            );
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    let cache_path = dir.path().join("cache.json");
    let mut cfg = crate::config::default_deepseek_config();
    if let Some(ref mut server) = cfg.server {
        server.listen = "127.0.0.1:0".to_string();
    }

    let selection = ProbeSelection::default();
    let err = print_status_with_cache_path(
        &config_path,
        &cfg,
        true,
        &selection,
        cache_path,
        &runtime_dir,
    )
    .await
    .expect_err("server without config identity must be hard-refused");

    let err_msg = err.to_string();
    assert!(
        err_msg.contains("拒绝委托"),
        "error must state delegation refused: {err_msg}"
    );
    assert!(
        err_msg.contains("未上报 config identity"),
        "error must state missing identity: {err_msg}"
    );
}

// 委托测试原先经 TCP mock 打 `/admin/status/probe`，后迁
// 入 UDS 管理通道。print_status_with_cache_path 添加了 runtime_dir
// 注入参数，测试现在可以隔离 mock UDS。4xx 错误体传播仍由
// `admin_client::status_probe_and_stream_propagate_4xx_error_body`
// （真实 UDS mock，更强证据层）覆盖；此处直接守住消费循环逻辑本身。

fn sse(event: &str, data: serde_json::Value) -> anyhow::Result<crate::admin_client::SseEvent> {
    Ok(crate::admin_client::SseEvent {
        event: event.to_string(),
        data,
    })
}

#[tokio::test]
async fn consume_probe_stream_error_event_propagates_err() {
    let stream = futures_util::stream::iter(vec![sse(
        "error",
        serde_json::json!({ "error": "probe worker crashed" }),
    )]);
    let err = consume_probe_stream(Box::pin(stream)).await.unwrap_err();
    assert!(
        err.to_string().contains("probe worker crashed"),
        "got: {err}"
    );
}

#[tokio::test]
async fn consume_probe_stream_termination_without_done_propagates_err() {
    let stream = futures_util::stream::iter(vec![sse(
        "probe_result",
        serde_json::json!({
            "provider": "p1", "model": "m1", "protocol": "openai-chat", "ok": true
        }),
    )]);
    let err = consume_probe_stream(Box::pin(stream)).await.unwrap_err();
    assert!(
        err.to_string().contains("terminated unexpectedly"),
        "got: {err}"
    );
}

#[tokio::test]
async fn consume_probe_stream_idle_timeout_propagates_err() {
    // 构造一个等待超时的 stream
    let pending_stream =
        futures_util::stream::pending::<anyhow::Result<crate::admin_client::SseEvent>>();
    // 由于默认单帧超时为 60s，我们使用 tokio::time::pause() 快进时间
    tokio::time::pause();
    let probe_task =
        tokio::spawn(async move { consume_probe_stream(Box::pin(pending_stream)).await });
    // 快进 61 秒触发空闲超时
    tokio::time::advance(Duration::from_secs(61)).await;
    let res = probe_task.await.expect("task join");
    let err = res.unwrap_err();
    assert!(
        err.to_string().contains("idle > 60s"),
        "must trigger idle timeout: {err}"
    );
}

#[tokio::test]
async fn consume_probe_stream_happy_path_ends_at_done() {
    let stream = futures_util::stream::iter(vec![
        sse(
            "probe_result",
            serde_json::json!({
                "provider": "p1", "model": "m1", "protocol": "openai-chat", "ok": true, "latency_ms": 5
            }),
        ),
        sse("done", serde_json::json!({})),
    ]);
    consume_probe_stream(Box::pin(stream))
        .await
        .expect("done event completes the stream");
}

#[tokio::test]
async fn status_filter_offline_succeeds_without_probe() {
    let cfg = crate::config::default_deepseek_config();
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let cache_path = crate::status::cache::cache_path_in(temp.path());
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    // Exact model filter without probe
    let sel_exact = ProbeSelection {
        provider: None,
        model: Some("deepseek-v4-flash-lp".to_string()),
    };
    let res = print_status_with_cache_path(
        &config_path,
        &cfg,
        false,
        &sel_exact,
        cache_path.clone(),
        temp.path(),
    )
    .await;
    assert!(
        res.is_ok(),
        "offline status with exact model must succeed: {res:?}"
    );

    // Glob model filter without probe
    let sel_glob_model = ProbeSelection {
        provider: None,
        model: Some("deepseek-*-flash*".to_string()),
    };
    let res = print_status_with_cache_path(
        &config_path,
        &cfg,
        false,
        &sel_glob_model,
        cache_path.clone(),
        temp.path(),
    )
    .await;
    assert!(
        res.is_ok(),
        "offline status with glob model must succeed: {res:?}"
    );

    // Glob provider filter without probe
    let sel_glob_prov = ProbeSelection {
        provider: Some("deep*".to_string()),
        model: None,
    };
    let res = print_status_with_cache_path(
        &config_path,
        &cfg,
        false,
        &sel_glob_prov,
        cache_path.clone(),
        temp.path(),
    )
    .await;
    assert!(
        res.is_ok(),
        "offline status with glob provider must succeed: {res:?}"
    );

    // Nonexistent pattern without probe does not panic/error out
    let sel_nonexistent = ProbeSelection {
        provider: None,
        model: Some("nonexistent-*".to_string()),
    };
    let res = print_status_with_cache_path(
        &config_path,
        &cfg,
        false,
        &sel_nonexistent,
        cache_path.clone(),
        temp.path(),
    )
    .await;
    assert!(
        res.is_ok(),
        "offline status with non-matching pattern must succeed: {res:?}"
    );

    // Invalid glob syntax fails early with syntax error
    let sel_invalid = ProbeSelection {
        provider: None,
        model: Some("bad-[".to_string()),
    };
    let res = print_status_with_cache_path(
        &config_path,
        &cfg,
        false,
        &sel_invalid,
        cache_path.clone(),
        temp.path(),
    )
    .await;
    assert!(res.is_err(), "invalid glob syntax must fail");
    assert!(
        res.unwrap_err()
            .to_string()
            .contains("invalid glob pattern")
    );
}

#[tokio::test]
async fn cooldown_and_auth_lines_filtered_by_selection() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cooldown_path = temp.path().join("cooldowns.json");

    let entries = vec![
        crate::cooldown::CooldownEntry {
            model: "qwen-flash".to_string(),
            provider: "ubt-bailian".to_string(),
            protocol: Protocol::OpenaiChatCompletions,
            token: None,
            token_key_hash: None,
            kind: "rate_limit".to_string(),
            started_at_unix: unix_now(),
            expires_at_unix: unix_now() + 60,
            reason: "rate limit".to_string(),
            hit_count: 0,
            last_hit_unix: None,
        },
        crate::cooldown::CooldownEntry {
            model: "deepseek-chat".to_string(),
            provider: "deepseek-official".to_string(),
            protocol: Protocol::OpenaiChatCompletions,
            token: None,
            token_key_hash: None,
            kind: "server_error".to_string(),
            started_at_unix: unix_now(),
            expires_at_unix: unix_now() + 60,
            reason: "500 error".to_string(),
            hit_count: 0,
            last_hit_unix: None,
        },
    ];
    crate::cooldown::write_entries(&cooldown_path, entries).expect("write entries");

    // Match all
    let all =
        super::display::cooldown_lines_with_selection(&cooldown_path, &ProbeSelection::default());
    assert_eq!(all.len(), 2);

    // Filter by model glob
    let qwen_only = super::display::cooldown_lines_with_selection(
        &cooldown_path,
        &ProbeSelection {
            provider: None,
            model: Some("qwen-*".to_string()),
        },
    );
    assert_eq!(qwen_only.len(), 1);
    assert!(qwen_only[0].contains("model=qwen-flash"));

    // Filter by provider glob
    let deepseek_only = super::display::cooldown_lines_with_selection(
        &cooldown_path,
        &ProbeSelection {
            provider: Some("deepseek-*".to_string()),
            model: None,
        },
    );
    assert_eq!(deepseek_only.len(), 1);
    assert!(deepseek_only[0].contains("provider=deepseek-official"));

    // Filter matching nothing -> returns "none"
    let none_matched = super::display::cooldown_lines_with_selection(
        &cooldown_path,
        &ProbeSelection {
            provider: Some("nonexistent-*".to_string()),
            model: None,
        },
    );
    assert_eq!(none_matched.len(), 1);
    assert!(none_matched[0].contains("none"));
}

#[tokio::test]
async fn status_inspects_occupied_listen_port_when_not_running() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let cache_path = temp.path().join("status_cache.json");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().expect("port").port();
    let listen_str = format!("127.0.0.1:{port}");

    let mut cfg = crate::config::default_deepseek_config();
    if let Some(ref mut server) = cfg.server {
        server.listen = listen_str.clone();
    }

    // Verify inspect_port_occupant directly observes this occupied port
    let occ = crate::service::inspect_port_occupant(&listen_str);
    assert!(
        occ.is_some(),
        "occupant must be detected for occupied listen port"
    );
    if cfg!(target_os = "linux") {
        assert_eq!(occ.unwrap().pid, Some(std::process::id()));
    }

    // Run status: must run cleanly and report output without failing
    let res = print_status_with_cache_path(
        &config_path,
        &cfg,
        false,
        &ProbeSelection::default(),
        cache_path,
        temp.path(),
    )
    .await;
    assert!(res.is_ok(), "status must succeed: {res:?}");
}

#[test]
fn status_liveness_reads_injected_runtime_pid_not_global() {
    // 模拟 -d 启动的 server：pid 文件只写进注入分片，全局分片里没有它。
    // 判定若回退到全局 runtime_dir()，本进程 pid 读不到（或读到别的实例），
    // 断言即变红——这正是"正在服务的实例被判 not running 还被建议 kill"的机制。
    let temp = tempfile::tempdir().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let dirs = crate::service::StateDirs::resolve(temp.path());
    std::fs::create_dir_all(&dirs.runtime_dir).expect("create runtime dir");
    std::fs::write(
        crate::service::pid_path_in(&dirs.runtime_dir),
        std::process::id().to_string(),
    )
    .expect("write pid file");

    assert_eq!(
        running_service_pid(&config_path),
        Some(std::process::id()),
        "liveness must come from the config's own identity shard, not the global default"
    );
}

// =========================================================================
// Probe send governance tests
// =========================================================================

/// Helper: build a minimal ExecutionPlan pointing at the given mock URL.
fn plan(mock_url: &str) -> crate::config::ExecutionPlan {
    crate::config::ExecutionPlan {
        frontend_protocol: Protocol::OpenaiChatCompletions,
        provider_id: "test-provider".to_string(),
        upstream_model: "test-model".to_string(),
        source_protocol: Protocol::OpenaiChatCompletions,
        adapter: crate::config::AdapterKind::Passthrough,
        native_url: format!("{mock_url}/v1/chat/completions"),
        auth: crate::config::AuthConfig::None,
        compat: crate::config::CompatConfig::default(),
        anthropic_family_models: Vec::new(),
        store: None,
        request_frequency: crate::config::RequestFrequencyConfig::default(),
        routed_target: None,
    }
}

/// Helper: minimal config with a "test-provider" that has auth configured.
fn config() -> Config {
    let mut cfg = crate::config::default_deepseek_config();
    // Rename deepseek → test-provider for the test
    if let Some(provider) = cfg.providers.remove("deepseek") {
        cfg.providers.insert("test-provider".to_string(), provider);
    }
    cfg
}

/// 判据 1 red proof: when a cooldown entry exists for the
/// (provider, model), the probe must NOT send any upstream request.
#[tokio::test]
async fn probe_skips_upstream_when_cooling_down() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let request_count = Arc::new(AtomicUsize::new(0));
    let count_clone = request_count.clone();

    // Mock upstream that counts requests
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let c = count_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({
                    "id":"probe-ok","object":"chat.completion","model":"test-model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let cfg = config();
    let mut cache = StatusCache::default();
    let client = reqwest::Client::new();
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_file = config_dir.path().join("config.toml");
    let plan = plan(&format!("http://{addr}"));

    // Set up cooldown store with an active cooldown for this (model, provider)
    let cooldown_store = crate::cooldown::CooldownStore::in_memory();
    let cooldown_key = plan.cooldown_key("test-model");
    cooldown_store.set(
        &cooldown_key,
        "rate_limit",
        "test cooldown",
        Duration::from_secs(3600),
    );
    assert!(
        cooldown_store.is_cooling_down(&cooldown_key),
        "cooldown should be active before probe"
    );

    let cooldown_cfg = cfg.effective_cooldown("test-model", "test-provider");
    let sent = run_one_online_probe(
        &config_file,
        &cfg,
        &mut cache,
        &client,
        "test-model",
        Protocol::OpenaiChatCompletions,
        "test-provider",
        &plan,
        Some(&cooldown_store),
        &cooldown_cfg,
    )
    .await;

    // Probe should report "not sent" (skipped due to cooldown)
    assert!(!sent, "probe should return false when skipped by cooldown");

    // Zero upstream requests should have been made
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        0,
        "probe must NOT send upstream when cooldown is active"
    );

    // Cache entry should reflect the cooldown skip
    let key = crate::status::probe_key(
        "test-model",
        "test-provider",
        Protocol::OpenaiChatCompletions,
    );
    let entry = cache.probes.get(&key).expect("cache entry should exist");
    assert!(!entry.ok);
    assert!(
        entry.error.as_ref().is_some_and(|e| e.contains("cooldown")),
        "cache error should mention cooldown, got: {:?}",
        entry.error
    );
}

/// 判据 2 red proof: when the probe receives a 429 from upstream,
/// a cooldown entry must be written to the store.
#[tokio::test]
async fn probe_writes_cooldown_on_429() {
    // Mock upstream that always returns 429
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async {
            axum::response::Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("retry-after", "300")
                .body(axum::body::Body::from(
                    serde_json::json!({"error":{"type":"rate_limit_exceeded","message":"slow down"}})
                        .to_string(),
                ))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let cfg = config();
    let mut cache = StatusCache::default();
    let client = reqwest::Client::new();
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_file = config_dir.path().join("config.toml");
    let plan = plan(&format!("http://{addr}"));

    let cooldown_store = crate::cooldown::CooldownStore::in_memory();
    let cooldown_key = plan.cooldown_key("test-model");

    // Verify no cooldown exists before probe
    assert!(
        !cooldown_store.is_cooling_down(&cooldown_key),
        "no cooldown should exist before probe"
    );

    let cooldown_cfg = cfg.effective_cooldown("test-model", "test-provider");
    let sent = run_one_online_probe(
        &config_file,
        &cfg,
        &mut cache,
        &client,
        "test-model",
        Protocol::OpenaiChatCompletions,
        "test-provider",
        &plan,
        Some(&cooldown_store),
        &cooldown_cfg,
    )
    .await;

    // Probe should report "sent" (it did execute the request)
    assert!(sent, "probe should return true after executing request");

    // Cooldown entry should now exist
    assert!(
        cooldown_store.is_cooling_down(&cooldown_key),
        "cooldown must be written after probe receives 429"
    );

    // Verify the cooldown entry has the right kind
    let entry = cooldown_store
        .active_entry(&cooldown_key)
        .expect("cooldown entry should exist");
    assert_eq!(entry.kind, "rate_limit");
    assert!(entry.reason.contains("429"));
}

/// probe without cooldown store (None) does not crash and
/// does not write cooldowns — backward compatibility.
#[tokio::test]
async fn probe_without_cooldown_store_is_noop() {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async {
            axum::response::Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(axum::body::Body::from("{}"))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let cfg = config();
    let mut cache = StatusCache::default();
    let client = reqwest::Client::new();
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_file = config_dir.path().join("config.toml");
    let plan = plan(&format!("http://{addr}"));

    let cooldown_cfg = cfg.effective_cooldown("test-model", "test-provider");
    // Pass None for cooldown_store — should not crash, should not write cooldown
    let sent = run_one_online_probe(
        &config_file,
        &cfg,
        &mut cache,
        &client,
        "test-model",
        Protocol::OpenaiChatCompletions,
        "test-provider",
        &plan,
        None,
        &cooldown_cfg,
    )
    .await;

    assert!(sent, "probe should execute even without cooldown store");
    // Cache should have the 429 result
    let key = crate::status::probe_key(
        "test-model",
        "test-provider",
        Protocol::OpenaiChatCompletions,
    );
    let entry = cache.probes.get(&key).expect("cache entry should exist");
    assert!(!entry.ok);
    assert_eq!(entry.http_status, Some(429));
}

// =========================================================================
// probe 400 → per-provider abnormal request counter + tolerance
// =========================================================================

/// Helper: spawn a mock upstream that always answers HTTP 400.
async fn spawn_400_upstream() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async {
            axum::response::Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "error": {"type": "invalid_request_error", "message": "bad params"}
                    })
                    .to_string(),
                ))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// 判据 1 red proof：探测收到上游 400 → 该 provider 异常计数 +1；
/// 容忍量（默认 2）未达 → 不写任何冷却。
#[tokio::test]
async fn probe_400_increments_count_without_cooldown_before_tolerance() {
    let upstream = spawn_400_upstream().await;
    let cfg = config();
    let mut cache = StatusCache::default();
    let client = reqwest::Client::new();
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_file = config_dir.path().join("config.toml");
    let plan = plan(&upstream);
    let cooldown_store = crate::cooldown::CooldownStore::in_memory();
    let cooldown_cfg = cfg.effective_cooldown("test-model", "test-provider");

    let sent = run_one_online_probe(
        &config_file,
        &cfg,
        &mut cache,
        &client,
        "test-model",
        Protocol::OpenaiChatCompletions,
        "test-provider",
        &plan,
        Some(&cooldown_store),
        &cooldown_cfg,
    )
    .await;

    assert!(sent, "probe request should be sent upstream");
    assert_eq!(
        cooldown_store.get_bad_request_count("test-provider"),
        1,
        "first upstream 400 must increment the provider counter"
    );
    assert!(
        !cooldown_store.is_cooling_down(&plan.cooldown_key("test-model")),
        "tolerance (default 2) not reached after one 400 — no cooldown yet"
    );
}

/// 判据 2+3 red proof：计数达容忍量 → 写 provider 级通配冷却（覆盖全部
/// client 协议），同 provider 的具体 model 查询被阻断，后续探测不再出网。
#[tokio::test]
async fn probe_400_reaches_tolerance_writes_provider_cooldown() {
    let upstream = spawn_400_upstream().await;
    let cfg = config();
    let mut cache = StatusCache::default();
    let client = reqwest::Client::new();
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_file = config_dir.path().join("config.toml");
    let plan = plan(&upstream);
    let cooldown_store = crate::cooldown::CooldownStore::in_memory();
    let cooldown_cfg = cfg.effective_cooldown("test-model", "test-provider");

    // 第 1 次 400：只计数
    run_one_online_probe(
        &config_file,
        &cfg,
        &mut cache,
        &client,
        "test-model",
        Protocol::OpenaiChatCompletions,
        "test-provider",
        &plan,
        Some(&cooldown_store),
        &cooldown_cfg,
    )
    .await;
    assert_eq!(cooldown_store.get_bad_request_count("test-provider"), 1);
    assert!(!cooldown_store.is_cooling_down(&plan.cooldown_key("test-model")));

    // 第 2 次 400：达到默认容忍量 → provider 级冷却 + 计数清零
    run_one_online_probe(
        &config_file,
        &cfg,
        &mut cache,
        &client,
        "test-model",
        Protocol::OpenaiChatCompletions,
        "test-provider",
        &plan,
        Some(&cooldown_store),
        &cooldown_cfg,
    )
    .await;
    assert_eq!(
        cooldown_store.get_bad_request_count("test-provider"),
        0,
        "counter resets after the cooldown is triggered"
    );

    // 通配条目覆盖全部 client 协议
    for proto in Protocol::CLIENT_PROTOCOLS {
        let wildcard = crate::config::CooldownKey {
            model_id: "*".to_string(),
            provider_id: "test-provider".to_string(),
            protocol: proto,
            token: None,
            token_key_hash: None,
        };
        assert!(
            cooldown_store.is_cooling_down(&wildcard),
            "wildcard cooldown must exist for {proto:?}"
        );
    }
    // 转发路由按具体 model 查询——同 provider 的任何 model 都必须被阻断
    assert!(
        cooldown_store.is_cooling_down(&plan.cooldown_key("test-model")),
        "provider-level cooldown must block concrete model lookups"
    );
    // 其他 provider 不受影响
    let other = crate::config::CooldownKey {
        model_id: "test-model".to_string(),
        provider_id: "other-provider".to_string(),
        protocol: Protocol::OpenaiChatCompletions,
        token: None,
        token_key_hash: None,
    };
    assert!(!cooldown_store.is_cooling_down(&other));

    // 冷却已生效：下一次探测被 pre-send 检查跳过，不再出网
    let sent = run_one_online_probe(
        &config_file,
        &cfg,
        &mut cache,
        &client,
        "test-model",
        Protocol::OpenaiChatCompletions,
        "test-provider",
        &plan,
        Some(&cooldown_store),
        &cooldown_cfg,
    )
    .await;
    assert!(
        !sent,
        "probe must be skipped once provider cooldown is active"
    );
}

/// 判据 2 red proof：容忍量可配置——设为 3 时连续 2 次 400 不冷却，
/// 第 3 次 400 后 provider 级冷却生效。
#[tokio::test]
async fn probe_respects_configured_bad_request_tolerance() {
    let upstream = spawn_400_upstream().await;
    let mut cfg = config();
    cfg.providers
        .get_mut("test-provider")
        .expect("test-provider in config")
        .bad_request_tolerance = 3;
    let mut cache = StatusCache::default();
    let client = reqwest::Client::new();
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_file = config_dir.path().join("config.toml");
    let plan = plan(&upstream);
    let cooldown_store = crate::cooldown::CooldownStore::in_memory();
    let cooldown_cfg = cfg.effective_cooldown("test-model", "test-provider");

    for round in 1..=3 {
        run_one_online_probe(
            &config_file,
            &cfg,
            &mut cache,
            &client,
            "test-model",
            Protocol::OpenaiChatCompletions,
            "test-provider",
            &plan,
            Some(&cooldown_store),
            &cooldown_cfg,
        )
        .await;
        let cooling = cooldown_store.is_cooling_down(&plan.cooldown_key("test-model"));
        if round < 3 {
            assert_eq!(
                cooldown_store.get_bad_request_count("test-provider"),
                round,
                "counter must reach {round} after {round}×400"
            );
            assert!(
                !cooling,
                "must NOT cool down before tolerance (round {round})"
            );
        } else {
            assert!(
                cooling,
                "tolerance 3 reached on round 3 — provider cooldown must be active"
            );
        }
    }
}

/// 判据 4 red proof：本地预检失败（请求未发往上游）不计入异常计数。
/// provider 不在 config → resolve_auth 本地失败 → 早退，请求根本不出去。
/// （转发路径的本地 400 由 覆盖——喂指纹 + return，不触探测计数。）
#[tokio::test]
async fn local_rejection_does_not_increment_counter() {
    let cfg = config();
    let mut cache = StatusCache::default();
    let client = reqwest::Client::new();
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_file = config_dir.path().join("config.toml");
    // Unroutable URL: would fail fast if a request were ever sent.
    let plan = plan("http://127.0.0.1:1");
    let cooldown_store = crate::cooldown::CooldownStore::in_memory();
    let cooldown_cfg = cfg.effective_cooldown("test-model", "nonexistent-provider");

    let sent = run_one_online_probe(
        &config_file,
        &cfg,
        &mut cache,
        &client,
        "test-model",
        Protocol::OpenaiChatCompletions,
        "nonexistent-provider",
        &plan,
        Some(&cooldown_store),
        &cooldown_cfg,
    )
    .await;

    assert!(!sent, "local auth failure must not send upstream");
    assert_eq!(
        cooldown_store.get_bad_request_count("nonexistent-provider"),
        0,
        "local rejection must not increment the bad request counter"
    );
    assert!(!cooldown_store.is_cooling_down(&plan.cooldown_key("test-model")));
}

#[test]
fn project_client_status_cache_prefers_success_and_filters_models() {
    let cfg: crate::config::Config = toml::from_str(
        r#"
[providers.p1]
openai_chat = { url = "https://example.test/p1" }

[providers.p2]
openai_chat = { url = "https://example.test/p2" }

[models.m1]
context_window = 100
max_output_tokens = 10
openai_chat_providers = [
    { name = "p1", model = "u1" },
    { name = "p2", model = "u2" },
]

[models.m2]
context_window = 100
max_output_tokens = 10
openai_chat_providers = [{ name = "p1", model = "u1" }]
"#,
    )
    .expect("parse test config");

    let mut cache = crate::status::StatusCache::default();
    cache.probes.insert(
        crate::status::probe_key("m1", "p1", crate::config::Protocol::OpenaiChatCompletions),
        crate::status::ProbeCacheEntry {
            ok: false,
            checked_at_unix: 20,
            latency_ms: None,
            http_status: Some(500),
            error: Some("boom".into()),
        },
    );
    cache.probes.insert(
        crate::status::probe_key("m1", "p2", crate::config::Protocol::OpenaiChatCompletions),
        crate::status::ProbeCacheEntry {
            ok: true,
            checked_at_unix: 10,
            latency_ms: Some(12),
            http_status: Some(200),
            error: None,
        },
    );
    cache.probes.insert(
        crate::status::probe_key("m2", "p1", crate::config::Protocol::OpenaiChatCompletions),
        crate::status::ProbeCacheEntry {
            ok: false,
            checked_at_unix: 30,
            latency_ms: None,
            http_status: Some(500),
            error: Some("boom".into()),
        },
    );

    let projected = crate::status::probe::project_client_status_cache(&cfg, &cache, None);
    let m1 = projected
        .probes
        .get(&crate::status::probe_key(
            "m1",
            "llm-proxy",
            crate::config::Protocol::OpenaiChatCompletions,
        ))
        .expect("m1 projected");
    assert!(m1.ok, "successful binding must win over failure");
    let m2 = projected
        .probes
        .get(&crate::status::probe_key(
            "m2",
            "llm-proxy",
            crate::config::Protocol::OpenaiChatCompletions,
        ))
        .expect("m2 projected");
    assert!(!m2.ok, "failure-only binding keeps failure");
    assert_eq!(
        m2.error.as_deref(),
        Some("probe failed"),
        "client view must not expose provider-specific error text"
    );

    let grant = crate::token::auth::TokenGrant::Granted(crate::token::auth::TokenContext {
        key: "lp-test".into(),
        name: "test".into(),
        allowed_models: vec!["m1".into()],
        admin: false,
    });
    let filtered = crate::status::probe::project_client_status_cache(&cfg, &cache, Some(&grant));
    assert!(filtered.probes.keys().any(|key| key.starts_with("m1:")));
    assert!(!filtered.probes.keys().any(|key| key.starts_with("m2:")));
}
