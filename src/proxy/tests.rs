use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use axum::extract::State;
use serial_test::serial;

use super::*;
use crate::config;

/// 隔离 status cache：设置唯一的 LLM_PROXY_STATE_DIR（测试专用目录），
/// 避免测试读写与运行中 server 共享的 status-cache.json（edition 2024 set_var 为 unsafe）。
fn isolate_status_cache() {
    let dir = std::env::temp_dir().join(format!("llm-proxy-test-{}", std::process::id()));
    unsafe {
        std::env::set_var("LLM_PROXY_STATE_DIR", dir);
    }
    // 清理该目录内残留 cache，保证每个测试从空缓存开始
    let _ = std::fs::remove_file(crate::status::cache_path());
}

type CapturedRequest = Arc<Mutex<Option<Value>>>;
type CapturedHeader = Arc<Mutex<Option<String>>>;
type CapturedRequestAndHeader = (CapturedRequest, CapturedHeader);

fn test_execution_plan(url: String) -> config::ExecutionPlan {
    config::ExecutionPlan {
        frontend_protocol: Protocol::OpenaiChatCompletions,
        provider_id: "test-provider".to_string(),
        upstream_model: "m".to_string(),
        source_protocol: Protocol::OpenaiChatCompletions,
        adapter: AdapterKind::Passthrough,
        native_url: url,
        auth: config::AuthConfig::None,
        compat: config::CompatConfig::default(),
        anthropic_family_models: Vec::new(),
        store: None,
        request_frequency: config::RequestFrequencyConfig {
            requests_per_minute: Some(60),
            requests_per_hour: None,
            burst: Some(60),
            queue_timeout_seconds: Some(1),
        },
    }
}

/// Point every native endpoint of a provider at the mock upstream,
/// preserving each endpoint's URL path.
fn repoint_provider_urls(provider: &mut config::ProviderConfig, addr: std::net::SocketAddr) {
    let base = format!("http://{addr}");
    for endpoint in [
        provider.openai_chat.as_mut(),
        provider.openai_responses.as_mut(),
        provider.anthropic.as_mut(),
        provider.antigravity.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(url) = &mut endpoint.url {
            let path = url::Url::parse(url)
                .map(|parsed| parsed.path().to_string())
                .unwrap_or_default();
            *url = format!("{base}{path}");
        }
    }
}

#[test]
fn active_provider_tracking_obeys_ttl() {
    // §12.3/12.9：成功转发 = 活跃证据；TTL 窗口内活跃、窗口外不活跃
    let cfg = crate::config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);
    state.mark_provider_active("deepseek");
    assert!(state.is_provider_active("deepseek", Duration::from_secs(30)));
    assert!(!state.is_provider_active("unknown", Duration::from_secs(30)));
    assert_eq!(
        state.get_active_providers(Duration::from_secs(30)),
        vec!["deepseek".to_string()]
    );
    // TTL=0（零宽窗口）→ 不活跃
    assert!(!state.is_provider_active("deepseek", Duration::ZERO));
}

#[test]
fn active_provider_records_are_marked_by_record_usage() {
    // record_usage 成功路径应同时标记活跃（§12.3）
    let cfg = crate::config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);
    state.record_usage(
        "deepseek-chat".to_string(),
        "deepseek".to_string(),
        "https://api.deepseek.com/v1/chat/completions".to_string(),
        10,
        5,
        None,
    );
    assert!(state.is_provider_active("deepseek", Duration::from_secs(30)));
}

#[tokio::test]
async fn antigravity_responses_stream_aggregates_tool_call_chunks() {
    let upstream_app = Router::new().route(
        "/stream",
        get(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"lookup\",\"args\":{\"q\":\"x\"}}}]}}]}}\n\n",
                    "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"lookup\",\"args\":{\"limit\":1}}}]}}]}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, upstream_app).await.expect("serve") });
    let upstream = reqwest::Client::new()
        .get(format!("http://{addr}/stream"))
        .send()
        .await
        .expect("upstream");
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let response = antigravity_sse_to_responses_sse(
        &state,
        upstream,
        "frontend".to_string(),
        "deepseek".to_string(),
        "openai_responses".to_string(),
    )
    .await;
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("bytes")
            .to_vec(),
    )
    .expect("utf8");

    assert_eq!(body.matches("event: response.output_item.added").count(), 3); // message + two function_calls
    assert_eq!(
        body.matches("event: response.function_call_arguments.done")
            .count(),
        2
    );
    assert_eq!(body.matches("\"type\":\"function_call\"").count(), 6); // per call: added item + done arguments + completed output = 3; ×2 calls
    // Each streaming functionCall chunk is an independent call with its
    // COMPLETE args (antigravity never splits a call across chunks), so
    // the two same-named calls must NOT be merged.
    assert!(body.contains("{\\\"q\\\":\\\"x\\\"}"));
    assert!(body.contains("{\\\"limit\\\":1}"));
    assert!(!body.contains("{\\\"q\\\":\\\"x\\\"}{\\\"limit\\\":1}"));
}

#[tokio::test]
async fn antigravity_anthropic_stream_aggregates_tool_call_chunks() {
    let upstream_app = Router::new().route(
        "/stream",
        get(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"lookup\",\"args\":{\"q\":\"x\"}}}]}}]}}\n\n",
                    "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"lookup\",\"args\":{\"limit\":1}}}]}}]}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, upstream_app).await.expect("serve") });
    let upstream = reqwest::Client::new()
        .get(format!("http://{addr}/stream"))
        .send()
        .await
        .expect("upstream");
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let response = antigravity_sse_to_anthropic_sse(
        &state,
        upstream,
        "frontend".to_string(),
        "deepseek".to_string(),
        "anthropic".to_string(),
    )
    .await;
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("bytes")
            .to_vec(),
    )
    .expect("utf8");

    assert_eq!(body.matches("event: content_block_start").count(), 3); // initial text + two tool_use
    assert_eq!(body.matches("event: content_block_stop").count(), 3); // text + two tool_use
    assert!(body.contains("\"name\":\"lookup\""));
    // Two independent same-named calls, each with COMPLETE args (not merged).
    assert!(body.contains("{\\\"q\\\":\\\"x\\\"}"));
    assert!(body.contains("{\\\"limit\\\":1}"));
    assert!(!body.contains("{\\\"q\\\":\\\"x\\\"}{\\\"limit\\\":1}"));
}

#[test]
fn management_state_includes_stream_interruption_counters() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    state.observe_stream_interruption("responses-to-chat", "broken pipe");
    state.observe_stream_interruption("responses-to-chat", "reset");

    let snapshot = state.management_state_json();
    let entries = snapshot["stream_interruptions"]
        .as_array()
        .expect("entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["direction"], "responses-to-chat");
    assert_eq!(entries[0]["count"], 2);
    assert_eq!(entries[0]["last_error"], "reset");
}

#[tokio::test]
async fn model_listing_paths_filter_by_protocol_and_shape() {
    let mut cfg = config::default_deepseek_config();
    cfg.models.insert(
        "chat-only".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 1000,
            max_output_tokens: 100,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "deepseek".to_string(),
                model: "chat-only-upstream".to_string(),
            }],
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
            reasoning_level_map: None,
        },
    );
    let app = router(AppState::new_in_memory(cfg));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    let client = reqwest::Client::new();

    let openai: Value = client
        .get(format!("http://{addr}/openai/v1/models"))
        .send()
        .await
        .expect("openai models")
        .json()
        .await
        .expect("openai json");
    assert!(model_list_ids(&openai).contains(&"chat-only".to_string()));

    let responses: Value = client
        .get(format!("http://{addr}/responses/v1/models"))
        .send()
        .await
        .expect("responses models")
        .json()
        .await
        .expect("responses json");
    assert!(!model_list_ids(&responses).contains(&"chat-only".to_string()));

    let anthropic: Value = client
        .get(format!("http://{addr}/anthropic/v1/models"))
        .send()
        .await
        .expect("anthropic models")
        .json()
        .await
        .expect("anthropic json");
    assert!(anthropic["data"].as_array().is_some());
    assert!(!model_list_ids(&anthropic).contains(&"chat-only".to_string()));
}

fn model_list_ids(value: &Value) -> Vec<String> {
    value["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["id"].as_str().map(ToString::to_string))
        .collect()
}

#[tokio::test]
async fn send_upstream_retries_network_errors_uniformly_before_first_byte() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("addr");
    let attempts_for_task = Arc::clone(&attempts);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            attempts_for_task.fetch_add(1, Ordering::SeqCst);
            // 每个连接在独立 task 里 sleep，不阻塞 accept 循环
            tokio::spawn(async move {
                // 保持连接但不发送数据，触发超时（方案 B）
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                drop(socket);
            });
        }
    });

    let mut cfg = config::default_deepseek_config();
    cfg.fallback.max_retries = 2;
    cfg.fallback.timeout_seconds = 1;
    let state = AppState::new_in_memory(cfg);
    let url = format!("http://{addr}/chat/completions");
    let plan = test_execution_plan(url.clone());
    let req = state
        .client
        .post(url)
        .json(&json!({"model":"m","messages":[]}));
    assert!(send_upstream(&state, &plan, req).await.is_err());
    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    // §10.5.5: pre-first-byte network errors retry the same way for
    // streaming requests.
    attempts.store(0, Ordering::SeqCst);
    let url = format!("http://{addr}/chat/completions");
    let plan = test_execution_plan(url.clone());
    let req = state
        .client
        .post(url)
        .json(&json!({"model":"m","messages":[],"stream":true}));
    assert!(send_upstream(&state, &plan, req).await.is_err());
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn provider_frequency_limit_blocks_before_hitting_upstream() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_handler = Arc::clone(&attempts);
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let attempts = Arc::clone(&attempts_for_handler);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Json(json!({"ok": true}))
            }
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let cfg = config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);
    let url = format!("http://{upstream_addr}/chat/completions");
    let mut plan = test_execution_plan(url.clone());
    plan.request_frequency = config::RequestFrequencyConfig {
        requests_per_minute: Some(1),
        requests_per_hour: None,
        burst: Some(1),
        queue_timeout_seconds: Some(0),
    };

    let req = state
        .client
        .post(&url)
        .json(&json!({"model":"m","messages":[]}));
    send_upstream(&state, &plan, req)
        .await
        .expect("first allowed");
    let req = state
        .client
        .post(&url)
        .json(&json!({"model":"m","messages":[]}));
    let err = send_upstream(&state, &plan, req)
        .await
        .expect_err("second limited");
    assert!(matches!(err, UpstreamSendFailure::FrequencyLimited));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[test]
fn request_timeout_clamps_to_max_timeout_and_minimum_one_second() {
    let mut cfg = config::default_deepseek_config();
    cfg.fallback.timeout_seconds = 30;
    cfg.fallback.max_timeout_seconds = 5;
    assert_eq!(request_timeout(&cfg), Duration::from_secs(5));
    cfg.fallback.timeout_seconds = 0;
    cfg.fallback.max_timeout_seconds = 0;
    assert_eq!(request_timeout(&cfg), Duration::from_secs(1));
}

#[tokio::test]
async fn upstream_send_failure_response_marks_local_frequency_limit_header() {
    let limited = upstream_send_failure_response(UpstreamSendFailure::FrequencyLimited, json_error);
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(is_local_frequency_limited_response(&limited));

    let transport = upstream_send_failure_response(
        UpstreamSendFailure::Transport(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get("http://127.0.0.1:1")
                .send()
                .await
                .expect_err("transport error"),
        ),
        json_error,
    );
    assert_eq!(transport.status(), StatusCode::BAD_GATEWAY);
    assert!(!is_local_frequency_limited_response(&transport));
    // 错误消息应包含 "upstream request failed" 前缀（放宽断言，不依赖 reqwest 具体错误格式）
    let body = axum::body::to_bytes(transport.into_body(), 1 << 20)
        .await
        .expect("read body");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("upstream request failed"),
        "transport error should contain 'upstream request failed', got: {body_str}"
    );
}

