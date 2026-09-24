//! Sans-io branch step machine (8_HARNESS Step 3; substituted for code
//! mode by 23_ONE_AGENT.md A4).
//!
//! One `Runner` drives one branch: a deterministic, IO-free core the
//! host feeds with `StepInput`s and drains of `StepOutput`s. The host
//! owns the LLM API, tool execution, subagent loops, and scheduling;
//! the core never blocks. VM compute is host-fueled: the machine runs
//! one `step(fuel)` slice per `Tick` and reports `Working` when it
//! wants another, so a hot program can't starve the host loop.
//!
//! **The one substitution this file makes** (23_ONE_AGENT.md, "What is
//! actually changing"): the model's entire turn used to be `Turn {
//! text, tool_calls: [run_program|resume|answer] }`, chosen off three
//! tool schemas offered on every request and policed by a per-restart
//! validity check on the `Runner`. Now the model's entire turn **is** a program —
//! `Turn { source }`, bare — and there is nothing to choose among:
//! every turn compiles and runs. A restart of a suspended run is no
//! longer a distinguished tool call either; it is a direct,
//! host-driven call to [`Runner::resume`]/[`Runner::abandon`], made
//! only once the host has decided (by whatever means it uses to run a
//! handler completion and read its `return resume(value)` /
//! `return abandon()` decision — DESIGN.md's thesis table, built
//! outside this file) that this branch is the one to reactivate.
//! Because that decision never reaches here as a name the LLM typed,
//! ineligibility stops being an event kind this file has to render: a
//! host that calls `resume`/`abandon` with nothing suspended has a
//! bug of its own, not a user-facing refusal to construct.

use std::collections::HashMap;
use std::io;

use interp::{
    InvokeCall, PromisePtr, RcStr, ResumeMode, SettleCall, StepResult, VM, VMError, Value,
};

use crate::host::ProgramStatus;
use crate::report::{Artifact, ArtifactState, arg_preview, preview};
use crate::types::*;

/// The closed, harness-defined verb names this file recognizes — the
/// same strings `interp`'s compiler emits for a **bare** call
/// (`tell(...)`, `ask(...)`, `spawn(...)`, ...; the settle-at-dispatch
/// arm and the `ask` arm of `interp/src/compiler/call.rs`), never for a
/// `tools.foo(...)` call, which stays a configured capability the
/// registry answers. This is "the one place a bare verb's name becomes
/// a `Call` variant" (17_BRANCHES A2) generalized: folded in here from
/// the deleted `verbs.rs`, whose job was exactly this parse, just not
/// yet wired to a live session.
///
/// Which dispatcher sees a name depends on how it lowers.
/// `dispatch_calls` takes the ones that hold a promise — `ask`, `tell`,
/// and anything unrecognized — from a `StepResult::Pending` batch;
/// `dispatch_settle` takes the rest, one at a time, from a
/// `StepResult::Settle`. `interp::HARNESS_VERBS` is the whole list, and
/// `every_harness_verb_has_an_answerer` checks each one lands in one of
/// them rather than in the registry, which has no tool by any of these
/// names.
///
/// `resume`/`abandon` are deliberately **not** among these: they
/// compile to a plain tagged object (`{ __decision: "resume", value
/// }`), never to `Instr::Invoke` — a handler's `return resume(v)` is
/// pure value construction the host reads off a `Return`, not a call
/// this file ever sees arrive as a `Pending` effect. `raise` is
/// likewise absent: it is its own `Instr::Raise`, handled in `pump`.
pub const TOOL_SPAWN: &str = "spawn";
pub const TOOL_ASK: &str = "ask";
/// `choose(who, question, options)` — `ask`'s constrained sibling. The
/// value it settles with is one of `options`, `===`-equal, so the
/// asking program can compare and switch on it without checking; a
/// person who answers outside the set rejects the call instead, which
/// `Await` escalates as a *resumable* condition, so their actual words
/// reach a program that can judge them and `resume(...)` stands in for
/// the value. Nothing here is new machinery — that is what a rejected
/// await has always done. See [`Call::Send::options`].
pub const TOOL_CHOOSE: &str = "choose";
pub const TOOL_TELL: &str = "tell";
/// `fork()` — a divergent branch inheriting this agent's history,
/// settled with the fork's handle exactly as `spawn` is (types.rs
/// `Call::Fork`; DESIGN.md's "Exchanges").
pub const TOOL_FORK: &str = "fork";
/// `answer(question, label, value)` — discharges an open post from
/// *inside* a program, unlike the old top-level `answer` tool call:
/// there is no longer a distinguished "turn that only answers", so
/// this is an ordinary dispatched call like any other bare verb.
pub const TOOL_ANSWER: &str = "answer";
/// `note_history(value)` — logs an `EventPayload::Note` (22's "one
/// vocabulary decision" list; DESIGN.md "No exception"): what a mind
/// chose to remember for its own later turns, never re-derived and
/// never entering anyone else's context.
pub const TOOL_NOTE_HISTORY: &str = "note_history";
/// `fetch_history(id)` — id-addressable fetch from the log, resolved
/// synchronously without a host round trip. The **only** survivor of
/// the old budgeted-answer machinery (DESIGN.md "No exception": the
/// artifact model and id-addressable fetch stay; only the budgeted
/// copy-into-context goes).
///
/// Named `artifact` until 27.4, which is what it was while the thing
/// it read was a separate compartment — a menu of call results beside
/// the conversation. There is one history: the menu is those rows
/// rendered, `note_history` writes one, `remove_history` and
/// `rewrite_history` shorten one, and this reads one back. A verb
/// called `artifact` in that set names a compartment that no longer
/// exists, and it is the only one of the four that did not say what it
/// operated on.
///
/// **Reading is free.** This logs nothing, which is what lets the menu
/// be an index rather than a replay: a value reaches the *program*
/// without entering the *document*, so nothing one program fetched is
/// inflicted on the program after it.
pub const TOOL_FETCH_HISTORY: &str = "fetch_history";
/// `remove_history(id, label)` / `rewrite_history(id, label, value)` —
/// Part E's compaction verbs. Recognized here so a malformed call gets
/// a precise rejection rather than a confusing round trip to a host
/// tool that doesn't exist, but **not dispatched**: `compaction.rs` is
/// mid-rewrite in this same phase (re-rooting on `&Tree`/`&[Event]`,
/// 23_ONE_AGENT.md A3) and wiring it in here would be guessing at an
/// API that is still moving. Left for a later pass — flagged in A4's
/// own report, not silently dropped.
/// How many compaction programs a branch will ask for before giving up
/// and carrying on over budget. See [`Runner::compaction_attempts`] for
/// the floor that makes a bound necessary at all.
const COMPACTION_ATTEMPTS: u32 = 2;

pub const TOOL_KEEP_HISTORY: &str = "keep_history";
pub const TOOL_PEEK_HISTORY: &str = "peek_history";
pub const TOOL_REMOVE_HISTORY: &str = "remove_history";
pub const TOOL_REPLACE_HISTORY: &str = "replace_history";
/// `list_agents()` — every agent in this subtree, with status, exactly
/// as the card has advertised since phase 20. It is served by the
/// host's `serve_agents` (the one implementation, shared with
/// ), because the status half is live session state
/// no single runner can see; `dispatch_settle` routes it there.
pub const TOOL_LIST_AGENTS: &str = "list_agents";

/// Open-post ids named in the request's trailing note before it says
/// "and N more" — a bounded line, like every other rendered bound.
const OPEN_NOTE_MAX_IDS: usize = 8;
/// How many entries the compaction directive names as the heaviest.
///
/// Short on purpose. The list is a place to look first, not the plan —
/// a compaction program that worked only through this list would be
/// choosing by size alone, which is the choice the directive spends
/// three paragraphs arguing against.
const HEAVIEST_NAMED: usize = 5;
/// How full the conversation has to be before the tail says so.
///
/// Half. Below that the line is noise — the room is not the binding
/// constraint and saying so every request only spends bytes making that
/// point. Above it, every row dropped is dropped while it is still
/// cheap to drop, which is the whole reason for saying anything before
/// the hard trigger fires.
const SOFT_FULL_PERCENT: usize = 50;

/// The trailing presence line, the two ways round. It is deliberately
/// about the *client*, not the person: attached means a client is
/// connected, and claiming to know a human is reading would be a lie the
/// model would act on.
///
/// **And the wording has to hold that line too.** "No one is attached"
/// reads as "nobody is there", which is the inference the paragraph
/// above refuses to license — in a headless eval it is always false in
/// that sense, since a person reads the transcript afterwards.
///
/// **It states the fact and stops.** "may wait hours for an answer"
/// was a fact phrased as advice, and the advice was against asking —
/// against three card paragraphs arguing that a written-down ambiguity
/// is a question addressed to you, from the last line of the request,
/// which is the strongest position there is. `ambiguous-config` failed
/// 2 of 3 on 2026-09-20 with "never asked". Whether the wait is worth
/// it is the model's to weigh; that the question stays open is ours to
/// report.
const PRESENT: &str = "- A client is attached; an ask() may be answered promptly.";
const ABSENT: &str = "- No client is attached; an ask() stays open until someone answers it.";

/// What the harness says when the user interrupts a running program and
/// has nothing else to add. It is a `tell` — the branch owes no answer —
/// and it exists so the wake has a cause event in the log.
///
/// Rewritten for code mode (23_ONE_AGENT.md A4 dec. 1): the old text
/// offered a menu of three named tools (`resume()` /
/// `run_program(source)` / "a plain reply"), none of which exist as
/// distinguished choices anymore. There is exactly one thing to say:
/// nothing was lost, and the next program is whatever the model writes.
/// Woken after a reply that arrived with no text at all — see
/// [`Runner::stopped_short`].
/// **The cause is asserted, and it was checked.** "The whole
/// completion went to reasoning" is a claim about why the reply was
/// empty, not an observation of it — so across 154 kept runs every one
/// of the 17 empty replies was looked at: all 17 carried a `Thinking`
/// part, several of them tens of kilobytes, against zero bytes of
/// prose and cells. `usage.reasoning` is no help here — this provider
/// reports it as 0 and the eval driver estimates it — but the
/// `Thinking` part is the same fact, observed directly.
///
/// A completion that arrives with neither content nor thinking would
/// make this sentence wrong. None has.
const EMPTY_REPLY_NOTICE: &str = "Your last reply arrived empty: the whole completion went to \
     reasoning and nothing was written, so nothing ran and nobody was told anything. Write the \
     reply this time — prose for what you are about to do, a ```js block for the doing.";

/// Woken after a reply that wrote a tool call in another harness's
/// syntax — see [`Runner::stopped_short`].
const FOREIGN_TOOL_CALL_NOTICE: &str = "Your last reply contained a tool call in a syntax this \
     harness does not read — `<tool_call>`, `<function=…>` or similar. It was not parsed. It \
     reached the person as literal text, the call never happened, and nothing ran. **Code here \
     runs only inside a fenced ```js block**: write `await tools.bash(\"…\")` in one, and the \
     block executes as you finish it. Write the block now; the work you meant to do is still \
     undone.";

/// Woken after a reply that carried on the branch's own work and then
/// ran nothing — see [`Runner::stopped_short`].
const STOPPED_SHORT_NOTICE: &str = "Your last reply ran nothing, and nobody had asked you \
     anything — so the work stopped where it was rather than finishing. If the task really is \
     done, say so with `finish(text)` inside a ```js block. Otherwise carry on from where you left \
     off.";

/// **What a reply with no block does, said while it would be a
/// mistake.**
///
/// Prose-only is the right shape for answering a question and the
/// wrong one once work is under way: it runs nothing and hands back to
/// the person, mid-task, silently. That is the discrimination
/// `stopped_short`'s third branch makes *after* the fact, at the cost
/// of a round trip. Said here it costs nothing and arrives before the
/// reply is written.
///
/// **"Since you were last spoken to", not "since anyone spoke".** The
/// model speaks constantly — every `tell`, every line of prose — so
/// the first wording was simply false from where it sits. The
/// condition is the last `Post`: someone speaking *to* the branch.
///
/// It names the consequence rather than the unit. There is no word for
/// "the stretch of replies since the person last spoke": `turn` is one
/// reply (the card's own usage, and `<end_of_turn>` in the model's
/// prior), `run` is a program. The card already says what happens
/// without naming it, and this borrows the phrase.
const WORK_UNDER_WAY: &str =
    "- A reply with no ```js block ends here: nothing runs, and the person speaks next.";

/// **The contract, restated where it is about to be acted on.**
///
/// The card says this in its first paragraph, and the first paragraph
/// of a 22 KB system prompt sits 11 KB from the end of a short request
/// and 28 KB from the end of a long one. The one precedent in this file
/// points the same way: a card sentence forbidding foreign tool-call
/// syntax was ignored 3 runs of 3, and the same words in the report
/// worked.
///
/// It earns the bytes because it is the rule whose violation is
/// **silent**. A reply that meant to act and emitted no cell runs
/// nothing, rests the branch (D4), and exits reporting success. Three
/// of nine tasks failed exactly this way on 2026-09-20: one wrote its
/// program into a ```text block, one wrote `tell(…); finish();` with no
/// fence at all, one quoted a fragment of its own console back. None of
/// them is a hard task, and 19% of that run's replies ran nothing
/// against 6% across the kept corpus.
///
/// Unconditional, and last. Every other line here is a fact about right
/// now; this is the standing shape of the thing being written, and it
/// is what the model should be holding as it starts to write.
const REPLY_IS_MARKDOWN: &str = "- Your reply is markdown and reaches the person as written. \
     Only a ```js block runs; ```text and unfenced code do nothing.";

/// The same rule under `AGENT2_RUN_PROGRAM`, where the program rides in
/// a tool call instead of a fence. The line exists because its
/// violation is silent either way — a reply that meant to act and made
/// no call runs nothing and rests the branch — so the transport that
/// changes *how* to act has to change this sentence with it, or the
/// strongest slot in the request is spent telling the model to do
/// something that no longer works.
const REPLY_IS_A_CALL: &str = "- Your prose reaches the person as written. \
     Only a run_program call runs; code written in the reply does nothing.";

/// [`WORK_UNDER_WAY`] under the same knob.
const WORK_UNDER_WAY_CALL: &str =
    "- A reply with no run_program call ends here: nothing runs, and the person speaks next.";

/// **A `finish()` that told nobody anything is not honoured**, and this
/// is the request that says so.
///
/// A branch that rests having said nothing to anybody is a run that
/// ended without a word — measured at 1 in 12 runs, and 4 in 12 once a
/// crossing-table line talked the models out of `tell`. The verb
/// carried the answer itself for a while, which made the pairing its
/// arity; now it carries nothing, so the pairing is enforced here
/// instead: the reply is simply not rested, and the branch gets one
/// more turn with this in front of it.
///
/// **In the tail, not as a `Post`.** It is true of exactly one request
/// and false the moment the next reply speaks, so it should not be on
/// the log forever — which is what `stopped_short`'s notice does, and
/// is why that one is re-read on every request after it fires.
const SILENT_FINISH: &str = "- Your last finish() said nothing to anybody, so it was not \
     honoured. tell() the answer, then finish().";

/// **The reply-shape line**, for the request where someone has just
/// asked something and the branch has no work of its own outstanding.
///
/// The card says this already (card.md: "A reply with no code blocks in
/// it rests the branch... That is the right shape for answering a
/// question"), and its five worked examples say the opposite by
/// example: every one opens with a ```js block. The measurement is
/// `evals/spoke_in_code.py` — across 320 kept logs, 25% of replies that
/// ran a program had that program make no tool call at all, and in the
/// one conversation in the corpus it was 5 of 6.
///
/// It rides the **tail** rather than the card because the card's last
/// line is not the request's last line: in a two-exchange session the
/// card ends 11 KB from the end, and in `try21.jsonl` before compaction
/// it was 28 KB. The one precedent in this file points the same way —
/// a card sentence forbidding foreign tool calls was ignored 3 runs of
/// 3, and the same words in the report worked.
///
/// **Off unless asked for** (`AGENT2_REPLY_SHAPE_TAIL=1`), because it
/// is a guess until an arm says otherwise and every request pays for
/// it uncached.
const REPLY_SHAPE_TAIL: &str = "Someone has asked you something and you have no work of your \
     own outstanding. If answering needs nothing run, answer in prose and stop — a reply with \
     no ```js block in it is a complete answer, and it rests the branch.";

/// **The card's "do not rehearse a block" line, at the recency end.**
///
/// The card says it already — "Thinking is not writing, and only
/// writing survives... So do not rehearse a block — write it" — and on
/// `sweep-8` against `deepseek-v4-flash` the model drafted the program
/// **six times** inside one 23 KB reasoning block before emitting it
/// once. That is the largest single component of the reasoning premium
/// over a tool loop (`docs/evidence/2026-09-21-code-vs-pi-sweep8.md`),
/// and it is a card line losing to the shape of the work, the same way
/// 15.2% of prose parts still copy back a `↓ history[N]` the card
/// forbids.
///
/// Same argument as [`REPLY_IS_MARKDOWN`]: a sentence in the first
/// paragraph of a 22 KB system prompt is a long way from where the
/// model starts thinking, and the tail is the one position that is not.
///
/// **On by default now**, and last; `AGENT2_NO_REHEARSAL_TAIL=0` turns
/// it off, any other value replaces the text.
///
/// It is carried rather than dropped despite the measurement below
/// finding *nothing* for it. 80 samples an arm against a frozen
/// context put the tail slot at p = 0.35 against the middle of the
/// tail, while the same sentence as the card's **first line** took
/// drafting from 89% of replies to 51%. The position that works is
/// the front, and this line is not what does the work.
///
/// What keeps it here is that one model is not the set of them. The
/// front-position result is `deepseek-v4-flash`, one task, one
/// captured state; a model that weights the end of its context more
/// heavily would be served by this line and costs 80 bytes to
/// insure. `arm-both` measured exactly that combination and was
/// indistinguishable from the front alone (p = 1.00 on the rate), so
/// the insurance is known to be free rather than assumed to be.
///
/// The measurement is the count of fenced drafts inside
/// `Part::Thinking`, over `sweep-8` on `deepseek-v4-flash`:
///
/// - no tail line: **6** drafts, 24,191 B of reasoning, 137 s
/// - a factual phrasing ("nothing written while reasoning is read back;
///   a block drafted there is written twice"): **1** draft, 9,101 B, 53 s
///
/// This is the imperative variant. The two are not obviously ordered:
/// the card already carries the imperative in bold ("do not rehearse a
/// block — write it") and was ignored six times, so force may be the
/// thing that already failed, and the explanation may be what worked.
/// One run per arm either way — this is a probe, not a result.
///
/// **And three levers have now missed it, at n=90 per arm** (2026-09-24,
/// `deepseek-v4-flash`, three task shapes held still: a rename-and-check,
/// a search, a read-and-explain).
///
/// - This line, already shipped: drafting still runs 19%.
/// - A paragraph saying what a trapped block costs — the diagnostic, the
///   line, the calls that landed, the rows they made, what was printed,
///   all of it true: **19% -> 16%, p=0.69**. The account it tested was
///   that rehearsal hedges against an unknown cost, since the card
///   describes failure only as obligation and never says what one costs.
///   Supplying the fact changed nothing, so that account is wrong.
/// - A paragraph ruling out Node, added for another reason: **raised**
///   drafting, 19% against 8%, p=0.047. Naming a thing at length appears
///   to put it in mind.
///
/// What it does track is the task — 33% on the edit, 10% on the search,
/// 13% on the read — not what the card says. Reading one run closely,
/// much of it is real work ("what if `note_json` already exists? That's
/// a real hazard"). Drafted replies cost 4-17x wall clock and 8-21x
/// completion tokens when they happen, which is a reason to care and not
/// yet a lever.
///
/// Earlier figures here were measured on documents whose tool manifest
/// declared one tool beside examples calling three others; those showed
/// 43-80% and are not comparable. Capture from real sessions.
const NO_REHEARSAL_TAIL: &str =
    "- Thinking is for reasoning! Never draft code blocks! One-shot them in the reply.";

/// Whether this reply tried to call a tool in another harness's syntax.
///
/// Deliberately a short list of shapes that are **actions**, not prose:
/// a model quoting one of these in a sentence would be a false
/// positive, and the cost of that is one extra turn, against a run
/// silently abandoning its task.
fn foreign_tool_call(text: &str) -> bool {
    const SHAPES: [&str; 7] = [
        "<tool_call>",
        "<function=",
        "<function_call",
        "<parameter=",
        "<invoke name=",
        "[TOOL_REQUEST]",
        "\"tool_calls\"",
    ];
    SHAPES.iter().any(|shape| text.contains(shape))
}

/// Woken after a reply that was a program with the fence left off —
/// see [`unfenced_program`].
const UNFENCED_PROGRAM_NOTICE: &str = "Your last reply was a program with no fence around it, so \
     nothing ran — the text went to the person as their answer instead. **Code runs only inside \
     a fenced ```js block.** Write the same lines again inside one; the work you meant to do is \
     still undone.";

/// **A reply that is a program with no fence around it.**
///
/// Not a misunderstanding — a lapse. `delegate-direct` produced this
/// as its entire reply, twice, on 2026-09-20:
///
/// ```text
/// tell("The closing balance in notes/ledger.md is 1200.");
/// finish();
/// ```
///
/// Nothing ran, the text went to the person as their answer, and the
/// branch rested reporting success. The tail says ```js is what runs
/// and it was on that very request; instructions do not catch lapses,
/// which is what this branch of `stopped_short` is for.
///
/// **Bounded hard to keep it a lapse-catcher, not a prose classifier.**
/// Every non-blank line must be a statement in this dialect, and there
/// must be no fence anywhere — a reply with a ```js block in it has
/// already run something, and a sentence that merely mentions `tell(`
/// keeps its other lines. That is why this reads whole lines rather
/// than searching for a substring: 1 prose segment in 1,686 across the
/// kept corpus matches, and it is the one this was written for.
fn unfenced_program(text: &str) -> bool {
    if text.contains("```") {
        return false;
    }
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return false;
    }
    lines.iter().all(|l| {
        l.ends_with(';')
            && (l.starts_with("tell(")
                || l.starts_with("finish(")
                || l.starts_with("ask(")
                || l.starts_with("return ")
                || l.starts_with("const ")
                || l.starts_with("let ")
                || l.starts_with("await tools.")
                || l.starts_with("history.")
                || l.starts_with("console.log("))
    })
}

const INTERRUPT_NOTICE: &str = "The user interrupted your program. It is paused at its last fuel slice — nothing \
     is lost, every completed call is already an artifact — and what happens next is \
     whatever program you write.";

/// Iteration cap for one `Tick`: each extra round requires a
/// settle-at-dispatch verb the harness could answer on the spot (a
/// `fetch_history` off the log, an `note_history`, a compaction edit)
/// to have unblocked the program, but a pathological program could chain
/// those forever. Hitting the cap is a slice boundary, not a failure —
/// the branch re-enqueues and carries on next tick.
const MAX_PUMP_ROUNDS: usize = 100;

/// A fixed budget for tests calling [`Runner::document`]/
/// [`Runner::render_messages_for_test`] — no production budget lives on
/// `Runner` anymore (`document::render`'s own doc comment: it is
/// per-agent, host-tracked configuration, supplied by the caller). Tests
/// here don't track one either, so they need a stand-in.
#[cfg(test)]
const TEST_BUDGET: usize = 64 * 1024;

pub enum StepInput {
    /// The assistant's turn (logged with its author; the program is
    /// compiled and run).
    LlmResponse(LlmTurn),
    /// Settled calls, in resolution order. One door for all call
    /// kinds: a host tool's result, a `Spawn`'s or `Fork`'s handle, or
    /// a `Send`'s delivery receipt/answer — routing is by the
    /// **variant** already in the log, so the machine needs no second
    /// input for subagents.
    ToolResults(Vec<ToolResult>),
    /// Run one VM slice of at most `fuel` instructions.
    Tick { fuel: u64 },
}

/// One settled call. It is named by its **logged `Call` event id** — the
/// log's own key, which is also what the artifact menu shows and what
/// `fetch_history(id)` takes, so there is no second id space to keep in step
/// with it.
pub struct ToolResult {
    pub call: EventId,
    /// `Err` rejects the program-side promise with the message.
    pub result: Result<serde_json::Value, String>,
}

#[derive(Debug)]
pub enum StepOutput {
    /// Send this to the LLM and feed the response back as `LlmResponse`.
    LlmRequest(LlmRequest),
    /// Execute these tools (any order/concurrency); feed back as
    /// `ToolResults` in completion order.
    ToolCalls(Vec<OutCall>),
    /// Create these agents; settle each `Spawn` with `{ agent }`. Named
    /// by the logged `Call::Spawn` event id — the host reads name,
    /// charter and allowlist from the log rather than a copy.
    Spawns(Vec<EventId>),
    /// Create these forks — a divergent branch inheriting the caller's
    /// history, unlike `Spawns` which roots a clean-room agent. Settle
    /// each with the fork's handle exactly as a `Spawn` is. Neither verb
    /// carries a first message: creating is not messaging
    /// (`22_ONE_VOCABULARY.md`), so the child is told what to do in a
    /// separate `tell`/`ask` afterwards.
    Forks(Vec<EventId>),
    /// Deliver these `Send`s. Each names a logged `Call::Send`, and the
    /// address, body and `expects_reply` all live there — the host reads
    /// the log rather than being handed a copy, which is the same
    /// by-reference discipline the `Post` itself follows.
    ///
    /// An `ask` stays pending until the recipient's `Answer` produces its
    /// `Result`; a `tell` is settled by its delivery receipt as soon as
    /// the `Post` lands.
    Sends(Vec<EventId>),
    /// A program called `answer(question, label, value)`: the `Answer`
    /// it logged, for the host to surface (e.g. mark the branch as
    /// having discharged an obligation). There is no other producer —
    /// under code mode a turn that answers nothing simply runs a
    /// program that calls nothing (DESIGN.md "No exception": a bare
    /// turn is not a distinguished shape anymore, just an ordinary
    /// program with no calls in it).
    Answered {
        question: EventId,
        value: serde_json::Value,
    },
    /// The VM wants another `Tick`.
    Working,
}

/// One completed assistant turn, as an LLM client produced it: the bare
/// program source it decided to run, and any reasoning trace beside it.
///
/// A client speaks *for* a branch; it does not decide **who acted**. So
/// `author` is not here — the harness stamps it when it logs the
/// `Message::Turn`, which is also what lets the user take a branch's turn
/// through the very same path (`take_turn`).
#[derive(Clone, Debug, Default)]
pub struct LlmTurn {
    pub source: String,
    pub thinking: Option<String>,
    /// Set by the transport (`host/deepseek.rs`, off the SSE
    /// `finish_reason`) when this completion was cut off by the token
    /// budget mid-program. Detection lives there; **enforcement lives
    /// here** (`apply_turn`), because only this file has the VM-stack
    /// context to log `Cause::Truncated` with a `Disposition` at the
    /// same time it decides whether to touch a suspended run. Per
    /// `Cause::Truncated`'s own doc in `types.rs`: never compile a
    /// truncated completion — checked before `interp::compile`, not
    /// after.
    pub truncated: bool,
    /// What the provider said this completion cost. `None` when the
    /// transport is scripted, or when an endpoint does not report it.
    pub usage: Option<crate::host::Usage>,
    /// `Transport::RunProgram` only, and only when the model said
    /// something beside its `run_program` call (or instead of one) —
    /// `host/deepseek.rs`'s `content` deltas, which in that container
    /// are *the message a person reads*, never the program. Always
    /// `None` under `Transport::Program`, where `content` already **is**
    /// `source` and there is nothing left over to carry here.
    /// `apply_turn` dispatches a `Some` value to the user through the
    /// same `Call::Send` a program's own `tell()` produces, so the log
    /// cannot tell a prose reply from a `tell` apart (`agent score`'s
    /// `tells`/`silent` fields have to agree on both transports).
    pub reply: Option<String>,
}

/// A rendered request's **ephemeral** half only. Under code mode the
/// system prompt, message history and tool surface are no longer built
/// here — `document.rs` renders those straight from the log and the
/// card (23_ONE_AGENT.md A4: "the card is the surface", no tool array
/// on any request). This is the one thing that genuinely can't move
/// there: presence and "what's still open" are *session* state, not
/// log content, so they can only ever be attached by whoever holds the
/// `Runner`.
#[derive(Debug, Default)]
pub struct LlmRequest {
    /// The trailing ephemeral line — see [`Runner::request_tail`].
    pub tail: Option<String>,
}

#[derive(Debug)]
pub struct OutCall {
    /// The `Call::Invoke` event this settles.
    pub call: EventId,
    pub name: String,
    /// Positional arguments as a JSON array.
    pub args: serde_json::Value,
}

/// One program execution: the run a `Turn` started.
struct Run {
    /// The `Turn` event id — the program block's stable key, carried
    /// through `resume`/`abandon` so status transitions stay attached
    /// to the same block across a continuation.
    program_id: EventId,
    vm: VM,
    /// `Transport::Notebook` only: the reply's remaining cells and the paused
    /// compilation feeding them into `vm`.
    ///
    /// **A reply is one run** (D7), so this is not a second run beside the
    /// first — it is the compile half of *this* one. `vm` stays where it always
    /// was and is stepped exactly as it always was; the only new thing is that
    /// on `StepResult::Paused` there is somewhere to go for more code.
    /// `None` under every other transport, which is what keeps those paths
    /// byte-for-byte what they were.
    notebook: Option<crate::notebook::Notebook>,
    /// How much of `vm.console_lines` has already been written to a
    /// `Console` event.
    ///
    /// **A run can hand back more than once.** A `raise` logs its
    /// `Console` and parks; the resumed tail logs another at the
    /// terminal — and `console_lines` is never cleared, so the second
    /// carried everything the first already had, and the second
    /// report's `### it printed` replayed output the model had read a
    /// reply earlier.
    ///
    /// Clamped on read: the buffer is a ring, and an overflow that
    /// drops entries makes any stored index approximate. It says so
    /// itself with `[… N lines dropped]`.
    console_logged: usize,
    /// Set once the program has ended itself with a top-level
    /// `return`, holding the value it returned.
    ///
    /// **The program is over before the reply is.** `return` ends the
    /// program where it stands, part-way through a notebook that may
    /// still be arriving — so the ending cannot be logged at that
    /// instant: the reply's own `ReplyEnd`, carrying what the
    /// completion cost, is still in the future. Instead the ending is
    /// *remembered* here and applied when the reply closes
    /// (`halt_if_ready`), which is also what makes "nothing after it
    /// runs" true of the cells that have not arrived yet as well as the
    /// instructions that have: while this is `Some`, the VM is never
    /// stepped again and every remaining piece is logged but not
    /// executed.
    returned: Option<Value>,
    /// The outbox that drained when the program halted — calls it
    /// issued and never awaited, held until the ending is applied and
    /// then classified exactly as an ordinary completion's are.
    unstarted: Vec<InvokeCall>,
}

/// How a program ended itself. The value each verb was given is
/// already accounted for by the time this is recorded — `finish`'s text
/// is a logged `Call::Send`, `stop`'s reason rides the `Handback` —
/// so this only has to say which of the two it was.
/// Fuel for a slice driven by an arriving chunk rather than a `Tick`.
const TICK_FUEL: u64 = 100_000;

/// How `Runner::resume` re-enters a suspended run — the *live* half of a
/// suspension, kept beside the phase.
///
/// The vocabulary a suspension is described in lives in the log, as
/// [`Cause`]: that is what a report renders from and what survives a
/// crash. This is deliberately **not** the same value. A `VMError` is not
/// serialisable and only a live VM can consume one, so a `Cause` cannot
/// carry it — and the `Cause` variants that never ran a VM
/// (`CompileFailed`, `Interrupted`) have no live half at all.
enum ResumeWith {
    /// `raise(name, payload)` — resume via `VM::resume_raise`.
    Raise,
    /// Trapped VM error — resume via `VM::resume_with` when the error
    /// is `PushValueThenContinue`.
    Trapped(VMError),
    /// A post arrived and the run suspended at its next fuel slice (rule
    /// B). The VM is simply **parked between slices** — nothing asked for
    /// a value and nothing failed — so `resume(...)` just carries on,
    /// ignoring whatever value it was given.
    Continue,
}

// `Running` is the only variant carrying data now that the parked run
// has moved to `Runner::parked`, so the size gap is stark — but there is
// exactly one `Phase` per branch, never a collection of them, and boxing
// it would put an allocation and a deref on the hottest match in the
// file to save nothing measurable.
#[allow(clippy::large_enum_variant)]
enum Phase {
    /// No in-flight request; waiting for a `UserTurn` (or `kickoff`).
    Idle,
    /// An `LlmRequest` is out; waiting for `LlmResponse`.
    AwaitingLlm,
    /// A program is executing (waiting for `Tick`/`ToolResults`).
    Running(Run),
}

/// **A program parked mid-flight**, frozen exactly where it stopped.
///
/// `Phase` used to carry one of these in a `Suspended(Run, ResumeWith)`
/// variant *and* keep the rest on a separate stack, which made "a
/// request is out" and "a frame is parked" the same slot. They are not
/// the same fact — a parked branch does have a request out, which is
/// what `prompt_suspended` (`host/mod.rs`) has always done and what
/// `d38c416` made true of `prompt_if_needed` too — and encoding both in
/// one enum is what let `self.phase = Phase::AwaitingLlm` drop a live
/// VM on the floor (`4898a80`). With the run out of `Phase`, that
/// assignment cannot reach one, so the guard that used to stand in
/// front of it is gone rather than remembered.
struct Parked {
    run: Run,
    resume_with: ResumeWith,
    /// The generation this run's still-pending calls were dispatched
    /// under, so [`Runner::resume`] can re-stamp them (see its own doc).
    generation: u64,
}

/// One call in flight, keyed by the `Call` event logged at dispatch.
/// The name, args and address live there, not here: the log is the
/// record, and the session state only has to route the settlement.
struct PendingCall {
    /// Where the program is waiting for this call — exactly one place,
    /// always. (`tools.agent`'s old spawn-then-ask sugar, two producers
    /// for one promise, is gone from the vocabulary: 23_ONE_AGENT.md A4,
    /// `agent` is not one of the closed verbs.)
    slot: Slot,
    /// Which run issued it: results from an abandoned run are still
    /// logged as artifacts (the physics happened) but not delivered.
    generation: u64,
}

/// The two ways a program can be waiting on a call, and the only
/// difference between them at settlement time.
enum Slot {
    /// A promise the program holds (`Instr::Invoke`): settling it wakes
    /// whoever awaits it, whenever they get round to it, and a failure
    /// is a rejection they may never look at.
    Promise(PromisePtr),
    /// A frame frozen mid-call (`Instr::Settle`): the value goes
    /// straight onto its stack and it carries on, and a failure is a
    /// throw at the call site. Nothing else in the VM runs until this
    /// is answered, so there is at most one of these per runner.
    ///
    /// **A settle slot can still outlive a dispatch pass.** `spawn`,
    /// `fork` and `list_agents` are answered by the host a round trip
    /// later (it creates the child / reads live branch status, neither
    /// of which this runner can do), and a `fetch_history` that
    /// re-attaches waits for a call that is genuinely in flight. What
    /// `Settle` promises is that the *frame* is still standing when the
    /// answer lands — not that the answer is instant.
    Settle,
}

/// The tag `resume(v)`/`abandon()` compile to, if this value is one.
fn decision_tag(v: &serde_json::Value) -> Option<&str> {
    v.get("__decision").and_then(|d| d.as_str())
}

/// What is known about the size of the next request, in tokens.
///
/// Three states and not an `Option`, because "no count yet" and "the
/// count just stopped being true" call for opposite behaviour, and
/// collapsing them cost a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counted {
    /// No reply has reported a `prompt_tokens` yet — a fresh session,
    /// or a provider that does not report usage at all. The byte
    /// budget is the only signal there is, so it decides.
    Never,
    /// A count arrived, and then a compaction shrank the document out
    /// from under it. There is nothing to test until the next reply
    /// brings one, and **nothing is the right answer**: falling back to
    /// bytes here is what turned one compaction into seven on the
    /// sweep-200 run of 2026-09-19. Clearing the count made the next
    /// check a byte check, the byte budget was the tighter of the two
    /// and fired at once, committing that batch cleared the count
    /// again — a loop that cost that run 121k prompt tokens against a
    /// 92k baseline, with the counted prompt never once above 13,677
    /// of its 43,008 threshold.
    Stale,
    /// The floor as of the last reply.
    Floor(u64),
}

pub struct Runner {
    pub spine: Spine,
    /// The innermost `Agent` root above this branch's leaf — who the
    /// branch is a conversation with. Resolved once at construction; the
    /// leaf moves, the agent does not.
    agent: EventId,
    /// This branch's root event, and its id: the `Agent` for an agent's
    /// first branch, a `Fork` for a divergent one. Live state is keyed by
    /// it, so two forks of one agent are two runners — which is the whole
    /// of "any number of leaves growing at once".
    branch: EventId,
    phase: Phase,
    generation: u64,
    pending: HashMap<EventId, PendingCall>,
    /// The dialect card (8_HARNESS Step 6), prepended to every system
    /// message ahead of the agent prompt (host-fed, registry-generated).
    dialect_card: String,
    /// The most recently finished/abandoned run's VM, kept so the
    /// debugger's sticky panes can show final state post-mortem
    /// (9_TUI Step 4). Never executed again.
    last_vm: Option<VM>,
    /// Program-block status transitions logged during the current
    /// `step`, drained by the host into `SessionEvent::ProgramStatus`
    /// (decision 5). Buffered (not a `StepOutput`) so two transitions in
    /// one step — a rewrite abandoning the old run as a new one starts —
    /// both surface, and so the sans-io output set is untouched.
    status_transitions: Vec<(EventId, ProgramStatus)>,
    /// Which wire container this branch's requests are rendered for.
    ///
    /// Session configuration, not branch state: it is the same for every
    /// runner in a session, and nothing ever logs it. It lives here
    /// because two places need it and neither can reach the other's —
    /// `document()` renders with it, and `apply_turn` needs it to read an
    /// *empty* completion correctly (under `RunProgram` an empty `source`
    /// means "no `run_program` call arrived", i.e. the model's final
    /// prose; under `Program` it is just an empty program).
    ///
    /// Defaulted at construction to `document::configured_transport()`
    /// — the process's one start-up read — and overridable through
    /// [`set_transport`]. Defaulted rather than passed in because there
    /// are six construction sites across the session loop and a seventh
    /// that forgot to set it would silently render every request under
    /// the wrong container; the env var would simply stop working, with
    /// nothing failing to say so. A test names the transport it means
    /// through [`set_transport`], the way it does `dialect_card` and
    /// `attached`.
    ///
    /// It was an `AGENT2_TRANSPORT` lookup on every *use* until that
    /// global's per-test mutation turned out to be a data race — see
    /// `document::Transport`. Reading it once at construction is the
    /// same value with none of the exposure.
    ///
    /// [`set_transport`]: Runner::set_transport
    /// `Transport::Notebook` only: **which generation** the run in `phase`
    /// is assembling, by the host's own `llm_epoch` for this branch.
    /// `None` between replies.
    ///
    /// Deliberately an identity and not a flag. It was a `bool` with one
    /// set site and one clear site, and the clear site was on the single
    /// happy path — the `LlmResponse` a *successful* generation produces.
    /// Four other ways a generation ends leave no `LlmResponse` at all:
    /// the epoch moves and `LlmDone` is dropped (which is exactly what
    /// D11's cancel-on-trap does), the provider errors, the user
    /// interrupts, or a handler abandons. Each left the flag set with a
    /// stale `Run`, and the next reply was fed into the *previous*
    /// reply's VM — so D10's "nothing survives to the next notebook"
    /// stopped being true at runtime. A model that wrote `const files` in
    /// two consecutive replies, which is ordinary and legal, got
    /// `files is already declared` and lost the run.
    ///
    /// An epoch cannot be forgotten the way a flag can: a chunk from a
    /// generation this is not assembling starts a fresh reply by
    /// construction, on every path at once — including the ones nobody
    /// remembered to add a reset to.
    streaming_epoch: Option<u64>,
    /// **How big the next request is, as far as counting can reach.**
    ///
    /// **Measured, never converted.** The document is sized in bytes
    /// and a context window is in tokens, and the obvious move — a
    /// bytes-per-token ratio — is a guess about a tokenizer this crate
    /// does not have, applied to every byte in the document. Content
    /// that tokenizes badly (hashes, minified source, base64) runs
    /// nearer 2 bytes per token than 4, and the guess is wrong in the
    /// direction that overflows.
    ///
    /// No guess is needed for either half of this. `usage.prompt` is
    /// the size of the request that was just sent, counted by the thing
    /// that will reject it — and the same trailer counts the completion
    /// that reply consisted of, which is in the document from now on.
    /// So this is their sum, less the reasoning tokens, because
    /// thinking is on the log and not in the document (`document.rs`
    /// drops `Part::Thinking`). A provider that does not break
    /// reasoning out reports it as zero and this over-counts, which is
    /// the safe direction.
    ///
    /// **A floor, not the size.** What it cannot see is what the cells
    /// added while the reply streamed — results, console, the report
    /// built around them — because nothing counts those until the next
    /// request is sent. That growth is what the headroom is for, and
    /// the only bound on it is that a single turn can add a great deal
    /// at once.
    next_prompt_floor: Counted,
    /// Whether the suspension the branch is parked on **falsified the
    /// text still arriving**. See
    /// [`notebook_cancels_generation`](Self::notebook_cancels_generation);
    /// set wherever a run parks, because that is the one place the
    /// cause is in hand.
    pause_falsifies_the_rest: bool,
    /// The reply this generation has produced so far, verbatim.
    ///
    /// **Kept here rather than read off the run**, because the run does
    /// not outlive the generation: a reply whose cells all finish before
    /// its completion ends leaves `Phase::Running` first, and the text
    /// went with it. Measured 2026-09-18 — 12 of 44 completions logged
    /// an empty `text`, and every one of the twelve had its outcome
    /// logged immediately before, which is exactly that race. An empty
    /// `text` makes `document::render` fall back to per-`Turn` grouping,
    /// so the model saw bare cell source with its prose and fences
    /// stripped — the bug this field exists to stop, on a quarter of
    /// replies.
    streaming_reply: String,
    /// The `Reply` (or `Restart`) whose parts and handbacks are being
    /// written (28). Allocated before a byte arrives, so everything in
    /// the reply can name it — and so a generation that produces nothing
    /// still leaves the record that it was attempted.
    pub(crate) reply_id: EventId,
    /// How the reply being assembled stopped arriving, when it was
    /// anything but `Finished`. Set by whoever cut it off; read once by
    /// `finish_notebook_generation`.
    reply_ended: Option<ReplyEnd>,
    /// A `resume(...)`/`abandon()` handed to `history.note`, waiting
    /// for this reply's run to end. See the `TOOL_NOTE_HISTORY` arm.
    pending_decision: Option<serde_json::Value>,
    /// Whether a client is attached to the session right now.
    ///
    /// Presence is a **per-request fact**, never branch state that
    /// anything else reads: it goes in the trailing ephemeral line and
    /// nowhere else, so attaching or detaching changes the next render
    /// and not one byte of the cached prefix. A branch that ran alone
    /// overnight is simply told, on its next request, that you are back.
    attached: bool,
    /// Event-id high-water mark at this branch's **last request render**
    /// — the whole of the session state the trigger rule needs, and what
    /// replaces any pending queue.
    ///
    /// Three things are one mechanism here. It is what stops a branch
    /// being prompted twice for the same cause; what stops a post that
    /// arrived *during* a generation from stealing the next bare turn's
    /// binding; and what makes a **fork born idle** — a fork's mark
    /// starts at its `Fork` root, so history before it never triggers a
    /// prompt and the fork speaks only when spoken to.
    shown: u64,
    /// The `remove_history`/`rewrite_history` calls a compaction
    /// handler has made so far, held until it returns.
    ///
    /// They are collected rather than applied one at a time because
    /// `compaction::compact` validates the **batch**: no duplicate
    /// target, every label matching, and the result actually under the
    /// threshold. A row removed the moment its call is dispatched could
    /// not be un-removed when a later call in the same program turns out
    /// to be wrong, and the log is append-only, so the batch is the unit
    /// that either commits or does not.
    ///
    /// True exactly while a compaction program is the thing being asked
    /// for. It no longer gates `history.remove`/`history.replace`: this
    /// tracks only *whether the harness asked*, so `compaction_if_needed`
    /// does not ask twice and `request_tail` knows to carry the
    /// directive.
    compaction_requested: bool,
    /// How full the conversation was when `compaction_if_needed` last
    /// looked: `(measured, limit, unit)`, the same three numbers the
    /// directive quotes. Kept so [`Runner::request_tail`] can say so
    /// without rendering the document a second time — it is *called*
    /// from the render, so it cannot start one.
    ///
    /// One turn stale at worst, which a readout can afford and a
    /// trigger could not.
    last_fullness: Option<(usize, usize, Measure)>,
    /// What the document rendered to when the trigger last looked, and
    /// this conversation's measured bytes-per-token from pairing that
    /// with the count the provider returned for it. See
    /// [`Runner::fullness`] for why a counted trigger needs them.
    last_rendered_bytes: Option<usize>,
    bytes_per_token: Option<f64>,
    /// History edits this program has queued, applied when it finishes.
    ///
    /// Any program may queue them, not only a compaction program. The
    /// verbs used to be refused outside one, on the reasoning that
    /// history is not a thing an ordinary program edits — but a program
    /// that has just read a 40 KB file and pulled one number out of it
    /// knows, right then, that the entry is not worth carrying, and it
    /// knows it better than a compaction program will later, with less
    /// to go on. Deferring that to a compaction it has to be asked for
    /// is the same recon-then-stop shape the card argues against
    /// everywhere else.
    ///
    /// Applied at the end of the program that queued them, never
    /// mid-run, so a program that traps or is abandoned halfway leaves
    /// the log exactly as it found it.
    pending_edits: Vec<crate::compaction::CompactionOp>,
    /// Whether the program currently running ended itself with
    /// `finish(text)` — checked and reset by `finish_program`, which is
    /// the only reader. Set on the way through `halt`, which is the
    /// single moment that fact becomes true; `finish_program`'s own
    /// comment is where the polarity this exists to flip is explained.
    finished: bool,
    /// Whether a reply that ran nothing and answered nobody wakes the
    /// branch — the third branch of [`stopped_short`](Self::stopped_short),
    /// which is where the measurement behind this lives.
    ///
    /// **A field, not an `env::var` at the point of use.** The same
    /// mistake was made once with the transport and undone for the
    /// reason stated there: a process-global read cannot be set two
    /// ways at once, so the tests for on and off could not run beside
    /// each other. Read from `AGENT2_STOPPED_SHORT_NOTICE` when the
    /// runner is built, and settable directly in a test.
    nudge_when_nothing_ran: bool,
    /// Whether the tail carries [`REPLY_SHAPE_TAIL`] on a request that
    /// is answering a post. A field for the same reason as the line
    /// above: an arm has to be settable per-runner, not per-process.
    reply_shape_tail: bool,
    /// The rehearsal-ban line this branch's tail carries, if any.
    ///
    /// **Text, not a flag, because the wording is the variable.** The
    /// question is which phrasing works, and a compiled-in string means
    /// a rebuild per arm — which in turn means running the arms in
    /// blocks, so an hour of provider drift lands entirely on one of
    /// them. `AGENT2_NO_REHEARSAL_TAIL=1` takes
    /// [`NO_REHEARSAL_TAIL`]; any other value is the line itself, so
    /// arms can be interleaved from one binary.
    no_rehearsal_tail: Option<String>,
    /// **Position, held apart from wording, because they are two
    /// questions.** Set, the line goes below [`REPLY_IS_MARKDOWN`],
    /// into the last slot of the request; `AGENT2_NO_REHEARSAL_LAST=0`
    /// puts it back fifth of nine, under the attachment status.
    ///
    /// Last by default, and the reason is weak on purpose: the slot
    /// makes no measurable difference (p = 0.35 over 80 samples an
    /// arm), and 18 task runs with [`REPLY_IS_MARKDOWN`] demoted out
    /// of it all passed. Given a free choice between two positions
    /// that measure the same, the end is the one the other evidence in
    /// this file points at.
    no_rehearsal_last: bool,
    /// The program rides in a `run_program` call rather than a fence
    /// (`AGENT2_RUN_PROGRAM`). Read here only to keep the two tail
    /// lines about response shape true; the wire format itself is
    /// `host/deepseek.rs`'s business and nothing between them knows.
    run_program: bool,
    /// The last reply called `finish()` and told nobody anything, so it
    /// was not rested. The next request's tail says so — see
    /// [`SILENT_FINISH`].
    finish_ignored: bool,
    /// Runs suspended **beneath** the one currently in `phase`, each
    /// frozen exactly where it stopped, oldest first popped last (a
    /// stack) — see `Phase::Suspended`'s own doc for why this, and not
    /// another slot on that enum, is where nesting lives. Alongside
    /// each `Run` sits the generation its own still-pending calls were
    /// dispatched under (`finish_program` re-stamps them on a
    /// successful resume, so an old in-flight exchange isn't read as
    /// issued by a VM that's since moved on).
    ///
    /// Pushed by `apply_turn` when a new program starts on top of a
    /// `Suspended` one — that new program might be the raise's own
    /// handler — and popped by `finish_program` once *that* program's
    /// own completion says what to do: a `{__decision: "resume"|
    /// "abandon", ..}` tag routes to [`Runner::resume`]/
    /// [`Runner::abandon`]; anything else is a genuine rewrite, and the
    /// frame is discarded (`crate::types::Handback::Abandoned`) instead.
    parked: Vec<Parked>,
}

enum SuspendCause {
    Raise {
        condition: String,
        payload: Option<Value>,
    },
    Trapped(VMError),
    /// Someone spoke to the running program.
    Posted(Vec<EventId>),
    /// `Transport::Notebook` only: a cell after the first did not compile.
    ///
    /// Distinct from the `CompileFailed` handback in `apply_turn`, which is
    /// the case where *no VM was ever built*. Here one was, and earlier cells
    /// have already run — their calls made, their rows appended. So this
    /// suspends the run that is under way rather than refusing a turn that
    /// never started, and the effects that happened stay in the log. The
    /// cause is still `crate::types::Handback::CellFailed`: what went wrong is that the
    /// model wrote a cell that does not compile, and that is what the repair
    /// loop needs to see.
    CellCompileFailed(String),
}

/// What one reply produced — see [`Runner::replies`].
struct ReplyShape {
    id: EventId,
    /// Wrote at least one cell, so there was a program.
    ran: bool,
    /// Put something in front of the person: a cell (which can `tell`),
    /// or prose, which reaches them as it is written.
    spoke: bool,
    /// Its run has logged a terminal or a pause.
    handed_back: bool,
    /// The reply's own text — prose and cells, as written.
    text: String,
    /// A `Post` landed between the previous reply and this one, so this
    /// reply is an **answer**. Without one it is a continuation of the
    /// branch's own work, and a continuation that runs nothing has
    /// stopped that work rather than finished it.
    answering: bool,
    /// Its program called `finish(text)`: the branch rested on purpose,
    /// and its outcome is owed nothing.
    finished: bool,
}

impl Runner {
    /// Root agent of a tree. `charter` is what the agent is for; the
    /// system prompt is assembled from it and the card and snapshotted on
    /// the `Agent` event.
    pub fn new_root(tree: &mut Tree, charter: impl Into<String>, card: &str) -> io::Result<Self> {
        let charter = charter.into();
        let system = assemble_system(card, &charter);
        let spine = tree.start_agent(
            None,
            None,
            charter,
            None,
            system,
            crate::card::seed_exemplars().to_vec(),
        )?;
        let mut state = Self::with_spine(tree, spine);
        state.dialect_card = card.to_owned();
        Ok(state)
    }

    /// A new agent rooted at `call_site` — the `Spawn` on the caller's
    /// branch (the host maps each `Spawns` id to one of these). The
    /// `Agent` is the agent's own root and outlives the caller, its
    /// program, and often the conversation that created it; `tools` sits
    /// here, on that root, because the registry enforces a child's
    /// allowlist from the agent itself and not from an event on its
    /// parent's branch.
    ///
    /// It carries **no question**. A spawn creates; asking is a separate
    /// act, and the first question arrives like every other — as a
    /// `Post` naming the `Send` that dispatched it. So a bare `spawn(...)`
    /// leaves an idle agent with nothing open, which is exactly what the
    /// driving rule wants: nothing to say, no request.
    pub fn new_agent(
        tree: &mut Tree,
        call_site: EventId,
        name: Option<String>,
        charter: impl Into<String>,
        tools: Option<Vec<String>>,
        card: &str,
    ) -> io::Result<Self> {
        let charter = charter.into();
        let system = assemble_system(card, &charter);
        let spine = tree.start_agent(
            Some(call_site),
            name,
            charter,
            tools,
            system,
            crate::card::seed_exemplars().to_vec(),
        )?;
        let mut state = Self::with_spine(tree, spine);
        state.dialect_card = card.to_owned();
        Ok(state)
    }

    /// Resume an existing spine: a re-opened log, a fork, a re-anchor.
    pub fn with_spine(tree: &Tree, spine: Spine) -> Self {
        let agent = tree.enclosing_agent(spine.leaf_id).unwrap_or(spine.leaf_id);
        let branch = tree.branch_of(spine.leaf_id).unwrap_or(spine.leaf_id);
        let leaf = spine.leaf_id;
        Runner {
            compaction_requested: false,
            last_fullness: None,
            last_rendered_bytes: None,
            bytes_per_token: None,
            pending_edits: Vec::new(),
            finished: false,
            // Off unless asked for: of the thirteen times this fired
            // across 382 kept runs, twelve woke a branch whose next
            // reply was `done();`. See `stopped_short`.
            nudge_when_nothing_ran: std::env::var("AGENT2_STOPPED_SHORT_NOTICE")
                .is_ok_and(|v| v != "0"),
            reply_shape_tail: std::env::var("AGENT2_REPLY_SHAPE_TAIL").is_ok_and(|v| v != "0"),
            no_rehearsal_tail: match std::env::var("AGENT2_NO_REHEARSAL_TAIL") {
                Ok(v) if v == "0" => None,
                Ok(v) if v == "1" => Some(NO_REHEARSAL_TAIL.to_owned()),
                Ok(v) => Some(v),
                Err(_) => Some(NO_REHEARSAL_TAIL.to_owned()),
            },
            no_rehearsal_last: std::env::var("AGENT2_NO_REHEARSAL_LAST").map_or(true, |v| v != "0"),
            run_program: std::env::var("AGENT2_RUN_PROGRAM").is_ok_and(|v| v != "0"),
            finish_ignored: false,
            spine,
            agent,
            branch,
            phase: Phase::Idle,
            generation: 0,
            pending: HashMap::new(),
            dialect_card: String::new(),
            last_vm: None,
            status_transitions: Vec::new(),
            streaming_epoch: None,
            next_prompt_floor: Counted::Never,
            pause_falsifies_the_rest: false,
            streaming_reply: String::new(),
            pending_decision: None,
            reply_id: EventId::new(1),
            reply_ended: None,
            attached: false,
            // A branch handed to a fresh `Runner` has said nothing to
            // *this* session's LLM and is owed no prompt for its
            // history: a fork born at its `Fork` root speaks only when
            // spoken to, and a re-opened branch waits to be addressed.
            shown: leaf.as_u64(),
            parked: Vec::new(),
        }
    }

    /// The dialect card rendered as the root of the system message
    /// (the host generates it from the tool registry). It seeds the
    /// snapshot on a *new* agent's root; an existing branch's system
    /// prompt is the snapshot and never re-derived.
    pub fn set_dialect_card(&mut self, card: String) {
        self.dialect_card = card;
    }

    /// This branch's agent — the innermost `Agent` root on its path.
    pub fn agent_id(&self) -> EventId {
        self.agent
    }

    /// This branch's id: its root event. Two forks of one agent share
    /// `agent_id` and differ here, which is why live state is keyed by
    /// this and not by the agent.
    pub fn branch_id(&self) -> EventId {
        self.branch
    }

    /// Tell this branch whether anyone is attached. It changes the next
    /// request's trailing line and nothing else — no logged event, no
    /// prefix byte, no wake.
    pub fn set_attached(&mut self, attached: bool) {
        self.attached = attached;
    }

    /// Drain the program-status transitions logged during the just-run
    /// `step`; the host turns each into a `SessionEvent::ProgramStatus`.
    pub fn take_status_transitions(&mut self) -> Vec<(EventId, ProgramStatus)> {
        std::mem::take(&mut self.status_transitions)
    }

    /// Record a program-block status transition for the host to surface.
    fn note_status(&mut self, program: EventId, status: ProgramStatus) {
        self.status_transitions.push((program, status));
    }

    /// Whether the agent can accept a `UserTurn` right now.
    pub fn is_idle(&self) -> bool {
        matches!(self.phase, Phase::Idle) && self.parked.is_empty()
    }

    /// One-word phase description for agent lists / status lines.
    pub fn status(&self) -> &'static str {
        // Parked outranks idle-or-waiting: a branch holding a frame is
        // suspended whether or not it also has a request out, which is
        // the reading every caller had when `Phase` carried the run.
        if matches!(self.phase, Phase::Running(_)) {
            return "running";
        }
        if !self.parked.is_empty() {
            return "suspended";
        }
        match self.phase {
            Phase::Idle => "idle",
            Phase::AwaitingLlm => "awaiting llm",
            Phase::Running(_) => unreachable!("returned above"),
        }
    }

    /// Posts this branch owes an answer to, oldest first.
    pub fn open(&self) -> &[EventId] {
        &self.spine.context().open
    }

    /// **The trigger rule**, and the whole of it:
    ///
    /// > Prompt iff the branch holds no VM and there is a rendered
    /// > `Message` other than a `Turn` with id > `shown` — or the newest
    /// > `Turn`'s run has an outcome that has not been shown yet.
    ///
    /// - Every request has a **cause event**. The LLM is never prompted
    ///   "just because", and never twice for the same thing: `shown`
    ///   advances at each render.
    /// - `finish_program`/`suspend`/a `CompileFailed`/`Truncated`
    ///   handback each render *unconditionally* (bypassing this rule
    ///   entirely) rather than going through it, so this rule's own
    ///   outcome clause only ever matters for **reconciliation** — a
    ///   freshly reconstructed `Runner` (`with_spine`) starts `shown` at
    ///   its leaf, i.e. "everything already shown", which is wrong for a
    ///   branch a crash caught between logging an outcome and the
    ///   `render_request` call right after it. `document.rs::render`
    ///   derives the report straight off the `Return`/`Condition` event
    ///   at render time — there is no second logged event to check for.
    ///
    /// There is no interactive/autonomous split here, deliberately: this
    /// design already deleted one such flag (`is_root`), and a mode would
    /// resurrect it under a new name. What makes a branch autonomous is
    /// **the program still running**, not extra prompting.
    pub fn needs_prompt(&self, tree: &Tree) -> bool {
        // **Running and suspended are not the same thing.** A running
        // program is executing, and rule B delivers a post at its next
        // fuel slice; awaiting an LLM, a request is out and everything
        // logged since rides the next one. Both are right to decline.
        //
        // A parked run is neither. Nothing is executing, and the one
        // prompt a suspension is owed was sent by the host when it
        // parked (`prompt_suspended`). If the completion that prompt
        // earned never opens a reply of its own — all reasoning, no
        // text, so `notebook_stream` is never called and no `Reply` is
        // logged — the branch is left `Suspended` with nothing in
        // flight and every wake closed: `prompt_suspended` needs a
        // fresh transition, `open_notebook_reply` needs a text chunk,
        // and this returned `false` for anything but `Idle`. Posts were
        // logged and ignored, for good.
        //
        // An unseen post is the safe discriminator: rendering a request
        // advances `shown` (`render_request`), so a post that arrived
        // before the one-shot rides it and only a genuinely later one
        // asks again. Seen live on 2026-09-21; reproduced by
        // `a_branch_parked_mid_stream_survives_a_silent_completion`.
        match &self.phase {
            // Nothing running, nothing parked, nothing in flight: the
            // ordinary rules below decide.
            Phase::Idle if self.parked.is_empty() => {}
            // **A parked branch's wake belongs to the host.**
            // `prompt_suspended` sends the one prompt a suspension is
            // owed, and the branch reads as `Idle` now that the frame
            // lives on `parked` rather than in `phase` — so without
            // this it would fall through to the rules below, see the
            // suspension's own `Handback` as an unshown outcome, and
            // ask for a *second* generation over the host's. Two
            // spawns, an epoch bump between them, and the completion
            // that mattered dropped on arrival.
            //
            // An unseen post is still a cause: that is `d38c416`, the
            // fix for a branch going deaf after a completion that said
            // nothing, and it is the same rule it always was — only
            // keyed on the frame instead of on the phase.
            Phase::Idle => return !self.unseen_demanding_posts(tree).is_empty(),
            _ => return false,
        }
        // Any unseen `Post` is a cause — including one that arrived
        // during a generation, which the turn that just landed could not
        // have answered (its binding was fixed at `shown`), and including
        // a tell, which owes no answer but must still be seen.
        if !self.unseen_demanding_posts(tree).is_empty() {
            return true;
        }
        // **A reply that stopped short is not a rest.** The note
        // itself is logged by `prompt_if_needed`, which is where the
        // branch has a `&mut Tree` to log it into; this only has to
        // agree that there is one.
        if self.stopped_short(tree).is_some() {
            return true;
        }
        // The crash-recovery clause, `shown`-guarded like everything
        // else this rule checks: a genuinely new (unseen) outcome on the
        // most recent `Turn` is a cause even with no `Post` to find.
        self.last_turn_outcome(tree)
            .is_some_and(|id| id.as_u64() > self.shown)
    }

    /// Unseen posts that are a **reason to answer**, as opposed to ones
    /// that are merely new.
    ///
    /// A harness post that expects no reply is bookkeeping: the
    /// settlement notice for a call nobody awaited says, in its own
    /// words, "Nothing is owed in reply". It used to wake a branch that
    /// had already rested, and the branch spent a completion agreeing —
    /// `intr3` on 2026-09-21: the person said "actually never mind,
    /// forget it", the model said "Alright, forgot about it", the
    /// cancelled `bash` timed out thirty seconds later, and the notice
    /// bought one more turn saying "Understood, I've discarded the
    /// result." Two replies to a person who had said to stop.
    ///
    /// Nothing is lost by not waking: the notice is on the log and in
    /// the document the moment anything else asks for a turn.
    ///
    /// **Only the wake is narrowed, not rule B.** A post still suspends
    /// a *running* program at its next fuel slice whoever wrote it —
    /// `INTERRUPT_NOTICE` is a harness post expecting no reply, and
    /// being a cause is its entire purpose.
    fn unseen_demanding_posts(&self, tree: &Tree) -> Vec<EventId> {
        self.agent_segment(tree)
            .iter()
            .filter(|e| e.id.as_u64() > self.shown)
            .filter(|e| match &e.payload {
                EventPayload::Post {
                    from: Author::Harness,
                    origin,
                } => tree
                    .resolve(origin)
                    .direct()
                    .is_some_and(|(_, _, expects_reply)| expects_reply),
                EventPayload::Post { .. } => true,
                _ => false,
            })
            .map(|e| e.id)
            .collect()
    }

    /// Posts logged on this branch that its LLM has not been shown — the
    /// only thing `shown` is compared against, and what makes a post to a
    /// running program a condition at the next fuel slice (rule B).
    fn unseen_posts(&self, tree: &Tree) -> Vec<EventId> {
        self.agent_segment(tree)
            .iter()
            .filter(|e| e.id.as_u64() > self.shown)
            .filter(|e| matches!(e.payload, EventPayload::Post { .. }))
            .map(|e| e.id)
            .collect()
    }

    /// What each reply on this branch produced, oldest first — the one
    /// fold the resting rule is decided from.
    ///
    /// Three outcomes, and they are not the same question asked twice:
    ///
    /// - **ran a cell** — there is a program, and its `Handback` is a
    ///   cause for the next request.
    /// - **spoke only** — prose reached the person and no cell ran.
    ///   That is D4's implicit `finish(text)`: the model said its piece and
    ///   the next thing to happen is whatever the person says.
    /// - **said nothing** — no prose, no cell, no `tell`. Not an
    ///   answer, and not a rest: nobody was told anything, so a branch
    ///   that rests here has abandoned the task in silence.
    ///
    /// The third used to be folded into the second, because "no cells"
    /// was the whole test. It cost a live run on 2026-09-19: the
    /// provider spent the entire completion on `reasoning_content` and
    /// returned empty `content`, the branch rested, and the harness
    /// exited 0 with the task untouched.
    fn replies(&self, tree: &Tree) -> Vec<ReplyShape> {
        let segment = self.agent_segment(tree);
        let mut out: Vec<ReplyShape> = Vec::new();
        let mut posted_since = false;
        for event in &segment {
            match &event.payload {
                EventPayload::Reply | EventPayload::Restart => {
                    out.push(ReplyShape {
                        id: event.id,
                        ran: false,
                        spoke: false,
                        handed_back: false,
                        text: String::new(),
                        answering: std::mem::take(&mut posted_since),
                        finished: false,
                    });
                }
                EventPayload::Post { .. } => posted_since = true,
                EventPayload::Part { part, .. } => {
                    if let Some(last) = out.last_mut() {
                        match part {
                            crate::types::Part::Cell(t) => {
                                last.ran = true;
                                last.spoke = true;
                                last.text.push_str(t);
                            }
                            crate::types::Part::Prose(t) => {
                                last.spoke |= !t.trim().is_empty();
                                last.text.push_str(t);
                            }
                            crate::types::Part::Thinking(_) => {}
                        }
                    }
                }
                EventPayload::Handback { how, .. } => {
                    if let Some(last) = out.last_mut() {
                        last.handed_back = true;
                        last.finished |=
                            matches!(how, crate::types::Handback::Completed { rested: true, .. });
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// The most recent reply on this path **that ran something** and
    /// has since logged a `Handback` — regardless of `shown`. The two
    /// callers differ only in whether they apply that guard themselves:
    /// `needs_prompt` does (an already-shown outcome is not a fresh
    /// cause), `unrendered_cause` deliberately does not (reconciliation
    /// needs the fact independent of a `shown` a crash may have left
    /// pointing past it).
    ///
    /// **A reply that called `finish(text)` is owed nothing.** It is the
    /// one terminal that says the branch rested on purpose, so it is
    /// not an outcome waiting to be reported — and treating it as one
    /// meant that reopening a finished log woke it for a further reply
    /// (`try21.jsonl`: #50 finished, #61 said so again). Before
    /// `Handback::Finished` existed the log could not tell the two
    /// apart, which is why this went unnoticed until a live session
    /// was closed and reopened.
    fn last_turn_outcome(&self, tree: &Tree) -> Option<EventId> {
        let replies = self.replies(tree);
        let last = replies.last()?;
        (last.ran && last.handed_back && !last.finished).then_some(last.id)
    }

    /// **The last reply stopped short of doing anything**, and the one
    /// line that says so — the harness note the branch is woken with.
    /// `None` when the reply ended on purpose and the branch should
    /// rest.
    ///
    /// D4 says a reply with no cells is an implicit `finish(text)`: the model
    /// said its piece and the next thing to happen is whatever the
    /// person says. That is right for an **answer** and wrong for the
    /// two cases below, which the card already distinguishes in prose
    /// ("it is the wrong one for a task you meant to carry on with,
    /// where a reply that ends without running anything has stopped the
    /// work without saying so") and which the harness did not:
    ///
    /// - **A tool call in someone else's syntax.** Measured on
    ///   `qwen3.8-flash`, 2026-09-19: it wrote `<tool_call><function=bash>…`
    ///   as prose in four runs out of four, once with the task's whole
    ///   answer in it. That is not an answer, it is an action that
    ///   missed. A card sentence forbidding it was tried first and
    ///   ignored 3/3 — the model complied on one reply and drifted back
    ///   on the next — so it is said here instead, where it arrives at
    ///   the moment of the mistake rather than 16 KB earlier.
    /// - **A continuation that ran nothing.** No post prompted this
    ///   reply, so nobody asked it anything; it was carrying on its own
    ///   work and stopped without a `finish(text)`.
    ///
    /// A reply that *was* answering a post and ran nothing is left
    /// alone. That is `plain-question`'s whole shape, and a follow-up
    /// answered in prose mid-task is the same shape.
    ///
    /// **Bounded at one retry**, counting every trailing reply that ran
    /// nothing: twice in a row is a branch that cannot do this, and a
    /// third ask is a loop that bills for itself.
    fn stopped_short(&self, tree: &Tree) -> Option<String> {
        let replies = self.replies(tree);
        let last = replies.last()?;
        if last.ran || !last.handed_back {
            return None;
        }
        let trailing = replies
            .iter()
            .rev()
            .take_while(|r| !r.ran && r.handed_back)
            .count();
        if trailing != 1 {
            return None;
        }
        if !last.spoke {
            return Some(EMPTY_REPLY_NOTICE.to_owned());
        }
        if foreign_tool_call(&last.text) {
            return Some(FOREIGN_TOOL_CALL_NOTICE.to_owned());
        }
        if unfenced_program(&last.text) {
            return Some(UNFENCED_PROGRAM_NOTICE.to_owned());
        }
        // **Off by default, and the code stays.** Of the thirteen times
        // this last branch fired across 382 kept runs, twelve woke a
        // branch whose next reply was, verbatim, `done();` — a model
        // that had finished the work and had not said so in the one
        // syntax that rests a branch. One round trip each, and those
        // runs would have rested with the task complete and passed
        // anyway. The thirteenth was a genuine rescue
        // (`head3/sweep-40-020719`: the woken reply went on to
        // `read_file → parse_errors → replace_file → bash`).
        //
        // So the branch is right about what it does and wrong about
        // what it is for. What it mostly catches is a gap in the
        // *ending vocabulary* — every exemplar that finishes does so
        // after doing work, and none finishes after simply concluding —
        // and prodding is the wrong place to fix that.
        //
        // `AGENT2_STOPPED_SHORT_NOTICE=1` brings it back, because one
        // rescue in thirteen is not nothing and this is a number to
        // re-measure rather than a question to settle by comment. The
        // two branches above are untouched: an empty completion
        // (2026-09-19: the whole completion went to reasoning, the
        // branch rested, the harness exited 0 with the task untouched)
        // and a tool call in a foreign syntax (4 runs in 4 on
        // qwen3.8-flash, one carrying the task's whole answer) are real
        // strandings, and both were measured as such.
        if !self.nudge_when_nothing_ran {
            return None;
        }
        (!last.answering).then(|| STOPPED_SHORT_NOTICE.to_owned())
    }

    /// **Reconciliation's half of the trigger rule**: forget having shown
    /// anything from `cause` onward, so the rule can fire for a cause the
    /// crash swallowed.
    ///
    /// A fresh `Runner` starts `shown` at its leaf — a re-opened branch
    /// waits to be spoken to, and a fork is born idle by the same line.
    /// That is right for every branch the reconciliation table says
    /// nothing about, and wrong for the two rows it does speak to, which
    /// is what this lowers it for.
    pub fn owe_prompt(&mut self, cause: EventId) {
        self.shown = self.shown.min(cause.as_u64().saturating_sub(1));
    }

    /// The earliest event on this branch the trigger rule would call a
    /// cause, **ignoring `shown`** — what reconciliation lowers the mark
    /// to when a crash swallowed the request a run's outcome was owed.
    ///
    /// Two, matching the table's two prompting rows: a post this branch
    /// owes an answer to, and a run whose outcome was never rendered.
    pub fn unrendered_cause(&self, tree: &Tree) -> Option<EventId> {
        let owed = self.open().first().copied();
        let unreported = self.last_turn_outcome(tree);
        match (owed, unreported) {
            (Some(a), Some(b)) => Some(if a.as_u64() <= b.as_u64() { a } else { b }),
            (a, b) => a.or(b),
        }
    }

    /// Render a request if the trigger rule says to — the public door
    /// reconciliation wakes a re-hydrated branch through, so "never woken
    /// without a cause" still holds in one place.
    pub fn wake(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        self.prompt_if_needed(tree)
    }

    /// The branch that owes `question`, when it is not this one: an
    /// unanswered post on this path but **before** this branch's root.
    /// That is exactly the pre-fork case, and the answer is the branch
    /// whose root it sits at or after.
    ///
    /// Used by the `answer` dispatch arm to explain a rejected call —
    /// this is also the *only* enforcement of the fork-obligations rule
    /// now: a fork inherits history, not obligations, so a pre-fork post
    /// is not on its `open` list, and the rejection says whose it is
    /// rather than leaving it as something the model must have absorbed.
    fn owning_branch(&self, tree: &Tree, question: EventId) -> Option<EventId> {
        let path = tree.path_events(self.spine.leaf_id);
        let at = path.iter().position(|e| e.id == question)?;
        let root = tree.branch_of(self.spine.leaf_id)?;
        let root_at = path.iter().position(|e| e.id == root)?;
        if at >= root_at {
            return None; // on this branch; not the fork case
        }
        // Unanswered anywhere on this path, and expecting a reply.
        let expects_reply = match &path[at].payload {
            EventPayload::Post { origin, .. } => {
                matches!(tree.resolve(origin).direct(), Some((_, _, true)))
            }
            _ => return None,
        };
        let answered = path.iter().any(
            |e| matches!(&e.payload, EventPayload::Answer { question: q, .. } if *q == question),
        );
        if !expects_reply || answered {
            return None;
        }
        tree.branch_of(path[at].id)
    }

    /// The VM the debugger TUI renders from (9_TUI dec. 4): the live
    /// one while a program runs or is suspended, else the last run's
    /// final state (sticky post-mortem panes).
    pub fn vm(&self) -> Option<&VM> {
        match self.run_ref() {
            Some(run) => Some(&run.vm),
            None => self.last_vm.as_ref(),
        }
    }

    /// Whether `vm()` is the live, executing program (vs a post-mortem
    /// snapshot).
    pub fn vm_is_live(&self) -> bool {
        self.run_ref().is_some()
    }

    /// Start the conversation without a user turn — how child contexts
    /// begin (their input arrived in `Agent`).
    pub fn kickoff(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        let _ = tree;
        assert!(matches!(self.phase, Phase::Idle), "kickoff on a busy agent");
        self.phase = Phase::AwaitingLlm;
        Ok(vec![self.render_request(tree)])
    }

    pub fn step(&mut self, tree: &mut Tree, input: StepInput) -> io::Result<Vec<StepOutput>> {
        match input {
            StepInput::LlmResponse(turn) => {
                // A notebook fed in as it streamed has already logged every
                // piece of this reply; the final message only says the
                // completion is over (and whether it was cut off).
                if let Some(out) = self.notebook_stream_end(
                    tree,
                    turn.truncated,
                    turn.usage,
                    turn.thinking.clone(),
                )? {
                    return Ok(out);
                }
                let author = Author::Agent(self.agent_id());
                self.apply_turn(
                    tree,
                    turn.source,
                    turn.thinking,
                    turn.truncated,
                    author,
                    turn.usage,
                    turn.reply,
                )
            }
            StepInput::ToolResults(batch) => self.on_tool_results(tree, batch),
            StepInput::Tick { fuel } => self.on_tick(tree, fuel),
        }
    }

    /// Deliver a message into this branch — **rule A**: a post is logged
    /// on the branch it is delivered into, whoever authored it. The user,
    /// another agent's `Send`, and a harness notice all come through this
    /// one door; who is speaking is `from`, and where the body lives is
    /// `origin`.
    ///
    /// This is for **ordinary conversation** — someone (or something)
    /// speaking to the branch — and is a different door from
    /// [`take_turn`]: a `Post` here is *heard*, and the trigger rule
    /// decides whether it starts a fresh turn; a `Turn` there is the
    /// branch *acting*, always compiled and run. Plain human chat is a
    /// `Post`; a restart the user authors by hand is a `Turn`.
    ///
    /// Returns the `Post`'s id — a `tell`'s delivery receipt names it —
    /// beside what the branch does next: an idle branch starts a turn, a
    /// busy one has the post on its path for its next request (rule B's
    /// suspend-at-the-next-slice is B3).
    pub fn deliver(
        &mut self,
        tree: &mut Tree,
        from: Author,
        origin: Origin,
    ) -> io::Result<(EventId, Vec<StepOutput>)> {
        let post = tree.append(&mut self.spine, EventPayload::Post { from, origin })?;
        // Logged on arrival either way — visible and crash-safe before
        // anything decides what to do about it. Whether it starts a turn
        // *now* is the trigger rule's call and nothing else's.
        //
        // **Through the one door**, not a copy of it. This used to
        // inline `needs_prompt` + `render_request` and so skipped
        // everything else `prompt_if_needed` does on the way: the
        // compaction check (so a post arriving on an idle branch with an
        // over-budget document sent the over-budget request rather than
        // asking for a handler) and the `stopped_short` notice added on
        // 2026-09-19 (logged on one path and not the other). Two places
        // that have to agree about when a branch wakes, and they had
        // already stopped agreeing twice.
        let woken = self.prompt_if_needed(tree)?;
        if !woken.is_empty() {
            return Ok((post, woken));
        }
        // A running program's next fuel slice is where rule B delivers,
        // and a **parked** program has no next slice of its own — it is
        // waiting on a call, burning no fuel. So ask for one: that is
        // what turns "at the next slice" from a hope into the guarantee,
        // and it is why a parent awaiting its child cannot deadlock.
        if matches!(self.phase, Phase::Running(_)) {
            return Ok((post, vec![StepOutput::Working]));
        }
        Ok((post, Vec::new()))
    }

    /// The request this branch was waiting on **failed**. Nothing is
    /// logged — that turn did not happen, the same as a cancellation —
    /// and the branch drops back to idle so it can be spoken to again.
    pub fn abandon_request(&mut self) {
        if matches!(self.phase, Phase::AwaitingLlm) {
            self.phase = Phase::Idle;
        }
    }

    /// **`Interrupt`** — the one override on rule B's "next safe point".
    ///
    /// The cancellation of an in-flight generation is the *session's*
    /// half (nothing is logged: from the API's view that turn did not
    /// happen); this is what the branch does once it is cancelled.
    pub fn interrupt(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        match &self.phase {
            // Nothing is in flight and nothing is owed.
            Phase::Idle => Ok(Vec::new()),
            // The cancelled turn is gone. Back to idle, where the trigger
            // rule decides afresh: the post that arrived mid-generation
            // now starts a fresh turn.
            Phase::AwaitingLlm => {
                self.phase = Phase::Idle;
                self.prompt_if_needed(tree)
            }
            // Rule B delivers to a running program at its next fuel
            // slice, so an interrupt's job is to **be a cause** for one.
            // If nothing is unseen, the harness says so itself — in a
            // post, which is a wake with an event you can name in the
            // log, and never a bare "you stopped, is there more?".
            Phase::Running(_) => {
                if !self.unseen_posts(tree).is_empty() {
                    return Ok(vec![StepOutput::Working]);
                }
                let origin = Origin::Direct {
                    text: INTERRUPT_NOTICE.to_owned(),
                    input: serde_json::Value::Null,
                    options: Vec::new(),
                    expects_reply: false,
                };
                let (_, out) = self.deliver(tree, Author::Harness, origin)?;
                Ok(out)
            }
        }
    }

    // ── turns ────────────────────────────────────────────────────────

    /// **The user takes this branch's turn** — the handler hierarchy's
    /// outermost layer made literal, and now the *only* way a user
    /// restart works: there is no `UserCall` shape distinct from an
    /// LLM's turn anymore. `source` is either hand-typed text (the `e`
    /// gesture — which compiles and runs like anything else, and traps
    /// like anything else if it isn't valid JS) or a synthesized
    /// `resume(...)`/`answer(...)` expression (`v` and the answer
    /// gesture), matching `Message::Turn`'s own doc in `types.rs`.
    pub fn take_turn(&mut self, tree: &mut Tree, source: String) -> io::Result<Vec<StepOutput>> {
        // **A hand-typed turn is a cell.** The gestures that reach here
        // synthesize bare JavaScript — `resume(...)`, `answer(...)`, or
        // whatever the person typed at `e` — and a reply is markdown
        // now, where bare JavaScript is prose. Unfenced, the `v` gesture
        // resumed nothing and the `e` gesture ran nothing: the branch
        // simply rested, which is what a reply with no cells does.
        //
        // Markdown that already carries a fence is passed through, so a
        // person who wants prose, or two cells, can write them.
        let source = if source.contains("```") {
            source
        } else {
            format!("```js\n{source}\n```\n")
        };
        self.apply_turn(tree, source, None, false, Author::User, None, None)
    }

    /// Log one turn — the whole of what the branch itself just said —
    /// and start it running. `author` is the only difference between the
    /// LLM's turn and the user's: it renders as an assistant message
    /// either way, because the **branch** acted.
    ///
    /// A prior suspension is not discarded until this new program is
    /// known to actually run: the physics already happened (in-flight
    /// calls stay pending, their results still land as artifacts when
    /// they arrive), so the only thing genuinely at risk of being thrown
    /// away is the *VM*, and a program that fails to compile shouldn't
    /// cost you that.
    #[allow(clippy::too_many_arguments)]
    fn apply_turn(
        &mut self,
        tree: &mut Tree,
        source: String,
        thinking: Option<String>,
        truncated: bool,
        author: Author,
        usage: Option<crate::host::Usage>,
        _reply: Option<String>,
    ) -> io::Result<Vec<StepOutput>> {
        // **One implementation, and it is the streaming one.** A reply
        // that arrives whole — a user taking the branch's turn, a client
        // that does not stream — is fed through the same door as one that
        // arrives in chunks, and ends the same way.
        //
        // There used to be two: this function built its own `Run`, its
        // own `Notebook` and its own zero-cell rule, while production
        // only ever went through `notebook_stream`. They agreed on the
        // happy path and diverged on every other, and because the unit
        // tests drove *this* one, five bugs reached a live run through a
        // green suite.
        self.open_notebook_reply(tree, None, author)?;
        // Recorded first, run second. The text is all here, so there is
        // nothing to wait for, and ending the reply before driving it
        // keeps `ReplyEnd` where it belongs — after the reply's own
        // parts and before the effects of its cells.
        self.notebook_feed(tree, &source)?;
        Ok(self
            .notebook_stream_end(tree, truncated, usage, thinking)?
            .unwrap_or_default())
    }

    /// Continue a suspended program directly — the live half of a
    /// handler's `return resume(value)` decision (DESIGN.md's thesis
    /// table). Nothing new is **said**: no `Turn` is logged, because
    /// nothing entered the log beyond the run continuing on its own
    /// terms. The caller (host) is the one who ran the handler program
    /// and read its return value; by the time this is called, "is
    /// something suspended" is not this method's question — a host that
    /// calls it on an unsuspended `Runner` has a bug of its own, which is
    /// exactly what "ineligibility stops being an event kind"
    /// (23_ONE_AGENT.md A4) means: there is no LLM-facing refusal to
    /// construct here anymore, because the LLM never names this call.
    ///
    /// Takes `tree` only for signature symmetry with [`Runner::abandon`]
    /// and every other host-facing step method — nothing is logged here,
    /// so it goes unused.
    pub fn resume(
        &mut self,
        _tree: &mut Tree,
        value: serde_json::Value,
    ) -> io::Result<Vec<StepOutput>> {
        let Some(Parked {
            mut run,
            resume_with: suspension,
            ..
        }) = self.parked.pop()
        else {
            panic!("Runner::resume called with nothing parked — a host bookkeeping bug");
        };
        // **The halt has been decided, so it is no longer one.**
        // `stop` marks the run halted and `suspend` carries that flag
        // into `Phase::Suspended` untouched; without clearing it here
        // the frame comes back as `Running` with `halted` still set,
        // and both `pump` and `drive_notebook` refuse to step a halted
        // run — so resuming a stop produced no rows, no terminal
        // handback and no error, just a branch that went quiet with a
        // program still open. Found 2026-09-20 by resuming one: reply
        // #8 ran `history.note(resume(null))` and logged nothing at
        // all.
        run.returned = None;
        match &suspension {
            // Nothing to push: the VM was parked between slices, not
            // stopped at a raise or an error.
            ResumeWith::Continue => {}
            ResumeWith::Raise => {
                let v = json_arg(&mut run.vm, &value);
                run.vm.resume_raise(v);
            }
            ResumeWith::Trapped(e) => match e.resume {
                ResumeMode::PushValueThenContinue => {
                    let v = json_arg(&mut run.vm, &value);
                    run.vm
                        .resume_with(e, v)
                        .expect("resume audited as resumable by the host");
                }
                ResumeMode::NotResumable => {
                    panic!(
                        "Runner::resume called on a not-resumable trap — a host bookkeeping bug"
                    );
                }
            },
        }
        let program_id = run.program_id;
        self.phase = Phase::Running(run);
        self.note_status(program_id, ProgramStatus::Running);
        Ok(vec![StepOutput::Working])
    }

    /// Discard a suspended program without continuing it — the other
    /// half of a handler's decision (`return abandon()`). The physics
    /// already happened: in-flight calls stay pending and their results
    /// are still logged as artifacts when they arrive; only the VM is
    /// dropped. Like [`Runner::resume`], this is a direct host call, not
    /// something the LLM names.
    ///
    /// It logs `crate::types::Handback::Abandoned`, and must: a run needs exactly one
    /// log-visible terminal or nothing downstream can be derived from the
    /// log alone. `Return` is the completing case; this is the other one.
    /// Logging nothing — which is what this did before — left the branch
    /// reading as permanently suspended, and left `depth_after`
    /// (`tree.rs`, driving `document::render`'s fold) never decrementing
    /// the depth the raise had incremented, so every later event rendered
    /// inside a scope nothing would ever close.
    pub fn abandon(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        let Some(Parked { run, .. }) = self.parked.pop() else {
            panic!("Runner::abandon called with nothing parked — a host bookkeeping bug");
        };
        let discarded = run.program_id;
        self.note_status(discarded, ProgramStatus::Failed);
        // `Handover`: this condition closes the frame that decided, it
        // does not open one. `depth_after` matches the cause ahead of the
        // disposition for exactly this reason, so the value here is
        // belt-and-braces rather than load-bearing.
        tree.append(
            &mut self.spine,
            EventPayload::Handback {
                program: discarded,
                how: Handback::Abandoned,
                site: 0,
                stack: Vec::new(),
            },
        )?;
        self.last_vm = Some(run.vm);
        self.prompt_if_needed(tree)
    }

    fn on_tool_results(
        &mut self,
        tree: &mut Tree,
        batch: Vec<ToolResult>,
    ) -> io::Result<Vec<StepOutput>> {
        let mut delivered = false;
        // Calls that settled with **nothing awaiting them**: the run that
        // issued them has been rewritten away, or the branch holds no VM
        // at all (it re-entered after a crash). Rule C decides what
        // happens to them — see below.
        let mut unawaited: Vec<(EventId, EventId)> = Vec::new();
        for tr in batch {
            // A call this session never issued can still settle here: an
            // exchange the log left open routes home by its logged ids
            // alone, so a re-entered branch receives the answer its dead
            // VM was waiting for. Logging it is what makes "after a
            // resume, no completed work is invisible" true; who (if
            // anyone) was awaiting it is the next question, below.
            let pending = self.pending.remove(&tr.call);
            if pending.is_none() && !self.settleable(tree, tr.call) {
                continue; // unknown or duplicate — nothing to log
            }
            // Resolution order is arrival order: the `Result` lands now,
            // naming the `Call` logged at dispatch.
            // **The row's id travels with its value.** `keep(f)` and
            // `peek(f)` want the row a result came from, and a program
            // holding only the value had no way back to it — the id
            // appears in the document a turn later, on the menu row,
            // which is too late to name in the reply that made the
            // call. Injected before the value is logged, so a `fetch`
            // of the row and the call that produced it agree.
            //
            // Objects only: there is nowhere to hang a field on a
            // string or a number, and every tool that returns something
            // worth showing returns an object.
            let with_id = |v: &serde_json::Value| match v {
                serde_json::Value::Object(map) if !map.contains_key("id") => {
                    let mut map = map.clone();
                    map.insert("id".into(), serde_json::json!(tr.call.as_u64()));
                    serde_json::Value::Object(map)
                }
                other => other.clone(),
            };
            let tr = ToolResult {
                call: tr.call,
                result: tr.result.as_ref().map(&with_id).map_err(|e| e.clone()),
            };
            let outcome = match &tr.result {
                Ok(v) => Outcome::Delivered(v.clone()),
                Err(msg) => Outcome::Failed(msg.clone()),
            };
            let result = tree.append(
                &mut self.spine,
                EventPayload::Result {
                    call: tr.call,
                    outcome,
                },
            )?;

            // A `tell()`'s `Result` is a delivery receipt, not a value
            // anyone asked for — `expects_reply: false` already says so
            // (`Call::Send`'s own doc). It is logged above like any other
            // artifact (`fetch_history(id)` can still fetch it), but Rule C
            // exists to protect a call's *value* from going unseen after
            // a resume, and a `tell` has no value to protect: nothing
            // ever holds a promise for it (`Instr::Notify`,
            // 23_ONE_AGENT.md C0b), so treating its settlement as a
            // surprise nobody awaited would be wrong on every firing, not
            // just some. Checked before either `unawaited` push below —
            // both are reachable for a `tell` (the generation-mismatch
            // arm is actually the common one: `finish_program` bumps
            // `self.generation` immediately after dispatching an
            // unstarted `tell`, so its own registration is stale by the
            // time the result lands even when the branch never moved).
            let is_unwaited_tell = matches!(
                tree.events.get(&tr.call).map(|e| &e.payload),
                Some(EventPayload::Call(Call::Send {
                    expects_reply: false,
                    ..
                }))
            );
            // Deliver only into the run that issued the call. Anything
            // else is an artifact **and** a notice (rule C, below) —
            // unless it's a `tell`, which owes no one a notice either.
            let Some(pending) = pending else {
                if !is_unwaited_tell {
                    unawaited.push((tr.call, result));
                }
                continue;
            };
            if pending.generation != self.generation || self.run_ref().is_none() {
                if !is_unwaited_tell {
                    unawaited.push((tr.call, result));
                }
                continue;
            }
            // Where the program is waiting decides how this lands. A
            // promise settles and whoever awaits it wakes; a frozen
            // frame takes the value directly, and takes a failure as a
            // throw at its call site rather than as a rejection it
            // could never have caught.
            match pending.slot {
                Slot::Promise(promise) => {
                    let vm = self.settling_vm();
                    match tr.result {
                        Ok(v) => {
                            let val = json_arg(vm, &v);
                            vm.resolve_promise(promise, val)
                                .expect("pending promise is settleable");
                        }
                        Err(msg) => {
                            let val = Value::String(RcStr::from(msg.as_str()));
                            vm.reject_promise(promise, val)
                                .expect("pending promise is settleable");
                        }
                    }
                }
                Slot::Settle => self.settle(tr.result),
            }
            delivered = true;
        }
        let mut out = Vec::new();
        // **Rule C**, the other half: waiting is a property of the
        // awaiting program, never of the message. A value someone's
        // program awaits resolves its promise and never enters a context;
        // one **nobody** awaits is logged as an artifact *and* surfaced
        // as a harness post — a tell, so the branch notices without
        // owing anyone an answer.
        for (call, result) in unawaited {
            let origin = Origin::Direct {
                text: self.settled_notice(tree, call, result),
                input: serde_json::Value::Null,
                options: Vec::new(),
                expects_reply: false,
            };
            let (_, delivered) = self.deliver(tree, Author::Harness, origin)?;
            out.extend(delivered);
        }
        // A suspended run stays suspended (results land for later); a
        // running one can make progress now.
        if delivered && matches!(self.phase, Phase::Running(_)) {
            out.push(StepOutput::Working);
        }
        Ok(out)
    }

    /// Whether a `Result` for `call` still belongs on this branch: the
    /// call is on its own path and nothing has settled it yet.
    fn settleable(&self, tree: &Tree, call: EventId) -> bool {
        let segment = self.agent_segment(tree);
        segment.iter().any(|e| e.id == call) && settlement_of(&segment, call).is_none()
    }

    /// The body of the harness post that surfaces an unawaited `Result`.
    fn settled_notice(&self, tree: &Tree, call: EventId, result: EventId) -> String {
        let label = match tree.events.get(&call).map(|e| &e.payload) {
            Some(EventPayload::Call(c)) => call_label(c),
            _ => format!("#{}", call.as_u64()),
        };
        let outcome = match tree.events.get(&result).map(|e| &e.payload) {
            Some(EventPayload::Result { outcome, .. }) => match outcome {
                Outcome::Delivered(v) => preview(v),
                Outcome::Failed(msg) => format!("failed: {msg}"),
            },
            _ => String::new(),
        };
        format!(
            "A call you issued has settled with no program awaiting it: [{}] {label} → \
             {outcome}. Fetch the whole value with history.fetch({}). Nothing is owed in reply.",
            call.as_u64(),
            call.as_u64(),
        )
    }

    /// The VM a landing `Result` settles into — live while running or
    /// suspended (a suspended run's results land for later).
    fn settling_vm(&mut self) -> &mut VM {
        &mut self.run_mut().expect("no VM to settle into").vm
    }

    fn on_tick(&mut self, tree: &mut Tree, fuel: u64) -> io::Result<Vec<StepOutput>> {
        if !matches!(self.phase, Phase::Running(_)) {
            return Ok(Vec::new());
        }
        // **Rule B**: every fuel-slice boundary is a safe point, so a
        // post logged since the last render suspends the run *here*,
        // before another instruction executes. That is what makes the
        // delivery guarantee ≤ one slice, and it costs nothing: the
        // boundary already has total state visibility.
        let unseen = self.unseen_posts(tree);
        if !unseen.is_empty() {
            return self.suspend(tree, SuspendCause::Posted(unseen), Vec::new());
        }
        self.pump(tree, fuel)
    }
    /// Drive the VM until it blocks on the host, suspends, finishes, or
    /// runs out of fuel. Each round runs one `step(fuel)` slice; only a
    /// synchronous unblock — a call the harness answered on the spot,
    /// through either dispatcher — earns another round.
    fn pump(&mut self, tree: &mut Tree, fuel: u64) -> io::Result<Vec<StepOutput>> {
        let mut out = Vec::new();
        // Nothing after `finish`/`stop` runs, including whatever a
        // late-arriving tool result would otherwise have woken.
        if self.halted() {
            return Ok(out);
        }
        // **A VM parked at the end of the code it has is between cells,
        // not finished.** Stepping it once more runs off the end and
        // reports `Done`, which ends the reply — while the reply is
        // still arriving.
        //
        // `drive_notebook` has always checked this, because text
        // arriving is the obvious way to reach a parked VM. It is not
        // the only way: a cell that *ends* with a fire-and-forget call
        // — `tell("…")` as the last statement, which is the shape three
        // exemplars teach — reaches its `Pause` with that call still
        // outstanding, and the settlement the host hands back a moment
        // later arrives here through `on_tool_results`, which pumps
        // unguarded. The run then completed mid-reply, and every cell
        // still streaming was dropped in silence by `notebook_feed`,
        // which finds no run to feed.
        //
        // The guard belongs here rather than at each caller: being
        // parked between cells is a fact about the VM, and every path
        // that steps it has to respect it.
        if let Phase::Running(run) = &self.phase
            && run.vm.ip as usize >= run.vm.code.len()
            && run.notebook.as_ref().is_some_and(|n| !n.is_ended())
        {
            return Ok(out);
        }
        for _ in 0..MAX_PUMP_ROUNDS {
            let Phase::Running(run) = &mut self.phase else {
                unreachable!("pump outside Running");
            };
            match run.vm.step(fuel) {
                Ok(StepResult::OutOfFuel) => {
                    out.push(StepOutput::Working);
                    return Ok(out);
                }
                Ok(StepResult::Pending { calls }) => {
                    let progressed = self.dispatch_calls(tree, calls, &mut out)?;
                    if !progressed {
                        return Ok(out); // blocked on the host now
                    }
                }
                Ok(StepResult::Settle { call }) => {
                    // One call, answered into the frame that made it.
                    // `false` is not "failed" here — it is "the answer
                    // takes a round trip" (a `spawn`'s child, a
                    // re-attached fetch), and the frame waits, frozen,
                    // exactly as it would for any other host answer.
                    let progressed = self.dispatch_settle(tree, call, &mut out)?;
                    if !progressed {
                        return Ok(out);
                    }
                }
                // **The program ended.** Either it ran off the end of
                // its last cell — the ordinary way, and the reply is
                // over by then — or it ran a top-level `return`, which
                // can land while the provider is still writing the
                // cells after it. `halt` is the same ending, deferred
                // until the reply closes so `ReplyEnd` can carry what
                // the completion cost; it also stops the cells still to
                // come from running, which is what `return` means.
                Ok(StepResult::Done { value, unstarted }) => {
                    return self.halt(tree, value, unstarted, out);
                }
                // **A cell ended — the run did not** (D7). The frame is
                // still standing with every binding the cell declared, so
                // there is nothing to log and nothing to report: feed the
                // next cell in and keep stepping. When the cells run out,
                // the epilogue appended here is the ordinary root
                // `Return(0)`, so the very next step reports `Done` and
                // reaches `finish_program` on the one path every program
                // takes. That is why `finish_program` needs no notebook
                // case: a cell boundary never arrives there.
                Ok(StepResult::Paused { unstarted }) => {
                    // **A cell ended — the run did not** (D7). The frame is
                    // still standing with every binding the cell declared, so
                    // there is nothing terminal to log: walk the reply's
                    // pieces until the next cell, or close the run once they
                    // run out.
                    //
                    // Pieces are consumed here, in source order, rather than
                    // logged the moment the splitter recognises them. That is
                    // what keeps the log readable as the reply reads: a
                    // paragraph written between two cells lands after the
                    // first cell's calls, and each cell's `Turn` lands
                    // immediately before its own instructions execute, so
                    // `Call`'s placement holds (D15).
                    //
                    // The cell's own unawaited calls go out **first**, before
                    // the next piece is touched. A `tell()` a cell never
                    // awaited sits in the VM's outbox, which only drains when
                    // the program blocks or ends — so without this its `Call`
                    // would be logged after the *next* cell's `Turn`, and
                    // `Call`'s placement rule ("between the program's `Turn`
                    // and its eventual `Return`") would be false of it.
                    self.dispatch_calls(tree, unstarted, &mut out)?;
                    match self.advance_notebook(tree, &mut out)? {
                        NotebookStep::Ran => {}
                        // The next fence has not closed yet. The VM parks at
                        // its `Pause` while the completion keeps arriving;
                        // the next piece wakes it.
                        NotebookStep::Waiting => return Ok(out),
                        NotebookStep::Failed(report) => {
                            return self.suspend(
                                tree,
                                SuspendCause::CellCompileFailed(report),
                                out,
                            );
                        }
                    }
                }
                Ok(StepResult::Raise { condition, payload }) => {
                    return self.suspend(tree, SuspendCause::Raise { condition, payload }, out);
                }
                Err(e) => {
                    return self.suspend(tree, SuspendCause::Trapped(e), out);
                }
            }
        }
        out.push(StepOutput::Working);
        Ok(out)
    }

    /// Record that the program ended itself, and end the turn if the
    /// reply has already finished arriving.
    ///
    /// `return` ends the program **where it stands**, which is usually
    /// somewhere in the middle of a notebook the provider is still
    /// writing. Two things follow, and this is the one place both are
    /// arranged:
    ///
    /// - **Nothing after it runs** — not the instructions after it in
    ///   this cell, and not the cells that have not arrived yet. The
    ///   `returned` field is what enforces the second half: the pieces
    ///   still to come are logged as they arrive (28: the record of
    ///   what was written stays whole) and then dropped unrun.
    /// - **The turn still ends properly.** `ReplyEnd` carries what the
    ///   completion cost, and that number does not exist until the
    ///   stream closes — so the ending waits for it rather than logging
    ///   a reply with no end, or one whose end lands after its own
    ///   outcome. When the reply arrived whole there is nothing to wait
    ///   for and this ends the turn on the spot.
    fn halt(
        &mut self,
        tree: &mut Tree,
        value: Value,
        unstarted: Vec<InvokeCall>,
        out: Vec<StepOutput>,
    ) -> io::Result<Vec<StepOutput>> {
        let Phase::Running(run) = &mut self.phase else {
            unreachable!("halt outside Running");
        };
        run.returned = Some(value);
        // **Issued before the return, so they go out now** — not when
        // the reply eventually closes. A `tell` the program wrote and
        // never awaited reaches the person as the program runs, which
        // is what the card promises of it; deferring it with the
        // ending would log it after the reply's own `ReplyEnd` and
        // delay the words by however long the rest of the completion
        // takes to arrive.
        let mut out = out;
        self.dispatch_calls(tree, unstarted, &mut out)?;
        self.halt_if_ready(tree, out)
    }

    /// Apply a remembered `Halt` once the reply it belongs to is over,
    /// or leave it remembered. Called both by [`halt`](Self::halt) — for
    /// the reply that had already finished arriving — and by the
    /// notebook driver, which is what reaches it for the reply that had
    /// not.
    fn halt_if_ready(
        &mut self,
        tree: &mut Tree,
        out: Vec<StepOutput>,
    ) -> io::Result<Vec<StepOutput>> {
        let Phase::Running(run) = &self.phase else {
            return Ok(out);
        };
        let Some(value) = run.returned.clone() else {
            return Ok(out);
        };

        // Still arriving: park. `drive_notebook` comes back here each
        // time the reply grows, and `notebook_stream_end` ends it.
        if !run.notebook.as_ref().is_none_or(|n| n.is_ended()) {
            return Ok(out);
        }
        let Phase::Running(run) = &mut self.phase else {
            unreachable!("checked Running above");
        };
        let unstarted = std::mem::take(&mut run.unstarted);
        self.finish_program(tree, value, unstarted, out)
    }

    /// Whether the program has halted itself and is only waiting for
    /// its reply to finish arriving — the state in which the VM must
    /// not be stepped and no further cell may be fed in.
    fn halted(&self) -> bool {
        matches!(&self.phase, Phase::Running(run) if run.returned.is_some())
    }

    /// Classify one `Pending` batch — the calls that hold a **promise**
    /// (`Instr::Invoke`). **This is one of the two places a bare harness
    /// verb's name becomes a `Call` variant / log effect** (17_BRANCHES
    /// A2, folded in here from the deleted `verbs.rs`); its sibling is
    /// `dispatch_settle`, which takes the verbs that answer into a
    /// standing frame instead. Everything downstream — the artifact
    /// menu, reconciliation, re-attach, routing an answer home —
    /// matches on the variant, never on the string again.
    ///
    /// What arrives here: `ask` and `tell` become logged `Call::Send`s,
    /// and everything else (`tools.*`, plus any bare name this
    /// dispatcher does not recognize) becomes `ToolCalls`. Every call
    /// that leaves here is logged as a `Call` event *at dispatch*,
    /// settled later by exactly one `Result`.
    ///
    /// The settle-at-dispatch verbs — `spawn`, `fork`, `list_agents`,
    /// `finish`, `answer`, `note_history`, `fetch_history`,
    /// `remove_history`, `rewrite_history` — can no longer reach this
    /// function at all: the compiler lowers them to `Instr::Settle`, so
    /// they arrive as `StepResult::Settle` and are handled one at a time
    /// by `dispatch_settle`. Returns true if any call here made progress
    /// the program can run on.
    fn dispatch_calls(
        &mut self,
        tree: &mut Tree,
        calls: Vec<InvokeCall>,
        out: &mut Vec<StepOutput>,
    ) -> io::Result<bool> {
        let mut tool_calls = Vec::new();
        let mut sends = Vec::new();
        let mut progressed = false;

        for call in calls {
            match call.name.as_str() {
                TOOL_ASK | TOOL_CHOOSE | TOOL_TELL => {
                    let expects_reply = call.name != TOOL_TELL;
                    let args = self.call_args_json(&call.args);
                    // `tell(text)` / `tell(to, text)`, always
                    // `ask(who, text)` — positional, not an options
                    // object; `input` alongside the text is no longer
                    // expressible from the bare verb (verbs.rs never
                    // carried one either). Omitted `to`/`who` resolves
                    // to whoever this branch owes its oldest open post
                    // to (`resolve_address`).
                    let (to, text, options) = match (call.name.as_str(), args.as_slice()) {
                        (TOOL_TELL, [text]) => (None, coerce_text(text), Vec::new()),
                        (TOOL_CHOOSE, [to, text, options]) => match read_options(options) {
                            Ok(options) => (Some(to.clone()), coerce_text(text), options),
                            Err(msg) => {
                                self.reject_call(call.promise, &msg);
                                progressed = true;
                                continue;
                            }
                        },
                        (TOOL_CHOOSE, _) => (None, None, Vec::new()),
                        (_, [to, text]) => (Some(to.clone()), coerce_text(text), Vec::new()),
                        _ => (None, None, Vec::new()),
                    };
                    match (text, self.resolve_address(tree, to.as_ref())) {
                        (Some(text), Ok(to)) => {
                            // **The same bound as prose.** `tell` and a
                            // prose segment are the two ways a reply
                            // speaks, and they are built at different
                            // sites — so capping one left the other
                            // able to deliver a megabyte of repeated
                            // text to the person.
                            let text = crate::report::cap_prose(&text);
                            let send = self.issue_call(
                                tree,
                                Call::Send {
                                    to,
                                    prose: false,
                                    text,
                                    input: serde_json::Value::Null,
                                    options,
                                    expects_reply,
                                    site: self.rebase_site(call.site),
                                    site_end: self.rebase_site(call.site_end),
                                },
                                Slot::Promise(call.promise),
                            )?;
                            sends.push(send);
                        }
                        (None, _) => {
                            self.reject_call(
                                call.promise,
                                &format!(
                                    "{}({}text{}) needs a text argument",
                                    call.name,
                                    if expects_reply { "who, " } else { "[to, ]" },
                                    if call.name == TOOL_CHOOSE {
                                        ", options"
                                    } else {
                                        ""
                                    }
                                ),
                            );
                            progressed = true;
                        }
                        (_, Err(msg)) => {
                            self.reject_call(call.promise, &msg);
                            progressed = true;
                        }
                    }
                }
                _ => {
                    let args = {
                        let vm = self.running_vm();
                        match args_as_json(vm, &call) {
                            Ok(args) => serde_json::Value::Array(args),
                            Err(why) => {
                                self.reject_call(call.promise, &why);
                                progressed = true;
                                continue;
                            }
                        }
                    };
                    let id = self.issue_call(
                        tree,
                        Call::Invoke {
                            name: call.name.clone(),
                            args: args.clone(),
                            site: self.rebase_site(call.site),
                        },
                        Slot::Promise(call.promise),
                    )?;
                    tool_calls.push(OutCall {
                        call: id,
                        name: call.name,
                        args,
                    });
                }
            }
        }
        if !tool_calls.is_empty() {
            out.push(StepOutput::ToolCalls(tool_calls));
        }
        if !sends.is_empty() {
            out.push(StepOutput::Sends(sends));
        }
        Ok(progressed)
    }

    /// Serve one settle-at-dispatch call (`Instr::Settle`): the verbs
    /// that answer **into a frame that is still standing**, rather than
    /// into a promise the program has to remember to await. The other
    /// half of `dispatch_calls`, and the reason the compiler no longer
    /// has to emit an `await` nobody wrote.
    ///
    /// Two shapes live here, and the difference is only *when* the
    /// value is known, never whether the frame survives:
    ///
    /// - **Answered here.** `fetch_history` reads a row off the log,
    ///   `answer` and `note_history` append one, `remove_history` /
    ///   `rewrite_history` add an edit to the batch the running
    ///   compaction handler is building, `finish` sets the flag that
    ///   stops the loop. The value is pushed before this function
    ///   returns and the program runs on in the same pump round
    ///   (`true`).
    /// - **Answered a round trip later.** `spawn`, `fork` and
    ///   `list_agents` need what only the host above this runner has —
    ///   the registry and card to root a child with, the live per-branch
    ///   status `list_agents` reports — so they are logged as `Call`s
    ///   and settled by their `Result` like any other call, through
    ///   `Slot::Settle`. So is a `fetch_history` that **re-attaches** to
    ///   a call this session still has in flight. The frame stays frozen
    ///   meanwhile: `VM::step` reports an empty `Pending` rather than
    ///   running against a stack with a hole in it.
    ///
    /// A failure is `settle_throw`, not a rejection: it lands as a throw
    /// at the call site, where an ordinary `try`/`catch` can see it and
    /// where an uncaught one stops the program. That is the second half
    /// of what this change buys — an unawaited mistake (a wrong
    /// compaction label, an `answer` to a question this branch does not
    /// owe) used to settle a promise nobody read, which looked to the
    /// model exactly like a call that worked.
    fn dispatch_settle(
        &mut self,
        tree: &mut Tree,
        call: SettleCall,
        out: &mut Vec<StepOutput>,
    ) -> io::Result<bool> {
        match call.name.as_str() {
            TOOL_FETCH_HISTORY => {
                // **Re-attach, not re-ask.** A call this session still
                // has in flight is re-registered against the *current*
                // run, so a rewritten program is handed the answer the
                // dead VM would have got. Without it, "pending" in the
                // menu is amnesia with extra steps.
                if let Some(pending) = self.reattachable(&*tree, &call.args) {
                    self.pending.insert(
                        pending,
                        PendingCall {
                            slot: Slot::Settle,
                            generation: self.generation,
                        },
                    );
                    return Ok(false); // no progress: the frame parks on it
                }
                let fetched = self.fetch_history(&*tree, &call.args);
                self.settle(fetched);
                Ok(true)
            }
            TOOL_SPAWN => {
                // `spawn(charter)` — the folded-in verbs.rs convention:
                // one positional string, not the old `tools.spawn({
                // charter, name, tools })` options object. A name or a
                // tool allowlist is not expressible from the bare verb
                // (verbs.rs never showed a second argument either);
                // `tools.spawn` (a registry-configured capability, if
                // the agent has one) is the escape hatch for those.
                let args = self.call_args_json(&call.args);
                // **A second argument names the agent.**
                //
                // `Call::Spawn` has carried a `name` since the branch
                // vocabulary existed, `list_agents` reports it and the
                // TUI titles branches with it — and `spawn` passed
                // `None` regardless, so every agent a program made was
                // anonymous. That is survivable for one helper and
                // useless for a tree: a person reading a roster of
                // `agent 8`, `agent 11`, `agent 14` cannot tell the
                // testing manager from the parity one, and cannot
                // address either without counting.
                let name = args
                    .get(1)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned);
                match args.first().and_then(|v| v.as_str()) {
                    Some(charter) => {
                        let spawn = self.issue_call(
                            tree,
                            Call::Spawn {
                                name,
                                charter: charter.to_owned(),
                                tools: None,
                                site: self.rebase_site(call.site),
                            },
                            Slot::Settle,
                        )?;
                        out.push(StepOutput::Spawns(vec![spawn]));
                        Ok(false)
                    }
                    None => {
                        self.settle_err("spawn(charter) needs a charter string");
                        Ok(true)
                    }
                }
            }
            TOOL_FORK => {
                // `fork()` takes nothing: it creates a divergent branch
                // and settles with its handle. What the child should do
                // is said afterwards, in its own `tell` or `ask` —
                // creating is not messaging (`22_ONE_VOCABULARY.md`).
                let name = self
                    .call_args_json(&call.args)
                    .first()
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned);
                let fork = self.issue_call(
                    tree,
                    Call::Fork {
                        name,
                        site: self.rebase_site(call.site),
                    },
                    Slot::Settle,
                )?;
                out.push(StepOutput::Forks(vec![fork]));
                Ok(false)
            }
            TOOL_LIST_AGENTS => {
                // The card has advertised `list_agents()` since phase 20
                // and nothing answered it: it fell through to the tool
                // registry, which has no such tool, so a program that
                // took the card at its word got `unknown tool
                // list_agents`. It belongs here, next to `spawn` and
                // `fork` — spawn creates, fork creates, this one
                // enumerates, and all three are about agent topology.
                //
                // It is logged and dispatched rather than answered on
                // the spot for one reason: the card promises "with
                // status", and a branch's status is live session state
                // (which runner exists, what phase it is in) that this
                // runner cannot see for anyone but itself. The host's
                // `serve_agents` — the sole implementation — is where
                // that lives, so this routes there rather than growing a
                // second, weaker projection that would have to answer
                // "dormant" for everyone.
                //
                // The program's own options ride through — `list_agents()`
                // and `list_agents({ under, deep })` are the same verb,
                // which is what collapsing the old `tools.agents`
                // spelling into this one left behind. `deep` defaults to
                // true in `serve_agents`, because the card promises
                // *subtree*.
                let args = serde_json::Value::Array(self.call_args_json(&call.args));
                let id = self.issue_call(
                    tree,
                    Call::Invoke {
                        name: TOOL_LIST_AGENTS.to_owned(),
                        args: args.clone(),
                        site: self.rebase_site(call.site),
                    },
                    Slot::Settle,
                )?;
                out.push(StepOutput::ToolCalls(vec![OutCall {
                    call: id,
                    name: TOOL_LIST_AGENTS.to_owned(),
                    args,
                }]));
                Ok(false)
            }
            TOOL_ANSWER => {
                let args = self.call_args_json(&call.args);
                // **The middle argument was never read.** `[question,
                // _label, value]` is what this arm always matched: the
                // label was bound and dropped, while the card asked for
                // it in the signature — so every `answer` call computed
                // a string, paid tokens for it, and handed it to
                // nothing. Two arguments is the documented form now;
                // three is still accepted so a program written against
                // the old card is not punished for it.
                let args: Vec<serde_json::Value> = match args.as_slice() {
                    [question, value] => vec![question.clone(), value.clone()],
                    [question, _label, value] => vec![question.clone(), value.clone()],
                    _ => {
                        self.settle_err(
                            "answer(question, value) takes the question's id and the value",
                        );
                        return Ok(true);
                    }
                };
                match args.as_slice() {
                    [question, value] => {
                        match question.as_u64().filter(|n| *n > 0).map(EventId::new) {
                            Some(question) if self.open().contains(&question) => {
                                // A `choose` promised its asker one of
                                // the offered strings. An agent that
                                // answers outside the set is corrected
                                // here rather than escalated to the
                                // asker: unlike a person's prose, this
                                // is a program's mistake, and the
                                // program that made it is the one still
                                // running and able to fix it.
                                let options = Context::options(tree, question);
                                let value = if options.is_empty() {
                                    value.clone()
                                } else {
                                    let reply = match value.as_str() {
                                        Some(s) => s.to_owned(),
                                        None => value.to_string(),
                                    };
                                    match pick_option(&reply, &options) {
                                        Some(picked) => serde_json::Value::String(picked),
                                        None => {
                                            let offered = options
                                                .iter()
                                                .map(|o| format!("{o:?}"))
                                                .collect::<Vec<_>>()
                                                .join(", ");
                                            self.settle_err(&format!(
                                                "#{} offered a choice; answer it with one of \
                                                 [{offered}], not {reply:?}",
                                                question.as_u64()
                                            ));
                                            return Ok(true);
                                        }
                                    }
                                };
                                tree.append(
                                    &mut self.spine,
                                    EventPayload::Answer {
                                        question,
                                        value: value.clone(),
                                    },
                                )?;
                                self.settle(Ok(serde_json::Value::Bool(true)));
                                out.push(StepOutput::Answered { question, value });
                                // NOTE: the `label` checksum verbs.rs
                                // describes (must match the question's
                                // own label, the same way
                                // `compaction.rs`'s `CompactionOp`
                                // checks one) is **not** enforced here —
                                // flagged prominently in 23_ONE_AGENT.md
                                // A4's report. verbs.rs's own doc said
                                // the same: "not implemented at this
                                // layer (no log to check against yet)".
                                Ok(true)
                            }
                            Some(question) => {
                                let msg = match self.owning_branch(tree, question) {
                                    Some(branch) => format!(
                                        "#{} belongs to branch #{}; this fork inherited it \
                                         as history and does not owe it. To make your \
                                         answer the delivered one, the user can take that \
                                         branch's turn.",
                                        question.as_u64(),
                                        branch.as_u64()
                                    ),
                                    None => format!(
                                        "#{} is not open on this branch — it was already \
                                         answered, or it is a notice that owes no answer.",
                                        question.as_u64()
                                    ),
                                };
                                self.settle_err(&msg);
                                Ok(true)
                            }
                            None => {
                                self.settle_err("answer's question id must be a positive integer");
                                Ok(true)
                            }
                        }
                    }
                    _ => unreachable!("normalised to two arguments above"),
                }
            }
            TOOL_NOTE_HISTORY => {
                let args = self.call_args_json(&call.args);
                match args.first() {
                    // **A decision is not a row.** `resume(v)`/`abandon()`
                    // compile to a tagged object, and the card's own
                    // wording is "Appending it is the decision; calling
                    // it is not" — so appending one records the verdict
                    // instead of writing a note about it.
                    //
                    // This is the notebook half of C0a, and it did not
                    // exist until 2026-09-19. A reply has no `return`
                    // (D5), and `finish_program` read the tag off a
                    // program's return value and nowhere else — so under
                    // the notebook transport a handler could do exactly
                    // what the card told it to and the suspended run
                    // would sit there forever. It was invisible because
                    // every raise/resume test ran under the *program*
                    // transport, which is the argument for not keeping
                    // two.
                    //
                    // Recorded now, applied when the reply's run ends,
                    // which is where the old path applied it too: a
                    // handler may `answer(...)` or `tell()` first, and
                    // resuming mid-reply would restart a program while
                    // the cells after the decision were still to run.
                    Some(value) if decision_tag(value).is_some() => {
                        self.pending_decision = Some(value.clone());
                        self.settle(Ok(serde_json::Value::Null));
                    }
                    Some(value) => {
                        let (site, site_end) =
                            (self.rebase_site(call.site), self.rebase_site(call.site_end));
                        let row = tree.append(
                            &mut self.spine,
                            EventPayload::Note {
                                value: value.clone(),
                                site,
                                site_end,
                            },
                        )?;
                        // **The id, because the program asked for it.**
                        // It used to settle `null` and throw the id
                        // away, so a program that wanted to refer to
                        // the row it had just written had nothing to
                        // hold: the document shows `/* ← history[40] */`
                        // only on the way *back*, a turn later. Two
                        // live programs invented an identifier for it
                        // rather than do without —
                        // `history_append_id_placeholder` and
                        // `history_rows_at_this_point` — and died with
                        // `is not defined`. A value the caller reaches
                        // for by making up a name for it is one the
                        // call should be handing over.
                        self.settle(Ok(serde_json::json!(row.as_u64())));
                    }
                    None => self.settle_err("note_history(value) needs one argument"),
                }
                Ok(true)
            }
            TOOL_KEEP_HISTORY | TOOL_PEEK_HISTORY => {
                let args = self.call_args_json(&call.args);
                // **Either the result or its id.** A tool hands back an
                // object carrying `id`, so `keep(f)` is what a program
                // naturally writes and `keep(f.id)` is what it writes
                // when it has only kept the number. Refusing one of
                // them would be a rule with nothing behind it.
                let (id, from_object) = match args.first() {
                    Some(v) if v.is_u64() => (v.as_u64(), None),
                    Some(v) => (
                        v.get("id").and_then(|i| i.as_u64()),
                        Some(v.clone()),
                    ),
                    None => (None, None),
                };
                let Some(id) = id.filter(|n| *n > 0).map(EventId::new) else {
                    let verb = call.name.as_str();
                    self.settle_err(&format!(
                        "{verb}(result) needs a tool result, or the id of one — \
                         `{verb}(f)` or `{verb}(f.id)`"
                    ));
                    return Ok(true);
                };
                let mode = if call.name.as_str() == TOOL_KEEP_HISTORY {
                    crate::types::RenderMode::Kept
                } else {
                    crate::types::RenderMode::Peeked
                };
                // **No projection means the whole thing, cut the way it
                // was cut last time if it was.** A row shown once and
                // wanted again should not have to restate how it was
                // cut, and because a result never changes, re-applying
                // the same projection to the same value is the value
                // already stored. Failing that it is the result the
                // caller is holding, and failing *that* — `keep(4)`,
                // read off the menu with nothing in hand — the row's
                // own value, read back the way `history.fetch` reads it.
                let value = match args.get(1).filter(|v| !v.is_null()) {
                    Some(v) => v.clone(),
                    None => self
                        .last_rendered_value(tree, id)
                        .or(from_object)
                        .or_else(|| self.fetch_history(tree, &[Value::PosInt(id.as_u64())]).ok())
                        .unwrap_or(serde_json::Value::Null),
                };
                tree.append(
                    &mut self.spine,
                    EventPayload::Render { of: id, mode, value },
                )?;
                self.settle(Ok(serde_json::json!(id.as_u64())));
                Ok(true)
            }
            TOOL_REMOVE_HISTORY | TOOL_REPLACE_HISTORY => {
                // Nothing leaves the process and nothing settles later.
                // The op joins the batch this handler is building and
                // the value lands at once, so a compaction program reads
                // as ordinary straight-line code.
                let recorded = self
                    .record_compaction(&call.name, &call.args)
                    .map(|()| serde_json::Value::Null);
                self.settle(recorded);
                Ok(true)
            }
            // Unreachable from compiled code — the compiler emits
            // `Settle` for exactly the names above — but a hand-built
            // program could get here, and a clear throw beats a panic.
            other => {
                let msg = format!("`{other}` is not a settle-at-dispatch verb");
                self.settle_err(&msg);
                Ok(true)
            }
        }
    }

    /// Hand a settle-at-dispatch call its outcome: the value onto the
    /// frame's stack, or the failure as a throw at the call site.
    fn settle(&mut self, outcome: Result<serde_json::Value, String>) {
        match outcome {
            Ok(value) => {
                let vm = self.settling_vm();
                let v = vm.json_to_stack_value(&value, 0).expect("plain json");
                vm.push_settled(v).expect("a Settle is outstanding");
            }
            Err(msg) => self.settle_err(&msg),
        }
    }

    /// Fail a settle-at-dispatch call: throw into the frame that made
    /// it. An uncaught throw is left for the VM's next `step`, which
    /// reports it as the program's own trap — the same road any other
    /// uncaught throw takes.
    fn settle_err(&mut self, message: &str) {
        let vm = self.settling_vm();
        let v = Value::String(RcStr::from(message));
        vm.settle_throw(v).expect("a Settle is outstanding");
    }

    /// Every argument of a dispatched call, as JSON — the uniform shape
    /// every bare-verb parser above reads from (folded in from the
    /// deleted `verbs.rs`'s `args_as_json`).
    fn call_args_json(&mut self, args: &[Value]) -> Vec<serde_json::Value> {
        let vm = self.running_vm();
        args.iter().map(|v| value_json(vm, v)).collect()
    }

    /// Add one history edit to the batch the running compaction program
    /// is building.
    ///
    /// Refuses outside a compaction program rather than quietly doing
    /// nothing: history is not a thing an ordinary program edits, and a
    /// silently-ignored call would look to the model exactly like one
    /// that worked.
    ///
    /// No label argument. See `CompactionOp` for why the checksum went.
    fn record_compaction(&mut self, name: &str, args: &[Value]) -> Result<(), String> {
        use crate::compaction::CompactionOp;
        let args = self.call_args_json(args);
        let id_at = |n: usize| -> Option<EventId> {
            args.get(n)
                .and_then(|v| v.as_u64())
                .filter(|n| *n > 0)
                .map(EventId::new)
        };
        let Some(first) = id_at(0) else {
            return Err(format!("{name} needs the id of an entry"));
        };
        let op = if name == TOOL_REMOVE_HISTORY {
            // `remove(id)` is `remove(id, id)`. A range runs whichever
            // way it was written — the model reading ids off the
            // document should not have to care which end it named first.
            let last = id_at(1).unwrap_or(first);
            CompactionOp::Remove {
                from: first.min(last),
                to: first.max(last),
            }
        } else {
            let Some(text) = args.get(1).and_then(|v| v.as_str()) else {
                return Err(format!(
                    "{TOOL_REPLACE_HISTORY}(id, text) needs the text to show in its place"
                ));
            };
            CompactionOp::Replace {
                id: first,
                text: text.to_owned(),
            }
        };
        self.pending_edits.push(op);
        Ok(())
    }

    /// Reject a malformed call in place. Nothing is logged: the call was
    /// never dispatched, so it has no `Call` event and owes no `Result` —
    /// the rejection is the program's to catch (6_LANGUAGE Part B), and
    /// only an uncaught one traps into a condition.
    fn reject_call(&mut self, promise: PromisePtr, message: &str) {
        let v = Value::String(RcStr::from(message));
        self.running_vm()
            .reject_promise(promise, v)
            .expect("fresh promise");
    }

    /// Resolve a bare `ask`/`tell` address **before** the `Send` is
    /// logged, so nothing unresolved ever reaches the log.
    ///
    /// - omitted → the author of the oldest open post: *whoever asked
    ///   you*. For a root conversation that is the human, for a subagent
    ///   its parent, and a program never needs to know which.
    /// - a branch id → that branch.
    /// - an agent id with exactly one branch → that branch. An
    ///   **ambiguous** agent id (it has been forked) is a rejected call
    ///   naming the branches, because guessing which fork owes the answer
    ///   is exactly the race the one-owner rule exists to prevent.
    fn resolve_address(
        &self,
        tree: &Tree,
        to: Option<&serde_json::Value>,
    ) -> Result<Address, String> {
        let Some(to) = to.filter(|v| !v.is_null()) else {
            // Nothing open: the address is the user. An agent with
            // something to say and no one waiting on it is talking to
            // the person driving the session — who is always reachable,
            // having no branch of their own to be absent from. This is
            // also what makes a delegated child's "report directly
            // rather than through the parent" work without the child
            // having to know who spawned it (`22_ONE_VOCABULARY.md`,
            // "Creating is not messaging").
            //
            // This is deliberately *not* an answer: `18_TARGETING`'s
            // rule is that only `answer(question, value)` discharges an
            // open post, and a `tell` never does, whoever it reaches.
            let Some(&question) = self.spine.context().open.first() else {
                return Ok(Address::User);
            };
            return match asker_of(tree, question) {
                Some(Author::User) => Ok(Address::User),
                Some(Author::Agent(agent)) => Ok(Address::Branch(agent)),
                // A harness notice never expects a reply, so it cannot be
                // the oldest *open* post.
                _ => Err(format!(
                    "post #{} has no author to reply to",
                    question.as_u64()
                )),
            };
        };
        if to.as_str() == Some("user") {
            return Ok(Address::User);
        }
        // **`"parent"`, because a handle only points downwards.**
        //
        // `spawn()` hands the caller a handle to the child; nothing
        // hands the child one to whoever spawned it. So a child with a
        // question it cannot settle had exactly one address — `"user"`
        // — which reaches the person driving the session and nobody
        // else. In a headless run there is no such person, and the
        // child waits until the timeout.
        //
        // That is most of why steering a running child has never been
        // written: the child could not reach the only party watching
        // it. Everything after the address already worked — the post
        // lands on the parent's branch, arrives in its `# NEW EVENTS`
        // with an `[id]`, is counted by `list_agents`'s `open`, and is
        // discharged by `answer(question, value)`.
        //
        // The root has no parent, and saying so is better than quietly
        // meaning the user: a root that writes `ask("parent", …)` has
        // misunderstood where it sits, and a silent redirect would hide
        // that until the answer came back from the wrong mind.
        if to.as_str() == Some("parent") {
            let parent = tree
                .events
                .get(&self.agent)
                .and_then(|e| e.parent_id)
                .and_then(|p| tree.enclosing_agent(p));
            return match parent {
                Some(agent) => Ok(Address::Branch(agent)),
                None => Err(
                    "this agent has no parent — it is the root, and \"user\" is who it answers to"
                        .into(),
                ),
            };
        }
        // A handle, as `spawn()`/`fork()` hand it back — `{"agent": id}`.
        // Accepted whole, so `const h = spawn(...); await ask(h, ...)`
        // works, which is what the card teaches and an exemplar
        // demonstrates. Requiring `ask(h.agent, ...)` would make the
        // obvious spelling of the obvious task fail on an
        // implementation detail of the settlement value, which is
        // exactly the smell `DESIGN.md` names: if a model has to know a
        // mechanism exists to get the ordinary case right, the
        // mechanism is wrong.
        let to = to.get("agent").unwrap_or(to);
        // `"#16"` as well as `16`. Every id a program sees is *rendered*
        // `#16` — the artifact menu, the reports, the post lines — so a
        // model reading one back writes the form it was shown. Observed
        // live: a program held a fork from an earlier turn, found its id
        // in the menu, and wrote `ask("#16", …)`, which was refused. The
        // display teaches the spelling; the parser should accept it.
        let id = to.as_u64().or_else(|| {
            to.as_str()
                // **Whatever form the id was displayed in.** A model
                // writes back what it read, and it reads ids off the
                // menu, the rows and the reports. `#16` was accepted in
                // 2026-09-16 after a live run wrote `ask("#16", …)` and
                // was refused; the bracketed form is what those places
                // render now, so it has to address the same branch too.
                .map(|s| s.trim().trim_matches(|c| c == '#' || c == '[' || c == ']'))
                .and_then(|s| s.parse::<u64>().ok())
        });
        let Some(id) = id.filter(|n| *n > 0).map(EventId::new) else {
            return Err(format!(
                "the address must be an agent handle (from spawn()/fork()), a branch \
                 id, or \"user\"; got {to}"
            ));
        };
        match tree.events.get(&id).map(|e| &e.payload) {
            Some(EventPayload::Fork { .. }) => Ok(Address::Branch(id)),
            Some(EventPayload::Agent { .. }) => {
                let branches = tree.branches_of_agent(id);
                match branches.len() {
                    1 => Ok(Address::Branch(branches[0])),
                    _ => Err(format!(
                        "agent #{} has {} live branches ({}) — address one of them, \
                         not the agent",
                        id.as_u64(),
                        branches.len(),
                        branches
                            .iter()
                            .map(|b| format!("#{}", b.as_u64()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                }
            }
            _ => Err(format!("#{} is not an agent or a branch", id.as_u64())),
        }
    }

    fn running_vm(&mut self) -> &mut VM {
        match &mut self.phase {
            Phase::Running(run) => &mut run.vm,
            _ => unreachable!("no running VM"),
        }
    }

    /// Log a `Call` at dispatch and remember how to settle it. Returns the
    /// `Call` event's id — the log's own key, which the host echoes back
    /// with the result and which the artifact menu names.
    fn issue_call(&mut self, tree: &mut Tree, call: Call, slot: Slot) -> io::Result<EventId> {
        let logged = tree.append(&mut self.spine, EventPayload::Call(call))?;
        self.pending.insert(
            logged,
            PendingCall {
                slot,
                generation: self.generation,
            },
        );
        Ok(logged)
    }

    /// The call `fetch_history(id)` should **re-attach** to rather than read:
    /// one this session still has in flight, on this branch's own path.
    fn reattachable(&self, tree: &Tree, args: &[Value]) -> Option<EventId> {
        let Some(Value::PosInt(id)) = args.first() else {
            return None;
        };
        // **Zero is a number the model can write, and an id it cannot
        // have.** `EventId` is a `NonZeroU64`, so building one from a
        // program's own argument panics — and this was the one place
        // in the live path that built one without the `> 0` filter
        // every other site has. `history.fetch(0)` — or any expression
        // that arrived at 0, which is how it actually happened — took
        // the whole agent down with `expected non-zero EventId!`,
        // twice in 294 kept runs. Declining to re-attach hands the
        // call to the ordinary fetch path below, which already answers
        // a bad id with a message instead of a corpse.
        let id = EventId::checked(*id)?;
        // Scoped to this branch's own path, like every other fetch.
        let segment = self.agent_segment(tree);
        if !segment.iter().any(|e| e.id == id) {
            return None;
        }
        if self.pending.contains_key(&id) {
            return Some(id);
        }
        let is_open_send = matches!(
            tree.events.get(&id).map(|e| &e.payload),
            Some(EventPayload::Call(Call::Send { .. }))
        ) && settlement_of(&segment, id).is_none();
        // A **pre-fork** pending `Send` is not this branch's to re-await:
        // its `Result` lands on the branch that issued it, which this
        // path does not include, so the promise could never resolve.
        let inherited = tree
            .events
            .get(&id)
            .is_some_and(|e| self.pre_fork_pending(tree, e).is_some());
        (is_open_send && !inherited).then_some(id)
    }

    /// Serve `fetch_history(id)` from the log. Accepts a `Result` id or
    /// the id of the **call** it settles — the menu names calls, so a
    /// program reuses exactly the ids it was shown. Ids are scoped to
    /// this agent's spine segment (decision 3: never ancestor
    /// artifacts).
    ///
    /// **Every row, not just the calls.** Until 27.4 this served a
    /// `Result`, a settled `Call`, a `Return` and a `Console`, and
    /// refused everything else — so a `Post`, a `Note` and a program's
    /// own source were unreachable. That made a promise we had been
    /// repeating false in exactly the case it mattered: `compaction.rs`
    /// says it "never drops an id — only content", and the compaction
    /// request tells the model "nothing is deleted". For a compacted
    /// `Post` there was nothing to fetch, so removing one *was* a
    /// deletion.
    ///
    /// **A compacted row reads back whole**, and needs no code here to
    /// do it: compaction never touches its target — it appends a
    /// `Compacted` event that only the *renderer* consults
    /// (`document::pending_line`). This reads the log, so it reads the
    /// original. That asymmetry is the design: the document shrinks,
    /// the history does not.
    pub(crate) fn fetch_history(
        &self,
        tree: &Tree,
        args: &[Value],
    ) -> Result<serde_json::Value, String> {
        let id = match args.first() {
            Some(Value::PosInt(n)) => *n,
            _ => return Err("history.fetch needs a numeric id".into()),
        };
        let segment = self.agent_segment(tree);
        let Some(event) = segment.iter().find(|e| e.id.as_u64() == id) else {
            return Err(format!("no row #{id} in this agent"));
        };
        match &event.payload {
            EventPayload::Result { outcome, .. } => outcome_json(outcome),
            EventPayload::Call(_) => match settlement_of(&segment, event.id) {
                Some(outcome) => outcome_json(outcome),
                // Artifacts cross a `Fork`; **in-flight calls do not**.
                None => match self.pre_fork_pending(tree, event) {
                    Some(branch) => Err(format!(
                        "call #{id} is still pending on branch #{} — this fork inherited \
                         it as history, and its result will land there, not here. Issue \
                         your own call instead.",
                        branch.as_u64()
                    )),
                    None => Err(format!("call #{id} has no result yet")),
                },
            },
            // A handback carries no *return* value — a reply has no
            // `return` (D5) — but it is not empty: `Trapped` holds
            // `{kind, message, resumable}` and `Raised` holds
            // `{name, payload}`, which is exactly what a handler wants.
            //
            // **Serde, not `Debug`.** This used to hand back
            // `format!("{how:?}")` — Rust syntax in a JSON string, which
            // a program can only substring-match:
            //
            // ```text
            // Trapped { kind: "TypeError", message: "…", resumable: true }
            // ```
            //
            // The log already stores the same thing properly, so the
            // rendering was the only lossy step. `value_json` in this
            // file has a whole paragraph on why a Rust debug rendering
            // must not reach a program — a live run wrote a file whose
            // first line was the word `Undefined` — and this was the
            // same mistake one function over, model-facing.
            //
            // The terminal variants serialise to a bare string
            // (`"Completed"`), which is still "reads as the fact that it
            // happened"; the rich ones become an object a program can
            // index.
            EventPayload::Handback { how, .. } => serde_json::to_value(how)
                .map_err(|e| format!("handback #{id} has no JSON form: {e}")),
            // Not a menu row — it is named at the point it is
            // truncated, because it is context for one place rather than
            // work to be reused. Fetchable all the same.
            EventPayload::Console { lines } => Ok(serde_json::Value::Array(
                lines
                    .iter()
                    .map(|l| serde_json::Value::String(l.clone()))
                    .collect(),
            )),
            // The conversation's own rows. A `Post` resolves through
            // the tree first, because its body may live on the `Send`
            // that produced it rather than inline (`Message::Post`'s
            // `origin`) — the same `resolve` the renderer calls, so a
            // fetch and a render can never disagree about what a post
            // said.
            EventPayload::Post { origin, .. } => Ok(serde_json::Value::String(
                tree.resolve(origin)
                    .direct()
                    .map(|(t, _, _)| t)
                    .unwrap_or("")
                    .to_owned(),
            )),
            // A reply's own text, so a compacted one can be read back
            // by the reply that needs to know what it did (28: its parts
            // concatenated, which is what it is).
            EventPayload::Reply | EventPayload::Restart => {
                let path = tree.path_events(self.spine.leaf_id);
                let at = path.iter().position(|e| e.id.as_u64() == id);
                Ok(serde_json::Value::String(match at {
                    Some(at) => crate::report::reply_source(&path, at),
                    None => String::new(),
                }))
            }
            // **One block of a reply, as it was written.** The
            // document marks every block with `↓ history[N]`, and an
            // id the model can see has to be an id it can read:
            // without this arm the marker would name a row `fetch`
            // says does not exist, which is the papercut the
            // `append`/`fetch` round-trip was.
            //
            // Thinking carries no marker — it is on the log and not in
            // the document — so nothing can name it, and it falls
            // through to the refusal below with everything else that
            // is not a row.
            EventPayload::Part { part, .. } => match part {
                Part::Prose(t) | Part::Cell(t) => Ok(serde_json::Value::String(t.clone())),
                Part::Thinking(_) => Err(format!("#{id} is not a row of this conversation")),
            },
            // **Whole, as the card promises.** What was appended
            // comes back as it went in — not as its JSON text, which is
            // what a program had to know to `JSON.parse` before 28.
            EventPayload::Note { value, .. } => Ok(value.clone()),
            // Genuinely not a row: the agent's own root, a `Compacted`
            // event at its own position, structure. `document::label_of`
            // has no name for these either, and `compaction.rs` refuses
            // them for the same reason.
            _ => Err(format!("#{id} is not a row of this conversation")),
        }
    }

    /// The branch that owns a still-pending call, when this branch is a
    /// fork that inherited it: the call sits before this branch's root.
    fn pre_fork_pending(&self, tree: &Tree, call: &Event) -> Option<EventId> {
        let root = tree.branch_of(self.spine.leaf_id)?;
        if call.id.as_u64() >= root.as_u64() {
            return None; // issued on this branch
        }
        tree.branch_of(call.id)
    }

    fn finish_program(
        &mut self,
        tree: &mut Tree,
        value: Value,
        unstarted: Vec<InvokeCall>,
        mut out: Vec<StepOutput>,
    ) -> io::Result<Vec<StepOutput>> {
        // Fire-and-forget calls the program never awaited: classified
        // exactly like any other promise-holding call (`dispatch_calls`),
        // **while the VM is still `Running`**, so `tell` and `ask` land
        // as themselves instead of silently demoting to a generic
        // `Call::Invoke` sent to the tool registry (which has no such
        // tool and answers "unknown tool `tell`").
        //
        // Only those two and `tools.*` can be here at all now: a
        // settle-at-dispatch verb never enters the outbox, so there is
        // no such thing as an unstarted `spawn`. This used to build
        // `Call::Invoke` unconditionally for every unstarted call — the
        // bug 23_ONE_AGENT.md's Pass B flagged as a confirmed regression:
        // an unawaited `tell()` reached here, not `dispatch_calls`'s
        // `TOOL_ASK | TOOL_TELL` arm, because this was a second,
        // parallel classifier that never got the memo. The host still
        // decides whether to actually run them; the generation bump right
        // after keeps every one of them log-only — the program can no
        // longer observe them, whichever kind of call they turned out to
        // be.
        self.dispatch_calls(tree, unstarted, &mut out)?;
        self.generation += 1;

        // A compaction handler's batch commits here, at the end of the
        // program that built it, because that is when it is complete.
        // Nothing about the ops is applied before this point, so a
        // handler that traps or is abandoned halfway leaves the log
        // exactly as it found it.
        // Best effort, and nothing is reported back: the ops that name a
        // real entry apply, the rest are dropped, and whether the
        // document actually got smaller is something the next request
        // answers by being smaller. If it is still over budget,
        // `compaction_if_needed` notices that on its own terms rather
        // than on the strength of a refusal.
        self.apply_history_edits(tree)?;

        let Phase::Running(run) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            unreachable!()
        };
        // `undefined` has no JSON form, so `stack_value_to_json` rightly
        // refuses it — but a program that simply ends without a `return`
        // is the *ordinary* case, not an unrepresentable value, and
        // `types.rs` promises it logs `Return { value: null }`: that is
        // what makes "completed ⇒ Return" decidable from the log alone.
        // Falling through to the debug-repr fallback logged the string
        // `"Undefined"` instead, indistinguishable from a program that
        // really did return that text.
        // **`finish()` is a flag on the VM**, not a halt the host was
        // handed — so it is read here, while the run is still in hand,
        // rather than arriving as a `StepResult`. That is what lets a
        // `finish()` written before the last `tell` rest the branch and
        // still let the `tell` go out.
        // **Whose program this is**, kept before `run` is consumed: it
        // is what every handback below names, and after a resume it is
        // the reply that first ran the frame rather than the newest one.
        let finishing = run.program_id;
        self.finished = run.vm.finished;
        let value_json = if matches!(value, interp::Value::Undefined) {
            serde_json::Value::Null
        } else {
            run.vm
                .stack_value_to_json(&value, 0)
                .unwrap_or_else(|_| serde_json::Value::String(format!("{value:?}")))
        };

        // **C0a (23_ONE_AGENT.md): a tagged completion is a decision about
        // the suspended run beneath this one, not this program's own
        // result.** `interp`'s compiler gives `resume(v)`/`abandon()`
        // exactly one shape each — a plain object carrying `__decision`
        // (`call.rs`) — and this is the one place that tag is read back.
        // Checked only now, at completion, and not any earlier: a handler
        // may do other work first (`answer(...)`, a `tell()` — see
        // `upward_clarification_does_not_deadlock`) before deciding, or
        // may never decide at all, and only its own return value says
        // which. A handler that returns something untagged is an
        // ordinary program completion; it does not implicitly resume
        // anything (DESIGN.md's thesis table takes the tag as the whole
        // interface, on purpose — an implicit resume would feed a live
        // program a value nobody actually chose).
        // The reply's own value if it had one (the program transport's
        // shape, still what a hand-typed `take_turn` produces), else a
        // decision handed to `history.note` during the run.
        let appended = self.pending_decision.take();
        let value_json = match (&appended, decision_tag(&value_json)) {
            (Some(d), None) => d.clone(),
            _ => value_json,
        };
        let decision = value_json.get("__decision").and_then(|v| v.as_str());
        if matches!(decision, Some("resume") | Some("abandon")) && !self.parked.is_empty() {
            let decision = decision.expect("checked Some above").to_owned();
            // The frame stays on `parked` for `resume`/`abandon` to take.
            // It used to be popped here and written straight back into
            // `Phase::Suspended` so those two could read it out again —
            // a write whose only reader was the next line.
            let home_generation = self
                .parked
                .last()
                .expect("checked non-empty above")
                .generation;
            // This program's own execution genuinely happened — its
            // status is `Completed` and its final VM is kept for the
            // sticky debugger pane like any other — but it gets no
            // `Return`/`Console` row of its own: `Runner::resume`'s own
            // doc is explicit that nothing new is *said* by a decision,
            // and `program_status_survives_reopen` (tree.rs) already
            // fixes this exact shape — the resumed run's *own* eventual
            // `Return`/`Console` are what a report is derived from, not
            // this one's.
            self.note_status(finishing, ProgramStatus::Completed);
            // **A decision is still an ending.** Until now this program
            // logged no terminal of its own — the only handback under
            // it was the discard, which names the frame it discarded —
            // so nothing on the log said how *it* ended and a reopened
            // session read it as running for good. Its console went the
            // same way.
            //
            // The implicit-supersede path below has always logged both
            // (a `Superseded`, then its own `Completed`), and that is
            // the common case: 93 of 95 traps in the kept corpus were
            // answered by rewriting rather than by deciding. This is
            // the rare path catching up with the ordinary one, so what
            // the model reads gets *less* varied, not more.
            let console =
                run.vm.console_lines[run.console_logged.min(run.vm.console_lines.len())..].to_vec();
            self.last_vm = Some(run.vm);
            tree.append(
                &mut self.spine,
                EventPayload::Handback {
                    program: finishing,
                    how: Handback::Completed {
                        // The decision is not a result for anybody: it
                        // is an instruction to the harness, and the
                        // frame it names says what it did.
                        value: None,
                        rested: false,
                    },
                    site: 0,
                    stack: Vec::new(),
                },
            )?;
            tree.append(&mut self.spine, EventPayload::Console { lines: console })?;
            if decision == "resume" {
                // Revive the old run's own in-flight calls. They were
                // dispatched under `home_generation`, which this
                // handler's own start-and-finish already left behind
                // (two bumps: `apply_turn` starting it, this function
                // finishing it) — without re-stamping them, a still
                // -pending exchange the old run was waiting on (the
                // parent's own `ask()` in `upward_clarification_does_
                // not_deadlock`) would land marked stale and be routed
                // to rule C instead of delivered, even though the VM
                // that issued it is very much still the one running.
                // `abandon` deliberately skips this: its whole point is
                // that in-flight calls settle as artifacts nobody
                // receives (`crate::types::Handback::Abandoned`'s own doc), which is
                // exactly what leaving their generation stale achieves.
                for pending in self.pending.values_mut() {
                    if pending.generation == home_generation {
                        pending.generation = self.generation;
                    }
                }
            }
            // Mark the handler's own exchange as accounted-for before
            // handing off — the same advance the ordinary completion
            // path below makes before going idle. `Runner::abandon`
            // calls `prompt_if_needed` itself, whose crash-recovery
            // clause (`last_turn_outcome(tree) > self.shown`) would
            // otherwise see *this* handler's own `Turn`, still ahead of
            // a `shown` last advanced at the original suspend, followed
            // by the fresh `crate::types::Handback::Abandoned` `abandon()` is about to
            // log — indistinguishable from a genuinely new, unshown
            // completion — and fire a spurious prompt for an exchange
            // the branch has already fully seen.
            self.shown = self.spine.leaf_id.as_u64();
            let decision_value = value_json
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let mut routed = match decision.as_str() {
                "resume" => self.resume(tree, decision_value)?,
                "abandon" => self.abandon(tree)?,
                _ => unreachable!("matched Some(\"resume\") | Some(\"abandon\") above"),
            };
            out.append(&mut routed);
            return Ok(out);
        }
        // Not a decision — either untagged, or tagged with nothing
        // beneath to decide about (a root program's own `resume(...)`/
        // `abandon()` misuse: DESIGN.md's Part D2 flags this as a
        // real mistake worth its own compile/runtime error, not yet
        // built — logged here as an ordinary, if odd-looking, `Return`
        // rather than guessed at). If something **is** still stashed
        // beneath this completion regardless (a genuine rewrite: a new
        // program that never called `resume`/`abandon` at all, run
        // straight over a still-suspended raise), that suspension is
        // implicitly discarded now — the same "a fresh completion
        // silently replacing a suspended one" case `apply_turn` used to
        // close eagerly, moved here because the deciding fact (did this
        // program decide, or not) isn't known until this point.
        if let Some(Parked { run: old_run, .. }) = self.parked.pop() {
            let discarded = old_run.program_id;
            self.note_status(discarded, ProgramStatus::Failed);
            self.last_vm = Some(old_run.vm);
            tree.append(
                &mut self.spine,
                EventPayload::Handback {
                    program: discarded,
                    // **Superseded, not abandoned.** Nothing decided
                    // this: the reply simply wrote a new program over a
                    // suspended one. Saying a handler abandoned it
                    // describes a deliberate act by an agent that does
                    // not exist.
                    how: Handback::Superseded,
                    site: 0,
                    stack: Vec::new(),
                },
            )?;
        }

        // "Completed ⇒ a terminal `Handback`" holds without exception,
        // which is what makes recovery decidable from the log alone.
        //
        // Two facts ride it. **What the program returned**, if it ran a
        // top-level `return` with a value — `undefined` says nothing
        // and is not recorded. And **whether it rested**: `finish()`
        // sets a flag, and the flag is honoured only if this reply put
        // something in front of the person. A branch that rests having
        // said nothing to anybody is the failure the argument on the
        // old `finish(text)` existed to prevent (measured at 1 in 12
        // runs, and 4 in 12 once a card line talked the models out of
        // `tell`); here the reply is simply not rested and the tail
        // says why, which costs a round trip instead of the answer.
        // Already rendered above, and `Null` for a program that ran off
        // its end or returned `undefined` — neither said anything, so
        // neither is recorded.
        let returned = match &value_json {
            serde_json::Value::Null => None,
            v => Some(v.clone()),
        };
        let rested = self.finished && self.said_something(tree);
        let outcome = tree.append(
            &mut self.spine,
            EventPayload::Handback {
                program: finishing,
                how: Handback::Completed {
                    value: returned,
                    rested,
                },
                site: 0,
                stack: Vec::new(),
            },
        )?;
        // The console is a diagnostic stream, capped with an explicit
        // marker; the report carries only a bounded tail of it.
        //
        // **Only what this handback has not already logged.** A run
        // that paused on a `raise` wrote a `Console` then, and
        // `console_lines` is never cleared — so this one repeated
        // everything the first already carried, and the report here
        // replayed output the model read a reply earlier. See
        // `Run::console_logged`.
        tree.append(
            &mut self.spine,
            EventPayload::Console {
                lines: crate::report::cap_console(
                    &run.vm.console_lines[run.console_logged.min(run.vm.console_lines.len())..],
                    &format!("console event follows #{}", outcome.as_u64()),
                ),
            },
        )?;

        self.note_status(run.program_id, ProgramStatus::Completed);
        self.last_vm = Some(run.vm);
        // `document.rs::render` derives the completion report straight
        // off this `Return` event on every render (`derive_report`) — no
        // separate "tool result"/harness `Post` for it to answer, and no
        // subject to force one either.
        //
        // **A program completing continues the conversation by
        // default, on both transports.** `shown` is what decides that:
        // left where it was, `needs_prompt`'s outcome clause sees this
        // `Return` as unshown and `prompt_if_needed` (below) renders the
        // next request; advanced to the leaf, the branch rests. Before
        // this it went the other way — `Transport::Program` advanced
        // unconditionally (rest by default) and only `Transport::
        // RunProgram` skipped it (97a2622, patching the mirror-image
        // bug: there, advancing `shown` here made the just-logged
        // completion read as already accounted for, and the
        // conversation would simply stop, silently, one round trip
        // after every completed program). Both readings picked a
        // default for the same lever; this drops the split and picks
        // one default for both, because the failure the old default
        // invited is worse than the one this one invites: the dominant
        // observed failure across every card variant and both models is
        // a program that does one step and stops, abandoning the task —
        // and with rest as the default, the easiest accident (an
        // ordinary `return` with nothing left to say) causes exactly
        // that, most expensive, failure. `finish(text)` (`TOOL_DONE`) is the
        // opt-in the other way: rest happens only when a program
        // actually said so, so an accidental continuation costs one
        // visible, self-correcting turn instead.
        //
        // Continuing needs nothing else from here: the program's return
        // value is already rendered into the next request by the
        // completion report (`document::render`'s own fold), and
        // `render_request` (called from `prompt_if_needed`, below)
        // advances `shown` itself the moment that request actually goes
        // out, exactly as it always has.
        // **Only an honoured `finish()` rests.** One from a reply that
        // told nobody anything is remembered instead, so the next
        // request's tail can say why the branch is still going.
        self.finish_ignored = self.finished && !rested;
        if rested {
            // Matches `suspend`'s own depth>0 branch precedent: `shown`
            // advances here, marking this outcome accounted-for so
            // `needs_prompt`'s crash-recovery clause doesn't spuriously
            // re-fire for a completion this file just handled
            // synchronously (that clause is for a reopened log's
            // genuinely stale `shown`, not for "immediately after I
            // logged this myself").
            self.shown = self.spine.leaf_id.as_u64();
        }
        self.finished = false;
        self.phase = Phase::Idle;
        out.extend(self.prompt_if_needed(tree)?);
        Ok(out)
    }

    fn suspend(
        &mut self,
        tree: &mut Tree,
        cause: SuspendCause,
        out: Vec<StepOutput>,
    ) -> io::Result<Vec<StepOutput>> {
        let Phase::Running(mut run) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            unreachable!()
        };

        // Split the live suspension in two: a serialisable `Cause` for
        // the log (everything the report needs) and a `ResumeWith` handle
        // the *live* VM needs to resume. A `VMError` is not serialisable
        // and only a live VM can consume one, so the two cannot be the
        // same value.
        let (cause, site, suspension) = match cause {
            SuspendCause::Raise { condition, payload } => {
                let payload = payload.map(|v| value_json(&run.vm, &v));
                // `step()` advanced `ip` past the `Raise`, so the raise
                // site is the previous slot.
                let site = span_at(&run.vm, (run.vm.ip as usize).saturating_sub(1));
                (
                    Handback::Raised {
                        name: condition,
                        payload,
                    },
                    site,
                    ResumeWith::Raise,
                )
            }
            SuspendCause::Trapped(e) => {
                let site = span_at(&run.vm, e.ip as usize);
                let cause = Handback::Trapped {
                    kind: format!("{:?}", e.kind),
                    message: e.message.clone(),
                    resumable: matches!(e.resume, ResumeMode::PushValueThenContinue),
                };
                (cause, site, ResumeWith::Trapped(e))
            }
            SuspendCause::CellCompileFailed(report) => {
                // `ip` is parked on the failed cell's append position, which
                // has no instruction yet — so there is no span to point at,
                // and a zero-width site is the convention for exactly that.
                (
                    Handback::CellFailed { message: report },
                    0,
                    ResumeWith::Continue,
                )
            }
            SuspendCause::Posted(ids) => {
                let site = span_at(&run.vm, run.vm.ip as usize);
                // A post arriving is never a tail call — there is no
                // "handler" in the raise/resume sense here, just the
                // running program parking until its next fuel slice
                // (rule B). `Pushed` is simply correct.
                (Handback::Posted { ids }, site, ResumeWith::Continue)
            }
        };

        // **A handback's site is an offset into the reply**, exactly
        // as a `Call`'s is (28, "Sites"). The spans above come off the
        // VM, so they are offsets into the *parse buffer* — the prelude
        // and then the reply — and the prelude has to come off them
        // here, where `run` still holds the notebook that knows how
        // long it is.
        //
        // Left un-rebased until 2026-09-19, when a live `glm-5.3` run
        // trapped on a `ReferenceError` and the report read
        // `12:1: test is not defined` over a **blank line and a bare
        // caret**: line 12 of the parse buffer is inside the prelude,
        // and the report renders against the reply, which is ten lines
        // long. Every trap, raise and post report has been pointing
        // into the prelude — the one diagnostic phase 28 exists to make
        // nameable.
        let site = run
            .notebook
            .as_ref()
            .map_or(site, |nb| nb.rebase_site(site));
        self.pause_falsifies_the_rest = matches!(
            cause,
            Handback::Trapped { .. } | Handback::CellFailed { .. }
        );
        let stack: Vec<String> = run
            .vm
            .frames()
            .iter()
            .map(|f| f.name().to_owned())
            .collect();
        let console =
            run.vm.console_lines[run.console_logged.min(run.vm.console_lines.len())..].to_vec();
        run.console_logged = run.vm.console_lines.len();
        let program_id = run.program_id;
        // A handover does not park: there is nothing to come back to,
        // and nothing will resume it --
        // so the VM is dropped here rather than left `Suspended` for a
        // decision that is never coming. Two things follow, both wanted:
        // the branch goes `Idle`, so the ordinary "unseen outcome on the
        // last turn" rule asks for the next program (rather than
        // `prompt_suspended`'s one-shot handler prompt, which would
        // re-state a report the rolling document now already carries);
        // and no run is left for `apply_turn` to discard, so no
        // `crate::types::Handback::Abandoned` is logged for a program that finished on
        // purpose.
        let handed_over = cause.is_terminal();
        if handed_over {
            self.last_vm = Some(run.vm);
            self.phase = Phase::Idle;
            self.note_status(program_id, ProgramStatus::Completed);
        } else {
            // The branch itself is idle — nothing is executing and no
            // request is out yet; the host's `prompt_suspended` is what
            // puts one out, and it can now say so in `phase` without
            // touching the frame.
            self.phase = Phase::Idle;
            self.parked.push(Parked {
                run,
                resume_with: suspension,
                generation: self.generation,
            });
            self.note_status(program_id, ProgramStatus::Suspended);
        }
        // The outcome carries the site and the stack because those were
        // the last inputs that lived only in the VM, and the VM is never
        // persisted.
        let outcome = tree.append(
            &mut self.spine,
            EventPayload::Handback {
                program: program_id,
                how: cause,
                site,
                stack,
            },
        )?;
        tree.append(
            &mut self.spine,
            EventPayload::Console {
                lines: crate::report::cap_console(
                    &console,
                    &format!("console event follows #{}", outcome.as_u64()),
                ),
            },
        )?;
        // A handover is the exception to everything below: it does ask,
        // here, through the ordinary door. Its `Handover` disposition
        // opens no scope, so the rolling document *does* show the report
        // (unlike the `Pushed` case the rest of this comment describes),
        // and no run is parked for the host to build a one-shot prompt
        // around. `shown` is deliberately left where it is so
        // `needs_prompt` sees an unseen outcome on the last turn and
        // asks for the next program — which is the whole verb.
        if handed_over {
            let mut out = out;
            out.extend(self.prompt_if_needed(tree)?);
            return Ok(out);
        }

        // Deliberately **no** `StepOutput::LlmRequest` here. Read
        // `document.rs` (already finished by the concurrent A3 agent)
        // before assuming otherwise: `document::render`'s fold treats a
        // `Pushed`-disposition `Condition` as entering a nested,
        // *invisible* scope (`depth_after` increments past it, and
        // nothing renders again until a matching `Return` brings depth
        // back to 0) — "root programs are rendered; handler programs are
        // not". Since `suspend` always logs `Pushed` here (this file has
        // no way to detect a real tail-call handover — see the flag
        // above), the ordinary rolling `Document` would show **nothing
        // new** for this branch right now regardless of whether we ask
        // for one. The status transition already recorded above
        // (`note_status(.., Suspended)`) is the real signal: it is the
        // host's job, not this file's, to build whatever one-shot
        // handler-triggering prompt it needs from the `Condition` event
        // directly (`report::derive_report` on `outcome`), separate from
        // this branch's rolling document.
        //
        // `shown` still advances, though, exactly as `render_request`
        // would have: this outcome is accounted for by the trigger rule
        // even though this file isn't the one acting on it, so
        // `needs_prompt`'s crash-recovery clause doesn't re-fire for it
        // on the next ordinary call (e.g. from `Runner::abandon`, which
        // goes back through `prompt_if_needed` once the parked run is
        // dropped).
        self.shown = self.spine.leaf_id.as_u64();
        Ok(out)
    }

    /// Render a request iff the trigger rule says to. The one door an
    /// idle branch re-enters its LLM through, so "never woken without a
    /// cause" holds in one place.
    fn prompt_if_needed(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        if !self.needs_prompt(tree) {
            return Ok(Vec::new());
        }
        // **Say why, before asking again.** A reply that stopped short
        // gets woken with a harness post naming what went wrong, so the
        // next one is written against the mistake rather than repeating
        // it blind. Logged here because this is the one door an idle
        // branch re-enters its LLM through, and the only place with a
        // `&mut Tree` to log into.
        //
        // Appended directly rather than through `deliver`: the wake is
        // what *this* function is in the middle of doing, and `deliver`
        // comes back here to decide whether to do it — which is a cycle
        // with no base case, and was one until the stack overflowed.
        if let Some(text) = self.stopped_short(tree) {
            tree.append(
                &mut self.spine,
                EventPayload::Post {
                    from: Author::Harness,
                    origin: Origin::Direct {
                        text,
                        input: serde_json::Value::Null,
                        options: Vec::new(),
                        expects_reply: false,
                    },
                },
            )?;
        }
        // Checked before the request is built, not after: a document
        // over budget is over budget *for this request*, and the whole
        // point is to not send it. The handler's own request is built
        // from the compacted document on the next pass through here.
        let budget = self.document_budget();
        let headroom = crate::host::compaction_headroom();
        if let Some(request) = self.compaction_if_needed(tree, budget, headroom)? {
            return Ok(vec![request]);
        }
        self.await_llm();
        Ok(vec![self.render_request(tree)])
    }

    /// A request is out.
    ///
    /// This was a bare `self.phase = Phase::AwaitingLlm` at both call
    /// sites, then briefly a guarded one (`4898a80`) because the stamp
    /// could land on a `Phase::Suspended(run, _)` and drop the parked
    /// run. `Phase` no longer carries a run, so there is nothing to
    /// guard against and the guard is gone.
    pub(crate) fn await_llm(&mut self) {
        self.phase = Phase::AwaitingLlm;
    }

    /// Fire a compaction condition if the document has outgrown its
    /// budget, returning the request that asks for the handler.
    ///
    /// Checked here, at the one door an idle branch re-enters its LLM
    /// through, because that is the only moment the size is both known
    /// Compaction attempts since the last one that committed — **a fold
    /// over this branch's path, not a counter.**
    ///
    /// A document has a floor no handler can reach: the card and the
    /// worked examples open every request and are not rows, so a budget
    /// set near that floor makes every batch fail `StillOverThreshold`,
    /// and the condition would re-fire on the next prompt, forever.
    /// After [`COMPACTION_ATTEMPTS`] the branch stops asking and carries
    /// on over budget, which is the lesser failure: an over-long
    /// document still works, an infinite loop of compaction programs
    /// does not.
    ///
    /// It was a `u32` on this struct until a live log accumulated seven
    /// compaction conditions — the bound is per *process*, and the
    /// branch outlives the process. Reading it off the log instead is
    /// the same move the reports and the menu already make: the
    /// evidence for "we have tried twice" is in the log, so nothing
    /// needs to remember it. A `Compacted` event resets the count for
    /// the obvious reason — the next time the document grows, that is a
    /// fresh problem and not the same one again.
    fn compaction_attempts(&self, tree: &Tree) -> u32 {
        let mut attempts = 0;
        for ev in tree.path_events(self.spine.leaf_id) {
            match &ev.payload {
                EventPayload::Compaction { .. } => attempts += 1,
                EventPayload::Compacted { .. } => attempts = 0,
                _ => {}
            }
        }
        attempts
    }

    /// and actionable: a document is only ever too large *for a request*,
    /// and this is where requests are made.
    ///
    /// Returns `None` when there is nothing to do — under budget, or
    /// already compacting, which is what stops a handler that fails to
    /// shrink anything from firing itself again forever.
    fn compaction_if_needed(
        &mut self,
        tree: &mut Tree,
        budget: usize,
        headroom: f64,
    ) -> io::Result<Option<StepOutput>> {
        if self.compaction_requested || self.compaction_attempts(tree) >= COMPACTION_ATTEMPTS {
            return Ok(None);
        }
        let doc = crate::document::render(tree, &self.spine, budget);
        let rendered = crate::compaction::rendered_size(&doc);
        self.last_rendered_bytes = Some(rendered);
        // **One trigger, and it is the counted one where it exists.**
        // [`Runner::next_prompt_floor`] is two counted numbers added
        // together — the prompt the provider charged for, and the part
        // of the reply that survives into the document — so against a
        // configured window it is the whole decision: no tokenizer, no
        // ratio, no guess about content this crate cannot see.
        //
        // Checked here, which is after the cells have run and their
        // reports have landed, so the *byte* path sees everything this
        // turn added. The count cannot: usage arrives at the end of the
        // stream and describes the request that started it. What the
        // cells appended in between is the gap, and the headroom is
        // what covers it.
        //
        // **And the byte budget does not get a vote once that holds.**
        // Running both means the tighter one decides, and the tighter
        // one is the byte budget: the default 64 KB fires at 49,152
        // rendered bytes, somewhere near 14k tokens, while a 64k-token
        // window (less the reserve, less the headroom) allows about
        // 43k. The count would simply never be reached. Measured on the
        // 2026-09-19 hosted baseline, compaction fired once in 21 runs
        // and it fired on bytes, at 54,905 of 65,536.
        //
        // The byte path stays for the two cases where there is nothing
        // to count against: no window configured, and no reply has
        // reported a `prompt_tokens` yet.
        let Some((measured, limit, unit)) = self.fullness(tree, crate::host::context_tokens(), rendered, budget) else {
            return Ok(None);
        };
        self.last_fullness = Some((measured, limit, unit));
        if !crate::compaction::should_fire(measured, limit, headroom) {
            return Ok(None);
        }
        // **Ask only when asking can help.** The card and the worked
        // examples carry no id, so they are a floor no handler can get
        // under, and a budget near it makes every round succeed at
        // removing rows and fail to shrink anything.
        // `COMPACTION_ATTEMPTS` does not bite there because it counts
        // fires since the last *success*, and those rounds succeed.
        // Measured on `sweep-200` with a 34,000-byte budget against a
        // 25,792-byte floor: thirteen compactions, 49 rows removed, the
        // document never once below the floor.
        //
        // Bytes only. The floor is a byte measurement and the token
        // path's limit is in tokens, and converting between them is the
        // guess this trigger exists to avoid — against a real context
        // window the floor is far under it anyway, which is the case
        // this never fires in.
        if unit == Measure::Bytes
            && crate::compaction::should_fire(
                crate::compaction::floor_size(tree, &self.spine, budget),
                limit,
                headroom,
            )
        {
            return Ok(None);
        }
        tree.append(
            &mut self.spine,
            EventPayload::Compaction {
                measured,
                limit,
                unit,
            },
        )?;
        self.compaction_requested = true;
        self.await_llm();
        Ok(Some(self.render_request(tree)))
    }

    /// **How full this conversation is, in the one unit that decides.**
    /// `None` when that cannot be answered yet.
    ///
    /// Shared by the compaction trigger and the tail's readout, because
    /// the day they were two functions they disagreed: the readout fell
    /// back to bytes-against-the-document-budget whenever nothing had
    /// cached a measurement, so `agent document` on a run whose model
    /// has a 1M-token window announced "133% full" about a request the
    /// model saw as 2%. An instrument that contradicts the request is
    /// worse than one that is silent.
    ///
    /// `None` is the silence: a window is configured and no reply has
    /// reported a `prompt_tokens` against it yet, which is every
    /// reopened log. Substituting the other unit there is what went
    /// wrong — see [`Counted::Stale`] for the live run that first made
    /// the point.
    ///
    /// `configured` is the host's window, passed rather than read, so
    /// the rule is a pure function of its inputs and a test of it does
    /// not have to set a process-wide environment variable. It did, and
    /// the variable leaked into whichever other test happened to be
    /// running: `compaction_does_not_fire_while_already_compacting`
    /// failed about one run in three.
    fn fullness(
        &self,
        tree: &Tree,
        configured: Option<usize>,
        rendered: usize,
        budget: usize,
    ) -> Option<(usize, usize, Measure)> {
        // **Live state first, then the log.** A reopened log has no
        // `next_prompt_floor` and no environment, but it does have the
        // count and the window the last request actually carried —
        // which is why `Usage` records both. Without the second half,
        // rendering a past request had to guess, and guessed in bytes.
        let (context, counted) = match (configured, self.next_prompt_floor) {
            (Some(context), Counted::Floor(tokens)) => (Some(context), Some(tokens)),
            (window, Counted::Never) => match self.logged_usage(tree) {
                Some(usage) => (
                    usage.window.map(|n| n as usize).or(window),
                    Some(usage.prompt),
                ),
                None => (window, None),
            },
            (window, _) => (window, None),
        };
        match (context, counted) {
            (Some(context), Some(tokens)) => {
                let usable = context
                    .saturating_sub(crate::host::completion_reserve())
                    .min(crate::host::max_document_tokens());
                // **A count describes the request that returned it, and
                // the program that ran since then has been appending.**
                // Usage arrives at the end of a stream, so the trigger
                // holds a number from one request ago while `keep`,
                // `note` and a reply's own blocks grow the next one —
                // and a `keep` is large: three kept files put 96 KB in
                // a document on 2026-09-23, roughly 24k tokens, against
                // a window of 24k, and the trigger let it through
                // holding a count of 8,554 from the turn before.
                //
                // So: the count, or what this conversation's own
                // measured density says the document is worth now,
                // whichever is larger. Not a bytes-to-tokens constant —
                // the ratio comes from the last request's own bytes and
                // its own charged tokens, and is recomputed every
                // reply.
                let implied = self
                    .bytes_per_token
                    .filter(|d| *d > 0.0)
                    .map_or(0, |d| (rendered as f64 / d) as usize);
                Some(((tokens as usize).max(implied), usable, Measure::Tokens))
            }
            // A window, and nothing counted against it. Silence: the
            // other unit is a different question with a different
            // answer, and answering it here is what went wrong.
            (Some(_), None) => None,
            // No window anywhere: bytes are the only budget there is,
            // and the one the trigger uses.
            (None, _) => Some((rendered, budget, Measure::Bytes)),
        }
    }

    /// The usage of the newest reply on this path — the count, and the
    /// window it was counted against. What lets a reopened log measure
    /// itself the way the live session did.
    fn logged_usage(&self, tree: &Tree) -> Option<crate::host::Usage> {
        tree.path_events(self.spine.leaf_id)
            .iter()
            .rev()
            // A reply that reported nothing counted is not an answer:
            // `prompt` of zero is the absence of a measurement, not a
            // measurement of zero.
            .find_map(|e| match &e.payload {
                EventPayload::ReplyEnd { usage, .. } if usage.prompt > 0 => Some(*usage),
                _ => None,
            })
    }

    /// What the last `keep`/`peek` of this row chose to show.
    ///
    /// A result never changes, so re-applying the projection that was
    /// used before would produce exactly this — which is why asking for
    /// a row again without saying how to cut it can simply reuse it,
    /// and why that is not an approximation.
    fn last_rendered_value(&self, tree: &Tree, of: EventId) -> Option<serde_json::Value> {
        tree.path_events(self.spine.leaf_id)
            .iter()
            .rev()
            .find_map(|e| match &e.payload {
                EventPayload::Render { of: o, value, .. } if *o == of => Some(value.clone()),
                _ => None,
            })
    }

    /// Commit a finished compaction handler's batch, or say why not.
    ///
    /// `compaction::compact` validates the whole batch against the log
    /// before anything is appended, so this either appends every
    /// `Compacted` event or none of them. A rejection is returned as a
    /// string for the caller to hand back to the model, which is a
    /// retry rather than a failure: the log is untouched either way.
    fn apply_history_edits(&mut self, tree: &mut Tree) -> io::Result<usize> {
        let ops = std::mem::take(&mut self.pending_edits);
        let was_a_compaction_program = self.compaction_requested;
        self.compaction_requested = false;
        if ops.is_empty() {
            return Ok(0);
        }
        // **The count this commit invalidates is the count that fired
        // it.** `usage.prompt` describes the request that was sent, and
        // the request that was just sent was the pre-compaction
        // document — so leaving it in place would have
        // `compaction_if_needed` read the old size off a document that
        // has since shrunk and ask for a second handler on the
        // strength of it. Dropping it falls back to the byte check for
        // exactly one turn, which measures the real document, and the
        // next reply brings a count that does too.
        self.next_prompt_floor = Counted::Stale;
        let events = crate::compaction::compact(tree, &self.spine, &ops);
        let n = events.len();
        for event in events {
            tree.append(&mut self.spine, event)?;
        }
        if was_a_compaction_program {
            self.compact_the_compaction_program(tree)?;
        }
        Ok(n)
    }

    /// **A compaction program is the one reply that is not conversation**
    /// — it is work the harness asked for, in a document the harness
    /// asked to be made smaller — so it goes when it is spent.
    ///
    /// Shadowed, like everything else: the blocks stay on the log and
    /// `history.fetch` still answers for them. What goes is their place
    /// in the rendered document.
    ///
    /// **Because leaving it there taught the model to repeat it.** Live
    /// on `Qwen3.8-27B`, 2026-09-20: its first compaction program read
    /// `history.remove(4); history.remove(5); history.remove(9, 12);`
    /// and worked. Two rounds later it wrote `history.remove(9);
    /// history.remove(12);` — the same rows, which by then rendered
    /// nothing. The only place those ids still existed was the spent
    /// program sitting in its own history, and a model writing a
    /// compaction program imitates the compaction program in front of
    /// it.
    ///
    /// Anything the program *said* survives: a `tell` and a
    /// `history.note` are rows of their own, and the summary a good
    /// compaction leaves behind is exactly such a row.
    fn compact_the_compaction_program(&mut self, tree: &mut Tree) -> io::Result<()> {
        let reply = self.reply_id;
        let blocks: Vec<EventId> = tree
            .path_events(self.spine.leaf_id)
            .iter()
            .filter(|e| {
                matches!(
                    &e.payload,
                    EventPayload::Part {
                        reply: r,
                        part: Part::Prose(_) | Part::Cell(_),
                    } if *r == reply
                )
            })
            .map(|e| e.id)
            .collect();
        for of in blocks {
            tree.append(
                &mut self.spine,
                EventPayload::Compacted { of, text: None },
            )?;
        }
        Ok(())
    }

    // ── rendering ───────────────────────────────────────────────────

    /// The ephemeral half of a request — see [`LlmRequest`]'s own doc.
    /// The card, system prompt and message history are `document.rs`'s
    /// job now; this only marks `shown` (so "never prompted twice for
    /// the same thing" holds by construction) and computes the trailing
    /// line.
    ///
    /// `pub(crate)` rather than private: `host/mod.rs`'s one-shot
    /// handler prompt (built when a run suspends into a `Condition` —
    /// `suspend`'s own doc explains why that file never renders one
    /// itself) still needs `shown` advanced and the same open-
    /// questions/presence tail every other request gets, even though the
    /// rolling document it folds that tail onto is built by calling
    /// `document` directly rather than through `StepOutput::LlmRequest`.
    /// The byte budget the document is rendered against.
    ///
    /// **Rendering and context safety are different jobs.** This one
    /// clips reports so a single turn cannot swamp the page, and being
    /// approximate costs a clipped report. Keeping the context inside
    /// its window is `compaction_if_needed`'s job, and it uses the
    /// provider's own token count rather than anything derived from
    /// this.
    /// The byte budget `document::render` is called with.
    ///
    /// **It is the compaction fallback, not a render clip.** The
    /// parameter is threaded through `render` → `report_line` →
    /// `derive_report` → `render_handback`, where it meets
    /// `let _ = budget;` and is discarded — nothing has been clipped by
    /// it for some time. Its one live effect is
    /// `compaction::should_fire`, and that only runs for a session with
    /// no context window configured, or before the first reply has
    /// reported a token count.
    pub(crate) fn document_budget(&self) -> usize {
        crate::host::document_budget()
    }

    pub(crate) fn render_request(&mut self, tree: &Tree) -> StepOutput {
        // Everything logged so far is about to be shown. This is the one
        // place the mark moves.
        self.shown = self.spine.leaf_id.as_u64();
        StepOutput::LlmRequest(LlmRequest {
            tail: self.request_tail(tree),
        })
    }

    /// Whether a compaction has been asked for and not yet answered —
    /// **read off the log, not off `self`.**
    ///
    /// `compaction_requested` is the live flag: set beside the
    /// `Compaction` event, cleared when the handler's batch is applied
    /// at its handback. Both moments are on the log, so the same
    /// question has a logged answer — and it needs one, because the
    /// flag is false in every process that merely *reads* a log.
    /// `agent document` could not render the compaction directive at
    /// all, which is the one prompt in this system whose wording is
    /// argued over most and the only one nobody could look at.
    fn compaction_outstanding(&self, tree: &Tree) -> bool {
        tree.path_events(self.spine.leaf_id)
            .iter()
            .rev()
            .find_map(|e| match &e.payload {
                EventPayload::Compaction { .. } => Some(true),
                EventPayload::Handback { .. } => Some(false),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// The directive for the compaction currently outstanding, read
    /// back off the log rather than stashed on `self`: the `Condition`
    /// carrying the sizes was appended by `compaction_if_needed` one
    /// step ago, so the log is already the record and a second copy
    /// could only disagree with it.
    fn compaction_directive(&self, tree: &Tree) -> Option<String> {
        tree.path_events(self.spine.leaf_id)
            .iter()
            .rev()
            .find_map(|e| match &e.payload {
                EventPayload::Compaction {
                    measured,
                    limit,
                    unit,
                } => {
                    // The preamble's share, so the directive names the
                    // job rather than the window — see
                    // `compaction_message`. Rendering again costs a
                    // pass, and this runs once per compaction.
                    let doc = crate::document::render(tree, &self.spine, self.document_budget());
                    let fixed = matches!(unit, crate::types::Measure::Bytes).then(|| {
                        crate::compaction::rendered_size(&doc)
                            - doc
                                .conversation()
                                .iter()
                                .map(|m| m.content.len())
                                .sum::<usize>()
                    });
                    // What is big and what is stale, which the menu
                    // says nothing about — see `heaviest_rows`.
                    let heaviest = crate::report::heaviest_line(&crate::report::heaviest_rows(
                        &doc,
                        &self.row_ages(tree),
                        HEAVIEST_NAMED,
                    ));
                    Some(crate::report::compaction_message(
                        *measured, *limit, *unit, fixed, &heaviest,
                    ))
                }
                _ => None,
            })
    }

    /// How many replies ago each entry on this path was written —
    /// the "how long have I been carrying this" half of
    /// [`crate::report::heaviest_rows`].
    fn row_ages(&self, tree: &Tree) -> std::collections::HashMap<u64, usize> {
        let path = tree.path_events(self.spine.leaf_id);
        let replies: Vec<EventId> = path
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::Reply | EventPayload::Restart))
            .map(|e| e.id)
            .collect();
        path.iter()
            .map(|e| {
                let after = replies.iter().filter(|r| **r > e.id).count();
                (e.id.as_u64(), after)
            })
            .collect()
    }

    /// The trailing **ephemeral** line: per-request facts, emitted after
    /// the newest message and never logged. Next request it is simply
    /// re-emitted at the new end, so the prefix it followed stays intact —
    /// which is why a right-now fact may live here and nowhere else.
    ///
    /// Two facts so far, presence **last** — every request's last line
    /// says whether anyone is attached:
    ///
    /// - which questions are open, **each beside who asked it**
    ///   (18_TARGETING Step B2): a plain reply answers none of them, so
    ///   the model needs the id to reach for `answer` even when only one
    ///   post is open.
    /// - **presence**: whether a client is attached right now.
    pub(crate) fn request_tail(&self, tree: &Tree) -> Option<String> {
        // While a compaction is outstanding the directive *is* the tail,
        // and nothing else rides with it — the directive's own words are
        // "Write a compaction program. Nothing else", and the open-
        // questions and presence lines are about work that is explicitly
        // not being done in this program. It lives here rather than in
        // the document because it instructs rather than reports: see
        // `document::render_with_lookup`, which skips the row.
        if self.compaction_outstanding(tree)
            && let Some(directive) = self.compaction_directive(tree)
        {
            return Some(directive);
        }
        // **A readout, not an argument.** The card is where reasons
        // live and where a sentence is allowed to persuade; this is a
        // list of what is true right now, in the smallest number of
        // bytes that stays true — and it is paid uncached on every
        // request. Two lines written here today read as card prose and
        // both overclaimed; terse facts are harder to overclaim in.
        let mut lines: Vec<String> = Vec::new();
        // **A `peek` rides here and nowhere else.** This block is the
        // one part of the request re-emitted at the new end each time
        // rather than written into the history, so a value meant to be
        // in front of you for one reply and then gone costs no rewrite
        // of anything above it and no cache. It is also why the row it
        // names goes on rendering its value-less self up in the
        // conversation: the copy down here is not a second row, it is
        // this request's own end matter.
        lines.extend(self.peeked_block(tree));
        let open = self.open();
        if !open.is_empty() {
            let shown = open.len().min(OPEN_NOTE_MAX_IDS);
            let ids: Vec<String> = open[..shown]
                .iter()
                .map(|id| {
                    let who = asker_of(tree, *id)
                        .map(crate::report::author_label)
                        .unwrap_or_else(|| "an unknown author".to_owned());
                    format!("#{} ({who})", id.as_u64())
                })
                .collect();
            let more = match open.len() - shown {
                0 => String::new(),
                n => format!(" +{n} more"),
            };
            // The ids and who asked, because a plain reply answers none
            // of them and the verb needs the id (18_TARGETING B2).
            lines.push(format!(
                "- {} open: {}{more}. A reply does not answer them; \
                 answer(question, label, value) does.",
                open.len(),
                ids.join(", "),
            ));
        }
        if let Some((count, first, last)) = self.artifact_span(tree) {
            lines.push(format!(
                "- {count} rows, #{first}–#{last}. history.fetch(id) for any of them; \
                 a report lists only what is new."
            ));
        }
        // **How full it is, once that is a number worth knowing.** The
        // card says a row is cheap to drop while it is recent and costs
        // the whole conversation once it is old, and then leaves the
        // model with no way to tell where it stands — so it drops
        // nothing until the harness stops it and demands a compaction
        // program, by which time every cheap removal has become a dear
        // one. This is the missing half of that advice.
        //
        // A readout, not an argument: the two numbers and the verb, the
        // same shape as the open-questions line above.
        // Falling back to a fresh measurement when there has been no
        // live step to cache one — a reopened log, and in particular
        // `agent document`, which is how anyone checks what the model
        // was actually sent. A line that appears in the request and not
        // in the rendering of that request is a line nobody can audit,
        // and this file has been caught by that shape before. Through
        // `fullness`, so the rendering cannot answer in a different
        // unit from the trigger.
        let fullness = self.last_fullness.or_else(|| {
            let doc = crate::document::render(tree, &self.spine, self.document_budget());
            self.fullness(
                tree,
                crate::host::context_tokens(),
                crate::compaction::rendered_size(&doc),
                self.document_budget(),
            )
        });
        if let Some((measured, limit, unit)) = fullness
            && limit > 0
            && !self.compaction_outstanding(tree)
        {
            let pct = measured * 100 / limit;
            if pct >= SOFT_FULL_PERCENT {
                // **Prospective, because that is the only verb it has.**
                // It named `history.remove(id)` while the card still
                // declared it. The card does not: undoing costs a
                // rewrite of the history and the model cannot know
                // which rows are dear to rewrite, while not keeping a
                // thing in the first place costs nothing at all.
                lines.push(format!(
                    "- {pct}% full: {measured} {}s of {limit}. Everything you keep is paid for \
                     again every turn; peek what you only need once.",
                    unit.noun()
                ));
            }
        }
        if self.finish_ignored {
            lines.push(SILENT_FINISH.to_owned());
        }
        if self.reply_shape_tail && self.answering_a_post(tree) {
            lines.push(REPLY_SHAPE_TAIL.to_owned());
        }
        if let Some(line) = &self.no_rehearsal_tail
            && !self.no_rehearsal_last
        {
            lines.push(line.clone());
        }
        lines.push(if self.attached { PRESENT } else { ABSENT }.to_owned());
        if let Some(n) = self.replies_since_spoken_to(tree) {
            lines.push(format!(
                "- {n} program{} run since you were last spoken to.",
                if n == 1 { "" } else { "s" }
            ));
            lines.push(
                if self.run_program {
                    WORK_UNDER_WAY_CALL
                } else {
                    WORK_UNDER_WAY
                }
                .to_owned(),
            );
        }
        lines.push(
            if self.run_program {
                REPLY_IS_A_CALL
            } else {
                REPLY_IS_MARKDOWN
            }
            .to_owned(),
        );
        // Below the rule whose violation is silent, which is the whole
        // point of the arm: the two lines are competing for one slot.
        if let Some(line) = &self.no_rehearsal_tail
            && self.no_rehearsal_last
        {
            lines.push(line.clone());
        }
        Some(lines.join("\n"))
    }

    /// The `peek`ed rows, rendered for this request only — the block
    /// [`Runner::request_tail`] opens with.
    ///
    /// **Live means "no reply has been logged since".** That is one
    /// request exactly, with no timer in it, so a log reopened a week
    /// later renders what the model was actually looking at. A row
    /// whose newest `Render` is a `keep` is not here: it writes its
    /// value out in place, up where its id is.
    fn peeked_block(&self, tree: &Tree) -> Vec<String> {
        let path = tree.path_events(self.spine.leaf_id);
        let last_reply = path
            .iter()
            .rev()
            .find(|e| matches!(e.payload, EventPayload::Reply | EventPayload::Restart))
            .map(|e| e.id);
        let newest = last_renders(&path);
        let compacted = tree.compacted_lookup(self.spine.leaf_id);
        let mut rows: Vec<String> = Vec::new();
        for event in &path {
            let EventPayload::Render { of, mode, value } = &event.payload else {
                continue;
            };
            // Only the newest word on this row, and only while it is a
            // peek this reply has not already spent.
            if *mode != crate::types::RenderMode::Peeked
                || newest
                    .get(of)
                    .is_none_or(|(m, _)| *m != crate::types::RenderMode::Peeked)
                || last_reply.is_some_and(|r| r > event.id)
            {
                continue;
            }
            let body = shown_body("peek", *of, value);
            // The row as the menu would draw it, so the value arrives
            // under the line saying where it came from. A row that
            // renders nothing any more — removed, or never a row at all
            // — still shows its value under a bare id: `peek` is a
            // request to see something, and refusing it silently over
            // an unrelated `remove` would be the narrow channel again.
            let target: Vec<&Event> = path.iter().copied().filter(|e| e.id == *of).collect();
            let mut drawn = menu_rows(&target, 0, &compacted, &path);
            rows.push(match drawn.pop() {
                Some(mut a) => {
                    a.shown = Some(body);
                    crate::report::render_row(&a)
                }
                None => format!("- `[{}]` {body}", of.as_u64()),
            });
        }
        if rows.is_empty() {
            return Vec::new();
        }
        let mut out = vec!["### peeked — here for this reply only\n".to_owned()];
        out.extend(rows);
        out.push(String::new());
        out
    }

    /// How many replies have run a program since anyone last spoke to
    /// this branch — `None` when none have, which is also when
    /// [`WORK_UNDER_WAY`] does not apply.
    ///
    /// **A fact the model cannot derive.** It would have to re-read its
    /// own history to count, and the number is the one that says
    /// whether this is going anywhere: `sweep-40` spent nine programs
    /// and fifty minutes on one task without converging.
    fn replies_since_spoken_to(&self, tree: &Tree) -> Option<usize> {
        let segment = self.agent_segment(tree);
        let last_post = segment
            .iter()
            .rev()
            .find(|e| matches!(e.payload, EventPayload::Post { .. }))
            .map(|e| e.id.as_u64())
            .unwrap_or(0);
        // **Programs, not blocks.** This counted `Part::Cell` events,
        // and a reply's cells are *one* program that pauses between
        // them — the card's first paragraph says so. So the reply this
        // line exists to praise, one completion doing grep → edit →
        // verify in three cells, reported itself as "3 programs run",
        // and the number meant to catch a branch going nowhere
        // (`sweep-40`: nine programs, fifty minutes, no convergence)
        // read highest for the shape that is going somewhere.
        //
        // Seen on a `deepseek-v4-flash` smoke run, 2026-09-21: one
        // `Reply`, one `Handback`, three cells, tail said three.
        let n = segment
            .iter()
            .filter(|e| e.id.as_u64() > last_post)
            .filter_map(|e| match &e.payload {
                EventPayload::Part {
                    reply,
                    part: crate::types::Part::Cell(_),
                } => Some(*reply),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        (n > 0).then_some(n)
    }

    /// **Did this reply put anything in front of anybody?** Prose and
    /// `tell` both count — the card says so ("your prose reaches the
    /// person as its own row as well") — and a `Send` is what either
    /// leaves on the log, so one fold answers for both. So does an
    /// `answer`: a worker whose whole job was the question has said
    /// what it had to say, to the branch that asked.
    ///
    /// `finish()` is honoured only when this is true. A branch that
    /// rests having said nothing to anybody is the failure the old
    /// `finish(text)` argument existed to prevent.
    fn said_something(&self, tree: &Tree) -> bool {
        let reply = self.reply_id.as_u64();
        self.agent_segment(tree)
            .iter()
            .filter(|e| e.id.as_u64() > reply)
            .any(|e| {
                matches!(
                    &e.payload,
                    EventPayload::Call(Call::Send { .. }) | EventPayload::Answer { .. }
                )
            })
    }

    /// **Somebody just asked, and nothing of the branch's own is
    /// outstanding** — the one request shape where "you may simply
    /// answer" is apt. A continuation of the branch's own work is the
    /// opposite case, and the card is right about it: you are not
    /// trying to finish the task in one reply.
    fn answering_a_post(&self, tree: &Tree) -> bool {
        let replies = self.replies(tree);
        // A post since the last reply — `replies`'s own `answering`
        // fold, read for the reply that is about to be written rather
        // than for one already on the log.
        let last_reply = replies.last().map(|r| r.id.as_u64()).unwrap_or(0);
        let posted_since = self
            .agent_segment(tree)
            .iter()
            .any(|e| e.id.as_u64() > last_reply && matches!(e.payload, EventPayload::Post { .. }));
        // An **agent** waiting on this branch is the exception: its
        // program is suspended until a program here calls
        // `answer(question, …)`, so there is something to run and the
        // line would be wrong. A person's open question is the case
        // this is for — answering it needs nothing run.
        let agent_waiting = self
            .open()
            .iter()
            .any(|id| matches!(asker_of(tree, *id), Some(crate::types::Author::Agent(_))));
        // **Nothing parked.** A suspended run is a program waiting to
        // be resumed, so there is something to carry on with even
        // though a post arrived — `request_tail` runs at render time,
        // by which point the phase has already left `Idle`, so this
        // asks what is parked rather than what the phase is.
        let parked = !self.parked.is_empty();
        posted_since && !agent_waiting && !parked
    }

    /// How many **menu rows** this branch's path holds, and the id range
    /// they span — the pointer that lets each report list only what is
    /// *new* without putting an older id out of reach.
    fn artifact_span(&self, tree: &Tree) -> Option<(usize, u64, u64)> {
        let ids: Vec<u64> = self
            .agent_segment(tree)
            .iter()
            .filter(|e| {
                matches!(
                    e.payload,
                    EventPayload::Call(_) | EventPayload::Handback { .. }
                )
            })
            .map(|e| e.id.as_u64())
            .collect();
        Some((ids.len(), *ids.first()?, *ids.last()?))
    }

    /// The rendered `Document` for this branch's current path — a thin
    /// passthrough to `document::render`, for the host to call once it
    /// sees a `StepOutput::LlmRequest`.
    ///
    /// **This file deliberately does not build the `Document` itself**
    /// (a deviation from A5's (`host/`) first assumption, made after
    /// reading `document.rs` directly — see this step's report):
    /// `document::render(tree, spine, budget)` takes `budget` as a
    /// caller-supplied parameter *by design* (its own doc comment: "it
    /// has to arrive as a parameter from whichever caller already tracks
    /// it... rather than be smuggled onto a type that has no field for
    /// it"), and `Runner` has no such field anymore — the whole point of
    /// deleting `DEFAULT_ANSWER_BUDGET` was that this budget is a
    /// per-agent, host-tracked configuration value, not branch state.
    /// So the host calls this with whatever it tracks, then applies the
    /// tail itself: `runner.document(tree, budget).with_tail(&tail)`.
    pub fn document(&self, tree: &Tree, budget: usize) -> crate::document::Document {
        crate::document::render(tree, &self.spine, budget)
    }

    /// One request's ephemeral half, for tests that inspect the tail.
    #[cfg(test)]
    pub fn render_request_for_test(&mut self, tree: &Tree) -> LlmRequest {
        match self.render_request(tree) {
            StepOutput::LlmRequest(r) => r,
            _ => unreachable!("render_request returns a request"),
        }
    }

    /// The full rendered `Document` (card + history + tail), for tests
    /// that assert on what an LLM would actually see. `budget` is a
    /// fixed test constant (`TEST_BUDGET`) — no session tracks one in a
    /// test harness.
    #[cfg(test)]
    pub fn render_messages_for_test(&mut self, tree: &Tree) -> crate::document::Document {
        let tail = self.render_request_for_test(tree).tail;
        let doc = self.document(tree, TEST_BUDGET);
        match tail {
            Some(t) => doc.with_tail(&t),
            None => doc,
        }
    }

    /// Events of this agent's spine segment (its `Agent` down to
    /// the leaf), in log order.
    pub(crate) fn agent_segment<'t>(&self, tree: &'t Tree) -> Vec<&'t Event> {
        let mut events = Vec::new();
        let mut current = self.spine.leaf_id;
        while let Some(event) = tree.events.get(&current) {
            let is_agent_root = matches!(event.payload, EventPayload::Agent { .. });
            events.push(event);
            if is_agent_root {
                break;
            }
            match event.parent_id {
                Some(parent) => current = parent,
                None => break,
            }
        }
        events.reverse();
        events
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Assemble an agent's system prompt: the dialect card, then the charter.
/// The result is snapshotted on the `Agent` event and never re-derived —
/// `input` is *not* part of it, because machine-bound data belongs on the
/// post that carries it, previewed rather than dumped into context.
fn assemble_system(card: &str, charter: &str) -> String {
    let mut system = String::new();
    if !card.is_empty() {
        system.push_str(card);
        system.push_str("\n\n");
    }
    system.push_str(charter);
    system
}

/// Coerce a JSON scalar to text the way `tell`/`ask` want their body:
/// found live (the deleted `verbs.rs`, 2026-09-10) as `say(42)` — a bare
/// number where a string was clearly meant, which otherwise silently
/// misroutes to "no such tool" rather than running the call as intended.
/// Objects/arrays have no single obviously-right text form, so they are
/// rejected rather than guessed at.
fn coerce_text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => Some("null".to_owned()),
        _ => None,
    }
}

/// Read `choose`'s third argument: the offered options.
///
/// Strict, and deliberately so — a malformed `choose` is a program bug
/// the model should see named, not a round trip to a person who is then
/// asked to pick from one option or from `[object Object]`. Two is the
/// floor because a one-option choice is a `tell`, and duplicates are
/// refused because the whole promise of this verb is that the value
/// coming back identifies *which* option was picked.
fn read_options(v: &serde_json::Value) -> Result<Vec<String>, String> {
    let serde_json::Value::Array(items) = v else {
        return Err("choose(who, question, options) needs an array of option \
                    strings as its third argument"
            .to_owned());
    };
    let mut options = Vec::with_capacity(items.len());
    for item in items {
        match item {
            serde_json::Value::String(s) if !s.trim().is_empty() => options.push(s.clone()),
            serde_json::Value::String(_) => {
                return Err("choose: an option is empty; every option needs text a \
                            person can pick by"
                    .to_owned());
            }
            other => {
                return Err(format!(
                    "choose: options must be strings; got {}. A person picks by reading \
                     them, so each one has to say what it means.",
                    crate::report::input_preview(other)
                ));
            }
        }
    }
    if options.len() < 2 {
        return Err(format!(
            "choose: {} option{} is not a choice — offer at least two, or say it with \
             tell() and ask() if there is nothing to pick between",
            options.len(),
            if options.len() == 1 { "" } else { "s" }
        ));
    }
    for (i, a) in options.iter().enumerate() {
        if let Some(j) = options[..i].iter().position(|b| b.trim() == a.trim()) {
            return Err(format!(
                "choose: options {} and {} are the same ({a:?}); the answer could not \
                 say which was picked",
                j + 1,
                i + 1
            ));
        }
    }
    Ok(options)
}

/// Place a reply onto one of the offered options, or `None` if it does
/// not land on one.
///
/// Strict on purpose. A looser matcher (unique prefixes, substrings)
/// would buy nothing here and could guess wrong silently, because
/// *failing* to place a reply is not an error in this design: it hands
/// the person's actual words to a program that can read them. Being
/// strict costs one condition; being clever costs a wrong answer nobody
/// sees. Exact first, then case/whitespace, then the 1-based ordinal a
/// person naturally types when reading a numbered list — the ordinal
/// last so a literal option `"2"` still wins its own name.
pub(crate) fn pick_option(reply: &str, options: &[String]) -> Option<String> {
    if let Some(hit) = options.iter().find(|o| *o == reply) {
        return Some(hit.clone());
    }
    let folded = reply.trim().to_lowercase();
    if let Some(hit) = options.iter().find(|o| o.trim().to_lowercase() == folded) {
        return Some(hit.clone());
    }
    folded
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1 && *n <= options.len())
        .map(|n| options[n - 1].clone())
}

/// What a `choose` settles with when the reply did not land on an
/// option: a rejection carrying the words themselves.
///
/// `Await` escalates a rejected promise as a resumable error, so this
/// string is what the next program reads in its condition report, and
/// `resume(value)` stands in for the call. It therefore has to carry
/// everything that judgement needs — what was offered and what was
/// actually said — because the suspended program's own source is the
/// only other thing in view.
pub(crate) fn off_menu(who: &str, reply: &str, options: &[String]) -> String {
    let offered = options
        .iter()
        .map(|o| format!("{o:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("choose: {who} answered outside the offered set [{offered}], saying: {reply}")
}

/// `note_history(value)` takes any JSON value, but `EventPayload::Note`
/// stores rendered text: a JSON string is used verbatim, anything else is
/// serialized. The card's own guidance is to append a short projection
/// (a summary), not a raw result, so the common case is already a string.
pub(crate) fn note_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// An appended value as the **model** reads it back: its JSON, always.
///
/// **What you see is what `fetch` hands you.** A note is the one row
/// whose content is shown whole rather than indexed, so its rendering
/// is the model's only evidence of what `history.fetch(id)` will
/// return — and [`note_text`] renders a string bare, which makes
/// `append("{\"a\": 1}")` and `append({ a: 1 })` identical on the page
/// and different in the hand. Quoting costs a couple of characters and
/// removes the guess: `noted: "a conclusion"` is a string,
/// `noted: {"a":1}` is an object.
///
/// `note_text` stays as it is — `Outcome::appended` is contracted as
/// "the note's own literal text" and the benchmark checkers read it.
pub(crate) fn note_display(value: &serde_json::Value) -> String {
    value.to_string()
}

/// An appended row: what it shows, and what it says about the rest.
///
/// **Bounded here and nowhere else.** This was the one visible thing in
/// the system with no cap on it — and by bytes it is how models read,
/// whatever the card says: 72% of everything appended across 352 kept
/// runs was a verbatim copy of a result, and the biggest single row was
/// 38,342 bytes, re-rendered on every completion until something
/// compacted it.
///
/// **The JSON head survives the clip**, because the row is the model's
/// only evidence of what `history.fetch` will hand back — a leading `"`
/// means a string and a leading `{` an object, and
/// `an_appended_row_renders_as_the_json_it_will_hand_back` exists
/// because rendering a string bare made `append("{\"a\":1}")` and
/// `append({a:1})` identical on the page and different in the hand.
/// Clipping the tail keeps that distinction; clipping the head would
/// destroy it.
///
/// Clipped *before* escaping, so the byte counts it quotes are the
/// value's own — the thing `fetch` returns — rather than the rendering's.
///
/// **And it names `replace`, not `append`, for the next window.**
/// Appending each page would put every window in the document at once,
/// which is the cost this bound exists to avoid; replacing *moves* the
/// one row's view and leaves the context flat. `fetch` still returns
/// the original whole afterwards, so the bytes to cut the next window
/// from are always in reach — see
/// `paging_a_row_moves_its_window_and_leaves_the_value_whole`.
///
/// There used to be a third verb for this, `history.slice(id, from,
/// to)`, whose whole claim was that it wrote two numbers to the log
/// instead of the window's text. That bought nothing the reader can
/// see: the document is the rationed thing, and `replace` already
/// keeps it flat. What it cost was a verb — and a second meaning for
/// what a `Compacted` event is.
fn note_row(id: u64, value: &serde_json::Value) -> String {
    let full = note_display(value);
    let cap = crate::report::NOTE_ROW_MAX_BYTES;
    let mut b = cap.min(full.len());
    while !full.is_char_boundary(b) {
        b -= 1;
    }
    let shown = crate::document::escape_untrusted(&full[..b]);
    if b == full.len() {
        return format!("noted: {shown}");
    }
    format!(
        "noted: {shown}\n  … {b} of {} bytes — `history.fetch({id})` has all of it, and \
         `history.replace({id}, …)` moves this window onto the next stretch",
        full.len(),
    )
}

fn json_arg(vm: &mut VM, json: &serde_json::Value) -> Value {
    vm.json_to_stack_value(json, 0).unwrap_or(Value::Null)
}

/// A program value as JSON, with a **visible** stand-in when it has no
/// JSON form.
///
/// The fallback used to be `format!("{v:?}")`, the Rust debug
/// rendering, which is how a live `skipped-tests` run on 2026-09-17
/// wrote a file whose entire first line was the word `Undefined`:
/// something in the program evaluated to `undefined`, `replace_file`
/// was handed the *string* `"Undefined"`, and it wrote it. Python then
/// said `NameError: name 'Undefined' is not defined`, several steps
/// away from the mistake.
///
/// A value with no JSON form is a defect in the program either way; the
/// only question is whether it arrives as something a reader can
/// recognise. `undefined` specifically is common enough — a missing
/// property, a function with no return — to name outright. Tool
/// arguments do better still and refuse the call outright
/// ([`args_as_json`]); this stays for the paths where there is nobody
/// left to refuse to.
fn value_json(vm: &VM, v: &Value) -> serde_json::Value {
    vm.stack_value_to_json(v, 0).unwrap_or_else(|_| {
        serde_json::Value::String(match v {
            Value::Undefined => "<undefined — this value has no JSON form>".into(),
            other => format!("<{other:?} — this value has no JSON form>"),
        })
    })
}

/// Every argument of a call, as JSON, or the index of the first one
/// that has no JSON form.
///
/// **A call whose arguments cannot be represented does not happen.**
/// Passing `undefined` where a tool expects a string is a mistake the
/// program can fix, but only if it is told; writing the word
/// `Undefined` into a source file is a mistake it cannot see at all,
/// and the failure surfaces later, somewhere else, as somebody else's
/// syntax error.
fn args_as_json(vm: &VM, call: &InvokeCall) -> Result<Vec<serde_json::Value>, String> {
    call.args
        .iter()
        .enumerate()
        .map(|(i, v)| {
            vm.stack_value_to_json(v, 0).map_err(|_| {
                let what = match v {
                    Value::Undefined => "is `undefined`".to_string(),
                    other => format!("has no JSON form ({other:?})"),
                };
                format!(
                    "{}() argument {} {} — nothing was called. A value that cannot \
                     cross into a tool is a value the program does not have.",
                    call.name,
                    i + 1,
                    what
                )
            })
        })
        .collect()
}
/// The `Result` settling `call`, if one landed on this path.
pub(crate) fn settlement_of<'e>(segment: &[&'e Event], call: EventId) -> Option<&'e Outcome> {
    segment.iter().find_map(|e| match &e.payload {
        EventPayload::Result { call: c, outcome } if *c == call => Some(outcome),
        _ => None,
    })
}

/// The artifact menu as a **projection over the events on this path**
/// (17_BRANCHES): every call that landed here, plus every call still
/// pending, plus prior program results. Nothing maintains a store — the
/// log is the cache and the event id is the key.
///
/// Rows are named by the **call** id, which is what a program reuses:
/// `fetch_history(id)` resolves a call id through to its `Result`.
///
/// **`settlements` is a wider slice than `segment`, and has to be.**
/// Which rows are *in* this menu is decided by the segment — the
/// events between one outcome and the next. What each row's state *is*
/// cannot be: a call still in flight when the program parks settles
/// afterwards, so its `Result` sits past the segment's end. Reading
/// state from the segment reported it as `PendingInvoke` — "issued;
/// may have happened" — when the log said `Failed`, which is the
/// opposite claim and the one the card draws a line between. Seen live
/// on 2026-09-20: a `bash` killed at its 30s ceiling while the program
/// was suspended, reported to the model as possibly having happened.
///
/// The `Compacted` lookup one argument along is the same shape for the
/// same reason, and was already right.
pub(crate) fn menu_rows(
    segment: &[&Event],
    since: u64,
    compacted: &std::collections::HashMap<EventId, crate::tree::CompactedView>,
    settlements: &[&Event],
) -> Vec<Artifact> {
    // **What `history.keep` asked to be shown, on the row that shows
    // it.** A `Render` is not a row; it is one bit about a row that
    // already exists — does it write its value out — so it renders
    // where that row already is, under the id the model already has.
    //
    // It was a row of its own at first, and that double-charged
    // anything whose row already showed its own value: a note and a
    // `keep` of that note put the same bytes on the page twice, under
    // two ids.
    let inline = last_renders(settlements);
    segment
        .iter()
        .filter(|e| e.id.as_u64() > since)
        // **A removed row is removed here too.** `document.rs` drops a
        // compacted row from the history log via its `CompactedView`
        // shadow, but this list had no compaction awareness at all — so
        // a row the model deleted with `history.remove` vanished from
        // the log and went on being advertised beside it, which is the
        // one place the card promises removal means removal.
        .filter(|e| {
            !matches!(
                compacted.get(&e.id),
                Some(crate::tree::CompactedView { text: None })
            )
        })
        .filter_map(|event| {
            let id = event.id.as_u64();
            // **And a replaced row renders as its replacement.** It did
            // not before, because the rows the model could shorten
            // (notes, tells, asks) lived in `document.rs`, which honours
            // a shadow, while this list held only calls, which nobody
            // had tried to shorten. Now that they are one list, a
            // `history.replace` that did not shrink the thing it named
            // would be the same broken promise a `history.remove` was.
            // Something else stands here.
            if let Some(crate::tree::CompactedView { text: Some(text) }) =
                compacted.get(&event.id)
            {
                return Some(Artifact {
                    id,
                    label: String::new(),
                    state: ArtifactState::Whole(format!("… {text}")),
                    shown: None,
                });
            }
            match &event.payload {
                // **A `tell` gets no row.** Its text is already in the
                // document, verbatim, in the `tell(...)` call of the
                // program that made it — a menu row could only repeat
                // it, and the preview it repeated taught the opposite
                // of the card: the card says `tell` reaches "the
                // person, and only the person", while the row showed
                // the first fifty characters of exactly the content the
                // program had wanted to look at, sitting in the
                // conversation. That reads as "it is here, merely
                // truncated", and the next program reformats the same
                // `tell` rather than concluding the channel was wrong.
                // Measured 2026-09-17: 45% of programs read something,
                // wrote nothing, carried nothing forward and did not
                // finish; 140 of those 155 ended by telling the user.
                // One run repeated a read-and-tell program five times,
                // commenting "show them for context", then "show full
                // contents for inspection".
                //
                // **A prose segment leaves no row.** It is not something
                // the branch *did* — it is part of what the branch
                // *said*, and the assistant turn above already carries
                // it verbatim. A row would print the same words a
                // second time, in the other voice.
                EventPayload::Call(Call::Send { prose: true, .. }) => None,
                // **A `tell`, an `ask` and an `answer` are rows whose
                // content is the row**, and they sit in this list
                // rather than beside it. They used to render as loose
                // lines in `document.rs` while the calls rendered in a
                // menu here, so one run's doings were split across two
                // lists with the outcome wedged between them — and the
                // menu row a `tell` did get showed the first fifty
                // characters of exactly the content the program had
                // wanted to look at, which reads as "it is here, merely
                // truncated". Measured 2026-09-17: 45% of programs read
                // something, wrote nothing, carried nothing forward and
                // did not finish; 140 of those 155 ended by telling the
                // user. One run repeated a read-and-tell program five
                // times, commenting "show them for context", then "show
                // full contents for inspection".
                //
                // So: one row, whole, never a preview. Whole is also
                // the only honest rendering — 87% of the 503 tells
                // measured that day were computed, so the source shows
                // `tell("--- " + f.content)` and not one byte of what
                // was actually said.
                EventPayload::Call(Call::Send {
                    to,
                    text,
                    expects_reply,
                    options,
                    ..
                }) => Some(Artifact {
                    id,
                    label: String::new(),
                    state: ArtifactState::Whole(format!(
                        "you {} {}: {}{}",
                        if *expects_reply { "asked" } else { "told" },
                        address_label(to),
                        crate::document::escape_untrusted(text),
                        // A `choose`'s options belong on its own row.
                        // When the answer arrives it is one of these
                        // strings and nothing else, and a row reading
                        // `you asked user: 30s` two lines above `user
                        // answered [12]: 5m` is unreadable without them
                        // — the reader cannot tell a picked option from
                        // prose.
                        if options.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " — pick one of: {}",
                                options
                                    .iter()
                                    .map(|o| crate::document::escape_untrusted(o))
                                    .collect::<Vec<_>>()
                                    .join(" / ")
                            )
                        }
                    )),
                    shown: None,
                }),
                // A row's label comes from the call *variant*; its value
                // (or its absence) from the `Result`.
                EventPayload::Call(call) => Some(Artifact {
                    id,
                    label: call_label(call),
                    state: match settlement_of(settlements, event.id) {
                        Some(Outcome::Delivered(v)) => ArtifactState::Delivered(v.clone()),
                        Some(Outcome::Failed(msg)) => ArtifactState::Failed(msg.clone()),
                        // Only one pending kind can be re-attached: an
                        // `ask`'s answer is still coming, while a
                        // `Spawn`/`Fork`/`Invoke`'s worker died with the
                        // process.
                        None => match call {
                            Call::Send { .. } => ArtifactState::PendingSend,
                            Call::Spawn { .. } | Call::Fork { .. } | Call::Invoke { .. } => {
                                ArtifactState::PendingInvoke
                            }
                        },
                    },
                    shown: None,
                }),
                // **An `answer` is a row again.** It stopped being an
                // outcome in 28 — a reply can answer and keep going, and
                // one exemplar does — so the ack that used to stand in
                // for it is gone and the act itself is what the log
                // shows.
                EventPayload::Answer { question, value } => Some(Artifact {
                    id,
                    label: String::new(),
                    state: ArtifactState::Whole(format!(
                        "you answered [{}]: {}",
                        question.as_u64(),
                        crate::document::escape_untrusted(&value.to_string())
                    )),
                    shown: None,
                }),
                // A `history.note` — the one channel that crosses
                // between replies by design, so it belongs in the list
                // of what this run put on the record, beside the calls
                // that produced the values.
                //
                // **`noted:`, because that is the verb it wrote.** One
                // thing, one name, wherever it is spoken, and the
                // model-facing direction is the one that matters: the
                // row and the call that wrote it have said the same
                // word since the verb stopped being `append`.
                //
                // Why it stopped: the distinction the card now has to
                // teach is `note` against `keep` — bytes you wrote
                // against a result you were given — and `append`
                // against `keep` says nothing about which is which.
                EventPayload::Note { value, .. } => Some(Artifact {
                    id,
                    label: String::new(),
                    state: ArtifactState::Whole(note_row(id, value)),
                    shown: None,
                }),
                _ => None,
            }
        })
        .map(|mut a| {
            if let Some((crate::types::RenderMode::Kept, value)) = inline.get(&EventId::new(a.id)) {
                a.shown = Some(shown_body("keep", EventId::new(a.id), value));
            }
            a
        })
        .collect()
}

/// How big a whole-result body has to be before the row suggests
/// narrowing it. Under this the JSON braces are noise, not a cost.
const SHOWN_HINT_MIN_BYTES: usize = 1024;

/// The body a `keep`/`peek` writes under its row: the value as the
/// model reads it, bounded — and, when it is a whole object a
/// projection would narrow, one line saying so.
///
/// **The advice belongs where the wall of text is.** A bare
/// `keep(f)` on a `read_file` result stores `{content, version, id}`,
/// which renders as a single JSON line with every newline escaped:
/// barely legible, and 31 KB of it on a live run of 2026-09-23. The
/// card asks for a projection in the abstract; this is the moment that
/// request means something, and it can name the field.
fn shown_body(verb: &str, id: EventId, value: &serde_json::Value) -> String {
    let body = crate::report::clip(&note_text(value), crate::report::NOTE_ROW_MAX_BYTES);
    if !value.is_object() || body.len() <= SHOWN_HINT_MIN_BYTES {
        return body;
    }
    // The longest string field is the one worth reading on its own —
    // `content` on a read, `stdout` on a command. A result with no
    // string in it is one a projection would not help.
    let Some(field) = value.as_object().and_then(|map| {
        map.iter()
            .filter(|(_, v)| v.is_string())
            .max_by_key(|(_, v)| v.as_str().map_or(0, str::len))
            .map(|(k, _)| k.clone())
    }) else {
        return body;
    };
    format!(
        "{body}\n… that is the whole result, as JSON. \
         `history.{verb}({}, (v) => v.{field})` shows just that field.",
        id.as_u64()
    )
}

/// The last `keep`/`peek` naming each row, by the row it names.
///
/// One result, one answer: a row writes its value out in place, or
/// shows it once in the tail, or shows nothing — whichever the newest
/// `Render` on the path says. The same resolution `compacted_lookup`
/// gives `remove` against `replace`, for the same reason: they answer
/// one question about one row.
pub(crate) fn last_renders(
    path: &[&Event],
) -> std::collections::HashMap<EventId, (crate::types::RenderMode, serde_json::Value)> {
    let mut out = std::collections::HashMap::new();
    for event in path {
        if let EventPayload::Render { of, mode, value } = &event.payload {
            out.insert(*of, (*mode, value.clone()));
        }
    }
    out
}

/// A menu row's label, read from the call variant — never by re-parsing a
/// tool name. `ask` versus `tell` is `expects_reply`, the one place the
/// difference is visible.
///
/// Arguments go through [`arg_preview`], which is deliberately stingy.
/// A label is how a reader decides whether to spend a fetch, and the
/// whole argument is never the thing that decides it: an id and
/// `bash("cargo check 2>&1")` is a decision, `bash(<4 KB of script>)`
/// is the script itself arriving unasked. The call's arguments stay
/// fetchable whole under its id.
fn call_label(call: &Call) -> String {
    match call {
        Call::Send {
            to,
            text,
            expects_reply,
            ..
        } => {
            format!(
                "{}({}, {})",
                if *expects_reply { "ask" } else { "tell" },
                address_label(to),
                arg_preview(&serde_json::Value::String(text.clone()))
            )
        }
        // **The charter, when there is no name — which is almost
        // always.** `spawn(charter)` takes one argument and it is not a
        // name, so `name` is `None` on essentially every spawn in the
        // corpus and this row read `spawn(<unnamed>)`: the one call
        // whose identity the model must carry forward, rendered as the
        // only row on the menu that says nothing about itself.
        //
        // Seen live on 2026-09-20: a parent spawned a helper, was shown
        // `[9] spawn(<unnamed>) → ok, {agent}`, and spawned a second
        // helper on its very next reply.
        Call::Spawn { name, charter, .. } => format!(
            "spawn({})",
            match name {
                Some(n) => n.clone(),
                None => arg_preview(&serde_json::Value::String(charter.clone())),
            }
        ),
        Call::Fork { name, .. } => {
            format!("fork({})", name.as_deref().unwrap_or(""))
        }
        Call::Invoke { name, args, .. } => format!("{}({})", name, arg_preview(args)),
    }
}

pub(crate) fn address_label(to: &Address) -> String {
    match to {
        Address::User => "user".into(),
        Address::Branch(id) => format!("#{}", id.as_u64()),
    }
}

/// A settled call's value for the program: a delivered value resolves,
/// a failure rejects with its reason.
fn outcome_json(outcome: &Outcome) -> Result<serde_json::Value, String> {
    match outcome {
        Outcome::Delivered(v) => Ok(v.clone()),
        Outcome::Failed(msg) => Err(msg.clone()),
    }
}

impl Runner {
    /// **Feed streamed completion text to a notebook turn** (D11, 25.5).
    ///
    /// The reply is split as it arrives and each piece acted on the moment it
    /// is complete: prose logged as a `Send`, a cell compiled and run the
    /// moment its closing fence lands. A closing fence is decidable at the
    /// line level with no parsing, which is the property phase 24 could never
    /// get from a JS expression.
    ///
    /// The run is created on the first chunk, so the branch is `Running` with
    /// a generation still in flight — a state nothing else produces, and one
    /// nothing objects to: `needs_prompt` is false for both reasons at once.
    /// Until the first fence closes the VM simply parks at the prelude
    /// fragment's `Pause`.
    ///
    /// Cells still run strictly in sequence (D11): a fence can close while
    /// the previous cell is suspended on an await, and the cell then waits in
    /// the queue rather than jumping ahead of it.
    pub fn notebook_stream(
        &mut self,
        tree: &mut Tree,
        epoch: u64,
        text: &str,
    ) -> io::Result<Vec<StepOutput>> {
        if self.streaming_epoch != Some(epoch) {
            // A different generation: whatever was being assembled is
            // over, however it ended.
            // A streamed reply is always the model's.
            self.open_notebook_reply(tree, Some(epoch), Author::Agent(self.agent_id()))?;
        }
        self.notebook_stream_chunk(tree, text)
    }

    /// Append text to the reply already open, and run whatever that
    /// completes. The one place a notebook reply grows, whether the text
    /// arrived as a chunk or whole.
    fn notebook_stream_chunk(
        &mut self,
        tree: &mut Tree,
        text: &str,
    ) -> io::Result<Vec<StepOutput>> {
        self.notebook_feed(tree, text)?;
        self.drive_notebook(tree)
    }

    /// The run this branch holds: the one executing, else the most
    /// recently parked one. The single answer to what used to be
    /// written `Phase::Running(run) | Phase::Suspended(run, _)` in nine
    /// places, and it targets the same frame that arm did.
    fn run_mut(&mut self) -> Option<&mut Run> {
        if let Phase::Running(run) = &mut self.phase {
            return Some(run);
        }
        self.parked.last_mut().map(|p| &mut p.run)
    }

    /// **A frame is parked and nothing is executing** — what
    /// `Phase::Suspended` used to say on its own.
    ///
    /// The distinction the split makes explicit: a handler running over
    /// a parked frame is *not* this, which `!self.parked.is_empty()`
    /// alone would have said it was.
    fn is_suspended(&self) -> bool {
        !self.parked.is_empty() && !matches!(self.phase, Phase::Running(_))
    }

    /// [`Runner::run_mut`] without the borrow.
    fn run_ref(&self) -> Option<&Run> {
        if let Phase::Running(run) = &self.phase {
            return Some(run);
        }
        self.parked.last().map(|p| &p.run)
    }

    /// Take text into the reply and record whatever parts it completed —
    /// without running any of them.
    ///
    /// Split out from [`notebook_stream_chunk`](Self::notebook_stream_chunk)
    /// so a reply that arrives **whole** can record itself before it
    /// runs: its `ReplyEnd` is already known, and logging it after the
    /// first cell's effects put the end of the reply after the end of
    /// the run.
    fn notebook_feed(&mut self, tree: &mut Tree, text: &str) -> io::Result<()> {
        // **Suspended counts.** A raise or an arriving post parks the
        // run while the reply keeps being written (28's cancellation
        // rule: neither falsifies what follows), and text dropped here
        // is text the log can never concatenate back — the cells after
        // the raise would simply not exist when it was answered.
        let Some(run) = self.run_mut() else {
            return Ok(());
        };
        let Some(notebook) = run.notebook.as_mut() else {
            return Ok(());
        };
        let pieces = notebook.push_text(text);
        self.log_parts(tree, &pieces)?;
        let Some(run) = self.run_mut() else {
            return Ok(());
        };
        let Some(notebook) = run.notebook.as_mut() else {
            return Ok(());
        };
        // **From the notebook, not from the chunk.** `push_text` may drop a
        // span it has just been handed — a provider leaking its reasoning
        // into the reply channel (`Notebook::drop_leaked_reasoning`) — and
        // this is the text that gets logged as `Completion.text` and
        // replayed to the model as its own turn. Appending the raw chunk
        // here would put the leak back in the one place it does the most
        // harm: the model's own mouth, as an example of how to write a
        // reply.
        self.streaming_reply = notebook.reply().to_owned();
        Ok(())
    }

    /// The completion has finished arriving: hand over the trailing prose and
    /// let the run close.
    ///
    /// Returns `false` when no streaming notebook turn was under way, so the
    /// caller falls back to the batch path.
    pub fn notebook_stream_end(
        &mut self,
        tree: &mut Tree,
        truncated: bool,
        usage: Option<crate::host::Usage>,
        thinking: Option<String>,
    ) -> io::Result<Option<Vec<StepOutput>>> {
        // **Mark the reply ended before anything else can return.** A
        // raise or a trap in a cell parks the run *and clears the
        // epoch*, so the guard below would return first and the reply's
        // notebook would stay open — and a notebook that has not ended
        // never closes its run. The handler decides, the original
        // resumes, its last cell finishes, `advance_notebook` finds no
        // more pieces and asks whether the reply is over; the answer was
        // "no", forever, and the branch sat at Running with no terminal.
        //
        // The comment below this said it was handled. It was written
        // for the `Suspended` arm of the match and put *after* the
        // guard that makes the arm unreachable.
        if self.streaming_epoch.is_none() {
            if self.is_suspended() {
                let tail = match self.run_mut().and_then(|r| r.notebook.as_mut()) {
                    Some(notebook) => notebook.end_truncated(truncated),
                    None => Vec::new(),
                };
                self.log_parts(tree, &tail)?;
            }
            return Ok(None);
        }
        // **What the completion cost and said, recorded once.** Every
        // `Turn` this reply produced is already on the log — each was
        // written before its cell ran, which is while this completion was
        // still streaming (D15) — and the log is append-only, so there is
        // no `Turn` left to hang the figures on.
        // **Mark the reply ended wherever its run is**, suspended
        // included. A raise or a trap in an early cell parks the run, and
        // this used to return before marking it — so the notebook stayed
        // open, and when a handler resumed it the remaining cells waited
        // for a fence that was never coming. The reply is over either
        // way; whether its run can proceed is a separate question,
        // answered below.
        if truncated {
            self.reply_ended = Some(ReplyEnd::Truncated);
        }
        let tail = match self.run_mut() {
            Some(run) => match run.notebook.as_mut() {
                Some(notebook) => notebook.end_truncated(truncated),
                None => Vec::new(),
            },
            None => return Ok(Some(Vec::new())),
        };
        self.log_parts(tree, &tail)?;
        // **The end is logged last.** Trailing prose only becomes a
        // piece when the reply ends, so draining it first is what keeps
        // a reply's parts *before* its `ReplyEnd` — and the parts
        // concatenating back to the reply is the whole design (28).
        self.finish_notebook_generation(tree, usage, thinking)?;
        if !matches!(self.phase, Phase::Running(_)) {
            // Parked. The cells that are left run when a handler decides.
            return Ok(Some(Vec::new()));
        }
        Ok(Some(self.drive_notebook(tree)?))
    }

    /// Run whatever the notebook now has to run.
    ///
    /// **The VM must not be stepped when there is nothing to step.** A cell
    /// ends with `Pause`, which leaves `ip` at the append position — one past
    /// the last instruction — and stepping there runs off the end of the code
    /// and reports `Done`, ending the run before the next cell has even been
    /// written. So a VM parked at the end is advanced by *compiling* first,
    /// not by stepping: the code vector grows in front of it, and only then is
    /// there anything to execute.
    fn drive_notebook(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        // **A halted program is driven no further.** `finish`/`stop`
        // ended it part-way through this reply; what is left of the
        // reply is logged (by `notebook_feed`, before this is reached)
        // and then dropped unrun. This is the one door the rest of the
        // reply comes through, so it is the one place that has to
        // notice — and the place that ends the turn when the last of it
        // lands.
        if self.halted() {
            return self.halt_if_ready(tree, Vec::new());
        }
        let parked = match &self.phase {
            Phase::Running(run) => run.vm.ip as usize >= run.vm.code.len(),
            _ => return Ok(Vec::new()),
        };
        if !parked {
            // Mid-cell — suspended on an await, most likely. The pump will
            // reach the new pieces when this cell finishes.
            return self.pump(tree, TICK_FUEL);
        }
        let mut out = Vec::new();
        match self.advance_notebook(tree, &mut out)? {
            NotebookStep::Ran => {
                let more = self.pump(tree, TICK_FUEL)?;
                out.extend(more);
                Ok(out)
            }
            NotebookStep::Waiting => Ok(out),
            NotebookStep::Failed(report) => {
                self.suspend(tree, SuspendCause::CellCompileFailed(report), out)
            }
        }
    }

    /// Start the streaming run if it has not started, returning whether there
    /// is one to feed.
    /// Close out whatever reply was being assembled and open a fresh one.
    ///
    /// `epoch` names the generation this reply belongs to, or `None` for a
    /// reply that arrived whole rather than as chunks — a user taking the
    /// branch's turn by hand, or a client that does not stream.
    ///
    /// **No phase guard.** It used to refuse anything but `AwaitingLlm`,
    /// which made the `Phase::Suspended` arm below unreachable — so a
    /// *handler's* reply, which by definition streams while the branch is
    /// suspended, had every chunk dropped in silence and a `raise` was
    /// never answered on this transport. Every phase this can be reached
    /// in is a phase a reply can legitimately arrive in, so each is
    /// handled rather than refused: a suspended run is parked on
    /// `beneath` for its handler to decide, and a still-running one is
    /// discarded the way `apply_turn` discards it.
    fn open_notebook_reply(
        &mut self,
        tree: &mut Tree,
        epoch: Option<u64>,
        author: Author,
    ) -> io::Result<()> {
        self.finish_notebook_generation(tree, None, None)?;
        // **A reply that cannot be opened is an error, not a `false`.**
        // Both of these used to swallow the failure and return
        // `Ok(false)` — and both callers discard the `bool`, so the
        // branch would keep `streaming_epoch` and `reply_id` pointing
        // at the *previous* reply and append the new completion's parts
        // to it. That reply has already handed back; the log then says
        // a reply grew after it ended, and every later chunk retries the
        // open, fails again, and appends again.
        //
        // Nothing in the corpus proves this ever fired — `Ok(false)` is
        // unreachable-or-silent, which is exactly why it has to stop
        // being silent. If it never happens, nothing changes; if it
        // does, the host surfaces an error instead of quietly
        // corrupting the record.
        let mut vm = VM::for_incremental(self.spine.context().input(tree), serde_json::Value::Null)
            .map_err(|e| {
                io::Error::other(format!("could not start a VM for the next reply: {e:?}"))
            })?;
        let notebook = crate::notebook::Notebook::new(&mut vm)
            .map_err(|e| io::Error::other(format!("could not open the next reply: {e}")))?;
        // **A running program displaced by a new one.** Reachable from
        // `SessionCommand::Restart` — the TUI's rewrite gesture
        // (`debug/attach.rs`) — with a program mid-flight: `cmd_restart`
        // cancels the generation, not the run, and nothing on the path
        // to here checks the phase. This used to drop the `Run` on the
        // floor: no handback, no status, no `last_vm`, so the log went
        // on saying the program was running and its VM was simply gone.
        // The same silent loss `4898a80` fixed on the other door, in the
        // one slot that still holds a `Run`.
        //
        // `Superseded` is what happened, and it names the reply whose
        // program it was, because `reply_id` is still that reply here —
        // the new one is logged a few lines below.
        if let Phase::Running(run) = std::mem::replace(&mut self.phase, Phase::Idle) {
            let discarded = run.program_id;
            self.note_status(discarded, ProgramStatus::Failed);
            tree.append(
                &mut self.spine,
                EventPayload::Handback {
                    program: discarded,
                    how: Handback::Superseded,
                    site: 0,
                    stack: Vec::new(),
                },
            )?;
            self.last_vm = Some(run.vm);
        }
        // **A parked run's reply is over — a new one is arriving.** A run
        // parked by a raise or a trap never saw `notebook_stream_end`:
        // the host does not deliver a completion into a parked frame, so
        // this is the only thing that closes that reply out.
        //
        // A notebook that has not ended never closes its run. When the
        // handler decided and the original resumed, its last cell
        // finished, `advance_notebook` found no more pieces and asked
        // whether the reply was over — the answer was "no", forever. The
        // branch sat at `Running` with no terminal, no `Return`, no
        // `Console`: a raise could be answered and the program it
        // belonged to could never finish.
        //
        // `is_ended` guards the repeat: the frame stays on `parked`
        // across as many replies as it takes to decide about it, where
        // it used to be moved off `phase` exactly once.
        if let Some(notebook) = self.parked.last_mut().and_then(|p| p.run.notebook.as_mut())
            && !notebook.is_ended()
        {
            notebook.end();
        }
        self.generation += 1;
        // **The reply is logged before a byte of it arrives** (28).
        // Everything the reply produces names this id, and a generation
        // that dies before saying anything still leaves the record that
        // it was attempted — a provider error used to leave none.
        self.reply_id = match author {
            Author::User => tree.append(&mut self.spine, EventPayload::Restart)?,
            _ => tree.append(&mut self.spine, EventPayload::Reply)?,
        };
        self.phase = Phase::Running(Run {
            returned: None,
            unstarted: Vec::new(),
            console_logged: 0,
            program_id: self.reply_id,
            vm,
            notebook: Some(notebook),
        });
        self.streaming_epoch = epoch.or(Some(u64::MAX));
        Ok(())
    }

    /// Record what the generation being assembled cost and said, and stop
    /// assembling it. Idempotent: a reply already closed out closes again
    /// for nothing.
    ///
    /// **Called from every path that ends a generation**, and — because
    /// that list has been wrong twice — also from
    /// [`open_notebook_reply`](Self::open_notebook_reply), so a path
    /// nobody thought of still cannot leave a reply half-open.
    fn finish_notebook_generation(
        &mut self,
        tree: &mut Tree,
        usage: Option<crate::host::Usage>,
        thinking: Option<String>,
    ) -> io::Result<()> {
        if self.streaming_epoch.take().is_none() {
            return Ok(());
        }
        // The live buffer belongs to the reply that is ending; the
        // text itself is already on the log, part by part.
        self.streaming_reply.clear();
        // **Logged even when there is nothing to report.** A cancelled
        // generation has no usage, but "no event" and "no usage" must not
        // be the same state: that is what made a third of the arm's
        // completions invisible, and every per-reply metric was divided
        // by the wrong number.
        if let Some(thinking) = thinking.filter(|t| !t.is_empty()) {
            tree.append(
                &mut self.spine,
                EventPayload::Part {
                    reply: self.reply_id,
                    part: Part::Thinking(thinking),
                },
            )?;
        }
        let usage = usage.unwrap_or_default();
        if usage.prompt > 0 {
            // The reply itself lands in the document, so the request
            // after this one carries both. A cancelled generation
            // reports no usage at all and leaves the old floor standing
            // rather than replacing it with zero.
            let survives = usage.completion.saturating_sub(usage.reasoning);
            self.next_prompt_floor = Counted::Floor(usage.prompt + survives);
            // **This conversation's own density**, measured rather than
            // assumed: the bytes the last request rendered to, over the
            // tokens the provider charged for them. It is not a
            // bytes-to-tokens constant — it is this document's ratio,
            // recomputed every reply, and it exists so the trigger can
            // see growth that happened *after* the count it is holding.
            // **Taken, not read.** The pairing is only sound when the
            // size recorded is the size of *this* request, and
            // `compaction_if_needed` returns before rendering while a
            // compaction is already outstanding — so that request's
            // bytes are never recorded, and reading a stale number here
            // would calibrate the density against the wrong document.
            if let Some(bytes) = self.last_rendered_bytes.take() {
                self.bytes_per_token = Some(bytes as f64 / usage.prompt as f64);
            }
        }
        tree.append(
            &mut self.spine,
            EventPayload::ReplyEnd {
                reply: self.reply_id,
                how: self.reply_ended.take().unwrap_or(ReplyEnd::Finished),
                usage,
            },
        )?;
        Ok(())
    }

    /// **Log the reply's parts as they arrive**, which is what makes
    /// them concatenate back to it (28). They used to be logged as they
    /// were *consumed* — at execution — so a reply that arrived whole
    /// logged its `ReplyEnd` before any of its own text.
    fn log_parts(&mut self, tree: &mut Tree, pieces: &[crate::notebook::Piece]) -> io::Result<()> {
        let reply = self.reply_id;
        let outer: Vec<Part> = {
            let Some(run) = self.run_ref() else {
                return Ok(());
            };
            let Some(nb) = run.notebook.as_ref() else {
                return Ok(());
            };
            pieces
                .iter()
                .map(|piece| match piece {
                    crate::notebook::Piece::Prose(t) => Part::Prose(t.clone()),
                    crate::notebook::Piece::Cell(i) => Part::Cell(nb.cell_outer(*i)),
                })
                .collect()
        };
        for part in outer {
            tree.append(&mut self.spine, EventPayload::Part { reply, part })?;
        }
        Ok(())
    }

    /// The generation ended some way other than a completion arriving —
    /// cancelled, superseded, errored, interrupted. The reply is over.
    ///
    /// **And says so.** The text stops mid-sentence, and a reply whose
    /// `ReplyEnd` reads `Finished` gives the model no reason for that —
    /// it reads its own last turn breaking off and has to invent one.
    /// `Interrupted` renders as a marker where the text stops.
    pub fn notebook_generation_ended(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        if self.streaming_epoch.is_some() {
            self.reply_ended.get_or_insert(ReplyEnd::Interrupted);
        }
        // **The reply is over however it ended**, so the notebook it
        // was filling is closed too. Without this a program that had
        // already halted itself (`finish`/`stop`) and was waiting for the
        // last of its own reply would wait forever: cancellation is the
        // one ending that does not come through `notebook_stream_end`,
        // which is where every other reply gets its `end()`.
        if let Some(notebook) = self.run_mut().and_then(|r| r.notebook.as_mut()) {
            let tail = notebook.end();
            self.log_parts(tree, &tail)?;
        }
        self.finish_notebook_generation(tree, None, None)?;
        self.halt_if_ready(tree, Vec::new())
    }

    /// Whether a suspension should cancel the generation still in
    /// flight.
    ///
    /// **Cancel when the text that follows was written on a premise we
    /// now know is false** (28). That is one sentence, and it decides
    /// every case:
    ///
    /// - `Trapped`, `CellFailed` — cancel. Everything the model wrote
    ///   after that block assumed the block succeeded.
    /// - `Raised`, `Posted` — keep streaming. The model knew it was
    ///   asking; a message arriving falsifies nothing it wrote. The
    ///   cells after the raise run when the answer lands.
    /// - `finish(text)`, `stop(reason)` — halt rather than park, so they
    ///   never reach here. Everything after them is waste by the same
    ///   argument, but the ending is applied when the reply closes
    ///   (`Run::halted`), and closing it early would hand that ending
    ///   its outputs out of order. See the call site in `host/mod.rs`.
    ///
    /// The rule used to be "any suspension cancels", which contradicted
    /// the card — *"the blocks after this one do not run until it is
    /// answered"*, not *"are never written"* — and made the semantics
    /// depend on **provider speed**: if the later fences had already
    /// streamed they ran, and if not they had never been written. Same
    /// reply, same model, different behaviour.
    /// Whether a streamed reply is open — i.e. whether a generation is
    /// mid-flight as far as the log is concerned. The host asks before
    /// superseding one, so the reply it was carrying is closed rather
    /// than left owning `streaming_epoch`.
    pub fn notebook_generation_open(&self) -> bool {
        self.streaming_epoch.is_some()
    }

    pub fn notebook_cancels_generation(&self) -> bool {
        self.streaming_epoch.is_some() && self.is_suspended() && self.pause_falsifies_the_rest
    }
}

/// What one turn of the notebook driver did.
enum NotebookStep {
    /// A cell was compiled (or the run's epilogue was), so there is code to
    /// step into.
    Ran,
    /// Nothing complete to act on and the reply is still arriving.
    Waiting,
    /// A cell did not compile; the report is the repair loop's.
    Failed(String),
}

impl Runner {
    /// Turn a raw instruction span into the `site` a `Call` is logged with.
    ///
    /// `Call::site` means "an offset into the owning `Turn`'s `source`", and
    /// that meaning is kept exactly (D1). Under `Transport::Notebook` the
    /// owning `Turn` is one cell, while the spans the compiler emitted are
    /// offsets into the whole unit — absolute so the analyzer's span-keyed
    /// tables do not collide across cells (D2) — so the cell's own origin is
    /// subtracted here, at log time. Nothing downstream sees an absolute
    /// offset, and no other transport is touched.
    fn rebase_site(&self, raw: u32) -> u32 {
        match self.run_ref() {
            Some(run) => run.notebook.as_ref().map_or(raw, |nb| nb.rebase_site(raw)),
            None => raw,
        }
    }

    /// Walk the reply's pieces until something is compiled, logging each as
    /// it is acted on.
    ///
    /// Prose is a `Call::Send { to: User }` — a prose segment *is* a message
    /// to the person, and it renders in history exactly as a `tell` does, so
    /// the model re-reads its own reply in a shape it already knows (D15).
    /// It is logged and handed to the host through the same `Sends` door an
    /// unawaited `tell` uses, which is also what settles it: there is no VM
    /// promise behind it, and `on_tool_results` already treats a `None`
    /// pending lookup as the ordinary unwaited case.
    ///
    /// A cell is a `Turn` whose `source` is that cell's JavaScript, logged
    /// immediately before the cell's instructions run.
    fn advance_notebook(
        &mut self,
        tree: &mut Tree,
        out: &mut Vec<StepOutput>,
    ) -> io::Result<NotebookStep> {
        loop {
            let Phase::Running(run) = &mut self.phase else {
                unreachable!("advance_notebook outside Running");
            };
            let Some(notebook) = run.notebook.as_mut() else {
                unreachable!("advance_notebook with no notebook");
            };
            match notebook.take_piece() {
                Some(crate::notebook::Piece::Prose(verbatim)) => {
                    // **Verbatim on the log, trimmed to the person.** The
                    // part carries every byte so the reply can be put
                    // back together exactly (28); what someone reads is
                    // the same text without the blank lines that
                    // separated it from the fences — and without the
                    // `↓ history[N]` annotations the model copies out of
                    // its own document, which are ours and mean nothing
                    // to the person reading them.
                    let cleaned = crate::document::strip_imitated_markers(&verbatim);
                    let trimmed = cleaned.trim();
                    // **Nothing but punctuation is nothing to say.** A
                    // stray ` ``` ` — a fence the model opened and shut
                    // with no cell in it, or one closed twice — parses
                    // as prose and is not blank, so it reached the
                    // person as a message whose whole content was
                    // backticks. Seen live on 2026-09-22 (`new-2` of
                    // the card A/B): a reply whose second message to
                    // the person was "```\n```".
                    if trimmed
                        .lines()
                        .all(|l| l.trim().is_empty() || l.trim().starts_with("```"))
                    {
                        continue;
                    }
                    let text = crate::report::cap_prose(trimmed);
                    let send = tree.append(
                        &mut self.spine,
                        EventPayload::Call(Call::Send {
                            to: Address::User,
                            prose: true,
                            text,
                            input: serde_json::Value::Null,
                            options: Vec::new(),
                            expects_reply: false,
                            // Synthetic: no instruction issued this, so
                            // there is no source expression to point at.
                            // Zero width is the convention `span.rs`
                            // already names for exactly that.
                            site: 0,
                            site_end: 0,
                        }),
                    )?;
                    out.push(StepOutput::Sends(vec![send]));
                }
                Some(crate::notebook::Piece::Cell(i)) => {
                    // The part goes in **before** the cell is compiled,
                    // not merely before it runs. A cell that does not
                    // compile has to leave its source in the log too, or
                    // the repair loop is handed a diagnostic with nothing
                    // to read it against.
                    //
                    // It carries the cell's fences (28): that is what
                    // keeps the parts concatenating back to the reply,
                    // and what makes a cell's offset the sum of the
                    // lengths before it.
                    // The part is already on the log — it was written
                    // when it arrived (28). What identifies the run is
                    // the reply itself.
                    let turn = self.reply_id;
                    let Phase::Running(run) = &mut self.phase else {
                        unreachable!()
                    };
                    run.program_id = turn;
                    let notebook = run.notebook.as_mut().expect("a notebook");
                    if let Err(report) = notebook.feed_cell(&mut run.vm, i) {
                        return Ok(NotebookStep::Failed(report));
                    }
                    self.note_status(turn, ProgramStatus::Running);
                    return Ok(NotebookStep::Ran);
                }
                None => {
                    if !notebook.is_ended() {
                        return Ok(NotebookStep::Waiting);
                    }
                    // **Truncation does not end the run** (28): it is
                    // a fact about the text, recorded on `ReplyEnd`.
                    // Every cell that arrived ran, so the run closes
                    // the way any other reply's does — and the model
                    // learns why its own words stop mid-sentence from
                    // the marker the document renders there, not from
                    // a second, differently-worded terminal.
                    if let Err(report) = notebook.close(&mut run.vm) {
                        return Ok(NotebookStep::Failed(report));
                    }
                    return Ok(NotebookStep::Ran);
                }
            }
        }
    }
}
fn span_at(vm: &VM, ip: usize) -> u32 {
    vm.spans.get(ip).map(|s| s.start).unwrap_or(0)
}

/// Who authored the post `question` — the author an answer is owed to.
fn asker_of(tree: &Tree, question: EventId) -> Option<Author> {
    match &tree.events.get(&question)?.payload {
        EventPayload::Post { from, .. } => Some(*from),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Conversation, Ending, Invariant};
    use serde_json::json;

    const FUEL: u64 = 100_000;

    fn setup() -> (Tree, Runner) {
        setup_under()
    }

    /// `setup`, but for the tests that are *about* the wire container.
    /// The transport is set on the runner rather than in the process
    /// environment, so these tests say which container they mean and two
    /// of them can run at once on different threads — the env-var helper
    /// this replaced could not manage either (see `document::Transport`).
    fn setup_under() -> (Tree, Runner) {
        let mut tree = Tree::new(None);
        let state = Runner::new_root(&mut tree, "you are a test agent", "").unwrap();
        (tree, state)
    }

    /// A post whose body is inline — the user's and the harness's shape,
    /// the two authors with no send side.
    fn direct(text: &str, expects_reply: bool) -> Origin {
        Origin::Direct {
            text: text.into(),
            input: serde_json::Value::Null,
            options: Vec::new(),
            expects_reply,
        }
    }

    /// The user speaks *inside* a branch: one `Post`, delivered.
    fn user_post(state: &mut Runner, tree: &mut Tree, text: &str) -> Vec<StepOutput> {
        state
            .deliver(tree, Author::User, direct(text, true))
            .unwrap()
            .1
    }

    /// An agent's question, the full exchange shape: a `Send` on the
    /// asker's branch, a `Post` naming it on the answerer's. Returns the
    /// `Send` and what the answerer does next.
    fn ask(
        tree: &mut Tree,
        asker: &mut Runner,
        callee: &mut Runner,
        text: &str,
        input: serde_json::Value,
    ) -> (EventId, Vec<StepOutput>) {
        let to = Address::Branch(callee.agent_id());
        let asker_id = asker.agent_id();
        let send = tree
            .append(
                &mut asker.spine,
                EventPayload::Call(Call::Send {
                    prose: false,
                    to,
                    text: text.into(),
                    input,
                    options: Vec::new(),
                    expects_reply: true,
                    site: 0,
                    site_end: 0,
                }),
            )
            .unwrap();
        let (_, out) = callee
            .deliver(tree, Author::Agent(asker_id), Origin::Sent(send))
            .unwrap();
        (send, out)
    }

    /// A spawned agent and its first question.
    fn spawn_and_ask(
        tree: &mut Tree,
        asker: &mut Runner,
        charter: &str,
        input: serde_json::Value,
    ) -> (Runner, Vec<StepOutput>) {
        let spawn = tree
            .append(
                &mut asker.spine,
                EventPayload::Call(Call::Spawn {
                    name: None,
                    charter: charter.into(),
                    tools: None,
                    site: 0,
                }),
            )
            .unwrap();
        let mut child = Runner::new_agent(tree, spawn, None, charter, None, "").unwrap();
        let (_, out) = ask(tree, asker, &mut child, charter, input);
        (child, out)
    }

    /// **Every verb the card advertises has something that answers
    /// it.** `card.rs`'s `the_card_names_every_bare_verb` checks the
    /// card *mentions* each verb, and nothing checked the harness
    /// *answers* any of them — which is how `list_agents()` shipped
    /// advertised and unimplemented for three phases, falling past
    /// `dispatch_calls` into the tool registry and coming back `unknown
    /// tool \`list_agents\``.
    ///
    /// The list is `interp::HARNESS_VERBS`, the compiler's own closed
    /// vocabulary, so a verb cannot be added to the dialect without
    /// somewhere here growing an arm for it. What "answers it" means
    /// depends on how it lowers, and each case is checked against the
    /// real dispatcher rather than a copy of its match:
    ///
    /// - `Settle` — `dispatch_settle` must not fall through to its
    ///   "not a settle-at-dispatch verb" arm.
    /// - `Invoke`/`Notify` — `dispatch_calls` must recognise the name
    ///   rather than shipping it to the registry, *or* the loop must
    ///   serve it inline (`host::serves_inline`).
    #[test]
    fn every_harness_verb_has_an_answerer() {
        for verb in interp::HARNESS_VERBS {
            // Enough arguments that the arity-checked verbs compile;
            // nothing here runs past the first call, and a complaint
            // about the *arguments* is a real answer for this purpose.
            // `finish` and `stop` are effects: they halt the VM rather
            // than settling a call, so there is no answerer to find.
            if *verb == "finish" || *verb == "stop" {
                continue;
            }
            let src = if *verb == "fork" {
                format!("{verb}();")
            } else {
                format!("{verb}(1, 2, 3);")
            };
            let (mut tree, mut state) = setup();
            let program = interp::compile(&src).unwrap_or_else(|e| panic!("{verb}: {e:?}"));
            let mut vm = VM::for_program(program, serde_json::Value::Null).unwrap();
            match vm.step(FUEL).unwrap() {
                StepResult::Settle { call } => {
                    state.phase = Phase::Running(Run {
                        returned: None,
                        unstarted: Vec::new(),
                        console_logged: 0,
                        program_id: state.spine.leaf_id,
                        vm,
                        notebook: None,
                    });
                    let mut out = Vec::new();
                    state.dispatch_settle(&mut tree, call, &mut out).unwrap();
                    // The fallthrough arm throws this exact sentence;
                    // any other outcome — a value, or a complaint about
                    // the arguments — means the verb was recognised.
                    let vm = state.settling_vm();
                    let unanswered = vm.stack.iter().any(|v| {
                        matches!(v, Value::String(s)
                            if s.as_str().contains("is not a settle-at-dispatch verb"))
                    });
                    assert!(
                        !unanswered,
                        "`{verb}` reached dispatch_settle's fallthrough"
                    );
                    // A settle verb the runner cannot answer itself is
                    // logged as a tool call and served a round trip
                    // later. The loop must actually serve it: the
                    // registry has no tool named after a bare verb, so
                    // anything else comes back `unknown tool`, which is
                    // precisely the bug this gate exists for.
                    for output in &out {
                        if let StepOutput::ToolCalls(calls) = output {
                            for c in calls {
                                assert!(
                                    crate::host::serves_inline(&c.name),
                                    "`{verb}` is dispatched as tool `{}`, which nothing serves",
                                    c.name
                                );
                            }
                        }
                    }
                }
                StepResult::Pending { calls } => {
                    let name = calls[0].name.as_str();
                    assert!(
                        matches!(name, TOOL_ASK | TOOL_CHOOSE | TOOL_TELL)
                            || crate::host::serves_inline(name),
                        "`{verb}` falls through dispatch_calls to the tool registry, \
                         which has no such tool"
                    );
                }
                StepResult::Done { unstarted, .. } => {
                    // `tell` lowers to `Notify`: nothing awaits it, so
                    // the program ran to the end and the call is in the
                    // fire-and-forget batch instead.
                    let name = unstarted[0].name.as_str();
                    assert!(
                        matches!(name, TOOL_ASK | TOOL_CHOOSE | TOOL_TELL)
                            || crate::host::serves_inline(name),
                        "`{verb}` falls through dispatch_calls to the tool registry"
                    );
                }
                other => panic!("`{verb}`: nothing dispatched it ({other:?})"),
            }
        }
    }

    fn llm_program(source: &str) -> LlmTurn {
        crate::host::scripted_program(source)
    }

    /// Drive `Tick`s until the machine stops asking for them; collects
    /// every non-`Working` output.
    fn drain(state: &mut Runner, tree: &mut Tree, outputs: Vec<StepOutput>) -> Vec<StepOutput> {
        let mut result = Vec::new();
        let mut queue = outputs;
        for _ in 0..1000 {
            let mut working = false;
            for o in queue {
                match o {
                    StepOutput::Working => working = true,
                    other => result.push(other),
                }
            }
            if !working {
                return result;
            }
            queue = state.step(tree, StepInput::Tick { fuel: FUEL }).unwrap();
        }
        panic!("machine never settled");
    }

    /// The most recent report the LLM would read: derived (not stored —
    /// `document.rs::render` does exactly this on every render) from the
    /// last outcome (`Return`/`Condition`) on the branch.
    fn last_report(state: &Runner, tree: &Tree) -> String {
        let leaf = state.spine.leaf_id;
        let outcome = tree
            .path_events(leaf)
            .iter()
            .rev()
            .find(|e| matches!(e.payload, EventPayload::Handback { .. }))
            .map(|e| e.id)
            .expect("an outcome to render");
        crate::report::derive_report(tree, leaf, outcome, TEST_BUDGET)
    }

    fn payload_kinds(state: &Runner, tree: &Tree) -> Vec<&'static str> {
        state
            .agent_segment(tree)
            .iter()
            .map(|e| match &e.payload {
                EventPayload::Agent { .. } => "Agent",
                EventPayload::Fork { .. } => "Fork",
                EventPayload::Answer { .. } => "Answer",
                EventPayload::Post { .. } => "Post",
                EventPayload::Reply => "Reply",
                EventPayload::RequestFailed { .. } => "RequestFailed",
                EventPayload::Render { .. } => "Render",
                EventPayload::Compaction { .. } => "Compaction",
                EventPayload::Part { .. } => "Part",
                EventPayload::ReplyEnd { .. } => "ReplyEnd",
                EventPayload::Restart => "Restart",
                EventPayload::Handback { .. } => "Handback",
                EventPayload::Call(_) => "Call",
                EventPayload::Result { .. } => "Result",
                EventPayload::Console { .. } => "Console",
                EventPayload::Rename { .. } => "Rename",
                EventPayload::Note { .. } => "Note",
                EventPayload::Compacted { .. } => "Compacted",
            })
            .collect()
    }

    /// The reply's parts and the calls that ran off them, each in their
    /// own order — everything about a reply's log **except** where
    /// `ReplyEnd` falls, which is the one thing that legitimately
    /// depends on when the provider stopped talking.
    fn parts_and_calls(state: &Runner, tree: &Tree) -> (Vec<String>, Vec<String>) {
        let mut parts = Vec::new();
        let mut calls = Vec::new();
        for e in state.agent_segment(tree) {
            match &e.payload {
                EventPayload::Part { part, .. } => match part {
                    crate::types::Part::Prose(t) | crate::types::Part::Cell(t) => {
                        parts.push(t.clone())
                    }
                    crate::types::Part::Thinking(_) => {}
                },
                EventPayload::Call(c) => calls.push(format!("{c:?}")),
                _ => {}
            }
        }
        (parts, calls)
    }

    fn expect_request(outputs: &[StepOutput]) -> &LlmRequest {
        outputs
            .iter()
            .find_map(|o| match o {
                StepOutput::LlmRequest(r) => Some(r),
                _ => None,
            })
            .expect("an LlmRequest output")
    }

    fn expect_tool_calls(outputs: &[StepOutput]) -> &Vec<OutCall> {
        outputs
            .iter()
            .find_map(|o| match o {
                StepOutput::ToolCalls(c) => Some(c),
                _ => None,
            })
            .expect("a ToolCalls output")
    }

    // ── the basic round trip ────────────────────────────────────────

    #[test]
    fn user_turn_renders_request() {
        let (mut tree, mut state) = setup();
        let out = user_post(&mut state, &mut tree, "compute 6*7");
        let req = expect_request(&out);
        assert!(req.tail.as_deref().unwrap_or("").contains("attached"));
    }

    /// Gate: `Call::Send::site_end` is the call's own end, not a copy of
    /// `site` and not the enclosing statement's end. Compiles and runs a
    /// program containing `tell("hello")`, finds the logged `Call::Send`,
    /// and asserts `source[site..site_end]` is exactly `tell("hello")` —
    /// which only holds if `site` is the call's start and `site_end` its
    /// real end (a bug either off-by-one, or reusing `site` for both,
    /// would show up as extra/missing characters here).
    #[test]
    fn call_send_site_end_bounds_the_whole_call_expression() {
        let (mut tree, mut state) = setup();
        user_post(&mut state, &mut tree, "go");
        let source = "tell(\"hello\");";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(source)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let sends = settled
            .iter()
            .find_map(|o| match o {
                StepOutput::Sends(s) => Some(s.clone()),
                _ => None,
            })
            .expect("tell() dispatches as a Sends output");
        assert_eq!(sends.len(), 1);
        let EventPayload::Call(Call::Send { site, site_end, .. }) = &tree.events[&sends[0]].payload
        else {
            panic!("expected a Send");
        };
        let (site, site_end) = (*site as usize, *site_end as usize);
        // Reply-absolute (28): the site indexes the markdown the model
        // wrote, fences and all, not the bare cell body.
        let reply = format!("```js\n{source}\n```\n");
        assert_eq!(&reply[site..site_end], r#"tell("hello")"#);
    }
    /// `history.note` is a settle-at-dispatch verb, so its span comes
    /// through `SettleCall` rather than `InvokeCall` — a path that
    /// carried no end offset until the note needed one. End to end
    /// because that plumbing is the part with nothing else watching it.
    #[test]
    fn an_append_is_cross_referenced_to_the_note_it_wrote() {
        let mut c = Conversation::new();
        c.user("go");
        let reply = "```js\nhistory.note({ found: 3 });\n```\n";
        let r = c.reply(reply);

        assert_eq!(
            r.row().source_in(reply),
            "history.note({ found: 3 })",
            "the span is the whole call"
        );
        assert!(
            c.document()
                .contains(&format!("/* ← history[{}] */", r.row().id.as_u64())),
            "and the call points at the row it wrote: {}",
            c.document()
        );
    }

    /// End to end: the compiler's spans and the renderer's snip, on a
    /// real program run through the machine.
    ///
    /// The two halves were tested apart — that `source[site..site_end]`
    /// is the whole call, and that a hand-built span renders as a
    /// reference — and apart is where an off-by-one lives. This is the
    /// only test that fails if either side drifts.
    #[test]
    fn a_literal_tell_snips_against_the_compiler_s_own_spans() {
        let (mut tree, mut state) = setup();
        user_post(&mut state, &mut tree, "go");
        let source = "tell(\"checked every file and the build is green after the rename\");\ntell(\"x \" + String(1));";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(source)))
            .unwrap();
        drain(&mut state, &mut tree, out);

        let doc = crate::document::render(&tree, &state.spine, 64 * 1024);
        let program = doc
            .conversation()
            .iter()
            .find(|m| m.role == crate::document::ChatRole::Assistant)
            .expect("the program renders")
            .content
            .clone();
        assert!(
            program.contains("tell(/* ← snipped - history["),
            "the long literal tell became a reference: {program}"
        );
        assert!(
            !program.contains("green after the rename\""),
            "and its bytes are not in the document twice: {program}"
        );
        assert!(
            program.contains("String(1)") && program.contains(") /* ← history["),
            "the computed one keeps its construction and takes a reference: {program}"
        );
        let all: String = doc
            .conversation()
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert!(
            all.contains(
                "you told user: checked every file and the build is green after the rename"
            ),
            "and the row carries the text: {all}"
        );
    }

    #[test]
    fn program_completion_logs_a_harness_report_and_the_branch_goes_idle() {
        let (mut tree, mut state) = setup();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program(
                    "console.log(\"hi there\"); history.note(6 * 7);",
                )),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let report = last_report(&state, &tree);
        assert!(report.contains("42"), "{report}");
        assert!(report.contains("hi there"), "{report}");
        assert_eq!(
            payload_kinds(&state, &tree),
            [
                "Agent", "Post", "Reply", "Part", "ReplyEnd", "Note", "Handback", "Console"
            ]
        );
    }

    //
    // Commit 6084a70 added the transport switch but left `RunProgram`
    // inert: the model's prose reached no log and no user (gap 1), and
    // the conversation never turned after a program finished (gap 2).
    // These four pin the fix, one per named acceptance case.

    /// **Superseded by the `finish(text)` change**: this used to pin
    /// `Transport::Program`'s own regression guard — completing a
    /// program was a silent no-op by default (this file's old words in
    /// `finish_program`), and only `Transport::RunProgram` continued.
    /// That polarity is exactly what this phase inverts (see
    /// `finish_program`'s current comment): the dominant observed
    /// failure across every card variant and both models is a program
    /// that does one step and stops, so completing now continues on
    /// *both* transports unless the program called `finish(text)`. What was
    /// this test's assertion is now `finish_ends_the_conversation`'s; this
    /// one instead pins the new default.
    #[test]
    fn a_completed_program_continues_by_default() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("tell(\"working on it\");")),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            settled
                .iter()
                .any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "a completed program continues by default now, on both \
             transports, unless it called finish(text): {settled:?}"
        );
        assert!(!state.is_idle());
    }

    /// `finish(text)` is the one thing that rests the branch: the mirror
    /// image of the test above, same shape, only the program's text
    /// differs.
    #[test]
    fn finish_ends_the_conversation() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("tell(\"ok\"); finish();\n")),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            !settled
                .iter()
                .any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "finish(text) ends the conversation with no further request: {settled:?}"
        );
        assert!(state.is_idle());
    }

    /// **A second message while a program is parked must not destroy
    /// it.** Live in `lab/live2` on 2026-09-21: a `bash` was in flight,
    /// "actually stop" parked the program at its next fuel slice, a
    /// second line arrived before the command finished, and when the
    /// result landed the harness announced it as "settled with no
    /// program awaiting it".
    ///
    /// The cause was `prompt_if_needed` stamping `AwaitingLlm` over
    /// `Phase::Suspended`, which dropped the parked `Run`. The
    /// orphaned result is the visible half; the invisible half was that
    /// the `Posted` handback on the log still offered a `resume()` of a
    /// frame that had ceased to exist.
    #[test]
    fn a_second_post_leaves_the_parked_program_where_it_was() {
        let mut c = Conversation::new();
        c.never_answers("bash");
        c.user("run it");
        c.reply(
            "Running it.\n\n```js\nconst r = await tools.bash(\"./slow.sh\");\n\
             tell(r.stdout);\nfinish();\n```\n",
        );
        c.user("actually stop");
        assert_eq!(c.status(), "suspended");
        c.user("and tell me what phase it got to");
        assert_eq!(
            c.status(),
            "suspended",
            "a second post threw the suspension away"
        );

        let bash = c
            .tree()
            .events
            .values()
            .find(|e| {
                matches!(&e.payload, EventPayload::Call(crate::types::Call::Invoke { name, .. })
                    if name == "bash")
            })
            .map(|e| e.id)
            .expect("the program issued a bash call");
        c.settle(
            bash,
            Ok(serde_json::json!({"status": 0, "stdout": "phase 5 of 5\n", "stderr": ""})),
        );
        let dump = crate::transcript::render(c.tree(), c.runner().spine.leaf_id);
        assert!(
            !dump.contains("settled with no program awaiting it"),
            "the parked program's own call was orphaned:\n{dump}"
        );
    }

    /// `finish(text)` settles at dispatch (like `spawn`/`fork`), so it is
    /// recorded on the `Runner` well before the program's own
    /// completion is known — a call after it in the same program (here,
    /// a `tell()`) must not un-record it.
    #[test]
    fn finish_is_recorded_before_the_program_ends() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program(
                    "tell(\"ok\"); finish(); tell(\"wrapping up now\");",
                )),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            !settled
                .iter()
                .any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "a statement after finish(text) must not cancel it: {settled:?}"
        );
        assert!(state.is_idle());
    }

    #[test]
    fn a_reply_with_no_program_ends_the_task() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(crate::host::scripted_markdown(
                    "All done, nothing left to do.",
                )),
            )
            .unwrap();

        // No `Working` output at all: an empty `source` must never
        // reach `start_program`/`interp::compile` and manufacture a
        // spurious `crate::types::Handback::CellFailed`.
        assert!(
            !out.iter().any(|o| matches!(o, StepOutput::Working)),
            "an empty source must not start a program: {out:?}"
        );
        assert!(
            !out.iter().any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "a final answer owes no further request: {out:?}"
        );
        assert!(state.is_idle());
        assert_eq!(
            payload_kinds(&state, &tree),
            [
                "Agent", "Post", "Reply", "Part", "ReplyEnd", "Call", "Handback", "Console"
            ],
            "one reply, one part, and no cell in it"
        );

        let score = crate::score::score(&tree);
        assert_eq!(score.tells, ["All done, nothing left to do."]);
        assert_eq!(score.compile_failures, Vec::<String>::new());
    }

    #[test]
    fn compile_error_is_a_repair_loop_and_leaves_no_vm_behind() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("let = ;")))
            .unwrap();
        assert!(!out.iter().any(|o| matches!(o, StepOutput::Working)));
        let report = last_report(&state, &tree);
        assert!(report.contains("DID NOT RUN"), "{report}");
        assert_eq!(
            payload_kinds(&state, &tree),
            ["Agent", "Reply", "Part", "ReplyEnd", "Handback", "Console"]
        );
        // A cell that will not compile *suspends* the reply rather than
        // ending it: the condition is handed back and the next reply
        // repairs it, which is the repair loop this test is named for.
        assert!(
            state.is_idle() || matches!(state.status(), "awaiting llm" | "suspended"),
            "{}",
            state.status()
        );
    }

    #[test]
    fn raise_suspends_with_pushed_disposition_and_host_driven_resume_continues() {
        let mut c = Conversation::new();
        let r = c.reply(
            "```js\nconst x = raise(\"need_help\", { got: 41 });\nhistory.note(x + 1);\n```\n",
        );
        // A raise *pauses*: the run is still there to come back to,
        // which is what `suspended` means and a terminal handback does
        // not.
        assert_eq!(c.status(), "suspended");
        assert!(matches!(r.ended, Ending::Raised { .. }), "{:?}", r.ended);

        // The host, not the LLM, drives the continuation directly.
        let after = c.resume(json!(41));
        assert_eq!(after.values(), [&json!(42)]);
    }

    #[test]
    fn abandon_discards_the_suspended_vm_and_leaves_the_branch_idle() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("raise(\"need\", null); history.note(1);")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(state.status(), "suspended");

        let out = state.abandon(&mut tree).unwrap();
        // Nothing new to say (no open question, no unseen post), so
        // abandon leaves the branch quietly idle rather than forcing a
        // request out.
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(state.status(), "idle");
    }

    // **Superseded by 27.1 and then deleted with the verb.**
    // `next_program_hands_over_and_its_payload_stays_in_the_document`
    // pinned that `next_program(payload)` logs `Handover` rather than
    // `Pushed`, so its payload reached the rolling document instead of
    // only the one-shot handler prompt — a live run had handed over
    // 190KB and the program after next saw none of it.
    //
    // `return payload` does that job now and does it without a
    // condition at all: `a_completed_program_continues_by_default`
    // covers the continuation, and 27.7's
    // `a_return_value_reaches_the_next_program_whole` covers the
    // payload arriving intact, which is the property that test was
    // really about. The verb is gone (zero uses across 82 live runs
    // once no card named it), so there is nothing left to assert.

    #[test]
    fn an_id_is_addressable_in_the_form_it_is_displayed() {
        // Ids are rendered `#16` everywhere a program can see one -- the
        // artifact menu, reports, post lines -- so that is the form a
        // model writes back. Live 2026-09-16: a program held a fork from
        // an earlier turn, read its id off the menu, wrote
        // `ask("#16", ...)`, and was refused.
        let mut tree = Tree::new(None);
        let root = tree
            .start_agent(None, None, "root", None, "card", Vec::new())
            .unwrap();
        let child = tree
            .start_agent(Some(root.leaf_id), None, "worker", None, "card", Vec::new())
            .unwrap();
        let id = child.leaf_id.as_u64();
        let state = Runner::with_spine(&tree, root);

        for form in [
            serde_json::json!(id),
            serde_json::json!(format!("{id}")),
            serde_json::json!(format!("[{id}]")),
            serde_json::json!(format!("#{id}")),
        ] {
            assert!(
                state.resolve_address(&tree, Some(&form)).is_ok(),
                "{form} should address the same branch"
            );
        }
    }

    #[test]
    fn a_spawn_handle_is_usable_as_an_address() {
        // `spawn()` settles with `{"agent": id}` and the card teaches
        // `const h = spawn(...); await ask(h, ...)`. Live run 2026-09-15
        // trapped on exactly that: the handle was rejected because only
        // a bare number was accepted, so the documented spelling of the
        // documented pattern could not work.
        let mut tree = Tree::new(None);
        let root = tree
            .start_agent(None, None, "root", None, "card", Vec::new())
            .unwrap();
        let root_agent = root.leaf_id;
        let child = tree
            .start_agent(Some(root_agent), None, "worker", None, "card", Vec::new())
            .unwrap();
        let agent_id = child.leaf_id;
        let state = Runner::with_spine(&tree, root);

        let handle = serde_json::json!({ "agent": agent_id.as_u64() });
        let bare = serde_json::json!(agent_id.as_u64());
        assert_eq!(
            state.resolve_address(&tree, Some(&handle)).is_ok(),
            state.resolve_address(&tree, Some(&bare)).is_ok(),
            "a handle must address whatever the bare id addresses"
        );
    }

    #[test]
    fn a_tagged_completion_is_routed_to_resume_or_abandon() {
        let mut c = Conversation::new();
        let r = c.reply(
            "```js\nconst x = raise(\"need_help\", { got: 41 });\nhistory.note(x + 1);\n```\n",
        );
        assert_eq!(
            r.ended,
            Ending::Raised {
                name: "need_help".into(),
                payload: Some(json!({ "got": 41 }))
            }
        );
        assert_eq!(c.status(), "suspended");

        // The handler answers with a program, not a direct host call —
        // `finish_program` is the one reading the tag off its
        // completion.
        let handled = c.reply("```js\nhistory.note(resume(41));\n```\n");
        assert_eq!(
            handled.values(),
            [&json!(42)],
            "the raise resumed into its own expression"
        );
        assert_eq!(c.status(), "idle");

        // A second raise, this time abandoned the same way — through a
        // handler's own completion, not a direct host call.
        c.reply("```js\nraise(\"need\", null);\nhistory.note(1);\n```\n");
        assert_eq!(c.status(), "suspended");
        let gave_up = c.reply("```js\nhistory.note(abandon());\n```\n");
        assert_eq!(c.status(), "idle");
        assert_eq!(
            gave_up.ended,
            Ending::Abandoned,
            "the abandon is a logged handback"
        );
        // A decision is not a row: appending one records the verdict,
        // it does not write a note about it.
        assert!(
            gave_up.rows.is_empty(),
            "no decision object ever lands as a row: {:?}",
            gave_up.rows
        );
    }

    #[test]
    fn interrupt_of_a_running_program_delivers_the_rewritten_notice() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("while (true) {}")),
            )
            .unwrap();
        assert!(
            out.iter().any(|o| matches!(o, StepOutput::Working)),
            "{out:?}"
        );

        // `interrupt()` on a `Running` phase only delivers the notice
        // (`Working`, per `deliver`'s own rule for a busy branch) — the
        // program parks at its *next* fuel slice, rule B's job, so this
        // drains to let that slice actually run. It settles into
        // `Condition::Posted`, which deliberately produces no
        // `StepOutput::LlmRequest` of its own — building the prompt
        // from a suspension is the host's job (`prompt_suspended`), not
        // this file's; the `Post` this test actually checks is what
        // proves the interrupt landed.
        let out = state.interrupt(&mut tree).unwrap();
        drain(&mut state, &mut tree, out);
        let posted = state
            .agent_segment(&tree)
            .iter()
            .rev()
            .find_map(|e| match &e.payload {
                EventPayload::Post {
                    from: Author::Harness,
                    origin,
                } => origin.direct().map(|(t, _, _)| t.to_owned()),
                _ => None,
            });
        assert_eq!(posted.as_deref(), Some(INTERRUPT_NOTICE));
        assert!(
            !INTERRUPT_NOTICE.contains("run_program") && !INTERRUPT_NOTICE.contains("resume()"),
            "the notice no longer names old tool-call restarts: {INTERRUPT_NOTICE}"
        );
    }

    // ── bare-vocabulary dispatch ────────────────────────────────────

    #[test]
    fn bare_tell_and_ask_dispatch_to_send() {
        let mut tree = Tree::new(None);
        let mut root = Runner::new_root(&mut tree, "root", "").unwrap();
        let (mut child, out) =
            spawn_and_ask(&mut tree, &mut root, "child", serde_json::Value::Null);
        drain(&mut root, &mut tree, out);

        let out = child
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program(&format!(
                    "history.note(await ask({}, \"which file?\"));",
                    root.agent_id().as_u64()
                ))),
            )
            .unwrap();
        let settled = drain(&mut child, &mut tree, out);
        let sends = settled
            .iter()
            .find_map(|o| match o {
                StepOutput::Sends(s) => Some(s.clone()),
                _ => None,
            })
            .expect("a Sends output");
        assert_eq!(sends.len(), 1);
        let EventPayload::Call(Call::Send {
            to,
            text,
            expects_reply,
            ..
        }) = &tree.events[&sends[0]].payload
        else {
            panic!("expected a Send");
        };
        assert_eq!(*to, Address::Branch(root.agent_id()));
        assert_eq!(text, "which file?");
        assert!(*expects_reply);
    }

    #[test]
    fn bare_spawn_dispatches_to_call_spawn() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("history.note(await spawn(\"researcher\"));")),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let spawns = settled
            .iter()
            .find_map(|o| match o {
                StepOutput::Spawns(s) => Some(s.clone()),
                _ => None,
            })
            .expect("a Spawns output");
        let EventPayload::Call(Call::Spawn { charter, .. }) = &tree.events[&spawns[0]].payload
        else {
            panic!("expected a Spawn");
        };
        assert_eq!(charter, "researcher");
    }

    #[test]
    fn bare_fork_dispatches_to_call_fork_and_is_settled_like_a_spawn() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("history.note(fork());")),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let forks = settled
            .iter()
            .find_map(|o| match o {
                StepOutput::Forks(f) => Some(f.clone()),
                _ => None,
            })
            .expect("a Forks output");
        let EventPayload::Call(Call::Fork { .. }) = &tree.events[&forks[0]].payload else {
            panic!("expected a Fork call");
        };
    }

    #[test]
    fn note_history_logs_a_note_and_is_never_re_sent_to_context() {
        let mut c = Conversation::new();
        let r = c.reply(
            "```js\nawait note_history(\"figured out the bug is in parsing\");\n\
             history.note(1);\n```\n",
        );
        assert_eq!(
            r.values(),
            [&json!("figured out the bug is in parsing"), &json!(1)],
            "both spellings write a row"
        );
    }

    #[test]
    fn artifact_fetch_reuses_a_settled_call_by_id() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"await tools.fetch("a"); raise("stop", null);"#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let id = expect_tool_calls(&settled)[0].call;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: id,
                    result: Ok(json!("DATA")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let out = state.abandon(&mut tree).unwrap();
        drain(&mut state, &mut tree, out);
        let rewrite = format!("history.note(await fetch_history({}));", id.as_u64());
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(&rewrite)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            !settled
                .iter()
                .any(|o| matches!(o, StepOutput::ToolCalls(_))),
            "served from the log, no call re-issued"
        );
        assert!(last_report(&state, &tree).contains("DATA"));
    }

    /// **A compacted row reads back whole.** `compaction.rs` promises
    /// it "never drops an id — only content", and the compaction
    /// request tells the model "nothing is deleted". Until 27.4 that
    /// was false for a `Post`: the fetch refused anything that was not
    /// a call, a return or a console, so removing a post's content
    /// really was deleting it.
    ///
    /// Nothing in the fetch knows about compaction and nothing needs
    /// to: a `Compacted` event shadows its target only for the
    /// *renderer*, and this reads the log. The document shrinks; the
    /// history does not.
    #[test]
    fn fetch_history_reads_a_compacted_post_back_whole() {
        let mut c = Conversation::new();
        let post = c.user("the third column is the one that matters");
        c.reply(&format!("```js\nhistory.remove({});\n```\n", post.as_u64()));
        // It really is gone from what the model reads.
        assert!(
            !c.document().contains("third column"),
            "still in the document"
        );

        let r = c.reply(&format!(
            "```js\nhistory.note(await fetch_history({}));\n```\n",
            post.as_u64()
        ));
        assert_eq!(
            r.row().value,
            json!("the third column is the one that matters")
        );
        // **Reading is free.** The fetch adds nothing of its own: only
        // the program's own events appear, and none of them is a
        // `Call`/`Result` pair for the fetch. That is what lets the
        // menu be an index rather than a replay.
        assert!(r.calls.is_empty(), "served from the log, no call issued");
        assert_eq!(
            r.kinds,
            ["Reply", "Part", "Note", "ReplyEnd", "Handback", "Console"],
            "the fetch logged something of its own"
        );
    }

    /// **`append`/`fetch` round-trips.** The card promises "read any
    /// entry back, *whole*" and, until this, an object came back as its
    /// JSON *text*: a program had to know to `JSON.parse` a value it
    /// had handed over intact, and nothing said so. Six helpers in this
    /// suite were doing that parse by hand, which is the defect showing
    /// through from the other side.
    #[test]
    fn what_was_appended_comes_back_as_what_was_appended() {
        let mut c = Conversation::new();
        let first = c.reply("```js\nhistory.note({ dead: [\"a\", \"b\"], kept: 3 });\n```\n");
        let row = first.row().id.as_u64();

        let back = c.reply(&format!(
            "```js\nconst row = history.fetch({row});\n\
             history.note([typeof row, row.kept, row.dead[1]]);\n```\n"
        ));
        // An object, indexable — not a string anyone has to parse.
        assert_eq!(back.row().value, json!(["object", 3, "b"]));
    }

    /// **The row is the view; `fetch` is the value.**
    ///
    /// `history.note` was the one visible thing in the system with no
    /// bound on it, and by bytes it is how models read — 72% of
    /// everything appended across 352 kept runs was a verbatim copy of
    /// a result, rendered whole on every turn until something compacted
    /// it. Bounded at render and never on the log, so the same call is
    /// a bounded view *and* a whole value.
    #[test]
    fn a_long_appended_row_is_clipped_and_fetch_still_hands_back_all_of_it() {
        let mut c = Conversation::new();
        let n = crate::report::NOTE_ROW_MAX_BYTES * 3;
        let r = c.reply(&format!("```js\nhistory.note(\"x\".repeat({n}));\n```\n"));
        let id = r.row().id;

        // What the model is shown: bounded, and it says how much is left.
        let shown = c.row_shown(id);
        assert!(
            shown.len() < n / 2,
            "the row is bounded: {} bytes for a {n}-byte value",
            shown.len()
        );
        assert!(
            shown.contains(&format!("history.fetch({})", id.as_u64())),
            "and names the id that has the rest: {shown}"
        );
        assert!(
            shown.contains(&format!("of {}", n + 2)),
            "and how much there is (+2 for the JSON quotes): {shown}"
        );
        // The JSON head survives, so the row still says what kind of
        // thing `fetch` will return.
        assert!(
            shown.contains("noted: \"x"),
            "a string still looks like a string: {shown}"
        );

        // And the value itself is untouched — the clip is a rendering.
        let back = c.reply(&format!(
            "```js\nhistory.note((await fetch_history({})).length);\n```\n",
            id.as_u64()
        ));
        assert_eq!(
            back.row().value,
            json!(n),
            "fetch hands back all {n} characters"
        );
    }

    /// **Writing over a suspended program supersedes it; it does not
    /// abandon it.** Nothing decided — the reply simply wrote
    /// something else — and the report used to tell the model a
    /// handler had made a decision no handler made.
    #[test]
    fn a_rewritten_frame_is_superseded_not_abandoned() {
        let mut c = Conversation::new();
        c.user("go");
        c.reply("```js\nconst v = null; v.x;\n```\n");
        // Neither resume nor abandon: just a different program.
        let r = c.reply("```js\ntell(\"took another route\");\nfinish();\n```\n");

        let how: Vec<String> = c
            .tree()
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Handback { how, .. } => Some(format!("{how:?}")),
                _ => None,
            })
            .collect();
        assert!(
            how.iter().any(|h| h.contains("Superseded")),
            "the parked frame is superseded: {how:?}"
        );
        assert!(
            !how.iter().any(|h| h.contains("Abandoned")),
            "nobody abandoned anything: {how:?}"
        );
        assert_eq!(r.tells, ["took another route"]);
    }

    /// **A spawn row says what it spawned.** `spawn(charter)` takes one
    /// argument and it is not a name, so `name` is `None` on every
    /// spawn in the kept corpus — 12 of 12 — and the row read
    /// `spawn(<unnamed>)`: the one call whose identity the model has to
    /// carry forward, rendered as the only row that says nothing about
    /// itself. A parent shown that spawned a second helper on its next
    /// reply.
    #[test]
    fn a_spawn_row_names_what_it_spawned() {
        let mut c = Conversation::new();
        c.user("get a helper to total the ledger");
        c.reply("```js\nconst a = spawn(\"Total notes/ledger.md and say the number.\");\n```\n");

        let spawn = c
            .tree()
            .events
            .values()
            .find(|e| matches!(&e.payload, EventPayload::Call(Call::Spawn { .. })))
            .expect("the spawn is on the log")
            .id;
        let row = c.row_shown(spawn);
        assert!(
            row.contains("Total notes/ledger.md"),
            "the charter identifies it: {row}"
        );
        assert!(!row.contains("<unnamed>"), "{row}");
    }

    /// **A branch parked mid-stream, whose next completion says
    /// nothing, is still listening.**
    ///
    /// The live stall of 2026-09-21. Generation 1 is still streaming
    /// when a post parks the run (rule B), so `streaming_epoch` still
    /// names it. The host supersedes it with generation 2, which spends
    /// its whole budget on reasoning and returns no text at all — so
    /// `notebook_stream` is never called for it (thinking does not
    /// reach the runner) and no new `Reply` is ever opened. Its
    /// completion then lands in `notebook_stream_end`, where
    /// `streaming_epoch.is_some()` routes it down the path that records
    /// it as **the end of the reply that parked**, not as a generation
    /// of its own.
    ///
    /// The branch is then `Suspended` with nothing in flight, and every
    /// wake is closed: `prompt_suspended` needs a fresh transition,
    /// `open_notebook_reply` needs a text chunk, `needs_prompt` refuses
    /// anything that is not `Idle`. Post #16 — "are you still there?" —
    /// drew no reply at all.
    /// **A rewrite over a running program leaves a terminal on the
    /// log.** `SessionCommand::Restart` — the TUI's rewrite gesture —
    /// reaches `take_turn` with no phase check on the path, and
    /// `open_notebook_reply` used to `std::mem::replace` the phase and
    /// handle only the `Suspended` case: a `Running` run was dropped
    /// with no handback, no status and no `last_vm`, so the log went on
    /// saying the program was running and its VM was simply gone.
    ///
    /// Found by the audit that produced the `Phase` split, in the one
    /// slot that still holds a `Run`.
    /// **The log and the live run agree about which program ended
    /// how.** They did not: `programs_for` attached a handback to the
    /// innermost open program while the event named the reply that was
    /// newest, and for a supersede those are different frames — so a
    /// reopened log put the `Superseded` on the superseding program and
    /// the `Completed` after it on the superseded one, exactly
    /// inverted.
    ///
    /// No fold could have been written to pass this on the old log: the
    /// event said nothing about which frame it was about.
    #[test]
    fn a_reopened_log_agrees_with_the_run_about_who_ended_how() {
        use crate::host::ProgramStatus;
        let mut c = Conversation::new();
        c.allow(Invariant::HandbackNamesItsProgram);
        c.user("go");
        // Parks on a raise.
        let raiser = c.reply("```js\nraise(\"x\");\n```\n").reply;
        assert_eq!(c.status(), "suspended");
        // A rewrite: it neither resumes nor abandons, so the parked
        // frame is superseded and this program completes.
        let rewrite = c.reply("```js\ntell(\"instead\"); finish();\n```\n").reply;

        let agent = c.runner().agent_id();
        let leaf = c.runner().spine.leaf_id;
        let progs = c.tree().programs_for(agent, leaf);
        let status = |id| {
            progs
                .iter()
                .find(|p| p.id == id)
                .unwrap_or_else(|| panic!("no program view for {id:?}"))
                .status()
        };
        assert_eq!(
            status(raiser),
            ProgramStatus::Failed,
            "the superseded frame is the one that was discarded"
        );
        assert_eq!(
            status(rewrite),
            ProgramStatus::Completed,
            "the program that wrote over it completed"
        );
    }

    #[test]
    fn a_rewrite_over_a_running_program_says_so_on_the_log() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "do it");
        let out = state
            .notebook_stream(&mut tree, 1, "```js\nawait tools.bash(\"slow\");\n```\n")
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(state.status(), "running", "the call is still in flight");

        let out = state
            .take_turn(&mut tree, "tell(\"instead\");".to_owned())
            .unwrap();
        drain(&mut state, &mut tree, out);

        assert!(
            tree.events.values().any(|e| matches!(
                &e.payload,
                EventPayload::Handback {
                    how: Handback::Superseded,
                    ..
                }
            )),
            "the displaced program went with no terminal on the log"
        );
    }

    /// **Typing does not interrupt a turn in flight.** A new message
    /// queues and is delivered when the generation lands; interrupting
    /// is the separate, explicit gesture (`x` →
    /// `SessionCommand::Interrupt`). An unparked branch has always
    /// worked that way — `needs_prompt` declines while a request is
    /// out — but a parked one read as `Idle`, took the unseen-post arm
    /// and asked for a *second* generation over the host's.
    ///
    /// The fix is `prompt_suspended` stamping the phase, which was
    /// impossible while `Phase` carried the parked run: `AwaitingLlm`
    /// would have dropped it.
    #[test]
    fn a_message_typed_during_a_parked_turn_queues_instead_of_superseding() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "run it");
        let out = state
            .notebook_stream(&mut tree, 1, "```js\nawait tools.bash(\"slow\");\n```\n")
            .unwrap();
        drain(&mut state, &mut tree, out);
        let out = user_post(&mut state, &mut tree, "actually stop");
        drain(&mut state, &mut tree, out);
        assert_eq!(state.status(), "suspended");

        // What `prompt_suspended` does: render the request, and say one
        // is out.
        let _ = state.render_request(&tree);
        state.await_llm();

        let out = user_post(&mut state, &mut tree, "and what phase did it reach?");
        assert!(
            !out.iter().any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "typing superseded the turn in flight instead of queueing"
        );
        assert_eq!(state.status(), "suspended", "and the frame is untouched");
    }

    /// **One reply with three cells is one program.** The tail line
    /// exists to say whether a branch is going anywhere — `sweep-40`
    /// spent nine programs and fifty minutes without converging — and
    /// it counted `Part::Cell`, so the shape the card asks for (ask for
    /// everything you can already name, in one completion) reported the
    /// highest number. A `deepseek-v4-flash` smoke run on 2026-09-21
    /// did a whole three-file rename in one reply and was told it had
    /// run three programs.
    /// The rehearsal ban rides every request now, and can be turned
    /// off. It says the fact rather than scolding: reasoning is not
    /// read back, so a drafted block is written twice.
    #[test]
    fn the_rehearsal_ban_rides_every_request_and_can_be_turned_off() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "do it");
        let on = state.request_tail(&tree).unwrap_or_default();
        assert!(on.contains("One-shot"), "off by default: {on}");

        state.no_rehearsal_tail = None;
        let off = state.request_tail(&tree).unwrap_or_default();
        assert!(!off.contains("One-shot"), "could not be turned off: {off}");

        state.no_rehearsal_tail = Some(NO_REHEARSAL_TAIL.to_owned());
        let on = state.request_tail(&tree).unwrap_or_default();
        assert!(
            on.contains("Never draft code blocks"),
            "the arm does not reach the tail: {on}"
        );
        assert!(
            on.lines()
                .any(|l| l.starts_with("- ") && l.contains("One-shot")),
            "one line like its neighbours: {on}"
        );
    }

    /// **A child can address whoever spawned it.**
    ///
    /// `spawn()` hands the caller a handle to the child and nothing
    /// hands the child one back, so before `"parent"` a child with a
    /// question had one address — `"user"` — which reaches the person
    /// driving the session and, in a headless run, nobody. Everything
    /// after the address already worked; the address was the gap.
    #[test]
    fn a_child_can_ask_its_parent_and_the_root_cannot() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");

        // The root's parent is nobody, and it says so rather than
        // quietly meaning the user.
        let err = state
            .resolve_address(&tree, Some(&json!("parent")))
            .unwrap_err();
        assert!(err.contains("no parent"), "{err}");
        assert!(err.contains("user"), "and says who it does answer to: {err}");

        // `"user"` still means the person, from anywhere.
        assert!(matches!(
            state.resolve_address(&tree, Some(&json!("user"))),
            Ok(Address::User)
        ));
    }

    /// **The two shape lines follow the transport.**
    ///
    /// They are the lines whose violation is silent — a reply that
    /// meant to act and produced nothing runs nothing, rests the
    /// branch, and reports success — so under `run_program` they have
    /// to name the call rather than the fence. Asserted both ways
    /// because the failure is invisible: a request telling the model to
    /// write a ```js block when only a tool call runs costs a whole
    /// turn and looks like the model's fault.
    #[test]
    fn the_shape_lines_name_whichever_transport_is_running() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "do it");
        let fenced = state.request_tail(&tree).unwrap_or_default();
        assert!(fenced.contains("Only a ```js block runs"), "{fenced}");
        assert!(!fenced.contains("run_program"), "{fenced}");

        state.run_program = true;
        let called = state.request_tail(&tree).unwrap_or_default();
        assert!(called.contains("Only a run_program call runs"), "{called}");
        assert!(
            !called.contains("Only a ```js block runs"),
            "both rules at once: {called}"
        );
    }

    /// **Position is its own arm, and it costs something.**
    ///
    /// The default puts the ban fifth of nine, under the attachment
    /// status; `AGENT2_NO_REHEARSAL_LAST` puts it below
    /// [`REPLY_IS_MARKDOWN`], the only slot nothing else can reach.
    /// Both facts are asserted because the second is the price of the
    /// first: the rule whose violation is silent gets demoted, and a
    /// reorder that quietly dropped one of the two lines would read as
    /// a win.
    #[test]
    fn the_rehearsal_ban_can_be_moved_below_the_markdown_rule() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "do it");
        state.no_rehearsal_tail = Some(NO_REHEARSAL_TAIL.to_owned());

        let last_of = |t: &str| t.lines().last().unwrap_or_default().to_owned();
        let end = state.request_tail(&tree).unwrap_or_default();
        assert!(
            last_of(&end).contains("One-shot"),
            "by default the ban has the last slot: {end}"
        );

        state.no_rehearsal_last = false;
        let mid = state.request_tail(&tree).unwrap_or_default();
        assert!(
            last_of(&mid).contains("Only a ```js block runs"),
            "turned off, the markdown rule takes it back: {mid}"
        );
        assert!(
            end.contains("Only a ```js block runs"),
            "and the rule it displaced is still in the tail: {end}"
        );
        assert_eq!(
            mid.lines().count(),
            end.lines().count(),
            "a reorder, not an addition or a loss"
        );
    }

    /// **The tail says how full the conversation is, once that is a
    /// number worth knowing.**
    ///
    /// The card says a row is cheap to drop while it is recent and
    /// costs the whole conversation once it is old, and then leaves the
    /// model with no way to tell where it stands — so it drops nothing
    /// until the harness stops it and demands a compaction program, by
    /// which time every cheap removal has become a dear one.
    #[test]
    fn the_tail_says_how_full_it_is_only_once_it_is_half_full() {
        let mut c = Conversation::new();
        c.user("go");
        c.reply("Starting.\n");
        let early = c.runner().request_tail(c.tree()).unwrap_or_default();
        assert!(
            !early.contains("% full"),
            "an empty conversation is not news: {early}"
        );

        // Enough rows to pass the mark. Each is under the row bound, so
        // all of them render and the document really is that big.
        for i in 0..24 {
            c.reply(&format!(
                "```js\nhistory.note(\"{}\");\n```\n",
                format_args!("{i}{}", "z".repeat(1200))
            ));
        }
        let full = c.runner().request_tail(c.tree()).unwrap_or_default();
        assert!(full.contains("% full"), "{full}");
        // **The advice is prospective, because that is the only verb
        // the card gives it.** It named `history.remove(id)` while the
        // card still declared that; the card does not, since not
        // keeping a thing costs nothing and undoing it costs a rewrite
        // of every turn after the row.
        assert!(
            full.contains("peek what you only need once"),
            "it names the verb the card actually teaches: {full}"
        );
        assert!(!full.contains("remove"), "{full}");
    }

    #[test]
    fn the_tail_counts_programs_not_blocks() {
        let mut c = Conversation::new();
        c.answers("bash", serde_json::json!({ "status": 0, "stdout": "ok\n" }));
        c.user("rename it everywhere");
        c.reply(
            "Finding them.\n\n```js\nconst a = await tools.bash(\"grep -rl OLD .\");\n```\n\
             \nNow the edits.\n\n```js\nconst b = await tools.bash(\"true\");\n```\n\
             \nAnd the check.\n\n```js\ntell(\"done\"); finish();\n```\n",
        );
        let tail = c.runner().request_tail(c.tree()).unwrap_or_default();
        assert!(
            tail.contains("1 program run since"),
            "three cells of one reply are one program: {tail}"
        );
    }

    #[test]
    fn a_branch_parked_mid_stream_survives_a_silent_completion() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "document every file");

        // Generation 1 arrives as a stream and is *not* ended: it is
        // still in flight, exactly as it was live.
        let out = state
            .notebook_stream(
                &mut tree,
                1,
                "```js\nawait tools.read_file(\"a.py\");\n```\n",
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        // The person speaks; rule B parks the run at its next slice.
        let out = user_post(&mut state, &mut tree, "actually stop - just do a.py");
        drain(&mut state, &mut tree, out);
        assert_eq!(state.status(), "suspended", "rule B parks it");

        // Generation 2: all reasoning, no text, cut off at the cap.
        let mut turn = crate::host::scripted_program("");
        turn.source = String::new();
        turn.thinking = Some("thinking at length and saying nothing".to_owned());
        turn.truncated = true;
        let out = state.step(&mut tree, StepInput::LlmResponse(turn)).unwrap();
        drain(&mut state, &mut tree, out);

        // The person speaks again. This has to reach it.
        let out = user_post(&mut state, &mut tree, "are you still there?");
        assert!(
            out.iter().any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "the post drew no request — the branch is deaf (status {})",
            state.status()
        );
    }

    /// **A parked branch that is spoken to is still listening** — the
    /// contract, pinned.
    ///
    /// A suspension gets exactly one prompt, sent when it parks
    /// (`host::prompt_suspended`, whose own comment says "without it …
    /// the branch sits `Suspended` forever, since nothing else ever
    /// asks it for a decision"), so what happens to *later* posts is
    /// worth a test of its own.
    ///
    /// **It does not reproduce the stall seen live on 2026-09-21**, and
    /// is not claimed to: a run interrupted mid-task parked correctly,
    /// spent its one prompt on a reply truncated inside 28 KB of
    /// reasoning, went quiet, and then ignored "are you still there?"
    /// entirely. This sequence — park, cell-less truncated reply, post
    /// — recovers here. The live log is kept; the cause is open.
    #[test]
    fn a_parked_branch_is_still_listening() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "document every file");
        // A program that parks on a call it never gets an answer to.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("await tools.read_file(\"a.py\");")),
            )
            .unwrap();
        let _ = out;

        // Speaking to a running program parks it at its next fuel
        // slice (rule B) — the slice `deliver` asks for.
        let out = user_post(&mut state, &mut tree, "actually stop - just do a.py");
        drain(&mut state, &mut tree, out);
        assert_eq!(state.status(), "suspended", "rule B parks it");

        // Its one prompt is spent on a reply with no cell at all.
        let mut turn = crate::host::scripted_program("");
        turn.source = "Let me think about which one.\n".to_owned();
        turn.truncated = true;
        let out = state.step(&mut tree, StepInput::LlmResponse(turn)).unwrap();
        let _ = out;

        // The person speaks again. This has to reach it.
        let before = tree.events.len();
        let out = user_post(&mut state, &mut tree, "are you still there?");
        assert!(
            out.iter().any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "the post drew no request — the branch is deaf (status {}, {} events)",
            state.status(),
            tree.events.len() - before
        );
    }

    /// **A program with the fence left off is caught, and prose is
    /// not.** The reply `delegate-direct` actually produced twice on
    /// 2026-09-20 was two statements and nothing else; it ran nothing,
    /// went to the person as their answer, and rested the branch
    /// reporting success.
    #[test]
    fn a_program_without_its_fence_is_noticed() {
        let mut c = Conversation::new();
        c.user("what is the closing balance?");
        let r = c.reply("tell(\"The closing balance is 1200.\");\nfinish();\n");

        assert!(r.cells.is_empty(), "nothing was fenced, so nothing ran");
        assert!(
            c.runner()
                .request_tail(c.tree())
                .is_some_and(|t| !t.contains("program with no fence")),
            "the notice is a post, not a tail line"
        );
        assert!(
            c.tree().events.values().any(|e| matches!(
                &e.payload,
                EventPayload::Post { origin, .. }
                    if format!("{origin:?}").contains("no fence around it")
            )),
            "the branch is woken and told why"
        );
    }

    /// And ordinary prose that merely mentions a verb is left alone —
    /// the cost of a false positive is a wasted turn, so the bar is
    /// every line, not any line.
    #[test]
    fn prose_that_mentions_a_verb_is_not_mistaken_for_a_program() {
        for text in [
            "I will tell(…) you once the check passes.",
            "Reading it first.\n\nThen I will summarise.",
            "```js\ntell(\"hi\");\n```\n",
        ] {
            assert!(
                !unfenced_program(text),
                "{text:?} should not read as a program"
            );
        }
    }

    /// **A `return` skips the rest of the reply, blocks and prose
    /// alike.**
    ///
    /// The cells share one scope and one frame, so returning from that
    /// frame ends the reply — and that is the point: a program that has
    /// found out it cannot finish must not go on to write the file it
    /// was about to write. `finish()` is the opposite kind of thing, a
    /// flag that lets everything after it run.
    #[test]
    fn returning_skips_every_block_after_it() {
        let mut c = Conversation::new();
        let r = c.reply(
            "Checking first.\n\n```js\nhistory.note(\"before\");\nreturn \"the check disagrees\";\n```\n\nAnd now the part that must not happen.\n\n```js\nhistory.note(\"after\");\n```\n",
        );

        assert_eq!(
            r.values(),
            [&json!("before")],
            "the block after the return must not run"
        );
        // The prose after it is not sent either: it was written on the
        // assumption the work carried on, and it did not.
        assert_eq!(
            r.prose,
            ["Checking first."],
            "prose before the return reaches the person, prose after it does not"
        );
        // And what it returned is handed back, not lost: that value is
        // the whole of what a `return` says to the next reply.
        assert_eq!(
            r.ended,
            Ending::Completed(Some(json!("the check disagrees")))
        );
    }

    /// **Paging moves one row's window; it does not add rows.**
    ///
    /// Appending each page would put every window in the document at
    /// once, which is the cost the bound exists to avoid. `replace`
    /// shadows the row's rendering, and `fetch` still returns the
    /// original whole — so the bytes to cut the next window from are
    /// always in reach without re-reading anything.
    #[test]
    fn paging_a_row_moves_its_window_and_leaves_the_value_whole() {
        let mut c = Conversation::new();
        let n = crate::report::NOTE_ROW_MAX_BYTES * 2;
        let r = c.reply(&format!(
            "```js\nhistory.note(\"a\".repeat({n}) + \"TAIL\");\n```\n"
        ));
        let id = r.row().id;

        // Move the window to the end of the value, from the value itself.
        c.reply(&format!(
            "```js\nconst all = await fetch_history({0});\n\
             await replace_history({0}, all.slice(all.length - 12));\n```\n",
            id.as_u64()
        ));

        assert_eq!(c.rows_named(id), 1, "one row, not two");
        let shown = c.row_shown(id);
        assert!(shown.contains("TAIL"), "the window moved: {shown}");
        assert!(shown.len() < 100, "and it is small: {} bytes", shown.len());

        // And the value behind it is still all of it.
        let back = c.reply(&format!(
            "```js\nhistory.note((await fetch_history({})).length);\n```\n",
            id.as_u64()
        ));
        assert_eq!(
            back.row().value,
            json!(n + 4),
            "fetch still hands back the whole value"
        );
    }

    /// A row under the bound renders exactly as it always did — which
    /// is 90% of them: appended rows run to a median of 450 bytes.
    #[test]
    fn a_short_appended_row_is_untouched_by_the_bound() {
        let mut c = Conversation::new();
        let r = c.reply("```js\nhistory.note({ dead: 3 });\n```\n");
        // What the model reads, verbatim: the row's id and its value.
        assert_eq!(
            c.row_shown(r.row().id),
            format!("- `[{}]` noted: {{\"dead\":3}}", r.row().id.as_u64())
        );
    }

    /// **And the row says which it will be.** A note is shown whole,
    /// so its rendering is the model's only evidence of what
    /// `history.fetch` returns — and a string rendered bare made
    /// `append("{\"a\":1}")` and `append({a:1})` identical on the page
    /// and different in the hand (`note_display`).
    #[test]
    fn an_appended_row_renders_as_the_json_it_will_hand_back() {
        for (program, row) in [
            (
                "history.note({ kept: 3, dead: [\"a\"] });",
                r#"noted: {"kept":3,"dead":["a"]}"#,
            ),
            (
                "history.note(\"a conclusion\");",
                r#"noted: "a conclusion""#,
            ),
            // **The guess this exists to remove.** An object and a
            // string that happens to contain JSON rendered identically
            // while a string was shown bare — same row, different
            // things in the hand, and nothing to tell them apart.
            (
                "history.note(JSON.stringify({ a: 1 }));",
                r#"noted: "{\"a\":1}""#,
            ),
        ] {
            let (mut tree, mut state) = setup();
            state.kickoff(&mut tree).unwrap();
            let out = state
                .step(&mut tree, StepInput::LlmResponse(llm_program(program)))
                .unwrap();
            drain(&mut state, &mut tree, out);
            let doc = crate::document::render(&tree, &state.spine, 64 * 1024);
            let text: String = doc.messages.iter().map(|m| m.content.as_str()).collect();
            assert!(text.contains(row), "wanted {row:?} in:\n{text}");
        }
    }

    /// And the two are not the same row. This is the whole point: the
    /// rendering is the model's only evidence of what `fetch` returns,
    /// so an object and its serialisation must not look alike.
    #[test]
    fn an_object_and_its_serialisation_render_differently() {
        let row = |program: &str| {
            let (mut tree, mut state) = setup();
            state.kickoff(&mut tree).unwrap();
            let out = state
                .step(&mut tree, StepInput::LlmResponse(llm_program(program)))
                .unwrap();
            drain(&mut state, &mut tree, out);
            let doc = crate::document::render(&tree, &state.spine, 64 * 1024);
            let text: String = doc.messages.iter().map(|m| m.content.as_str()).collect();
            let at = text.find("noted: ").expect("a note row");
            text[at..].lines().next().unwrap().to_owned()
        };
        let object = row("history.note({ a: 1 });");
        let string = row("history.note(JSON.stringify({ a: 1 }));");
        assert_ne!(object, string, "the guess is back");
        assert_eq!(object, r#"noted: {"a":1}"#);
        assert_eq!(string, r#"noted: "{\"a\":1}""#);
    }

    /// **A resumed run does not replay what it already printed.**
    /// `console_lines` is never cleared, so a `raise` logged its
    /// console and the terminal after the resume logged the same
    /// entries again — and the second report's `### it printed` showed
    /// the model output it had read a reply earlier.
    #[test]
    fn a_resumed_run_logs_only_what_it_printed_since() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply(
            "```js\nconsole.log(\"before\");\nconst v = raise(\"which\");\n```\n\n\
             ```js\nconsole.log(\"after: \" + v);\n```\n",
        );
        let after = c.resume(json!("that one"));
        assert_eq!(r.printed, ["before"]);
        assert_eq!(
            after.printed,
            ["after: that one"],
            "each handback logs its own output, not the run's whole history"
        );
    }

    /// **The trigger is the provider's own count, not a conversion.**
    /// A document well under the byte budget can still be over the
    /// context window, and only `usage.prompt` knows which.
    #[test]
    fn compaction_fires_on_the_counted_prompt_not_the_estimate() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        // **Held for the whole test, because the process is shared.**
        // `cargo test` runs these on threads of one process, so a
        // window set here is set for every test running alongside. It
        // used to be harmless — an unexpected window fell through to
        // the byte path, which is what those tests wanted anyway — and
        // stopped being harmless the day `fullness` started answering
        // `None` to "a window, and nothing counted against it". Then
        // `compaction_does_not_fire_while_already_compacting` began
        // failing about one run in three, on a variable it never
        // mentions.
        let _env = env_lock();
        unsafe {
            std::env::set_var("AGENT2_CONTEXT_TOKENS", "10000");
            std::env::set_var("AGENT2_COMPLETION_RESERVE", "2000");
        }
        // usable = 8000, headroom 0.25 -> fires at 6000 counted tokens.
        let fires = |state: &mut Runner, tokens: u64, tree: &mut Tree| {
            state.compaction_requested = false;
            state.next_prompt_floor = Counted::Floor(tokens);
            state
                .compaction_if_needed(tree, 64 * 1024, 0.25)
                .unwrap()
                .is_some()
        };
        assert!(
            !fires(&mut state, 5_000, &mut tree),
            "under the window: nothing to do"
        );
        assert!(
            fires(&mut state, 7_000, &mut tree),
            "a small document can still be over the window; only the count knows"
        );
        // And the reverse, which is the whole point of having one
        // trigger: a byte budget this document is hugely over cannot
        // fire anything while the count says there is room. Running
        // both would mean the byte budget decides every time, because
        // it is always the tighter of the two.
        state.compaction_requested = false;
        state.next_prompt_floor = Counted::Floor(5_000);
        assert!(
            state
                .compaction_if_needed(&mut tree, 1, 0.25)
                .unwrap()
                .is_none(),
            "a counted prompt with room to spare overrides any byte budget"
        );
        // With no count to go on there is nothing to override it with,
        // so the byte budget is the trigger again.
        // That the byte path is what a missing count falls back to is
        // asserted where there is something compactable to fall back
        // *to* — see `compaction_gives_up_rather_than_looping_when_it_
        // cannot_help`, whose fixture has rows. This document is almost
        // all card, so no byte budget both fires and clears the floor,
        // which is the floor guard doing its job.
        // But a count that a commit has just invalidated is not the
        // same as never having had one, and the byte budget must not
        // step in for it — see `Counted::Stale`.
        state.next_prompt_floor = Counted::Stale;
        assert!(
            state
                .compaction_if_needed(&mut tree, 1, 0.25)
                .unwrap()
                .is_none(),
            "a stale count waits for a fresh one rather than handing the decision to bytes"
        );
        unsafe {
            std::env::remove_var("AGENT2_CONTEXT_TOKENS");
            std::env::remove_var("AGENT2_COMPLETION_RESERVE");
        }
    }

    /// **A handback fetches as structure, not as Rust.** It used to
    /// come back `format!("{how:?}")` — `Trapped { kind: "TypeError",
    /// … }`, which a program can only substring-match — while the log
    /// stored the same thing as proper JSON all along. `value_json` in
    /// this file has a paragraph on why a debug rendering must never
    /// reach a program; this was the same mistake one function over.
    #[test]
    fn a_handback_fetches_as_structure_not_as_rust() {
        let mut c = Conversation::new();
        let r = c.reply("```js\nconst v = null; v.x;\n```\n");

        let fetched = c.fetch(r.handback.expect("the trap"));
        // Indexable: a handler can branch on `resumable` without
        // parsing a sentence.
        assert_eq!(fetched["Trapped"]["kind"], json!("TypeError"));
        assert_eq!(fetched["Trapped"]["resumable"], json!(true));
        assert!(
            fetched["Trapped"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("cannot read property")),
            "{fetched}"
        );
    }

    /// And a terminal one is still just the fact that it happened —
    /// **which terminal** being the fact, now that `finish` has one of
    /// its own and a program running off its end has another.
    #[test]
    fn a_completed_handback_fetches_as_a_bare_name() {
        let mut c = Conversation::new();
        let r = c.reply("```js\ntell(\"ok\"); finish();\n```\n");
        assert_eq!(
            c.fetch(r.handback.expect("the handback")),
            json!({ "Completed": { "rested": true } })
        );

        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply("```js\ntell(\"one step done\");\n```\n");
        assert_eq!(
            c.fetch(r.handback.expect("the handback")),
            json!({ "Completed": {} }),
            "a program that ran off its end handed on; it did not finish"
        );
    }

    /// A `Note` and a program's own source come back too — the three
    /// row kinds 27.4 added, one test each would be three copies of the
    /// same walk.
    #[test]
    fn fetch_history_reads_a_note_and_a_program_back() {
        let mut c = Conversation::new();
        let src = "```js\nawait note_history(\"the parser drops the last field\");\n\
                   history.note(1);\n```\n";
        let first = c.reply(src);

        let back = c.reply(&format!(
            "```js\nhistory.note([await fetch_history({}), await fetch_history({})]);\n```\n",
            first.rows[0].id.as_u64(),
            first.reply.as_u64()
        ));
        let returned = &back.row().value;
        assert_eq!(returned[0], json!("the parser drops the last field"));
        // A reply comes back as the markdown the model wrote — fences
        // and all, because that is the row (28).
        assert_eq!(returned[1], json!(src));
    }

    /// **Nothing a program can write may take the agent down.**
    ///
    /// Every entry point here is called with whatever the model's
    /// arithmetic produced, which is not the same set as what the card
    /// describes. Two panics reached live runs before anyone swept for
    /// them — `history.fetch(0)` against a `NonZeroU64`, and call
    /// offsets outliving the text compaction shortened — so the sweep
    /// is worth having standing rather than rediscovering from an
    /// `exit=101` in a verdict file.
    ///
    /// The bar is only "no panic". A refusal is a fine answer, and so
    /// is doing the thing; what is not fine is the process ending
    /// mid-task, because everything the run had done is then
    /// unreachable behind a corpse.
    #[test]
    fn no_argument_a_program_can_write_panics_the_agent() {
        // Ids that are not ids, arguments of the wrong shape, arity
        // that does not match, and the values JS arithmetic reaches
        // when something upstream went wrong.
        let hostile = [
            "await fetch_history(0);",
            "await fetch_history(-1);",
            "await fetch_history(1e400);",
            "await fetch_history(0.5);",
            "await fetch_history(\"7\");",
            "await fetch_history(null);",
            "await fetch_history(undefined);",
            "await fetch_history();",
            "await fetch_history([1, 2]);",
            "await fetch_history({});",
            "await fetch_history(NaN);",
            "await remove_history(0);",
            "await remove_history(0, 0);",
            "await remove_history(9, 2);",
            "await remove_history(undefined);",
            "await replace_history(0, \"x\");",
            "await replace_history(1);",
            "await replace_history(undefined, undefined);",
            "history.note(undefined);",
            "history.note();",
            "await tell();",
            "await tell(undefined);",
            "await tell(null);",
            "await ask(0, \"q\");",
            "await ask(\"#0\", \"q\");",
            "await ask(\"\", \"q\");",
            "await ask(undefined, undefined);",
            "await choose(\"user\", \"q\", []);",
            "await choose(\"user\", \"q\", undefined);",
            "await choose(0, 0, 0);",
            "await list_agents({ under: 0 });",
            "await list_agents({ under: -1 });",
            "await list_agents(undefined);",
            "await spawn();",
            "await spawn(undefined, undefined);",
            "await fork();",
            "await fork(0);",
            "await answer(0, \"x\");",
            "await answer(undefined, undefined);",
        ];
        for src in hostile {
            let mut c = Conversation::new();
            // An `ask` left open is an ordinary outcome here, not a
            // stalled harness — the point is only that the process is
            // still alive to report whatever happened.
            c.allow(Invariant::CallsSettle);
            // Wrapped, because a rejected promise is an ordinary
            // outcome here too.
            let r = c.reply(&format!(
                "```js\ntry {{ {src} }} catch (e) {{ /* fine */ }}\n```\n"
            ));
            // **And it has to have run.** A case that does not compile
            // exercises the parser and nothing else, which is how a
            // sweep like this quietly stops testing what it names.
            assert!(
                !matches!(r.ended, Ending::CellFailed(_)),
                "`{src}` never compiled, so it tested nothing"
            );
        }
    }

    /// **Zero is a number a program can arrive at, and never an id.**
    ///
    /// `EventId` is a `NonZeroU64`. Every id-taking entry point filters
    /// `> 0` before converting — except the re-attach check, which ran
    /// first and built one straight from the argument, so
    /// `history.fetch(0)` panicked the agent out of the run: `expected
    /// non-zero EventId!`, `exit=101`, twice in 294 kept runs. It must
    /// come back as an answer the program can read.
    #[test]
    fn fetching_row_zero_is_an_error_not_a_crash() {
        let mut c = Conversation::new();
        let r = c.reply(
            "```js\ntry { await fetch_history(0); history.note(\"no throw\"); }\n\
             catch (e) { history.note(String(e.message || e)); }\n```\n",
        );
        let said = r.row().value.as_str().unwrap_or_default();
        assert!(
            said.contains("#0"),
            "the id it asked for is named back to it: {said}"
        );
        assert!(said != "no throw", "and it is an error, not a silent pass");
    }

    /// **A call whose arguments cannot be represented does not happen.**
    /// `undefined` used to reach the tool as the *string* `"Undefined"`
    /// — the Rust debug rendering, via a lossy fallback — and on
    /// 2026-09-17 a live run wrote a Python file whose entire first
    /// line was that word. The program could not see it; Python said
    /// `NameError: name 'Undefined' is not defined` several steps
    /// later, and the run failed for a reason unrelated to what it got
    /// wrong.
    #[test]
    fn a_call_with_an_unrepresentable_argument_is_refused_not_stringified() {
        let mut c = Conversation::new();
        // `parsed.missing` is undefined; the write must not happen.
        let r = c.reply(
            "```js\nconst parsed = {};\n\
             try { await tools.write_file(\"out.py\", parsed.missing); }\n\
             catch (e) { tell(`refused: ${e}`); }\n\
             tell(\"ok\"); finish();\n```\n",
        );
        assert!(
            r.calls.is_empty(),
            "the write was issued anyway: {:?}",
            r.calls
        );
        assert!(
            r.tells
                .iter()
                .any(|t| t.contains("argument 2 is `undefined`")),
            "the program is told which argument, and that nothing ran: {:?}",
            r.tells
        );
    }

    #[test]
    fn answer_dispatches_from_inside_a_program() {
        let mut c = Conversation::new();
        c.user("which one?");
        let question = c.open()[0];

        let r = c.reply(&format!(
            "```js\nawait answer({}, \"q\", \"the second\");\nhistory.note(1);\n```\n",
            question.as_u64()
        ));
        assert_eq!(r.answered, [(question, json!("the second"))]);
    }

    #[test]
    fn answer_on_an_unowned_pre_fork_post_is_rejected_in_program() {
        let mut tree = Tree::new(None);
        let mut original = Runner::new_root(&mut tree, "root", "").unwrap();
        user_post(&mut original, &mut tree, "which one?");
        let question = original.open()[0];

        let mut spine = tree.fork(original.spine.leaf_id).unwrap();
        // `tree.fork` only anchors a `Spine` at the divergence point — it
        // does not itself log anything (`Tree::fork`'s own doc: callers
        // append the actual `Fork` event, `host/mod.rs`'s `cmd_fork` and
        // `create_fork` both do). Without it, obligations *would* cross,
        // because nothing ever cleared `open` — so appending it here is
        // the fix, not a workaround.
        let fork_root = tree
            .append(&mut spine, EventPayload::Fork { name: None })
            .unwrap();
        let mut fork = Runner::with_spine(&tree, tree.spine_at(fork_root));
        assert!(fork.open().is_empty(), "obligations do not cross a Fork");
        fork.kickoff(&mut tree).unwrap();

        let src = format!(
            "try {{ await answer({}, \"q\", 1); history.note(\"unreachable\"); }} catch (e) {{ history.note(\
             \"caught: \" + e); }}",
            question.as_u64()
        );
        let out = fork
            .step(&mut tree, StepInput::LlmResponse(llm_program(&src)))
            .unwrap();
        drain(&mut fork, &mut tree, out);
        let report = last_report(&fork, &tree);
        assert!(report.contains("inherited it as history"), "{report}");
    }

    // ── fan-out / resolution order (unchanged substance) ────────────

    #[test]
    fn fanout_batch_and_resolution_order() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"
            const a = tools.fetch("x");
            const b = tools.fetch("y");
            history.note([await a, await b]);
        "#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let calls = expect_tool_calls(&settled);
        assert_eq!(calls.len(), 2, "one fan-out batch");

        let (xa, yb) = (calls[0].call, calls[1].call);
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: yb,
                    result: Ok(json!("Y")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: xa,
                    result: Ok(json!("X")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        assert!(last_report(&state, &tree).contains(r#"["X","Y"]"#));
    }

    #[test]
    fn a_failed_call_settles_with_its_reason() {
        let mut c = Conversation::new();
        c.rejects("fetch", "host is down");
        let r = c.reply(
            "```js\ntry { history.note(await tools.fetch(\"a\")); }\n\
             catch (e) { history.note(\"caught: \" + e); }\n```\n",
        );

        assert!(
            matches!(&r.settled[0].1, Outcome::Failed(m) if m == "host is down"),
            "{:?}",
            r.settled
        );
        assert!(
            r.row()
                .value
                .as_str()
                .is_some_and(|t| t.contains("host is down")),
            "and the reason reaches the program that catches it: {:?}",
            r.row().value
        );
    }

    #[test]
    fn hot_loop_yields_per_tick() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("while (true) {}")),
            )
            .unwrap();
        assert!(
            out.iter().any(|o| matches!(o, StepOutput::Working)),
            "a hot loop yields rather than running to the end: {out:?}"
        );
        for _ in 0..3 {
            let out = state
                .step(&mut tree, StepInput::Tick { fuel: 10_000 })
                .unwrap();
            assert!(
                matches!(&out[..], [StepOutput::Working]),
                "a hot loop keeps yielding, never blocks"
            );
        }
    }

    // ── input binding (unchanged substance) ─────────────────────────

    #[test]
    fn large_input_previews_in_context_and_binds_whole() {
        let mut tree = Tree::new(None);
        let mut root = Runner::new_root(&mut tree, "root", "").unwrap();
        let big = "z".repeat(9_000);
        let (mut child, _out) = spawn_and_ask(
            &mut tree,
            &mut root,
            "summarize it",
            json!({ "body": big.clone(), "path": "PLAN.md" }),
        );
        let out = child
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("history.note(input.body.length);")),
            )
            .unwrap();
        drain(&mut child, &mut tree, out);
        assert!(last_report(&child, &tree).contains("9000"));
    }

    /// `Context::input`/`open` bookkeeping only — deliberately not driven
    /// through `step`/`StepInput::LlmResponse`. A scripted `answer(...)`
    /// program racing a second `deliver` would run straight into rule B
    /// (`on_tick`'s "a post logged since the last render suspends the
    /// run here"): `finish_program` unconditionally re-arms a fresh
    /// `LlmRequest` after every completion (the trigger rule's own
    /// crash-recovery clause — "the newest Turn's run has an outcome
    /// that has not been shown yet" — fires for a run's *own* outcome
    /// just as much as for a crash), so the branch never actually
    /// returns to `Phase::Idle` on its own and a second `deliver` cannot
    /// count on a fresh render to pick it up. None of that is what this
    /// test is about — it is about `open`'s ordering and `input()`
    /// reading its head — so it appends the `Answer` directly, the same
    /// event `dispatch_calls`'s `TOOL_ANSWER` arm would log.
    #[test]
    fn input_moves_to_the_next_open_post_as_each_is_answered() {
        let (mut tree, mut state) = setup();
        let (first, _) = state
            .deliver(
                &mut tree,
                Author::User,
                Origin::Direct {
                    text: "one".into(),
                    input: json!({ "n": 1 }),
                    options: Vec::new(),
                    expects_reply: true,
                },
            )
            .unwrap();
        let (second, _) = state
            .deliver(
                &mut tree,
                Author::User,
                Origin::Direct {
                    text: "two".into(),
                    input: json!({ "n": 2 }),
                    options: Vec::new(),
                    expects_reply: true,
                },
            )
            .unwrap();
        assert_eq!(state.spine.context().input(&tree), json!({ "n": 1 }));

        tree.append(
            &mut state.spine,
            EventPayload::Answer {
                question: first,
                value: json!("ok"),
            },
        )
        .unwrap();
        assert_eq!(state.spine.context().input(&tree), json!({ "n": 2 }));

        tree.append(
            &mut state.spine,
            EventPayload::Answer {
                question: second,
                value: json!("ok"),
            },
        )
        .unwrap();
        assert_eq!(state.spine.context().input(&tree), serde_json::Value::Null);
    }

    // ── trigger rule ─────────────────────────────────────────────────

    #[test]
    fn needs_prompt_iff_unseen_post_and_no_vm() {
        let (mut tree, mut state) = setup();
        assert!(!state.needs_prompt(&tree), "nothing has happened yet");
        state.kickoff(&mut tree).unwrap();
        assert!(!state.needs_prompt(&tree), "a request is already out");
    }

    #[test]
    fn fork_is_born_idle() {
        let mut tree = Tree::new(None);
        let mut original = Runner::new_root(&mut tree, "root", "").unwrap();
        user_post(&mut original, &mut tree, "hello");
        let mut spine = tree.fork(original.spine.leaf_id).unwrap();
        let fork_root = spine.leaf_id;
        let _ = &mut spine;
        let fork = Runner::with_spine(&tree, tree.spine_at(fork_root));
        assert!(
            !fork.needs_prompt(&tree),
            "a fork speaks only when spoken to"
        );
    }
    // ── compaction ──────────────────────────────────────────────────

    /// A branch with twenty real posts on it, and the budget that makes
    /// it over-large.
    ///
    /// The budget is returned rather than passed in because an empty
    /// branch is *already* about thirty kilobytes — the worked examples
    /// open every document — so "add rows until it exceeds N" only
    /// terminates for an N larger than the floor. Sizing the budget to
    /// the branch instead of the branch to the budget avoids an
    /// infinite loop that cost a test run to find.
    /// A document over budget, with a budget compaction can actually
    /// reach.
    ///
    /// **The filler has to outweigh the preamble**, and for a long time
    /// it did by accident. The budget here is half the rendered
    /// document, and compaction can only shrink the *conversation* —
    /// the card and its worked examples are fixed. Once the preamble is
    /// more than half, no program can get under budget however good it
    /// is, and every test built on this fixture quietly stops measuring
    /// what it names: `fires_over_budget` sees a give-up instead of a
    /// request, and `gives_up_rather_than_looping` passes for the wrong
    /// reason. Two exemplars added on 2026-09-22 crossed that line, and
    /// twenty posts of filler was simply the number that happened to be
    /// enough before.
    ///
    /// So it is measured rather than guessed: fill until the whole is
    /// three times the preamble, which leaves half the document
    /// reachable with room to spare.
    /// Serialises the tests that set process-wide environment
    /// variables against the tests that read them. Poisoning is not
    /// interesting here — a panicking test has already failed — so the
    /// guard is taken through the poison.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn crowded() -> (Tree, Runner, usize) {
        let (mut tree, mut state) = setup();
        let rendered = |tree: &Tree, state: &Runner| {
            crate::compaction::rendered_size(&crate::document::render(
                tree,
                &state.spine,
                64 * 1024,
            ))
        };
        let preamble = rendered(&tree, &state);
        let mut size = preamble;
        while size < preamble * 3 {
            for _ in 0..20 {
                tree.append(
                    &mut state.spine,
                    EventPayload::Post {
                        from: Author::User,
                        origin: direct(&"filler ".repeat(40), false),
                    },
                )
                .unwrap();
            }
            size = rendered(&tree, &state);
        }
        (tree, state, size / 2)
    }

    /// Under budget, nothing fires — the check has to be able to say no
    /// before its yes means anything.
    #[test]
    fn compaction_does_not_fire_under_budget() {
        let (mut tree, mut state) = setup();
        let fired = state
            .compaction_if_needed(&mut tree, 1024 * 1024, 0.25)
            .unwrap();
        assert!(fired.is_none());
        assert!(!state.compaction_requested);
        assert!(
            !tree
                .events
                .values()
                .any(|e| matches!(e.payload, EventPayload::Handback { .. }))
        );
    }

    /// Over budget, a `Compaction` condition is logged carrying both
    /// numbers, and a request goes out for the handler.
    #[test]
    fn compaction_fires_over_budget_and_asks_for_a_program() {
        let (mut tree, mut state, budget) = crowded();
        let fired = state.compaction_if_needed(&mut tree, budget, 0.25).unwrap();
        assert!(matches!(fired, Some(StepOutput::LlmRequest(_))));
        assert!(state.compaction_requested, "a compaction was asked for");

        // A compaction request is its own event now — not a condition,
        // because nothing stopped.
        let logged = tree
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Compaction {
                    measured, limit, ..
                } => Some((*measured, *limit)),
                _ => None,
            })
            .expect("a compaction request");
        let (rendered, b) = logged;
        assert_eq!(b, budget);
        assert!(rendered > budget, "{rendered} should exceed {budget}");
    }

    /// A document has a floor no handler can reach — the card and the
    /// worked examples are 21KB before a conversation starts, and are
    /// not rows — so a budget set near it makes every batch fail and the
    /// condition re-fire on every prompt. The bound is what makes that
    /// terminate: after two attempts the branch carries on over budget,
    /// which is the lesser failure.
    #[test]
    fn compaction_gives_up_rather_than_looping_when_it_cannot_help() {
        let (mut tree, mut state, budget) = crowded();
        // A budget under the floor: nothing the handler removes can
        // bring the document beneath it.
        let impossible = 1024;
        // **It does not ask even once.** This used to assert that the
        // branch asked `COMPACTION_ATTEMPTS` times and then stopped —
        // two completions spent on a document no handler can shrink.
        // The bound was the wrong instrument: it counts fires since the
        // last *success*, and at the floor every round succeeds at
        // removing rows while shrinking nothing, so on `sweep-200` with
        // a 34,000-byte budget against a 25,792-byte floor it fired
        // thirteen times. Comparing the floor to the threshold settles
        // it before the first ask.
        for _ in 0..=COMPACTION_ATTEMPTS {
            assert!(
                state
                    .compaction_if_needed(&mut tree, impossible, 0.25)
                    .unwrap()
                    .is_none(),
                "a budget under the floor is not worth a completion"
            );
            state.compaction_requested = false;
        }
        // And the fixture's own budget — which the floor fits under —
        // still asks, so the guard has not simply turned compaction off.
        assert!(
            state
                .compaction_if_needed(&mut tree, budget, 0.25)
                .unwrap()
                .is_some(),
            "a document that compaction can bring under budget is still asked about"
        );
    }

    /// **The bound survives the process.** It was a counter on the
    /// `Runner` until a live log accumulated seven compaction
    /// conditions: a restart built a fresh `Runner` over the same
    /// branch, the count came back 0, and the branch asked again — two
    /// attempts per process, forever, on a document no handler could
    /// shrink. This drives the same loop through a *new* `Runner` each
    /// time, which is what the old counter could not survive.
    #[test]
    fn the_compaction_bound_is_read_off_the_log_not_remembered() {
        let (mut tree, state, budget) = crowded();
        let mut leaf = state.spine.leaf_id;
        // The fixture's own budget: over it, and with a floor under it,
        // so every ask is one the floor guard allows. The bound is for
        // the other failure — a handler that runs and frees nothing —
        // and that is what a fresh `Runner` each round stands in for.
        let impossible = budget;
        for attempt in 0..COMPACTION_ATTEMPTS {
            let mut fresh = Runner::with_spine(&tree, tree.spine_at(leaf));
            assert!(
                fresh
                    .compaction_if_needed(&mut tree, impossible, 0.25)
                    .unwrap()
                    .is_some(),
                "attempt {attempt} within the bound still asks"
            );
            // The handler returned and its batch was rejected; the
            // process ends here, taking every field with it. Only the
            // log carries over — which is the point.
            leaf = fresh.spine.leaf_id;
        }
        let mut fresh = Runner::with_spine(&tree, tree.spine_at(leaf));
        assert!(
            fresh
                .compaction_if_needed(&mut tree, impossible, 0.25)
                .unwrap()
                .is_none(),
            "past the bound it stops asking, even in a process that never asked"
        );
    }

    /// The request has to reach what the compaction program is written
    /// from, and it has to leave again afterwards.
    ///
    /// Reaching it was the original bug: logged `Pushed` the directive
    /// rendered nowhere, and two live runs saw only the conversation
    /// and its unanswered task, and answered the task. Every other test
    /// here passed throughout — they checked the condition was logged
    /// and that its report *renders*, never that the model would be
    /// shown it.
    ///
    /// Leaving again is the other half, and making it a durable row was
    /// how that got broken: an instruction saying "write a compaction
    /// program, nothing else" stayed in the history after the episode
    /// ended, and on 2026-09-17 two expired copies of it — 3,788 bytes
    /// in a document that had just been compacted for being too large —
    /// led the model to write a third compaction program unprompted,
    /// which trapped. So it rides the ephemeral tail: the last thing
    /// read before the program is written, and gone by the next
    /// request.
    #[test]
    fn the_compaction_request_reaches_the_model_but_is_not_a_row() {
        let (mut tree, mut state, budget) = crowded();
        // A real conversation has run a program before it is big enough
        // to compact, and the fold inserts a report where a program
        // handed back — so a branch with no `Turn` on it would not
        // exercise the path a live run takes.
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("tell(\"working\");")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        let fired = state
            .compaction_if_needed(&mut tree, budget, 0.25)
            .unwrap()
            .expect("the document is over budget, so compaction fires");
        let StepOutput::LlmRequest(request) = fired else {
            panic!("compaction asks for a completion: {fired:?}");
        };
        let tail = request.tail.expect("the directive rides the tail");
        let doc = crate::document::render(&tree, &state.spine, 64 * 1024);

        // The rolling document carries no trace of the directive — what
        // survives a compaction episode is its `Compacted` events and
        // the shortened rows, not the instruction that asked for them.
        let rows = doc
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !rows.contains("STOP"),
            "the directive must not be a durable row: {rows}"
        );

        // What the model is actually sent is the document plus the tail.
        let text = doc
            .with_tail(&tail)
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("compaction program"), "not in the request");
        assert!(text.contains("history.remove"), "the verbs are not there");

        // The conversation stays: it is the cache prefix, so re-sending
        // it is nearly free, and a rewrite needs the text it is
        // shortening. What changes is the register of the request at
        // the end of it — three live runs read a polite one and did the
        // unfinished task instead.
        assert!(text.contains("STOP"), "{text}");
        assert!(text.contains("not being worked on"), "{text}");
        assert!(
            text.contains("filler filler"),
            "the history itself is still there to compact"
        );
    }

    /// Already compacting, it does not fire again — which is what stops
    /// a handler that frees nothing from asking for itself forever.
    #[test]
    fn compaction_does_not_fire_while_already_compacting() {
        // Reads the environment through `context_tokens`, so it waits
        // for whoever is setting it — see `env_lock`.
        let _env = env_lock();
        let (mut tree, mut state, budget) = crowded();
        state.compaction_if_needed(&mut tree, budget, 0.25).unwrap();
        let again = state.compaction_if_needed(&mut tree, budget, 0.25).unwrap();
        assert!(again.is_none());
    }

    /// The whole cycle: fire, run a handler that removes a row, and
    /// find the `Compacted` event on the log with the original still
    /// present underneath it.
    #[test]
    fn a_compaction_handler_commits_its_batch() {
        let (mut tree, mut state, budget) = crowded();
        let target = tree
            .events
            .values()
            .filter(|e| matches!(e.payload, EventPayload::Post { .. }))
            .map(|e| e.id.as_u64())
            .min()
            .map(EventId::new)
            .expect("a post to compact");

        state.compaction_if_needed(&mut tree, budget, 0.25).unwrap();
        state.next_prompt_floor = Counted::Floor(60_000);
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program(&format!(
                    "remove_history({}, \"post\");",
                    target.as_u64()
                ))),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert!(!state.compaction_requested, "the request is closed");
        assert_eq!(
            state.next_prompt_floor,
            Counted::Stale,
            "the count described the document this commit just shrank, \
             so it cannot be the evidence for compacting again"
        );
        let compacted: Vec<_> = tree
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Compacted { of, .. } => Some(*of),
                _ => None,
            })
            .collect();
        // The row it named, **and the program that named it** — a
        // spent compaction program is harness-requested work in a
        // document the harness asked to be shrunk, so it goes too.
        assert!(
            compacted.contains(&target),
            "the row it named: {compacted:?}"
        );
        let own: Vec<EventId> = tree
            .events
            .values()
            .filter(|e| {
                matches!(&e.payload, EventPayload::Part { reply, part: Part::Cell(_) }
                         if *reply == state.reply_id)
            })
            .map(|e| e.id)
            .collect();
        for block in &own {
            assert!(
                compacted.contains(block),
                "the compaction program's own block #{} is spent: {compacted:?}",
                block.as_u64()
            );
        }
        assert!(
            tree.events.contains_key(&target),
            "the original row is shadowed, never removed"
        );

        // **And the document still alternates.** Every block of the
        // compaction program is shadowed, so its assistant slot is
        // empty — which is the case `render` handles by emitting no
        // turn and merging the user blocks either side, rather than
        // leaving two assistant messages next to each other for a
        // provider to reject.
        let doc = crate::document::render(&tree, &state.spine, 64 * 1024);
        let roles: Vec<_> = doc.conversation().iter().map(|m| m.role).collect();
        for pair in roles.windows(2) {
            assert_ne!(
                pair[0], pair[1],
                "two turns in the same role after a self-compaction: {roles:?}"
            );
        }
    }

    // `a_history_edit_applies_from_any_program` lives in `testkit`
    // now, where "this reply's own blocks" is something the projection
    // already knows rather than something the test reconstructs.

    /// A label indexes a call; it does not replay its arguments. The
    /// argument that *identifies* the call survives a huge one standing
    /// beside it, which is the whole reason each is clipped on its own
    /// rather than the joined string being clipped once.
    #[test]
    fn a_label_indexes_a_call_instead_of_replaying_its_arguments() {
        let whole_file = "x".repeat(100_000);
        let label = call_label(&Call::Invoke {
            name: "replace_file".into(),
            args: json!(["src/lib.rs", whole_file]),
            site: 0,
        });
        assert!(label.contains("src/lib.rs"), "{label}");
        assert!(
            !label.contains(&"x".repeat(100)),
            "payload replayed: {label}"
        );
        assert!(label.len() < crate::report::LABEL_MAX_BYTES + 32, "{label}");

        // A `tell` is the same shape: the person already read the text,
        // and the row is here so a later program can find the call.
        let label = call_label(&Call::Send {
            prose: false,
            to: Address::User,
            text: "y".repeat(8_000),
            input: serde_json::Value::Null,
            options: Vec::new(),
            expects_reply: false,
            site: 0,
            site_end: 0,
        });
        assert!(label.starts_with("tell(user, "), "{label}");
        assert!(label.len() < crate::report::LABEL_MAX_BYTES + 32, "{label}");
    }

    // ── `Transport::Notebook` (phase 25, step 25.4) ────────────────
    //
    // Batch first: the reply is split into cells and they run in
    // sequence *after* the completion ends. **A reply is one run**
    // (D7) — the cells share a frame that is never unwound between
    // them, so a cell boundary never reaches `finish_program` and the
    // reply's own `Return(0)` does, once.

    /// A three-cell reply, decomposed. **The reply is recorded, not
    /// reassembled** (28): it becomes its pieces in source order — a
    /// `Part::Prose` for each paragraph, a `Part::Cell` holding each
    /// fenced block *with its fences* — and it is still **one run**
    /// with one handback (D7), because the cells share a frame nothing
    /// unwinds between them.
    #[test]
    fn a_three_cell_reply_is_three_cells_one_run_and_one_handback() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply_whole(
            "Reading the two files first.\n\n\
             ```js\n\
             let total = 1;\n\
             ```\n\n\
             Now the adjustment.\n\n\
             ```js\n\
             total = total + 41;\n\
             ```\n\n\
             And the answer.\n\n\
             ```js\n\
             console.log(`total is ${total}`);\n\
             ```\n",
        );

        // Each cell holds the block the model wrote, fences included —
        // that is what makes the parts concatenate back to the reply
        // byte for byte, which the harness now checks on every reply
        // (`Invariant::PartsConcatenate`).
        assert_eq!(
            r.cells,
            [
                "```js\nlet total = 1;\n```\n",
                "```js\ntotal = total + 41;\n```\n",
                "```js\nconsole.log(`total is ${total}`);\n```\n",
            ]
        );

        // Prose and cells interleave in source order. The whole reply
        // arrived in one piece here, so every part is on the log before
        // the first cell runs — see
        // `chunk_boundaries_do_not_change_the_reply` for what that does
        // and does not guarantee.
        assert_eq!(
            r.kinds,
            [
                "Reply", "Part", // "Reading the two files first."
                "Part", // cell 0
                "Part", // "Now the adjustment."
                "Part", // cell 1
                "Part", // "And the answer."
                "Part", // cell 2
                // The reply's cost, once. The whole text arrived
                // before anything ran, so this is where it stopped.
                "ReplyEnd", "Call", // the three prose segments, as sends to the user
                "Call", "Call", "Handback", "Console", "Result", "Result", "Result",
            ],
            "three cells, six parts, one ReplyEnd, one Handback"
        );

        // And they really shared a scope.
        assert_eq!(r.printed, ["total is 42"]);
    }

    /// The prose reaches the person as a `Call::Send { to: User }` —
    /// the same shape a `tell` takes, so the model re-reads its own
    /// reply in a form it already knows (D15).
    #[test]
    fn prose_segments_are_sends_to_the_user_in_source_order() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply_whole(
            "First I look.\n\n```js\nlet a = 1;\n```\n\n\
             Then I decide.\n\n```js\na = 2;\n```\n\nThat is all.\n",
        );
        assert_eq!(r.prose, ["First I look.", "Then I decide.", "That is all."]);
        // Their sites are synthetic — checked on every reply now, by
        // `Invariant::ProseIsSynthetic`, which is where that rule went.
    }

    /// **A prose send gets exactly one `Result` and leaves no dangling
    /// promise.** It has no VM promise behind it — nothing awaited it,
    /// nothing can — so it settles through the same unwaited-`tell`
    /// path the host already has rather than a second mechanism.
    #[test]
    fn a_prose_send_settles_exactly_once() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply("Just a sentence.\n\n```js\ntell(\"ok\"); finish();\n```\n");

        // Two sends leave this reply — the prose, and the `tell` the
        // program wrote. Only the prose has no source behind it, which
        // is what the zero-width site means: `finish()` sends nothing
        // now, so it is the one send in the reply with no expression of
        // its own. It settles the way the host settles any unwaited
        // `tell`, with no program awaiting it and no complaint. That
        // every call in a reply settles exactly once is
        // `Invariant::CallsSettle`, checked on every reply; this one
        // names the prose send in particular.
        assert_eq!(r.prose, ["Just a sentence."], "one prose segment");
        let settlements = r
            .settled
            .iter()
            .filter(|(call, _)| c.site_of(*call) == (0, 0))
            .count();
        assert_eq!(settlements, 1, "exactly one delivery for the prose");
        assert_eq!(r.tells, ["ok"], "and the program's own `tell` went too");
    }

    /// A multi-paragraph report — the case this whole phase exists for —
    /// arrives as one message with its structure intact, rather than
    /// one send per line.
    #[test]
    fn a_multi_paragraph_report_is_one_send() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply(
            "# What I found\n\n\
             The retry policy lives in two places.\n\n\
             - `client.rs` sets the ceiling\n\
             - `retry.rs` sets the backoff\n\n\
             I would keep the second.\n",
        );
        assert_eq!(r.prose.len(), 1, "one report, one message");
        assert!(r.prose[0].starts_with("# What I found"));
        assert!(r.prose[0].contains("- `client.rs` sets the ceiling\n- `retry.rs`"));
        assert!(r.prose[0].ends_with("I would keep the second."));
    }

    /// One report and one next completion for a three-cell reply, not
    /// three. The branch prompts exactly once, when the *reply* ends.
    #[test]
    fn a_three_cell_reply_prompts_once() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply(
            "```js\nconsole.log(\"a\");\n```\n\n\
             ```js\nconsole.log(\"b\");\n```\n\n\
             ```js\nconsole.log(\"c\");\n```\n",
        );
        assert_eq!(r.printed, ["a", "b", "c"], "three cells, one scope");
        // One report and one next completion, not one per cell — the
        // handback is the reply's, not the cell's. `Invariant::OneReply`
        // already holds the other half of this.
        assert_eq!(
            r.kinds.iter().filter(|k| **k == "Handback").count(),
            1,
            "one report, not one per cell: {:?}",
            r.kinds
        );
        assert!(!r.rests, "and exactly one next completion");
    }

    /// **A raise in cell 0, resumed, runs cells 1 and 2.** The VM is
    /// parked *inside* cell 0; resuming continues from that instruction
    /// and falls out of the cell into the driver, which walks on to the
    /// next one rather than treating the handback as a finished run
    /// (D9).
    #[test]
    fn a_raise_in_cell_0_resumes_into_the_later_cells() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply(
            "```js\nconst pick = raise(\"which\", { of: [1, 2] });\n```\n\n\
             ```js\nconsole.log(`picked ${pick}`);\n```\n\n\
             ```js\nconsole.log(\"and the last cell ran\");\n```\n",
        );
        assert!(
            matches!(r.ended, Ending::Raised { .. }),
            "a raise suspends the run"
        );

        let after = c.resume(json!("the second one"));
        assert_eq!(
            after.printed,
            ["picked the second one", "and the last cell ran"],
            "the cells after the raise run on resume"
        );
    }

    /// **`Call::site` keeps its meaning exactly**: an offset into the
    /// reply the model wrote (28, "Sites"). The compiler emits it
    /// against the parse buffer — the prelude, then the reply with
    /// prose blanked and cells verbatim — so subtracting
    /// `ReplCore::source_base()` once, at log time, makes it an offset
    /// into the markdown itself. Nothing downstream sees a prelude
    /// offset, and nothing has to work out which cell first.
    #[test]
    fn every_call_site_resolves_into_the_reply() {
        let mut c = Conversation::new();
        c.user("go");
        let reply = "First I speak.\n\n\
                     ```js\ntell(\"from the first cell\");\n```\n\n\
                     Then again, further down.\n\n\
                     ```js\nconst x = 1;\ntell(\"from the second cell\");\n```\n";
        let r = c.reply(reply);

        // That every span lands inside the reply, on a boundary, is
        // `Invariant::SitesAreReplyAbsolute`, checked on every reply;
        // and that the parts *are* the reply is `PartsConcatenate`.
        // What is left is that a span names the call it belongs to.
        assert_eq!(r.told.len(), 2, "two tells");
        for told in &r.told {
            let (site, site_end) = c.site_of(*told);
            let sliced = &reply[site as usize..site_end as usize];
            assert!(
                sliced.starts_with("tell(") && sliced.contains("cell"),
                "site sliced {sliced:?} out of the reply"
            );
        }
    }

    /// And the subtraction is real work, not a no-op in the other
    /// direction either: the second cell's `tell` sits well into the
    /// markdown, and the site says so — it is *not* cell-local.
    #[test]
    fn a_site_is_reply_absolute_not_cell_local() {
        let mut c = Conversation::new();
        c.user("go");
        let reply = "A fairly long opening paragraph, so the offsets differ.\n\n\
                     ```js\nlet a = 1;\n```\n\n\
                     More prose here as well.\n\n\
                     ```js\ntell(\"second\");\n```\n";
        let r = c.reply(reply);

        let site = c.site_of(r.told[0]).0 as usize;
        assert_eq!(
            site,
            reply.find("tell(\"second\")").unwrap(),
            "the site is where the tell is in the reply"
        );
        assert!(site > 60, "and that is far from the start of its own cell");
    }

    /// **A reply with no cells rests the branch** (D4), and nothing was
    /// implemented to make it: with no `Turn` and no outcome logged,
    /// `last_turn_outcome` finds nothing and `needs_prompt` is false on
    /// both clauses — a `Send` is not a `Post`. That is bit-for-bit the
    /// state `finish(text)` produces.
    ///
    /// The model still **speaks**: silence is a branch that produces
    /// nothing, and this one produced an answer.
    #[test]
    fn a_reply_with_no_cells_speaks_and_rests_the_branch() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply(
            "The retry policy already lives in `retry.rs`, so there is \
             nothing to change.\n\n```text\njust a quote, not a cell\n```\n",
        );

        assert!(
            r.rests,
            "the model spoke and stopped: that is a finished turn"
        );
        assert!(r.cells.is_empty(), "a ```text block is a quote, not a cell");
        assert!(
            r.prose[0].contains("already lives in `retry.rs`"),
            "{:?}",
            r.prose
        );
        assert_eq!(
            r.kinds,
            // A cell-less reply is still a completion, and still what
            // the model must be shown as its own past turn. It runs an
            // empty program — there is no separate "nothing to run"
            // path any more — so it closes with an outcome like any
            // other reply, and rests because no cell of its own was
            // ever logged for `last_turn_outcome` to find.
            [
                "Reply", "Part", "ReplyEnd", "Call", "Handback", "Console", "Result"
            ],
            "the prose is delivered and nothing ran"
        );
    }

    /// **A prose-only reply still rests.** This is the `plain-question`
    /// shape — a question with no work in it, answered in a sentence —
    /// and D4 is what makes it possible at all. The empty-reply rule
    /// below must not touch it: *spoke* is the test, not *ran*.
    #[test]
    fn a_prose_only_reply_rests_the_branch_and_is_not_asked_again() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "what is a mutex?");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(crate::host::scripted_markdown(
                    "A mutex has one holder; a semaphore has a count.\n",
                )),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        assert!(state.is_idle());
        assert!(
            !state.needs_prompt(&tree),
            "the model answered; the next thing to happen is whatever the person says"
        );
    }

    /// **The cap is wired, not just written.** `cap_prose` has a unit
    /// test of its own; this is the half that was missing when the bug
    /// existed — a degenerate reply actually reaching the person in
    /// full. Both doors: a prose segment and a `tell`, which are built
    /// at different sites and so can be fixed one at a time by
    /// accident.
    #[test]
    fn neither_door_delivers_a_degenerate_reply_whole() {
        let huge = "x".repeat(crate::report::PROSE_MAX_BYTES * 2);
        for source in [
            // Prose: the harness makes the Send itself.
            format!("{huge}\n\n```js\nconsole.log(1);\n```\n"),
            // `tell`: the program makes it.
            format!("```js\ntell({});\n```\n", serde_json::json!(huge)),
        ] {
            let (mut tree, mut state) = setup_under();
            user_post(&mut state, &mut tree, "go");
            let out = state
                .step(
                    &mut tree,
                    StepInput::LlmResponse(crate::host::scripted_markdown(&source)),
                )
                .unwrap();
            drain(&mut state, &mut tree, out);

            let said: Vec<usize> = tree
                .path_events(state.spine.leaf_id)
                .iter()
                .filter_map(|e| match &e.payload {
                    EventPayload::Call(Call::Send {
                        to: Address::User,
                        text,
                        ..
                    }) => Some(text.len()),
                    _ => None,
                })
                .collect();
            assert!(!said.is_empty(), "something was said");
            for len in said {
                assert!(
                    len <= crate::report::PROSE_MAX_BYTES + 64,
                    "delivered {len} bytes of a {} byte reply",
                    huge.len()
                );
            }
        }
    }

    /// **`keep` and `peek` take a value or a lambda, a result or an id.**
    ///
    /// The lambda is why: it lets the result stay anonymous. The value
    /// form has to name it twice — once to keep, once to project — and
    /// a name costs a `const` the next cell cannot reuse.
    #[test]
    fn keep_and_peek_project_with_a_lambda_or_a_value() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(crate::host::scripted_markdown(
                    "```js\n\
                     history.keep(2, \"beta\");\n\
                     history.peek({ id: 2 }, (r) => \"from \" + r.id);\n\
                     ```\n",
                )),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let shown: Vec<(crate::types::RenderMode, String)> = tree
            .path_events(state.spine.leaf_id)
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Render { mode, value, .. } => {
                    Some((*mode, note_text(value)))
                }
                _ => None,
            })
            .collect();
        assert_eq!(shown.len(), 2, "both landed: {shown:?}");
        assert_eq!(
            shown[0],
            (crate::types::RenderMode::Kept, "beta".to_owned()),
            "a plain value projection lands, and a bare id names the row"
        );
        assert_eq!(
            shown[1],
            (crate::types::RenderMode::Peeked, "from 2".to_owned()),
            "the lambda ran, was given the whole object, and its result was stored"
        );
    }

    /// **The whole claim, end to end: `keep` stays and `peek` goes.**
    ///
    /// Everything else about these two verbs is bookkeeping. This is
    /// the promise the card makes — "in front of you now, gone from the
    /// next turn" — checked against the rendered document, because that
    /// is the only place the model can tell the difference.
    ///
    /// **And they live in different halves of the request.** A `keep`
    /// writes its value into the row's own line, up in the
    /// conversation, under the id that is already there; a `peek` rides
    /// the ephemeral tail, which is re-emitted at the new end each
    /// request and never written into the history — so a value meant
    /// for one reply costs no rewrite of anything above it.
    ///
    /// **One request exactly, with no timer in it.** A peek is spent by
    /// the next `Reply` on the log, so a log reopened a week later
    /// renders what the model was actually looking at.
    #[test]
    fn a_kept_row_stays_and_a_peeked_one_is_gone_next_turn() {
        // Two tools, so neither value is also a literal in the program
        // source the document carries: `contains` on a projection
        // written into the reply passes on the reply alone.
        let mut c = Conversation::new();
        c.answers("kept", serde_json::json!({ "out": "KEPT-VALUE" }));
        c.answers("peeked", serde_json::json!({ "out": "PEEKED-VALUE" }));
        c.reply(
            "```js\n\
             history.keep(await tools.kept());\n\
             history.peek(await tools.peeked());\n\
             ```\n",
        );

        let now = c.document();
        assert!(now.contains("KEPT-VALUE"), "the keep is inline: {now}");
        assert!(now.contains("PEEKED-VALUE"), "the peek is in the tail");
        assert!(
            now.contains("### peeked — here for this reply only"),
            "and the block says what it is: {now}"
        );

        // One more reply, which is what spends a peek.
        c.reply("Done.\n");
        let after = c.document();
        assert!(after.contains("KEPT-VALUE"), "kept means kept: {after}");
        assert!(
            !after.contains("PEEKED-VALUE"),
            "the peek was spent by the reply after it: {after}"
        );
        assert!(!after.contains("### peeked"), "and its block went too");
    }

    /// **A kept value renders on the row's own line, not beside it.**
    ///
    /// The first shape of this made the `Render` a row of its own,
    /// which double-charged anything whose row already showed its own
    /// value: `note("x")` and a `keep` of that note put `x` on the page
    /// twice, under two ids. A `keep` is one bit about a row that
    /// already exists, so there is one row and one id.
    #[test]
    fn a_kept_value_is_shown_by_the_row_it_came_from() {
        let mut c = Conversation::new();
        c.answers("echo", serde_json::json!({ "out": "ECHOED" }));
        c.reply("```js\nhistory.keep(await tools.echo(1));\n```\n");

        let doc = c.document();
        assert_eq!(
            doc.matches("ECHOED").count(),
            1,
            "one row, one copy of the value: {doc}"
        );
        // And it is under the call's *menu row*, indented as part of
        // it — not somewhere else that happens to hold those bytes.
        let row = doc
            .lines()
            .position(|l| l.starts_with("- `[") && l.contains("echo(1)"))
            .expect("the call's row");
        assert!(
            doc.lines()
                .nth(row + 1)
                .is_some_and(|l| l.starts_with("  ") && l.contains("ECHOED")),
            "the value is the indented line under the row: {doc}"
        );
    }

    /// **One result, one answer: the last `keep`/`peek` on it wins.**
    ///
    /// `keep` after `peek` promotes the row in place instead of leaving
    /// the value in the tail, and `peek` after `keep` retires the keep
    /// — have it one more turn, then stop paying for it, which is the
    /// only way to undo a `keep` short of compaction. Same rule as
    /// `remove` against `replace`, and for the same reason: they answer
    /// one question about one row, so the newest answer is the answer.
    ///
    /// It crosses turns, which is where a memo used to lie about it:
    /// the report carrying the kept row was frozen on the strength of
    /// being a pure function of the log up to its own outcome. See
    /// `Tree::append`, which drops the memo when a `Render` lands.
    #[test]
    fn the_last_keep_or_peek_on_one_result_is_the_one_that_renders() {
        let mut c = Conversation::new();
        c.answers("echo", serde_json::json!({ "out": "ECHOED" }));
        c.reply("```js\nhistory.keep(await tools.echo(1));\n```\n");
        assert!(
            !c.document().contains("### peeked"),
            "a keep is inline, not in the tail"
        );

        // A later reply peeks the same result: one more turn, then out.
        c.reply("```js\nhistory.peek(4);\n```\n");
        let once_more = c.document();
        assert!(
            once_more.contains("### peeked"),
            "the keep was retired into a peek: {once_more}"
        );
        assert_eq!(
            once_more.matches("ECHOED").count(),
            1,
            "and it is shown once, in the tail: {once_more}"
        );

        c.reply("Done.\n");
        let spent = c.document();
        assert!(!spent.contains("ECHOED"), "then not at all: {spent}");
    }

    /// **A projection is applied to the row's value, however the row
    /// was named.**
    ///
    /// `keep(f, v => v.content)` hands the lambda the result, and
    /// `keep(6, v => v.content)` has to hand it the same thing — the
    /// row is the only thing either call names. It did not: the lambda
    /// got the *id*, so a live `deepseek-v4-flash` run on 2026-09-23
    /// wrote `history.keep(15, (x) => x.map((f) => f.content))` off the
    /// menu and trapped on `cannot read .length of a number (15)`,
    /// losing eight file reads with it.
    #[test]
    fn a_projection_is_given_the_rows_value_even_when_the_row_is_named_by_id() {
        let mut c = Conversation::new();
        c.answers("echo", serde_json::json!({ "out": "ECHOED" }));
        c.reply("```js\nawait tools.echo(1);\nhistory.keep(4, (v) => v.out);\n```\n");

        let doc = c.document();
        let row = doc
            .lines()
            .position(|l| l.starts_with("- `[") && l.contains("echo(1)"))
            .expect("the call's row");
        assert!(
            doc.lines()
                .nth(row + 1)
                .is_some_and(|l| l.trim() == "ECHOED"),
            "the lambda saw the result, not the number 4: {doc}"
        );
    }

    /// **A whole result kept without a projection says how to narrow
    /// it, and names the field.**
    ///
    /// `keep(f)` on a `read_file` result stores `{content, version,
    /// id}`, which renders as one JSON line with every newline escaped
    /// — 31 KB of it on a live run of 2026-09-23, barely legible and
    /// paid for on every turn after. The card asks for a projection in
    /// the abstract; the row can ask for it where the wall of text is.
    #[test]
    fn a_whole_result_kept_says_which_field_to_keep_instead() {
        let mut c = Conversation::new();
        c.answers(
            "read_file",
            serde_json::json!({ "content": "x".repeat(4000), "version": "v1" }),
        );
        c.reply("```js\nhistory.keep(await tools.read_file(\"a.rs\"));\n```\n");

        let doc = c.document();
        assert!(
            doc.contains("`history.keep(4, (v) => v.content)` shows just that field"),
            "it names the verb, the row and the field: {doc}"
        );

        // A projection was given, so there is nothing to suggest.
        let mut c = Conversation::new();
        c.answers(
            "read_file",
            serde_json::json!({ "content": "x".repeat(4000), "version": "v1" }),
        );
        c.reply("```js\nhistory.keep(await tools.read_file(\"a.rs\"), (v) => v.content);\n```\n");
        assert!(
            !c.document().contains("shows just that field"),
            "no advice where none is owed"
        );
    }

    /// **The readout and the trigger answer in one unit, or not at
    /// all.**
    ///
    /// They were two functions for a day and disagreed inside it: the
    /// tail fell back to bytes-against-the-document-budget whenever
    /// nothing had cached a measurement, so `agent document` on a
    /// 1M-token model announced "133% full" about a request the live
    /// trigger had measured at 2% and correctly let pass. The rendering
    /// of a request is how anyone checks what the model was sent, so a
    /// number there that the request never carried is worse than no
    /// number.
    ///
    /// Now one function answers both, and its silence is the case that
    /// went wrong: a window configured, nothing counted against it yet,
    /// which is every reopened log until this commit gave `Usage` the
    /// window to carry.
    #[test]
    fn the_fullness_readout_is_silent_rather_than_answering_in_the_wrong_unit() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        state.next_prompt_floor = Counted::Never;

        // No window anywhere: bytes are the only budget there is, and
        // the trigger uses them, so the readout speaks in them.
        assert!(state.fullness(&tree, None, 40_000, 65_536).is_some());

        // A window *is* configured and nothing has counted against it,
        // on the log or live. The trigger declines to fire; the readout
        // declines to speak, rather than answering the other question.
        let quiet = state.fullness(&tree, Some(1_000_000), 40_000, 65_536);
        assert!(
            quiet.is_none(),
            "it answered in bytes about a token window: {quiet:?}"
        );
    }

    /// **A reopened log measures itself the way the live session did.**
    ///
    /// `prompt` was on the log and the window was in a shell variable,
    /// so rendering a past request could not reproduce it — `agent
    /// document` on a 1M-token run measured in bytes and announced
    /// "133% full" about a request the live trigger passed at 2%.
    /// `Usage` carries both halves now, and `fullness` reads them when
    /// there is no live state, so the instrument and the request agree
    /// whatever shell it is run from.
    #[test]
    fn a_reopened_log_measures_itself_from_its_own_usage() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        // What a replay has: no live count, no environment.
        state.next_prompt_floor = Counted::Never;

        // Nothing counted on the log either, so bytes are all there is.
        let (m, _, unit) = state.fullness(&tree, None, 40_000, 65_536).expect("bytes");
        assert_eq!((m, unit), (40_000, Measure::Bytes));

        // A reply that reported a count against a window. The replay
        // reads both off the log and answers in tokens — the unit the
        // live trigger used.
        let reply = tree.append(&mut state.spine, EventPayload::Reply).unwrap();
        tree.append(
            &mut state.spine,
            EventPayload::ReplyEnd {
                reply,
                how: crate::types::ReplyEnd::Finished,
                usage: crate::host::Usage {
                    prompt: 12_000,
                    cached: 0,
                    completion: 10,
                    reasoning: 0,
                    window: Some(1_000_000),
                },
            },
        )
        .unwrap();

        let (m, limit, unit) = state.fullness(&tree, None, 40_000, 65_536).expect("tokens");
        assert_eq!((m, unit), (12_000, Measure::Tokens));
        assert!(limit > 100_000, "the window came off the log too: {limit}");
    }

    /// **A count describes the request that returned it, and the
    /// program that ran since then has been appending.**
    ///
    /// Usage arrives at the end of a stream, so the trigger holds a
    /// number from one request ago while the next one grows. That gap
    /// was small when only `note` could widen it. It is not small now:
    /// a live run on 2026-09-23 with a 24,000-token window kept three
    /// files in one program — 96 KB, about 24k tokens — and the trigger
    /// let the request through holding a count of 8,554 from the turn
    /// before.
    ///
    /// So the trigger takes the larger of the count and what this
    /// conversation's own measured density says the document is worth
    /// now. The density is this document's bytes over this document's
    /// charged tokens, recomputed every reply — not a constant, and not
    /// a guess about content the crate cannot see.
    #[test]
    fn the_trigger_sees_what_the_program_added_since_the_last_count() {
        let (tree, mut state) = setup_under();
        state.next_prompt_floor = Counted::Floor(8_554);

        // No density measured yet: the count is all there is, and it is
        // comfortably under a 24k window.
        let (measured, _, unit) = state
            .fullness(&tree, Some(24_000), 140_000, 64 * 1024)
            .expect("counted");
        assert_eq!((measured, unit), (8_554, Measure::Tokens));

        // One reply's worth of calibration: the request that counted
        // 8,554 tokens rendered to 34,000 bytes, so this conversation
        // runs about 4 bytes to the token.
        state.bytes_per_token = Some(34_000.0 / 8_554.0);
        let (measured, usable, _) = state
            .fullness(&tree, Some(24_000), 140_000, 64 * 1024)
            .expect("counted");
        assert!(
            measured > usable,
            "140 KB at this document's own density is over a 24k window: \
             {measured} against {usable}"
        );
        assert!(
            crate::compaction::should_fire(measured, usable, 0.2),
            "and that is what the trigger asks"
        );
    }

    /// **The compaction directive renders from the log.**
    ///
    /// It was gated on `compaction_requested`, a live flag that is
    /// false in every process that merely reads a log — so `agent
    /// document` could not show the directive at all. That is the one
    /// prompt in this system whose wording gets argued over most, and
    /// it was the only one nobody could look at. Both moments that move
    /// the flag are already on the log: the `Compaction` event, and the
    /// handback of the program that answers it.
    #[test]
    fn a_reopened_log_can_render_the_compaction_directive() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        tree.append(
            &mut state.spine,
            EventPayload::Compaction {
                measured: 34_734,
                limit: 20_000,
                unit: Measure::Tokens,
            },
        )
        .unwrap();

        // The flag a live run would be holding is not set here, which
        // is exactly the state a reopened log is in.
        assert!(!state.compaction_requested);
        let tail = state.request_tail(&tree).unwrap_or_default();
        assert!(
            tail.contains("STOP — THIS CONVERSATION IS FULL"),
            "the directive is what the tail carries: {tail}"
        );
        assert!(tail.contains("34734"), "with the numbers it fired on");
    }

    /// **Printing a result's bytes is told about, the way copying them
    /// into a note has been.**
    ///
    /// Measured on the card of 2026-09-24: 9 of 60 first replies to a
    /// reading task wrote `console.log(f.content)`, and two wordings
    /// failed to shift them — a closing consequence and a flat ban
    /// naming the syntax performed identically, 9/60 against 11/60,
    /// p=0.81. The report was the channel that had never said anything
    /// about it: `copied_note` fires when a run copies a row's bytes
    /// into a note, and was blind to the run printing them instead.
    ///
    /// Containment, not equality — a program writes `console.log("X:\n"
    /// + f.content)`, and an equality test sees nothing wrong with it.
    #[test]
    fn printing_a_results_bytes_is_named_in_the_report() {
        let mut c = Conversation::new();
        let body = "warning: unused\n".repeat(60);
        c.answers("bash", serde_json::json!({ "status": 0, "stdout": body }));
        c.reply("```js\nconst r = await tools.bash(\"build\");\nconsole.log(\"out:\\n\" + r.stdout);\n```\n");

        let doc = c.document();
        assert!(
            doc.contains("You printed the bytes of"),
            "the report says so: {doc}"
        );
        assert!(
            doc.contains("history.keep(id)") && doc.contains("history.peek(id)"),
            "and names what to do instead"
        );

        // A trace of what happened is the channel's job and draws
        // nothing — otherwise the advisory would fire on every loop
        // the card asks for.
        let mut c = Conversation::new();
        c.answers("bash", serde_json::json!({ "status": 0, "stdout": body }));
        c.reply("```js\nconst r = await tools.bash(\"build\");\nconsole.log(\"lines:\", r.stdout.split(\"\\n\").length);\n```\n");
        assert!(
            !c.document().contains("You printed the bytes of"),
            "a count is not a payload: {}",
            c.document()
        );
    }

    /// **A stray fence is not a message.**
    ///
    /// A ` ``` ` the model opened and shut with no cell in it parses as
    /// prose, and prose that is not blank becomes a `Send` — so the
    /// person got a message whose entire content was backticks. Seen
    /// live on 2026-09-22 in the card A/B, where a reply's second
    /// message to the person was "```\n```".
    #[test]
    fn prose_that_is_only_fence_punctuation_is_not_sent() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(crate::host::scripted_markdown(
                    "Looking now.\n\n```js\nconsole.log(1);\n```\n\n```\n```\n",
                )),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let said: Vec<String> = tree
            .path_events(state.spine.leaf_id)
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Call(Call::Send {
                    to: Address::User,
                    text,
                    ..
                }) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(said, ["Looking now."], "only the sentence reached them");
    }

    /// **A reply that said nothing at all is asked again — once.**
    ///
    /// Not the same case as the one above, and folding the two together
    /// cost a live run on 2026-09-19: the provider spent the whole
    /// completion on `reasoning_content` and returned empty `content`,
    /// so the reply had no prose and no cell, the branch rested, and
    /// the harness exited 0 with the task untouched. Nobody was told
    /// anything — that is not an answer.
    #[test]
    fn a_reply_that_said_nothing_is_asked_again_once() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let empty = || StepInput::LlmResponse(crate::host::scripted_markdown(""));

        let asked =
            |outs: &[StepOutput]| outs.iter().any(|o| matches!(o, StepOutput::LlmRequest(_)));

        let out = state.step(&mut tree, empty()).unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            asked(&settled),
            "an empty completion is usually a hiccup: ask again — {settled:?}"
        );

        // Twice in a row is a branch that cannot speak, and a third ask
        // is a loop that bills for itself.
        let out = state.step(&mut tree, empty()).unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            !asked(&settled),
            "two empty replies rest, with both turns on the log to be seen"
        );
        assert!(state.is_idle());
    }

    /// And the model is *told* its reply was empty, in the one place it
    /// reads its own turns back. A zero-byte assistant message says
    /// nothing and is malformed on the wire besides.
    #[test]
    fn an_empty_reply_renders_as_a_marker_not_as_nothing() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(crate::host::scripted_markdown("")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let doc = crate::document::render(&tree, &state.spine, 64 * 1024);
        let assistant = doc
            .conversation()
            .iter()
            .find(|m| m.role == crate::document::ChatRole::Assistant)
            .expect("the empty reply still gets a turn")
            .content
            .clone();
        assert_eq!(assistant, crate::document::EMPTY_REPLY_NOTE);
    }

    /// **A handback's site points into the reply, not the prelude.**
    /// Same coordinate system as a `Call`'s (28, "Sites") — and it was
    /// not, until a live run made it visible: a trap report read
    /// `12:1: test is not defined` over a blank line and a bare caret,
    /// because line 12 of the parse buffer is prelude and the report
    /// renders against the ten-line reply.
    #[test]
    fn a_traps_site_is_an_offset_into_the_reply() {
        let mut c = Conversation::new();
        c.user("go");
        let reply = "Using what the last reply read.\n\n\
                     ```js\nconst prev = null;\nconsole.log(prev.content);\n```\n";
        let r = c.reply(reply);

        // That the site is in the reply at all is checked on every
        // reply now (`Invariant::SitesAreReplyAbsolute`); what is left
        // for this test is that it names the right expression.
        let Ending::Trapped { message, site } = &r.ended else {
            panic!("expected a trap, got {:?}", r.ended)
        };
        assert!(message.contains("cannot read property"), "{message}");
        assert_eq!(
            &reply[*site as usize..*site as usize + 7],
            "content",
            "it names the offending expression"
        );
    }

    /// **A tool call in another harness's syntax is an action that
    /// missed, not an answer.** Measured on `qwen3.8-flash`: it wrote
    /// `<tool_call><function=bash>…` as prose in four runs out of four,
    /// once carrying the task's entire answer. The reply *spoke*, so
    /// D4 rested the branch and the work was abandoned with the answer
    /// sitting in the transcript.
    #[test]
    fn a_reply_that_wrote_a_foreign_tool_call_is_told_so_and_asked_again() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply(
            "Let me look at what's here.\n\n<tool_call>\n<function=bash>\n\
             <parameter=command>\nls -la\n</parameter>\n</function>\n</tool_call>",
        );

        assert!(!r.rests, "the work is not finished");
        // And it is *told* why, in the channel it demonstrably reads.
        assert!(
            r.notices.last().is_some_and(|n| n.contains("```js")),
            "the notice names the one channel that runs: {:?}",
            r.notices
        );
    }

    /// **But a prose answer is still an answer.** A reply that ran
    /// nothing because it was answering a post rests, exactly as D4
    /// says — `plain-question`'s whole shape, and a follow-up answered
    /// mid-task is the same shape.
    #[test]
    fn a_prose_answer_to_a_post_rests_even_after_cells_have_run() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        // A first reply that really does run something, and rests — so
        // the post below is one the branch is actually shown, rather
        // than one that arrives while a request is already out.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("let a = 1; tell(\"ok\"); finish();")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert!(state.is_idle(), "finish(text) rests");

        // Then the person asks a question, and it is answered in prose.
        user_post(&mut state, &mut tree, "what does that mean?");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(crate::host::scripted_markdown(
                    "It means the total is one.\n",
                )),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            !settled
                .iter()
                .any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "answering is not stopping short: {settled:?}"
        );
        assert!(state.is_idle());
    }

    /// **A continuation that ran nothing did stop short** — nobody asked
    /// it anything; it was carrying on its own work and ended without a
    /// `finish(text)`. Off by default now, so the branch has to be asked
    /// for it.
    ///
    /// Kept because the measurement that turned it off is a *ratio*,
    /// not a refutation: one rescue in thirteen. If that ratio is ever
    /// worth having back, this is the test that says it still works.
    /// **And by default it is not asked again.** Twelve of the thirteen
    /// times this notice fired across 382 kept runs, the reply it woke
    /// was `done();` — a model that had finished the work and had not
    /// said so in the syntax that rests a branch. Those runs would have
    /// rested with the task complete and passed anyway, so the prod
    /// bought a round trip and changed nothing.
    ///
    /// The other two branches of `stopped_short` are untouched and have
    /// their own measurements: an empty completion, and a tool call in
    /// a syntax this harness does not read.
    #[test]
    fn a_continuation_that_ran_nothing_rests_by_default() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("let a = 1;")))
            .unwrap();
        drain(&mut state, &mut tree, out);

        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(crate::host::scripted_markdown(
                    "That is the lay of the land.\n",
                )),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            !settled
                .iter()
                .any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "the branch rests: {settled:?}"
        );
        assert!(!state.needs_prompt(&tree));
    }

    /// **A `finish()` that said nothing is not honoured**, and the next
    /// request says why. The pairing used to be the verb's arity —
    /// `finish(text)` could not be silent — and with the answer moved
    /// back to `tell` it is enforced here instead.
    #[test]
    fn a_silent_finish_does_not_rest_the_branch() {
        let mut c = Conversation::new();
        c.user("is it green?");
        let r = c.reply("```js\nfinish();\n```\n");
        assert!(
            !r.rests,
            "it told nobody anything, so the branch carries on"
        );
        assert!(
            c.runner()
                .request_tail(c.tree())
                .is_some_and(|t| t.contains("said nothing to anybody")),
            "and the next request says why"
        );

        // Say something and it is honoured.
        let r = c.reply("```js\ntell(\"green.\");\nfinish();\n```\n");
        assert!(r.rests, "whatever finishes, speaks — and this one did");
        assert!(
            !c.runner()
                .request_tail(c.tree())
                .is_some_and(|t| t.contains("said nothing to anybody")),
            "the note is true of one request and gone the next"
        );
    }

    /// **The mid-work line appears once work is under way, and not
    /// before.** Before any cell has run, a prose-only reply is the
    /// right answer to a question; after one has, it stops the work
    /// silently. The tail says which situation this is.
    #[test]
    fn the_mid_work_line_waits_until_a_cell_has_run() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "what is a mutex?");
        let tail = state.request_tail(&tree).expect("a tail");
        assert!(
            !tail.contains("run since you were last spoken to"),
            "nobody has run anything yet: {tail}"
        );

        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("let a = 1;")))
            .unwrap();
        drain(&mut state, &mut tree, out);
        let tail = state.request_tail(&tree).expect("a tail");
        assert!(
            tail.contains("run since you were last spoken to"),
            "a cell has run: {tail}"
        );

        // The person speaks again and it is a fresh question, so the
        // line goes away until work restarts.
        user_post(&mut state, &mut tree, "and a semaphore?");
        let tail = state.request_tail(&tree).expect("a tail");
        assert!(
            !tail.contains("run since you were last spoken to"),
            "a new post resets it: {tail}"
        );
    }

    /// **The reply-shape line rides the tail, and only where it is
    /// apt.** It says "you may simply answer", which is right when
    /// somebody has just asked and wrong when the branch is carrying on
    /// its own work — the card is right about the second case ("you are
    /// not trying to finish the task in one reply").
    #[test]
    fn the_reply_shape_line_is_on_the_answering_request_only() {
        let (mut tree, mut state) = setup_under();
        state.reply_shape_tail = true;
        user_post(&mut state, &mut tree, "what is a mutex?");

        let tail = state
            .request_tail(&tree)
            .expect("every request carries a tail");
        assert!(
            tail.contains("no ```js block in it is a complete answer"),
            "somebody just asked: {tail}"
        );
        // Last in the request, and inside the final `User` message —
        // never its own trailing turn, which is the position that makes
        // it worth saying at all.
        let doc = state.document(&tree, TEST_BUDGET).with_tail(&tail);
        let last = doc.messages.last().expect("a rendered request");
        assert_eq!(last.role, crate::document::ChatRole::User);
        assert!(last.content.ends_with(&tail), "{}", last.content);

        // Now a reply that carries the work on, with nobody having
        // asked anything since.
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("let a = 1;")))
            .unwrap();
        drain(&mut state, &mut tree, out);
        let tail = state.request_tail(&tree).expect("a tail");
        // Its own wording, not the shared phrase: `WORK_UNDER_WAY`
        // also says "no ```js block", and says the complementary half
        // — these two never fire on the same request.
        assert!(
            !tail.contains("is a complete answer"),
            "nobody asked anything; this is the branch's own work: {tail}"
        );
    }

    /// **The tail is ephemeral: it rides whichever message is last and
    /// is never baked into one.** It is the only part of a request that
    /// is not recoverable from the log, so a copy left behind in a
    /// message would be invisible until it started contradicting a
    /// later one — and the whole reason it can carry a right-now fact
    /// is that next request re-emits it at the new end.
    #[test]
    fn the_tail_moves_and_leaves_nothing_behind() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "what is in this directory?");

        let first = state.render_messages_for_test(&tree);
        let asked = first.messages.last().expect("a request").content.clone();
        assert!(asked.contains("## RIGHT NOW"), "{asked}");

        // A reply, then a second question — so the message that carried
        // the tail is no longer the last one.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("tell(\"three\");")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        user_post(&mut state, &mut tree, "present as a table");

        let second = state.render_messages_for_test(&tree);
        assert_eq!(
            second
                .messages
                .iter()
                .filter(|m| m.content.contains("## RIGHT NOW"))
                .count(),
            1,
            "exactly one tail in the request, at its end"
        );
        let last = second.messages.last().expect("a request");
        assert!(last.content.contains("## RIGHT NOW"), "{}", last.content);
        assert_eq!(last.role, crate::document::ChatRole::User);
    }

    /// And it is off unless asked for.
    #[test]
    fn the_reply_shape_line_is_off_by_default() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "what is a mutex?");
        let tail = state.request_tail(&tree).expect("a tail");
        assert!(!tail.contains("is a complete answer"), "{tail}");
    }

    #[test]
    fn a_continuation_that_ran_nothing_is_asked_again() {
        let (mut tree, mut state) = setup_under();
        state.nudge_when_nothing_ran = true;
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("let a = 1;")))
            .unwrap();
        drain(&mut state, &mut tree, out);

        // No post in between: this reply continues the branch's own work.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(crate::host::scripted_markdown(
                    "Next I will fix the config.\n",
                )),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            settled
                .iter()
                .any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "announcing work is not doing it: {settled:?}"
        );

        // Bounded: a second one in a row rests.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(crate::host::scripted_markdown("Still thinking.\n")),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            !settled
                .iter()
                .any(|o| matches!(o, StepOutput::LlmRequest(_))),
            "twice in a row rests: {settled:?}"
        );
    }

    #[test]
    fn a_foreign_tool_call_is_recognised_by_shape_not_by_guesswork() {
        for text in [
            "<tool_call>\n<function=bash>",
            "ok <function=read_file> ok",
            "<parameter=command>",
            "<invoke name=\"bash\">",
            "[TOOL_REQUEST]",
            "{\"tool_calls\": []}",
        ] {
            assert!(foreign_tool_call(text), "{text:?}");
        }
        for text in [
            "```js\nawait tools.bash(\"ls\");\n```",
            "the function signature is f(x)",
            "a < b and c > d",
            "I called the tool and it worked",
            "",
        ] {
            assert!(!foreign_tool_call(text), "{text:?}");
        }
    }

    /// A cell that does not compile stops the run — but the cells
    /// before it have already run, and what they did stays in the log.
    #[test]
    fn a_later_cell_that_does_not_compile_leaves_the_earlier_ones_standing() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let reply = "```js\nconsole.log(\"the first cell ran\");\n```\n\n\
                     ```js\nthis is not javascript\n```\n";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(reply)))
            .unwrap();
        drain(&mut state, &mut tree, out);

        let kinds = payload_kinds(&state, &tree);
        assert!(
            kinds.contains(&"Console"),
            "cell 0's effect stands: {kinds:?}"
        );
        assert!(
            kinds.contains(&"Handback"),
            "and the run has an outcome: {kinds:?}"
        );
        // The effects stand in the *log* — that is what "partial
        // progress" means. The report renders the compile failure,
        // because that is what the repair loop has to act on.
        let report = last_report(&state, &tree);
        assert!(
            report.contains("Expected a semicolon") || report.contains("compile"),
            "the report names the failure: {report}"
        );
    }

    /// **A top-level `return` ends the reply, and what it returned is
    /// what the next one is told** — carried end to end, from the cell
    /// to the report the next request renders.
    #[test]
    fn a_cell_that_returns_hands_the_value_to_the_next_reply() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let reply = "```js\nreturn { done: true };\n```\n";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(reply)))
            .unwrap();
        drain(&mut state, &mut tree, out);

        let report = last_report(&state, &tree);
        assert!(report.contains("done"), "{report}");
    }

    /// `finish(text)` does not stop anything (D8): the cells after it still
    /// **`finish()` stops nothing, and `return` stops everything.**
    ///
    /// The verb halted for a while, which needed a card paragraph and
    /// a worked bug to explain; before that it was a flag that let
    /// every block after it run, and 51 programs in the kept corpus
    /// wrote real statements after it, including a `replace_file` made
    /// once the program had already decided it was finished. It is a
    /// flag again — but now there is a verb whose whole job is
    /// stopping, so "say it is finished" and "stop here" are written
    /// separately and neither has to explain the other.
    #[test]
    fn finish_in_cell_0_does_not_stop_the_later_cells() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply(
            "```js\ntell(\"ok\"); finish();\n```\n\n\
             ```js\nconsole.log(\"still runs\");\n```\n",
        );
        assert_eq!(
            r.printed,
            ["still runs"],
            "a flag stops nothing: the block after it runs"
        );
        assert!(r.rests, "and the branch still rests, because it spoke");
        assert_eq!(r.tells, ["ok"], "and the answer reached the person");
    }

    /// The existing transport is untouched: a plain program under
    /// `Transport::Program` is compiled as one program, as it always
    /// was.
    #[test]
    fn the_program_transport_still_compiles_the_whole_completion() {
        let (mut tree, mut state) = setup();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("console.log(\"plain program\");")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(
            payload_kinds(&state, &tree),
            [
                "Agent", "Post", "Reply", "Part", "ReplyEnd", "Handback", "Console"
            ]
        );
    }

    // ── streaming: execute as the fences close (D11, D15, 25.5) ────

    /// Push `reply` through the streaming door in chunks, the way the
    /// session loop does, without ending the completion. All one
    /// generation, so they assemble into one reply.
    const TEST_EPOCH: u64 = 1;

    fn stream_chunks(state: &mut Runner, tree: &mut Tree, chunks: &[&str]) -> Vec<StepOutput> {
        stream_chunks_at(state, tree, TEST_EPOCH, chunks)
    }

    fn stream_chunks_at(
        state: &mut Runner,
        tree: &mut Tree,
        epoch: u64,
        chunks: &[&str],
    ) -> Vec<StepOutput> {
        let mut out = Vec::new();
        for chunk in chunks {
            let produced = state.notebook_stream(tree, epoch, chunk).unwrap();
            out.extend(drain(state, tree, produced));
        }
        out
    }

    /// **A cell runs the moment its fence closes**, with the completion
    /// still arriving (D11). Cell 0's effect is in the log before cell
    /// 1's text has been written at all.
    #[test]
    fn a_cell_runs_before_the_next_ones_fence_arrives() {
        let mut c = Conversation::new();
        c.user("go");

        let so_far = c.chunk("Looking now.\n\n```js\ntell(\"cell 0 ran\");\n```\n");

        // Cell 0 has run, and nothing of cell 1 exists yet. A `tell` is
        // logged at dispatch, so it is the effect that shows mid-reply;
        // the console is a diagnostic stream drained at the run's end.
        assert_eq!(
            so_far.kinds,
            ["Reply", "Part", "Part", "Call", "Call", "Result", "Result"],
            "cell 0 ran, and was delivered, while the reply was still open"
        );
        assert_eq!(so_far.ended, Ending::Running, "but the run has not ended");
        assert_eq!(c.status(), "running");

        // Now the rest arrives.
        c.chunk("\nAnd the second.\n\n```js\ntell(\"cell 1 ran\");\n```\n");
        let whole = c.end_reply();
        assert_eq!(whole.tells, ["cell 0 ran", "cell 1 ran"]);
    }

    /// Prose lands as its own `Send` as the reply streams, so the person
    /// reads the narration beside the effects rather than after them.
    #[test]
    fn streamed_prose_is_delivered_before_the_cell_below_it_runs() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        state.phase = Phase::AwaitingLlm;

        stream_chunks(
            &mut state,
            &mut tree,
            &["I will read both files.\n\n```js\ntell(\"read\");\n```\n"],
        );
        assert_eq!(
            payload_kinds(&state, &tree),
            ["Agent", "Post", "Reply", "Part", "Part", "Call", "Call"],
            "the paragraph is delivered, then the cell runs and speaks"
        );
    }

    /// **A trap cancels the generation still in flight** (D11): the VM is
    /// parked, so no later cell can run until a handler resumes it, and
    /// every token still being generated is waste.
    #[test]
    fn a_trap_in_a_cell_asks_for_the_generation_to_be_cancelled() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        state.phase = Phase::AwaitingLlm;

        stream_chunks(
            &mut state,
            &mut tree,
            &["```js\nundefined_thing_here();\n```\n"],
        );
        assert!(!state.parked.is_empty(), "the trap parked the run");
        assert!(
            state.notebook_cancels_generation(),
            "so the harness stops reading the completion"
        );
    }

    /// A `raise` is the same: the run is parked until a handler decides.
    /// **The reply's text survives its own run.** A reply whose cells
    /// all finish before the completion ends leaves `Phase::Running`
    /// first, and the text used to be read off the run at that point —
    /// so it came back empty. `document::render` treats an empty
    /// `Completion.text` as "no verbatim reply" and falls back to
    /// grouping the `Turn`s, which shows the model bare cell source
    /// with its prose and fences stripped: the very thing the verbatim
    /// rendering exists to prevent. Measured 2026-09-18: empty on 12 of
    /// 44 completions, and all twelve had their outcome logged
    /// immediately before.
    #[test]
    fn a_completed_reply_still_records_its_text() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        state.phase = Phase::AwaitingLlm;

        let reply = "Here is the answer.\n\n```js\ntell(\"ok\"); finish();\n```\n";
        stream_chunks(&mut state, &mut tree, &[reply]);
        // Stand in for the run having already ended, which is what the
        // real ordering does — every one of the twelve empty-text
        // completions logged `Return, Console, Completion`, so the run
        // was gone by the time the text was wanted. Forcing the phase
        // here pins the invariant (the text outlives the run) without
        // depending on the upstream ordering that produces it.
        state.phase = Phase::Idle;
        state
            .notebook_stream_end(&mut tree, false, None, None)
            .unwrap();

        let text = state
            .agent_segment(&tree)
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Part { part, .. } => match part {
                    crate::types::Part::Prose(t) | crate::types::Part::Cell(t) => Some(t.clone()),
                    crate::types::Part::Thinking(_) => None,
                },
                _ => None,
            })
            .collect::<String>();
        // **The invariant, end to end**: the reply's parts on the log
        // concatenate back to the completion that produced them (28).
        assert_eq!(text, reply, "verbatim, prose and fences included");
    }

    /// **A raise does not** (28). The model knew it was asking; the
    /// cells it wrote after the raise were not written on a premise the
    /// raise falsified, and the card promises they run once it is
    /// answered — *"the blocks after this one do not run until it is
    /// answered"*, not *"are never written"*.
    ///
    /// Cancelling here also made the semantics depend on provider
    /// speed: the same reply ran its later cells or lost them
    /// altogether depending on whether they had streamed in yet.
    #[test]
    fn a_raise_in_a_cell_lets_the_generation_finish() {
        let mut c = Conversation::new();
        c.user("go");
        let parked = c.chunk("```js\nraise(\"which\");\n```\n");
        assert!(matches!(parked.ended, Ending::Raised { .. }));
        assert_eq!(c.status(), "suspended");

        // The rest arrives while the run is parked, and lands on the
        // log as parts of the same reply…
        c.chunk("\n```js\nconsole.log(\"after the raise\");\n```\n");
        c.end_reply();
        assert_eq!(
            c.status(),
            "suspended",
            "still parked: the reply ended, the run did not"
        );

        // …and runs when the raise is answered.
        let after = c.resume(json!("that one"));
        assert_eq!(after.printed, ["after the raise"]);
    }

    /// A post arriving is the same: it parks the run (rule B), and
    /// falsifies nothing the model wrote.
    #[test]
    fn a_post_arriving_lets_the_generation_finish() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        state.phase = Phase::AwaitingLlm;
        stream_chunks(
            &mut state,
            &mut tree,
            &["```js\nawait tools.slow();\n```\n"],
        );
        // A post the branch has not been shown, logged mid-run: the
        // next fuel slice parks on it.
        user_post(&mut state, &mut tree, "one more thing");
        let _ = state
            .step(&mut tree, StepInput::Tick { fuel: TICK_FUEL })
            .unwrap();
        assert!(!state.parked.is_empty(), "the post parked the run");
        assert!(
            !state.notebook_cancels_generation(),
            "a message arriving falsifies nothing the model wrote"
        );
    }

    // `finish_in_a_cell_skips_the_cells_after_it` lives in `testkit`
    // now — the same facts, written against the reply rather than
    // against the log, at a fifth of the length.

    /// **Truncation becomes partial progress** (D11). The cell that
    /// closed ran and its effects stand; the half-written one after it
    /// was never a cell at all, and the turn is reported as truncated so
    /// the next completion knows it was cut off.
    #[test]
    fn a_mid_stream_truncation_keeps_what_ran_and_reports_partial() {
        let mut c = Conversation::new();
        c.user("go");
        c.chunk("```js\ntell(\"cell 0 ran\");\n```\n\nNext I will\n\n```js\nawait tools.read_fi");
        // The token budget ran out here.
        let r = c.truncate_reply();

        assert_eq!(r.tells, ["cell 0 ran"], "cell 0's effects stand");
        // Truncation is a fact about the *text*, not about the program
        // (28). Every cell that arrived ran to the end, so the run
        // completed; it is `ReplyEnd` that says the reply was cut off,
        // and the document renders that marker where the text stops so
        // the model can see why it seems to end mid-sentence.
        assert_eq!(
            r.ended,
            Ending::Completed(None),
            "the cells that arrived all ran"
        );
        assert_eq!(
            r.reply_ended,
            Some(crate::types::ReplyEnd::Truncated),
            "and the reply says it was cut off: kinds {:?}",
            r.kinds
        );
    }

    /// **What the provider's chunking may and may not change.**
    ///
    /// It may change the *interleaving*: a part is logged when it is
    /// seen, and a call when it runs, so a reply that arrives whole has
    /// all its parts on the log before its first cell executes, while a
    /// reply that dribbles in line by line alternates. Both are honest
    /// records of arrival order, and 28's worked log is the streamed
    /// one.
    ///
    /// It may not change the reply. The parts are the same parts, in
    /// the same order, concatenating to the same bytes — that is the
    /// invariant the whole phase exists for — and the same calls run
    /// off them.
    #[test]
    fn chunk_boundaries_do_not_change_the_reply() {
        let reply = "One.\n\n```js\nlet a = 1;\n```\n\nTwo.\n\n```js\na = 2;\n```\n\nThree.\n";
        let run = |chunks: &[&str]| {
            let (mut tree, mut state) = setup_under();
            user_post(&mut state, &mut tree, "go");
            state.phase = Phase::AwaitingLlm;
            stream_chunks(&mut state, &mut tree, chunks);
            let out = state
                .step(&mut tree, StepInput::LlmResponse(llm_program("")))
                .unwrap();
            drain(&mut state, &mut tree, out);
            parts_and_calls(&state, &tree)
        };
        let whole = run(&[reply]);
        let split = run(&reply.split_inclusive('\n').collect::<Vec<_>>());
        assert_eq!(whole, split);
        assert_eq!(whole.0.concat(), reply, "and the parts are the reply");
    }

    /// One `LlmTurn` carrying usage, for the transports that read it.
    fn llm_program_with_usage(source: &str, completion: u64) -> LlmTurn {
        LlmTurn {
            usage: Some(crate::host::Usage {
                completion,
                ..Default::default()
            }),
            ..llm_program(source)
        }
    }

    /// **The reply rides along with the count it arrived with.**
    /// `usage.prompt` describes the request that was *sent*; by the
    /// time it arrives the reply it paid for is already in the
    /// document, and the request after this one carries both. Both
    /// halves are counted numbers off the same trailer, so adding them
    /// costs nothing and guesses nothing.
    ///
    /// Reasoning is the part that does not survive: `document.rs` drops
    /// `Part::Thinking`, so those tokens are charged for once and never
    /// sent again.
    #[test]
    fn the_measured_size_includes_the_reply_but_not_its_thinking() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(LlmTurn {
                    usage: Some(crate::host::Usage {
                        prompt: 10_000,
                        completion: 900,
                        reasoning: 700,
                        cached: 0,
                        window: None,
                    }),
                    ..llm_program("tell(\"hi\");")
                }),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(
            state.next_prompt_floor,
            Counted::Floor(10_200),
            "the prompt, plus the 200 tokens of it that were not thinking"
        );
    }

    /// **A reply's cost is recorded exactly once, however many cells it
    /// held.** The usage belongs to the *completion*, not to any one
    /// cell, and a three-cell reply is one completion.
    ///
    /// It cannot live on a `Turn` here: every one of this reply's `Turn`s
    /// is written before its cell runs — while the completion is still
    /// streaming — and the log is append-only, so by the time the
    /// provider says what the completion cost there is no `Turn` left to
    /// put it on.
    #[test]
    fn a_three_cell_reply_records_its_usage_exactly_once() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let reply = "```js\nconsole.log(\"a\");\n```\n\n\
                     ```js\nconsole.log(\"b\");\n```\n\n\
                     ```js\nconsole.log(\"c\");\n```\n";
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program_with_usage(reply, 321)),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let costs: Vec<u64> = state
            .agent_segment(&tree)
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::ReplyEnd { usage, .. } => Some(usage.completion),
                _ => None,
            })
            .collect();
        assert_eq!(costs, vec![321], "one completion, one cost");

        let (replies, cells) = state
            .agent_segment(&tree)
            .iter()
            .fold((0, 0), |(r, c), e| match &e.payload {
                EventPayload::Reply => (r + 1, c),
                EventPayload::Part {
                    part: crate::types::Part::Cell(_),
                    ..
                } => (r, c + 1),
                _ => (r, c),
            });
        assert_eq!((replies, cells), (1, 3), "three cells, one reply, one cost");
    }

    /// The same through the *streaming* door, which is the one a real
    /// session uses and the one the figure used to fall through.
    #[test]
    fn a_streamed_reply_records_its_usage_exactly_once() {
        let mut c = Conversation::new();
        c.user("go");
        c.chunk("Reading it.\n\n```js\nlet n = 1;\n```\n");
        c.chunk("\nNow the sum.\n\n```js\ntell(`n is ${n + 41}`);\n```\n");
        // Every cell has already run, so there is no cell left for the
        // figure to ride in on.
        let r = c.end_reply_costing(654);

        assert_eq!(
            r.usage.iter().map(|u| u.completion).collect::<Vec<_>>(),
            [654],
            "one completion, one cost"
        );
        assert_eq!(r.tells, ["n is 42"], "and the reply really ran");
    }

    /// **`programs` counts round trips, not cells.** This is the other
    /// number 25.8 compares, and counting `Turn`s would report a
    /// three-cell reply as three turns of chat-mode drift when nothing
    /// drifted.
    #[test]
    fn a_three_cell_reply_scores_as_one_program() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let reply = "```js\nconsole.log(\"a\");\n```\n\n\
                     ```js\nconsole.log(\"b\");\n```\n\n\
                     ```js\nconsole.log(\"c\");\n```\n";
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program_with_usage(reply, 321)),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let score = crate::score::score(&tree);
        assert_eq!(score.programs, 1, "one completion, one program");
        assert_eq!(score.completion_out, 321, "and its cost is summed");
    }

    /// And a log that records no completions at all — a scripted run
    /// nobody was billed for — still counts its turns, because there
    /// every `Turn` really is its own round trip.
    #[test]
    fn a_log_with_no_recorded_cost_still_counts_its_turns() {
        let (mut tree, mut state) = setup();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("tell(\"ok\"); finish();\n")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let score = crate::score::score(&tree);
        assert_eq!(score.programs, 1);
        assert_eq!(score.completion_out, 0, "nobody was billed");
    }

    // ── a reply reads back as what it was (25.8 follow-up) ──────────

    /// Render the conversation of a scripted notebook reply.
    fn notebook_conversation(reply: &str) -> Vec<(crate::document::ChatRole, String)> {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(reply)))
            .unwrap();
        drain(&mut state, &mut tree, out);
        let doc = crate::document::render(&tree, &state.spine, 100_000);
        doc.conversation()
            .iter()
            .map(|m| (m.role, m.content.clone()))
            .collect()
    }

    /// **`history.note` hands back the row's id.** A program that
    /// wants to name what it just wrote should not have to wait a turn
    /// to read the annotation off its own source — two live programs
    /// invented an identifier rather than do without, and died on it.
    #[test]
    fn appending_to_history_returns_the_row_it_made() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply(
            "```js\nconst id = history.note({ a: 1 });\nconsole.log(typeof id, id);\n```\n",
        );
        assert_eq!(
            r.printed,
            [format!("number {}", r.row().id.as_u64())],
            "the id of the row it just wrote, as a number it can pass to fetch"
        );
    }

    /// **A marker the model wrote is replaced, not doubled.** It reads
    /// these in its own turns and writes them back — that is what
    /// `imitated_annotation` exists for on the `←` side, after a live
    /// run emitted four invented ids and the pass added the four real
    /// ones beside them. A `↓` it wrote is just as wrong, and a
    /// doubled marker is the example it imitates next turn.
    #[test]
    fn a_block_marker_the_model_wrote_is_replaced() {
        let rows =
            notebook_conversation("↓ history[999]\nReading it first.\n\n```js\nlet n = 1;\n```\n");
        let assistant: Vec<&String> = rows
            .iter()
            .filter(|(r, _)| *r == crate::document::ChatRole::Assistant)
            .map(|(_, c)| c)
            .collect();
        let text = assistant[0];
        assert!(!text.contains("999"), "the invented id is gone: {text}");
        assert_eq!(
            text.matches("↓ history[").count(),
            2,
            "one marker per block, not one per block plus a forgery: {text}"
        );
        assert!(text.contains("Reading it first."), "{text}");
    }

    /// **A marker names a row, so the row has to answer.** Every block
    /// of a reply carries `↓ history[N]` above it; an id the model can
    /// see and cannot read back is the papercut the `append`/`fetch`
    /// round-trip was, and this is the arm that keeps the marker
    /// honest.
    #[test]
    fn a_block_of_a_reply_fetches_back_as_its_own_text() {
        let mut c = Conversation::new();
        c.user("go");
        let r = c.reply("Reading it first.\n\n```js\nlet n = 1;\n```\n");

        let fetched: Vec<serde_json::Value> = r.parts.iter().map(|id| c.fetch(*id)).collect();
        assert_eq!(
            fetched,
            vec![
                json!("Reading it first.\n\n"),
                json!("```js\nlet n = 1;\n```\n"),
            ],
            "each block comes back as itself, fences and all"
        );
    }

    /// **The model's own past turn is what it generated.** Not rebuilt
    /// from pieces, not re-fenced, not reassembled — the completion
    /// verbatim, with only the documented annotate-and-snip pass on top.
    ///
    /// It used to be rendered from the pieces: one assistant message per
    /// *cell*, each bare JavaScript, with the prose showing up in the
    /// user-role history as `[3] you told user: …`. So every turn the
    /// model was given a card saying "your reply is markdown and the code
    /// blocks in it run", worked examples in markdown, and then its own
    /// history as a series of bare programs. Its context taught it the
    /// opposite of its card.
    #[test]
    fn a_reply_reads_back_as_the_markdown_it_was() {
        let reply = "Reading it first.\n\n\
                     ```js\nlet n = 1;\n```\n\n\
                     Now the sum.\n\n\
                     ```js\nconsole.log(n + 41);\n```\n\n\
                     That is it.\n";
        let rows = notebook_conversation(reply);
        let assistant: Vec<&String> = rows
            .iter()
            .filter(|(r, _)| *r == crate::document::ChatRole::Assistant)
            .map(|(_, c)| c)
            .collect();

        assert_eq!(assistant.len(), 1, "one message per reply, not per cell");
        // **The markers are the only difference.** Each block carries a
        // `↓ history[N]` line above it so the model can name it; lift
        // those and what is left is the completion, byte for byte — no
        // re-fencing, no reassembly, no normalisation.
        let stripped: String = assistant[0]
            .split_inclusive('\n')
            .filter(|l| !l.starts_with("↓ history["))
            .collect();
        assert_eq!(
            stripped, reply,
            "byte-identical to what the model generated, once the markers are lifted"
        );
        assert_eq!(
            assistant[0].matches("↓ history[").count(),
            5,
            "one marker per block — three prose, two cells: {}",
            assistant[0]
        );
    }

    /// The prose is in the assistant turn, so it does not also appear as
    /// a history row — it would be the same words twice, in the other
    /// voice. A `tell` keeps its row: it is not in the reply's text, only
    /// its call is.
    #[test]
    fn prose_leaves_no_row_but_a_tell_still_does() {
        let reply = "Some narration here.\n\n```js\ntell(\"the finding\");\n```\n";
        let rows = notebook_conversation(reply);
        let user: String = rows
            .iter()
            .filter(|(r, _)| *r == crate::document::ChatRole::User)
            .map(|(_, c)| c.clone())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !user.contains("Some narration here"),
            "the prose is in the assistant turn already:\n{user}"
        );
        assert!(
            user.contains("you told user: the finding"),
            "a tell keeps its row:\n{user}"
        );
    }

    /// **The gate case: a long literal in the *second* cell.**
    ///
    /// A `Call::site` is cell-local (D1) — the cell's offset is
    /// subtracted at log time — so those offsets do not index the whole
    /// reply. Rendering the reply without adding the offset back cut at
    /// bytes that happen to work for cell 0 and land anywhere for cell 1.
    /// The offset is derived from the stored text by the same splitter
    /// that produced the cells, so it is a fact about the bytes rather
    /// than a field anyone has to keep in step.
    #[test]
    fn a_long_literal_in_the_second_cell_snips_at_the_right_bytes() {
        let filler = "x".repeat(400);
        let reply = format!(
            "First, something short.\n\n\
             ```js\ntell(\"short one\");\n```\n\n\
             Now the long one.\n\n\
             ```js\ntell(\"{filler}\");\n```\n"
        );
        let rows = notebook_conversation(&reply);
        let assistant: Vec<&String> = rows
            .iter()
            .filter(|(r, _)| *r == crate::document::ChatRole::Assistant)
            .map(|(_, c)| c)
            .collect();
        assert_eq!(assistant.len(), 1);
        let shown = assistant[0];

        // The long literal is gone, replaced by its row reference — and
        // the *second* cell is where the replacement happened.
        assert!(
            !shown.contains(&filler),
            "the long literal should have been snipped:\n{shown}"
        );
        assert!(
            shown.contains("← snipped - history["),
            "and replaced by its row:\n{shown}"
        );
        // Everything around it is intact: the prose, both fences, and
        // the short tell that must *not* have been cut.
        assert!(shown.contains("First, something short."), "{shown}");
        assert!(shown.contains("Now the long one."), "{shown}");
        assert!(
            shown.contains("tell(\"short one\")"),
            "cell 0's own literal is short and stays:\n{shown}"
        );
        assert_eq!(
            shown.matches("```js").count(),
            2,
            "both fences kept:\n{shown}"
        );
    }

    /// A reply that never reported a completion — truncated mid-stream,
    /// or a log written before the text was stored — still renders. The
    /// cells are all that is left, so they render as themselves, which
    /// is the behaviour this replaced.
    #[test]
    fn a_reply_with_no_stored_text_falls_back_to_its_cells() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        // A truncated reply logs its cells and no completion text.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(LlmTurn {
                    source: "```js\nconsole.log(\"ran\");\n```\n".into(),
                    thinking: None,
                    truncated: true,
                    usage: None,
                    reply: None,
                }),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        let doc = crate::document::render(&tree, &state.spine, 100_000);
        let assistant: Vec<&String> = doc
            .conversation()
            .iter()
            .filter(|m| m.role == crate::document::ChatRole::Assistant)
            .map(|m| &m.content)
            .collect();
        assert_eq!(assistant.len(), 1);
        assert!(
            assistant[0].contains("console.log(\"ran\")"),
            "the cell still renders: {:?}",
            assistant[0]
        );
    }

    /// A scripted completion that reasoned before answering.
    fn llm_program_thinking(source: &str, thinking: &str, reasoning: u64) -> LlmTurn {
        LlmTurn {
            source: source.into(),
            thinking: Some(thinking.into()),
            truncated: false,
            usage: Some(crate::host::Usage {
                prompt: 100,
                cached: 40,
                completion: 200,
                reasoning,
                window: None,
            }),
            reply: None,
        }
    }

    fn recorded_thinking(state: &Runner, tree: &Tree) -> Vec<String> {
        state
            .agent_segment(tree)
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Part {
                    part: crate::types::Part::Thinking(t),
                    ..
                } => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// **The model's reasoning is kept, once per completion.**
    ///
    /// It used to be dropped entirely on this transport: the streaming
    /// path takes the completion at `LlmDone` and discarded everything
    /// but `truncated` and `usage`, and every cell `Turn` is written with
    /// `thinking: None` because the reasoning has not finished arriving
    /// when the cell runs. A run that reasoned for 55KB scored 0.0 — which
    /// reads as "the model did not think" rather than "the harness did
    /// not keep it", and cost a 56-run comparison.
    #[test]
    fn a_notebook_reply_records_its_reasoning_exactly_once() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let reply = "```js\nlet n = 1;\n```\n\n```js\nconsole.log(n);\n```\n";
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program_thinking(reply, "weighing the options", 900)),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        assert_eq!(
            recorded_thinking(&state, &tree),
            vec!["weighing the options"],
            "two cells, one completion, one reasoning record"
        );
    }

    /// And through the streaming door, which is the one a real session
    /// uses and the one the reasoning fell through.
    #[test]
    fn a_streamed_notebook_reply_records_its_reasoning() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        state.phase = Phase::AwaitingLlm;

        stream_chunks(
            &mut state,
            &mut tree,
            &["Looking.\n\n```js\ntell(\"done\");\n```\n"],
        );
        // The reasoning arrives with the completion, after every cell
        // `Turn` is already on the log.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program_thinking("", "the long way round", 900)),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        assert_eq!(recorded_thinking(&state, &tree), vec!["the long way round"]);
    }

    /// **`agent score` reads it**, which is the number the comparison
    /// turns on — and the provider's own reasoning-token count comes
    /// through the same event, so both halves of the symptom are one
    /// cause.
    #[test]
    fn score_reads_a_notebook_replys_reasoning() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        let reply = "```js\nconsole.log(\"a\");\n```\n";
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program_thinking(reply, "a lot of thinking", 1234)),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let score = crate::score::score(&tree);
        assert_eq!(
            score.thinking_bytes,
            "a lot of thinking".len(),
            "the reasoning text is counted"
        );
        assert_eq!(
            score.reasoning_out, 1234,
            "and so is the provider's own token count"
        );
    }

    /// The program transport is unmoved: its reasoning still rides on the
    /// `Turn`, where it always did.
    #[test]
    fn the_program_transport_still_keeps_reasoning_on_the_turn() {
        let (mut tree, mut state) = setup();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program_thinking(
                    "tell(\"ok\"); finish();\n",
                    "still thinking",
                    55,
                )),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        assert_eq!(recorded_thinking(&state, &tree), vec!["still thinking"]);
        let score = crate::score::score(&tree);
        assert_eq!(score.thinking_bytes, "still thinking".len());
        assert_eq!(score.reasoning_out, 55);
    }

    // ── one lifecycle, closed on every path (D10) ───────────────────
    //
    // `streaming_notebook` was a `bool` with one clear site, on the one
    // path a *successful* generation takes. Four others end a generation
    // without producing an `LlmResponse` at all, and each left the flag
    // set with a stale `Run` — so the next reply was fed into the
    // previous reply's VM and D10's "nothing survives to the next
    // notebook" stopped being true. These drive the reply through the
    // streaming door, which is the one production uses.

    /// Feed a whole reply as one generation and end it, the way the
    /// session loop does.
    fn stream_reply(state: &mut Runner, tree: &mut Tree, epoch: u64, reply: &str) {
        state.phase = Phase::AwaitingLlm;
        stream_chunks_at(state, tree, epoch, &[reply]);
        let out = state
            .step(&mut *tree, StepInput::LlmResponse(llm_program("")))
            .unwrap();
        drain(state, tree, out);
    }

    /// Whether the branch's most recent reply trapped on a name the
    /// *previous* reply declared — which is what a leaked VM looks like.
    fn leaked_binding(state: &Runner, tree: &Tree) -> bool {
        state.agent_segment(tree).iter().any(|e| {
            matches!(&e.payload, EventPayload::Handback { how: cause, .. }
                if format!("{cause:?}").contains("already declared"))
        })
    }

    /// **A trap in reply N leaves reply N+1 a fresh VM** — the case the
    /// live arm died on: two consecutive replies each opening with
    /// `const files`, which is ordinary and legal, and the second getting
    /// `files is already declared` because it ran in the first one's
    /// frame.
    #[test]
    fn a_trap_does_not_leak_its_vm_into_the_next_reply() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");

        // Reply 1 declares `files` and then traps.
        stream_reply(
            &mut state,
            &mut tree,
            1,
            "```js\nconst files = 1;\nundefined_thing_here();\n```\n",
        );
        assert!(!state.parked.is_empty(), "it trapped");

        // Reply 2 declares the same name. A fresh VM has never heard of it.
        stream_reply(
            &mut state,
            &mut tree,
            2,
            "```js\nconst files = 2;\nconsole.log(\"second reply ran\");\n```\n",
        );
        assert!(!leaked_binding(&state, &tree), "reply 2 got a fresh VM");
        assert!(
            state.agent_segment(&tree).iter().any(|e| {
                matches!(&e.payload, EventPayload::Part { part: crate::types::Part::Cell(source), .. }
                    if source.contains("second reply ran"))
            }),
            "and its chunks were not dropped"
        );
    }

    /// The same for a generation that ends without any `LlmResponse` at
    /// all — a provider error, an interrupt, a cancelled generation. The
    /// next reply's chunks simply carry a different epoch, and that alone
    /// is what starts a fresh reply: nothing had to remember to reset.
    #[test]
    fn a_generation_that_never_completes_does_not_leak_its_vm() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");

        // Reply 1 streams and is never completed — no `LlmResponse` ever
        // arrives for it.
        state.phase = Phase::AwaitingLlm;
        stream_chunks_at(
            &mut state,
            &mut tree,
            1,
            &["```js\nconst files = 1;\n```\n"],
        );

        // Reply 2 arrives under the next generation.
        stream_reply(
            &mut state,
            &mut tree,
            2,
            "```js\nconst files = 2;\nconsole.log(\"still fine\");\n```\n",
        );
        assert!(!leaked_binding(&state, &tree), "reply 2 got a fresh VM");
    }

    /// **Every reply logs exactly one `Completion`**, including one whose
    /// generation was abandoned. "No event" and "no usage" are different
    /// states: a third of the live arm's completions went unlogged, and
    /// every per-reply metric was divided by the wrong number.
    #[test]
    fn every_reply_logs_exactly_one_completion() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");

        // One abandoned generation, one that traps, one that finishes.
        state.phase = Phase::AwaitingLlm;
        stream_chunks_at(&mut state, &mut tree, 1, &["```js\nlet a = 1;\n```\n"]);
        stream_reply(&mut state, &mut tree, 2, "```js\nboom_undefined();\n```\n");
        stream_reply(
            &mut state,
            &mut tree,
            3,
            "```js\nconsole.log(\"ok\");\n```\n",
        );

        let completions = state
            .agent_segment(&tree)
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::ReplyEnd { .. }))
            .count();
        assert_eq!(completions, 3, "three replies, three completions");
        assert_eq!(
            crate::score::score(&tree).programs,
            3,
            "and `agent score` counts three round trips"
        );
    }

    /// A reply that arrives whole rather than in chunks — a user taking
    /// the branch's turn, a client that does not stream — goes through
    /// the same door and lands in the same place. There is no second
    /// implementation for it to diverge from.
    ///
    /// "The same place" is the parts and the calls. `ReplyEnd` sits
    /// where the text stopped, which is before the first cell here and
    /// after it when the cells were running as the text arrived — see
    /// `chunk_boundaries_do_not_change_the_reply`.
    #[test]
    fn a_whole_reply_and_a_streamed_one_land_identically() {
        let reply = "Looking.\n\n```js\nlet n = 1;\n```\n\n```js\nconsole.log(n + 41);\n```\n";

        let whole = {
            let (mut tree, mut state) = setup_under();
            user_post(&mut state, &mut tree, "go");
            let out = state
                .step(&mut tree, StepInput::LlmResponse(llm_program(reply)))
                .unwrap();
            drain(&mut state, &mut tree, out);
            parts_and_calls(&state, &tree)
        };
        let streamed = {
            let (mut tree, mut state) = setup_under();
            user_post(&mut state, &mut tree, "go");
            stream_reply(&mut state, &mut tree, 1, reply);
            parts_and_calls(&state, &tree)
        };
        assert_eq!(whole, streamed);
        assert_eq!(whole.0.concat(), reply);
    }

    /// **A handler's reply runs.** It streams while the branch is
    /// `Suspended`, which the old phase guard refused outright — so every
    /// chunk was dropped in silence and a `raise` was never answered on
    /// this transport.
    #[test]
    fn a_handlers_reply_is_not_dropped_while_suspended() {
        let (mut tree, mut state) = setup_under();
        user_post(&mut state, &mut tree, "go");
        stream_reply(
            &mut state,
            &mut tree,
            1,
            "```js\nconst pick = raise(\"which\");\nconsole.log(`picked ${pick}`);\n```\n",
        );
        assert!(!state.parked.is_empty(), "it raised");

        // The handler's own reply arrives while the branch is still
        // suspended — which is the only time a handler's reply ever
        // arrives.
        let out = state
            .notebook_stream(&mut tree, 2, "```js\nconsole.log(\"handler ran\");\n```\n")
            .unwrap();
        drain(&mut state, &mut tree, out);

        assert!(
            state.agent_segment(&tree).iter().any(|e| {
                matches!(&e.payload, EventPayload::Part { part: crate::types::Part::Cell(source), .. }
                    if source.contains("handler ran"))
            }),
            "the handler's cell was compiled and logged, not dropped"
        );
    }
}
