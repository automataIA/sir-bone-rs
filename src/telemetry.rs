//! Process-wide feature-attribution counters for the bench `[usage]` line.
//!
//! Plain atomics instead of event plumbing: increments happen deep inside
//! features (compaction, historia, hooks) and are only *read* when
//! `SIRBONE_USAGE=1` prints the usage line — threading them through
//! `AgentEvent` would touch every frontend for a bench-only metric. Counters
//! are cumulative for the process; the bench runs one task per process, so
//! per-run and per-process are the same there.

use std::sync::atomic::{AtomicU64, Ordering};

/// Successful context compactions (the summarize-and-replace path ran).
pub static COMPACTION_FIRED: AtomicU64 = AtomicU64::new(0);
/// Legacy model-authored `historia` appends. Kept in persisted telemetry for
/// backward compatibility; the deterministic history tool never increments it.
pub static HISTORIA_WRITES: AtomicU64 = AtomicU64::new(0);
/// Structured `historia` queries that returned at least one matching session.
pub static HISTORIA_HITS: AtomicU64 = AtomicU64::new(0);
/// Chars of the assembled system prompt. The prompt-ablation A/B compares this
/// between arms: without it a `prompt:*` run shows a token delta with no way to
/// tell how much came from the prompt itself vs. the trajectory it produced.
pub static SYSTEM_PROMPT_CHARS: AtomicU64 = AtomicU64::new(0);
pub static HOOK_PRE_RUNS: AtomicU64 = AtomicU64::new(0);
pub static HOOK_PRE_DENIES: AtomicU64 = AtomicU64::new(0);
pub static HOOK_POST_RUNS: AtomicU64 = AtomicU64::new(0);
pub static HOOK_POST_FAILURES: AtomicU64 = AtomicU64::new(0);
/// `tusk` filter invocations over a tool result (one per matching hook).
pub static TUSK_RUNS: AtomicU64 = AtomicU64::new(0);
/// `tusk` invocations that rewrote the result. Non-zero is the only proof the
/// filter is actually reaching content rather than sitting idle behind a glob
/// that never matches.
pub static TUSK_EDITS: AtomicU64 = AtomicU64::new(0);
/// `tusk` invocations that withheld the result — a deliberate exit 2, or the
/// fail-closed path (spawn failure, timeout, unexpected exit code).
pub static TUSK_WITHHELD: AtomicU64 = AtomicU64::new(0);
pub static HOOK_STOP_RUNS: AtomicU64 = AtomicU64::new(0);
pub static HOOK_STOP_RETRIES: AtomicU64 = AtomicU64::new(0);
pub static HOOK_STOP_EXHAUSTED: AtomicU64 = AtomicU64::new(0);
pub static ORACLE_RUNS: AtomicU64 = AtomicU64::new(0);
pub static ORACLE_FAILURES: AtomicU64 = AtomicU64::new(0);
pub static ORACLE_RETRIES: AtomicU64 = AtomicU64::new(0);
pub static ORACLE_ROLLBACKS: AtomicU64 = AtomicU64::new(0);
pub static ORACLE_EXHAUSTED: AtomicU64 = AtomicU64::new(0);
pub static ASK_USER_ROUNDS: AtomicU64 = AtomicU64::new(0);
pub static ASK_USER_QUESTIONS: AtomicU64 = AtomicU64::new(0);
/// One-shot `/verify` or model-facing `verify` invocations.
pub static VERIFY_TOOL_RUNS: AtomicU64 = AtomicU64::new(0);
/// Tool outputs too large for the context that were written to a recoverable
/// spill file. Zero means truncation never happened, so the mechanism was never
/// exercised — not the same as "it was exercised and did not help", which is
/// exactly the distinction a bench null needs to make.
pub static SPILL_WRITES: AtomicU64 = AtomicU64::new(0);
/// `read` calls answered with a structural outline instead of the whole file.
///
/// Counts the localization pre-pass too: it runs its own agent loop over
/// [`crate::tools::read_only_registry`], which holds the same `read` tool. So
/// this can exceed the `read` calls visible in the session transcript — a live
/// check on 2026-08-11 recorded 2 outlines for 1 transcript read. Non-zero is
/// still exactly what the bench's mechanism check asks; only the magnitude
/// spans both passes.
pub static READ_OUTLINES: AtomicU64 = AtomicU64::new(0);
/// `patch` calls that reached the filesystem.
pub static PATCH_APPLIES: AtomicU64 = AtomicU64::new(0);
/// `patch` calls refused before touching the filesystem: grammar error, stale
/// tag, out-of-range address. This is where the hashline format's real cost
/// shows up — output tokens saved by not re-quoting are given straight back by
/// a round trip the model spends rewriting a patch it got wrong.
pub static PATCH_REJECTS: AtomicU64 = AtomicU64::new(0);
/// Turns aborted mid-stream by a stream rule and restarted.
pub static STREAM_RULE_TRIPS: AtomicU64 = AtomicU64::new(0);
/// Deterministic Plan-mode contracts created at task start.
pub static PLAN_CONTRACT_INITIALIZED: AtomicU64 = AtomicU64::new(0);
/// Model-authored updates accepted while preserving required sections.
pub static PLAN_CONTRACT_UPDATED: AtomicU64 = AtomicU64::new(0);
/// Workspace mutations refused because a Plan contract was incomplete.
pub static PLAN_MUTATIONS_BLOCKED: AtomicU64 = AtomicU64::new(0);
/// Assistant turns that carried at least one tool call.
pub static TOOL_BATCHES: AtomicU64 = AtomicU64::new(0);
/// Tool calls across those turns. Divided by [`TOOL_BATCHES`] this is the mean
/// batch width: how many calls the model puts in one message. The executor
/// already runs a batch's non-conflicting calls in parallel lanes
/// (`crate::agent::plan_lanes`), so a mean near 1.0 says the parallelism exists
/// and goes unused — a prompt property, not an engine one. Counted before the
/// permission pass, so denials do not hide what the model intended to fan out.
pub static TOOL_CALLS_EMITTED: AtomicU64 = AtomicU64::new(0);

