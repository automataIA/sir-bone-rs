use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{truncate_output, TypedTool};
use crate::session::{self, SessionEntry, SessionRecord};
use crate::types::{ContentBlock, Message, Role};

const DEFAULT_SESSION_LIMIT: usize = 6;
const MAX_SESSION_LIMIT: usize = 20;
const MAX_EXCERPT_CHARS: usize = 2_400;
const MAX_OUTPUT_BYTES: usize = 32_000;
const MAX_OUTPUT_LINES: usize = 500;

fn default_session_limit() -> usize {
    DEFAULT_SESSION_LIMIT
}

/// Build the user turn used by `/historia` after the deterministic JSONL lookup.
/// The retrieved text is supplied directly so continuation does not depend on
/// the model deciding whether to call the tool.
pub fn continuation_prompt(query: &str, history: &str) -> String {
    let scope = if query.trim().is_empty() {
        "the latest relevant project history"
    } else {
        query.trim()
    };
    format!(
        "The user explicitly invoked `/historia` to continue previous project work.\n\
         Retrieval scope: {scope}\n\n\
         The following context was reconstructed deterministically from the project's persisted JSONL chat sessions:\n\
         <historia>\n{history}\n</historia>\n\n\
         Before acting, identify what was completed, the active or unfinished plan, prior failures and their causes, and the solutions that worked. \
         Inspect the current workspace before assuming historical state is still current. Then continue the work from that state, reuse validated solutions, \
         and do not repeat failed attempts. Do not merely summarize unless continuation is impossible or the user explicitly requested a summary."
    )
}

#[derive(Deserialize, JsonSchema)]
pub struct HistoriaInput {
    /// Words or topic to find. Leave empty for the latest project state.
    #[serde(default)]
    pub query: String,
    /// Maximum matching sessions to return (1-20, default 6).
    #[serde(default = "default_session_limit")]
    pub max_sessions: usize,
    /// Restrict matching to a JSONL field family; `all` uses weighted fields.
    #[serde(default)]
    pub focus: HistoriaFocus,
}

#[derive(Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoriaFocus {
    #[default]
    All,
    Requests,
    Assistant,
    Plans,
    Files,
    Problems,
    Status,
}

/// Read-only, deterministic project memory reconstructed from persisted chats.
pub struct HistoriaTool {
    pub project: PathBuf,
}

#[derive(Default)]
struct SessionMemory {
    id: String,
    timestamp: Option<DateTime<Local>>,
    status: Option<String>,
    requests: Vec<String>,
    assistant_record: Vec<String>,
    plans: Vec<String>,
    changed_files: Vec<String>,
    errors: Vec<String>,
}

impl SessionMemory {
    fn is_empty(&self) -> bool {
        self.requests.is_empty()
            && self.assistant_record.is_empty()
            && self.plans.is_empty()
            && self.changed_files.is_empty()
            && self.errors.is_empty()
    }

    #[cfg(test)]
    fn searchable_text(&self) -> String {
        self.requests
            .iter()
            .chain(&self.assistant_record)
            .chain(&self.plans)
            .chain(&self.changed_files)
            .chain(&self.errors)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase()
    }
}

#[async_trait]
impl TypedTool for HistoriaTool {
    type Input = HistoriaInput;

    fn name(&self) -> &'static str {
        "historia"
    }

    fn description(&self) -> &'static str {
        "Search the project's persisted chat history and reconstruct prior requests, \
         assistant reasoning/outcomes, plans, changed files, failures, and run status. \
         Use it when the user asks what happened, why a choice was made, the current \
         project state, or to resume earlier work. `query` may be empty for recent \
         state; `focus` can target requests, assistant answers, plans, files, problems, \
         or status, and `max_sessions` is 1-20. Read-only and fully deterministic: it never \
         asks an LLM to author or summarize project memory."
    }

    async fn run(&self, input: HistoriaInput) -> Result<String> {
        let dir = crate::project_store::project_dir(&self.project).join("sessions");
        let output = render_history(&dir, &input).await?;
        if !output.memories.is_empty() {
            crate::telemetry::add(&crate::telemetry::HISTORIA_HITS, 1);
        }
        Ok(output.text)
    }
}

struct HistoryOutput {
    text: String,
    memories: Vec<SessionMemory>,
}

