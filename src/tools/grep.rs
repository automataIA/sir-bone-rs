use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

use super::code_map::{render_refs, REF_LINES_PER_FILE};
use super::{truncate::truncate_default, TypedTool};
use crate::structure::{self, Index};

#[derive(Deserialize, JsonSchema)]
pub struct GrepInput {
    /// Regex pattern to search for (extended syntax: `a|b` alternates, `\(` is a
    /// literal paren)
    pub pattern: String,
    /// File or directory to search (default: the working directory)
    #[serde(default = "default_path")]
    pub path: String,
    /// File glob to include (e.g. "*.rs")
    pub include: Option<String>,
    /// Case-insensitive search
    #[serde(default)]
    pub case_insensitive: bool,
    /// Maximum number of results (default: 100)
    #[serde(default = "default_max")]
    pub max_results: u32,
    /// Plain matching lines even for a bare identifier with many matches
    #[serde(default)]
    pub raw: bool,
}

fn default_max() -> u32 {
    100
}

/// `path` used to be required, and a call that omitted it died on serde's
/// `missing field \`path\`` — 3 of the 13 failing `grep` calls in the local
/// session corpus. The model was writing the ripgrep contract, where the
/// working directory is the default; the tool now agrees with it.
fn default_path() -> String {
    ".".into()
}

pub struct GrepTool;

#[async_trait]
impl TypedTool for GrepTool {
    type Input = GrepInput;

    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search for a pattern in files. Returns matching lines with file:line format. \
         Respects .gitignore when rg (ripgrep) is available. Prefer this over \
         running grep/rg through bash."
    }

    async fn run(&self, input: GrepInput) -> Result<String> {
        let raw = match run_rg(&input).await {
            Ok(out) => out,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                run_grep(&input).await.context("grep failed to run")?
            }
            Err(e) => return Err(e).context("rg failed to run"),
        };

        let stdout = String::from_utf8_lossy(&raw.stdout);
        let stderr = String::from_utf8_lossy(&raw.stderr);

        if stdout.is_empty() {
            let (diagnostic, unreadable) = split_stderr(&stderr);
            return match diagnostic {
                Some(d) => Err(anyhow::anyhow!("{d}")),
                None if unreadable > 0 => Ok(format!(
                    "no matches ({unreadable} path(s) skipped: unreadable)"
                )),
                None => Ok("no matches".into()),
            };
        }

        let out = match structural_view(&input, &stdout).await {
            Some(view) => view,
            None => stdout
                .lines()
                .take(input.max_results as usize)
                .collect::<Vec<_>>()
                .join("\n"),
        };
        Ok(truncate_default(match body_view(&input).await {
            Some(body) => format!("{out}\n\n{body}"),
            None => out,
        }))
    }
}

/// Lines of a definition body appended to a grep answer, at most.
const BODY_MAX_LINES: usize = 60;

/// Opt-in (`SIRBONE_ROUTE_BODY=1`, under A/B): when a bare identifier search
/// names a symbol declared in exactly one file, append that declaration's
/// body. The commonest navigation idiom in the session corpus is grep for a
/// name, then `sed -n` its line range (20.7 pairs per 100 navigation calls,
/// `bench/harness_levers_probe/results.md`); this answers both in one call.
async fn body_view(input: &GrepInput) -> Option<String> {
    if std::env::var_os("SIRBONE_ROUTE_BODY").is_none()
        || input.raw
        || input.case_insensitive
        || crate::ablate::grep_route_disabled()
    {
        return None;
    }
    let symbol = bare_symbol(&input.pattern)?.to_owned();
    let root = std::env::current_dir().ok()?.canonicalize().ok()?;
    let body = tokio::task::spawn_blocking(move || definition_body(&root, &symbol))
        .await
        .ok()
        .flatten()?;
    crate::telemetry::add(&crate::telemetry::ROUTE_BODIES, 1);
    Some(body)
}

