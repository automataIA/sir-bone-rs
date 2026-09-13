use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{bail, Context as _, Result};
use clap::{CommandFactory, Parser};
use rustyline_async::{Readline, ReadlineEvent, SharedWriter};
use session::SessionEntry;
use sirbone::{
    agent::{AgentContext, ConfirmBridge, LlmClient, Prompt, PromptKind, PromptReply},
    ai::{AnthropicClient, CodexClient, OpenAiClient},
    claude_md, render, session,
    snapshot::Snapshots,
    tools::{
        BashTool, CodeMapTool, EditTool, GlobTool, GrepTool, HistoriaTool, JobStatusTool,
        LoadSkillTool, NoteTool, ReadStamps, ReadTool, TodoTool, ToolRegistry, UndoStore, UndoTool,
        VerifyTool, WebFetchTool, WebSearchTool, WriteTool,
    },
    types::{AgentEvent, ContentBlock, Message},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod cmd;

#[derive(Parser)]
#[command(
    name = "sirbone",
    version = sirbone::VERSION,
    about = "AI coding agent",
    long_about = "Sir Bone — a from-scratch AI coding agent. Streams LLM responses, runs tools \
                  (bash, file ops, web fetch), in REPL or TUI mode. Provider auto-detected from \
                  env: SIRBONE_CODEX → ChatGPT/Codex, else ANTHROPIC_AUTH_TOKEN → Anthropic, \
                  else OPENAI_API_KEY → OpenAI \
                  (OPENAI_BASE_URL targets Ollama/Groq/etc.).",
    after_help = "EXAMPLES:\n  \
        sirbone \"refactor this module\"          one-shot prompt\n  \
        sirbone                                   interactive TUI (default)\n  \
        sirbone demo                              replay a recorded run — no API key needed\n  \
        sirbone hook install                      review the staged diff on every commit\n  \
        sirbone doctor                            check local setup without calling a model\n  \
        sirbone update                            self-upgrade (installer builds only)\n  \
        sirbone env                               list every env var sirbone reads (+ current value)\n  \
        sirbone audit                             summarize the latest session\n  \
        sirbone stats                             per-tool usage across every local session\n  \
        sirbone ground PLAN.md                    verify a file's code claims (no model)\n  \
        sirbone --repl                            interactive REPL\n  \
        sirbone -c \"follow-up\"                    resume the most recent session\n  \
        sirbone --thinking-budget 10000 \"...\"     extended thinking\n  \
        sirbone --image shot.png \"what's wrong?\"  image input (Anthropic; --vision elsewhere)\n  \
        sirbone --completions zsh > _sirbone      generate shell completions"
)]
struct Cli {
    #[arg(short, long, env = "SIRBONE_MODEL")]
    model: Option<String>,

    #[arg(long, env = "OPENAI_BASE_URL")]
    base_url: Option<String>,

    #[arg(long, env = "OPENAI_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    #[arg(long, env = "ANTHROPIC_AUTH_TOKEN", hide_env_values = true)]
    anthropic_key: Option<String>,

    #[arg(long, env = "ANTHROPIC_BASE_URL")]
    anthropic_base_url: Option<String>,

    /// Use ChatGPT Plus/Pro through the official Codex CLI OAuth session.
    #[arg(long, env = "SIRBONE_CODEX", value_parser = parse_activation_bool)]
    codex: bool,

    /// Headless print mode (like `claude -p`): prompt from the positional arg or,
    /// if absent, from stdin (whole input = one prompt); run one turn and exit
    #[arg(short = 'p', long)]
    print: bool,

    /// Output for one-shot/-p runs: `text` (streamed, default), `json` (quiet run,
    /// then one {result,status,usage,session} object), or `stream-json` (one NDJSON
    /// event per line as they arrive: text/thinking/tool_start/tool_end/ctx, then a
    /// final {type:result,...} — for front-ends that render live, e.g. the editor UI)
    #[arg(long, value_parser = ["text", "json", "stream-json"], default_value = "text")]
    output_format: String,

    /// Control channel for `-p --output-format stream-json`: `text` (default, no
    /// interactive prompts — destructive commands auto-deny) or `stream-json`
    /// (the front-end answers permission/`ask_user` prompts). With stream-json,
    /// each prompt is emitted as `{type:"ask",id,prompt}` on stdout and the reply
    /// is read from stdin as `{type:"reply",id,index?,text?}`.
    #[arg(long, value_parser = ["text", "stream-json"], default_value = "text")]
    input_format: String,

    #[arg(long)]
    session: Option<PathBuf>,

    /// Resume the most recent session (no UUID needed).
    #[arg(short = 'c', long = "continue")]
    continue_recent: bool,

    /// Use the REPL/readline mode instead of the default TUI.
    #[arg(long)]
    repl: bool,

    /// Start tasks with a compact persistent implementation contract.
    #[arg(long, env = "SIRBONE_PLAN", value_parser = parse_activation_bool)]
    plan: bool,

    /// Extended thinking budget in tokens. Anthropic: chain-of-thought budget.
    /// GLM on z.ai: maps to reasoning effort — off/light, 8k=low, 16k=medium,
    /// 32k=max (z.ai has no full off; "disabled" still thinks lightly).
    #[arg(long, env = "SIRBONE_THINKING_BUDGET")]
    thinking_budget: Option<u32>,

    /// Sampling temperature sent to the provider (unset = provider default,
    /// 1.0 on both Anthropic and z.ai GLM-5.x). For benches: pin it so both
    /// arms sample alike. Claude models after Opus 4.6 reject anything but 1.0.
    #[arg(long, env = "SIRBONE_TEMPERATURE")]
    temperature: Option<f32>,

    /// Attach image file(s) to the first prompt (base64-encoded).
    #[arg(long = "image", value_name = "PATH")]
    images: Vec<PathBuf>,

    /// Declare that the OpenAI-compatible endpoint reads images (llama-server with
    /// --mmproj, a vision model on OpenRouter, …). No effect on the Anthropic path.
    #[arg(long, env = "SIRBONE_VISION", value_parser = parse_activation_bool)]
    vision: bool,

    /// Generate shell completions to stdout and exit.
    #[arg(long, value_name = "SHELL")]
    completions: Option<clap_complete::Shell>,

    /// Render a man page (roff) to stdout and exit.
    #[arg(long)]
    man: bool,

    /// Seed the global ~/.sirbone/.env (configure credentials once for every directory), then print it.
    #[arg(long)]
    login: bool,

    /// Non-interactive login (with `login`): provider path; token is read from stdin.
    #[arg(long, value_parser = ["anthropic", "openai", "codex"])]
    login_provider: Option<String>,

    /// Non-interactive login: base URL to write (empty = provider default).
    #[arg(long)]
    login_base_url: Option<String>,

    /// Non-interactive login: model id to write.
    #[arg(long)]
    login_model: Option<String>,

    /// Check provider env, config, project instructions, snapshots, MCP, hooks, and tools; no model call.
    #[arg(long)]
    doctor: bool,

    /// With doctor, explicitly probe provider endpoints.
    #[arg(long = "network", alias = "doctor-network")]
    doctor_network: bool,

    /// Export a local session audit summary; with no path, uses the latest session.
    #[arg(long, value_name = "PATH", num_args = 0..=1)]
    audit: Option<Option<PathBuf>>,

    /// Emit `audit` as JSON instead of Markdown.
    #[arg(long)]
    audit_json: bool,

    /// Fold every local session into one per-tool picture (calls, result tokens,
    /// errors, and how the mix shifts as the context grows). Offline.
    #[arg(long)]
    stats: bool,

    /// Limit `stats` to project slugs containing this substring.
    #[arg(long, value_name = "SLUG")]
    stats_project: Option<String>,

    /// Ground a file's codebase claims (paths/symbols/counts) against the project,
    /// deterministically (no model). With no path, grounds the latest session's
    /// final answer. Exits non-zero on a divergence — usable as a CI gate.
    #[arg(long, value_name = "PATH", num_args = 0..=1)]
    ground: Option<Option<PathBuf>>,

    /// Emit commands that support it as JSON.
    #[arg(long)]
    json: bool,

    /// Enable the configured authoritative verification gate in headless/REPL runs.
    #[arg(long, env = "SIRBONE_ORACLE", value_parser = parse_activation_bool)]
    oracle: bool,

    /// Read-only review run: no file writes, no MCP tools, and bash limited to
    /// the read-only whitelist. Intended for CI and pre-commit review, where the
    /// agent must report without touching the tree.
    #[arg(long, env = "SIRBONE_REVIEW_ONLY", value_parser = parse_activation_bool)]
    review_only: bool,

    /// Prompt. Omit to enter interactive mode.
    prompt: Vec<String>,
}

fn parse_activation_bool(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!(
            "expected one of 1, 0, true, false, yes, no, on, off; got {value:?}"
        )),
    }
}

/// Is a named system-prompt block enabled? Ablate one with
/// `SIRBONE_DISABLE=prompt:<name>`, or all of them with `prompt:*` (the "naked
/// prompt" arm — identity + platform context + the user's own CLAUDE.md only).
///
/// The point is measurement, not a shipped mode: prompt clauses accumulate as
/// patches for a given model's failures, and a later model may not need them. On
/// this repo the pattern is already proven — the HISTORIA clause cost ~15% of
/// tokens for 12 writes / 3 hits. `prompt:*` prices the whole stack at once
/// instead of one A/B per clause.
fn keep_block(name: &str) -> bool {
    !sirbone::ablate::disabled_prompt(name)
}

