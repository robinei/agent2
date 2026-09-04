//! Chat transcript state (9_TUI Step 4 · 11_INTROSPECT Step 3 ·
//! 17_BRANCHES Step D1), driven **exclusively** by `SessionEvent`s — the
//! serializable boundary a remote client would consume. Enforced
//! structurally, not by discipline: `ChatState`'s fields are private to
//! this module and its only mutator is `apply(&SessionEvent)`, so nothing
//! privileged (VMs, tree, session) can leak into what this pane shows. Do
//! not add imports from `crate::host` beyond the protocol types, and none
//! from `interp`.
//!
//! A `run_program` execution renders as one **block** (decision 2): a
//! `run_program: <status>` header (status tracked live from
//! `ProgramStatus`) with the program's inner `Invoke`s listed beneath as
//! `⚙` lines; a `resume` folds into the same block. The completion/
//! condition report body is *not* inlined — it lives in the right
//! console/result pane. The transcript is **per-branch** (17_BRANCHES):
//! `rows(branch)` renders that branch's own slice plus — for a forked
//! branch — the shared prefix it inherited, reconstructed from the event
//! stream alone (`fork_parent`, below), since a fork carries *history,
//! not obligations* and this pane never reaches past the protocol into
//! the `Tree` to get it.

use std::collections::HashMap;

use crate::host::{AgentId, BranchId, ProgramStatus, SessionEvent};
use crate::types::{Address, Call, Event, EventId, EventPayload, Message, Outcome};

/// What a transcript row is, for styling by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatKind {
    /// The agent's stored system prompt (a collapsible header).
    System,
    User,
    Assistant,
    /// In-flight streamed text (replaced by the logged message).
    Streaming,
    /// A `run_program` block header or one of its `⚙` inner-call lines.
    ToolCall,
    /// An attachment line within a program block.
    Attachment,
    /// Context lifecycle markers.
    Marker,
    Error,
}

/// Click-hit metadata for a transcript row.
#[derive(Debug, Clone, PartialEq)]
pub enum RowDetail {
    /// No special click target.
    None,
    /// A program block header row — clicking selects the program.
    Program(EventId),
    /// An attachment row — clicking selects the program + that attachment.
    Attachment(EventId, String),
    /// An invoke row — clicking selects the program + that invoke.
    Invoke(EventId, usize),
}

/// One transcript entry. A `Header` computes its text live from the
/// program's `ProgramStatus`; every other entry is a fixed line.
enum Entry {
    Line {
        /// The branch this entry's event landed on — the render key.
        branch: BranchId,
        /// This entry's own event id, so a forked branch's rendering can
        /// tell "before the fork" from "after" (`rows`, below).
        id: EventId,
        kind: ChatKind,
        text: String,
        /// The program this row belongs to (for click hit-testing): the
        /// `run_program` event id for block rows, the `System` event id
        /// for the system header, else `None`.
        program: Option<EventId>,
    },
    /// A `run_program` block header, keyed by the program's event id.
    Header {
        branch: BranchId,
        program: EventId,
        /// Attachment names in definition order.
        attachments: Vec<String>,
    },
}

#[derive(Default)]
pub struct ChatState {
    entries: Vec<Entry>,
    /// Accumulating streamed text per branch, shown until the logged
    /// assistant message replaces it.
    streaming: Vec<(BranchId, String)>,
    /// The first branch seen — the default transcript when none is
    /// selected.
    main_branch: Option<BranchId>,
    /// The root agent, for the "is this a subagent" check on `Answer`
    /// markers — an agent-level fact, not a branch-level one, so it stays
    /// separate from `main_branch`.
    main_agent: Option<AgentId>,
    /// Transcript row index of each logged `Call`, so its `Result` can
    /// complete the row in place rather than pushing a second line.
    call_rows: HashMap<EventId, usize>,
    /// The open `run_program` block per branch: its inner calls and a
    /// folding `resume` attach here.
    current_program: HashMap<BranchId, EventId>,
    /// Live status per program block, titling its header.
    program_status: HashMap<EventId, ProgramStatus>,
    /// Attachment content per program: program_id → (name → content).
    pub attachment_content: HashMap<EventId, HashMap<String, String>>,
    /// Every event's own branch, by id — including events that never
    /// become a chat row (`Call`, `Result`, `Console`, …). What lets a
    /// `Fork`'s parent branch be resolved from `event.parent_id` alone.
    event_branch: HashMap<EventId, BranchId>,
    /// A forked branch → (the branch it forked from, the fork-point event
    /// id on that branch). `rows` walks this to reconstruct the inherited
    /// prefix without ever touching the `Tree`.
    fork_parent: HashMap<BranchId, (BranchId, EventId)>,
}

