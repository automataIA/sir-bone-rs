use std::process::Stdio;
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
         immediately; poll with job_status instead of blocking."
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
        let child = Command::new("bash")
            .arg("-c")
            .arg(&input.command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
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

        let result = match (!stdout.is_empty(), !stderr.is_empty()) {
            (true, true) => format!("{stdout}\n{stderr}"),
            (true, false) => stdout.into_owned(),
            (false, true) => stderr.into_owned(),
            (false, false) if !output.status.success() => {
                format!("exit code: {}", output.status.code().unwrap_or(-1))
            }
            (false, false) => String::new(),
        };

        Ok(truncate_default(result))
    }
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
