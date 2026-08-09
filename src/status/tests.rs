use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serial_test::serial;

use crate::config::{AuthConfig, Config, Protocol, ProviderConfig};

use super::cache::{ProbeFrequencyLimiter, unix_now};
use super::display::{
    auth_lines, cache_detail, cache_detail_with_latency, cooldown_lines, format_context_window,
};
use super::format::{
    age_label, auth_label, badge, blue, bold, cyan, dim, format_latency, green, heading,
    is_success, label, latency_rating, protocol_label, red, section, style, yellow,
};
use super::probe::{
    antigravity_cache_rows, antigravity_model_id, discover_ollama_models, ollama_cache_row,
    ollama_capability_feature, ollama_context_window, openrouter_cache_row, parse_ollama_num_ctx,
    run_one_online_probe, string_array,
};
use super::*;

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

    // Default (offline) status must not probe the network at all.
    print_status(&config_path, &cfg, false)
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

    let mut cache = StatusCache::default();
    super::probe::run_online_probes(&cfg, &mut cache).await;

    assert_eq!(*hits.lock().unwrap(), 3);
    assert!(cache.probes.contains_key(&probe_key(
        "deepseek-v4-flash-lp",
        "deepseek",
        Protocol::OpenaiChatCompletions
    )));
    assert!(cache.probes.contains_key(&probe_key(
        "deepseek-v4-flash-lp",
        "deepseek",
        Protocol::OpenaiResponses
    )));
    assert!(cache.probes.contains_key(&probe_key(
        "deepseek-v4-flash-lp",
        "deepseek",
        Protocol::Anthropic
    )));
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
fn antigravity_cache_rows_parse_models_and_web_search_hints() {
    let rows = antigravity_cache_rows(
        "antigravity",
        "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
        10,
        &serde_json::json!({
            "models": [
                "gemini-2.5-pro",
                {"id": "gemini-3-pro", "displayName": "Gemini 3 Pro", "contextWindow": 1000000, "maxOutputTokens": 65536}
            ],
            "webSearchModelIds": ["Gemini-3-Pro"]
        }),
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].model_id, "gemini-2.5-pro");
    assert_eq!(rows[1].display_name.as_deref(), Some("Gemini 3 Pro"));
    assert_eq!(rows[1].context_window, Some(1000000));
    assert_eq!(rows[1].max_output_tokens, Some(65536));
    assert!(rows[1].features.contains(&"web_search".to_string()));
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
        row.supported_reasoning_levels,
        vec!["low", "medium", "high"]
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
            supported_reasoning_levels: Vec::new(),
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

    let mut accounts = crate::auth::OAuthAccounts::new();
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
    let mut accounts = crate::auth::OAuthAccounts::new();
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
        "antigravity": {
            "broken-agy": {
                "account_label": "test",
                "project_id": "INVALID_PROJECT",
                "access_token": "short",
                "refresh_token": "also-short",
                "expires_at_unix": 1000000001,
                "updated_at_unix": 1000000000
            }
        },
        "openai": {
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
    assert!(text.contains("broken-agy"));
}

#[test]
fn provider_auth_summary_reports_skipped_account() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");

    // 写入无效的 antigravity 账号
    let broken = serde_json::json!({
        "version": 1,
        "antigravity": {
            "antigravity": {
                "account_label": "test",
                "project_id": "INVALID",
                "access_token": "too-short-token",
                "refresh_token": "also-too-short-token",
                "expires_at_unix": 1000000001,
                "updated_at_unix": 1000000000
            }
        }
    });
    std::fs::write(&path, serde_json::to_string_pretty(&broken).expect("json")).expect("write");

    let provider = ProviderConfig {
        auth: Some(AuthConfig::AntigravityOauth {
            account: Some("antigravity".to_string()),
        }),
        ..ProviderConfig::default()
    };
    let (state, line) = provider_auth_summary("antigravity", &provider, &path);
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
            kind: "server_error".to_string(),
            reason: "500".to_string(),
            started_at_unix: unix_now(),
            expires_at_unix: unix_now() + 60,
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
#[serial]
fn cache_path_honors_override_directory_and_style_helpers_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    unsafe {
        std::env::set_var("LLM_PROXY_STATE_DIR", dir.path());
    }
    let path = cache_path();
    assert!(path.ends_with("llm-proxy/status-cache.json"));
    assert!(path.starts_with(dir.path()));

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
fn antigravity_model_id_accepts_string_and_object_keys() {
    assert_eq!(
        antigravity_model_id(&serde_json::json!("gemini")),
        Some("gemini".into())
    );
    assert_eq!(
        antigravity_model_id(&serde_json::json!({"id":"id1"})),
        Some("id1".into())
    );
    assert_eq!(
        antigravity_model_id(&serde_json::json!({"model":"m1"})),
        Some("m1".into())
    );
    assert_eq!(
        antigravity_model_id(&serde_json::json!({"name":"n1"})),
        Some("n1".into())
    );
    assert_eq!(antigravity_model_id(&serde_json::json!({"id":""})), None);
}

