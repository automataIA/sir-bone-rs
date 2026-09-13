//! The agent state machine: `run()` plus its turn/tool-execution helpers, the
//! chit-chat short-circuit, and the read-only localization pre-pass.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use futures::stream::{self, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use super::compact::prune_tool_results;
use super::policy::{decide, is_test_path, note_test_edits, nudge_if_stuck, unfinished_steps};
use super::tokens::{context_tokens, should_compact};
use super::{
    compact, estimate_context_tokens, AgentContext, Prompt, PromptKind,
    COMPACTION_KEEP_RECENT_DEFAULT,
};
use crate::permissions::{self, Decision, PermissionConfig, PreAction};
use crate::tools::ToolRegistry;
use crate::types::{
    extract_text, AgentEvent, AgentState, ContentBlock, Message, NoticeLevel, ToolCall,
};

/// Max times a `stop` hook may force the loop to continue, so a mis-written hook
/// (one that never exits 0) can't spin forever.
const STOP_HOOK_MAX: usize = 5;

/// The completion check speaks at most once per run. It costs one model call
/// (the measured price of the mechanism it comes from), and a second one would
/// be pressure to look finished rather than evidence about being finished.
const COMPLETION_CHECK_MAX: usize = 1;

/// Feedback for a run that is ending with its own step list unfinished, or
/// `None` when the check is off, already spent, or satisfied. Deterministic: no
/// model call is made to decide, and the one it costs is the retry itself.
async fn completion_retry(ctx: &AgentContext, spent: &mut usize) -> Option<String> {
    if std::env::var_os("SIRBONE_COMPLETION_CHECK").is_none() || *spent >= COMPLETION_CHECK_MAX {
        return None;
    }
    let msg = unfinished_steps(&ctx.tools.todos.get())?;
    *spent += 1;
    crate::telemetry::add(&crate::telemetry::COMPLETION_CHECKS_FIRED, 1);
    ctx.events
        .send(AgentEvent::Notice {
            text: "completion check: the step list is not finished".into(),
            level: NoticeLevel::Info,
        })
        .await
        .ok();
    Some(msg)
}

