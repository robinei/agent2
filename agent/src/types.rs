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

    /// Execution event; the branch root of an agent. Parent: the
    /// call-site event on the caller's spine (`None` for the tree
    /// root). Starts a new spine: the caller's spine continues past the
    /// call site independently. Renders to chat: no — the child agent's
    /// LLM request is rendered *from* `prompt`/`input`, and the child
    /// never sees ancestor transcripts (clean-room, decision 3).
    Agent {
        prompt: String,
        input: serde_json::Value,
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

    /// Marker naming a branch for fork/leaf UX. Parent: the owning
    /// agent's spine. Renders to chat: no.
    Label(String),

    /// Execution event; the full, unclipped console output of one program
    /// run, logged at its terminal (success/suspend/abandon). Parent: the
    /// owning agent's spine, by the run's `Tool` result. Renders to chat:
    /// no — it is the faithful console for the log and the debugger panes
    /// (the completion/condition report carries only a clipped tail), so
    /// a finished program's console survives reload. Never sent to the LLM.
    Console { lines: Vec<String> },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Message {
    User {
        text: String,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    System {
        text: String,
    },
    Tool {
        name: String,
        call_id: String,
        text: String,
    },
}

impl Message {
    pub fn text(&self) -> &str {
        match self {
            Message::User { text }
            | Message::Assistant { text, .. }
            | Message::System { text }
            | Message::Tool { text, .. } => text,
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
/// the `Agent`-ancestor chain: the prompt and input it was rooted with,
/// the chat messages logged on its segment of the path, and the result
/// if the agent has completed.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Context {
    pub prompt: String,
    pub input: serde_json::Value,
    pub messages: Vec<Message>,
    pub result: Option<serde_json::Value>,
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
