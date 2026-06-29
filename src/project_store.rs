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

fn projects_root() -> PathBuf {
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

/// Per-project agent memory log (`~/.sirbone/projects/<slug>/HISTORIA.md`).
/// Model-authored, append-on-top changelog the agent reads on demand to recall
/// past decisions across sessions.
pub fn historia_path(project: &Path) -> PathBuf {
    project_dir(project).join("HISTORIA.md")
}

/// Title + format legend written above the entries. Shared by the seed and the
/// prepend path so a fresh log and a tool-created one carry the same header.
const HISTORIA_HEADER: &str = "# HISTORIA\n\n\
     Persistent memory of changes made in this project. Newest entry on top.\n\
     Each entry: `## DD/MM/YYYY - HH:MM \u{2014} <title>` then bullets (what changed, files).\n";

/// Seed `HISTORIA.md` with its title + format legend if it doesn't exist yet, so
/// the agent's read-on-demand never misses and the entry format is authoritative.
/// Only ever writes the scaffold header — entries stay model-authored. No-op if
/// the file is already present.
pub fn ensure_historia(project: &Path) -> Result<()> {
    let path = historia_path(project);
    if path.exists() {
        return Ok(());
    }
    let dir = project_dir(project);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create project dir {}", dir.display()))?;
    std::fs::write(&path, HISTORIA_HEADER)
        .with_context(|| format!("cannot seed {}", path.display()))?;
    Ok(())
}

/// Render one entry: a `## <stamp> \u{2014} <title>` heading followed by one bullet
/// per line. `stamp` is the local time, already formatted as `DD/MM/YYYY - HH:MM`.
fn render_historia_entry(stamp: &str, title: &str, bullets: &[String]) -> String {
    let head = format!("## {stamp} \u{2014} {}\n", title.trim());
    bullets
        .iter()
        .filter(|b| !b.trim().is_empty())
        .map(|b| format!("- {}\n", b.trim()))
        .fold(head, |mut acc, line| {
            acc.push_str(&line);
            acc
        })
}

/// Prepend an entry into `HISTORIA.md` inside `dir`, newest-first: it lands after
/// the header legend (everything before the first `## ` heading) and above any
/// existing entries. Seeds the header if the file is absent. `stamp` is injected
/// so the insertion logic is testable without a clock.
fn prepend_historia_in(dir: &Path, stamp: &str, title: &str, bullets: &[String]) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create project dir {}", dir.display()))?;
    let path = dir.join("HISTORIA.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let existing = if existing.trim().is_empty() {
        HISTORIA_HEADER.to_string()
    } else {
        existing
    };
    let entry = render_historia_entry(stamp, title, bullets);
    let out = match existing.find("\n## ") {
        // Insert above the first existing entry, keeping a blank line between them.
        Some(i) => {
            let (head, rest) = existing.split_at(i + 1);
            format!("{head}{entry}\n{rest}")
        }
        // No entries yet — drop it under the legend with a blank line.
        None => {
            let sep = if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            format!("{existing}{sep}{entry}")
        }
    };
    std::fs::write(&path, out).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Prepend a model-authored entry to this project's `HISTORIA.md`, stamped with the
/// current local time (`DD/MM/YYYY - HH:MM`). Backs the `historia` tool.
pub fn prepend_historia(project: &Path, title: &str, bullets: &[String]) -> Result<()> {
    let stamp = chrono::Local::now().format("%d/%m/%Y - %H:%M").to_string();
    prepend_historia_in(&project_dir(project), &stamp, title, bullets)
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

    #[test]
    fn historia_path_lives_in_slug_dir() {
        let p = historia_path(Path::new("/home/dio/pi"));
        assert!(p.ends_with("-home-dio-pi/HISTORIA.md"), "{}", p.display());
    }

    #[test]
    fn prepend_historia_seeds_then_stacks_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("HISTORIA.md");

        // First entry seeds the header and lands under the legend.
        prepend_historia_in(dir.path(), "01/01/2026 - 09:00", "first", &["did A".into()]).unwrap();
        let after_first = std::fs::read_to_string(&path).unwrap();
        assert!(after_first.starts_with("# HISTORIA"), "header seeded");
        assert!(after_first.contains("## 01/01/2026 - 09:00 \u{2014} first"));
        assert!(after_first.contains("- did A"));

        // Second entry goes ABOVE the first (newest-first), header still on top.
        prepend_historia_in(
            dir.path(),
            "02/01/2026 - 10:30",
            "second",
            &["did B".into()],
        )
        .unwrap();
        let after_second = std::fs::read_to_string(&path).unwrap();
        assert!(
            after_second.starts_with("# HISTORIA"),
            "header stays on top"
        );
        let i_second = after_second.find("\u{2014} second").unwrap();
        let i_first = after_second.find("\u{2014} first").unwrap();
        assert!(i_second < i_first, "newest entry precedes older one");
        // Empty bullets are dropped.
        prepend_historia_in(
            dir.path(),
            "03/01/2026 - 11:00",
            "third",
            &["".into(), " ".into()],
        )
        .unwrap();
        let after_third = std::fs::read_to_string(&path).unwrap();
        let block = &after_third[..after_third.find("\u{2014} second").unwrap()];
        assert!(
            !block.contains("\n- "),
            "no empty bullet lines in newest block"
        );
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
