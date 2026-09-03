//! Chat transcript state (9_TUI Step 4 · 11_INTROSPECT Step 3), driven
//! **exclusively** by `SessionEvent`s — the serializable boundary a
//! remote client would consume. Enforced structurally, not by
//! discipline: `ChatState`'s fields are private to this module and its
//! only mutator is `apply(&SessionEvent)`, so nothing privileged (VMs,
//! tree, session) can leak into what this pane shows. Do not add imports
//! from `crate::host` beyond the protocol types, and none from `interp`.
//!
//! A `run_program` execution renders as one **block** (decision 2): a
//! `run_program: <status>` header (status tracked live from
//! `ProgramStatus`) with the program's inner `Invoke`s listed beneath as
//! `⚙` lines; a `resume` folds into the same block. The completion/
//! condition report body is *not* inlined — it lives in the right
//! console/result pane. The transcript is **per-agent** (decision 6):
//! `rows(agent)` renders just that agent's slice, including its own
//! clean-room `System` prompt.

use std::collections::HashMap;

use crate::host::{AgentId, ProgramStatus, SessionEvent};
use crate::types::{Call, EventId, EventPayload, Message, Outcome};

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
        agent: AgentId,
        kind: ChatKind,
        text: String,
        /// The program this row belongs to (for click hit-testing): the
        /// `run_program` event id for block rows, the `System` event id
        /// for the system header, else `None`.
        program: Option<EventId>,
    },
    /// A `run_program` block header, keyed by the program's event id.
    Header {
        agent: AgentId,
        program: EventId,
        /// Attachment names in definition order.
        attachments: Vec<String>,
    },
}

#[derive(Default)]
pub struct ChatState {
    entries: Vec<Entry>,
    /// Accumulating streamed text per agent, shown until the logged
    /// assistant message replaces it.
    streaming: Vec<(AgentId, String)>,
    /// The first agent seen — the default transcript when none is selected.
    main_agent: Option<AgentId>,
    /// Transcript row index of each logged `Call`, so its `Result` can
    /// complete the row in place rather than pushing a second line.
    call_rows: HashMap<EventId, usize>,
    /// The open `run_program` block per agent: its inner calls and a
    /// folding `resume` attach here.
    current_program: HashMap<AgentId, EventId>,
    /// Live status per program block, titling its header.
    program_status: HashMap<EventId, ProgramStatus>,
    /// Attachment content per program: program_id → (name → content).
    pub attachment_content: HashMap<EventId, HashMap<String, String>>,
}