#[test]
fn default_reasoning_level_is_injected_when_client_omits_it() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    let chat = body_with_default_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","messages":[]}),
    );
    assert_eq!(chat["reasoning_effort"], "high");

    let responses = body_with_default_reasoning(
        &state,
        Protocol::OpenaiResponses,
        json!({"model":"deepseek-v4-flash-lp","input":"ping"}),
    );
    assert_eq!(responses["reasoning"]["effort"], "high");

    let explicit = body_with_default_reasoning(
        &state,
        Protocol::OpenaiResponses,
        json!({"model":"deepseek-v4-flash-lp","input":"ping","reasoning":{"effort":"low"}}),
    );
    assert_eq!(explicit["reasoning"]["effort"], "low");
}

#[test]
fn reasoning_level_map_rewrites_client_level_for_upstream() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());

    let mapped = body_with_mapped_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","messages":[],"reasoning_effort":"xhigh"}),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("map reasoning");

    assert_eq!(mapped["reasoning_effort"], "max");
}

#[test]
fn reasoning_level_map_null_rejects_disabled_level() {
    let mut cfg = config::default_deepseek_config();
    let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
    model.reasoning_level_map = Some(vec![config::ReasoningLevelMapping {
        level: "low".to_string(),
        api_value: None,
    }]);
    let state = AppState::new_in_memory(cfg);
    let plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());

    let err = body_with_mapped_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","messages":[],"reasoning_effort":"low"}),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect_err("disabled level");

    assert!(err.to_string().contains("disables upstream thinking"));
}

#[test]
fn enable_thinking_false_strips_reasoning_before_upstream() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .enable_thinking = Some(false);
    let state = AppState::new_in_memory(cfg);
    let plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());

    let defaulted = body_with_default_reasoning(
        &state,
        Protocol::OpenaiResponses,
        json!({"model":"deepseek-v4-flash-lp","input":"ping"}),
    );
    assert!(defaulted.get("reasoning").is_none());

    let mapped = body_with_mapped_reasoning(
        &state,
        Protocol::OpenaiResponses,
        json!({"model":"deepseek-v4-flash-lp","input":"ping","reasoning":{"effort":"high"}}),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("strip reasoning");
    assert!(mapped.get("reasoning").is_none());
}

#[tokio::test]
async fn responses_endpoint_forwards_to_chat_upstream() {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream_app =
        Router::new()
            .route(
                "/chat/completions",
                post(
                    |State(captured): State<Arc<Mutex<Option<Value>>>>,
                     Json(body): Json<Value>| async move {
                        *captured.lock().unwrap() = Some(body);
                        Json(json!({
                            "id": "chatcmpl_test",
                            "created": 123,
                            "choices": [{
                                "message": {
                                    "role": "assistant",
                                    "content": "pong"
                                }
                            }],
                            "usage": {
                                "prompt_tokens": 1,
                                "completion_tokens": 1,
                                "total_tokens": 2
                            }
                        }))
                    },
                ),
            )
            .with_state(captured_for_handler);
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let client = reqwest::Client::new();
    let response: Value = client
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "input": "ping",
            "stream": false
        }))
        .send()
        .await
        .expect("send proxy request")
        .json()
        .await
        .expect("parse proxy response");

    assert_eq!(response["model"], "deepseek-v4-pro-lp");
    assert_eq!(response["output"][0]["content"][0]["text"], "pong");

    let captured = captured
        .lock()
        .unwrap()
        .clone()
        .expect("captured upstream body");
    assert_eq!(captured["model"], "deepseek-v4-pro");
    assert_eq!(
        captured["messages"][0],
        json!({ "role": "user", "content": "ping" })
    );
}

#[tokio::test]
async fn streaming_chat_chunks_convert_to_responses_sse() {
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"id\":\"chatcmpl_1\",\"created\":123,\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"he\"}}]}\n\n",
                    "data: {\"id\":\"chatcmpl_1\",\"created\":123,\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let body = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "input": "ping",
            "stream": true
        }))
        .send()
        .await
        .expect("send proxy request")
        .text()
        .await
        .expect("read stream");

    assert!(body.contains("event: response.output_text.delta"));
    assert!(body.contains("\"delta\":\"he\""));
    assert!(body.contains("\"delta\":\"llo\""));
    assert!(body.contains("event: response.completed"));
    assert!(body.contains("\"total_tokens\":2"));
}

#[tokio::test]
async fn responses_native_passthrough_maps_model_and_preserves_body() {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream_app = Router::new()
        .route(
            "/v1/responses",
            post(
                |State(captured): State<Arc<Mutex<Option<Value>>>>,
                 Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "resp_native",
                        "object": "response",
                        "status": "completed",
                        "model": "gpt-5.5",
                        "output": [{"type":"message","content":[{"type":"output_text","text":"pong"}]}]
                    }))
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut provider = crate::catalog::openai_payg().provider;
    repoint_provider_urls(&mut provider, upstream_addr);
    provider.api_key_env = None;
    cfg.providers.insert("openai-payg".to_string(), provider);
    cfg.models.insert(
        "gpt-5.5-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 400_000,
            max_output_tokens: 128_000,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: Vec::new(),
            openai_responses_providers: vec![config::ProviderBinding {
                name: "openai-payg".to_string(),
                model: "gpt-5.5".to_string(),
            }],
            anthropic_providers: Vec::new(),
        },
    );
    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model":"gpt-5.5-lp","input":"ping","stream":false}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(response["model"], "gpt-5.5-lp");
    assert_eq!(response["output"][0]["content"][0]["text"], "pong");
    let captured = captured.lock().unwrap().clone().expect("captured body");
    assert_eq!(captured["model"], "gpt-5.5");
    // egress adaptation: string input is normalized to a list, store is
    // filled with the standard default (set-if-absent).
    assert_eq!(captured["input"][0]["content"][0]["text"], "ping");
    assert_eq!(captured["store"], json!(false));
}

#[tokio::test]
async fn responses_native_streaming_passthrough_maps_request_model() {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream_app = Router::new()
        .route(
            "/v1/responses",
            post(
                |State(captured): State<Arc<Mutex<Option<Value>>>>,
                 Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.5\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.5\"},\"model\":\"gpt-5.5\"}\n\n",
                    )
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut provider = crate::catalog::openai_payg().provider;
    repoint_provider_urls(&mut provider, upstream_addr);
    provider.api_key_env = None;
    cfg.providers.insert("openai-payg".to_string(), provider);
    cfg.models.insert(
        "gpt-5.5-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 400_000,
            max_output_tokens: 128_000,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: Vec::new(),
            openai_responses_providers: vec![config::ProviderBinding {
                name: "openai-payg".to_string(),
                model: "gpt-5.5".to_string(),
            }],
            anthropic_providers: Vec::new(),
        },
    );
    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let body = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model":"gpt-5.5-lp","input":"ping","stream":true}))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");
    assert!(body.contains("response.completed"));
    assert!(body.contains("gpt-5.5-lp"));
    assert!(!body.contains(r#""model":"gpt-5.5""#));
    let captured = captured.lock().unwrap().clone().expect("captured body");
    assert_eq!(captured["model"], "gpt-5.5");
    assert_eq!(captured["stream"], true);
}

#[tokio::test]
async fn anthropic_sse_converts_to_chat_sse() {
    let upstream_app = Router::new().route(
        "/v1/messages",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"he\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"llo\"}}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut anthropic = crate::catalog::anthropic().provider;
    repoint_provider_urls(&mut anthropic, upstream_addr);
    anthropic.api_key_env = None;
    cfg.providers.insert("anthropic".to_string(), anthropic);
    cfg.models.insert(
        "claude-sonnet-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 200_000,
            max_output_tokens: 64_000,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "anthropic".to_string(),
                model: "claude-sonnet-4-8".to_string(),
            }],
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
        },
    );
    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let body = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({"model":"claude-sonnet-lp","messages":[{"role":"user","content":"ping"}],"stream":true}))
        .send().await.expect("send").text().await.expect("text");
    assert!(body.contains("\"content\":\"he\""));
    assert!(body.contains("\"content\":\"llo\""));
    assert!(body.contains("data: [DONE]"));
    assert!(body.contains("\"total_tokens\":3"));
}

#[tokio::test]
async fn anthropic_sse_converts_to_responses_sse() {
    let upstream_app = Router::new().route(
        "/v1/messages",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut anthropic = crate::catalog::anthropic().provider;
    repoint_provider_urls(&mut anthropic, upstream_addr);
    anthropic.api_key_env = None;
    cfg.providers.insert("anthropic".to_string(), anthropic);
    cfg.models.insert(
        "claude-sonnet-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 200_000,
            max_output_tokens: 64_000,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: Vec::new(),
            openai_responses_providers: vec![config::ProviderBinding {
                name: "anthropic".to_string(),
                model: "claude-sonnet-4-8".to_string(),
            }],
            anthropic_providers: Vec::new(),
        },
    );
    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let body = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model":"claude-sonnet-lp","input":"ping","stream":true}))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");
    assert!(body.contains("event: response.output_text.delta"));
    assert!(body.contains("\"delta\":\"hi\""));
    assert!(body.contains("event: response.completed"));
    assert!(body.contains("\"total_tokens\":3"));
}

fn anthropic_tool_use_sse() -> &'static str {
    concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Beijing\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    )
}

#[tokio::test]
async fn streaming_chat_tool_calls_convert_to_anthropic_tool_use() {
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n",
                    "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
                    "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Beijing\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":5,\"total_tokens\":6}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;
    provider.anthropic = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiChatCompletions,
    ));

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let body = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "messages": [{"role": "user", "content": "weather?"}],
            "max_tokens": 64,
            "stream": true
        }))
        .send()
        .await
        .expect("send proxy request")
        .text()
        .await
        .expect("read stream");

    assert!(body.contains("\"type\":\"tool_use\""));
    assert!(body.contains("\"name\":\"get_weather\""));
    assert!(body.contains("\"type\":\"input_json_delta\""));
    assert!(body.contains("\"partial_json\":\"{\\\"city\\\":\""));
    assert!(body.contains("\"partial_json\":\"\\\"Beijing\\\"}\""));
    assert!(body.contains("\"stop_reason\":\"tool_use\""));
}

#[tokio::test]
async fn streaming_anthropic_tool_use_converts_to_chat_tool_calls() {
    let upstream_app = Router::new().route(
        "/v1/messages",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                anthropic_tool_use_sse(),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut anthropic = crate::catalog::anthropic().provider;
    repoint_provider_urls(&mut anthropic, upstream_addr);
    anthropic.api_key_env = None;
    cfg.providers.insert("anthropic".to_string(), anthropic);
    cfg.models.insert(
        "claude-sonnet-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 200_000,
            max_output_tokens: 64_000,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "anthropic".to_string(),
                model: "claude-sonnet-4-8".to_string(),
            }],
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
        },
    );
    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let body = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({"model":"claude-sonnet-lp","messages":[{"role":"user","content":"weather?"}],"stream":true}))
        .send().await.expect("send").text().await.expect("text");

    assert!(body.contains("\"tool_calls\""));
    assert!(body.contains("\"name\":\"get_weather\""));
    assert!(body.contains("\"arguments\":\"{\\\"city\\\":\""));
    assert!(body.contains("\"arguments\":\"\\\"Beijing\\\"}\""));
    assert!(body.contains("\"finish_reason\":\"tool_calls\""));
    assert!(body.contains("data: [DONE]"));
}

