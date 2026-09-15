use std::collections::HashMap;
use std::num::NonZeroU64;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct EventId(NonZeroU64);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Event {
    pub id: EventId,
    pub parent_id: Option<EventId>,
    #[serde(with = "jiff::fmt::serde::timestamp::millisecond::required")]
    pub timestamp: Timestamp,
    pub payload: EventPayload,
}

impl Event {
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }
}

/// Event vocabulary (8_HARNESS Step 1). Two classes:
///
/// - **Chat events** render into LLM requests for their agent.
/// - **Execution events** are harness-internal: queried for replay,
///   artifacts, and UI — never sent to the LLM as messages.
///
/// Each variant documents its parent rule, which spine it lands on, and
/// whether it renders to chat.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum EventPayload {
    /// Chat event. Parent: the previous event on the owning agent's
    /// spine. Renders to chat: yes — a `Turn` is the assistant message,
    /// its `source` the bare program the model ran; the harness's report
    /// on what that program did comes back as a `Post` from
    /// `Author::Harness`, not as a distinguished reply kind of its own.
    Message(Message),

    /// Chat event. Parent: the previous event on the owning agent's
    /// spine. Renders to chat: yes — as a marker in the branch's own
    /// history, nothing more.
    ///
    /// `append_history` is what **I** should remember; a `tell`/`Post`
    /// is what **someone else** needs to know. A `Note` has no
    /// recipient and wakes no branch — where every `tell` is heard by
    /// someone, a note is heard by no one but the branch's own future
    /// self, reading its own history back on a later turn. It exists for
    /// the program that has worked something out and wants it on the
    /// record for its *next* program to see rendered, not merely held in
    /// a variable this run's VM is about to drop — not to read something
    /// back later in the *same* turn, since if you need the value now
    /// you are already holding it.
    Note { text: String },

    /// Structural event; roots an agent's first branch. Parent: the
    /// call-site event on the caller's spine (`None` for the tree
    /// root). Starts a new spine: the caller's spine continues past the
    /// call site independently. Renders to chat: no — but `context()`
    /// **resets** here, which is clean-room isolation (decision 3) in the
    /// type rather than in an `is_some()`.
    ///
    /// `system` is the deliberate exception to "nothing regenerable is
    /// stored": the system prompt is assembled from the registry — state
    /// outside the log — and sits at the very front of the prompt, where
    /// churn is most expensive. Snapshotting it is what keeps a later card
    /// edit or a new registry tool from altering an existing
    /// conversation's cached prefix.
    Agent {
        /// The branch's name at birth; `None` shows a derived label until
        /// someone names it. A branch's name is the last [`Rename`] at or
        /// after its root, else this.
        ///
        /// [`Rename`]: EventPayload::Rename
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// What this agent is for — the role its system prompt states.
        charter: String,
        /// The agent's tool allowlist, enforced by the registry from the
        /// agent's own root; `None` inherits the spawner's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<String>>,
        /// The assembled system prompt, snapshotted at creation.
        system: String,
    },

    /// Structural event; roots a **divergent** branch. Parent: the event
    /// forked from. Renders to chat: a harness line (C1).
    ///
    /// `context()` **carries through** a `Fork` — history and artifacts
    /// cross it — but obligations do not: `replay_event` clears `open`
    /// here, so pre-fork posts stay the original branch's to answer and
    /// there is exactly one owner for every open post.
    Fork {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    /// Settlement event; **this branch answered** the `Post` named by
    /// `question`. Parent: the answering branch's spine. Renders to chat:
    /// no — the answer's text is already this branch's `Turn`.
    ///
    /// The other half of the exchange is a `Result` on the *asker's*
    /// branch, which names the `Send`. Neither copies the other: the
    /// `Answer` holds the value and the `Result` names it.
    Answer {
        question: EventId,
        value: serde_json::Value,
    },

    /// Execution event; one per call a program issues, logged at
    /// **dispatch** (17_BRANCHES A2). Parent: the owning agent's spine,
    /// between the program's `Turn` and its eventual `Return`/`Condition`.
    /// Renders to chat: no — queried for replay, the artifact menu, and
    /// UI. Addressable via `artifact(id)`, which resolves a call id
    /// through to its `Result`.
    ///
    /// Logging at issue rather than at resolution is what distinguishes a
    /// call that **definitively did not work** (a `Failed` `Result`) from
    /// one that was **in flight when the process died** (no `Result` at
    /// all) — a `send_email` issued a millisecond before `kill -9` used to
    /// be invisible in the log.
    Call(Call),

    /// Execution event; a call settled — the artifact. Parent: the owning
    /// agent's spine, in **resolution order** (decision 7). `call` names
    /// the `Call` event this settles; every call gets exactly one
    /// `Result`. Renders to chat: no.
    Result { call: EventId, outcome: Outcome },

    /// Run event; the program finished, and this is its `return` value.
    /// Parent: the owning branch's spine. Renders to chat: no — an
    /// id-addressable artifact like any tool result; the completion
    /// report is *rendered around* it.
    ///
    /// `Return` settles nothing and has no `call`: it is the program's own
    /// output, flowing **into** its branch's LLM rather than back from a
    /// call. A program that ends without a `return` still logs
    /// `Return { value: null }`, so "completed ⇒ `Return`" holds without
    /// exception — which is what makes recovery decidable from the log
    /// alone.
    Return { value: serde_json::Value },

    /// Run event; **everything else** a handback can be — a raise, a
    /// trapped error, an arriving post, a compile failure, a truncated
    /// completion, an interruption. Parent: the owning branch's spine.
    /// Renders to chat: no — its *report* is rendered from it.
    ///
    /// Exactly one outcome per handback (not per run): a single program
    /// may raise, be resumed, trap, be resumed again and finally return,
    /// and each handback logs its own outcome.
    ///
    /// It carries `site` and `stack` because those were the last inputs
    /// that lived only in the VM, and the VM is never persisted. With them
    /// logged, **nothing the model ever saw depends on state outside the
    /// log.**
    Condition {
        cause: Cause,
        /// Where the program stopped: a source byte offset, for the
        /// line-and-caret diagnostic. `0` when no program ran.
        site: u32,
        /// The VM call-stack chain, outermost first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        stack: Vec<String>,
        /// Whether this raise **pushed a handler frame** onto the stack,
        /// or was a **handover** — the raising frame had nothing left to
        /// do but forward the decision, so no frame was pushed.
        ///
        /// Load-bearing for replay's derived depth counter: a `Pushed`
        /// condition increments it, a `Handover` does not, and if the log
        /// doesn't say which happened, every subsequent depth is wrong —
        /// and with it the document's `depth > 0` filter and the
        /// decision/completion reading of `Return`.
        #[serde(default)]
        disposition: Disposition,
    },

    /// This branch is called this from here on. Parent: the owning
    /// branch's spine. Renders to chat: **no** — a `Rename` is a record,
    /// so renaming a branch never wakes it (the driving rule counts only
    /// `Message`s).
    ///
    /// A branch's name is the last `Rename` **at or after its root**, else
    /// its root's name. That is per-*path*, so renaming an original leaves
    /// its forks alone — which is what you want when a fork was named for
    /// how it differs.
    Rename { name: String },

    /// Structural event; one per compaction operation. Parent: the owning
    /// agent's spine. Renders to chat: replaces its target's row — `of`
    /// names the event being compacted, `label` is the short marker shown
    /// in its place, `text` is `None` when the target row is *removed*
    /// and `Some` when it is *rewritten* to shorter text.
    ///
    /// **Never removes the target row from the log.** Replay builds a
    /// lookup (target id → this event) that the renderer consults instead
    /// of re-deriving the row from `of` directly — the log stays
    /// append-only, and a branch forked before the compaction still sees
    /// the original event exactly as it was, because nothing was ever
    /// deleted, only shadowed for later renders.
    ///
    /// A compacted **program** (a `Turn`) renders as a comment-only
    /// assistant turn — still valid JavaScript, still carrying its own
    /// id, saying how to fetch the original (`artifact(id)`) — rather
    /// than as a non-assistant stub. That is what keeps role alternation
    /// intact under compaction with no special case: whatever occupies
    /// the assistant's slot in the rendered transcript is still an
    /// assistant turn.
    Compacted {
        of: EventId,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },

    /// Execution event; the full, unclipped console output of one program
    /// run, logged at its terminal (success/suspend/abandon). Parent: the
    /// owning agent's spine, by the run's `Tool` result. Renders to chat:
    /// no — it is the faithful console for the log and the debugger panes
    /// (the completion/condition report carries only a clipped tail), so
    /// a finished program's console survives reload. Never sent to the LLM.
    Console { lines: Vec<String> },
}

