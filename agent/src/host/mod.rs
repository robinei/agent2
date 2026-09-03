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

use crate::machine::{
    LlmRequest, LlmTurn, OutCall, Runner, SpawnRequest, StepInput, StepOutput, ToolResult,
};
use crate::types::{
    Address, Author, Call, Cause, EventId, EventPayload, Message, Origin, Outcome, Spine, Tree,
};

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
        result: Result<LlmTurn, String>,
    },
    ToolDone {
        agent: AgentId,
        call: EventId,
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
        charter: &str,
        registry: ToolRegistry,
        llm: Box<dyn LlmClient>,
        events: Sender<SessionEvent>,
    ) -> io::Result<Self> {
        if tree.events.is_empty() {
            let emitted = tree.id_counter;
            let state = Runner::new_root(&mut tree, charter, &dialect_card(&registry))?;
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
        let state = Runner::with_spine(&tree, tree.spine_at(leaf));
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
        // Opening is a step of its own: the root `Agent`, or a repair
        // appended by reconciliation, is durable before the loop runs.
        session.tree.sync()?;
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
        let _ = self
            .step_agent(agent, StepInput::Tick { fuel })
            .and_then(|()| self.tree.sync());
    }

    fn on_msg(&mut self, msg: LoopMsg) {
        // One inbox message is one loop step, and the log syncs once at
        // the end of it — never once per event, which a fan-out turn
        // would make hundreds of fsyncs on the loop thread.
        let stepped = self.dispatch(msg).and_then(|()| self.tree.sync());
        if let Err(e) = stepped {
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
            LoopMsg::Command(SessionCommand::UserTurn { branch, text }) => {
                let root = branch;
                let Some(state) = self.states.get(&root) else {
                    self.emit(SessionEvent::Error {
                        agent: Some(branch),
                        message: format!("no live branch {}", branch.as_u64()),
                    });
                    return Ok(());
                };
                // **Nothing you say is ever rejected.** A post is logged
                // on arrival in every phase and delivered at the
                // recipient's next safe point — for a running program,
                // its next fuel slice (rule B).
                let _ = state;
                self.awaiting_user = false; // the user took their turn
                self.deliver_post(
                    root,
                    Author::User,
                    Origin::Direct {
                        text,
                        input: serde_json::Value::Null,
                        expects_reply: true,
                    },
                )
                .map(|_| ())
            }
            LoopMsg::Command(SessionCommand::Reply {
                branch,
                call,
                value,
            }) => self.cmd_reply(branch, call, value),
            LoopMsg::Command(SessionCommand::ListLeaves) => {
                let leaves = self.leaf_infos();
                self.emit(SessionEvent::Leaves(leaves));
                Ok(())
            }
            LoopMsg::Command(SessionCommand::Rename(name)) => self.cmd_rename(name),
            LoopMsg::Command(SessionCommand::Fork { from, name }) => self.cmd_fork(from, name),
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
                    // A dead branch fails every call waiting on it.
                    let owed = self.owed_by(agent);
                    if owed.is_empty() {
                        self.done = true;
                    }
                    for (asker, send) in owed {
                        let _ = self.tx.send(LoopMsg::ToolDone {
                            agent: asker,
                            call: send,
                            result: Err(format!("subagent failed: {message}")),
                        });
                    }
                    Ok(())
                }
            },
            LoopMsg::ToolDone {
                agent,
                call,
                result,
            } => self.step_agent(
                agent,
                StepInput::ToolResults(vec![ToolResult { call, result }]),
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

    /// Name the active branch. A `Rename` is a **record**, not a message,
    /// so it changes the navigator and wakes nothing.
    fn cmd_rename(&mut self, name: String) -> io::Result<()> {
        let Some(root) = self.idle_root() else {
            return Ok(());
        };
        let state = self.states.get_mut(&root).expect("idle_root checked");
        self.tree
            .append(&mut state.spine, EventPayload::Rename { name })?;
        self.emit_new(root);
        let leaves = self.leaf_infos();
        self.emit(SessionEvent::Leaves(leaves));
        Ok(())
    }

    fn cmd_fork(&mut self, from: EventId, name: Option<String>) -> io::Result<()> {
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
        // A `Fork` roots the divergent branch: history and artifacts
        // cross it, obligations do not.
        self.tree.append(&mut spine, EventPayload::Fork { name })?;
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
        let mut state = Runner::with_spine(&self.tree, spine);
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
            .map(|(leaf, name)| LeafInfo {
                leaf,
                agent: agent_root_of(&self.tree, leaf),
                name,
                open: self.tree.spine_at(leaf).context().open.len(),
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
                StepOutput::Spawns(spawns) => {
                    for spawn in spawns {
                        self.create_agent(agent, spawn)?;
                    }
                }
                StepOutput::Sends(sends) => {
                    for send in sends {
                        self.deliver_send(agent, send)?;
                    }
                }
                // A branch answered and went idle. **The branch is the
                // address**: where the answer goes is decided by who
                // asked, which is a fact on the post itself — not by any
                // flag on the branch.
                StepOutput::Answered { question, value } => {
                    self.route_answer(agent, question, value)?
                }
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
            // `agents` is the one tool the registry cannot serve: its
            // answer is a projection over the tree **plus live session
            // state** (a branch's status), which no `ToolHandler` can
            // see. Answered inline, on the loop thread — it reads memory.
            if call.name == crate::machine::TOOL_AGENTS {
                let _ = self.tx.send(LoopMsg::ToolDone {
                    agent,
                    call: call.call,
                    result: self.serve_agents(agent, &call.args),
                });
                continue;
            }
            if let Err(refused) = self.check_allowlist(agent, &call.name) {
                let _ = self.tx.send(LoopMsg::ToolDone {
                    agent,
                    call: call.call,
                    result: Err(refused),
                });
                continue;
            }
            match self.registry.get(&call.name) {
                Some(def) => {
                    let def = Arc::clone(def);
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let result = guard_size((def.handler)(call.args));
                        let _ = tx.send(LoopMsg::ToolDone {
                            agent,
                            call: call.call,
                            result,
                        });
                    });
                }
                None => {
                    let _ = self.tx.send(LoopMsg::ToolDone {
                        agent,
                        call: call.call,
                        result: Err(format!("unknown tool `{}`", call.name)),
                    });
                }
            }
        }
    }

    /// An agent's tool allowlist, read from its **own `Agent` root** —
    /// not from the `Spawn` on its parent's branch, which is why the root
    /// agent (which has no `Spawn`) is not a special case. `None` is
    /// "everything the registry has".
    fn allowlist(&self, agent: AgentId) -> Option<Vec<String>> {
        match self.tree.events.get(&agent).map(|e| &e.payload) {
            Some(EventPayload::Agent { tools, .. }) => tools.clone(),
            _ => None,
        }
    }

    /// Enforce the allowlist at the call. A narrowed agent asking for a
    /// tool it does not have gets a `Failed` result naming what it does
    /// have — a value the program may catch, a condition only if it does
    /// not (6_LANGUAGE Part B).
    fn check_allowlist(&self, agent: AgentId, name: &str) -> Result<(), String> {
        match self.allowlist(agent) {
            Some(allowed) if !allowed.iter().any(|t| t == name) => Err(format!(
                "tool `{name}` is not in this agent's allowlist (have: {})",
                allowed.join(", ")
            )),
            _ => Ok(()),
        }
    }

    /// Serve one `Spawn`: root an `Agent` under the call and settle the
    /// caller with `{ agent }`.
    ///
    /// The two events are the two ends of one act — `Spawn` is the
    /// caller's request, settled by a `Result`; `Agent` is the agent's
    /// own root and outlives the caller, its program, and often the
    /// conversation that created it. Nothing is asked here: a spawned
    /// agent is idle with nothing open, so the driving rule leaves it
    /// silent until someone speaks to it.
    fn create_agent(&mut self, parent: AgentId, spawn: SpawnRequest) -> io::Result<()> {
        let SpawnRequest { call, budget } = spawn;
        // `name`/`charter`/`tools` live on the logged `Spawn`; the host
        // reads them there rather than being handed a copy.
        let Some(EventPayload::Call(Call::Spawn {
            name,
            charter,
            tools,
            ..
        })) = self.tree.events.get(&call).map(|e| &e.payload)
        else {
            unreachable!("a SpawnRequest names its logged Spawn");
        };
        let (name, charter) = (name.clone(), charter.clone());
        // `tools` **narrows**: a child can never widen past its parent's
        // allowlist, so an intersection is the only honest reading of
        // "default: yours".
        let tools = match (self.allowlist(parent), tools.clone()) {
            (None, child) => child,
            (Some(parent_tools), None) => Some(parent_tools),
            (Some(parent_tools), Some(child)) => Some(
                child
                    .into_iter()
                    .filter(|t| parent_tools.contains(t))
                    .collect(),
            ),
        };
        let card = match &tools {
            Some(allowed) => dialect_card(&self.registry.narrowed(allowed)),
            None => dialect_card(&self.registry),
        };
        let child = Runner::new_agent(&mut self.tree, call, name, charter, tools, budget, &card)?;
        let child_id = child.agent_id();
        self.states.insert(child_id, child);
        self.emit_new(child_id);
        let _ = self.tx.send(LoopMsg::ToolDone {
            agent: parent,
            call,
            result: Ok(serde_json::json!({ "agent": child_id.as_u64() })),
        });
        Ok(())
    }

    /// Deliver one `Send` — rule A, the other half of the exchange the
    /// asker already logged. The body, address and `expects_reply` are
    /// read from the `Send` itself: neither side copies the other.
    ///
    /// - to an agent: a `Post` naming this `Send` lands on that branch.
    ///   An `ask` then waits for its `Answer`; a `tell` is settled by its
    ///   delivery receipt in the same step, so the recipient owes nothing.
    /// - to the human: **no `Post` anywhere** — they have no branch to
    ///   post into. An `ask` stays pending until `Reply` settles it; a
    ///   `tell` is a receipt with no post to name.
    fn deliver_send(&mut self, sender: AgentId, send: EventId) -> io::Result<()> {
        let Some(EventPayload::Call(Call::Send {
            to, expects_reply, ..
        })) = self.tree.events.get(&send).map(|e| &e.payload)
        else {
            unreachable!("a Sends output names its logged Send");
        };
        let (to, expects_reply) = (*to, *expects_reply);
        let branch = match to {
            Address::User => {
                if !expects_reply {
                    let _ = self.tx.send(LoopMsg::ToolDone {
                        agent: sender,
                        call: send,
                        result: Ok(serde_json::json!({ "post": serde_json::Value::Null })),
                    });
                }
                return Ok(());
            }
            Address::Branch(branch) => branch,
        };
        if !self.states.contains_key(&branch) {
            let _ = self.tx.send(LoopMsg::ToolDone {
                agent: sender,
                call: send,
                result: Err(format!(
                    "branch #{} is not live in this session",
                    branch.as_u64()
                )),
            });
            return Ok(());
        }
        let post = self.deliver_post(branch, Author::Agent(sender), Origin::Sent(send))?;
        // A tell resolves as soon as its post lands: what a tell spares
        // is the answer, not the attention.
        if !expects_reply {
            let _ = self.tx.send(LoopMsg::ToolDone {
                agent: sender,
                call: send,
                result: Ok(serde_json::json!({ "post": post.map(|p| p.as_u64()) })),
            });
        }
        Ok(())
    }

    /// Append a `Post` to `branch` and run whatever it wants to do next.
    /// Returns the `Post`'s id — a `tell`'s receipt names it.
    fn deliver_post(
        &mut self,
        branch: AgentId,
        from: Author,
        origin: Origin,
    ) -> io::Result<Option<EventId>> {
        let Some(state) = self.states.get_mut(&branch) else {
            return Ok(None);
        };
        let (post, outputs) = state.deliver(&mut self.tree, from, origin)?;
        let transitions = state.take_status_transitions();
        self.emit_new(branch);
        for (program, status) in transitions {
            self.emit(SessionEvent::ProgramStatus {
                agent: branch,
                program,
                status,
            });
        }
        self.process(branch, outputs)?;
        Ok(Some(post))
    }

    /// Settle a pending `Send { to: user }` with the human's reply. The
    /// user has no branch, so their side of the exchange is only this:
    /// the `Result` lands on the branch that asked.
    fn cmd_reply(
        &mut self,
        branch: AgentId,
        call: EventId,
        value: serde_json::Value,
    ) -> io::Result<()> {
        let pending = matches!(
            self.tree.events.get(&call).map(|e| &e.payload),
            Some(EventPayload::Call(Call::Send {
                to: Address::User,
                expects_reply: true,
                ..
            }))
        );
        if !pending {
            self.emit(SessionEvent::Error {
                agent: Some(branch),
                message: format!("#{} is not a question to you", call.as_u64()),
            });
            return Ok(());
        }
        let Some(state) = self.states.get_mut(&branch) else {
            return Ok(());
        };
        self.tree.append(
            &mut state.spine,
            EventPayload::Result {
                call,
                outcome: Outcome::Delivered(value),
            },
        )?;
        self.emit_new(branch);
        Ok(())
    }

    /// The exchanges `branch` still owes: `(asker, send)` for every open
    /// post on it that names a `Send`. Read from the log — no wait table:
    /// the four events form a closed loop of ids, so the asker and the
    /// call to settle are both one lookup from the post.
    fn owed_by(&self, branch: AgentId) -> Vec<(AgentId, EventId)> {
        let Some(state) = self.states.get(&branch) else {
            return Vec::new();
        };
        state
            .open()
            .iter()
            .filter_map(|post| self.asking_branch(*post))
            .collect()
    }

    /// Where an answer to `post` goes: the `Send` it names and the branch
    /// that `Send` sits on. `None` for a post with no send side — the
    /// user's or the harness's — which is read inline instead.
    ///
    /// This is the closed loop of ids walked in one direction:
    /// `Answer.question → Post`, `Post.origin → Send`, and the `Send`'s
    /// position **is** the asker's branch. Nothing session-local is
    /// consulted, so it survives a reopen as-is.
    fn asking_branch(&self, post: EventId) -> Option<(AgentId, EventId)> {
        let Some(EventPayload::Message(Message::Post {
            origin: Origin::Sent(send),
            ..
        })) = self.tree.events.get(&post).map(|e| &e.payload)
        else {
            return None;
        };
        let send = *send;
        let asker = self.tree.enclosing_agent(send)?;
        Some((asker, send))
    }

    /// Deliver a branch's answer. Rule A: the answer is logged on the
    /// branch that produced it (already done); *this* is the other half —
    /// a `Result` on the branch that asked, or nothing at all when the
    /// asker was the human, who reads the answer inline where it sits.
    fn route_answer(
        &mut self,
        branch: AgentId,
        question: Option<EventId>,
        value: serde_json::Value,
    ) -> io::Result<()> {
        let Some(question) = question else {
            // Nothing was owed: the branch is simply idle now.
            self.awaiting_user = true;
            return Ok(());
        };
        match self.asking_branch(question) {
            // An agent asked: its `Send` settles on its own branch.
            Some((asker, send)) => {
                let _ = self.tx.send(LoopMsg::ToolDone {
                    agent: asker,
                    call: send,
                    result: guard_size(Ok(value)),
                });
            }
            // The user asked. They have no branch and no program, so
            // there is nothing to settle — the answer is read inline.
            None => {
                self.awaiting_user = true;
                self.emit(SessionEvent::Answered {
                    agent: branch,
                    question,
                    value,
                });
            }
        }
        Ok(())
    }

    /// Serve `tools.agents({ under?, deep? })`: **discovery**, the one
    /// addition that makes long-running orchestration possible. Agents
    /// outlive programs but a program's handles to them do not, so the
    /// next program re-discovers its workers by query rather than by
    /// memory.
    ///
    /// One row per **branch**, so a forked worker lists twice, sharing
    /// its `agent`. Nondeterministic by construction — `status` is live
    /// session state — which is fine: it is a tool result, never a
    /// rendered message.
    fn serve_agents(
        &self,
        caller: AgentId,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let arg = args.get(0).cloned().unwrap_or(serde_json::Value::Null);
        let under = match arg.get("under").filter(|v| !v.is_null()) {
            Some(v) => v
                .as_u64()
                .filter(|n| *n > 0)
                .map(EventId::new)
                .ok_or_else(|| format!("`under` must be an agent id; got {v}"))?,
            None => caller,
        };
        let deep = arg.get("deep").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut rows = Vec::new();
        for (branch, leaf) in self.tree.branches() {
            let Some(agent) = self.tree.enclosing_agent(branch) else {
                continue;
            };
            if !self.is_under(agent, under, deep) {
                continue;
            }
            let EventPayload::Agent { charter, .. } = &self.tree.events[&agent].payload else {
                continue;
            };
            let last_answer = self
                .tree
                .path_events(leaf)
                .iter()
                .rev()
                .find(|e| matches!(e.payload, EventPayload::Answer { .. }))
                .map(|e| e.id.as_u64());
            rows.push(serde_json::json!({
                "agent": agent.as_u64(),
                "branch": branch.as_u64(),
                "name": self.tree.branch_name(leaf),
                "charter": charter,
                "parent": self.tree.events[&agent]
                    .parent_id
                    .and_then(|p| self.tree.enclosing_agent(p))
                    .map(|a| a.as_u64()),
                "status": self.branch_status(branch),
                "open": self.tree.spine_at(leaf).context().open.len(),
                "last_answer": last_answer,
            }));
        }
        Ok(serde_json::Value::Array(rows))
    }

    /// Whether `agent` is a direct child of `under` — or anywhere in its
    /// subtree with `deep`. `under` itself is never a row: `agents()`
    /// answers "who works for me", not "who am I".
    fn is_under(&self, agent: AgentId, under: AgentId, deep: bool) -> bool {
        if agent == under {
            return false;
        }
        let mut current = self.tree.events.get(&agent).and_then(|e| e.parent_id);
        while let Some(cur) = current {
            let Some(parent) = self.tree.enclosing_agent(cur) else {
                return false;
            };
            if parent == under {
                return true;
            }
            if !deep {
                return false;
            }
            current = self.tree.events.get(&parent).and_then(|e| e.parent_id);
        }
        false
    }

    /// A branch's live status word. `dormant` is a branch with no runner
    /// in this session — nothing is lost, it is re-hydrated when spoken
    /// to (C1/C2); `thinking` is the model-facing name for awaiting-LLM.
    fn branch_status(&self, branch: EventId) -> &'static str {
        match self.states.get(&branch).map(|s| s.status()) {
            Some("awaiting llm") => "thinking",
            Some(word) => word,
            None => "dormant",
        }
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

/// A `Turn(run_program)` with **no outcome** was interrupted mid-program
/// and its VM is gone. Append `Condition{Interrupted}` so the run has an
/// outcome like any other and its report renders from the log — the one
/// repair the reconciliation table needs for this row.
///
/// A turn that *does* have an outcome needs nothing: the report renders
/// from it, so "the report was lost" is not a case that can exist.
/// Returns the (possibly updated) leaf id.
fn synthesize_if_interrupted(tree: &mut Tree, leaf: EventId) -> io::Result<EventId> {
    let path = tree.path_events(leaf);
    let Some(turn) = path
        .iter()
        .rev()
        .find(|e| matches!(e.payload, EventPayload::Message(Message::Turn { .. })))
    else {
        return Ok(leaf);
    };
    let EventPayload::Message(Message::Turn { tool_calls, .. }) = &turn.payload else {
        return Ok(leaf);
    };
    if tool_calls.is_empty() {
        return Ok(leaf); // a turn with no calls is idle, not interrupted
    }
    let turn_id = turn.id;
    if !crate::report::outcomes_of_turn(tree, leaf, turn_id).is_empty() {
        return Ok(leaf);
    }

    let mut spine = tree.spine_at(leaf);
    tree.append(
        &mut spine,
        EventPayload::Condition {
            cause: Cause::Interrupted,
            site: 0,
            stack: Vec::new(),
        },
    )
}

/// Auto-pick a resume anchor for a re-opened log: the lowest-id leaf
/// that **owes something** — an open post, or a `run_program` turn with
/// no outcome — else the lowest-id leaf, so the loop still lives for
/// `ListLeaves`/`Fork`/`Resume`.
///
/// Nothing is "complete" any more (agents never close), so the question
/// is no longer "which branch is unfinished" but "which branch has work
/// waiting on it".
fn pick_resume_leaf(tree: &Tree) -> io::Result<EventId> {
    let mut leaves: Vec<EventId> = tree.list_leaves().into_iter().map(|(id, _)| id).collect();
    leaves.sort_by_key(|id| id.as_u64());
    leaves
        .iter()
        .copied()
        .find(|id| owes_work(tree, *id))
        .or_else(|| leaves.first().copied())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "log has no leaves"))
}