#[tokio::test]
async fn streaming_anthropic_tool_use_converts_to_responses_function_call() {
    let upstream_app = Router::new().route(
        "/v1/messages",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                anthropic_tool_use_sse(),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut anthropic = crate::catalog::anthropic().provider;
    repoint_provider_urls(&mut anthropic, upstream_addr);
    anthropic.api_key_env = None;
    cfg.providers.insert("anthropic".to_string(), anthropic);
    cfg.models.insert(
        "claude-sonnet-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 200_000,
            max_output_tokens: 64_000,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: Vec::new(),
            openai_responses_providers: vec![config::ProviderBinding {
                name: "anthropic".to_string(),
                model: "claude-sonnet-4-8".to_string(),
            }],
            anthropic_providers: Vec::new(),
        },
    );
    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let body = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model":"claude-sonnet-lp","input":"weather?","stream":true}))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");

    assert!(body.contains("event: response.output_item.added"));
    assert!(body.contains("\"type\":\"function_call\""));
    assert!(body.contains("\"name\":\"get_weather\""));
    assert!(body.contains("event: response.function_call_arguments.delta"));
    assert!(body.contains("event: response.function_call_arguments.done"));
    assert!(body.contains("event: response.output_item.done"));
    assert!(body.contains("event: response.completed"));
    // The completed response output must include the function_call item.
    assert!(body.contains("\"arguments\":\"{\\\"city\\\":\\\"Beijing\\\"}\""));
}

#[tokio::test]
async fn chat_endpoint_forwards_to_anthropic_upstream() {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream_app =
        Router::new()
            .route(
                "/v1/messages",
                post(
                    |State(captured): State<Arc<Mutex<Option<Value>>>>,
                     Json(body): Json<Value>| async move {
                        *captured.lock().unwrap() = Some(body);
                        Json(json!({
                            "id": "msg_chat_anthropic",
                            "type": "message",
                            "role": "assistant",
                            "model": "claude-sonnet-4-8",
                            "content": [{"type": "text", "text": "pong"}],
                            "stop_reason": "end_turn",
                            "usage": {"input_tokens": 2, "output_tokens": 3}
                        }))
                    },
                ),
            )
            .with_state(captured_for_handler);
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut anthropic = crate::catalog::anthropic().provider;
    repoint_provider_urls(&mut anthropic, upstream_addr);
    anthropic.api_key_env = None;
    cfg.providers.insert("anthropic".to_string(), anthropic);
    cfg.models.insert(
        "claude-sonnet-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 200_000,
            max_output_tokens: 64_000,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "anthropic".to_string(),
                model: "claude-sonnet-4-8".to_string(),
            }],
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
        },
    );

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "claude-sonnet-lp",
            "messages": [
                {"role": "system", "content": "be concise"},
                {"role": "user", "content": "ping"}
            ],
            "max_tokens": 64,
            "stream": false
        }))
        .send()
        .await
        .expect("send proxy request")
        .json()
        .await
        .expect("parse proxy response");

    assert_eq!(response["model"], "claude-sonnet-lp");
    assert_eq!(response["choices"][0]["message"]["content"], "pong");
    let captured = captured.lock().unwrap().clone().expect("captured body");
    assert_eq!(captured["model"], "claude-sonnet-4-8");
    assert_eq!(captured["system"], "be concise");
    assert_eq!(captured["messages"][0]["content"][0]["text"], "ping");
}

#[tokio::test]
async fn responses_endpoint_forwards_to_anthropic_upstream() {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream_app =
        Router::new()
            .route(
                "/v1/messages",
                post(
                    |State(captured): State<Arc<Mutex<Option<Value>>>>,
                     Json(body): Json<Value>| async move {
                        *captured.lock().unwrap() = Some(body);
                        Json(json!({
                            "id": "msg_resp_anthropic",
                            "type": "message",
                            "role": "assistant",
                            "model": "claude-sonnet-4-8",
                            "content": [{"type": "text", "text": "pong"}],
                            "usage": {"input_tokens": 2, "output_tokens": 3}
                        }))
                    },
                ),
            )
            .with_state(captured_for_handler);
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut anthropic = crate::catalog::anthropic().provider;
    repoint_provider_urls(&mut anthropic, upstream_addr);
    anthropic.api_key_env = None;
    cfg.providers.insert("anthropic".to_string(), anthropic);
    cfg.models.insert(
        "claude-sonnet-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 200_000,
            max_output_tokens: 64_000,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: Vec::new(),
            openai_responses_providers: vec![config::ProviderBinding {
                name: "anthropic".to_string(),
                model: "claude-sonnet-4-8".to_string(),
            }],
            anthropic_providers: vec![config::ProviderBinding {
                name: "anthropic".to_string(),
                model: "claude-sonnet-4-8".to_string(),
            }],
        },
    );

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "claude-sonnet-lp",
            "instructions": "be concise",
            "input": "ping",
            "max_output_tokens": 64,
            "stream": false
        }))
        .send()
        .await
        .expect("send proxy request")
        .json()
        .await
        .expect("parse proxy response");

    assert_eq!(response["model"], "claude-sonnet-lp");
    assert_eq!(response["output"][0]["content"][0]["text"], "pong");
    assert_eq!(response["usage"]["total_tokens"], 5);
    let captured = captured.lock().unwrap().clone().expect("captured body");
    assert_eq!(captured["model"], "claude-sonnet-4-8");
    assert_eq!(captured["system"], "be concise");
    assert_eq!(captured["messages"][0]["content"][0]["text"], "ping");
}

#[tokio::test]
async fn anthropic_messages_endpoint_forwards_to_chat_upstream() {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream_app = Router::new()
        .route(
            "/chat/completions",
            post(
                |State(captured): State<Arc<Mutex<Option<Value>>>>,
                 Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "chatcmpl_anthropic",
                        "created": 123,
                        "choices": [{
                            "finish_reason": "stop",
                            "message": {"role": "assistant", "content": "pong"}
                        }],
                        "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
                    }))
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;
    // Exercise the anthropic-from-chat adapter: derive the anthropic
    // endpoint from chat instead of using the catalog-native one.
    provider.anthropic = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiChatCompletions,
    ));

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "system": "be concise",
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 64,
            "stream": false
        }))
        .send()
        .await
        .expect("send proxy request")
        .json()
        .await
        .expect("parse proxy response");

    assert_eq!(response["type"], "message");
    assert_eq!(response["model"], "deepseek-v4-pro-lp");
    assert_eq!(response["content"][0]["text"], "pong");
    assert_eq!(response["usage"]["output_tokens"], 3);

    let captured = captured
        .lock()
        .unwrap()
        .clone()
        .expect("captured upstream body");
    assert_eq!(captured["model"], "deepseek-v4-pro");
    assert_eq!(
        captured["messages"][0],
        json!({"role": "system", "content": "be concise"})
    );
    assert_eq!(
        captured["messages"][1],
        json!({"role": "user", "content": "ping"})
    );
}

#[tokio::test]
async fn anthropic_native_passthrough_uses_anthropic_headers_and_model_mapping() {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_header: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let header_for_handler = Arc::clone(&captured_header);
    let upstream_app = Router::new()
        .route(
            "/v1/messages",
            post(
                |State((captured, captured_header)): State<CapturedRequestAndHeader>,
                 headers: axum::http::HeaderMap,
                 Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    *captured_header.lock().unwrap() = headers
                        .get("anthropic-version")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    Json(json!({
                        "id": "msg_native",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-sonnet-4-8",
                        "content": [{"type": "text", "text": "pong"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 2, "output_tokens": 3}
                    }))
                },
            ),
        )
        .with_state((captured_for_handler, header_for_handler));
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut anthropic = crate::catalog::anthropic().provider;
    repoint_provider_urls(&mut anthropic, upstream_addr);
    anthropic.api_key_env = None;
    cfg.providers.insert("anthropic".to_string(), anthropic);
    cfg.models.insert(
        "claude-sonnet-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 200_000,
            max_output_tokens: 64_000,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: Vec::new(),
            openai_responses_providers: Vec::new(),
            anthropic_providers: vec![config::ProviderBinding {
                name: "anthropic".to_string(),
                model: "claude-sonnet-4-8".to_string(),
            }],
        },
    );

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "claude-sonnet-lp",
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 64,
            "stream": false
        }))
        .send()
        .await
        .expect("send proxy request")
        .json()
        .await
        .expect("parse proxy response");

    assert_eq!(response["model"], "claude-sonnet-lp");
    assert_eq!(response["content"][0]["text"], "pong");
    assert_eq!(
        captured.lock().unwrap().clone().expect("body")["model"],
        "claude-sonnet-4-8"
    );
    assert_eq!(
        captured_header.lock().unwrap().as_deref(),
        Some("2023-06-01")
    );
}

#[tokio::test]
async fn streaming_chat_chunks_convert_to_anthropic_sse() {
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"he\"}}]}\n\n",
                    "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;
    // Exercise the anthropic-from-chat adapter: derive the anthropic
    // endpoint from chat instead of using the catalog-native one.
    provider.anthropic = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiChatCompletions,
    ));

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let body = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 64,
            "stream": true
        }))
        .send()
        .await
        .expect("send proxy request")
        .text()
        .await
        .expect("read stream");

    assert!(body.contains("event: message_start"));
    assert!(body.contains("event: content_block_delta"));
    assert!(body.contains("\"text\":\"he\""));
    assert!(body.contains("\"text\":\"llo\""));
    assert!(body.contains("event: message_stop"));
}

