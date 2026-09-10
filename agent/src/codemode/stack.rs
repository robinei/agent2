//! The handler stack (phase 20 doc, Part D "`raise()`, the handler
//! stack, and the decision value", Step D1).
//!
//! `raise()` suspends a running program and asks the mind for a
//! decision; the mind answers by writing another program, which runs
//! *while the raising frame is still live* — Lisp's actual semantics,
//! which needs a stack of VMs rather than one. [`ProgramStack`] is
//! that stack: host-side, innermost scheduled, every other frame
//! frozen for *execution* only.
//!
//! Not wired to a session loop or the event log — this is the
//! container and its two operations (push, apply a decision), usable
//! and testable on `interp`'s public VM API alone. What actually
//! raises a program onto this stack (compiling a handler from an LLM
//! completion) and what happens after a decision applies (rendering
//! the next document, Part B) are host-loop concerns this module does
//! not have an opinion on.

use interp::{PromisePtr, ResumeMode, VM, VMError, Value};

use super::decision::Decision;

/// Why a frame is suspended, waiting for the frame above it (its
/// handler) to decide. Recorded at push time from whatever `step()`
/// (or the `Err` it returned) said stopped it — the two shapes
/// DESIGN.md's suspension table lists that a *program* can hit and
/// still be resumed with a value: `raise()` and a trapped, resumable
/// error. (`OutOfFuel` never reaches this stack — the host just calls
/// `step` again on the same frame; it never stops being current.)
#[derive(Debug)]
pub enum Suspension {
    /// `StepResult::Raise { condition, payload }` — resume via
    /// `VM::resume_raise`.
    Raised {
        condition: String,
        payload: Option<Value>,
    },
    /// A trapped runtime error. Resumable only when its own
    /// `resume == ResumeMode::PushValueThenContinue` — a `NotResumable`
    /// trap accepts no injected value at all, so `Decision::Resume`
    /// against one is a caller error, not something this stack
    /// silently coerces into an `Abandon`.
    Trapped(VMError),
}

impl Suspension {
    pub fn is_resumable(&self) -> bool {
        match self {
            Suspension::Raised { .. } => true,
            Suspension::Trapped(e) => matches!(e.resume, ResumeMode::PushValueThenContinue),
        }
    }
}

struct Frame {
    vm: VM,
    /// `None` for the current (topmost) frame — it is running, not
    /// suspended. Every frame below it always carries one: that is
    /// what put it there.
    suspension: Option<Suspension>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PushError {
    /// Depth is bounded by host policy (Step D1): every frame is a
    /// full-context inference, unlike an ordinary call stack.
    DepthExceeded { depth: usize, max: usize },
}

#[derive(Debug)]
pub enum ApplyError {
    /// The stack held only the frame that just decided — there is no
    /// caller beneath it to apply the decision to. Not reachable
    /// through ordinary use (a frame only exists on this stack because
    /// something raised into it, which means a caller pushed it), kept
    /// as a checked error rather than a panic on a stack a future
    /// caller could still misuse.
    NoCaller,
    /// `Decision::Resume` against a frame whose recorded `Suspension`
    /// is not resumable (`Suspension::is_resumable() == false`) — the
    /// caller's mistake to report, not this stack's to paper over by
    /// silently treating it as an `Abandon`.
    NotResumable,
    /// The VM-level resume call itself failed (`resume_with`'s own
    /// bad-arg check, or a JSON value too deeply nested to convert).
    Vm(VMError),
}

/// A stack of independently-stepped VMs — the load-bearing property
/// stays intact by construction: only [`ProgramStack::current_mut`]
/// is ever stepped, and it borrows exactly one frame at a time, so no
/// VM is ever reachable from another's call stack.
pub struct ProgramStack {
    frames: Vec<Frame>,
}

impl ProgramStack {
    /// A stack with just the root program running.
    pub fn new(root: VM) -> Self {
        ProgramStack {
            frames: vec![Frame {
                vm: root,
                suspension: None,
            }],
        }
    }

    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// The only frame ever stepped.
    pub fn current_mut(&mut self) -> &mut VM {
        &mut self.frames.last_mut().expect("stack is never empty").vm
    }

    pub fn current(&self) -> &VM {
        &self.frames.last().expect("stack is never empty").vm
    }