/// Tool calls the permission pipeline refused, by who refused them. Three
/// different products behind one word: a `soft_deny`/`is_destructive`/classifier
/// block says the policy is doing its job (or is too strict), a user denial says
/// the model proposed something the human did not want, and an unattended denial
/// is pure headless friction — the same call would have been allowed with someone
/// there to answer. The transcript already carries the reason as text for the
/// model; these count it for the audit, which is the only telemetry the
/// supervision pillar has.
pub static PERMISSION_DENIES_POLICY: AtomicU64 = AtomicU64::new(0);
pub static PERMISSION_DENIES_USER: AtomicU64 = AtomicU64::new(0);
pub static PERMISSION_DENIES_UNATTENDED: AtomicU64 = AtomicU64::new(0);

/// Bench parity invariants, both zero in a normal build.
///
/// `PERMISSION_BYPASSED` counts calls that skipped the permission pass entirely
/// (`--features bench_bypass`); `TOOL_CALLS_DISPATCHED` counts calls that
/// actually reached the executor. They exist because neither existing counter
/// can prove the bypass: `TOOL_CALLS_EMITTED` is incremented before the pass,
/// and the CLI's `tool_calls` counts `ToolCallEnd` events, which blocked calls
/// also emit. `bypassed == dispatched` with all three denial counters at zero is
/// the evidence that a bench arm ran with no gate at all — the claim the
/// comparison rests on, so it is measured rather than assumed. `emitted` is only
/// a lower bound on those two, never an equal: it is incremented in the main
/// loop alone, so the localization pre-pass — which drives `run_tools` itself —
/// contributes to bypassed/dispatched without passing through it. Deliberately
/// outside `run_delta`/the session audit: they are a property of the *build*,
/// read off the `[usage]` line, not per-run history worth persisting.
pub static PERMISSION_BYPASSED: AtomicU64 = AtomicU64::new(0);
pub static TOOL_CALLS_DISPATCHED: AtomicU64 = AtomicU64::new(0);

/// Runs the completion check pulled back for another iteration, because the
/// model's own step list was unfinished when the loop tried to end
/// (`SIRBONE_COMPLETION_CHECK`). Capped at one per run, so this is also the
/// count of runs that would otherwise have stopped mid-plan.
pub static COMPLETION_CHECKS_FIRED: AtomicU64 = AtomicU64::new(0);

/// Successful tool calls that wrote to a file recognized as a test.
///
/// Editing tests is ordinary work — this is not a violation counter and nothing
/// blocks on it. It exists because the number is only meaningful *next to a
/// claim*: a run that reports fixing the source while its only writes landed in
/// `tests/` is the shape reward hacking takes, and the harness cannot correlate
/// what it never recorded. Counts calls, not distinct files, and sees only the
/// native file tools — a write performed through `bash` is invisible here.
pub static TEST_FILE_MUTATIONS: AtomicU64 = AtomicU64::new(0);

/// Agent runs started under best-of-K selection (`SIRBONE_BEST_OF`).
///
/// The first attempt counts too, so the number is the run count outright: `1`
/// means the feature was on and one attempt sufficed, `0` means it was off.
/// Test-time scaling buys resolution rate with calls and tokens, and the honest
/// gate is cost per *stably resolved* task — a ratio nobody can compute without
/// knowing how many runs went into the answer.
pub static BEST_OF_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

