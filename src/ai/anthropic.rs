use std::collections::HashMap;

use crate::{
    agent::{LlmClient, TurnResult},
    tools::ToolRegistry,
    types::{extract_text, AgentEvent, AgentState, ContentBlock, EventTx, Message, Role, ToolCall},
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::select;
use tokio_util::sync::CancellationToken;

use super::{backoff_secs, json_u64, MAX_ATTEMPTS};

pub struct AnthropicClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    /// Current model. Behind a lock so it can be switched at runtime through a
    /// shared `Arc<dyn LlmClient>` (e.g. the `/model` picker).
    model: std::sync::RwLock<String>,
    /// Extended-thinking budget in tokens, behind interior mutability so it can be
    /// changed at runtime through a shared `Arc<dyn LlmClient>` (e.g. the Settings
    /// screen). 0 = disabled.
    thinking_budget: std::sync::atomic::AtomicU32,
    /// Context window of the active model, lazily fetched from `/v1/models/{id}`
    /// and cached. 0 = not fetched yet (reset by `set_model`).
    context_window: std::sync::atomic::AtomicU32,
}

impl AnthropicClient {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: std::sync::RwLock::new(model.to_string()),
            thinking_budget: std::sync::atomic::AtomicU32::new(0),
            context_window: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

fn to_anthropic_request(
    messages: &[&Message],
    registry: &ToolRegistry,
    model: &str,
    thinking_budget: Option<u32>,
) -> serde_json::Value {
    let mut system: Option<String> = None;
    let mut anthro_msgs: Vec<serde_json::Value> = vec![];

    for &msg in messages {
        match msg.role {
            Role::System => {
                system = Some(extract_text(&msg.content));
            }
            Role::User => {
                let mut blocks: Vec<serde_json::Value> = vec![];
                for c in &msg.content {
                    match c {
                        ContentBlock::Text { text } => {
                            blocks.push(serde_json::json!({"type": "text", "text": text}));
                        }
                        ContentBlock::Image { media_type, data } => {
                            blocks.push(serde_json::json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": media_type,
                                    "data": data,
                                }
                            }));
                        }
                        _ => {}
                    }
                }
                if blocks.len() == 1 && blocks[0]["type"] == "text" {
                    // Simple text-only message — keep as plain string for compat
                    anthro_msgs.push(serde_json::json!({
                        "role": "user",
                        "content": extract_text(&msg.content),
                    }));
                } else {
                    anthro_msgs.push(serde_json::json!({"role": "user", "content": blocks}));
                }
            }
            Role::Assistant => {
                let content: Vec<serde_json::Value> = msg
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            Some(serde_json::json!({"type": "text", "text": text}))
                        }
                        ContentBlock::Thinking { thinking } => {
                            Some(serde_json::json!({"type": "thinking", "thinking": thinking}))
                        }
                        ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        })),
                        _ => None,
                    })
                    .collect();
                anthro_msgs.push(serde_json::json!({"role": "assistant", "content": content}));
            }
            Role::Tool => {
                // Merge consecutive tool results into one user message
                let tool_blocks: Vec<serde_json::Value> = msg
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => Some(serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": content,
                            "is_error": is_error,
                        })),
                        _ => None,
                    })
                    .collect();

                // If the last message is already a user message (tool results merge batch)
                if let Some(last) = anthro_msgs.last_mut() {
                    if last["role"] == "user" {
                        if let Some(arr) = last["content"].as_array_mut() {
                            arr.extend(tool_blocks);
                            continue;
                        }
                    }
                }
                anthro_msgs.push(serde_json::json!({"role": "user", "content": tool_blocks}));
            }
        }
    }

    // Prompt caching: mark the last content block of the final message so each
    // request reuses the previous turn's cached prefix — without this only
    // system+tools are cached and the whole conversation history is re-read at
    // full input price every turn. String-form content is promoted to a block,
    // since cache_control only attaches to blocks.
    let cache = !crate::ablate::cache_disabled();
    if let Some(last) = anthro_msgs.last_mut().filter(|_| cache) {
        if let Some(text) = last["content"].as_str() {
            let text = text.to_string();
            last["content"] = serde_json::json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"}
            }]);
        } else if let Some(block) = last["content"].as_array_mut().and_then(|a| a.last_mut()) {
            block["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }
    }

    let mut tools: Vec<serde_json::Value> = registry
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name(),
                "description": t.description(),
                "input_schema": t.schema(),
            })
        })
        .collect();

    // Prompt caching: mark last tool definition with cache_control
    if let Some(last) = tools.last_mut().filter(|_| cache) {
        last["cache_control"] = serde_json::json!({"type": "ephemeral"});
    }

    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": 16384,
        "messages": anthro_msgs,
        "stream": true,
    });

    if let Some(sys) = system {
        // Prompt caching: system prompt as content block with cache_control
        // (omitted when ablation disables caching, so it stays plain text).
        body["system"] = if cache {
            serde_json::json!([{
                "type": "text",
                "text": sys,
                "cache_control": {"type": "ephemeral"}
            }])
        } else {
            serde_json::json!(sys)
        };
    }
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
    }

    // Extended thinking
    if let Some(budget) = thinking_budget {
        body["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": budget
        });
    }

    body
}

