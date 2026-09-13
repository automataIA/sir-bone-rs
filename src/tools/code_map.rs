use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{truncate::truncate_default, TypedTool};
use crate::structure::{self, Index};

// Op semantics live in `description()`, not in doc comments: schemars copies doc
// comments into the JSON schema, so documenting them twice ships the same prose
// to the model twice on every turn (the schema rides the cached prefix).
#[derive(Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    // Project symbol map: every supported file with the symbols it declares.
    #[default]
    List,
    // Files that reference `symbol` (whole-word) — textual "who uses X".
    FindReferences,
    // Definer of `symbol` plus the files referencing it = who breaks on change.
    Callers,
    // Other files the definer of `symbol` depends on.
    Callees,
    // Whole-project file→file dependency graph (every edge at once).
    Graph,
    // Transitive "who uses `symbol`": the upstream usage chain, by hop depth.
    Chain,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct CodeMapInput {
    #[serde(default)]
    pub op: Op,
    /// Required by find_references, callers, callees and chain.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Restricts op=list to files under this directory or path prefix.
    #[serde(default)]
    pub path: Option<String>,
}

/// Signature listing budget, in chars. Half of [`truncate::DEFAULT_MAX_BYTES`]:
/// the global cap is the bound a single result must never cross, not a target to
/// aim at. Past this the listing degrades to [`render_index`] rather than being
/// cut off — measured on this repo, the full signature map is 2.5x the global
/// cap, so before the budget existed every `list` call returned an alphabetically
/// truncated map with no indication of what was missing.
const LIST_BUDGET_CHARS: usize = super::truncate::DEFAULT_MAX_BYTES / 2;