/// Why a run handed back. Lisp's word on purpose: there, `condition` is
/// the supertype and `error` a subtype, so a condition need not be an
/// error — which is exactly the claim that a raise, a trapped error and a
/// user interrupt are rows of one table.
///
/// **Only handbacks log one.** A condition the program itself handles —
/// a caught throw, a failed call it recovered from, a fuel slice — never
/// reaches the LLM and is not one of these.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Cause {
    /// `raise(name, payload)` — the program asked for a decision.
    Raised {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    /// A trapped VM error. `resumable` is whether `resume(value)` can
    /// stand in for the failed operation, which the report must state and
    /// which only the live error knew.
    Trapped {
        kind: String,
        message: String,
        resumable: bool,
    },
    /// Posts arrived at a running program; it suspended at its next fuel
    /// slice so the branch could hear them (rule B).
    Posted { ids: Vec<EventId> },
    /// The program did not compile. No VM was built, so this run has no
    /// console and no artifacts — the repair loop.
    CompileFailed { message: String },
    /// A completion that hit `max_tokens` mid-program. **Never compile a
    /// truncated completion** — cut off wherever the token budget ran
    /// out, it may still parse and run, half-written, on a program the
    /// model never actually finished emitting, which is strictly worse
    /// than a clean compile failure the repair loop can see and retry.
    /// The harness detects this from the completion itself, before
    /// attempting to compile, and logs it directly rather than letting
    /// truncated text reach the compiler at all.
    Truncated,
    /// The process died mid-program and the VM went with it. Written by
    /// reconciliation so an interrupted run has an outcome like any other.
    Interrupted,
    /// A handler decided `return abandon()`: the suspended run is
    /// discarded rather than continued. In-flight calls it issued stay
    /// pending and still land as artifacts; only the VM is dropped.
    ///
    /// This exists because **a run must have exactly one log-visible
    /// terminal, or nothing downstream can be derived from the log
    /// alone.** `Return`'s doc states the rule for the completing case
    /// ("a program that ends without a `return` still logs `Return {
    /// value: null }`"); abandonment is the other way a run ends, and
    /// before this it logged nothing at all — so a branch that abandoned
    /// read, forever after, as still suspended.
    ///
    /// It settles exactly one frame, the same as `Return`: the frame
    /// that decided. `depth_after` pops on it for that reason, and must
    /// pop regardless of the condition's own `disposition` — the
    /// disposition describes the raise that *opened* a scope, while this
    /// cause describes a decision that *closes* one.
    Abandoned,
}

