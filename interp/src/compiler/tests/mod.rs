use super::*;
use crate::vm::{StepResult, VM};

pub mod async_await;
pub mod codegen_shape;
pub mod control_flow;
pub mod diagnostics;
pub mod effects;
pub mod exceptions;
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
// ── Legacy helpers (kept for input-subject tests only) ────────────────
//
// Most tests now use `testutil::run_ret` / `run_val` + top-level `return`.
// These helpers remain for the deliberate input-lowering tests that must
// continue exercising the input-object path (e.g. `input_is_ptr_zero`,
// `for_in_over_input`, member-assignment-through-`input`).

/// Read `input.<key>` (a slot of the objects[0] input object) from a finished VM.
pub(super) fn input_val(vm: &VM, key: &str) -> Value {
    vm.objects[0]
        .get(&RcStr::from(key))
        .cloned()
        .unwrap_or_else(|| panic!("no input.{key}"))
}