impl ChatState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Chunk {
                agent,
                thinking,
                text,
            } => {
                if *thinking {
                    return; // thinking stays live-only and unrendered for now
                }
                match self.streaming.iter_mut().find(|(f, _)| f == agent) {
                    Some((_, buf)) => buf.push_str(text),
                    None => self.streaming.push((*agent, text.clone())),
                }
            }
            SessionEvent::Error { agent, message } => {
                if let Some(f) = agent.or(self.main_agent) {
                    self.entries.push(Entry::Line {
                        agent: f,
                        kind: ChatKind::Error,
                        text: format!("error: {message}"),
                        program: None,
                    });
                }
            }
            SessionEvent::Event { agent, event } => {
                self.apply_payload(*agent, event.id, &event.payload);
            }
            // Live program-block status titles the matching header.
            SessionEvent::ProgramStatus {
                program, status, ..
            } => {
                self.program_status.insert(*program, *status);
            }
            // Leaf-list data is for the fork/leaf UI, not the chat pane.
            SessionEvent::Leaves(_) => {}
        }
    }

    fn apply_payload(&mut self, agent: AgentId, id: EventId, payload: &EventPayload) {
        match payload {
            EventPayload::Agent { system, .. } => {
                // The system prompt is a snapshot on the root, not a
                // message: render it as this agent's leading block.
                self.main_agent.get_or_insert(agent);
                self.entries.push(Entry::Line {
                    agent,
                    kind: ChatKind::System,
                    text: system.clone(),
                    program: Some(id),
                });
            }
            EventPayload::FrameResult { result } => {
                if Some(agent) != self.main_agent {
                    self.entries.push(Entry::Line {
                        agent,
                        kind: ChatKind::Marker,
                        text: format!("subagent finished: {result}"),
                        program: None,
                    });
                }
            }
            EventPayload::Message(Message::Post { from, origin }) => {
                self.entries.push(Entry::Line {
                    agent,
                    kind: ChatKind::User,
                    text: crate::report::render_post(*from, origin),
                    program: None,
                });
            }
            EventPayload::Message(Message::Turn {
                text, tool_calls, ..
            }) => {
                self.streaming.retain(|(f, _)| *f != agent);
                if !text.is_empty() {
                    self.entries.push(Entry::Line {
                        agent,
                        kind: ChatKind::Assistant,
                        text: text.clone(),
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
                        agent,
                        program: id,
                        attachments: attachment_names,
                    });
                    self.current_program.insert(agent, id);
                    if !attachments.is_empty() {
                        self.attachment_content.insert(id, attachments);
                    }
                }
            }
            // The report body lives in the right console/result pane, not
            // the transcript (decision 2).
            EventPayload::Message(Message::Tool { .. }) => {}
            // A call is logged at dispatch, so its row appears the moment
            // it is issued; the `Result` completes the same row in place.
            EventPayload::Call(call) => {
                if let Some(&program) = self.current_program.get(&agent) {
                    let name = match call {
                        Call::Invoke { name, .. } => name.clone(),
                        Call::Send { expects_reply, .. } => {
                            if *expects_reply { "ask" } else { "tell" }.to_owned()
                        }
                        Call::Spawn { .. } => "spawn".to_owned(),
                    };
                    self.call_rows.insert(id, self.entries.len());
                    self.entries.push(Entry::Line {
                        agent,
                        kind: ChatKind::ToolCall,
                        text: format!("⚙ {name} → …"),
                        program: Some(program),
                    });
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
            EventPayload::ProgramResult { .. } | EventPayload::Console { .. } => {}
            // A rename is a record: it changes the navigator, never the
            // transcript, and never wakes the branch.
            EventPayload::Rename { .. } => {}
        }
    }

    /// Transcript rows for `agent` (or the main agent when `None`): one
    /// `(kind, line, detail)` per visual line. `detail` carries click-hit
    /// metadata: which program, attachment, or invoke a row targets.
    /// Multi-line items split; the system prompt collapses to a single
    /// header row.
    pub fn rows(&self, agent: Option<AgentId>) -> Vec<(ChatKind, String, RowDetail)> {
        let Some(target) = agent.or(self.main_agent) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut invoke_index: HashMap<EventId, usize> = HashMap::new();
        for entry in &self.entries {
            match entry {
                Entry::Header {
                    agent,
                    program,
                    attachments,
                } if *agent == target => {
                    let status = self
                        .program_status
                        .get(program)
                        .map(|s| status_label(*s))
                        .unwrap_or("running");
                    out.push((
                        ChatKind::ToolCall,
                        format!("run_program: {status}"),
                        RowDetail::Program(*program),
                    ));
                    for name in attachments {
                        out.push((
                            ChatKind::Attachment,
                            format!("⬡ attachment: {name}"),
                            RowDetail::Attachment(*program, name.clone()),
                        ));
                    }
                }
                Entry::Line {
                    agent,
                    kind,
                    text,
                    program,
                } if *agent == target => {
                    if *kind == ChatKind::System {
                        out.push((ChatKind::System, "system".into(), RowDetail::None));
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
                    push_wrapped(&mut out, *kind, text, detail);
                }
                _ => {}
            }
        }
        for (f, buf) in &self.streaming {
            if *f != target {
                continue;
            }
            for line in buf.lines() {
                out.push((ChatKind::Streaming, line.to_owned(), RowDetail::None));
            }
        }
        out
    }
}

/// Push an item's visual lines, labelling user/assistant prose and
/// indenting continuation lines under the label.
fn push_wrapped(
    out: &mut Vec<(ChatKind, String, RowDetail)>,
    kind: ChatKind,
    text: &str,
    detail: RowDetail,
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
        out.push((kind, format!("{head}{line}"), detail.clone()));
    }
    if !any {
        out.push((kind, label.to_owned(), detail));
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
    use crate::types::{Author, Event, Origin, ToolCall};
    use jiff::Timestamp;

    fn ev(id: u64, payload: EventPayload) -> SessionEvent {
        SessionEvent::Event {
            agent: EventId::new(1),
            event: Event {
                id: EventId::new(id),
                parent_id: None,
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
        chat.apply(&ev(
            2,
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "hi".into(),
                    input: serde_json::Value::Null,
                    expects_reply: true,
                },
            }),
        ));
        chat.apply(&SessionEvent::Chunk {
            agent: EventId::new(1),
            thinking: false,
            text: "thinki".into(),
        });
        let rows = chat.rows(None);
        assert!(
            rows.iter()
                .any(|(k, t, _)| *k == ChatKind::User && t.contains("hi"))
        );
        assert!(
            rows.iter()
                .any(|(k, t, _)| *k == ChatKind::Streaming && t.contains("thinki"))
        );

        // The logged assistant message replaces the stream.
        chat.apply(&run_program(3));
        let rows = chat.rows(None);
        assert!(!rows.iter().any(|(k, _, _)| *k == ChatKind::Streaming));
        // A run_program renders as a status-titled block header.
        assert!(rows.iter().any(|(k, t, p)| *k == ChatKind::ToolCall
            && t == "run_program: running"
            && *p == RowDetail::Program(EventId::new(3))));

        // ProgramResult/Rename never reach the transcript.
        let before = chat.rows(None).len();
        chat.apply(&ev(
            5,
            EventPayload::ProgramResult {
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
        let glyphs: Vec<&(ChatKind, String, RowDetail)> = rows
            .iter()
            .filter(|(k, t, _)| *k == ChatKind::ToolCall && t.starts_with('⚙'))
            .collect();
        assert_eq!(glyphs.len(), 2, "two inner-call lines");
        // Each ⚙ line carries the program id + invoke index for hit-testing.
        assert!(
            glyphs
                .iter()
                .all(|(_, _, p)| matches!(p, RowDetail::Invoke(_, _)))
        );
        assert!(
            glyphs
                .iter()
                .any(|(_, _, p)| *p == RowDetail::Invoke(EventId::new(2), 0))
        );
        assert!(
            glyphs
                .iter()
                .any(|(_, _, p)| *p == RowDetail::Invoke(EventId::new(2), 1))
        );

        // Header starts at running…
        assert!(
            chat.rows(None)
                .iter()
                .any(|(_, t, _)| t == "run_program: running")
        );
        // …and tracks ProgramStatus to completed.
        chat.apply(&SessionEvent::ProgramStatus {
            agent: EventId::new(1),
            program: EventId::new(2),
            status: ProgramStatus::Completed,
        });
        assert!(
            chat.rows(None)
                .iter()
                .any(|(_, t, _)| t == "run_program: completed")
        );

        // The completion report body is not inlined.
        chat.apply(&ev(
            5,
            EventPayload::Message(Message::Tool {
                name: "run_program".into(),
                call_id: "c1".into(),
                text: "## program completed\nreturned: 1".into(),
            }),
        ));
        assert!(
            !chat
                .rows(None)
                .iter()
                .any(|(_, t, _)| t.contains("program completed"))
        );
    }

    /// `Agent.system` — the snapshot on the branch root — renders as the
    /// leading `system` row of its agent, and selecting another agent
    /// shows that agent's slice (its own system block), not the root's.
    #[test]
    fn system_block_is_leading_and_per_agent() {
        let mut chat = ChatState::new();
        // Root agent #1.
        chat.apply(&ev(
            1,
            EventPayload::Agent {
                name: None,
                charter: "root".into(),
                tools: None,
                system: "ROOT SYSTEM PROMPT".into(),
            },
        ));
        chat.apply(&ev(
            3,
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "root q".into(),
                    input: serde_json::Value::Null,
                    expects_reply: true,
                },
            }),
        ));
        // Subagent agent #4 with its own system prompt.
        let child = EventId::new(4);
        let child_event = |id: u64, payload| SessionEvent::Event {
            agent: child,
            event: Event {
                id: EventId::new(id),
                parent_id: None,
                timestamp: Timestamp::now(),
                payload,
            },
        };
        chat.apply(&child_event(
            4,
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
                .any(|(k, t, _)| *k == ChatKind::User && t.contains("root q"))
        );
        // The child's content is not in the root's slice.
        assert!(!root_rows.iter().any(|(_, t, _)| t.contains("CHILD")));

        // The child's slice leads with its own system header.
        let child_rows = chat.rows(Some(child));
        assert_eq!(child_rows[0].0, ChatKind::System);
        assert_eq!(child_rows[0].2, RowDetail::None);
        assert!(!child_rows.iter().any(|(_, t, _)| t.contains("root q")));
    }
}
