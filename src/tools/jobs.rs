use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::TypedTool;
use crate::types::lock_or_recover;

pub struct Job {
    pub id: u32,
    pub command: String,
    pub log_path: PathBuf,
    pub started: Instant,
    pub started_unix: u64,
    pub pid: Option<u32>,
    /// `Some((exit_code, wall_time))` once the process has exited.
    /// `exit_code = None` = died without reporting (killed, crash, reboot).
    pub done: Option<(Option<i32>, Duration)>,
    /// The UI already surfaced the completion of this job.
    pub notified: bool,
    /// Last progress marker seen in the log (0.0–1.0), if the job emits any.
    pub progress: Option<f32>,
    /// How far into the log `poll_progress` has scanned.
    pub log_offset: u64,
}

/// On-disk form of a running job, in `registry-<sirbone pid>.json`. A later
/// sirbone finding the owner pid dead adopts these entries.
#[derive(Serialize, Deserialize)]
struct PersistedJob {
    id: u32,
    pid: u32,
    command: String,
    log: PathBuf,
    started_unix: u64,
}

#[derive(Default)]
struct JobState {
    next_id: u32,
    jobs: Vec<Job>,
}

/// Shared store of background jobs (`bash` with `background: true`). Clones
/// share state (Arc), so the bash tool, the `job_status` tool and the UI all
/// see the same jobs.
///
/// Lifecycle is decoupled from the sirbone process: the spawned command itself
/// writes its exit code to `<log>.exit`, and running jobs are mirrored to a
/// per-process registry file. If sirbone dies, the next start adopts the
/// orphans (`restore_orphans`): `.exit` present → finished while away; pid
/// alive → keep watching; pid dead without `.exit` → finished, exit unknown.
#[derive(Clone)]
pub struct JobStore {
    state: Arc<Mutex<JobState>>,
    dir: PathBuf,
}

impl Default for JobStore {
    fn default() -> Self {
        let dir = dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".sirbone")
            .join("jobs");
        Self::with_dir(dir)
    }
}

fn exit_path(log: &Path) -> PathBuf {
    log.with_extension("exit")
}

/// `Some(code)` when the sentinel exists (code `None` if unparsable), `None`
/// when the job hasn't written it yet.
fn read_exit_file(p: &Path) -> Option<Option<i32>> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().parse::<i32>().ok())
}

/// Liveness via procfs (Linux/WSL — sirbone's targets). A recycled pid can
/// false-positive; the `.exit` sentinel remains the authoritative signal.
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