    /// A specific frame's VM by depth index (`0` is the root), for
    /// resolving a promise on a frame that is not current — "outer
    /// frames are frozen for execution only: their in-flight calls
    /// keep landing" (Step D1). Resolving/rejecting a promise needs no
    /// stepping, so this needs no special-casing beyond plain access.
    pub fn frame_mut(&mut self, index: usize) -> Option<&mut VM> {
        self.frames.get_mut(index).map(|f| &mut f.vm)
    }

    /// Resolve a settled tool call on whichever frame it belongs to —
    /// the common case an outer frame's own outstanding calls need
    /// while an inner handler is deliberating.
    pub fn resolve_promise_at(
        &mut self,
        index: usize,
        promise: PromisePtr,
        value: Value,
    ) -> Result<(), VMError> {
        self.frames[index].vm.resolve_promise(promise, value)
    }

    /// Push a handler for the current frame's suspension: the current
    /// frame is marked suspended (this is what `raise()`/a trap
    /// running out the mind's turn actually means — the frame that
    /// *was* current stops being steppable until this handler
    /// decides), and `handler` becomes the new current frame.
    pub fn push(
        &mut self,
        suspension: Suspension,
        handler: VM,
        max_depth: usize,
    ) -> Result<(), PushError> {
        if self.frames.len() >= max_depth {
            return Err(PushError::DepthExceeded {
                depth: self.frames.len(),
                max: max_depth,
            });
        }
        self.frames
            .last_mut()
            .expect("stack is never empty")
            .suspension = Some(suspension);
        self.frames.push(Frame {
            vm: handler,
            suspension: None,
        });
        Ok(())
    }

