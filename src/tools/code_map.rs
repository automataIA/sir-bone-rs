use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{truncate::truncate_default, TypedTool};
use crate::structure::{self, Index};

#[derive(Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Project symbol map: every supported file with the symbols it declares.
    #[default]
    List,
    /// Files that reference `symbol` (whole-word) — textual "who uses X".
    FindReferences,
    /// File-level call graph: files that reference `symbol` = who breaks if you
    /// change it (the definer file is listed separately).
    Callers,
    /// File-level call graph: other files the definer of `symbol` depends on.
    Callees,
    /// Whole-project file→file dependency graph (every edge at once).
    Graph,
    /// Transitive "who uses `symbol`": the upstream usage chain, by hop depth.
    Chain,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct CodeMapInput {
    /// "list" (default), "find_references", "callers", or "callees".
    #[serde(default)]
    pub op: Op,
    /// Symbol required by find_references / callers / callees.
    #[serde(default)]
    pub symbol: Option<String>,
}

/// Deterministic, no-LLM project structure index (Pass 0/1).
pub struct CodeMapTool {
    /// Workspace root (matches the cwd used for per-project state elsewhere).
    pub root: PathBuf,
}

#[async_trait]
impl TypedTool for CodeMapTool {
    type Input = CodeMapInput;

    fn name(&self) -> &'static str {
        "code_map"
    }

    fn description(&self) -> &'static str {
        "Deterministic project structure map (no LLM, no network). \
         op=\"list\" (default) returns each supported source file with the full \
         signatures (params + return types) of the classes/functions/structs it \
         declares — a fast project overview that usually spares reading the file. \
         op=\"find_references\" with `symbol` lists files that mention it (whole-word). \
         op=\"callers\" with `symbol` is the file-level call graph: which files \
         reference it = who breaks if you change it (the definer is listed apart). \
         op=\"callees\" with `symbol` lists other files the definer depends on. \
         op=\"graph\" returns the whole project's file→file dependency graph at once. \
         op=\"chain\" with `symbol` is the transitive \"who uses it\": the upstream usage \
         chain by hop depth (direct users, then their users, …) — the flow chart of \
         what depends on it across the codebase. \
         Honours .gitignore and skips node_modules/target/etc. Cached by file \
         mtime; callers/callees resolve cross-file edges at query time (heuristic, \
         conservative: matches by name, so a name in a comment/string can over-report)."
    }

    async fn run(&self, input: CodeMapInput) -> Result<String> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            // Refresh the cache (reparses only changed files), then answer.
            let index = structure::update(&root, Index::load(&root));
            index.save(&root)?;
            let require_symbol = |op: &str| {
                input
                    .symbol
                    .clone()
                    .ok_or_else(|| anyhow!("op={op} requires `symbol`"))
            };
            match input.op {
                Op::List => Ok(render_list(&root, &index)),
                Op::FindReferences => {
                    let symbol = require_symbol("find_references")?;
                    Ok(render_refs(
                        &root,
                        &structure::find_references(&root, &symbol),
                        &symbol,
                    ))
                }
                Op::Callers => {
                    let symbol = require_symbol("callers")?;
                    Ok(render_callers(
                        &root,
                        &structure::callers(&index, &symbol),
                        &symbol,
                    ))
                }
                Op::Callees => {
                    let symbol = require_symbol("callees")?;
                    Ok(render_callees(
                        &root,
                        &structure::callees(&index, &symbol),
                        &symbol,
                    ))
                }
                Op::Graph => Ok(render_graph(&root, &structure::graph_cached(&root, &index))),
                Op::Chain => {
                    let symbol = require_symbol("chain")?;
                    let edges = structure::graph_cached(&root, &index);
                    Ok(render_chain(
                        &root,
                        &structure::usage_chain(&index, &edges, &symbol),
                        &symbol,
                    ))
                }
            }
        })
        .await
        .context("code_map task panicked")?
        .map(truncate_default)
    }
}

/// Per file: the full declaration signatures it contains, one per indented
/// line (aider-style "repo map" skeleton), sorted, paths relative to root.
/// Falls back to names when a stale cache predates signature extraction.
fn render_list(root: &Path, index: &Index) -> String {
    if index.files.is_empty() {
        return "no supported source files found".into();
    }
    let mut rows: Vec<(String, &structure::FileStructure)> = index
        .files
        .iter()
        .map(|(p, c)| (rel(root, p), &c.data))
        .collect();
    rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter()
        .map(|(path, data)| {
            let sigs = if data.sigs.len() == data.defs.len() {
                &data.sigs
            } else {
                &data.defs
            };
            let body: String = sigs.iter().map(|s| format!("  {s}\n")).collect();
            format!("{path}:\n{body}")
        })
        .collect()
}

fn render_refs(root: &Path, hits: &[PathBuf], symbol: &str) -> String {
    if hits.is_empty() {
        return format!("no files reference `{symbol}`");
    }
    let list: String = hits.iter().map(|p| format!("{}\n", rel(root, p))).collect();
    format!("{} file(s) reference `{symbol}`:\n{list}", hits.len())
}

