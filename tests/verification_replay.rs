//! Deterministic, network-free replay of the three verification loops. Each
//! scripted client first leaves a reproducible failure, observes deterministic
//! feedback, and then supplies the correction.

mod common;

use common::{ctx, text_turn, tool_turn, MockClient};
use sirbone::checks::{Hooks, PostEditChecks};
use sirbone::{ReadStamps, ToolRegistry, UndoStore, WriteTool};
use tokio::sync::mpsc;

fn tools() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(WriteTool {
        undo: UndoStore::default(),
        stamps: ReadStamps::default(),
    });
    registry
}

#[tokio::test]
async fn verification_feedback_is_observed_and_corrected_with_exact_counters() {
    // Keep this as the only test in this integration-test process: telemetry is
    // process-wide, which makes its final delta exact and race-free.
    let dir = tempfile::tempdir().unwrap();

    let post_path = dir.path().join("post.rs");
    let post = MockClient::new(vec![
        tool_turn(
            "post-bad",
            "write",
            serde_json::json!({"path": post_path, "content": "bad"}),
        ),
        tool_turn(
            "post-good",
            "write",
            serde_json::json!({"path": post_path, "content": "good"}),
        ),
        text_turn("done after post-check feedback"),
    ]);
    let (tx, _rx) = mpsc::channel(64);
    let mut post_ctx = ctx(post, tools(), tx);
    post_ctx.hooks.post = PostEditChecks::new([(
        post_path.to_string_lossy().into_owned(),
        format!("grep -qx good {}", post_path.display()),
    )]);
    sirbone::run(&mut post_ctx).await.unwrap();
    assert_eq!(std::fs::read_to_string(&post_path).unwrap(), "good");

    let oracle_path = dir.path().join("oracle.txt");
    std::fs::write(&oracle_path, "bad").unwrap();
    let oracle = MockClient::new(vec![
        text_turn("done too early"),
        tool_turn(
            "oracle-fix",
            "write",
            serde_json::json!({"path": oracle_path, "content": "good"}),
        ),
        text_turn("done after oracle feedback"),
    ]);
    let (tx, _rx) = mpsc::channel(64);
    let mut oracle_ctx = ctx(oracle, tools(), tx);
    oracle_ctx.oracle =
        sirbone::oracle::Oracle::new(format!("grep -qx good {}", oracle_path.display()), 3);
    sirbone::run(&mut oracle_ctx).await.unwrap();
    assert_eq!(std::fs::read_to_string(&oracle_path).unwrap(), "good");

    let marker = dir.path().join("completion.marker");
    let stop = MockClient::new(vec![
        text_turn("done without completion invariant"),
        tool_turn(
            "stop-fix",
            "write",
            serde_json::json!({"path": marker, "content": "complete"}),
        ),
        text_turn("done after stop-hook feedback"),
    ]);
    let (tx, _rx) = mpsc::channel(64);
    let mut stop_ctx = ctx(stop, tools(), tx);
    stop_ctx.hooks = Hooks {
        stop: vec![format!(
            "test -f {} || {{ echo completion marker missing; exit 2; }}",
            marker.display()
        )],
        ..Default::default()
    };
    sirbone::run(&mut stop_ctx).await.unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "complete");

    let counters = sirbone::telemetry::run_delta();
    assert_eq!(counters.hook_post_runs, 2);
    assert_eq!(counters.hook_post_failures, 1);
    assert_eq!(counters.oracle_runs, 2);
    assert_eq!(counters.oracle_failures, 1);
    assert_eq!(counters.oracle_retries, 1);
    assert_eq!(counters.oracle_exhausted, 0);
    assert_eq!(counters.hook_stop_runs, 2);
    assert_eq!(counters.hook_stop_retries, 1);
    assert_eq!(counters.hook_stop_exhausted, 0);
}