fn build_system_prompt(cwd: &std::path::Path, claude_md: &str) -> String {
    let mut parts = Vec::new();

    // Core identity. Never ablatable: with no tools named, `prompt:*` runs stop
    // being a lean-prompt arm and become a different agent. `SIRBONE_IDENTITY`
    // *replaces* the line instead (leaving every other block in place) — for
    // embedders with their own persona, and mandatory alongside `SIRBONE_TOOLS`,
    // which can otherwise leave this line naming tools that are not registered.
    parts.push(std::env::var("SIRBONE_IDENTITY").unwrap_or_else(|_| {
        "You are a helpful coding assistant with tools for running shell commands, reading, writing, and editing files.".to_string()
    }));

    // Grounding rule (verify-before-answer). Adopts Anthropic's documented
    // "investigate before answering" pattern (Minimizing hallucinations in
    // agentic coding, Claude Platform Docs) and Cursor's "do NOT guess" tool
    // policy, wrapped in an XML tag per Anthropic's structuring guidance.
    // Default ON but opt-out via SIRBONE_NO_GROUNDING so the ~1.4 KB block can be
    // A/B'd — it is the one always-on prompt addition, and the project gates such
    // additions on a measured win.
    if std::env::var("SIRBONE_NO_GROUNDING").is_err() && keep_block("grounding") {
        let historia_line = if historia_enabled() && keep_block("historia") {
            "- When the user asks about prior changes, decisions, current project state, or continuing an earlier plan in THIS project, use the historia tool before answering.\n"
        } else {
            ""
        };
        parts.push(format!(
            "<investigate_before_answering>\n\
Never speculate about code you have not opened. Any claim about this codebase — \
its structure, behavior, signatures, dependencies, or history — must be grounded \
in something you have read or run THIS turn, not memory or the surrounding context, \
which may be stale, summarized, or wrong.\n\
- Analyze/summarize/describe = inspect the actual code first with the read-only \
tools (read, grep, glob, code_map), even when the context already seems sufficient. \
code_map yields module paths and signatures; the project's own docs are acceptable \
as base-level truth for those.\n\
{historia_line}\
- If you are not sure about file contents or structure, use the tools to find out: \
do NOT guess or make up an answer. Bias toward finding the answer yourself over \
asking the user.\n\
- For external facts (library versions, APIs, standards), verify against installed \
files (Cargo.lock, source), the live API, or the actual file before asserting them. \
Treat AI summaries and \"deep research\" output as unverified — they routinely mix \
real facts with fabricated specifics. If a source cannot confirm a claim, say it is \
unverified instead of guessing.\n\
- When reporting a fix as done, you must have run it (build/test/check); never \
present unverified work as complete.\n\
- Prefer citing the specific file:line over restating from memory.\n\
</investigate_before_answering>"
        ));
    }

    // A tool fan-out clause used to live here, behind its own env flag: the
    // executor has always run a message's non-conflicting calls in parallel
    // lanes (`agent::plan_lanes`) and nothing told the model so. The A/B killed it —
    // width moved 1.64 → 1.78 while total tool calls fell 29% and cited
    // file:line anchors dropped 17.0 → 7.3. It bought tokens by checking less,
    // not by fanning out. The `tool_batches`/`tool_calls_emitted` counters that
    // caught it stay; see docs/BENCH_DECISIONS.md.

    // Platform context
    parts.push(format!(
        "Environment: {} {} | Shell: {} | Working directory: {} | Date: {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::var("SHELL").unwrap_or_else(|_| "unknown".into()),
        cwd.display(),
        chrono::Local::now().format("%Y-%m-%d"),
    ));

    // Git context (if available). Captured once at startup — a session-start
    // snapshot, like the rest of the system prompt.
    if keep_block("git") {
        if let Ok(output) = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(cwd)
            .output()
        {
            if output.status.success() {
                let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                parts.push(format!("Git branch: {branch}"));
            }
        }
        if let Ok(output) = std::process::Command::new("git")
            .args(["status", "--short"])
            .current_dir(cwd)
            .output()
        {
            if output.status.success() && !output.stdout.is_empty() {
                let status: String = String::from_utf8_lossy(&output.stdout)
                    .chars()
                    .take(2000)
                    .collect();
                parts.push(format!("Git status:\n{}", status.trim_end()));
            }
        }
    }

    // Skills catalog: list available skills (name + description) so the model
    // knows they exist and can pull a skill's full instructions via `load_skill`.
    // Bodies are loaded on demand, not dumped here.
    let skills = sirbone::skills::scan_skills();
    // Auto-recall = `always: true` OR a `paths:` glob that matches a file in the
    // working tree. Path-matching is done once here (build_system_prompt runs at
    // startup), so the result is stable for the session and never busts the cache.
    let is_auto = |s: &&sirbone::skills::SkillMeta| {
        s.always || (!s.paths.is_empty() && sirbone::skills::path_globs_match(cwd, &s.paths))
    };
    let on_demand: Vec<_> = skills.iter().filter(|s| !is_auto(s)).collect();
    if !on_demand.is_empty() {
        let mut block = String::from(
            "<skills>\nAvailable skills. Call the `load_skill` tool with a skill's \
             name to get its full instructions before using it.\n",
        );
        for s in &on_demand {
            block.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
        block.push_str("</skills>");
        parts.push(block);
    }
    // Auto-recalled skills (`always: true`, or a `paths:` glob matching the working
    // tree): inject the full body now so the model applies them without a `load_skill`
    // round-trip. This is how user-saved best practices come back on the next startup;
    // path-scoped skills keep irrelevant rules out of context (e.g. no TS rules on a
    // Rust-only tree).
    for s in skills.iter().filter(|s| is_auto(s)) {
        if let Some(body) = sirbone::skills::load_skill_body(&s.path) {
            parts.push(format!("<skill name=\"{}\">\n{}\n</skill>", s.name, body));
        }
    }

    // Output filtering guidance — gated to workspaces with recognized code, like
    // `debug_toolkit`. It's about build/test/log noise; on a docs-only or empty
    // directory it's dead weight in the base prompt.
    if keep_block("output_filter") && !sirbone::structure::discover(cwd).is_empty() {
        parts.push(
            "When a command yields large or noisy output (e.g. `cargo clippy`, `cargo check`, \
            `cargo test`, build or server logs), filter at the shell to keep only what matters \
            rather than dumping everything: pipe to `grep`/`rg` for the problem lines \
            (errors, warnings, failures, tracebacks) or `tail -n` the end. Pick the filter that \
            fits the command and the task."
                .to_string(),
        );
    }

    // Bug-fix discipline: avoid the common failure of "self-authored tests pass,
    // so I'm done" while the real defect remains.
    if keep_block("bugfix") {
        parts.push(
            "When fixing a reported bug or failing behavior: find and change the source \
            cause, then confirm by reproducing the exact scenario from the report and by \
            running the project's existing tests for regressions. Do not invent new tests \
            that pass and treat them as proof the bug is fixed — a self-authored test can \
            pass while the real defect remains. Do not edit, weaken, or delete existing \
            tests to make them pass; fix the code instead."
                .to_string(),
        );
    }

    // Behavioral guardrails distilled from Claude Code's system prompts. Kept to
    // three fragments on purpose: instruction-following degrades as rule count
    // grows, so only rules tracing to observed agent failures live here.
    // Minimalism rule. The YAGNI/KISS reuse-before-write clause is opt-in via
    // SIRBONE_YAGNI while its effect is being A/B'd on the bench (benchmarks/yagni_ab.py).
    // It stays inside this one fragment instead of becoming a new rule, since
    // instruction-following degrades as rule count grows. The safety half is
    // non-negotiable so "write less" never drops a trust-boundary check.
    if keep_block("minimal") {
        let mut minimal = String::from(
            "Do what was asked; nothing more. No extra features, refactors, comments, or \
            error handling for scenarios that cannot occur. Prefer editing existing files \
            over creating new ones; never create documentation files unless explicitly \
            requested.",
        );
        if std::env::var_os("SIRBONE_YAGNI").is_some() {
            minimal.push_str(
                " Before writing new code, reuse what already exists \u{2014} the standard \
                library, a platform feature, or an already-present dependency \u{2014} and pick \
                the smallest change that solves the task; add no new dependency or abstraction \
                that was not requested. Never trade away input validation, error handling, or \
                security to write less.",
            );
        }
        parts.push(minimal);
    }
    if keep_block("style") {
        parts.push(
            "Lead with the outcome, then only the detail needed to act on it. Keep \
            responses short; skip preamble and restating what you are about to do."
                .to_string(),
        );
    }
    // The ambiguity clause is opt-in via SIRBONE_ASK_AMBIGUOUS while its effect is
    // measured: on deliberately under-specified tasks the reported rate of
    // test-hardcoding is an order of magnitude higher than on unambiguous ones
    // (EvilGenie, arXiv 2511.21654), which makes "ask instead of guess" an honesty
    // countermeasure and not a UX comfort. It sits inside this fragment rather than
    // becoming a new rule, since instruction-following degrades as rule count grows.
    if keep_block("ask") {
        let mut ask = String::from(
            "When you need to clarify something, ask one question at a time and wait for \
            the answer before the next — not a batched list. First try to answer it \
            yourself by reading the code; only ask when the code cannot settle it.",
        );
        if std::env::var_os("SIRBONE_ASK_AMBIGUOUS").is_some() {
            ask.push_str(
                " Treat an under-specified task as a question, not as freedom: when the \
                request admits materially different implementations and neither the code \
                nor the tests settle which one is wanted, call `ask_user` once, naming the \
                choice, before writing the code that assumes an answer. Never settle an \
                ambiguity by changing a test so your guess passes. If no user can answer, \
                proceed with the most conservative reading and state in your final message \
                which reading you took.",
            );
        }
        parts.push(ask);
    }
    if keep_block("truthful") {
        parts.push(
            "Report results truthfully: if a test fails, a step was skipped, or something \
            is unverified, say so plainly — never present partial work as done. Reference \
            code as file_path:line_number."
                .to_string(),
        );
    }

    // The verify block used to live here: 583 chars restating the grounding
    // block's own rules. A SpecBench A/B (2026-08-07) came back neutral on every
    // metric, so its one unique idea — AI summaries are not a source — moved into
    // the grounding block's external-facts bullet and the duplicate is gone.

    // The historia completion-requirement clause used to live here. Stage-1
    // ablation measured it at zero effect (historia_writes 24 with, 24 without),
    // so it is gone: the tool pointer in the tools list is the whole surface now.

    // Per-language debugging cheat-sheet (batch/non-interactive), gated to the
    // languages present in the workspace.
    if keep_block("debug_toolkit") {
        if let Some(toolkit) = sirbone::system_prompt::debug_toolkit(cwd) {
            parts.push(toolkit);
        }
    }

    // Trusted user customization from ~/.sirbone/system/*.md, appended (not replacing
    // the base) so core instructions can be extended but not broken.
    if keep_block("user_appends") {
        if let Some(user) = sirbone::system_prompt::user_appends() {
            parts.push(user);
        }
    }

    let base = parts.join("\n");
    // CLAUDE.md survives `prompt:*` on purpose: the naked arm prices *our* dead
    // weight, not the user's project instructions. Ablate it explicitly with
    // `prompt:claude_md` when that is the question being asked.
    let prompt = if claude_md.is_empty() || sirbone::ablate::disabled_prompt_exact("claude_md") {
        base
    } else {
        format!("{base}\n\n<instructions>\n{claude_md}\n</instructions>")
    };
    sirbone::telemetry::add(
        &sirbone::telemetry::SYSTEM_PROMPT_CHARS,
        prompt.len() as u64,
    );
    prompt
}

/// Schema-aware project history is default-on; `SIRBONE_NO_HISTORIA` disables
/// its read-only tool and prompt fragment so the feature can be A/B ablated.
fn historia_enabled() -> bool {
    std::env::var_os("SIRBONE_NO_HISTORIA").is_none()
}

/// Return the optional scope of an exact `/historia` command.
///
/// Keep this parsing at the CLI boundary: editor front-ends use one-shot mode,
/// so they must get the same deterministic prefetch as the interactive REPL.
fn historia_command_query(input: &str) -> Option<&str> {
    let command = input.strip_prefix('/')?;
    let mut parts = command.splitn(2, char::is_whitespace);
    (parts.next()? == "historia").then(|| parts.next().unwrap_or_default().trim())
}

async fn expand_historia_command(input: &str, tools: &ToolRegistry) -> Result<Option<String>> {
    let Some(query) = historia_command_query(input) else {
        return Ok(None);
    };
    let max_sessions = if query.is_empty() { 6 } else { 20 };
    let history = tools
        .execute(
            "historia",
            serde_json::json!({
                "query": query,
                "max_sessions": max_sessions,
                "focus": "all"
            }),
        )
        .await
        .context("historia lookup failed")?;
    Ok(Some(sirbone::tools::historia::continuation_prompt(
        query, &history,
    )))
}

/// Tools whose only consumer is a human at a front-end: `ask_user` needs someone
/// to answer, `todo` renders as the live plan in the TUI/ACP/VSCode. A SpecBench
/// A/B (2026-08-07, 27 headless runs) recorded zero calls to either, against 726
/// tokens of schema paid on every turn — so headless runs don't register them.
/// `job_status` and `undo` also went uncalled there but stay: both work headless
/// (background jobs, edit recovery), and dropping them is a capability cut.
fn make_tools(cwd: &Path, interactive: bool) -> ToolRegistry {
    let undo = UndoStore::default();
    // Shared across read/edit/sed/write so an edit can detect the file changed
    // since the read it was based on (the "lost update" guard, Feature A).
    let stamps = ReadStamps::default();
    let mut t = ToolRegistry::new();
    // SIRBONE_NO_TOOLS: run with no tools at all, so the model must answer from
    // prior knowledge without reading the codebase — the "cold" regime where
    // factual hallucinations actually occur (and where SIRBONE_GROUND earns its
    // keep). Used by the grounding A/B's cold arm.
    if std::env::var_os("SIRBONE_NO_TOOLS").is_some() {
        return t;
    }
    t.register(BashTool {
        jobs: t.jobs.clone(),
    });
    t.register(JobStatusTool {
        jobs: t.jobs.clone(),
    });
    // Adopt background jobs left behind by dead sirbone processes (their logs
    // and `.exit` sentinels persist on disk); completions then surface through
    // the normal notification path.
    t.jobs.restore_orphans();
    t.register(ReadTool {
        stamps: stamps.clone(),
    });
    // Review-only: don't register the writers at all. The permission gate would
    // deny each call anyway, but leaving their schemas out stops the model from
    // planning an edit it cannot make — and stops paying for those schemas every
    // turn. The gate stays the guarantee; this is only the cheaper path to it.
    if !sirbone::permissions::review_only() {
        t.register(WriteTool {
            undo: undo.clone(),
            stamps: stamps.clone(),
        });
        // One edit tool at a time: `patch` supersedes `edit` under the hashline
        // arm, so the model never has to choose between two ways to say the same
        // thing (and the A/B stays clean).
        if sirbone::tools::patch::enabled() {
            t.register(sirbone::tools::PatchTool {
                undo: undo.clone(),
                stamps,
            });
        } else {
            t.register(EditTool {
                undo: undo.clone(),
                stamps,
            });
        }
        t.register(UndoTool { store: undo });
    }
    t.register(GrepTool);
    t.register(GlobTool);
    t.register(WebFetchTool);
    t.register(WebSearchTool::default());
    t.register(LoadSkillTool);
    t.register(NoteTool {
        store: t.notes.clone(),
    });
    if interactive {
        if sirbone::ablate::ask_rounds_enabled() {
            t.register(sirbone::tools::AskUserRoundTool);
        } else {
            t.register(sirbone::tools::AskUserTool);
        }
        t.register(TodoTool {
            store: t.todos.clone(),
        });
    }
    if sirbone::oracle::load_test_command().is_some() {
        t.register(VerifyTool);
    }
    t.register(CodeMapTool {
        root: cwd.to_path_buf(),
    });
    if historia_enabled() {
        t.register(HistoriaTool {
            project: cwd.to_path_buf(),
        });
    }
    t.apply_ablation();
    t
}

/// Join the background MCP load, register the discovered tools into `tools`, and
/// surface the once-per-session schema-cost line. Returns the server handles —
/// the caller must keep them alive for the whole session (dropping one kills its
/// child). `announce` is false in the TUI (its alt-screen would mangle stderr).
async fn register_mcp(
    tools: &mut ToolRegistry,
    task: tokio::task::JoinHandle<sirbone::mcp::McpLoad>,
    announce: bool,
) -> Vec<Arc<sirbone::mcp::client::McpServer>> {
    let handles = match task.await {
        Ok((tool_vec, handles)) => {
            for tool in tool_vec {
                tools.register_dyn(tool);
            }
            handles
        }
        Err(e) => {
            eprintln!("warning: MCP load task failed: {e}");
            Vec::new()
        }
    };
    // MCP tool schemas ride the system payload every turn, so they spend input
    // budget like the prompt. The [usage] report carries the same figure.
    let (n, tok) = tools.mcp_schema_cost();
    if announce && n > 0 {
        eprintln!("MCP: {n} tools, ~{tok} tok/turn of schema (counts against the context window)");
    }
    handles
}

/// Shared slot holding the REPL's `SharedWriter` once readline is up. The
/// `tracing` fmt layer writes through this: when the slot is set, log lines print
/// *above* the prompt (no clobbering the typed input); otherwise they fall back to
/// raw stderr (one-shot mode).
#[derive(Clone)]
struct LogSink(Arc<Mutex<Option<SharedWriter>>>);

/// Concrete writer handed out per log event by `LogSink::make_writer`.
enum LogWriter {
    Shared(SharedWriter),
    Stderr(std::io::Stderr),
}

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Shared(w) => w.write(buf),
            Self::Stderr(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Shared(w) => w.flush(),
            Self::Stderr(w) => w.flush(),
        }
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for LogSink {
    type Writer = LogWriter;
    fn make_writer(&self) -> Self::Writer {
        match self.0.lock() {
            Ok(slot) => match slot.as_ref() {
                Some(w) => LogWriter::Shared(w.clone()),
                None => LogWriter::Stderr(std::io::stderr()),
            },
            Err(_) => LogWriter::Stderr(std::io::stderr()),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("a rustls crypto provider was already installed"))?;
    dotenvy::dotenv().ok();
    // Global credentials fallback: a single ~/.sirbone/.env so you configure once
    // and run from any directory. Loaded AFTER the project .env so the process env
    // and the project file win (dotenvy never overrides an already-set var).
    if let Some(p) = sirbone::config::global_env_path() {
        let _ = dotenvy::from_path(&p);
    }

    let cli = Cli::parse();
    // Before anything builds a tool registry or a permission config: both read
    // the mode back out of `permissions`, so it has to be set first.
    sirbone::permissions::set_review_only(cli.review_only);
    // Textual log destination: filled with the REPL's SharedWriter later (logs then
    // print above the prompt); until then it falls back to raw stderr.
    let log_sink = LogSink(Arc::new(Mutex::new(None)));
    {
        use tracing_subscriber::prelude::*;
        let env_filter = tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("sirbone=info".parse().context("default log directive")?);
        // In TUI mode the alternate screen is owned by ratatui; raw writes would paint
        // over the UI (e.g. land in the input box). So mount the textual log layer only
        // outside the TUI — the F12 debug panel (tui-logger buffer) always captures.
        let tui_mode = !cli.repl && !cli.print && cli.prompt.join(" ").is_empty();
        let log_layer = (!tui_mode).then(|| {
            tracing_subscriber::fmt::layer()
                .with_writer(log_sink.clone())
                .with_filter(env_filter)
        });
        // Capture into tui-logger's buffer (F12 debug panel) and optionally print logs.
        tracing_subscriber::registry()
            .with(tui_logger::TuiTracingSubscriberLayer)
            .with(log_layer)
            .init();
        tui_logger::init_logger(tui_logger::LevelFilter::Trace).context("init tui-logger")?;
        tui_logger::set_default_level(tui_logger::LevelFilter::Info);
    }

    if let Some(shell) = cli.completions {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        return Ok(());
    }
    if cli.man {
        clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
        return Ok(());
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cli.prompt.first().map(|s| s.as_str()) == Some("setup-verification") {
        return cmd::run_setup_verification(&cwd, cli.json).await;
    }
    let login_codex_command = (cli.login
        || cli.prompt.first().map(|s| s.as_str()) == Some("login"))
        && (cli.codex || cli.prompt.get(1).map(|s| s.as_str()) == Some("codex"));
    if login_codex_command {
        return cmd::login_codex().await;
    }
    if cli.login || cli.prompt.first().map(|s| s.as_str()) == Some("login") {
        if let Some(provider) = cli.login_provider.as_deref() {
            if provider == "codex" {
                return cmd::login_codex().await;
            }
            return cmd::set_credentials(
                provider,
                cli.login_base_url.as_deref(),
                cli.login_model.as_deref(),
            )
            .await;
        }
        return cmd::run_login().await;
    }
    if cli.doctor || (cli.prompt.len() == 1 && cli.prompt[0] == "doctor") {
        // Assembling the prompt here (rather than inside doctor) keeps
        // `build_system_prompt` where the blocks live, and lets doctor report the
        // weight of the current block selection — the number the prompt-ablation
        // loop needs before deciding what to cut.
        let mut instructions = claude_md::load_instructions(&cwd, "AGENTS.md").await;
        if instructions.trim().is_empty() {
            instructions = claude_md::load_instructions(&cwd, "CLAUDE.md").await;
        }
        let prompt = build_system_prompt(&cwd, &instructions);
        return cmd::run_doctor(&cwd, &cli, prompt.len()).await;
    }
    if let Some(path) = cli.audit.clone() {
        return cmd::run_audit(path, cli.audit_json).await;
    }
    if cli.prompt.first().map(|s| s.as_str()) == Some("audit") {
        let path = cli.prompt.get(1).map(PathBuf::from);
        return cmd::run_audit(path, cli.audit_json).await;
    }
    if cli.stats || cli.prompt.first().map(|s| s.as_str()) == Some("stats") {
        // The live registry supplies the "never called" list: a tool absent from
        // every session is only interesting if it is actually registered. Take
        // the interactive superset so the report can name a front-end-only tool.
        let registry = make_tools(&cwd, true)
            .schema_ranking()
            .into_iter()
            .map(|(name, _)| name.to_string())
            .collect();
        return cmd::run_stats(cli.stats_project.clone(), cli.json, registry).await;
    }
    if cli.prompt.first().map(|s| s.as_str()) == Some("snapshots") {
        return cmd::run_snapshots(cli.json).await;
    }
    // `sirbone demo [session.jsonl]` — replay before the provider check below,
    // since the whole point is that it runs without credentials.
    if cli.prompt.first().map(|s| s.as_str()) == Some("demo") {
        return cmd::run_demo(cli.prompt.get(1).map(PathBuf::from)).await;
    }
    // `sirbone hook install|uninstall` — installing a pre-commit review needs
    // no provider, so it also runs before the credential check.
    if cli.prompt.first().map(|s| s.as_str()) == Some("hook") {
        return cmd::run_hook(cli.prompt.get(1).map(String::as_str), &cwd);
    }
    // Exact `sirbone update` only: a longer prompt starting with "update" is a
    // task ("update the changelog"), not the self-upgrade command.
    if cli.prompt.len() == 1 && cli.prompt[0] == "update" {
        return cmd::run_update();
    }
    if cli.prompt.first().map(|s| s.as_str()) == Some("env") {
        cmd::run_env_list(cli.json);
        return Ok(());
    }
    if let Some(arg) = cli.ground.clone() {
        return cmd::run_ground(arg, &cwd).await;
    }
    if cli.prompt.first().map(|s| s.as_str()) == Some("ground") {
        return cmd::run_ground(cli.prompt.get(1).map(PathBuf::from), &cwd).await;
    }

    // Lift any inline mcpServers from config.json into the ~/.sirbone/mcp.json catalog.
    sirbone::mcp::migrate_inline_servers();

    let mut meta = sirbone::project_store::load_meta(&cwd);

    let anthropic_key = cli
        .anthropic_key
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok());

    // Model precedence: --model/SIRBONE_MODEL → last model used in this project →
    // provider-appropriate default (the provider is decided by which key is set).
    let model = cli.model.or_else(|| meta.model.clone()).unwrap_or_else(|| {
        if cli.codex {
            "auto"
        } else if anthropic_key.is_some() {
            "claude-opus-4-7"
        } else {
            "gpt-4o-mini"
        }
        .into()
    });
    if meta.model.as_deref() != Some(model.as_str()) {
        meta.model = Some(model.clone());
        let _ = sirbone::project_store::save_meta(&cwd, &mut meta);
    }
    // `images_ok` is inferred only for Anthropic: the official API reads images,
    // while Anthropic-compatible proxies (e.g. z.ai/GLM) accept them but the model
    // is blind and hallucinates. No OpenAI-compatible endpoint advertises vision,
    // so there it is declared by the user with `--vision` instead of guessed.
    if cli.codex && cli.temperature.is_some() {
        bail!("--temperature is not supported with --codex (Codex CLI owns sampling)");
    }
    let (client, provider, images_ok): (Arc<dyn LlmClient>, &str, bool) = if cli.codex {
        (Arc::new(CodexClient::new(&model, &cwd)), "codex", false)
    } else if let Some(key) = anthropic_key {
        let base = cli
            .anthropic_base_url
            .unwrap_or_else(|| "https://api.anthropic.com".into());
        // Vision works reliably only on the official Anthropic API. Match the
        // host (not a substring) so a proxy whose name happens to contain
        // "api.anthropic.com" doesn't falsely report vision support.
        let images_ok = url::Url::parse(&base)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .is_some_and(|h| h == "api.anthropic.com" || h.ends_with(".api.anthropic.com"));
        let c = AnthropicClient::new(&base, &key, &model).with_temperature(cli.temperature);
        c.set_thinking_budget(cli.thinking_budget);
        (Arc::new(c), "anthropic", images_ok)
    } else if let Some(key) = cli.api_key {
        let base = cli
            .base_url
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        let c = OpenAiClient::new(&base, &key, &model).with_temperature(cli.temperature);
        // GLM endpoints translate the dial into reasoning_effort; elsewhere
        // the stored budget is inert (never sent, not reported by the client).
        c.set_thinking_budget(cli.thinking_budget);
        (Arc::new(c), "openai", cli.vision)
    } else if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // Interactive onboarding: run the same wizard as `sirbone login`, then
        // re-exec so the freshly written ~/.sirbone/.env is picked up cleanly.
        eprintln!("No API key found — let's set one up.\n");
        cmd::run_login().await?;
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new(std::env::current_exe()?)
                .args(std::env::args_os().skip(1))
                .exec();
            bail!("re-exec after login failed: {err}");
        }
        #[cfg(not(unix))]
        {
            println!("\nCredentials saved. Re-run `sirbone`.");
            return Ok(());
        }
    } else {
        bail!(
             "no provider configured.\n\n\
             Set one of these (env var or a `.env` file in the project root):\n  \
             ANTHROPIC_AUTH_TOKEN=sk-...     → Anthropic (Claude)\n  \
             OPENAI_API_KEY=sk-...           → OpenAI, or any OpenAI-compatible endpoint\n                                  \
             (set OPENAI_BASE_URL for Ollama, Groq, z.ai, …)\n\n\
             For a ChatGPT Plus/Pro subscription, install Codex CLI, run `sirbone login --codex`,\n  \
             then start with `sirbone --codex`.\n\n\
             Run `sirbone login` to create a global ~/.sirbone/.env, then fill in a key:\n  \
             sirbone login"
        );
    };
    // Record the endpoint's real vision capability once: every front-end that can
    // attach an image (TUI Ctrl-V, REPL `/attach`) reads it from here instead of
    // re-deriving the provider rules.
    sirbone::attachments::set_vision_supported(images_ok);
    if !cli.images.is_empty() && !images_ok {
        eprintln!(
            "warning: --image may be ignored or hallucinated — the active endpoint ({provider}) is not declared vision-capable; pass --vision (or SIRBONE_VISION=1) on an OpenAI-compatible endpoint that reads images"
        );
    }
    // Project instructions follow the AGENTS.md open standard so other harnesses
    // read the same file: AGENTS.md is primary, CLAUDE.md the fallback for repos
    // that only ship that. Neither present means the project isn't initialized.
    let mut instructions = claude_md::load_instructions(&cwd, "AGENTS.md").await;
    if instructions.trim().is_empty() {
        instructions = claude_md::load_instructions(&cwd, "CLAUDE.md").await;
    }
    if instructions.trim().is_empty() {
        eprintln!("no AGENTS.md or CLAUDE.md found — run /init to generate one");
    }
    let system_prompt = build_system_prompt(&cwd, &instructions);

    let session_path = match (cli.session, cli.continue_recent) {
        (Some(p), _) => p,
        (None, true) => match session::latest_session_path().await {
            Some(p) => {
                eprintln!("Continuing session {}", p.display());
                p
            }
            None => {
                eprintln!("No previous session found; starting a new one.");
                session::new_session_path()
            }
        },
        (None, false) => session::new_session_path(),
    };
    let mut messages: Vec<Message> = session::collapse(session::load(&session_path).await?);

    // Build the tool registry once. MCP servers (if any configured) are spawned
    // here and their handles kept alive in `_mcp_servers` for the whole session;
    // dropping a handle kills its child process. No config = no overhead.
    // A front-end that can answer a question and render a plan: the TUI, the
    // REPL, Zed's ACP panel, or a bidirectional stream-json client (VSCode).
    let interactive = cli.repl
        || cli.prompt.first().map(|s| s.as_str()) == Some("acp")
        || (!cli.print && cli.prompt.join(" ").is_empty())
        || (cli.output_format == "stream-json" && cli.input_format == "stream-json");
    let mut tools = make_tools(&cwd, interactive);
    // Spawn MCP servers off the critical path: `npx`-launched servers cost
    // seconds, so awaiting them here would stall every launch. The handle is
    // joined just before the first turn (overlapping localization / the TUI's
    // first frame / the readline prompt), then the tools are registered.
    let mcp_task = tokio::spawn(sirbone::mcp::collect_tools());

    // Warm the code structure index in the background so it's fresh from the
    // start of the session (the `code_map` tool also refreshes on demand). Fire
    // and forget: never blocks startup, errors are non-fatal.
    {
        let cwd = cwd.clone();
        tokio::spawn(tokio::task::spawn_blocking(move || {
            let idx = sirbone::structure::update(&cwd, sirbone::structure::Index::load(&cwd));
            let _ = idx.save(&cwd);
            // Materialise the file→file graph so the tool reads it back fast;
            // rebuilt only if a file changed since last run (fingerprint).
            let _ = sirbone::structure::graph_cached(&cwd, &idx);
        }));
    }

    // ACP server mode (`sirbone acp`): speak the Agent Client Protocol on
    // stdin/stdout for Zed's Agent panel. Register MCP first (handles kept alive
    // for the whole serve), then hand off — stdout is the JSON-RPC wire, so this
    // must return before any of the one-shot/TUI paths write to it.
    if cli.prompt.first().map(|s| s.as_str()) == Some("acp") {
        let _mcp = register_mcp(&mut tools, mcp_task, true).await;
        return sirbone::acp::serve(model, client, images_ok, system_prompt, tools).await;
    }

    let mut prompt = cli.prompt.join(" ");
    // `-p/--print` without a positional prompt: the whole stdin is the prompt.
    if cli.print && prompt.is_empty() {
        use tokio::io::AsyncReadExt as _;
        let mut buf = String::new();
        tokio::io::stdin().read_to_string(&mut buf).await?;
        prompt = buf.trim().to_string();
        if prompt.is_empty() {
            eprintln!("error: -p/--print requires a prompt (argument or stdin)");
            std::process::exit(2);
        }
    }
    if !cli.images.is_empty() && prompt.is_empty() {
        // Fail loud instead of silently dropping the images (they were only
        // attached inside the prompt branch below).
        eprintln!("error: --image requires a prompt describing what to do with the image");
        std::process::exit(2);
    }
    if let Some(expanded) = expand_historia_command(&prompt, &tools).await? {
        prompt = expanded;
    }
    if !prompt.is_empty() {
        if cli.plan {
            tools.start_plan(&prompt);
        }
        // Front-loaded context for a more linear run: combine a DETERMINISTIC
        // grounding seed (exact location+signature of the entities the prompt
        // names — no LLM) with the LLM localization pre-pass. Seeded together so
        // the model starts with the real map and goes straight there instead of
        // searching. `notes.seed` is first-wins, so build one combined block.
        let mut seed_parts: Vec<String> = Vec::new();
        if std::env::var_os("SIRBONE_NO_GROUND_CONTEXT").is_none() {
            let root = cwd.clone();
            let p = prompt.clone();
            if let Some(ctx) =
                tokio::task::spawn_blocking(move || sirbone::agent::prompt_context(&root, &p))
                    .await
                    .ok()
                    .flatten()
            {
                seed_parts.push(ctx);
            }
        }
        if std::env::var_os("SIRBONE_NO_LOCALIZE").is_none() {
            // Localization pre-pass (Agentless stage-1): a bounded read-only run that
            // seeds the working notes with *where* to change before the main session.
            // Opt out with SIRBONE_NO_LOCALIZE=1. (Planning is now the model's own
            // `plan` tool, called mid-session; no separate human-gated pre-pass.)
            eprintln!("localizing…");
            if let Some(report) = sirbone::agent::localize(
                client.clone(),
                &model,
                &prompt,
                sirbone::tools::read_only_registry(),
                6,
                &CancellationToken::new(),
            )
            .await
            {
                seed_parts.push(format!(
                    "LOCALIZATION (where the change likely belongs):\n{report}"
                ));
            }
        }
        if !seed_parts.is_empty() {
            tools.notes.seed(seed_parts.join("\n\n"));
        }
        // `--image` goes through the same session store as a TUI/REPL paste, so
        // a resumed session still has the file the turn referred to.
        let attach_dir = sirbone::attachments::dir_for(&session_path);
        let attached: Vec<_> = cli
            .images
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                match sirbone::attachments::save_file(&attach_dir, format!("image_{}", i + 1), p) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        eprintln!("warning: cannot load image {}: {e}", p.display());
                        None
                    }
                }
            })
            .collect();
        let user_msg = Message {
            role: sirbone::Role::User,
            injected: false,
            content: sirbone::attachments::user_content(&attached, prompt),
        };
        session::append(&session_path, &SessionEntry::Message(user_msg.clone())).await?;
        messages.push(user_msg);
        // Register MCP now (overlapped with localization above); keep handles alive.
        let _mcp = register_mcp(&mut tools, mcp_task, true).await;
        // Oracle gate is opt-in (default OFF) — ablations found it net-neutral/negative.
        let json_out = cli.output_format == "json";
        let stream = cli.output_format == "stream-json";
        // Interactive control channel: only when the front-end opts in with
        // `--input-format stream-json` (e.g. the VSCode extension). Otherwise the
        // headless run stays non-interactive and auto-denies destructive commands.
        let confirm = (stream && cli.input_format == "stream-json").then(spawn_stdio_prompt_bridge);
        // Best-of-K (opt-in): K independent attempts from the same tree, the
        // winner picked by running the project's tests, never by the model
        // judging itself. `None` = the ordinary single attempt.
        let mut session_path = session_path;
        let res = match best_of_plan(confirm.is_some()) {
            Some((best, snapshots)) => {
                run_best_of(
                    best,
                    snapshots,
                    &model,
                    &client,
                    &mut session_path,
                    &system_prompt,
                    &mut messages,
                    &tools,
                    json_out || stream,
                    stream,
                    cli.oracle,
                )
                .await
            }
            None => {
                run_turn(
                    &model,
                    &client,
                    &session_path,
                    &system_prompt,
                    &mut messages,
                    &tools,
                    false,
                    json_out || stream,
                    stream,
                    confirm,
                    cli.oracle,
                    CancellationToken::new(),
                )
                .await
            }
        };
        // JSON / stream-json: emit the final result object (stream-json tags it
        // `type:result` so it trails the NDJSON event lines already printed).
        if json_out || stream {
            if let Ok(usage) = &res {
                let result_text = messages
                    .iter()
                    .rev()
                    .find(|m| matches!(m.role, sirbone::Role::Assistant))
                    .map(|m| sirbone::types::extract_text(&m.content))
                    .unwrap_or_default();
                let session = session_path.display().to_string();
                let obj = if stream {
                    serde_json::json!({ "type": "result", "result": result_text, "status": "done", "usage": usage, "session": session })
                } else {
                    serde_json::json!({ "result": result_text, "status": "done", "usage": usage, "session": session })
                };
                println!("{obj}");
            }
        }
        // Deterministic claim grounding (opt-in, SIRBONE_GROUND): after the run,
        // print the verified facts for the paths/symbols/counts the answer
        // references — straight to the user, NOT back to the model. The bench
        // showed detection is reliable but trusting the model to self-correct is
        // not, so surface the facts (same engine as `sirbone ground`) and let the
        // user act; no extra LLM turn, no added load.
        if res.is_ok() && std::env::var_os("SIRBONE_GROUND").is_some() {
            if let Some(draft) = messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, sirbone::Role::Assistant))
                .map(|m| sirbone::types::extract_text(&m.content))
                .filter(|t| !t.trim().is_empty())
            {
                let root = std::env::current_dir().unwrap_or_default();
                let facts =
                    tokio::task::spawn_blocking(move || sirbone::agent::facts(&root, &draft))
                        .await
                        .unwrap_or_default();
                if let Some(block) = sirbone::agent::facts_block(&facts) {
                    println!("\n{block}");
                }
            }
        }
        return res.map(|_| ());
    }

    // TUI mode (default)
    if !cli.repl {
        return sirbone::tui::run_tui(
            model,
            provider.to_string(),
            client,
            tools,
            system_prompt,
            messages,
            session_path,
            mcp_task,
            cli.plan,
        )
        .await;
    }

    // REPL/readline mode (--repl)
    run_repl(
        model,
        client,
        cwd,
        system_prompt,
        session_path,
        messages,
        tools,
        mcp_task,
        log_sink,
        cli.oracle,
        cli.plan,
    )
    .await
}

