//! Host-effect tests: `tools.*` (Invoke) and `raise` (Raise) —
//! both instruction-shape (lowering) and behavioral (end-to-end).

use super::*;
use crate::compiler::compile;
use crate::vm::{Instr, StepResult, VM, Value};

// ── Phase 4: effects (tools / raise) ────────────────────────────────

#[test]
fn tools_call_lowers_to_invoke() {
    // `tools.foo(a, b)` lowers to args-then-`Invoke("foo", 2)`.
    let prog = compile("tools.notify(1, 2);").expect("compiles");
    assert!(
        prog.code.contains(&Instr::Invoke("notify".into(), 2)),
        "expected Invoke in {:?}",
        prog.code
    );
}

#[test]
fn tools_call_yields_invoke_effect() {
    // End-to-end: a `tools.*` call yields an `Invoke` effect carrying the
    // method name and the evaluated args; the host pushes a result to resume.
    let prog = compile("input.r = tools.add(10, 3);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Invoke { calls } => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "add");
            assert_eq!(calls[0].args, vec![Value::PosInt(10), Value::PosInt(3)]);
        }
        other => panic!("expected Invoke, got {other:?}"),
    }
    // Host resolves the call and pushes the result; the program stores it.
    vm.stack.push(Value::PosInt(13));
    loop {
        match vm.step().unwrap() {
            StepResult::Done { .. } => break,
            other => panic!("unexpected effect: {other:?}"),
        }
    }
    assert_eq!(input_val(&vm, "r"), Value::PosInt(13));
}

#[test]
fn tools_call_with_no_args() {
    let prog = compile("tools.tick();").expect("compiles");
    assert!(prog.code.contains(&Instr::Invoke("tick".into(), 0)));
}

#[test]
fn raise_lowers_to_raise_instr() {
    let prog = compile("raise(\"need_input\");").expect("compiles");
    assert!(
        prog.code.contains(&Instr::Raise("need_input".into())),
        "expected Raise in {:?}",
        prog.code
    );
}

#[test]
fn raise_yields_effect_and_resumes_as_expression() {
    // `raise(...)` is an expression: it yields a `Raise` effect, then the
    // host pushes the resumed value which the program consumes.
    let prog = compile("input.r = raise(\"pick_a_number\");").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Raise { condition } => assert_eq!(condition, "pick_a_number"),
        other => panic!("expected Raise, got {other:?}"),
    }
    // Resume restart: advance past the Raise and push the resumed value.
    vm.ip += 1;
    vm.stack.push(Value::PosInt(42));
    loop {
        match vm.step().unwrap() {
            StepResult::Done { .. } => break,
            other => panic!("unexpected effect: {other:?}"),
        }
    }
    assert_eq!(input_val(&vm, "r"), Value::PosInt(42));
}
