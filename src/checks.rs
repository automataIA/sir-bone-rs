//! Post-edit checks: auto-lint feedback injected into tool results.
//!
//! Config (`~/.sirbone/config.json`, key `post_edit_check`) maps a path glob to
//! a fast check command, e.g. `{"*.rs": "cargo check -q --message-format=short",
//! "*.py": "ruff check -q"}`. After a batch of mutating tool calls succeeds,
//! every command whose glob matches an edited path runs once (deduplicated);
//! failures are appended to the last tool result so the model reads them inline
//! and fixes the breakage in the same turn — no "remember to run the linter"
//! round-trip. Keep commands quiet/short-format: their output lands in context.
//!
//! Advisory by design: a failed check never reverts the edit and never blocks
//! the loop. No config = no checks (current behavior).
//!
//! [`PostEditChecks`] is the `post_tool_use` event of the more general
//! [`Hooks`] (config key `hooks`), which also exposes a deterministic
//! `pre_tool_use` gate (exit-code allow/deny/ask, input rewrite, or a
//! short-circuit that answers the call without running the tool — all of which
//! skip the LLM command classifier), a `tusk` filter over tool *results*
//! ([`TuskHook`]), and a `stop` hook (force another loop iteration). Legacy
//! top-level `post_edit_check` config still loads, as `hooks.post_tool_use`.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;
use tokio::io::AsyncWriteExt;

use crate::permissions::glob_matches;

const TIMEOUT_SECS: u64 = 30;
const MAX_OUTPUT: usize = 4000;

enum CheckOutcome {
    Passed,
    Failed(String),
    Incomplete(String),
}

/// `path:line[:col][:] ` — the location prefix compilers put in front of a
/// diagnostic. Stripping it makes "same message, moved by an edit" compare equal.
static DIAG_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\S+:\d+(:\d+)?:?\s+").expect("static regex"));

/// Message identity: the diagnostic without its location, so a warning that
/// merely shifted lines is not reported as new.
fn diagnostic_identity(line: &str) -> &str {
    let line = line.trim_end();
    DIAG_PREFIX
        .find(line)
        .map_or(line.trim_start(), |m| &line[m.end()..])
}

/// Per-command memory of diagnostics already shown to the model.
///
/// A post-edit check re-runs after *every* batch of edits, so an unrelated
/// pre-existing warning is otherwise re-billed as input tokens on every single
/// edit. The ledger emits only what changed. It never hides the *failure*: when
/// nothing is new, the block collapses to one line that still says the check
/// is red (see [`PostEditChecks::run`]).
#[derive(Debug, Default)]
struct DiagnosticsLedger(Mutex<HashMap<String, HashSet<String>>>);

/// What the ledger decided about one check's output.
#[derive(Debug, PartialEq, Eq)]
struct Reduced {
    /// Lines never shown for this command before, in original order.
    fresh: String,
    /// How many lines were suppressed as already-reported.
    repeated: usize,
}

impl DiagnosticsLedger {
    fn reduce(&self, cmd: &str, output: &str) -> Reduced {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let seen = guard.entry(cmd.to_string()).or_default();
        let mut fresh = String::new();
        let mut repeated = 0;
        for line in output.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if seen.insert(diagnostic_identity(line).to_string()) {
                fresh.push_str(line);
                fresh.push('\n');
            } else {
                repeated += 1;
            }
        }
        Reduced {
            fresh: fresh.trim_end().to_string(),
            repeated,
        }
    }

    fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

#[derive(Debug, Clone, Default)]
pub struct PostEditChecks {
    /// `(path glob, command)` in config order.
    rules: Vec<(String, String)>,
    /// Shared across clones on purpose: one memory per agent run.
    ledger: Arc<DiagnosticsLedger>,
}

