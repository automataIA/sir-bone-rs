//! Verification Oracle: an opt-in post-run test loop (`--oracle`).
//!
//! When the agent reports Done, sirbone runs the project's configured test
//! command. Green → finish. Red → the failure log (signal lines hoisted, then
//! truncated) is injected back as a user message and the agent loop resumes,
//! capped at `max_attempts` (Fail Loud → hand back to the human). When an attempt
//! makes things worse — more tests failing than the best state so far — the
//! workspace is rolled back to that best state's snapshot, so the agent never
//! digs deeper from a broken tree (the "hallucination cascade").
//!
//! The injected prompt asks the model to re-read its saved plan note and explain
//! the failure before editing: the explain-before-fix tactic from *Teaching LLMs
//! to Self-Debug* (arXiv 2304.05128). Separating a fix step from an independent
//! verification step echoes *AgentCoder* (arXiv 2312.13010).
//!
//! Config (`~/.sirbone/config.json`, key `oracle`):
//! `{"test_command": "cargo test -q", "max_attempts": 3}`. No command → disabled.

use std::time::Duration;

use crate::snapshot::Snapshots;
use crate::tools::truncate::truncate_default;
use crate::types::{AgentEvent, EventTx, NoticeLevel};

const DEFAULT_MAX_ATTEMPTS: usize = 3;
const TEST_TIMEOUT_SECS: u64 = 600;

/// Lines worth hoisting to the top of the failure feedback so the model sees the
/// signal first (Rust spans/panics/errors, Python tracebacks).
const SIGNAL_MARKERS: [&str; 5] = ["-->", "panicked at", "error[", "error:", "File \""];

/// One verification cycle's verdict.
#[derive(Debug, Clone)]
pub struct OracleResult {
    pub passed: bool,
    /// Failing-test count — the regression metric. `usize::MAX` for a test
    /// command that timed out or could not spawn.
    pub failed: usize,
    pub raw: String,
}

/// Outcome of an oracle gate cycle, consumed by the agent loop.
pub enum Outcome {
    /// Stop the loop: tests pass, or attempts exhausted (Fail-Loud event emitted).
    Done,
    /// Inject this message as a new user turn and keep looping.
    Retry(String),
}

pub struct Oracle {
    test_command: String,
    max_attempts: usize,
    attempts: usize,
    /// Fewest failures seen so far; `None` until the first red cycle.
    best_failed: Option<usize>,
    /// Commit id of the snapshot taken at the best state — the rollback target.
    last_snapshot: Option<String>,
}

impl Oracle {
    /// Load the `oracle` section: per-project
    /// `~/.sirbone/projects/<slug>/config.json` if it defines it, else global
    /// `~/.sirbone/config.json`. `None` = disabled (missing/malformed config, or
    /// empty `test_command`) — never an error.
    pub fn load() -> Option<Self> {
        Self::from_value(crate::config::section("oracle").as_ref())
    }

    fn from_value(v: Option<&serde_json::Value>) -> Option<Self> {
        let obj = v?.as_object()?;
        let test_command = obj.get("test_command")?.as_str()?.trim().to_string();
        if test_command.is_empty() {
            return None;
        }
        let max_attempts = obj
            .get("max_attempts")
            .and_then(|m| m.as_u64())
            .map_or(DEFAULT_MAX_ATTEMPTS, |n| (n as usize).max(1));
        Some(Self {
            test_command,
            max_attempts,
            attempts: 0,
            best_failed: None,
            last_snapshot: None,
        })
    }