impl ChatState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Chunk {
                branch,
                thinking,
                text,
                ..
            } => {
                if *thinking {
                    return; // thinking stays live-only and unrendered for now
                }
                match self.streaming.iter_mut().find(|(b, _)| b == branch) {
                    Some((_, buf)) => buf.push_str(text),
                    None => self.streaming.push((*branch, text.clone())),
                }
            }
            SessionEvent::Error { branch, message } => {
                if let Some(b) = branch.or(self.main_branch) {
                    // Not tied to a log position — a live notification,
                    // not history — so it is visible only on an exact
                    // branch match, never inherited by a descendant fork.
                    self.entries.push(Entry::Line {
                        branch: b,
                        id: EventId::new(u64::MAX),
                        kind: ChatKind::Error,
                        text: format!("error: {message}"),
                        program: None,
                    });
                }
            }
            // The answer to a user's question is already this branch's
            // `Turn` in the transcript — the event only says it landed.
            SessionEvent::Answered { .. } => {}
            SessionEvent::Event {
                agent,
                branch,
                event,
            } => {
                self.event_branch.insert(event.id, *branch);
                self.apply_payload(*agent, *branch, event);
            }
            // Live program-block status titles the matching header.
            SessionEvent::ProgramStatus {
                program, status, ..
            } => {
                self.program_status.insert(*program, *status);
            }
            // Leaf/branch lists are for the navigator, not the chat pane;
            // a branch opening is the navigator's news too.
            SessionEvent::Leaves(_)
            | SessionEvent::Branches(_)
            | SessionEvent::BranchOpened { .. } => {}
        }
    }

    fn apply_payload(&mut self, agent: AgentId, branch: BranchId, event: &Event) {
        let id = event.id;
        match &event.payload {
            EventPayload::Agent { system, .. } => {
                // The system prompt is a snapshot on the root, not a
                // message: render it as this branch's leading block.
                self.main_branch.get_or_insert(branch);
                self.main_agent.get_or_insert(agent);
                self.entries.push(Entry::Line {
                    branch,
                    id,
                    kind: ChatKind::System,
                    text: system.clone(),
                    program: Some(id),
                });
            }
            // A subagent answering is a marker, not an ending: agents
            // never close, so the branch stays addressable after it.
            EventPayload::Answer { value, .. } => {
                if Some(agent) != self.main_agent {
                    self.entries.push(Entry::Line {
                        branch,
                        id,
                        kind: ChatKind::Marker,
                        text: format!("subagent answered: {}", short(value)),
                        program: None,
                    });
                }
            }
            EventPayload::Fork { name } => {
                // This branch's own root *is* the Fork event, so its id
                // is `id`/`branch` alike; `event.parent_id` is the fork
                // point on the branch it diverged from.
                if let Some(parent_point) = event.parent_id
                    && let Some(&parent_branch) = self.event_branch.get(&parent_point)
                {
                    self.fork_parent
                        .insert(branch, (parent_branch, parent_point));
                }
                self.entries.push(Entry::Line {
                    branch,
                    id,
                    kind: ChatKind::Marker,
                    text: format!(
                        "forked{}",
                        match name {
                            Some(n) => format!(" as «{n}»"),
                            None => String::new(),
                        }
                    ),
                    program: None,
                });
            }
            EventPayload::Message(Message::Post { from, origin }) => {
                self.entries.push(Entry::Line {
                    branch,
                    id,
                    kind: ChatKind::User,
                    text: crate::report::render_post(id, *from, origin),
                    program: None,
                });
            }
            EventPayload::Message(Message::Turn {
                author,
                text,
                tool_calls,
                ..
            }) => {
                self.streaming.retain(|(b, _)| *b != branch);
                // A `Turn { author: User }` is the user taking this
                // branch's turn (`Restart`). It renders as an assistant
                // message to the API — the *branch* acted — but the
                // person driving needs to see whose hand it was.
                let by_user = matches!(author, crate::types::Author::User);
                if !text.is_empty() || by_user {
                    let calls: Vec<&str> = tool_calls.iter().map(|c| c.name.as_str()).collect();
                    self.entries.push(Entry::Line {
                        branch,
                        id,
                        kind: if by_user {
                            ChatKind::Marker
                        } else {
                            ChatKind::Assistant
                        },
                        text: if by_user {
                            format!("you took this branch's turn: {}", calls.join(", "))
                        } else {
                            text.clone()
                        },
                        program: None,
                    });
                }
                // A `run_program` opens a new block keyed by this event;
                // a `resume` folds into the open one (decision 2).
                if let Some(call) = tool_calls.first()
                    && call.name == crate::machine::TOOL_RUN_PROGRAM
                {
                    let attachment_names: Vec<String> = call
                        .arguments
                        .get("attachments")
                        .and_then(|v| v.as_object())
                        .map(|obj| obj.keys().cloned().collect())
                        .unwrap_or_default();
                    let attachments: HashMap<String, String> = call
                        .arguments
                        .get("attachments")
                        .and_then(|v| v.as_object())
                        .map(|obj| {
                            obj.iter()
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    self.entries.push(Entry::Header {
                        branch,
                        program: id,
                        attachments: attachment_names,
                    });
                    self.current_program.insert(branch, id);
                    if !attachments.is_empty() {
                        self.attachment_content.insert(id, attachments);
                    }
                }
            }
            // A call is logged at dispatch, so its row appears the moment
            // it is issued; the `Result` completes the same row in place.
            // `wait_until` is exempt: a polling loop calls it repeatedly for
            // no reason a human watching needs to see, and it never has
            // interesting output — the log itself is untouched, only this
            // derived transcript view.
            EventPayload::Call(call) => {
                // A message to the user is a message, not a tool call — it
                // renders like the agent just spoke, same as a plain-text
                // Turn, with no `⚙ .../→ ...` tool-call framing. This is the
                // only place that text could ever be read; a Send to
                // another branch stays a compact tool-call summary, since
                // it's still readable in full there as a real Post.
                if let Call::Send {
                    to: Address::User,
                    text,
                    ..
                } = call
                {
                    self.entries.push(Entry::Line {
                        branch,
                        id,
                        kind: ChatKind::Assistant,
                        text: text.clone(),
                        program: None,
                    });
                } else {
                    let is_wait_until =
                        matches!(call, Call::Invoke { name, .. } if name.as_str() == "wait_until");
                    if !is_wait_until && let Some(&program) = self.current_program.get(&branch) {
                        let name = match call {
                            Call::Invoke { name, .. } => name.clone(),
                            Call::Send {
                                to: Address::Branch(id),
                                text,
                                expects_reply,
                                ..
                            } => {
                                let verb = if *expects_reply { "ask" } else { "tell" };
                                format!("{verb} #{}: {text}", id.as_u64())
                            }
                            Call::Send {
                                to: Address::User, ..
                            } => unreachable!("handled above"),
                            Call::Spawn { .. } => "spawn".to_owned(),
                        };
                        self.call_rows.insert(id, self.entries.len());
                        self.entries.push(Entry::Line {
                            branch,
                            id,
                            kind: ChatKind::ToolCall,
                            text: format!("⚙ {name} → …"),
                            program: Some(program),
                        });
                    }
                }
            }
            EventPayload::Result { call, outcome } => {
                if let Some(&row) = self.call_rows.get(call)
                    && let Some(Entry::Line { text, .. }) = self.entries.get_mut(row)
                {
                    let head = text.rsplit_once(" → ").map(|(h, _)| h.to_owned());
                    if let Some(head) = head {
                        *text = match outcome {
                            Outcome::Delivered(v) => format!("{head} → {}", short(v)),
                            Outcome::Failed(msg) => format!("{head} → failed: {msg}"),
                        };
                    }
                }
            }
            // Execution/marker events are debug-pane data, never transcript.
            // The report body lives in the right console/result pane, not
            // the transcript (decision 2); it is derived from these.
            EventPayload::Return { .. }
            | EventPayload::Condition { .. }
            | EventPayload::Console { .. } => {}
            // A rename is a record: it changes the navigator, never the
            // transcript, and never wakes the branch.
            EventPayload::Rename { .. } => {}
        }
    }

    /// The ancestor chain from `target` back to its root-most branch,
    /// with the cutoff event id each non-final ancestor's own entries are
    /// bounded by — the event at which the *next* branch in the chain
    /// diverged from it. Reconstructs "history crosses a fork,
    /// obligations do not" from the event stream alone.
    fn ancestry(&self, target: BranchId) -> HashMap<BranchId, Option<EventId>> {
        let mut chain = HashMap::new();
        chain.insert(target, None);
        let mut cur = target;
        while let Some(&(parent, fork_point)) = self.fork_parent.get(&cur) {
            chain.insert(parent, Some(fork_point));
            cur = parent;
        }
        chain
    }

    /// Transcript rows for `branch` (or the main branch when `None`): one
    /// `(kind, line, detail, id)` per visual line — this branch's own
    /// events plus, for a fork, the shared prefix it inherited. `detail`
    /// carries click-hit metadata: which program, attachment, or invoke a
    /// row targets. `id` is the row's own underlying event — what a
    /// "fork at this point" gesture forks from (D2); a still-streaming
    /// row carries no logged id yet, so it gets a sentinel no branch root
    /// can ever equal. Multi-line items split; the system prompt
    /// collapses to a single header row.
    pub fn rows(&self, branch: Option<BranchId>) -> Vec<(ChatKind, String, RowDetail, EventId)> {
        let Some(target) = branch.or(self.main_branch) else {
            return Vec::new();
        };
        let chain = self.ancestry(target);
        let visible = |branch: BranchId, id: EventId| match chain.get(&branch) {
            None => false,
            Some(None) => true,
            Some(Some(cutoff)) => id.as_u64() <= cutoff.as_u64(),
        };
        let mut out = Vec::new();
        let mut invoke_index: HashMap<EventId, usize> = HashMap::new();
        for entry in &self.entries {
            match entry {
                Entry::Header {
                    branch,
                    program,
                    attachments,
                } if visible(*branch, *program) => {
                    let status = self
                        .program_status
                        .get(program)
                        .map(|s| status_label(*s))
                        .unwrap_or("running");
                    out.push((
                        ChatKind::ToolCall,
                        format!("run_program: {status}"),
                        RowDetail::Program(*program),
                        *program,
                    ));
                    for name in attachments {
                        out.push((
                            ChatKind::Attachment,
                            format!("⬡ attachment: {name}"),
                            RowDetail::Attachment(*program, name.clone()),
                            *program,
                        ));
                    }
                }
                Entry::Line {
                    branch,
                    id,
                    kind,
                    text,
                    program,
                } if visible(*branch, *id) => {
                    if *kind == ChatKind::System {
                        out.push((ChatKind::System, "system".into(), RowDetail::None, *id));
                        continue;
                    }
                    let detail = if *kind == ChatKind::ToolCall {
                        if let Some(pid) = program {
                            let idx = invoke_index.entry(*pid).or_insert(0);
                            let d = RowDetail::Invoke(*pid, *idx);
                            *idx += 1;
                            d
                        } else {
                            RowDetail::None
                        }
                    } else {
                        RowDetail::None
                    };
                    push_wrapped(&mut out, *kind, text, detail, *id);
                }
                _ => {}
            }
        }
        for (b, buf) in &self.streaming {
            if *b != target {
                continue;
            }
            for line in buf.lines() {
                out.push((
                    ChatKind::Streaming,
                    line.to_owned(),
                    RowDetail::None,
                    EventId::new(u64::MAX),
                ));
            }
        }
        out
    }
}

/// Push an item's visual lines, labelling user/assistant prose and
/// indenting continuation lines under the label.
fn push_wrapped(
    out: &mut Vec<(ChatKind, String, RowDetail, EventId)>,
    kind: ChatKind,
    text: &str,
    detail: RowDetail,
    id: EventId,
) {
    let label = match kind {
        ChatKind::User => "you ❯ ",
        ChatKind::Assistant => "agent ❯ ",
        _ => "",
    };
    let mut any = false;
    for (i, line) in text.lines().enumerate() {
        any = true;
        let head = if i == 0 {
            label.to_owned()
        } else {
            " ".repeat(label.chars().count())
        };
        out.push((kind, format!("{head}{line}"), detail.clone(), id));
    }
    if !any {
        out.push((kind, label.to_owned(), detail, id));
    }
}

fn status_label(status: ProgramStatus) -> &'static str {
    match status {
        ProgramStatus::Running => "running",
        ProgramStatus::Suspended => "suspended",
        ProgramStatus::Completed => "completed",
        ProgramStatus::Failed => "failed",
    }
}

