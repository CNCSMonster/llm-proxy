use serde_json::{Value, json};

use super::*;
// Access internal (pub(super)) helpers not re-exported by mod.rs
use super::anthropic_responses::*;
use super::chat_anthropic::*;
use super::shared::*;

#[test]
fn chat_to_anthropic_rejects_image_without_string_url() {
    let input = json!({
        "messages": [{
            "role": "user",
            "content": [{"type": "image_url", "image_url": {"url": null}}]
        }]
    });

    let err = chat_to_anthropic_request(input, "claude-upstream")
        .expect_err("invalid Chat image must be rejected locally");
    assert!(
        err.to_string()
            .contains("Chat image_url.url must be a non-empty string")
    );
}

#[test]
fn anthropic_to_responses_rejects_image_without_source_payload() {
    let input = json!({
        "messages": [{
            "role": "user",
            "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png"}}]
        }]
    });

    let err = anthropic_to_responses_request(input, "responses-upstream")
        .expect_err("invalid Anthropic image must be rejected locally");
    assert!(
        err.to_string()
            .contains("Anthropic image source.data must be a non-empty string")
    );
}

#[test]
fn anthropic_to_chat_rejects_image_without_source_payload() {
    let input = json!({
        "messages": [{
            "role": "user",
            "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png"}}]
        }]
    });

    let err = anthropic_to_chat(input, "chat-upstream")
        .expect_err("invalid Anthropic image must be rejected locally");
    assert!(
        err.to_string()
            .contains("Anthropic image source.data must be a non-empty string")
    );
}

#[test]
fn anthropic_to_chat_rejects_document_content() {
    let input = json!({
        "messages": [{
            "role": "user",
            "content": [{
                "type": "document",
                "title": "spec",
                "source": {"type": "url", "url": "https://example.com/spec.pdf"}
            }]
        }]
    });

    let err = anthropic_to_chat(input, "chat-upstream")
        .expect_err("Anthropic document must not be fabricated as Chat content");
    assert!(
        err.to_string()
            .contains("Anthropic document content is unsupported for Chat Completions")
    );
}

#[test]
fn anthropic_to_responses_preserves_interleaved_content_and_tool_use() {
    let input = json!({
        "messages": [{
            "role": "assistant",
            "content": [
                {"type": "text", "text": "before"},
                {"type": "tool_use", "id": "call_1", "name": "lookup", "input": {}},
                {"type": "text", "text": "after"}
            ]
        }]
    });

    let out =
        anthropic_to_responses_request(input, "responses-upstream").expect("preserve source order");
    assert_eq!(
        out["input"],
        json!([
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "before"}]},
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "after"}]}
        ])
    );
}

#[test]
fn anthropic_to_responses_rejects_document_without_source_payload() {
    let input = json!({
        "messages": [{
            "role": "user",
            "content": [{"type": "document", "source": {"type": "url", "url": null}}]
        }]
    });

    let err = anthropic_to_responses_request(input, "responses-upstream")
        .expect_err("invalid Anthropic document must be rejected locally");
    assert!(
        err.to_string()
            .contains("Anthropic document source.url must be a non-empty string")
    );
}

#[test]
fn chat_to_anthropic_preserves_valid_image_url() {
    let input = json!({
        "messages": [{
            "role": "user",
            "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw"}}]
        }]
    });

    let out = chat_to_anthropic_request(input, "claude-upstream").expect("convert valid image");
    assert_eq!(
        out["messages"][0]["content"],
        json!([{
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw"}
        }])
    );
}

#[test]
fn anthropic_image_maps_to_valid_openai_image_url() {
    let input = json!({
        "messages": [{
            "role": "user",
            "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/4AAQ"}}]
        }]
    });

    let responses = anthropic_to_responses_request(input.clone(), "responses-upstream")
        .expect("convert valid image to Responses");
    assert_eq!(
        responses["input"][0]["content"][0],
        json!({"type": "input_image", "image_url": "data:image/jpeg;base64,/9j/4AAQ"})
    );

    let chat = anthropic_to_chat(input, "chat-upstream").expect("convert valid image to Chat");
    assert_eq!(
        chat["messages"][0]["content"][0],
        json!({"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/4AAQ"}})
    );
}

#[test]
fn responses_image_url_object_is_normalized_for_chat_and_anthropic() {
    let input = json!({
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_image", "image_url": {"url": "data:image/png;base64,iVBORw"}}]
        }]
    });

    let chat =
        responses_to_chat(input.clone(), "chat-upstream").expect("convert image dialect to Chat");
    assert_eq!(
        chat["messages"][0]["content"],
        json!([{"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw"}}])
    );

    let anthropic = responses_to_anthropic(input, "anthropic-upstream")
        .expect("convert image dialect to Anthropic");
    assert_eq!(
        anthropic["messages"][0]["content"],
        json!([{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw"}}])
    );
}

#[test]
fn responses_to_anthropic_rejects_empty_file_payload() {
    let input = json!({
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_file", "filename": "report.pdf", "file_data": ""}]
        }]
    });

    let err = responses_to_anthropic(input, "anthropic-upstream")
        .expect_err("empty Responses file payload must be rejected locally");
    assert!(
        err.to_string()
            .contains("Responses input_file must contain a non-empty file_data or file_url")
    );
}

#[test]
fn chat_to_anthropic_merges_multiple_system_messages_in_order() {
    let input = json!({
        "messages": [
            {"role": "system", "content": "Rule A"},
            {"role": "system", "content": "Rule B"},
            {"role": "user", "content": "Hello"}
        ]
    });

    let out = chat_to_anthropic_request(input, "anthropic-upstream").expect("convert systems");
    assert_eq!(out["system"], "Rule A\nRule B");
}

#[test]
fn anthropic_to_responses_maps_truncation_stop_reasons() {
    let truncated = json!({
        "id": "resp_upstream_1",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "秋天总是来得悄无声"}],
        "stop_reason": "max_tokens",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let out = anthropic_to_responses(truncated, "frontend");
    assert_eq!(out["id"], "resp_upstream_1");
    assert_eq!(out["status"], "incomplete");
    assert_eq!(out["incomplete_details"]["reason"], "max_output_tokens");
    for item in out["output"].as_array().expect("output") {
        assert_eq!(item["status"], "incomplete", "{item}");
    }

    let refused = json!({
        "content": [],
        "stop_reason": "refusal",
        "usage": {"input_tokens": 10, "output_tokens": 0}
    });
    let refused_out = anthropic_to_responses(refused, "frontend");
    assert_eq!(refused_out["status"], "incomplete");
    assert_eq!(
        refused_out["incomplete_details"]["reason"],
        "content_filter"
    );

    let stopped = json!({
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 1}
    });
    let stopped_out = anthropic_to_responses(stopped, "frontend");
    assert_eq!(stopped_out["status"], "completed");
    assert_eq!(stopped_out["incomplete_details"], Value::Null);
}

#[test]
fn responses_text_request_converts_to_chat_completions() {
    let input = json!({
        "model": "deepseek-v4-pro-lp",
        "instructions": "be concise",
        "input": "hello",
        "stream": true,
        "max_output_tokens": 128,
        "reasoning": { "effort": "high" }
    });

    let out = responses_to_chat(input, "deepseek-v4-pro").expect("convert");

    assert_eq!(out["model"], "deepseek-v4-pro");
    assert_eq!(out["stream"], true);
    assert_eq!(out["max_tokens"], 128);
    assert_eq!(out["reasoning_effort"], "high");
    assert_eq!(
        out["messages"],
        json!([
            { "role": "system", "content": "be concise" },
            { "role": "user", "content": "hello" }
        ])
    );
    assert_eq!(out["stream_options"], json!({ "include_usage": true }));
}

#[test]
fn chat_request_converts_to_responses_request() {
    let input = json!({
        "model": "frontend",
        "messages": [
            {"role": "system", "content": "be concise"},
            {"role": "user", "content": "hello"}
        ],
        "stream": true,
        "max_tokens": 128,
        "tools": [{
            "type": "function",
            "function": {
                "name": "lookup",
                "description": "Lookup",
                "parameters": {"type": "object"}
            }
        }]
    });

    let out = chat_to_responses_request(input, "upstream").expect("convert");

    assert_eq!(out["model"], "upstream");
    assert_eq!(out["instructions"], "be concise");
    assert_eq!(out["stream"], true);
    assert_eq!(out["max_output_tokens"], 128);
    assert_eq!(out["input"][0]["role"], "user");
    assert_eq!(
        out["input"][0]["content"][0],
        json!({"type": "input_text", "text": "hello"})
    );
    assert_eq!(out["tools"][0]["name"], "lookup");
}

#[test]
fn chat_image_url_object_converts_to_responses_string_url() {
    let input = json!({
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": {
                    "url": "data:image/png;base64,iVBORw",
                    "detail": "high"
                }
            }]
        }]
    });

    let out = chat_to_responses_request(input, "upstream").expect("convert");

    assert_eq!(
        out["input"][0]["content"][0],
        json!({
            "type": "input_image",
            "image_url": "data:image/png;base64,iVBORw",
            "detail": "high"
        })
    );
}

#[test]
fn chat_image_url_without_string_url_is_rejected_locally() {
    for image_url in [json!({}), json!({"url": null}), json!({"url": 42})] {
        let input = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": image_url
                }]
            }]
        });

        let err = chat_to_responses_request(input, "upstream").expect_err("invalid image URL");
        assert!(
            err.to_string()
                .contains("Chat image_url.url must be a non-empty string"),
            "{err}"
        );
    }
}

#[test]
fn responses_response_converts_to_chat_response() {
    let input = json!({
        "id": "resp_1",
        "created_at": 123,
        "status": "completed",
        "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "hello"}]},
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"x\"}"}
        ],
        "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7}
    });

    let out = responses_to_chat_response(input, "frontend");

    assert_eq!(out["id"], "resp_1");
    assert_eq!(out["model"], "frontend");
    assert_eq!(out["choices"][0]["message"]["content"], "hello");
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"][0]["id"],
        "call_1"
    );
    assert_eq!(out["usage"]["total_tokens"], 7);
}

#[test]
fn chat_response_converts_to_responses_response() {
    let input = json!({
        "id": "chatcmpl_1",
        "created": 123,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": "hello"
            }
        }],
        "usage": {
            "prompt_tokens": 3,
            "completion_tokens": 4,
            "total_tokens": 7,
            "prompt_tokens_details": {"cached_tokens": 1, "cache_write_tokens": 2},
            "completion_tokens_details": {"reasoning_tokens": 2}
        }
    });

    let out = chat_to_responses(input, "deepseek-v4-pro-lp");

    assert_eq!(out["id"], "chatcmpl_1");
    assert_eq!(out["model"], "deepseek-v4-pro-lp");
    assert_eq!(out["output"][0]["content"][0]["text"], "hello");
    assert_eq!(out["usage"]["total_tokens"], 7);
}

#[test]
fn anthropic_request_converts_to_chat_completions() {
    let input = json!({
        "model": "frontend",
        "system": "be concise",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
        "tools": [{"name": "lookup", "description": "Lookup", "input_schema": {"type": "object"}}],
        "max_tokens": 128
    });

    let out = anthropic_to_chat(input, "upstream").expect("convert");

    assert_eq!(out["model"], "upstream");
    assert_eq!(out["max_tokens"], 128);
    assert_eq!(
        out["messages"][0],
        json!({"role": "system", "content": "be concise"})
    );
    assert_eq!(out["messages"][1]["role"], "user");
    assert_eq!(out["tools"][0]["function"]["name"], "lookup");
}

#[test]
fn anthropic_request_converts_to_responses_request() {
    let input = json!({
        "model": "frontend",
        "system": "be concise",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
        "tools": [{"name": "lookup", "description": "Lookup", "input_schema": {"type": "object"}}],
        "max_tokens": 128
    });

    let out = anthropic_to_responses_request(input, "upstream").expect("convert");

    assert_eq!(out["model"], "upstream");
    assert_eq!(out["instructions"], "be concise");
    assert_eq!(out["max_output_tokens"], 128);
    assert_eq!(out["input"][0]["role"], "user");
    assert_eq!(
        out["input"][0]["content"][0],
        json!({"type": "input_text", "text": "hello"})
    );
    assert_eq!(out["tools"][0]["name"], "lookup");
}

#[test]
fn responses_response_converts_to_anthropic_message() {
    let input = json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "hello"}]},
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"x\"}"}
        ],
        "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7}
    });

    let out = responses_to_anthropic_response(input, "frontend");

    // the Responses `resp_1` id is not surfaced on the Anthropic slot.
    assert!(out["id"].as_str().expect("id").starts_with("msg_"));
    assert_eq!(out["model"], "frontend");
    assert_eq!(out["content"][0], json!({"type": "text", "text": "hello"}));
    assert_eq!(out["content"][1]["type"], "tool_use");
    assert_eq!(out["stop_reason"], "tool_use");
    assert_eq!(out["usage"]["input_tokens"], 3);
}

