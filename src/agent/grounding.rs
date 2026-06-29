//! Deterministic claim grounding (opt-in, `SIRBONE_GROUND`): **facts, not
//! accusations**.
//!
//! Extracts the concrete entities an LLM draft references about THIS project —
//! paths, code symbols, explicit counts — and looks up the ground truth for each
//! against the filesystem and the [`crate::structure`] symbol index (no LLM in
//! the lookup, so it cannot itself hallucinate). It injects those facts
//! *neutrally* ("`X` is defined in Y; path `Z` is not a current file") so the
//! model reconciles its draft against them.
//!
//! Why facts and not verdicts: judging a claim true/false needs its polarity and
//! intent — "there is no `X` *usage*" is not "`X` doesn't exist", and a plan
//! naming a file to *create* is not claiming it exists. That is not
//! deterministic; an earlier refute-and-correct design injected *wrong*
//! corrections on real plans (precision collapsed). Stating verified facts
//! sidesteps intent: a "not a current file" line is harmless for a to-create
//! path and corrective for a hallucinated one. We only emit a fact we are
//! mechanically certain of (a symbol that *is* declared; a path that *cannot* be
//! resolved; a real occurrence count); anything else is silently skipped — an
//! external `HashMap` produces no fact, not a wrong one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::structure::{self, Index};

/// Cap on injected facts, so a long draft can't flood the correction turn.
const MAX_FACTS: usize = 20;
/// A symbol declared in more files than this has no useful single "where" —
/// emitting "`new` is defined in … (+22 more)" is noise, not a fact worth acting
/// on. Skip it (still correct, just low-utility).
const MAX_SYMBOL_DECLS: usize = 5;

// Extensions that make a backticked token "look like a path" without a slash.
const PATH_EXTS: &[&str] = &[
    "rs", "py", "pyi", "js", "jsx", "ts", "tsx", "mjs", "cjs", "sql", "toml", "json", "yaml",
    "yml", "lock", "md", "sh", "rb", "go", "c", "h", "cpp", "hpp", "java", "kt", "swift", "scala",
    "php", "pl", "ex", "exs", "dart", "jl", "rkt",
];

