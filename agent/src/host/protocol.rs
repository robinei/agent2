//! The serializable UI boundary (8_HARNESS Step 5): UIs talk to the
//! session loop *only* through these enums — `SessionCommand` into the
//! inbox, `SessionEvent` out, chunks included. The M0 CLI, the attached
//! TUI's chat pane, and any future remote client are the same consumer.

use serde::{Deserialize, Serialize};

use crate::types::{Event, EventId};

/// A frame is identified by its `FrameStart` event id.
pub type FrameId = EventId;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SessionCommand {
    /// A user message for the root frame (rejected with an error event
    /// while the frame is busy — steering mid-program is M2).
    UserTurn(String),
    /// Stop the session loop.
    Shutdown,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SessionEvent {
    /// An event appended to the log, attributed to its frame.
    Event { frame: FrameId, event: Event },
    /// Live streaming chunk (never logged).
    Chunk {
        frame: FrameId,
        thinking: bool,
        text: String,
    },
    /// Something went wrong outside a frame's own condition machinery
    /// (LLM client failure, rejected command).
    Error {
        frame: Option<FrameId>,
        message: String,
    },
}
