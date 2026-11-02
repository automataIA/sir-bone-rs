//! Native ChatGPT Plus/Pro (Codex) client: OAuth login + Responses API.
//!
//! Credentials are owned by Sir Bone. `sirbone login --codex` runs the same
//! PKCE browser flow as the official Codex CLI (an access path OpenAI
//! endorses for third-party harnesses via its "Codex for OSS" program) and
//! stores the tokens in `~/.sirbone/codex-auth.json`; model turns go straight
//! to the Codex backend (`chatgpt.com/backend-api/codex/responses`) with the
//! `chatgpt-account-id` claim from the login JWT. Unlike the previous design
//! (delegating the turn to `codex exec`), Sir Bone owns the tool loop here, so
//! its permission system, retry policy and compaction all apply.

use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{LlmClient, TurnResult},
    tools::ToolRegistry,
    types::{
        read_or_recover, write_or_recover, AgentEvent, AgentState, ContentBlock, EventTx, Message,
        Role, TokenUsage, ToolCall,
    },
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
/// JWT claim carrying the ChatGPT account id the backend routes the request by.
const ACCOUNT_CLAIM: &str = "https://api.openai.com/auth";
/// Identifies this harness to the backend (the OSS-program equivalent of
/// Codex's own `originator`).
const ORIGINATOR: &str = "sir-bone";
const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
/// The model catalog endpoint gates by client version: older versions get an
/// empty list (verified live 2026-09-17). Bump when OpenAI retires the row.
const CODEX_CLIENT_VERSION: &str = "1.0.0";
/// Offline fallback for `--model auto`: the first catalog model today is
/// `gpt-6-astra`. Note the slugs — plain `gpt-5.6` and `gpt-5.6-codex` are
/// rejected; the 5.6 generation ships as `gpt-5.6-sol/terra/luna`.
const DEFAULT_MODEL: &str = "gpt-5.6-sol";

// ============================================================================
// OAuth credentials
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexAuth {
    pub access: String,
    pub refresh: String,
    /// Unix seconds after which `access` must be refreshed.
    pub expires: u64,
    pub account_id: String,
}

/// Build the browser authorize URL. Returns `(url, code_verifier, state)` —
/// the verifier must reach [`exchange_code`] only after the redirect, the
/// state must round-trip through the callback untouched.
pub fn oauth_login_url() -> (String, String, String) {
    let mut randomness = [0u8; 32];
    getrandom::fill(&mut randomness).expect("OS randomness unavailable");
    let verifier = URL_SAFE_NO_PAD.encode(randomness);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = uuid::Uuid::new_v4().to_string();
    let mut url = url::Url::parse(AUTHORIZE_URL).expect("static authorize URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR);
    (url.to_string(), verifier, state)
}

/// Exchange the redirect's authorization code for tokens (once, at login).
pub async fn exchange_code(code: &str, verifier: &str) -> Result<CodexAuth> {
    let (access, refresh, expires_in) = token_request(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", REDIRECT_URI),
    ])
    .await?;
    auth_from_tokens(access, refresh, expires_in)
}

/// Refresh an expired access token.
pub async fn refresh_auth(refresh_token: &str) -> Result<CodexAuth> {
    let (access, refresh, expires_in) = token_request(&[
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh_token),
    ])
    .await?;
    auth_from_tokens(access, refresh, expires_in)
}

async fn token_request(form: &[(&str, &str)]) -> Result<(String, String, u64)> {
    // urlencoded by hand: reqwest's `.form()` is behind a feature this build
    // doesn't carry, and the `url` crate's serializer already encodes values.
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().copied())
        .finish();
    let response = crate::ai::http_client()
        .post(TOKEN_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .with_context(|| format!("unreadable Codex token response (HTTP {status})"))?;
    if !status.is_success() {
        bail!("Codex token request failed (HTTP {status}): {body}");
    }
    let access = body["access_token"]
        .as_str()
        .context("Codex token response missing access_token")?;
    let refresh = body["refresh_token"]
        .as_str()
        .context("Codex token response missing refresh_token")?;
    let expires_in = body["expires_in"].as_u64().context("Codex token response missing expires_in")?;
    Ok((access.to_string(), refresh.to_string(), expires_in))
}