#[tokio::test]
async fn responses_non_streaming_falls_back_to_next_provider_after_cooldown_error() {
    let first_hits = Arc::new(Mutex::new(0usize));
    let first_hits_handler = Arc::clone(&first_hits);
    let first_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let first_hits_handler = Arc::clone(&first_hits_handler);
            async move {
                *first_hits_handler.lock().unwrap() += 1;
                (StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }
        }),
    );
    let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind first upstream");
    let first_addr = first_listener.local_addr().expect("first upstream addr");
    tokio::spawn(async move {
        axum::serve(first_listener, first_app)
            .await
            .expect("serve first upstream");
    });

    let second_app = Router::new().route(
        "/chat/completions",
        post(|| async {
            Json(json!({
                "id": "chatcmpl_second",
                "created": 123,
                "choices": [{"message": {"role": "assistant", "content": "fallback ok"}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }))
        }),
    );
    let second_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind second upstream");
    let second_addr = second_listener.local_addr().expect("second upstream addr");
    tokio::spawn(async move {
        axum::serve(second_listener, second_app)
            .await
            .expect("serve second upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut fallback_provider = cfg.providers.get("deepseek").unwrap().clone();
    repoint_provider_urls(cfg.providers.get_mut("deepseek").unwrap(), first_addr);
    cfg.providers.get_mut("deepseek").unwrap().api_key_env = None;
    repoint_provider_urls(&mut fallback_provider, second_addr);
    fallback_provider.api_key_env = None;
    cfg.providers
        .insert("fallback".to_string(), fallback_provider);
    let model = cfg.models.get_mut("deepseek-v4-pro-lp").unwrap();
    model
        .openai_responses_providers
        .push(crate::config::ProviderBinding {
            name: "fallback".to_string(),
            model: "fallback-model".to_string(),
        });

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model": "deepseek-v4-pro-lp", "input": "ping", "stream": false}))
        .send()
        .await
        .expect("send proxy request")
        .json()
        .await
        .expect("parse proxy response");

    assert_eq!(response["output"][0]["content"][0]["text"], "fallback ok");
    assert_eq!(*first_hits.lock().unwrap(), 1);
}

#[test]
fn cooldown_policy_uses_configured_durations_and_auth_does_not_cooldown() {
    let cooldown = crate::config::FallbackCooldownConfig {
        network_seconds: 11,
        server_error_seconds: 22,
        rate_limit_seconds: 33,
        model_unavailable_seconds: 44,
        client_error_seconds: 55,
    };

    assert_eq!(
        cooldown_duration_for_status(StatusCode::INTERNAL_SERVER_ERROR, &cooldown),
        Some(Duration::from_secs(22))
    );
    assert_eq!(
        cooldown_duration_for_status(StatusCode::TOO_MANY_REQUESTS, &cooldown),
        Some(Duration::from_secs(33))
    );
    let mut headers = HeaderMap::new();
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("7"));
    assert_eq!(
        cooldown_duration_for_response(StatusCode::TOO_MANY_REQUESTS, &headers, &cooldown),
        Some(Duration::from_secs(7))
    );
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("999999"));
    assert_eq!(
        cooldown_duration_for_response(StatusCode::TOO_MANY_REQUESTS, &headers, &cooldown),
        Some(Duration::from_secs(24 * 60 * 60))
    );
    let future = SystemTime::now() + Duration::from_secs(120);
    let future = httpdate::fmt_http_date(future);
    headers.insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&future).expect("future retry-after"),
    );
    let duration =
        cooldown_duration_for_response(StatusCode::TOO_MANY_REQUESTS, &headers, &cooldown)
            .expect("http-date retry-after");
    assert!(duration.as_secs() <= 120);
    assert!(duration.as_secs() >= 1);
    headers.insert(
        header::RETRY_AFTER,
        HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
    );
    assert_eq!(
        cooldown_duration_for_response(StatusCode::TOO_MANY_REQUESTS, &headers, &cooldown),
        Some(Duration::from_secs(33))
    );
    assert_eq!(
        cooldown_duration_for_status(StatusCode::NOT_FOUND, &cooldown),
        Some(Duration::from_secs(44))
    );
    assert_eq!(
        cooldown_duration_for_status(StatusCode::BAD_REQUEST, &cooldown),
        Some(Duration::from_secs(55))
    );
    assert_eq!(
        cooldown_duration_for_status(StatusCode::UNAUTHORIZED, &cooldown),
        None
    );
    assert_eq!(
        cooldown_duration_for_status(StatusCode::FORBIDDEN, &cooldown),
        None
    );
}

#[tokio::test]
async fn chat_non_streaming_falls_back_to_next_provider_after_cooldown_error() {
    let first_app = Router::new().route(
        "/chat/completions",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind first upstream");
    let first_addr = first_listener.local_addr().expect("first upstream addr");
    tokio::spawn(async move {
        axum::serve(first_listener, first_app)
            .await
            .expect("serve first upstream");
    });

    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let second_app = Router::new()
        .route(
            "/chat/completions",
            post(
                |State(captured): State<Arc<Mutex<Option<Value>>>>,
                 Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "chatcmpl_second",
                        "created": 123,
                        "choices": [{"message": {"role": "assistant", "content": "chat fallback ok"}}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    }))
                },
            ),
        )
        .with_state(captured_for_handler);
    let second_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind second upstream");
    let second_addr = second_listener.local_addr().expect("second upstream addr");
    tokio::spawn(async move {
        axum::serve(second_listener, second_app)
            .await
            .expect("serve second upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut fallback_provider = cfg.providers.get("deepseek").unwrap().clone();
    repoint_provider_urls(cfg.providers.get_mut("deepseek").unwrap(), first_addr);
    cfg.providers.get_mut("deepseek").unwrap().api_key_env = None;
    repoint_provider_urls(&mut fallback_provider, second_addr);
    fallback_provider.api_key_env = None;
    cfg.providers
        .insert("fallback".to_string(), fallback_provider);
    cfg.models
        .get_mut("deepseek-v4-pro-lp")
        .unwrap()
        .openai_chat_providers
        .push(crate::config::ProviderBinding {
            name: "fallback".to_string(),
            model: "fallback-chat-model".to_string(),
        });

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "messages": [{"role": "user", "content": "ping"}],
            "stream": false
        }))
        .send()
        .await
        .expect("send proxy request")
        .json()
        .await
        .expect("parse proxy response");

    assert_eq!(response["model"], "deepseek-v4-pro-lp");
    assert_eq!(
        response["choices"][0]["message"]["content"],
        "chat fallback ok"
    );
    let captured = captured
        .lock()
        .unwrap()
        .clone()
        .expect("captured second request");
    assert_eq!(captured["model"], "fallback-chat-model");
}

#[tokio::test]
async fn anthropic_non_streaming_falls_back_to_next_provider_after_cooldown_error() {
    let first_app = Router::new().route(
        "/anthropic/v1/messages",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind first upstream");
    let first_addr = first_listener.local_addr().expect("first upstream addr");
    tokio::spawn(async move {
        axum::serve(first_listener, first_app)
            .await
            .expect("serve first upstream");
    });

    let second_app = Router::new().route(
        "/anthropic/v1/messages",
        post(|| async {
            Json(json!({
                "id": "msg_fallback",
                "type": "message",
                "role": "assistant",
                "model": "fallback-model",
                "content": [{"type": "text", "text": "anthropic fallback ok"}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }))
        }),
    );
    let second_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind second upstream");
    let second_addr = second_listener.local_addr().expect("second upstream addr");
    tokio::spawn(async move {
        axum::serve(second_listener, second_app)
            .await
            .expect("serve second upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut fallback_provider = cfg.providers.get("deepseek").unwrap().clone();
    repoint_provider_urls(cfg.providers.get_mut("deepseek").unwrap(), first_addr);
    cfg.providers.get_mut("deepseek").unwrap().api_key_env = None;
    repoint_provider_urls(&mut fallback_provider, second_addr);
    fallback_provider.api_key_env = None;
    cfg.providers
        .insert("fallback".to_string(), fallback_provider);
    cfg.models
        .get_mut("deepseek-v4-pro-lp")
        .unwrap()
        .anthropic_providers
        .push(crate::config::ProviderBinding {
            name: "fallback".to_string(),
            model: "fallback-anthropic-model".to_string(),
        });

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 64,
            "stream": false
        }))
        .send()
        .await
        .expect("send proxy request")
        .json()
        .await
        .expect("parse proxy response");

    assert_eq!(response["model"], "deepseek-v4-pro-lp");
    assert_eq!(response["content"][0]["text"], "anthropic fallback ok");
}

#[tokio::test]
async fn repeated_bad_request_is_blocked_before_hitting_upstream() {
    let hits = Arc::new(Mutex::new(0usize));
    let hits_for_handler = Arc::clone(&hits);
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let hits_for_handler = Arc::clone(&hits_for_handler);
            async move {
                *hits_for_handler.lock().unwrap() += 1;
                (StatusCode::BAD_REQUEST, "bad shape")
            }
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    cfg.protection.bad_request.max_errors = 1;
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let client = reqwest::Client::new();
    let body = json!({
        "model": "deepseek-v4-pro-lp",
        "messages": [{"role": "user", "content": "bad"}],
        "stream": false
    });
    let first = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("first request");
    assert_eq!(first.status(), StatusCode::BAD_REQUEST);

    let second = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("second request");
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(*hits.lock().unwrap(), 1);
}

#[tokio::test]
async fn resolution_failures_are_deterministic_4xx_before_provider_selection() {
    let hits = Arc::new(Mutex::new(0usize));
    let hits_for_handler = Arc::clone(&hits);
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let hits_for_handler = Arc::clone(&hits_for_handler);
            async move {
                *hits_for_handler.lock().unwrap() += 1;
                Json(json!({
                    "id": "chatcmpl_never",
                    "created": 123,
                    "choices": [{"message": {"role": "assistant", "content": "unreachable"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }))
            }
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;
    cfg.models.insert(
        "chat-only-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 128_000,
            max_output_tokens: 8_192,
            features: Vec::new(),
            supported_reasoning_levels: vec!["low".to_string(), "medium".to_string()],
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "deepseek".to_string(),
                model: "deepseek-v4-pro".to_string(),
            }],
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
        },
    );

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let client = reqwest::Client::new();

    // Unknown model: explicit 4xx, no default substitution.
    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({"model": "no-such-model", "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .expect("unknown model request");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("unknown model body");
    assert!(body.to_string().contains("unknown model"));

    // Model without a binding for the request protocol: 4xx listing the
    // supported protocols.
    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model": "chat-only-lp", "input": "ping"}))
        .send()
        .await
        .expect("unsupported protocol request");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("unsupported protocol body");
    assert!(body.to_string().contains("does not support"));
    assert!(body.to_string().contains("chat_completions"));

    // Capability filtering: image input against a model that does not
    // declare image_input fails locally before provider selection.
    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "chat-only-lp",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}}
                ]
            }]
        }))
        .send()
        .await
        .expect("capability request");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("capability body");
    assert!(body.to_string().contains("image_input"));

    // Reasoning level gating: declared level vocabularies are enforced as
    // local configuration/client errors before provider selection.
    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "chat-only-lp",
            "messages": [{"role": "user", "content": "ping"}],
            "reasoning_effort": "high"
        }))
        .send()
        .await
        .expect("reasoning level request");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("reasoning level body");
    assert!(
        body.to_string()
            .contains("does not support reasoning level")
    );
    assert!(body.to_string().contains("low, medium"));

    // No request ever reached the upstream, and no cooldown/fingerprint
    // accounting happened for these local rejections.
    assert_eq!(*hits.lock().unwrap(), 0);
}

#[tokio::test]
async fn streaming_falls_back_to_next_provider_before_first_byte() {
    let first_app = Router::new().route(
        "/chat/completions",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind first upstream");
    let first_addr = first_listener.local_addr().expect("first upstream addr");
    tokio::spawn(async move {
        axum::serve(first_listener, first_app)
            .await
            .expect("serve first upstream");
    });

    let second_app = Router::new().route(
        "/chat/completions",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"id\":\"chatcmpl_fb\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"stream fallback ok\"}}]}\n\n",
                    "data: {\"id\":\"chatcmpl_fb\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let second_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind second upstream");
    let second_addr = second_listener.local_addr().expect("second upstream addr");
    tokio::spawn(async move {
        axum::serve(second_listener, second_app)
            .await
            .expect("serve second upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut fallback_provider = cfg.providers.get("deepseek").unwrap().clone();
    repoint_provider_urls(cfg.providers.get_mut("deepseek").unwrap(), first_addr);
    cfg.providers.get_mut("deepseek").unwrap().api_key_env = None;
    repoint_provider_urls(&mut fallback_provider, second_addr);
    fallback_provider.api_key_env = None;
    cfg.providers
        .insert("fallback".to_string(), fallback_provider);
    cfg.models
        .get_mut("deepseek-v4-pro-lp")
        .unwrap()
        .openai_chat_providers
        .push(crate::config::ProviderBinding {
            name: "fallback".to_string(),
            model: "fallback-chat-model".to_string(),
        });

    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    // §10.5.5: an upstream error status before the first downstream byte
    // triggers cooldown + fallback for streaming requests, exactly as for
    // non-streaming ones.
    let body = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "messages": [{"role": "user", "content": "ping"}],
            "stream": true
        }))
        .send()
        .await
        .expect("send proxy request");
    assert_eq!(body.status(), StatusCode::OK);
    let text = body.text().await.expect("read stream");
    assert!(text.contains("stream fallback ok"));
    assert!(text.contains("data: [DONE]"));
}

#[tokio::test]
async fn client_protocol_error_shapes() {
    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body bytes");
        serde_json::from_slice(&bytes).expect("json body")
    }

    let chat = body_json(json_error(StatusCode::BAD_REQUEST, "nope")).await;
    assert_eq!(chat["error"]["type"], "proxy_error");
    assert_eq!(chat["error"]["code"], "bad_request");
    assert!(chat["error"]["message"].as_str().unwrap().contains("nope"));

    let responses = body_json(responses_error(StatusCode::TOO_MANY_REQUESTS, "slow down")).await;
    assert_eq!(responses["type"], "error");
    assert_eq!(responses["error"]["type"], "too_many_requests");
    assert!(
        responses["error"]["message"]
            .as_str()
            .unwrap()
            .contains("slow down")
    );

    let anthropic = body_json(anthropic_error(StatusCode::BAD_REQUEST, "bad")).await;
    assert_eq!(anthropic["type"], "error");
    assert_eq!(anthropic["error"]["type"], "invalid_request_error");

    let anthropic_server = body_json(anthropic_error(StatusCode::BAD_GATEWAY, "boom")).await;
    assert_eq!(anthropic_server["error"]["type"], "api_error");
}

