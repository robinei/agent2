//! Chat transcript state (9_TUI Step 4), driven **exclusively** by
//! `SessionEvent`s — the serializable boundary a remote client would
//! consume. Enforced structurally, not by discipline: `ChatState`'s
//! fields are private to this module and its only mutator is
//! `apply(&SessionEvent)`, so nothing privileged (VMs, tree, session)
//! can leak into what this pane shows. Do not add imports from
//! `crate::host` beyond the protocol types, and none from `interp`.

use crate::host::{FrameId, SessionEvent};
use crate::types::{EventPayload, Message};

/// What a transcript row is, for styling by the renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChatKind {
    User,
    Assistant,
    /// In-flight streamed text (replaced by the logged message).
    Streaming,
    /// A `run_program`/`resume` tool call header.
    ToolCall,
    /// A tool result body (completion/condition report).
    ToolResult,
    /// Frame lifecycle markers.
    Marker,
    Error,
}

#[derive(Debug)]
pub struct ChatItem {
    pub frame: Option<FrameId>,
    pub kind: ChatKind,
    pub text: String,
}

#[derive(Default)]
pub struct ChatState {
    items: Vec<ChatItem>,
    /// Accumulating streamed text per frame, shown until the logged
    /// assistant message replaces it.
    streaming: Vec<(FrameId, String)>,
    /// The first frame seen is "the" conversation; others get a
    /// `[frame N]` prefix.
    main_frame: Option<FrameId>,
}

impl ChatState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Chunk {
                frame,
                thinking,
                text,
            } => {
                if *thinking {
                    return; // thinking stays live-only and unrendered for now
                }
                match self.streaming.iter_mut().find(|(f, _)| f == frame) {
                    Some((_, buf)) => buf.push_str(text),
                    None => self.streaming.push((*frame, text.clone())),
                }
            }
            SessionEvent::Error { frame, message } => {
                self.push(*frame, ChatKind::Error, format!("error: {message}"));
            }
            SessionEvent::Event { frame, event } => {
                self.apply_payload(*frame, &event.payload);
            }
        }
    }

    fn apply_payload(&mut self, frame: FrameId, payload: &EventPayload) {
        match payload {
            EventPayload::FrameStart { prompt, .. } => {
                if self.main_frame.is_none() {
                    self.main_frame = Some(frame);
                    self.push(Some(frame), ChatKind::Marker, format!("session: {prompt}"));
                } else {
                    self.push(
                        Some(frame),
                        ChatKind::Marker,
                        format!("subagent started: {prompt}"),
                    );
                }
            }
            EventPayload::FrameResult { result } => {
                if Some(frame) != self.main_frame {
                    self.push(
                        Some(frame),
                        ChatKind::Marker,
                        format!("subagent finished: {result}"),
                    );
                }
            }
            EventPayload::Message(Message::User { text }) => {
                self.push(Some(frame), ChatKind::User, text.clone());
            }
            EventPayload::Message(Message::Assistant {
                text, tool_calls, ..
            }) => {
                self.streaming.retain(|(f, _)| *f != frame);
                if !text.is_empty() {
                    self.push(Some(frame), ChatKind::Assistant, text.clone());
                }
                for call in tool_calls {
                    self.push(Some(frame), ChatKind::ToolCall, format!("⚙ {}", call.name));
                }
            }
            EventPayload::Message(Message::Tool { name, text, .. }) => {
                self.push(
                    Some(frame),
                    ChatKind::ToolResult,
                    format!("{name}:\n{text}"),
                );
            }
            // System prompts render via the FrameStart marker; execution
            // events (Invoke/ProgramResult/Label) are debug-pane data,
            // not transcript (they never render to chat — Step 1 rules).
            EventPayload::Message(Message::System { .. })
            | EventPayload::Invoke { .. }
            | EventPayload::ProgramResult { .. }
            | EventPayload::Label(_)
            | EventPayload::TextChunk(_)
            | EventPayload::ThinkingChunk(_) => {}
        }
    }

    fn push(&mut self, frame: Option<FrameId>, kind: ChatKind, text: String) {
        self.items.push(ChatItem { frame, kind, text });
    }

    /// Transcript rows for rendering: one `(kind, line)` per visual
    /// line, multi-line items split, off-main frames prefixed.
    pub fn rows(&self) -> Vec<(ChatKind, String)> {
        let mut out = Vec::new();
        for item in &self.items {
            let prefix = match (item.frame, self.main_frame) {
                (Some(f), Some(main)) if f != main => format!("[frame {}] ", f.as_u64()),
                _ => String::new(),
            };
            let label = match item.kind {
                ChatKind::User => "you ❯ ",
                ChatKind::Assistant => "agent ❯ ",
                _ => "",
            };
            for (i, line) in item.text.lines().enumerate() {
                let head = if i == 0 {
                    format!("{prefix}{label}")
                } else {
                    " ".repeat(prefix.chars().count() + label.chars().count())
                };
                out.push((item.kind, format!("{head}{line}")));
            }
            if item.text.is_empty() {
                out.push((item.kind, format!("{prefix}{label}")));
            }
        }
        for (frame, buf) in &self.streaming {
            let prefix = match self.main_frame {
                Some(main) if *frame != main => format!("[frame {}] ", frame.as_u64()),
                _ => String::new(),
            };
            for line in buf.lines() {
                out.push((ChatKind::Streaming, format!("{prefix}{line}")));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Event, EventId, ToolCall};
    use jiff::Timestamp;

    fn ev(id: u64, payload: EventPayload) -> SessionEvent {
        SessionEvent::Event {
            frame: EventId::new(1),
            event: Event {
                id: EventId::new(id),
                parent_id: None,
                timestamp: Timestamp::now(),
                payload,
            },
        }
    }

    #[test]
    fn transcript_builds_from_session_events_only() {
        let mut chat = ChatState::new();
        chat.apply(&ev(
            1,
            EventPayload::FrameStart {
                prompt: "be helpful".into(),
                input: serde_json::Value::Null,
            },
        ));
        chat.apply(&ev(
            2,
            EventPayload::Message(Message::User { text: "hi".into() }),
        ));
        chat.apply(&SessionEvent::Chunk {
            frame: EventId::new(1),
            thinking: false,
            text: "thinki".into(),
        });
        let rows = chat.rows();
        assert!(
            rows.iter()
                .any(|(k, t)| *k == ChatKind::User && t.contains("hi"))
        );
        assert!(
            rows.iter()
                .any(|(k, t)| *k == ChatKind::Streaming && t.contains("thinki"))
        );

        // The logged assistant message replaces the stream.
        chat.apply(&ev(
            3,
            EventPayload::Message(Message::Assistant {
                text: "thinking done".into(),
                thinking: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "run_program".into(),
                    arguments: serde_json::json!({}),
                }],
            }),
        ));
        let rows = chat.rows();
        assert!(!rows.iter().any(|(k, _)| *k == ChatKind::Streaming));
        assert!(
            rows.iter()
                .any(|(k, t)| *k == ChatKind::ToolCall && t.contains("run_program"))
        );

        // Execution events never reach the transcript.
        chat.apply(&ev(
            4,
            EventPayload::Invoke {
                name: "fetch".into(),
                args: serde_json::json!([]),
                result: serde_json::json!("x"),
            },
        ));
        assert_eq!(chat.rows().len(), rows.len());
    }
}
