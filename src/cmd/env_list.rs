//! `sirbone env` — reference for every environment variable the agent reads,
//! with its current value. The vars live scattered across the codebase (main,
//! tools, agent submodules); this table is the single user-facing index so
//! options are discoverable without reading source. Keep it in sync when adding
//! or removing a `SIRBONE_*` read.

/// `(name, description, secret)` — `secret` masks the value when set.
const VARS: &[(&str, &str, bool)] = &[
    // Provider credentials (see also `sirbone login`).
    (
        "ANTHROPIC_AUTH_TOKEN",
        "Anthropic (or compatible) API token — selects the Anthropic client",
        true,
    ),
    (
        "ANTHROPIC_BASE_URL",
        "Anthropic-compatible endpoint override (z.ai, proxies)",
        false,
    ),
    (
        "OPENAI_API_KEY",
        "OpenAI (or compatible) API key — used when no Anthropic token",
        true,
    ),
    (
        "OPENAI_BASE_URL",
        "OpenAI-compatible endpoint (Ollama, Groq, OpenRouter, …)",
        false,
    ),
    (
        "SIRBONE_CODEX",
        "1 = use ChatGPT Plus/Pro through the official Codex CLI OAuth session",
        false,
    ),
    (
        "SIRBONE_CODEX_SANDBOX",
        "Codex CLI sandbox: read-only, workspace-write (default), or danger-full-access",
        false,
    ),
    (
        "SIRBONE_CODEX_REASONING",
        "Codex CLI reasoning effort override (e.g. low) — else ~/.codex/config.toml decides",
        false,
    ),
    (
        "SIRBONE_STREAM_IDLE_SECS",
        "Seconds of stream silence before a connection is declared stalled (default 90)",
        false,
    ),
    // Core knobs.
    ("SIRBONE_MODEL", "Model id (same as --model)", false),
    (
        "SIRBONE_CONTEXT_WINDOW",
        "Context window override in tokens (else fetched per model)",
        false,
    ),
    (
        "SIRBONE_THINKING_BUDGET",
        "Extended-thinking token budget, Anthropic only (same as --thinking-budget)",
        false,
    ),
    (
        "SIRBONE_TEMPERATURE",
        "Sampling temperature (same as --temperature; unset = provider default)",
        false,
    ),
    (
        "SIRBONE_MAX_STEPS",
        "Safety cap on LLM turns per run (AFK bound; unset = no cap)",
        false,
    ),
    (
        "SIRBONE_VISION",
        "1 = the OpenAI-compatible endpoint reads images (same as --vision)",
        false,
    ),
    (
        "SIRBONE_THEME",
        "CLI/TUI color theme override (else cli.theme config)",
        false,
    ),
    (
        "SIRBONE_WEB_FETCH_ALLOW_PRIVATE",
        "1 = let web_fetch reach private/local addresses (dev servers)",
        false,
    ),
    // Default-on features — set to disable.
    (
        "SIRBONE_NO_SNAPSHOT",
        "Disable pre-mutation workspace snapshots (/rollback)",
        false,
    ),
    (
        "SIRBONE_NO_COMPACT",
        "Disable context compaction (transcript grows unbounded)",
        false,
    ),
    (
        "SIRBONE_NO_HISTORIA",
        "Disable schema-aware project chat history",
        false,
    ),
    (
        "SIRBONE_NO_GROUNDING",
        "Drop the grounding-rules block from the system prompt",
        false,
    ),
    (
        "SIRBONE_NO_GROUND_CONTEXT",
        "Disable proactive grounding context (project-structure pass)",
        false,
    ),
    (
        "SIRBONE_NO_LOCALIZE",
        "Disable the read-only localization pre-pass",
        false,
    ),
    (
        "SIRBONE_NO_EDIT_HINT",
        "Disable the post-edit verification hint on the edit tool",
        false,
    ),
    (
        "SIRBONE_NO_TOOLS",
        "Register no tools at all (model answers from context only)",
        false,
    ),
    (
        "SIRBONE_TOOLS",
        "Comma allowlist: register only these tools (fails closed; MCP included)",
        false,
    ),
    (
        "SIRBONE_IDENTITY",
        "Replace the core identity line of the system prompt (set it with SIRBONE_TOOLS)",
        false,
    ),
    // Opt-in features.
    (
        "SIRBONE_GROUND",
        "1 = deterministic claim grounding after each run (see `sirbone ground`)",
        false,
    ),
    (
        "SIRBONE_ORACLE",
        "1 = enable the configured post-Done oracle in headless/REPL runs (same as --oracle)",
        false,
    ),
    (
        "SIRBONE_REVIEW_ONLY",
        "1 = review-only run: no writes, no MCP, read-only bash (same as --review-only)",
        false,
    ),
    (
        "SIRBONE_ASK_ROUNDS",
        "1 = expose the experimental 1-3 question ask_user schema",
        false,
    ),
    (
        "SIRBONE_PLAN",
        "1 = enable the persistent deterministic task contract (same as --plan)",
        false,
    ),
    (
        "SIRBONE_HOOK",
        "off = the pre-commit review hook skips the run (`sirbone hook install`)",
        false,
    ),
    (
        "SIRBONE_HOOK_TIMEOUT",
        "Seconds the pre-commit hook waits for the review before giving up (default 180)",
        false,
    ),
    (
        "SIRBONE_BEST_OF",
        "K = run the task K times from the same tree and keep the attempt the project's test command scores best (needs oracle.test_command; headless one-shot only)",
        false,
    ),
    (
        "SIRBONE_COMPLETION_CHECK",
        "1 = before the run ends, pull it back once if the model's own todo list is unfinished",
        false,
    ),
    (
        "SIRBONE_READ_OUTLINE",
        "1 = read returns a declaration outline plus elided ranges for long source files",
        false,
    ),
    (
        "SIRBONE_COMPACT_BUDGET",
        "Compact at this absolute token budget instead of at 87.5% of the window",
        false,
    ),
    (
        "SIRBONE_SUMMARY_VERBATIM",
        "1 = the compaction summarizer reuses the turn prefix (system + tools + region) instead of a flattened copy",
        false,
    ),
    (
        "SIRBONE_HASHLINE",
        "1 = replace the edit tool with patch (line-addressed, content-tag anchored)",
        false,
    ),
    (
        "SIRBONE_REF_RANK",
        "1 = code_map find_references spends its extra detail on the most-matched files instead of by path (every file stays visible either way)",
        false,
    ),
    (
        "SIRBONE_TOOL_STDERR",
        "1 = print a `tool-start: <name>` / `tool-end: <name>` line on stderr per tool call (embedder progress)",
        false,
    ),
    // Bench / ablation harness.
    (
        "SIRBONE_USAGE",
        "1 = print the per-feature usage line after a run (bench telemetry)",
        false,
    ),
    (
        "SIRBONE_DISABLE",
        "Feature ablation: hook:{pre,post,stop,ledger}, oracle:gate, ask:rounds, read:outline, stream:rules, tool:spill, test:notice",
        false,
    ),
    (
        "SIRBONE_ASK_AMBIGUOUS",
        "1 = prompt clause: ask before assuming an answer on an under-specified task, and never settle an ambiguity by editing a test (honesty arm; off by default)",
        false,
    ),
    (
        "SIRBONE_YAGNI",
        "1 = minimal-code prompt clause (A/B'd net-negative; off by default)",
        false,
    ),
];