#[instrument(skip_all, fields(model = %ctx.model))]
pub async fn run(ctx: &mut AgentContext) -> Result<()> {
    // Greeting / meta-question short-circuit: a pure chit-chat message is
    // answered in a single minimal turn (no tools, no full system prompt)
    // instead of spinning up the task workflow. Saves the whole prompt + tool
    // schemas + any spurious init/tool calls.
    if chit_chat_gate(ctx).await? {
        return Ok(());
    }
    let max_steps = ctx.max_steps;
    let mut steps = 0usize;
    let mut stop_retries = 0usize;
    let mut completion_retries = 0usize;
    let mut overflow_recoveries = 0usize;
    let mut state = AgentState::Idle;
    loop {
        // Count LLM turns (the Idle→run_turn transition); cap them when opted in.
        if matches!(state, AgentState::Idle) {
            steps += 1;
            if let Some(cap) = max_steps {
                if steps > cap {
                    ctx.events
                        .send(AgentEvent::Notice {
                            text: format!("Max steps reached ({cap}) — stopping"),
                            level: NoticeLevel::Info,
                        })
                        .await
                        .ok();
                    // The cap blocks further provider calls, not deterministic
                    // verification. A tool call on the last allowed turn must
                    // not bypass the authoritative Done-time oracle.
                    if let Some(oracle) = ctx.oracle.as_mut() {
                        oracle.final_gate(&ctx.events).await;
                    }
                    break;
                }
            }
            // Token spend cap (Feature C): stop cleanly before spending more.
            // A Notice (not an Error) — hitting a budget is not a failure.
            if let Some(cap) = ctx.spend_cap {
                if ctx.tokens_spent >= cap {
                    ctx.events
                        .send(AgentEvent::Notice {
                            text: format!(
                                "spend cap reached ({}/{} tokens) — raise the limit or reset",
                                ctx.tokens_spent, cap
                            ),
                            level: NoticeLevel::Info,
                        })
                        .await
                        .ok();
                    break;
                }
            }
        }
        state = match state {
            // Verification oracle gate: when enabled, a Done turn runs the
            // project's tests; a red result injects feedback and resumes the
            // loop (Self-Debug / AgentCoder, see `oracle` module).
            AgentState::Done => {
                // Oracle gate first: a red test result resumes the loop.
                let oracle_retry = match ctx.oracle.as_mut() {
                    None => None,
                    Some(oracle) => {
                        let snaps = ctx.snapshots.clone();
                        match oracle.gate(snaps.as_deref(), &ctx.events).await {
                            crate::oracle::Outcome::Done => None,
                            crate::oracle::Outcome::Retry(msg) => Some(msg),
                        }
                    }
                };
                match oracle_retry {
                    Some(msg) => {
                        ctx.messages.push(Message::injected(msg));
                        AgentState::Idle
                    }
                    // Stop hooks (Feature B): exit 2 = "not done", force another
                    // iteration with the hook output as feedback. Capped so a
                    // mis-written hook can't loop forever.
                    None => match ctx.hooks.stop().await {
                        Some(reason) if stop_retries < STOP_HOOK_MAX => {
                            stop_retries += 1;
                            ctx.events
                                .send(AgentEvent::Notice {
                                    text: "stop hook requested another iteration".into(),
                                    level: NoticeLevel::Info,
                                })
                                .await
                                .ok();
                            ctx.messages.push(Message::injected(reason));
                            AgentState::Idle
                        }
                        Some(_) => {
                            crate::telemetry::add(&crate::telemetry::HOOK_STOP_EXHAUSTED, 1);
                            break;
                        }
                        // Built-in completion check, last: it only speaks when
                        // nothing else has asked for another iteration.
                        None => match completion_retry(ctx, &mut completion_retries).await {
                            Some(msg) => {
                                ctx.messages.push(Message::injected(msg));
                                AgentState::Idle
                            }
                            None => break,
                        },
                    },
                }
            }
            // The estimate that drives compaction is a heuristic; the provider's
            // 400 is the authority. When it says the request did not fit, fold
            // the history and resend the same turn instead of ending the run —
            // the work so far is still good, only the transcript was too long.
            AgentState::Idle => match run_turn(ctx).await {
                Ok(next) => {
                    overflow_recoveries = 0;
                    next
                }
                Err(e)
                    if crate::ai::is_context_overflow(&e.to_string())
                        && overflow_recoveries < OVERFLOW_MAX_RECOVERIES =>
                {
                    overflow_recoveries += 1;
                    // The threshold has already been passed by definition, so
                    // compact unconditionally rather than re-asking the estimate.
                    let compacted = compact(ctx).await;
                    // Summarizing cannot touch the kept window, and a single
                    // huge tool result inside it overflows on its own. Prune
                    // only when folding was not enough — or not possible.
                    let window = ctx.context_window.unwrap_or(128_000);
                    let freed = if compacted.is_err() || over_threshold(ctx, window) {
                        prune_tool_results(&mut ctx.messages)
                    } else {
                        0
                    };
                    if freed > 0 {
                        // History was rewritten under the anchor; the provider
                        // count no longer describes this transcript.
                        ctx.last_request = None;
                    }
                    if let Err(ce) = compacted {
                        if freed == 0 {
                            tracing::warn!(error = %ce, "overflow recovery could not compact");
                            return Err(e);
                        }
                        tracing::warn!(error = %ce, freed, "overflow recovery pruned instead");
                    }
                    let how = if freed > 0 { "pruned" } else { "compacted" };
                    ctx.events
                        .send(AgentEvent::Notice {
                            text: format!("context overflow — {how} and retrying the turn"),
                            level: NoticeLevel::Info,
                        })
                        .await
                        .ok();
                    AgentState::Idle
                }
                Err(e) => return Err(e),
            },
            AgentState::ToolCalling(tcs) => {
                // Batch width, measured here and not inside `run_tools`: the
                // localization pre-pass drives its own loop through that
                // function with a different prompt and registry, and folding it
                // in would make the number describe two agents at once. Counted
                // before the permission pass, since a denial is the user's
                // choice, not evidence about how wide the model fans out.
                crate::telemetry::add(&crate::telemetry::TOOL_BATCHES, 1);
                crate::telemetry::add(&crate::telemetry::TOOL_CALLS_EMITTED, tcs.len() as u64);
                let next = run_tools(tcs, ctx).await?;
                nudge_if_stuck(&mut ctx.messages);
                next
            }
        };

        let context_window = ctx.context_window.unwrap_or(128_000);
        let keep = ctx
            .compaction_keep_recent
            .unwrap_or(COMPACTION_KEEP_RECENT_DEFAULT)
            .min(ctx.messages.len());
        // Opt-in history hygiene used to prune the transcript here. A SpecBench
        // long-session A/B (2026-07-14) failed the quality gate on the very bench
        // it was designed for — stubbing superseded reads removes the decision
        // context a later task depends on — so the feature is gone rather than
        // left off. Compaction is the only history rewriter now.
        let tokens = context_tokens(ctx);
        // SIRBONE_NO_COMPACT: ablation toggle — let the transcript grow untouched
        // so the A/B baseline is the full uncompacted history.
        let compaction_on = std::env::var_os("SIRBONE_NO_COMPACT").is_none();
        // Turn boundary only: with tool calls already emitted but not yet run,
        // rewriting history strands them mid-turn. Wait for the results to land.
        let at_turn_boundary = !matches!(state, AgentState::ToolCalling(_));
        if compaction_on
            && at_turn_boundary
            && should_compact(tokens, context_window, ctx.messages.len(), keep)
        {
            compact_nonfatal(ctx, context_window).await;
        }
    }
    Ok(())
}

/// How many times a provider-confirmed context overflow may be answered by
/// compacting and resending the same turn. Bounded so a request that is too
/// large for reasons compaction cannot fix (one enormous tool result in the
/// kept window) surfaces its error instead of looping.
const OVERFLOW_MAX_RECOVERIES: usize = 2;