fn auth_from_tokens(access: String, refresh: String, expires_in: u64) -> Result<CodexAuth> {
    Ok(CodexAuth {
        account_id: account_id_from_jwt(&access)?,
        access,
        refresh,
        expires: now_secs().saturating_add(expires_in),
    })
}

/// The `chatgpt_account_id` the backend requires as a request header.
fn account_id_from_jwt(token: &str) -> Result<String> {
    let payload = token.split('.').nth(1).context("Codex access token is not a JWT")?;
    let decoded = URL_SAFE
        .decode(payload)
        .or_else(|_| URL_SAFE_NO_PAD.decode(payload))
        .or_else(|_| STANDARD.decode(payload))
        .context("cannot decode Codex access token payload")?;
    let value: Value = serde_json::from_slice(&decoded)?;
    value[ACCOUNT_CLAIM]["chatgpt_account_id"]
        .as_str()
        .map(str::to_string)
        .context("Codex access token carries no chatgpt_account_id")
}

/// Persist credentials with owner-only permissions.
pub fn save_auth(auth: &CodexAuth) -> Result<()> {
    let path = crate::config::codex_auth_path().context("HOME is not set")?;
    crate::config::write_json_atomic(&path, &serde_json::to_value(auth)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn load_auth() -> Result<CodexAuth> {
    let path = crate::config::codex_auth_path().context("HOME is not set")?;
    let bytes = std::fs::read(&path).with_context(|| {
        format!("no ChatGPT login at {} — run `sirbone login --codex`", path.display())
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Expired with a minute of headroom for the refresh round-trip itself.
fn is_expired(auth: &CodexAuth, now: u64) -> bool {
    auth.expires <= now.saturating_add(60)
}

/// Extract `code` (after checking `state`) from a pasted callback URL; a bare
/// authorization code pasted without the redirect wrapper is accepted as-is.
pub fn code_from_pasted_url(input: &str, state: &str) -> Result<String> {
    let input = input.trim();
    let query = input.split('?').nth(1).unwrap_or(input);
    let (mut code, mut state_ok) = (None, false);
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "code" => code = Some(value.to_string()),
            "state" => state_ok = value == state,
            _ => {}
        }
    }
    match (code, state_ok) {
        (Some(code), true) => Ok(code),
        (Some(_), false) => bail!("callback state mismatch — start the login again"),
        (None, false) if !input.is_empty() && !input.contains('=') => Ok(input.to_string()),
        (None, _) => bail!("no authorization code in the pasted URL"),
    }
}

/// One-shot local listener on the fixed redirect port. Returns the
/// authorization code once the browser bounces off auth.openai.com.
pub fn wait_for_callback(state: &str) -> Result<String> {
    use std::io::Write as _;
    let listener =
        std::net::TcpListener::bind("127.0.0.1:1455").context("cannot listen on localhost:1455")?;
    let (mut stream, _) = listener.accept().context("callback connection failed")?;
    let mut buf = [0u8; 4096];
    let read = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
    // "GET /auth/callback?code=…&state=… HTTP/1.1"
    let target = String::from_utf8_lossy(&buf[..read]);
    let target = target.split_whitespace().nth(1).unwrap_or_default();
    let code = code_from_pasted_url(target, state);
    let (head, page) = match &code {
        Ok(_) => (
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n",
            "<html><body><h3>Sir Bone: ChatGPT login complete — you can close this window.</h3></body></html>",
        ),
        Err(_) => (
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n",
            "<html><body><h3>Sir Bone: login callback rejected.</h3></body></html>",
        ),
    };
    let _ = stream.write_all(format!("{head}{page}").as_bytes());
    code
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ============================================================================
// Request mapping
// ============================================================================

/// Map the transcript onto the Responses API: the (first) system message
/// becomes `instructions`, everything else becomes typed input items. Sir-bone
/// notes travel as `Role::User` (`Message::injected`), so `instructions` stays
/// the authoritative system prompt; `Thinking` and `Image` blocks are dropped
/// (the backend keeps its own encrypted reasoning; images are unsupported here).
fn to_responses_input(messages: &[&Message]) -> (String, Vec<Value>) {
    let mut instructions = String::new();
    let mut input = Vec::new();
    for message in messages {
        match message.role {
            Role::System => {
                for block in &message.content {
                    if let ContentBlock::Text { text } = block {
                        if !instructions.is_empty() {
                            instructions.push('\n');
                        }
                        instructions.push_str(text);
                    }
                }
            }
            Role::User | Role::Assistant | Role::Tool => {
                let role = if message.role == Role::Assistant { "assistant" } else { "user" };
                let text_type = if message.role == Role::Assistant { "output_text" } else { "input_text" };
                let mut text = String::new();
                // Flush accumulated prose before a tool block so wire order
                // mirrors transcript order (speak, then call).
                let flush = |text: &mut String, input: &mut Vec<Value>| {
                    if !text.is_empty() {
                        input.push(json!({
                            "type": "message",
                            "role": role,
                            "content": [{"type": text_type, "text": text}],
                        }));
                        text.clear();
                    }
                };
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: t } | ContentBlock::Thinking { thinking: t } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                        ContentBlock::ToolUse { id, name, input: args } => {
                            flush(&mut text, &mut input);
                            input.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": args.to_string(),
                            }));
                        }
                        ContentBlock::ToolResult { tool_use_id, content, .. } => {
                            flush(&mut text, &mut input);
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": content,
                            }));
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                flush(&mut text, &mut input);
            }
        }
    }
    (instructions, input)
}

