//! Permission decisions and the retry-loop (stuck) detector.

use std::collections::HashMap;

use super::AgentContext;
use crate::permissions::{self, Decision, PreAction};
use crate::telemetry;
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

/// Deterministic completion check (opt-in, `SIRBONE_COMPLETION_CHECK`): the run
/// is about to end while the model's own step list still has unfinished items.
///
/// The signal is the model's own declaration, not an interpretation of its prose,
/// so it is language-independent and cannot fire on a run that never planned. It
/// says *which* check failed and leaves the answer to the model — including the
/// honest exit, since "I stopped early" is a valid outcome and forcing work would
/// only trade a silent abort for a fabricated one.
pub(crate) fn unfinished_steps(todos: &[crate::tools::todo::TodoItem]) -> Option<String> {
    use crate::tools::todo::TodoStatus;
    let pending: Vec<&crate::tools::todo::TodoItem> = todos
        .iter()
        .filter(|t| !matches!(t.status, TodoStatus::Completed))
        .collect();
    if pending.is_empty() {
        return None;
    }
    let list: String = pending
        .iter()
        .map(|t| format!("- {}\n", t.content))
        .collect();
    Some(format!(
        "[system] Completion check: this run is ending, but the step list you wrote still has \
         unfinished items:\n{list}Finish them, or — if the work is done, or you decided to stop — \
         update the list with `todo` and state plainly what you did not do. Do not mark a step \
         completed unless it is."
    ))
}