/// True when the transcript is still over the compaction threshold.
fn over_threshold(ctx: &AgentContext, context_window: usize) -> bool {
    let keep = ctx
        .compaction_keep_recent
        .unwrap_or(COMPACTION_KEEP_RECENT_DEFAULT)
        .min(ctx.messages.len());
    let tokens = context_tokens(ctx);
    should_compact(tokens, context_window, ctx.messages.len(), keep)
}

/// Compact once, and never end the run over the outcome.
///
/// The old path treated "still above threshold" and "summarizer failed" as
/// terminal — it reported "start a new session" on the error channel and broke
/// the loop, making the user restate the whole task at the moment the session
/// held the most work. Neither condition justifies that: the threshold is an
/// *estimate*, and the only authority on whether a request fits is the provider,
/// whose verdict `run()` answers with `OVERFLOW_MAX_RECOVERIES`.
///
/// Compacting a second time here would be dead code: a successful pass leaves
/// `summary + ack + kept window`, the kept window is capped at half the context
/// while the threshold sits at seven eighths of it, so a still-full transcript
/// means the unconditional two-message floor alone overflows — and a second pass
/// finds nothing new to fold. What shrinks *that* is pruning tool results out of
/// the kept window, not another summary.
async fn compact_nonfatal(ctx: &mut AgentContext, context_window: usize) {
    let text = match compact(ctx).await {
        Ok(()) if over_threshold(ctx, context_window) => {
            "context still high after compacting — continuing".to_string()
        }
        Ok(()) => return,
        Err(e) => {
            let msg = format!("{e}");
            // "Nothing left to fold" is a state, not a failure: the context is
            // token-heavy but message-poor (one large tool result).
            if msg.contains("too few messages") || msg.contains("nothing new") {
                "context high but nothing left to compact — continuing".to_string()
            } else {
                format!("compaction failed ({msg}) — continuing on the current transcript")
            }
        }
    };
    ctx.events
        .send(AgentEvent::Notice {
            text,
            level: NoticeLevel::Info,
        })
        .await
        .ok();
}

/// Greetings / identity questions that `is_chit_chat` treats as non-tasks.
/// Matched exactly (after lowercasing and stripping punctuation), so a short
/// technical prompt like "crea il file X" or "fix the parser" never matches.
const CHIT_CHAT: &[&str] = &[
    "ciao",
    "salve",
    "hey",
    "hi",
    "hello",
    "hola",
    "yo",
    "ehi",
    "ola",
    "buongiorno",
    "buonasera",
    "buonanotte",
    "grazie",
    "thanks",
    "thank you",
    "ty",
    "chi sei",
    "chi sei tu",
    "who are you",
    "what are you",
    "cosa sai fare",
    "cosa puoi fare",
    "che cosa fai",
    "che fai",
    "what can you do",
    "come stai",
    "come va",
    "how are you",
    "come ti chiami",
    "what is your name",
    "whats your name",
    "aiuto",
    "help",
];

/// Conservative greeting/meta detector — pure heuristic, no LLM call, so normal
/// prompts pay nothing. True only on an exact whitelist match, which a real task
/// prompt cannot hit; ambiguous input falls through to the normal workflow.
pub(crate) fn is_chit_chat(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() || lower.len() > 64 {
        return false;
    }
    let norm = lower
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    CHIT_CHAT.contains(&norm.as_str())
}

/// True when the current workspace has no agent instruction file (CLAUDE.md or
/// AGENTS.md) — the chit-chat gate uses it to offer initialization.
fn project_uninitialized() -> bool {
    match std::env::current_dir() {
        Ok(cwd) => !cwd.join("CLAUDE.md").exists() && !cwd.join("AGENTS.md").exists(),
        Err(_) => false,
    }
}

/// When the user's latest message is pure chit-chat, answer it in one turn with
/// a minimal system prompt and an empty tool registry, then restore the real
/// ones. Returns true when it handled the turn so `run()` returns early.
async fn chit_chat_gate(ctx: &mut AgentContext) -> Result<bool> {
    let Some(last) = ctx.messages.last() else {
        return Ok(false);
    };
    if !matches!(last.role, crate::types::Role::User) {
        return Ok(false);
    }
    if !is_chit_chat(&extract_text(&last.content)) {
        return Ok(false);
    }

    const MINI_SYSTEM: &str = "You are Sir Bone (\"sirbone\"), an AI coding agent that runs \
        shell commands, reads and edits files, and helps with software tasks. The user sent a \
        greeting or a question about you, not a task. Reply briefly and conversationally in the \
        user's language. Do not start any task, make a plan, or use any tool. If they want to \
        work on something, invite them to describe it.";

    let mut system = MINI_SYSTEM.to_string();
    if project_uninitialized() {
        system.push_str(
            " This workspace has no CLAUDE.md/AGENTS.md yet, so it is not initialized; if the \
             user wants to start working here, briefly offer to create a CLAUDE.md documenting \
             the project (purpose, layout, build/test commands).",
        );
    }

    // Swap in the minimal prompt + an empty registry for this one turn, then put
    // the real ones back — a greeting must not pay for the full prompt or tools.
    let saved_system = ctx.system_prompt.replace(system);
    let saved_tools = std::mem::replace(&mut ctx.tools, ToolRegistry::new());
    let result = run_turn(ctx).await;
    ctx.system_prompt = saved_system;
    ctx.tools = saved_tools;
    result?;
    Ok(true)
}

