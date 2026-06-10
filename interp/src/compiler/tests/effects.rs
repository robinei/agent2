//! Host-effect tests: `tools.*` (Invoke) and `raise` (Raise) —
//! behavioral (end-to-end) tests. Lowering/shape tests live in
//! `codegen_shape.rs`.

use crate::compiler::compile;
use crate::vm::{StepResult, VM, Value};

// ── effects (tools / raise) ───────────────────────────────────────

#[test]
fn tools_call_yields_invoke_effect() {
    // End-to-end: a `tools.*` call yields an `Invoke` effect carrying the
    // method name and the evaluated args; the host pushes a result to resume.
    let prog = compile("return tools.add(10, 3);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Invoke { calls } => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "add");
            assert_eq!(calls[0].args, vec![Value::PosInt(10), Value::PosInt(3)]);
        }
        other => panic!("expected Invoke, got {other:?}"),
    }
    // Host resolves the call and pushes the result; the program returns it.
    vm.stack.push(Value::PosInt(13));
    loop {
        match vm.step().unwrap() {
            StepResult::Done { value } => {
                assert_eq!(value, Value::PosInt(13));
                break;
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}

#[test]
fn raise_yields_effect_and_resumes_as_expression() {
    // `raise(...)` is an expression: it yields a `Raise` effect, then the
    // host pushes the resumed value which the program returns.
    let prog = compile("return raise(\"pick_a_number\");").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Raise { condition, payload } => {
            assert_eq!(condition, "pick_a_number");
            assert!(payload.is_none(), "no payload for raise(\"name\")");
        }
        other => panic!("expected Raise, got {other:?}"),
    }
    // Resume via resume_raise (ip already advanced by step()).
    vm.resume_raise(Value::PosInt(42));
    loop {
        match vm.step().unwrap() {
            StepResult::Done { value } => {
                assert_eq!(value, Value::PosInt(42));
                break;
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}

// ── host-seeded `input` binding ─────────────────────────────────────

#[test]
fn for_program_seeds_input_object() {
    // `testutil::run_ret` uses `Null` seed; test manual `for_program` seeding.
    let prog = crate::testutil::compile_ok("return input.x + input.y;");
    let mut vm = VM::for_program(prog, serde_json::json!({"x": 10, "y": 20})).unwrap();
    match vm.step().unwrap() {
        StepResult::Done { value } => assert_eq!(value, Value::Float(30.0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn input_with_null_seed_is_empty_object() {
    // `Null` seed (or missing) yields an empty input object.
    let prog = compile("return Object.keys(input).length;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Done { value } => assert_eq!(value, Value::Float(0.0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

// ── Step 5: raise payload and resume_raise ───────────────────────

#[test]
fn raise_with_payload_roundtrip() {
    // `raise("name", expr)` passes the payload in StepResult::Raise.
    let prog = compile("raise(\"err\", 42);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Raise { condition, payload } => {
            assert_eq!(condition, "err");
            assert_eq!(payload, Some(Value::PosInt(42)));
        }
        other => panic!("expected Raise, got {other:?}"),
    }
}

#[test]
fn raise_no_payload_resume_raise() {
    // `raise("name")` → no payload, resume_raise feeds the result value.
    let prog = compile("return raise(\"question\");").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Raise { condition, payload } => {
            assert_eq!(condition, "question");
            assert!(payload.is_none());
        }
        other => panic!("expected Raise, got {other:?}"),
    }
    vm.resume_raise(Value::String("answer".into()));
    match vm.step().unwrap() {
        StepResult::Done { value } => {
            assert_eq!(value, Value::String("answer".into()));
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn raise_too_many_args_is_compile_error() {
    let errs = crate::testutil::compile_errs("raise(\"x\", 1, 2);");
    assert!(!errs.is_empty(), "should be a compile error");
}

#[test]
fn raise_non_literal_name_is_compile_error() {
    let errs = crate::testutil::compile_errs("let n = \"x\"; raise(n);");
    assert!(!errs.is_empty(), "should be a compile error");
}
