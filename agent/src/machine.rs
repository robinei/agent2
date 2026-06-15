//! Sans-io frame step machine (8_HARNESS Step 3).
//!
//! One `AgentState` drives one frame: a deterministic, IO-free core the
//! host feeds with `StepInput`s and drains of `StepOutput`s. The host
//! owns the LLM API, tool execution, subagent loops, and scheduling;
//! the core never blocks. VM compute is host-fueled: the machine runs
//! one `step(fuel)` slice per `Tick` and reports `Working` when it
//! wants another, so a hot program can't starve the host loop.

use std::collections::HashMap;
use std::io;

use interp::{
    Diagnostic, InvokeCall, PromisePtr, RcStr, ResumeMode, StepResult, VM, VMError, Value, compile,
};

use crate::host::ProgramStatus;
use crate::report::{
    Artifact, CompletionReport, ConditionReport, PAYLOAD_MAX_BYTES, ResumeKind, clip, preview,
};
use crate::types::*;

/// Tool names offered to the LLM. `run_program` is the primary tool;
/// `resume` appears only while suspended on a resumable condition.
pub const TOOL_RUN_PROGRAM: &str = "run_program";
pub const TOOL_RESUME: &str = "resume";

/// A full tool definition offered to the LLM: what every client
/// serializes into its wire format (name + JSON-schema'd parameters).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON schema of the tool-call arguments object.
    pub parameters: serde_json::Value,
}

/// The `run_program` definition — the single place its schema lives.
pub fn run_program_spec() -> ToolSpec {
    ToolSpec {
        name: TOOL_RUN_PROGRAM.into(),
        description: "Run a complete JavaScript program (harness dialect, per the system \
                      message). The tool result is a report: the returned value, or a \
                      condition with restart options."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "the complete program source"
                },
                "attachments": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "optional name→content map for authored bodies \
                                    (file contents, large blobs). The program reads \
                                    them as the read-only const `attachments.<name>` — \
                                    keep them out of `source` so it stays small and \
                                    the content is inert (no JS escaping)."
                }
            },
            "required": ["source"]
        }),
    }
}

/// The `resume` definition — offered only while suspended on a
/// resumable condition.
pub fn resume_spec() -> ToolSpec {
    ToolSpec {
        name: TOOL_RESUME.into(),
        description: "Resume the suspended program: execution continues with `value` as \
                      the result of the failed operation (or of the raise expression)."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "value": {
                    "description": "the JSON value to resume with"
                }
            },
            "required": ["value"]
        }),
    }
}

/// Iteration cap for one `Tick`: each extra round requires a synchronous
/// artifact fetch (`tools.tool_result`) to have unblocked the program,
/// but a pathological program could chain those forever.
const MAX_PUMP_ROUNDS: usize = 100;

/// Default budget for a frame's *answer* — the one value that deliberately
/// crosses into a mind's context: a program's `return` (into its own
/// frame) and a subagent's final turn (into its caller). Sized to a
/// typical source file so an ordinary read or summary lands in one shot
/// (DESIGN.md "The one exception"; 12_ANSWERS). The full value is always a
/// fetchable artifact; only the context copy is truncated past this. A
/// caller may raise a child's budget via `agent({ budget })`.
const DEFAULT_ANSWER_BUDGET: usize = 64 * 1024;

/// How many times a subagent whose final answer exceeds its budget is
/// re-prompted to tighten it before the host truncates it deterministically.
const ANSWER_RETRY_LIMIT: u8 = 1;

/// A `create_file`/`replace_file` whose inline content exceeds this draws
/// the attachments nudge (when the run passed no `attachments`): more than
/// a snippet belongs in the `attachments` channel, not the program source.
const INLINE_BODY_ADVICE_BYTES: usize = 512;

pub enum StepInput {
    /// A user message. Valid while idle; arriving mid-program it becomes
    /// a host-injected condition — deferred to M2 (panics until then).
    UserTurn(String),
    /// The assistant's turn (logged verbatim; tool calls dispatched).
    LlmResponse(Message),
    /// Completed host tool calls, in resolution order.
    ToolResults(Vec<ToolResult>),
    /// A child frame's `FrameResult` arriving at its call site.
    SubagentResult {
        invoke_id: u64,
        result: serde_json::Value,
    },
    /// Run one VM slice of at most `fuel` instructions.
    Tick { fuel: u64 },
}

pub struct ToolResult {
    pub invoke_id: u64,
    /// `Err` rejects the program-side promise with the message.
    pub result: Result<serde_json::Value, String>,
}

#[derive(Debug)]
pub enum StepOutput {
    /// Send this to the LLM and feed the response back as `LlmResponse`.
    LlmRequest(LlmRequest),
    /// Execute these tools (any order/concurrency); feed back as
    /// `ToolResults` in completion order.
    ToolCalls(Vec<OutCall>),
    /// Spawn child frames; feed each result back as `SubagentResult`.
    SpawnFrames(Vec<SpawnFrame>),
    /// The frame completed; its `FrameResult` is logged.
    FrameDone(serde_json::Value),
    /// The root frame produced a final answer but does *not* complete:
    /// the top conversation never ends, it yields the turn back to the
    /// user. No `FrameResult` is logged (the spine stays appendable); the
    /// frame goes idle awaiting the next `UserTurn`.
    Yielded,
    /// The VM wants another `Tick`.
    Working,
}

#[derive(Debug)]
pub struct LlmRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug)]
pub struct OutCall {
    pub invoke_id: u64,
    pub name: String,
    /// Positional arguments as a JSON array.
    pub args: serde_json::Value,
}

#[derive(Debug)]
pub struct SpawnFrame {
    pub invoke_id: u64,
    pub prompt: String,
    pub input: serde_json::Value,
    /// The child's answer budget (`agent({ budget })`); `None` → default.
    pub budget: Option<usize>,
}

/// One program execution: the `run_program` tool call being served.
struct Run {
    /// The `run_program` Assistant event id — the program block's stable
    /// key across `resume` (decision 2), carried by `ProgramStatus`.
    program_id: EventId,
    /// LLM tool-call id the eventual tool result answers.
    call_id: String,
    vm: VM,
    /// Event-id high-water mark when the run started: artifacts logged
    /// after it are "new" in this run's completion report.
    started_at: u64,
    /// Whether this run was given a non-empty `attachments` map — used to
    /// suppress the inline-body nudge once the model is using the channel.
    had_attachments: bool,
}

/// Why a run is suspended, and how `resume(value)` re-enters it.
enum Suspension {
    /// `raise(name, payload)` — resume via `VM::resume_raise`.
    Raise,
    /// Trapped VM error — resume via `VM::resume_with` when the error
    /// is `PushValueThenContinue`.
    Trapped(VMError),
}

enum Phase {
    /// No in-flight request; waiting for a `UserTurn` (or `kickoff`).
    Idle,
    /// An `LlmRequest` is out; waiting for `LlmResponse`.
    AwaitingLlm,
    /// A program is executing (waiting for `Tick`/`ToolResults`).
    Running(Run),
    /// A condition report went out; waiting for the restart choice.
    Suspended(Run, Suspension),
    /// `FrameResult` logged; terminal.
    Done,
}

struct PendingCall {
    name: String,
    args: serde_json::Value,
    promise: PromisePtr,
    /// Which run issued it: results from an abandoned run are still
    /// logged as artifacts (the physics happened) but not delivered.
    generation: u64,
}

