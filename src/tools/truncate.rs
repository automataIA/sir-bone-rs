/// Crude token estimate shared with the agent's context accounting
/// (`estimate_context_tokens`, `should_compact`): 1 token ≈ 3 bytes.
pub const CHARS_PER_TOKEN_ESTIMATE: usize = 3;
/// Cap a single tool result at ~16k estimated tokens, so one output can't eat
/// the context window (the line cap alone doesn't bound long-line output).
pub const DEFAULT_MAX_RESULT_TOKENS: usize = 16_000;

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = DEFAULT_MAX_RESULT_TOKENS * CHARS_PER_TOKEN_ESTIMATE;

/// Truncate with the default line+byte caps — the common case. Web tools pass
/// an explicit (smaller) line cap, so they stay on [`truncate_output`].
pub fn truncate_default(content: String) -> String {
    truncate_output(content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
}

/// Truncate tool output to fit within line and byte limits.
/// Returns the content unchanged if within limits; otherwise keeps the head and
/// tail, eliding the middle (where the signal usually lives in logs/tests), and
/// the marker tells the agent how to narrow the next query.
pub fn truncate_output(content: String, max_lines: usize, max_bytes: usize) -> String {
    if content.len() <= max_bytes && content.lines().count() <= max_lines {
        return content;
    }

    let lines: Vec<&str> = content.lines().collect();
    let head = take_lines(lines.iter().copied(), max_lines / 2, max_bytes / 2);
    let mut tail = take_lines(
        lines.iter().rev().copied(),
        max_lines - max_lines / 2,
        max_bytes - max_bytes / 2,
    );
    tail.reverse();
    let omitted = lines.len().saturating_sub(head.len() + tail.len());

    // When every line exceeds the byte budget, head+tail are both empty and the
    // output would be just the truncation marker — the agent would see none of
    // the payload. Fall back to a char-boundary head slice PLUS the marker, so
    // it always sees at least the start of the content and the truncation signal.
    if head.is_empty() && tail.is_empty() {
        let mut out = take_char_prefix(&content, max_bytes);
        out.push_str(
            "\n... (content truncated — narrow with grep/offset/limit, or re-run with | tail -n N) ...\n",
        );
        return out;
    }

    let mut out = head.join("\n");
    out.push_str(&format!(
        "\n... ({omitted} lines truncated — narrow with grep/offset/limit, or re-run with | tail -n N) ...\n"
    ));
    out.push_str(&tail.join("\n"));
    out
}

/// Take a prefix of `s` no longer than `max_bytes`, cut on a UTF-8 char boundary.
/// A non-empty input never yields an empty result: if the budget is smaller than
/// the first char, the first char is included anyway (a slight over-budget beat
/// showing nothing).
fn take_char_prefix(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        end = s.char_indices().nth(1).map(|(i, _)| i).unwrap_or(s.len());
    }
    s[..end].to_string()
}