async fn render_history(dir: &Path, input: &HistoriaInput) -> Result<HistoryOutput> {
    let mut paths = Vec::new();
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HistoryOutput {
                text: "No persisted chat sessions exist for this project yet.".into(),
                memories: vec![],
            });
        }
        Err(e) => return Err(e).with_context(|| format!("cannot list {}", dir.display())),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }

    let mut memories = Vec::with_capacity(paths.len());
    for path in paths {
        let records = session::load_records(&path).await?;
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let mut memory = extract_memory(id, &records);
        if memory.timestamp.is_none() {
            memory.timestamp = tokio::fs::metadata(&path)
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(DateTime::<Local>::from);
        }
        if !memory.is_empty() {
            memories.push(memory);
        }
    }

    memories.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id)));
    let terms = query_terms(&input.query);
    if !terms.is_empty() {
        memories.retain(|memory| relevance(memory, &terms, &input.query, input.focus) > 0);
        memories.sort_by(|a, b| {
            relevance(b, &terms, &input.query, input.focus)
                .cmp(&relevance(a, &terms, &input.query, input.focus))
                .then_with(|| b.timestamp.cmp(&a.timestamp))
        });
    }
    memories.truncate(input.max_sessions.clamp(1, MAX_SESSION_LIMIT));

    if memories.is_empty() {
        let text = if input.query.trim().is_empty() {
            "No useful human/assistant chat content was found for this project.".into()
        } else {
            format!(
                "No persisted project chat matched query {:?}.",
                input.query.trim()
            )
        };
        return Ok(HistoryOutput { text, memories });
    }

    let mut text = format!(
        "# Project history\n\nDeterministically reconstructed from {} persisted chat session(s); thinking, images, successful tool output, internal injected messages, and compaction boilerplate were removed.\n",
        memories.len()
    );
    for memory in &memories {
        render_memory(&mut text, memory);
    }
    let text = truncate_output(text, MAX_OUTPUT_LINES, MAX_OUTPUT_BYTES);
    Ok(HistoryOutput { text, memories })
}

fn extract_memory(id: String, records: &[SessionRecord]) -> SessionMemory {
    let mut memory = SessionMemory {
        id,
        ..Default::default()
    };
    let mut seen_text = HashSet::new();
    let mut seen_tool_ids = HashSet::new();
    let mut seen_result_ids = HashSet::new();
    let mut tool_names = HashMap::new();

    for record in records {
        memory.timestamp = memory.timestamp.max(record.ts);
        match &record.entry {
            SessionEntry::Message(message) => extract_message(
                message,
                &mut memory,
                &mut seen_text,
                &mut seen_tool_ids,
                &mut seen_result_ids,
                &mut tool_names,
            ),
            SessionEntry::RunStatus { status, reason } => {
                memory.status = Some(match reason {
                    Some(reason) if !reason.trim().is_empty() => {
                        format!("{}: {}", status, clean_excerpt(reason))
                    }
                    _ => status.clone(),
                });
            }
            _ => {}
        }
    }
    // A process interruption around compaction can leave messages only inside
    // the checkpoint. Recover those after primary records; content/tool IDs
    // already seen above prevent replaying the retained tail twice.
    for record in records {
        if let SessionEntry::Compaction { messages } = &record.entry {
            for message in messages {
                extract_message(
                    message,
                    &mut memory,
                    &mut seen_text,
                    &mut seen_tool_ids,
                    &mut seen_result_ids,
                    &mut tool_names,
                );
            }
        }
    }
    dedup(&mut memory.changed_files);
    dedup(&mut memory.plans);
    memory
}

fn extract_message(
    message: &Message,
    memory: &mut SessionMemory,
    seen_text: &mut HashSet<String>,
    seen_tool_ids: &mut HashSet<String>,
    seen_result_ids: &mut HashSet<String>,
    tool_names: &mut HashMap<String, String>,
) {
    if message.injected || message.role == Role::System {
        return;
    }

    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = clean_excerpt(&text);
    if !text.is_empty() && !is_compaction_boilerplate(&text) {
        let key = format!("{:?}\0{text}", message.role);
        if seen_text.insert(key) {
            match message.role {
                Role::User => memory.requests.push(text),
                Role::Assistant => memory.assistant_record.push(text),
                Role::System | Role::Tool => {}
            }
        }
    }

    for block in &message.content {
        match block {
            ContentBlock::ToolUse { id, name, input } if seen_tool_ids.insert(id.clone()) => {
                tool_names.insert(id.clone(), name.clone());
                extract_tool_facts(name, input, memory);
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: true,
            } if seen_result_ids.insert(tool_use_id.clone()) => {
                let name = tool_names
                    .get(tool_use_id)
                    .map(String::as_str)
                    .unwrap_or("tool");
                memory
                    .errors
                    .push(format!("{name}: {}", clean_excerpt(content)));
            }
            _ => {}
        }
    }
}

