//! User-authored system-prompt extensions under `~/.sirbone/system/*.md`.
//!
//! These are *trusted* (the user wrote them in their own home dir): their text
//! is appended to the hardcoded base prompt rather than replacing it, so core
//! instructions can be extended but not broken. Files are concatenated in
//! filename order; frontmatter, if present, is stripped like skills.

use std::path::{Path, PathBuf};

use crate::structure::{self, Lang};

/// Per-language debugging/tooling cheat-sheet, gated to the languages actually
/// present in `cwd`, so the base prompt stays lean. Steers the model toward
/// non-interactive, batch-mode debugging — the only kind that survives the
/// one-shot `bash` tool, which gives the child no stdin (interactive REPLs hit
/// EOF or time out). `None` when no recognised language is found.
pub fn debug_toolkit(cwd: &Path) -> Option<String> {
    let (mut rust, mut python, mut jsts, mut sql) = (false, false, false, false);
    for (_, lang) in structure::discover(cwd) {
        match lang {
            Lang::Rust => rust = true,
            Lang::Python => python = true,
            Lang::JsTs => jsts = true,
            Lang::Sql => sql = true,
        }
    }
    if !(rust || python || jsts || sql) {
        return None;
    }

    let mut s = String::from(
        "Debugging & tooling: commands run with no stdin, so never launch a blocking or \
         interactive process — a bare `pdb`/`gdb`/`lldb` REPL, a pager like `less`, a `--watch` \
         or foreground dev server — it will hang until killed. Debug by reading the \
         error/traceback, adding targeted logging or asserts and re-running a focused test, or \
         driving a debugger in batch mode. Prefer quiet/machine-readable output flags.",
    );
    if rust {
        s.push_str(
            "\n- Rust: `RUST_BACKTRACE=1 cargo test <name> -- --nocapture`; batch debugger \
             `rust-gdb -batch -ex run -ex bt --args <bin>`; lint `cargo clippy --quiet`.",
        );
    }
    if python {
        s.push_str(
            "\n- Python: `pytest -x -q --tb=short <path>`; scripted pdb \
             `python -m pdb -c 'b file.py:42' -c c -c 'pp var' -c q script.py`; lint \
             `ruff check --output-format=concise`.",
        );
    }
    if jsts {
        s.push_str(
            "\n- JS/TS: tests `npx vitest run` or `npx jest --silent`; \
             `node --stack-trace-limit=50 file.js`; lint `npx eslint --format compact`. \
             Never use `--watch`.",
        );
    }
    if sql {
        s.push_str(
            "\n- SQL: run scripts non-interactively (`psql -f q.sql` / \
             `sqlite3 db '.read q.sql'`); avoid the interactive shell.",
        );
    }
    Some(s)
}

fn system_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sirbone")
        .join("system")
}

/// Body of a `*.md` file, minus an optional leading `--- ... ---` frontmatter.
fn body_of(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            return rest[end + 3..].trim().to_string();
        }
    }
    text.trim().to_string()
}

/// Concatenate the bodies of every `*.md` in `dir`, filename-sorted, blank
/// entries skipped. `None` if the dir is absent or yields nothing.
fn appends_in(dir: &Path) -> Option<String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
        .collect();
    files.sort();

    let joined = files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|t| body_of(&t))
        .filter(|b| !b.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    (!joined.is_empty()).then_some(joined)
}

/// Trusted user system-prompt text from `~/.sirbone/system/`, if any.
pub fn user_appends() -> Option<String> {
    appends_in(&system_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_dir_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(appends_in(&tmp.path().join("nope")).is_none());
    }

    #[test]
    fn debug_toolkit_gated_on_present_languages() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty workspace -> no toolkit.
        assert!(debug_toolkit(tmp.path()).is_none());

        // A Python file -> Python line only, no Rust/JS lines.
        std::fs::write(tmp.path().join("app.py"), "x = 1\n").unwrap();
        let t = debug_toolkit(tmp.path()).unwrap();
        assert!(t.contains("Python:"));
        assert!(!t.contains("Rust:"));
        assert!(!t.contains("JS/TS:"));
    }

    #[test]
    fn concatenates_sorted_and_strips_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("b.md"), "second").unwrap();
        std::fs::write(tmp.path().join("a.md"), "---\nname: x\n---\nfirst").unwrap();
        std::fs::write(tmp.path().join("ignore.txt"), "nope").unwrap();

        assert_eq!(appends_in(tmp.path()).unwrap(), "first\n\nsecond");
    }
}