/// The `--repl` readline loop, split out of `main()`: piped stdin fallback,
/// slash commands, prompt bridge, background-job notifications.
#[allow(clippy::too_many_arguments)]
async fn run_repl(
    mut model: String,
    client: Arc<dyn LlmClient>,
    cwd: PathBuf,
    system_prompt: String,
    mut session_path: PathBuf,
    mut messages: Vec<Message>,
    mut tools: ToolRegistry,
    mcp_task: tokio::task::JoinHandle<sirbone::mcp::McpLoad>,
    log_sink: LogSink,
    oracle_requested: bool,
    plan_requested: bool,
) -> anyhow::Result<()> {
    let mut plan_mode = plan_requested;
    // Piped/scripted input (no TTY): rustyline can't run (ENXIO). Read prompts
    // line-by-line from stdin instead — one turn per line, `/quit` or EOF ends,
    // destructive commands auto-deny (no confirm channel).
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        if !messages.is_empty() {
            eprintln!("Resuming session ({} messages loaded)", messages.len());
        }
        let _mcp = register_mcp(&mut tools, mcp_task, true).await;
        use tokio::io::AsyncBufReadExt as _;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Some(line) = lines.next_line().await? {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            if line == "/quit" {
                break;
            }
            if line == "/plan" {
                plan_mode = !plan_mode;
                if !plan_mode {
                    tools.notes.stop_plan();
                }
                eprintln!("plan mode {}", if plan_mode { "on" } else { "off" });
                continue;
            }
            if plan_mode {
                tools.start_plan(&line);
            } else {
                tools.notes.stop_plan();
            }
            let user_msg = Message::user(line);
            session::append(&session_path, &SessionEntry::Message(user_msg.clone())).await?;
            messages.push(user_msg);
            run_turn(
                &model,
                &client,
                &session_path,
                &system_prompt,
                &mut messages,
                &tools,
                false,
                false,
                false,
                None,
                oracle_requested,
                CancellationToken::new(),
            )
            .await?;
        }
        return Ok(());
    }
    let (mut rl, mut writer) = Readline::new("> ".to_string())?;
    // Route tracing logs through the SharedWriter so they print above the prompt
    // instead of clobbering the typed input line.
    if let Ok(mut slot) = log_sink.0.lock() {
        *slot = Some(writer.clone());
    }
    if !messages.is_empty() {
        eprintln!("Resuming session ({} messages loaded)", messages.len());
    }
    // Register MCP before the first prompt (overlapped with readline setup).
    let _mcp = register_mcp(&mut tools, mcp_task, true).await;

    let mut repl_models: Vec<String> = Vec::new();
    // Images staged by `/attach` or `/paste`, flushed into the next prompt.
    let mut attached: Vec<sirbone::attachments::Attachment> = Vec::new();
    // `/oracle` toggle: post-Done test gate. Default OFF (ablations net-neutral/
    // negative); `/oracle` flips it on at runtime when a test command is configured.
    let mut oracle_on = oracle_requested;
    // Background-job completions: polled between readline events; the
    // SharedWriter prints above the prompt without clobbering typed input.
    let mut job_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        tokio::select! {
          _ = job_tick.tick() => {
            for (id, command, exit, dur) in tools.jobs.take_finished() {
                let mark = if exit == Some(0) { "✓" } else { "✗" };
                let code = exit.map_or_else(|| "?".into(), |c| c.to_string());
                use std::io::Write as _;
                let _ = writeln!(
                    writer,
                    "{mark} job #{id} finished · {}s · exit {code} — {command}",
                    dur.as_secs()
                );
            }
          }
          res = rl.readline() => match res {
            Ok(ReadlineEvent::Line(line)) => {
                let mut line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                // Slash commands. Unknown ones fall through and are sent as a message.
                let mut replacement_prompt = None;
                if let Some(cmd) = line.strip_prefix('/') {
                    let (name, arg) = cmd.split_once(' ').map_or((cmd, ""), |(n, a)| (n, a.trim()));
                    if name == "quit" {
                        break;
                    }
                    let handled = match name {
                        "model" => {
                            repl_model_command(arg, &client, &cwd, &mut model, &mut repl_models).await;
                            true
                        }
                        "attach" | "paste" => {
                            let dir = sirbone::attachments::dir_for(&session_path);
                            let label = format!("image_{}", attached.len() + 1);
                            let res = if name == "paste" {
                                sirbone::attachments::from_clipboard(&dir, label)
                            } else if arg.is_empty() {
                                Err(anyhow::anyhow!("usage: /attach PATH"))
                            } else {
                                sirbone::attachments::save_file(&dir, label, Path::new(arg))
                            };
                            match res {
                                Ok(att) => {
                                    println!("📎 {}", att.describe());
                                    if !sirbone::attachments::vision_supported() {
                                        eprintln!("warning: {}", sirbone::attachments::NO_VISION_WARNING);
                                    }
                                    attached.push(att);
                                }
                                Err(e) => eprintln!("{e}"),
                            }
                            true
                        }
                        "rollback" => { repl_rollback_command(arg).await; true }
                        "snapshots" => { repl_snapshots_command().await; true }
                        "fork" => {
                            match session::fork(&session_path).await {
                                Ok(new) => {
                                    session_path = new;
                                    println!("forked → {} (original frozen)", session_path.display());
                                }
                                Err(e) => eprintln!("fork failed: {e}"),
                            }
                            true
                        }
                        "tokens" => {
                            let mut msgs = vec![Message {
                                role: sirbone::Role::System,
                                injected: false,
                                content: vec![ContentBlock::Text { text: system_prompt.clone() }],
                            }];
                            msgs.extend(messages.iter().cloned());
                            match client.count_tokens(&msgs.iter().collect::<Vec<_>>(), &tools).await {
                                Ok(n) => println!("{n} tokens (system + tools + conversation)"),
                                Err(_) => println!(
                                    "~{} tokens (local estimate — provider count unavailable)",
                                    sirbone::agent::estimate_context_tokens(&msgs)
                                ),
                            }
                            true
                        }
                        "init" => { cmd::init_project(&cwd); true }
                        "plan" => {
                            plan_mode = !plan_mode;
                            if !plan_mode {
                                tools.notes.stop_plan();
                            }
                            println!("plan mode {} — tasks use a compact persistent contract", if plan_mode { "on" } else { "off" });
                            true
                        }
                        "oracle" => {
                            oracle_on = !oracle_on;
                            println!("oracle gate {}", if oracle_on { "on" } else { "off" });
                            true
                        }
                        "verify" => { println!("{}", sirbone::oracle::verify_once().await); true }
                        "historia" => {
                            let limit = if arg.is_empty() { 6 } else { 20 };
                            match tools.execute(
                                "historia",
                                serde_json::json!({
                                    "query": arg,
                                    "max_sessions": limit,
                                    "focus": "all"
                                }),
                            ).await {
                                Ok(history) => {
                                    replacement_prompt = Some(
                                        sirbone::tools::historia::continuation_prompt(arg, &history)
                                    );
                                    false
                                }
                                Err(e) => {
                                    eprintln!("historia failed: {e}");
                                    true
                                }
                            }
                        }
                        "clear" => {
                            messages.clear();
                            attached.clear();
                            session_path = session::new_session_path();
                            println!("cleared — new session: {}", session_path.display());
                            true
                        }
                        _ => false,
                    };
                    if handled { continue; }
                }
                if let Some(prompt) = replacement_prompt {
                    line = prompt;
                }
                if plan_mode {
                    tools.start_plan(&line);
                } else {
                    tools.notes.stop_plan();
                }
                let images = std::mem::take(&mut attached);
                let user_msg = Message {
                    role: sirbone::Role::User,
                    injected: false,
                    content: sirbone::attachments::user_content(&images, line),
                };
                session::append(&session_path, &SessionEntry::Message(user_msg.clone())).await?;
                messages.push(user_msg);
                println!();
                // Run the turn while still watching the keyboard: in raw mode Ctrl-C
                // does not raise SIGINT, so rustyline's `Interrupted` is the only abort
                // signal — cancel the in-flight inference when it fires. Destructive
                // confirmations are answered by this same loop (the readline owns the
                // terminal; a raw `stdin().read_line` would never see Enter's `\r`).
                let cancel = CancellationToken::new();
                let (ask_tx, mut ask_rx) = mpsc::channel::<Prompt>(1);
                let (reply_tx, reply_rx) = mpsc::channel::<PromptReply>(1);
                let bridge = ConfirmBridge { ask: ask_tx, reply: reply_rx };
                let turn = run_turn(&model, &client, &session_path, &system_prompt, &mut messages, &tools, true, false, false, Some(bridge), oracle_on, cancel.clone());
                tokio::pin!(turn);
                let mut confirm_pending: Option<Prompt> = None;
                loop {
                    tokio::select! {
                        res = &mut turn => { res?; break; }
                        Some(prompt) = ask_rx.recv() => {
                            use std::io::Write as _;
                            let _ = writeln!(writer, "\u{26a0} {}", prompt.title);
                            if let Some(detail) = &prompt.detail {
                                let preview: String = detail.chars().take(200).collect();
                                let _ = writeln!(writer, "   {preview}");
                            }
                            if let PromptKind::QuestionRound { questions } = &prompt.kind {
                                for (q, question) in questions.iter().enumerate() {
                                    let _ = writeln!(writer, "   {}. {}", q + 1, question.title);
                                    if let Some(detail) = &question.detail {
                                        let _ = writeln!(writer, "      {detail}");
                                    }
                                    for (i, option) in question.options.iter().enumerate() {
                                        let _ = writeln!(writer, "      {}. {option}", i + 1);
                                    }
                                }
                                let _ = writeln!(writer, "   Reply once with comma-separated choices, e.g. 1,2,1");
                            } else {
                                for (i, opt) in prompt.options.iter().enumerate() {
                                    let _ = writeln!(writer, "   {}. {opt}", i + 1);
                                }
                                if prompt.allow_free_text {
                                    let _ = writeln!(writer, "   {}. Other (type: {} <text>)", prompt.options.len() + 1, prompt.options.len() + 1);
                                }
                            }
                            if let PromptKind::Permission { suggested_glob } = &prompt.kind {
                                let _ = writeln!(writer, "   (allow-always rule: {suggested_glob} — override with: 2 <glob>)");
                            }
                            let _ = rl.update_prompt("choice> ");
                            confirm_pending = Some(prompt);
                        }
                        _ = job_tick.tick() => {
                            for (id, command, exit, dur) in tools.jobs.take_finished() {
                                let mark = if exit == Some(0) { "✓" } else { "✗" };
                                let code = exit.map_or_else(|| "?".into(), |c| c.to_string());
                                use std::io::Write as _;
                                let _ = writeln!(writer, "{mark} job #{id} finished · {}s · exit {code} — {command}", dur.as_secs());
                            }
                        }
                        ev = rl.readline() => match ev {
                            Ok(ReadlineEvent::Interrupted) => cancel.cancel(),
                            Ok(ReadlineEvent::Line(l)) if confirm_pending.is_some() => {
                                let prompt = confirm_pending.take().expect("pending checked");
                                let reply = parse_repl_choice(&l, &prompt);
                                let expected = match &prompt.kind {
                                    PromptKind::QuestionRound { questions } => Some(questions.len()),
                                    _ => None,
                                };
                                if expected.is_some_and(|count| !round_reply_complete(&reply, count)) {
                                    let _ = writeln!(writer, "   Invalid round: answer every question, separated by commas.");
                                    let _ = rl.update_prompt("choice> ");
                                    confirm_pending = Some(prompt);
                                } else {
                                    let _ = rl.update_prompt("> ");
                                    let _ = reply_tx.send(reply).await;
                                }
                            }
                            // Other lines/EOF typed mid-turn are ignored; abort with Ctrl-C.
                            _ => {}
                        }
                    }
                }
                // The reply streamed straight to stdout, burying the input line
                // rustyline drew when the user pressed Enter. Drop to a fresh line
                // (raw mode needs the `\r`) and repaint "> " at the bottom, so a
                // finished turn is unmistakable.
                use std::io::Write as _;
                print!("\r\n");
                let _ = std::io::stdout().flush();
                let _ = rl.update_prompt("> ");
            }
            Ok(ReadlineEvent::Eof) | Ok(ReadlineEvent::Interrupted) => break,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
          }
        }
    }
    Ok(())
}

