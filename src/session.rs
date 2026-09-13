use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::{ContentBlock, Message, Role};
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Clone, Serialize, Deserialize)]
// `RunTelemetry` is far wider than the other variants and is deliberately not
// boxed: one is built once per run and written straight to JSONL, so the
// indirection would buy nothing, and the flat wire shape is the one every
// already-recorded session uses.
#[allow(clippy::large_enum_variant)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Message(Message),
    ModelChange {
        model: String,
    },
    RunUsage {
        input_tokens: u64,
        cached_tokens: u64,
        peak_context_tokens: u32,
    },
    WorkspaceSnapshot {
        id: String,
        label: String,
    },
    RunStatus {
        status: String,
        reason: Option<String>,
    },
    /// Fingerprint of the cacheable request prefix at the start of a run: model,
    /// system prompt and tool schemas, the three things every turn resends
    /// unchanged and the provider's cache is keyed on.
    ///
    /// Hashes, never the text — a system prompt carries project paths and
    /// CLAUDE.md content, and the session file is a plain readable log. A hash
    /// answers the only question worth asking of it afterwards: did the prefix
    /// change between runs? A run whose `cached_tokens` collapsed with an
    /// unchanged header was a provider-side miss; one where the header moved
    /// paid for a prefix sirbone itself invalidated, which is a bug with a fix.
    RequestHeader {
        model: String,
        system_hash: String,
        tools_hash: String,
        tool_count: usize,
    },
    /// Context compaction checkpoint: the full post-compaction transcript
    /// (summary + ack + kept recent). On load it replaces everything before it.
    Compaction {
        messages: Vec<Message>,
    },
    /// Feature-attribution counters for the run that just ended. Same numbers
    /// the bench's `[usage]` line prints, persisted so real-project usage can be
    /// audited later — `SIRBONE_USAGE` only reaches stderr, which nobody keeps.
    /// Absent from sessions written before this entry existed.
    RunTelemetry {
        compaction_fired: u64,
        historia_writes: u64,
        historia_hits: u64,
        system_prompt_tokens: u64,
        #[serde(default)]
        hook_pre_runs: u64,
        #[serde(default)]
        hook_pre_denies: u64,
        #[serde(default)]
        hook_post_runs: u64,
        #[serde(default)]
        hook_post_failures: u64,
        #[serde(default)]
        hook_stop_runs: u64,
        #[serde(default)]
        hook_stop_retries: u64,
        #[serde(default)]
        hook_stop_exhausted: u64,
        #[serde(default)]
        oracle_runs: u64,
        #[serde(default)]
        oracle_failures: u64,
        #[serde(default)]
        oracle_retries: u64,
        #[serde(default)]
        oracle_rollbacks: u64,
        #[serde(default)]
        oracle_exhausted: u64,
        #[serde(default)]
        ask_user_rounds: u64,
        #[serde(default)]
        ask_user_questions: u64,
        #[serde(default)]
        verify_tool_runs: u64,
        #[serde(default)]
        spill_writes: u64,
        #[serde(default)]
        read_outlines: u64,
        #[serde(default)]
        patch_applies: u64,
        #[serde(default)]
        patch_rejects: u64,
        #[serde(default)]
        stream_rule_trips: u64,
        #[serde(default)]
        plan_contract_initialized: u64,
        #[serde(default)]
        plan_contract_updated: u64,
        #[serde(default)]
        plan_mutations_blocked: u64,
        #[serde(default)]
        tool_batches: u64,
        #[serde(default)]
        tool_calls_emitted: u64,
        #[serde(default)]
        completion_checks_fired: u64,
        #[serde(default)]
        test_file_mutations: u64,
        #[serde(default)]
        best_of_attempts: u64,
        #[serde(default)]
        best_of_selections: u64,
        #[serde(default)]
        permission_denies_policy: u64,
        #[serde(default)]
        permission_denies_user: u64,
        #[serde(default)]
        permission_denies_unattended: u64,
        #[serde(default)]
        tusk_runs: u64,
        #[serde(default)]
        tusk_edits: u64,
        #[serde(default)]
        tusk_withheld: u64,
    },
}

/// One persisted line: an entry plus the wall-clock time it was appended. The
/// timestamp is flattened alongside the entry's own fields, so a line reads as
/// `{"ts":"…","type":"message",…}`. `ts` is `None` for legacy lines written
/// before timestamps were recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Local>>,
    #[serde(flatten)]
    pub entry: SessionEntry,
}