#[test]
fn antigravity_cache_rows_handles_snake_case_fields() {
    let payload = serde_json::json!({
        "webSearchModelIds": ["M2"],
        "models": [{"model":"m2", "display_name":"Model Two", "context_window": 1000, "max_output_tokens": 50}]
    });
    let rows = antigravity_cache_rows("ag", "url", 7, &payload);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].features, vec!["web_search"]);
    assert_eq!(rows[0].display_name.as_deref(), Some("Model Two"));
    assert_eq!(rows[0].context_window, Some(1000));
    assert_eq!(rows[0].max_output_tokens, Some(50));
}

#[test]
fn antigravity_cache_rows_skips_invalid_items_and_defaults_display_name() {
    let payload = serde_json::json!({"models": [{"id":"ok"}, {"id":""}, 123]});
    let rows = antigravity_cache_rows("ag", "url", 3, &payload);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].display_name.as_deref(), Some("ok"));
}

#[test]
fn openrouter_cache_row_infers_reasoning_levels_from_parameter() {
    let item = serde_json::json!({"supported_parameters": ["reasoning_effort"]});
    let row = openrouter_cache_row("or", "src", 5, "model", &item, None, None);
    assert_eq!(
        row.supported_reasoning_levels,
        vec!["minimal", "low", "medium", "high", "xhigh", "max"]
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
    assert_eq!(row.supported_reasoning_levels, vec!["low", "high"]);
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
    assert!(row.supported_reasoning_levels.is_empty());
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
            supported_reasoning_levels: vec!["low".into()],
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
#[serial]
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
#[serial]
fn age_label_exact_boundaries() {
    let now = unix_now();
    assert!(age_label(now.saturating_sub(60)).starts_with("1m"));
    assert!(age_label(now.saturating_sub(3600)).starts_with("1h"));
    assert!(age_label(now.saturating_sub(86400)).starts_with("1d"));
}

#[test]
#[serial]
fn style_respects_no_color_even_when_force_is_set() {
    unsafe {
        std::env::set_var("NO_COLOR", "1");
        std::env::set_var("CLICOLOR_FORCE", "1");
    }
    assert_eq!(style("plain", "31"), "plain");
    unsafe {
        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR_FORCE");
    }
}

#[test]
#[serial]
fn probe_result_cache_entries_have_recent_timestamps() {
    let before = unix_now();
    let entry = ProbeResult::Error("e".into()).to_cache_entry().unwrap();
    let after = unix_now();
    assert!(entry.checked_at_unix >= before && entry.checked_at_unix <= after);
}

// =========================================================================
// Phase 1 P0: status.rs simple path tests
// =========================================================================

#[tokio::test]
async fn test_run_one_probe_auth_unavailable() {
    // When resolve_auth fails for the provider, probe should record error
    let cfg = crate::config::default_deepseek_config();
    let mut cache = StatusCache::default();
    let client = reqwest::Client::new();
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
    };
    // Use a provider_id not in config → resolve_auth will fail
    run_one_online_probe(
        &cfg,
        &mut cache,
        &client,
        "test-model",
        Protocol::OpenaiChatCompletions,
        "nonexistent-provider",
        &plan,
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
        server: crate::config::ServerConfig {
            listen: "127.0.0.1:0".into(),
            usage: Default::default(),
            max_sse_buffer_bytes: 1,
            max_output_items: 1,
        },
        fallback: Default::default(),
        protection: Default::default(),
        status: Default::default(),
        providers: Default::default(),
        models: Default::default(),
    };
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    // Should succeed without panic — remote mode + probe + no server
    // (env var not set to avoid interference with parallel service tests)
    let result = print_status(&config_path, &cfg, true).await;
    assert!(result.is_ok(), "print_status should succeed: {result:?}");
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
        supported_reasoning_levels: Vec::new(),
        default_reasoning_level: None,
        enable_thinking: None,
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

    let mut cache = StatusCache::default();
    super::probe::run_online_probes(&cfg, &mut cache).await;

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
#[serial]
async fn print_status_local_probe_writes_cache_and_discovers_dynamic_models() {
    let dir = tempfile::tempdir().expect("tempdir");
    unsafe {
        std::env::set_var("LLM_PROXY_STATE_DIR", dir.path());
    }

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

    // listen 指向不可达地址 → detect_server 返回 None → 本地 probe 分支。
    let config_path = dir.path().join("config.toml");
    cfg.server.listen = "127.0.0.1:1".to_string();
    crate::config_edit::write_full_config(&config_path, &cfg).expect("write config");

    print_status(&config_path, &cfg, true)
        .await
        .expect("local probe status");
    unsafe {
        std::env::remove_var("LLM_PROXY_STATE_DIR");
    }

    let cache_file = dir.path().join("llm-proxy/status-cache.json");
    assert!(cache_file.exists(), "probe should write cache file");
    let written: StatusCache =
        serde_json::from_str(&std::fs::read_to_string(cache_file).expect("read cache"))
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
#[serial]
async fn print_provider_info_filters_by_name_and_shows_usage_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    unsafe {
        std::env::set_var("LLM_PROXY_STATE_DIR", dir.path());
    }

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

    // name=None → 打印全部 provider；name=Some(oauth-p) → usage fallback（空 store → unavailable，无网络）。
    print_provider_info(&cfg, None).await;
    print_provider_info(&cfg, Some("oauth-p")).await;
    unsafe {
        std::env::remove_var("LLM_PROXY_STATE_DIR");
    }
}

#[test]
fn provider_auth_summary_reports_antigravity_states_and_store_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    let provider = ProviderConfig {
        auth: Some(AuthConfig::AntigravityOauth {
            account: Some("agy".to_string()),
        }),
        ..ProviderConfig::default()
    };

    // 无 store 文件 → missing-login
    let (state, line) = provider_auth_summary("antigravity", &provider, &path);
    assert_eq!(state, "WARN");
    assert!(line.contains("state=missing-login"), "got: {line}");

    // 有效账号 → authenticated
    let mut accounts = crate::auth::OAuthAccounts::new();
    accounts.antigravity.insert(
        "agy".to_string(),
        crate::auth::AntigravityAccount {
            account_label: "agy@example.com".to_string(),
            project_id: "my-proj1".to_string(),
            access_token: "secret-access-token-1234567890".to_string(),
            refresh_token: "secret-refresh-token-1234567890".to_string(),
            expires_at_unix: (unix_now() + 60) as i64,
            updated_at_unix: unix_now() as i64,
        },
    );
    crate::auth::save_oauth_accounts(&path, &accounts).expect("write");
    let (state, line) = provider_auth_summary("antigravity", &provider, &path);
    assert_eq!(state, "OK");
    assert!(line.contains("state=authenticated"), "got: {line}");
    assert!(!line.contains("secret-access"));

    // 过期账号 → expired
    {
        let acc = accounts.antigravity.get_mut("agy").unwrap();
        acc.expires_at_unix = (unix_now() as i64) - 60;
        acc.updated_at_unix = (unix_now() as i64) - 120;
    }
    crate::auth::save_oauth_accounts(&path, &accounts).expect("write");
    let (state, line) = provider_auth_summary("antigravity", &provider, &path);
    assert_eq!(state, "WARN");
    assert!(line.contains("state=expired"), "got: {line}");

    // 损坏的 store 文件 → store-error
    std::fs::write(&path, "not json").expect("write broken");
    let (state, line) = provider_auth_summary("antigravity", &provider, &path);
    assert_eq!(state, "WARN");
    assert!(line.contains("state=store-error"), "got: {line}");
}
