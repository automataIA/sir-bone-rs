//! Layered JSON config: global `~/.sirbone/config.json` overlaid with the
//! per-project `~/.sirbone/projects/<slug>/config.json` (same shape as global).
//!
//! Section semantics: object sections (`permissions`, `oracle`, `cli`) are
//! **deep-merged** — the global section provides defaults and the project file
//! overrides individual keys, so a project that sets only one key no longer
//! silently drops the rest. For `permissions`, the `allow`/`soft_deny` arrays
//! are **unioned** (a project never silently loses a curated global deny rule);
//! other arrays (e.g. `environment`) are replaced by the project's value.
//! Allowlist sections (`skills.enabled`, `mcp.enabled`) are read from the
//! project file only — a project opts in to nothing by default.
//!
//! Both files are optional; a missing or malformed file contributes nothing and
//! is never an error.

use std::path::PathBuf;

use serde_json::{Map, Value};

/// Global config path (`~/.sirbone/config.json`), if HOME is set.
pub fn global_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::Path::new(&home).join(".sirbone/config.json"))
}

/// Global env path (`~/.sirbone/.env`), if HOME is set. Loaded at startup with
/// lower precedence than the process environment and the project's `.env`, so a
/// single global file configures credentials once for every directory.
pub fn global_env_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::Path::new(&home).join(".sirbone/.env"))
}

/// The bundled `.env` template, embedded so the installed binary can seed it
/// without the repo checkout present (see `ensure_global_env`).
pub const ENV_TEMPLATE: &str = include_str!("../.env.example");

/// Outcome of seeding the global env file, for the caller to render.
pub struct LoginInfo {
    pub path: PathBuf,
    pub created: bool,
}

/// Ensure `~/.sirbone/.env` exists, seeded from [`ENV_TEMPLATE`] (perms `0600` on
/// unix). Never overwrites an existing file — that would clobber the user's key.
pub fn ensure_global_env() -> std::io::Result<LoginInfo> {
    let path = global_env_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))?;
    ensure_env_at(path)
}

/// Path-explicit core of [`ensure_global_env`] (no `HOME` lookup, so it's testable).
fn ensure_env_at(path: PathBuf) -> std::io::Result<LoginInfo> {
    if path.exists() {
        return Ok(LoginInfo {
            path,
            created: false,
        });
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, ENV_TEMPLATE)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(LoginInfo {
        path,
        created: true,
    })
}

/// Per-project config path (`~/.sirbone/projects/<slug>/config.json`) for the
/// working directory. State lives under the home sirbone dir, not in the repo.
pub fn project_path() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    Some(crate::project_store::project_dir(&cwd).join("config.json"))
}

fn read(path: Option<PathBuf>) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path?).ok()?).ok()
}

/// A top-level object section, deep-merging global + project (see module docs).
pub fn section(key: &str) -> Option<Value> {
    merge_section(
        key,
        read(global_path())
            .as_ref()
            .and_then(|v| v.get(key).cloned()),
        read(project_path())
            .as_ref()
            .and_then(|v| v.get(key).cloned()),
    )
}

/// Token spend cap (`spend_cap` section, Feature C). `Some(max_tokens)` only when
/// `enabled` is true and `max_tokens > 0`; otherwise `None` (disabled — the
/// default, since not every provider bills per token). `window` is currently
/// always per-session (daily persistence deferred).
pub fn spend_cap() -> Option<u64> {
    let s = section("spend_cap")?;
    if !s.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    s.get("max_tokens")
        .and_then(Value::as_u64)
        .filter(|&m| m > 0)
}

/// REPL color theme name (`cli.theme` string: `dark`/`light`/`none`), with the
/// project file's `cli` section replacing the global one. `SIRBONE_THEME` and
/// `NO_COLOR` take precedence over this (see `render::ColorTheme::load`).
pub fn cli_theme() -> Option<String> {
    section("cli")?.get("theme")?.as_str().map(str::to_string)
}