pub struct AgentState {
    pub spine: Spine,
    /// The session's top frame: the user-facing conversation. It never
    /// completes — a final no-tool-call turn *yields* to the user
    /// instead of logging a `FrameResult`. Child (subagent) frames are
    /// not root: they complete and return to their caller.
    is_root: bool,
    phase: Phase,
    invoke_counter: u64,
    generation: u64,
    pending: HashMap<u64, PendingCall>,
    /// The dialect card (8_HARNESS Step 6), prepended to every system
    /// message ahead of the frame prompt (host-fed, registry-generated).
    dialect_card: String,
    /// The most recently finished/abandoned run's VM, kept so the
    /// debugger's sticky panes can show final state post-mortem
    /// (9_TUI Step 4). Never executed again.
    last_vm: Option<VM>,
    /// Program-block status transitions logged during the current
    /// `step`, drained by the host into `SessionEvent::ProgramStatus`
    /// (decision 5). Buffered (not a `StepOutput`) so two transitions in
    /// one step — a rewrite abandoning the old run as a new one starts —
    /// both surface, and so the sans-io output set is untouched.
    status_transitions: Vec<(EventId, ProgramStatus)>,
    /// Byte budget for this frame's *answer* into context (decisions 2, 6):
    /// program `return`s and (for a subagent) the final turn. Seeded from
    /// the spawning `agent({ budget })` or `DEFAULT_ANSWER_BUDGET`.
    answer_budget: usize,
    /// Re-prompts spent tightening an over-budget final answer (decision 4).
    answer_retries: u8,
}

enum SuspendCause {
    Raise {
        condition: String,
        payload: Option<Value>,
    },
    Trapped(VMError),
}

impl AgentState {
    /// Root frame of a tree.
    pub fn new_root(
        tree: &mut Tree,
        prompt: impl Into<String>,
        input: serde_json::Value,
    ) -> io::Result<Self> {
        let spine = tree.start_frame(None, prompt, input)?;
        Ok(Self::with_spine(spine))
    }

    /// Child frame branching at `call_site` on the caller's spine (the
    /// host maps each `SpawnFrame` to one of these).
    pub fn new_child(
        tree: &mut Tree,
        call_site: EventId,
        prompt: impl Into<String>,
        input: serde_json::Value,
        budget: Option<usize>,
    ) -> io::Result<Self> {
        let spine = tree.start_frame(Some(call_site), prompt, input)?;
        let mut state = Self::with_spine(spine);
        state.is_root = false; // a subagent frame completes and returns
        state.answer_budget = budget.unwrap_or(DEFAULT_ANSWER_BUDGET);
        Ok(state)
    }

    /// Resume an existing spine (re-opened log). This is the session's
    /// top frame — `is_root` — whether freshly rooted (`new_root`) or
    /// re-anchored on resume (`open_at`); `new_child` clears the flag.
    pub fn with_spine(spine: Spine) -> Self {
        AgentState {
            spine,
            is_root: true,
            phase: Phase::Idle,
            invoke_counter: 0,
            generation: 0,
            pending: HashMap::new(),
            dialect_card: String::new(),
            last_vm: None,
            status_transitions: Vec::new(),
            answer_budget: DEFAULT_ANSWER_BUDGET,
            answer_retries: 0,
        }
    }

    /// The dialect card rendered as the root of the system message
    /// (the host generates it from the tool registry).
    pub fn set_dialect_card(&mut self, card: String) {
        self.dialect_card = card;
    }

    /// Drain the program-status transitions logged during the just-run
    /// `step`; the host turns each into a `SessionEvent::ProgramStatus`.
    pub fn take_status_transitions(&mut self) -> Vec<(EventId, ProgramStatus)> {
        std::mem::take(&mut self.status_transitions)
    }

    /// Record a program-block status transition for the host to surface.
    fn note_status(&mut self, program: EventId, status: ProgramStatus) {
        self.status_transitions.push((program, status));
    }

    /// Materialize this frame's system prompt once (decision 4): the
    /// assembled dialect card + prompt + input, logged as the spine's
    /// first `Message::System`. Idempotent — a spine that already carries
    /// a leading `System` (a re-opened log) is left untouched, so the
    /// stored prompt replays verbatim even as the registry's card evolves.
    fn ensure_system(&mut self, tree: &mut Tree) -> io::Result<()> {
        if matches!(
            self.spine.frame().messages.first(),
            Some(Message::System { .. })
        ) {
            return Ok(());
        }
        let text = self.assemble_system();
        tree.append(
            &mut self.spine,
            EventPayload::Message(Message::System { text }),
        )?;
        Ok(())
    }

    /// Assemble the system prompt string: the dialect card, the frame
    /// prompt, then the frame input as a fenced JSON block.
    fn assemble_system(&self) -> String {
        let frame = self.spine.frame();
        let mut system = String::new();
        if !self.dialect_card.is_empty() {
            system.push_str(&self.dialect_card);
            system.push_str("\n\n");
        }
        system.push_str(&frame.prompt);
        if !frame.input.is_null() {
            system.push_str("\n\nInput:\n```json\n");
            system.push_str(&frame.input.to_string());
            system.push_str("\n```");
        }
        system
    }

    /// Whether the frame can accept a `UserTurn` right now.
    pub fn is_idle(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }

