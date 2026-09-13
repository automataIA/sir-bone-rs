//! `sirbone stats` — fold every local session into one per-tool picture.
//!
//! `audit` answers "what happened in this session". This answers the question
//! that decides whether a tool earns its schema: *on real projects, does the
//! model ever reach for it, and does that change as the context grows?* The
//! bench cannot answer it — bench workspaces are small by construction — but the
//! sessions already on disk can, at no cost and without leaving the machine.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Serialize;
use sirbone::session::{self, AuditSummary};

/// Upper bound of each peak-context bucket, in tokens; the last bucket is open.
const BUCKETS: [(u32, &str); 3] = [(10_000, "<10k"), (30_000, "10-30k"), (60_000, "30-60k")];

fn bucket_of(peak: u32) -> &'static str {
    BUCKETS
        .iter()
        .find(|(limit, _)| peak < *limit)
        .map_or(">60k", |(_, label)| *label)
}

#[derive(Default, Serialize)]
pub struct ToolStats {
    pub calls: usize,
    pub sessions: usize,
    pub errors: usize,
    pub result_tokens: u64,
}

#[derive(Default, Serialize)]
pub struct Group {
    pub sessions: usize,
    pub tool_calls: usize,
    pub compactions: usize,
    /// Sessions the human came back to after the first prompt — the cheapest
    /// available proxy for "the run did not land first time".
    pub multi_turn_sessions: usize,
    pub tools: BTreeMap<String, ToolStats>,
    pub telemetry: sirbone::session::TelemetryTotals,
}

impl Group {
    fn absorb(&mut self, s: &AuditSummary) {
        self.sessions += 1;
        self.tool_calls += s.tool_calls;
        self.compactions += s.compactions;
        self.multi_turn_sessions += usize::from(s.user_turns > 1);
        let t = &s.telemetry;
        self.telemetry.runs += t.runs;
        self.telemetry.compaction_fired += t.compaction_fired;
        self.telemetry.historia_writes += t.historia_writes;
        self.telemetry.historia_hits += t.historia_hits;
        self.telemetry.system_prompt_tokens = self
            .telemetry
            .system_prompt_tokens
            .max(t.system_prompt_tokens);
        self.telemetry.hook_pre_runs += t.hook_pre_runs;
        self.telemetry.hook_pre_denies += t.hook_pre_denies;
        self.telemetry.hook_post_runs += t.hook_post_runs;
        self.telemetry.hook_post_failures += t.hook_post_failures;
        self.telemetry.hook_stop_runs += t.hook_stop_runs;
        self.telemetry.hook_stop_retries += t.hook_stop_retries;
        self.telemetry.hook_stop_exhausted += t.hook_stop_exhausted;
        self.telemetry.tusk_runs += t.tusk_runs;
        self.telemetry.tusk_edits += t.tusk_edits;
        self.telemetry.tusk_withheld += t.tusk_withheld;
        self.telemetry.oracle_runs += t.oracle_runs;
        self.telemetry.oracle_failures += t.oracle_failures;
        self.telemetry.oracle_retries += t.oracle_retries;
        self.telemetry.oracle_rollbacks += t.oracle_rollbacks;
        self.telemetry.oracle_exhausted += t.oracle_exhausted;
        self.telemetry.ask_user_rounds += t.ask_user_rounds;
        self.telemetry.ask_user_questions += t.ask_user_questions;
        self.telemetry.verify_tool_runs += t.verify_tool_runs;
        self.telemetry.spill_writes += t.spill_writes;
        self.telemetry.read_outlines += t.read_outlines;
        self.telemetry.patch_applies += t.patch_applies;
        self.telemetry.patch_rejects += t.patch_rejects;
        self.telemetry.stream_rule_trips += t.stream_rule_trips;
        self.telemetry.plan_contract_initialized += t.plan_contract_initialized;
        self.telemetry.plan_contract_updated += t.plan_contract_updated;
        self.telemetry.plan_mutations_blocked += t.plan_mutations_blocked;
        self.telemetry.tool_batches += t.tool_batches;
        self.telemetry.tool_calls_emitted += t.tool_calls_emitted;
        self.telemetry.completion_checks_fired += t.completion_checks_fired;
        self.telemetry.test_file_mutations += t.test_file_mutations;
        self.telemetry.best_of_attempts += t.best_of_attempts;
        self.telemetry.best_of_selections += t.best_of_selections;
        for (tool, calls) in &s.tools {
            let e = self.tools.entry(tool.clone()).or_default();
            e.calls += calls;
            e.sessions += 1;
            e.errors += s.errors_by_tool.get(tool).copied().unwrap_or(0);
            e.result_tokens += s.result_tokens(tool);
        }
    }

