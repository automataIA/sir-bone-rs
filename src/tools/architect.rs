use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ToolRegistry, TypedTool};
use crate::agent::LlmClient;
use crate::types::{extract_text, ContentBlock, Message, Role};

const SYSTEM: &str = "You are a principal engineer doing an adversarial review of a weaker coding \
    agent. You see its full transcript. Your job is to catch what it's about to get wrong — not to \
    praise it.\n\n\
    First, big-picture: what is the ACTUAL requirement behind the task (not just the literal example \
    in the issue)? What's the root cause, and what must a COMPLETE fix cover?\n\n\
    Then audit adversarially — assume the agent's approach is incomplete or subtly wrong until proven \
    otherwise. Hunt for: edge cases/inputs it ignored (hidden tests won't); the gap between \"my own \
    test passes\" and \"the real defect is fixed\"; unstated requirements, related code paths, \
    regressions; wrong API/behavior assumptions.\n\n\
    Be calibrated, not contrarian: report only real problems, rank by severity, and say plainly if \
    the approach is sound. Don't invent issues. Output a short verdict: key risk(s), the exact \
    change(s) needed, what to verify. Don't write the full solution.";

/// Snapshot of the live conversation, refreshed by the agent loop before tool
/// execution so the `architect` tool can forward it without needing access to
/// `AgentContext`.
pub type Transcript = Arc<Mutex<Vec<Message>>>;

/// A stronger second-opinion model, configured independently from the executor
/// (own provider/key/model) so the cheap executor and the smart advisor can be
/// on different services. `calls` is shared with the registry for per-task reset.
#[derive(Clone)]
pub struct Architect {
    client: Arc<dyn LlmClient>,
    transcript: Transcript,
    calls: Arc<Mutex<u32>>,
    max_calls: u32,
    enabled: Arc<AtomicBool>,
}

impl Architect {
    pub fn new(
        client: Arc<dyn LlmClient>,
        transcript: Transcript,
        calls: Arc<Mutex<u32>>,
        max_calls: u32,
        enabled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            client,
            transcript,
            calls,
            max_calls,
            enabled,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ArchitectInput {
    /// Optional focus — a specific question or what you're unsure about. The
    /// architect sees your full transcript regardless of this field.
    #[serde(default)]
    pub question: String,
}

pub struct ArchitectTool {
    pub arch: Architect,
}

#[async_trait]
impl TypedTool for ArchitectTool {
    type Input = ArchitectInput;

    fn name(&self) -> &'static str {
        "architect"
    }

    fn description(&self) -> &'static str {
        "Consult a stronger architect/reviewer model for a strategic plan or course-correction. \
         It automatically sees your full transcript. Call it BEFORE substantive work (before \
         writing or committing to an approach) and again before declaring a hard task done. \
         Optionally pass `question` to focus it. Give its advice serious weight."
    }

    async fn run(&self, input: ArchitectInput) -> Result<String> {
        if !self.arch.enabled.load(Ordering::Relaxed) {
            return Ok("(architect disabled in settings)".into());
        }
        {
            let mut c = crate::types::lock_or_recover(&self.arch.calls);
            if *c >= self.arch.max_calls {
                return Ok(format!(
                    "(architect unavailable: max {} consults reached for this task)",
                    self.arch.max_calls
                ));
            }
            *c += 1;
        }

        let transcript = render_transcript(&crate::types::lock_or_recover(&self.arch.transcript));
        let focus = if input.question.trim().is_empty() {
            String::new()
        } else {
            format!("Focus on: {}\n\n", input.question.trim())
        };
        let messages = [
            Message::system(SYSTEM),
            Message::user(format!(
                "{focus}Here is the coding agent's transcript so far:\n\n{transcript}\n\n\
                 Give your plan / course-correction now. Keep it under ~120 words."
            )),
        ];

        // Silent sub-call (drain events), no tools, fresh cancel — like compaction.
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = self
            .arch
            .client
            .run_turn(
                &messages.iter().collect::<Vec<_>>(),
                &ToolRegistry::new(),
                &tx,
                &CancellationToken::new(),
            )
            .await;
        drop(tx);
        drain.await.ok();

        match result {
            Ok(turn) => {
                let text = extract_text(&turn.assistant_message.content);
                Ok(if text.trim().is_empty() {
                    "(architect returned no advice)".into()
                } else {
                    text
                })
            }
            Err(e) => Ok(format!("(architect error: {e})")),
        }
    }
}

/// Flatten the conversation to plain text (no structured tool_use/result blocks)
/// so it forwards to any provider without tool-pairing constraints.
fn render_transcript(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        let role = match m.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        for b in &m.content {
            match b {
                ContentBlock::Text { text } => out.push_str(&format!("[{role}]: {text}\n")),
                ContentBlock::ToolUse { name, input, .. } => {
                    let brief: String = input.to_string().chars().take(200).collect();
                    out.push_str(&format!("[{role} tool_use]: {name}({brief})\n"));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let t: String = content.chars().take(400).collect();
                    let kind = if *is_error { "error" } else { "result" };
                    out.push_str(&format!("[Tool {kind}]: {t}\n"));
                }
                ContentBlock::Thinking { .. } | ContentBlock::Image { .. } => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TurnResult;
    use crate::types::EventTx;

    /// Fails if ever invoked — proves the gates short-circuit before any LLM call.
    struct NeverClient;

    #[async_trait]
    impl LlmClient for NeverClient {
        async fn run_turn(
            &self,
            _m: &[&Message],
            _r: &ToolRegistry,
            _e: &EventTx,
            _c: &CancellationToken,
        ) -> Result<TurnResult> {
            panic!("architect must not call the client here");
        }
    }

    fn tool(enabled: bool, calls: u32, max: u32) -> ArchitectTool {
        let arch = Architect::new(
            Arc::new(NeverClient),
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(calls)),
            max,
            Arc::new(AtomicBool::new(enabled)),
        );
        ArchitectTool { arch }
    }

    #[tokio::test]
    async fn disabled_returns_notice_without_calling_client() {
        let out = tool(false, 0, 3)
            .run(ArchitectInput {
                question: String::new(),
            })
            .await
            .unwrap();
        assert!(out.contains("disabled"));
    }

    #[tokio::test]
    async fn cap_reached_returns_notice_without_calling_client() {
        let out = tool(true, 2, 2)
            .run(ArchitectInput {
                question: String::new(),
            })
            .await
            .unwrap();
        assert!(out.contains("max 2 consults"));
    }

    #[test]
    fn render_transcript_flattens_roles() {
        let msgs = vec![Message::user("hi"), Message::assistant("yo")];
        let out = render_transcript(&msgs);
        assert!(out.contains("[User]: hi"));
        assert!(out.contains("[Assistant]: yo"));
    }
}
