//! The host layer (8_HARNESS Step 5): one main-loop thread owns the
//! tree and the agent step machines and `recv()`s a single
//! `std::sync::mpsc` inbox of one unified message enum. Worker threads
//! exist only for blocking IO — one per in-flight LLM completion,
//! spawn-per-call for tool fan-out — and only ever hold a cloned
//! `Sender`. VM compute runs on the loop thread in fuel slices
//! (`StepInput::Tick`), with a `Continue` message re-enqueued between
//! slices so a hot program never starves other contexts or the UI.
//!
//! The single inbox gives one total arrival order, which *is* the
//! logged resolution order (decision 7) — no select fairness anywhere.
//! UIs talk to the loop only through the serializable
//! `SessionCommand`/`SessionEvent` pair (`protocol.rs`); the debugger
//! TUI additionally borrows VMs/tree directly because it renders on
//! this same thread (9_TUI decision 4).

mod deepseek;
mod demo;
mod dialect;
mod llm;
mod protocol;
mod registry;
mod structural;
mod tools;

pub use deepseek::*;
pub use demo::*;
pub use dialect::*;
pub use llm::*;
pub use protocol::*;
pub use registry::*;
pub use tools::*;

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

use crate::machine::{LlmRequest, OutCall, Runner, SpawnAgent, StepInput, StepOutput, ToolResult};
use crate::types::{Call, EventId, EventPayload, Message, Outcome, Spine, Tree};

/// Instructions per VM slice on the loop thread (9_TUI decision 3).
pub const FUEL_SLICE: u64 = 100_000;

/// Default cap on LLM completions running at once across all contexts
/// (root + subagents). Overridable via `AGENT2_LLM_CONCURRENCY`.
pub const DEFAULT_LLM_CONCURRENCY: usize = 4;

