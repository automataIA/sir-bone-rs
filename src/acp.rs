//! ACP (Agent Client Protocol) server — makes sirbone a first-class external
//! agent in Zed's Agent panel. Speaks JSON-RPC 2.0 over stdio (stdout = wire,
//! stderr = logs). This is the ACP-framed sibling of the ad-hoc NDJSON bridge in
//! `main.rs` (`spawn_stdio_prompt_bridge`/`stream_emit`), reusing the same core
//! seam: `crate::agent::run` streams `AgentEvent`s and asks for tool permission
//! via a `ConfirmBridge`.
//!
//! Implements the Agent side of ACP v1: `initialize`, `session/new`,
//! `session/load`, `session/prompt`, `session/cancel`. Tool permission is bridged
//! to the Client's `session/request_permission`. Agent output streams as
//! `session/update` notifications. Filesystem is touched directly by sirbone's own
//! tools (native paths), so the optional `fs/*` and `terminal/*` client methods are
//! not used — this fits the Zed-remote-into-WSL topology where the agent runs in
//! the same environment as the project.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex,
    },
};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{
        AgentContext, ConfirmBridge, LlmClient, Prompt, PromptAnswer, PromptKind, PromptReply,
    },
    session::{self, SessionEntry},
    tools::ToolRegistry,
    types::{AgentEvent, ContentBlock, Message, Role},
};

/// ACP protocol MAJOR version this agent speaks.
const PROTOCOL_VERSION: i64 = 1;

/// Immutable, per-process run dependencies shared into every session/prompt.
struct Deps {
    model: String,
    client: Arc<dyn LlmClient>,
    images_ok: bool,
    system_prompt: String,
    tools: ToolRegistry,
}

/// In-memory state for one session. `sessionId` is the session file's absolute
/// path (opaque to the client, reconstructable across restarts).
struct Session {
    path: PathBuf,
    messages: Vec<Message>,
    cancel: CancellationToken,
}

/// The JSON-RPC connection: a serialized stdout writer plus correlation state for
/// outgoing requests (their responses arrive interleaved on stdin).
struct Conn {
    out: mpsc::UnboundedSender<String>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
}

