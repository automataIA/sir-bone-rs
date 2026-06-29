use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

use super::{truncate::DEFAULT_MAX_BYTES, truncate_output, TypedTool};

/// Realistic UA — DDG's HTML endpoint blocks requests without a browser UA.
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
/// Min spacing between DDG calls (~30/min) to stay under its rate limit.
const DDG_MIN_INTERVAL: Duration = Duration::from_millis(2100);
const DDG_URL: &str = "https://html.duckduckgo.com/html/";

#[derive(Deserialize, JsonSchema)]
pub struct WebSearchInput {
    /// Search query.
    pub query: String,
    /// Max results to return (default 8).
    #[serde(default = "default_max")]
    pub max_results: usize,
    /// Optional recency filter: "day", "week", "month", or "year".
    #[serde(default)]
    pub time_range: Option<String>,
}

fn default_max() -> usize {
    8
}

/// Web search. Prefers a JSON metasearch endpoint (SearXNG/Websurfx) when
/// `SIRBONE_SEARXNG_URL` is set — no rate limit, richer results — and otherwise
/// falls back to scraping DuckDuckGo's keyless HTML endpoint.
#[derive(Default)]
pub struct WebSearchTool {
    /// Reserves the next DDG slot so calls self-throttle (also across clones).
    ddg_gate: Arc<Mutex<Option<Instant>>>,
}

#[async_trait]
impl TypedTool for WebSearchTool {
    type Input = WebSearchInput;

    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the web and return ranked results (title, url, snippet). Use it to \
         find docs, APIs, error messages, or solutions, then `web_fetch` a result URL \
         for the full page. Set SIRBONE_SEARXNG_URL for a self-hosted JSON backend \
         (no rate limit); otherwise it uses DuckDuckGo (keyless)."
    }

    async fn run(&self, input: WebSearchInput) -> Result<String> {
        let max = input.max_results.clamp(1, 20);
        let results = match std::env::var("SIRBONE_SEARXNG_URL") {
            Ok(base) if !base.trim().is_empty() => {
                searxng_search(base.trim_end_matches('/'), &input, max).await?
            }
            _ => self.ddg_search(&input, max).await?,
        };
        if results.is_empty() {
            return Ok("(no results — the backend may be rate-limited; set \
                       SIRBONE_SEARXNG_URL for a self-hosted backend)"
                .into());
        }
        let body: String = results
            .iter()
            .enumerate()
            .map(|(i, (t, u, s))| {
                let snip = if s.is_empty() {
                    String::new()
                } else {
                    format!("\n   {s}")
                };
                format!("{}. {t}\n   {u}{snip}\n", i + 1)
            })
            .collect();
        Ok(truncate_output(body, 200, DEFAULT_MAX_BYTES))
    }
}

