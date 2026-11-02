//! On-demand navigation views. They share one byte budget, not nested truncation.
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Result};

use super::{rel, CodeMapInput};
use crate::structure::{self, impact, rust_syntax, Index, Lang};

struct Budget {
    text: String,
    limit: usize,
    omitted: usize,
    lines: usize,
}

impl Budget {
    fn new(input: &CodeMapInput) -> Result<Self> {
        let limit = input.max_bytes.unwrap_or(12_000);
        if !(512..=24_000).contains(&limit) {
            bail!("max_bytes must be in 512..=24000");
        }
        Ok(Self {
            text: String::new(),
            limit,
            omitted: 0,
            lines: 0,
        })
    }

    fn push(&mut self, record: impl AsRef<str>) {
        let record = record.as_ref();
        let lines = record.lines().count();
        // Leave room for the omission notice; keep below the tool's global line cap.
        if self.text.len() + record.len() + 97 <= self.limit && self.lines + lines < 1900 {
            self.text.push_str(record);
            self.text.push('\n');
            self.lines += lines;
        } else {
            self.omitted += 1;
        }
    }

    fn finish(mut self) -> String {
        if self.omitted > 0 {
            self.text.push_str(&format!(
                "[budget: {} record(s) omitted; narrow the query or raise max_bytes]\n",
                self.omitted
            ));
        }
        self.text
    }
}

fn depth(input: &CodeMapInput) -> Result<usize> {
    let depth = input.max_depth.unwrap_or(3);
    if depth > 10 {
        bail!("max_depth must be in 0..=10");
    }
    Ok(depth)
}

fn relative_path(root: &Path, input: &str) -> Result<PathBuf> {
    let path = Path::new(input);
    if input.trim().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("expected a root-relative file path without '..': {input}");
    }
    let normalized: PathBuf = path
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .collect();
    if normalized.as_os_str().is_empty() {
        bail!("expected a file path: {input}");
    }
    Ok(root.join(normalized))
}

