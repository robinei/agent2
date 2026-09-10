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
    pub completions_used: usize,
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
            Err("this harness does not back spawn() with a real agent yet".into())
        }
        Ok(HarnessEffect::Fork) => {
            Err("this harness does not back fork() with a real context yet".into())
        }
        Ok(HarnessEffect::Artifact { .. }) => {
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
            from: "robin".into(),
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

    let root_completion = take_completion(source, &root_doc)?;
    let root_vm = program_from_completion(&root_completion)?;
    let mut stack = ProgramStack::new(root_vm);
    // The document each currently-stacked frame was generated from —
    // same length and indexing as the stack, so an `Abandon` can
    // regenerate a replacement from exactly what the frame being
    // replaced originally saw.
    let mut docs_by_depth = vec![root_doc];

    let mut transcript = Vec::new();
    let mut appended = Vec::new();
    let mut raise_count = 0;

    loop {
        let step = stack.current_mut().step(u64::MAX);
        match step {
            Ok(StepResult::Pending { calls }) => {
                for call in &calls {
                    dispatch_call(
                        stack.current_mut(),
                        call,
                        tools,
                        &mut transcript,
                        &mut appended,
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
                    dispatch_call(
                        stack.current_mut(),
                        call,
                        tools,
                        &mut transcript,
                        &mut appended,
                    )?;
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
                        completions_used,
                    });
                }
                // A handler finished — read and apply its decision.
                let Some(decision) = decision::read(stack.current(), &value) else {
                    return Err(RunError::HandlerDidNotDecide);
                };
                match decision {
                    Decision::Resume(v) => {
                        docs_by_depth.pop();
                        stack
                            .apply_decision(Decision::Resume(v), || unreachable!())
                            .map_err(RunError::Apply)?;
                    }
                    Decision::Abandon => {
                        docs_by_depth.pop();
                        // The new top (post-pop) is the frame being
                        // replaced; regenerate from what *it* saw.
                        let replacement_doc = docs_by_depth
                            .last()
                            .expect("a non-root frame always has a caller doc")
                            .clone();
                        let completion = take_completion(source, &replacement_doc)?;
                        let replacement_vm = program_from_completion(&completion)?;
                        stack
                            .apply_decision(Decision::Abandon, || replacement_vm)
                            .map_err(RunError::Apply)?;
                        *docs_by_depth.last_mut().unwrap() = replacement_doc;
                    }
                }
            }
            Ok(StepResult::Raise { condition, payload }) => {
                raise_count += 1;
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
                let completion = take_completion(source, &handler_doc)?;
                let handler_vm = program_from_completion(&completion)?;
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
                let raising_source = stack.current().source.to_string();
                let suspension = Suspension::Trapped(vm_error);
                let tail = handler_tail(&raising_source, &suspension);
                let handler_doc = docs_by_depth
                    .last()
                    .expect("stack is never empty")
                    .clone()
                    .with_tail(&tail);
                let completion = take_completion(source, &handler_doc)?;
                let handler_vm = program_from_completion(&completion)?;
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
        let mut source = ScriptedSource::new(["say('robin', 'done');"]);
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
                to: Some("robin".into()),
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
            ScriptedSource::new(["const a = await ask('robin', 'q?'); say(String(a));"]);
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
    fn a_completion_that_does_not_parse_is_reported_with_its_source() {
        let mut source = ScriptedSource::new(["const x = ;"]);
        let result = run(
            CARD,
            "go",
            &TestTools::default(),
            &mut source,
            &RunConfig::default(),
        );
        match result {
            Err(RunError::DidNotParse { source, .. }) => assert_eq!(source, "const x = ;"),
            other => panic!("expected DidNotParse, got {other:?}"),
        }
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
