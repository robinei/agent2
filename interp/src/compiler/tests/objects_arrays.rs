//! Object/array literals, member/index access, destructuring,
//! optional chaining, optional invocation, `in`/`delete`, and input pointer.

use super::*;
use crate::compiler::compile;
use crate::rc_str::RcStr;
use crate::testutil;
use crate::testutil::{eval, eval_str};
use crate::vm::{StepResult, VM, Value};

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
    let vm = testutil::run_vm("input.obj = { a: 1 }; input.r = (input.obj.a = 9);");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(9));
    match input_val(&vm, "obj") {
        Value::Object(p) => {
            let o = &vm.objects[p as usize];
            assert_eq!(o.get(&RcStr::from("a")), Some(&Value::PosInt(9)))
        }
        other => panic!("{other:?}"),
    }
    let vm = testutil::run_vm("input.arr = [1, 2, 3]; input.arr[0] = 99; input.r = input.arr[0];");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(99));
    let vm = testutil::run_vm("input.o = {}; input.o[\"k\"] = 7; input.r = input.o.k;");
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
    assert!(err.kind == crate::vm::ErrorKind::TypeError, "got: {err:?}");
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
        testutil::run_ret(
            "let o = { a: 1 }; let r = delete o.a; let had = \"a\" in o; return { r, had };"
        ),
        serde_json::json!({"r": true, "had": false})
    );
}

// ── spread ────────────────────────────────────────────────────────

#[test]
fn array_spread() {
    assert_eq!(eval("[...[1, 2, 3]].length"), testutil::num(3.0));
    assert_eq!(
        testutil::run_val("let a = [1, 2]; return [...a, 3, 4].length;"),
        testutil::num(4.0)
    );
    assert_eq!(
        testutil::run_val("let a = [1, 2]; return [0, ...a].length;"),
        testutil::num(3.0)
    );
    assert_eq!(
        testutil::run_val("let a = [1]; let b = [2]; return [...a, ...b, 3].length;"),
        testutil::num(3.0)
    );
}

#[test]
fn array_spread_element_values() {
    assert_eq!(
        testutil::run_ret("let a = [10, 20]; return [...a, 30][2];"),
        serde_json::json!(30)
    );
    assert_eq!(
        testutil::run_ret("let a = [10, 20]; return [0, ...a][1];"),
        serde_json::json!(10)
    );
    assert_eq!(
        testutil::run_ret("let a = [1, 2]; return [...a, 3, 4][3];"),
        serde_json::json!(4)
    );
    // Empty spread.
    assert_eq!(
        testutil::run_ret("let a = []; return [...a, 99].length;"),
        serde_json::json!(1)
    );
}

#[test]
fn object_spread() {
    assert_eq!(
        testutil::run_ret("let a = { x: 1 }; return { ...a }.x;"),
        serde_json::json!(1)
    );
    assert_eq!(
        testutil::run_ret("let a = { x: 1 }; return { ...a, y: 2 }.y;"),
        serde_json::json!(2)
    );
    assert_eq!(
        testutil::run_ret("let a = { x: 1 }; return { y: 2, ...a }.x;"),
        serde_json::json!(1)
    );
}

#[test]
fn object_spread_later_wins() {
    // Later field overwrites earlier.
    assert_eq!(
        testutil::run_ret("let a = { x: 1 }; return { ...a, x: 99 }.x;"),
        serde_json::json!(99)
    );
    assert_eq!(
        testutil::run_ret("let a = { x: 99 }; return { x: 1, ...a }.x;"),
        serde_json::json!(99)
    );
}

#[test]
fn object_spread_null_undefined() {
    // null/undefined spread source is a no-op.
    assert_eq!(
        testutil::run_ret("let a = null; return { ...a, x: 1 }.x;"),
        serde_json::json!(1)
    );
    assert_eq!(
        testutil::run_ret("let a = undefined; return { ...a, x: 2 }.x;"),
        serde_json::json!(2)
    );
}

#[test]
fn object_spread_multiple() {
    assert_eq!(
        testutil::run_ret(
            "let a = { x: 1 }; let b = { y: 2 }; return { ...a, ...b, z: 3 }.z;"
        ),
        serde_json::json!(3)
    );
    assert_eq!(
        testutil::run_ret(
            "let a = { x: 1 }; let b = { y: 2 }; return { ...a, ...b }.y;"
        ),
        serde_json::json!(2)
    );
}

/// Spread of a non-array / non-object value is a TypeError (documented
/// divergence: JS spreads any iterable in arrays — including strings — and
/// copies index keys from arrays/strings in objects).
#[test]
fn spread_non_container_errors() {
    use crate::vm::ErrorKind;
    // Array spread: strings and numbers are not spreadable.
    assert!(matches!(
        testutil::run_err_kind("let a = [...\"ab\"]; return a;"),
        ErrorKind::TypeError
    ));
    assert!(matches!(
        testutil::run_err_kind("let n = 5; let a = [1, ...n]; return a;"),
        ErrorKind::TypeError
    ));
    // Object spread: arrays, strings, and numbers are not spreadable
    // (null/undefined are no-ops, tested separately).
    assert!(matches!(
        testutil::run_err_kind("let o = { ...\"ab\" }; return o;"),
        ErrorKind::TypeError
    ));
    assert!(matches!(
        testutil::run_err_kind("let o = { ...[1, 2] }; return o;"),
        ErrorKind::TypeError
    ));
    assert!(matches!(
        testutil::run_err_kind("let o = { ...5 }; return o;"),
        ErrorKind::TypeError
    ));
}

