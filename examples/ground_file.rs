//! Measurement helper for the planning A/B bench: look up the verified facts for
//! the claims in a text file against the current working directory (the target
//! repo). No LLM, no network.
//!
//! Run: `cargo run --example ground_file -- <path-to-text>`
//! Prints `facts=<n>` then one `FACT: <line>` per injected fact.

use sirbone::agent::facts;

fn main() {
    let path = std::env::args().nth(1).expect("usage: ground_file <file>");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let root = std::env::current_dir().unwrap_or_default();
    let f = facts(&root, &text);
    println!("facts={}", f.len());
    for line in f {
        println!("FACT: {line}");
    }
}
