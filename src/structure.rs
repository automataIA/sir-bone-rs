//! Deterministic project structure extraction ("Pass 0/1").
//!
//! Pass 0 walks the workspace with `ignore`'s parallel walker (honouring
//! `.gitignore`) plus a hardcoded prune list, so a fresh clone with a huge
//! `node_modules`/`target` can't tank the walk. Pass 1 extracts per-language
//! symbol definitions (with full signatures) and imports with pre-compiled
//! regexes — no tree-sitter, no LLM. File reads + regex scans fan out over
//! scoped std threads ([`par_map`]). Results are cached per file by mtime, so
//! re-scans only reparse changed files. The cache lives under
//! `~/.sirbone/projects/<slug>/structure.bin` (no artifacts in the repo),
//! reusing the stable slug from [`crate::project_store`].
//!
//! Regex extraction is heuristic: a keyword inside a string or comment can be
//! mis-captured. For an agent's project map this is acceptable; a precise
//! caller→callee graph would need a real parser and is out of scope here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Directories always pruned, even when no `.gitignore` excludes them.
const PRUNE_DIRS: &[&str] = &["node_modules", "target", ".venv", "dist", "build", ".git"];

/// Files larger than this are skipped during discovery (aider's 1 MB cap) — a
/// guard against megabyte-scale generated/serialized blobs that aren't ignored.
const MAX_FILE_BYTES: u64 = 1_000_000;

/// A file whose mean line length exceeds this is treated as minified and
/// skipped (scc's heuristic: mean line ≥ 255 bytes). Catches minified bundles
/// that slip under [`MAX_FILE_BYTES`].
const MAX_MEAN_LINE: usize = 255;

/// Languages recognised by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lang {
    Rust,
    Python,
    JsTs,
    Sql,
}

impl Lang {
    fn from_ext(ext: &str) -> Option<Lang> {
        match ext {
            "rs" => Some(Lang::Rust),
            "py" | "pyi" => Some(Lang::Python),
            "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Some(Lang::JsTs),
            "sql" => Some(Lang::Sql),
            _ => None,
        }
    }
}

/// Symbols a single file declares (`defs`) and depends on (`imports`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStructure {
    pub lang: Option<Lang>,
    pub defs: Vec<String>,
    /// Full declaration signatures, index-aligned with `defs`. `defs` stays
    /// names-only because the graph's alternation regex matches names; `sigs`
    /// is what `code_map list` shows the model (params + return types, so it
    /// can call a symbol without opening the file).
    pub sigs: Vec<String>,
    pub imports: Vec<String>,
}

/// One cached file: its mtime plus the extracted structure. No `serde(flatten)`
/// — bincode (the cache format) is not self-describing and can't flatten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCache {
    pub mtime: SystemTime,
    pub data: FileStructure,
}

/// The whole-project index, persisted as bincode.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    pub files: HashMap<PathBuf, FileCache>,
}

// --- Pre-compiled per-language regexes (compiled once) ---------------------

struct LangRegex {
    defs: Vec<Regex>,
    imports: Vec<Regex>,
}

fn rx(p: &str) -> Regex {
    Regex::new(p).expect("static regex must compile")
}

/// Optional Rust visibility prefix: `pub`, `pub(crate)`, `pub(in path)`, etc.
const RUST_VIS: &str = r"(?:pub(?:\([^)]*\))?\s+)?";

static RUST: LazyLock<LangRegex> = LazyLock::new(|| LangRegex {
    defs: vec![
        // fn — through any of: pub, default, const, async, unsafe, extern "C"
        rx(&format!(
            r#"(?m)^\s*{RUST_VIS}(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"#
        )),
        // struct / enum / trait / union
        rx(&format!(
            r"(?m)^\s*{RUST_VIS}(?:unsafe\s+)?(?:struct|enum|trait|union)\s+([A-Za-z_][A-Za-z0-9_]*)"
        )),
    ],
    imports: vec![rx(r"(?m)^\s*(?:pub\s+)?use\s+([^;]+);")],
});

static PYTHON: LazyLock<LangRegex> = LazyLock::new(|| LangRegex {
    defs: vec![
        rx(r"(?m)^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)"),
        rx(r"(?m)^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)"),
    ],
    imports: vec![
        rx(r"(?m)^\s*from\s+([.\w]+)\s+import"),
        rx(r"(?m)^\s*import\s+([.\w]+)"),
    ],
});