impl Conn {
    fn send(&self, v: &Value) {
        let _ = self.out.send(v.to_string());
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn respond(&self, id: Value, result: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn respond_err(&self, id: Value, code: i64, message: &str) {
        self.send(
            &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        );
    }

    /// Issue an outgoing request and await the client's response `result` (or an
    /// error if the connection dropped). Registers a oneshot keyed by our id; the
    /// dispatch loop routes the matching response back.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        crate::types::lock_or_recover(&self.pending).insert(id, tx);
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let resp = rx.await?;
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// Serve the ACP protocol on stdin/stdout until the client closes the connection.
pub async fn serve(
    model: String,
    client: Arc<dyn LlmClient>,
    images_ok: bool,
    system_prompt: String,
    tools: ToolRegistry,
) -> Result<()> {
    // Serialized stdout writer: one task drains the queue so JSON-RPC lines never
    // interleave, flushing each so the client sees updates as they happen.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });

    let conn = Arc::new(Conn {
        out: out_tx,
        next_id: AtomicI64::new(1),
        pending: Mutex::new(HashMap::new()),
    });
    let deps = Arc::new(Deps {
        model,
        client,
        images_ok,
        system_prompt,
        tools,
    });
    let sessions: Arc<Mutex<HashMap<String, Session>>> = Arc::new(Mutex::new(HashMap::new()));

    // Blocking stdin reader → async channel (stdin is a blocking file descriptor).
    let (line_tx, mut line_rx) = mpsc::channel::<String>(64);
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::stdin().lock().lines() {
            let Ok(l) = line else { break };
            if line_tx.blocking_send(l).is_err() {
                break;
            }
        }
    });

    while let Some(line) = line_rx.recv().await {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if v.get("method").is_some() {
            dispatch(&conn, &deps, &sessions, v);
        } else if let Some(id) = v.get("id").and_then(Value::as_i64) {
            // Response to one of our outgoing requests (e.g. request_permission).
            if let Some(tx) = crate::types::lock_or_recover(&conn.pending).remove(&id) {
                let _ = tx.send(v);
            }
        }
    }
    Ok(())
}

/// Route one incoming request/notification. Quick methods respond inline; the
/// long-running `session/prompt` is spawned so the dispatch loop keeps flowing
/// (it must route the permission responses the running turn awaits).
fn dispatch(
    conn: &Arc<Conn>,
    deps: &Arc<Deps>,
    sessions: &Arc<Mutex<HashMap<String, Session>>>,
    v: Value,
) {
    let method = v.get("method").and_then(Value::as_str).unwrap_or_default();
    let id = v.get("id").cloned();
    let params = v.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            if let Some(id) = id {
                conn.respond(
                    id,
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "agentInfo": { "name": "sirbone", "version": env!("CARGO_PKG_VERSION") },
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": { "image": deps.images_ok }
                        }
                    }),
                );
            }
        }
        "authenticate" => {
            if let Some(id) = id {
                conn.respond(id, json!({}));
            }
        }
        "session/new" => {
            if let Some(id) = id {
                let path = session::new_session_path();
                let session_id = path.to_string_lossy().to_string();
                crate::types::lock_or_recover(sessions).insert(
                    session_id.clone(),
                    Session {
                        path,
                        messages: Vec::new(),
                        cancel: CancellationToken::new(),
                    },
                );
                conn.respond(id, json!({ "sessionId": session_id }));
            }
        }
        "session/load" => {
            let conn = conn.clone();
            let sessions = sessions.clone();
            tokio::spawn(async move {
                let Some(id) = id else { return };
                let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
                    conn.respond_err(id, -32602, "missing sessionId");
                    return;
                };
                let path = PathBuf::from(session_id);
                let messages = match session::load(&path).await {
                    Ok(entries) => session::collapse(entries),
                    Err(e) => {
                        conn.respond_err(id, -32603, &format!("load failed: {e}"));
                        return;
                    }
                };
                for msg in &messages {
                    for update in replay_updates(msg) {
                        conn.notify(
                            "session/update",
                            json!({ "sessionId": session_id, "update": update }),
                        );
                    }
                }
                crate::types::lock_or_recover(&sessions).insert(
                    session_id.to_string(),
                    Session {
                        path,
                        messages,
                        cancel: CancellationToken::new(),
                    },
                );
                conn.respond(id, json!({}));
            });
        }
        "session/prompt" => {
            let conn = conn.clone();
            let deps = deps.clone();
            let sessions = sessions.clone();
            tokio::spawn(async move {
                let Some(id) = id else { return };
                run_prompt(conn, deps, sessions, params, id).await;
            });
        }
        "session/cancel" => {
            if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
                if let Some(s) = crate::types::lock_or_recover(sessions).get(session_id) {
                    s.cancel.cancel();
                }
            }
        }
        _ => {
            if let Some(id) = id {
                conn.respond_err(id, -32601, "method not found");
            }
        }
    }
}