/// Parse a REPL prompt answer: a leading option number (1-based) and optional
/// trailing text (an edited allow-glob, deny feedback, or the "Other" value).
/// Anything unrecognized (or empty) denies — the safe default.
fn parse_repl_choice(line: &str, prompt: &Prompt) -> PromptReply {
    if let PromptKind::QuestionRound { questions } = &prompt.kind {
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() != questions.len() {
            return PromptReply::default();
        }
        let answers = parts
            .into_iter()
            .zip(questions)
            .map(|(part, question)| {
                parse_choice(part, &question.options, question.allow_free_text, true)
            })
            .collect();
        return PromptReply {
            answers,
            ..PromptReply::default()
        };
    }
    let answer = parse_choice(
        line,
        &prompt.options,
        prompt.allow_free_text,
        matches!(prompt.kind, PromptKind::Question),
    );
    PromptReply {
        index: answer.index,
        text: answer.text,
        ..PromptReply::default()
    }
}

fn round_reply_complete(reply: &PromptReply, expected: usize) -> bool {
    reply.answers.len() == expected
        && reply.answers.iter().all(|answer| {
            answer.index.is_some()
                || answer
                    .text
                    .as_deref()
                    .is_some_and(|text| !text.trim().is_empty())
        })
}

fn parse_choice(
    line: &str,
    options: &[String],
    allow_free_text: bool,
    allow_bare_free_text: bool,
) -> sirbone::agent::PromptAnswer {
    let line = line.trim();
    let (head, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let text = {
        let t = rest.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    let n_opts = options.len();
    match head.parse::<usize>() {
        Ok(i) if (1..=n_opts).contains(&i) => sirbone::agent::PromptAnswer {
            index: Some(i - 1),
            text,
        },
        Ok(i) if allow_free_text && i == n_opts + 1 => {
            sirbone::agent::PromptAnswer { index: None, text }
        }
        _ if allow_bare_free_text && !line.is_empty() => sirbone::agent::PromptAnswer {
            index: None,
            text: Some(line.to_string()),
        },
        _ => sirbone::agent::PromptAnswer::default(),
    }
}

/// `/rollback` for the readline REPL: no arg lists snapshots, `<n|id>` restores.
async fn repl_rollback_command(arg: &str) {
    let Some(snaps) = sirbone::snapshot::workspace_snapshots() else {
        println!("snapshots disabled (SIRBONE_NO_SNAPSHOT)");
        return;
    };
    if arg.is_empty() {
        let entries = snaps.list_detailed(10).await;
        if entries.is_empty() {
            println!("no snapshots yet (one is taken before each run that edits files)");
            return;
        }
        for (i, entry) in entries.iter().enumerate() {
            println!(
                "{:>2}. {}  {}  — {}",
                i + 1,
                entry.short_id,
                entry.age,
                entry.label
            );
            for file in &entry.changed_files {
                println!("    {file}");
            }
        }
        println!("restore with /rollback <n|id>");
        return;
    }
    match snaps.rollback(arg).await {
        Ok(msg) => println!("{msg}"),
        Err(e) => eprintln!("rollback failed: {e}"),
    }
}

async fn repl_snapshots_command() {
    repl_rollback_command("").await;
}

/// `/model` for the readline REPL: no arg lists models (cached for index
/// selection), `<n>`/`<name>` switches.
async fn repl_model_command(
    arg: &str,
    client: &Arc<dyn LlmClient>,
    cwd: &Path,
    model: &mut String,
    cached: &mut Vec<String>,
) {
    if arg.is_empty() {
        match client.list_models().await {
            Ok(models) if !models.is_empty() => {
                println!("Models (current: {model}):");
                for (i, m) in models.iter().enumerate() {
                    let mark = if m == model { "*" } else { " " };
                    println!("  {:>2} {mark} {m}", i + 1);
                }
                println!("select with /model <n|name>");
                *cached = models;
            }
            Ok(_) => eprintln!("no models listed — use /model <name>"),
            Err(e) => eprintln!("model listing unavailable ({e}) — use /model <name>"),
        }
        return;
    }
    let name = match arg.parse::<usize>() {
        Ok(n) => match cached.get(n.wrapping_sub(1)) {
            Some(m) => m.clone(),
            None => {
                eprintln!("invalid index '{arg}'");
                return;
            }
        },
        Err(_) => arg.to_string(),
    };
    sirbone::agent::switch_model(client.as_ref(), cwd, name.clone());
    *model = name;
    println!("model → {model}");
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    model: &str,
    client: &Arc<dyn LlmClient>,
    session_path: &Path,
    system_prompt: &str,
    messages: &mut Vec<Message>,
    tools: &ToolRegistry,
    interactive: bool,
    // Quiet run: no streaming render (stdout stays clean for `--output-format json`);
    // usage accounting and session persistence still happen.
    quiet: bool,
    // Emit one NDJSON event per line to stdout as events arrive (`--output-format
    // stream-json`). Implies quiet (no ANSI). Accounting/persistence unchanged.
    stream: bool,
    // Destructive-command confirmations. `None` (one-shot / piped input) auto-denies;
    // the REPL passes a bridge answered by its own readline loop — never a raw
    // `stdin().read_line`, which deadlocks under rustyline's raw mode (Enter = `\r`).
    confirm: Option<ConfirmBridge>,
    oracle: bool,
    cancel: CancellationToken,
) -> Result<UsageTotals> {
    let n_before = messages.len();
    let (tx, rx) = mpsc::channel::<AgentEvent>(64);
    let cancel_c = cancel.clone();
    let cancel_for_status = cancel.clone();
    let ctrl_c = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        cancel_c.cancel();
    });

    let print_task = tokio::spawn(render_events(
        rx,
        session_path.to_path_buf(),
        tools.mcp_schema_cost(),
        tools.native_schema_cost(),
        interactive,
        quiet,
        stream,
    ));

    // Fingerprint the cacheable prefix before the first turn, so a later audit
    // can tell a provider-side cache miss from a prefix sirbone itself changed.
    if let Err(e) =
        session::append_request_header(session_path, model, Some(system_prompt), tools).await
    {
        tracing::warn!("session request header append: {e}");
    }

    let run_result = {
        let context_window = client.context_window().await.map(|n| n as usize);
        let mut ctx = AgentContext {
            model: model.to_string(),
            system_prompt: Some(system_prompt.to_string()),
            messages: std::mem::take(messages),
            tools: tools.clone(),
            client: Arc::clone(client),
            events: tx,
            cancel,
            context_window,
            confirm,
            compaction_keep_recent: None,
            permissions: sirbone::PermissionConfig::load(),
            snapshots: sirbone::snapshot::workspace_snapshots(),
            hooks: sirbone::checks::Hooks::load(),
            oracle: oracle.then(sirbone::oracle::Oracle::load).flatten(),
            max_steps: sirbone::agent::env_max_steps(),
            spend_cap: sirbone::config::spend_cap(),
            tokens_spent: 0,
            stream_rules: sirbone::stream_rules::install(client.as_ref()),
            compacted_files: Vec::new(),
            last_request: None,
        };
        let r = sirbone::run(&mut ctx).await;
        *messages = std::mem::take(&mut ctx.messages);
        r
    }; // ctx (and tx) dropped -> print_task drains and finishes

    ctrl_c.abort();
    let (compaction_base, usage) = print_task.await??;

    // After a mid-run compaction the session already holds a Compaction
    // checkpoint covering everything up to `compaction_base`; append only the tail.
    let persist_from = compaction_base.unwrap_or(n_before);
    for msg in messages.get(persist_from..).unwrap_or(&[]) {
        session::append(session_path, &SessionEntry::Message(msg.clone())).await?;
    }

    let (status, reason) = if cancel_for_status.is_cancelled() {
        ("cancelled", Some("cancelled by user".to_string()))
    } else if let Err(e) = &run_result {
        ("error", Some(e.to_string()))
    } else {
        ("done", None)
    };
    session::append(
        session_path,
        &SessionEntry::RunStatus {
            status: status.to_string(),
            reason,
        },
    )
    .await?;

    run_result.map(|()| usage)
}

