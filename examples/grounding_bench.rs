//! Correctness bench for `SIRBONE_GROUND` facts-injection (no LLM, no network).
//!
//! Builds a controlled fixture repo and runs the grounder on drafts that include
//! the real false-positive traps from the planning A/B (a file the plan intends
//! to *create*, "no X *usage*", glob/URL/compound spans, line-number adjacency).
//! Asserts every emitted fact is TRUE and neutral, and that the traps produce no
//! *false* fact. A facts design has no precision/recall in the verdict sense; the
//! invariant is **no false fact**.
//!
//! Run: `cargo run --example grounding_bench`

use std::path::Path;

use sirbone::agent::facts;

fn build_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    // 3 whole-word `unwrap`; symbols validate + Config; src/main.rs absent.
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn validate() {}\npub struct Config {}\nlet a=x.unwrap();\nlet b=y.unwrap()+z.unwrap();\n",
    )
    .unwrap();
}

/// (draft, must-contain substrings, must-NOT-contain substrings, label)
type Case = (&'static str, &'static [&'static str], &'static [&'static str], &'static str);

fn cases() -> Vec<Case> {
    vec![
        (
            "Create a `src/agent/classifier.rs` module for it.",
            &["`src/agent/classifier.rs` is not a current file"],
            &[],
            "to-create file -> neutral fact (was a false 'does not exist' accusation)",
        ),
        (
            "The doctor path has no `validate` usage.",
            &["`validate` is defined in src/lib.rs"],
            &["is not a current file"],
            "'no X usage' -> authoritative location, NOT an absence accusation",
        ),
        (
            "They have no dependency on `Config`.",
            &["`Config` is defined in src/lib.rs"],
            &[],
            "'no dependency on X' -> location fact, not 'X missing'",
        ),
        (
            "It has 99 `unwrap`s.",
            &["`unwrap` occurs 3 times", "99"],
            &[],
            "wrong count -> real number",
        ),
        (
            "Edit `src/lib.rs`.",
            &[],
            &["`src/lib.rs` is not a current file"],
            "existing path -> no noise fact",
        ),
        (
            "We use `HashMap` and `Vec`.",
            &[],
            &["HashMap", "Vec"],
            "external symbols -> NO fact (no wrong fact)",
        ),
        (
            "Touch all `src/*.rs`, see `https://x.io/a.rs`, span `mock.rs:1 fn f`.",
            &[],
            &["src/*.rs", "x.io", "mock.rs"],
            "glob / URL / compound span -> ignored",
        ),
        (
            "Defined around line 23 `Config`.",
            &["`Config` is defined in src/lib.rs"],
            &["occurs"],
            "line-number adjacency -> not a count",
        ),
    ]
}

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    build_fixture(root);

    let mut pass = 0u32;
    let mut fail = 0u32;
    for (draft, want, deny, label) in cases() {
        let f = facts(root, draft);
        let joined = f.join("\n");
        let miss: Vec<_> = want.iter().filter(|w| !joined.contains(**w)).collect();
        let bad: Vec<_> = deny.iter().filter(|d| joined.contains(**d)).collect();
        let ok = miss.is_empty() && bad.is_empty();
        if ok {
            pass += 1;
        } else {
            fail += 1;
        }
        println!("[{}] {label}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            if !miss.is_empty() {
                println!("    missing: {miss:?}");
            }
            if !bad.is_empty() {
                println!("    FALSE FACT present: {bad:?}");
            }
            println!("    facts: {f:?}");
        }
    }
    println!("\n{pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
}
