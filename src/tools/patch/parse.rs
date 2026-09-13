//! Parser for the hashline patch language. Pure: text in, [`Patch`] out.

use anyhow::{bail, Result};

/// What fills the span an operation covers.
#[derive(Debug, PartialEq, Eq)]
pub enum Body {
    /// Literal `+` lines, already stripped of the marker.
    Lines(Vec<String>),
    /// Contents of a register captured by an earlier `CUT`.
    Register(String),
}

/// One span operation. `start`/`end` index the *original* line vector,
/// 0-based with an exclusive end; a zero-width span is an insertion.
#[derive(Debug, PartialEq, Eq)]
pub struct Op {
    pub start: usize,
    pub end: usize,
    pub body: Body,
    /// Register the removed span is captured into (`""` = anonymous).
    pub capture: Option<String>,
    /// `(0-based line index, expected line tag)` for each address written as
    /// `N#hh`. Checked in `apply` before anything is spliced, so an address
    /// that points at the wrong line is refused instead of applied.
    pub anchors: Vec<(usize, String)>,
}

/// Whole-file operation. Mutually exclusive with span operations.
#[derive(Debug, PartialEq, Eq)]
pub enum FileOp {
    Move(String),
    Remove,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Patch {
    pub path: String,
    pub tag: String,
    pub ops: Vec<Op>,
    pub file_op: Option<FileOp>,
}

/// The `[path#TAG]` header of the single file section, without parsing the body.
/// Used by `mutation_target`, which must answer before the file is read.
pub fn header_path(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let (path, _) = inner.rsplit_once('#')?;
    (!path.is_empty()).then(|| path.to_string())
}

pub fn parse(text: &str) -> Result<Patch> {
    let mut lines = text.lines().enumerate().peekable();

    // --- header -----------------------------------------------------------
    let mut header = None;
    for (i, raw) in lines.by_ref() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Some(inner) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) else {
            bail!(
                "patch line {}: expected a [path#TAG] header, got `{line}`",
                i + 1
            );
        };
        let Some((path, tag)) = inner.rsplit_once('#') else {
            bail!("patch line {}: header `{line}` has no #TAG", i + 1);
        };
        if path.is_empty() || tag.is_empty() {
            bail!(
                "patch line {}: header `{line}` needs both a path and a #TAG",
                i + 1
            );
        }
        header = Some((path.to_string(), tag.to_string()));
        break;
    }
    let Some((path, tag)) = header else {
        bail!("empty patch: expected a [path#TAG] header");
    };

    // --- operations -------------------------------------------------------
    let mut ops: Vec<Op> = Vec::new();
    let mut file_op = None;
    while let Some((i, raw)) = lines.next() {
        let n = i + 1;
        let line = raw.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') {
            bail!(
                "patch line {n}: one file section per call — split `{trimmed}` into its own patch"
            );
        }
        if let Some(rest) = trimmed.strip_prefix('+') {
            bail!("patch line {n}: body line `+{rest}` does not follow a PUT");
        }

        let (verb, rest) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        let rest = rest.trim();
        // The model drops the `PUT` and writes the address alone often enough
        // that rejecting it is a measured cost: a patch that never lands burns a
        // whole round trip. `2.=2:` can only mean `PUT 2.=2:`, so read it that
        // way. The tool description still teaches the one canonical form — this
        // is tolerance at the parser, not a second grammar to advertise.
        let (verb, rest) = match verb {
            "REM" | "MV" | "CUT" | "PUT" => (verb, rest),
            addr if is_address(addr) => ("PUT", trimmed),
            other => bail!("patch line {n}: unknown operation `{other}` (PUT, CUT, MV, REM)"),
        };
        match verb {
            "REM" => {
                if !rest.is_empty() {
                    bail!("patch line {n}: REM takes no argument");
                }
                set_file_op(&mut file_op, FileOp::Remove, n)?;
            }
            "MV" => {
                if rest.is_empty() {
                    bail!("patch line {n}: MV needs a destination path");
                }
                set_file_op(&mut file_op, FileOp::Move(rest.to_string()), n)?;
            }
            "CUT" => {
                let (addr, reg) = split_register(rest);
                let (start, end, anchors) = parse_range(addr, n)?;
                ops.push(Op {
                    start,
                    end,
                    body: Body::Lines(Vec::new()),
                    capture: Some(reg.unwrap_or_default()),
                    anchors,
                });
            }
            "PUT" => {
                let (addr, reg) = split_register(rest);
                let marked = addr.ends_with(':');
                let addr = addr.trim_end_matches(':').trim();
                let (start, end, anchors) = parse_addr(addr, n)?;
                // `:` is the documented body marker, but a PUT already followed
                // by a `+` line means the same thing without it — the second
                // recurring rejection, and equally free to accept. A PUT with
                // neither still fails, so the "needs a body" error survives.
                let takes_body = marked
                    || (reg.is_none()
                        && lines
                            .peek()
                            .is_some_and(|(_, l)| l.trim_end().starts_with('+')));
                let body = match (takes_body, reg) {
                    (true, Some(_)) => {
                        bail!(
                            "patch line {n}: PUT takes either a `:` body or a @register, not both"
                        )
                    }
                    (true, None) => {
                        let mut body = Vec::new();
                        while let Some((_, next)) = lines.peek() {
                            let Some(text) = next.trim_end().strip_prefix('+') else {
                                break;
                            };
                            body.push(text.to_string());
                            lines.next();
                        }
                        Body::Lines(body)
                    }
                    (false, Some(reg)) => Body::Register(reg),
                    (false, None) => bail!(
                        "patch line {n}: PUT needs `:` followed by + lines, or a @register to paste"
                    ),
                };
                ops.push(Op {
                    start,
                    end,
                    body,
                    capture: None,
                    anchors,
                });
            }
            // `verb` was narrowed to the four operations above.
            _ => unreachable!(),
        }
    }

    if file_op.is_some() && !ops.is_empty() {
        bail!("MV/REM cannot be combined with line edits — send them as separate patches");
    }
    if file_op.is_none() && ops.is_empty() {
        bail!("patch has a header but no operations");
    }

    check_disjoint(&mut ops)?;
    Ok(Patch {
        path,
        tag,
        ops,
        file_op,
    })
}

