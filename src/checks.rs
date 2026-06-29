//! Post-edit checks: auto-lint feedback injected into tool results.
//!
//! Config (`~/.sirbone/config.json`, key `post_edit_check`) maps a path glob to
//! a fast check command, e.g. `{"*.rs": "cargo check -q --message-format=short",
//! "*.py": "ruff check -q"}`. After a batch of mutating tool calls succeeds,
//! every command whose glob matches an edited path runs once (deduplicated);
//! failures are appended to the last tool result so the model reads them inline
//! and fixes the breakage in the same turn — no "remember to run the linter"
//! round-trip. Keep commands quiet/short-format: their output lands in context.
//!
//! Advisory by design: a failed check never reverts the edit and never blocks
//! the loop. No config = no checks (current behavior).
//!
//! [`PostEditChecks`] is the `post_tool_use` event of the more general
//! [`Hooks`] (config key `hooks`), which also exposes a deterministic
//! `pre_tool_use` gate (exit-code allow/deny, short-circuiting the LLM command
//! classifier) and a `stop` hook (force another loop iteration). Legacy
//! top-level `post_edit_check` config still loads, as `hooks.post_tool_use`.

use std::process::Stdio;

use tokio::io::AsyncWriteExt;

use crate::permissions::glob_matches;

const TIMEOUT_SECS: u64 = 30;
const MAX_OUTPUT: usize = 4000;

#[derive(Debug, Clone, Default)]
pub struct PostEditChecks {
    /// `(path glob, command)` in config order.
    rules: Vec<(String, String)>,
}

impl PostEditChecks {
    /// Load from `~/.sirbone/config.json` (key `post_edit_check`). Missing
    /// file or malformed config yields no checks — never an error.
    pub fn load() -> Self {
        let Some(home) = std::env::var_os("HOME") else {
            return Self::default();
        };
        let path = std::path::Path::new(&home).join(".sirbone/config.json");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .map(|v| Self::from_value(v.get("post_edit_check")))
            .unwrap_or_default()
    }

