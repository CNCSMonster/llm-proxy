use serde_json::{Value, json};

use super::*;
// Access internal (pub(super)) helpers not re-exported by mod.rs
use super::anthropic_responses::*;
use super::antigravity::*;
use super::chat_anthropic::*;
use super::shared::*;

#[test]
fn responses_request_converts_to_antigravity_envelope() {
    let input = json!({
        "model": "frontend",
        "instructions": "be concise",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }],
        "max_output_tokens": 128,
        "reasoning": {"effort": "high"},
        "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]
    });

    let out = responses_to_antigravity_request(input, "gemini-upstream", "project-1", &[])
        .expect("convert");

    assert_eq!(out["project"], "project-1");
    assert_eq!(out["model"], "gemini-upstream");
    // model is only at the envelope level, not duplicated inside request
    assert!(out["request"]["model"].is_null());
    assert_eq!(
        out["request"]["systemInstruction"]["parts"][0]["text"],
        "be concise"
    );
    assert_eq!(out["request"]["contents"][0]["role"], "user");
    assert_eq!(out["request"]["contents"][0]["parts"][0]["text"], "hello");
    assert_eq!(out["request"]["generationConfig"]["maxOutputTokens"], 128);
    // "high" maps to thinkingBudget: 16384 (not raw thinkingLevel string)
    assert_eq!(
        out["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        16384
    );
    assert_eq!(
        out["request"]["tools"][0]["functionDeclarations"][0]["name"],
        "lookup"
    );
}

#[test]
fn antigravity_stream_chunk_extracts_text_tool_usage_and_finish() {
    let input = json!({
        "response": {
            "candidates": [{
                "finishReason": "STOP",
                "content": {"parts": [
                    {"text": "hello"},
                    {"functionCall": {"name": "lookup", "args": {"q": "x"}}}
                ]}
            }],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7}
        }
    });

    let chunk = antigravity_stream_chunk(&input);

    assert_eq!(chunk.text, "hello");
    assert_eq!(chunk.function_calls[0].name, "lookup");
    assert_eq!(chunk.function_calls[0].arguments, r#"{"q":"x"}"#);
    assert_eq!(chunk.prompt_tokens, 3);
    assert_eq!(chunk.output_tokens, 4);
    assert_eq!(chunk.total_tokens, 7);
    assert_eq!(chunk.finish_reason.as_deref(), Some("STOP"));
}

#[test]
fn antigravity_thought_parts_do_not_become_output_text() {
    let input = json!({
        "response": {
            "candidates": [{"content": {"parts": [
                {"text": "hidden", "thought": true},
                {"text": "visible"}
            ]}}]
        }
    });

    let chunk = antigravity_stream_chunk(&input);
    assert_eq!(chunk.reasoning_text, "hidden");
    assert_eq!(chunk.text, "visible");

    let out = antigravity_to_responses_response(input.clone(), "frontend");
    assert_eq!(out["output"][0]["type"], "reasoning");
    assert_eq!(out["output"][0]["summary"][0]["text"], "hidden");
    assert_eq!(out["output"][1]["content"][0]["text"], "visible");

    let anthropic = antigravity_to_anthropic_response(input, "frontend");
    assert_eq!(anthropic["content"][0]["type"], "thinking");
    assert_eq!(anthropic["content"][0]["thinking"], "hidden");
    assert_eq!(anthropic["content"][1]["text"], "visible");
}

#[test]
fn antigravity_response_converts_to_responses_response() {
    let input = json!({
        "response": {
            "candidates": [{"content": {"parts": [{"text": "hello"}]}}],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7}
        }
    });

    let out = antigravity_to_responses_response(input, "frontend");

    assert_eq!(out["model"], "frontend");
    assert_eq!(out["output"][0]["content"][0]["text"], "hello");
    assert_eq!(out["usage"]["total_tokens"], 7);
}

