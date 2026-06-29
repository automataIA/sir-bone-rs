use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::TypedTool;

#[derive(Deserialize, JsonSchema)]
pub struct GlobInput {
    /// Glob pattern, e.g. "crates/**/*.rs" or "src/**/{mod,lib}.rs"
    pub pattern: String,
    /// Maximum number of results (default: 200)
    #[serde(default = "default_max")]
    pub max_results: usize,
}

fn default_max() -> usize {
    200
}

pub struct GlobTool;

#[async_trait]
impl TypedTool for GlobTool {
    type Input = GlobInput;

    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files matching a glob pattern. Supports ** for recursive matching, \
         e.g. \"crates/**/*.rs\". Faster than find for path-pattern searches; \
         prefer this over shelling out to find via bash."
    }

    async fn run(&self, input: GlobInput) -> Result<String> {
        let pattern = input.pattern.clone();
        let max = input.max_results;
        let paths: Vec<String> = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            Ok(glob::glob(&pattern)
                .with_context(|| format!("invalid glob pattern: {pattern}"))?
                .take(max)
                .filter_map(|r| r.ok())
                .map(|p| p.display().to_string())
                .collect())
        })
        .await
        .context("glob task panicked")??;

        if paths.is_empty() {
            return Ok("no files matched".into());
        }
        Ok(paths.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn glob_rs_files() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("main.rs"), "").unwrap();
        std::fs::write(sub.join("lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();

        let pattern = format!("{}/**/*.rs", dir.path().display());
        let result = GlobTool
            .run(GlobInput {
                pattern,
                max_results: 200,
            })
            .await
            .unwrap();
        assert!(result.contains("main.rs"));
        assert!(result.contains("lib.rs"));
        assert!(!result.contains("README.md"));
    }

    #[tokio::test]
    async fn glob_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let pattern = format!("{}/**/*.xyz", dir.path().display());
        let result = GlobTool
            .run(GlobInput {
                pattern,
                max_results: 200,
            })
            .await
            .unwrap();
        assert_eq!(result, "no files matched");
    }
}
