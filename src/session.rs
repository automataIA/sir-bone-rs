use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::{ContentBlock, Message, Role};
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Context compaction checkpoint: the full post-compaction transcript
    /// (summary + ack + kept recent). On load it replaces everything before it.
    Compaction {
        messages: Vec<Message>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditSummary {
    pub path: String,
    pub records: usize,
    pub started_at: Option<DateTime<Local>>,
    pub ended_at: Option<DateTime<Local>>,
    pub duration_secs: Option<i64>,
    pub first_user: Option<String>,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_errors: usize,
    pub tools: BTreeMap<String, usize>,
    pub errors_by_tool: BTreeMap<String, usize>,
    pub changed_files: Vec<String>,
    pub model_changes: Vec<String>,
    pub total_input_tokens: u64,
    pub cached_tokens: u64,
    pub peak_context_tokens: u32,
    pub snapshots: Vec<String>,
    pub compactions: usize,
    pub final_status: Option<String>,
    pub status_reason: Option<String>,
}

impl AuditSummary {
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
            out.push_str(&format!("- Cached tokens: {}\n", self.cached_tokens));
            out.push_str(&format!("- Peak context: {}\n", self.peak_context_tokens));
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
            out.push_str("| Tool | Calls | Errors |\n|---|---:|---:|\n");
            for (tool, calls) in &self.tools {
                let errors = self.errors_by_tool.get(tool).copied().unwrap_or(0);
                out.push_str(&format!("| `{tool}` | {calls} | {errors} |\n"));
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
        }
    }
    out
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
    let mut assistant_messages = 0;
    let mut tool_calls = 0;
    let mut tool_errors = 0;
    let mut tools = BTreeMap::new();
    let mut errors_by_tool = BTreeMap::new();
    let mut changed_files = BTreeSet::new();
    let mut model_changes = Vec::new();
    let mut total_input_tokens = 0;
    let mut cached_tokens = 0;
    let mut peak_context_tokens = 0;
    let mut snapshots = Vec::new();
    let mut compactions = 0;
    let mut final_status = None;
    let mut status_reason = None;
    let mut id_to_tool = HashMap::new();

    for record in records {
        match &record.entry {
            SessionEntry::Message(m) => match &m.role {
                Role::User => {
                    user_messages += 1;
                    if first_user.is_none() {
                        first_user =
                            message_text(m).map(|t| truncate_chars(&t.replace('\n', " "), 140));
                    }
                    for block in &m.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            is_error,
                            ..
                        } = block
                        {
                            if *is_error {
                                tool_errors += 1;
                                let tool = id_to_tool
                                    .get(tool_use_id)
                                    .cloned()
                                    .unwrap_or_else(|| "unknown".into());
                                *errors_by_tool.entry(tool).or_insert(0) += 1;
                            }
                        }
                    }
                }
                Role::Assistant => {
                    assistant_messages += 1;
                    for block in &m.content {
                        if let ContentBlock::ToolUse { id, name, input } = block {
                            tool_calls += 1;
                            id_to_tool.insert(id.clone(), name.clone());
                            *tools.entry(name.clone()).or_insert(0) += 1;
                            if let Some(path) = mutation_path_from_tool_use(name, input) {
                                changed_files.insert(path);
                            }
                        }
                    }
                }
                Role::Tool => {
                    for block in &m.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            is_error,
                            ..
                        } = block
                        {
                            if *is_error {
                                tool_errors += 1;
                                let tool = id_to_tool
                                    .get(tool_use_id)
                                    .cloned()
                                    .unwrap_or_else(|| "unknown".into());
                                *errors_by_tool.entry(tool).or_insert(0) += 1;
                            }
                        }
                    }
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
            SessionEntry::WorkspaceSnapshot { id, .. } => snapshots.push(id.clone()),
            SessionEntry::RunStatus { status, reason } => {
                final_status = Some(status.clone());
                status_reason = reason.clone();
            }
            SessionEntry::Compaction { .. } => compactions += 1,
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
        assistant_messages,
        tool_calls,
        tool_errors,
        tools,
        errors_by_tool,
        changed_files: changed_files.into_iter().collect(),
        model_changes,
        total_input_tokens,
        cached_tokens,
        peak_context_tokens,
        snapshots,
        compactions,
        final_status,
        status_reason,
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

    fn msg(text: &str, role: Role) -> Message {
        Message {
            role,
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