fn extract_tool_facts(name: &str, input: &serde_json::Value, memory: &mut SessionMemory) {
    match name {
        "write" | "edit" | "sed" => {
            if let Some(path) = input
                .get("path")
                .or_else(|| input.get("file_path"))
                .and_then(|value| value.as_str())
            {
                memory.changed_files.push(path.to_string());
            }
        }
        "patch" => {
            if let Some(patch) = input.get("patch").and_then(|value| value.as_str()) {
                if let Some(header) = patch.lines().next() {
                    if let Some(path) = header.strip_prefix('[').and_then(|h| h.split('#').next()) {
                        memory.changed_files.push(path.to_string());
                    }
                }
            }
        }
        "todo" => {
            if let Some(items) = input.get("todos").and_then(|value| value.as_array()) {
                for item in items {
                    let Some(content) = item.get("content").and_then(|value| value.as_str()) else {
                        continue;
                    };
                    let status = item
                        .get("status")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown");
                    memory
                        .plans
                        .push(format!("[{status}] {}", clean_excerpt(content)));
                }
            }
        }
        _ => {}
    }
}

fn clean_excerpt(text: &str) -> String {
    let cleaned = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.chars().count() <= MAX_EXCERPT_CHARS {
        return cleaned;
    }
    let mut end = cleaned.len();
    for (chars, (index, _)) in cleaned.char_indices().enumerate() {
        if chars == MAX_EXCERPT_CHARS {
            end = index;
            break;
        }
    }
    format!("{}…", &cleaned[..end])
}

fn is_compaction_boilerplate(text: &str) -> bool {
    text.starts_with("[Previous conversation summary")
        || text == "Understood. I have the context from the summary. Continuing where we left off."
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .map(str::to_lowercase)
        .filter(|term| term.chars().count() >= 2)
        .collect();
    dedup(&mut terms);
    terms
}

fn relevance(
    memory: &SessionMemory,
    terms: &[String],
    raw_query: &str,
    focus: HistoriaFocus,
) -> usize {
    let fields: Vec<(usize, String)> = match focus {
        HistoriaFocus::All => vec![
            (
                6,
                memory
                    .timestamp
                    .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default(),
            ),
            (5, memory.requests.join("\n")),
            (5, memory.plans.join("\n")),
            (3, memory.assistant_record.join("\n")),
            (2, memory.changed_files.join("\n")),
            (2, memory.errors.join("\n")),
            (2, memory.status.clone().unwrap_or_default()),
        ],
        HistoriaFocus::Requests => vec![(1, memory.requests.join("\n"))],
        HistoriaFocus::Assistant => vec![(1, memory.assistant_record.join("\n"))],
        HistoriaFocus::Plans => vec![(1, memory.plans.join("\n"))],
        HistoriaFocus::Files => vec![(1, memory.changed_files.join("\n"))],
        HistoriaFocus::Problems => vec![(1, memory.errors.join("\n"))],
        HistoriaFocus::Status => vec![(1, memory.status.clone().unwrap_or_default())],
    };
    let phrase = raw_query.trim().to_lowercase();
    fields
        .into_iter()
        .map(|(weight, field)| {
            let field = field.to_lowercase();
            let term_score: usize = terms
                .iter()
                .map(|term| field.match_indices(term).count())
                .sum();
            let phrase_bonus = usize::from(!phrase.is_empty() && field.contains(&phrase)) * 4;
            weight * (term_score + phrase_bonus)
        })
        .sum()
}