/// Bounded, read-only pre-pass. Runs up to `max_turns` with `read_only_tools`
/// under the given `system` prompt and returns the last assistant message
/// carrying non-empty text. Never edits (the registry is read-only); events are
/// drained silently. Best-effort — any error or empty result yields `None`.
/// Shared by `localize` (find *where*) and `plan` (decide *what*).
async fn read_only_prepass(
    client: Arc<dyn super::LlmClient>,
    model: &str,
    task: &str,
    read_only_tools: ToolRegistry,
    max_turns: usize,
    system: &str,
    cancel: &CancellationToken,
) -> Option<String> {
    // Silent: drain events into a dummy channel (like compaction).
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let mut ctx = AgentContext {
        model: model.to_string(),
        system_prompt: Some(system.to_string()),
        messages: vec![Message::user(task)],
        tools: read_only_tools,
        client,
        events: tx,
        // Share the caller's cancel token so an Esc/Ctrl-C during this pre-pass
        // interrupts it immediately, not only the main run that follows.
        cancel: cancel.clone(),
        context_window: None,
        confirm: None,
        compaction_keep_recent: None,
        permissions: PermissionConfig::default(),
        snapshots: None,
        hooks: Default::default(),
        oracle: None,
        max_steps: None,
        spend_cap: None,
        tokens_spent: 0,
        stream_rules: Default::default(),
        compacted_files: Vec::new(),
        last_request: None,
    };

    let mut state = AgentState::Idle;
    for _ in 0..max_turns {
        state = match state {
            AgentState::Done => break,
            AgentState::Idle => run_turn(&mut ctx).await.ok()?,
            AgentState::ToolCalling(tcs) => run_tools(tcs, &mut ctx).await.ok()?,
        };
    }

    // Result = the last assistant message carrying non-empty text.
    ctx.messages
        .iter()
        .rev()
        .filter(|m| matches!(m.role, crate::types::Role::Assistant))
        .map(|m| extract_text(&m.content))
        .find(|t| !t.trim().is_empty())
}

/// Bounded, read-only localization pre-pass (Agentless stage-1). Finds *where*
/// a change must happen and returns a concise report. The caller seeds the
/// result into the working notes.
pub async fn localize(
    client: Arc<dyn super::LlmClient>,
    model: &str,
    task: &str,
    read_only_tools: ToolRegistry,
    max_turns: usize,
    cancel: &CancellationToken,
) -> Option<String> {
    const SYSTEM: &str = "You are a code-localization assistant. Given a task or bug \
        report, find WHERE in the codebase the change must happen. Do NOT edit anything — \
        investigate with the read-only tools (grep, glob, find, ls, read, code_map; \
        code_map gives the symbol index and call graph), then output a concise report \
        listing the exact file path(s), the relevant function/class, the approximate line \
        range, and a one-line reason for each. If unsure, give your best candidates.";
    read_only_prepass(
        client,
        model,
        task,
        read_only_tools,
        max_turns,
        SYSTEM,
        cancel,
    )
    .await
}

/// The working-note text that carries an approved plan into the main run. It
/// is re-injected every turn and survives compaction, so the plan stays the
/// contract for the whole session.
pub fn approved_plan_note(spec: &str) -> String {
    format!("APPROVED PLAN — follow this. If you must deviate, say why in your reply before acting:\n{spec}")
}

