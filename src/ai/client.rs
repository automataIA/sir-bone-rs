use std::collections::HashMap;

use crate::{
    agent::{LlmClient, TurnResult},
    tools::ToolRegistry,
    types::{extract_text, AgentEvent, AgentState, ContentBlock, EventTx, Message, Role, ToolCall},
};
use anyhow::{anyhow, Result};
use async_openai::{
    config::OpenAIConfig,
    error::OpenAIError,
    types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
        ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent, ChatCompletionStreamOptions, ChatCompletionTool,
        ChatCompletionTools, CreateChatCompletionRequest, FunctionCall, FunctionObject,
    },
    Client,
};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::select;
use tokio_util::sync::CancellationToken;

use super::{backoff_secs, json_u64, MAX_ATTEMPTS};

pub struct OpenAiClient {
    inner: Client<OpenAIConfig>,
    /// Current model. Locked so it can be switched at runtime through a shared
    /// `Arc<dyn LlmClient>` (the `/model` picker).
    model: std::sync::RwLock<String>,
    /// Kept for raw `/models` queries — async-openai's typed `Model` drops the
    /// non-standard fields (Groq `context_window`, OpenRouter `context_length`).
    base_url: String,
    api_key: String,
    /// Context window of the active model, lazily fetched from `/models` and
    /// cached. 0 = not fetched yet (reset by `set_model`).
    context_window: std::sync::atomic::AtomicU32,
}

impl OpenAiClient {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        let config = OpenAIConfig::new()
            .with_api_base(base_url)
            .with_api_key(api_key);
        Self {
            inner: Client::with_config(config),
            model: std::sync::RwLock::new(model.to_string()),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            context_window: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

fn to_oai_message(msg: &Message) -> Result<ChatCompletionRequestMessage> {
    match msg.role {
        Role::System => {
            let text = extract_text(&msg.content);
            Ok(ChatCompletionRequestMessage::System(
                ChatCompletionRequestSystemMessage {
                    content: ChatCompletionRequestSystemMessageContent::Text(text),
                    name: None,
                },
            ))
        }
        Role::User => {
            let text = extract_text(&msg.content);
            Ok(ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(text),
                    name: None,
                },
            ))
        }
        Role::Assistant => {
            let text: String = msg
                .content
                .iter()
                .filter_map(|c| {
                    if let ContentBlock::Text { text } = c {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect();

            let tool_calls: Vec<ChatCompletionMessageToolCalls> = msg
                .content
                .iter()
                .filter_map(|c| {
                    if let ContentBlock::ToolUse { id, name, input } = c {
                        Some(ChatCompletionMessageToolCalls::Function(
                            ChatCompletionMessageToolCall {
                                id: id.clone(),
                                function: FunctionCall {
                                    name: name.clone(),
                                    arguments: input.to_string(),
                                },
                            },
                        ))
                    } else {
                        None
                    }
                })
                .collect();

            let content = if text.is_empty() {
                None
            } else {
                Some(ChatCompletionRequestAssistantMessageContent::Text(text))
            };
            let tool_calls_opt = if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            };

            Ok(ChatCompletionRequestMessage::Assistant(
                ChatCompletionRequestAssistantMessage {
                    content,
                    tool_calls: tool_calls_opt,
                    name: None,
                    audio: None,
                    refusal: None,
                    #[allow(deprecated)]
                    function_call: None,
                },
            ))
        }
        Role::Tool => {
            let (tool_use_id, content) = msg
                .content
                .iter()
                .find_map(|c| {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = c
                    {
                        Some((tool_use_id.clone(), content.clone()))
                    } else {
                        None
                    }
                })
                .ok_or_else(|| anyhow!("Tool message has no ToolResult block"))?;

            Ok(ChatCompletionRequestMessage::Tool(
                ChatCompletionRequestToolMessage {
                    content: ChatCompletionRequestToolMessageContent::Text(content),
                    tool_call_id: tool_use_id,
                },
            ))
        }
    }
}

fn build_tools(registry: &ToolRegistry) -> Vec<ChatCompletionTools> {
    registry
        .iter()
        .map(|t| {
            ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name: t.name().to_string(),
                    description: Some(t.description().to_string()),
                    parameters: Some(t.schema()),
                    strict: None,
                },
            })
        })
        .collect()
}