    /// One-word phase description for frame lists / status lines.
    pub fn status(&self) -> &'static str {
        match self.phase {
            Phase::Idle => "idle",
            Phase::AwaitingLlm => "awaiting llm",
            Phase::Running(_) => "running",
            Phase::Suspended(..) => "suspended",
            Phase::Done => "done",
        }
    }

    /// The VM the debugger TUI renders from (9_TUI dec. 4): the live
    /// one while a program runs or is suspended, else the last run's
    /// final state (sticky post-mortem panes).
    pub fn vm(&self) -> Option<&VM> {
        match &self.phase {
            Phase::Running(run) | Phase::Suspended(run, _) => Some(&run.vm),
            _ => self.last_vm.as_ref(),
        }
    }

    /// Whether `vm()` is the live, executing program (vs a post-mortem
    /// snapshot).
    pub fn vm_is_live(&self) -> bool {
        matches!(self.phase, Phase::Running(_) | Phase::Suspended(..))
    }

    /// Start the conversation without a user turn — how child frames
    /// begin (their input arrived in `FrameStart`).
    pub fn kickoff(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        assert!(matches!(self.phase, Phase::Idle), "kickoff on a busy frame");
        self.ensure_system(tree)?;
        self.phase = Phase::AwaitingLlm;
        Ok(vec![self.render_request()])
    }

    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    pub fn step(&mut self, tree: &mut Tree, input: StepInput) -> io::Result<Vec<StepOutput>> {
        match input {
            StepInput::UserTurn(text) => self.on_user_turn(tree, text),
            StepInput::LlmResponse(message) => self.on_llm_response(tree, message),
            StepInput::ToolResults(batch) => self.on_tool_results(tree, batch),
            StepInput::SubagentResult { invoke_id, result } => self.on_tool_results(
                tree,
                vec![ToolResult {
                    invoke_id,
                    result: Ok(result),
                }],
            ),
            StepInput::Tick { fuel } => self.on_tick(tree, fuel),
        }
    }

    // ── input handlers ──────────────────────────────────────────────

    fn on_user_turn(&mut self, tree: &mut Tree, text: String) -> io::Result<Vec<StepOutput>> {
        match self.phase {
            Phase::Idle => {}
            Phase::Running(_) | Phase::Suspended(..) => {
                // Known hole (8_HARNESS): a mid-program UserTurn becomes a
                // host-injected condition. Lands with M2's restart work.
                panic!("mid-program user turns are not implemented yet (M2)");
            }
            Phase::AwaitingLlm => panic!("user turn while an LLM request is in flight"),
            Phase::Done => panic!("user turn on a completed frame"),
        }
        self.ensure_system(tree)?;
        tree.append(
            &mut self.spine,
            EventPayload::Message(Message::User { text }),
        )?;
        self.phase = Phase::AwaitingLlm;
        Ok(vec![self.render_request()])
    }

    fn on_llm_response(
        &mut self,
        tree: &mut Tree,
        message: Message,
    ) -> io::Result<Vec<StepOutput>> {
        assert!(
            matches!(self.phase, Phase::AwaitingLlm | Phase::Suspended(..)),
            "LlmResponse with no request in flight"
        );
        let tool_calls = match &message {
            Message::Assistant { tool_calls, .. } => tool_calls.clone(),
            other => panic!("LlmResponse must be an Assistant message, got {other:?}"),
        };
        let assistant_id = tree.append(&mut self.spine, EventPayload::Message(message))?;

        let Some(call) = tool_calls.first().cloned() else {
            // No tool call: the assistant's text completes the frame.
            return self.finish_frame(tree);
        };
        for extra in &tool_calls[1..] {
            self.log_tool_error(tree, extra, "one tool call per turn; this call was ignored")?;
        }

        match call.name.as_str() {
            TOOL_RUN_PROGRAM => {
                let Some(source) = call.arguments.get("source").and_then(|s| s.as_str()) else {
                    self.log_tool_error(tree, &call, "run_program needs a `source` string")?;
                    self.phase = Phase::AwaitingLlm;
                    return Ok(vec![self.render_request()]);
                };
                // `attachments` is this run's authored content (name → string),
                // seeded as the program's `attachments` const. A malformed
                // shape is a cheap repair loop, like a missing source.
                let attachments = match attachments_from_args(&call.arguments) {
                    Ok(a) => a,
                    Err(msg) => {
                        self.log_tool_error(tree, &call, &msg)?;
                        self.phase = Phase::AwaitingLlm;
                        return Ok(vec![self.render_request()]);
                    }
                };
                // A rewrite abandons any suspended VM — never the physics:
                // in-flight calls stay pending and their results are still
                // logged as artifacts when they arrive (the generation bump
                // stops delivery to the dead VM). The abandoned VM is kept
                // for post-mortem rendering.
                if let Phase::Suspended(run, _) = std::mem::replace(&mut self.phase, Phase::Idle) {
                    // The abandoned program never completed — Failed.
                    self.note_status(run.program_id, ProgramStatus::Failed);
                    self.last_vm = Some(run.vm);
                }
                self.generation += 1;
                match self.start_program(assistant_id, source, attachments, call.id.clone()) {
                    Ok(run) => {
                        self.phase = Phase::Running(run);
                        self.note_status(assistant_id, ProgramStatus::Running);
                        Ok(vec![StepOutput::Working])
                    }
                    Err(report) => {
                        // Compile (or input-binding) error: a cheap repair
                        // loop — the report is the tool result, nothing ran.
                        tree.append(
                            &mut self.spine,
                            EventPayload::Message(Message::Tool {
                                name: TOOL_RUN_PROGRAM.into(),
                                call_id: call.id.clone(),
                                text: report,
                            }),
                        )?;
                        self.phase = Phase::AwaitingLlm;
                        Ok(vec![self.render_request()])
                    }
                }
            }
            TOOL_RESUME => {
                let value = call
                    .arguments
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                match std::mem::replace(&mut self.phase, Phase::Idle) {
                    Phase::Suspended(mut run, suspension) => {
                        let resumed = match &suspension {
                            Suspension::Raise => {
                                let v = json_arg(&mut run.vm, &value);
                                run.vm.resume_raise(v);
                                true
                            }
                            Suspension::Trapped(e) => match e.resume {
                                ResumeMode::PushValueThenContinue => {
                                    let v = json_arg(&mut run.vm, &value);
                                    run.vm
                                        .resume_with(e, v)
                                        .expect("resume audited as resumable");
                                    true
                                }
                                ResumeMode::NotResumable => false,
                            },
                        };
                        if resumed {
                            // The next report (completion or re-suspension)
                            // answers *this* resume call, not the original
                            // run_program — the chat transcript requires every
                            // assistant tool_call to be followed by a tool
                            // message bearing its id.
                            run.call_id = call.id.clone();
                            let program_id = run.program_id;
                            self.phase = Phase::Running(run);
                            // Same block resumes — keep the originating id.
                            self.note_status(program_id, ProgramStatus::Running);
                            Ok(vec![StepOutput::Working])
                        } else {
                            self.phase = Phase::Suspended(run, suspension);
                            self.log_tool_error(
                                tree,
                                &call,
                                "this condition is not resumable; use run_program",
                            )?;
                            Ok(vec![self.render_request()])
                        }
                    }
                    other => {
                        self.phase = other;
                        self.log_tool_error(tree, &call, "nothing to resume")?;
                        if !matches!(self.phase, Phase::Suspended(..)) {
                            self.phase = Phase::AwaitingLlm;
                        }
                        Ok(vec![self.render_request()])
                    }
                }
            }
            unknown => {
                self.log_tool_error(
                    tree,
                    &call,
                    &format!("unknown tool `{unknown}` (have: run_program, resume)"),
                )?;
                if !matches!(self.phase, Phase::Suspended(..)) {
                    self.phase = Phase::AwaitingLlm;
                }
                Ok(vec![self.render_request()])
            }
        }
    }

    fn on_tool_results(
        &mut self,
        tree: &mut Tree,
        batch: Vec<ToolResult>,
    ) -> io::Result<Vec<StepOutput>> {
        if matches!(self.phase, Phase::Done) {
            // The spine is complete (nothing may follow FrameResult);
            // results of stragglers the frame outlived are dropped.
            return Ok(Vec::new());
        }
        let mut delivered = false;
        for tr in batch {
            let Some(p) = self.pending.remove(&tr.invoke_id) else {
                continue; // unknown or duplicate — nothing to log
            };
            // Resolution order is arrival order: log now, result included.
            let logged = match &tr.result {
                Ok(v) => v.clone(),
                Err(msg) => serde_json::json!({ "error": msg }),
            };
            tree.append(
                &mut self.spine,
                EventPayload::Invoke {
                    name: p.name,
                    args: p.args,
                    result: logged,
                },
            )?;

            // Deliver only into the run that issued the call.
            if p.generation != self.generation {
                continue;
            }
            let vm = match &mut self.phase {
                Phase::Running(run) | Phase::Suspended(run, _) => &mut run.vm,
                _ => continue,
            };
            match tr.result {
                Ok(v) => {
                    let val = json_arg(vm, &v);
                    vm.resolve_promise(p.promise, val)
                        .expect("pending promise is settleable");
                }
                Err(msg) => {
                    let val = Value::String(RcStr::from(msg.as_str()));
                    vm.reject_promise(p.promise, val)
                        .expect("pending promise is settleable");
                }
            }
            delivered = true;
        }
        // A suspended run stays suspended (results land for later); a
        // running one can make progress now.
        if delivered && matches!(self.phase, Phase::Running(_)) {
            Ok(vec![StepOutput::Working])
        } else {
            Ok(Vec::new())
        }
    }

    fn on_tick(&mut self, tree: &mut Tree, fuel: u64) -> io::Result<Vec<StepOutput>> {
        if !matches!(self.phase, Phase::Running(_)) {
            return Ok(Vec::new());
        }
        self.pump(tree, fuel)
    }

    // ── program driving ─────────────────────────────────────────────

    /// Compile + bind the host consts (`input` from the frame, `attachments`
    /// from this run). `Err` is the rendered repair-loop report.
    fn start_program(
        &mut self,
        program_id: EventId,
        source: &str,
        attachments: serde_json::Value,
        call_id: String,
    ) -> Result<Run, String> {
        let program = compile(source).map_err(|diags| render_diags(source, &diags))?;
        let had_attachments = attachments.as_object().is_some_and(|m| !m.is_empty());
        let vm = VM::for_program_with(program, self.spine.frame().input.clone(), attachments)
            .map_err(|e| format!("program setup failed: {}", e.message))?;
        Ok(Run {
            program_id,
            call_id,
            vm,
            started_at: self.spine.leaf_id.as_u64(),
            had_attachments,
        })
    }

    /// Drive the VM until it blocks on the host, suspends, finishes, or
    /// runs out of fuel. Each round runs one `step(fuel)` slice; only a
    /// synchronous unblock (an artifact fetch answered from the log)
    /// earns another round.
    fn pump(&mut self, tree: &mut Tree, fuel: u64) -> io::Result<Vec<StepOutput>> {
        let mut out = Vec::new();
        for _ in 0..MAX_PUMP_ROUNDS {
            let Phase::Running(run) = &mut self.phase else {
                unreachable!("pump outside Running");
            };
            match run.vm.step(fuel) {
                Ok(StepResult::OutOfFuel) => {
                    out.push(StepOutput::Working);
                    return Ok(out);
                }
                Ok(StepResult::Pending { calls }) => {
                    let progressed = self.dispatch_calls(tree, calls, &mut out);
                    if !progressed {
                        return Ok(out); // blocked on the host now
                    }
                }
                Ok(StepResult::Done { value, unstarted }) => {
                    return self.finish_program(tree, value, unstarted, out);
                }
                Ok(StepResult::Raise { condition, payload }) => {
                    return self.suspend(tree, SuspendCause::Raise { condition, payload }, out);
                }
                Err(e) => {
                    return self.suspend(tree, SuspendCause::Trapped(e), out);
                }
            }
        }
        out.push(StepOutput::Working);
        Ok(out)
    }

    /// Classify one `Pending` batch: artifact fetches are answered from
    /// the log immediately (returns true if any were — the program can
    /// run again), `tools.agent` becomes `SpawnFrames`, everything else
    /// becomes `ToolCalls`.
    fn dispatch_calls(
        &mut self,
        tree: &Tree,
        calls: Vec<InvokeCall>,
        out: &mut Vec<StepOutput>,
    ) -> bool {
        let mut tool_calls = Vec::new();
        let mut spawns = Vec::new();
        let mut progressed = false;

        for call in calls {
            match call.name.as_str() {
                "tool_result" => {
                    let fetched = self.fetch_artifact(tree, &call);
                    let vm = self.running_vm();
                    match fetched {
                        Ok(json) => {
                            let v = json_arg(vm, &json);
                            vm.resolve_promise(call.promise, v).expect("fresh promise");
                        }
                        Err(msg) => {
                            let v = Value::String(RcStr::from(msg.as_str()));
                            vm.reject_promise(call.promise, v).expect("fresh promise");
                        }
                    }
                    progressed = true;
                }
                "agent" => {
                    let arg = {
                        let vm = self.running_vm();
                        call.args
                            .first()
                            .map(|v| value_json(vm, v))
                            .unwrap_or(serde_json::Value::Null)
                    };
                    let prompt = arg
                        .get("prompt")
                        .and_then(|p| p.as_str())
                        .map(str::to_owned);
                    match prompt {
                        Some(prompt) => {
                            let input =
                                arg.get("input").cloned().unwrap_or(serde_json::Value::Null);
                            let budget = arg
                                .get("budget")
                                .and_then(|b| b.as_u64())
                                .map(|b| b as usize);
                            let id = self.register_pending(
                                "agent",
                                serde_json::json!([arg]),
                                call.promise,
                            );
                            spawns.push(SpawnFrame {
                                invoke_id: id,
                                prompt,
                                input,
                                budget,
                            });
                        }
                        None => {
                            let v =
                                Value::String(RcStr::from("tools.agent needs { prompt, input }"));
                            self.running_vm()
                                .reject_promise(call.promise, v)
                                .expect("fresh promise");
                            progressed = true;
                        }
                    }
                }
                _ => {
                    let args = {
                        let vm = self.running_vm();
                        serde_json::Value::Array(
                            call.args.iter().map(|v| value_json(vm, v)).collect(),
                        )
                    };
                    let id = self.register_pending(&call.name, args.clone(), call.promise);
                    tool_calls.push(OutCall {
                        invoke_id: id,
                        name: call.name,
                        args,
                    });
                }
            }
        }
        if !tool_calls.is_empty() {
            out.push(StepOutput::ToolCalls(tool_calls));
        }
        if !spawns.is_empty() {
            out.push(StepOutput::SpawnFrames(spawns));
        }
        progressed
    }

    fn running_vm(&mut self) -> &mut VM {
        match &mut self.phase {
            Phase::Running(run) => &mut run.vm,
            _ => unreachable!("no running VM"),
        }
    }

    fn register_pending(
        &mut self,
        name: &str,
        args: serde_json::Value,
        promise: PromisePtr,
    ) -> u64 {
        self.invoke_counter += 1;
        self.pending.insert(
            self.invoke_counter,
            PendingCall {
                name: name.to_owned(),
                args,
                promise,
                generation: self.generation,
            },
        );
        self.invoke_counter
    }

    /// Serve `tools.tool_result(id)` from the log. Artifact ids are
    /// scoped to this frame's spine segment (decision 3: never ancestor
    /// artifacts).
    fn fetch_artifact(&self, tree: &Tree, call: &InvokeCall) -> Result<serde_json::Value, String> {
        let id = match call.args.first() {
            Some(Value::PosInt(n)) => *n,
            _ => return Err("tool_result needs a numeric artifact id".into()),
        };
        for event in self.frame_segment(tree) {
            if event.id.as_u64() != id {
                continue;
            }
            return match &event.payload {
                EventPayload::Invoke { result, .. } => Ok(result.clone()),
                EventPayload::ProgramResult { value } => Ok(value.clone()),
                _ => Err(format!("event #{id} is not an artifact")),
            };
        }
        Err(format!("no artifact #{id} in this frame"))
    }

    fn finish_program(
        &mut self,
        tree: &mut Tree,
        value: Value,
        unstarted: Vec<InvokeCall>,
        mut out: Vec<StepOutput>,
    ) -> io::Result<Vec<StepOutput>> {
        let Phase::Running(run) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            unreachable!()
        };
        let value_json = run
            .vm
            .stack_value_to_json(&value, 0)
            .unwrap_or_else(|_| serde_json::Value::String(format!("{value:?}")));

        // The return is the program's *answer* into this frame's context —
        // the one value that deliberately crosses into a mind (DESIGN.md
        // "The one exception"). It is budgeted, not rejected: the full
        // value is logged as a fetchable `ProgramResult` below, and only
        // the context copy is truncated (naming its id) by the report.
        let completion_value = value_json.clone();

        // Fire-and-forget calls the program never awaited: the host
        // decides whether to run them; results are logged as late
        // artifacts (the generation bump keeps them log-only — the
        // program can no longer observe them).
        let mut fire_and_forget = Vec::new();
        for call in &unstarted {
            let args = serde_json::Value::Array(
                call.args
                    .iter()
                    .map(|v| value_json_of(&run.vm, v))
                    .collect(),
            );
            let id = self.register_pending(&call.name, args.clone(), call.promise);
            fire_and_forget.push(OutCall {
                invoke_id: id,
                name: call.name.clone(),
                args,
            });
        }
        self.generation += 1;

        tree.append(
            &mut self.spine,
            EventPayload::ProgramResult {
                value: value_json.clone(),
            },
        )?;

        let advise_attachments =
            !run.had_attachments && self.run_inlined_large_body(tree, run.started_at);
        let report = CompletionReport {
            value: completion_value,
            budget: self.answer_budget,
            console: run.vm.console_lines.clone(),
            new_artifacts: self.new_artifacts(tree, run.started_at),
            advise_attachments,
        }
        .render();
        tree.append(
            &mut self.spine,
            EventPayload::Message(Message::Tool {
                name: TOOL_RUN_PROGRAM.into(),
                call_id: run.call_id,
                text: report,
            }),
        )?;
        // Faithful, unclipped console for the log/UI (decision 8): the
        // report above carries only a clipped tail for the LLM.
        tree.append(
            &mut self.spine,
            EventPayload::Console {
                lines: run.vm.console_lines.clone(),
            },
        )?;

        if !fire_and_forget.is_empty() {
            out.push(StepOutput::ToolCalls(fire_and_forget));
        }
        self.note_status(run.program_id, ProgramStatus::Completed);
        self.last_vm = Some(run.vm);
        self.phase = Phase::AwaitingLlm;
        out.push(self.render_request());
        Ok(out)
    }

    fn suspend(
        &mut self,
        tree: &mut Tree,
        cause: SuspendCause,
        mut out: Vec<StepOutput>,
    ) -> io::Result<Vec<StepOutput>> {
        let Phase::Running(run) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            unreachable!()
        };

        let (what, resume, suspension) = match cause {
            SuspendCause::Raise { condition, payload } => {
                let payload = payload
                    .map(|v| {
                        run.vm
                            .stack_value_to_json(&v, 0)
                            .map(|j| j.to_string())
                            .unwrap_or_else(|_| format!("{v:?}"))
                    })
                    .unwrap_or_else(|| "(none)".into());
                let mut what = raise_location(&run.vm, &condition);
                what.push_str("\npayload: ");
                what.push_str(&clip(&payload, PAYLOAD_MAX_BYTES));
                (what, ResumeKind::Raise, Suspension::Raise)
            }
            SuspendCause::Trapped(e) => {
                let what = run.vm.render_error(&e);
                let resume = match e.resume {
                    ResumeMode::PushValueThenContinue => ResumeKind::Operation,
                    ResumeMode::NotResumable => ResumeKind::No,
                };
                (what, resume, Suspension::Trapped(e))
            }
        };

        let report = ConditionReport {
            what,
            stack: run
                .vm
                .frames()
                .iter()
                .map(|f| f.name().to_owned())
                .collect(),
            console: run.vm.console_lines.clone(),
            artifacts: self.frame_artifacts(tree),
            resume,
        }
        .render();
        let console = run.vm.console_lines.clone();
        let call_id = run.call_id.clone();
        let program_id = run.program_id;
        self.phase = Phase::Suspended(run, suspension);
        self.note_status(program_id, ProgramStatus::Suspended);
        tree.append(
            &mut self.spine,
            EventPayload::Message(Message::Tool {
                name: TOOL_RUN_PROGRAM.into(),
                call_id,
                text: report,
            }),
        )?;
        // Faithful, unclipped console for the log/UI (decision 8).
        tree.append(&mut self.spine, EventPayload::Console { lines: console })?;
        out.push(self.render_request());
        Ok(out)
    }

    fn finish_frame(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        // Abandon any suspended program: a no-tool-call turn completes
        // the frame, its text is the result.
        if let Phase::Suspended(run, _) = std::mem::replace(&mut self.phase, Phase::Idle) {
            self.note_status(run.program_id, ProgramStatus::Failed);
            self.last_vm = Some(run.vm);
        }
        self.generation += 1;
        let text = match self.spine.frame().messages.last() {
            Some(Message::Assistant { text, .. }) => text.clone(),
            _ => String::new(),
        };
        if self.is_root {
            // The top conversation never ends: yield the turn to the user
            // without logging a `FrameResult`, so the spine stays
            // appendable for the next `UserTurn`. The root's answer goes to
            // the *user*, not into another context — human-bound, delivered
            // in full (the answer budget governs mind-bound answers only).
            self.phase = Phase::Idle;
            return Ok(vec![StepOutput::Yielded]);
        }
        // A subagent's final answer crosses into its caller's context — the
        // one mind-bound deliverable (decision 4). Budget it: re-prompt
        // once to tighten, then truncate-with-note. The full prose stays on
        // the spine as the Assistant message regardless.
        if text.len() > self.answer_budget && self.answer_retries < ANSWER_RETRY_LIMIT {
            self.answer_retries += 1;
            let nudge = format!(
                "[harness] Your answer is {} bytes; the budget is {}. Tighten it to a \
                 digest, or write a large product with create_file and report its path.",
                text.len(),
                self.answer_budget
            );
            tree.append(
                &mut self.spine,
                EventPayload::Message(Message::User { text: nudge }),
            )?;
            self.phase = Phase::AwaitingLlm;
            return Ok(vec![self.render_request()]);
        }
        let result = if text.is_empty() {
            serde_json::Value::Null
        } else if text.len() > self.answer_budget {
            serde_json::Value::String(truncate_answer(&text, self.answer_budget))
        } else {
            serde_json::Value::String(text)
        };
        tree.append(
            &mut self.spine,
            EventPayload::FrameResult {
                result: result.clone(),
            },
        )?;
        self.phase = Phase::Done;
        Ok(vec![StepOutput::FrameDone(result)])
    }

    // ── rendering ───────────────────────────────────────────────────

    fn render_request(&self) -> StepOutput {
        // The system prompt is the spine's first message (materialized
        // once by `ensure_system`, decision 4); send the frame's messages
        // verbatim — no synthesized prepend.
        let messages = self.spine.frame().messages.clone();

        let tools = match &self.phase {
            Phase::Suspended(_, Suspension::Trapped(e))
                if matches!(e.resume, ResumeMode::NotResumable) =>
            {
                vec![run_program_spec()]
            }
            Phase::Suspended(..) => vec![resume_spec(), run_program_spec()],
            _ => vec![run_program_spec()],
        };
        StepOutput::LlmRequest(LlmRequest { messages, tools })
    }

    fn log_tool_error(&mut self, tree: &mut Tree, call: &ToolCall, msg: &str) -> io::Result<()> {
        tree.append(
            &mut self.spine,
            EventPayload::Message(Message::Tool {
                name: call.name.clone(),
                call_id: call.id.clone(),
                text: format!("error: {msg}"),
            }),
        )?;
        Ok(())
    }

    /// Events of this frame's spine segment (its `FrameStart` down to
    /// the leaf), in log order.
    fn frame_segment<'t>(&self, tree: &'t Tree) -> Vec<&'t Event> {
        let mut events = Vec::new();
        let mut current = self.spine.leaf_id;
        while let Some(event) = tree.events.get(&current) {
            let is_frame_start = matches!(event.payload, EventPayload::FrameStart { .. });
            events.push(event);
            if is_frame_start {
                break;
            }
            match event.parent_id {
                Some(parent) => current = parent,
                None => break,
            }
        }
        events.reverse();
        events
    }

    /// Artifact-menu entries for every artifact on this frame so far.
    fn frame_artifacts(&self, tree: &Tree) -> Vec<Artifact> {
        self.frame_segment(tree)
            .into_iter()
            .filter_map(artifact_entry)
            .collect()
    }

    fn new_artifacts(&self, tree: &Tree, since: u64) -> Vec<Artifact> {
        self.frame_segment(tree)
            .into_iter()
            .filter(|e| e.id.as_u64() > since)
            .filter_map(artifact_entry)
            .collect()
    }

    /// Whether any `create_file`/`replace_file` logged by this run inlined
    /// a content body past [`INLINE_BODY_ADVICE_BYTES`]. Content is the last
    /// positional arg; the full (unclipped) args live on the `Invoke` event.
    fn run_inlined_large_body(&self, tree: &Tree, since: u64) -> bool {
        self.frame_segment(tree)
            .into_iter()
            .filter(|e| e.id.as_u64() > since)
            .any(|e| match &e.payload {
                EventPayload::Invoke { name, args, .. }
                    if name == "create_file" || name == "replace_file" =>
                {
                    args.as_array()
                        .and_then(|a| a.last())
                        .and_then(|v| v.as_str())
                        .is_some_and(|content| content.len() > INLINE_BODY_ADVICE_BYTES)
                }
                _ => false,
            })
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Extract + validate the optional `attachments` map from a `run_program`
/// call: an object of name → content string (this run's authored bodies).
/// Absent/null yields an empty object. A malformed shape returns a message
/// for the repair loop (the LLM fixes the call) rather than crashing.
fn attachments_from_args(args: &serde_json::Value) -> Result<serde_json::Value, String> {
    match args.get("attachments") {
        None | Some(serde_json::Value::Null) => Ok(serde_json::Value::Object(Default::default())),
        Some(serde_json::Value::Object(map)) => {
            if let Some((k, _)) = map.iter().find(|(_, v)| !v.is_string()) {
                return Err(format!(
                    "run_program `attachments.{k}` must be a string — each attachment is \
                     content text (e.g. a file body), read in the program as attachments.{k}"
                ));
            }
            Ok(serde_json::Value::Object(map.clone()))
        }
        Some(_) => Err(
            "run_program `attachments` must be an object mapping names to content \
                        strings, e.g. {\"gameJs\": \"...\"}; read them in the program as \
                        attachments.<name>"
                .into(),
        ),
    }
}

fn json_arg(vm: &mut VM, json: &serde_json::Value) -> Value {
    vm.json_to_stack_value(json, 0).unwrap_or(Value::Null)
}

fn value_json(vm: &VM, v: &Value) -> serde_json::Value {
    vm.stack_value_to_json(v, 0)
        .unwrap_or_else(|_| serde_json::Value::String(format!("{v:?}")))
}

// Alias for call sites where `value_json` would shadow a local.
fn value_json_of(vm: &VM, v: &Value) -> serde_json::Value {
    value_json(vm, v)
}

/// Truncate an over-budget subagent answer for delivery to its caller,
/// with a note pointing at the two sound moves (ask for less / write a
/// file). The full prose remains on the child's spine for the log/TUI.
fn truncate_answer(text: &str, budget: usize) -> String {
    let mut end = budget.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}… [answer truncated to {} of {} bytes — ask for less, or have me write a file]",
        &text[..end],
        end,
        text.len()
    )
}