/// Greedily take lines until either budget is hit, in iteration order.
fn take_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    max_lines: usize,
    max_bytes: usize,
) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for line in lines {
        if out.len() >= max_lines || bytes + line.len() + 1 > max_bytes {
            break;
        }
        bytes += line.len() + 1;
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Within both budgets, the content is returned byte-for-byte.
        #[test]
        fn passthrough_within_limits(s in ".*") {
            let ml = s.lines().count() + 1;
            let mb = s.len() + 1;
            prop_assert_eq!(truncate_output(s.clone(), ml, mb), s);
        }

        /// More lines than the line budget always truncates (and says so).
        #[test]
        fn line_overflow_truncates(n in 4usize..200, ml in 1usize..3) {
            let s: String = (0..n).map(|i| format!("line{i}\n")).collect();
            prop_assume!(s.lines().count() > ml);
            let out = truncate_output(s, ml, usize::MAX);
            prop_assert!(out.contains("truncated"));
        }

        /// Arbitrary input under tight budgets must never panic, and never turn
        /// non-empty input into empty output.
        #[test]
        fn never_panics_preserves_nonempty(s in ".*", ml in 1usize..10, mb in 2usize..64) {
            let out = truncate_output(s.clone(), ml, mb);
            prop_assert_eq!(s.is_empty(), out.is_empty());
        }
    }

    #[test]
    fn no_truncation_under_limits() {
        let s = "hello\nworld\n".to_string();
        assert_eq!(truncate_output(s.clone(), 100, 1_000_000), s);
    }

    #[test]
    fn truncate_by_lines() {
        let s = "a\nb\nc\nd\ne\n".to_string();
        let r = truncate_output(s, 3, 1_000_000);
        assert!(r.starts_with("a\n"));
        assert!(r.contains("truncated"));
        assert!(r.contains("d"));
        assert!(r.contains("e"));
        assert!(!r.contains("\nb\n"));
        assert!(!r.contains("\nc\n"));
    }

    #[test]
    fn truncate_by_bytes() {
        let s = "hello world\n".repeat(100);
        let r = truncate_output(s, DEFAULT_MAX_LINES, 50);
        assert!(r.contains("truncated"));
        assert!(r.len() < 200);
    }

    #[test]
    fn empty_content_unchanged() {
        assert_eq!(truncate_output(String::new(), 100, 1_000), String::new());
    }

    #[test]
    fn default_byte_cap_bounds_long_line_output() {
        // Few lines but huge: the line cap can't bite, the byte cap must.
        // Pins the default to a context-window-safe token estimate.
        let s = "x".repeat(10 * DEFAULT_MAX_BYTES);
        let r = truncate_output(s, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        assert!(r.contains("truncated"));
        assert!(
            r.len() / CHARS_PER_TOKEN_ESTIMATE <= DEFAULT_MAX_RESULT_TOKENS + 100,
            "estimated tokens {} exceed budget {DEFAULT_MAX_RESULT_TOKENS}",
            r.len() / CHARS_PER_TOKEN_ESTIMATE
        );
    }

    // --- exact head/tail/omitted accounting (pins the budget arithmetic) ---

    #[test]
    fn line_budget_splits_head_and_tail_exactly() {
        // 14 lines, line budget 6, byte budget effectively unlimited. The split
        // is max_lines/2 head + the rest tail; any mutation to the `/2`, `-`, or
        // the omitted `head+tail` sum shifts which lines survive.
        let s: String = (0..14).map(|i| format!("L{i:02}\n")).collect();
        let r = truncate_output(s, 6, 1_000_000);
        assert!(r.starts_with("L00\nL01\nL02\n"), "head = first 3: {r}");
        assert!(r.ends_with("L11\nL12\nL13"), "tail = last 3: {r}");
        assert!(r.contains("(8 lines truncated"), "omitted = 14 - 6: {r}");
        assert!(!r.contains("L06"), "middle elided");
    }

    #[test]
    fn byte_budget_truncation_at_30() {
        // 12 four-char lines, byte budget 30 -> 15 per side. Each line costs 5
        // bytes (4 + newline); 3 fit per side. Pins the byte check `>`, the
        // `bytes + len + 1` accumulation, and the head budget split.
        let s: String = (0..12).map(|i| format!("aa{i:02}\n")).collect();
        let r = truncate_output(s, 1000, 30);
        assert!(
            r.starts_with("aa00\naa01\naa02\n"),
            "head = 3 by bytes: {r}"
        );
        assert!(r.ends_with("aa09\naa10\naa11"), "tail = 3 by bytes: {r}");
        assert!(r.contains("(6 lines truncated"), "omitted = 12 - 6: {r}");
        assert!(!r.contains("aa05"));
    }

    #[test]
    fn byte_budget_truncation_at_28() {
        // Budget 28 -> 14 per side: only 2 four-char lines fit (the 3rd check is
        // 10 + 4 + 1 = 15 > 14). Catches the off-by-one in `line.len() + 1`
        // inside the budget check.
        let s: String = (0..12).map(|i| format!("aa{i:02}\n")).collect();
        let r = truncate_output(s, 1000, 28);
        assert!(r.starts_with("aa00\naa01\n"), "head = 2 by bytes: {r}");
        assert!(!r.contains("aa02"), "the 3rd line must not fit");
        assert!(r.contains("(8 lines truncated"), "omitted = 12 - 4: {r}");
    }

    #[test]
    fn tiny_byte_budget_keeps_a_head_slice() {
        // Every line is longer than the per-side byte budget → head+tail empty.
        // The agent must still see SOMETHING, never just the truncation marker.
        let r = truncate_output("AAAA\nBBBB\nCCCC\n".to_string(), 2, 4);
        assert!(!r.is_empty());
        assert!(r.starts_with("AAAA"), "head slice of the first line: {r}");
        assert!(
            !r.contains("lines truncated"),
            "no marker when falling back: {r}"
        );
    }

    #[test]
    fn char_prefix_cuts_on_boundary() {
        assert_eq!(take_char_prefix("héllo", 2), "h"); // é is 2 bytes; cut before it
        assert_eq!(take_char_prefix("abc", 10), "abc");
        assert_eq!(take_char_prefix("abcdef", 4), "abcd");
    }
}
