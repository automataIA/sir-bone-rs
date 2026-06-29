pub mod anthropic;
pub mod client;

pub use anthropic::AnthropicClient;
pub use client::OpenAiClient;

/// Manual context-window override in tokens, for providers whose API doesn't
/// expose it (Ollama OpenAI-compat, bare proxies).
pub(crate) fn env_context_window() -> Option<u32> {
    std::env::var("SIRBONE_CONTEXT_WINDOW").ok()?.parse().ok()
}

/// Max send+stream attempts per turn before giving up (shared by both clients).
pub(crate) const MAX_ATTEMPTS: u32 = 5;

/// Capped exponential backoff in seconds for a 1-based attempt: 1,2,4,8,16…≤30.
pub(crate) fn backoff_secs(attempt: u32) -> u64 {
    (1u64 << attempt.saturating_sub(1).min(5)).min(30)
}

/// Read a JSON numeric field as u64, tolerating providers that emit it as a
/// float (`131072.0`) instead of an integer. `as_u64()` alone returns None on
/// a float, which would silently drop the real context window.
pub(crate) fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))
}

const SECRET_ENV_VARS: [&str; 4] = [
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "SIRBONE_ARCHITECT_API_KEY",
];

/// Replace any known API key/token value with `[redacted]`. Applied to error
/// strings before they reach tracing logs, so a server or proxy that echoes
/// credentials back can't leak them into log files.
pub fn redact_secrets(text: &str) -> String {
    static SECRETS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let secrets = SECRETS.get_or_init(|| {
        SECRET_ENV_VARS
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            // Too-short values would redact unrelated text (e.g. a key set to "test").
            .filter(|v| v.len() >= 8)
            .collect()
    });
    redact_with(text, secrets)
}

fn redact_with(text: &str, secrets: &[String]) -> String {
    secrets.iter().fold(text.to_string(), |acc, s| {
        acc.replace(s.as_str(), "[redacted]")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_replaces_every_occurrence_of_each_secret() {
        let secrets = vec!["sk-ant-abc123".to_string(), "sk-oai-xyz789".to_string()];
        let out = redact_with(
            "401 for key sk-ant-abc123 (sk-ant-abc123), alt sk-oai-xyz789",
            &secrets,
        );
        assert_eq!(out, "401 for key [redacted] ([redacted]), alt [redacted]");
    }

    #[test]
    fn redact_leaves_clean_text_untouched() {
        let secrets = vec!["sk-ant-abc123".to_string()];
        assert_eq!(
            redact_with("connection refused", &secrets),
            "connection refused"
        );
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(backoff_secs(1), 1);
        assert_eq!(backoff_secs(3), 4);
        assert!(backoff_secs(20) <= 30);
    }
}
