//! Shared test harness for compiler + VM integration tests.
//!
//! Consolidates helpers that were duplicated across `compiler/tests.rs`,
//! `vm/tests.rs`, and `builtin/mod.rs`.

use crate::Instr;
use crate::compiler::{Program, compile};
use crate::vm::{ErrorKind, StepResult, VM, VMError, Value};

// ── compilation ──────────────────────────────────────────────────────────

/// Compile `src` to a `Program`, panicking with rendered diagnostics on
/// failure.
pub fn compile_ok(src: &str) -> Program {
    match compile(src) {
        Ok(prog) => prog,
        Err(errs) => {
            let rendered: Vec<String> = errs.iter().map(|d| d.render(src)).collect();
            panic!("compile failed:\n{}", rendered.join("\n"));
        }
    }
}

/// Compile `src` and return the rendered diagnostic strings on failure.
pub fn compile_errs(src: &str) -> Vec<String> {
    match compile(src) {
        Ok(_) => panic!("expected compile failure, got success"),
        Err(errs) => errs.iter().map(|d| d.render(src)).collect(),
    }
}

// ── execution ────────────────────────────────────────────────────────────

pub fn run_instrs(code: Vec<Instr>) -> Vec<Value> {
    let mut vm = VM::new(code);
    match vm.step().unwrap() {
        StepResult::Done { .. } => return vm.stack.clone(),
        other => panic!("unexpected effect: {other:?}"),
    }
}

/// Compile + run to `Done`, returning the finished VM. Panics on any
/// effect or runtime error.
pub fn run_vm(src: &str) -> VM {
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step().unwrap() {
            StepResult::Done { .. } => return vm,
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}

/// Compile + run to `Done`, returning the program's top-level return
/// value serialized as JSON. This is the primary workhorse for end-to-end
/// tests: `assert_eq!(run_ret("return 1 + 2;"), json!(3))`.
pub fn run_ret(src: &str) -> serde_json::Value {
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step().unwrap() {
            StepResult::Done { value } => {
                return vm.stack_value_to_json(&value, 0).expect("value to JSON");
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}

/// Compile + run to `Done`, returning the program's top-level return
/// value directly (no JSON conversion). Use for tests that need to match
/// exact `Value` variants (e.g. `PosInt`, `Float`).
pub fn run_val(src: &str) -> Value {
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step().unwrap() {
            StepResult::Done { value } => return value,
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}

/// Compile + run until the first `Invoke` or `Raise` effect. Returns the
/// paused VM and the effect. Panics on `Done` or runtime error.
pub fn run_to_effect(src: &str) -> (VM, StepResult) {
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step().unwrap() {
            StepResult::Done { .. } => panic!("unexpected completion"),
            effect @ (StepResult::Invoke { .. } | StepResult::Raise { .. }) => {
                return (vm, effect);
            }
        }
    }
}

/// Compile + run until a runtime `VMError` occurs. Panics on `Done` or
/// any effect.
pub fn run_runtime_err(src: &str) -> VMError {
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step() {
            Err(e) => return e,
            Ok(StepResult::Done { .. }) => panic!("expected runtime error, got Done"),
            Ok(other) => panic!("expected runtime error, got effect: {other:?}"),
        }
    }
}

/// Compile + run until a runtime error occurs; return only the `ErrorKind`.
/// Sugar for tests that don't need the full `VMError`.
pub fn run_err_kind(src: &str) -> ErrorKind {
    run_runtime_err(src).kind
}

// ── value constructors ───────────────────────────────────────────────────

/// Shorthand for `Value::Float(f64)`. Used in assertions: `num(7.0)`.
pub fn num(v: f64) -> Value {
    Value::Float(v)
}

// ── expression eval helpers ──────────────────────────────────────────────

/// Evaluate a single expression by compiling `return (<expr>);` and
/// returning the top-level `Value`. Convenience wrapper around `run_val`.
pub fn eval(expr: &str) -> Value {
    run_val(&format!("return ({expr});"))
}

/// Like [`eval`], but resolves the result to an owned `String`.
pub fn eval_str(expr: &str) -> String {
    match run_val(&format!("return ({expr});")) {
        Value::String(s) => s.as_str().to_owned(),
        other => panic!("not a string: {other:?}"),
    }
}

// ── self-tests (harness API coverage) ──────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::StepResult;

    #[test]
    fn compile_errs_renders_diagnostics() {
        let errs = compile_errs("let x = y;"); // undeclared `y`
        assert!(!errs.is_empty(), "expected at least one diagnostic");
        assert!(
            errs[0].contains("undeclared") || errs[0].contains("y"),
            "got: {}",
            errs[0]
        );
    }

    #[test]
    fn run_to_effect_yields_invoke() {
        let (_vm, effect) = run_to_effect("tools.foo(1);");
        match effect {
            StepResult::Invoke { calls } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "foo");
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
    }

    #[test]
    fn run_runtime_err_catches_type_error() {
        // Accessing a property on a non-object is a runtime TypeError.
        let err = run_runtime_err("return null.foo;");
        assert!(err.kind == ErrorKind::TypeError, "got {err:?}");
        // The kind-only sugar agrees.
        assert_eq!(run_err_kind("return null.foo;"), ErrorKind::TypeError);
    }
}
