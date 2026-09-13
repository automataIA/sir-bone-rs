//! `sirbone login` — interactive credential wizard. Picks a provider preset
//! (base URL + which env var + Anthropic/OpenAI path), writes `~/.sirbone/.env`
//! (chmod 600), and pings the endpoint to confirm the key works. Every preset
//! rides one of the two API clients, while ChatGPT Plus/Pro uses the official
//! Codex CLI OAuth flow. Falls back to seeding the template when stdin is not a
//! TTY (CI/headless).

use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use sirbone::{
    agent::LlmClient,
    ai::{redact_secrets, AnthropicClient, OpenAiClient},
    config,
};

/// Authenticate the official Codex CLI with a ChatGPT Plus/Pro account.
/// Sir Bone intentionally does not read or persist the OAuth refresh token.
pub async fn login_codex() -> Result<()> {
    println!("Starting the official Codex login flow…");
    println!("Choose ‘Sign in with ChatGPT’ in the browser, then return here.");
    let status = tokio::process::Command::new("codex")
        .arg("login")
        .status()
        .await
        .context("start `codex login` (install the official Codex CLI first)")?;
    if !status.success() {
        anyhow::bail!("`codex login` exited with status {status}");
    }
    println!("ChatGPT/Codex authentication completed. Verify with `codex login status`.");
    Ok(())
}

