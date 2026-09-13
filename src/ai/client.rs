use std::collections::HashMap;

use crate::{
    agent::{LlmClient, TurnResult},
    tools::ToolRegistry,
    types::{extract_text, AgentEvent, AgentState, ContentBlock, EventTx, Message, Role, ToolCall},
};
use anyhow::{anyhow, Result};
use async_openai::{
    config::OpenAIConfig,
    error::{ApiError, ApiErrorResponse, OpenAIError, StreamError, WrappedError},
    types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
        ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartImage,
        ChatCompletionRequestMessageContentPartText, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
        ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
        ChatCompletionStreamOptions, ChatCompletionTool, ChatCompletionTools,
        CreateChatCompletionRequest, FunctionCall, FunctionObject, ImageDetail, ImageUrl,
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
    /// Thinking-budget dial (None = off). On GLM it maps to a z.ai
    /// reasoning-effort level ([`super::glm_effort_label`]); stored whatever
    /// the model, but only ever sent when [`super::is_glm`] says the endpoint
    /// understands it — other providers reject the field outright.
    thinking_budget: std::sync::atomic::AtomicU32,
    /// Context window of the active model, lazily fetched from `/models` and
    /// cached. 0 = not fetched yet (reset by `set_model`).
    context_window: std::sync::atomic::AtomicU32,
    /// Reused for raw `/models` queries — building a fresh `reqwest::Client`
    /// per call would redo connection-pool + TLS setup every time.
    http: reqwest::Client,
    /// Rules that abort generation mid-stream. Empty by default, so the hot
    /// path is untouched unless the user configured `stream_rules`.
    stream_rules: std::sync::RwLock<std::sync::Arc<crate::stream_rules::StreamRules>>,
    /// Sampling temperature. None = field omitted, provider default applies
    /// (1.0 on z.ai GLM-5.x). Sent verbatim; the provider owns the valid range.
    temperature: Option<f32>,
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
            thinking_budget: std::sync::atomic::AtomicU32::new(0),
            context_window: std::sync::atomic::AtomicU32::new(0),
            http: crate::ai::http_client(),
            stream_rules: std::sync::RwLock::new(std::sync::Arc::default()),
            temperature: None,
        }
    }

    /// Fix the sampling temperature for the life of the client (bench runs pin
    /// it so both arms of an A/B sample alike).
    pub fn with_temperature(mut self, temperature: Option<f32>) -> Self {
        self.temperature = temperature;
        self
    }
}

/// The reasoning-effort level to send, if any. Only GLM endpoints get one:
/// they default to a heavy effort when the field is absent (measured 390-850
/// reasoning tokens on z.ai), so the dial's "off" must still send `minimal`
/// to matter — while non-GLM providers may reject the field with a 400.
fn glm_reasoning_effort(
    base_url: &str,
    model: &str,
    budget: Option<u32>,
) -> Option<async_openai::types::chat::ReasoningEffort> {
    if !super::is_glm(base_url, model) {
        return None;
    }
    use async_openai::types::chat::ReasoningEffort;
    Some(match budget {
        None => ReasoningEffort::Minimal,
        Some(b) if b <= 8000 => ReasoningEffort::Low,
        Some(b) if b <= 16000 => ReasoningEffort::Medium,
        Some(_) => ReasoningEffort::Xhigh,
    })
}