fn render_callers(root: &Path, c: &structure::Callers, symbol: &str) -> String {
    if c.defined_in.is_empty() && c.referenced_by.is_empty() {
        return format!("`{symbol}` is not defined or referenced in any indexed file");
    }
    let defined = if c.defined_in.is_empty() {
        "  defined in: (no indexed definition — external or built-in)\n".to_string()
    } else {
        let files: String = c
            .defined_in
            .iter()
            .map(|p| format!(" {}", rel(root, p)))
            .collect();
        let warn = if c.defined_in.len() > 1 {
            "  ⚠ defined in multiple files — callers resolved by name only\n"
        } else {
            ""
        };
        format!("  defined in:{files}\n{warn}")
    };
    let callers = if c.referenced_by.is_empty() {
        "  callers: none (no other indexed file references it)\n".to_string()
    } else {
        let list: String = c
            .referenced_by
            .iter()
            .map(|p| format!("    {}\n", rel(root, p)))
            .collect();
        format!(
            "  callers ({} file(s) — who breaks if you change it):\n{list}",
            c.referenced_by.len()
        )
    };
    format!("`{symbol}`:\n{defined}{callers}")
}

fn render_callees(root: &Path, deps: &[(String, PathBuf)], symbol: &str) -> String {
    if deps.is_empty() {
        return format!("`{symbol}`'s definer references no symbols defined in other files");
    }
    // Group (symbol, file) by file for a compact "file: sym, sym" listing.
    let mut by_file: std::collections::BTreeMap<String, Vec<&str>> = Default::default();
    for (sym, file) in deps {
        by_file
            .entry(rel(root, file))
            .or_default()
            .push(sym.as_str());
    }
    let body: String = by_file
        .into_iter()
        .map(|(file, mut syms)| {
            syms.sort_unstable();
            syms.dedup();
            format!("  {file}: {}\n", syms.join(", "))
        })
        .collect();
    format!("`{symbol}`'s definer depends on (cross-file):\n{body}")
}

fn render_graph(root: &Path, edges: &[(PathBuf, PathBuf)]) -> String {
    if edges.is_empty() {
        return "no cross-file dependencies found".into();
    }
    // Adjacency list: source file -> its dependency files.
    let mut by_src: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (a, b) in edges {
        by_src.entry(rel(root, a)).or_default().push(rel(root, b));
    }
    let body: String = by_src
        .into_iter()
        .map(|(src, mut dsts)| {
            dsts.sort_unstable();
            dsts.dedup();
            let lines: String = dsts.iter().map(|d| format!("    -> {d}\n")).collect();
            format!("{src}\n{lines}")
        })
        .collect();
    format!(
        "file-level dependency graph ({} edges):\n{body}",
        edges.len()
    )
}

fn render_chain(root: &Path, c: &structure::Chain, symbol: &str) -> String {
    if c.roots.is_empty() {
        return format!("`{symbol}` is not defined in any indexed file");
    }
    let defs: String = c
        .roots
        .iter()
        .map(|p| rel(root, p))
        .collect::<Vec<_>>()
        .join(", ");
    if c.levels.is_empty() {
        return format!("`{symbol}` (defined in {defs}) — used by nothing in the index");
    }
    // Group by hop distance: [hop 1] direct users, [hop 2] their users, …
    let mut by_depth: std::collections::BTreeMap<usize, Vec<String>> = Default::default();
    for (depth, path) in &c.levels {
        by_depth.entry(*depth).or_default().push(rel(root, path));
    }
    let body: String = by_depth
        .into_iter()
        .map(|(depth, mut files)| {
            files.sort_unstable();
            files.dedup();
            format!("  [hop {depth}] {}\n", files.join(", "))
        })
        .collect();
    format!(
        "`{symbol}` (defined in {defs}) — used by, transitively ({} files):\n{body}",
        c.levels.len()
    )
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_then_find_references() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "pub fn alpha() { beta(); }").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn beta() {}").unwrap();
        let tool = CodeMapTool { root: root.clone() };

        let listed = tool.run(CodeMapInput::default()).await.unwrap();
        assert!(listed.contains("a.rs:"), "missing file header: {listed}");
        assert!(
            listed.contains("pub fn alpha()"),
            "signature not rendered: {listed}"
        );
        assert!(
            listed.contains("pub fn beta()"),
            "signature not rendered: {listed}"
        );

        let refs = tool
            .run(CodeMapInput {
                op: Op::FindReferences,
                symbol: Some("beta".into()),
            })
            .await
            .unwrap();
        assert!(refs.contains("a.rs")); // caller
        assert!(refs.contains("b.rs")); // definition
    }

    #[tokio::test]
    async fn callers_and_callees_ops() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(
            root.join("a.rs"),
            "pub struct Foo {}\npub fn alpha() { beta(); }",
        )
        .unwrap();
        std::fs::write(root.join("b.rs"), "pub fn beta() -> Foo { Foo {} }").unwrap();
        let tool = CodeMapTool { root: root.clone() };

        let callers = tool
            .run(CodeMapInput {
                op: Op::Callers,
                symbol: Some("Foo".into()),
            })
            .await
            .unwrap();
        assert!(callers.contains("defined in: a.rs"));
        assert!(
            callers.contains("b.rs"),
            "type usage of Foo not reported: {callers}"
        );

        let callees = tool
            .run(CodeMapInput {
                op: Op::Callees,
                symbol: Some("beta".into()),
            })
            .await
            .unwrap();
        assert!(callees.contains("a.rs: Foo"), "callees wrong: {callees}");

        // missing symbol is a clean error, not a panic
        let err = tool
            .run(CodeMapInput {
                op: Op::Callers,
                symbol: None,
            })
            .await;
        assert!(err.is_err());

        let graph = tool
            .run(CodeMapInput {
                op: Op::Graph,
                symbol: None,
            })
            .await
            .unwrap();
        assert!(
            graph.contains("b.rs"),
            "graph missing dependency edge: {graph}"
        );
        assert!(
            graph.contains("-> a.rs"),
            "graph missing b.rs -> a.rs: {graph}"
        );
    }
}