#[test]
fn responses_text_request_converts_to_anthropic_messages() {
    let input = json!({
        "model": "claude-sonnet-lp",
        "instructions": "be concise",
        "input": "hello",
        "stream": false,
        "max_output_tokens": 128,
        "tools": [{
            "type": "function",
            "name": "lookup",
            "description": "Lookup",
            "parameters": {"type": "object"}
        }]
    });

    let out = responses_to_anthropic(input, "claude-sonnet-4-8").expect("convert");

    assert_eq!(out["model"], "claude-sonnet-4-8");
    assert_eq!(out["system"], "be concise");
    assert_eq!(out["max_tokens"], 128);
    assert_eq!(out["messages"][0]["role"], "user");
    assert_eq!(out["messages"][0]["content"][0]["text"], "hello");
    assert_eq!(out["tools"][0]["name"], "lookup");
}

#[test]
fn anthropic_response_converts_to_responses_response() {
    let input = json!({
        "id": "msg_123",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "hello"}],
        "usage": {"input_tokens": 3, "output_tokens": 4}
    });

    let out = anthropic_to_responses(input, "claude-sonnet-lp");

    assert_eq!(out["id"], "msg_123");
    assert_eq!(out["model"], "claude-sonnet-lp");
    assert_eq!(out["output"][0]["content"][0]["text"], "hello");
    assert_eq!(out["usage"]["total_tokens"], 7);
}

#[test]
fn chat_request_converts_to_anthropic_messages() {
    let input = json!({
        "model": "claude-sonnet-lp",
        "messages": [
            {"role": "system", "content": "be concise"},
            {"role": "user", "content": "hello"}
        ],
        "max_tokens": 128
    });

    let out = chat_to_anthropic_request(input, "claude-sonnet-4-8").expect("convert");

    assert_eq!(out["model"], "claude-sonnet-4-8");
    assert_eq!(out["system"], "be concise");
    assert_eq!(out["max_tokens"], 128);
    assert_eq!(out["messages"][0]["role"], "user");
    assert_eq!(out["messages"][0]["content"][0]["text"], "hello");
}

#[test]
fn anthropic_response_converts_to_chat_response() {
    let input = json!({
        "id": "msg_123",
        "type": "message",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 4}
    });

    let out = anthropic_to_chat_response(input, "claude-sonnet-lp");

    assert_eq!(out["id"], "msg_123");
    // Anthropic upstreams carry no timestamp; `created` is the synthesis time,
    // never a fabricated 0 (spec.md §6 rule 3).
    assert!(out["created"].as_i64().expect("created") > 1_700_000_000);
    assert_eq!(out["model"], "claude-sonnet-lp");
    assert_eq!(out["choices"][0]["message"]["content"], "hello");
    assert_eq!(out["choices"][0]["finish_reason"], "stop");
    assert_eq!(out["usage"]["total_tokens"], 7);
}

#[test]
fn chat_response_converts_to_anthropic_message() {
    let input = json!({
        "id": "chatcmpl_1",
        "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "hello"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4}
    });

    let out = chat_to_anthropic(input, "frontend");

    // Chat `chatcmpl…` ids are never surfaced in the Anthropic message-id
    // slot; the proxy regenerates one in the `msg_` namespace.
    let anthropic_id = out["id"].as_str().expect("anthropic id");
    assert!(
        anthropic_id.starts_with("msg_"),
        "Anthropic surface leaked {anthropic_id}"
    );
    assert_ne!(anthropic_id, "chatcmpl_1");
    assert_eq!(out["type"], "message");
    assert_eq!(out["model"], "frontend");
    assert_eq!(out["content"][0], json!({"type": "text", "text": "hello"}));
    assert_eq!(out["usage"]["input_tokens"], 3);
}

#[test]
fn tool_arguments_normalize_to_json_object() {
    // Valid object strings round-trip.
    assert_eq!(
        normalize_arguments_value(Some(&json!("{\"a\":1}"))),
        json!({"a": 1})
    );
    assert_eq!(normalize_arguments(Some(&json!("{\"a\":1}"))), "{\"a\":1}");
    // Missing, null, empty, unparsable, and non-object values become {}.
    for value in [
        None,
        Some(&Value::Null),
        Some(&json!("")),
        Some(&json!("not json")),
        Some(&json!("123")),
        Some(&json!("[1,2]")),
        Some(&json!(42)),
    ] {
        assert_eq!(normalize_arguments_value(value), json!({}), "{value:?}");
        assert_eq!(normalize_arguments(value), "{}", "{value:?}");
    }
    // Already-decoded objects pass through.
    assert_eq!(
        normalize_arguments_value(Some(&json!({"b": 2}))),
        json!({"b": 2})
    );
}

#[test]
fn anthropic_url_image_preserves_url_when_converting_to_openai_shapes() {
    let part = json!({"type":"image","source":{"type":"url","url":"https://example.com/x.png"}});
    assert_eq!(
        anthropic_image_to_responses_image(&part),
        json!({"type":"input_image","image_url":"https://example.com/x.png"})
    );
    assert_eq!(
        anthropic_block_to_chat_content(&part).expect("convert valid Anthropic image"),
        json!({"type":"image_url","image_url":{"url":"https://example.com/x.png"}})
    );
}

#[test]
fn responses_file_and_anthropic_document_convert_without_fetching() {
    let responses = json!({
        "type": "input_file",
        "filename": "paper.pdf",
        "file_data": "data:application/pdf;base64,JVBERi0x"
    });
    assert_eq!(
        responses_file_to_anthropic_document(responses.as_object().unwrap()).unwrap(),
        json!({
            "type": "document",
            "title": "paper.pdf",
            "source": {"type":"base64","media_type":"application/pdf","data":"JVBERi0x"}
        })
    );

    let anthropic = json!({
        "type": "document",
        "title": "remote.pdf",
        "source": {"type":"url","url":"https://example.com/remote.pdf"}
    });
    assert_eq!(
        anthropic_document_to_responses_file(&anthropic),
        json!({"type":"input_file","filename":"remote.pdf","file_url":"https://example.com/remote.pdf"})
    );
}

#[test]
fn image_url_to_anthropic_source_parses_data_uri_media_type() {
    let source = image_url_to_anthropic_source("data:image/jpeg;base64,/9j/4AAQ");
    assert_eq!(
        source,
        json!({"type": "base64", "media_type": "image/jpeg", "data": "/9j/4AAQ"})
    );

    let source = image_url_to_anthropic_source("data:image/png;base64,iVBORw");
    assert_eq!(source["media_type"], "image/png");

    // Plain URLs must not be treated as base64 payloads.
    let source = image_url_to_anthropic_source("https://example.com/x.png");
    assert_eq!(
        source,
        json!({"type": "url", "url": "https://example.com/x.png"})
    );
}

// type=adaptive → thinkingLevel (string), not thinkingBudget (integer)

#[test]
fn responses_to_anthropic_converts_reasoning_effort_to_thinking() {
    let input = json!({
        "model": "frontend",
        "input": "hello",
        "reasoning": {"effort": "high"}
    });
    let out = responses_to_anthropic(input, "claude-sonnet-4-20250514").expect("convert");
    assert_eq!(out["thinking"]["type"], "adaptive");
    assert_eq!(out["output_config"]["effort"], "high");
}

#[test]
fn responses_to_anthropic_reasoning_none_produces_disabled_thinking() {
    let input = json!({
        "model": "frontend",
        "input": "hello",
        "reasoning": {"effort": "none"}
    });
    let out = responses_to_anthropic(input, "claude-sonnet-4-20250514").expect("convert");
    assert_eq!(out["thinking"]["type"], "disabled");
    assert!(
        out.get("output_config").is_none(),
        "effort=none should not set output_config"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Layer C: Anthropic 4.6+ thinking/output_config ↔ Chat/Responses effort.
// ─────────────────────────────────────────────────────────────────────────────

fn anthropic_request(thinking: Option<Value>, effort: Option<&str>) -> Value {
    let mut body = json!({
        "model": "frontend",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "ping"}],
    });
    if let Some(thinking) = thinking {
        body.as_object_mut()
            .unwrap()
            .insert("thinking".to_string(), thinking);
    }
    if let Some(effort) = effort {
        body.as_object_mut()
            .unwrap()
            .insert("output_config".to_string(), json!({"effort": effort}));
    }
    body
}

/// `adaptive + effort=E` → the peer's single effort field.
#[test]
fn anthropic_46_adaptive_with_effort_converts_to_chat_and_responses_effort() {
    let input = anthropic_request(Some(json!({"type": "adaptive"})), Some("high"));

    let chat = anthropic_to_chat(input.clone(), "upstream").expect("chat convert");
    assert_eq!(chat["reasoning_effort"], "high");
    assert!(chat.get("thinking").is_none(), "no thinking switch on Chat");
    assert!(chat.get("output_config").is_none());

    let responses = anthropic_to_responses_request(input, "upstream").expect("responses convert");
    assert_eq!(responses["reasoning"]["effort"], "high");
    assert!(responses.get("thinking").is_none());
}

/// `disabled` → the peer's protocol-standard off value.
#[test]
fn anthropic_46_disabled_converts_to_chat_and_responses_none() {
    let input = anthropic_request(Some(json!({"type": "disabled"})), None);

    let chat = anthropic_to_chat(input.clone(), "upstream").expect("chat convert");
    assert_eq!(chat["reasoning_effort"], "none");

    let responses = anthropic_to_responses_request(input, "upstream").expect("responses convert");
    assert_eq!(responses["reasoning"]["effort"], "none");
}

/// `disabled + effort` keeps the switch and drops the level (the peer has one
/// field and cannot express both) — never re-expressed as an enabling effort.
#[test]
fn anthropic_46_disabled_with_effort_keeps_off_and_drops_level() {
    let input = anthropic_request(Some(json!({"type": "disabled"})), Some("high"));

    let chat = anthropic_to_chat(input.clone(), "upstream").expect("chat convert");
    assert_eq!(
        chat["reasoning_effort"], "none",
        "degraded: keep off, drop the level"
    );

    let responses = anthropic_to_responses_request(input, "upstream").expect("responses convert");
    assert_eq!(responses["reasoning"]["effort"], "none");
}

/// effort without a switch ("只给力度") still carries the level across; the peer
/// has no way to express "do not touch the switch", so the level alone is forwarded.
#[test]
fn anthropic_46_effort_without_thinking_forwards_level_only() {
    let input = anthropic_request(None, Some("low"));

    let chat = anthropic_to_chat(input.clone(), "upstream").expect("chat convert");
    assert_eq!(chat["reasoning_effort"], "low");

    let responses = anthropic_to_responses_request(input, "upstream").expect("responses convert");
    assert_eq!(responses["reasoning"]["effort"], "low");
}

/// Chat/Responses have no separate switch, so an activity level becomes
/// `adaptive + output_config.effort`.
#[test]
fn chat_effort_converts_to_anthropic_adaptive_with_effort() {
    let input = json!({
        "model": "frontend",
        "messages": [{"role": "user", "content": "ping"}],
        "reasoning_effort": "medium",
    });
    let out = chat_to_anthropic_request(input, "upstream").expect("convert");
    assert_eq!(out["thinking"]["type"], "adaptive");
    assert_eq!(out["output_config"]["effort"], "medium");
}

/// Chat `none` closes the switch and never emits a budget or an effort.
#[test]
fn chat_none_converts_to_anthropic_disabled_without_budget_or_effort() {
    let input = json!({
        "model": "frontend",
        "messages": [{"role": "user", "content": "ping"}],
        "reasoning_effort": "none",
    });
    let out = chat_to_anthropic_request(input, "upstream").expect("convert");
    assert_eq!(out["thinking"]["type"], "disabled");
    assert!(out["thinking"].get("budget_tokens").is_none());
    assert!(out.get("output_config").is_none());
}

/// the conversion layer never invents a token budget from a level.
#[test]
fn anthropic_effort_conversion_never_emits_budget_tokens() {
    for effort in ["minimal", "low", "medium", "high", "xhigh", "max"] {
        let input = anthropic_request(Some(json!({"type": "adaptive"})), Some(effort));
        let out = anthropic_to_responses_request(input, "upstream").expect("convert");
        assert_eq!(out["reasoning"]["effort"], effort);
        assert!(
            out.get("thinking").is_none(),
            "no Anthropic thinking object on Responses egress"
        );
    }
}

