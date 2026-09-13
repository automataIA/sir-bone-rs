use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::TypedTool;
use crate::types::lock_or_recover;

/// Cap on the persisted note. Replace-semantics keep it bounded by construction;
/// this also bounds the dynamic context injected on every turn.
pub const MAX_NOTE_CHARS: usize = 4000;

const SECTIONS: [&str; 7] = [
    "Obiettivo",
    "Criteri di accettazione",
    "Invarianti/non-obiettivi",
    "File rilevanti",
    "Decisioni",
    "Comando di verifica",
    "Stato corrente e prossimo passo",
];

#[derive(Default)]
struct NoteState {
    content: String,
    plan_mode: bool,
}

/// Shared, mutable working note. Lives in the `ToolRegistry` (and thus survives
/// context compaction, which only rewrites `messages`); `run_turn` re-injects it
/// each turn so it stays visible regardless of how old the task is.
#[derive(Clone, Default)]
pub struct NoteStore(Arc<Mutex<NoteState>>);

impl NoteStore {
    pub fn get(&self) -> String {
        lock_or_recover(&self.0).content.clone()
    }

    pub fn set(&self, s: String) {
        lock_or_recover(&self.0).content = s;
    }

    pub fn plan_mode(&self) -> bool {
        lock_or_recover(&self.0).plan_mode
    }

    /// Start a task with a small deterministic contract. It is complete at
    /// initialization, so normal work does not spend a model turn authoring it.
    pub fn start_plan(&self, task: &str) {
        // The original user message remains in the transcript. Keep only a
        // compact anchor here because this note is re-injected after turns and
        // compaction.
        let objective: String = task.trim().chars().take(240).collect();
        let objective = if objective.is_empty() {
            "Completare la richiesta corrente."
        } else {
            objective.as_str()
        };
        let content = render_contract(&[
            objective,
            "Richiesta completata e verifica pertinente superata.",
            "Preservare il comportamento estraneo alla richiesta; nessuna modifica non correlata.",
            "Nessuno identificato all'avvio; usare letture prima delle modifiche.",
            "Applicare la modifica minima compatibile con il progetto.",
            "git diff --check",
            "Ispezionare, implementare, verificare.",
        ]);
        let mut state = lock_or_recover(&self.0);
        state.plan_mode = true;
        state.content = content;
        crate::telemetry::add(&crate::telemetry::PLAN_CONTRACT_INITIALIZED, 1);
    }

    pub fn stop_plan(&self) {
        lock_or_recover(&self.0).plan_mode = false;
    }

    /// Set the note only if currently empty. In Plan mode, localization is
    /// appended as bounded context so it cannot overwrite the contract.
    pub fn seed(&self, s: String) {
        let mut state = lock_or_recover(&self.0);
        if state.plan_mode {
            if s.trim().is_empty() {
                return;
            }
            let suffix = format!("\n\n## Contesto localizzato\n{}", s.trim());
            let room = MAX_NOTE_CHARS.saturating_sub(state.content.chars().count());
            state.content.extend(suffix.chars().take(room));
        } else if state.content.trim().is_empty() {
            state.content = s;
        }
    }

    /// Required fields which are absent, empty, or still placeholders.
    pub fn incomplete_sections(&self) -> Vec<&'static str> {
        let state = lock_or_recover(&self.0);
        if !state.plan_mode {
            return Vec::new();
        }
        incomplete_sections(&state.content)
    }

    fn replace(&self, proposed: String) -> Result<()> {
        let mut state = lock_or_recover(&self.0);
        if !state.plan_mode {
            state.content = proposed.chars().take(MAX_NOTE_CHARS).collect();
            return Ok(());
        }
        if proposed.chars().count() > MAX_NOTE_CHARS {
            bail!("note exceeds the {MAX_NOTE_CHARS}-character Plan mode limit");
        }

        // Preserve mandatory sections omitted by a replacement. Free-form and
        // optional sections still have ordinary replace semantics.
        let previous = parse_sections(&state.content);
        let next = parse_sections(&proposed);
        let values: Vec<&str> = SECTIONS
            .iter()
            .map(|heading| {
                next.get(*heading)
                    .filter(|value| !is_placeholder(value))
                    .or_else(|| previous.get(*heading))
                    .copied()
                    .unwrap_or("")
            })
            .collect();
        state.content = render_contract(&values);
        crate::telemetry::add(&crate::telemetry::PLAN_CONTRACT_UPDATED, 1);
        Ok(())
    }
}