/// Times a later attempt won the selection, i.e. left strictly fewer failing
/// tests than every attempt before it.
///
/// Zero selections with a nonzero attempt count is the result that matters most:
/// it means the extra runs were paid for and bought nothing.
pub static BEST_OF_SELECTIONS: AtomicU64 = AtomicU64::new(0);

pub fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

pub fn get(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

/// System-prompt size in tokens, with the same ~4 chars/token heuristic as
/// [`crate::agent::estimate_context_tokens`].
pub fn system_prompt_tokens() -> u64 {
    get(&SYSTEM_PROMPT_CHARS) / 4
}

/// What a single run contributed, as persisted by
/// [`crate::session::append_run_telemetry`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunCounters {
    pub compaction_fired: u64,
    pub historia_writes: u64,
    pub historia_hits: u64,
    pub system_prompt_tokens: u64,
    pub hook_pre_runs: u64,
    pub hook_pre_denies: u64,
    pub hook_post_runs: u64,
    pub hook_post_failures: u64,
    pub tusk_runs: u64,
    pub tusk_edits: u64,
    pub tusk_withheld: u64,
    pub hook_stop_runs: u64,
    pub hook_stop_retries: u64,
    pub hook_stop_exhausted: u64,
    pub oracle_runs: u64,
    pub oracle_failures: u64,
    pub oracle_retries: u64,
    pub oracle_rollbacks: u64,
    pub oracle_exhausted: u64,
    pub ask_user_rounds: u64,
    pub ask_user_questions: u64,
    pub verify_tool_runs: u64,
    pub spill_writes: u64,
    pub read_outlines: u64,
    pub patch_applies: u64,
    pub patch_rejects: u64,
    pub stream_rule_trips: u64,
    pub plan_contract_initialized: u64,
    pub plan_contract_updated: u64,
    pub plan_mutations_blocked: u64,
    pub tool_batches: u64,
    pub tool_calls_emitted: u64,
    pub completion_checks_fired: u64,
    pub test_file_mutations: u64,
    pub best_of_attempts: u64,
    pub best_of_selections: u64,
    pub permission_denies_policy: u64,
    pub permission_denies_user: u64,
    pub permission_denies_unattended: u64,
}

/// Counter values as of the previous [`run_delta`] call, one slot per field of
/// [`RunCounters`] in declaration order.
static LAST_EMITTED: [AtomicU64; 39] = [const { AtomicU64::new(0) }; 39];

fn delta(slot: usize, now: u64) -> u64 {
    now.saturating_sub(LAST_EMITTED[slot].swap(now, Ordering::Relaxed))
}

/// Counters attributable to the run that just ended: process totals minus what
/// the previous call already reported. The REPL and TUI run many tasks in one
/// process, so reading the totals directly would make every run look like the
/// sum of all runs before it. The bench runs one task per process, where the
/// delta equals the total.
pub fn run_delta() -> RunCounters {
    RunCounters {
        compaction_fired: delta(0, get(&COMPACTION_FIRED)),
        historia_writes: delta(1, get(&HISTORIA_WRITES)),
        historia_hits: delta(2, get(&HISTORIA_HITS)),
        system_prompt_tokens: delta(3, system_prompt_tokens()),
        hook_pre_runs: delta(4, get(&HOOK_PRE_RUNS)),
        hook_pre_denies: delta(5, get(&HOOK_PRE_DENIES)),
        hook_post_runs: delta(6, get(&HOOK_POST_RUNS)),
        hook_post_failures: delta(7, get(&HOOK_POST_FAILURES)),
        tusk_runs: delta(36, get(&TUSK_RUNS)),
        tusk_edits: delta(37, get(&TUSK_EDITS)),
        tusk_withheld: delta(38, get(&TUSK_WITHHELD)),
        hook_stop_runs: delta(8, get(&HOOK_STOP_RUNS)),
        hook_stop_retries: delta(9, get(&HOOK_STOP_RETRIES)),
        hook_stop_exhausted: delta(10, get(&HOOK_STOP_EXHAUSTED)),
        oracle_runs: delta(11, get(&ORACLE_RUNS)),
        oracle_failures: delta(12, get(&ORACLE_FAILURES)),
        oracle_retries: delta(13, get(&ORACLE_RETRIES)),
        oracle_rollbacks: delta(14, get(&ORACLE_ROLLBACKS)),
        oracle_exhausted: delta(15, get(&ORACLE_EXHAUSTED)),
        ask_user_rounds: delta(16, get(&ASK_USER_ROUNDS)),
        ask_user_questions: delta(17, get(&ASK_USER_QUESTIONS)),
        verify_tool_runs: delta(18, get(&VERIFY_TOOL_RUNS)),
        spill_writes: delta(19, get(&SPILL_WRITES)),
        read_outlines: delta(20, get(&READ_OUTLINES)),
        patch_applies: delta(21, get(&PATCH_APPLIES)),
        patch_rejects: delta(22, get(&PATCH_REJECTS)),
        stream_rule_trips: delta(23, get(&STREAM_RULE_TRIPS)),
        plan_contract_initialized: delta(24, get(&PLAN_CONTRACT_INITIALIZED)),
        plan_contract_updated: delta(25, get(&PLAN_CONTRACT_UPDATED)),
        plan_mutations_blocked: delta(26, get(&PLAN_MUTATIONS_BLOCKED)),
        tool_batches: delta(27, get(&TOOL_BATCHES)),
        tool_calls_emitted: delta(28, get(&TOOL_CALLS_EMITTED)),
        completion_checks_fired: delta(32, get(&COMPLETION_CHECKS_FIRED)),
        test_file_mutations: delta(33, get(&TEST_FILE_MUTATIONS)),
        best_of_attempts: delta(34, get(&BEST_OF_ATTEMPTS)),
        best_of_selections: delta(35, get(&BEST_OF_SELECTIONS)),
        permission_denies_policy: delta(29, get(&PERMISSION_DENIES_POLICY)),
        permission_denies_user: delta(30, get(&PERMISSION_DENIES_USER)),
        permission_denies_unattended: delta(31, get(&PERMISSION_DENIES_UNATTENDED)),
    }
}

