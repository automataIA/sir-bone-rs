use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::TypedTool;

/// Input for the `ask_user` tool: a question and its selectable options.
#[derive(Deserialize, JsonSchema)]
pub struct AskUserInput {
    /// Relevant premise, constraints, and why this decision matters.
    pub context: String,
    /// The question to put to the user.
    pub question: String,
    /// 2-4 mutually exclusive choices. Explain the concrete consequence or
    /// trade-off of every choice. A free-text "Other" is added by the UI.
    pub options: Vec<crate::questions::QuestionOption>,
}

/// Ask the user a multiple-choice question when a decision is genuinely theirs
/// to make. Registered for its schema only: actual execution is intercepted in
/// the agent loop ([`crate::agent`]), which routes the question through the
/// interactive prompt bridge and feeds the answer back as this call's result.
pub struct AskUserTool;

/// Experimental multi-question schema. Registered only with
/// `SIRBONE_ASK_ROUNDS=1` (and ablatable via `ask:rounds`).
pub struct AskUserRoundTool;

#[async_trait]
impl TypedTool for AskUserTool {
    type Input = AskUserInput;

    fn name(&self) -> &'static str {
        "ask_user"
    }

    fn description(&self) -> &'static str {
        "Ask the user a multiple-choice question when the decision is genuinely \
         theirs — an ambiguous requirement, a library/approach choice, a naming or \
         scope call you can't infer from the codebase. State the relevant context, \
         then provide 2-4 concise options with the consequence of each choice; \
         the UI adds an \"Other\" free-text choice automatically, so never add one. \
         The result reports the user's selection (or their typed answer). Use \
         sparingly: if a reasonable default exists, take it instead of asking. In a \
         non-interactive session no user can answer, and the tool tells you to \
         proceed with your best default."
    }

    async fn run(&self, input: AskUserInput) -> Result<String> {
        // Reached only if the interactive interception is bypassed (e.g. no
        // bridge). Fall back to the first option so the agent still progresses.
        let fallback = input
            .options
            .first()
            .map(|option| option.label.clone())
            .unwrap_or_else(|| "no options provided".into());
        Ok(format!(
            "No interactive user is available to answer; proceeding with the default: {fallback}"
        ))
    }
}

#[async_trait]
impl TypedTool for AskUserRoundTool {
    type Input = crate::questions::QuestionRound;

    fn name(&self) -> &'static str {
        "ask_user"
    }

    fn description(&self) -> &'static str {
        "Ask 1-3 independent multiple-choice questions in one round. For each, state the relevant context and give 2-4 options with a short label plus the concrete consequence or trade-off. Put the recommended option first. Dependent questions belong in a later round."
    }

    async fn run(&self, input: Self::Input) -> Result<String> {
        input.validate().map_err(anyhow::Error::msg)?;
        Ok("No interactive user is available; proceed with the first recommended option for each question and state the assumptions.".into())
    }
}