/// The body of the single declaration of `symbol`, or `None` when it is
/// declared in zero or several files, or the body is a single line (the grep
/// line already shows it).
fn definition_body(root: &Path, symbol: &str) -> Option<String> {
    let index = structure::update(root, Index::load(root));
    let _ = index.save(root);
    let [file] = structure::declarations(&index, symbol).try_into().ok()?;
    let path = root.join(&file);
    let content = std::fs::read_to_string(&path).ok()?;
    let lang = structure::lang_for_path(&path)?;
    let word = Regex::new(&format!(r"\b{}\b", regex::escape(symbol))).ok()?;
    let decl = structure::outline(lang, &content)
        .into_iter()
        .find(|d| word.is_match(&d.sig))?;
    let lines: Vec<&str> = content.lines().collect();
    let (start, end) = body_range(&lines, decl.line - 1);
    if end - start < 2 {
        return None;
    }
    let shown = end.min(start + BODY_MAX_LINES);
    let rel = path.strip_prefix(root).unwrap_or(&path).display();
    let mut out = format!(
        "[definition of `{symbol}` — {rel}:{}-{end}]\n",
        start + 1
    );
    for (i, l) in lines[start..shown].iter().enumerate() {
        out.push_str(&format!("{}: {l}\n", start + i + 1));
    }
    if shown < end {
        out.push_str(&format!(
            "… {} more lines: read {rel} from line {}\n",
            end - shown,
            shown + 1
        ));
    }
    Some(out)
}

/// Half-open line range of the declaration starting at `start` (0-based).
/// It ends at the first non-blank line indented no deeper than the
/// declaration: a closing `}`/`)`/`]` there is included (brace languages),
/// anything else is not (the next item; Python's dedent).
fn body_range(lines: &[&str], start: usize) -> (usize, usize) {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let base = indent(lines[start]);
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        let t = l.trim_start();
        if t.is_empty() || indent(l) > base {
            continue;
        }
        end = if t.starts_with(['}', ')', ']']) { i + 1 } else { i };
        break;
    }
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    (start, end)
}

/// Matching lines past which a bare identifier search is answered with the
/// `code_map find_references` view instead of raw lines. That view is bounded
/// (40 content lines plus one inventory row per further file), so below this
/// plain grep is already as small and the routing could only lose information
/// — ast-grep's outline measured exactly that regression on small repos.
const ROUTE_MIN_LINES: usize = 40;

/// Leads the routed answer, so the model knows what it got and how to opt out.
const ROUTE_NOTE: &str = "[structural view: whole-word matches, declaring line always \
    shown, every file listed; raw=true for plain grep lines]";

/// The same for a shell search, whose opt-out is a pipe: piped commands are
/// never routed.
const SHELL_ROUTE_NOTE: &str = "[structural view: whole-word matches, declaring line always \
    shown, every file listed; pipe through `cat` for plain grep lines]";

/// Long flags a routed shell search may carry: scope and formatting only. A
/// flag that changes what a line means (`--count`, `--invert-match`,
/// `--hidden`) is not here, so its command is left alone.
const SHELL_LONG_FLAGS: &[&str] = &[
    "include",
    "exclude",
    "exclude-dir",
    "recursive",
    "line-number",
    "word-regexp",
    "color",
    "colour",
    "type",
    "type-not",
    "glob",
    "no-heading",
    "with-filename",
];

/// A bare identifier, optionally wrapped in `\b`: the "where is X" search. A
/// qualified form (`fn x`, `struct X`, `x\(`) is a declaration or call search
/// that plain grep already answers in a few lines, so it is left alone.
static BARE_SYMBOL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\\b)?([A-Za-z_]\w{2,})(?:\\b)?$").expect("static regex"));

