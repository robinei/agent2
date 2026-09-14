//! A standalone execution loop, the piece Part H's regression harness
//! needs underneath it and the thing this session's `codemode-probe`
//! subcommand couldn't do (it shows one completion; it never runs
//! one). [`run`] wires together everything built so far —
//! `document::render`, `transport`/`fence`, `verbs::parse_effect`,
//! `decision::read`, `stack::ProgramStack` — into "dispatch a task,
//! run the program it produces, answer its tool/verb calls, generate
//! a real handler completion on every `raise()` or resumable trap,
//! apply the decision, repeat until the root program finishes."
//!
//! Still not `machine.rs`/`tree.rs`: no event log, no branches, no
//! real agents. `Ask`/`Spawn`/`Fork`/`Artifact`/`Compact`/
//! `ListAgents` are answered with the plainest honest response this
//! harness can give without that backing (documented per verb, below)
//! rather than guessed at — a task that needs one of those for real
//! is not a task this harness can run yet.
//!
//! Parametrized over [`CompletionSource`] specifically so the loop
//! itself — dispatch, raise/trap handling, decision application,
//! nesting, the guard rails — is unit-tested on scripted completions,
//! no network, same as everything else in this phase; only a real
//! task run (Part H proper, or a manual probe) supplies a live one.

use std::collections::VecDeque;

use interp::{PromisePtr, StepResult, VM, VMError, Value};

use super::decision::{self, Decision};
use super::document::{self, Document};
use super::entry::{Entry, EntryId};
use super::stack::{ApplyError, ProgramStack, PushError, Suspension};
use super::transport::{self, Completion};
use super::verbs::{self, HarnessEffect};

/// Where a program's completion comes from. Live network calls and
/// scripted, no-network fixtures are the same trait so [`run`]'s own
/// logic never has to know which it's talking to.
pub trait CompletionSource {
    fn complete(&mut self, doc: &Document) -> Result<Completion, String>;
}

/// The real thing: one `Endpoint`/model pair, calling
/// `transport::complete` every time. Used by an actual task run, never
/// by this module's own tests.
pub struct LiveSource<'a> {
    pub endpoint: transport::Endpoint<'a>,
    pub model: &'a str,
    pub max_tokens: u32,
}

impl CompletionSource for LiveSource<'_> {
    fn complete(&mut self, doc: &Document) -> Result<Completion, String> {
        transport::complete(
            doc,
            &transport::DeepSeekCodeModeRequest {
                model: self.model,
                max_tokens: self.max_tokens,
            },
            &self.endpoint,
        )
    }
}

/// A fixed queue of already-written program sources, popped in order
/// — the `ScriptedLlm` of this module. Each is wrapped as a `Completion`
/// with no thinking and a clean `"stop"` finish, since what the loop's
/// own tests are exercising is dispatch and nesting, not transport
/// behavior (that is `transport.rs`'s own test module).
pub struct ScriptedSource {
    programs: VecDeque<String>,
}

impl ScriptedSource {
    pub fn new(programs: impl IntoIterator<Item = &'static str>) -> Self {
        ScriptedSource {
            programs: programs.into_iter().map(str::to_owned).collect(),
        }
    }
}

impl CompletionSource for ScriptedSource {
    fn complete(&mut self, _doc: &Document) -> Result<Completion, String> {
        let text = self
            .programs
            .pop_front()
            .ok_or_else(|| "scripted source ran out of programs".to_owned())?;
        Ok(Completion {
            text,
            thinking: None,
            finish_reason: Some("stop".into()),
        })
    }
}

/// A task's fake capabilities — what `tools.*` calls resolve to, and
/// (separately) what `ask()` resolves to, since neither has a real
/// implementation to reach in this standalone harness.
pub trait FakeTools {
    /// `tools.<name>(args)`. `Err` rejects the call's promise — a
    /// program is free to `try`/`catch` it (6_LANGUAGE Part B), same
    /// as any real tool failure.
    fn call(&self, name: &str, args: &[serde_json::Value]) -> Result<serde_json::Value, String>;

    /// `ask(who, text)`. Default: no task needs this yet, so the
    /// honest answer is a rejection, not a guessed value.
    fn ask(&self, _who: Option<&str>, _text: &str) -> Result<serde_json::Value, String> {
        Err("this harness has no ask() handler configured".into())
    }
}

