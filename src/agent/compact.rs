//! Context compaction: summarize old messages via the LLM, keep the most recent.

use anyhow::Result;
use tracing::instrument;

use super::{estimate_context_tokens, AgentContext, COMPACTION_KEEP_RECENT_DEFAULT};
use crate::tools::ToolRegistry;
use crate::types::{AgentEvent, ContentBlock, Message};

#[instrument(skip_all, fields(messages = ctx.messages.len()))]
pub async fn compact(ctx: &mut AgentContext) -> Result<()> {
    let requested = ctx
        .compaction_keep_recent
        .unwrap_or(COMPACTION_KEEP_RECENT_DEFAULT)
        .min(ctx.messages.len());
    // `keep` is a message count, but the window is a token budget: six recent
    // messages carrying large tool results can exceed the whole context on their
    // own, so compaction succeeds and the very next check still overflows
    // ("Context window full after compaction"). Walk back from the newest message
    // and stop once the kept window would blow half the window, keeping the last
    // exchange unconditionally so there is always something to continue from.
    let budget = super::tokens::keep_budget(ctx.context_window.unwrap_or(128_000));
    let mut used = 0;
    let mut keep = 0;
    for msg in ctx.messages.iter().rev().take(requested) {
        let cost = estimate_context_tokens(std::iter::once(msg));
        if keep >= 2 && used + cost > budget {
            break;
        }
        used += cost;
        keep += 1;
    }
    // The split point is a blind index, so it can land between a tool_use and
    // its tool_result: the result stays in the kept window while the call that
    // produced it is summarized away. Providers reject an orphaned tool_result,
    // which kills the run. Walk the boundary back until the kept window no
    // longer opens on one.
    let mut to_compact = ctx.messages.len() - keep;
    while to_compact > 0
        && ctx.messages[to_compact]
            .content
            .first()
            .is_some_and(|b| matches!(b, ContentBlock::ToolResult { .. }))
    {
        to_compact -= 1;
    }

    // Same blindness, one level up: the boundary can land inside a turn, so the
    // kept window opens on the agent's own reasoning while the request that
    // motivated it is summarized away — the model reads its half-finished work
    // with no statement of what it was for. Pull the boundary back to the start
    // of the turn it landed in. `is_turn_start` is the structural test: the
    // machine also speaks as `Role::User` (oracle retries, stop-hook reasons,
    // stream-rule reminders), and those are continuations, not new turns.
    // Inclusive: when the boundary already *is* a turn start there is nothing
    // to pull back, and searching the open range would move it a turn too far.
    let turn_start = ctx.messages[..=to_compact]
        .iter()
        .rposition(|m| m.is_turn_start())
        .unwrap_or(0);
    // Only if it is affordable: the extra messages are kept, so they come out
    // of the same budget as `keep` above, and there must still be a real window
    // left to summarize. Otherwise the old boundary stands — a mid-turn cut is
    // worse context, but failing to compact at all kills the run.
    let extra = estimate_context_tokens(ctx.messages[turn_start..to_compact].iter());
    if turn_start >= 2 && used + extra <= budget {
        to_compact = turn_start;
    }

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

    // Union with what earlier compactions already recorded, so the list grows
    // with the session instead of describing only the latest window.
    touched.extend(ctx.compacted_files.iter().cloned());
    // Deterministic order for the summary's "Files modified" list.
    touched.sort_unstable();
    touched.dedup();
    ctx.compacted_files.clone_from(&touched);

    // Section list follows what compaction research and Claude Code's own
    // summarizer converged on: preserve user turns and constraints (the agent
    // must not deviate from the trajectory it was on), record failures so they
    // aren't retried, and pin the next step to a verbatim quote to avoid drift.
    const SUMMARY_INSTRUCTIONS: &str =
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
             Be concise. Omit pleasantries and tool invocation boilerplate.\n\
             Some state outlives this transcript and stays readable afterwards: \
             files written with `note`, and the spill files that truncation markers \
             point at (`full output: <path>`). Carry any path still worth reopening \
             into the summary — the path is enough, never copy the contents.";

    let files_list: String = touched.iter().map(|p| format!("- {p}\n")).collect();
    let files_header = "Files modified during this conversation (extracted from tool calls — \
         use this as the authoritative list for section 1):";

    // `SIRBONE_SUMMARY_VERBATIM` — send the region as the messages it already
    // is, behind the conversation's own system prompt and tools, instead of a
    // flattened re-serialization of it. Off by default and unproven: it is a bet
    // on the prefix cache being warm. When it is, every token of the region is
    // billed at the cache-read rate because the provider has seen exactly this
    // prefix; when it is not (expired TTL, an endpoint that ignores caching), it
    // costs more than the flattened copy, which truncates every tool result to
    // 300 chars. Ratios differ by an order of magnitude across providers, so
    // only an A/B on the target provider can settle it.
    //
    // Two caveats, both cost-only. The per-turn working-notes block sits between
    // system and transcript in `run_turn` and is not reproduced here, so on a
    // session that uses `note` the exact-prefix match stops at system + tools.
    // And the real registry is sent, which is what makes the tool schemas a hit
    // rather than an absent block — hence the explicit "do not call tools".
    let verbatim = std::env::var_os("SIRBONE_SUMMARY_VERBATIM").is_some();
    let system = Message::system(match (verbatim, &ctx.system_prompt) {
        (true, Some(s)) => s.as_str(),
        _ => SUMMARY_INSTRUCTIONS,
    });
    let closing = Message::user(match (verbatim, touched.is_empty()) {
        (true, true) => format!("{SUMMARY_INSTRUCTIONS}\nDo not call any tool: reply with the summary text only."),
        (true, false) => format!("{SUMMARY_INSTRUCTIONS}\nDo not call any tool: reply with the summary text only.\n\n{files_header}\n{files_list}"),
        (false, true) => conversation,
        (false, false) => format!("{files_header}\n{files_list}\n{conversation}"),
    });
    let region: &[Message] = if verbatim {
        // From index 0, not from `start`: an exact prefix of the last request is
        // what the cache is keyed on, and the prior summary pair is part of it.
        &ctx.messages[..to_compact]
    } else {
        &[]
    };
    let empty_registry = ToolRegistry::new();
    let summary_request: Vec<&Message> = std::iter::once(&system)
        .chain(region.iter())
        .chain(std::iter::once(&closing))
        .collect();

    let (dummy_tx, mut dummy_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move { while dummy_rx.recv().await.is_some() {} });

    let result = ctx
        .client
        .run_turn(
            &summary_request,
            if verbatim {
                &ctx.tools
            } else {
                &empty_registry
            },
            &dummy_tx,
            &ctx.cancel,
        )
        .await?;

    let summary = crate::types::extract_text(&result.assistant_message.content);
    if summary.is_empty() {
        anyhow::bail!("empty compaction summary");
    }

    // A summary is only worth swapping in if it is smaller than what it
    // replaces. A model that restates the region instead of condensing it (or
    // that answers the transcript rather than summarizing it) would otherwise
    // leave the context above the threshold, and the caller would compact again
    // on a window that cannot shrink. Refuse before the destructive drain, so
    // the transcript is untouched and the caller can try a different region.
    let head = Message::injected(format!(
        "[Previous conversation summary — {to_compact} messages compacted]\n{summary}"
    ));
    let ack = Message::assistant(
        "Understood. I have the context from the summary. Continuing where we left off.",
    );
    let region_tokens = estimate_context_tokens(ctx.messages[start..to_compact].iter());
    let summary_tokens = estimate_context_tokens([&head, &ack]);
    // Only where the comparison means something. The summary carries a fixed
    // frame (its header line and the acknowledgement) that costs ~45 tokens
    // whatever it summarizes, so on a tiny region the frame — not the model —
    // decides the verdict. A region that small is not what compaction is for:
    // the threshold that calls it never fires there.
    const MIN_MEANINGFUL_REGION: usize = 128;
    if region_tokens >= MIN_MEANINGFUL_REGION && summary_tokens >= region_tokens {
        anyhow::bail!(
            "summary is not smaller than the region it replaces \
             ({summary_tokens} estimated tokens >= {region_tokens})"
        );
    }

    let original = ctx.messages.len();
    let recent: Vec<Message> = ctx.messages.drain(to_compact..).collect();
    ctx.messages.clear();
    ctx.messages.push(head);
    ctx.messages.push(ack);
    // The measured-size anchor is keyed to a message index that this rewrite
    // just invalidated. Drop it; the next turn measures the new transcript.
    ctx.last_request = None;
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
            output_tokens: 0,
        })
        .await
        .ok();

    // The turns a post-edit diagnostic was first reported in may have just been
    // summarized away; keeping it in the ledger would suppress it as
    // "already reported above" when it is no longer above anything.
    ctx.hooks.post.forget_reported();

    crate::telemetry::add(&crate::telemetry::COMPACTION_FIRED, 1);
    tracing::info!(
        original,
        compacted_to = ctx.messages.len(),
        "context compacted"
    );

    Ok(())
}

