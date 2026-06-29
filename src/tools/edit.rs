use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use similar::TextDiff;

use super::{freshness::ReadStamps, undo::UndoStore, TypedTool};

#[derive(Deserialize, JsonSchema)]
pub struct EditInput {
    /// Path to the file to edit
    pub path: String,
    /// Exact string to find (must appear exactly once)
    pub old_string: String,
    /// Replacement string
    pub new_string: String,
}

pub struct EditTool {
    pub undo: UndoStore,
    pub stamps: ReadStamps,
}

#[async_trait]
impl TypedTool for EditTool {
    type Input = EditInput;

    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Replace a string in a file. Exact match preferred; falls back to \
         whitespace-tolerant line matching. old_string must match exactly once: \
         read the file first, copy old_string verbatim including indentation, \
         and include enough surrounding lines to make the match unique."
    }

    fn mutation_target(&self, args: &serde_json::Value) -> Option<std::path::PathBuf> {
        args.get("path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
    }

    async fn run(&self, input: EditInput) -> Result<String> {
        let content = tokio::fs::read_to_string(&input.path)
            .await
            .with_context(|| format!("cannot read {}", input.path))?;

        self.stamps.guard(&input.path, &content).await?;

        let new_content = apply_edit(&content, &input.old_string, &input.new_string)
            .with_context(|| format!("in {}", input.path))?;

        let result = commit_edit(&self.undo, &input.path, &content, &new_content).await?;
        self.stamps.record(&input.path, &new_content).await;
        Ok(result)
    }
}

/// Multi-pass replacement: exact substring first, then line-window matching that
/// tolerates trailing whitespace, then leading+trailing whitespace (re-indenting
/// the replacement to the file's indentation). Each pass requires a unique match.
pub(super) fn apply_edit(content: &str, old: &str, new: &str) -> Result<String> {
    match content.matches(old).count() {
        1 => return Ok(content.replacen(old, new, 1)),
        n if n > 1 => bail!("old_string matches {n} times — must be unique"),
        _ => {}
    }

    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let old_lines: Vec<&str> = old.lines().collect();
    if old_lines.is_empty() {
        bail!("old_string not found");
    }
    for trim_both in [false, true] {
        let eq = |a: &str, b: &str| {
            if trim_both {
                a.trim() == b.trim()
            } else {
                a.trim_end() == b.trim_end()
            }
        };
        let starts: Vec<usize> = (0..lines.len().saturating_sub(old_lines.len() - 1))
            .filter(|&i| {
                old_lines
                    .iter()
                    .enumerate()
                    .all(|(j, ol)| eq(lines[i + j], ol))
            })
            .collect();
        match starts[..] {
            [start] => {
                return Ok(splice_lines(
                    &lines,
                    start,
                    old_lines.len(),
                    old,
                    new,
                    trim_both,
                ))
            }
            [] => continue,
            _ => bail!(
                "old_string matches {} times (whitespace-insensitive) — must be unique",
                starts.len()
            ),
        }
    }
    bail!("old_string not found (tried exact and whitespace-tolerant matching)")
}