static BACKTICK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
static IDENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_]\w*$").unwrap());
// A path token, optionally suffixed with `:line` or `:line-line` (stripped).
static PATH_TOK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z0-9_][\w./\-]*?)(?::\d+(?:-\d+)?)?$").unwrap());
// Count claims need an explicit cue so a line number next to a symbol
// ("src/main.rs:23 `Cli`") is not misread as "23 occurrences of `Cli`".
static COUNT_PLURAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{1,7})\s+`([A-Za-z_]\w*)`s\b").unwrap());
static COUNT_CUE_PRE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(\d{1,7})\s+(?:occurrences?|instances?|calls?|copies|usages?|uses?)\s+(?:of\s+|to\s+)?`([A-Za-z_]\w*)`",
    )
    .unwrap()
});
static COUNT_CUE_POST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)`([A-Za-z_]\w*)`[^`\n]{0,20}?\b(\d{1,7})\s+(?:times|occurrences?|instances?|calls?)\b",
    )
    .unwrap()
});

/// The entities a draft refers to, deduped. `counts` carries the claimed number.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refs {
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
    pub counts: Vec<(usize, String)>,
}

fn looks_like_path(tok: &str) -> bool {
    if tok.contains('/') {
        return true;
    }
    tok.rsplit_once('.')
        .is_some_and(|(_, ext)| PATH_EXTS.contains(&ext))
}

/// Lift the referenced paths / symbols / counts out of `draft`. Pure, no IO.
pub fn extract_refs(draft: &str) -> Refs {
    let mut refs = Refs::default();
    let (mut sp, mut ss): (BTreeSet<String>, BTreeSet<String>) = Default::default();

    for cap in COUNT_PLURAL.captures_iter(draft) {
        if let Ok(n) = cap[1].parse() {
            refs.counts.push((n, cap[2].to_string()));
        }
    }
    for cap in COUNT_CUE_PRE.captures_iter(draft) {
        if let Ok(n) = cap[1].parse() {
            refs.counts.push((n, cap[2].to_string()));
        }
    }
    for cap in COUNT_CUE_POST.captures_iter(draft) {
        if let Ok(n) = cap[2].parse() {
            refs.counts.push((n, cap[1].to_string()));
        }
    }
    for cap in BACKTICK.captures_iter(draft) {
        let tok = cap[1].trim();
        if tok.is_empty() || tok.contains(char::is_whitespace) {
            continue; // compound span like "mock_tui.rs:1553 fn exec_slash"
        }
        if IDENT.is_match(tok) {
            if ss.insert(tok.to_string()) {
                refs.symbols.push(tok.to_string());
            }
            continue;
        }
        if tok.contains('*') || tok.contains('?') || tok.contains("://") || tok.starts_with('/') {
            continue; // glob / URL / absolute path — out of scope
        }
        if let Some(p) = PATH_TOK.captures(tok).map(|c| c[1].to_string()) {
            if looks_like_path(&p) && sp.insert(p.clone()) {
                refs.paths.push(p);
            }
        }
    }
    refs.counts.sort();
    refs.counts.dedup();
    refs
}

/// Whether a claimed relative path is genuinely absent from the project. A name
/// matching several files (e.g. a bare `mod.rs`) is *present but ambiguous*, not
/// missing — saying "not a current file" there is a misleading fact, so only a
/// ZERO-match (no exact path, no suffix match) counts as absent.
fn path_is_absent(root: &Path, files: &[PathBuf], claimed: &str) -> bool {
    if root.join(claimed).exists() {
        return false;
    }
    !files
        .iter()
        .any(|p| p.ends_with(claimed) || p.to_string_lossy().ends_with(claimed))
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

/// Look up the ground truth for everything `draft` references and return neutral
/// fact lines. Deterministic, sync (CPU/IO bound — call under `spawn_blocking`).
pub fn facts(root: &Path, draft: &str) -> Vec<String> {
    let refs = extract_refs(draft);
    if refs.paths.is_empty() && refs.symbols.is_empty() && refs.counts.is_empty() {
        return Vec::new();
    }
    let index = structure::update(root, Index::load(root));
    let files: Vec<PathBuf> = structure::discover(root)
        .into_iter()
        .map(|(p, _)| p)
        .collect();

    let mut out: Vec<String> = Vec::new();

    // Paths: only the genuinely-absent ones are noteworthy (harmless for a
    // to-create file, corrective for a hallucinated existing one). A bare name
    // matching several files is ambiguous-but-present → no fact.
    for p in &refs.paths {
        if path_is_absent(root, &files, p) {
            out.push(format!("path `{p}` is not a current file in the project"));
        }
    }
    // Symbols: authoritative declaration site. Skip undeclared (external /
    // other-language) symbols — no fact beats a wrong one.
    for s in &refs.symbols {
        let decls = structure::declarations(&index, s);
        if decls.len() > MAX_SYMBOL_DECLS {
            continue; // ubiquitous name — no useful single location
        }
        if let Some(first) = decls.first() {
            let extra = if decls.len() > 1 {
                format!(" (+{} more)", decls.len() - 1)
            } else {
                String::new()
            };
            out.push(format!("`{s}` is defined in {}{extra}", rel(root, first)));
        }
    }
    // Counts: state the real number when it differs from the claim.
    for (n, tok) in &refs.counts {
        let actual = structure::count_occurrences(root, tok);
        if actual != *n {
            out.push(format!("`{tok}` occurs {actual} times in the project (you wrote {n})"));
        }
    }

    out.truncate(MAX_FACTS);
    out
}

/// Wrap the facts into the injection block, or `None` when there are none.
/// Proactive grounding (front-loaded context): for the paths/symbols the PROMPT
/// names, look up their real location + signature and return a block to seed into
/// the working notes BEFORE the run. Same deterministic engine as [`facts`] but
/// run forward — so the model goes straight to the right file instead of
/// searching (fewer exploratory tool calls, a more linear path). No LLM.
pub fn prompt_context(root: &Path, prompt: &str) -> Option<String> {
    let refs = extract_refs(prompt);
    if refs.symbols.is_empty() && refs.paths.is_empty() {
        return None;
    }
    let index = structure::update(root, Index::load(root));
    let files: Vec<PathBuf> = structure::discover(root)
        .into_iter()
        .map(|(p, _)| p)
        .collect();

    let mut lines: Vec<String> = Vec::new();
    for s in &refs.symbols {
        // Declaring files + the signature aligned with the matched def name.
        let mut hits: Vec<(PathBuf, String)> = index
            .files
            .iter()
            .filter_map(|(path, cache)| {
                cache
                    .data
                    .defs
                    .iter()
                    .position(|d| d == s)
                    .map(|i| (path.clone(), cache.data.sigs.get(i).cloned().unwrap_or_default()))
            })
            .collect();
        hits.sort();
        if hits.is_empty() || hits.len() > MAX_SYMBOL_DECLS {
            continue;
        }
        let (p, sig) = &hits[0];
        let extra = if hits.len() > 1 {
            format!(" (+{} more)", hits.len() - 1)
        } else {
            String::new()
        };
        if sig.trim().is_empty() {
            lines.push(format!("`{s}` → {}{extra}", rel(root, p)));
        } else {
            lines.push(format!("`{s}` → {} : `{sig}`{extra}", rel(root, p)));
        }
    }
    for p in &refs.paths {
        if !path_is_absent(root, &files, p) {
            lines.push(format!("`{p}` exists"));
        }
    }
    lines.truncate(MAX_FACTS);
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "RELEVANT CODE (verified, deterministic — these are the real locations; go \
         straight to them, don't search):\n{}",
        lines
            .iter()
            .map(|l| format!("- {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

pub fn facts_block(facts: &[String]) -> Option<String> {
    if facts.is_empty() {
        return None;
    }
    Some(format!(
        "VERIFIED FACTS — checked mechanically against the code (no model in the \
         loop). Reconcile your answer with these before finishing; some may differ \
         from what you wrote, and a path that \"is not a current file\" is expected \
         if you intend to create it:\n{}",
        facts
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn validate() {}\npub struct Config {}\nlet a=x.unwrap();\nlet b=y.unwrap()+z.unwrap();\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn extracts_paths_symbols_counts_skips_noise() {
        let r = extract_refs(
            "Edit `src/lib.rs` and create `src/new.rs`. `validate` exists. 12 `unwrap`s. \
             Use `HashMap`. Glob `src/*.rs`. URL `https://x.io/a.rs`. \
             Span `mock.rs:1 fn f`. Range `src/lib.rs:5-9`.",
        );
        assert!(r.paths.contains(&"src/lib.rs".to_string()));
        assert!(r.paths.contains(&"src/new.rs".to_string()));
        assert!(r.symbols.contains(&"validate".to_string()));
        assert!(r.symbols.contains(&"HashMap".to_string())); // extracted; no fact later
        assert!(r.counts.contains(&(12, "unwrap".to_string())));
        // noise excluded:
        assert!(!r.paths.iter().any(|p| p.contains('*'))); // glob
        assert!(!r.paths.iter().any(|p| p.contains("http"))); // URL
        assert!(!r.paths.iter().any(|p| p.contains(' '))); // compound span
        // range suffix stripped to the bare path (resolves to the real file)
        assert!(r.paths.contains(&"src/lib.rs".to_string()));
    }

    #[test]
    fn line_number_adjacency_is_not_a_count() {
        let r = extract_refs("Add the flag at src/main.rs:23 `Cli`.");
        assert!(r.counts.is_empty(), "{r:?}");
    }

    #[test]
    fn facts_are_true_and_neutral() {
        let dir = fixture();
        let root = dir.path();
        let draft = "Plan: edit `src/lib.rs`, create a `src/new.rs` module, the doctor \
                     path has no `validate` usage, we keep `Config`. It has 99 `unwrap`s. \
                     We also use `HashMap`.";
        let f = facts(root, draft);
        // missing path -> neutral fact (harmless: it's a to-create file)
        assert!(f.iter().any(|x| x.contains("`src/new.rs` is not a current file")));
        // existing-symbol -> authoritative location (NOT an accusation)
        assert!(f.iter().any(|x| x.contains("`validate` is defined in src/lib.rs")));
        assert!(f.iter().any(|x| x.contains("`Config` is defined in src/lib.rs")));
        // wrong count -> real number
        assert!(f.iter().any(|x| x.contains("`unwrap` occurs 3 times") && x.contains("99")));
        // external symbol -> NO fact (no noise, no wrong fact)
        assert!(!f.iter().any(|x| x.contains("`HashMap`")));
        // an existing path is not noise
        assert!(!f.iter().any(|x| x.contains("`src/lib.rs` is not a current file")));
        // every emitted line is a true statement about the fixture
        assert!(facts_block(&f).is_some());
    }

    #[test]
    fn no_refs_no_facts() {
        let dir = fixture();
        assert!(facts(dir.path(), "just prose, nothing to ground").is_empty());
    }

    #[test]
    fn prompt_context_seeds_real_locations() {
        let dir = fixture();
        let root = dir.path();
        let ctx = prompt_context(root, "add a flag near `validate`, reusing `Config`")
            .expect("should ground the named symbols");
        assert!(ctx.contains("`validate` → src/lib.rs"));
        assert!(ctx.contains("`Config` → src/lib.rs"));
        // external symbol the prompt didn't name -> not invented
        assert!(prompt_context(root, "use a HashMap somewhere").is_none());
    }

    #[test]
    fn ambiguous_bare_filename_is_not_flagged_missing() {
        let dir = fixture();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/mod.rs"), "pub fn x() {}").unwrap();
        std::fs::write(root.join("b/mod.rs"), "pub fn y() {}").unwrap();
        let f = facts(root, "Edit `mod.rs`, and create `truly_absent.rs`.");
        // `mod.rs` exists (ambiguously) -> NOT flagged as missing (was misleading)
        assert!(!f.iter().any(|x| x.contains("`mod.rs` is not a current file")), "{f:?}");
        // a genuinely absent file is still flagged
        assert!(f.iter().any(|x| x.contains("`truly_absent.rs` is not a current file")));
    }
}