/// Feature-attribution counters summed over a session's runs. All zero when the
/// session predates [`SessionEntry::RunTelemetry`]; `runs` distinguishes "no
/// data" (0) from "measured zero".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TelemetryTotals {
    pub runs: usize,
    pub compaction_fired: u64,
    pub historia_writes: u64,
    pub historia_hits: u64,
    /// Largest system prompt seen, not a sum: it is a per-run size, not a cost.
    pub system_prompt_tokens: u64,
    pub hook_pre_runs: u64,
    pub hook_pre_denies: u64,
    pub hook_post_runs: u64,
    pub hook_post_failures: u64,
    pub hook_stop_runs: u64,
    pub hook_stop_retries: u64,
    pub hook_stop_exhausted: u64,
    pub oracle_runs: u64,
    pub oracle_failures: u64,
    pub oracle_retries: u64,
    pub oracle_rollbacks: u64,
    pub oracle_exhausted: u64,
    pub ask_user_rounds: u64,
    pub ask_user_questions: u64,
    pub verify_tool_runs: u64,
    pub spill_writes: u64,
    pub read_outlines: u64,
    pub patch_applies: u64,
    pub patch_rejects: u64,
    pub stream_rule_trips: u64,
    pub plan_contract_initialized: u64,
    pub plan_contract_updated: u64,
    pub plan_mutations_blocked: u64,
    pub tool_batches: u64,
    pub tool_calls_emitted: u64,
    pub completion_checks_fired: u64,
    /// Successful writes to a test file (see `crate::telemetry::TEST_FILE_MUTATIONS`).
    pub test_file_mutations: u64,
    /// Agent runs and later-attempt wins under best-of-K selection (see
    /// `crate::telemetry::BEST_OF_ATTEMPTS`).
    pub best_of_attempts: u64,
    pub best_of_selections: u64,
    /// Tool calls the permission gate refused, split by who refused (see
    /// `crate::telemetry::PERMISSION_DENIES_POLICY` and its siblings).
    pub permission_denies_policy: u64,
    pub permission_denies_user: u64,
    pub permission_denies_unattended: u64,
    pub tusk_runs: u64,
    pub tusk_edits: u64,
    pub tusk_withheld: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditSummary {
    pub path: String,
    pub records: usize,
    pub started_at: Option<DateTime<Local>>,
    pub ended_at: Option<DateTime<Local>>,
    pub duration_secs: Option<i64>,
    pub first_user: Option<String>,
    pub user_messages: usize,
    /// User messages carrying actual text. Tool results also arrive as `User`
    /// messages, so `user_messages` over-counts human turns; this does not.
    /// More than one means the human came back — the cheapest available proxy
    /// for "the run did not land first time".
    pub user_turns: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_errors: usize,
    pub tools: BTreeMap<String, usize>,
    /// Chars each tool poured into the context via its results. The other half
    /// of a tool's cost: `sirbone doctor` prices its schema, this prices what it
    /// returns.
    pub result_chars_by_tool: BTreeMap<String, u64>,
    pub errors_by_tool: BTreeMap<String, usize>,
    pub changed_files: Vec<String>,
    pub model_changes: Vec<String>,
    pub total_input_tokens: u64,
    pub cached_tokens: u64,
    pub peak_context_tokens: u32,
    pub snapshots: Vec<String>,
    pub compactions: usize,
    /// Runs that resent a *different* cacheable prefix than the run before them.
    /// Each one is a cold cache the session paid for; zero means the prefix held
    /// across the whole session. Always 0 for sessions written before
    /// [`SessionEntry::RequestHeader`] existed.
    #[serde(default)]
    pub prefix_breaks: usize,
    pub final_status: Option<String>,
    pub status_reason: Option<String>,
    pub telemetry: TelemetryTotals,
}

impl AuditSummary {
    /// Share of input tokens the provider served from its prefix cache, as a
    /// whole percent.
    ///
    /// The retroactive half of the cold-cache question, and the half that costs
    /// nothing: every session already records what was sent and what was cached,
    /// so caching can be checked over real runs without paying for a probe. Read
    /// it next to `prefix_breaks` — a low ratio with a stable prefix is the
    /// provider's doing, a low ratio with breaks is sirbone's.
    pub fn cache_hit_pct(&self) -> u64 {
        match self.total_input_tokens {
            0 => 0,
            total => self.cached_tokens.min(total) * 100 / total,
        }
    }