impl JobStore {
    pub fn with_dir(dir: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(JobState::default())),
            dir,
        }
    }

    fn registry_path(&self) -> PathBuf {
        self.dir
            .join(format!("registry-{}.json", std::process::id()))
    }

    /// Mirror the running jobs to this process's registry file (best-effort).
    fn write_registry(&self) {
        let persisted: Vec<PersistedJob> = {
            let st = lock_or_recover(&self.state);
            st.jobs
                .iter()
                .filter(|j| j.done.is_none())
                .filter_map(|j| {
                    j.pid.map(|pid| PersistedJob {
                        id: j.id,
                        pid,
                        command: j.command.clone(),
                        log: j.log_path.clone(),
                        started_unix: j.started_unix,
                    })
                })
                .collect()
        };
        let path = self.registry_path();
        if persisted.is_empty() {
            let _ = std::fs::remove_file(path);
        } else if let Ok(json) = serde_json::to_string(&persisted) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Spawn `command` detached with stdout+stderr to a log file. The command
    /// itself writes its exit code to `<log>.exit` (survives sirbone's death);
    /// an in-process watcher records the exit for the live case. Returns
    /// `(job id, log path)` immediately.
    pub fn spawn(&self, command: &str) -> Result<(u32, PathBuf)> {
        std::fs::create_dir_all(&self.dir).context("create jobs dir")?;
        let id = {
            let mut st = lock_or_recover(&self.state);
            st.next_id += 1;
            st.next_id
        };
        // Process id in the name keeps logs from concurrent sirbone sessions apart.
        let log_path = self.dir.join(format!("{}-{id}.log", std::process::id()));
        let log = std::fs::File::create(&log_path).context("create job log")?;
        // Exit sentinel written by the job itself, not by our watcher. The
        // command runs in a subshell `( … )` — a brace group would let an
        // `exit` inside the command skip the sentinel write.
        let wrapped = format!(
            "(\n{command}\n)\nstatus=$?\necho $status > '{}'\nexit $status",
            exit_path(&log_path).display()
        );
        let child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(wrapped)
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                log.try_clone().context("clone job log handle")?,
            ))
            .stderr(Stdio::from(log))
            .spawn()
            .context("spawn background job")?;
        let pid = child.id();
        lock_or_recover(&self.state).jobs.push(Job {
            id,
            command: command.to_string(),
            log_path: log_path.clone(),
            started: Instant::now(),
            started_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            pid,
            done: None,
            notified: false,
            progress: None,
            log_offset: 0,
        });
        self.write_registry();
        self.watch_child(id, child);
        Ok((id, log_path))
    }

    fn watch_child(&self, id: u32, mut child: tokio::process::Child) {
        let store = self.clone();
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code());
            store.mark_done(id, code);
        });
    }

    fn mark_done(&self, id: u32, code: Option<i32>) {
        {
            let mut st = lock_or_recover(&self.state);
            if let Some(j) = st.jobs.iter_mut().find(|j| j.id == id) {
                j.done = Some((code, j.started.elapsed()));
            }
        }
        self.write_registry();
    }

    /// Adopt jobs left behind by dead sirbone processes: scan the jobs dir for
    /// `registry-<pid>.json` whose owner is gone, classify each entry via the
    /// `.exit` sentinel + pid liveness, and re-watch the still-running ones.
    /// Returns how many jobs were adopted.
    pub fn restore_orphans(&self) -> usize {
        let me = std::process::id();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut adopted = 0;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(owner) = name
                .strip_prefix("registry-")
                .and_then(|s| s.strip_suffix(".json"))
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if owner == me || pid_alive(owner) {
                continue; // ours, or a live sirbone still owns it
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let jobs: Vec<PersistedJob> = serde_json::from_str(&text).unwrap_or_default();
            for pj in jobs {
                self.adopt(pj);
                adopted += 1;
            }
            let _ = std::fs::remove_file(entry.path());
        }
        if adopted > 0 {
            self.write_registry();
        }
        adopted
    }

    fn adopt(&self, pj: PersistedJob) {
        let sentinel = exit_path(&pj.log);
        let started_sys = UNIX_EPOCH + Duration::from_secs(pj.started_unix);
        let since_start = SystemTime::now()
            .duration_since(started_sys)
            .unwrap_or_default();
        let started = Instant::now()
            .checked_sub(since_start)
            .unwrap_or_else(Instant::now);

        let done = if let Some(code) = read_exit_file(&sentinel) {
            // Finished while sirbone was closed — wall time from the sentinel's mtime.
            let dur = std::fs::metadata(&sentinel)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|mt| mt.duration_since(started_sys).ok())
                .unwrap_or(since_start);
            Some((code, dur))
        } else if pid_alive(pj.pid) {
            None // still running — re-watch below
        } else {
            // Died without writing the sentinel (killed, crash, reboot).
            Some((None, since_start))
        };
        let still_running = done.is_none();

        {
            let mut st = lock_or_recover(&self.state);
            st.next_id = st.next_id.max(pj.id);
            st.jobs.push(Job {
                id: pj.id,
                command: pj.command,
                log_path: pj.log,
                started,
                started_unix: pj.started_unix,
                pid: Some(pj.pid),
                done,
                notified: false,
                progress: None,
                log_offset: 0,
            });
        }
        if still_running {
            self.watch_orphan(pj.id, pj.pid, sentinel);
        }
    }

    /// Watcher for an adopted job: we can't `wait()` on a non-child, so poll
    /// the `.exit` sentinel (and pid liveness as the fallback) once a second.
    fn watch_orphan(&self, id: u32, pid: u32, sentinel: PathBuf) {
        let store = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Some(code) = read_exit_file(&sentinel) {
                    store.mark_done(id, code);
                    break;
                }
                if !pid_alive(pid) {
                    // Grace window: the sentinel write may race process death,
                    // and the file may be partially flushed. Retry briefly before
                    // falling back to "unknown" so a successful job isn't misreported.
                    let mut code = None;
                    for _ in 0..10 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        if let Some(c) = read_exit_file(&sentinel) {
                            code = c;
                            break;
                        }
                    }
                    store.mark_done(id, code);
                    break;
                }
            }
        });
    }

    /// Jobs still running, as `(id, command, elapsed, progress)` — for the UI
    /// status line.
    pub fn running(&self) -> Vec<(u32, String, Duration, Option<f32>)> {
        lock_or_recover(&self.state)
            .jobs
            .iter()
            .filter(|j| j.done.is_none())
            .map(|j| (j.id, j.command.clone(), j.started.elapsed(), j.progress))
            .collect()
    }

    /// Scan new log output of running jobs for progress markers — `[12/80]`
    /// (cargo) or `[ 45%]` (pytest & co). Heuristic: the marker appearing last
    /// wins; multi-phase jobs (build → test) make it jump, which is faithful
    /// to the underlying output. Call throttled (~1/s): reads are incremental
    /// (per-job offset) and capped at 64 KiB.
    pub fn poll_progress(&self) {
        use std::io::{Read, Seek, SeekFrom};
        const CAP: u64 = 64 * 1024;
        let targets: Vec<(u32, PathBuf, u64)> = {
            let st = lock_or_recover(&self.state);
            st.jobs
                .iter()
                .filter(|j| j.done.is_none())
                .map(|j| (j.id, j.log_path.clone(), j.log_offset))
                .collect()
        };
        for (id, log, offset) in targets {
            let Ok(mut f) = std::fs::File::open(&log) else {
                continue;
            };
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len <= offset {
                continue;
            }
            // If the log grew beyond the cap, skip ahead — old progress is stale anyway.
            let start = offset.max(len.saturating_sub(CAP));
            if f.seek(SeekFrom::Start(start)).is_err() {
                continue;
            }
            let mut bytes = Vec::new();
            if f.take(CAP).read_to_end(&mut bytes).is_err() {
                continue;
            }
            let pct = parse_progress(&String::from_utf8_lossy(&bytes));
            let mut st = lock_or_recover(&self.state);
            if let Some(j) = st.jobs.iter_mut().find(|j| j.id == id) {
                j.log_offset = len;
                if pct.is_some() {
                    j.progress = pct;
                }
            }
        }
    }

    /// Newly finished jobs as `(id, command, exit_code, wall_time)`, marking
    /// them notified so each completion is surfaced exactly once.
    pub fn take_finished(&self) -> Vec<(u32, String, Option<i32>, Duration)> {
        let mut st = lock_or_recover(&self.state);
        st.jobs
            .iter_mut()
            .filter(|j| j.done.is_some() && !j.notified)
            .map(|j| {
                j.notified = true;
                let (code, dur) = j.done.expect("filtered on done");
                (j.id, j.command.clone(), code, dur)
            })
            .collect()
    }

    /// Human-readable report for the `job_status` tool and `/jobs`: one job
    /// (`id = Some`) or all, each with state and the tail of its log.
    pub fn report(&self, id: Option<u32>, tail_lines: usize) -> String {
        struct Snapshot {
            id: u32,
            command: String,
            log_path: PathBuf,
            elapsed: Duration,
            done: Option<(Option<i32>, Duration)>,
            progress: Option<f32>,
        }
        // Snapshot under the lock; log files are read after releasing it.
        let entries: Vec<Snapshot> = {
            let st = lock_or_recover(&self.state);
            st.jobs
                .iter()
                .filter(|j| id.is_none_or(|want| j.id == want))
                .map(|j| Snapshot {
                    id: j.id,
                    command: j.command.clone(),
                    log_path: j.log_path.clone(),
                    elapsed: j.started.elapsed(),
                    done: j.done,
                    progress: j.progress,
                })
                .collect()
        };
        if entries.is_empty() {
            return match id {
                Some(n) => format!("no job #{n}"),
                None => "no background jobs in this session".to_string(),
            };
        }
        let mut out = String::new();
        for Snapshot {
            id: jid,
            command,
            log_path,
            elapsed,
            done,
            progress,
        } in entries
        {
            let state = match done {
                None => {
                    let pct = progress
                        .map(|p| format!(" · ~{:.0}%", p * 100.0))
                        .unwrap_or_default();
                    format!("running · {}s elapsed{pct}", elapsed.as_secs())
                }
                Some((Some(0), dur)) => format!("finished ok · {}s", dur.as_secs()),
                Some((code, dur)) => format!(
                    "finished with exit {} · {}s",
                    code.map_or_else(|| "?".into(), |c| c.to_string()),
                    dur.as_secs()
                ),
            };
            out.push_str(&format!("job #{jid} [{state}] {command}\n"));
            let tail = tail_of_file(&log_path, tail_lines);
            if tail.is_empty() {
                out.push_str("  (no output yet)\n");
            } else {
                for l in tail.lines() {
                    out.push_str("  ");
                    out.push_str(l);
                    out.push('\n');
                }
            }
        }
        out.trim_end().to_string()
    }
}