/// Resolve the best-of-K plan for this run, printing the reason whenever the
/// flag is set but the feature cannot run.
///
/// Every refusal is loud on purpose: an opt-in scaling flag that silently does
/// nothing would reach a bench report as a measured null result.
fn best_of_plan(has_confirm_bridge: bool) -> Option<(sirbone::best_of::BestOf, Arc<Snapshots>)> {
    let best = match sirbone::best_of::BestOf::load() {
        Ok(Some(b)) => b,
        Ok(None) => return None,
        Err(why) => {
            eprintln!("warning: {why} — running a single attempt");
            return None;
        }
    };
    // Each attempt must start from the tree the previous one started from, and
    // that is exactly what a snapshot rollback gives. Without it, attempt 2 would
    // build on attempt 1's edits: a repair pass, not an independent sample, and
    // the two answer different questions.
    let Some(snapshots) = sirbone::snapshot::workspace_snapshots() else {
        eprintln!(
            "warning: SIRBONE_BEST_OF needs workspace snapshots (unset SIRBONE_NO_SNAPSHOT) \
             — running a single attempt"
        );
        return None;
    };
    if has_confirm_bridge {
        // The bridge owns a receiver and cannot be handed to a second attempt.
        // Passing `None` instead would quietly downgrade attempt 2 to auto-deny,
        // which is a change of permission semantics mid-run.
        eprintln!(
            "warning: SIRBONE_BEST_OF is not supported with an interactive confirm bridge \
             (--input-format stream-json) — running a single attempt"
        );
        return None;
    }
    Some((best, snapshots))
}