    /// Apply the current (topmost) frame's decision to the frame it
    /// was handling — the whole of Step D2's `resume`/`abandon`
    /// sequencing:
    ///
    /// - `Resume(value)`: pop the decided handler; inject `value` into
    ///   the new top via whichever of `resume_raise`/`resume_with` its
    ///   recorded [`Suspension`] calls for. The new top is now current
    ///   again — "the raising program continues".
    /// - `Abandon`: pop the decided handler; **replace** the new top's
    ///   `VM` outright with `replacement()`, called lazily so a caller
    ///   need not construct one for the (more common) `Resume` case.
    ///   "`abandon()` replaces the caller frame; it does not unwind
    ///   the stack" — from depth 3, this pops frame 3 and replaces
    ///   frame 2, leaving frame 1 (if any) untouched underneath.
    pub fn apply_decision(
        &mut self,
        decision: Decision,
        replacement: impl FnOnce() -> VM,
    ) -> Result<(), ApplyError> {
        self.frames.pop();
        let target = self.frames.last_mut().ok_or(ApplyError::NoCaller)?;
        let suspension = target
            .suspension
            .take()
            .expect("every non-root frame carries a suspension");
        match decision {
            Decision::Resume(value) => {
                if !suspension.is_resumable() {
                    // Put it back — the decision did not apply, so the
                    // frame is exactly as suspended as it was.
                    target.suspension = Some(suspension);
                    return Err(ApplyError::NotResumable);
                }
                let v = target
                    .vm
                    .json_to_stack_value(&value, 0)
                    .map_err(ApplyError::Vm)?;
                match suspension {
                    Suspension::Raised { .. } => target.vm.resume_raise(v),
                    Suspension::Trapped(e) => {
                        target.vm.resume_with(&e, v).map_err(ApplyError::Vm)?;
                    }
                }
            }
            Decision::Abandon => {
                target.vm = replacement();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interp::{StepResult, compile};

    fn vm_for(source: &str) -> VM {
        let prog = compile(source).unwrap_or_else(|e| panic!("compile error: {e:?}"));
        VM::for_program(prog, serde_json::Value::Null).unwrap()
    }

    /// Step to the first `Raise`, returning the `(condition, payload)`
    /// so a test can build the matching [`Suspension`] to push with.
    fn run_to_raise(vm: &mut VM) -> (String, Option<Value>) {
        match vm.step(u64::MAX).unwrap() {
            StepResult::Raise { condition, payload } => (condition, payload),
            other => panic!("expected Raise, got {other:?}"),
        }
    }

    #[test]
    fn a_fresh_stack_has_depth_one() {
        let stack = ProgramStack::new(vm_for("return 1;"));
        assert_eq!(stack.depth(), 1);
    }

    #[test]
    fn push_makes_the_handler_current_and_depth_two() {
        let mut root = vm_for("raise('x');");
        let (condition, payload) = run_to_raise(&mut root);
        let mut stack = ProgramStack::new(root);
        stack
            .push(
                Suspension::Raised { condition, payload },
                vm_for("return resume(1);"),
                8,
            )
            .unwrap();
        assert_eq!(stack.depth(), 2);
        // The handler is what steps next.
        match stack.current_mut().step(u64::MAX).unwrap() {
            StepResult::Done { .. } => {}
            other => panic!("expected the handler to run, got {other:?}"),
        }
    }

    #[test]
    fn depth_is_bounded_and_reported_as_a_typed_error() {
        let mut stack = ProgramStack::new(vm_for("raise('x');"));
        run_to_raise(stack.current_mut());
        assert_eq!(
            stack.push(
                Suspension::Raised {
                    condition: "x".into(),
                    payload: None
                },
                vm_for("return 1;"),
                1, // max_depth already met at depth 1
            ),
            Err(PushError::DepthExceeded { depth: 1, max: 1 })
        );
        assert_eq!(stack.depth(), 1, "a rejected push must not apply");
    }

    #[test]
    fn resume_injects_the_value_and_the_raising_program_continues() {
        let mut root = vm_for("const x = raise('pick'); return x + 1;");
        let (condition, payload) = run_to_raise(&mut root);
        let mut stack = ProgramStack::new(root);
        stack
            .push(
                Suspension::Raised { condition, payload },
                vm_for("return resume(41);"),
                8,
            )
            .unwrap();
        // Run the handler to its decision.
        let decision = match stack.current_mut().step(u64::MAX).unwrap() {
            StepResult::Done { value, .. } => super::super::decision::read(stack.current(), &value)
                .expect("handler returned a decision"),
            other => panic!("expected Done, got {other:?}"),
        };
        stack
            .apply_decision(decision, || unreachable!("Resume never calls replacement"))
            .unwrap();
        assert_eq!(
            stack.depth(),
            1,
            "the handler is gone; the raiser is current again"
        );
        match stack.current_mut().step(u64::MAX).unwrap() {
            StepResult::Done { value, .. } => assert_eq!(value, Value::Float(42.0)),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn abandon_replaces_the_caller_frame_not_the_whole_stack() {
        // Depth 3: root raises, its handler itself raises (nested),
        // and the innermost handler abandons — only frame 2 (the
        // middle one) is replaced; frame 1 (root) is untouched
        // underneath and never even sees this happen.
        let mut root = vm_for("raise('outer');");
        let (c1, p1) = run_to_raise(&mut root);
        let mut stack = ProgramStack::new(root);
        stack
            .push(
                Suspension::Raised {
                    condition: c1,
                    payload: p1,
                },
                vm_for("raise('inner');"),
                8,
            )
            .unwrap();
        let (c2, p2) = run_to_raise(stack.current_mut());
        stack
            .push(
                Suspension::Raised {
                    condition: c2,
                    payload: p2,
                },
                vm_for("return abandon();"),
                8,
            )
            .unwrap();
        assert_eq!(stack.depth(), 3);
        let decision = match stack.current_mut().step(u64::MAX).unwrap() {
            StepResult::Done { value, .. } => {
                super::super::decision::read(stack.current(), &value).unwrap()
            }
            other => panic!("expected Done, got {other:?}"),
        };
        stack
            .apply_decision(decision, || vm_for("return 99;"))
            .unwrap();
        assert_eq!(
            stack.depth(),
            2,
            "the innermost handler is gone; frame 1 (root) is untouched below"
        );
        // The new frame 2 is the replacement, not the original middle
        // program (which would have hung forever on its own raise).
        match stack.current_mut().step(u64::MAX).unwrap() {
            StepResult::Done { value, .. } => assert_eq!(value, Value::PosInt(99)),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn resume_against_a_not_resumable_trap_is_rejected_not_silently_coerced() {
        // An uncaught `throw` is `NotResumable` (interp's own
        // `throw_without_handler_is_uncaught_exception`) — unlike a
        // resumable operational trap (e.g. `[] - 1`), there is no
        // single failed instruction to feed a replacement value into.
        let mut root = vm_for("throw 'boom';");
        let err = loop {
            match root.step(u64::MAX) {
                Err(e) => break e,
                Ok(StepResult::Done { .. }) => panic!("expected an error"),
                Ok(_) => {}
            }
        };
        assert!(matches!(err.resume, ResumeMode::NotResumable));
        let mut stack = ProgramStack::new(root);
        stack
            .push(Suspension::Trapped(err), vm_for("return resume(1);"), 8)
            .unwrap();
        let result = stack.apply_decision(Decision::Resume(serde_json::json!(1)), || {
            unreachable!("not taken on Resume")
        });
        assert!(matches!(result, Err(ApplyError::NotResumable)));
        // The frame is left exactly as suspended as it was — the
        // rejected decision did not partially apply.
        assert_eq!(stack.depth(), 1);
    }

    #[test]
    fn resume_against_a_resumable_trap_uses_resume_with() {
        let mut root = vm_for("return [] - 1;"); // TypeError, resumable
        let err = match root.step(u64::MAX) {
            Err(e) => e,
            other => panic!("expected a trapped error, got {other:?}"),
        };
        assert!(matches!(err.resume, ResumeMode::PushValueThenContinue));
        let mut stack = ProgramStack::new(root);
        stack
            .push(Suspension::Trapped(err), vm_for("return resume(0);"), 8)
            .unwrap();
        let decision = match stack.current_mut().step(u64::MAX).unwrap() {
            StepResult::Done { value, .. } => {
                super::super::decision::read(stack.current(), &value).unwrap()
            }
            other => panic!("expected Done, got {other:?}"),
        };
        stack
            .apply_decision(decision, || unreachable!("Resume never calls replacement"))
            .unwrap();
        assert_eq!(stack.depth(), 1);
        match stack.current_mut().step(u64::MAX).unwrap() {
            // `resume_with` pushes exactly the value handed to it —
            // no re-execution of the failed subtraction, so this is
            // the JSON `0` as parsed (`PosInt`), not a recomputed
            // float.
            StepResult::Done { value, .. } => assert_eq!(value, Value::PosInt(0)),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn outer_frames_keep_resolving_promises_while_an_inner_handler_runs() {
        // Frame 0 has an outstanding `tools.*` call when it raises;
        // the host can still settle that call while frame 1 (the
        // handler) is the one being stepped — "outer frames are
        // frozen for execution only". An unawaited `Invoke` does not
        // yield by itself (only `Await`/`Raise`/fuel exhaustion do),
        // so to learn `p2`'s promise id before the raise, `p1` is
        // awaited first — draining *both* calls' outbox entries at
        // that single yield point, per `InvokeCall`'s own doc
        // ("every tool call started since the last yield").
        let mut root = vm_for(
            "const p1 = tools.a(); const p2 = tools.b();              await p1; raise('x'); return await p2;",
        );
        let (p1_promise, p2_promise) = match root.step(u64::MAX).unwrap() {
            StepResult::Pending { calls } => {
                assert_eq!(calls.len(), 2);
                (calls[0].promise, calls[1].promise)
            }
            other => panic!("expected Pending, got {other:?}"),
        };
        root.resolve_promise(p1_promise, Value::Undefined).unwrap();
        let (condition, payload) = run_to_raise(&mut root);
        let mut stack = ProgramStack::new(root);
        stack
            .push(
                Suspension::Raised { condition, payload },
                vm_for("return resume(1);"),
                8,
            )
            .unwrap();
        // Settle frame 0's still-outstanding promise while frame 1
        // is current.
        stack
            .resolve_promise_at(0, p2_promise, Value::PosInt(7))
            .unwrap();
        // Run the handler to its decision and apply it.
        let decision = match stack.current_mut().step(u64::MAX).unwrap() {
            StepResult::Done { value, .. } => {
                super::super::decision::read(stack.current(), &value).unwrap()
            }
            other => panic!("expected Done, got {other:?}"),
        };
        stack
            .apply_decision(decision, || unreachable!("Resume never calls replacement"))
            .unwrap();
        // Frame 0 resumes and its already-settled `await p` completes
        // immediately with the value resolved while it was frozen.
        match stack.current_mut().step(u64::MAX).unwrap() {
            StepResult::Done { value, .. } => assert_eq!(value, Value::PosInt(7)),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn a_lone_frame_deciding_has_no_caller() {
        // Defensive: applying a decision when the stack is down to one
        // frame (nothing pushed a handler for it) is a checked error,
        // not a panic — the container does not assume its own
        // invariants against a caller that never pushed correctly.
        let mut stack = ProgramStack::new(vm_for("return resume(1);"));
        let decision = match stack.current_mut().step(u64::MAX).unwrap() {
            StepResult::Done { value, .. } => {
                super::super::decision::read(stack.current(), &value).unwrap()
            }
            other => panic!("expected Done, got {other:?}"),
        };
        assert!(matches!(
            stack.apply_decision(decision, || unreachable!()),
            Err(ApplyError::NoCaller)
        ));
    }
}