/// Last progress marker in `text`: `[cur/total]` fractions or `[NN%]`
/// percentages, whichever occurs later in the stream.
fn parse_progress(text: &str) -> Option<f32> {
    static FRACTION: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static PERCENT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let fraction =
        FRACTION.get_or_init(|| Regex::new(r"\[\s*(\d+)\s*/\s*(\d+)\s*\]").expect("static regex"));
    let percent = PERCENT.get_or_init(|| Regex::new(r"\[\s*(\d+)%\s*\]").expect("static regex"));

    let mut last: Option<(usize, f32)> = None;
    for caps in fraction.captures_iter(text) {
        if let (Ok(c), Ok(t)) = (caps[1].parse::<f32>(), caps[2].parse::<f32>()) {
            if t > 0.0 {
                let pos = caps.get(0).map_or(0, |m| m.start());
                last = Some((pos, (c / t).clamp(0.0, 1.0)));
            }
        }
    }
    for caps in percent.captures_iter(text) {
        if let Ok(p) = caps[1].parse::<f32>() {
            let pos = caps.get(0).map_or(0, |m| m.start());
            if last.is_none_or(|(lp, _)| pos > lp) {
                last = Some((pos, (p / 100.0).clamp(0.0, 1.0)));
            }
        }
    }
    last.map(|(_, p)| p)
}