fn codex_tools(registry: &ToolRegistry) -> Vec<Value> {
    registry
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name(),
                "description": t.description(),
                "parameters": t.schema(),
            })
        })
        .collect()
}

/// `SIRBONE_CODEX_REASONING` maps onto the Responses API reasoning effort.
fn reasoning_effort() -> Option<String> {
    std::env::var("SIRBONE_CODEX_REASONING")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ============================================================================
// Model catalog
// ============================================================================

/// Load credentials from disk, refreshing them first when (nearly) expired.
async fn fresh_auth() -> Result<CodexAuth> {
    let mut auth = load_auth()?;
    if is_expired(&auth, now_secs()) {
        auth = refresh_auth(&auth.refresh).await?;
        save_auth(&auth)?;
    }
    Ok(auth)
}

/// Usable chat models from the live catalog, in the backend's own order
/// (newest flagship first). `codex-auto-review` is a review-only model, not a
/// conversation partner, and is filtered out.
async fn fetch_models(base_url: &str, auth: &CodexAuth) -> Result<Vec<Value>> {
    let url = format!("{base_url}/codex/models?client_version={CODEX_CLIENT_VERSION}");
    let response = crate::ai::http_client()
        .get(&url)
        .bearer_auth(&auth.access)
        .header("chatgpt-account-id", &auth.account_id)
        .header("originator", ORIGINATOR)
        .send()
        .await?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .with_context(|| format!("unreadable Codex model catalog (HTTP {status})"))?;
    if !status.is_success() {
        bail!("Codex model catalog failed (HTTP {status}): {body}");
    }
    Ok(body["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter(|m| m["slug"].as_str() != Some("codex-auto-review"))
                .cloned()
                .collect()
        })
        .unwrap_or_default())
}

/// What `--model auto` resolves to: the newest catalog model, or the
/// hardcoded fallback when the catalog is unreachable.
async fn resolve_default_model(base_url: &str, auth: &CodexAuth) -> String {
    match fetch_models(base_url, auth).await {
        Ok(models) => models
            .first()
            .and_then(|m| m["slug"].as_str())
            .unwrap_or(DEFAULT_MODEL)
            .to_string(),
        Err(_) => DEFAULT_MODEL.to_string(),
    }
}

// ============================================================================
// Stream accumulation
// ============================================================================

