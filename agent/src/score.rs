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

use crate::types::{Call, Event, EventPayload, Tree};

/// One finished log, reduced to the numbers a change is argued from.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Score {
    /// **Completions, i.e. round trips** — which is not the same as
    /// `Turn`s any more.
    ///
    /// Under `Transport::Program` it is: one `Turn` per completion. Under
    /// `Transport::Notebook` a reply is N `Turn`s, one per cell, and one
    /// completion — so counting `Turn`s there would report a three-cell
    /// reply as three round trips and make the arm look like it drifted
    /// when nothing drifted. `EventPayload::Completion` is logged exactly
    /// once per completion, so where any exist they are what is counted;
    /// a log with none is a scripted or hand-driven run nobody was billed
    /// for, where every `Turn` really is its own turn.
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
    /// Time inside completions: each reply's first `Turn` minus the
    /// **previous reply's outcome**. The provider's share, not ours.
    ///
    /// Measured from the outcome rather than from whatever event happens
    /// to precede the `Turn`, because under `Transport::Notebook` that
    /// is the reply's own opening prose — logged while the completion is
    /// still streaming — and the gap to it is nil. The generation wait
    /// is real on both transports and lands here on both.
    ///
    /// It does **not** partition wall clock under the notebook the way
    /// it does under the program transport. Cells run while later ones
    /// are still being generated (D11), so part of what this counts is
    /// also counted by `exec_ms`, and the two no longer sum to
    /// `span_ms`. That overlap is the point of the transport, not an
    /// error in the measurement.
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
    ///
    /// **`reasoning_out` is estimated when the provider will not say.**
    /// `opencode.ai/zen` folds reasoning into `completion_tokens` and
    /// reports `reasoning_tokens: 0`, so a run that wrote 300 bytes of
    /// program and 17.6 KB of reasoning read as `4909 out (0
    /// reasoning)` — indistinguishable from 4,909 tokens of program,
    /// which is the opposite of what happened. Where the log holds
    /// reasoning text and the provider claimed none, the bytes are
    /// divided by [`BYTES_PER_TOKEN`] instead. An estimate labelled as
    /// one beats a zero that reads as a measurement.
    pub prompt_in: u64,
    pub cached_in: u64,
    pub completion_out: u64,
    pub reasoning_out: u64,
    /// Whether `reasoning_out` was estimated from `thinking_bytes`
    /// rather than reported.
    pub reasoning_estimated: bool,
}