fn bare_symbol(pattern: &str) -> Option<&str> {
    BARE_SYMBOL
        .captures(pattern)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

/// The routed answer for a noisy bare identifier search, or `None` to keep
/// the plain lines: opted out, case-insensitive (the index is exact-case), not
/// a bare identifier, or too few matches to be worth it.
async fn structural_view(input: &GrepInput, stdout: &str) -> Option<String> {
    if input.raw || input.case_insensitive || crate::ablate::grep_route_disabled() {
        return None;
    }
    let symbol = bare_symbol(&input.pattern)?.to_owned();
    routed(symbol, stdout, ROUTE_NOTE).await
}

/// [`structural_view`] for a `bash` command that is one plain recursive search
/// for a bare identifier: the model reaches for shell grep about half as often
/// as for the tool, and the noise is the same. Opt-in (`SIRBONE_SHELL_ROUTE=1`):
/// on edit tasks it never fired, the model pipes its searches
/// (`grep -rlw X | xargs sed`), so it has no measured effect to ship by default.
pub(crate) async fn shell_structural_view(command: &str, stdout: &str) -> Option<String> {
    if std::env::var("SIRBONE_SHELL_ROUTE").as_deref() != Ok("1")
        || crate::ablate::grep_route_disabled()
    {
        return None;
    }
    let symbol = shell_search_symbol(command)?.to_owned();
    routed(symbol, stdout, SHELL_ROUTE_NOTE).await
}

async fn routed(symbol: String, stdout: &str, note: &'static str) -> Option<String> {
    if stdout.lines().count() <= ROUTE_MIN_LINES {
        return None;
    }
    let root = std::env::current_dir().ok()?.canonicalize().ok()?;
    let stdout = stdout.to_owned();
    tokio::task::spawn_blocking(move || route(&root, &symbol, &stdout, note))
        .await
        .ok()
        .flatten()
}

/// The symbol of a shell command that is a single recursive `grep`/`rg` for a
/// bare identifier (`grep -rn X src`, `rg -w X -t rust`), else `None`. Only
/// allow-listed flags pass, so anything that changes what the output means
/// (`-l`, `-c`, `-v`, `-i`, context lines) or that the model will post-process
/// (a pipe, a redirect, `&&`) keeps its plain output.
fn shell_search_symbol(command: &str) -> Option<&str> {
    if command.contains(['|', ';', '&', '<', '>', '$', '`', '(', '\n']) {
        return None;
    }
    let mut tokens = command.split_whitespace();
    let tool = tokens.next()?;
    let (short_flags, arg_flags): (&str, &[&str]) = match tool {
        "grep" => ("rRnwHIsEF", &[]),
        "rg" => (
            "nwHIsF",
            &["-t", "-T", "-g", "--type", "--type-not", "--glob"],
        ),
        _ => return None,
    };
    // grep without -r searches named files, whose lines carry no path to scope by.
    let mut recursive = tool == "rg";
    let mut pattern = None;
    while let Some(tok) = tokens.next() {
        if arg_flags.contains(&tok) {
            tokens.next()?;
        } else if let Some(long) = tok.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            if !SHELL_LONG_FLAGS.contains(&name) {
                return None;
            }
            recursive |= name == "recursive";
        } else if let Some(short) = tok.strip_prefix('-').filter(|s| !s.is_empty()) {
            if !short.chars().all(|c| short_flags.contains(c)) {
                return None;
            }
            recursive |= short.contains(['r', 'R']);
        } else if pattern.is_none() {
            pattern = Some(unquote(tok)?);
        }
        // Further positionals are paths: rg's file set already reflects them.
    }
    if !recursive {
        return None;
    }
    bare_symbol(pattern?)
}

/// A shell word without its enclosing quotes; `None` for a quote left open
/// (a pattern with spaces, split by the whitespace tokenizer).
fn unquote(tok: &str) -> Option<&str> {
    for q in ['\'', '"'] {
        if let Some(inner) = tok.strip_prefix(q) {
            return inner.strip_suffix(q).filter(|s| !s.contains(q));
        }
    }
    (!tok.contains(['\'', '"'])).then_some(tok)
}

