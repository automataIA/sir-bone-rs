use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Global,
    Local,
}

#[derive(Clone, Debug)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: Scope,
    /// `always: true` in frontmatter — auto-recalled: its body is injected into
    /// the system prompt at startup instead of waiting for a `load_skill` call.
    pub always: bool,
    /// `paths:` glob list in frontmatter — path-scoped auto-recall: the body is
    /// injected at startup only when the working tree contains a file matching one
    /// of the globs (e.g. `["**/*.rs"]` for a Rust-only skill). Computed once at
    /// startup, so it never invalidates the cached system prompt mid-session.
    pub paths: Vec<String>,
}

/// True if the working tree under `cwd` contains at least one file matching any
/// of the globs. Existence check only — short-circuits on the first hit.
pub fn path_globs_match(cwd: &Path, globs: &[String]) -> bool {
    globs.iter().any(|g| {
        let pattern = cwd.join(g);
        glob::glob(&pattern.to_string_lossy())
            .ok()
            .and_then(|mut it| it.find_map(Result::ok))
            .is_some()
    })
}

/// Scan global (`~/.sirbone/skills/`, `~/.agents/skills/`) and local
/// (`.sirbone/skills/`, `.agents/skills/`) directories for every skill present,
/// ignoring any disable list. Sir Bone's native `.sirbone/skills` roots are the
/// canonical home; `.agents/skills` is read for cross-client compatibility.
/// Project-local skills override user-level skills with the same name. Used by
/// the TUI skill picker, which needs to show disabled skills too (so they can be
/// re-enabled).
pub fn scan_all_skills() -> Vec<SkillMeta> {
    let mut skills = Vec::new();

    // 1. user-level compatibility, then native (native wins name collisions)
    if let Some(home) = dirs::home_dir() {
        scan_dir(
            &home.join(".agents").join("skills"),
            Scope::Global,
            &mut skills,
        );
        scan_dir(
            &home.join(".sirbone").join("skills"),
            Scope::Global,
            &mut skills,
        );
    }

    // 2. project-level compatibility, then native (project and native win)
    scan_dir(Path::new(".agents/skills"), Scope::Local, &mut skills);
    scan_dir(Path::new(".sirbone/skills"), Scope::Local, &mut skills);

    skills
}

/// The skills actually offered to the model: only those the current project has
/// **opted in** to (config `skills.enabled` allowlist), minus any hidden by the
/// `SIRBONE_DISABLE` feature-audit env var. Allowlist semantics — a project with
/// no config enables nothing, so a fresh project starts with every skill off.
/// Applied once at startup (catalog built then), keeping the prompt cache stable.
pub fn scan_skills() -> Vec<SkillMeta> {
    let enabled = crate::config::skills_enabled();
    let mut skills = scan_all_skills();
    skills.retain(|s| {
        enabled.iter().any(|e| e == &s.name) && !crate::ablate::disabled_skill(&s.name)
    });
    skills
}

/// Parse a single `SKILL.md`/`skill.md` given its direct path (scope defaults to local).
pub fn scan_one(skill_md: &Path) -> Option<SkillMeta> {
    parse_frontmatter(skill_md, Scope::Local)
}

/// Load full body (everything after frontmatter `---`) on demand.
pub fn load_skill_body(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    // skip YAML frontmatter: first `---` ... second `---`
    let rest = text.strip_prefix("---")?;
    let body_start = rest.find("---").map(|i| i + 3)?;
    let body = rest[body_start..].trim();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

fn scan_dir(dir: &Path, scope: Scope, skills: &mut Vec<SkillMeta>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(skill_md) = find_skill_md(&entry.path()) else {
            continue;
        };
        if let Some(meta) = parse_frontmatter(&skill_md, scope.clone()) {
            // Later roots are higher precedence: native overrides compatibility,
            // and project-local roots override user-level roots.
            skills.retain(|s| s.name != meta.name);
            skills.push(meta);
        }
    }
}

fn find_skill_md(skill_dir: &Path) -> Option<PathBuf> {
    ["SKILL.md", "skill.md"]
        .iter()
        .map(|name| skill_dir.join(name))
        .find(|path| path.is_file())
}

