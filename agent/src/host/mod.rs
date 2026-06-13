//! The host layer (8_HARNESS Step 5): one main-loop thread owns the
//! tree and the frame step machines and `recv()`s a single
//! `std::sync::mpsc` inbox of one unified message enum. Worker threads
//! exist only for blocking IO — one per in-flight LLM completion,
//! spawn-per-call for tool fan-out — and only ever hold a cloned
//! `Sender`. VM compute runs on the loop thread in fuel slices
//! (`StepInput::Tick`), with a `Continue` message re-enqueued between
//! slices so a hot program never starves other frames or the UI.
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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use crate::machine::{
    AgentState, LlmRequest, OutCall, SpawnFrame, StepInput, StepOutput, ToolResult,
};
use crate::types::{EventId, EventPayload, Message, Tree};

/// Instructions per VM slice on the loop thread (9_TUI decision 3).
pub const FUEL_SLICE: u64 = 100_000;

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
        frame: FrameId,
        thinking: bool,
        text: String,
    },
    LlmDone {
        frame: FrameId,
        result: Result<Message, String>,
    },
    ToolDone {
        frame: FrameId,
        invoke_id: u64,
        result: Result<serde_json::Value, String>,
    },
    /// Fuel-slice continuation, re-enqueued between slices.
    Continue {
        frame: FrameId,
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
    states: HashMap<FrameId, AgentState>,
    root: FrameId,
    /// Child frame → (caller frame, the caller's `agent` invoke id).
    parents: HashMap<FrameId, (FrameId, u64)>,
    registry: ToolRegistry,
    llm: Arc<Mutex<Box<dyn LlmClient>>>,
    rx: Receiver<LoopMsg>,
    tx: Sender<LoopMsg>,
    events: Sender<SessionEvent>,
    /// High-water mark of event ids already surfaced as `SessionEvent`s.
    emitted: u64,
    /// Frames whose VM the debugger paused: their `Continue` messages
    /// are parked in `starved` instead of ticking.
    paused: HashSet<FrameId>,
    starved: HashSet<FrameId>,
    done: bool,
}

