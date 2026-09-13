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
//! `{"test_command": "cargo test -q", "max_attempts": 3, "min_tests": 1}`.
//! `min_tests` is optional and defaults to zero; set it for filtered test
//! commands that must not accept a successful zero-test run. No command → disabled.

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
    /// Minimum number of executed tests required for a green verdict. This is
    /// opt-in because non-test commands (for example `cargo check`) are valid
    /// oracle commands too. A filtered Cargo invocation can set this to `1` so
    /// libtest's successful "0 tests" exit cannot masquerade as verification.
    min_tests: usize,
    max_attempts: usize,
    attempts: usize,
    /// Fewest failures seen so far; `None` until the first red cycle.
    best_failed: Option<usize>,
    /// Commit id of the snapshot taken at the best state — the rollback target.
    last_snapshot: Option<String>,
}

impl Oracle {
    /// Build an explicitly configured oracle. Configuration loaders should
    /// normally use [`Self::load`]; this constructor keeps offline replays and
    /// embedders independent from process-global project configuration.
    pub fn new(test_command: impl Into<String>, max_attempts: usize) -> Option<Self> {
        let test_command = test_command.into().trim().to_string();
        if test_command.is_empty() {
            return None;
        }
        Some(Self {
            test_command,
            min_tests: 0,
            max_attempts: max_attempts.max(1),
            attempts: 0,
            best_failed: None,
            last_snapshot: None,
        })
    }

    /// Load the `oracle` section: per-project
    /// `~/.sirbone/projects/<slug>/config.json` if it defines it, else global
    /// `~/.sirbone/config.json`. `None` = disabled (missing/malformed config, or
    /// empty `test_command`) — never an error.
    pub fn load() -> Option<Self> {
        let configured = Self::from_value(crate::config::section("oracle").as_ref());
        if crate::ablate::oracle_gate_disabled() {
            None
        } else {
            configured
        }
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
        let min_tests = obj.get("min_tests").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
        let mut oracle = Self::new(test_command, max_attempts)?;
        oracle.min_tests = min_tests;
        Some(oracle)
    }

