use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::TypedTool;

#[derive(Deserialize, JsonSchema, Default)]
pub struct VerifyInput {
    /// Optional one-line note on what you're checking (for the log). Ignored otherwise.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Runs the project's configured test command on demand and reports the verdict.
/// The model calls this to self-check before claiming done; humans trigger the
/// same via `/verify`. Command source: `oracle.test_command` in config.
pub struct VerifyTool;

#[async_trait]
impl TypedTool for VerifyTool {
    type Input = VerifyInput;

    fn name(&self) -> &'static str {
        "verify"
    }

    fn description(&self) -> &'static str {
        "Run the project's configured test command and report pass/fail with the failing \
         lines hoisted. Use it to self-check before reporting done — never claim success \
         without verifying. Verification is only as strong as the tests: prefer checking \
         PROPERTIES/INVARIANTS (round-trip, idempotence, bounds, monotonicity, agreement \
         with a simple naive reference) over echoing the example inputs/outputs — buggy \
         code often passes its author's example-only tests. Needs `oracle.test_command` \
         set in ~/.sirbone/config.json."
    }

    async fn run(&self, _input: VerifyInput) -> Result<String> {
        Ok(crate::oracle::verify_once().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verify_runs_without_config() {
        // With no oracle config in the test env, it returns guidance, not an error.
        let out = VerifyTool.run(VerifyInput::default()).await.unwrap();
        assert!(!out.is_empty());
    }
}