/// Folds one SSE `data:` payload into the turn state. Returns text to stream to
/// the user, if the event carried any. Kept pure for tests.
#[derive(Default)]
struct TurnAcc {
    answer: String,
    usage: TokenUsage,
    calls: Vec<ToolCall>,
    /// item id → (call id, tool name, arguments buffer)
    pending: HashMap<String, (String, String, String)>,
    finished: bool,
}

impl TurnAcc {
    fn apply(&mut self, data: &str) -> std::result::Result<Option<String>, String> {
        let event: Value = serde_json::from_str(data).map_err(|e| format!("bad SSE event: {e}"))?;
        match event["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                let text = event["delta"].as_str().unwrap_or_default();
                if text.is_empty() {
                    return Ok(None);
                }
                self.answer.push_str(text);
                Ok(Some(text.to_string()))
            }
            "response.output_item.added" if event["item"]["type"] == "function_call" => {
                let item = &event["item"];
                self.pending.insert(
                    item["id"].as_str().unwrap_or_default().to_string(),
                    (
                        item["call_id"].as_str().unwrap_or_default().to_string(),
                        item["name"].as_str().unwrap_or_default().to_string(),
                        String::new(),
                    ),
                );
                Ok(None)
            }
            "response.function_call_arguments.delta" => {
                if let Some((.., args)) = self
                    .pending
                    .get_mut(event["item_id"].as_str().unwrap_or_default())
                {
                    args.push_str(event["delta"].as_str().unwrap_or_default());
                }
                Ok(None)
            }
            "response.output_item.done" if event["item"]["type"] == "function_call" => {
                let item = &event["item"];
                let item_id = item["id"].as_str().unwrap_or_default().to_string();
                let (call_id, name, buffered) = self
                    .pending
                    .remove(&item_id)
                    .unwrap_or((item_id, String::new(), String::new()));
                let arguments = if buffered.is_empty() {
                    item["arguments"].as_str().unwrap_or("{}")
                } else {
                    &buffered
                };
                self.calls.push(ToolCall {
                    id: if call_id.is_empty() { "codex-tool".into() } else { call_id },
                    name: if name.is_empty() {
                        item["name"].as_str().unwrap_or_default().to_string()
                    } else {
                        name
                    },
                    arguments: serde_json::from_str(arguments).unwrap_or(json!({})),
                });
                Ok(None)
            }
            "response.completed" | "response.incomplete" => {
                let usage = &event["response"]["usage"];
                self.usage.input = crate::ai::json_u64(&usage["input_tokens"]).unwrap_or(0) as u32;
                self.usage.output = crate::ai::json_u64(&usage["output_tokens"]).unwrap_or(0) as u32;
                self.finished = true;
                Ok(None)
            }
            "response.failed" => {
                let error = &event["response"]["error"];
                Err(format!(
                    "Codex response failed: {}",
                    error["message"].as_str().unwrap_or_else(|| error["code"].as_str().unwrap_or("unknown"))
                ))
            }
            "error" => Err(format!(
                "Codex stream error: {}",
                event["message"].as_str().or_else(|| event["error"]["message"].as_str()).unwrap_or("unknown")
            )),
            _ => Ok(None),
        }
    }
}

// ============================================================================
// Client
// ============================================================================

pub struct CodexClient {
    model: RwLock<String>,
    /// `context_window()` result per model id — the catalog GET must not ride
    /// along with every turn.
    context_cache: RwLock<Option<(String, u32)>>,
    /// Test seams: production always loads auth from disk and uses
    /// [`DEFAULT_BASE_URL`].
    #[cfg(test)]
    auth: Option<CodexAuth>,
    #[cfg(test)]
    base_url: String,
}

