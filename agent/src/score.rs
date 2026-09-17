//! `agent score <log…>` — the numbers a card or dialect change is judged
//! by, folded out of a finished log.
//!
//! Everything here is derived, never stored: a log is the record, and a
//! score is one pass over it (`DESIGN.md`'s derived-not-stored rule, the
//! same discipline `scripted::fold` already follows — this *is* that
//! fold, lifted off `Session` so it also runs on a log from a live run
//! rather than only on one the eval harness drove).
//!
//! It exists because the alternative was a throwaway JSONL parser per
//! question, written from scratch each time and agreeing with nothing.
//! The counters a change is argued from have to come from one
//! implementation, or a before/after is comparing two different
//! definitions of "call".
//!
//! # The metrics, and why each
//!
//! **`calls_per_program`** is the thesis in one number. Code mode's
//! claim is that a program does many calls per inference where a tool
//! loop does one; a change that keeps tasks passing while this falls has
//! given the advantage back.
//!
//! **`provider_ms` split out of `span_ms`** because wall clock is
//! otherwise unreadable. The same task, same card, same model ran in 34s
//! and in 1195s on 2026-09-16 — the second spent 98% of itself waiting
//! for the endpoint. Time spent inside a completion is the provider's;
//! what is left is ours, and only the second is evidence about a change
//! we made.
//!
//! **`trap_messages`** rather than a trap count alone. A trap is the
//! dialect surprising the model, and *which* surprise it was is the
//! whole finding: "cannot read .length of a map" and "falls inside a
//! multi-byte UTF-8 character" each cost a run and each named its own
//! fix. A count would have said only that something went wrong.
//!
//! **`prompt_bytes`** — what was actually *sent*, summed over the
//! completions, and the only number that tests the round-trip claim
//! from the input side. A tool loop re-sends a growing conversation on
//! every call, so twenty-three calls means twenty-three prompts each
//! larger than the last; one program means one prompt. Nothing stores
//! it, because nothing needs to: the document is a fold over the log
//! (`document::render`) and the system prompt is snapshotted on the
//! `Agent` event, so what went out is reconstructible exactly from
//! what came back. Derived, not stored, like everything else here.
//!
//! **`silent`** — a run whose programs never reached a person. A root
//! program that never calls `tell()` is a no-op the log will otherwise
//! report as a clean completion.

use crate::types::{Author, Call, Cause, Event, EventPayload, Message, Tree};

/// One finished log, reduced to the numbers a change is argued from.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Score {
    /// Completions, i.e. round trips — one per program the model wrote.
    pub programs: usize,
    /// `Call::Invoke` — tool calls. With `programs`, the ratio below.
    pub tool_calls: usize,
    /// `Call::Send` — `tell`/`ask`, which cost a dispatch but not a
    /// round trip. Counted apart from tool calls so narration cannot
    /// inflate the headline number.
    pub sends: usize,
    pub calls_per_program: f64,
    /// Statements per program, in order — a program that shrinks over a
    /// run is the transcript shape appearing.
    pub program_lengths: Vec<usize>,
    /// `raise()` for a judgement — every one of which is a question
    /// the program comes back from. There is no second kind since
    /// `next_program` went: a handover is a `Return` the next program
    /// reads, and `programs` counts those.
    pub raises: usize,
    pub traps: usize,
    /// Every trap's `kind: message`, deduplicated, in first-seen order.
    pub trap_messages: Vec<String>,
    pub compile_failures: Vec<String>,
    pub abandons: usize,
    pub resumes: usize,
    pub spawn_children: usize,
    pub notes: usize,
    /// `ask()` to a person — a `Send` that expects a reply. Counted
    /// apart from `sends` because the two are opposite acts: a `tell`
    /// informs and carries on, an `ask` stops for a judgement only a
    /// person can give. A task about recognising an ambiguity has
    /// nothing else to check.
    pub asks: usize,
    /// A run that never said anything to anyone.
    pub silent: bool,
    /// What the run actually told a person, in order. A checker often
    /// turns on this rather than on any file — "did it report the
    /// count", "did it say which ones it kept" — and a task whose whole
    /// product is an answer has nothing else to look at. Each is
    /// clipped: a `tell` carrying a pasted file would otherwise
    /// dominate the score it is one field of.
    pub tells: Vec<String>,
    /// First to last event.
    pub span_ms: i64,
    /// Time inside completions: each program's arrival minus the event
    /// before it. The provider's share, not ours.
    pub provider_ms: i64,
    /// `span_ms - provider_ms` — the harness, the VM, and the tools.
    pub exec_ms: i64,
    /// Bytes of prompt sent, summed over every completion — the
    /// document as it stood when each program was asked for.
    pub prompt_bytes: usize,
    /// Bytes of program written, summed.
    pub source_bytes: usize,
    /// Bytes of reasoning, summed. Measured 2026-09-16 at five to
    /// twenty times `source_bytes` on a real task, which is where a
    /// long run's wall clock actually goes — not into a queue, and not
    /// into the harness, whose share is `exec_ms`.
    pub thinking_bytes: usize,
    /// Tokens, as the provider counted them, summed over the
    /// completions that reported any. `cached_in` is the share of
    /// `prompt_in` served from the prefix cache — without it, "one
    /// program instead of twenty-three round trips" overstates the
    /// saving, because most of a re-sent context is cache.
    /// `reasoning_out` is the part of `completion_out` spent thinking.
    pub prompt_in: u64,
    pub cached_in: u64,
    pub completion_out: u64,
    pub reasoning_out: u64,
}

