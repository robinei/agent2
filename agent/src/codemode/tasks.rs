//! Part H's fixed task set — "four or five fixed tasks... each one
//! where a large program is the right answer: fan-out over N inputs,
//! retry-and-branch, a pipeline with a judgment call in the middle.
//! Each has a checkable success condition."
//!
//! Every task here is runnable two ways: through [`runner::run`] with
//! a [`runner::ScriptedSource`] (this file's own tests — a
//! hand-written "ideal" program per task, verifying the *success
//! check itself* is correct, no network) and through a real
//! [`runner::LiveSource`] (a live run, exercised by
//! `agent codemode-harness`, not by `cargo test`, per the ground
//! rule: "Part H's harness... necessarily talks to a live model").
//!
//! This is the harness's task *definitions* — the "four or five fixed
//! tasks" and their success conditions. The three numbers Part H asks
//! for (median program length, round-trips, task success) are
//! computed from a live run's `RunOutcome` plus the program source
//! itself; that aggregation is `agent codemode-harness`'s job, not
//! this file's.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use super::runner::{FakeTools, RunOutcome};

/// One recorded call: the tool name and its JSON-ified arguments, in
/// the order `dispatch_call` (`runner.rs`) resolved them.
#[derive(Clone, Debug, PartialEq)]
pub struct Recorded {
    pub name: String,
    pub args: serde_json::Value,
}

type ToolResult = Result<serde_json::Value, String>;
type ScriptQueue = VecDeque<ToolResult>;

/// A [`FakeTools`] built from small per-name response queues — popped
/// in call order, so "fails the first time, succeeds the second"
/// (a retry-and-branch task's whole point) is just two queued
/// responses, not special-cased machinery. Every call is logged
/// (name + args) regardless of outcome, so a task's success check can
/// inspect not just *what* the program said but *what it actually
/// did* — `Rc<RefCell<..>>` because `FakeTools::call` takes `&self`
/// (many calls share one log) and a task's checker needs its own
/// handle to read it back after the run.
#[derive(Clone, Default)]
pub struct RecordingTools {
    log: Rc<RefCell<Vec<Recorded>>>,
    scripts: Rc<RefCell<HashMap<String, ScriptQueue>>>,
    ask_script: Rc<RefCell<ScriptQueue>>,
}

impl RecordingTools {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one more response for `name`, popped on its next call.
    /// Calling this twice for the same name queues two calls' worth —
    /// exactly a retry scenario's "fail once, then succeed".
    pub fn respond(&self, name: &str, response: ToolResult) -> &Self {
        self.scripts
            .borrow_mut()
            .entry(name.to_owned())
            .or_default()
            .push_back(response);
        self
    }

    pub fn respond_ask(&self, response: ToolResult) -> &Self {
        self.ask_script.borrow_mut().push_back(response);
        self
    }

    pub fn calls(&self) -> Vec<Recorded> {
        self.log.borrow().clone()
    }

    pub fn call_count(&self, name: &str) -> usize {
        self.calls().iter().filter(|c| c.name == name).count()
    }
}

impl FakeTools for RecordingTools {
    fn call(&self, name: &str, args: &[serde_json::Value]) -> Result<serde_json::Value, String> {
        let args_json = serde_json::Value::Array(args.to_vec());
        self.log.borrow_mut().push(Recorded {
            name: name.to_owned(),
            args: args_json,
        });
        let mut scripts = self.scripts.borrow_mut();
        let queue = scripts.entry(name.to_owned()).or_default();
        match queue.pop_front() {
            Some(response) => {
                // Once the queue empties, keep giving the last
                // response rather than erroring on the next call.
                // Found live (2026-09-10): a program recovering from
                // an abandon()/raise() cycle naturally re-reads a file
                // it already read, with no memory of the earlier
                // read — a real fixture should answer that the same
                // way a real file would, not report "no scripted
                // response left" and cascade into a chain of
                // misleading "unreadable" failures a fresh attempt
                // never actually caused. Task checks still inspect
                // exact call counts/order directly, so this loosens
                // nothing they check.
                if queue.is_empty() {
                    queue.push_back(response.clone());
                }
                response
            }
            None => Err(format!("no scripted response left for tool `{name}`")),
        }
    }

