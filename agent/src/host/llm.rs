//! The LLM client trait (8_HARNESS Step 5). M0 ships the scripted
//! implementation only; the real HTTP client (M1) implements the same
//! trait, so the session loop never knows the difference.
//!
//! Under code mode (23_ONE_AGENT) a request is a rendered [`Document`]
//! (`document::render`, over `&Tree`/`Spine`) rather than a tool-spec'd
//! `LlmRequest` — there is no `tools` array on the wire, because the
//! model's whole response *is* the program, not a pick from a function
//! menu. `LlmTurn` shrinks to match: `source` is the bare program text
//! (`Message::Turn.source`, the same field name, because a scripted or
//! live completion and a logged turn are the same shape all the way
//! through), plus `thinking` and `truncated` — the one bit
//! `host/deepseek.rs` must set before this turn ever reaches a compiler,
//! per `types.rs`'s `Cause::Truncated`: **never compile a truncated
//! completion**.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::document::Document;
use crate::machine::LlmTurn;

/// A cancellation token, one per in-flight completion.
///
/// `Interrupt` is the one override on rule B: a generation the user no
/// longer wants is cancelled, and **from the API's view it did not
/// happen** — nothing is logged for it. Correctness does not rest on the
/// client noticing: the session drops a cancelled generation's response
/// by epoch whatever the client returns. The token is what stops the
/// wasted work, and it is why the trait carries one rather than the
/// session racing a thread it cannot reach.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the in-flight completion to stop. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

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
        request: &Document,
        cancel: &Cancel,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<LlmTurn, String>;
}

/// Scripted client: pops canned assistant turns in order. Each turn's
/// source is also streamed as a single chunk so the chunk path is
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

/// A scripted assistant turn: the bare program `source`, exactly what a
/// real completion's whole response would be — no call id, no tool name,
/// because there is no tool-call wrapper left to name (23_ONE_AGENT's
/// substitution: the model's whole response *is* `Turn { source }`).
pub fn scripted_program(source: &str) -> LlmTurn {
    LlmTurn {
        usage: None,
        source: source.into(),
        thinking: None,
        truncated: false,
        reply: None,
    }
}

/// A scripted `Transport::RunProgram` completion: prose beside a
/// program (`source` non-empty — the "run it, then keep going" shape),
/// or prose with no call at all (`source` empty — the "that was the
/// final answer" shape). `Transport::Program` never sets `LlmTurn.reply`
/// (its own doc comment), so `scripted_program` above, unchanged, is
/// still every existing call site's helper; this is the one
/// `Transport::RunProgram`'s two completion shapes need and
/// `scripted_program` alone cannot express.
#[cfg(test)]
pub fn scripted_reply(reply: &str, source: &str) -> LlmTurn {
    LlmTurn {
        reply: Some(reply.to_owned()),
        ..scripted_program(source)
    }
}

/// A scripted **handler** turn: a program whose only job is to decide a
/// suspended raise/trap, `return resume(value);` — the restart a
/// condition report offers, spelled as the program that makes it
/// (DESIGN.md: "the handler is not a turn picking a restart off a menu —
/// it is a program the LLM writes... whose return value **is** the
/// restart").
#[allow(dead_code)]
pub fn scripted_resume(value: serde_json::Value) -> LlmTurn {
    scripted_program(&format!("return resume({value});"))
}

/// A scripted handler turn that discharges an open `ask()` by the
/// question's own id, `answer(question, label, value)`, and leaves the
/// raising program alone — the restart that binds explicitly rather than
/// supplying a `resume` value.
#[cfg(test)]
pub fn scripted_answer(
    question: crate::types::EventId,
    label: &str,
    value: serde_json::Value,
) -> LlmTurn {
    scripted_program(&format!(
        "answer({}, {}, {value});",
        question.as_u64(),
        serde_json::json!(label)
    ))
}

/// A scripted turn that only speaks — `tell("user", text)` — and ends
/// the task. Under code mode there is no bare-text assistant reply any
/// more (`card.rs`: "no prose, no code fence... the whole response is
/// parsed as JavaScript"); the closest equivalent to the pre-code-mode
/// "plain reply ends the turn" is a two-line program that tells the user
/// something and then calls `done()` — completing on its own is no
/// longer enough to rest a branch (`machine.rs`'s `finish_program`), so
/// every call site that reached for this helper for exactly that "say
/// it and stop" shape needs the explicit `done()` to still get it,
/// rather than a needless extra scripted round trip (or, for a test
/// double that answers every re-prompt the same way regardless of what
/// is open, an outright infinite loop — `AutoAnswerLlm`'s own doc).
#[allow(dead_code)] // used only from test modules, which the non-test
// build does not compile; not dead.
pub fn scripted_text(text: &str) -> LlmTurn {
    scripted_program(&format!(
        "tell(\"user\", {}); done();",
        serde_json::json!(text)
    ))
}