#[instrument(skip_all)]
pub(crate) async fn run_turn(ctx: &mut AgentContext) -> Result<AgentState> {
    ctx.events.send(AgentEvent::TurnStart).await.ok();

    // Persistent working notes: re-injected every turn as a (non-system, so the
    // cached system+tools prefix stays warm) user+ack pair right after the system
    // prompt. Lives in the tool registry, so it survives context compaction.
    let notes = ctx.tools.notes.get();
    let note_msgs: Vec<Message> = if notes.trim().is_empty() {
        Vec::new()
    } else if ctx.tools.notes.plan_mode() {
        vec![
            Message::injected(format!(
                "<task_contract>\n{notes}\n</task_contract>\nUse this persistent contract while completing the task."
            )),
            Message::assistant("Acknowledged."),
        ]
    } else {
        vec![
            Message::injected(format!(
                "<working_notes>\n{notes}\n</working_notes>\n\
                 These are my persistent notes; I keep them current via the `note` tool."
            )),
            Message::assistant("Acknowledged — I'll use and update these notes."),
        ]
    };

    let sys_msgs: Vec<Message> = ctx
        .system_prompt
        .iter()
        .map(|s| Message::system(s.as_str()))
        .collect();

    // A stream rule aborts generation mid-token; the partial message is dropped,
    // the rule is injected as a reminder, and the turn restarts. Each tripped
    // rule is removed from the client for the rest of the turn, so the loop
    // terminates after at most `MAX_TRIPS` restarts (then it runs unrestricted).
    let mut spent: Vec<String> = Vec::new();
    let result = loop {
        // System + notes are small owned locals; the transcript (`ctx.messages`)
        // is borrowed by reference — no per-turn deep clone of the whole history
        // (the biggest allocation in the steady-state loop on long sessions).
        let messages: Vec<&Message> = sys_msgs
            .iter()
            .chain(note_msgs.iter())
            .chain(ctx.messages.iter())
            .collect();

        let result = ctx
            .client
            .run_turn(&messages, &ctx.tools, &ctx.events, &ctx.cancel)
            .await?;

        // Accumulate session token spend (Feature C). Prefer the provider's real
        // usage; fall back to the estimate when it reports none (local/odd
        // endpoints). An aborted attempt was still generated, so it is billed.
        if ctx.spend_cap.is_some() {
            let turn = match result.usage.total() {
                0 => {
                    estimate_context_tokens(messages.iter().copied())
                        + estimate_context_tokens(std::slice::from_ref(&result.assistant_message))
                }
                n => n as usize,
            };
            ctx.tokens_spent += turn as u64;
            if let Some(cap) = ctx.spend_cap {
                ctx.events
                    .send(AgentEvent::SpendUsage {
                        spent: ctx.tokens_spent,
                        cap,
                    })
                    .await
                    .ok();
            }
        }

        let Some(name) = result.tripped_rule.clone() else {
            break result;
        };
        // Backstop: the previous trip already disarmed every rule, so a compliant
        // client cannot reach this. Take what was generated rather than spin.
        if spent.len() >= crate::stream_rules::MAX_TRIPS {
            break result;
        }
        let reminder = ctx.stream_rules.message(&name).unwrap_or(&name).to_string();
        spent.push(name.clone());
        crate::telemetry::add(&crate::telemetry::STREAM_RULE_TRIPS, 1);
        ctx.client
            .set_stream_rules(Arc::new(if spent.len() >= crate::stream_rules::MAX_TRIPS {
                crate::stream_rules::StreamRules::default()
            } else {
                ctx.stream_rules.without(&spent)
            }));
        // Not an Error: nothing failed, the model was steered before it wrote.
        ctx.events
            .send(AgentEvent::Notice {
                text: format!("stream rule `{name}` tripped — restarting the turn"),
                level: NoticeLevel::Info,
            })
            .await
            .ok();
        ctx.messages.push(Message::injected(format!(
            "<system-reminder>\n{reminder}\n</system-reminder>"
        )));
    };

    if !spent.is_empty() {
        ctx.client.set_stream_rules(ctx.stream_rules.clone());
    }

    // Anchor the compaction trigger on what the provider actually counted for
    // this request — taken before the reply is appended, so the recorded length
    // matches the transcript that was sent. Zero means the endpoint reported no
    // usage; leave the previous anchor rather than record a false measurement.
    if result.usage.input > 0 {
        ctx.last_request = Some(super::RequestAnchor {
            input_tokens: result.usage.input as usize,
            messages_len: ctx.messages.len(),
        });
    }

    ctx.messages.push(result.assistant_message);
    Ok(result.state)
}

