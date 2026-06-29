use std::{
    collections::VecDeque,
    io,
    io::Write as _,
    time::{Duration, Instant},
};

use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        cursor::{Hide, Show},
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
            MouseButton, MouseEvent, MouseEventKind,
        },
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal,
};
use sirbone::tui::{
    color_to_hex, ctx_usage_color, edit_diff_block, fmt_elapsed, fmt_tok, job_gauge, kb_combo,
    kb_single, load_boar, md_to_lines, out_preview_rows, render_confirm_dialog,
    render_scroll_indicators, running_tool_block, thread_blank, thread_wrap, timeline_entry_lines,
    timeline_tree_parts, tool_box_row, tool_box_top, user_box, BoarAnim, BrailleWidget, Palette,
    KB_PANEL_W, PALETTES, THREAD_GUTTER, TIMELINE_W, TIMELINE_W_MAX, TIMELINE_W_MIN,
};
use sirbone::types::AgentEvent;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn st(color: ratatui::style::Color, bold: bool) -> Style {
    let s = Style::default().fg(color);
    if bold {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

// ── demo skill ─────────────────────────────────────────────────────────────
// One skill, two invocation paths (mirrors the real app):
//   • user  → types `/commit-helper`            → exec_slash injects the body
//   • model → calls the `load_skill` tool       → scripted turn in build_events
const DEMO_SKILL_NAME: &str = "commit-helper";
const DEMO_SKILL_DESC: &str = "Generate Conventional Commits messages from the staged diff";
const DEMO_SKILL_BODY: &str = "# commit-helper\n\n\
    Generate a **Conventional Commits** message from the staged diff:\n\n\
    - `type(scope): subject` — subject ≤ 50 chars, imperative\n\
    - body only if the *why* isn't obvious from the diff\n\
    - footer `BREAKING CHANGE:` if it breaks the public API\n";

// ── settings picker (mirrors src/tui: app.rs PickerKind/PickRow/Picker) ──────
// The real app scans ~/.sirbone/skills and reads ~/.sirbone/mcp.json; here we
// use static rows so the picker UI can be prototyped without the filesystem.
#[derive(Clone, Copy, PartialEq)]
enum MockPickerKind {
    Skills,
    Mcp,
}

// Mirrors src/tui: app.rs SettingsRow / SETTINGS_ROWS — arrow-key nav over the
// Settings screen.
#[derive(Clone, Copy, PartialEq)]
enum MockSettingsRow {
    Localize,
    Plan,
    Oracle,
    QuotaBar,
    Architect,
    Thinking,
    Skills,
    Mcp,
}
const MOCK_SETTINGS_ROWS: &[MockSettingsRow] = &[
    MockSettingsRow::Localize,
    MockSettingsRow::Plan,
    MockSettingsRow::Oracle,
    MockSettingsRow::QuotaBar,
    MockSettingsRow::Architect,
    MockSettingsRow::Thinking,
    MockSettingsRow::Skills,
    MockSettingsRow::Mcp,
];
struct MockPickRow {
    name: &'static str,
    description: &'static str,
    tag: &'static str,
    enabled: bool,
}
struct MockPicker {
    kind: MockPickerKind,
    rows: Vec<MockPickRow>,
    cursor: usize,
}
// Allowlist semantics: a fresh project opts in to nothing, so every row starts
// disabled (`enabled: false`). The user checks the ones they want per project.
fn mock_pick_rows(kind: MockPickerKind) -> Vec<MockPickRow> {
    match kind {
        MockPickerKind::Skills => vec![
            MockPickRow {
                name: DEMO_SKILL_NAME,
                description: DEMO_SKILL_DESC,
                tag: "  (local)",
                enabled: false,
            },
            MockPickRow {
                name: "tdd",
                description: "Red-green-refactor TDD loop",
                tag: "",
                enabled: false,
            },
            MockPickRow {
                name: "diagnose",
                description: "Disciplined hard-bug diagnosis loop",
                tag: "",
                enabled: false,
            },
            MockPickRow {
                name: "rust",
                description: "Idiomatic Rust coding guidelines",
                tag: "  (always)",
                enabled: false,
            },
        ],
        MockPickerKind::Mcp => vec![
            MockPickRow {
                name: "context7",
                description: "npx -y @upstash/context7-mcp@latest",
                tag: "  (trust)",
                enabled: false,
            },
            MockPickRow {
                name: "filesystem",
                description: "npx -y @modelcontextprotocol/server-filesystem",
                tag: "",
                enabled: false,
            },
        ],
    }
}

fn mock_tool_input(name: &str) -> Option<&'static str> {
    match name {
        "bash" => Some("cargo test --workspace"),
        "grep" => Some("ratio > COMPACTION_THRESHOLD"),
        "read" => Some("crates/pi-agent/src/types.rs"),
        "find" => Some("**/*.rs  pattern: AgentEvent"),
        "edit" => Some("crates/pi-agent/src/agent.rs"),
        "write" => Some("CHANGELOG.md"),
        "load_skill" => Some(DEMO_SKILL_NAME),
        _ => None,
    }
}

// ── mock event schedule ───────────────────────────────────────────────────────

#[derive(Clone)]
enum MockEv {
    User(&'static str),
    Agent(AgentEvent),
    /// Destructive-command approval request: opens the y/n dialog and pauses
    /// the schedule until the user answers (mirrors the real ConfirmBridge).
    Confirm(&'static str),
    /// A background job starts ticking in the info bar (mirrors the real
    /// JobStore polling; cleared by the matching `AgentEvent::JobDone`).
    JobStart(u32, &'static str),
}

fn build_events() -> Vec<(u64, MockEv)> {
    let mut out = Vec::new();
    let mut t = 0u64;
    let mut at = |delay: u64, ev: MockEv| {
        t += delay;
        out.push((t, ev));
    };

    at(
        10,
        MockEv::User("analyze the project structure and tell me where AgentEvent is defined"),
    );
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    // Extended thinking: only the tail shows up, dimmed, in the status bar.
    at(8, MockEv::Agent(AgentEvent::ThinkingStart));
    for chunk in [
        "The user wants the AgentEvent definition.\n",
        "Listing the workspace first, then narrowing with find.\n",
    ] {
        at(14, MockEv::Agent(AgentEvent::ThinkingChunk(chunk.into())));
    }
    at(
        20,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "ls".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        22,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "ls".into(),
            result:
                "crates/  playground/  Cargo.toml  Cargo.lock  README.md  CLAUDE.md  CHANGELOG.md"
                    .into(),
        }),
    );
    at(
        15,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "find".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        28,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "find".into(),
            result: "crates/pi-agent/src/types.rs".into(),
        }),
    );
    at(
        15,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "read".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        30,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "read".into(),
            result: "pub enum AgentEvent { TurnStart, TextChunk(String), TurnEnd, \
                 ToolCallStart { name: String }, ToolCallEnd { name, result }, \
                 Error(String), Cancelled }"
                .into(),
        }),
    );
    for chunk in [
        "Found `AgentEvent` in `crates/pi-agent/src/types.rs`.\n\n",
        "## Variants\n\n",
        "- **`TurnStart`** — LLM turn start\n",
        "- **`TextChunk(String)`** — streaming text\n",
        "- **`TurnEnd`** — turn end, flush markdown\n",
        "- **`ToolCallStart { name }`** — tool start\n",
        "- **`ToolCallEnd { name, result }`** — tool result\n",
        "- **`Error(String)`** — fatal error\n",
        "- **`Notice { text, level }`** — non-failure status (oracle green/retry)\n",
        "- **`Cancelled`** — operation cancelled by the user\n",
    ] {
        at(7, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    // ── PROTOTYPE demo (proposta.md fix #1): text-then-tool interleave ───────
    //   The model narrates, *then* calls a tool mid-thought. This is the case
    //   the fix targets: today the ToolCallStart would dump the half-typed
    //   sentence to full instantly; with the deferral the typewriter finishes
    //   the sentence first, then the running box appears. Watch the cursor ▌
    //   reach the period before the `grep` box drops in.
    at(
        45,
        MockEv::User("where is the permission classifier wired up?"),
    );
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    for chunk in [
        "Good question — the classifier is the last stage of the permission ",
        "pipeline, so it only sees bash commands that the allow/deny globs ",
        "left undecided. Let me grep for where it's invoked before I explain.",
    ] {
        at(7, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(
        4, // fires while the sentence above is still revealing
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "grep".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        55, // long enough that sentence 1 finishes revealing and the running box shows
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "grep".into(),
            result: "src/agent.rs:412:    let decision = self.classify(&call).await?;".into(),
        }),
    );
    for chunk in [
        "Found it: `agent.rs:412` calls `classify`, which reuses `run_turn` ",
        "with an empty registry so the model itself rules on the command.",
    ] {
        at(7, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    // ── note + wait tools ──────────────────────────────────────────────────
    //   • model → records a working note (persists across compaction)
    //   • model → polls with the `wait` tool until a slow job finishes
    at(
        55,
        MockEv::User("run the slow integration suite and keep a note of progress while it runs"),
    );
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    at(
        18,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "note".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        14,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "note".into(),
            result: "note saved".into(),
        }),
    );
    at(
        16,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "bash".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        20,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "bash".into(),
            result: "started integration suite in background (pid 4821)".into(),
        }),
    );
    at(
        16,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "wait".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        45,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "wait".into(),
            result: "waited 120s".into(),
        }),
    );
    at(
        16,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "bash".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        20,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "bash".into(),
            result: "test result: ok. 84 passed; 0 failed".into(),
        }),
    );
    at(
        14,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "note".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        12,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "note".into(),
            result: "note saved".into(),
        }),
    );
    for chunk in [
        "Integration suite finished: **84 passed, 0 failed**.\n\n",
        "I kept a working note across the wait (it survives context compaction) ",
        "and polled with the `wait` tool until the run completed.\n",
    ] {
        at(8, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    // Background job: bash background:true returns immediately, the user is
    // free to ask other things, the ⚙ gauge ticks in the info bar, and a
    // ✓ block lands when the job exits.
    at(
        55,
        MockEv::User("run the full benchmark suite, it takes a while"),
    );
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    at(
        18,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "bash".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        14,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "bash".into(),
            result: "started background job #1 (log: ~/.sirbone/jobs/4821-1.log). \
                     Don't busy-wait: do other work or end the turn."
                .into(),
        }),
    );
    at(6, MockEv::JobStart(1, "cargo bench --all"));
    for chunk in [
        "Benchmark running as **job #1** — you'll be notified here when it ",
        "finishes; meanwhile I'm free for other tasks (`/jobs` for details).\n",
    ] {
        at(8, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    // The user asks something else while the job runs.
    at(
        45,
        MockEv::User("meanwhile, which file defines the tool registry?"),
    );
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    at(
        16,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "grep".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        20,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "grep".into(),
            result: "crates/pi-agent/src/tools/mod.rs:42:pub struct ToolRegistry {".into(),
        }),
    );
    at(
        10,
        MockEv::Agent(AgentEvent::TextChunk(
            "`ToolRegistry` lives in `crates/pi-agent/src/tools/mod.rs`.\n".into(),
        )),
    );
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    // The background job exits while the agent is idle.
    at(
        70,
        MockEv::Agent(AgentEvent::JobDone {
            id: 1,
            command: "cargo bench --all".into(),
            exit: Some(0),
            secs: 95,
        }),
    );

    // Failing tool: the OUT row gets a red ✗ (is_error from the ToolResult).
    at(55, MockEv::User("read the deployment config"));
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    at(
        18,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "read".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        20,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: true,
            name: "read".into(),
            result: "Error: deploy.toml: no such file or directory".into(),
        }),
    );
    at(
        8,
        MockEv::Agent(AgentEvent::TextChunk(
            "There is no `deploy.toml` in this project — deployment is configured elsewhere.\n"
                .into(),
        )),
    );
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    at(
        55,
        MockEv::User("the context window check uses > instead of >=, find it and fix it"),
    );
    at(35, MockEv::Agent(AgentEvent::TurnStart));
    at(
        18,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "grep".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        25,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "grep".into(),
            result: "crates/pi-agent/src/agent.rs:187:    if ratio > COMPACTION_THRESHOLD {".into(),
        }),
    );
    at(
        15,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "read".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        28,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "read".into(),
            result: "const COMPACTION_THRESHOLD: f32 = 0.875;\n\
                 let ratio = usage as f32 / ctx_window as f32;\n\
                 if ratio > COMPACTION_THRESHOLD { warn_compaction(); }"
                .into(),
        }),
    );
    at(
        15,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "edit".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        22,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "edit".into(),
            result: "Edited crates/pi-agent/src/agent.rs.\n\n\
                 @@ -41,9 +41,10 @@\n\
                 \x20    let resp = self\n\
                 \x20        .http\n\
                 \x20        .post(&url)\n\
                 \x20        .header(\"x-api-key\", &self.api_key)\n\
                 \x20        .json(&body)\n\
                 \x20        .send()\n\
                 -        .await?;\n\
                 +        .await\n\
                 +        .map_err(enrich_reqwest_error)?;"
                .into(),
        }),
    );
    at(
        18,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "bash".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        55,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "bash".into(),
            result: "cargo test --workspace … test result: ok. 12 passed; 0 failed; 0 ignored"
                .into(),
        }),
    );
    for chunk in [
        "Fix applied: `>` → `>=` in `agent.rs:187`.\n\n",
        "The compaction warning now triggers **exactly** at 87.5% ",
        "of the context window. Tests: **12 passed**.\n",
    ] {
        at(9, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    at(55, MockEv::User("update CHANGELOG with the fix just made"));
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    at(
        18,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "write".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        22,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "write".into(),
            result: "CHANGELOG.md written (912 bytes)".into(),
        }),
    );
    at(
        18,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "bash".into(),
            input: serde_json::Value::Null,
        }),
    );
    // y/n dialog: schedule pauses here until answered; deny → blocked box.
    at(
        10,
        MockEv::Confirm("git commit -m \"fix(agent): compaction threshold\""),
    );
    at(
        14,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: true,
            name: "bash".into(),
            result: "blocked".into(),
        }),
    );
    at(
        12,
        MockEv::Agent(AgentEvent::TextChunk("Updated `CHANGELOG.md`. ".into())),
    );
    at(
        10,
        MockEv::Agent(AgentEvent::TextChunk(
            "The `git commit` command requires ".into(),
        )),
    );
    at(
        10,
        MockEv::Agent(AgentEvent::TextChunk(
            "explicit confirmation from the user…".into(),
        )),
    );
    at(9, MockEv::Agent(AgentEvent::Cancelled));

    at(
        60,
        MockEv::User("show me the architecture as a mermaid diagram"),
    );
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    for chunk in [
        "Here's the project architecture:\n\n",
        "```mermaid\ngraph LR\n",
        "    CLI[\"pi-cli\"]\n",
        "    AI[\"pi-ai\"]\n",
        "    AGENT[\"pi-agent\"]\n",
        "    CLI --> AI\n",
        "    CLI --> AGENT\n",
        "    AI --> AGENT\n",
        "```\n\n",
        "The arrows show the dependencies between crates.\n",
    ] {
        at(8, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    at(
        60,
        MockEv::User("comparison of the supported LLM providers"),
    );
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    for chunk in [
        "Here are the supported providers:\n\n",
        "| Provider  | Auth env              | Default model     |\n",
        "|-----------|---------------------- |-------------------|\n",
        "| Anthropic | ANTHROPIC_AUTH_TOKEN  | claude-opus-4-7   |\n",
        "| OpenAI    | OPENAI_API_KEY        | gpt-4o            |\n",
        "| Ollama    | OPENAI_API_KEY=x      | llama3.2          |\n",
        "| Groq      | OPENAI_API_KEY        | llama-3.3-70b     |\n",
        "\nDetection: `ANTHROPIC_AUTH_TOKEN` → Anthropic; otherwise OpenAI-compatible.\n",
    ] {
        at(8, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    at(60, MockEv::User("project timeline in mermaid"));
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    for chunk in [
        "Here's the timeline:\n\n",
        "```mermaid\ntimeline\n",
        "    title Roadmap\n",
        "    2024 : v0.1 alpha\n",
        "    2025 : v1.0 stable\n",
        "```\n",
    ] {
        at(8, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    at(
        60,
        MockEv::User("show me an example of rust syntax highlighting"),
    );
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    for chunk in [
        "Here's the agent's main loop with syntax highlighting:\n\n",
        "```rust\n",
        "pub async fn run(ctx: &mut AgentContext) -> Result<()> {\n",
        "    loop {\n",
        "        // ask the model for the next step\n",
        "        let turn = ctx.client.run_turn(ctx).await?;\n",
        "        match turn.state {\n",
        "            AgentState::Done => break,\n",
        "            AgentState::ToolCalling => {\n",
        "                let results = execute_tools(&turn.calls, ctx).await;\n",
        "                ctx.messages.extend(results);\n",
        "            }\n",
        "        }\n",
        "    }\n",
        "    Ok(())\n",
        "}\n",
        "```\n\n",
        "Keywords in orange, strings in green, types in purple.\n",
    ] {
        at(7, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    // Model-invoked skill: the LLM autonomously calls `load_skill` to pull the
    // commit-helper instructions, then writes the commit in that format.
    at(
        60,
        MockEv::User("write the commit message for the earlier fix"),
    );
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    at(
        18,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "load_skill".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        24,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "load_skill".into(),
            result: DEMO_SKILL_BODY.into(),
        }),
    );
    for chunk in [
        "Loaded the `commit-helper` skill and applying the format:\n\n",
        "```\nfix(agent): include the exact 87.5% in the compaction threshold\n\n",
        "The warning only triggered above 87.5% (`>`); now it uses `>=`.\n```\n",
    ] {
        at(8, MockEv::Agent(AgentEvent::TextChunk(chunk.into())));
    }
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    // Plan mode (`/plan`) + verify: the model records a SPEC via the `plan` tool,
    // then self-checks via the `verify` tool before reporting done.
    at(60, MockEv::User("add a --version flag to the CLI"));
    at(30, MockEv::Agent(AgentEvent::TurnStart));
    at(
        20,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "plan".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        18,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "plan".into(),
            result: "Plan recorded as the session contract.".into(),
        }),
    );
    at(
        20,
        MockEv::Agent(AgentEvent::ToolCallStart {
            id: String::new(),
            name: "verify".into(),
            input: serde_json::Value::Null,
        }),
    );
    at(
        18,
        MockEv::Agent(AgentEvent::ToolCallEnd {
            id: String::new(),
            is_error: false,
            name: "verify".into(),
            result: "✓ all tests pass (`cargo test -q`)".into(),
        }),
    );
    at(10, MockEv::Agent(AgentEvent::TurnEnd));

    out
}

fn build_mock_kb_lines(p: &Palette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for chunk in [
        kb_single("Esc", "Quit", p),
        kb_single("Tab", "Focus chat ↔ input", p),
        kb_single("↑ ↓", "Scroll output", p),
        kb_combo("Pg", "Up/Dn", "Fast scroll", p),
        kb_single("G", "Go to bottom (auto-scroll)", p),
        kb_combo("Alt", "B", "Toggle timeline panel", p),
        kb_combo("Alt", "P", "Cycle palette", p),
        kb_combo("Alt", "A", "About", p),
        kb_combo("Alt", "S", "Settings", p),
    ] {
        lines.extend(chunk);
    }
    lines
}

// ── tool box popup ────────────────────────────────────────────────────────────

struct MockToolBox {
    line_start: usize,
    line_end: usize,
    name: String,
    full_input: String,
    full_output: String,
}

/// Mirrors `app::TimelineEntry` — one clickable row in the side trail panel.
struct MockTimelineEntry {
    label: String,
    clock: String,
    target: usize,
    is_user: bool,
}

struct MockPopup {
    tool_idx: usize,
    scroll: u16,
}

// ── app ───────────────────────────────────────────────────────────────────────

enum MockStatus {
    Idle,
    Thinking,
    Tool(String),
}

#[derive(PartialEq)]
enum Focus {
    Input,
    Output,
}

struct MockApp {
    lines: Vec<Line<'static>>,
    pending: String,
    revealed: usize,
    reveal_anchor: Option<Instant>,
    reveal_clock: Option<Instant>,
    reveal_carry: f64,
    reveal_rate: f64,
    reveal_drain: bool,
    // PROTOTYPE (proposta.md fix #1): segment-boundary events (ToolCallStart,
    // TurnEnd, …) that arrived while the typewriter still trails the text are
    // parked here instead of dumping the tail via flush_pending. Drained in
    // FIFO order by `drain_deferred` once `revealing()` catches up.
    deferred: VecDeque<AgentEvent>,
    tail_cache: Option<(usize, usize, Vec<Line<'static>>)>,
    input: String,
    focus: Focus,
    scroll: u16,
    auto_scroll: bool,
    spinner_tick: u64,
    busy: bool,
    status: MockStatus,
    events: Vec<(u64, MockEv)>,
    processed: Vec<MockEv>,
    frame: u64,
    boar: BoarAnim,
    panel_visible: bool,
    about_mode: bool,
    settings_mode: bool,
    settings_cursor: usize,
    picker: Option<MockPicker>,
    localize: bool,
    architect_on: bool,
    thinking_budget: Option<u32>,
    ctx_pct: u8,
    spend_tokens: u64,      // mirrors tui: cumulative token spend (Feature C)
    spend_cap: Option<u64>, // mirrors tui: spend cap, None = disabled
    ctx_warned: bool,       // mirrors tui: one-shot 70% context-rot warning
    quota_window_start: chrono::DateTime<chrono::Local>, // mock: seeded at launch
    quota_bar: bool,        // Settings toggle (mirrors tui)
    show_logs: bool,        // F12 debug overlay (mirrors tui)
    pending_tool: Option<(String, Option<&'static str>, Instant, String)>,
    tool_durations: Vec<Option<Duration>>,
    replaying: bool,
    replay_tool_idx: usize,
    busy_since: Option<Instant>,
    thinking_tail: String,
    history: Vec<String>,
    history_idx: Option<usize>,
    history_stash: String,
    queued_input: Option<String>,
    jobs_running: Vec<(u32, String, Instant)>,
    tool_boxes: Vec<MockToolBox>,
    timeline: Vec<MockTimelineEntry>,
    timeline_area: Rect,
    timeline_rows: Vec<usize>,
    panel_w: u16,
    resizing_panel: bool,
    trail_scroll: u16,
    trail_max_scroll: u16,
    hover_divider: bool,
    scroll_to: Option<u16>,
    selected_entry: Option<usize>,
    popup: Option<MockPopup>,
    chat_area: Rect,
    max_scroll: u16,
    palette_idx: usize,
    palette: &'static Palette,
    thread_active: bool,
    confirm_request: Option<String>,
    plan: bool,
    oracle: bool,
}

impl MockApp {
    fn new() -> Self {
        let boar = load_boar().unwrap_or_else(|| BoarAnim::new_mock(60, 20));
        Self {
            lines: Vec::new(),
            pending: String::new(),
            revealed: 0,
            reveal_anchor: None,
            reveal_clock: None,
            reveal_carry: 0.0,
            reveal_rate: 0.0,
            reveal_drain: false,
            deferred: VecDeque::new(),
            tail_cache: None,
            input: String::new(),
            focus: Focus::Input,
            scroll: 0,
            auto_scroll: true,
            spinner_tick: 0,
            busy: false,
            status: MockStatus::Idle,
            events: build_events(),
            processed: Vec::new(),
            frame: 0,
            boar,
            panel_visible: true,
            about_mode: false,
            settings_mode: false,
            settings_cursor: 0,
            picker: None,
            localize: true,
            architect_on: true,
            thinking_budget: None,
            // Demo values so the `tok N/M` indicator is visible in the sandbox.
            spend_tokens: 142_000,
            spend_cap: Some(500_000),
            ctx_pct: 8,
            ctx_warned: false,
            quota_window_start: chrono::Local::now(),
            quota_bar: true,
            show_logs: false,
            pending_tool: None,
            tool_durations: Vec::new(),
            replaying: false,
            replay_tool_idx: 0,
            busy_since: None,
            thinking_tail: String::new(),
            history: Vec::new(),
            history_idx: None,
            history_stash: String::new(),
            queued_input: None,
            jobs_running: Vec::new(),
            tool_boxes: Vec::new(),
            timeline: Vec::new(),
            timeline_area: Rect::default(),
            timeline_rows: Vec::new(),
            panel_w: TIMELINE_W,
            resizing_panel: false,
            trail_scroll: 0,
            trail_max_scroll: 0,
            hover_divider: false,
            scroll_to: None,
            selected_entry: None,
            popup: None,
            chat_area: Rect::default(),
            max_scroll: 0,
            palette_idx: 0,
            palette: &PALETTES[0].1,
            thread_active: false,
            confirm_request: None,
            plan: false,
            oracle: false,
        }
    }

    fn chat_w(&self) -> usize {
        let tw = ratatui::crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80);
        let panel_w = if self.panel_visible {
            self.panel_w as usize
        } else {
            0
        };
        tw.saturating_sub(panel_w + 2)
    }

    fn tick(&mut self) {
        self.frame += 1;
        if self.busy {
            self.spinner_tick = self.spinner_tick.wrapping_add(1);
            if self.frame.is_multiple_of(20) {
                self.ctx_pct = self.ctx_pct.saturating_add(1).min(99);
                // Mirrors tui.rs: one-shot context-rot warning at the amber threshold.
                if self.ctx_pct >= 70 && !self.ctx_warned {
                    self.ctx_warned = true;
                    let p = self.palette;
                    self.push_block(vec![Line::from(Span::styled(
                        format!("⚠ context at {}% of 200k tokens — quality may degrade, /compact to summarize",
                                self.ctx_pct),
                        st(p.accent, false),
                    ))]);
                } else if self.ctx_pct < 70 {
                    self.ctx_warned = false;
                }
            }
        }

        if self.about_mode {
            let term_w = ratatui::crossterm::terminal::size()
                .map(|(w, _)| w)
                .unwrap_or(200);
            let boar_area_w = term_w.saturating_sub(KB_PANEL_W + 2);
            self.boar.advance_about(boar_area_w);
        }

        // Pending approval: auto-dismiss after 2s in mock mode.
        if let Some(req) = self.confirm_request.take() {
            self.processed.push(MockEv::Confirm(req.leak()));
        }
        // Run a queued command once the agent goes idle (mirrors tui.rs).
        if !self.busy {
            if let Some(q) = self.queued_input.take() {
                if let Some(cmd) = q.strip_prefix('/') {
                    self.exec_slash(cmd);
                }
            }
        }
        while let Some((f, ev)) = self.events.first() {
            if *f > self.frame {
                break;
            }
            // PROTOTYPE (proposta.md fix #1): don't start the next user turn until
            // the current one has fully settled — typewriter drained AND parked
            // boundaries (deferred) applied. Mirrors the real app, which finishes
            // the agent run (with its end-of-run drain) before accepting the next
            // message. Without this gate the scripted timeline fires the next User
            // mid-reveal, it bypasses the deferred queue, and every later event
            // backs up behind a never-drained TurnEnd.
            if matches!(ev, MockEv::User(_))
                && (self.busy || self.revealing() || !self.deferred.is_empty())
            {
                break;
            }
            let (_, ev) = self.events.remove(0);
            // Confirm is interactive-only: don't record it, or replay() (palette
            // switch, boar toggle) would re-open an already-answered dialog.
            if let MockEv::Confirm(cmd) = &ev {
                self.confirm_request = Some(cmd.to_string());
                break;
            }
            self.processed.push(ev.clone());
            self.process_ev(&ev);
        }
    }

    fn process_ev(&mut self, ev: &MockEv) {
        match ev {
            MockEv::User(text) => {
                self.thread_active = false; // new turn: close the previous rail
                let target = self.lines.len();
                self.lines.push(Line::default());
                self.lines
                    .extend(user_box(text, self.chat_w(), self.palette));
                let label: String = text.lines().next().unwrap_or("").chars().take(60).collect();
                self.timeline.push(MockTimelineEntry {
                    label,
                    clock: String::new(),
                    target,
                    is_user: true,
                });
                self.busy = true;
                if !self.replaying {
                    self.busy_since = Some(Instant::now());
                }
            }
            MockEv::Agent(aev) => self.push_agent(aev.clone()),
            MockEv::Confirm(_) => {} // handled in tick(), never recorded
            MockEv::JobStart(id, cmd) => {
                self.jobs_running
                    .push((*id, cmd.to_string(), Instant::now()));
            }
        }
    }

    fn replay(&mut self) {
        let events = std::mem::take(&mut self.processed);
        self.lines.clear();
        self.pending.clear();
        self.deferred.clear(); // PROTOTYPE: rebuild is synchronous, no parked events
        self.reveal_drain = false;
        self.tool_boxes.clear();
        self.timeline.clear();
        self.popup = None;
        self.thread_active = false;
        self.replaying = true;
        self.replay_tool_idx = 0;
        for ev in &events {
            self.process_ev(ev);
        }
        self.replaying = false;
        self.processed = events;
    }

    fn push_history(&mut self, text: &str) {
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
        self.history_idx = None;
    }

    /// ↑ — recall the previous submitted input (mirrors tui.rs).
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.history_stash = std::mem::take(&mut self.input);
                self.history_idx = Some(self.history.len() - 1);
            }
            Some(0) => {}
            Some(i) => self.history_idx = Some(i - 1),
        }
        if let Some(i) = self.history_idx {
            self.input = self.history[i].clone();
        }
    }

    /// ↓ — move toward the present; past the newest entry restores the draft.
    fn history_next(&mut self) {
        let Some(i) = self.history_idx else { return };
        if i + 1 < self.history.len() {
            self.history_idx = Some(i + 1);
            self.input = self.history[i + 1].clone();
        } else {
            self.history_idx = None;
            self.input = std::mem::take(&mut self.history_stash);
        }
    }

    /// Append an agent block onto the timeline rail (mirrors `App::push_block`).
    fn push_block(&mut self, lines: Vec<Line<'static>>) -> usize {
        if self.thread_active {
            self.lines.push(thread_blank(self.palette));
        } else {
            self.lines.push(Line::default());
            self.thread_active = true;
        }
        let start = self.lines.len();
        self.lines.extend(thread_wrap(lines, true, self.palette));
        start
    }

    /// PROTOTYPE (proposta.md fix #1) — gate in front of `apply_agent`.
    ///
    /// Today every `ToolCallStart`/`TurnEnd` calls `flush_pending`, which dumps
    /// the still-unrevealed text in one shot (resets `revealed`, pushes the whole
    /// block). Since Anthropic streams text *before* tool_use in the same message,
    /// almost every tool-using turn snaps its trailing text to full instead of
    /// finishing the typewriter — the real visible jank.
    ///
    /// Fix: if such a "segment boundary" arrives while the cursor still trails
    /// (`revealing()`), set `reveal_drain` (release the jitter reserve, drain the
    /// tail at constant rate) and park the event. `drain_deferred` replays it once
    /// the text has fully revealed. Order is preserved by parking *every* later
    /// event while the queue is non-empty.
    ///
    /// Replay rebuilds history synchronously, so it bypasses the queue entirely.
    fn push_agent(&mut self, ev: AgentEvent) {
        if self.replaying {
            self.apply_agent(ev);
            return;
        }
        if !self.deferred.is_empty() {
            self.deferred.push_back(ev);
            return;
        }
        if self.revealing() && Self::is_segment_boundary(&ev) {
            self.reveal_drain = true; // release reserve so the tail drains to the end
            self.deferred.push_back(ev);
            return;
        }
        self.apply_agent(ev);
    }

    /// Events that currently call `flush_pending` and thus would dump the tail.
    /// These are the ones worth deferring until the typewriter catches up.
    fn is_segment_boundary(ev: &AgentEvent) -> bool {
        matches!(
            ev,
            AgentEvent::ToolCallStart { .. }
                | AgentEvent::TurnEnd
                | AgentEvent::Error(_)
                | AgentEvent::Notice { .. }
                | AgentEvent::Cancelled
        )
    }

    /// True while the reveal cursor trails the received text (mirrors
    /// `App::revealing`). Keeps deferred boundaries parked until the tail drains.
    fn revealing(&self) -> bool {
        self.revealed < self.pending.chars().count()
    }

    /// Replay parked boundary events in FIFO order once the typewriter has caught
    /// up. A parked `TextChunk` (text after a tool) restarts the reveal, so we
    /// stop and let it play before draining the rest.
    fn drain_deferred(&mut self) {
        while !self.deferred.is_empty() && !self.revealing() {
            let ev = self.deferred.pop_front().unwrap();
            self.apply_agent(ev);
            if self.revealing() {
                break;
            }
        }
        // Still parked events behind a just-revealed TextChunk → keep draining the
        // tail fully (release the jitter reserve) so `revealing()` will clear and
        // the rest of the queue can flush. Without this a boundary's flush_pending
        // resets `reveal_drain`, and the reserve cap (`busy && !reveal_drain` in
        // tick_reveal) would freeze the reveal below total → the queue deadlocks.
        if !self.deferred.is_empty() {
            self.reveal_drain = true;
        }
    }

    fn apply_agent(&mut self, ev: AgentEvent) {
        let p = self.palette;
        match ev {
            AgentEvent::TurnStart => {
                self.status = MockStatus::Thinking;
                self.thinking_tail.clear();
            }
            AgentEvent::TextChunk(s) => {
                self.pending.push_str(&s);
            }
            AgentEvent::TurnEnd => {
                self.flush_pending();
                self.status = MockStatus::Idle;
                self.busy = false;
                self.busy_since = None;
            }
            AgentEvent::ToolCallStart { name, .. } => {
                self.flush_pending();
                self.status = MockStatus::Tool(name.clone());
                let mock_in = mock_tool_input(&name);
                let clock = chrono::Local::now().format("%H:%M").to_string();
                self.pending_tool = Some((name, mock_in, Instant::now(), clock));
            }
            AgentEvent::ToolCallEnd {
                name: _,
                result,
                is_error,
                ..
            } => {
                self.status = MockStatus::Thinking;
                let (tool_name, mock_in, t0, clock) = self
                    .pending_tool
                    .take()
                    .unwrap_or_else(|| (String::new(), None, Instant::now(), String::new()));
                let cw = self.chat_w().saturating_sub(THREAD_GUTTER);
                let full_input = mock_in.unwrap_or("").to_string();
                // Wall time: measured live, recalled by index during replay. The
                // start clock isn't persisted in the sandbox → None on replay.
                let (elapsed, start_clock) = if self.replaying {
                    let d = self
                        .tool_durations
                        .get(self.replay_tool_idx)
                        .copied()
                        .flatten();
                    self.replay_tool_idx += 1;
                    (d, None)
                } else {
                    let d = Some(t0.elapsed());
                    self.tool_durations.push(d);
                    (d, Some(clock))
                };

                let mut blk: Vec<Line<'static>> = Vec::new();
                if let Some(rest) = result.strip_prefix("Edited ") {
                    let (head, diff) = rest.split_once("\n\n").unwrap_or((rest, ""));
                    let path = head.trim_end_matches('.').trim();
                    blk.extend(edit_diff_block(path, diff, cw, p));
                } else {
                    blk.push(tool_box_top(
                        &tool_name,
                        start_clock.as_deref(),
                        elapsed,
                        cw,
                        p,
                    ));

                    if !full_input.is_empty() {
                        blk.push(tool_box_row(
                            "IN",
                            vec![Span::styled(full_input.clone(), st(p.muted, false))],
                            cw,
                            p,
                        ));
                    }

                    if result == "blocked" {
                        blk.push(tool_box_row(
                            "OUT",
                            vec![Span::styled("✗ blocked", st(p.err, false))],
                            cw,
                            p,
                        ));
                    } else {
                        blk.extend(out_preview_rows(&tool_name, &result, is_error, cw, p));
                    }
                }
                let box_start = self.push_block(blk);
                self.timeline.push(MockTimelineEntry {
                    label: tool_name.clone(),
                    clock: start_clock.unwrap_or_default(),
                    target: box_start,
                    is_user: false,
                });
                self.tool_boxes.push(MockToolBox {
                    line_start: box_start,
                    line_end: self.lines.len() - 1,
                    name: tool_name,
                    full_input,
                    full_output: result,
                });
            }
            AgentEvent::Error(e) => {
                self.flush_pending();
                self.pending_tool = None; // no End will come — drop the "running" box
                self.status = MockStatus::Idle;
                self.busy = false;
                self.busy_since = None;
                self.push_block(vec![Line::from(Span::styled(
                    format!("✗ {e}"),
                    st(p.err, false),
                ))]);
            }
            AgentEvent::Notice { text, level } => {
                // Non-failure status (oracle green/retry): success green, info amber.
                self.flush_pending();
                let (glyph, color) = match level {
                    sirbone::NoticeLevel::Success => ("✓", p.success),
                    sirbone::NoticeLevel::Info => ("•", p.info),
                };
                self.push_block(vec![Line::from(Span::styled(
                    format!("{glyph} {text}"),
                    st(color, false),
                ))]);
            }
            AgentEvent::Cancelled => {
                self.flush_pending();
                self.pending_tool = None;
                self.status = MockStatus::Idle;
                self.busy = false;
                self.busy_since = None;
                self.push_block(vec![Line::from(Span::styled(
                    "↩ cancelled",
                    st(p.muted, false),
                ))]);
            }
            AgentEvent::Compacted { messages } => {
                self.push_block(vec![Line::from(Span::styled(
                    format!("· context compacted ({} messages)", messages.len()),
                    st(p.muted, false),
                ))]);
            }
            AgentEvent::WorkspaceSnapshot { .. } => {}
            AgentEvent::JobDone {
                id,
                command,
                exit,
                secs,
            } => {
                self.jobs_running.retain(|(jid, _, _)| *jid != id);
                let ok = exit == Some(0);
                let (mark, color) = if ok {
                    ("✓ ", p.success)
                } else {
                    ("✗ ", p.err)
                };
                let code = exit.map_or_else(|| "?".into(), |c| c.to_string());
                self.push_block(vec![Line::from(vec![
                    Span::styled(mark, st(color, false)),
                    Span::styled(
                        format!(
                            "job #{id} finished · {} · exit {code}",
                            fmt_elapsed(Duration::from_secs(secs))
                        ),
                        st(p.fg, true),
                    ),
                    Span::styled(format!("  — {command}"), st(p.muted, false)),
                ])]);
            }
            AgentEvent::ThinkingStart => {
                self.status = MockStatus::Thinking;
                self.thinking_tail.clear();
            }
            AgentEvent::ThinkingChunk(s) => {
                // Mirrors tui.rs: tail only, feeds the status bar.
                self.thinking_tail.push_str(&s);
                let overflow = self.thinking_tail.chars().count().saturating_sub(200);
                if overflow > 0 {
                    self.thinking_tail = self.thinking_tail.chars().skip(overflow).collect();
                }
            }
            AgentEvent::ContextUsage { .. } => {}
            AgentEvent::SpendUsage { spent, cap } => {
                self.spend_tokens = spent;
                self.spend_cap = Some(cap);
            }
        }
    }

    fn flush_pending(&mut self) {
        self.revealed = 0;
        self.reveal_anchor = None;
        self.reveal_clock = None;
        self.reveal_carry = 0.0;
        self.reveal_rate = 0.0;
        self.reveal_drain = false;
        self.tail_cache = None;
        let md = std::mem::take(&mut self.pending);
        if md.is_empty() {
            return;
        }
        let cw = self.chat_w().saturating_sub(THREAD_GUTTER);
        let lines = md_to_lines(&md, cw, self.palette);
        self.push_block(lines);
    }

    /// Mirrors `App::tick_reveal`: rate-paced typewriter cursor over `pending`
    /// (average production rate + jitter-buffer reserve while streaming).
    fn tick_reveal(&mut self) {
        let total = self.pending.chars().count();
        if total == 0 || self.revealed >= total {
            self.reveal_clock = None;
            self.reveal_carry = 0.0;
            self.reveal_rate = 0.0;
            return;
        }
        const MIN_RATE: f64 = 25.0;
        const MAX_RATE: f64 = 220.0;
        const PACE: f64 = 0.85;
        const TAU: f64 = 1.5;
        const RESERVE_SECS: f64 = 0.6;
        let now = Instant::now();
        let anchor = *self.reveal_anchor.get_or_insert(now);
        let dt = self
            .reveal_clock
            .map_or(0.016, |c| now.duration_since(c).as_secs_f64())
            .min(0.1);
        self.reveal_clock = Some(now);
        if !self.reveal_drain {
            let produced = now.duration_since(anchor).as_secs_f64();
            let measured = if produced > 0.25 {
                total as f64 / produced
            } else {
                MIN_RATE
            };
            let target = (measured * PACE).clamp(MIN_RATE, MAX_RATE);
            if self.reveal_rate <= 0.0 {
                self.reveal_rate = target;
            } else {
                let a = 1.0 - (-dt / TAU).exp();
                self.reveal_rate += (target - self.reveal_rate) * a;
            }
        }
        let rate = if self.reveal_rate > 0.0 {
            self.reveal_rate
        } else {
            MIN_RATE
        };
        self.reveal_carry += rate * dt;
        let cap = if self.busy && !self.reveal_drain {
            total - ((rate * RESERVE_SECS) as usize).min(total / 4)
        } else {
            total
        };
        let target = (self.revealed + self.reveal_carry as usize).min(cap);
        let advanced = target.saturating_sub(self.revealed);
        self.reveal_carry -= advanced as f64;
        if target >= cap {
            self.reveal_carry = self.reveal_carry.min(1.0);
        }
        self.revealed = target;
    }

    /// User-invoked slash command (the `/name` path). `/commit-helper` injects
    /// the demo skill's full body — the same body the model loads via the
    /// `load_skill` tool in the scripted turn.
    fn exec_slash(&mut self, cmd: &str) {
        let p = self.palette;
        let name = cmd.split_whitespace().next().unwrap_or("");
        self.thread_active = false; // fresh block, not attached to a prior rail
        let cw = self.chat_w().saturating_sub(THREAD_GUTTER);
        match name {
            "help" => {
                let mut blk = vec![
                    Line::from(Span::styled("Commands:", st(p.accent, true))),
                    Line::from(vec![
                        Span::styled("  /help", st(p.fg, false)),
                        Span::styled("  — show commands", st(p.muted, false)),
                    ]),
                    Line::from(Span::styled("Skills:", st(p.accent, true))),
                    Line::from(vec![
                        Span::styled(format!("  /{DEMO_SKILL_NAME}"), st(p.fg, false)),
                        Span::styled(format!("  — {DEMO_SKILL_DESC} [local]"), st(p.muted, false)),
                    ]),
                ];
                blk.push(Line::default());
                self.push_block(blk);
            }
            DEMO_SKILL_NAME => {
                let mut blk = vec![Line::from(Span::styled(
                    format!("▌ skill: {DEMO_SKILL_NAME}"),
                    st(p.accent, true),
                ))];
                blk.extend(md_to_lines(DEMO_SKILL_BODY, cw, p));
                self.push_block(blk);
            }
            _ => {
                self.push_block(vec![Line::from(Span::styled(
                    format!("unknown command: /{name}  (try /help)"),
                    st(p.muted, false),
                ))]);
            }
        }
        self.auto_scroll = true;
    }

    fn render(&mut self, f: &mut Frame) {
        if self.settings_mode {
            self.render_settings(f);
            return;
        }
        if self.about_mode {
            self.render_about(f);
            return;
        }

        let p = self.palette;
        let sp = SPINNER[(self.spinner_tick / 6) as usize % SPINNER.len()];
        let out_focused = self.focus == Focus::Output;
        let focus_color = |focused: bool| if focused { p.accent } else { p.muted };

        let [output_area, status_area, input_area, info_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(f.area());

        let (chat_area, timeline_panel) = if self.panel_visible {
            let [chat, panel]: [Rect; 2] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(20), Constraint::Length(self.panel_w)])
                .areas(output_area);
            (chat, Some(panel))
        } else {
            (output_area, None)
        };
        self.chat_area = chat_area;

        // Transient tail rebuilt each frame (mirrors tui.rs): streaming text +
        // in-flight tool box; stored history is never cloned wholesale.
        let mut tail: Vec<Line<'static>> = Vec::new();
        let mut rail_open = self.thread_active;
        let cw = self.chat_w().saturating_sub(THREAD_GUTTER);
        if self.revealed > 0 {
            // Live markdown of the revealed slice, cached by (revealed, cw)
            // (mirrors src/tui/draw.rs).
            let fresh = !matches!(&self.tail_cache,
                Some((r, w, _)) if *r == self.revealed && *w == cw);
            if fresh {
                let shown: String = self.pending.chars().take(self.revealed).collect();
                let lines = md_to_lines(&shown, cw, self.palette);
                self.tail_cache = Some((self.revealed, cw, lines));
            }
            let mut blk = self
                .tail_cache
                .as_ref()
                .map(|(_, _, l)| l.clone())
                .unwrap_or_default();
            match blk.last_mut() {
                Some(last) => last.spans.push(Span::raw("▌")),
                None => blk.push(Line::from(Span::raw("▌"))),
            }
            tail.push(if rail_open {
                thread_blank(p)
            } else {
                Line::default()
            });
            tail.extend(thread_wrap(blk, true, p));
            rail_open = true;
        }
        // In-flight tool: transient "running" box (mirrors tui.rs).
        if let Some((name, mock_in, t0, clock)) = &self.pending_tool {
            let blk = running_tool_block(
                name,
                mock_in.unwrap_or(""),
                sp,
                Some(clock),
                t0.elapsed(),
                cw,
                p,
            );
            tail.push(if rail_open {
                thread_blank(p)
            } else {
                Line::default()
            });
            tail.extend(thread_wrap(blk, true, p));
        }
        let total_n = self.lines.len() + tail.len();
        let total = total_n as u16;
        let viewport = chat_area.height.saturating_sub(2);
        let max_scroll = total.saturating_sub(viewport);
        self.max_scroll = max_scroll;
        // Keep self.scroll in sync with the screen (mirrors tui.rs). A trail-click
        // jump (`scroll_to`) pins its target to the top, past max_scroll.
        self.scroll = if let Some(t) = self.scroll_to.take() {
            self.auto_scroll = false;
            t.min(max_scroll)
        } else if self.auto_scroll {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        let scroll = self.scroll;
        let start = scroll as usize;
        let end = (start + viewport as usize).min(total_n);
        let visible: Vec<Line<'static>> = (start..end)
            .map(|i| {
                if i < self.lines.len() {
                    self.lines[i].clone()
                } else {
                    tail[i - self.lines.len()].clone()
                }
            })
            .collect();
        f.render_widget(
            Paragraph::new(visible).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled(" sirbone — mock ", st(p.muted, false)))
                    .border_style(Style::default().fg(if out_focused {
                        p.muted
                    } else {
                        p.border
                    })),
            ),
            chat_area,
        );
        render_scroll_indicators(f, chat_area, scroll, max_scroll, self.auto_scroll, p);

        if let Some(panel) = timeline_panel {
            self.render_timeline(f, panel);
        } else {
            self.timeline_area = Rect::default();
        }

        // Shortcuts: always visible, pinned to the far right of the status row
        // (mirrors tui.rs). The left side carries the run state.
        let shortcuts =
            "Tab focus  ↑↓ scroll  ⌥B trail  ⌥P palette  ⌥A about  ⌥S settings  ⌥Q quit";
        let busy_for = self
            .busy_since
            .map(|t0| format!(" {}", fmt_elapsed(t0.elapsed())))
            .unwrap_or_default();
        let busy_line = |label: &str| {
            Line::from(vec![
                Span::styled(format!(" {sp} "), st(p.accent, false)),
                Span::styled(format!("{label}…{busy_for}"), st(p.fg, false)),
                Span::styled("   Esc cancel", st(p.muted, false)),
            ])
        };
        let left_line = if self.busy {
            match &self.status {
                MockStatus::Thinking => {
                    let mut line = busy_line("thinking");
                    let tail = self
                        .thinking_tail
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .trim();
                    if !tail.is_empty() {
                        let t: String = tail.chars().take(60).collect();
                        line.spans
                            .push(Span::styled(format!("  ·  {t}"), st(p.muted, false)));
                    }
                    line
                }
                MockStatus::Tool(name) => busy_line(name),
                MockStatus::Idle => Line::default(),
            }
        } else {
            let hint = if self.tool_boxes.is_empty() {
                ""
            } else {
                "  · click box to expand"
            };
            Line::from(Span::styled(format!("  {hint}"), st(p.muted, false)))
        };
        let sc_w = (shortcuts.chars().count() as u16 + 2).min(status_area.width);
        let [left_area, right_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(sc_w)])
            .areas(status_area);
        f.render_widget(Paragraph::new(left_line), left_area);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{shortcuts} "),
                st(p.muted, false),
            )))
            .alignment(Alignment::Right),
            right_area,
        );

        let inp_title = if let Some(q) = &self.queued_input {
            let prev: String = q.chars().take(40).collect();
            let ell = if q.chars().count() > 40 { "…" } else { "" };
            Span::styled(
                format!(" queued: {prev}{ell} · Esc drop "),
                st(p.accent, false),
            )
        } else if self.busy {
            Span::styled(" Esc cancel ", st(p.muted, false))
        } else {
            Span::styled(" › ", st(p.accent, false))
        };
        // Horizontal window over the input (cursor is always at the end here).
        let inner_w = input_area.width.saturating_sub(2) as usize;
        let n_chars = self.input.chars().count();
        let input_skip = n_chars.saturating_sub(inner_w.saturating_sub(1));
        let input_visible: String = self.input.chars().skip(input_skip).take(inner_w).collect();
        f.render_widget(
            Paragraph::new(Span::styled(input_visible, Style::default().fg(p.fg))).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(inp_title)
                    .border_style(Style::default().fg(focus_color(!out_focused))),
            ),
            input_area,
        );

        if self.focus == Focus::Input {
            let max_x = input_area.x + input_area.width.saturating_sub(2);
            let cx = (input_area.x + 1 + (n_chars - input_skip) as u16).min(max_x);
            f.set_cursor_position((cx, input_area.y + 1));
        }

        self.render_info_bar(f, info_area);
        self.render_popup(f);
        if let Some(cmd) = &self.confirm_request {
            render_confirm_dialog(f, cmd, self.palette);
        }
        if self.show_logs {
            self.render_logs(f);
        }
    }

    /// F12 debug overlay (mirrors `App::render_logs`): the tui-logger buffer in a
    /// centered box. Unfiltered — shows everything captured since startup.
    fn render_logs(&self, f: &mut Frame) {
        let p = self.palette;
        let full = f.area();
        let w = full.width.saturating_sub(8).min(120);
        let h = full.height.saturating_sub(4).max(6);
        let area = Rect::new(
            full.x + (full.width.saturating_sub(w)) / 2,
            full.y + (full.height.saturating_sub(h)) / 2,
            w,
            h,
        );
        f.render_widget(Clear, area);
        let widget = tui_logger::TuiLoggerWidget::default()
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(Span::styled(
                        " logs — F12 to close ",
                        Style::default().fg(p.muted),
                    ))
                    .border_style(Style::default().fg(p.border)),
            )
            .output_separator(' ')
            .output_timestamp(Some("%H:%M:%S".to_string()))
            .output_level(Some(tui_logger::TuiLoggerLevelOutput::Abbreviated))
            .output_target(true)
            .output_file(false)
            .output_line(false)
            .style_error(Style::default().fg(p.err))
            .style_warn(Style::default().fg(p.info))
            .style_info(Style::default().fg(p.success))
            .style_debug(Style::default().fg(p.muted))
            .style_trace(Style::default().fg(p.muted))
            .style(Style::default().fg(p.fg).bg(p.bg));
        f.render_widget(widget, area);
    }

    /// Side trail panel (mirrors `App::render_timeline`): scrollable, wrapping,
    /// clickable; left border lights up over the resize grab zone.
    fn render_timeline(&mut self, f: &mut Frame, panel: Rect) {
        let p = self.palette;
        let grab = self.hover_divider || self.resizing_panel;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" trail ", st(p.muted, false)))
            .border_style(Style::default().fg(if grab { p.accent } else { p.border }));
        let inner = block.inner(panel);
        f.render_widget(block, panel);
        self.timeline_area = inner;

        let rows = inner.height as usize;
        let w = inner.width as usize;
        let selected = self.selected_entry.or_else(|| {
            self.timeline
                .iter()
                .rposition(|e| e.target <= self.scroll as usize)
        });

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut owners: Vec<usize> = Vec::new();
        for (idx, e) in self.timeline.iter().enumerate() {
            let last_child = self.timeline.get(idx + 1).is_none_or(|n| n.is_user);
            let (prefix, cont, body) =
                timeline_tree_parts(e.is_user, &e.label, &e.clock, last_child);
            for line in timeline_entry_lines(prefix, cont, &body, selected == Some(idx), w, p) {
                lines.push(line);
                owners.push(idx);
            }
        }
        self.trail_max_scroll = (lines.len().saturating_sub(rows)) as u16;
        self.trail_scroll = self.trail_scroll.min(self.trail_max_scroll);
        let end = lines.len().saturating_sub(self.trail_scroll as usize);
        let start = end.saturating_sub(rows);
        self.timeline_rows = owners[start..end].to_vec();
        f.render_widget(Paragraph::new(lines[start..end].to_vec()), inner);
    }

    fn render_popup(&mut self, f: &mut Frame) {
        let Some(scroll_req) = self.popup.as_ref().map(|pop| pop.scroll) else {
            return;
        };
        let Some(tb) = self
            .popup
            .as_ref()
            .and_then(|pop| self.tool_boxes.get(pop.tool_idx))
        else {
            return;
        };
        let p = self.palette;
        let area = f.area();

        let pw = (area.width * 4 / 5).clamp(40, 120);
        let ph = (area.height * 7 / 10).clamp(8, 50);
        let popup_area = Rect::new(
            area.x + area.width.saturating_sub(pw) / 2,
            area.y + area.height.saturating_sub(ph) / 2,
            pw,
            ph,
        );

        let mut lines: Vec<Line<'static>> = Vec::new();
        if !tb.full_input.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("IN  ", st(p.muted, true)),
                Span::styled(tb.full_input.clone(), st(p.fg, false)),
            ]));
            lines.push(Line::default());
        }
        for (i, raw_line) in tb.full_output.lines().enumerate() {
            let lbl = if i == 0 { "OUT " } else { "    " };
            let color = if raw_line.starts_with("@@") {
                p.purple
            } else if raw_line.starts_with('+') {
                p.success
            } else if raw_line.starts_with('-') {
                p.err
            } else {
                p.fg
            };
            lines.push(Line::from(vec![
                Span::styled(lbl, st(p.muted, i == 0)),
                Span::styled(raw_line.to_string(), st(color, false)),
            ]));
        }
        let title = format!(" ▸ {} ", tb.name);

        // Clamp so scrolling stops at the last content line (mirrors tui.rs).
        let viewport = ph.saturating_sub(2);
        let scroll = scroll_req.min((lines.len() as u16).saturating_sub(viewport));
        if let Some(pop) = &mut self.popup {
            pop.scroll = scroll;
        }

        f.render_widget(Clear, popup_area);
        f.render_widget(
            Paragraph::new(lines)
                .scroll((scroll, 0))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(st(p.accent, false))
                        .title(Span::styled(title, st(p.fg, true)))
                        .title_bottom(Span::styled(
                            " ↑↓ scroll  Esc/click close ",
                            st(p.muted, false),
                        )),
                ),
            popup_area,
        );
    }

    fn render_info_bar(&self, f: &mut Frame, area: Rect) {
        let p = self.palette;
        let filled = (self.ctx_pct as usize * 10 / 100).min(10);
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled));
        let ctx_color = ctx_usage_color(self.ctx_pct);
        let palette_name = PALETTES[self.palette_idx].0;
        let mut spans = vec![
            Span::raw("  "),
            Span::styled("anthropic", st(p.muted, false)),
            Span::styled("  ·  ", st(p.muted, false)),
            Span::styled("claude-opus-4-7", st(p.accent, false)),
            Span::styled("  ·  ctx ", st(p.muted, false)),
            Span::styled(bar, Style::default().fg(ctx_color)),
            Span::styled(
                format!(" {}%", self.ctx_pct),
                Style::default().fg(ctx_color),
            ),
        ];
        // Prompt-cache hit share (mirrors tui.rs). Mock-only: fixed demo value
        // once the simulated context grows past the first turn.
        if self.ctx_pct > 8 {
            spans.push(Span::styled(" ⚡87%", st(p.muted, false)));
        }
        // Token spend cap `tok N/M` (mirrors tui.rs, Feature C); only when enabled.
        if let Some(cap) = self.spend_cap {
            let pct = ((self.spend_tokens * 100) / cap.max(1)).min(100) as u8;
            spans.push(Span::styled(
                format!("  ·  tok {}/{}", fmt_tok(self.spend_tokens), fmt_tok(cap)),
                Style::default().fg(ctx_usage_color(pct)),
            ));
        }
        // Estimated 5-hour quota-window span (mirrors tui.rs). Mock: seeded at
        // launch, gated by the Settings toggle.
        if self.quota_bar {
            let start = self.quota_window_start;
            let end = start + chrono::Duration::hours(5);
            spans.push(Span::styled(
                format!("  ·  win {}→{}", start.format("%H:%M"), end.format("%H:%M")),
                st(p.muted, false),
            ));
        }
        spans.push(Span::styled(
            format!("  ·  theme:{}", palette_name),
            st(p.muted, false),
        ));
        // Live background jobs (mirrors tui.rs). Mock-only: progress is a
        // simulated ramp so the bar/ETA rendering can be eyeballed.
        match self.jobs_running.as_slice() {
            [] => {}
            [(id, cmd, t0)] => {
                let elapsed = t0.elapsed();
                let progress = (elapsed.as_secs_f32() / 40.0).min(0.97);
                let c: String = cmd.chars().take(24).collect();
                let ell = if cmd.chars().count() > 24 { "…" } else { "" };
                spans.push(Span::styled(
                    format!(
                        "  ·  ⚙ #{id} {c}{ell} {}",
                        job_gauge(elapsed, Some(progress))
                    ),
                    st(p.info, false),
                ));
            }
            many => {
                let oldest = many
                    .iter()
                    .map(|(_, _, t0)| t0.elapsed())
                    .max()
                    .unwrap_or_default();
                spans.push(Span::styled(
                    format!("  ·  ⚙ {} jobs {}", many.len(), fmt_elapsed(oldest)),
                    st(p.info, false),
                ));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_about(&self, f: &mut Frame) {
        let area = f.area();
        let p = self.palette;
        f.render_widget(Clear, area);

        let [kb_area, boar_area]: [Rect; 2] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(KB_PANEL_W), Constraint::Min(0)])
            .areas(area);

        let kb_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .title(Span::styled(" keybindings ", st(p.muted, false)));
        let kb_inner = kb_block.inner(kb_area);
        f.render_widget(kb_block, kb_area);
        f.render_widget(
            Paragraph::new(build_mock_kb_lines(p)).wrap(Wrap { trim: false }),
            kb_inner,
        );

        let boar_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .title(Span::styled(" sirbone ", st(p.muted, false)));
        let boar_inner = boar_block.inner(boar_area);
        f.render_widget(boar_block, boar_area);

        let boar_y = boar_inner.y + boar_inner.height.saturating_sub(self.boar.h) / 2;
        let fi = self.boar.frame_idx();
        let frames = self.boar.current_frames();
        if fi < frames.len() {
            f.render_widget(
                BrailleWidget {
                    frame: &frames[fi],
                    x_offset: self.boar.x,
                },
                Rect {
                    x: boar_inner.x,
                    y: boar_y,
                    width: boar_inner.width,
                    height: self.boar.h.min(boar_inner.height),
                },
            );
        }

        if boar_inner.height > 0 {
            f.render_widget(
                Paragraph::new(Span::styled(
                    "  q quit  ·  any key → chat",
                    st(p.muted, false),
                )),
                Rect {
                    x: boar_inner.x,
                    y: boar_inner.y + boar_inner.height.saturating_sub(1),
                    width: boar_inner.width,
                    height: 1,
                },
            );
        }
    }

    fn open_picker(&mut self, kind: MockPickerKind) {
        self.picker = Some(MockPicker {
            kind,
            rows: mock_pick_rows(kind),
            cursor: 0,
        });
    }

    fn toggle_picked(&mut self) {
        if let Some(picker) = &mut self.picker {
            if let Some(row) = picker.rows.get_mut(picker.cursor) {
                row.enabled = !row.enabled;
            }
        }
    }

    fn render_picker(&self, f: &mut Frame) {
        let Some(picker) = &self.picker else { return };
        let area = f.area();
        let p = self.palette;
        f.render_widget(Clear, area);
        let title = match picker.kind {
            MockPickerKind::Skills => " settings · skills ",
            MockPickerKind::Mcp => " settings · mcp ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .title(Span::styled(title, st(p.muted, false)));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let mut lines = vec![
            Line::from(Span::styled(
                "  ↑↓ move · space toggle · esc back · applies next launch",
                st(p.muted, false),
            )),
            Line::default(),
        ];
        for (i, r) in picker.rows.iter().enumerate() {
            let here = i == picker.cursor;
            let mark = if r.enabled { "[x]" } else { "[ ]" };
            let cur = if here { "›" } else { " " };
            let name_color = if r.enabled { p.fg } else { p.muted };
            lines.push(Line::from(vec![
                Span::styled(format!(" {cur} {mark} "), st(p.accent, here)),
                Span::styled(r.name.to_string(), st(name_color, here)),
                Span::styled(r.tag.to_string(), st(p.muted, false)),
            ]));
            if here && !r.description.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("        {}", r.description),
                    st(p.muted, false),
                )));
            }
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_settings(&self, f: &mut Frame) {
        if self.picker.is_some() {
            self.render_picker(f);
            return;
        }
        let area = f.area();
        let p = self.palette;
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .title(Span::styled(" settings ", st(p.muted, false)));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let on = |b: bool| if b { "ON" } else { "OFF" };
        let cur = self.settings_cursor;
        let think = match self.thinking_budget {
            None => "off".to_string(),
            Some(b) => format!("{}k", b / 1000),
        };
        let rows: [(String, &str); 8] = [
            (
                format!("Localize pre-pass:  {}", on(self.localize)),
                "Run the localization pre-pass before each turn.",
            ),
            (
                format!("Plan mode:  {}", on(self.plan)),
                "Record a SPEC via the plan tool before editing code.",
            ),
            (
                format!("Oracle gate:  {}", on(self.oracle)),
                "After the model finishes, run the test gate (loop + rollback).",
            ),
            (
                format!("Quota window bar:  {}", on(self.quota_bar)),
                "Show the estimated 5-hour quota window (start→end) in the info bar.",
            ),
            (
                format!("Architect:  {}", on(self.architect_on)),
                "Consult a second model for a design opinion each turn.",
            ),
            (
                format!("Thinking budget:  {think}"),
                "Extended-thinking token budget — cycle off / 8k / 16k / 32k.",
            ),
            (
                "Skills".to_string(),
                "Choose which skills load for this project.",
            ),
            (
                "MCP".to_string(),
                "Choose which MCP servers load for this project.",
            ),
        ];
        let mut lines = vec![Line::default()];
        for (i, (label, help)) in rows.iter().enumerate() {
            let here = i == cur;
            let mark = if here { "›" } else { " " };
            lines.push(Line::from(vec![
                Span::styled(format!("  {mark}  "), st(p.accent, here)),
                Span::styled(label.clone(), st(p.fg, here)),
            ]));
            if here {
                lines.push(Line::from(Span::styled(
                    format!("        {help}"),
                    st(p.muted, false),
                )));
            }
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  ↑↓ move · ←/→/space toggle · esc → chat",
            st(p.muted, false),
        )));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