/// Whether a mutated path is a test file, by the naming conventions the
/// ecosystems actually use.
///
/// Deliberately conservative: a directory segment that *is* a test root, or a
/// filename in one of the standard test spellings. It does not guess from
/// substrings — `src/testing/harness.rs` and `latest_run.rs` are source, and
/// counting them would make the signal unreadable in exactly the runs where it
/// matters. Errs toward missing a test file rather than accusing a source file,
/// because the number this feeds is read next to a claim of honest work.
pub(crate) fn is_test_path(path: &std::path::Path) -> bool {
    const TEST_DIRS: [&str; 5] = ["tests", "test", "spec", "specs", "__tests__"];
    if path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .any(|seg| TEST_DIRS.contains(&seg))
    {
        return true;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let stem = name.split('.').next().unwrap_or(name);
    // `test_x.py` / `x_test.go` / `TestX.java`, and `x.test.ts` / `x.spec.ts`.
    stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("Test")
        || name.contains(".test.")
        || name.contains(".spec.")
}

/// After a batch in which a write to a test file landed, append the one fact
/// that keeps the model's own summary honest: a test it wrote or changed cannot
/// be evidence that the implementation is right.
///
/// A fact, not a prohibition — editing tests is ordinary work, and a graded
/// benchmark restores its own test files anyway, so blocking the write would
/// only cost legitimate coverage. What fails is the *inference*: a run that
/// rewrites an assertion to match its implementation and then reports "all
/// tests pass" has verified nothing, and nothing in the transcript said so.
/// Rides inside the last tool result for the same reason as
/// [`nudge_if_stuck`]: two consecutive user messages are rejected by the API.
pub(crate) fn note_test_edits(messages: &mut [Message]) {
    let note = "\n\n[system] A write to a test file landed in this batch. A test you wrote or \
                changed is not evidence that the implementation is correct — it asserts what you \
                already believe. If an existing test contradicted your change, read it as the \
                expected contract (names, signatures, exact output) and reconcile the code with \
                the test, not the test with the code. Name the tests you modified when you \
                report what you did.";
    if let Some(ContentBlock::ToolResult { content, .. }) =
        messages.last_mut().and_then(|m| m.content.last_mut())
    {
        content.push_str(note);
    }
}

/// Decide whether a tool call may run. Static rules (allow/soft-deny globs,
/// destructive patterns) are checked first; anything left undecided with a
/// configured `environment` is sent to the LLM classifier. Returns the
/// decision plus whatever the policy wants done to the call itself.
pub(crate) async fn decide(
    ctx: &AgentContext,
    tool: &str,
    args: &serde_json::Value,
) -> (Decision, PreAction) {
    // Bench-only (`--features bench_bypass`): the entire pass below is skipped,
    // so a benchmark arm can match a harness running with permissions bypassed.
    // Deliberately first — before review-only, globs, the hook and the
    // classifier — because a partial bypass is the worst of both: it still
    // denies some calls, and the denials correlate with the treatment.
    if cfg!(feature = "bench_bypass") {
        telemetry::add(&telemetry::PERMISSION_BYPASSED, 1);
        return (Decision::Allow, PreAction::AsIs);
    }
    let inner = permissions::tool_inner(tool, args);
    // Review-only (`--review-only`): checked before every other rule, including
    // the user's own `allow` globs. The mode is the reason the run exists, so a
    // glob stored for normal work must not be able to unlock a write inside it.
    if ctx.permissions.review_only {
        if let Some(reason) = permissions::review_only_refusal(tool, &inner) {
            return (Decision::Deny(reason), PreAction::AsIs);
        }
        if let Some(target) = ctx.tools.mutation_target(tool, args) {
            return (
                Decision::Deny(format!(
                    "review-only run: `{tool}` would modify {}",
                    target.display()
                )),
                PreAction::AsIs,
            );
        }
    }
    if let Some(d) = ctx.permissions.static_decision(tool, &inner) {
        return (d, PreAction::AsIs);
    }
    // MCP tools: gate by per-server trust (untrusted => confirm). User
    // allow/soft-deny globs above still take precedence.
    if let Some(d) = ctx.permissions.mcp_decision(tool) {
        return (d, PreAction::AsIs);
    }
    // Guard the trust root: any tool that writes a file into
    // `~/.sirbone/{system,prompts,skills}` or `config.json` needs confirmation.
    // Uses `DynTool::mutation_target` so EVERY writer is covered (edit/write/sed,
    // save_skill, future MCP file tools) — not a hardcoded `write|edit|sed` list
    // that silently misses the rest. A user `allow` glob above can still opt out.
    if let Some(target) = ctx.tools.mutation_target(tool, args) {
        if let Some(home) = dirs::home_dir() {
            if permissions::is_protected_config_path(&home, &target.to_string_lossy()) {
                return (Decision::Ask, PreAction::AsIs);
            }
        }
    }
    // Deterministic pre_tool_use gate (Feature B): a user hook can allow or deny
    // by exit code, short-circuiting both the destructive-Ask and — critically —
    // the LLM command classifier below (the one extra turn it would have cost).
    // User allow/soft-deny globs and the trust-root guard above still win.
    match ctx.hooks.pre_tool_use(tool, args).await {
        crate::checks::PreVerdict::Allow => return (Decision::Allow, PreAction::AsIs),
        crate::checks::PreVerdict::Ask(_reason) => return (Decision::Ask, PreAction::AsIs),
        crate::checks::PreVerdict::Deny(reason) => {
            return (Decision::Deny(reason), PreAction::AsIs)
        }
        crate::checks::PreVerdict::AllowRewritten(patch) => {
            return (Decision::Allow, PreAction::Rewrite(patch))
        }
        crate::checks::PreVerdict::Short(result) => {
            return (Decision::Allow, PreAction::Short(result))
        }
        crate::checks::PreVerdict::Pass => {}
    }
    if tool == "bash" && permissions::is_destructive(&inner) {
        return (Decision::Ask, PreAction::AsIs);
    }
    let needs_classify = tool == "bash"
        && !ctx.permissions.environment.is_empty()
        && !permissions::is_safe_readonly(&inner);
    if needs_classify {
        return classify(ctx, &inner).await;
    }
    (Decision::Allow, PreAction::AsIs)
}

/// Ask the LLM to classify a command against the configured environment.
/// Reuses the standard turn path with an empty tool registry; any failure is
/// treated as `Ask` (fail-safe).
async fn classify(ctx: &AgentContext, cmd: &str) -> (Decision, PreAction) {
    let system = permissions::classifier_system_prompt(&ctx.permissions.environment);
    let messages = [Message::system(system), Message::user(cmd)];
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let empty = ToolRegistry::new();
    let result = ctx
        .client
        .run_turn(
            &messages.iter().collect::<Vec<_>>(),
            &empty,
            &tx,
            &ctx.cancel,
        )
        .await;
    drop(tx);
    drain.await.ok();
    match result {
        Ok(turn) => {
            let text = extract_text(&turn.assistant_message.content);
            let (decision, rewrite) = permissions::parse_classification(&text, cmd);
            let action = rewrite.map_or(PreAction::AsIs, |command| {
                PreAction::Rewrite(serde_json::json!({ "command": command }))
            });
            (decision, action)
        }
        Err(_) => (Decision::Ask, PreAction::AsIs),
    }
}