impl CodexClient {
    pub fn new(model: &str) -> Self {
        Self {
            model: RwLock::new(model.to_string()),
            context_cache: RwLock::new(None),
            #[cfg(test)]
            auth: None,
            #[cfg(test)]
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }
}

#[async_trait]
impl LlmClient for CodexClient {
    async fn run_turn(
        &self,
        messages: &[&Message],
        registry: &ToolRegistry,
        events: &EventTx,
        cancel: &CancellationToken,
    ) -> Result<TurnResult> {
        let requested = read_or_recover(&self.model).clone();
        #[cfg(test)]
        let mut auth = self.auth.clone().context("test auth not injected")?;
        #[cfg(not(test))]
        let mut auth = fresh_auth().await?;
        #[cfg(test)]
        let base_url = self.base_url.clone();
        #[cfg(not(test))]
        let base_url = DEFAULT_BASE_URL.to_string();
        let model = if requested == "auto" {
            resolve_default_model(&base_url, &auth).await
        } else {
            requested
        };

        let (instructions, input) = to_responses_input(messages);
        let mut body = json!({
            "model": model,
            "instructions": if instructions.is_empty() { "You are a helpful assistant.".to_string() } else { instructions },
            "input": input,
            "tools": codex_tools(registry),
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "store": false,
            "stream": true,
        });
        if let Some(effort) = reasoning_effort() {
            body["reasoning"] = json!({ "effort": effort });
        }
        let url = format!("{base_url}/codex/responses");

        // Open with retry; one token refresh is granted on a rejected access
        // token (server-side revocation the expiry check can't see).
        let mut refreshed = false;
        let mut attempt = 0u32;
        let response = loop {
            if cancel.is_cancelled() {
                events.send(AgentEvent::Cancelled).await.ok();
                return Ok(empty_turn());
            }
            attempt += 1;
            let sent = crate::ai::http_client()
                .post(&url)
                .bearer_auth(&auth.access)
                .header("chatgpt-account-id", &auth.account_id)
                .header("originator", ORIGINATOR)
                .header("accept", "text/event-stream")
                .json(&body)
                .send();
            let opened = tokio::select! {
                r = sent => r,
                _ = cancel.cancelled() => {
                    events.send(AgentEvent::Cancelled).await.ok();
                    return Ok(empty_turn());
                }
            };
            match opened {
                Ok(response) if response.status().is_success() => break response,
                Ok(response) => {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                        refreshed = true;
                        auth = refresh_auth(&auth.refresh).await?;
                        save_auth(&auth)?;
                        continue;
                    }
                    if attempt < crate::ai::MAX_ATTEMPTS && (status.as_u16() == 429 || status.is_server_error()) {
                        backoff(attempt, cancel, &format!("codex HTTP {status}")).await;
                        continue;
                    }
                    bail!(
                        "Codex backend error (HTTP {status}): {}",
                        crate::ai::redact_secrets(&text)
                    );
                }
                Err(error) if attempt < crate::ai::MAX_ATTEMPTS && retryable_network(&error) => {
                    backoff(attempt, cancel, &format!("codex connection: {error}")).await;
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        };

        events.send(AgentEvent::TurnStart).await.ok();
        let mut acc = TurnAcc::default();
        let mut buffer = String::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = tokio::select! {
            chunk = stream.next() => chunk,
            _ = cancel.cancelled() => {
                // Dropping the stream closes the connection mid-flight.
                events.send(AgentEvent::Cancelled).await.ok();
                return Ok(empty_turn());
            }
        } {
            buffer.push_str(&String::from_utf8_lossy(
                chunk.map_err(|e| anyhow::anyhow!("reading Codex stream: {e}"))?.as_ref(),
            ));
            while let Some(end) = buffer.find('\n') {
                let line: String = buffer.drain(..=end).collect();
                let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                    continue;
                };
                if data.is_empty() {
                    continue;
                }
                match acc.apply(data) {
                    Ok(Some(text)) => {
                        events.send(AgentEvent::TextChunk(text)).await.ok();
                    }
                    Ok(None) => {}
                    Err(message) => bail!("{message}"),
                }
            }
            if acc.finished {
                break;
            }
        }

        // Every text delta was already streamed as it arrived.
        events.send(AgentEvent::TurnEnd).await.ok();
        // The transcript gets `result.assistant_message` verbatim, so the
        // tool calls MUST ride in it: without the matching function_call
        // items the backend rejects the follow-up's function_call_output.
        let mut content = Vec::new();
        if !acc.answer.is_empty() {
            content.push(ContentBlock::Text {
                text: std::mem::take(&mut acc.answer),
            });
        }
        let state = if acc.calls.is_empty() {
            AgentState::Done
        } else {
            content.extend(acc.calls.iter().map(|tc| ContentBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.arguments.clone(),
            }));
            AgentState::ToolCalling(std::mem::take(&mut acc.calls))
        };
        Ok(TurnResult {
            assistant_message: Message {
                role: Role::Assistant,
                injected: false,
                content,
            },
            state,
            usage: acc.usage,
            tripped_rule: None,
        })
    }

