//! Best-of-K attempts, selected by execution (`SIRBONE_BEST_OF`, default off).
//!
//! Sampling several candidates raises resolution rate; choosing between them is
//! where the published gains stop ("SWE-World", arXiv 2602.03419: 55.0 at K=1 →
//! 68.2 at TTS@8, with the ranking claimed as the contribution, not the
//! sampling). This repository measured the same wall from the other side: the
//! oracle's Tier-1 differential moved nothing, and the edit-capable self-review
//! variant of `verify` flipped zero cases. A generator does not judge itself — the
//! Best@K/Pass@K gap under a generative verifier is that same fact.
//!
//! So the selector here is not a model. Each attempt starts from the same
//! work-tree, and the project's own test command scores the tree it left behind;
//! the tree with the fewest failing tests wins. No judgement, no prose, nothing
//! the generator can talk its way past.
//!
//! **Cost, stated up front:** every form of test-time scaling improves `pass^k`
//! while spending more calls and tokens. The honest gate for this feature is
//! cost *per stably-resolved task*, never resolve rate — see `MISSION.md`.
//! Attempt 1 passing short-circuits, so the extra cost is paid only on the runs
//! that were failing anyway.

use crate::oracle::OracleResult;

/// Upper bound on K. Each attempt is a full agent run, so a mistyped
/// `SIRBONE_BEST_OF=20` would spend twenty runs' worth of quota before anyone
/// looked at the log.
const MAX_K: usize = 4;

pub struct BestOf {
    /// Attempts to run at most, `2..=MAX_K`.
    pub k: usize,
    test_command: String,
    min_tests: usize,
}

impl BestOf {
    /// `SIRBONE_BEST_OF=K` together with a configured `oracle.test_command`.
    ///
    /// `Ok(None)` = off. `Err(reason)` = the flag is set but the feature cannot
    /// run; the caller prints it and continues with a single attempt. Failing
    /// silently would let a mistyped bench arm look like a measured null result.
    pub fn load() -> Result<Option<Self>, String> {
        let Some(raw) = std::env::var_os("SIRBONE_BEST_OF") else {
            return Ok(None);
        };
        let Some(k) = parse_k(&raw.to_string_lossy())? else {
            return Ok(None);
        };
        let test_command = crate::oracle::load_test_command().ok_or(
            "SIRBONE_BEST_OF needs `oracle.test_command` in the config: the attempts are \
             selected by running it, and with nothing to run there is no selector but the \
             model's own opinion",
        )?;
        Ok(Some(Self {
            k,
            test_command,
            min_tests: crate::oracle::load_min_tests(),
        }))
    }

    pub fn command(&self) -> &str {
        &self.test_command
    }

    /// Score the current work-tree.
    pub async fn score(&self) -> OracleResult {
        crate::oracle::score_workspace(&self.test_command, self.min_tests).await
    }
}

/// K from the raw flag value. `Ok(None)` = explicitly off (`0`, `1`).
fn parse_k(raw: &str) -> Result<Option<usize>, String> {
    let k: usize = raw
        .trim()
        .parse()
        .map_err(|_| format!("SIRBONE_BEST_OF: expected a number, got {raw:?}"))?;
    match k {
        0 | 1 => Ok(None),
        k if k <= MAX_K => Ok(Some(k)),
        k => Err(format!(
            "SIRBONE_BEST_OF={k} is above the cap of {MAX_K}; each attempt is a full run"
        )),
    }
}

/// Whether a finished attempt should displace the incumbent.
///
/// Strictly fewer failing tests, and nothing else. A tie keeps the earlier
/// attempt: its tree is already on disk and it cost less, so swapping would
/// churn the work-tree for no measured gain. `usize::MAX` — the oracle's marker
/// for a test command that timed out or could not spawn — loses to every real
/// count, which is what stops an infra flake from winning a selection.
pub fn improves(candidate: &OracleResult, best: &OracleResult) -> bool {
    candidate.failed < best.failed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(failed: usize) -> OracleResult {
        OracleResult {
            passed: failed == 0,
            failed,
            raw: String::new(),
        }
    }

    #[test]
    fn k_is_parsed_and_bounded() {
        assert_eq!(parse_k("2"), Ok(Some(2)));
        assert_eq!(parse_k(" 4 "), Ok(Some(4)));
        // Explicitly off, not an error: a bench arm sets 1 for its control.
        assert_eq!(parse_k("1"), Ok(None));
        assert_eq!(parse_k("0"), Ok(None));
        assert!(parse_k("5").is_err(), "above the cap");
        assert!(parse_k("yes").is_err());
        assert!(parse_k("").is_err());
    }

    #[test]
    fn selection_prefers_fewer_failures_and_keeps_ties() {
        assert!(improves(&res(0), &res(3)));
        assert!(improves(&res(1), &res(2)));
        assert!(
            !improves(&res(2), &res(2)),
            "a tie keeps the cheaper attempt"
        );
        assert!(!improves(&res(3), &res(1)));
        // A timed-out or unspawnable test command never wins a selection.
        assert!(!improves(&res(usize::MAX), &res(9)));
        assert!(improves(&res(9), &res(usize::MAX)));
    }
}