/// A `say(to, text)` call, in order — the transcript a task's success
/// condition reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Said {
    pub to: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    pub transcript: Vec<Said>,
    /// Every `append_history(value)` call, in order — not backed by a
    /// real log in this harness, but worth surfacing for a task's
    /// success condition to inspect.
    pub appended: Vec<serde_json::Value>,
    pub raise_count: usize,
    /// Of `raise_count`, how many were a **trapped runtime error**
    /// rather than a deliberate `raise()` call — `raise_count` counts
    /// both (Step D1: a resumable trap gets a handler the same way a
    /// raise does), which is right for "how many suspensions did this
    /// run need" but wrong for anything asking "did the model
    /// deliberately suspend for judgment" — a task whose check gates
    /// on `raise_count > 0` as evidence of a deliberate decision can
    /// be satisfied by an unrelated coding-mistake trap earlier in the
    /// run, resolved by `abandon()`, with the actual consequential
    /// action later run by a *disconnected* fresh attempt that never
    /// raised at all. Live evidence this is not hypothetical
    /// (2026-09-14): exactly this shape produced a false pass on
    /// `destructive-migration-gate` — round 1 trapped on an engine gap
    /// (spreading a `Set`, unrelated to the task's judgment), got
    /// abandoned, and round 3's *fresh, unconnected* rewrite ran the
    /// destructive migration with no deliberate raise anywhere near
    /// it, yet `raise_count == 1` (from round 1's trap) let the check
    /// pass. A check gating on "was there a deliberate decision point"
    /// should use `raise_count - trap_count`, not `raise_count`.
    pub trap_count: usize,
    pub completions_used: usize,
    /// How many of `raise_count`'s raises resolved via `resume()` (as
    /// opposed to `abandon()`) — the measurement this field and
    /// `handover_count` exist for: is `raise`'s "program keeps running
    /// with an injected value" case actually exercised, or is
    /// `abandon` (a fresh replacement) what handlers reach for.
    pub resume_count: usize,
    /// Of `resume_count`, how many resumed frames made **zero** further
    /// calls before reaching their own `Done` — the runtime proxy for
    /// "the raise's result was never used for anything but falling
    /// through to completion," i.e. what a tail raise with preemptive
    /// popping (see the design conversation this instruments) would
    /// have handled as a handover instead of a genuine mid-program
    /// resume. Not a compile-time tail check — this harness has none —
    /// so a program that happens to do no more work *coincidentally*
    /// still counts; it is a proxy, not a proof.
    pub handover_count: usize,
    /// How many raises resolved via `abandon()`.
    pub abandon_count: usize,
    /// `fork()`/`spawn()`/`artifact()` call attempts, regardless of
    /// outcome — all three are honest-error stubs in this harness
    /// (`dispatch_call`, below), so these count reach, not success.
    /// Real fork/spawn chain depth and artifact fetch rate need the
    /// verbs backed for real first; these are what's honestly
    /// measurable before that.
    pub fork_attempts: usize,
    pub spawn_attempts: usize,
    pub artifact_attempts: usize,
}

#[derive(Debug)]
pub enum RunError {
    Completion(String),
    DidNotParse {
        source: String,
        message: String,
    },
    /// Step A1: distinguished from an ordinary parse failure — the
    /// completion never finished, so there is nothing to blame the
    /// model's JavaScript for.
    Truncated {
        while_thinking: bool,
    },
    Depth(PushError),
    TooManyCompletions,
    /// A root program returned a `resume`/`abandon` value — Step D2:
    /// "returning a decision from a root program is an error: there
    /// is no caller to decide about."
    RootReturnedADecision,
    /// A handler fell off the end without deciding (Step D2: "only a
    /// decision counts").
    HandlerDidNotDecide,
    Apply(ApplyError),
    Vm(VMError),
}

pub struct RunConfig {
    pub max_depth: usize,
    pub max_completions: usize,
    /// Worked user/assistant pairs to open `messages` with, right
    /// after the card (Step C4's seed exemplars) — spliced in by
    /// [`run`] itself so a live task run gets the same demonstrations
    /// `codemode-probe` always has, instead of the two silently
    /// drifting apart. Empty by default: this standalone loop's own
    /// tests exercise dispatch and nesting, not register effects, and
    /// a `ScriptedSource` doesn't care what the document says anyway.
    pub exemplars: &'static [super::card::Exemplar],
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            max_depth: 8,
            max_completions: 16,
            exemplars: &[],
        }
    }
}

/// Turn one completion into a compiled `VM`, applying the no-fence
/// rule (Step A1) uniformly regardless of where the completion came
/// from.
fn program_from_completion(completion: &Completion) -> Result<VM, RunError> {
    if completion.was_truncated() {
        return Err(RunError::Truncated {
            while_thinking: completion.text.trim().is_empty(),
        });
    }
    let source = super::fence::extract(&completion.text);
    let program = interp::compile(&source).map_err(|diags| RunError::DidNotParse {
        source: source.clone(),
        message: diags
            .iter()
            .map(|d| d.render(&source))
            .collect::<Vec<_>>()
            .join("\n"),
    })?;
    VM::for_program(program, serde_json::Value::Null).map_err(RunError::Vm)
}

