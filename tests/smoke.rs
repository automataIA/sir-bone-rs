//! Smoke tests for the `sirbone` binary: it builds, the CLI surface works, and
//! a missing provider key fails loudly instead of panicking.

use assert_cmd::Command;
use httpmock::{Method::GET, MockServer};
use predicates::str::contains;

fn bin() -> Command {
    let mut c = Command::cargo_bin("sirbone").unwrap();
    // Run away from the repo so dotenvy doesn't pick up a local .env, and strip
    // any provider keys from the inherited environment.
    let tmp = std::env::temp_dir();
    c.current_dir(tmp)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("OPENAI_API_KEY");
    c
}

#[test]
fn help_succeeds() {
    bin()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("AI coding agent"));
}

#[test]
fn version_succeeds() {
    bin()
        .arg("--version")
        .assert()
        .success()
        .stdout(contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn doctor_succeeds_without_provider_key() {
    bin()
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("Sir Bone doctor"))
        .stdout(contains("doctor:"));
}

#[test]
fn doctor_flag_succeeds_without_provider_key() {
    bin()
        .arg("--doctor")
        .assert()
        .success()
        .stdout(contains("Sir Bone doctor"))
        .stdout(contains("provider: no API key"));
}

#[test]
fn doctor_network_without_provider_key_warns_but_succeeds() {
    bin()
        .args(["doctor", "--network"])
        .assert()
        .success()
        .stdout(contains("Sir Bone doctor"))
        .stdout(contains("network probe: skipped"));
}

#[test]
fn doctor_network_probes_openai_compatible_models() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/models");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(serde_json::json!({
                "object": "list",
                "data": [
                    {
                        "id": "gpt-test",
                        "object": "model",
                        "created": 0,
                        "owned_by": "test",
                        "context_window": 12345
                    }
                ]
            }));
    });

    bin()
        .args(["--doctor", "--network", "--model", "gpt-test"])
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", server.base_url())
        .assert()
        .success()
        .stdout(contains("network models: ok (1 model(s))"))
        .stdout(contains("network context window: 12345 tokens"));
}

#[test]
fn audit_succeeds_without_provider_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"message","role":"user","content":[{"type":"text","text":"fix the bug"}]}"#,
            "\n",
            r#"{"type":"message","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"edit","input":{"path":"src/lib.rs"}}]}"#,
            "\n",
            r#"{"type":"message","role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}"#,
            "\n",
            r#"{"type":"run_status","status":"done","reason":null}"#,
            "\n"
        ),
    )
    .unwrap();

    bin()
        .args(["--audit", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("Sir Bone Session Audit"))
        .stdout(contains("Final status: done"))
        .stdout(contains("src/lib.rs"));
}

#[test]
fn audit_json_succeeds_without_provider_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"message","role":"user","content":[{"type":"text","text":"hello"}]}"#,
            "\n",
            r#"{"type":"run_usage","input_tokens":42,"cached_tokens":7,"peak_context_tokens":40}"#,
            "\n",
            r#"{"type":"workspace_snapshot","id":"abc123","label":"hello"}"#,
            "\n",
            r#"{"type":"run_status","status":"error","reason":"boom"}"#,
            "\n"
        ),
    )
    .unwrap();

    bin()
        .args(["--audit", path.to_str().unwrap(), "--audit-json"])
        .assert()
        .success()
        .stdout(contains("\"user_messages\": 1"))
        .stdout(contains("\"total_input_tokens\": 42"))
        .stdout(contains("\"final_status\": \"error\""))
        .stdout(contains("\"status_reason\": \"boom\""))
        .stdout(contains("\"abc123\""));
}

#[test]
fn snapshots_json_succeeds_without_provider_key() {
    bin()
        .args(["snapshots", "--json"])
        .assert()
        .success()
        .stdout(contains("[]"));
}

#[test]
fn missing_key_fails_cleanly() {
    bin()
        .arg("do something")
        .assert()
        .failure()
        .stderr(contains("no API key"));
}