    /// Run one verification cycle. On red, applies rollback-on-regression and
    /// returns the feedback to inject; on green or exhaustion returns `Done`.
    pub async fn gate(&mut self, snapshots: Option<&Snapshots>, events: &EventTx) -> Outcome {
        let result = self.run_tests().await;
        if result.passed {
            notice(
                events,
                NoticeLevel::Success,
                "[oracle] all tests pass".into(),
            )
            .await;
            return Outcome::Done;
        }

        self.attempts += 1;

        // A timeout or spawn failure carries no pass/fail signal (`failed ==
        // usize::MAX`): comparing it would always read as "regressed" and trigger
        // a spurious rollback (plus a `18446744073709551615 failing` notice).
        // Treat it as an infra flake — neither compare, rollback, nor snapshot.
        let infra = result.failed == usize::MAX;
        let regressed = if infra {
            false
        } else {
            let reg = self.best_failed.is_some_and(|best| result.failed > best);
            if reg {
                self.rollback(snapshots, events, result.failed).await;
            } else {
                // New best (or first red): snapshot it as the next rollback target.
                self.best_failed = Some(result.failed);
                if let Some(snaps) = snapshots {
                    match snaps.snapshot_id("oracle: best-so-far").await {
                        Ok(id) => self.last_snapshot = Some(id),
                        Err(e) => tracing::warn!("oracle snapshot failed: {e}"),
                    }
                }
            }
            reg
        };

        if self.attempts > self.max_attempts {
            // Critical: give up and hand back to the human — stays on the red
            // `Error` channel.
            error(
                events,
                format!(
                    "[oracle] still failing after {} attempts — stopping, over to you",
                    self.max_attempts
                ),
            )
            .await;
            return Outcome::Done;
        }
        let msg = if infra {
            format!(
                "[oracle] test run did not complete (timeout/spawn) — retry {}/{}",
                self.attempts, self.max_attempts
            )
        } else {
            format!(
                "[oracle] tests failing ({}) — retry {}/{}",
                result.failed, self.attempts, self.max_attempts
            )
        };
        notice(events, NoticeLevel::Info, msg).await;
        Outcome::Retry(self.feedback(&result, regressed))
    }

    async fn rollback(&self, snapshots: Option<&Snapshots>, events: &EventTx, failed: usize) {
        let (Some(snaps), Some(id)) = (snapshots, &self.last_snapshot) else {
            return;
        };
        match snaps.rollback(id).await {
            Ok(_) => {
                notice(
                    events,
                    NoticeLevel::Info,
                    format!("[oracle] attempt worsened tests ({failed} failing) — rolled back to last good state"),
                )
                .await
            }
            Err(e) => error(events, format!("[oracle] rollback failed: {e}")).await,
        }
    }

    async fn run_tests(&self) -> OracleResult {
        run_command(&self.test_command).await
    }

    fn feedback(&self, result: &OracleResult, regressed: bool) -> String {
        let reverted = if regressed {
            "\n\nNote: your last change made more tests fail, so the workspace was rolled back to \
             the previous state. Try a different approach."
        } else {
            ""
        };
        format!(
            "The verification step ran the project's tests and they failed.\n\n{diag}\n\n\
             Before changing code:\n\
             1. Re-read the approved plan in your saved note — do not violate the original spec to make a test pass.\n\
             2. In one sentence, explain why the test is failing.\n\
             3. Then apply the fix.{reverted}",
            diag = diagnostic(&result.raw),
        )
    }
}

/// Read just the configured `oracle.test_command` (for the on-demand `verify`
/// tool, which doesn't need the retry/rollback state of a full `Oracle`).
pub fn load_test_command() -> Option<String> {
    let cmd = crate::config::section("oracle")?
        .get("test_command")?
        .as_str()?
        .trim()
        .to_string();
    (!cmd.is_empty()).then_some(cmd)
}

/// Run the test command once and return a verdict. Shared by the post-Done gate
/// and the on-demand `verify` tool.
async fn run_command(command: &str) -> OracleResult {
    let fut = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output();
    match tokio::time::timeout(Duration::from_secs(TEST_TIMEOUT_SECS), fut).await {
        Err(_) => OracleResult {
            passed: false,
            failed: usize::MAX,
            raw: format!("(test command timed out after {TEST_TIMEOUT_SECS}s)"),
        },
        Ok(Err(e)) => OracleResult {
            passed: false,
            failed: usize::MAX,
            raw: format!("(failed to spawn test command: {e})"),
        },
        Ok(Ok(out)) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                s.push('\n');
                s.push_str(&err);
            }
            parse_result(&s, out.status.success())
        }
    }
}

/// One-shot verification for the `verify` tool: run the configured test command
/// and return a model-readable verdict (pass, or hoisted-diagnostic on failure).
pub async fn verify_once() -> String {
    let Some(cmd) = load_test_command() else {
        return "No test command configured. Set `oracle.test_command` in ~/.sirbone/config.json \
                to enable verification."
            .into();
    };
    let r = run_command(&cmd).await;
    if r.passed {
        format!("✓ all tests pass (`{cmd}`)")
    } else {
        format!(
            "✗ tests failing ({}) via `{cmd}`\n\n{}",
            r.failed,
            diagnostic(&r.raw)
        )
    }
}

async fn notice(events: &EventTx, level: NoticeLevel, text: String) {
    events.send(AgentEvent::Notice { text, level }).await.ok();
}

async fn error(events: &EventTx, msg: String) {
    events.send(AgentEvent::Error(msg)).await.ok();
}