/// The transient tail a handler's document carries (Step D1: "the
/// ordinary document plus a transient condition report... it vanishes
/// as the stack unwinds and never becomes history"). Named choices
/// this harness makes, not yet pinned by the plan doc: the raising
/// program's own source is included verbatim (a handler cannot decide
/// about a frame it cannot see), and the wording asks explicitly for
/// one of the two decision values by name.
fn handler_tail(raising_source: &str, suspension: &Suspension) -> String {
    let cause = match suspension {
        Suspension::Raised { condition, payload } => match payload {
            Some(p) => format!("raised `{condition}` with payload {p:?}"),
            None => format!("raised `{condition}`"),
        },
        Suspension::Trapped(e) => format!("trapped: {:?}: {}", e.kind, e.message),
        Suspension::Posted => "was interrupted by a message".to_owned(),
    };
    format!(
        "the program below is suspended — it {cause}:\n\n{raising_source}\n\n\
         decide by returning `resume(value)` to continue it, or `abandon()` \
         to discard it and write a replacement next."
    )
}

/// Attempt counts `dispatch_call` bumps for verbs this harness stubs
/// with an honest error — `RunOutcome`'s doc explains why these are
/// "did the model reach for it," not "how deep did the chain go."
#[derive(Default)]
struct Attempts {
    fork: usize,
    spawn: usize,
    artifact: usize,
}

/// Dispatch one settled `InvokeCall`: either a harness verb
/// (`verbs::parse_effect`) or a task's fake tool. Resolves or rejects
/// the call's promise on `vm` directly — the caller just needs to
/// know what happened for the transcript/report.
#[allow(clippy::too_many_arguments)]
fn dispatch_call(
    vm: &mut VM,
    call: &interp::InvokeCall,
    tools: &dyn FakeTools,
    transcript: &mut Vec<Said>,
    appended: &mut Vec<serde_json::Value>,
    attempts: &mut Attempts,
) -> Result<(), RunError> {
    let promise: PromisePtr = call.promise;
    let effect = verbs::parse_effect(vm, call);
    let outcome: Result<serde_json::Value, String> = match effect {
        Ok(HarnessEffect::Say { to, text }) => {
            transcript.push(Said {
                to: to.clone(),
                text,
            });
            Ok(serde_json::Value::Null)
        }
        Ok(HarnessEffect::Ask { who, text }) => tools.ask(who.as_deref(), &text),
        Ok(HarnessEffect::Answer { value, .. }) => Ok(value),
        Ok(HarnessEffect::AppendHistory { value }) => {
            appended.push(value);
            Ok(serde_json::Value::Null)
        }
        Ok(HarnessEffect::ListAgents) => Ok(serde_json::json!([])),
        Ok(HarnessEffect::Spawn { .. }) => {
            attempts.spawn += 1;
            Err("this harness does not back spawn() with a real agent yet".into())
        }
        Ok(HarnessEffect::Fork) => {
            attempts.fork += 1;
            Err("this harness does not back fork() with a real context yet".into())
        }
        Ok(HarnessEffect::Artifact { .. }) => {
            attempts.artifact += 1;
            Err("this harness has no artifact store to fetch from yet".into())
        }
        Ok(HarnessEffect::Compact(_)) => {
            Err("this harness does not back compaction with a real record yet".into())
        }
        Err(_verb_error) => {
            // Not a recognized harness verb — the expected, correct
            // classification for every ordinary `tools.*` call, since
            // the two are disjoint namespaces by design (Step C1).
            // Not decorated with `_verb_error`: that would misleadingly
            // suggest something went wrong in *this* dispatch, when a
            // ordinary tool call failing `parse_effect` is exactly
            // what should happen every time — the tool's own error
            // message (or `FakeTools`'s own "no such tool" default for
            // a genuinely unconfigured name) is the whole story.
            let args: Vec<serde_json::Value> = call
                .args
                .iter()
                .map(|v| {
                    vm.stack_value_to_json(v, 0)
                        .unwrap_or_else(|_| serde_json::Value::String(format!("{v:?}")))
                })
                .collect();
            tools.call(&call.name, &args)
        }
    };
    match outcome {
        Ok(value) => {
            let v = vm.json_to_stack_value(&value, 0).map_err(RunError::Vm)?;
            vm.resolve_promise(promise, v).map_err(RunError::Vm)
        }
        Err(message) => {
            let v = Value::String(message.into());
            vm.reject_promise(promise, v).map_err(RunError::Vm)
        }
    }
}