fn tail_of_file(path: &Path, n: usize) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn default_tail() -> usize {
    20
}

#[derive(Deserialize, JsonSchema)]
pub struct JobStatusInput {
    /// Job id to inspect; omit to list every job of this session.
    pub id: Option<u32>,
    /// Trailing log lines to include per job (default 20).
    #[serde(default = "default_tail")]
    pub tail_lines: usize,
}

pub struct JobStatusTool {
    pub jobs: JobStore,
}

#[async_trait]
impl TypedTool for JobStatusTool {
    type Input = JobStatusInput;

    fn name(&self) -> &'static str {
        "job_status"
    }

    fn description(&self) -> &'static str {
        "Check background jobs started with bash background:true — state \
         (running / finished + exit code), elapsed time, and the tail of the \
         log. Don't busy-wait on a job: do other work or end the turn; the \
         user is notified automatically when it finishes."
    }

    async fn run(&self, input: JobStatusInput) -> Result<String> {
        Ok(self.jobs.report(input.id, input.tail_lines))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, JobStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::with_dir(dir.path().to_path_buf());
        (dir, store)
    }

    async fn wait_done(store: &JobStore) {
        for _ in 0..200 {
            if store.running().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job did not finish in time");
    }

    #[tokio::test]
    async fn spawn_runs_and_reports_completion() {
        let (_dir, store) = temp_store();
        let (id, log) = store.spawn("echo out; echo err >&2; exit 3").unwrap();
        assert_eq!(id, 1);
        wait_done(&store).await;

        let finished = store.take_finished();
        assert_eq!(finished.len(), 1);
        let (fid, cmd, code, _) = &finished[0];
        assert_eq!(*fid, id);
        assert!(cmd.starts_with("echo out"));
        assert_eq!(*code, Some(3));
        // Notified exactly once.
        assert!(store.take_finished().is_empty());

        let report = store.report(Some(id), 20);
        assert!(report.contains("exit 3"), "report: {report}");
        assert!(report.contains("out"), "log tail in report: {report}");
        assert!(report.contains("err"), "stderr in same log: {report}");
        // The job itself wrote the exit sentinel.
        assert_eq!(read_exit_file(&exit_path(&log)), Some(Some(3)));
    }

    #[tokio::test]
    async fn registry_mirrors_running_jobs_and_clears_when_done() {
        let (_dir, store) = temp_store();
        store.spawn("sleep 5").unwrap();
        let reg = store.registry_path();
        let text = std::fs::read_to_string(&reg).expect("registry written on spawn");
        assert!(text.contains("sleep 5"));
        store.mark_done(1, Some(0));
        assert!(!reg.exists(), "registry removed once nothing is running");
    }

    #[tokio::test]
    async fn restore_adopts_finished_and_killed_orphans() {
        let (dir, store) = temp_store();
        // Owner pid is dead (way past Linux's default pid_max) so the registry
        // is up for adoption. Job 1 finished (sentinel present), job 2 died
        // without writing it (also a dead pid).
        let log1 = dir.path().join("x-1.log");
        std::fs::write(&log1, "all good\n").unwrap();
        std::fs::write(exit_path(&log1), "0\n").unwrap();
        let log2 = dir.path().join("x-2.log");
        std::fs::write(&log2, "partial\n").unwrap();
        let persisted = serde_json::json!([
            {"id": 1, "pid": 4_009_999_991_u32, "command": "make release", "log": log1, "started_unix": 1},
            {"id": 2, "pid": 4_009_999_992_u32, "command": "make docs", "log": log2, "started_unix": 1},
        ]);
        std::fs::write(
            dir.path().join("registry-4009999990.json"),
            persisted.to_string(),
        )
        .unwrap();

        assert_eq!(store.restore_orphans(), 2);
        assert!(
            store.running().is_empty(),
            "both orphans classified as done"
        );

        let mut finished = store.take_finished();
        finished.sort_by_key(|(id, ..)| *id);
        assert_eq!(finished.len(), 2);
        assert_eq!(
            (finished[0].0, finished[0].2),
            (1, Some(0)),
            "sentinel exit code"
        );
        assert_eq!(
            (finished[1].0, finished[1].2),
            (2, None),
            "no sentinel → exit unknown"
        );
        // Adopted registry consumed; new ids continue past the adopted ones.
        assert!(!dir.path().join("registry-4009999990.json").exists());
        let (id, _) = store.spawn("true").unwrap();
        assert_eq!(id, 3);
    }

    #[tokio::test]
    async fn restore_skips_live_owners_and_own_registry() {
        let (dir, store) = temp_store();
        // Our own pid is alive → both "owner == me" and "owner alive" paths.
        let me = std::process::id();
        std::fs::write(
            dir.path().join(format!("registry-{me}.json")),
            r#"[{"id":9,"pid":1,"command":"x","log":"/tmp/x.log","started_unix":1}]"#,
        )
        .unwrap();
        assert_eq!(store.restore_orphans(), 0);
    }

    #[test]
    fn parse_progress_picks_last_marker() {
        assert_eq!(parse_progress("[ 5/100] x\n[50/100] y"), Some(0.5));
        assert_eq!(parse_progress("tests [ 45%]"), Some(0.45));
        // Whichever marker kind appears later in the stream wins.
        assert_eq!(parse_progress("[50/100] build\n[ 80%] test"), Some(0.8));
        assert_eq!(parse_progress("[ 80%] a\n[1/4] b"), Some(0.25));
        assert_eq!(parse_progress("no markers"), None);
        assert_eq!(parse_progress("[0/0] degenerate"), None);
    }

    #[tokio::test]
    async fn poll_progress_reads_log_incrementally() {
        let (_dir, store) = temp_store();
        let (_, log) = store.spawn("sleep 3").unwrap();
        std::fs::write(&log, "[10/100] compiling\n").unwrap();
        store.poll_progress();
        assert_eq!(store.running()[0].3, Some(0.1));

        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "[ 90%] almost done").unwrap();
        store.poll_progress();
        assert_eq!(store.running()[0].3, Some(0.9));

        // No new bytes → progress unchanged (offset bookkeeping works).
        store.poll_progress();
        assert_eq!(store.running()[0].3, Some(0.9));
    }

    #[tokio::test]
    async fn report_handles_unknown_and_empty() {
        let (_dir, store) = temp_store();
        assert_eq!(store.report(None, 5), "no background jobs in this session");
        assert_eq!(store.report(Some(9), 5), "no job #9");
    }

    #[tokio::test]
    async fn job_status_tool_lists_jobs() {
        let (_dir, store) = temp_store();
        store.spawn("true").unwrap();
        let tool = JobStatusTool { jobs: store };
        let out = tool
            .run(JobStatusInput {
                id: None,
                tail_lines: 5,
            })
            .await
            .unwrap();
        assert!(out.contains("job #1"), "{out}");
    }
}