/// Session file for attempt N>1.
///
/// Each attempt writes its own file. A resumed session must not replay the
/// messages of a discarded attempt: its edits are no longer in the tree, so the
/// model would be reading a transcript of work that does not exist.
fn attempt_session(base: &Path, attempt: usize) -> PathBuf {
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    base.with_file_name(format!("{stem}.k{attempt}.jsonl"))
}

/// Run the task up to K times from the same starting tree and keep the attempt
/// the project's own test command scores best (`SIRBONE_BEST_OF`, see
/// `sirbone::best_of`).
///
/// `messages` and `session_path` come back as the winner's, and the work-tree is
/// restored to the winner's state. The usage returned is the **sum** over every
/// attempt, including the discarded ones: reporting only the winner's would hide
/// exactly the cost this feature spends.
#[allow(clippy::too_many_arguments)]
async fn run_best_of(
    best: sirbone::best_of::BestOf,
    snapshots: Arc<Snapshots>,
    model: &str,
    client: &Arc<dyn LlmClient>,
    session_path: &mut PathBuf,
    system_prompt: &str,
    messages: &mut Vec<Message>,
    tools: &ToolRegistry,
    quiet: bool,
    stream: bool,
    oracle: bool,
) -> Result<UsageTotals> {
    /// The best attempt so far. `tree` is the snapshot holding its work-tree,
    /// captured before the next attempt rolls back over it.
    struct Attempt {
        n: usize,
        failed: usize,
        verdict: String,
        session: PathBuf,
        messages: Vec<Message>,
        tree: String,
    }

    let base_tree = snapshots
        .snapshot_id("best-of: starting tree")
        .await
        .context("best-of needs a snapshot of the starting tree")?;
    let base_messages = messages.clone();
    let base_session = session_path.clone();

    let mut total = UsageTotals::default();
    let mut winner: Option<Attempt> = None;
    let mut attempts_run = 0usize;
    let mut last_err = None;

    for attempt in 1..=best.k {
        if attempt > 1 {
            snapshots.rollback(&base_tree).await?;
            *messages = base_messages.clone();
            *session_path = attempt_session(&base_session, attempt);
        }
        sirbone::telemetry::add(&sirbone::telemetry::BEST_OF_ATTEMPTS, 1);
        match run_turn(
            model,
            client,
            session_path,
            system_prompt,
            messages,
            tools,
            false,
            quiet,
            stream,
            None,
            oracle,
            CancellationToken::new(),
        )
        .await
        {
            Ok(usage) => {
                attempts_run += 1;
                total = total.plus(usage);
            }
            // A provider error is infrastructure, not a bad patch: stop sampling
            // and select among the attempts that did finish. Only if none did is
            // the error the run's answer.
            Err(e) => {
                eprintln!("[best-of] attempt {attempt}/{} failed: {e}", best.k);
                last_err = Some(e);
                break;
            }
        }
        let score = best.score().await;
        let tree = snapshots
            .snapshot_id(&format!("best-of: attempt {attempt}"))
            .await?;
        let won = match &winner {
            None => true,
            Some(w) => {
                let incumbent = sirbone::oracle::OracleResult {
                    passed: w.failed == 0,
                    failed: w.failed,
                    raw: String::new(),
                };
                sirbone::best_of::improves(&score, &incumbent)
            }
        };
        let verdict = verdict(&score);
        eprintln!(
            "[best-of] attempt {attempt}/{}: {verdict} — {}",
            best.k,
            if won { "selected" } else { "discarded" }
        );
        if won {
            if attempt > 1 {
                sirbone::telemetry::add(&sirbone::telemetry::BEST_OF_SELECTIONS, 1);
            }
            winner = Some(Attempt {
                n: attempt,
                failed: score.failed,
                verdict,
                session: session_path.clone(),
                messages: messages.clone(),
                tree,
            });
        }
        // Nothing can beat a green tree, and another attempt would only spend
        // quota to tie it.
        if score.passed {
            break;
        }
    }

    let Some(win) = winner else {
        return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("best-of: no attempt completed")));
    };
    if *session_path != win.session {
        snapshots.rollback(&win.tree).await?;
        *session_path = win.session;
        *messages = win.messages;
    }
    // The `[usage]` line and the session telemetry record are both written inside
    // `run_turn`, so they close before this loop knows which attempt won: a
    // selection scored afterwards reaches neither. Flush it into the winner's
    // session file, or `sirbone stats` would report attempts that never selected.
    if win.n > 1 {
        session::append_run_telemetry(session_path).await?;
    }
    eprintln!(
        "[best-of] winner = attempt {}/{} ({}), {attempts_run} attempt(s) run; the `[usage]` lines above are per attempt, the run's total is the `usage` object",
        win.n, best.k, win.verdict
    );
    Ok(total)
}

/// One-line reading of a score for the progress log.
fn verdict(score: &sirbone::oracle::OracleResult) -> String {
    match (score.passed, score.failed) {
        (true, _) => "all tests pass".into(),
        (false, usize::MAX) => "test command did not complete".into(),
        (false, n) => format!("{n} test(s) failing"),
    }
}

/// Per-run token/call accounting, returned by `run_turn` (and serialized as the
/// `usage` field of `--output-format json`).
#[derive(Default, Clone, Copy, serde::Serialize)]
struct UsageTotals {
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    peak_context: u32,
    edit_fails: u64,
    tool_calls: u64,
    tool_errors: u64,
}

