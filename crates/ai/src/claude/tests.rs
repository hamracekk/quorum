use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use quorum_core::chat::{ChatMessage, ToolSpec};

fn request() -> LlmRequest {
    LlmRequest {
        system: "frozen instructions".into(),
        user: "<news_item>\ntitle: x\n</news_item>".into(),
        schema: json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        max_tokens: 2_048,
    }
}

fn provider(server: &MockServer, fallbacks: bool) -> ClaudeProvider {
    ClaudeProvider::with_base_url(
        &server.uri(),
        "test-key".into(),
        "claude-opus-5-5",
        "low",
        fallbacks,
    )
    .expect("provider")
}

fn message(stop_reason: &str, text: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [
            { "type": "thinking", "thinking": "", "signature": "sig" },
            { "type": "text", "text": text }
        ],
        "stop_reason": stop_reason,
        "usage": {
            "input_tokens": 120, "output_tokens": 40,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 600
        }
    })
}

#[tokio::test]
async fn requests_use_structured_outputs_caching_effort_and_fallbacks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("anthropic-beta", "server-side-fallback-2026-07-01"))
        .and(body_partial_json(json!({
            "model": "claude-opus-5-5",
            "max_tokens": 2048,
            "fallbacks": "default",
            "system": [{ "type": "text", "text": "frozen instructions",
                         "cache_control": { "type": "ephemeral" } }],
            "output_config": { "effort": "low", "format": { "type": "json_schema" } }
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(message("end_turn", r#"{"relevant":false,"events":[]}"#)),
        )
        .mount(&server)
        .await;
    let answer = provider(&server, true)
        .complete(&request())
        .await
        .expect("answer");
    assert_eq!(answer.text, r#"{"relevant":false,"events":[]}"#);
    assert_eq!(
        (answer.input_tokens, answer.output_tokens),
        (720, 40),
        "cached input counts toward the budget"
    );
}

#[tokio::test]
async fn without_fallbacks_neither_header_nor_field_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message("end_turn", "{}")))
        .mount(&server)
        .await;
    provider(&server, false)
        .complete(&request())
        .await
        .expect("answer");
    let sent = &server.received_requests().await.expect("recorded")[0];
    assert!(!sent.headers.contains_key("anthropic-beta"));
    let body: Value = serde_json::from_slice(&sent.body).expect("json body");
    assert!(body.get("fallbacks").is_none());
}

#[tokio::test]
async fn refusals_and_truncation_are_errors() {
    let server = MockServer::start().await;
    let mut refused = message("refusal", "");
    refused["stop_details"] = json!({ "type": "refusal", "category": "cyber" });
    Mock::given(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(refused))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message("max_tokens", "{\"rel")))
        .mount(&server)
        .await;
    let claude = provider(&server, true);
    let refusal = claude.complete(&request()).await.expect_err("refusal");
    assert!(refusal.to_string().contains("cyber"), "{refusal}");
    let truncated = claude.complete(&request()).await.expect_err("truncated");
    assert!(truncated.to_string().contains("max_tokens"), "{truncated}");
}

#[tokio::test]
async fn overload_is_retried_but_bad_requests_are_not() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(529)
                .insert_header("retry-after", "0")
                .set_body_json(json!({
                    "type": "error", "error": { "type": "overloaded_error", "message": "busy" }
                })),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message("end_turn", "{}")))
        .mount(&server)
        .await;
    provider(&server, true)
        .complete(&request())
        .await
        .expect("succeeds after one retry");

    let failing = MockServer::start().await;
    Mock::given(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": "schema not supported" }
        })))
        .mount(&failing)
        .await;
    let err = provider(&failing, true)
        .complete(&request())
        .await
        .expect_err("400");
    assert!(err.to_string().contains("schema not supported"), "{err}");
    assert_eq!(
        failing.received_requests().await.expect("recorded").len(),
        1,
        "a 400 is not retried"
    );
}

#[test]
fn the_key_never_appears_in_debug_output() {
    let claude =
        ClaudeProvider::new("sk-secret-value".into(), "claude-opus-5-5", "low", true).expect("p");
    let debug = format!("{claude:?}");
    assert!(!debug.contains("sk-secret-value"), "{debug}");
    assert!(debug.contains("<redacted>"));
}

fn chat_request() -> ChatRequest {
    ChatRequest {
        system: "agent instructions".into(),
        messages: vec![
            ChatMessage::user_text("check the market"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ChatBlock::Opaque {
                        raw: json!({ "type": "thinking", "thinking": "", "signature": "keep-me" }),
                    },
                    ChatBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "market_overview".into(),
                        input: json!({}),
                    },
                ],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "{\"btc\": 1}".into(),
                    is_error: false,
                }],
            },
        ],
        tools: vec![ToolSpec {
            name: "market_overview".into(),
            description: "prices".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }],
        max_tokens: 4_096,
    }
}

#[tokio::test]
async fn tool_use_steps_send_tools_history_and_reasoning_back_verbatim() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "tools": [{ "name": "market_overview", "description": "prices" }],
            "system": [{ "cache_control": { "type": "ephemeral" } }],
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "check the market" }] },
                { "role": "assistant", "content": [
                    { "type": "thinking", "signature": "keep-me" },
                    { "type": "tool_use", "id": "toolu_1", "name": "market_overview" }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "is_error": false }
                ]}
            ],
            "output_config": { "effort": "low" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [
                { "type": "thinking", "thinking": "", "signature": "next" },
                { "type": "text", "text": "Let me look at signals." },
                { "type": "tool_use", "id": "toolu_2", "name": "signal_study", "input": { "days": 90 } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 50, "output_tokens": 30,
                       "cache_creation_input_tokens": 0, "cache_read_input_tokens": 400 }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let reply = provider(&server, false)
        .chat(&chat_request())
        .await
        .expect("reply");
    assert_eq!(reply.stop, StopReason::ToolUse);
    assert_eq!(reply.text(), "Let me look at signals.");
    assert_eq!(
        reply.tool_uses(),
        [("toolu_2", "signal_study", &json!({ "days": 90 }))]
    );
    assert!(matches!(&reply.content[0], ChatBlock::Opaque { raw } if raw["signature"] == "next"));
    assert_eq!((reply.input_tokens, reply.output_tokens), (450, 30));
}

#[tokio::test]
async fn a_refused_step_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [], "stop_reason": "refusal",
            "stop_details": { "category": "cyber" },
            "usage": { "input_tokens": 1, "output_tokens": 0 }
        })))
        .mount(&server)
        .await;
    let err = provider(&server, false)
        .chat(&chat_request())
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("refusal"), "{err}");
}