/// Run one prompt turn: assemble the user message, drive `crate::agent::run` with
/// events bridged to `session/update` and permission prompts bridged to
/// `session/request_permission`, persist the tail, and reply with a stop reason.
async fn run_prompt(
    conn: Arc<Conn>,
    deps: Arc<Deps>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    params: Value,
    req_id: Value,
) {
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        conn.respond_err(req_id, -32602, "missing sessionId");
        return;
    };

    // Take the session's transcript out for the turn; install a fresh cancel token.
    let cancel = CancellationToken::new();
    let (mut messages, path) = {
        let mut map = crate::types::lock_or_recover(&sessions);
        let Some(s) = map.get_mut(session_id) else {
            drop(map);
            conn.respond_err(req_id, -32602, "unknown sessionId");
            return;
        };
        s.cancel = cancel.clone();
        (std::mem::take(&mut s.messages), s.path.clone())
    };

    // Assemble and persist the user message from the prompt content blocks.
    let content = prompt_to_content(params.get("prompt"), deps.images_ok);
    let user_msg = Message {
        role: Role::User,
        injected: false,
        content,
    };
    let _ = session::append(&path, &SessionEntry::Message(user_msg.clone())).await;
    // Prefix fingerprint for the run about to start (see `SessionEntry::RequestHeader`).
    let _ =
        session::append_request_header(&path, &deps.model, Some(&deps.system_prompt), &deps.tools)
            .await;
    messages.push(user_msg);
    let n_before = messages.len();

    // Event pump: translate AgentEvents to session/update notifications.
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let pump = {
        let conn = conn.clone();
        let sid = session_id.to_string();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let Some(update) = event_to_update(&ev) {
                    conn.notify(
                        "session/update",
                        json!({ "sessionId": sid, "update": update }),
                    );
                }
                // A `todo` call additionally streams the ACP-native plan view
                // (Zed renders it as the agent's live step list).
                if let Some(plan) = todo_plan_update(&ev) {
                    conn.notify(
                        "session/update",
                        json!({ "sessionId": sid, "update": plan }),
                    );
                }
            }
        })
    };

    // Permission bridge: each Prompt becomes an outgoing session/request_permission.
    let (ask_tx, mut ask_rx) = mpsc::channel::<Prompt>(1);
    let (reply_tx, reply_rx) = mpsc::channel::<PromptReply>(1);
    let perm = {
        let conn = conn.clone();
        let sid = session_id.to_string();
        tokio::spawn(async move {
            while let Some(p) = ask_rx.recv().await {
                let reply = match &p.kind {
                    // ACP v1 has no multi-question form. Preserve the round's
                    // single model-facing call while adapting it to sequential
                    // native permission requests at this protocol boundary.
                    PromptKind::QuestionRound { questions } => {
                        let mut answers = Vec::with_capacity(questions.len());
                        for question in questions {
                            let single = Prompt {
                                title: question.title.clone(),
                                detail: question.detail.clone(),
                                options: question.options.clone(),
                                allow_free_text: question.allow_free_text,
                                kind: PromptKind::Question,
                            };
                            let answer = match conn
                                .request(
                                    "session/request_permission",
                                    permission_params(&sid, &single),
                                )
                                .await
                            {
                                Ok(result) => outcome_to_reply(&result),
                                Err(_) => PromptReply::default(),
                            };
                            answers.push(PromptAnswer {
                                index: answer.index,
                                text: answer.text,
                            });
                        }
                        PromptReply {
                            answers,
                            ..PromptReply::default()
                        }
                    }
                    _ => match conn
                        .request("session/request_permission", permission_params(&sid, &p))
                        .await
                    {
                        Ok(result) => outcome_to_reply(&result),
                        Err(_) => PromptReply::default(),
                    },
                };
                if reply_tx.send(reply).await.is_err() {
                    break;
                }
            }
        })
    };

    let context_window = deps.client.context_window().await.map(|n| n as usize);
    let mut ctx = AgentContext {
        model: deps.model.clone(),
        system_prompt: Some(deps.system_prompt.clone()),
        messages: std::mem::take(&mut messages),
        tools: deps.tools.clone(),
        client: Arc::clone(&deps.client),
        events: tx,
        cancel: cancel.clone(),
        context_window,
        confirm: Some(ConfirmBridge {
            ask: ask_tx,
            reply: reply_rx,
        }),
        compaction_keep_recent: None,
        permissions: crate::permissions::PermissionConfig::load(),
        snapshots: crate::snapshot::workspace_snapshots(),
        hooks: crate::checks::Hooks::load(),
        oracle: None,
        max_steps: crate::agent::env_max_steps(),
        spend_cap: crate::config::spend_cap(),
        tokens_spent: 0,
        stream_rules: crate::stream_rules::install(deps.client.as_ref()),
        compacted_files: Vec::new(),
        last_request: None,
    };
    let run_result = crate::agent::run(&mut ctx).await;
    messages = std::mem::take(&mut ctx.messages);
    drop(ctx); // drops the event sender → the pump task finishes draining
    let _ = pump.await;
    perm.abort();

    // Persist the new tail (assistant + tool messages) and a run-status marker.
    for msg in messages.get(n_before..).unwrap_or(&[]) {
        let _ = session::append(&path, &SessionEntry::Message(msg.clone())).await;
    }
    let (status, reason) = if cancel.is_cancelled() {
        ("cancelled", Some("cancelled by user".to_string()))
    } else if let Err(e) = &run_result {
        ("error", Some(e.to_string()))
    } else {
        ("done", None)
    };
    let _ = session::append(
        &path,
        &SessionEntry::RunStatus {
            status: status.to_string(),
            reason,
        },
    )
    .await;

    // Return the transcript to the session for the next prompt.
    if let Some(s) = crate::types::lock_or_recover(&sessions).get_mut(session_id) {
        s.messages = messages;
    }

    let stop_reason = if cancel.is_cancelled() {
        "cancelled"
    } else {
        "end_turn"
    };
    conn.respond(req_id, json!({ "stopReason": stop_reason }));
}