/// The pre-enrichment description, used only by the `code_map:lines` control arm.
const DESCRIPTION_BARE_PATHS: &str = "Map the codebase without reading files. Use \
     before editing an unfamiliar symbol, to find where it lives and what breaks if \
     you change it. \
     op=list (default): every source file with the signatures it declares — \
     scope it with path=\"<dir>\"; a repo too large to list falls back to \
     symbol counts per file. \
     op=find_references: files mentioning `symbol` (whole-word), searching every \
     text file, not just parsed sources. \
     op=callers: the file defining `symbol`, plus the files that use it. \
     op=callees: files the definer of `symbol` depends on. \
     op=graph: whole-project file→file dependency graph. \
     op=chain: transitive users of `symbol`, grouped by hop distance. \
     Matches by name, so a mention in a comment or string can over-report.";

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
        // The find_references sentence differs per arm of the `code_map:lines`
        // ablation: a control that promises `path:line:code` and returns bare
        // paths would be measuring a broken tool, not the old one.
        if crate::ablate::code_map_lines_disabled() {
            return DESCRIPTION_BARE_PATHS;
        }
        "Map the codebase without reading files. Use before editing an unfamiliar \
         symbol, to find where it lives and what breaks if you change it. \
         op=list (default): every source file with the signatures it declares — \
         scope it with path=\"<dir>\"; a repo too large to list falls back to \
         symbol counts per file. \
         op=find_references: every mention of `symbol` (whole-word) as \
         path:line:code, so you can judge the call sites without opening the files; \
         searches every text file, not just parsed sources. \
         op=callers: the file defining `symbol`, plus the files that use it. \
         op=callees: files the definer of `symbol` depends on. \
         op=graph: whole-project file→file dependency graph. \
         op=chain: transitive users of `symbol`, grouped by hop distance. \
         Matches by name, so a mention in a comment or string can over-report."
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
                Op::List => Ok(render_list(&root, &index, input.path.as_deref())),
                Op::FindReferences => {
                    let symbol = require_symbol("find_references")?;
                    Ok(render_refs(
                        &root,
                        &structure::find_reference_lines(&root, &symbol, REF_LINES_PER_FILE),
                        &symbol,
                        crate::ablate::code_map_lines_disabled(),
                        &structure::declarations(&index, &symbol),
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
///
/// Scoped by `filter` (a path prefix) when given. If the full listing would blow
/// [`LIST_BUDGET_CHARS`] it degrades to [`render_index`]: a complete but shallow
/// map beats a detailed one silently cut off partway through the alphabet.
fn render_list(root: &Path, index: &Index, filter: Option<&str>) -> String {
    let filter = filter.map(|f| f.trim_start_matches("./").trim_end_matches('/'));
    let mut rows: Vec<(String, &structure::FileStructure)> = index
        .files
        .iter()
        .map(|(p, c)| (rel(root, p), &c.data))
        .filter(|(path, _)| filter.is_none_or(|f| path.starts_with(f)))
        .collect();
    if rows.is_empty() {
        return match filter {
            Some(f) => format!("no supported source files under `{f}`"),
            None => "no supported source files found".into(),
        };
    }
    rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let full: String = rows
        .iter()
        .map(|(path, data)| {
            let sigs = if data.sigs.len() == data.defs.len() {
                &data.sigs
            } else {
                &data.defs
            };
            let body: String = sigs.iter().map(|s| format!("  {s}\n")).collect();
            format!("{path}:\n{body}")
        })
        .collect();
    if full.len() <= LIST_BUDGET_CHARS {
        return full;
    }
    render_index(&rows)
}

/// One line per file: path and how many symbols it declares. The fallback when
/// signatures do not fit — it stays complete, and names the way to drill down.
fn render_index(rows: &[(String, &structure::FileStructure)]) -> String {
    let total: usize = rows.iter().map(|(_, d)| d.defs.len()).sum();
    let body: String = rows
        .iter()
        .map(|(path, data)| format!("{path} ({})\n", data.defs.len()))
        .collect();
    format!(
        "{} files, {total} symbols — too large to list signatures, showing symbol \
         counts per file. Re-run with path=\"<dir>\" for the signatures of a \
         subtree, or use op=find_references/callers for one symbol.\n{body}",
        rows.len()
    )
}

/// `find_references` returns `path:line:content`, not bare paths: bare locations
/// answer nothing on their own, so the model had to open the definer file to learn
/// anything. Measured on this repo + graphrag-rs (n=48 paired symbols,
/// `examples/code_map_lines_probe.rs`): the declaration line is already present in
/// 47/48 cases for +996 B/call, against ~30 kB for the path-plus-read path it
/// replaces and 4.3 kB for the equivalent `rg -nw` dump.
///
/// That probe also found that *printing* the declaration line first gained
/// nothing — with a 3-line cap it is usually among the first matches anyway. It
/// did not test whether the declaration line survives the cap at all, which is a
/// different question and one the widened text corpus made real: see
/// [`structure::find_reference_lines`], which now spends a slot on it.
const REF_LINES_PER_FILE: usize = 3;
/// Whole-result content-line cap, so a symbol used everywhere cannot flood the
/// context. [`allocate`] spends it breadth-first; files past it still get an
/// inventory row, which is outside the cap but one short line each.
const REF_LINES_TOTAL: usize = 40;
/// Content lines are trimmed and clipped to this width.
const REF_LINE_CHARS: usize = 160;

/// True when reference *detail* is ordered by match count instead of by path
/// (`SIRBONE_REF_RANK=1`, ablatable with `SIRBONE_DISABLE=code_map:ref_rank`).
///
/// Opt-in on purpose. Match count is a plausible relevance signal and an
/// unproven one: `RefLines::extra` counts matching *lines*, so a repetitive
/// changelog or a generated fixture can outrank a real call site. Nothing here
/// promotes an unmeasured heuristic to default — and note that damping the count
/// (`sqrt`, `log`) would change nothing while it is the sole sort key, since any
/// monotone transform yields the same order. Damping only earns its place once
/// the count is combined with another signal.
fn ref_rank_enabled() -> bool {
    std::env::var_os("SIRBONE_REF_RANK").is_some() && !crate::ablate::ref_rank_disabled()
}

/// Content lines granted to each hit, index-aligned with `hits`.
///
/// Coverage before detail: the budget buys one line for every file first, and
/// only then a second and a third, one pass at a time. Spending three lines on
/// the alphabetically first files instead made the tail *invisible* — the old
/// code filled from `hits[0]` and cut whatever came after, so on this repo
/// `CHANGELOG.md` (1 mention of `ToolRegistry`) was detailed while
/// `src/tui/run.rs` (9) vanished, and the declaring file survived only by
/// alphabetical luck. A file the caller cannot see is a file it must find
/// again; a file shown with one line instead of three is merely less detailed.
///
/// `definers` (from [`structure::declarations`], so indexed languages only) go
/// first in every pass: it is the one file a symbol task nearly always needs,
/// and an absent declaration is not evidence — the index covers 5 languages, so
/// a Go or Ruby definer simply is not in the list, and must not be demoted for
/// it.
///
/// Ordering within a pass is by path unless [`ref_rank_enabled`] is on. Which
/// files get the *extra* detail is the disputed half of this design and stays
/// opt-in; which files are *visible* is not disputed and is unconditional.
fn allocate(
    hits: &[structure::RefLines],
    definers: &[PathBuf],
    budget: usize,
    by_rank: bool,
) -> Vec<usize> {
    let is_definer = |h: &structure::RefLines| definers.contains(&h.path);
    // `hits` arrives path-sorted and both sorts below are stable, so path stays
    // the tie-break and the whole allocation is deterministic.
    let mut order: Vec<usize> = (0..hits.len()).collect();
    if by_rank {
        order.sort_by_key(|&i| std::cmp::Reverse(hits[i].lines.len() + hits[i].extra));
    }
    order.sort_by_key(|&i| !is_definer(&hits[i]));

    let mut alloc = vec![0usize; hits.len()];
    let mut left = budget;
    for pass in 0..REF_LINES_PER_FILE {
        for &i in &order {
            if left == 0 {
                return alloc;
            }
            // `alloc[i] == pass` keeps the passes honest: a file only takes its
            // n-th line once every other file has had a chance at its (n-1)-th.
            if alloc[i] == pass && hits[i].lines.len() > pass {
                alloc[i] += 1;
                left -= 1;
            }
        }
    }
    alloc
}

/// `bare` is the `code_map:lines` control arm: file paths only, as before.
fn render_refs(
    root: &Path,
    hits: &[structure::RefLines],
    symbol: &str,
    bare: bool,
    definers: &[PathBuf],
) -> String {
    if hits.is_empty() {
        return format!("no files reference `{symbol}`");
    }
    if bare {
        let list: String = hits
            .iter()
            .map(|h| format!("{}\n", rel(root, &h.path)))
            .collect();
        return format!("{} file(s) reference `{symbol}`:\n{list}", hits.len());
    }
    if crate::ablate::ref_budget_disabled() {
        return render_refs_legacy(root, hits, symbol);
    }
    let alloc = allocate(hits, definers, REF_LINES_TOTAL, ref_rank_enabled());
    let mut body = String::new();
    // Files past the budget still get a row, with their match count: a complete
    // inventory lets the caller pick the file to open, where the old opaque
    // "N more file(s) not shown" left it re-running the search. Same trade
    // `render_list` already makes — complete and shallow beats detailed and cut.
    let mut inventory = String::new();
    for (hit, &n) in hits.iter().zip(&alloc) {
        let path = rel(root, &hit.path);
        let total = hit.lines.len() + hit.extra;
        if n == 0 {
            inventory.push_str(&format!("  {path} ({total} match(es))\n"));
            continue;
        }
        for (lineno, text) in hit.lines.iter().take(n) {
            let clipped: String = text.chars().take(REF_LINE_CHARS).collect();
            let ell = if clipped.len() < text.len() {
                "…"
            } else {
                ""
            };
            body.push_str(&format!("{path}:{lineno}:{clipped}{ell}\n"));
        }
        // The remainder rides the last shown row instead of taking a row of its
        // own, so `REF_LINES_TOTAL` bounds the whole result and not just part.
        if total > n {
            body.pop();
            body.push_str(&format!("  (+{} more in this file)\n", total - n));
        }
    }
    let tail = if inventory.is_empty() {
        String::new()
    } else {
        let shown = alloc.iter().filter(|&&n| n > 0).count();
        format!(
            "  … lines shown for {shown}/{} file(s); the rest, with match counts:\n{inventory}",
            hits.len()
        )
    };
    format!("{} file(s) reference `{symbol}`:\n{body}{tail}", hits.len())
}

/// The `code_map:ref_budget` control arm: the budget as it was spent before the
/// coverage-first allocation — three lines for the first files in path order
/// until the cap is gone, overflow notes on their own rows, and one opaque count
/// for everything cut. Kept verbatim rather than approximated, since an arm that
/// is not the previous tool measures nothing.
fn render_refs_legacy(root: &Path, hits: &[structure::RefLines], symbol: &str) -> String {
    let mut left = REF_LINES_TOTAL;
    let mut shown = 0usize;
    let mut body = String::new();
    for hit in hits {
        if left == 0 {
            break;
        }
        let path = rel(root, &hit.path);
        for (lineno, text) in hit.lines.iter().take(left) {
            let clipped: String = text.chars().take(REF_LINE_CHARS).collect();
            let ell = if clipped.len() < text.len() {
                "…"
            } else {
                ""
            };
            body.push_str(&format!("{path}:{lineno}:{clipped}{ell}\n"));
            left -= 1;
        }
        if hit.extra > 0 && left > 0 {
            body.push_str(&format!("  … {} more match(es) in {path}\n", hit.extra));
            left -= 1;
        }
        shown += 1;
    }
    let tail = match hits.len() - shown {
        0 => String::new(),
        n => format!("  … {n} more file(s) not shown\n"),
    };
    format!("{} file(s) reference `{symbol}`:\n{body}{tail}", hits.len())
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

    /// Builds a file whose signature listing is guaranteed to blow the budget.
    fn wide_repo(root: &Path, files: usize, per_file: usize) {
        for f in 0..files {
            let body: String = (0..per_file)
                .map(|s| format!("pub fn symbol_{f}_{s}_with_a_long_enough_name() {{}}\n"))
                .collect();
            std::fs::write(root.join(format!("mod{f}.rs")), body).unwrap();
        }
    }

    #[tokio::test]
    async fn oversized_list_degrades_to_an_index_instead_of_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        wide_repo(&root, 60, 20);
        let tool = CodeMapTool { root: root.clone() };

        let listed = tool.run(CodeMapInput::default()).await.unwrap();

        assert!(
            listed.len() <= LIST_BUDGET_CHARS,
            "fallback is {} chars, over budget",
            listed.len()
        );
        assert!(
            listed.contains("60 files, 1200 symbols"),
            "missing the totals header: {listed}"
        );
        assert!(listed.contains("path="), "no drill-down hint: {listed}");
        // Complete: the last file alphabetically survives, which is exactly what
        // truncation used to drop.
        assert!(listed.contains("mod9.rs (20)"), "index is not complete");
        assert!(
            !listed.contains("pub fn symbol_0_0"),
            "signatures should be dropped, not the tail of the file list"
        );
    }

    #[tokio::test]
    async fn path_scopes_the_listing_back_to_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        wide_repo(&root, 60, 20);
        std::fs::create_dir(root.join("small")).unwrap();
        std::fs::write(root.join("small/only.rs"), "pub fn scoped_fn() {}").unwrap();
        let tool = CodeMapTool { root: root.clone() };

        let scoped = tool
            .run(CodeMapInput {
                path: Some("small".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(
            scoped.contains("pub fn scoped_fn()"),
            "a subtree that fits must keep its signatures: {scoped}"
        );
        assert!(!scoped.contains("mod0.rs"), "filter leaked: {scoped}");

        let missing = tool
            .run(CodeMapInput {
                path: Some("nope".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(missing.contains("no supported source files under `nope`"));
    }

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
                path: None,
            })
            .await
            .unwrap();
        assert!(refs.contains("a.rs")); // caller
        assert!(refs.contains("b.rs")); // definition
    }

    #[tokio::test]
    async fn find_references_returns_path_line_content_within_caps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("def.rs"), "\n\npub fn target() {}\n").unwrap();
        // More matching lines than REF_LINES_PER_FILE, so the overflow is counted.
        let uses: String = (0..REF_LINES_PER_FILE + 4)
            .map(|i| format!("fn use{i}() {{ target(); }}\n"))
            .collect();
        std::fs::write(root.join("uses.rs"), uses).unwrap();
        let tool = CodeMapTool { root: root.clone() };

        let out = tool
            .run(CodeMapInput {
                op: Op::FindReferences,
                symbol: Some("target".into()),
                path: None,
            })
            .await
            .unwrap();

        // The declaration line is present with its real line number and content —
        // this is what removes the follow-up read.
        assert!(
            out.contains("def.rs:3:pub fn target() {}"),
            "declaration line missing: {out}"
        );
        assert!(
            out.contains("uses.rs:1:fn use0() { target(); }"),
            "call site missing: {out}"
        );
        let shown = out.lines().filter(|l| l.contains("uses.rs:")).count();
        assert_eq!(shown, REF_LINES_PER_FILE, "per-file cap not applied: {out}");
        assert!(
            out.contains("(+4 more in this file)"),
            "overflow not reported: {out}"
        );
    }

    #[test]
    fn bare_arm_reproduces_the_pre_enrichment_output() {
        let root = Path::new("/repo");
        let hits = vec![structure::RefLines {
            path: root.join("a.rs"),
            lines: vec![(7, "pub fn target() {}".into())],
            extra: 2,
        }];

        let bare = render_refs(root, &hits, "target", true, &[]);
        assert_eq!(bare, "1 file(s) reference `target`:\na.rs\n");

        let rich = render_refs(root, &hits, "target", false, &[]);
        assert!(rich.contains("a.rs:7:pub fn target() {}"), "{rich}");
        assert!(rich.contains("(+2 more in this file)"), "{rich}");
    }

    #[tokio::test]
    async fn find_references_caps_total_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // One match per file, more files than the whole-result budget.
        let files = REF_LINES_TOTAL + 5;
        for i in 0..files {
            std::fs::write(root.join(format!("f{i}.rs")), "fn hit() { widget(); }\n").unwrap();
        }
        let tool = CodeMapTool { root: root.clone() };

        let out = tool
            .run(CodeMapInput {
                op: Op::FindReferences,
                symbol: Some("widget".into()),
                path: None,
            })
            .await
            .unwrap();

        let content_lines = out.lines().filter(|l| l.contains(".rs:")).count();
        assert_eq!(
            content_lines, REF_LINES_TOTAL,
            "total cap not applied: {out}"
        );
        assert!(
            out.contains(&format!("{files} file(s) reference")),
            "header must still report the true total: {out}"
        );
        assert!(
            out.contains(&format!(
                "lines shown for {REF_LINES_TOTAL}/{files} file(s)"
            )),
            "truncation not disclosed: {out}"
        );
        // Past the budget a file still gets an inventory row with its match
        // count, so nothing is invisible and the caller need not search again.
        for i in 0..files {
            assert!(
                out.contains(&format!("f{i}.rs")),
                "f{i}.rs absent from a complete inventory: {out}"
            );
        }
    }

    /// `n` files, `matches` matching lines each, path-sorted as `allocate` expects.
    fn hits_of(counts: &[usize]) -> Vec<structure::RefLines> {
        counts
            .iter()
            .enumerate()
            .map(|(i, &total)| structure::RefLines {
                path: PathBuf::from(format!("f{i:02}.rs")),
                lines: (1..=total.min(REF_LINES_PER_FILE))
                    .map(|n| (n, format!("line {n}")))
                    .collect(),
                extra: total.saturating_sub(REF_LINES_PER_FILE),
            })
            .collect()
    }

    #[test]
    fn rank_off_spends_extra_detail_by_path_and_on_only_that() {
        // 3 files, 3 matches each, budget 4: everyone is covered, and the single
        // spare line is the whole difference between the two arms.
        let hits = hits_of(&[3, 3, 3]);
        assert_eq!(allocate(&hits, &[], 4, false), vec![2, 1, 1]);
        // Ranked: the last file has the most matches, so it takes the spare.
        let hits = hits_of(&[3, 3, 9]);
        assert_eq!(allocate(&hits, &[], 4, true), vec![1, 1, 2]);
        // Coverage is not up for debate: both arms show all three files.
        for arm in [true, false] {
            assert!(allocate(&hits, &[], 4, arm).iter().all(|&n| n > 0));
        }
    }

    #[test]
    fn definer_is_reserved_in_both_arms() {
        // Budget 2, three files: the definer sorts last and has the fewest
        // matches, so only the reservation can keep it visible.
        let hits = hits_of(&[9, 9, 1]);
        let definers = vec![PathBuf::from("f02.rs")];
        for arm in [true, false] {
            let alloc = allocate(&hits, &definers, 2, arm);
            assert_eq!(alloc[2], 1, "definer dropped (by_rank={arm}): {alloc:?}");
        }
    }

    #[tokio::test]
    async fn budget_covers_every_file_before_detailing_any() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // 20 files × 3 matches = 60 lines wanted against a 40-line budget. The
        // old path-order fill gave the first 13 files three lines each and made
        // the remaining 7 invisible; coverage-first shows all 20.
        let files = 20;
        let body: String = (0..REF_LINES_PER_FILE)
            .map(|i| format!("fn use{i}() {{ widget(); }}\n"))
            .collect();
        for i in 0..files {
            std::fs::write(root.join(format!("f{i:02}.rs")), &body).unwrap();
        }
        let tool = CodeMapTool { root: root.clone() };

        let out = tool
            .run(CodeMapInput {
                op: Op::FindReferences,
                symbol: Some("widget".into()),
                path: None,
            })
            .await
            .unwrap();

        for i in 0..files {
            assert!(
                out.contains(&format!("f{i:02}.rs:")),
                "f{i:02}.rs got no line: {out}"
            );
        }
        let content_lines = out.lines().filter(|l| l.contains(".rs:")).count();
        assert_eq!(content_lines, REF_LINES_TOTAL, "budget not spent: {out}");
    }

    #[tokio::test]
    async fn declaring_file_keeps_a_line_past_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // More files than the budget, and the definer sorts last by path — the
        // case where alphabetical order used to hide the declaration.
        for i in 0..REF_LINES_TOTAL + 5 {
            std::fs::write(root.join(format!("a{i:02}.rs")), "fn c() { widget(); }\n").unwrap();
        }
        std::fs::write(root.join("zz.rs"), "pub fn widget() {}\n").unwrap();
        let tool = CodeMapTool { root: root.clone() };

        let out = tool
            .run(CodeMapInput {
                op: Op::FindReferences,
                symbol: Some("widget".into()),
                path: None,
            })
            .await
            .unwrap();

        assert!(
            out.contains("zz.rs:1:pub fn widget() {}"),
            "declaring file demoted by path order: {out}"
        );
    }

    #[test]
    fn declaration_line_wins_its_slot_over_earlier_mentions() {
        // Four mentions before the declaration and a 3-line per-file cap: taking
        // matches in file order would drop the one line that answers the query.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut src: String = (0..4).map(|i| format!("// widget note {i}\n")).collect();
        src.push_str("pub fn widget() {}\n");
        std::fs::write(root.join("a.rs"), src).unwrap();

        let hits = structure::find_reference_lines(root, "widget", REF_LINES_PER_FILE);
        let out = render_refs(root, &hits, "widget", false, &[root.join("a.rs")]);
        assert!(out.contains("a.rs:5:pub fn widget() {}"), "{out}");
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
                path: None,
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
                path: None,
            })
            .await
            .unwrap();
        assert!(callees.contains("a.rs: Foo"), "callees wrong: {callees}");

        // missing symbol is a clean error, not a panic
        let err = tool
            .run(CodeMapInput {
                op: Op::Callers,
                symbol: None,
                path: None,
            })
            .await;
        assert!(err.is_err());

        let graph = tool
            .run(CodeMapInput {
                op: Op::Graph,
                symbol: None,
                path: None,
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