/// One user block as an OpenAI content part. Images travel as a base64 data URI —
/// the wire format llama.cpp, OpenAI and OpenRouter all read. `detail` is spelled
/// out rather than left `None`: the field has no `skip_serializing_if`, and a
/// literal `"detail": null` is outside the OpenAI schema even where it is tolerated.
fn to_oai_content_part(
    block: &ContentBlock,
) -> Option<ChatCompletionRequestUserMessageContentPart> {
    match block {
        ContentBlock::Text { text } => Some(ChatCompletionRequestUserMessageContentPart::Text(
            ChatCompletionRequestMessageContentPartText { text: text.clone() },
        )),
        ContentBlock::Image { media_type, data } => {
            Some(ChatCompletionRequestUserMessageContentPart::ImageUrl(
                ChatCompletionRequestMessageContentPartImage {
                    image_url: ImageUrl {
                        url: format!("data:{media_type};base64,{data}"),
                        detail: Some(ImageDetail::Auto),
                    },
                },
            ))
        }
        _ => None,
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
            // Text-only stays a plain string: "OpenAI-compatible" endpoints vary in
            // how well they parse the content-part array, and most turns carry no
            // image. Only a turn with an attachment pays for the richer form.
            let content = if msg
                .content
                .iter()
                .any(|c| matches!(c, ContentBlock::Image { .. }))
            {
                ChatCompletionRequestUserMessageContent::Array(
                    msg.content.iter().filter_map(to_oai_content_part).collect(),
                )
            } else {
                ChatCompletionRequestUserMessageContent::Text(extract_text(&msg.content))
            };
            Ok(ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content,
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

/// One usable delta extracted from an OpenAI-compatible SSE event. Modeled on
/// what this client consumes, not on the wire schema on purpose: Mistral (and
/// anything else implementing "citations") sends `delta.content` as a TYPED
/// PART ARRAY (`[{"type":"reference",…},{"type":"text","text":"…"}]`) once the
/// answer grounds in tool results — async-openai's strict
/// `delta.content: Option<String>` deserialization aborted the whole turn on
/// it (HTTP 200 half-streamed, then JSONDeserialize). Parsing here keeps the
/// text parts and drops the citation markers instead of losing the answer.
struct SseDelta {
    text: Option<String>,
    tool_calls: Vec<SseToolCall>,
    usage: Option<SseUsage>,
}

struct SseToolCall {
    index: u32,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

struct SseUsage {
    prompt: u32,
    completion: u32,
    cached: u64,
}

/// Classify one `data:` payload. `Ok(None)` = nothing to apply (keepalive,
/// role-only chunk); `Err` = fatal stream error.
fn sse_delta(data: &str) -> Result<Option<SseDelta>, OpenAIError> {
    let v: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| OpenAIError::JSONDeserialize(e, data.to_string()))?;
    if let Some(err) = v.get("error") {
        return Err(OpenAIError::StreamError(Box::new(
            StreamError::EventStream(format!("stream error: {err}")),
        )));
    }
    let usage = v.get("usage").filter(|u| u.is_object()).map(|u| SseUsage {
        prompt: json_u64(&u["prompt_tokens"]).unwrap_or(0) as u32,
        completion: json_u64(&u["completion_tokens"]).unwrap_or(0) as u32,
        cached: json_u64(&u["prompt_tokens_details"]["cached_tokens"]).unwrap_or(0),
    });
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for choice in v["choices"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let delta = &choice["delta"];
        match &delta["content"] {
            serde_json::Value::Null | serde_json::Value::Object(_) => {}
            serde_json::Value::String(s) => text.push_str(s),
            serde_json::Value::Array(parts) => {
                for part in parts {
                    // Citation markers carry only document ids — no player-
                    // visible content; every other part contributes its text.
                    if part["type"] == "reference" {
                        continue;
                    }
                    if let Some(t) = part["text"].as_str() {
                        text.push_str(t);
                    }
                }
            }
            other => {
                return Err(OpenAIError::StreamError(Box::new(
                    StreamError::EventStream(format!("unsupported delta.content shape: {other}")),
                )))
            }
        }
        for tc in delta["tool_calls"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            tool_calls.push(SseToolCall {
                index: tc["index"].as_u64().unwrap_or(0) as u32,
                id: tc["id"].as_str().map(str::to_string),
                name: tc["function"]["name"].as_str().map(str::to_string),
                arguments: tc["function"]["arguments"].as_str().map(str::to_string),
            });
        }
    }
    if usage.is_none() && text.is_empty() && tool_calls.is_empty() {
        return Ok(None);
    }
    Ok(Some(SseDelta {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
        usage,
    }))
}

/// Process one SSE line. `None` = end of stream (`data: [DONE]`);
/// `Some(Ok(None))` = separator/keepalive/ignorable field;
/// `Some(Some(Ok))` = delta; `Some(Some(Err))` = fatal.
fn sse_line(line: &str) -> Option<Result<Option<SseDelta>, OpenAIError>> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return Some(Ok(None));
    }
    let Some(data) = line.strip_prefix("data:") else {
        return Some(Ok(None)); // comments, event:/id:/retry: fields
    };
    let data = data.trim();
    if data == "[DONE]" {
        return None;
    }
    if data.is_empty() {
        return Some(Ok(None));
    }
    Some(sse_delta(data))
}

/// Read an SSE body into parsed deltas, ending after `data: [DONE]`. Own SSE
/// reading instead of `Chat::create_stream` for the tolerant [`sse_delta`]
/// shape — the typed path aborts the stream on Mistral's citation chunks.
fn sse_stream<B, T>(
    body: B,
    idle: std::time::Duration,
) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<SseDelta, OpenAIError>> + Send>>
where
    B: futures::Stream<Item = reqwest::Result<T>> + Unpin + Send + 'static,
    T: AsRef<[u8]> + Send + 'static,
{
    Box::pin(futures::stream::unfold(
        (body, String::new(), false, idle),
        |mut st| async move {
            loop {
                if let Some(pos) = st.1.find('\n') {
                    let line: String = st.1.drain(..=pos).collect();
                    match sse_line(&line) {
                        Some(Ok(Some(delta))) => return Some((Ok(delta), st)),
                        Some(Ok(None)) => continue,
                        Some(Err(e)) => return Some((Err(e), st)),
                        None => return None,
                    }
                } else if st.2 {
                    // Body ended: flush a final unterminated line, then stop.
                    if st.1.trim().is_empty() {
                        return None;
                    }
                    let line = std::mem::take(&mut st.1);
                    match sse_line(&line) {
                        Some(Ok(Some(delta))) => return Some((Ok(delta), st)),
                        Some(Ok(None)) => return None,
                        Some(Err(e)) => return Some((Err(e), st)),
                        None => return None,
                    }
                } else {
                    // Per-read idle cap: a provider that opens the response and
                    // then goes silent (mid-stream stall, silently dropped
                    // connection) would otherwise hang here forever.
                    match tokio::time::timeout(st.3, st.0.next()).await {
                        Ok(Some(Ok(bytes))) => {
                            st.1.push_str(&String::from_utf8_lossy(bytes.as_ref()))
                        }
                        Ok(Some(Err(e))) => return Some((Err(OpenAIError::Reqwest(e)), st)),
                        Ok(None) => st.2 = true,
                        Err(_elapsed) => {
                            if crate::ai::stderr_markers_enabled() {
                                eprintln!(
                                    "retry: stream idle for {}s — connection declared stalled",
                                    st.3.as_secs()
                                );
                            }
                            return Some((Err(stream_idle_error(st.3)), st));
                        }
                    }
                }
            }
        },
    ))
}

/// A stall is reported as a 504 so it classifies like every other upstream
/// timeout: retryable, the provider's fault, not a malformed request.
fn stream_idle_error(idle: std::time::Duration) -> OpenAIError {
    OpenAIError::ApiError(ApiErrorResponse {
        status_code: reqwest::StatusCode::GATEWAY_TIMEOUT,
        api_error: ApiError {
            message: format!(
                "stream idle for {}s — no bytes from the provider",
                idle.as_secs()
            ),
            r#type: Some("stream_idle_timeout".to_string()),
            param: None,
            code: None,
        },
    })
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
            temperature: self.temperature,
            ..Default::default()
        };
        // GLM reasoning effort rides on the same dial as the Anthropic thinking
        // budget; everywhere else the field stays absent.
        if let Some(effort) =
            glm_reasoning_effort(&self.base_url, &req.model, self.thinking_budget())
        {
            req.reasoning_effort = Some(effort);
        }
        if !tools.is_empty() {
            req.tools = Some(tools);
        }

        let mut stream = {
            let mut attempt = 0u32;
            // Same wire request `Chat::create_stream` sends (it sets stream=true
            // internally) — only the response side changes: SSE parsed tolerantly
            // ([`sse_stream`]) instead of through strict typed chunks.
            req.stream = Some(true);
            loop {
                attempt += 1;
                // select! so Esc/Ctrl-C interrupts while waiting for the response
                // to open, not only once chunks are streaming (a reasoning model
                // can take many seconds before the first chunk).
                let sent = self
                    .http
                    .post(format!("{}/chat/completions", self.base_url))
                    .bearer_auth(&self.api_key)
                    .json(&req)
                    .send();
                let opened: Result<reqwest::Response, OpenAIError> = select! {
                    r = sent => match r {
                        Ok(resp) if resp.status().is_success() => Ok(resp),
                        Ok(resp) => {
                            let status = resp.status();
                            let text = resp.text().await.unwrap_or_default();
                            // Normalize every failure into ApiError so the existing
                            // 429/5xx retry classification also covers bodies that
                            // aren't the OpenAI error envelope (Mistral's flat
                            // {"message":…,"code":429}, HTML error pages).
                            let api_error = serde_json::from_str::<WrappedError>(&text)
                                .map(|w| w.error)
                                .unwrap_or(ApiError {
                                    message: crate::ai::redact_secrets(&text),
                                    r#type: None,
                                    param: None,
                                    code: None,
                                });
                            Err(OpenAIError::ApiError(ApiErrorResponse {
                                status_code: status,
                                api_error,
                            }))
                        }
                        Err(e) => Err(OpenAIError::Reqwest(e)),
                    },
                    _ = cancel.cancelled() => {
                        events.send(AgentEvent::Cancelled).await.ok();
                        return Ok(TurnResult {
                            assistant_message: Message::assistant(String::new()),
                            state: AgentState::Done,
                            usage: crate::types::TokenUsage::default(),
                            tripped_rule: None,
                        });
                    }
                };
                match opened {
                    Ok(resp) => {
                        break sse_stream(resp.bytes_stream(), crate::ai::stream_idle_secs())
                    }
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
                        // Progress for a driving process (the game): the open
                        // phase failed and a retry is coming — not a hang.
                        if crate::ai::stderr_markers_enabled() {
                            eprintln!(
                                "retry: attempt {attempt}/{MAX_ATTEMPTS} failed, retrying in {secs}s after {}",
                                crate::ai::redact_secrets(&e.to_string())
                            );
                        }
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
                                    tripped_rule: None,
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
        // Cloned out of the lock like `model`; empty unless `stream_rules` is
        // configured, and then the per-delta check is skipped entirely.
        let rules = crate::types::read_or_recover(&self.stream_rules).clone();
        // Rolling tail of the generated text, matched against the rules.
        let mut rule_tail = String::new();
        let mut tripped: Option<String> = None;

        'stream: loop {
            select! {
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(delta)) => {
                            if let Some(u) = &delta.usage {
                                if u.cached > 0 {
                                    tracing::info!(cached = u.cached, "prompt cache stats");
                                }
                                usage.input = u.prompt;
                                usage.output = u.completion;
                                events.send(AgentEvent::ContextUsage {
                                    used_tokens: u.prompt,
                                    context_window: self.context_window().await.unwrap_or(128_000),
                                    cached_tokens: u.cached as u32,
                                    output_tokens: u.completion,
                                }).await.ok();
                            }
                            if let Some(text) = delta.text {
                                text_parts.push(text.clone());
                                events.send(AgentEvent::TextChunk(text.clone())).await.ok();
                                if !rules.is_empty() {
                                    rule_tail.push_str(&text);
                                    rule_tail = crate::stream_rules::window(&rule_tail).to_string();
                                    if let Some(rule) = rules.trip(&rule_tail, &[]) {
                                        tripped = Some(rule.name.clone());
                                        break 'stream;
                                    }
                                }
                            }
                            for tc in delta.tool_calls {
                                let entry = tool_map.entry(tc.index).or_default();
                                if let Some(id) = tc.id {
                                    entry.id = id;
                                }
                                if let Some(name) = tc.name {
                                    entry.name = name;
                                }
                                if let Some(args) = tc.arguments {
                                    entry.arguments.push_str(&args);
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
                        tripped_rule: None,
                    });
                }
            }
        }

        if let Some(name) = tripped {
            // Partial by construction — the agent discards it and re-runs the
            // turn with the rule injected as a reminder. `stream` is dropped
            // here, closing the connection.
            return Ok(TurnResult {
                assistant_message: Message::assistant(text_parts.join("")),
                state: AgentState::Done,
                usage,
                tripped_rule: Some(name),
            });
        }

        events.send(AgentEvent::TurnEnd).await.ok();

        let text = text_parts.join("");

        if tool_map.is_empty() {
            return Ok(TurnResult {
                assistant_message: Message::assistant(text),
                state: AgentState::Done,
                usage,
                tripped_rule: None,
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
                injected: false,
                content,
            },
            state: AgentState::ToolCalling(tool_calls),
            usage,
            tripped_rule: None,
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

    fn set_thinking_budget(&self, budget: Option<u32>) {
        self.thinking_budget
            .store(budget.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    }

    /// The dial only does something on GLM, so only report it there — a level
    /// shown for a provider that ignores it would be a lie in the Settings UI.
    fn thinking_budget(&self) -> Option<u32> {
        if !super::is_glm(
            &self.base_url,
            &crate::types::read_or_recover(&self.model).clone(),
        ) {
            return None;
        }
        match self
            .thinking_budget
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0 => None,
            n => Some(n),
        }
    }

    fn set_stream_rules(&self, rules: std::sync::Arc<crate::stream_rules::StreamRules>) {
        *crate::types::write_or_recover(&self.stream_rules) = rules;
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
        let resp = self
            .http
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

    /// A text-only turn stays a plain string — endpoints that only parse the
    /// simple form must not start seeing an array.
    #[test]
    fn text_only_user_message_stays_a_string() {
        let msg = Message::user("hello");
        let out = serde_json::to_value(to_oai_message(&msg).expect("message")).expect("json");
        assert_eq!(out["content"], json!("hello"));
    }

    #[test]
    fn images_become_data_uri_parts_in_block_order() {
        let msg = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAA".into(),
                },
                ContentBlock::Image {
                    media_type: "image/jpeg".into(),
                    data: "BBB".into(),
                },
                ContentBlock::Text {
                    text: "what is this?".into(),
                },
            ],
            ..Message::user("")
        };
        let out = serde_json::to_value(to_oai_message(&msg).expect("message")).expect("json");
        assert_eq!(
            out["content"],
            json!([
                {
                    "type": "image_url",
                    "image_url": { "url": "data:image/png;base64,AAA", "detail": "auto" },
                },
                {
                    "type": "image_url",
                    "image_url": { "url": "data:image/jpeg;base64,BBB", "detail": "auto" },
                },
                { "type": "text", "text": "what is this?" },
            ])
        );
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

    /// A provider that opens the stream and then never sends another byte must
    /// produce an Err (classified retryable), not hang the reader forever —
    /// the 2026-09-02 live hang ("thinking…" for 15+ minutes, no error).
    #[test]
    fn sse_stream_errors_after_the_idle_cap_instead_of_hanging() {
        use futures::stream::{once, pending};
        use std::time::Duration;

        // one chunk, then silence forever
        let body: std::pin::Pin<
            Box<dyn futures::Stream<Item = reqwest::Result<&'static [u8]>> + Send>,
        > = Box::pin(once(async { Ok("data: {\"x\":1}\n\n".as_bytes()) }).chain(pending()));
        let idle = Duration::from_millis(80);
        let mut stream = sse_stream(body, idle);

        let start = std::time::Instant::now();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let first = rt.block_on(futures::StreamExt::next(&mut stream));
        assert!(first.is_some(), "the buffered line arrives first");
        let second = rt.block_on(futures::StreamExt::next(&mut stream));
        let elapsed = start.elapsed();
        match second {
            Some(Err(OpenAIError::ApiError(resp))) => {
                assert_eq!(resp.status_code, reqwest::StatusCode::GATEWAY_TIMEOUT);
            }
            Some(Ok(_)) => panic!("expected the idle-timeout error, got a delta"),
            Some(Err(e)) => panic!("expected a 504 idle error, got {e}"),
            None => panic!("stream ended without reporting the stall"),
        }
        assert!(
            elapsed >= idle,
            "the cap must actually wait the idle period"
        );
    }

    #[test]
    fn stream_idle_secs_parsing() {
        assert_eq!(
            super::super::stream_idle_secs_from(None),
            super::super::DEFAULT_STREAM_IDLE_SECS
        );
        assert_eq!(super::super::stream_idle_secs_from(Some(" 30 ")), 30);
        assert_eq!(
            super::super::stream_idle_secs_from(Some("0")),
            super::super::DEFAULT_STREAM_IDLE_SECS
        );
        assert_eq!(
            super::super::stream_idle_secs_from(Some("abc")),
            super::super::DEFAULT_STREAM_IDLE_SECS
        );
    }

    #[test]
    fn assistant_tool_use_maps_to_tool_calls() {
        let msg = Message {
            role: Role::Assistant,
            injected: false,
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
            injected: false,
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
    async fn mistral_citation_chunks_do_not_abort_the_stream() {
        // Mistral sends delta.content as a TYPED PART ARRAY once the answer
        // cites tool results ({"type":"reference"} + {"type":"text"} chunks);
        // the strict typed deserialization aborted the turn mid-stream. The
        // text parts must survive, citation markers are dropped, and the
        // trailing usage-only chunk still lands.
        init_crypto();
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/chat/completions");
                then.status(200).header("content-type", "text/event-stream").body(oai_sse(&[
                    r#"{"id":"x","choices":[{"index":0,"delta":{"role":"assistant"}}],"created":0,"model":"m","object":"chat.completion.chunk"}"#,
                    r#"{"id":"x","choices":[{"index":0,"delta":{"content":[{"type":"reference","reference_ids":[1,2]},{"type":"text","text":"grounded"}]}}],"created":0,"model":"m","object":"chat.completion.chunk"}"#,
                    r#"{"id":"x","choices":[{"index":0,"delta":{"content":"."}}],"created":0,"model":"m","object":"chat.completion.chunk"}"#,
                    r#"{"id":"x","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":0}},"created":0,"model":"m","object":"chat.completion.chunk"}"#,
                ]));
            })
            .await;
        let client = OpenAiClient::new(&server.base_url(), "k", "m");
        let reg = ToolRegistry::new();
        let (tx, mut rx) = mpsc::channel(64);
        let out = client
            .run_turn(&[&Message::user("q")], &reg, &tx, &CancellationToken::new())
            .await
            .expect("turn succeeds despite citation chunks");
        drop(tx);
        while rx.recv().await.is_some() {}
        let text = extract_text(&out.assistant_message.content);
        assert_eq!(text, "grounded.");
        assert_eq!(out.usage.input, 10);
        assert_eq!(out.usage.output, 2);
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

    #[test]
    fn reasoning_effort_only_for_glm_dial_levels() {
        use async_openai::types::chat::ReasoningEffort;
        // Off still sends minimal: a GLM endpoint left without the field
        // defaults to a heavy effort, so "off" has to be explicit.
        assert_eq!(
            glm_reasoning_effort("https://api.z.ai/api/coding/paas/v4", "glm-5.2", None),
            Some(ReasoningEffort::Minimal)
        );
        assert_eq!(
            glm_reasoning_effort("https://api.z.ai/api/coding/paas/v4", "glm-5.2", Some(8000)),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            glm_reasoning_effort(
                "https://api.z.ai/api/coding/paas/v4",
                "glm-5.3",
                Some(16000)
            ),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            glm_reasoning_effort(
                "https://api.z.ai/api/coding/paas/v4",
                "glm-5.3",
                Some(32000)
            ),
            Some(ReasoningEffort::Xhigh)
        );
        // Non-GLM: the field must never be sent — other providers 400 on it.
        assert_eq!(
            glm_reasoning_effort("https://api.openai.com/v1", "gpt-4o-mini", Some(16000)),
            None
        );
    }

    #[test]
    fn thinking_budget_reported_only_for_glm() {
        init_crypto();
        let glm = OpenAiClient::new("https://api.z.ai/api/coding/paas/v4", "k", "glm-5.2");
        glm.set_thinking_budget(Some(16000));
        assert_eq!(glm.thinking_budget(), Some(16000));
        let other = OpenAiClient::new("https://api.openai.com/v1", "k", "gpt-4o-mini");
        other.set_thinking_budget(Some(16000));
        // Stored but inert: reporting it would show a level that does nothing.
        assert_eq!(other.thinking_budget(), None);
    }

    #[tokio::test]
    async fn run_turn_sends_reasoning_effort_for_glm() {
        init_crypto();
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/chat/completions")
                    .body_includes("glm-distinct")
                    .body_includes("\"reasoning_effort\":\"medium\"");
                then.status(200).header("content-type", "text/event-stream").body(oai_sse(&[
                    r#"{"id":"x","choices":[{"index":0,"delta":{"content":"ok"}}],"created":0,"model":"m","object":"chat.completion.chunk"}"#,
                ]));
            })
            .await;
        let client = OpenAiClient::new(&server.base_url(), "k", "glm-distinct");
        client.set_thinking_budget(Some(16000));
        let (tx, mut rx) = mpsc::channel(64);
        client
            .run_turn(
                &[&Message::user("hello")],
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

    /// `temperature` rides in the body only when pinned: unset must leave the
    /// provider default in charge (1.0 on GLM-5.x), not send an explicit value.
    async fn temperature_wire(temperature: Option<f32>, sent: bool) {
        init_crypto();
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                let when = when.method(POST).path("/chat/completions");
                if sent {
                    when.body_includes("\"temperature\":0.0");
                } else {
                    when.body_excludes("temperature");
                }
                then.status(200).header("content-type", "text/event-stream").body(oai_sse(&[
                    r#"{"id":"x","choices":[{"index":0,"delta":{"content":"ok"}}],"created":0,"model":"m","object":"chat.completion.chunk"}"#,
                ]));
            })
            .await;
        let client =
            OpenAiClient::new(&server.base_url(), "k", "glm-5.2").with_temperature(temperature);
        let (tx, mut rx) = mpsc::channel(64);
        client
            .run_turn(
                &[&Message::user("hello")],
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

    #[tokio::test]
    async fn run_turn_sends_temperature_when_pinned() {
        temperature_wire(Some(0.0), true).await;
    }

    #[tokio::test]
    async fn run_turn_omits_temperature_when_unset() {
        temperature_wire(None, false).await;
    }

    #[tokio::test]
    async fn run_turn_omits_reasoning_effort_for_non_glm() {
        init_crypto();
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/chat/completions")
                    .body_includes("gpt-distinct")
                    .body_excludes("reasoning_effort");
                then.status(200).header("content-type", "text/event-stream").body(oai_sse(&[
                    r#"{"id":"x","choices":[{"index":0,"delta":{"content":"ok"}}],"created":0,"model":"m","object":"chat.completion.chunk"}"#,
                ]));
            })
            .await;
        let client = OpenAiClient::new(&server.base_url(), "k", "gpt-distinct");
        client.set_thinking_budget(Some(16000));
        let (tx, mut rx) = mpsc::channel(64);
        client
            .run_turn(
                &[&Message::user("hello")],
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
}
