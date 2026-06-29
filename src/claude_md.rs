use std::path::{Path, PathBuf};

/// Walk from `cwd` up to root collecting `filename` files (root→cwd order),
/// plus `~/.config/sirbone/<filename>` as global override.
pub async fn load_instructions(cwd: &Path, filename: &str) -> String {
    let mut paths: Vec<PathBuf> = Vec::new();

    let mut ancestors: Vec<&Path> = cwd.ancestors().collect();
    ancestors.reverse();
    for dir in ancestors {
        let p = dir.join(filename);
        if tokio::fs::metadata(&p).await.is_ok() {
            paths.push(p);
        }
    }

    if let Some(home) = dirs::home_dir() {
        let global = home.join(".config").join("sirbone").join(filename);
        if tokio::fs::metadata(&global).await.is_ok() {
            paths.push(global);
        }
    }

    let mut parts: Vec<String> = Vec::new();
    for p in &paths {
        if let Ok(content) = tokio::fs::read_to_string(p).await {
            if !content.trim().is_empty() {
                parts.push(content);
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn collects_files_root_to_cwd_skipping_empty() {
        // Unique name so the `~/.config/sirbone/<name>` global path can't exist.
        let name = "SIRBONE_TEST_INSTR.md";
        let root = tempfile::tempdir().unwrap();
        let sub = root.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.path().join(name), "ROOT").unwrap();
        std::fs::write(sub.join(name), "  ").unwrap(); // blank → skipped
        std::fs::write(root.path().join("a").join(name), "MID").unwrap();

        let out = load_instructions(&sub, name).await;
        // Root before mid (ancestors reversed), blank leaf omitted.
        assert_eq!(out, "ROOT\n\nMID");
    }

    #[tokio::test]
    async fn missing_file_yields_empty_string() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_instructions(dir.path(), "NOPE.md").await, "");
    }
}
