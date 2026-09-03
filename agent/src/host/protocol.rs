//! The serializable UI boundary (8_HARNESS Step 5): UIs talk to the
//! session loop *only* through these enums — `SessionCommand` into the
//! inbox, `SessionEvent` out, chunks included. The M0 CLI, the attached
//! TUI's chat pane, and any future remote client are the same consumer.

use serde::{Deserialize, Serialize};

use crate::types::{Event, EventId};

/// An agent is identified by its `Agent` event id.
pub type AgentId = EventId;

/// A **branch** is identified by its root event: the `Agent` that roots
/// an agent's first branch, or the `Fork` that roots a divergent one.
///
/// Nothing is minted. A branch id is a log fact, so it survives a crash,
/// names the same conversation forever, and needs no session-local table
/// to interpret — which is what lets live state be keyed by it while the
/// log stays untouched by concurrency.
pub type BranchId = EventId;

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

/// One restart, as the **user** takes a branch's turn — the handler
/// hierarchy's outermost layer made literal (DESIGN.md).
///
/// These are exactly the calls the branch's own menu would offer, which
/// is the point: a `Restart` is applied through the same path an LLM
/// turn takes, so nothing downstream is special-cased. It carries values
/// where `machine::Restart` carries only the kind, because the user is
/// supplying the value.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum UserCall {
    /// Paste a rewrite: replace the program.
    RunProgram { source: String },
    /// Supply the value the suspension asked for (or nothing, when it
    /// asked for nothing and `resume()` just carries on).
    Resume {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<serde_json::Value>,
    },
    /// Answer an open post on this branch explicitly — including one a
    /// fork explored and the original still owes.
    Answer {
        question: EventId,
        value: serde_json::Value,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SessionCommand {
    /// A user message **for one branch**. The user has no branch of
    /// their own — they speak *inside* branches — so every utterance
    /// names the one it lands in, and the reply is read there.
    ///
    /// **Nothing you say is ever rejected**: the post is logged on
    /// arrival in every phase and delivered at the recipient's next safe
    /// point (rule B). `expects_reply` is the ask/tell modifier: cleared,
    /// the post lands, wakes the branch, and owes nothing.
    UserTurn {
        branch: BranchId,
        text: String,
        #[serde(default = "yes")]
        expects_reply: bool,
    },
    /// The human's reply to a branch's question. An agent asks the user
    /// with a `Send { to: user }`, which stays pending until this settles
    /// it — the mirror image of `UserTurn`, where the user asks and the
    /// branch answers.
    Reply {
        branch: BranchId,
        call: EventId,
        value: serde_json::Value,
    },
    /// The user takes a branch's turn: a `Turn { author: User }` carrying
    /// one restart, applied exactly as if the LLM had made it. Any
    /// in-flight generation on that branch is cancelled first.
    Restart { branch: BranchId, call: UserCall },
    /// Cancel an in-flight generation, or stop a running program at its
    /// next fuel slice, so a post lands **now** rather than when the
    /// generation finishes. The one override on rule B's "next safe
    /// point"; it is the debugger's pause, addressed at a branch.
    Interrupt { branch: BranchId },
    /// Create an agent under `parent` and ask it `text` — the user's own
    /// spawn, the gesture a program spells `tools.spawn` + `tools.ask`.
    Spawn {
        parent: BranchId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        charter: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Request the current leaf set (replied to with `SessionEvent::Leaves`).
    ListLeaves,
    /// Request the branch set (replied to with `SessionEvent::Branches`).
    ListBranches,
    /// Name a branch from here on. A record, not a message: renaming
    /// never wakes the branch, and it may be sent in any phase.
    Rename { branch: BranchId, name: String },
    /// **Fork adds a branch; it never moves you.** `Fork` logs a `Fork`
    /// as the new branch's root, opens it live and idle, and leaves every
    /// other branch exactly as it was. The new id comes back as
    /// `SessionEvent::BranchOpened`.
    Fork {
        from: EventId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Open the branch `leaf` sits on as a live branch (re-hydrating it
    /// if this session had no runner for it) and announce its id. It
    /// moves no cursor: there is none.
    Resume(EventId),
    /// Stop the session loop.
    Shutdown,
}

/// serde default for `UserTurn.expects_reply`: a user turn asks unless
/// it says otherwise.
fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SessionEvent {
    /// An event appended to the log, attributed to the **branch** it
    /// landed on and the agent that branch is a conversation with. Two
    /// forks of one agent share `agent` and differ in `branch`, which is
    /// exactly the distinction a navigator needs and an agent id cannot
    /// make.
    Event {
        agent: AgentId,
        branch: BranchId,
        event: Event,
    },
    /// Live streaming chunk (never logged).
    Chunk {
        agent: AgentId,
        branch: BranchId,
        thinking: bool,
        text: String,
    },
    /// A program block's live status changed (decision 5). Live-only,
    /// like `Chunk`; `program` is the `run_program` Assistant event id
    /// that keys the clickable block to its record (a `resume` keeps the
    /// originating program's id).
    ProgramStatus {
        agent: AgentId,
        branch: BranchId,
        program: EventId,
        status: ProgramStatus,
    },
    /// Something went wrong outside an agent's own condition machinery
    /// (LLM client failure, an unaddressable command).
    Error {
        branch: Option<BranchId>,
        message: String,
    },
    /// A branch is now live in this session: a `Fork` just rooted one, or
    /// a `Resume` re-hydrated one. This is how a UI learns a new branch's
    /// id — the only thing `Fork` returns, since forking moves nothing.
    BranchOpened { branch: BranchId },
    /// The tree's leaf set (reply to `ListLeaves`).
    Leaves(Vec<LeafInfo>),
    /// The tree's branch set (reply to `ListBranches`, also pushed after
    /// a `Fork`/`Resume`/`Rename` so a UI reflects the new shape).
    Branches(Vec<BranchInfo>),
    /// A branch answered a question the **user** asked. The user has no
    /// branch and no program, so there is nothing to settle: this is how
    /// a UI learns the answer landed, and it is read inline in that
    /// branch's chat.
    Answered {
        agent: AgentId,
        branch: BranchId,
        question: EventId,
        value: serde_json::Value,
    },
}

/// One branch, as a UI sees it. The navigator's row — and the unit the
/// whole session is addressed in, since **the branch is the address**.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BranchInfo {
    /// The branch's root event, and its id.
    pub branch: BranchId,
    /// The agent this branch is a conversation with. Two forks of one
    /// agent share it.
    pub agent: AgentId,
    /// The branch's current leaf (what a fork anchors on).
    pub leaf: EventId,
    /// The last `Rename` at or after the root, else the root's own name.
    /// `None` shows a derived label; naming is never automatic.
    pub name: Option<String>,
    /// The branch this one diverged from (`Fork`) or was spawned from
    /// (`Agent`); `None` for the root.
    pub parent_branch: Option<BranchId>,
    /// `idle` / `thinking` / `running` / `suspended` / `dormant` — live
    /// session state, so it is a view and never a log fact.
    pub status: String,
    /// How many posts this branch still owes an answer to.
    pub open: usize,
    /// The `Send { to: user }` this branch is waiting on, if any: the
    /// question renders inline and the navigator highlights the branch.
    pub asking_user: Option<EventId>,
    /// Whether a generation is in flight right now.
    pub thinking: bool,
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
    /// How many posts this branch still owes an answer to. Agents never
    /// close, so "owes something" is the only state there is.
    pub open: usize,
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
        roundtrip_cmd(SessionCommand::UserTurn {
            branch: id,
            text: "hi".into(),
            expects_reply: true,
        });
        roundtrip_cmd(SessionCommand::Reply {
            branch: id,
            call: id,
            value: serde_json::json!("yes"),
        });
        roundtrip_cmd(SessionCommand::Restart {
            branch: id,
            call: UserCall::Resume { value: None },
        });
        roundtrip_cmd(SessionCommand::Restart {
            branch: id,
            call: UserCall::RunProgram {
                source: "return 1;".into(),
            },
        });
        roundtrip_cmd(SessionCommand::Restart {
            branch: id,
            call: UserCall::Answer {
                question: id,
                value: serde_json::json!(5),
            },
        });
        roundtrip_cmd(SessionCommand::Interrupt { branch: id });
        roundtrip_cmd(SessionCommand::Spawn {
            parent: id,
            name: Some("worker".into()),
            charter: "read files".into(),
            text: Some("start".into()),
        });
        roundtrip_cmd(SessionCommand::ListLeaves);
        roundtrip_cmd(SessionCommand::ListBranches);
        roundtrip_cmd(SessionCommand::Rename {
            branch: id,
            name: "branch A".into(),
        });
        roundtrip_cmd(SessionCommand::Fork {
            from: id,
            name: Some("retry".into()),
        });
        roundtrip_cmd(SessionCommand::Resume(id));
        roundtrip_cmd(SessionCommand::Shutdown);

        roundtrip_evt(SessionEvent::ProgramStatus {
            agent: EventId::new(1),
            branch: EventId::new(1),
            program: id,
            status: ProgramStatus::Completed,
        });
        roundtrip_evt(SessionEvent::BranchOpened { branch: id });
        roundtrip_evt(SessionEvent::Leaves(vec![LeafInfo {
            leaf: id,
            agent: EventId::new(1),
            name: Some("branch A".into()),
            open: 1,
            summary: "Assistant: hello".into(),
        }]));
        roundtrip_evt(SessionEvent::Branches(vec![BranchInfo {
            branch: EventId::new(1),
            agent: EventId::new(1),
            leaf: id,
            name: Some("branch A".into()),
            parent_branch: None,
            status: "idle".into(),
            open: 1,
            asking_user: None,
            thinking: false,
        }]));
        roundtrip_evt(SessionEvent::Event {
            agent: EventId::new(1),
            branch: EventId::new(1),
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

    /// A `UserTurn` from an older client (or a hand-written one) still
    /// asks: the modifier is the exception, so its default is the ask.
    #[test]
    fn a_user_turn_asks_by_default() {
        let cmd: SessionCommand =
            serde_json::from_str(r#"{"UserTurn":{"branch":7,"text":"hi"}}"#).unwrap();
        let SessionCommand::UserTurn { expects_reply, .. } = cmd else {
            panic!("expected a UserTurn");
        };
        assert!(expects_reply);
    }
}