// ── §12/§13/§14 集成测试：admin 端点、委托链路、活跃 provider ─────────────

#[tokio::test]
async fn admin_status_reports_active_providers_within_ttl() {
    // §12.3/12.5：/admin/status 返回活跃 provider 列表（TTL 内）
    let cfg = config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);
    state.mark_provider_active("deepseek");
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });

    let resp: Value = reqwest::Client::new()
        .get(format!("http://{addr}/admin/status"))
        .send()
        .await
        .expect("admin status")
        .json()
        .await
        .expect("status json");
    let active = resp["data"]["active_providers"].as_array().expect("array");
    assert!(active.iter().any(|p| p == "deepseek"));
    assert!(
        resp["data"]["cache"].is_object(),
        "status should carry cache"
    );
}

#[tokio::test]
#[serial]
async fn admin_status_probe_probes_inactive_providers_against_upstream() {
    // §12.5/12.7：/admin/status/probe 对非活跃 provider 做真实探活并返回结果
    // 隔离全局 status_cache.json，避免与其他测试的 probe 结果相互污染
    isolate_status_cache();
    let mut cfg = config::default_deepseek_config();
    // deepseek 的 endpoint 指向 mock upstream
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Json(json!({
                "id": "probe-ok",
                "object": "chat.completion",
                "model": "deepseek-chat",
                "choices": [{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],
                "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
            }))
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });
    repoint_provider_urls(
        cfg.providers
            .get_mut("deepseek")
            .expect("deepseek provider"),
        upstream_addr,
    );

    let state = AppState::new_in_memory(cfg);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });

    let resp: Value = reqwest::Client::new()
        .post(format!("http://{addr}/admin/status/probe"))
        .send()
        .await
        .expect("admin probe")
        .json()
        .await
        .expect("probe json");
    assert_eq!(resp["status"], "ok");
    let probed = resp["data"]["probed_providers"].as_array().expect("array");
    assert!(
        probed.iter().any(|p| p == "deepseek"),
        "deepseek should be probed"
    );

    // singleflight：5 秒窗口内再次 probe 应跳过（不重复发请求）
    let resp2: Value = reqwest::Client::new()
        .post(format!("http://{addr}/admin/status/probe"))
        .send()
        .await
        .expect("admin probe 2")
        .json()
        .await
        .expect("probe json 2");
    let probed2 = resp2["data"]["probed_providers"].as_array().expect("array");
    assert!(
        probed2.is_empty(),
        "5s singleflight window should skip re-probing"
    );
}

#[test]
fn thought_signature_replay_matches_by_key_across_turns() {
    let cfg = crate::config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);

    // R1 响应：fc1 → sig1（keyed by name+args）
    let r1 = json!({
        "response": {"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "ls", "args": {}}, "thoughtSignature": "sig1"}
        ]}}]}
    });
    state.collect_thought_signatures(&r1);

    // R2 请求：历史含 fc1 → 注入 sig1
    let mut body = json!({
        "request": {"contents": [
            {"role": "model", "parts": [{"functionCall": {"name": "ls", "args": {}}}]}
        ]}
    });
    state.inject_thought_signatures(&mut body);
    let part = &body["request"]["contents"][0]["parts"][0];
    assert_eq!(part["thoughtSignature"], "sig1", "R2 应注入 fc1 的签名");

    // R2 响应：fc2 → sig2（不同工具）
    let r2 = json!({
        "response": {"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}, "thoughtSignature": "sig2"}
        ]}}]}
    });
    state.collect_thought_signatures(&r2);

    // R3 请求：历史含 fc1 + fc2 → 各自注入对应签名（按 key，非顺序）
    let mut body3 = json!({
        "request": {"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"name": "ls", "args": {}}},
                {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}}
            ]}
        ]}
    });
    state.inject_thought_signatures(&mut body3);
    let parts3 = &body3["request"]["contents"][0]["parts"];
    assert_eq!(parts3[0]["thoughtSignature"], "sig1", "fc1 应注入 sig1");
    assert_eq!(parts3[1]["thoughtSignature"], "sig2", "fc2 应注入 sig2");
}

#[test]
fn thought_signature_replay_never_misassigns_stale_signature() {
    let cfg = crate::config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);

    // 缓存里只有 fc1 的签名
    let r1 = json!({
        "response": {"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "ls", "args": {}}, "thoughtSignature": "sig1"}
        ]}}]}
    });
    state.collect_thought_signatures(&r1);

    // 请求里出现一个缓存中不存在的 functionCall → 不注入任何签名（宁缺勿错）
    let mut body = json!({
        "request": {"contents": [
            {"role": "model", "parts": [{"functionCall": {"name": "unknown_tool", "args": {}}}]}
        ]}
    });
    state.inject_thought_signatures(&mut body);
    let part = &body["request"]["contents"][0]["parts"][0];
    assert!(
        part.get("thoughtSignature").is_none(),
        "未知 functionCall 不应注入过期签名: {part:?}"
    );
}

#[test]
fn thought_signature_map_is_bounded() {
    let cfg = crate::config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);

    // 灌入超过上限的签名（模拟超长会话）
    for i in 0..(THOUGHT_SIGNATURE_MAP_MAX + 10) {
        let resp = json!({
            "response": {"candidates": [{"content": {"parts": [
                {"functionCall": {"name": format!("tool_{i}"), "args": {}}, "thoughtSignature": format!("sig_{i}")}
            ]}}]}
        });
        state.collect_thought_signatures(&resp);
    }
    let queue = state.thought_sig_queue.lock().unwrap();
    assert!(
        queue.len() <= THOUGHT_SIGNATURE_MAP_MAX,
        "签名 map 应被上限约束，got {}",
        queue.len()
    );
}

#[test]
fn thought_signature_inject_falls_back_to_name_args_key() {
    // Claude 方向：流式收集用 name+args key，但注入时 functionCall 带 id。
    // 注入必须 fallback 到 name+args key 才能命中（High-2 回归修复）。
    let cfg = crate::config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);

    // 流式收集：chunk 里只有 name+args（无 id）
    let streamed = json!({
        "response": {"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}, "thoughtSignature": "sig_stream"}
        ]}}]}
    });
    let chunk = convert::antigravity_stream_chunk(&streamed);
    {
        let mut queue = state.thought_sig_queue.lock().unwrap();
        for (name, signature) in chunk.signature_pairs {
            queue.insert(
                convert::signature_key_from_name_args(&name, r#"{"path":"a.txt"}"#),
                signature,
            );
        }
    }

    // 注入：claude 路径的 functionCall 带 id（如 call_1），且无签名
    let mut body = json!({
        "request": {"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"id": "call_1", "name": "read_file", "args": {"path": "a.txt"}}}
            ]}
        ]}
    });
    state.inject_thought_signatures(&mut body);
    let part = &body["request"]["contents"][0]["parts"][0];
    assert_eq!(
        part["thoughtSignature"], "sig_stream",
        "带 id 的 functionCall 应 fallback 命中 name+args key: {part:?}"
    );
}

#[test]
fn collect_thought_signatures_unwraps_array_response() {
    // Antigravity 非流式响应可能是数组（anthropic ingress 实测）。
    // collect 必须解包数组 + 存储双 key（id + name+args），
    // 以便 Anthropic 客户端回传（无上游 id）也能命中注入。
    let cfg = crate::config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);

    let arr_resp = json!([{
        "response": {"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "fib", "args": {"n": 20}, "id": "upstream_id_1"}, "thoughtSignature": "sig_arr"}
        ]}}]}
    }]);
    state.collect_thought_signatures(&arr_resp);

    // Anthropic 客户端回传：functionCall 无上游 id → 走 name+args key
    let mut body = json!({
        "request": {"contents": [
            {"role": "model", "parts": [{"functionCall": {"name": "fib", "args": {"n": 20}}}]}
        ]}
    });
    state.inject_thought_signatures(&mut body);
    let part = &body["request"]["contents"][0]["parts"][0];
    assert_eq!(
        part["thoughtSignature"], "sig_arr",
        "数组响应收集的签名应经 name+args key 注入: {part:?}"
    );
}

#[tokio::test]
async fn streaming_signature_pairs_keyed_with_assembled_args() {
    // antigravity 流式 functionCall 的 args 是完整 JSON 对象一次到达
    //（Go 版无增量拼接；CLIProxyAPI 注释 "emits the full function call
    // payload at once"）。签名必须用完整 args 做 key，下一轮回放才能命中。
    let upstream_app = axum::Router::new().route(
        "/v1internal:streamGenerateContent",
        axum::routing::post(|body: String| async move {
            let _ = body;
            let chunks = [
                "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"a.txt\"}},\"thoughtSignature\":\"sig_stream\"}]}}]}}\n\n",
                "data: [DONE]\n\n",
            ];
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(chunks.concat()))
                .unwrap()
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let cfg = config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);
    let url = format!("http://{upstream_addr}/v1internal:streamGenerateContent");
    let mut plan = test_execution_plan(url.clone());
    plan.request_frequency = config::RequestFrequencyConfig {
        requests_per_minute: Some(60),
        requests_per_hour: None,
        burst: Some(60),
        queue_timeout_seconds: Some(1),
    };

    let req = state
        .client
        .post(&url)
        .json(&json!({"model":"m","stream":true}));
    let upstream = send_upstream(&state, &plan, req)
        .await
        .expect("upstream request");

    // 直接调用流式转换并消费响应体，驱动流执行以收集签名
    let resp = antigravity_sse_to_responses_sse(
        &state,
        upstream,
        "gemini-3.6-flash-high-lp".to_string(),
        "antigravity".to_string(),
        "antigravity".to_string(),
    )
    .await;
    let _body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("collect stream body");

    // 签名的 key 基于完整 args：fn:read_file:{"path":"a.txt"}
    let queue = state.thought_sig_queue.lock().unwrap();
    let key = convert::signature_key_from_name_args("read_file", r#"{"path":"a.txt"}"#);
    assert_eq!(
        queue.get(&key).map(String::as_str),
        Some("sig_stream"),
        "流式签名必须用完整 args 做 key，queue={queue:?}"
    );
}

#[tokio::test]
async fn admin_ping_and_status_endpoints_work() {
    let cfg = config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    let client = reqwest::Client::new();

    // GET /admin/ping
    let ping: Value = client
        .get(format!("http://{addr}/admin/ping"))
        .send()
        .await
        .expect("admin ping")
        .json()
        .await
        .expect("ping json");
    assert_eq!(ping["status"], "ok");
    assert!(!ping["version"].as_str().unwrap_or("").is_empty());

    // GET /admin/status（无活跃 provider）
    let status: Value = client
        .get(format!("http://{addr}/admin/status"))
        .send()
        .await
        .expect("admin status")
        .json()
        .await
        .expect("status json");
    assert_eq!(status["status"], "ok");
    assert_eq!(status["data"]["providers"], 1);
    assert_eq!(status["data"]["models"], 2);
    assert!(
        status["data"]["active_providers"]
            .as_array()
            .unwrap()
            .is_empty(),
        "no active providers initially"
    );
}

/// status_probe 的 L1 活跃过滤：活跃 provider 不触发任何真实网络请求。
#[tokio::test]
#[serial]
async fn admin_status_probe_skips_active_provider() {
    // 隔离全局 status_cache.json，避免其他测试的 probe 结果污染 L2 缓存
    isolate_status_cache();
    let mut cfg = config::default_deepseek_config();

    // mock upstream：计数真实请求
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = counter_clone.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "id": "probe-ok",
                    "object": "chat.completion",
                    "model": "deepseek-chat",
                    "choices": [{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],
                    "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                }))
            }
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });
    repoint_provider_urls(
        cfg.providers
            .get_mut("deepseek")
            .expect("deepseek provider"),
        upstream_addr,
    );

    let state = AppState::new_in_memory(cfg);
    // 标记 deepseek 为活跃（L1 应跳过，不触发网络请求）
    state.mark_provider_active("deepseek");

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });

    let resp: Value = reqwest::Client::new()
        .post(format!("http://{addr}/admin/status/probe"))
        .send()
        .await
        .expect("admin probe")
        .json()
        .await
        .expect("probe json");
    assert_eq!(resp["status"], "ok");
    let probed = resp["data"]["probed_providers"].as_array().expect("array");
    assert!(probed.is_empty(), "active provider should not be probed");
    // L1 活跃过滤：活跃 provider 不触发任何真实网络请求
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "active provider must not trigger network probe"
    );
}