/// Whether a branch has work waiting on it: a post it has not answered,
/// or a turn whose calls never produced an outcome.
fn owes_work(tree: &Tree, leaf: EventId) -> bool {
    if !tree.spine_at(leaf).context().open.is_empty() {
        return true;
    }
    tree.path_events(leaf)
        .iter()
        .rev()
        .find(|e| matches!(e.payload, EventPayload::Message(Message::Turn { .. })))
        .is_some_and(|turn| {
            let EventPayload::Message(Message::Turn { tool_calls, .. }) = &turn.payload else {
                return false;
            };
            !tool_calls.is_empty()
                && crate::report::outcomes_of_turn(tree, leaf, turn.id).len() < tool_calls.len()
        })
}

/// One-word label for a logged condition's cause.
fn cause_label(cause: &Cause) -> &'static str {
    match cause {
        Cause::Raised { .. } => "raised",
        Cause::Trapped { .. } => "trapped",
        Cause::Posted { .. } => "posted",
        Cause::CompileFailed { .. } => "compile failed",
        Cause::Refused { .. } => "refused",
        Cause::Interrupted => "interrupted",
    }
}

/// One-line preview of an event for the leaf list, clipped to the
/// report preview bound.
fn leaf_summary(tree: &Tree, leaf: EventId) -> String {
    let Some(event) = tree.events.get(&leaf) else {
        return String::new();
    };
    let s = match &event.payload {
        EventPayload::Agent { charter, .. } => format!("Agent: {charter}"),
        EventPayload::Fork { name } => {
            format!("Fork: {}", name.as_deref().unwrap_or("<unnamed>"))
        }
        EventPayload::Answer { question, value } => {
            format!("Answer to #{}: {value}", question.as_u64())
        }
        EventPayload::Message(Message::Post { from, origin }) => {
            format!("Post: {}", crate::report::render_post(*from, origin))
        }
        EventPayload::Message(Message::Turn {
            text, tool_calls, ..
        }) => {
            if tool_calls.is_empty() {
                format!("Turn: {text}")
            } else {
                let names: Vec<&str> = tool_calls.iter().map(|c| c.name.as_str()).collect();
                format!("Turn: ⚙ {}", names.join(", "))
            }
        }
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
        EventPayload::Return { value } => format!("Return: {value}"),
        EventPayload::Condition { cause, .. } => format!("Condition: {}", cause_label(cause)),
        EventPayload::Rename { name } => format!("Rename: {name}"),
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

    use crate::types::{Author, Origin, ToolCall};

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
        script: Vec<LlmTurn>,
        user_turn: &str,
    ) -> (Session, Vec<SessionEvent>) {
        let (tx, rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "test agent",
            registry,
            Box::new(ScriptedLlm::new(script)),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.root(),
            text: user_turn.into(),
        });
        let session = session.run();
        let events = rx.try_iter().collect();
        (session, events)
    }

    /// Build a session, send one user turn, and drive the loop until it
    /// goes momentarily quiet.
    ///
    /// Not `run()`: that stops the moment **any** branch answers and owes
    /// nothing, which from B1 on includes a worker going idle while the
    /// orchestrator is still running. It also keeps going where `run()`
    /// would block forever — a branch still waiting on the human, which
    /// is a legitimate end state for several of these tests.
    fn run_routed(
        registry: ToolRegistry,
        rules: impl IntoIterator<Item = (&'static str, Vec<LlmTurn>)>,
        user_turn: &str,
    ) -> (Session, Vec<SessionEvent>) {
        let (tx, rx) = channel();
        let mut session = Session::new(
            Tree::new(None),
            "test agent",
            registry,
            Box::new(RoutedLlm::new(rules)),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.root(),
            text: user_turn.into(),
        });
        session.pump_until(Instant::now() + QUIET, &mut Vec::new());
        let events = rx.try_iter().collect();
        (session, events)
    }

    /// How long `run_routed` waits for the inbox to fall quiet. Long
    /// enough for a few LLM worker threads to hand back, short enough not
    /// to slow the suite.
    const QUIET: Duration = Duration::from_millis(300);

    /// The value a branch's program returned — the `Return` on its path.
    fn returned(tree: &Tree, leaf: EventId) -> serde_json::Value {
        tree.path_events(leaf)
            .iter()
            .rev()
            .find_map(|e| match &e.payload {
                EventPayload::Return { value } => Some(value.clone()),
                _ => None,
            })
            .expect("a Return on this branch")
    }

    /// Every `Agent` root in the log, by charter.
    fn agent_by_charter(tree: &Tree, want: &str) -> EventId {
        tree.events
            .values()
            .find(|e| matches!(&e.payload, EventPayload::Agent { charter, .. } if charter == want))
            .map(|e| e.id)
            .unwrap_or_else(|| panic!("no agent chartered {want:?}"))
    }

    /// Payload kinds of one agent's spine segment, log order.
    fn kinds(tree: &Tree, leaf: EventId) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut current = leaf;
        loop {
            let event = &tree.events[&current];
            out.push(match &event.payload {
                EventPayload::Agent { .. } => "Agent",
                EventPayload::Fork { .. } => "Fork",
                EventPayload::Answer { .. } => "Answer",
                EventPayload::Message(Message::Post { .. }) => "Post",
                EventPayload::Message(Message::Turn { .. }) => "Turn",
                EventPayload::Call(_) => "Call",
                EventPayload::Result { .. } => "Result",
                EventPayload::Return { .. } => "Return",
                EventPayload::Condition { .. } => "Condition",
                EventPayload::Console { .. } => "Console",
                EventPayload::Rename { .. } => "Rename",
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

    /// The reports the LLM read on the root branch. They are **derived,
    /// not stored**, so a test derives them exactly the way a request
    /// does — from each turn's outcome and the events around it.
    fn tool_texts(session: &Session) -> Vec<String> {
        derived_reports(session.tree(), root_leaf(session))
    }

    /// Every derived tool message on a branch, in render order, paired
    /// with the call id it answers.
    fn derived_with_ids(tree: &Tree, leaf: EventId) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for event in tree.path_events(leaf) {
            let EventPayload::Message(Message::Turn { tool_calls, .. }) = &event.payload else {
                continue;
            };
            let outcomes = crate::report::outcomes_of_turn(tree, leaf, event.id);
            for (call, outcome) in tool_calls.iter().zip(outcomes) {
                out.push((
                    call.id.clone(),
                    crate::report::derive_report(tree, leaf, outcome, 64 * 1024),
                ));
            }
        }
        out
    }

    fn derived_reports(tree: &Tree, leaf: EventId) -> Vec<String> {
        derived_with_ids(tree, leaf)
            .into_iter()
            .map(|(_, text)| text)
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
                "Agent", "Post", "Turn",
                // Calls are logged at dispatch, their results at landing.
                "Call", "Call", "Result", "Result", "Return", "Console", "Turn",
                // The final turn answers the user's post — and the branch
                // is idle, not done. Agents never close.
                "Answer",
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
        ) -> Result<LlmTurn, String> {
            self.seen.lock().unwrap().push(request.system.clone());
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
            registry,
            Box::new(llm),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.root(),
            text: "go".into(),
        });
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
        let prog = LlmTurn {
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
                EventPayload::Return { value } => value.as_str().map(str::to_owned),
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
        // The final turn answers the user's post; the branch goes idle,
        // not done.
        assert_eq!(spine.last(), Some(&"Answer"));

        // The completion report must answer the *resume* call ("c2"), not
        // the original run_program ("c1") — otherwise the next chat
        // request has an assistant tool_call with no matching tool reply
        // and the provider 400s.
        let completion_call_id = derived_with_ids(session.tree(), root_leaf(&session))
            .into_iter()
            .find(|(_, text)| text.contains("returned: 42"))
            .map(|(id, _)| id)
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
                    EventPayload::Message(Message::Turn { tool_calls, .. })
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
    /// The system prompt is a snapshot on the branch **root**, not a
    /// message on the spine: `Agent.system` carries the assembled card +
    /// charter, and every request rebuilds its system message from it.
    #[test]
    fn the_agent_root_carries_the_system_prompt_snapshot() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("fetch_page", |_| Ok(json!(null))));
        let (session, _) = run_session(registry, vec![scripted_text("done")], "go");
        let tree = session.tree();
        let EventPayload::Agent {
            charter, system, ..
        } = &tree.events[&EventId::new(1)].payload
        else {
            panic!("#1 must be the root Agent");
        };
        assert_eq!(charter, "test agent");
        assert!(
            system.contains("- tools.fetch_page"),
            "card present: {system}"
        );
        assert!(
            system.contains("test agent"),
            "the charter follows the card"
        );
        // Nothing on the spine is a system message any more.
        assert!(
            !kinds(tree, root_leaf(&session)).contains(&"System"),
            "the system prompt is not a spine message"
        );
    }

    /// M2: a trapped runtime error reports, and the rewrite restart reuses
    /// the already-logged tool result by id (`tools.tool_result`) instead
    /// of repeating the call — served from the log, no second Invoke.
    #[test]
    fn trapped_error_rewrite_reuses_artifact_through_the_session() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("fetch", |_| Ok(json!("DATA"))));
        // Event ids are deterministic: Agent 1, Post 2, Turn 3, the fetch
        // `Call` 4 — so the rewrite names `tool_result(4)`, which is the
        // call id the menu shows and which resolves to its `Result`.
        let script = vec![
            scripted_program(
                "c1",
                r#"await tools.fetch("expensive"); const v = null; return v.x;"#,
            ),
            scripted_program("c2", "return await tools.tool_result(4);"),
            scripted_text("done"),
        ];
        let (session, _) = run_session(registry, script, "fetch then trip");

        // The fetch call really is #4 (guards the hardcoded id above).
        let fetch_invoke = session
            .tree()
            .events
            .values()
            .find(|e| matches!(&e.payload, EventPayload::Call(Call::Invoke { name, .. }) if name == "fetch"))
            .expect("the fetch Invoke");
        assert_eq!(fetch_invoke.id.as_u64(), 4);

        // The condition report rendered the trapped error and the menu.
        let reports = tool_texts(&session);
        assert!(
            reports[0].contains("[#4]") && reports[0].contains("fetch"),
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
                matches!(&e.payload, EventPayload::Agent { charter, .. } if charter == "child task")
            })
            .expect("child Agent");
        let child_leaf = tree
            .list_leaves()
            .into_iter()
            .map(|(id, _)| id)
            .find(|id| *id != root_leaf(&session))
            .unwrap();
        assert_eq!(kinds(tree, child_leaf), ["Agent", "Post", "Turn", "Answer"]);

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

    /// **Agents never close.** After a child answers and the parent
    /// joins, a second post to that child gets a second `Answer` — and
    /// the parent's branch is untouched by it, because the answer is owed
    /// to whoever asked *that* question.
    #[test]
    fn answered_agent_stays_addressable() {
        let script = vec![
            scripted_program(
                "c1",
                r#"return await tools.agent({ prompt: "child task", input: null });"#,
            ),
            scripted_text("first answer"),
            scripted_text("parent done"),
            scripted_text("second answer"),
        ];
        let (mut session, rx) = {
            let (tx, rx) = channel();
            let session = Session::new(
                Tree::new(None),
                "test agent",
                ToolRegistry::new(),
                Box::new(ScriptedLlm::new(script)),
                tx,
            )
            .unwrap();
            (session, rx)
        };
        session.handle().send(SessionCommand::UserTurn {
            branch: session.root(),
            text: "delegate this".into(),
        });
        while session.pump_one() {}

        // The child answered, and the parent joined its result.
        let root = session.root();
        let child = session
            .tree()
            .agent_list()
            .into_iter()
            .find(|v| v.id != root)
            .expect("a child agent");
        assert_eq!(
            kinds(
                session.tree(),
                session.state(child.id).unwrap().spine.leaf_id
            ),
            ["Agent", "Post", "Turn", "Answer"]
        );
        let parent_before = kinds(session.tree(), root_leaf(&session)).len();

        // Now speak to the child directly. It is idle, not done: the post
        // lands, it answers again, and nothing routes to the parent.
        session.handle().send(SessionCommand::UserTurn {
            branch: child.id,
            text: "one more thing".into(),
        });
        session.awaiting_user = false;
        while session.pump_one() {}

        let child_kinds = kinds(
            session.tree(),
            session.state(child.id).unwrap().spine.leaf_id,
        );
        assert_eq!(
            child_kinds,
            ["Agent", "Post", "Turn", "Answer", "Post", "Turn", "Answer"],
            "a second question gets a second answer"
        );
        assert_eq!(
            kinds(session.tree(), root_leaf(&session)).len(),
            parent_before,
            "the parent's branch is untouched by the second exchange"
        );
        let _ = rx;
    }

    /// The user's side of an exchange has no branch and no program: an
    /// agent's question to them is a `Send { to: user }` that stays
    /// pending until `Reply` settles it with a `Result`. (B1 gives
    /// programs the tool that issues one; here the `Send` is placed by
    /// hand, which is exactly what a re-opened log would hold.)
    #[test]
    fn reply_settles_a_question_to_the_user() {
        let mut tree = tree_with_open_root();
        let mut spine = tree.spine_at(EventId::new(3));
        let send = tree
            .append(
                &mut spine,
                EventPayload::Call(Call::Send {
                    to: Address::User,
                    text: "which file?".into(),
                    input: json!(null),
                    expects_reply: true,
                    site: 0,
                }),
            )
            .unwrap();

        let (session, rx) = open(tree, vec![]);
        let branch = session.root();
        let h = session.handle();
        // A call that is not a question to the user is refused…
        h.send(SessionCommand::Reply {
            branch,
            call: EventId::new(2),
            value: json!("nope"),
        });
        // …and the real one settles.
        h.send(SessionCommand::Reply {
            branch,
            call: send,
            value: json!("PLAN.md"),
        });
        h.send(SessionCommand::Shutdown);
        let session = drain(session);

        let events: Vec<SessionEvent> = rx.try_iter().collect();
        let errs = errors(&events);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("not a question to you"), "{}", errs[0]);

        let settled = session
            .tree()
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Result { call, outcome } => Some((*call, outcome.clone())),
                _ => None,
            })
            .expect("the question settled");
        assert_eq!(settled.0, send);
        assert_eq!(settled.1, Outcome::Delivered(json!("PLAN.md")));
    }

    /// A user turn is exactly one `Post`, and its answer exactly one
    /// `Answer` — no `Call`/`Result` anywhere. The user has no program to
    /// send with and no context to post into: they speak *inside* the
    /// branch and read the reply there.
    #[test]
    fn user_turn_is_a_post_on_the_branch() {
        let (session, events) = run_session(
            ToolRegistry::new(),
            vec![scripted_text("hello back")],
            "hello",
        );
        let tree = session.tree();
        assert_eq!(
            kinds(tree, root_leaf(&session)),
            ["Agent", "Post", "Turn", "Answer"]
        );

        // The post is the user's own, and it expected a reply.
        let post = tree
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Message(Message::Post { from, origin }) => {
                    Some((e.id, *from, origin.clone()))
                }
                _ => None,
            })
            .expect("a Post");
        assert_eq!(post.1, Author::User);
        assert_eq!(
            post.2.direct().map(|(t, _, r)| (t, r)),
            Some(("hello", true))
        );

        // The `Answer` names it, and nothing was sent or settled.
        let answered = tree
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Answer { question, value } => Some((*question, value.clone())),
                _ => None,
            })
            .expect("an Answer");
        assert_eq!(answered.0, post.0);
        assert_eq!(answered.1, json!("hello back"));
        assert!(
            !tree.events.values().any(|e| matches!(
                e.payload,
                EventPayload::Call(_) | EventPayload::Result { .. }
            )),
            "no call, no result: the user is an author, not an agent"
        );

        // The UI hears about it, because the human has no branch to
        // settle a call on.
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::Answered { question, .. } if *question == post.0
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
                EventPayload::Agent { charter, .. } => Some(charter.clone()),
                _ => None,
            })
            .collect();
        assert!(child_prompts.contains("task A") && child_prompts.contains("task B"));

        // Each child answered its own question independently — and each
        // is idle afterwards, still addressable.
        for (leaf, _) in tree.list_leaves() {
            if leaf == root_leaf(&session) {
                continue;
            }
            assert_eq!(kinds(tree, leaf), ["Agent", "Post", "Turn", "Answer"]);
        }

        // The caller logged both agent calls as artifacts on its spine —
        // and `tools.agent` is spawn **then** ask (B1), so each is two
        // calls: the `Spawn` that made the agent and the `Send` that
        // asked it, the second addressed at the first's result.
        let mut spawns = Vec::new();
        let mut sends = Vec::new();
        for event in tree.path_events(root_leaf(&session)) {
            match &event.payload {
                EventPayload::Call(Call::Spawn { charter, .. }) => spawns.push(charter.clone()),
                EventPayload::Call(Call::Send { to, text, .. }) => sends.push((*to, text.clone())),
                _ => {}
            }
        }
        assert_eq!(spawns.len(), 2, "one Spawn per agent call: {spawns:?}");
        assert_eq!(sends.len(), 2, "one Send per agent call: {sends:?}");
        for (to, text) in &sends {
            let Address::Branch(branch) = to else {
                panic!("a subagent question is addressed at its branch, got {to:?}");
            };
            assert!(
                matches!(&tree.events[branch].payload,
                         EventPayload::Agent { charter, .. } if charter == text),
                "the Send goes to the agent its Spawn created"
            );
        }

        // …and both results joined into the program's returned array.
        let result = tree
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Return { value } => Some(value.clone()),
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
        ) -> Result<LlmTurn, String> {
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
            ToolRegistry::new(),
            Box::new(llm),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.root(),
            text: "delegate two".into(),
        });
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
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([scripted_program(
                "c1",
                "while (true) {}",
            )])),
            tx,
        )
        .unwrap();
        let handle = session.handle();
        handle.send(SessionCommand::UserTurn {
            branch: session.root(),
            text: "spin forever".into(),
        });

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

    /// **Rule B.** A post to a running program is logged on arrival and
    /// the run suspends into `Condition::Posted` at its next fuel slice —
    /// never rejected, never queued invisibly, never lost to a crash.
    /// (Re-pointed from the M2-era test that asserted the rejection.)
    #[test]
    fn user_turn_while_busy_suspends_the_program() {
        let (tx, rx) = channel();
        let mut session = Session::new(
            Tree::new(None),
            "test agent",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([
                scripted_program("c1", "while (true) {}"),
                scripted_text("stopping, then"),
            ])),
            tx,
        )
        .unwrap();
        let handle = session.handle();
        handle.send(SessionCommand::UserTurn {
            branch: session.root(),
            text: "go".into(),
        });
        for _ in 0..10 {
            assert!(session.pump_one(), "the hot program keeps ticking");
        }
        handle.send(SessionCommand::UserTurn {
            branch: session.root(),
            text: "are you done yet?".into(),
        });
        session.pump_until(Instant::now() + QUIET, &mut Vec::new());
        // No rejection: the post landed, and the hot program suspended
        // into a condition whose report *is* the message.
        assert!(
            !rx.try_iter().any(
                |e| matches!(&e, SessionEvent::Error { message, .. } if message.contains("busy"))
            ),
            "nothing you say is rejected"
        );
        let tree = session.tree();
        let post = tree
            .events
            .values()
            .find(|e| {
                matches!(&e.payload,
                EventPayload::Message(Message::Post { origin, .. })
                if origin.direct().is_some_and(|(t, _, _)| t == "are you done yet?"))
            })
            .expect("the post is logged on arrival")
            .id;
        let posted = tree
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Condition {
                    cause: Cause::Posted { ids },
                    ..
                } => Some(ids.clone()),
                _ => None,
            })
            .expect("the run suspended into Condition::Posted");
        assert_eq!(posted, [post], "the condition names the message");
        handle.send(SessionCommand::Shutdown);
        while session.pump_one() {}
    }

    // --- M4: fork / label / resume / list-leaves ---

    fn user(text: &str) -> EventPayload {
        EventPayload::Message(Message::Post {
            from: Author::User,
            origin: Origin::Direct {
                text: text.into(),
                input: json!(null),
                expects_reply: true,
            },
        })
    }

    fn assistant(text: &str) -> EventPayload {
        EventPayload::Message(Message::Turn {
            author: Author::Agent(EventId::new(1)),
            text: text.into(),
            thinking: None,
            tool_calls: Vec::new(),
        })
    }

    /// An incomplete root agent: Agent(1), User(2 "q"),
    /// Assistant(3 "a1"). Leaf = #3 — open, so resumable and forkable.
    fn tree_with_open_root() -> Tree {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "").unwrap();
        tree.append(&mut spine, user("q")).unwrap();
        tree.append(&mut spine, assistant("a1")).unwrap();
        tree
    }

    fn open(tree: Tree, script: Vec<LlmTurn>) -> (Session, Receiver<SessionEvent>) {
        let (tx, rx) = channel();
        let session = Session::new(
            tree,
            "ignored on resume",
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
        assert!(leaves[0].active && leaves[0].open == 1);
        assert_eq!(leaves[0].summary, "Turn: a1");
    }

    #[test]
    fn rename_logs_on_the_active_branch_and_surfaces() {
        let (session, rx) = open(tree_with_open_root(), vec![]);
        let h = session.handle();
        h.send(SessionCommand::Rename("my-branch".into()));
        h.send(SessionCommand::Shutdown);
        let session = drain(session);

        // A Rename event was logged on the root spine.
        assert!(
            session.tree().events.values().any(
                |e| matches!(&e.payload, EventPayload::Rename { name } if name == "my-branch")
            )
        );
        // …and the refreshed leaf list carries it as the branch's name.
        let leaves = last_leaves(&rx.try_iter().collect::<Vec<_>>());
        assert_eq!(leaves.len(), 1);
        assert!(leaves[0].active);
        assert_eq!(leaves[0].name.as_deref(), Some("my-branch"));
    }

    /// A `Rename` is a record, not a `Message`: it must not start an LLM
    /// turn. The scripted client has no responses at all, so any request
    /// would fail the run.
    #[test]
    fn rename_does_not_wake() {
        let (session, rx) = open(tree_with_open_root(), vec![]);
        let h = session.handle();
        h.send(SessionCommand::Rename("quiet".into()));
        h.send(SessionCommand::Shutdown);
        let session = drain(session);

        let events: Vec<SessionEvent> = rx.try_iter().collect();
        assert!(errors(&events).is_empty(), "{:?}", errors(&events));
        // Nothing but the rename was appended, and no turn was taken.
        let kinds = kinds(session.tree(), root_leaf(&session));
        assert_eq!(kinds, ["Agent", "Post", "Turn", "Rename"], "{kinds:?}");
    }

    #[test]
    fn fork_then_user_turn_diverges_in_the_same_agent() {
        let (session, rx) = open(tree_with_open_root(), vec![scripted_text("forked done")]);
        let h = session.handle();
        // Fork off the user message (#2), dropping the original a1 reply.
        h.send(SessionCommand::Fork {
            from: EventId::new(2),
            name: Some("retry".into()),
        });
        h.send(SessionCommand::UserTurn {
            branch: session.root(),
            text: "forked follow-up".into(),
        });
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
        // the spine stays open — nothing seals a branch.
        let forked_leaf = leaves
            .iter()
            .map(|(id, _)| *id)
            .find(|id| *id != EventId::new(3))
            .unwrap();
        let forked = tree.spine_at(forked_leaf);
        assert!(session.is_awaiting_user());
        // The system prompt is on the branch root now, not a message.
        let msgs: Vec<&str> = forked.context().messages.iter().map(|m| m.text()).collect();
        assert_eq!(msgs, ["q", "forked follow-up", "forked done"]);
        // The fork's name sits on the new branch, not the original.
        let leaves = last_leaves(&rx.try_iter().collect::<Vec<_>>());
        assert_eq!(
            leaves
                .iter()
                .find(|l| l.name.is_some())
                .and_then(|l| l.name.clone()),
            Some("retry".into())
        );
        assert_eq!(
            leaves.iter().filter(|l| l.name.is_some()).count(),
            1,
            "renaming the fork left the original unnamed"
        );
    }

    /// Nothing seals a branch any more, so the only resume that can be
    /// rejected is one naming an id the log does not hold. An
    /// already-answered branch is a perfectly good place to resume.
    #[test]
    fn resume_switches_root_and_rejects_only_the_unknown() {
        // Open root (#3) plus a sibling branch, forked off #2, that has
        // answered — under the old rules that spine was sealed.
        let mut tree = tree_with_open_root();
        let mut branch = tree.fork(EventId::new(2)).unwrap();
        tree.append(&mut branch, user("other")).unwrap();
        let answered_leaf = tree
            .append(
                &mut branch,
                EventPayload::Answer {
                    question: EventId::new(2),
                    value: json!("x"),
                },
            )
            .unwrap();

        let (session, rx) = open(tree, vec![]);
        let h = session.handle();
        h.send(SessionCommand::Resume(EventId::new(3)));
        h.send(SessionCommand::Resume(answered_leaf)); // answered — still fine
        h.send(SessionCommand::Resume(EventId::new(99))); // unknown — rejected
        h.send(SessionCommand::Shutdown);
        let session = drain(session);

        let events: Vec<SessionEvent> = rx.try_iter().collect();
        let errs = errors(&events);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("not in the log"));
        // The last accepted resume left the active root on that branch.
        assert_eq!(
            session.state(session.root()).unwrap().spine.leaf_id,
            answered_leaf
        );
    }

    #[test]
    fn mutating_commands_are_rejected_while_the_agent_is_busy() {
        let (mut session, rx) = open(
            tree_with_open_root(),
            vec![scripted_program("c1", "while (true) {}")],
        );
        let h = session.handle();
        h.send(SessionCommand::UserTurn {
            branch: session.root(),
            text: "spin".into(),
        });
        for _ in 0..6 {
            session.pump_one();
        }
        // The agent is now Running; every mutating command bounces.
        h.send(SessionCommand::Rename("late".into()));
        h.send(SessionCommand::Fork {
            from: EventId::new(2),
            name: None,
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
        assert_eq!(busy, 3, "rename/fork/resume each rejected while busy");
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
            .start_agent(None, None, "you are an agent", None, "")
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "do something".into(),
                    input: json!(null),
                    expects_reply: true,
                },
            }),
        )
        .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Turn {
                author: Author::Agent(EventId::new(1)),
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
        // No outcome at all — the VM was lost with the process.

        // Open the log; pick_resume_leaf finds leaf #3.
        let (tx, rx) = channel();
        let session = Session::new(
            tree,
            "ignored",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new(vec![
                scripted_program("c2", "return 999;"),
                scripted_text("final"),
            ])),
            tx,
        )
        .expect("opens the interrupted log");

        // The repair is one event — `Condition{Interrupted}` — and the
        // report is *derived* from it, so "the report was lost" is not a
        // case that can exist.
        assert!(
            session.tree().events.values().any(|e| matches!(
                &e.payload,
                EventPayload::Condition {
                    cause: Cause::Interrupted,
                    ..
                }
            )),
            "the interrupted run was given an outcome"
        );
        let tools = tool_texts(&session);
        let interrupted_report = tools
            .first()
            .expect("a derived report for the interrupted run_program");
        assert!(
            interrupted_report.contains("interrupted before completing"),
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
        session.handle().send(SessionCommand::UserTurn {
            branch: session.root(),
            text: "continue".into(),
        });
        let session = session.run();

        // The rewrite should have produced a completion report, then a
        // final text turn that yields the root's turn back to the user
        // (an `Answer`, then idle — the conversation never ends).
        let all_tools = tool_texts(&session);
        assert!(
            all_tools.iter().any(|t| t.contains("program completed")),
            "rewrite completed: {all_tools:?}"
        );
        let kinds = kinds(session.tree(), root_leaf(&session));
        assert!(
            kinds.last() == Some(&"Answer"),
            "the branch answered and went idle: {kinds:?}"
        );
        assert!(session.is_awaiting_user());

        let _events: Vec<SessionEvent> = rx.try_iter().collect();
    }

    /// A log where every question has been answered opens **idle**, not
    /// done: agents never close, so the branch is addressable and the
    /// next post simply continues it.
    #[test]
    fn a_fully_answered_log_opens_idle() {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "").unwrap();
        let question = tree.append(&mut spine, user("q")).unwrap();
        tree.append(&mut spine, assistant("done")).unwrap();
        tree.append(
            &mut spine,
            EventPayload::Answer {
                question,
                value: json!("done"),
            },
        )
        .unwrap();

        let (tx, _rx) = channel();
        let session = Session::new(
            tree,
            "ignored",
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new(vec![])),
            tx,
        )
        .expect("an answered log opens idle, not an error");
        assert_eq!(session.root(), EventId::new(1));
        let state = session.state(session.root()).unwrap();
        assert!(state.is_idle());
        assert!(state.open().is_empty(), "nothing is owed");
    }

    // ── B1: spawn / ask / tell / agents ─────────────────────────────

    /// The exchange is four events forming a **closed loop of ids** —
    /// `Post.origin → Send`, `Result.call → Send`, `Answer.question →
    /// Post` — so from any one the other three are one lookup away. That
    /// is what reconciliation walks, what a renderer resolves a body
    /// through, and how an answer finds the *branch* that asked.
    #[test]
    fn exchange_ids_form_a_closed_loop() {
        let (session, _) = run_routed(
            ToolRegistry::new(),
            [
                (
                    "reads files",
                    vec![scripted_text("PLAN.md, and it is 40 lines")],
                ),
                (
                    "test agent",
                    vec![
                        scripted_program(
                            "c1",
                            r#"const w = await tools.spawn({ name: "r", charter: "reads files" });
                               return await tools.ask({ to: w.agent, text: "which file?" });"#,
                        ),
                        scripted_text("done"),
                    ],
                ),
            ],
            "ask the researcher",
        );
        let tree = session.tree();
        let worker = agent_by_charter(tree, "reads files");

        // Start from the `Send` — the event that records "I asked" — and
        // walk to the other three, then back.
        let send = tree
            .events
            .values()
            .find(|e| matches!(&e.payload, EventPayload::Call(Call::Send { .. })))
            .map(|e| e.id)
            .expect("the Send");
        assert_eq!(
            tree.enclosing_agent(send),
            Some(session.root()),
            "the Send sits on the asking branch"
        );

        // Send → Post: the delivery marker naming it, on the callee.
        let post = tree
            .events
            .values()
            .find(|e| {
                matches!(&e.payload,
                    EventPayload::Message(Message::Post { origin: Origin::Sent(s), .. })
                    if *s == send)
            })
            .map(|e| e.id)
            .expect("the Post naming that Send");
        let EventPayload::Message(Message::Post { from, .. }) = &tree.events[&post].payload else {
            unreachable!()
        };
        assert_eq!(*from, Author::Agent(session.root()));
        assert_eq!(tree.enclosing_agent(post), Some(worker));

        // Post → Answer: the reply, on the answerer's branch.
        let (answer, value) = tree
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Answer { question, value } if *question == post => {
                    Some((e.id, value.clone()))
                }
                _ => None,
            })
            .expect("the Answer naming that Post");
        assert_eq!(value, json!("PLAN.md, and it is 40 lines"));
        assert_eq!(tree.enclosing_agent(answer), Some(worker));

        // Send → Result: the settlement, back on the asker's branch. No
        // routing table was consulted to get it there.
        let result = tree
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Result { call, outcome } if *call == send => {
                    Some((e.id, outcome.clone()))
                }
                _ => None,
            })
            .expect("the Result naming that Send");
        assert_eq!(result.1, Outcome::Delivered(value.clone()));
        assert_eq!(tree.enclosing_agent(result.0), Some(session.root()));

        // …and the program got the answer, whole.
        assert_eq!(returned(tree, root_leaf(&session)), value);

        // The agent's own root is a child of the `Spawn` that made it.
        let spawn = tree
            .events
            .values()
            .find(|e| matches!(&e.payload, EventPayload::Call(Call::Spawn { .. })))
            .map(|e| e.id)
            .expect("the Spawn");
        assert_eq!(tree.events[&worker].parent_id, Some(spawn));
    }

    /// `tell` informs without asking: three events, no `Answer`. The
    /// sender's receipt lands as soon as the post does — what a tell
    /// spares is the answer, not the attention, so the recipient still
    /// spends a turn noticing it.
    #[test]
    fn tell_delivers_receipt_and_opens_nothing() {
        let (session, _) = run_routed(
            ToolRegistry::new(),
            [
                ("takes notes", vec![scripted_text("noted")]),
                (
                    "test agent",
                    vec![
                        scripted_program(
                            "c1",
                            r#"const w = await tools.spawn({ name: "n", charter: "takes notes" });
                               return await tools.tell({ to: w.agent, text: "fyi: skip the cache" });"#,
                        ),
                        scripted_text("told them"),
                    ],
                ),
            ],
            "inform the worker",
        );
        let tree = session.tree();
        let worker = agent_by_charter(tree, "takes notes");
        let worker_leaf = session.state(worker).unwrap().spine.leaf_id;

        // The recipient: a post, a turn, and **no `Answer`** — it owes
        // nothing, so nothing is open on it.
        assert_eq!(kinds(tree, worker_leaf), ["Agent", "Post", "Turn"]);
        assert!(
            tree.spine_at(worker_leaf).context().open.is_empty(),
            "a tell opens nothing"
        );

        // The sender: a receipt naming the post that landed.
        let post = tree
            .path_events(worker_leaf)
            .iter()
            .find(|e| {
                matches!(
                    e.payload,
                    EventPayload::Message(Message::Post {
                        origin: Origin::Sent(_),
                        ..
                    })
                )
            })
            .map(|e| e.id)
            .expect("the delivered post");
        assert_eq!(
            returned(tree, root_leaf(&session)),
            json!({ "post": post.as_u64() })
        );
    }

    /// **Agents outlive programs, but a program's handles to them do
    /// not.** A later program — a new VM — re-discovers its workers by
    /// query rather than by memory, which is what makes orchestration
    /// across programs, hours and crashes possible.
    #[test]
    fn agents_survive_across_programs() {
        let (session, _) = run_routed(
            ToolRegistry::new(),
            [(
                "test agent",
                vec![
                    scripted_program(
                        "c1",
                        r#"const names = ["alpha", "beta", "gamma"];
                           const made = await Promise.all(
                             names.map(n => tools.spawn({ name: n, charter: "worker " + n })));
                           return made.map(m => m.agent);"#,
                    ),
                    // A different program, a fresh VM: the handles above
                    // are gone, and the workers are found by query.
                    scripted_program("c2", "return await tools.agents();"),
                    scripted_text("three workers, all idle"),
                ],
            )],
            "spawn three workers",
        );
        let tree = session.tree();
        // The *last* `Return` on the branch is the second program's.
        let rows: Vec<serde_json::Value> = returned(tree, root_leaf(&session))
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rows.len(), 3, "three rows: {rows:?}");

        let mut listed: Vec<(String, u64, &'static str)> = rows
            .iter()
            .map(|r| {
                (
                    r["name"].as_str().unwrap().to_owned(),
                    r["agent"].as_u64().unwrap(),
                    match r["status"].as_str().unwrap() {
                        "idle" => "idle",
                        other => panic!("a spawned-but-unasked agent is idle, got {other}"),
                    },
                )
            })
            .collect();
        listed.sort();
        assert_eq!(
            listed.iter().map(|(n, ..)| n.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta", "gamma"]
        );
        for row in &rows {
            assert_eq!(row["parent"].as_u64(), Some(session.root().as_u64()));
            assert_eq!(row["open"].as_u64(), Some(0), "nobody asked them anything");
            assert!(row["last_answer"].is_null());
            assert_eq!(
                row["branch"], row["agent"],
                "an unforked agent's one branch is rooted at the agent itself"
            );
            assert!(row["charter"].as_str().unwrap().starts_with("worker "));
        }
    }

    /// `agents()` lists your **direct** children; `{ deep: true }` reaches
    /// the whole subtree — a worker's own worker included.
    #[test]
    fn agents_deep_reaches_a_grandchild() {
        let (session, _) = run_routed(
            ToolRegistry::new(),
            [
                (
                    "worker",
                    vec![
                        scripted_program(
                            "w1",
                            r#"const g = await tools.spawn({ name: "helper", charter: "helps" });
                               return g.agent;"#,
                        ),
                        scripted_text("made a helper"),
                    ],
                ),
                (
                    "test agent",
                    vec![
                        scripted_program(
                            "c1",
                            r#"const w = await tools.spawn({ name: "w", charter: "worker" });
                               return await tools.ask({ to: w.agent, text: "make a helper" });"#,
                        ),
                        scripted_program(
                            "c2",
                            r#"return { direct: await tools.agents(),
                                        deep: await tools.agents({ deep: true }) };"#,
                        ),
                        scripted_text("done"),
                    ],
                ),
            ],
            "delegate a delegation",
        );
        let tree = session.tree();
        let listing = returned(tree, root_leaf(&session));
        let names = |key: &str| {
            let mut out: Vec<String> = listing[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["name"].as_str().unwrap().to_owned())
                .collect();
            out.sort();
            out
        };
        assert_eq!(names("direct"), ["w"], "direct children only by default");
        assert_eq!(
            names("deep"),
            ["helper", "w"],
            "deep reaches the grandchild"
        );

        // The grandchild's `parent` is the worker, not the root.
        let worker = agent_by_charter(tree, "worker");
        let helper = listing["deep"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "helper")
            .unwrap()
            .clone();
        assert_eq!(helper["parent"].as_u64(), Some(worker.as_u64()));
    }

    /// One row per **branch**. An agent has one branch until someone
    /// forks it; then it has two, both live, both its own — and both list,
    /// sharing the `agent` they are branches of.
    #[test]
    fn forked_agent_lists_two_branches() {
        // Built as a log rather than driven: forking is a user gesture
        // (C1's command), and what is under test is the projection.
        let mut tree = Tree::new(None);
        let mut root = tree
            .start_agent(None, Some("root".into()), "test agent", None, "test agent")
            .unwrap();
        let spawn = tree
            .append(
                &mut root,
                EventPayload::Call(Call::Spawn {
                    name: Some("w".into()),
                    charter: "worker".into(),
                    tools: None,
                    site: 0,
                }),
            )
            .unwrap();
        let mut worker = tree
            .start_agent(Some(spawn), Some("w".into()), "worker", None, "worker")
            .unwrap();
        tree.append(&mut worker, user("first question")).unwrap();
        let fork_point = worker.leaf_id;
        let mut sidebar = tree.fork(fork_point).unwrap();
        tree.append(
            &mut sidebar,
            EventPayload::Fork {
                name: Some("sidebar".into()),
            },
        )
        .unwrap();

        let (tx, _rx) = channel();
        let session = Session::open_at(
            tree,
            fork_point,
            ToolRegistry::new(),
            Box::new(ScriptedLlm::new([])),
            tx,
        )
        .unwrap();
        // Asked from the orchestrator: `under` defaults to the caller.
        let orchestrator = agent_by_charter(session.tree(), "test agent");
        let rows = session
            .serve_agents(orchestrator, &json!([serde_json::Value::Null]))
            .unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 2, "two branches of one agent: {rows:?}");
        let agent = agent_by_charter(session.tree(), "worker").as_u64();
        assert!(
            rows.iter().all(|r| r["agent"].as_u64() == Some(agent)),
            "both rows share the agent: {rows:?}"
        );
        let mut branches: Vec<(u64, &str)> = rows
            .iter()
            .map(|r| (r["branch"].as_u64().unwrap(), r["name"].as_str().unwrap()))
            .collect();
        branches.sort();
        assert_eq!(branches[0].1, "w", "the agent's own branch keeps its name");
        assert_eq!(
            branches[1].1, "sidebar",
            "the fork is named for how it differs"
        );
        assert_ne!(branches[0].0, branches[1].0, "two distinct branch ids");
    }

    /// Broadcast is not a primitive: it is `Promise.all` over `agents()`.
    /// No relay, kill or subscribe tool exists either, because each would
    /// be a tool doing what a line of program already does.
    #[test]
    fn broadcast_is_promise_all_over_agents() {
        let (session, _) = run_routed(
            ToolRegistry::new(),
            [
                ("worker a", vec![scripted_text("a: ok")]),
                ("worker b", vec![scripted_text("b: ok")]),
                ("worker c", vec![scripted_text("c: ok")]),
                (
                    "test agent",
                    vec![
                        scripted_program(
                            "c1",
                            r#"await Promise.all(["a", "b", "c"].map(n =>
                                 tools.spawn({ name: n, charter: "worker " + n })));
                               return "spawned";"#,
                        ),
                        scripted_program(
                            "c2",
                            r#"const rows = await tools.agents();
                               return await Promise.all(
                                 rows.map(r => tools.ask({ to: r.branch, text: "status?" })));"#,
                        ),
                        scripted_text("all three reported"),
                    ],
                ),
            ],
            "check on everyone",
        );
        let tree = session.tree();
        let mut answers: Vec<String> = returned(tree, root_leaf(&session))
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        answers.sort();
        assert_eq!(answers, ["a: ok", "b: ok", "c: ok"]);
        // One question each, and each answered on its own branch.
        for charter in ["worker a", "worker b", "worker c"] {
            let agent = agent_by_charter(tree, charter);
            let leaf = session.state(agent).unwrap().spine.leaf_id;
            assert_eq!(kinds(tree, leaf), ["Agent", "Post", "Turn", "Answer"]);
        }
    }

    /// A worker keeps its context between questions: the second question
    /// arrives in a conversation that already holds the first exchange.
    /// **Agents never close** — that is what makes a second question just
    /// another post.
    #[test]
    fn second_question_sees_first_exchange() {
        let (session, _) = run_routed(
            ToolRegistry::new(),
            [
                (
                    "remembers",
                    vec![scripted_text("seven"), scripted_text("still seven")],
                ),
                (
                    "test agent",
                    vec![
                        scripted_program(
                            "c1",
                            r#"const w = await tools.spawn({ name: "m", charter: "remembers" });
                               const first = await tools.ask({ to: w.agent, text: "how many?" });
                               const second = await tools.ask({ to: w.agent, text: "sure?" });
                               return [first, second];"#,
                        ),
                        scripted_text("asked twice"),
                    ],
                ),
            ],
            "ask twice",
        );
        let tree = session.tree();
        let worker = agent_by_charter(tree, "remembers");
        let leaf = session.state(worker).unwrap().spine.leaf_id;

        // Two full exchanges on one branch, nothing sealed in between.
        assert_eq!(
            kinds(tree, leaf),
            ["Agent", "Post", "Turn", "Answer", "Post", "Turn", "Answer"]
        );
        assert!(
            tree.spine_at(leaf).context().open.is_empty(),
            "both questions answered"
        );

        // The second question landed in a conversation that already held
        // the first exchange — the worker kept its context.
        let messages: Vec<String> = tree
            .spine_at(leaf)
            .context()
            .messages
            .iter()
            .map(|m| m.text().to_owned())
            .collect();
        assert_eq!(messages, ["how many?", "seven", "sure?", "still seven"]);
        assert_eq!(
            returned(tree, root_leaf(&session)),
            json!(["seven", "still seven"])
        );
    }

    /// `ask` with `to` omitted means *the author of the question you are
    /// answering*. For a root conversation that is the human; for a
    /// subagent it is its parent — and a program never needs to know
    /// which.
    #[test]
    fn default_to_is_the_current_asker() {
        // Case 1: the root's asker is the human. The invoke is addressed
        // `to: user` and stays pending — the user has no branch, so there
        // is **no `Post` anywhere**, only the question inline.
        let (session, _) = run_routed(
            ToolRegistry::new(),
            [(
                "test agent",
                vec![scripted_program(
                    "c1",
                    r#"return await tools.ask({ text: "which one did you mean?" });"#,
                )],
            )],
            "do the thing",
        );
        let tree = session.tree();
        let sends: Vec<&Call> = tree
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Call(c @ Call::Send { .. }) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(sends.len(), 1);
        let Call::Send {
            to, expects_reply, ..
        } = sends[0]
        else {
            unreachable!()
        };
        assert_eq!(*to, Address::User, "the root's asker is the human");
        assert!(*expects_reply);
        assert!(
            !tree.events.values().any(|e| matches!(
                &e.payload,
                EventPayload::Message(Message::Post {
                    origin: Origin::Sent(_),
                    ..
                })
            )),
            "a question to the human posts nowhere"
        );
        assert!(
            !tree
                .events
                .values()
                .any(|e| matches!(e.payload, EventPayload::Result { .. })),
            "and stays pending until the human replies"
        );

        // Case 2: a subagent's asker is its parent.
        let (session, _) = run_routed(
            ToolRegistry::new(),
            [
                (
                    "needs guidance",
                    vec![scripted_program(
                        "w1",
                        r#"return await tools.ask({ text: "which one?" });"#,
                    )],
                ),
                (
                    "test agent",
                    vec![scripted_program(
                        "c1",
                        r#"const w = await tools.spawn({ name: "w", charter: "needs guidance" });
                           return await tools.ask({ to: w.agent, text: "pick one" });"#,
                    )],
                ),
            ],
            "delegate",
        );
        let tree = session.tree();
        let upward = tree
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Call(Call::Send { to, text, .. }) if text == "which one?" => {
                    Some(*to)
                }
                _ => None,
            })
            .expect("the upward ask");
        assert_eq!(
            upward,
            Address::Branch(session.root()),
            "a subagent's asker is its parent's branch"
        );
        // It reached the parent: logged on arrival even mid-program, and
        // heard there as a condition (rule B, `upward_clarification_
        // does_not_deadlock` walks the whole round trip).
        let parent_path = tree.path_events(root_leaf(&session));
        assert!(
            parent_path.iter().any(|e| matches!(
                &e.payload,
                EventPayload::Message(Message::Post { from: Author::Agent(a), .. })
                if tree.enclosing_agent(*a) != Some(session.root())
            )),
            "the worker's question is on the parent's branch: {:?}",
            kinds(tree, root_leaf(&session))
        );
        assert!(
            parent_path.iter().any(|e| matches!(
                &e.payload,
                EventPayload::Condition {
                    cause: Cause::Posted { .. },
                    ..
                }
            )),
            "and the running parent suspended on it"
        );
    }

    /// An agent id is an address only while the agent has one branch. A
    /// forked one is **ambiguous**, and guessing which fork owes the
    /// answer is exactly the race the one-owner rule exists to prevent —
    /// so the call is rejected, naming the branches.
    #[test]
    fn ambiguous_agent_id_is_refused() {
        let mut tree = Tree::new(None);
        let mut root = tree
            .start_agent(None, None, "test agent", None, "test agent")
            .unwrap();
        let spawn = tree
            .append(
                &mut root,
                EventPayload::Call(Call::Spawn {
                    name: Some("w".into()),
                    charter: "worker".into(),
                    tools: None,
                    site: 0,
                }),
            )
            .unwrap();
        let worker = tree
            .start_agent(Some(spawn), None, "worker", None, "worker")
            .unwrap();
        let mut sidebar = tree.fork(worker.leaf_id).unwrap();
        tree.append(&mut sidebar, EventPayload::Fork { name: None })
            .unwrap();
        let anchor = root.leaf_id;
        let worker_id = worker.leaf_id;

        let (tx, rx) = channel();
        let mut session = Session::open_at(
            tree,
            anchor,
            ToolRegistry::new(),
            Box::new(RoutedLlm::new([(
                "test agent",
                vec![scripted_program(
                    "c1",
                    &format!(
                        r#"try {{ return await tools.ask({{ to: {}, text: "hi" }}); }}
                           catch (e) {{ return "refused: " + e; }}"#,
                        worker_id.as_u64()
                    ),
                )],
            )])),
            tx,
        )
        .unwrap();
        session.handle().send(SessionCommand::UserTurn {
            branch: session.root(),
            text: "ask the worker".into(),
        });
        session.pump_until(Instant::now() + QUIET, &mut Vec::new());
        drop(rx);

        let refused = returned(session.tree(), root_leaf(&session));
        let text = refused.as_str().expect("a rejected call is catchable");
        assert!(text.contains("2 live branches"), "{text}");
        assert!(text.contains("address one of them"), "{text}");
        // Nothing was logged: the call was never dispatched, so it owes
        // no `Result`.
        assert!(
            !session
                .tree()
                .events
                .values()
                .any(|e| matches!(&e.payload, EventPayload::Call(Call::Send { .. }))),
            "a rejected address logs no Send"
        );
    }

    /// `tools` narrows a child's allowlist, enforced by the registry from
    /// the **child's own `Agent` root** — which is why the root agent,
    /// having no `Spawn`, is not a special case. A child can never widen
    /// past its parent, so "default: yours" is an intersection.
    #[test]
    fn spawned_tools_narrow_the_childs_allowlist() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("allowed", |_| Ok(json!("ok"))));
        registry.register(tool("forbidden", |_| Ok(json!("nope"))));
        let (session, _) = run_routed(
            registry,
            [
                (
                    "narrowed",
                    vec![
                        scripted_program(
                            "w1",
                            r#"const ok = await tools.allowed();
                               let denied;
                               try { denied = await tools.forbidden(); }
                               catch (e) { denied = "refused: " + e; }
                               return [ok, denied];"#,
                        ),
                        scripted_text("one of two"),
                    ],
                ),
                (
                    "test agent",
                    vec![
                        scripted_program(
                            "c1",
                            r#"const w = await tools.spawn(
                                 { name: "n", charter: "narrowed", tools: ["allowed"] });
                               return await tools.ask({ to: w.agent, text: "try both" });"#,
                        ),
                        scripted_text("done"),
                    ],
                ),
            ],
            "narrow a child",
        );
        let tree = session.tree();
        let child = agent_by_charter(tree, "narrowed");
        let EventPayload::Agent { tools, .. } = &tree.events[&child].payload else {
            unreachable!()
        };
        assert_eq!(
            tools.as_deref(),
            Some(&["allowed".to_owned()][..]),
            "the allowlist lives on the agent's own root"
        );
        let leaf = session.state(child).unwrap().spine.leaf_id;
        let pair = returned(tree, leaf);
        assert_eq!(pair[0], json!("ok"));
        let refused = pair[1].as_str().unwrap();
        assert!(
            refused.contains("not in this agent's allowlist"),
            "{refused}"
        );
        assert!(refused.contains("have: allowed"), "{refused}");
        // The child's card never advertised what its calls would refuse.
        let EventPayload::Agent { system, .. } = &tree.events[&child].payload else {
            unreachable!()
        };
        assert!(system.contains("- tools.allowed"), "narrowed card");
        assert!(!system.contains("- tools.forbidden"), "narrowed card");
    }

    // ── B2: structured answers ──────────────────────────────────────

    /// `Answer.value` is JSON: a structured answer reaches the asking
    /// **program** as an object, not as prose it would have to parse.
    /// 8_HARNESS decision 3 said a subagent's result is a JSON value and
    /// `finish_frame` could only produce a string. Closed.
    #[test]
    fn structured_answer_reaches_the_program() {
        // Event ids are deterministic: Agent 1, Post 2, Turn 3, Spawn 4,
        // Agent 5, Result 6, Send 7, Post 8 — so the worker's one open
        // question is #8, which the assertion below guards.
        let question = 8;
        let (session, _) = run_routed(
            ToolRegistry::new(),
            [
                (
                    "counts things",
                    vec![
                        scripted_answer(
                            "w1",
                            EventId::new(question),
                            json!({ "files": 3, "bytes": 1200 }),
                        ),
                        scripted_text("counted"),
                    ],
                ),
                (
                    "test agent",
                    vec![
                        scripted_program(
                            "c1",
                            r#"const w = await tools.spawn(
                                 { name: "c", charter: "counts things" });
                               const v = await tools.ask({ to: w.agent, text: "how many?" });
                               return [typeof v, v.files, v.bytes];"#,
                        ),
                        scripted_text("structured"),
                    ],
                ),
            ],
            "count them",
        );
        let tree = session.tree();
        // The hardcoded id really is the worker's open question.
        let worker = agent_by_charter(tree, "counts things");
        let worker_leaf = session.state(worker).unwrap().spine.leaf_id;
        assert!(
            matches!(
                &tree.events[&EventId::new(question)].payload,
                EventPayload::Message(Message::Post {
                    origin: Origin::Sent(_),
                    ..
                })
            ),
            "#{question} must be the delivered question"
        );

        // An object, indexable — never a string the program must parse.
        assert_eq!(
            returned(tree, root_leaf(&session)),
            json!(["object", 3, 1200])
        );
        // `answer` left the worker's phase alone, so it took another turn
        // and then went idle owing nothing.
        assert_eq!(
            kinds(tree, worker_leaf),
            ["Agent", "Post", "Turn", "Answer", "Turn"]
        );
        assert!(tree.spine_at(worker_leaf).context().open.is_empty());
    }

    // ── B3: the upward round trip ───────────────────────────────────

    /// **Upward questions cannot deadlock.** A parent awaiting its child
    /// is one fuel slice from being told: the post suspends the parent's
    /// program into a condition, its LLM answers and resumes in one turn,
    /// the child's `Result` lands, the child answers, and the parent's ask
    /// resolves.
    ///
    /// A real runtime deadlocks here because the waiter is on a stack.
    /// Here the waiter is a `StepResult`, which is the whole point of
    /// rule B being written against fuel slices.
    #[test]
    fn upward_clarification_does_not_deadlock() {
        // Ids are deterministic: Agent 1, Post 2, Turn 3, Spawn 4,
        // Agent 5, Result 6, Send 7 (parent→child), Post 8 (on the
        // child), Turn 9, Send 10 (child→parent), Post 11 (on the
        // parent) — so the parent answers #11 and the child answers #8.
        let child_question = 8;
        let upward_question = 11;
        let (session, _) = run_routed(
            ToolRegistry::new(),
            [
                (
                    "needs a path",
                    vec![
                        // The child asks upward with no `to`: whoever
                        // asked it. It is parked, costing no fuel.
                        scripted_program(
                            "w1",
                            r#"const path = await tools.ask({ text: "which file?" });
                               return "read " + path;"#,
                        ),
                        scripted_text("done, read PLAN.md"),
                    ],
                ),
                (
                    "test agent",
                    vec![
                        scripted_program(
                            "c1",
                            r#"const w = await tools.spawn(
                                 { name: "w", charter: "needs a path" });
                               return await tools.ask({ to: w.agent, text: "read the plan" });"#,
                        ),
                        // The post-condition report's first move: answer
                        // and carry on, in one turn.
                        LlmTurn {
                            text: String::new(),
                            thinking: None,
                            tool_calls: vec![
                                ToolCall {
                                    id: "a1".into(),
                                    name: crate::machine::TOOL_ANSWER.into(),
                                    arguments: json!({
                                        "question": upward_question,
                                        "value": "PLAN.md",
                                    }),
                                },
                                ToolCall {
                                    id: "r1".into(),
                                    name: crate::machine::TOOL_RESUME.into(),
                                    arguments: json!({}),
                                },
                            ],
                        },
                        scripted_text("the worker read it"),
                    ],
                ),
            ],
            "have the worker read the plan",
        );
        let tree = session.tree();
        let child = agent_by_charter(tree, "needs a path");
        let child_leaf = session.state(child).unwrap().spine.leaf_id;

        // The hardcoded ids really are those two posts.
        for id in [child_question, upward_question] {
            assert!(
                matches!(
                    &tree.events[&EventId::new(id)].payload,
                    EventPayload::Message(Message::Post {
                        origin: Origin::Sent(_),
                        ..
                    })
                ),
                "#{id} must be a delivered question"
            );
        }
        // The parent heard the upward question as a *condition*, not as a
        // deadlock.
        assert!(
            tree.events.values().any(|e| matches!(
                &e.payload,
                EventPayload::Condition { cause: Cause::Posted { ids }, .. }
                if ids.contains(&EventId::new(upward_question))
            )),
            "the running parent suspended on the child's question"
        );
        // Both sides of both exchanges settled, and the parent's program
        // got the child's answer.
        // The child's *program* returned into its own context; what
        // crossed to the parent is the child's answer — its final turn,
        // which is the exchange's other half.
        assert_eq!(returned(tree, child_leaf), json!("read PLAN.md"));
        assert_eq!(
            returned(tree, root_leaf(&session)),
            json!("done, read PLAN.md"),
            "the child's answer reached the parent's program"
        );
        assert_eq!(
            kinds(tree, child_leaf),
            [
                "Agent", "Post", "Turn", "Call", "Result", "Return", "Console", "Turn", "Answer"
            ],
            "the child asked, was answered, finished, and answered in turn"
        );
        // Nothing is left owed anywhere.
        for (_, leaf) in tree.branches() {
            assert!(
                tree.spine_at(leaf).context().open.is_empty(),
                "branch at #{} still owes an answer",
                leaf.as_u64()
            );
        }
    }
}
