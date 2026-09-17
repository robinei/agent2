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
    Diagnostic, InvokeCall, PromisePtr, RcStr, ResumeMode, SettleCall, StepResult, VM, VMError,
    Value, compile,
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
/// `append_history(value)` — logs an `EventPayload::Note` (22's "one
/// vocabulary decision" list; DESIGN.md "No exception"): what a mind
/// chose to remember for its own later turns, never re-derived and
/// never entering anyone else's context.
pub const TOOL_APPEND_HISTORY: &str = "append_history";
/// `fetch_history(id)` — id-addressable fetch from the log, resolved
/// synchronously without a host round trip. The **only** survivor of
/// the old budgeted-answer machinery (DESIGN.md "No exception": the
/// artifact model and id-addressable fetch stay; only the budgeted
/// copy-into-context goes).
///
/// Named `artifact` until 27.4, which is what it was while the thing
/// it read was a separate compartment — a menu of call results beside
/// the conversation. There is one history: the menu is those rows
/// rendered, `append_history` writes one, `remove_history` and
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

pub const TOOL_REMOVE_HISTORY: &str = "remove_history";
pub const TOOL_REWRITE_HISTORY: &str = "rewrite_history";
/// `done()` — the only thing that stops the loop. See `finish_program`'s
/// own comment for the polarity this inverts: completing a program is,
/// by itself, never enough to rest a branch anymore (on either
/// transport) — the dominant observed failure across every card variant
/// and both models is a program that does one step and stops,
/// abandoning the task, and a *default* rest made that the cheapest
/// accident to have. Calling `done()` makes stopping an act instead of
/// an omission: it costs nothing to call correctly, and forgetting it
/// costs one extra, self-correcting turn rather than an abandoned task.
pub const TOOL_DONE: &str = "done";
/// `list_agents()` — every agent in this subtree, with status, exactly
/// as the card has advertised since phase 20. It is served by the
/// host's `serve_agents` (the one implementation, shared with
/// ), because the status half is live session state
/// no single runner can see; `dispatch_settle` routes it there.
pub const TOOL_LIST_AGENTS: &str = "list_agents";

/// Open-post ids named in the request's trailing note before it says
/// "and N more" — a bounded line, like every other rendered bound.
const OPEN_NOTE_MAX_IDS: usize = 8;

/// The trailing presence line, the two ways round. It is deliberately
/// about the *client*, not the person: attached means a client is
/// connected, and claiming to know a human is reading would be a lie the
/// model would act on.
const PRESENT: &str = "Someone is attached to this session right now.";
const ABSENT: &str = "No one is attached to this session right now; a question to the user \
                      may sit unanswered for a long time.";

/// What the harness says when the user interrupts a running program and
/// has nothing else to add. It is a `tell` — the branch owes no answer —
/// and it exists so the wake has a cause event in the log.
///
/// Rewritten for code mode (23_ONE_AGENT.md A4 dec. 1): the old text
/// offered a menu of three named tools (`resume()` /
/// `run_program(source)` / "a plain reply"), none of which exist as
/// distinguished choices anymore. There is exactly one thing to say:
/// nothing was lost, and the next program is whatever the model writes.
const INTERRUPT_NOTICE: &str = "The user interrupted your program. It is paused at its last fuel slice — nothing \
     is lost, every completed call is already an artifact — and what happens next is \
     whatever program you write.";

/// Iteration cap for one `Tick`: each extra round requires a
/// settle-at-dispatch verb the harness could answer on the spot (a
/// `fetch_history` off the log, an `append_history`, a compaction edit)
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
}

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

enum Phase {
    /// No in-flight request; waiting for a `UserTurn` (or `kickoff`).
    Idle,
    /// An `LlmRequest` is out; waiting for `LlmResponse`.
    AwaitingLlm,
    /// A program is executing (waiting for `Tick`/`ToolResults`).
    Running(Run),
    /// A condition report went out; waiting for a direct
    /// [`Runner::resume`]/[`Runner::abandon`] call from the host.
    ///
    /// There is still only ever **one** parked run in this variant —
    /// `Phase` itself never represents nesting. What changed in C0a
    /// (23_ONE_AGENT.md) is where a *new* program starting on top of
    /// this one goes: not straight into `Cause::Abandoned`, but onto
    /// [`Runner::beneath`], a stack of exactly these frozen `(Run,
    /// ResumeWith)` pairs. That stack, not another dimension on this
    /// enum, is "the handler stack is a host-side structure of
    /// independently-stepped VMs" (DESIGN.md's load-bearing property):
    /// only `phase`'s own run is ever stepped, everything in `beneath`
    /// is inert data until its turn to be reactivated, and no VM here
    /// is ever on another VM's stack. (An earlier version of this
    /// comment said nesting would be built from multiple `Runner`
    /// instances instead — a design this file never actually needed:
    /// a handler's own `Turn` runs on the very same branch, so it
    /// belongs on the very same `Runner`.)
    Suspended(Run, ResumeWith),
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
    /// `Some` exactly while a compaction handler is running, which is
    /// also what makes the two verbs an error anywhere else: outside a
    /// compaction program there is nothing to add them to.
    compacting: Option<Vec<crate::compaction::CompactionOp>>,
    /// Whether `done()` (`TOOL_DONE`) was called by the program currently
    /// running — checked and reset by `finish_program`, which is the
    /// only reader. A program can call it and keep going (nothing else
    /// about execution changes), so this has to be recorded at dispatch
    /// time rather than inferred from anything the completion itself
    /// carries; `finish_program`'s own comment is where the polarity
    /// this exists to flip is explained.
    done: bool,
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
    /// frame is discarded (`Cause::Abandoned`) instead.
    beneath: Vec<(Run, ResumeWith, u64)>,
}