/// A scripted client that answers by **which agent asked**, not by
/// arrival order: each rule is a **charter** and its own queue of turns,
/// popped in order.
///
/// A charter is matched as the *tail* of the system prompt (`Document`'s
/// first message, always `System` — `document::render`'s own invariant),
/// which is exactly where an agent's snapshotted `system` puts it —
/// behind the card. Matching anywhere in the prompt would collide with
/// the card's own prose (which says "worker" a few times), so the tail
/// is both simpler and correct.
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
        request: &Document,
        cancel: &Cancel,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<LlmTurn, String> {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let system = request
            .messages
            .first()
            .map(|m| m.content.as_str())
            .unwrap_or_default();
        for (charter, queue) in &self.rules {
            if !system.ends_with(charter.as_str()) {
                continue;
            }
            let Some(turn) = queue.lock().unwrap().pop_front() else {
                return Err(format!("scripted rule `{charter}` ran out of turns"));
            };
            if let Some(t) = &turn.thinking {
                chunk(LlmChunk::Thinking(t.clone()));
            }
            if !turn.source.is_empty() {
                chunk(LlmChunk::Text(turn.source.clone()));
            }
            return Ok(turn);
        }
        let tail = system.len().saturating_sub(80);
        Err(format!(
            "no scripted rule matches this branch's charter: …{}",
            &system[tail..]
        ))
    }
}

impl LlmClient for ScriptedLlm {
    fn complete(
        &self,
        _request: &Document,
        cancel: &Cancel,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<LlmTurn, String> {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let message = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or("scripted LLM ran out of responses")?;
        if let Some(t) = &message.thinking {
            chunk(LlmChunk::Thinking(t.clone()));
        }
        if !message.source.is_empty() {
            chunk(LlmChunk::Text(message.source.clone()));
        }
        Ok(message)
    }
}

/// A scripted client whose first `hold` completions **block until they
/// are cancelled** — the one thing a test needs that no pure script can
/// give it: a generation that is genuinely still in flight when
/// `Interrupt` arrives. Later completions are served from `inner`.
///
/// It also counts the cancellations it observed, so a test can assert
/// the token reached the worker rather than only that the session
/// dropped the answer.
#[cfg(test)]
pub struct HoldingLlm {
    held: Mutex<usize>,
    observed: std::sync::atomic::AtomicUsize,
    inner: ScriptedLlm,
}

#[cfg(test)]
impl HoldingLlm {
    pub fn new(hold: usize, responses: impl IntoIterator<Item = LlmTurn>) -> Self {
        HoldingLlm {
            held: Mutex::new(hold),
            observed: std::sync::atomic::AtomicUsize::new(0),
            inner: ScriptedLlm::new(responses),
        }
    }

    /// How many completions saw their token cancelled.
    pub fn cancelled(&self) -> usize {
        self.observed.load(Ordering::SeqCst)
    }
}

/// A shared client is the normal case (several branches think at once),
/// so a test that keeps a handle on one hands the session an `Arc`.
impl<T: LlmClient + ?Sized> LlmClient for Arc<T> {
    fn complete(
        &self,
        request: &Document,
        cancel: &Cancel,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<LlmTurn, String> {
        (**self).complete(request, cancel, chunk)
    }
}

#[cfg(test)]
impl LlmClient for HoldingLlm {
    fn complete(
        &self,
        request: &Document,
        cancel: &Cancel,
        chunk: &mut dyn FnMut(LlmChunk),
    ) -> Result<LlmTurn, String> {
        let hold = {
            let mut left = self.held.lock().unwrap();
            let hold = *left > 0;
            *left = left.saturating_sub(1);
            hold
        };
        if hold {
            while !cancel.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            self.observed.fetch_add(1, Ordering::SeqCst);
            return Err("cancelled".into());
        }
        self.inner.complete(request, cancel, chunk)
    }
}