    fn set_model(&self, model: String) {
        *write_or_recover(&self.model) = model;
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let auth = fresh_auth().await?;
        Ok(fetch_models(DEFAULT_BASE_URL, &auth)
            .await?
            .iter()
            .filter_map(|m| m["slug"].as_str())
            .map(str::to_string)
            .collect())
    }

    async fn context_window(&self) -> Option<u32> {
        let model = read_or_recover(&self.model).clone();
        if let Some((cached_model, window)) = read_or_recover(&self.context_cache).as_ref() {
            if cached_model == &model {
                return Some(*window);
            }
        }
        let auth = fresh_auth().await.ok()?;
        let window = fetch_models(DEFAULT_BASE_URL, &auth)
            .await
            .ok()?
            .iter()
            .find(|m| m["slug"].as_str() == Some(model.as_str()))
            .and_then(|m| crate::ai::json_u64(&m["context_window"]))
            .map(|v| v.min(u32::MAX as u64) as u32)?;
        *write_or_recover(&self.context_cache) = Some((model, window));
        Some(window)
    }
}

fn retryable_network(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout()
}

/// Cancellable exponential backoff with the shared cap; announces on stderr
/// markers like the other clients' retry paths.
async fn backoff(attempt: u32, cancel: &CancellationToken, reason: &str) {
    let secs = crate::ai::backoff_secs(attempt);
    if crate::ai::stderr_markers_enabled() {
        eprintln!("retry: {reason} — backing off {secs}s (attempt {attempt})");
    }
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
        _ = cancel.cancelled() => {}
    }
}

