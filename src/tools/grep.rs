use std::io::ErrorKind;

use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

use super::{truncate::truncate_default, TypedTool};

#[derive(Deserialize, JsonSchema)]
pub struct GrepInput {
    /// Regex pattern to search for
    pub pattern: String,
    /// File or directory to search
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

        if stdout.is_empty() && !stderr.is_empty() {
            return Err(anyhow::anyhow!("{}", stderr.trim()));
        }
        if stdout.is_empty() {
            return Ok("no matches".into());
        }

        let limited: String = stdout
            .lines()
            .take(input.max_results as usize)
            .collect::<Vec<_>>()
            .join("\n");

        Ok(truncate_default(limited))
    }
}

async fn run_rg(input: &GrepInput) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("rg");
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

async fn run_grep(input: &GrepInput) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("grep");
    cmd.arg("-rn");
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