/// `code_map find_references` for `symbol`, restricted to the files `rg` hit:
/// rg already applied `path`, `include` and `.gitignore`, so its file set is
/// the scope the model asked for. `None` when no whole-word hit survives (an
/// rg match on `IndexMap` for `Index`), so the caller falls back to raw lines.
fn route(root: &Path, symbol: &str, stdout: &str, note: &str) -> Option<String> {
    let scope: HashSet<PathBuf> = stdout
        .lines()
        .filter_map(|l| l.split_once(':').map(|(p, _)| p))
        .collect::<HashSet<&str>>()
        .into_iter()
        .filter_map(|p| Path::new(p).canonicalize().ok())
        .collect();
    let index = structure::update(root, Index::load(root));
    // Best-effort cache: the answer below does not depend on it persisting.
    let _ = index.save(root);
    let hits: Vec<_> = structure::find_reference_lines(root, symbol, REF_LINES_PER_FILE, false)
        .into_iter()
        .filter(|h| scope.contains(&h.path))
        .collect();
    if hits.is_empty() {
        return None;
    }
    let definers = structure::declarations(&index, symbol);
    let view = render_refs(root, &hits, symbol, false, &definers);
    Some(format!(
        "{note}\n{view}{}",
        skipped_note(root, symbol, &scope, &hits)
    ))
}

/// Files rg matched whole-word that the reference scan does not read (pruned
/// build dirs, minified or oversized files), named so the view's "every file
/// listed" stays true instead of a gap that reads as "not used there".
fn skipped_note(
    root: &Path,
    symbol: &str,
    scope: &HashSet<PathBuf>,
    hits: &[structure::RefLines],
) -> String {
    const MAX_NAMED: usize = 20;
    let Ok(re) = Regex::new(&format!(r"\b{}\b", regex::escape(symbol))) else {
        return String::new();
    };
    let seen: HashSet<&PathBuf> = hits.iter().map(|h| &h.path).collect();
    let mut skipped: Vec<String> = scope
        .iter()
        .filter(|p| !seen.contains(p))
        .filter(|p| std::fs::read(p).is_ok_and(|b| re.is_match(&String::from_utf8_lossy(&b))))
        .map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string())
        .collect();
    if skipped.is_empty() {
        return String::new();
    }
    skipped.sort_unstable();
    let more = skipped.len().saturating_sub(MAX_NAMED);
    let more = if more > 0 { format!(", +{more} more") } else { String::new() };
    let names = skipped[..skipped.len().min(MAX_NAMED)].join(", ");
    format!(
        "  … {} more file(s) match but are not in this view (build dir, minified or \
         oversized; raw=true for their lines): {names}{more}\n",
        skipped.len()
    )
}

/// Separate a real diagnostic from the per-file noise a recursive search emits.
///
/// A recursive walk prints one line per path it cannot open, and a build
/// directory produces hundreds; the old code returned the whole block as the
/// error, so the real cause — when there was one — arrived buried, and when
/// there was none a plain "no matches" was reported as a failure (observed on
/// `AGENTS\.md|agents\.md` over this repo, drowned in `target/` lock files).
/// Returns the first real lines, if any, plus how many paths were skipped.
fn split_stderr(stderr: &str) -> (Option<String>, usize) {
    let (noise, real): (Vec<&str>, Vec<&str>) = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .partition(|l| l.contains("Permission denied") || l.contains("Is a directory"));
    let diagnostic =
        (!real.is_empty()).then(|| real.iter().take(3).copied().collect::<Vec<_>>().join("\n"));
    (diagnostic, noise.len())
}

async fn run_rg(input: &GrepInput) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("rg");
    cmd.env("LC_ALL", "C");
    cmd.arg("--no-heading").arg("-n");
    if input.case_insensitive {
        cmd.arg("-i");
    }
    if let Some(inc) = &input.include {
        cmd.arg("-g").arg(inc);
    }
    cmd.arg(&input.pattern).arg(&input.path);
    cmd.output().await
}

