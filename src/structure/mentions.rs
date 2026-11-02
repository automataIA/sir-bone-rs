//! Where a symbol mention sits: code, a comment, or a string literal.
//!
//! A lexer, not a parser: it only finds comment and string spans, which is
//! all a whole-word match needs to be classified. Opt-in via `SIRBONE_MENTIONS`
//! while the two ways of showing it to the model are A/B'd: `tags` prefixes
//! non-code lines in the references view, `filter` adds `code_only` to
//! `code_map find_references`.

use super::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mention {
    Code,
    Comment,
    Str,
}

impl Mention {
    /// The prefix the `tags` mode puts on a non-code line.
    pub fn tag(self) -> Option<&'static str> {
        match self {
            Mention::Code => None,
            Mention::Comment => Some("[comment]"),
            Mention::Str => Some("[string]"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Tags,
    Filter,
}

/// `SIRBONE_MENTIONS`: `tags`, `filter`, or unset (the shipped behaviour).
pub fn mode() -> Option<Mode> {
    match std::env::var("SIRBONE_MENTIONS").as_deref() {
        Ok("tags") => Some(Mode::Tags),
        Ok("filter") => Some(Mode::Filter),
        _ => None,
    }
}

/// Comment and string spans of `src` as `(start, end, kind)` byte ranges, in
/// order. Unterminated tokens run to the end of the file.
pub fn spans(lang: Lang, src: &str) -> Vec<(usize, usize, Mention)> {
    let b = src.as_bytes();
    let (line_comment, block, quotes): (&[u8], bool, &[u8]) = match lang {
        Lang::Rust => (b"//", true, b"\""),
        Lang::JsTs => (b"//", true, b"\"'`"),
        Lang::Python => (b"#", false, b"\"'"),
        // A double-quoted SQL name is an identifier, not a string.
        Lang::Sql => (b"--", true, b"'"),
    };
    let find = |from: usize, pat: &[u8]| {
        b[from..]
            .windows(pat.len())
            .position(|w| w == pat)
            .map_or(b.len(), |p| from + p + pat.len())
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let rest = &b[i..];
        let (end, kind) = if rest.starts_with(line_comment) {
            (find(i, b"\n"), Mention::Comment)
        } else if block && rest.starts_with(b"/*") {
            (block_end(b, i, lang == Lang::Rust), Mention::Comment)
        } else if let Some(end) = (lang == Lang::Rust).then(|| raw_string_end(b, i)).flatten() {
            (end, Mention::Str)
        } else if lang == Lang::Python && (rest.starts_with(b"\"\"\"") || rest.starts_with(b"'''"))
        {
            (find(i + 3, &rest[..3]), Mention::Str)
        } else if quotes.contains(&b[i]) {
            (quoted_end(b, i), Mention::Str)
        } else if let Some(end) = (lang == Lang::Rust && b[i] == b'\'')
            .then(|| char_literal_end(src, i))
            .flatten()
        {
            (end, Mention::Str)
        } else {
            i += 1;
            continue;
        };
        out.push((i, end, kind));
        i = end;
    }
    out
}

/// Where the mention starting at byte `pos` sits, given [`spans`].
pub fn kind_at(spans: &[(usize, usize, Mention)], pos: usize) -> Mention {
    let i = spans.partition_point(|s| s.1 <= pos);
    spans
        .get(i)
        .filter(|s| s.0 <= pos)
        .map_or(Mention::Code, |s| s.2)
}

/// End of a `/* */` comment; Rust nests them.
fn block_end(b: &[u8], start: usize, nested: bool) -> usize {
    let (mut depth, mut i) = (0usize, start);
    while i + 1 < b.len() {
        match &b[i..i + 2] {
            b"/*" if nested || depth == 0 => (depth, i) = (depth + 1, i + 2),
            b"*/" => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return i;
                }
            }
            _ => i += 1,
        }
    }
    b.len()
}

/// End of a string opened by the quote at `start`, honouring backslash escapes
/// (an SQL `''` closes and reopens, which classifies the same).
fn quoted_end(b: &[u8], start: usize) -> usize {
    let q = b[start];
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == q => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// `r"…"`, `r#"…"#`, `br"…"` at `start`, not inside an identifier.
fn raw_string_end(b: &[u8], start: usize) -> Option<usize> {
    if start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_') {
        return None;
    }
    let r = start + usize::from(b[start] == b'b');
    if b.get(r) != Some(&b'r') {
        return None;
    }
    let hashes = b[r + 1..].iter().take_while(|&&c| c == b'#').count();
    let open = r + 1 + hashes;
    if b.get(open) != Some(&b'"') {
        return None;
    }
    let close: Vec<u8> = std::iter::once(b'"')
        .chain(std::iter::repeat_n(b'#', hashes))
        .collect();
    Some(
        b[open + 1..]
            .windows(close.len())
            .position(|w| w == close.as_slice())
            .map_or(b.len(), |p| open + 1 + p + close.len()),
    )
}

/// `'x'` or `'\n'` at `start`; `None` for a lifetime or label (`'a`).
fn char_literal_end(src: &str, start: usize) -> Option<usize> {
    let body = &src[start + 1..];
    let len = match body.strip_prefix('\\') {
        Some(esc) => 1 + esc.find('\'').filter(|&n| n <= 10)?,
        None => body.chars().next()?.len_utf8(),
    };
    (body.as_bytes().get(len) == Some(&b'\'')).then_some(start + 1 + len + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(lang: Lang, src: &str, word: &str) -> Vec<Mention> {
        let s = spans(lang, src);
        src.match_indices(word)
            .map(|(p, _)| kind_at(&s, p))
            .collect()
    }

    #[test]
    fn rust_comments_strings_and_lifetimes() {
        let src = "/// doc W\nfn f<'a>(x: &'a W) -> char { let _ = \"W \\\" W\"; \
                   let _ = r#\"W \"# ; /* W /* W */ W */ 'W' } // W\nW";
        use Mention::*;
        assert_eq!(
            kinds(Lang::Rust, src, "W"),
            [Comment, Code, Str, Str, Str, Comment, Comment, Comment, Str, Comment, Code]
        );
    }

    #[test]
    fn python_and_sql() {
        use Mention::*;
        assert_eq!(
            kinds(Lang::Python, "x = W # W\n'''W\nW''' + \"W\"", "W"),
            [Code, Comment, Str, Str, Str]
        );
        assert_eq!(
            kinds(Lang::Sql, "select \"W\", 'W' -- W\nfrom W", "W"),
            [Code, Str, Comment, Code]
        );
    }
}