/// Plain literals must compile byte-for-byte unchanged: the no-spread fast
/// path emits exactly the pre-spread instruction sequence, with no
/// ArrExtend/ArrPush/ObjExtend.
#[test]
fn no_spread_plain_literals_compile_unchanged() {
    use crate::vm::Instr;
    let prog = compile("return [1, 2, 3];").expect("compiles");
    assert_eq!(
        prog.code,
        vec![
            Instr::PushPosInt(1),
            Instr::PushPosInt(2),
            Instr::PushPosInt(3),
            Instr::ArrNew(3),
            Instr::Return(1),
        ]
    );

    let prog = compile("return { a: 1, b: 2 };").expect("compiles");
    assert_eq!(
        prog.code,
        vec![
            Instr::PushPosInt(1),
            Instr::PushPosInt(2),
            Instr::ObjNew(vec!["a".into(), "b".into()].into()),
            Instr::Return(1),
        ]
    );
}

// ── spread calls ─────────────────────────────────────────────────────

#[test]
fn call_spread_builtin() {
    assert_eq!(
        testutil::run_ret("return Math.max(...[3, 7]);"),
        serde_json::json!(7)
    );
    assert_eq!(
        testutil::run_ret("return Math.max(...[10, 2, 8]);"),
        serde_json::json!(10)
    );
}

#[test]
fn call_spread_user_fn() {
    assert_eq!(
        testutil::run_ret("function f(a, b) { return a - b; } return f(...[10, 3]);"),
        serde_json::json!(7)
    );
}

#[test]
fn call_spread_mixed_args() {
    assert_eq!(
        testutil::run_ret("function f(a, b, c) { return a + b + c; } return f(1, ...[2], 3);"),
        serde_json::json!(6)
    );
}

#[test]
fn call_spread_closure() {
    assert_eq!(
        testutil::run_ret("let f = (a, b) => a * b; return f(...[5, 6]);"),
        serde_json::json!(30)
    );
}

#[test]
fn call_spread_optional() {
    // Non-nullish callee with optional spread call.
    assert_eq!(
        testutil::run_ret("let f = (a, b) => a + b; return f?.(...[3, 4]);"),
        serde_json::json!(7)
    );
    // Nullish callee short-circuits to undefined.
    assert_eq!(
        testutil::run_val("return ({}).nope?.(...[1, 2]);"),
        Value::Undefined
    );
    // Short-circuit: RHS must not evaluate when target is nullish.
    assert_eq!(
        testutil::run_val("let hit = 0; ({}).nope?.(...[hit = 1]); return hit;"),
        Value::PosInt(0)
    );
}

/// Spread on a tool call is a targeted compile error, not a misleading
/// undeclared-variable error (`Invoke` has a static arg count).
#[test]
fn call_spread_on_tools_is_rejected() {
    let errs = testutil::compile_errs("let a = [1]; tools.foo(...a);");
    assert!(
        errs.iter().any(|e| e.contains("spread arguments are not supported on tool calls")),
        "expected targeted tool-call spread error, got: {errs:?}"
    );
}

/// Plain calls (no spread) must not emit CallSpread.
#[test]
fn no_spread_plain_calls_have_no_call_spread() {
    use crate::Instr;
    let prog = crate::compile("return Math.max(1, 2);").expect("compiles");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::CallSpread)),
        "no-spread call emitted CallSpread"
    );
}

// ── computed object keys (A3) ────────────────────────────────────

#[test]
fn computed_object_key_basic() {
    assert_eq!(eval("({ [\"a\"]: 1 }).a"), Value::PosInt(1));
    assert_eq!(
        testutil::run_val("let k = \"b\"; return ({ [k]: 2 }).b;"),
        Value::PosInt(2)
    );
    assert_eq!(
        testutil::run_val("let k = \"x\"; return ({ [k]: 42 })[k];"),
        Value::PosInt(42)
    );
}

#[test]
fn computed_object_key_mixed_with_static() {
    assert_eq!(
        testutil::run_ret("let k = \"y\"; return JSON.stringify({ a: 1, [k]: 2, b: 3 });"),
        serde_json::json!("{\"a\":1,\"y\":2,\"b\":3}")
    );
    let vm = testutil::run_vm(
        "let k = \"k\"; input.obj = { [k]: 10, static: 20 }; input.r = input.obj.k;"
    );
    assert_eq!(input_val(&vm, "r"), Value::PosInt(10));
}

#[test]
fn computed_object_key_with_spread() {
    let vm = testutil::run_vm(
        "let k = \"c\"; let base = { a: 1, b: 2 }; input.obj = { ...base, [k]: 3 }; input.r = JSON.stringify(input.obj);"
    );
    match input_val(&vm, "r") {
        Value::String(s) => {
            let json = s.as_str();
            assert!(json.contains("\"a\":1"), "got {json}");
            assert!(json.contains("\"b\":2"), "got {json}");
            assert!(json.contains("\"c\":3"), "got {json}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn computed_object_key_expression() {
    assert_eq!(
        eval("({ [1 + 2]: \"three\" })[\"3\"]"),
        eval("\"three\"")
    );
    assert_eq!(
        eval("({ [\"hello \" + \"world\"]: true })[\"hello world\"]"),
        Value::Bool(true)
    );
}

// ── input pointer ────────────────────────────────────────────────

#[test]
fn input_is_ptr_zero() {
    let vm = testutil::run_vm("input.a = 1; input.r = JSON.stringify(input);");
    match input_val(&vm, "r") {
        Value::String(s) => {
            assert!(
                s.as_str().contains("\"a\":1"),
                "got {}",
                s.as_str().to_owned()
            )
        }
        other => panic!("{other:?}"),
    }
}
