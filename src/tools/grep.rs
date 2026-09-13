use std::io::ErrorKind;

use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

use super::{truncate::truncate_default, TypedTool};

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

        let limited: String = stdout
            .lines()
            .take(input.max_results as usize)
            .collect::<Vec<_>>()
            .join("\n");

        Ok(truncate_default(limited))
    }
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
        })
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("real.txt"));
        assert!(!stdout.contains("target/junk.txt"));
    }
}