// ── mouse ─────────────────────────────────────────────────────────────────────

fn handle_mouse(app: &mut MockApp, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if let Some(p) = &mut app.popup {
                p.scroll = p.scroll.saturating_sub(3);
            } else if over_trail(app, mouse.column) {
                app.trail_scroll = app.trail_scroll.saturating_add(3).min(app.trail_max_scroll);
            } else {
                app.auto_scroll = false;
                app.scroll = app.scroll.saturating_sub(3);
                app.selected_entry = None; // manual scroll → resume auto-follow
            }
        }
        MouseEventKind::ScrollDown => {
            if let Some(p) = &mut app.popup {
                p.scroll = p.scroll.saturating_add(3);
            } else if over_trail(app, mouse.column) {
                app.trail_scroll = app.trail_scroll.saturating_sub(3);
            } else {
                app.scroll = app.scroll.saturating_add(3);
                app.selected_entry = None; // manual scroll → resume auto-follow
                if app.scroll >= app.max_scroll {
                    app.auto_scroll = true;
                }
            }
        }
        // Light up the divider when the pointer is over its grab column.
        MouseEventKind::Moved => app.hover_divider = on_divider(app, mouse.column),
        // Drag the chat/panel divider to resize the side panel on the fly.
        MouseEventKind::Drag(MouseButton::Left) if app.resizing_panel => {
            resize_panel(app, mouse.column);
        }
        MouseEventKind::Up(MouseButton::Left) => app.resizing_panel = false,
        MouseEventKind::Down(MouseButton::Left) => {
            app.popup = None;
            // Grab the divider (panel's left border column) to start a resize.
            if on_divider(app, mouse.column) {
                app.resizing_panel = true;
                return;
            }
            // Click in the trail panel: scroll the chat to that step.
            let ta = app.timeline_area;
            if ta.width > 0
                && mouse.row >= ta.y
                && mouse.row < ta.y + ta.height
                && mouse.column >= ta.x
                && mouse.column < ta.x + ta.width
            {
                if let Some(&idx) = app.timeline_rows.get((mouse.row - ta.y) as usize) {
                    if let Some(entry) = app.timeline.get(idx) {
                        app.scroll_to = Some(entry.target as u16);
                        app.selected_entry = Some(idx);
                    }
                }
                return;
            }
            let ca = app.chat_area;
            if ca.height >= 2
                && mouse.row > ca.y
                && mouse.row < ca.y + ca.height - 1
                && mouse.column > ca.x
                && mouse.column < ca.x + ca.width - 1
            {
                let content_row = (mouse.row - ca.y - 1) as usize + app.scroll as usize;
                if let Some(idx) = app
                    .tool_boxes
                    .iter()
                    .position(|tb| content_row >= tb.line_start && content_row <= tb.line_end)
                {
                    app.popup = Some(MockPopup {
                        tool_idx: idx,
                        scroll: 0,
                    });
                }
            }
        }
        _ => {}
    }
}

