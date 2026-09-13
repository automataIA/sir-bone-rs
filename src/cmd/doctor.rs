//! `sirbone doctor`: local setup check, optionally probing provider endpoints.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use sirbone::{
    agent::LlmClient,
    ai::{AnthropicClient, OpenAiClient},
    tools::ToolRegistry,
    types::{ContentBlock, Message},
};

use crate::{make_tools, Cli};

pub(crate) fn provider_env(cli: &Cli) -> (&'static str, bool, String) {
    let anthropic = cli
        .anthropic_key
        .as_ref()
        .or_else(|| cli.api_key.as_ref().filter(|_| false))
        .is_some()
        || std::env::var_os("ANTHROPIC_AUTH_TOKEN").is_some()
        || std::env::var_os("ANTHROPIC_API_KEY").is_some();
    if anthropic {
        let base = cli
            .anthropic_base_url
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.anthropic.com".into());
        return ("anthropic", true, base);
    }
    let openai = cli.api_key.is_some() || std::env::var_os("OPENAI_API_KEY").is_some();
    let base = cli
        .base_url
        .clone()
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .unwrap_or_else(|| "https://api.openai.com/v1".into());
    ("openai", openai, base)
}

fn json_file_status(path: Option<PathBuf>, label: &str) -> (bool, String) {
    let Some(path) = path else {
        return (false, format!("{label}: unavailable"));
    };
    if !path.exists() {
        return (true, format!("{label}: missing ({})", path.display()));
    }
    match std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).context("parse json"))
    {
        Ok(_) => (true, format!("{label}: valid ({})", path.display())),
        Err(e) => (
            false,
            format!("{label}: invalid ({}) — {e}", path.display()),
        ),
    }
}

fn doctor_line(ok: bool, msg: impl AsRef<str>) {
    println!("{} {}", if ok { "ok" } else { "WARN" }, msg.as_ref());
}

