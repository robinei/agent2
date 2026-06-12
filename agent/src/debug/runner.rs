//! Standalone debug runner (9_TUI Step 2): drives one VM in fuel slices
//! with a stub tool registry — no LLM, no harness. Everything here is
//! terminal-free and unit-testable; the TUI layer calls `tick` /
//! `step_instr` / `step_line` and renders the state.

use std::time::{Duration, Instant};

use interp::{InvokeCall, Program, StepResult, VM, Value};

/// Instructions per `tick` while running: large enough to make progress,
/// small enough that pause and render latency stay well under a frame.
pub const RUN_SLICE: u64 = 20_000;

/// Cap for one `step_line` keypress, so a hot loop confined to a single
/// source line cannot wedge the UI.
const LINE_STEP_CAP: u64 = 50_000;

#[derive(Debug, Clone, PartialEq)]
pub enum RunState {
    Paused,
    Running,
    /// Blocked awaiting stub-tool results (e.g. a pending `sleep`).
    Waiting,
    /// Suspended on a `raise(...)`; `resume_condition` continues with
    /// `null` as the raise result.
    Condition {
        condition: String,
        payload: String,
    },
    Done {
        value: String,
    },
    Failed {
        error: String,
    },
}

pub struct Runner {
    pub vm: VM,
    pub state: RunState,
    /// Deferred `sleep` resolutions: (due, promise).
    sleeps: Vec<(Instant, interp::PromisePtr)>,
}

impl Runner {
    pub fn new(program: Program, input: serde_json::Value) -> Result<Self, String> {
        let vm = VM::for_program(program, input).map_err(|e| e.message)?;
        Ok(Runner {
            vm,
            state: RunState::Paused,
            sleeps: Vec::new(),
        })
    }

    /// One running tick: a big fuel slice (only in `Running`).
    pub fn tick(&mut self) {
        if self.state == RunState::Running {
            let res = self.vm.step(RUN_SLICE);
            self.handle(res);
        }
    }

    /// Execute exactly one instruction (pauses a running program first).
    pub fn step_instr(&mut self) {
        if !self.steppable() {
            return;
        }
        self.state = RunState::Paused;
        let res = self.vm.step(1);
        self.handle(res);
    }

    /// Step instructions until the current source line changes (or the
    /// program yields/finishes, or the cap is hit).
    pub fn step_line(&mut self) {
        if !self.steppable() {
            return;
        }
        self.state = RunState::Paused;
        let start = self.current_line();
        for _ in 0..LINE_STEP_CAP {
            let res = self.vm.step(1);
            self.handle(res);
            if self.state != RunState::Paused {
                return;
            }
            if start.is_none() || self.current_line() != start {
                return;
            }
        }
    }

    pub fn toggle_run(&mut self) {
        self.state = match self.state {
            RunState::Paused => RunState::Running,
            RunState::Running => RunState::Paused,
            ref other => other.clone(),
        };
    }

    /// Resume a `Condition` with `null` as the raise result.
    pub fn resume_condition(&mut self) {
        if matches!(self.state, RunState::Condition { .. }) {
            self.vm.resume_raise(Value::Null);
            self.state = RunState::Paused;
        }
    }