/// Mean tool calls per assistant turn that used tools, or `None` when the run
/// made no tool call at all — which is not the same as a run that stayed at
/// width 1, and the bench has to tell those apart.
pub fn mean_batch_width(c: &RunCounters) -> Option<f64> {
    (c.tool_batches > 0).then(|| c.tool_calls_emitted as f64 / c.tool_batches as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `run_delta` swaps *every* slot, so a concurrent test would have its
    /// increments consumed by the other one's call. Anything touching the
    /// process-wide counters takes this first.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn run_delta_reports_per_run_not_cumulative() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        add(&HISTORIA_WRITES, 3);
        assert_eq!(run_delta().historia_writes, 3);
        // No further activity: the next run contributed nothing.
        assert_eq!(run_delta().historia_writes, 0);
        add(&HISTORIA_WRITES, 2);
        assert_eq!(run_delta().historia_writes, 2);
    }

    #[test]
    fn every_counter_has_its_own_delta_slot() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // The bug this catches is a copy-pasted slot index in `run_delta`: two
        // fields sharing a slot report each other's traffic, and a bench reads a
        // mechanism as fired when it never was. Distinct increments make any
        // collision show up as a wrong number.
        run_delta(); // clear whatever the rest of the suite left behind
        add(&SPILL_WRITES, 1);
        add(&READ_OUTLINES, 2);
        add(&PATCH_APPLIES, 3);
        add(&PATCH_REJECTS, 4);
        add(&STREAM_RULE_TRIPS, 5);
        add(&TOOL_BATCHES, 6);
        add(&TOOL_CALLS_EMITTED, 7);
        add(&COMPLETION_CHECKS_FIRED, 8);
        add(&TEST_FILE_MUTATIONS, 9);
        add(&BEST_OF_ATTEMPTS, 10);
        add(&BEST_OF_SELECTIONS, 11);
        let d = run_delta();
        assert_eq!(
            (
                d.spill_writes,
                d.read_outlines,
                d.patch_applies,
                d.patch_rejects,
                d.stream_rule_trips,
                d.tool_batches,
                d.tool_calls_emitted,
                d.completion_checks_fired,
                d.test_file_mutations,
                d.best_of_attempts,
                d.best_of_selections
            ),
            (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11)
        );
    }

    /// A run that called no tool has no width, and reporting 0.0 there would let
    /// the bench average "never used tools" together with "used one at a time".
    #[test]
    fn mean_batch_width_distinguishes_no_tools_from_width_one() {
        assert_eq!(mean_batch_width(&RunCounters::default()), None);
        let c = RunCounters {
            tool_batches: 4,
            tool_calls_emitted: 4,
            ..Default::default()
        };
        assert_eq!(mean_batch_width(&c), Some(1.0));
        let c = RunCounters {
            tool_batches: 4,
            tool_calls_emitted: 10,
            ..Default::default()
        };
        assert_eq!(mean_batch_width(&c), Some(2.5));
    }
}