impl UsageTotals {
    /// Sum across best-of attempts. Every counter adds — the discarded attempts
    /// were paid for — except `peak_context`, which is a high-water mark.
    fn plus(self, o: Self) -> Self {
        Self {
            calls: self.calls + o.calls,
            input_tokens: self.input_tokens + o.input_tokens,
            output_tokens: self.output_tokens + o.output_tokens,
            cached_tokens: self.cached_tokens + o.cached_tokens,
            peak_context: self.peak_context.max(o.peak_context),
            edit_fails: self.edit_fails + o.edit_fails,
            tool_calls: self.tool_calls + o.tool_calls,
            tool_errors: self.tool_errors + o.tool_errors,
        }
    }
}

/// Serialize a [`Prompt`] for the stream-json `ask` event.
fn prompt_to_json(p: &Prompt) -> serde_json::Value {
    let (kind, questions) = match &p.kind {
        PromptKind::Permission { suggested_glob } => (
            serde_json::json!({ "type": "permission", "suggested_glob": suggested_glob }),
            None,
        ),
        PromptKind::Question => (serde_json::json!({ "type": "question" }), None),
        PromptKind::QuestionRound { questions } => (
            serde_json::json!({ "type": "question_round" }),
            Some(
                questions
                    .iter()
                    .map(|q| {
                        serde_json::json!({
                            "id": q.id,
                            "title": q.title,
                            "detail": q.detail,
                            "options": q.options,
                            "allow_free_text": q.allow_free_text,
                        })
                    })
                    .collect::<Vec<_>>(),
            ),
        ),
    };
    serde_json::json!({
        "title": p.title,
        "detail": p.detail,
        "options": p.options,
        "allow_free_text": p.allow_free_text,
        "kind": kind,
        "questions": questions,
    })
}

