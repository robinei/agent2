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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum EventPayload {
    PushFrame(String),
    PopFrame,
    Message(Message),
    Label(String),

    // not used for storage (only live streaming)
    TextChunk(String),
    ThinkingChunk(String),
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Frame {
    pub prompt: String,
    pub leaf_id: EventId,
    pub messages: Vec<Message>,
}

pub struct Tree {
    pub id_counter: u64,
    pub leaf_id: EventId,
    pub events: HashMap<EventId, Event>,
    pub frames: Vec<Frame>,
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
