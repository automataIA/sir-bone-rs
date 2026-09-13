pub mod anthropic;
pub mod client;
pub mod codex;

pub use anthropic::AnthropicClient;
pub use client::OpenAiClient;
pub use codex::CodexClient;

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

/// Seconds of stream silence after which a connection is declared stalled.
/// Without it a provider that opens the response and then sends nothing (a
/// mid-stream stall, a silently dropped connection) hangs the caller forever —
/// seen live 2026-09-02: the game showed "thinking" for 15+ minutes with no
/// error. 90s is generous headroom over the slowest legitimate first token
/// (deep-thinking models stream reasoning deltas, so bytes keep flowing) and
/// still lands the failure inside Juno's own 180s call timeout.
pub(crate) const DEFAULT_STREAM_IDLE_SECS: u64 = 90;

/// `SIRBONE_STREAM_IDLE_SECS` override, validated to > 0.
pub(crate) fn stream_idle_secs() -> std::time::Duration {
    std::time::Duration::from_secs(stream_idle_secs_from(
        std::env::var("SIRBONE_STREAM_IDLE_SECS").ok().as_deref(),
    ))
}

fn stream_idle_secs_from(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_STREAM_IDLE_SECS)
}

/// Machine-readable stderr markers (retry progress, stream stalls) for a
/// driving process — the same gate as the tool-start/tool-end markers.
pub(crate) fn stderr_markers_enabled() -> bool {
    std::env::var_os("SIRBONE_TOOL_STDERR").is_some()
}

/// The gateway's HTTP client: an IDLE-read cap instead of a total-duration
/// cap. A provider that goes silent (stalled headers, dead body) must fail
/// visibly — a slow-but-flowing stream runs as long as it needs.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .read_timeout(stream_idle_secs())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Read a JSON numeric field as u64, tolerating providers that emit it as a
/// float (`131072.0`) instead of an integer. `as_u64()` alone returns None on a
/// float, which would silently drop the real context window.
pub(crate) fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))
}

/// True when the model is a GLM served the z.ai way, where the thinking-budget
/// dial maps onto reasoning-effort levels instead of an Anthropic token budget.
///
/// Live-checked against api.z.ai: on the Anthropic-compatible endpoint
/// `budget_tokens` alone just selects a heavy default, and `thinking.type:
/// disabled` / `reasoning_effort` are the only dials that actually change the
/// reasoning volume. Model name is checked as well as the host so a GLM behind
/// an OpenRouter-style prefixed id (`z-ai/glm-5.2`) is caught too.
pub(crate) fn is_glm(base_url: &str, model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    if model.starts_with("glm") || model.contains("/glm") {
        return true;
    }
    let host = url::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_default();
    host == "api.z.ai"
        || host.ends_with(".z.ai")
        || host == "api.bigmodel.cn"
        || host.ends_with(".bigmodel.cn")
        || host.ends_with(".bigmodel.com")
}

/// The z.ai reasoning-effort level the thinking-budget dial stands for, for
/// display. The dial's None is "light", not full off: z.ai converts every
/// disable spelling to a lightweight-thinking low, so no client of theirs can
/// turn reasoning off entirely.
pub(crate) fn glm_effort_label(budget: Option<u32>) -> &'static str {
    match budget {
        None => "light",
        Some(b) if b <= 8000 => "low",
        Some(b) if b <= 16000 => "medium",
        Some(_) => "max",
    }
}

const SECRET_ENV_VARS: [&str; 3] = [
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
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

/// Provider phrasings for "this request did not fit in the context window".
/// Every provider reports it as a plain 400, indistinguishable by status from a
/// malformed request, so the message text is the only signal available.
const CONTEXT_OVERFLOW_MARKERS: [&str; 6] = [
    // Anthropic: "prompt is too long: 213004 tokens > 200000 maximum"
    "prompt is too long",
    // Anthropic, when the reply budget is what pushes it over.
    "exceed context limit",
    // OpenAI error code, echoed verbatim in the body.
    "context_length_exceeded",
    // OpenAI prose: "This model's maximum context length is 128000 tokens".
    "maximum context length",
    // OpenAI-compatible endpoints (Groq, z.ai, Ollama) phrase it either way.
    "context window",
    "too many tokens",
];

/// True when a provider error means the request was too large for the model's
/// context, rather than being malformed in some other way.
///
/// Worth classifying because it is the one 400 that is *recoverable*: the
/// request is fine, there is simply too much history behind it. Compacting and
/// resending saves the session, where the generic terminal path would end the
/// run and make the user restate the whole task.
pub fn is_context_overflow(error: &str) -> bool {
    let lower = error.to_lowercase();
    CONTEXT_OVERFLOW_MARKERS.iter().any(|m| lower.contains(m))
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
    fn context_overflow_recognizes_every_provider_phrasing() {
        for msg in [
            "invalid_request_error: prompt is too long: 213004 tokens > 200000 maximum",
            "input length and `max_tokens` exceed context limit: 199000 + 8000 > 200000",
            "400 Bad Request: {\"error\":{\"code\":\"context_length_exceeded\"}}",
            "This model's maximum context length is 128000 tokens, however you requested 131000",
            "requested tokens exceed the context window of this model",
            "Too many tokens in the request",
        ] {
            assert!(is_context_overflow(msg), "not classified: {msg}");
        }
    }

    #[test]
    fn context_overflow_ignores_other_client_errors() {
        for msg in [
            "401 Unauthorized: invalid api key",
            "400 Bad Request: messages: final message must be a user turn",
            "tool `read` input failed schema validation",
            "connection refused",
        ] {
            assert!(!is_context_overflow(msg), "misclassified: {msg}");
        }
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(backoff_secs(1), 1);
        assert_eq!(backoff_secs(3), 4);
        assert!(backoff_secs(20) <= 30);
    }

    #[test]
    fn glm_detected_by_model_or_host() {
        for (base, model) in [
            ("https://api.z.ai/api/anthropic", "glm-5.2"),
            ("https://api.z.ai/api/coding/paas/v4", "glm-5.3"),
            ("https://openrouter.ai/api/v1", "z-ai/glm-5.2"),
            ("https://api.z.ai/api/anthropic", "claude-opus-4-7"),
            ("https://proxy.bigmodel.cn/x", "whatever"),
        ] {
            assert!(is_glm(base, model), "not detected: {base} / {model}");
        }
        for (base, model) in [
            ("https://api.anthropic.com", "claude-opus-4-7"),
            ("https://api.openai.com/v1", "gpt-4o-mini"),
            ("https://my-glm-proxy.example.com", "llama-3.3-70b"),
        ] {
            assert!(!is_glm(base, model), "false positive: {base} / {model}");
        }
    }

    #[test]
    fn budget_dial_maps_to_effort_labels() {
        assert_eq!(glm_effort_label(None), "light");
        assert_eq!(glm_effort_label(Some(8000)), "low");
        assert_eq!(glm_effort_label(Some(16000)), "medium");
        assert_eq!(glm_effort_label(Some(32000)), "max");
    }
}