/// Drive an `ask_user` call through the interactive prompt bridge and turn the
/// answer into a tool result. With no bridge (headless/piped), tell the model to
/// pick a sensible default itself. Returns `(result_text, is_error)`; questions
/// never error — a dismissed prompt just yields best-judgment guidance.
async fn handle_ask_user(ctx: &mut AgentContext, tc: &ToolCall) -> (String, bool) {
    let (round, legacy) = match crate::questions::QuestionRound::from_tool_arguments(&tc.arguments)
    {
        Ok(parsed) => parsed,
        Err(e) => return (format!("Invalid question round: {e}"), true),
    };
    crate::telemetry::add(&crate::telemetry::ASK_USER_ROUNDS, 1);
    crate::telemetry::add(
        &crate::telemetry::ASK_USER_QUESTIONS,
        round.questions.len() as u64,
    );

    let Some(bridge) = &mut ctx.confirm else {
        return (
            "No interactive user is available in this session to answer. Proceed with \
             the most reasonable default and state the assumption you made."
                .into(),
            false,
        );
    };

    let mut questions = round.questions;
    let replies = if legacy {
        let question = &questions[0];
        let prompt = Prompt {
            title: question.question.clone(),
            detail: (!question.context.trim().is_empty()).then(|| question.context.clone()),
            options: question
                .options
                .iter()
                .map(|option| option.display())
                .collect(),
            allow_free_text: true,
            kind: PromptKind::Question,
        };
        bridge.ask.send(prompt).await.ok();
        let reply = bridge.reply.recv().await.unwrap_or_default();
        vec![crate::agent::PromptAnswer {
            index: reply.index,
            text: reply.text,
        }]
    } else {
        let prompt_questions = questions
            .iter()
            .map(|question| crate::agent::PromptQuestion {
                id: question.id.clone(),
                title: question.question.clone(),
                detail: (!question.context.trim().is_empty()).then(|| question.context.clone()),
                options: question
                    .options
                    .iter()
                    .map(|option| option.display())
                    .collect(),
                allow_free_text: true,
            })
            .collect();
        let prompt = Prompt {
            title: format!("{} questions", questions.len()),
            detail: Some("Answer every question, then submit the round once.".into()),
            options: Vec::new(),
            allow_free_text: false,
            kind: PromptKind::QuestionRound {
                questions: prompt_questions,
            },
        };
        bridge.ask.send(prompt).await.ok();
        bridge.reply.recv().await.unwrap_or_default().answers
    };

    let mut answers = Vec::with_capacity(questions.len());
    for (position, question) in questions.drain(..).enumerate() {
        let reply = replies.get(position).cloned().unwrap_or_default();
        let (value, origin) = match reply.index {
            Some(i) => (
                question
                    .options
                    .get(i)
                    .map(|option| option.label.clone())
                    .unwrap_or_default(),
                crate::questions::AnswerOrigin::User,
            ),
            None if reply.text.as_deref().is_some_and(|t| !t.trim().is_empty()) => (
                reply.text.unwrap_or_default(),
                crate::questions::AnswerOrigin::FreeText,
            ),
            None => (String::new(), crate::questions::AnswerOrigin::Dismissed),
        };
        answers.push(crate::questions::QuestionAnswer {
            id: question.id,
            value,
            index: reply.index,
            origin,
        });
    }
    if legacy {
        let answer = &answers[0];
        if answer.origin == crate::questions::AnswerOrigin::Dismissed {
            ("The user dismissed the question without choosing. Proceed with your best judgment and state the assumption you made.".into(), false)
        } else {
            (format!("The user chose: {}", answer.value), false)
        }
    } else {
        (serde_json::json!({"answers": answers}).to_string(), false)
    }
}