/// Plain-`grep` fallback, used only where `rg` is absent.
///
/// `-E` is load-bearing, not a preference. `grep -r` defaults to *basic*
/// regular expressions, where `|` is a literal pipe and `\(` opens a group —
/// so the ordinary pattern a model writes either dies (`add\(` →
/// "Unmatched ( or \(", 8 of the 13 failing `grep` calls in the local session
/// corpus) or, worse, succeeds wrongly: `add|ground` matches nothing and the
/// tool answers **"no matches"**, telling the model a symbol does not exist.
/// Extended syntax is what `rg` accepts on the primary path, so the same
/// pattern now means the same thing whether or not ripgrep is installed.
/// `LC_ALL=C` keeps a syntax error in English instead of the user's locale.
/// Parity is close, not exact: `\d` and lazy quantifiers stay ripgrep-only.
async fn run_grep(input: &GrepInput) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("grep");
    cmd.env("LC_ALL", "C");
    cmd.arg("-rnE");
    for dir in exclude_dirs().await {
        cmd.arg(format!("--exclude-dir={dir}"));
    }
    if input.case_insensitive {
        cmd.arg("-i");
    }
    if let Some(inc) = &input.include {
        cmd.arg("--include").arg(inc);
    }
    cmd.arg(&input.pattern).arg(&input.path);
    cmd.output().await
}