    /// Tools by call count, dearest first.
    fn ranked(&self) -> Vec<(&str, &ToolStats)> {
        let mut rows: Vec<_> = self.tools.iter().map(|(k, v)| (k.as_str(), v)).collect();
        rows.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then(a.0.cmp(b.0)));
        rows
    }
}

#[derive(Serialize)]
pub struct Stats {
    pub sessions_scanned: usize,
    pub sessions_with_tool_calls: usize,
    pub projects: usize,
    pub overall: Group,
    /// Keyed by peak-context bucket, so a tool that only pays off on long
    /// contexts shows up as a rising share instead of a flat average.
    pub by_context: BTreeMap<String, Group>,
    /// Registered tools that no session ever called.
    pub never_called: Vec<String>,
}

/// Audit every session under `~/.sirbone/projects/*/sessions/`, optionally
/// limited to slugs containing `project`.
async fn collect(project: Option<&str>) -> Result<(Vec<AuditSummary>, usize)> {
    let root = sirbone::project_store::projects_root();
    let Ok(mut projects) = tokio::fs::read_dir(&root).await else {
        anyhow::bail!("no local session history at {}", root.display());
    };
    let mut summaries = Vec::new();
    let mut seen_projects = 0;
    while let Ok(Some(entry)) = projects.next_entry().await {
        let slug = entry.file_name().to_string_lossy().into_owned();
        if project.is_some_and(|p| !slug.contains(p)) {
            continue;
        }
        let Ok(mut sessions) = tokio::fs::read_dir(entry.path().join("sessions")).await else {
            continue;
        };
        seen_projects += 1;
        while let Ok(Some(file)) = sessions.next_entry().await {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                if let Ok(summary) = session::audit(&path).await {
                    summaries.push(summary);
                }
            }
        }
    }
    Ok((summaries, seen_projects))
}

pub async fn run_stats(project: Option<String>, json: bool, registry: Vec<String>) -> Result<()> {
    let (summaries, projects) = collect(project.as_deref()).await?;
    let scanned = summaries.len();
    let used: Vec<_> = summaries.iter().filter(|s| s.tool_calls > 0).collect();

    let mut overall = Group::default();
    let mut by_context: BTreeMap<String, Group> = BTreeMap::new();
    for s in &summaries {
        overall.absorb(s);
        by_context
            .entry(bucket_of(s.peak_context_tokens).into())
            .or_default()
            .absorb(s);
    }
    let never_called = registry
        .into_iter()
        .filter(|t| !overall.tools.contains_key(t))
        .collect();

    let stats = Stats {
        sessions_scanned: scanned,
        sessions_with_tool_calls: used.len(),
        projects,
        overall,
        by_context,
        never_called,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        print!("{}", render(&stats));
    }
    Ok(())
}

