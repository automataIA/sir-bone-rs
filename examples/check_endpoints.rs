//! Endpoint diagnostic: probes what the configured providers (.env) actually
//! support — model listing, token counting, per-model info, and the z.ai/GLM
//! "thinking" format. No-ops for any provider whose key is unset, so it is safe
//! to run with partial credentials. Not run by `cargo test` (examples aren't),
//! so it never spends quota in CI.
//!
//! Run: `cargo run --example check_endpoints`
use serde_json::json;
use sirbone::{AnthropicClient, LlmClient, Message, OpenAiClient, ToolRegistry};

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let http = reqwest::Client::new();
    let model = std::env::var("SIRBONE_MODEL").unwrap_or_else(|_| "glm-4.6".into());
    let registry = ToolRegistry::new();
    let convo = [Message::user("hello")];

    // ---- Anthropic-style ----
    if let Ok(key) = std::env::var("ANTHROPIC_AUTH_TOKEN") {
        let base = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());
        let base = base.trim_end_matches('/').to_string();
        println!("\n=== Anthropic-style @ {base} ===");

        let client = AnthropicClient::new(&base, &key, &model);
        match client.list_models().await {
            Ok(m) => println!("  list_models: {} — {:?}", m.len(), &m[..m.len().min(8)]),
            Err(e) => println!("  list_models: ERROR {e}"),
        }
        match client
            .count_tokens(&convo.iter().collect::<Vec<_>>(), &registry)
            .await
        {
            Ok(n) => println!("  count_tokens(\"hello\"): {n}"),
            Err(e) => println!("  count_tokens: ERROR {e}"),
        }
        report(
            http.get(format!("{base}/v1/models/{model}"))
                .header("x-api-key", &key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await,
            "GET models/{id}",
        )
        .await;

        // Does this endpoint honor Anthropic's budget_tokens thinking form?
        report(
            http.post(format!("{base}/v1/messages"))
                .header("x-api-key", &key)
                .header("anthropic-version", "2023-06-01")
                .json(&json!({
                    "model": model, "max_tokens": 64,
                    "messages": [{"role": "user", "content": "2+2?"}],
                    "thinking": {"type": "enabled", "budget_tokens": 1024}
                }))
                .send()
                .await,
            "thinking budget_tokens",
        )
        .await;
    }

    // ---- OpenAI-style ----
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        let base =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let base = base.trim_end_matches('/').to_string();
        println!("\n=== OpenAI-style @ {base} ===");

        match OpenAiClient::new(&base, &key, &model).list_models().await {
            Ok(m) => println!("  list_models: {} — {:?}", m.len(), &m[..m.len().min(8)]),
            Err(e) => println!("  list_models: ERROR {e}"),
        }
        report(
            http.get(format!("{base}/models/{model}"))
                .bearer_auth(&key)
                .send()
                .await,
            "GET models/{id}",
        )
        .await;
    }
}

async fn report(r: reqwest::Result<reqwest::Response>, name: &str) {
    match r {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(140).collect();
            println!("  {name}: HTTP {status} — {snippet}");
        }
        Err(e) => println!("  {name}: transport error: {e}"),
    }
}
