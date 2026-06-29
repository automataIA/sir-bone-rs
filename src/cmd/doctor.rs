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

pub async fn run_doctor(cwd: &Path, cli: &Cli) -> Result<()> {
    println!("Sir Bone doctor");
    println!("cwd: {}", cwd.display());
    println!("version: {}", env!("CARGO_PKG_VERSION"));

    let mut warnings = 0usize;
    let mut check = |ok: bool, msg: String| {
        if !ok {
            warnings += 1;
        }
        doctor_line(ok, msg);
    };

    let (provider, has_key, base) = provider_env(cli);
    check(
        has_key,
        if has_key {
            format!("provider: {provider} key present, base {base}")
        } else {
            format!("provider: no API key found for {provider}; set ANTHROPIC_AUTH_TOKEN or OPENAI_API_KEY")
        },
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

    let tools = make_tools(cwd);
    let native_tools = tools
        .iter()
        .filter(|t| !t.name().starts_with("mcp__"))
        .count();
    check(true, format!("native tools registered: {native_tools}"));

    let hooks = sirbone::config::section("hooks");
    check(
        hooks.as_ref().map(|v| v.is_object()).unwrap_or(true),
        if hooks.is_some() {
            "hooks: configured".into()
        } else {
            "hooks: not configured".into()
        },
    );
    match sirbone::config::spend_cap() {
        Some(max) => check(true, format!("spend cap: enabled, {max} tokens")),
        None => check(true, "spend cap: disabled".into()),
    }

    let skills = sirbone::skills::scan_skills();
    check(true, format!("skills discovered: {}", skills.len()));

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
