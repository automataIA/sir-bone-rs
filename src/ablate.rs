//! Feature-ablation toggles via `SIRBONE_DISABLE` (feature-audit harness).
//!
//! Format: comma list of `kind:name`, e.g. `"tool:web_search,skill:tdd,cache:prompt"`.
//! `name` may be `*` to disable every entry of that kind (`skill:*` hides all skills).
//! Unset/empty => everything enabled (normal behavior — zero effect on shipped runs).
//!
//! Four cut points read these: `make_tools`/`read_only_registry` (drop tools),
//! `scan_skills` (hide skills from catalog + `load_skill`), the Anthropic client
//! (skip prompt-cache `cache_control`), and `build_system_prompt` (drop named
//! prompt blocks). One module so the parse rule lives once.
//!
//! `prompt:*` is the "naked prompt" arm — every sirbone-authored block off, only
//! identity + platform context + the user's own CLAUDE.md left. It exists to
//! measure how much of the prompt is dead weight the model no longer needs, the
//! way Claude Code's `CLAUDE_CODE_SIMPLE` does: delete everything, then add back
//! one block at a time on a *measured* failure, not on a hunch.

/// True if `spec` lists `kind:name`, honouring `kind:*` when `wildcard`. Pure —
/// the testable core.
fn spec_matches(spec: &str, kind: &str, name: &str, wildcard: bool) -> bool {
    spec.split(',')
        .filter_map(|e| e.trim().split_once(':'))
        .any(|(k, n)| {
            k.trim() == kind && {
                let n = n.trim();
                n == name || (wildcard && n == "*")
            }
        })
}

/// True if `spec` lists `kind:name` (or `kind:*`).
fn spec_has(spec: &str, kind: &str, name: &str) -> bool {
    spec_matches(spec, kind, name, true)
}

fn env_has(kind: &str, name: &str) -> bool {
    std::env::var("SIRBONE_DISABLE").is_ok_and(|v| spec_has(&v, kind, name))
}

/// True if `spec` (a comma allowlist) permits `name`. Empty spec = no allowlist.
fn spec_allows(spec: &str, name: &str) -> bool {
    spec.trim().is_empty() || spec.split(',').any(|t| t.trim() == name)
}

/// True when the tool must not be registered: named in `SIRBONE_DISABLE`, or
/// missing from a `SIRBONE_TOOLS` allowlist.
///
/// `SIRBONE_TOOLS` is the shipped knob (`SIRBONE_TOOLS=web_search` for a
/// search-only gateway), not part of the audit harness: it fails *closed*, so a
/// tool added to sirbone later stays out until it is named, where the blacklist
/// would have let it in silently. Unset/empty = every tool, as before.
pub fn disabled_tool(name: &str) -> bool {
    env_has("tool", name)
        || std::env::var("SIRBONE_TOOLS").is_ok_and(|spec| !spec_allows(&spec, name))
}

pub fn disabled_skill(name: &str) -> bool {
    env_has("skill", name)
}

pub fn disabled_hook(name: &str) -> bool {
    env_has("hook", name)
}

pub fn oracle_gate_disabled() -> bool {
    env_has("oracle", "gate")
}

pub fn ask_rounds_enabled() -> bool {
    std::env::var_os("SIRBONE_ASK_ROUNDS").is_some() && !env_has("ask", "rounds")
}

/// `tool:spill` — stop writing truncated tool output to a recoverable file.
pub fn spill_disabled() -> bool {
    env_has("tool", "spill")
}

/// `read:outline` — turn the structural read off even with `SIRBONE_READ_OUTLINE` set.
pub fn read_outline_disabled() -> bool {
    env_has("read", "outline")
}

/// `patch:anchors` — stop printing per-line tags in `read` and stop checking the
/// ones a patch address quotes. The control arm for the anchor A/B: with the
/// tags absent from the read view the model has nothing to quote, so the arm
/// reproduces the pre-anchor patch tool exactly.
pub fn patch_anchors_disabled() -> bool {
    env_has("patch", "anchors")
}

/// `code_map:lines` — make `code_map find_references` answer with bare file
/// paths again, and describe itself that way. The control arm for the
/// `path:line:content` A/B: it reproduces the pre-enrichment tool exactly, both
/// the output and the schema text the model reads, so the arm is the shipped
/// change as one unit (output format + description) rather than either half.
pub fn code_map_lines_disabled() -> bool {
    env_has("code_map", "lines")
}

