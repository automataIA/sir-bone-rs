//! Account-wide 5-hour quota-window estimate, persisted under `~/.sirbone` so it
//! survives restarts and is shared across every project. Models the provider's
//! reset (Claude/z.ai subscriptions): the window opens on the first prompt and a
//! fresh one opens on the first prompt sent *after* the previous window's 5
//! hours have elapsed. The on-disk counter — not the session files — is the
//! source of truth, so a single long-lived process that spans the gap still
//! rolls correctly.

use std::path::PathBuf;

use chrono::{DateTime, Duration, Local};
use serde::{Deserialize, Serialize};

const WINDOW_HOURS: i64 = 5;
/// z.ai's own quota endpoint, the one the ZCode client polls for the same figure.
const GLM_QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Window {
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    /// Share of the window's prompt pool already consumed, when the provider
    /// reports it (GLM on z.ai). None everywhere else: the window is then just
    /// the local time estimate.
    #[serde(default)]
    pub used_pct: Option<u8>,
}

/// `~/.sirbone/quota_window.json`, if HOME is set.
fn path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".sirbone").join("quota_window.json"))
}

fn load() -> Option<Window> {
    serde_json::from_str(&std::fs::read_to_string(path()?).ok()?).ok()
}

fn save(w: &Window) {
    let (Some(p), Ok(s)) = (path(), serde_json::to_string(w)) else {
        return;
    };
    let _ = std::fs::write(p, s);
}

/// Pure roll: keep `prev` while it's still open at `now`, else open a fresh
/// 5-hour window starting at `now`.
fn roll(prev: Option<Window>, now: DateTime<Local>) -> Window {
    match prev {
        Some(w) if now < w.end => w,
        _ => Window {
            start: now,
            end: now + Duration::hours(WINDOW_HOURS),
            used_pct: None,
        },
    }
}

/// Register a prompt send: keep the active window, or open a new 5-hour one when
/// none is active (first ever send, or the first after the prior window lapsed).
/// Persists and returns the active window.
pub fn touch() -> Window {
    let w = roll(load(), Local::now());
    save(&w);
    w
}

/// Replace the estimate with the provider's own figures while the active model
/// is a GLM served by z.ai, which exposes the real 5-hour pool. Best-effort: a
/// missing key, a non-z.ai base URL, an HTTP failure or an unexpected shape all
/// leave the local estimate untouched.
pub async fn refresh_glm(model: &str) {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let key = match var("ANTHROPIC_AUTH_TOKEN") {
        k if !k.is_empty() => k,
        _ => var("OPENAI_API_KEY"),
    };
    if key.is_empty()
        || !model.to_ascii_lowercase().starts_with("glm")
        || !(var("ANTHROPIC_BASE_URL").contains("z.ai") || var("OPENAI_BASE_URL").contains("z.ai"))
    {
        return;
    }
    let Ok(resp) = reqwest::Client::new()
        .get(GLM_QUOTA_URL)
        .bearer_auth(key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    else {
        return;
    };
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return;
    };
    let Some((used_pct, reset)) = five_hour_limit(&body) else {
        return;
    };
    // The API reset is authoritative when present; it is omitted while the pool
    // is untouched, so fall back to the window already open (and to nothing at
    // all when the account has not opened one yet).
    let end = match (reset, load().filter(|w| Local::now() < w.end)) {
        (Some(r), _) => r,
        (None, Some(w)) => w.end,
        (None, None) => return,
    };
    save(&Window {
        start: end - Duration::hours(WINDOW_HOURS),
        end,
        used_pct: Some(used_pct),
    });
}

/// `(used_pct, reset)` of the z.ai 5-hour prompt pool — the `TOKENS_LIMIT`
/// entry with `unit: 3, number: 5`. `percentage` is the share *consumed*, and
/// `nextResetTime` (epoch ms) is absent while the pool is still untouched.
fn five_hour_limit(body: &serde_json::Value) -> Option<(u8, Option<DateTime<Local>>)> {
    let limit = body["data"]["limits"]
        .as_array()?
        .iter()
        .find(|l| l["type"] == "TOKENS_LIMIT" && l["unit"] == 3 && l["number"] == 5)?;
    let used = limit["percentage"].as_f64()?.clamp(0.0, 100.0) as u8;
    let reset = limit["nextResetTime"]
        .as_i64()
        .and_then(DateTime::from_timestamp_millis)
        .map(|t| t.with_timezone(&Local));
    Some((used, reset))
}

/// The active window, or None when none is open (never used, or already lapsed).
pub fn current() -> Option<Window> {
    load().filter(|w| Local::now() < w.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_keeps_and_rolls() {
        let t0 = Local::now();
        // No prior window → opens one 5h long.
        let w = roll(None, t0);
        assert_eq!(w.start, t0);
        assert_eq!(w.end, t0 + Duration::hours(5));
        // A send inside the window keeps it unchanged.
        let mid = roll(Some(w), t0 + Duration::hours(4));
        assert_eq!(mid.start, w.start);
        // The first send after it lapses opens a fresh window from that instant.
        let later = t0 + Duration::hours(5) + Duration::minutes(31);
        let next = roll(Some(w), later);
        assert_eq!(next.start, later);
        assert_eq!(next.end, later + Duration::hours(5));
    }

    #[test]
    fn reads_the_glm_five_hour_pool() {
        // Live shape from api.z.ai: the tool limit comes first and carries a
        // reset, the 5-hour pool is TOKENS_LIMIT unit 3 / number 5.
        let body = serde_json::json!({"code":200,"data":{"level":"lite","limits":[
            {"type":"TIME_LIMIT","unit":5,"number":1,"percentage":15,"nextResetTime":1787377211997i64},
            {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":0}]}});
        assert_eq!(five_hour_limit(&body), Some((0, None)));
        // With a reset, it is parsed into local time.
        let body = serde_json::json!({"data":{"limits":[
            {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":42,"nextResetTime":1787377211997i64}]}});
        let (used, reset) = five_hour_limit(&body).unwrap();
        assert_eq!(used, 42);
        assert_eq!(reset.unwrap().timestamp_millis(), 1787377211997);
        // No 5-hour entry at all → nothing to show.
        assert!(five_hour_limit(&serde_json::json!({"data":{"limits":[]}})).is_none());
    }
}