/// Skill names enabled for the **current project** (`skills.enabled` array in
/// `~/.sirbone/projects/<slug>/config.json`). Allowlist semantics: a project
/// with no config enables nothing — every skill is off until opted in here, so
/// global enables are intentionally ignored. Read at startup by `scan_skills`.
pub fn skills_enabled() -> Vec<String> {
    string_array(read(project_path()).as_ref(), "skills", "enabled")
}

/// Persist the project's `skills.enabled` allowlist. See [`set_project_array`].
pub fn set_skills_enabled(names: &[String]) -> std::io::Result<()> {
    set_project_array("skills", "enabled", names)
}

/// MCP server names enabled for the **current project** (`mcp.enabled` array).
/// Same allowlist semantics as [`skills_enabled`]: nothing runs until opted in.
pub fn mcp_enabled() -> Vec<String> {
    string_array(read(project_path()).as_ref(), "mcp", "enabled")
}

/// Persist the project's `mcp.enabled` allowlist. See [`set_project_array`].
pub fn set_mcp_enabled(names: &[String]) -> std::io::Result<()> {
    set_project_array("mcp", "enabled", names)
}

/// Drop keys made obsolete by the catalog/allowlist migration from the global
/// `config.json`: `mcpServers` (definitions now live in `~/.sirbone/mcp.json`)
/// and the dead `skills.disabled` (superseded by per-project `skills.enabled`).
/// Only writes when something was actually removed. Call **after** the MCP
/// catalog migration has safely copied the servers out.
pub fn strip_obsolete_global_keys() {
    let Some(path) = global_path() else { return };
    let Some(mut root) = read(Some(path.clone())) else {
        return;
    };
    let Some(obj) = root.as_object_mut() else {
        return;
    };
    let mut changed = obj.remove("mcpServers").is_some();
    if let Some(Value::Object(skills)) = obj.get_mut("skills") {
        changed |= skills.remove("disabled").is_some();
        if skills.is_empty() {
            obj.remove("skills"); // drop an emptied section to keep the file tidy
        }
    }
    if changed {
        if let Ok(text) = serde_json::to_string_pretty(&root) {
            let _ = std::fs::write(&path, text);
        }
    }
}

/// Pull `root[section][key]` as a string array, else empty. (pure)
fn string_array(root: Option<&Value>, section: &str, key: &str) -> Vec<String> {
    root.and_then(|v| v.get(section))
        .and_then(|v| v.get(key))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Write `<section>.<key> = names` into the **per-project** config file,
/// preserving every other key. Creates the file/dir if absent. Errors propagate
/// so a write failure can be surfaced. Project-scoped because enablement is a
/// per-project choice (a project starts with everything off).
fn set_project_array(section: &str, key: &str, names: &[String]) -> std::io::Result<()> {
    let path = project_path().ok_or_else(|| std::io::Error::other("no current dir"))?;
    let mut root = read(Some(path.clone())).unwrap_or_else(|| Value::Object(Map::new()));
    let obj = root
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("config root is not an object"))?;
    let sect = obj
        .entry(section)
        .or_insert_with(|| Value::Object(Map::new()));
    match sect.as_object_mut() {
        Some(sm) => {
            sm.insert(key.into(), Value::from(names));
        }
        None => {
            return Err(std::io::Error::other(format!(
                "config `{section}` is not an object"
            )));
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&root)?)
}

/// Merge a global and project section value. Pure.
fn merge_section(_key: &str, global: Option<Value>, project: Option<Value>) -> Option<Value> {
    match (global, project) {
        (None, None) => None,
        (None, Some(p)) | (Some(p), None) => Some(p),
        (Some(g), Some(p)) => Some(merge_objects(g, p)),
    }
}