impl WebSearchTool {
    /// DDG HTML scrape with self-throttling. Returns (title, url, snippet).
    async fn ddg_search(
        &self,
        input: &WebSearchInput,
        max: usize,
    ) -> Result<Vec<(String, String, String)>> {
        // Reserve the next slot, then sleep the remaining gap (guard not held
        // across the await — std Mutex).
        let wait = {
            let mut g = crate::types::lock_or_recover(&self.ddg_gate);
            let wait = g
                .map(|next| next.saturating_duration_since(Instant::now()))
                .unwrap_or_default();
            *g = Some(Instant::now() + wait + DDG_MIN_INTERVAL);
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }

        // Browser-like headers are required: without Accept/Accept-Language/Referer
        // DDG's HTML endpoint returns an HTTP 202 "anomaly" challenge page.
        let mut args: Vec<String> = vec![
            "-s".into(),
            "-A".into(),
            UA.into(),
            "-H".into(),
            "Accept: text/html,application/xhtml+xml".into(),
            "-H".into(),
            "Accept-Language: en-US,en;q=0.9".into(),
            "-H".into(),
            "Referer: https://duckduckgo.com/".into(),
            "--max-time".into(),
            "15".into(),
            "--data-urlencode".into(),
            format!("q={}", input.query),
            "--data".into(),
            "kl=us-en".into(),
        ];
        if let Some(tr) = input.time_range.as_deref().and_then(map_time_range) {
            args.push("--data".into());
            args.push(format!("df={tr}"));
        }
        args.push(DDG_URL.into());

        let out = Command::new("curl")
            .args(&args)
            .output()
            .await
            .context("curl not found")?;
        if !out.status.success() {
            bail!(
                "curl error: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(parse_ddg(&String::from_utf8_lossy(&out.stdout), max))
    }
}

/// SearXNG / Websurfx JSON API: `GET {base}/search?q=..&format=json`.
async fn searxng_search(
    base: &str,
    input: &WebSearchInput,
    max: usize,
) -> Result<Vec<(String, String, String)>> {
    let mut args: Vec<String> = vec![
        "-s".into(),
        "--max-time".into(),
        "15".into(),
        "--get".into(),
        "--data-urlencode".into(),
        format!("q={}", input.query),
        "--data".into(),
        "format=json".into(),
        "--data".into(),
        "categories=general".into(),
    ];
    if let Some(tr) = input.time_range.as_deref().and_then(map_time_range) {
        args.push("--data".into());
        args.push(format!("time_range={tr}"));
    }
    args.push(format!("{base}/search"));

    let out = Command::new("curl")
        .args(&args)
        .output()
        .await
        .context("curl not found")?;
    if !out.status.success() {
        bail!(
            "curl error: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&body)
        .context("SearXNG did not return JSON (is `format: json` enabled in settings.yml?)")?;
    let results = v["results"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    Ok(results
        .iter()
        .take(max)
        .filter_map(|r| {
            let url = r["url"].as_str()?.to_string();
            let title = r["title"].as_str().unwrap_or("").to_string();
            let snippet = r["content"].as_str().unwrap_or("").to_string();
            Some((title, url, snippet))
        })
        .collect())
}

/// Normalize a recency filter to the single-letter code both backends accept.
fn map_time_range(s: &str) -> Option<&'static str> {
    match s.to_lowercase().as_str() {
        "day" | "d" => Some("d"),
        "week" | "w" => Some("w"),
        "month" | "m" => Some("m"),
        "year" | "y" => Some("y"),
        _ => None,
    }
}

/// Extract (title, url, snippet) triples from DDG's HTML result page.
fn parse_ddg(html: &str, max: usize) -> Vec<(String, String, String)> {
    let snippets: Vec<String> = html
        .split("class=\"result__snippet\"")
        .skip(1)
        .filter_map(inner_text)
        .collect();

    let mut out = Vec::new();
    for (i, seg) in html.split("class=\"result__a\"").skip(1).enumerate() {
        let Some(href) = attr_value(seg, "href=\"") else {
            continue;
        };
        if href.contains("/y.js") {
            continue; // sponsored result
        }
        let url = match href.find("uddg=") {
            Some(p) => percent_decode(href[p + 5..].split('&').next().unwrap_or("")),
            None => href.trim_start_matches("//").to_string(),
        };
        let title = inner_text(seg).unwrap_or_default();
        if title.is_empty() || url.is_empty() {
            continue;
        }
        out.push((title, url, snippets.get(i).cloned().unwrap_or_default()));
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Value of an `attr="..."` occurring in `seg` (e.g. `href="..."`).
fn attr_value(seg: &str, attr: &str) -> Option<String> {
    let start = seg.find(attr)? + attr.len();
    let rest = &seg[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Text between the first `>` and the next `</a>`, tags/entities cleaned.
fn inner_text(seg: &str) -> Option<String> {
    let open = seg.find('>')? + 1;
    let rest = &seg[open..];
    let end = rest.find("</a>")?;
    Some(clean(&rest[..end]))
}

/// Strip HTML tags, decode a few common entities, collapse whitespace.
fn clean(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out = out
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Minimal percent-decoder for the `uddg` redirect parameter (`%XX`, `+`).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_uddg() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fdoc.rs%2Fa+b"),
            "https://doc.rs/a b"
        );
    }

    #[test]
    fn cleans_html() {
        assert_eq!(clean("<b>Rust</b> &amp; async"), "Rust & async");
    }

    #[test]
    fn parses_ddg_result_and_skips_ads() {
        let html = r#"
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&rut=x">The <b>Rust</b> Lang</a>
            <a class="result__snippet" href="x">A systems language.</a>
            <a class="result__a" href="//duckduckgo.com/y.js?ad=1">Sponsored</a>
        "#;
        let r = parse_ddg(html, 8);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "The Rust Lang");
        assert_eq!(r[0].1, "https://rust-lang.org/");
        assert_eq!(r[0].2, "A systems language.");
    }

    #[test]
    fn maps_time_range() {
        assert_eq!(map_time_range("week"), Some("w"));
        assert_eq!(map_time_range("nonsense"), None);
    }
}
