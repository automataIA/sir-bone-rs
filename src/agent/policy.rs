//! Permission decisions and the retry-loop (stuck) detector.

use std::collections::HashMap;

use super::AgentContext;
use crate::permissions::{self, Decision};
use crate::tools::ToolRegistry;
use crate::types::{extract_text, ContentBlock, Message};

/// Identical failing tool calls in a row that count as a retry loop.
pub(crate) const STUCK_THRESHOLD: usize = 3;

/// Retry-loop detector. Pairs each tool call with its result, then measures the
/// trailing run of *identical failing* calls (same tool + same arguments).
/// Returns the tool name only when that run length equals [`STUCK_THRESHOLD`],
/// so the caller nudges exactly once per streak (a longer run won't re-fire; a
/// fresh streak on a different call will).
pub(crate) fn stuck_tool(messages: &[Message]) -> Option<String> {
    // tool_use_id -> (name, args) from assistant ToolUse blocks.
    let mut calls: HashMap<&str, (&str, &serde_json::Value)> = HashMap::new();
    // Chronological (name, args, is_error), ordered by when results came back.
    let mut seq: Vec<(&str, &serde_json::Value, bool)> = Vec::new();
    for m in messages {
        for block in &m.content {
            match block {
                ContentBlock::ToolUse { id, name, input } => {
                    calls.insert(id.as_str(), (name.as_str(), input));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => {
                    if let Some(&(name, args)) = calls.get(tool_use_id.as_str()) {
                        seq.push((name, args, *is_error));
                    }
                }
                _ => {}
            }
        }
    }
    let &(name, args, last_err) = seq.last()?;
    if !last_err {
        return None;
    }
    let run = seq
        .iter()
        .rev()
        .take_while(|&&(n, a, e)| e && n == name && a == args)
        .count();
    (run == STUCK_THRESHOLD).then(|| name.to_string())
}

/// On a detected retry loop, append a strategy-change nudge to the last tool
/// result so the model reads it inline. Riding inside the existing tool_result
/// (rather than a new message) avoids two consecutive user messages, which the
/// Anthropic API rejects.
pub(crate) fn nudge_if_stuck(messages: &mut [Message]) {
    let Some(tool) = stuck_tool(messages) else {
        return;
    };
    let nudge = format!(
        "\n\n[system] You've issued the same `{tool}` call and gotten the same error \
         {STUCK_THRESHOLD} times in a row. Stop repeating it — re-read the error and change \
         approach: different command or arguments, add diagnostics to test your hypothesis, or \
         reconsider the root cause. Do not retry the identical call."
    );
    if let Some(ContentBlock::ToolResult { content, .. }) =
        messages.last_mut().and_then(|m| m.content.last_mut())
    {
        content.push_str(&nudge);
    }
}

/// Decide whether a tool call may run. Static rules (allow/soft-deny globs,
/// destructive patterns) are checked first; anything left undecided with a
/// configured `environment` is sent to the LLM classifier. Returns the
/// decision and an optional rewritten command (`updatedInput`).
pub(crate) async fn decide(
    ctx: &AgentContext,
    tool: &str,
    args: &serde_json::Value,
) -> (Decision, Option<String>) {
    let inner = permissions::tool_inner(tool, args);
    if let Some(d) = ctx.permissions.static_decision(tool, &inner) {
        return (d, None);
    }
    // MCP tools: gate by per-server trust (untrusted => confirm). User
    // allow/soft-deny globs above still take precedence.
    if let Some(d) = ctx.permissions.mcp_decision(tool) {
        return (d, None);
    }
    // Guard the trust root: any tool that writes a file into
    // `~/.sirbone/{system,prompts,skills}` or `config.json` needs confirmation.
    // Uses `DynTool::mutation_target` so EVERY writer is covered (edit/write/sed,
    // save_skill, future MCP file tools) — not a hardcoded `write|edit|sed` list
    // that silently misses the rest. A user `allow` glob above can still opt out.
    if let Some(target) = ctx.tools.mutation_target(tool, args) {
        if let Some(home) = dirs::home_dir() {
            if permissions::is_protected_config_path(&home, &target.to_string_lossy()) {
                return (Decision::Ask, None);
            }
        }
    }
    // Deterministic pre_tool_use gate (Feature B): a user hook can allow or deny
    // by exit code, short-circuiting both the destructive-Ask and — critically —
    // the LLM command classifier below (the one extra turn it would have cost).
    // User allow/soft-deny globs and the trust-root guard above still win.
    match ctx.hooks.pre_tool_use(tool, args).await {
        crate::checks::PreVerdict::Allow => return (Decision::Allow, None),
        crate::checks::PreVerdict::Deny(reason) => return (Decision::Deny(reason), None),
        crate::checks::PreVerdict::Pass => {}
    }
    if tool == "bash" && permissions::is_destructive(&inner) {
        return (Decision::Ask, None);
    }
    let needs_classify = tool == "bash"
        && !ctx.permissions.environment.is_empty()
        && !permissions::is_safe_readonly(&inner);
    if needs_classify {
        return classify(ctx, &inner).await;
    }
    (Decision::Allow, None)
}

/// Ask the LLM to classify a command against the configured environment.
/// Reuses the standard turn path with an empty tool registry; any failure is
/// treated as `Ask` (fail-safe).
async fn classify(ctx: &AgentContext, cmd: &str) -> (Decision, Option<String>) {
    let system = permissions::classifier_system_prompt(&ctx.permissions.environment);
    let messages = [Message::system(system), Message::user(cmd)];
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let empty = ToolRegistry::new();
    let result = ctx
        .client
        .run_turn(&messages.iter().collect::<Vec<_>>(), &empty, &tx, &ctx.cancel)
        .await;
    drop(tx);
    drain.await.ok();
    match result {
        Ok(turn) => {
            let text = extract_text(&turn.assistant_message.content);
            permissions::parse_classification(&text, cmd)
        }
        Err(_) => (Decision::Ask, None),
    }
}
