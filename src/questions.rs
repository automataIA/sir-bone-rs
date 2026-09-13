use serde::{Deserialize, Serialize};

/// One decision option. The model-facing schema publishes the explanatory
/// object form; deserialization also accepts a bare string for old transcripts.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QuestionOption {
    /// Short value returned to the model when this option is selected.
    pub label: String,
    /// Concrete consequence or trade-off the user should understand.
    pub description: String,
}

impl<'de> Deserialize<'de> for QuestionOption {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Historical(String),
            Explained { label: String, description: String },
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Historical(label) => Self {
                label,
                description: String::new(),
            },
            Wire::Explained { label, description } => Self { label, description },
        })
    }
}

impl QuestionOption {
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
        }
    }

    pub fn display(&self) -> String {
        if self.description.trim().is_empty() {
            self.label.clone()
        } else {
            format!("{} — {}", self.label, self.description)
        }
    }
}

impl From<String> for QuestionOption {
    fn from(label: String) -> Self {
        Self::new(label, "")
    }
}

impl From<&str> for QuestionOption {
    fn from(label: &str) -> Self {
        Self::new(label, "")
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Question {
    pub id: String,
    /// Premise, constraints and why the decision matters. Do not repeat the
    /// question or hide consequences here.
    pub context: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
}

impl<'de> Deserialize<'de> for Question {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            id: String,
            #[serde(default)]
            context: String,
            question: String,
            options: Vec<QuestionOption>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            id: wire.id,
            context: wire.context,
            question: wire.question,
            options: wire.options,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct QuestionRound {
    pub questions: Vec<Question>,
}

impl QuestionRound {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=3).contains(&self.questions.len()) {
            return Err("a round must contain 1-3 questions".into());
        }
        let mut ids = std::collections::HashSet::new();
        for q in &self.questions {
            if q.id.trim().is_empty() || !ids.insert(q.id.as_str()) {
                return Err("question ids must be non-empty and unique".into());
            }
            if q.question.trim().is_empty() || !(2..=4).contains(&q.options.len()) {
                return Err(format!("question `{}` must have 2-4 options", q.id));
            }
            for option in &q.options {
                if option.label.trim().is_empty() {
                    return Err(format!("question `{}` has an empty option label", q.id));
                }
            }
        }
        Ok(())
    }

    /// Parse the current model-facing shape while retaining the historical
    /// single-question payload for replay and older clients. The returned flag
    /// is true for the legacy shape; it is deliberately absent from the schema.
    pub fn from_tool_arguments(value: &serde_json::Value) -> Result<(Self, bool), String> {
        if value.get("questions").is_some() {
            let round: Self = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            round.validate()?;
            return Ok((round, false));
        }
        let question = value
            .get("question")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "missing `questions` (or legacy `question`)".to_string())?;
        let options = value
            .get("options")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "missing legacy `options`".to_string())?
            .iter()
            .map(|item| serde_json::from_value(item.clone()).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let round = Self {
            questions: vec![Question {
                id: "answer".into(),
                context: value
                    .get("context")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .into(),
                question: question.into(),
                options,
            }],
        };
        round.validate()?;
        Ok((round, true))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnswerOrigin {
    User,
    FreeText,
    Dismissed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub id: String,
    pub value: String,
    pub index: Option<usize>,
    pub origin: AnswerOrigin,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_round_contract() {
        let round = QuestionRound {
            questions: vec![Question {
                id: "test".into(),
                context: "The choice changes the data path.".into(),
                question: "Which?".into(),
                options: vec![
                    QuestionOption::new("A", "Keeps compatibility"),
                    QuestionOption::new("B", "Allows a breaking change"),
                ],
            }],
        };
        assert!(round.validate().is_ok());
    }

    #[test]
    fn accepts_legacy_arguments_without_publishing_them() {
        let (round, legacy) = QuestionRound::from_tool_arguments(&serde_json::json!({
            "question": "Which?", "options": ["A", "B"]
        }))
        .unwrap();
        assert!(legacy);
        assert_eq!(round.questions[0].id, "answer");
        assert_eq!(round.questions[0].options[0].label, "A");
    }

    #[test]
    fn accepts_explained_options_in_single_question_shape() {
        let (round, legacy) = QuestionRound::from_tool_arguments(&serde_json::json!({
            "context": "Affects compatibility.",
            "question": "Which?",
            "options": [
                {"label": "A", "description": "Compatible"},
                {"label": "B", "description": "Breaking"}
            ]
        }))
        .unwrap();
        assert!(legacy);
        assert_eq!(round.questions[0].context, "Affects compatibility.");
        assert_eq!(round.questions[0].options[1].display(), "B — Breaking");
    }

    #[test]
    fn accepts_historical_string_options_in_rounds() {
        let (round, legacy) = QuestionRound::from_tool_arguments(&serde_json::json!({
            "questions": [{"id": "x", "question": "Which?", "options": ["A", "B"]}]
        }))
        .unwrap();
        assert!(!legacy);
        assert_eq!(round.questions[0].context, "");
        assert_eq!(round.questions[0].options[1].display(), "B");
    }

    #[test]
    fn published_round_schema_requires_explanatory_fields() {
        let schema = serde_json::to_value(schemars::schema_for!(QuestionRound)).unwrap();
        let question_required = schema["$defs"]["Question"]["required"].as_array().unwrap();
        assert!(question_required.iter().any(|field| field == "context"));
        let option_required = schema["$defs"]["QuestionOption"]["required"]
            .as_array()
            .unwrap();
        assert!(option_required.iter().any(|field| field == "label"));
        assert!(option_required.iter().any(|field| field == "description"));
    }
}
