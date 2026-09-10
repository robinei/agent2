//! Frame introspection (phase 20 doc, Part F).
//!
//! "The load-bearing property is stated as 'stop between two
//! instructions with the heap, log, await-chain, and console buffer
//! inspectable.' Today all of that is spent rendering a *text report*
//! the harness clips. A handler that can query it instead is the
//! largest unrealized payoff in the architecture." Most of the
//! underlying capability already exists in `interp`, built for the
//! debugger (9_TUI) — `VM::frames()`, `VM::console_lines`,
//! `VM::source` — and is already exactly the "stable projection, not
//! `CallFrame` itself" Part F asks for (`FrameView` is `interp`'s own
//! read-only debugger view, not the raw internal `CallFrame`). This
//! module is the harness-facing wrapper: a JSON-serializable snapshot
//! a handler dispatcher can hand to a rendered `vm` binding, once one
//! exists (no such binding is built here — see the module's own doc
//! in `mod.rs` on what this phase leaves to a host loop).
//!
//! Read only, with no exceptions (Part F): every accessor here takes
//! `&VM`, never `&mut VM` — nothing executes, nothing is written back.

use interp::{VM, Value};

use super::stack::Suspension;

/// One local or temporary value, named when debug info makes that
/// possible — `interp`'s own graceful degradation (`FrameView::name`)
/// on a `VM::new` program with no debug table.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedValue {
    pub name: Option<String>,
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FrameSnapshot {
    pub function: String,
    pub locals: Vec<NamedValue>,
}

/// Why the frame this snapshot is of is suspended — `None` for a
/// still-running frame (the innermost of a `ProgramStack`, which has
/// nothing to introspect *as a suspension* because it is not one).
#[derive(Clone, Debug, PartialEq)]
pub enum ConditionSnapshot {
    Raised {
        condition: String,
        payload: Option<serde_json::Value>,
    },
    Trapped {
        kind: String,
        message: String,
    },
    Posted,
}

/// The whole read-only projection Part F asks for: `vm.frames()`,
/// `vm.console()`, `vm.condition`, `vm.source` — everything but
/// `vm.caller`, which is a [`super::stack::ProgramStack`]-level
/// concept ("read further up the *program* stack"), not a property of
/// one VM, and so lives one level up from this snapshot rather than
/// inside it.
#[derive(Clone, Debug, PartialEq)]
pub struct VmSnapshot {
    /// Outermost (root) first, matching `VM::frames()` — the VM's own
    /// call stack, not to be confused with the program stack
    /// (`17_BRANCHES`: `frame` is reserved for exactly this).
    pub frames: Vec<FrameSnapshot>,
    pub console: Vec<String>,
    pub source: String,
    pub condition: Option<ConditionSnapshot>,
}

fn json_of(vm: &VM, v: &Value) -> serde_json::Value {
    vm.stack_value_to_json(v, 0)
        .unwrap_or_else(|_| serde_json::Value::String(format!("{v:?}")))
}

/// Snapshot `vm`'s call stack and console — lazy in the sense that
/// nothing here is cached or streamed incrementally (Part F: "lazy and
/// fuel-metered... access fetches on demand"), but this module has no
/// host loop to fetch *through*; a real dispatcher would call this only
/// when a handler program's `vm.frames()`/`vm.locals()` is actually
/// evaluated, not eagerly on every raise. `suspension` is `None` when
/// snapshotting the innermost (running) frame of a
/// [`super::stack::ProgramStack`].
pub fn snapshot(vm: &VM, suspension: Option<&Suspension>) -> VmSnapshot {
    let frames = vm
        .frames()
        .into_iter()
        .map(|f| FrameSnapshot {
            function: f.name().to_owned(),
            locals: f
                .locals
                .iter()
                .enumerate()
                .map(|(i, v)| NamedValue {
                    name: f.local_name(i).map(str::to_owned),
                    value: json_of(vm, v),
                })
                .collect(),
        })
        .collect();
    VmSnapshot {
        frames,
        console: vm.console_lines.clone(),
        source: vm.source.to_string(),
        condition: suspension.map(|s| condition_of(vm, s)),
    }
}

fn condition_of(vm: &VM, suspension: &Suspension) -> ConditionSnapshot {
    match suspension {
        Suspension::Raised { condition, payload } => ConditionSnapshot::Raised {
            condition: condition.clone(),
            payload: payload.as_ref().map(|v| json_of(vm, v)),
        },
        Suspension::Trapped(e) => ConditionSnapshot::Trapped {
            kind: format!("{:?}", e.kind),
            message: e.message.clone(),
        },
        Suspension::Posted => ConditionSnapshot::Posted,
    }
}

