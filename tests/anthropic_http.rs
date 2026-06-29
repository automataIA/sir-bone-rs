//! Wire-level tests for the Anthropic client against a mock HTTP server
//! (`httpmock`). These exercise the SSE streaming parser, tool-call
//! accumulation, and the terminal error paths — the most intricate code in the
//! crate, previously covered only by pure-helper unit tests. No network, no key.
//!
//! Every case here is fast by construction: the error paths chosen (stream
//! `error` event, 429 with an over-cap `retry-after`, non-2xx) all resolve
//! without entering the backoff sleep.

use httpmock::{
    Method::{GET, POST},
    MockServer,
};
use serde_json::json;
use sirbone::types::extract_text;
use sirbone::{AgentState, AnthropicClient, ContentBlock, LlmClient, Message, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// reqwest (built with `rustls-no-provider`) needs a crypto provider before any
/// `Client`. `main` installs it at startup; tests must too. Idempotent: only the
/// first call wins, the rest are ignored.
fn init_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Wrap each JSON event as one `data: …` SSE line. A trailing newline per line
/// matters: the client only parses a line once it sees `\n`.
fn sse(events: &[&str]) -> String {
    events.iter().map(|e| format!("data: {e}\n")).collect()
}

/// Drive `run_turn` against a mock that returns `status` + `body` for
/// `POST /v1/messages`. Returns the turn result and all emitted events.
async fn run_against(
    status: u16,
    retry_after: Option<&str>,
    body: String,
) -> (
    anyhow::Result<sirbone::TurnResult>,
    Vec<sirbone::AgentEvent>,
) {
    init_crypto();
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            let mut t = then
                .status(status)
                .header("content-type", "text/event-stream");
            if let Some(ra) = retry_after {
                t = t.header("retry-after", ra);
            }
            t.body(body);
        })
        .await;

    let client = AnthropicClient::new(&server.base_url(), "test-key", "claude-test");
    let (tx, mut rx) = mpsc::channel(1024);
    let registry = ToolRegistry::new();
    let cancel = CancellationToken::new();
    let result = client
        .run_turn(&[&Message::user("hi")], &registry, &tx, &cancel)
        .await;
    drop(tx);

    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    (result, events)
}

/// 200 + an SSE stream. Convenience over [`run_against`] for the success cases.
async fn stream(events: &[&str]) -> (sirbone::TurnResult, Vec<sirbone::AgentEvent>) {
    let (r, evs) = run_against(200, None, sse(events)).await;
    (r.expect("stream should produce a turn"), evs)
}

#[tokio::test]
async fn text_stream_accumulates() {
    let (turn, events) = stream(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":", world"}}"#,
    ])
    .await;

    assert!(matches!(turn.state, AgentState::Done));
    assert_eq!(
        extract_text(&turn.assistant_message.content),
        "Hello, world"
    );
    let chunks: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            sirbone::AgentEvent::TextChunk(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(chunks, ["Hello", ", world"]);
}

#[tokio::test]
async fn tool_use_assembles_from_partial_json() {
    let (turn, _) = stream(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"/etc/hosts\"}"}}"#,
    ])
    .await;

    match turn.state {
        AgentState::ToolCalling(calls) => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "toolu_1");
            assert_eq!(calls[0].name, "read");
            assert_eq!(
                calls[0].arguments,
                serde_json::json!({"path": "/etc/hosts"})
            );
        }
        other => panic!("expected ToolCalling, got {other:?}"),
    }
    assert!(turn
        .assistant_message
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "read")));
}

#[tokio::test]
async fn multiple_tools_sorted_by_block_index() {
    // Deltas arrive out of index order; the client must order calls by block
    // index, not by arrival.
    let (turn, _) = stream(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"read"}}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"b","name":"grep"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":\"x\"}"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"p\"}"}}"#,
    ])
    .await;

    match turn.state {
        AgentState::ToolCalling(calls) => {
            assert_eq!(
                calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
                ["a", "b"]
            );
            assert_eq!(calls[0].arguments, serde_json::json!({"path": "p"}));
            assert_eq!(calls[1].arguments, serde_json::json!({"pattern": "x"}));
        }
        other => panic!("expected ToolCalling, got {other:?}"),
    }
}