#[test]
fn egress_compat_adapts_responses_body() {
    // 无 compat + 无 endpoint store：input 数组化 + 默认注入 store=false
    let mut body = json!({
        "input": "plain string",
        "stream": false,
    });
    convert::apply_responses_egress_compat(&mut body, &config::CompatConfig::default(), None);
    assert!(
        body["input"].is_array(),
        "input must be normalized to a list"
    );
    assert_eq!(
        body["store"],
        json!(false),
        "default store=false (research §3.1a scenario 2)"
    );
    assert_eq!(body["stream"], json!(false));

    // 客户端显式 store:true，无配置 → 透传（scenario 1）
    let mut body = json!({
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        "stream": false,
        "store": true,
    });
    convert::apply_responses_egress_compat(&mut body, &config::CompatConfig::default(), None);
    assert_eq!(
        body["store"],
        json!(true),
        "client explicit store must pass through"
    );

    // endpoint 配置 store=false → 覆盖客户端（scenario 3）
    let mut body = json!({
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        "stream": false,
        "store": true,
    });
    convert::apply_responses_egress_compat(
        &mut body,
        &config::CompatConfig::default(),
        Some(false),
    );
    assert_eq!(
        body["store"],
        json!(false),
        "endpoint store overrides client"
    );

    // force_stream + strip_max_output_tokens + must_not_store 强制 false（scenario 4）
    let compat = config::CompatConfig {
        force_stream: Some(true),
        strip_max_output_tokens: Some(true),
        must_not_store: Some(true),
        ..config::CompatConfig::default()
    };
    let mut body = json!({
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        "stream": false,
        "max_output_tokens": 16,
        "store": true,
    });
    convert::apply_responses_egress_compat(&mut body, &compat, Some(true));
    assert_eq!(body["stream"], json!(true));
    assert!(body.get("max_output_tokens").is_none());
    assert_eq!(
        body["store"],
        json!(false),
        "must_not_store forces store=false regardless of endpoint config"
    );
}

/// 构造带默认内存保护配置的测试 state（aggregate 测试专用）
fn aggregate_test_state() -> AppState {
    AppState::new_in_memory(crate::config::default_deepseek_config())
}

#[tokio::test]
async fn aggregate_responses_sse_collects_text_usage_and_tool_calls() {
    use axum::http::header;
    let sse = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
        "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"path\\\":\"}\n\n",
        "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"\\\"Cargo.toml\\\"}\"}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"done\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
    );
    let upstream_app = Router::new().route(
        "/sse",
        get(move || async move {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.to_string(),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });
    let state = aggregate_test_state();
    let resp = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/sse"))
        .send()
        .await
        .expect("fetch sse");

    let value = aggregate_responses_sse_to_value(&state, resp)
        .await
        .expect("aggregate");
    let output = value["output"].as_array().expect("output array");
    assert_eq!(output.len(), 2, "message + function_call items");
    assert_eq!(output[0]["type"], "function_call");
    assert_eq!(output[0]["call_id"], "call_1");
    assert_eq!(output[0]["name"], "read_file");
    assert_eq!(output[0]["arguments"], r#"{"path":"Cargo.toml"}"#);
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "done");
    assert_eq!(value["usage"]["input_tokens"], json!(10));
    assert_eq!(value["usage"]["output_tokens"], json!(5));
    assert_eq!(value["status"], "completed");
}

#[tokio::test]
async fn aggregate_responses_sse_surfaces_upstream_failure() {
    use axum::http::header;
    // Upstream reports response.failed — the aggregate must surface the
    // error instead of fabricating a completed response.
    let sse = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"partial\"}\n\n",
        "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"content filter blocked\"}}}\n\n",
    );
    let upstream_app = Router::new().route(
        "/sse",
        get(move || async move {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.to_string(),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let state = aggregate_test_state();
    let resp = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/sse"))
        .send()
        .await
        .expect("fetch sse");

    let err = aggregate_responses_sse_to_value(&state, resp)
        .await
        .expect_err("failed event must be surfaced as an error, not a completed response");
    assert!(
        err.to_string().contains("content filter blocked"),
        "error must carry the upstream message, got: {err}"
    );
}

#[tokio::test]
async fn aggregate_responses_sse_preserves_incomplete_status_and_usage() {
    use axum::http::header;
    // Upstream truncates (response.incomplete): the aggregate must keep
    // status=incomplete + incomplete_details + usage so downstream
    // conversion maps to finish_reason=length instead of a normal stop.
    let sse = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"truncated text\"}\n\n",
        "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_tokens\"},\"usage\":{\"input_tokens\":20,\"output_tokens\":8}}}\n\n",
    );
    let upstream_app = Router::new().route(
        "/sse",
        get(move || async move {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.to_string(),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let state = aggregate_test_state();
    let resp = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/sse"))
        .send()
        .await
        .expect("fetch sse");

    let value = aggregate_responses_sse_to_value(&state, resp)
        .await
        .expect("aggregate");
    assert_eq!(
        value["status"], "incomplete",
        "truncation must not be reported as completed"
    );
    assert_eq!(value["incomplete_details"]["reason"], "max_tokens");
    assert_eq!(value["usage"]["input_tokens"], json!(20));
    assert_eq!(value["usage"]["output_tokens"], json!(8));
}

#[tokio::test]
async fn aggregate_responses_sse_preserves_order_and_interleaved_tool_calls() {
    use axum::http::header;
    // message (index 0) first, two function_calls (index 1, 2) with
    // interleaved argument deltas; fc_b completes via the done event payload.
    let sse = concat!(
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_0\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"id\":\"fc_b\",\"type\":\"function_call\",\"call_id\":\"call_b\",\"name\":\"write_file\",\"arguments\":\"\"}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_0\",\"delta\":\"first\"}\n\n",
        "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_a\",\"delta\":\"{\\\"path\\\":\"}\n\n",
        "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_b\",\"delta\":\"{\\\"name\\\":\"}\n\n",
        "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_a\",\"delta\":\"\\\"Cargo.toml\\\"}\"}\n\n",
        "event: response.function_call_arguments.done\ndata: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_b\",\"arguments\":\"{\\\"name\\\":\\\"x.rs\\\"}\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n",
    );
    let upstream_app = Router::new().route(
        "/sse",
        get(move || async move {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.to_string(),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let state = aggregate_test_state();
    let resp = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/sse"))
        .send()
        .await
        .expect("fetch sse");

    let value = aggregate_responses_sse_to_value(&state, resp)
        .await
        .expect("aggregate");
    let output = value["output"].as_array().expect("output array");
    // Output order follows upstream output_index, not item type.
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["text"], "first");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["call_id"], "call_a");
    assert_eq!(output[1]["arguments"], r#"{"path":"Cargo.toml"}"#);
    assert_eq!(output[2]["type"], "function_call");
    // fc_b's delta was partial; the done event's complete payload wins.
    assert_eq!(output[2]["arguments"], r#"{"name":"x.rs"}"#);
}

#[tokio::test]
async fn aggregate_responses_sse_rejects_done_not_extending_delta() {
    use axum::http::header;
    // done does NOT extend the accumulated delta — the upstream event
    // stream is inconsistent; surface an error instead of trusting either.
    let sse = concat!(
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
        "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}\n\n",
        // done disagrees with the accumulated delta (different field).
        "event: response.function_call_arguments.done\ndata: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{\\\"name\\\":\\\"x.rs\\\"}\"}\n\n",
    );
    let upstream_app = Router::new().route(
        "/sse",
        get(move || async move {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.to_string(),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let state = aggregate_test_state();
    let resp = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/sse"))
        .send()
        .await
        .expect("fetch sse");

    let err = aggregate_responses_sse_to_value(&state, resp)
        .await
        .expect_err("prefix-mismatched done must be surfaced as an error");
    assert!(
        err.to_string().contains("does not extend"),
        "error must explain the prefix mismatch, got: {err}"
    );
}

#[tokio::test]
async fn aggregate_responses_sse_collects_reasoning_item() {
    use axum::http::header;
    // Reasoning item (type=reasoning) with summary text deltas must be
    // aggregated so downstream convert (responses→anthropic) can emit
    // thinking blocks.
    let sse = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\"}}\n\n",
        "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"delta\":\"let me \"}\n\n",
        "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"delta\":\"think...\"}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"answer\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":10}}}\n\n",
    );
    let upstream_app = Router::new().route(
        "/sse",
        get(move || async move {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.to_string(),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let state = aggregate_test_state();
    let resp = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/sse"))
        .send()
        .await
        .expect("fetch sse");

    let value = aggregate_responses_sse_to_value(&state, resp)
        .await
        .expect("aggregate");
    let output = value["output"].as_array().expect("output array");
    assert_eq!(output.len(), 2, "reasoning + message items");
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(
        output[0]["summary"][0]["text"], "let me think...",
        "reasoning summary must accumulate deltas"
    );
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "answer");
}

#[tokio::test]
async fn aggregate_responses_sse_output_item_done_fallback() {
    use axum::http::header;
    // Upstream emits only item-level added + done (no field-level deltas).
    // The aggregate must still capture the complete content via the done event.
    let sse = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"complete answer\"}]}}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":7}}}\n\n",
    );
    let upstream_app = Router::new().route(
        "/sse",
        get(move || async move {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.to_string(),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let state = aggregate_test_state();
    let resp = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/sse"))
        .send()
        .await
        .expect("fetch sse");

    let value = aggregate_responses_sse_to_value(&state, resp)
        .await
        .expect("aggregate");
    let output = value["output"].as_array().expect("output array");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(
        output[0]["content"][0]["text"], "complete answer",
        "output_item.done must fill in empty content"
    );
}

#[tokio::test]
async fn aggregate_responses_sse_multi_content_part() {
    use axum::http::header;
    // Message with multiple content parts; deltas use content_index to
    // address each part independently.
    let sse = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"\"},{\"type\":\"output_text\",\"text\":\"\"}]}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"content_index\":0,\"delta\":\"Hello\"}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"content_index\":1,\"delta\":\"World\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":4}}}\n\n",
    );
    let upstream_app = Router::new().route(
        "/sse",
        get(move || async move {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse.to_string(),
            )
        }),
    );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let state = aggregate_test_state();
    let resp = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/sse"))
        .send()
        .await
        .expect("fetch sse");

    let value = aggregate_responses_sse_to_value(&state, resp)
        .await
        .expect("aggregate");
    let output = value["output"].as_array().expect("output array");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["content"][0]["text"], "Hello");
    assert_eq!(output[0]["content"][1]["text"], "World");
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_via_openai_sub_non_streaming_aggregates_and_preserves_tool_calls() {
    use axum::http::header;
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream_app = Router::new()
        .route(
            // repoint_provider_urls preserves the native URL path:
            // openai-sub → /backend-api/codex/responses.
            "/backend-api/codex/responses",
            post(
                |State(captured): State<Arc<Mutex<Option<Value>>>>,
                 Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    let sse = concat!(
                        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_0\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
                        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_0\",\"delta\":\"checking\"}\n\n",
                        "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}\n\n",
                        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
                    );
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        sse.to_string(),
                    )
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    let mut provider = crate::catalog::openai_sub().provider;
    repoint_provider_urls(&mut provider, upstream_addr);
    provider.api_key_env = None;
    provider.auth = None; // Remove OAuth dependency for isolated testing
    cfg.providers.insert("openai-sub".to_string(), provider);
    cfg.models.insert(
        "gpt-5.5-sub-lp".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 272_000,
            max_output_tokens: 128_000,
            features: Vec::new(),
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            enable_thinking: None,
            reasoning_level_map: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "openai-sub".to_string(),
                model: "gpt-5.5".to_string(),
            }],
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
        },
    );
    let app = router(AppState::new_in_memory(cfg));
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    // Non-streaming chat request through the openai-sub chat endpoint.
    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "gpt-5.5-sub-lp",
            "messages": [{"role": "user", "content": "inspect Cargo.toml"}],
            "stream": false
        }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    // egress adaptation applied to the upstream body.
    let captured = captured.lock().unwrap().clone().expect("captured body");
    assert_eq!(captured["model"], "gpt-5.5");
    assert_eq!(captured["store"], json!(false));
    assert_eq!(captured["stream"], json!(true));
    assert!(captured.get("max_output_tokens").is_none());
    assert!(captured["input"].is_array());
    // non-streaming client receives aggregated chat JSON with the tool call.
    let message = &response["choices"][0]["message"];
    assert_eq!(message["content"], "checking");
    let tool_calls = message["tool_calls"].as_array().expect("tool_calls");
    assert_eq!(tool_calls[0]["function"]["name"], "read_file");
    assert_eq!(
        tool_calls[0]["function"]["arguments"],
        r#"{"path":"Cargo.toml"}"#
    );
}

#[test]
fn resolve_request_candidates_validates_missing_and_unknown_models() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    let missing = resolve_request_candidates(
        &state,
        Protocol::OpenaiChatCompletions,
        &json!({}),
        json_error,
    )
    .unwrap_err();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

    let unknown = resolve_request_candidates(
        &state,
        Protocol::OpenaiChatCompletions,
        &json!({"model":"unknown"}),
        json_error,
    )
    .unwrap_err();
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn resolve_request_candidates_rejects_protocol_capability_and_reasoning_mismatch() {
    let mut cfg = config::default_deepseek_config();
    cfg.models.insert(
        "limited".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 1000,
            max_output_tokens: 100,
            features: vec![],
            supported_reasoning_levels: vec!["low".to_string()],
            default_reasoning_level: None,
            enable_thinking: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "deepseek".to_string(),
                model: "limited-upstream".to_string(),
            }],
            openai_responses_providers: vec![],
            anthropic_providers: vec![],
            reasoning_level_map: None,
        },
    );
    let state = AppState::new_in_memory(cfg);

    let protocol_err = resolve_request_candidates(
        &state,
        Protocol::OpenaiResponses,
        &json!({"model":"limited"}),
        responses_error,
    )
    .unwrap_err();
    assert_eq!(protocol_err.status(), StatusCode::BAD_REQUEST);

    let capability_err = resolve_request_candidates(
        &state,
        Protocol::OpenaiChatCompletions,
        &json!({
            "model":"limited",
            "messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://e/x.png"}}]}]
        }),
        json_error,
    )
    .unwrap_err();
    assert_eq!(capability_err.status(), StatusCode::BAD_REQUEST);

    let reasoning_err = resolve_request_candidates(
        &state,
        Protocol::OpenaiChatCompletions,
        &json!({"model":"limited","reasoning_effort":"high"}),
        json_error,
    )
    .unwrap_err();
    assert_eq!(reasoning_err.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn reasoning_and_request_helpers_cover_edge_paths() {
    let mut cfg = config::default_deepseek_config();
    cfg.providers.get_mut("deepseek").unwrap().enable_thinking = Some(false);
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .enable_thinking = Some(false);
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .default_reasoning_level = Some("medium".to_string());
    let state = AppState::new_in_memory(cfg.clone());
    let plan = state
        .cfg
        .resolve_model_request_candidates(Protocol::OpenaiChatCompletions, "deepseek-v4-flash-lp")
        .into_iter()
        .next()
        .expect("candidate")
        .1;

    let unchanged = body_with_default_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"low"}),
    );
    assert_eq!(unchanged["reasoning_effort"], "low");

    let disabled = body_with_mapped_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"medium"}),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("disabled strips");
    assert!(disabled.get("reasoning_effort").is_none());

    let chat = json!({
        "messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"https://e/x.png"}},
            {"type":"file","file_id":"f1"}
        ]}]
    });
    assert!(request_has_image(Protocol::OpenaiChatCompletions, &chat));
    assert!(request_has_document(Protocol::OpenaiChatCompletions, &chat));
}

