//! The LLM client trait (8_HARNESS Step 5). M0 ships the scripted
//! implementation only; the real HTTP client (M1) implements the same
//! trait, so the session loop never knows the difference.
//!
//! Under code mode (23_ONE_AGENT) a request is a rendered [`Document`]
//! (`document::render`, over `&Tree`/`Spine`) rather than a tool-spec'd
//! `LlmRequest` — there is no `tools` array on the wire, because the
//! model's whole response *is* the program, not a pick from a function
//! menu. `LlmTurn` shrinks to match: `source` is the reply's markdown,
//! plus `thinking` and `truncated`. The log keeps that reply as the
//! parts it decomposes into (28) — a `Reply`, its `Part`s and a
//! `ReplyEnd` carrying `how` — so nothing here has to say twice what
//! the model wrote.

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

/// A scripted assistant turn from JavaScript: a reply whose whole
/// content is one cell.
///
/// **A reply is markdown now, and a fixture may write either.** Given
/// bare JavaScript this wraps it in a `js` fence, which is what the
/// model would have written and what the notebook driver reads. Given
/// markdown — anything already containing a fence — it is passed
/// through untouched, so a fixture that means to test prose, several
/// cells, or a cell-less reply says so directly.
///
/// The wrapping is here rather than in a hundred fixtures because the
/// fixtures are about what the *program* does; which syntax carried it
/// is this function's business.
pub fn scripted_program(source: &str) -> LlmTurn {
    let source = if source.contains("```") || source.is_empty() {
        source.to_owned()
    } else {
        format!("```js\n{source}\n```\n")
    };
    LlmTurn {
        usage: None,
        source,
        thinking: None,
        truncated: false,
        reply: None,
    }
}