fn render_contract(values: &[&str]) -> String {
    SECTIONS
        .iter()
        .zip(values)
        .map(|(heading, value)| format!("## {heading}\n{}", value.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn parse_sections(content: &str) -> HashMap<&str, &str> {
    let mut out = HashMap::new();
    for (index, heading) in SECTIONS.iter().enumerate() {
        let marker = format!("## {heading}\n");
        let Some(body_start) = content.find(&marker).map(|p| p + marker.len()) else {
            continue;
        };
        let body_end = SECTIONS
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .filter_map(|(_, other)| content[body_start..].find(&format!("\n## {other}\n")))
            .min()
            .map_or(content.len(), |relative| body_start + relative);
        out.insert(*heading, content[body_start..body_end].trim());
    }
    out
}

// Kept separate to make placeholder policy table-testable and conservative.
fn is_placeholder(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "todo" | "tbd" | "da definire" | "da compilare" | "..." | "-"
    )
}

fn incomplete_sections(content: &str) -> Vec<&'static str> {
    let parsed = parse_sections(content);
    SECTIONS
        .iter()
        .copied()
        .filter(|heading| parsed.get(heading).is_none_or(|v| is_placeholder(v)))
        .collect()
}

#[derive(Deserialize, JsonSchema)]
pub struct NoteInput {
    /// The full updated note (this REPLACES the previous one).
    pub content: String,
}

pub struct NoteTool {
    pub store: NoteStore,
}

#[async_trait]
impl TypedTool for NoteTool {
    type Input = NoteInput;

    fn name(&self) -> &'static str {
        "note"
    }

    fn description(&self) -> &'static str {
        "Save your working notes — plan, root-cause hypothesis, files changed so far, \
         what's left. This REPLACES the previous note, so keep one concise living \
         summary (not an append log). The note persists across context compaction."
    }

    async fn run(&self, input: NoteInput) -> Result<String> {
        self.store.replace(input.content)?;
        Ok("note saved".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replaces_and_caps_outside_plan_mode() {
        let store = NoteStore::default();
        let tool = NoteTool {
            store: store.clone(),
        };
        tool.run(NoteInput {
            content: "first".into(),
        })
        .await
        .unwrap();
        tool.run(NoteInput {
            content: "x".repeat(5000),
        })
        .await
        .unwrap();
        assert_eq!(store.get().chars().count(), MAX_NOTE_CHARS);
    }

    #[tokio::test]
    async fn deterministic_contract_is_complete_and_preserves_sections() {
        let store = NoteStore::default();
        store.start_plan("Fix the parser");
        assert!(store.incomplete_sections().is_empty());
        assert!(store.get().len() < 600);

        NoteTool {
            store: store.clone(),
        }
        .run(NoteInput {
            content: "## Obiettivo\nParser corretto".into(),
        })
        .await
        .unwrap();
        let note = store.get();
        assert!(note.contains("## Obiettivo\nParser corretto"));
        for heading in SECTIONS {
            assert!(note.contains(&format!("## {heading}\n")));
        }
        assert!(store.incomplete_sections().is_empty());
    }

    #[tokio::test]
    async fn plan_rejects_oversize_and_placeholder_does_not_erase_value() {
        let store = NoteStore::default();
        store.start_plan("x");
        let tool = NoteTool {
            store: store.clone(),
        };
        assert!(tool
            .run(NoteInput {
                content: "x".repeat(MAX_NOTE_CHARS + 1),
            })
            .await
            .is_err());
        tool.run(NoteInput {
            content: "## Obiettivo\nTBD".into(),
        })
        .await
        .unwrap();
        assert!(store.get().contains("## Obiettivo\nx"));
    }

    #[test]
    fn parser_reports_missing_empty_and_placeholder_sections() {
        let missing = incomplete_sections("## Obiettivo\nTODO\n\n## Decisioni\n");
        assert!(missing.contains(&"Obiettivo"));
        assert!(missing.contains(&"Decisioni"));
        assert!(missing.contains(&"Criteri di accettazione"));
    }

    #[test]
    fn localization_is_appended_without_overwriting_contract() {
        let store = NoteStore::default();
        store.start_plan("x");
        store.seed("src/parser.rs".into());
        let note = store.get();
        assert!(note.contains("## Obiettivo\nx"));
        assert!(note.contains("## Contesto localizzato\nsrc/parser.rs"));
        assert!(note.chars().count() <= MAX_NOTE_CHARS);
    }
}