enum SuspendCause {
    Raise {
        condition: String,
        payload: Option<Value>,
    },
    Trapped(VMError),
    /// Someone spoke to the running program.
    Posted(Vec<EventId>),
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
            compacting: None,
            done: false,
            spine,
            agent,
            branch,
            phase: Phase::Idle,
            generation: 0,
            pending: HashMap::new(),
            dialect_card: String::new(),
            last_vm: None,
            status_transitions: Vec::new(),
            attached: false,
            // A branch handed to a fresh `Runner` has said nothing to
            // *this* session's LLM and is owed no prompt for its
            // history: a fork born at its `Fork` root speaks only when
            // spoken to, and a re-opened branch waits to be addressed.
            shown: leaf.as_u64(),
            beneath: Vec::new(),
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
        matches!(self.phase, Phase::Idle)
    }

    /// One-word phase description for agent lists / status lines.
    pub fn status(&self) -> &'static str {
        match self.phase {
            Phase::Idle => "idle",
            Phase::AwaitingLlm => "awaiting llm",
            Phase::Running(_) => "running",
            Phase::Suspended(..) => "suspended",
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
        // Running or suspended: the branch holds a VM. Awaiting an LLM: a
        // request is already out, and everything logged since will ride
        // the next one.
        if !matches!(self.phase, Phase::Idle) {
            return false;
        }
        // Any unseen `Post` is a cause — including one that arrived
        // during a generation, which the turn that just landed could not
        // have answered (its binding was fixed at `shown`), and including
        // a tell, which owes no answer but must still be seen.
        if !self.unseen_posts(tree).is_empty() {
            return true;
        }
        // The crash-recovery clause, `shown`-guarded like everything
        // else this rule checks: a genuinely new (unseen) outcome on the
        // most recent `Turn` is a cause even with no `Post` to find.
        self.last_turn_outcome(tree)
            .is_some_and(|id| id.as_u64() > self.shown)
    }

    /// Posts logged on this branch that its LLM has not been shown — the
    /// only thing `shown` is compared against, and what makes a post to a
    /// running program a condition at the next fuel slice (rule B).
    fn unseen_posts(&self, tree: &Tree) -> Vec<EventId> {
        self.agent_segment(tree)
            .iter()
            .filter(|e| e.id.as_u64() > self.shown)
            .filter(|e| matches!(e.payload, EventPayload::Message(Message::Post { .. })))
            .map(|e| e.id)
            .collect()
    }

    /// The most recent `Turn` on this path whose run has logged an
    /// outcome (`Return`/`Condition`) — regardless of `shown`. The two
    /// callers differ only in whether they apply that guard themselves:
    /// `needs_prompt` does (an already-shown outcome is not a fresh
    /// cause), `unrendered_cause` deliberately does not (reconciliation
    /// needs the fact independent of a `shown` a crash may have left
    /// pointing past it).
    fn last_turn_outcome(&self, tree: &Tree) -> Option<EventId> {
        let segment = self.agent_segment(tree);
        let at = segment
            .iter()
            .rposition(|e| matches!(e.payload, EventPayload::Message(Message::Turn { .. })))?;
        segment[at + 1..]
            .iter()
            .any(|e| {
                matches!(
                    e.payload,
                    EventPayload::Return { .. } | EventPayload::Condition { .. }
                )
            })
            .then_some(segment[at].id)
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
        let expects_reply = matches!(
            tree.resolve(match &path[at].payload {
                EventPayload::Message(m) => m,
                _ => return None,
            }),
            Message::Post { origin, .. } if matches!(origin.direct(), Some((_, _, true)))
        );
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
        match &self.phase {
            Phase::Running(run) | Phase::Suspended(run, _) => Some(&run.vm),
            _ => self.last_vm.as_ref(),
        }
    }

    /// Whether `vm()` is the live, executing program (vs a post-mortem
    /// snapshot).
    pub fn vm_is_live(&self) -> bool {
        matches!(self.phase, Phase::Running(_) | Phase::Suspended(..))
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
        let post = tree.append(
            &mut self.spine,
            EventPayload::Message(Message::Post { from, origin }),
        )?;
        // Logged on arrival either way — visible and crash-safe before
        // anything decides what to do about it. Whether it starts a turn
        // *now* is the trigger rule's call and nothing else's.
        if self.needs_prompt(tree) {
            self.phase = Phase::AwaitingLlm;
            return Ok((post, vec![self.render_request(tree)]));
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
            // A suspended branch is waiting on a direct host decision
            // (`resume`/`abandon`), not a rendered request — nothing to
            // interrupt here that isn't already the host's own call to
            // make.
            Phase::Suspended(..) => Ok(Vec::new()),
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
        reply: Option<String>,
    ) -> io::Result<Vec<StepOutput>> {
        let message = Message::Turn {
            author,
            source: source.clone(),
            thinking,
            usage,
        };
        let assistant_id = tree.append(&mut self.spine, EventPayload::Message(message))?;

        // `Transport::RunProgram`'s prose (`LlmTurn.reply`'s own doc) is
        // *the message a person reads* in that container, exactly what a
        // program's own `tell()` is in either container — so it goes out
        // through the identical `Call::Send { to: Address::User,
        // expects_reply: false, .. }` `dispatch_calls`'s `TOOL_TELL` arm
        // logs, via the same `StepOutput::Sends` door, rather than a
        // second, parallel way for text to reach a person. That is what
        // makes `agent score`'s `tells`/`silent` fields (folded straight
        // off `Call::Send` in the log) unable to tell a prose reply from
        // an ordinary `tell` apart. `site: 0`: no VM ran to have a call
        // site, the same convention `Cause::CompileFailed`/`Truncated`
        // below use for the same reason. Dispatched before the
        // truncated/compile branches below so it goes out regardless of
        // what (if anything) the turn's program does next.
        let mut out = Vec::new();
        if let Some(reply) = reply {
            let send = tree.append(
                &mut self.spine,
                EventPayload::Call(Call::Send {
                    to: Address::User,
                    text: reply,
                    input: serde_json::Value::Null,
                    expects_reply: false,
                    site: 0,
                }),
            )?;
            out.push(StepOutput::Sends(vec![send]));
        }

        if truncated {
            // **Never compile a truncated completion** (`Cause::Truncated`'s
            // own doc in `types.rs`): cut off wherever the token budget ran
            // out, it may still parse and run — half-written, on a program
            // the model never actually finished emitting — which is
            // strictly worse than a clean compile failure the repair loop
            // can see and retry. `host/deepseek.rs` only *detects* this
            // (off the SSE `finish_reason`); this is where detection
            // becomes an enforced, logged outcome, checked before
            // `start_program`/`compile` ever sees the text. Whatever was
            // previously suspended is untouched, same as a `CompileFailed`
            // handback — no VM ran here either.
            tree.append(
                &mut self.spine,
                EventPayload::Condition {
                    cause: Cause::Truncated,
                    site: 0,
                    stack: Vec::new(),
                    disposition: Disposition::Pushed,
                },
            )?;
            self.phase = Phase::AwaitingLlm;
            out.push(self.render_request(tree));
            return Ok(out);
        }

        // `Transport::RunProgram`'s other completion shape: prose with
        // **no** call at all — `host/deepseek.rs`'s `parse_sse` hands
        // back an empty `source` precisely when no `run_program` tool
        // call ever arrived (never for `Transport::Program`, whose
        // `source` is the model's whole response text and stays this
        // module's business regardless of length). That is the model's
        // final answer, already dispatched above as `reply`, so this
        // turn does not run — reaching `start_program` with an empty
        // program would ask `interp::compile` to compile nothing and
        // manufacture a spurious `Cause::CompileFailed` no repair loop
        // could ever fix. Gated on the transport, not just on
        // `source.is_empty()`, so `Transport::Program` — where an empty
        // response has always fallen through to `start_program` below —
        // is untouched.
        if source.is_empty()
            && crate::document::transport() == crate::document::Transport::RunProgram
        {
            self.phase = Phase::Idle;
            return Ok(out);
        }

        match self.start_program(tree, assistant_id, &source) {
            Ok(run) => {
                if let Phase::Suspended(old, resume_with) =
                    std::mem::replace(&mut self.phase, Phase::Idle)
                {
                    // Not discarded yet — **C0a** (23_ONE_AGENT.md):
                    // this new program might be the raise's own handler,
                    // "a program the LLM writes... whose return value
                    // *is* the restart" (DESIGN.md's thesis). Its return
                    // value is not known until `finish_program`, so the
                    // decision — resume, abandon, or (a genuine rewrite)
                    // neither — is made there, not here. Stashing rather
                    // than discarding is also what fixes the depth-
                    // rendering gap this comment used to carry: with
                    // nothing closing the old raise's scope until this
                    // program's own fate is known, its `Turn` and
                    // whatever it does before deciding fold at the
                    // raise's nested depth exactly like a handler's
                    // should, instead of `assistant_id` alone rendering
                    // at the wrong depth while its `Call`/`Return`
                    // rendered at the right one.
                    self.beneath.push((old, resume_with, self.generation));
                }
                self.generation += 1;
                self.phase = Phase::Running(run);
                self.note_status(assistant_id, ProgramStatus::Running);
                out.push(StepOutput::Working);
                Ok(out)
            }
            Err(message) => {
                // A compile error is an outcome like any other — no VM
                // was built, so this run has no console and no
                // artifacts, and whatever was previously suspended is
                // untouched (still there to resume once the model fixes
                // its program). This is also A5's (`host/mod.rs`) terminal
                // case: its own `Session::on_llm_response` repair loop
                // pre-checks `interp::compile` and re-asks up to
                // `MAX_REPAIR_ATTEMPTS` times with the diagnostic appended
                // *before* a source ever reaches here; once exhausted it
                // falls through to `step_branch`, and this is the real,
                // logged `Cause::CompileFailed` that produces. `document.rs`
                // renders it straight off the `Condition` event (no
                // separate "tool result" text to build).
                tree.append(
                    &mut self.spine,
                    EventPayload::Condition {
                        cause: Cause::CompileFailed { message },
                        site: 0,
                        stack: Vec::new(),
                        // No VM ran, so "did this push a handler frame"
                        // has no subject. Flagged in 23_ONE_AGENT.md A4's
                        // report as a default, not a determination —
                        // `Disposition`'s own safe choice, and it costs
                        // nothing here since a `CompileFailed` outcome is
                        // never itself something `resume` re-enters.
                        disposition: Disposition::Pushed,
                    },
                )?;
                self.phase = Phase::AwaitingLlm;
                out.push(self.render_request(tree));
                Ok(out)
            }
        }
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
        let Phase::Suspended(mut run, suspension) = std::mem::replace(&mut self.phase, Phase::Idle)
        else {
            panic!("Runner::resume called with nothing suspended — a host bookkeeping bug");
        };
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
    /// It logs `Cause::Abandoned`, and must: a run needs exactly one
    /// log-visible terminal or nothing downstream can be derived from the
    /// log alone. `Return` is the completing case; this is the other one.
    /// Logging nothing — which is what this did before — left the branch
    /// reading as permanently suspended, and left `depth_after`
    /// (`tree.rs`, driving `document::render`'s fold) never decrementing
    /// the depth the raise had incremented, so every later event rendered
    /// inside a scope nothing would ever close.
    pub fn abandon(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        let Phase::Suspended(run, _) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            panic!("Runner::abandon called with nothing suspended — a host bookkeeping bug");
        };
        self.note_status(run.program_id, ProgramStatus::Failed);
        // `Handover`: this condition closes the frame that decided, it
        // does not open one. `depth_after` matches the cause ahead of the
        // disposition for exactly this reason, so the value here is
        // belt-and-braces rather than load-bearing.
        tree.append(
            &mut self.spine,
            EventPayload::Condition {
                cause: Cause::Abandoned,
                site: 0,
                stack: Vec::new(),
                disposition: Disposition::Handover,
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
            if pending.generation != self.generation
                || !matches!(self.phase, Phase::Running(_) | Phase::Suspended(..))
            {
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
            "A call you issued has settled with no program awaiting it: [#{}] {label} → \
             {outcome}. Fetch the whole value with fetch_history({}). Nothing is owed in reply.",
            call.as_u64(),
            call.as_u64(),
        )
    }

    /// The VM a landing `Result` settles into — live while running or
    /// suspended (a suspended run's results land for later).
    fn settling_vm(&mut self) -> &mut VM {
        match &mut self.phase {
            Phase::Running(run) | Phase::Suspended(run, _) => &mut run.vm,
            _ => unreachable!("no VM to settle into"),
        }
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

    // ── program driving ─────────────────────────────────────────────

    /// Compile + bind the host const (`input`, from the agent's oldest
    /// still-open post). `Err` is the repair-loop report.
    fn start_program(
        &mut self,
        tree: &Tree,
        program_id: EventId,
        source: &str,
    ) -> Result<Run, String> {
        let program = compile(source).map_err(|diags| render_diags(source, &diags))?;
        // The whole `input` reaches the program even though the context
        // saw only a bounded preview of it.
        let vm = VM::for_program(program, self.spine.context().input(tree))
            .map_err(|e| format!("program setup failed: {}", e.message))?;
        Ok(Run { program_id, vm })
    }

    /// Drive the VM until it blocks on the host, suspends, finishes, or
    /// runs out of fuel. Each round runs one `step(fuel)` slice; only a
    /// synchronous unblock — a call the harness answered on the spot,
    /// through either dispatcher — earns another round.
    fn pump(&mut self, tree: &mut Tree, fuel: u64) -> io::Result<Vec<StepOutput>> {
        let mut out = Vec::new();
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
                Ok(StepResult::Done { value, unstarted }) => {
                    return self.finish_program(tree, value, unstarted, out);
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
    /// `done`, `answer`, `append_history`, `fetch_history`,
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
                TOOL_ASK | TOOL_TELL => {
                    let expects_reply = call.name == TOOL_ASK;
                    let args = self.call_args_json(&call.args);
                    // `tell(text)` / `tell(to, text)`, always
                    // `ask(who, text)` — positional, not an options
                    // object; `input` alongside the text is no longer
                    // expressible from the bare verb (verbs.rs never
                    // carried one either). Omitted `to`/`who` resolves
                    // to whoever this branch owes its oldest open post
                    // to (`resolve_address`).
                    let (to, text) = match (call.name.as_str(), args.as_slice()) {
                        (TOOL_TELL, [text]) => (None, coerce_text(text)),
                        (_, [to, text]) => (Some(to.clone()), coerce_text(text)),
                        _ => (None, None),
                    };
                    match (text, self.resolve_address(tree, to.as_ref())) {
                        (Some(text), Ok(to)) => {
                            let send = self.issue_call(
                                tree,
                                Call::Send {
                                    to,
                                    text,
                                    input: serde_json::Value::Null,
                                    expects_reply,
                                    site: call.site,
                                },
                                Slot::Promise(call.promise),
                            )?;
                            sends.push(send);
                        }
                        (None, _) => {
                            self.reject_call(
                                call.promise,
                                &format!(
                                    "{}({}text) needs a text argument",
                                    call.name,
                                    if expects_reply { "who, " } else { "[to, ]" }
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
                            site: call.site,
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
    ///   `answer` and `append_history` append one, `remove_history` /
    ///   `rewrite_history` add an edit to the batch the running
    ///   compaction handler is building, `done` sets the flag that
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
                match args.first().and_then(|v| v.as_str()) {
                    Some(charter) => {
                        let spawn = self.issue_call(
                            tree,
                            Call::Spawn {
                                name: None,
                                charter: charter.to_owned(),
                                tools: None,
                                site: call.site,
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
                let fork = self.issue_call(
                    tree,
                    Call::Fork {
                        name: None,
                        site: call.site,
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
                        site: call.site,
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
                match args.as_slice() {
                    [question, _label, value] => {
                        match question.as_u64().filter(|n| *n > 0).map(EventId::new) {
                            Some(question) if self.open().contains(&question) => {
                                let value = value.clone();
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
                    _ => {
                        self.settle_err(
                            "answer(question, label, value) takes exactly three arguments",
                        );
                        Ok(true)
                    }
                }
            }
            TOOL_APPEND_HISTORY => {
                let args = self.call_args_json(&call.args);
                match args.first() {
                    Some(value) => {
                        tree.append(
                            &mut self.spine,
                            EventPayload::Note {
                                text: note_text(value),
                            },
                        )?;
                        self.settle(Ok(serde_json::Value::Null));
                    }
                    None => self.settle_err("append_history(value) needs one argument"),
                }
                Ok(true)
            }
            TOOL_DONE => {
                // Nothing leaves the process. Recorded on the `Runner`
                // rather than answered-and-forgotten because the
                // decision it feeds (rest instead of continue) isn't
                // made until the program's return value is known, in
                // `finish_program` — a program can call `done()` and
                // then keep running (more `tell`s, more calls) before
                // actually returning, and the flag has to survive to see
                // that.
                self.done = true;
                self.settle(Ok(serde_json::Value::Null));
                Ok(true)
            }
            TOOL_REMOVE_HISTORY | TOOL_REWRITE_HISTORY => {
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

    /// Add one `remove_history`/`rewrite_history` call to the batch the
    /// running compaction handler is building.
    ///
    /// Refuses outside a compaction program rather than quietly doing
    /// nothing: history is not a thing an ordinary program edits, and a
    /// silently-ignored call would look to the model exactly like one
    /// that worked.
    fn record_compaction(&mut self, name: &str, args: &[Value]) -> Result<(), String> {
        use crate::compaction::CompactionOp;
        if self.compacting.is_none() {
            return Err(format!(
                "{name} is only available in a compaction program, which the harness asks for \
                 when the conversation outgrows its budget"
            ));
        }
        let args = self.call_args_json(args);
        let id = match args.first().and_then(|v| v.as_u64()) {
            Some(n) if n > 0 => EventId::new(n),
            _ => return Err(format!("{name}(id, label, …) needs the row's id")),
        };
        let label = match args.get(1).and_then(|v| v.as_str()) {
            Some(l) => l.to_owned(),
            None => {
                return Err(format!(
                    "{name}(id, label, …) needs the row's label, which is checked against the \
                     row itself so a wrong id cannot compact the wrong thing"
                ));
            }
        };
        let op = if name == TOOL_REMOVE_HISTORY {
            CompactionOp::Remove { id, label }
        } else {
            let Some(text) = args.get(2).and_then(|v| v.as_str()) else {
                return Err(format!(
                    "{TOOL_REWRITE_HISTORY}(id, label, value) needs the replacement text"
                ));
            };
            CompactionOp::Rewrite {
                id,
                label,
                text: text.to_owned(),
            }
        };
        self.compacting.as_mut().expect("checked above").push(op);
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
                .map(|s| s.trim().trim_start_matches('#'))
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
        let id = EventId::new(*id);
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
    fn fetch_history(&self, tree: &Tree, args: &[Value]) -> Result<serde_json::Value, String> {
        let id = match args.first() {
            Some(Value::PosInt(n)) => *n,
            _ => return Err("fetch_history needs a numeric id".into()),
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
            EventPayload::Return { value } => Ok(value.clone()),
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
            EventPayload::Message(msg @ Message::Post { .. }) => match tree.resolve(msg) {
                Message::Post { origin, .. } => Ok(serde_json::Value::String(
                    origin.direct().map(|(t, _, _)| t).unwrap_or("").to_owned(),
                )),
                _ => unreachable!("resolve() never changes a Post's variant"),
            },
            // A program's own source, so a compacted turn can be read
            // back by the turn that needs to know what it did.
            EventPayload::Message(Message::Turn { source, .. }) => {
                Ok(serde_json::Value::String(source.clone()))
            }
            EventPayload::Note { text } => Ok(serde_json::Value::String(text.clone())),
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
        if self.compacting.is_some() {
            let budget = crate::host::document_budget();
            let headroom = crate::host::compaction_headroom();
            match self.finish_compaction(tree, budget, headroom)? {
                Ok(_) => {}
                Err(why) => {
                    // Not a failure: the log is untouched, so the
                    // condition simply fires again with the reason in
                    // front of the next program.
                    tree.append(
                        &mut self.spine,
                        EventPayload::Message(Message::Post {
                            from: Author::Harness,
                            origin: Origin::Direct {
                                text: why,
                                input: serde_json::Value::Null,
                                expects_reply: false,
                            },
                        }),
                    )?;
                }
            }
        }

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
        let decision = value_json.get("__decision").and_then(|v| v.as_str());
        if matches!(decision, Some("resume") | Some("abandon")) && !self.beneath.is_empty() {
            let decision = decision.expect("checked Some above").to_owned();
            let (old_run, resume_with, home_generation) =
                self.beneath.pop().expect("checked non-empty above");
            // This program's own execution genuinely happened — its
            // status is `Completed` and its final VM is kept for the
            // sticky debugger pane like any other — but it gets no
            // `Return`/`Console` row of its own: `Runner::resume`'s own
            // doc is explicit that nothing new is *said* by a decision,
            // and `program_status_survives_reopen` (tree.rs) already
            // fixes this exact shape — the resumed run's *own* eventual
            // `Return`/`Console` are what a report is derived from, not
            // this one's.
            self.note_status(run.program_id, ProgramStatus::Completed);
            self.last_vm = Some(run.vm);
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
                // receives (`Cause::Abandoned`'s own doc), which is
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
            // by the fresh `Cause::Abandoned` `abandon()` is about to
            // log — indistinguishable from a genuinely new, unshown
            // completion — and fire a spurious prompt for an exchange
            // the branch has already fully seen.
            self.shown = self.spine.leaf_id.as_u64();
            self.phase = Phase::Suspended(old_run, resume_with);
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
        if let Some((old_run, _resume_with, _home_generation)) = self.beneath.pop() {
            self.note_status(old_run.program_id, ProgramStatus::Failed);
            self.last_vm = Some(old_run.vm);
            tree.append(
                &mut self.spine,
                EventPayload::Condition {
                    cause: Cause::Abandoned,
                    site: 0,
                    stack: Vec::new(),
                    disposition: Disposition::Handover,
                },
            )?;
        }

        // "Completed ⇒ `Return`" holds without exception — a program that
        // ends without a `return` still logs `Return { value: null }` —
        // which is what makes recovery decidable from the log alone.
        let outcome = tree.append(
            &mut self.spine,
            EventPayload::Return {
                value: value_json.clone(),
            },
        )?;
        // The console is a diagnostic stream, capped with an explicit
        // marker; the report carries only a bounded tail of it.
        tree.append(
            &mut self.spine,
            EventPayload::Console {
                lines: crate::report::cap_console(
                    &run.vm.console_lines,
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
        // that, most expensive, failure. `done()` (`TOOL_DONE`) is the
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
        if self.done {
            // Matches `suspend`'s own depth>0 branch precedent: `shown`
            // advances here, marking this outcome accounted-for so
            // `needs_prompt`'s crash-recovery clause doesn't spuriously
            // re-fire for a completion this file just handled
            // synchronously (that clause is for a reopened log's
            // genuinely stale `shown`, not for "immediately after I
            // logged this myself").
            self.shown = self.spine.leaf_id.as_u64();
            self.done = false;
        }
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
        let Phase::Running(run) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            unreachable!()
        };

        // Split the live suspension in two: a serialisable `Cause` for
        // the log (everything the report needs) and a `ResumeWith` handle
        // the *live* VM needs to resume. A `VMError` is not serialisable
        // and only a live VM can consume one, so the two cannot be the
        // same value.
        let (cause, site, suspension, disposition) = match cause {
            SuspendCause::Raise { condition, payload } => {
                let payload = payload.map(|v| value_json(&run.vm, &v));
                // `step()` advanced `ip` past the `Raise`, so the raise
                // site is the previous slot.
                let site = span_at(&run.vm, (run.vm.ip as usize).saturating_sub(1));
                (
                    Cause::Raised {
                        name: condition,
                        payload,
                    },
                    site,
                    ResumeWith::Raise,
                    // FLAGGED (23_ONE_AGENT.md A4 — "record Disposition
                    // on every Condition you log"): whether this raise
                    // was a tail call (`Handover` — the raising frame
                    // already popped, nothing left but the epilogue) or
                    // ordinary deliberation (`Pushed`) is not derivable
                    // from anything `StepResult::Raise`/`VM::frames()`
                    // exposes here. `Pushed` is `Disposition`'s own safe
                    // default: it only ever costs an unnecessary nesting
                    // level on replay, never a miscounted depth. Detecting
                    // a real tail-call handover (comparing frame depth
                    // before/after, or a VM-side marker) is left for
                    // whoever next touches replay depth counting.
                    Disposition::Pushed,
                )
            }
            SuspendCause::Trapped(e) => {
                let site = span_at(&run.vm, e.ip as usize);
                let cause = Cause::Trapped {
                    kind: format!("{:?}", e.kind),
                    message: e.message.clone(),
                    resumable: matches!(e.resume, ResumeMode::PushValueThenContinue),
                };
                // Same flag as above: a trap has no tail-call shape to
                // even ask the question of (it isn't a `raise`), so
                // `Pushed` here is not a default standing in for an
                // unknown answer — it is simply correct. Noted anyway so
                // the two cases aren't confused when this is read later.
                (cause, site, ResumeWith::Trapped(e), Disposition::Pushed)
            }
            SuspendCause::Posted(ids) => {
                let site = span_at(&run.vm, run.vm.ip as usize);
                // A post arriving is never a tail call — there is no
                // "handler" in the raise/resume sense here, just the
                // running program parking until its next fuel slice
                // (rule B). `Pushed` is simply correct.
                (
                    Cause::Posted { ids },
                    site,
                    ResumeWith::Continue,
                    Disposition::Pushed,
                )
            }
        };

        let stack: Vec<String> = run
            .vm
            .frames()
            .iter()
            .map(|f| f.name().to_owned())
            .collect();
        let console = run.vm.console_lines.clone();
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
        // `Cause::Abandoned` is logged for a program that finished on
        // purpose.
        let handed_over = disposition == Disposition::Handover;
        if handed_over {
            self.last_vm = Some(run.vm);
            self.phase = Phase::Idle;
            self.note_status(program_id, ProgramStatus::Completed);
        } else {
            self.phase = Phase::Suspended(run, suspension);
            self.note_status(program_id, ProgramStatus::Suspended);
        }
        // The outcome carries the site and the stack because those were
        // the last inputs that lived only in the VM, and the VM is never
        // persisted.
        let outcome = tree.append(
            &mut self.spine,
            EventPayload::Condition {
                cause,
                site,
                stack,
                disposition,
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
        // Checked before the request is built, not after: a document
        // over budget is over budget *for this request*, and the whole
        // point is to not send it. The handler's own request is built
        // from the compacted document on the next pass through here.
        let budget = crate::host::document_budget();
        let headroom = crate::host::compaction_headroom();
        if let Some(request) = self.compaction_if_needed(tree, budget, headroom)? {
            return Ok(vec![request]);
        }
        self.phase = Phase::AwaitingLlm;
        Ok(vec![self.render_request(tree)])
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
                EventPayload::Condition {
                    cause: Cause::Compaction { .. },
                    ..
                } => attempts += 1,
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
        if self.compacting.is_some() || self.compaction_attempts(tree) >= COMPACTION_ATTEMPTS {
            return Ok(None);
        }
        let doc = crate::document::render(tree, &self.spine, budget);
        let rendered = crate::compaction::rendered_size(&doc);
        if !crate::compaction::should_fire(rendered, budget, headroom) {
            return Ok(None);
        }
        tree.append(
            &mut self.spine,
            EventPayload::Condition {
                cause: Cause::Compaction { rendered, budget },
                site: 0,
                stack: Vec::new(),
                // `Handover`, not `Pushed`: `document::render` inserts a
                // report for a `Handover` and *hides* a `Pushed` one,
                // because a pushed condition means a nested handler is
                // about to run and nothing chat-visible has happened
                // yet. Logged as `Pushed`, the request to compact went
                // into the rolling document nowhere at all — two live
                // runs on 2026-09-16 saw an ordinary conversation with
                // an unanswered task, and did the task. Nothing opens a
                // scope here: the compaction program is this branch's
                // next turn, not a deliberation beneath it.
                disposition: Disposition::Handover,
            },
        )?;
        self.compacting = Some(Vec::new());
        self.phase = Phase::AwaitingLlm;
        Ok(Some(self.render_request(tree)))
    }

    /// Commit a finished compaction handler's batch, or say why not.
    ///
    /// `compaction::compact` validates the whole batch against the log
    /// before anything is appended, so this either appends every
    /// `Compacted` event or none of them. A rejection is returned as a
    /// string for the caller to hand back to the model, which is a
    /// retry rather than a failure: the log is untouched either way.
    fn finish_compaction(
        &mut self,
        tree: &mut Tree,
        budget: usize,
        headroom: f64,
    ) -> io::Result<Result<usize, String>> {
        let Some(ops) = self.compacting.take() else {
            return Ok(Ok(0));
        };
        let threshold = (budget as f64 * (1.0 - headroom)) as usize;
        match crate::compaction::compact(tree, &self.spine, &ops, budget, threshold) {
            Ok(events) => {
                let n = events.len();
                for event in events {
                    tree.append(&mut self.spine, event)?;
                }
                Ok(Ok(n))
            }
            Err(e) => Ok(Err(e.to_string())),
        }
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
    pub(crate) fn render_request(&mut self, tree: &Tree) -> StepOutput {
        // Everything logged so far is about to be shown. This is the one
        // place the mark moves.
        self.shown = self.spine.leaf_id.as_u64();
        StepOutput::LlmRequest(LlmRequest {
            tail: self.request_tail(tree),
        })
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
                EventPayload::Condition {
                    cause: Cause::Compaction { rendered, budget },
                    ..
                } => Some(crate::report::compaction_message(*rendered, *budget)),
                _ => None,
            })
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
    fn request_tail(&self, tree: &Tree) -> Option<String> {
        // While a compaction is outstanding the directive *is* the tail,
        // and nothing else rides with it — the directive's own words are
        // "Write a compaction program. Nothing else", and the open-
        // questions and presence lines are about work that is explicitly
        // not being done in this program. It lives here rather than in
        // the document because it instructs rather than reports: see
        // `document::render_with_lookup`, which skips the row.
        if self.compacting.is_some() {
            if let Some(directive) = self.compaction_directive(tree) {
                return Some(directive);
            }
        }
        let mut lines: Vec<String> = Vec::new();
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
                n => format!(", and {n} more"),
            };
            let count = match open.len() {
                1 => "1 question is".to_owned(),
                n => format!("{n} questions are"),
            };
            lines.push(format!(
                "{count} open on this branch: {}{more}. A post from an agent means that \
                 agent's program is suspended on this value and stays suspended until a \
                 program on this branch calls answer(question, label, value) naming it — any \
                 of your open questions, in any order, from anywhere in the program.",
                ids.join(", "),
            ));
        }
        if let Some((count, first, last)) = self.artifact_span(tree) {
            lines.push(format!(
                "{count} artifacts on this branch, #{first}–#{last}. A report lists only \
                 what is new since the last one; every id above stays fetchable with \
                 fetch_history(id)."
            ));
        }
        lines.push(if self.attached { PRESENT } else { ABSENT }.to_owned());
        Some(lines.join("\n"))
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
                    EventPayload::Call(_) | EventPayload::Return { .. }
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
    fn agent_segment<'t>(&self, tree: &'t Tree) -> Vec<&'t Event> {
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

/// `append_history(value)` takes any JSON value, but `EventPayload::Note`
/// stores rendered text: a JSON string is used verbatim, anything else is
/// serialized. The card's own guidance is to append a short projection
/// (a summary), not a raw result, so the common case is already a string.
fn note_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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

fn render_diags(source: &str, diags: &[Diagnostic]) -> String {
    let rendered: Vec<String> = diags.iter().take(5).map(|d| d.render(source)).collect();
    format!("compile error:\n{}", rendered.join("\n"))
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
pub(crate) fn menu_rows(segment: &[&Event], since: u64) -> Vec<Artifact> {
    segment
        .iter()
        .filter(|e| e.id.as_u64() > since)
        .filter_map(|event| {
            let id = event.id.as_u64();
            match &event.payload {
                // A row's label comes from the call *variant*; its value
                // (or its absence) from the `Result`.
                EventPayload::Call(call) => Some(Artifact {
                    id,
                    label: call_label(call),
                    state: match call {
                        // A `tell` settles, but nothing about the
                        // settlement is news: it expects no reply, so
                        // there is neither an answer to await nor a
                        // value worth a byte count.
                        Call::Send {
                            expects_reply: false,
                            ..
                        } => ArtifactState::Told,
                        _ => match settlement_of(segment, event.id) {
                            Some(Outcome::Delivered(v)) => ArtifactState::Delivered(v.clone()),
                            Some(Outcome::Failed(msg)) => ArtifactState::Failed(msg.clone()),
                            // Only one pending kind can be re-attached:
                            // an `ask`'s answer is still coming, while a
                            // `Spawn`/`Fork`/`Invoke`'s worker died with
                            // the process.
                            None => match call {
                                Call::Send { .. } => ArtifactState::PendingSend,
                                Call::Spawn { .. } | Call::Fork { .. } | Call::Invoke { .. } => {
                                    ArtifactState::PendingInvoke
                                }
                            },
                        },
                    },
                }),
                EventPayload::Return { value } => Some(Artifact {
                    id,
                    label: "program result".into(),
                    state: ArtifactState::Delivered(value.clone()),
                }),
                _ => None,
            }
        })
        .collect()
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
        } => format!(
            "{}({}, {})",
            if *expects_reply { "ask" } else { "tell" },
            address_label(to),
            arg_preview(&serde_json::Value::String(text.clone()))
        ),
        Call::Spawn { name, .. } => format!("spawn({})", name.as_deref().unwrap_or("<unnamed>")),
        Call::Fork { name, .. } => {
            format!("fork({})", name.as_deref().unwrap_or(""))
        }
        Call::Invoke { name, args, .. } => format!("{}({})", name, arg_preview(args)),
    }
}

fn address_label(to: &Address) -> String {
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

/// The source byte offset of instruction `ip` — what a `Condition`
/// records so its report can point a caret without a live VM.
fn span_at(vm: &VM, ip: usize) -> u32 {
    vm.spans.get(ip).copied().unwrap_or(0)
}

/// Who authored the post `question` — the author an answer is owed to.
fn asker_of(tree: &Tree, question: EventId) -> Option<Author> {
    match &tree.events.get(&question)?.payload {
        EventPayload::Message(Message::Post { from, .. }) => Some(*from),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FUEL: u64 = 100_000;

    fn setup() -> (Tree, Runner) {
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
                    to,
                    text: text.into(),
                    input,
                    expects_reply: true,
                    site: 0,
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
            let src = if *verb == "done" || *verb == "fork" {
                format!("{verb}();")
            } else {
                format!("{verb}(1, 2, 3);")
            };
            let (mut tree, mut state) = setup();
            let program = compile(&src).unwrap_or_else(|e| panic!("{verb}: {e:?}"));
            let mut vm = VM::for_program(program, serde_json::Value::Null).unwrap();
            match vm.step(FUEL).unwrap() {
                StepResult::Settle { call } => {
                    state.phase = Phase::Running(Run {
                        program_id: state.spine.leaf_id,
                        vm,
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
                        matches!(name, TOOL_ASK | TOOL_TELL) || crate::host::serves_inline(name),
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
                        matches!(name, TOOL_ASK | TOOL_TELL) || crate::host::serves_inline(name),
                        "`{verb}` falls through dispatch_calls to the tool registry"
                    );
                }
                other => panic!("`{verb}`: nothing dispatched it ({other:?})"),
            }
        }
    }

    fn llm_program(source: &str) -> LlmTurn {
        LlmTurn {
            source: source.into(),
            thinking: None,
            truncated: false,
            usage: None,
            reply: None,
        }
    }

    /// Runs `f` with `AGENT2_TRANSPORT` set to `value`, restoring
    /// whatever was there before (or its absence) once `f` returns —
    /// `document.rs`'s own `with_transport` test helper, duplicated here
    /// because it is `cfg(test)`-private to that module. Relies on this
    /// crate's documented gate (`cargo test -p agent --
    /// --test-threads=1`) running the binary single-threaded: a
    /// per-process env var has no other safe way to be scoped to one
    /// test.
    fn with_transport<T>(value: &str, f: impl FnOnce() -> T) -> T {
        let key = "AGENT2_TRANSPORT";
        let prev = std::env::var(key).ok();
        // SAFETY: the test binary runs on one thread — enforced by
        // `RUST_TEST_THREADS = "1"` in `.cargo/config.toml`, which
        // exists for this. It was previously only asserted here,
        // while `cargo test` ran one thread per core.
        unsafe { std::env::set_var(key, value) };
        let result = f();
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        result
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
            .find(|e| {
                matches!(
                    e.payload,
                    EventPayload::Return { .. } | EventPayload::Condition { .. }
                )
            })
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
                EventPayload::Message(Message::Post { .. }) => "Post",
                EventPayload::Message(Message::Turn { .. }) => "Turn",
                EventPayload::Call(_) => "Call",
                EventPayload::Result { .. } => "Result",
                EventPayload::Return { .. } => "Return",
                EventPayload::Condition { .. } => "Condition",
                EventPayload::Console { .. } => "Console",
                EventPayload::Rename { .. } => "Rename",
                EventPayload::Note { .. } => "Note",
                EventPayload::Compacted { .. } => "Compacted",
            })
            .collect()
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

    #[test]
    fn program_completion_logs_a_harness_report_and_the_branch_goes_idle() {
        let (mut tree, mut state) = setup();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("console.log(\"hi there\"); return 6 * 7;")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let report = last_report(&state, &tree);
        assert!(report.contains("42"), "{report}");
        assert!(report.contains("hi there"), "{report}");
        assert_eq!(
            payload_kinds(&state, &tree),
            ["Agent", "Post", "Turn", "Return", "Console"]
        );
    }

    // ── `Transport::RunProgram`'s two follow-up gaps ────────────────
    //
    // Commit 6084a70 added the transport switch but left `RunProgram`
    // inert: the model's prose reached no log and no user (gap 1), and
    // the conversation never turned after a program finished (gap 2).
    // These four pin the fix, one per named acceptance case.

    #[test]
    fn run_program_prose_reaches_the_user_as_a_send() {
        with_transport("run_program", || {
            let (mut tree, mut state) = setup();
            user_post(&mut state, &mut tree, "go");
            let out = state
                .step(
                    &mut tree,
                    StepInput::LlmResponse(crate::host::scripted_reply(
                        "On it — computing now.",
                        "return 1;",
                    )),
                )
                .unwrap();
            let settled = drain(&mut state, &mut tree, out);
            let sends = settled
                .iter()
                .find_map(|o| match o {
                    StepOutput::Sends(s) => Some(s.clone()),
                    _ => None,
                })
                .expect("the reply dispatches as a Sends output, exactly like a tell()");
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
            assert_eq!(*to, Address::User);
            assert_eq!(text, "On it — computing now.");
            assert!(!expects_reply, "a reply owes no answer, same as a tell()");

            // The log cannot tell a prose reply from a `tell` apart:
            // `agent score`'s `tells`/`silent` fields see the identical
            // shape either way.
            let score = crate::score::score(&tree);
            assert_eq!(score.tells, ["On it — computing now."]);
            assert!(!score.silent);
        });
    }

    #[test]
    fn run_program_continues_after_a_program_returns() {
        with_transport("run_program", || {
            let (mut tree, mut state) = setup();
            user_post(&mut state, &mut tree, "go");
            let out = state
                .step(&mut tree, StepInput::LlmResponse(llm_program("return 1;")))
                .unwrap();
            let settled = drain(&mut state, &mut tree, out);
            let requests = settled
                .iter()
                .filter(|o| matches!(o, StepOutput::LlmRequest(_)))
                .count();
            assert_eq!(
                requests, 1,
                "a completed program's return value is a tool result under \
                 Transport::RunProgram, not a chat reply — the conversation \
                 must continue with a fresh request: {settled:?}"
            );
        });
    }

    /// **Superseded by the `done()` change**: this used to pin
    /// `Transport::Program`'s own regression guard — completing a
    /// program was a silent no-op by default (this file's old words in
    /// `finish_program`), and only `Transport::RunProgram` continued.
    /// That polarity is exactly what this phase inverts (see
    /// `finish_program`'s current comment): the dominant observed
    /// failure across every card variant and both models is a program
    /// that does one step and stops, so completing now continues on
    /// *both* transports unless the program called `done()`. What was
    /// this test's assertion is now `done_ends_the_conversation`'s; this
    /// one instead pins the new default.
    #[test]
    fn a_completed_program_continues_by_default() {
        with_transport("program", || {
            let (mut tree, mut state) = setup();
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
                 transports, unless it called done(): {settled:?}"
            );
            assert!(!state.is_idle());
        });
    }

    /// `done()` is the one thing that stops the loop (`TOOL_DONE`'s own
    /// doc): the mirror image of the test above, same shape, only the
    /// program's text differs.
    #[test]
    fn done_ends_the_conversation() {
        with_transport("program", || {
            let (mut tree, mut state) = setup();
            user_post(&mut state, &mut tree, "go");
            let out = state
                .step(&mut tree, StepInput::LlmResponse(llm_program("done();")))
                .unwrap();
            let settled = drain(&mut state, &mut tree, out);
            assert!(
                !settled
                    .iter()
                    .any(|o| matches!(o, StepOutput::LlmRequest(_))),
                "done() ends the conversation with no further request: {settled:?}"
            );
            assert!(state.is_idle());
        });
    }

    /// `done()` settles at dispatch (like `spawn`/`fork`), so it is
    /// recorded on the `Runner` well before the program's own
    /// completion is known — a call after it in the same program (here,
    /// a `tell()`) must not un-record it.
    #[test]
    fn done_is_recorded_before_the_program_ends() {
        with_transport("program", || {
            let (mut tree, mut state) = setup();
            user_post(&mut state, &mut tree, "go");
            let out = state
                .step(
                    &mut tree,
                    StepInput::LlmResponse(llm_program("done(); tell(\"wrapping up now\");")),
                )
                .unwrap();
            let settled = drain(&mut state, &mut tree, out);
            assert!(
                !settled
                    .iter()
                    .any(|o| matches!(o, StepOutput::LlmRequest(_))),
                "a statement after done() must not cancel it: {settled:?}"
            );
            assert!(state.is_idle());
        });
    }

    #[test]
    fn a_reply_with_no_program_ends_the_task() {
        with_transport("run_program", || {
            let (mut tree, mut state) = setup();
            user_post(&mut state, &mut tree, "go");
            let out = state
                .step(
                    &mut tree,
                    StepInput::LlmResponse(crate::host::scripted_reply(
                        "All done, nothing left to do.",
                        "",
                    )),
                )
                .unwrap();

            // No `Working` output at all: an empty `source` must never
            // reach `start_program`/`interp::compile` and manufacture a
            // spurious `Cause::CompileFailed`.
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
                ["Agent", "Post", "Turn", "Call"],
                "no Return, no Condition — nothing ran"
            );

            let score = crate::score::score(&tree);
            assert_eq!(score.tells, ["All done, nothing left to do."]);
            assert_eq!(score.compile_failures, Vec::<String>::new());
        });
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
        assert!(report.contains("compile error"), "{report}");
        assert_eq!(payload_kinds(&state, &tree), ["Agent", "Turn", "Condition"]);
        assert!(state.is_idle() || matches!(state.status(), "awaiting llm"));
    }

    #[test]
    fn raise_suspends_with_pushed_disposition_and_host_driven_resume_continues() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"
            const x = raise("need_help", { got: 41 });
            return x + 1;
        "#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(src)))
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(state.status(), "suspended");

        let disposition = state
            .agent_segment(&tree)
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::Condition { disposition, .. } => Some(*disposition),
                _ => None,
            })
            .expect("a logged Condition");
        assert_eq!(disposition, Disposition::Pushed);

        // The host, not the LLM, drives the continuation directly.
        let out = state.resume(&mut tree, json!(41)).unwrap();
        drain(&mut state, &mut tree, out);
        assert!(last_report(&state, &tree).contains("42"));
    }

    #[test]
    fn abandon_discards_the_suspended_vm_and_leaves_the_branch_idle() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("raise(\"need\", null); return 1;")),
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

    /// **Superseded by 27.1 and then deleted with the verb.**
    /// `next_program_hands_over_and_its_payload_stays_in_the_document`
    /// pinned that `next_program(payload)` logs `Handover` rather than
    /// `Pushed`, so its payload reached the rolling document instead of
    /// only the one-shot handler prompt — a live run had handed over
    /// 190KB and the program after next saw none of it.
    ///
    /// `return payload` does that job now and does it without a
    /// condition at all: `a_completed_program_continues_by_default`
    /// covers the continuation, and 27.7's
    /// `a_return_value_reaches_the_next_program_whole` covers the
    /// payload arriving intact, which is the property that test was
    /// really about. The verb is gone (zero uses across 82 live runs
    /// once no card named it), so there is nothing left to assert.

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
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program(
                    "const x = raise(\"need_help\", { got: 41 }); return x + 1;",
                )),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(state.status(), "suspended");

        // The handler answers with a program, not a direct host call —
        // `finish_program` is the one reading the tag off its
        // completion.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("return resume(41);")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert!(last_report(&state, &tree).contains("42"));
        assert_eq!(state.status(), "idle");

        // A second raise, this time abandoned the same way — through a
        // handler's own completion, not a direct host call.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("raise(\"need\", null); return 1;")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(state.status(), "suspended");

        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("return abandon();")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(state.status(), "idle");
        // The abandoned raise is a logged `Cause::Abandoned`, not a
        // `Return` of the raw decision object anywhere on the branch.
        assert!(
            state.agent_segment(&tree).iter().any(|e| matches!(
                &e.payload,
                EventPayload::Condition {
                    cause: Cause::Abandoned,
                    ..
                }
            )),
            "the abandon is a logged Condition"
        );
        assert!(
            !state.agent_segment(&tree).iter().any(|e| matches!(
                &e.payload,
                EventPayload::Return { value } if value.get("__decision").is_some()
            )),
            "no decision object ever lands as a program's own Return"
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
        assert!(matches!(&out[..], [StepOutput::Working]));

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
                EventPayload::Message(Message::Post {
                    from: Author::Harness,
                    origin,
                }) => origin.direct().map(|(t, _, _)| t.to_owned()),
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
                    "return await ask({}, \"which file?\");",
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
                StepInput::LlmResponse(llm_program("return await spawn(\"researcher\");")),
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
                StepInput::LlmResponse(llm_program("return fork();")),
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
    fn append_history_logs_a_note_and_is_never_re_sent_to_context() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program(
                    "await append_history(\"figured out the bug is in parsing\"); return 1;",
                )),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        let note = state
            .agent_segment(&tree)
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::Note { text } => Some(text.clone()),
                _ => None,
            });
        assert_eq!(note.as_deref(), Some("figured out the bug is in parsing"));
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
        let rewrite = format!("return await fetch_history({});", id.as_u64());
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
        let (mut tree, mut state) = setup();
        let out = user_post(
            &mut state,
            &mut tree,
            "the third column is the one that matters",
        );
        drain(&mut state, &mut tree, out);
        let post = state
            .agent_segment(&tree)
            .iter()
            .rev()
            .find(|e| matches!(e.payload, EventPayload::Message(Message::Post { .. })))
            .unwrap()
            .id;

        tree.append(
            &mut state.spine,
            EventPayload::Compacted {
                of: post,
                label: "post".into(),
                text: None,
            },
        )
        .unwrap();
        // It really is gone from what the model reads.
        let doc = crate::document::render(&tree, &state.spine, TEST_BUDGET);
        let rendered: String = doc.messages.iter().map(|m| m.content.clone()).collect();
        assert!(!rendered.contains("third column"), "still in the document");

        let before = payload_kinds(&state, &tree).len();
        let src = format!("return await fetch_history({});", post.as_u64());
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(&src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            !settled
                .iter()
                .any(|o| matches!(o, StepOutput::ToolCalls(_))),
            "served from the log, no call issued"
        );
        let returned = tree
            .path_events(state.spine.leaf_id)
            .iter()
            .rev()
            .find_map(|e| match &e.payload {
                EventPayload::Return { value } => Some(value.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(returned, json!("the third column is the one that matters"));

        // **Reading is free.** The fetch adds nothing of its own: only
        // the program's own events appear, and none of them is a
        // `Call`/`Result` pair for the fetch. That is what lets the
        // menu be an index rather than a replay.
        assert_eq!(
            &payload_kinds(&state, &tree)[before..],
            ["Turn", "Return", "Console"],
            "the fetch logged something of its own"
        );
    }

    /// A `Note` and a program's own source come back too — the three
    /// row kinds 27.4 added, one test each would be three copies of the
    /// same walk.
    #[test]
    fn fetch_history_reads_a_note_and_a_program_back() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = "await append_history(\"the parser drops the last field\"); return 1;";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(src)))
            .unwrap();
        drain(&mut state, &mut tree, out);
        let segment = state.agent_segment(&tree);
        let note = segment
            .iter()
            .find(|e| matches!(e.payload, EventPayload::Note { .. }))
            .unwrap()
            .id;
        let turn = segment
            .iter()
            .find(|e| matches!(e.payload, EventPayload::Message(Message::Turn { .. })))
            .unwrap()
            .id;

        let fetch = format!(
            "return [await fetch_history({}), await fetch_history({})];",
            note.as_u64(),
            turn.as_u64()
        );
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(&fetch)))
            .unwrap();
        drain(&mut state, &mut tree, out);
        let returned = tree
            .path_events(state.spine.leaf_id)
            .iter()
            .rev()
            .find_map(|e| match &e.payload {
                EventPayload::Return { value } => Some(value.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(returned[0], json!("the parser drops the last field"));
        assert_eq!(returned[1], json!(src));
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
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        // `parsed.missing` is undefined; the write must not happen.
        let src = "const parsed = {};\n\
                   try { await tools.write_file(\"out.py\", parsed.missing); }\n\
                   catch (e) { tell(`refused: ${e}`); }\n\
                   done();";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        assert!(
            !settled
                .iter()
                .any(|o| matches!(o, StepOutput::ToolCalls(_))),
            "the write was issued anyway"
        );
        let said: Vec<String> = tree
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Call(Call::Send { text, .. }) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            said.iter().any(|t| t.contains("argument 2 is `undefined`")),
            "the program is told which argument, and that nothing ran: {said:?}"
        );
    }

    #[test]
    fn answer_dispatches_from_inside_a_program() {
        let (mut tree, mut state) = setup();
        let out = user_post(&mut state, &mut tree, "which one?");
        let question = state.open()[0];
        drain(&mut state, &mut tree, out);

        let src = format!(
            "await answer({}, \"q\", \"the second\"); return 1;",
            question.as_u64()
        );
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program(&src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let answered = settled.iter().find_map(|o| match o {
            StepOutput::Answered { question: q, value } => Some((*q, value.clone())),
            _ => None,
        });
        assert_eq!(answered, Some((question, json!("the second"))));
        assert!(state.agent_segment(&tree).iter().any(
            |e| matches!(&e.payload, EventPayload::Answer { question: q, .. } if *q == question)
        ));
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
            "try {{ await answer({}, \"q\", 1); return \"unreachable\"; }} catch (e) {{ return \
             \"caught: \" + e; }}",
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
            return [await a, await b];
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
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"try { return await tools.fetch("a"); } catch (e) { return "caught: " + e; }"#;
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
                    result: Err("host is down".into()),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let outcome = state
            .agent_segment(&tree)
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::Result { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .expect("a Result");
        assert!(matches!(&outcome, Outcome::Failed(m) if m == "host is down"));
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
        assert!(matches!(&out[..], [StepOutput::Working]));
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
                StepInput::LlmResponse(llm_program("return input.body.length;")),
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
    fn crowded() -> (Tree, Runner, usize) {
        let (mut tree, mut state) = setup();
        for _ in 0..20 {
            tree.append(
                &mut state.spine,
                EventPayload::Message(Message::Post {
                    from: Author::User,
                    origin: direct(&"filler ".repeat(40), false),
                }),
            )
            .unwrap();
        }
        let size = crate::compaction::rendered_size(&crate::document::render(
            &tree,
            &state.spine,
            64 * 1024,
        ));
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
        assert!(state.compacting.is_none());
        assert!(
            !tree
                .events
                .values()
                .any(|e| matches!(e.payload, EventPayload::Condition { .. }))
        );
    }

    /// Over budget, a `Compaction` condition is logged carrying both
    /// numbers, and a request goes out for the handler.
    #[test]
    fn compaction_fires_over_budget_and_asks_for_a_program() {
        let (mut tree, mut state, budget) = crowded();
        let fired = state.compaction_if_needed(&mut tree, budget, 0.25).unwrap();
        assert!(matches!(fired, Some(StepOutput::LlmRequest(_))));
        assert!(state.compacting.is_some(), "the batch is open");

        let logged = tree
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Condition { cause, .. } => Some(cause.clone()),
                _ => None,
            })
            .expect("a condition");
        let Cause::Compaction {
            rendered,
            budget: b,
        } = logged
        else {
            panic!("wrong cause: {logged:?}");
        };
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
        let (mut tree, mut state, _) = crowded();
        // A budget under the floor: nothing the handler removes can
        // bring the document beneath it.
        let impossible = 1024;
        for _ in 0..COMPACTION_ATTEMPTS {
            assert!(
                state
                    .compaction_if_needed(&mut tree, impossible, 0.25)
                    .unwrap()
                    .is_some(),
                "each attempt within the bound still asks"
            );
            state.compacting = None; // the handler returned, batch rejected
        }
        assert!(
            state
                .compaction_if_needed(&mut tree, impossible, 0.25)
                .unwrap()
                .is_none(),
            "past the bound it stops asking"
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
        let (mut tree, state, _) = crowded();
        let mut leaf = state.spine.leaf_id;
        let impossible = 1024;
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
        assert!(text.contains("remove_history"), "the verbs are not there");

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
            .filter(|e| matches!(e.payload, EventPayload::Message(Message::Post { .. })))
            .map(|e| e.id.as_u64())
            .min()
            .map(EventId::new)
            .expect("a post to compact");

        state.compaction_if_needed(&mut tree, budget, 0.25).unwrap();
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
        assert!(state.compacting.is_none(), "the batch closed");
        let compacted: Vec<_> = tree
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Compacted { of, .. } => Some(*of),
                _ => None,
            })
            .collect();
        assert_eq!(compacted, vec![target], "exactly the row it named");
        assert!(
            tree.events.contains_key(&target),
            "the original row is shadowed, never removed"
        );
    }

    /// Outside a compaction program the verbs are refused rather than
    /// quietly doing nothing — history is not something an ordinary
    /// program edits, and a silently-ignored call looks exactly like one
    /// that worked.
    #[test]
    fn history_verbs_are_refused_outside_a_compaction_program() {
        let (mut tree, mut state) = setup();
        user_post(&mut state, &mut tree, "go");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program(
                    "try { remove_history(1, \"post\"); tell(\"no trap\"); } \
                     catch (e) { tell(`refused: ${e}`); }",
                )),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        let said: Vec<String> = tree
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Call(Call::Send { text, .. }) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            said.iter()
                .any(|t| t.contains("refused")
                    && t.contains("only available in a compaction program")),
            "{said:?}"
        );
    }

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
            to: Address::User,
            text: "y".repeat(8_000),
            input: serde_json::Value::Null,
            expects_reply: false,
            site: 0,
        });
        assert!(label.starts_with("tell(user, "), "{label}");
        assert!(label.len() < crate::report::LABEL_MAX_BYTES + 32, "{label}");
    }
}