#[test]
fn anthropic_to_responses_converts_thinking_blocks_to_reasoning() {
    let input = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "let me think...", "signature": "sig_abc"},
            {"type": "text", "text": "the answer"}
        ],
        "model": "claude-sonnet-4-20250514",
        "usage": {"input_tokens": 10, "output_tokens": 20}
    });
    let out = anthropic_to_responses(input, "claude-sonnet-4-20250514");
    let output = out["output"].as_array().expect("output array");
    // First item should be reasoning
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[0]["summary"][0]["text"], "let me think...");
    assert_eq!(output[0]["signature"], "sig_abc");
    // Second item should be message with text
    assert_eq!(output[1]["type"], "message");
}

#[test]
fn anthropic_to_responses_request_accepts_string_content() {
    // Anthropic API allows `content` to be a plain string, not only an array.
    // Regression: string content was dropped, producing an empty `input`
    // that the upstream rejects with 400.
    let input = json!({
        "model": "claude-sonnet-4-20250514",
        "messages": [
            {"role": "user", "content": "Say hi"},
            {"role": "assistant", "content": "Hi there"}
        ]
    });
    let out = anthropic_to_responses_request(input, "gpt-5.4").expect("convert");
    let input_arr = out["input"].as_array().expect("input array");
    assert_eq!(input_arr.len(), 2, "both messages must be preserved");
    assert_eq!(input_arr[0]["type"], "message");
    assert_eq!(input_arr[0]["role"], "user");
    assert_eq!(input_arr[0]["content"][0]["type"], "input_text");
    assert_eq!(input_arr[0]["content"][0]["text"], "Say hi");
    assert_eq!(input_arr[1]["type"], "message");
    assert_eq!(input_arr[1]["role"], "assistant");
    assert_eq!(input_arr[1]["content"][0]["type"], "output_text");
    assert_eq!(input_arr[1]["content"][0]["text"], "Hi there");
}

#[test]
fn anthropic_string_content_is_not_dropped() {
    // Anthropic content 可以是纯字符串（等价单个 text block）。
    // 若被当作数组处理会静默跳过消息 → 空上游请求 → 400。
    let input = json!({
        "model": "gemini-3.6-flash-high",
        "max_tokens": 100,
        "messages": [
            {"role": "user", "content": "say OK"}
        ]
    });
    let out = anthropic_to_responses_request(input, "gemini-3.6-flash-high").expect("convert");
    let items = out["input"].as_array().expect("input array");
    assert_eq!(items.len(), 1, "字符串 content 的消息不能被丢弃: {out}");
    assert_eq!(items[0]["type"], "message");
    assert_eq!(items[0]["content"][0]["text"], "say OK");
}

#[test]
fn test_apply_responses_egress_compat_string_input() {
    let mut body = json!({
        "model": "gpt-4o",
        "input": "hello world"
    });
    let compat = crate::config::CompatConfig::default();
    apply_responses_egress_compat(&mut body, &compat, None);

    assert_eq!(
        body["input"],
        json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello world"}]}])
    );
}

#[test]
fn test_apply_responses_egress_compat_store_and_flags() {
    let mut body = json!({
        "model": "gpt-4o",
        "max_output_tokens": 500
    });
    let compat = crate::config::CompatConfig {
        must_not_store: Some(true),
        force_stream: Some(true),
        strip_max_output_tokens: Some(true),
        ..Default::default()
    };
    apply_responses_egress_compat(&mut body, &compat, Some(true));

    assert_eq!(
        body["store"], false,
        "must_not_store should override store param to false"
    );
    assert_eq!(
        body["stream"], true,
        "force_stream should set stream to true"
    );
    assert!(
        body.get("max_output_tokens").is_none(),
        "strip_max_output_tokens should remove field"
    );
}

#[test]
fn test_responses_to_chat_core_logic() {
    let req = json!({
        "instructions": "system prompt",
        "input": "user input",
        "temperature": 0.7,
        "top_p": 0.9,
        "max_output_tokens": 200,
        "reasoning": {"effort": "high"},
        "stream": true,
        "tools": [{
            "type": "function",
            "name": "get_weather",
            "parameters": {"type": "object"}
        }]
    });

    let out = responses_to_chat(req, "target-model").expect("responses_to_chat conversion failed");

    assert_eq!(out["model"], "target-model");
    assert_eq!(out["temperature"], 0.7);
    assert_eq!(out["top_p"], 0.9);
    assert_eq!(out["max_tokens"], 200);
    assert_eq!(out["reasoning_effort"], "high");
    assert_eq!(out["stream"], true);
    assert_eq!(out["stream_options"]["include_usage"], true);

    let messages = out["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "system prompt");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "user input");

    assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
}

#[test]
fn test_responses_to_chat_response_conversion() {
    let resp = json!({
        "id": "resp_123",
        "created_at": 1000,
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Hello response"}]
            },
            {
                "id": "call_abc",
                "type": "function_call",
                "name": "calculator",
                "arguments": "{\"x\":1}"
            }
        ],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 20
        }
    });

    let out = responses_to_chat_response(resp, "frontend-model");
    assert_eq!(out["id"], "resp_123");
    assert_eq!(out["choices"][0]["message"]["content"], "Hello response");
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"][0]["id"],
        "call_abc"
    );
}

#[test]
fn test_chat_to_responses_core_logic() {
    let req = json!({
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Hello!"}
        ],
        "temperature": 0.5,
        "max_tokens": 150,
        "tools": [{
            "type": "function",
            "function": {
                "name": "search",
                "parameters": {"type": "object"}
            }
        }]
    });

    let out =
        chat_to_responses_request(req, "upstream-model").expect("chat_to_responses_request failed");

    assert_eq!(out["model"], "upstream-model");
    assert_eq!(out["instructions"], "You are helpful.");
    assert_eq!(out["temperature"], 0.5);
    assert_eq!(out["max_output_tokens"], 150);

    let input = out["input"].as_array().expect("input array");
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["text"], "Hello!");

    let resp_json = json!({
        "id": "chat_999",
        "created": 2000,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "content": "Hi there!",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "search",
                        "arguments": "{\"q\":\"rust\"}"
                    }
                }]
            }
        }]
    });
    let converted_resp = chat_to_responses(resp_json, "frontend-model");
    assert_eq!(converted_resp["model"], "frontend-model");
    assert_eq!(converted_resp["status"], "completed");

    let output = converted_resp["output"].as_array().expect("output array");
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["type"], "output_text");
    assert_eq!(output[0]["content"][0]["text"], "Hi there!");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["name"], "search");
}

#[test]
fn test_chat_to_responses_reasoning_content_plain_text() {
    let resp = json!({
        "id": "chatcmpl_reasoning",
        "created": 1700000000,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "reasoning_content": "   First step: analyze.\nSecond step: formulate answer.   ",
                "content": "Final answer"
            }
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 25,
            "total_tokens": 35
        }
    });

    let converted = chat_to_responses(resp, "qwen3.7-plus-bailian-lp");
    assert_eq!(converted["status"], "completed");

    let output = converted["output"].as_array().expect("output array");
    assert_eq!(output.len(), 2);

    // First item is reasoning
    assert_eq!(output[0]["type"], "reasoning");
    let rs_id = output[0]["id"].as_str().expect("rs_id");
    assert!(rs_id.starts_with("rs_"), "ID must have rs_ prefix: {rs_id}");
    assert!(rs_id.len() > 3);
    assert_eq!(output[0]["summary"], json!([]));
    let content = output[0]["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "reasoning_text");
    // Whitespace preserved verbatim without trim
    assert_eq!(
        content[0]["text"],
        "   First step: analyze.\nSecond step: formulate answer.   "
    );

    // Second item is message
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "Final answer");
}

#[test]
fn test_chat_to_responses_canonical_order_reasoning_text_tools() {
    let resp = json!({
        "id": "chatcmpl_canonical",
        "created": 1700000001,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "reasoning_content": "Thinking about tools...",
                "content": "I will search the web.",
                "tool_calls": [
                    {
                        "id": "call_search_1",
                        "type": "function",
                        "function": {
                            "name": "web_search",
                            "arguments": "{\"query\":\"rust lang\"}"
                        }
                    },
                    {
                        "id": "call_calc_2",
                        "type": "function",
                        "function": {
                            "name": "calculator",
                            "arguments": "{\"expr\":\"2+2\"}"
                        }
                    }
                ]
            }
        }]
    });

    let converted = chat_to_responses(resp, "test-model");
    assert_eq!(converted["status"], "completed");

    let output = converted["output"].as_array().expect("output array");
    assert_eq!(output.len(), 4);

    // 1. Reasoning
    assert_eq!(output[0]["type"], "reasoning");
    assert!(output[0]["id"].as_str().unwrap().starts_with("rs_"));

    // 2. Assistant text message
    assert_eq!(output[1]["type"], "message");
    assert!(output[1]["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(output[1]["content"][0]["text"], "I will search the web.");

    // 3. First tool call: preserves call_id, distinct local fc_ ID
    assert_eq!(output[2]["type"], "function_call");
    assert_eq!(output[2]["call_id"], "call_search_1");
    let fc1_id = output[2]["id"].as_str().unwrap();
    assert!(fc1_id.starts_with("fc_"));
    assert_ne!(fc1_id, "call_search_1");
    assert_eq!(output[2]["name"], "web_search");
    assert_eq!(output[2]["arguments"], "{\"query\":\"rust lang\"}");

    // 4. Second tool call: preserves call_id, distinct local fc_ ID
    assert_eq!(output[3]["type"], "function_call");
    assert_eq!(output[3]["call_id"], "call_calc_2");
    let fc2_id = output[3]["id"].as_str().unwrap();
    assert!(fc2_id.starts_with("fc_"));
    assert_ne!(fc1_id, fc2_id);
    assert_eq!(output[3]["name"], "calculator");
}

#[test]
fn test_chat_to_responses_omit_empty_message() {
    // 1. Reasoning + tool calls, no content text: must NOT produce an empty message
    let resp = json!({
        "id": "chatcmpl_notext",
        "created": 1700000002,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "reasoning_content": "Just thinking",
                "content": "",
                "tool_calls": [{
                    "id": "call_99",
                    "type": "function",
                    "function": {
                        "name": "do_it",
                        "arguments": "{}"
                    }
                }]
            }
        }]
    });
    let converted = chat_to_responses(resp, "test-model");
    let output = converted["output"].as_array().expect("output array");
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[1]["type"], "function_call");

    // 2. Reasoning-only, null content: only reasoning output item
    let resp_reasoning_only = json!({
        "id": "chatcmpl_reasoning_only",
        "created": 1700000003,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "reasoning_content": "Only reasoning here",
                "content": Value::Null
            }
        }]
    });
    let converted2 = chat_to_responses(resp_reasoning_only, "test-model");
    let output2 = converted2["output"].as_array().expect("output array");
    assert_eq!(output2.len(), 1);
    assert_eq!(output2[0]["type"], "reasoning");

    // 3. All empty: output is []
    let resp_empty = json!({
        "id": "chatcmpl_empty",
        "created": 1700000004,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": "",
                "reasoning_content": ""
            }
        }]
    });
    let converted3 = chat_to_responses(resp_empty, "test-model");
    let output3 = converted3["output"].as_array().expect("output array");
    assert!(output3.is_empty());
}

#[test]
fn test_chat_to_responses_rejects_malformed_tool_metadata() {
    // Missing call id -> failed with server_error (no invented call_unknown)
    let resp_no_id = json!({
        "id": "chatcmpl_err1",
        "created": 1700000005,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "tool_calls": [{
                    "type": "function",
                    "function": {
                        "name": "foo",
                        "arguments": "{}"
                    }
                }]
            }
        }]
    });
    let conv1 = chat_to_responses(resp_no_id, "test-model");
    assert_eq!(conv1["status"], "failed");
    assert_eq!(conv1["error"]["code"], "server_error");

    // Missing function name -> failed with server_error (no invented unknown)
    let resp_no_name = json!({
        "id": "chatcmpl_err2",
        "created": 1700000006,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "arguments": "{}"
                    }
                }]
            }
        }]
    });
    let conv2 = chat_to_responses(resp_no_name, "test-model");
    assert_eq!(conv2["status"], "failed");
    assert_eq!(conv2["error"]["code"], "server_error");

    // Missing arguments -> failed with server_error (no invented {})
    let resp_no_args = json!({
        "id": "chatcmpl_err3",
        "created": 1700000007,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "foo"
                    }
                }]
            }
        }]
    });
    let conv3 = chat_to_responses(resp_no_args, "test-model");
    assert_eq!(conv3["status"], "failed");
    assert_eq!(conv3["error"]["code"], "server_error");
}