/// Map incoming ACP prompt content blocks to sirbone `ContentBlock`s. Text and
/// image are first-class; resource/resource_link are inlined as text references
/// (the file lives in the shared workspace, reachable by the read tool).
fn prompt_to_content(prompt: Option<&Value>, images_ok: bool) -> Vec<ContentBlock> {
    let mut out = Vec::new();
    let Some(blocks) = prompt.and_then(Value::as_array) else {
        return out;
    };
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = b.get("text").and_then(Value::as_str) {
                    out.push(ContentBlock::Text {
                        text: text.to_string(),
                    });
                }
            }
            Some("image") if images_ok => {
                if let (Some(data), Some(mime)) = (
                    b.get("data").and_then(Value::as_str),
                    b.get("mimeType").and_then(Value::as_str),
                ) {
                    out.push(ContentBlock::Image {
                        media_type: mime.to_string(),
                        data: data.to_string(),
                    });
                }
            }
            Some("resource_link") => {
                if let Some(uri) = b.get("uri").and_then(Value::as_str) {
                    out.push(ContentBlock::Text {
                        text: format!("[resource: {uri}]"),
                    });
                }
            }
            Some("resource") => {
                if let Some(text) = b
                    .get("resource")
                    .and_then(|r| r.get("text"))
                    .and_then(Value::as_str)
                {
                    out.push(ContentBlock::Text {
                        text: text.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Translate a streamed `AgentEvent` into a `session/update` inner object. Returns
/// `None` for events with no client-facing representation (mirrors `stream_emit`).
fn event_to_update(ev: &AgentEvent) -> Option<Value> {
    Some(match ev {
        AgentEvent::TextChunk(s) => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": s }
        }),
        AgentEvent::ThinkingChunk(s) => json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": s }
        }),
        AgentEvent::ToolCallStart { id, name, input } => json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": name,
            "kind": tool_kind(name),
            "status": "in_progress",
            "rawInput": input
        }),
        AgentEvent::ToolCallEnd {
            id,
            result,
            is_error,
            ..
        } => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if *is_error { "failed" } else { "completed" },
            "content": [ { "type": "content", "content": { "type": "text", "text": result } } ]
        }),
        AgentEvent::Error(s) => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": format!("⚠ {s}") }
        }),
        AgentEvent::Notice { text, .. } => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": text }
        }),
        _ => return None,
    })
}

/// ACP-native plan view for the `todo` tool: a `sessionUpdate: "plan"` whose
/// entries mirror the call's step list. Emitted alongside the regular
/// `tool_call` update so clients without plan support still see the call.
fn todo_plan_update(ev: &AgentEvent) -> Option<Value> {
    let AgentEvent::ToolCallStart { name, input, .. } = ev else {
        return None;
    };
    if name != "todo" {
        return None;
    }
    let items: Vec<crate::tools::TodoItem> =
        serde_json::from_value(input.get("todos")?.clone()).ok()?;
    let entries: Vec<Value> = items
        .iter()
        .map(|t| {
            json!({
                "content": t.content,
                "priority": "medium",
                "status": serde_json::to_value(t.status).unwrap_or_else(|_| "pending".into()),
            })
        })
        .collect();
    Some(json!({ "sessionUpdate": "plan", "entries": entries }))
}

