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

    /// Execution event; one per tool call a program makes, logged in
    /// resolution order (decision 7). Parent: the owning agent's spine,
    /// between the program's `run_program` tool-call message and its
    /// `Tool` result. Renders to chat: no — queried for replay, the
    /// artifact menu, and UI. Addressable via `tools.tool_result(id)`.
    Invoke {
        name: String,
        args: serde_json::Value,
        result: serde_json::Value,
    },

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