/// A compact one-line preview of an inner call's result.
fn short(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s.len() <= 40 {
        return s;
    }
    let mut end = 40;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Author, Origin, ToolCall};
    use jiff::Timestamp;

    fn ev(id: u64, payload: EventPayload) -> SessionEvent {
        ev_on(1, id, None, payload)
    }

    fn ev_on(branch: u64, id: u64, parent: Option<u64>, payload: EventPayload) -> SessionEvent {
        SessionEvent::Event {
            agent: EventId::new(1),
            branch: EventId::new(branch),
            event: Event {
                id: EventId::new(id),
                parent_id: parent.map(EventId::new),
                timestamp: Timestamp::now(),
                payload,
            },
        }
    }

    fn run_program(id: u64) -> SessionEvent {
        ev(
            id,
            EventPayload::Message(Message::Turn {
                author: Author::Agent(EventId::new(1)),
                text: String::new(),
                thinking: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "run_program".into(),
                    arguments: serde_json::json!({}),
                }],
            }),
        )
    }

    fn invoke(id: u64, name: &str) -> SessionEvent {
        ev(
            id,
            EventPayload::Call(Call::Invoke {
                name: name.into(),
                args: serde_json::json!([]),
                site: 0,
            }),
        )
    }

    fn settled(id: u64, call: u64, result: serde_json::Value) -> SessionEvent {
        ev(
            id,
            EventPayload::Result {
                call: EventId::new(call),
                outcome: Outcome::Delivered(result),
            },
        )
    }

    fn post(id: u64, text: &str) -> SessionEvent {
        ev(
            id,
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: text.into(),
                    input: serde_json::Value::Null,
                    expects_reply: true,
                },
            }),
        )
    }

    #[test]
    fn transcript_builds_from_session_events_only() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "be helpful".into(),
                tools: None,
                system: String::new(),
            },
        ));
        chat.apply(&post(2, "hi"));
        chat.apply(&SessionEvent::Chunk {
            branch: EventId::new(1),
            agent: EventId::new(1),
            thinking: false,
            text: "thinki".into(),
        });
        let rows = chat.rows(None);
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::User && t.contains("hi"))
        );
        assert!(
            rows.iter()
                .any(|(k, t, _, _)| *k == ChatKind::Streaming && t.contains("thinki"))
        );

        // The logged assistant message replaces the stream.
        chat.apply(&run_program(3));
        let rows = chat.rows(None);
        assert!(!rows.iter().any(|(k, _, _, _)| *k == ChatKind::Streaming));
        // A run_program renders as a status-titled block header.
        assert!(rows.iter().any(|(k, t, p, _)| *k == ChatKind::ToolCall
            && t == "run_program: running"
            && *p == RowDetail::Program(EventId::new(3))));

        // Return/Rename never reach the transcript.
        let before = chat.rows(None).len();
        chat.apply(&ev(
            5,
            EventPayload::Return {
                value: serde_json::json!("done"),
            },
        ));
        chat.apply(&ev(
            6,
            EventPayload::Rename {
                name: "note".into(),
            },
        ));
        assert_eq!(chat.rows(None).len(), before);
    }

    /// A run_program with two inner calls renders as one block: a
    /// status-titled header + two `⚙` lines, the header tracking
    /// `ProgramStatus`. No report body in the rows.
    #[test]
    fn run_program_block_lists_inner_calls_and_tracks_status() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "p".into(),
                tools: None,
                system: String::new(),
            },
        ));
        chat.apply(&run_program(2));
        chat.apply(&invoke(3, "fetch"));
        chat.apply(&invoke(4, "store"));
        // The `Result`s complete the rows already pushed at dispatch —
        // two calls stay two lines, not four.
        chat.apply(&settled(5, 3, serde_json::json!("A")));
        chat.apply(&settled(6, 4, serde_json::json!(true)));

        let rows = chat.rows(None);
        let glyphs: Vec<&(ChatKind, String, RowDetail, EventId)> = rows
            .iter()
            .filter(|(k, t, _, _)| *k == ChatKind::ToolCall && t.starts_with('⚙'))
            .collect();
        assert_eq!(glyphs.len(), 2, "two inner-call lines");
        // Each ⚙ line carries the program id + invoke index for hit-testing.
        assert!(
            glyphs
                .iter()
                .all(|(_, _, p, _)| matches!(p, RowDetail::Invoke(_, _)))
        );
        assert!(
            glyphs
                .iter()
                .any(|(_, _, p, _)| *p == RowDetail::Invoke(EventId::new(2), 0))
        );
        assert!(
            glyphs
                .iter()
                .any(|(_, _, p, _)| *p == RowDetail::Invoke(EventId::new(2), 1))
        );

        // Header starts at running…
        assert!(
            chat.rows(None)
                .iter()
                .any(|(_, t, _, _)| t == "run_program: running")
        );
        // …and tracks ProgramStatus to completed.
        chat.apply(&SessionEvent::ProgramStatus {
            branch: EventId::new(1),
            agent: EventId::new(1),
            program: EventId::new(2),
            status: ProgramStatus::Completed,
        });
        assert!(
            chat.rows(None)
                .iter()
                .any(|(_, t, _, _)| t == "run_program: completed")
        );

        // The report body is never in the transcript — and now it is
        // never even an event: it is derived from the outcome.
        assert!(
            !chat
                .rows(None)
                .iter()
                .any(|(_, t, _, _)| t.contains("program completed"))
        );
    }

    /// `wait_until` never becomes a row — a polling loop calling it
    /// repeatedly would otherwise spam the transcript with lines no one
    /// watching needs to see. Its `Result` is likewise silent (no row was
    /// ever inserted for `call_rows` to find). A sibling call is unaffected.
    #[test]
    fn wait_until_calls_are_never_transcript_rows() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "p".into(),
                tools: None,
                system: String::new(),
            },
        ));
        chat.apply(&run_program(2));
        chat.apply(&invoke(3, "wait_until"));
        chat.apply(&invoke(4, "fetch"));
        chat.apply(&settled(5, 3, serde_json::json!(null)));
        chat.apply(&settled(6, 4, serde_json::json!("A")));

        let glyphs: Vec<String> = chat
            .rows(None)
            .into_iter()
            .filter(|(k, t, _, _)| *k == ChatKind::ToolCall && t.starts_with('⚙'))
            .map(|(_, t, _, _)| t)
            .collect();
        assert_eq!(glyphs, vec!["⚙ fetch → \"A\""]);
    }

    /// A `tell`/`ask` to the user must show its actual text — it is the
    /// only place that message could ever be read (unlike a Send to
    /// another branch, visible there too as a real `Post`).
    #[test]
    fn tell_and_ask_to_the_user_render_as_plain_assistant_messages() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "p".into(),
                tools: None,
                system: String::new(),
            },
        ));
        chat.apply(&run_program(2));
        chat.apply(&ev(
            3,
            EventPayload::Call(Call::Send {
                to: Address::User,
                text: "hello 👋".into(),
                input: serde_json::Value::Null,
                expects_reply: false,
                site: 0,
            }),
        ));
        chat.apply(&ev(
            4,
            EventPayload::Call(Call::Send {
                to: Address::User,
                text: "what's your name?".into(),
                input: serde_json::Value::Null,
                expects_reply: true,
                site: 0,
            }),
        ));

        // Neither is a `⚙` tool-call row at all — both are plain
        // `Assistant`-kind messages, exactly like the agent's own text.
        assert!(
            !chat
                .rows(None)
                .iter()
                .any(|(k, t, _, _)| *k == ChatKind::ToolCall && t.starts_with('⚙'))
        );
        let messages: Vec<String> = chat
            .rows(None)
            .into_iter()
            .filter(|(k, _, _, _)| *k == ChatKind::Assistant)
            .map(|(_, t, _, _)| t)
            .collect();
        assert_eq!(
            messages,
            vec!["agent ❯ hello 👋", "agent ❯ what's your name?"]
        );
    }

    /// `Agent.system` — the snapshot on the branch root — renders as the
    /// leading `system` row of its branch, and selecting another agent's
    /// branch shows that branch's slice (its own system block), not the
    /// root's.
    #[test]
    fn system_block_is_leading_and_per_branch() {
        let mut chat = ChatState::new();
        // Root branch #1.
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "root".into(),
                tools: None,
                system: "ROOT SYSTEM PROMPT".into(),
            },
        ));
        chat.apply(&post(3, "root q"));
        // Subagent branch #4 with its own system prompt.
        let child = EventId::new(4);
        chat.apply(&ev_on(
            4,
            4,
            None,
            EventPayload::Agent {
                name: None,
                charter: "child".into(),
                tools: None,
                system: "CHILD SYSTEM PROMPT".into(),
            },
        ));

        // Root's slice: leading system row, then the user message.
        let root_rows = chat.rows(Some(EventId::new(1)));
        assert_eq!(root_rows[0].0, ChatKind::System);
        assert_eq!(root_rows[0].2, RowDetail::None);
        assert!(
            root_rows
                .iter()
                .any(|(k, t, _, _)| *k == ChatKind::User && t.contains("root q"))
        );
        // The child's content is not in the root's slice.
        assert!(!root_rows.iter().any(|(_, t, _, _)| t.contains("CHILD")));

        // The child's slice leads with its own system header.
        let child_rows = chat.rows(Some(child));
        assert_eq!(child_rows[0].0, ChatKind::System);
        assert_eq!(child_rows[0].2, RowDetail::None);
        assert!(!child_rows.iter().any(|(_, t, _, _)| t.contains("root q")));
    }

    /// A fork's own rows include the shared prefix up to (and including)
    /// its fork point, but nothing the original branch does afterward —
    /// "history crosses a fork, obligations do not" (17_BRANCHES),
    /// reconstructed here from the event stream alone.
    #[test]
    fn fork_inherits_prefix_not_the_original_s_future() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "root".into(),
                tools: None,
                system: "SYS".into(),
            },
        ));
        chat.apply(&post(2, "shared question"));
        // The fork point: an assistant turn on the original branch.
        chat.apply(&ev_on(
            1,
            3,
            Some(2),
            EventPayload::Message(Message::Turn {
                author: Author::Agent(EventId::new(1)),
                text: "shared answer".into(),
                thinking: None,
                tool_calls: vec![],
            }),
        ));
        // Fork at #3: branch id 10, rooted with parent_id = 3.
        chat.apply(&ev_on(
            10,
            10,
            Some(3),
            EventPayload::Fork {
                name: Some("try again".into()),
            },
        ));
        // After the fork: the original keeps going...
        chat.apply(&post(4, "original continues"));
        // ...and the fork has its own new activity.
        chat.apply(&ev_on(
            10,
            11,
            Some(10),
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "fork continues".into(),
                    input: serde_json::Value::Null,
                    expects_reply: true,
                },
            }),
        ));

        let fork_rows = chat.rows(Some(EventId::new(10)));
        assert!(
            fork_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("shared question"))
        );
        assert!(
            fork_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("shared answer"))
        );
        assert!(
            fork_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("fork continues"))
        );
        assert!(
            !fork_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("original continues")),
            "a fork owes nothing of what the original does afterward"
        );

        let original_rows = chat.rows(Some(EventId::new(1)));
        assert!(
            original_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("original continues"))
        );
        assert!(
            !original_rows
                .iter()
                .any(|(_, t, _, _)| t.contains("fork continues")),
            "the original does not see the fork's own history"
        );
    }
}
