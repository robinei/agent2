//! The LLM client trait (8_HARNESS Step 5). M0 ships the scripted
//! implementation only; the real HTTP client (M1) implements the same
//! trait, so the session loop never knows the difference.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::machine::{LlmRequest, LlmTurn};

/// A streamed piece of the assistant turn, forwarded to UIs live.
pub enum LlmChunk {
    Text(String),
    Thinking(String),
}

/// One blocking completion per call; runs on a worker thread owned by
/// the session loop. Chunks go through `chunk` as they arrive; the
/// returned `LlmTurn` is the complete assistant turn. A client speaks for
/// a branch and never stamps *who acted* — the harness does that when it
/// logs the turn.
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
    ) -> Result<LlmTurn, String>;
}

/// Scripted client: pops canned assistant turns in order. Each turn's
/// text is also streamed as a single chunk so the chunk path is
/// exercised end-to-end without a network. The queue is behind a `Mutex`
/// so the shared `&self` client stays `Sync` under concurrent pops.
pub struct ScriptedLlm {
    responses: Mutex<VecDeque<LlmTurn>>,
}

impl ScriptedLlm {
    pub fn new(responses: impl IntoIterator<Item = LlmTurn>) -> Self {
        ScriptedLlm {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}

/// A scripted assistant turn calling `run_program` with `source`.
pub fn scripted_program(call_id: &str, source: &str) -> LlmTurn {
    LlmTurn {
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
#[allow(dead_code)]
pub fn scripted_resume(call_id: &str, value: serde_json::Value) -> LlmTurn {
    LlmTurn {
        text: String::new(),
        thinking: None,
        tool_calls: vec![crate::types::ToolCall {
            id: call_id.into(),
            name: crate::machine::TOOL_RESUME.into(),
            arguments: serde_json::json!({ "value": value }),
        }],
    }
}

/// A scripted plain-text assistant turn (completes the agent).
pub fn scripted_text(text: &str) -> LlmTurn {
    LlmTurn {
        text: text.into(),
        thinking: None,
        tool_calls: Vec::new(),
    }
}

/// A scripted client that answers by **which agent asked**, not by
/// arrival order: each rule is a **charter** and its own queue of turns,
/// popped in order.
///
/// A charter is matched as the *tail* of the system prompt, which is
/// exactly where `assemble_system` puts it — behind the card. Matching
/// anywhere in the prompt would collide with the card's own prose (which
/// says "worker" a few times), so the tail is both simpler and correct.
///
/// From B1 on, several branches think at once as a matter of course. A
/// single queue makes the *test* racy where the system is not: two
/// workers prompted in the same step pop in whatever order their threads
/// win. Keying on the branch removes that without weakening anything —
/// each branch still gets its scripted turns in order.
#[cfg(test)]
pub struct RoutedLlm {
    rules: Vec<(String, Mutex<VecDeque<LlmTurn>>)>,
}

#[cfg(test)]
impl RoutedLlm {
    /// Rules are tried in order, first match wins, so list the most
    /// specific charter first when one is a suffix of another.
    pub fn new(rules: impl IntoIterator<Item = (&'static str, Vec<LlmTurn>)>) -> Self {
        RoutedLlm {
            rules: rules
                .into_iter()
                .map(|(needle, turns)| (needle.to_owned(), Mutex::new(turns.into())))
                .collect(),
        }
    }
}

#[cfg(test)]
impl LlmClient for RoutedLlm {
    fn complete(
        &self,
        request: &LlmRequest,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<LlmTurn, String> {
        for (charter, queue) in &self.rules {
            if !request.system.ends_with(charter.as_str()) {
                continue;
            }
            let Some(turn) = queue.lock().unwrap().pop_front() else {
                return Err(format!("scripted rule `{charter}` ran out of turns"));
            };
            if let Some(t) = &turn.thinking {
                chunk(LlmChunk::Thinking(t.clone()));
            }
            if !turn.text.is_empty() {
                chunk(LlmChunk::Text(turn.text.clone()));
            }
            return Ok(turn);
        }
        let tail = request.system.len().saturating_sub(80);
        Err(format!(
            "no scripted rule matches this branch's charter: …{}",
            &request.system[tail..]
        ))
    }
}

impl LlmClient for ScriptedLlm {
    fn complete(
        &self,
        _request: &LlmRequest,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<LlmTurn, String> {
        let message = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or("scripted LLM ran out of responses")?;
        if let Some(t) = &message.thinking {
            chunk(LlmChunk::Thinking(t.clone()));
        }
        if !message.text.is_empty() {
            chunk(LlmChunk::Text(message.text.clone()));
        }
        Ok(message)
    }
}