static JSTS: LazyLock<LangRegex> = LazyLock::new(|| LangRegex {
    defs: vec![
        rx(
            r"(?m)^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)",
        ),
        rx(r"(?m)^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)"),
        // const foo = (...) => / const Foo = async (...) =>
        rx(
            r"(?m)^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*=>",
        ),
    ],
    imports: vec![rx(r#"(?m)^\s*import\s+.*?from\s+['"]([^'"]+)['"]"#)],
});

static SQL: LazyLock<LangRegex> = LazyLock::new(|| LangRegex {
    defs: vec![
        rx(
            r#"(?i)CREATE\s+(?:TEMP(?:ORARY)?\s+)?TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?[`"\[]?([A-Za-z0-9_.]+)"#,
        ),
        rx(
            r#"(?i)CREATE\s+(?:OR\s+REPLACE\s+)?(?:MATERIALIZED\s+)?VIEW\s+(?:IF\s+NOT\s+EXISTS\s+)?[`"\[]?([A-Za-z0-9_.]+)"#,
        ),
    ],
    imports: vec![rx(r#"(?i)REFERENCES\s+[`"\[]?([A-Za-z0-9_.]+)"#)],
});

/// Start of a Rust inline test module. Defs below it are test helpers
/// (`fn file`, `fn tool`) that must not pollute the cross-file symbol table.
/// Anchored to line start (`(?m)^\s*`) so the pattern appearing inside a
/// doc-comment or string (preceded by `///`, `"`, …) does NOT match and
/// wrongly truncate real code. Requires `mod` after, so a lone per-function
/// test attribute mid-file doesn't truncate either.
static TEST_MOD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*#\[cfg\(test\)\]\s*mod\s").expect("static regex must compile")
});

fn regexes(lang: Lang) -> &'static LangRegex {
    match lang {
        Lang::Rust => &RUST,
        Lang::Python => &PYTHON,
        Lang::JsTs => &JSTS,
        Lang::Sql => &SQL,
    }
}

/// Run every regex over `text`, capturing group 1, sorted and de-duplicated.
fn extract(res: &[Regex], text: &str) -> Vec<String> {
    let mut out: Vec<String> = res
        .iter()
        .flat_map(|re| re.captures_iter(text))
        .filter_map(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Signatures longer than this are truncated with `…` — keeps one pathological
/// declaration from bloating the whole map.
const MAX_SIG_LEN: usize = 160;

/// Full declaration signature starting at byte `start` (the def regex match).
/// A regex can't balance parens, so this is a byte scanner: depth counts
/// `(`/`[` (plus `{` for Python — dict literals in default args), and the
/// terminator is language-specific at depth 0: `:` for Python, `{`/`;` for
/// Rust/JS (plus `=>` for JS arrows). Newline at depth 0 is a universal
/// fallback so a runaway scan never crosses a logical line. Whitespace runs
/// collapse to one space, so multi-line param lists render on one line.
fn signature(lang: Lang, text: &str, start: usize) -> String {
    let bytes = text.as_bytes();
    // `(?m)^\s*` can start the match on a blank line *before* the declaration
    // (\s eats newlines); skip leading whitespace or the \n terminator fires
    // immediately and yields an empty signature.
    let mut start = start;
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    let mut depth = 0usize;
    let mut end = bytes.len();
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b'{' if lang == Lang::Python => depth += 1,
            b'}' if lang == Lang::Python => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                if lang == Lang::JsTs && b == b'=' && bytes.get(i + 1) == Some(&b'>') {
                    end = i + 2; // keep the arrow: `const f = (a, b) =>`
                    break;
                }
                let stop = match lang {
                    Lang::Python => matches!(b, b':' | b'\n'),
                    _ => matches!(b, b'{' | b';' | b'\n'),
                };
                if stop {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let sig = text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if sig.len() > MAX_SIG_LEN {
        let mut cut = MAX_SIG_LEN;
        while !sig.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &sig[..cut])
    } else {
        sig
    }
}

/// Like [`extract`] but pairs each def name with its full [`signature`].
/// Returns `(defs, sigs)` index-aligned, sorted by name, deduped by name.
/// SQL defs (tables/views) have no useful signature — the name is the sig.
fn extract_defs(lang: Lang, res: &[Regex], text: &str) -> (Vec<String>, Vec<String>) {
    let mut pairs: Vec<(String, String)> = res
        .iter()
        .flat_map(|re| re.captures_iter(text))
        .filter_map(|c| {
            let name = c.get(1)?.as_str().trim().to_string();
            let sig = match lang {
                Lang::Sql => name.clone(),
                _ => signature(lang, text, c.get(0)?.start()),
            };
            Some((name, sig))
        })
        .collect();
    pairs.sort_unstable();
    pairs.dedup_by(|a, b| a.0 == b.0);
    pairs.into_iter().unzip()
}

/// Map `f` over `items` on up to `available_parallelism` scoped std threads,
/// preserving input order. Serial when the batch is trivial. The per-item work
/// here is a file read + regex scan — parallel enough to matter on real repos
/// without pulling in rayon. A panicking worker is propagated, not swallowed.
fn par_map<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(items.len());
    if threads < 2 {
        return items.into_iter().map(f).collect();
    }
    let chunk_len = items.len().div_ceil(threads);
    let mut chunks: Vec<Vec<T>> = Vec::with_capacity(threads);
    let mut iter = items.into_iter();
    loop {
        let chunk: Vec<T> = iter.by_ref().take(chunk_len).collect();
        if chunk.is_empty() {
            break;
        }
        chunks.push(chunk);
    }
    let f = &f;
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| scope.spawn(move || chunk.into_iter().map(f).collect::<Vec<R>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| match h.join() {
                Ok(v) => v,
                Err(p) => std::panic::resume_unwind(p),
            })
            .collect()
    })
}

/// Read a file for analysis, skipping non-UTF-8/binary (a NUL byte, ripgrep's
/// heuristic) and minified blobs (any single line longer than [`MAX_MEAN_LINE`]
/// — the previous mean-length check let a huge single line padded with blanks
/// through). Returns `None` to skip. Centralises every analysis read so the caps
/// apply uniformly.
fn read_source(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    if content.as_bytes().contains(&0) {
        return None; // binary
    }
    let too_long = content.lines().any(|l| l.len() > MAX_MEAN_LINE);
    (!too_long).then_some(content)
}

/// Whether a regex match at byte offset `start` should be dropped as a
/// non-free-call reference. `\b` already excludes ASCII identifier chars, so
/// this handles what it misses:
/// - method call / chain `x.name`, `foo()\n    .name()` (skips whitespace back),
/// - associated call `Type::name` / `Self::name` (kept: `module::name`, lowercase),
/// - `$name` (JS) / `r#name` (Rust raw) where `$`/`#` aren't ASCII word chars.
fn should_skip_match(bytes: &[u8], start: usize) -> bool {
    if start > 0 && matches!(bytes[start - 1], b'$' | b'#') {
        return true; // `$foo` / `r#foo` — inside a larger identifier
    }
    let mut i = start;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1; // method chains put `.` at the start of the next line
    }
    if i == 0 {
        return false;
    }
    if bytes[i - 1] == b'.' {
        return true; // `.name` — method / field / module attribute
    }
    if i >= 2 && bytes[i - 1] == b':' && bytes[i - 2] == b':' {
        // `Qualifier::name`: drop if Qualifier is a type (`Vec::`, `Self::`),
        // keep if it's a lowercase module path (`crate::module::free_fn`).
        let mut j = i - 2;
        while j > 0 && (bytes[j - 1].is_ascii_alphanumeric() || bytes[j - 1] == b'_') {
            j -= 1;
        }
        return bytes.get(j).is_some_and(u8::is_ascii_uppercase);
    }
    false
}

