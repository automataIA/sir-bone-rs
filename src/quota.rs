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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Window {
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
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
}
