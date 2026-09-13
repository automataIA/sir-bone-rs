//! Applier for a parsed [`Patch`]. Pure: original text in, new text out.

use std::collections::HashMap;

use anyhow::{bail, Result};

use super::parse::{Body, Op, Patch};

/// Apply every span operation of `patch` to `content`.
///
/// All addresses refer to the *original* file, so registers are captured from
/// the original lines first and the edits are then applied bottom-up: no
/// operation can shift the addresses of another.
pub fn apply(patch: &Patch, content: &str) -> Result<String> {
    let mut lines: Vec<&str> = content.lines().collect();
    let len = lines.len();

    // Resolve `>$` and bounds-check before touching anything.
    let mut ops: Vec<(usize, usize, &Op)> = Vec::with_capacity(patch.ops.len());
    for op in &patch.ops {
        let (start, end) = if op.start == usize::MAX {
            (len, len)
        } else {
            (op.start, op.end)
        };
        if end > len || start > len {
            bail!(
                "lines {}-{} are outside {} ({len} lines) — re-read it and use real line numbers",
                start + 1,
                end,
                patch.path
            );
        }
        ops.push((start, end, op));
    }

    // Anchors: an address written `32#a7` claims line 32 still holds the line
    // whose tag `read` printed. The header tag only proves the file is the one
    // that was read — it says nothing about the address being off by a few
    // lines, which is the miscount that damages a healthy file silently.
    let anchors = ops
        .iter()
        .filter(|_| !crate::ablate::patch_anchors_disabled())
        .flat_map(|(_, _, op)| &op.anchors);
    for (idx, expect) in anchors {
        let Some(line) = lines.get(*idx) else {
            bail!(
                "line {} is outside {} ({len} lines) — re-read it",
                idx + 1,
                patch.path
            );
        };
        let actual = crate::tools::freshness::line_tag(line);
        if &actual != expect {
            bail!(
                "line {} is `{}` (tag {actual}), not the {expect} you addressed — \
                 re-read {} and use the line numbers it prints",
                idx + 1,
                snippet(line),
                patch.path
            );
        }
    }

    // Phase 1: capture registers from the untouched original.
    let mut regs: HashMap<&str, Vec<&str>> = HashMap::new();
    for (start, end, op) in &ops {
        if let Some(name) = &op.capture {
            regs.insert(name.as_str(), lines[*start..*end].to_vec());
        }
    }

    // Phase 2: splice bottom-up so earlier addresses stay valid.
    ops.sort_by_key(|o| std::cmp::Reverse(o.0));
    for (start, end, op) in &ops {
        let body: Vec<&str> = match &op.body {
            Body::Lines(l) => l.iter().map(String::as_str).collect(),
            Body::Register(name) => {
                let Some(reg) = regs.get(name.as_str()) else {
                    let named = if name.is_empty() {
                        "the anonymous register".to_string()
                    } else {
                        format!("register @{name}")
                    };
                    bail!("{named} is empty — a CUT must fill it in the same patch");
                };
                reg.clone()
            }
        };
        lines.splice(*start..*end, body);
    }

    // `lines()` drops the terminator; a non-empty result is normalized to end
    // with one newline, whether or not the original did.
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// Enough of a line to recognize it in an error, without echoing a long line
/// back into the transcript.
fn snippet(line: &str) -> String {
    let line = line.trim();
    match line.char_indices().nth(48) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::parse;
    use super::*;

    const SRC: &str = "one\ntwo\nthree\nfour\nfive\n";

    fn run(patch: &str) -> String {
        apply(&parse(patch).expect("parse"), SRC).expect("apply")
    }

    #[test]
    fn put_replaces_an_inclusive_range() {
        assert_eq!(run("[f#T]\nPUT 2.=3:\n+TWO\n"), "one\nTWO\nfour\nfive\n");
    }

    #[test]
    fn put_inserts_before_after_and_at_the_end() {
        assert_eq!(
            run("[f#T]\nPUT <1:\n+zero\n"),
            "zero\none\ntwo\nthree\nfour\nfive\n"
        );
        assert_eq!(
            run("[f#T]\nPUT >1:\n+1.5\n"),
            "one\n1.5\ntwo\nthree\nfour\nfive\n"
        );
        assert_eq!(
            run("[f#T]\nPUT >$:\n+six\n"),
            "one\ntwo\nthree\nfour\nfive\nsix\n"
        );
    }

    #[test]
    fn cut_removes_and_an_empty_body_deletes() {
        assert_eq!(run("[f#T]\nCUT 2.=4\n"), "one\nfive\n");
        assert_eq!(run("[f#T]\nPUT 1.=1:\n"), "two\nthree\nfour\nfive\n");
    }

    #[test]
    fn registers_move_a_block() {
        // Move lines 1-2 to the end: addresses still refer to the original.
        assert_eq!(
            run("[f#T]\nCUT 1.=2 @blk\nPUT >$ @blk\n"),
            "three\nfour\nfive\none\ntwo\n"
        );
    }

    #[test]
    fn non_adjacent_edits_do_not_shift_each_other() {
        // Growing the top must not move the bottom address.
        assert_eq!(
            run("[f#T]\nPUT 1.=1:\n+a\n+b\n+c\nPUT 5.=5:\n+FIVE\n"),
            "a\nb\nc\ntwo\nthree\nfour\nFIVE\n"
        );
    }

    #[test]
    fn out_of_range_and_undefined_registers_are_rejected() {
        let too_far = parse("[f#T]\nPUT 9.=9:\n+x\n").unwrap();
        assert!(apply(&too_far, SRC)
            .unwrap_err()
            .to_string()
            .contains("outside"));
        let unbound = parse("[f#T]\nPUT >1 @nope\n").unwrap();
        assert!(apply(&unbound, SRC)
            .unwrap_err()
            .to_string()
            .contains("empty"));
    }

    #[test]
    fn a_line_tag_that_does_not_match_refuses_the_patch() {
        let tag = crate::tools::freshness::line_tag("two");
        // The right tag on the right line applies as usual.
        let good = parse(&format!("[f#T]\nPUT 2#{tag}.=2:\n+TWO\n")).unwrap();
        assert_eq!(apply(&good, SRC).unwrap(), "one\nTWO\nthree\nfour\nfive\n");
        // The same tag one line off names a line that is no longer there.
        let off_by_one = parse(&format!("[f#T]\nPUT 3#{tag}.=3:\n+TWO\n")).unwrap();
        let err = apply(&off_by_one, SRC).unwrap_err().to_string();
        assert!(err.contains("line 3 is `three`"), "{err}");
        assert!(err.contains("re-read"), "{err}");
    }

    #[test]
    fn a_file_without_a_trailing_newline_keeps_its_shape() {
        let patch = parse("[f#T]\nPUT 1.=1:\n+ONE\n").unwrap();
        assert_eq!(apply(&patch, "one\ntwo").unwrap(), "ONE\ntwo\n");
    }
}
