//! Object/array literals, member/index access, destructuring,
//! optional chaining, optional invocation, `in`/`delete`, and input pointer.

use super::*;
use crate::compiler::compile;
use crate::testutil;
use crate::testutil::{eval, eval_str};
use crate::vm::{Instr, StepResult, VM, Value};
use crate::builtin::Builtin;
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
    assert_eq!(eval("input.nope?.x"), Value::Undefined);
    let vm = testutil::run("input.obj = { x: 7 }; input.r = input.obj?.x;");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(7));
    assert_eq!(eval("input.nope?.a?.b"), Value::Undefined);
}

#[test]
fn optional_method_calls() {
    let vm = testutil::run("input.arr = [1]; input.r = input.arr?.push(2);");
    assert_eq!(input_val(&vm, "r"), testutil::num(2.0));
    let vm = testutil::run("input.arr = [1]; input.arr?.push(2); input.r = input.arr.length;");
    assert_eq!(input_val(&vm, "r"), testutil::num(2.0));
    let vm = testutil::run("input.r = input.nope?.push(2);");
    assert_eq!(input_val(&vm, "r"), Value::Undefined);
    let vm = testutil::run("input.hit = 0; input.r = input.nope?.push(input.hit = 1);");
    assert_eq!(input_val(&vm, "r"), Value::Undefined);
    assert_eq!(input_val(&vm, "hit"), Value::PosInt(0));
    let vm = testutil::run("input.s = \"a,b,c\"; input.r = input.s?.split(\",\").length;");
    assert_eq!(input_val(&vm, "r"), testutil::num(3.0));
}

#[test]
fn first_class_builtin_refs() {
    assert_eq!(eval_str("typeof Math.sqrt"), "function");
}

#[test]
fn optional_invocation_calls() {
    assert_eq!(eval("Math.max?.(3, 7)"), testutil::num(7.0));
    assert_eq!(eval("Math.sqrt?.(9)"), testutil::num(3.0));
    let vm = testutil::run("input.f = Math.sqrt; input.r = input.f?.(16);");
    assert_eq!(input_val(&vm, "r"), testutil::num(4.0));
    assert_eq!(eval("input.nope?.()"), Value::Undefined);
    let vm = testutil::run("input.hit = 0; input.r = input.nope?.(input.hit = 1);");
    assert_eq!(input_val(&vm, "r"), Value::Undefined);
    assert_eq!(input_val(&vm, "hit"), Value::PosInt(0));
    let prog = compile("input.x = 5; input.x?.();").expect("compiles");
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

#[test]
fn optional_call_reclaims_static_builtin() {
    let prog = compile("Math.max?.(3, 7);").expect("compiles");
    assert!(
        prog.code.iter().any(|i| matches!(i, Instr::CallBuiltin(Builtin::MathMax, 2))),
        "expected CallBuiltin(MathMax, 2), got {:?}", prog.code
    );
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::CallDyn(_) | Instr::JNotNullish(_))),
        "guard/CallDyn should have been reclaimed: {:?}", prog.code
    );
    assert_eq!(eval("Math.max?.(3, 7)"), testutil::num(7.0));
}

// ── in / delete ──────────────────────────────────────────────────

#[test]
fn in_and_delete() {
    let vm = testutil::run("input.o = { a: 1 }; input.r = (\"a\" in input.o);");
    assert_eq!(input_val(&vm, "r"), Value::Bool(true));
    let vm = testutil::run("input.o = { a: 1 }; input.r = (\"b\" in input.o);");
    assert_eq!(input_val(&vm, "r"), Value::Bool(false));
    let vm = testutil::run("input.o = { a: 1 }; input.r = delete input.o.a; input.had = (\"a\" in input.o);");
    assert_eq!(input_val(&vm, "r"), Value::Bool(true));
    assert_eq!(input_val(&vm, "had"), Value::Bool(false));
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