    /// Run one verification cycle. On red, applies rollback-on-regression and
    /// returns the feedback to inject; on green or exhaustion returns `Done`.
    pub async fn gate(&mut self, snapshots: Option<&Snapshots>, events: &EventTx) -> Outcome {
        let result = self.run_tests().await;
        crate::telemetry::add(&crate::telemetry::ORACLE_RUNS, 1);
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
        crate::telemetry::add(&crate::telemetry::ORACLE_FAILURES, 1);

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

        if self.attempts >= self.max_attempts {
            crate::telemetry::add(&crate::telemetry::ORACLE_EXHAUSTED, 1);
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
        crate::telemetry::add(&crate::telemetry::ORACLE_RETRIES, 1);
        Outcome::Retry(self.feedback(&result, regressed))
    }

    async fn rollback(&self, snapshots: Option<&Snapshots>, events: &EventTx, failed: usize) {
        let (Some(snaps), Some(id)) = (snapshots, &self.last_snapshot) else {
            return;
        };
        match snaps.rollback(id).await {
            Ok(_) => {
                crate::telemetry::add(&crate::telemetry::ORACLE_ROLLBACKS, 1);
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
        run_command(&self.test_command, self.min_tests).await
    }

    /// Run the authoritative check once when the LLM-turn budget is exhausted.
    ///
    /// This deliberately cannot request another model turn: the budget remains
    /// a hard cap. It does ensure that a correct patch is not rejected merely
    /// because the model spent its final turn on a tool call instead of a prose
    /// `Done`, and that a red workspace fails loudly with deterministic evidence.
    pub(crate) async fn final_gate(&mut self, events: &EventTx) -> bool {
        let result = self.run_tests().await;
        crate::telemetry::add(&crate::telemetry::ORACLE_RUNS, 1);
        if result.passed {
            notice(
                events,
                NoticeLevel::Success,
                "[oracle] final budget gate: all tests pass".into(),
            )
            .await;
            true
        } else {
            crate::telemetry::add(&crate::telemetry::ORACLE_FAILURES, 1);
            error(
                events,
                format!(
                    "[oracle] final budget gate failed; no LLM turns remain\n\n{}",
                    diagnostic(&result.raw)
                ),
            )
            .await;
            false
        }
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

/// Read `oracle.min_tests` (0 when unset), the companion of
/// [`load_test_command`] for callers that run the command themselves.
pub fn load_min_tests() -> usize {
    crate::config::section("oracle")
        .and_then(|o| o.get("min_tests").and_then(serde_json::Value::as_u64))
        .unwrap_or(0) as usize
}

/// Score the current work-tree, for a caller that *selects between* independent
/// attempts instead of repairing one.
///
/// Same runner as the gate and the `verify` tool, deliberately: the point of
/// best-of-K selection is that the judge is execution, not the generator
/// grading itself. Carries no retry or rollback state.
pub async fn score_workspace(command: &str, min_tests: usize) -> OracleResult {
    run_command(command, min_tests).await
}

/// Run the test command once and return a verdict. Shared by the post-Done gate
/// and the on-demand `verify` tool.
async fn run_command(command: &str, min_tests: usize) -> OracleResult {
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
            parse_result(&s, out.status.success(), min_tests)
        }
    }
}

/// One-shot verification for the `verify` tool: run the configured test command
/// and return a model-readable verdict (pass, or hoisted-diagnostic on failure).
pub async fn verify_once() -> String {
    verify_with_command(load_test_command()).await
}

/// Runs verification with an explicitly supplied command.
///
/// Keeping configuration lookup outside this core makes callers such as tests
/// deterministic and prevents them from inheriting a user's recursive test command.
pub(crate) async fn verify_with_command(command: Option<String>) -> String {
    let Some(cmd) = command else {
        return "No test command configured. Set `oracle.test_command` in ~/.sirbone/config.json \
                to enable verification."
            .into();
    };
    crate::telemetry::add(&crate::telemetry::VERIFY_TOOL_RUNS, 1);
    let r = run_command(&cmd, 0).await;
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
fn parse_result(output: &str, success: bool, min_tests: usize) -> OracleResult {
    if success {
        if min_tests > 0 {
            let executed = count_test_summary(output);
            if executed < min_tests {
                return OracleResult {
                    passed: false,
                    failed: 1,
                    raw: format!(
                        "verification command executed {executed} test(s), fewer than required {min_tests}\n{output}"
                    ),
                };
            }
        }
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

/// Sum passed and failed tests from libtest/pytest-style summaries. This is
/// intentionally used only when `oracle.min_tests` opts in.
fn count_test_summary(output: &str) -> usize {
    let toks: Vec<&str> = output.split_whitespace().collect();
    let mut total = 0usize;
    for (i, tok) in toks.iter().enumerate().skip(1) {
        let label = tok.trim_end_matches([';', ',', '.']);
        if matches!(label, "passed" | "failed") {
            if let Ok(n) = toks[i - 1].parse::<usize>() {
                total = total.saturating_add(n);
            }
        }
    }
    total
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
        assert_eq!(o.min_tests, 0);
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
        assert_eq!(
            cfg(r#"{"oracle": {"test_command": "x", "min_tests": 2}}"#)
                .unwrap()
                .min_tests,
            2
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
        let r = parse_result("0 failed", true, 0);
        assert!(r.passed && r.failed == 0);
        // Non-zero exit with no count defaults to one failure.
        let r = parse_result("compilation error", false, 0);
        assert!(!r.passed && r.failed == 1);
        let r = parse_result("1 passed; 2 failed", false, 0);
        assert_eq!(r.failed, 2);
    }

    #[test]
    fn minimum_test_count_rejects_a_green_zero_test_filter() {
        let zero = "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored";
        let r = parse_result(zero, true, 1);
        assert!(!r.passed);
        assert!(r.raw.contains("executed 0 test(s)"), "{}", r.raw);

        let one = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored";
        assert!(parse_result(one, true, 1).passed);
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