/// Smallest tool result worth pruning. Below this the marker costs nearly as
/// much as the content and the loss buys nothing.
const PRUNE_MIN_CHARS: usize = 2_000;

/// Replace bulky tool results with a marker that says how to get them back.
///
/// The last-resort lever, and deliberately not a general one: an unconditional
/// version of this ("history hygiene", stubbing superseded reads) was A/B'd on
/// SpecBench in 2026-07 and removed — it failed the quality gate on the very
/// long-session bench it was built for, because a later task needed the decision
/// context it had thrown away. That verdict stands. What makes this call site
/// different is the counterfactual: it runs only after the provider has refused
/// the request and compaction alone could not shrink it, where the alternative
/// is not a better transcript but no session at all.
///
/// Two things keep it honest even there. It touches only tool *results* — never
/// a user message, an assistant reply, or a tool call — so the decisions and the
/// reasoning that produced them stay. And the marker carries the spill path when
/// there is one, which makes the content one `read` away rather than gone.
///
/// The newest message is spared on the first pass — it is usually the result the
/// model is about to reason over — but not spared unconditionally: an overflow
/// caused *by* that one result is the common case (a huge `bash` or `read` output
/// that just landed), and there refusing to touch it means ending the run.
///
/// Returns the estimated tokens freed.
pub(crate) fn prune_tool_results(messages: &mut [Message]) -> usize {
    let older = messages.len().saturating_sub(1);
    match prune_range(&mut messages[..older]) {
        0 => prune_range(&mut messages[older..]),
        freed => freed,
    }
}

