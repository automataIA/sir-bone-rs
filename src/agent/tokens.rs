//! Context-size estimation and the compaction trigger.

use crate::tools::truncate::CHARS_PER_TOKEN_ESTIMATE;
use crate::types::{ContentBlock, Message};

pub fn estimate_context_tokens<'a>(messages: impl IntoIterator<Item = &'a Message>) -> usize {
    messages
        .into_iter()
        .flat_map(|m| m.content.iter())
        .map(|b| match b {
            ContentBlock::Text { text } | ContentBlock::Thinking { thinking: text } => {
                text.len() / CHARS_PER_TOKEN_ESTIMATE
            }
            ContentBlock::ToolUse { input, .. } => {
                input.to_string().len() / CHARS_PER_TOKEN_ESTIMATE
            }
            ContentBlock::ToolResult { content, .. } => content.len() / CHARS_PER_TOKEN_ESTIMATE,
            ContentBlock::Image { data, .. } => data.len() / CHARS_PER_TOKEN_ESTIMATE,
        })
        .sum()
}

/// Size of the context as it stands, measured where possible and estimated only
/// for the remainder.
///
/// The provider counted the last request exactly; everything appended since is
/// estimated the old way. This beats estimating the whole transcript for two
/// reasons that both push the same direction: `estimate_context_tokens` never
/// sees the system prompt or the tool schemas (thousands of tokens, resent every
/// turn), and dividing bytes by four is itself unreliable on code and JSON. Both
/// errors are one-sided — they read the context as emptier than it is — which is
/// how a run reaches the provider's hard limit while the trigger says there is
/// room.
///
/// Falls back to the pure estimate whenever there is no anchor: before the first
/// reply, after a compaction, or on an endpoint that reports no usage.
pub(crate) fn context_tokens(ctx: &crate::agent::AgentContext) -> usize {
    match ctx.last_request {
        // A shorter transcript than the anchor means history was rewritten
        // under it; the index no longer points where it did.
        Some(anchor) if anchor.messages_len <= ctx.messages.len() => {
            anchor.input_tokens + estimate_context_tokens(&ctx.messages[anchor.messages_len..])
        }
        _ => estimate_context_tokens(&ctx.messages),
    }
}

/// `SIRBONE_COMPACT_BUDGET` — compact at an absolute token budget instead of at
/// a fraction of the window (experimental; unset = legacy behavior exactly).
///
/// `W - W/8` answers "will the next turn overflow?", which is the right question
/// for safety and the wrong one for cost: carried history is billed at the cache
/// rate *every turn*, so the per-turn cost of a threshold grows with the window
/// while the optimum does not — it is set by the post-compaction floor and by how
/// fast history grows, both independent of `W`. On a 1M window the two are two
/// orders of magnitude apart. See docs/costi.md §4b for the model.
pub fn compact_budget() -> Option<usize> {
    std::env::var("SIRBONE_COMPACT_BUDGET")
        .ok()?
        .parse::<usize>()
        .ok()
        .filter(|b| *b > 0)
}

/// Token ceiling for the window compaction keeps. A third of the trigger, so the
/// next check has room to breathe — keeping a tail as large as the trigger would
/// re-fire compaction immediately.
pub(crate) fn keep_budget(context_window: usize) -> usize {
    let legacy = context_window / 2;
    compact_budget().map_or(legacy, |b| legacy.min(b / 3))
}

pub(crate) fn should_compact(
    estimated_tokens: usize,
    context_window: usize,
    msg_count: usize,
    keep: usize,
) -> bool {
    let reserve = context_window / 8;
    // The window rule stays as the overflow backstop whatever the budget says:
    // a budget larger than the window must never disable compaction.
    let threshold = context_window
        .saturating_sub(reserve)
        .min(compact_budget().unwrap_or(usize::MAX));
    // Don't attempt compaction if there aren't enough messages to compact.
    // Prevents "too few messages to compact" bail from killing the session.
    msg_count.saturating_sub(keep) >= 2 && estimated_tokens >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 1_000_000;

    /// The budget lives in a process-global env var, so a test that sets it and a
    /// test that assumes it unset cannot run at the same time — and under the
    /// parallel runner they otherwise do.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn unset_budget_keeps_the_window_fraction() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());

        // 87.5% of the window, and a keep window of half of it.
        assert!(!should_compact(874_000, W, 10, 6));
        assert!(should_compact(875_000, W, 10, 6));
        assert_eq!(keep_budget(W), W / 2);
    }

    /// One test for every budget case: the var is process-global, so splitting
    /// these would let them race each other under the parallel test runner.
    #[test]
    fn budget_moves_the_trigger_without_ever_disabling_the_backstop() {
        const VAR: &str = "SIRBONE_COMPACT_BUDGET";
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());

        std::env::set_var(VAR, "125000");
        assert!(!should_compact(124_000, W, 10, 6));
        assert!(should_compact(125_000, W, 10, 6));
        // A third of the trigger, so the post-compaction check has room.
        assert_eq!(keep_budget(W), 41_666);

        // The window rule is a floor on the threshold: a budget wider than the
        // window must not stop compaction from firing before an overflow.
        std::env::set_var(VAR, "9000000");
        assert!(should_compact(875_000, W, 10, 6));

        for bad in ["0", "lots", ""] {
            std::env::set_var(VAR, bad);
            assert_eq!(compact_budget(), None, "should ignore {bad:?}");
        }

        std::env::remove_var(VAR);
        assert_eq!(keep_budget(W), W / 2);
    }
}