/// Hoist signal lines above the (truncated) full log so the model reads the
/// diagnosis first even when the raw output is large.
fn diagnostic(raw: &str) -> String {
    let signal: String = raw
        .lines()
        .filter(|l| SIGNAL_MARKERS.iter().any(|m| l.contains(m)))
        .take(20)
        .map(|l| format!("{l}\n"))
        .collect();
    let body = truncate_default(raw.to_string());
    if signal.is_empty() {
        body
    } else {
        format!("Key lines:\n{signal}\nFull output:\n{body}")
    }
}

/// Map a runner's output + exit status to a verdict. A clean exit is a pass
/// regardless of text; on failure, count failing tests from the summary line
/// (libtest "… N failed", pytest "N failed,", jest "N failed,"), defaulting to 1
/// when no count is recognised.
fn parse_result(output: &str, success: bool) -> OracleResult {
    if success {
        return OracleResult {
            passed: true,
            failed: 0,
            raw: output.to_string(),
        };
    }
    let failed = count_failed(output).unwrap_or(1);
    OracleResult {
        passed: false,
        failed,
        raw: output.to_string(),
    }
}

/// Sum every `<n> failed` occurrence (libtest prints one per test binary).
/// `None` when no such pattern is present.
fn count_failed(output: &str) -> Option<usize> {
    let toks: Vec<&str> = output.split_whitespace().collect();
    let mut total = 0usize;
    let mut found = false;
    for (i, tok) in toks.iter().enumerate() {
        if i == 0 {
            continue;
        }
        if tok.trim_end_matches([';', ',', '.']) == "failed" {
            if let Ok(n) = toks[i - 1].parse::<usize>() {
                total += n;
                found = true;
            }
        }
    }
    found.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(json: &str) -> Option<Oracle> {
        let v: serde_json::Value = serde_json::from_str(json).expect("test json");
        Oracle::from_value(v.get("oracle"))
    }

    #[test]
    fn config_load_and_garbage() {
        let o = cfg(r#"{"oracle": {"test_command": "cargo test -q", "max_attempts": 5}}"#)
            .expect("loads");
        assert_eq!(o.test_command, "cargo test -q");
        assert_eq!(o.max_attempts, 5);
        // Defaults + disabling cases.
        assert_eq!(
            cfg(r#"{"oracle": {"test_command": "x"}}"#)
                .unwrap()
                .max_attempts,
            DEFAULT_MAX_ATTEMPTS
        );
        assert!(
            cfg(r#"{"oracle": {"test_command": "  "}}"#).is_none(),
            "blank command disables"
        );
        assert!(cfg(r#"{"oracle": {}}"#).is_none(), "no command disables");
        assert!(cfg(r#"{}"#).is_none(), "no key disables");
        assert_eq!(
            cfg(r#"{"oracle": {"test_command": "x", "max_attempts": 0}}"#)
                .unwrap()
                .max_attempts,
            1
        );
    }

    #[test]
    fn parses_failure_counts() {
        // Rust libtest, summed across two binaries.
        let rust = "test result: FAILED. 4 passed; 3 failed; 0 ignored\n\
                    test result: FAILED. 1 passed; 2 failed; 0 ignored";
        assert_eq!(count_failed(rust), Some(5));
        // pytest and jest summary lines.
        assert_eq!(count_failed("=== 3 failed, 5 passed in 1.20s ==="), Some(3));
        assert_eq!(
            count_failed("Tests: 2 failed, 10 passed, 12 total"),
            Some(2)
        );
        // No pattern.
        assert_eq!(count_failed("everything is fine"), None);
    }

    #[test]
    fn parse_result_trusts_exit_status() {
        // Clean exit is a pass even if the word "failed" appears in output.
        let r = parse_result("0 failed", true);
        assert!(r.passed && r.failed == 0);
        // Non-zero exit with no count defaults to one failure.
        let r = parse_result("compilation error", false);
        assert!(!r.passed && r.failed == 1);
        let r = parse_result("1 passed; 2 failed", false);
        assert_eq!(r.failed, 2);
    }

    #[test]
    fn diagnostic_hoists_signal_lines() {
        let raw = "running 1 test\n  --> src/foo.rs:10:5\nlots of noise\npanicked at 'boom'";
        let d = diagnostic(raw);
        assert!(d.starts_with("Key lines:"));
        assert!(d.contains("--> src/foo.rs:10:5"));
        assert!(d.contains("panicked at 'boom'"));
        // No markers → just the (truncated) body, no header.
        assert!(!diagnostic("plain output").starts_with("Key lines:"));
    }
}