#[test]
fn test_chat_to_responses_arguments_json_validation_and_truncation() {
    // Stop finish_reason with invalid JSON arguments -> failed with server_error
    let resp_bad_json = json!({
        "id": "chatcmpl_bad_json",
        "created": 1700000008,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "tool_calls": [{
                    "id": "call_bad",
                    "type": "function",
                    "function": {
                        "name": "foo",
                        "arguments": "invalid-json-text"
                    }
                }]
            }
        }]
    });
    let conv_bad = chat_to_responses(resp_bad_json, "test-model");
    assert_eq!(conv_bad["status"], "failed");
    assert_eq!(conv_bad["error"]["code"], "server_error");

    // Length finish_reason with truncated invalid JSON arguments -> retained as incomplete
    let resp_truncated = json!({
        "id": "chatcmpl_trunc",
        "created": 1700000009,
        "choices": [{
            "finish_reason": "length",
            "message": {
                "tool_calls": [{
                    "id": "call_trunc",
                    "type": "function",
                    "function": {
                        "name": "foo",
                        "arguments": "{\"incomplete_arg\": \"value"
                    }
                }]
            }
        }]
    });
    let conv_trunc = chat_to_responses(resp_truncated, "test-model");
    assert_eq!(conv_trunc["status"], "incomplete");
    assert_eq!(
        conv_trunc["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    let output = conv_trunc["output"].as_array().expect("output");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["arguments"], "{\"incomplete_arg\": \"value");
}

#[test]
fn test_chat_to_responses_terminal_status_and_errors() {
    // 1. content_filter -> incomplete with content_filter reason
    let resp_cf = json!({
        "id": "chatcmpl_cf",
        "created": 1700000010,
        "choices": [{
            "finish_reason": "content_filter",
            "message": {
                "content": "filtered content"
            }
        }]
    });
    let conv_cf = chat_to_responses(resp_cf, "test-model");
    assert_eq!(conv_cf["status"], "incomplete");
    assert_eq!(conv_cf["incomplete_details"]["reason"], "content_filter");

    // 2. Missing finish_reason -> failed with server_error
    let resp_missing_finish = json!({
        "id": "chatcmpl_nofinish",
        "created": 1700000011,
        "choices": [{
            "message": {
                "content": "hello"
            }
        }]
    });
    let conv_nofinish = chat_to_responses(resp_missing_finish, "test-model");
    assert_eq!(conv_nofinish["status"], "failed");
    assert_eq!(conv_nofinish["error"]["code"], "server_error");

    // 3. Unknown finish_reason -> failed with server_error
    let resp_unknown_finish = json!({
        "id": "chatcmpl_unknown",
        "created": 1700000012,
        "choices": [{
            "finish_reason": "unrecognized_status",
            "message": {
                "content": "hello"
            }
        }]
    });
    let conv_unknown = chat_to_responses(resp_unknown_finish, "test-model");
    assert_eq!(conv_unknown["status"], "failed");
    assert_eq!(conv_unknown["error"]["code"], "server_error");

    // 4. In-band upstream error -> failed with server_error
    let resp_inband_err = json!({
        "id": "chatcmpl_err",
        "error": {
            "message": "Internal engine fault",
            "code": "upstream_fault"
        }
    });
    let conv_err = chat_to_responses(resp_inband_err, "test-model");
    assert_eq!(conv_err["status"], "failed");
    assert_eq!(conv_err["error"]["code"], "server_error");
}

#[test]
fn test_chat_to_responses_usage_approved_rules() {
    // 1. Complete usage with details: mapped accurately
    let resp_full_usage = json!({
        "id": "chatcmpl_usage1",
        "created": 1700000013,
        "choices": [{
            "finish_reason": "stop",
            "message": { "content": "Done" }
        }],
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": 18,
            "total_tokens": 30,
            "prompt_tokens_details": {
                "cache_write_tokens": 2,
                "cached_tokens": 4
            },
            "completion_tokens_details": {
                "reasoning_tokens": 8
            }
        }
    });
    let conv_full = chat_to_responses(resp_full_usage, "test-model");
    assert_eq!(conv_full["status"], "completed");
    assert_eq!(conv_full["usage"]["input_tokens"], 12);
    assert_eq!(conv_full["usage"]["output_tokens"], 18);
    assert_eq!(conv_full["usage"]["total_tokens"], 30);
    assert_eq!(
        conv_full["usage"]["input_tokens_details"]["cache_write_tokens"],
        2
    );
    assert_eq!(
        conv_full["usage"]["input_tokens_details"]["cached_tokens"],
        4
    );
    assert_eq!(
        conv_full["usage"]["output_tokens_details"]["reasoning_tokens"],
        8
    );

    // 2. Incomplete usage (missing total_tokens): omit entire client usage, never invent total=input+output
    let resp_incomplete_usage = json!({
        "id": "chatcmpl_usage2",
        "created": 1700000014,
        "choices": [{
            "finish_reason": "stop",
            "message": { "content": "Done" }
        }],
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": 18
        }
    });
    let conv_incomp = chat_to_responses(resp_incomplete_usage, "test-model");
    assert_eq!(
        conv_incomp["status"], "completed",
        "finish status must remain completed"
    );
    assert!(
        conv_incomp.get("usage").is_none(),
        "usage field must be omitted when incomplete"
    );

    // 3. Entirely missing usage: omit client usage
    let resp_no_usage = json!({
        "id": "chatcmpl_usage3",
        "created": 1700000015,
        "choices": [{
            "finish_reason": "stop",
            "message": { "content": "Done" }
        }]
    });
    let conv_no = chat_to_responses(resp_no_usage, "test-model");
    assert_eq!(conv_no["status"], "completed");
    assert!(conv_no.get("usage").is_none());
}

#[test]
fn test_chat_to_responses_usage_validation_is_complete_and_warns_once() {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;

    #[derive(Default)]
    struct Fields(BTreeMap<String, String>);
    impl tracing::field::Visit for Fields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }
    struct Warnings(Arc<Mutex<Vec<Fields>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() == "llm_proxy::convert"
                && *event.metadata().level() == tracing::Level::WARN
            {
                let mut fields = Fields::default();
                event.record(&mut fields);
                self.0.lock().unwrap().push(fields);
            }
        }
    }

    let full = json!({
        "prompt_tokens": 10,
        "completion_tokens": 20,
        // Preserve the upstream total, even when it differs from the local sum.
        "total_tokens": 37,
        "prompt_tokens_details": {"cache_write_tokens": 3, "cached_tokens": 4},
        "completion_tokens_details": {"reasoning_tokens": 5}
    });
    let base = json!({
        "id": "usage_validation",
        "created": 1700000000,
        "choices": [{
            "finish_reason": "stop",
            "message": {"reasoning_content": "推理 \n ", "content": "Answer retained"}
        }]
    });
    let warnings = Arc::new(Mutex::new(Vec::new()));
    // Keep two live dispatchers: tracing-core's single-dispatch fast path can
    // otherwise cache a concurrent test thread's no-subscriber interest.
    // Neither dispatcher is global; unrelated tests remain fully concurrent.
    let _registration_guard = tracing::Dispatch::new(tracing_subscriber::registry());
    let subscriber = tracing_subscriber::registry().with(Warnings(Arc::clone(&warnings)));
    tracing::subscriber::with_default(subscriber, || {
        let convert = |usage: Option<Value>| {
            let mut resp = base.clone();
            if let Some(usage) = usage {
                resp["usage"] = usage;
            }
            chat_to_responses(resp, "test-model")
        };
        let valid = convert(Some(full.clone()));
        assert_eq!(
            valid["usage"],
            json!({
                "input_tokens": 10,
                "output_tokens": 20,
                "total_tokens": 37,
                "input_tokens_details": {"cache_write_tokens": 3, "cached_tokens": 4},
                "output_tokens_details": {"reasoning_tokens": 5}
            })
        );
        assert!(warnings.lock().unwrap().is_empty());

        // Absent and malformed containers are diagnosed once at their fixed path.
        let mut cases = vec![
            (None, vec!["usage"], vec![]),
            (Some(Value::Null), vec![], vec!["usage"]),
            (Some(json!("private-invalid-usage")), vec![], vec!["usage"]),
            (Some(json!([])), vec![], vec!["usage"]),
        ];
        for (pointer, field) in [
            ("/prompt_tokens", "prompt_tokens"),
            ("/completion_tokens", "completion_tokens"),
            ("/total_tokens", "total_tokens"),
            ("/prompt_tokens_details", "prompt_tokens_details"),
            ("/completion_tokens_details", "completion_tokens_details"),
            (
                "/prompt_tokens_details/cache_write_tokens",
                "prompt_tokens_details.cache_write_tokens",
            ),
            (
                "/prompt_tokens_details/cached_tokens",
                "prompt_tokens_details.cached_tokens",
            ),
            (
                "/completion_tokens_details/reasoning_tokens",
                "completion_tokens_details.reasoning_tokens",
            ),
        ] {
            let mut missing = full.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            missing
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            cases.push((Some(missing), vec![field], vec![]));
            for invalid in [
                Value::Null,
                json!(-7),
                json!(1.5),
                json!("private-invalid-count"),
                json!(true),
                json!([]),
            ] {
                let mut usage = full.clone();
                *usage.pointer_mut(pointer).unwrap() = invalid;
                cases.push((Some(usage), vec![], vec![field]));
            }
            if !pointer.ends_with("_details") {
                for invalid in [json!({}), json!(u64::MAX)] {
                    let mut usage = full.clone();
                    *usage.pointer_mut(pointer).unwrap() = invalid;
                    cases.push((Some(usage), vec![], vec![field]));
                }
            }
        }
        cases.push((
            Some(json!({
                "completion_tokens": -7,
                "total_tokens": null,
                "prompt_tokens_details": {"cache_write_tokens": "private-invalid-count"},
                "completion_tokens_details": {}
            })),
            vec![
                "prompt_tokens",
                "prompt_tokens_details.cached_tokens",
                "completion_tokens_details.reasoning_tokens",
            ],
            vec![
                "completion_tokens",
                "total_tokens",
                "prompt_tokens_details.cache_write_tokens",
            ],
        ));
        for (usage, missing, invalid) in cases {
            let converted = convert(usage);
            assert!(converted.get("usage").is_none(), "{converted}");
            assert_eq!(converted["status"], "completed");
            assert_eq!(converted.get("error"), Some(&Value::Null));
            assert_eq!(converted.get("incomplete_details"), Some(&Value::Null));
            assert_eq!(converted["output"][0]["content"][0]["text"], "推理 \n ");
            assert_eq!(
                converted["output"][1]["content"][0]["text"],
                "Answer retained"
            );
            let mut emitted = warnings.lock().unwrap();
            assert_eq!(
                emitted.len(),
                1,
                "one converter warning per invalid usage: missing={missing:?}, invalid={invalid:?}"
            );
            let fields = &emitted[0].0;
            assert_eq!(fields["missing_fields"], format!("{missing:?}"));
            assert_eq!(fields["invalid_fields"], format!("{invalid:?}"));
            assert_eq!(fields["correlation_id"], "usage_validation");
            assert!(!format!("{fields:?}").contains("private-invalid"));
            emitted.clear();
        }

        let mut incomplete = base.clone();
        incomplete["choices"][0]["finish_reason"] = json!("length");
        incomplete["usage"] = json!({"prompt_tokens": 10});
        let converted = chat_to_responses(incomplete, "test-model");
        assert_eq!(converted["status"], "incomplete");
        assert_eq!(
            converted["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert!(converted.get("usage").is_none());
        assert_eq!(
            converted["output"][1]["content"][0]["text"],
            "Answer retained"
        );
        assert_eq!(warnings.lock().unwrap().len(), 1);
    });
}

#[test]
fn test_chat_to_responses_original_findings_and_complete_failed_envelope() {
    let call = json!({
        "id": "call_original",
        "type": "function",
        "function": {"name": "lookup", "arguments": "{}"}
    });
    let base = json!({
        "id": "chatcmpl_findings",
        "created": 1700000000,
        "choices": [{
            "finish_reason": "stop",
            "message": {"reasoning_content": "思考 \n ", "content": "Answer", "tool_calls": [call]}
        }]
    });
    let request = json!({
        "tools": [{"type": "function", "name": "lookup", "parameters": {}, "strict": false}],
        "tool_choice": {"type": "function", "name": "lookup"},
        "parallel_tool_calls": false
    });
    let mut failures = Vec::new();
    let mut missing_type = base.clone();
    missing_type["choices"][0]["message"]["tool_calls"][0]
        .as_object_mut()
        .unwrap()
        .remove("type");
    failures.push((missing_type, "Invalid tool call type in upstream response"));
    let mut duplicate = base.clone();
    duplicate["choices"][0]["message"]["tool_calls"] = json!([call, call]);
    failures.push((duplicate, "Duplicate tool call id in upstream response"));
    let mut reasoning_object = base.clone();
    reasoning_object["choices"][0]["message"]["reasoning_content"] =
        json!({"text": "not a string"});
    failures.push((
        reasoning_object,
        "Malformed reasoning_content in upstream response",
    ));
    for invalid_type in [Value::Null, json!(42), json!(false), json!({}), json!([])] {
        let mut resp = base.clone();
        resp["choices"][0]["message"]["tool_calls"][0]["type"] = invalid_type;
        failures.push((resp, "Invalid tool call type in upstream response"));
    }
    let mut message_finish = base.clone();
    message_finish["choices"][0]
        .as_object_mut()
        .unwrap()
        .remove("finish_reason");
    message_finish["choices"][0]["message"]["finish_reason"] = json!("stop");
    failures.push((
        message_finish,
        "Missing or invalid finish reason from upstream",
    ));
    let mut inband_error = base.clone();
    inband_error["error"] = json!({"message": "private upstream failure"});
    failures.push((inband_error, "Upstream service error"));

    for (resp, error_message) in failures {
        for context in [None, Some(&request)] {
            let converted = chat_to_responses_with_context(resp.clone(), "test-model", context);
            let mut expected = json!({
                "id": "chatcmpl_findings",
                "object": "response",
                "created_at": 1700000000,
                "model": "test-model",
                "status": "failed",
                "error": {"code": "server_error", "message": error_message},
                "incomplete_details": null,
                "tools": [],
                "tool_choice": "none",
                "parallel_tool_calls": true,
                "output": []
            });
            if context.is_some() {
                for field in ["tools", "tool_choice", "parallel_tool_calls"] {
                    expected[field] = request[field].clone();
                }
            }
            assert_eq!(converted, expected);
        }
    }

    for (finish, reason) in [
        ("length", "max_output_tokens"),
        ("content_filter", "content_filter"),
    ] {
        let mut truncated = base.clone();
        truncated["choices"][0]["finish_reason"] = json!(finish);
        truncated["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
            json!("{\"partial\":");
        let converted = chat_to_responses(truncated, "test-model");
        assert_eq!(converted["status"], "incomplete");
        assert_eq!(converted.get("error"), Some(&Value::Null));
        assert_eq!(converted["incomplete_details"]["reason"], reason);
        let output = converted["output"].as_array().unwrap();
        assert_eq!(output.len(), 3);
        for item in output {
            assert_eq!(item["status"], "incomplete", "{item}");
        }
        assert_eq!(output[0]["content"][0]["text"], "思考 \n ");
        assert_eq!(output[1]["content"][0]["text"], "Answer");
        assert_eq!(output[2]["call_id"], "call_original");
        assert_eq!(output[2]["arguments"], "{\"partial\":");
    }
}

#[test]
fn test_chat_to_responses_request_context_defaults() {
    let resp = json!({
        "id": "chatcmpl_ctx",
        "created": 1700000016,
        "choices": [{
            "finish_reason": "stop",
            "message": { "content": "Answer without tools" }
        }]
    });

    let tools_def = json!([{
        "type": "function",
        "name": "lookup",
        "description": "Look up item",
        "parameters": {"type": "object"}
    }]);

    // Request context with tools but omitted tool_choice / parallel_tool_calls:
    // reflects tools and effective protocol defaults (tool_choice="auto", parallel_tool_calls=true)
    let req_with_tools = json!({
        "model": "frontend-model",
        "tools": tools_def
    });
    let conv_with_tools =
        chat_to_responses_with_context(resp.clone(), "frontend-model", Some(&req_with_tools));
    assert_eq!(conv_with_tools["tools"], tools_def);
    assert_eq!(conv_with_tools["tool_choice"], "auto");
    assert_eq!(conv_with_tools["parallel_tool_calls"], true);

    // Request context with explicit tool_choice and parallel_tool_calls
    let req_explicit = json!({
        "model": "frontend-model",
        "tools": tools_def,
        "tool_choice": "none",
        "parallel_tool_calls": false
    });
    let conv_explicit =
        chat_to_responses_with_context(resp.clone(), "frontend-model", Some(&req_explicit));
    assert_eq!(conv_explicit["tool_choice"], "none");
    assert_eq!(conv_explicit["parallel_tool_calls"], false);

    // No requested tools: emit required envelope defaults, not inferred tools
    // or an inferred choice based on the generated function_call.
    let resp_with_call = json!({
        "id": "chatcmpl_ctx2",
        "created": 1700000017,
        "choices": [{
            "finish_reason": "stop",
            "message": {
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "f1", "arguments": "{}" }
                }]
            }
        }]
    });
    let req_no_tools = json!({
        "model": "frontend-model"
    });
    let conv_no_tools =
        chat_to_responses_with_context(resp_with_call, "frontend-model", Some(&req_no_tools));
    assert_eq!(conv_no_tools["tools"], json!([]));
    assert_eq!(conv_no_tools["tool_choice"], "none");
    assert_eq!(conv_no_tools["parallel_tool_calls"], true);
}