    pub(crate) fn from_value(v: Option<&serde_json::Value>) -> Self {
        let rules = v
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(glob, cmd)| Some((glob.clone(), cmd.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Run every check whose glob matches an edited path (each command once).
    /// Returns a report block to append to the last tool result, or `None`
    /// when all checks pass (or none apply).
    pub async fn run(&self, edited_paths: &[String]) -> Option<String> {
        let mut commands: Vec<&str> = Vec::new();
        for (glob, cmd) in &self.rules {
            if !commands.contains(&cmd.as_str())
                && edited_paths.iter().any(|p| glob_matches(glob, p))
            {
                commands.push(cmd);
            }
        }
        let mut failures = String::new();
        for cmd in commands {
            if let Some(output) = run_check(cmd).await {
                failures.push_str(&format!(
                    "\n\n[system] post-edit check failed — fix before proceeding:\n$ {cmd}\n{output}"
                ));
            }
        }
        (!failures.is_empty()).then_some(failures)
    }
}

/// A `pre_tool_use` gate hook: a shell command run before a tool executes,
/// selected when its `matcher` glob matches the tool name. The command receives
/// `{"tool":…,"input":…}` JSON on stdin. Exit `0` = allow (and skip the LLM
/// classifier), exit `2` = deny (stdout/stderr becomes the reason shown to the
/// model), any other exit (or spawn failure) = no verdict, fall through to the
/// normal permission path.
#[derive(Debug, Clone)]
pub struct PreHook {
    pub matcher: String,
    pub command: String,
}

/// Verdict of the [`Hooks::pre_tool_use`] gate.
pub enum PreVerdict {
    Allow,
    Deny(String),
    /// No matching hook returned a decisive exit code.
    Pass,
}

/// Deterministic shell hooks over the tool-call lifecycle (config key `hooks`).
/// One struct, three events — not a parallel system: `post` *is* the existing
/// [`PostEditChecks`].
#[derive(Debug, Clone, Default)]
pub struct Hooks {
    /// `pre_tool_use`: exit-code gate run before a tool, by tool-name glob.
    pub pre: Vec<PreHook>,
    /// `post_tool_use`: the advisory post-edit checks (also legacy `post_edit_check`).
    pub post: PostEditChecks,
    /// `stop`: commands consulted when the agent would finish; exit `2` forces
    /// another loop iteration (its output is fed back as the continuation reason).
    pub stop: Vec<String>,
}

impl Hooks {
    /// Load the merged (`global` + per-project) `hooks` section. When that section
    /// has no `post_tool_use`, fall back to the legacy top-level `post_edit_check`.
    pub fn load() -> Self {
        let section = crate::config::section("hooks");
        let pre = section
            .as_ref()
            .and_then(|v| v.get("pre_tool_use"))
            .map(parse_pre_hooks)
            .unwrap_or_default();
        let stop = section
            .as_ref()
            .and_then(|v| v.get("stop"))
            .map(parse_command_list)
            .unwrap_or_default();
        let post = match section.as_ref().and_then(|v| v.get("post_tool_use")) {
            Some(v) => PostEditChecks::from_value(Some(v)),
            None => PostEditChecks::load(),
        };
        Self { pre, post, stop }
    }

    /// Run `pre_tool_use` hooks matching `tool`. The first hook to return a
    /// decisive exit code (0 allow / 2 deny) wins; otherwise [`PreVerdict::Pass`].
    pub async fn pre_tool_use(&self, tool: &str, input: &serde_json::Value) -> PreVerdict {
        if self.pre.is_empty() {
            return PreVerdict::Pass;
        }
        let payload = serde_json::json!({ "tool": tool, "input": input }).to_string();
        for h in &self.pre {
            if !glob_matches(&h.matcher, tool) {
                continue;
            }
            match run_hook(&h.command, &payload).await {
                Some((0, _)) => return PreVerdict::Allow,
                Some((2, out)) => {
                    let reason = if out.is_empty() {
                        format!("blocked by pre_tool_use hook: {}", h.command)
                    } else {
                        out
                    };
                    return PreVerdict::Deny(reason);
                }
                _ => continue,
            }
        }
        PreVerdict::Pass
    }

    /// Consult `stop` hooks when the agent would finish. `Some(reason)` (a hook
    /// exited `2`) means "not done" — the caller resumes the loop with `reason`
    /// as feedback. `None` lets the run terminate.
    pub async fn stop(&self) -> Option<String> {
        for cmd in &self.stop {
            if let Some((2, out)) = run_hook(cmd, "").await {
                return Some(if out.is_empty() {
                    format!("stop hook requested another iteration: {cmd}")
                } else {
                    out
                });
            }
        }
        None
    }
}

/// Parse `[{ "match": glob, "command": cmd }, …]`; entries missing either field
/// are skipped. `match` defaults to `"*"` (all tools) when absent.
fn parse_pre_hooks(v: &serde_json::Value) -> Vec<PreHook> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let command = e.get("command")?.as_str()?.to_string();
                    let matcher = e
                        .get("match")
                        .and_then(|m| m.as_str())
                        .unwrap_or("*")
                        .to_string();
                    Some(PreHook { matcher, command })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `[{ "command": cmd }, …]` (or a bare `[cmd, …]`) into command strings.
fn parse_command_list(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    e.as_str()
                        .map(String::from)
                        .or_else(|| e.get("command").and_then(|c| c.as_str()).map(String::from))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Run a hook command feeding `stdin`, returning `(exit_code, combined_output)`.
/// `None` on spawn failure or timeout (treated as "no verdict" by callers).
async fn run_hook(cmd: &str, stdin: &str) -> Option<(i32, String)> {
    run_hook_with_timeout(cmd, stdin, std::time::Duration::from_secs(TIMEOUT_SECS)).await
}

async fn run_hook_with_timeout(
    cmd: &str,
    stdin: &str,
    timeout: std::time::Duration,
) -> Option<(i32, String)> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    if let Some(mut si) = child.stdin.take() {
        si.write_all(stdin.as_bytes()).await.ok();
        drop(si);
    }
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err.trim());
    }
    Some((out.status.code().unwrap_or(-1), text))
}

/// Run one check command; `Some(output)` on failure, `None` on success.
async fn run_check(cmd: &str) -> Option<String> {
    let fut = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output();
    let out = match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), fut).await {
        Err(_) => return Some(format!("(timed out after {TIMEOUT_SECS}s)")),
        Ok(Err(e)) => return Some(format!("(failed to spawn: {e})")),
        Ok(Ok(out)) => out,
    };
    if out.status.success() {
        return None;
    }
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err.trim());
    }
    if text.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… [check output truncated]");
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(json: &str) -> PostEditChecks {
        let v: serde_json::Value = serde_json::from_str(json).expect("test json");
        PostEditChecks::from_value(Some(&v))
    }

    #[test]
    fn parses_config_and_handles_garbage() {
        let c = checks(r#"{"*.rs": "cargo check -q", "*.py": "ruff check -q"}"#);
        assert_eq!(c.rules.len(), 2);
        assert!(PostEditChecks::from_value(None).is_empty());
        assert!(
            checks(r#"{"*.rs": 42}"#).is_empty(),
            "non-string command skipped"
        );
    }

    #[tokio::test]
    async fn failing_check_reports_and_passing_is_silent() {
        let c = checks(r#"{"*.rs": "echo boom >&2; exit 1", "*.py": "true"}"#);
        let report = c.run(&["src/foo.rs".into()]).await.expect("must fail");
        assert!(report.contains("post-edit check failed"));
        assert!(report.contains("boom"));
        assert!(
            c.run(&["a.py".into()]).await.is_none(),
            "passing check is silent"
        );
        assert!(c.run(&["a.go".into()]).await.is_none(), "no matching glob");
    }

    #[tokio::test]
    async fn same_command_runs_once_for_many_files() {
        // Two globs, same command: a batch touching both kinds must not
        // duplicate the report.
        let c = checks(r#"{"*.rs": "echo dup; exit 1", "src/*": "echo dup; exit 1"}"#);
        let report = c
            .run(&["src/a.rs".into(), "src/b.rs".into()])
            .await
            .expect("fails");
        assert_eq!(report.matches("post-edit check failed").count(), 1);
    }

    fn pre(json: &str) -> Vec<PreHook> {
        parse_pre_hooks(&serde_json::from_str(json).expect("test json"))
    }

    #[test]
    fn parse_pre_hooks_defaults_match_to_star_and_skips_incomplete() {
        let hs = pre(r#"[{"match":"bash","command":"a"},{"command":"b"},{"match":"x"}]"#);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].matcher, "bash");
        assert_eq!(hs[1].matcher, "*", "missing match defaults to all tools");
    }

    #[test]
    fn parse_command_list_accepts_bare_and_object() {
        let v = serde_json::json!(["a", {"command": "b"}, {"nope": 1}]);
        assert_eq!(
            parse_command_list(&v),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn malformed_hook_config_is_ignored_safely() {
        assert!(parse_pre_hooks(&serde_json::json!({"bad": true})).is_empty());
        assert!(parse_pre_hooks(&serde_json::json!([
            {"match": "bash"},
            {"command": 1},
            "nope"
        ]))
        .is_empty());
        assert!(parse_command_list(&serde_json::json!({"bad": true})).is_empty());
    }

    #[tokio::test]
    async fn pre_gate_allows_denies_and_passes() {
        let allow = Hooks {
            pre: pre(r#"[{"match":"bash","command":"exit 0"}]"#),
            ..Default::default()
        };
        assert!(matches!(
            allow.pre_tool_use("bash", &serde_json::json!({})).await,
            PreVerdict::Allow
        ));

        let deny = Hooks {
            pre: pre(r#"[{"match":"bash","command":"echo nope >&2; exit 2"}]"#),
            ..Default::default()
        };
        match deny.pre_tool_use("bash", &serde_json::json!({})).await {
            PreVerdict::Deny(r) => assert!(r.contains("nope"), "{r}"),
            _ => panic!("expected deny"),
        }

        // Non-matching tool and non-decisive exit both fall through to Pass.
        let miss = Hooks {
            pre: pre(r#"[{"match":"bash","command":"exit 0"}]"#),
            ..Default::default()
        };
        assert!(matches!(
            miss.pre_tool_use("read", &serde_json::json!({})).await,
            PreVerdict::Pass
        ));
        let other = Hooks {
            pre: pre(r#"[{"match":"bash","command":"exit 1"}]"#),
            ..Default::default()
        };
        assert!(matches!(
            other.pre_tool_use("bash", &serde_json::json!({})).await,
            PreVerdict::Pass
        ));

        // No hooks configured: cheap Pass without spawning anything.
        assert!(matches!(
            Hooks::default()
                .pre_tool_use("bash", &serde_json::json!({}))
                .await,
            PreVerdict::Pass
        ));
    }

    #[tokio::test]
    async fn pre_gate_receives_tool_and_input_on_stdin() {
        let h = Hooks {
            pre: pre(r#"[{"match":"*","command":"grep -q '\"tool\":\"bash\"' && exit 2"}]"#),
            ..Default::default()
        };
        assert!(matches!(
            h.pre_tool_use("bash", &serde_json::json!({"command":"ls"}))
                .await,
            PreVerdict::Deny(_)
        ));
    }

    #[tokio::test]
    async fn hook_timeout_returns_no_verdict() {
        let out = run_hook_with_timeout(
            "sleep 1; echo late; exit 2",
            "",
            std::time::Duration::from_millis(10),
        )
        .await;
        assert!(out.is_none(), "timed-out hooks must not hang or deny");
    }

    #[tokio::test]
    async fn stop_hook_forces_continuation_on_exit_2() {
        let cont = Hooks {
            stop: vec!["echo keep going; exit 2".into()],
            ..Default::default()
        };
        assert_eq!(cont.stop().await.as_deref(), Some("keep going"));
        let done = Hooks {
            stop: vec!["exit 0".into()],
            ..Default::default()
        };
        assert!(done.stop().await.is_none());
        assert!(Hooks::default().stop().await.is_none());
    }
}
