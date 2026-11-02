//! Reverse dependency reachability and advisory affected-test discovery.
//!
//! The test check is deliberately textual and conventional: it can miss
//! generated or dynamically selected tests, and a marker in a comment/string
//! can produce a false positive. Treat its results as candidates to inspect.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use regex::Regex;

/// A file reachable from a changed seed through reverse dependency edges.
/// `chain` runs from a seed to `path`; its length minus one is the hop count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reach {
    pub path: PathBuf,
    pub chain: Vec<PathBuf>,
}

/// A reachable indexed file that looks like a test file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCandidate {
    pub path: PathBuf,
    pub line: usize,
    pub chain: Vec<PathBuf>,
}

/// Find reverse-reachable files from `seeds`.
///
/// Edges are `(source, dependency)` pairs, so traversal follows edges backward
/// from a dependency to files that depend on it. Seeds are included at hop 0.
/// Sorted roots and neighbors make the shortest witness chain deterministic.
pub fn reverse_paths(
    edges: &[(PathBuf, PathBuf)],
    seeds: &[PathBuf],
    max_depth: usize,
) -> Vec<Reach> {
    let mut reverse: HashMap<&Path, Vec<&Path>> = HashMap::new();
    for (source, dependency) in edges {
        reverse
            .entry(dependency.as_path())
            .or_default()
            .push(source.as_path());
    }
    for neighbors in reverse.values_mut() {
        neighbors.sort_unstable();
        neighbors.dedup();
    }

    let mut roots = seeds.iter().collect::<Vec<_>>();
    roots.sort_unstable();
    roots.dedup();

    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    let mut out = Vec::new();
    for root in roots {
        if seen.insert(root.clone()) {
            let chain = vec![root.clone()];
            queue.push_back((root.clone(), chain.clone()));
            out.push(Reach {
                path: root.clone(),
                chain,
            });
        }
    }

    while let Some((path, chain)) = queue.pop_front() {
        let depth = chain.len() - 1;
        if depth >= max_depth {
            continue;
        }
        if let Some(neighbors) = reverse.get(path.as_path()) {
            for neighbor in neighbors {
                if seen.insert((*neighbor).to_path_buf()) {
                    let mut next_chain = chain.clone();
                    next_chain.push((*neighbor).to_path_buf());
                    queue.push_back(((*neighbor).to_path_buf(), next_chain.clone()));
                    out.push(Reach {
                        path: (*neighbor).to_path_buf(),
                        chain: next_chain,
                    });
                }
            }
        }
    }

    out.sort_unstable_by(|a, b| (a.chain.len(), &a.path).cmp(&(b.chain.len(), &b.path)));
    out
}

/// Find reachable indexed files that conventionally contain tests or contain
/// Rust test markers. Returned candidates follow reverse_paths ordering.
pub fn affected_tests(
    root: &Path,
    index: &super::Index,
    edges: &[(PathBuf, PathBuf)],
    seeds: &[PathBuf],
    max_depth: usize,
) -> Vec<TestCandidate> {
    let mut candidates = Vec::new();
    for reach in reverse_paths(edges, seeds, max_depth) {
        if !index.files.contains_key(&reach.path) {
            continue;
        }
        let marker_line = if reach.path.extension().is_some_and(|ext| ext == "rs") {
            super::read_source(&reach.path)
                .as_deref()
                .and_then(rust_test_marker_line)
        } else {
            None
        };
        let relative_path = reach.path.strip_prefix(root).unwrap_or(&reach.path);
        if let Some(line) =
            marker_line.or_else(|| is_conventional_test_path(relative_path).then_some(1))
        {
            candidates.push(TestCandidate {
                path: reach.path,
                line,
                chain: reach.chain,
            });
        }
    }
    candidates
}

fn rust_test_marker_line(content: &str) -> Option<usize> {
    static MARKER: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"(?m)#\s*\[\s*(?:test\b|tokio\s*::\s*test\b(?:\s*\([^]]*\))?|cfg\s*\(\s*test\s*\))\s*\]",
        )
        .expect("static test-marker regex must compile")
    });
    MARKER
        .find(content)
        .map(|m| content[..m.start()].bytes().filter(|b| *b == b'\n').count() + 1)
}