fn witness(root: &Path, chain: &[PathBuf]) -> String {
    chain
        .iter()
        .map(|p| rel(root, p))
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn test_records(out: &mut Budget, root: &Path, tests: &[impact::TestCandidate]) {
    if tests.is_empty() {
        out.push("[tests] No candidates found within this depth; this does not establish that a change is safe.");
    }
    for test in tests {
        out.push(format!(
            "[test hop={}] {}:{} via {}",
            test.chain.len() - 1,
            rel(root, &test.path),
            test.line,
            witness(root, &test.chain)
        ));
    }
}

const ADVISORY: &str = "Advisory only: textual file dependencies and test conventions can miss or over-report tests. Ambiguous names, methods, macros, deleted definitions and runtime dependencies are not resolved; keep the normal test gate.";

pub(super) fn affected(root: &Path, index: &Index, input: &CodeMapInput) -> Result<String> {
    if input.files.is_empty() || input.files.len() > 64 {
        bail!("op=affected_tests requires 1..=64 changed `files`");
    }
    let depth = depth(input)?;
    let mut out = Budget::new(input)?;
    out.push(ADVISORY);
    out.push(format!("Affected test candidates (max_depth={depth})."));
    let seeds: BTreeSet<PathBuf> = input
        .files
        .iter()
        .map(|p| relative_path(root, p))
        .collect::<Result<_>>()?;
    for path in &seeds {
        if !index.files.contains_key(path) {
            out.push(format!(
                "[unindexed] {}: missing, deleted, ignored or unsupported; impact unknown",
                rel(root, path)
            ));
        }
    }
    let seeds: Vec<_> = seeds.into_iter().collect();
    let edges = structure::graph_cached(root, index);
    test_records(
        &mut out,
        root,
        &impact::affected_tests(root, index, &edges, &seeds, depth),
    );
    Ok(out.finish())
}

pub(super) fn inspect(root: &Path, index: &Index, input: &CodeMapInput) -> Result<String> {
    let path = relative_path(
        root,
        input
            .path
            .as_deref()
            .ok_or_else(|| anyhow!("op=inspect requires `path`"))?,
    )?;
    if index.files.get(&path).and_then(|c| c.data.lang) != Some(Lang::Rust) {
        bail!("op=inspect requires an indexed Rust source file");
    }
    let mut out = Budget::new(input)?;
    let source = structure::read_source(&path)
        .ok_or_else(|| anyhow!("source unreadable, binary or oversized lines"))?;
    out.push("Rust syntax only: function bodies exclude comments/strings. Calls are syntactic occurrences, not resolved dependencies; methods/indirect calls are unresolved and macros are unexpanded. cfg is not evaluated.");
    let functions = match rust_syntax::parse(&source) {
        Ok(functions) => functions,
        Err(error) => {
            out.push(format!(
                "[textual fallback] Rust parse failed: {error}. No AST call evidence available."
            ));
            let matcher = input
                .symbol
                .as_deref()
                .map(regex::escape)
                .map(|s| regex::Regex::new(&format!(r"\b{s}\b")))
                .transpose()?;
            for decl in structure::outline(Lang::Rust, &source) {
                if matcher.as_ref().is_none_or(|re| re.is_match(&decl.sig)) {
                    out.push(format!(
                        "[textual declaration] {}:{} {}",
                        rel(root, &path),
                        decl.line,
                        decl.sig
                    ));
                }
            }
            return Ok(out.finish());
        }
    };
    let mut matched = 0;
    for function in functions {
        if input
            .symbol
            .as_deref()
            .is_some_and(|s| s != function.name && Some(s) != function.name.rsplit("::").next())
        {
            continue;
        }
        matched += 1;
        out.push(format!(
            "[function] {}:{}-{} ::{}",
            rel(root, &path),
            function.start_line,
            function.end_line,
            function.name
        ));
        for call in function.calls {
            out.push(format!(
                "[{} in {}] {}:{} {}",
                call.kind,
                function.name,
                rel(root, &path),
                call.line,
                call.name
            ));
        }
    }
    if matched == 0 {
        out.push("No matching Rust functions found.");
    }
    Ok(out.finish())
}

pub(super) fn context(
    root: &Path,
    index: &Index,
    input: &CodeMapInput,
    symbol: &str,
) -> Result<String> {
    let depth = depth(input)?;
    let mut out = Budget::new(input)?;
    out.push(ADVISORY);
    out.push(format!("Context for `{symbol}` (max_depth={depth})."));
    let seeds = structure::declarations(index, symbol);
    if seeds.is_empty() {
        out.push("No indexed declaration found; references below are textual evidence only.");
    }
    let mut seen = BTreeSet::new();
    for path in &seeds {
        let cache = &index.files[path];
        if let (Some(lang), Some(source)) = (cache.data.lang, structure::read_source(path)) {
            let signatures: BTreeSet<_> = cache
                .data
                .defs
                .iter()
                .zip(&cache.data.sigs)
                .filter(|(name, _)| *name == symbol)
                .map(|(_, sig)| sig)
                .collect();
            for decl in structure::outline(lang, &source) {
                if signatures.contains(&decl.sig) && seen.insert((path.clone(), decl.line)) {
                    out.push(format!(
                        "[declaration] {}:{} {}",
                        rel(root, path),
                        decl.line,
                        decl.sig
                    ));
                }
            }
        }
    }
    let edges = structure::graph_cached(root, index);
    let tests = impact::affected_tests(root, index, &edges, &seeds, depth);
    test_records(&mut out, root, &tests);
    let refs = structure::find_reference_lines(root, symbol, 3, false);
    // Bound breadth explicitly; list omitted-file counts even if the byte budget fits.
    let mut shown = 0;
    for slot in 0..3 {
        for file in &refs {
            if let Some((line, source)) = file.lines.get(slot) {
                if shown < 40 && seen.insert((file.path.clone(), *line)) {
                    out.push(format!(
                        "[reference] {}:{line} {source}",
                        rel(root, &file.path)
                    ));
                    shown += 1;
                }
            }
        }
    }
    let total = refs.iter().map(|f| f.lines.len() + f.extra).sum::<usize>();
    if total > seen.len() {
        out.push(format!(
            "[references] {} additional mention(s) omitted; use find_references or grep to expand",
            total - seen.len()
        ));
    }
    for reach in impact::reverse_paths(&edges, &seeds, depth)
        .into_iter()
        .filter(|r| r.chain.len() > 1)
    {
        if tests.iter().any(|t| t.path == reach.path) {
            continue;
        }
        out.push(format!(
            "[dependent hop={}] {} via {}",
            reach.chain.len() - 1,
            rel(root, &reach.path),
            witness(root, &reach.chain)
        ));
    }
    Ok(out.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        code_map::{CodeMapTool, Op},
        TypedTool,
    };

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("tests")).unwrap();
        for (path, source) in [
            ("lib.rs", "pub fn alpha() {}\n#[cfg(test)]\nmod local { #[test] fn check() { super::alpha(); } }\n"),
            ("service.rs", "pub fn consume() { alpha(); }\n"),
            ("tests/flows.rs", "#[test]\nfn integration() { consume(); }\n"),
            ("unused.rs", "pub fn unrelated() {}\n"),
        ] {
            std::fs::write(dir.path().join(path), source).unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn affected_reports_transitive_witness_and_depth_zero_tests() {
        let dir = fixture();
        let tool = CodeMapTool {
            root: dir.path().to_owned(),
        };
        let mut input: CodeMapInput = serde_json::from_value(serde_json::json!({
            "op":"affected_tests", "files":["./lib.rs"], "max_depth":2
        }))
        .unwrap();
        let result = tool.run(input).await.unwrap();
        assert!(
            result.contains(
                "[test hop=2] tests/flows.rs:1 via lib.rs -> service.rs -> tests/flows.rs"
            ),
            "{result}"
        );
        assert!(result.contains("[test hop=0] lib.rs:2"), "{result}");
        assert!(!result.contains("unused.rs"));
        input = CodeMapInput {
            op: Op::AffectedTests,
            files: vec!["tests/flows.rs".into()],
            max_depth: Some(0),
            ..Default::default()
        };
        let direct = tool.run(input).await.unwrap();
        assert!(direct.contains("[test hop=0] tests/flows.rs:1"), "{direct}");
        assert!(!direct.contains("[test hop=1]"));
    }

    #[tokio::test]
    async fn missing_file_is_unknown_and_invalid_paths_are_rejected() {
        let dir = fixture();
        let tool = CodeMapTool {
            root: dir.path().to_owned(),
        };
        let result = tool
            .run(CodeMapInput {
                op: Op::AffectedTests,
                files: vec!["deleted.rs".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.contains("[unindexed] deleted.rs"));
        assert!(result.contains("does not establish that a change is safe"));
        for path in ["../outside.rs", "/etc/passwd", "", "."] {
            assert!(tool
                .run(CodeMapInput {
                    op: Op::AffectedTests,
                    files: vec![path.into()],
                    ..Default::default()
                })
                .await
                .is_err());
        }
        assert!(tool
            .run(CodeMapInput {
                op: Op::AffectedTests,
                ..Default::default()
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn context_is_deterministic_deduplicated_and_includes_evidence() {
        let dir = fixture();
        let tool = CodeMapTool {
            root: dir.path().to_owned(),
        };
        let input = || CodeMapInput {
            op: Op::Context,
            symbol: Some("alpha".into()),
            ..Default::default()
        };
        let first = tool.run(input()).await.unwrap();
        assert_eq!(first, tool.run(input()).await.unwrap());
        assert!(first.contains("[declaration] lib.rs:1"), "{first}");
        assert!(!first.contains("[reference] lib.rs:1"));
        assert!(first.contains("[reference] service.rs:1"), "{first}");
        assert!(
            first.contains("tests/flows.rs:1 via lib.rs -> service.rs -> tests/flows.rs"),
            "{first}"
        );
    }

    #[tokio::test]
    async fn inspect_keeps_bodies_separate_and_exposes_fallback() {
        let dir = fixture();
        std::fs::write(dir.path().join("syntax.rs"), "mod a { fn same() { alpha(); let _ = \"fake()\"; } }\nmod b { fn same() { beta(); } }\n").unwrap();
        let tool = CodeMapTool {
            root: dir.path().to_owned(),
        };
        let input = || CodeMapInput {
            op: Op::Inspect,
            path: Some("syntax.rs".into()),
            symbol: Some("a::same".into()),
            ..Default::default()
        };
        let result = tool.run(input()).await.unwrap();
        assert!(result.contains("::a::same"));
        assert!(result.contains("syntax.rs:1 alpha"));
        assert!(!result.contains("beta"));
        assert!(!result.contains("fake"));
        std::fs::write(dir.path().join("syntax.rs"), "fn broken() {\n").unwrap();
        let fallback = tool.run(input()).await.unwrap();
        assert!(fallback.contains("[textual fallback]"), "{fallback}");
        assert!(tool
            .run(CodeMapInput {
                op: Op::Inspect,
                path: Some("missing.rs".into()),
                ..Default::default()
            })
            .await
            .is_err());
    }

    #[test]
    fn budget_keeps_complete_utf8_records_and_reports_omissions() {
        let input = CodeMapInput {
            max_bytes: Some(512),
            ..Default::default()
        };
        let mut out = Budget::new(&input).unwrap();
        for _ in 0..100 {
            out.push("èvidence 🦀");
        }
        let result = out.finish();
        assert!(result.len() <= 512);
        assert!(result.contains("record(s) omitted"));
        assert!(result
            .lines()
            .filter(|l| !l.starts_with("[budget"))
            .all(|l| l == "èvidence 🦀"));
        assert!(Budget::new(&CodeMapInput {
            max_bytes: Some(0),
            ..Default::default()
        })
        .is_err());
        assert!(depth(&CodeMapInput {
            max_depth: Some(11),
            ..Default::default()
        })
        .is_err());
    }

    #[tokio::test]
    async fn compact_context_preserves_notice_and_budget() {
        let dir = fixture();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn reverse_paths(edges: &[(PathBuf, PathBuf)], seeds: &[PathBuf], max_depth: usize) -> Vec<Reach> { vec![] }\n#[test]\nfn check() { reverse_paths(); }\n",
        )
        .unwrap();
        let tool = CodeMapTool {
            root: dir.path().to_owned(),
        };
        let result = tool
            .run(CodeMapInput {
                op: Op::Context,
                symbol: Some("reverse_paths".into()),
                max_bytes: Some(512),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.len() <= 512);
        assert!(result.contains("Advisory only:"));
        assert!(result.contains("[declaration] lib.rs:1 pub fn reverse_paths("));
        assert!(result.contains("record(s) omitted"));
    }
}