struct AccumTool {
    id: String,
    name: String,
    input_json: String,
}

/// Traverse the reqwest error source chain to surface the deepest cause.
fn enrich_reqwest_error(e: reqwest::Error) -> anyhow::Error {
    let url = e.url().map(|u| u.as_str()).unwrap_or("unknown").to_string();

    // Walk to the deepest source
    let root_cause = {
        let mut msg = String::new();
        let mut src: &dyn std::error::Error = &e;
        while let Some(next) = src.source() {
            msg = next.to_string();
            src = next;
        }
        msg
    };

    let kind = if e.is_connect() {
        "connection failed — check ANTHROPIC_BASE_URL / network"
    } else if e.is_timeout() {
        "request timed out"
    } else {
        ""
    };

    match (root_cause.is_empty(), kind.is_empty()) {
        (false, false) => anyhow!("API request to {url} failed: {root_cause} ({kind})"),
        (false, true) => anyhow!("API request to {url} failed: {root_cause}"),
        (true, false) => anyhow!("API request to {url} failed ({kind})"),
        (true, true) => anyhow!("{e}"),
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn run_turn(
        &self,
        messages: &[&Message],
        registry: &ToolRegistry,
        events: &EventTx,
        cancel: &CancellationToken,
    ) -> Result<TurnResult> {
        // Clone out of the lock so no guard is held across an `.await`.
        let model = crate::types::read_or_recover(&self.model).clone();
        let body = to_anthropic_request(messages, registry, &model, self.thinking_budget());
        let url = format!("{}/v1/messages", self.base_url);
        // Real window when discoverable; conservative fallback otherwise.
        let context_window = self.context_window().await.unwrap_or(200_000);

        let mut text_parts: Vec<String> = vec![];
        let mut thinking_parts: Vec<String> = vec![];
        let mut tool_map: HashMap<usize, AccumTool> = HashMap::new();
        let mut usage = crate::types::TokenUsage::default();

        // Send + stream wrapped in one retry loop. Tools run only *after* this
        // returns (in the agent loop), so re-sending on a transient mid-stream
        // failure has no side effects. Retryable: network drops, 429/5xx, and
        // `overloaded`/network stream-error events. Terminal: auth, bad request,
        // and quota/usage limits (short backoff can't help). On retry we re-emit
        // partial TextChunk events — cosmetic double-print in interactive mode.
        let mut attempt = 0u32;
        let outcome = loop {
            attempt += 1;

            // --- send (network failure is retryable) ---
            let resp = match self
                .http
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let e = enrich_reqwest_error(e);
                    if attempt >= MAX_ATTEMPTS {
                        break Outcome::Fatal(e);
                    }
                    tracing::warn!(
                        attempt,
                        "send failed, retrying: {}",
                        crate::ai::redact_secrets(&e.to_string())
                    );
                    select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs(attempt))) => {}
                        _ = cancel.cancelled() => break Outcome::Cancelled,
                    }
                    continue;
                }
            };

            // --- status (rate limit / overload -> retry, honoring retry-after) ---
            let status = resp.status().as_u16();
            if status == 429 || (500..600).contains(&status) {
                let ra = retry_after_secs(&resp);
                // A long retry-after means a sustained limit (e.g. the quota
                // window) — don't sit on it, fail fast.
                if ra.is_some_and(|s| s > RETRY_AFTER_CAP_SECS) || attempt >= MAX_ATTEMPTS {
                    let body_text =
                        crate::ai::redact_secrets(&resp.text().await.unwrap_or_default());
                    break Outcome::Fatal(anyhow!(
                        "Anthropic API error {status} (after {attempt} attempts): {body_text}"
                    ));
                }
                let secs = ra.unwrap_or_else(|| backoff_secs(attempt));
                tracing::warn!(status, attempt, secs, "retrying after rate limit/overload");
                select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
                    _ = cancel.cancelled() => break Outcome::Cancelled,
                }
                continue;
            }
            if !resp.status().is_success() {
                let body_text = crate::ai::redact_secrets(&resp.text().await.unwrap_or_default());
                break Outcome::Fatal(anyhow!("Anthropic API error {status}: {body_text}"));
            }

            // --- stream (fresh accumulators per attempt) ---
            text_parts.clear();
            thinking_parts.clear();
            tool_map.clear();
            let mut byte_stream = resp.bytes_stream();
            let mut line_buf = String::new();

            let stream_outcome = 'stream: loop {
                select! {
                    chunk = byte_stream.next() => {
                        match chunk {
                            Some(Ok(bytes)) => {
                                line_buf.push_str(&String::from_utf8_lossy(&bytes));
                                loop {
                                    match line_buf.find('\n') {
                                        None => break,
                                        Some(pos) => {
                                            // Borrow the line as a slice instead of allocating a
                                            // fresh String per SSE event; drain after use.
                                            let raw = line_buf[..pos].trim_end_matches('\r');
                                            if let Some(data) = raw.strip_prefix("data: ") {
                                                if data == "[DONE]" { break 'stream Outcome::Done; }
                                                if let Ok(ev) = serde_json::from_str::<serde_json::Value>(data) {
                                                    // Streaming APIs (e.g. z.ai) may return HTTP 200 then
                                                    // emit an `error` SSE event. Classify and surface it
                                                    // instead of silently ending with empty output.
                                                    if ev["type"].as_str() == Some("error") {
                                                        let msg = ev["error"]["message"].as_str().unwrap_or("unknown error");
                                                        let full = format!("Anthropic API stream error: {msg}");
                                                        break 'stream if stream_error_retryable(&ev) {
                                                            Outcome::Retry(full)
                                                        } else {
                                                            Outcome::Fatal(anyhow!(full))
                                                        };
                                                    }
                                                    handle_event(ev, &mut text_parts, &mut thinking_parts, &mut tool_map, &mut usage, events, context_window).await;
                                                }
                                            }
                                            line_buf.drain(..=pos);
                                        }
                                    }
                                }
                            }
                            // network drop mid-stream is transient -> retry the turn
                            Some(Err(e)) => break 'stream Outcome::Retry(enrich_reqwest_error(e).to_string()),
                            None => break 'stream Outcome::Done,
                        }
                    }
                    _ = cancel.cancelled() => break 'stream Outcome::Cancelled,
                }
            };

            match stream_outcome {
                Outcome::Retry(msg) if attempt < MAX_ATTEMPTS => {
                    tracing::warn!(
                        attempt,
                        "retrying after stream error: {}",
                        crate::ai::redact_secrets(&msg)
                    );
                    select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs(attempt))) => {}
                        _ = cancel.cancelled() => break Outcome::Cancelled,
                    }
                    continue;
                }
                other => break other,
            }
        };

        // Resolve the loop outcome; Done falls through to message assembly.
        match outcome {
            Outcome::Cancelled => {
                events.send(AgentEvent::Cancelled).await.ok();
                let text = text_parts.join("");
                return Ok(TurnResult {
                    assistant_message: Message::assistant(text),
                    state: AgentState::Done,
                    usage,
                });
            }
            Outcome::Fatal(e) => {
                events.send(AgentEvent::Error(e.to_string())).await.ok();
                return Err(e);
            }
            Outcome::Retry(msg) => {
                let e = anyhow!("{msg} (after {MAX_ATTEMPTS} attempts)");
                events.send(AgentEvent::Error(e.to_string())).await.ok();
                return Err(e);
            }
            Outcome::Done => {}
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
                    serde_json::from_str(&acc.input_json).unwrap_or(serde_json::json!({}));
                ToolCall {
                    id: acc.id,
                    name: acc.name,
                    arguments,
                }
            })
            .collect();

        let mut content = Vec::new();
        let thinking = thinking_parts.join("");
        if !thinking.is_empty() {
            content.push(ContentBlock::Thinking { thinking });
        }
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
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(enrich_reqwest_error)?;
        if !resp.status().is_success() {
            anyhow::bail!("list models failed: HTTP {}", resp.status());
        }
        let body: serde_json::Value = resp.json().await?;
        Ok(body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(String::from))
            .collect())
    }

    fn set_model(&self, model: String) {
        *crate::types::write_or_recover(&self.model) = model;
        // Window is per-model — refetch on next use.
        self.context_window
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

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
        let url = format!("{}/v1/models/{model}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        let n = json_u64(&v["max_input_tokens"])? as u32;
        self.context_window.store(n, Relaxed);
        Some(n)
    }

    fn set_thinking_budget(&self, budget: Option<u32>) {
        self.thinking_budget
            .store(budget.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    }

    fn thinking_budget(&self) -> Option<u32> {
        match self
            .thinking_budget
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0 => None,
            n => Some(n),
        }
    }

    async fn count_tokens(&self, messages: &[&Message], registry: &ToolRegistry) -> Result<u64> {
        let model = crate::types::read_or_recover(&self.model).clone();
        // Same system + tools + messages assembly as a real request, minus the
        // fields the count endpoint rejects, so the count matches what we send.
        let mut body = to_anthropic_request(messages, registry, &model, None);
        if let Some(obj) = body.as_object_mut() {
            obj.remove("max_tokens");
            obj.remove("stream");
        }
        let url = format!("{}/v1/messages/count_tokens", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(enrich_reqwest_error)?;
        if !resp.status().is_success() {
            return Err(anyhow!("count_tokens failed: HTTP {}", resp.status()));
        }
        let v: serde_json::Value = resp.json().await?;
        v["input_tokens"]
            .as_u64()
            .ok_or_else(|| anyhow!("count_tokens: missing input_tokens"))
    }
}

/// Track which content block indices are thinking blocks.
async fn handle_event(
    ev: serde_json::Value,
    text_parts: &mut Vec<String>,
    thinking_parts: &mut Vec<String>,
    tool_map: &mut HashMap<usize, AccumTool>,
    usage: &mut crate::types::TokenUsage,
    events: &EventTx,
    context_window: u32,
) {
    let ev_type = ev["type"].as_str().unwrap_or("");
    match ev_type {
        "message_start" => {
            if let Some(tokens) = ev["message"]["usage"]["input_tokens"].as_u64() {
                let cache_read = ev["message"]["usage"]["cache_read_input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                let cache_creation = ev["message"]["usage"]["cache_creation_input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                if cache_read > 0 || cache_creation > 0 {
                    tracing::info!(cache_read, cache_creation, "prompt cache stats");
                }
                // input_tokens alone is only the uncached remainder; the real
                // prompt size is the sum of all three usage fields.
                usage.input = (tokens + cache_read + cache_creation) as u32;
                events
                    .send(AgentEvent::ContextUsage {
                        used_tokens: (tokens + cache_read + cache_creation) as u32,
                        context_window,
                        cached_tokens: cache_read as u32,
                    })
                    .await
                    .ok();
            }
        }
        // Some Anthropic-compatible endpoints (z.ai) report a 0 usage at
        // message_start and the real prompt size only in the final message_delta.
        // Emit again from here so token accounting isn't stuck at zero.
        "message_delta" => {
            // Output tokens are reported here (final cumulative count for the message).
            if let Some(out) = ev["usage"]["output_tokens"].as_u64() {
                usage.output = out as u32;
            }
            if let Some(tokens) = ev["usage"]["input_tokens"].as_u64() {
                if tokens > 0 {
                    let cache_read = ev["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    let cache_creation = ev["usage"]["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    usage.input = (tokens + cache_read + cache_creation) as u32;
                    events
                        .send(AgentEvent::ContextUsage {
                            used_tokens: (tokens + cache_read + cache_creation) as u32,
                            context_window,
                            cached_tokens: cache_read as u32,
                        })
                        .await
                        .ok();
                }
            }
        }
        "content_block_start" => {
            let idx = ev["index"].as_u64().unwrap_or(0) as usize;
            let block = &ev["content_block"];
            match block["type"].as_str().unwrap_or("") {
                "tool_use" => {
                    tool_map.insert(
                        idx,
                        AccumTool {
                            id: block["id"].as_str().unwrap_or("").to_string(),
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            input_json: String::new(),
                        },
                    );
                }
                "thinking" => {
                    events.send(AgentEvent::ThinkingStart).await.ok();
                }
                _ => {}
            }
        }
        "content_block_delta" => {
            let delta = &ev["delta"];
            match delta["type"].as_str().unwrap_or("") {
                "text_delta" => {
                    if let Some(text) = delta["text"].as_str() {
                        text_parts.push(text.to_string());
                        events
                            .send(AgentEvent::TextChunk(text.to_string()))
                            .await
                            .ok();
                    }
                }
                "thinking_delta" => {
                    if let Some(text) = delta["thinking"].as_str() {
                        thinking_parts.push(text.to_string());
                        events
                            .send(AgentEvent::ThinkingChunk(text.to_string()))
                            .await
                            .ok();
                    }
                }
                "input_json_delta" => {
                    let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                    if let Some(partial) = delta["partial_json"].as_str() {
                        if let Some(acc) = tool_map.get_mut(&idx) {
                            acc.input_json.push_str(partial);
                        }
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// A `retry-after` longer than this signals a sustained limit (e.g. the quota
/// window) — treat as terminal rather than sleeping on it.
const RETRY_AFTER_CAP_SECS: u64 = 120;

/// Loop outcome for a single send+stream attempt.
enum Outcome {
    /// Stream completed; assemble the turn from the accumulators.
    Done,
    /// User cancelled (Ctrl-C) — never retried.
    Cancelled,
    /// Transient failure; the outer loop retries after backoff.
    Retry(String),
    /// Terminal failure; return the error.
    Fatal(anyhow::Error),
}

/// Parse the `retry-after` header: integer-seconds form first, then the
/// RFC 7231 HTTP-date form (`Wed, 21 Oct 2026 07:28:00 GMT`) → seconds from now.
/// Returning None on an unrecognized value lets the caller fall back to
/// exponential backoff.
fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    let raw = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(secs);
    }
    let dt = chrono::DateTime::parse_from_rfc2822(raw).ok()?;
    let secs = dt.signed_duration_since(chrono::Utc::now()).num_seconds();
    Some(secs.max(0) as u64)
}

/// Whether a streamed `error` SSE event is worth retrying. Retryable: transient
/// server overload / network blips. Terminal: auth, bad request, and quota/usage
/// limits (e.g. z.ai code 1308) where short backoff can't help.
fn stream_error_retryable(ev: &serde_json::Value) -> bool {
    let err = &ev["error"];
    let etype = err["type"].as_str().unwrap_or("");
    let code = err["code"].as_str().unwrap_or("");
    let msg = err["message"].as_str().unwrap_or("").to_lowercase();
    if code == "1308" || msg.contains("usage limit") || msg.contains("quota") {
        return false; // quota / sustained limit -> terminal
    }
    matches!(etype, "overloaded_error" | "api_error")
        || code == "1234" // z.ai transient network error
        || msg.contains("network error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ReadTool;
    use serde_json::json;
    use tokio::sync::mpsc;

    /// `AnthropicClient::new` builds a reqwest client, which (with
    /// `rustls-no-provider`) needs an installed crypto provider. Idempotent.
    fn init_crypto() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    // --- to_anthropic_request: message/role/block mapping ---

    #[test]
    fn to_anthropic_request_maps_roles_and_blocks() {
        let msgs = [
            Message::system("SYS"),
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text { text: "hi".into() },
                    ContentBlock::Image {
                        media_type: "image/png".into(),
                        data: "DATA".into(),
                    },
                ],
            },
            Message::user("just text"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "th".into(),
                    },
                    ContentBlock::Text { text: "ans".into() },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "read".into(),
                        input: json!({"path": "x"}),
                    },
                ],
            },
            Message::tool_result("t1", "RESULT", false),
        ];
        let mut reg = ToolRegistry::new();
        reg.register(ReadTool::default());
        let body = to_anthropic_request(&msgs.iter().collect::<Vec<_>>(), &reg, "m", Some(1024));

        // Top-level shape (kills the `Default::default()` whole-body mutant).
        assert_eq!(body["model"], "m");
        assert_eq!(body["max_tokens"], 16384);
        // System prompt: cached content block.
        assert_eq!(body["system"][0]["text"], "SYS");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        // Extended thinking passed through.
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        // Tools: last one carries the cache breakpoint.
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");

        let m = body["messages"].as_array().unwrap();
        // [0] user with image -> blocks array carrying both text and image.
        assert_eq!(m[0]["role"], "user");
        let blocks = m[0]["content"].as_array().unwrap();
        assert!(blocks
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == "hi"));
        assert!(blocks.iter().any(|b| b["type"] == "image"
            && b["source"]["media_type"] == "image/png"
            && b["source"]["data"] == "DATA"));
        // [1] text-only user -> collapsed to a plain string (kills the `len()==1 && type==text` mutants).
        assert_eq!(m[1]["content"], "just text");
        // [2] assistant -> thinking + text + tool_use, in order.
        let a = m[2]["content"].as_array().unwrap();
        assert!(a
            .iter()
            .any(|b| b["type"] == "thinking" && b["thinking"] == "th"));
        assert!(a.iter().any(|b| b["type"] == "text" && b["text"] == "ans"));
        assert!(a
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "read" && b["id"] == "t1"));
        // [3] tool result -> its own user message.
        assert_eq!(m[3]["role"], "user");
        assert_eq!(m[3]["content"][0]["type"], "tool_result");
        assert_eq!(m[3]["content"][0]["tool_use_id"], "t1");
        assert_eq!(m[3]["content"][0]["is_error"], false);
        // Last block of the final message carries the conversation cache breakpoint;
        // earlier messages must not.
        assert_eq!(m[3]["content"][0]["cache_control"]["type"], "ephemeral");
        assert!(m[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["cache_control"].is_null()));
    }

    #[test]
    fn to_anthropic_request_caches_string_form_final_message() {
        // A text-only final user message is stored as a plain string; the cache
        // breakpoint must promote it to a content block.
        let msgs = [Message::user("hello")];
        let body = to_anthropic_request(
            &msgs.iter().collect::<Vec<_>>(),
            &ToolRegistry::new(),
            "m",
            None,
        );
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m[0]["content"][0]["type"], "text");
        assert_eq!(m[0]["content"][0]["text"], "hello");
        assert_eq!(m[0]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn to_anthropic_request_drops_empty_assistant_text() {
        // Empty assistant text must NOT become a `{type:text,text:""}` block
        // (pins the `!text.is_empty()` guard against both true/false mutations).
        let msgs = [Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: String::new(),
                },
                ContentBlock::ToolUse {
                    id: "t".into(),
                    name: "read".into(),
                    input: json!({}),
                },
            ],
        }];
        let body = to_anthropic_request(
            &msgs.iter().collect::<Vec<_>>(),
            &ToolRegistry::new(),
            "m",
            None,
        );
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert!(
            !blocks.iter().any(|b| b["type"] == "text"),
            "empty text omitted"
        );
        assert!(blocks.iter().any(|b| b["type"] == "tool_use"));
        // No thinking budget -> no thinking field.
        assert!(body.get("thinking").is_none());
    }

    // --- handle_event: arms that emit events ---

    #[tokio::test]
    async fn handle_event_message_start_emits_context_usage() {
        let (tx, mut rx) = mpsc::channel(8);
        let (mut tp, mut th, mut tm) = (Vec::new(), Vec::new(), HashMap::new());
        handle_event(
            json!({"type":"message_start","message":{"usage":{
                "input_tokens":10, "cache_read_input_tokens":70, "cache_creation_input_tokens":20
            }}}),
            &mut tp,
            &mut th,
            &mut tm,
            &mut crate::types::TokenUsage::default(),
            &tx,
            1_000_000,
        )
        .await;
        drop(tx);
        let mut saw_usage = false;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::ContextUsage {
                used_tokens,
                context_window,
                cached_tokens,
            } = ev
            {
                assert_eq!(
                    context_window, 1_000_000,
                    "window must come from the caller"
                );
                // input_tokens is the uncached remainder; used must be the sum.
                assert_eq!(used_tokens, 100);
                assert_eq!(cached_tokens, 70);
                saw_usage = true;
            }
        }
        assert!(saw_usage, "message_start should emit ContextUsage");
    }

    #[tokio::test]
    async fn handle_event_thinking_block_emits_thinking_start() {
        let (tx, mut rx) = mpsc::channel(8);
        let (mut tp, mut th, mut tm) = (Vec::new(), Vec::new(), HashMap::new());
        handle_event(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            &mut tp, &mut th, &mut tm, &mut crate::types::TokenUsage::default(), &tx, 200_000,
        ).await;
        drop(tx);
        let mut saw = false;
        while let Some(ev) = rx.recv().await {
            if matches!(ev, AgentEvent::ThinkingStart) {
                saw = true;
            }
        }
        assert!(
            saw,
            "thinking content_block_start should emit ThinkingStart"
        );
    }

    // --- runtime model / thinking-budget accessors ---

    #[test]
    fn set_model_updates_the_active_model() {
        init_crypto();
        let c = AnthropicClient::new("http://x", "k", "old");
        c.set_model("new".into());
        assert_eq!(*c.model.read().unwrap(), "new");
    }

    #[tokio::test]
    async fn context_window_fetches_caches_and_resets_on_model_switch() {
        init_crypto();
        let server = httpmock::MockServer::start_async().await;
        let m1 = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET).path("/v1/models/m1");
                then.status(200).json_body(json!({
                    "id": "m1", "max_input_tokens": 1_000_000, "max_tokens": 128_000
                }));
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET).path("/v1/models/m2");
                then.status(200).json_body(json!({
                    "id": "m2", "max_input_tokens": 200_000, "max_tokens": 64_000
                }));
            })
            .await;
        let c = AnthropicClient::new(&server.base_url(), "k", "m1");
        assert_eq!(c.context_window().await, Some(1_000_000));
        assert_eq!(c.context_window().await, Some(1_000_000));
        m1.assert_calls_async(1).await; // second call served from cache
        c.set_model("m2".into()); // switch invalidates the cache
        assert_eq!(c.context_window().await, Some(200_000));
    }

    #[test]
    fn thinking_budget_round_trips() {
        init_crypto();
        let c = AnthropicClient::new("http://x", "k", "m");
        assert_eq!(c.thinking_budget(), None); // 0 -> None
        c.set_thinking_budget(Some(2048));
        assert_eq!(c.thinking_budget(), Some(2048));
        c.set_thinking_budget(None);
        assert_eq!(c.thinking_budget(), None);
    }

    #[test]
    fn quota_is_terminal() {
        // z.ai 5-hour usage cap (the exact shape we observed)
        let ev = json!({"type":"error","error":{"type":"rate_limit_error","code":"1308","message":"Usage limit reached for 5 hour"}});
        assert!(!stream_error_retryable(&ev));
    }

    #[test]
    fn overload_and_network_are_retryable() {
        assert!(stream_error_retryable(
            &json!({"type":"error","error":{"type":"overloaded_error","message":"overloaded"}})
        ));
        assert!(stream_error_retryable(
            &json!({"type":"error","error":{"code":"1234","message":"Network error, please try again"}})
        ));
    }

    #[test]
    fn auth_is_terminal() {
        let ev =
            json!({"type":"error","error":{"type":"authentication_error","message":"invalid key"}});
        assert!(!stream_error_retryable(&ev));
    }

    #[test]
    fn stream_error_each_terminal_branch_independent() {
        // code 1308 is terminal even when the type looks transient (kills the
        // first `||` -> `&&` in the terminal check).
        assert!(!stream_error_retryable(
            &json!({"type":"error","error":{"type":"overloaded_error","code":"1308","message":"x"}})
        ));
        // "usage limit" text alone is terminal even when the type looks transient
        // (kills the second `||` -> `&&`).
        assert!(!stream_error_retryable(
            &json!({"type":"error","error":{"type":"api_error","message":"usage limit reached"}})
        ));
    }

    #[test]
    fn stream_error_network_text_alone_is_retryable() {
        // "network error" text alone (no code 1234) must retry — pins the
        // `|| msg.contains("network error")` against a `&&` mutation.
        assert!(stream_error_retryable(
            &json!({"type":"error","error":{"type":"server_error","message":"network error occurred"}})
        ));
    }
}