    fn ask(&self, who: Option<&str>, text: &str) -> Result<serde_json::Value, String> {
        // Logged into the same call history as `tools.*` (as `"ask"`)
        // so a task's check can see *where* it happened relative to
        // everything else, not just whether it happened at all.
        self.log.borrow_mut().push(Recorded {
            name: "ask".to_owned(),
            args: serde_json::json!([who, text]),
        });
        self.ask_script
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err("no scripted ask() response left".into()))
    }
}

/// One fixed task: a prompt, the fake environment it runs against, and
/// a checkable success condition read from the finished
/// [`RunOutcome`] plus what actually got called.
pub struct Task {
    pub name: &'static str,
    pub user_message: &'static str,
    /// What this task's `tools.*` actually are, in the exact form Step
    /// C2 says a tool surface is conveyed: "the names and signatures
    /// are card surface" — no schema, no registry, appended to the
    /// card as this agent's own fixed description of itself. Empty for
    /// a task that configures none. Live-only: `ScriptedSource` never
    /// sees the card at all, so this has no effect on this file's own
    /// scripted tests — only `harness::run_task` reads it.
    pub tool_manifest: &'static str,
    pub tools: fn() -> RecordingTools,
    pub check: fn(&RunOutcome, &RecordingTools) -> Result<(), String>,
}

fn contains(haystack: &[super::runner::Said], needle: &str) -> bool {
    haystack.iter().any(|s| s.text.contains(needle))
}

/// Case-insensitive, any-of match — for outcomes with more than one
/// natural wording ("passed" is exactly as correct a report of a
/// successful build as "succeeded"; the check should not prefer one
/// word choice over an equally correct one).
fn contains_any_ci(haystack: &[super::runner::Said], needles: &[&str]) -> bool {
    haystack.iter().any(|s| {
        let lower = s.text.to_lowercase();
        needles.iter().any(|n| lower.contains(&n.to_lowercase()))
    })
}

/// **Fan-out over N inputs.** Three independent files to read and
/// summarize — the natural shape for `Promise.all`, and a program
/// that only reads one of the three has not done the task.
pub const FAN_OUT: Task = Task {
    name: "fan-out",
    user_message: "read a.txt, b.txt, and c.txt, and tell me one interesting thing from each",
    tool_manifest: "This session's tools: tools.read_file(path) -> { content: string }.",
    tools: || {
        let t = RecordingTools::new();
        t.respond(
            "read_file",
            Ok(serde_json::json!({ "content": "a.txt: the ANSWER is 42" })),
        );
        t.respond(
            "read_file",
            Ok(serde_json::json!({ "content": "b.txt: the SECRET is qux" })),
        );
        t.respond(
            "read_file",
            Ok(serde_json::json!({ "content": "c.txt: the COUNT is 7" })),
        );
        t
    },
    check: |outcome, tools| {
        if tools.call_count("read_file") < 3 {
            return Err(format!(
                "expected all 3 files read, got {}",
                tools.call_count("read_file")
            ));
        }
        for needle in ["42", "qux", "7"] {
            if !contains(&outcome.transcript, needle) {
                return Err(format!(
                    "transcript never mentions {needle:?} from its file"
                ));
            }
        }
        Ok(())
    },
};

/// **Retry-and-branch.** The build fails once, then succeeds — the
/// program must try again rather than giving up (or reporting success)
/// on the first failure.
pub const RETRY: Task = Task {
    name: "retry-and-branch",
    user_message: "run the build; if it fails, try once more before giving up",
    tool_manifest: "This session's tools: tools.bash(command) -> { exit: number, output: string }. \
                     The build command is exactly `npm run build` — do not run anything else with it.",
    tools: || {
        let t = RecordingTools::new();
        t.respond(
            "bash",
            Ok(serde_json::json!({ "exit": 1, "output": "error: flaky link step" })),
        );
        t.respond(
            "bash",
            Ok(serde_json::json!({ "exit": 0, "output": "build succeeded" })),
        );
        t
    },
    check: |outcome, tools| {
        let n = tools.call_count("bash");
        if n < 2 {
            return Err(format!("expected at least 2 attempts, got {n}"));
        }
        if !contains_any_ci(
            &outcome.transcript,
            &["succeed", "success", "passed", "pass"],
        ) {
            return Err("transcript never reports success after the retry".into());
        }
        Ok(())
    },
};

