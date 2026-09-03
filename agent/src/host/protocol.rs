//! The serializable UI boundary (8_HARNESS Step 5): UIs talk to the
//! session loop *only* through these enums — `SessionCommand` into the
//! inbox, `SessionEvent` out, chunks included. The M0 CLI, the attached
//! TUI's chat pane, and any future remote client are the same consumer.

use serde::{Deserialize, Serialize};

use crate::types::{Event, EventId};

/// An agent is identified by its `Agent` event id.
pub type AgentId = EventId;

/// The status of a program block (decision 5). Carried by
/// `SessionEvent::ProgramStatus` so the chat pane — which is VM-free and
/// cannot read `Phase` — can title each `run_program` block live.
///
/// It is no longer live-*only*: with exactly one outcome event per
/// handback, `ProgramView::status` derives the same answer from a
/// reopened log, so suspended-vs-failed survives a restart.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramStatus {
    /// Executing (or resumed and executing again).
    Running,
    /// Raised a condition / trapped; awaiting a `resume` or rewrite.
    Suspended,
    /// Returned a value (ran to completion).
    Completed,
    /// Abandoned — rewritten away or left suspended when the agent ended.
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SessionCommand {
    /// A user message for the root agent (rejected with an error event
    /// while the agent is busy — steering mid-program is M2 — or while
    /// the active spine is already complete — fork to continue past it).
    UserTurn(String),
    /// Request the current leaf set (replied to with `SessionEvent::Leaves`).
    ListLeaves,
    /// Name the active branch from here on (idle only). A record, not a
    /// message: renaming never wakes the branch.
    Rename(String),
    /// Fork from any event into a divergent branch and make it the active
    /// root (idle only). The optional name names the new branch.
    Fork { from: EventId, name: Option<String> },
    /// Re-anchor the active root at an existing leaf (idle only; rejected
    /// if that leaf's spine is already complete).
    Resume(EventId),
    /// Stop the session loop.
    Shutdown,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SessionEvent {
    /// An event appended to the log, attributed to its agent.
    Event { agent: AgentId, event: Event },
    /// Live streaming chunk (never logged).
    Chunk {
        agent: AgentId,
        thinking: bool,
        text: String,
    },
    /// A program block's live status changed (decision 5). Live-only,
    /// like `Chunk`; `program` is the `run_program` Assistant event id
    /// that keys the clickable block to its record (a `resume` keeps the
    /// originating program's id).
    ProgramStatus {
        agent: AgentId,
        program: EventId,
        status: ProgramStatus,
    },
    /// Something went wrong outside an agent's own condition machinery
    /// (LLM client failure, rejected command).
    Error {
        agent: Option<AgentId>,
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
    /// The innermost `Agent` above the leaf (which agent it belongs to).
    pub agent: AgentId,
    /// The branch's name: the last `Rename` at or after its root, else
    /// the root's own name. One concept, one field.
    pub name: Option<String>,
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
    use crate::types::{Author, Event, EventPayload, Message, Origin};
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
        roundtrip_cmd(SessionCommand::Rename("branch A".into()));
        roundtrip_cmd(SessionCommand::Fork {
            from: id,
            name: Some("retry".into()),
        });
        roundtrip_cmd(SessionCommand::Resume(id));
        roundtrip_cmd(SessionCommand::Shutdown);

        roundtrip_evt(SessionEvent::ProgramStatus {
            agent: EventId::new(1),
            program: id,
            status: ProgramStatus::Completed,
        });
        roundtrip_evt(SessionEvent::Leaves(vec![LeafInfo {
            leaf: id,
            agent: EventId::new(1),
            name: Some("branch A".into()),
            complete: false,
            active: true,
            summary: "Assistant: hello".into(),
        }]));
        roundtrip_evt(SessionEvent::Event {
            agent: EventId::new(1),
            event: Event {
                id,
                parent_id: Some(EventId::new(1)),
                timestamp: Timestamp::UNIX_EPOCH,
                payload: EventPayload::Message(Message::Post {
                    from: Author::User,
                    origin: Origin::Direct {
                        text: "x".into(),
                        input: serde_json::Value::Null,
                        expects_reply: true,
                    },
                }),
            },
        });
    }
}
