//! Host-effect tests: `tools.*` (Invoke) and `raise` (Raise) —
//! behavioral (end-to-end) tests. Lowering/shape tests live in
//! `codegen_shape.rs`.

use crate::compiler::compile;
use crate::vm::{StepResult, VM, Value};

// ── effects (tools / raise) ───────────────────────────────────────

#[test]
fn awaited_tools_call_yields_pending_effect() {
    // End-to-end: an awaited `tools.*` call yields a `Pending` effect
    // carrying the method name and evaluated args; the host settles the
    // call's promise to resume.
    let prog = compile("return await tools.add(10, 3);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let id = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "add");
            assert_eq!(calls[0].args, vec![Value::PosInt(10), Value::PosInt(3)]);
            calls[0].promise
        }
        other => panic!("expected Pending, got {other:?}"),
    };
    // Host resolves the call; the re-executed await returns its value.
    vm.resolve_promise(id, Value::PosInt(13)).unwrap();
    loop {
        match vm.step(u64::MAX).unwrap() {
            StepResult::Done { value, .. } => {
                assert_eq!(value, Value::PosInt(13));
                break;
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}

#[test]
fn pending_round_trips_under_single_stepping() {
    // A `Pending` yield in the middle of `step(1)` slices: the host
    // resolves the call and single-stepping continues to completion.
    let prog = compile("const x = await tools.f(1); return x + 1;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step(1).unwrap() {
            StepResult::OutOfFuel => {}
            StepResult::Pending { calls } => {
                for call in calls {
                    vm.resolve_promise(call.promise, Value::PosInt(41)).unwrap();
                }
            }
            StepResult::Done { value, .. } => {
                assert_eq!(value, Value::Float(42.0));
                break;
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}

#[test]
fn unawaited_tools_call_is_fire_and_forget() {
    // Without an await, the program runs to completion and the started call
    // is reported in `Done::unstarted` (host decides whether to run it).
    let prog = compile("tools.log(\"hi\"); return 1;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, unstarted } => {
            assert_eq!(value, Value::PosInt(1));
            assert_eq!(unstarted.len(), 1);
            assert_eq!(unstarted[0].name, "log");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn raise_yields_effect_and_resumes_as_expression() {
    // `raise(...)` is an expression: it yields a `Raise` effect, then the
    // host pushes the resumed value which the program returns.
    let prog = compile("return raise(\"pick_a_number\");").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Raise { condition, payload } => {
            assert_eq!(condition, "pick_a_number");
            assert!(payload.is_none(), "no payload for raise(\"name\")");
        }
        other => panic!("expected Raise, got {other:?}"),
    }
    // Resume via resume_raise (ip already advanced by step()).
    vm.resume_raise(Value::PosInt(42));
    loop {
        match vm.step(u64::MAX).unwrap() {
            StepResult::Done { value, .. } => {
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
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::Float(30.0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn input_with_null_seed_is_empty_object() {
    // `Null` seed (or missing) yields an empty input object.
    let prog = compile("return Object.keys(input).length;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::Float(0.0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

// ── Step 5: raise payload and resume_raise ───────────────────────

#[test]
fn raise_with_payload_roundtrip() {
    // `raise("name", expr)` passes the payload in StepResult::Raise.
    let prog = compile("raise(\"err\", 42);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
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
    match vm.step(u64::MAX).unwrap() {
        StepResult::Raise { condition, payload } => {
            assert_eq!(condition, "question");
            assert!(payload.is_none());
        }
        other => panic!("expected Raise, got {other:?}"),
    }
    vm.resume_raise(Value::String("answer".into()));
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => {
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