#[test]
fn antigravity_response_merges_multiple_elements_and_cpa_usage_metadata() {
    let input = json!([
        {
            "response": {
                "candidates": [{
                    "content": {
                        "parts": [
                            {"text": "hel"},
                            {"functionCall": {"name": "read_file", "args": {"path": "Cargo.toml"}}}
                        ]
                    }
                }]
            }
        },
        {
            "response": {
                "candidates": [{
                    "content": {
                        "parts": [
                            {"text": "lo"}
                        ]
                    }
                }],
                "cpaUsageMetadata": {
                    "promptTokenCount": 2,
                    "candidatesTokenCount": 3
                },
                "modelVersion": "gemini-test-v1"
            }
        }
    ]);

    let out = antigravity_to_responses_response(input, "frontend");

    assert_eq!(out["model"], "frontend");
    let output = out["output"].as_array().expect("output array");
    assert!(
        output
            .iter()
            .any(|item| item["type"] == "message" && item["content"][0]["text"] == "hello")
    );
    assert!(output.iter().any(|item| item["type"] == "function_call"
        && item["name"] == "read_file"
        && item["arguments"] == r#"{"path":"Cargo.toml"}"#));
    assert_eq!(out["usage"]["input_tokens"], 0);
    assert_eq!(out["usage"]["output_tokens"], 0);
    assert_eq!(out["usage"]["total_tokens"], 0);
}

#[test]
fn responses_to_gemini_request_rejects_unsupported_scalar_input() {
    let err = responses_to_gemini_request(
        json!({
            "input": 42
        }),
        "gemini-test",
        &[],
    )
    .expect_err("scalar input should fail");

    assert!(
        err.to_string()
            .contains("unsupported Responses input shape")
    );
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
            "message": {
                "role": "assistant",
                "content": "hello"
            }
        }],
        "usage": {
            "prompt_tokens": 3,
            "completion_tokens": 4,
            "total_tokens": 7
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

    assert_eq!(out["id"], "resp_1");
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

    assert_eq!(out["id"], "chatcmpl_1");
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
        anthropic_block_to_chat_content(&part),
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

    let source = image_url_to_anthropic_source("data:image/png;base64,iVBOR");
    assert_eq!(source["media_type"], "image/png");

    // Plain URLs must not be treated as base64 payloads.
    let source = image_url_to_anthropic_source("https://example.com/x.png");
    assert_eq!(
        source,
        json!({"type": "url", "url": "https://example.com/x.png"})
    );
}

#[test]
fn responses_reasoning_effort_none_omits_thinking_config() {
    let input = json!({
        "model": "frontend",
        "input": "hello",
        "reasoning": {"effort": "none"}
    });
    let out = responses_to_antigravity_request(input, "gemini-3.6-flash-high", "p1", &[])
        .expect("convert");
    // budget=0 → thinkingConfig should be omitted entirely
    assert!(
        out["request"]["generationConfig"]["thinkingConfig"].is_null(),
        "effort=none should omit thinkingConfig, got {:?}",
        out["request"]["generationConfig"]
    );
}

#[test]
fn responses_reasoning_effort_auto_maps_to_negative_budget() {
    let input = json!({
        "model": "frontend",
        "input": "hello",
        "reasoning": {"effort": "auto"}
    });
    let out =
        responses_to_antigravity_request(input, "gemini-2.5-pro", "p1", &[]).expect("convert");
    assert_eq!(
        out["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        -1
    );
    assert_eq!(
        out["request"]["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
}

#[test]
fn anthropic_to_antigravity_preserves_thinking_config() {
    let input = json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "thinking": {"type": "enabled", "budget_tokens": 8192},
        "messages": [{"role": "user", "content": "hello"}]
    });
    let out =
        anthropic_to_antigravity_request(input, "gemini-upstream", "p1", &[]).expect("convert");
    assert_eq!(
        out["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        8192
    );
}

#[test]
fn anthropic_to_antigravity_injects_thinking_parts_in_model_contents() {
    let input = json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "messages": [
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "let me reason...", "signature": "sig123"},
                    {"type": "text", "text": "the answer"}
                ]
            },
            {"role": "user", "content": "thanks"}
        ]
    });
    let out =
        anthropic_to_antigravity_request(input, "gemini-upstream", "p1", &[]).expect("convert");
    let contents = out["request"]["contents"]
        .as_array()
        .expect("contents array");
    // First content should be model role with thinking part prepended
    assert_eq!(contents[0]["role"], "model");
    let parts = contents[0]["parts"].as_array().expect("parts array");
    assert!(
        parts.len() >= 2,
        "expected thinking + text parts, got {parts:?}"
    );
    assert_eq!(parts[0]["thought"], true);
    assert_eq!(parts[0]["text"], "let me reason...");
    assert_eq!(parts[0]["thoughtSignature"], "sig123");
    assert_eq!(parts[1]["text"], "the answer");
}

