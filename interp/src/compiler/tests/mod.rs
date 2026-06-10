use super::*;
use crate::vm::{StepResult, VM};

pub mod codegen_shape;
pub mod control_flow;
pub mod coverage;
pub mod diagnostics;
pub mod effects;
pub mod functions_closures;
pub mod hof;
pub mod lang_basics;
pub mod objects_arrays;
pub mod perf_allocs;

/// Run a compiled program to completion via `for_program`, returning the
/// finished VM so the heap/stack can be inspected.
pub(super) fn run_program(prog: Program) -> VM {
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step().unwrap() {
            StepResult::Done { .. } => return vm,
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}
// ── Phase 1: expressions ───────────────────────────────────────────
//
// Most expression behavior is exercised end-to-end: compile a program that
// writes its result into `input.r`, run it, then read `objects[0]["r"]`. This
// routes every expression through the real VM and the `input`/`Object(0)`
// lowering at once.

/// Read `input.<key>` (a slot of the objects[0] input object) from a finished VM.
pub(super) fn input_val(vm: &VM, key: &str) -> Value {
    vm.objects[0]
        .get(&RcStr::from(key))
        .cloned()
        .unwrap_or_else(|| panic!("no input.{key}"))
}