#[test]
fn test_anthropic_to_responses_core_logic() {
    let req = json!({
        "system": "Anthropic system prompt",
        "messages": [
            {"role": "user", "content": "Query"}
        ],
        "max_tokens": 300,
        "thinking": {"type": "enabled", "budget_tokens": 2048}
    });

    let out = anthropic_to_responses_request(req, "target-model")
        .expect("anthropic_to_responses_request failed");

    assert_eq!(out["model"], "target-model");
    assert_eq!(out["instructions"], "Anthropic system prompt");
    assert_eq!(out["max_output_tokens"], 300);

    let anthropic_resp = json!({
        "id": "msg_anthropic_1",
        "content": [
            {"type": "thinking", "thinking": "Let me think..."},
            {"type": "text", "text": "Answer from Anthropic"}
        ]
    });

    let converted_resp = anthropic_to_responses(anthropic_resp, "frontend-model");
    assert_eq!(converted_resp["id"], "msg_anthropic_1");

    let output = converted_resp["output"].as_array().expect("output array");
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "Answer from Anthropic");
}

#[test]
fn test_responses_to_anthropic_core_logic() {
    let req = json!({
        "instructions": "Be helpful",
        "input": "User query",
        "max_output_tokens": 400,
        "reasoning": {"effort": "medium"}
    });

    let out =
        responses_to_anthropic(req, "claude-3-5-sonnet").expect("responses_to_anthropic failed");

    assert_eq!(out["model"], "claude-3-5-sonnet");
    assert_eq!(out["system"], "Be helpful");
    assert_eq!(out["max_tokens"], 400);
    assert_eq!(out["thinking"]["type"], "adaptive");
    assert_eq!(out["output_config"]["effort"], "medium");

    let responses_resp = json!({
        "id": "resp_anth_out",
        "output": [
            {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "Pondering..."}]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Anthropic style answer"}]
            }
        ],
        "usage": {"input_tokens": 5, "output_tokens": 15}
    });

    let out_msg = responses_to_anthropic_response(responses_resp, "frontend-model");
    // a `resp_`-prefixed id must never surface on the Anthropic Messages
    // slot; the proxy regenerates one in the `msg_` namespace instead.
    let anthropic_id = out_msg["id"].as_str().expect("anthropic id");
    assert!(
        anthropic_id.starts_with("msg_"),
        "Anthropic surface leaked {anthropic_id}"
    );
    assert_ne!(anthropic_id, "resp_anth_out");
    let content = out_msg["content"].as_array().expect("content array");
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "Pondering...");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "Anthropic style answer");
}

#[test]
fn test_extract_text_from_content_unit() {
    assert_eq!(extract_text_from_content(None), "");
    assert_eq!(extract_text_from_content(Some(&json!(null))), "");
    assert_eq!(extract_text_from_content(Some(&json!(123))), "");

    assert_eq!(extract_text_from_content(Some(&json!("hello"))), "hello");

    let array_val = json!([
        {"type": "input_text", "text": "hello "},
        {"type": "text", "text": "world"},
        "!"
    ]);
    assert_eq!(extract_text_from_content(Some(&array_val)), "hello world!");

    let obj_val = json!({"output_text": "object text"});
    assert_eq!(extract_text_from_content(Some(&obj_val)), "object text");
}

#[test]
fn test_normalize_tool_calls_unit() {
    let calls = json!([
        {
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "get_weather",
                "arguments": "{\"location\":\"Beijing\"}"
            }
        },
        {
            "call_id": "call_2",
            "name": "calculator",
            "arguments": {"expr": "2+2"}
        },
        {
            "id": "call_3",
            "function": {
                "name": "invalid_args",
                "arguments": "bad json"
            }
        }
    ]);

    let normalized = normalize_tool_calls(calls.as_array().unwrap());

    assert_eq!(normalized.len(), 3);
    assert_eq!(normalized[0]["id"], "call_1");
    assert_eq!(normalized[0]["function"]["name"], "get_weather");
    assert_eq!(
        normalized[0]["function"]["arguments"],
        "{\"location\":\"Beijing\"}"
    );

    assert_eq!(normalized[1]["id"], "call_2");
    assert_eq!(normalized[1]["function"]["name"], "calculator");
    assert_eq!(normalized[1]["function"]["arguments"], "{\"expr\":\"2+2\"}");

    assert_eq!(normalized[2]["id"], "call_3");
    assert_eq!(normalized[2]["function"]["name"], "invalid_args");
    assert_eq!(normalized[2]["function"]["arguments"], "{}");
}

#[test]
fn responses_to_anthropic_response_infers_stop_and_defaults_usage() {
    let resp = json!({
        "output": [
            {
                "type": "message",
                "content": [
                    {"type": "text", "text": "hello "},
                    {"type": "output_text", "text": "world"}
                ]
            },
            {
                "type": "function_call",
                "id": "call_1",
                "name": "lookup",
                "arguments": "not-json"
            },
            {
                "type": "reasoning",
                "summary": [
                    {"text": "chain"},
                    {"text": " of thought"}
                ],
                "signature": "sig-1"
            }
        ],
        "status": "incomplete"
    });

    let converted = responses_to_anthropic_response(resp, "front-model");

    // Missing Responses id ⇒ generated Anthropic id, never a constant (P5/P7).
    let generated_id = converted["id"].as_str().expect("generated msg id");
    assert!(
        generated_id.starts_with("msg_") && generated_id.len() > "msg_".len(),
        "Anthropic id must be generated in msg_ namespace: {generated_id}"
    );
    assert_eq!(converted["model"], "front-model");
    assert_eq!(converted["stop_reason"], "tool_use");
    assert_eq!(
        converted["usage"],
        json!({"input_tokens": 0, "output_tokens": 0})
    );
    assert_eq!(
        converted["content"][0],
        json!({"type": "text", "text": "hello "})
    );
    assert_eq!(
        converted["content"][1],
        json!({
            "type": "text",
            "text": "world"
        })
    );
    assert_eq!(
        converted["content"][2],
        json!({
            "type": "tool_use",
            "id": "call_1",
            "name": "lookup",
            "input": {}
        })
    );
    assert_eq!(
        converted["content"][3],
        json!({
            "type": "thinking",
            "thinking": "chain of thought",
            "signature": "sig-1"
        })
    );
}

#[test]
fn anthropic_to_responses_request_converts_mixed_blocks_and_tools() {
    let req = json!({
        "system": [
            {"type": "text", "text": "be careful"},
            {"type": "cache_control", "text": "ignored"}
        ],
        "stream": true,
        "temperature": 0.4,
        "top_p": 0.8,
        "max_tokens": 128,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "tool_use", "id": "tool-1", "name": "calc", "input": {"x": 1}},
                    {"type": "thinking", "thinking": "hidden"}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "abc"}},
                    {"type": "document", "title": "spec", "source": {"type": "url", "url": "https://example.com/spec.pdf"}},
                    {"type": "tool_result", "tool_use_id": "tool-1", "content": [{"stdout": "4"}, {"text": "2"}]},
                    "literal"
                ]
            }
        ],
        "tools": [
            {"name": "calc", "description": "Calculator", "input_schema": {"type": "object", "properties": {"x": {"type": "number"}}}},
            {"description": "missing name"}
        ],
        "tool_choice": {"type": "tool", "name": "calc"}
    });

    let converted = anthropic_to_responses_request(req, "upstream").unwrap();

    assert_eq!(converted["model"], "upstream");
    assert_eq!(converted["instructions"], "be careful\nignored");
    assert_eq!(
        converted["tools"],
        json!([{
            "type": "function",
            "name": "calc",
            "description": "Calculator",
            "parameters": {"type": "object", "properties": {"x": {"type": "number"}}}
        }])
    );
    assert_eq!(
        converted["tool_choice"],
        json!({"type": "function", "function": {"name": "calc"}})
    );
    let input = converted["input"].as_array().unwrap();
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "assistant");
    assert_eq!(
        input[0]["content"],
        json!([{ "type": "output_text", "text": "answer" }])
    );
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "tool-1");
    assert_eq!(input[1]["arguments"], "{\"x\":1}");
    assert_eq!(input[2]["type"], "message");
    assert_eq!(input[2]["role"], "assistant");
    // thinking blocks are now explicitly converted to reasoning
    // parts instead of falling through to the catch-all text serialisation.
    assert_eq!(input[2]["content"][0]["type"], "reasoning");
    assert_eq!(input[2]["content"][0]["text"], "hidden");
    assert_eq!(input[3]["type"], "message");
    assert_eq!(input[3]["role"], "user");
    assert_eq!(input[3]["content"][0]["type"], "input_image");
    assert_eq!(
        input[3]["content"][1],
        json!({
            "type": "input_file",
            "filename": "spec",
            "file_url": "https://example.com/spec.pdf"
        })
    );
    assert_eq!(input[4]["type"], "function_call_output");
    assert_eq!(input[4]["output"], "42");
    assert_eq!(
        input[5]["content"],
        json!([{ "type": "input_text", "text": "\"literal\"" }])
    );
}

