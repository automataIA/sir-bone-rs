//! Context-size estimation and the compaction trigger.

use crate::tools::truncate::CHARS_PER_TOKEN_ESTIMATE;
use crate::types::{ContentBlock, Message};

pub fn estimate_context_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
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

pub(crate) fn should_compact(
    estimated_tokens: usize,
    context_window: usize,
    msg_count: usize,
    keep: usize,
) -> bool {
    let reserve = context_window / 8;
    // Don't attempt compaction if there aren't enough messages to compact.
    // Prevents "too few messages to compact" bail from killing the session.
    msg_count.saturating_sub(keep) >= 2
        && estimated_tokens >= context_window.saturating_sub(reserve)
}
