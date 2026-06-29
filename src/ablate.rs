//! Feature-ablation toggles via `SIRBONE_DISABLE` (feature-audit harness).
//!
//! Format: comma list of `kind:name`, e.g. `"tool:web_search,skill:tdd,cache:prompt"`.
//! `name` may be `*` to disable every entry of that kind (`skill:*` hides all skills).
//! Unset/empty => everything enabled (normal behavior — zero effect on shipped runs).
//!
//! Three cut points read these: `make_tools`/`read_only_registry` (drop tools),
//! `scan_skills` (hide skills from catalog + `load_skill`), the Anthropic client
//! (skip prompt-cache `cache_control`). One module so the parse rule lives once.

/// True if `spec` lists `kind:name` (or `kind:*`). Pure — the testable core.
fn spec_has(spec: &str, kind: &str, name: &str) -> bool {
    spec.split(',')
        .filter_map(|e| e.trim().split_once(':'))
        .any(|(k, n)| {
            k.trim() == kind && {
                let n = n.trim();
                n == name || n == "*"
            }
        })
}

fn env_has(kind: &str, name: &str) -> bool {
    std::env::var("SIRBONE_DISABLE").is_ok_and(|v| spec_has(&v, kind, name))
}

pub fn disabled_tool(name: &str) -> bool {
    env_has("tool", name)
}

pub fn disabled_skill(name: &str) -> bool {
    env_has("skill", name)
}

pub fn cache_disabled() -> bool {
    env_has("cache", "prompt")
}

#[cfg(test)]
mod tests {
    use super::spec_has;

    #[test]
    fn matches_kind_and_name() {
        let s = "tool:web_search, skill:tdd ,cache:prompt";
        assert!(spec_has(s, "tool", "web_search"));
        assert!(spec_has(s, "skill", "tdd"));
        assert!(spec_has(s, "cache", "prompt"));
    }

    #[test]
    fn no_false_positives() {
        let s = "tool:web_search";
        assert!(!spec_has(s, "tool", "code_map"));
        assert!(!spec_has(s, "skill", "web_search")); // wrong kind
        assert!(!spec_has("", "tool", "web_search"));
    }

    #[test]
    fn wildcard_matches_any_name_of_kind() {
        assert!(spec_has("skill:*", "skill", "tdd"));
        assert!(spec_has("skill:*", "skill", "diagnose"));
        assert!(!spec_has("skill:*", "tool", "code_map")); // wildcard is kind-scoped
    }
}