impl Session {
    /// Open a session over `tree`: a fresh tree roots a new frame with
    /// `prompt`/`input`; a re-opened log resumes its lowest incomplete
    /// leaf (richer resume/fork UX is M4).
    pub fn new(
        mut tree: Tree,
        prompt: &str,
        input: serde_json::Value,
        registry: ToolRegistry,
        llm: Box<dyn LlmClient>,
        events: Sender<SessionEvent>,
    ) -> io::Result<Self> {
        let emitted = tree.id_counter;
        let mut state = if tree.events.is_empty() {
            AgentState::new_root(&mut tree, prompt, input)?
        } else {
            let mut leaves: Vec<EventId> =
                tree.list_leaves().into_iter().map(|(id, _)| id).collect();
            leaves.sort_by_key(|id| id.as_u64());
            let leaf = leaves
                .into_iter()
                .find(|id| !tree.spine_at(*id).is_complete())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "every spine in this log is complete",
                    )
                })?;
            AgentState::with_spine(tree.spine_at(leaf))
        };
        state.set_effectful_tools(registry.effectful_names());
        state.set_dialect_card(dialect_card(&registry));
        let root = frame_start_id(&tree, state.spine.leaf_id);

        let (tx, rx) = channel();
        let mut session = Session {
            tree,
            states: HashMap::from([(root, state)]),
            root,
            parents: HashMap::new(),
            registry,
            llm: Arc::new(Mutex::new(llm)),
            rx,
            tx,
            events,
            emitted,
            paused: HashSet::new(),
            starved: HashSet::new(),
            done: false,
        };
        session.emit_new(root); // a fresh tree's FrameStart
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

    pub fn root(&self) -> FrameId {
        self.root
    }

    pub fn state(&self, frame: FrameId) -> Option<&AgentState> {
        self.states.get(&frame)
    }

    /// Active frames, for frame lists (id, machine status).
    pub fn frames(&self) -> Vec<(FrameId, &'static str)> {
        let mut out: Vec<(FrameId, &'static str)> = self
            .states
            .iter()
            .map(|(id, s)| (*id, s.status()))
            .collect();
        out.sort_by_key(|(id, _)| id.as_u64());
        out
    }

    /// Run to completion: until the root frame finishes or `Shutdown`.
    pub fn run(mut self) -> Self {
        while self.pump_one() {}
        self
    }

    /// Block for one inbox message and handle it; `false` once the
    /// session is over.
    pub fn pump_one(&mut self) -> bool {
        if self.done {
            return false;
        }
        match self.rx.recv() {
            Ok(msg) => {
                self.on_msg(msg);
                !self.done
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

    /// Whether the conversation is over (root frame done / `Shutdown`).
    /// The attached TUI keeps rendering past this for post-mortem
    /// reading; `run()` exits on it.
    pub fn is_done(&self) -> bool {
        self.done
    }

    // ── debugger controls (privileged: same thread as the loop) ──────

    /// Pause/resume a frame's VM. Pausing parks its fuel-slice
    /// continuations; resuming re-enqueues a parked one.
    pub fn set_paused(&mut self, frame: FrameId, paused: bool) {
        if paused {
            self.paused.insert(frame);
        } else if self.paused.remove(&frame) && self.starved.remove(&frame) {
            let _ = self.tx.send(LoopMsg::Continue { frame });
        }
    }

    pub fn is_paused(&self, frame: FrameId) -> bool {
        self.paused.contains(&frame)
    }

    /// Run one slice of at most `fuel` instructions on a (paused)
    /// frame — the debugger's step keys.
    pub fn step_paused(&mut self, frame: FrameId, fuel: u64) {
        let _ = self.step_frame(frame, StepInput::Tick { fuel });
    }

    fn on_msg(&mut self, msg: LoopMsg) {
        if let Err(e) = self.dispatch(msg) {
            self.emit(SessionEvent::Error {
                frame: None,
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
                    // Steering a busy frame is M2's host-injected
                    // condition; until then the command is rejected.
                    self.emit(SessionEvent::Error {
                        frame: Some(root),
                        message: format!("frame is busy ({})", state.status()),
                    });
                    return Ok(());
                }
                self.step_frame(root, StepInput::UserTurn(text))
            }
            LoopMsg::LlmChunk {
                frame,
                thinking,
                text,
            } => {
                self.emit(SessionEvent::Chunk {
                    frame,
                    thinking,
                    text,
                });
                Ok(())
            }
            LoopMsg::LlmDone { frame, result } => match result {
                Ok(message) => self.step_frame(frame, StepInput::LlmResponse(message)),
                Err(message) => {
                    self.emit(SessionEvent::Error {
                        frame: Some(frame),
                        message: message.clone(),
                    });
                    match self.parents.get(&frame).copied() {
                        // A dead child rejects the caller's `agent` call.
                        Some((parent, invoke_id)) => {
                            let _ = self.tx.send(LoopMsg::ToolDone {
                                frame: parent,
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
                frame,
                invoke_id,
                result,
            } => self.step_frame(
                frame,
                StepInput::ToolResults(vec![ToolResult { invoke_id, result }]),
            ),
            LoopMsg::Continue { frame } => {
                if self.paused.contains(&frame) {
                    // Park the slice; `set_paused(false)` re-enqueues it.
                    self.starved.insert(frame);
                    return Ok(());
                }
                self.step_frame(frame, StepInput::Tick { fuel: FUEL_SLICE })
            }
            // Handled by `pump_until`; harmless if one reaches `run()`.
            LoopMsg::Ui(_) => Ok(()),
        }
    }

    fn step_frame(&mut self, frame: FrameId, input: StepInput) -> io::Result<()> {
        let Some(state) = self.states.get_mut(&frame) else {
            return Ok(());
        };
        let outputs = state.step(&mut self.tree, input)?;
        self.emit_new(frame);
        self.process(frame, outputs)
    }

    fn process(&mut self, frame: FrameId, outputs: Vec<StepOutput>) -> io::Result<()> {
        for output in outputs {
            match output {
                StepOutput::LlmRequest(request) => self.spawn_llm(frame, request),
                StepOutput::ToolCalls(calls) => self.spawn_tools(frame, calls),
                StepOutput::SpawnFrames(spawns) => {
                    for spawn in spawns {
                        self.spawn_child(frame, spawn)?;
                    }
                }
                StepOutput::FrameDone(result) => match self.parents.get(&frame).copied() {
                    Some((parent, invoke_id)) => {
                        let _ = self.tx.send(LoopMsg::ToolDone {
                            frame: parent,
                            invoke_id,
                            result: guard_size(Ok(result)),
                        });
                    }
                    None => self.done = true,
                },
                StepOutput::Working => {
                    let _ = self.tx.send(LoopMsg::Continue { frame });
                }
            }
        }
        Ok(())
    }

    /// One worker thread per in-flight completion (blocking reads live
    /// there; chunks and the final message come back through the inbox).
    fn spawn_llm(&self, frame: FrameId, request: LlmRequest) {
        let llm = Arc::clone(&self.llm);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let mut on_chunk = |chunk: LlmChunk| {
                let (thinking, text) = match chunk {
                    LlmChunk::Text(t) => (false, t),
                    LlmChunk::Thinking(t) => (true, t),
                };
                let _ = tx.send(LoopMsg::LlmChunk {
                    frame,
                    thinking,
                    text,
                });
            };
            let result = match llm.lock() {
                Ok(mut client) => client.complete(&request, &mut on_chunk),
                Err(_) => Err("llm client mutex poisoned".into()),
            };
            let _ = tx.send(LoopMsg::LlmDone { frame, result });
        });
    }

    /// Spawn-per-call fan-out; completions arrive at the inbox in
    /// whatever order the tools finish — that arrival order is the
    /// logged resolution order.
    fn spawn_tools(&self, frame: FrameId, calls: Vec<OutCall>) {
        for call in calls {
            match self.registry.get(&call.name) {
                Some(def) => {
                    let def = Arc::clone(def);
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let result = guard_size((def.handler)(call.args));
                        let _ = tx.send(LoopMsg::ToolDone {
                            frame,
                            invoke_id: call.invoke_id,
                            result,
                        });
                    });
                }
                None => {
                    let _ = self.tx.send(LoopMsg::ToolDone {
                        frame,
                        invoke_id: call.invoke_id,
                        result: Err(format!("unknown tool `{}`", call.name)),
                    });
                }
            }
        }
    }

    /// The `agent` tool: a `SpawnFrame` becomes a child `AgentState` on
    /// a branch rooted at the caller's call site.
    fn spawn_child(&mut self, parent: FrameId, spawn: SpawnFrame) -> io::Result<()> {
        let SpawnFrame {
            invoke_id,
            prompt,
            input,
        } = spawn;
        let call_site = self.states[&parent].spine.leaf_id;
        let mut child = AgentState::new_child(&mut self.tree, call_site, prompt, input)?;
        child.set_effectful_tools(self.registry.effectful_names());
        child.set_dialect_card(dialect_card(&self.registry));
        let child_id = child.spine.leaf_id; // the FrameStart it was rooted at
        self.emit_new(child_id);
        self.parents.insert(child_id, (parent, invoke_id));
        let outputs = child.kickoff();
        self.states.insert(child_id, child);
        self.process(child_id, outputs)
    }

    /// Surface every newly logged event as a `SessionEvent`, attributed
    /// to the frame just stepped (a `FrameStart` is its own frame).
    fn emit_new(&mut self, frame: FrameId) {
        while self.emitted < self.tree.id_counter {
            self.emitted += 1;
            let id = EventId::new(self.emitted);
            let Some(event) = self.tree.events.get(&id) else {
                continue;
            };
            let owner = if matches!(event.payload, EventPayload::FrameStart { .. }) {
                id
            } else {
                frame
            };
            let event = event.clone();
            let _ = self.events.send(SessionEvent::Event {
                frame: owner,
                event,
            });
        }
    }

    fn emit(&mut self, event: SessionEvent) {
        let _ = self.events.send(event);
    }
}

/// The innermost `FrameStart` at or above `leaf`.
fn frame_start_id(tree: &Tree, leaf: EventId) -> FrameId {
    let mut current = leaf;
    loop {
        let Some(event) = tree.events.get(&current) else {
            return leaf;
        };
        if matches!(event.payload, EventPayload::FrameStart { .. }) {
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

    fn tool(
        name: &str,
        effectful: bool,
        handler: impl Fn(serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync + 'static,
    ) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: String::new(),
            input_schema: json!({ "type": "array" }),
            output_schema: json!({}),
            effectful,
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

    /// Payload kinds of one frame's spine segment, log order.
    fn kinds(tree: &Tree, leaf: EventId) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut current = leaf;
        loop {
            let event = &tree.events[&current];
            out.push(match &event.payload {
                EventPayload::FrameStart { .. } => "FrameStart",
                EventPayload::FrameResult { .. } => "FrameResult",
                EventPayload::Message(Message::User { .. }) => "User",
                EventPayload::Message(Message::Assistant { .. }) => "Assistant",
                EventPayload::Message(Message::System { .. }) => "System",
                EventPayload::Message(Message::Tool { .. }) => "Tool",
                EventPayload::Invoke { .. } => "Invoke",
                EventPayload::ProgramResult { .. } => "ProgramResult",
                EventPayload::Label(_) => "Label",
                EventPayload::TextChunk(_) | EventPayload::ThinkingChunk(_) => "Chunk",
            });
            if matches!(event.payload, EventPayload::FrameStart { .. }) {
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
            .frame()
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
                "FrameStart",
                "User",
                "Assistant",
                "Invoke",
                "Invoke",
                "ProgramResult",
                "Tool",
                "Assistant",
                "FrameResult"
            ]
        );
        // Both fan-out results landed (order is completion order).
        let invokes: Vec<serde_json::Value> = session
            .tree()
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Invoke { result, .. } => Some(result.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(invokes.len(), 2);
        assert!(invokes.contains(&json!("alpha")) && invokes.contains(&json!("beta")));
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
            &mut self,
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
        registry.register(tool("fetch_page", false, |_| Ok(json!(null))));
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            inner: ScriptedLlm::new([scripted_text("done")]),
            seen: std::sync::Arc::clone(&seen),
        };
        let (tx, _rx) = channel();
        let session = Session::new(
            Tree::new(None),
            "frame prompt here",
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
            system.contains("frame prompt here"),
            "frame prompt follows the card"
        );
    }

    #[test]
    fn fanout_logs_in_completion_order() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("slow", false, |_| {
            std::thread::sleep(Duration::from_millis(40));
            Ok(json!("slow"))
        }));
        registry.register(tool("fast", false, |_| Ok(json!("fast"))));
        let script = vec![
            scripted_program(
                "c1",
                "const s = tools.slow(); const f = tools.fast(); return [await s, await f];",
            ),
            scripted_text("done"),
        ];
        let (session, _) = run_session(registry, script, "race them");

        let mut invokes: Vec<(u64, serde_json::Value)> = session
            .tree()
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::Invoke { result, .. } => Some((e.id.as_u64(), result.clone())),
                _ => None,
            })
            .collect();
        invokes.sort_by_key(|(id, _)| *id);
        let order: Vec<&serde_json::Value> = invokes.iter().map(|(_, v)| v).collect();
        assert_eq!(
            order,
            [&json!("fast"), &json!("slow")],
            "inbox arrival order is the logged resolution order"
        );
        // The program still saw its own await order.
        assert!(tool_texts(&session)[0].contains(r#"returned: ["slow","fast"]"#));
    }

    #[test]
    fn effectful_flag_warns_in_artifact_menu() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("send_email", true, |_| Ok(json!({ "sent": true }))));
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
            report.contains("already happened; calling again repeats the effect"),
            "{report}"
        );
    }

    #[test]
    fn oversized_result_is_guarded_before_the_log() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("big", false, |_| Ok(json!("x".repeat(70_000)))));
        let script = vec![
            scripted_program(
                "c1",
                r#"try { return await tools.big(); } catch (e) { return "rejected: " + e; }"#,
            ),
            scripted_text("done"),
        ];
        let (session, _) = run_session(registry, script, "fetch something huge");

        let invoke = session
            .tree()
            .events
            .values()
            .find_map(|e| match &e.payload {
                EventPayload::Invoke { result, .. } => Some(result.clone()),
                _ => None,
            })
            .expect("the call is still logged");
        let error = invoke["error"].as_str().unwrap();
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
        assert!(!spine.contains(&"Invoke"), "{spine:?}");
        assert_eq!(spine.last(), Some(&"FrameResult"));
    }

    /// M2: a trapped runtime error reports, and the rewrite restart reuses
    /// the already-logged tool result by id (`tools.tool_result`) instead
    /// of repeating the call — served from the log, no second Invoke.
    #[test]
    fn trapped_error_rewrite_reuses_artifact_through_the_session() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("fetch", false, |_| Ok(json!("DATA"))));
        // Event ids are deterministic: FrameStart 1, User 2, Assistant 3,
        // the fetch Invoke 4 — so the rewrite can name `tool_result(4)`.
        let script = vec![
            scripted_program(
                "c1",
                r#"await tools.fetch("expensive"); const v = null; return v.x;"#,
            ),
            scripted_program("c2", "return await tools.tool_result(4);"),
            scripted_text("done"),
        ];
        let (session, _) = run_session(registry, script, "fetch then trip");

        // The fetch really is artifact #4 (guards the hardcoded id above).
        let fetch_invoke = session
            .tree()
            .events
            .values()
            .find(|e| matches!(&e.payload, EventPayload::Invoke { name, .. } if name == "fetch"))
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
            .filter(|e| matches!(&e.payload, EventPayload::Invoke { name, .. } if name == "fetch"))
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
    fn agent_tool_spawns_child_frame_and_joins() {
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
                matches!(&e.payload, EventPayload::FrameStart { prompt, .. } if prompt == "child task")
            })
            .expect("child FrameStart");
        let child_leaf = tree
            .list_leaves()
            .into_iter()
            .map(|(id, _)| id)
            .find(|id| *id != root_leaf(&session))
            .unwrap();
        assert_eq!(
            kinds(tree, child_leaf),
            ["FrameStart", "Assistant", "FrameResult"]
        );

        // The join: the child's result is the caller's logged artifact
        // and reaches the caller's program.
        assert!(kinds(tree, root_leaf(&session)).contains(&"Invoke"));
        assert!(tool_texts(&session)[0].contains(r#"returned: "child says 42""#));

        // Child events were attributed to the child frame.
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::Event { frame, event } if *frame == child_start.id && event.id == child_start.id
        )));
    }

    /// M3: `Promise.all` over two `tools.agent` calls spawns both child
    /// frames concurrently (one fan-out batch, two branches) and joins
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

        // Both child frames were rooted, each with the prompt it was given.
        let child_prompts: HashSet<String> = tree
            .events
            .values()
            .filter_map(|e| match &e.payload {
                EventPayload::FrameStart { prompt, .. } => Some(prompt.clone()),
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
                ["FrameStart", "Assistant", "FrameResult"]
            );
        }

        // The caller logged both agent calls as artifacts on its spine…
        let invokes = kinds(tree, root_leaf(&session))
            .iter()
            .filter(|k| **k == "Invoke")
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
}