#[test]
fn responses_to_anthropic_converts_reasoning_effort_to_thinking() {
    let input = json!({
        "model": "frontend",
        "input": "hello",
        "reasoning": {"effort": "high"}
    });
    let out = responses_to_anthropic(input, "claude-sonnet-4-20250514").expect("convert");
    assert_eq!(out["thinking"]["type"], "enabled");
    assert_eq!(out["thinking"]["budget_tokens"], 16384);
}

#[test]
fn responses_to_anthropic_reasoning_none_omits_thinking() {
    let input = json!({
        "model": "frontend",
        "input": "hello",
        "reasoning": {"effort": "none"}
    });
    let out = responses_to_anthropic(input, "claude-sonnet-4-20250514").expect("convert");
    assert!(
        out.get("thinking").is_none(),
        "effort=none should not set thinking"
    );
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
fn empty_text_parts_are_filtered_in_gemini_contents() {
    let input = json!({
        "model": "frontend",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": ""}]},
            {"type": "function_call", "call_id": "fc_1", "name": "exec_command", "arguments": "{\"cmd\":\"ls\"}"},
            {"type": "function_call_output", "call_id": "fc_1", "output": "file1.txt\n"}
        ]
    });
    let out = responses_to_antigravity_request(input, "claude-opus-4-6-thinking", "p1", &[])
        .expect("convert");
    let contents = out["request"]["contents"]
        .as_array()
        .expect("contents array");
    // The empty assistant text part must be dropped entirely
    for c in contents {
        let role = c.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "model" {
            let parts = c
                .get("parts")
                .and_then(Value::as_array)
                .expect("model parts");
            assert!(
                parts.iter().any(|p| p.get("functionCall").is_some()),
                "model content should contain functionCall, got {parts:?}"
            );
            // No part may have empty text
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    assert!(!t.is_empty(), "empty text part leaked: {parts:?}");
                }
            }
        }
    }
    // functionCall must be present after empty text filtering
    let has_function_call = contents.iter().any(|c| {
        c.get("parts")
            .and_then(Value::as_array)
            .map(|parts| parts.iter().any(|p| p.get("functionCall").is_some()))
            .unwrap_or(false)
    });
    assert!(has_function_call, "expected functionCall part");
}

#[test]
fn function_call_output_without_name_falls_back_to_call_id() {
    let input = json!({
        "model": "frontend",
        "input": [
            {"type": "function_call", "call_id": "fc_1", "name": "exec_command", "arguments": "{\"cmd\":\"ls\"}"},
            {"type": "function_call_output", "call_id": "fc_1", "output": "file1.txt\n"}
        ]
    });
    let out = responses_to_antigravity_request(input, "gemini-3.6-flash-high", "p1", &[])
        .expect("convert");
    let contents = out["request"]["contents"]
        .as_array()
        .expect("contents array");
    // Find the functionResponse part — name must be non-empty (call_id fallback)
    let mut found = false;
    for c in contents {
        if let Some(parts) = c.get("parts").and_then(Value::as_array) {
            for p in parts {
                if let Some(fr) = p.get("functionResponse") {
                    let name = fr.get("name").and_then(Value::as_str).unwrap_or("");
                    assert!(
                        !name.is_empty(),
                        "function_response.name must not be empty, got {fr:?}"
                    );
                    // Name should be the real function name resolved from the paired function_call
                    assert_eq!(name, "exec_command", "name should resolve to function name");
                    found = true;
                }
            }
        }
    }
    assert!(found, "expected functionResponse part");
}