/// **A pipeline with a judgment call in the middle.** Recon first
/// (Step C4: "look before you leap, once"), then a genuine decision
/// this harness answers via `ask()` — the config file's format is
/// ambiguous enough that guessing would be wrong, so proceeding
/// without asking is a failure here regardless of what else happened.
pub const JUDGMENT_IN_THE_MIDDLE: Task = Task {
    name: "judgment-in-the-middle",
    user_message: "the deploy config looks stale — check it and fix whatever's wrong",
    tool_manifest: "This session's tools: tools.read_file(path) -> { content: string }; \
                     tools.write_file(path, content) -> { written: boolean }. The config \
                     path is 'deploy.yaml'.",
    tools: || {
        let t = RecordingTools::new();
        t.respond(
            "read_file",
            Ok(serde_json::json!({
                "content": "region: eu-west-1  # or is it us-east-1 now? both are referenced elsewhere"
            })),
        );
        // Deliberately unambiguous once asked: the current value is
        // wrong and the answer says so directly, so a `write_file` is
        // the only correct outcome — found live (2026-09-10) that an
        // answer merely confirming the existing value ("keep X") makes
        // "no edit" a reasonable reading too, which this check can't
        // tell apart from skipping the question.
        t.respond_ask(Ok(serde_json::json!(
            "us-east-1 is correct now — we migrated off eu-west-1 last quarter, please update the file"
        )));
        t.respond("write_file", Ok(serde_json::json!({ "written": true })));
        t
    },
    check: |outcome, tools| {
        let calls = tools.calls();
        let index_of = |name: &str| calls.iter().position(|c| c.name == name);
        let Some(read_at) = index_of("read_file") else {
            return Err("never looked at the config before acting".into());
        };
        let Some(write_at) = index_of("write_file") else {
            return Err("never wrote a fix after resolving the ambiguity".into());
        };
        if read_at >= write_at {
            return Err("wrote before ever reading the config".into());
        }
        // The genuinely checkable part: the ambiguity must have been
        // resolved by asking or raising, not guessed — and that
        // resolution has to sit *between* the read and the write, not
        // merely happen somewhere in the run.
        let asked_between = index_of("ask").is_some_and(|i| read_at < i && i < write_at);
        if !asked_between && outcome.raise_count == 0 {
            return Err("resolved the ambiguity without asking or raising for it".into());
        }
        Ok(())
    },
};

/// **Match the program to the task.** A trivial question needs no
/// tools and no `raise()` — a two-line program that just answers.
/// Failing this by *over*-orchestrating is exactly the failure mode
/// this whole phase exists to move away from.
pub const TRIVIAL_QUESTION: Task = Task {
    name: "trivial-question",
    user_message: "what is 12 + 30?",
    tool_manifest: "",
    tools: RecordingTools::new,
    check: |outcome, tools| {
        if !contains(&outcome.transcript, "42") {
            return Err("never said the answer".into());
        }
        if !tools.calls().is_empty() {
            return Err(format!(
                "used {} tool call(s) for a question needing none",
                tools.calls().len()
            ));
        }
        if outcome.raise_count > 0 {
            return Err("raised for a question needing no judgement at all".into());
        }
        Ok(())
    },
};