    /// Resolve due `sleep` timers; a `Waiting` program becomes runnable
    /// again once something resolved.
    pub fn poll_timers(&mut self) {
        let now = Instant::now();
        let due: Vec<_> = {
            let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.sleeps)
                .into_iter()
                .partition(|(t, _)| *t <= now);
            self.sleeps = rest;
            due
        };
        if due.is_empty() {
            return;
        }
        for (_, promise) in due {
            let _ = self.vm.resolve_promise(promise, Value::Null);
        }
        if self.state == RunState::Waiting {
            self.state = RunState::Running;
        }
    }

    /// Earliest pending timer, for the event-loop poll timeout.
    pub fn next_timer(&self) -> Option<Instant> {
        self.sleeps.iter().map(|(t, _)| *t).min()
    }

    /// Source line (1-based) of the current instruction, when known.
    pub fn current_line(&self) -> Option<usize> {
        let span = *self.vm.spans.get(self.vm.ip as usize)?;
        if self.vm.source.is_empty() {
            return None;
        }
        let (line, _, _) = interp::diag::line_col(&self.vm.source, span);
        Some(line)
    }

    fn steppable(&self) -> bool {
        matches!(
            self.state,
            RunState::Paused | RunState::Running | RunState::Waiting
        )
    }

    fn handle(&mut self, res: Result<StepResult, interp::VMError>) {
        match res {
            Ok(StepResult::OutOfFuel) => {}
            Ok(StepResult::Done { value, .. }) => {
                let rendered = self
                    .vm
                    .stack_value_to_json(&value, 0)
                    .map(|j| j.to_string())
                    .unwrap_or_else(|_| format!("{value:?}"));
                self.state = RunState::Done { value: rendered };
            }
            Ok(StepResult::Pending { calls }) => {
                for call in calls {
                    self.serve(call);
                }
                // Still blocked: if nothing resolved synchronously the next
                // step would yield Pending again, so wait for timers.
                if !self.sleeps.is_empty() {
                    self.state = RunState::Waiting;
                }
            }
            Ok(StepResult::Raise { condition, payload }) => {
                let payload = payload
                    .map(|v| {
                        self.vm
                            .stack_value_to_json(&v, 0)
                            .map(|j| j.to_string())
                            .unwrap_or_else(|_| format!("{v:?}"))
                    })
                    .unwrap_or_default();
                self.state = RunState::Condition { condition, payload };
            }
            Err(e) => {
                self.state = RunState::Failed {
                    error: self.vm.render_error(&e),
                };
            }
        }
    }

    /// The stub tool registry: enough surface to exercise every
    /// `StepResult` path without a harness.
    ///
    /// - `echo(x)` → `x`
    /// - `sleep(ms)` → `null`, resolved after `ms` (drives `Waiting`)
    /// - `fail(msg)` → rejects with `msg`
    /// - anything else → rejects with an unknown-tool message
    fn serve(&mut self, call: InvokeCall) {
        match call.name.as_str() {
            "echo" => {
                let v = call.args.into_iter().next().unwrap_or(Value::Undefined);
                let _ = self.vm.resolve_promise(call.promise, v);
            }
            "sleep" => {
                let ms = match call.args.first() {
                    Some(Value::PosInt(n)) => *n,
                    Some(Value::Float(f)) if *f >= 0.0 => *f as u64,
                    _ => 0,
                };
                let due = Instant::now() + Duration::from_millis(ms);
                self.sleeps.push((due, call.promise));
            }
            "fail" => {
                let v = call
                    .args
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| Value::String(interp::RcStr::from("tool failed")));
                let _ = self.vm.reject_promise(call.promise, v);
            }
            other => {
                let msg = Value::String(interp::RcStr::from(
                    format!("unknown stub tool `{other}` (have: echo, sleep, fail)").as_str(),
                ));
                let _ = self.vm.reject_promise(call.promise, msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(src: &str) -> Runner {
        let prog = interp::compile(src).expect("compiles");
        Runner::new(prog, serde_json::Value::Null).expect("vm")
    }

    fn run_to_end(r: &mut Runner) {
        r.state = RunState::Running;
        for _ in 0..10_000 {
            r.poll_timers();
            r.tick();
            match r.state {
                RunState::Running | RunState::Waiting => {}
                _ => return,
            }
            if r.state == RunState::Waiting {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        panic!("did not finish: {:?}", r.state);
    }

    #[test]
    fn single_stepping_reaches_done() {
        let mut r = runner("return 1 + 1;");
        for _ in 0..1000 {
            r.step_instr();
            if let RunState::Done { ref value } = r.state {
                assert_eq!(value, "2");
                return;
            }
        }
        panic!("never finished");
    }

    #[test]
    fn echo_and_sleep_stubs_round_trip() {
        let mut r = runner("const x = await tools.echo(7); await tools.sleep(1); return x;");
        run_to_end(&mut r);
        assert_eq!(r.state, RunState::Done { value: "7".into() });
    }

    #[test]
    fn fail_stub_rejects_into_catch() {
        let mut r =
            runner(r#"try { await tools.fail("boom"); } catch (e) { return "caught " + e; }"#);
        run_to_end(&mut r);
        assert_eq!(
            r.state,
            RunState::Done {
                value: "\"caught boom\"".into()
            }
        );
    }

    #[test]
    fn raise_pauses_and_resume_continues() {
        let mut r = runner(r#"raise("demo", { n: 1 }); return "after";"#);
        run_to_end(&mut r);
        match &r.state {
            RunState::Condition { condition, payload } => {
                assert_eq!(condition, "demo");
                assert!(payload.contains("\"n\":1"), "{payload}");
            }
            other => panic!("expected Condition, got {other:?}"),
        }
        r.resume_condition();
        run_to_end(&mut r);
        assert_eq!(
            r.state,
            RunState::Done {
                value: "\"after\"".into()
            }
        );
    }

    #[test]
    fn demo_sample_runs_to_condition_then_done() {
        // The committed demo exercises calls, echo/sleep/fail awaits,
        // console output, and a raise — headless, same Runner the TUI uses.
        let mut r = runner(include_str!("../../samples/demo.js"));
        run_to_end(&mut r);
        match &r.state {
            RunState::Condition { condition, .. } => assert_eq!(condition, "demo_condition"),
            other => panic!("expected the demo condition, got {other:?}"),
        }
        assert!(
            r.vm.console_lines
                .iter()
                .any(|l| l.contains("fib(10) = 55")),
            "{:?}",
            r.vm.console_lines
        );
        r.resume_condition();
        run_to_end(&mut r);
        assert_eq!(
            r.state,
            RunState::Done {
                value: "\"done\"".into()
            }
        );
    }

    #[test]
    fn step_line_advances_past_the_current_line() {
        let mut r = runner("let a = 1;\nlet b = 2;\nreturn a + b;");
        // Step once to land on real code, note the line, step-line once.
        r.step_instr();
        let before = r.current_line().unwrap();
        r.step_line();
        if r.state == RunState::Paused {
            assert_ne!(r.current_line().unwrap(), before);
        }
    }
}
