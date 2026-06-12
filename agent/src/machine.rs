//! Sans-io frame step machine (8_HARNESS Step 3).
//!
//! One `AgentState` drives one frame: a deterministic, IO-free core the
//! host feeds with `StepInput`s and drains of `StepOutput`s. The host
//! owns the LLM API, tool execution, subagent loops, and scheduling;
//! the core never blocks. VM compute is host-fueled: the machine runs
//! one `step(fuel)` slice per `Tick` and reports `Working` when it
//! wants another, so a hot program can't starve the host loop.

use std::collections::{HashMap, HashSet};
use std::io;

use interp::{
    Diagnostic, InvokeCall, PromisePtr, RcStr, ResumeMode, StepResult, VM, VMError, Value, compile,
};

use crate::types::*;

/// Tool names offered to the LLM. `run_program` is the primary tool;
/// `resume` appears only while suspended on a resumable condition.
pub const TOOL_RUN_PROGRAM: &str = "run_program";
pub const TOOL_RESUME: &str = "resume";

/// Iteration cap for one `Tick`: each extra round requires a synchronous
/// artifact fetch (`tools.tool_result`) to have unblocked the program,
/// but a pathological program could chain those forever.
const MAX_PUMP_ROUNDS: usize = 100;

/// Console lines quoted in completion/condition reports (tail).
const CONSOLE_TAIL: usize = 20;

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
    /// The VM wants another `Tick`.
    Working,
}

#[derive(Debug)]
pub struct LlmRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<String>,
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
}