/// The innermost frame's locals alone — `vm.locals()`'s shorthand for
/// the common case (Part F's list gives it equal billing with
/// `vm.frames()`, so a handler that only wants "what am I looking at
/// right now" need not walk the whole snapshot to get it).
pub fn innermost_locals(vm: &VM) -> Vec<NamedValue> {
    vm.frames()
        .last()
        .map(|f| {
            f.locals
                .iter()
                .enumerate()
                .map(|(i, v)| NamedValue {
                    name: f.local_name(i).map(str::to_owned),
                    value: json_of(vm, v),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use interp::compile;

    fn vm_for(source: &str) -> VM {
        let prog = compile(source).unwrap_or_else(|e| panic!("compile error: {e:?}"));
        VM::for_program(prog, serde_json::Value::Null).unwrap()
    }

    #[test]
    fn a_running_frame_has_no_condition() {
        let vm = vm_for("const x = 1;");
        let snap = snapshot(&vm, None);
        assert_eq!(snap.condition, None);
    }

    #[test]
    fn locals_are_named_when_debug_info_is_present() {
        // A function *parameter*, not a top-level `const`: this
        // compiler constant-folds/propagates literal `const`s
        // aggressively (`codegen_shape.rs`'s own
        // `const_propagation_respects_shadowing` test), so a literal
        // `const answer = 42` never occupies a real local slot to
        // attach a debug name to at all — a parameter's value depends
        // on the call and cannot be folded away.
        let mut vm = vm_for("(function f(answer) { raise('x'); return answer; })(42);");
        match vm.step(u64::MAX).unwrap() {
            interp::StepResult::Raise { .. } => {}
            other => panic!("expected Raise, got {other:?}"),
        }
        let snap = snapshot(&vm, None);
        // Two frames now: the module root, and `f`'s own call —
        // `answer` lives in the innermost (last), matching
        // `VM::frames()`'s own "outermost first" ordering.
        assert_eq!(snap.frames.len(), 2);
        let named = snap.frames[1]
            .locals
            .iter()
            .find(|nv| nv.name.as_deref() == Some("answer"));
        assert_eq!(
            named.map(|nv| nv.value.clone()),
            Some(serde_json::json!(42))
        );
    }

    #[test]
    fn console_lines_are_captured() {
        let mut vm = vm_for("console.log('hello'); raise('x');");
        vm.step(u64::MAX).unwrap();
        let snap = snapshot(&vm, None);
        assert!(snap.console.iter().any(|l| l.contains("hello")));
    }

    #[test]
    fn source_is_the_original_program_text() {
        let vm = vm_for("const x = 1;");
        let snap = snapshot(&vm, None);
        assert!(snap.source.contains("const x = 1;"));
    }

    #[test]
    fn a_raised_condition_carries_its_name_and_payload() {
        let mut vm = vm_for("raise('pick_a_number', 7);");
        let (condition, payload) = match vm.step(u64::MAX).unwrap() {
            interp::StepResult::Raise { condition, payload } => (condition, payload),
            other => panic!("expected Raise, got {other:?}"),
        };
        let susp = Suspension::Raised { condition, payload };
        let snap = snapshot(&vm, Some(&susp));
        assert_eq!(
            snap.condition,
            Some(ConditionSnapshot::Raised {
                condition: "pick_a_number".into(),
                payload: Some(serde_json::json!(7)),
            })
        );
    }

    #[test]
    fn a_trapped_condition_carries_its_kind_and_message() {
        let mut vm = vm_for("return [] - 1;");
        let err = match vm.step(u64::MAX) {
            Err(e) => e,
            other => panic!("expected error, got {other:?}"),
        };
        let snap = snapshot(&vm, Some(&Suspension::Trapped(err)));
        match snap.condition {
            Some(ConditionSnapshot::Trapped { kind, .. }) => assert_eq!(kind, "TypeError"),
            other => panic!("expected Trapped, got {other:?}"),
        }
    }

    #[test]
    fn a_posted_condition_snapshots_with_no_payload() {
        let vm = vm_for("const x = 1;");
        let snap = snapshot(&vm, Some(&Suspension::Posted));
        assert_eq!(snap.condition, Some(ConditionSnapshot::Posted));
    }

    #[test]
    fn innermost_locals_is_the_last_frames_locals() {
        let mut vm = vm_for("(function f(only) { raise('x'); return only; })(5);");
        match vm.step(u64::MAX).unwrap() {
            interp::StepResult::Raise { .. } => {}
            other => panic!("expected Raise, got {other:?}"),
        }
        let locals = innermost_locals(&vm);
        assert!(
            locals
                .iter()
                .any(|nv| nv.name.as_deref() == Some("only") && nv.value == serde_json::json!(5))
        );
    }

    #[test]
    fn frames_are_never_mutated_by_snapshotting() {
        // Read only, with no exceptions (Part F) — snapshotting twice
        // in a row must be idempotent and side-effect-free.
        let vm = vm_for("const x = 1;");
        assert_eq!(snapshot(&vm, None), snapshot(&vm, None));
    }
}