fn prune_range(messages: &mut [Message]) -> usize {
    let mut freed = 0;
    for msg in messages {
        for block in &mut msg.content {
            let ContentBlock::ToolResult {
                content, is_error, ..
            } = block
            else {
                continue;
            };
            if content.len() < PRUNE_MIN_CHARS || content.starts_with("[pruned") {
                continue;
            }
            // Keep whatever the truncation marker already said about recovery;
            // it names the spill file when one was written.
            let recovery = content
                .lines()
                .find(|l| l.contains("full output: "))
                .map(|l| format!(" — {}", l.trim()))
                .unwrap_or_default();
            let kind = if *is_error { "error output" } else { "output" };
            let replacement = format!(
                "[pruned to fit the context: {} chars of tool {kind} dropped{recovery}]",
                content.len()
            );
            freed += (content.len() - replacement.len())
                / crate::tools::truncate::CHARS_PER_TOKEN_ESTIMATE;
            *content = replacement;
        }
    }
    freed
}

#[cfg(test)]
mod prune_tests {
    use super::*;
    use crate::types::Role;

    fn content_of(m: &Message) -> &str {
        match &m.content[0] {
            ContentBlock::ToolResult { content, .. } => content,
            ContentBlock::Text { text } => text,
            _ => panic!("unexpected block"),
        }
    }

    #[test]
    fn a_bulky_tool_result_becomes_a_marker_and_frees_tokens() {
        let mut msgs = vec![
            Message::tool_result("t1", "y".repeat(20_000), false),
            Message::user("next"),
        ];
        let freed = prune_tool_results(&mut msgs);
        assert!(freed > 6_000, "freed only {freed} tokens");
        assert!(content_of(&msgs[0]).starts_with("[pruned to fit the context:"));
    }

    #[test]
    fn the_marker_keeps_the_spill_path_so_the_output_stays_one_read_away() {
        let body = format!(
            "{}\n[truncated — full output: /tmp/spill-7.txt]",
            "y".repeat(9_000)
        );
        let mut msgs = vec![
            Message::tool_result("t1", body, false),
            Message::user("next"),
        ];
        prune_tool_results(&mut msgs);
        assert!(
            content_of(&msgs[0]).contains("full output: /tmp/spill-7.txt"),
            "recovery path lost: {}",
            content_of(&msgs[0])
        );
    }

    /// Small results and non-tool content are off limits whatever happens: the
    /// pruner may cost tokens, never decisions.
    #[test]
    fn small_results_and_conversation_content_are_never_touched() {
        let big = "y".repeat(20_000);
        let mut msgs = vec![
            Message::tool_result("t1", "small output", false),
            Message::user(big.clone()),
            Message::assistant(big.clone()),
            Message::tool_result("t2", big.clone(), false),
            Message::user("go on"),
        ];
        let before = msgs.clone();
        prune_tool_results(&mut msgs);
        assert_eq!(msgs[..3], before[..3]);
        assert_eq!(msgs[3].role, Role::Tool);
    }

    /// Sparing the newest result is a preference, not a rule — an overflow caused
    /// by the result that just landed has nothing else to give back.
    #[test]
    fn the_newest_result_is_pruned_only_when_nothing_older_can_be() {
        let big = "y".repeat(20_000);
        let mut with_older = vec![
            Message::tool_result("t1", big.clone(), false),
            Message::tool_result("t2", big.clone(), false),
        ];
        prune_tool_results(&mut with_older);
        assert!(content_of(&with_older[0]).starts_with("[pruned"));
        assert_eq!(content_of(&with_older[1]), big);

        let mut newest_only = vec![
            Message::user("run it"),
            Message::tool_result("t1", big, false),
        ];
        assert!(prune_tool_results(&mut newest_only) > 0);
        assert!(content_of(&newest_only[1]).starts_with("[pruned"));
    }

    #[test]
    fn a_second_pass_finds_nothing_left_to_prune() {
        let mut msgs = vec![
            Message::tool_result("t1", "y".repeat(20_000), false),
            Message::user("next"),
        ];
        prune_tool_results(&mut msgs);
        assert_eq!(prune_tool_results(&mut msgs), 0);
    }
}
