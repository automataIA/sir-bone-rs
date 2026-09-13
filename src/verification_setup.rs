//! Offline, deterministic verification discovery and project configuration.

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    Authoritative,
    PostEdit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub kind: CandidateKind,
    pub command: String,
    pub directory: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globs: Vec<String>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discovery {
    pub project: String,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub authoritative: String,
    pub post_edit: BTreeMap<String, String>,
    pub max_attempts: u64,
    /// Add the opt-in deterministic confirmation policy for risky operations.
    pub high_risk: bool,
}

trait Detector {
    fn detect(&self, root: &Path, out: &mut Vec<Candidate>);
}

struct RustDetector;
struct PythonDetector;
struct NodeDetector;

pub fn discover(root: &Path) -> Discovery {
    let mut candidates = Vec::new();
    let registry: [&dyn Detector; 3] = [&RustDetector, &PythonDetector, &NodeDetector];
    for detector in registry {
        detector.detect(root, &mut candidates);
    }
    Discovery {
        project: root.display().to_string(),
        candidates,
    }
}

impl Detector for RustDetector {
    fn detect(&self, root: &Path, out: &mut Vec<Candidate>) {
        let Ok(manifest) = std::fs::read_to_string(root.join("Cargo.toml")) else {
            return;
        };
        let source = if manifest.lines().any(|line| line.trim() == "[workspace]") {
            "Cargo.toml:[workspace]"
        } else if manifest.lines().any(|line| {
            matches!(
                line.trim(),
                "[lib]" | "[[bin]]" | "[[example]]" | "[[test]]" | "[[bench]]"
            )
        }) {
            "Cargo.toml:targets"
        } else {
            "Cargo.toml"
        };
        out.push(candidate(
            "rust-test",
            CandidateKind::Authoritative,
            "cargo test -q",
            &[],
            source,
        ));
        out.push(candidate(
            "rust-check",
            CandidateKind::PostEdit,
            "cargo check -q --message-format=short",
            &["*.rs"],
            source,
        ));
    }
}

impl Detector for PythonDetector {
    fn detect(&self, root: &Path, out: &mut Vec<Candidate>) {
        let path = root.join("pyproject.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let has_pytest = text
            .lines()
            .any(|line| line.trim().starts_with("[tool.pytest"));
        let has_ruff = text
            .lines()
            .any(|line| line.trim().starts_with("[tool.ruff"));
        if has_pytest {
            out.push(candidate(
                "python-pytest",
                CandidateKind::Authoritative,
                "python -m pytest -q",
                &[],
                "pyproject.toml:[tool.pytest]",
            ));
        }
        let runner = if root.join("uv.lock").is_file() {
            Some("uv run")
        } else if root.join("poetry.lock").is_file() {
            Some("poetry run")
        } else {
            None
        };
        if let Some(runner) = runner {
            let mut added_authoritative = has_pytest;
            let mut added_post_edit = has_ruff;
            for name in declared_toml_scripts(&text) {
                let kind = if matches!(name.as_str(), "test" | "check") {
                    CandidateKind::Authoritative
                } else {
                    CandidateKind::PostEdit
                };
                if (kind == CandidateKind::Authoritative && added_authoritative)
                    || (kind == CandidateKind::PostEdit && added_post_edit)
                {
                    continue;
                }
                let globs: &[&str] = if kind == CandidateKind::PostEdit {
                    &["*.py"]
                } else {
                    &[]
                };
                out.push(candidate(
                    &format!("python-{name}"),
                    kind,
                    &format!("{runner} {name}"),
                    globs,
                    &format!("pyproject.toml:declared-script.{name}"),
                ));
                match kind {
                    CandidateKind::Authoritative => added_authoritative = true,
                    CandidateKind::PostEdit => added_post_edit = true,
                }
            }
        }
        if has_ruff {
            out.push(candidate(
                "python-ruff",
                CandidateKind::PostEdit,
                "python -m ruff check .",
                &["*.py"],
                "pyproject.toml:[tool.ruff]",
            ));
        }
    }
}

/// Extract only explicitly declared, verification-shaped entry points. This is
/// deliberately a narrow TOML scanner: arbitrary values, prose and unknown
/// script names never become executable suggestions.
fn declared_toml_scripts(text: &str) -> Vec<String> {
    let mut section = "";
    let mut names = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line;
            continue;
        }
        if !matches!(section, "[project.scripts]" | "[tool.poetry.scripts]") {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches(['\'', '"']);
        if matches!(key, "test" | "check" | "typecheck" | "lint")
            && matches!(value.trim().chars().next(), Some('\'' | '"'))
            && !names.iter().any(|existing| existing == key)
        {
            names.push(key.to_string());
        }
    }
    names
}

