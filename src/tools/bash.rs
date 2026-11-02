use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

use super::{jobs::JobStore, truncate::truncate_default, TypedTool};

#[derive(Deserialize, JsonSchema)]
pub struct BashInput {
    /// Shell command to execute
    pub command: String,
    /// Timeout in milliseconds (default: 30000)
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Run detached as a background job: returns a job id immediately instead
    /// of waiting. Check progress with the `job_status` tool; the user is
    /// notified automatically when the job finishes.
    #[serde(default)]
    pub background: bool,
}

fn default_timeout() -> u64 {
    30_000
}

#[derive(Default)]
pub struct BashTool {
    pub jobs: JobStore,
}

#[async_trait]
impl TypedTool for BashTool {
    type Input = BashInput;

    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command and return its output (stdout + stderr). \
         Each call spawns a fresh shell: `cd` and env vars do NOT persist between \
         calls — use absolute paths and chain dependent commands with `&&` in one \
         call. Independent commands can be issued as separate parallel tool calls. \
         Prefer the dedicated tools over shell equivalents: `read` over cat, `grep` \
         over grep/rg, `glob`/`find` over find, `edit`/`sed` over sed -i, `write` \
         over echo/heredoc — they return cleaner, structured results. Quote file \
         paths that contain spaces. Git: avoid destructive operations (`reset \
         --hard`, `push --force`, `checkout .`) unless explicitly requested, and \
         never skip hooks with `--no-verify` — fix the underlying issue instead. \
         For long-running work set background:true — it returns a job id \
         immediately; poll with job_status instead of blocking. \
         Large output is truncated in the middle, and the full text is saved to \
         a file whose path is in the truncation marker — read that file instead \
         of re-running the command."
    }

    async fn run(&self, input: BashInput) -> Result<String> {
        if input.background {
            let (id, log) = self.jobs.spawn(&input.command)?;
            return Ok(format!(
                "started background job #{id} (log: {}). Don't busy-wait: do \
                 other work or end the turn — the user is notified when it \
                 finishes; check details with job_status.",
                log.display()
            ));
        }
        // process_group(0): bash leads its own group, so on timeout we can signal
        // the whole group (bash + grandchildren) instead of just the leader.
        // kill_on_drop is belt-and-suspenders for the leader when the future drops.
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(&input.command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let child = command
            .kill_on_drop(true)
            .spawn()
            .context("failed to spawn bash")?;
        let pid = child.id();
        let fut = child.wait_with_output();
        let output = match tokio::time::timeout(Duration::from_millis(input.timeout_ms), fut).await
        {
            Ok(res) => res.context("failed to spawn bash")?,
            Err(_) => {
                // Negative pid = whole process group; the `kill` binary avoids
                // the unsafe FFI that `unsafe_code = "forbid"` rules out.
                if let Some(pid) = pid {
                    // `--` is required: without it `kill` swallows the negative
                    // pid as an option and silently signals nothing.
                    let _ = Command::new("kill")
                        .arg("-KILL")
                        .arg("--")
                        .arg(format!("-{pid}"))
                        // The process group may exit between the timeout and
                        // this cleanup attempt; that race is harmless and
                        // should not leak `kill` diagnostics into the caller.
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .await;
                }
                bail!(
                    "command timed out after {}ms (process group killed). For long-running work, \
                     raise timeout_ms, or run it in the background (e.g. \
                     `nohup CMD >out.log 2>&1 &`) and poll the log.",
                    input.timeout_ms
                );
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // A plain recursive search for a bare identifier gets the structural view
        // the `grep` tool gives it (see `grep::shell_structural_view`).
        if output.status.success() && stderr.is_empty() {
            if let Some(view) = super::grep::shell_structural_view(&input.command, &stdout).await {
                return Ok(truncate_default(view));
            }
        }
        let body = match (!stdout.is_empty(), !stderr.is_empty()) {
            (true, true) => format!("{stdout}\n{stderr}"),
            (true, false) => stdout.into_owned(),
            (false, true) => stderr.into_owned(),
            (false, false) => String::new(),
        };
        // Failure status goes last and survives truncation: output alone (e.g. a
        // compiler's diagnostics) does not tell the agent the command failed.
        let status = match (output.status.code(), signal(output.status)) {
            (Some(0), _) => None,
            (Some(code), _) => Some(format!("exit code: {code}")),
            (None, Some(sig)) => Some(format!("killed by signal {sig}")),
            (None, None) => Some("exit code: -1".to_string()),
        };

        Ok(match (status, body.is_empty()) {
            (None, _) => truncate_default(body),
            (Some(s), true) => s,
            (Some(s), false) => format!("{}\n{s}", truncate_default(body).trim_end()),
        })
    }
}

#[cfg(unix)]
fn signal(status: ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(&status)
}

#[cfg(not(unix))]
fn signal(_: ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bash_echo() {
        let tool = BashTool::default();
        let result = tool
            .run(BashInput {
                command: "echo hello".into(),
                timeout_ms: 5000,
                background: false,
            })
            .await
            .unwrap();
        assert_eq!(result.trim(), "hello");
    }

    #[tokio::test]
    async fn bash_stderr() {
        let tool = BashTool::default();
        let result = tool
            .run(BashInput {
                command: "echo err >&2".into(),
                timeout_ms: 5000,
                background: false,
            })
            .await
            .unwrap();
        assert!(result.contains("err"));
    }

    // A failing command that prints (every compiler error) must still say it
    // failed: output alone does not tell the agent the exit status.
    #[tokio::test]
    async fn bash_nonzero_exit_with_output_reports_code() {
        let tool = BashTool::default();
        let result = tool
            .run(BashInput {
                command: "echo out; echo err >&2; exit 3".into(),
                timeout_ms: 5000,
                background: false,
            })
            .await
            .unwrap();
        assert!(result.contains("out") && result.contains("err"), "{result}");
        assert!(result.trim_end().ends_with("exit code: 3"), "{result}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_signal_termination_is_reported() {
        let tool = BashTool::default();
        let result = tool
            .run(BashInput {
                command: "echo partial; kill -9 $$".into(),
                timeout_ms: 5000,
                background: false,
            })
            .await
            .unwrap();
        assert!(result.contains("killed by signal 9"), "{result}");
    }

    #[tokio::test]
    async fn bash_success_has_no_exit_line() {
        let tool = BashTool::default();
        let result = tool
            .run(BashInput {
                command: "echo ok".into(),
                timeout_ms: 5000,
                background: false,
            })
            .await
            .unwrap();
        assert!(!result.contains("exit code"), "{result}");
    }

    #[tokio::test]
    async fn bash_background_returns_job_id_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool {
            jobs: JobStore::with_dir(dir.path().to_path_buf()),
        };
        let result = tool
            .run(BashInput {
                command: "sleep 5".into(),
                timeout_ms: 100,
                background: true,
            })
            .await
            .unwrap();
        // Returns at once (no timeout despite timeout_ms < sleep) with a job handle.
        assert!(result.contains("background job #1"), "{result}");
        assert_eq!(tool.jobs.running().len(), 1);
    }

    #[tokio::test]
    async fn bash_timeout() {
        let tool = BashTool::default();
        let result = tool
            .run(BashInput {
                command: "sleep 10".into(),
                timeout_ms: 100,
                background: false,
            })
            .await;
        assert!(result.is_err());
    }

    // A backgrounded grandchild (different from the bash leader) must die with
    // the group on timeout — otherwise it lingers holding the pipe open.
    #[tokio::test]
    async fn bash_timeout_kills_grandchildren() {
        let tool = BashTool::default();
        // 4793s is an unusual duration so pgrep won't match unrelated sleeps.
        // The trailing foreground sleep keeps bash alive past the timeout so the
        // group-kill path actually fires (otherwise bash would exit on its own).
        let result = tool
            .run(BashInput {
                command: "sleep 4793 & sleep 4793".into(),
                timeout_ms: 200,
                background: false,
            })
            .await;
        assert!(result.is_err());
        // Give the group-kill a moment to land, then assert the grandchild is gone.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let survivors = Command::new("pgrep")
            .arg("-f")
            .arg("sleep 4793")
            .output()
            .await
            .unwrap();
        assert!(
            survivors.stdout.is_empty(),
            "orphaned sleep survived: {}",
            String::from_utf8_lossy(&survivors.stdout)
        );
    }
}
