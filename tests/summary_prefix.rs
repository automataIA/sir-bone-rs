//! `SIRBONE_SUMMARY_VERBATIM`: the summarizer call reuses the turn prefix.
//!
//! Its own test binary because the flag is read from the process environment and
//! changes what every compaction sends — setting it inside the lib test binary
//! would race the compaction tests running beside it.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{ctx, text_turn};
use sirbone::tools::write::WriteTool;
use sirbone::tools::{ReadStamps, UndoStore};
use sirbone::{AgentEvent, EventTx, LlmClient, Message, ToolRegistry, TurnResult};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Records every request it is given, then answers with a short summary.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(Vec<Message>, Vec<String>)>>,
    idx: AtomicUsize,
}

#[async_trait]
impl LlmClient for Recorder {
    async fn run_turn(
        &self,
        messages: &[&Message],
        registry: &ToolRegistry,
        events: &EventTx,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<TurnResult> {
        self.calls.lock().unwrap().push((
            messages.iter().map(|m| (*m).clone()).collect(),
            registry.iter().map(|t| t.name().to_string()).collect(),
        ));
        self.idx.fetch_add(1, Ordering::SeqCst);
        events.send(AgentEvent::TurnEnd).await.ok();
        Ok(text_turn("1. **Files modified**: none"))
    }
}

#[tokio::test]
async fn the_verbatim_summarizer_sends_the_real_prefix_and_the_region_unflattened() {
    std::env::set_var("SIRBONE_SUMMARY_VERBATIM", "1");

    let mut registry = ToolRegistry::new();
    registry.register(WriteTool {
        undo: UndoStore::default(),
        stamps: ReadStamps::default(),
    });

    let client = Arc::new(Recorder::default());
    let (tx, _rx) = mpsc::channel(256);
    let mut c = ctx(client.clone(), registry, tx);
    c.system_prompt = Some("REAL SYSTEM PROMPT".into());
    c.messages = (0..8)
        .map(|i| {
            if i % 2 == 0 {
                Message::user(format!("request {i}"))
            } else {
                Message::assistant(format!("reply {i}"))
            }
        })
        .collect();
    // Default keep-recent is 6, so the folded region is the first two messages.
    let region_size = c.messages.len() - 6;

    sirbone::agent::compact(&mut c).await.expect("compaction");

    let calls = client.calls.lock().unwrap();
    let (sent, tools) = calls.first().expect("summarizer was called");

    // The prefix the provider already cached: same system prompt, same tools.
    assert_eq!(
        sirbone::types::extract_text(&sent[0].content),
        "REAL SYSTEM PROMPT"
    );
    assert_eq!(tools, &["write"]);

    // The region travels as the messages it already is, not re-serialized into
    // one "[User]: …" blob — that identity is what makes it a cache hit.
    let region: Vec<String> = sent[1..sent.len() - 1]
        .iter()
        .map(|m| sirbone::types::extract_text(&m.content))
        .collect();
    assert_eq!(region.len(), region_size, "region: {region:?}");
    assert_eq!(region[0], "request 0");
    assert!(region.iter().all(|m| !m.starts_with("[User]")));

    // The instruction goes last, so everything before it stays an exact prefix.
    let closing = sirbone::types::extract_text(&sent[sent.len() - 1].content);
    assert!(closing.starts_with("You are a conversation summarizer"));
    assert!(closing.contains("Do not call any tool"));

    // And the transcript really was compacted.
    assert!(
        sirbone::types::extract_text(&c.messages[0].content)
            .starts_with("[Previous conversation summary"),
        "not compacted: {:?}",
        c.messages
    );
}