/// Replay a historical message as `session/update` inner objects (for `session/load`).
fn replay_updates(msg: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => {
                let kind = match msg.role {
                    Role::User => "user_message_chunk",
                    _ => "agent_message_chunk",
                };
                out.push(json!({
                    "sessionUpdate": kind,
                    "content": { "type": "text", "text": text }
                }));
            }
            ContentBlock::ToolUse { id, name, input } => out.push(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": name,
                "kind": tool_kind(name),
                "status": "completed",
                "rawInput": input
            })),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => out.push(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_use_id,
                "status": if *is_error { "failed" } else { "completed" },
                "content": [ { "type": "content", "content": { "type": "text", "text": content } } ]
            })),
            _ => {}
        }
    }
    out
}

/// Map a tool name to an ACP `ToolKind` (drives the client's icon/UI treatment).
fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" => "read",
        "write" | "edit" | "undo" => "edit",
        "bash" | "job_status" => "execute",
        "grep" | "glob" | "code_map" => "search",
        "web_fetch" | "web_search" => "fetch",
        _ => "other",
    }
}

/// Build the `session/request_permission` params from a sirbone `Prompt`. Option
/// index becomes the `optionId`; kinds mirror the three-way permission choice the
/// agent already offers (allow once / allow always / reject).
fn permission_params(session_id: &str, p: &Prompt) -> Value {
    let is_permission = matches!(p.kind, PromptKind::Permission { .. });
    let last = p.options.len().saturating_sub(1);
    let options: Vec<Value> = p
        .options
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let kind = if is_permission {
                match i {
                    0 => "allow_once",
                    1 => "allow_always",
                    _ => "reject_once",
                }
            } else if i == last {
                "reject_once"
            } else {
                "allow_once"
            };
            json!({ "optionId": i.to_string(), "name": name, "kind": kind })
        })
        .collect();
    json!({
        "sessionId": session_id,
        "toolCall": {
            "toolCallId": "permission",
            "title": p.title,
            "kind": "other",
            "status": "pending",
            "rawInput": { "detail": p.detail }
        },
        "options": options
    })
}