pub const ALL: &[Task] = &[FAN_OUT, RETRY, JUDGMENT_IN_THE_MIDDLE, TRIVIAL_QUESTION];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codemode::runner::{RunConfig, ScriptedSource, run};

    const CARD: &str = "test card";

    /// Each task's checker, verified against a hand-written "ideal"
    /// program — no network, and no dependence on what a real model
    /// happens to write. This is the harness verifying *itself*: if
    /// this fails, the task's success condition is wrong, not the
    /// (unbuilt-here) model output.
    #[test]
    fn fan_out_check_accepts_an_ideal_program() {
        let tools = (FAN_OUT.tools)();
        let mut source = ScriptedSource::new(["const [a, b, c] = await Promise.all([\
                tools.read_file('a.txt'), tools.read_file('b.txt'), tools.read_file('c.txt')]);\
             say(a.content + ' | ' + b.content + ' | ' + c.content);"]);
        let outcome = run(
            CARD,
            FAN_OUT.user_message,
            &tools,
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        (FAN_OUT.check)(&outcome, &tools).unwrap();
    }

    #[test]
    fn fan_out_check_rejects_reading_only_one_file() {
        let tools = (FAN_OUT.tools)();
        let mut source =
            ScriptedSource::new(["const a = await tools.read_file('a.txt'); say(a.content);"]);
        let outcome = run(
            CARD,
            FAN_OUT.user_message,
            &tools,
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert!((FAN_OUT.check)(&outcome, &tools).is_err());
    }

    #[test]
    fn retry_check_accepts_a_program_that_retries_once() {
        let tools = (RETRY.tools)();
        let mut source = ScriptedSource::new(["let r = await tools.bash('build'); \
             if (r.exit !== 0) { r = await tools.bash('build'); } \
             say(r.exit === 0 ? 'build succeeded' : 'build still failing: ' + r.output);"]);
        let outcome = run(
            CARD,
            RETRY.user_message,
            &tools,
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        (RETRY.check)(&outcome, &tools).unwrap();
    }

    #[test]
    fn retry_check_rejects_giving_up_after_one_failure() {
        let tools = (RETRY.tools)();
        let mut source = ScriptedSource::new([
            "const r = await tools.bash('build'); say('build failed: ' + r.output);",
        ]);
        let outcome = run(
            CARD,
            RETRY.user_message,
            &tools,
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert!((RETRY.check)(&outcome, &tools).is_err());
    }

    #[test]
    fn judgment_check_accepts_recon_then_ask_then_write() {
        let tools = (JUDGMENT_IN_THE_MIDDLE.tools)();
        let mut source = ScriptedSource::new(["const cfg = await tools.read_file('deploy.yml'); \
             const region = await ask('robin', 'which region is right? ' + cfg.content); \
             await tools.write_file('deploy.yml', 'region: ' + region); \
             say('updated the config');"]);
        let outcome = run(
            CARD,
            JUDGMENT_IN_THE_MIDDLE.user_message,
            &tools,
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        (JUDGMENT_IN_THE_MIDDLE.check)(&outcome, &tools).unwrap();
    }

    #[test]
    fn judgment_check_rejects_writing_without_ever_reading_first() {
        let tools = (JUDGMENT_IN_THE_MIDDLE.tools)();
        let mut source = ScriptedSource::new([
            "await tools.write_file('deploy.yml', 'region: us-east-1'); \
             say('fixed it');",
        ]);
        let outcome = run(
            CARD,
            JUDGMENT_IN_THE_MIDDLE.user_message,
            &tools,
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert!((JUDGMENT_IN_THE_MIDDLE.check)(&outcome, &tools).is_err());
    }

    #[test]
    fn judgment_check_rejects_writing_before_the_ambiguity_is_resolved() {
        // Reads, then writes a guess, *then* asks — the ask happened,
        // but too late to have informed the write. Catches exactly
        // what a pure "did it ever ask" check (without ordering) would
        // have missed.
        let tools = (JUDGMENT_IN_THE_MIDDLE.tools)();
        let mut source = ScriptedSource::new(["await tools.read_file('deploy.yml'); \
             await tools.write_file('deploy.yml', 'region: us-east-1'); \
             await ask('robin', 'was that the right region?'); \
             say('fixed it, hope that was right');"]);
        let outcome = run(
            CARD,
            JUDGMENT_IN_THE_MIDDLE.user_message,
            &tools,
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert!((JUDGMENT_IN_THE_MIDDLE.check)(&outcome, &tools).is_err());
    }

    #[test]
    fn trivial_question_check_accepts_a_two_line_program() {
        let tools = (TRIVIAL_QUESTION.tools)();
        let mut source = ScriptedSource::new(["say(String(12 + 30));"]);
        let outcome = run(
            CARD,
            TRIVIAL_QUESTION.user_message,
            &tools,
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        (TRIVIAL_QUESTION.check)(&outcome, &tools).unwrap();
    }

    #[test]
    fn trivial_question_check_rejects_unnecessary_tool_use() {
        let tools = (TRIVIAL_QUESTION.tools)();
        tools.respond("bash", Ok(serde_json::json!({ "exit": 0, "output": "42" })));
        let mut source = ScriptedSource::new([
            "const r = await tools.bash('echo $((12+30))'); say(r.output.trim());",
        ]);
        let outcome = run(
            CARD,
            TRIVIAL_QUESTION.user_message,
            &tools,
            &mut source,
            &RunConfig::default(),
        )
        .unwrap();
        assert!((TRIVIAL_QUESTION.check)(&outcome, &tools).is_err());
    }

    #[test]
    fn all_four_tasks_are_distinctly_named() {
        let names: std::collections::HashSet<_> = ALL.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), ALL.len());
    }
}