/// Fold a tree into a [`Score`]. One pass, in event-id order, which is
/// dispatch order.
pub fn score(tree: &Tree) -> Score {
    let mut events: Vec<&Event> = tree.events.values().collect();
    events.sort_by_key(|e| e.id.as_u64());

    let mut s = Score {
        programs: 0,
        tool_calls: 0,
        sends: 0,
        calls_per_program: 0.0,
        program_lengths: Vec::new(),
        raises: 0,
        traps: 0,
        trap_messages: Vec::new(),
        compile_failures: Vec::new(),
        abandons: 0,
        resumes: 0,
        spawn_children: 0,
        notes: 0,
        asks: 0,
        silent: true,
        tells: Vec::new(),
        span_ms: 0,
        provider_ms: 0,
        exec_ms: 0,
        prompt_bytes: 0,
        source_bytes: 0,
        thinking_bytes: 0,
        prompt_in: 0,
        cached_in: 0,
        completion_out: 0,
        reasoning_out: 0,
    };

    // The same stack `scripted::fold` keeps, and for the same reason:
    // `resume()` logs nothing of its own, so a resume is an open scope
    // closed by a `Return` rather than by an `Abandoned`. A stack, not a
    // subtraction, so a suspension still open when the log ends counts
    // as neither.
    let mut open_scopes = 0usize;
    let mut spawn_calls = std::collections::HashSet::new();
    let mut prev_ms: Option<i64> = None;

    for e in &events {
        let ms = e.timestamp.as_millisecond();
        match &e.payload {
            EventPayload::Message(Message::Turn {
                author: Author::Agent(_),
                source,
                thinking,
                usage,
            }) => {
                s.programs += 1;
                s.source_bytes += source.len();
                s.thinking_bytes += thinking.as_ref().map_or(0, |t| t.len());
                if let Some(u) = usage {
                    s.prompt_in += u.prompt;
                    s.cached_in += u.cached;
                    s.completion_out += u.completion;
                    s.reasoning_out += u.reasoning;
                }
                // The document as it stood when *this* program was
                // asked for: the spine ending at the event before it.
                if let Some(parent) = e.parent_id {
                    let spine = tree.spine_at(parent);
                    let doc = crate::document::render(
                        tree,
                        &spine,
                        crate::host::DEFAULT_DOCUMENT_BUDGET,
                        crate::document::configured_transport(),
                    );
                    s.prompt_bytes += doc.messages.iter().map(|m| m.content.len()).sum::<usize>();
                }
                s.program_lengths.push(interp::count_statements(source));
                // The gap before a program arrived is the completion
                // that produced it.
                if let Some(prev) = prev_ms {
                    s.provider_ms += ms - prev;
                }
            }
            EventPayload::Call(Call::Invoke { .. }) => s.tool_calls += 1,
            EventPayload::Call(Call::Send {
                text,
                expects_reply,
                ..
            }) => {
                s.sends += 1;
                if *expects_reply {
                    s.asks += 1;
                }
                s.silent = false;
                const CLIP: usize = 2000;
                s.tells.push(match text.char_indices().nth(CLIP) {
                    Some((byte, _)) => format!("{}…", &text[..byte]),
                    None => text.clone(),
                });
            }
            EventPayload::Call(Call::Spawn { .. }) => {
                spawn_calls.insert(e.id);
            }
            EventPayload::Result { call, .. } => {
                if spawn_calls.contains(call) {
                    s.spawn_children += 1;
                }
            }
            EventPayload::Note { .. } => s.notes += 1,
            EventPayload::Return { .. } => {
                if open_scopes > 0 {
                    open_scopes -= 1;
                    s.resumes += 1;
                }
            }
            EventPayload::Condition {
                cause, disposition, ..
            } => {
                match cause {
                    // Every raise is a question the program comes back
                    // from now. `handovers` counted `next_program`,
                    // which `return` replaced in 27.1 and which is gone;
                    // a handover is a `Return` the next program reads,
                    // and `programs` already counts those.
                    Cause::Raised { .. } => s.raises += 1,
                    Cause::Trapped { kind, message, .. } => {
                        s.traps += 1;
                        let line = format!("{kind}: {message}");
                        if !s.trap_messages.contains(&line) {
                            s.trap_messages.push(line);
                        }
                    }
                    Cause::Abandoned => s.abandons += 1,
                    Cause::CompileFailed { message } => {
                        s.compile_failures.push(message.clone());
                    }
                    _ => {}
                }
                if matches!(cause, Cause::Abandoned) {
                    open_scopes = open_scopes.saturating_sub(1);
                } else if *disposition == crate::types::Disposition::Pushed {
                    open_scopes += 1;
                }
            }
            _ => {}
        }
        prev_ms = Some(ms);
    }

    if let (Some(first), Some(last)) = (events.first(), events.last()) {
        s.span_ms = last.timestamp.as_millisecond() - first.timestamp.as_millisecond();
    }
    s.exec_ms = s.span_ms - s.provider_ms;
    s.calls_per_program = s.tool_calls as f64 / s.programs.max(1) as f64;
    s
}