/// True if `content` references `re`'s symbol at least once as a free reference
/// (filtering out method/associated calls via [`should_skip_match`]).
fn references_freely(re: &Regex, content: &str) -> bool {
    re.find_iter(content)
        .any(|m| !should_skip_match(content.as_bytes(), m.start()))
}

/// Pass 1: extract structure from in-memory source of a known language. Defs
/// inside a Rust inline `#[cfg(test)] mod` are excluded from the symbol table.
pub fn parse_str(lang: Lang, content: &str) -> FileStructure {
    let r = regexes(lang);
    let code = TEST_MOD
        .find(content)
        .map_or(content, |m| &content[..m.start()]);
    let (defs, sigs) = extract_defs(lang, &r.defs, code);
    FileStructure {
        lang: Some(lang),
        defs,
        sigs,
        imports: extract(&r.imports, code),
    }
}

/// Pass 0: walk `root`, returning supported (path, lang) pairs, sorted by path
/// (the parallel walk yields entries in nondeterministic order). Honours
/// `.gitignore` and always prunes [`PRUNE_DIRS`].
pub fn discover(root: &Path) -> Vec<(PathBuf, Lang)> {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        // Honour .gitignore even outside a git repo (default only applies it
        // inside one), so a not-yet-committed project is still filtered.
        .require_git(false)
        // Skip oversized blobs before they ever reach a read/regex.
        .max_filesize(Some(MAX_FILE_BYTES))
        .threads(std::thread::available_parallelism().map_or(1, |n| n.get()))
        .filter_entry(|e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            let pruned = e
                .file_name()
                .to_str()
                .is_some_and(|n| PRUNE_DIRS.contains(&n));
            !(is_dir && pruned)
        })
        .build_parallel();

    let (tx, rx) = std::sync::mpsc::channel::<(PathBuf, Lang)>();
    walker.run(|| {
        let tx = tx.clone();
        Box::new(move |entry| {
            let Ok(e) = entry else {
                return ignore::WalkState::Continue;
            };
            let lang = e
                .file_type()
                .is_some_and(|t| t.is_file())
                .then(|| {
                    e.path()
                        .extension()
                        .and_then(|x| x.to_str())
                        .and_then(Lang::from_ext)
                })
                .flatten();
            if let Some(lang) = lang {
                let _ = tx.send((e.path().to_path_buf(), lang));
            }
            ignore::WalkState::Continue
        })
    });
    drop(tx);
    let mut out: Vec<(PathBuf, Lang)> = rx.into_iter().collect();
    out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Incremental scan: reuse cached entries whose mtime is unchanged, reparse the
/// rest, and drop files no longer on disk (auto-GC). NB: a branch switch can
/// move mtime backward and leave an entry stale; reparse is sub-ms so the only
/// cost is startup latency, accepted for v1.
pub fn update(root: &Path, mut old: Index) -> Index {
    let mut new = Index::default();
    let mut to_parse: Vec<(PathBuf, Lang, SystemTime)> = Vec::new();
    for (path, lang) in discover(root) {
        let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            continue;
        };
        let fresh = old.files.get(&path).is_some_and(|c| c.mtime >= mtime);
        if fresh {
            if let Some(cached) = old.files.remove(&path) {
                new.files.insert(path, cached);
                continue;
            }
        }
        to_parse.push((path, lang, mtime));
    }
    // Read + regex-parse changed files in parallel; cold scans touch every file.
    let parsed = par_map(to_parse, |(path, lang, mtime)| {
        let data = read_source(&path).map(|c| parse_str(lang, &c))?;
        Some((path, FileCache { mtime, data }))
    });
    new.files.extend(parsed.into_iter().flatten());
    new
}

/// Files (sorted) containing a whole-word match of `symbol` — a textual "who
/// uses X" across supported files in `root`.
pub fn find_references(root: &Path, symbol: &str) -> Vec<PathBuf> {
    let Ok(re) = Regex::new(&format!(r"\b{}\b", regex::escape(symbol))) else {
        return Vec::new();
    };
    let mut hits: Vec<PathBuf> = par_map(discover(root), |(p, _)| {
        read_source(&p).filter(|c| re.is_match(c)).map(|_| p)
    })
    .into_iter()
    .flatten()
    .collect();
    hits.sort_unstable();
    hits
}

/// Deterministic whole-word occurrence count of `token` across the same source
/// corpus as [`find_references`]. Used by claim grounding to check numeric
/// assertions ("N `unwrap`s"). Counts every whole-word match — including
/// comments/strings — so it does not distinguish call sites from mentions;
/// treat a mismatch as evidence, not proof.
pub fn count_occurrences(root: &Path, token: &str) -> usize {
    let Ok(re) = Regex::new(&format!(r"\b{}\b", regex::escape(token))) else {
        return 0;
    };
    par_map(discover(root), |(p, _)| {
        read_source(&p).map_or(0, |c| re.find_iter(&c).count())
    })
    .into_iter()
    .sum()
}

/// Files that *declare* `symbol` (exact name in `defs`), from the cached index.
/// Authoritative for the indexed languages: unlike [`find_references`] (any
/// whole-word mention), a hit here is a real declaration — so its absence is
/// strong evidence the symbol genuinely is not defined in the project.
pub fn declarations(index: &Index, symbol: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = index
        .files
        .iter()
        .filter(|(_, c)| c.data.defs.iter().any(|d| d == symbol))
        .map(|(p, _)| p.clone())
        .collect();
    v.sort_unstable();
    v
}

// --- File-level call graph (resolved at query time, not cached) -------------
//
// "Extract local, resolve global": defs are cached per file, but cross-file
// edges depend on every file's defs, so they're recomputed on demand. The
// symbol table is the set of cached `defs`; a reference is any whole-word
// occurrence of a known def name (NOT just `name(` — structs/enums/traits are
// used as `Foo {}` / `Foo::X` / `: Foo`, never `Foo(`). Names defined in more
// than one file resolve by name only (flagged ambiguous to the caller).

