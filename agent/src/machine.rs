//! Sans-io branch step machine (8_HARNESS Step 3).
//!
//! One `Runner` drives one branch: a deterministic, IO-free core the
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
use crate::report::{Artifact, ArtifactState, preview};
use crate::types::*;

/// Tool names offered to the LLM. `run_program` is the primary tool;
/// `resume` appears only while suspended on a resumable condition.
pub const TOOL_RUN_PROGRAM: &str = "run_program";
pub const TOOL_RESUME: &str = "resume";

/// Program-facing tool names `dispatch_calls` interprets — **the one
/// place a `tools.*` name becomes a `Call` variant** (A2). Everything
/// downstream matches on the variant.
pub const TOOL_SPAWN: &str = "spawn";
pub const TOOL_ASK: &str = "ask";
pub const TOOL_TELL: &str = "tell";
/// `spawn` + `ask` in one call, kept verbatim from 8_HARNESS.
pub const TOOL_AGENT: &str = "agent";
pub const TOOL_TOOL_RESULT: &str = "tool_result";

/// Discovery. A host tool in the program's view like any other, but its
/// answer needs **live session state** (a branch's status), so the
/// session serves it inline instead of the registry. This const is the
/// one place the name is written.
pub const TOOL_AGENTS: &str = "agents";

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

/// Default budget for an agent's *answer* — the one value that deliberately
/// crosses into a mind's context: a program's `return` (into its own
/// agent) and a subagent's final turn (into its caller). Sized to a
/// typical source file so an ordinary read or summary lands in one shot
/// (DESIGN.md "The one exception"; 12_ANSWERS). The full value is always a
/// fetchable artifact; only the context copy is truncated past this. A
/// caller may raise a child's budget via `agent({ budget })`.
const DEFAULT_ANSWER_BUDGET: usize = 64 * 1024;

/// How many times a subagent whose final answer exceeds its budget is
/// re-prompted to tighten it before the host truncates it deterministically.
const ANSWER_RETRY_LIMIT: u8 = 1;

pub enum StepInput {
    /// The assistant's turn (logged with its author; tool calls
    /// dispatched).
    LlmResponse(LlmTurn),
    /// Settled calls, in resolution order. One door for all three call
    /// kinds: a host tool's result, a `Spawn`'s agent handle, a `Tell`'s
    /// delivery receipt, or an `Ask`'s answer — routing is by the
    /// **variant** already in the log, so the machine needs no second
    /// input for subagents.
    ToolResults(Vec<ToolResult>),
    /// Run one VM slice of at most `fuel` instructions.
    Tick { fuel: u64 },
}

/// One settled call. It is named by its **logged `Call` event id** — the
/// log's own key, which is also what the artifact menu shows and what
/// `tools.tool_result` takes, so there is no second id space to keep in
/// step with it.
pub struct ToolResult {
    pub call: EventId,
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
    /// Create these agents; settle each `Spawn` with `{ agent }`.
    Spawns(Vec<SpawnRequest>),
    /// Deliver these `Send`s. Each names a logged `Call::Send`, and the
    /// address, body and `expects_reply` all live there — the host reads
    /// the log rather than being handed a copy, which is the same
    /// by-reference discipline the `Post` itself follows.
    ///
    /// An `ask` stays pending until the recipient's `Answer` produces its
    /// `Result`; a `tell` is settled by its delivery receipt as soon as
    /// the `Post` lands.
    Sends(Vec<EventId>),
    /// This branch took a bare turn and went **idle**. Agents never
    /// close: idle costs nothing and the branch stays addressable, so a
    /// later question to it — from anyone — is just another post.
    ///
    /// `question` is the `Post` the turn answered: the oldest that was
    /// open, with an `Answer` naming it logged. It is `None` when nothing
    /// was open — then no `Answer` is logged and the turn's text is read
    /// where it sits. (The plan writes this output as
    /// `Answered { question, value }`; the option is what "a bare turn
    /// with nothing open logs no `Answer`" needs to stay expressible.)
    Answered {
        question: Option<EventId>,
        value: serde_json::Value,
    },
    /// The VM wants another `Tick`.
    Working,
}

/// One completed assistant turn, as an LLM client produced it.
///
/// A client speaks *for* a branch; it does not decide **who acted**. So
/// the `author` is not here: the harness stamps it when it logs the
/// `Message::Turn`, which is also what lets the user take a branch's turn
/// through the very same path (`Restart`).
#[derive(Clone, Debug, Default)]
pub struct LlmTurn {
    pub text: String,
    pub thinking: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// One message as it goes out to the API. Log `Message`s render into
/// these, and the **tool-role entries are derived** — from the run's
/// outcome and the events around it — never stored, which is why they
/// have no `EventPayload` counterpart.
///
/// Each variant is exactly one API role, chosen by the variant and never
/// by a flag.
#[derive(Clone, Debug, PartialEq)]
pub enum Rendered {
    /// user role: a `Post`, author-labelled, with any `input` previewed.
    User(String),
    /// assistant role: a `Turn`.
    Assistant {
        text: String,
        thinking: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    /// tool role: the derived report answering one of a `Turn`'s calls.
    /// The API requires every `tool_call_id` to be answered, so the
    /// renderer emits exactly one of these per tool call in a `Turn`.
    Tool { call_id: String, text: String },
}

#[derive(Debug)]
pub struct LlmRequest {
    /// The branch's system prompt, rebuilt verbatim from `Agent.system`.
    /// It is a *snapshot*, so a later card edit or a new registry tool
    /// never alters an existing conversation's cached prefix.
    pub system: String,
    pub messages: Vec<Rendered>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug)]
pub struct OutCall {
    /// The `Call::Invoke` event this settles.
    pub call: EventId,
    pub name: String,
    /// Positional arguments as a JSON array.
    pub args: serde_json::Value,
}

/// One agent to create. `name`, `charter` and `tools` live on the
/// `Call::Spawn` named by `call`, so the host reads them from the log.
#[derive(Debug)]
pub struct SpawnRequest {
    pub call: EventId,
    /// The child's answer budget (`agent({ budget })`); `None` → default.
    /// The one field not in the log: it is the *caller's* choice about
    /// its own context, not part of what the agent is.
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
}

/// How `resume(value)` re-enters a suspended run — the *live* half of a
/// suspension, kept beside the phase.
///
/// The vocabulary a suspension is described in lives in the log, as
/// [`Cause`]: that is what a report renders from and what survives a
/// crash. This is deliberately **not** the same value. A `VMError` is not
/// serialisable and only a live VM can consume one, so a `Cause` cannot
/// carry it — and the `Cause` variants that never ran a VM
/// (`CompileFailed`, `Refused`, `Interrupted`) have no live half at all.
enum ResumeWith {
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
    Suspended(Run, ResumeWith),
}

/// One call in flight, keyed by the `Call` event logged at dispatch.
/// The name, args and address live there, not here: the log is the
/// record, and the session state only has to route the settlement.
struct PendingCall {
    settle: Settle,
    /// Which run issued it: results from an abandoned run are still
    /// logged as artifacts (the physics happened) but not delivered.
    generation: u64,
}

/// What a landing `Result` does to the program.
enum Settle {
    /// Resolve (or reject) this promise with the outcome.
    Promise(PromisePtr),
    /// `tools.agent`'s sugar, the one call that is two: the `Spawn`'s
    /// `{ agent }` is not the program's answer, so the first question is
    /// issued to the new agent and **its** answer settles the promise.
    /// Both halves are ordinary logged calls with ordinary `Result`s —
    /// the desugaring lives here and nowhere downstream.
    ThenAsk {
        promise: PromisePtr,
        text: String,
        input: serde_json::Value,
        /// The `tools.agent(...)` call site: the `Send` shares it,
        /// because it *is* the same place in the source.
        site: u32,
    },
}

pub struct Runner {
    pub spine: Spine,
    /// The innermost `Agent` root above this branch's leaf — who the
    /// branch is a conversation with. Resolved once at construction; the
    /// leaf moves, the agent does not.
    agent: EventId,
    phase: Phase,
    generation: u64,
    pending: HashMap<EventId, PendingCall>,
    /// The dialect card (8_HARNESS Step 6), prepended to every system
    /// message ahead of the agent prompt (host-fed, registry-generated).
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
    /// Byte budget for this agent's *answer* into context (decisions 2, 6):
    /// program `return`s and (for a subagent) the final turn. Seeded from
    /// the spawning `agent({ budget })` or `DEFAULT_ANSWER_BUDGET`.
    answer_budget: usize,
    /// Re-prompts spent tightening an over-budget final answer (decision 4).
    answer_retries: u8,
    /// Refusals owed to the extra tool calls of the current turn. They are
    /// logged only once the first call's outcome has landed, so outcomes
    /// stay in call order and the positional pairing holds.
    deferred_refusals: Vec<String>,
}

enum SuspendCause {
    Raise {
        condition: String,
        payload: Option<Value>,
    },
    Trapped(VMError),
}

impl Runner {
    /// Root agent of a tree. `charter` is what the agent is for; the
    /// system prompt is assembled from it and the card and snapshotted on
    /// the `Agent` event.
    pub fn new_root(tree: &mut Tree, charter: impl Into<String>, card: &str) -> io::Result<Self> {
        let charter = charter.into();
        let system = assemble_system(card, &charter);
        let spine = tree.start_agent(None, None, charter, None, system)?;
        let mut state = Self::with_spine(tree, spine);
        state.dialect_card = card.to_owned();
        Ok(state)
    }