/// Where a preset's base URL comes from.
enum Base {
    /// Provider's official endpoint — not written (the client defaults to it).
    Default,
    /// A fixed OpenAI-/Anthropic-compatible endpoint.
    Fixed(&'static str),
    /// Ask the user (Ollama, self-hosted, any OpenAI-compatible proxy).
    Prompt,
}

/// A provider preset. `anthropic` selects the client path (Messages API with
/// caching/thinking vs OpenAI-compatible chat completions); it also decides
/// which env var the token is written to (`ANTHROPIC_AUTH_TOKEN` / `OPENAI_API_KEY`).
struct Preset {
    label: &'static str,
    anthropic: bool,
    base: Base,
    default_model: &'static str,
}

const PRESETS: &[Preset] = &[
    Preset {
        label: "Anthropic (Claude — prompt caching + extended thinking)",
        anthropic: true,
        base: Base::Default,
        default_model: "claude-opus-4-7",
    },
    Preset {
        label: "z.ai / GLM (Anthropic-compatible — caching + thinking)",
        anthropic: true,
        base: Base::Fixed("https://api.z.ai/api/anthropic"),
        default_model: "glm-5.2",
    },
    Preset {
        label: "OpenRouter (one key, most models)",
        anthropic: false,
        base: Base::Fixed("https://openrouter.ai/api/v1"),
        default_model: "anthropic/claude-opus-4-8",
    },
    Preset {
        label: "Groq (fast, free tier)",
        anthropic: false,
        base: Base::Fixed("https://api.groq.com/openai/v1"),
        default_model: "llama-3.3-70b-versatile",
    },
    Preset {
        label: "Google AI Studio (free tier, no card)",
        anthropic: false,
        base: Base::Fixed("https://generativelanguage.googleapis.com/v1beta/openai/"),
        default_model: "gemini-2.5-flash",
    },
    Preset {
        label: "OpenAI",
        anthropic: false,
        base: Base::Default,
        default_model: "gpt-4o-mini",
    },
    Preset {
        label: "Custom OpenAI-compatible (Ollama, Mistral, …)",
        anthropic: false,
        base: Base::Prompt,
        default_model: "",
    },
];

pub async fn run_login() -> Result<()> {
    let path = config::global_env_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;

    // Non-interactive (CI, piped stdin): keep the old seed-and-print behavior.
    if !io::stdin().is_terminal() {
        let info = config::ensure_global_env().context("seeding ~/.sirbone/.env")?;
        if info.created {
            println!("Created {} (chmod 600).", info.path.display());
        } else {
            println!(
                "Found existing {} — leaving it untouched.",
                info.path.display()
            );
        }
        println!("Edit it and fill in ONE provider, then run `sirbone`:\n");
        print!("{}", config::ENV_TEMPLATE);
        return Ok(());
    }

    let fresh = !path.exists();
    if !fresh {
        println!("{} already exists.", path.display());
        if !ask_yes_no("Reconfigure (overwrites credentials)?", false)? {
            println!("Leaving the file as is.");
            return Ok(());
        }
    } else if let Some((token_key, token, base, anthropic, model)) = env_pickup() {
        // Smart pickup: a key already exported in the shell (or a project .env)
        // — offer to persist it globally instead of asking to paste it again.
        println!(
            "Found credentials in the environment: {token_key}={}",
            mask(&token)
        );
        if ask_yes_no("Save them to ~/.sirbone/.env?", true)? {
            return finish(&path, token_key, &token, base.as_deref(), anthropic, &model).await;
        }
    }

    println!("\nPick a provider:");
    for (i, p) in PRESETS.iter().enumerate() {
        println!("  {}) {}", i + 1, p.label);
    }
    let choice = loop {
        let s = prompt_line("> ")?;
        match s.trim().parse::<usize>() {
            Ok(n) if (1..=PRESETS.len()).contains(&n) => break &PRESETS[n - 1],
            _ => println!("Enter a number 1–{}.", PRESETS.len()),
        }
    };

    let token = loop {
        let t = prompt_line("Paste the API key / token: ")?;
        let t = t.trim();
        if !t.is_empty() {
            break t.to_string();
        }
        println!("Empty token.");
    };

    let base: Option<String> = match choice.base {
        Base::Default => None,
        Base::Fixed(u) => Some(u.to_string()),
        Base::Prompt => Some(loop {
            let b = prompt_line("Base URL (e.g. http://localhost:11434/v1): ")?;
            let b = b.trim();
            if !b.is_empty() {
                break b.to_string();
            }
            println!("Empty URL.");
        }),
    };

    let model = {
        let d = choice.default_model;
        let shown = if d.is_empty() { "required" } else { d };
        let m = prompt_line(&format!("Model [{shown}]: "))?;
        let m = m.trim();
        if m.is_empty() {
            d.to_string()
        } else {
            m.to_string()
        }
    };

    let token_key = if choice.anthropic {
        "ANTHROPIC_AUTH_TOKEN"
    } else {
        "OPENAI_API_KEY"
    };
    finish(
        &path,
        token_key,
        &token,
        base.as_deref(),
        choice.anthropic,
        &model,
    )
    .await
}

/// Non-interactive credential set for front-ends (the VS Code extension, scripts).
/// The token is read from **stdin** — never argv, so it can't leak via `ps` or
/// shell history. Reuses [`finish`] (write `~/.sirbone/.env` at 0600 + probe), so
/// there is one code path for the secure write, shared with the interactive wizard.
pub async fn set_credentials(
    provider: &str,
    base: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    let path = config::global_env_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let mut token = String::new();
    io::stdin()
        .read_to_string(&mut token)
        .context("read token from stdin")?;
    let token = token.trim();
    if token.is_empty() {
        anyhow::bail!("empty token on stdin");
    }
    let anthropic = match provider {
        "anthropic" => true,
        "openai" => false,
        other => anyhow::bail!("unknown provider '{other}' (use anthropic|openai)"),
    };
    let token_key = if anthropic {
        "ANTHROPIC_AUTH_TOKEN"
    } else {
        "OPENAI_API_KEY"
    };
    let default_model = if anthropic {
        "claude-opus-4-7"
    } else {
        "gpt-4o-mini"
    };
    let model = model.filter(|m| !m.is_empty()).unwrap_or(default_model);
    finish(&path, token_key, token, base, anthropic, model).await
}

/// Credentials already present in the process env (shell export or project
/// `.env`), returned as `(token_key, token, base_url, anthropic, model)`.
fn env_pickup() -> Option<(&'static str, String, Option<String>, bool, String)> {
    let nonempty = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    if let Some(tok) = nonempty("ANTHROPIC_AUTH_TOKEN") {
        let model = nonempty("SIRBONE_MODEL").unwrap_or_else(|| "claude-opus-4-7".into());
        return Some((
            "ANTHROPIC_AUTH_TOKEN",
            tok,
            nonempty("ANTHROPIC_BASE_URL"),
            true,
            model,
        ));
    }
    if let Some(tok) = nonempty("OPENAI_API_KEY") {
        let model = nonempty("SIRBONE_MODEL").unwrap_or_else(|| "gpt-4o-mini".into());
        return Some((
            "OPENAI_API_KEY",
            tok,
            nonempty("OPENAI_BASE_URL"),
            false,
            model,
        ));
    }
    None
}

/// Write the `.env` and probe the endpoint so the user gets immediate feedback.
async fn finish(
    path: &Path,
    token_key: &str,
    token: &str,
    base: Option<&str>,
    anthropic: bool,
    model: &str,
) -> Result<()> {
    write_env(path, token_key, token, anthropic, base, model)?;
    println!("\nWrote {} (chmod 600).", path.display());

    let test_base = base.map(str::to_string).unwrap_or_else(|| {
        if anthropic {
            "https://api.anthropic.com".into()
        } else {
            "https://api.openai.com/v1".into()
        }
    });
    print!("Testing connection ({model} @ {test_base})… ");
    io::stdout().flush().ok();
    let client: Arc<dyn LlmClient> = if anthropic {
        Arc::new(AnthropicClient::new(&test_base, token, model))
    } else {
        Arc::new(OpenAiClient::new(&test_base, token, model))
    };
    match tokio::time::timeout(std::time::Duration::from_secs(15), client.list_models()).await {
        Ok(Ok(m)) => println!("ok ({} models).", m.len()),
        Ok(Err(e)) => println!(
            "warning: {} (credentials written; the endpoint may not expose /models — try `sirbone`).",
            redact_secrets(&e.to_string())
        ),
        Err(_) => println!(
            "warning: 15s timeout (endpoint unreachable; credentials written — try `sirbone`)."
        ),
    }
    println!("Done. Start with: sirbone");
    Ok(())
}

fn write_env(
    path: &Path,
    token_key: &str,
    token: &str,
    anthropic: bool,
    base: Option<&str>,
    model: &str,
) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let base_key = if anthropic {
        "ANTHROPIC_BASE_URL"
    } else {
        "OPENAI_BASE_URL"
    };
    let mut body = String::from(
        "# Sir Bone credentials — written by `sirbone login`.\n\
         # Optional flags (thinking, snapshot, …): see .env.example in the repo.\n",
    );
    body.push_str(&format!("{token_key}={token}\n"));
    if let Some(b) = base {
        body.push_str(&format!("{base_key}={b}\n"));
    }
    if !model.is_empty() {
        body.push_str(&format!("SIRBONE_MODEL={model}\n"));
    }
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    Ok(())
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut s = String::new();
    if io::stdin().read_line(&mut s).context("read stdin")? == 0 {
        anyhow::bail!("input closed (EOF) — login cancelled");
    }
    Ok(s)
}

fn ask_yes_no(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let s = prompt_line(&format!("{question} {hint} "))?;
    Ok(match s.trim().to_lowercase().as_str() {
        "" => default_yes,
        // "s"/"si" kept for muscle memory from the old Italian wizard.
        "y" | "yes" | "s" | "si" | "sì" => true,
        _ => false,
    })
}

use super::mask;