fn empty_turn() -> TurnResult {
    TurnResult {
        assistant_message: Message::assistant(String::new()),
        state: AgentState::Done,
        usage: TokenUsage::default(),
        tripped_rule: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::WebSearchTool;

    fn user(text: &str) -> Message {
        Message::user(text)
    }

    #[test]
    fn mapping_covers_system_tools_and_results() {
        let system = Message {
            role: Role::System,
            content: vec![ContentBlock::Text { text: "be terse".into() }],
            injected: false,
        };
        let mut user_msg = user("list files");
        user_msg
            .content
            .push(ContentBlock::ToolResult { tool_use_id: "c1".into(), content: "a.rs".into(), is_error: false });
        let mut assistant = Message::assistant("running ls");
        assistant.content.push(ContentBlock::ToolUse {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({"command": "ls"}),
        });

        let (instructions, input) = to_responses_input(&[&system, &assistant, &user_msg]);

        assert_eq!(instructions, "be terse");
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[0]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "c1");
        assert_eq!(input[1]["arguments"], r#"{"command":"ls"}"#);
        assert_eq!(input[2]["content"][0]["type"], "input_text");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "c1");
    }

    #[test]
    fn events_fold_into_text_calls_and_usage() {
        let mut acc = TurnAcc::default();
        acc.apply(r#"{"type":"response.output_text.delta","delta":"Hel"}"#).unwrap();
        acc.apply(r#"{"type":"response.output_text.delta","delta":"lo"}"#).unwrap();
        acc.apply(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc1","call_id":"call1","name":"bash"}}"#,
        )
        .unwrap();
        acc.apply(r#"{"type":"response.function_call_arguments.delta","item_id":"fc1","delta":"{\"cmd\":"}"#)
            .unwrap();
        acc.apply(r#"{"type":"response.function_call_arguments.delta","item_id":"fc1","delta":"\"ls\"}"}"#)
            .unwrap();
        acc.apply(
            r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc1","call_id":"call1","name":"bash"}}"#,
        )
        .unwrap();
        acc.apply(
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":3}}}"#,
        )
        .unwrap();

        assert_eq!(acc.answer, "Hello");
        assert_eq!(acc.usage.input, 10);
        assert_eq!(acc.usage.output, 3);
        assert_eq!(acc.calls.len(), 1);
        assert_eq!(acc.calls[0].id, "call1");
        assert_eq!(acc.calls[0].name, "bash");
        assert_eq!(acc.calls[0].arguments, json!({"cmd": "ls"}));
    }

    #[test]
    fn failed_response_is_an_error() {
        let mut acc = TurnAcc::default();
        let error = acc
            .apply(r#"{"type":"response.failed","response":{"error":{"message":"usage limit (plus plan)"}}}"#)
            .unwrap_err();
        assert!(error.contains("usage limit"));
    }

    #[test]
    fn pasted_urls_yield_the_code_and_check_state() {
        let (.., state) = oauth_login_url();
        let url = format!("http://localhost:1455/auth/callback?code=abc123&state={state}");
        assert_eq!(code_from_pasted_url(&url, &state).unwrap(), "abc123");
        assert_eq!(code_from_pasted_url("xyz", &state).unwrap(), "xyz");
        assert!(code_from_pasted_url(&url, "other").is_err());
        assert!(code_from_pasted_url("http://localhost:1455/auth/callback?state=x", state.as_str()).is_err());
    }

    #[test]
    fn expiry_leaves_a_refresh_minute() {
        let auth = CodexAuth {
            access: "a".into(),
            refresh: "r".into(),
            expires: 1_000,
            account_id: "acc".into(),
        };
        assert!(!is_expired(&auth, 900));
        assert!(is_expired(&auth, 990));
        assert!(is_expired(&auth, 1_001));
    }

    #[test]
    fn account_id_comes_from_the_jwt_claim() {
        // header.payload.signature with payload = {"https://api.openai.com/auth":
        // {"chatgpt_account_id": "acc-1"}}
        let payload = URL_SAFE.encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-1"}}"#);
        let token = format!("{}.{}.{}",
            URL_SAFE.encode(r#"{"alg":"none"}"#),
            payload,
            URL_SAFE.encode("sig"));
        assert_eq!(account_id_from_jwt(&token).unwrap(), "acc-1");
        assert!(account_id_from_jwt("not-a-jwt").is_err());
    }

    /// Wire-level turn against a mock backend: text streams, a tool call
    /// completes, usage lands — the full `run_turn` contract.
    #[tokio::test]
    async fn streamed_turn_against_mock_backend() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/codex/responses");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n\
                     data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc1\",\"call_id\":\"call1\",\"name\":\"web_search\"}}\n\n\
                     data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc1\",\"delta\":\"{\\\"query\\\":\\\"rs\\\"}\"}\n\n\
                     data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc1\",\"call_id\":\"call1\",\"name\":\"web_search\"}}\n\n\
                     data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\n\n",
                );
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let cancel = CancellationToken::new();
        let msg = user("search rust");
        let mut registry = ToolRegistry::new();
        registry.register(WebSearchTool::default());

        let mut client = CodexClient::new("gpt-5.6-sol");
        client.auth = Some(CodexAuth {
            access: "tok".into(),
            refresh: "ref".into(),
            expires: now_secs() + 3_600,
            account_id: "acc".into(),
        });
        client.base_url = server.base_url();
        let result = client
            .run_turn(&[&msg], &registry, &tx, &cancel)
            .await
            .expect("turn succeeds");
        assert_eq!(result.usage.input, 7);
        assert!(matches!(result.state, AgentState::ToolCalling(_)));
        // The transcript message must carry both the prose and the tool call:
        // the backend rejects a follow-up whose function_call_output has no
        // matching function_call in the history.
        assert_eq!(result.assistant_message.content.len(), 2);
        assert!(matches!(
            result.assistant_message.content[1],
            ContentBlock::ToolUse { ref name, .. } if name == "web_search"
        ));
        let mut saw_text = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, AgentEvent::TextChunk(t) if t == "Hi") {
                saw_text = true;
            }
        }
        assert!(saw_text);
    }
}