    /// A new agent rooted at `call_site` — the `Spawn` on the caller's
    /// branch (the host maps each `SpawnRequest` to one of these). The
    /// `Agent` is the agent's own root and outlives the caller, its
    /// program, and often the conversation that created it; `tools` sits
    /// here, on that root, because the registry enforces a child's
    /// allowlist from the agent itself and not from an event on its
    /// parent's branch.
    ///
    /// It carries **no question**. A spawn creates; asking is a separate
    /// act, and the first question arrives like every other — as a
    /// `Post` naming the `Send` that dispatched it. So a bare
    /// `tools.spawn` leaves an idle agent with nothing open, which is
    /// exactly what the driving rule wants: nothing to say, no request.
    pub fn new_agent(
        tree: &mut Tree,
        call_site: EventId,
        name: Option<String>,
        charter: impl Into<String>,
        tools: Option<Vec<String>>,
        budget: Option<usize>,
        card: &str,
    ) -> io::Result<Self> {
        let charter = charter.into();
        let system = assemble_system(card, &charter);
        let spine = tree.start_agent(Some(call_site), name, charter, tools, system)?;
        let mut state = Self::with_spine(tree, spine);
        state.dialect_card = card.to_owned();
        state.answer_budget = budget.unwrap_or(DEFAULT_ANSWER_BUDGET);
        Ok(state)
    }

    /// Resume an existing spine: a re-opened log, a fork, a re-anchor.
    pub fn with_spine(tree: &Tree, spine: Spine) -> Self {
        let agent = tree.enclosing_agent(spine.leaf_id).unwrap_or(spine.leaf_id);
        Runner {
            spine,
            agent,
            phase: Phase::Idle,
            generation: 0,
            pending: HashMap::new(),
            dialect_card: String::new(),
            last_vm: None,
            status_transitions: Vec::new(),
            answer_budget: DEFAULT_ANSWER_BUDGET,
            answer_retries: 0,
            deferred_refusals: Vec::new(),
        }
    }

    /// The dialect card rendered as the root of the system message
    /// (the host generates it from the tool registry). It seeds the
    /// snapshot on a *new* agent's root; an existing branch's system
    /// prompt is the snapshot and never re-derived.
    pub fn set_dialect_card(&mut self, card: String) {
        self.dialect_card = card;
    }

    /// This branch's agent — the innermost `Agent` root on its path.
    pub fn agent_id(&self) -> EventId {
        self.agent
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

    /// Whether the agent can accept a `UserTurn` right now.
    pub fn is_idle(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }

    /// One-word phase description for agent lists / status lines.
    pub fn status(&self) -> &'static str {
        match self.phase {
            Phase::Idle => "idle",
            Phase::AwaitingLlm => "awaiting llm",
            Phase::Running(_) => "running",
            Phase::Suspended(..) => "suspended",
        }
    }