fn parse_frontmatter(path: &Path, scope: Scope) -> Option<SkillMeta> {
    let text = std::fs::read_to_string(path).ok()?;
    let rest = text.strip_prefix("---")?;
    let end = rest.find("---")?;
    let front = &rest[..end];

    let mut name = None;
    let mut description = None;
    let mut always = false;
    let mut paths = Vec::new();

    let lines: Vec<&str> = front.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if let Some(v) = trimmed.strip_prefix("name:") {
            name = Some(inline_value(v.trim()));
        } else if let Some(v) = trimmed.strip_prefix("description:") {
            let v = v.trim();
            if let Some(folded) = block_scalar_style(v) {
                // `description: >-` / `description: |-` — multi-line block scalar.
                // Consume the indented continuation lines so the value isn't read
                // as the literal marker (`>-`).
                let (body, consumed) = collect_block_scalar(&lines[i + 1..], folded);
                description = Some(body);
                i += consumed;
            } else {
                description = Some(inline_value(v));
            }
        } else if let Some(v) = trimmed.strip_prefix("always:") {
            always = v.trim().eq_ignore_ascii_case("true");
        } else if let Some(v) = trimmed.strip_prefix("paths:") {
            // Accept a flow-sequence (`paths: ["*.rs", "*.toml"]`) or a bare
            // comma list (`paths: *.rs, *.toml`).
            paths = v
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(|p| inline_value(p.trim()))
                .filter(|p| !p.is_empty())
                .collect();
        }
        i += 1;
    }

    Some(SkillMeta {
        name: name.or_else(|| path.parent()?.file_name()?.to_str().map(String::from))?,
        description: description.unwrap_or_default(),
        path: path.to_path_buf(),
        scope,
        always,
        paths,
    })
}

/// Strip surrounding single/double quotes from an inline frontmatter value.
fn inline_value(v: &str) -> String {
    v.trim_matches('"').trim_matches('\'').to_string()
}

/// Detect a YAML block-scalar marker: `>` (folded) or `|` (literal). Chomping
/// and indent modifiers (`-`, `+`, digits) are accepted but only the style is
/// returned. `None` means a plain inline value.
fn block_scalar_style(v: &str) -> Option<bool> {
    match v.trim_start().chars().next()? {
        '>' => Some(true),
        '|' => Some(false),
        _ => None,
    }
}