/// A scripted reply **verbatim** — markdown, prose, fences and all.
/// For the fixtures that are about the reply's shape rather than its
/// program: a reply with no cell, one with two, one whose prose matters.
#[cfg_attr(not(test), allow(dead_code))]
pub fn scripted_markdown(markdown: &str) -> LlmTurn {
    LlmTurn {
        usage: None,
        source: markdown.to_owned(),
        thinking: None,
        truncated: false,
        reply: None,
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
    scripted_program(&format!("history.append(resume({value}));"))
}

/// A scripted handler turn that discharges an open `ask()` by the
/// question's own id, `answer(question, value)`, and leaves the
/// raising program alone — the restart that binds explicitly rather than
/// supplying a `resume` value.
#[cfg(test)]
pub fn scripted_answer(
    question: crate::types::EventId,
    label: &str,
    value: serde_json::Value,
) -> LlmTurn {
    scripted_program(&format!(
        "answer({}, {label:?}, {value});",
        question.as_u64()
    ))
}

/// A scripted turn that only speaks — `tell("user", text)` — and ends
/// the task. Under code mode there is no bare-text assistant reply any
/// more (`card.rs`: "no prose, no code fence... the whole response is
/// parsed as JavaScript"); the closest equivalent to the pre-code-mode
/// "plain reply ends the turn" is a two-line program that tells the user
/// something and then calls `finish(text)` — completing on its own is no
/// longer enough to rest a branch (`machine.rs`'s `finish_program`), so
/// every call site that reached for this helper for exactly that "say
/// it and stop" shape needs the explicit `finish(text)` to still get it,
/// rather than a needless extra scripted round trip (or, for a test
/// double that answers every re-prompt the same way regardless of what
/// is open, an outright infinite loop — `AutoAnswerLlm`'s own doc).
#[allow(dead_code)] // used only from test modules, which the non-test
// build does not compile; not dead.
pub fn scripted_text(text: &str) -> LlmTurn {
    // `finish(text)` is the pair this used to write out longhand: the
    // answer goes to the person and the task ends, in one call.
    scripted_program(&format!("finish({});", serde_json::json!(text)))
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
    route: Route,
}

/// Which part of the request a rule's needle is matched against.
#[cfg(test)]
enum Route {
    /// The tail of the system prompt — an agent's charter. The right key
    /// when the branches that think at once belong to *different*
    /// agents.
    CharterTail,
    /// Anywhere in the conversation proper (everything after the
    /// preamble). The right key when two branches of the **same** agent
    /// think at once: they share a charter, so nothing in the system
    /// prompt can tell them apart, and only what was said on each branch
    /// can. The preamble is excluded for the same reason `CharterTail`
    /// matches only the tail — the card's own prose would otherwise
    /// match needles meant for the conversation.
    Conversation,
}

#[cfg(test)]
impl RoutedLlm {
    /// Rules are tried in order, first match wins, so list the most
    /// specific charter first when one is a suffix of another.
    pub fn new(rules: impl IntoIterator<Item = (&'static str, Vec<LlmTurn>)>) -> Self {
        Self::with_route(Route::CharterTail, rules)
    }

    /// [`RoutedLlm`], keyed on what the branch has *said* rather than on
    /// whose charter it is — the only key available when two forks of one
    /// agent think at once.
    pub fn by_conversation(rules: impl IntoIterator<Item = (&'static str, Vec<LlmTurn>)>) -> Self {
        Self::with_route(Route::Conversation, rules)
    }

    fn with_route(
        route: Route,
        rules: impl IntoIterator<Item = (&'static str, Vec<LlmTurn>)>,
    ) -> Self {
        RoutedLlm {
            route,
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
        // Content *and* any tool-call source: under
        // `Transport::RunProgram` a turn's program rides in the call, not
        // in `content`, and a rule keyed on what a branch said must not
        // depend on which container carried it.
        let conversation: String = request
            .conversation()
            .iter()
            .flat_map(|m| std::iter::once(m.content.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        for (charter, queue) in &self.rules {
            let matched = match self.route {
                Route::CharterTail => system.ends_with(charter.as_str()),
                Route::Conversation => conversation.contains(charter.as_str()),
            };
            if !matched {
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
        Err(match self.route {
            Route::CharterTail => {
                let tail = system.len().saturating_sub(80);
                format!(
                    "no scripted rule matches this branch's charter: …{}",
                    &system[tail..]
                )
            }
            Route::Conversation => {
                format!("no scripted rule matches this branch's conversation: {conversation:?}")
            }
        })
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
///
/// **A hold slot is claimed by whichever worker thread reaches
/// `complete` first, which is not necessarily the generation the test
/// meant to hold** — and that is a deadlock, not a flake, because the
/// session only ever cancels the generation it has *superseded*. The
/// last generation's token is never cancelled by anyone, so a worker
/// that holds *it* spins forever; `Session::in_flight` never returns to
/// zero, `quiet()` is therefore permanently false, and the loop's
/// `pump_one` blocks in `rx.recv()` for good.
///
/// That is exactly how `host::tests::interrupt_cancels_generation` hung
/// the whole test binary. `spawn_llm` starts a worker and returns; the
/// loop thread then processes the next queued command and starts a
/// second worker, so two generations' threads race to enter `complete`.
/// On an idle machine the first wins and the test passes — it passed 30
/// runs in a row alone. Under load (20 test threads, or 24 spinners and
/// `--test-threads=1`) the second wins often, and an instrumented run
/// correlated the two outcomes perfectly across 25 attempts: every run
/// whose held request was the pre-interrupt document passed, and every
/// run whose held request was the post-interrupt one hung.
///
/// So the claim is *observable*: [`wait_until_held`] blocks until `n`
/// completions have actually taken a hold slot, which lets a test send
/// the interrupting turn only once the generation it means to interrupt
/// is genuinely parked. Asserting the branch's status is not enough —
/// that only proves the loop spawned the worker, not that the worker
/// ran.
///
/// [`wait_until_held`]: HoldingLlm::wait_until_held
#[cfg(test)]
pub struct HoldingLlm {
    holds: Mutex<Holds>,
    /// Signalled when a completion claims a hold slot.
    claimed: std::sync::Condvar,
    observed: std::sync::atomic::AtomicUsize,
    inner: ScriptedLlm,
}

/// Hold slots, as a pair rather than one counter: `left` is what
/// `complete` decrements, `claimed` is what `wait_until_held` waits on.
/// One counter cannot serve both — a test cannot tell "not claimed yet"
/// from "claimed and released" by watching a number go down.
#[cfg(test)]
struct Holds {
    left: usize,
    claimed: usize,
}

#[cfg(test)]
impl HoldingLlm {
    pub fn new(hold: usize, responses: impl IntoIterator<Item = LlmTurn>) -> Self {
        HoldingLlm {
            holds: Mutex::new(Holds {
                left: hold,
                claimed: 0,
            }),
            claimed: std::sync::Condvar::new(),
            observed: std::sync::atomic::AtomicUsize::new(0),
            inner: ScriptedLlm::new(responses),
        }
    }

    /// Block until `n` completions have claimed a hold slot. Call it
    /// before doing anything that starts a *second* generation: once the
    /// slot is taken, the later worker cannot take it, so which
    /// generation is parked stops depending on thread start order.
    ///
    /// Waits unconditionally rather than with a deadline: the worker is
    /// already spawned when a test gets here, so this returns as soon as
    /// it is scheduled, and a timeout would only trade a diagnosable
    /// hang for a flaky assertion on a loaded machine.
    pub fn wait_until_held(&self, n: usize) {
        let mut holds = self.holds.lock().unwrap();
        while holds.claimed < n {
            holds = self.claimed.wait(holds).unwrap();
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
            let mut holds = self.holds.lock().unwrap();
            let hold = holds.left > 0;
            if hold {
                holds.left -= 1;
                holds.claimed += 1;
                self.claimed.notify_all();
            }
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