/// Replace `len` lines starting at `start` with `new`. When the match ignored
/// leading whitespace, transplant the file's indentation onto the replacement.
fn splice_lines(
    lines: &[&str],
    start: usize,
    len: usize,
    old: &str,
    new: &str,
    reindent: bool,
) -> String {
    let new_block = if reindent {
        let file_indent = indent_of(lines[start]);
        let old_indent = indent_of(old.lines().next().unwrap_or(""));
        new.lines()
            .map(|l| match l.strip_prefix(old_indent) {
                Some(rest) if !l.trim().is_empty() => format!("{file_indent}{rest}"),
                _ => l.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        new.to_string()
    };
    let mut out: String = lines[..start].concat();
    out.push_str(&new_block);
    if lines[start + len - 1].ends_with('\n') && !new_block.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&lines[start + len..].concat());
    out
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

pub(super) fn generate_diff(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    diff.unified_diff().context_radius(3).to_string()
}

/// Commit a file mutation shared by `edit` and `sed`: snapshot for undo, write,
/// then return the `Edited <path>.\n\n<diff>` envelope the front-ends parse.
/// Keeping the format string here means producers can't drift from each other.
pub(super) async fn commit_edit(
    undo: &UndoStore,
    path: &str,
    old: &str,
    new: &str,
) -> Result<String> {
    undo.snapshot(path).await;
    tokio::fs::write(path, new)
        .await
        .with_context(|| format!("cannot write {path}"))?;
    Ok(format!("Edited {path}.\n\n{}", generate_diff(old, new)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::{ReadInput, ReadTool};
    use std::io::Write;

    #[tokio::test]
    async fn edit_basic() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "hello world").unwrap();
        let tool = EditTool {
            undo: UndoStore::default(),
            stamps: ReadStamps::default(),
        };
        let result = tool
            .run(EditInput {
                path: tmp.path().to_str().unwrap().into(),
                old_string: "world".into(),
                new_string: "rust".into(),
            })
            .await
            .unwrap();
        let content = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(content.contains("rust"));
        assert!(!content.contains("world"));
        assert!(result.contains('+'), "diff should show added line");
        assert!(result.contains('-'), "diff should show removed line");
    }

    #[tokio::test]
    async fn edit_not_found() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "hello").unwrap();
        let tool = EditTool {
            undo: UndoStore::default(),
            stamps: ReadStamps::default(),
        };
        let result = tool
            .run(EditInput {
                path: tmp.path().to_str().unwrap().into(),
                old_string: "notexist".into(),
                new_string: "x".into(),
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn edit_ambiguous() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "foo foo").unwrap();
        let tool = EditTool {
            undo: UndoStore::default(),
            stamps: ReadStamps::default(),
        };
        let result = tool
            .run(EditInput {
                path: tmp.path().to_str().unwrap().into(),
                old_string: "foo".into(),
                new_string: "bar".into(),
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn edit_after_clean_read_passes() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "alpha").unwrap();
        let path: String = tmp.path().to_str().unwrap().into();
        let stamps = ReadStamps::default();
        ReadTool {
            stamps: stamps.clone(),
        }
        .run(ReadInput {
            path: path.clone(),
            offset: 1,
            limit: None,
        })
        .await
        .unwrap();
        let tool = EditTool {
            undo: UndoStore::default(),
            stamps,
        };
        let r = tool
            .run(EditInput {
                path,
                old_string: "alpha".into(),
                new_string: "beta".into(),
            })
            .await;
        assert!(r.is_ok(), "{r:?}");
    }

    #[tokio::test]
    async fn edit_rejects_when_file_changed_since_read() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "alpha").unwrap();
        let path: String = tmp.path().to_str().unwrap().into();
        let stamps = ReadStamps::default();
        ReadTool {
            stamps: stamps.clone(),
        }
        .run(ReadInput {
            path: path.clone(),
            offset: 1,
            limit: None,
        })
        .await
        .unwrap();
        // Something else mutates the file after the read.
        std::fs::write(&path, "alpha changed\n").unwrap();
        let tool = EditTool {
            undo: UndoStore::default(),
            stamps,
        };
        let err = tool
            .run(EditInput {
                path,
                old_string: "alpha".into(),
                new_string: "beta".into(),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed since you last read it"), "{err}");
    }

    #[tokio::test]
    async fn consecutive_edits_dont_trip_guard() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "one\ntwo").unwrap();
        let path: String = tmp.path().to_str().unwrap().into();
        let stamps = ReadStamps::default();
        ReadTool {
            stamps: stamps.clone(),
        }
        .run(ReadInput {
            path: path.clone(),
            offset: 1,
            limit: None,
        })
        .await
        .unwrap();
        let tool = EditTool {
            undo: UndoStore::default(),
            stamps,
        };
        tool.run(EditInput {
            path: path.clone(),
            old_string: "one".into(),
            new_string: "1".into(),
        })
        .await
        .unwrap();
        // Second edit relies on the post-apply stamp refresh, not a fresh read.
        let r = tool
            .run(EditInput {
                path,
                old_string: "two".into(),
                new_string: "2".into(),
            })
            .await;
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn apply_edit_exact_takes_priority() {
        let out = apply_edit("a\nb\nc\n", "b", "B").unwrap();
        assert_eq!(out, "a\nB\nc\n");
    }

    #[test]
    fn apply_edit_tolerates_trailing_whitespace() {
        // File has trailing spaces the model's quote lacks.
        let content = "fn main() {   \n    let x = 1;\t\n}\n";
        let out = apply_edit(
            content,
            "fn main() {\n    let x = 1;",
            "fn main() {\n    let x = 2;",
        )
        .unwrap();
        assert_eq!(out, "fn main() {\n    let x = 2;\n}\n");
    }

    #[test]
    fn apply_edit_reindents_on_leading_whitespace_mismatch() {
        // Model quoted the block without the file's 4-space indent.
        let content = "if x {\n    do_a();\n    do_b();\n}\n";
        let out = apply_edit(content, "do_a();\ndo_b();", "do_a();\ndo_c();").unwrap();
        assert_eq!(out, "if x {\n    do_a();\n    do_c();\n}\n");
    }

    #[test]
    fn apply_edit_fuzzy_ambiguous_errors() {
        // No exact match (old has a trailing space); trim-both matches twice.
        let content = "  x=1\ny\n    x=1\n";
        let err = apply_edit(content, "x=1 ", "x=2").unwrap_err().to_string();
        assert!(err.contains("whitespace-insensitive"), "{err}");
        assert!(err.contains("must be unique"), "{err}");
    }

    #[test]
    fn apply_edit_not_found_errors() {
        let err = apply_edit("a\nb\n", "zzz", "y").unwrap_err().to_string();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn apply_edit_match_at_eof_without_trailing_newline() {
        let out = apply_edit("a\nlast", "last", "LAST").unwrap();
        assert_eq!(out, "a\nLAST");
    }

    #[test]
    fn apply_edit_exact_duplicates_still_error() {
        let err = apply_edit("foo\nfoo\n", "foo", "bar")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be unique"), "{err}");
    }
}
