use std::net::IpAddr;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;
use url::{Host, Url};

use super::{truncate::DEFAULT_MAX_BYTES, truncate_output, TypedTool};

/// Addresses an agent-driven fetch must never reach: loopback, RFC1918,
/// link-local (incl. 169.254.169.254 cloud metadata), unspecified, multicast.
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_private_ip(IpAddr::V4(v4)),
            None => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local()
            }
        },
    }
}

/// Validate scheme and host; for domain names resolve via DNS and reject if any
/// address is private. Returns a curl `--resolve host:port:ip` pin for vetted
/// domains so curl cannot be rebound to a different address afterwards.
/// `SIRBONE_WEB_FETCH_ALLOW_PRIVATE=1` skips the address checks (local dev servers).
async fn vet_url(raw: &str) -> Result<Option<String>> {
    let parsed = Url::parse(raw).with_context(|| format!("invalid URL: {raw}"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        bail!("scheme '{scheme}' not allowed (http/https only)");
    }
    if std::env::var("SIRBONE_WEB_FETCH_ALLOW_PRIVATE").is_ok_and(|v| v == "1") {
        return Ok(None);
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    match parsed.host().context("URL has no host")? {
        Host::Ipv4(ip) if is_private_ip(IpAddr::V4(ip)) => {
            bail!("refusing to fetch private/internal address {ip}")
        }
        Host::Ipv6(ip) if is_private_ip(IpAddr::V6(ip)) => {
            bail!("refusing to fetch private/internal address {ip}")
        }
        Host::Ipv4(_) | Host::Ipv6(_) => Ok(None),
        Host::Domain(domain) => {
            let addrs: Vec<_> = tokio::net::lookup_host((domain, port))
                .await
                .with_context(|| format!("DNS lookup failed for {domain}"))?
                .collect();
            let first = addrs.first().context("DNS returned no addresses")?.ip();
            if let Some(bad) = addrs.iter().find(|a| is_private_ip(a.ip())) {
                bail!(
                    "refusing to fetch {domain}: resolves to private/internal address {}",
                    bad.ip()
                );
            }
            // Pin the vetted IP so a second resolution inside curl can't be rebound.
            Ok(Some(format!("{domain}:{port}:{first}")))
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct WebFetchInput {
    /// URL to fetch
    pub url: String,
    /// Timeout in seconds (default: 15)
    #[serde(default = "default_timeout")]
    pub timeout: u32,
}

fn default_timeout() -> u32 {
    15
}

pub struct WebFetchTool;

#[async_trait]
impl TypedTool for WebFetchTool {
    type Input = WebFetchInput;

    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch the text content of a URL via curl. HTML pages are converted to markdown; other bodies are returned raw. Truncated if large."
    }

    async fn run(&self, input: WebFetchInput) -> Result<String> {
        let body = curl_fetch(&input.url, input.timeout).await?;
        if body.is_empty() {
            return Ok("(empty response)".into());
        }
        Ok(truncate_output(extract_text(body), 500, DEFAULT_MAX_BYTES))
    }
}

/// Convert HTML bodies to markdown so markup/script boilerplate never reaches
/// the context (headroom `html_extractor` pattern, ported natively). Non-HTML
/// bodies and failed/empty conversions pass through untouched.
fn extract_text(body: String) -> String {
    if !looks_like_html(&body) {
        return body;
    }
    match htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript"])
        .build()
        .convert(&body)
    {
        Ok(md) if !md.trim().is_empty() => md,
        _ => body,
    }
}

/// Sniff an HTML document without a Content-Type header: doctype or an <html>
/// tag within the first KiB. JSON bodies (leading `{`/`[`) never match, even
/// when a string value embeds markup.
fn looks_like_html(body: &str) -> bool {
    let trimmed = body.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return false;
    }
    let head: String = trimmed
        .chars()
        .take(1024)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("<!doctype") || head.contains("<html")
}

/// Fetch a URL's full body via SSRF-vetted curl. Redirects are followed
/// **manually** (curl is invoked without `-L`): each `Location:` is re-vetted by
/// `vet_url` before the next hop, so a redirect to `169.254.169.254` or any
/// private/internal host is blocked instead of silently followed. Each hop is
/// also DNS-pinned (`--resolve`) so a low-TTL rebinding server can't flip the
/// address between sirbone's check and curl's resolution.
/// Returns the untruncated body so callers can store or post-process it; shared
/// by `web_fetch` and `fetch_docs`.
pub(crate) async fn curl_fetch(url: &str, timeout: u32) -> Result<String> {
    const MAX_REDIRECTS: usize = 5;
    let mut current = url.to_string();
    for _ in 0..MAX_REDIRECTS {
        let resolve_pin = vet_url(&current).await?;
        let timeout_s = timeout.to_string();
        let marker = "\n__SIRBONE_REDIR__";
        let wfmt = format!("{marker}%{{redirect_url}}");
        let mut args = vec![
            "-s",
            "--proto",
            "=http,https",
            "--max-time",
            &timeout_s,
            "-w",
            &wfmt,
        ];
        if let Some(pin) = &resolve_pin {
            args.extend(["--resolve", pin]);
        }
        args.push(&current);
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
        let raw = String::from_utf8_lossy(&out.stdout);
        match raw.rfind(marker) {
            None => return Ok(raw.into_owned()),
            Some(idx) => {
                let body = raw[..idx].to_string();
                let redir = raw[idx + marker.len()..].trim();
                if redir.is_empty() {
                    return Ok(body);
                }
                // Re-vet on the next iteration before fetching.
                current = Url::parse(&current)?.join(redir)?.to_string();
            }
        }
    }
    bail!("too many redirects (>{MAX_REDIRECTS}) fetching {url}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeout_is_15s() {
        assert_eq!(default_timeout(), 15);
    }

    #[test]
    fn html_body_converted_to_markdown_script_dropped() {
        let html = "<!DOCTYPE html><html><head><script>var x=1;</script>\
                    <style>body{color:red}</style></head>\
                    <body><h1>Title</h1><p>Hello <a href=\"/x\">link</a></p></body></html>";
        let md = extract_text(html.into());
        assert!(md.contains("# Title"), "got: {md}");
        assert!(md.contains("Hello"));
        assert!(!md.contains("var x=1"));
        assert!(!md.contains("color:red"));
    }

    #[test]
    fn json_and_plain_text_pass_through() {
        let json = r#"{"items": ["<html> looks html but is a string value"]}"#;
        assert_eq!(extract_text(json.into()), json);
        let text = "plain text, no markup";
        assert_eq!(extract_text(text.into()), text);
    }

    #[test]
    fn html_sniff_ignores_late_html_tag() {
        // "<html" only matters within the first KiB.
        let body = format!("{}<html>", "x".repeat(2000));
        assert_eq!(extract_text(body.clone()), body);
    }

    #[tokio::test]
    async fn unfetchable_url_errors_without_panicking() {
        // A scheme curl cannot resolve → non-zero exit → `bail`, not a panic.
        let res = WebFetchTool
            .run(WebFetchInput {
                url: "http://0.0.0.0:1/".into(),
                timeout: 1,
            })
            .await;
        assert!(res.is_err());
    }

    #[test]
    fn private_ips_detected() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "172.16.5.5",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.1.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12:3456:789a::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip} should be private");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[tokio::test]
    async fn vet_rejects_private_ip_literals_and_bad_schemes() {
        assert!(vet_url("http://127.0.0.1:8080/").await.is_err());
        assert!(vet_url("http://169.254.169.254/latest/meta-data/")
            .await
            .is_err());
        assert!(vet_url("http://[::1]/").await.is_err());
        assert!(vet_url("file:///etc/passwd").await.is_err());
        assert!(vet_url("gopher://example.com/").await.is_err());
        assert!(vet_url("not a url").await.is_err());
    }

    #[tokio::test]
    async fn vet_allows_public_ip_literal_without_pin() {
        // IP literal needs no DNS pin; no network I/O involved.
        assert_eq!(vet_url("https://1.1.1.1/").await.unwrap(), None);
    }
}
