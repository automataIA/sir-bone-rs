use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{bail, Context as _, Result};
use clap::{CommandFactory, Parser};
use rustyline_async::{Readline, ReadlineEvent, SharedWriter};
use session::SessionEntry;
use sirbone::{
    agent::{AgentContext, ConfirmBridge, LlmClient},
    ai::{AnthropicClient, OpenAiClient},
    claude_md, render, session,
    tools::{
        Architect, ArchitectTool, BashTool, CodeMapTool, EditTool, GlobTool, GrepTool,
        HistoriaTool, JobStatusTool, LoadSkillTool, NoteTool, ReadStamps, ReadTool, ToolRegistry,
        UndoStore, UndoTool, VerifyTool, WebFetchTool, WebSearchTool, WriteTool,
    },
    types::{AgentEvent, ContentBlock, Message},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod cmd;

#[derive(Parser)]
#[command(
    name = "sirbone",
    version,
    about = "AI coding agent",
    long_about = "Sir Bone — a from-scratch AI coding agent. Streams LLM responses, runs tools \
                  (bash, file ops, web fetch), in REPL or TUI mode. Provider auto-detected from \
                  env: ANTHROPIC_AUTH_TOKEN → Anthropic, else OPENAI_API_KEY → OpenAI \
                  (OPENAI_BASE_URL targets Ollama/Groq/etc.).",
    after_help = "EXAMPLES:\n  \
        sirbone \"refactor this module\"          one-shot prompt\n  \
        sirbone                                   interactive TUI (default)\n  \
        sirbone doctor                            check local setup without calling a model\n  \
        sirbone audit                             summarize the latest session\n  \
        sirbone ground PLAN.md                    verify a file's code claims (no model)\n  \
        sirbone --repl                            interactive REPL\n  \
        sirbone -c \"follow-up\"                    resume the most recent session\n  \
        sirbone --thinking-budget 10000 \"...\"     extended thinking\n  \
        sirbone --image shot.png \"what's wrong?\"  image input (Anthropic)\n  \
        sirbone --completions zsh > _sirbone      generate shell completions"
)]
struct Cli {
    #[arg(short, long, env = "SIRBONE_MODEL")]
    model: Option<String>,

    #[arg(long, env = "OPENAI_BASE_URL")]
    base_url: Option<String>,

    #[arg(long, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    #[arg(long, env = "ANTHROPIC_AUTH_TOKEN")]
    anthropic_key: Option<String>,

    #[arg(long, env = "ANTHROPIC_BASE_URL")]
    anthropic_base_url: Option<String>,

    #[arg(long)]
    session: Option<PathBuf>,

    /// Resume the most recent session (no UUID needed).
    #[arg(short = 'c', long = "continue")]
    continue_recent: bool,

    /// Use the REPL/readline mode instead of the default TUI.
    #[arg(long)]
    repl: bool,

    /// Extended thinking budget in tokens (Anthropic only). Enables chain-of-thought.
    #[arg(long, env = "SIRBONE_THINKING_BUDGET")]
    thinking_budget: Option<u32>,

    /// Attach image file(s) to the first prompt (base64-encoded).
    #[arg(long = "image", value_name = "PATH")]
    images: Vec<PathBuf>,

    /// Generate shell completions to stdout and exit.
    #[arg(long, value_name = "SHELL")]
    completions: Option<clap_complete::Shell>,

    /// Render a man page (roff) to stdout and exit.
    #[arg(long)]
    man: bool,

    /// Seed the global ~/.sirbone/.env (configure credentials once for every directory), then print it.
    #[arg(long)]
    login: bool,

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

    /// Ground a file's codebase claims (paths/symbols/counts) against the project,
    /// deterministically (no model). With no path, grounds the latest session's
    /// final answer. Exits non-zero on a divergence — usable as a CI gate.
    #[arg(long, value_name = "PATH", num_args = 0..=1)]
    ground: Option<Option<PathBuf>>,

    /// Emit commands that support it as JSON.
    #[arg(long)]
    json: bool,

