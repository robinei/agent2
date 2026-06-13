//! The LLM client trait (8_HARNESS Step 5). M0 ships the scripted
//! implementation only; the real HTTP client (M1) implements the same
//! trait, so the session loop never knows the difference.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::machine::LlmRequest;
use crate::types::Message;

/// A streamed piece of the assistant turn, forwarded to UIs live.
pub enum LlmChunk {
    Text(String),
    Thinking(String),
}

/// One blocking completion per call; runs on a worker thread owned by
/// the session loop. Chunks go through `chunk` as they arrive; the
/// returned `Message` is the complete assistant turn.
///
/// `complete` takes `&self` so the session loop can share one client
/// (`Arc<dyn LlmClient>`) and run several completions concurrently —
/// subagents think in parallel, bounded by the loop's semaphore. Any
/// per-call mutable state lives behind the client's own interior
/// mutability (`Send + Sync`).
pub trait LlmClient: Send + Sync {
    fn complete(
        &self,
        request: &LlmRequest,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<Message, String>;
}

/// Scripted client: pops canned assistant turns in order. Each turn's
/// text is also streamed as a single chunk so the chunk path is
/// exercised end-to-end without a network. The queue is behind a `Mutex`
/// so the shared `&self` client stays `Sync` under concurrent pops.
pub struct ScriptedLlm {
    responses: Mutex<VecDeque<Message>>,
}

impl ScriptedLlm {
    pub fn new(responses: impl IntoIterator<Item = Message>) -> Self {
        ScriptedLlm {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}

/// A scripted assistant turn calling `run_program` with `source`.
pub fn scripted_program(call_id: &str, source: &str) -> Message {
    Message::Assistant {
        text: String::new(),
        thinking: None,
        tool_calls: vec![crate::types::ToolCall {
            id: call_id.into(),
            name: crate::machine::TOOL_RUN_PROGRAM.into(),
            arguments: serde_json::json!({ "source": source }),
        }],
    }
}

/// A scripted assistant turn calling `resume` with `value` — the
/// restart offered while a program is suspended on a condition.
pub fn scripted_resume(call_id: &str, value: serde_json::Value) -> Message {
    Message::Assistant {
        text: String::new(),
        thinking: None,
        tool_calls: vec![crate::types::ToolCall {
            id: call_id.into(),
            name: crate::machine::TOOL_RESUME.into(),
            arguments: serde_json::json!({ "value": value }),
        }],
    }
}

/// A scripted plain-text assistant turn (completes the frame).
pub fn scripted_text(text: &str) -> Message {
    Message::Assistant {
        text: text.into(),
        thinking: None,
        tool_calls: Vec::new(),
    }
}

impl LlmClient for ScriptedLlm {
    fn complete(
        &self,
        _request: &LlmRequest,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<Message, String> {
        let message = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or("scripted LLM ran out of responses")?;
        if let Message::Assistant { text, thinking, .. } = &message {
            if let Some(t) = thinking {
                chunk(LlmChunk::Thinking(t.clone()));
            }
            if !text.is_empty() {
                chunk(LlmChunk::Text(text.clone()));
            }
        }
        Ok(message)
    }
}