fn set_file_op(slot: &mut Option<FileOp>, op: FileOp, n: usize) -> Result<()> {
    if slot.is_some() {
        bail!("patch line {n}: only one MV/REM per patch");
    }
    *slot = Some(op);
    Ok(())
}

/// Does this token address lines rather than name an operation? `<A`, `>A`,
/// `>$`, anything carrying the `.=` range separator, and a bare line number
/// (with or without its `#tag` anchor).
fn is_address(token: &str) -> bool {
    let token = token.trim_end_matches(':');
    let num = token.split_once('#').map_or(token, |(num, _)| num);
    token.starts_with('<')
        || token.starts_with('>')
        || token.contains(".=")
        || (!num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()))
}

/// Split a trailing `@register` off an address. `@` alone names the anonymous
/// register, the one an argument-less `CUT` fills.
fn split_register(rest: &str) -> (&str, Option<String>) {
    match rest.rsplit_once('@') {
        Some((addr, reg)) => (addr.trim(), Some(reg.trim().to_string())),
        None => (rest, None),
    }
}

/// An address and the anchors it carries, ready for [`Op`].
type Span = (usize, usize, Vec<(usize, String)>);

/// `A.=B` — an inclusive 1-based line range. A bare `N` is the range `N.=N`:
/// the third recurring slip, and the shortest form the model reaches for.
fn parse_range(addr: &str, n: usize) -> Result<Span> {
    let Some((a, b)) = addr.split_once(".=") else {
        let (a, tag) = match parse_point(addr, n)? {
            Some(point) => point,
            None => bail!("patch line {n}: expected a range `A.=B` or a line number, got `{addr}`"),
        };
        return Ok((a - 1, a, anchor(a, tag)));
    };
    let (a, ta) = point(a, n)?;
    let (b, tb) = point(b, n)?;
    if a > b {
        bail!("patch line {n}: range {a}.={b} runs backwards");
    }
    let anchors = [anchor(a, ta), anchor(b, tb)].concat();
    Ok((a - 1, b, anchors))
}

/// A `PUT` address: `A.=B` (replace), `<A` (insert before), `>A` / `>$` (after).
fn parse_addr(addr: &str, n: usize) -> Result<Span> {
    if let Some(a) = addr.strip_prefix('<') {
        let (a, tag) = point(a, n)?;
        return Ok((a - 1, a - 1, anchor(a, tag)));
    }
    if let Some(a) = addr.strip_prefix('>') {
        let a = a.trim();
        // `$` is end-of-file; resolved against the real length in `apply`.
        if a == "$" {
            return Ok((usize::MAX, usize::MAX, Vec::new()));
        }
        let (a, tag) = point(a, n)?;
        return Ok((a, a, anchor(a, tag)));
    }
    parse_range(addr, n)
}