/// Whether a raise pushed a handler frame onto the stack, or handed the
/// decision to a frame already vacated. See `EventPayload::Condition`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The raising program is still on the stack, suspended, waiting on
    /// the handler's decision — ordinary deliberation. The handler
    /// pushes, decides, and pops; the raising program resumes beneath it.
    Pushed,
    /// The raising program had nothing left to do but forward the
    /// decision (a tail raise: `await raise(...)` with nothing done with
    /// the result, nothing left but the epilogue). Its VM is popped
    /// *before* the handler's is built, not stacked below it — a real
    /// tail call, not merely tail-shaped — so no frame is pushed and the
    /// handler **is** the continuation.
    Handover,
}

impl Default for Disposition {
    /// `Pushed` is the safe default for logs written before this field
    /// existed: every one of them predates the handover mechanism, so
    /// every raise in them really was deliberation. Defaulting the other
    /// way would silently un-count a frame the depth counter is relying
    /// on, turning an old log's `depth > 0` filter and decision reads
    /// wrong; defaulting to `Pushed` only ever costs an unnecessary
    /// nesting level in a render, never a miscounted depth.
    fn default() -> Self {
        Disposition::Pushed
    }
}

/// The rendered kinds — one per API role, chosen by the **variant**,
/// never by a flag.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Message {
    /// A message delivered *here* (user role). Parent: the previous event
    /// on the receiving branch's spine.
    ///
    /// **A `Post` is a delivery marker, not a copy.** The new fact it
    /// records is that this message landed here, at this position in this
    /// branch's transcript; where its body lives is `origin`.
    Post { from: Author, origin: Origin },

    /// This context's own output (assistant role). Parent: the previous
    /// event on the branch's spine. `author` is the LLM, or the user
    /// taking a turn on this branch — it renders as an assistant message
    /// either way, because the *branch* acted.
    ///
    /// Under code mode the assistant's entire turn **is** a program:
    /// `source` holds the complete JavaScript text the model emitted —
    /// there is no separate prose channel and no tool-call wrapper around
    /// it. A program that wants to speak calls `tell()`/`ask()` from
    /// inside itself (`Call::Send`); it never returns prose alongside a
    /// list of calls, because there is no second channel for the prose to
    /// live in. A user-authored restart is the same shape: `source` is
    /// either hand-typed text (the `e` gesture) or a synthesized
    /// `resume(...)`/`answer(...)` call (`v` and the answer gesture) —
    /// what's shown in the pane is exactly what ran. A compacted program
    /// still lands here as a comment-only `source`, which is what keeps
    /// role alternation intact under compaction with no special case.
    Turn {
        author: Author,
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
    },
}