/// Collect a folded (`>`) or literal (`|`) block scalar body from the lines
/// following the marker. The block ends at the first line indented less than
/// the body (a sibling key) or at end of input. Returns the joined value and
/// how many lines were consumed. Default chomping: trailing blank lines drop.
fn collect_block_scalar(lines: &[&str], folded: bool) -> (String, usize) {
    let mut block_indent: Option<usize> = None;
    let mut body: Vec<&str> = Vec::new();
    let mut consumed = 0;
    for line in lines {
        if line.trim().is_empty() {
            consumed += 1;
            body.push("");
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let bi = match block_indent {
            None => {
                block_indent = Some(indent);
                indent
            }
            Some(bi) if indent < bi => break,
            Some(bi) => bi,
        };
        body.push(&line[bi..]);
        consumed += 1;
    }
    while body.last().is_some_and(|l| l.is_empty()) {
        body.pop();
    }
    let value = if folded {
        let mut out = String::new();
        for (idx, l) in body.iter().enumerate() {
            if l.is_empty() {
                out.push('\n');
            } else {
                if idx != 0 && !body[idx - 1].is_empty() {
                    out.push(' ');
                }
                out.push_str(l);
            }
        }
        out
    } else {
        body.join("\n")
    };
    (value, consumed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_and_load_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: test-skill
description: A test skill
---

Do the thing.
"#,
        )
        .unwrap();

        let mut skills = Vec::new();
        scan_dir(tmp.path(), Scope::Local, &mut skills);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "test-skill");
        assert_eq!(skills[0].description, "A test skill");

        let body = load_skill_body(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(body, "Do the thing.");
    }

    #[test]
    fn local_overrides_global() {
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path().join("global").join("my-skill");
        let l = tmp.path().join("local").join("my-skill");
        fs::create_dir_all(&g).unwrap();
        fs::create_dir_all(&l).unwrap();
        fs::write(
            g.join("SKILL.md"),
            "---\nname: my-skill\ndescription: global\n---\nbody",
        )
        .unwrap();
        fs::write(
            l.join("SKILL.md"),
            "---\nname: my-skill\ndescription: local\n---\nbody",
        )
        .unwrap();

        let mut skills = Vec::new();
        scan_dir(
            tmp.path().join("global").as_path(),
            Scope::Global,
            &mut skills,
        );
        scan_dir(
            tmp.path().join("local").as_path(),
            Scope::Local,
            &mut skills,
        );
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "local");
        assert_eq!(skills[0].scope, Scope::Local);
    }

    #[test]
    fn later_roots_override_earlier_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let compat = tmp.path().join("agents").join("my-skill");
        let native = tmp.path().join("sirbone").join("my-skill");
        fs::create_dir_all(&compat).unwrap();
        fs::create_dir_all(&native).unwrap();
        fs::write(
            compat.join("SKILL.md"),
            "---\nname: my-skill\ndescription: compat\n---\nbody",
        )
        .unwrap();
        fs::write(
            native.join("SKILL.md"),
            "---\nname: my-skill\ndescription: native\n---\nbody",
        )
        .unwrap();

        let mut skills = Vec::new();
        scan_dir(compat.parent().unwrap(), Scope::Global, &mut skills);
        scan_dir(native.parent().unwrap(), Scope::Global, &mut skills);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "native");
    }

    #[test]
    fn lowercase_skill_md_is_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("lowercase-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("skill.md"),
            "---\nname: lowercase-skill\ndescription: d\n---\nbody",
        )
        .unwrap();

        let mut skills = Vec::new();
        scan_dir(tmp.path(), Scope::Local, &mut skills);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "lowercase-skill");
        assert_eq!(skills[0].path.file_name().unwrap(), "skill.md");
    }

    #[test]
    fn always_flag_parsed_from_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let on = tmp.path().join("auto-skill");
        let off = tmp.path().join("plain-skill");
        fs::create_dir_all(&on).unwrap();
        fs::create_dir_all(&off).unwrap();
        fs::write(
            on.join("SKILL.md"),
            "---\nname: auto-skill\ndescription: d\nalways: true\n---\nbody",
        )
        .unwrap();
        fs::write(
            off.join("SKILL.md"),
            "---\nname: plain-skill\ndescription: d\n---\nbody",
        )
        .unwrap();

        let mut skills = Vec::new();
        scan_dir(tmp.path(), Scope::Global, &mut skills);
        assert!(
            skills
                .iter()
                .find(|s| s.name == "auto-skill")
                .unwrap()
                .always
        );
        assert!(
            !skills
                .iter()
                .find(|s| s.name == "plain-skill")
                .unwrap()
                .always
        );
    }

    #[test]
    fn paths_parsed_and_globs_match_working_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("rust-rules");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: rust-rules\ndescription: d\npaths: [\"**/*.rs\", \"*.toml\"]\n---\nbody",
        )
        .unwrap();

        let meta = scan_one(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(
            meta.paths,
            vec!["**/*.rs".to_string(), "*.toml".to_string()]
        );

        // A tree with a .rs file matches; an empty tree does not.
        let tree = tmp.path().join("tree");
        fs::create_dir_all(tree.join("src")).unwrap();
        assert!(!path_globs_match(&tree, &meta.paths));
        fs::write(tree.join("src").join("main.rs"), "fn main() {}").unwrap();
        assert!(path_globs_match(&tree, &meta.paths));
    }

    #[test]
    fn folded_block_scalar_description() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("folded");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: folded\ndescription: >-\n  one line.\n  second line.\nlicense: MIT\n---\nbody",
        )
        .unwrap();
        let meta = scan_one(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(meta.name, "folded");
        assert_eq!(meta.description, "one line. second line.");
    }

    #[test]
    fn literal_block_scalar_description() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("lit");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: lit\ndescription: |-\n  keep\n  newlines\n---\nbody",
        )
        .unwrap();
        let meta = scan_one(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(meta.description, "keep\nnewlines");
    }

    #[test]
    fn block_scalar_ends_at_sibling_key() {
        // Folded: blank line → newline; block must end at the less-indented
        // `always:` sibling (not swallow it into the description).
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("blk");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: blk\ndescription: >-\n  para one.\n\n  para two.\nalways: true\n---\nbody",
        )
        .unwrap();
        let meta = scan_one(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(meta.description, "para one.\npara two.");
        assert!(meta.always);
    }

    #[test]
    fn fallback_name_from_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("inferred-name");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: no name field\n---\nbody",
        )
        .unwrap();

        let mut skills = Vec::new();
        scan_dir(tmp.path(), Scope::Global, &mut skills);
        assert_eq!(skills[0].name, "inferred-name");
    }
}