/// Parse one stdin control line into a [`PromptReply`] for the pending ask `id`.
/// Returns `None` for non-reply lines or an id mismatch, so unrelated NDJSON is
/// skipped. A missing `id` matches any pending prompt (single-prompt front-ends).
fn parse_stdin_reply(line: &str, expect_id: u64) -> Option<PromptReply> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("reply") {
        return None;
    }
    if let Some(id) = v.get("id").and_then(serde_json::Value::as_u64) {
        if id != expect_id {
            return None;
        }
    }
    Some(PromptReply {
        index: v
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .map(|i| i as usize),
        text: v.get("text").and_then(|t| t.as_str()).map(str::to_string),
        answers: v
            .get("answers")
            .and_then(serde_json::Value::as_array)
            .map(|answers| {
                answers
                    .iter()
                    .map(|answer| sirbone::agent::PromptAnswer {
                        index: answer
                            .get("index")
                            .and_then(serde_json::Value::as_u64)
                            .map(|i| i as usize),
                        text: answer
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Interactive prompt bridge for `-p --input-format stream-json`: the agent's
/// prompts are emitted as `{type:"ask",id,prompt}` NDJSON on stdout and the
/// front-end's reply is read from stdin as `{type:"reply",id,index?,text?}`.
/// Stdin is read on a blocking thread (the positional prompt means stdin is free
/// as a control channel); a closed stdin denies the pending prompt.
fn spawn_stdio_prompt_bridge() -> ConfirmBridge {
    let (ask_tx, mut ask_rx) = mpsc::channel::<Prompt>(1);
    let (reply_tx, reply_rx) = mpsc::channel::<PromptReply>(1);
    let (line_tx, mut line_rx) = mpsc::channel::<String>(8);
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::stdin().lock().lines() {
            let Ok(l) = line else { break };
            if line_tx.blocking_send(l).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut counter: u64 = 0;
        while let Some(prompt) = ask_rx.recv().await {
            counter += 1;
            let obj = serde_json::json!({
                "type": "ask", "id": counter, "prompt": prompt_to_json(&prompt),
            });
            {
                use std::io::Write as _;
                println!("{obj}");
                let _ = std::io::stdout().flush();
            }
            let reply = loop {
                match line_rx.recv().await {
                    Some(line) => {
                        if let Some(r) = parse_stdin_reply(&line, counter) {
                            break r;
                        }
                    }
                    None => break PromptReply::default(), // stdin closed -> deny
                }
            };
            if reply_tx.send(reply).await.is_err() {
                break;
            }
        }
    });
    ConfirmBridge {
        ask: ask_tx,
        reply: reply_rx,
    }
}

/// Serialize an agent event to one NDJSON line on stdout for `--output-format
/// stream-json`. Only the events a live front-end (the editor UI) needs are
/// emitted; the rest are dropped. Flushed per line so the consumer sees them as
/// they happen rather than block-buffered behind the pipe.
fn stream_emit(ev: &AgentEvent) {
    use AgentEvent::*;
    let obj = match ev {
        TextChunk(s) => serde_json::json!({ "type": "text", "text": s }),
        ThinkingChunk(s) => serde_json::json!({ "type": "thinking", "text": s }),
        ToolCallStart { id, name, input } => {
            serde_json::json!({ "type": "tool_start", "id": id, "name": name, "input": input })
        }
        ToolCallEnd {
            id,
            result,
            is_error,
            ..
        } => {
            serde_json::json!({ "type": "tool_end", "id": id, "is_error": is_error, "content": result })
        }
        ContextUsage {
            used_tokens,
            context_window,
            output_tokens,
            ..
        } => {
            serde_json::json!({ "type": "ctx", "used": used_tokens, "window": context_window, "output": output_tokens })
        }
        Error(s) => serde_json::json!({ "type": "error", "text": s }),
        Notice { text, .. } => serde_json::json!({ "type": "notice", "text": text }),
        _ => return,
    };
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{obj}");
    let _ = out.flush();
}

/// Render events to the terminal. Returns the transcript length recorded by the
/// last `Compacted` event, if any (so the caller can fix its append bookkeeping),
/// plus the run's usage totals. `quiet` skips all terminal output but keeps
/// accounting and session persistence.
async fn render_events(
    mut rx: mpsc::Receiver<AgentEvent>,
    session_path: PathBuf,
    mcp_cost: (usize, usize),
    native_cost: (usize, usize),
    raw: bool,
    quiet: bool,
    stream: bool,
) -> Result<(Option<usize>, UsageTotals)> {
    let mut r = render::Renderer::new(raw);
    let mut ctx_warned = false;
    let mut compaction_base = None;
    // Token accounting (opt-in via SIRBONE_USAGE=1): sum the real per-call prompt
    // size and cache hits across the run; the bench harness parses the final line.
    let (mut usage_calls, mut usage_input, mut usage_cached, mut usage_peak) =
        (0u64, 0u64, 0u64, 0u32);
    let mut usage_output = 0u64;
    let (mut usage_tool_calls, mut usage_tool_errors) = (0u64, 0u64);
    // Failed edit calls — the bench's mechanism metric for the edit-hint arm.
    let mut usage_edit_fails = 0u64;
    // Drive a braille spinner on an interval during the dead air between the
    // model's turns and while tools run; `biased` drains real events first so the
    // spinner never delays output. The Renderer no-ops the spinner off a tty.
    let mut spin = tokio::time::interval(std::time::Duration::from_millis(80));
    loop {
        let ev = tokio::select! {
            biased;
            maybe = rx.recv() => match maybe { Some(ev) => ev, None => break },
            _ = spin.tick() => { if !quiet { r.spin_tick(); } continue; }
        };
        // stream-json: emit this event as one NDJSON line for a live front-end
        // (the extension). Accounting/persistence in the arms below still run.
        if stream {
            stream_emit(&ev);
        }
        // Flush any buffered prose/code line before a non-text event renders,
        // so streaming output is not stranded behind tool boxes or notices.
        if !quiet && !matches!(ev, AgentEvent::TextChunk(_)) {
            r.flush_text();
        }
        // Quiet mode: skip pure-display events; the arms below keep accounting
        // (usage, edit_fails) and session side effects, guarding only the render.
        if quiet
            && matches!(
                ev,
                AgentEvent::TextChunk(_)
                    | AgentEvent::Notice { .. }
                    | AgentEvent::Cancelled
                    | AgentEvent::TurnStart
                    | AgentEvent::TurnEnd
                    | AgentEvent::ThinkingStart
            )
        {
            continue;
        }
        match ev {
            AgentEvent::TextChunk(s) => {
                r.spin_stop();
                r.text(&s);
            }
            AgentEvent::ToolCallStart { name, .. } => {
                if !quiet {
                    r.tool_start(&name);
                    r.spin_start("running");
                }
            }
            AgentEvent::ToolCallEnd {
                name,
                result,
                is_error,
                ..
            } => {
                usage_tool_calls += 1;
                if is_error {
                    usage_tool_errors += 1;
                }
                if is_error && name == "edit" {
                    usage_edit_fails += 1;
                }
                if !quiet {
                    r.tool_end(&name, &result, is_error);
                    r.spin_start("thinking");
                }
            }
            AgentEvent::Error(e) => {
                if quiet {
                    // stdout is reserved for the JSON object; errors still surface.
                    eprintln!("error: {e}");
                } else {
                    r.spin_stop();
                    r.error(&e);
                }
            }
            AgentEvent::Notice { text, level } => r.notice(&text, level),
            AgentEvent::Cancelled => {
                r.spin_stop();
                r.cancelled();
            }
            AgentEvent::Compacted { messages } => {
                compaction_base = Some(messages.len());
                session::append(&session_path, &SessionEntry::Compaction { messages }).await?;
            }
            AgentEvent::WorkspaceSnapshot { id, label } => {
                session::append(
                    &session_path,
                    &SessionEntry::WorkspaceSnapshot { id, label },
                )
                .await?;
            }
            AgentEvent::ContextUsage {
                used_tokens,
                context_window,
                cached_tokens,
                output_tokens,
            } => {
                usage_output += output_tokens as u64;
                // Gate on >0: z.ai emits a zero usage at message_start and the real
                // figure at message_delta — counting both would double the call count.
                if used_tokens > 0 {
                    usage_calls += 1;
                    usage_input += used_tokens as u64;
                    usage_cached += cached_tokens as u64;
                    usage_peak = usage_peak.max(used_tokens);
                }
                if used_tokens > 0 {
                    let pct =
                        ((used_tokens as u64 * 100) / context_window.max(1) as u64).min(100) as u8;
                    // One-shot context-rot warning at the amber threshold; rearms
                    // when usage drops back (compaction).
                    if pct >= 70 && !ctx_warned {
                        ctx_warned = true;
                        if !quiet {
                            r.ctx_warning(pct, context_window);
                        }
                    } else if pct < 70 {
                        ctx_warned = false;
                    }
                }
            }
            AgentEvent::TurnStart | AgentEvent::ThinkingStart => r.spin_start("thinking"),
            AgentEvent::TurnEnd => r.spin_stop(),
            AgentEvent::ThinkingChunk(_)
            | AgentEvent::JobDone { .. }
            | AgentEvent::SpendUsage { .. } => {}
        }
    }
    r.flush_text();
    if usage_calls > 0 {
        session::append(
            &session_path,
            &SessionEntry::RunUsage {
                input_tokens: usage_input,
                cached_tokens: usage_cached,
                peak_context_tokens: usage_peak,
            },
        )
        .await?;
        session::append_run_telemetry(&session_path).await?;
    }
    if std::env::var_os("SIRBONE_USAGE").is_some() {
        let (mcp_tools, mcp_schema_tokens) = mcp_cost;
        let (native_tools, native_schema_tokens) = native_cost;
        // Feature-attribution counters (see `sirbone::telemetry`): cumulative for
        // the process, which equals per-run in the bench's one-task-per-process mode.
        use sirbone::telemetry as tm;
        eprintln!(
            "[usage] calls={usage_calls} input_tokens={usage_input} output_tokens={usage_output} cached_tokens={usage_cached} peak_context={usage_peak} tool_calls={usage_tool_calls} tool_errors={usage_tool_errors} edit_fails={usage_edit_fails} mcp_tools={mcp_tools} mcp_schema_tokens={mcp_schema_tokens} native_tools={native_tools} native_schema_tokens={native_schema_tokens} compaction_fired={} historia_writes={} historia_hits={} system_prompt_tokens={} hook_pre_runs={} hook_pre_denies={} hook_post_runs={} hook_post_failures={} hook_stop_runs={} hook_stop_retries={} hook_stop_exhausted={} oracle_runs={} oracle_failures={} oracle_retries={} oracle_rollbacks={} oracle_exhausted={} ask_user_rounds={} ask_user_questions={} verify_tool_runs={} spill_writes={} read_outlines={} patch_applies={} patch_rejects={} stream_rule_trips={} plan_contract_initialized={} plan_contract_updated={} plan_mutations_blocked={} tool_batches={} tool_calls_emitted={} completion_checks_fired={} test_file_mutations={} best_of_attempts={} best_of_selections={} permission_denies_policy={} permission_denies_user={} permission_denies_unattended={} permission_bypassed={} tool_calls_dispatched={} tusk_runs={} tusk_edits={} tusk_withheld={}",
            tm::get(&tm::COMPACTION_FIRED),
            tm::get(&tm::HISTORIA_WRITES),
            tm::get(&tm::HISTORIA_HITS),
            tm::system_prompt_tokens(),
            tm::get(&tm::HOOK_PRE_RUNS), tm::get(&tm::HOOK_PRE_DENIES),
            tm::get(&tm::HOOK_POST_RUNS), tm::get(&tm::HOOK_POST_FAILURES),
            tm::get(&tm::HOOK_STOP_RUNS), tm::get(&tm::HOOK_STOP_RETRIES), tm::get(&tm::HOOK_STOP_EXHAUSTED),
            tm::get(&tm::ORACLE_RUNS), tm::get(&tm::ORACLE_FAILURES), tm::get(&tm::ORACLE_RETRIES), tm::get(&tm::ORACLE_ROLLBACKS), tm::get(&tm::ORACLE_EXHAUSTED),
            tm::get(&tm::ASK_USER_ROUNDS), tm::get(&tm::ASK_USER_QUESTIONS), tm::get(&tm::VERIFY_TOOL_RUNS),
            tm::get(&tm::SPILL_WRITES), tm::get(&tm::READ_OUTLINES),
            tm::get(&tm::PATCH_APPLIES), tm::get(&tm::PATCH_REJECTS), tm::get(&tm::STREAM_RULE_TRIPS),
            tm::get(&tm::PLAN_CONTRACT_INITIALIZED), tm::get(&tm::PLAN_CONTRACT_UPDATED), tm::get(&tm::PLAN_MUTATIONS_BLOCKED),
            tm::get(&tm::TOOL_BATCHES), tm::get(&tm::TOOL_CALLS_EMITTED), tm::get(&tm::COMPLETION_CHECKS_FIRED), tm::get(&tm::TEST_FILE_MUTATIONS),
            tm::get(&tm::BEST_OF_ATTEMPTS), tm::get(&tm::BEST_OF_SELECTIONS),
            tm::get(&tm::PERMISSION_DENIES_POLICY), tm::get(&tm::PERMISSION_DENIES_USER), tm::get(&tm::PERMISSION_DENIES_UNATTENDED),
            tm::get(&tm::PERMISSION_BYPASSED), tm::get(&tm::TOOL_CALLS_DISPATCHED),
            tm::get(&tm::TUSK_RUNS), tm::get(&tm::TUSK_EDITS), tm::get(&tm::TUSK_WITHHELD),
        );
    }
    Ok((
        compaction_base,
        UsageTotals {
            calls: usage_calls,
            input_tokens: usage_input,
            output_tokens: usage_output,
            cached_tokens: usage_cached,
            peak_context: usage_peak,
            edit_fails: usage_edit_fails,
            tool_calls: usage_tool_calls,
            tool_errors: usage_tool_errors,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A discarded attempt must not end up in the session a resume would read,
    /// so every attempt after the first writes its own file next to the base.
    #[test]
    fn each_best_of_attempt_gets_its_own_session_file() {
        let base = std::path::Path::new("/s/1f0c-9a.jsonl");
        assert_eq!(
            attempt_session(base, 2),
            std::path::Path::new("/s/1f0c-9a.k2.jsonl")
        );
        assert_eq!(
            attempt_session(base, 3),
            std::path::Path::new("/s/1f0c-9a.k3.jsonl")
        );
    }

    /// Usage is summed over every attempt, discarded ones included: the cost of
    /// sampling is the whole point of the metric it feeds. `peak_context` is the
    /// exception, being a high-water mark rather than a total.
    #[test]
    fn best_of_usage_sums_attempts_but_keeps_peak_as_a_maximum() {
        let a = UsageTotals {
            calls: 3,
            input_tokens: 100,
            peak_context: 40_000,
            tool_calls: 5,
            ..Default::default()
        };
        let b = UsageTotals {
            calls: 4,
            input_tokens: 250,
            peak_context: 12_000,
            tool_calls: 2,
            ..Default::default()
        };
        let sum = a.plus(b);
        assert_eq!(
            (
                sum.calls,
                sum.input_tokens,
                sum.tool_calls,
                sum.peak_context
            ),
            (7, 350, 7, 40_000)
        );
    }

    #[test]
    fn one_shot_historia_command_parses_only_the_exact_slash_command() {
        assert_eq!(historia_command_query("/historia"), Some(""));
        assert_eq!(
            historia_command_query("/historia 2026-08-11 parser"),
            Some("2026-08-11 parser")
        );
        assert_eq!(historia_command_query("/historia\tparser"), Some("parser"));
        assert_eq!(historia_command_query("/historiador"), None);
        assert_eq!(historia_command_query("continue historia"), None);
    }

    /// Prompt ablation: `prompt:*` must strip every sirbone-authored block while
    /// keeping identity and the user's CLAUDE.md, and each named block must be
    /// removable on its own. Setting `SIRBONE_DISABLE=prompt:…` is inert for the
    /// other readers (`disabled_tool`/`disabled_skill`/`cache_disabled` match on
    /// their own kind), so this cannot disturb tests running in parallel.
    #[test]
    fn prompt_ablation_strips_blocks_and_keeps_identity() {
        let cwd = std::path::Path::new(".");
        let claude_md = "PROJECT RULE: never touch vendor/.";
        let full = build_system_prompt(cwd, claude_md);
        assert!(full.contains("<investigate_before_answering>"));
        assert!(full.contains("Report results truthfully"));

        std::env::set_var("SIRBONE_DISABLE", "prompt:*");
        let naked = build_system_prompt(cwd, claude_md);
        std::env::remove_var("SIRBONE_DISABLE");

        // Identity and the user's own instructions survive; our blocks do not.
        assert!(naked.contains("You are a helpful coding assistant"));
        assert!(naked.contains(claude_md));
        assert!(!naked.contains("<investigate_before_answering>"));
        assert!(!naked.contains("Report results truthfully"));
        assert!(!naked.contains("Do what was asked; nothing more"));
        assert!(!naked.contains("Ground every claim in the primary source"));
        assert!(naked.len() < full.len() / 2, "naked should be far smaller");

        // One block at a time: only the named block goes.
        std::env::set_var("SIRBONE_DISABLE", "prompt:truthful");
        let one = build_system_prompt(cwd, claude_md);
        std::env::remove_var("SIRBONE_DISABLE");
        assert!(!one.contains("Report results truthfully"));
        assert!(one.contains("<investigate_before_answering>"));
    }

    fn perm_prompt() -> Prompt {
        Prompt {
            title: "permission required".into(),
            detail: Some("rm -rf build/".into()),
            options: vec!["Allow once".into(), "Allow always".into(), "Deny".into()],
            allow_free_text: true,
            kind: PromptKind::Permission {
                suggested_glob: "Bash(rm -rf build/)".into(),
            },
        }
    }

    #[test]
    fn repl_choice_parses_index_and_trailing_text() {
        let p = perm_prompt();
        // Bare number → that option, no text.
        let r = parse_repl_choice("1", &p);
        assert_eq!(r.index, Some(0));
        assert_eq!(r.text, None);
        // Allow-always with an edited glob.
        let r = parse_repl_choice("2 Bash(rm -rf build/*)", &p);
        assert_eq!(r.index, Some(1));
        assert_eq!(r.text.as_deref(), Some("Bash(rm -rf build/*)"));
        // The "Other" row (options.len()+1) → index None, text carried.
        let r = parse_repl_choice("4 do it differently", &p);
        assert_eq!(r.index, None);
        assert_eq!(r.text.as_deref(), Some("do it differently"));
        // Empty / garbage → deny (safe default).
        assert_eq!(parse_repl_choice("", &p).index, None);
        assert_eq!(parse_repl_choice("nope", &p).index, None);
    }

    #[test]
    fn stdin_reply_matches_id_and_filters_noise() {
        // Matching id → parsed.
        let r = parse_stdin_reply(r#"{"type":"reply","id":3,"index":1}"#, 3).unwrap();
        assert_eq!(r.index, Some(1));
        // Id mismatch → skipped.
        assert!(parse_stdin_reply(r#"{"type":"reply","id":2,"index":1}"#, 3).is_none());
        // Missing id → matches any pending prompt.
        let r = parse_stdin_reply(r#"{"type":"reply","text":"polars"}"#, 7).unwrap();
        assert_eq!(r.text.as_deref(), Some("polars"));
        assert_eq!(r.index, None);
        // Non-reply NDJSON (e.g. an echoed event) → ignored.
        assert!(parse_stdin_reply(r#"{"type":"text","text":"hi"}"#, 1).is_none());
        assert!(parse_stdin_reply("not json", 1).is_none());
    }

    #[test]
    fn prompt_json_round_trips_kind() {
        let v = prompt_to_json(&perm_prompt());
        assert_eq!(v["kind"]["type"], "permission");
        assert_eq!(v["kind"]["suggested_glob"], "Bash(rm -rf build/)");
        assert_eq!(v["allow_free_text"], true);
        assert_eq!(v["options"][1], "Allow always");
    }

    fn round_prompt() -> Prompt {
        Prompt {
            title: "2 questions".into(),
            detail: Some("Submit once".into()),
            options: Vec::new(),
            allow_free_text: false,
            kind: PromptKind::QuestionRound {
                questions: vec![
                    sirbone::agent::PromptQuestion {
                        id: "db".into(),
                        title: "Database?".into(),
                        detail: Some("Local fixture".into()),
                        options: vec!["SQLite".into(), "Postgres".into()],
                        allow_free_text: true,
                    },
                    sirbone::agent::PromptQuestion {
                        id: "format".into(),
                        title: "Format?".into(),
                        detail: None,
                        options: vec!["JSON".into(), "CSV".into()],
                        allow_free_text: true,
                    },
                ],
            },
        }
    }

    #[test]
    fn repl_round_parses_one_aggregate_submission() {
        let reply = parse_repl_choice("2,custom", &round_prompt());
        assert_eq!(reply.answers.len(), 2);
        assert_eq!(reply.answers[0].index, Some(1));
        assert_eq!(reply.answers[1].text.as_deref(), Some("custom"));

        let invalid = parse_repl_choice("1", &round_prompt());
        assert!(
            invalid.answers.is_empty(),
            "partial rounds must not be submitted"
        );
        assert!(!round_reply_complete(&invalid, 2));
        assert!(!round_reply_complete(
            &parse_repl_choice("1,", &round_prompt()),
            2
        ));
        assert!(round_reply_complete(&reply, 2));
    }

    #[test]
    fn stream_json_round_has_questions_and_accepts_answers() {
        let value = prompt_to_json(&round_prompt());
        assert_eq!(value["kind"]["type"], "question_round");
        assert_eq!(value["questions"][0]["id"], "db");
        assert_eq!(value["questions"][1]["options"][1], "CSV");

        let reply = parse_stdin_reply(
            r#"{"type":"reply","id":9,"answers":[{"index":0},{"text":"yaml"}]}"#,
            9,
        )
        .unwrap();
        assert_eq!(reply.answers.len(), 2);
        assert_eq!(reply.answers[0].index, Some(0));
        assert_eq!(reply.answers[1].text.as_deref(), Some("yaml"));
    }

    #[test]
    fn cli_plan_flag_is_opt_in() {
        use clap::Parser as _;

        let enabled = Cli::try_parse_from(["sirbone", "--plan", "task"]).unwrap();
        assert!(enabled.plan);
        let normal = Cli::try_parse_from(["sirbone", "task"]).unwrap();
        assert!(!normal.plan);
    }

    #[test]
    fn removed_experiments_cannot_rejoin_tools_or_prompt_via_legacy_env() {
        let ace = concat!("SIRBONE_", "ACE");
        let architect = concat!("SIRBONE_", "ARCHITECT_", "ENABLE");
        std::env::set_var(ace, "1");
        std::env::set_var(architect, "1");

        let tools = make_tools(Path::new("."), true);
        let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
        let prompt = build_system_prompt(Path::new("."), "");

        std::env::remove_var(ace);
        std::env::remove_var(architect);
        assert!(!names.contains(&"architect"));
        assert!(!names.contains(&"playbook"));
        assert!(!prompt.contains("`architect` tool"));
        assert!(!prompt.contains("<playbook>"));
    }
}