fn dedup(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn render_memory(out: &mut String, memory: &SessionMemory) {
    let stamp = memory
        .timestamp
        .map(|ts| ts.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "legacy session".into());
    out.push_str(&format!("\n## {stamp} — `{}`", memory.id));
    if let Some(status) = &memory.status {
        out.push_str(&format!(" — {status}"));
    }
    out.push('\n');
    render_section(out, "Human requests", &memory.requests);
    render_section(out, "Assistant record", &memory.assistant_record);
    render_section(out, "Plans", &memory.plans);
    render_section(out, "Changed files", &memory.changed_files);
    render_section(out, "Problems", &memory.errors);
}

fn render_section(out: &mut String, title: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    out.push_str(&format!("\n{title}:\n"));
    for value in values {
        out.push_str(&format!("- {value}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use chrono::TimeZone;

    fn record(entry: SessionEntry, minute: u32) -> SessionRecord {
        SessionRecord {
            ts: Local.with_ymd_and_hms(2026, 8, 11, 10, minute, 0).single(),
            entry,
        }
    }

    #[test]
    fn extraction_keeps_chat_facts_and_drops_internal_content() {
        let assistant = Message {
            role: Role::Assistant,
            injected: false,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "private reasoning".into(),
                },
                ContentBlock::Text {
                    text: "The parser failed because the input was stale; I changed the guard."
                        .into(),
                },
                ContentBlock::ToolUse {
                    id: "edit-1".into(),
                    name: "edit".into(),
                    input: serde_json::json!({"path": "src/parser.rs"}),
                },
            ],
        };
        let records = vec![
            record(
                SessionEntry::Message(Message::user("Fix the stale parser")),
                0,
            ),
            record(SessionEntry::Message(assistant), 1),
            record(
                SessionEntry::Message(Message::tool_result("edit-1", "edit rejected", true)),
                2,
            ),
            record(
                SessionEntry::Message(Message::injected("internal retry instruction")),
                3,
            ),
        ];
        let memory = extract_memory("session-a".into(), &records);

        assert_eq!(memory.requests, ["Fix the stale parser"]);
        assert!(memory.assistant_record[0].contains("failed because"));
        assert_eq!(memory.changed_files, ["src/parser.rs"]);
        assert_eq!(memory.errors, ["edit: edit rejected"]);
        let all = memory.searchable_text();
        assert!(!all.contains("private reasoning"));
        assert!(!all.contains("internal retry"));
    }

    #[test]
    fn compaction_recovers_missing_messages_without_replaying_existing_ones() {
        let request = Message::user("Continue the migration plan");
        let records = vec![
            record(SessionEntry::Message(request.clone()), 0),
            record(
                SessionEntry::Compaction {
                    messages: vec![
                        Message::user("[Previous conversation summary: synthetic]"),
                        request,
                        Message::assistant("Migration step two is still pending."),
                    ],
                },
                1,
            ),
        ];
        let memory = extract_memory("session-b".into(), &records);

        assert_eq!(memory.requests, ["Continue the migration plan"]);
        assert_eq!(
            memory.assistant_record,
            ["Migration step two is still pending."]
        );
    }

    #[tokio::test]
    async fn query_returns_only_matching_sessions_and_honours_limit() {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in [
            ("old.jsonl", "database migration"),
            ("new.jsonl", "CSS polish"),
        ] {
            let line = serde_json::to_string(&record(
                SessionEntry::Message(Message::user(text)),
                if name == "old.jsonl" { 1 } else { 2 },
            ))
            .unwrap();
            tokio::fs::write(dir.path().join(name), format!("{line}\n"))
                .await
                .unwrap();
        }

        let result = render_history(
            dir.path(),
            &HistoriaInput {
                query: "migration database".into(),
                max_sessions: 1,
                focus: HistoriaFocus::Requests,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.memories.len(), 1);
        assert_eq!(result.memories[0].id, "old");
        assert!(!result.text.contains("CSS polish"));
    }

    #[tokio::test]
    async fn legacy_records_use_file_modification_time_for_recency() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = SessionRecord {
            ts: None,
            entry: SessionEntry::Message(Message::user("Legacy project decision")),
        };
        let line = serde_json::to_string(&legacy).unwrap();
        tokio::fs::write(dir.path().join("legacy.jsonl"), format!("{line}\n"))
            .await
            .unwrap();

        let result = render_history(
            dir.path(),
            &HistoriaInput {
                query: String::new(),
                max_sessions: 1,
                focus: HistoriaFocus::All,
            },
        )
        .await
        .unwrap();

        assert_eq!(result.memories.len(), 1);
        assert!(result.memories[0].timestamp.is_some());
        assert!(!result.text.contains("legacy session"));
    }

    #[test]
    fn continuation_prompt_requires_reuse_and_avoids_failed_attempts() {
        let prompt = continuation_prompt("2026-08-10 compaction", "prior structured history");

        assert!(prompt.contains("Retrieval scope: 2026-08-10 compaction"));
        assert!(prompt.contains("reuse validated solutions"));
        assert!(prompt.contains("do not repeat failed attempts"));
        assert!(prompt.contains("prior structured history"));
    }

    #[tokio::test]
    async fn all_focus_can_select_a_session_by_date() {
        let dir = tempfile::tempdir().unwrap();
        let line = serde_json::to_string(&record(
            SessionEntry::Message(Message::user("Unrelated wording")),
            3,
        ))
        .unwrap();
        tokio::fs::write(dir.path().join("dated.jsonl"), format!("{line}\n"))
            .await
            .unwrap();

        let result = render_history(
            dir.path(),
            &HistoriaInput {
                query: "2026-08-11".into(),
                max_sessions: 6,
                focus: HistoriaFocus::All,
            },
        )
        .await
        .unwrap();

        assert_eq!(result.memories.len(), 1);
        assert_eq!(result.memories[0].id, "dated");
    }
}