/// Directories the plain-`grep` fallback should skip. Unlike `rg`, `grep -r`
/// ignores `.gitignore`, so without this it walks build artifacts (a multi-GB
/// `target/`, `node_modules/`, …) and stalls for minutes. Seeds a default set,
/// then folds in simple directory names from `.gitignore` when one is present.
async fn exclude_dirs() -> Vec<String> {
    let mut dirs: Vec<String> = [
        "target",
        ".git",
        "node_modules",
        "dist",
        "build",
        ".venv",
        "__pycache__",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Ok(gitignore) = tokio::fs::read_to_string(".gitignore").await {
        for line in gitignore.lines() {
            let entry = line.trim().trim_end_matches('/');
            // Skip blanks, comments, negations, globs and nested paths — keep
            // only plain directory names that `--exclude-dir` matches by base.
            if entry.is_empty()
                || entry.starts_with('#')
                || entry.starts_with('!')
                || entry.contains('*')
                || entry.contains('/')
            {
                continue;
            }
            if !dirs.iter().any(|d| d == entry) {
                dirs.push(entry.to_string());
            }
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn grep_finds_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "hello world\nfoo bar\nhello again").unwrap();
        let tool = GrepTool;
        let result = tool
            .run(GrepInput {
                pattern: "hello".into(),
                path: path.to_str().unwrap().into(),
                include: None,
                case_insensitive: false,
                max_results: 100,
                raw: false,
            })
            .await
            .unwrap();
        assert!(result.contains("hello"));
        assert!(!result.contains("foo bar"));
    }

    #[tokio::test]
    async fn grep_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();
        let tool = GrepTool;
        let result = tool
            .run(GrepInput {
                pattern: "zzznomatch".into(),
                path: path.to_str().unwrap().into(),
                include: None,
                case_insensitive: false,
                max_results: 100,
                raw: false,
            })
            .await
            .unwrap();
        assert_eq!(result, "no matches");
    }

    /// The failure that produced no error at all: in basic syntax `|` is a
    /// literal pipe, so an alternation matched nothing and the tool reported
    /// "no matches" — a false negative about the codebase, not a tool error.
    #[tokio::test]
    async fn grep_fallback_reads_alternation_as_alternation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn add(x: i32) {}\nfn ground() {}\n",
        )
        .unwrap();
        let out = run_grep(&GrepInput {
            pattern: "add|ground".into(),
            path: dir.path().to_str().unwrap().into(),
            include: None,
            case_insensitive: false,
            max_results: 100,
            raw: false,
        })
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("fn add"), "left branch missing: {stdout}");
        assert!(
            stdout.contains("fn ground"),
            "right branch missing: {stdout}"
        );
    }

    /// The visible half of the same defect: `\(` is a literal paren in extended
    /// syntax and a group opener in basic, where it died as "Unmatched ( or \(".
    #[tokio::test]
    async fn grep_fallback_takes_an_escaped_paren_literally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn add(x: i32) {}\nadd = 1\n").unwrap();
        let out = run_grep(&GrepInput {
            pattern: r"add\(".into(),
            path: dir.path().to_str().unwrap().into(),
            include: None,
            case_insensitive: false,
            max_results: 100,
            raw: false,
        })
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("fn add(x"));
        assert!(!stdout.contains("add = 1"));
        assert!(out.stderr.is_empty(), "syntax error: {:?}", out.stderr);
    }

    /// `missing field \`path\`` was 3 of the 13 observed failures.
    #[test]
    fn path_defaults_to_the_working_directory() {
        let input: GrepInput = serde_json::from_value(serde_json::json!({"pattern": "x"})).unwrap();
        assert_eq!(input.path, ".");
    }

    #[test]
    fn unreadable_paths_are_counted_not_reported_as_the_error() {
        let noise = "grep: /p/a.lock: Permission denied\ngrep: /p/b.lock: Permission denied";
        assert_eq!(split_stderr(noise), (None, 2));

        let (diagnostic, skipped) =
            split_stderr("grep: /p/a.lock: Permission denied\ngrep: Unmatched ( or \\(");
        assert_eq!(skipped, 1);
        assert_eq!(diagnostic.as_deref(), Some("grep: Unmatched ( or \\("));
    }

    #[tokio::test]
    async fn no_match_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "nothing here").unwrap();
        let result = GrepTool
            .run(GrepInput {
                pattern: "zzz|qqq".into(),
                path: dir.path().to_str().unwrap().into(),
                include: None,
                case_insensitive: false,
                max_results: 100,
                raw: false,
            })
            .await
            .unwrap();
        assert_eq!(result, "no matches");
    }

    #[tokio::test]
    async fn grep_fallback_skips_excluded_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/junk.txt"), "needle").unwrap();
        std::fs::write(dir.path().join("real.txt"), "needle").unwrap();
        let out = run_grep(&GrepInput {
            pattern: "needle".into(),
            path: dir.path().to_str().unwrap().into(),
            include: None,
            case_insensitive: false,
            max_results: 100,
            raw: false,
        })
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("real.txt"));
        assert!(!stdout.contains("target/junk.txt"));
    }

    #[test]
    fn bare_symbol_reads_only_bare_identifiers() {
        for (pattern, want) in [
            ("ToolRegistry", Some("ToolRegistry")),
            (r"\bToolRegistry\b", Some("ToolRegistry")),
            ("render_refs", Some("render_refs")),
            ("fn render_refs", None),
            ("struct Widget", None),
            (r"render_refs\(", None),
            ("Foo|Bar", None),
            ("TODO.*fix", None),
            ("ab", None),
            ("self.root", None),
        ] {
            assert_eq!(bare_symbol(pattern), want, "pattern {pattern:?}");
        }
    }

    #[test]
    fn shell_search_symbol_routes_only_plain_recursive_searches() {
        for (command, want) in [
            ("grep -rn ToolRegistry src", Some("ToolRegistry")),
            (
                "grep -rnw 'ToolRegistry' --include=*.rs .",
                Some("ToolRegistry"),
            ),
            (r#"rg -n "\bToolRegistry\b" -t rust"#, Some("ToolRegistry")),
            (
                "rg ToolRegistry --type rust src tests",
                Some("ToolRegistry"),
            ),
            ("grep -n ToolRegistry src/tools/mod.rs", None),
            ("grep -rl ToolRegistry .", None),
            ("grep -rni ToolRegistry .", None),
            ("rg -C3 ToolRegistry", None),
            ("rg --count ToolRegistry", None),
            ("rg ToolRegistry | head", None),
            ("rg -l ToolRegistry | xargs sed -i 's/a/b/'", None),
            ("cd src && rg ToolRegistry", None),
            ("rg 'fn render_refs'", None),
            ("rg -e ToolRegistry", None),
            ("cat ToolRegistry", None),
        ] {
            assert_eq!(shell_search_symbol(command), want, "command {command:?}");
        }
    }

    /// rg output for `symbol` over `dir`, as the tool would see it.
    fn rg_lines(dir: &Path, files: &[&str], symbol: &str) -> String {
        files
            .iter()
            .flat_map(|f| {
                let path = dir.join(f);
                std::fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .enumerate()
                    .filter(|(_, l)| l.contains(symbol))
                    .map(|(i, l)| format!("{}:{}:{l}\n", path.display(), i + 1))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn route_shows_the_declaration_and_stays_in_rg_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("def.rs"), "// x\npub struct Widget;\n").unwrap();
        for f in ["u0.rs", "u1.rs", "u2.rs", "outside.rs"] {
            let body: String = (0..20)
                .map(|i| format!("fn f{i}(_: Widget) {{}}\n"))
                .collect();
            std::fs::write(root.join(f), body).unwrap();
        }
        let stdout = rg_lines(&root, &["u0.rs", "u1.rs", "u2.rs", "def.rs"], "Widget");
        let out = route(&root, "Widget", &stdout, ROUTE_NOTE).expect("routed");
        assert!(out.starts_with(ROUTE_NOTE), "{out}");
        assert!(
            out.contains("def.rs:2:pub struct Widget;"),
            "declaration shown:\n{out}"
        );
        assert!(out.contains("u2.rs:"), "uses shown:\n{out}");
        assert!(!out.contains("outside.rs"), "outside rg scope:\n{out}");
    }

    /// A file rg matched whole-word but the reference scan cannot see (pruned
    /// `build/`, minified, oversized) must still be named: the note promises
    /// every file, and a silent gap reads as "not used there".
    #[test]
    fn route_names_rg_files_the_scan_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let body: String = (0..30)
            .map(|i| format!("fn f{i}(_: Widget) {{}}\n"))
            .collect();
        std::fs::write(root.join("a.rs"), &body).unwrap();
        std::fs::create_dir(root.join("build")).unwrap();
        std::fs::write(root.join("build/gen.rs"), &body).unwrap();
        let stdout = rg_lines(&root, &["a.rs", "build/gen.rs"], "Widget");
        let out = route(&root, "Widget", &stdout, ROUTE_NOTE).expect("routed");
        assert!(out.contains("not in this view") && out.contains("build/gen.rs"), "{out}");
    }

    #[test]
    fn body_range_brace_python_and_eof() {
        let js = ["  function kpis(rows) {", "    let a = 1;", "", "    return a;", "  }", "  function next() {}"];
        assert_eq!(body_range(&js, 0), (0, 5));
        let py = ["def f(x):", "    y = x", "", "    return y", "", "def g():"];
        assert_eq!(body_range(&py, 0), (0, 4));
        assert_eq!(body_range(&["fn a() {", "  1"], 0), (0, 2));
    }

    #[test]
    fn definition_body_needs_a_single_definer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("a.rs"),
            "pub fn widget_total(x: u32) -> u32 {\n    let y = x + 1;\n    y * 2\n}\n\npub fn other() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("b.rs"), "fn dup() {}\n").unwrap();
        std::fs::write(root.join("c.rs"), "fn dup() {}\n").unwrap();
        let out = definition_body(&root, "widget_total").expect("body");
        assert!(out.starts_with("[definition of `widget_total` — a.rs:1-4]"), "{out}");
        assert!(out.contains("3:     y * 2") && !out.contains("other"), "{out}");
        assert_eq!(definition_body(&root, "dup"), None);
        assert_eq!(definition_body(&root, "missing"), None);
    }

    #[test]
    fn route_falls_back_without_whole_word_hits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let body: String = (0..50)
            .map(|i| format!("let w{i} = WidgetFactory::new();\n"))
            .collect();
        std::fs::write(root.join("a.rs"), body).unwrap();
        let stdout = rg_lines(&root, &["a.rs"], "Widget");
        assert_eq!(route(&root, "Widget", &stdout, ROUTE_NOTE), None);
    }
}