fn is_conventional_test_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    if path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("tests" | "test" | "__tests__")
        )
    }) {
        return true;
    }
    if [".spec.js", ".spec.jsx", ".spec.ts", ".spec.tsx"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
        || [".test.js", ".test.jsx", ".test.ts", ".test.tsx"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
    {
        return true;
    }
    (lower.ends_with(".py") && lower.starts_with("test_"))
        || lower.ends_with("_test.py")
        || lower.ends_with("_test.rs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structure;

    fn p(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    fn chain(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(|path| p(path)).collect()
    }

    #[test]
    fn reverse_bfs_is_sorted_shortest_and_cycle_safe() {
        let edges = vec![
            (p("src/z.rs"), p("src/a.rs")),
            (p("src/a.rs"), p("src/core.rs")),
            (p("src/b.rs"), p("src/core.rs")),
            (p("src/a.rs"), p("src/b.rs")),
            (p("src/core.rs"), p("src/a.rs")),
            (p("src/b.rs"), p("src/a.rs")),
        ];
        let reaches = reverse_paths(&edges, &[p("src/core.rs"), p("src/core.rs")], 3);
        assert_eq!(
            reaches,
            vec![
                Reach {
                    path: p("src/core.rs"),
                    chain: chain(&["src/core.rs"])
                },
                Reach {
                    path: p("src/a.rs"),
                    chain: chain(&["src/core.rs", "src/a.rs"])
                },
                Reach {
                    path: p("src/b.rs"),
                    chain: chain(&["src/core.rs", "src/b.rs"])
                },
                Reach {
                    path: p("src/z.rs"),
                    chain: chain(&["src/core.rs", "src/a.rs", "src/z.rs"])
                },
            ]
        );
    }

    #[test]
    fn reverse_bfs_has_deterministic_multiple_roots_and_depth_bound() {
        let edges = vec![
            (p("src/shared.rs"), p("src/b.rs")),
            (p("src/shared.rs"), p("src/a.rs")),
            (p("src/deep.rs"), p("src/shared.rs")),
        ];
        let reaches = reverse_paths(&edges, &[p("src/b.rs"), p("src/a.rs")], 1);
        assert_eq!(
            reaches,
            vec![
                Reach {
                    path: p("src/a.rs"),
                    chain: chain(&["src/a.rs"])
                },
                Reach {
                    path: p("src/b.rs"),
                    chain: chain(&["src/b.rs"])
                },
                Reach {
                    path: p("src/shared.rs"),
                    chain: chain(&["src/a.rs", "src/shared.rs"])
                },
            ]
        );
    }

    #[test]
    fn affected_tests_finds_conventional_and_inline_rust_tests() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tests").join("project");
        std::fs::create_dir_all(&root).unwrap();
        let changed = root.join("src/lib.rs");
        let direct_test = root.join("tests/changed.rs");
        let convention = root.join("tests/integration.rs");
        let inline = root.join("src/helper.rs");
        let ordinary = root.join("src/ordinary.rs");
        for path in [&changed, &direct_test, &convention, &inline, &ordinary] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&changed, "pub fn changed() {}\n").unwrap();
        std::fs::write(&direct_test, "use app;\n").unwrap();
        std::fs::write(&convention, "use app;\n").unwrap();
        std::fs::write(
            &inline,
            "fn helper() {}\n\n #[ tokio :: test ]\nasync fn works() {}\n",
        )
        .unwrap();
        std::fs::write(&ordinary, "fn ordinary() {}\n").unwrap();
        let index = structure::update(&root, structure::Index::default());
        let edges = vec![
            (convention.clone(), changed.clone()),
            (inline.clone(), changed.clone()),
            (ordinary.clone(), changed.clone()),
        ];

        let found = affected_tests(
            &root,
            &index,
            &edges,
            &[changed.clone(), direct_test.clone()],
            1,
        );
        assert_eq!(
            found,
            vec![
                TestCandidate {
                    path: direct_test.clone(),
                    line: 1,
                    chain: vec![direct_test.clone()]
                },
                TestCandidate {
                    path: inline.clone(),
                    line: 3,
                    chain: vec![root.join("src/lib.rs"), inline]
                },
                TestCandidate {
                    path: convention.clone(),
                    line: 1,
                    chain: vec![root.join("src/lib.rs"), convention]
                },
            ]
        );
    }

    #[test]
    fn rust_marker_line_handles_cfg_and_tokio_arguments() {
        assert_eq!(
            rust_test_marker_line("fn helper() {}\n#[cfg (\n    test\n)]\n"),
            Some(2)
        );
        assert_eq!(
            rust_test_marker_line(
                "fn helper() {}\n#[tokio::test(\n    flavor = \"multi_thread\"\n)]\n"
            ),
            Some(2)
        );
    }

    #[test]
    fn spec_suffixes_are_limited_to_supported_javascript_extensions() {
        assert!(is_conventional_test_path(Path::new("widget.spec.tsx")));
        assert!(!is_conventional_test_path(Path::new("widget.spec.rs")));
    }
}