#[derive(Default)]
struct AccumToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn run_turn(
        &self,
        messages: &[&Message],
        registry: &ToolRegistry,
        events: &EventTx,
        cancel: &CancellationToken,
    ) -> Result<TurnResult> {
        let oai_messages: Vec<ChatCompletionRequestMessage> = messages
            .iter()
            .copied()
            .map(to_oai_message)
            .collect::<Result<Vec<_>>>()?;

        let tools = build_tools(registry);

        let mut req = CreateChatCompletionRequest {
            // Clone out of the lock; no guard held across `.await`.
            model: crate::types::read_or_recover(&self.model).clone(),
            messages: oai_messages,
            // Final chunk carries usage (prompt size + cached share) — OpenAI-style
            // caching is automatic server-side, this is the only visibility into it.
            stream_options: Some(ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            }),
            ..Default::default()
        };
        if !tools.is_empty() {
            req.tools = Some(tools);
        }

        let mut stream = {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match self.inner.chat().create_stream(req.clone()).await {
                    Ok(s) => break s,
                    Err(e) => {
                        if !is_retryable(&e) || attempt >= MAX_ATTEMPTS {
                            events
                                .send(AgentEvent::Error(crate::ai::redact_secrets(&e.to_string())))
                                .await
                                .ok();
                            return Err(e.into());
                        }
                        let secs = backoff_secs(attempt);
                        tracing::warn!(attempt, secs, error = %crate::ai::redact_secrets(&e.to_string()), "retrying OpenAI API call");
                        // Cancellable backoff: Ctrl-C during the wait ends cleanly
                        // rather than after the full sleep.
                        select! {
                            _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
                            _ = cancel.cancelled() => {
                                events.send(AgentEvent::Cancelled).await.ok();
                                return Ok(TurnResult {
                                    assistant_message: Message::assistant(String::new()),
                                    state: AgentState::Done,
                                    usage: crate::types::TokenUsage::default(),
                                });
                            }
                        }
                    }
                }
            }
        };

        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_map: HashMap<u32, AccumToolCall> = HashMap::new();
        let mut usage = crate::types::TokenUsage::default();

        loop {
            select! {
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(resp)) => {
                            if let Some(u) = &resp.usage {
                                let cached = u.prompt_tokens_details.as_ref()
                                    .and_then(|d| d.cached_tokens)
                                    .unwrap_or(0);
                                if cached > 0 {
                                    tracing::info!(cached, "prompt cache stats");
                                }
                                usage.input = u.prompt_tokens;
                                usage.output = u.completion_tokens;
                                events.send(AgentEvent::ContextUsage {
                                    used_tokens: u.prompt_tokens,
                                    context_window: self.context_window().await.unwrap_or(128_000),
                                    cached_tokens: cached,
                                }).await.ok();
                            }
                            for choice in resp.choices {
                                let delta = choice.delta;
                                if let Some(text) = delta.content {
                                    text_parts.push(text.clone());
                                    events.send(AgentEvent::TextChunk(text)).await.ok();
                                }
                                if let Some(tcs) = delta.tool_calls {
                                    for tc in tcs {
                                        let entry = tool_map.entry(tc.index).or_default();
                                        if let Some(id) = tc.id {
                                            entry.id = id;
                                        }
                                        if let Some(f) = tc.function {
                                            if let Some(name) = f.name {
                                                entry.name = name;
                                            }
                                            if let Some(args) = f.arguments {
                                                entry.arguments.push_str(&args);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            events.send(AgentEvent::Error(e.to_string())).await.ok();
                            return Err(e.into());
                        }
                        None => break,
                    }
                }
                _ = cancel.cancelled() => {
                    events.send(AgentEvent::Cancelled).await.ok();
                    let text = text_parts.join("");
                    return Ok(TurnResult {
                        assistant_message: Message::assistant(text),
                        state: AgentState::Done,
                        usage,
                    });
                }
            }
        }

        events.send(AgentEvent::TurnEnd).await.ok();

        let text = text_parts.join("");

        if tool_map.is_empty() {
            return Ok(TurnResult {
                assistant_message: Message::assistant(text),
                state: AgentState::Done,
                usage,
            });
        }

        let mut sorted: Vec<_> = tool_map.into_iter().collect();
        sorted.sort_unstable_by_key(|(idx, _)| *idx);
        let tool_calls: Vec<ToolCall> = sorted
            .into_iter()
            .map(|(_, acc)| {
                let arguments =
                    serde_json::from_str(&acc.arguments).unwrap_or(serde_json::json!({}));
                ToolCall {
                    id: acc.id,
                    name: acc.name,
                    arguments,
                }
            })
            .collect();

        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text: text.clone() });
        }
        for tc in &tool_calls {
            content.push(ContentBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.arguments.clone(),
            });
        }

        Ok(TurnResult {
            assistant_message: Message {
                role: Role::Assistant,
                content,
            },
            state: AgentState::ToolCalling(tool_calls),
            usage,
        })
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let resp = self.inner.models().list().await?;
        Ok(resp.data.into_iter().map(|m| m.id).collect())
    }

    fn set_model(&self, model: String) {
        *crate::types::write_or_recover(&self.model) = model;
        // Window is per-model — refetch on next use.
        self.context_window
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Best-effort: standard OpenAI `/models` has no window field, but Groq
    /// (`context_window`) and OpenRouter (`context_length`) extend the schema.
    async fn context_window(&self) -> Option<u32> {
        use std::sync::atomic::Ordering::Relaxed;
        if let Some(n) = super::env_context_window() {
            return Some(n);
        }
        let cached = self.context_window.load(Relaxed);
        if cached != 0 {
            return Some(cached);
        }
        let model = crate::types::read_or_recover(&self.model).clone();
        let url = format!("{}/models", self.base_url);
        let resp = reqwest::Client::new()
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        let entry = v["data"]
            .as_array()?
            .iter()
            .find(|m| m["id"].as_str() == Some(&model))?;
        let n = json_u64(&entry["context_window"]).or_else(|| json_u64(&entry["context_length"]))?
            as u32;
        self.context_window.store(n, Relaxed);
        Some(n)
    }
}