/// Bytes per token, for the one figure that has to be estimated. A
/// round number on purpose: it is an order-of-magnitude stand-in, not
/// a tokenizer, and there is none in this crate.
pub const BYTES_PER_TOKEN: usize = 4;

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
        reasoning_estimated: false,
    };

    // The same stack `scripted::fold` keeps, and for the same reason:
    // `resume()` logs nothing of its own, so a resume is an open scope
    // closed by a `Return` rather than by an `Abandoned`. A stack, not a
    // subtraction, so a suspension still open when the log ends counts
    // as neither.
    let mut open_scopes = 0usize;
    let mut spawn_calls = std::collections::HashSet::new();
    // `Turn`s seen, and completions seen. They differ under
    // `Transport::Notebook`; see `Score::programs`.
    let mut turns = 0usize;
    let mut completions = 0usize;
    // The end of the last reply — an outcome, or, for the first one,
    // the log's own start. The generation wait is measured from here,
    // not from whatever event happens to sit immediately before a
    // `Turn`: under `Transport::Notebook` that is the reply's own
    // opening prose, logged mid-generation, and the gap to it is nil.
    let mut last_outcome_ms: Option<i64> = events.first().map(|e| e.timestamp.as_millisecond());
    // Whether a `Turn` has been seen since that outcome, so only the
    // first one of a reply charges the wait.
    let mut turn_since_outcome = false;

    for e in &events {
        let ms = e.timestamp.as_millisecond();
        match &e.payload {
            // **One reply, one round trip** (28). It used to be one
            // per agent-authored `Turn`, filtered to exclude the user's
            // restarts — a distinction the log now makes by kind, so it
            // cannot be miscounted by a field being set wrong.
            EventPayload::Reply => {
                turns += 1;
                // The document as it stood when *this* reply was asked
                // for: the spine ending at the event before it.
                if let Some(parent) = e.parent_id {
                    let spine = tree.spine_at(parent);
                    let doc =
                        crate::document::render(tree, &spine, crate::host::DEFAULT_DOCUMENT_BUDGET);
                    s.prompt_bytes += doc.messages.iter().map(|m| m.content.len()).sum::<usize>();
                }
                if !turn_since_outcome && let Some(prev) = last_outcome_ms {
                    s.provider_ms += ms - prev;
                }
                turn_since_outcome = true;
            }
            EventPayload::Part { part, .. } => match part {
                crate::types::Part::Thinking(t) => s.thinking_bytes += t.len(),
                crate::types::Part::Cell(t) => {
                    s.source_bytes += t.len();
                    s.program_lengths.push(interp::count_statements(t));
                }
                crate::types::Part::Prose(_) => {}
            },
            EventPayload::ReplyEnd { usage, .. } => {
                completions += 1;
                s.prompt_in += usage.prompt;
                s.cached_in += usage.cached;
                s.completion_out += usage.completion;
                s.reasoning_out += usage.reasoning;
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
            EventPayload::Handback { how, .. } => {
                last_outcome_ms = Some(ms);
                turn_since_outcome = false;
                if how.is_terminal() && open_scopes > 0 {
                    s.resumes += 1;
                }
                match how {
                    // Every raise is a question the program comes back
                    // from now. `handovers` counted `next_program`,
                    // which `return` replaced in 27.1 and which is gone;
                    // a handover is a `Return` the next program reads,
                    // and `programs` already counts those.
                    crate::types::Handback::Raised { .. } => s.raises += 1,
                    crate::types::Handback::Trapped { kind, message, .. } => {
                        s.traps += 1;
                        let line = format!("{kind}: {message}");
                        if !s.trap_messages.contains(&line) {
                            s.trap_messages.push(line);
                        }
                    }
                    crate::types::Handback::Abandoned => s.abandons += 1,
                    crate::types::Handback::CellFailed { message } => {
                        s.compile_failures.push(message.clone());
                    }
                    _ => {}
                }
                if how.is_terminal() {
                    open_scopes = open_scopes.saturating_sub(1);
                } else {
                    open_scopes += 1;
                }
            }
            _ => {}
        }
    }

    if let (Some(first), Some(last)) = (events.first(), events.last()) {
        s.span_ms = last.timestamp.as_millisecond() - first.timestamp.as_millisecond();
    }
    // **The provider counted no reasoning and the log holds some.**
    // See `Score::reasoning_out`.
    if s.reasoning_out == 0 && s.thinking_bytes > 0 {
        s.reasoning_out = (s.thinking_bytes / BYTES_PER_TOKEN) as u64;
        s.reasoning_estimated = true;
    }
    // See `Score::programs`: a log that records completions is counted
    // by them, because a `Turn` there may be one cell of several.
    s.programs = if completions > 0 { completions } else { turns };
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

    /// **The generation wait is measured from the previous reply's
    /// outcome, not from the event before the `Turn`** — and under
    /// `Transport::Notebook` those are very different things.
    ///
    /// A notebook reply's first logged event is its own opening prose,
    /// written while the completion is still streaming. Anchoring on
    /// "the event before the `Turn`" therefore measured the gap from
    /// that prose to the cell below it — microseconds — and reported a
    /// six-second run as `waiting on the provider 0.0s`, with the whole
    /// wall clock attributed to execution.
    ///
    /// Built from a log with chosen timestamps, because that is the only
    /// way to assert a duration rather than assume one.
    #[test]
    fn the_provider_wait_is_measured_from_the_previous_outcome() {
        // t=0 the branch opens; the request goes out; the provider
        // takes five seconds to say anything; then the reply's prose,
        // its two cells, its end and its outcome land in quick
        // succession.
        let log = synthetic_log(&[
            (0, r#"{"Agent":{"charter":"c","system":"s"}}"#),
            (
                10,
                r#"{"Post":{"from":"User","origin":{"Direct":{"text":"go","input":null,"options":[],"expects_reply":true}}}}"#,
            ),
            // Five seconds of generation, invisible in the log until
            // the first token arrives and opens the reply.
            (5_010, r#""Reply""#),
            (
                5_012,
                r#"{"Part":{"reply":3,"part":{"Prose":"Looking now.\n\n"}}}"#,
            ),
            (
                5_015,
                r#"{"Call":{"Send":{"to":"User","text":"Looking now.","input":null,"options":[],"expects_reply":false,"site":0,"site_end":0}}}"#,
            ),
            (
                5_020,
                r#"{"Part":{"reply":3,"part":{"Cell":"```js\ntell(\"hi\");\n```\n"}}}"#,
            ),
            (
                5_030,
                r#"{"Part":{"reply":3,"part":{"Cell":"```js\ndone(\"ok\");\n```\n"}}}"#,
            ),
            (
                5_040,
                r#"{"ReplyEnd":{"reply":3,"how":"Finished","usage":{"prompt":10,"cached":0,"completion":20,"reasoning":0}}}"#,
            ),
            (
                5_050,
                r#"{"Handback":{"reply":3,"how":"Completed","site":0,"stack":[]}}"#,
            ),
        ]);
        let tree = crate::open_tree_read_only(log.path().to_str().unwrap()).unwrap();
        let s = score(&tree);

        assert_eq!(s.programs, 1, "two cells, one round trip");
        assert_eq!(s.completion_out, 20);
        // The five seconds are the provider's, and they are charged
        // once — not once per cell, and not lost to the prose, which is
        // logged mid-generation and would show a gap of nil.
        assert_eq!(
            s.provider_ms, 5_010,
            "the wait runs from the log's start to the reply opening"
        );
        assert!(
            s.exec_ms < 100,
            "and the rest is execution, not the whole run: {}",
            s.exec_ms
        );
    }

    /// Write a log with chosen timestamps and hand back the file.
    fn synthetic_log(rows: &[(i64, &str)]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"version":{}}}"#, crate::tree::LOG_VERSION).unwrap();
        for (i, (ms, payload)) in rows.iter().enumerate() {
            let id = i + 1;
            let parent = if i == 0 {
                "null".to_string()
            } else {
                id.saturating_sub(1).to_string()
            };
            writeln!(
                f,
                r#"{{"id":{id},"parent_id":{parent},"timestamp":{ms},"payload":{payload}}}"#
            )
            .unwrap();
        }
        f.flush().unwrap();
        f
    }
}