/// Who authored a message. The user is an author, not an agent: they have
/// no branch of their own and speak *inside* branches.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author {
    User,
    Agent(EventId),
    Harness,
}

/// Where a delivered message's body lives.
///
/// A body is stored **once**: the `Send` holds the question and the `Post`
/// names it. The card's own pattern hands the same plan to every worker,
/// so copying would write twenty bodies for a ten-way fan-out.
///
/// The in-memory `Context` is the other side of that trade: `replay_event`
/// resolves `Sent` through the `Tree` and stores the resolved body inline,
/// so only the *log* is free of copies.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Origin {
    /// The `Send` that dispatched this delivery; the body is read there.
    /// This is also what routes an answer back to the sender's branch.
    Sent(EventId),
    /// The body inline — for the user and harness posts that have no send
    /// side.
    Direct {
        text: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        input: serde_json::Value,
        expects_reply: bool,
    },
}

impl Origin {
    /// The inline body, when this origin carries one. A `Sent` origin
    /// resolves through the `Tree` instead (`Tree::resolve_origin`).
    pub fn direct(&self) -> Option<(&str, &serde_json::Value, bool)> {
        match self {
            Origin::Direct {
                text,
                input,
                expects_reply,
            } => Some((text, input, *expects_reply)),
            Origin::Sent(_) => None,
        }
    }
}

impl Message {
    /// The message's own text: a `Turn`'s program `source`, or a `Post`
    /// whose body is inline (`Origin::Direct`). A `Post` whose body is
    /// still by-reference (`Origin::Sent`) has none — resolve it through
    /// the `Tree` first, which is what `replay_event` does when building
    /// a `Context`.
    pub fn text(&self) -> &str {
        match self {
            Message::Turn { source, .. } => source,
            Message::Post { origin, .. } => origin.direct().map(|(t, _, _)| t).unwrap_or(""),
        }
    }
}