/// The resize grab column: the chat/panel divider (±1). Mirrors `run::on_divider`.
fn on_divider(app: &MockApp, col: u16) -> bool {
    if !app.panel_visible {
        return false;
    }
    let divider = app.chat_area.x + app.chat_area.width;
    col + 1 >= divider && col <= divider + 1
}

/// Whether the pointer column is over the side trail panel (scroll routing).
fn over_trail(app: &MockApp, col: u16) -> bool {
    app.panel_visible && col >= app.chat_area.x + app.chat_area.width
}

/// Resize the side panel so its left edge tracks the cursor (mirrors `run::resize_panel`).
fn resize_panel(app: &mut MockApp, cursor_col: u16) {
    let term_w = ratatui::crossterm::terminal::size()
        .map(|(w, _)| w)
        .unwrap_or(80);
    let new_w = term_w.saturating_sub(cursor_col).clamp(
        TIMELINE_W_MIN,
        TIMELINE_W_MAX.min(term_w.saturating_sub(22)),
    );
    if new_w != app.panel_w {
        app.panel_w = new_w;
        app.replay(); // re-wrap chat to the new width
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn set_cursor_color(input_focused: bool, accent: ratatui::style::Color) {
    let esc = if input_focused {
        format!("\x1b]12;{}\x1b\\", color_to_hex(accent))
    } else {
        "\x1b]112\x1b\\".to_string()
    };
    let _ = write!(io::stdout(), "{esc}");
    let _ = io::stdout().flush();
}

fn main() -> anyhow::Result<()> {
    // Mirror the real TUI: tui-logger captures tracing events for the F12 panel,
    // and a panic hook restores the terminal before the crash message prints.
    {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tui_logger::TuiTracingSubscriberLayer)
            .init();
        tui_logger::init_logger(tui_logger::LevelFilter::Trace)?;
        tui_logger::set_default_level(tui_logger::LevelFilter::Info);
    }
    // Seed a few events so the F12 overlay shows something in the sandbox.
    tracing::info!(target: "mock", "sandbox started");
    tracing::warn!(target: "mock", "this is a sample warning");
    tracing::error!(target: "mock", "this is a sample error");

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        prev(info);
    }));

    let recording = std::env::var_os("SIRBONE_RECORDING").is_some();
    enable_raw_mode()?;
    if recording {
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;
    } else {
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    }
    let result = run(
        &mut Terminal::new(CrosstermBackend::new(io::stdout()))?,
        recording,
    );
    disable_raw_mode()?;
    execute!(io::stdout(), Show, DisableMouseCapture, LeaveAlternateScreen)?;
    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    recording: bool,
) -> anyhow::Result<()> {
    let mut app = MockApp::new();
    loop {
        app.tick();
        app.tick_reveal();
        app.drain_deferred(); // PROTOTYPE: replay parked boundaries once text drained
        if !recording || app.frame == 1 || app.frame.is_multiple_of(4) {
            terminal.draw(|f| app.render(f))?;
            if !app.about_mode && !recording {
                set_cursor_color(app.focus == Focus::Input, app.palette.accent);
            }
        }

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Resize(_, _) => app.replay(), // re-wrap chat to the new width
                Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
                Event::Key(key) => {
                    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                    let alt = key.modifiers.contains(KeyModifiers::ALT);
                    // F12 toggles the debug log overlay from any mode (mirrors tui).
                    if key.code == KeyCode::F(12) {
                        app.show_logs = !app.show_logs;
                    } else if app.about_mode {
                        match (key.code, shift) {
                            (KeyCode::Char('q'), _) if alt => break,
                            (KeyCode::Esc, _) => break,
                            _ => app.about_mode = false,
                        }
                    } else if app.settings_mode {
                        if app.picker.is_some() {
                            match key.code {
                                KeyCode::Up => {
                                    if let Some(pk) = &mut app.picker {
                                        pk.cursor = pk.cursor.saturating_sub(1);
                                    }
                                }
                                KeyCode::Down => {
                                    if let Some(pk) = &mut app.picker {
                                        if pk.cursor + 1 < pk.rows.len() {
                                            pk.cursor += 1;
                                        }
                                    }
                                }
                                KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Right => {
                                    app.toggle_picked()
                                }
                                KeyCode::Esc | KeyCode::Left => app.picker = None,
                                _ => {}
                            }
                            terminal.draw(|f| app.render(f))?;
                            continue;
                        }
                        // Arrow-key nav (mirrors src/tui/run.rs): ↑/↓ move, ←/→/space/enter
                        // act on the focused row, esc backs out.
                        let act = |app: &mut MockApp, row: MockSettingsRow, fwd: bool| match row {
                            MockSettingsRow::Localize => app.localize = !app.localize,
                            MockSettingsRow::Plan => app.plan = !app.plan,
                            MockSettingsRow::Oracle => app.oracle = !app.oracle,
                            MockSettingsRow::QuotaBar => app.quota_bar = !app.quota_bar,
                            MockSettingsRow::Architect => app.architect_on = !app.architect_on,
                            MockSettingsRow::Thinking => {
                                app.thinking_budget = if fwd {
                                    match app.thinking_budget {
                                        None => Some(8000),
                                        Some(b) if b < 16000 => Some(16000),
                                        Some(b) if b < 32000 => Some(32000),
                                        _ => None,
                                    }
                                } else {
                                    match app.thinking_budget {
                                        None => Some(32000),
                                        Some(b) if b > 16000 => Some(16000),
                                        Some(b) if b > 8000 => Some(8000),
                                        _ => None,
                                    }
                                };
                            }
                            MockSettingsRow::Skills => {
                                if fwd {
                                    app.open_picker(MockPickerKind::Skills);
                                }
                            }
                            MockSettingsRow::Mcp => {
                                if fwd {
                                    app.open_picker(MockPickerKind::Mcp);
                                }
                            }
                        };
                        match (key.code, shift) {
                            (KeyCode::Char('q'), _) if alt => break,
                            (KeyCode::Esc, _) => app.settings_mode = false,
                            (KeyCode::Up, _) => {
                                app.settings_cursor = app.settings_cursor.saturating_sub(1)
                            }
                            (KeyCode::Down, _)
                                if app.settings_cursor + 1 < MOCK_SETTINGS_ROWS.len() =>
                            {
                                app.settings_cursor += 1;
                            }
                            (
                                KeyCode::Left
                                | KeyCode::Right
                                | KeyCode::Char(' ')
                                | KeyCode::Enter,
                                _,
                            ) => {
                                let fwd = key.code != KeyCode::Left;
                                let row = MOCK_SETTINGS_ROWS[app.settings_cursor];
                                act(&mut app, row, fwd);
                            }
                            _ => {}
                        }
                    } else {
                        // ── confirm dialog intercept ──────────────────────
                        if app.confirm_request.is_some() {
                            match key.code {
                                KeyCode::Char('y')
                                | KeyCode::Char('Y')
                                | KeyCode::Char('n')
                                | KeyCode::Char('N')
                                | KeyCode::Esc => {
                                    app.confirm_request = None;
                                }
                                _ => {}
                            }
                        } else
                        // ── popup key intercept ───────────────────────────
                        if app.popup.is_some() {
                            match key.code {
                                KeyCode::Esc => {
                                    app.popup = None;
                                }
                                KeyCode::Up => {
                                    if let Some(p) = &mut app.popup {
                                        p.scroll = p.scroll.saturating_sub(1);
                                    }
                                }
                                KeyCode::Down => {
                                    if let Some(p) = &mut app.popup {
                                        p.scroll += 1;
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            match (key.code, shift) {
                                (KeyCode::Char('q'), _) if alt => break,
                                // First Esc drops a queued message; otherwise quit.
                                (KeyCode::Esc, _) if app.queued_input.is_some() => {
                                    app.queued_input = None;
                                }
                                (KeyCode::Esc, _) => break,
                                (KeyCode::Char('b'), _) if alt => {
                                    app.panel_visible = !app.panel_visible;
                                    app.replay(); // re-wrap chat to the new width
                                }
                                (KeyCode::Char('p'), _) if alt => {
                                    app.palette_idx = (app.palette_idx + 1) % PALETTES.len();
                                    app.palette = &PALETTES[app.palette_idx].1;
                                    app.replay();
                                }
                                (KeyCode::Char('a'), _) if alt => app.about_mode = true,
                                (KeyCode::Char('s'), _) if alt => {
                                    app.settings_mode = true;
                                    app.picker = None;
                                }
                                (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                                    app.focus = if app.focus == Focus::Input {
                                        Focus::Output
                                    } else {
                                        Focus::Input
                                    };
                                }
                                (KeyCode::Down, _) if app.focus == Focus::Output => {
                                    app.scroll = app.scroll.saturating_add(1);
                                    if app.scroll >= app.max_scroll {
                                        app.auto_scroll = true;
                                    }
                                    app.selected_entry = None;
                                }
                                (KeyCode::Up, _) if app.focus == Focus::Output => {
                                    app.auto_scroll = false;
                                    app.scroll = app.scroll.saturating_sub(1);
                                    app.selected_entry = None;
                                }
                                (KeyCode::Up, _) if app.focus == Focus::Input => {
                                    app.history_prev();
                                }
                                (KeyCode::Down, _) if app.focus == Focus::Input => {
                                    app.history_next();
                                }
                                (KeyCode::PageUp, _) => {
                                    app.auto_scroll = false;
                                    app.scroll = app.scroll.saturating_sub(10);
                                    app.selected_entry = None;
                                }
                                (KeyCode::PageDown, _) => {
                                    app.auto_scroll = false;
                                    app.scroll = app.scroll.saturating_add(10);
                                    app.selected_entry = None;
                                }
                                // `(_, _)` on purpose: uppercase letters arrive with the
                                // SHIFT modifier set, a `shift == false` guard eats them.
                                (KeyCode::Char('G'), _) if app.focus == Focus::Output => {
                                    app.auto_scroll = true;
                                    app.selected_entry = None;
                                }
                                (KeyCode::Char(c), _) if app.focus == Focus::Input => {
                                    app.history_idx = None;
                                    app.input.push(c);
                                }
                                (KeyCode::Backspace, _) if app.focus == Focus::Input => {
                                    app.history_idx = None;
                                    app.input.pop();
                                }
                                (KeyCode::Enter, _)
                                    if app.focus == Focus::Input && !app.input.is_empty() =>
                                {
                                    let text = std::mem::take(&mut app.input);
                                    app.push_history(&text);
                                    if app.busy && app.queued_input.is_none() {
                                        app.queued_input = Some(text);
                                    } else if let Some(cmd) = text.strip_prefix('/') {
                                        app.exec_slash(cmd);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                } // Event::Key
                _ => {}
            } // match event::read()
        }
    }
    Ok(())
}