impl Detector for NodeDetector {
    fn detect(&self, root: &Path, out: &mut Vec<Candidate>) {
        let path = root.join("package.json");
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(package) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let Some(scripts) = package.get("scripts").and_then(Value::as_object) else {
            return;
        };
        let manager = if root.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if root.join("yarn.lock").is_file() {
            "yarn"
        } else if root.join("bun.lockb").is_file() || root.join("bun.lock").is_file() {
            "bun"
        } else if root.join("package-lock.json").is_file() {
            "npm"
        } else {
            return;
        };
        for name in ["test", "check"] {
            if scripts.get(name).and_then(Value::as_str).is_some() {
                out.push(candidate(
                    &format!("node-{name}"),
                    CandidateKind::Authoritative,
                    &format!("{manager} run {name}"),
                    &[],
                    &format!("package.json:scripts.{name}"),
                ));
                break;
            }
        }
        for name in ["typecheck", "lint"] {
            if scripts.get(name).and_then(Value::as_str).is_some() {
                out.push(candidate(
                    &format!("node-{name}"),
                    CandidateKind::PostEdit,
                    &format!("{manager} run {name}"),
                    &["*.js", "*.jsx", "*.ts", "*.tsx"],
                    &format!("package.json:scripts.{name}"),
                ));
            }
        }
    }
}

fn candidate(
    id: &str,
    kind: CandidateKind,
    command: &str,
    globs: &[&str],
    source: &str,
) -> Candidate {
    Candidate {
        id: id.into(),
        kind,
        command: command.into(),
        directory: ".".into(),
        globs: globs.iter().map(|s| (*s).into()).collect(),
        source: source.into(),
    }
}

pub fn config_patch(selection: &Selection) -> Value {
    let mut patch = serde_json::json!({
        "oracle": {"test_command": selection.authoritative, "max_attempts": selection.max_attempts},
        "hooks": {"post_tool_use": selection.post_edit}
    });
    if selection.high_risk {
        patch["hooks"]["presets"] = serde_json::json!(["high_risk"]);
    }
    patch
}

pub fn save(root: &Path, selection: &Selection) -> std::io::Result<std::path::PathBuf> {
    crate::config::update_project_config(root, |config| {
        merge_patch(config, config_patch(selection))
    })
}

fn merge_patch(config: &mut Map<String, Value>, patch: Value) -> std::io::Result<()> {
    let patch = patch
        .as_object()
        .ok_or_else(|| std::io::Error::other("invalid verification patch"))?;
    for (section, value) in patch {
        let incoming = value
            .as_object()
            .ok_or_else(|| std::io::Error::other("invalid verification section"))?;
        let target = config
            .entry(section)
            .or_insert_with(|| Value::Object(Map::new()));
        let target = target
            .as_object_mut()
            .ok_or_else(|| std::io::Error::other(format!("config `{section}` is not an object")))?;
        for (key, value) in incoming {
            target.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_only_uses_declared_scripts_and_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"vitest","other":"unsafe"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        let found = discover(dir.path());
        assert_eq!(found.candidates.len(), 1);
        assert_eq!(found.candidates[0].command, "npm run test");
    }

    #[test]
    fn config_patch_has_no_advanced_hooks() {
        let patch = config_patch(&Selection {
            authoritative: "cargo test -q".into(),
            post_edit: BTreeMap::from([("*.rs".into(), "cargo check -q".into())]),
            max_attempts: 3,
            high_risk: false,
        });
        assert!(patch.pointer("/hooks/pre_tool_use").is_none());
        assert!(patch.pointer("/hooks/stop").is_none());
    }

    #[test]
    fn python_declared_scripts_require_a_supported_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project.scripts]\ntest = 'pkg:test'\ndeploy = 'pkg:deploy'\n",
        )
        .unwrap();
        assert!(discover(dir.path()).candidates.is_empty());
        std::fs::write(dir.path().join("uv.lock"), "").unwrap();
        let found = discover(dir.path());
        assert_eq!(found.candidates.len(), 1);
        assert_eq!(found.candidates[0].command, "uv run test");
    }

    #[test]
    fn merging_preserves_unrelated_and_advanced_keys() {
        let mut root = serde_json::json!({
            "theme": "dark",
            "oracle": {"old": true},
            "hooks": {"pre_tool_use": ["policy"], "stop": ["invariant"]}
        })
        .as_object()
        .unwrap()
        .clone();
        merge_patch(
            &mut root,
            config_patch(&Selection {
                authoritative: "cargo test -q".into(),
                post_edit: BTreeMap::from([("*.rs".into(), "cargo check -q".into())]),
                max_attempts: 3,
                high_risk: true,
            }),
        )
        .unwrap();
        assert_eq!(root["theme"], "dark");
        assert_eq!(root["oracle"]["old"], true);
        assert_eq!(root["hooks"]["pre_tool_use"][0], "policy");
        assert_eq!(root["hooks"]["stop"][0], "invariant");
        assert_eq!(root["hooks"]["presets"][0], "high_risk");
    }

    #[test]
    fn high_risk_is_opt_in_and_visible_in_exact_patch() {
        let selection = |high_risk| Selection {
            authoritative: "cargo test -q".into(),
            post_edit: BTreeMap::new(),
            max_attempts: 3,
            high_risk,
        };
        assert!(config_patch(&selection(false))
            .pointer("/hooks/presets")
            .is_none());
        assert_eq!(
            config_patch(&selection(true))["hooks"]["presets"],
            serde_json::json!(["high_risk"])
        );
    }
}