#[test]
fn responses_to_anthropic_converts_tooling_defaults_and_auto_reasoning() {
    let req = json!({
        "instructions": "sys",
        "input": [
            {"type": "message", "role": "developer", "content": [
                {"type": "input_text", "text": "policy"},
                {"type": "input_image", "image_url": "https://img"},
                {"type": "input_file", "filename": "doc.pdf", "file_data": "data:application/pdf;base64,Zm9v"}
            ]},
            {"type": "function_call", "call_id": "c1", "name": "sum", "arguments": "{\"n\":2}"},
            {"type": "function_call_output", "call_id": "c1", "output": {"result": 2}}
        ],
        "tools": [
            {"type": "function", "function": {"name": "sum", "description": "adder", "parameters": {"type": "object"}}},
            {"type": "other"}
        ],
        "reasoning": {"effort": "weird"},
        "stream": true,
        "max_output_tokens": 77
    });

    let converted = responses_to_anthropic(req, "claude-upstream").unwrap();

    assert_eq!(converted["model"], "claude-upstream");
    assert_eq!(converted["system"], "sys");
    assert_eq!(converted["thinking"], json!({"type": "adaptive"}));
    assert_eq!(converted["output_config"], json!({"effort": "weird"}));
    assert_eq!(
        converted["tools"],
        json!([{
            "name": "sum",
            "description": "adder",
            "input_schema": {"type": "object"}
        }])
    );
    let messages = converted["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(
        messages[0]["content"][0],
        json!({"type": "text", "text": "policy"})
    );
    assert_eq!(messages[0]["content"][1]["type"], "image");
    assert_eq!(messages[0]["content"][2]["type"], "document");
    assert_eq!(
        messages[1],
        json!({
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "c1", "name": "sum", "input": {"n": 2}}]
        })
    );
    assert_eq!(
        messages[2],
        json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "c1", "content": "{\"result\":2}"}]
        })
    );
}

#[test]
fn chat_to_anthropic_request_handles_system_tool_and_named_choice() {
    let req = json!({
        "stream": true,
        "temperature": 0.2,
        "top_p": 0.9,
        "max_tokens": 33,
        "stop": ["END"],
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "rules"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "hello"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,aaa"}}
            ], "tool_calls": [{"id": "call-1", "function": {"name": "lookup", "arguments": "{\"q\":\"rust\"}"}}]},
            {"role": "tool", "tool_call_id": "call-1", "content": [{"text": "done"}]}
        ],
        "tools": [{"type": "function", "function": {"name": "lookup", "description": "search", "parameters": {"type": "object"}}}],
        "tool_choice": {"function": {"name": "lookup"}}
    });

    let converted = chat_to_anthropic_request(req, "claude-model").unwrap();

    assert_eq!(converted["system"], "rules");
    assert_eq!(
        converted["tool_choice"],
        json!({"type": "tool", "name": "lookup"})
    );
    let messages = converted["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(
        messages[0]["content"][0],
        json!({"type": "text", "text": "hello"})
    );
    assert_eq!(messages[0]["content"][1]["type"], "image");
    assert_eq!(
        messages[0]["content"][2],
        json!({
            "type": "tool_use",
            "id": "call-1",
            "name": "lookup",
            "input": {"q": "rust"}
        })
    );
    assert_eq!(
        messages[1],
        json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "call-1", "content": "done"}]
        })
    );
}

#[test]
fn anthropic_to_chat_adds_system_stream_options_and_tool_choice_mapping() {
    let req = json!({
        "system": [{"type": "text", "text": "stay concise"}],
        "stream": true,
        "stop_sequences": ["###"],
        "top_k": 3,
        "messages": [
            {"role": "assistant", "content": [
                {"type": "text", "text": "Hi"},
                {"type": "tool_use", "id": "tu1", "name": "calc", "input": {"v": 1}}
            ]},
            {"role": "user", "content": [
                {"type": "text", "text": "Use tool"},
                {"type": "tool_result", "tool_use_id": "tu1", "content": [{"text": "ok"}]}
            ]},
            {"role": "custom", "content": "raw"}
        ],
        "tools": [{"name": "calc", "description": "math", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "any"}
    });

    let converted = anthropic_to_chat(req, "chat-upstream").unwrap();

    assert_eq!(converted["tool_choice"], "required");
    assert_eq!(converted["stream_options"], json!({"include_usage": true}));
    let messages = converted["messages"].as_array().unwrap();
    assert_eq!(
        messages[0],
        json!({"role": "system", "content": "stay concise"})
    );
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "calc");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]["content"],
        json!([{"type": "text", "text": "Use tool"}])
    );
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "tu1", "content": "ok"})
    );
    assert_eq!(messages[4], json!({"role": "custom", "content": "raw"}));
}

#[test]
fn chat_to_anthropic_response_maps_tool_calls_and_length_stop() {
    let resp = json!({
        "id": "chat-1",
        "choices": [{
            "message": {
                "content": [
                    {"type": "text", "text": "prefix "},
                    {"type": "text", "text": "suffix"}
                ],
                "tool_calls": [{
                    "id": "call-z",
                    "function": {"name": "compute", "arguments": "bad json"}
                }]
            },
            "finish_reason": "length"
        }],
        "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
    });

    let converted = chat_to_anthropic(resp, "claude-front");

    // the Chat `chat-1` id is not surfaced on the Anthropic slot.
    assert!(converted["id"].as_str().expect("id").starts_with("msg_"));
    assert_eq!(
        converted["content"][0],
        json!({"type": "text", "text": "prefix suffix"})
    );
    assert_eq!(
        converted["content"][1],
        json!({
            "type": "tool_use",
            "id": "call-z",
            "name": "compute",
            "input": {}
        })
    );
    assert_eq!(converted["stop_reason"], "max_tokens");
    assert_eq!(
        converted["usage"],
        json!({"input_tokens": 2, "output_tokens": 3})
    );
}

#[test]
fn c1_history_preserves_narrow_reasoning_and_boundaries() {
    let r = json!({"type":"reasoning","summary":[],"content":[
        {"type":"reasoning_text","text":" \t思考🦀\r\n"},
        {"type":"reasoning_text","text":""},
        {"type":"reasoning_text","text":"e\u{301} 重复重复 "}
    ]});
    let r_text = " \t思考🦀\r\ne\u{301} 重复重复 ";
    let t1 = json!({"type":"message","role":"assistant","content":"正文一\n"});
    let t2 = json!({"type":"message","role":"assistant","content":[
        {"type":"output_text","text":"正文二"},{"type":"output_text","text":"\t尾 "}
    ]});
    let t1_chat = json!({"role":"assistant","content":"正文一\n"});
    let t2_chat = json!({"role":"assistant","content":[
        {"type":"text","text":"正文二"},{"type":"text","text":"\t尾 "}
    ]});
    let mut rt1 = t1_chat.clone();
    rt1["reasoning_content"] = json!(r_text);
    let mut rt2 = t2_chat.clone();
    rt2["reasoning_content"] = json!(r_text);
    // Raw strings are not parsed or normalized, including incomplete JSON.
    let args1 = " \n{\"字\": \"🦀\", \"n\":1}\t ";
    let args2 = "{\"unfinished\":";
    let c1 =
        json!({"type":"function_call","id":"item_1","call_id":"c1","name":"one","arguments":args1});
    let c2 =
        json!({"type":"function_call","id":"item_2","call_id":"c2","name":"two","arguments":args2});
    let calls = json!({"role":"assistant","tool_calls":[
        {"id":"c1","type":"function","function":{"name":"one","arguments":args1}},
        {"id":"c2","type":"function","function":{"name":"two","arguments":args2}}
    ]});
    let mut rcalls = calls.clone();
    rcalls["reasoning_content"] = json!(r_text);
    let o1 = json!({"type":"function_call_output","call_id":"c1","output":" \r\n结果一🦀"});
    let o2 = json!({"type":"function_call_output","call_id":"c2","output":"结果二\t "});
    let out1 = json!({"role":"tool","tool_call_id":"c1","content":" \r\n结果一🦀"});
    let out2 = json!({"role":"tool","tool_call_id":"c2","content":"结果二\t "});
    let empty = json!({"type":"message","role":"assistant","content":[]});

    for (name, input, expected) in [
        (
            "R,T,C1,C2,O1,O2",
            json!([r, t1, c1, c2, o1, o2]),
            json!([rt1, calls, out1, out2]),
        ),
        (
            "R,C1,C2,O2,O1",
            json!([r, c1, c2, o2, o1]),
            json!([rcalls, out2, out1]),
        ),
        (
            "T1,R,T2,calls,outputs,T1",
            json!([t1, r, t2, c1, c2, o1, o2, t1]),
            json!([t1_chat, rt2, calls, out1, out2, t1_chat]),
        ),
        ("R,T1,T2", json!([r, t1, t2]), json!([rt1, t2_chat])),
        (
            "empty assistant before and within calls",
            json!([r, empty, c1, empty, c2, o1, o2]),
            json!([rcalls, out1, out2]),
        ),
        (
            "R after completed group",
            json!([c1, c2, o1, o2, r, t1]),
            json!([calls, out1, out2, rt1]),
        ),
        (
            "R consumed once across turns",
            json!([r, t1, c1, c2, o1, o2, t2]),
            json!([rt1, calls, out1, out2, t2_chat]),
        ),
    ] {
        let converted = responses_to_chat(json!({"input":input}), "upstream").expect(name);
        assert_eq!(converted["messages"], expected, "{name}");
    }

    let converted = responses_to_chat(json!({"input":[r,r,t1]}), "upstream").unwrap();
    rt1["reasoning_content"] = json!(format!("{r_text}{r_text}"));
    assert_eq!(converted["messages"], json!([rt1]), "no separator or dedup");
}