fn render_diags(source: &str, diags: &[Diagnostic]) -> String {
    let rendered: Vec<String> = diags.iter().take(5).map(|d| d.render(source)).collect();
    format!("compile error:\n{}", rendered.join("\n"))
}

/// One artifact-menu entry from a logged execution event.
fn artifact_entry(event: &Event) -> Option<Artifact> {
    match &event.payload {
        EventPayload::Invoke { name, args, result } => Some(Artifact {
            id: event.id.as_u64(),
            label: format!("{}({})", name, preview(args)),
            result: result.clone(),
        }),
        EventPayload::ProgramResult { value } => Some(Artifact {
            id: event.id.as_u64(),
            label: "program result".into(),
            result: value.clone(),
        }),
        _ => None,
    }
}

/// "condition `name` raised" rendered at the raise site (source line +
/// caret). `step()` advanced `ip` past the `Raise` instruction, so the
/// raise site is the previous slot.
fn raise_location(vm: &VM, condition: &str) -> String {
    let message = format!("condition `{condition}` raised");
    let ip = (vm.ip as usize).saturating_sub(1);
    match vm.spans.get(ip) {
        Some(&span) if !vm.source.is_empty() => Diagnostic { span, message }.render(&vm.source),
        _ => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FUEL: u64 = 100_000;

    fn setup() -> (Tree, AgentState) {
        let mut tree = Tree::new(None);
        let state = AgentState::new_root(&mut tree, "you are a test agent", json!(null)).unwrap();
        (tree, state)
    }

    fn llm_program(call_id: &str, source: &str) -> Message {
        Message::Assistant {
            text: String::new(),
            thinking: None,
            tool_calls: vec![ToolCall {
                id: call_id.into(),
                name: TOOL_RUN_PROGRAM.into(),
                arguments: json!({ "source": source }),
            }],
        }
    }

    fn llm_resume(call_id: &str, value: serde_json::Value) -> Message {
        Message::Assistant {
            text: String::new(),
            thinking: None,
            tool_calls: vec![ToolCall {
                id: call_id.into(),
                name: TOOL_RESUME.into(),
                arguments: json!({ "value": value }),
            }],
        }
    }

    fn llm_text(text: &str) -> Message {
        Message::Assistant {
            text: text.into(),
            thinking: None,
            tool_calls: Vec::new(),
        }
    }

    /// Drive `Tick`s until the machine stops asking for them; collects
    /// every non-`Working` output.
    fn drain(state: &mut AgentState, tree: &mut Tree, outputs: Vec<StepOutput>) -> Vec<StepOutput> {
        let mut result = Vec::new();
        let mut queue = outputs;
        for _ in 0..1000 {
            let mut working = false;
            for o in queue {
                match o {
                    StepOutput::Working => working = true,
                    other => result.push(other),
                }
            }
            if !working {
                return result;
            }
            queue = state.step(tree, StepInput::Tick { fuel: FUEL }).unwrap();
        }
        panic!("machine never settled");
    }

    fn last_tool_text(state: &AgentState) -> String {
        state
            .spine
            .frame()
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Tool { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("a tool message")
    }

    fn payload_kinds(state: &AgentState, tree: &Tree) -> Vec<&'static str> {
        state
            .frame_segment(tree)
            .iter()
            .map(|e| match &e.payload {
                EventPayload::FrameStart { .. } => "FrameStart",
                EventPayload::FrameResult { .. } => "FrameResult",
                EventPayload::Message(Message::User { .. }) => "User",
                EventPayload::Message(Message::Assistant { .. }) => "Assistant",
                EventPayload::Message(Message::System { .. }) => "System",
                EventPayload::Message(Message::Tool { .. }) => "Tool",
                EventPayload::Invoke { .. } => "Invoke",
                EventPayload::ProgramResult { .. } => "ProgramResult",
                EventPayload::Console { .. } => "Console",
                EventPayload::Label(_) => "Label",
            })
            .collect()
    }

    fn expect_request(outputs: &[StepOutput]) -> &LlmRequest {
        outputs
            .iter()
            .find_map(|o| match o {
                StepOutput::LlmRequest(r) => Some(r),
                _ => None,
            })
            .expect("an LlmRequest output")
    }

    fn tool_names(req: &LlmRequest) -> Vec<&str> {
        req.tools.iter().map(|t| t.name.as_str()).collect()
    }

    fn expect_tool_calls(outputs: &[StepOutput]) -> &Vec<OutCall> {
        outputs
            .iter()
            .find_map(|o| match o {
                StepOutput::ToolCalls(c) => Some(c),
                _ => None,
            })
            .expect("a ToolCalls output")
    }

    // ── the scripted round-trip ─────────────────────────────────────

    #[test]
    fn user_turn_renders_request() {
        let (mut tree, mut state) = setup();
        let out = state
            .step(&mut tree, StepInput::UserTurn("compute 6*7".into()))
            .unwrap();
        let req = expect_request(&out);
        assert!(
            matches!(&req.messages[0], Message::System { text } if text.contains("test agent"))
        );
        assert!(matches!(&req.messages[1], Message::User { text } if text == "compute 6*7"));
        assert_eq!(tool_names(req), [TOOL_RUN_PROGRAM]);
        // The full definition rides along: schema'd parameters, not a name.
        assert!(req.tools[0].parameters["properties"]["source"].is_object());
        assert!(!req.tools[0].description.is_empty());
    }

    #[test]
    fn dialect_card_roots_the_system_message() {
        let (mut tree, mut state) = setup();
        state.set_dialect_card("THE DIALECT CARD".into());
        let out = state
            .step(&mut tree, StepInput::UserTurn("go".into()))
            .unwrap();
        let req = expect_request(&out);
        let Message::System { text } = &req.messages[0] else {
            panic!("first message must be the system message");
        };
        assert!(text.starts_with("THE DIALECT CARD\n\n"), "{text}");
        // The stored prompt is the spine's first event after FrameStart.
        assert_eq!(payload_kinds(&state, &tree).first(), Some(&"FrameStart"));
        assert_eq!(payload_kinds(&state, &tree).get(1), Some(&"System"));
        assert!(
            text.contains("you are a test agent"),
            "frame prompt follows the card: {text}"
        );
    }

    /// Step 2 (decision 4): the system prompt is materialized once and
    /// replays verbatim. Re-opening the spine with a *different* card does
    /// not re-derive it — `render_request` sends the stored `System` as-is.
    #[test]
    fn reopened_spine_replays_the_stored_system_prompt() {
        let mut tree = Tree::new(None);
        let mut state = AgentState::new_root(&mut tree, "agent", json!({ "n": 1 })).unwrap();
        state.set_dialect_card("CARD A".into());
        state.kickoff(&mut tree).unwrap(); // logs the System (#2) with CARD A
        let stored = match &tree.events[&EventId::new(2)].payload {
            EventPayload::Message(Message::System { text }) => text.clone(),
            other => panic!("expected a System at #2, got {other:?}"),
        };
        assert!(stored.starts_with("CARD A"));

        // Re-anchor a fresh state on the logged spine, card the registry
        // differently, take a new turn — the request's system message is
        // the stored CARD A prompt, not a CARD B re-derivation.
        let mut reopened = AgentState::with_spine(tree.spine_at(EventId::new(2)));
        reopened.set_dialect_card("CARD B — evolved".into());
        let out = reopened
            .step(&mut tree, StepInput::UserTurn("more".into()))
            .unwrap();
        let req = expect_request(&out);
        let Message::System { text } = &req.messages[0] else {
            panic!("first message must be the stored system prompt");
        };
        assert_eq!(text, &stored, "stored prompt replays verbatim");
        assert!(
            !text.contains("CARD B"),
            "the evolved card must not leak in"
        );
    }

    #[test]
    fn input_binding_reaches_the_program() {
        let mut tree = Tree::new(None);
        let mut state = AgentState::new_root(&mut tree, "agent", json!({ "n": 7 })).unwrap();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "return input.n;")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert!(last_tool_text(&state).contains("returned: 7"));
    }

    #[test]
    fn attachments_reach_the_program_as_a_const() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        // A run_program carrying authored content in `attachments`; the
        // program reads it as the `attachments` const, never embedding it
        // in `source`.
        let msg = Message::Assistant {
            text: String::new(),
            thinking: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: TOOL_RUN_PROGRAM.into(),
                arguments: json!({
                    "source": "return attachments.greeting.length;",
                    "attachments": { "greeting": "hello world" },
                }),
            }],
        };
        let out = state.step(&mut tree, StepInput::LlmResponse(msg)).unwrap();
        drain(&mut state, &mut tree, out);
        // "hello world" is 11 bytes.
        assert!(
            last_tool_text(&state).contains("returned: 11"),
            "{}",
            last_tool_text(&state)
        );
    }

    #[test]
    fn malformed_attachments_is_a_repair_loop() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let msg = Message::Assistant {
            text: String::new(),
            thinking: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: TOOL_RUN_PROGRAM.into(),
                arguments: json!({
                    "source": "return 1;",
                    "attachments": { "body": 42 }, // not a string
                }),
            }],
        };
        let out = state.step(&mut tree, StepInput::LlmResponse(msg)).unwrap();
        // No program ran; the error is the tool result and a fresh request
        // follows (the repair loop), exactly like a missing `source`.
        let report = last_tool_text(&state);
        assert!(
            report.contains("attachments.body") && report.contains("must be a string"),
            "{report}"
        );
        assert!(expect_request(&out).messages.last().is_some());
    }

    #[test]
    fn program_completion_then_root_yields() {
        let (mut tree, mut state) = setup();
        state
            .step(&mut tree, StepInput::UserTurn("go".into()))
            .unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program(
                    "c1",
                    "console.log(\"hi there\"); return 6 * 7;",
                )),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);

        // Completion report is the tool result; the next request follows.
        let report = last_tool_text(&state);
        assert!(report.contains("returned: 42"), "{report}");
        assert!(report.contains("hi there"), "{report}");
        let req = expect_request(&settled);
        assert!(matches!(req.messages.last(), Some(Message::Tool { .. })));

        // Final text turn on the *root* frame yields to the user — the
        // top conversation never ends, so no `FrameResult` is logged and
        // the frame stays idle, ready for the next turn.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_text("the answer is 42")),
            )
            .unwrap();
        assert!(matches!(&out[..], [StepOutput::Yielded]), "{out:?}");
        assert!(!state.is_done());
        assert!(state.is_idle());
        assert_eq!(
            payload_kinds(&state, &tree),
            [
                "FrameStart",
                "System",
                "User",
                "Assistant",
                "ProgramResult",
                "Tool",
                "Console",
                "Assistant",
            ]
        );

        // A follow-up turn appends onto the same spine and runs again.
        state
            .step(&mut tree, StepInput::UserTurn("more".into()))
            .unwrap();
        assert!(matches!(payload_kinds(&state, &tree).last(), Some(&"User")));
    }

    #[test]
    fn fanout_batch_and_resolution_order() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"
            const a = tools.fetch("x");
            const b = tools.fetch("y");
            return [await a, await b];
        "#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let calls = expect_tool_calls(&settled);
        assert_eq!(calls.len(), 2, "one fan-out batch");
        assert_eq!(calls[0].args, json!(["x"]));
        assert_eq!(calls[1].args, json!(["y"]));

        // Resolve out of order: y first. Resolution order is what's logged.
        let (xa, yb) = (calls[0].invoke_id, calls[1].invoke_id);
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    invoke_id: yb,
                    result: Ok(json!("Y")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out); // still blocked on `a`
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    invoke_id: xa,
                    result: Ok(json!("X")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        assert!(last_tool_text(&state).contains(r#"returned: ["X","Y"]"#));
        let invokes: Vec<serde_json::Value> = state
            .frame_segment(&tree)
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Invoke { result, .. } => Some(result.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            invokes,
            vec![json!("Y"), json!("X")],
            "logged in resolution order"
        );
    }

    #[test]
    fn compile_error_is_a_repair_loop() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "let = ;")),
            )
            .unwrap();
        // No Working: nothing ran.
        assert!(!out.iter().any(|o| matches!(o, StepOutput::Working)));
        let req = expect_request(&out);
        assert_eq!(tool_names(req), [TOOL_RUN_PROGRAM]);
        assert!(last_tool_text(&state).contains("compile error"));
        // No execution events were logged.
        assert_eq!(
            payload_kinds(&state, &tree),
            ["FrameStart", "System", "Assistant", "Tool"]
        );
    }

    #[test]
    fn raise_reports_and_resume_continues() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"
            const x = await tools.fetch("a");
            raise("need_help", { got: x });
            return x + 1;
        "#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let id = expect_tool_calls(&settled)[0].invoke_id;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    invoke_id: id,
                    result: Ok(json!(41)),
                }]),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);

        // Suspended: report + restart tools (full definitions).
        let req = expect_request(&settled);
        assert_eq!(tool_names(req), [TOOL_RESUME, TOOL_RUN_PROGRAM]);
        assert!(req.tools[0].parameters["properties"]["value"].is_object());
        let report = last_tool_text(&state);
        assert!(report.contains("condition `need_help`"), "{report}");
        assert!(report.contains(r#"{"got":41}"#), "{report}");
        assert!(
            report.contains("fetch"),
            "artifact menu lists the call: {report}"
        );
        assert!(report.contains("resume(value)"), "{report}");

        // Resume: same VM continues.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_resume("c2", json!(null))),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert!(last_tool_text(&state).contains("returned: 42"));
    }

    #[test]
    fn trapped_type_error_resumes_with_value() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "const v = null; return v.x;")),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let req = expect_request(&settled);
        assert_eq!(tool_names(req), [TOOL_RESUME, TOOL_RUN_PROGRAM]);

        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_resume("c2", json!(42))),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert!(last_tool_text(&state).contains("returned: 42"));
    }

    #[test]
    fn rewrite_reuses_artifact_from_the_log() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"
            const a = await tools.fetch("expensive");
            raise("stop", null);
        "#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let id = expect_tool_calls(&settled)[0].invoke_id;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    invoke_id: id,
                    result: Ok(json!("DATA")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out); // suspended on the raise

        // Find the logged Invoke artifact id from the report's menu.
        let artifact_id = state
            .frame_segment(&tree)
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::Invoke { .. } => Some(e.id.as_u64()),
                _ => None,
            })
            .unwrap();

        // Rewrite: fetch the artifact instead of repeating the call.
        let rewrite = format!("return await tools.tool_result({artifact_id});");
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c2", &rewrite)),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);

        // Served from the log: no ToolCalls went out.
        assert!(
            !settled
                .iter()
                .any(|o| matches!(o, StepOutput::ToolCalls(_))),
            "{settled:?}"
        );
        assert!(last_tool_text(&state).contains(r#"returned: "DATA""#));
    }

    #[test]
    fn agent_call_spawns_child_frame() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"return await tools.agent({ prompt: "summarize", input: { n: 1 } });"#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let spawn = settled
            .iter()
            .find_map(|o| match o {
                StepOutput::SpawnFrames(s) => Some(&s[0]),
                _ => None,
            })
            .expect("a SpawnFrames output");
        assert_eq!(spawn.prompt, "summarize");
        assert_eq!(spawn.input, json!({ "n": 1 }));

        // Host side: run the child frame to completion on its own branch.
        let mut child = AgentState::new_child(
            &mut tree,
            state.spine.leaf_id,
            &spawn.prompt,
            spawn.input.clone(),
            spawn.budget,
        )
        .unwrap();
        assert_eq!(tree.list_leaves().len(), 2, "caller + in-flight child");
        child.kickoff(&mut tree).unwrap();
        let out = child
            .step(&mut tree, StepInput::LlmResponse(llm_text("child says hi")))
            .unwrap();
        let result = match &out[..] {
            [StepOutput::FrameDone(v)] => v.clone(),
            other => panic!("expected FrameDone, got {other:?}"),
        };

        // Join: the child's result resolves the caller's agent call.
        let invoke_id = spawn.invoke_id;
        let out = state
            .step(&mut tree, StepInput::SubagentResult { invoke_id, result })
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert!(last_tool_text(&state).contains(r#"returned: "child says hi""#));
        assert!(
            payload_kinds(&state, &tree).contains(&"Invoke"),
            "agent call logged as an artifact"
        );
    }

    #[test]
    fn hot_loop_yields_per_tick() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "while (true) {}")),
            )
            .unwrap();
        assert!(matches!(&out[..], [StepOutput::Working]));
        for _ in 0..3 {
            let out = state
                .step(&mut tree, StepInput::Tick { fuel: 10_000 })
                .unwrap();
            assert!(
                matches!(&out[..], [StepOutput::Working]),
                "a hot loop keeps yielding, never blocks"
            );
        }
    }

    // ── golden renders (8_HARNESS Step 4) ───────────────────────────
    //
    // Exact full-string asserts: the reports are a prompt-engineering
    // artifact, so format changes should be deliberate diffs here, not
    // incidental. Driven through the real machine (real event ids,
    // real console output).

    #[test]
    fn golden_condition_report_raise_with_payload() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = "const x = await tools.fetch(\"a\");\nconsole.log(\"fetched: \" + x);\nraise(\"need_help\", { got: x });\nreturn x + 1;";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let id = expect_tool_calls(&settled)[0].invoke_id;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    invoke_id: id,
                    result: Ok(json!(41)),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(
            last_tool_text(&state),
            r#"## what happened
3:1: condition `need_help` raised
raise("need_help", { got: x });
^
payload: {"got":41}

## where
in <root>
console (last 1 of 1 lines):
fetched: 41

## artifacts — fetch with tools.tool_result(id)
[#4] fetch(["a"]) → 41

## restarts
- resume(value): continue past the raise; `value` becomes the result of the raise(...) expression
- run_program(source): replace the program — new source runs in a fresh VM; results in the artifact menu stay fetchable via tools.tool_result(id), so reuse them instead of repeating calls"#
        );
    }

    #[test]
    fn golden_condition_report_trapped_type_error() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "const v = null;\nreturn v.x;")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(
            last_tool_text(&state),
            r#"## what happened
2:10: cannot read property on null
return v.x;
         ^

## where
in <root>
console: (no output)

## artifacts — fetch with tools.tool_result(id)
(none)

## restarts
- resume(value): continue as if the failed operation had produced `value`
- run_program(source): replace the program — new source runs in a fresh VM; results in the artifact menu stay fetchable via tools.tool_result(id), so reuse them instead of repeating calls"#
        );
    }

    #[test]
    fn golden_completion_report() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = "const a = await tools.fetch(\"x\");\nconsole.log(\"got \" + a);\nreturn [a, 2];";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let id = expect_tool_calls(&settled)[0].invoke_id;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    invoke_id: id,
                    result: Ok(json!("X")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(
            last_tool_text(&state),
            r#"## program completed
returned: ["X",2]

console (last 1 of 1 lines):
got X

## new artifacts — fetch with tools.tool_result(id)
[#4] fetch(["x"]) → "X"
[#5] program result → ["X",2]"#
        );
    }

    #[test]
    #[should_panic(expected = "mid-program user turns")]
    fn mid_program_user_turn_panics_for_now() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "await tools.fetch(1); return 0;")),
            )
            .unwrap();
        let _ = state.step(&mut tree, StepInput::UserTurn("are you done?".into()));
    }

    fn program_result_value(state: &AgentState, tree: &Tree) -> serde_json::Value {
        state
            .frame_segment(tree)
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::ProgramResult { value } => Some(value.clone()),
                _ => None,
            })
            .expect("a ProgramResult")
    }

    #[test]
    fn program_return_is_delivered_up_to_budget() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        // A moderate return (well under the 64 KB answer budget) is
        // delivered in full and logged in full — the read/summarize happy
        // path that the old 4 KB reject broke (12_ANSWERS).
        let src = "return \"x\".repeat(5000);";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        drain(&mut state, &mut tree, out);
        let report = last_tool_text(&state);
        assert!(report.contains(&"x".repeat(5000)), "delivered in full");
        assert!(!report.contains("tools.tool_result(#"), "no spill marker");
        assert_eq!(
            program_result_value(&state, &tree).as_str().unwrap().len(),
            5000,
            "full value logged"
        );
    }

    #[test]
    fn over_budget_return_truncates_with_fetch_id() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        // Force a tiny budget so a modest return is over it deterministically
        // (the VM caps `String.repeat` at 10 KB, well under the default).
        state.answer_budget = 100;
        let src = "return \"x\".repeat(5000);";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        drain(&mut state, &mut tree, out);
        let report = last_tool_text(&state);
        // The returned-value line is truncated with a marker naming the id.
        let returned = report.lines().nth(1).unwrap();
        assert!(
            returned.contains("tools.tool_result(#"),
            "spill marker on returned line: {returned}"
        );
        // The full value is logged for fetching.
        assert_eq!(
            program_result_value(&state, &tree).as_str().unwrap().len(),
            5000,
            "full value logged"
        );
    }

    #[test]
    fn over_budget_subagent_answer_reprompts_then_truncates() {
        let (mut tree, root) = setup();
        let call_site = root.spine.leaf_id;
        let mut child =
            AgentState::new_child(&mut tree, call_site, "summarize", json!({}), Some(50)).unwrap();
        child.kickoff(&mut tree).unwrap();
        let long = "y".repeat(500);
        // First over-budget final answer → one re-prompt, not completion.
        let out = child
            .step(&mut tree, StepInput::LlmResponse(llm_text(&long)))
            .unwrap();
        assert!(
            matches!(out[..], [StepOutput::LlmRequest(_)]),
            "re-prompted once: {out:?}"
        );
        // Second over-budget answer → deterministic truncate-with-note.
        let out = child
            .step(&mut tree, StepInput::LlmResponse(llm_text(&long)))
            .unwrap();
        let result = match &out[..] {
            [StepOutput::FrameDone(v)] => v.clone(),
            other => panic!("expected FrameDone, got {other:?}"),
        };
        let delivered = result.as_str().unwrap();
        assert!(delivered.contains("answer truncated"), "note: {delivered}");
        assert!(delivered.len() < long.len(), "delivered value is bounded");
        // The full prose stays on the child's spine (last Assistant message).
        assert!(
            child
                .spine
                .frame()
                .messages
                .iter()
                .any(|m| matches!(m, Message::Assistant { text, .. } if text.len() == 500)),
            "full prose retained on spine"
        );
    }
}