impl PostEditChecks {
    /// Build an explicit rule set. This is useful for deterministic replays and
    /// embedders that already own the configuration lifecycle.
    pub fn new(rules: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            rules: rules.into_iter().collect(),
            ledger: Arc::default(),
        }
    }

    /// Load from `~/.sirbone/config.json` (key `post_edit_check`). Missing
    /// file or malformed config yields no checks — never an error.
    pub fn load() -> Self {
        let Some(home) = std::env::var_os("HOME") else {
            return Self::default();
        };
        let path = std::path::Path::new(&home).join(".sirbone/config.json");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .map(|v| Self::from_value(v.get("post_edit_check")))
            .unwrap_or_default()
    }

    pub(crate) fn from_value(v: Option<&serde_json::Value>) -> Self {
        let rules = v
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(glob, cmd)| Some((glob.clone(), cmd.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            rules,
            ledger: Arc::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Forget every diagnostic reported so far.
    ///
    /// The ledger's whole contract is "already reported **above**", so it is only
    /// sound while the transcript it refers to is intact. Compaction summarizes
    /// old turns away: without this, a warning first reported in a summarized
    /// turn would keep being suppressed as already-seen when the model can no
    /// longer read it anywhere.
    pub fn forget_reported(&self) {
        self.ledger.clear();
    }

    /// Run every check whose glob matches an edited path (each command once).
    /// Returns a report block to append to the last tool result, or `None`
    /// when all checks pass (or none apply).
    pub async fn run(&self, edited_paths: &[String]) -> Option<String> {
        let mut commands: Vec<&str> = Vec::new();
        for (glob, cmd) in &self.rules {
            if !commands.contains(&cmd.as_str())
                && edited_paths.iter().any(|p| glob_matches(glob, p))
            {
                commands.push(cmd);
            }
        }
        let dedup = !crate::ablate::disabled_hook("ledger");
        let mut failures = String::new();
        for cmd in commands {
            crate::telemetry::add(&crate::telemetry::HOOK_POST_RUNS, 1);
            let (output, incomplete) = match run_check(cmd).await {
                CheckOutcome::Passed => continue,
                CheckOutcome::Failed(output) => (output, false),
                CheckOutcome::Incomplete(output) => (output, true),
            };
            crate::telemetry::add(&crate::telemetry::HOOK_POST_FAILURES, 1);
            if incomplete {
                failures.push_str(&format!(
                    "\n\n[system] post-edit check did not complete — this is verifier/infrastructure evidence, not a code diagnostic. Do not repeat the same command blindly; continue with a cheaper targeted check or the final oracle:\n$ {cmd}\n{output}"
                ));
                continue;
            }
            if !dedup {
                failures.push_str(&format!(
                    "\n\n[system] post-edit check failed — fix before proceeding:\n$ {cmd}\n{output}"
                ));
                continue;
            }
            let Reduced { fresh, repeated } = self.ledger.reduce(cmd, &output);
            failures.push_str(&match (fresh.is_empty(), repeated) {
                // Still red, nothing new: one line instead of re-billing the
                // whole output. Never silently drop the failure.
                (true, n) => format!(
                    "\n\n[system] post-edit check still failing — {n} diagnostic(s) already reported above, unchanged:\n$ {cmd}"
                ),
                (false, 0) => format!(
                    "\n\n[system] post-edit check failed — fix before proceeding:\n$ {cmd}\n{fresh}"
                ),
                (false, n) => format!(
                    "\n\n[system] post-edit check failed — fix before proceeding:\n$ {cmd}\n{fresh}\n(+{n} diagnostic(s) already reported above, unchanged)"
                ),
            });
        }
        (!failures.is_empty()).then_some(failures)
    }
}

/// A `pre_tool_use` gate hook: a shell command run before a tool executes,
/// selected when its `matcher` glob matches the tool name. The command receives
/// `{"tool":…,"input":…}` JSON on stdin, and its exit code is the verdict:
///
/// | exit | meaning |
/// |---|---|
/// | `0` | allow (and skip the LLM classifier) |
/// | `2` | deny — stdout/stderr becomes the reason shown to the model |
/// | `3` | ask — route through the interactive permission prompt |
/// | `4` | allow with a rewritten input — stdout is a JSON object merged into the tool input |
/// | `5` | short-circuit — stdout *is* the tool result; the tool never runs |
/// | other, spawn failure, timeout | no verdict, fall through to the normal permission path |
#[derive(Debug, Clone)]
pub struct PreHook {
    pub matcher: String,
    pub command: String,
}

/// Verdict of the [`Hooks::pre_tool_use`] gate.
pub enum PreVerdict {
    Allow,
    /// Route through the existing interactive permission prompt.
    Ask(String),
    Deny(String),
    /// Allow, after merging this JSON object into the tool input (`updatedInput`).
    AllowRewritten(serde_json::Value),
    /// Resolve the call without running the tool: this is its result.
    Short(String),
    /// No matching hook returned a decisive exit code.
    Pass,
}

/// A `tusk` filter: a shell command that sees a tool result before anything
/// else does and may rewrite or withhold it.
///
/// The gate hooks above decide *whether* a call runs; this one decides what the
/// rest of the system is allowed to see of what it produced. It is the only
/// place a secret that a tool printed can be removed before it reaches the
/// model's context, the session transcript, and the UI — all three are fed from
/// the single point where the filter runs.
///
/// The tool result arrives on stdin **verbatim**, not wrapped in JSON, so an
/// ordinary text filter is a valid hook (`sed`, `grep -v`, a redaction script)
/// and a chain of them composes exactly like a shell pipeline. The metadata a
/// filter might branch on comes through the environment instead:
/// `SIRBONE_TOOL`, `SIRBONE_TOOL_INPUT` (the tool input as JSON), and
/// `SIRBONE_IS_ERROR` (`0`/`1`).
///
/// The exit code is the verdict: `0` with empty stdout passes the result
/// through unchanged, `0` with output replaces it, `2` withholds it
/// (stdout/stderr becomes the reason), and **anything else — including a spawn
/// failure or a timeout — withholds it too**. A filter that fails open is not a
/// filter, so the failure mode here is deliberately the opposite of the
/// advisory hooks around it.
///
/// The practical consequence: a filter must exit `0`. `grep -v SECRET` exits `1`
/// when it selects no lines, so write it `grep -v SECRET || true`.
#[derive(Debug, Clone)]
pub struct TuskHook {
    pub matcher: String,
    pub command: String,
}

/// Outcome of the [`Hooks::tusk`] filter chain.
#[derive(Debug, PartialEq)]
pub enum TuskOutcome {
    Unchanged,
    /// Use this content in place of the tool result.
    Replaced(String),
    /// Do not surface the result at all; this reason takes its place, as an error.
    Withheld(String),
}

/// Deterministic shell hooks over the tool-call lifecycle (config key `hooks`).
/// One struct, three events — not a parallel system: `post` *is* the existing
/// [`PostEditChecks`].
#[derive(Debug, Clone, Default)]
pub struct Hooks {
    /// Built-in deterministic policies enabled through `hooks.presets`.
    pub presets: Vec<HookPreset>,
    /// Unrecognized names retained for diagnostics; they never execute.
    pub unknown_presets: Vec<String>,
    /// `pre_tool_use`: exit-code gate run before a tool, by tool-name glob.
    pub pre: Vec<PreHook>,
    /// `post_tool_use`: the advisory post-edit checks (also legacy `post_edit_check`).
    pub post: PostEditChecks,
    /// `tusk`: filters that may rewrite or withhold a tool result, by tool-name glob.
    pub tusk: Vec<TuskHook>,
    /// `stop`: commands consulted when the agent would finish; exit `2` forces
    /// another loop iteration (its output is fed back as the continuation reason).
    pub stop: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPreset {
    HighRisk,
}

impl HookPreset {
    pub const fn name(self) -> &'static str {
        match self {
            Self::HighRisk => "high_risk",
        }
    }
}

impl Hooks {
    /// Load the merged (`global` + per-project) `hooks` section. When that section
    /// has no `post_tool_use`, fall back to the legacy top-level `post_edit_check`.
    pub fn load() -> Self {
        let hooks = Self::from_section(crate::config::section("hooks").as_ref());
        arm_tusk(&hooks.tusk);
        hooks
    }

    /// The parsing half of [`Hooks::load`], separated so the documented config
    /// shapes are testable without a `HOME`. A typo here fails *open* — no hook
    /// parsed is no hook run — which is exactly the failure the runtime
    /// guarantees are written to avoid, so it is worth a test.
    pub(crate) fn from_section(section: Option<&serde_json::Value>) -> Self {
        let (presets, unknown_presets) = configured_presets(section);
        let pre = section
            .as_ref()
            .and_then(|v| v.get("pre_tool_use"))
            .map(|v| matched_hooks(v, |matcher, command| PreHook { matcher, command }))
            .unwrap_or_default();
        let tusk = section
            .as_ref()
            .and_then(|v| v.get("tusk"))
            .map(|v| matched_hooks(v, |matcher, command| TuskHook { matcher, command }))
            .unwrap_or_default();
        let stop = section
            .as_ref()
            .and_then(|v| v.get("stop"))
            .map(parse_command_list)
            .unwrap_or_default();
        let post = match section.as_ref().and_then(|v| v.get("post_tool_use")) {
            Some(v) => PostEditChecks::from_value(Some(v)),
            None => PostEditChecks::load(),
        };
        Self {
            presets,
            unknown_presets,
            pre: if crate::ablate::disabled_hook("pre") {
                Vec::new()
            } else {
                pre
            },
            post: if crate::ablate::disabled_hook("post") {
                PostEditChecks::default()
            } else {
                post
            },
            tusk: if crate::ablate::disabled_hook("tusk") {
                Vec::new()
            } else {
                tusk
            },
            stop: if crate::ablate::disabled_hook("stop") {
                Vec::new()
            } else {
                stop
            },
        }
    }

    /// Run `pre_tool_use` hooks matching `tool`. The first hook to return a
    /// decisive exit code (0 allow / 2 deny) wins; otherwise [`PreVerdict::Pass`].
    pub async fn pre_tool_use(&self, tool: &str, input: &serde_json::Value) -> PreVerdict {
        if self.presets.contains(&HookPreset::HighRisk) {
            if let Some(reason) = high_risk_reason(tool, input) {
                crate::telemetry::add(&crate::telemetry::HOOK_PRE_RUNS, 1);
                return PreVerdict::Ask(reason);
            }
        }
        if self.pre.is_empty() {
            return PreVerdict::Pass;
        }
        let payload = serde_json::json!({ "tool": tool, "input": input }).to_string();
        for h in &self.pre {
            if !glob_matches(&h.matcher, tool) {
                continue;
            }
            crate::telemetry::add(&crate::telemetry::HOOK_PRE_RUNS, 1);
            let Some(out) = run_hook_split(&h.command, &payload).await else {
                continue;
            };
            let reason = |verb: &str| {
                let merged = out.merged();
                if merged.is_empty() {
                    format!("{verb} by pre_tool_use hook: {}", h.command)
                } else {
                    merged
                }
            };
            match out.code {
                0 => return PreVerdict::Allow,
                2 => {
                    crate::telemetry::add(&crate::telemetry::HOOK_PRE_DENIES, 1);
                    return PreVerdict::Deny(reason("blocked"));
                }
                3 => return PreVerdict::Ask(reason("held for confirmation")),
                // A hook that meant to rewrite but emitted something unusable is
                // a config error, not a licence to run the original command: say
                // so instead of silently doing the thing it wanted changed.
                4 => {
                    return match serde_json::from_str::<serde_json::Value>(out.stdout.trim()) {
                        Ok(v) if v.is_object() => PreVerdict::AllowRewritten(v),
                        _ => PreVerdict::Deny(format!(
                        "pre_tool_use hook `{}` exited 4 (rewrite) but stdout is not a JSON object",
                        h.command
                    )),
                    }
                }
                5 => return PreVerdict::Short(out.stdout),
                _ => continue,
            }
        }
        PreVerdict::Pass
    }

    /// Run the `tusk` filter chain over one tool result: each matching hook sees
    /// what the previous one produced, and the first to withhold ends the chain.
    ///
    /// Runs on failed calls too — an error message quotes the command that
    /// produced it, so it is exactly as likely to carry a secret as a success is.
    pub async fn tusk(
        &self,
        tool: &str,
        input: &serde_json::Value,
        result: &str,
        is_error: bool,
    ) -> TuskOutcome {
        let mut current: Option<String> = None;
        for h in &self.tusk {
            if !glob_matches(&h.matcher, tool) {
                continue;
            }
            crate::telemetry::add(&crate::telemetry::TUSK_RUNS, 1);
            let env = [
                ("SIRBONE_TOOL", tool.to_string()),
                ("SIRBONE_TOOL_INPUT", input.to_string()),
                ("SIRBONE_IS_ERROR", u8::from(is_error).to_string()),
            ];
            let withheld = |reason: String| {
                crate::telemetry::add(&crate::telemetry::TUSK_WITHHELD, 1);
                TuskOutcome::Withheld(reason)
            };
            let Some(out) = run_hook_with_timeout(
                &h.command,
                current.as_deref().unwrap_or(result),
                &env,
                std::time::Duration::from_secs(TIMEOUT_SECS),
            )
            .await
            else {
                return withheld(format!("tusk filter `{}` did not run", h.command));
            };
            match out.code {
                0 if out.stdout.trim().is_empty() => {}
                0 => {
                    crate::telemetry::add(&crate::telemetry::TUSK_EDITS, 1);
                    current = Some(out.stdout);
                }
                2 => {
                    let merged = out.merged();
                    return withheld(if merged.is_empty() {
                        format!("result withheld by tusk filter: {}", h.command)
                    } else {
                        merged
                    });
                }
                code => {
                    return withheld(format!(
                        "tusk filter `{}` exited {code}; result withheld",
                        h.command
                    ))
                }
            }
        }
        current.map_or(TuskOutcome::Unchanged, TuskOutcome::Replaced)
    }

    /// Consult `stop` hooks when the agent would finish. `Some(reason)` (a hook
    /// exited `2`) means "not done" — the caller resumes the loop with `reason`
    /// as feedback. `None` lets the run terminate.
    pub async fn stop(&self) -> Option<String> {
        for cmd in &self.stop {
            crate::telemetry::add(&crate::telemetry::HOOK_STOP_RUNS, 1);
            if let Some((2, out)) = run_hook(cmd, "").await {
                crate::telemetry::add(&crate::telemetry::HOOK_STOP_RETRIES, 1);
                return Some(if out.is_empty() {
                    format!("stop hook requested another iteration: {cmd}")
                } else {
                    out
                });
            }
        }
        None
    }
}

/// Parse built-in hook preset names, separating unknown values for `doctor`.
pub fn configured_presets(section: Option<&serde_json::Value>) -> (Vec<HookPreset>, Vec<String>) {
    let mut known = Vec::new();
    let mut unknown = Vec::new();
    for name in section
        .and_then(|v| v.get("presets"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
    {
        match name {
            "high_risk" if !known.contains(&HookPreset::HighRisk) => {
                known.push(HookPreset::HighRisk);
            }
            "high_risk" => {}
            other if !unknown.iter().any(|value| value == other) => unknown.push(other.to_string()),
            _ => {}
        }
    }
    (known, unknown)
}

fn high_risk_reason(tool: &str, input: &serde_json::Value) -> Option<String> {
    let command = input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if tool == "bash" {
        const DEPENDENCY_COMMANDS: &[&str] = &[
            "cargo add",
            "cargo remove",
            "cargo update",
            "npm install",
            "npm uninstall",
            "npm update",
            "pnpm add",
            "pnpm remove",
            "pnpm update",
            "yarn add",
            "yarn remove",
            "yarn upgrade",
            "pip install",
            "pip uninstall",
            "uv add",
            "uv remove",
            "uv lock",
            "poetry add",
            "poetry remove",
            "poetry update",
            "go get",
            "go mod tidy",
            "bundle add",
            "bundle remove",
            "bundle update",
        ];
        const MIGRATION_COMMANDS: &[&str] = &[
            "alembic upgrade",
            "alembic downgrade",
            "diesel migration",
            "sqlx migrate",
            "prisma migrate",
            "prisma db push",
            "rails db:migrate",
            "rake db:migrate",
            "typeorm migration",
            "sequelize db:migrate",
            "knex migrate",
            "flyway migrate",
            "liquibase update",
            " migrate up",
            " migrate down",
            "migration apply",
            "migration reset",
        ];
        if DEPENDENCY_COMMANDS
            .iter()
            .any(|needle| command.contains(needle))
        {
            return Some("high_risk preset: dependency or lockfile operation".into());
        }
        if MIGRATION_COMMANDS
            .iter()
            .any(|needle| command.contains(needle))
        {
            return Some("high_risk preset: migration apply/reset operation".into());
        }
        let protected_name = [
            "cargo.toml",
            "cargo.lock",
            "package.json",
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            "pyproject.toml",
            "poetry.lock",
            "uv.lock",
            "requirements",
            "go.mod",
            "go.sum",
            "gemfile",
            "composer.json",
            "composer.lock",
            "pom.xml",
            "build.gradle",
            ".proto",
            ".graphql",
            ".gql",
            "openapi",
            "swagger",
            "/migrations/",
            "/migration/",
        ]
        .iter()
        .any(|name| command.contains(name));
        let mutating_command = [
            "sed -i", "perl -pi", "tee ", "cat >", " >", ">>", "cp ", "mv ", "rm ", "touch ",
        ]
        .iter()
        .any(|operator| command.contains(operator));
        if protected_name && mutating_command {
            return Some("high_risk preset: manifest, schema, or migration file change".into());
        }

        // A model can otherwise route around an Ask returned by `write`/`edit`
        // by emitting the same source through a shell heredoc. Keep this in the
        // existing preset and recognize only explicit source-file mutations
        // that introduce a conventional public marker.
        let source_target = [".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".java", ".kt"]
            .iter()
            .any(|extension| command.contains(extension));
        let public_marker = [
            "pub fn ",
            "pub struct ",
            "pub enum ",
            "pub trait ",
            "pub type ",
            "pub mod ",
            "pub use ",
            "export ",
            "export default ",
            "__all__",
            "public class ",
            "public interface ",
            "public fun ",
        ]
        .iter()
        .any(|marker| command.contains(marker));
        let python_public_function = command.lines().any(|line| {
            let line = line.trim_start();
            line.strip_prefix("def ")
                .is_some_and(|name| !name.starts_with('_'))
        });
        if source_target && mutating_command && (public_marker || python_public_function) {
            return Some("high_risk preset: recognizable public API surface change".into());
        }
    }

    let path = input
        .get("path")
        .or_else(|| input.get("file_path"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let basename = normalized.rsplit('/').next().unwrap_or_default();
    let manifest = matches!(
        basename,
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "pyproject.toml"
            | "poetry.lock"
            | "uv.lock"
            | "requirements.txt"
            | "go.mod"
            | "go.sum"
            | "gemfile"
            | "gemfile.lock"
            | "composer.json"
            | "composer.lock"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "gradle.lockfile"
            | "mix.exs"
            | "mix.lock"
    );
    let manifest = manifest || (basename.starts_with("requirements") && basename.ends_with(".txt"));
    let schema = basename.ends_with(".proto")
        || basename.ends_with(".graphql")
        || basename.ends_with(".gql")
        || basename.contains("openapi")
        || basename.contains("swagger")
        || matches!(basename, "schema.json" | "schema.yaml" | "schema.yml")
        || normalized
            .split('/')
            .any(|part| matches!(part, "migrations" | "migration"));
    if manifest {
        return Some("high_risk preset: dependency manifest or lockfile change".into());
    }
    if schema {
        return Some("high_risk preset: schema or migration file change".into());
    }

    let changes = ["old_string", "new_string", "content"]
        .iter()
        .filter_map(|key| input.get(key).and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let source_path = [".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".java", ".kt"]
        .iter()
        .any(|extension| normalized.ends_with(extension));
    let public_marker = [
        "pub fn ",
        "pub struct ",
        "pub enum ",
        "pub trait ",
        "pub type ",
        "pub mod ",
        "pub use ",
        "export ",
        "export default ",
        "__all__",
        "public class ",
        "public interface ",
        "public fun ",
    ]
    .iter()
    .any(|marker| changes.contains(marker));
    (source_path && public_marker)
        .then(|| "high_risk preset: recognizable public API surface change".into())
}

/// Parse `[{ "match": glob, "command": cmd }, …]`; entries without a `command`
/// are skipped. `match` defaults to `"*"` (all tools) when absent.
fn matched_hooks<T>(v: &serde_json::Value, make: impl Fn(String, String) -> T) -> Vec<T> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let command = e.get("command")?.as_str()?.to_string();
                    let matcher = e
                        .get("match")
                        .and_then(|m| m.as_str())
                        .unwrap_or("*")
                        .to_string();
                    Some(make(matcher, command))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether this process has any `tusk` filter configured.
///
/// Read by [`crate::tools::spill`]: truncation spills the *whole* pre-filter
/// output to a file from inside the tool, before a filter could ever see it, so
/// a redaction filter and an on-disk copy of what it redacts cannot both exist.
/// Arming tusk turns spilling off for the run.
static TUSK_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn tusk_armed() -> bool {
    TUSK_ARMED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Called by [`Hooks::load`] only, never by the parser: arming is a global
/// side effect, and a parse must stay free of it so it can be tested without
/// switching spilling off for every other test in the process.
fn arm_tusk(hooks: &[TuskHook]) {
    TUSK_ARMED.store(!hooks.is_empty(), std::sync::atomic::Ordering::Relaxed);
}

/// Parse `[{ "command": cmd }, …]` (or a bare `[cmd, …]`) into command strings.
fn parse_command_list(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    e.as_str()
                        .map(String::from)
                        .or_else(|| e.get("command").and_then(|c| c.as_str()).map(String::from))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A finished hook process. `stdout` is kept verbatim because for a rewrite or
/// a filter it *is* the payload; the reason strings that callers show the model
/// come from [`HookOutput::merged`] instead.
struct HookOutput {
    code: i32,
    stdout: String,
    stderr: String,
}

impl HookOutput {
    /// Trimmed stdout and stderr joined — the human-readable form used for a
    /// denial reason or a withholding reason.
    fn merged(&self) -> String {
        [self.stdout.trim(), self.stderr.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Run a hook command feeding `stdin`, returning `(exit_code, combined_output)`.
/// `None` on spawn failure or timeout (treated as "no verdict" by callers).
async fn run_hook(cmd: &str, stdin: &str) -> Option<(i32, String)> {
    let out = run_hook_split(cmd, stdin).await?;
    Some((out.code, out.merged()))
}

async fn run_hook_split(cmd: &str, stdin: &str) -> Option<HookOutput> {
    run_hook_with_timeout(
        cmd,
        stdin,
        &[],
        std::time::Duration::from_secs(TIMEOUT_SECS),
    )
    .await
}

async fn run_hook_with_timeout(
    cmd: &str,
    stdin: &str,
    env: &[(&str, String)],
    timeout: std::time::Duration,
) -> Option<HookOutput> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    if let Some(mut si) = child.stdin.take() {
        si.write_all(stdin.as_bytes()).await.ok();
        drop(si);
    }
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    Some(HookOutput {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Run one check command, distinguishing a red verdict from missing evidence.
async fn run_check(cmd: &str) -> CheckOutcome {
    run_check_with_timeout(cmd, std::time::Duration::from_secs(TIMEOUT_SECS)).await
}

async fn run_check_with_timeout(cmd: &str, timeout: std::time::Duration) -> CheckOutcome {
    let fut = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output();
    let out = match tokio::time::timeout(timeout, fut).await {
        Err(_) => {
            return CheckOutcome::Incomplete(format!(
                "(timed out after {}s)",
                timeout.as_secs_f64()
            ))
        }
        Ok(Err(e)) => return CheckOutcome::Incomplete(format!("(failed to spawn: {e})")),
        Ok(Ok(out)) => out,
    };
    if out.status.success() {
        return CheckOutcome::Passed;
    }
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err.trim());
    }
    if text.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… [check output truncated]");
    }
    CheckOutcome::Failed(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(json: &str) -> PostEditChecks {
        let v: serde_json::Value = serde_json::from_str(json).expect("test json");
        PostEditChecks::from_value(Some(&v))
    }

    #[test]
    fn identity_strips_the_location_prefix() {
        assert_eq!(
            diagnostic_identity("src/foo.rs:12:5: error[E0308]: mismatched types"),
            "error[E0308]: mismatched types"
        );
        // Same message after an edit shifted it down: identical identity.
        assert_eq!(
            diagnostic_identity("src/foo.rs:99:5: error[E0308]: mismatched types"),
            diagnostic_identity("src/foo.rs:12:5: error[E0308]: mismatched types")
        );
        // No location => the line is its own identity.
        assert_eq!(
            diagnostic_identity("error: could not compile `sirbone`"),
            "error: could not compile `sirbone`"
        );
    }

    #[test]
    fn ledger_emits_only_the_delta() {
        let led = DiagnosticsLedger::default();
        let first = led.reduce(
            "check",
            "a.rs:1:1: warning: unused\nb.rs:2:1: warning: dead",
        );
        assert_eq!(first.repeated, 0);
        assert_eq!(first.fresh.lines().count(), 2);

        // Same two diagnostics, one moved, plus a new one.
        let second = led.reduce(
            "check",
            "a.rs:1:1: warning: unused\nb.rs:9:1: warning: dead\nc.rs:3:1: error: boom",
        );
        assert_eq!(second.repeated, 2);
        assert_eq!(second.fresh, "c.rs:3:1: error: boom");

        // Nothing new at all.
        let third = led.reduce("check", "a.rs:1:1: warning: unused");
        assert!(third.fresh.is_empty());
        assert_eq!(third.repeated, 1);
    }

    #[test]
    fn forgetting_makes_a_diagnostic_reportable_again() {
        // Compaction removes the turns the diagnostic was reported in, so it has
        // to be reported again rather than suppressed as already-seen.
        let c = PostEditChecks::new([("*.rs".to_string(), "check".to_string())]);
        let out = "a.rs:1:1: warning: unused";
        assert_eq!(c.ledger.reduce("check", out).repeated, 0);
        assert_eq!(c.ledger.reduce("check", out).repeated, 1);
        c.forget_reported();
        assert_eq!(c.ledger.reduce("check", out).repeated, 0);
    }

    #[test]
    fn ledger_is_keyed_per_command() {
        let led = DiagnosticsLedger::default();
        led.reduce("cargo check", "a.rs:1:1: warning: unused");
        let other = led.reduce("ruff", "a.rs:1:1: warning: unused");
        assert_eq!(other.repeated, 0, "another command has its own memory");
    }

    #[tokio::test]
    async fn repeated_failure_collapses_but_stays_visible() {
        let c = checks(r#"{"*.rs": "echo 'a.rs:1:1: error: boom' >&2; exit 1"}"#);
        let paths = vec!["x.rs".to_string()];
        let first = c.run(&paths).await.expect("check fails");
        assert!(first.contains("error: boom"), "{first}");

        let second = c
            .run(&paths)
            .await
            .expect("still failing => still reported");
        assert!(second.contains("still failing"), "{second}");
        assert!(
            second.lines().count() < first.lines().count(),
            "diagnostic not re-billed: {second}"
        );
    }

    #[test]
    fn parses_config_and_handles_garbage() {
        let c = checks(r#"{"*.rs": "cargo check -q", "*.py": "ruff check -q"}"#);
        assert_eq!(c.rules.len(), 2);
        assert!(PostEditChecks::from_value(None).is_empty());
        assert!(
            checks(r#"{"*.rs": 42}"#).is_empty(),
            "non-string command skipped"
        );
    }

    #[tokio::test]
    async fn failing_check_reports_and_passing_is_silent() {
        let c = checks(r#"{"*.rs": "echo boom >&2; exit 1", "*.py": "true"}"#);
        let report = c.run(&["src/foo.rs".into()]).await.expect("must fail");
        assert!(report.contains("post-edit check failed"));
        assert!(report.contains("boom"));
        assert!(
            c.run(&["a.py".into()]).await.is_none(),
            "passing check is silent"
        );
        assert!(c.run(&["a.go".into()]).await.is_none(), "no matching glob");
    }

    #[tokio::test]
    async fn timed_out_post_check_is_incomplete_not_a_code_failure() {
        let outcome = run_check_with_timeout("sleep 1", std::time::Duration::from_millis(10)).await;
        match outcome {
            CheckOutcome::Incomplete(message) => assert!(message.contains("timed out")),
            CheckOutcome::Passed => panic!("timeout cannot pass"),
            CheckOutcome::Failed(_) => panic!("timeout is not a code verdict"),
        }
    }

    #[tokio::test]
    async fn same_command_runs_once_for_many_files() {
        // Two globs, same command: a batch touching both kinds must not
        // duplicate the report.
        let c = checks(r#"{"*.rs": "echo dup; exit 1", "src/*": "echo dup; exit 1"}"#);
        let report = c
            .run(&["src/a.rs".into(), "src/b.rs".into()])
            .await
            .expect("fails");
        assert_eq!(report.matches("post-edit check failed").count(), 1);
    }

    fn pre(json: &str) -> Vec<PreHook> {
        pre_value(&serde_json::from_str(json).expect("test json"))
    }

    fn pre_value(v: &serde_json::Value) -> Vec<PreHook> {
        matched_hooks(v, |matcher, command| PreHook { matcher, command })
    }

    #[test]
    fn parse_pre_hooks_defaults_match_to_star_and_skips_incomplete() {
        let hs = pre(r#"[{"match":"bash","command":"a"},{"command":"b"},{"match":"x"}]"#);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].matcher, "bash");
        assert_eq!(hs[1].matcher, "*", "missing match defaults to all tools");
    }

    #[test]
    fn parse_command_list_accepts_bare_and_object() {
        let v = serde_json::json!(["a", {"command": "b"}, {"nope": 1}]);
        assert_eq!(
            parse_command_list(&v),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn malformed_hook_config_is_ignored_safely() {
        assert!(pre_value(&serde_json::json!({"bad": true})).is_empty());
        assert!(pre_value(&serde_json::json!([
            {"match": "bash"},
            {"command": 1},
            "nope"
        ]))
        .is_empty());
        assert!(parse_command_list(&serde_json::json!({"bad": true})).is_empty());
    }

    #[tokio::test]
    async fn pre_gate_allows_denies_and_passes() {
        let allow = Hooks {
            pre: pre(r#"[{"match":"bash","command":"exit 0"}]"#),
            ..Default::default()
        };
        assert!(matches!(
            allow.pre_tool_use("bash", &serde_json::json!({})).await,
            PreVerdict::Allow
        ));

        let deny = Hooks {
            pre: pre(r#"[{"match":"bash","command":"echo nope >&2; exit 2"}]"#),
            ..Default::default()
        };
        match deny.pre_tool_use("bash", &serde_json::json!({})).await {
            PreVerdict::Deny(r) => assert!(r.contains("nope"), "{r}"),
            _ => panic!("expected deny"),
        }

        // Non-matching tool and non-decisive exit both fall through to Pass.
        let miss = Hooks {
            pre: pre(r#"[{"match":"bash","command":"exit 0"}]"#),
            ..Default::default()
        };
        assert!(matches!(
            miss.pre_tool_use("read", &serde_json::json!({})).await,
            PreVerdict::Pass
        ));
        let other = Hooks {
            pre: pre(r#"[{"match":"bash","command":"exit 1"}]"#),
            ..Default::default()
        };
        assert!(matches!(
            other.pre_tool_use("bash", &serde_json::json!({})).await,
            PreVerdict::Pass
        ));

        // No hooks configured: cheap Pass without spawning anything.
        assert!(matches!(
            Hooks::default()
                .pre_tool_use("bash", &serde_json::json!({}))
                .await,
            PreVerdict::Pass
        ));
    }

    #[test]
    fn preset_config_separates_known_unknown_and_deduplicates() {
        let section = serde_json::json!({
            "presets": ["high_risk", "future_policy", "high_risk", "future_policy", 7]
        });
        let (known, unknown) = configured_presets(Some(&section));
        assert_eq!(known, vec![HookPreset::HighRisk]);
        assert_eq!(unknown, vec!["future_policy"]);
        assert_eq!(configured_presets(None), (Vec::new(), Vec::new()));
    }

    #[tokio::test]
    async fn high_risk_preset_asks_once_for_risky_operations() {
        let hooks = Hooks {
            presets: vec![HookPreset::HighRisk],
            pre: pre(r#"[{"match":"*","command":"exit 0"}]"#),
            ..Default::default()
        };
        let risky = [
            ("bash", serde_json::json!({"command": "cargo add serde"})),
            (
                "bash",
                serde_json::json!({"command": "alembic upgrade head"}),
            ),
            (
                "edit",
                serde_json::json!({"path": "Cargo.toml", "old_string": "a", "new_string": "b"}),
            ),
            (
                "write",
                serde_json::json!({"path": "api/openapi.yaml", "content": "openapi: 3"}),
            ),
            (
                "edit",
                serde_json::json!({"path": "migrations/001.sql", "old_string": "a", "new_string": "b"}),
            ),
            (
                "edit",
                serde_json::json!({"path": "src/lib.rs", "old_string": "fn x() {}", "new_string": "pub fn x() {}"}),
            ),
            (
                "bash",
                serde_json::json!({
                    "command": "cat > notesdb/export.py <<'PY'\ndef export_json(src, dst):\n    pass\nPY"
                }),
            ),
        ];
        for (tool, input) in risky {
            assert!(
                matches!(hooks.pre_tool_use(tool, &input).await, PreVerdict::Ask(_)),
                "expected Ask for {tool} {input}"
            );
        }
    }

    #[tokio::test]
    async fn high_risk_preset_is_silent_for_safe_controls() {
        let hooks = Hooks {
            presets: vec![HookPreset::HighRisk],
            ..Default::default()
        };
        let safe = [
            ("bash", serde_json::json!({"command": "cargo test"})),
            ("bash", serde_json::json!({"command": "git diff --check"})),
            (
                "edit",
                serde_json::json!({"path": "src/internal.rs", "old_string": "let a = 1", "new_string": "let a = 2"}),
            ),
            (
                "write",
                serde_json::json!({"path": "README.md", "content": "docs"}),
            ),
            (
                "bash",
                serde_json::json!({
                    "command": "cat > notesdb/internal.py <<'PY'\ndef _normalize(value):\n    return value\nPY"
                }),
            ),
        ];
        for (tool, input) in safe {
            assert!(
                matches!(hooks.pre_tool_use(tool, &input).await, PreVerdict::Pass),
                "unexpected prompt for {tool} {input}"
            );
        }
    }

    #[tokio::test]
    async fn pre_gate_receives_tool_and_input_on_stdin() {
        let h = Hooks {
            pre: pre(r#"[{"match":"*","command":"grep -q '\"tool\":\"bash\"' && exit 2"}]"#),
            ..Default::default()
        };
        assert!(matches!(
            h.pre_tool_use("bash", &serde_json::json!({"command":"ls"}))
                .await,
            PreVerdict::Deny(_)
        ));
    }

    #[tokio::test]
    async fn hook_timeout_returns_no_verdict() {
        let out = run_hook_with_timeout(
            "sleep 1; echo late; exit 2",
            "",
            &[],
            std::time::Duration::from_millis(10),
        )
        .await;
        assert!(out.is_none(), "timed-out hooks must not hang or deny");
    }

    #[tokio::test]
    async fn stop_hook_forces_continuation_on_exit_2() {
        let cont = Hooks {
            stop: vec!["echo keep going; exit 2".into()],
            ..Default::default()
        };
        assert_eq!(cont.stop().await.as_deref(), Some("keep going"));
        let done = Hooks {
            stop: vec!["exit 0".into()],
            ..Default::default()
        };
        assert!(done.stop().await.is_none());
        assert!(Hooks::default().stop().await.is_none());
    }

    fn tusks(json: &str) -> Vec<TuskHook> {
        matched_hooks(
            &serde_json::from_str(json).expect("test json"),
            |matcher, command| TuskHook { matcher, command },
        )
    }

    /// The exact `hooks` block the configuration page tells users to write.
    /// A silent parse failure here would be the worst kind: no filter parsed is
    /// no filter run, so the user would believe they were redacting and not be.
    #[test]
    fn the_documented_hooks_block_parses() {
        let hooks = Hooks::from_section(Some(&serde_json::json!({
            "presets": ["high_risk"],
            "pre_tool_use": [{"match": "bash", "command": "./policy-check"}],
            "post_tool_use": {"*.py": "python -m ruff check ."},
            "tusk": [{"match": "*", "command": "~/.sirbone/redact-secrets"}],
            "stop": ["./completion-invariant"]
        })));
        assert_eq!(hooks.presets, vec![HookPreset::HighRisk]);
        assert_eq!(hooks.pre.len(), 1);
        assert_eq!(hooks.stop, vec!["./completion-invariant".to_string()]);
        assert!(!hooks.post.is_empty());
        assert_eq!(hooks.tusk.len(), 1, "the documented tusk block must parse");
        assert_eq!(hooks.tusk[0].matcher, "*");
        assert_eq!(hooks.tusk[0].command, "~/.sirbone/redact-secrets");
    }

    /// The single-key form the page shows on its own, and the promise that a
    /// filter path with a `~` is expanded (hooks run through `sh -c`).
    #[tokio::test]
    async fn a_tusk_filter_path_goes_through_the_shell() {
        let hooks = Hooks::from_section(Some(&serde_json::json!({
            "tusk": [{"match": "*", "command": "echo $HOME"}]
        })));
        let home = std::env::var("HOME").expect("HOME set in tests");
        assert_eq!(
            hooks.tusk("bash", &serde_json::json!({}), "x", false).await,
            TuskOutcome::Replaced(format!("{home}\n"))
        );
    }

    #[tokio::test]
    async fn pre_gate_asks_rewrites_and_short_circuits() {
        let ask = Hooks {
            pre: pre(r#"[{"match":"bash","command":"echo confirm first; exit 3"}]"#),
            ..Default::default()
        };
        match ask.pre_tool_use("bash", &serde_json::json!({})).await {
            PreVerdict::Ask(r) => assert_eq!(r, "confirm first"),
            _ => panic!("expected ask"),
        }

        let rewrite = Hooks {
            pre: pre(r#"[{"match":"bash","command":"echo '{\"command\":\"pnpm i\"}'; exit 4"}]"#),
            ..Default::default()
        };
        match rewrite
            .pre_tool_use("bash", &serde_json::json!({"command": "npm i"}))
            .await
        {
            PreVerdict::AllowRewritten(v) => assert_eq!(v["command"], "pnpm i"),
            _ => panic!("expected rewrite"),
        }

        // A rewrite that produced garbage must not fall back to running the
        // original command the hook was trying to change.
        let broken = Hooks {
            pre: pre(r#"[{"match":"bash","command":"echo not-json; exit 4"}]"#),
            ..Default::default()
        };
        match broken.pre_tool_use("bash", &serde_json::json!({})).await {
            PreVerdict::Deny(r) => assert!(r.contains("not a JSON object"), "{r}"),
            _ => panic!("expected deny on unusable rewrite"),
        }

        let short = Hooks {
            pre: pre(r#"[{"match":"web_search","command":"echo cached answer; exit 5"}]"#),
            ..Default::default()
        };
        match short
            .pre_tool_use("web_search", &serde_json::json!({}))
            .await
        {
            PreVerdict::Short(r) => assert_eq!(r.trim(), "cached answer"),
            _ => panic!("expected short-circuit"),
        }
    }

    #[tokio::test]
    async fn tusk_passes_through_transforms_and_chains() {
        // Exit 0 with no output leaves the result exactly as produced.
        let noop = Hooks {
            tusk: tusks(r#"[{"match":"*","command":"cat >/dev/null"}]"#),
            ..Default::default()
        };
        assert_eq!(
            noop.tusk("bash", &serde_json::json!({}), "secret=abc", false)
                .await,
            TuskOutcome::Unchanged
        );

        // The result arrives raw on stdin, so a plain text filter is a valid
        // hook and its stdout replaces the result.
        let redact = Hooks {
            tusk: tusks(r#"[{"match":"bash","command":"sed s/abc/[redacted]/"}]"#),
            ..Default::default()
        };
        assert_eq!(
            redact
                .tusk("bash", &serde_json::json!({}), "secret=abc", false)
                .await,
            TuskOutcome::Replaced("secret=[redacted]".into())
        );

        // Metadata a filter might branch on comes through the environment.
        let meta = Hooks {
            tusk: tusks(
                r#"[{"match":"*","command":"echo \"$SIRBONE_TOOL $SIRBONE_IS_ERROR $SIRBONE_TOOL_INPUT\""}]"#,
            ),
            ..Default::default()
        };
        assert_eq!(
            meta.tusk("bash", &serde_json::json!({"command": "ls"}), "x", true)
                .await,
            TuskOutcome::Replaced("bash 1 {\"command\":\"ls\"}\n".into())
        );

        // Each filter sees what the previous one produced.
        let chain = Hooks {
            tusk: tusks(
                r#"[{"match":"*","command":"echo one"},{"match":"*","command":"grep -q one && echo two"}]"#,
            ),
            ..Default::default()
        };
        assert_eq!(
            chain
                .tusk("bash", &serde_json::json!({}), "zero", false)
                .await,
            TuskOutcome::Replaced("two\n".into())
        );

        // A non-matching filter never runs.
        assert_eq!(
            redact
                .tusk("read", &serde_json::json!({}), "secret=abc", false)
                .await,
            TuskOutcome::Unchanged
        );
        assert_eq!(
            Hooks::default()
                .tusk("bash", &serde_json::json!({}), "x", false)
                .await,
            TuskOutcome::Unchanged
        );
    }

    #[tokio::test]
    async fn tusk_fails_closed() {
        // Exit 2 is the explicit withhold, with the hook's output as the reason.
        let refuse = Hooks {
            tusk: tusks(r#"[{"match":"*","command":"echo contains a key >&2; exit 2"}]"#),
            ..Default::default()
        };
        assert_eq!(
            refuse
                .tusk("bash", &serde_json::json!({}), "sk-live-1", false)
                .await,
            TuskOutcome::Withheld("contains a key".into())
        );

        // A broken filter withholds too: a filter that fails open is not a filter.
        let broken = Hooks {
            tusk: tusks(r#"[{"match":"*","command":"exit 7"}]"#),
            ..Default::default()
        };
        match broken
            .tusk("bash", &serde_json::json!({}), "sk-live-1", false)
            .await
        {
            TuskOutcome::Withheld(r) => {
                assert!(r.contains("exited 7"), "{r}");
                assert!(!r.contains("sk-live-1"), "reason must not leak the result");
            }
            other => panic!("expected withhold, got {other:?}"),
        }

        // The chain stops at the first withhold: the later filter never runs.
        let stops = Hooks {
            tusk: tusks(
                r#"[{"match":"*","command":"exit 2"},{"match":"*","command":"echo late"}]"#,
            ),
            ..Default::default()
        };
        match stops.tusk("bash", &serde_json::json!({}), "x", false).await {
            TuskOutcome::Withheld(r) => assert!(!r.contains("late"), "{r}"),
            other => panic!("expected withhold, got {other:?}"),
        }
    }
}