pub async fn run_doctor(cwd: &Path, cli: &Cli, system_prompt_chars: usize) -> Result<()> {
    println!("Sir Bone doctor");
    println!("cwd: {}", cwd.display());
    println!("version: {}", sirbone::VERSION);

    let mut warnings = 0usize;
    let mut check = |ok: bool, msg: String| {
        if !ok {
            warnings += 1;
        }
        doctor_line(ok, msg);
    };

    let (provider, has_key, base) = provider_env(cli);
    // Flag a redirected base URL instead of printing it like any other setting:
    // pointing it at another host sends the API key there, and the failure mode
    // of that class of bug (CVE-2026-21852) is that nobody notices. Not a warning
    // — third-party endpoints (z.ai, Groq, Ollama) are a supported setup.
    let default_base = if provider == "anthropic" {
        "https://api.anthropic.com"
    } else {
        "https://api.openai.com/v1"
    };
    let base_note = if base == default_base {
        String::new()
    } else {
        " (non-default — the API key is sent here)".into()
    };
    check(
        has_key,
        if has_key {
            format!("provider: {provider} key present, base {base}{base_note}")
        } else {
            format!("provider: no API key found for {provider}; set ANTHROPIC_AUTH_TOKEN or OPENAI_API_KEY")
        },
    );
    // Prompt weight, for the ablation loop: what the model reads before the first
    // user token. `SIRBONE_DISABLE=prompt:*` prices the naked baseline.
    check(
        true,
        format!(
            "system prompt: {} chars (~{} tokens){}",
            system_prompt_chars,
            system_prompt_chars / 4,
            // Set-but-empty disables nothing (see `ablate::disabled`), so it must
            // not claim an ablated prompt — that misreads a whole A/B arm.
            if std::env::var("SIRBONE_DISABLE").is_ok_and(|v| !v.trim().is_empty()) {
                " — ablated by SIRBONE_DISABLE"
            } else {
                ""
            }
        ),
    );

    let model = cli
        .model
        .clone()
        .or_else(|| std::env::var("SIRBONE_MODEL").ok())
        .unwrap_or_else(|| {
            if provider == "anthropic" {
                "claude-opus-4-7"
            } else {
                "gpt-4o-mini"
            }
            .into()
        });
    check(true, format!("model: {model}"));

    for (ok, msg) in [
        json_file_status(sirbone::config::global_path(), "global config"),
        json_file_status(sirbone::config::project_path(), "project config"),
        json_file_status(sirbone::mcp::catalog_path(), "MCP catalog"),
    ] {
        check(ok, msg);
    }

    let instructions = ["AGENTS.md", "CLAUDE.md"]
        .into_iter()
        .find(|name| cwd.join(name).is_file());
    check(
        instructions.is_some(),
        instructions
            .map(|name| format!("project instructions: {name}"))
            .unwrap_or_else(|| {
                "project instructions: missing AGENTS.md/CLAUDE.md (run /init interactively)".into()
            }),
    );

    let project_dir = sirbone::project_store::project_dir(cwd);
    check(true, format!("project state: {}", project_dir.display()));
    check(
        std::env::var_os("SIRBONE_NO_SNAPSHOT").is_none(),
        if std::env::var_os("SIRBONE_NO_SNAPSHOT").is_none() {
            "snapshots: enabled".into()
        } else {
            "snapshots: disabled by SIRBONE_NO_SNAPSHOT".into()
        },
    );

    let enabled_mcp = sirbone::config::mcp_enabled();
    let catalog = sirbone::mcp::read_catalog();
    check(
        enabled_mcp.iter().all(|name| catalog.contains_key(name)),
        if enabled_mcp.is_empty() {
            "MCP enabled servers: none".into()
        } else {
            let missing: Vec<_> = enabled_mcp
                .iter()
                .filter(|name| !catalog.contains_key(*name))
                .cloned()
                .collect();
            if missing.is_empty() {
                format!("MCP enabled servers: {}", enabled_mcp.join(", "))
            } else {
                format!(
                    "MCP enabled servers missing from catalog: {}",
                    missing.join(", ")
                )
            }
        },
    );

    let tools = make_tools(cwd, true);
    let (native_tools, native_schema_tokens) = tools.native_schema_cost();
    check(
        true,
        format!("native tools registered: {native_tools} (~{native_schema_tokens} schema tokens)"),
    );
    // `ask_user` and `todo` need a front-end, so a headless run carries fewer.
    let (headless_tools, headless_tokens) = make_tools(cwd, false).native_schema_cost();
    check(
        true,
        format!("headless run: {headless_tools} tools (~{headless_tokens} schema tokens)"),
    );
    // Per-tool ranking: the schema rides in the cached prefix every turn, so this
    // is the always-on cost of the tool surface. Deciding which tools are worth
    // an A/B starts here — cheap ones are not worth the quota whatever they do.
    for (name, tok) in tools.schema_ranking() {
        println!("   {tok:>5} tok  {name}");
    }

    let search2md = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::process::Command::new("search2md")
            .arg("--version")
            .output(),
    )
    .await;
    match search2md {
        Ok(Ok(output)) if output.status.success() => check(
            true,
            format!(
                "search2md: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            ),
        ),
        Ok(Ok(output)) => check(
            false,
            format!("search2md: --version exited with {}", output.status),
        ),
        Ok(Err(_)) => check(
            false,
            "search2md: missing from PATH (required by web_search)".into(),
        ),
        Err(_) => check(false, "search2md: --version timed out".into()),
    }

    let hooks = sirbone::config::section("hooks");
    let (hook_presets, unknown_hook_presets) = sirbone::checks::configured_presets(hooks.as_ref());
    check(
        hooks.as_ref().map(|v| v.is_object()).unwrap_or(true),
        if hooks.is_some() {
            let active = hook_presets
                .iter()
                .map(|preset| preset.name())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "hooks: configured; presets: {}",
                if active.is_empty() { "none" } else { &active }
            )
        } else {
            "hooks: not configured".into()
        },
    );
    check(
        unknown_hook_presets.is_empty(),
        if unknown_hook_presets.is_empty() {
            "hook presets: all names recognized".into()
        } else {
            format!(
                "hook presets: unknown name(s): {}",
                unknown_hook_presets.join(", ")
            )
        },
    );
    match sirbone::config::spend_cap() {
        Some(max) => check(true, format!("spend cap: enabled, {max} tokens")),
        None => check(true, "spend cap: disabled".into()),
    }

    // Distinguish on-disk from enabled: a SKILL.md dropped in a supported skills
    // root is invisible until listed in the project config's `skills.enabled`.
    let on_disk = sirbone::skills::scan_all_skills();
    let enabled = sirbone::skills::scan_skills();
    let dormant = !on_disk.is_empty() && enabled.is_empty();
    check(
        !dormant,
        format!(
            "skills: {} on disk, {} enabled{}",
            on_disk.len(),
            enabled.len(),
            if dormant {
                " — add names to skills.enabled in the project config"
            } else {
                ""
            }
        ),
    );

    if cli.doctor_network {
        if has_key {
            let client: Arc<dyn LlmClient> = if provider == "anthropic" {
                let key = cli
                    .anthropic_key
                    .clone()
                    .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                    .expect("has_key checked above");
                let c = AnthropicClient::new(&base, &key, &model);
                c.set_thinking_budget(cli.thinking_budget);
                Arc::new(c)
            } else {
                let key = cli
                    .api_key
                    .clone()
                    .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                    .expect("has_key checked above");
                Arc::new(OpenAiClient::new(&base, &key, &model))
            };

            match client.list_models().await {
                Ok(models) => check(
                    true,
                    format!("network models: ok ({} model(s))", models.len()),
                ),
                Err(e) => check(
                    false,
                    format!(
                        "network models: {}",
                        sirbone::ai::redact_secrets(&e.to_string())
                    ),
                ),
            }
            match client.context_window().await {
                Some(n) => check(true, format!("network context window: {n} tokens")),
                None => check(false, "network context window: unknown".into()),
            }

            let probe = [
                Message {
                    role: sirbone::Role::System,
                    injected: false,
                    content: vec![ContentBlock::Text {
                        text: "You are a token-counting probe.".into(),
                    }],
                },
                Message::user("Reply with ok."),
            ];
            match client
                .count_tokens(&probe.iter().collect::<Vec<_>>(), &ToolRegistry::new())
                .await
            {
                Ok(n) => check(true, format!("network token count: {n} tokens")),
                Err(e) => check(
                    false,
                    format!(
                        "network token count: {}",
                        sirbone::ai::redact_secrets(&e.to_string())
                    ),
                ),
            }
        } else {
            check(
                false,
                "network probe: skipped because no provider key is configured".into(),
            );
        }
    }

    if warnings == 0 {
        println!("doctor: ready");
    } else {
        println!("doctor: {warnings} warning(s)");
    }
    Ok(())
}