/// One program execution: the `run_program` tool call being served.
struct Run {
    /// LLM tool-call id the eventual tool result answers.
    call_id: String,
    vm: VM,
    /// Event-id high-water mark when the run started: artifacts logged
    /// after it are "new" in this run's completion report.
    started_at: u64,
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
    phase: Phase,
    invoke_counter: u64,
    generation: u64,
    pending: HashMap<u64, PendingCall>,
    /// Tool names whose artifacts get the "already happened; calling
    /// again repeats the effect" warning in reports (registry-fed).
    effectful_tools: HashSet<String>,
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
    ) -> io::Result<Self> {
        let spine = tree.start_frame(Some(call_site), prompt, input)?;
        Ok(Self::with_spine(spine))
    }

    /// Resume an existing spine (re-opened log).
    pub fn with_spine(spine: Spine) -> Self {
        AgentState {
            spine,
            phase: Phase::Idle,
            invoke_counter: 0,
            generation: 0,
            pending: HashMap::new(),
            effectful_tools: HashSet::new(),
        }
    }

    /// Names whose artifact-menu lines carry the effectful warning
    /// (the host feeds these from the tool registry).
    pub fn set_effectful_tools(&mut self, names: HashSet<String>) {
        self.effectful_tools = names;
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

    /// The live VM, while a program is running or suspended — the
    /// privileged borrow the debugger TUI renders from (9_TUI dec. 4).
    pub fn vm(&self) -> Option<&VM> {
        match &self.phase {
            Phase::Running(run) | Phase::Suspended(run, _) => Some(&run.vm),
            _ => None,
        }
    }

    /// Start the conversation without a user turn — how child frames
    /// begin (their input arrived in `FrameStart`).
    pub fn kickoff(&mut self) -> Vec<StepOutput> {
        assert!(matches!(self.phase, Phase::Idle), "kickoff on a busy frame");
        self.phase = Phase::AwaitingLlm;
        vec![self.render_request()]
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
        tree.append(&mut self.spine, EventPayload::Message(message))?;

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
                // A rewrite abandons any suspended VM — never the physics:
                // in-flight calls stay pending and their results are still
                // logged as artifacts when they arrive (the generation bump
                // stops delivery to the dead VM).
                self.generation += 1;
                match self.start_program(source, call.id.clone()) {
                    Ok(run) => {
                        self.phase = Phase::Running(run);
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
                            self.phase = Phase::Running(run);
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

    /// Compile + bind input. `Err` is the rendered repair-loop report.
    fn start_program(&mut self, source: &str, call_id: String) -> Result<Run, String> {
        let program = compile(source).map_err(|diags| render_diags(source, &diags))?;
        let vm = VM::for_program(program, self.spine.frame().input.clone())
            .map_err(|e| format!("program setup failed: {}", e.message))?;
        Ok(Run {
            call_id,
            vm,
            started_at: self.spine.leaf_id.as_u64(),
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
                            let id = self.register_pending(
                                "agent",
                                serde_json::json!([arg]),
                                call.promise,
                            );
                            spawns.push(SpawnFrame {
                                invoke_id: id,
                                prompt,
                                input,
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

        let report = completion_report(
            &value_json,
            &run.vm,
            self.new_artifacts(tree, run.started_at),
        );
        tree.append(
            &mut self.spine,
            EventPayload::Message(Message::Tool {
                name: TOOL_RUN_PROGRAM.into(),
                call_id: run.call_id,
                text: report,
            }),
        )?;

        if !fire_and_forget.is_empty() {
            out.push(StepOutput::ToolCalls(fire_and_forget));
        }
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

        let (what, resumable, suspension) = match cause {
            SuspendCause::Raise { condition, payload } => {
                let payload = payload
                    .map(|v| {
                        run.vm
                            .stack_value_to_json(&v, 0)
                            .map(|j| j.to_string())
                            .unwrap_or_else(|_| format!("{v:?}"))
                    })
                    .unwrap_or_else(|| "(none)".into());
                (
                    format!("condition `{condition}` raised\npayload: {payload}"),
                    true,
                    Suspension::Raise,
                )
            }
            SuspendCause::Trapped(e) => {
                let what = run.vm.render_error(&e);
                let resumable = matches!(e.resume, ResumeMode::PushValueThenContinue);
                (what, resumable, Suspension::Trapped(e))
            }
        };

        let report = condition_report(&what, resumable, &run.vm, self.frame_artifacts(tree));
        let call_id = run.call_id.clone();
        self.phase = Phase::Suspended(run, suspension);
        tree.append(
            &mut self.spine,
            EventPayload::Message(Message::Tool {
                name: TOOL_RUN_PROGRAM.into(),
                call_id,
                text: report,
            }),
        )?;
        out.push(self.render_request());
        Ok(out)
    }

    fn finish_frame(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        // Abandon any suspended program: a no-tool-call turn completes
        // the frame, its text is the result.
        self.generation += 1;
        let result = match self.spine.frame().messages.last() {
            Some(Message::Assistant { text, .. }) => serde_json::Value::String(text.clone()),
            _ => serde_json::Value::Null,
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
        let frame = self.spine.frame();
        let mut system = frame.prompt.clone();
        if !frame.input.is_null() {
            system.push_str("\n\nInput:\n```json\n");
            system.push_str(&frame.input.to_string());
            system.push_str("\n```");
        }
        let mut messages = vec![Message::System { text: system }];
        messages.extend(frame.messages.iter().cloned());

        let tools = match &self.phase {
            Phase::Suspended(_, Suspension::Trapped(e))
                if matches!(e.resume, ResumeMode::NotResumable) =>
            {
                vec![TOOL_RUN_PROGRAM.to_owned()]
            }
            Phase::Suspended(..) => vec![TOOL_RESUME.to_owned(), TOOL_RUN_PROGRAM.to_owned()],
            _ => vec![TOOL_RUN_PROGRAM.to_owned()],
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

    /// `[#id] name(args) → preview` lines for the artifact menu.
    fn frame_artifacts(&self, tree: &Tree) -> Vec<String> {
        self.frame_segment(tree)
            .into_iter()
            .filter_map(|e| artifact_line(e, &self.effectful_tools))
            .collect()
    }

    fn new_artifacts(&self, tree: &Tree, since: u64) -> Vec<String> {
        self.frame_segment(tree)
            .into_iter()
            .filter(|e| e.id.as_u64() > since)
            .filter_map(|e| artifact_line(e, &self.effectful_tools))
            .collect()
    }
}

// ── helpers ─────────────────────────────────────────────────────────

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

fn render_diags(source: &str, diags: &[Diagnostic]) -> String {
    let rendered: Vec<String> = diags.iter().take(5).map(|d| d.render(source)).collect();
    format!("compile error:\n{}", rendered.join("\n"))
}

fn console_tail(vm: &VM) -> String {
    let lines = &vm.console_lines;
    if lines.is_empty() {
        return "(no console output)".into();
    }
    let start = lines.len().saturating_sub(CONSOLE_TAIL);
    lines[start..].join("\n")
}

fn preview(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s.len() <= 80 {
        return s;
    }
    let mut end = 77;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &s[..end], s.len())
}

fn artifact_line(event: &Event, effectful: &HashSet<String>) -> Option<String> {
    match &event.payload {
        EventPayload::Invoke { name, args, result } => Some(format!(
            "[#{}] {}({}) → {}{}",
            event.id.as_u64(),
            name,
            preview(args),
            preview(result),
            if effectful.contains(name.as_str()) {
                " ⚠ effectful: already happened; calling again repeats the effect"
            } else {
                ""
            }
        )),
        EventPayload::ProgramResult { value } => Some(format!(
            "[#{}] program result → {}",
            event.id.as_u64(),
            preview(value)
        )),
        _ => None,
    }
}

/// The `run_program` tool result for a successful run. Minimal Step 3
/// shape — Step 4 turns this into the real product surface.
fn completion_report(value: &serde_json::Value, vm: &VM, new_artifacts: Vec<String>) -> String {
    let mut report = format!(
        "program completed\nreturned: {value}\n\nconsole:\n{}",
        console_tail(vm)
    );
    if !new_artifacts.is_empty() {
        report.push_str("\n\nnew artifacts (fetch with tools.tool_result(id)):\n");
        report.push_str(&new_artifacts.join("\n"));
    }
    report
}

/// The `run_program` tool result for a raise/trapped error. Minimal
/// Step 3 shape — Step 4 makes it good.
fn condition_report(what: &str, resumable: bool, vm: &VM, artifacts: Vec<String>) -> String {
    let mut report = format!("{what}\n\nconsole:\n{}", console_tail(vm));
    if !artifacts.is_empty() {
        report.push_str("\n\nartifacts (fetch with tools.tool_result(id)):\n");
        report.push_str(&artifacts.join("\n"));
    }
    report.push_str("\n\nrestarts:\n");
    if resumable {
        report.push_str("- resume(value): continue as if the failed operation returned `value`\n");
    }
    report.push_str(
        "- run_program(source): replace the program (reuse prior work via the artifact ids above)",
    );
    report
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
                EventPayload::Label(_) => "Label",
                EventPayload::TextChunk(_) | EventPayload::ThinkingChunk(_) => "Chunk",
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
        assert_eq!(req.tools, vec![TOOL_RUN_PROGRAM.to_owned()]);
    }

    #[test]
    fn input_binding_reaches_the_program() {
        let mut tree = Tree::new(None);
        let mut state = AgentState::new_root(&mut tree, "agent", json!({ "n": 7 })).unwrap();
        state.kickoff();
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
    fn program_completion_then_frame_done() {
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

        // Final text turn completes the frame.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_text("the answer is 42")),
            )
            .unwrap();
        assert!(matches!(
            &out[..],
            [StepOutput::FrameDone(v)] if v == &json!("the answer is 42")
        ));
        assert!(state.is_done());
        assert_eq!(
            payload_kinds(&state, &tree),
            [
                "FrameStart",
                "User",
                "Assistant",
                "ProgramResult",
                "Tool",
                "Assistant",
                "FrameResult"
            ]
        );
    }

    #[test]
    fn fanout_batch_and_resolution_order() {
        let (mut tree, mut state) = setup();
        state.kickoff();
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
        state.kickoff();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "let = ;")),
            )
            .unwrap();
        // No Working: nothing ran.
        assert!(!out.iter().any(|o| matches!(o, StepOutput::Working)));
        let req = expect_request(&out);
        assert_eq!(req.tools, vec![TOOL_RUN_PROGRAM.to_owned()]);
        assert!(last_tool_text(&state).contains("compile error"));
        // No execution events were logged.
        assert_eq!(
            payload_kinds(&state, &tree),
            ["FrameStart", "Assistant", "Tool"]
        );
    }

    #[test]
    fn raise_reports_and_resume_continues() {
        let (mut tree, mut state) = setup();
        state.kickoff();
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

        // Suspended: report + restart tools.
        let req = expect_request(&settled);
        assert_eq!(
            req.tools,
            vec![TOOL_RESUME.to_owned(), TOOL_RUN_PROGRAM.to_owned()]
        );
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
        state.kickoff();
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "const v = null; return v.x;")),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let req = expect_request(&settled);
        assert_eq!(
            req.tools,
            vec![TOOL_RESUME.to_owned(), TOOL_RUN_PROGRAM.to_owned()]
        );

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
        state.kickoff();
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
        state.kickoff();
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
        )
        .unwrap();
        assert_eq!(tree.list_leaves().len(), 2, "caller + in-flight child");
        child.kickoff();
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
        state.kickoff();
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

    #[test]
    #[should_panic(expected = "mid-program user turns")]
    fn mid_program_user_turn_panics_for_now() {
        let (mut tree, mut state) = setup();
        state.kickoff();
        state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "await tools.fetch(1); return 0;")),
            )
            .unwrap();
        let _ = state.step(&mut tree, StepInput::UserTurn("are you done?".into()));
    }
}