/// What a program's branch waits on. Four **typed** kinds, not one
/// `Invoke` with a magic `name`: from a program's view they are all
/// `tools.*` calls ("subagents are tools", 8_HARNESS dec. 2, holds at the
/// API), but `dispatch_calls` interprets the name exactly once, at
/// dispatch, and everything downstream — the artifact menu,
/// reconciliation, re-attach, routing an answer home — matches on the
/// variant instead of re-parsing a string.
///
/// Every variant carries `site`, the source byte offset of its `Invoke`
/// instruction, so a report can annotate the program source per call site
/// from the log alone (`InvokeCall::site`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Call {
    /// This branch's program messaged an agent or the user — the mirror of
    /// `Post`. `tools.ask` and `tools.tell` both log one, differing only in
    /// `expects_reply`; the body lives here and the `Post` names it, so it
    /// is never copied.
    Send {
        to: Address,
        text: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        input: serde_json::Value,
        expects_reply: bool,
        site: u32,
    },
    /// This branch's program created an agent. Settled with the agent
    /// handle; the `Agent` event it roots is a child of this `Spawn`.
    Spawn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        charter: String,
        /// The child's tool allowlist; `None` inherits the caller's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<String>>,
        site: u32,
    },
    /// This branch's program forked itself — a divergent branch that
    /// inherits the caller's history rather than starting clean. Settled
    /// with the fork's handle; the `Fork` event it roots is a child of
    /// this `Call::Fork`. `fork()` is program-initiated and awaited, so it
    /// needs a call to settle, exactly `Spawn`'s existing pattern.
    ///
    /// The asymmetry with `Spawn`'s single string is intentional, not a
    /// gap: `spawn`'s string is both identity and first task — it becomes
    /// `Agent.charter` **and** the kickoff `Post` body, because a spawned
    /// agent has a role to state before it has anything to do. `fork`'s
    /// string is only the task, because a fork has no charter of its own
    /// to state — it inherits the caller's context instead, so there is
    /// nothing else for the string to be. Both verbs kick their child off
    /// in the same call that creates it: a spawned or forked agent with
    /// nothing to do never exists.
    Fork {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        task: String,
        site: u32,
    },
    /// This branch's program called a host tool.
    Invoke {
        name: String,
        args: serde_json::Value,
        site: u32,
    },
}

impl Call {
    pub fn site(&self) -> u32 {
        match self {
            Call::Send { site, .. }
            | Call::Spawn { site, .. }
            | Call::Fork { site, .. }
            | Call::Invoke { site, .. } => *site,
        }
    }
}

/// Where a `Send` is addressed. `tools.ask` with `to` omitted resolves to
/// the author of the question being answered *before* the call is logged,
/// so nothing unresolved ever reaches the log.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Address {
    /// The human driving the session. They have no branch — they speak
    /// inside branches — so a question to them is pending until their
    /// reply produces this `Send`'s `Result`.
    User,
    /// A branch, named by its root event id (an agent's `Agent`, or a
    /// `Fork`).
    Branch(EventId),
}

/// How a call settled. `Failed` is load-bearing, not a convenience: it is
/// what separates a call that **definitively did not work** from one that
/// was merely **issued** (no `Result` at all), which reconciliation must
/// read as "may have happened".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Outcome {
    /// An answer, a delivery receipt, an agent handle, or a tool result.
    Delivered(serde_json::Value),
    /// It definitively did not happen; the message is the reason.
    Failed(String),
}

impl Outcome {
    /// The delivered value, or `None` for a failure.
    pub fn value(&self) -> Option<&serde_json::Value> {
        match self {
            Outcome::Delivered(v) => Some(v),
            Outcome::Failed(_) => None,
        }
    }
}