/// `code_map:ref_rank` — turn the match-count ordering of reference detail off
/// even with `SIRBONE_REF_RANK` set. The candidate/control pair for that
/// ordering only: coverage and the reserved definer slot are unconditional, so
/// both arms show the same *files* and differ solely in which of them get a
/// second and third line.
pub fn ref_rank_disabled() -> bool {
    env_has("code_map", "ref_rank")
}

/// `code_map:ref_budget` — restore the pre-2026-09-04 reference budget: three
/// lines per file in path order until the 40-line cap is gone, each overflow
/// note taking a row of its own, everything past the cap collapsed into an
/// opaque "N more file(s) not shown", and no slot reserved for the declaring
/// file or for the declaration line inside a file. The control arm for the
/// coverage-first allocation, so both halves of that change (which files are
/// visible, and which line of a file is) revert together as one unit.
pub fn ref_budget_disabled() -> bool {
    env_has("code_map", "ref_budget")
}

/// `stream:rules` — turn configured mid-stream rules off.
pub fn stream_rules_disabled() -> bool {
    env_has("stream", "rules")
}

/// `test:notice` — stop appending the "a test you changed is not evidence" fact
/// after a batch that wrote to a test file. The control arm for that A/B: the
/// note is the whole treatment, so removing it reproduces the previous run
/// exactly (the `test_file_mutations` counter is unaffected either way, which
/// is what lets the arms be compared on runs where the note could have fired).
pub fn test_notice_disabled() -> bool {
    env_has("test", "notice")
}

pub fn cache_disabled() -> bool {
    env_has("cache", "prompt")
}

/// True if the named system-prompt block is ablated (`prompt:<name>` or `prompt:*`).
pub fn disabled_prompt(name: &str) -> bool {
    env_has("prompt", name)
}

/// Like [`disabled_prompt`] but ignores `prompt:*`. For blocks that must survive
/// the naked-prompt sweep unless named outright — `claude_md` carries the user's
/// project instructions, and the sweep prices sirbone's own prompt weight.
pub fn disabled_prompt_exact(name: &str) -> bool {
    std::env::var("SIRBONE_DISABLE").is_ok_and(|v| spec_matches(&v, "prompt", name, false))
}

#[cfg(test)]
mod tests {
    use super::{spec_allows, spec_has, spec_matches};

    #[test]
    fn allowlist_keeps_only_named_tools() {
        assert!(spec_allows("web_search, read", "read"));
        assert!(!spec_allows("web_search", "bash"));
        assert!(spec_allows("", "bash")); // no allowlist => everything
    }

    #[test]
    fn matches_kind_and_name() {
        let s = "tool:web_search, skill:tdd ,cache:prompt";
        assert!(spec_has(s, "tool", "web_search"));
        assert!(spec_has(s, "skill", "tdd"));
        assert!(spec_has(s, "cache", "prompt"));
    }

    #[test]
    fn no_false_positives() {
        let s = "tool:web_search";
        assert!(!spec_has(s, "tool", "code_map"));
        assert!(!spec_has(s, "skill", "web_search")); // wrong kind
        assert!(!spec_has("", "tool", "web_search"));
    }

    #[test]
    fn wildcard_matches_any_name_of_kind() {
        assert!(spec_has("skill:*", "skill", "tdd"));
        assert!(spec_has("skill:*", "skill", "diagnose"));
        assert!(!spec_has("skill:*", "tool", "code_map")); // wildcard is kind-scoped
    }

    #[test]
    fn prompt_kind_is_independent_of_the_others() {
        assert!(spec_has("prompt:historia", "prompt", "historia"));
        assert!(!spec_has("prompt:historia", "tool", "historia")); // block != tool
        assert!(spec_has("prompt:*", "prompt", "bugfix")); // naked-prompt arm
        assert!(!spec_has("prompt:*", "skill", "tdd"));
    }

    #[test]
    fn exact_match_opts_out_of_the_wildcard_sweep() {
        // `claude_md` (user instructions) survives `prompt:*`, goes only when named.
        assert!(!spec_matches("prompt:*", "prompt", "claude_md", false));
        assert!(spec_matches(
            "prompt:*,prompt:claude_md",
            "prompt",
            "claude_md",
            false
        ));
    }
}