    /// Posts this branch owes an answer to, oldest first.
    pub fn open(&self) -> &[EventId] {
        &self.spine.context().open
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

    /// Start the conversation without a user turn — how child contexts
    /// begin (their input arrived in `Agent`).
    pub fn kickoff(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        let _ = tree;
        assert!(matches!(self.phase, Phase::Idle), "kickoff on a busy agent");
        self.phase = Phase::AwaitingLlm;
        Ok(vec![self.render_request(tree)])
    }

    pub fn step(&mut self, tree: &mut Tree, input: StepInput) -> io::Result<Vec<StepOutput>> {
        match input {
            StepInput::LlmResponse(message) => self.on_llm_response(tree, message),
            StepInput::ToolResults(batch) => self.on_tool_results(tree, batch),
            StepInput::Tick { fuel } => self.on_tick(tree, fuel),
        }
    }

    /// Deliver a message into this branch — **rule A**: a post is logged
    /// on the branch it is delivered into, whoever authored it. The user,
    /// another agent's `Send`, and a harness notice all come through this
    /// one door; who is speaking is `from`, and where the body lives is
    /// `origin`.
    ///
    /// Returns the `Post`'s id — a `tell`'s delivery receipt names it —
    /// beside what the branch does next: an idle branch starts a turn, a
    /// busy one has the post on its path for its next request (rule B's
    /// suspend-at-the-next-slice is B3).
    ///
    /// It is a door of its own rather than a `StepInput` because it
    /// **returns a fact about the log** the caller needs, the way
    /// `kickoff` does.
    pub fn deliver(
        &mut self,
        tree: &mut Tree,
        from: Author,
        origin: Origin,
    ) -> io::Result<(EventId, Vec<StepOutput>)> {
        let post = tree.append(
            &mut self.spine,
            EventPayload::Message(Message::Post { from, origin }),
        )?;
        let out = match self.phase {
            Phase::Idle => {
                self.phase = Phase::AwaitingLlm;
                vec![self.render_request(tree)]
            }
            // Logged on arrival, so it is visible and crash-safe already;
            // it reaches the LLM at the branch's next request.
            Phase::AwaitingLlm | Phase::Running(_) | Phase::Suspended(..) => Vec::new(),
        };
        Ok((post, out))
    }

    // ── input handlers ──────────────────────────────────────────────

    fn on_llm_response(&mut self, tree: &mut Tree, turn: LlmTurn) -> io::Result<Vec<StepOutput>> {
        assert!(
            matches!(self.phase, Phase::AwaitingLlm | Phase::Suspended(..)),
            "LlmResponse with no request in flight"
        );
        let tool_calls = turn.tool_calls.clone();
        // The branch's own LLM acted: stamp the author here, where the
        // agent id is known, rather than asking a client to invent it.
        let message = Message::Turn {
            author: Author::Agent(self.agent_id()),
            text: turn.text,
            thinking: turn.thinking,
            tool_calls: turn.tool_calls,
        };
        let assistant_id = tree.append(&mut self.spine, EventPayload::Message(message))?;

        let Some(call) = tool_calls.first().cloned() else {
            // No tool call: the assistant's text completes the agent.
            return self.answer_open(tree);
        };
        // Every call gets exactly one outcome, and outcomes are logged in
        // **call order** so the positional pairing holds. The first call
        // may drive the VM and settle much later, so refusals of the ones
        // after it are deferred until its outcome has landed.
        self.deferred_refusals = (1..tool_calls.len())
            .map(|_| "one tool call per turn; this call was ignored".to_owned())
            .collect();

        match call.name.as_str() {
            TOOL_RUN_PROGRAM => {
                let Some(source) = call.arguments.get("source").and_then(|s| s.as_str()) else {
                    self.refuse(tree, "run_program needs a `source` string")?;
                    self.flush_refusals(tree)?;
                    self.phase = Phase::AwaitingLlm;
                    return Ok(vec![self.render_request(tree)]);
                };
                // `attachments` is this run's authored content (name → string),
                // seeded as the program's `attachments` const. A malformed
                // shape is a cheap repair loop, like a missing source.
                let attachments = match attachments_from_args(&call.arguments) {
                    Ok(a) => a,
                    Err(msg) => {
                        self.refuse(tree, &msg)?;
                        self.flush_refusals(tree)?;
                        self.phase = Phase::AwaitingLlm;
                        return Ok(vec![self.render_request(tree)]);
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
                    Err(message) => {
                        // A compile (or input-binding) error is an outcome
                        // like any other — no VM was built, so this run has
                        // no console and no artifacts. The repair loop is
                        // unchanged; only where the text lives has moved.
                        tree.append(
                            &mut self.spine,
                            EventPayload::Condition {
                                cause: Cause::CompileFailed { message },
                                site: 0,
                                stack: Vec::new(),
                            },
                        )?;
                        self.flush_refusals(tree)?;
                        self.phase = Phase::AwaitingLlm;
                        Ok(vec![self.render_request(tree)])
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
                            ResumeWith::Raise => {
                                let v = json_arg(&mut run.vm, &value);
                                run.vm.resume_raise(v);
                                true
                            }
                            ResumeWith::Trapped(e) => match e.resume {
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
                            self.refuse(tree, "this condition is not resumable; use run_program")?;
                            self.flush_refusals(tree)?;
                            Ok(vec![self.render_request(tree)])
                        }
                    }
                    other => {
                        self.phase = other;
                        self.refuse(tree, "nothing to resume")?;
                        self.flush_refusals(tree)?;
                        if !matches!(self.phase, Phase::Suspended(..)) {
                            self.phase = Phase::AwaitingLlm;
                        }
                        Ok(vec![self.render_request(tree)])
                    }
                }
            }
            unknown => {
                self.refuse(
                    tree,
                    &format!("unknown tool `{unknown}` (have: run_program, resume)"),
                )?;
                self.flush_refusals(tree)?;
                if !matches!(self.phase, Phase::Suspended(..)) {
                    self.phase = Phase::AwaitingLlm;
                }
                Ok(vec![self.render_request(tree)])
            }
        }
    }

    fn on_tool_results(
        &mut self,
        tree: &mut Tree,
        batch: Vec<ToolResult>,
    ) -> io::Result<Vec<StepOutput>> {
        let mut delivered = false;
        let mut sends = Vec::new();
        for tr in batch {
            let Some(p) = self.pending.remove(&tr.call) else {
                continue; // unknown or duplicate — nothing to log
            };
            // Resolution order is arrival order: the `Result` lands now,
            // naming the `Call` logged at dispatch.
            let outcome = match &tr.result {
                Ok(v) => Outcome::Delivered(v.clone()),
                Err(msg) => Outcome::Failed(msg.clone()),
            };
            tree.append(
                &mut self.spine,
                EventPayload::Result {
                    call: tr.call,
                    outcome,
                },
            )?;

            // Deliver only into the run that issued the call.
            if p.generation != self.generation {
                continue;
            }
            if !matches!(self.phase, Phase::Running(_) | Phase::Suspended(..)) {
                continue;
            }
            // `tools.agent` is the one call that is two: the spawn just
            // settled, so now ask the new agent its first question and
            // let *that* `Result` settle the program's promise.
            if let Settle::ThenAsk {
                promise,
                text,
                input,
                site,
            } = p.settle
            {
                match &tr.result {
                    Ok(value) => {
                        let agent = value
                            .get("agent")
                            .and_then(|v| v.as_u64())
                            .map(EventId::new)
                            .expect("a Spawn settles with { agent }");
                        let send = self.issue_call(
                            tree,
                            Call::Send {
                                to: Address::Branch(agent),
                                text,
                                input,
                                expects_reply: true,
                                site,
                            },
                            Settle::Promise(promise),
                        )?;
                        sends.push(send);
                    }
                    Err(msg) => {
                        let val = Value::String(RcStr::from(msg.as_str()));
                        self.settling_vm()
                            .reject_promise(promise, val)
                            .expect("pending promise is settleable");
                        delivered = true;
                    }
                }
                continue;
            }
            let Settle::Promise(promise) = p.settle else {
                unreachable!("ThenAsk handled above");
            };
            let vm = self.settling_vm();
            match tr.result {
                Ok(v) => {
                    let val = json_arg(vm, &v);
                    vm.resolve_promise(promise, val)
                        .expect("pending promise is settleable");
                }
                Err(msg) => {
                    let val = Value::String(RcStr::from(msg.as_str()));
                    vm.reject_promise(promise, val)
                        .expect("pending promise is settleable");
                }
            }
            delivered = true;
        }
        let mut out = Vec::new();
        if !sends.is_empty() {
            out.push(StepOutput::Sends(sends));
        }
        // A suspended run stays suspended (results land for later); a
        // running one can make progress now.
        if delivered && matches!(self.phase, Phase::Running(_)) {
            out.push(StepOutput::Working);
        }
        Ok(out)
    }

    /// The VM a landing `Result` settles into — live while running or
    /// suspended (a suspended run's results land for later).
    fn settling_vm(&mut self) -> &mut VM {
        match &mut self.phase {
            Phase::Running(run) | Phase::Suspended(run, _) => &mut run.vm,
            _ => unreachable!("no VM to settle into"),
        }
    }

    fn on_tick(&mut self, tree: &mut Tree, fuel: u64) -> io::Result<Vec<StepOutput>> {
        if !matches!(self.phase, Phase::Running(_)) {
            return Ok(Vec::new());
        }
        self.pump(tree, fuel)
    }

    // ── program driving ─────────────────────────────────────────────

    /// Compile + bind the host consts (`input` from the agent, `attachments`
    /// from this run). `Err` is the rendered repair-loop report.
    fn start_program(
        &mut self,
        program_id: EventId,
        source: &str,
        attachments: serde_json::Value,
        call_id: String,
    ) -> Result<Run, String> {
        let program = compile(source).map_err(|diags| render_diags(source, &diags))?;
        // The whole `input` reaches the program even though the context
        // saw only a bounded preview of it.
        let vm = VM::for_program_with(program, self.spine.context().input().clone(), attachments)
            .map_err(|e| format!("program setup failed: {}", e.message))?;
        Ok(Run {
            program_id,
            call_id,
            vm,
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
                    let progressed = self.dispatch_calls(tree, calls, &mut out)?;
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

    /// Classify one `Pending` batch. **This is the one place a `tools.*`
    /// name becomes a `Call` variant** (17_BRANCHES A2): everything
    /// downstream — the artifact menu, reconciliation, re-attach, routing
    /// an answer home — matches on the variant, never on the string again.
    ///
    /// Artifact fetches are answered from the log immediately and log
    /// nothing (returns true if any were — the program can run again);
    /// `spawn`/`ask`/`tell` become `Spawn`/`Send` calls, `agent` desugars
    /// to a spawn that then asks, and everything else becomes
    /// `ToolCalls`. Every call that leaves here is logged as a `Call`
    /// event *at dispatch*, settled later by exactly one `Result`.
    fn dispatch_calls(
        &mut self,
        tree: &mut Tree,
        calls: Vec<InvokeCall>,
        out: &mut Vec<StepOutput>,
    ) -> io::Result<bool> {
        let mut tool_calls = Vec::new();
        let mut spawns = Vec::new();
        let mut sends = Vec::new();
        let mut progressed = false;

        for call in calls {
            match call.name.as_str() {
                TOOL_TOOL_RESULT => {
                    let fetched = self.fetch_artifact(&*tree, &call);
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
                TOOL_SPAWN => {
                    let arg = self.first_arg(&call);
                    match arg.get("charter").and_then(|c| c.as_str()) {
                        Some(charter) => {
                            let spawn = self.issue_call(
                                tree,
                                Call::Spawn {
                                    name: string_field(&arg, "name"),
                                    charter: charter.to_owned(),
                                    tools: allowlist_field(&arg),
                                    site: call.site,
                                },
                                Settle::Promise(call.promise),
                            )?;
                            spawns.push(SpawnRequest {
                                call: spawn,
                                budget: None,
                            });
                        }
                        None => {
                            self.reject_call(
                                call.promise,
                                "tools.spawn needs { charter } — what the agent is for \
                                 (optionally { name, tools })",
                            );
                            progressed = true;
                        }
                    }
                }
                TOOL_ASK | TOOL_TELL => {
                    let expects_reply = call.name == TOOL_ASK;
                    let arg = self.first_arg(&call);
                    let text = arg.get("text").and_then(|t| t.as_str()).map(str::to_owned);
                    match (text, self.resolve_address(tree, arg.get("to"))) {
                        (Some(text), Ok(to)) => {
                            let send = self.issue_call(
                                tree,
                                Call::Send {
                                    to,
                                    text,
                                    input: arg.get("input").cloned().unwrap_or_default(),
                                    expects_reply,
                                    site: call.site,
                                },
                                Settle::Promise(call.promise),
                            )?;
                            sends.push(send);
                        }
                        (None, _) => {
                            self.reject_call(
                                call.promise,
                                &format!("tools.{} needs {{ text }}", call.name),
                            );
                            progressed = true;
                        }
                        (_, Err(msg)) => {
                            self.reject_call(call.promise, &msg);
                            progressed = true;
                        }
                    }
                }
                // Sugar, kept verbatim: spawn + ask. Two logged calls,
                // one program promise — the `Spawn`'s `{ agent }` is not
                // the answer, so the `Send` issued when it lands is what
                // settles the program (`Settle::ThenAsk`).
                TOOL_AGENT => {
                    let arg = self.first_arg(&call);
                    match arg.get("prompt").and_then(|p| p.as_str()) {
                        Some(prompt) => {
                            let spawn = self.issue_call(
                                tree,
                                Call::Spawn {
                                    name: None,
                                    charter: prompt.to_owned(),
                                    tools: None,
                                    site: call.site,
                                },
                                Settle::ThenAsk {
                                    promise: call.promise,
                                    text: prompt.to_owned(),
                                    input: arg.get("input").cloned().unwrap_or_default(),
                                    site: call.site,
                                },
                            )?;
                            spawns.push(SpawnRequest {
                                call: spawn,
                                budget: arg
                                    .get("budget")
                                    .and_then(|b| b.as_u64())
                                    .map(|b| b as usize),
                            });
                        }
                        None => {
                            self.reject_call(call.promise, "tools.agent needs { prompt, input }");
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
                    let id = self.issue_call(
                        tree,
                        Call::Invoke {
                            name: call.name.clone(),
                            args: args.clone(),
                            site: call.site,
                        },
                        Settle::Promise(call.promise),
                    )?;
                    tool_calls.push(OutCall {
                        call: id,
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
            out.push(StepOutput::Spawns(spawns));
        }
        if !sends.is_empty() {
            out.push(StepOutput::Sends(sends));
        }
        Ok(progressed)
    }

    /// The first argument of an options-object call (`spawn`/`ask`/
    /// `tell`/`agent`), as JSON.
    fn first_arg(&mut self, call: &InvokeCall) -> serde_json::Value {
        let vm = self.running_vm();
        call.args
            .first()
            .map(|v| value_json(vm, v))
            .unwrap_or(serde_json::Value::Null)
    }

    /// Reject a malformed call in place. Nothing is logged: the call was
    /// never dispatched, so it has no `Call` event and owes no `Result` —
    /// the rejection is the program's to catch (6_LANGUAGE Part B), and
    /// only an uncaught one traps into a condition.
    fn reject_call(&mut self, promise: PromisePtr, message: &str) {
        let v = Value::String(RcStr::from(message));
        self.running_vm()
            .reject_promise(promise, v)
            .expect("fresh promise");
    }

    /// Resolve an `ask`/`tell` address **before** the `Send` is logged,
    /// so nothing unresolved ever reaches the log.
    ///
    /// - omitted → the author of the oldest open post: *whoever asked
    ///   you*. For a root conversation that is the human, for a subagent
    ///   its parent, and a program never needs to know which.
    /// - a branch id → that branch.
    /// - an agent id with exactly one branch → that branch. An
    ///   **ambiguous** agent id (it has been forked) is a rejected call
    ///   naming the branches, because guessing which fork owes the answer
    ///   is exactly the race the one-owner rule exists to prevent.
    fn resolve_address(
        &self,
        tree: &Tree,
        to: Option<&serde_json::Value>,
    ) -> Result<Address, String> {
        let Some(to) = to.filter(|v| !v.is_null()) else {
            let Some(&question) = self.spine.context().open.first() else {
                return Err(
                    "tools.ask/tell with no `to` answers whoever asked you, but \
                            nothing is open on this branch — pass { to } (an agent or \
                            branch id from tools.agents())"
                        .into(),
                );
            };
            return match asker_of(tree, question) {
                Some(Author::User) => Ok(Address::User),
                Some(Author::Agent(agent)) => Ok(Address::Branch(agent)),
                // A harness notice never expects a reply, so it cannot be
                // the oldest *open* post.
                _ => Err(format!(
                    "post #{} has no author to reply to",
                    question.as_u64()
                )),
            };
        };
        if to.as_str() == Some("user") {
            return Ok(Address::User);
        }
        let Some(id) = to.as_u64().filter(|n| *n > 0).map(EventId::new) else {
            return Err(format!(
                "`to` must be an agent or branch id (a number), or \"user\"; got {to}"
            ));
        };
        match tree.events.get(&id).map(|e| &e.payload) {
            Some(EventPayload::Fork { .. }) => Ok(Address::Branch(id)),
            Some(EventPayload::Agent { .. }) => {
                let branches = tree.branches_of_agent(id);
                match branches.len() {
                    1 => Ok(Address::Branch(branches[0])),
                    _ => Err(format!(
                        "agent #{} has {} live branches ({}) — address one of them, \
                         not the agent",
                        id.as_u64(),
                        branches.len(),
                        branches
                            .iter()
                            .map(|b| format!("#{}", b.as_u64()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                }
            }
            _ => Err(format!(
                "#{} is not an agent or a branch — tools.agents() lists both ids",
                id.as_u64()
            )),
        }
    }

    fn running_vm(&mut self) -> &mut VM {
        match &mut self.phase {
            Phase::Running(run) => &mut run.vm,
            _ => unreachable!("no running VM"),
        }
    }

    /// Log a `Call` at dispatch and remember how to settle it. Returns the
    /// `Call` event's id — the log's own key, which the host echoes back
    /// with the result and which the artifact menu names.
    fn issue_call(&mut self, tree: &mut Tree, call: Call, settle: Settle) -> io::Result<EventId> {
        let logged = tree.append(&mut self.spine, EventPayload::Call(call))?;
        self.pending.insert(
            logged,
            PendingCall {
                settle,
                generation: self.generation,
            },
        );
        Ok(logged)
    }

    /// Serve `tools.tool_result(id)` from the log. Accepts a `Result` id
    /// or the id of the **call** it settles — the menu names calls, so a
    /// program reuses exactly the ids it was shown. Ids are scoped to this
    /// agent's spine segment (decision 3: never ancestor artifacts).
    fn fetch_artifact(&self, tree: &Tree, call: &InvokeCall) -> Result<serde_json::Value, String> {
        let id = match call.args.first() {
            Some(Value::PosInt(n)) => *n,
            _ => return Err("tool_result needs a numeric artifact id".into()),
        };
        let segment = self.agent_segment(tree);
        let Some(event) = segment.iter().find(|e| e.id.as_u64() == id) else {
            return Err(format!("no artifact #{id} in this agent"));
        };
        match &event.payload {
            EventPayload::Result { outcome, .. } => outcome_json(outcome),
            EventPayload::Call(_) => match settlement_of(&segment, event.id) {
                Some(outcome) => outcome_json(outcome),
                None => Err(format!("call #{id} has no result yet")),
            },
            EventPayload::Return { value } => Ok(value.clone()),
            _ => Err(format!("event #{id} is not an artifact")),
        }
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
            let id = self.issue_call(
                tree,
                Call::Invoke {
                    name: call.name.clone(),
                    args: args.clone(),
                    site: call.site,
                },
                Settle::Promise(call.promise),
            )?;
            fire_and_forget.push(OutCall {
                call: id,
                name: call.name.clone(),
                args,
            });
        }
        self.generation += 1;

        // "Completed ⇒ `Return`" holds without exception — a program that
        // ends without a `return` still logs `Return { value: null }` —
        // which is what makes recovery decidable from the log alone.
        let outcome = tree.append(
            &mut self.spine,
            EventPayload::Return {
                value: value_json.clone(),
            },
        )?;
        // The console is a diagnostic stream, capped with an explicit
        // marker; the report carries only a bounded tail of it.
        tree.append(
            &mut self.spine,
            EventPayload::Console {
                lines: crate::report::cap_console(
                    &run.vm.console_lines,
                    &format!("console event follows #{}", outcome.as_u64()),
                ),
            },
        )?;
        self.flush_refusals(tree)?;

        if !fire_and_forget.is_empty() {
            out.push(StepOutput::ToolCalls(fire_and_forget));
        }
        self.note_status(run.program_id, ProgramStatus::Completed);
        self.last_vm = Some(run.vm);
        self.phase = Phase::AwaitingLlm;
        out.push(self.render_request(tree));
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

        // Split the live suspension in two: a serialisable `Cause` for
        // the log (everything the report needs) and a `ResumeWith` handle
        // the *live* VM needs to resume. A `VMError` is not serialisable
        // and only a live VM can consume one, so the two cannot be the
        // same value.
        let (cause, site, suspension) = match cause {
            SuspendCause::Raise { condition, payload } => {
                let payload = payload.map(|v| value_json(&run.vm, &v));
                // `step()` advanced `ip` past the `Raise`, so the raise
                // site is the previous slot.
                let site = span_at(&run.vm, (run.vm.ip as usize).saturating_sub(1));
                (
                    Cause::Raised {
                        name: condition,
                        payload,
                    },
                    site,
                    ResumeWith::Raise,
                )
            }
            SuspendCause::Trapped(e) => {
                let site = span_at(&run.vm, e.ip as usize);
                let cause = Cause::Trapped {
                    kind: format!("{:?}", e.kind),
                    message: e.message.clone(),
                    resumable: matches!(e.resume, ResumeMode::PushValueThenContinue),
                };
                (cause, site, ResumeWith::Trapped(e))
            }
        };

        let stack: Vec<String> = run
            .vm
            .frames()
            .iter()
            .map(|f| f.name().to_owned())
            .collect();
        let console = run.vm.console_lines.clone();
        let program_id = run.program_id;
        self.phase = Phase::Suspended(run, suspension);
        self.note_status(program_id, ProgramStatus::Suspended);
        // The outcome carries the site and the stack because those were
        // the last inputs that lived only in the VM, and the VM is never
        // persisted.
        let outcome = tree.append(
            &mut self.spine,
            EventPayload::Condition { cause, site, stack },
        )?;
        tree.append(
            &mut self.spine,
            EventPayload::Console {
                lines: crate::report::cap_console(
                    &console,
                    &format!("console event follows #{}", outcome.as_u64()),
                ),
            },
        )?;
        self.flush_refusals(tree)?;
        out.push(self.render_request(tree));
        Ok(out)
    }

    /// A bare turn answers the **oldest post that was open** and the
    /// branch goes idle. Nothing closes: idle costs nothing and the
    /// branch stays addressable, so a later question to it — from anyone
    /// — is just another post, answered to whoever asked *that* one.
    fn answer_open(&mut self, tree: &mut Tree) -> io::Result<Vec<StepOutput>> {
        // A no-tool-call turn abandons any suspended program — never the
        // physics: in-flight calls stay pending and their results are
        // still logged as artifacts when they arrive.
        if let Phase::Suspended(run, _) = std::mem::replace(&mut self.phase, Phase::Idle) {
            self.note_status(run.program_id, ProgramStatus::Failed);
            self.last_vm = Some(run.vm);
        }
        self.generation += 1;
        self.phase = Phase::Idle;

        let text = match self.spine.context().messages.last() {
            Some(Message::Turn { text, .. }) => text.clone(),
            _ => String::new(),
        };
        let Some(question) = self.spine.context().open.first().copied() else {
            // Nothing was open: no `Answer` is logged and the branch is
            // simply idle. The user reads the text where it sits.
            return Ok(vec![StepOutput::Answered {
                question: None,
                value: text_value(&text),
            }]);
        };

        // Whether this answer is *mind-bound* is a fact about who asked:
        // an answer to the human is delivered in full, an answer that
        // crosses into another agent's context is budgeted (decision 4).
        // That replaces `is_root`, which asked the same question of the
        // branch instead of the asker.
        let mind_bound = !matches!(
            self.spine.context().messages.iter().find_map(|m| match m {
                Message::Post { from, .. } => Some(*from),
                _ => None,
            }),
            Some(Author::User)
        ) && matches!(asker_of(tree, question), Some(Author::Agent(_)));

        if mind_bound && text.len() > self.answer_budget && self.answer_retries < ANSWER_RETRY_LIMIT
        {
            self.answer_retries += 1;
            let nudge = format!(
                "Your answer is {} bytes; the budget is {}. Tighten it to a digest, or \
                 write a large product with create_file and report its path.",
                text.len(),
                self.answer_budget
            );
            tree.append(
                &mut self.spine,
                EventPayload::Message(Message::Post {
                    from: Author::Harness,
                    origin: Origin::Direct {
                        text: nudge,
                        input: serde_json::Value::Null,
                        expects_reply: false,
                    },
                }),
            )?;
            self.phase = Phase::AwaitingLlm;
            return Ok(vec![self.render_request(tree)]);
        }

        let value = if mind_bound && text.len() > self.answer_budget {
            serde_json::Value::String(truncate_answer(&text, self.answer_budget))
        } else {
            text_value(&text)
        };
        tree.append(
            &mut self.spine,
            EventPayload::Answer {
                question,
                value: value.clone(),
            },
        )?;
        Ok(vec![StepOutput::Answered {
            question: Some(question),
            value,
        }])
    }

    // ── rendering ───────────────────────────────────────────────────

    /// Build the request from the **log**: the system prompt from the
    /// `Agent`'s snapshot, the posts and turns from this branch's path,
    /// and one derived tool message per tool call in each turn.
    ///
    /// Placing each tool message immediately after the `Turn` whose call
    /// it answers is what keeps the API's adjacency rule satisfied — the
    /// completion API rejects anything between an assistant tool call and
    /// its tool result.
    fn render_request(&self, tree: &Tree) -> StepOutput {
        // The system prompt is rebuilt from the `Agent`'s snapshot, not
        // re-derived from the registry: the prefix is immutable, so a
        // later card edit must not alter an existing conversation.
        let system = self.spine.context().system.clone();
        let messages = self.render_messages(tree);

        let tools = match &self.phase {
            Phase::Suspended(_, ResumeWith::Trapped(e))
                if matches!(e.resume, ResumeMode::NotResumable) =>
            {
                vec![run_program_spec()]
            }
            Phase::Suspended(..) => vec![resume_spec(), run_program_spec()],
            _ => vec![run_program_spec()],
        };
        StepOutput::LlmRequest(LlmRequest {
            system,
            messages,
            tools,
        })
    }

    /// The rendered message list for a request: the branch's posts and
    /// turns, with each turn immediately followed by one derived tool
    /// message per tool call it made.
    ///
    /// Pairing is **positional**: the k-th outcome after a turn answers
    /// its k-th tool call. The machine keeps that true by deferring a
    /// refusal until after the outcome of the call that preceded it.
    fn render_messages(&self, tree: &Tree) -> Vec<Rendered> {
        let leaf = self.spine.leaf_id;
        let mut out = Vec::new();
        for event in self.agent_segment(tree) {
            let EventPayload::Message(msg) = &event.payload else {
                continue;
            };
            match msg {
                Message::Post { from, .. } => {
                    // Resolve a by-reference body the way a `Context`
                    // does — the log stays copy-free.
                    let resolved = tree.resolve(msg);
                    let Message::Post { origin, .. } = &resolved else {
                        continue;
                    };
                    out.push(Rendered::User(crate::report::render_post(*from, origin)));
                }
                Message::Turn {
                    text,
                    thinking,
                    tool_calls,
                    ..
                } => {
                    out.push(Rendered::Assistant {
                        text: text.clone(),
                        thinking: thinking.clone(),
                        tool_calls: tool_calls.clone(),
                    });
                    let outcomes = crate::report::outcomes_of_turn(tree, leaf, event.id);
                    for (call, outcome) in tool_calls.iter().zip(outcomes) {
                        out.push(Rendered::Tool {
                            call_id: call.id.clone(),
                            text: crate::report::derive_report(
                                tree,
                                leaf,
                                outcome,
                                self.answer_budget,
                            ),
                        });
                    }
                }
            }
        }
        out
    }

    /// Log the refusals deferred behind a VM-driving call, now that its
    /// outcome has landed — keeping outcomes in call order.
    fn flush_refusals(&mut self, tree: &mut Tree) -> io::Result<()> {
        for reason in std::mem::take(&mut self.deferred_refusals) {
            self.refuse(tree, &reason)?;
        }
        Ok(())
    }

    /// Refuse a call: log a `Condition{Refused}` so the call still has
    /// exactly one outcome event, and the report renders from it rather
    /// than from replayed eligibility.
    fn refuse(&mut self, tree: &mut Tree, reason: &str) -> io::Result<()> {
        tree.append(
            &mut self.spine,
            EventPayload::Condition {
                cause: Cause::Refused {
                    reason: reason.to_owned(),
                },
                site: 0,
                stack: Vec::new(),
            },
        )?;
        Ok(())
    }

    /// Events of this agent's spine segment (its `Agent` down to
    /// the leaf), in log order.
    fn agent_segment<'t>(&self, tree: &'t Tree) -> Vec<&'t Event> {
        let mut events = Vec::new();
        let mut current = self.spine.leaf_id;
        while let Some(event) = tree.events.get(&current) {
            let is_agent_root = matches!(event.payload, EventPayload::Agent { .. });
            events.push(event);
            if is_agent_root {
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
}

// ── helpers ─────────────────────────────────────────────────────────

/// Assemble an agent's system prompt: the dialect card, then the charter.
/// The result is snapshotted on the `Agent` event and never re-derived —
/// `input` is *not* part of it, because machine-bound data belongs on the
/// post that carries it, previewed rather than dumped into context.
fn assemble_system(card: &str, charter: &str) -> String {
    let mut system = String::new();
    if !card.is_empty() {
        system.push_str(card);
        system.push_str("\n\n");
    }
    system.push_str(charter);
    system
}

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

/// An optional string field of an options object (`{ name }`).
fn string_field(arg: &serde_json::Value, key: &str) -> Option<String> {
    arg.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

/// A `{ tools: [...] }` allowlist, if the call named one. Absent means
/// "inherit the caller's" — the registry resolves that from the child's
/// own `Agent` root, not from this call.
fn allowlist_field(arg: &serde_json::Value) -> Option<Vec<String>> {
    let items = arg.get("tools")?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
    )
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

/// The `Result` settling `call`, if one landed on this path.
fn settlement_of<'e>(segment: &[&'e Event], call: EventId) -> Option<&'e Outcome> {
    segment.iter().find_map(|e| match &e.payload {
        EventPayload::Result { call: c, outcome } if *c == call => Some(outcome),
        _ => None,
    })
}

/// The artifact menu as a **projection over the events on this path**
/// (17_BRANCHES): every call that landed here, plus every call still
/// pending, plus prior program results. Nothing maintains a store — the
/// log is the cache and the event id is the key.
///
/// Rows are named by the **call** id, which is what a program reuses:
/// `tools.tool_result` resolves a call id through to its `Result`.
pub(crate) fn menu_rows(segment: &[&Event], since: u64) -> Vec<Artifact> {
    segment
        .iter()
        .filter(|e| e.id.as_u64() > since)
        .filter_map(|event| {
            let id = event.id.as_u64();
            match &event.payload {
                // A row's label comes from the call *variant*; its value
                // (or its absence) from the `Result`.
                EventPayload::Call(call) => Some(Artifact {
                    id,
                    label: call_label(call),
                    state: match settlement_of(segment, event.id) {
                        Some(Outcome::Delivered(v)) => ArtifactState::Delivered(v.clone()),
                        Some(Outcome::Failed(msg)) => ArtifactState::Failed(msg.clone()),
                        // Only one pending kind can be re-attached: a
                        // `Send`'s answer is still coming, while an
                        // `Invoke`'s worker died with the process.
                        None => match call {
                            Call::Send { .. } => ArtifactState::PendingSend,
                            Call::Spawn { .. } | Call::Invoke { .. } => {
                                ArtifactState::PendingInvoke
                            }
                        },
                    },
                }),
                EventPayload::Return { value } => Some(Artifact {
                    id,
                    label: "program result".into(),
                    state: ArtifactState::Delivered(value.clone()),
                }),
                _ => None,
            }
        })
        .collect()
}

/// A menu row's label, read from the call variant — never by re-parsing a
/// tool name. `ask` versus `tell` is `expects_reply`, the one place the
/// difference is visible.
fn call_label(call: &Call) -> String {
    match call {
        Call::Send {
            to,
            text,
            expects_reply,
            ..
        } => format!(
            "{}({}, {})",
            if *expects_reply { "ask" } else { "tell" },
            address_label(to),
            preview(&serde_json::Value::String(text.clone()))
        ),
        Call::Spawn { name, .. } => format!("spawn({})", name.as_deref().unwrap_or("<unnamed>")),
        Call::Invoke { name, args, .. } => format!("{}({})", name, preview(args)),
    }
}

fn address_label(to: &Address) -> String {
    match to {
        Address::User => "user".into(),
        Address::Branch(id) => format!("#{}", id.as_u64()),
    }
}

/// A settled call's value for the program: a delivered value resolves,
/// a failure rejects with its reason.
fn outcome_json(outcome: &Outcome) -> Result<serde_json::Value, String> {
    match outcome {
        Outcome::Delivered(v) => Ok(v.clone()),
        Outcome::Failed(msg) => Err(msg.clone()),
    }
}

/// The source byte offset of instruction `ip` — what a `Condition`
/// records so its report can point a caret without a live VM.
fn span_at(vm: &VM, ip: usize) -> u32 {
    vm.spans.get(ip).copied().unwrap_or(0)
}

/// An empty final turn answers with `null` rather than an empty string —
/// there was no answer, and the value says so.
fn text_value(text: &str) -> serde_json::Value {
    if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(text.to_owned())
    }
}

/// Who authored the post `question` — the author an answer is owed to.
fn asker_of(tree: &Tree, question: EventId) -> Option<Author> {
    match &tree.events.get(&question)?.payload {
        EventPayload::Message(Message::Post { from, .. }) => Some(*from),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FUEL: u64 = 100_000;

    fn setup() -> (Tree, Runner) {
        let mut tree = Tree::new(None);
        let state = Runner::new_root(&mut tree, "you are a test agent", "").unwrap();
        (tree, state)
    }

    /// A post whose body is inline — the user's and the harness's shape,
    /// the two authors with no send side.
    fn direct(text: &str, expects_reply: bool) -> Origin {
        Origin::Direct {
            text: text.into(),
            input: serde_json::Value::Null,
            expects_reply,
        }
    }

    /// The user speaks *inside* a branch: one `Post`, delivered.
    fn user_post(state: &mut Runner, tree: &mut Tree, text: &str) -> Vec<StepOutput> {
        state
            .deliver(tree, Author::User, direct(text, true))
            .unwrap()
            .1
    }

    /// An agent's question, the full exchange shape: a `Send` on the
    /// asker's branch, a `Post` naming it on the answerer's. Returns the
    /// `Send` and what the answerer does next.
    fn ask(
        tree: &mut Tree,
        asker: &mut Runner,
        callee: &mut Runner,
        text: &str,
        input: serde_json::Value,
    ) -> (EventId, Vec<StepOutput>) {
        let to = Address::Branch(callee.agent_id());
        let asker_id = asker.agent_id();
        let send = tree
            .append(
                &mut asker.spine,
                EventPayload::Call(Call::Send {
                    to,
                    text: text.into(),
                    input,
                    expects_reply: true,
                    site: 0,
                }),
            )
            .unwrap();
        let (_, out) = callee
            .deliver(tree, Author::Agent(asker_id), Origin::Sent(send))
            .unwrap();
        (send, out)
    }

    /// A spawned agent and its first question — the pair `tools.agent`
    /// desugars to: `Spawn` → `Agent`, then `Send` → `Post`.
    fn spawn_and_ask(
        tree: &mut Tree,
        asker: &mut Runner,
        charter: &str,
        input: serde_json::Value,
    ) -> (Runner, Vec<StepOutput>) {
        let spawn = tree
            .append(
                &mut asker.spine,
                EventPayload::Call(Call::Spawn {
                    name: None,
                    charter: charter.into(),
                    tools: None,
                    site: 0,
                }),
            )
            .unwrap();
        let mut child = Runner::new_agent(tree, spawn, None, charter, None, None, "").unwrap();
        let (_, out) = ask(tree, asker, &mut child, charter, input);
        (child, out)
    }

    fn llm_program(call_id: &str, source: &str) -> LlmTurn {
        LlmTurn {
            text: String::new(),
            thinking: None,
            tool_calls: vec![ToolCall {
                id: call_id.into(),
                name: TOOL_RUN_PROGRAM.into(),
                arguments: json!({ "source": source }),
            }],
        }
    }

    fn llm_resume(call_id: &str, value: serde_json::Value) -> LlmTurn {
        LlmTurn {
            text: String::new(),
            thinking: None,
            tool_calls: vec![ToolCall {
                id: call_id.into(),
                name: TOOL_RESUME.into(),
                arguments: json!({ "value": value }),
            }],
        }
    }

    fn llm_text(text: &str) -> LlmTurn {
        LlmTurn {
            text: text.into(),
            thinking: None,
            tool_calls: Vec::new(),
        }
    }

    /// Drive `Tick`s until the machine stops asking for them; collects
    /// every non-`Working` output.
    fn drain(state: &mut Runner, tree: &mut Tree, outputs: Vec<StepOutput>) -> Vec<StepOutput> {
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

    /// The most recent report the LLM read. Reports are **derived, not
    /// stored**, so a test derives it the way a request does: from the
    /// last outcome on the branch.
    fn last_report(state: &Runner, tree: &Tree) -> String {
        let leaf = state.spine.leaf_id;
        let outcome = tree
            .path_events(leaf)
            .iter()
            .rev()
            .find(|e| {
                matches!(
                    e.payload,
                    EventPayload::Return { .. } | EventPayload::Condition { .. }
                )
            })
            .map(|e| e.id)
            .expect("an outcome to render");
        crate::report::derive_report(tree, leaf, outcome, state.answer_budget)
    }

    fn payload_kinds(state: &Runner, tree: &Tree) -> Vec<&'static str> {
        state
            .agent_segment(tree)
            .iter()
            .map(|e| match &e.payload {
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

    // ── rendered messages (A3) ──────────────────────────────────────

    /// Machine-bound data travels by reference: the *context* sees a
    /// bounded shape preview, while the whole value reaches the
    /// *program* as the `input` const. A caller passing a large `input`
    /// must never dump it into the callee's context.
    #[test]
    fn large_input_previews_in_context_and_binds_whole() {
        let mut tree = Tree::new(None);
        let root = Runner::new_root(&mut tree, "root", "").unwrap();
        let big = "z".repeat(9_000);
        let mut root = root;
        let (mut child, out) = spawn_and_ask(
            &mut tree,
            &mut root,
            "summarize it",
            json!({ "body": big.clone(), "path": "PLAN.md" }),
        );

        // What the LLM sees: shape, keys, size — not the bytes.
        let req = expect_request(&out);
        let rendered = match &req.messages[0] {
            Rendered::User(text) => text.clone(),
            other => panic!("expected a user message, got {other:?}"),
        };
        assert!(!rendered.contains(&big), "the body must not enter context");
        assert!(rendered.contains("summarize it"), "{rendered}");
        assert!(
            rendered.contains("object, 2 keys: body, path"),
            "{rendered}"
        );
        assert!(rendered.len() < 500, "the preview is bounded: {rendered}");

        // What the program sees: the whole value.
        let out = child
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "return input.body.length;")),
            )
            .unwrap();
        drain(&mut child, &mut tree, out);
        assert!(
            last_report(&child, &tree).contains("returned: 9000"),
            "{}",
            last_report(&child, &tree)
        );
    }

    /// A post's author is rendered, not guessed: a harness notice reads
    /// as one, and the user's own words carry no label.
    #[test]
    fn post_rendering_labels_its_author() {
        let plain = crate::report::render_post(
            Author::User,
            &Origin::Direct {
                text: "hello".into(),
                input: json!(null),
                expects_reply: true,
            },
        );
        assert_eq!(plain, "hello");
        let harness = crate::report::render_post(
            Author::Harness,
            &Origin::Direct {
                text: "your answer is too long".into(),
                input: json!(null),
                expects_reply: false,
            },
        );
        assert_eq!(harness, "[harness] your answer is too long");
        let agent = crate::report::render_post(
            Author::Agent(EventId::new(7)),
            &Origin::Direct {
                text: "which file?".into(),
                input: json!(null),
                expects_reply: true,
            },
        );
        assert_eq!(agent, "[agent 7] which file?");
    }

    // ── typed calls (A2) ────────────────────────────────────────────

    /// A menu row's label comes from the `Call` **variant**, never from
    /// re-parsing a tool name: `ask` vs `tell` is `expects_reply`, and
    /// `spawn` shows the child's name.
    #[test]
    fn menu_labels_come_from_the_call_variant() {
        let send = |expects_reply| Call::Send {
            to: Address::Branch(EventId::new(3)),
            text: "which file?".into(),
            input: json!(null),
            expects_reply,
            site: 0,
        };
        assert_eq!(call_label(&send(true)), r#"ask(#3, "which file?")"#);
        assert_eq!(call_label(&send(false)), r#"tell(#3, "which file?")"#);
        assert_eq!(
            call_label(&Call::Send {
                to: Address::User,
                text: "ok?".into(),
                input: json!(null),
                expects_reply: true,
                site: 0,
            }),
            r#"ask(user, "ok?")"#
        );
        assert_eq!(
            call_label(&Call::Spawn {
                name: Some("researcher".into()),
                charter: "read things".into(),
                tools: None,
                site: 0,
            }),
            "spawn(researcher)"
        );
        assert_eq!(
            call_label(&Call::Invoke {
                name: "fetch".into(),
                args: json!(["x"]),
                site: 0,
            }),
            r#"fetch(["x"])"#
        );
    }

    /// `tools.tool_result` accepts either id the log offers: the `Result`
    /// itself, or the **call** it settles — which is what the menu names.
    #[test]
    fn tool_result_accepts_a_call_id_or_its_result_id() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"await tools.fetch("a"); raise("stop", null);"#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let id = expect_tool_calls(&settled)[0].call;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: id,
                    result: Ok(json!("DATA")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let segment = state.agent_segment(&tree);
        let call = segment
            .iter()
            .find(|e| matches!(e.payload, EventPayload::Call(_)))
            .unwrap()
            .id;
        let result = segment
            .iter()
            .find(|e| matches!(e.payload, EventPayload::Result { .. }))
            .unwrap()
            .id;
        assert_ne!(call, result, "the two halves are separate events");

        // Two successive rewrites on the same branch, each a fresh VM:
        // one reuses by the call id the menu showed, one by the `Result`
        // id. Neither re-issues the call.
        for (n, fetch_by) in [call, result].into_iter().enumerate() {
            let rewrite = format!("return await tools.tool_result({});", fetch_by.as_u64());
            let out = state
                .step(
                    &mut tree,
                    StepInput::LlmResponse(llm_program(&format!("c{}", n + 2), &rewrite)),
                )
                .unwrap();
            let settled = drain(&mut state, &mut tree, out);
            assert!(
                !settled
                    .iter()
                    .any(|o| matches!(o, StepOutput::ToolCalls(_))),
                "served from the log, no call re-issued"
            );
            assert!(
                last_report(&state, &tree).contains(r#"returned: "DATA""#),
                "fetching #{} failed: {}",
                fetch_by.as_u64(),
                last_report(&state, &tree)
            );
        }
    }

    /// A call that definitively failed is a `Failed` outcome on its
    /// `Result` — distinguishable in the log from one that was merely
    /// issued (no `Result` at all).
    #[test]
    fn a_failed_call_settles_with_its_reason() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"try { return await tools.fetch("a"); } catch (e) { return "caught: " + e; }"#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let id = expect_tool_calls(&settled)[0].call;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: id,
                    result: Err("host is down".into()),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let outcome = state
            .agent_segment(&tree)
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::Result { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .expect("a Result");
        assert!(matches!(&outcome, Outcome::Failed(m) if m == "host is down"));
        // And the menu says so rather than previewing a value.
        let report = last_report(&state, &tree);
        assert!(report.contains("failed: host is down"), "{report}");
    }

    // ── the scripted round-trip ─────────────────────────────────────

    #[test]
    fn user_turn_renders_request() {
        let (mut tree, mut state) = setup();
        let out = user_post(&mut state, &mut tree, "compute 6*7");
        let req = expect_request(&out);
        assert!(req.system.contains("test agent"));
        assert!(matches!(&req.messages[0], Rendered::User(t) if t == "compute 6*7"));
        assert_eq!(tool_names(req), [TOOL_RUN_PROGRAM]);
        // The full definition rides along: schema'd parameters, not a name.
        assert!(req.tools[0].parameters["properties"]["source"].is_object());
        assert!(!req.tools[0].description.is_empty());
    }

    #[test]
    fn dialect_card_roots_the_system_prompt() {
        let mut tree = Tree::new(None);
        let mut state =
            Runner::new_root(&mut tree, "you are a test agent", "THE DIALECT CARD").unwrap();
        let out = user_post(&mut state, &mut tree, "go");
        let req = expect_request(&out);
        assert!(
            req.system.starts_with("THE DIALECT CARD\n\n"),
            "{}",
            req.system
        );
        assert!(
            req.system.contains("you are a test agent"),
            "the charter follows the card: {}",
            req.system
        );
        // The system prompt is the snapshot on the branch root — not a
        // message on the spine.
        assert_eq!(payload_kinds(&state, &tree), ["Agent", "Post"]);
    }

    /// The prefix is immutable: a request's system message **equals**
    /// `Agent.system`, the snapshot taken when the agent was created. A
    /// later card edit cannot alter an existing conversation.
    #[test]
    fn the_request_system_equals_the_agent_snapshot() {
        let mut tree = Tree::new(None);
        let state = Runner::new_root(&mut tree, "agent", "CARD A").unwrap();
        let EventPayload::Agent { system: stored, .. } = &tree.events[&EventId::new(1)].payload
        else {
            panic!("#1 must be the root Agent");
        };
        let stored = stored.clone();
        assert!(stored.starts_with("CARD A"));
        let root = state.spine.leaf_id;

        // Re-anchor a fresh runner on the logged spine, card the registry
        // differently, take a new turn — the request's system prompt is
        // the stored CARD A snapshot, not a CARD B re-derivation.
        let mut reopened = Runner::with_spine(&tree, tree.spine_at(root));
        reopened.set_dialect_card("CARD B — evolved".into());
        let out = user_post(&mut reopened, &mut tree, "more");
        let req = expect_request(&out);
        assert_eq!(req.system, stored, "the snapshot replays verbatim");
        assert!(
            !req.system.contains("CARD B"),
            "the evolved card must not leak in"
        );
    }

    /// The whole `input` reaches the program as the `input` const — it
    /// travels on the post that carried it, and a child's first `Post` is
    /// where a caller's data lands.
    #[test]
    fn input_binding_reaches_the_program() {
        let mut tree = Tree::new(None);
        let mut root = Runner::new_root(&mut tree, "root", "").unwrap();
        let (mut state, _) = spawn_and_ask(&mut tree, &mut root, "agent", json!({ "n": 7 }));
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "return input.n;")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert!(last_report(&state, &tree).contains("returned: 7"));
    }

    #[test]
    fn attachments_reach_the_program_as_a_const() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        // A run_program carrying authored content in `attachments`; the
        // program reads it as the `attachments` const, never embedding it
        // in `source`.
        let msg = LlmTurn {
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
            last_report(&state, &tree).contains("returned: 11"),
            "{}",
            last_report(&state, &tree)
        );
    }

    #[test]
    fn malformed_attachments_is_a_repair_loop() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let msg = LlmTurn {
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
        let report = last_report(&state, &tree);
        assert!(
            report.contains("attachments.body") && report.contains("must be a string"),
            "{report}"
        );
        assert!(expect_request(&out).messages.last().is_some());
    }

    #[test]
    fn program_completion_then_root_yields() {
        let (mut tree, mut state) = setup();
        user_post(&mut state, &mut tree, "go");
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
        let report = last_report(&state, &tree);
        assert!(report.contains("returned: 42"), "{report}");
        assert!(report.contains("hi there"), "{report}");
        let req = expect_request(&settled);
        assert!(matches!(req.messages.last(), Some(Rendered::Tool { .. })));

        // A final text turn answers the user's post and the branch goes
        // idle, ready for the next turn. Nothing closes.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_text("the answer is 42")),
            )
            .unwrap();
        // The branch answered the user's post and went idle. Nothing
        // closes: it is still addressable for the next turn.
        assert!(
            matches!(&out[..], [StepOutput::Answered { question: Some(_), value }]
                     if value == &json!("the answer is 42")),
            "{out:?}"
        );
        assert!(state.is_idle());
        assert!(state.open().is_empty(), "the user's post is answered");
        assert_eq!(
            payload_kinds(&state, &tree),
            [
                "Agent", "Post", "Turn", "Return", "Console", "Turn", "Answer"
            ]
        );

        // A follow-up turn appends onto the same spine and runs again.
        user_post(&mut state, &mut tree, "more");
        assert!(matches!(payload_kinds(&state, &tree).last(), Some(&"Post")));
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
        let (xa, yb) = (calls[0].call, calls[1].call);
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: yb,
                    result: Ok(json!("Y")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out); // still blocked on `a`
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: xa,
                    result: Ok(json!("X")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        assert!(last_report(&state, &tree).contains(r#"returned: ["X","Y"]"#));
        // Calls are logged at dispatch, in the program's issue order…
        let issued: Vec<serde_json::Value> = state
            .agent_segment(&tree)
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Call(Call::Invoke { args, .. }) => Some(args.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(issued, vec![json!(["x"]), json!(["y"])], "issue order");
        // …and their `Result`s in resolution order, which is arrival order.
        let settled: Vec<serde_json::Value> = state
            .agent_segment(&tree)
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Result { outcome, .. } => outcome.value().cloned(),
                _ => None,
            })
            .collect();
        assert_eq!(
            settled,
            vec![json!("Y"), json!("X")],
            "results logged in resolution order"
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
        assert!(last_report(&state, &tree).contains("compile error"));
        // No VM was built, so this run has no console and no artifacts —
        // the diagnostic alone, as its one outcome.
        assert_eq!(payload_kinds(&state, &tree), ["Agent", "Turn", "Condition"]);
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
        let id = expect_tool_calls(&settled)[0].call;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: id,
                    result: Ok(json!(41)),
                }]),
            )
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);

        // Suspended: report + restart tools (full definitions).
        let req = expect_request(&settled);
        assert_eq!(tool_names(req), [TOOL_RESUME, TOOL_RUN_PROGRAM]);
        assert!(req.tools[0].parameters["properties"]["value"].is_object());
        let report = last_report(&state, &tree);
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
        assert!(last_report(&state, &tree).contains("returned: 42"));
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
        assert!(last_report(&state, &tree).contains("returned: 42"));
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
        let id = expect_tool_calls(&settled)[0].call;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: id,
                    result: Ok(json!("DATA")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out); // suspended on the raise

        // Find the logged call's id from the report's menu — the menu
        // names calls, and `tool_result` resolves one to its `Result`.
        let artifact_id = state
            .agent_segment(&tree)
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::Call(_) => Some(e.id.as_u64()),
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
        assert!(last_report(&state, &tree).contains(r#"returned: "DATA""#));
    }

    /// `tools.agent` is sugar for spawn **then** ask (B1): two logged
    /// calls, one program promise. The `Spawn`'s `{ agent }` is not the
    /// answer, so the `Send` issued when it lands is what settles the
    /// program — and the caller's own report is rendered around its
    /// `return`.
    #[test]
    fn agent_call_desugars_to_spawn_then_ask() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = r#"return await tools.agent({ prompt: "summarize", input: { n: 1 } });"#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let spawn = *settled
            .iter()
            .find_map(|o| match o {
                StepOutput::Spawns(s) => Some(&s[0].call),
                _ => None,
            })
            .expect("a Spawns output");
        let EventPayload::Call(Call::Spawn { charter, .. }) = &tree.events[&spawn].payload else {
            panic!("#{} must be a Spawn", spawn.as_u64());
        };
        assert_eq!(charter, "summarize", "the prompt is the child's charter");

        // Host side: root the agent under the `Spawn` and settle it.
        let mut child =
            Runner::new_agent(&mut tree, spawn, None, "summarize", None, None, "").unwrap();
        assert_eq!(tree.list_leaves().len(), 2, "caller + the new agent");
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: spawn,
                    result: Ok(json!({ "agent": child.agent_id().as_u64() })),
                }]),
            )
            .unwrap();

        // …which makes the machine issue the first question, not resolve
        // the program: the `{ agent }` handle never reaches the source.
        let send = match &out[..] {
            [StepOutput::Sends(sends)] => sends[0],
            other => panic!("expected Sends, got {other:?}"),
        };
        let EventPayload::Call(Call::Send {
            to, text, input, ..
        }) = &tree.events[&send].payload
        else {
            panic!("#{} must be a Send", send.as_u64());
        };
        assert_eq!(*to, Address::Branch(child.agent_id()));
        assert_eq!(text, "summarize");
        assert_eq!(*input, json!({ "n": 1 }));

        // Deliver it, let the child answer, settle the `Send`.
        let (_, out) = child
            .deliver(
                &mut tree,
                Author::Agent(state.agent_id()),
                Origin::Sent(send),
            )
            .unwrap();
        let Rendered::User(rendered) = &expect_request(&out).messages[0] else {
            panic!("the question renders as a user-role post");
        };
        assert!(rendered.starts_with("[agent 1] summarize"), "{rendered}");
        let out = child
            .step(&mut tree, StepInput::LlmResponse(llm_text("child says hi")))
            .unwrap();
        let result = match &out[..] {
            [StepOutput::Answered { value, .. }] => value.clone(),
            other => panic!("expected Answered, got {other:?}"),
        };
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: send,
                    result: Ok(result),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert!(last_report(&state, &tree).contains(r#"returned: "child says hi""#));
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

    // ── outcomes and derived reports (A4) ───────────────────────────

    /// Every outcome event on a branch, in log order.
    fn outcomes(state: &Runner, tree: &Tree) -> Vec<EventId> {
        state
            .agent_segment(tree)
            .iter()
            .filter(|e| {
                matches!(
                    e.payload,
                    EventPayload::Return { .. } | EventPayload::Condition { .. }
                )
            })
            .map(|e| e.id)
            .collect()
    }

    /// Exactly one outcome per **handback**, not per run: a single
    /// `run_program` that raises, is resumed, traps, is resumed again and
    /// finally returns logs four, with `Return` only on the last.
    #[test]
    fn one_outcome_per_handback_not_per_run() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        // resume #1 lands 1 → `x` is 1; resume #2 stands in for the
        // failed property read with 41, so the program returns 42.
        let src = r#"
            const x = raise("need", null);
            const bad = null;
            return bad.missing + x + 40;
        "#;
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        drain(&mut state, &mut tree, out); // handback 1: raised
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_resume("c2", json!(1))),
            )
            .unwrap();
        drain(&mut state, &mut tree, out); // handback 2: trapped
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_resume("c3", json!(1))),
            )
            .unwrap();
        drain(&mut state, &mut tree, out); // handback 3: returned

        let kinds: Vec<&'static str> = outcomes(&state, &tree)
            .iter()
            .map(|id| match &tree.events[id].payload {
                EventPayload::Return { .. } => "Return",
                EventPayload::Condition {
                    cause: Cause::Raised { .. },
                    ..
                } => "raised",
                EventPayload::Condition {
                    cause: Cause::Trapped { .. },
                    ..
                } => "trapped",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["raised", "trapped", "Return"], "{kinds:?}");
        assert!(last_report(&state, &tree).contains("returned: 42"));
    }

    /// Every tool call gets exactly one outcome event — including one
    /// that never ran. Without that, a refusal's tool message would have
    /// to be rebuilt by replaying eligibility to that path position.
    #[test]
    fn every_tool_call_has_an_outcome_event() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();

        // A `resume` with nothing suspended: refused, and the refusal is
        // the call's outcome.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_resume("c1", json!(1))),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        // A run that compiles and returns.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c2", "return 1;")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        // A run that does not compile.
        let out = state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c3", "let = ;")),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        // One outcome per turn, and the request answers every call id.
        let leaf = state.spine.leaf_id;
        for event in state.agent_segment(&tree) {
            let EventPayload::Message(Message::Turn { tool_calls, .. }) = &event.payload else {
                continue;
            };
            assert_eq!(
                crate::report::outcomes_of_turn(&tree, leaf, event.id).len(),
                tool_calls.len(),
                "each call gets exactly one outcome"
            );
        }
        let StepOutput::LlmRequest(req) = state.render_request(&tree) else {
            panic!("expected a request");
        };
        let answered: Vec<&str> = req
            .messages
            .iter()
            .filter_map(|m| match m {
                Rendered::Tool { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(answered, ["c1", "c2", "c3"]);
    }

    /// A report renders identically forever **for a given renderer**: the
    /// same log rendered twice is byte-identical, because every input the
    /// report needs is in the log and nothing decorates it with a fact
    /// that was true at the time and logged nowhere.
    #[test]
    fn derived_reports_are_stable_for_a_renderer() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        let src = "const a = await tools.fetch(\"x\");\nconsole.log(a);\nreturn a;";
        let out = state
            .step(&mut tree, StepInput::LlmResponse(llm_program("c1", src)))
            .unwrap();
        let settled = drain(&mut state, &mut tree, out);
        let id = expect_tool_calls(&settled)[0].call;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: id,
                    result: Ok(json!("X")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);

        let first = state.render_messages(&tree);
        tree.clear_report_memo();
        let second = state.render_messages(&tree);
        assert_eq!(first, second, "the same log renders the same bytes");
        assert!(
            first
                .iter()
                .any(|m| matches!(m, Rendered::Tool { text, .. } if text.contains("returned:"))),
            "{first:?}"
        );
    }

    /// Reports are derived, so without a memo every request re-derives
    /// every report on the path and a session is quadratic in branch
    /// length. With one, re-derivation is amortised O(1).
    #[test]
    fn report_memo_avoids_rederiving_history() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        for (n, id) in ["c1", "c2", "c3"].into_iter().enumerate() {
            let out = state
                .step(
                    &mut tree,
                    StepInput::LlmResponse(llm_program(id, &format!("return {n};"))),
                )
                .unwrap();
            drain(&mut state, &mut tree, out);
        }
        // Three runs, so three reports were derived — once each, even
        // though each request re-rendered the whole path.
        assert_eq!(tree.report_derivations(), 3);

        let before = tree.report_derivations();
        state.render_messages(&tree);
        state.render_messages(&tree);
        assert_eq!(
            tree.report_derivations(),
            before,
            "history is served from the memo, not re-derived"
        );

        // The memo is a cache of one renderer's output: dropping it makes
        // the next render pay again, and produce the same bytes.
        let rendered = state.render_messages(&tree);
        tree.clear_report_memo();
        assert_eq!(state.render_messages(&tree), rendered);
        assert!(tree.report_derivations() > before);
    }

    /// The console is a diagnostic stream, not data: it is capped with an
    /// explicit marker rather than silently truncated, and the program's
    /// own `return` is the channel for anything that must survive whole.
    #[test]
    fn oversized_console_is_capped_and_marked() {
        let lines: Vec<String> = (0..crate::report::CONSOLE_MAX_LINES + 500)
            .map(|i| format!("line {i}"))
            .collect();
        let capped = crate::report::cap_console(&lines, "console event follows #9");
        assert!(capped.len() <= crate::report::CONSOLE_MAX_LINES + 1);
        assert!(capped[0].contains("console truncated"), "{}", capped[0]);
        assert!(
            capped[0].contains("500 earlier lines dropped"),
            "{}",
            capped[0]
        );
        assert!(capped[0].contains("#9"), "the marker names the event");
        // The tail is what is kept — the latest output before the stop.
        assert_eq!(capped.last().unwrap(), "line 2499");

        // A byte-heavy console is capped too, by the same marker.
        let fat: Vec<String> = (0..40).map(|_| "z".repeat(10_000)).collect();
        let capped = crate::report::cap_console(&fat, "x");
        let bytes: usize = capped.iter().skip(1).map(|l| l.len()).sum();
        assert!(bytes <= crate::report::CONSOLE_MAX_BYTES);
        assert!(capped[0].contains("console truncated"));
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
        let id = expect_tool_calls(&settled)[0].call;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: id,
                    result: Ok(json!(41)),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(
            last_report(&state, &tree),
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
[#3] fetch(["a"]) → 41

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
            last_report(&state, &tree),
            r#"## what happened
2:10: cannot read property 'x' on null
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
        let id = expect_tool_calls(&settled)[0].call;
        let out = state
            .step(
                &mut tree,
                StepInput::ToolResults(vec![ToolResult {
                    call: id,
                    result: Ok(json!("X")),
                }]),
            )
            .unwrap();
        drain(&mut state, &mut tree, out);
        assert_eq!(
            last_report(&state, &tree),
            r#"## program completed
returned: ["X",2]

console (last 1 of 1 lines):
got X

## new artifacts — fetch with tools.tool_result(id)
[#3] fetch(["x"]) → "X"
[#5] program result → ["X",2]"#
        );
    }

    /// A post to a **running** branch is logged on arrival and is never
    /// rejected: nothing you say is lost. B1 delivers it through the one
    /// door every author uses, so the M2-era panic is gone; suspending
    /// the program into `Condition::Posted` at its next slice is B3.
    #[test]
    fn mid_program_post_is_logged_not_rejected() {
        let (mut tree, mut state) = setup();
        state.kickoff(&mut tree).unwrap();
        state
            .step(
                &mut tree,
                StepInput::LlmResponse(llm_program("c1", "await tools.fetch(1); return 0;")),
            )
            .unwrap();
        let out = user_post(&mut state, &mut tree, "are you done?");
        assert!(out.is_empty(), "no request while the VM holds the branch");
        assert_eq!(
            payload_kinds(&state, &tree).last(),
            Some(&"Post"),
            "logged on arrival, at the position it landed"
        );
        assert_eq!(
            state.open().len(),
            1,
            "and it is open — someone owes a reply"
        );
    }

    fn program_result_value(state: &Runner, tree: &Tree) -> serde_json::Value {
        state
            .agent_segment(tree)
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::Return { value } => Some(value.clone()),
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
        let report = last_report(&state, &tree);
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
        let report = last_report(&state, &tree);
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
        let (mut tree, mut root) = setup();
        let (mut child, _) = spawn_and_ask(&mut tree, &mut root, "summarize", json!({}));
        child.answer_budget = 50;
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
            [StepOutput::Answered { value, .. }] => value.clone(),
            other => panic!("expected Answered, got {other:?}"),
        };
        let delivered = result.as_str().unwrap();
        assert!(delivered.contains("answer truncated"), "note: {delivered}");
        assert!(delivered.len() < long.len(), "delivered value is bounded");
        // The full prose stays on the child's spine (last Assistant message).
        assert!(
            child
                .spine
                .context()
                .messages
                .iter()
                .any(|m| matches!(m, Message::Turn { text, .. } if text.len() == 500)),
            "full prose retained on spine"
        );
    }
}
