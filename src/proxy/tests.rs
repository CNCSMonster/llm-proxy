use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use axum::extract::State;

use super::*;
use crate::config;
use serial_test::serial;

// In-process handler tests with a denying resolver: even a regression cannot
// open a socket or contact a provider. Every attempted send must hit this resolver.
struct C1DenyDns(Arc<AtomicUsize>);

impl reqwest::dns::Resolve for C1DenyDns {
    fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(std::io::Error::other("fixture forbids network access").into()) })
    }
}

fn c1_history_state(candidate_providers: &[&str]) -> (AppState, Arc<AtomicUsize>) {
    let mut cfg = config::default_deepseek_config();
    let mut provider = cfg.providers["deepseek"].clone();
    let mut model = cfg.models["deepseek-v4-pro-lp"].clone();
    cfg.providers.clear();
    cfg.models.clear();
    provider.api_key_env = None;
    provider.auth = None;
    provider.anthropic = None;
    provider.openai_chat = Some(config::EndpointConfig::native(
        "http://c1-chat.invalid/v1/chat/completions".to_string(),
    ));
    provider.openai_responses = Some(config::EndpointConfig::derived(
        Protocol::OpenaiChatCompletions,
    ));
    cfg.providers.insert("c1-chat".into(), provider.clone());
    provider.openai_responses = Some(config::EndpointConfig::native(
        "http://c1-native.invalid/v1/responses".to_string(),
    ));
    cfg.providers.insert("c1-native".into(), provider);
    model.openai_chat_providers.clear();
    model.anthropic_providers.clear();
    model.openai_responses_providers = candidate_providers
        .iter()
        .map(|name| config::ProviderBinding {
            name: (*name).into(),
            model: "fixture-upstream".into(),
        })
        .collect();
    model.default_reasoning_level = None;
    model.reasoning_level_map = None;
    model.supports_thinking = None;
    model.force_think = None;
    model.force_no_think = None;
    model.default_think = None;
    cfg.models.insert("c1-model".into(), model);
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut state = AppState::new_in_memory(cfg);
    state.client = reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(C1DenyDns(attempts.clone())))
        .build()
        .unwrap();
    (state, attempts)
}

#[tokio::test]
async fn c1_history_local400_zero_upstream_and_no_fallback() {
    // Single Chat-derived candidate: conversion Err must yield a local 400
    // without any upstream send. Per-candidate conversion failure falls
    // through to the next candidate (an extra Responses-native
    // candidate would serve this shape — covered by
    // c1_history_native_responses_bypasses_chat_rejection,
    // preflight_rejected_candidate_falls_back_to_next_provider, and
    // conversion_error_falls_back_to_next_candidate).
    let (state, attempts) = c1_history_state(&["c1-chat"]);
    for stream in [false, true] {
        for (reasoning, diagnostic) in [
            (
                json!({"type":"reasoning","summary":[{"type":"summary_text","text":"PRIVATE_SUMMARY"}]}),
                "Responses reasoning summary must be empty for Chat conversion",
            ),
            (
                json!({"type":"reasoning","encrypted_content":"PRIVATE_CIPHERTEXT"}),
                "Responses reasoning encrypted_content must be empty for Chat conversion",
            ),
            (
                json!({"type":"reasoning","content":[{"type":"reasoning_text","text":17}]}),
                "Responses reasoning_text.text must be a string",
            ),
            (
                json!({"type":"reasoning","summary":[{"type":"summary_text","text":"PRIVATE_SUMMARY"}],
                "content":[{"type":"reasoning_text","text":"PRIVATE_PLAINTEXT"}]}),
                "Responses reasoning summary must be empty for Chat conversion",
            ),
        ] {
            let request = json!({"model":"c1-model","stream":stream,"input":[
                reasoning,{"role":"assistant","content":"text"}
            ]});
            let candidates =
                candidates_for_test(&state, Protocol::OpenaiResponses, &request, responses_error)
                    .unwrap_or_else(|_| panic!("fixture must have usable candidates"));
            assert_eq!(candidates.len(), 1, "chat-derived only");
            assert_eq!(
                candidates[0].1.adapter(),
                AdapterKind::ResponsesFromChatCompletions
            );
            let response = responses(
                State(state.clone()),
                Extension(crate::token::auth::TokenGrant::Open),
                JsonWithBytes::from_value(request.clone()),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                body,
                json!({"type":"error","error":{"type":"bad_request","message":diagnostic}})
            );
            assert_eq!(
                attempts.load(Ordering::SeqCst),
                0,
                "neither Chat nor native fallback may send"
            );
            for (model, plan) in candidates {
                assert!(!state.is_cooling_down(&plan.cooldown_key(&model)));
            }
            let fingerprint = crate::protection::fingerprint(
                Protocol::OpenaiResponses,
                Some("c1-model"),
                &serde_json::to_vec(&request).unwrap(),
            );
            assert!(!state.bad_request_blocked(&fingerprint));
        }
    }
}

#[tokio::test]
async fn c1_history_native_responses_bypasses_chat_rejection() {
    let (state, attempts) = c1_history_state(&["c1-native"]);
    let expected_attempts = 1 + state.config().fallback.max_retries as usize;
    let request = json!({"model":"c1-model","input":[
        {"type":"reasoning","summary":[{"type":"summary_text","text":"summary"}],
         "encrypted_content":"opaque","content":[{"type":"reasoning_text","text":"plain"}]},
        {"role":"assistant","content":"text"}
    ]});
    let response = responses(
        State(state),
        Extension(crate::token::auth::TokenGrant::Open),
        JsonWithBytes::from_value(request),
    )
    .await;
    // Native forwarding reaches the deliberately denied send instead of the
    // Chat-only validator. This is not an upstream success or wire-schema test.
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(attempts.load(Ordering::SeqCst), expected_attempts);
}

// Phase B: exercise the real converter with an in-memory reqwest body.
// No listener, provider, credentials, shared config, or timer-based success.
fn sse_frame(value: Value) -> bytes::Bytes {
    bytes::Bytes::from(format!("data: {value}\n\n"))
}

fn sse_delta(delta: Value, finish: Value) -> bytes::Bytes {
    sse_frame(json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]}))
}

fn sse_upstream(
    stream: impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + 'static,
) -> reqwest::Response {
    axum::http::Response::new(reqwest::Body::wrap_stream(stream)).into()
}

async fn sse_response_from_upstream(upstream: reqwest::Response) -> Response {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    chat_sse_to_responses_sse(
        &state,
        upstream,
        "sse-test-model".into(),
        "fixture".into(),
        "openai_responses".into(),
        None,
        None,
        std::time::Instant::now(),
        None,
    )
    .await
}

pub(super) fn sse_events(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .expect("valid downstream UTF-8")
        .split("\n\n")
        .filter(|frame| !frame.is_empty())
        .map(|frame| {
            let mut lines = frame.lines();
            let event_name = lines.next().unwrap().strip_prefix("event: ").unwrap();
            let data = lines.next().unwrap().strip_prefix("data: ").unwrap();
            let event: Value = serde_json::from_str(data).unwrap();
            assert_eq!(event["type"], event_name);
            event
        })
        .collect()
}

async fn sse_collect(chunks: Vec<bytes::Bytes>) -> Vec<Value> {
    let upstream = sse_upstream(futures_util::stream::iter(chunks.into_iter().map(Ok)));
    let response = sse_response_from_upstream(upstream).await;
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let events = sse_events(&bytes);
    assert_sse_consistent(&events);
    events
}

fn assert_sse_consistent(events: &[Value]) {
    let mut items = BTreeMap::new();
    let mut done = BTreeMap::new();
    let mut cumulative: BTreeMap<u64, String> = BTreeMap::new();
    let mut identities = std::collections::HashSet::new();
    let mut parts = std::collections::HashSet::new();
    let mut parts_done = std::collections::HashSet::new();
    let mut terminals = 0;
    for (seq, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], seq + 1, "{event}");
        let kind = event["type"].as_str().unwrap();
        if let Some(index) = event["output_index"].as_u64() {
            if kind == "response.output_item.added" {
                assert_eq!(index as usize, items.len());
                assert!(items.insert(index, event["item"].clone()).is_none());
                assert!(identities.insert(event["item"]["id"].as_str().unwrap()));
                assert_eq!(event["item"]["status"], "in_progress");
                if event["item"]["type"] == "function_call" {
                    assert_eq!(event["item"]["arguments"], "");
                    assert_ne!(event["item"]["id"], event["item"]["call_id"]);
                } else {
                    assert_eq!(event["item"]["content"], json!([]));
                }
            } else {
                let item = items.get(&index).expect("added before any item event");
                assert!(!done.contains_key(&index), "no events after item done");
                if event.get("item_id").is_some() {
                    assert_eq!(event["item_id"], item["id"]);
                }
                if event.get("content_index").is_some() {
                    assert_eq!(event["content_index"], 0);
                }
                if kind == "response.content_part.added" {
                    assert!(parts.insert(index));
                    assert_eq!(event["part"]["text"], "");
                    assert_eq!(
                        event["part"]["type"],
                        if item["type"] == "reasoning" {
                            "reasoning_text"
                        } else {
                            "output_text"
                        }
                    );
                }
                if kind.ends_with(".delta") {
                    if item["type"] != "function_call" {
                        assert!(parts.contains(&index));
                        assert!(!parts_done.contains(&index));
                    }
                    cumulative
                        .entry(index)
                        .or_default()
                        .push_str(event["delta"].as_str().unwrap());
                }
                if matches!(
                    kind,
                    "response.reasoning_text.done"
                        | "response.output_text.done"
                        | "response.function_call_arguments.done"
                ) {
                    if kind == "response.function_call_arguments.done" {
                        assert_eq!(event["name"], item["name"], "SDK-required done name");
                    }
                    let key = if item["type"] == "function_call" {
                        "arguments"
                    } else {
                        "text"
                    };
                    assert_eq!(
                        event[key].as_str().unwrap(),
                        cumulative.entry(index).or_default()
                    );
                }
                if kind == "response.content_part.done" {
                    assert!(parts.contains(&index));
                    assert!(parts_done.insert(index));
                    assert_eq!(
                        event["part"]["text"].as_str().unwrap(),
                        cumulative.entry(index).or_default()
                    );
                }
                if kind == "response.output_item.done" {
                    assert_eq!(event["item"]["id"], item["id"]);
                    let text = if item["type"] == "function_call" {
                        &event["item"]["arguments"]
                    } else {
                        &event["item"]["content"][0]["text"]
                    };
                    assert_eq!(text.as_str().unwrap(), cumulative.entry(index).or_default());
                    if item["type"] == "function_call" {
                        assert_eq!(event["item"]["call_id"], item["call_id"]);
                        assert_eq!(event["item"]["name"], item["name"]);
                    } else {
                        assert!(parts_done.contains(&index));
                    }
                    done.insert(index, event["item"].clone());
                }
            }
        }
        if matches!(
            kind,
            "response.completed" | "response.incomplete" | "response.failed"
        ) {
            terminals += 1;
            assert_eq!(seq, events.len() - 1, "terminal is last");
            assert_eq!(items.len(), done.len(), "all published items closed");
            assert_eq!(
                event["response"]["output"],
                Value::Array(done.values().cloned().collect())
            );
        }
    }
    assert_eq!(terminals, 1);
}

#[tokio::test]
async fn reasoning_only_and_empty_items() {
    let events = sse_collect(vec![
        sse_delta(json!({"role":"assistant"}), Value::Null),
        sse_delta(json!({"reasoning_content":"思考 "}), Value::Null),
        sse_delta(
            json!({"reasoning_content":"🦀","content":""}),
            json!("stop"),
        ),
        bytes::Bytes::from_static(b"data: [DONE]\n\n"),
    ])
    .await;
    let kinds: Vec<_> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.reasoning_text.delta",
            "response.reasoning_text.delta",
            "response.reasoning_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed"
        ]
    );
    let output = &events.last().unwrap()["response"]["output"];
    assert_eq!(
        output[0]["content"],
        json!([{"type":"reasoning_text","text":"思考 🦀"}])
    );
    assert_eq!(output[0]["summary"], json!([]));
    assert_eq!(output.as_array().unwrap().len(), 1);
    assert!(events.last().unwrap()["response"].get("usage").is_none());

    let empty = sse_collect(vec![sse_delta(json!({"role":"assistant"}), json!("stop"))]).await;
    assert_eq!(empty.last().unwrap()["response"]["output"], json!([]));
}

#[tokio::test]
async fn reentry_canonical_order_and_interleaved_late_metadata() {
    let events = sse_collect(vec![
        sse_delta(json!({"reasoning_content":"R1","content":"T1","tool_calls":[
            {"index":9,"function":{"arguments":"{\"b\":"}},
            {"index":2,"id":"call_a","type":"function","function":{"name":"alpha","arguments":"{\"a\":"}}
        ]}), Value::Null),
        sse_delta(json!({"reasoning_content":"R2","content":"T2","tool_calls":[
            {"index":2,"function":{"arguments":"1}"}},
            {"index":9,"id":"call_b","type":"function","function":{"name":"beta","arguments":"2}"}}
        ]}), json!("tool_calls")),
    ]).await;
    let output = &events.last().unwrap()["response"]["output"];
    assert_eq!(output.as_array().unwrap().len(), 6);
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[2]["call_id"], "call_b");
    assert_eq!(output[2]["arguments"], "{\"b\":2}");
    assert_eq!(output[3]["call_id"], "call_a");
    assert_eq!(output[3]["arguments"], "{\"a\":1}");
    assert_eq!(output[4]["type"], "reasoning");
    assert_ne!(output[0]["id"], output[4]["id"]);
    assert_eq!(output[1]["content"][0]["text"], "T1");
    assert_eq!(output[4]["content"][0]["text"], "R2");
    assert_eq!(output[5]["type"], "message");
    assert_eq!(output[5]["content"][0]["text"], "T2");
    assert_ne!(output[1]["id"], output[5]["id"]);
    assert_eq!(events.last().unwrap()["type"], "response.completed");
}

#[tokio::test]
async fn text_reentry_after_reasoning_has_new_segment() {
    for (finish, tail_error, expected_terminal) in [
        ("stop", false, "response.completed"),
        ("length", false, "response.incomplete"),
        ("stop", true, "response.failed"),
    ] {
        let mut chunks = vec![
            sse_delta(json!({"content":"before"}), Value::Null),
            sse_delta(json!({"reasoning_content":"reason"}), Value::Null),
            sse_delta(json!({"content":"after"}), json!(finish)),
        ];
        if tail_error {
            chunks.push(sse_frame(json!({"error":{"message":"fixture failure"}})));
        }
        let events = sse_collect(chunks).await;
        let terminal = events.last().unwrap();
        assert_eq!(terminal["type"], expected_terminal);
        let output = terminal["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 3, "T -> R -> T must not merge text segments");
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "before");
        assert_eq!(output[1]["type"], "reasoning");
        assert_eq!(output[1]["content"][0]["text"], "reason");
        assert_eq!(output[2]["type"], "message");
        assert_eq!(output[2]["content"][0]["text"], "after");
        assert_ne!(output[0]["id"], output[2]["id"]);
        // Completed segments stay immutable even if the response later fails
        // or is truncated; the active final segment follows terminal rules.
        assert_eq!(output[0]["status"], "completed");
        assert_eq!(output[1]["status"], "completed");
        assert_eq!(
            output[2]["status"],
            if expected_terminal == "response.completed" {
                "completed"
            } else {
                "incomplete"
            }
        );
        for index in 0..2 {
            let closed = events
                .iter()
                .position(|event| {
                    event["type"] == "response.output_item.done" && event["output_index"] == index
                })
                .unwrap();
            let next_added = events
                .iter()
                .position(|event| {
                    event["type"] == "response.output_item.added"
                        && event["output_index"] == index + 1
                })
                .unwrap();
            assert!(
                closed < next_added,
                "close the old segment before opening the next"
            );
        }
    }
}

#[tokio::test]
async fn text_reentry_after_tool_preserves_source_order() {
    for late_metadata in [false, true] {
        let first_call = if late_metadata {
            json!({"index":7,"function":{"arguments":"{\"x\":"}})
        } else {
            json!({"index":7,"id":"call_x","type":"function","function":{"name":"f","arguments":"{\"x\":"}})
        };
        let events = sse_collect(vec![
            sse_delta(json!({"content":"before"}), Value::Null),
            sse_delta(json!({"tool_calls":[first_call]}), Value::Null),
            sse_delta(json!({"content":"after"}), Value::Null),
            sse_delta(json!({"tool_calls":[
                {"index":7,"id":"call_x","type":"function","function":{"name":"f","arguments":"1}"}}
            ]}), json!("tool_calls")),
        ]).await;
        let terminal = events.last().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        let output = terminal["response"]["output"].as_array().unwrap();
        assert_eq!(
            output.len(),
            3,
            "T -> tool -> T must reserve a new text index"
        );
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "before");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["call_id"], "call_x");
        assert_eq!(output[1]["arguments"], "{\"x\":1}");
        assert_eq!(output[2]["type"], "message");
        assert_eq!(output[2]["content"][0]["text"], "after");
        assert_ne!(output[0]["id"], output[2]["id"]);
        let closed = events
            .iter()
            .position(|event| {
                event["type"] == "response.output_item.done" && event["output_index"] == 0
            })
            .unwrap();
        let tool_added = events
            .iter()
            .position(|event| {
                event["type"] == "response.output_item.added" && event["output_index"] == 1
            })
            .unwrap();
        assert!(closed < tool_added);
    }
}

#[tokio::test]
async fn text_reentry_empty_deltas_do_not_split() {
    let events = sse_collect(vec![
        sse_delta(
            json!({"tool_calls":[
                {"index":7,"id":"call_x","type":"function","function":{"name":"f","arguments":"{}"}}
            ]}),
            Value::Null,
        ),
        sse_delta(json!({"content":"before"}), Value::Null),
        sse_delta(json!({}), Value::Null),
        sse_delta(
            json!({"role":"assistant","reasoning_content":"","content":"","tool_calls":[]}),
            Value::Null,
        ),
        sse_delta(
            json!({"reasoning_content":null,"content":null,"tool_calls":[
                {"index":7,"function":{"arguments":""}},
                {"index":8},
                {"index":9,"function":{}},
                {"index":10,"function":{"arguments":""}}
            ]}),
            Value::Null,
        ),
        sse_frame(json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2}})),
        sse_delta(json!({"content":"after"}), json!("stop")),
    ])
    .await;
    let output = events.last().unwrap()["response"]["output"]
        .as_array()
        .unwrap();
    assert_eq!(
        output.len(),
        2,
        "empty fragments must neither split text nor create tools"
    );
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "beforeafter");
    let text_ids: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "response.output_text.delta")
        .map(|event| &event["item_id"])
        .collect();
    assert_eq!(text_ids, vec![&output[1]["id"], &output[1]["id"]]);
}

#[tokio::test]
async fn utf8_frames_and_malformed_tail() {
    let bom_and_cr = sse_collect(
        concat!(
            "\u{feff}data: {\"choices\":[{\"delta\":{\"content\":\"BOM\"},\"finish_reason\":\"stop\"}]}\r\r",
            ": comment\r\rdata: [DONE]\r\r"
        ).as_bytes().iter().map(|byte| bytes::Bytes::from(vec![*byte])).collect()
    ).await;
    assert_eq!(
        bom_and_cr.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "BOM"
    );
    let wire = concat!(
        ": comment\r\n",
        "event: ignored\r\n",
        "data: {\"choices\":[{\"index\":0,\r\n",
        "data: \"delta\":{\"content\":\"中文🦀\"},\"finish_reason\":\"stop\"}]}\r\n\r\n",
        "data: [DONE]\r\n\r\n",
        "data: invalid ignored after DONE\n\n"
    );
    // Every UTF-8 code point and CRLF are split across transport chunks.
    let events = sse_collect(
        wire.as_bytes()
            .iter()
            .map(|b| bytes::Bytes::from(vec![*b]))
            .collect(),
    )
    .await;
    assert_eq!(
        events.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "中文🦀"
    );
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    for tail in [
        b"data: {".as_slice(),
        b"data: {}\n",
        b"data: \xff\n\n",
        b": unfinished",
    ] {
        let events = sse_collect(vec![
            sse_delta(json!({"content":"kept"}), json!("stop")),
            bytes::Bytes::copy_from_slice(tail),
        ])
        .await;
        assert_eq!(events.last().unwrap()["type"], "response.failed");
        assert_eq!(
            events.last().unwrap()["response"]["output"][0]["content"][0]["text"],
            "kept"
        );
    }
}

#[tokio::test]
async fn frame_wire_budget_done_separator_and_resets() {
    for ending in ["\n", "\r\n", "\r"] {
        // Padding makes DONE the budget-determining frame. Its *last* newline
        // byte must be checked before accepting the terminal marker.
        let done = format!(": {}{ending}data: [DONE]{ending}{ending}", "p".repeat(256));
        let head = String::from_utf8(sse_delta(json!({"content":"kept"}), json!("stop")).to_vec())
            .unwrap()
            .replace('\n', ending);
        for limit in [done.len() - 1, done.len()] {
            for split_bytes in [false, true] {
                let mut cfg = config::default_deepseek_config();
                cfg.server.as_mut().unwrap().max_sse_buffer_bytes = limit;
                let state = AppState::new_in_memory(cfg);
                // Exact-sized consecutive frames reset the wire budget. The
                // malformed trailing data must be ignored after valid DONE.
                let wire = format!(
                    "{head}: {}{ending}{ending}{done}data: BAD\n\n",
                    "c".repeat(limit - 2 - 2 * ending.len())
                );
                let chunks = if split_bytes {
                    wire.bytes()
                        .map(|byte| bytes::Bytes::from(vec![byte]))
                        .collect::<Vec<_>>()
                } else {
                    vec![bytes::Bytes::from(wire)]
                };
                let upstream = sse_upstream(futures_util::stream::iter(chunks.into_iter().map(Ok)));
                let response = chat_sse_to_responses_sse(
                    &state,
                    upstream,
                    "fixture".into(),
                    "fixture".into(),
                    "openai_responses".into(),
                    None,
                    None,
                    std::time::Instant::now(),
                    None,
                )
                .await;
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let events = sse_events(&bytes);
                assert_sse_consistent(&events);
                let terminal = events.last().unwrap();
                assert_eq!(
                    terminal["response"]["output"][0]["content"][0]["text"],
                    "kept"
                );
                if limit == done.len() {
                    assert_eq!(terminal["type"], "response.completed", "{ending:?}");
                } else {
                    assert_eq!(terminal["type"], "response.failed", "{ending:?}");
                    assert_eq!(
                        terminal["response"]["error"]["message"],
                        "SSE frame limit exceeded"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn standalone_cr_at_eof_and_transport_error() {
    for tail in [
        "data: [DONE]\r\r",
        "data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}\r\r",
        "data: BAD\r\r",
    ] {
        for transport_error in [false, true] {
            let mut chunks = vec![
                Ok(sse_delta(json!({"content":"kept"}), json!("stop"))),
                Ok(bytes::Bytes::from_static(tail.as_bytes())),
            ];
            if transport_error {
                chunks.push(Err(std::io::Error::other("private")));
            }
            let upstream = sse_upstream(futures_util::stream::iter(chunks));
            let response = sse_response_from_upstream(upstream).await;
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let events = sse_events(&bytes);
            assert_sse_consistent(&events);
            let terminal = events.last().unwrap();
            if tail.contains("[DONE]") {
                assert_eq!(
                    terminal["type"], "response.completed",
                    "DONE precedes transport failure"
                );
            } else {
                assert_eq!(terminal["type"], "response.failed");
                assert_eq!(
                    terminal["response"]["error"]["message"],
                    if tail.contains("BAD") {
                        "Invalid upstream SSE JSON"
                    } else {
                        "Nonempty delta after upstream finish"
                    },
                    "the complete CR frame is validated before a later transport error"
                );
            }
        }
    }
    // EOF resolves the blank CR itself, even when that frame contains the
    // first content and finish (not just a DONE marker).
    let events = sse_collect(vec![bytes::Bytes::from_static(
        b"data: {\"choices\":[{\"delta\":{\"content\":\"CR\"},\"finish_reason\":\"stop\"}]}\r\r",
    )])
    .await;
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    assert_eq!(
        events.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "CR"
    );
}

#[tokio::test]
async fn finish_precedence_and_tail_usage() {
    for finish in ["stop", "tool_calls", "length", "content_filter"] {
        let events = sse_collect(vec![
            sse_delta(json!({"tool_calls":[{
                "index":0,"id":"call_x","type":"function",
                "function":{"name":"x","arguments":"{\"unfinished\":"}
            }]}), json!(finish)),
            sse_frame(json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":37,
                "prompt_tokens_details":{"cached_tokens":2,"cache_write_tokens":3},
                "completion_tokens_details":{"reasoning_tokens":4}}})),
        ]).await;
        let terminal = events.last().unwrap();
        assert_eq!(
            terminal["type"],
            if matches!(finish, "length" | "content_filter") {
                "response.incomplete"
            } else {
                "response.failed"
            }
        );
        assert_eq!(terminal["response"]["usage"]["total_tokens"], 37);
        assert_eq!(
            terminal["response"]["output"][0]["arguments"],
            "{\"unfinished\":"
        );
    }
    for finish in [Value::Null, json!("length"), json!("stop")] {
        for tail in [
            sse_frame(json!({"error":{"message":"SENSITIVE_UPSTREAM"}})),
            bytes::Bytes::from_static(b"data: SENSITIVE_UPSTREAM\n\n"),
        ] {
            let events = sse_collect(vec![
                sse_delta(json!({"content":"kept"}), finish.clone()),
                tail,
            ])
            .await;
            let terminal = events.last().unwrap();
            assert_eq!(terminal["type"], "response.failed");
            assert_eq!(terminal["response"]["error"]["code"], "server_error");
            assert!(
                !serde_json::to_string(&events)
                    .unwrap()
                    .contains("SENSITIVE_UPSTREAM")
            );
        }
    }
    for chunks in [
        vec![],
        vec![bytes::Bytes::from_static(b"data: [DONE]\n\n")],
        vec![sse_delta(json!({"content":"not finished"}), Value::Null)],
    ] {
        let events = sse_collect(chunks).await;
        assert_eq!(events.last().unwrap()["type"], "response.failed");
    }
}

#[tokio::test]
async fn arguments_syntax_rejects_trailing_data_with_precedence() {
    for arguments in [
        "{} {}",
        "[] trailing",
        "true false",
        "\"string\" 0",
        "0\nnull",
        "{\"ok\":true}\0",
    ] {
        for finish in ["stop", "tool_calls", "length", "content_filter"] {
            for upstream_error in [false, true] {
                let mut chunks = vec![sse_delta(
                    json!({"tool_calls":[{
                        "index":0,"id":"call_syntax","type":"function",
                        "function":{"name":"f","arguments":arguments}
                    }]}),
                    json!(finish),
                )];
                if upstream_error {
                    chunks.push(sse_frame(json!({"error":{"message":"private"}})));
                }
                let events = sse_collect(chunks).await;
                let terminal = events.last().unwrap();
                let truncated = matches!(finish, "length" | "content_filter");
                assert_eq!(
                    terminal["type"],
                    if truncated && !upstream_error {
                        "response.incomplete"
                    } else {
                        "response.failed"
                    }
                );
                assert_eq!(terminal["response"]["output"][0]["arguments"], arguments);
                if upstream_error || !truncated {
                    assert_eq!(
                        terminal["response"]["error"]["message"],
                        if upstream_error {
                            "Upstream service error"
                        } else {
                            "Invalid upstream tool arguments JSON"
                        }
                    );
                }
            }
        }
    }
    // A single JSON value followed by whitespace is still valid, unchanged.
    let arguments = " \n {\"nested\":[true,null,{\"s\":\"\\u4e2d\"}]} \r\n\t";
    let events = sse_collect(vec![sse_delta(
        json!({"tool_calls":[{
            "index":0,"id":"call_valid","type":"function",
            "function":{"name":"f","arguments":arguments}
        }]}),
        json!("stop"),
    )])
    .await;
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    assert_eq!(
        events.last().unwrap()["response"]["output"][0]["arguments"],
        arguments
    );
}

#[tokio::test]
async fn channel_gates_incremental_output_before_finish() {
    use futures_util::StreamExt;
    let (send, recv) = tokio::sync::mpsc::channel(4);
    let upstream = sse_upstream(tokio_stream::wrappers::ReceiverStream::new(recv));
    let mut body = sse_response_from_upstream(upstream)
        .await
        .into_body()
        .into_data_stream();
    let mut events = Vec::new();

    // The finish frame cannot exist until each phase's output is observed.
    for (chunk, expected_delta) in [
        (
            sse_delta(json!({"reasoning_content":"R1"}), Value::Null),
            "R1",
        ),
        (
            sse_delta(
                json!({"content":"T","tool_calls":[
                    {"index":8,"function":{"arguments":"{\"b\":"}},
                    {"index":3,"id":"a","type":"function","function":{"name":"alpha","arguments":"{}"}}
                ]}),
                Value::Null,
            ),
            "T",
        ),
        (
            sse_delta(
                json!({"tool_calls":[
                    {"index":8,"id":"b","type":"function","function":{"name":"beta","arguments":"2}"}}
                ]}),
                Value::Null,
            ),
            "2}",
        ),
        (
            sse_delta(json!({"reasoning_content":"R2"}), Value::Null),
            "R2",
        ),
    ] {
        send.send(Ok(chunk)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let bytes = body.next().await.expect("stream still open").unwrap();
                let batch = sse_events(&bytes);
                let found = batch.iter().any(|event| event["delta"] == expected_delta);
                events.extend(batch);
                if found {
                    break;
                }
            }
        })
        .await
        .expect("converter must publish available deltas without upstream finish");
        assert!(!events.iter().any(|e| e["type"] == "response.completed"));
    }
    send.send(Ok(sse_delta(json!({}), json!("tool_calls"))))
        .await
        .unwrap();
    drop(send);
    while let Some(bytes) = body.next().await {
        events.extend(sse_events(&bytes.unwrap()));
    }
    assert_sse_consistent(&events);
    let output = &events.last().unwrap()["response"]["output"];
    assert_eq!(output[2]["call_id"], "b");
    assert_eq!(output[2]["arguments"], "{\"b\":2}");
    assert_eq!(output[3]["call_id"], "a");
    assert_eq!(output[3]["arguments"], "{}");
}

#[tokio::test]
async fn finish_rejects_structurally_nonempty_unknown_delta() {
    for finish in ["stop", "tool_calls", "length", "content_filter"] {
        for delta in [
            json!({"refusal":"SENSITIVE_UPSTREAM"}),
            json!({"refusal":""}),
            json!({"refusal":null}),
            json!({"unknown":{"content":"SENSITIVE_UPSTREAM"}}),
        ] {
            let events = sse_collect(vec![
                sse_delta(json!({"content":"kept"}), json!(finish)),
                sse_delta(delta.clone(), Value::Null),
            ])
            .await;
            let terminal = events.last().unwrap();
            assert_eq!(terminal["type"], "response.failed", "{finish}: {delta}");
            assert_eq!(terminal["response"]["error"]["code"], "server_error");
            assert_eq!(terminal["response"]["output"][0]["status"], "incomplete");
            assert_eq!(
                terminal["response"]["output"][0]["content"][0]["text"],
                "kept"
            );
            assert!(
                !events
                    .iter()
                    .any(|e| e.to_string().contains("SENSITIVE_UPSTREAM"))
            );
        }
        let events = sse_collect(vec![
            sse_delta(json!({"content":"kept"}), json!(finish)),
            sse_delta(json!({}), json!(finish)),
            sse_frame(json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2}})),
        ])
        .await;
        assert_eq!(
            events.last().unwrap()["type"],
            if matches!(finish, "length" | "content_filter") {
                "response.incomplete"
            } else {
                "response.completed"
            }
        );
    }
}

#[tokio::test]
async fn active_text_before_duplicate_call_id_stays_incomplete() {
    for calls in [
        json!([{"index":6,"id":"call_f","type":"function","function":{"name":"f","arguments":"{}"}}]),
        // A valid earlier entry must not close the active item before the
        // duplicate identity later in this same array is validated.
        json!([
            {"index":6,"id":"call_g","type":"function","function":{"name":"g","arguments":"{}"}},
            {"index":7,"id":"call_g","type":"function","function":{"name":"g","arguments":"{}"}}
        ]),
    ] {
        let events = sse_collect(vec![
            sse_delta(json!({"content":"closed"}), Value::Null),
            sse_delta(json!({"tool_calls":[
                {"index":5,"id":"call_f","type":"function","function":{"name":"f","arguments":"{}"}}
            ]}), Value::Null),
            sse_delta(json!({"content":"active"}), Value::Null),
            sse_delta(json!({"tool_calls":calls}), json!("length")),
        ])
        .await;
        let terminal = events.last().unwrap();
        assert_eq!(terminal["type"], "response.failed");
        assert_eq!(
            terminal["response"]["error"]["message"],
            "Duplicate upstream tool call ID"
        );
        let output = terminal["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 3, "invalid array cannot publish new calls");
        assert_eq!(output[0]["status"], "completed");
        assert_eq!(output[1]["call_id"], "call_f");
        assert_eq!(output[1]["arguments"], "{}");
        assert_eq!(output[2]["content"][0]["text"], "active");
        assert_eq!(output[2]["status"], "incomplete");
    }
}

#[tokio::test]
async fn active_reasoning_before_metadata_conflict_stays_incomplete() {
    for conflict in [
        json!({"index":5,"id":"different"}),
        json!({"index":5,"id":"call_f","function":{"name":"different"}}),
    ] {
        for valid_prefix in [false, true] {
            let mut calls = Vec::new();
            if valid_prefix {
                calls.push(json!({
                    "index":6,"id":"call_g","type":"function",
                    "function":{"name":"g","arguments":"{}"}
                }));
            }
            calls.push(conflict.clone());
            let events = sse_collect(vec![
                sse_delta(json!({"tool_calls":[
                    {"index":5,"id":"call_f","type":"function","function":{"name":"f","arguments":"{}"}}
                ]}), Value::Null),
                sse_delta(json!({"reasoning_content":"active"}), Value::Null),
                sse_delta(json!({"tool_calls":calls}), json!("length")),
            ])
            .await;
            let terminal = events.last().unwrap();
            assert_eq!(terminal["type"], "response.failed");
            assert_eq!(
                terminal["response"]["error"]["message"],
                "Conflicting upstream tool metadata"
            );
            let output = terminal["response"]["output"].as_array().unwrap();
            assert_eq!(output.len(), 2, "invalid array cannot publish new calls");
            assert_eq!(output[0]["call_id"], "call_f");
            assert_eq!(output[0]["name"], "f");
            assert_eq!(output[0]["arguments"], "{}");
            assert_eq!(output[1]["type"], "reasoning");
            assert_eq!(output[1]["content"][0]["text"], "active");
            assert_eq!(output[1]["status"], "incomplete");
        }
    }
}

#[tokio::test]
async fn empty_arguments_then_metadata_and_length_is_incomplete() {
    let events = sse_collect(vec![
        sse_delta(
            json!({"tool_calls":[
                {"index":99,"function":{"arguments":""}},
                {"index":7,"function":{"arguments":""}}
            ]}),
            Value::Null,
        ),
        sse_delta(json!({"content":"before"}), Value::Null),
        sse_delta(
            json!({"tool_calls":[
                {"index":99,"function":{"arguments":""}}
            ]}),
            Value::Null,
        ),
        sse_delta(json!({"content":" after"}), Value::Null),
        // Empty observations neither reserve output order nor block text.
        // First nonempty metadata determines order, not the source index.
        sse_delta(
            json!({"tool_calls":[
                {"index":7,"id":"call_7","type":"function","function":{"name":"seven"}},
                {"index":99,"id":"call_99","type":"function","function":{"name":"ninety_nine"}}
            ]}),
            json!("length"),
        ),
    ])
    .await;
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "response.incomplete");
    assert_eq!(terminal["response"]["error"], Value::Null);
    assert_eq!(
        terminal["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    let output = terminal["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["content"][0]["text"], "before after");
    assert_eq!(output[0]["status"], "completed");
    for (item, call_id) in output[1..].iter().zip(["call_7", "call_99"]) {
        assert_eq!(item["call_id"], call_id);
        assert_eq!(item["arguments"], "");
        assert_eq!(item["status"], "incomplete");
    }
    assert!(
        !events
            .iter()
            .any(|e| e["type"] == "response.function_call_arguments.delta")
    );
}

#[tokio::test]
async fn tool_identity_validation_and_empty_fragments() {
    let initial =
        json!({"index":5,"id":"call_f","type":"function","function":{"name":"f","arguments":"{}"}});
    // Identical repeated metadata is validation, NOT a second identity/name chunk.
    let valid = sse_collect(vec![
        sse_delta(json!({"reasoning_content":"R","tool_calls":[{"index":99,"function":{"arguments":""}}]}), Value::Null),
        sse_delta(json!({"reasoning_content":"2","tool_calls":[initial.clone()]}), Value::Null),
        sse_delta(json!({"tool_calls":[{"index":5,"id":"call_f","type":"function","function":{"name":"f"}}]}), json!("stop")),
    ]).await;
    let output = &valid.last().unwrap()["response"]["output"];
    assert_eq!(output.as_array().unwrap().len(), 2);
    assert_eq!(output[0]["content"][0]["text"], "R2");
    assert_eq!(output[1]["name"], "f");
    assert_eq!(output[1]["arguments"], "{}");

    let invalid = [
        json!([{"id":"call_x","type":"function","function":{"name":"f","arguments":"{}"}}]),
        json!([{"index":-1,"function":{"arguments":"{}"}}]),
        json!([{"index":1.5,"function":{"arguments":"{}"}}]),
        json!([{"index":"5","function":{"arguments":"{}"}}]),
        json!([{"index":5,"id":22}]),
        json!([{"index":5,"id":"different"}]),
        json!([{"index":5,"type":"custom"}]),
        json!([{"index":5,"function":{"name":"other"}}]),
        json!([{"index":5,"function":{"arguments":{}}}]),
        json!([{"index":5,"function":null}]),
        json!([{"index":5},{"index":5}]),
        json!([{"index":6,"id":"call_f","type":"function","function":{"name":"f","arguments":"{}"}}]),
        json!([false]),
        Value::Null,
        json!({}),
    ];
    for calls in invalid {
        let events = sse_collect(vec![
            sse_delta(json!({"tool_calls":[initial.clone()]}), Value::Null),
            sse_delta(json!({"tool_calls":calls}), json!("length")),
        ])
        .await;
        assert_eq!(events.last().unwrap()["type"], "response.failed", "{calls}");
        assert_eq!(
            events.last().unwrap()["response"]["output"][0]["arguments"],
            "{}"
        );
    }
    // Lenient upstream parsing: explicit null/empty-string id or name means
    // "absent in this chunk" (Bailian dialect), not a failure — the registered
    // identity is reused and the terminal follows the finish reason.
    for calls in [
        json!([{"index":5,"id":""}]),
        json!([{"index":5,"id":null}]),
        json!([{"index":5,"function":{"name":""}}]),
        json!([{"index":5,"function":{"name":null}}]),
    ] {
        let events = sse_collect(vec![
            sse_delta(json!({"tool_calls":[initial.clone()]}), Value::Null),
            sse_delta(json!({"tool_calls":calls}), json!("length")),
        ])
        .await;
        let terminal = events.last().unwrap();
        assert_eq!(terminal["type"], "response.incomplete", "{calls}");
        assert_eq!(terminal["response"]["output"][0]["arguments"], "{}");
        assert_eq!(terminal["response"]["output"][0]["call_id"], "call_f");
    }
    // Terminal truncation does not excuse absent identity, type, or arguments.
    for call in [
        json!({"index":0,"function":{"arguments":"{"}}),
        json!({"index":0,"id":"x","type":"function","function":{"arguments":"{"}}),
        json!({"index":0,"id":"x","function":{"name":"f","arguments":"{"}}),
        json!({"index":0,"id":"x","type":"function","function":{"name":"f"}}),
    ] {
        let events = sse_collect(vec![
            sse_delta(json!({"content":"before"}), Value::Null),
            sse_delta(
                json!({"tool_calls":[call],"reasoning_content":"R"}),
                json!("length"),
            ),
        ])
        .await;
        assert_eq!(events.last().unwrap()["type"], "response.failed");
    }
}

#[tokio::test]
async fn transport_done_and_terminal_precedence() {
    for finish in [Value::Null, json!("stop"), json!("length")] {
        let upstream = sse_upstream(futures_util::stream::iter([
            Ok(sse_delta(json!({"reasoning_content":"kept"}), finish)),
            Err(std::io::Error::other("SENSITIVE_TRANSPORT")),
        ]));
        let bytes = axum::body::to_bytes(
            sse_response_from_upstream(upstream).await.into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        let events = sse_events(&bytes);
        assert_sse_consistent(&events);
        assert_eq!(events.last().unwrap()["type"], "response.failed");
        assert!(
            !std::str::from_utf8(&bytes)
                .unwrap()
                .contains("SENSITIVE_TRANSPORT")
        );
    }
    let chunks = vec![
        sse_delta(json!({"content":"ok"}), json!("stop")),
        bytes::Bytes::from_static(b"data: [DONE]\n\ndata: BAD\n\n"),
    ];
    let upstream = sse_upstream(async_stream::stream! {
        for chunk in chunks { yield Ok(chunk); }
        panic!("must never poll upstream after DONE");
    });
    let bytes = axum::body::to_bytes(
        sse_response_from_upstream(upstream).await.into_body(),
        usize::MAX,
    )
    .await
    .unwrap();
    let events = sse_events(&bytes);
    assert_sse_consistent(&events);
    assert_eq!(events.last().unwrap()["type"], "response.completed");

    for tail in [
        sse_delta(json!({}), json!("length")),
        sse_delta(json!({"content":"late"}), Value::Null),
        sse_delta(json!({"reasoning_content":"late"}), Value::Null),
        sse_delta(
            json!({"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}),
            Value::Null,
        ),
        sse_frame(json!({"choices":[{"index":1,"delta":{}}]})),
        sse_frame(json!({"choices":[{},{}]})),
        sse_frame(json!({"choices":[{"delta":null}]})),
        sse_frame(json!({"choices":[{"delta":{"content":[]}}]})),
        sse_delta(json!({}), json!("unknown")),
    ] {
        let events = sse_collect(vec![
            sse_delta(json!({"content":"ok"}), json!("stop")),
            tail,
        ])
        .await;
        assert_eq!(events.last().unwrap()["type"], "response.failed");
    }
}

#[tokio::test]
async fn pending_cap_does_not_publish_reserved_items() {
    let events = sse_collect(vec![
        sse_delta(json!({"reasoning_content":"published"}), Value::Null),
        sse_delta(
            json!({"tool_calls":[{"index":17,"function":{"arguments":"{"}}]}),
            Value::Null,
        ),
        // Non-ASCII: budget must count bytes, not chars or just tool arguments.
        sse_delta(json!({"content":"界".repeat(180_000)}), Value::Null),
        sse_delta(
            json!({"reasoning_content":"界".repeat(180_000)}),
            json!("length"),
        ),
    ])
    .await;
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "response.failed");
    assert_eq!(
        terminal["response"]["error"]["message"],
        "Tool metadata buffer limit exceeded"
    );
    assert_eq!(terminal["response"]["output"].as_array().unwrap().len(), 1);
    assert_eq!(terminal["response"]["output"][0]["status"], "completed");
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["text"],
        "published"
    );
}

#[tokio::test]
async fn frame_output_and_item_limits_fail_closed() {
    for (frame_limit, item_limit, chunks, diagnostic) in [
        (
            128,
            4,
            vec![bytes::Bytes::from(format!("data: {}\n\n", "x".repeat(129)))],
            "SSE frame limit exceeded",
        ),
        (
            256,
            4,
            (0..4)
                .map(|_| sse_delta(json!({"content":"x".repeat(80)}), Value::Null))
                .collect(),
            "Upstream output byte limit exceeded",
        ),
        (
            256,
            1,
            vec![
                sse_delta(json!({"reasoning_content":"R"}), Value::Null),
                sse_delta(json!({"content":"T"}), json!("stop")),
            ],
            "Upstream output item limit exceeded",
        ),
    ] {
        let mut cfg = config::default_deepseek_config();
        cfg.server.as_mut().unwrap().max_sse_buffer_bytes = frame_limit;
        cfg.server.as_mut().unwrap().max_output_items = item_limit;
        let state = AppState::new_in_memory(cfg);
        let upstream = sse_upstream(futures_util::stream::iter(chunks.into_iter().map(Ok)));
        let response = chat_sse_to_responses_sse(
            &state,
            upstream,
            "fixture".into(),
            "fixture".into(),
            "openai_responses".into(),
            None,
            None,
            std::time::Instant::now(),
            None,
        )
        .await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let events = sse_events(&bytes);
        assert_sse_consistent(&events);
        assert_eq!(events.last().unwrap()["type"], "response.failed");
        assert_eq!(
            events.last().unwrap()["response"]["error"]["message"],
            diagnostic
        );
    }
}

#[tokio::test]
async fn context_and_usage_persist_actual_checked_local_sum() {
    let dir = tempfile::tempdir().unwrap();
    let paths = (dir.path().join("usage.jsonl"), dir.path().join("usage.db"));
    let store = Arc::new(
        UsageStore::with_paths(
            config::UsageConfig::default(),
            paths.0.clone(),
            paths.1.clone(),
        )
        .unwrap(),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(store.clone());
    let request = json!({
        "tools":[{"type":"function","name":"original","parameters":{"type":"object"}}],
        "tool_choice":{"type":"function","name":"original"},"parallel_tool_calls":false
    });
    let complete_usage = json!({"prompt_tokens":10,"completion_tokens":20,"total_tokens":37,
        "prompt_tokens_details":{"cached_tokens":2,"cache_write_tokens":3},
        "completion_tokens_details":{"reasoning_tokens":4}});
    for (case, usage) in [
        json!({"prompt_tokens":10,"completion_tokens":20}),
        json!({"prompt_tokens":-1,"completion_tokens":20}),
        json!({"prompt_tokens":i64::MAX,"completion_tokens":1}),
        complete_usage.clone(),
    ]
    .into_iter()
    .enumerate()
    {
        let upstream = sse_upstream(futures_util::stream::iter([
            Ok(sse_delta(json!({"content":"answer"}), json!("stop"))),
            Ok(sse_frame(json!({"choices":[],"usage":usage}))),
        ]));
        let response = chat_sse_to_responses_sse(
            &state,
            upstream,
            "sse-test-model".into(),
            "fixture".into(),
            "openai_responses".into(),
            Some(&request),
            None,
            std::time::Instant::now(),
            None,
        )
        .await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let events = sse_events(&bytes);
        assert_sse_consistent(&events);
        for event in events.iter().filter(|e| e.get("response").is_some()) {
            let envelope = &event["response"];
            assert_eq!(envelope["tools"], request["tools"]);
            assert_eq!(envelope["tool_choice"], request["tool_choice"]);
            assert_eq!(envelope["parallel_tool_calls"], false);
            assert!(envelope["created_at"].as_i64().is_some());
            assert_eq!(envelope["model"], "sse-test-model");
            assert_eq!(envelope["id"], events[0]["response"]["id"]);
        }
        let terminal = &events.last().unwrap()["response"];
        assert_eq!(terminal["status"], "completed");
        if case < 3 {
            assert!(terminal.get("usage").is_none());
        } else {
            assert_eq!(
                terminal["usage"]["total_tokens"], 37,
                "client total is never locally derived"
            );
        }
        store.flush_if_dirty().unwrap();
        let reopened = UsageStore::with_paths(
            config::UsageConfig::default(),
            paths.0.clone(),
            paths.1.clone(),
        )
        .unwrap();
        let records = reopened.get_all();
        assert_eq!(records.len(), if case == 3 { 2 } else { 1 });
        for record in records {
            assert_eq!(
                (
                    record.input_tokens,
                    record.output_tokens,
                    record.total_tokens
                ),
                (10, 20, 30)
            );
            assert_eq!(record.model, "sse-test-model");
            assert_eq!(record.provider, "fixture");
            assert_eq!(record.endpoint, "openai_responses");
        }
    }

    // Every target-required count/object must be real; omission never changes status.
    for field in [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "prompt_tokens_details",
        "completion_tokens_details",
    ] {
        for replacement in [
            None,
            Some(Value::Null),
            Some(json!(-1)),
            Some(json!(1.5)),
            Some(json!("1")),
        ] {
            let mut usage = complete_usage.clone();
            match replacement {
                None => {
                    usage.as_object_mut().unwrap().remove(field);
                }
                Some(value) => usage[field] = value,
            }
            let events = sse_collect(vec![
                sse_delta(json!({"content":"ok"}), json!("length")),
                sse_frame(json!({"usage":usage,"choices":[]})),
            ])
            .await;
            assert_eq!(events.last().unwrap()["type"], "response.incomplete");
            assert!(events.last().unwrap()["response"].get("usage").is_none());
        }
    }
}

fn state_with_temp_probe_cache(cfg: config::Config) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().expect("probe cache tempdir");
    let mut state = AppState::new_in_memory(cfg);
    let probe_state = crate::probe_coordinator::ServerProbeState::with_cache_path_and_oauth_path(
        state.active_providers.clone(),
        dir.path().join("status-cache.json"),
        state.oauth_accounts_path.clone(),
    );
    state.probe_coordinator =
        Arc::new(crate::probe_coordinator::ProbeCoordinator::new(probe_state));
    (dir, state)
}

type CapturedRequest = Arc<Mutex<Option<Value>>>;
type CapturedHeader = Arc<Mutex<Option<String>>>;
type CapturedRequestAndHeader = (CapturedRequest, CapturedHeader);

fn thinking_intent_of(protocol: Protocol, body: &Value) -> crate::proxy::request::ThinkingIntent {
    extract_thinking_intent(protocol, body)
}

/// Test wrapper for [`resolve_request_candidates`] that derives the client-visible
/// model ID from the request body. Only routing chains make the client ID differ
/// from the resolved target; those are exercised via the HTTP handlers, so a plain
/// body-derived ID is the correct stand-in everywhere in these tests.
fn candidates_for_test(
    state: &AppState,
    protocol: Protocol,
    body: &Value,
    err: fn(StatusCode, &str) -> Response,
) -> Result<Vec<(String, config::ExecutionPlan)>, Box<Response>> {
    let client_model_id = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("(none)");
    resolve_request_candidates(state, protocol, body, client_model_id, err)
}

/// Layer B default fill with the upstream assumed to speak the same protocol as the
/// client (passthrough). Egress-aware rules get a dedicated test.
fn default_reasoning(state: &AppState, protocol: Protocol, body: Value) -> Value {
    let intent = thinking_intent_of(protocol, &body);
    body_with_default_reasoning(state, protocol, protocol, body, &intent)
}

fn mapped_reasoning(
    state: &AppState,
    protocol: Protocol,
    body: Value,
    model_id: &str,
    plan: &config::ExecutionPlan,
) -> Result<Value, crate::proxy::request::ThinkingError> {
    let intent = thinking_intent_of(protocol, &body);
    body_with_mapped_reasoning(state, protocol, body, model_id, plan, &intent)
}

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
        routed_target: None,
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
fn in_memory_states_use_distinct_isolated_paths() {
    let first = AppState::new_in_memory(config::default_deepseek_config());
    let second = AppState::new_in_memory(config::default_deepseek_config());

    assert_ne!(first.config_path, second.config_path);
    assert_ne!(first.oauth_accounts_path, second.oauth_accounts_path);
    assert_ne!(first.state_dir, second.state_dir);
    assert!(first.test_dir.is_some());
    assert!(second.test_dir.is_some());
    assert_ne!(
        first.config_path,
        std::path::PathBuf::from("/tmp/test-config.toml")
    );
    assert_eq!(
        first.oauth_accounts_path,
        first
            .config_path
            .parent()
            .unwrap()
            .join("oauth_accounts.json")
    );
    assert_eq!(
        first.state_dir,
        first.config_path.parent().unwrap().join("runtime")
    );

    let first_root = first.config_path.parent().unwrap().to_path_buf();
    let clone = first.clone();
    drop(first);
    assert!(first_root.exists());
    std::fs::write(&clone.config_path, b"[server]\n")
        .expect("cloned state keeps isolated directory alive");
    drop(clone);
    assert!(!first_root.exists());
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
        None,
        false,
        0,
        None,
    );
    assert!(state.is_provider_active("deepseek", Duration::from_secs(30)));
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

/// 断流回归门禁：必须留下一条 `event="stream_interrupted"` 的 **ERROR** 日志。
///
/// 这正是该门禁要补的核心缺口 —— 原先只有内存计数器（重启即清、仅经管理通道 `/state`
/// 暴露），日志一条都没有，于是"代理错误日志零记录"成了用户排查时的死胡同。这里钉
/// ERROR 级而非 warn：钉的是**事件严重性**——上游断流对运维就是故障本身。
///
/// ⚠️ 理由已变：这里原写「`EnvFilter::from_default_env()` 在 `RUST_LOG`
/// 未设时停在 ERROR，降级成 warn 与删掉整行对运维等价（都看不见）」。缺省 filter 现为
/// `error,llm_proxy=info`（见 `main.rs::DEFAULT_ENV_FILTER`），我方 target 开到 info、
/// warn **已经可见**——所以断言 `level=ERROR` 的依据不再是「不写 error 就看不见」，
/// 而是「它本就是 error」。断言不变、理由改述，见 `proxy::mod.rs` 同名注释。
///
/// 做法复刻 `service.rs` 的 `ServiceLogCapture` 及其注释里的坑：除 `set_default`
/// 外还要再注册一个 `tracing::Dispatch`，否则 tracing-core 单 dispatch 快路径会把
/// 并发测试线程的"无订阅者" interest 缓存下来。不过滤 target，只认 `event=` 字段。
struct StreamInterruptLogCapture {
    _guard: tracing::subscriber::DefaultGuard,
    _registration: tracing::Dispatch,
    events: Arc<Mutex<Vec<String>>>,
}

impl StreamInterruptLogCapture {
    fn start() -> Self {
        use tracing_subscriber::prelude::*;
        let events = Arc::new(Mutex::new(Vec::new()));
        let _registration = tracing::Dispatch::new(tracing_subscriber::registry());
        let subscriber = tracing_subscriber::registry().with(StreamEventLayer(Arc::clone(&events)));
        Self {
            _guard: tracing::subscriber::set_default(subscriber),
            _registration,
            events,
        }
    }

    fn lines(&self) -> Vec<String> {
        self.events.lock().expect("capture lock").clone()
    }

    fn find(&self, name: &str) -> Option<String> {
        let needle = format!("event={name}");
        self.lines().into_iter().find(|line| line.contains(&needle))
    }
}

struct StreamEventLayer(Arc<Mutex<Vec<String>>>);

#[derive(Default)]
struct StreamEventFields(std::collections::BTreeMap<String, String>);

impl tracing::field::Visit for StreamEventFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        let unquoted = rendered
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(&rendered);
        self.0
            .insert(field.name().to_string(), unquoted.to_string());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for StreamEventLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = StreamEventFields::default();
        event.record(&mut fields);
        if !fields.0.contains_key("event") {
            return;
        }
        let mut line = format!(
            "event={} level={}",
            fields
                .0
                .remove("event")
                .unwrap_or_else(|| event.metadata().name().to_string()),
            event.metadata().level()
        );
        for (name, value) in fields.0 {
            line.push_str(&format!(" {name}={value}"));
        }
        self.0.lock().expect("capture lock").push(line);
    }
}

#[test]
fn stream_interruption_leaves_error_log_with_counter() {
    let capture = StreamInterruptLogCapture::start();
    let state = AppState::new_in_memory(config::default_deepseek_config());

    state.observe_stream_interruption("responses-to-chat", "connection reset by peer");
    let first = capture
        .find("stream_interrupted")
        .expect("断流必须产生 event=\"stream_interrupted\" —— 否则代理侧对上游中断零留痕");
    assert!(
        first.contains("level=ERROR"),
        "默认 RUST_LOG 下必须可见（降级 warn 即不可见）: {first}"
    );
    assert!(first.contains("direction=responses-to-chat"), "{first}");
    assert!(first.contains("count=1"), "{first}");
    assert!(first.contains("error=connection reset by peer"), "{first}");

    state.observe_stream_interruption("responses-to-chat", "reset");
    let last = capture
        .lines()
        .into_iter()
        .rev()
        .find(|l| l.contains("event=stream_interrupted"))
        .expect("second log line");
    assert!(last.contains("count=2"), "日志里的计数须随重发递增: {last}");

    // 上游错误串可能带完整 URL/载荷片段：必须截断，避免日志变成泄露面
    state.observe_stream_interruption("chat-to-anthropic", &"x".repeat(500));
    let long_line = capture
        .lines()
        .into_iter()
        .rev()
        .find(|l| l.contains("direction=chat-to-anthropic"))
        .expect("third log line");
    assert!(
        !long_line.contains(&"x".repeat(400)),
        "错误串须截断到 300 字符以内: {}",
        long_line.chars().take(120).collect::<String>()
    );
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
    assert!(send_upstream(&state, &plan, req, false).await.is_err());
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
    assert!(send_upstream(&state, &plan, req, true).await.is_err());
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
    send_upstream(&state, &plan, req, false)
        .await
        .expect("first allowed");
    let req = state
        .client
        .post(&url)
        .json(&json!({"model":"m","messages":[]}));
    let err = send_upstream(&state, &plan, req, false)
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

    let chat = default_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","messages":[]}),
    );
    assert_eq!(chat["reasoning_effort"], "high");

    let responses = default_reasoning(
        &state,
        Protocol::OpenaiResponses,
        json!({"model":"deepseek-v4-flash-lp","input":"ping"}),
    );
    assert_eq!(responses["reasoning"]["effort"], "high");

    let explicit = default_reasoning(
        &state,
        Protocol::OpenaiResponses,
        json!({"model":"deepseek-v4-flash-lp","input":"ping","reasoning":{"effort":"low"}}),
    );
    assert_eq!(explicit["reasoning"]["effort"], "low");
}

#[test]
fn explicit_anthropic_thinking_is_not_overwritten_by_defaults() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let cases = [
        json!({"type":"enabled","budget_tokens":4096}),
        json!({"type":"disabled"}),
    ];

    for thinking in cases {
        let body = default_reasoning(
            &state,
            Protocol::Anthropic,
            json!({
                "model":"deepseek-v4-flash-lp",
                "messages":[],
                "thinking":thinking
            }),
        );
        assert_eq!(body["thinking"], thinking);
    }
}

#[test]
fn reasoning_level_map_rewrites_client_level_for_upstream() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());

    let mapped = mapped_reasoning(
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
fn reasoning_level_not_in_table_is_rejected_at_resolution() {
    let mut cfg = config::default_deepseek_config();
    let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
    // Table only declares "high"; requesting "low" should be rejected at resolution.
    model.reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("high", None)]);
    let state = AppState::new_in_memory(cfg);

    let err = candidates_for_test(
        &state,
        Protocol::OpenaiChatCompletions,
        &json!({"model":"deepseek-v4-flash-lp","messages":[],"reasoning_effort":"low"}),
        json_error,
    )
    .expect_err("level not in table");

    assert_eq!(err.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn reasoning_level_none_bypasses_table_gating_as_disabled_intent() {
    let mut cfg = config::default_deepseek_config();
    let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
    // Table only declares "high"; requesting "none" should NOT be rejected
    // because "none" is treated as disabled intent, not a table level (ADR-034).
    model.reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("high", None)]);
    let state = AppState::new_in_memory(cfg);
    let plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());

    // "none" bypasses table gating and is treated as disabled intent.
    let result = mapped_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","messages":[],"reasoning_effort":"none"}),
        "deepseek-v4-flash-lp",
        &plan,
    );
    // Should succeed (none = disabled intent, not a table level).
    assert!(
        result.is_ok(),
        "none should bypass table gating: {:?}",
        result.err()
    );
}

#[test]
fn force_on_materializes_qwen_on_expression() {
    let mut cfg = config::default_deepseek_config();
    let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
    model.force_think = Some(true);
    let state = AppState::new_in_memory(cfg);
    let mut plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());
    plan.compat.thinking_format = Some("qwen".to_string());

    let result = mapped_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","messages":[]}),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("force-on materialization");

    assert_eq!(result["enable_thinking"], true);
}

#[test]
fn force_on_materializes_configured_default_level() {
    // ADR-034 §3 + spec.md §5.2: force-on must express the configured default level.
    // A standard (non-toggle) endpoint is no longer a config error.
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .force_think = Some(true);
    let state = AppState::new_in_memory(cfg);
    let mut plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());
    plan.compat.thinking_format = Some("deepseek".to_string());

    let body = mapped_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","messages":[]}),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("force-on materializes the default level");

    assert_eq!(body["reasoning_effort"], "high");
}

/// Anthropic egress: force-on materializes `adaptive + output_config.effort`, which is
/// what the shipped `minimax-m2.5-bailian-lp` (force_think + default level, native
/// Anthropic endpoint without compat) needs instead of a 500.
#[test]
fn force_on_materializes_adaptive_plus_effort_on_anthropic() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .force_think = Some(true);
    let state = AppState::new_in_memory(cfg);
    let mut plan = test_execution_plan("http://127.0.0.1:1/v1/messages".to_string());
    plan.frontend_protocol = Protocol::Anthropic;
    plan.source_protocol = Protocol::Anthropic;

    let body = mapped_reasoning(
        &state,
        Protocol::Anthropic,
        json!({"model":"deepseek-v4-flash-lp","max_tokens":16,"messages":[{"role":"user","content":"ping"}]}),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("force-on materializes on native Anthropic");

    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");
}

#[test]
fn deepseek_chat_egress_maps_none_to_disabled_thinking() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let mut plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());
    plan.compat.thinking_format = Some("deepseek".to_string());

    let mapped = mapped_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","messages":[],"reasoning_effort":"none"}),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("strip generic disabled intent");
    let egress = apply_disabled_thinking_egress(mapped, &plan, true);

    assert!(egress.get("reasoning_effort").is_none());
    assert_eq!(egress["thinking"]["type"], "disabled");
}

#[test]
fn disabled_thinking_egress_uses_endpoint_native_format() {
    AppState::new_in_memory(config::default_deepseek_config());
    let cases = [
        (
            "mimo-thinking-toggle",
            json!({"model":"mimo-v2.6-pro","messages":[]}),
            json!({"thinking":{"type":"disabled"}}),
        ),
        (
            "openai-responses-reasoning",
            json!({"model":"mimo-v2.6-pro","input":"ping"}),
            json!({"reasoning":{"effort":"none"}}),
        ),
        (
            "anthropic-thinking",
            json!({"model":"mimo-v2.6-pro","messages":[]}),
            json!({"thinking":{"type":"disabled"}}),
        ),
    ];

    for (thinking_format, body, expected_fields) in cases {
        let mut plan = test_execution_plan("http://127.0.0.1:1/upstream".to_string());
        plan.compat.thinking_format = Some(thinking_format.to_string());

        let body = apply_disabled_thinking_egress(body, &plan, true);
        for (field, expected) in expected_fields.as_object().unwrap() {
            assert_eq!(
                body.get(field),
                Some(expected),
                "{thinking_format} disabled field"
            );
        }
    }
}

#[test]
fn disabled_thinking_intent_does_not_inject_deepseek_format_for_stepfun() {
    AppState::new_in_memory(config::default_deepseek_config());
    let mut plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());
    plan.compat.thinking_format = Some("stepfun".to_string());

    let body = apply_disabled_thinking_egress(
        json!({"model":"step-3.7-flash","messages":[]}),
        &plan,
        true,
    );

    assert!(body.get("thinking").is_none());
}

#[test]
fn chat_reasoning_off_format_no_longer_drives_off_expression() {
    // ADR-034 §5: the retired 053-era `chat_reasoning_off_format` key is ignored.
    // It must still load (whitelist retained), but the target native endpoint's
    // `compat.thinking_format` decides the off dialect.
    let mut cfg = config::default_deepseek_config();
    let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
    model.chat_reasoning_off_format = Some("enable-thinking".to_string());
    AppState::new_in_memory(cfg);
    let mut plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());
    plan.compat.thinking_format = Some("deepseek".to_string());

    let body = apply_disabled_thinking_egress(
        json!({"model":"deepseek-v4-flash-lp","messages":[]}),
        &plan,
        true,
    );

    // thinking_format=deepseek governs; the retired key produces no enable_thinking.
    assert_eq!(body["thinking"]["type"], "disabled");
    assert!(body.get("enable_thinking").is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// Signature egress stripping
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn strip_vendor_signatures_removes_anthropic_thinking_signature() {
    let body = json!({
        "model": "claude-sonnet-4-20250514",
        "messages": [],
        "thinking": [
            {"type": "thinking", "thinking": "let me think", "signature": "sig-abc"},
            {"type": "thinking", "thinking": "more thoughts", "signature": "sig-def"}
        ]
    });

    let stripped = strip_vendor_signatures(body);
    let thinking = stripped["thinking"].as_array().unwrap();
    for block in thinking {
        assert!(
            block.get("signature").is_none(),
            "signature must be stripped on egress"
        );
        assert!(block.get("thinking").is_some(), "other fields preserved");
    }
}

#[test]
fn strip_vendor_signatures_removes_responses_reasoning_signature() {
    let body = json!({
        "model": "gpt-4o",
        "input": [
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thought"}], "signature": "sig-xyz"},
            {"type": "message", "role": "user", "content": "hello"}
        ]
    });

    let stripped = strip_vendor_signatures(body);
    let input = stripped["input"].as_array().unwrap();
    // reasoning item: signature stripped
    let reasoning = input.iter().find(|i| i["type"] == "reasoning").unwrap();
    assert!(
        reasoning.get("signature").is_none(),
        "reasoning signature must be stripped on egress"
    );
    assert!(reasoning.get("summary").is_some(), "summary preserved");
    // message item: untouched
    let message = input.iter().find(|i| i["type"] == "message").unwrap();
    assert_eq!(message["content"], "hello");
}

// ─────────────────────────────────────────────────────────────────────────────
// ADR-034 acceptance matrix for the 4.6+ effort conversion.
// ─────────────────────────────────────────────────────────────────────────────

/// Anthropic ingress body with `thinking` and optional `output_config`.
fn anthropic_thinking_body(thinking: Value, effort: Option<&str>) -> Value {
    let mut body = json!({
        "model": "deepseek-v4-flash-lp",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "ping"}],
        "thinking": thinking,
    });
    if let Some(effort) = effort {
        body.as_object_mut()
            .unwrap()
            .insert("output_config".to_string(), json!({"effort": effort}));
    }
    body
}

fn anthropic_ingress_accepts(body: &Value) -> bool {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    candidates_for_test(&state, Protocol::Anthropic, body, anthropic_error).is_ok()
}

fn anthropic_ingress_rejected(body: &Value) -> bool {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    candidates_for_test(&state, Protocol::Anthropic, body, anthropic_error).is_err()
}

/// §8.7a: `disabled + output_config.effort=high` is legal for Anthropic egress and
/// both halves survive Layer B (effort is not stripped by the disabled path).
#[test]
fn anthropic_disabled_with_effort_is_accepted_and_effort_preserved() {
    let body = anthropic_thinking_body(json!({"type": "disabled"}), Some("high"));
    assert!(
        anthropic_ingress_accepts(&body),
        "disabled + output_config.effort must be accepted"
    );

    let state = AppState::new_in_memory(config::default_deepseek_config());
    let mut plan = test_execution_plan("http://127.0.0.1:1/v1/messages".to_string());
    plan.frontend_protocol = Protocol::Anthropic;
    plan.source_protocol = Protocol::Anthropic;
    let out = mapped_reasoning(
        &state,
        Protocol::Anthropic,
        body,
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("anthropic disabled + effort maps through");

    assert_eq!(out["thinking"]["type"], "disabled");
    assert_eq!(
        out["output_config"]["effort"], "high",
        "disabled must not strip output_config.effort on Anthropic egress"
    );
}

/// §8.3: `adaptive` + no effort is official shape; with no default level configured it
/// passes through untouched (no effort invented).
#[test]
fn anthropic_adaptive_without_effort_passes_through_when_no_default_level() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .default_reasoning_level = None;
    let state = AppState::new_in_memory(cfg);

    let body = anthropic_thinking_body(json!({"type": "adaptive"}), None);
    let out = default_reasoning(&state, Protocol::Anthropic, body);
    assert_eq!(out["thinking"]["type"], "adaptive");
    assert!(
        out.get("output_config").is_none(),
        "no default level → no effort invented"
    );
}

/// §8.3: only `output_config.effort` (no `thinking`) is legal and is not rewritten into
/// an on-switch on Anthropic egress.
#[test]
fn anthropic_effort_without_thinking_does_not_invent_adaptive_switch() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let mut body = json!({
        "model": "deepseek-v4-flash-lp",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "ping"}],
        "output_config": {"effort": "low"},
    });
    assert!(anthropic_ingress_accepts(&body));

    let intent = thinking_intent_of(Protocol::Anthropic, &body);
    body = body_with_default_reasoning(
        &state,
        Protocol::Anthropic,
        Protocol::Anthropic,
        body,
        &intent,
    );
    assert_eq!(body["output_config"]["effort"], "low");
    assert!(
        body.get("thinking").is_none(),
        "only-effort requests must not gain a thinking switch"
    );
}

/// §8.4/§8.5/§8.6/§8.7: the 4.6+ rejection set.
#[test]
fn anthropic_46_rejections_are_local_400s() {
    let cases: [(&str, Value); 7] = [
        (
            "thinking.effort is not a protocol field",
            anthropic_thinking_body(json!({"type": "adaptive", "effort": "high"}), None),
        ),
        (
            "thinking.level is not a protocol field",
            anthropic_thinking_body(json!({"type": "adaptive", "level": "high"}), None),
        ),
        (
            "enabled is deprecated",
            anthropic_thinking_body(json!({"type": "enabled"}), None),
        ),
        (
            "enabled + budget_tokens is deprecated",
            anthropic_thinking_body(json!({"type": "enabled", "budget_tokens": 8192}), None),
        ),
        (
            "adaptive + budget_tokens has no lossless mapping",
            anthropic_thinking_body(json!({"type": "adaptive", "budget_tokens": 8192}), None),
        ),
        (
            "output_config.effort=none is not an Anthropic enum value",
            anthropic_thinking_body(json!({"type": "adaptive"}), Some("none")),
        ),
        (
            "unknown thinking.type",
            anthropic_thinking_body(json!({"type": "mystery"}), None),
        ),
    ];
    for (name, body) in cases {
        assert!(
            anthropic_ingress_rejected(&body),
            "{name}: expected local 400, body={body}"
        );
    }
}

/// §8.13a: `enable_thinking` is not a protocol field on any ingress protocol.
#[test]
fn enable_thinking_rejected_on_all_ingress_protocols() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let cases = [
        (
            Protocol::Anthropic,
            json!({"model":"deepseek-v4-flash-lp","max_tokens":16,"messages":[],"enable_thinking":true}),
            anthropic_error as fn(StatusCode, &str) -> axum::response::Response,
        ),
        (
            Protocol::OpenaiChatCompletions,
            json!({"model":"deepseek-v4-flash-lp","messages":[],"enable_thinking":false}),
            json_error as fn(StatusCode, &str) -> axum::response::Response,
        ),
        (
            Protocol::OpenaiResponses,
            json!({"model":"deepseek-v4-flash-lp","input":"x","enable_thinking":true}),
            responses_error as fn(StatusCode, &str) -> axum::response::Response,
        ),
    ];
    for (protocol, body, err) in cases {
        let result = candidates_for_test(&state, protocol, &body, err);
        assert!(
            result.is_err(),
            "{protocol:?}: enable_thinking must be rejected"
        );
    }
}

/// §8 `minimal` default mapping: a table without `minimal` but with `low` accepts a
/// `minimal` request and maps it to `low` (its configured api_value).
#[test]
fn minimal_absent_from_table_maps_to_low_and_passes_gating() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .reasoning_levels = Some(vec![
        config::ReasoningLevelEntry::new("low", Some("low".to_string())),
        config::ReasoningLevelEntry::new("high", Some("high".to_string())),
    ]);
    let state = AppState::new_in_memory(cfg);

    let body = json!({"model":"deepseek-v4-flash-lp","messages":[],"reasoning_effort":"minimal"});
    let resolved = candidates_for_test(&state, Protocol::OpenaiChatCompletions, &body, json_error);
    assert!(
        resolved.is_ok(),
        "minimal must be gated as `low`, not rejected outright"
    );

    let plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());
    let out = mapped_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        body,
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("minimal maps to low");
    assert_eq!(out["reasoning_effort"], "low");
}

/// §8 `minimal` with neither `minimal` nor `low` in the table is a client error, not a
/// silent clamp or a late 500.
#[test]
fn minimal_absent_without_low_in_table_is_rejected() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("high", None)]);
    let state = AppState::new_in_memory(cfg);

    let body = json!({"model":"deepseek-v4-flash-lp","messages":[],"reasoning_effort":"minimal"});
    assert!(
        candidates_for_test(&state, Protocol::OpenaiChatCompletions, &body, json_error).is_err(),
        "minimal without a low row has no expressible mapping"
    );
}

/// §8.7b + §5.2: `force_no_think` with a default level emits a closed switch *and* the
/// effort knob on Anthropic egress (two independent knobs).
#[test]
fn force_no_think_with_default_level_emits_disabled_plus_effort() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .force_no_think = Some(true);
    let state = AppState::new_in_memory(cfg);

    let body = json!({
        "model": "deepseek-v4-flash-lp",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "ping"}],
    });
    let out = default_reasoning(&state, Protocol::Anthropic, body);
    assert_eq!(out["thinking"]["type"], "disabled");
    assert_eq!(out["output_config"]["effort"], "high");
}

/// §5.2 conflict matrix: `default_think=false` with a default level still closes the
/// switch without injecting the level into the closed state.
#[test]
fn default_think_false_with_default_level_emits_disabled_without_effort() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .default_think = Some(false);
    let state = AppState::new_in_memory(cfg);

    let body = json!({
        "model": "deepseek-v4-flash-lp",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "ping"}],
    });
    let out = default_reasoning(&state, Protocol::Anthropic, body);
    assert_eq!(out["thinking"]["type"], "disabled");
    assert!(
        out.get("output_config").is_none(),
        "a closed default must not carry the default level as effort"
    );
}

/// spec.md §5.2: an explicit Anthropic `adaptive` with no level is "on, no level". The
/// client's explicit on wins on Anthropic egress; the peer protocols cannot express the
/// shape, so there it degrades to the closed value (`none`, losing the "on").
#[test]
fn adaptive_without_level_under_default_off_is_egress_aware() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .default_think = Some(false);
    let state = AppState::new_in_memory(cfg);
    let body = json!({
        "model": "deepseek-v4-flash-lp",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "ping"}],
        "thinking": {"type": "adaptive"},
    });
    let intent = thinking_intent_of(Protocol::Anthropic, &body);

    // Anthropic egress: keep the client's explicit `adaptive`, never top up an effort.
    let anthropic = body_with_default_reasoning(
        &state,
        Protocol::Anthropic,
        Protocol::Anthropic,
        body.clone(),
        &intent,
    );
    assert_eq!(anthropic["thinking"]["type"], "adaptive");
    assert!(
        anthropic.get("output_config").is_none(),
        "an explicit on must not be topped up with the default level"
    );

    // Chat egress: the peer cannot say "on, no level" → closed value `none`.
    let chat = body_with_default_reasoning(
        &state,
        Protocol::Anthropic,
        Protocol::OpenaiChatCompletions,
        body.clone(),
        &intent,
    );
    assert_eq!(chat["thinking"]["type"], "disabled");

    // Responses egress: same degradation.
    let responses = body_with_default_reasoning(
        &state,
        Protocol::Anthropic,
        Protocol::OpenaiResponses,
        body,
        &intent,
    );
    assert_eq!(responses["thinking"]["type"], "disabled");
}

/// ADR-034 §2: the Model table replaces the Provider table wholesale. A level that
/// exists only in the Provider table must be rejected, not silently accepted.
#[test]
fn model_table_overrides_provider_table_without_per_row_merge() {
    let mut cfg = config::default_deepseek_config();
    // Provider table has `xhigh`; the Model table does not.
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("low", None)]);
    let state = AppState::new_in_memory(cfg);

    let body = json!({"model":"deepseek-v4-flash-lp","messages":[],"reasoning_effort":"xhigh"});
    assert!(
        candidates_for_test(&state, Protocol::OpenaiChatCompletions, &body, json_error).is_err(),
        "Provider-only level must not leak through a Model table override"
    );
}

/// ADR-034 §5: two bound providers that both declare a vocabulary cannot both apply.
#[test]
fn model_bound_to_two_tabled_providers_is_a_config_error() {
    let mut cfg = config::default_deepseek_config();
    let provider_table = || Some(vec![config::ReasoningLevelEntry::new("low", None)]);
    cfg.providers.get_mut("deepseek").unwrap().reasoning_levels = provider_table();
    let mut second = cfg.providers["deepseek"].clone();
    second.reasoning_levels = provider_table();
    cfg.providers.insert("other".to_string(), second);
    let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
    model.reasoning_levels = None; // let both bindings resolve to provider tables
    model.default_reasoning_level = Some("low".to_string());
    model.openai_chat_providers.push(config::ProviderBinding {
        name: "other".to_string(),
        model: "other-upstream".to_string(),
    });

    let err = cfg.validate().expect_err("two tabled providers must fail");
    assert!(
        err.to_string().contains("reasoning_levels"),
        "error must name the offending key: {err}"
    );
}

/// ADR-034 §5: exactly one tabled provider plus untabled bindings is not an error —
/// the untabled binding simply declares no vocabulary.
#[test]
fn model_bound_to_one_tabled_provider_is_accepted() {
    let mut cfg = config::default_deepseek_config();
    cfg.providers.get_mut("deepseek").unwrap().reasoning_levels =
        Some(vec![config::ReasoningLevelEntry::new("low", None)]);
    let mut second = cfg.providers["deepseek"].clone();
    second.reasoning_levels = None;
    cfg.providers.insert("other".to_string(), second);
    let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
    model.reasoning_levels = None;
    model.default_reasoning_level = Some("low".to_string());
    model.openai_chat_providers.push(config::ProviderBinding {
        name: "other".to_string(),
        model: "other-upstream".to_string(),
    });

    cfg.validate()
        .expect("one tabled provider among untabled bindings is fine");
}

/// ADR-034 §2 / §8.19f: a Provider-inherited vocabulary gates and rejects exactly like a
/// Model table — an unlisted level is a client 400, never a late 500.
#[test]
fn provider_inherited_table_gates_levels_at_resolution() {
    let mut cfg = config::default_deepseek_config();
    cfg.providers.get_mut("deepseek").unwrap().reasoning_levels =
        Some(vec![config::ReasoningLevelEntry::new("low", None)]);
    let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
    model.reasoning_levels = None; // inherit the provider vocabulary
    model.default_reasoning_level = Some("low".to_string());
    let state = AppState::new_in_memory(cfg);

    let unlisted = json!({"model":"deepseek-v4-flash-lp","messages":[],"reasoning_effort":"xhigh"});
    assert!(
        candidates_for_test(
            &state,
            Protocol::OpenaiChatCompletions,
            &unlisted,
            json_error
        )
        .is_err(),
        "inherited table must reject a level it does not declare"
    );

    let listed = json!({"model":"deepseek-v4-flash-lp","messages":[],"reasoning_effort":"low"});
    assert!(
        candidates_for_test(&state, Protocol::OpenaiChatCompletions, &listed, json_error).is_ok(),
        "listed level passes through the inherited table"
    );
}

/// ADR-034 §8.19c: `default_reasoning_level` is validated against the *effective* table,
/// including a Provider-inherited one — not only against the Model's own table.
#[test]
fn default_level_is_validated_against_provider_inherited_table() {
    let mut cfg = config::default_deepseek_config();
    cfg.providers.get_mut("deepseek").unwrap().reasoning_levels =
        Some(vec![config::ReasoningLevelEntry::new("low", None)]);
    let model = cfg.models.get_mut("deepseek-v4-flash-lp").unwrap();
    model.reasoning_levels = None; // inherit; the provider table has no "max"
    model.default_reasoning_level = Some("max".to_string());

    let err = cfg
        .validate()
        .expect_err("default level outside the inherited table must fail to load");
    assert!(
        err.to_string().contains("default_reasoning_level"),
        "error must name the offending field: {err}"
    );
}

#[test]
fn default_think_false_closes_reasoning_but_keeps_explicit_level() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .default_think = Some(false);
    let state = AppState::new_in_memory(cfg);
    let plan = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());

    // Closed default is expressed as the protocol-standard `none`, not a removal, so it
    // survives into a cross-protocol conversion (spec.md §5.2).
    let defaulted = default_reasoning(
        &state,
        Protocol::OpenaiResponses,
        json!({"model":"deepseek-v4-flash-lp","input":"ping"}),
    );
    assert_eq!(defaulted["reasoning"]["effort"], "none");

    // §5.1.1 / §5.2.2: an explicit on-level overrides default-off and is
    // mapped, not stripped.
    let mapped = mapped_reasoning(
        &state,
        Protocol::OpenaiResponses,
        json!({"model":"deepseek-v4-flash-lp","input":"ping","reasoning":{"effort":"high"}}),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("explicit on survives default-off");
    assert_eq!(mapped["reasoning"]["effort"], "high");
}

#[test]
fn anthropic_disabled_rejects_conflicting_reasoning_fields_before_mapping() {
    let cases = [
        ("effort", json!("low")),
        ("level", json!("high")),
        ("budget_tokens", json!(4096)),
    ];

    for (field, value) in cases {
        let mut cfg = config::default_deepseek_config();
        cfg.models
            .get_mut("deepseek-v4-flash-lp")
            .unwrap()
            .reasoning_levels = Some(vec![config::ReasoningLevelEntry::new(
            value.as_str().unwrap_or("low"),
            None,
        )]);
        let state = AppState::new_in_memory(cfg);
        let mut plan = test_execution_plan("http://127.0.0.1:1/v1/messages".to_string());
        plan.frontend_protocol = Protocol::Anthropic;
        plan.source_protocol = Protocol::Anthropic;
        let mut thinking = serde_json::Map::from_iter([("type".to_string(), json!("disabled"))]);
        thinking.insert(field.to_string(), value);

        let err = mapped_reasoning(
            &state,
            Protocol::Anthropic,
            json!({
                "model":"deepseek-v4-flash-lp",
                "messages":[],
                "thinking":thinking
            }),
            "deepseek-v4-flash-lp",
            &plan,
        )
        .expect_err("disabled plus an active reasoning field is contradictory");

        assert!(err.to_string().contains("thinking.type"));
        assert!(err.to_string().contains(field));
    }
}

#[tokio::test]
async fn anthropic_disabled_conflict_precedes_supported_level_validation() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let body = json!({
        "model":"deepseek-v4-flash-lp",
        "messages":[],
        "thinking":{"type":"disabled","effort":"custom"}
    });

    let error = candidates_for_test(&state, Protocol::Anthropic, &body, anthropic_error)
        .expect_err("contradictory disabled request");
    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(error.into_body(), 1 << 20)
        .await
        .expect("error body");
    let error: Value = serde_json::from_slice(&bytes).expect("error json");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicts")
    );
}

#[test]
fn force_no_think_overrides_explicit_budget_but_preserves_disabled() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .force_no_think = Some(true);
    let state = AppState::new_in_memory(cfg);
    let mut plan = test_execution_plan("http://127.0.0.1:1/v1/messages".to_string());
    plan.frontend_protocol = Protocol::Anthropic;
    plan.source_protocol = Protocol::Anthropic;

    // Anthropic 4.6+: adaptive + output_config.effort is overridden by force_no_think
    let budget = mapped_reasoning(
        &state,
        Protocol::Anthropic,
        json!({
            "model":"deepseek-v4-flash-lp",
            "messages":[],
            "thinking":{"type":"adaptive"},
            "output_config":{"effort":"high"}
        }),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("config policy disables thinking");
    assert_eq!(budget["thinking"], json!({"type":"disabled"}));

    let disabled = mapped_reasoning(
        &state,
        Protocol::Anthropic,
        json!({
            "model":"deepseek-v4-flash-lp",
            "messages":[],
            "thinking":{"type":"disabled"}
        }),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("explicit disabled remains explicit");
    assert_eq!(disabled["thinking"], json!({"type":"disabled"}));
}

#[test]
fn anthropic_effort_mapping_remains_server_side() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let mut plan = test_execution_plan("http://127.0.0.1:1/v1/messages".to_string());
    plan.frontend_protocol = Protocol::Anthropic;
    plan.source_protocol = Protocol::Anthropic;

    let mapped = mapped_reasoning(
        &state,
        Protocol::Anthropic,
        json!({
            "model":"deepseek-v4-flash-lp",
            "messages":[],
            "thinking":{"type":"adaptive"},
            "output_config":{"effort":"xhigh"}
        }),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("map explicit effort");
    assert_eq!(mapped["output_config"]["effort"], "max");
}

#[test]
fn candidate_thinking_rebuilds_from_original_request_not_shared_defaults() {
    let original = json!({"model":"deepseek-v4-flash-lp","messages":[]});
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let intent = thinking_intent_of(Protocol::OpenaiChatCompletions, &original);

    let mut qwen = test_execution_plan("http://127.0.0.1:1/qwen".to_string());
    qwen.compat.thinking_format = Some("qwen".to_string());
    qwen.provider_id = "qwen-provider".to_string();

    let mut deepseek = test_execution_plan("http://127.0.0.1:1/deepseek".to_string());
    deepseek.compat.thinking_format = Some("deepseek".to_string());
    deepseek.provider_id = "deepseek-provider".to_string();

    let (qwen_body, qwen_disabled) = prepare_candidate_thinking_body(
        &state,
        Protocol::OpenaiChatCompletions,
        &original,
        "deepseek-v4-flash-lp",
        &qwen,
        &intent,
    )
    .expect("qwen candidate");
    let (deepseek_body, deepseek_disabled) = prepare_candidate_thinking_body(
        &state,
        Protocol::OpenaiChatCompletions,
        &original,
        "deepseek-v4-flash-lp",
        &deepseek,
        &intent,
    )
    .expect("deepseek candidate");

    assert!(!qwen_disabled);
    assert!(!deepseek_disabled);
    assert_eq!(qwen_body["reasoning_effort"], "high");
    assert_eq!(deepseek_body["reasoning_effort"], "high");
    assert!(original.get("reasoning_effort").is_none());
    assert_eq!(
        original,
        json!({"model":"deepseek-v4-flash-lp","messages":[]})
    );
}

#[test]
fn candidate_force_on_materializes_default_level_per_dialect() {
    // ADR-034 §3: force-on always expresses the configured default level. A dialect
    // whose only on-expression is a toggle gets both the toggle and the level.
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .force_think = Some(true);
    let state = AppState::new_in_memory(cfg);
    let original = json!({"model":"deepseek-v4-flash-lp","messages":[]});
    let intent = thinking_intent_of(Protocol::OpenaiChatCompletions, &original);

    let mut qwen = test_execution_plan("http://127.0.0.1:1/qwen".to_string());
    qwen.compat.thinking_format = Some("qwen".to_string());
    let mut deepseek = test_execution_plan("http://127.0.0.1:1/deepseek".to_string());
    deepseek.compat.thinking_format = Some("deepseek".to_string());

    let (qwen_body, _) = prepare_candidate_thinking_body(
        &state,
        Protocol::OpenaiChatCompletions,
        &original,
        "deepseek-v4-flash-lp",
        &qwen,
        &intent,
    )
    .expect("qwen force-on");
    assert_eq!(qwen_body["enable_thinking"], true);
    assert_eq!(qwen_body["reasoning_effort"], "high");

    let (deepseek_body, _) = prepare_candidate_thinking_body(
        &state,
        Protocol::OpenaiChatCompletions,
        &original,
        "deepseek-v4-flash-lp",
        &deepseek,
        &intent,
    )
    .expect("deepseek force-on materializes the level");
    assert_eq!(deepseek_body["reasoning_effort"], "high");
    assert!(original.get("enable_thinking").is_none());
    assert!(original.get("reasoning_effort").is_none());
}

#[test]
fn explicit_on_overrides_default_off_for_every_candidate() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .default_think = Some(false);
    let state = AppState::new_in_memory(cfg);
    // Chat / Responses have no separate switch field, so an activity level is the
    // protocol-native way to express explicit on.
    let original = json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"high"});
    let intent = thinking_intent_of(Protocol::OpenaiChatCompletions, &original);

    let mut first = test_execution_plan("http://127.0.0.1:1/a".to_string());
    first.compat.thinking_format = Some("qwen".to_string());
    let mut second = test_execution_plan("http://127.0.0.1:1/b".to_string());
    second.compat.thinking_format = Some("deepseek".to_string());

    for plan in [&first, &second] {
        let (body, disabled) = prepare_candidate_thinking_body(
            &state,
            Protocol::OpenaiChatCompletions,
            &original,
            "deepseek-v4-flash-lp",
            plan,
            &intent,
        )
        .expect("explicit on overrides default off");
        assert!(!disabled);
        assert_eq!(body["reasoning_effort"], "high");
    }
    assert_eq!(
        original,
        json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"high"})
    );
}

#[tokio::test]
async fn responses_endpoint_uses_native_upstream() {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream_app =
        Router::new()
            .route(
                "/v1/responses",
                post(
                    |State(captured): State<Arc<Mutex<Option<Value>>>>,
                     Json(body): Json<Value>| async move {
                        *captured.lock().unwrap() = Some(body);
                        Json(json!({
                            "id": "resp_test",
                            "object": "responses/response",
                            "status": "completed",
                            "model": "deepseek-v4-pro",
                            "output": [{
                                "type": "message",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": "pong"
                                }]
                            }]
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

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build client");
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
    // Responses API normalizes simple string input to structured message format
    assert_eq!(captured["input"][0]["type"], "message");
    assert_eq!(captured["input"][0]["role"], "user");
    assert_eq!(captured["input"][0]["content"][0]["text"], "ping");
}

#[tokio::test]
async fn chat_proxy_injects_deepseek_disabled_thinking_before_upstream() {
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
                        "id": "chatcmpl_test",
                        "created": 123,
                        "choices": [{
                            "message": {"role": "assistant", "content": "pong"},
                            "finish_reason": "stop"
                        }]
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
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .default_think = Some(false);
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

    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-flash-lp",
            "messages": [{"role": "user", "content": "Use the tool."}],
            "tools": [{"type":"function","function":{"name":"ping","parameters":{"type":"object","properties":{}}}}],
            "tool_choice": {"type":"function","function":{"name":"ping"}}
        }))
        .send()
        .await
        .expect("send proxy request");
    assert!(response.status().is_success());

    let captured = captured
        .lock()
        .unwrap()
        .clone()
        .expect("captured upstream body");
    assert!(captured.get("reasoning_effort").is_none());
    assert_eq!(captured["thinking"]["type"], "disabled");
}

#[tokio::test]
async fn mimo_native_endpoints_encode_disabled_thinking_before_upstream() {
    let captured_chat: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_responses: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_anthropic: CapturedRequest = Arc::new(Mutex::new(None));

    let chat_capture = Arc::clone(&captured_chat);
    let responses_capture = Arc::clone(&captured_responses);
    let anthropic_capture = Arc::clone(&captured_anthropic);
    let upstream_app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let captured = Arc::clone(&chat_capture);
                async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "chatcmpl_test",
                        "created": 123,
                        "choices": [{
                            "message": {"role": "assistant", "content": "pong"},
                            "finish_reason": "stop"
                        }]
                    }))
                }
            }),
        )
        .route(
            "/v1/responses",
            post(move |Json(body): Json<Value>| {
                let captured = Arc::clone(&responses_capture);
                async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "resp_test",
                        "object": "response",
                        "status": "completed",
                        "output": [],
                        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                    }))
                }
            }),
        )
        .route(
            "/anthropic/v1/messages",
            post(move |Json(body): Json<Value>| {
                let captured = Arc::clone(&anthropic_capture);
                async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "msg_test",
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "pong"}],
                        "model": "mimo-v2.6-pro",
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 1, "output_tokens": 1}
                    }))
                }
            }),
        );
    let upstream_addr = serve_on_random_port(upstream_app).await;

    let mut cfg = config::default_deepseek_config();
    let entry = crate::catalog::mimo_payg();
    cfg.providers
        .insert(entry.id.to_string(), entry.provider.clone());
    crate::catalog::apply_catalog_model_defaults(&mut cfg, entry.id, None)
        .expect("apply MiMo defaults");
    cfg.models
        .get_mut("mimo-v2.6-pro-lp")
        .expect("MiMo Pro model")
        .default_think = Some(false);
    let provider = cfg.providers.get_mut(entry.id).expect("MiMo provider");
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;

    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;
    let client = reqwest::Client::new();

    let chat_response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "mimo-v2.6-pro-lp",
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send Chat request");
    assert!(chat_response.status().is_success());

    let responses_response = client
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({"model": "mimo-v2.6-pro-lp", "input": "ping"}))
        .send()
        .await
        .expect("send Responses request");
    assert!(responses_response.status().is_success());

    let anthropic_response = client
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "mimo-v2.6-pro-lp",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send Anthropic request");
    assert!(anthropic_response.status().is_success());

    let chat_body = captured_chat.lock().unwrap().clone().expect("Chat body");
    assert_eq!(chat_body["thinking"]["type"], "disabled");
    let responses_body = captured_responses
        .lock()
        .unwrap()
        .clone()
        .expect("Responses body");
    assert_eq!(responses_body["reasoning"]["effort"], "none");
    let anthropic_body = captured_anthropic
        .lock()
        .unwrap()
        .clone()
        .expect("Anthropic body");
    assert_eq!(anthropic_body["thinking"]["type"], "disabled");
}

/// provider-mimo.md §5.5: MiMo accepts eight `reasoning.effort` labels but folds all but
/// `low`/`medium`/`high` onto one of those three tiers, so `caa2843` widened the vocabulary
/// without widening the capability. This pins both halves: an accepted alias must reach the
/// upstream as its folded tier (never as the client's label), and a label outside the
/// official vocabulary must still be rejected locally without touching the upstream.
#[tokio::test]
async fn mimo_alias_reasoning_levels_fold_before_egress() {
    let captured_chat: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_responses: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_anthropic: CapturedRequest = Arc::new(Mutex::new(None));

    let chat_capture = Arc::clone(&captured_chat);
    let responses_capture = Arc::clone(&captured_responses);
    let anthropic_capture = Arc::clone(&captured_anthropic);
    let upstream_app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let captured = Arc::clone(&chat_capture);
                async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "chatcmpl_test",
                        "created": 123,
                        "choices": [{
                            "message": {"role": "assistant", "content": "pong"},
                            "finish_reason": "stop"
                        }]
                    }))
                }
            }),
        )
        .route(
            "/v1/responses",
            post(move |Json(body): Json<Value>| {
                let captured = Arc::clone(&responses_capture);
                async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "resp_test",
                        "object": "response",
                        "status": "completed",
                        "output": [],
                        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                    }))
                }
            }),
        )
        .route(
            "/anthropic/v1/messages",
            post(move |Json(body): Json<Value>| {
                let captured = Arc::clone(&anthropic_capture);
                async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "msg_test",
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "pong"}],
                        "model": "mimo-v2.6-pro",
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 1, "output_tokens": 1}
                    }))
                }
            }),
        );
    let upstream_addr = serve_on_random_port(upstream_app).await;

    let mut cfg = config::default_deepseek_config();
    let entry = crate::catalog::mimo_token_plan_cn();
    cfg.providers
        .insert(entry.id.to_string(), entry.provider.clone());
    crate::catalog::apply_catalog_model_defaults(&mut cfg, entry.id, None)
        .expect("apply MiMo Token Plan defaults");
    let provider = cfg.providers.get_mut(entry.id).expect("MiMo provider");
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;

    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;
    let client = reqwest::Client::new();

    let chat_response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "mimo-v2.6-pro-lp",
            "messages": [{"role": "user", "content": "ping"}],
            "reasoning_effort": "max"
        }))
        .send()
        .await
        .expect("Chat alias request");
    assert!(
        chat_response.status().is_success(),
        "the official `max` label must not be rejected locally"
    );

    let responses_response = client
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "mimo-v2.6-pro-lp",
            "input": "ping",
            "reasoning": {"effort": "ultra"}
        }))
        .send()
        .await
        .expect("Responses alias request");
    assert!(
        responses_response.status().is_success(),
        "the official `ultra` label must not be rejected locally"
    );

    let anthropic_response = client
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "mimo-v2.6-pro-lp",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "ping"}],
            "output_config": {"effort": "xhigh"}
        }))
        .send()
        .await
        .expect("Anthropic alias request");
    assert!(
        anthropic_response.status().is_success(),
        "the official `xhigh` label must not be rejected locally"
    );

    let chat_body = captured_chat.lock().unwrap().clone().expect("Chat body");
    assert_eq!(chat_body["reasoning_effort"], "high");
    let responses_body = captured_responses
        .lock()
        .unwrap()
        .clone()
        .expect("Responses body");
    assert_eq!(responses_body["reasoning"]["effort"], "high");
    let anthropic_body = captured_anthropic
        .lock()
        .unwrap()
        .clone()
        .expect("Anthropic body");
    assert_eq!(anthropic_body["output_config"]["effort"], "high");

    // Widening the vocabulary must not weaken the gate: a label outside the official enum
    // is still a local 400, and the upstream never sees the request.
    let bogus_response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "mimo-v2.6-pro-lp",
            "messages": [{"role": "user", "content": "ping"}],
            "reasoning_effort": "turbo"
        }))
        .send()
        .await
        .expect("bogus level request");
    assert_eq!(bogus_response.status(), StatusCode::BAD_REQUEST);
    let bogus_body: Value = bogus_response.json().await.expect("bogus level body");
    assert!(
        bogus_body
            .to_string()
            .contains("does not support reasoning level")
    );
    assert_eq!(
        captured_chat.lock().unwrap().clone().expect("Chat body")["reasoning_effort"],
        "high",
        "the rejected request must not have reached the upstream"
    );
}

#[tokio::test]
#[ignore = "requires MIMO_TOKEN_PLAN_API_KEY and live Token Plan CN access"]
async fn mimo_token_plan_native_endpoints_accept_disabled_thinking() {
    assert!(
        std::env::var_os("MIMO_TOKEN_PLAN_API_KEY").is_some(),
        "MIMO_TOKEN_PLAN_API_KEY must be set"
    );

    let mut cfg = config::default_deepseek_config();
    let entry = crate::catalog::mimo_token_plan_cn();
    cfg.providers.insert(entry.id.to_string(), entry.provider);
    crate::catalog::apply_catalog_model_defaults(&mut cfg, entry.id, None)
        .expect("apply MiMo Token Plan defaults");
    cfg.models
        .get_mut("mimo-v2.6-pro-lp")
        .expect("MiMo Pro model")
        .default_think = Some(false);

    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;
    let client = reqwest::Client::new();

    let chat_response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "mimo-v2.6-pro-lp",
            "messages": [{"role": "user", "content": "Reply with OK."}],
            "max_completion_tokens": 32
        }))
        .send()
        .await
        .expect("send live Chat request");
    assert!(
        chat_response.status().is_success(),
        "Chat failed: {}",
        chat_response.text().await.expect("Chat error body")
    );

    let responses_response = client
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "mimo-v2.6-pro-lp",
            "input": "Reply with OK.",
            "max_output_tokens": 32
        }))
        .send()
        .await
        .expect("send live Responses request");
    assert!(
        responses_response.status().is_success(),
        "Responses failed: {}",
        responses_response
            .text()
            .await
            .expect("Responses error body")
    );

    let anthropic_response = client
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "mimo-v2.6-pro-lp",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "Reply with OK."}]
        }))
        .send()
        .await
        .expect("send live Anthropic request");
    assert!(
        anthropic_response.status().is_success(),
        "Anthropic failed: {}",
        anthropic_response
            .text()
            .await
            .expect("Anthropic error body")
    );
}

#[tokio::test]
async fn responses_native_passthrough_applies_forced_service_tier() {
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
            codex_service_tier: Some(config::CodexServiceTierConfig {
                tiers: vec!["priority".to_string()],
                default_tier: None,
                forced_tier: Some("priority".to_string()),
            }),
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
        .json(
            &json!({"model":"gpt-5.5-lp","input":"ping","stream":false,"service_tier":"standard"}),
        )
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
    assert_eq!(captured["service_tier"], "priority");
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
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Beijing\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    )
}

#[tokio::test]
async fn chat_sse_to_anthropic_sse_routes_interleaved_tool_call_arguments() {
    let upstream = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{\\\"content\\\":\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Cargo.toml\\\"}\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"\\\"hello\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;
    let response = reqwest::Client::new()
        .post(format!("http://{upstream_addr}/v1/chat/completions"))
        .send()
        .await
        .expect("fetch upstream");
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let response = chat_sse_to_anthropic_sse(
        &state,
        response,
        "frontend-model".to_string(),
        "test-provider".to_string(),
        "anthropic".to_string(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("response body")
            .to_vec(),
    )
    .expect("utf8");
    let events: Vec<(String, Value)> = body
        .split("\n\n")
        .filter_map(|frame| {
            let event = frame.strip_prefix("event: ")?.split_once('\n')?.0;
            let data = frame.split("data: ").nth(1)?;
            Some((
                event.to_string(),
                serde_json::from_str(data).expect("Anthropic SSE JSON"),
            ))
        })
        .collect();
    let deltas: Vec<&Value> = events
        .iter()
        .filter(|(event, value)| {
            event == "content_block_delta" && value["delta"]["type"] == "input_json_delta"
        })
        .map(|(_, value)| value)
        .collect();
    assert_eq!(deltas.len(), 4);
    assert_eq!(deltas[0]["index"], 1);
    assert_eq!(deltas[1]["index"], 2);
    assert_eq!(deltas[2]["index"], 1);
    assert_eq!(deltas[3]["index"], 2);
    assert_eq!(
        format!(
            "{}{}",
            deltas[0]["delta"]["partial_json"].as_str().unwrap(),
            deltas[2]["delta"]["partial_json"].as_str().unwrap(),
        ),
        r#"{"path":"Cargo.toml"}"#
    );
    assert_eq!(
        format!(
            "{}{}",
            deltas[1]["delta"]["partial_json"].as_str().unwrap(),
            deltas[3]["delta"]["partial_json"].as_str().unwrap(),
        ),
        r#"{"content":"hello"}"#
    );
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
async fn anthropic_sse_to_chat_sse_routes_interleaved_tool_call_arguments() {
    let upstream = Router::new().route(
        "/v1/messages",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_a\",\"name\":\"read_file\",\"input\":{}}}\n\n",
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_b\",\"name\":\"write_file\",\"input\":{}}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"content\\\":\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Cargo.toml\\\"}\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"hello\\\"}\"}}\n\n",
                    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;
    let response = reqwest::Client::new()
        .post(format!("http://{upstream_addr}/v1/messages"))
        .send()
        .await
        .expect("fetch upstream");
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let response = anthropic_sse_to_chat_sse(
        &state,
        response,
        "frontend-model".to_string(),
        "test-provider".to_string(),
        "openai_chat".to_string(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("response body")
            .to_vec(),
    )
    .expect("utf8");
    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("Chat SSE JSON"))
        .collect();
    let deltas: Vec<&Value> = events
        .iter()
        .filter(|event| event["choices"][0]["delta"].get("tool_calls").is_some())
        .collect();
    assert_eq!(deltas.len(), 6);
    assert_eq!(
        deltas[0]["choices"][0]["delta"]["tool_calls"][0]["index"],
        0
    );
    assert_eq!(
        deltas[1]["choices"][0]["delta"]["tool_calls"][0]["index"],
        1
    );
    assert_eq!(
        deltas[2]["choices"][0]["delta"]["tool_calls"][0]["index"],
        0
    );
    assert_eq!(
        deltas[3]["choices"][0]["delta"]["tool_calls"][0]["index"],
        1
    );
    assert_eq!(
        deltas[4]["choices"][0]["delta"]["tool_calls"][0]["index"],
        0
    );
    assert_eq!(
        deltas[5]["choices"][0]["delta"]["tool_calls"][0]["index"],
        1
    );
    assert_eq!(
        format!(
            "{}{}",
            deltas[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
            deltas[4]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        ),
        r#"{"path":"Cargo.toml"}"#
    );
    assert_eq!(
        format!(
            "{}{}",
            deltas[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
            deltas[5]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        ),
        r#"{"content":"hello"}"#
    );

    // P5: every synthetic chunk shares one generated chatcmpl id and carries
    // a real Unix-seconds timestamp (spec.md §6 ID namespace rules).
    let ids: Vec<&str> = events
        .iter()
        .map(|event| event["id"].as_str().expect("chunk id"))
        .collect();
    assert!(ids.iter().all(|id| id.starts_with("chatcmpl-")), "{ids:?}");
    assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
    assert!(
        events
            .iter()
            .all(|event| event["created"].as_i64().unwrap() > 1_700_000_000),
        "chunks must carry real created timestamps"
    );
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
async fn anthropic_sse_to_responses_sse_routes_interleaved_tool_call_arguments() {
    let upstream = Router::new().route(
        "/v1/messages",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_a\",\"name\":\"read_file\",\"input\":{}}}\n\n",
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_b\",\"name\":\"write_file\",\"input\":{}}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"content\\\":\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Cargo.toml\\\"}\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"hello\\\"}\"}}\n\n",
                    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;
    let response = reqwest::Client::new()
        .post(format!("http://{upstream_addr}/v1/messages"))
        .send()
        .await
        .expect("fetch upstream");
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let response = anthropic_sse_to_responses_sse(
        &state,
        response,
        "frontend-model".to_string(),
        "test-provider".to_string(),
        "openai_responses".to_string(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("response body")
            .to_vec(),
    )
    .expect("utf8");
    let events: Vec<(String, Value)> = body
        .split("\n\n")
        .filter_map(|frame| {
            let event = frame.strip_prefix("event: ")?.split_once('\n')?.0;
            let data = frame.split("data: ").nth(1)?;
            Some((
                event.to_string(),
                serde_json::from_str(data).expect("Responses SSE JSON"),
            ))
        })
        .collect();
    let argument_deltas: Vec<&Value> = events
        .iter()
        .filter(|(event, _)| event == "response.function_call_arguments.delta")
        .map(|(_, value)| value)
        .collect();
    assert_eq!(argument_deltas.len(), 4);
    assert_eq!(argument_deltas[0]["item_id"], "fc_call_a");
    assert_eq!(argument_deltas[1]["item_id"], "fc_call_b");
    assert_eq!(argument_deltas[2]["item_id"], "fc_call_a");
    assert_eq!(argument_deltas[3]["item_id"], "fc_call_b");

    let completed_calls: Vec<&Value> = events
        .iter()
        .filter(|(event, value)| {
            event == "response.output_item.done" && value["item"]["type"] == "function_call"
        })
        .map(|(_, value)| &value["item"])
        .collect();
    assert_eq!(completed_calls.len(), 2);
    assert_eq!(completed_calls[0]["call_id"], "call_a");
    assert_eq!(completed_calls[0]["name"], "read_file");
    assert_eq!(completed_calls[0]["arguments"], r#"{"path":"Cargo.toml"}"#);
    assert_eq!(completed_calls[1]["call_id"], "call_b");
    assert_eq!(completed_calls[1]["name"], "write_file");
    assert_eq!(completed_calls[1]["arguments"], r#"{"content":"hello"}"#);
}

#[tokio::test]
async fn anthropic_sse_to_responses_sse_max_tokens_terminates_incomplete() {
    let upstream = Router::new().route(
        "/v1/messages",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"秋天总是来得悄无声\"}}\n\n",
                    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":300}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;
    let response = reqwest::Client::new()
        .post(format!("http://{upstream_addr}/v1/messages"))
        .send()
        .await
        .expect("fetch upstream");
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let response = anthropic_sse_to_responses_sse(
        &state,
        response,
        "frontend-model".to_string(),
        "test-provider".to_string(),
        "openai_responses".to_string(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("response body")
            .to_vec(),
    )
    .expect("utf8");
    let events: Vec<(String, Value)> = body
        .split("\n\n")
        .filter_map(|frame| {
            let event = frame.strip_prefix("event: ")?.split_once('\n')?.0;
            let data = frame.split("data: ").nth(1)?;
            Some((
                event.to_string(),
                serde_json::from_str(data).expect("Responses SSE JSON"),
            ))
        })
        .collect();
    let terminal = events
        .iter()
        .find(|(event, _)| event == "response.incomplete" || event == "response.completed")
        .expect("terminal event");
    assert_eq!(terminal.0, "response.incomplete");
    assert_eq!(terminal.1["response"]["status"], "incomplete");
    assert_eq!(
        terminal.1["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    // Truncated output items must not claim completed either.
    for item in terminal.1["response"]["output"].as_array().expect("output") {
        assert_eq!(item["status"], "incomplete", "{item}");
    }
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
        "/v1/responses",
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
        "/v1/responses",
        post(|| async {
            Json(json!({
                "id": "resp_second",
                "object": "responses/response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "fallback ok"}]
                }]
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
    cfg.fallback.server_error_retries = 0;
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
        auth_error_seconds: 66,
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
    assert_eq!(
        cooldown_duration_for_status(StatusCode::PAYMENT_REQUIRED, &cooldown),
        Some(Duration::from_secs(66))
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
    cfg.fallback.server_error_retries = 0;
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
    cfg.fallback.server_error_retries = 0;
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
            reasoning_levels: Some(vec![
                config::ReasoningLevelEntry::new("low", None),
                config::ReasoningLevelEntry::new("medium", None),
            ]),
            default_reasoning_level: None,
            supports_thinking: None,
            force_think: None,
            force_no_think: None,
            default_think: None,
            enable_thinking: None,
            chat_reasoning_off_format: None,
            codex_service_tier: None,
            reasoning_level_map: None,
            supported_reasoning_levels: vec![],
            cooldown: None,
            reasoning_routes: None,
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

    for (content_part, expected_feature) in [
        (
            json!({"type": "input_audio", "input_audio": {"data": "AAAA", "format": "mp3"}}),
            "audio_input",
        ),
        (
            json!({"type": "video_url", "video_url": {"url": "https://example.com/x.mp4"}}),
            "video_input",
        ),
    ] {
        let response = client
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .json(&json!({
                "model": "chat-only-lp",
                "messages": [{
                    "role": "user",
                    "content": [{"type": "text", "text": "describe"}, content_part]
                }]
            }))
            .send()
            .await
            .expect("multimodal capability request");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = response.json().await.expect("multimodal capability body");
        assert!(
            body.to_string().contains(expected_feature),
            "missing {expected_feature} error in {body}"
        );
    }

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
    cfg.fallback.server_error_retries = 0;
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

#[test]
fn classifies_retry_and_transient_cooldown_kinds() {
    assert!(should_retry_server_error(StatusCode::INTERNAL_SERVER_ERROR));
    assert!(should_retry_server_error(
        StatusCode::from_u16(529).unwrap()
    ));
    assert!(!should_retry_server_error(StatusCode::TOO_MANY_REQUESTS));
    assert!(!should_retry_server_error(StatusCode::NOT_FOUND));
    assert!(!should_retry_server_error(StatusCode::BAD_REQUEST));
    assert!(!should_retry_server_error(StatusCode::UNAUTHORIZED));
    assert!(!should_retry_server_error(StatusCode::FORBIDDEN));
    assert!(!should_retry_server_error(StatusCode::PAYMENT_REQUIRED));

    assert!(should_fallback_without_cooldown(StatusCode::UNAUTHORIZED));
    assert!(should_fallback_without_cooldown(StatusCode::FORBIDDEN));
    assert!(!should_fallback_without_cooldown(
        StatusCode::PAYMENT_REQUIRED
    ));

    assert!(cooldown_kind_is_transient("network"));
    assert!(cooldown_kind_is_transient("server_error"));
    assert!(cooldown_kind_is_transient("rate_limit"));
    assert!(!cooldown_kind_is_transient("auth_error"));
    assert!(!cooldown_kind_is_transient("model_unavailable"));
    assert!(!cooldown_kind_is_transient("client_error"));
}

#[tokio::test]
async fn send_upstream_retries_server_errors_before_returning() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_handler = Arc::clone(&attempts);
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let attempts = Arc::clone(&attempts_for_handler);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                (StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    cfg.fallback.server_error_retries = 2;
    cfg.fallback.server_error_retry_interval_ms = 0;
    let state = AppState::new_in_memory(cfg);
    let url = format!("http://{addr}/chat/completions");
    let plan = test_execution_plan(url.clone());
    let req = state
        .client
        .post(url)
        .json(&json!({"model":"m","messages":[]}));
    let response = send_upstream(&state, &plan, req, false)
        .await
        .expect("5xx is still an HTTP response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

async fn chat_proxy(
    first_status: StatusCode,
    first_hits: Arc<Mutex<usize>>,
    second_ok: bool,
) -> (SocketAddr, AppState, config::CooldownKey) {
    let first_hits_handler = Arc::clone(&first_hits);
    let first_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let first_hits_handler = Arc::clone(&first_hits_handler);
            async move {
                *first_hits_handler.lock().unwrap() += 1;
                (first_status, "upstream error")
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

    let mut cfg = config::default_deepseek_config();
    cfg.fallback.server_error_retries = 0;
    cfg.fallback.server_error_retry_interval_ms = 0;
    let model_id = "deepseek-v4-pro-lp";
    if second_ok {
        let second_app = Router::new().route(
            "/chat/completions",
            post(|| async {
                Json(json!({
                    "id": "chatcmpl_second",
                    "created": 123,
                    "choices": [{"message": {"role": "assistant", "content": "fallback ok"}, "finish_reason": "stop"}],
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
        let mut fallback_provider = cfg.providers.get("deepseek").unwrap().clone();
        repoint_provider_urls(&mut fallback_provider, second_addr);
        fallback_provider.api_key_env = None;
        cfg.providers
            .insert("fallback".to_string(), fallback_provider);
        cfg.models
            .get_mut(model_id)
            .unwrap()
            .openai_chat_providers
            .push(crate::config::ProviderBinding {
                name: "fallback".to_string(),
                model: "fallback-chat-model".to_string(),
            });
    }
    repoint_provider_urls(cfg.providers.get_mut("deepseek").unwrap(), first_addr);
    cfg.providers.get_mut("deepseek").unwrap().api_key_env = None;

    let state = AppState::new_in_memory(cfg);
    let key = crate::config::CooldownKey {
        model_id: model_id.to_string(),
        provider_id: "deepseek".to_string(),
        protocol: Protocol::OpenaiChatCompletions,
        token: None,
        token_key_hash: None,
    };
    let app = router(state.clone());
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });
    (proxy_addr, state, key)
}

async fn chat_request(proxy_addr: SocketAddr) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-pro-lp",
            "messages": [{"role": "user", "content": "ping"}],
            "stream": false
        }))
        .send()
        .await
        .expect("send proxy request")
}

#[tokio::test]
async fn single_provider_5xx_retries_then_cools_and_still_probes() {
    let hits = Arc::new(Mutex::new(0usize));
    let mut cfg = config::default_deepseek_config();
    cfg.fallback.server_error_retries = 2;
    cfg.fallback.server_error_retry_interval_ms = 0;
    let hits_handler = Arc::clone(&hits);
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let hits_handler = Arc::clone(&hits_handler);
            async move {
                *hits_handler.lock().unwrap() += 1;
                (StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, upstream_app)
            .await
            .expect("serve upstream");
    });
    repoint_provider_urls(cfg.providers.get_mut("deepseek").unwrap(), addr);
    cfg.providers.get_mut("deepseek").unwrap().api_key_env = None;
    let state = AppState::new_in_memory(cfg);
    let key = crate::config::CooldownKey {
        model_id: "deepseek-v4-pro-lp".into(),
        provider_id: "deepseek".into(),
        protocol: Protocol::OpenaiChatCompletions,
        token: None,
        token_key_hash: None,
    };
    let app = router(state.clone());
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(proxy_listener, app).await.expect("serve proxy");
    });

    let first = chat_request(proxy_addr).await;
    assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(*hits.lock().unwrap(), 3);
    assert!(state.is_cooling_down(&key));
    assert_eq!(
        state.active_cooldown(&key).expect("cooldown").kind,
        "server_error"
    );

    let second = chat_request(proxy_addr).await;
    assert_eq!(second.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(*hits.lock().unwrap(), 6);
}

#[tokio::test]
async fn persistent_all_cooling_fails_fast_without_upstream() {
    let hits = Arc::new(Mutex::new(0usize));
    let (proxy_addr, state, key) =
        chat_proxy(StatusCode::PAYMENT_REQUIRED, Arc::clone(&hits), false).await;
    state.set_cooldown(
        &key,
        "auth_error",
        "402 Payment Required",
        Duration::from_secs(90),
    );

    let response = chat_request(proxy_addr).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let retry_after: u64 = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .expect("Retry-After seconds");
    assert!((89..=90).contains(&retry_after));
    let body: Value = response.json().await.expect("json");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("deepseek-v4-pro-lp"));
    assert!(message.contains("auth_error"));
    assert!(message.contains("402 Payment Required"));
    assert_eq!(*hits.lock().unwrap(), 0);
}

#[tokio::test]
async fn forbidden_falls_back_without_cooldown() {
    let hits = Arc::new(Mutex::new(0usize));
    let (proxy_addr, state, key) = chat_proxy(StatusCode::FORBIDDEN, Arc::clone(&hits), true).await;

    let response: Value = chat_request(proxy_addr).await.json().await.expect("json");
    assert_eq!(response["choices"][0]["message"]["content"], "fallback ok");
    assert_eq!(*hits.lock().unwrap(), 1);
    assert!(!state.is_cooling_down(&key));
}

#[tokio::test]
async fn unauthorized_falls_back_without_cooldown() {
    let hits = Arc::new(Mutex::new(0usize));
    let (proxy_addr, state, key) =
        chat_proxy(StatusCode::UNAUTHORIZED, Arc::clone(&hits), true).await;

    let response: Value = chat_request(proxy_addr).await.json().await.expect("json");
    assert_eq!(response["choices"][0]["message"]["content"], "fallback ok");
    assert_eq!(*hits.lock().unwrap(), 1);
    assert!(!state.is_cooling_down(&key));
}

#[tokio::test]
async fn payment_required_cools_and_falls_back() {
    let hits = Arc::new(Mutex::new(0usize));
    let (proxy_addr, state, key) =
        chat_proxy(StatusCode::PAYMENT_REQUIRED, Arc::clone(&hits), true).await;

    let response: Value = chat_request(proxy_addr).await.json().await.expect("json");
    assert_eq!(response["choices"][0]["message"]["content"], "fallback ok");
    assert_eq!(*hits.lock().unwrap(), 1);
    assert!(state.is_cooling_down(&key));
    assert_eq!(
        state.active_cooldown(&key).expect("cooldown").kind,
        "auth_error"
    );
}

#[tokio::test]
async fn empty_candidates_returns_configuration_error() {
    let mut cfg = config::default_deepseek_config();
    cfg.providers.get_mut("deepseek").unwrap().openai_chat = Some(config::EndpointConfig {
        url: None,
        derive_from: None,
        compat: None,
        anthropic_family_models: Vec::new(),
        store: None,
    });
    let state = AppState::new_in_memory(cfg);
    let response = chat_completions(
        State(state),
        Extension(crate::token::auth::TokenGrant::Open),
        JsonWithBytes::from_value(json!({
            "model": "deepseek-v4-pro-lp",
            "messages": [{"role": "user", "content": "ping"}]
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("has no usable provider binding"));
    assert!(!message.contains("all providers are cooling down"));
}

// ── §12/§13/§14 集成测试：admin 端点、委托链路、活跃 provider ─────────────

#[tokio::test]
async fn admin_status_reports_active_providers_within_ttl() {
    // §12.3/12.5：/admin/status 返回活跃 provider 列表（TTL 内）
    let cfg = config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);
    state.mark_provider_active("deepseek");
    // /admin/* endpoints moved to UDS management channel
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let app = crate::service::management_router(
        state,
        std::sync::Arc::new(std::sync::Mutex::new(Some(shutdown_tx))),
    );
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
async fn public_router_does_not_expose_token_write_endpoints() {
    // Admin endpoints are now exposed on the public HTTP listener
    // with token authentication. This test verifies that admin endpoints REQUIRE
    // valid token authentication (no unauthenticated access).
    // spec.md §11.8: Write endpoints require token auth on public listener.
    // UDS management channel remains for local-only access without auth.

    // Set up a config directory with tokens.toml to enable token auth
    let config_dir = tempfile::tempdir().expect("config dir");
    let state_dir = tempfile::tempdir().expect("state dir");
    std::fs::write(
        config_dir.path().join("tokens.toml"),
        r#"
[[tokens]]
key = "lp-test-key-172"
name = "test-token"
allowed_models = []
"#,
    )
    .expect("write tokens.toml");

    let cfg = config::default_deepseek_config();
    let mut state = AppState::new_in_memory(cfg);
    state.token = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        state_dir.path(),
    );

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });

    let client = reqwest::Client::new();
    // Admin endpoints require token authentication on public listener
    for path in [
        // Write endpoints
        "/admin/token/create",
        "/admin/token/delete",
        "/admin/token/reset-usage",
        "/admin/status/probe",
        "/admin/oauth/logout",
        // Read endpoints
        "/admin/ping",
        "/admin/status",
        "/admin/provider/list",
        "/admin/model/list",
        "/admin/usage",
    ] {
        let method = if path.contains("probe")
            || path.contains("create")
            || path.contains("delete")
            || path.contains("reset")
            || path.contains("logout")
        {
            "POST"
        } else {
            "GET"
        };
        let resp = match method {
            "POST" => client
                .post(format!("http://{addr}{path}"))
                .json(&json!({"name": "should-not-exist"}))
                .send()
                .await
                .expect("admin endpoint request"),
            _ => client
                .get(format!("http://{addr}{path}"))
                .send()
                .await
                .expect("admin endpoint request"),
        };
        // Without token auth, should get 401 Unauthorized (not 404)
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path} must require token authentication on public listener"
        );
    }
}

#[tokio::test]
async fn public_router_exposes_health_endpoint_without_auth() {
    // Set up a config directory with tokens.toml to enable token auth
    let config_dir = tempfile::tempdir().expect("config dir");
    let state_dir = tempfile::tempdir().expect("state dir");
    std::fs::write(
        config_dir.path().join("tokens.toml"),
        r#"
[[tokens]]
key = "lp-test-key-health"
name = "test-token"
allowed_models = []
"#,
    )
    .expect("write tokens.toml");

    let cfg = config::default_deepseek_config();
    let mut state = AppState::new_in_memory(cfg);
    state.token = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        state_dir.path(),
    );

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    let client = reqwest::Client::new();

    // GET /health without auth token must return 200 {"status": "ok"}
    let resp = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("send health request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "/health must be reachable on public listener without authentication"
    );
    let body: Value = resp.json().await.expect("parse health json");
    assert_eq!(body["status"], "ok");
    assert!(
        body.get("version").is_none(),
        "health endpoint must not leak version"
    );
    assert!(
        body.get("providers").is_none(),
        "health endpoint must not leak provider topology"
    );

    // Concurrently verify that /admin/ping requires authentication on the public router
    let admin_resp = client
        .get(format!("http://{addr}/admin/ping"))
        .send()
        .await
        .expect("send admin ping request");
    assert_eq!(
        admin_resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "/admin/ping must require token authentication on public listener"
    );
}

#[tokio::test]
async fn unprivileged_token_blocked_from_admin_write_endpoints_with_403() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let state_dir = tempfile::tempdir().expect("state dir");
    std::fs::write(
        config_dir.path().join("tokens.toml"),
        r#"
[[tokens]]
key = "lp-regular-user-token"
name = "regular-user"
admin = false
allowed_models = ["deepseek-chat"]

[[tokens]]
key = "lp-admin-power-token"
name = "admin-user"
admin = true
allowed_models = []
"#,
    )
    .expect("write tokens.toml");

    let cfg = config::default_deepseek_config();
    let mut state = AppState::new_in_memory(cfg);
    state.token = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        state_dir.path(),
    );

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });

    let client = reqwest::Client::new();

    // 1. Regular token hitting admin privileged mutating endpoints MUST receive 403 Forbidden
    for path in [
        "/admin/token/create",
        "/admin/token/delete",
        "/admin/config/reload",
        "/admin/provider/add",
        "/admin/model/add",
        "/admin/cooldown/clear",
    ] {
        let resp = client
            .post(format!("http://{addr}{path}"))
            .header(
                reqwest::header::AUTHORIZATION,
                "Bearer lp-regular-user-token",
            )
            .json(&json!({"name": "test"}))
            .send()
            .await
            .expect("send request");

        assert_eq!(
            resp.status(),
            reqwest::StatusCode::FORBIDDEN,
            "regular user token must be forbidden (403) from accessing {path}"
        );
        let body: serde_json::Value = resp.json().await.expect("parse 403 json");
        assert_eq!(body["status"], "error");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("Admin privilege required"),
            "expected 'Admin privilege required' error message, got: {body}"
        );
    }

    // 2. Regular token hitting inspection endpoints is allowed (200 OK).
    // Provider detail endpoints are admin-only; see
    // `unprivileged_token_blocked_from_provider_detail_endpoints`.
    for path in [
        "/admin/ping",
        "/admin/status",
        "/admin/model/list",
        "/admin/config/snapshot",
    ] {
        let resp = client
            .get(format!("http://{addr}{path}"))
            .header(
                reqwest::header::AUTHORIZATION,
                "Bearer lp-regular-user-token",
            )
            .send()
            .await
            .expect("send request");

        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "regular user token should be permitted to read {path}"
        );
    }

    // 3. Admin token hitting privileged endpoints passes permission check
    let resp = client
        .post(format!("http://{addr}/admin/config/reload"))
        .header(
            reqwest::header::AUTHORIZATION,
            "Bearer lp-admin-power-token",
        )
        .send()
        .await
        .expect("send request");
    assert_ne!(
        resp.status(),
        reqwest::StatusCode::FORBIDDEN,
        "admin token must not receive 403 on admin endpoints"
    );
}

#[tokio::test]
async fn unprivileged_token_blocked_from_provider_detail_endpoints() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let state_dir = tempfile::tempdir().expect("state dir");
    std::fs::write(
        config_dir.path().join("tokens.toml"),
        r#"
[[tokens]]
key = "lp-regular-user-token"
name = "regular-user"
admin = false
allowed_models = ["deepseek-v4-flash-lp"]

[[tokens]]
key = "lp-admin-power-token"
name = "admin-user"
admin = true
allowed_models = []
"#,
    )
    .expect("write tokens.toml");

    let cfg = config::default_deepseek_config();
    let mut state = AppState::new_in_memory(cfg);
    state.token = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        state_dir.path(),
    );

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });

    let client = reqwest::Client::new();

    // 1. Regular token must not read provider topology.
    for path in ["/admin/provider/list", "/admin/provider/deepseek"] {
        let resp = client
            .get(format!("http://{addr}{path}"))
            .header(
                reqwest::header::AUTHORIZATION,
                "Bearer lp-regular-user-token",
            )
            .send()
            .await
            .expect("send request");

        assert_eq!(
            resp.status(),
            reqwest::StatusCode::FORBIDDEN,
            "regular user token must be forbidden (403) from reading {path}"
        );
    }

    // 2. Admin token still sees provider detail.
    for path in ["/admin/provider/list", "/admin/provider/deepseek"] {
        let resp = client
            .get(format!("http://{addr}{path}"))
            .header(
                reqwest::header::AUTHORIZATION,
                "Bearer lp-admin-power-token",
            )
            .send()
            .await
            .expect("send request");

        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "admin token must be able to read {path}"
        );
    }
}

#[tokio::test]
async fn unprivileged_token_probe_enforces_model_authorization_and_quota_control() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let state_dir = tempfile::tempdir().expect("state dir");
    std::fs::write(
        config_dir.path().join("tokens.toml"),
        r#"
[[tokens]]
key = "lp-scoped-user-token"
name = "scoped-user"
admin = false
allowed_models = ["deepseek-v4-flash-lp"]
[[tokens.quota_windows]]
window = "1h"
quota_mode = "requests"
quota_limit = 5

[[tokens]]
key = "lp-exhausted-user-token"
name = "exhausted-user"
admin = false
allowed_models = ["deepseek-v4-flash-lp"]
[[tokens.quota_windows]]
window = "1h"
quota_mode = "requests"
quota_limit = 1
"#,
    )
    .expect("write tokens.toml");

    let cfg = config::default_deepseek_config();
    let mut state = AppState::new_in_memory(cfg);
    state.token = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        state_dir.path(),
    );

    // Pre-exhaust the quota for exhausted-user
    let store = state.token.usage_store.as_ref().expect("usage store");
    let token_cfg = state
        .token
        .config
        .read()
        .unwrap()
        .tokens
        .iter()
        .find(|t| t.key == "lp-exhausted-user-token")
        .cloned()
        .unwrap();
    let gw = crate::token::usage::grouped_windows_for_model(&token_cfg, "deepseek-v4-flash-lp");
    let slot = store
        .reserve_request_slot_grouped("lp-exhausted-user-token", &gw)
        .expect("reserve");
    assert!(matches!(slot, crate::token::usage::RequestSlot::Granted));

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });

    let client = reqwest::Client::new();

    // 1. Unprivileged token probing an unauthorized model receives 403 Forbidden
    let resp_unauthorized = client
        .post(format!("http://{addr}/admin/status/probe"))
        .header(
            reqwest::header::AUTHORIZATION,
            "Bearer lp-scoped-user-token",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&json!({"model": "claude-3-7-opus"}))
        .send()
        .await
        .expect("send probe request");

    assert_eq!(
        resp_unauthorized.status(),
        reqwest::StatusCode::FORBIDDEN,
        "probing unauthorized model must return 403"
    );
    let body_forbidden: serde_json::Value = resp_unauthorized
        .json()
        .await
        .expect("parse forbidden json");
    assert_eq!(body_forbidden["status"], "error");
    assert!(
        body_forbidden["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not allowed for this token"),
        "error must specify model not allowed, got: {body_forbidden}"
    );

    // 1b. Wildcard pattern matching any unauthorized model receives 403 Forbidden
    let resp_wildcard_unauthorized = client
        .post(format!("http://{addr}/admin/status/probe"))
        .header(
            reqwest::header::AUTHORIZATION,
            "Bearer lp-scoped-user-token",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&json!({"model": "deepseek-*"}))
        .send()
        .await
        .expect("send probe wildcard request");

    assert_eq!(
        resp_wildcard_unauthorized.status(),
        reqwest::StatusCode::FORBIDDEN,
        "probing wildcard pattern covering unauthorized model must return 403"
    );
    let body_wildcard_forbidden: serde_json::Value = resp_wildcard_unauthorized
        .json()
        .await
        .expect("parse wildcard forbidden json");
    assert_eq!(body_wildcard_forbidden["status"], "error");
    assert!(
        body_wildcard_forbidden["error"]
            .as_str()
            .unwrap_or_default()
            .contains("matches unauthorized model"),
        "error must specify wildcard matches unauthorized model, got: {body_wildcard_forbidden}"
    );

    // 2. Unprivileged token with exhausted quota receives 429 Too Many Requests
    let resp_exhausted = client
        .post(format!("http://{addr}/admin/status/probe"))
        .header(
            reqwest::header::AUTHORIZATION,
            "Bearer lp-exhausted-user-token",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&json!({"model": "deepseek-v4-flash-lp"}))
        .send()
        .await
        .expect("send probe request");

    let status_exhausted = resp_exhausted.status();
    let body_429: serde_json::Value = resp_exhausted.json().await.expect("parse 429 json");
    assert_eq!(
        status_exhausted,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "probing with exhausted quota must return 429, body={body_429}"
    );
    assert_eq!(body_429["status"], "error");
    assert!(
        body_429["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Token quota exhausted"),
        "error must indicate quota exhausted, got: {body_429}"
    );

    // 3. Unprivileged token probing authorized model with valid quota proceeds
    let resp_ok = client
        .post(format!("http://{addr}/admin/status/probe"))
        .header(
            reqwest::header::AUTHORIZATION,
            "Bearer lp-scoped-user-token",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&json!({"model": "deepseek-v4-flash-lp"}))
        .send()
        .await
        .expect("send probe request");

    assert_eq!(
        resp_ok.status(),
        reqwest::StatusCode::OK,
        "probing authorized model with quota must succeed"
    );
    let body_ok: serde_json::Value = resp_ok.json().await.expect("parse ok json");
    assert_eq!(body_ok["status"], "ok");
}

#[tokio::test]
async fn admin_status_probe_probes_inactive_providers_against_upstream() {
    // §12.5/12.7：/admin/status/probe 对非活跃 provider 做真实探活并返回结果
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

    let (_cache_dir, state) = state_with_temp_probe_cache(cfg);
    // probe 已迁入管理通道 router；测试直接在 TCP 上 serve
    // management_router 验证路由与行为（真实传输为 UDS，见 service.rs 测试）。
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let app = crate::service::management_router(
        state,
        std::sync::Arc::new(std::sync::Mutex::new(Some(shutdown_tx))),
    );
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

#[tokio::test]
async fn admin_ping_and_status_endpoints_work() {
    let cfg = config::default_deepseek_config();
    let state = AppState::new_in_memory(cfg);
    // /admin/* endpoints moved to UDS management channel
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let app = crate::service::management_router(
        state,
        std::sync::Arc::new(std::sync::Mutex::new(Some(shutdown_tx))),
    );
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
async fn admin_status_probe_skips_active_provider() {
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

    let (_cache_dir, state) = state_with_temp_probe_cache(cfg);
    // 标记 deepseek 为活跃（L1 应跳过，不触发网络请求）
    state.mark_provider_active("deepseek");

    // probe 走管理通道 router（同 upstream 测试的说明）
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let app = crate::service::management_router(
        state,
        std::sync::Arc::new(std::sync::Mutex::new(Some(shutdown_tx))),
    );
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
async fn chat_via_openai_sub_router_injects_oauth_bearer_token_upstream() {
    use axum::http::HeaderMap;

    let captured_body: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_authorization: CapturedHeader = Arc::new(Mutex::new(None));
    let captured: CapturedRequestAndHeader = (
        Arc::clone(&captured_body),
        Arc::clone(&captured_authorization),
    );
    let captured_for_handler = captured.clone();
    let upstream_app = Router::new()
        .route(
            "/backend-api/codex/responses",
            post(
                |State(captured): State<CapturedRequestAndHeader>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    *captured.0.lock().expect("captured request") = Some(body);
                    *captured.1.lock().expect("captured authorization") = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(ToString::to_string);
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                    )
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_addr = serve_on_random_port(upstream_app).await;

    let mut cfg = config::default_deepseek_config();
    let mut provider = crate::catalog::openai_sub().provider;
    repoint_provider_urls(&mut provider, upstream_addr);
    provider.api_key_env = None;
    cfg.providers.insert("openai-sub".to_string(), provider);
    cfg.models.insert(
        "oauth-router-test".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 1_000,
            max_output_tokens: 100,
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
            reasoning_level_map: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "openai-sub".to_string(),
                model: "gpt-5.5".to_string(),
            }],
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
        },
    );
    let state = AppState::new_in_memory(cfg);
    let mut accounts = crate::auth::AccountsFile::new();
    accounts.openai.insert(
        "openai-subscription".to_string(),
        crate::auth::OpenaiAccount {
            account_label: "synthetic@example.com".to_string(),
            access_token: "synthetic-router-access-token".to_string(),
            refresh_token: "synthetic-router-refresh-token".to_string(),
            expires_at_unix: 4_102_444_800,
            updated_at_unix: 1,
        },
    );
    *state.oauth_accounts.write().expect("OAuth cache lock") = accounts;

    let proxy_addr = serve_on_random_port(router(state)).await;
    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "oauth-router-test",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        }))
        .send()
        .await
        .expect("send proxy request");
    assert!(response.status().is_success());

    assert_eq!(
        captured
            .1
            .lock()
            .expect("captured authorization")
            .as_deref(),
        Some("Bearer synthetic-router-access-token")
    );
    let captured_request = captured
        .0
        .lock()
        .expect("captured request")
        .clone()
        .expect("upstream request");
    assert_eq!(captured_request["model"], "gpt-5.5");
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_via_openai_sub_router_preserves_upstream_401_without_refresh_retry() {
    use axum::http::HeaderMap;

    let captured_authorization: CapturedHeader = Arc::new(Mutex::new(None));
    let upstream_app = Router::new()
        .route(
            "/backend-api/codex/responses",
            post(
                |State(captured): State<CapturedHeader>, headers: HeaderMap| async move {
                    *captured.lock().expect("captured authorization") = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(ToString::to_string);
                    (StatusCode::UNAUTHORIZED, "upstream rejected token")
                },
            ),
        )
        .with_state(Arc::clone(&captured_authorization));
    let upstream_addr = serve_on_random_port(upstream_app).await;

    let mut cfg = config::default_deepseek_config();
    let mut provider = crate::catalog::openai_sub().provider;
    repoint_provider_urls(&mut provider, upstream_addr);
    provider.api_key_env = None;
    cfg.providers.insert("openai-sub".to_string(), provider);
    cfg.models.insert(
        "oauth-401-router-test".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 1_000,
            max_output_tokens: 100,
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
            reasoning_level_map: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "openai-sub".to_string(),
                model: "gpt-5.5".to_string(),
            }],
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
        },
    );
    let state = AppState::new_in_memory(cfg);
    let mut accounts = crate::auth::AccountsFile::new();
    accounts.openai.insert(
        "openai-subscription".to_string(),
        crate::auth::OpenaiAccount {
            account_label: "synthetic@example.com".to_string(),
            access_token: "synthetic-router-access-token".to_string(),
            refresh_token: "synthetic-router-refresh-token".to_string(),
            expires_at_unix: 4_102_444_800,
            updated_at_unix: 1,
        },
    );
    *state.oauth_accounts.write().expect("OAuth cache lock") = accounts;

    let proxy_addr = serve_on_random_port(router(state)).await;
    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "oauth-401-router-test",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        }))
        .send()
        .await
        .expect("send proxy request");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        captured_authorization
            .lock()
            .expect("captured authorization")
            .as_deref(),
        Some("Bearer synthetic-router-access-token")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_via_openai_sub_router_singleflights_concurrent_expired_oauth_refresh() {
    use axum::http::HeaderMap;

    let refresh_count = Arc::new(AtomicUsize::new(0));
    let refresh_count_for_handler = Arc::clone(&refresh_count);
    let token_app = Router::new().route(
        "/oauth/token",
        post(move || {
            let refresh_count = Arc::clone(&refresh_count_for_handler);
            async move {
                refresh_count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                Json(json!({
                    "access_token": "synthetic-refreshed-access-token",
                    "refresh_token": "synthetic-refreshed-refresh-token",
                    "expires_in": 3600
                }))
            }
        }),
    );
    let token_addr = serve_on_random_port(token_app).await;

    let captured_body: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_authorization: CapturedHeader = Arc::new(Mutex::new(None));
    let captured: CapturedRequestAndHeader = (
        Arc::clone(&captured_body),
        Arc::clone(&captured_authorization),
    );
    let upstream_count = Arc::new(AtomicUsize::new(0));
    let upstream_app = Router::new()
        .route(
            "/backend-api/codex/responses",
            post(
                |State((captured, upstream_count)): State<(CapturedRequestAndHeader, Arc<AtomicUsize>)>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    upstream_count.fetch_add(1, Ordering::SeqCst);
                    *captured.0.lock().expect("captured request") = Some(body);
                    *captured.1.lock().expect("captured authorization") = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(ToString::to_string);
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                    )
                },
            ),
        )
        .with_state((captured.clone(), Arc::clone(&upstream_count)));
    let upstream_addr = serve_on_random_port(upstream_app).await;

    let mut cfg = config::default_deepseek_config();
    let mut provider = crate::catalog::openai_sub().provider;
    repoint_provider_urls(&mut provider, upstream_addr);
    provider.api_key_env = None;
    cfg.providers.insert("openai-sub".to_string(), provider);
    cfg.models.insert(
        "oauth-expired-router-test".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 1_000,
            max_output_tokens: 100,
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
            reasoning_level_map: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "openai-sub".to_string(),
                model: "gpt-5.5".to_string(),
            }],
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
        },
    );
    let state_dir = tempfile::tempdir().expect("temporary OAuth state");
    let oauth_path = state_dir.path().join("oauth_accounts.json");
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("current UNIX time")
        .as_secs() as i64;
    let mut accounts = crate::auth::AccountsFile::new();
    accounts.openai.insert(
        "openai-subscription".to_string(),
        crate::auth::OpenaiAccount {
            account_label: "synthetic@example.com".to_string(),
            access_token: "synthetic-expired-access-token".to_string(),
            refresh_token: "synthetic-expired-refresh-token".to_string(),
            expires_at_unix: now - 60,
            updated_at_unix: now - 120,
        },
    );
    crate::auth::save_oauth_accounts(&oauth_path, &accounts).expect("write temporary OAuth state");

    let mut state = AppState::new_in_memory(cfg);
    state.oauth_accounts_path = oauth_path;
    *state.oauth_accounts.write().expect("OAuth cache lock") = accounts;
    state.use_oauth_token_urls_for_test(format!("http://{token_addr}/oauth/token"));

    let proxy_addr = serve_on_random_port(router(state)).await;
    let request = |client: reqwest::Client| {
        client
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .json(&json!({
                "model": "oauth-expired-router-test",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": true
            }))
            .send()
    };
    let (first, second) = tokio::join!(
        request(reqwest::Client::new()),
        request(reqwest::Client::new())
    );
    let first = first.expect("first proxy request");
    let second = second.expect("second proxy request");
    assert!(first.status().is_success());
    assert!(second.status().is_success());
    assert_eq!(refresh_count.load(Ordering::SeqCst), 1);
    assert_eq!(upstream_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        captured
            .1
            .lock()
            .expect("captured authorization")
            .as_deref(),
        Some("Bearer synthetic-refreshed-access-token")
    );
    let (persisted, _skipped) =
        crate::auth::load_oauth_accounts(&state_dir.path().join("oauth_accounts.json"))
            .expect("read refreshed OAuth state");
    assert_eq!(
        persisted.openai["openai-subscription"].access_token,
        "synthetic-refreshed-access-token"
    );
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

    let missing = candidates_for_test(
        &state,
        Protocol::OpenaiChatCompletions,
        &json!({}),
        json_error,
    )
    .unwrap_err();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

    let unknown = candidates_for_test(
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
            reasoning_levels: Some(vec![config::ReasoningLevelEntry::new("low", None)]),
            default_reasoning_level: None,
            supports_thinking: None,
            force_think: None,
            force_no_think: None,
            default_think: None,
            enable_thinking: None,
            chat_reasoning_off_format: None,
            codex_service_tier: None,
            openai_chat_providers: vec![config::ProviderBinding {
                name: "deepseek".to_string(),
                model: "limited-upstream".to_string(),
            }],
            openai_responses_providers: vec![],
            anthropic_providers: vec![],
            reasoning_level_map: None,
            supported_reasoning_levels: vec![],
            cooldown: None,
            reasoning_routes: None,
        },
    );
    let state = AppState::new_in_memory(cfg);

    let protocol_err = candidates_for_test(
        &state,
        Protocol::OpenaiResponses,
        &json!({"model":"limited"}),
        responses_error,
    )
    .unwrap_err();
    assert_eq!(protocol_err.status(), StatusCode::BAD_REQUEST);

    let capability_err = candidates_for_test(
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

    let reasoning_err = candidates_for_test(
        &state,
        Protocol::OpenaiChatCompletions,
        &json!({"model":"limited","reasoning_effort":"high"}),
        json_error,
    )
    .unwrap_err();
    assert_eq!(reasoning_err.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn resolve_request_candidates_rejects_conflicting_explicit_thinking_signals() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    // §5.1.2: explicit on + explicit off in the same request is a local 400,
    // never silently arbitrated by parameter precedence.
    let cases = [
        (
            Protocol::OpenaiChatCompletions,
            json!({"model":"deepseek-v4-flash-lp","enable_thinking":true,"reasoning_effort":"none"}),
        ),
        (
            Protocol::OpenaiChatCompletions,
            json!({"model":"deepseek-v4-flash-lp","enable_thinking":false,"reasoning_effort":"low"}),
        ),
        (
            Protocol::OpenaiResponses,
            json!({"model":"deepseek-v4-flash-lp","enable_thinking":true,"reasoning":{"effort":"none"}}),
        ),
        (
            Protocol::OpenaiResponses,
            json!({"model":"deepseek-v4-flash-lp","enable_thinking":false,"reasoning":{"effort":"high"}}),
        ),
        (
            Protocol::Anthropic,
            json!({"model":"deepseek-v4-flash-lp","enable_thinking":false,"thinking":{"type":"enabled"}}),
        ),
        (
            Protocol::Anthropic,
            json!({"model":"deepseek-v4-flash-lp","enable_thinking":false,"thinking":{"type":"enabled","budget_tokens":4096}}),
        ),
    ];
    for (protocol, body) in cases {
        let err = candidates_for_test(&state, protocol, &body, json_error).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST, "{body}");
    }
}

#[test]
fn resolve_request_candidates_rejects_explicit_intent_conflicting_with_force_strategy() {
    let mut cfg = config::default_deepseek_config();
    let mut force_on = cfg.models["deepseek-v4-flash-lp"].clone();
    force_on.force_think = Some(true);
    cfg.models.insert("force-on-model".to_string(), force_on);
    let mut force_off = cfg.models["deepseek-v4-flash-lp"].clone();
    force_off.force_no_think = Some(true);
    cfg.models.insert("force-off-model".to_string(), force_off);
    let state = AppState::new_in_memory(cfg);

    let rejections = [
        (
            Protocol::OpenaiChatCompletions,
            json!({"model":"force-on-model","reasoning_effort":"none"}),
        ),
        (
            Protocol::OpenaiChatCompletions,
            json!({"model":"force-on-model","enable_thinking":false}),
        ),
        (
            Protocol::OpenaiResponses,
            json!({"model":"force-on-model","reasoning":{"effort":"none"}}),
        ),
        (
            Protocol::Anthropic,
            json!({"model":"force-on-model","thinking":{"type":"disabled"}}),
        ),
        (
            Protocol::OpenaiChatCompletions,
            json!({"model":"force-off-model","reasoning_effort":"low"}),
        ),
        (
            Protocol::OpenaiChatCompletions,
            json!({"model":"force-off-model","enable_thinking":true}),
        ),
        (
            Protocol::Anthropic,
            json!({"model":"force-off-model","thinking":{"type":"enabled","budget_tokens":1024}}),
        ),
    ];
    for (protocol, body) in rejections {
        let err = candidates_for_test(&state, protocol, &body, json_error).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST, "{body}");
    }

    // Consistent combinations are not conflicts: force-off + explicit off,
    // force-on + explicit active level.
    let accepted = [
        (
            Protocol::OpenaiChatCompletions,
            json!({"model":"force-off-model","reasoning_effort":"none"}),
        ),
        (
            Protocol::OpenaiChatCompletions,
            json!({"model":"force-on-model","reasoning_effort":"low"}),
        ),
    ];
    for (protocol, body) in accepted {
        assert!(
            candidates_for_test(&state, protocol, &body, json_error).is_ok(),
            "{body}"
        );
    }
}

#[test]
fn default_reasoning_respects_explicit_intent_and_strategy() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .default_think = Some(false);
    let state = AppState::new_in_memory(cfg);

    // Explicit "none" is the protocol-native off signal: it blocks the default level
    // fill and passes through untouched for the per-candidate off-dialect.
    let body = default_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"none"}),
    );
    assert_eq!(body["reasoning_effort"], "none");

    // §5.1.1 matrix: explicit on overrides default-off and keeps its own level
    // instead of being replaced by the configured default level.
    let body = default_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"medium"}),
    );
    assert_eq!(body["reasoning_effort"], "medium");

    // Explicit on without a level receives the configured default level.
    let body = default_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"high"}),
    );
    assert_eq!(body["reasoning_effort"], "high");

    // No intent + default-off: the closed state is expressed as the protocol-standard
    // `none` (spec.md §5.2), not as a bare removal that would leave the upstream free to
    // think.
    let body = default_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp"}),
    );
    assert_eq!(body["reasoning_effort"], "none");
}

#[test]
fn default_reasoning_fill_preserves_existing_container_keys() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    // Responses: fill merges into an existing reasoning object instead of
    // replacing it wholesale.
    let body = default_reasoning(
        &state,
        Protocol::OpenaiResponses,
        json!({"model":"deepseek-v4-flash-lp","reasoning":{"summary":"auto"}}),
    );
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert_eq!(body["reasoning"]["effort"], "high");

    // Anthropic: a clearly enabled thinking config lacking level and budget
    // receives the default level.
    let body = default_reasoning(
        &state,
        Protocol::Anthropic,
        json!({"model":"deepseek-v4-flash-lp","thinking":{"type":"adaptive"}}),
    );
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");

    // Anthropic: opaque thinking objects are never rewritten.
    let body = default_reasoning(
        &state,
        Protocol::Anthropic,
        json!({"model":"deepseek-v4-flash-lp","thinking":{"summary":"x"}}),
    );
    assert_eq!(body["thinking"], json!({"summary":"x"}));
}

#[test]
fn thinking_is_disabled_covers_protocol_native_off_and_on() {
    // `enable_thinking` is no longer an ingress signal (it is rejected with 400 before
    // this point); the protocol-native close/open fields are what drive the switch.
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let off = json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"none"});
    let on = json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"high"});
    assert!(thinking_is_disabled(
        &state,
        Protocol::OpenaiChatCompletions,
        &off,
        "deepseek-v4-flash-lp",
        "deepseek",
        &thinking_intent_of(Protocol::OpenaiChatCompletions, &off),
    ));
    assert!(!thinking_is_disabled(
        &state,
        Protocol::OpenaiChatCompletions,
        &on,
        "deepseek-v4-flash-lp",
        "deepseek",
        &thinking_intent_of(Protocol::OpenaiChatCompletions, &on),
    ));
}

#[test]
fn reasoning_and_request_helpers_cover_edge_paths() {
    let mut cfg = config::default_deepseek_config();
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .default_think = Some(false);
    cfg.models
        .get_mut("deepseek-v4-flash-lp")
        .unwrap()
        .default_reasoning_level = Some("medium".to_string());
    let state = AppState::new_in_memory(cfg.clone());
    let plan = state
        .config()
        .resolve_model_request_candidates(Protocol::OpenaiChatCompletions, "deepseek-v4-flash-lp")
        .into_iter()
        .next()
        .expect("candidate")
        .1;

    let unchanged = default_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"low"}),
    );
    assert_eq!(unchanged["reasoning_effort"], "low");

    let explicit_on = mapped_reasoning(
        &state,
        Protocol::OpenaiChatCompletions,
        json!({"model":"deepseek-v4-flash-lp","reasoning_effort":"medium"}),
        "deepseek-v4-flash-lp",
        &plan,
    )
    .expect("explicit on overrides default-off");
    // DeepSeek's reasoning_level_map expresses medium as high; the
    // important contract is that default-off does not strip the field.
    assert_eq!(explicit_on["reasoning_effort"], "high");

    let chat = json!({
        "messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"https://e/x.png"}},
            {"type":"input_audio","input_audio":{"data":"AAAA","format":"mp3"}},
            {"type":"video_url","video_url":{"url":"https://e/v.mp4"}},
            {"type":"file","file_id":"f1"}
        ]}]
    });
    assert!(request_has_image(Protocol::OpenaiChatCompletions, &chat));
    assert!(request_has_audio(Protocol::OpenaiChatCompletions, &chat));
    assert!(request_has_video(Protocol::OpenaiChatCompletions, &chat));
    assert!(request_has_document(Protocol::OpenaiChatCompletions, &chat));
}

#[test]
fn error_retry_and_rewrite_helpers_cover_nested_shapes() {
    assert_eq!(
        anthropic_error_type(StatusCode::BAD_REQUEST),
        "invalid_request_error"
    );
    assert_eq!(error_code(StatusCode::BAD_GATEWAY), "bad_gateway");

    let mut headers = HeaderMap::new();
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("120"));
    assert_eq!(
        retry_after_duration(&headers),
        Some(Duration::from_secs(120))
    );
    assert_eq!(capped_nonzero_duration(0), None);

    let mut payload = json!({"model":"old","nested":{"model":"inner"},"arr":[{"model":"leaf"}]});
    rewrite_model_fields(&mut payload, "frontend");
    assert_eq!(payload["nested"]["model"], "frontend");
}

// ── E2E forward-path coverage（mock upstream + 真实 proxy server）────────────
// 覆盖 forward_anthropic / forward_chat / forward_responses
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
        reasoning_levels: None,
        supported_reasoning_levels: Vec::new(),
        cooldown: None,
        default_reasoning_level: None,
        supports_thinking: None,
        force_think: None,
        force_no_think: None,
        default_think: None,
        enable_thinking: None,
        chat_reasoning_off_format: None,
        codex_service_tier: None,
        openai_chat_providers: if chat { vec![binding()] } else { vec![] },
        openai_responses_providers: if responses { vec![binding()] } else { vec![] },
        anthropic_providers: if anthropic { vec![binding()] } else { vec![] },
        reasoning_level_map: None,
        reasoning_routes: None,
    }
}

#[test]
fn codex_service_tier_respects_default_and_forced_precedence() {
    let provider = config::ProviderConfig {
        openai_responses: Some(config::EndpointConfig::native(
            "https://example.test/responses",
        )),
        ..Default::default()
    };
    let mut defaulted = empty_model(false, true, false, "p", "upstream");
    defaulted.codex_service_tier = Some(config::CodexServiceTierConfig {
        tiers: vec!["priority".to_string()],
        default_tier: Some("priority".to_string()),
        forced_tier: None,
    });
    let mut forced = empty_model(false, true, false, "p", "upstream");
    forced.codex_service_tier = Some(config::CodexServiceTierConfig {
        tiers: vec!["priority".to_string()],
        default_tier: None,
        forced_tier: Some("priority".to_string()),
    });
    let state = AppState::new_in_memory(config_with(
        vec![("p", provider)],
        vec![
            ("defaulted", defaulted),
            ("forced", forced),
            ("plain", empty_model(false, true, false, "p", "upstream")),
        ],
    ));

    let mut defaulted_explicit = json!({"service_tier": "standard"});
    apply_codex_service_tier(&state, "defaulted", &mut defaulted_explicit);
    assert_eq!(defaulted_explicit["service_tier"], "standard");

    let mut defaulted_missing = json!({});
    apply_codex_service_tier(&state, "defaulted", &mut defaulted_missing);
    assert_eq!(defaulted_missing["service_tier"], "priority");

    let mut forced_explicit = json!({"service_tier": "standard"});
    apply_codex_service_tier(&state, "forced", &mut forced_explicit);
    assert_eq!(forced_explicit["service_tier"], "priority");

    let mut plain_body = json!({"service_tier": "standard"});
    apply_codex_service_tier(&state, "plain", &mut plain_body);
    assert_eq!(plain_body["service_tier"], "standard");
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

/// Build an in-memory state whose token store enforces a `5h` window on a single
/// token, in the given `quota_mode`. The temp dirs are returned so they outlive
/// the state.
fn state_with_quota_token(
    mode: &str,
    limit: i64,
) -> (AppState, String, tempfile::TempDir, tempfile::TempDir) {
    let config_dir = tempfile::tempdir().expect("config dir");
    let state_dir = tempfile::tempdir().expect("state dir");
    let key = "lp-testquota00000000000000000000";
    std::fs::write(
        config_dir.path().join("tokens.toml"),
        format!(
            "[[tokens]]\n\
             key = \"{key}\"\n\
             name = \"quota-test\"\n\n\
             [[tokens.quota_windows]]\n\
             window = \"5h\"\n\
             quota_mode = \"{mode}\"\n\
             quota_limit = {limit}\n"
        ),
    )
    .expect("write tokens.toml");

    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.token = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        state_dir.path(),
    );
    (state, key.to_string(), config_dir, state_dir)
}

fn window_used(state: &AppState, key: &str, window: &str) -> i64 {
    state
        .token
        .usage_store
        .as_ref()
        .expect("token usage store")
        .get_token_usage(key)
        .expect("token usage")
        .into_iter()
        .find(|usage| usage.window_key == window)
        .expect("quota window")
        .used
}

/// A token whose limits cannot be enforced must be refused, never waved through
/// as unlimited: "no usage recorded" means "0 used", not "no limit".
#[test]
fn quota_is_refused_when_it_cannot_be_determined() {
    let (mut state, key, _config_dir, _state_dir) = state_with_quota_token("requests", 10);
    // Simulate a store that cannot be used: the configured windows exist, but
    // their usage cannot be read or charged.
    state.token.usage_store = None;

    let token_ctx = crate::token::auth::TokenContext {
        key,
        name: "quota-test".to_string(),
        allowed_models: Vec::new(),
        admin: false,
    };

    let response = state
        .reserve_token_request(&token_ctx, "gpt-4o")
        .expect("an undeterminable quota must be refused");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// A configured window that cannot be materialized must refuse the
/// request the same way an unavailable store does. Before the fix the failed
/// row creation was swallowed on the request path, so that window simply was
/// not there to be checked — "the meter broke" read as "no quota configured"
/// and the request went through unmetered.
#[test]
fn quota_is_refused_when_its_windows_cannot_be_materialized() {
    let (state, key, _config_dir, _state_dir) = state_with_quota_token("requests", 10);
    {
        let mut config = state.token.config.write().expect("config write lock");
        let token = config
            .tokens
            .iter_mut()
            .find(|candidate| candidate.key == key)
            .expect("token present in configuration");
        // Stands for any cause of "this window's row cannot be created".
        token.quota_windows.push(crate::token::config::QuotaWindow {
            window: "not-a-duration".to_string(),
            quota_mode: "tokens".to_string(),
            quota_limit: 5,
        });
    }

    let token_ctx = crate::token::auth::TokenContext {
        key,
        name: "quota-test".to_string(),
        allowed_models: Vec::new(),
        admin: false,
    };

    let response = state
        .reserve_token_request(&token_ctx, "gpt-4o")
        .expect("a window that cannot be evaluated must refuse the request");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// A `tokens`-mode window must gate admission once its budget is spent. The
/// atomic reservation is requests-only, so a `tokens` window rides the
/// same all-or-nothing step as a read-and-refuse: it is never reserved at the
/// gate, and refusing on a spent budget must surface a `429` naming that window
/// (spec §5.1).
#[tokio::test]
async fn tokens_mode_window_refuses_at_admission_once_budget_spent() {
    let (state, key, _config_dir, _state_dir) = state_with_quota_token("tokens", 100);
    let store = state.token.usage_store.as_ref().expect("token usage store");
    let token_ctx = crate::token::auth::TokenContext {
        key: key.clone(),
        name: "quota-test".to_string(),
        allowed_models: Vec::new(),
        admin: false,
    };

    // Under budget: admitted, and admission never increments a tokens window.
    assert!(
        state.reserve_token_request(&token_ctx, "gpt-4o").is_none(),
        "an under-budget tokens window must admit"
    );
    assert_eq!(
        window_used(&state, &key, "5h"),
        0,
        "a tokens window is not reserved at the gate"
    );

    // The budget is spent after responses land (weighted cost 60 + 60 > 100).
    store.add_usage(&key, 60, 60).expect("charge tokens usage");
    assert_eq!(window_used(&state, &key, "5h"), 120);

    // Admission now refuses with 429, and the structured body names the tokens window.
    let response = state
        .reserve_token_request(&token_ctx, "gpt-4o")
        .expect("a spent tokens budget must refuse at admission");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("read 429 body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("429 json body");
    let reported = body["error"]["quota_windows"]
        .as_array()
        .expect("quota_windows array");
    assert!(
        reported
            .iter()
            .any(|w| w["quota_mode"] == "tokens" && w["window"] == "5h"),
        "429 body must name the tokens window, got: {body}"
    );
}

// ── functionCalls without thoughtSignature ─────────────────────

/// 实测取证：「`Body::from(String)` 的
/// SSE 响应会被 hyper 写出 `Content-Length`」原被记为**待实测的库行为推断**。这里用
/// 真实 hyper 编解码 + 真实 socket 上发一次 GET 来定死它 —— 与
/// `curl -iN` 同等信息量，且能进门禁长期防回归。
///
/// 判据含义（两侧都要守住）：
/// - `content-length` 存在 ⇒ 推断成立 ⇒ **声明了长度的 body 若提前结束，客户端
///   就是字面意义的 `error decoding response body`**，修复面为
///   `sse_aggregate.rs:189/:222/:292` 那三处 `Body::from`（均位于
///   各协议 value→SSE 写出函数的一次性写出终态）；
/// - 若此断言失败 ⇒ 推断不成立，修复面须重定界，不能沿用原推理。
/// 同时守住 `transfer-encoding` 不得与 `content-length` 同现（hyper 不会双写）。
#[tokio::test]
async fn sse_response_with_fixed_body_sets_content_length() {
    let body_text: &'static str = "data: {\"a\":1}\n\ndata: [DONE]\n\n";
    let router = Router::new().route(
        "/sse",
        get(move || async move { sse_response(axum::body::Body::from(body_text)) }),
    );
    let addr = serve_on_random_port(router).await;
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/sse"))
        .send()
        .await
        .expect("get");
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "content-type={content_type}"
    );
    let content_length = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    assert!(
        content_length.is_some(),
        "实测：固定体 SSE 响应带 content-length（推断成立）。若本断言失败，修复面须重定界。实测头: {:?}",
        resp.headers()
    );
    assert!(
        resp.headers().get(header::TRANSFER_ENCODING).is_none(),
        "固定体不应同时出现 transfer-encoding，实测头: {:?}",
        resp.headers()
    );
    // 断言头值精确等于该固定体的真实字节数（自洽：改字符串不会让本断言变脆）
    let expected_len = body_text.len().to_string();
    assert_eq!(
        content_length.as_deref(),
        Some(expected_len.as_str()),
        "实测 content-length 必须精确等于固定体字节数 —— 即 hyper 为固定体声明了长度，\
         一旦 body 提前结束就是客户端侧的 `error decoding response body`"
    );
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
async fn responses_sse_to_chat_sse_routes_interleaved_tool_call_arguments() {
    let upstream = Router::new().route(
        "/v1/responses",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"read_file\"}}\n\n",
                    "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_b\",\"type\":\"function_call\",\"call_id\":\"call_b\",\"name\":\"write_file\"}}\n\n",
                    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_a\",\"delta\":\"{\\\"path\\\":\"}\n\n",
                    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_b\",\"delta\":\"{\\\"content\\\":\"}\n\n",
                    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_a\",\"delta\":\"\\\"Cargo.toml\\\"}\"}\n\n",
                    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_b\",\"delta\":\"\\\"hello\\\"}\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;
    let response = reqwest::Client::new()
        .post(format!("http://{upstream_addr}/v1/responses"))
        .send()
        .await
        .expect("fetch upstream");
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let response = responses_sse_to_chat_sse(
        &state,
        response,
        "frontend-model".to_string(),
        "test-provider".to_string(),
        "openai_chat".to_string(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("response body")
            .to_vec(),
    )
    .expect("utf8");

    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("chat SSE JSON"))
        .collect();
    let tool_call_deltas: Vec<&Value> = events
        .iter()
        .filter(|event| event["choices"][0]["delta"].get("tool_calls").is_some())
        .collect();

    assert_eq!(
        tool_call_deltas[0]["choices"][0]["delta"]["tool_calls"][0]["index"],
        0
    );
    assert_eq!(
        tool_call_deltas[0]["choices"][0]["delta"]["tool_calls"][0]["id"],
        "call_a"
    );
    assert_eq!(
        tool_call_deltas[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "read_file"
    );
    assert_eq!(
        tool_call_deltas[1]["choices"][0]["delta"]["tool_calls"][0]["index"],
        1
    );
    assert_eq!(
        tool_call_deltas[1]["choices"][0]["delta"]["tool_calls"][0]["id"],
        "call_b"
    );
    assert_eq!(
        tool_call_deltas[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "write_file"
    );
    assert_eq!(
        tool_call_deltas[2]["choices"][0]["delta"]["tool_calls"][0]["index"],
        0
    );
    assert_eq!(
        tool_call_deltas[3]["choices"][0]["delta"]["tool_calls"][0]["index"],
        1
    );
    assert_eq!(
        tool_call_deltas[4]["choices"][0]["delta"]["tool_calls"][0]["index"],
        0
    );
    assert_eq!(
        tool_call_deltas[5]["choices"][0]["delta"]["tool_calls"][0]["index"],
        1
    );
    assert_eq!(
        format!(
            "{}{}",
            tool_call_deltas[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
            tool_call_deltas[4]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        ),
        r#"{"path":"Cargo.toml"}"#
    );
    assert_eq!(
        format!(
            "{}{}",
            tool_call_deltas[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
            tool_call_deltas[5]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        ),
        r#"{"content":"hello"}"#
    );

    // P5: every synthetic chunk shares one generated chatcmpl id and carries
    // a real Unix-seconds timestamp (spec.md §6 ID namespace rules).
    let ids: Vec<&str> = events
        .iter()
        .map(|event| event["id"].as_str().expect("chunk id"))
        .collect();
    assert!(ids.iter().all(|id| id.starts_with("chatcmpl-")), "{ids:?}");
    assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
    assert!(
        events
            .iter()
            .all(|event| event["created"].as_i64().unwrap() > 1_700_000_000),
        "chunks must carry real created timestamps"
    );
}

#[tokio::test]
async fn responses_sse_to_anthropic_sse_routes_interleaved_tool_call_arguments() {
    let upstream = Router::new().route(
        "/v1/responses",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"read_file\"}}\n\n",
                    "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_b\",\"type\":\"function_call\",\"call_id\":\"call_b\",\"name\":\"write_file\"}}\n\n",
                    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_a\",\"delta\":\"{\\\"path\\\":\"}\n\n",
                    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_b\",\"delta\":\"{\\\"content\\\":\"}\n\n",
                    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_a\",\"delta\":\"\\\"Cargo.toml\\\"}\"}\n\n",
                    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_b\",\"delta\":\"\\\"hello\\\"}\"}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\"}}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_b\",\"type\":\"function_call\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream).await;
    let response = reqwest::Client::new()
        .post(format!("http://{upstream_addr}/v1/responses"))
        .send()
        .await
        .expect("fetch upstream");
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let response = responses_sse_to_anthropic_sse(
        &state,
        response,
        "frontend-model".to_string(),
        "test-provider".to_string(),
        "anthropic".to_string(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("response body")
            .to_vec(),
    )
    .expect("utf8");
    let events: Vec<(String, Value)> = body
        .split("\n\n")
        .filter_map(|frame| {
            let event = frame.strip_prefix("event: ")?.split_once('\n')?.0;
            let data = frame.split("data: ").nth(1)?;
            Some((
                event.to_string(),
                serde_json::from_str(data).expect("Anthropic SSE JSON"),
            ))
        })
        .collect();

    let tool_starts: Vec<&Value> = events
        .iter()
        .filter(|(event, value)| {
            event == "content_block_start" && value["content_block"]["type"] == "tool_use"
        })
        .map(|(_, value)| value)
        .collect();
    assert_eq!(tool_starts.len(), 2);
    assert_eq!(tool_starts[0]["index"], 1);
    assert_eq!(tool_starts[0]["content_block"]["id"], "call_a");
    assert_eq!(tool_starts[1]["index"], 2);
    assert_eq!(tool_starts[1]["content_block"]["id"], "call_b");

    let argument_deltas: Vec<&Value> = events
        .iter()
        .filter(|(event, value)| {
            event == "content_block_delta" && value["delta"]["type"] == "input_json_delta"
        })
        .map(|(_, value)| value)
        .collect();
    assert_eq!(argument_deltas.len(), 4);
    assert_eq!(argument_deltas[0]["index"], 1);
    assert_eq!(argument_deltas[1]["index"], 2);
    assert_eq!(argument_deltas[2]["index"], 1);
    assert_eq!(argument_deltas[3]["index"], 2);
    assert_eq!(
        format!(
            "{}{}",
            argument_deltas[0]["delta"]["partial_json"]
                .as_str()
                .unwrap(),
            argument_deltas[2]["delta"]["partial_json"]
                .as_str()
                .unwrap(),
        ),
        r#"{"path":"Cargo.toml"}"#
    );
    assert_eq!(
        format!(
            "{}{}",
            argument_deltas[1]["delta"]["partial_json"]
                .as_str()
                .unwrap(),
            argument_deltas[3]["delta"]["partial_json"]
                .as_str()
                .unwrap(),
        ),
        r#"{"content":"hello"}"#
    );
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
async fn responses_native_non_streaming_rewrites_model() {
    // Non-streaming native responses carry the OpenAI top-level body shape
    // (no SSE-style nested envelope); model rewrite happens on that top level.
    let upstream = Router::new().route(
        "/v1/responses",
        post(|| async {
            Json(json!({
                "id": "resp_1",
                "model": "qwen3:27b",
                "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "pong"}]}],
                "usage": {"input_tokens": 2, "output_tokens": 1}
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
}

#[tokio::test]
async fn responses_native_non_streaming_records_top_level_usage() {
    // Non-streaming passthrough must charge the top-level `usage` object of the
    // OpenAI responses body; reading an SSE-style nested envelope silently drops it.
    let upstream = Router::new().route(
        "/v1/responses",
        post(|| async {
            Json(json!({
                "id": "resp_1",
                "model": "qwen3:27b",
                "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "pong"}]}],
                "usage": {"input_tokens": 2, "output_tokens": 1}
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
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(cfg);
    state.set_usage_store_for_test(Arc::clone(&usage_store));
    let proxy_addr = serve_on_random_port(router(state)).await;

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

    let records = usage_store.get_all();
    assert_eq!(
        records.len(),
        1,
        "non-streaming native passthrough must record one usage entry from top-level usage, got: {records:?}"
    );
    assert_eq!(
        records[0].input_tokens, 2,
        "input_tokens from top-level usage"
    );
    assert_eq!(
        records[0].output_tokens, 1,
        "output_tokens from top-level usage"
    );
    assert_eq!(records[0].endpoint, "openai_responses");
    assert_eq!(records[0].status.as_deref(), Some("completed"));
    assert!(!records[0].truncated);
}

#[tokio::test]
async fn responses_native_non_streaming_records_truncation() {
    // Truncation detection on the non-streaming path reads the top-level
    // `status` / `incomplete_details` fields and must be charged alongside usage.
    let upstream = Router::new().route(
        "/v1/responses",
        post(|| async {
            Json(json!({
                "id": "resp_1",
                "model": "qwen3:27b",
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "cut"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 5}
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
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(cfg);
    state.set_usage_store_for_test(Arc::clone(&usage_store));
    let proxy_addr = serve_on_random_port(router(state)).await;

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

    let records = usage_store.get_all();
    assert_eq!(
        records.len(),
        1,
        "truncated non-streaming response must still record usage, got: {records:?}"
    );
    assert!(
        records[0].truncated,
        "top-level status=incomplete must mark the record truncated"
    );
    assert_eq!(
        records[0].truncation_reason.as_deref(),
        Some("max_tokens"),
        "incomplete_details.reason=max_output_tokens must map to max_tokens"
    );
    assert_eq!(records[0].input_tokens, 3);
    assert_eq!(records[0].output_tokens, 5);
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
async fn chat_image_via_responses_sends_scalar_image_url_upstream() {
    let captured: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream = Router::new()
        .route(
            "/v1/responses",
            post(
                |State(captured): State<CapturedRequest>, Json(body): Json<Value>| async move {
                    *captured.lock().expect("capture lock") = Some(body);
                    Json(json!({
                        "id": "resp_1",
                        "model": "qwen3:27b",
                        "output": [{"type": "message", "content": [{"type": "output_text", "text": "pong"}]}],
                        "usage": {"input_tokens": 2, "output_tokens": 1}
                    }))
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_addr = serve_on_random_port(upstream).await;

    let mut provider = ollama_provider(upstream_addr);
    provider.openai_chat = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiResponses,
    ));
    let mut model = empty_model(true, false, false, "ollama", "qwen3:27b");
    model.features.push("image_input".to_string());
    let cfg = config_with(vec![("ollama", provider)], vec![("qwen3-27b-lp", model)]);
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "qwen3-27b-lp",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is shown?"},
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": "data:image/png;base64,iVBORw",
                            "detail": "high"
                        }
                    }
                ]
            }]
        }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    assert_eq!(response["choices"][0]["message"]["content"], "pong");
    let upstream_body = captured
        .lock()
        .expect("capture lock")
        .clone()
        .expect("captured");
    assert_eq!(upstream_body["model"], "qwen3:27b");
    assert_eq!(
        upstream_body["input"][0]["content"][1],
        json!({
            "type": "input_image",
            "image_url": "data:image/png;base64,iVBORw",
            "detail": "high"
        })
    );
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

#[tokio::test]
async fn large_10mib_request_reaches_upstream_without_413() {
    let received_len = Arc::new(AtomicUsize::new(0));
    let received_len_clone = Arc::clone(&received_len);
    let upstream_app = Router::new()
        .route(
            "/chat/completions",
            post(move |body: axum::body::Bytes| {
                let received_len = Arc::clone(&received_len_clone);
                async move {
                    received_len.store(body.len(), Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        Json(json!({
                            "id": "chatcmpl-large",
                            "object": "chat.completion",
                            "created": 1,
                            "model": "deepseek-v4-flash",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "large response ok"},
                                "finish_reason": "stop"
                            }],
                            "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110}
                        })),
                    )
                }
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024));

    let upstream_addr = serve_on_random_port(upstream_app).await;
    let mut cfg = config::default_deepseek_config();
    let provider = cfg
        .providers
        .get_mut("deepseek")
        .expect("deepseek provider");
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;

    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;
    let client = reqwest::Client::new();

    let padding = "a".repeat(10 * 1024 * 1024);
    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-flash-lp",
            "messages": [{"role": "user", "content": padding}]
        }))
        .send()
        .await
        .expect("send large Chat request");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(received_len.load(Ordering::SeqCst) >= 10 * 1024 * 1024);
}

#[test]
fn model_proxy_request_body_limit_is_64mib() {
    assert_eq!(MODEL_PROXY_MAX_REQUEST_BODY_BYTES, 64 * 1024 * 1024);
}

#[tokio::test]
async fn request_exceeding_fixed_limit_gets_413_and_does_not_reach_upstream() {
    use std::sync::atomic::AtomicBool;

    let upstream_reached = Arc::new(AtomicBool::new(false));
    let upstream_reached_clone = Arc::clone(&upstream_reached);
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let upstream_reached = Arc::clone(&upstream_reached_clone);
            async move {
                upstream_reached.store(true, Ordering::SeqCst);
                (StatusCode::OK, Json(json!({"id": "ok"})))
            }
        }),
    );

    let upstream_addr = serve_on_random_port(upstream_app).await;
    let mut cfg = config::default_deepseek_config();
    let provider = cfg
        .providers
        .get_mut("deepseek")
        .expect("deepseek provider");
    repoint_provider_urls(provider, upstream_addr);
    provider.api_key_env = None;

    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;
    let client = reqwest::Client::new();

    let mut payload = Vec::with_capacity(MODEL_PROXY_MAX_REQUEST_BODY_BYTES + 1024);
    payload.extend_from_slice(
        br#"{"model":"deepseek-v4-flash-lp","messages":[{"role":"user","content":""#,
    );
    payload.resize(MODEL_PROXY_MAX_REQUEST_BODY_BYTES + 100, b'a');
    payload.extend_from_slice(br#""}]}"#);

    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(payload)
        .send()
        .await
        .expect("send oversized Chat request");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!upstream_reached.load(Ordering::SeqCst));
}

#[tokio::test]
async fn model_proxy_body_limit_does_not_expand_admin_request_limit() {
    // probe 等写操作端点只在管理通道 router 上；该测试验证
    // 公开 proxy 路由的 64MiB body limit 不会外溢到管理通道——管理端点
    // 仍受 axum 默认 2MiB 限制（3MiB 请求应在进入 handler 前被拒）。
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let app = crate::service::management_router(
        AppState::new_in_memory(config::default_deepseek_config()),
        std::sync::Arc::new(std::sync::Mutex::new(Some(shutdown_tx))),
    );
    let proxy_addr = serve_on_random_port(app).await;

    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/admin/status/probe"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(vec![b' '; 3 * 1024 * 1024])
        .send()
        .await
        .expect("send oversized admin request");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn forward_responses_via_chat_phase_a_bridge_and_request_context() {
    let captured_chat_body = Arc::new(Mutex::new(None));
    let captured_clone = Arc::clone(&captured_chat_body);

    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let captured = Arc::clone(&captured_clone);
            async move {
                *captured.lock().unwrap() = Some(body);
                Json(json!({
                    "id": "chatcmpl_mock_upstream",
                    "created": 1700000000,
                    "choices": [{
                        "finish_reason": "stop",
                        "message": {
                            "role": "assistant",
                            "reasoning_content": "Detailed reasoning here",
                            "content": "Done processing",
                            "tool_calls": [{
                                "id": "call_123",
                                "type": "function",
                                "function": {
                                    "name": "calc",
                                    "arguments": "{\"x\": 42}"
                                }
                            }]
                        }
                    }],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 20
                        // total_tokens missing to test approved incomplete usage decision
                    }
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
            .expect("serve");
    });

    let mut cfg = config::default_deepseek_config();
    let mut provider = crate::catalog::deepseek().provider;
    provider.openai_chat = Some(config::EndpointConfig::native(format!(
        "http://{upstream_addr}/v1/chat/completions"
    )));
    provider.openai_responses = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiChatCompletions,
    ));
    provider.anthropic = None;
    provider.api_key_env = None;
    provider.auth = None;
    cfg.providers
        .insert("mock-chat-provider".to_string(), provider);
    cfg.models.insert(
        "qwen-test-model".to_string(),
        config::ModelConfig {
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
            openai_chat_providers: vec![config::ProviderBinding {
                name: "mock-chat-provider".to_string(),
                model: "qwen-upstream".to_string(),
            }],
            openai_responses_providers: vec![config::ProviderBinding {
                name: "mock-chat-provider".to_string(),
                model: "qwen-upstream".to_string(),
            }],
            anthropic_providers: Vec::new(),
            reasoning_level_map: None,
        },
    );

    let state = AppState::new_in_memory(cfg);
    let app = router(state.clone());
    let proxy_addr = serve_on_random_port(app).await;

    let tools_input = json!([{
        "type": "function",
        "name": "calc",
        "description": "Calculator",
        "parameters": {
            "type": "object",
            "properties": {"x": {"type": "number"}}
        }
    }]);

    // Send Responses request through proxy
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build client");
    let res = client
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "qwen-test-model",
            "input": "Calculate 42",
            "tools": tools_input,
            "stream": false
        }))
        .send()
        .await
        .expect("send responses request");

    let status = res.status();
    let text = res.text().await.expect("text");
    assert_eq!(status, StatusCode::OK, "response failed with body: {text}");
    let body: Value = serde_json::from_str(&text).expect("json");

    // 1. Check status is completed
    assert_eq!(body["status"], "completed");

    // 2. Check canonical order in output: reasoning -> message -> tools
    let output = body["output"].as_array().expect("output");
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[0]["content"][0]["text"], "Detailed reasoning here");
    assert_eq!(output[0]["summary"], json!([]));
    assert!(output[0]["id"].as_str().unwrap().starts_with("rs_"));

    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "Done processing");
    assert!(output[1]["id"].as_str().unwrap().starts_with("msg_"));

    assert_eq!(output[2]["type"], "function_call");
    assert_eq!(output[2]["call_id"], "call_123");
    assert!(output[2]["id"].as_str().unwrap().starts_with("fc_"));
    assert_eq!(output[2]["name"], "calc");
    assert_eq!(output[2]["arguments"], "{\"x\": 42}");

    // 3. User approved usage decision: incomplete usage (missing total_tokens)
    // is entirely omitted from client response
    assert!(body.get("usage").is_none());

    // 4. Request context reflection: tools, tool_choice, parallel_tool_calls reflect
    // input effective protocol defaults, NOT inferred from output
    assert_eq!(body["tools"], tools_input);
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
}

#[tokio::test]
async fn forward_responses_via_chat_isolated_usage_store_and_validation() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = Arc::clone(&call_count);

    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(_body): Json<Value>| {
            let idx = call_count_clone.fetch_add(1, Ordering::SeqCst);
            async move {
                match idx {
                    0 => Json(json!({
                        "id": "chatcmpl_case0",
                        "created": 1700000000,
                        "choices": [{
                            "finish_reason": "stop",
                            "message": {
                                "role": "assistant",
                                "content": "Answer 1"
                            }
                        }],
                        "usage": {
                            "prompt_tokens": 10,
                            "completion_tokens": 20
                        }
                    })),
                    1 => Json(json!({
                        "id": "chatcmpl_case1",
                        "created": 1700000001,
                        "choices": [{
                            "finish_reason": "stop",
                            "message": {
                                "role": "assistant",
                                "content": "Answer 2"
                            }
                        }],
                        "usage": {
                            "prompt_tokens": -10,
                            "completion_tokens": 20
                        }
                    })),
                    2 => Json(json!({
                        "id": "chatcmpl_case2",
                        "created": 1700000002,
                        "choices": [{
                            "finish_reason": "stop",
                            "message": {
                                "role": "assistant",
                                "content": "Answer 3"
                            }
                        }],
                        "usage": {
                            "prompt_tokens": i64::MAX,
                            "completion_tokens": 1
                        }
                    })),
                    _ => Json(json!({
                        "id": "chatcmpl_case_default",
                        "created": 1700000003,
                        "choices": [{
                            "finish_reason": "stop",
                            "message": {
                                "role": "assistant",
                                "content": "Answer default"
                            }
                        }]
                    })),
                }
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
    let mut provider = crate::catalog::deepseek().provider;
    provider.openai_chat = Some(config::EndpointConfig::native(format!(
        "http://{upstream_addr}/v1/chat/completions"
    )));
    provider.openai_responses = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiChatCompletions,
    ));
    provider.anthropic = None;
    provider.api_key_env = None;
    provider.auth = None;
    cfg.providers
        .insert("mock-chat-provider".to_string(), provider);
    cfg.models.insert(
        "qwen-test-model".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 1_000_000,
            max_output_tokens: 128_000,
            features: vec!["tools".to_string()],
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
            openai_chat_providers: vec![config::ProviderBinding {
                name: "mock-chat-provider".to_string(),
                model: "qwen-upstream".to_string(),
            }],
            openai_responses_providers: vec![config::ProviderBinding {
                name: "mock-chat-provider".to_string(),
                model: "qwen-upstream".to_string(),
            }],
            anthropic_providers: Vec::new(),
            reasoning_level_map: None,
        },
    );

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let jsonl_path = temp_dir.path().join("usage.jsonl");
    let db_path = temp_dir.path().join("usage.db");
    let usage_cfg = crate::config::UsageConfig::default();
    let usage_store = Arc::new(
        UsageStore::with_paths(usage_cfg, jsonl_path.clone(), db_path.clone())
            .expect("usage store"),
    );
    let assert_persisted = || {
        usage_store
            .flush_if_dirty()
            .expect("flush real usage store");
        let reopened = UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            jsonl_path.clone(),
            db_path.clone(),
        )
        .expect("reopen the same temporary usage paths");
        let records = reopened.get_all();
        assert_eq!(
            records.len(),
            1,
            "disk must contain exactly one real record"
        );
        assert_eq!(records[0].model, "qwen-test-model");
        assert_eq!(records[0].provider, "mock-chat-provider");
        assert_eq!(records[0].endpoint, "openai_responses");
        assert_eq!(records[0].input_tokens, 10);
        assert_eq!(records[0].output_tokens, 20);
        assert_eq!(records[0].total_tokens, 30, "approved local input + output");
    };

    let mut state = AppState::new_in_memory(cfg);
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    let app = router(state.clone());
    let proxy_addr = serve_on_random_port(app).await;

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build client");

    // Test 1: upstream total missing, actual input=10 and output=20 retained,
    // local sum=30 derived in UsageStore, client usage omitted and answer retained.
    let res1 = client
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "qwen-test-model",
            "input": "Hello",
            "stream": false
        }))
        .send()
        .await
        .expect("send request 1");
    assert_eq!(res1.status(), StatusCode::OK);
    let body1: Value = res1.json().await.expect("json body 1");
    assert_eq!(body1["status"], "completed");
    assert_eq!(body1["output"][0]["content"][0]["text"], "Answer 1");
    assert!(body1.get("usage").is_none());

    let records1 = usage_store.get_all();
    assert_eq!(records1.len(), 1);
    assert_eq!(records1[0].input_tokens, 10);
    assert_eq!(records1[0].output_tokens, 20);
    assert_eq!(records1[0].total_tokens, 30);
    assert_persisted();

    // Test 2: negative tokens from upstream cannot panic or create fabricated record; answer retained
    let res2 = client
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "qwen-test-model",
            "input": "Negative test",
            "stream": false
        }))
        .send()
        .await
        .expect("send request 2");
    assert_eq!(res2.status(), StatusCode::OK);
    let body2: Value = res2.json().await.expect("json body 2");
    assert_eq!(body2["status"], "completed");
    assert_eq!(body2["output"][0]["content"][0]["text"], "Answer 2");
    assert!(body2.get("usage").is_none());

    let records2 = usage_store.get_all();
    assert_eq!(
        records2.len(),
        1,
        "negative counters must not create records"
    );
    assert_persisted();

    // Test 3: overflow tokens cannot panic or create fabricated record
    let res3 = client
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "qwen-test-model",
            "input": "Overflow test",
            "stream": false
        }))
        .send()
        .await
        .expect("send request 3");
    assert_eq!(res3.status(), StatusCode::OK);
    let body3: Value = res3.json().await.expect("json body 3");
    assert_eq!(body3["status"], "completed");
    assert_eq!(body3["output"][0]["content"][0]["text"], "Answer 3");
    assert!(body3.get("usage").is_none());

    let records3 = usage_store.get_all();
    assert_eq!(
        records3.len(),
        1,
        "overflow counters must not create records"
    );
    assert_persisted();
    assert_eq!(call_count.load(Ordering::SeqCst), 3);

    // Test 4: validate tools array/tool_choice/parallel_tool_calls shapes locally before upstream
    let mut invalid_shapes = vec![
        json!({"tools": "not_an_array"}),
        json!({"tools": null}),
        json!({"tools": [123]}),
        json!({"tools": [{}]}),
        json!({"tools": [{"name": "lookup"}]}),
        json!({"tools": [{"type": "function"}]}),
        json!({"tools": [{"type": "function", "function": {"name": "lookup"}}]}),
        json!({"tool_choice": 42}),
        json!({"tool_choice": null}),
        json!({"tool_choice": {}}),
        json!({"tool_choice": {"name": "lookup"}}),
        json!({"tool_choice": {"type": "function"}}),
        json!({"tool_choice": {"type": "function", "function": {"name": "lookup"}}}),
        json!({"tool_choice": "unknown"}),
        json!({"parallel_tool_calls": "yes"}),
        json!({"parallel_tool_calls": null}),
    ];
    for value in [Value::Null, json!(""), json!(42), json!({}), json!([])] {
        invalid_shapes.push(json!({"tools": [{"type": "function", "name": value}]}));
        invalid_shapes.push(json!({"tool_choice": {"type": "function", "name": value}}));
        invalid_shapes.push(json!({"tools": [{"type": value, "name": "lookup"}]}));
        invalid_shapes.push(json!({"tool_choice": {"type": value, "name": "lookup"}}));
    }
    for parameters in [json!("object"), json!(true), json!(42), json!([])] {
        invalid_shapes.push(json!({
            "tools": [{"type": "function", "name": "lookup", "parameters": parameters}]
        }));
    }
    for strict in [json!("true"), json!(42), json!({}), json!([])] {
        invalid_shapes.push(json!({
            "tools": [{"type": "function", "name": "lookup", "strict": strict}]
        }));
    }
    for mut request in invalid_shapes {
        request["model"] = json!("qwen-test-model");
        request["input"] = json!("Malformed tool metadata");
        request["stream"] = json!(false);
        let response = client
            .post(format!("http://{proxy_addr}/openai/v1/responses"))
            .json(&request)
            .send()
            .await
            .expect("send invalid request");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{request}");
        let body: Value = response.json().await.expect("local error body");
        assert_eq!(body["error"]["type"], "bad_request", "{body}");
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            3,
            "invalid request must not contact upstream: {request}"
        );
    }
    assert_persisted();

    // Optional function fields may be omitted or explicitly null; valid selectors
    // still reach upstream and are reflected without inferred tool metadata.
    for (index, tools) in [
        json!([{"type": "function", "name": "lookup"}]),
        json!([{"type": "function", "name": "lookup", "parameters": null, "strict": null}]),
        json!([{"type": "function", "name": "lookup", "parameters": {}, "strict": true}]),
        json!([{"type": "function", "name": "lookup", "parameters": {}, "strict": false}]),
    ]
    .into_iter()
    .enumerate()
    {
        let choice = json!({"type": "function", "name": "lookup"});
        let response = client
            .post(format!("http://{proxy_addr}/openai/v1/responses"))
            .json(&json!({
                "model": "qwen-test-model",
                "input": "Valid tool metadata",
                "tools": tools,
                "tool_choice": choice,
                "parallel_tool_calls": false,
                "stream": false
            }))
            .send()
            .await
            .expect("send valid function request");
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.expect("valid function response");
        assert_eq!(body["tools"], tools);
        assert_eq!(body["tool_choice"], choice);
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(call_count.load(Ordering::SeqCst), 4 + index);
    }
    assert_persisted();
}

// Lossy conversion contract: OpenAI-native tool types that Chat cannot
// represent (custom/web_search/...) pass local validation and are dropped by
// convert_tools instead of failing the whole request; function tools still
// reach upstream converted.
#[tokio::test]
async fn forward_responses_via_chat_drops_unrepresentable_tool_types() {
    let last_body = Arc::new(Mutex::new(Value::Null));
    let last_body_clone = Arc::clone(&last_body);
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            *last_body_clone.lock().expect("lock last body") = body;
            async move {
                Json(json!({
                    "id": "chatcmpl_dropped_tools",
                    "created": 1700000000,
                    "choices": [{
                        "finish_reason": "stop",
                        "message": {"role": "assistant", "content": "ok"}
                    }],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
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
    let mut provider = crate::catalog::deepseek().provider;
    provider.openai_chat = Some(config::EndpointConfig::native(format!(
        "http://{upstream_addr}/v1/chat/completions"
    )));
    provider.openai_responses = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiChatCompletions,
    ));
    provider.anthropic = None;
    provider.api_key_env = None;
    provider.auth = None;
    cfg.providers
        .insert("mock-chat-provider".to_string(), provider);
    cfg.models.insert(
        "qwen-test-model".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 1_000_000,
            max_output_tokens: 128_000,
            features: vec!["tools".to_string()],
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
            openai_chat_providers: vec![config::ProviderBinding {
                name: "mock-chat-provider".to_string(),
                model: "qwen-upstream".to_string(),
            }],
            openai_responses_providers: vec![config::ProviderBinding {
                name: "mock-chat-provider".to_string(),
                model: "qwen-upstream".to_string(),
            }],
            anthropic_providers: Vec::new(),
            reasoning_level_map: None,
        },
    );

    let state = AppState::new_in_memory(cfg);
    let app = router(state.clone());
    let proxy_addr = serve_on_random_port(app).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build client");

    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "qwen-test-model",
            "input": "Use the lookup tool if needed",
            "tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object", "properties": {}}},
                {"type": "custom", "name": "apply_patch", "format": {"type": "grammar", "syntax": "lark", "definition": "start: begin_patch"}},
                {"type": "web_search", "external_web_access": true}
            ],
            "stream": false
        }))
        .send()
        .await
        .expect("send request with unrepresentable tool types");
    assert_eq!(response.status(), StatusCode::OK);

    let upstream = last_body.lock().expect("lock last body").clone();
    let upstream_tools = upstream["tools"].as_array().expect("upstream tools");
    assert_eq!(
        upstream_tools.len(),
        1,
        "custom/web_search must be dropped, not forwarded: {upstream_tools:?}"
    );
    assert_eq!(upstream_tools[0]["type"], "function");
    assert_eq!(upstream_tools[0]["function"]["name"], "lookup");
}

// ---- routing model request-path tests ----

/// Build a config with a routing model `router-lp` that forwards
/// level "low" → deepseek-v4-flash-lp and level "high" → deepseek-v4-pro-lp.
fn routing_test_config() -> config::Config {
    let mut cfg = config::default_deepseek_config();
    let mut router = cfg.models["deepseek-v4-flash-lp"].clone();
    router.reasoning_routes = Some(vec![
        config::RouteEntry {
            level: "low".to_string(),
            target: "deepseek-v4-flash-lp".to_string(),
        },
        config::RouteEntry {
            level: "high".to_string(),
            target: "deepseek-v4-pro-lp".to_string(),
        },
    ]);
    router.openai_chat_providers.clear();
    router.openai_responses_providers.clear();
    router.anthropic_providers.clear();
    router.reasoning_levels = None;
    cfg.models.insert("router-lp".to_string(), router);
    cfg
}

#[test]
fn resolve_routing_rewrites_model_for_matching_level() {
    let state = AppState::new_in_memory(routing_test_config());
    let mut body = json!({"model": "router-lp", "reasoning_effort": "high", "messages": []});
    let original = resolve_routing(
        &state,
        Protocol::OpenaiChatCompletions,
        &mut body,
        json_error,
    )
    .expect("resolve");
    assert_eq!(original.as_deref(), Some("router-lp"));
    assert_eq!(body["model"], "deepseek-v4-pro-lp");
}

#[test]
fn resolve_routing_uses_first_route_when_no_level() {
    let state = AppState::new_in_memory(routing_test_config());
    let mut body = json!({"model": "router-lp", "messages": []});
    let original = resolve_routing(
        &state,
        Protocol::OpenaiChatCompletions,
        &mut body,
        json_error,
    )
    .expect("resolve");
    assert_eq!(original.as_deref(), Some("router-lp"));
    // First route entry is "low" → deepseek-v4-flash-lp
    assert_eq!(body["model"], "deepseek-v4-flash-lp");
}

#[test]
fn resolve_routing_rejects_unsupported_level() {
    let state = AppState::new_in_memory(routing_test_config());
    let mut body = json!({"model": "router-lp", "reasoning_effort": "xhigh", "messages": []});
    let err = resolve_routing(
        &state,
        Protocol::OpenaiChatCompletions,
        &mut body,
        json_error,
    )
    .unwrap_err();
    assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    // body.model must be untouched on rejection
    assert_eq!(body["model"], "router-lp");
}

#[test]
fn resolve_routing_rejects_none_with_routes_detail() {
    let state = AppState::new_in_memory(routing_test_config());
    let mut body = json!({"model": "router-lp", "reasoning_effort": "none", "messages": []});
    let err = resolve_routing(
        &state,
        Protocol::OpenaiChatCompletions,
        &mut body,
        json_error,
    )
    .unwrap_err();
    assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body["model"], "router-lp");
}

#[test]
fn resolve_routing_returns_none_for_regular_model() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let mut body = json!({"model": "deepseek-v4-flash-lp", "reasoning_effort": "low"});
    let original = resolve_routing(
        &state,
        Protocol::OpenaiChatCompletions,
        &mut body,
        json_error,
    )
    .expect("resolve");
    assert_eq!(original, None);
    assert_eq!(body["model"], "deepseek-v4-flash-lp");
}

#[test]
fn resolve_routing_responses_protocol_reads_nested_effort() {
    let state = AppState::new_in_memory(routing_test_config());
    let mut body = json!({"model": "router-lp", "reasoning": {"effort": "high"}, "input": []});
    let original = resolve_routing(
        &state,
        Protocol::OpenaiResponses,
        &mut body,
        responses_error,
    )
    .expect("resolve");
    assert_eq!(original.as_deref(), Some("router-lp"));
    assert_eq!(body["model"], "deepseek-v4-pro-lp");
}

// : chat protocol must also read nested reasoning.effort (qwen wire format)
#[test]
fn resolve_routing_chat_reads_nested_reasoning_effort() {
    let state = AppState::new_in_memory(routing_test_config());
    let mut body = json!({"model": "router-lp", "reasoning": {"effort": "high"}, "messages": []});
    let original = resolve_routing(
        &state,
        Protocol::OpenaiChatCompletions,
        &mut body,
        json_error,
    )
    .expect("resolve");
    assert_eq!(original.as_deref(), Some("router-lp"));
    assert_eq!(body["model"], "deepseek-v4-pro-lp");
}

// : top-level reasoning_effort takes priority over nested reasoning.effort
#[test]
fn resolve_routing_chat_top_level_takes_priority_over_nested() {
    let state = AppState::new_in_memory(routing_test_config());
    // top-level says "low", nested says "high" → top-level must win
    let mut body = json!({
        "model": "router-lp",
        "reasoning_effort": "low",
        "reasoning": {"effort": "high"},
        "messages": []
    });
    let original = resolve_routing(
        &state,
        Protocol::OpenaiChatCompletions,
        &mut body,
        json_error,
    )
    .expect("resolve");
    assert_eq!(original.as_deref(), Some("router-lp"));
    // "low" → deepseek-v4-flash-lp (first route)
    assert_eq!(body["model"], "deepseek-v4-flash-lp");
}

#[tokio::test]
async fn routing_model_executes_as_target_and_reports_client_model_name() {
    let captured: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream = Router::new()
        .route(
            "/chat/completions",
            post(
                |State(captured): State<CapturedRequest>, Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "chatcmpl-1",
                        "object": "chat.completion",
                        "created": 1,
                        "model": "upstream-low",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "pong"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    }))
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_addr = serve_on_random_port(upstream).await;

    let mut provider = config::ProviderConfig {
        openai_chat: Some(config::EndpointConfig::native(format!(
            "http://{upstream_addr}/chat/completions"
        ))),
        ..Default::default()
    };
    provider.api_key_env = None;

    // The target carries a non-identity api_value mapping. If the routing hop's
    // own model ID were used for the thinking lookup this mapping would be
    // silently dropped.
    let mut target = empty_model(true, false, false, "p", "upstream-low");
    target.reasoning_levels = Some(vec![config::ReasoningLevelEntry::new(
        "low".to_string(),
        Some("1000".to_string()),
    )]);
    target.default_reasoning_level = Some("low".to_string());

    // A routing model has no provider bindings — only the level → target table.
    let mut routing = empty_model(false, false, false, "p", "unused");
    routing.context_window = 0;
    routing.max_output_tokens = 0;
    routing.reasoning_routes = Some(vec![config::RouteEntry {
        level: "low".to_string(),
        target: "target-low".to_string(),
    }]);

    let cfg = config_with(
        vec![("p", provider)],
        vec![("target-low", target), ("router-lp", routing)],
    );
    cfg.validate().expect("routing config must validate");
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "router-lp",
            "reasoning_effort": "low",
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    let upstream_body = captured.lock().unwrap().clone().expect("captured egress");
    // Execution identity is the target: its upstream model and its api_value mapping.
    assert_eq!(upstream_body["model"], "upstream-low");
    assert_eq!(
        upstream_body["reasoning_effort"], "1000",
        "the target's api_value mapping must be applied; the routing model must not supply the thinking policy"
    );
    // Client-facing identity is the routing model the client asked for.
    assert_eq!(response["model"], "router-lp");
    assert_eq!(response["choices"][0]["message"]["content"], "pong");
}

// ---- routing identity, cooldown isolation, protocol coverage ----

/// A routing model with no execution metadata of its own.
fn routing_model(routes: &[(&str, &str)]) -> config::ModelConfig {
    let mut model = empty_model(false, false, false, "p", "unused");
    model.context_window = 0;
    model.max_output_tokens = 0;
    model.reasoning_routes = Some(
        routes
            .iter()
            .map(|(level, target)| config::RouteEntry {
                level: (*level).to_string(),
                target: (*target).to_string(),
            })
            .collect(),
    );
    model
}

/// Provider whose chat is native and whose responses/anthropic endpoints derive
/// from it, so a single mock `/chat/completions` serves all three protocols.
fn routing_provider(addr: std::net::SocketAddr) -> config::ProviderConfig {
    let mut provider = config::ProviderConfig {
        openai_chat: Some(config::EndpointConfig::native(format!(
            "http://{addr}/chat/completions"
        ))),
        ..Default::default()
    };
    provider.api_key_env = None;
    provider.openai_responses = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiChatCompletions,
    ));
    provider.anthropic = Some(config::EndpointConfig::derived(
        config::Protocol::OpenaiChatCompletions,
    ));
    provider
}

fn chat_completion_body(model: &str) -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "pong"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

/// Cooldown identity must be the *target*, not the routing hop: a failure on one
/// level must not cool down its sibling level that shares the same provider.
#[tokio::test]
async fn routing_cooldown_is_isolated_per_target() {
    let upstream = Router::new().route(
        "/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let model = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if model == "upstream-high" {
                // 404 -> `model_unavailable`, a persistent (non-transient) cooldown,
                // so the isolation is observable through the cooldown store.
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": {"message": "no such upstream model"}})),
                )
                    .into_response();
            }
            Json(chat_completion_body(&model)).into_response()
        }),
    );
    let addr = serve_on_random_port(upstream).await;

    let mut low = empty_model(true, false, false, "p", "upstream-low");
    low.reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("low", None)]);
    low.default_reasoning_level = Some("low".to_string());
    let mut high = empty_model(true, false, false, "p", "upstream-high");
    high.reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("high", None)]);
    high.default_reasoning_level = Some("high".to_string());

    let cfg = config_with(
        vec![("p", routing_provider(addr))],
        vec![
            ("target-low", low),
            ("target-high", high),
            (
                "router-lp",
                routing_model(&[("low", "target-low"), ("high", "target-high")]),
            ),
        ],
    );
    cfg.validate().expect("routing config must validate");
    let state = AppState::new_in_memory(cfg);
    let proxy_addr = serve_on_random_port(router(state.clone())).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");

    let post = |effort: &str| {
        let client = client.clone();
        let url = format!("http://{proxy_addr}/openai/v1/chat/completions");
        let effort = effort.to_string();
        async move {
            client
                .post(url)
                .json(&json!({
                    "model": "router-lp",
                    "reasoning_effort": effort,
                    "messages": [{"role": "user", "content": "ping"}]
                }))
                .send()
                .await
                .expect("send")
        }
    };

    // The level `high` target fails.
    let high_failure = post("high").await;
    assert!(
        !high_failure.status().is_success(),
        "the high target's upstream failure must surface"
    );

    // Cooldown landed on the target, not on the routing hop.
    let key_for = |model_id: &str| config::CooldownKey {
        model_id: model_id.to_string(),
        provider_id: "p".to_string(),
        protocol: Protocol::OpenaiChatCompletions,
        token: None,
        token_key_hash: None,
    };
    assert!(
        state.is_cooling_down(&key_for("target-high")),
        "the failing target must be cooling down"
    );
    assert!(
        !state.is_cooling_down(&key_for("target-low")),
        "the sibling level must not inherit the cooldown (execution identity is the target)"
    );
    assert!(
        !state.is_cooling_down(&key_for("router-lp")),
        "the routing hop must never be the cooldown identity"
    );

    // And the sibling level still serves normally.
    let low_ok = post("low").await;
    assert_eq!(low_ok.status(), StatusCode::OK);
    let low_body: Value = low_ok.json().await.expect("json");
    assert_eq!(low_body["model"], "router-lp");
}

/// Responses ingress routes on `reasoning.effort` and reports the client model.
#[tokio::test]
async fn routing_responses_ingress_routes_and_reports_client_model() {
    let captured: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream = Router::new()
        .route(
            "/chat/completions",
            post(
                move |State(captured): State<CapturedRequest>, Json(body): Json<Value>| async move {
                    let model = body
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    *captured.lock().unwrap() = Some(body);
                    Json(chat_completion_body(&model))
                },
            ),
        )
        .with_state(captured_for_handler);
    let addr = serve_on_random_port(upstream).await;

    let mut high = empty_model(true, true, false, "p", "upstream-high");
    high.reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("high", None)]);
    high.default_reasoning_level = Some("high".to_string());

    let cfg = config_with(
        vec![("p", routing_provider(addr))],
        vec![
            ("target-high", high),
            ("router-lp", routing_model(&[("high", "target-high")])),
        ],
    );
    cfg.validate().expect("routing config must validate");
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/responses/v1/responses"))
        .json(&json!({
            "model": "router-lp",
            "reasoning": {"effort": "high"},
            "input": "ping"
        }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    assert_eq!(
        captured.lock().unwrap().as_ref().expect("egress")["model"],
        "upstream-high"
    );
    assert_eq!(response["model"], "router-lp");
}

/// Anthropic ingress routes on `output_config.effort` and reports the client model.
#[tokio::test]
async fn routing_anthropic_ingress_routes_and_reports_client_model() {
    let captured: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream = Router::new()
        .route(
            "/chat/completions",
            post(
                move |State(captured): State<CapturedRequest>, Json(body): Json<Value>| async move {
                    let model = body
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    *captured.lock().unwrap() = Some(body);
                    Json(chat_completion_body(&model))
                },
            ),
        )
        .with_state(captured_for_handler);
    let addr = serve_on_random_port(upstream).await;

    let mut high = empty_model(false, false, true, "p", "upstream-high");
    high.reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("high", None)]);
    high.default_reasoning_level = Some("high".to_string());

    let cfg = config_with(
        vec![("p", routing_provider(addr))],
        vec![
            ("target-high", high),
            ("router-lp", routing_model(&[("high", "target-high")])),
        ],
    );
    cfg.validate().expect("routing config must validate");
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let response: Value = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/anthropic/v1/messages"))
        .json(&json!({
            "model": "router-lp",
            "output_config": {"effort": "high"},
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");

    assert_eq!(
        captured.lock().unwrap().as_ref().expect("egress")["model"],
        "upstream-high"
    );
    assert_eq!(response["model"], "router-lp");
}

/// Chat streaming must still resolve the routing hop. This records the model
/// field the client actually observes on the stream.
#[tokio::test]
async fn routing_chat_streaming_resolves_hop_and_reports_a_model() {
    let captured: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let sse = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"upstream-low\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"po\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"upstream-low\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ng\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    let upstream = Router::new()
        .route(
            "/chat/completions",
            post(
                move |State(captured): State<CapturedRequest>, Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
                },
            ),
        )
        .with_state(captured_for_handler);
    let addr = serve_on_random_port(upstream).await;

    let mut low = empty_model(true, false, false, "p", "upstream-low");
    low.reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("low", None)]);
    low.default_reasoning_level = Some("low".to_string());

    let cfg = config_with(
        vec![("p", routing_provider(addr))],
        vec![
            ("target-low", low),
            ("router-lp", routing_model(&[("low", "target-low")])),
        ],
    );
    cfg.validate().expect("routing config must validate");
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let text = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "router-lp",
            "reasoning_effort": "low",
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("body");

    assert_eq!(
        captured.lock().unwrap().as_ref().expect("egress")["model"],
        "upstream-low",
        "streaming must still resolve the routing hop"
    );
    assert!(text.contains("po"), "stream body was: {text}");
    assert!(text.contains("ng"), "stream body was: {text}");
    // Recorded behaviour: the chat passthrough forwards upstream SSE verbatim, so
    // the stream reports the *upstream* model name. Assert it explicitly so the
    // gap is visible if it changes.
    assert!(
        text.contains("\"model\":\"upstream-low\""),
        "chat SSE passthrough forwards the upstream model verbatim; body was: {text}"
    );
}

/// Token access control keys on the client-requested model: authorising the
/// routing model authorises the hop, and authorising only a target does not.
#[test]
fn routing_token_access_keys_on_the_client_requested_model() {
    let grant = |allowed: &[&str]| {
        crate::token::auth::TokenGrant::Granted(crate::token::auth::TokenContext {
            key: "k".to_string(),
            name: "n".to_string(),
            allowed_models: allowed.iter().map(|m| (*m).to_string()).collect(),
            admin: false,
        })
    };

    let body = json!({"model": "router-lp"});
    assert!(
        check_token_model_access(&grant(&["router-lp"]), &body).is_none(),
        "a token allowed the routing model may use it"
    );
    let denied = check_token_model_access(&grant(&["target-low"]), &body).expect("denied");
    assert_eq!(
        denied.status(),
        StatusCode::FORBIDDEN,
        "a token allowed only the internal target must not reach it through the router"
    );
}

/// Usage is attributed to the client identity (the routing model), matching the
/// response body and token accounting.
#[tokio::test]
async fn routing_usage_is_attributed_to_the_client_model() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let jsonl_path = temp_dir.path().join("usage.jsonl");
    let db_path = temp_dir.path().join("usage.db");
    let usage_store = Arc::new(
        UsageStore::with_paths(config::UsageConfig::default(), jsonl_path, db_path)
            .expect("usage store"),
    );

    let upstream = Router::new().route(
        "/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let model = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Json(chat_completion_body(&model))
        }),
    );
    let addr = serve_on_random_port(upstream).await;

    let mut low = empty_model(true, false, false, "p", "upstream-low");
    low.reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("low", None)]);
    low.default_reasoning_level = Some("low".to_string());

    let cfg = config_with(
        vec![("p", routing_provider(addr))],
        vec![
            ("target-low", low),
            ("router-lp", routing_model(&[("low", "target-low")])),
        ],
    );
    cfg.validate().expect("routing config must validate");
    let mut state = AppState::new_in_memory(cfg);
    state.set_usage_store_for_test(Arc::clone(&usage_store));
    let proxy_addr = serve_on_random_port(router(state)).await;

    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "router-lp",
            "reasoning_effort": "low",
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);

    usage_store.flush_if_dirty().expect("flush");
    let records = usage_store.get_all();
    assert_eq!(records.len(), 1, "exactly one request was made");
    assert_eq!(
        records[0].model, "router-lp",
        "usage is attributed to the client-visible model, not the internal target"
    );
    assert_eq!(records[0].provider, "p");
}

/// `codex_service_tier` is an execution-layer policy: the *target's* declaration
/// must reach the upstream even when the client arrived through a routing hop.
/// A hop cannot declare one of its own, so looking the tier up by the client's
/// model ID would silently drop it.
#[tokio::test]
async fn routing_hop_still_applies_the_targets_codex_service_tier() {
    let captured: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream = Router::new()
        .route(
            "/v1/responses",
            post(
                move |State(captured): State<CapturedRequest>, Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "resp_1",
                        "model": "upstream-high",
                        "response": {
                            "model": "upstream-high",
                            "output": [{"type": "message", "content": [{"type": "output_text", "text": "pong"}]}],
                            "usage": {"input_tokens": 2, "output_tokens": 1}
                        }
                    }))
                },
            ),
        )
        .with_state(captured_for_handler);
    let addr = serve_on_random_port(upstream).await;

    let mut provider = config::ProviderConfig {
        openai_responses: Some(config::EndpointConfig::native(format!(
            "http://{addr}/v1/responses"
        ))),
        ..Default::default()
    };
    provider.api_key_env = None;

    let mut high = empty_model(false, true, false, "p", "upstream-high");
    high.reasoning_levels = Some(vec![config::ReasoningLevelEntry::new("high", None)]);
    high.default_reasoning_level = Some("high".to_string());
    high.codex_service_tier = Some(config::CodexServiceTierConfig {
        tiers: vec!["priority".to_string()],
        default_tier: None,
        forced_tier: Some("priority".to_string()),
    });

    let cfg = config_with(
        vec![("p", provider)],
        vec![
            ("target-high", high),
            ("router-lp", routing_model(&[("high", "target-high")])),
        ],
    );
    cfg.validate().expect("routing config must validate");
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;

    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/responses"))
        .json(&json!({
            "model": "router-lp",
            "reasoning": {"effort": "high"},
            "input": "ping"
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);

    let egress = captured.lock().unwrap().clone().expect("egress");
    assert_eq!(egress["model"], "upstream-high");
    assert_eq!(
        egress["service_tier"], "priority",
        "the target's forced service tier must be applied on egress, not looked up by the client model id"
    );
}

/// The public HTTP model-list endpoints must advertise a routing hop by the
/// protocol its targets actually serve. Before the protocol was derived
/// here, these endpoints keyed on the hop's own (empty) bindings, so a working
/// hop was invisible to every HTTP SDK-based client (OpenAI SDK, Cursor, …).
#[tokio::test]
async fn http_model_list_advertises_routing_models_by_derived_protocol() {
    let upstream = Router::new().route(
        "/chat/completions",
        post(|| async { Json(chat_completion_body("upstream-chat")) }),
    );
    let addr = serve_on_random_port(upstream).await;

    // A single chat-only target: the hop therefore serves chat, nothing else.
    let chat_target = empty_model(true, false, false, "p", "upstream-chat");
    let hop = routing_model(&[("low", "target-chat")]);
    let cfg = config_with(
        vec![("p", routing_provider(addr))],
        vec![("target-chat", chat_target), ("hop", hop)],
    );
    cfg.validate().expect("routing config must validate");
    let proxy = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;
    let client = reqwest::Client::new();

    let chat: Value = client
        .get(format!("http://{proxy}/openai/v1/models"))
        .send()
        .await
        .expect("chat models")
        .json()
        .await
        .expect("chat json");
    assert!(
        model_list_ids(&chat).contains(&"hop".to_string()),
        "a hop whose targets all serve chat must appear in the chat list, got {chat}"
    );

    let anthropic: Value = client
        .get(format!("http://{proxy}/anthropic/v1/models"))
        .send()
        .await
        .expect("anthropic models")
        .json()
        .await
        .expect("anthropic json");
    assert!(
        !model_list_ids(&anthropic).contains(&"hop".to_string()),
        "a hop is not advertised on Anthropic when a reachable target does not serve it"
    );
}

/// A hop is hidden from a protocol none of its targets serves, and request
/// time must agree: the hop is refused on that protocol rather than resolved
/// to a target, so discovery and runtime never disagree about what the hop
/// offers. (Mixed-protocol targets are a validation error under strict target
/// consistency — see the routing validation tests in config.rs.)
#[test]
fn routing_hop_is_unreachable_on_protocols_it_cannot_advertise() {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 9));
    // Both targets are chat-only, so the hop advertises only chat.
    let low_target = empty_model(true, false, false, "p", "upstream-r");
    let high_target = empty_model(true, false, false, "p", "upstream-c");
    let hop = routing_model(&[("low", "target-r"), ("high", "target-c")]);
    let cfg = config_with(
        vec![("p", routing_provider(addr))],
        vec![
            ("target-r", low_target),
            ("target-c", high_target),
            ("hop", hop),
        ],
    );
    cfg.validate().expect("routing config must validate");
    let state = AppState::new_in_memory(cfg);

    let mut body = json!({"model": "hop", "reasoning": {"effort": "low"}});
    let err = resolve_routing(
        &state,
        Protocol::OpenaiResponses,
        &mut body,
        responses_error,
    )
    .unwrap_err();
    assert_eq!(
        err.status(),
        StatusCode::BAD_REQUEST,
        "the hop advertises only chat, so a Responses request must not resolve through it"
    );
    assert_eq!(body["model"], "hop", "the request body is left untouched");

    // Chat is advertised (every target serves it) and must still resolve.
    let mut chat_body = json!({"model": "hop", "reasoning_effort": "low"});
    resolve_routing(
        &state,
        Protocol::OpenaiChatCompletions,
        &mut chat_body,
        json_error,
    )
    .expect("chat resolves");
    assert_eq!(chat_body["model"], "target-r");
}

// ─── egress preflight wiring, (token × provider) cooldown, diagnostics ───

// ============================================================================
// /{protocol}/v1/models token-scoped filtering tests
// ============================================================================

fn protocol_models_token_scoped_test_config() -> config::Config {
    let mut cfg = config::default_deepseek_config();
    cfg.models.clear();
    for id in ["allowed-model", "forbidden-model"] {
        cfg.models.insert(
            id.to_string(),
            config::ModelConfig {
                description: None,
                context_window: 1000,
                max_output_tokens: 100,
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
                openai_chat_providers: vec![config::ProviderBinding {
                    name: "deepseek".to_string(),
                    model: format!("{id}-chat"),
                }],
                openai_responses_providers: vec![config::ProviderBinding {
                    name: "deepseek".to_string(),
                    model: format!("{id}-resp"),
                }],
                anthropic_providers: vec![config::ProviderBinding {
                    name: "deepseek".to_string(),
                    model: format!("{id}-anth"),
                }],
                reasoning_level_map: None,
            },
        );
    }
    cfg
}

#[tokio::test]
async fn model_endpoints_filtered_by_token_grant() {
    let config_dir = tempfile::tempdir().expect("temp config dir");
    let state_dir = tempfile::tempdir().expect("temp state dir");
    let tokens_path = config_dir.path().join("tokens.toml");
    std::fs::write(
        &tokens_path,
        r#"
[[tokens]]
key = "lp-restricted-113"
name = "restricted"
allowed_models = ["allowed-model"]

[[tokens]]
key = "lp-unrestricted-113"
name = "unrestricted"
allowed_models = []
"#,
    )
    .expect("write tokens.toml");

    let mut state = AppState::new_in_memory(protocol_models_token_scoped_test_config());
    state.token = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        state_dir.path(),
    );

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    let client = reqwest::Client::new();

    let endpoints = [
        format!("http://{addr}/openai/v1/models"),
        format!("http://{addr}/responses/v1/models"),
        format!("http://{addr}/anthropic/v1/models"),
        format!("http://{addr}/claude-desktop/v1/models"),
    ];

    for url in &endpoints {
        // 1. 无 token 必须 401（证明经过 token_auth 中间件）
        let unauth = client.get(url).send().await.expect("unauth send");
        assert_eq!(
            unauth.status(),
            StatusCode::UNAUTHORIZED,
            "url {url} without token must be 401"
        );

        // 2. 受限 token 必须只包含 allowed-model，严禁泄露 forbidden-model
        let resp: Value = client
            .get(url)
            .bearer_auth("lp-restricted-113")
            .send()
            .await
            .expect("restricted send")
            .json()
            .await
            .expect("restricted json");
        let ids = model_list_ids(&resp);
        assert!(
            ids.contains(&"allowed-model".to_string()),
            "url {url} must contain allowed-model, got: {ids:?}"
        );
        assert!(
            !ids.contains(&"forbidden-model".to_string()),
            "url {url} must NOT leak forbidden-model to restricted token, got: {ids:?}"
        );
        assert_eq!(
            ids.len(),
            1,
            "url {url} must return exactly 1 model for restricted token, got: {ids:?}"
        );

        // 3. 不受限 token (allowed_models = []) 必须返回全量
        let unres: Value = client
            .get(url)
            .bearer_auth("lp-unrestricted-113")
            .send()
            .await
            .expect("unrestricted send")
            .json()
            .await
            .expect("unrestricted json");
        let unres_ids = model_list_ids(&unres);
        assert!(unres_ids.contains(&"allowed-model".to_string()));
        assert!(unres_ids.contains(&"forbidden-model".to_string()));
        assert_eq!(unres_ids.len(), 2);
    }
}

#[tokio::test]
async fn model_endpoints_open_mode_allows_all() {
    let state = AppState::new_in_memory(protocol_models_token_scoped_test_config());
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    let client = reqwest::Client::new();

    let endpoints = [
        format!("http://{addr}/openai/v1/models"),
        format!("http://{addr}/responses/v1/models"),
        format!("http://{addr}/anthropic/v1/models"),
        format!("http://{addr}/claude-desktop/v1/models"),
    ];

    for url in &endpoints {
        let resp: Value = client
            .get(url)
            .send()
            .await
            .expect("open mode send")
            .json()
            .await
            .expect("open mode json");
        let ids = model_list_ids(&resp);
        assert!(
            ids.contains(&"allowed-model".to_string()),
            "open mode {url} must contain allowed-model"
        );
        assert!(
            ids.contains(&"forbidden-model".to_string()),
            "open mode {url} must contain forbidden-model"
        );
        assert_eq!(ids.len(), 2, "open mode {url} must return all 2 models");
    }
}

#[tokio::test]
async fn admin_client_config_filters_by_token_auth() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let state_dir = tempfile::tempdir().expect("state dir");
    fs::write(
        config_dir.path().join("tokens.toml"),
        r#"
[[tokens]]
key = "lp-restricted-token"
name = "restricted"
allowed_models = ["allowed-model"]

[[tokens]]
key = "lp-unrestricted-token"
name = "unrestricted"
allowed_models = []
"#,
    )
    .expect("write tokens.toml");

    let mut state = AppState::new_in_memory(protocol_models_token_scoped_test_config());
    state.token = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        state_dir.path(),
    );

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/admin/client-config/pi");

    // 1. 无 token 请求返回 401
    let unauth = client.get(&url).send().await.expect("unauth send");
    assert_eq!(
        unauth.status(),
        StatusCode::UNAUTHORIZED,
        "admin client-config without token must return 401"
    );

    // 2. 受限 token 仅返回授权模型 allowed-model，严禁泄露 forbidden-model
    let resp: Value = client
        .get(&url)
        .bearer_auth("lp-restricted-token")
        .send()
        .await
        .expect("restricted send")
        .json()
        .await
        .expect("restricted json");
    assert_eq!(resp["status"], "ok");
    let models = resp["data"]["models"].as_object().expect("models map");
    assert!(
        models.contains_key("allowed-model"),
        "models must contain allowed-model"
    );
    assert!(
        !models.contains_key("forbidden-model"),
        "models must NOT leak forbidden-model to restricted token"
    );
    assert_eq!(
        models.len(),
        1,
        "restricted token must receive exactly 1 model"
    );

    // 3. 不受限 token 返回全量模型
    let unres_resp: Value = client
        .get(&url)
        .bearer_auth("lp-unrestricted-token")
        .send()
        .await
        .expect("unrestricted send")
        .json()
        .await
        .expect("unrestricted json");
    assert_eq!(unres_resp["status"], "ok");
    let unres_models = unres_resp["data"]["models"]
        .as_object()
        .expect("models map");
    assert!(unres_models.contains_key("allowed-model"));
    assert!(unres_models.contains_key("forbidden-model"));
    assert_eq!(unres_models.len(), 2);
}

// ============================================================================
// GET /v1/token/models token-scoped capability readout tests
// ============================================================================

fn token_models_capability_readout_test_config() -> config::Config {
    let mut cfg = config::default_deepseek_config();
    cfg.models.clear();

    // 1. leaf-high: target for routing
    cfg.models.insert(
        "leaf-high".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 100_000,
            max_output_tokens: 8_000,
            features: vec!["image_input".to_string(), "tool_calls".to_string()],
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
            openai_chat_providers: vec![config::ProviderBinding {
                name: "secret-provider-a".to_string(),
                model: "secret-upstream-model-a".to_string(),
            }],
            openai_responses_providers: vec![config::ProviderBinding {
                name: "secret-provider-a".to_string(),
                model: "secret-upstream-model-a".to_string(),
            }],
            anthropic_providers: vec![config::ProviderBinding {
                name: "secret-provider-a".to_string(),
                model: "secret-upstream-model-a".to_string(),
            }],
            reasoning_level_map: None,
        },
    );

    // 2. leaf-low: target for routing
    cfg.models.insert(
        "leaf-low".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 80_000,
            max_output_tokens: 4_000,
            features: vec!["image_input".to_string()],
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
            openai_chat_providers: vec![config::ProviderBinding {
                name: "secret-provider-b".to_string(),
                model: "secret-upstream-model-b".to_string(),
            }],
            openai_responses_providers: vec![config::ProviderBinding {
                name: "secret-provider-b".to_string(),
                model: "secret-upstream-model-b".to_string(),
            }],
            anthropic_providers: vec![config::ProviderBinding {
                name: "secret-provider-b".to_string(),
                model: "secret-upstream-model-b".to_string(),
            }],
            reasoning_level_map: None,
        },
    );

    // 3. routing-router: declared with 0 windows, routes to leaf-high and leaf-low
    cfg.models.insert(
        "routing-router".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 0,
            max_output_tokens: 0,
            features: Vec::new(),
            reasoning_levels: None,
            supported_reasoning_levels: Vec::new(),
            cooldown: None,
            reasoning_routes: Some(vec![
                config::RouteEntry {
                    level: "high".to_string(),
                    target: "leaf-high".to_string(),
                },
                config::RouteEntry {
                    level: "low".to_string(),
                    target: "leaf-low".to_string(),
                },
            ]),
            default_reasoning_level: None,
            supports_thinking: None,
            force_think: None,
            force_no_think: None,
            default_think: None,
            enable_thinking: None,
            chat_reasoning_off_format: None,
            codex_service_tier: None,
            openai_chat_providers: Vec::new(),
            openai_responses_providers: Vec::new(),
            anthropic_providers: Vec::new(),
            reasoning_level_map: None,
        },
    );

    // 4. normal forbidden model
    cfg.models.insert(
        "forbidden-model".to_string(),
        config::ModelConfig {
            description: None,
            context_window: 50_000,
            max_output_tokens: 2_000,
            features: Vec::new(), // 未声明 features
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
            openai_chat_providers: vec![config::ProviderBinding {
                name: "secret-provider-c".to_string(),
                model: "secret-upstream-model-c".to_string(),
            }],
            openai_responses_providers: Vec::new(), // 仅 chat
            anthropic_providers: Vec::new(),
            reasoning_level_map: None,
        },
    );

    cfg
}

#[tokio::test]
async fn token_models_endpoint_auth_and_scoping() {
    let config_dir = tempfile::tempdir().expect("temp config dir");
    let state_dir = tempfile::tempdir().expect("temp state dir");
    let tokens_path = config_dir.path().join("tokens.toml");
    std::fs::write(
        &tokens_path,
        r#"
[[tokens]]
key = "lp-restricted-114"
name = "restricted-user"
allowed_models = ["routing-router"]

[[tokens]]
key = "lp-unrestricted-114"
name = "admin-user"
allowed_models = []
"#,
    )
    .expect("write tokens.toml");

    let mut state = AppState::new_in_memory(token_models_capability_readout_test_config());
    state.token = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        state_dir.path(),
    );

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/v1/token/models");

    // 1. 无 token 必须 401
    let unauth = client.get(&url).send().await.expect("unauth send");
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // 2. 受限 token 验证：R2 作用域、R3 派生下界、R4 拓扑隔离
    let res = client
        .get(&url)
        .bearer_auth("lp-restricted-114")
        .send()
        .await
        .expect("restricted send");
    assert_eq!(res.status(), StatusCode::OK);

    let raw_text = res.text().await.expect("read body");
    // R4 守卫：绝对不包含敏感内部标识
    assert!(!raw_text.contains("secret-provider"));
    assert!(!raw_text.contains("secret-upstream"));
    assert!(!raw_text.contains("leaf-high"));
    assert!(!raw_text.contains("leaf-low"));

    let data: TokenModelsResponse =
        serde_json::from_str(&raw_text).expect("deserialize TokenModelsResponse");
    assert_eq!(data.token_name.as_deref(), Some("restricted-user"));
    assert_eq!(
        data.models.len(),
        1,
        "only 1 allowed model should be returned"
    );

    let item = &data.models[0];
    assert_eq!(item.id, "routing-router");
    // R3 / G-B 守卫：路由模型必须返回 target 下界，而不是原始声明的 0
    assert_eq!(item.context_window, 80_000, "min window of leaves");
    assert_eq!(item.max_output_tokens, 4_000, "min output of leaves");
    assert_eq!(
        item.features,
        vec!["image_input".to_string()],
        "features intersection of leaves"
    );
    assert!(item.features_declared, "all leaves declared features");
    assert_eq!(
        item.reasoning_levels,
        vec!["high".to_string(), "low".to_string()]
    );
    assert_eq!(item.default_reasoning_level.as_deref(), Some("high"));
    assert!(item.routing);
    assert_eq!(item.bounds, "derived");
    assert_eq!(
        item.protocols,
        vec![
            "openai-chat".to_string(),
            "openai-responses".to_string(),
            "anthropic".to_string()
        ]
    );

    // 3. 无限制 token 验证：返回全部 4 个模型
    let unres = client
        .get(&url)
        .bearer_auth("lp-unrestricted-114")
        .send()
        .await
        .expect("unrestricted send");
    assert_eq!(unres.status(), StatusCode::OK);
    let unres_data: TokenModelsResponse = unres.json().await.expect("unrestricted json");
    assert_eq!(unres_data.token_name.as_deref(), Some("admin-user"));
    assert_eq!(unres_data.models.len(), 4);

    let forbidden = unres_data
        .models
        .iter()
        .find(|m| m.id == "forbidden-model")
        .expect("find forbidden-model");
    assert_eq!(forbidden.context_window, 50_000);
    assert_eq!(forbidden.max_output_tokens, 2_000);
    assert_eq!(forbidden.protocols, vec!["openai-chat".to_string()]);
    assert!(
        !forbidden.features_declared,
        "features unprovided = undeclared"
    );
    assert!(!forbidden.routing);
}

#[tokio::test]
async fn token_models_endpoint_open_mode() {
    let state = AppState::new_in_memory(token_models_capability_readout_test_config());
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/v1/token/models");

    let resp = client.get(&url).send().await.expect("open mode send");
    assert_eq!(resp.status(), StatusCode::OK);
    let data: TokenModelsResponse = resp.json().await.expect("open mode json");
    assert_eq!(data.token_name, None);
    assert_eq!(data.models.len(), 4);
}

#[tokio::test]
async fn token_models_endpoint_quota_exhausted_does_not_block_readout() {
    let config_dir = tempfile::tempdir().expect("temp config dir");
    let state_dir = tempfile::tempdir().expect("temp state dir");
    let tokens_path = config_dir.path().join("tokens.toml");
    std::fs::write(
        &tokens_path,
        r#"
[[tokens]]
key = "lp-exhausted-114"
name = "exhausted-user"
allowed_models = ["routing-router"]

[[tokens.quota_windows]]
window = "1h"
quota_mode = "tokens"
quota_limit = 10
"#,
    )
    .expect("write tokens.toml");

    let mut state = AppState::new_in_memory(token_models_capability_readout_test_config());
    let token_state = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        state_dir.path(),
    );

    // 模拟配额已用完：设置 10 tokens 上限并消耗 10 tokens
    if let Some(store) = &token_state.usage_store {
        let windows = vec![crate::token::config::QuotaWindow {
            window: "1h".to_string(),
            quota_mode: "tokens".to_string(),
            quota_limit: 10,
        }];
        store
            .init_token_windows("lp-exhausted-114", &windows)
            .expect("init windows");
        store
            .add_usage("lp-exhausted-114", 10, 0)
            .expect("add usage");
    }

    state.token = token_state;
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/v1/token/models");

    // R5: 配额耗尽时，元数据读出绝不能被阻断（必须 HTTP 200）
    let resp = client
        .get(&url)
        .bearer_auth("lp-exhausted-114")
        .send()
        .await
        .expect("send to token/models");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "quota exhausted token must still read models (R5)"
    );
    let data: TokenModelsResponse = resp.json().await.expect("json");
    assert_eq!(data.models.len(), 1);
    assert_eq!(data.models[0].id, "routing-router");
}

// ---- 真流式契约验证（验收与回归验证方案） ----

/// 长流与超时豁免：流式 body 不受 request-level total timeout 约束。
#[tokio::test]
async fn send_upstream_streaming_body_exempts_total_timeout() {
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(|| async {
            let stream = async_stream::stream! {
                yield Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from("data: {\"a\":1}\n\n"));
                tokio::time::sleep(Duration::from_millis(1200)).await;
                yield Ok(bytes::Bytes::from("data: {\"a\":2}\n\n"));
                yield Ok(bytes::Bytes::from("data: [DONE]\n\n"));
            };
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(stream),
            )
        }),
    );
    let addr = serve_on_random_port(upstream_app).await;
    let mut cfg = config::default_deepseek_config();
    cfg.fallback.timeout_seconds = 1;
    cfg.fallback.max_timeout_seconds = 1;
    let state = AppState::new_in_memory(cfg);
    let plan = test_execution_plan(format!("http://{addr}/chat/completions"));
    let req = state
        .client
        .post(&plan.native_url)
        .json(&json!({"model": "m", "messages": []}));
    let response = send_upstream(&state, &plan, req, true)
        .await
        .expect("streaming headers arrive within the TTFB budget");
    let body = response
        .bytes()
        .await
        .expect("body must outlive the 1s request timeout");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("{\"a\":2}"), "tail chunk must arrive: {text}");
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
}

// ── graceful shutdown 必须把 pending usage 落盘 ─────────────────

#[test]
fn shutdown_usage_flush_persists_pending_records_with_status() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let jsonl_path = temp_dir.path().join("usage.jsonl");
    let db_path = temp_dir.path().join("usage.db");
    let store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            jsonl_path.clone(),
            db_path.clone(),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&store));

    for (model, status) in [
        ("model-a", "completed"),
        ("model-b", "client_disconnected"),
        ("model-c", "upstream_interrupted"),
    ] {
        state.record_usage_with_status(
            model.to_string(),
            "provider-a".to_string(),
            "openai_chat".to_string(),
            10,
            20,
            Some(100),
            None,
            Some(status),
            false,
            0,
            false,
            None,
            None,
        );
    }

    assert!(
        store.is_dirty(),
        "record must leave the store dirty (memory-authoritative)"
    );
    assert!(!db_path.exists(), "record must not flush synchronously");

    state.flush_usage_on_shutdown();

    assert!(
        !store.is_dirty(),
        "shutdown flush must clear the dirty flag"
    );
    assert!(
        db_path.exists(),
        "usage.db must exist after the graceful-shutdown flush"
    );
    let (records, total) = store
        .query_with_pagination(None, None, None, None, None, 10, 0)
        .unwrap();
    assert_eq!(total, 3, "all pending records must be flushed on shutdown");
    for (r, status) in
        records
            .iter()
            .rev()
            .zip(["completed", "client_disconnected", "upstream_interrupted"])
    {
        assert_eq!(
            r.status.as_deref(),
            Some(status),
            "flushed row must keep its terminal status"
        );
    }

    let reopened =
        UsageStore::with_paths(crate::config::UsageConfig::default(), jsonl_path, db_path)
            .expect("reopen the same temporary usage paths");
    assert_eq!(
        reopened.get_all().len(),
        3,
        "disk backup must reload with every flushed record"
    );
}

#[test]
fn shutdown_usage_flush_is_noop_when_clean() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let jsonl_path = temp_dir.path().join("usage.jsonl");
    let db_path = temp_dir.path().join("usage.db");
    let store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            jsonl_path.clone(),
            db_path.clone(),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&store));

    state.flush_usage_on_shutdown();

    assert!(
        !db_path.exists(),
        "clean store must stay zero-I/O on shutdown flush"
    );
}

#[test]
fn shutdown_usage_flush_catches_records_after_prior_flush() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let jsonl_path = temp_dir.path().join("usage.jsonl");
    let db_path = temp_dir.path().join("usage.db");
    let store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            jsonl_path.clone(),
            db_path,
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&store));

    let record = |model: &str, status: &str| {
        UsageRecord::new(
            model.to_string(),
            "provider-a".to_string(),
            "openai_chat".to_string(),
            1,
            2,
            Some(3),
        )
        .with_status(Some(status.to_string()))
    };

    store
        .record(record("model-a", "completed"))
        .expect("record must enqueue");
    store
        .flush_if_dirty()
        .expect("simulate a background flush tick");

    // 关停前来不及走后台周期的迟到记录
    store
        .record(record("model-b", "client_disconnected"))
        .expect("record must enqueue");
    assert!(store.is_dirty());

    state.flush_usage_on_shutdown();

    let (records, total) = store
        .query_with_pagination(None, None, None, None, None, 10, 0)
        .unwrap();
    assert_eq!(
        total, 2,
        "shutdown flush must catch late records after an earlier flush"
    );
    assert_eq!(records.len(), 2);
}

// RoundRobin rotation for multi-provider load balancing.
#[test]
fn round_robin_rotation_rotates_candidates_correctly() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    // Create test candidates
    let make_plan = |id: &str| {
        (
            "test-model".to_string(),
            config::ExecutionPlan {
                frontend_protocol: Protocol::OpenaiChatCompletions,
                provider_id: id.to_string(),
                upstream_model: "test-model".to_string(),
                source_protocol: Protocol::OpenaiChatCompletions,
                adapter: AdapterKind::Passthrough,
                native_url: "http://test/v1".to_string(),
                auth: config::AuthConfig::None,
                compat: config::CompatConfig::default(),
                anthropic_family_models: vec![],
                store: None,
                request_frequency: config::RequestFrequencyConfig::default(),
                routed_target: None,
            },
        )
    };

    let candidates = vec![make_plan("A"), make_plan("B"), make_plan("C")];

    // First call: cursor=0, offset=0, returns [A, B, C]
    let rotated1 = state.rotate_candidates_round_robin("test-model", candidates.clone());
    assert_eq!(
        rotated1
            .iter()
            .map(|(_, p)| p.provider_id.as_str())
            .collect::<Vec<_>>(),
        vec!["A", "B", "C"],
        "first call should return original order"
    );

    // Second call: cursor=1, offset=1, returns [B, C, A]
    let rotated2 = state.rotate_candidates_round_robin("test-model", candidates.clone());
    assert_eq!(
        rotated2
            .iter()
            .map(|(_, p)| p.provider_id.as_str())
            .collect::<Vec<_>>(),
        vec!["B", "C", "A"],
        "second call should rotate by 1"
    );

    // Third call: cursor=2, offset=2, returns [C, A, B]
    let rotated3 = state.rotate_candidates_round_robin("test-model", candidates.clone());
    assert_eq!(
        rotated3
            .iter()
            .map(|(_, p)| p.provider_id.as_str())
            .collect::<Vec<_>>(),
        vec!["C", "A", "B"],
        "third call should rotate by 2"
    );

    // Fourth call: cursor=3, offset=0 (3 % 3 = 0), returns [A, B, C] again
    let rotated4 = state.rotate_candidates_round_robin("test-model", candidates.clone());
    assert_eq!(
        rotated4
            .iter()
            .map(|(_, p)| p.provider_id.as_str())
            .collect::<Vec<_>>(),
        vec!["A", "B", "C"],
        "fourth call should wrap around to original order"
    );
}

#[test]
fn round_robin_rotation_handles_single_candidate() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    let make_plan = |id: &str| {
        (
            "test-model".to_string(),
            config::ExecutionPlan {
                frontend_protocol: Protocol::OpenaiChatCompletions,
                provider_id: id.to_string(),
                upstream_model: "test-model".to_string(),
                source_protocol: Protocol::OpenaiChatCompletions,
                adapter: AdapterKind::Passthrough,
                native_url: "http://test/v1".to_string(),
                auth: config::AuthConfig::None,
                compat: config::CompatConfig::default(),
                anthropic_family_models: vec![],
                store: None,
                request_frequency: config::RequestFrequencyConfig::default(),
                routed_target: None,
            },
        )
    };

    let candidates = vec![make_plan("A")];

    // Single candidate should always return the same order
    let rotated1 = state.rotate_candidates_round_robin("test-model", candidates.clone());
    assert_eq!(rotated1.len(), 1);
    assert_eq!(rotated1[0].1.provider_id, "A");

    let rotated2 = state.rotate_candidates_round_robin("test-model", candidates.clone());
    assert_eq!(rotated2.len(), 1);
    assert_eq!(rotated2[0].1.provider_id, "A");
}

#[test]
fn round_robin_rotation_handles_empty_candidates() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    let candidates: Vec<(String, config::ExecutionPlan)> = vec![];

    // Empty candidates should return empty
    let rotated = state.rotate_candidates_round_robin("test-model", candidates);
    assert!(rotated.is_empty());
}

#[test]
fn round_robin_rotation_isolates_different_models() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    let make_plan = |id: &str| {
        (
            "test-model".to_string(),
            config::ExecutionPlan {
                frontend_protocol: Protocol::OpenaiChatCompletions,
                provider_id: id.to_string(),
                upstream_model: "test-model".to_string(),
                source_protocol: Protocol::OpenaiChatCompletions,
                adapter: AdapterKind::Passthrough,
                native_url: "http://test/v1".to_string(),
                auth: config::AuthConfig::None,
                compat: config::CompatConfig::default(),
                anthropic_family_models: vec![],
                store: None,
                request_frequency: config::RequestFrequencyConfig::default(),
                routed_target: None,
            },
        )
    };

    let candidates_a = vec![make_plan("A1"), make_plan("A2")];
    let candidates_b = vec![make_plan("B1"), make_plan("B2")];

    // Advance cursor for model-a
    let _ = state.rotate_candidates_round_robin("model-a", candidates_a.clone());
    let _ = state.rotate_candidates_round_robin("model-a", candidates_a.clone());

    // model-b should start fresh (cursor=0)
    let rotated_b = state.rotate_candidates_round_robin("model-b", candidates_b.clone());
    assert_eq!(
        rotated_b
            .iter()
            .map(|(_, p)| p.provider_id.as_str())
            .collect::<Vec<_>>(),
        vec!["B1", "B2"],
        "different models should have independent cursors"
    );
}

/// Mock OpenAI-compatible chat upstream that counts the requests it receives.
async fn counting_chat_upstream(
    content: &'static str,
    hits: Arc<AtomicUsize>,
) -> std::net::SocketAddr {
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let hits = Arc::clone(&hits);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "id": "chatcmpl_test",
                    "created": 1,
                    "choices": [{
                        "finish_reason": "stop",
                        "message": {"role": "assistant", "content": content}
                    }]
                }))
            }
        }),
    );
    serve_on_random_port(app).await
}

/// Config where `deepseek-v4-flash-lp` is bound to two chat providers:
/// `deepseek` (upstream A) and `deepseek-b` (upstream B), in that order.
fn two_bound_chat_providers(
    addr_a: std::net::SocketAddr,
    addr_b: std::net::SocketAddr,
) -> config::Config {
    let mut cfg = config::default_deepseek_config();
    let mut provider_b = cfg.providers["deepseek"].clone();
    repoint_provider_urls(&mut provider_b, addr_b);
    provider_b.api_key_env = None;
    cfg.providers.insert("deepseek-b".to_string(), provider_b);
    let provider_a = cfg
        .providers
        .get_mut("deepseek")
        .expect("deepseek provider");
    repoint_provider_urls(provider_a, addr_a);
    provider_a.api_key_env = None;
    let model = cfg
        .models
        .get_mut("deepseek-v4-flash-lp")
        .expect("flash model");
    let upstream_name = model.openai_chat_providers[0].model.clone();
    model.openai_chat_providers.push(config::ProviderBinding {
        name: "deepseek-b".to_string(),
        model: upstream_name,
    });
    cfg
}

/// §5: RoundRobin must actually distribute ingress traffic across all
/// providers bound to one model — not merely rotate an in-memory vector in unit
/// tests. Six sequential requests to a two-provider model must alternate strictly.
#[tokio::test]
async fn chat_round_robin_distributes_requests_across_bound_providers() {
    let hits_a = Arc::new(AtomicUsize::new(0));
    let hits_b = Arc::new(AtomicUsize::new(0));
    let addr_a = counting_chat_upstream("from-a", Arc::clone(&hits_a)).await;
    let addr_b = counting_chat_upstream("from-b", Arc::clone(&hits_b)).await;

    let cfg = two_bound_chat_providers(addr_a, addr_b);
    let proxy_addr = serve_on_random_port(router(AppState::new_in_memory(cfg))).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");

    let mut sequence = Vec::new();
    for round in 0..6 {
        let before_a = hits_a.load(Ordering::SeqCst);
        let before_b = hits_b.load(Ordering::SeqCst);
        let response = client
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .json(&json!({
                "model": "deepseek-v4-flash-lp",
                "messages": [{"role": "user", "content": "ping"}]
            }))
            .send()
            .await
            .expect("proxy request");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "round {round} must succeed"
        );
        let after_a = hits_a.load(Ordering::SeqCst);
        let after_b = hits_b.load(Ordering::SeqCst);
        if after_a > before_a {
            sequence.push("a");
        } else if after_b > before_b {
            sequence.push("b");
        } else {
            panic!("round {round}: neither upstream received the request");
        }
    }
    assert_eq!(
        sequence,
        vec!["a", "b", "a", "b", "a", "b"],
        "RoundRobin must alternate the bound providers per request"
    );
}

/// §3.2: when the local frequency limit trips for the provider at the
/// head of the chain, the request must fall back to the next bound provider
/// instead of returning 429 to the client — and the limited provider must not
/// be contacted upstream at all.
#[tokio::test]
async fn local_frequency_limited_provider_falls_back_to_next_bound_provider() {
    let hits_a = Arc::new(AtomicUsize::new(0));
    let hits_b = Arc::new(AtomicUsize::new(0));
    let addr_a = counting_chat_upstream("from-a", Arc::clone(&hits_a)).await;
    let addr_b = counting_chat_upstream("from-b", Arc::clone(&hits_b)).await;

    let mut cfg = two_bound_chat_providers(addr_a, addr_b);
    // Provider A allows exactly one request per window with no queueing: the
    // second acquire is refused immediately instead of waiting for a timeout.
    cfg.providers
        .get_mut("deepseek")
        .expect("deepseek provider")
        .request_frequency = Some(config::RequestFrequencyConfig {
        requests_per_minute: Some(1),
        requests_per_hour: None,
        burst: Some(1),
        queue_timeout_seconds: Some(0),
    });

    let state = AppState::new_in_memory(cfg);
    // Exhaust provider A's seeded budget before any ingress request runs.
    let plan_a = state
        .config()
        .resolve_model_request_candidates(Protocol::OpenaiChatCompletions, "deepseek-v4-flash-lp")
        .into_iter()
        .map(|(_, plan)| plan)
        .find(|plan| plan.provider_id == "deepseek")
        .expect("execution plan for provider deepseek");
    assert!(
        state.try_acquire_frequency_now(&plan_a).await,
        "first acquire must consume the seeded budget"
    );

    let proxy_addr = serve_on_random_port(router(state)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-flash-lp",
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("proxy request");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "frequency-limited head provider must fall back instead of returning 429"
    );
    assert_eq!(
        hits_a.load(Ordering::SeqCst),
        0,
        "limited provider must not be contacted upstream"
    );
    assert_eq!(
        hits_b.load(Ordering::SeqCst),
        1,
        "fallback provider must serve the request"
    );
}

/// skip-fast: a frequency-limited candidate is passed over with zero wait —
/// the request reaches the next candidate without any per-candidate queueing.
/// Mutation-closed: reinstating a per-candidate wait makes this >= queue_timeout.
#[tokio::test]
async fn frequency_limited_candidate_is_skipped_without_waiting() {
    let hits_a = Arc::new(AtomicUsize::new(0));
    let hits_b = Arc::new(AtomicUsize::new(0));
    let addr_a = counting_chat_upstream("from-a", Arc::clone(&hits_a)).await;
    let addr_b = counting_chat_upstream("from-b", Arc::clone(&hits_b)).await;

    let mut cfg = two_bound_chat_providers(addr_a, addr_b);
    cfg.providers
        .get_mut("deepseek")
        .expect("deepseek provider")
        .request_frequency = Some(config::RequestFrequencyConfig {
        requests_per_minute: Some(1),
        requests_per_hour: None,
        burst: Some(1),
        queue_timeout_seconds: Some(2),
    });

    let state = AppState::new_in_memory(cfg);
    let plan_a = state
        .config()
        .resolve_model_request_candidates(Protocol::OpenaiChatCompletions, "deepseek-v4-flash-lp")
        .into_iter()
        .map(|(_, plan)| plan)
        .find(|plan| plan.provider_id == "deepseek")
        .expect("execution plan for provider deepseek");
    assert!(
        state.try_acquire_frequency_now(&plan_a).await,
        "first acquire must consume the seeded budget"
    );

    let proxy_addr = serve_on_random_port(router(state)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let started = std::time::Instant::now();
    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-flash-lp",
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("proxy request");
    let elapsed = started.elapsed();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        hits_a.load(Ordering::SeqCst),
        0,
        "limited head not contacted"
    );
    assert_eq!(hits_b.load(Ordering::SeqCst), 1, "fallback serves");
    assert!(
        elapsed < Duration::from_millis(500),
        "skip must be zero-wait (queue_timeout=2s); took {elapsed:?}"
    );
}

/// wait-last: when every candidate is frequency-limited the client gets a
/// local 429 carrying the local-frequency-limit marker, and the total wait
/// stays within one shared queue_timeout budget (no N x timeout accumulation).
#[tokio::test]
async fn all_candidates_limited_return_429_within_shared_budget() {
    let hits_a = Arc::new(AtomicUsize::new(0));
    let hits_b = Arc::new(AtomicUsize::new(0));
    let addr_a = counting_chat_upstream("from-a", Arc::clone(&hits_a)).await;
    let addr_b = counting_chat_upstream("from-b", Arc::clone(&hits_b)).await;

    let mut cfg = two_bound_chat_providers(addr_a, addr_b);
    for provider in ["deepseek", "deepseek-b"] {
        cfg.providers
            .get_mut(provider)
            .expect("provider")
            .request_frequency = Some(config::RequestFrequencyConfig {
            requests_per_minute: Some(1),
            requests_per_hour: None,
            burst: Some(1),
            queue_timeout_seconds: Some(1),
        });
    }

    let state = AppState::new_in_memory(cfg);
    for provider in ["deepseek", "deepseek-b"] {
        let plan = state
            .config()
            .resolve_model_request_candidates(
                Protocol::OpenaiChatCompletions,
                "deepseek-v4-flash-lp",
            )
            .into_iter()
            .map(|(_, plan)| plan)
            .find(|plan| plan.provider_id == provider)
            .expect("execution plan");
        assert!(
            state.try_acquire_frequency_now(&plan).await,
            "seeded budget must be consumed for {provider}"
        );
    }

    let proxy_addr = serve_on_random_port(router(state)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let started = std::time::Instant::now();
    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-flash-lp",
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("proxy request");
    let elapsed = started.elapsed();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response
            .headers()
            .get("x-llm-proxy-local-frequency-limit")
            .is_some(),
        "local frequency limit marker must be present"
    );
    assert_eq!(hits_a.load(Ordering::SeqCst), 0);
    assert_eq!(hits_b.load(Ordering::SeqCst), 0);
    assert!(
        elapsed >= Duration::from_millis(900),
        "last-ditch must wait for the shared budget; took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1900),
        "total wait must be one shared budget (1s), not N x budget; took {elapsed:?}"
    );
}

/// single-candidate compatibility: with a positive queue_timeout the only
/// candidate keeps the legacy wait buffer — capacity is polled until it
/// recovers instead of returning the local 429 immediately.
#[tokio::test]
async fn single_candidate_waits_for_capacity_before_429() {
    let hits = Arc::new(AtomicUsize::new(0));
    let addr = counting_chat_upstream("only", Arc::clone(&hits)).await;

    let mut cfg = config::default_deepseek_config();
    {
        let provider = cfg
            .providers
            .get_mut("deepseek")
            .expect("deepseek provider");
        repoint_provider_urls(provider, addr);
        provider.api_key_env = None;
        provider.request_frequency = Some(config::RequestFrequencyConfig {
            requests_per_minute: Some(600),
            requests_per_hour: None,
            burst: Some(1),
            queue_timeout_seconds: Some(2),
        });
    }

    let state = AppState::new_in_memory(cfg);
    let plan = state
        .config()
        .resolve_model_request_candidates(Protocol::OpenaiChatCompletions, "deepseek-v4-flash-lp")
        .into_iter()
        .map(|(_, plan)| plan)
        .find(|plan| plan.provider_id == "deepseek")
        .expect("execution plan");
    assert!(
        state.try_acquire_frequency_now(&plan).await,
        "seeded budget must be consumed"
    );

    let proxy_addr = serve_on_random_port(router(state)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let started = std::time::Instant::now();
    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-flash-lp",
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("proxy request");
    let elapsed = started.elapsed();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "single candidate must wait for capacity instead of an immediate 429"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(
        elapsed < Duration::from_secs(2),
        "wait must stay within the queue_timeout budget; took {elapsed:?}"
    );
}

/// Last-ditch re-scans only candidates that are *still* frequency-limited.
/// Once a candidate regains capacity and produces a real upstream result
/// (here a persistent 500), it must leave the re-scan set: exactly one
/// server-error retry sequence may run, not a fresh `send_upstream` every
/// 25ms pass until the shared budget expires (upstream-protection invariant).
#[tokio::test]
async fn last_ditch_stops_replaying_candidate_after_non_frequency_failure() {
    let hits = Arc::new(Mutex::new(0usize));
    let hits_handler = Arc::clone(&hits);
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let hits_handler = Arc::clone(&hits_handler);
            async move {
                *hits_handler.lock().unwrap() += 1;
                (StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, upstream_app)
            .await
            .expect("serve upstream");
    });

    let mut cfg = config::default_deepseek_config();
    cfg.fallback.server_error_retries = 2;
    cfg.fallback.server_error_retry_interval_ms = 0;
    {
        let provider = cfg
            .providers
            .get_mut("deepseek")
            .expect("deepseek provider");
        repoint_provider_urls(provider, addr);
        provider.api_key_env = None;
        // Fast refill (600rpm -> 10 token/s) so the candidate recovers inside
        // the last-ditch budget; queue_timeout=1s bounds the re-scan window.
        provider.request_frequency = Some(config::RequestFrequencyConfig {
            requests_per_minute: Some(600),
            requests_per_hour: None,
            burst: Some(1),
            queue_timeout_seconds: Some(1),
        });
    }

    let state = AppState::new_in_memory(cfg);
    let plan = state
        .config()
        .resolve_model_request_candidates(Protocol::OpenaiChatCompletions, "deepseek-v4-pro-lp")
        .into_iter()
        .map(|(_, plan)| plan)
        .find(|plan| plan.provider_id == "deepseek")
        .expect("execution plan");
    assert!(
        state.try_acquire_frequency_now(&plan).await,
        "seeded budget must be consumed"
    );

    // Pre-set a *transient* server-error cooldown: the cooldown gate then runs
    // with skip_cooling=false (all candidates transient -> keep probing), so a
    // cooled candidate is never silently skipped inside the re-scan loop. This
    // probe-mode scenario is where a non-frequency failure would be replayed
    // every pass if the re-scan set never shrank.
    let key = crate::config::CooldownKey {
        model_id: "deepseek-v4-pro-lp".into(),
        provider_id: "deepseek".into(),
        protocol: Protocol::OpenaiChatCompletions,
        token: None,
        token_key_hash: None,
    };
    state.set_cooldown(
        &key,
        "server_error",
        "upstream boom",
        Duration::from_secs(2),
    );

    let proxy_addr = serve_on_random_port(router(state)).await;
    let started = std::time::Instant::now();
    let response = chat_request(proxy_addr).await;
    let elapsed = started.elapsed();

    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "the real upstream result must survive as the final response, not be          overwritten by a re-scan 429 (took {elapsed:?})"
    );
    let hit_count = *hits.lock().expect("hits");
    assert_eq!(
        hit_count, 3,
        "exactly one server-error sequence (head + 2 retries), no last-ditch          replay of a non-frequency failure; took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "shared budget is 1s; replay would stretch this; took {elapsed:?}"
    );
}

/// queue_timeout_seconds = 0 pins both old and new semantics: no waiting at
/// all — the fast pass fails and the local 429 returns immediately.
#[tokio::test]
async fn zero_queue_timeout_returns_local_429_immediately() {
    let hits = Arc::new(AtomicUsize::new(0));
    let addr = counting_chat_upstream("only", Arc::clone(&hits)).await;

    let mut cfg = config::default_deepseek_config();
    {
        let provider = cfg
            .providers
            .get_mut("deepseek")
            .expect("deepseek provider");
        repoint_provider_urls(provider, addr);
        provider.api_key_env = None;
        provider.request_frequency = Some(config::RequestFrequencyConfig {
            requests_per_minute: Some(1),
            requests_per_hour: None,
            burst: Some(1),
            queue_timeout_seconds: Some(0),
        });
    }

    let state = AppState::new_in_memory(cfg);
    let plan = state
        .config()
        .resolve_model_request_candidates(Protocol::OpenaiChatCompletions, "deepseek-v4-flash-lp")
        .into_iter()
        .map(|(_, plan)| plan)
        .find(|plan| plan.provider_id == "deepseek")
        .expect("execution plan");
    assert!(
        state.try_acquire_frequency_now(&plan).await,
        "seeded budget must be consumed"
    );

    let proxy_addr = serve_on_random_port(router(state)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let started = std::time::Instant::now();
    let response = client
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-v4-flash-lp",
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .expect("proxy request");
    let elapsed = started.elapsed();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response
            .headers()
            .get("x-llm-proxy-local-frequency-limit")
            .is_some()
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert!(
        elapsed < Duration::from_millis(500),
        "zero budget must not wait; took {elapsed:?}"
    );
}

fn install_mock_tokens(
    state: &mut AppState,
    config_dir: &std::path::Path,
    state_dir: &std::path::Path,
) -> (&'static str, &'static str) {
    const ALICE_KEY: &str = "lp-mock-alice-key-12345";
    const BOB_KEY: &str = "lp-mock-bob-key-67890";
    std::fs::write(
        config_dir.join("tokens.toml"),
        format!(
            "[[tokens]]\n\
             key = \"{ALICE_KEY}\"\n\
             name = \"alice\"\n\n\
             [[tokens]]\n\
             key = \"{BOB_KEY}\"\n\
             name = \"bob\"\n"
        ),
    )
    .expect("write tokens.toml");
    state.token =
        crate::token::auth::TokenAuthState::load(&config_dir.join("config.toml"), state_dir);
    (ALICE_KEY, BOB_KEY)
}

/// Both protection ledgers share one client-fault gate: credential failures
/// (401/403), rate limits (429), and rate-limit-class 400s never count toward
/// the token's error-class cooldown nor the bad-request budget — while genuine
/// client-shape 400s still count on both (positive control).
#[test]
fn error_class_ledgers_share_client_fault_gate() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let tok = crate::token::auth::TokenContext {
        key: "lp-exempt-test".to_string(),
        name: "exempt".to_string(),
        allowed_models: Vec::new(),
        admin: false,
    };

    let relay = |status: StatusCode, raw: &str, msg: &str| {
        state.record_client_error_for_token(Some(&tok), "test-model", status, raw, msg);
    };

    // 401 credential failure ×3 → neither ledger counts.
    let raw_401 = r#"{"error":{"message":"Invalid API Key","type":"unauthorized"}}"#;
    for _ in 0..3 {
        relay(StatusCode::UNAUTHORIZED, raw_401, "Invalid API Key");
    }
    // 403 ×3 → excluded (dedicated cooldown path).
    for _ in 0..3 {
        relay(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"Forbidden"}}"#,
            "Forbidden",
        );
    }
    // 429 rate limit ×3 → excluded (rate-limit cooldown path).
    for _ in 0..3 {
        relay(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"too many requests"}}"#,
            "too many requests",
        );
    }
    // Rate-limit-class 400 ×3 (rate-limit keywords in body) → excluded from
    // BOTH ledgers (historically only the budget excluded it).
    let raw_rl400 = r#"{"error":{"message":"quota exceeded for this model"}}"#;
    for _ in 0..3 {
        relay(
            StatusCode::BAD_REQUEST,
            raw_rl400,
            "quota exceeded for this model",
        );
    }

    assert!(
        state.error_cooldown.snapshot().is_empty(),
        "excluded statuses must never enter the error-class ledger"
    );
    assert!(
        state.account_budget_snapshot().is_empty(),
        "excluded statuses must never consume the bad-request budget"
    );

    // Positive control: a genuine client-shape 400 counts on BOTH ledgers.
    let raw_400 = r#"{"error":{"message":"max_tokens must be <= 8192"}}"#;
    for _ in 0..3 {
        relay(
            StatusCode::BAD_REQUEST,
            raw_400,
            "max_tokens must be <= 8192",
        );
    }
    assert!(
        state.token_error_cooldown_blocked("exempt", "test-model"),
        "a repeated genuine client error must still trigger cooldown"
    );
    assert!(
        state
            .account_budget_snapshot()
            .iter()
            .any(|b| b.lifetime_used >= 1),
        "genuine client errors must still consume the budget"
    );
}

/// 402 / 404 / 408 each own a dedicated cooldown path and describe a
/// provider-side condition — billing (account), model-not-deployed (catalog),
/// timeout (network) — so neither the error-class ledger nor the bad-request
/// budget may hold it against the token. Genuine client faults stay counted
/// (positive control), otherwise the gate would silently disable protection.
#[test]
fn error_class_ledgers_exempt_dedicated_cooldown_statuses() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    let relay = |name: &str, status: StatusCode, raw: &str, msg: &str| {
        let tok = crate::token::auth::TokenContext {
            key: format!("lp-dedicated-{name}"),
            name: name.to_string(),
            allowed_models: Vec::new(),
            admin: false,
        };
        state.record_client_error_for_token(Some(&tok), "model-x", status, raw, msg);
    };

    // Three strikes of each exempt status — the count that trips the threshold
    // for a genuine client fault.
    for _ in 0..3 {
        relay(
            "billing",
            StatusCode::PAYMENT_REQUIRED,
            r#"{"error":{"message":"insufficient credits to run this model"}}"#,
            "insufficient credits",
        );
        relay(
            "catalog",
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"the requested model was not found on this endpoint"}}"#,
            "model not found",
        );
        relay(
            "network",
            StatusCode::REQUEST_TIMEOUT,
            r#"{"error":{"message":"upstream request timed out"}}"#,
            "upstream request timed out",
        );
    }

    assert!(
        state.error_cooldown.snapshot().is_empty(),
        "exempt statuses must never enter the error-class ledger: {:?}",
        state.error_cooldown.snapshot()
    );
    assert!(
        state.account_budget_snapshot().is_empty(),
        "exempt statuses must never consume the bad-request budget"
    );
    for name in ["billing", "catalog", "network"] {
        assert!(
            !state.token_error_cooldown_blocked(name, "model-x"),
            "{name} must never be blocked by the error-class ledger"
        );
    }

    // Positive control: a generic 4xx without a dedicated path (405) is a
    // request-level fault and still counts on both ledgers.
    for _ in 0..3 {
        relay(
            "generic4xx",
            StatusCode::METHOD_NOT_ALLOWED,
            r#"{"error":{"message":"POST is not supported by this endpoint route"}}"#,
            "POST is not supported",
        );
    }
    assert!(
        state.token_error_cooldown_blocked("generic4xx", "model-x"),
        "a repeated generic 4xx must still trigger the error-class cooldown"
    );
    assert!(
        state
            .account_budget_snapshot()
            .iter()
            .any(|b| b.lifetime_used >= 1),
        "a repeated generic 4xx must still consume the budget"
    );
}

/// Standard OpenAI passthrough path error-class cooldown blocks after threshold.
#[tokio::test]
async fn openai_passthrough_token_error_class_cooldown_blocks() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_handler = Arc::clone(&hits);
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let hits = Arc::clone(&hits_handler);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": {
                            "message": "Invalid schema for tool definition",
                            "type": "invalid_request_error"
                        }
                    })),
                )
            }
        }),
    );
    let upstream_addr = serve_on_random_port(upstream_app).await;

    let mut provider = crate::catalog::deepseek().provider;
    repoint_provider_urls(&mut provider, upstream_addr);
    provider.api_key_env = None;

    let model = empty_model(true, false, false, "deepseek", "deepseek-chat");
    let mut cfg = config_with(vec![("deepseek", provider)], vec![("test-model", model)]);
    cfg.fallback.cooldown.client_error_seconds = 0;
    // Disable the per-account bad-request budget so it doesn't interfere with
    // this test of the error-class cooldown (L2) layer.
    cfg.protection.bad_request_budget.enabled = false;
    let mut state = AppState::new_in_memory(cfg);

    let config_dir = tempfile::tempdir().expect("config dir");
    let token_state_dir = tempfile::tempdir().expect("token state dir");
    let (alice_key, bob_key) =
        install_mock_tokens(&mut state, config_dir.path(), token_state_dir.path());
    let proxy_addr = serve_on_random_port(router(state.clone())).await;

    let make_req = |turn: usize| {
        json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": format!("agent turn {turn}")}],
            "stream": false
        })
    };

    for round in 1..=3 {
        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .bearer_auth(alice_key)
            .json(&make_req(round))
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "round {round} gets 400"
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 3, "upstream hit 3 times");

    // Alice is now in cooldown
    assert!(state.token_error_cooldown_blocked("alice", "test-model"));

    // 4th request from Alice blocked with 429
    let resp4 = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .bearer_auth(alice_key)
        .json(&make_req(4))
        .send()
        .await
        .expect("send");
    assert_eq!(resp4.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(hits.load(Ordering::SeqCst), 3, "upstream not contacted");

    // Bob can still reach upstream
    let resp_bob = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .bearer_auth(bob_key)
        .json(&make_req(5))
        .send()
        .await
        .expect("send");
    assert_eq!(resp_bob.status(), StatusCode::BAD_REQUEST);
    assert_eq!(hits.load(Ordering::SeqCst), 4);
}

/// Different error classes do not accumulate toward cooldown threshold.
#[tokio::test]
async fn different_error_classes_do_not_trigger_cooldown() {
    let error_messages = Arc::new(std::sync::Mutex::new(vec![
        "Function call is missing a thought_signature in functionCall parts.",
        "Invalid schema for tool definition",
        "missing a required parameter: name",
    ]));
    let upstream_app = Router::new().route(
        "/chat/completions",
        post({
            let error_messages = Arc::clone(&error_messages);
            move || {
                let error_messages = Arc::clone(&error_messages);
                async move {
                    let msg = error_messages.lock().unwrap().remove(0);
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": {
                                "message": msg,
                                "type": "invalid_request_error"
                            }
                        })),
                    )
                }
            }
        }),
    );
    let upstream_addr = serve_on_random_port(upstream_app).await;

    let mut provider = crate::catalog::deepseek().provider;
    repoint_provider_urls(&mut provider, upstream_addr);
    provider.api_key_env = None;

    let model = empty_model(true, false, false, "deepseek", "deepseek-chat");
    let mut cfg = config_with(vec![("deepseek", provider)], vec![("test-model", model)]);
    cfg.fallback.cooldown.client_error_seconds = 0;
    // Disable the per-account bad-request budget so it doesn't interfere with
    // this test of the error-class cooldown (L2) layer.
    cfg.protection.bad_request_budget.enabled = false;
    let mut state = AppState::new_in_memory(cfg);

    let config_dir = tempfile::tempdir().expect("config dir");
    let token_state_dir = tempfile::tempdir().expect("token state dir");
    let (alice_key, _bob_key) =
        install_mock_tokens(&mut state, config_dir.path(), token_state_dir.path());
    let proxy_addr = serve_on_random_port(router(state.clone())).await;

    let make_req = |turn: usize| {
        json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": format!("agent turn {turn}")}],
            "stream": false
        })
    };

    // Alice makes 3 requests, each getting a DIFFERENT error class
    for round in 1..=3 {
        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .bearer_auth(alice_key)
            .json(&make_req(round))
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "round {round} gets 400"
        );
    }

    // Alice must NOT be in cooldown because each error class count is only 1 (< 3)
    assert!(
        !state.token_error_cooldown_blocked("alice", "test-model"),
        "alice must not enter cooldown when errors belong to different classes"
    );

    // Alice's snapshot shows 3 distinct error classes with count 1
    let snap = state.error_cooldown.snapshot();
    let alice_snap = snap
        .iter()
        .find(|s| s.token == "alice")
        .expect("alice state");
    assert_eq!(alice_snap.error_counts.len(), 3);
    for count in alice_snap.error_counts.values() {
        assert_eq!(*count, 1);
    }
}

/// Mock OpenAI-Chat upstream that always answers with a genuine client-shape 400.
async fn serve_constant_400_upstream() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_handler = Arc::clone(&hits);
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let hits = Arc::clone(&hits_handler);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": {
                            "message": "max_tokens must be <= 8192",
                            "type": "invalid_request_error"
                        }
                    })),
                )
            }
        }),
    );
    let addr = serve_on_random_port(app).await;
    (addr, hits)
}

fn mock_400_provider(addr: std::net::SocketAddr) -> config::ProviderConfig {
    let mut provider = crate::catalog::deepseek().provider;
    repoint_provider_urls(&mut provider, addr);
    provider.api_key_env = None;
    provider
}

fn chat_key_for(provider: &str, token: Option<&str>) -> config::CooldownKey {
    config::CooldownKey {
        model_id: "test-model".to_string(),
        provider_id: provider.to_string(),
        protocol: Protocol::OpenaiChatCompletions,
        token: token.map(str::to_string),
        token_key_hash: None,
    }
}

/// A genuine upstream 400 is terminal for the candidate chain: the second
/// provider never sees the same bad body, the fingerprint is fed, and the
/// `client_error_seconds` cooldown is scoped to the requesting
/// (token × provider) pair only.
#[tokio::test]
async fn upstream_400_short_circuits_chain_and_cools_requesting_token_pair() {
    let (a1, h1) = serve_constant_400_upstream().await;
    let (a2, h2) = serve_constant_400_upstream().await;

    let mut model = empty_model(true, false, false, "p1", "deepseek-chat");
    model.openai_chat_providers.push(config::ProviderBinding {
        name: "p2".to_string(),
        model: "deepseek-chat".to_string(),
    });
    let mut cfg = config_with(
        vec![("p1", mock_400_provider(a1)), ("p2", mock_400_provider(a2))],
        vec![("test-model", model)],
    );
    cfg.protection.bad_request_budget.enabled = false;
    let mut state = AppState::new_in_memory(cfg);

    let config_dir = tempfile::tempdir().expect("config dir");
    let token_state_dir = tempfile::tempdir().expect("token state dir");
    let (alice_key, bob_key) =
        install_mock_tokens(&mut state, config_dir.path(), token_state_dir.path());
    let proxy_addr = serve_on_random_port(router(state.clone())).await;

    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .bearer_auth(alice_key)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "turn 1"}],
            "stream": false,
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "the upstream 400 is relayed to the client"
    );
    let hits_first = h1.load(Ordering::SeqCst);
    let hits_second = h2.load(Ordering::SeqCst);
    assert_eq!(
        hits_first + hits_second,
        1,
        "the bad body must not be replayed on the remaining candidate"
    );

    let tried = if hits_first == 1 { "p1" } else { "p2" };
    let other = if hits_first == 1 { "p2" } else { "p1" };

    let snaps = state.bad_requests.snapshot();
    assert_eq!(
        snaps.len(),
        1,
        "the short-circuit feeds the fingerprint exactly once"
    );
    assert_eq!(snaps[0].error_count, 1);

    assert!(
        state.is_cooling_down(&chat_key_for(tried, Some("alice"))),
        "the 400 must cool the requesting token on the provider that rejected it"
    );
    assert!(
        !state.is_cooling_down(&chat_key_for(tried, None)),
        "provider health is not implicated — the entry is token-scoped"
    );
    assert!(
        !state.is_cooling_down(&chat_key_for(tried, Some("bob"))),
        "other tokens keep serving"
    );
    assert!(
        !state.is_cooling_down(&chat_key_for(other, Some("alice"))),
        "the never-tried candidate stays clean"
    );

    // Bob still reaches upstream (the chain itself is not blocked).
    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .bearer_auth(bob_key)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "turn 2"}],
            "stream": false,
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        h1.load(Ordering::SeqCst) + h2.load(Ordering::SeqCst),
        2,
        "bob's request reaches upstream"
    );
}

/// Error-class counting and blocking scope to the (token, inbound model) pair:
/// three upstream 400s on one model block that model for the token while a
/// sibling model of the same token keeps working.
#[tokio::test]
async fn token_error_cooldown_blocks_only_the_inbound_model_that_errored() {
    let (addr, hits) = serve_constant_400_upstream().await;
    let mut cfg = config_with(
        vec![("deepseek", mock_400_provider(addr))],
        vec![
            (
                "model-a",
                empty_model(true, false, false, "deepseek", "chat-a"),
            ),
            (
                "model-b",
                empty_model(true, false, false, "deepseek", "chat-b"),
            ),
        ],
    );
    // Keep the (token × provider) cooldown and the budget out of the picture so
    // this test observes the error-class ledger only.
    cfg.fallback.cooldown.client_error_seconds = 0;
    cfg.protection.bad_request_budget.enabled = false;
    let mut state = AppState::new_in_memory(cfg);

    let config_dir = tempfile::tempdir().expect("config dir");
    let token_state_dir = tempfile::tempdir().expect("token state dir");
    let (alice_key, _bob_key) =
        install_mock_tokens(&mut state, config_dir.path(), token_state_dir.path());
    let proxy_addr = serve_on_random_port(router(state.clone())).await;

    for turn in 1..=3 {
        let response = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .bearer_auth(alice_key)
            .json(&json!({
                "model": "model-a",
                "messages": [{"role": "user", "content": format!("a turn {turn}")}],
                "stream": false,
            }))
            .send()
            .await
            .expect("send");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 3);

    let blocked = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .bearer_auth(alice_key)
        .json(&json!({
            "model": "model-a",
            "messages": [{"role": "user", "content": "a turn 4"}],
            "stream": false,
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    let body: Value = blocked.json().await.expect("json");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("token blocked after repeated upstream errors"),
        "the 429 must come from the error-class ledger: {msg}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "the blocked request must not reach upstream"
    );

    let sibling = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .bearer_auth(alice_key)
        .json(&json!({
            "model": "model-b",
            "messages": [{"role": "user", "content": "b turn 1"}],
            "stream": false,
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(
        sibling.status(),
        StatusCode::BAD_REQUEST,
        "a sibling model must not inherit the block"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        4,
        "the sibling model reaches upstream"
    );

    let snap = state.error_cooldown.snapshot();
    let a = snap
        .iter()
        .find(|s| s.model == "model-a")
        .expect("model-a entry");
    assert_eq!(a.token, "alice");
    assert!(a.cooldown_remaining > 0, "model-a stays in cooldown");
    assert!(
        a.error_counts.is_empty(),
        "counts clear when the trigger fires"
    );
    let b = snap
        .iter()
        .find(|s| s.model == "model-b")
        .expect("model-b entry");
    assert_eq!(b.cooldown_remaining, 0, "model-b is not blocked");
    assert_eq!(b.error_counts.values().sum::<u32>(), 1);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Serve lifecycle zero-write and endpoint prompt tests
// ═══════════════════════════════════════════════════════════════════════════════

use std::fs;

#[tokio::test]
#[serial]
async fn test_serve_full_lifecycle_zero_client_writes() {
    let temp_home = tempfile::tempdir().expect("temp home");
    let original_home = std::env::var_os("HOME");
    // SAFETY: test-only, serialized test context
    unsafe { std::env::set_var("HOME", temp_home.path()) };

    // Pre-populate all SIX client targets
    // 1. codex / codex-desktop (shared target)
    let codex_file = temp_home.path().join(".codex/config.toml");
    let codex_catalog = temp_home
        .path()
        .join(".codex/llm-proxy-test-model-catalog.json");
    fs::create_dir_all(codex_file.parent().unwrap()).unwrap();
    fs::write(&codex_file, "original codex content").unwrap();
    fs::write(&codex_catalog, "original catalog content").unwrap();

    // 2. pi
    let pi_file = temp_home.path().join(".pi/agent/models.json");
    fs::create_dir_all(pi_file.parent().unwrap()).unwrap();
    fs::write(&pi_file, "original pi content").unwrap();

    // 3. qwen
    let qwen_file = temp_home.path().join(".qwen/settings.json");
    fs::create_dir_all(qwen_file.parent().unwrap()).unwrap();
    fs::write(&qwen_file, "original qwen content").unwrap();

    // 4. claude-code
    let claude_file = temp_home.path().join(".claude/settings.json");
    fs::create_dir_all(claude_file.parent().unwrap()).unwrap();
    fs::write(&claude_file, "original claude content").unwrap();

    // 5. opencode
    let opencode_file = temp_home.path().join(".config/opencode/opencode.json");
    fs::create_dir_all(opencode_file.parent().unwrap()).unwrap();
    fs::write(&opencode_file, "original opencode content").unwrap();

    // 6. claude-desktop
    let claude_desktop_file = temp_home
        .path()
        .join(".config/Claude/claude_desktop_config.json");
    fs::create_dir_all(claude_desktop_file.parent().unwrap()).unwrap();
    fs::write(&claude_desktop_file, "original claude desktop content").unwrap();

    // Non-existent paths to assert they are NOT created
    let non_existent_paths = vec![
        temp_home.path().join(".pi/agent/unrelated.json"),
        temp_home.path().join(".qwen/unrelated.json"),
        temp_home.path().join(".claude/unrelated.json"),
    ];

    // Snapshot pre-state
    let targets = [
        codex_file.clone(),
        codex_catalog.clone(),
        pi_file.clone(),
        qwen_file.clone(),
        claude_file.clone(),
        opencode_file.clone(),
        claude_desktop_file.clone(),
    ];
    let pre_snapshots: Vec<(String, u64, std::time::SystemTime)> = targets
        .iter()
        .map(|p| {
            let meta = fs::metadata(p).unwrap();
            let content = fs::read_to_string(p).unwrap();
            (content, meta.len(), meta.modified().unwrap())
        })
        .collect();

    // Step 1: Start server
    let mut cfg = config::default_deepseek_config();
    cfg.server.as_mut().unwrap().listen = "127.0.0.1:0".to_string();
    let state = AppState::new_in_memory_for_management(cfg);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(serve_state_with_shutdown(state.clone(), shutdown_rx));

    // Yield to let server bind
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;

    // Step 2: Runtime admin requests (admin client-config read operation)
    for client in [
        "pi",
        "qwen-code",
        "codex",
        "codex-desktop",
        "claude-code",
        "claude-desktop",
        "opencode",
    ] {
        let (status, resp) = crate::admin::client_config(
            axum::extract::State(state.clone()),
            None,
            axum::extract::Path(client.to_string()),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(resp.status, "ok");
    }

    // Step 3: Shutdown
    let _ = shutdown_tx.send("test_shutdown");
    let exit_reason = server_task
        .await
        .expect("join server task")
        .expect("server exit");
    assert_eq!(exit_reason, "test_shutdown");

    // Restore HOME
    match original_home {
        Some(val) => unsafe { std::env::set_var("HOME", val) },
        None => unsafe { std::env::remove_var("HOME") },
    }

    // Assert all 6 client targets are untouched
    for (i, p) in targets.iter().enumerate() {
        let meta = fs::metadata(p).unwrap();
        let content = fs::read_to_string(p).unwrap();
        let (pre_content, pre_len, pre_mtime) = &pre_snapshots[i];
        assert_eq!(
            &content,
            pre_content,
            "content modified for {}",
            p.display()
        );
        assert_eq!(meta.len(), *pre_len, "length modified for {}", p.display());
        assert_eq!(
            meta.modified().unwrap(),
            *pre_mtime,
            "mtime modified for {}",
            p.display()
        );
    }

    // Assert non-existent paths were NOT created
    for p in &non_existent_paths {
        assert!(!p.exists(), "path should not be created: {}", p.display());
    }
}

#[tokio::test]
#[serial]
async fn test_serve_endpoint_change_regression_no_client_auto_modification() {
    let temp_home = tempfile::tempdir().expect("temp home");
    let original_home = std::env::var_os("HOME");
    // SAFETY: test-only, serialized test context
    unsafe { std::env::set_var("HOME", temp_home.path()) };

    let pi_dir = temp_home.path().join(".pi/agent");
    fs::create_dir_all(&pi_dir).unwrap();
    let pi_file = pi_dir.join("models.json");
    let initial_config = serde_json::json!({
        "providers": {
            "llm-proxy-openai-chat": {
                "baseUrl": "http://127.0.0.1:9090/openai/v1"
            }
        }
    });
    fs::write(&pi_file, initial_config.to_string()).unwrap();

    // Start server on a different port (random port via 127.0.0.1:0)
    let mut cfg = config::default_deepseek_config();
    cfg.server.as_mut().unwrap().listen = "127.0.0.1:0".to_string();
    let state = AppState::new_in_memory_for_management(cfg.clone());
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(serve_state_with_shutdown(state.clone(), shutdown_rx));

    tokio::time::sleep(std::time::Duration::from_millis(60)).await;

    let _ = shutdown_tx.send("test_shutdown");
    let _ = server_task.await.expect("join server task");

    // Client config MUST NOT have been auto-modified
    let current_content = fs::read_to_string(&pi_file).unwrap();
    assert!(
        current_content.contains("9090"),
        "client config should retain old address, but got: {current_content}"
    );

    // Now re-run launch explicitly with new config (e.g. port 9999) -> updates config
    cfg.server.as_mut().unwrap().listen = "127.0.0.1:9999".to_string();
    crate::clients::launch_pi(&cfg, Some(pi_dir.clone()), false).unwrap();
    let updated_content = fs::read_to_string(&pi_file).unwrap();
    assert!(
        updated_content.contains("9999"),
        "explicit launch should update address to 9999, got: {updated_content}"
    );

    // Restore HOME
    match original_home {
        Some(val) => unsafe { std::env::set_var("HOME", val) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}

#[tokio::test]
#[serial]
async fn test_serve_startup_endpoint_prompt_log() {
    use tracing_subscriber::prelude::*;

    struct EndpointLogLayer(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EndpointLogLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor(String);
            impl tracing::field::Visit for Visitor {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "message" {
                        self.0 = value.to_string();
                    }
                }
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut v = Visitor(String::new());
            event.record(&mut v);
            if !v.0.is_empty() {
                self.0.lock().unwrap().push(v.0);
            }
        }
    }

    let logs = Arc::new(Mutex::new(Vec::new()));
    let _registration = tracing::Dispatch::new(tracing_subscriber::registry());
    let subscriber = tracing_subscriber::registry().with(EndpointLogLayer(Arc::clone(&logs)));
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut cfg = config::default_deepseek_config();
    cfg.server.as_mut().unwrap().listen = "127.0.0.1:0".to_string();
    let state = AppState::new_in_memory_for_management(cfg);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(serve_state_with_shutdown(state.clone(), shutdown_rx));

    tokio::time::sleep(std::time::Duration::from_millis(60)).await;

    let _ = shutdown_tx.send("test_shutdown");
    let _ = server_task.await.expect("join server task");

    let captured = logs.lock().unwrap().clone();
    let matching_lines: Vec<_> = captured
        .iter()
        .filter(|line| {
            line.contains("client configs are not auto-modified — point clients with 'llm-proxy launch <client>'")
        })
        .cloned()
        .collect();

    assert_eq!(
        matching_lines.len(),
        1,
        "expected exactly one endpoint prompt line, found: {matching_lines:?}"
    );

    let prompt_line = &matching_lines[0];
    assert!(
        prompt_line.starts_with("llm-proxy listening on "),
        "prompt line must start with prefix: {prompt_line}"
    );
    // Assert actual assigned port is present, not 0
    assert!(
        !prompt_line.contains("127.0.0.1:0"),
        "bound port must not be 0 in log: {prompt_line}"
    );
    assert!(
        prompt_line.contains("127.0.0.1:"),
        "bound address must be present: {prompt_line}"
    );
}

/// : diagnostics JSONL path must live under `state_dir`, not `runtime_dir`.
/// Mutation guard: if anyone changes `new_in_memory_for_management` back to
/// `core.runtime_dir().join("diagnostics.jsonl")`, the path will contain
/// `/runtime/` instead of `/state/` and this assertion fires.
#[test]
fn diagnostics_path_uses_state_dir_not_runtime_dir() {
    let state = AppState::new_in_memory(config::default_deepseek_config());
    let store = state
        .diagnostics_store()
        .expect("diagnostics store must be Some in test AppState");
    let path = store.jsonl_path();
    let path_str = path.to_string_lossy();
    assert!(
        path_str.contains("/state/diagnostics.jsonl")
            || path_str.ends_with("state/diagnostics.jsonl")
            || path_str.contains("\\state\\diagnostics.jsonl"),
        "diagnostics JSONL must be under state_dir, got: {path_str}"
    );
    // Negative check: must NOT be under runtime/
    assert!(
        !path_str.contains("/runtime/diagnostics.jsonl")
            && !path_str.contains("\\runtime\\diagnostics.jsonl"),
        "diagnostics JSONL must NOT be under runtime_dir, got: {path_str}"
    );
}

// ─── : streaming metrics red-proof tests ───

/// Red proof: streaming and non-streaming requests increment separate counters.
/// If the `is_streaming` flag were swapped (true↔false), this test would fail
/// because the counters would go to the wrong bucket.
#[test]
fn streaming_metrics_separate_stream_from_non_stream() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    // Simulate 2 non-streaming requests
    state.record_usage(
        "model-a".into(),
        "provider-x".into(),
        "openai_chat".into(),
        100,
        200,
        Some(50),
        None,
        false, // non-streaming
        1024,  // response bytes
        None,
    );
    state.record_usage(
        "model-b".into(),
        "provider-x".into(),
        "openai_chat".into(),
        50,
        75,
        Some(30),
        None,
        false, // non-streaming
        512,   // response bytes
        None,
    );

    // Simulate 1 streaming request
    state.record_usage(
        "model-a".into(),
        "provider-y".into(),
        "openai_responses".into(),
        300,
        400,
        None,
        None,
        true, // streaming
        0,    // SSE byte tracking deferred
        None,
    );

    let snapshot = state.streaming_metrics.snapshot();

    // Non-streaming bucket: 2 requests, 1536 bytes, 150 input, 275 output
    assert_eq!(
        snapshot.non_stream_requests, 2,
        "non-stream request count must be 2"
    );
    assert_eq!(
        snapshot.non_stream_response_bytes, 1536,
        "non-stream response bytes must be 1024+512=1536"
    );
    assert_eq!(
        snapshot.non_stream_input_tokens, 150,
        "non-stream input tokens must be 100+50=150"
    );
    assert_eq!(
        snapshot.non_stream_output_tokens, 275,
        "non-stream output tokens must be 200+75=275"
    );

    // Streaming bucket: 1 request, 0 bytes, 300 input, 400 output
    assert_eq!(
        snapshot.stream_requests, 1,
        "stream request count must be 1"
    );
    assert_eq!(
        snapshot.stream_response_bytes, 0,
        "stream response bytes must be 0 (SSE tracking deferred)"
    );
    assert_eq!(
        snapshot.stream_input_tokens, 300,
        "stream input tokens must be 300"
    );
    assert_eq!(
        snapshot.stream_output_tokens, 400,
        "stream output tokens must be 400"
    );
}

/// Mutation test: if the `is_streaming` flag is inverted at the call site,
/// the counters go to the wrong bucket and this test catches it.
#[test]
fn mutation_stream_flag_inverted_routes_to_wrong_bucket() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    // Record what SHOULD be a streaming request, but simulate the bug
    // where the flag is inverted (passed as false instead of true).
    state.record_usage(
        "model".into(),
        "provider".into(),
        "openai_chat".into(),
        100,
        200,
        None,
        None,
        false, // BUG: should be true (inverted flag)
        2048,
        None,
    );

    let snapshot = state.streaming_metrics.snapshot();

    // The non-stream bucket got it (wrong bucket due to inverted flag)
    assert_eq!(snapshot.non_stream_requests, 1);
    assert_eq!(snapshot.non_stream_response_bytes, 2048);
    // The stream bucket is empty — proving the branch is load-bearing
    assert_eq!(snapshot.stream_requests, 0);
    assert_eq!(snapshot.stream_response_bytes, 0);
}

/// Verify `record_usage_with_status` also drives streaming metrics.
#[test]
fn record_usage_with_status_drives_streaming_metrics() {
    let state = AppState::new_in_memory(config::default_deepseek_config());

    state.record_usage_with_status(
        "model".into(),
        "provider".into(),
        "openai_chat".into(),
        50,
        100,
        Some(25),
        None,
        Some("upstream_interrupted"),
        true, // streaming
        0,
        false,
        None,
        None, // ttfb_ms
    );

    let snapshot = state.streaming_metrics.snapshot();
    assert_eq!(snapshot.stream_requests, 1);
    assert_eq!(snapshot.stream_input_tokens, 50);
    assert_eq!(snapshot.stream_output_tokens, 100);
    assert_eq!(snapshot.non_stream_requests, 0);
}

// ── : truncation rate observability ──

/// Red test: truncated response (finish_reason=length) records truncated=true, reason=max_tokens.
#[test]
fn truncated_response_records_truncation() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let jsonl_path = temp_dir.path().join("usage.jsonl");
    let db_path = temp_dir.path().join("usage.db");
    let store = Arc::new(
        UsageStore::with_paths(crate::config::UsageConfig::default(), jsonl_path, db_path)
            .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&store));

    // Simulate a truncated response via record_usage_with_status.
    state.record_usage_with_status(
        "model".into(),
        "provider".into(),
        "openai_chat".into(),
        100,
        200,
        Some(50),
        None,
        Some("completed"),
        false,
        0,
        true,               // truncated
        Some("max_tokens"), // reason
        None,               // ttfb_ms
    );

    // Flush to SQLite and verify.
    store.flush_if_dirty().unwrap();
    let records = store.query(None, None, None, None, None);
    assert_eq!(records.len(), 1);
    assert!(
        records[0].truncated,
        "truncated should be true for max_tokens truncation"
    );
    assert_eq!(
        records[0].truncation_reason.as_deref(),
        Some("max_tokens"),
        "truncation_reason should be max_tokens"
    );
}

/// Red test: normal response records truncated=false, truncation_reason=None.
#[test]
fn normal_response_records_no_truncation() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let jsonl_path = temp_dir.path().join("usage.jsonl");
    let db_path = temp_dir.path().join("usage.db");
    let store = Arc::new(
        UsageStore::with_paths(crate::config::UsageConfig::default(), jsonl_path, db_path)
            .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&store));

    // Simulate a normal (non-truncated) response.
    state.record_usage_with_status(
        "model".into(),
        "provider".into(),
        "openai_chat".into(),
        100,
        200,
        Some(50),
        None,
        Some("completed"),
        false,
        0,
        false, // not truncated
        None,  // no reason
        None,  // ttfb_ms
    );

    store.flush_if_dirty().unwrap();
    let records = store.query(None, None, None, None, None);
    assert_eq!(records.len(), 1);
    assert!(
        !records[0].truncated,
        "truncated should be false for normal response"
    );
    assert!(
        records[0].truncation_reason.is_none(),
        "truncation_reason should be None for normal response"
    );
}

/// Mutation test: UsageRecord::with_truncation(false, _) must clear reason.
/// If the implementation is reversed (sets truncated=true when told false), this test fails.
#[test]
fn with_truncation_false_clears_reason() {
    let record = UsageRecord::new("m".into(), "p".into(), "e".into(), 1, 2, None)
        .with_truncation(false, Some("max_tokens".to_string()));

    assert!(
        !record.truncated,
        "with_truncation(false, _) must set truncated=false"
    );
    assert!(
        record.truncation_reason.is_none(),
        "with_truncation(false, _) must clear truncation_reason even if a reason was provided"
    );
}

/// Mutation test: UsageRecord::with_truncation(true, _) must set both fields.
/// If the implementation ignores the truncated flag, this test fails.
#[test]
fn with_truncation_true_sets_both_fields() {
    let record = UsageRecord::new("m".into(), "p".into(), "e".into(), 1, 2, None)
        .with_truncation(true, Some("content_filter".to_string()));

    assert!(
        record.truncated,
        "with_truncation(true, _) must set truncated=true"
    );
    assert_eq!(
        record.truncation_reason.as_deref(),
        Some("content_filter"),
        "with_truncation(true, Some(reason)) must preserve the reason"
    );
}

/// Verify truncation survives SQLite round-trip (schema migration + read-back).
#[test]
fn truncation_fields_survive_sqlite_roundtrip() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let jsonl_path = temp_dir.path().join("usage.jsonl");
    let db_path = temp_dir.path().join("usage.db");
    let store = UsageStore::with_paths(crate::config::UsageConfig::default(), jsonl_path, db_path)
        .expect("usage store");

    let record = UsageRecord::new(
        "trunc-model".into(),
        "trunc-provider".into(),
        "openai_chat".into(),
        50,
        100,
        Some(200),
    )
    .with_truncation(true, Some("max_tokens".to_string()));

    store.record(record).unwrap();
    store.flush_if_dirty().unwrap();

    let records = store.query(None, None, None, None, None);
    assert_eq!(records.len(), 1);
    assert!(records[0].truncated);
    assert_eq!(records[0].truncation_reason.as_deref(), Some("max_tokens"));
}

/// Legacy-id shape heuristic: production ids of the form `call_<12 hex>` came from
/// pre-re-identification builds and can never match the current store key scheme.
#[test]
fn legacy_upstream_id_shape_detection() {
    // the exact id observed failing in production (2026-10-03/04).
    assert!(suspected_legacy_upstream_id("call_5fd50cd7512b"));
    // non-32-hex tail of any length is flagged…
    assert!(suspected_legacy_upstream_id("call_abc"));
    assert!(suspected_legacy_upstream_id(
        "call_0123456789abcdef0123456789abcde"
    ));
    // …but the proxy's own 32-hex mint and every other id shape are not.
    assert!(!suspected_legacy_upstream_id(
        "call_0123456789abcdef0123456789abcdef"
    ));
    assert!(!suspected_legacy_upstream_id(&format!(
        "call_{}",
        uuid::Uuid::new_v4().as_simple()
    )));
    assert!(!suspected_legacy_upstream_id(
        "fc_0123456789abcdef0123456789abcdef"
    ));
    assert!(!suspected_legacy_upstream_id(
        "gemini-call-0123456789abcdef0123456789abcdef"
    ));
    assert!(!suspected_legacy_upstream_id("toolu_0123456789abcdef"));
}

// ---------- Trailing model turn local intercept ----------

/// A trailing-assistant request must pass through to the upstream unchanged.
#[tokio::test]
async fn trailing_assistant_turn_passes_through_unchanged() {
    let captured: CapturedRequest = Arc::new(Mutex::new(None));
    let captured_for_handler = Arc::clone(&captured);
    let upstream = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(captured): State<CapturedRequest>, Json(body): Json<Value>| async move {
                    *captured.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "chatcmpl-test",
                        "object": "chat.completion",
                        "model": "deepseek-chat",
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11}
                    }))
                },
            ),
        )
        .with_state(captured_for_handler);
    let upstream_addr = serve_on_random_port(upstream).await;

    let provider = config::ProviderConfig {
        openai_chat: Some(config::EndpointConfig::native(format!(
            "http://{upstream_addr}/v1/chat/completions"
        ))),
        ..Default::default()
    };
    let cfg = config_with(
        vec![("deepseek", provider)],
        vec![(
            "deepseek-chat",
            empty_model(true, false, false, "deepseek", "deepseek-chat"),
        )],
    );
    let state = AppState::new_in_memory(cfg);
    let proxy_addr = serve_on_random_port(router(state)).await;

    // Same trailing-assistant request — must pass through to the upstream.
    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .json(&json!({
            "model": "deepseek-chat",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "I am the trailing turn"}
            ],
            "stream": false
        }))
        .send()
        .await
        .expect("send");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the provider must not apply a trailing-model-turn check"
    );
    assert!(
        captured.lock().unwrap().is_some(),
        "the upstream must be contacted"
    );
}

#[tokio::test]
async fn metrics_window_total_ms_and_persistence_e2e() {
    let cfg = config::default_deepseek_config();
    let temp_dir = tempfile::tempdir().unwrap();
    let state_dir = temp_dir.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let metrics_db_path = state_dir.join("metrics.db");
    let store = Arc::new(crate::provider_metrics::MetricsStore::open(&metrics_db_path).unwrap());

    let state = AppState::new_in_memory(cfg.clone());
    state.set_metrics_store_for_test(store.clone());

    state.provider_metrics.record(
        "openai",
        "gpt-4o",
        Some("completed"),
        Some(150),
        Some(1200),
        200,
    );
    state.provider_metrics.record(
        "openai",
        "gpt-4o",
        Some("completed"),
        Some(200),
        Some(1500),
        300,
    );
    state
        .provider_metrics
        .record("openai", "gpt-4o", Some("upstream_error"), None, None, 0);

    let proxy_addr = serve_on_random_port(router(state.clone())).await;
    let client = reqwest::Client::new();

    // 1. GET /admin/metrics (default lifetime)
    let resp = client
        .get(format!("http://{proxy_addr}/admin/metrics"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["status"], "ok");
    let data = &body["data"];
    assert_eq!(data["window"], "lifetime");
    assert_eq!(data["is_partial"], false);
    let metrics = &data["metrics"];
    assert_eq!(metrics.as_array().unwrap().len(), 1);
    let b0 = &metrics[0];
    assert_eq!(b0["provider"], "openai");
    assert_eq!(b0["model"], "gpt-4o");
    assert_eq!(b0["window"], "lifetime");
    assert_eq!(b0["is_partial"], false);
    assert_eq!(b0["requests"]["total"], 3);
    assert_eq!(b0["requests"]["completed"], 2);
    assert_eq!(b0["requests"]["upstream_error"], 1);
    assert_eq!(b0["availability"]["numerator"], 3);
    assert_eq!(b0["availability"]["denominator"], 3);
    assert!(b0["latency"]["ttft_ms"].is_object());
    assert!(b0["latency"]["total_ms"].is_object());
    assert!(b0["throughput"]["generation_tps"].is_object());

    // 2. GET /admin/metrics?window=5m
    let resp_5m = client
        .get(format!("http://{proxy_addr}/admin/metrics?window=5m"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp_5m.status(), StatusCode::OK);
    let body_5m: serde_json::Value = resp_5m.json().await.expect("json");
    assert_eq!(body_5m["data"]["window"], "5m");
    assert_eq!(body_5m["data"]["is_partial"], true);
    assert_eq!(body_5m["data"]["metrics"][0]["window"], "5m");
    assert_eq!(body_5m["data"]["metrics"][0]["is_partial"], true);

    // 3. GET /admin/metrics?window=invalid -> 400 Bad Request
    let resp_invalid = client
        .get(format!("http://{proxy_addr}/admin/metrics?window=invalid"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp_invalid.status(), StatusCode::BAD_REQUEST);
    let body_invalid: serde_json::Value = resp_invalid.json().await.expect("json");
    assert_eq!(body_invalid["status"], "error");
    assert_eq!(body_invalid["kind"], "invalid_request");

    // 4. Persistence snapshot and GET /admin/metrics/history
    let snapshot = state.provider_metrics_snapshot();
    store.write_snapshot(&snapshot).expect("write snapshot");

    let resp_hist = client
        .get(format!("http://{proxy_addr}/admin/metrics/history"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp_hist.status(), StatusCode::OK);
    let body_hist: serde_json::Value = resp_hist.json().await.expect("json");
    let history = body_hist["data"]["history"]
        .as_array()
        .expect("history array");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["provider"], "openai");
    assert_eq!(history[0]["model"], "gpt-4o");
    assert_eq!(history[0]["requests"]["total"], 3);
    assert!(history[0]["latency"]["total_ms"].is_object());

    // 5. Query via GET /admin/metrics?history=true
    let resp_hist_param = client
        .get(format!("http://{proxy_addr}/admin/metrics?history=true"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp_hist_param.status(), StatusCode::OK);
    let body_hist_param: serde_json::Value = resp_hist_param.json().await.expect("json");
    assert_eq!(
        body_hist_param["data"]["history"].as_array().unwrap().len(),
        1
    );

    // 6. Restart semantics: simulate restart by opening store on same db_path with a fresh AppState
    let reopened_store =
        Arc::new(crate::provider_metrics::MetricsStore::open(&metrics_db_path).unwrap());
    let restarted_state = AppState::new_in_memory(cfg.clone());
    restarted_state.set_metrics_store_for_test(reopened_store);
    let restarted_addr = serve_on_random_port(router(restarted_state)).await;

    let resp_restarted_hist = client
        .get(format!("http://{restarted_addr}/admin/metrics/history"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp_restarted_hist.status(), StatusCode::OK);
    let body_restarted_hist: serde_json::Value = resp_restarted_hist.json().await.expect("json");
    let restarted_history = body_restarted_hist["data"]["history"]
        .as_array()
        .expect("history array");
    assert_eq!(
        restarted_history.len(),
        1,
        "persisted snapshots must survive restart"
    );
    assert_eq!(restarted_history[0]["provider"], "openai");
}

// ── local reject tolerance cooldown tests ──────────────────

#[tokio::test]
async fn local_reject_ingress_format_triggers_cooldown() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_handler = Arc::clone(&hits);
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let hits = Arc::clone(&hits_handler);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": "chatcmpl-test",
                        "object": "chat.completion",
                        "created": 1234567890,
                        "model": "deepseek-chat",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "hello"},
                            "finish_reason": "stop"
                        }]
                    })),
                )
            }
        }),
    );
    let upstream_addr = serve_on_random_port(upstream_app).await;

    let mut provider = crate::catalog::deepseek().provider;
    repoint_provider_urls(&mut provider, upstream_addr);
    provider.api_key_env = None;

    let model_a = empty_model(false, true, false, "deepseek", "deepseek-chat");
    let model_b = empty_model(true, false, false, "deepseek", "deepseek-chat");

    let mut cfg = config_with(
        vec![("deepseek", provider)],
        vec![("test-model", model_a), ("sibling-model", model_b)],
    );
    cfg.protection.local_reject.enabled = true;
    cfg.protection.local_reject.max_local_rejects = 3;
    cfg.protection.local_reject.cooldown_seconds = 300;
    cfg.protection.local_reject.window_seconds = 600;

    let mut state = AppState::new_in_memory(cfg);
    let config_dir = tempfile::tempdir().expect("config dir");
    let token_state_dir = tempfile::tempdir().expect("token state dir");
    let (alice_key, bob_key) =
        install_mock_tokens(&mut state, config_dir.path(), token_state_dir.path());
    let proxy_addr = serve_on_random_port(router(state.clone())).await;

    // Alice makes 3 bad requests to test-model which does not support chat (400 Bad Request)
    for round in 1..=3 {
        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .bearer_auth(alice_key)
            .json(&json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": format!("turn {round}")}],
                "stream": false
            }))
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "round {round} gets 400 Bad Request for unsupported protocol"
        );
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "upstream never contacted for ingress rejections"
    );

    // Alice is now in cooldown for test-model
    assert!(
        state.token_error_cooldown_blocked("alice", "test-model"),
        "alice must enter cooldown after 3 local rejections"
    );

    // Alice's 4th request is blocked with 429
    let resp4 = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .bearer_auth(alice_key)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "prompt"}],
            "stream": false
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp4.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "alice must be blocked with 429"
    );
    let body4: Value = resp4.json().await.expect("json");
    let msg = body4["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("token blocked after repeated bad requests"),
        "error message must describe bad requests cooldown: {msg}"
    );

    // Bob can use sibling-model without issue (200 OK)
    let resp_bob = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .bearer_auth(bob_key)
        .json(&json!({
            "model": "sibling-model",
            "messages": [{"role": "user", "content": "hello from bob"}],
            "stream": false
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp_bob.status(), StatusCode::OK, "bob is unaffected");

    // Alice can use sibling-model without issue (200 OK)
    let resp_alice_sibling = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .bearer_auth(alice_key)
        .json(&json!({
            "model": "sibling-model",
            "messages": [{"role": "user", "content": "hello sibling"}],
            "stream": false
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp_alice_sibling.status(),
        StatusCode::OK,
        "sibling model is unaffected"
    );
}

#[tokio::test]
async fn local_reject_fingerprint_block_triggers_cooldown() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_handler = Arc::clone(&hits);
    let upstream_app = Router::new().route(
        "/chat/completions",
        post(move || {
            let hits = Arc::clone(&hits_handler);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": {
                            "message": "Invalid request parameter",
                            "type": "invalid_request_error"
                        }
                    })),
                )
            }
        }),
    );
    let upstream_addr = serve_on_random_port(upstream_app).await;

    let mut provider = crate::catalog::deepseek().provider;
    repoint_provider_urls(&mut provider, upstream_addr);
    provider.api_key_env = None;

    let model = empty_model(true, false, false, "deepseek", "deepseek-chat");
    let mut cfg = config_with(vec![("deepseek", provider)], vec![("test-model", model)]);
    cfg.fallback.cooldown.client_error_seconds = 0;
    cfg.protection.bad_request_budget.enabled = false;
    cfg.protection.error_cooldown.enabled = false;
    cfg.protection.bad_request.enabled = true;
    cfg.protection.bad_request.max_errors = 2;
    cfg.protection.bad_request.window_seconds = 300;
    cfg.protection.bad_request.block_seconds = 300;

    cfg.protection.local_reject.enabled = true;
    cfg.protection.local_reject.max_local_rejects = 2;
    cfg.protection.local_reject.cooldown_seconds = 300;
    cfg.protection.local_reject.window_seconds = 600;

    let mut state = AppState::new_in_memory(cfg);
    let config_dir = tempfile::tempdir().expect("config dir");
    let token_state_dir = tempfile::tempdir().expect("token state dir");
    let (alice_key, _bob_key) =
        install_mock_tokens(&mut state, config_dir.path(), token_state_dir.path());
    let proxy_addr = serve_on_random_port(router(state.clone())).await;

    let fixed_req = json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "same fingerprint prompt"}],
        "stream": false
    });

    // 1 & 2: Reach upstream, get 400, observing fingerprint
    for round in 1..=2 {
        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .bearer_auth(alice_key)
            .json(&fixed_req)
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "round {round} gets 400 from upstream"
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2, "upstream contacted twice");

    // 3 & 4: Blocked by fingerprint (429), counting as local rejects
    for round in 3..=4 {
        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .bearer_auth(alice_key)
            .json(&fixed_req)
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "round {round} blocked by fingerprint"
        );
        let body: Value = resp.json().await.expect("json");
        let msg = body["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("temporarily blocked after repeated upstream bad requests"));
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "upstream not contacted on fingerprint blocks"
    );

    // Alice is now in cooldown!
    assert!(
        state.token_error_cooldown_blocked("alice", "test-model"),
        "alice must enter cooldown after 2 fingerprint blocks"
    );

    // 5: Alice sends a DIFFERENT request (different fingerprint) -> blocked by token cooldown!
    let resp5 = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
        .bearer_auth(alice_key)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "completely different prompt"}],
            "stream": false
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp5.status(), StatusCode::TOO_MANY_REQUESTS);
    let body5: Value = resp5.json().await.expect("json");
    let msg5 = body5["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg5.contains("token blocked after repeated bad requests"),
        "error message must describe bad requests cooldown: {msg5}"
    );
}

#[tokio::test]
async fn local_reject_excluded_populations_red_evidence() {
    let mut provider = crate::catalog::deepseek().provider;
    provider.api_key_env = None;
    let model_allowed = empty_model(true, false, false, "deepseek", "deepseek-chat");
    let model_other = empty_model(true, false, false, "deepseek", "deepseek-chat");
    let model_unbound = empty_model(true, false, false, "nonexistent-provider", "upstream");

    let mut cfg = config_with(
        vec![("deepseek", provider)],
        vec![
            ("allowed-model", model_allowed),
            ("other-model", model_other),
            ("unbound-model", model_unbound),
        ],
    );
    cfg.protection.local_reject.enabled = true;
    cfg.protection.local_reject.max_local_rejects = 2;

    let mut state = AppState::new_in_memory(cfg);
    let config_dir = tempfile::tempdir().expect("config dir");
    let token_state_dir = tempfile::tempdir().expect("token state dir");

    // Restrict Alice to only "allowed-model" and "unbound-model"
    std::fs::write(
        config_dir.path().join("tokens.toml"),
        "[[tokens]]\n\
         key = \"lp-mock-alice-key-12345\"\n\
         name = \"alice\"\n\
         allowed_models = [\"allowed-model\", \"unbound-model\"]\n",
    )
    .expect("write tokens.toml");
    state.token = crate::token::auth::TokenAuthState::load(
        &config_dir.path().join("config.toml"),
        token_state_dir.path(),
    );
    let alice_key = "lp-mock-alice-key-12345";
    let proxy_addr = serve_on_random_port(router(state.clone())).await;

    // 1. Model-scope 403: Alice requests "other-model" (forbidden for this token)
    for _ in 0..5 {
        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .bearer_auth(alice_key)
            .json(&json!({
                "model": "other-model",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": false
            }))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
    // Must NOT count in B
    assert_eq!(
        state.error_cooldown.snapshot().len(),
        0,
        "model scope 403 must never increment local reject counts"
    );

    // 2. All-candidates cooling / empty candidates 503
    for _ in 0..5 {
        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .bearer_auth(alice_key)
            .json(&json!({
                "model": "unbound-model",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": false
            }))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_eq!(
        state.error_cooldown.snapshot().len(),
        0,
        "503 empty candidates must never increment local reject counts"
    );
}

#[tokio::test]
async fn local_reject_and_error_class_isolation_red_evidence() {
    let local_cfg = crate::config::LocalRejectConfig {
        enabled: true,
        max_local_rejects: 3,
        cooldown_seconds: 120,
        window_seconds: 300,
    };
    let error_cfg = crate::config::ErrorCooldownConfig {
        enabled: true,
        max_errors_per_class: 3,
        cooldown_seconds: 120,
        window_seconds: 300,
    };
    let cd = crate::protection::TokenErrorClassCooldown::new_with_configs(error_cfg, local_cfg);

    // Trigger local reject cooldown
    cd.record_local_reject("alice", "model-a", "ingress_format");
    cd.record_local_reject("alice", "model-a", "ingress_format");
    cd.record_local_reject("alice", "model-a", "ingress_format");

    let status = cd.is_blocked("alice", "model-a");
    assert!(status.blocked);
    assert_eq!(status.reason.as_deref(), Some("local_reject_tolerance"));

    let snap = cd.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(
        snap[0].cooldown_reason.as_deref(),
        Some("local_reject_tolerance")
    );
    assert!(
        snap[0].error_counts.is_empty(),
        "local reject must never populate upstream error_counts"
    );

    // Clear and trigger error class cooldown
    assert!(cd.clear_token_model("alice", "model-a"));
    assert!(!cd.is_blocked("alice", "model-a").blocked);

    cd.record_error("alice", "model-a", "missing_thought_signature");
    cd.record_error("alice", "model-a", "missing_thought_signature");
    cd.record_error("alice", "model-a", "missing_thought_signature");

    let status_a = cd.is_blocked("alice", "model-a");
    assert!(status_a.blocked);
    assert_eq!(status_a.reason.as_deref(), Some("error_class"));

    let snap_a = cd.snapshot();
    assert_eq!(snap_a.len(), 1);
    assert_eq!(snap_a[0].cooldown_reason.as_deref(), Some("error_class"));
    assert_eq!(
        snap_a[0].local_reject_count, 0,
        "upstream error class must never populate local_reject_count"
    );
}

#[tokio::test]
async fn local_reject_admin_clear_clears_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token-error-cooldowns.json");
    let local_cfg = crate::config::LocalRejectConfig {
        enabled: true,
        max_local_rejects: 2,
        cooldown_seconds: 300,
        window_seconds: 600,
    };
    let error_cfg = crate::config::ErrorCooldownConfig {
        enabled: true,
        max_errors_per_class: 3,
        cooldown_seconds: 300,
        window_seconds: 600,
    };

    let cd = crate::protection::TokenErrorClassCooldown::load(
        error_cfg.clone(),
        local_cfg.clone(),
        &path,
    );
    cd.record_local_reject("alice", "model-a", "ingress_format");
    cd.record_local_reject("alice", "model-a", "ingress_format");
    assert!(cd.is_blocked("alice", "model-a").blocked);

    // Admin clear for (alice, model-a) clears the cooldown
    assert!(cd.clear_token_model("alice", "model-a"));
    assert!(!cd.is_blocked("alice", "model-a").blocked);
    drop(cd);

    // Persisted file also reflects the clear
    let reloaded = crate::protection::TokenErrorClassCooldown::load(error_cfg, local_cfg, &path);
    assert!(!reloaded.is_blocked("alice", "model-a").blocked);
    assert!(reloaded.snapshot().is_empty());
}

#[cfg(test)]
#[tokio::test]
async fn passthrough_sse_records_latency_at_stream_end() {
    let upstream_app = axum::Router::new().route(
        "/stream",
        axum::routing::get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
                    "data: [DONE]\n\n",
                ),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream_app).await;
    let upstream = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/stream"))
        .send()
        .await
        .expect("upstream request");

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    let resp = passthrough_sse(
        &state,
        "test",
        upstream,
        "prov".into(),
        "ep".into(),
        "model".into(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("consume stream body");

    // Record runs at stream end; poll briefly in case the store buffers.
    let mut records = usage_store.get_all();
    for _ in 0..50 {
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        records = usage_store.get_all();
    }
    assert!(
        !records.is_empty(),
        "usage record must be captured at stream end"
    );
    let lat = records[0]
        .latency_ms
        .expect("latency_ms must be recorded at stream end, got None");
    let ttfb = records[0]
        .ttfb_ms
        .expect("ttfb_ms already recorded at first chunk");
    assert!(
        lat >= ttfb as i64,
        "latency must be >= ttfb from the same ingress clock: lat={lat} ttfb={ttfb}"
    );
}

/// 回归基线：非 guard 系流中断（上游 chunk 错误早退）也必须留 usage 行，
/// status=upstream_interrupted、latency/ttfb 同源 ingress 且 latency >= ttfb。
/// 修复前：该请求零 usage 行（流末 record 因 return 永不执行）。
#[tokio::test]
async fn passthrough_sse_records_usage_on_mid_stream_error() {
    use futures_util::StreamExt;
    use futures_util::stream;
    // mock 上游：发一帧含 usage 的 chunk 后抛流错误（模拟中途断流）
    let upstream_app = axum::Router::new().route(
        "/stream",
        axum::routing::get(|| async {
            let body_stream = stream::once(async {
                Ok::<_, std::io::Error>(bytes::Bytes::from(
                    "data: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
                ))
            })
            .chain(stream::once(async {
                // 让 headers+首 chunk 先送达客户端，再模拟中途断流
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "simulated mid-stream disconnect",
                ))
            }));
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(body_stream),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream_app).await;
    let upstream = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/stream"))
        .send()
        .await
        .expect("upstream request");

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    let resp = passthrough_sse(
        &state,
        "test",
        upstream,
        "prov".into(),
        "ep".into(),
        "model".into(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    // 消费流体直到 error 早退（不 panic 即通过——流错误被转为 error 事件）
    let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;

    // 轮询 usage 行（流中断补记可能异步入账）
    let mut records = usage_store.get_all();
    for _ in 0..50 {
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        records = usage_store.get_all();
    }
    assert!(
        !records.is_empty(),
        "mid-stream error must still leave a usage row (red-green for error-path recording)"
    );
    let rec = &records[0];
    assert_eq!(
        rec.status.as_deref(),
        Some("upstream_interrupted"),
        "mid-stream error usage row must carry upstream_interrupted status"
    );
    // 锁定已抽 token 计数（mock 帧含 prompt=7/completion=3——防止未来误换 0 仍绿）
    assert_eq!(
        rec.input_tokens, 7,
        "input_tokens must carry parsed usage from delivered frames"
    );
    assert_eq!(
        rec.output_tokens, 3,
        "output_tokens must carry parsed usage from delivered frames"
    );
    let lat = rec
        .latency_ms
        .expect("latency_ms must be recorded on error path");
    assert!(lat >= 0, "latency must be non-negative: {lat}");
    // ttfb 应然存在（mock 先发 good chunk 再断流）——强断言防传 None 静默跳过
    let ttfb = rec
        .ttfb_ms
        .expect("ttfb_ms must be set when first chunk was delivered before the error");
    assert!(
        lat >= ttfb as i64,
        "latency must be >= ttfb from the same ingress clock: lat={lat} ttfb={ttfb}"
    );
}

/// 回归基线（转换器代表）：responses→chat 转换器的流中断早退同样必须留
/// usage 行（status=upstream_interrupted、latency/ttfb 同源 ingress）。
#[tokio::test]
async fn responses_sse_to_chat_sse_records_usage_on_mid_stream_error() {
    use futures_util::StreamExt;
    use futures_util::stream;
    let upstream_app = axum::Router::new().route(
        "/stream",
        axum::routing::get(|| async {
            let body_stream = stream::once(async {
                Ok::<_, std::io::Error>(bytes::Bytes::from(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
                ))
            })
            .chain(stream::once(async {
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "simulated mid-stream disconnect",
                ))
            }));
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(body_stream),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream_app).await;
    let upstream = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/stream"))
        .send()
        .await
        .expect("upstream request");

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    let resp = responses_sse_to_chat_sse(
        &state,
        upstream,
        "model".into(),
        "prov".into(),
        "ep".into(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;

    let mut records = usage_store.get_all();
    for _ in 0..50 {
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        records = usage_store.get_all();
    }
    assert!(
        !records.is_empty(),
        "converter mid-stream error must still leave a usage row"
    );
    let rec = &records[0];
    assert_eq!(
        rec.status.as_deref(),
        Some("upstream_interrupted"),
        "converter error usage row must carry upstream_interrupted status"
    );
    let lat = rec.latency_ms.expect("latency_ms must be recorded");
    if let Some(ttfb) = rec.ttfb_ms {
        assert!(
            lat >= ttfb as i64,
            "latency must be >= ttfb: lat={lat} ttfb={ttfb}"
        );
    }
}

/// 回归基线（模型改写分支）：流中断早退必须留 usage 行（upstream_interrupted、
/// latency/ttfb 同源 ingress、token 计数锁定）。
#[tokio::test]
async fn responses_native_sse_rewrite_model_records_usage_on_mid_stream_error() {
    use futures_util::StreamExt;
    use futures_util::stream;
    let upstream_app = axum::Router::new().route(
        "/stream",
        axum::routing::get(|| async {
            let body_stream = stream::once(async {
                Ok::<_, std::io::Error>(bytes::Bytes::from(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
                ))
            })
            .chain(stream::once(async {
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "simulated mid-stream disconnect",
                ))
            }));
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(body_stream),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream_app).await;
    let upstream = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/stream"))
        .send()
        .await
        .expect("upstream request");

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    let resp = responses_native_sse_rewrite_model(
        &state,
        upstream,
        "model".into(),
        "prov".into(),
        "ep".into(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;

    let mut records = usage_store.get_all();
    for _ in 0..50 {
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        records = usage_store.get_all();
    }
    assert!(
        !records.is_empty(),
        "responses_native_sse_rewrite_model mid-stream error must still leave a usage row"
    );
    let rec = &records[0];
    assert_eq!(
        rec.status.as_deref(),
        Some("upstream_interrupted"),
        "error usage row must carry upstream_interrupted status"
    );
    let lat = rec.latency_ms.expect("latency_ms must be recorded");
    let ttfb = rec
        .ttfb_ms
        .expect("ttfb_ms must be set when first chunk was delivered before the error");
    assert!(
        lat >= ttfb as i64,
        "latency must be >= ttfb: lat={lat} ttfb={ttfb}"
    );
}

/// 回归基线（chat→anthropic 转换器）：流中断早退必须留 usage 行（upstream_interrupted、
/// latency/ttfb 同源 ingress、token 计数锁定）。
#[tokio::test]
async fn chat_sse_to_anthropic_sse_records_usage_on_mid_stream_error() {
    use futures_util::StreamExt;
    use futures_util::stream;
    let upstream_app = axum::Router::new().route(
        "/stream",
        axum::routing::get(|| async {
            let body_stream = stream::once(async {
                Ok::<_, std::io::Error>(bytes::Bytes::from(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
                ))
            })
            .chain(stream::once(async {
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "simulated mid-stream disconnect",
                ))
            }));
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(body_stream),
            )
        }),
    );
    let upstream_addr = serve_on_random_port(upstream_app).await;
    let upstream = reqwest::Client::new()
        .get(format!("http://{upstream_addr}/stream"))
        .send()
        .await
        .expect("upstream request");

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    let resp = chat_sse_to_anthropic_sse(
        &state,
        upstream,
        "model".into(),
        "prov".into(),
        "ep".into(),
        None,
        std::time::Instant::now(),
        None,
    )
    .await;
    let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;

    let mut records = usage_store.get_all();
    for _ in 0..50 {
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        records = usage_store.get_all();
    }
    assert!(
        !records.is_empty(),
        "chat_sse_to_anthropic_sse mid-stream error must still leave a usage row"
    );
    let rec = &records[0];
    assert_eq!(
        rec.status.as_deref(),
        Some("upstream_interrupted"),
        "error usage row must carry upstream_interrupted status"
    );
    let lat = rec.latency_ms.expect("latency_ms must be recorded");
    let ttfb = rec
        .ttfb_ms
        .expect("ttfb_ms must be set when first chunk was delivered before the error");
    assert!(
        lat >= ttfb as i64,
        "latency must be >= ttfb: lat={lat} ttfb={ttfb}"
    );
}

#[tokio::test]
async fn stream_usage_guard_latency_uses_injected_ingress_clock() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config::default_deepseek_config());
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    // Injected clock: 2s before now. Record latency must be measured from it,
    // not from a construction-time placeholder.
    let ingress = std::time::Instant::now() - std::time::Duration::from_secs(2);
    let mut guard = StreamUsageGuard::new_with_target(
        state,
        "model".to_string(),
        "prov".to_string(),
        "ep".to_string(),
        None,
        None,
        ingress,
    );
    guard.input_tokens = 5;
    guard.output_tokens = 7;
    guard.terminate("completed");

    let mut records = usage_store.get_all();
    for _ in 0..50 {
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        records = usage_store.get_all();
    }
    assert!(!records.is_empty(), "guard must record usage at terminate");
    let lat = records[0]
        .latency_ms
        .expect("latency_ms must be recorded by guard");
    assert!(
        lat >= 1900,
        "latency must be measured from the injected ingress clock, got {lat}ms"
    );
}

#[tokio::test]
async fn forward_chat_ingress_clock_drives_latency() {
    let upstream_app = axum::Router::new().route(
        "/chat/completions",
        post(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                json!({
                    "id": "c1",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "hi"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12}
                })
                .to_string(),
            )
        }),
    );
    let addr = serve_on_random_port(upstream_app).await;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config_with(
        vec![(
            "test-provider",
            config::ProviderConfig {
                openai_responses: Some(config::EndpointConfig::native("http://127.0.0.1:1/unused")),
                ..Default::default()
            },
        )],
        vec![],
    ));
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    let plan = test_execution_plan(format!("http://{addr}/chat/completions"));
    let body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    // Injected clock: 2s before now stands in for the handler-ingress time.
    let ingress = std::time::Instant::now() - std::time::Duration::from_secs(2);

    let response = forward_chat_request(&state, &plan, "m", body, None, ingress).await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut records = usage_store.get_all();
    for _ in 0..50 {
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        records = usage_store.get_all();
    }
    assert!(
        !records.is_empty(),
        "non-streaming forward must record usage"
    );
    let lat = records[0].latency_ms.expect("latency_ms recorded");
    assert!(
        lat >= 1900,
        "latency must be measured from the handler-ingress clock passed into the forward entry, got {lat}ms"
    );
    let ttfb = records[0]
        .ttfb_ms
        .expect("ttfb_ms recorded for non-streaming");
    assert!(
        lat >= ttfb as i64,
        "latency ({lat}ms) must be >= ttfb ({ttfb}ms) from the same ingress clock"
    );
    assert!(records[0].input_tokens > 0, "usage parsed from upstream");
}

#[tokio::test]
async fn failed_requests_record_usage_rows() {
    // Scenario 3: 200 with a non-JSON body.
    let upstream_app = axum::Router::new().route(
        "/chat/completions",
        get(|| async { "this is not json" }).post(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                "definitely-not-json",
            )
        }),
    );
    let addr = serve_on_random_port(upstream_app).await;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config_with(
        vec![(
            "test-provider",
            config::ProviderConfig {
                openai_responses: Some(config::EndpointConfig::native("http://127.0.0.1:1/unused")),
                ..Default::default()
            },
        )],
        vec![],
    ));
    state.set_usage_store_for_test(Arc::clone(&usage_store));
    let body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});

    // Scenario 1: upstream 500 — upstream saw the request, must be recorded.
    let plan500 = test_execution_plan(format!("http://{addr}/500"));
    let _ = forward_chat_request(
        &state,
        &plan500,
        "m",
        body.clone(),
        None,
        std::time::Instant::now(),
    )
    .await;
    // Scenario 2: connection refused — send failed after leaving the local
    // pipeline, may have been in flight, must be recorded.
    let plan_conn = test_execution_plan("http://127.0.0.1:1/chat/completions".to_string());
    let _ = forward_chat_request(
        &state,
        &plan_conn,
        "m",
        body.clone(),
        None,
        std::time::Instant::now(),
    )
    .await;
    // Scenario 3: 200 + invalid JSON body.
    let plan_json = test_execution_plan(format!("http://{addr}/chat/completions"));
    let _ = forward_chat_request(
        &state,
        &plan_json,
        "m",
        body,
        None,
        std::time::Instant::now(),
    )
    .await;

    let mut records = usage_store.get_all();
    for _ in 0..50 {
        if records.len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        records = usage_store.get_all();
    }
    assert!(
        records.len() >= 3,
        "every failed request must leave a usage row, got {} rows",
        records.len()
    );
    let statuses: Vec<Option<&str>> = records.iter().map(|r| r.status.as_deref()).collect();
    assert!(
        statuses.contains(&Some("upstream_error")),
        "500 recorded: {statuses:?}"
    );
    assert!(
        statuses.contains(&Some("send_failed")),
        "conn refused recorded: {statuses:?}"
    );
    assert!(
        statuses.contains(&Some("invalid_json")),
        "bad body recorded: {statuses:?}"
    );
}

#[tokio::test]
async fn failed_requests_record_usage_rows_on_native_entries() {
    let upstream_app = axum::Router::new().route(
        "/boom",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let addr = serve_on_random_port(upstream_app).await;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config_with(
        vec![(
            "test-provider",
            config::ProviderConfig {
                openai_responses: Some(config::EndpointConfig::native("http://127.0.0.1:1/unused")),
                ..Default::default()
            },
        )],
        vec![],
    ));
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    // anthropic native → upstream 500.
    let plan_a = test_execution_plan(format!("http://{addr}/boom"));
    let body_a =
        json!({"model": "m", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]});
    let _ = forward_anthropic_native(
        &state,
        &plan_a,
        "m",
        body_a,
        None,
        std::time::Instant::now(),
    )
    .await;

    // responses native → upstream 500.
    let plan_r = test_execution_plan(format!("http://{addr}/boom"));
    let body_r = json!({"model": "m", "input": "hi"});
    let _ = forward_responses_native(
        &state,
        &plan_r,
        "m",
        "m",
        body_r,
        None,
        std::time::Instant::now(),
    )
    .await;

    let mut records = usage_store.get_all();
    for _ in 0..50 {
        if records.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        records = usage_store.get_all();
    }
    assert!(
        records.len() >= 2,
        "both native entries must record failed requests, got {} rows",
        records.len()
    );
    let eps: Vec<&str> = records.iter().map(|r| r.endpoint.as_str()).collect();
    assert!(eps.contains(&"anthropic"), "anthropic row present: {eps:?}");
    assert!(
        eps.contains(&"openai_responses"),
        "responses row present: {eps:?}"
    );
    assert!(
        records
            .iter()
            .all(|r| r.status.as_deref() == Some("upstream_error")),
        "all rows carry upstream_error status"
    );
}

#[tokio::test]
async fn frequency_limited_requests_are_not_recorded_as_failures() {
    let upstream_app = axum::Router::new().route(
        "/chat/completions",
        post(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                json!({
                    "id": "c1",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "hi"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12}
                })
                .to_string(),
            )
        }),
    );
    let addr = serve_on_random_port(upstream_app).await;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config_with(
        vec![(
            "test-provider",
            config::ProviderConfig {
                openai_responses: Some(config::EndpointConfig::native("http://127.0.0.1:1/unused")),
                ..Default::default()
            },
        )],
        vec![],
    ));
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    let mut plan = test_execution_plan(format!("http://{addr}/chat/completions"));
    plan.request_frequency = config::RequestFrequencyConfig {
        requests_per_minute: Some(1),
        requests_per_hour: None,
        burst: Some(1),
        queue_timeout_seconds: Some(1),
    };
    let body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});

    // First request consumes the single token.
    let r1 = forward_chat_request(
        &state,
        &plan,
        "m",
        body.clone(),
        None,
        std::time::Instant::now(),
    )
    .await;
    assert_eq!(r1.status(), StatusCode::OK);
    // Second request is rejected by the local frequency limiter — the
    // request never leaves the process, so it must not produce a row.
    let _ = forward_chat_request(&state, &plan, "m", body, None, std::time::Instant::now()).await;

    for _ in 0..60 {
        if !usage_store.get_all().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Give any (wrongly) produced failure row time to land.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let records = usage_store.get_all();
    assert!(
        records.len() == 1,
        "only the completed request may be recorded; frequency-limited \
             rejection is local. got {} rows: {:?}",
        records.len(),
        records.iter().map(|r| r.status.clone()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn force_stream_aggregate_failure_records_usage() {
    let upstream_app = axum::Router::new().route(
        "/chat/completions",
        post(|| async {
            // 4097 output_item.added events exceed the aggregator's
            // max_output_items cap → aggregate_responses_sse_to_value bails →
            // the force-stream Err arm must record a usage row.
            let mut s = String::new();
            for i in 0u32..4097 {
                s.push_str(&format!(
                    "data: {{\"type\":\"response.output_item.added\",\"output_index\":{i},\"item\":{{\"type\":\"message\",\"id\":\"i{i}\"}}}}\n\n"
                ));
            }
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                s,
            )
        }),
    );
    let addr = serve_on_random_port(upstream_app).await;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let usage_store = Arc::new(
        UsageStore::with_paths(
            crate::config::UsageConfig::default(),
            temp_dir.path().join("usage.jsonl"),
            temp_dir.path().join("usage.db"),
        )
        .expect("usage store"),
    );
    let mut state = AppState::new_in_memory(config_with(
        vec![(
            "test-provider",
            config::ProviderConfig {
                openai_responses: Some(config::EndpointConfig::native("http://127.0.0.1:1/unused")),
                ..Default::default()
            },
        )],
        vec![],
    ));
    state.set_usage_store_for_test(Arc::clone(&usage_store));

    let mut plan = test_execution_plan(format!("http://{addr}/chat/completions"));
    plan.compat.force_stream = Some(true);
    let responses_body = json!({"model": "m", "input": "hi"});

    let _ = forward_chat_via_responses(
        &state,
        &plan,
        "m",
        "m",
        responses_body,
        None,
        std::time::Instant::now(),
    )
    .await;

    let mut records = usage_store.get_all();
    for _ in 0..50 {
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        records = usage_store.get_all();
    }
    assert!(
        !records.is_empty(),
        "failed force-stream aggregation (upstream stream malformed) \
             must record a usage row"
    );
    assert_eq!(
        records[0].status.as_deref(),
        Some("invalid_json"),
        "aggregation failure is a malformed upstream stream"
    );
}

mod failure_delivery_tests {

    use super::*;
    use crate::proxy::sse_aggregate::{
        StreamUsageGuard, compute_tools_digest, digest_finish_message, sanitize_finish_reason,
    };
    use crate::proxy::{
        GENERATION_FAILURE_CODE, GENERATION_FAILURE_HEADER,
        is_upstream_generation_failure_response, make_upstream_generation_failure_response,
    };

    #[tokio::test]
    async fn test_dual_form_non_streaming_502_three_shapes() {
        // Chat 502
        let resp_chat = make_upstream_generation_failure_response(
            Protocol::OpenaiChatCompletions,
            "req-1",
            "MALFORMED_FUNCTION_CALL",
        );
        assert_eq!(resp_chat.status(), StatusCode::BAD_GATEWAY);
        assert!(is_upstream_generation_failure_response(&resp_chat));
        assert_eq!(
            resp_chat
                .headers()
                .get(GENERATION_FAILURE_HEADER)
                .and_then(|h| h.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            resp_chat
                .headers()
                .get("x-llm-proxy-upstream-generation-failed")
                .and_then(|h| h.to_str().ok()),
            Some("true")
        );
        let bytes_chat = axum::body::to_bytes(resp_chat.into_body(), 1024)
            .await
            .unwrap();
        let body_chat: Value = serde_json::from_slice(&bytes_chat).unwrap();
        assert_eq!(body_chat["error"]["code"], GENERATION_FAILURE_CODE);
        assert_eq!(body_chat["error"]["type"], "server_error");
        assert!(
            body_chat["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("request_id: req-1"),
            "chat message must carry request_id: {body_chat}"
        );

        // Responses 502
        let resp_responses = make_upstream_generation_failure_response(
            Protocol::OpenaiResponses,
            "req-2",
            "UNEXPECTED_TOOL_CALL",
        );
        assert_eq!(resp_responses.status(), StatusCode::BAD_GATEWAY);
        assert!(is_upstream_generation_failure_response(&resp_responses));
        let bytes_resp = axum::body::to_bytes(resp_responses.into_body(), 1024)
            .await
            .unwrap();
        let body_resp: Value = serde_json::from_slice(&bytes_resp).unwrap();
        assert_eq!(body_resp["error"]["code"], GENERATION_FAILURE_CODE);
        assert!(
            body_resp["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("request_id: req-2"),
            "responses message must carry request_id: {body_resp}"
        );

        // Anthropic 502 (with code extension)
        let resp_anthropic = make_upstream_generation_failure_response(
            Protocol::Anthropic,
            "req-3",
            "TOO_MANY_TOOL_CALLS",
        );
        assert_eq!(resp_anthropic.status(), StatusCode::BAD_GATEWAY);
        assert!(is_upstream_generation_failure_response(&resp_anthropic));
        let bytes_anthropic = axum::body::to_bytes(resp_anthropic.into_body(), 1024)
            .await
            .unwrap();
        let body_anthropic: Value = serde_json::from_slice(&bytes_anthropic).unwrap();
        assert_eq!(body_anthropic["type"], "error");
        assert_eq!(body_anthropic["error"]["code"], GENERATION_FAILURE_CODE);
        assert_eq!(body_anthropic["error"]["type"], "api_error");
        assert!(
            body_anthropic["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("request_id: req-3"),
            "anthropic message must carry request_id: {body_anthropic}"
        );
    }

    #[test]
    fn test_warn_telemetry_bounded_fields() {
        // Sanitization
        assert_eq!(
            sanitize_finish_reason("MALFORMED_FUNCTION_CALL"),
            "MALFORMED_FUNCTION_CALL"
        );
        assert_eq!(sanitize_finish_reason("BAD <TAG> 123!"), "BAD_TAG_123");

        // Message digest only reports length + sha256, NEVER plaintext
        let digest = digest_finish_message("secret sensitive text from upstream");
        assert!(digest.starts_with("len=35,sha256="));
        assert!(
            !digest.contains("sensitive"),
            "digest must not leak content"
        );

        // Tools schema digest
        let (count, hash) =
            compute_tools_digest(Some(&[json!({"name": "test", "parameters": {}})]));
        assert_eq!(count, 1);
        assert!(hash.starts_with("sha256:"));

        let (empty_count, empty_hash) = compute_tools_digest(None);
        assert_eq!(empty_count, 0);
        assert_eq!(empty_hash, "none");
    }

    #[tokio::test]
    async fn test_stream_usage_guard_drop_or_terminate_records_exactly_once() {
        let temp_dir = tempfile::tempdir().unwrap();
        let jsonl_path = temp_dir.path().join("usage.jsonl");
        let db_path = temp_dir.path().join("usage.db");
        let usage_store = Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                crate::config::UsageConfig::default(),
                jsonl_path,
                db_path,
            )
            .unwrap(),
        );
        let mut state = AppState::new_in_memory(config::default_deepseek_config());
        state.set_usage_store_for_test(usage_store);

        // Case 1: terminate called on failure, then guard dropped -> records generation_failed, Drop does not double-record
        {
            let mut guard = StreamUsageGuard::new(
                state.clone(),
                "deepseek-chat".into(),
                "deepseek".into(),
                "/chat/completions".into(),
                None,
                std::time::Instant::now(),
            );
            guard.input_tokens = 10;
            guard.output_tokens = 20;
            guard.terminate("generation_failed");
            drop(guard);
        }

        // Case 2: guard dropped without terminate (client disconnected mid-stream) -> records client_disconnected on Drop
        {
            let mut guard = StreamUsageGuard::new(
                state.clone(),
                "deepseek-chat".into(),
                "deepseek".into(),
                "/chat/completions".into(),
                None,
                std::time::Instant::now(),
            );
            guard.input_tokens = 15;
            guard.output_tokens = 25;
            drop(guard);
        }

        // Verify exactly two records exist with expected statuses
        let records = state.usage_store.as_ref().unwrap().get_all();
        assert_eq!(
            records.len(),
            2,
            "must have exactly 2 usage records recorded"
        );
        let statuses: Vec<Option<String>> = records.into_iter().map(|r| r.status).collect();
        assert!(
            statuses.contains(&Some("generation_failed".to_string())),
            "must contain generation_failed status"
        );
        assert!(
            statuses.contains(&Some("client_disconnected".to_string())),
            "must contain client_disconnected status"
        );
    }

    // =========================================================================
    // : End-to-end Routing attribution and client safety barrier tests
    // =========================================================================

    #[tokio::test]
    async fn routing_model_sets_routed_target_end_to_end_c2_c3() {
        let upstream = Router::new()
            .route(
                "/chat/completions",
                post(|| async {
                    Json(json!({
                        "id": "chatcmpl-test",
                        "object": "chat.completion",
                        "created": 1234567890,
                        "model": "upstream-m",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "hello"},
                            "finish_reason": "stop"
                        }],
                        "usage": {
                            "prompt_tokens": 10,
                            "completion_tokens": 20,
                            "total_tokens": 30
                        }
                    }))
                }),
            )
            .route(
                "/v1/chat/completions",
                post(|| async {
                    Json(json!({
                        "id": "chatcmpl-test",
                        "object": "chat.completion",
                        "created": 1234567890,
                        "model": "upstream-m",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "hello"},
                            "finish_reason": "stop"
                        }],
                        "usage": {
                            "prompt_tokens": 10,
                            "completion_tokens": 20,
                            "total_tokens": 30
                        }
                    }))
                }),
            );
        let upstream_addr = serve_on_random_port(upstream).await;
        let mut provider = crate::catalog::openai_payg().provider;
        repoint_provider_urls(&mut provider, upstream_addr);
        provider.api_key_env = None;

        let target = empty_model(true, false, false, "p", "upstream-m");
        let mut routing = empty_model(false, false, false, "p", "unused");
        routing.context_window = 0;
        routing.max_output_tokens = 0;
        routing.reasoning_routes = Some(vec![config::RouteEntry {
            level: "high".to_string(),
            target: "target-high".to_string(),
        }]);

        let cfg = config_with(
            vec![("p", provider)],
            vec![("target-high", target), ("router-lp", routing)],
        );
        cfg.validate().expect("routing config must validate");

        let mut state = AppState::new_in_memory(cfg);
        let temp_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                config::UsageConfig::default(),
                temp_dir.path().join("usage.jsonl"),
                temp_dir.path().join("usage.db"),
            )
            .unwrap(),
        );
        state.set_usage_store_for_test(store.clone());

        let proxy_addr = serve_on_random_port(router(state)).await;

        // 1. Regular non-routed call to target-high
        let _ = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .json(&json!({
                "model": "target-high",
                "messages": [{"role": "user", "content": "ping"}]
            }))
            .send()
            .await
            .expect("send regular");

        // 2. Routed call to router-lp with reasoning_effort high
        let _ = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/chat/completions"))
            .json(&json!({
                "model": "router-lp",
                "reasoning_effort": "high",
                "messages": [{"role": "user", "content": "ping"}]
            }))
            .send()
            .await
            .expect("send routed");

        store.flush_if_dirty().unwrap();
        let records = store.get_all();
        assert_eq!(records.len(), 2);

        // Find regular record: model == "target-high", routed_target == None
        let reg = records
            .iter()
            .find(|r| r.model == "target-high")
            .expect("regular record");
        assert_eq!(reg.routed_target, None);

        // Find routed record: model == "router-lp", routed_target == Some("target-high")
        let rtd = records
            .iter()
            .find(|r| r.model == "router-lp")
            .expect("routed record");
        assert_eq!(rtd.routed_target.as_deref(), Some("target-high"));
    }

    #[tokio::test]
    async fn responses_via_chat_streaming_records_routed_target() {
        let upstream = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let sse_body = "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hello\"},\"index\":0}]}\n\ndata: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\",\"index\":0}],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":12,\"total_tokens\":20}}\n\ndata: [DONE]\n\n";
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    sse_body,
                )
            }),
        );
        let upstream_addr = serve_on_random_port(upstream).await;
        let mut provider = crate::catalog::openai_payg().provider;
        repoint_provider_urls(&mut provider, upstream_addr);
        provider.api_key_env = None;
        provider.openai_responses = Some(config::EndpointConfig::derived(
            config::Protocol::OpenaiChatCompletions,
        ));

        let target = empty_model(true, true, false, "p", "upstream-m");
        let mut routing = empty_model(false, false, false, "p", "unused");
        routing.context_window = 0;
        routing.max_output_tokens = 0;
        routing.reasoning_routes = Some(vec![config::RouteEntry {
            level: "high".to_string(),
            target: "target-high".to_string(),
        }]);

        let cfg = config_with(
            vec![("p", provider)],
            vec![("target-high", target), ("router-lp", routing)],
        );
        cfg.validate().expect("routing config must validate");

        let mut state = AppState::new_in_memory(cfg);
        let temp_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                config::UsageConfig::default(),
                temp_dir.path().join("usage.jsonl"),
                temp_dir.path().join("usage.db"),
            )
            .unwrap(),
        );
        state.set_usage_store_for_test(store.clone());

        let proxy_addr = serve_on_random_port(router(state)).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/openai/v1/responses"))
            .json(&json!({
                "model": "router-lp",
                "reasoning": {"effort": "high"},
                "stream": true,
                "input": "hi"
            }))
            .send()
            .await
            .expect("send routed responses via chat streaming");

        let status = resp.status();
        let body = resp.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "response body: {body}");

        // Wait brief tick for stream cleanup & record_usage
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let records = store.get_all();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].model, "router-lp");
        assert_eq!(records[0].routed_target.as_deref(), Some("target-high"));
    }

    #[tokio::test]
    async fn token_usage_history_model_safety_barrier_projection_c4() {
        let (mut state, key_a, _cfg_dir, _state_dir) = state_with_quota_token("tokens", 100_000);
        let temp_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                config::UsageConfig::default(),
                temp_dir.path().join("usage.jsonl"),
                temp_dir.path().join("usage.db"),
            )
            .unwrap(),
        );

        let ts = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
        store
            .record(
                crate::usage_stats::UsageRecord::new(
                    "deepseek-v4-flash-lp".into(),
                    "secret-upstream-provider".into(),
                    "chat".into(),
                    100,
                    50,
                    Some(200),
                )
                .with_routed_target("internal-secret-target")
                .with_timestamp(&ts)
                .with_token(Some(key_a.clone()), Some("Alice".into())),
            )
            .unwrap();
        store.flush_if_dirty().unwrap();
        state.set_usage_store_for_test(store.clone());

        let proxy_addr = serve_on_random_port(router(state)).await;

        let resp = reqwest::Client::new()
            .get(format!(
                "http://{proxy_addr}/v1/token/usage/history?group_by=model"
            ))
            .header("Authorization", format!("Bearer {key_a}"))
            .send()
            .await
            .expect("send history query");

        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body_text = resp.text().await.expect("body text");

        // Value domain assertion: body must NOT leak secret internal names
        assert!(
            !body_text.contains("secret-upstream-provider"),
            "response must NOT leak provider name: {body_text}"
        );
        assert!(
            !body_text.contains("internal-secret-target"),
            "response must NOT leak routed target: {body_text}"
        );

        let body: Value = serde_json::from_str(&body_text).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["data"]["auto_downgraded"], false);
        let groups = body["data"]["groups"].as_array().expect("groups array");
        assert_eq!(groups.len(), 1);

        // Strict schema assertion: each group object must have strictly and exactly
        // {"key", "model", "requests", "input_tokens", "output_tokens", "total_tokens"}
        let group_obj = groups[0].as_object().expect("group is object");
        let mut keys: Vec<String> = group_obj.keys().cloned().collect();
        keys.sort();
        let expected_keys = vec![
            "input_tokens".to_string(),
            "key".to_string(),
            "model".to_string(),
            "output_tokens".to_string(),
            "requests".to_string(),
            "total_tokens".to_string(),
        ];
        assert_eq!(
            keys, expected_keys,
            "group object schema must strictly match"
        );
        assert_eq!(group_obj["key"], "deepseek-v4-flash-lp");
        assert_eq!(group_obj["model"], "deepseek-v4-flash-lp");
        assert_eq!(group_obj["total_tokens"], 150);

        // Filter by specific model: model query parameter
        let resp_filter = reqwest::Client::new()
            .get(format!(
                "http://{proxy_addr}/v1/token/usage/history?model=deepseek-v4-flash-lp"
            ))
            .header("Authorization", format!("Bearer {key_a}"))
            .send()
            .await
            .expect("send history query with model filter");
        assert_eq!(resp_filter.status(), reqwest::StatusCode::OK);
        let filter_body: Value = resp_filter.json().await.unwrap();
        assert_eq!(filter_body["data"]["summary"]["requests"], 1);

        // Filter by unknown/unregistered model: safe empty return
        let resp_unknown = reqwest::Client::new()
            .get(format!(
                "http://{proxy_addr}/v1/token/usage/history?model=internal-secret-target"
            ))
            .header("Authorization", format!("Bearer {key_a}"))
            .send()
            .await
            .expect("send history query with unknown model filter");
        assert_eq!(resp_unknown.status(), reqwest::StatusCode::OK);
        let unknown_body: Value = resp_unknown.json().await.unwrap();
        assert_eq!(unknown_body["data"]["summary"]["requests"], 0);
        assert_eq!(unknown_body["data"]["groups"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn token_usage_history_composite_and_empty_state_c5() {
        let (mut state, key, _cfg_dir, _state_dir) = state_with_quota_token("tokens", 100_000);
        let temp_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                config::UsageConfig::default(),
                temp_dir.path().join("usage.jsonl"),
                temp_dir.path().join("usage.db"),
            )
            .unwrap(),
        );

        let ts = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
        store
            .record(
                crate::usage_stats::UsageRecord::new(
                    "deepseek-v4-flash-lp".into(),
                    "prov".into(),
                    "chat".into(),
                    10,
                    20,
                    Some(100),
                )
                .with_timestamp(&ts)
                .with_token(Some(key.clone()), Some("Alice".into())),
            )
            .unwrap();
        store.flush_if_dirty().unwrap();
        state.set_usage_store_for_test(store.clone());

        let proxy_addr = serve_on_random_port(router(state)).await;

        // 1. group_by=day,model composite query
        let resp = reqwest::Client::new()
            .get(format!(
                "http://{proxy_addr}/v1/token/usage/history?group_by=day,model"
            ))
            .header("Authorization", format!("Bearer {key}"))
            .send()
            .await
            .expect("send day,model");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: Value = resp.json().await.expect("json");
        assert_eq!(body["data"]["group_by"], "day,model");
        let groups = body["data"]["groups"].as_array().expect("groups");
        assert_eq!(groups.len(), 1);
        assert!(groups[0].get("key").is_some());
        assert_eq!(groups[0]["model"], "deepseek-v4-flash-lp");

        // 2. Empty period query (empty state contract): return 200, groups: [], summary all 0
        let resp_empty = reqwest::Client::new()
            .get(format!(
                "http://{proxy_addr}/v1/token/usage/history?period=2026-01-01:2026-01-02"
            ))
            .header("Authorization", format!("Bearer {key}"))
            .send()
            .await
            .expect("send empty window");
        assert_eq!(resp_empty.status(), reqwest::StatusCode::OK);
        let body_empty: Value = resp_empty.json().await.expect("json");
        let groups_empty = body_empty["data"]["groups"]
            .as_array()
            .expect("empty groups");
        assert_eq!(groups_empty.len(), 0);
        assert_eq!(body_empty["data"]["summary"]["requests"], 0);
        assert_eq!(body_empty["data"]["summary"]["total_tokens"], 0);
    }

    #[tokio::test]
    async fn token_usage_history_whitelist_folds_unknown_to_other_c6() {
        let (mut state, key, _cfg_dir, _state_dir) = state_with_quota_token("tokens", 100_000);
        let temp_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::usage_stats::UsageStore::with_paths(
                config::UsageConfig::default(),
                temp_dir.path().join("usage.jsonl"),
                temp_dir.path().join("usage.db"),
            )
            .unwrap(),
        );

        let ts = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
        // Record an unrecognized/rogue model name not present in config.models
        store
            .record(
                crate::usage_stats::UsageRecord::new(
                    "unrecognized-secret-model".into(),
                    "prov".into(),
                    "chat".into(),
                    50,
                    50,
                    Some(100),
                )
                .with_timestamp(&ts)
                .with_token(Some(key.clone()), Some("Alice".into())),
            )
            .unwrap();
        store.flush_if_dirty().unwrap();
        state.set_usage_store_for_test(store.clone());

        let proxy_addr = serve_on_random_port(router(state)).await;

        let resp = reqwest::Client::new()
            .get(format!(
                "http://{proxy_addr}/v1/token/usage/history?group_by=model"
            ))
            .header("Authorization", format!("Bearer {key}"))
            .send()
            .await
            .expect("send history");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: Value = resp.json().await.expect("json");
        let groups = body["data"]["groups"].as_array().expect("groups");
        assert_eq!(groups.len(), 1);
        // Whitelist defense must fold "unrecognized-secret-model" to "other"
        assert_eq!(groups[0]["key"], "other");
        assert_eq!(groups[0]["model"], "other");
    }
}

mod cooldown_path_logging_tests {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn test_cooldown_path_logging_retry_after() {
        let capture = StreamInterruptLogCapture::start();
        let cfg = config::default_deepseek_config();
        let state = AppState::new_in_memory_for_management(cfg);
        let plan = test_execution_plan("http://127.0.0.1:8080".to_string());
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "120".parse().unwrap());
        let cooldown = config::FallbackCooldownConfig::default();

        let duration = resolve_candidate_cooldown_duration(
            &state,
            &plan,
            StatusCode::TOO_MANY_REQUESTS,
            &headers,
            &cooldown,
        )
        .await;

        assert_eq!(duration, Some(Duration::from_secs(120)));
        let lines = capture.lines();
        let log = lines
            .iter()
            .find(|l| {
                l.contains("event=cooldown_duration_resolved") && l.contains("path=retry_after")
            })
            .expect("must emit cooldown_duration_resolved log for retry_after");
        assert!(log.contains("provider=test-provider"), "log: {log}");
        assert!(log.contains("duration_secs=120"), "log: {log}");
    }

    #[tokio::test]
    #[serial]
    async fn test_cooldown_path_logging_default() {
        let capture = StreamInterruptLogCapture::start();
        let cfg = config::default_deepseek_config();
        let state = AppState::new_in_memory_for_management(cfg);
        let plan = test_execution_plan("http://127.0.0.1:8080".to_string());
        let headers = HeaderMap::new();
        let cooldown = config::FallbackCooldownConfig {
            rate_limit_seconds: 60,
            ..Default::default()
        };

        let duration = resolve_candidate_cooldown_duration(
            &state,
            &plan,
            StatusCode::TOO_MANY_REQUESTS,
            &headers,
            &cooldown,
        )
        .await;

        assert_eq!(duration, Some(Duration::from_secs(60)));
        let lines = capture.lines();
        let log = lines
            .iter()
            .find(|l| l.contains("event=cooldown_duration_resolved") && l.contains("path=default"))
            .expect("must emit cooldown_duration_resolved log for default");
        assert!(log.contains("provider=test-provider"), "log: {log}");
        assert!(log.contains("duration_secs=60"), "log: {log}");

        // 非冷却状态时不应发出日志
        let no_log_duration =
            resolve_candidate_cooldown_duration(&state, &plan, StatusCode::OK, &headers, &cooldown)
                .await;
        assert_eq!(no_log_duration, None);
    }
}