/// One agent's reconstructed conversation along a spine — its slice of
/// the `Agent`-ancestor chain: what it is for, the system prompt it was
/// rooted with, the rendered messages on its segment of the path, and
/// what it still owes an answer to.
///
/// Bodies here are **resolved**: a `Post` that names its `Send` in the log
/// carries the body inline once it reaches a `Context`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Context {
    /// What this agent is for (`Agent.charter`).
    pub charter: String,
    /// The system prompt snapshotted at the agent's root (`Agent.system`),
    /// rebuilt into every request verbatim.
    pub system: String,
    pub messages: Vec<Message>,
    /// Posts open on this branch, oldest first: unanswered, expecting a
    /// reply, and at or after this branch's root. **Agents never close** —
    /// a branch that has answered is `Idle`, not done — so this is what
    /// "owes something" means, and it is the only such state.
    pub open: Vec<EventId>,
}

impl Context {
    /// The `input` const a program binds: the machine-bound data of the
    /// **oldest still-open post**, whole. The context sees only a
    /// bounded preview of the same value, so a caller passing a large
    /// `input` never dumps it into the callee's context.
    ///
    /// `open` is exactly this branch's obligations (18_TARGETING: only
    /// `answer(question, value)` closes one), so `open.first()` — not a
    /// scan of `messages` for the first post that merely *expected* a
    /// reply — is the post a fresh program should bind to; after the
    /// first is answered, the next program sees the next one.
    /// `Value::Null` when nothing is open.
    pub fn input(&self, tree: &Tree) -> serde_json::Value {
        let Some(&question) = self.open.first() else {
            return serde_json::Value::Null;
        };
        let Some(EventPayload::Message(msg)) = tree.events.get(&question).map(|e| &e.payload)
        else {
            return serde_json::Value::Null;
        };
        match tree.resolve(msg) {
            Message::Post { origin, .. } => origin
                .direct()
                .map(|(_, input, _)| input.clone())
                .unwrap_or(serde_json::Value::Null),
            _ => serde_json::Value::Null,
        }
    }
}

/// A handle on one active leaf of the tree: the cursor appends go
/// through, plus the reconstructed agent chain above it. The innermost
/// agent (`contexts.last()`) owns new events.
#[derive(Clone, Debug)]
pub struct Spine {
    pub leaf_id: EventId,
    pub contexts: Vec<Context>,
}

impl Spine {
    /// The context events on this spine belong to — the innermost
    /// agent's slice of the path.
    pub fn context(&self) -> &Context {
        self.contexts.last().expect("spine has no contexts")
    }
}

pub struct Tree {
    pub id_counter: u64,
    pub events: HashMap<EventId, Event>,
    pub file: Option<std::fs::File>,
    /// Derived reports, keyed by their **outcome event id** — never
    /// logged. Reports are pure functions of the log, so without a memo
    /// every request re-derives every report on the path and a session is
    /// quadratic in branch length; with it, re-derivation is amortised
    /// O(1).
    ///
    /// The `Tree` is the right home because renders happen per branch per
    /// request and the memo must outlive any one `Runner`. It is dropped
    /// wholesale when the renderer changes
    /// (`Tree::clear_report_memo`) — a memo is a cache of one renderer's
    /// output, and editing `report.rs` invalidates all of it.
    pub(crate) reports: std::cell::RefCell<ReportMemo>,
}

/// The report cache, with a counter so a test can observe that history is
/// not re-derived on every request.
pub struct ReportMemo {
    pub entries: HashMap<EventId, String>,
    /// How many reports have actually been rendered (memo misses).
    pub derivations: u64,
    /// The renderer this cache belongs to.
    pub version: u32,
}

impl Default for ReportMemo {
    fn default() -> Self {
        ReportMemo {
            entries: HashMap::new(),
            derivations: 0,
            version: crate::report::REPORT_FORMAT_VERSION,
        }
    }
}

impl EventId {
    pub fn new(value: u64) -> Self {
        Self(NonZeroU64::try_from(value).expect("expected non-zero EventId!"))
    }

    pub fn as_u64(self) -> u64 {
        let EventId(val) = self;
        val.get()
    }
}
