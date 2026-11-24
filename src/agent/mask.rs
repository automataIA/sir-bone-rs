//! Observation masking (opt-in, `SIRBONE_MASK=1`).
//!
//! Every request re-sends the whole transcript, so an old `read`/`grep` result
//! is paid for again on each later turn. Masking replaces the body of a
//! read-only tool result with a short stub once it is [`KEEP_TURNS`] assistant
//! turns old. Edits, writes, mutating bash and test output are never touched,
//! and the tool call itself stays, so the model can see what it ran and run it
//! again.
//!
//! Each sweep changes an early block, which invalidates the provider's prompt
//! cache from that point on. A sweep therefore runs only when at least
//! [`MIN_SWEEP_BYTES`] can be reclaimed (like Anthropic's `clear_at_least`).
//! Parameters are the ones pre-registered in
//! `bench/harness_levers_probe/results.md`; they are not tuned here.

use std::collections::HashMap;

use serde_json::Value;

use crate::permissions::{shell_segments, writes_file};
use crate::types::{ContentBlock, Message, Role};

/// Results younger than this many assistant turns are never masked.
const KEEP_TURNS: usize = 20;
/// Reclaimable bytes needed before a sweep runs (~5k tokens at 3 bytes/token).
const MIN_SWEEP_BYTES: usize = 15_000;
/// Results smaller than this are left alone: the stub would save little.
const MIN_RESULT_BYTES: usize = 600;
const STUB_PREFIX: &str = "[elided:";

pub fn enabled() -> bool {
    std::env::var_os("SIRBONE_MASK").is_some()
}

/// Mask stale read-only results in place. Returns how many were masked.
pub fn mask_stale(messages: &mut [Message]) -> usize {
    let calls: HashMap<String, String> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } if is_nav(name, input) => {
                Some((id.clone(), summary(name, input)))
            }
            _ => None,
        })
        .collect();
    let total = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();

    let mut seen = 0;
    let mut targets = Vec::new();
    let mut bytes = 0;
    for (i, m) in messages.iter().enumerate() {
        if m.role == Role::Assistant {
            seen += 1;
            continue;
        }
        if total - seen < KEEP_TURNS {
            break; // everything from here on is younger
        }
        for (j, b) in m.content.iter().enumerate() {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: false,
            } = b
            else {
                continue;
            };
            if content.len() >= MIN_RESULT_BYTES
                && !content.starts_with(STUB_PREFIX)
                && calls.contains_key(tool_use_id)
            {
                targets.push((i, j));
                bytes += content.len();
            }
        }
    }
    if bytes < MIN_SWEEP_BYTES {
        return 0;
    }
    for &(i, j) in &targets {
        if let ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } = &mut messages[i].content[j]
        {
            let what = calls.get(tool_use_id).map_or("tool call", String::as_str);
            *content = format!(
                "{STUB_PREFIX} output of {what} ({} bytes) dropped from context after \
                 {KEEP_TURNS} turns; run it again if you need it]",
                content.len()
            );
        }
    }
    targets.len()
}

fn is_nav(name: &str, input: &Value) -> bool {
    match name {
        "read" | "grep" | "glob" | "code_map" => true,
        "bash" => input
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(is_readonly_bash),
        _ => false,
    }
}

/// Read-only bash: every segment is a known reader and nothing writes a file.
/// Wider than `permissions::is_safe_readonly` (adds `sed -n`, `find`, `awk`…)
/// because a wrong guess here costs a re-read, not a permission bypass.
fn is_readonly_bash(cmd: &str) -> bool {
    const NAV: &[&str] = &[
        "ls", "cat", "head", "tail", "wc", "grep", "rg", "echo", "pwd", "sed -n", "awk", "find",
        "nl", "sort", "uniq", "cut", "stat", "tree", "jq", "cd", "git show", "git grep",
        "git log", "git diff", "git status",
    ];
    let cmd = cmd
        .replace("2>/dev/null", "")
        .replace("&>/dev/null", "")
        .replace(">/dev/null", "");
    let Some(segs) = shell_segments(&cmd) else {
        return false;
    };
    !segs.is_empty()
        && segs.iter().all(|seg| {
            let c = seg.trim();
            !writes_file(seg)
                && !(c.starts_with("find") && (c.contains("-exec") || c.contains("-delete")))
                && !(c.starts_with("awk") && c.contains("system("))
                && NAV
                    .iter()
                    .any(|p| c == *p || c.starts_with(&format!("{p} ")))
        })
}

fn summary(name: &str, input: &Value) -> String {
    let arg = ["command", "path", "file_path", "pattern", "symbol"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .unwrap_or("");
    let arg: String = arg.chars().take(80).collect();
    format!("`{name} {arg}`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str, input: Value) -> Message {
        Message {
            role: Role::Assistant,
            injected: false,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            }],
        }
    }

    fn session(turns: usize, body: &str) -> Vec<Message> {
        (0..turns)
            .flat_map(|i| {
                let id = format!("c{i}");
                [
                    call(&id, "read", json!({"path": format!("f{i}.rs")})),
                    Message::tool_result(id, body, false),
                ]
            })
            .collect()
    }

    fn result(m: &Message) -> &str {
        match &m.content[0] {
            ContentBlock::ToolResult { content, .. } => content,
            _ => "",
        }
    }

    #[test]
    fn masks_only_results_older_than_keep_turns() {
        let mut msgs = session(KEEP_TURNS + 10, &"x".repeat(2000));
        let n = mask_stale(&mut msgs);
        assert_eq!(n, 10);
        assert!(result(&msgs[1]).starts_with(STUB_PREFIX));
        assert!(result(&msgs[1]).contains("`read f0.rs`"));
        assert!(!result(&msgs[msgs.len() - 1]).starts_with(STUB_PREFIX));
        // a second sweep has nothing left to reclaim
        assert_eq!(mask_stale(&mut msgs), 0);
    }

    #[test]
    fn no_sweep_below_threshold() {
        let mut msgs = session(KEEP_TURNS + 2, &"x".repeat(1000));
        assert_eq!(mask_stale(&mut msgs), 0);
    }

    #[test]
    fn mutating_and_error_results_stay() {
        let big = "x".repeat(20_000);
        let mut msgs = vec![
            call("e", "edit", json!({"path": "a.rs"})),
            Message::tool_result("e", big.clone(), false),
            call("b", "bash", json!({"command": "cargo test"})),
            Message::tool_result("b", big.clone(), false),
            call("r", "read", json!({"path": "a.rs"})),
            Message::tool_result("r", big.clone(), true),
        ];
        msgs.extend(session(KEEP_TURNS, "small"));
        assert_eq!(mask_stale(&mut msgs), 0);
    }

    #[test]
    fn readonly_bash_classifier() {
        assert!(is_readonly_bash("sed -n 1,20p src/a.rs"));
        assert!(is_readonly_bash("cd x && grep -rn foo src 2>/dev/null | head"));
        assert!(!is_readonly_bash("sed -i s/a/b/ f"));
        assert!(!is_readonly_bash("cat a > b"));
        assert!(!is_readonly_bash("find . -delete"));
        assert!(!is_readonly_bash("node tests/run.js"));
        assert!(!is_readonly_bash("echo $(rm -rf x)"));
    }
}