#[test]
fn c1_history_empty_reasoning_is_ignored() {
    let t = json!({"role":"assistant","content":"text"});
    let c1 = json!({"type":"function_call","call_id":"c1","name":"f","arguments":""});
    let c2 = json!({"type":"function_call","call_id":"c2","name":"f","arguments":"{}"});
    let o1 = json!({"type":"function_call_output","call_id":"c1","output":""});
    let o2 = json!({"type":"function_call_output","call_id":"c2","output":""});
    for r in [
        json!({"type":"reasoning"}),
        json!({"type":"reasoning","content":null,"summary":null,"encrypted_content":null}),
        json!({"type":"reasoning","content":[],"summary":[],"encrypted_content":""}),
        json!({"type":"reasoning","content":[{"type":"reasoning_text","text":""}]}),
    ] {
        for (input, expected) in [
            (json!([r]), json!([])),
            (json!([r, t]), json!([t])),
            (json!([t, r]), json!([t])),
        ] {
            let converted = responses_to_chat(json!({"input":input}), "upstream").unwrap();
            assert_eq!(converted["messages"], expected, "{r}");
        }
        let input = json!([c1, r, c2, o2, o1]);
        let converted = responses_to_chat(json!({"input":input}), "upstream").unwrap();
        assert_eq!(converted["messages"].as_array().unwrap().len(), 3);
        assert_eq!(
            converted["messages"][0]["tool_calls"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(converted["messages"][0].get("reasoning_content").is_none());
        let converted = responses_to_chat(
            json!({"input":[
                {"type":"reasoning","content":[{"type":"reasoning_text","text":" \t"}]},r,t
            ]}),
            "upstream",
        )
        .unwrap();
        assert_eq!(
            converted["messages"],
            json!([{"role":"assistant","content":"text","reasoning_content":" \t"}])
        );
    }
}

#[test]
fn c1_history_empty_text_parts_do_not_consume_reasoning() {
    let converted = responses_to_chat(json!({"input":[
        {"type":"reasoning","content":[{"type":"reasoning_text","text":"R"}]},
        {"role":"assistant","content":[{"type":"output_text","text":""},{"type":"output_text","text":""}]},
        {"role":"assistant","content":" "}
    ]}), "upstream").unwrap();
    assert_eq!(
        converted["messages"],
        json!([{"role":"assistant","content":" ","reasoning_content":"R"}])
    );
}

#[test]
fn c1_history_rejects_unsupported_or_malformed_reasoning() {
    let base = json!({"type":"reasoning","content":[{"type":"reasoning_text","text":"PRIVATE_REASONING"}]});
    let mut cases = Vec::new();
    for content in [
        json!("PRIVATE_REASONING"),
        json!({}),
        json!(7),
        json!([null]),
        json!(["text"]),
        json!([{}]),
        json!([{"type":"output_text","text":"PRIVATE_REASONING"}]),
        json!([{"type":"reasoning_text"}]),
        json!([{"type":"reasoning_text","text":null}]),
        json!([{"type":"reasoning_text","text":7}]),
        json!([{"type":"reasoning_text","text":["PRIVATE_REASONING"]}]),
    ] {
        let mut r = base.clone();
        r["content"] = content;
        cases.push(r);
    }
    for summary in [
        json!([{"type":"summary_text","text":"PRIVATE_SUMMARY"}]),
        json!([{"type":"summary_text","text":""}]),
        json!(""),
        json!({}),
        json!(false),
    ] {
        let mut r = base.clone();
        r["summary"] = summary;
        cases.push(r);
    }
    for encrypted in [
        json!("PRIVATE_CIPHERTEXT"),
        json!(" "),
        json!([]),
        json!(false),
        json!(1),
    ] {
        let mut r = base.clone();
        r["encrypted_content"] = encrypted;
        cases.push(r);
    }
    for r in cases {
        let err = responses_to_chat(
            json!({"input":[r,{"role":"assistant","content":"text"}]}),
            "upstream",
        )
        .expect_err("unsupported reasoning must not be dropped");
        let message = err.to_string();
        assert!(message.contains("reasoning"), "{message}");
        assert!(
            !message.contains("PRIVATE"),
            "diagnostic must not expose content"
        );
    }
}

#[test]
fn c1_history_rejects_orphans_adjacency_and_invalid_identities() {
    let r = json!({"type":"reasoning","content":[{"type":"reasoning_text","text":"R"}]});
    let t = json!({"role":"assistant","content":"T"});
    let c1 = json!({"type":"function_call","call_id":"c1","name":"f","arguments":"{}"});
    let c2 = json!({"type":"function_call","call_id":"c2","name":"g","arguments":"{}"});
    let o1 = json!({"type":"function_call_output","call_id":"c1","output":"one"});
    let o2 = json!({"type":"function_call_output","call_id":"c2","output":"two"});
    for (name, input) in [
        ("orphan", json!([r])),
        (
            "empty assistant cannot carry R",
            json!([r,{"role":"assistant","content":""}]),
        ),
        ("user boundary", json!([r,{"role":"user","content":"u"},t])),
        (
            "system boundary",
            json!([r,{"role":"system","content":"s"},t]),
        ),
        (
            "developer boundary",
            json!([r,{"role":"developer","content":"d"},t]),
        ),
        ("output boundary", json!([c1, r, o1, t])),
        ("C1,R,C2,O1,O2", json!([c1, r, c2, o1, o2])),
        ("R,C1,R,C2,O1,O2", json!([r, c1, r, c2, o1, o2])),
        ("assistant before outputs", json!([c1, t, o1])),
        ("assistant before all outputs", json!([c1, c2, o1, t, o2])),
        (
            "new group before all outputs",
            json!([c1,c2,o1,{"type":"function_call","call_id":"c3","name":"h","arguments":"{}"},o2]),
        ),
        ("missing all outputs", json!([c1])),
        ("missing one output", json!([c1, c2, o1])),
        ("foreign output", json!([c1, o2])),
        ("output before call", json!([o1, c1])),
        ("duplicate output", json!([c1, o1, o1])),
        ("duplicate ID in group", json!([c1, c1, o1])),
        ("duplicate ID across groups", json!([c1, o1, c1, o1])),
    ] {
        assert!(
            responses_to_chat(json!({"input":input}), "upstream").is_err(),
            "{name}"
        );
    }
    for field in ["call_id", "name", "arguments"] {
        for invalid in [
            None,
            Some(Value::Null),
            Some(json!(9)),
            Some(json!({})),
            Some(json!([])),
            Some(json!("")),
        ] {
            if field == "arguments" && invalid == Some(json!("")) {
                continue; // The contract preserves raw arguments, including empty strings.
            }
            let mut call = c1.clone();
            if let Some(value) = invalid {
                call[field] = value;
            } else {
                call.as_object_mut().unwrap().remove(field);
            }
            assert!(
                responses_to_chat(json!({"input":[call,o1]}), "upstream").is_err(),
                "{field}"
            );
        }
    }
    for id in [None, Some(Value::Null), Some(json!("")), Some(json!(1))] {
        let mut output = o1.clone();
        if let Some(value) = id {
            output["call_id"] = value;
        } else {
            output.as_object_mut().unwrap().remove("call_id");
        }
        assert!(responses_to_chat(json!({"input":[c1,output]}), "upstream").is_err());
    }
    let mut invalid_call_id = c1.clone();
    invalid_call_id["id"] = json!("c1");
    invalid_call_id["call_id"] = Value::Null;
    assert!(
        responses_to_chat(json!({"input":[invalid_call_id,o1]}), "upstream").is_err(),
        "explicit malformed call_id cannot fall back to id"
    );
}

#[test]
fn c1_history_preserves_id_only_and_shell_paths() {
    for (call_type, output_type) in [
        ("function_call", "function_call_output"),
        ("shell_call", "shell_call_output"),
        ("local_shell_call", "local_shell_call_output"),
    ] {
        let mut call = json!({"type":call_type,"id":"legacy_id"});
        let (name, arguments) = if call_type == "function_call" {
            call["name"] = json!("lookup");
            call["arguments"] = json!("  {\"x\":1}\n");
            ("lookup", "  {\"x\":1}\n".to_string())
        } else {
            let action = json!({"command":["printf","🦀\n"],"working_directory":"/tmp"});
            call["action"] = action.clone();
            ("shell", action.to_string())
        };
        let output = json!({"type":output_type,"call_id":"legacy_id","output":[
            {"stdout":" 一\n"},{"text":"🦀\t"},{"stdout":"尾 "}
        ]});
        let converted = responses_to_chat(
            json!({"input":[
                {"type":"reasoning","content":[{"type":"reasoning_text","text":"R"}]},
                call,output
            ]}),
            "upstream",
        )
        .unwrap();
        assert_eq!(
            converted["messages"],
            json!([
                {"role":"assistant","reasoning_content":"R","tool_calls":[
                    {"id":"legacy_id","type":"function","function":{"name":name,"arguments":arguments}}
                ]},
                {"role":"tool","tool_call_id":"legacy_id","content":" 一\n🦀\t尾 "}
            ])
        );
        if call_type != "function_call" {
            for action in [None, Some(Value::Null), Some(json!("pwd")), Some(json!([]))] {
                let mut invalid = call.clone();
                if let Some(value) = action {
                    invalid["action"] = value;
                } else {
                    invalid.as_object_mut().unwrap().remove("action");
                }
                assert!(responses_to_chat(json!({"input":[invalid,output]}), "upstream").is_err());
            }
        }
    }
}

#[test]
fn responses_to_chat_handles_pending_shell_calls_and_skips_empty_assistant() {
    // Existing role-mapping/shell fixture: all outputs are present before any
    // new assistant/call group. Grouping does not add general role-order validation.
    let req = json!({
        "input": [
            {"type": "shell_call", "call_id": "s1", "action": {"command": ["pwd"]}},
            {"type": "message", "role": "assistant", "content": []},
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
            {"type": "shell_call_output", "call_id": "s1", "output": [{"stdout": "/tmp"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_image", "image_url": "https://example.com/i.png"}]}
        ]
    });

    let converted = responses_to_chat(req, "gpt-up").unwrap();
    let messages = converted["messages"].as_array().unwrap();

    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "shell");
    assert_eq!(messages[1], json!({"role": "system", "content": "rules"}));
    assert_eq!(
        messages[2],
        json!({"role": "tool", "tool_call_id": "s1", "content": "/tmp"})
    );
    assert_eq!(
        messages[3]["content"],
        json!([{"type": "image_url", "image_url": {"url": "https://example.com/i.png"}}])
    );
}

#[test]
fn convert_tools_supports_shell_namespace_and_defaults() {
    let tools = json!([
        {"type": "function", "name": "direct", "description": "d", "parameters": {"type": "object", "properties": {"a": {"type": "string"}}}},
        {"type": "shell"},
        {"type": "namespace", "name": "math", "tools": [
            {"name": "sum", "description": "adder", "parameters": {"type": "object"}},
            {"description": "skip me"}
        ]},
        "ignore"
    ]);

    let converted = convert_tools(tools.as_array().unwrap());

    assert_eq!(converted.len(), 3);
    assert_eq!(converted[0]["function"]["name"], "direct");
    assert_eq!(converted[1]["function"]["name"], "shell");
    assert_eq!(
        converted[1]["function"]["parameters"]["required"],
        json!(["command"])
    );
    assert_eq!(
        converted[2],
        json!({
            "type": "function",
            "function": {
                "name": "math__sum",
                "description": "adder",
                "parameters": {"type": "object"}
            }
        })
    );
}

#[test]
fn normalize_tool_input_object_and_extract_tool_output_cover_edge_shapes() {
    assert_eq!(normalize_tool_input_object(None), json!({}));
    assert_eq!(normalize_tool_input_object(Some(&json!(""))), json!({}));
    assert_eq!(normalize_tool_input_object(Some(&json!("[]"))), json!({}));
    assert_eq!(
        normalize_tool_input_object(Some(&json!({"x": 1}))),
        json!({"x": 1})
    );

    assert_eq!(
        extract_tool_output(Some(
            &json!([{"stdout": "a"}, {"text": "b"}, {"ignored": 1}])
        )),
        "ab"
    );
    assert_eq!(extract_tool_output(Some(&json!({"x": 1}))), "{\"x\":1}");
    assert_eq!(extract_tool_output(None), "");
}

#[test]
fn responses_file_and_usage_helpers_cover_defaults_and_urls() {
    let url_doc = responses_file_to_anthropic_document(
        json!({
            "file_url": "https://example.com/file.pdf",
            "name": "report.pdf"
        })
        .as_object()
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        url_doc,
        json!({
            "type": "document",
            "source": {"type": "url", "url": "https://example.com/file.pdf"},
            "title": "report.pdf"
        })
    );

    assert_eq!(
        file_data_to_anthropic_source("rawbase64").expect("convert raw file payload"),
        json!({
            "type": "base64",
            "media_type": "application/pdf",
            "data": "rawbase64"
        })
    );
    assert_eq!(
        responses_usage_from_anthropic(Some(&json!({"input_tokens": 4}))),
        json!({
            "input_tokens": 4,
            "output_tokens": 0,
            "total_tokens": 4
        })
    );
}
#[test]
fn test_map_reasoning_effort_unit() {
    assert_eq!(map_reasoning_effort("none"), Some(0));
    assert_eq!(map_reasoning_effort("low"), Some(1024));
    assert_eq!(map_reasoning_effort("medium"), Some(4096));
    assert_eq!(map_reasoning_effort("high"), Some(16384));
    assert_eq!(map_reasoning_effort("auto"), Some(-1));
    assert_eq!(map_reasoning_effort("unknown"), None);
    assert_eq!(map_reasoning_effort(""), None);
}

// ---- Track 2 polyfill tests ----

/// redacted_thinking blocks must be stripped when converting
/// Anthropic assistant messages to Chat format, so third-party models
/// don't receive Anthropic-encrypted signatures they can't verify.
#[test]
fn anthropic_to_chat_strips_redacted_thinking() {
    let req = json!({
        "model": "test-model",
        "max_tokens": 1024,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Let me analyze this..."},
                    {"type": "redacted_thinking", "data": "base64encrypteddata"},
                    {"type": "text", "text": "Here is my answer."}
                ]
            }
        ]
    });
    let out = anthropic_to_chat(req, "test-model").expect("conversion should succeed");
    let content = out["messages"][0]["content"].as_str().unwrap();
    // thinking text is preserved
    assert!(
        content.contains("Let me analyze this..."),
        "thinking text must be preserved"
    );
    // text is preserved
    assert!(
        content.contains("Here is my answer."),
        "text must be preserved"
    );
    // redacted_thinking data must NOT appear
    assert!(
        !content.contains("base64encrypteddata"),
        "redacted_thinking must be stripped"
    );
}

/// empty-string tool_use input must be normalised to `{}`
/// in the Chat conversion path, because strict third-party endpoints
/// reject `""` as invalid JSON for function arguments.
#[test]
fn anthropic_to_chat_normalises_empty_tool_input() {
    let req = json!({
        "model": "test-model",
        "max_tokens": 1024,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "call_1", "name": "TodoRead", "input": ""}
                ]
            }
        ]
    });
    let out = anthropic_to_chat(req, "test-model").expect("conversion should succeed");
    let args = out["messages"][0]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(args, "{}", "empty-string input must be normalised to {{}}");
}

