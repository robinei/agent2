//! Targeted coverage-gap tests (Phase 2 Step 3).
//!
//! Each test targets an area with previously thin or missing coverage.
//! See `docs/2_TESTS.md` Step 3 for the full list.

use super::*;
use crate::compiler::compile;
use crate::testutil;
use crate::vm::{Instr, StepResult, VM, VMError, Value};

// ── top-level return semantics ──────────────────────────────────────────

#[test]
fn top_level_return_void_yields_undefined() {
    assert_eq!(testutil::run_val("return;"), Value::Undefined);
}

#[test]
fn top_level_return_object_values() {
    // Arbitrary JSON-able values round-trip through `Done { value }`.
    assert_eq!(
        testutil::run_ret("return { a: 1, b: \"hi\" };"),
        serde_json::json!({"a": 1, "b": "hi"})
    );
    assert_eq!(testutil::run_ret("return [1, 2, 3];"), serde_json::json!([1, 2, 3]));
    assert_eq!(testutil::run_ret("return true;"), serde_json::json!(true));
    assert_eq!(testutil::run_ret("return null;"), serde_json::json!(null));
}

// ── host-seeded `input` binding ─────────────────────────────────────────

#[test]
fn for_program_seeds_input_object() {
    // `testutil::run_ret` uses `Null` seed; test manual `for_program` seeding.
    let prog = crate::testutil::compile_ok("return input.x + input.y;");
    let mut vm =
        VM::for_program(prog, serde_json::json!({"x": 10, "y": 20})).unwrap();
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

// ── string edge cases ───────────────────────────────────────────────────

#[test]
fn string_mid_codepoint_index_errors() {
    // Indexing into the middle of a multi-byte UTF-8 codepoint is a
    // ValueError (not a panic).
    let mut vm = VM::new(vec![
        Instr::PushStr("é".into()), // 2-byte UTF-8
        Instr::PushPosInt(1),       // middle of codepoint
        Instr::IndexGet,
    ]);
    match vm.step() {
        Err(VMError::ValueError) => {} // expected
        other => panic!("expected ValueError, got {other:?}"),
    }
}

#[test]
fn string_empty_needle_index_of() {
    // Empty-string needle: `indexOf` returns 0, `includes` returns true,
    // `startsWith` returns true (matching JS).
    let mut vm = VM::new(vec![
        Instr::PushStr("hello".into()),
        Instr::PushStr("".into()),
        Instr::CallBuiltin(crate::builtin::Builtin::StrIndexOf, 2),
    ]);
    loop {
        match vm.step().unwrap() {
            StepResult::Done { .. } => break,
            other => panic!("unexpected effect: {other:?}"),
        }
    }
    assert_eq!(vm.stack.last(), Some(&Value::Float(0.0)));
}

// ── control-flow corners ────────────────────────────────────────────────

#[test]
fn continue_in_do_while_retests_condition() {
    // `continue` inside `do-while` jumps to the condition, which re-tests.
    let vm = testutil::run(
        "let i = 0; let s = 0; \
             do { i++; if (i < 3) continue; s++; } while (i < 5); \
             input.r_i = i; input.r_s = s;",
    );
    assert_eq!(input_val(&vm, "r_i"), Value::Float(5.0));
    // i=1,2: continue (skip s++). i=3,4,5: s++ → s=3.
    assert_eq!(input_val(&vm, "r_s"), Value::Float(3.0));
}

#[test]
fn labeled_statement_is_rejected() {
    let errs = compile("outer: while (true) break outer;").expect_err("should fail");
    let msg = &errs[0].message;
    assert!(
        msg.contains("labeled"),
        "expected 'labeled' in message, got: {msg}"
    );
}

#[test]
fn switch_break_only_continue_skips_to_enclosing_loop() {
    // `continue` inside a `switch` (no loop on the switch itself) must
    // skip to the innermost enclosing loop, not error.
    let vm = testutil::run(
        "let s = 0; \
             for (let i = 1; i <= 3; i++) { \
               switch (i) { \
                 case 1: s++; break; \
                 case 2: continue; \
                 default: s += 10; \
               } \
             } \
             input.r = s;",
    );
    // i=1: s=1, break. i=2: continue → skip increment. i=3: s=11.
    assert_eq!(input_val(&vm, "r"), Value::Float(11.0));
}

// ── resource guards ─────────────────────────────────────────────────────

#[test]
fn out_of_fuel_stops_infinite_loop() {
    let mut vm = VM::new(vec![
        Instr::Jump(0), // infinite loop
    ]);
    vm.fuel = 5; // tiny budget
    let err = loop {
        match vm.step() {
            Err(e) => break e,
            Ok(StepResult::Done { .. }) => panic!("unexpected completion"),
            Ok(_) => {}
        }
    };
    assert!(matches!(err, VMError::OutOfFuel));
}

// ── testutil API coverage ───────────────────────────────────────────────

#[test]
fn compile_errs_renders_diagnostics() {
    let errs = testutil::compile_errs("let x = y;"); // undeclared `y`
    assert!(!errs.is_empty(), "expected at least one diagnostic");
    assert!(
        errs[0].contains("undeclared") || errs[0].contains("y"),
        "got: {}",
        errs[0]
    );
}

#[test]
fn run_to_effect_yields_invoke() {
    let (_vm, effect) = testutil::run_to_effect("tools.foo(1);");
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
    let err = testutil::run_runtime_err("return null.foo;");
    assert!(matches!(err, VMError::TypeError), "got {err:?}");
}