/// Run one task to completion: dispatch `user_message` as the root
/// program's only history, execute it, generate a real handler
/// completion for every `raise()` or resumable trap, and keep going
/// until the root program itself finishes (or a guard rail trips).
pub fn run(
    card: &str,
    user_message: &str,
    tools: &dyn FakeTools,
    source: &mut dyn CompletionSource,
    config: &RunConfig,
) -> Result<RunOutcome, RunError> {
    let log = vec![(
        EntryId::new(1),
        Entry::Message {
            from: "user".into(),
            text: user_message.to_owned(),
        },
    )];
    let mut root_doc = document::render(card, &log).expect("a single-message log always renders");
    // Seed exemplars open `messages`, right after the card (Step C4)
    // — spliced once here so every completion this run ever takes,
    // including a decider's or a regenerated replacement's (both
    // clone `root_doc` via `docs_by_depth`), carries the same
    // demonstrations `codemode-probe` always has.
    for (i, ex) in config.exemplars.iter().enumerate() {
        root_doc.messages.splice(
            (1 + i * 2)..(1 + i * 2),
            [
                document::ChatMessage {
                    role: document::ChatRole::User,
                    content: ex.user.to_owned(),
                },
                document::ChatMessage {
                    role: document::ChatRole::Assistant,
                    content: ex.assistant.to_owned(),
                },
            ],
        );
    }

    let mut completions_used = 0;
    let mut take_completion =
        |source: &mut dyn CompletionSource, doc: &Document| -> Result<Completion, RunError> {
            if completions_used >= config.max_completions {
                return Err(RunError::TooManyCompletions);
            }
            completions_used += 1;
            source.complete(doc).map_err(RunError::Completion)
        };
    // The repair loop `types.rs`'s `Cause::CompileFailed` doc comment
    // names but this harness never implemented: a parse failure used
    // to be `program_from_completion`'s `Err` propagating straight out
    // of `run`, ending the whole task with zero chance to recover —
    // exactly the asymmetry the card's own "a response that fails to
    // parse comes back as a trap" promises the model but this code
    // never delivered. Live evidence this was not academic
    // (2026-09-14): a genuinely well-engineered ~80-line program
    // failed the entire task over one unbalanced paren
    // (`open.push("..." + x.join(", ")) — text");`, a slip at least as
    // mechanically fixable as any runtime trap — the compiler even
    // names the exact line and column. Bounded by `take_completion`'s
    // own `max_completions` check, same as any other retry here — no
    // separate limit needed.
    let mut take_program =
        |source: &mut dyn CompletionSource, doc: &Document| -> Result<VM, RunError> {
            let mut current_doc = doc.clone();
            loop {
                let completion = take_completion(source, &current_doc)?;
                match program_from_completion(&completion) {
                    Ok(vm) => return Ok(vm),
                    Err(RunError::DidNotParse { message, .. }) => {
                        current_doc = current_doc.with_tail(&format!(
                            "the previous response did not parse as JavaScript:\n{message}\n\n\
                         reply again with corrected source — the whole response is \
                         parsed as JavaScript, nothing else."
                        ));
                    }
                    Err(other) => return Err(other),
                }
            }
        };

    let root_vm = take_program(source, &root_doc)?;
    let mut stack = ProgramStack::new(root_vm);
    // The document each currently-stacked frame was generated from —
    // same length and indexing as the stack, so an `Abandon` can
    // regenerate a replacement from exactly what the frame being
    // replaced originally saw.
    let mut docs_by_depth = vec![root_doc];

    let mut transcript = Vec::new();
    let mut appended = Vec::new();
    let mut raise_count = 0;
    let mut trap_count = 0;
    let mut resume_count = 0;
    let mut handover_count = 0;
    let mut abandon_count = 0;
    let mut attempts = Attempts::default();
    // Set the instant a `Resume` decision is applied to the raising
    // frame, `Some(true)`; cleared to `Some(false)` by the first call
    // that frame makes afterward. Read (and reset to `None`) the next
    // time that same frame reaches `Done` — `Some(true)` there means
    // the resumed value flowed straight to completion with no further
    // work, `RunOutcome::handover_count`'s runtime proxy for a tail
    // raise. Any intervening `Raise`/trap on the same frame (it
    // suspended again instead of finishing) also clears it to `None`
    // without counting — that resume didn't reach a `Done` at all.
    let mut tracking_handover: Option<bool> = None;

    loop {
        let step = stack.current_mut().step(u64::MAX);
        match step {
            Ok(StepResult::Pending { calls }) => {
                for call in &calls {
                    if tracking_handover.is_some() {
                        tracking_handover = Some(false);
                    }
                    dispatch_call(
                        stack.current_mut(),
                        call,
                        tools,
                        &mut transcript,
                        &mut appended,
                        &mut attempts,
                    )?;
                }
            }
            Ok(StepResult::OutOfFuel) => continue,
            Ok(StepResult::Done { value, unstarted }) => {
                // Fire-and-forget calls: `say(x)` and friends need no
                // await (their resolution value is `undefined` and
                // nothing waits for it), so a program written the
                // natural way ends with these in `unstarted`, never
                // `Pending`, per `StepResult::Done`'s own doc ("the
                // host decides whether to run or drop them"). Run them
                // for their side effects — the transcript is the
                // whole point of a `say()` call, awaited or not — the
                // resolved value goes nowhere, since the VM that would
                // have read it is finishing this same step.
                for call in &unstarted {
                    if tracking_handover.is_some() {
                        tracking_handover = Some(false);
                    }
                    dispatch_call(
                        stack.current_mut(),
                        call,
                        tools,
                        &mut transcript,
                        &mut appended,
                        &mut attempts,
                    )?;
                }
                if tracking_handover.take() == Some(true) {
                    handover_count += 1;
                }
                if stack.depth() == 1 {
                    // The root program finished. Its return value is
                    // read by nobody (Step D2) — except to check it
                    // did not confuse itself for a handler.
                    if decision::read(stack.current(), &value).is_some() {
                        return Err(RunError::RootReturnedADecision);
                    }
                    return Ok(RunOutcome {
                        transcript,
                        appended,
                        raise_count,
                        trap_count,
                        completions_used,
                        resume_count,
                        handover_count,
                        abandon_count,
                        fork_attempts: attempts.fork,
                        spawn_attempts: attempts.spawn,
                        artifact_attempts: attempts.artifact,
                    });
                }
                // A handler finished — read and apply its decision.
                let Some(decision) = decision::read(stack.current(), &value) else {
                    return Err(RunError::HandlerDidNotDecide);
                };
                match decision {
                    Decision::Resume(v) => {
                        resume_count += 1;
                        tracking_handover = Some(true);
                        docs_by_depth.pop();
                        stack
                            .apply_decision(Decision::Resume(v), || unreachable!())
                            .map_err(RunError::Apply)?;
                    }
                    Decision::Abandon => {
                        abandon_count += 1;
                        docs_by_depth.pop();
                        // The new top (post-pop) is the frame being
                        // replaced; regenerate from what *it* saw.
                        let replacement_doc = docs_by_depth
                            .last()
                            .expect("a non-root frame always has a caller doc")
                            .clone();
                        let replacement_vm = take_program(source, &replacement_doc)?;
                        stack
                            .apply_decision(Decision::Abandon, || replacement_vm)
                            .map_err(RunError::Apply)?;
                        *docs_by_depth.last_mut().unwrap() = replacement_doc;
                    }
                }
            }
            Ok(StepResult::Raise { condition, payload }) => {
                raise_count += 1;
                tracking_handover = None;
                let raising_source = stack.current().source.to_string();
                let tail = handler_tail(
                    &raising_source,
                    &Suspension::Raised {
                        condition: condition.clone(),
                        payload: payload.clone(),
                    },
                );
                let handler_doc = docs_by_depth
                    .last()
                    .expect("stack is never empty")
                    .clone()
                    .with_tail(&tail);
                let handler_vm = take_program(source, &handler_doc)?;
                stack
                    .push(
                        Suspension::Raised { condition, payload },
                        handler_vm,
                        config.max_depth,
                    )
                    .map_err(RunError::Depth)?;
                docs_by_depth.push(handler_doc);
            }
            Err(vm_error) => {
                raise_count += 1;
                trap_count += 1;
                tracking_handover = None;
                let raising_source = stack.current().source.to_string();
                let suspension = Suspension::Trapped(vm_error);
                let tail = handler_tail(&raising_source, &suspension);
                let handler_doc = docs_by_depth
                    .last()
                    .expect("stack is never empty")
                    .clone()
                    .with_tail(&tail);
                let handler_vm = take_program(source, &handler_doc)?;
                stack
                    .push(suspension, handler_vm, config.max_depth)
                    .map_err(RunError::Depth)?;
                docs_by_depth.push(handler_doc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const CARD: &str = "test card";

    /// A `FakeTools` test double: canned answers by tool name, and an
    /// optional canned answer for `ask()`.
    #[derive(Default)]
    struct TestTools {
        calls: HashMap<String, Result<serde_json::Value, String>>,
        ask_answer: Option<Result<serde_json::Value, String>>,
    }

    impl TestTools {
        fn with(mut self, name: &str, result: Result<serde_json::Value, String>) -> Self {
            self.calls.insert(name.into(), result);
            self
        }
        fn with_ask(mut self, result: Result<serde_json::Value, String>) -> Self {
            self.ask_answer = Some(result);
            self
        }
    }

    impl FakeTools for TestTools {
        fn call(
            &self,
            name: &str,
            _args: &[serde_json::Value],
        ) -> Result<serde_json::Value, String> {
            self.calls
                .get(name)
                .cloned()
                .unwrap_or_else(|| Err(format!("no such tool: {name}")))
        }
        fn ask(&self, _who: Option<&str>, _text: &str) -> Result<serde_json::Value, String> {
            self.ask_answer
                .clone()
                .unwrap_or_else(|| Err("no ask handler configured for this test".into()))
        }
    }

    #[test]
    fn a_simple_program_completes_and_records_say() {
        let mut source = ScriptedSource::new(["say('hi'); say('bye');"]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(
            outcome.transcript,
            vec![
                Said {
                    to: None,
                    text: "hi".into()
                },
                Said {
                    to: None,
                    text: "bye".into()
                },
            ]
        );
        assert_eq!(outcome.completions_used, 1);
        assert_eq!(outcome.raise_count, 0);
    }

    #[test]
    fn say_with_a_target_is_recorded() {
        let mut source = ScriptedSource::new(["say('user', 'done');"]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(
            outcome.transcript,
            vec![Said {
                to: Some("user".into()),
                text: "done".into()
            }]
        );
    }

    #[test]
    fn a_fake_tool_call_resolves_and_the_program_uses_the_result() {
        let mut source =
            ScriptedSource::new(["const r = await tools.read_file('x'); say(r.content);"]);
        let tools = TestTools::default().with(
            "read_file",
            Ok(serde_json::json!({ "content": "file contents" })),
        );
        let outcome = run(CARD, "go", &tools, &mut source, &RunConfig::default()).unwrap();
        assert_eq!(outcome.transcript[0].text, "file contents");
    }

    #[test]
    fn a_rejected_tool_call_is_catchable() {
        // A rejection is a plain string value (matching `machine.rs`'s
        // own `reject_call` convention), not an `Error`-shaped object
        // — caught as the string itself, not via `.message`.
        let mut source = ScriptedSource::new([
            "try { await tools.read_file('x'); } catch (e) { say('caught: ' + e); }",
        ]);
        let tools = TestTools::default().with("read_file", Err("no such file".into()));
        let outcome = run(CARD, "go", &tools, &mut source, &RunConfig::default()).unwrap();
        assert_eq!(outcome.transcript[0].text, "caught: no such file");
    }

    #[test]
    fn ask_is_dispatched_to_the_fake_ask_handler() {
        let mut source =
            ScriptedSource::new(["const a = await ask('user', 'q?'); say(String(a));"]);
        let tools = TestTools::default().with_ask(Ok(serde_json::json!(42)));
        let outcome = run(CARD, "go", &tools, &mut source, &RunConfig::default()).unwrap();
        assert_eq!(outcome.transcript[0].text, "42");
    }

    #[test]
    fn append_history_is_recorded_in_the_outcome() {
        let mut source = ScriptedSource::new(["append_history({ rows: 4 }); say('done');"]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(outcome.appended, vec![serde_json::json!({ "rows": 4 })]);
    }

    #[test]
    fn list_agents_resolves_to_an_empty_array_with_no_tool_configured() {
        let mut source =
            ScriptedSource::new(["const a = await list_agents(); say(String(a.length));"]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(outcome.transcript[0].text, "0");
    }

    #[test]
    fn spawn_is_rejected_and_catchable_not_a_crash() {
        let mut source = ScriptedSource::new(["try { await spawn('reviewer'); say('spawned'); } \
             catch (e) { say('no spawn: ' + e.message); }"]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert!(outcome.transcript[0].text.starts_with("no spawn:"));
        // Reach, not success (module doc on `Attempts`): the stub
        // rejects it, but the harness must still know the model asked.
        assert_eq!(outcome.spawn_attempts, 1);
        assert_eq!(outcome.fork_attempts, 0);
        assert_eq!(outcome.artifact_attempts, 0);
    }

    #[test]
    fn fork_is_rejected_and_catchable_not_a_crash() {
        let mut source = ScriptedSource::new(["try { await fork(); say('forked'); } \
             catch (e) { say('no fork: ' + e.message); }"]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert!(outcome.transcript[0].text.starts_with("no fork:"));
        assert_eq!(outcome.fork_attempts, 1);
        assert_eq!(outcome.spawn_attempts, 0);
    }

    #[test]
    fn artifact_is_rejected_and_catchable_not_a_crash() {
        let mut source = ScriptedSource::new(["try { await artifact(1); say('fetched'); } \
             catch (e) { say('no artifact: ' + e.message); }"]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert!(outcome.transcript[0].text.starts_with("no artifact:"));
        assert_eq!(outcome.artifact_attempts, 1);
    }

    #[test]
    fn attempt_counts_accumulate_across_repeated_calls_in_one_program() {
        // A model that keeps reaching for the same refused verb should
        // show up as more than one attempt — this is a count, not a
        // "did it happen at all" flag.
        let mut source = ScriptedSource::new(["try { await fork(); } catch (e) {}\n\
             try { await fork(); } catch (e) {}\n\
             try { await spawn('x'); } catch (e) {}\n\
             say('done');"]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(outcome.fork_attempts, 2);
        assert_eq!(outcome.spawn_attempts, 1);
        assert_eq!(outcome.artifact_attempts, 0);
    }

    #[test]
    fn raise_then_resume_continues_the_raising_program_with_the_value() {
        let mut source = ScriptedSource::new([
            "const x = raise('pick_a_number'); say('got ' + x);",
            "return resume(42);",
        ]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(outcome.transcript[0].text, "got 42");
        assert_eq!(outcome.raise_count, 1);
        assert_eq!(outcome.completions_used, 2);
        assert_eq!(outcome.resume_count, 1);
        assert_eq!(outcome.abandon_count, 0);
        // The resumed program's very next act is `say(...)` — a call —
        // so this is exactly the case `handover_count` must NOT count:
        // the resumed value was used for real work, not just carried
        // straight out.
        assert_eq!(
            outcome.handover_count, 0,
            "a say() after resume is real work, not a handover"
        );
    }

    #[test]
    fn resume_that_flows_straight_to_completion_is_a_handover() {
        // The case `handover_count` exists to catch: nothing at all
        // happens between `resume(v)` landing and the program ending —
        // the resumed value is carried straight out, no further call.
        let mut source = ScriptedSource::new([
            "const x = raise('need_a_value'); return x;",
            "return resume(42);",
        ]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(outcome.raise_count, 1);
        assert_eq!(outcome.resume_count, 1);
        assert_eq!(outcome.handover_count, 1);
        assert_eq!(outcome.abandon_count, 0);
    }

    #[test]
    fn resume_that_makes_another_tool_call_is_not_a_handover() {
        // Same shape as the plain handover case, except the resumed
        // frame does one more `tools.*` call before finishing — real
        // orchestration work, so it must not count.
        let mut source = ScriptedSource::new([
            "const x = raise('need_a_value'); \
             const r = await tools.echo(x); return r;",
            "return resume(42);",
        ]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default().with("echo", Ok(serde_json::json!("42-echoed"))),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(outcome.resume_count, 1);
        assert_eq!(outcome.handover_count, 0);
    }

    #[test]
    fn a_resumable_trap_that_flows_straight_to_completion_is_also_a_handover() {
        // `handover_count` is defined over `resume()`, not over
        // `raise()` specifically — a resumed trap that does no further
        // work is exactly as much a handover as a resumed `raise()`.
        // This is the shape the live harness run actually hit: both of
        // its raises were traps.
        let mut source = ScriptedSource::new([
            "const x = [].nonExistentMethod(); return x + 1;",
            "return resume(4);",
        ]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(outcome.raise_count, 1);
        assert_eq!(outcome.resume_count, 1);
        assert_eq!(outcome.handover_count, 1);
    }

    #[test]
    fn raise_then_abandon_runs_a_replacement_program_instead() {
        let mut source = ScriptedSource::new([
            "say('original, should never run'); raise('need_help');",
            "return abandon();",
            "say('replacement ran');",
        ]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        // Only the replacement's `say` — the original's first `say`
        // did run before its own `raise` (that line executes first),
        // but the whole original frame is discarded on abandon, and
        // nothing about the replacement resumes it.
        assert_eq!(outcome.transcript.last().unwrap().text, "replacement ran");
        assert_eq!(outcome.completions_used, 3);
        assert_eq!(outcome.abandon_count, 1);
        assert_eq!(outcome.resume_count, 0);
        assert_eq!(outcome.handover_count, 0);
    }

    #[test]
    fn a_resumable_trap_gets_a_handler_the_same_way_a_raise_does() {
        let mut source = ScriptedSource::new([
            "const x = [] - 1; say('recovered: ' + x);",
            "return resume(0);",
        ]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(outcome.transcript[0].text, "recovered: 0");
        assert_eq!(outcome.raise_count, 1);
        assert_eq!(outcome.resume_count, 1);
        assert_eq!(outcome.handover_count, 0, "say() after resume intervenes");
    }

    #[test]
    fn nested_raise_resolves_both_levels() {
        let mut source = ScriptedSource::new([
            "const x = raise('outer'); say('outer got ' + x);",
            "const y = raise('inner'); return resume(y + 1);",
            "return resume(100);",
        ]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(outcome.transcript[0].text, "outer got 101");
        assert_eq!(outcome.raise_count, 2);
        assert_eq!(outcome.completions_used, 3);
        assert_eq!(outcome.resume_count, 2);
        // Only the *inner* resume is a handover: the middle program's
        // entire continuation after `resume(100)` lands is
        // `return resume(y + 1);` — a pure computation forwarding a
        // decision, no call. The outer resume is not: `say(...)`
        // intervenes before the root program ends. This is the sharp
        // edge of the proxy worth having pinned down explicitly —
        // "forwarding a decision through a pure computation" and
        // "a handler doing real work with no more suspension" both
        // read as `handover_count` hits, because neither issues a call
        // between resume and Done. The counter cannot tell them apart;
        // only that a call did or didn't happen. See docs/22's
        // "Handover vs. deliberation detection" open item — this test
        // is the concrete case that open item is about.
        assert_eq!(outcome.handover_count, 1);
    }

    #[test]
    fn a_root_program_returning_a_decision_is_an_error() {
        let mut source = ScriptedSource::new(["return resume(1);"]);
        let result = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        );
        assert!(matches!(result, Err(RunError::RootReturnedADecision)));
    }

    #[test]
    fn a_handler_that_does_not_decide_is_an_error() {
        let mut source = ScriptedSource::new(["raise('x');", "1 + 1;"]);
        let result = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        );
        assert!(matches!(result, Err(RunError::HandlerDidNotDecide)));
    }

    #[test]
    fn a_completion_that_does_not_parse_retries_and_reports_the_source_if_exhausted() {
        // Only one program is scripted, so the repair loop's retry
        // (below) has nothing left to try after this one fails — the
        // surfaced error is `Completion` (the source ran dry), not
        // `DidNotParse` directly, since a parse failure alone no
        // longer terminates the run: see
        // `a_parse_failure_recovers_via_the_repair_loop`.
        let mut source = ScriptedSource::new(["const x = ;"]);
        let result = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        );
        assert!(
            matches!(result, Err(RunError::Completion(_))),
            "expected the retry to exhaust the scripted source, got {result:?}"
        );
    }

    #[test]
    fn a_parse_failure_recovers_via_the_repair_loop() {
        // `types.rs`'s `Cause::CompileFailed` doc comment names "the
        // repair loop"; this is it actually working — a parse failure
        // gets a chance to be corrected, the same way a resumable trap
        // does, rather than ending the whole run.
        let mut source = ScriptedSource::new(["const x = ;", "say('recovered');"]);
        let outcome = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(outcome.transcript[0].text, "recovered");
        assert_eq!(outcome.completions_used, 2);
        // A parse-failure retry is not a raise/trap/handler — nothing
        // suspended, no decision was asked for, just a corrected
        // resubmission of the same "turn".
        assert_eq!(outcome.raise_count, 0);
    }

    #[test]
    fn repeated_parse_failures_are_bounded_by_max_completions() {
        let mut source =
            ScriptedSource::new(["const x = ;", "const y = ;", "const z = ;", "const w = ;"]);
        let result = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig {
                max_completions: 2,
                ..RunConfig::default()
            },
        );
        assert!(matches!(result, Err(RunError::TooManyCompletions)));
    }

    #[test]
    fn a_truncated_completion_is_reported_distinctly() {
        struct AlwaysTruncated;
        impl CompletionSource for AlwaysTruncated {
            fn complete(&mut self, _doc: &Document) -> Result<Completion, String> {
                Ok(Completion {
                    text: String::new(),
                    thinking: Some("thinking forever".into()),
                    finish_reason: Some("length".into()),
                })
            }
        }
        let result = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut AlwaysTruncated,
            &RunConfig::default(),
        );
        assert!(matches!(
            result,
            Err(RunError::Truncated {
                while_thinking: true
            })
        ));
    }

    #[test]
    fn depth_limit_is_enforced_and_reported() {
        // Every handler raises again, forever — the depth guard must
        // stop this rather than recursing without bound.
        let mut source = ScriptedSource::new(std::iter::repeat_n("raise('again');", 20));
        let result = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig {
                max_depth: 3,
                max_completions: 100,
                exemplars: &[],
            },
        );
        assert!(matches!(result, Err(RunError::Depth(_))));
    }

    #[test]
    fn completion_budget_is_enforced_and_reported() {
        let mut source = ScriptedSource::new(std::iter::repeat_n("raise('again');", 100));
        let result = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig {
                max_depth: 1000,
                max_completions: 3,
                exemplars: &[],
            },
        );
        assert!(matches!(result, Err(RunError::TooManyCompletions)));
    }

    #[test]
    fn running_out_of_scripted_programs_is_a_clean_error_not_a_panic() {
        let mut source = ScriptedSource::new([]);
        let result = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        );
        assert!(matches!(result, Err(RunError::Completion(_))));
    }
}
