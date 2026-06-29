//! Context compaction: summarize old messages via the LLM, keep the most recent.

use anyhow::Result;
use tracing::instrument;

use super::{estimate_context_tokens, AgentContext, COMPACTION_KEEP_RECENT_DEFAULT};
use crate::tools::ToolRegistry;
use crate::types::{AgentEvent, ContentBlock, Message};

#[instrument(skip_all, fields(messages = ctx.messages.len()))]
pub async fn compact(ctx: &mut AgentContext) -> Result<()> {
    let keep = ctx
        .compaction_keep_recent
        .unwrap_or(COMPACTION_KEEP_RECENT_DEFAULT)
        .min(ctx.messages.len());
    let to_compact = ctx.messages.len() - keep;

    if to_compact < 2 {
        anyhow::bail!("too few messages to compact");
    }

    // Detect if first message is already a prior compaction summary
    let prior_summary = ctx
        .messages
        .first()
        .and_then(|m| m.content.first())
        .and_then(|b| match b {
            ContentBlock::Text { text } if text.starts_with("[Previous conversation summary") => {
                Some(text.clone())
            }
            _ => None,
        });

    // Build textual representation of messages to compact. Collect the paths of
    // every mutating tool call deterministically so the summary's "Files
    // modified" section is grounded in fact, not the LLM's recollection.
    let mut touched: Vec<String> = Vec::new();
    let mut conversation = String::new();
    if let Some(prev) = &prior_summary {
        conversation.push_str(prev);
        conversation.push_str("\n\n--- New messages since last summary ---\n\n");
    }
    // Skip the prior summary pair (summary at 0 + assistant ack at 1) if present.
    // Only trust the ack-skip when messages[1] is actually an assistant message,
    // so a broken pairing (replay, resume, future code) doesn't silently drop a
    // real turn from the summary.
    let prior = prior_summary.is_some();
    let ack_present = ctx
        .messages
        .get(1)
        .is_some_and(|m| matches!(m.role, crate::types::Role::Assistant));
    let start = match (prior, ack_present) {
        (true, true) => 2.min(to_compact),
        (true, false) => 1,
        _ => 0,
    };
    // Nothing new beyond the prior summary pair — bail rather than ask the model
    // to summarize an empty window (a wasted turn producing a summary-of-summary).
    if to_compact <= start {
        anyhow::bail!("nothing new to compact beyond the prior summary");
    }
    for msg in &ctx.messages[start..to_compact] {
        let role = match msg.role {
            crate::types::Role::System => "System",
            crate::types::Role::User => "User",
            crate::types::Role::Assistant => "Assistant",
            crate::types::Role::Tool => "Tool",
        };
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    conversation.push_str(&format!("[{role}]: {text}\n"));
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    if let Some(p) = ctx.tools.mutation_target(name, input) {
                        // "Files modified" lists WORKSPACE files; exclude the
                        // trust root (e.g. save_skill writes ~/.sirbone/skills).
                        let is_trust = dirs::home_dir()
                            .map(|h| p.starts_with(h.join(".sirbone")))
                            .unwrap_or(false);
                        if !is_trust {
                            touched.push(p.to_string_lossy().into_owned());
                        }
                    }
                    // Keep tool name and path/command, truncate large inputs
                    let brief: String = input.to_string().chars().take(200).collect();
                    conversation.push_str(&format!("[{role} tool_use]: {name}({brief})\n"));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let prefix = if *is_error { "error" } else { "result" };
                    let truncated: String = content.chars().take(300).collect();
                    let display = if content.len() > 300 {
                        format!("{truncated}…")
                    } else {
                        truncated
                    };
                    conversation.push_str(&format!("[Tool {prefix}]: {display}\n"));
                }
                ContentBlock::Thinking { .. } | ContentBlock::Image { .. } => {}
            }
        }
    }

    // Deterministic order for the summary's "Files modified" list.
    touched.sort_unstable();
    touched.dedup();

    // Section list follows what compaction research and Claude Code's own
    // summarizer converged on: preserve user turns and constraints (the agent
    // must not deviate from the trajectory it was on), record failures so they
    // aren't retried, and pin the next step to a verbatim quote to avoid drift.
    let summary_request = [
        Message::system(
            "You are a conversation summarizer. The conversation will be replaced by \
             your summary, so anything you omit is lost. Produce a structured summary:\n\
             1. **Files modified**: list every file path that was edited/written/created.\n\
             2. **Key changes**: what was changed and why (one bullet per change).\n\
             3. **Decisions**: any architectural or design decisions made, with rationale.\n\
             4. **User requests and constraints**: every request the user made, in order, \
             including corrections and changes of direction. Preserve security-relevant \
             instructions (files or data to avoid, operations not to perform, \
             credential/secret handling rules) VERBATIM — they must keep applying.\n\
             5. **Errors and fixes**: errors hit and how they were resolved, especially \
             where the user said to do something differently.\n\
             6. **Failed approaches**: what was tried and didn't work, and why — so it \
             isn't retried.\n\
             7. **Current task state**: what was being worked on and what's left.\n\
             8. **Next step**: only if directly in line with the most recent request; \
             quote the relevant recent message verbatim so the task isn't reinterpreted. \
             If the last task concluded, say so instead.\n\
             Be concise. Omit pleasantries and tool invocation boilerplate.",
        ),
        Message::user(if touched.is_empty() {
            conversation
        } else {
            let files: String = touched.iter().map(|p| format!("- {p}\n")).collect();
            format!(
                "Files modified during this conversation (extracted from tool calls — \
                 use this as the authoritative list for section 1):\n{files}\n{conversation}"
            )
        }),
    ];

    let (dummy_tx, mut dummy_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move { while dummy_rx.recv().await.is_some() {} });

    let result = ctx
        .client
        .run_turn(
            &summary_request.iter().collect::<Vec<_>>(),
            &ToolRegistry::new(),
            &dummy_tx,
            &ctx.cancel,
        )
        .await?;

    let summary = crate::types::extract_text(&result.assistant_message.content);
    if summary.is_empty() {
        anyhow::bail!("empty compaction summary");
    }

    let original = ctx.messages.len();
    let recent: Vec<Message> = ctx.messages.drain(to_compact..).collect();
    ctx.messages.clear();
    ctx.messages.push(Message::user(format!(
        "[Previous conversation summary — {to_compact} messages compacted]\n{summary}"
    )));
    ctx.messages.push(Message::assistant(
        "Understood. I have the context from the summary. Continuing where we left off.",
    ));
    ctx.messages.extend(recent);

    // Let consumers persist the compacted transcript (session JSONL) and fix
    // up their "messages already saved" bookkeeping.
    ctx.events
        .send(AgentEvent::Compacted {
            messages: ctx.messages.clone(),
        })
        .await
        .ok();

    // Emit updated context usage so TUI refreshes the bar
    let context_window = ctx.context_window.unwrap_or(128_000);
    let used_tokens = estimate_context_tokens(&ctx.messages);
    ctx.events
        .send(AgentEvent::ContextUsage {
            used_tokens: used_tokens as u32,
            context_window: context_window as u32,
            cached_tokens: 0,
        })
        .await
        .ok();

    tracing::info!(
        original,
        compacted_to = ctx.messages.len(),
        "context compacted"
    );

    Ok(())
}
