//! Mid-stream rules: project constraints that cost nothing until they fire.
//!
//! A rule that lives in the system prompt is billed as input tokens on every
//! single turn, whether or not the model was about to break it. A stream rule
//! is dormant instead: its regex watches the text as it is generated, and the
//! first match aborts the stream, feeds the rule back as a system reminder, and
//! restarts the turn. The model reads the rule exactly when it is about to
//! violate it.
//!
//! Config (`stream_rules`, layered global + per-project like every other
//! section):
//!
//! ```json
//! {"stream_rules": [
//!   {"name": "box-leak", "pattern": "Box::leak",
//!    "message": "Never Box::leak in production paths; use Arc<str>."}
//! ]}
//! ```
//!
//! No config = no rules = the streaming path is byte-for-byte what it was.
//! Ablatable with `SIRBONE_DISABLE=stream:rules`.

use regex::Regex;

/// How many trailing bytes of the generated text each rule is matched against.
/// A pattern spanning more than this will not fire — the alternative is
/// re-scanning the whole response on every delta.
pub const WINDOW_BYTES: usize = 4096;

/// How many times one turn may be aborted and restarted before it is allowed to
/// run to completion. Each rule additionally fires at most once per turn, so
/// the loop always terminates.
pub const MAX_TRIPS: usize = 2;

#[derive(Debug)]
pub struct StreamRule {
    pub name: String,
    pub pattern: Regex,
    /// Text injected as a system reminder when the rule fires.
    pub message: String,
}

#[derive(Debug, Default)]
pub struct StreamRules(Vec<StreamRule>);

impl StreamRules {
    /// Load from the `stream_rules` config section. Malformed entries (bad
    /// regex, missing field) are dropped with a warning, never fatal — same
    /// policy as [`crate::checks::PostEditChecks::load`].
    pub fn load() -> Self {
        if crate::ablate::stream_rules_disabled() {
            return Self::default();
        }
        Self::from_value(crate::config::section("stream_rules").as_ref())
    }

    pub(crate) fn from_value(v: Option<&serde_json::Value>) -> Self {
        let rules = v
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let name = e.get("name")?.as_str()?.to_string();
                        let message = e.get("message")?.as_str()?.to_string();
                        let raw = e.get("pattern")?.as_str()?;
                        match Regex::new(raw) {
                            Ok(pattern) => Some(StreamRule {
                                name,
                                pattern,
                                message,
                            }),
                            Err(e) => {
                                tracing::warn!("stream rule `{name}` has an invalid pattern: {e}");
                                None
                            }
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self(rules)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The first rule matching `text`, skipping any in `spent`. Callers pass the
    /// tail window of what the model has generated so far.
    pub fn trip<'a>(&'a self, text: &str, spent: &[String]) -> Option<&'a StreamRule> {
        self.0
            .iter()
            .find(|r| !spent.contains(&r.name) && r.pattern.is_match(text))
    }

    /// A copy without the named rules. The agent installs one of these for the
    /// rest of a turn once a rule has fired, so a reminder the model ignored
    /// cannot abort the same turn twice.
    pub fn without(&self, names: &[String]) -> Self {
        Self(
            self.0
                .iter()
                .filter(|r| !names.contains(&r.name))
                .map(|r| StreamRule {
                    name: r.name.clone(),
                    pattern: r.pattern.clone(),
                    message: r.message.clone(),
                })
                .collect(),
        )
    }

    /// Look a rule up by name, to recover its message after a client reports a trip.
    pub fn message(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.message.as_str())
    }
}

/// Load the configured rules and arm the client with them. Returns the canonical
/// set for [`crate::AgentContext::stream_rules`]; no config = an empty set and a
/// client whose streaming path is untouched.
pub fn install(client: &dyn crate::LlmClient) -> std::sync::Arc<StreamRules> {
    let rules = std::sync::Arc::new(StreamRules::load());
    if !rules.is_empty() {
        client.set_stream_rules(rules.clone());
    }
    rules
}

/// The last [`WINDOW_BYTES`] of `text`, cut on a char boundary.
pub fn window(text: &str) -> &str {
    if text.len() <= WINDOW_BYTES {
        return text;
    }
    let mut cut = text.len() - WINDOW_BYTES;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    &text[cut..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(json: &str) -> StreamRules {
        let v: serde_json::Value = serde_json::from_str(json).expect("test json");
        StreamRules::from_value(Some(&v))
    }

    #[test]
    fn no_config_means_no_rules() {
        assert!(StreamRules::from_value(None).is_empty());
        assert!(rules("[]").is_empty());
    }

    #[test]
    fn invalid_entries_are_dropped_not_fatal() {
        let r = rules(
            r#"[{"name":"bad","pattern":"([","message":"m"},
                {"name":"partial","pattern":"x"},
                {"name":"ok","pattern":"Box::leak","message":"no leaks"}]"#,
        );
        assert!(r.trip("please Box::leak this", &[]).is_some());
        assert_eq!(r.message("ok"), Some("no leaks"));
        assert!(r.message("bad").is_none());
        assert!(r.message("partial").is_none());
    }

    #[test]
    fn a_spent_rule_does_not_fire_again() {
        let r = rules(r#"[{"name":"ok","pattern":"Box::leak","message":"m"}]"#);
        let text = "Box::leak(x)";
        assert_eq!(r.trip(text, &[]).map(|r| r.name.as_str()), Some("ok"));
        assert!(r.trip(text, &["ok".to_string()]).is_none());
        assert!(r.trip("nothing to see", &[]).is_none());
    }

    #[test]
    fn window_keeps_the_tail_on_a_char_boundary() {
        let long = "à".repeat(WINDOW_BYTES);
        let w = window(&long);
        assert!(w.len() <= WINDOW_BYTES);
        assert!(long.ends_with(w));
        assert_eq!(window("short"), "short");
    }
}
