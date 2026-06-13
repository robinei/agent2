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
    /// while the frame is busy — steering mid-program is M2 — or while
    /// the active spine is already complete — fork to continue past it).
    UserTurn(String),
    /// Request the current leaf set (replied to with `SessionEvent::Leaves`).
    ListLeaves,
    /// Label the active root spine's leaf for fork/leaf UX (idle only).
    Label(String),
    /// Fork from any event into a divergent branch and make it the active
    /// root (idle only). The optional label names the new branch.
    Fork {
        from: EventId,
        label: Option<String>,
    },
    /// Re-anchor the active root at an existing leaf (idle only; rejected
    /// if that leaf's spine is already complete).
    Resume(EventId),
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
    /// The tree's leaf set (reply to `ListLeaves`, also pushed after a
    /// `Fork`/`Resume`/`Label` so the UI reflects the new active leaf).
    Leaves(Vec<LeafInfo>),
}

/// One resumable leaf, for fork/leaf UX. The serializable summary a UI
/// renders without reaching past the protocol into the `Tree`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LeafInfo {
    /// The leaf event itself (what `Resume`/`fork` anchor on).
    pub leaf: EventId,
    /// The innermost `FrameStart` above the leaf (which frame it belongs to).
    pub frame: FrameId,
    /// Nearest `Label` on the spine, if any.
    pub label: Option<String>,
    /// Whether the leaf's spine has recorded its `FrameResult`.
    pub complete: bool,
    /// Whether this is the session's current active root leaf.
    pub active: bool,
    /// One-line preview of the leaf event (kind + clipped content).
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Event, EventPayload, Message};
    use jiff::Timestamp;

    fn roundtrip_cmd(cmd: SessionCommand) {
        let json = serde_json::to_string(&cmd).unwrap();
        let _: SessionCommand = serde_json::from_str(&json).unwrap();
    }

    fn roundtrip_evt(evt: SessionEvent) {
        let json = serde_json::to_string(&evt).unwrap();
        let _: SessionEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn new_commands_and_events_round_trip() {
        let id = EventId::new(7);
        roundtrip_cmd(SessionCommand::UserTurn("hi".into()));
        roundtrip_cmd(SessionCommand::ListLeaves);
        roundtrip_cmd(SessionCommand::Label("branch A".into()));
        roundtrip_cmd(SessionCommand::Fork {
            from: id,
            label: Some("retry".into()),
        });
        roundtrip_cmd(SessionCommand::Resume(id));
        roundtrip_cmd(SessionCommand::Shutdown);

        roundtrip_evt(SessionEvent::Leaves(vec![LeafInfo {
            leaf: id,
            frame: EventId::new(1),
            label: Some("branch A".into()),
            complete: false,
            active: true,
            summary: "Assistant: hello".into(),
        }]));
        roundtrip_evt(SessionEvent::Event {
            frame: EventId::new(1),
            event: Event {
                id,
                parent_id: Some(EventId::new(1)),
                timestamp: Timestamp::UNIX_EPOCH,
                payload: EventPayload::Message(Message::User { text: "x".into() }),
            },
        });
    }
}
