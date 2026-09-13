//! `patch` — hash-anchored line edits (opt-in, `SIRBONE_HASHLINE=1`).
//!
//! `edit` makes the model re-type the code it wants to change: `old_string`
//! must be copied verbatim, which is the bulk of its output tokens and the
//! source of the "string not found" retry loop. `patch` replaces the quote
//! with an address — the model cites line numbers it already has from `read`.
//!
//! Correctness comes from the `[path#TAG]` header: `TAG` is the content tag
//! `read` printed, so a patch written against a stale view is rejected before
//! anything is written. Registered *instead of* `edit` when the flag is on —
//! two competing edit tools would only confuse the model.

pub mod apply;
pub mod parse;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{
    edit::commit_edit,
    freshness::{tag_of, ReadStamps},
    undo::UndoStore,
    TypedTool,
};
use parse::FileOp;

/// True when the hashline experiment is on (`SIRBONE_HASHLINE=1`). `read` reads
/// the same flag to emit `[path#TAG]` headers and line numbers.
pub fn enabled() -> bool {
    std::env::var_os("SIRBONE_HASHLINE").is_some()
}

#[derive(Deserialize, JsonSchema)]
pub struct PatchInput {
    /// The patch text: a `[path#TAG]` header followed by operations.
    pub patch: String,
}

pub struct PatchTool {
    pub undo: UndoStore,
    pub stamps: ReadStamps,
}

#[async_trait]
impl TypedTool for PatchTool {
    type Input = PatchInput;

    fn name(&self) -> &'static str {
        "patch"
    }

    fn description(&self) -> &'static str {
        // The anchor paragraph is dropped under `patch:anchors`, where `read`
        // prints no line tags: documenting a form the model cannot observe would
        // make the control arm a third thing instead of the pre-anchor tool.
        const GRAMMAR: &str =
            "Edit a file by line address instead of by quoting it. One file per call.\n\
             Format — a header, then operations, in any order:\n\
             [path/to/file.rs#TAG]   TAG is the tag `read` printed for that file; a\n\
                                     stale TAG is rejected, so re-read after editing.\n\
             PUT A.=B:               replace lines A through B (inclusive) with the + lines below\n\
             PUT <A:                 insert the + lines before line A\n\
             PUT >A:                 insert the + lines after line A (`>$` = end of file)\n\
             CUT A.=B                delete lines A through B\n\
             CUT A.=B @name          delete them and capture the block into register @name\n\
             PUT <A @name            paste register @name (also valid with >A and A.=B)\n\
             MV new/path.rs          rename the file (alone in its patch)\n\
             REM                     delete the file (alone in its patch)\n\
             +text                   one literal line of a PUT body; `+` alone is a blank line\n";
        const ANCHORS: &str =
            "Any line number may carry the two-digit tag `read` printed beside it\n\
             (`read` shows `32#a7| code`, so write `PUT 32#a7.=34#1c:`). The patch is\n\
             refused if line 32 is no longer that line — quote the tags whenever you\n\
             have them: the header TAG only proves the file is unchanged, the line\n\
             tags prove you addressed the lines you meant.\n";
        const TAIL: &str =
            "Every address refers to the file as you read it — edits never shift each\n\
             other, and no two operations may touch the same line. Indentation after\n\
             `+` is preserved exactly.";
        static FULL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        static BARE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        match crate::ablate::patch_anchors_disabled() {
            true => BARE.get_or_init(|| format!("{GRAMMAR}{TAIL}")),
            false => FULL.get_or_init(|| format!("{GRAMMAR}{ANCHORS}{TAIL}")),
        }
    }

    fn mutation_target(&self, args: &serde_json::Value) -> Option<std::path::PathBuf> {
        args.get("patch")
            .and_then(|v| v.as_str())
            .and_then(parse::header_path)
            .map(std::path::PathBuf::from)
    }

    async fn run(&self, input: PatchInput) -> Result<String> {
        // Split so every rejection is counted once, wherever it comes from —
        // grammar, stale tag, or a bad address. A patch that never lands costs
        // a whole round trip, so the reject count is what tells an A/B whether
        // the format is actually cheaper than re-quoting with `edit`.
        let result = self.apply(input).await;
        crate::telemetry::add(
            if result.is_ok() {
                &crate::telemetry::PATCH_APPLIES
            } else {
                &crate::telemetry::PATCH_REJECTS
            },
            1,
        );
        result
    }
}