#[test]
fn error_retry_and_rewrite_helpers_cover_nested_shapes() {
    assert_eq!(
        anthropic_error_type(StatusCode::BAD_REQUEST),
        "invalid_request_error"
    );
    assert_eq!(error_code(StatusCode::BAD_GATEWAY), "bad_gateway");

    let usage = chat_usage_to_responses_usage(&json!({"prompt_tokens":3,"completion_tokens":4}));
    assert_eq!(usage["total_tokens"], 7);

    let mut headers = HeaderMap::new();
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("120"));
    assert_eq!(
        retry_after_duration(&headers),
        Some(Duration::from_secs(120))
    );
    assert_eq!(capped_nonzero_duration(0), None);

    let mut acc = Vec::new();
    accumulate_tool_calls(
        &mut acc,
        &[
            json!({"index":1,"id":"c2","function":{"name":"lookup","arguments":"{\"q\""}}),
            json!({"index":1,"function":{"arguments":":\"x\"}"}}),
        ],
    );
    assert_eq!(acc[1].arguments, "{\"q\":\"x\"}");

    let mut payload = json!({"model":"old","nested":{"model":"inner"},"arr":[{"model":"leaf"}]});
    rewrite_model_fields(&mut payload, "frontend");
    assert_eq!(payload["nested"]["model"], "frontend");
}

// ── E2E forward-path coverage（mock upstream + 真实 proxy server）────────────
// 覆盖 forward_antigravity / forward_anthropic / forward_chat / forward_responses
// 的转发主路径；同时经过 mod.rs 路由分发与 sse_aggregate 的部分事件处理。

async fn serve_on_random_port(app: Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    addr
}

fn empty_model(
    chat: bool,
    responses: bool,
    anthropic: bool,
    provider: &str,
    upstream: &str,
) -> config::ModelConfig {
    let binding = || config::ProviderBinding {
        name: provider.to_string(),
        model: upstream.to_string(),
    };
    config::ModelConfig {
        description: None,
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        features: Vec::new(),
        supported_reasoning_levels: Vec::new(),
        default_reasoning_level: None,
        enable_thinking: None,
        openai_chat_providers: if chat { vec![binding()] } else { vec![] },
        openai_responses_providers: if responses { vec![binding()] } else { vec![] },
        anthropic_providers: if anthropic { vec![binding()] } else { vec![] },
        reasoning_level_map: None,
    }
}

fn config_with(
    providers: Vec<(&str, config::ProviderConfig)>,
    models: Vec<(&str, config::ModelConfig)>,
) -> config::Config {
    let mut cfg = config::default_deepseek_config();
    cfg.providers.clear();
    cfg.models.clear();
    for (name, p) in providers {
        cfg.providers.insert(name.to_string(), p);
    }
    for (id, m) in models {
        cfg.models.insert(id.to_string(), m);
    }
    cfg
}

fn antigravity_account(token: &str, project: &str) -> crate::auth::AntigravityAccount {
    crate::auth::AntigravityAccount {
        account_label: "test".into(),
        project_id: project.into(),
        access_token: token.into(),
        refresh_token: "refresh-token-long-enough".into(),
        expires_at_unix: 4_102_444_800,
        updated_at_unix: 1,
    }
}

fn antigravity_provider(addr: std::net::SocketAddr) -> config::ProviderConfig {
    let mut p = crate::catalog::google_antigravity().provider;
    repoint_provider_urls(&mut p, addr);
    p
}

const ANTIGRAVITY_JSON: &str = r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"pong"}]}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3},"modelVersion":"gemini-3.6-flash-high"}}"#;