/// `agent score <log…>` — one JSON object per log on stdout, so a
/// before/after is `diff` or `jq`, not a second parser.
pub fn run_cli(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("usage: agent score <log.jsonl> [log.jsonl…]".into());
    }
    for path in paths {
        let tree = crate::open_tree_read_only(path)?;
        let s = score(&tree);
        let mut row = serde_json::to_value(&s).map_err(|e| e.to_string())?;
        row["log"] = serde_json::Value::String(path.clone());
        println!(
            "{}",
            serde_json::to_string_pretty(&row).map_err(|e| e.to_string())?
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The score of a log the eval harness drove must agree with the
    /// `Outcome` the harness folded live — same tree, same definitions.
    /// This is the guard against the two folds drifting apart, which is
    /// the whole reason `score` exists rather than a throwaway parser.
    #[test]
    fn score_agrees_with_the_eval_fold() {
        let (sandbox, outcome) = crate::scripted::tests::drive_scripted(
            &crate::scripted::FAN_OUT,
            vec![
                "const [a, b, c] = await Promise.all([tools.read_file('a.txt'), \
                 tools.read_file('b.txt'), tools.read_file('c.txt')]); \
                 tell(\"user\", a.content + ' | ' + b.content + ' | ' + c.content);",
            ],
        );
        let s = score(outcome.tree());
        assert_eq!(s.programs, outcome.round_trips);
        assert_eq!(s.tool_calls, outcome.tool_calls);
        assert_eq!(s.traps, outcome.trap_count);
        assert_eq!(s.abandons, outcome.abandon_count);
        assert_eq!(s.resumes, outcome.resume_count);
        assert!(!s.silent, "the program told the user something");
        drop(sandbox);
    }
}