    /// Result chars a tool returned, in tokens (same ~4 chars/token estimate the
    /// rest of the codebase uses).
    pub fn result_tokens(&self, tool: &str) -> u64 {
        self.result_chars_by_tool.get(tool).copied().unwrap_or(0) / 4
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Sir Bone Session Audit\n\n");
        out.push_str(&format!("- Session: `{}`\n", self.path));
        out.push_str(&format!("- Records: {}\n", self.records));
        if let Some(started) = self.started_at {
            out.push_str(&format!("- Started: {}\n", started.to_rfc3339()));
        }
        if let Some(ended) = self.ended_at {
            out.push_str(&format!("- Ended: {}\n", ended.to_rfc3339()));
        }
        if let Some(secs) = self.duration_secs {
            out.push_str(&format!("- Duration: {}s\n", secs.max(0)));
        }
        if let Some(status) = &self.final_status {
            match &self.status_reason {
                Some(reason) if !reason.is_empty() => {
                    out.push_str(&format!("- Final status: {status} ({reason})\n"));
                }
                _ => out.push_str(&format!("- Final status: {status}\n")),
            }
        }
        if let Some(first) = &self.first_user {
            out.push_str(&format!(
                "- First user request: {}\n",
                first.replace('\n', " ")
            ));
        }
        out.push_str(&format!(
            "- Messages: {} user, {} assistant\n",
            self.user_messages, self.assistant_messages
        ));
        out.push_str(&format!(
            "- Tool calls: {} total, {} error(s)\n",
            self.tool_calls, self.tool_errors
        ));
        if self.total_input_tokens > 0 {
            out.push_str(&format!("- Input tokens: {}\n", self.total_input_tokens));
            out.push_str(&format!(
                "- Cached tokens: {} ({}% of input)\n",
                self.cached_tokens,
                self.cache_hit_pct()
            ));
            out.push_str(&format!("- Peak context: {}\n", self.peak_context_tokens));
        }
        if self.prefix_breaks > 0 {
            out.push_str(&format!(
                "- Prefix breaks: {} (runs that could not reuse the cached prefix)\n",
                self.prefix_breaks
            ));
        }
        if !self.snapshots.is_empty() {
            out.push_str(&format!("- Snapshots: {}\n", self.snapshots.len()));
        }
        out.push_str(&format!("- Compactions: {}\n", self.compactions));

        if !self.model_changes.is_empty() {
            out.push_str("\n## Models\n\n");
            for model in &self.model_changes {
                out.push_str(&format!("- `{model}`\n"));
            }
        }
        if !self.tools.is_empty() {
            out.push_str("\n## Tools\n\n");
            out.push_str("| Tool | Calls | Errors | Result tokens |\n|---|---:|---:|---:|\n");
            for (tool, calls) in &self.tools {
                let errors = self.errors_by_tool.get(tool).copied().unwrap_or(0);
                let tokens = self.result_tokens(tool);
                out.push_str(&format!("| `{tool}` | {calls} | {errors} | {tokens} |\n"));
            }
        }
        if self.telemetry.runs > 0 {
            let t = &self.telemetry;
            out.push_str("\n## Features\n\n");
            out.push_str(&format!("- Runs recorded: {}\n", t.runs));
            out.push_str(&format!("- Compactions fired: {}\n", t.compaction_fired));
            out.push_str(&format!(
                "- Historia: {} write(s), {} hit(s)\n",
                t.historia_writes, t.historia_hits
            ));
            out.push_str(&format!(
                "- System prompt: {} tokens\n",
                t.system_prompt_tokens
            ));
            let denies = t.permission_denies_policy
                + t.permission_denies_user
                + t.permission_denies_unattended;
            if denies > 0 {
                out.push_str(&format!(
                    "- Permission denials: {denies} ({} policy, {} user, {} unattended)\n",
                    t.permission_denies_policy,
                    t.permission_denies_user,
                    t.permission_denies_unattended
                ));
            }
            out.push_str(&format!(
                "- Hooks: pre {}/{} denies, post {}/{} failures, stop {}/{} retries/{} exhausted, tusk {}/{} edits/{} withheld\n",
                t.hook_pre_runs,
                t.hook_pre_denies,
                t.hook_post_runs,
                t.hook_post_failures,
                t.hook_stop_runs,
                t.hook_stop_retries,
                t.hook_stop_exhausted,
                t.tusk_runs,
                t.tusk_edits,
                t.tusk_withheld
            ));
            out.push_str(&format!(
                "- Oracle: {} runs, {} failures, {} retries, {} rollbacks, {} exhausted; verify: {} runs\n",
                t.oracle_runs,
                t.oracle_failures,
                t.oracle_retries,
                t.oracle_rollbacks,
                t.oracle_exhausted,
                t.verify_tool_runs
            ));
            out.push_str(&format!(
                "- Ask user: {} rounds, {} questions\n",
                t.ask_user_rounds, t.ask_user_questions
            ));
            out.push_str(&format!(
                "- Plan contract: {} initialized, {} updated, {} mutations blocked\n",
                t.plan_contract_initialized, t.plan_contract_updated, t.plan_mutations_blocked
            ));
            if t.tool_batches > 0 {
                out.push_str(&format!(
                    "- Tool batches: {} turns, {} calls, mean width {:.2}\n",
                    t.tool_batches,
                    t.tool_calls_emitted,
                    t.tool_calls_emitted as f64 / t.tool_batches as f64
                ));
            }
        }
        if !self.snapshots.is_empty() {
            out.push_str("\n## Snapshots\n\n");
            for id in &self.snapshots {
                out.push_str(&format!("- `{id}`\n"));
            }
        }
        if !self.changed_files.is_empty() {
            out.push_str("\n## Changed Files\n\n");
            for path in &self.changed_files {
                out.push_str(&format!("- `{path}`\n"));
            }
        }
        out
    }
}

/// Rebuild the in-memory transcript from session entries: messages accumulate,
/// a `Compaction` checkpoint replaces everything seen so far with its snapshot.
pub fn collapse(entries: Vec<SessionEntry>) -> Vec<Message> {
    let mut out = Vec::new();
    for entry in entries {
        match entry {
            SessionEntry::Message(m) => out.push(m),
            SessionEntry::Compaction { messages } => out = messages,
            SessionEntry::ModelChange { .. } => {}
            SessionEntry::RunUsage { .. } => {}
            SessionEntry::WorkspaceSnapshot { .. } => {}
            SessionEntry::RunStatus { .. } => {}
            SessionEntry::RunTelemetry { .. } => {}
            SessionEntry::RequestHeader { .. } => {}
        }
    }
    out
}

/// Append the current feature-attribution counters. Called once per run by every
/// frontend, so real-project sessions carry the same numbers the bench prints.
pub async fn append_run_telemetry(path: &Path) -> Result<()> {
    let c = crate::telemetry::run_delta();
    append(
        path,
        &SessionEntry::RunTelemetry {
            compaction_fired: c.compaction_fired,
            historia_writes: c.historia_writes,
            historia_hits: c.historia_hits,
            system_prompt_tokens: c.system_prompt_tokens,
            hook_pre_runs: c.hook_pre_runs,
            hook_pre_denies: c.hook_pre_denies,
            hook_post_runs: c.hook_post_runs,
            hook_post_failures: c.hook_post_failures,
            hook_stop_runs: c.hook_stop_runs,
            hook_stop_retries: c.hook_stop_retries,
            hook_stop_exhausted: c.hook_stop_exhausted,
            oracle_runs: c.oracle_runs,
            oracle_failures: c.oracle_failures,
            oracle_retries: c.oracle_retries,
            oracle_rollbacks: c.oracle_rollbacks,
            oracle_exhausted: c.oracle_exhausted,
            ask_user_rounds: c.ask_user_rounds,
            ask_user_questions: c.ask_user_questions,
            verify_tool_runs: c.verify_tool_runs,
            spill_writes: c.spill_writes,
            read_outlines: c.read_outlines,
            patch_applies: c.patch_applies,
            patch_rejects: c.patch_rejects,
            stream_rule_trips: c.stream_rule_trips,
            plan_contract_initialized: c.plan_contract_initialized,
            plan_contract_updated: c.plan_contract_updated,
            plan_mutations_blocked: c.plan_mutations_blocked,
            tool_batches: c.tool_batches,
            tool_calls_emitted: c.tool_calls_emitted,
            completion_checks_fired: c.completion_checks_fired,
            test_file_mutations: c.test_file_mutations,
            best_of_attempts: c.best_of_attempts,
            best_of_selections: c.best_of_selections,
            permission_denies_policy: c.permission_denies_policy,
            permission_denies_user: c.permission_denies_user,
            permission_denies_unattended: c.permission_denies_unattended,
            tusk_runs: c.tusk_runs,
            tusk_edits: c.tusk_edits,
            tusk_withheld: c.tusk_withheld,
        },
    )
    .await
}

