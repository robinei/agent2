use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::PathBuf;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
/// - **Chat events** render into LLM requests for their frame.
/// - **Execution events** are harness-internal: queried for replay,
///   artifacts, and UI — never sent to the LLM as messages.
///
/// Each variant documents its parent rule, which spine it lands on, and
/// whether it renders to chat.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum EventPayload {
    /// Chat event. Parent: the previous event on the owning frame's
    /// spine. Renders to chat: yes — `Assistant.tool_calls` carries
    /// `run_program`/`resume`, `Tool` carries their results (completion
    /// summaries, condition reports).
    Message(Message),

    /// Execution event; the branch root of a frame. Parent: the
    /// call-site event on the caller's spine (`None` for the tree
    /// root). Starts a new spine: the caller's spine continues past the
    /// call site independently. Renders to chat: no — the child frame's
    /// LLM request is rendered *from* `prompt`/`input`, and the child
    /// never sees ancestor transcripts (clean-room, decision 3).
    FrameStart {
        prompt: String,
        input: serde_json::Value,
    },

    /// Execution event; the terminal event of a frame's spine. Parent:
    /// the frame's last event. Nothing may be appended after it.
    /// Renders to chat: no — the caller records the result on its own
    /// spine (as the `tools.agent` call's `Tool` message).
    FrameResult { result: serde_json::Value },

    /// Execution event; one per tool call a program makes, logged in
    /// resolution order (decision 7). Parent: the owning frame's spine,
    /// between the program's `run_program` tool-call message and its
    /// `Tool` result. Renders to chat: no — queried for replay, the
    /// artifact menu, and UI. Addressable via `tools.tool_result(id)`.
    Invoke {
        name: String,
        args: serde_json::Value,
        result: serde_json::Value,
    },

    /// Execution event; a program's top-level `return` value, logged
    /// after each successful run. Parent: the owning frame's spine.
    /// Renders to chat: no — an id-addressable artifact like any tool
    /// result (the completion report quotes it).
    ProgramResult { value: serde_json::Value },

    /// Marker naming a branch for fork/leaf UX. Parent: the owning
    /// frame's spine. Renders to chat: no.
    Label(String),

    /// Live streaming only — never stored in the log.
    TextChunk(String),
    /// Live streaming only — never stored in the log.
    ThinkingChunk(String),
}

impl EventPayload {
    /// Whether this payload may be appended to the log (chunks are
    /// live-only).
    pub fn is_storable(&self) -> bool {
        !matches!(
            self,
            EventPayload::TextChunk(_) | EventPayload::ThinkingChunk(_)
        )
    }
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

/// One frame in a spine's `FrameStart`-ancestor chain: its prompt and
/// input, the chat messages logged on its segment of the path, and the
/// result if the frame has completed.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Frame {
    pub prompt: String,
    pub input: serde_json::Value,
    pub messages: Vec<Message>,
    pub result: Option<serde_json::Value>,
}

/// A handle on one active leaf of the tree: the cursor appends go
/// through, plus the reconstructed frame chain above it. The innermost
/// frame (`frames.last()`) owns new events.
#[derive(Clone, Debug)]
pub struct Spine {
    pub leaf_id: EventId,
    pub frames: Vec<Frame>,
}

impl Spine {
    /// The frame events on this spine belong to.
    pub fn frame(&self) -> &Frame {
        self.frames.last().expect("spine has no frames")
    }

    /// Whether this spine's frame has recorded its `FrameResult`.
    pub fn is_complete(&self) -> bool {
        self.frame().result.is_some()
    }
}

pub struct Tree {
    pub id_counter: u64,
    pub events: HashMap<EventId, Event>,
    pub file: Option<std::fs::File>,
}

pub struct Agent {
    pub config_root: PathBuf,
    pub config_file: PathBuf,
    pub trees_root: PathBuf,
    pub trees: HashMap<Uuid, Tree>,
}

pub struct AgentState {
    pub tree: Tree,
}

pub enum StepInput {
    UserTurn(String),
    ToolResults(),
    LlmResponse(),
}

pub enum StepOutput {
    LlmRequest(),
    ToolCalls(),
}

impl AgentState {
    pub fn step(&mut self, input: StepInput) -> StepOutput {
        todo!()
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

pub(crate) fn get_config_path() -> PathBuf {
    let path = std::env::home_dir().expect("unable to get home dir!");
    path.join(".agent2")
}
