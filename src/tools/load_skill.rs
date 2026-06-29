use anyhow::{bail, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::TypedTool;
use crate::skills::{load_skill_body, scan_skills};

#[derive(Deserialize, JsonSchema)]
pub struct LoadSkillInput {
    /// Name of the skill to load (as listed in the <skills> catalog)
    pub name: String,
}

pub struct LoadSkillTool;

#[async_trait]
impl TypedTool for LoadSkillTool {
    type Input = LoadSkillInput;

    fn name(&self) -> &'static str {
        "load_skill"
    }

    fn description(&self) -> &'static str {
        "Load a skill's full instructions by name. Call before using a skill listed in <skills>."
    }

    async fn run(&self, input: LoadSkillInput) -> Result<String> {
        let skills = scan_skills();
        let Some(skill) = skills.into_iter().find(|s| s.name == input.name) else {
            bail!("no skill named '{}'", input.name);
        };
        match load_skill_body(&skill.path) {
            Some(body) => Ok(body),
            None => bail!("skill '{}' has no instructions", input.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_skill_errors() {
        let tool = LoadSkillTool;
        let err = tool
            .run(LoadSkillInput {
                name: "definitely-not-a-skill".into(),
            })
            .await;
        assert!(err.is_err());
    }
}