/// FNV-1a, hex. Not `DefaultHasher`: that one is free to change between Rust
/// releases, and a fingerprint that moves on a toolchain bump would report a
/// prefix break that never happened.
fn fingerprint(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        // Field separator: without it "read" + "grep" and a single tool named
        // "readgrep" hash alike, and a rename could pass as no change at all.
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        for byte in part.as_ref() {
            h ^= *byte as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// Record the prefix fingerprint for a run. Called once per run by each
/// frontend, before the first turn.
pub async fn append_request_header(
    path: &Path,
    model: &str,
    system_prompt: Option<&str>,
    tools: &crate::tools::ToolRegistry,
) -> Result<()> {
    // Name plus schema, in registration order — the order is what the provider
    // is sent, so a reordering is a real cache break and must show as one.
    let mut schemas: Vec<String> = Vec::new();
    for tool in tools.iter() {
        schemas.push(tool.name().to_string());
        schemas.push(tool.description().to_string());
        schemas.push(tool.schema().to_string());
    }
    append(
        path,
        &SessionEntry::RequestHeader {
            model: model.to_string(),
            system_hash: fingerprint([system_prompt.unwrap_or_default()]),
            tools_hash: fingerprint(&schemas),
            tool_count: tools.iter().count(),
        },
    )
    .await
}

pub async fn append(path: &Path, entry: &SessionEntry) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create session dir {}", parent.display()))?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("cannot open session file {}", path.display()))?;

    let record = SessionRecord {
        ts: Some(Local::now()),
        entry: entry.clone(),
    };
    let line = serde_json::to_string(&record).context("cannot serialize session entry")?;
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    Ok(())
}

pub async fn load(path: &Path) -> Result<Vec<SessionEntry>> {
    Ok(load_records(path)
        .await?
        .into_iter()
        .map(|r| r.entry)
        .collect())
}

/// Like [`load`] but keeps each line's recorded timestamp (`SessionRecord::ts`),
/// for callers that need per-message timing. `ts` is `None` on legacy lines.
pub async fn load_records(path: &Path) -> Result<Vec<SessionRecord>> {
    if !path.exists() {
        return Ok(vec![]);
    }

    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("cannot open session file {}", path.display()))?;

    let mut reader = BufReader::new(file).lines();
    let mut records = Vec::new();

    while let Some(line) = reader.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A truncated/corrupt line (the process was killed mid-append, disk full,
        // …) must not brick the whole session resume — skip it, keep the valid
        // records, and surface that something was dropped.
        match serde_json::from_str::<SessionRecord>(line) {
            Ok(record) => records.push(record),
            Err(e) => tracing::warn!("skipping corrupt session line: {e}"),
        }
    }

    Ok(records)
}

/// Per-project session directory: `~/.sirbone/projects/<slug>/sessions/` for the
/// current working directory. Scopes `/resume` and `--continue` to this project,
/// matching where the rest of the per-project state lives (meta, history, config,
/// snapshots). `--session <path>` still works with any explicit path.
pub fn sessions_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::project_store::project_dir(&cwd).join("sessions")
}

pub fn new_session_path() -> PathBuf {
    sessions_dir().join(format!("{}.jsonl", uuid::Uuid::new_v4()))
}

/// A previous conversation on disk, for `/resume`.
pub struct SessionInfo {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub preview: String, // first user message, truncated
}

