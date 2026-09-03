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
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum EventPayload {
    /// Chat event. Parent: the previous event on the owning agent's
    /// spine. Renders to chat: yes — `Assistant.tool_calls` carries
    /// `run_program`/`resume`, `Tool` carries their results (completion
    /// summaries, condition reports).
    Message(Message),

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

    /// Execution event; the terminal event of an agent's spine. Parent:
    /// the agent's last event. Nothing may be appended after it.
    /// Renders to chat: no — the caller records the result on its own
    /// spine (as the `tools.agent` call's `Tool` message).
    FrameResult { result: serde_json::Value },

    /// Execution event; one per call a program issues, logged at
    /// **dispatch** (17_BRANCHES A2). Parent: the owning agent's spine,
    /// between the program's `run_program` tool-call message and its
    /// `Tool` result. Renders to chat: no — queried for replay, the
    /// artifact menu, and UI. Addressable via `tools.tool_result(id)`,
    /// which resolves a call id through to its `Result`.
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

    /// Execution event; a program's top-level `return` value, logged
    /// after each successful run. Parent: the owning agent's spine.
    /// Renders to chat: no — an id-addressable artifact like any tool
    /// result (the completion report quotes it).
    ProgramResult { value: serde_json::Value },

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

    /// Execution event; the full, unclipped console output of one program
    /// run, logged at its terminal (success/suspend/abandon). Parent: the
    /// owning agent's spine, by the run's `Tool` result. Renders to chat:
    /// no — it is the faithful console for the log and the debugger panes
    /// (the completion/condition report carries only a clipped tail), so
    /// a finished program's console survives reload. Never sent to the LLM.
    Console { lines: Vec<String> },
}

/// The rendered kinds — one per API role, chosen by the **variant**,
/// never by a flag.
#[derive(Serialize, Deserialize, Clone, Debug)]
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
    Turn {
        author: Author,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },

    /// A tool-role message answering one of a `Turn`'s tool calls.
    ///
    /// Deleted in A4: these are *rendered* from the run's outcome and the
    /// events around it, never stored.
    Tool {
        name: String,
        call_id: String,
        text: String,
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
#[derive(Serialize, Deserialize, Clone, Debug)]
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
    /// The message's own text. A `Post` whose body is still by-reference
    /// (`Origin::Sent`) has none — resolve it through the `Tree` first,
    /// which is what `replay_event` does when building a `Context`.
    pub fn text(&self) -> &str {
        match self {
            Message::Turn { text, .. } | Message::Tool { text, .. } => text,
            Message::Post { origin, .. } => origin.direct().map(|(t, _, _)| t).unwrap_or(""),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// What a program's branch waits on. Three **typed** kinds, not one
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
            Call::Send { site, .. } | Call::Spawn { site, .. } | Call::Invoke { site, .. } => *site,
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
/// rooted with, the rendered messages on its segment of the path, and the
/// result if the agent has completed.
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
    pub result: Option<serde_json::Value>,
}

impl Context {
    /// The `input` const a program binds: the machine-bound data of the
    /// **oldest open post**, whole. The context sees only a bounded
    /// preview of the same value, so a caller passing a large `input`
    /// never dumps it into the callee's context.
    ///
    /// "Open" is refined in A6 to "unanswered, at or after this branch's
    /// root"; here it is the oldest post that expects a reply.
    pub fn input(&self) -> &serde_json::Value {
        self.messages
            .iter()
            .find_map(|m| match m {
                Message::Post { origin, .. } => match origin.direct() {
                    Some((_, input, true)) => Some(input),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or(&serde_json::Value::Null)
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

    /// Whether this spine's agent has recorded its `FrameResult`.
    pub fn is_complete(&self) -> bool {
        self.context().result.is_some()
    }
}

pub struct Tree {
    pub id_counter: u64,
    pub events: HashMap<EventId, Event>,
    pub file: Option<std::fs::File>,
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