/// How many LLM completions may run concurrently — env override, else
/// the default. Floored at 1 (a parse of 0/garbage falls back).
fn llm_concurrency() -> usize {
    std::env::var("AGENT2_LLM_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(DEFAULT_LLM_CONCURRENCY)
}

/// A counting semaphore (std-only) bounding concurrent LLM completions.
/// LLM worker threads block in `acquire` until a permit frees; the
/// returned `Permit` returns it on drop. A permit is held only for the
/// duration of one `complete()` call — never while an agent is parked
/// awaiting tool/subagent results — so it cannot deadlock a join.
struct Semaphore {
    permits: Mutex<usize>,
    available: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Self {
        Self {
            permits: Mutex::new(n.max(1)),
            available: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> Permit {
        let mut n = self.permits.lock().unwrap();
        while *n == 0 {
            n = self.available.wait(n).unwrap();
        }
        *n -= 1;
        Permit {
            sem: Arc::clone(self),
        }
    }
}

struct Permit {
    sem: Arc<Semaphore>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        *self.sem.permits.lock().unwrap() += 1;
        self.sem.available.notify_one();
    }
}

/// Raw terminal input. The attached TUI's input thread feeds these into
/// the inbox (9_TUI decision 4) so key handling happens on the loop
/// thread, between message drains; the loop hands them back out of
/// `pump_until` untouched.
pub type UiInput = ratatui::crossterm::event::Event;

/// The unified inbox message. Every producer — UI, LLM worker, tool
/// worker, the loop itself — sends this one enum.
pub(crate) enum LoopMsg {
    Command(SessionCommand),
    LlmChunk {
        agent: AgentId,
        thinking: bool,
        text: String,
    },
    LlmDone {
        agent: AgentId,
        result: Result<Message, String>,
    },
    ToolDone {
        agent: AgentId,
        invoke_id: u64,
        result: Result<serde_json::Value, String>,
    },
    /// Fuel-slice continuation, re-enqueued between slices.
    Continue {
        agent: AgentId,
    },
    /// Terminal input for the embedding TUI; opaque to the loop.
    Ui(UiInput),
}

/// A cloneable command channel into the loop — the only way UIs make
/// things happen.
#[derive(Clone)]
pub struct SessionHandle {
    tx: Sender<LoopMsg>,
}

impl SessionHandle {
    pub fn send(&self, command: SessionCommand) {
        let _ = self.tx.send(LoopMsg::Command(command));
    }

    /// Forward terminal input into the inbox; `false` once the session
    /// is gone (the input thread should exit).
    pub fn send_input(&self, input: UiInput) -> bool {
        self.tx.send(LoopMsg::Ui(input)).is_ok()
    }
}

pub struct Session {
    tree: Tree,
    states: HashMap<AgentId, Runner>,
    root: AgentId,
    /// Child agent → (caller agent, the caller's `agent` invoke id).
    parents: HashMap<AgentId, (AgentId, u64)>,
    registry: ToolRegistry,
    /// Shared client: `complete(&self)` lets several contexts think at
    /// once. Concurrency is bounded by `llm_permits`, not by the client.
    llm: Arc<dyn LlmClient>,
    /// Caps concurrent LLM completions (root + subagents).
    llm_permits: Arc<Semaphore>,
    rx: Receiver<LoopMsg>,
    tx: Sender<LoopMsg>,
    events: Sender<SessionEvent>,
    /// High-water mark of event ids already surfaced as `SessionEvent`s.
    emitted: u64,
    /// Agents whose VM the debugger paused: their `Continue` messages
    /// are parked in `starved` instead of ticking.
    paused: HashSet<AgentId>,
    starved: HashSet<AgentId>,
    done: bool,
    /// The root agent yielded its turn back to the user (it produced a
    /// final answer but, being the top conversation, did not complete).
    /// `run()` stops here; interactive front-ends keep going and clear it
    /// on the next `UserTurn`.
    awaiting_user: bool,
}

impl Session {
    /// Open a session over `tree`: a fresh tree roots a new agent with
    /// `prompt`/`input`; a re-opened log auto-picks a resume anchor
    /// (`pick_resume_leaf`: lowest incomplete leaf, else — every spine
    /// complete — the lowest-id leaf, so the loop still lives for
    /// `ListLeaves`/`Fork`/`Resume`). M4's `open_at` anchors a chosen
    /// leaf instead.
    pub fn new(
        mut tree: Tree,
        prompt: &str,
        input: serde_json::Value,
        registry: ToolRegistry,
        llm: Box<dyn LlmClient>,
        events: Sender<SessionEvent>,
    ) -> io::Result<Self> {
        if tree.events.is_empty() {
            let emitted = tree.id_counter;
            let state = Runner::new_root(&mut tree, prompt, input)?;
            Self::assemble(tree, state, registry, llm, events, emitted)
        } else {
            let leaf = pick_resume_leaf(&tree)?;
            Self::open_at(tree, leaf, registry, llm, events)
        }
    }

    /// Open a re-loaded log anchored at a chosen `leaf` (M4 resume seam,
    /// generalizing `new`'s auto-pick). The root agent is the one `leaf`
    /// belongs to; if `leaf`'s spine is already complete the session
    /// opens idle (a direct `UserTurn` is rejected — fork to continue).
    ///
    /// If the leaf's last assistant turn is an unanswered `run_program`
    /// (the program was interrupted mid-execution), a synthesized report
    /// is appended so the LLM receives it as that call's tool result.
    pub fn open_at(
        mut tree: Tree,
        leaf: EventId,
        registry: ToolRegistry,
        llm: Box<dyn LlmClient>,
        events: Sender<SessionEvent>,
    ) -> io::Result<Self> {
        if !tree.events.contains_key(&leaf) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("cannot open at {leaf:?}: not in the log"),
            ));
        }
        let leaf = synthesize_if_interrupted(&mut tree, leaf)?;
        let emitted = 0; // replay all existing events into the chat pane
        let state = Runner::with_spine(tree.spine_at(leaf));
        Self::assemble(tree, state, registry, llm, events, emitted)
    }

    /// Shared construction for `new`/`open_at`: card the state, derive
    /// the root agent, wire the inbox, and surface any logged events.
    fn assemble(
        tree: Tree,
        mut state: Runner,
        registry: ToolRegistry,
        llm: Box<dyn LlmClient>,
        events: Sender<SessionEvent>,
        emitted: u64,
    ) -> io::Result<Self> {
        state.set_dialect_card(dialect_card(&registry));
        let root = agent_root_of(&tree, state.spine.leaf_id);

        let (tx, rx) = channel();
        let mut session = Session {
            tree,
            states: HashMap::from([(root, state)]),
            root,
            parents: HashMap::new(),
            registry,
            llm: Arc::from(llm),
            llm_permits: Arc::new(Semaphore::new(llm_concurrency())),
            rx,
            tx,
            events,
            emitted,
            paused: HashSet::new(),
            starved: HashSet::new(),
            done: false,
            awaiting_user: false,
        };
        session.emit_new(root);
        Ok(session)
    }

    pub fn handle(&self) -> SessionHandle {
        SessionHandle {
            tx: self.tx.clone(),
        }
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn root(&self) -> AgentId {
        self.root
    }

    pub fn state(&self, agent: AgentId) -> Option<&Runner> {
        self.states.get(&agent)
    }

    /// Active contexts, for agent lists (id, machine status).
    pub fn agents(&self) -> Vec<(AgentId, &'static str)> {
        let mut out: Vec<(AgentId, &'static str)> = self
            .states
            .iter()
            .map(|(id, s)| (*id, s.status()))
            .collect();
        out.sort_by_key(|(id, _)| id.as_u64());
        out
    }

    /// Run until the root agent yields the turn back to the user or
    /// `Shutdown`. The root never *completes* (the top conversation
    /// never ends); `run()` is the one-shot convenience that stops at the
    /// yield. Interactive front-ends drive `pump_until` instead and keep
    /// going across turns.
    pub fn run(mut self) -> Self {
        while self.pump_one() {}
        self
    }

    /// Block for one inbox message and handle it; `false` once the
    /// session is over (`Shutdown`) or the root has yielded its turn.
    pub fn pump_one(&mut self) -> bool {
        if self.done || self.awaiting_user {
            return false;
        }
        match self.rx.recv() {
            Ok(msg) => {
                self.on_msg(msg);
                !(self.done || self.awaiting_user)
            }
            Err(_) => false,
        }
    }

    /// Drain the inbox until `deadline` (the embedding TUI's render
    /// tick), handling session messages on this thread and collecting
    /// terminal input into `inputs`. Returns early once input arrived
    /// and the inbox went momentarily quiet, so keystrokes stay snappy.
    pub fn pump_until(&mut self, deadline: Instant, inputs: &mut Vec<UiInput>) {
        loop {
            let now = Instant::now();
            if now >= deadline {
                // Out of time even if messages are pending (a hot
                // program enqueues a Continue per slice) — render now.
                return;
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(LoopMsg::Ui(input)) => {
                    inputs.push(input);
                    return;
                }
                Ok(msg) => self.on_msg(msg),
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Whether the conversation is over (root agent done / `Shutdown`).
    /// The attached TUI keeps rendering past this for post-mortem
    /// reading; `run()` exits on it.
    #[allow(dead_code)]
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Whether the root agent has yielded its turn back to the user (a
    /// final answer is on the spine and the agent is idle, awaiting the
    /// next `UserTurn`). Distinct from `is_done`: the conversation lives.
    #[allow(dead_code)]
    pub fn is_awaiting_user(&self) -> bool {
        self.awaiting_user
    }

    // ── debugger controls (privileged: same thread as the loop) ──────

    /// Pause/resume an agent's VM. Pausing parks its fuel-slice
    /// continuations; resuming re-enqueues a parked one.
    pub fn set_paused(&mut self, agent: AgentId, paused: bool) {
        if paused {
            self.paused.insert(agent);
        } else if self.paused.remove(&agent) && self.starved.remove(&agent) {
            let _ = self.tx.send(LoopMsg::Continue { agent });
        }
    }

    pub fn is_paused(&self, agent: AgentId) -> bool {
        self.paused.contains(&agent)
    }

    /// Run one slice of at most `fuel` instructions on a (paused)
    /// agent — the debugger's step keys.
    pub fn step_paused(&mut self, agent: AgentId, fuel: u64) {
        let _ = self.step_agent(agent, StepInput::Tick { fuel });
    }

    fn on_msg(&mut self, msg: LoopMsg) {
        if let Err(e) = self.dispatch(msg) {
            self.emit(SessionEvent::Error {
                agent: None,
                message: format!("session io error: {e}"),
            });
            self.done = true;
        }
    }

    fn dispatch(&mut self, msg: LoopMsg) -> io::Result<()> {
        match msg {
            LoopMsg::Command(SessionCommand::Shutdown) => {
                self.done = true;
                Ok(())
            }
            LoopMsg::Command(SessionCommand::UserTurn(text)) => {
                let root = self.root;
                let Some(state) = self.states.get(&root) else {
                    return Ok(());
                };
                if !state.is_idle() {
                    // Steering a busy agent is M2's host-injected
                    // condition; until then the command is rejected.
                    self.emit(SessionEvent::Error {
                        agent: Some(root),
                        message: format!("agent is busy ({})", state.status()),
                    });
                    return Ok(());
                }
                if state.spine.is_complete() {
                    // Nothing may follow a `FrameResult`; fork from an
                    // earlier event to continue past a finished spine.
                    self.emit(SessionEvent::Error {
                        agent: Some(root),
                        message: "spine is complete; fork from an earlier event to continue".into(),
                    });
                    return Ok(());
                }
                self.awaiting_user = false; // the user took their turn
                self.step_agent(root, StepInput::UserTurn(text))
            }
            LoopMsg::Command(SessionCommand::ListLeaves) => {
                let leaves = self.leaf_infos();
                self.emit(SessionEvent::Leaves(leaves));
                Ok(())
            }
            LoopMsg::Command(SessionCommand::Label(text)) => self.cmd_label(text),
            LoopMsg::Command(SessionCommand::Fork { from, label }) => self.cmd_fork(from, label),
            LoopMsg::Command(SessionCommand::Resume(leaf)) => self.cmd_resume(leaf),
            LoopMsg::LlmChunk {
                agent,
                thinking,
                text,
            } => {
                self.emit(SessionEvent::Chunk {
                    agent,
                    thinking,
                    text,
                });
                Ok(())
            }
            LoopMsg::LlmDone { agent, result } => match result {
                Ok(message) => self.step_agent(agent, StepInput::LlmResponse(message)),
                Err(message) => {
                    self.emit(SessionEvent::Error {
                        agent: Some(agent),
                        message: message.clone(),
                    });
                    match self.parents.get(&agent).copied() {
                        // A dead child rejects the caller's `agent` call.
                        Some((parent, invoke_id)) => {
                            let _ = self.tx.send(LoopMsg::ToolDone {
                                agent: parent,
                                invoke_id,
                                result: Err(format!("subagent failed: {message}")),
                            });
                        }
                        None => self.done = true,
                    }
                    Ok(())
                }
            },
            LoopMsg::ToolDone {
                agent,
                invoke_id,
                result,
            } => self.step_agent(
                agent,
                StepInput::ToolResults(vec![ToolResult { invoke_id, result }]),
            ),
            LoopMsg::Continue { agent } => {
                if self.paused.contains(&agent) {
                    // Park the slice; `set_paused(false)` re-enqueues it.
                    self.starved.insert(agent);
                    return Ok(());
                }
                self.step_agent(agent, StepInput::Tick { fuel: FUEL_SLICE })
            }
            // Handled by `pump_until`; harmless if one reaches `run()`.
            LoopMsg::Ui(_) => Ok(()),
        }
    }

    /// The active root agent's id if it is idle; otherwise emit a
    /// rejection and return `None`. Fork/label/resume re-anchor the root
    /// and must not tear down a running VM (like `UserTurn`).
    fn idle_root(&mut self) -> Option<AgentId> {
        let root = self.root;
        match self.states.get(&root) {
            Some(state) if state.is_idle() => Some(root),
            Some(state) => {
                let status = state.status();
                self.emit(SessionEvent::Error {
                    agent: Some(root),
                    message: format!("agent is busy ({status})"),
                });
                None
            }
            None => None,
        }
    }

    fn cmd_label(&mut self, text: String) -> io::Result<()> {
        let Some(root) = self.idle_root() else {
            return Ok(());
        };
        let state = self.states.get_mut(&root).expect("idle_root checked");
        if state.spine.is_complete() {
            self.emit(SessionEvent::Error {
                agent: Some(root),
                message: "spine is complete; cannot label past a FrameResult".into(),
            });
            return Ok(());
        }
        self.tree
            .append(&mut state.spine, EventPayload::Label(text))?;
        self.emit_new(root);
        let leaves = self.leaf_infos();
        self.emit(SessionEvent::Leaves(leaves));
        Ok(())
    }

    fn cmd_fork(&mut self, from: EventId, label: Option<String>) -> io::Result<()> {
        if self.idle_root().is_none() {
            return Ok(());
        }
        let mut spine = match self.tree.fork(from) {
            Ok(spine) => spine,
            Err(e) => {
                self.emit(SessionEvent::Error {
                    agent: None,
                    message: format!("fork failed: {e}"),
                });
                return Ok(());
            }
        };
        if let Some(text) = label {
            self.tree.append(&mut spine, EventPayload::Label(text))?;
        }
        self.reanchor_root(spine);
        let leaves = self.leaf_infos();
        self.emit(SessionEvent::Leaves(leaves));
        Ok(())
    }

    fn cmd_resume(&mut self, leaf: EventId) -> io::Result<()> {
        if self.idle_root().is_none() {
            return Ok(());
        }
        if !self.tree.events.contains_key(&leaf) {
            self.emit(SessionEvent::Error {
                agent: None,
                message: format!("cannot resume {leaf:?}: not in the log"),
            });
            return Ok(());
        }
        let spine = self.tree.spine_at(leaf);
        if spine.is_complete() {
            self.emit(SessionEvent::Error {
                agent: None,
                message: format!(
                    "{leaf:?} is a completed spine; fork from an earlier event to continue"
                ),
            });
            return Ok(());
        }
        self.reanchor_root(spine);
        let leaves = self.leaf_infos();
        self.emit(SessionEvent::Leaves(leaves));
        Ok(())
    }

    /// Make `spine` the active root: card a fresh `Runner`, key it by
    /// its agent's `Agent` (replacing any prior in-memory state for
    /// that agent — the superseded branch stays in the tree, re-listable
    /// via `ListLeaves`), and point `root` at it. Any freshly logged
    /// events (a fork's `Label`) are surfaced.
    fn reanchor_root(&mut self, spine: Spine) {
        let mut state = Runner::with_spine(spine);
        state.set_dialect_card(dialect_card(&self.registry));
        let root = agent_root_of(&self.tree, state.spine.leaf_id);
        self.states.insert(root, state);
        self.root = root;
        self.emit_new(root);
    }

    /// The tree's leaves as serializable `LeafInfo`s, lowest id first,
    /// the current active root leaf flagged.
    fn leaf_infos(&self) -> Vec<LeafInfo> {
        let active = self.states.get(&self.root).map(|s| s.spine.leaf_id);
        let mut leaves = self.tree.list_leaves();
        leaves.sort_by_key(|(id, _)| id.as_u64());
        leaves
            .into_iter()
            .map(|(leaf, label)| LeafInfo {
                leaf,
                agent: agent_root_of(&self.tree, leaf),
                label,
                complete: self.tree.spine_at(leaf).is_complete(),
                active: Some(leaf) == active,
                summary: leaf_summary(&self.tree, leaf),
            })
            .collect()
    }

    fn step_agent(&mut self, agent: AgentId, input: StepInput) -> io::Result<()> {
        let Some(state) = self.states.get_mut(&agent) else {
            return Ok(());
        };
        let outputs = state.step(&mut self.tree, input)?;
        let transitions = state.take_status_transitions();
        self.emit_new(agent);
        for (program, status) in transitions {
            self.emit(SessionEvent::ProgramStatus {
                agent,
                program,
                status,
            });
        }
        self.process(agent, outputs)
    }

    fn process(&mut self, agent: AgentId, outputs: Vec<StepOutput>) -> io::Result<()> {
        for output in outputs {
            match output {
                StepOutput::LlmRequest(request) => self.spawn_llm(agent, request),
                StepOutput::ToolCalls(calls) => self.spawn_tools(agent, calls),
                StepOutput::SpawnAgents(spawns) => {
                    for spawn in spawns {
                        self.spawn_child(agent, spawn)?;
                    }
                }
                StepOutput::AgentDone(result) => match self.parents.get(&agent).copied() {
                    Some((parent, invoke_id)) => {
                        let _ = self.tx.send(LoopMsg::ToolDone {
                            agent: parent,
                            invoke_id,
                            result: guard_size(Ok(result)),
                        });
                    }
                    // A parentless agent finishing is the root; it yields
                    // rather than completing (see `StepOutput::Yielded`).
                    // Reaching here means a non-root agent had no caller —
                    // end the session rather than strand it.
                    None => self.done = true,
                },
                // The root produced a final answer and yielded the turn:
                // the conversation stays open, idle, awaiting the user.
                StepOutput::Yielded => self.awaiting_user = true,
                StepOutput::Working => {
                    let _ = self.tx.send(LoopMsg::Continue { agent });
                }
            }
        }
        Ok(())
    }

    /// One worker thread per in-flight completion (blocking reads live
    /// there; chunks and the final message come back through the inbox).
    fn spawn_llm(&self, agent: AgentId, request: LlmRequest) {
        let llm = Arc::clone(&self.llm);
        let permits = Arc::clone(&self.llm_permits);
        let tx = self.tx.clone();
        thread::spawn(move || {
            // Block off-loop until a completion slot is free; the permit
            // is held only for this `complete()` call and released on drop.
            let _permit = permits.acquire();
            let mut on_chunk = |chunk: LlmChunk| {
                let (thinking, text) = match chunk {
                    LlmChunk::Text(t) => (false, t),
                    LlmChunk::Thinking(t) => (true, t),
                };
                let _ = tx.send(LoopMsg::LlmChunk {
                    agent,
                    thinking,
                    text,
                });
            };
            let result = llm.complete(&request, &mut on_chunk);
            let _ = tx.send(LoopMsg::LlmDone { agent, result });
        });
    }

    /// Spawn-per-call fan-out; completions arrive at the inbox in
    /// whatever order the tools finish — that arrival order is the
    /// logged resolution order.
    fn spawn_tools(&self, agent: AgentId, calls: Vec<OutCall>) {
        for call in calls {
            match self.registry.get(&call.name) {
                Some(def) => {
                    let def = Arc::clone(def);
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let result = guard_size((def.handler)(call.args));
                        let _ = tx.send(LoopMsg::ToolDone {
                            agent,
                            invoke_id: call.invoke_id,
                            result,
                        });
                    });
                }
                None => {
                    let _ = self.tx.send(LoopMsg::ToolDone {
                        agent,
                        invoke_id: call.invoke_id,
                        result: Err(format!("unknown tool `{}`", call.name)),
                    });
                }
            }
        }
    }

    /// The `agent` tool: a `SpawnAgent` becomes a child `Runner` on
    /// a branch rooted at the caller's call site.
    fn spawn_child(&mut self, parent: AgentId, spawn: SpawnAgent) -> io::Result<()> {
        let SpawnAgent {
            invoke_id,
            prompt,
            input,
            budget,
        } = spawn;
        let call_site = self.states[&parent].spine.leaf_id;
        let mut child = Runner::new_child(&mut self.tree, call_site, prompt, input, budget)?;
        child.set_dialect_card(dialect_card(&self.registry));
        let child_id = child.spine.leaf_id; // the Agent it was rooted at
        self.emit_new(child_id);
        self.parents.insert(child_id, (parent, invoke_id));
        let outputs = child.kickoff(&mut self.tree)?;
        self.states.insert(child_id, child);
        self.process(child_id, outputs)
    }

    /// Surface every newly logged event as a `SessionEvent`, attributed
    /// to the agent just stepped (a `Agent` is its own agent).
    fn emit_new(&mut self, agent: AgentId) {
        while self.emitted < self.tree.id_counter {
            self.emitted += 1;
            let id = EventId::new(self.emitted);
            let Some(event) = self.tree.events.get(&id) else {
                continue;
            };
            let owner = if matches!(event.payload, EventPayload::Agent { .. }) {
                id
            } else {
                agent
            };
            let event = event.clone();
            let _ = self.events.send(SessionEvent::Event {
                agent: owner,
                event,
            });
        }
    }

    fn emit(&mut self, event: SessionEvent) {
        let _ = self.events.send(event);
    }
}

/// If the leaf is an unanswered `run_program` (the program was
/// interrupted before completing), synthesize a tool result so the LLM
/// receives it as that call's response and can rewrite.
/// Returns the (possibly updated) leaf id.
fn synthesize_if_interrupted(tree: &mut Tree, leaf: EventId) -> io::Result<EventId> {
    let spine = tree.spine_at(leaf);
    let msgs = &spine.context().messages;
    let Some(Message::Assistant { tool_calls, .. }) = msgs.last() else {
        return Ok(leaf);
    };
    let Some(call) = tool_calls.first() else {
        return Ok(leaf);
    };
    if call.name.as_str() != crate::machine::TOOL_RUN_PROGRAM {
        return Ok(leaf);
    }
    // Already answered? (Tool message with matching call_id)
    if msgs
        .iter()
        .any(|m| matches!(m, Message::Tool { call_id, .. } if *call_id == call.id))
    {
        return Ok(leaf);
    }

    // Collect artifacts from the agent's spine segment.
    let mut artifacts = Vec::new();
    let mut current = leaf;
    while let Some(event) = tree.events.get(&current) {
        match &event.payload {
            EventPayload::Call(Call::Invoke { name, args, .. }) => {
                artifacts.push(format!(
                    "[#{}] {}({})",
                    event.id.as_u64(),
                    name,
                    crate::report::preview(args)
                ));
            }
            EventPayload::Agent { .. } => break,
            _ => {}
        }
        match event.parent_id {
            Some(parent) => current = parent,
            None => break,
        }
    }
    artifacts.reverse();

    let mut report = String::from("## program interrupted\n");
    report.push_str(
        "This program was interrupted before completing. The artifacts \
         below are still fetchable by id — rewrite to continue.\n",
    );
    report.push_str("\n## artifacts — fetch with tools.tool_result(id)\n");
    if artifacts.is_empty() {
        report.push_str("(none)\n");
    } else {
        for a in &artifacts {
            report.push_str(a);
            report.push('\n');
        }
    }
    report.push_str("\n## restarts\n");
    report.push_str(
        "- run_program(source): rewrite the program to continue from \
         where it left off; all artifacts above are still valid.\n",
    );

    let mut spine = tree.spine_at(leaf);
    let new_leaf = tree.append(
        &mut spine,
        EventPayload::Message(Message::Tool {
            name: crate::machine::TOOL_RUN_PROGRAM.into(),
            call_id: call.id.clone(),
            text: report,
        }),
    )?;

    Ok(new_leaf)
}

/// Auto-pick a resume anchor for a re-opened log: the lowest-id
/// incomplete leaf, or — every spine complete — the lowest-id leaf
/// (so the loop still lives for `ListLeaves`/`Fork`/`Resume`). Errors
/// only on a non-empty log with no leaves at all (corrupt).
fn pick_resume_leaf(tree: &Tree) -> io::Result<EventId> {
    let mut leaves: Vec<EventId> = tree.list_leaves().into_iter().map(|(id, _)| id).collect();
    leaves.sort_by_key(|id| id.as_u64());
    leaves
        .iter()
        .copied()
        .find(|id| !tree.spine_at(*id).is_complete())
        .or_else(|| leaves.first().copied())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "log has no leaves"))
}

/// One-line preview of an event for the leaf list, clipped to the
/// report preview bound.
fn leaf_summary(tree: &Tree, leaf: EventId) -> String {
    let Some(event) = tree.events.get(&leaf) else {
        return String::new();
    };
    let s = match &event.payload {
        EventPayload::Agent { prompt, .. } => format!("Agent: {prompt}"),
        EventPayload::FrameResult { result } => format!("FrameResult: {result}"),
        EventPayload::Message(Message::User { text }) => format!("User: {text}"),
        EventPayload::Message(Message::Assistant {
            text, tool_calls, ..
        }) => {
            if tool_calls.is_empty() {
                format!("Assistant: {text}")
            } else {
                let names: Vec<&str> = tool_calls.iter().map(|c| c.name.as_str()).collect();
                format!("Assistant: ⚙ {}", names.join(", "))
            }
        }
        EventPayload::Message(Message::System { .. }) => "System".into(),
        EventPayload::Message(Message::Tool { name, .. }) => format!("Tool: {name}"),
        EventPayload::Call(Call::Invoke { name, .. }) => format!("Invoke: {name}"),
        EventPayload::Call(Call::Send { expects_reply, .. }) => {
            format!("Send: {}", if *expects_reply { "ask" } else { "tell" })
        }
        EventPayload::Call(Call::Spawn { name, .. }) => {
            format!("Spawn: {}", name.as_deref().unwrap_or("<unnamed>"))
        }
        EventPayload::Result { call, outcome } => match outcome {
            Outcome::Delivered(v) => format!("Result of #{}: {v}", call.as_u64()),
            Outcome::Failed(msg) => format!("Result of #{}: failed: {msg}", call.as_u64()),
        },
        EventPayload::ProgramResult { value } => format!("ProgramResult: {value}"),
        EventPayload::Label(label) => format!("Label: {label}"),
        EventPayload::Console { lines } => format!("Console: {} lines", lines.len()),
    };
    crate::report::clip(&s, crate::report::PREVIEW_MAX_BYTES)
}

/// The innermost `Agent` at or above `leaf`.
fn agent_root_of(tree: &Tree, leaf: EventId) -> AgentId {
    let mut current = leaf;
    loop {
        let Some(event) = tree.events.get(&current) else {
            return leaf;
        };
        if matches!(event.payload, EventPayload::Agent { .. }) {
            return current;
        }
        match event.parent_id {
            Some(parent) => current = parent,
            None => return current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    use crate::types::ToolCall;

    fn tool(
        name: &str,
        handler: impl Fn(serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync + 'static,
    ) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: String::new(),
            input_schema: json!({ "type": "array" }),
            handler: Box::new(handler),
        }
    }

    /// Build a session over a fresh tree, send one user turn, run to
    /// completion, and return it with the buffered `SessionEvent`s.
    fn run_session(
        registry: ToolRegistry,
        script: Vec<Message>,
        user_turn: &str,
    ) -> (Session, Vec<SessionEvent>) {
        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "test agent",
            json!(null),
            registry,
            Box::new(ScriptedLlm::new(script)),
            tx,
        )
        .unwrap();
        session
            .handle()
            .send(SessionCommand::UserTurn(user_turn.into()));
        let session = session.run();
        let events = rx.try_iter().collect();
        (session, events)
    }

    /// Payload kinds of one agent's spine segment, log order.
    fn kinds(tree: &Tree, leaf: EventId) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut current = leaf;
        loop {
            let event = &tree.events[&current];
            out.push(match &event.payload {
                EventPayload::Agent { .. } => "Agent",
                EventPayload::FrameResult { .. } => "FrameResult",
                EventPayload::Message(Message::User { .. }) => "User",
                EventPayload::Message(Message::Assistant { .. }) => "Assistant",
                EventPayload::Message(Message::System { .. }) => "System",
                EventPayload::Message(Message::Tool { .. }) => "Tool",
                EventPayload::Call(_) => "Call",
                EventPayload::Result { .. } => "Result",
                EventPayload::ProgramResult { .. } => "ProgramResult",
                EventPayload::Console { .. } => "Console",
                EventPayload::Label(_) => "Label",
            });
            if matches!(event.payload, EventPayload::Agent { .. }) {
                break;
            }
            match event.parent_id {
                Some(parent) => current = parent,
                None => break,
            }
        }
        out.reverse();
        out
    }

    fn root_leaf(session: &Session) -> EventId {
        session.state(session.root()).unwrap().spine.leaf_id
    }

    fn tool_texts(session: &Session) -> Vec<String> {
        session
            .state(session.root())
            .unwrap()
            .spine
            .context()
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Tool { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn m0_scripted_demo_end_to_end() {
        let (tx, rx) = channel();
        let session = run_demo(Tree::new(None), tx).unwrap();
        let events: Vec<SessionEvent> = rx.try_iter().collect();

        // The whole M0 arc, asserted on the event log.
        assert_eq!(
            kinds(session.tree(), root_leaf(&session)),
            [
                "Agent",
                "System",
                "User",
                "Assistant",
                // Calls are logged at dispatch, their results at landing.
                "Call",
                "Call",
                "Result",
                "Result",
                "ProgramResult",
                "Tool",
                "Console",
                "Assistant",
                // The root yields its final answer to the user; the top
                // conversation never ends, so no `FrameResult` is logged.
            ]
        );
        // Both fan-out results landed (order is completion order).
        let results: Vec<serde_json::Value> = session
            .tree()
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Result { outcome, .. } => outcome.value().cloned(),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert!(results.contains(&json!("alpha")) && results.contains(&json!("beta")));
        let report = &tool_texts(&session)[0];
        assert!(report.contains("fanned out"), "{report}");

        // The UI saw it all through the serializable channel: logged
        // events, live chunks, and everything serde round-trips.
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::Chunk {
                thinking: false,
                ..
            }
        )));
        let n_logged = events
            .iter()
            .filter(|e| matches!(e, SessionEvent::Event { .. }))
            .count();
        assert_eq!(n_logged, session.tree().events.len());
        for event in &events {
            let json = serde_json::to_string(event).unwrap();
            let _: SessionEvent = serde_json::from_str(&json).unwrap();
        }
    }

    /// Records each request's system message before delegating to the
    /// scripted client — asserts on what actually crosses the LLM trait.
    struct CapturingLlm {
        inner: ScriptedLlm,
        seen: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl LlmClient for CapturingLlm {
        fn complete(
            &self,
            request: &LlmRequest,
            chunk: &mut dyn FnMut(LlmChunk),
        ) -> Result<Message, String> {
            if let Some(Message::System { text }) = request.messages.first() {
                self.seen.lock().unwrap().push(text.clone());
            }
            self.inner.complete(request, chunk)
        }
    }

    #[test]
    fn dialect_card_reaches_the_llm_with_the_tool_list() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("fetch_page", |_| Ok(json!(null))));
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            inner: ScriptedLlm::new([scripted_text("done")]),
            seen: std::sync::Arc::clone(&seen),
        };
        let (tx, _rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "agent prompt here",
            json!(null),
            registry,
            Box::new(llm),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn("go".into()));
        session.run();

        let seen = seen.lock().unwrap();
        let system = seen.first().expect("a system message");
        assert!(system.starts_with("You act by writing JavaScript programs"));
        assert!(system.contains("- tools.fetch_page"), "{system}");
        assert!(
            system.contains("agent prompt here"),
            "agent prompt follows the card"
        );
    }

    #[test]
    fn fanout_logs_in_completion_order() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("slow", |_| {
            std::thread::sleep(Duration::from_millis(40));
            Ok(json!("slow"))
        }));
        registry.register(tool("fast", |_| Ok(json!("fast"))));
        let script = vec![
            scripted_program(
                "c1",
                "const s = tools.slow(); const f = tools.fast(); return [await s, await f];",
            ),
            scripted_text("done"),
        ];
        let (session, _) = run_session(registry, script, "race them");

        // Calls are logged at *dispatch* (issue order); their `Result`s
        // land in completion order, which is what the inbox decides.
        let mut results: Vec<(u64, serde_json::Value)> = session
            .tree()
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Result { outcome, .. } => {
                    Some((e.id.as_u64(), outcome.value().cloned()?))
                }
                _ => None,
            })
            .collect();
        results.sort_by_key(|(id, _)| *id);
        let order: Vec<&serde_json::Value> = results.iter().map(|(_, v)| v).collect();
        assert_eq!(
            order,
            [&json!("fast"), &json!("slow")],
            "inbox arrival order is the logged resolution order"
        );
        // The program still saw its own await order.
        assert!(tool_texts(&session)[0].contains(r#"returned: ["slow","fast"]"#));
    }

    #[test]
    fn menu_has_no_effectful_warning() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("send_email", |_| Ok(json!({ "sent": true }))));
        let script = vec![
            scripted_program(
                "c1",
                r#"await tools.send_email("hi"); raise("inspect", null);"#,
            ),
            scripted_text("stopping here"),
        ];
        let (session, _) = run_session(registry, script, "send it");

        let report = tool_texts(&session)
            .into_iter()
            .find(|t| t.contains("send_email"))
            .expect("a report listing the artifact");
        assert!(
            !report.contains("effectful"),
            "effectful flag removed: {report}"
        );
        assert!(
            !report.contains("already happened; calling again"),
            "effectful warning gone: {report}"
        );
    }

    /// A large file body inlined into `source` (no `attachments`) draws
    /// the attachments nudge in the completion report.
    #[test]
    fn inlined_large_body_nudges_toward_attachments() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("create_file", |_| Ok(json!({ "version": "v1" }))));
        let big = "x".repeat(600); // > INLINE_BODY_ADVICE_BYTES
        let script = vec![
            scripted_program(
                "c1",
                &format!(r#"await tools.create_file("/x/a.js", "{big}"); return "ok";"#),
            ),
            scripted_text("done"),
        ];
        let (session, _) = run_session(registry, script, "write it");
        let report = tool_texts(&session)
            .into_iter()
            .find(|t| t.contains("program completed"))
            .expect("a completion report");
        assert!(report.contains("inlined into `source`"), "{report}");
    }

    /// The same large body, passed through `attachments` and referenced
    /// from `source`, is rewarded: no nudge even though the written content
    /// is large (the model is using the channel).
    #[test]
    fn attachments_suppress_the_inline_nudge() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("create_file", |_| Ok(json!({ "version": "v1" }))));
        let prog = Message::Assistant {
            text: String::new(),
            thinking: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "run_program".into(),
                arguments: json!({
                    "source": r#"await tools.create_file("/x/a.js", attachments.body); return "ok";"#,
                    "attachments": { "body": "x".repeat(600) },
                }),
            }],
        };
        let script = vec![prog, scripted_text("done")];
        let (session, _) = run_session(registry, script, "write it");
        let report = tool_texts(&session)
            .into_iter()
            .find(|t| t.contains("program completed"))
            .expect("a completion report");
        assert!(!report.contains("inlined into `source`"), "{report}");
    }

    #[test]
    fn oversized_result_is_guarded_before_the_log() {
        let mut registry = ToolRegistry::new();
        // MAX_RESULT_BYTES is now MB-scale (16 MB); trigger it.
        registry.register(tool("big", |_| Ok(json!("x".repeat(MAX_RESULT_BYTES + 1)))));
        let script = vec![
            scripted_program(
                "c1",
                r#"try { return await tools.big(); } catch (e) { return "rejected: " + e; }"#,
            ),
            scripted_text("done"),
        ];
        let (session, _) = run_session(registry, script, "fetch something huge");

        // The call is logged at dispatch either way; the guard shows up as
        // a `Failed` outcome on its `Result` — definitively did not work,
        // as distinct from a call with no `Result` at all.
        assert!(
            session
                .tree()
                .events
                .values()
                .any(|e| matches!(&e.payload, EventPayload::Call(Call::Invoke { name, .. }) if name == "big")),
            "the call is still logged"
        );
        let outcome = session
            .tree()
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Result { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .expect("the call settled");
        let Outcome::Failed(error) = &outcome else {
            panic!("expected a Failed outcome, got {outcome:?}");
        };
        assert!(error.contains("result too large"), "{error}");
        let program_result = session
            .tree()
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::ProgramResult { value } => value.as_str().map(str::to_owned),
                _ => None,
            })
            .unwrap();
        assert!(program_result.starts_with("rejected: result too large"));
    }

    /// M2: a `raise` with payload round-trips through the full session
    /// loop — condition report logged as a `Tool` event, the LLM's
    /// `resume(value)` re-enters the *same* VM, and the resumed value
    /// becomes the raise expression's result.
    #[test]
    fn raise_round_trips_resume_through_the_session() {
        let script = vec![
            scripted_program(
                "c1",
                r#"const x = raise("need_value", { why: "no default" }); return x + 1;"#,
            ),
            scripted_resume("c2", json!(41)),
            scripted_text("got it"),
        ];
        let (session, _) = run_session(ToolRegistry::new(), script, "compute it");

        // The condition report reached the log as a Tool event naming the
        // condition and previewing its payload.
        let reports = tool_texts(&session);
        assert!(
            reports[0].contains("need_value") && reports[0].contains("no default"),
            "{}",
            reports[0]
        );
        // Resume continued the same VM: x = 41, so it returned 42.
        assert!(
            reports.iter().any(|t| t.contains("returned: 42")),
            "{reports:?}"
        );
        // A raise consumes no tools, so the spine carries no Invoke; the
        // arc is program → condition → resume → completion → text.
        let spine = kinds(session.tree(), root_leaf(&session));
        assert!(!spine.contains(&"Call"), "{spine:?}");
        // Root yields its final answer (no `FrameResult`); the top
        // conversation never ends.
        assert_eq!(spine.last(), Some(&"Assistant"));

        // The completion report must answer the *resume* call ("c2"), not
        // the original run_program ("c1") — otherwise the next chat
        // request has an assistant tool_call with no matching tool reply
        // and the provider 400s.
        let completion_call_id = session
            .tree()
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Message(Message::Tool { call_id, text, .. })
                    if text.contains("returned: 42") =>
                {
                    Some(call_id.clone())
                }
                _ => None,
            })
            .expect("a completion report");
        assert_eq!(completion_call_id, "c2");
    }

    /// The `run_program` Assistant event id — the program block key
    /// (decision 2) the `ProgramStatus` events reference.
    fn run_program_id(tree: &Tree) -> EventId {
        tree.events
            .values()
            .find(|e| {
                matches!(&e.payload,
                    EventPayload::Message(Message::Assistant { tool_calls, .. })
                        if tool_calls.first().is_some_and(|c| c.name == crate::machine::TOOL_RUN_PROGRAM))
            })
            .map(|e| e.id)
            .expect("a run_program Assistant event")
    }

    /// The `ProgramStatus` statuses emitted for `program`, in order.
    fn statuses_for(events: &[SessionEvent], program: EventId) -> Vec<ProgramStatus> {
        events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::ProgramStatus {
                    program: p, status, ..
                } if *p == program => Some(*status),
                _ => None,
            })
            .collect()
    }

    /// Step 2 (decision 5): a plain program emits `Running → Completed`
    /// for its block's id (the `run_program` Assistant event).
    #[test]
    fn program_status_runs_then_completes() {
        let script = vec![
            scripted_program("c1", "return 1 + 1;"),
            scripted_text("done"),
        ];
        let (session, events) = run_session(ToolRegistry::new(), script, "go");
        let program = run_program_id(session.tree());
        assert_eq!(
            statuses_for(&events, program),
            [ProgramStatus::Running, ProgramStatus::Completed]
        );
    }

    /// Step 2 (decision 5): a raise+resume folds into one block whose
    /// status walks `Running → Suspended → Running → Completed`, all under
    /// the original `run_program` id (the `resume` keeps it).
    #[test]
    fn program_status_tracks_raise_and_resume() {
        let script = vec![
            scripted_program("c1", r#"const x = raise("need", null); return x;"#),
            scripted_resume("c2", json!(7)),
            scripted_text("done"),
        ];
        let (session, events) = run_session(ToolRegistry::new(), script, "go");
        let program = run_program_id(session.tree());
        assert_eq!(
            statuses_for(&events, program),
            [
                ProgramStatus::Running,
                ProgramStatus::Suspended,
                ProgramStatus::Running,
                ProgramStatus::Completed,
            ]
        );
    }

    /// Step 2 (decision 4): the first event after an agent's `Agent`
    /// is the stored `Message::System` — the assembled card + prompt.
    #[test]
    fn agent_root_is_followed_by_the_stored_system_prompt() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("fetch_page", |_| Ok(json!(null))));
        let (session, _) = run_session(registry, vec![scripted_text("done")], "go");
        let tree = session.tree();
        // Agent is #1; the system prompt is the next event, #2.
        let system = &tree.events[&EventId::new(2)];
        assert_eq!(system.parent_id, Some(EventId::new(1)));
        let EventPayload::Message(Message::System { text }) = &system.payload else {
            panic!("the event after Agent must be the system prompt");
        };
        assert!(text.contains("- tools.fetch_page"), "card present: {text}");
        assert!(text.contains("test agent"), "agent prompt follows the card");
    }

    /// M2: a trapped runtime error reports, and the rewrite restart reuses
    /// the already-logged tool result by id (`tools.tool_result`) instead
    /// of repeating the call — served from the log, no second Invoke.
    #[test]
    fn trapped_error_rewrite_reuses_artifact_through_the_session() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("fetch", |_| Ok(json!("DATA"))));
        // Event ids are deterministic: Agent 1, System 2, User 3,
        // Assistant 4, the fetch Invoke 5 — so the rewrite names
        // `tool_result(5)` (the stored system prompt is id 2, decision 4).
        let script = vec![
            scripted_program(
                "c1",
                r#"await tools.fetch("expensive"); const v = null; return v.x;"#,
            ),
            scripted_program("c2", "return await tools.tool_result(5);"),
            scripted_text("done"),
        ];
        let (session, _) = run_session(registry, script, "fetch then trip");

        // The fetch really is artifact #5 (guards the hardcoded id above).
        let fetch_invoke = session
            .tree()
            .events
            .values()
            .find(|e| matches!(&e.payload, EventPayload::Call(Call::Invoke { name, .. }) if name == "fetch"))
            .expect("the fetch Invoke");
        assert_eq!(fetch_invoke.id.as_u64(), 5);

        // The condition report rendered the trapped error and the menu.
        let reports = tool_texts(&session);
        assert!(
            reports[0].contains("[#5]") && reports[0].contains("fetch"),
            "{}",
            reports[0]
        );

        // The rewrite reused the artifact: exactly one fetch Invoke on the
        // spine (no repeat), and the completion returns the reused value.
        let fetch_invokes = session
            .tree()
            .events
            .values()
            .filter(|e| matches!(&e.payload, EventPayload::Call(Call::Invoke { name, .. }) if name == "fetch"))
            .count();
        assert_eq!(
            fetch_invokes, 1,
            "fetch must not be repeated by the rewrite"
        );
        assert!(
            reports.iter().any(|t| t.contains(r#"returned: "DATA""#)),
            "{reports:?}"
        );
    }

    #[test]
    fn agent_tool_spawns_child_agent_and_joins() {
        let script = vec![
            scripted_program(
                "c1",
                r#"return await tools.agent({ prompt: "child task", input: { n: 1 } });"#,
            ),
            scripted_text("child says 42"),
            scripted_text("parent done"),
        ];
        let (session, events) = run_session(ToolRegistry::new(), script, "delegate this");
        let tree = session.tree();

        // Two spines: the caller's and the (now completed) child's.
        assert_eq!(tree.list_leaves().len(), 2);
        let child_start = tree
            .events
            .values()
            .find(|e| {
                matches!(&e.payload, EventPayload::Agent { prompt, .. } if prompt == "child task")
            })
            .expect("child Agent");
        let child_leaf = tree
            .list_leaves()
            .into_iter()
            .map(|(id, _)| id)
            .find(|id| *id != root_leaf(&session))
            .unwrap();
        assert_eq!(
            kinds(tree, child_leaf),
            ["Agent", "System", "Assistant", "FrameResult"]
        );

        // The join: the child's result is the caller's logged artifact
        // and reaches the caller's program.
        assert!(kinds(tree, root_leaf(&session)).contains(&"Call"));
        assert!(tool_texts(&session)[0].contains(r#"returned: "child says 42""#));

        // Child events were attributed to the child agent.
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::Event { agent, event } if *agent == child_start.id && event.id == child_start.id
        )));
    }

    /// M3: `Promise.all` over two `tools.agent` calls spawns both child
    /// contexts concurrently (one fan-out batch, two branches) and joins
    /// both results back into the parent program.
    #[test]
    fn promise_all_over_concurrent_agents_joins_both() {
        let script = vec![
            scripted_program(
                "c1",
                r#"return await Promise.all([
                    tools.agent({ prompt: "task A", input: { id: 1 } }),
                    tools.agent({ prompt: "task B", input: { id: 2 } }),
                ]);"#,
            ),
            // Two child turns; which child pops which is race-dependent
            // (shared scripted client), so the assertions below are
            // order-independent (set membership, not position).
            scripted_text("done: A"),
            scripted_text("done: B"),
            scripted_text("both back"),
        ];
        let (session, _) = run_session(ToolRegistry::new(), script, "delegate two");
        let tree = session.tree();

        // Three spines: the caller plus the two (completed) children.
        assert_eq!(tree.list_leaves().len(), 3);

        // Both child contexts were rooted, each with the prompt it was given.
        let child_prompts: HashSet<String> = tree
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Agent { prompt, .. } => Some(prompt.clone()),
                _ => None,
            })
            .collect();
        assert!(child_prompts.contains("task A") && child_prompts.contains("task B"));

        // Each child spine ran to completion independently.
        for (leaf, _) in tree.list_leaves() {
            if leaf == root_leaf(&session) {
                continue;
            }
            assert_eq!(
                kinds(tree, leaf),
                ["Agent", "System", "Assistant", "FrameResult"]
            );
        }

        // The caller logged both agent calls as artifacts on its spine…
        let invokes = kinds(tree, root_leaf(&session))
            .iter()
            .filter(|k| **k == "Call")
            .count();
        assert_eq!(invokes, 2, "both agent calls join as artifacts");

        // …and both results joined into the program's returned array.
        let result = tree
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::ProgramResult { value } => Some(value.clone()),
                _ => None,
            })
            .expect("a ProgramResult");
        let joined: HashSet<String> = result
            .as_array()
            .expect("an array result")
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            joined,
            HashSet::from(["done: A".to_owned(), "done: B".to_owned()])
        );
    }

    /// Records peak concurrent `complete()` calls, sleeping inside the
    /// call so overlapping requests are caught in the act.
    struct ConcurrencyProbe {
        inner: ScriptedLlm,
        inflight: Arc<Mutex<usize>>,
        peak: Arc<Mutex<usize>>,
    }

    impl LlmClient for ConcurrencyProbe {
        fn complete(
            &self,
            request: &LlmRequest,
            chunk: &mut dyn FnMut(LlmChunk),
        ) -> Result<Message, String> {
            {
                let mut n = self.inflight.lock().unwrap();
                *n += 1;
                let mut p = self.peak.lock().unwrap();
                *p = (*p).max(*n);
            }
            thread::sleep(Duration::from_millis(50));
            let result = self.inner.complete(request, chunk);
            *self.inflight.lock().unwrap() -= 1;
            result
        }
    }

    /// Two subagents spawned by one `Promise.all` think *concurrently*:
    /// their `complete()` calls overlap (peak in-flight ≥ 2), not
    /// serialized behind a single client lock.
    #[test]
    fn subagent_completions_run_concurrently() {
        let peak = Arc::new(Mutex::new(0usize));
        let llm = ConcurrencyProbe {
            inner: ScriptedLlm::new(vec![
                scripted_program(
                    "c1",
                    r#"return await Promise.all([
                        tools.agent({ prompt: "A", input: null }),
                        tools.agent({ prompt: "B", input: null }),
                    ]);"#,
                ),
                scripted_text("child one"),
                scripted_text("child two"),
                scripted_text("both back"),
            ]),
            inflight: Arc::new(Mutex::new(0)),
            peak: Arc::clone(&peak),
        };
        let (tx, _rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "test agent",
            json!(null),
            ToolRegistry::new(),
            Box::new(llm),
            tx,
        )
        .unwrap();
        session
            .handle()
            .send(SessionCommand::UserTurn("delegate two".into()));
        session.run();

        assert!(
            *peak.lock().unwrap() >= 2,
            "two subagents should think at once; peak in-flight was {}",
            *peak.lock().unwrap()
        );
    }

    /// The semaphore bounds concurrency to its permit count: cap=1
    /// serializes (peak 1), cap=3 lets three of six workers overlap.
    /// Deterministic and env-free (the integration proof above uses the
    /// default cap; this pins the mechanism the `AGENT2_LLM_CONCURRENCY`
    /// override feeds).
    #[test]
    fn semaphore_bounds_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for (cap, expected_peak) in [(1usize, 1usize), (3, 3)] {
            let sem = Arc::new(Semaphore::new(cap));
            let inflight = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let handles: Vec<_> = (0..6)
                .map(|_| {
                    let sem = Arc::clone(&sem);
                    let inflight = Arc::clone(&inflight);
                    let peak = Arc::clone(&peak);
                    thread::spawn(move || {
                        let _permit = sem.acquire();
                        let n = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(n, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        inflight.fetch_sub(1, Ordering::SeqCst);
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(peak.load(Ordering::SeqCst), expected_peak, "cap {cap}");
        }
    }

    #[test]
    fn hot_loop_keeps_the_inbox_responsive() {
        let (tx, _rx) = channel();
        let mut session = Session::new(
            Tree::new(None),
            "test agent",
            json!(null),
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_program(
                "c1",
                "while (true) {}",
            )])),
            tx,
        )
        .unwrap();
        let handle = session.handle();
        handle.send(SessionCommand::UserTurn("spin forever".into()));

        // The program never finishes; the loop keeps taking fuel-slice
        // continues without ever blocking inside the VM.
        for _ in 0..20 {
            assert!(session.pump_one());
        }
        // A Shutdown queued behind the pending Continue is reached
        // within a slice, not starved.
        handle.send(SessionCommand::Shutdown);
        let mut hops = 0;
        while session.pump_one() {
            hops += 1;
            assert!(hops < 5, "Shutdown starved by a hot program");
        }
    }

    #[test]
    fn user_turn_while_busy_is_rejected_not_panicked() {
        let (tx, rx) = channel();
        let mut session = Session::new(
            Tree::new(None),
            "test agent",
            json!(null),
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_program(
                "c1",
                "while (true) {}",
            )])),
            tx,
        )
        .unwrap();
        let handle = session.handle();
        handle.send(SessionCommand::UserTurn("go".into()));
        for _ in 0..10 {
            assert!(session.pump_one());
        }
        handle.send(SessionCommand::UserTurn("are you done yet?".into()));
        for _ in 0..5 {
            assert!(session.pump_one());
        }
        let saw_rejection = rx
            .try_iter()
            .any(|e| matches!(&e, SessionEvent::Error { message, .. } if message.contains("busy")));
        assert!(saw_rejection);
        handle.send(SessionCommand::Shutdown);
        while session.pump_one() {}
    }

    // --- M4: fork / label / resume / list-leaves ---

    fn user(text: &str) -> EventPayload {
        EventPayload::Message(Message::User { text: text.into() })
    }

    fn assistant(text: &str) -> EventPayload {
        EventPayload::Message(Message::Assistant {
            text: text.into(),
            thinking: None,
            tool_calls: Vec::new(),
        })
    }

    /// An incomplete root agent: Agent(1), User(2 "q"),
    /// Assistant(3 "a1"). Leaf = #3 — open, so resumable and forkable.
    fn tree_with_open_root() -> Tree {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null)).unwrap();
        tree.append(&mut spine, user("q")).unwrap();
        tree.append(&mut spine, assistant("a1")).unwrap();
        tree
    }

    fn open(tree: Tree, script: Vec<Message>) -> (Session, Receiver<SessionEvent>) {
        let (tx, rx) = channel();
        let session = Session::new(
            tree,
            "ignored on resume",
            json!(null),
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new(script)),
            tx,
        )
        .unwrap();
        (session, rx)
    }

    fn drain(mut session: Session) -> Session {
        while session.pump_one() {}
        session
    }

    fn last_leaves(events: &[SessionEvent]) -> Vec<LeafInfo> {
        events
            .iter()
            .rev()
            .find_map(|e| match e {
                SessionEvent::Leaves(l) => Some(l.clone()),
                _ => None,
            })
            .expect("a Leaves event")
    }

    fn errors(events: &[SessionEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn list_leaves_reports_the_active_root() {
        let (session, rx) = open(tree_with_open_root(), vec![]);
        session.handle().send(SessionCommand::ListLeaves);
        session.handle().send(SessionCommand::Shutdown);
        let _ = drain(session);

        let leaves = last_leaves(&rx.try_iter().collect::<Vec<_>>());
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].leaf, EventId::new(3));
        assert_eq!(leaves[0].agent, EventId::new(1));
        assert!(leaves[0].active && !leaves[0].complete);
        assert_eq!(leaves[0].summary, "Assistant: a1");
    }

    #[test]
    fn label_logs_on_the_active_leaf_and_surfaces() {
        let (session, rx) = open(tree_with_open_root(), vec![]);
        let h = session.handle();
        h.send(SessionCommand::Label("my-branch".into()));
        h.send(SessionCommand::Shutdown);
        let session = drain(session);

        // A Label event was logged on the root spine.
        assert!(
            session
                .tree()
                .events
                .values()
                .any(|e| matches!(&e.payload, EventPayload::Label(l) if l == "my-branch"))
        );
        // …and the refreshed leaf list carries it as the active leaf's label.
        let leaves = last_leaves(&rx.try_iter().collect::<Vec<_>>());
        assert_eq!(leaves.len(), 1);
        assert!(leaves[0].active);
        assert_eq!(leaves[0].label.as_deref(), Some("my-branch"));
    }

    #[test]
    fn fork_then_user_turn_diverges_in_the_same_agent() {
        let (session, rx) = open(tree_with_open_root(), vec![scripted_text("forked done")]);
        let h = session.handle();
        // Fork off the user message (#2), dropping the original a1 reply.
        h.send(SessionCommand::Fork {
            from: EventId::new(2),
            label: Some("retry".into()),
        });
        h.send(SessionCommand::UserTurn("forked follow-up".into()));
        let session = drain(session); // forked agent yields the turn back
        let tree = session.tree();

        // Two leaves, both under the root agent (Agent #1).
        let leaves = tree.list_leaves();
        assert_eq!(leaves.len(), 2);
        for (leaf, _) in &leaves {
            assert_eq!(agent_root_of(tree, *leaf), EventId::new(1));
        }
        // The original assistant leaf (#3) survived untouched.
        assert!(leaves.iter().any(|(id, _)| *id == EventId::new(3)));
        // The forked branch diverged off #2 (never saw "a1"). It is the
        // root conversation, so it yields rather than completing — no
        // `FrameResult`, the spine stays open.
        let forked_leaf = leaves
            .iter()
            .map(|(id, _)| *id)
            .find(|id| *id != EventId::new(3))
            .unwrap();
        let forked = tree.spine_at(forked_leaf);
        assert!(!forked.is_complete());
        assert!(session.is_awaiting_user());
        // Chat messages, minus the materialized system prompt (decision 4;
        // this legacy tree had none, so it's inserted on the first turn).
        let msgs: Vec<&str> = forked
            .context()
            .messages
            .iter()
            .filter(|m| !matches!(m, Message::System { .. }))
            .map(|m| m.text())
            .collect();
        assert_eq!(msgs, ["q", "forked follow-up", "forked done"]);
        // The fork's label sits on the new branch, not the original.
        assert_eq!(
            last_leaves(&rx.try_iter().collect::<Vec<_>>())
                .iter()
                .find(|l| l.label.is_some())
                .and_then(|l| l.label.clone()),
            Some("retry".into())
        );
    }

    #[test]
    fn resume_switches_root_and_rejects_complete_and_unknown() {
        // Open root (#3) plus a completed sibling branch forked off #2.
        let mut tree = tree_with_open_root();
        let mut branch = tree.fork(EventId::new(2)).unwrap();
        tree.append(&mut branch, user("other")).unwrap();
        let complete_leaf = tree
            .append(
                &mut branch,
                EventPayload::FrameResult { result: json!("x") },
            )
            .unwrap();

        let (session, rx) = open(tree, vec![]);
        let h = session.handle();
        h.send(SessionCommand::Resume(EventId::new(3))); // open branch — ok
        h.send(SessionCommand::Resume(complete_leaf)); // completed — rejected
        h.send(SessionCommand::Resume(EventId::new(99))); // unknown — rejected
        h.send(SessionCommand::Shutdown);
        let session = drain(session);

        let events: Vec<SessionEvent> = rx.try_iter().collect();
        let errs = errors(&events);
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs.iter().any(|m| m.contains("completed spine")));
        assert!(errs.iter().any(|m| m.contains("not in the log")));
        // The one accepted resume left the active root on the open leaf.
        assert_eq!(
            session.state(session.root()).unwrap().spine.leaf_id,
            EventId::new(3)
        );
    }

    #[test]
    fn mutating_commands_are_rejected_while_the_agent_is_busy() {
        let (mut session, rx) = open(
            tree_with_open_root(),
            vec![scripted_program("c1", "while (true) {}")],
        );
        let h = session.handle();
        h.send(SessionCommand::UserTurn("spin".into()));
        for _ in 0..6 {
            session.pump_one();
        }
        // The agent is now Running; every mutating command bounces.
        h.send(SessionCommand::Label("late".into()));
        h.send(SessionCommand::Fork {
            from: EventId::new(2),
            label: None,
        });
        h.send(SessionCommand::Resume(EventId::new(2)));
        for _ in 0..6 {
            session.pump_one();
        }
        let busy = rx
            .try_iter()
            .filter(
                |e| matches!(e, SessionEvent::Error { message, .. } if message.contains("busy")),
            )
            .count();
        assert_eq!(busy, 3, "label/fork/resume each rejected while busy");
        h.send(SessionCommand::Shutdown);
        while session.pump_one() {}
    }

    #[test]
    fn open_at_anchors_the_chosen_leaf_not_the_auto_pick() {
        // Two open leaves: #3 (auto-pick) and a second forked branch.
        let mut tree = tree_with_open_root();
        let mut branch = tree.fork(EventId::new(2)).unwrap();
        let other_leaf = tree.append(&mut branch, assistant("branch2")).unwrap();

        let (tx, _rx) = channel();
        let session = Session::open_at(
            tree,
            other_leaf,
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new(vec![])),
            tx,
        )
        .unwrap();
        // `new` would auto-pick #3; `open_at` honours the chosen leaf.
        assert_eq!(
            session.state(session.root()).unwrap().spine.leaf_id,
            other_leaf
        );

        let (tx, _rx) = channel();
        let result = Session::open_at(
            tree_with_open_root(),
            EventId::new(99),
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new(vec![])),
            tx,
        );
        match result {
            Ok(_) => panic!("open_at on an unknown id must error"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        }
    }

    #[test]
    fn interrupted_run_program_synthesizes_report_and_rewrite_continues() {
        // Build a tree with an unanswered run_program: the program was
        // interrupted before completing. Event ids are deterministic:
        // Agent 1, User 2, Assistant 3 (run_program, no result).
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, "you are an agent", json!(null))
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Message(Message::User {
                text: "do something".into(),
            }),
        )
        .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Assistant {
                text: String::new(),
                thinking: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "run_program".into(),
                    arguments: json!({ "source": "return 42;" }),
                }],
            }),
        )
        .unwrap();
        // No Tool/ProgramResult/FrameResult — the VM was lost.

        // Open the log; pick_resume_leaf finds leaf #3.
        let (tx, rx) = channel();
        let session = Session::new(
            tree,
            "ignored",
            json!(null),
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new(vec![
                scripted_program("c2", "return 999;"),
                scripted_text("final"),
            ])),
            tx,
        )
        .expect("opens the interrupted log");

        let tools = tool_texts(&session);
        let interrupted_report = tools
            .first()
            .expect("a synthesized tool result for the interrupted run_program");
        assert!(
            interrupted_report.contains("program interrupted"),
            "{interrupted_report}"
        );
        assert!(
            interrupted_report.contains("fetchable by id"),
            "{interrupted_report}"
        );
        assert!(
            interrupted_report.contains("restarts"),
            "{interrupted_report}"
        );

        // Now send a user turn; the LLM sees the interrupted report and
        // responds with a run_program rewrite.
        session
            .handle()
            .send(SessionCommand::UserTurn("continue".into()));
        let session = session.run();

        // The rewrite should have produced a completion report, then a
        // final text turn that yields the root's turn back to the user
        // (no `FrameResult` — the top conversation never ends).
        let all_tools = tool_texts(&session);
        assert!(
            all_tools.iter().any(|t| t.contains("program completed")),
            "rewrite completed: {all_tools:?}"
        );
        let kinds = kinds(session.tree(), root_leaf(&session));
        assert!(
            kinds.last() == Some(&"Assistant"),
            "agent yielded its final answer: {kinds:?}"
        );
        assert!(session.is_awaiting_user());

        let _events: Vec<SessionEvent> = rx.try_iter().collect();
    }

    #[test]
    fn all_complete_log_opens_idle_for_fork() {
        // A fully completed single-agent log: previously `new` errored.
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, "root", json!(null)).unwrap();
        tree.append(&mut spine, assistant("done")).unwrap();
        tree.append(&mut spine, EventPayload::FrameResult { result: json!(1) })
            .unwrap();

        let (tx, _rx) = channel();
        let session = Session::new(
            tree,
            "ignored",
            json!(null),
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new(vec![])),
            tx,
        )
        .expect("an all-complete log opens idle, not an error");
        assert_eq!(session.root(), EventId::new(1));
        let state = session.state(session.root()).unwrap();
        assert!(state.is_idle() && state.spine.is_complete());
    }
}