#[tokio::test]
async fn responses_via_antigravity_non_streaming_records_usage() {
    let captured: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream = Router::new()
        .route(
            "/v1internal:streamGenerateContent",
            post(
                |State(captured): State<CapturedRequest>, Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(serde_json::from_str::<Value>(ANTIGRAVITY_JSON).unwrap())
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_addr = serve_on_random_port(upstream).await;

    let cfg = config_with(
        vec![("google-antigravity", antigravity_provider(upstream_addr))],
        vec![(
            "gemini-3.6-flash-high-lp",
            empty_model(
                false,
                true,
                false,
                "google-antigravity",
                "gemini-3.6-flash-high",
            ),
        )],
    );
    let state = AppState::new_in_memory(cfg);
    state
        .oauth_accounts
        .write()
        .expect("oauth write")
        .antigravity
        .insert("antigravity".into(), antigravity_account("tok", "proj"));
    let proxy_addr = serve_on_random_port(router(state)).await;

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model": "gemini-3.6-flash-high-lp", "input": "ping", "stream": false}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    assert_eq!(response["model"], "gemini-3.6-flash-high-lp");
    assert_eq!(response["output"][0]["content"][0]["text"], "pong");

    let upstream_body = captured.lock().unwrap().clone().expect("captured");
    // antigravity 请求包裹在 project + model 外层（responses_to_antigravity_request）
    assert_eq!(upstream_body["project"], "proj");
    assert_eq!(upstream_body["model"], "gemini-3.6-flash-high");
}

#[tokio::test]
async fn responses_via_antigravity_streaming_appends_alt_sse() {
    let captured_query: CapturedHeader = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured_query);
    let upstream = Router::new()
        .route(
            "/v1internal:streamGenerateContent",
            post(
                |State(captured): State<CapturedHeader>, req: axum::extract::Request| async move {
                    *captured.lock().unwrap() =
                        Some(req.uri().query().unwrap_or_default().to_string());
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        concat!(
                            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"po\"}]}}]}}\n\n",
                            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ng\"}]}}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1}}}\n\n",
                            "data: [DONE]\n\n",
                        ),
                    )
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_addr = serve_on_random_port(upstream).await;

    let cfg = config_with(
        vec![("google-antigravity", antigravity_provider(upstream_addr))],
        vec![(
            "gemini-3.6-flash-high-lp",
            empty_model(
                false,
                true,
                false,
                "google-antigravity",
                "gemini-3.6-flash-high",
            ),
        )],
    );
    let state = AppState::new_in_memory(cfg);
    state
        .oauth_accounts
        .write()
        .expect("oauth write")
        .antigravity
        .insert("antigravity".into(), antigravity_account("tok", "proj"));
    let proxy_addr = serve_on_random_port(router(state)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model": "gemini-3.6-flash-high-lp", "input": "ping", "stream": true}))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("response.output_text.delta") || body.contains("response.created"));

    let query = captured_query.lock().unwrap().clone().expect("query");
    assert!(query.contains("alt=sse"), "query was: {query}");
}

#[tokio::test]
async fn anthropic_via_antigravity_non_streaming() {
    let upstream = Router::new().route(
        "/v1internal:streamGenerateContent",
        post(|| async { Json(serde_json::from_str::<Value>(ANTIGRAVITY_JSON).unwrap()) }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;

    let cfg = config_with(
        vec![("google-antigravity", antigravity_provider(upstream_addr))],
        vec![(
            "claude-sonnet-agy-lp",
            empty_model(false, false, true, "google-antigravity", "claude-sonnet-5"),
        )],
    );
    let state = AppState::new_in_memory(cfg);
    state
        .oauth_accounts
        .write()
        .expect("oauth write")
        .antigravity
        .insert("antigravity".into(), antigravity_account("tok", "proj"));
    let proxy_addr = serve_on_random_port(router(state)).await;

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "claude-sonnet-agy-lp",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    assert_eq!(response["model"], "claude-sonnet-agy-lp");
    assert_eq!(response["content"][0]["text"], "pong");
}

#[tokio::test]
async fn anthropic_via_chat_non_streaming() {
    let upstream = Router::new().route(
        "/chat/completions",
        post(|| async {
            Json(json!({
                "id": "chatcmpl_1", "created": 123, "model": "deepseek-v4-pro",
                "choices": [{"message": {"role": "assistant", "content": "pong"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
            }))
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;

    let mut cfg = config::default_deepseek_config();
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    // 强制 anthropic 走 chat 转换路径（catalog 默认是 native anthropic 端点）
    provider.anthropic = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiChatCompletions,
    ));
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    assert_eq!(response["model"], "deepseek-v4-pro-lp");
    assert_eq!(response["content"][0]["text"], "pong");
}

fn anthropic_native_provider(addr: std::net::SocketAddr) -> config::ProviderConfig {
    let mut p = crate::catalog::anthropic().provider;
    repoint_provider_urls(&mut p, addr);
    p.api_key_env = None;
    p
}

const ANTHROPIC_JSON: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-5","content":[{"type":"text","text":"pong"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}}"#;

#[tokio::test]
async fn anthropic_native_passthrough_rewrites_model_both_ways() {
    let captured: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream = Router::new()
        .route(
            "/v1/messages",
            post(
                |State(captured): State<CapturedRequest>, Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(serde_json::from_str::<Value>(ANTHROPIC_JSON).unwrap())
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_addr = serve_on_random_port(upstream).await;

    let cfg = config_with(
        vec![("anthropic", anthropic_native_provider(upstream_addr))],
        vec![(
            "claude-sonnet-lp",
            empty_model(false, false, true, "anthropic", "claude-sonnet-5"),
        )],
    );
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "claude-sonnet-lp",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    let upstream_body = captured.lock().unwrap().clone().expect("captured");
    assert_eq!(upstream_body["model"], "claude-sonnet-5");
    assert_eq!(response["model"], "claude-sonnet-lp");
    assert_eq!(response["content"][0]["text"], "pong");
}

fn ollama_provider(addr: std::net::SocketAddr) -> config::ProviderConfig {
    let mut p = crate::catalog::ollama().provider;
    repoint_provider_urls(&mut p, addr);
    p
}

const RESPONSES_SSE: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"po\"}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ng\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n",
    "data: [DONE]\n\n",
);

fn responses_sse_upstream() -> Router {
    Router::new().route(
        "/v1/responses",
        post(|| async { ([(header::CONTENT_TYPE, "text/event-stream")], RESPONSES_SSE) }),
    )
}

#[tokio::test]
async fn chat_via_responses_streaming() {
    let upstream_addr = serve_on_random_port(responses_sse_upstream()).await;

    let mut provider = ollama_provider(upstream_addr);
    provider.openai_chat = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiResponses,
    ));
    let cfg = config_with(
        vec![("ollama", provider)],
        vec![(
            "qwen3-27b-lp",
            empty_model(true, false, false, "ollama", "qwen3:27b"),
        )],
    );
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "qwen3-27b-lp",
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("chat.completion.chunk"), "body was: {body}");
    assert!(body.contains("\"content\":\"po\""), "body was: {body}");
    assert!(body.contains("data: [DONE]"), "body was: {body}");
}

#[tokio::test]
async fn anthropic_via_responses_streaming() {
    let upstream_addr = serve_on_random_port(responses_sse_upstream()).await;

    let mut provider = ollama_provider(upstream_addr);
    provider.anthropic = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiResponses,
    ));
    let cfg = config_with(
        vec![("ollama", provider)],
        vec![(
            "qwen3-27b-lp",
            empty_model(false, false, true, "ollama", "qwen3:27b"),
        )],
    );
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "qwen3-27b-lp",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("message_start"), "body was: {body}");
    assert!(body.contains("content_block_delta"), "body was: {body}");
}

#[tokio::test]
async fn anthropic_via_antigravity_streaming() {
    let upstream = Router::new().route(
        "/v1internal:streamGenerateContent",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"po\"}]}}]}}\n\n",
                    "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ng\"}]}}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1}}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;

    let cfg = config_with(
        vec![("google-antigravity", antigravity_provider(upstream_addr))],
        vec![(
            "claude-sonnet-agy-lp",
            empty_model(false, false, true, "google-antigravity", "claude-sonnet-5"),
        )],
    );
    let state = AppState::new_in_memory(cfg);
    state
        .oauth_accounts
        .write()
        .expect("oauth write")
        .antigravity
        .insert("antigravity".into(), antigravity_account("tok", "proj"));
    let proxy_addr = serve_on_random_port(router(state)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "claude-sonnet-agy-lp",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("message_start"), "body was: {body}");
}

#[tokio::test]
async fn responses_native_non_streaming_rewrites_model() {
    let upstream = Router::new().route(
        "/v1/responses",
        post(|| async {
            Json(json!({
                "id": "resp_1",
                "model": "qwen3:27b",
                "response": {
                    "model": "qwen3:27b",
                    "output": [{"type": "message", "content": [{"type": "output_text", "text": "pong"}]}],
                    "usage": {"input_tokens": 2, "output_tokens": 1}
                }
            }))
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;

    let cfg = config_with(
        vec![("ollama", ollama_provider(upstream_addr))],
        vec![(
            "qwen3-27b-lp",
            empty_model(false, true, false, "ollama", "qwen3:27b"),
        )],
    );
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model": "qwen3-27b-lp", "input": "ping", "stream": false}))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    assert_eq!(response["model"], "qwen3-27b-lp");
    assert_eq!(response["response"]["model"], "qwen3-27b-lp");
}

#[tokio::test]
async fn responses_native_streaming() {
    let upstream_addr = serve_on_random_port(responses_sse_upstream()).await;

    let cfg = config_with(
        vec![("ollama", ollama_provider(upstream_addr))],
        vec![(
            "qwen3-27b-lp",
            empty_model(false, true, false, "ollama", "qwen3:27b"),
        )],
    );
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model": "qwen3-27b-lp", "input": "ping", "stream": true}))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("response.output_text.delta"),
        "body was: {body}"
    );
}

#[tokio::test]
async fn chat_via_responses_non_streaming() {
    let upstream = Router::new().route(
        "/v1/responses",
        post(|| async {
            Json(json!({
                "id": "resp_1",
                "model": "qwen3:27b",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "pong"}]}],
                "usage": {"input_tokens": 2, "output_tokens": 1}
            }))
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;

    let mut provider = ollama_provider(upstream_addr);
    provider.openai_chat = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiResponses,
    ));
    let cfg = config_with(
        vec![("ollama", provider)],
        vec![(
            "qwen3-27b-lp",
            empty_model(true, false, false, "ollama", "qwen3:27b"),
        )],
    );
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "qwen3-27b-lp",
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    assert_eq!(response["choices"][0]["message"]["content"], "pong");
}

#[tokio::test]
async fn chat_passthrough_streaming() {
    let upstream = Router::new().route(
        "/chat/completions",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"deepseek-v4-pro\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"po\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"deepseek-v4-pro\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;

    let mut cfg = config::default_deepseek_config();
    let provider = cfg.providers.get_mut("deepseek").unwrap();
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("chat.completion.chunk"), "body was: {body}");
    assert!(body.contains("data: [DONE]"), "body was: {body}");
}

#[tokio::test]
async fn chat_via_anthropic_streaming() {
    let upstream = Router::new().route(
        "/v1/messages",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":2}}}\n\n",
                    "event: content_block_delta\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"po\"}}\n\n",
                    "event: message_delta\n",
                    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n",
                ),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;

    let cfg = config_with(
        vec![("anthropic", anthropic_native_provider(upstream_addr))],
        vec![(
            "claude-sonnet-lp",
            empty_model(true, false, false, "anthropic", "claude-sonnet-5"),
        )],
    );
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "claude-sonnet-lp",
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("chat.completion.chunk"), "body was: {body}");
    assert!(body.contains("data: [DONE]"), "body was: {body}");
}

// ── flatten_upstream_error_message ──────────────────────────────────

#[test]
fn flatten_double_wrapped_error() {
    // The exact pattern from the bug report: error message is itself a JSON
    // error envelope serialised as a string.
    let input = r#"{"error":{"message":"{\"error\":{\"message\":\"Rate limit exceeded\",\"type\":\"rate_limit\"}}"}}"#;
    let result = flatten_upstream_error_message(input);
    assert_eq!(result, "Rate limit exceeded");
}

#[test]
fn flatten_single_envelope() {
    let input = r#"{"error":{"message":"Invalid API key","type":"authentication_error"}}"#;
    let result = flatten_upstream_error_message(input);
    assert_eq!(result, "Invalid API key");
}

#[test]
fn flatten_plain_text_unchanged() {
    let input = "Internal Server Error";
    let result = flatten_upstream_error_message(input);
    assert_eq!(result, input);
}

#[test]
fn flatten_json_without_message_field() {
    let input = r#"{"code":"bad_request","details":"missing field"}"#;
    let result = flatten_upstream_error_message(input);
    // No "error" or "message" field at the top level — return original.
    assert_eq!(result, input);
}

#[test]
fn flatten_triple_wrapped() {
    // Three levels of nesting — should still resolve.
    let inner = r#"{"error":{"message":"deep error"}}"#;
    let mid = format!(
        r#"{{"error":{{"message":{}}}}}"#,
        serde_json::to_string(inner).unwrap()
    );
    let outer = format!(
        r#"{{"error":{{"message":{}}}}}"#,
        serde_json::to_string(&mid).unwrap()
    );
    let result = flatten_upstream_error_message(&outer);
    assert_eq!(result, "deep error");
}