impl PatchTool {
    async fn apply(&self, input: PatchInput) -> Result<String> {
        let patch = parse::parse(&input.patch)?;
        let content = tokio::fs::read_to_string(&patch.path)
            .await
            .with_context(|| format!("cannot read {}", patch.path))?;

        self.stamps.guard(&patch.path, &content).await?;
        let actual = tag_of(&content);
        if actual != patch.tag {
            bail!(
                "tag mismatch for {}: patch says #{} but the file is #{actual} — re-read it and \
                 rewrite the patch against the current line numbers",
                patch.path,
                patch.tag
            );
        }

        match patch.file_op {
            Some(FileOp::Remove) => {
                self.undo.snapshot(&patch.path).await;
                tokio::fs::remove_file(&patch.path)
                    .await
                    .with_context(|| format!("cannot remove {}", patch.path))?;
                Ok(format!("Removed {}.", patch.path))
            }
            Some(FileOp::Move(dest)) => {
                if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
                    bail!("{dest} already exists — move aborted");
                }
                self.undo.snapshot(&patch.path).await;
                self.undo.snapshot(&dest).await;
                tokio::fs::rename(&patch.path, &dest)
                    .await
                    .with_context(|| format!("cannot move {} to {dest}", patch.path))?;
                self.stamps.record(&dest, &content).await;
                Ok(format!("Moved {} to {dest}.", patch.path))
            }
            None => {
                let new_content = apply::apply(&patch, &content)?;
                let result = commit_edit(&self.undo, &patch.path, &content, &new_content).await?;
                self.stamps.record(&patch.path, &new_content).await;
                Ok(format!("{result}\n\nNew tag: #{}", tag_of(&new_content)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tool() -> PatchTool {
        PatchTool {
            undo: UndoStore::default(),
            stamps: ReadStamps::default(),
        }
    }

    async fn file(content: &str) -> tempfile::NamedTempFile {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write!(tmp, "{content}").expect("write");
        tmp
    }

    #[tokio::test]
    async fn applies_a_patch_and_reports_the_new_tag() {
        let tmp = file("one\ntwo\nthree\n").await;
        let path = tmp.path().to_str().unwrap().to_string();
        let tag = tag_of("one\ntwo\nthree\n");
        let out = tool()
            .run(PatchInput {
                patch: format!("[{path}#{tag}]\nPUT 2.=2:\n+TWO\n"),
            })
            .await
            .expect("patch applies");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nTWO\nthree\n");
        assert!(out.contains("New tag: #"), "{out}");
    }

    #[tokio::test]
    async fn a_stale_tag_is_rejected_without_writing() {
        let tmp = file("one\ntwo\n").await;
        let path = tmp.path().to_str().unwrap().to_string();
        let err = tool()
            .run(PatchInput {
                patch: format!("[{path}#0000]\nPUT 1.=1:\n+X\n"),
            })
            .await
            .expect_err("stale tag");
        assert!(err.to_string().contains("tag mismatch"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }

    #[tokio::test]
    async fn move_and_remove_are_file_level() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dest = dir.path().join("b.txt");
        std::fs::write(&src, "x\n").unwrap();
        let tag = tag_of("x\n");

        tool()
            .run(PatchInput {
                patch: format!("[{}#{tag}]\nMV {}\n", src.display(), dest.display()),
            })
            .await
            .expect("move");
        assert!(!src.exists() && dest.exists());

        tool()
            .run(PatchInput {
                patch: format!("[{}#{tag}]\nREM\n", dest.display()),
            })
            .await
            .expect("remove");
        assert!(!dest.exists());
    }

    #[test]
    fn mutation_target_reads_the_header() {
        let args = serde_json::json!({"patch": "[src/lib.rs#4F2A]\nREM\n"});
        assert_eq!(
            tool().mutation_target(&args).as_deref(),
            Some(std::path::Path::new("src/lib.rs"))
        );
        assert!(tool()
            .mutation_target(&serde_json::json!({"patch": "garbage"}))
            .is_none());
    }
}