/// Deep-merge two object values: `overlay` (project) overrides `base` (global)
/// key-by-key; `permissions.allow`/`soft_deny` arrays are unioned so a project
/// can't drop a global rule. Non-objects: overlay wins.
fn merge_objects(mut base: Value, overlay: Value) -> Value {
    let Some(overlay_map) = overlay.as_object().cloned() else {
        return overlay;
    };
    let Some(base_map) = base.as_object_mut() else {
        return Value::Object(overlay_map);
    };
    for (k, ov) in overlay_map {
        let union_arrays = matches!(k.as_str(), "allow" | "soft_deny");
        match (base_map.remove(&k), &ov) {
            (Some(Value::Array(mut g)), Value::Array(p)) if union_arrays => {
                for v in p {
                    if !g.contains(v) {
                        g.push(v.clone());
                    }
                }
                base_map.insert(k, Value::Array(g));
            }
            _ => {
                base_map.insert(k, ov);
            }
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ensure_env_at_seeds_then_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".sirbone/.env");

        // First call seeds the file from the template.
        let first = ensure_env_at(path.clone()).unwrap();
        assert!(first.created);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ENV_TEMPLATE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "global env must be private (0600)");
        }

        // Second call must leave the user's edits untouched (no clobber).
        std::fs::write(&path, "ANTHROPIC_AUTH_TOKEN=sk-secret\n").unwrap();
        let second = ensure_env_at(path.clone()).unwrap();
        assert!(!second.created);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "ANTHROPIC_AUTH_TOKEN=sk-secret\n"
        );
    }

    #[test]
    fn section_merges_global_with_project_overrides() {
        let global = json!({ "test_command": "cargo test" });
        let project = json!({ "test_command": "pytest" });
        // Project key overrides the global one.
        assert_eq!(
            merge_section("oracle", Some(global.clone()), Some(project)).unwrap()["test_command"],
            "pytest"
        );
        // Falls back to global when the project omits the section.
        assert_eq!(
            merge_section("oracle", Some(global), None).unwrap()["test_command"],
            "cargo test"
        );
        // Neither defines it → None.
        assert!(merge_section("oracle", None, None).is_none());
        // Deep merge: a project key doesn't drop an unrelated global key.
        let merged = merge_section(
            "oracle",
            Some(json!({ "test_command": "cargo test" })),
            Some(json!({ "max_attempts": 5 })),
        )
        .unwrap();
        assert_eq!(merged["test_command"], "cargo test");
        assert_eq!(merged["max_attempts"], 5);
    }

    #[test]
    fn permissions_allow_soft_deny_are_unioned() {
        // A project must not silently drop a curated global allow/soft_deny rule.
        let global = json!({ "allow": ["Bash(ls*)"], "soft_deny": ["Bash(rm -rf*)"], "environment": ["g.env"] });
        let project = json!({ "allow": ["Bash(cat*)"], "soft_deny": ["Bash(dd*)"], "environment": ["p.env"] });
        let m = merge_section("permissions", Some(global), Some(project)).unwrap();
        let allow = m["allow"].as_array().unwrap();
        assert!(allow.contains(&json!("Bash(ls*)")));
        assert!(allow.contains(&json!("Bash(cat*)")));
        let deny = m["soft_deny"].as_array().unwrap();
        assert!(deny.contains(&json!("Bash(rm -rf*)")));
        assert!(deny.contains(&json!("Bash(dd*)")));
        // environment is NOT a union key — the project's value replaces global.
        assert_eq!(m["environment"].as_array().unwrap(), &vec![json!("p.env")]);
    }

    #[test]
    fn string_array_reads_section_key_else_empty() {
        let v = json!({ "skills": { "enabled": ["tdd", "diagnose"] } });
        assert_eq!(
            string_array(Some(&v), "skills", "enabled"),
            vec!["tdd", "diagnose"]
        );
        // Missing key, wrong type, missing section, and absent root all yield empty.
        assert!(string_array(Some(&json!({ "skills": {} })), "skills", "enabled").is_empty());
        assert!(string_array(
            Some(&json!({ "skills": { "enabled": "tdd" } })),
            "skills",
            "enabled"
        )
        .is_empty());
        assert!(string_array(Some(&json!({})), "skills", "enabled").is_empty());
        assert!(string_array(None, "skills", "enabled").is_empty());
    }
}