#[instrument(skip_all, fields(n_tools = tool_calls.len()))]
async fn run_tools(tool_calls: Vec<ToolCall>, ctx: &mut AgentContext) -> Result<AgentState> {
    // Sequential permission pass: decide each tool call before any parallel
    // execution begins.
    let mut approved: Vec<ToolCall> = Vec::new();
    for mut tc in tool_calls {
        // `ask_user` is handled inline (not run in parallel): it needs the
        // interactive prompt bridge, which only the sequential pass can touch.
        if tc.name == "ask_user" {
            let (result, is_error) = handle_ask_user(ctx, &tc).await;
            ctx.events
                .send(AgentEvent::ToolCallStart {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input: tc.arguments.clone(),
                })
                .await
                .ok();
            ctx.events
                .send(AgentEvent::ToolCallEnd {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    result: result.clone(),
                    is_error,
                })
                .await
                .ok();
            ctx.messages
                .push(Message::tool_result(&tc.id, result, is_error));
            continue;
        }
        if mutates_workspace(&tc, &ctx.tools) {
            let missing = ctx.tools.notes.incomplete_sections();
            if !missing.is_empty() {
                crate::telemetry::add(&crate::telemetry::PLAN_MUTATIONS_BLOCKED, 1);
                let reason = format!(
                    "Workspace mutation blocked: the persistent task contract is incomplete ({missing}). Update it with the `note` tool, then retry.",
                    missing = missing.join(", ")
                );
                ctx.events
                    .send(AgentEvent::ToolCallStart {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input: tc.arguments.clone(),
                    })
                    .await
                    .ok();
                ctx.events
                    .send(AgentEvent::ToolCallEnd {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        result: "blocked: incomplete task contract".into(),
                        is_error: true,
                    })
                    .await
                    .ok();
                ctx.messages
                    .push(Message::tool_result(&tc.id, reason, true));
                continue;
            }
        }
        let inner = permissions::tool_inner(&tc.name, &tc.arguments);
        let (decision, action) = decide(ctx, &tc.name, &tc.arguments).await;

        // updatedInput: merge a rewritten input over the call the model wrote —
        // a `pre_tool_use` hook's exit-4 patch, or the classifier's safer
        // command. Merged, not replaced, so a hook that only cares about one
        // field does not have to echo the rest of the input back.
        if let PreAction::Rewrite(patch) = &action {
            if let (Some(obj), Some(fields)) = (tc.arguments.as_object_mut(), patch.as_object()) {
                for (k, v) in fields {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }

        // Steers the model after a block: pursue the goal another legitimate
        // way, but never relitigate the denial through a side door.
        const DENY_GUIDANCE: &str = "You may pursue the goal another reasonable \
            way that respects the intent behind this denial — do not work around \
            it (e.g. by doing the same thing through a different tool). If this \
            capability is essential, stop, explain what you were trying to do \
            and why, and let the user decide.";
        let blocked_reason = match decision {
            Decision::Allow => None,
            Decision::Deny(reason) => {
                crate::telemetry::add(&crate::telemetry::PERMISSION_DENIES_POLICY, 1);
                Some(format!("Command blocked: {reason}. {DENY_GUIDANCE}"))
            }
            Decision::Ask => match &mut ctx.confirm {
                Some(bridge) => {
                    let glob = permissions::suggested_glob(&tc.name, &inner);
                    let prompt = Prompt {
                        title: "permission required".into(),
                        detail: Some(inner.clone()),
                        options: vec!["Allow once".into(), "Allow always".into(), "Deny".into()],
                        allow_free_text: true,
                        kind: PromptKind::Permission {
                            suggested_glob: glob.clone(),
                        },
                    };
                    bridge.ask.send(prompt).await.ok();
                    let reply = bridge.reply.recv().await.unwrap_or_default();
                    match reply.index {
                        // Allow once: run this call, remember nothing.
                        Some(0) => None,
                        // Allow always: persist an allow-glob (the user's edited
                        // rule, or the suggested one) to the project config and
                        // honor it for the rest of this session, then run.
                        Some(1) => {
                            let rule = reply.text.filter(|s| !s.trim().is_empty()).unwrap_or(glob);
                            if let Err(e) = crate::config::add_project_allow(&rule) {
                                tracing::warn!("failed to persist allow rule {rule}: {e}");
                            }
                            ctx.permissions.allow.push(rule);
                            None
                        }
                        // Deny (index 2 or a bare free-text reply): block, and
                        // forward any typed feedback to the model.
                        _ => {
                            crate::telemetry::add(&crate::telemetry::PERMISSION_DENIES_USER, 1);
                            let feedback = reply
                                .text
                                .filter(|s| !s.trim().is_empty())
                                .map(|s| format!(" User feedback: {s}."))
                                .unwrap_or_default();
                            Some(format!(
                                "Command blocked (denied by the user): {inner}.{feedback} {DENY_GUIDANCE}"
                            ))
                        }
                    }
                }
                // No confirm channel (one-shot / piped): auto-deny with an explicit
                // reason so the model reports back instead of retrying variants.
                None => {
                    crate::telemetry::add(&crate::telemetry::PERMISSION_DENIES_UNATTENDED, 1);
                    Some(format!(
                        "Command blocked (auto-denied: destructive commands need user \
                     confirmation, which is unavailable in this non-interactive \
                     session — do NOT retry it or a variant of it): {inner}. {DENY_GUIDANCE}"
                    ))
                }
            },
        };

        if let Some(reason) = blocked_reason {
            ctx.events
                .send(AgentEvent::ToolCallStart {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input: tc.arguments.clone(),
                })
                .await
                .ok();
            ctx.events
                .send(AgentEvent::ToolCallEnd {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    result: "blocked".into(),
                    is_error: true,
                })
                .await
                .ok();
            ctx.messages
                .push(Message::tool_result(&tc.id, reason, true));
            continue;
        }

        // A `pre_tool_use` hook answered the call itself (exit 5): the tool
        // never runs. This is how a cache hit or an overridden built-in tool is
        // expressed — the model sees an ordinary result either way.
        if let PreAction::Short(result) = action {
            ctx.events
                .send(AgentEvent::ToolCallStart {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input: tc.arguments.clone(),
                })
                .await
                .ok();
            ctx.events
                .send(AgentEvent::ToolCallEnd {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    result: result.clone(),
                    is_error: false,
                })
                .await
                .ok();
            ctx.messages
                .push(Message::tool_result(&tc.id, result, false));
            continue;
        }
        approved.push(tc);
    }

    if approved.is_empty() {
        return Ok(AgentState::Idle);
    }

    // Workspace snapshot (shadow git, once per run) before anything mutates,
    // so the whole run is rollback-able — including bash side effects that the
    // per-file undo tool can't see.
    if let Some(snaps) = &ctx.snapshots {
        if approved.iter().any(|tc| mutates_workspace(tc, &ctx.tools)) {
            // Last (not first) user prompt: in a multi-turn REPL session every
            // snapshot would otherwise carry the session's opening prompt.
            let label = last_user_prompt(&ctx.messages);
            if let Some(id) = snaps.take_once_id(&label).await {
                ctx.events
                    .send(AgentEvent::WorkspaceSnapshot { id, label })
                    .await
                    .ok();
            }
        }
    }

    // Paths edited by this batch, for the post-edit checks below.
    let edited: Vec<String> = approved
        .iter()
        .filter_map(|tc| mutation_path(tc, &ctx.tools))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    // Group calls into lanes: mutating calls hitting the same file share a lane
    // and run in order; everything else gets its own lane. Lanes run in
    // parallel, calls within a lane sequentially — so two edits/writes to the
    // same path can't race and silently drop one update.
    let lanes = plan_lanes(approved, &ctx.tools);

    // Test writes are counted inside the lanes; read the counter here so the
    // delta below is this batch's alone and no per-run state has to be threaded.
    let tests_before = crate::telemetry::get(&crate::telemetry::TEST_FILE_MUTATIONS);

    let parallelism = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16);
    let tools = ctx.tools.clone();
    let events = ctx.events.clone();
    let hooks = ctx.hooks.clone();

    let results: Vec<Vec<Message>> = stream::iter(lanes)
        .map(|lane| {
            let tools = tools.clone();
            let events = events.clone();
            let hooks = hooks.clone();
            async move {
                let mut out = Vec::with_capacity(lane.len());
                for tc in lane {
                    events
                        .send(AgentEvent::ToolCallStart {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            input: tc.arguments.clone(),
                        })
                        .await
                        .ok();
                    // The one place a call actually reaches a tool; counted here
                    // so the bench can tell dispatch from a blocked call, which
                    // still emits ToolCallStart/End.
                    crate::telemetry::add(&crate::telemetry::TOOL_CALLS_DISPATCHED, 1);
                    let exec_result = tools.execute(&tc.name, tc.arguments.clone()).await;
                    let (content, is_error) = match exec_result {
                        Ok(s) => (s, false),
                        Err(e) => (format!("Error: {e}"), true),
                    };
                    // `tusk` filters see the result here, before anything else
                    // does: the model's context, the session transcript and the
                    // UI are all fed from this one point, so a secret removed
                    // here is removed from all three.
                    let (content, is_error) = match hooks
                        .tusk(&tc.name, &tc.arguments, &content, is_error)
                        .await
                    {
                        crate::checks::TuskOutcome::Unchanged => (content, is_error),
                        crate::checks::TuskOutcome::Replaced(filtered) => (filtered, is_error),
                        crate::checks::TuskOutcome::Withheld(reason) => (reason, true),
                    };
                    // Honesty signal, recorded only on a write that landed: an
                    // edit rejected for a stale `old_string` changed no test.
                    if !is_error && mutation_path(&tc, &tools).is_some_and(|p| is_test_path(&p)) {
                        crate::telemetry::add(&crate::telemetry::TEST_FILE_MUTATIONS, 1);
                    }
                    events
                        .send(AgentEvent::ToolCallEnd {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            result: content.clone(),
                            is_error,
                        })
                        .await
                        .ok();
                    out.push(Message::tool_result(&tc.id, content, is_error));
                }
                out
            }
        })
        .buffer_unordered(parallelism)
        .collect()
        .await;

    ctx.messages.extend(results.into_iter().flatten());

    // A test write landed in this batch: state the one fact that keeps the
    // model's later summary honest. Default on — deterministic, strictly more
    // information than the silence it replaces, and it can only fire where the
    // counter already moved.
    if crate::telemetry::get(&crate::telemetry::TEST_FILE_MUTATIONS) > tests_before
        && !crate::ablate::test_notice_disabled()
    {
        note_test_edits(&mut ctx.messages);
    }

    // Post-edit checks: lint/typecheck the batch and ride failures into the
    // last tool result (same inline mechanism as the stuck-loop nudge), so the
    // model fixes breakage in this turn instead of discovering it edits later.
    if !edited.is_empty() && !ctx.hooks.post.is_empty() {
        if let Some(report) = ctx.hooks.post.run(&edited).await {
            if let Some(ContentBlock::ToolResult { content, .. }) =
                ctx.messages.last_mut().and_then(|m| m.content.last_mut())
            {
                content.push_str(&report);
            }
        }
    }
    Ok(AgentState::Idle)
}

/// True if the call can change the workspace: a file-mutating tool, or a bash
/// command that isn't known read-only (background jobs included).
fn mutates_workspace(tc: &ToolCall, tools: &ToolRegistry) -> bool {
    mutation_path(tc, tools).is_some()
        || (tc.name == "bash"
            && !permissions::is_safe_readonly(
                tc.arguments
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            ))
}

/// First user message of the transcript — the snapshot label.
fn last_user_prompt(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, crate::types::Role::User))
        .map(|m| extract_text(&m.content))
        .unwrap_or_default()
}

/// Resolved target path of a mutating call (canonicalized for lane grouping), or
/// `None` if the call doesn't mutate a file. Asks the tool via
/// [`ToolRegistry::mutation_target`]; falls back to the raw path when the file
/// doesn't exist yet.
fn mutation_path(tc: &ToolCall, tools: &ToolRegistry) -> Option<PathBuf> {
    let raw = tools.mutation_target(&tc.name, &tc.arguments)?;
    Some(std::fs::canonicalize(&raw).unwrap_or(raw))
}

/// Partition approved calls into execution lanes. Mutating calls on the same
/// resolved path land in one lane (serial); all others get a private lane.
pub(crate) fn plan_lanes(approved: Vec<ToolCall>, tools: &ToolRegistry) -> Vec<Vec<ToolCall>> {
    let mut lanes: Vec<Vec<ToolCall>> = Vec::new();
    let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
    for tc in approved {
        match mutation_path(&tc, tools) {
            Some(path) => {
                let idx = *by_path.entry(path).or_insert_with(|| {
                    lanes.push(Vec::new());
                    lanes.len() - 1
                });
                lanes[idx].push(tc);
            }
            None => lanes.push(vec![tc]),
        }
    }
    lanes
}
