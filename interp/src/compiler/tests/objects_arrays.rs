//! Object/array literals, member/index access, destructuring,
//! optional chaining, optional invocation, `in`/`delete`, and input pointer.

use super::*;
use crate::compiler::compile;
use crate::testutil;
use crate::testutil::{eval, eval_str};
use crate::vm::{StepResult, VM, Value};
use crate::rc_str::RcStr;

// ── arrays & objects ─────────────────────────────────────────────

#[test]
fn arrays_and_objects() {
    assert_eq!(eval("[10, 20, 30].length"), testutil::num(3.0));
    assert_eq!(eval("[10, 20, 30][1]"), Value::PosInt(20));
    assert_eq!(eval("[10, 20][5]"), Value::Undefined);
    assert_eq!(eval("({ a: 1, b: 2 }).b"), Value::PosInt(2));
    assert_eq!(eval("({ a: 1, b: 2 })[\"a\"]"), Value::PosInt(1));
    assert_eq!(eval("({ a: 1 }).missing"), Value::Undefined);
    assert_eq!(eval("({ 1: \"x\" })[1]"), eval("\"x\""));
}

#[test]
fn member_and_index_assignment() {
    let vm = testutil::run("input.obj = { a: 1 }; input.r = (input.obj.a = 9);");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(9));
    match input_val(&vm, "obj") {
        Value::Object(p) => {
            let o = &vm.objects[p as usize];
            assert_eq!(o.get(&RcStr::from("a")), Some(&Value::PosInt(9)))
        }
        other => panic!("{other:?}"),
    }
    let vm = testutil::run("input.arr = [1, 2, 3]; input.arr[0] = 99; input.r = input.arr[0];");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(99));
    let vm = testutil::run("input.o = {}; input.o[\"k\"] = 7; input.r = input.o.k;");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(7));
}

// ── optional chaining & invocation ───────────────────────────────

#[test]
fn optional_chaining() {
    assert_eq!(eval("({}).nope?.x"), Value::Undefined);
    assert_eq!(
        testutil::run_val("let obj = { x: 7 }; return obj?.x;"),
        Value::PosInt(7)
    );
    assert_eq!(eval("({}).nope?.a?.b"), Value::Undefined);
}

#[test]
fn optional_method_calls() {
    assert_eq!(
        testutil::run_ret("let arr = [1]; return arr?.push(2);"),
        serde_json::json!(2)
    );
    assert_eq!(
        testutil::run_ret("let arr = [1]; arr?.push(2); return arr.length;"),
        serde_json::json!(2)
    );
    assert_eq!(
        testutil::run_val("return ({}).nope?.push(2);"),
        Value::Undefined
    );
    // Short-circuit: RHS must not evaluate when target is nullish.
    assert_eq!(
        testutil::run_val("let hit = 0; ({}).nope?.push(hit = 1); return hit;"),
        Value::PosInt(0)
    );
    assert_eq!(
        testutil::run_ret("let s = \"a,b,c\"; return s?.split(\",\").length;"),
        serde_json::json!(3)
    );
}

#[test]
fn first_class_builtin_refs() {
    assert_eq!(eval_str("typeof Math.sqrt"), "function");
}

#[test]
fn optional_invocation_calls() {
    assert_eq!(eval("Math.max?.(3, 7)"), testutil::num(7.0));
    assert_eq!(eval("Math.sqrt?.(9)"), testutil::num(3.0));
    assert_eq!(
        testutil::run_val("let f = Math.sqrt; return f?.(16);"),
        testutil::num(4.0)
    );
    assert_eq!(eval("({}).nope?.()"), Value::Undefined);
    // Short-circuit: RHS must not evaluate when target is nullish.
    assert_eq!(
        testutil::run_val("let hit = 0; ({}).nope?.(hit = 1); return hit;"),
        Value::PosInt(0)
    );
    // Calling a non-function value is a runtime TypeError.
    let prog = compile("let x = 5; x?.();").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let err = loop {
        match vm.step() {
            Ok(StepResult::Done { .. }) => panic!("expected a runtime error"),
            Ok(_) => continue,
            Err(e) => break e,
        }
    };
    assert!(matches!(err, crate::vm::VMError::TypeError), "got: {err:?}");
}

// ── in / delete ──────────────────────────────────────────────────

#[test]
fn in_and_delete() {
    assert_eq!(
        testutil::run_ret("let o = { a: 1 }; return \"a\" in o;"),
        serde_json::json!(true)
    );
    assert_eq!(
        testutil::run_ret("let o = { a: 1 }; return \"b\" in o;"),
        serde_json::json!(false)
    );
    assert_eq!(
        testutil::run_ret("let o = { a: 1 }; let r = delete o.a; let had = \"a\" in o; return { r, had };"),
        serde_json::json!({"r": true, "had": false})
    );
}

// ── input pointer ────────────────────────────────────────────────

#[test]
fn input_is_ptr_zero() {
    let vm = testutil::run("input.a = 1; input.r = JSON.stringify(input);");
    match input_val(&vm, "r") {
        Value::String(s) => {
            assert!(s.as_str().contains("\"a\":1"), "got {}", s.as_str().to_owned())
        }
        other => panic!("{other:?}"),
    }
}