/// List saved sessions, most-recent-first, each with a short preview.
/// Empty conversations (no user text) are skipped.
pub async fn list_sessions() -> Vec<SessionInfo> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(sessions_dir()).await else {
        return out;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(preview) = first_user_text(&path).await else {
            continue;
        };
        let modified = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(UNIX_EPOCH);
        out.push(SessionInfo {
            path,
            modified,
            preview,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// Most-recently-modified non-empty session in the sessions dir, if any.
/// Backs `--continue`: resume the last conversation without naming a UUID.
pub async fn latest_session_path() -> Option<PathBuf> {
    list_sessions().await.into_iter().next().map(|s| s.path)
}

pub async fn audit(path: &Path) -> Result<AuditSummary> {
    let records = load_records(path).await?;
    Ok(audit_records(path, &records))
}

fn audit_records(path: &Path, records: &[SessionRecord]) -> AuditSummary {
    let started_at = records.iter().find_map(|r| r.ts);
    let ended_at = records.iter().rev().find_map(|r| r.ts);
    let duration_secs = started_at.zip(ended_at).map(|(s, e)| (e - s).num_seconds());

    let mut first_user = None;
    let mut user_messages = 0;
    let mut user_turns = 0;
    let mut assistant_messages = 0;
    let mut tool_calls = 0;
    let mut tool_errors = 0;
    let mut tools = BTreeMap::new();
    let mut result_chars_by_tool: BTreeMap<String, u64> = BTreeMap::new();
    let mut errors_by_tool = BTreeMap::new();
    let mut telemetry = TelemetryTotals::default();
    let mut changed_files = BTreeSet::new();
    let mut model_changes = Vec::new();
    let mut total_input_tokens = 0;
    let mut cached_tokens = 0;
    let mut peak_context_tokens = 0;
    let mut snapshots = Vec::new();
    let mut compactions = 0;
    let mut prefix: Option<(String, String, String)> = None;
    let mut prefix_breaks = 0;
    let mut final_status = None;
    let mut status_reason = None;
    let mut id_to_tool = HashMap::new();
    let mut seen_tool_calls = HashSet::new();
    let mut seen_tool_results = HashSet::new();

    // Compaction checkpoints can be the only persisted copy of messages
    // produced during the run that triggered them. Tool ids make it possible
    // to merge those messages with ordinary records without double-counting.
    let mut take_uses = |m: &Message,
                         tool_calls: &mut usize,
                         id_to_tool: &mut HashMap<String, String>,
                         tools: &mut BTreeMap<String, usize>,
                         changed_files: &mut BTreeSet<String>| {
        for block in &m.content {
            let ContentBlock::ToolUse { id, name, input } = block else {
                continue;
            };
            if !seen_tool_calls.insert(id.clone()) {
                continue;
            }
            *tool_calls += 1;
            id_to_tool.insert(id.clone(), name.clone());
            *tools.entry(name.clone()).or_insert(0) += 1;
            if let Some(path) = mutation_path_from_tool_use(name, input) {
                changed_files.insert(path);
            }
        }
    };

    // Tool results arrive under `User` on the Anthropic path and under `Tool`
    // elsewhere; both need the same accounting.
    let mut take_results =
        |m: &Message,
         id_to_tool: &HashMap<String, String>,
         tool_errors: &mut usize,
         errors_by_tool: &mut BTreeMap<String, usize>| {
            for block in &m.content {
                let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = block
                else {
                    continue;
                };
                if !seen_tool_results.insert(tool_use_id.clone()) {
                    continue;
                }
                let tool = id_to_tool
                    .get(tool_use_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown".into());
                *result_chars_by_tool.entry(tool.clone()).or_insert(0) +=
                    content.chars().count() as u64;
                if *is_error {
                    *tool_errors += 1;
                    *errors_by_tool.entry(tool).or_insert(0) += 1;
                }
            }
        };

    for record in records {
        match &record.entry {
            SessionEntry::Message(m) => match &m.role {
                Role::User => {
                    user_messages += 1;
                    let text = message_text(m);
                    if text.is_some() {
                        user_turns += 1;
                    }
                    if first_user.is_none() {
                        first_user = text.map(|t| truncate_chars(&t.replace('\n', " "), 140));
                    }
                    take_results(m, &id_to_tool, &mut tool_errors, &mut errors_by_tool);
                }
                Role::Assistant => {
                    assistant_messages += 1;
                    take_uses(
                        m,
                        &mut tool_calls,
                        &mut id_to_tool,
                        &mut tools,
                        &mut changed_files,
                    );
                }
                Role::Tool => {
                    take_results(m, &id_to_tool, &mut tool_errors, &mut errors_by_tool);
                }
                Role::System => {}
            },
            SessionEntry::ModelChange { model } => model_changes.push(model.clone()),
            SessionEntry::RunUsage {
                input_tokens,
                cached_tokens: cached,
                peak_context_tokens: peak,
            } => {
                total_input_tokens += *input_tokens;
                cached_tokens += *cached;
                peak_context_tokens = peak_context_tokens.max(*peak);
            }
            SessionEntry::RequestHeader {
                model,
                system_hash,
                tools_hash,
                ..
            } => {
                let now = (model.clone(), system_hash.clone(), tools_hash.clone());
                if prefix.as_ref().is_some_and(|before| *before != now) {
                    prefix_breaks += 1;
                }
                prefix = Some(now);
            }
            SessionEntry::WorkspaceSnapshot { id, .. } => snapshots.push(id.clone()),
            SessionEntry::RunStatus { status, reason } => {
                final_status = Some(status.clone());
                status_reason = reason.clone();
            }
            SessionEntry::Compaction { messages } => {
                compactions += 1;
                // Do not count checkpoint roles as conversation messages: the
                // snapshot contains synthetic summary/ack messages and may
                // repeat retained history. Tool ids, unlike roles, are stable.
                for message in messages {
                    match message.role {
                        Role::Assistant => take_uses(
                            message,
                            &mut tool_calls,
                            &mut id_to_tool,
                            &mut tools,
                            &mut changed_files,
                        ),
                        Role::User | Role::Tool => take_results(
                            message,
                            &id_to_tool,
                            &mut tool_errors,
                            &mut errors_by_tool,
                        ),
                        Role::System => {}
                    }
                }
            }
            SessionEntry::RunTelemetry {
                compaction_fired,
                historia_writes,
                historia_hits,
                system_prompt_tokens,
                hook_pre_runs,
                hook_pre_denies,
                hook_post_runs,
                hook_post_failures,
                hook_stop_runs,
                hook_stop_retries,
                hook_stop_exhausted,
                oracle_runs,
                oracle_failures,
                oracle_retries,
                oracle_rollbacks,
                oracle_exhausted,
                ask_user_rounds,
                ask_user_questions,
                verify_tool_runs,
                spill_writes,
                read_outlines,
                patch_applies,
                patch_rejects,
                stream_rule_trips,
                plan_contract_initialized,
                plan_contract_updated,
                plan_mutations_blocked,
                tool_batches,
                tool_calls_emitted,
                completion_checks_fired,
                test_file_mutations,
                best_of_attempts,
                best_of_selections,
                permission_denies_policy,
                permission_denies_user,
                permission_denies_unattended,
                tusk_runs,
                tusk_edits,
                tusk_withheld,
            } => {
                telemetry.runs += 1;
                telemetry.compaction_fired += compaction_fired;
                telemetry.historia_writes += historia_writes;
                telemetry.historia_hits += historia_hits;
                telemetry.system_prompt_tokens =
                    telemetry.system_prompt_tokens.max(*system_prompt_tokens);
                telemetry.hook_pre_runs += hook_pre_runs;
                telemetry.hook_pre_denies += hook_pre_denies;
                telemetry.hook_post_runs += hook_post_runs;
                telemetry.hook_post_failures += hook_post_failures;
                telemetry.hook_stop_runs += hook_stop_runs;
                telemetry.hook_stop_retries += hook_stop_retries;
                telemetry.hook_stop_exhausted += hook_stop_exhausted;
                telemetry.oracle_runs += oracle_runs;
                telemetry.oracle_failures += oracle_failures;
                telemetry.oracle_retries += oracle_retries;
                telemetry.oracle_rollbacks += oracle_rollbacks;
                telemetry.oracle_exhausted += oracle_exhausted;
                telemetry.ask_user_rounds += ask_user_rounds;
                telemetry.ask_user_questions += ask_user_questions;
                telemetry.verify_tool_runs += verify_tool_runs;
                telemetry.spill_writes += spill_writes;
                telemetry.read_outlines += read_outlines;
                telemetry.patch_applies += patch_applies;
                telemetry.patch_rejects += patch_rejects;
                telemetry.stream_rule_trips += stream_rule_trips;
                telemetry.plan_contract_initialized += plan_contract_initialized;
                telemetry.plan_contract_updated += plan_contract_updated;
                telemetry.plan_mutations_blocked += plan_mutations_blocked;
                telemetry.tool_batches += tool_batches;
                telemetry.tool_calls_emitted += tool_calls_emitted;
                telemetry.completion_checks_fired += completion_checks_fired;
                telemetry.test_file_mutations += test_file_mutations;
                telemetry.best_of_attempts += best_of_attempts;
                telemetry.best_of_selections += best_of_selections;
                telemetry.permission_denies_policy += permission_denies_policy;
                telemetry.permission_denies_user += permission_denies_user;
                telemetry.permission_denies_unattended += permission_denies_unattended;
                telemetry.tusk_runs += tusk_runs;
                telemetry.tusk_edits += tusk_edits;
                telemetry.tusk_withheld += tusk_withheld;
            }
        }
    }

    AuditSummary {
        path: path.display().to_string(),
        records: records.len(),
        started_at,
        ended_at,
        duration_secs,
        first_user,
        user_messages,
        user_turns,
        assistant_messages,
        tool_calls,
        tool_errors,
        tools,
        result_chars_by_tool,
        errors_by_tool,
        changed_files: changed_files.into_iter().collect(),
        model_changes,
        total_input_tokens,
        cached_tokens,
        peak_context_tokens,
        snapshots,
        compactions,
        prefix_breaks,
        final_status,
        status_reason,
        telemetry,
    }
}

fn message_text(m: &Message) -> Option<String> {
    let text = m
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

fn mutation_path_from_tool_use(name: &str, input: &serde_json::Value) -> Option<String> {
    match name {
        "write" | "edit" | "sed" | "undo" => input
            .get("path")
            .or_else(|| input.get("file_path"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => None,
    }
}

/// Fork-lite: snapshot `src` into a fresh session file and return its path.
/// Subsequent appends go to the copy; `src` is left frozen at the fork point.
/// A flat duplicate — no branch tree, just a divergence point you can keep.
pub async fn fork(src: &Path) -> Result<PathBuf> {
    let dst = new_session_path();
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create session dir {}", parent.display()))?;
    }
    if src.exists() {
        tokio::fs::copy(src, &dst)
            .await
            .with_context(|| format!("cannot fork {} -> {}", src.display(), dst.display()))?;
    }
    Ok(dst)
}

async fn first_user_text(path: &Path) -> Option<String> {
    for entry in load(path).await.ok()? {
        if let SessionEntry::Message(m) = entry {
            if m.role == Role::User {
                let mut imgs = 0;
                for b in &m.content {
                    match b {
                        ContentBlock::Text { text } => {
                            let t = text.trim();
                            if !t.is_empty() {
                                return Some(t.chars().take(70).collect());
                            }
                        }
                        ContentBlock::Image { .. } => imgs += 1,
                        _ => {}
                    }
                }
                // Image-only first turn (`--image x.png` with no text): show a
                // descriptive placeholder instead of skipping to an empty preview.
                if imgs > 0 {
                    return Some(format!("[image: {imgs} attachment(s)]"));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Role};

    #[test]
    fn legacy_run_telemetry_ignores_removed_fields_and_defaults_new_counters() {
        let record: SessionRecord = serde_json::from_str(
            r#"{"type":"run_telemetry","compaction_fired":0,"historia_writes":0,"historia_hits":0,"playbook_injected":0,"playbook_applied":0,"system_prompt_tokens":321}"#,
        )
        .unwrap();

        let serialized = serde_json::to_string(&record).unwrap();
        assert!(!serialized.contains("playbook"));

        let SessionEntry::RunTelemetry {
            plan_contract_initialized,
            plan_contract_updated,
            plan_mutations_blocked,
            ..
        } = record.entry
        else {
            panic!("expected run telemetry");
        };
        assert_eq!(plan_contract_initialized, 0);
        assert_eq!(plan_contract_updated, 0);
        assert_eq!(plan_mutations_blocked, 0);
    }

    fn msg(text: &str, role: Role) -> Message {
        Message {
            role,
            injected: false,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[tokio::test]
    async fn append_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        let entries = vec![
            SessionEntry::Message(msg("hi", Role::User)),
            SessionEntry::Message(msg("hello", Role::Assistant)),
            SessionEntry::ModelChange {
                model: "gpt-4o".into(),
            },
        ];

        for e in &entries {
            append(&path, e).await.unwrap();
        }

        let loaded = load(&path).await.unwrap();
        assert_eq!(loaded.len(), 3);

        match &loaded[0] {
            SessionEntry::Message(m) => {
                assert_eq!(m.role, Role::User);
            }
            _ => panic!("expected Message"),
        }
        match &loaded[2] {
            SessionEntry::ModelChange { model } => assert_eq!(model, "gpt-4o"),
            _ => panic!("expected ModelChange"),
        }
    }

    #[tokio::test]
    async fn first_user_text_placeholders_image_only_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let m = Message {
            role: Role::User,
            injected: false,
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "x".into(),
            }],
        };
        append(&path, &SessionEntry::Message(m)).await.unwrap();
        assert_eq!(
            first_user_text(&path).await.as_deref(),
            Some("[image: 1 attachment(s)]")
        );
    }

    #[tokio::test]
    async fn append_stamps_ts_and_legacy_lines_have_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        // A legacy line (no `ts`) written by hand parses with ts = None.
        tokio::fs::write(&path, "{\"type\":\"model_change\",\"model\":\"old\"}\n")
            .await
            .unwrap();
        // A freshly appended line carries a timestamp.
        let before = Local::now();
        append(&path, &SessionEntry::Message(msg("hi", Role::User)))
            .await
            .unwrap();

        let recs = load_records(&path).await.unwrap();
        assert_eq!(recs.len(), 2);
        assert!(recs[0].ts.is_none(), "legacy line should have no ts");
        let ts = recs[1].ts.expect("appended line should be stamped");
        assert!(ts >= before, "ts should be at/after the append call");
        assert!(matches!(recs[1].entry, SessionEntry::Message(_)));
        // load() still drops timestamps and returns plain entries.
        assert_eq!(load(&path).await.unwrap().len(), 2);
    }

    #[test]
    fn audit_records_counts_tools_errors_and_changed_files() {
        let records = vec![
            SessionRecord {
                ts: None,
                entry: SessionEntry::Message(Message::user("fix it")),
            },
            SessionRecord {
                ts: None,
                entry: SessionEntry::Message(Message::assistant_with_tools(vec![
                    crate::types::ToolCall {
                        id: "a".into(),
                        name: "edit".into(),
                        arguments: serde_json::json!({ "path": "src/lib.rs" }),
                    },
                    crate::types::ToolCall {
                        id: "b".into(),
                        name: "grep".into(),
                        arguments: serde_json::json!({ "pattern": "foo" }),
                    },
                ])),
            },
            SessionRecord {
                ts: None,
                entry: SessionEntry::RunUsage {
                    input_tokens: 100,
                    cached_tokens: 25,
                    peak_context_tokens: 90,
                },
            },
            SessionRecord {
                ts: None,
                entry: SessionEntry::WorkspaceSnapshot {
                    id: "abcdef".into(),
                    label: "fix it".into(),
                },
            },
            SessionRecord {
                ts: None,
                entry: SessionEntry::RunStatus {
                    status: "done".into(),
                    reason: None,
                },
            },
            SessionRecord {
                ts: None,
                entry: SessionEntry::Message(Message {
                    role: Role::User,
                    injected: false,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "a".into(),
                            content: "ok".into(),
                            is_error: false,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "b".into(),
                            content: "no match".into(),
                            is_error: true,
                        },
                    ],
                }),
            },
        ];

        let audit = audit_records(Path::new("/tmp/session.jsonl"), &records);
        assert_eq!(audit.user_messages, 2);
        assert_eq!(audit.assistant_messages, 1);
        assert_eq!(audit.tool_calls, 2);
        assert_eq!(audit.tool_errors, 1);
        assert_eq!(audit.tools["edit"], 1);
        assert_eq!(audit.tools["grep"], 1);
        assert_eq!(audit.errors_by_tool["grep"], 1);
        assert_eq!(audit.changed_files, vec!["src/lib.rs"]);
        assert_eq!(audit.first_user.as_deref(), Some("fix it"));
        assert_eq!(audit.total_input_tokens, 100);
        assert_eq!(audit.cached_tokens, 25);
        assert_eq!(audit.peak_context_tokens, 90);
        assert_eq!(audit.snapshots, vec!["abcdef"]);
        assert_eq!(audit.final_status.as_deref(), Some("done"));
        assert_eq!(audit.status_reason, None);
    }

    #[test]
    fn the_audit_counts_a_prefix_break_only_when_the_prefix_actually_moved() {
        let header = |model: &str, sys: &str| SessionRecord {
            ts: None,
            entry: SessionEntry::RequestHeader {
                model: model.into(),
                system_hash: sys.into(),
                tools_hash: "t".into(),
                tool_count: 3,
            },
        };
        let repeated = vec![header("m", "s"), header("m", "s"), header("m", "s")];
        assert_eq!(
            audit_records(Path::new("/tmp/s.jsonl"), &repeated).prefix_breaks,
            0
        );

        // A different system prompt (a CLAUDE.md edit, a new project) and a model
        // switch are both cold caches the next run pays for.
        let moved = vec![header("m", "s"), header("m", "s2"), header("m2", "s2")];
        assert_eq!(
            audit_records(Path::new("/tmp/s.jsonl"), &moved).prefix_breaks,
            2
        );
    }

    #[test]
    fn the_cache_hit_ratio_reads_what_the_session_already_recorded() {
        let records = vec![
            SessionRecord {
                ts: None,
                entry: SessionEntry::RunUsage {
                    input_tokens: 1_000,
                    cached_tokens: 750,
                    peak_context_tokens: 0,
                },
            },
            SessionRecord {
                ts: None,
                entry: SessionEntry::RunUsage {
                    input_tokens: 1_000,
                    cached_tokens: 250,
                    peak_context_tokens: 0,
                },
            },
        ];
        let audit = audit_records(Path::new("/tmp/s.jsonl"), &records);
        assert_eq!(audit.cache_hit_pct(), 50);
        assert!(audit
            .to_markdown()
            .contains("Cached tokens: 1000 (50% of input)"));

        // A session with no usage recorded reports 0, never a division by zero.
        assert_eq!(
            audit_records(Path::new("/tmp/s.jsonl"), &[]).cache_hit_pct(),
            0
        );
    }

    #[test]
    fn a_prefix_fingerprint_is_stable_and_separates_different_prefixes() {
        assert_eq!(fingerprint(["abc"]), fingerprint(["abc"]));
        assert_ne!(fingerprint(["abc"]), fingerprint(["abd"]));
        // Field boundaries matter: two tools must not hash like one renamed tool.
        assert_ne!(fingerprint(["read", "grep"]), fingerprint(["readgrep"]));
    }

    #[test]
    fn audit_records_counts_tools_stored_only_in_compaction_once() {
        let tool = |id: &str, name: &str, path: &str| crate::types::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({ "path": path }),
        };
        let result = |id: &str, is_error: bool| ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: "result".into(),
            is_error,
        };
        let records = vec![
            SessionRecord {
                ts: None,
                entry: SessionEntry::Message(Message::assistant_with_tools(vec![tool(
                    "a",
                    "read",
                    "README.md",
                )])),
            },
            SessionRecord {
                ts: None,
                entry: SessionEntry::Compaction {
                    messages: vec![
                        // Retained history may repeat an already persisted call.
                        Message::assistant_with_tools(vec![
                            tool("a", "read", "README.md"),
                            tool("b", "edit", "src/lib.rs"),
                        ]),
                        Message {
                            role: Role::Tool,
                            injected: false,
                            content: vec![result("a", false), result("b", true)],
                        },
                    ],
                },
            },
            SessionRecord {
                ts: None,
                entry: SessionEntry::Message(Message {
                    role: Role::Tool,
                    injected: false,
                    // A retained result repeated after the checkpoint is also
                    // counted once.
                    content: vec![result("b", true)],
                }),
            },
        ];

        let audit = audit_records(Path::new("/tmp/session.jsonl"), &records);
        assert_eq!(audit.tool_calls, 2);
        assert_eq!(audit.tool_errors, 1);
        assert_eq!(audit.tools["read"], 1);
        assert_eq!(audit.tools["edit"], 1);
        assert_eq!(audit.changed_files, vec!["src/lib.rs"]);
    }

    #[test]
    fn audit_records_reads_final_status() {
        let records = vec![
            SessionRecord {
                ts: None,
                entry: SessionEntry::RunStatus {
                    status: "error".into(),
                    reason: Some("model overloaded".into()),
                },
            },
            SessionRecord {
                ts: None,
                entry: SessionEntry::RunStatus {
                    status: "done".into(),
                    reason: None,
                },
            },
        ];

        let audit = audit_records(Path::new("/tmp/session.jsonl"), &records);
        assert_eq!(audit.final_status.as_deref(), Some("done"));
        assert_eq!(audit.status_reason, None);
    }

    #[tokio::test]
    async fn load_nonexistent_returns_empty() {
        let path = PathBuf::from("/tmp/nonexistent_session_xyz.jsonl");
        let entries = load(&path).await.unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn new_session_path_is_jsonl() {
        let p = new_session_path();
        assert!(p.to_str().unwrap().ends_with(".jsonl"));
    }

    #[test]
    fn collapse_replaces_prefix_at_compaction_checkpoint() {
        let entries = vec![
            SessionEntry::Message(msg("old 1", Role::User)),
            SessionEntry::Message(msg("old 2", Role::Assistant)),
            SessionEntry::ModelChange { model: "m".into() },
            SessionEntry::Compaction {
                messages: vec![msg("[summary]", Role::User), msg("ack", Role::Assistant)],
            },
            SessionEntry::Message(msg("after", Role::User)),
        ];
        let out = collapse(entries);
        let texts: Vec<String> = out
            .iter()
            .map(|m| crate::types::extract_text(&m.content))
            .collect();
        assert_eq!(texts, ["[summary]", "ack", "after"]);
    }

    #[test]
    fn collapse_without_checkpoint_keeps_all_messages() {
        let entries = vec![
            SessionEntry::Message(msg("a", Role::User)),
            SessionEntry::ModelChange { model: "m".into() },
            SessionEntry::Message(msg("b", Role::Assistant)),
        ];
        assert_eq!(collapse(entries).len(), 2);
    }

    #[tokio::test]
    async fn compaction_entry_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let entry = SessionEntry::Compaction {
            messages: vec![msg("sum", Role::User)],
        };
        append(&path, &entry).await.unwrap();
        match &load(&path).await.unwrap()[0] {
            SessionEntry::Compaction { messages } => assert_eq!(messages.len(), 1),
            other => panic!("expected Compaction, got {other:?}"),
        }
    }
}