/// The anchor list for a 1-based line number: empty unless the address quoted
/// the line's tag, so an un-tagged address behaves exactly as it did before.
fn anchor(line: usize, tag: Option<String>) -> Vec<(usize, String)> {
    tag.into_iter().map(|t| (line - 1, t)).collect()
}

/// The two hex digits `read` prints beside a line number.
fn is_tag(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `N` or `N#hh` — a line number and the tag `read` printed beside it.
fn parse_point(s: &str, n: usize) -> Result<Option<(usize, Option<String>)>> {
    let s = s.trim();
    let (num, tag) = match s.split_once('#') {
        // A tag is exactly the two hex digits `read` prints. Anything else is a
        // typo, most often `55#b0:=55#b0` — the range separator written `:=`
        // instead of `.=`, which would otherwise parse as one point carrying a
        // nonsense tag and be reported as a mismatch instead of bad grammar.
        Some((num, tag)) if is_tag(tag) => (num, Some(tag.to_ascii_lowercase())),
        Some((_, tag)) => bail!(
            "patch line {n}: `{tag}` is not a line tag — write `32#a7`, \
             and separate the two ends of a range with `.=`"
        ),
        None => (s, None),
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    Ok(Some((line_no(num, n)?, tag)))
}

/// [`parse_point`] where a non-number is an error rather than a fall-through.
fn point(s: &str, n: usize) -> Result<(usize, Option<String>)> {
    match parse_point(s, n)? {
        Some(point) => Ok(point),
        None => bail!("patch line {n}: `{}` is not a line number", s.trim()),
    }
}

fn line_no(s: &str, n: usize) -> Result<usize> {
    match s.trim().parse::<usize>() {
        Ok(0) => bail!("patch line {n}: line numbers are 1-based, got 0"),
        Ok(v) => Ok(v),
        Err(_) => bail!("patch line {n}: `{s}` is not a line number"),
    }
}

/// Addresses are all relative to the original file, so two operations may not
/// touch the same lines — that would make the result depend on apply order.
fn check_disjoint(ops: &mut [Op]) -> Result<()> {
    let mut order: Vec<(usize, usize)> = ops.iter().map(|o| (o.start, o.end)).collect();
    order.sort_unstable();
    for w in order.windows(2) {
        let (prev, cur) = (w[0], w[1]);
        if cur.0 < prev.1 || prev == cur {
            bail!(
                "overlapping operations at lines {}-{} and {}-{}: each line may be touched once",
                prev.0 + 1,
                prev.1,
                cur.0 + 1,
                cur.1
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Patch {
        parse(s).expect("valid patch")
    }

    #[test]
    fn header_is_required_and_yields_path_and_tag() {
        let patch = p("[src/lib.rs#4F2A]\nREM\n");
        assert_eq!(patch.path, "src/lib.rs");
        assert_eq!(patch.tag, "4F2A");
        assert_eq!(patch.file_op, Some(FileOp::Remove));
        assert_eq!(
            header_path("[src/lib.rs#4F2A]\nREM\n").as_deref(),
            Some("src/lib.rs")
        );

        assert!(parse("PUT <1:\n+x\n").is_err(), "no header");
        assert!(parse("[src/lib.rs]\nREM\n").is_err(), "no tag");
        assert!(parse("").is_err(), "empty");
    }

    #[test]
    fn addresses_map_to_half_open_spans() {
        assert_eq!(p("[f#T]\nPUT 3.=5:\n+x\n").ops[0].start, 2);
        assert_eq!(p("[f#T]\nPUT 3.=5:\n+x\n").ops[0].end, 5);
        // insert before line 3 is a zero-width span at index 2
        let before = &p("[f#T]\nPUT <3:\n+x\n").ops[0];
        assert_eq!((before.start, before.end), (2, 2));
        // insert after line 3 is a zero-width span at index 3
        let after = &p("[f#T]\nPUT >3:\n+x\n").ops[0];
        assert_eq!((after.start, after.end), (3, 3));
    }

    #[test]
    fn body_lines_are_collected_until_a_non_plus_line() {
        let patch = p("[f#T]\nPUT <1:\n+alpha\n+\n+beta\nCUT 9.=9\n");
        assert_eq!(
            patch.ops[0].body,
            Body::Lines(vec!["alpha".into(), "".into(), "beta".into()])
        );
        assert_eq!(patch.ops[1].capture.as_deref(), Some(""));
    }

    #[test]
    fn registers_round_trip_through_cut_and_put() {
        let patch = p("[f#T]\nCUT 1.=2 @blk\nPUT >9 @blk\n");
        assert_eq!(patch.ops[0].capture.as_deref(), Some("blk"));
        assert_eq!(patch.ops[1].body, Body::Register("blk".into()));
    }

    /// The two forms the model actually emitted in the A/B: a bare address with
    /// no `PUT`, and a `PUT` with no trailing `:`. Both must land, and must land
    /// on exactly the span the canonical spelling produces.
    #[test]
    fn tolerates_the_recurring_grammar_slips() {
        let canonical = p("[f#T]\nPUT 2.=2:\n+x\n");
        for slip in [
            "[f#T]\n2.=2:\n+x\n",
            "[f#T]\nPUT 2.=2\n+x\n",
            "[f#T]\n2.=2\n+x\n",
            // A bare line number is the single-line range.
            "[f#T]\nPUT 2:\n+x\n",
            "[f#T]\nPUT 2\n+x\n",
            "[f#T]\n2:\n+x\n",
            "[f#T]\n2\n+x\n",
        ] {
            let got = p(slip);
            assert_eq!(got.ops, canonical.ops, "should match canonical: {slip:?}");
        }
        // Insert addresses lose the verb too.
        assert_eq!(p("[f#T]\n<3:\n+x\n").ops, p("[f#T]\nPUT <3:\n+x\n").ops);
        assert_eq!(p("[f#T]\n>$\n+x\n").ops, p("[f#T]\nPUT >$:\n+x\n").ops);
        // Register pastes keep working — no `+` line follows, so no body is stolen.
        let reg = p("[f#T]\nCUT 1.=2 @blk\n>9 @blk\n");
        assert_eq!(reg.ops[1].body, Body::Register("blk".into()));
        // CUT takes the bare number too.
        assert_eq!(p("[f#T]\nCUT 3 @r\n").ops, p("[f#T]\nCUT 3.=3 @r\n").ops);
    }

    #[test]
    fn line_tags_ride_along_with_the_address() {
        // Every address form carries its tags, as 0-based indices.
        assert_eq!(
            p("[f#T]\nPUT 2#a7.=4#1c:\n+x\n").ops[0].anchors,
            vec![(1, "a7".to_string()), (3, "1c".to_string())]
        );
        assert_eq!(
            p("[f#T]\nPUT <3#0f:\n+x\n").ops[0].anchors,
            vec![(2, "0f".to_string())]
        );
        assert_eq!(
            p("[f#T]\nPUT >3#0f:\n+x\n").ops[0].anchors,
            vec![(2, "0f".to_string())]
        );
        assert_eq!(
            p("[f#T]\nCUT 3#0f @r\n").ops[0].anchors,
            vec![(2, "0f".to_string())]
        );
        // A tagged bare number is still an address without the verb.
        assert_eq!(p("[f#T]\n2#a7:\n+x\n").ops, p("[f#T]\nPUT 2#a7:\n+x\n").ops);
        // Untagged addresses carry nothing, so nothing is checked.
        assert!(p("[f#T]\nPUT 2.=4:\n+x\n").ops[0].anchors.is_empty());
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "[f#T]\nPUT 5.=1:\n+x\n",           // backwards range
            "[f#T]\nPUT 0.=1:\n+x\n",           // 0 is not a line
            "[f#T]\nPUT 0:\n+x\n",              // nor is a bare 0
            "[f#T]\nPUT 2x:\n+x\n",             // not a number at all
            "[f#T]\nPUT 2#:\n+x\n",             // `#` with no line tag
            "[f#T]\nPUT 2#zz:\n+x\n",           // a tag that is not two hex digits
            "[f#T]\nPUT 2#b0:=2#b0:\n+x\n",     // range written `:=` instead of `.=`
            "[f#T]\nZAP 1.=1\n",                // unknown verb
            "[f#T]\nPUT 1.=2\n",                // neither body nor register
            "[f#T]\nPUT 1.=2: @r\n+x\n",        // both body and register
            "[f#T]\n+orphan\n",                 // body without a PUT
            "[f#T]\nPUT 1.=3:\n+x\nCUT 2.=2\n", // overlap
            "[f#T]\nREM\nPUT <1:\n+x\n",        // file op mixed with edits
            "[f#T]\n",                          // header only
            "[f#T]\nREM\n[g#T]\nREM\n",         // two sections
        ] {
            assert!(parse(bad).is_err(), "should reject: {bad:?}");
        }
    }
}
