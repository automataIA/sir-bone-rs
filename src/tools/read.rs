use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{
    freshness::{tag_of, ReadStamps},
    truncate::truncate_default,
    TypedTool,
};
use crate::structure;

/// Below this many lines a whole-file read is cheap enough that an outline only
/// costs a round-trip.
const OUTLINE_MIN_LINES: usize = 80;

/// True when the structural-read experiment is on (`SIRBONE_READ_OUTLINE=1`,
/// ablatable with `SIRBONE_DISABLE=read:outline`).
fn outline_enabled() -> bool {
    std::env::var_os("SIRBONE_READ_OUTLINE").is_some() && !crate::ablate::read_outline_disabled()
}

/// Render the declaration outline of a long source file plus the ranges it
/// elides, or `None` when the file is short, unsupported, or has no
/// declarations worth an outline.
fn outline_view(path: &str, content: &str) -> Option<String> {
    let total = content.lines().count();
    if total < OUTLINE_MIN_LINES {
        return None;
    }
    let lang = structure::lang_for_path(std::path::Path::new(path))?;
    let decls = structure::outline(lang, content);
    if decls.is_empty() {
        return None;
    }
    let width = total.to_string().len();
    let mut out = format!("{path} — outline ({total} lines, {} decls)\n", decls.len());
    // Every line that is not a declaration is elided; report those ranges so
    // the model asks for a slice instead of guessing what is in between.
    let mut gaps: Vec<String> = Vec::new();
    let mut cursor = 1usize;
    for d in &decls {
        if d.line > cursor {
            gaps.push(range_label(cursor, d.line - 1));
        }
        out.push_str(&format!("{:>width$} | {}\n", d.line, d.sig));
        cursor = d.line + 1;
    }
    if cursor <= total {
        gaps.push(range_label(cursor, total));
    }
    out.push_str(&format!(
        "elided: {} — read ONLY these ranges with offset+limit; never guess their contents.",
        gaps.join(", ")
    ));
    Some(out)
}

fn range_label(a: usize, b: usize) -> String {
    if a == b {
        a.to_string()
    } else {
        format!("{a}-{b}")
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadInput {
    /// Path to the file to read
    pub path: String,
    /// Starting line number, 1-indexed (default: 1)
    #[serde(default = "default_offset")]
    pub offset: u64,
    /// Maximum number of lines to read (default: all)
    pub limit: Option<u64>,
}

fn default_offset() -> u64 {
    1
}

#[derive(Default)]
pub struct ReadTool {
    pub stamps: ReadStamps,
}

#[async_trait]
impl TypedTool for ReadTool {
    type Input = ReadInput;

    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read file contents, optionally from a starting line with a line limit. \
         Prefer this over cat/head/tail via bash. For large files read the \
         relevant slice (offset + limit) instead of the whole file. When output \
         is truncated, the full text is saved to a file whose path is in the \
         truncation marker."
    }

    async fn run(&self, input: ReadInput) -> Result<String> {
        let content = tokio::fs::read_to_string(&input.path)
            .await
            .with_context(|| format!("cannot open {}", input.path))?;

        // Stamp the whole-file hash so a later edit can tell the file changed
        // underneath it, independent of which slice we return below.
        self.stamps.record(&input.path, &content).await;

        let whole_file = input.offset <= 1 && input.limit.is_none();
        if whole_file && outline_enabled() {
            if let Some(view) = outline_view(&input.path, &content) {
                let header = if super::patch::enabled() {
                    format!("[{}#{}]\n", input.path, tag_of(&content))
                } else {
                    String::new()
                };
                crate::telemetry::add(&crate::telemetry::READ_OUTLINES, 1);
                return Ok(truncate_default(header + &view));
            }
        }

        let start = input.offset.max(1) as usize;
        let slice = content
            .lines()
            .skip(start - 1)
            .take(input.limit.map_or(usize::MAX, |l| l as usize));

        // Under the hashline arm the model edits by line address, so it needs
        // the numbers and the content tag to quote back in the patch header.
        let output: String = if super::patch::enabled() {
            let width = content.lines().count().max(1).to_string().len();
            let anchors = !crate::ablate::patch_anchors_disabled();
            std::iter::once(format!("[{}#{}]\n", input.path, tag_of(&content)))
                .chain(slice.enumerate().map(|(i, l)| {
                    let n = start + i;
                    match anchors {
                        true => format!("{n:>width$}#{}| {l}\n", super::freshness::line_tag(l)),
                        false => format!("{n:>width$}| {l}\n"),
                    }
                }))
                .collect()
        } else {
            slice.flat_map(|l| [l, "\n"]).collect()
        };

        Ok(truncate_default(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn read_whole_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "line1\nline2\nline3").unwrap();
        let tool = ReadTool::default();
        let result = tool
            .run(ReadInput {
                path: tmp.path().to_str().unwrap().into(),
                offset: 1,
                limit: None,
            })
            .await
            .unwrap();
        assert!(result.contains("line1"));
        assert!(result.contains("line3"));
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for i in 1..=5 {
            writeln!(tmp, "line{i}").unwrap();
        }
        let tool = ReadTool::default();
        let result = tool
            .run(ReadInput {
                path: tmp.path().to_str().unwrap().into(),
                offset: 2,
                limit: Some(2),
            })
            .await
            .unwrap();
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
        assert!(!result.contains("line1"));
        assert!(!result.contains("line4"));
    }

    /// A Rust file long enough to trip `OUTLINE_MIN_LINES`, with two decls far
    /// apart so the elided ranges are non-trivial.
    fn long_rust_source() -> String {
        let mut s = String::from("pub fn alpha(x: u8) -> u8 {\n");
        s.push_str(&"    // filler\n".repeat(60));
        s.push_str("}\n\npub struct Beta;\n");
        s.push_str(&"// tail\n".repeat(30));
        s
    }

    #[test]
    fn outline_lists_decls_and_elided_ranges() {
        let view = outline_view("src/demo.rs", &long_rust_source()).expect("long rust file");
        assert!(view.contains("pub fn alpha(x: u8) -> u8"), "{view}");
        assert!(view.contains("pub struct Beta"), "{view}");
        assert!(view.contains("elided: 2-63"), "{view}");
        assert!(view.contains("offset+limit"), "{view}");
    }

    #[test]
    fn outline_declines_short_or_unsupported_files() {
        assert!(outline_view("src/demo.rs", "pub fn a() {}\n").is_none());
        let long_text = "just prose\n".repeat(200);
        assert!(outline_view("notes.txt", &long_text).is_none());
        // Supported extension, no declarations at all.
        assert!(outline_view("src/demo.rs", &long_text).is_none());
    }

    #[tokio::test]
    async fn outline_is_off_by_default() {
        let mut tmp = tempfile::Builder::new().suffix(".rs").tempfile().unwrap();
        write!(tmp, "{}", long_rust_source()).unwrap();
        let tool = ReadTool::default();
        let result = tool
            .run(ReadInput {
                path: tmp.path().to_str().unwrap().into(),
                offset: 1,
                limit: None,
            })
            .await
            .unwrap();
        assert!(
            result.contains("// filler"),
            "full contents without the flag"
        );
        assert!(!result.contains("elided:"));
    }
}