/// Map each defined symbol name to the files that declare it.
fn def_table(index: &Index) -> std::collections::HashMap<String, Vec<PathBuf>> {
    let mut table: std::collections::HashMap<String, Vec<PathBuf>> = Default::default();
    for (path, cache) in &index.files {
        for name in &cache.data.defs {
            table.entry(name.clone()).or_default().push(path.clone());
        }
    }
    table
}

/// Where `symbol` is defined, and which *other* files reference it whole-word.
/// `referenced_by` answers "who breaks if I change `symbol`".
pub struct Callers {
    pub defined_in: Vec<PathBuf>,
    pub referenced_by: Vec<PathBuf>,
}

pub fn callers(index: &Index, symbol: &str) -> Callers {
    let mut defined_in: Vec<PathBuf> = index
        .files
        .iter()
        .filter(|(_, c)| c.data.defs.iter().any(|d| d == symbol))
        .map(|(p, _)| p.clone())
        .collect();
    defined_in.sort_unstable();

    let referenced_by = Regex::new(&format!(r"\b{}\b", regex::escape(symbol)))
        .ok()
        .map(|re| {
            let candidates: Vec<PathBuf> = index
                .files
                .keys()
                .filter(|p| !defined_in.contains(p))
                .cloned()
                .collect();
            let mut hits: Vec<PathBuf> = par_map(candidates, |p| {
                read_source(&p)
                    .is_some_and(|c| references_freely(&re, &c))
                    .then_some(p)
            })
            .into_iter()
            .flatten()
            .collect();
            hits.sort_unstable();
            hits
        })
        .unwrap_or_default();

    Callers {
        defined_in,
        referenced_by,
    }
}

/// `callees` links only symbols with a *single* definition site. File-level
/// name matching can't distinguish a method call (`obj.load()`) from a free
/// function (`load()`), so any name defined in more than one file is dropped as
/// unresolvable — precision over recall, since noise (not missing edges) is what
/// makes a dependency list useless. (aider downweights such names; we have no
/// ranking layer, so we drop them.)
const AMBIG_MAX_DEFS: usize = 1;

