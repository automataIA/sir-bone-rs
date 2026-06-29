#[test]
fn policy_pack_json_files_parse() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/policy-packs");
    let entries = std::fs::read_dir(&dir).expect("policy-packs dir");
    let mut seen = 0;
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        seen += 1;
        let text = std::fs::read_to_string(&path).expect("read policy pack");
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|e| panic!("{} should parse as JSON: {e}", path.display()));
    }
    assert!(seen >= 4, "expected policy pack JSON files");
}
