//! Shared syntax-highlight helpers, free of any UI type. Both the ratatui TUI
//! (`tui::markdown`) and the crossterm REPL (`render`) drive `synoptic` through
//! these: map a fence language to a synoptic extension, and a synoptic token
//! kind to a semantic [`SynRole`] that each side resolves to its own color.

/// Map a markdown fence language name to a synoptic file extension.
pub fn lang_to_ext(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "rust" | "rs" => "rs",
        "python" | "py" => "py",
        "javascript" | "js" | "jsx" => "js",
        "typescript" | "ts" | "tsx" => "ts",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "c++" | "cc" | "cxx" => "cpp",
        "csharp" | "cs" | "c#" => "cs",
        "java" => "java",
        "kotlin" | "kt" => "kt",
        "swift" => "swift",
        "scala" => "scala",
        "ruby" | "rb" => "rb",
        "php" => "php",
        "lua" => "lua",
        "haskell" | "hs" => "hs",
        "r" => "r",
        "dart" => "dart",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "xml" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "sql" => "sql",
        "bash" | "sh" | "shell" | "zsh" => "sh",
        "diff" | "patch" => "diff",
        "markdown" | "md" => "md",
        _ => return None,
    })
}

/// Semantic class of a highlighted token, resolved to a concrete color by each
/// front-end. Keeps the synoptic token-kind vocabulary in one place.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SynRole {
    Comment,
    Keyword,
    Str,
    Number,
    Function,
    Type,
    Reference,
    Macro,
    Heading,
    Link,
    Plain,
}

/// Classify a synoptic token-kind name into a [`SynRole`].
pub fn syn_role(name: &str) -> SynRole {
    match name {
        "comment" => SynRole::Comment,
        "keyword" => SynRole::Keyword,
        "string" | "character" => SynRole::Str,
        "digit" | "number" | "boolean" => SynRole::Number,
        "function" => SynRole::Function,
        "struct" | "type" => SynRole::Type,
        "namespace" | "reference" => SynRole::Reference,
        "macro" | "attribute" | "tag" => SynRole::Macro,
        "heading" => SynRole::Heading,
        "link" => SynRole::Link,
        _ => SynRole::Plain,
    }
}