use super::mask;

pub fn run_env_list(json: bool) {
    if json {
        let entries: Vec<serde_json::Value> = VARS
            .iter()
            .map(|(name, desc, secret)| {
                let value = std::env::var(name)
                    .ok()
                    .map(|v| if *secret { mask(&v) } else { v });
                serde_json::json!({"name": name, "description": desc, "value": value})
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(entries))
                .unwrap_or_else(|_| "[]".into())
        );
        return;
    }
    let width = VARS.iter().map(|(n, ..)| n.len()).max().unwrap_or(0);
    println!(
        "Environment variables read by sirbone (set in shell, project .env, or ~/.sirbone/.env):\n"
    );
    for (name, desc, secret) in VARS {
        let value = match std::env::var(name) {
            Ok(v) if *secret => mask(&v),
            Ok(v) => v,
            Err(_) => "(unset)".into(),
        };
        println!("  {name:<width$}  {value:<12}  {desc}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `SIRBONE_*` env var read anywhere in `src/` must have a row here
    /// (discoverability gate). Test-only vars are exempt.
    #[test]
    fn table_covers_all_sirbone_vars_in_source() {
        // `SIRBONE_TOOL*`/`SIRBONE_IS_ERROR` are *exported to* `tusk` filters,
        // not read from the environment, so they belong in the hook docs rather
        // than in a table of inputs the user can set.
        let exempt = [
            "SIRBONE_TEST_INSTR",
            "SIRBONE_REDIR__",
            "SIRBONE_TOOL",
            "SIRBONE_TOOL_INPUT",
            "SIRBONE_IS_ERROR",
        ];
        let mut missing = Vec::new();
        let re = regex_lite();
        for entry in walk_rs(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
            let Ok(text) = std::fs::read_to_string(&entry) else {
                continue;
            };
            for var in re(&text) {
                // Bare "SIRBONE_" (this file's own extractor needle) is not a var.
                if var != "SIRBONE_"
                    && !exempt.contains(&var.as_str())
                    && !VARS.iter().any(|(n, ..)| *n == var)
                    && !missing.contains(&var)
                {
                    missing.push(var);
                }
            }
        }
        assert!(
            missing.is_empty(),
            "SIRBONE vars missing from `sirbone env` table: {missing:?}"
        );
    }

    fn walk_rs(dir: std::path::PathBuf) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        out
    }

    /// Tiny extractor for `SIRBONE_[A-Z_0-9]+` tokens (no regex dep).
    fn regex_lite() -> impl Fn(&str) -> Vec<String> {
        |text: &str| {
            let mut vars = Vec::new();
            let bytes = text.as_bytes();
            let mut i = 0;
            while let Some(pos) = text[i..].find("SIRBONE_") {
                let start = i + pos;
                let mut end = start + "SIRBONE_".len();
                while end < bytes.len()
                    && (bytes[end].is_ascii_uppercase()
                        || bytes[end].is_ascii_digit()
                        || bytes[end] == b'_')
                {
                    end += 1;
                }
                vars.push(text[start..end].to_string());
                i = end;
            }
            vars
        }
    }

    #[test]
    fn mask_never_reveals_short_tokens() {
        assert_eq!(mask("abc"), "•••");
        assert!(mask("sk-verylongtoken123").starts_with("sk-ver"));
        assert!(!mask("sk-verylongtoken123").contains("token"));
    }
}