    /// Prompt. Omit to enter interactive mode.
    prompt: Vec<String>,
}

fn build_system_prompt(cwd: &std::path::Path, claude_md: &str) -> String {
    let mut parts = Vec::new();

    // Core identity
    parts.push("You are a helpful coding assistant with tools for running shell commands, reading, writing, and editing files.".to_string());

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
    if !sirbone::structure::discover(cwd).is_empty() {
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
    parts.push(
        "When fixing a reported bug or failing behavior: find and change the source \
        cause, then confirm by reproducing the exact scenario from the report and by \
        running the project's existing tests for regressions. Do not invent new tests \
        that pass and treat them as proof the bug is fixed — a self-authored test can \
        pass while the real defect remains. Do not edit, weaken, or delete existing \
        tests to make them pass; fix the code instead."
            .to_string(),
    );

    // Behavioral guardrails distilled from Claude Code's system prompts. Kept to
    // three fragments on purpose: instruction-following degrades as rule count
    // grows, so only rules tracing to observed agent failures live here.
    // Minimalism rule. The YAGNI/KISS reuse-before-write clause is opt-in via
    // SIRBONE_YAGNI while its effect is being A/B'd on the bench (benchmarks/yagni_ab.py).
    // It stays inside this one fragment instead of becoming a new rule, since
    // instruction-following degrades as rule count grows. The safety half is
    // non-negotiable so "write less" never drops a trust-boundary check.
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
    parts.push(
        "Lead with the outcome, then only the detail needed to act on it. Keep \
        responses short; skip preamble and restating what you are about to do."
            .to_string(),
    );
    parts.push(
        "When you need to clarify something, ask one question at a time and wait for \
        the answer before the next — not a batched list. First try to answer it \
        yourself by reading the code; only ask when the code cannot settle it."
            .to_string(),
    );
    parts.push(
        "Report results truthfully: if a test fails, a step was skipped, or something \
        is unverified, say so plainly — never present partial work as done. Reference \
        code as file_path:line_number."
            .to_string(),
    );

    // Verify-from-source discipline: one rule, two targets. Grounds both
    // codebase claims (stale recollection of a file/signature/version) and
    // external claims (AI dumps that blend real facts with fabricated specifics)
    // in primary sources. Merged from two fragments to keep the rule count low.
    parts.push(
        "Ground every claim in the primary source, not memory or assumption. For this \
        codebase, read or grep for a file's contents, a function/type signature, a \
        config value, or a dependency version before relying on it. For external claims, \
        treat AI summaries, \"deep research\", and second-hand sources as unverified — \
        they routinely mix real facts with fabricated specifics (APIs, versions, \
        standards, citations) — and confirm them against the official docs, the live API, \
        or the actual file before acting on or repeating them. If a source cannot confirm \
        it, say so instead of guessing."
            .to_string(),
    );

    // Project memory log. Gated to real projects (like the output-filtering
    // fragment) so empty/docs-only dirs don't carry it. The body is NOT injected
    // — only the path — so the model reads it on demand at no per-session cost.
    if !sirbone::structure::discover(cwd).is_empty() {
        parts.push(format!(
            "This project has a persistent memory log at {path}. When starting work that \
            may depend on earlier decisions, read it first. After you change project files, \
            record the change by calling the `historia` tool (a short `title` plus `bullets` \
            of what changed and the files touched). This is a completion requirement, not \
            optional documentation: a file-changing task is NOT done until the entry is \
            logged — do it even for small changes and even when told to change nothing else \
            (logging the change is not a change to the project). The tool stamps the exact \
            local time and prepends newest-first; don't hand-edit the file to add entries.",
            path = sirbone::project_store::historia_path(cwd).display(),
        ));
    }

    // Per-language debugging cheat-sheet (batch/non-interactive), gated to the
    // languages present in the workspace.
    if let Some(toolkit) = sirbone::system_prompt::debug_toolkit(cwd) {
        parts.push(toolkit);
    }

    // Architect-tool steering — only when the architect is enabled (opt-in).
    if architect_enabled() {
        parts.push(
            "You have an `architect` tool backed by a stronger reviewer model that automatically \
            sees your full transcript. Call it BEFORE substantive work (before writing or committing \
            to an approach) and again before declaring a hard task done. Give its advice serious \
            weight; if a step it suggests fails empirically, adapt.".to_string()
        );
    }

    // Trusted user customization from ~/.sirbone/system/*.md, appended (not replacing
    // the base) so core instructions can be extended but not broken.
    if let Some(user) = sirbone::system_prompt::user_appends() {
        parts.push(user);
    }

    let base = parts.join("\n");
    if claude_md.is_empty() {
        base
    } else {
        format!("{base}\n\n<instructions>\n{claude_md}\n</instructions>")
    }
}

/// Load image file as base64-encoded ContentBlock.
fn load_image(path: &Path) -> Result<ContentBlock> {
    use base64::Engine;
    let data = std::fs::read(path)?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let media_type = match ext.to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    Ok(ContentBlock::Image {
        media_type: media_type.to_string(),
        data: encoded,
    })
}

/// Architect is opt-in (default OFF). Requires both a configured key and an
/// explicit `SIRBONE_ARCHITECT_ENABLE` flag. A 2x2 ablation on SWE-bench-verified-mini
/// found the architect net-negative (it rewrote correct fixes into failing ones), so
/// it no longer activates on key presence alone.
fn architect_enabled() -> bool {
    std::env::var_os("SIRBONE_ARCHITECT_API_KEY").is_some()
        && std::env::var_os("SIRBONE_ARCHITECT_ENABLE").is_some()
}

fn make_tools(cwd: &Path) -> ToolRegistry {
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
    t.register(WriteTool {
        undo: undo.clone(),
        stamps: stamps.clone(),
    });
    t.register(EditTool {
        undo: undo.clone(),
        stamps,
    });
    t.register(UndoTool { store: undo });
    t.register(GrepTool);
    t.register(GlobTool);
    t.register(WebFetchTool);
    #[cfg(feature = "rag")]
    t.register(sirbone::tools::DocSearchTool);
    t.register(WebSearchTool::default());
    t.register(LoadSkillTool);
    t.register(NoteTool {
        store: t.notes.clone(),
    });
    t.register(VerifyTool);
    t.register(CodeMapTool {
        root: cwd.to_path_buf(),
    });
    t.register(HistoriaTool {
        project: cwd.to_path_buf(),
    });

    // Optional architect (stronger second-opinion model on a separate provider).
    // Opt-in: needs SIRBONE_ARCHITECT_API_KEY *and* SIRBONE_ARCHITECT_ENABLE.
    // Default OFF — a 2x2 ablation on SWE-bench-verified-mini found architect
    // net-negative (it rewrote correct fixes into failing ones on flip tasks).
    if let (true, Ok(key)) = (
        architect_enabled(),
        std::env::var("SIRBONE_ARCHITECT_API_KEY"),
    ) {
        let base = std::env::var("SIRBONE_ARCHITECT_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());
        let model =
            std::env::var("SIRBONE_ARCHITECT_MODEL").unwrap_or_else(|_| "claude-opus-4-8".into());
        let max_calls = std::env::var("SIRBONE_ARCHITECT_MAX_CALLS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        // Provider: explicit SIRBONE_ARCHITECT_PROVIDER=anthropic|openai wins;
        // otherwise fall back to a URL heuristic (z.ai's anthropic vs paas paths).
        let is_anthropic = match std::env::var("SIRBONE_ARCHITECT_PROVIDER")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "anthropic" => true,
            "openai" => false,
            _ => base.contains("anthropic"),
        };
        let client: Arc<dyn LlmClient> = if is_anthropic {
            Arc::new(AnthropicClient::new(&base, &key, &model))
        } else {
            Arc::new(OpenAiClient::new(&base, &key, &model))
        };
        t.architect_enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let arch = Architect::new(
            client,
            t.transcript.clone(),
            t.architect_calls.clone(),
            max_calls,
            t.architect_enabled.clone(),
        );
        t.register(ArchitectTool { arch });
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
        let tui_mode = !cli.repl && cli.prompt.join(" ").is_empty();
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
    if cli.login || cli.prompt.first().map(|s| s.as_str()) == Some("login") {
        return cmd::run_login();
    }
    if cli.doctor || (cli.prompt.len() == 1 && cli.prompt[0] == "doctor") {
        return cmd::run_doctor(&cwd, &cli).await;
    }
    if let Some(path) = cli.audit.clone() {
        return cmd::run_audit(path, cli.audit_json).await;
    }
    if cli.prompt.first().map(|s| s.as_str()) == Some("audit") {
        let path = cli.prompt.get(1).map(PathBuf::from);
        return cmd::run_audit(path, cli.audit_json).await;
    }
    if cli.prompt.first().map(|s| s.as_str()) == Some("snapshots") {
        return cmd::run_snapshots(cli.json).await;
    }
    if let Some(arg) = cli.ground.clone() {
        return cmd::run_ground(arg, &cwd).await;
    }
    if cli.prompt.first().map(|s| s.as_str()) == Some("ground") {
        return cmd::run_ground(cli.prompt.get(1).map(PathBuf::from), &cwd).await;
    }

    // Lift any inline mcpServers from config.json into the ~/.sirbone/mcp.json catalog.
    sirbone::mcp::migrate_inline_servers();

    let _ = sirbone::project_store::ensure_historia(&cwd);
    let mut meta = sirbone::project_store::load_meta(&cwd);

    let anthropic_key = cli
        .anthropic_key
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok());

    // Model precedence: --model/SIRBONE_MODEL → last model used in this project →
    // provider-appropriate default (the provider is decided by which key is set).
    let mut model = cli.model.or_else(|| meta.model.clone()).unwrap_or_else(|| {
        if anthropic_key.is_some() {
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
    // `images_ok` is true only on the official Anthropic API: the OpenAI path
    // ignores image blocks, and Anthropic-compatible proxies (e.g. z.ai/GLM)
    // accept them but the model is blind and hallucinates — only real Claude
    // reads images reliably.
    let (client, provider, images_ok): (Arc<dyn LlmClient>, &str, bool) = if let Some(key) =
        anthropic_key
    {
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
        let c = AnthropicClient::new(&base, &key, &model);
        c.set_thinking_budget(cli.thinking_budget);
        (Arc::new(c), "anthropic", images_ok)
    } else if let Some(key) = cli.api_key {
        let base = cli
            .base_url
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        (
            Arc::new(OpenAiClient::new(&base, &key, &model)),
            "openai",
            false,
        )
    } else {
        bail!(
            "no API key found.\n\n\
             Set one of these (env var or a `.env` file in the project root):\n  \
             ANTHROPIC_AUTH_TOKEN=sk-...     → Anthropic (Claude)\n  \
             OPENAI_API_KEY=sk-...           → OpenAI, or any OpenAI-compatible endpoint\n                                  \
             (set OPENAI_BASE_URL for Ollama, Groq, z.ai, …)\n\n\
             Run `sirbone login` to create a global ~/.sirbone/.env, then fill in a key:\n  \
             sirbone login"
        );
    };
    if !cli.images.is_empty() && !images_ok {
        eprintln!(
            "warning: --image may be ignored or hallucinated — the active endpoint ({provider}) is not a vision-capable Anthropic API; only real Claude reads images reliably"
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

    let mut session_path = match (cli.session, cli.continue_recent) {
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
    let mut tools = make_tools(&cwd);
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

    let prompt = cli.prompt.join(" ");
    if !cli.images.is_empty() && prompt.is_empty() {
        // Fail loud instead of silently dropping the images (they were only
        // attached inside the prompt branch below).
        eprintln!("error: --image requires a prompt describing what to do with the image");
        std::process::exit(2);
    }
    if !prompt.is_empty() {
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
        let mut content = Vec::new();
        // Attach images if provided
        for img_path in &cli.images {
            match load_image(img_path) {
                Ok(block) => content.push(block),
                Err(e) => eprintln!("warning: cannot load image {}: {e}", img_path.display()),
            }
        }
        content.push(ContentBlock::Text { text: prompt });
        let user_msg = Message {
            role: sirbone::Role::User,
            content,
        };
        session::append(&session_path, &SessionEntry::Message(user_msg.clone())).await?;
        messages.push(user_msg);
        // Register MCP now (overlapped with localization above); keep handles alive.
        let _mcp = register_mcp(&mut tools, mcp_task, true).await;
        // Oracle gate is opt-in (default OFF) — ablations found it net-neutral/negative.
        let res = run_turn(
            &model,
            &client,
            &session_path,
            &system_prompt,
            &mut messages,
            &tools,
            false,
            false,
            CancellationToken::new(),
        )
        .await;
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
        return res;
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
        )
        .await;
    }

    // REPL/readline mode (--repl)
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
    // `/plan` toggle: when on, each message is prefixed with a directive telling
    // the model to record a SPEC via the `plan` tool before editing.
    let mut plan_mode = false;
    // `/oracle` toggle: post-Done test gate. Default OFF (ablations net-neutral/
    // negative); `/oracle` flips it on at runtime when a test command is configured.
    let mut oracle_on = false;
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
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                // Slash commands. Unknown ones fall through and are sent as a message.
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
                            println!("plan mode {} — messages will ask the model to plan first", if plan_mode { "on" } else { "off" });
                            true
                        }
                        "oracle" => {
                            oracle_on = !oracle_on;
                            println!("oracle gate {}", if oracle_on { "on" } else { "off" });
                            true
                        }
                        "verify" => { println!("{}", sirbone::oracle::verify_once().await); true }
                        "clear" => {
                            messages.clear();
                            session_path = session::new_session_path();
                            println!("cleared — new session: {}", session_path.display());
                            true
                        }
                        _ => false,
                    };
                    if handled { continue; }
                }
                // Plan mode prefixes the message with a directive to use the `plan`
                // tool first; the model produces and follows its own SPEC.
                let line = if plan_mode {
                    format!(
                        "Before editing, call the `plan` tool to record an implementation SPEC, \
                         then follow it.\n\nTask: {line}"
                    )
                } else {
                    line
                };
                let user_msg = Message::user(line);
                session::append(&session_path, &SessionEntry::Message(user_msg.clone())).await?;
                messages.push(user_msg);
                println!();
                // Run the turn while still watching the keyboard: in raw mode Ctrl-C
                // does not raise SIGINT, so rustyline's `Interrupted` is the only abort
                // signal — cancel the in-flight inference when it fires.
                let cancel = CancellationToken::new();
                let turn = run_turn(&model, &client, &session_path, &system_prompt, &mut messages, &tools, true, oracle_on, cancel.clone());
                tokio::pin!(turn);
                loop {
                    tokio::select! {
                        res = &mut turn => { res?; break; }
                        _ = job_tick.tick() => {
                            for (id, command, exit, dur) in tools.jobs.take_finished() {
                                let mark = if exit == Some(0) { "✓" } else { "✗" };
                                let code = exit.map_or_else(|| "?".into(), |c| c.to_string());
                                use std::io::Write as _;
                                let _ = writeln!(writer, "{mark} job #{id} finished · {}s · exit {code} — {command}", dur.as_secs());
                            }
                        }
                        ev = rl.readline() => {
                            if let Ok(ReadlineEvent::Interrupted) = ev { cancel.cancel(); }
                            // Lines/EOF typed mid-turn are ignored; abort with Ctrl-C.
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
    oracle: bool,
    cancel: CancellationToken,
) -> Result<()> {
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
        interactive,
    ));

    // In interactive mode wire up a confirmation bridge for destructive commands.
    let (confirm, confirm_task) = if interactive {
        let (ask_tx, mut ask_rx) = mpsc::channel::<String>(1);
        let (reply_tx, reply_rx) = mpsc::channel::<bool>(1);
        let task = tokio::spawn(async move {
            while let Some(cmd) = ask_rx.recv().await {
                let preview: String = cmd.chars().take(120).collect();
                eprint!("\n\u{26a0} destructive: {preview}\nAllow? [y/N] ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input).ok();
                reply_tx
                    .send(input.trim().eq_ignore_ascii_case("y"))
                    .await
                    .ok();
            }
        });
        (
            Some(ConfirmBridge {
                ask: ask_tx,
                reply: reply_rx,
            }),
            Some(task),
        )
    } else {
        (None, None)
    };

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
        };
        let r = sirbone::run(&mut ctx).await;
        *messages = std::mem::take(&mut ctx.messages);
        r
    }; // ctx (and tx) dropped -> print_task drains and finishes

    ctrl_c.abort();
    if let Some(t) = confirm_task {
        t.abort();
    }
    let compaction_base = print_task.await??;

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

    run_result
}

/// Render events to the terminal. Returns the transcript length recorded by the
/// last `Compacted` event, if any, so the caller can fix its append bookkeeping.
async fn render_events(
    mut rx: mpsc::Receiver<AgentEvent>,
    session_path: PathBuf,
    mcp_cost: (usize, usize),
    raw: bool,
) -> Result<Option<usize>> {
    let mut r = render::Renderer::new(raw);
    let mut ctx_warned = false;
    let mut compaction_base = None;
    // Token accounting (opt-in via SIRBONE_USAGE=1): sum the real per-call prompt
    // size and cache hits across the run; the bench harness parses the final line.
    let (mut usage_calls, mut usage_input, mut usage_cached, mut usage_peak) =
        (0u64, 0u64, 0u64, 0u32);
    // Drive a braille spinner on an interval during the dead air between the
    // model's turns and while tools run; `biased` drains real events first so the
    // spinner never delays output. The Renderer no-ops the spinner off a tty.
    let mut spin = tokio::time::interval(std::time::Duration::from_millis(80));
    loop {
        let ev = tokio::select! {
            biased;
            maybe = rx.recv() => match maybe { Some(ev) => ev, None => break },
            _ = spin.tick() => { r.spin_tick(); continue; }
        };
        // Flush any buffered prose/code line before a non-text event renders,
        // so streaming output is not stranded behind tool boxes or notices.
        if !matches!(ev, AgentEvent::TextChunk(_)) {
            r.flush_text();
        }
        match ev {
            AgentEvent::TextChunk(s) => {
                r.spin_stop();
                r.text(&s);
            }
            AgentEvent::ToolCallStart { name, .. } => {
                r.tool_start(&name);
                r.spin_start("running");
            }
            AgentEvent::ToolCallEnd {
                name,
                result,
                is_error,
                ..
            } => {
                r.tool_end(&name, &result, is_error);
                r.spin_start("thinking");
            }
            AgentEvent::Error(e) => {
                r.spin_stop();
                r.error(&e);
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
            } => {
                // Gate on >0: z.ai emits a zero usage at message_start and the real
                // figure at message_delta — counting both would double the call count.
                if used_tokens > 0 {
                    usage_calls += 1;
                    usage_input += used_tokens as u64;
                    usage_cached += cached_tokens as u64;
                    usage_peak = usage_peak.max(used_tokens);
                }
                let pct =
                    ((used_tokens as u64 * 100) / context_window.max(1) as u64).min(100) as u8;
                // One-shot context-rot warning at the amber threshold; rearms
                // when usage drops back (compaction).
                if pct >= 70 && !ctx_warned {
                    ctx_warned = true;
                    r.ctx_warning(pct, context_window);
                } else if pct < 70 {
                    ctx_warned = false;
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
    }
    if std::env::var_os("SIRBONE_USAGE").is_some() {
        let (mcp_tools, mcp_schema_tokens) = mcp_cost;
        eprintln!("[usage] calls={usage_calls} input_tokens={usage_input} cached_tokens={usage_cached} peak_context={usage_peak} mcp_tools={mcp_tools} mcp_schema_tokens={mcp_schema_tokens}");
    }
    Ok(compaction_base)
}