/// redacted_thinking blocks must be stripped when converting
/// Anthropic requests to Responses format.
#[test]
fn anthropic_to_responses_strips_redacted_thinking() {
    let req = json!({
        "model": "test-model",
        "max_tokens": 1024,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Reasoning step..."},
                    {"type": "redacted_thinking", "data": "secret_signature"},
                    {"type": "text", "text": "My response."}
                ]
            }
        ]
    });
    let out = anthropic_to_responses_request(req, "test-model").expect("conversion should succeed");
    let input = out["input"].as_array().unwrap();
    // Find the assistant message
    let assistant_msg = input.iter().find(|m| m["role"] == "assistant").unwrap();
    let content = assistant_msg["content"].as_array().unwrap();
    // reasoning block preserved
    assert!(
        content
            .iter()
            .any(|c| c["type"] == "reasoning" && c["text"] == "Reasoning step..."),
        "thinking must be converted to reasoning"
    );
    // text preserved
    assert!(
        content
            .iter()
            .any(|c| c["type"] == "output_text" && c["text"] == "My response."),
        "text must be preserved"
    );
    // redacted_thinking must NOT appear anywhere
    let serialized = serde_json::to_string(&out).unwrap();
    assert!(
        !serialized.contains("secret_signature"),
        "redacted_thinking data must be stripped"
    );
    assert!(
        !serialized.contains("redacted_thinking"),
        "redacted_thinking type must be stripped"
    );
}

/// empty-string tool_use arguments normalised to `{}`
/// in the Responses conversion path.
#[test]
fn anthropic_to_responses_normalises_empty_tool_input() {
    let req = json!({
        "model": "test-model",
        "max_tokens": 1024,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "call_1", "name": "TodoRead", "input": ""}
                ]
            }
        ]
    });
    let out = anthropic_to_responses_request(req, "test-model").expect("conversion should succeed");
    let input = out["input"].as_array().unwrap();
    let function_call = input.iter().find(|m| m["type"] == "function_call").unwrap();
    assert_eq!(
        function_call["arguments"].as_str().unwrap(),
        "{}",
        "empty-string input must be normalised to {{}}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// placeholder fallback opt-in
// ─────────────────────────────────────────────────────────────────────────────

/// Default behavior lock: when placeholder_enabled=false (the default via
/// `responses_to_chat`), missing function_call_output still 400 rejects.
#[test]
fn default_rejects_missing_function_call_output() {
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "message", "role": "assistant", "content": "next"}
    ]);
    // Default path (placeholder_enabled=false) must reject
    assert!(
        responses_to_chat(json!({"input": input}), "upstream").is_err(),
        "default behavior must reject missing function_call_output"
    );
    // Explicit false must also reject
    assert!(
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", false, false)
            .is_err(),
        "explicit placeholder_enabled=false must reject"
    );
}

/// Placeholder enabled: missing function_call_output → placeholder content, continues.
#[test]
fn placeholder_fills_missing_function_call_output() {
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "message", "role": "assistant", "content": "next"}
    ]);
    let (converted, events) =
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, false)
            .expect("placeholder_enabled=true must succeed");
    let messages = converted["messages"].as_array().unwrap();
    // Should have: assistant(tool_calls), tool(placeholder), assistant("next")
    assert_eq!(
        messages.len(),
        3,
        "expected 3 messages: assistant+tool+assistant"
    );
    assert_eq!(messages[0]["role"], "assistant");
    assert_eq!(messages[1]["role"], "tool");
    assert_eq!(messages[1]["tool_call_id"], "c1");
    assert_eq!(
        messages[1]["content"].as_str().unwrap(),
        "[placeholder: function_call_output missing for call_id=c1]"
    );
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "next");
    // Events should record the placeholder
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason, "missing_function_call_output");
    assert_eq!(events[0].call_id.as_deref(), Some("c1"));
}

/// Placeholder enabled: missing outputs at end of history → placeholder.
#[test]
fn placeholder_fills_missing_outputs_at_end() {
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call", "call_id": "c2", "name": "g", "arguments": "{}"}
    ]);
    let (converted, events) =
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, false)
            .expect("placeholder_enabled=true must succeed");
    let messages = converted["messages"].as_array().unwrap();
    // Should have: assistant(tool_calls=[c1,c2]), tool(placeholder c1), tool(placeholder c2)
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "assistant");
    let tool_msgs: Vec<_> = messages.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(tool_msgs.len(), 2);
    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .all(|e| e.reason == "missing_function_call_output")
    );
}

/// Placeholder enabled: orphan reasoning (between call and output) → discarded, continues.
#[test]
fn placeholder_discards_orphan_reasoning() {
    // Orphan reasoning: reasoning appears AFTER a tool call but BEFORE its output,
    // with no way to attach it to the assistant message (tool call already flushed).
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "orphan thinking"}]},
        {"type": "function_call_output", "call_id": "c1", "output": "result"}
    ]);
    let (converted, events) =
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, false)
            .expect("placeholder_enabled=true must succeed with orphan reasoning");
    let messages = converted["messages"].as_array().unwrap();
    // Should have: assistant(tool_calls), tool(output)
    assert_eq!(messages.len(), 2);
    // The assistant message should NOT have reasoning_content (orphan was discarded)
    assert!(
        messages[0].get("reasoning_content").is_none(),
        "orphan reasoning must be discarded, not attached"
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason, "orphan_reasoning_discarded");
    assert!(events[0].call_id.is_none());
}

/// Placeholder enabled: orphan reasoning at end of history → discarded.
#[test]
fn placeholder_discards_trailing_orphan_reasoning() {
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "c1", "output": "result"},
        {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "trailing thought"}]}
    ]);
    let (converted, events) =
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, false)
            .expect("placeholder_enabled=true must succeed");
    let messages = converted["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2); // assistant(tool_calls) + tool(output)
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason, "orphan_reasoning_discarded");
}

/// No-placeholder scenarios: duplicate call ID → still 400 even with placeholder enabled.
#[test]
fn duplicate_call_id_still_rejected_with_placeholder() {
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call", "call_id": "c1", "name": "g", "arguments": "{}"}
    ]);
    assert!(
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, false)
            .is_err(),
        "duplicate call ID is a protocol error, must not be placeholder'd"
    );
}

/// No-placeholder scenarios: tool output with no matching call ID → still 400.
#[test]
fn unmatched_tool_output_still_rejected_with_placeholder() {
    let input = json!([
        {"type": "function_call_output", "call_id": "foreign", "output": "data"}
    ]);
    assert!(
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, false)
            .is_err(),
        "tool output with no matching call ID is a protocol error"
    );
}

/// Placeholder enabled: new call group before all outputs → placeholder fills, continues.
#[test]
fn placeholder_fills_before_new_call_group() {
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call", "call_id": "c2", "name": "g", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "c2", "output": "two"},
        {"type": "function_call", "call_id": "c3", "name": "h", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "c3", "output": "three"}
    ]);
    let (converted, events) =
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, false)
            .expect("placeholder_enabled=true must succeed");
    let messages = converted["messages"].as_array().unwrap();
    // c1 has no output → placeholder; c2 has output; c3 has output
    let tool_msgs: Vec<_> = messages.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(tool_msgs.len(), 3); // placeholder for c1 + real c2 + real c3
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason, "missing_function_call_output");
    assert_eq!(events[0].call_id.as_deref(), Some("c1"));
}

/// Placeholder enabled: orphan reasoning before non-assistant → discarded.
#[test]
fn placeholder_discards_orphan_before_non_assistant() {
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "c1", "output": "result"},
        {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "orphan"}]},
        {"type": "message", "role": "user", "content": "hello"}
    ]);
    let (converted, events) =
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, false)
            .expect("placeholder_enabled=true must succeed");
    let messages = converted["messages"].as_array().unwrap();
    // Should have: assistant(tool_calls), tool(output), user("hello")
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason, "orphan_reasoning_discarded");
}

// ─────────────────────────────────────────────────────────────────────────────
// reasoning_content placeholder opt-in
// ─────────────────────────────────────────────────────────────────────────────

/// requires=true + placeholder=false → local 400 with readable error.
#[test]
fn requires_without_placeholder_rejects() {
    // assistant with tool_calls but no reasoning item → reasoning_content missing
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "c1", "output": "result"},
        {"type": "message", "role": "assistant", "content": "next"}
    ]);
    let err = responses_to_chat_with_placeholder(json!({"input": input}), "upstream", false, true)
        .expect_err("requires=true + placeholder=false must reject");
    let msg = err.to_string();
    assert!(
        msg.contains("reasoning_content required but missing"),
        "error message must be readable: {msg}"
    );
}

/// requires=true + placeholder=true → placeholder injected + event recorded.
#[test]
fn requires_with_placeholder_injects() {
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "c1", "output": "result"},
        {"type": "message", "role": "assistant", "content": "next"}
    ]);
    let (converted, events) =
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, true)
            .expect("requires=true + placeholder=true must succeed");
    let messages = converted["messages"].as_array().unwrap();
    // Find the assistant message with tool_calls (first message)
    let assistant_with_calls = messages
        .iter()
        .find(|m| m.get("tool_calls").is_some())
        .unwrap();
    assert_eq!(
        assistant_with_calls["reasoning_content"].as_str().unwrap(),
        "[placeholder: reasoning_content missing for assistant with tool_calls]"
    );
    // Event recorded
    assert!(
        events
            .iter()
            .any(|e| e.reason == "missing_reasoning_content"),
        "expected missing_reasoning_content event, got: {events:?}"
    );
}

/// requires=false → no impact, field omitted as before.
#[test]
fn no_requires_omits_field() {
    let input = json!([
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "c1", "output": "result"},
        {"type": "message", "role": "assistant", "content": "next"}
    ]);
    let (converted, events) =
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", false, false)
            .expect("requires=false must succeed");
    let messages = converted["messages"].as_array().unwrap();
    let assistant_with_calls = messages
        .iter()
        .find(|m| m.get("tool_calls").is_some())
        .unwrap();
    assert!(
        assistant_with_calls.get("reasoning_content").is_none(),
        "requires=false must omit reasoning_content field"
    );
    // No reasoning-related events
    assert!(
        !events
            .iter()
            .any(|e| e.reason == "missing_reasoning_content"),
        "no missing_reasoning_content event when requires=false"
    );
}

/// reasoning non-empty → normal attach, new logic not triggered.
#[test]
fn real_reasoning_not_overridden() {
    let input = json!([
        {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "real thinking"}]},
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "c1", "output": "result"}
    ]);
    let (converted, events) =
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, true)
            .expect("real reasoning + requires=true must succeed");
    let messages = converted["messages"].as_array().unwrap();
    let assistant_with_calls = messages
        .iter()
        .find(|m| m.get("tool_calls").is_some())
        .unwrap();
    assert_eq!(
        assistant_with_calls["reasoning_content"].as_str().unwrap(),
        "real thinking",
        "real reasoning must not be replaced by placeholder"
    );
    // No missing_reasoning_content events
    assert!(
        !events
            .iter()
            .any(|e| e.reason == "missing_reasoning_content"),
        "real reasoning must not trigger missing_reasoning_content event"
    );
}

/// requires=true + placeholder=true + real reasoning → placeholder does not override.
#[test]
fn placeholder_does_not_override_real_reasoning() {
    // This verifies the matrix: when reasoning IS present for the assistant(tool_calls)
    // message, it is used as-is. No trailing assistant message to avoid a second
    // attach_pending_reasoning call on a message without reasoning.
    let input = json!([
        {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "genuine thought"}]},
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "c1", "output": "done"}
    ]);
    let (converted, events) =
        responses_to_chat_with_placeholder(json!({"input": input}), "upstream", true, true)
            .expect("must succeed");
    let messages = converted["messages"].as_array().unwrap();
    let assistant_with_calls = messages
        .iter()
        .find(|m| m.get("tool_calls").is_some())
        .unwrap();
    assert_eq!(
        assistant_with_calls["reasoning_content"].as_str().unwrap(),
        "genuine thought"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.reason == "missing_reasoning_content"),
        "no placeholder event when real reasoning exists"
    );
}
