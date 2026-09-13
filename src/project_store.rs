//! Per-project agent state under `~/.sirbone/projects/<slug>/`.
//!
//! Nothing is written into the repo (design option B): the project directory is
//! mapped to a stable, readable slug derived from its absolute path, e.g.
//! `/home/dio/pi` -> `-home-dio-pi`. State captured here is agent output (chosen
//! palette, model, prompt history), kept separate from user-authored config.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Captured per-project state. Missing fields default; the file is created on
/// first save.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub project_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palette: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Persisted TUI Settings toggles (None = never set → fall back to defaults).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_bar: Option<bool>,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
}

/// One executed prompt, appended to `history.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub timestamp: u64,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Readable slug of an absolute path: every non-alphanumeric char becomes `-`.
/// Mirrors the `~/.claude/projects/<slug>/` convention.
pub fn project_slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Test-only redirect for the projects root, so tests never write into the real
/// `~/.sirbone`. Set once by integration harnesses via [`set_projects_root_override`];
/// lib unit tests get an automatic temp redirect via the `cfg!(test)` branch below.
static PROJECTS_ROOT_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Point the projects root at `dir` for the rest of the process (first call wins).
/// Integration tests call this in setup; production never does.
pub fn set_projects_root_override(dir: PathBuf) {
    let _ = PROJECTS_ROOT_OVERRIDE.set(dir);
}

/// `~/.sirbone/projects/` — the per-project state root every slug hangs off.
pub fn projects_root() -> PathBuf {
    if let Some(over) = PROJECTS_ROOT_OVERRIDE.get() {
        return over.clone();
    }
    // Lib unit tests compile with cfg(test): keep their per-project caches
    // (structure.bin, graph.bin, …) out of the real `~/.sirbone`.
    if cfg!(test) {
        return std::env::temp_dir().join("sirbone-unit-test-projects");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sirbone")
        .join("projects")
}

/// `~/.sirbone/projects/<slug>/` for the given project root.
pub fn project_dir(project: &Path) -> PathBuf {
    projects_root().join(project_slug(project))
}

/// Result of [`link_config_into_repo`].
pub enum LinkOutcome {
    /// Symlink newly created at this path.
    Created(PathBuf),
    /// A `.sirbone-symlink` entry was already present (left untouched).
    Existed,
    /// Symlinks aren't supported on this platform (non-unix).
    Unsupported,
}

/// Create `<project>/.sirbone-symlink` → `~/.sirbone/projects/<slug>/`, making
/// the target dir first if needed. The home-side dir holds this project's state
/// and config, so backing up just `~/.sirbone` captures everything; the link
/// exposes it from inside the repo. The `-symlink` suffix flags it as a link,
/// not the real directory. Never overwrites an existing `.sirbone-symlink`.
pub fn link_config_into_repo(project: &Path) -> Result<LinkOutcome> {
    link_dir_into(&project_dir(project), project)
}

/// Core of [`link_config_into_repo`] with the link target passed explicitly, so
/// it's testable without touching `HOME`.
fn link_dir_into(target: &Path, project: &Path) -> Result<LinkOutcome> {
    std::fs::create_dir_all(target)
        .with_context(|| format!("creating project dir {}", target.display()))?;
    let link = project.join(".sirbone-symlink");
    if link.symlink_metadata().is_ok() {
        return Ok(LinkOutcome::Existed);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, &link)
            .with_context(|| format!("creating symlink {}", link.display()))?;
        Ok(LinkOutcome::Created(link))
    }
    #[cfg(not(unix))]
    {
        Ok(LinkOutcome::Unsupported)
    }
}

/// Load `meta.json` from `dir`, or a default seeded with `project` if absent.
fn load_meta_in(dir: &Path, project: &Path) -> ProjectMeta {
    std::fs::read_to_string(dir.join("meta.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| ProjectMeta {
            project_path: project.to_string_lossy().into_owned(),
            created: now_secs(),
            ..Default::default()
        })
}

/// Load `meta.json`, or a default seeded with the project path if absent.
pub fn load_meta(project: &Path) -> ProjectMeta {
    load_meta_in(&project_dir(project), project)
}

fn save_meta_in(dir: &Path, meta: &mut ProjectMeta) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create project dir {}", dir.display()))?;
    if meta.created == 0 {
        meta.created = now_secs();
    }
    meta.updated = now_secs();
    let json = serde_json::to_string_pretty(meta).context("cannot serialize meta")?;
    std::fs::write(dir.join("meta.json"), json).context("cannot write meta.json")?;
    Ok(())
}

/// Write `meta.json`, stamping `updated` (and `created` on first write).
pub fn save_meta(project: &Path, meta: &mut ProjectMeta) -> Result<()> {
    save_meta_in(&project_dir(project), meta)
}

fn append_history_in(dir: &Path, prompt: &str, model: Option<&str>) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create project dir {}", dir.display()))?;
    let entry = HistoryEntry {
        timestamp: now_secs(),
        prompt: prompt.to_string(),
        model: model.map(String::from),
    };
    let line = serde_json::to_string(&entry).context("cannot serialize history entry")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("history.jsonl"))
        .context("cannot open history.jsonl")?;
    writeln!(file, "{line}").context("cannot append history")?;
    Ok(())
}

/// Append one executed prompt to `history.jsonl`.
pub fn append_history(project: &Path, prompt: &str, model: Option<&str>) -> Result<()> {
    append_history_in(&project_dir(project), prompt, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_claude_convention() {
        assert_eq!(project_slug(Path::new("/home/dio/pi")), "-home-dio-pi");
        // dots and other separators also become dashes
        assert_eq!(project_slug(Path::new("/a/.b_c")), "-a--b-c");
    }

    #[cfg(unix)]
    #[test]
    fn link_dir_into_creates_then_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let target = home.path().join("projects/slug");

        // First call creates the symlink pointing at the (now-created) target.
        match link_dir_into(&target, repo.path()).unwrap() {
            LinkOutcome::Created(link) => {
                assert_eq!(link, repo.path().join(".sirbone-symlink"));
                assert!(target.is_dir(), "target dir created");
                assert_eq!(std::fs::read_link(&link).unwrap(), target);
            }
            _ => panic!("expected Created"),
        }
        // Second call leaves the existing link untouched.
        assert!(matches!(
            link_dir_into(&target, repo.path()).unwrap(),
            LinkOutcome::Existed
        ));
    }

    #[test]
    fn meta_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let project = Path::new("/some/project");

        let mut meta = load_meta_in(tmp.path(), project);
        assert_eq!(meta.project_path, "/some/project");
        meta.palette = Some("dracula".into());
        meta.model = Some("claude-opus-4-7".into());
        save_meta_in(tmp.path(), &mut meta).unwrap();
        assert!(meta.created > 0 && meta.updated > 0);

        let reloaded = load_meta_in(tmp.path(), project);
        assert_eq!(reloaded.palette.as_deref(), Some("dracula"));
        assert_eq!(reloaded.model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn history_appends() {
        let tmp = tempfile::tempdir().unwrap();

        append_history_in(tmp.path(), "first prompt", Some("m1")).unwrap();
        append_history_in(tmp.path(), "second prompt", None).unwrap();

        let body = std::fs::read_to_string(tmp.path().join("history.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let e0: HistoryEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(e0.prompt, "first prompt");
        assert_eq!(e0.model.as_deref(), Some("m1"));
    }
}