/// Whether an OpenAI error is worth retrying. Retryable: transient network
/// failures (connect/timeout), 429 rate limits, and 5xx server errors. Terminal:
/// auth, bad request, deserialize, and other client-side errors — short backoff
/// can't help.
fn is_retryable(e: &OpenAIError) -> bool {
    match e {
        OpenAIError::Reqwest(re) => re.is_connect() || re.is_timeout(),
        OpenAIError::ApiError(resp) => {
            let s = resp.status_code.as_u16();
            s == 429 || (500..600).contains(&s)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ReadTool;
    use async_openai::error::{ApiError, ApiErrorResponse};
    use httpmock::{
        Method::{GET, POST},
        MockServer,
    };
    use serde_json::json;
    use tokio::sync::mpsc;

    /// async-openai builds a reqwest client (`rustls-no-provider`) -> needs a
    /// crypto provider installed first. Idempotent.
    fn init_crypto() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    /// Wrap chunks as an OpenAI-style SSE stream, terminated by `[DONE]`.
    fn oai_sse(chunks: &[&str]) -> String {
        let mut s: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        s.push_str("data: [DONE]\n\n");
        s
    }

    fn api_error(code: u16) -> OpenAIError {
        OpenAIError::ApiError(ApiErrorResponse {
            status_code: reqwest::StatusCode::from_u16(code).unwrap(),
            api_error: ApiError {
                message: "x".into(),
                r#type: None,
                param: None,
                code: None,
            },
        })
    }

    #[test]
    fn rate_limit_and_server_errors_retry() {
        assert!(is_retryable(&api_error(429)));
        assert!(is_retryable(&api_error(500)));
        assert!(is_retryable(&api_error(503)));
    }

    #[test]
    fn client_errors_are_terminal() {
        assert!(!is_retryable(&api_error(400)));
        assert!(!is_retryable(&api_error(401)));
        assert!(!is_retryable(&api_error(404)));
    }

    #[test]
    fn user_message_maps_to_text() {
        let m = to_oai_message(&Message::user("hello")).unwrap();
        match m {
            ChatCompletionRequestMessage::User(u) => assert!(matches!(
                u.content,
                ChatCompletionRequestUserMessageContent::Text(t) if t == "hello"
            )),
            _ => panic!("expected User message"),
        }
    }

    #[test]
    fn assistant_tool_use_maps_to_tool_calls() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "x"}),
            }],
        };
        match to_oai_message(&msg).unwrap() {
            ChatCompletionRequestMessage::Assistant(a) => {
                let calls = a.tool_calls.expect("tool_calls present");
                assert_eq!(calls.len(), 1);
                assert!(a.content.is_none(), "no text → content None");
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn tool_message_without_result_errors() {
        let msg = Message {
            role: Role::Tool,
            content: vec![ContentBlock::Text { text: "x".into() }],
        };
        assert!(to_oai_message(&msg).is_err());
    }

    #[test]
    fn build_tools_emits_one_function_per_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(ReadTool::default());
        let tools = build_tools(&reg);
        assert_eq!(tools.len(), 1);
        match &tools[0] {
            ChatCompletionTools::Function(f) => {
                assert_eq!(f.function.name, "read");
                assert!(f.function.parameters.is_some());
            }
            other => panic!("expected a function tool, got {other:?}"),
        }
    }

    #[test]
    fn set_model_updates_the_active_model() {
        init_crypto();
        let c = OpenAiClient::new("http://x", "k", "old");
        c.set_model("new".into());
        assert_eq!(*c.model.read().unwrap(), "new");
    }

    #[tokio::test]
    async fn run_turn_sends_messages_and_tools() {
        // The request body must carry the conversation AND the tool schema; if
        // either field were dropped, the mock wouldn't match and assert fails.
        init_crypto();
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/chat/completions")
                    .body_includes("hello") // conversation
                    .body_includes("read") // tool schema
                    .body_includes("gpt-distinct"); // model
                then.status(200).header("content-type", "text/event-stream").body(oai_sse(&[
                    r#"{"id":"x","choices":[{"index":0,"delta":{"content":"ok"}}],"created":0,"model":"m","object":"chat.completion.chunk"}"#,
                ]));
            })
            .await;
        let client = OpenAiClient::new(&server.base_url(), "k", "gpt-distinct");
        let mut reg = ToolRegistry::new();
        reg.register(ReadTool::default());
        let (tx, mut rx) = mpsc::channel(64);
        client
            .run_turn(
                &[&Message::user("hello")],
                &reg,
                &tx,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        drop(tx);
        while rx.recv().await.is_some() {}
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn run_turn_parses_text_and_tool_calls() {
        init_crypto();
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/chat/completions");
                then.status(200).header("content-type", "text/event-stream").body(oai_sse(&[
                    r#"{"id":"x","choices":[{"index":0,"delta":{"content":"answer"}}],"created":0,"model":"m","object":"chat.completion.chunk"}"#,
                    r#"{"id":"x","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"path\":\"p\"}"}}]}}],"created":0,"model":"m","object":"chat.completion.chunk"}"#,
                ]));
            })
            .await;
        let client = OpenAiClient::new(&server.base_url(), "k", "m");
        let (tx, mut rx) = mpsc::channel(64);
        let turn = client
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
        // Non-empty assistant text is kept (kills the `!text.is_empty()` guard).
        assert!(turn
            .assistant_message
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "answer")));
        match turn.state {
            AgentState::ToolCalling(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "read");
                assert_eq!(calls[0].arguments, json!({"path": "p"}));
            }
            other => panic!("expected ToolCalling, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_turn_does_not_retry_a_client_error() {
        // A 400 is terminal: exactly one request, no retry loop.
        init_crypto();
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/chat/completions");
                then.status(400)
                    .json_body(json!({"error": {"message": "bad request"}}));
            })
            .await;
        let client = OpenAiClient::new(&server.base_url(), "k", "m");
        let (tx, mut rx) = mpsc::channel(64);
        let r = client
            .run_turn(
                &[&Message::user("hi")],
                &ToolRegistry::new(),
                &tx,
                &CancellationToken::new(),
            )
            .await;
        drop(tx);
        while rx.recv().await.is_some() {}
        assert!(r.is_err());
        mock.assert_calls_async(1).await;
    }

    #[tokio::test]
    async fn list_models_parses_ids() {
        init_crypto();
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/models");
                then.status(200).json_body(json!({
                    "object": "list",
                    "data": [
                        {"id": "gpt-x", "object": "model", "created": 0, "owned_by": "o"},
                        {"id": "gpt-y", "object": "model", "created": 0, "owned_by": "o"}
                    ]
                }));
            })
            .await;
        let client = OpenAiClient::new(&server.base_url(), "k", "m");
        let models = client.list_models().await.unwrap();
        assert_eq!(models, vec!["gpt-x", "gpt-y"]);
    }

    #[tokio::test]
    async fn context_window_reads_groq_and_openrouter_fields() {
        init_crypto();
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/models");
                then.status(200).json_body(json!({
                    "object": "list",
                    "data": [
                        {"id": "groq-m", "object": "model", "created": 0, "owned_by": "o",
                         "context_window": 131_072},
                        {"id": "router-m", "object": "model", "created": 0, "owned_by": "o",
                         "context_length": 200_000},
                        {"id": "plain-m", "object": "model", "created": 0, "owned_by": "o"}
                    ]
                }));
            })
            .await;
        let groq = OpenAiClient::new(&server.base_url(), "k", "groq-m");
        assert_eq!(groq.context_window().await, Some(131_072));
        let router = OpenAiClient::new(&server.base_url(), "k", "router-m");
        assert_eq!(router.context_window().await, Some(200_000));
        // Standard OpenAI schema has no window field — unknown, caller falls back.
        let plain = OpenAiClient::new(&server.base_url(), "k", "plain-m");
        assert_eq!(plain.context_window().await, None);
    }
}