#[test]
fn multiturn_merges_adjacent_same_role_contents() {
    // Codex 三轮工具调用历史：转换后不得出现连续相同 role 的 content
    let input = json!({
        "model": "gemini-3.6-flash-high-lp",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Q1: 列出文件"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "我来查看"}]},
            {"type": "function_call", "call_id": "fc_1", "name": "ls", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "fc_1", "output": "file1.txt\nfile2.txt"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Q2: 分析 file1"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "正在分析"}]},
            {"type": "function_call", "call_id": "fc_2", "name": "read_file", "arguments": "{\"path\":\"file1.txt\"}"},
            {"type": "function_call_output", "call_id": "fc_2", "output": "内容为 ABC"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Q3: 总结"}]}
        ]
    });
    let out = responses_to_antigravity_request(input, "gemini-3.6-flash-high", "p1", &[])
        .expect("convert");
    let contents = out["request"]["contents"]
        .as_array()
        .expect("contents array");
    // 断言：相邻 content 的 role 必须交替
    let mut prev_role: Option<&str> = None;
    for c in contents {
        let role = c.get("role").and_then(Value::as_str).expect("role");
        if let Some(prev) = prev_role {
            assert_ne!(
                role, prev,
                "consecutive contents share role {role:?}; must alternate: {contents:#?}"
            );
        }
        prev_role = Some(role);
    }
    // 且所有消息内容都在（无丢失）：9 个 input item 折叠为 5 个交替 content
    assert_eq!(
        contents.len(),
        5,
        "expected 5 alternating contents, got {contents:#?}"
    );
    // model 文本与 functionCall 合并进同一个 content
    let merged_model = &contents[1];
    assert_eq!(merged_model["role"], "model");
    let model_parts = merged_model["parts"].as_array().expect("model parts");
    assert_eq!(
        model_parts.len(),
        2,
        "assistant text + functionCall merged: {model_parts:?}"
    );
    assert!(model_parts[0].get("text").is_some());
    assert!(model_parts[1].get("functionCall").is_some());
}

#[test]
fn function_call_signature_key_prefers_id_then_name_args() {
    let with_id = json!({"functionCall": {"id": "toolu_1", "name": "ls", "args": {}}});
    let key_id = function_call_signature_key(&with_id["functionCall"], "ls", "{}");
    assert_eq!(key_id, "id:toolu_1");

    let no_id = json!({"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}});
    let key_fn =
        function_call_signature_key(&no_id["functionCall"], "read_file", r#"{"path":"a.txt"}"#);
    assert_eq!(key_fn, r#"fn:read_file:{"path":"a.txt"}"#);

    // 相同 args 不同键序应产生相同 key（serde_json 对象键序稳定）
    let key_reordered =
        function_call_signature_key(&no_id["functionCall"], "read_file", r#"{"path":"a.txt"}"#);
    assert_eq!(key_fn, key_reordered);
}

#[test]
fn antigravity_anthropic_family_models_need_tool_call_ids() {
    let anthropic = ["claude-*".to_string(), "gpt-oss-*".to_string()];
    // claude 与 gpt-oss 都走 antigravity 的 Anthropic 转换路径，需要 tool_use id
    assert!(antigravity_needs_tool_call_ids(
        "claude-opus-4-6-thinking",
        &anthropic
    ));
    assert!(antigravity_needs_tool_call_ids(
        "claude-sonnet-4-6",
        &anthropic
    ));
    assert!(antigravity_needs_tool_call_ids(
        "gpt-oss-120b-medium",
        &anthropic
    ));
    // Gemini-native 不需要 id
    assert!(!antigravity_needs_tool_call_ids(
        "gemini-3.6-flash-high",
        &anthropic
    ));
    assert!(!antigravity_needs_tool_call_ids(
        "gemini-pro-agent",
        &anthropic
    ));
    // 空声明 = 全部原生语义
    let empty: Vec<String> = Vec::new();
    assert!(!antigravity_needs_tool_call_ids(
        "claude-opus-4-6-thinking",
        &empty
    ));
}

#[test]
fn antigravity_anthropic_family_empty_by_default() {
    // EndpointConfig.anthropic_family_models 缺省为空 = 全部 Gemini 原生
    let endpoint = crate::config::EndpointConfig::default();
    assert!(endpoint.anthropic_family_models.is_empty());
}

#[test]
fn antigravity_anthropic_family_glob_syntax() {
    // `?` 单字符通配
    let patterns = ["claude-sonnet-?-*".to_string()];
    assert!(antigravity_needs_tool_call_ids(
        "claude-sonnet-4-6",
        &patterns
    ));
    assert!(!antigravity_needs_tool_call_ids(
        "claude-sonnet-45-6",
        &patterns
    ));
    // 字符类
    let patterns = ["gpt-oss-[0-9]*".to_string()];
    assert!(antigravity_needs_tool_call_ids("gpt-oss-120b", &patterns));
    assert!(!antigravity_needs_tool_call_ids("gpt-oss-b", &patterns));
    // `{a,b}` 交替
    let patterns = ["{claude,gpt-oss}-*".to_string()];
    assert!(antigravity_needs_tool_call_ids(
        "claude-opus-4-6",
        &patterns
    ));
    assert!(antigravity_needs_tool_call_ids("gpt-oss-120b", &patterns));
    assert!(!antigravity_needs_tool_call_ids(
        "gemini-3.6-flash",
        &patterns
    ));
}

#[test]
fn stream_chunk_binds_signature_to_function_call_name() {
    let input = json!({
        "response": {
            "candidates": [{
                "content": {"parts": [
                    {"functionCall": {"name": "ls", "args": {}}, "thoughtSignature": "sig_ls"},
                    {"text": "hello"}
                ]}
            }]
        }
    });
    let chunk = antigravity_stream_chunk(&input);
    assert_eq!(chunk.function_calls.len(), 1);
    assert_eq!(chunk.signature_pairs.len(), 1);
    // 流式 chunk 只记录 (name, sig)；key 延迟到消费端用完整 args 计算
    assert_eq!(chunk.signature_pairs[0].0, "ls");
    assert_eq!(chunk.signature_pairs[0].1, "sig_ls");
    // 无签名的 functionCall 不产生 pair（避免错配）
    let unsigned = json!({
        "response": {"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "ls", "args": {}}}
        ]}}]}
    });
    let chunk2 = antigravity_stream_chunk(&unsigned);
    assert!(chunk2.signature_pairs.is_empty());
}

#[test]
fn signature_key_from_name_args_is_stable() {
    // 相同参数不同键序应产生相同 key（serde_json 对象键序稳定）
    let a = signature_key_from_name_args("read_file", r#"{"path":"a.txt","mode":"r"}"#);
    let b = signature_key_from_name_args("read_file", r#"{"mode":"r","path":"a.txt"}"#);
    assert_eq!(a, b);
    assert_eq!(a, r#"fn:read_file:{"mode":"r","path":"a.txt"}"#);
}

#[test]
fn stream_chunk_collects_thought_part_signatures() {
    // thinking part 的 thoughtSignature 独立于 functionCall 签名收集
    let input = json!({
        "response": {"candidates": [{"content": {"parts": [
            {"text": "thinking...", "thought": true, "thoughtSignature": "think_sig_1"},
            {"text": "more thinking", "thought": true},
            {"functionCall": {"name": "ls", "args": {}}, "thoughtSignature": "fc_sig"}
        ]}}]}
    });
    let chunk = antigravity_stream_chunk(&input);
    assert_eq!(chunk.reasoning_text, "thinking...more thinking");
    // 思考块的签名（只收集带签名的 thought part）
    assert_eq!(chunk.thought_signatures, vec!["think_sig_1".to_string()]);
    // functionCall 签名仍走 signature_pairs
    assert_eq!(chunk.signature_pairs.len(), 1);
    assert_eq!(chunk.signature_pairs[0].1, "fc_sig");
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
            "message": {
                "content": "Hi there!",
                "tool_calls": [{
                    "id": "call_1",
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
    assert_eq!(out["thinking"]["type"], "enabled");
    assert_eq!(out["thinking"]["budget_tokens"], 4096);

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
    assert_eq!(out_msg["id"], "resp_anth_out");
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

    assert_eq!(converted["id"], "msg_llm_proxy");
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
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["call_id"], "tool-1");
    assert_eq!(input[0]["arguments"], "{\"x\":1}");
    assert_eq!(input[1]["type"], "message");
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(
        input[1]["content"][0],
        json!({"type": "output_text", "text": "answer"})
    );
    assert_eq!(input[1]["content"][1]["type"], "output_text");
    assert!(
        input[1]["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("\"type\":\"thinking\"")
    );
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["output"], "42");
    assert_eq!(input[3]["content"][0]["type"], "input_image");
    assert_eq!(
        input[3]["content"][1],
        json!({
            "type": "input_file",
            "filename": "spec",
            "file_url": "https://example.com/spec.pdf"
        })
    );
    assert_eq!(
        input[3]["content"][2],
        json!({"type": "input_text", "text": "\"literal\""})
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
    assert_eq!(
        converted["thinking"],
        json!({"type": "enabled", "budget_tokens": 8192})
    );
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

    assert_eq!(converted["id"], "chat-1");
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
fn responses_to_chat_handles_pending_shell_calls_and_skips_empty_assistant() {
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
        file_data_to_anthropic_source("rawbase64"),
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
    assert_eq!(
        responses_usage(Some(&json!({"completion_tokens": 6}))),
        json!({
            "input_tokens": 0,
            "output_tokens": 6,
            "total_tokens": 6
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