/// `(referenced_symbol, defining_file)` pairs in *other* files that the
/// definer(s) of `symbol` reference — what the definer depends on. Only symbols
/// with a unique definition site are linked (see [`AMBIG_MAX_DEFS`]).
pub fn callees(index: &Index, symbol: &str) -> Vec<(String, PathBuf)> {
    let table = def_table(index);
    let Some(definers) = table.get(symbol) else {
        return Vec::new();
    };
    // One alternation over every known def name; match positions aren't needed,
    // only which names occur in the definer's source. Longest names first so a
    // short name that prefixes another can't shadow it before backtracking.
    let mut names: Vec<String> = table.keys().map(|n| regex::escape(n)).collect();
    names.sort_by_key(|a| std::cmp::Reverse(a.len()));
    let Ok(alt) = Regex::new(&format!(r"\b({})\b", names.join("|"))) else {
        return Vec::new();
    };

    let mut out: Vec<(String, PathBuf)> = definers
        .iter()
        .filter_map(|definer| read_source(definer).map(|c| (definer, c)))
        .flat_map(|(definer, content)| {
            alt.find_iter(&content)
                .filter(|m| !should_skip_match(content.as_bytes(), m.start()))
                .map(|m| m.as_str().to_string())
                .filter(|name| name != symbol)
                .flat_map(|name| {
                    table
                        .get(&name)
                        .filter(|files| files.len() <= AMBIG_MAX_DEFS)
                        .into_iter()
                        .flatten()
                        .filter(|f| *f != definer)
                        .map(move |f| (name.clone(), f.clone()))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Map of every symbol with a *single* definition site → its defining file.
/// BTreeMap so iteration (and the fingerprint built from it) is deterministic.
fn unique_defs(index: &Index) -> std::collections::BTreeMap<String, PathBuf> {
    let mut counts: HashMap<&str, (usize, &Path)> = HashMap::new();
    for (path, c) in &index.files {
        for name in &c.data.defs {
            counts
                .entry(name)
                .and_modify(|e| e.0 += 1)
                .or_insert((1, path));
        }
    }
    counts
        .into_iter()
        .filter(|(_, (n, _))| *n == 1)
        .map(|(name, (_, path))| (name.to_string(), path.to_path_buf()))
        .collect()
}

/// One alternation regex over every uniquely-defined symbol name.
fn alt_regex(unique: &std::collections::BTreeMap<String, PathBuf>) -> Option<Regex> {
    if unique.is_empty() {
        return None;
    }
    let mut names: Vec<String> = unique.keys().map(|n| regex::escape(n)).collect();
    names.sort_by_key(|a| std::cmp::Reverse(a.len()));
    Regex::new(&format!(r"\b({})\b", names.join("|"))).ok()
}

/// Dependency files of one source file: every uniquely-defined symbol it
/// references freely (no method/assoc calls), minus itself. Sorted, deduped.
fn file_targets(
    path: &Path,
    content: &str,
    alt: &Regex,
    unique: &std::collections::BTreeMap<String, PathBuf>,
) -> Vec<PathBuf> {
    let mut targets: std::collections::BTreeSet<&Path> = Default::default();
    for m in alt.find_iter(content) {
        if should_skip_match(content.as_bytes(), m.start()) {
            continue;
        }
        if let Some(def) = unique.get(m.as_str()) {
            if def != path {
                targets.insert(def);
            }
        }
    }
    targets.into_iter().map(Path::to_path_buf).collect()
}

/// Transitive "who uses X": the upstream flow chart. Files that directly or
/// indirectly depend on a symbol defined in `symbol`'s file(s), as breadth-first
/// levels (each file at its shortest hop count from the definer). Built from the
/// live [`graph_cached`] edges; cycles are handled by a global visited set.
pub struct Chain {
    pub roots: Vec<PathBuf>,
    pub levels: Vec<(usize, PathBuf)>,
}

pub fn usage_chain(index: &Index, edges: &[(PathBuf, PathBuf)], symbol: &str) -> Chain {
    let mut roots: Vec<PathBuf> = index
        .files
        .iter()
        .filter(|(_, c)| c.data.defs.iter().any(|d| d == symbol))
        .map(|(p, _)| p.clone())
        .collect();
    roots.sort_unstable();
    if roots.is_empty() {
        return Chain {
            roots,
            levels: Vec::new(),
        };
    }

    // Reverse adjacency: `rev[B]` = files that depend on B (its direct users).
    let mut rev: std::collections::HashMap<&Path, Vec<&Path>> = Default::default();
    for (a, b) in edges {
        rev.entry(b.as_path()).or_default().push(a.as_path());
    }

    let mut seen: std::collections::HashSet<&Path> = roots.iter().map(PathBuf::as_path).collect();
    let mut levels: Vec<(usize, PathBuf)> = Vec::new();
    let mut frontier: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    let mut depth = 1;
    while !frontier.is_empty() {
        let mut next: Vec<&Path> = Vec::new();
        for f in &frontier {
            for &user in rev.get(f).into_iter().flatten() {
                if seen.insert(user) {
                    levels.push((depth, user.to_path_buf()));
                    next.push(user);
                }
            }
        }
        frontier = next;
        depth += 1;
    }
    levels.sort();
    Chain { roots, levels }
}

/// Build the `AGENTS.md` project doc from the analysed index + graph: a concise
/// overview (language/symbol counts) and the most-depended-on files (graph
/// in-degree), with placeholders for the user to fill. Deterministic, no LLM.
pub fn init_doc(root: &Path, index: &Index, edges: &[(PathBuf, PathBuf)]) -> String {
    use std::collections::BTreeMap;
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");

    let mut by_lang: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for c in index.files.values() {
        let lang = match c.data.lang {
            Some(Lang::Rust) => "Rust",
            Some(Lang::Python) => "Python",
            Some(Lang::JsTs) => "JS/TS",
            Some(Lang::Sql) => "SQL",
            None => "other",
        };
        let e = by_lang.entry(lang).or_default();
        e.0 += 1;
        e.1 += c.data.defs.len();
    }
    let langs: String = by_lang
        .iter()
        .map(|(l, (f, s))| format!("{l} ({f} files, {s} symbols)"))
        .collect::<Vec<_>>()
        .join(", ");

    // In-degree = how many files depend on each file → the core modules.
    let mut indeg: BTreeMap<&Path, usize> = BTreeMap::new();
    for (_, b) in edges {
        *indeg.entry(b.as_path()).or_default() += 1;
    }
    let mut ranked: Vec<(&Path, usize)> = indeg.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let key: String = ranked
        .iter()
        .take(10)
        .map(|(p, n)| {
            format!(
                "- `{}` — used by {n} files\n",
                p.strip_prefix(root).unwrap_or(p).display()
            )
        })
        .collect();

    format!(
        "# {name}\n\n\
         > Auto-generated by `sirbone /init` (AGENTS.md open standard). Edit freely — sirbone and other agent harnesses read this file as project instructions.\n\n\
         ## Overview\n\
         - {} source files, {} dependency edges.\n\
         - Languages: {langs}\n\n\
         ## Key modules (most depended-on)\n{key}\n\
         ## Spec-driven workflow\n\
         For non-trivial work, write the intent in a `SPEC.md` first (goal, constraints, \
         acceptance criteria), then ask sirbone to \"implement SPEC.md\". You own the spec; \
         sirbone translates it to code — fewer iterations, less drift.\n\n\
         ## Notes (fill in)\n\
         - **Architecture**: <how the pieces fit together>\n\
         - **Build / test**: <commands>\n\
         - **Conventions**: <style, patterns to follow>\n",
        index.files.len(),
        edges.len(),
    )
}

// --- Graph cache: per-file edges, only changed files are re-scanned ---------

/// One source file's resolved dependencies: its mtime when scanned plus the
/// files it references (paths relative to root).
#[derive(Serialize, Deserialize)]
struct FileRefs {
    mtime: SystemTime,
    targets: Vec<PathBuf>,
}

/// Per-file edge cache keyed by the unique-def table's fingerprint. Edits that
/// don't change the def set (the common case: body-only edits) re-scan just the
/// touched files; a def added/removed/moved changes `names_fp` and forces a
/// full rebuild, because the alternation regex itself changed.
#[derive(Serialize, Deserialize, Default)]
struct GraphCache {
    names_fp: u64,
    files: HashMap<PathBuf, FileRefs>,
}

fn graph_cache_path(root: &Path) -> PathBuf {
    crate::project_store::project_dir(root).join("graph.bin")
}

/// Hash of the unique-def table (name → relative defining file), sorted via
/// BTreeMap iteration. Changes exactly when the alternation regex (or a
/// symbol's resolution target) would — that's the full-rebuild key.
/// (DefaultHasher isn't stable across Rust versions → a toolchain bump forces
/// one harmless rebuild, never stale data.)
fn names_fingerprint(root: &Path, unique: &std::collections::BTreeMap<String, PathBuf>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (name, path) in unique {
        name.hash(&mut h);
        path.strip_prefix(root).unwrap_or(path).hash(&mut h);
    }
    h.finish()
}

/// Pure core of [`graph_cached`]: reuse per-file targets whose mtime is
/// unchanged (when the def set still matches), re-scan the rest in parallel,
/// drop files no longer indexed. Returns the refreshed cache plus the edges.
fn rebuild_graph(
    root: &Path,
    index: &Index,
    old: GraphCache,
) -> (GraphCache, Vec<(PathBuf, PathBuf)>) {
    let unique = unique_defs(index);
    let names_fp = names_fingerprint(root, &unique);
    let old_files = if old.names_fp == names_fp {
        old.files
    } else {
        HashMap::new()
    };

    let mut files: HashMap<PathBuf, FileRefs> = HashMap::new();
    let mut to_scan: Vec<(PathBuf, PathBuf, SystemTime)> = Vec::new(); // (abs, rel, mtime)
    for (abs, cache) in &index.files {
        let rel = abs.strip_prefix(root).unwrap_or(abs).to_path_buf();
        match old_files.get(&rel) {
            Some(fr) if fr.mtime >= cache.mtime => {
                files.insert(
                    rel,
                    FileRefs {
                        mtime: fr.mtime,
                        targets: fr.targets.clone(),
                    },
                );
            }
            _ => to_scan.push((abs.clone(), rel, cache.mtime)),
        }
    }
    if let Some(alt) = alt_regex(&unique) {
        let scanned = par_map(to_scan, |(abs, rel, mtime)| {
            let targets: Vec<PathBuf> = read_source(&abs)
                .map(|c| file_targets(&abs, &c, &alt, &unique))
                .unwrap_or_default()
                .into_iter()
                .map(|t| t.strip_prefix(root).unwrap_or(&t).to_path_buf())
                .collect();
            (rel, FileRefs { mtime, targets })
        });
        files.extend(scanned);
    }

    let mut edges: Vec<(PathBuf, PathBuf)> = files
        .iter()
        .flat_map(|(src, fr)| {
            fr.targets
                .iter()
                .map(move |t| (root.join(src), root.join(t)))
        })
        .collect();
    edges.sort();
    edges.dedup();
    (GraphCache { names_fp, files }, edges)
}

/// File→file edges for `root`, incrementally maintained: per-file results are
/// cached in `graph.bin`, so a body-only edit re-scans one file instead of the
/// whole repo (the old all-or-nothing fingerprint re-read every file on any
/// edit). This is what makes repeated `graph`/`chain` queries fast and always
/// current.
pub fn graph_cached(root: &Path, index: &Index) -> Vec<(PathBuf, PathBuf)> {
    let path = graph_cache_path(root);
    let old: GraphCache = std::fs::read(&path)
        .ok()
        .and_then(|b| bincode::deserialize(&b).ok())
        .unwrap_or_default();
    let (cache, edges) = rebuild_graph(root, index, old);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = bincode::serialize(&cache) {
        let _ = std::fs::write(&path, bytes);
    }
    edges
}

// --- Persistence (reuses the per-project slug, no repo artifacts) -----------

fn cache_path(root: &Path) -> PathBuf {
    crate::project_store::project_dir(root).join("structure.bin")
}

impl Index {
    /// Load the cached index for `root`, or an empty one if absent/corrupt.
    /// Paths are stored relative to `root`; restored to absolute for use.
    pub fn load(root: &Path) -> Index {
        let files: std::collections::HashMap<PathBuf, FileCache> = std::fs::read(cache_path(root))
            .ok()
            .and_then(|b| bincode::deserialize(&b).ok())
            .unwrap_or_default();
        Index {
            files: files.into_iter().map(|(p, c)| (root.join(p), c)).collect(),
        }
    }

    /// Persist as bincode under `~/.sirbone/projects/<slug>/`, with paths stored
    /// **relative to `root`** so the absolute prefix isn't repeated per file.
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = cache_path(root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create cache dir {}", dir.display()))?;
        }
        let rel: std::collections::HashMap<&Path, &FileCache> = self
            .files
            .iter()
            .map(|(p, c)| (p.strip_prefix(root).unwrap_or(p.as_path()), c))
            .collect();
        let bytes = bincode::serialize(&rel).context("cannot serialize structure index")?;
        std::fs::write(&path, bytes).context("cannot write structure cache")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_regex_catches_pub_and_async_items() {
        let src = r#"
            pub fn public_fn() {}
            pub async fn async_fn() {}
            pub(crate) fn crate_fn() {}
            fn private_fn() {}
            pub struct MyStruct;
            pub enum MyEnum {}
            pub trait MyTrait {}
            use std::collections::HashMap;
            use crate::foo::Bar;
        "#;
        let s = parse_str(Lang::Rust, src);
        for want in [
            "public_fn",
            "async_fn",
            "crate_fn",
            "private_fn",
            "MyStruct",
            "MyEnum",
            "MyTrait",
        ] {
            assert!(
                s.defs.contains(&want.to_string()),
                "missing def {want}: {:?}",
                s.defs
            );
        }
        assert!(s.imports.iter().any(|i| i.contains("HashMap")));
    }

    #[test]
    fn python_and_sql_extraction() {
        let py = "import os\nfrom a.b import c\nclass Foo:\n    async def bar(self): pass\n";
        let s = parse_str(Lang::Python, py);
        assert!(s.defs.contains(&"Foo".to_string()));
        assert!(s.defs.contains(&"bar".to_string()));
        assert!(s.imports.contains(&"os".to_string()));

        let sql = "CREATE TABLE users (id INT);\nCREATE VIEW active AS SELECT * FROM users;\nFOREIGN KEY (uid) REFERENCES users(id);";
        let s = parse_str(Lang::Sql, sql);
        assert!(s.defs.contains(&"users".to_string()));
        assert!(s.defs.contains(&"active".to_string()));
        assert!(s.imports.contains(&"users".to_string()));
    }

    #[test]
    fn discover_prunes_node_modules_and_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(root.join("ignored.rs"), "fn nope() {}").unwrap();
        // huge dependency dir with no .gitignore entry -> hardcoded prune
        let nm = root.join("node_modules").join("pkg");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("dep.js"), "function x() {}").unwrap();

        let found: Vec<String> = discover(root)
            .into_iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(found.contains(&"main.rs".to_string()));
        assert!(
            !found.contains(&"dep.js".to_string()),
            "node_modules not pruned: {found:?}"
        );
        assert!(
            !found.contains(&"ignored.rs".to_string()),
            ".gitignore not honoured: {found:?}"
        );
    }

    #[test]
    fn update_reuses_unchanged_and_drops_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = root.join("a.rs");
        std::fs::write(&a, "fn first() {}").unwrap();

        let idx = update(root, Index::default());
        assert_eq!(idx.files.len(), 1);
        assert!(idx.files[&a].data.defs.contains(&"first".to_string()));

        // Unchanged file -> entry reused (same mtime).
        let idx2 = update(root, idx);
        assert!(idx2.files[&a].data.defs.contains(&"first".to_string()));

        // Delete the file -> dropped from the new index.
        std::fs::remove_file(&a).unwrap();
        let idx3 = update(root, idx2);
        assert!(
            idx3.files.is_empty(),
            "deleted file not GC'd: {:?}",
            idx3.files
        );
    }

    #[test]
    fn find_references_whole_word_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("def.rs"), "fn App() {}").unwrap();
        std::fs::write(root.join("user.rs"), "fn x() { App(); }").unwrap();
        std::fs::write(root.join("nomatch.rs"), "fn y() { Apple(); }").unwrap();

        let refs: Vec<String> = find_references(root, "App")
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(refs.contains(&"user.rs".to_string()));
        assert!(refs.contains(&"def.rs".to_string()));
        assert!(
            !refs.contains(&"nomatch.rs".to_string()),
            "Apple matched App: {refs:?}"
        );
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn callers_finds_type_usage_without_parens() {
        // a.rs defines Foo (a struct, never used as `Foo(`) and fn alpha calling beta.
        // b.rs defines beta and uses Foo as a return type → the key case the
        // `word(` heuristic would miss.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("a.rs"),
            "pub struct Foo {}\npub fn alpha() { beta(); }",
        )
        .unwrap();
        std::fs::write(root.join("b.rs"), "pub fn beta() -> Foo { Foo {} }").unwrap();
        let index = update(root, Index::default());

        let c = callers(&index, "Foo");
        assert_eq!(names(&c.defined_in), ["a.rs"]);
        assert_eq!(
            names(&c.referenced_by),
            ["b.rs"],
            "type usage `-> Foo` not found"
        );

        let c = callers(&index, "beta");
        assert_eq!(names(&c.defined_in), ["b.rs"]);
        assert_eq!(names(&c.referenced_by), ["a.rs"], "call `beta()` not found");
    }

    #[test]
    fn callees_resolves_cross_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "pub struct Foo {}").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn beta() -> Foo { beta(); Foo {} }").unwrap();
        let index = update(root, Index::default());

        // beta is defined in b.rs; it references Foo (a.rs) and itself (skipped).
        let deps = callees(&index, "beta");
        let rendered: Vec<(String, String)> = deps
            .into_iter()
            .map(|(s, p)| (s, p.file_name().unwrap().to_string_lossy().into_owned()))
            .collect();
        assert_eq!(rendered, [("Foo".to_string(), "a.rs".to_string())]);
    }

    #[test]
    fn graph_rebuild_is_incremental_and_correct() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "pub struct Foo {}").unwrap();
        let b = root.join("b.rs");
        std::fs::write(&b, "pub fn beta() {}").unwrap();

        let idx = update(root, Index::default());
        let (cache, edges) = rebuild_graph(root, &idx, GraphCache::default());
        assert!(edges.is_empty(), "no references yet: {edges:?}");

        // Body-only edit (def set unchanged → names_fp stable → only b.rs is
        // re-scanned): the new Foo reference must still surface as an edge.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&b, "pub fn beta() -> Foo { Foo {} }").unwrap();
        let idx = update(root, idx);
        let (cache, edges) = rebuild_graph(root, &idx, cache);
        assert_eq!(edges, [(b.clone(), root.join("a.rs"))]);

        // Def-set change (new file/symbol → names_fp changes → full rebuild).
        std::fs::write(root.join("c.rs"), "pub fn gamma() { beta(); }").unwrap();
        let idx = update(root, idx);
        let (cache, edges) = rebuild_graph(root, &idx, cache);
        assert!(
            edges.contains(&(root.join("c.rs"), b.clone())),
            "new edge missing: {edges:?}"
        );

        // Deleted file → its cache entry and edges are dropped.
        std::fs::remove_file(root.join("c.rs")).unwrap();
        let idx = update(root, idx);
        let (_, edges) = rebuild_graph(root, &idx, cache);
        assert_eq!(
            edges,
            [(b.clone(), root.join("a.rs"))],
            "stale edge survived deletion"
        );
    }

    #[test]
    fn signatures_capture_params_and_return_types() {
        let s = parse_str(
            Lang::Rust,
            "pub fn auth(user: &str, n: [u8; 4]) -> Result<(), E> {\n}",
        );
        assert_eq!(
            s.sigs,
            ["pub fn auth(user: &str, n: [u8; 4]) -> Result<(), E>"]
        );
        let s = parse_str(Lang::Rust, "pub struct Foo;\n");
        assert_eq!(s.sigs, ["pub struct Foo"]);
        // multi-line param list collapses to one line, stops at `{`
        let s = parse_str(
            Lang::Rust,
            "fn multi(\n    a: u32,\n    b: u32,\n) -> u32 {\n",
        );
        assert_eq!(s.sigs, ["fn multi( a: u32, b: u32, ) -> u32"]);
        // Python: `:` inside parens/brackets/braces must not terminate
        let s = parse_str(
            Lang::Python,
            "def f(x: int = 1, m={'a': 1}) -> dict[str, int]:\n    pass\n",
        );
        assert_eq!(s.sigs, ["def f(x: int = 1, m={'a': 1}) -> dict[str, int]"]);
        // JS arrow keeps the arrow, drops the body
        let s = parse_str(Lang::JsTs, "export const go = async (a, b) => a + b\n");
        assert_eq!(s.sigs, ["export const go = async (a, b) =>"]);
        let s = parse_str(Lang::JsTs, "function fetchAll(urls) {\n  return 1;\n}");
        assert_eq!(s.sigs, ["function fetchAll(urls)"]);
        // blank line before the decl: `^\s*` starts the match there — the sig
        // must not come out empty (regression)
        let s = parse_str(Lang::Rust, "}\n\npub fn after_blank(x: u32) {}");
        assert_eq!(s.sigs, ["pub fn after_blank(x: u32)"]);
    }

    #[test]
    fn sigs_stay_aligned_with_defs() {
        let s = parse_str(Lang::Rust, "pub fn zeta() {}\npub fn alpha(x: u32) {}");
        assert_eq!(s.defs, ["alpha", "zeta"]);
        assert_eq!(s.sigs, ["pub fn alpha(x: u32)", "pub fn zeta()"]);
        // SQL has no useful signature — name doubles as sig, alignment holds
        let s = parse_str(Lang::Sql, "CREATE TABLE users (id INT);");
        assert_eq!(s.defs, s.sigs);
    }

    #[test]
    fn long_signature_truncated() {
        let src = format!("fn long({}) {{}}", "a: u64, ".repeat(40));
        let s = parse_str(Lang::Rust, &src);
        assert_eq!(s.defs, ["long"]);
        assert!(s.sigs[0].len() <= MAX_SIG_LEN + '…'.len_utf8());
        assert!(s.sigs[0].ends_with('…'), "no ellipsis: {}", s.sigs[0]);
    }

    #[test]
    fn init_doc_has_overview_and_key_modules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "pub struct Foo {}").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn bridge() -> Foo { Foo {} }").unwrap();
        let index = update(root, Index::default());
        let md = init_doc(root, &index, &graph_cached(root, &index));
        assert!(md.contains("## Overview"));
        assert!(md.contains("Rust (2 files"));
        assert!(
            md.contains("a.rs"),
            "key module a.rs (used by b.rs) missing: {md}"
        );
    }

    #[test]
    fn usage_chain_is_transitive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "pub struct Foo {}").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn bridge() -> Foo { Foo {} }").unwrap();
        std::fs::write(root.join("c.rs"), "pub fn top() { bridge(); }").unwrap();
        let index = update(root, Index::default());

        let ch = usage_chain(&index, &graph_cached(root, &index), "Foo");
        assert_eq!(names(&ch.roots), ["a.rs"]);
        let levels: Vec<(usize, String)> = ch.levels.iter().map(|(d, p)| (*d, file(p))).collect();
        assert!(
            levels.contains(&(1, "b.rs".into())),
            "direct user missing: {levels:?}"
        );
        assert!(
            levels.contains(&(2, "c.rs".into())),
            "transitive user missing: {levels:?}"
        );
    }

    #[test]
    fn graph_links_unique_defs_cross_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "pub struct Foo {}").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn beta() -> Foo { Foo {} }").unwrap();
        std::fs::write(root.join("c.rs"), "pub fn gamma() { beta(); }").unwrap();
        let index = update(root, Index::default());

        let edges: Vec<(String, String)> = graph_cached(root, &index)
            .into_iter()
            .map(|(a, b)| (file(&a), file(&b)))
            .collect();
        // b uses Foo (a); c calls beta (b). No self-edges.
        assert!(edges.contains(&("b.rs".into(), "a.rs".into())));
        assert!(edges.contains(&("c.rs".into(), "b.rs".into())));
        assert!(!edges.iter().any(|(a, b)| a == b));
    }

    fn file(p: &Path) -> String {
        p.file_name().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn disambiguation_drops_method_and_assoc_calls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "pub fn alpha() {}").unwrap();
        // method call on a chain (newline before `.`) + associated `Foo::alpha`
        std::fs::write(
            root.join("b.rs"),
            "pub fn beta() {\n    obj\n        .alpha();\n    Foo::alpha();\n}",
        )
        .unwrap();
        // genuine free call
        std::fs::write(root.join("c.rs"), "pub fn gamma() { alpha(); }").unwrap();
        let index = update(root, Index::default());

        let c = callers(&index, "alpha");
        assert_eq!(names(&c.defined_in), ["a.rs"]);
        assert_eq!(
            names(&c.referenced_by),
            ["c.rs"],
            "method/assoc calls must not count: {:?}",
            names(&c.referenced_by)
        );
    }

    #[test]
    fn module_path_call_is_kept() {
        // `crate::a::alpha()` — lowercase module qualifier → a real edge, kept.
        let bytes = b"crate::a::alpha()";
        let start = bytes.len() - "alpha()".len();
        assert!(
            !should_skip_match(bytes, start),
            "module-path free call wrongly dropped"
        );
        // `Foo::alpha()` — type qualifier → dropped.
        let bytes = b"Foo::alpha()";
        assert!(should_skip_match(bytes, bytes.len() - "alpha()".len()));
        // `$alpha` (JS) — sub-identifier → dropped.
        assert!(should_skip_match(b"$alpha", 1));
    }

    #[test]
    fn test_module_defs_excluded() {
        let src = "pub fn real() {}\n\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n    fn file() {}\n}";
        let s = parse_str(Lang::Rust, src);
        assert!(s.defs.contains(&"real".to_string()));
        assert!(
            !s.defs.contains(&"helper".to_string()),
            "test-module def leaked: {:?}",
            s.defs
        );
        assert!(!s.defs.contains(&"file".to_string()));
        // a lone `#[cfg(test)] fn` must NOT truncate real code after it
        let src2 = "#[cfg(test)]\nfn only_test() {}\npub fn after() {}";
        assert!(parse_str(Lang::Rust, src2)
            .defs
            .contains(&"after".to_string()));
        // the pattern inside a doc-comment/string must NOT truncate (regression:
        // line-start anchor) — `real_after` is declared below such a mention.
        let src3 = "/// see #[cfg(test)] mod tests for examples\npub fn real_after() {}";
        assert!(
            parse_str(Lang::Rust, src3)
                .defs
                .contains(&"real_after".to_string()),
            "doc-comment mention truncated real code"
        );
    }

    #[test]
    fn caps_skip_minified_and_binary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("ok.rs"), "pub fn real() {}").unwrap();
        // one 300-char line → mean line > 255 → minified → skipped
        std::fs::write(root.join("min.js"), format!("var x={};", "a".repeat(300))).unwrap();
        // NUL byte → binary → skipped
        std::fs::write(root.join("blob.py"), "def f():\0\n    pass").unwrap();
        let index = update(root, Index::default());

        let kept: Vec<String> = index.files.keys().map(|p| file(p)).collect();
        assert!(kept.contains(&"ok.rs".to_string()));
        assert!(
            !kept.contains(&"min.js".to_string()),
            "minified not skipped: {kept:?}"
        );
        assert!(
            !kept.contains(&"blob.py".to_string()),
            "binary not skipped: {kept:?}"
        );
    }
}