/// Map a `session/request_permission` result to a `PromptReply`. `cancelled` (or a
/// missing/garbled outcome) denies; `selected` picks the chosen option index.
fn outcome_to_reply(result: &Value) -> PromptReply {
    let outcome = result.get("outcome");
    match outcome
        .and_then(|o| o.get("outcome"))
        .and_then(Value::as_str)
    {
        Some("selected") => {
            let index = outcome
                .and_then(|o| o.get("optionId"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<usize>().ok());
            PromptReply {
                index,
                text: None,
                ..PromptReply::default()
            }
        }
        _ => PromptReply::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_kind_maps_categories() {
        assert_eq!(tool_kind("read"), "read");
        assert_eq!(tool_kind("edit"), "edit");
        assert_eq!(tool_kind("bash"), "execute");
        assert_eq!(tool_kind("grep"), "search");
        assert_eq!(tool_kind("web_fetch"), "fetch");
        assert_eq!(tool_kind("something_else"), "other");
    }

    #[test]
    fn text_event_becomes_agent_message_chunk() {
        let u = event_to_update(&AgentEvent::TextChunk("hi".into())).unwrap();
        assert_eq!(u["sessionUpdate"], "agent_message_chunk");
        assert_eq!(u["content"]["text"], "hi");
    }

    #[test]
    fn tool_end_error_marks_failed() {
        let u = event_to_update(&AgentEvent::ToolCallEnd {
            id: "t1".into(),
            name: "bash".into(),
            result: "boom".into(),
            is_error: true,
        })
        .unwrap();
        assert_eq!(u["sessionUpdate"], "tool_call_update");
        assert_eq!(u["toolCallId"], "t1");
        assert_eq!(u["status"], "failed");
        assert_eq!(u["content"][0]["content"]["text"], "boom");
    }

    #[test]
    fn turn_start_has_no_update() {
        assert!(event_to_update(&AgentEvent::TurnStart).is_none());
    }

    #[test]
    fn todo_start_also_emits_plan_update() {
        let ev = AgentEvent::ToolCallStart {
            id: "t1".into(),
            name: "todo".into(),
            input: json!({ "todos": [
                { "content": "step one", "status": "completed" },
                { "content": "step two", "status": "in_progress" },
            ]}),
        };
        // Regular tool_call still flows for clients without plan support.
        assert_eq!(event_to_update(&ev).unwrap()["sessionUpdate"], "tool_call");
        let plan = todo_plan_update(&ev).unwrap();
        assert_eq!(plan["sessionUpdate"], "plan");
        assert_eq!(plan["entries"][0]["status"], "completed");
        assert_eq!(plan["entries"][1]["content"], "step two");
    }

    #[test]
    fn non_todo_tools_emit_no_plan() {
        let ev = AgentEvent::ToolCallStart {
            id: "t1".into(),
            name: "bash".into(),
            input: json!({ "command": "ls" }),
        };
        assert!(todo_plan_update(&ev).is_none());
    }

    #[test]
    fn selected_outcome_parses_index() {
        let r = outcome_to_reply(&json!({ "outcome": { "outcome": "selected", "optionId": "1" } }));
        assert_eq!(r.index, Some(1));
    }

    #[test]
    fn cancelled_outcome_denies() {
        let r = outcome_to_reply(&json!({ "outcome": { "outcome": "cancelled" } }));
        assert_eq!(r.index, None);
        assert_eq!(r.text, None);
    }

    #[test]
    fn permission_options_get_allow_kinds() {
        let p = Prompt {
            title: "permission required".into(),
            detail: Some("rm -rf x".into()),
            options: vec!["Allow once".into(), "Allow always".into(), "Deny".into()],
            allow_free_text: false,
            kind: PromptKind::Permission {
                suggested_glob: "rm *".into(),
            },
        };
        let params = permission_params("sid", &p);
        let opts = params["options"].as_array().unwrap();
        assert_eq!(opts[0]["kind"], "allow_once");
        assert_eq!(opts[0]["optionId"], "0");
        assert_eq!(opts[1]["kind"], "allow_always");
        assert_eq!(opts[2]["kind"], "reject_once");
        assert_eq!(params["sessionId"], "sid");
    }

    #[test]
    fn prompt_text_and_gated_image() {
        let prompt = json!([
            { "type": "text", "text": "hello" },
            { "type": "image", "data": "AAAA", "mimeType": "image/png" }
        ]);
        // images_ok=false drops the image block
        let c = prompt_to_content(Some(&prompt), false);
        assert_eq!(c.len(), 1);
        assert!(matches!(&c[0], ContentBlock::Text { text } if text == "hello"));
        // images_ok=true keeps it
        let c = prompt_to_content(Some(&prompt), true);
        assert_eq!(c.len(), 2);
        assert!(
            matches!(&c[1], ContentBlock::Image { media_type, .. } if media_type == "image/png")
        );
    }

    #[test]
    fn replay_maps_roles_and_tools() {
        let user = Message::user("q");
        let up = replay_updates(&user);
        assert_eq!(up[0]["sessionUpdate"], "user_message_chunk");

        let tool = Message {
            role: Role::Assistant,
            injected: false,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: json!({ "path": "a" }),
            }],
        };
        let up = replay_updates(&tool);
        assert_eq!(up[0]["sessionUpdate"], "tool_call");
        assert_eq!(up[0]["kind"], "read");
        assert_eq!(up[0]["status"], "completed");
    }
}