fn render(s: &Stats) -> String {
    let mut out = String::new();
    out.push_str("# Sir Bone Tool Stats\n\n");
    out.push_str(&format!(
        "- Sessions: {} scanned, {} with tool calls, across {} project(s)\n",
        s.sessions_scanned, s.sessions_with_tool_calls, s.projects
    ));
    out.push_str(&format!("- Tool calls: {}\n", s.overall.tool_calls));
    out.push_str(&format!("- Compactions: {}\n", s.overall.compactions));
    let t = &s.overall.telemetry;
    out.push_str(&format!(
        "- Historia: {} successful structured queries; {} legacy writes\n",
        t.historia_hits, t.historia_writes
    ));
    out.push_str(&format!("- Verification: pre {} runs/{} denies, post {} runs/{} failures, stop {} runs/{} retries/{} exhausted, oracle {} runs/{} failures/{} retries/{} rollbacks/{} exhausted, verify {} runs, tusk {} runs/{} edits/{} withheld\n", t.hook_pre_runs, t.hook_pre_denies, t.hook_post_runs, t.hook_post_failures, t.hook_stop_runs, t.hook_stop_retries, t.hook_stop_exhausted, t.oracle_runs, t.oracle_failures, t.oracle_retries, t.oracle_rollbacks, t.oracle_exhausted, t.verify_tool_runs, t.tusk_runs, t.tusk_edits, t.tusk_withheld));
    out.push_str(&format!(
        "- Ask user: {} rounds, {} questions\n",
        t.ask_user_rounds, t.ask_user_questions
    ));
    out.push_str(&format!(
        "- Context tools: {} spills, {} read outlines, {} patches ({} rejected), {} stream-rule trips\n",
        t.spill_writes, t.read_outlines, t.patch_applies, t.patch_rejects, t.stream_rule_trips
    ));
    out.push_str(&format!(
        "- Sessions the user re-prompted: {}/{}\n",
        s.overall.multi_turn_sessions, s.sessions_with_tool_calls
    ));

    if s.overall.tool_calls == 0 {
        out.push_str("\nNo tool calls recorded yet.\n");
        return out;
    }

    out.push_str("\n## Tools\n\n");
    out.push_str("| Tool | Calls | Share | Sessions | Errors | Result tokens |\n");
    out.push_str("|---|---:|---:|---:|---:|---:|\n");
    let total = s.overall.tool_calls as f64;
    for (tool, t) in s.overall.ranked() {
        out.push_str(&format!(
            "| `{tool}` | {} | {:.1}% | {}/{} | {} | {} |\n",
            t.calls,
            t.calls as f64 / total * 100.0,
            t.sessions,
            s.sessions_with_tool_calls,
            t.errors,
            t.result_tokens
        ));
    }

    out.push_str("\n## By peak context\n\n");
    out.push_str("| Peak context | Sessions | Calls/session | Compactions | Top tools |\n");
    out.push_str("|---|---:|---:|---:|---|\n");
    for label in BUCKETS.iter().map(|(_, l)| *l).chain([">60k"]) {
        let Some(g) = s.by_context.get(label) else {
            continue;
        };
        let top: Vec<String> = g
            .ranked()
            .iter()
            .take(4)
            .map(|(name, t)| format!("{name}:{}", t.calls))
            .collect();
        out.push_str(&format!(
            "| {label} | {} | {:.1} | {} | {} |\n",
            g.sessions,
            g.tool_calls as f64 / g.sessions as f64,
            g.compactions,
            top.join(" ")
        ));
    }

    if !s.never_called.is_empty() {
        out.push_str(&format!(
            "\nNever called on real projects: {}\n",
            s.never_called
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_split_on_peak_context() {
        assert_eq!(bucket_of(0), "<10k");
        assert_eq!(bucket_of(9_999), "<10k");
        assert_eq!(bucket_of(10_000), "10-30k");
        assert_eq!(bucket_of(59_999), "30-60k");
        assert_eq!(bucket_of(60_000), ">60k");
    }

    #[test]
    fn absorb_counts_sessions_separately_from_calls() {
        let mut g = Group::default();
        let mut s = AuditSummary {
            path: "x".into(),
            records: 0,
            started_at: None,
            ended_at: None,
            duration_secs: None,
            first_user: None,
            user_messages: 0,
            user_turns: 2,
            assistant_messages: 0,
            tool_calls: 3,
            tool_errors: 0,
            tools: BTreeMap::from([("read".to_string(), 3)]),
            result_chars_by_tool: BTreeMap::from([("read".to_string(), 400)]),
            errors_by_tool: BTreeMap::new(),
            changed_files: vec![],
            model_changes: vec![],
            total_input_tokens: 0,
            cached_tokens: 0,
            peak_context_tokens: 0,
            snapshots: vec![],
            prefix_breaks: 0,
            compactions: 0,
            final_status: None,
            status_reason: None,
            telemetry: sirbone::session::TelemetryTotals {
                runs: 1,
                historia_hits: 2,
                system_prompt_tokens: 400,
                plan_contract_initialized: 1,
                ..Default::default()
            },
        };
        g.absorb(&s);
        s.tools = BTreeMap::from([("read".to_string(), 1)]);
        g.absorb(&s);

        let read = &g.tools["read"];
        assert_eq!(read.calls, 4, "calls sum across sessions");
        assert_eq!(read.sessions, 2, "sessions count once each");
        assert_eq!(read.result_tokens, 200, "400 chars ≈ 100 tokens, twice");
        assert_eq!(g.multi_turn_sessions, 2);
        assert_eq!(g.telemetry.runs, 2);
        assert_eq!(g.telemetry.historia_hits, 4);
        assert_eq!(
            g.telemetry.system_prompt_tokens, 400,
            "prompt size is a max"
        );
        assert_eq!(g.telemetry.plan_contract_initialized, 2);
    }
}