#[tokio::test]
async fn thinking_text_and_tool_combined() {
    let (turn, _) = stream(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reasoning"}}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t","name":"bash"}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}"#,
    ])
    .await;

    assert!(matches!(turn.state, AgentState::ToolCalling(_)));
    let c = &turn.assistant_message.content;
    // Order: thinking, then text, then tool_use.
    assert!(matches!(&c[0], ContentBlock::Thinking { thinking } if thinking == "reasoning"));
    assert!(matches!(&c[1], ContentBlock::Text { text } if text == "answer"));
    assert!(matches!(&c[2], ContentBlock::ToolUse { name, .. } if name == "bash"));
}

#[tokio::test]
async fn stream_error_event_is_terminal() {
    // HTTP 200 then an `error` SSE event with a quota code — must surface as an
    // error, not silently end with empty output.
    let (result, events) = run_against(
        200,
        None,
        sse(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
            r#"{"type":"error","error":{"type":"rate_limit_error","code":"1308","message":"Usage limit reached"}}"#,
        ]),
    )
    .await;

    let err = result.unwrap_err().to_string();
    assert!(err.contains("Usage limit reached"), "got: {err}");
    assert!(events
        .iter()
        .any(|e| matches!(e, sirbone::AgentEvent::Error(_))));
}

#[tokio::test]
async fn over_cap_retry_after_fails_fast() {
    // 429 with a retry-after beyond the cap signals a sustained limit: fail
    // immediately at attempt 1, rather than sleeping or looping to MAX_ATTEMPTS.
    // The "after 1 attempts" assertion pins the attempt counter and the
    // retry-after-cap path (a mutated `retry_after_secs` or `||`→`&&` status
    // check would loop to 5 attempts or skip the cap).
    let (result, _) = run_against(429, Some("9999"), "rate limited".into()).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("429"), "got: {err}");
    assert!(
        err.contains("after 1 attempts"),
        "should fail fast at attempt 1: {err}"
    );
}

#[tokio::test]
async fn over_cap_retry_after_on_5xx_fails_fast() {
    // Same fast-fail on a 5xx with an over-cap retry-after — covers the
    // `(500..600)` arm of the retryable-status check.
    let (result, _) = run_against(503, Some("9999"), "down".into()).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("503"), "got: {err}");
    assert!(err.contains("after 1 attempts"), "got: {err}");
}

#[tokio::test]
async fn non_success_status_surfaces_body() {
    let (result, events) = run_against(401, None, "bad api key".into()).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("401"), "got: {err}");
    assert!(err.contains("bad api key"), "got: {err}");
    assert!(events
        .iter()
        .any(|e| matches!(e, sirbone::AgentEvent::Error(_))));
}

#[tokio::test]
async fn list_models_parses_ids() {
    init_crypto();
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/v1/models");
            then.status(200)
                .json_body(json!({"data": [{"id": "claude-a"}, {"id": "claude-b"}]}));
        })
        .await;
    let client = AnthropicClient::new(&server.base_url(), "k", "m");
    let models = client.list_models().await.unwrap();
    assert_eq!(models, vec!["claude-a", "claude-b"]);
}

#[tokio::test]
async fn list_models_errors_on_non_success() {
    init_crypto();
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/v1/models");
            then.status(500).body("boom");
        })
        .await;
    let client = AnthropicClient::new(&server.base_url(), "k", "m");
    assert!(client.list_models().await.is_err());
}

#[tokio::test]
async fn count_tokens_reads_input_tokens() {
    init_crypto();
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages/count_tokens");
            then.status(200).json_body(json!({"input_tokens": 42}));
        })
        .await;
    let client = AnthropicClient::new(&server.base_url(), "k", "m");
    let n = client
        .count_tokens(&[&Message::user("hi")], &ToolRegistry::new())
        .await
        .unwrap();
    assert_eq!(n, 42);
}

#[tokio::test]
async fn set_model_changes_the_request_model() {
    // After set_model, run_turn must send the NEW model. The mock only matches a
    // body carrying "new-model"; if set_model were a no-op, the request would
    // carry "old", the mock wouldn't match, and `assert_async` would fail.
    init_crypto();
    let server = MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages").body_includes("new-model");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse(&[r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#]));
        })
        .await;
    let client = AnthropicClient::new(&server.base_url(), "k", "old-model");
    client.set_model("new-model".into());
    let (tx, mut rx) = mpsc::channel(64);
    let _ = client
        .run_turn(
            &[&Message::user("hi")],
            &ToolRegistry::new(),
            &tx,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    drop(tx);
    while rx.recv().await.is_some() {}
    mock.assert_async().await;
}
