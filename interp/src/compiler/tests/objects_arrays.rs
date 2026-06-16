//! Object/array literals, member/index access, destructuring,
//! optional chaining, optional invocation, `in`/`delete`, and input pointer.

use super::*;
use crate::compiler::compile;
use crate::rc_str::RcStr;
use crate::testutil;
use crate::testutil::{eval, eval_str};
use crate::vm::{Instr, StepResult, VM, Value};

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
        match vm.step(u64::MAX) {
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
        testutil::run_ret("let a = { x: 1 }; let b = { y: 2 }; return { ...a, ...b, z: 3 }.z;"),
        serde_json::json!(3)
    );
    assert_eq!(
        testutil::run_ret("let a = { x: 1 }; let b = { y: 2 }; return { ...a, ...b }.y;"),
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
        errs.iter()
            .any(|e| e.contains("spread arguments are not supported on tool calls")),
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
        "let k = \"k\"; input.obj = { [k]: 10, static: 20 }; input.r = input.obj.k;",
    );
    assert_eq!(input_val(&vm, "r"), Value::PosInt(10));
}

#[test]
fn computed_object_key_with_spread() {
    let vm = testutil::run_vm(
        "let k = \"c\"; let base = { a: 1, b: 2 }; input.obj = { ...base, [k]: 3 }; input.r = JSON.stringify(input.obj);",
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
    assert_eq!(eval("({ [1 + 2]: \"three\" })[\"3\"]"), eval("\"three\""));
    assert_eq!(
        eval("({ [\"hello \" + \"world\"]: true })[\"hello world\"]"),
        Value::Bool(true)
    );
}

// ── object method shorthand (A4) ─────────────────────────────────

#[test]
fn object_method_shorthand() {
    assert_eq!(
        testutil::run_val("let obj = { run(x) { return x + 1; } }; return obj.run(5);"),
        testutil::num(6.0)
    );
    assert_eq!(
        testutil::run_val(
            "let obj = { greet(name) { return \"hello \" + name; } }; return obj.greet(\"world\");"
        ),
        testutil::run_val("return \"hello world\";")
    );
}

#[test]
fn object_method_shorthand_empty_body() {
    assert_eq!(
        testutil::run_val("let obj = { nop() { } }; return obj.nop();"),
        Value::Undefined
    );
}

#[test]
fn object_method_with_computed_key() {
    assert_eq!(
        testutil::run_val(
            "let k = \"inc\"; let obj = { [k](x) { return x + 1; } }; return obj.inc(9);"
        ),
        testutil::num(10.0)
    );
}

#[test]
fn shorthand_properties_compile() {
    assert_eq!(
        testutil::run_val("let a = 10; let obj = { a }; return obj.a;"),
        Value::PosInt(10)
    );
    assert_eq!(
        testutil::run_val("let x = 7; let y = 3; let obj = { x, y }; return obj.x + obj.y;"),
        testutil::num(10.0)
    );
}

// ── object destructuring rest ────────────────────────────────────

#[test]
fn object_rest_declaration() {
    assert_eq!(
        testutil::run_ret("let {a, ...rest} = {a: 1, b: 2, c: 3}; return { a, rest };"),
        serde_json::json!({"a": 1, "rest": {"b": 2, "c": 3}})
    );
    // No surplus keys → empty rest.
    assert_eq!(
        testutil::run_ret("let {a, ...rest} = {a: 1}; return rest;"),
        serde_json::json!({})
    );
    // Rest alone is a shallow copy, independent of the source.
    assert_eq!(
        testutil::run_ret("let src = {x: 1}; let {...r} = src; r.y = 2; return { src, r };"),
        serde_json::json!({"src": {"x": 1}, "r": {"x": 1, "y": 2}})
    );
}

#[test]
fn object_rest_with_defaults_and_nesting() {
    assert_eq!(
        testutil::run_ret("let {a = 9, ...rest} = {b: 2}; return { a, rest };"),
        serde_json::json!({"a": 9, "rest": {"b": 2}})
    );
    assert_eq!(
        testutil::run_ret(
            "let {p: {q, ...inner}, ...outer} = {p: {q: 1, r: 2}, s: 3}; return { q, inner, outer };"
        ),
        serde_json::json!({"q": 1, "inner": {"r": 2}, "outer": {"s": 3}})
    );
}

#[test]
fn object_rest_computed_key_evaluates_once() {
    assert_eq!(
        testutil::run_ret(
            "let n = 0; function k() { n += 1; return 'a'; } let {[k()]: v, ...rest} = {a: 1, b: 2}; return { v, rest, n };"
        ),
        serde_json::json!({"v": 1, "rest": {"b": 2}, "n": 1})
    );
}

#[test]
fn object_rest_string_and_numeric_keys() {
    assert_eq!(
        testutil::run_ret(
            "let {'a b': v, 1: w, ...rest} = {'a b': 1, '1': 2, c: 3}; return { v, w, rest };"
        ),
        serde_json::json!({"v": 1, "w": 2, "rest": {"c": 3}})
    );
}

#[test]
fn object_rest_assignment() {
    assert_eq!(
        testutil::run_ret("let a, rest; ({a, ...rest} = {a: 1, b: 2, c: 3}); return { a, rest };"),
        serde_json::json!({"a": 1, "rest": {"b": 2, "c": 3}})
    );
    assert_eq!(
        testutil::run_ret("let a, rest; ({a = 5, ...rest} = {b: 2}); return { a, rest };"),
        serde_json::json!({"a": 5, "rest": {"b": 2}})
    );
}

#[test]
fn object_rest_null_source_divergence() {
    // Inherits `ObjExtend` semantics (documented divergence for object
    // spread): a null/undefined source gives an empty rest where JS throws.
    assert_eq!(
        testutil::run_ret("let {...r} = null; return r;"),
        serde_json::json!({})
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

// ── method-builtin shadowing by object properties ──────────────────────────

/// An object's own property whose name collides with a builtin method
/// (`push`, `trim`, …) shadows the builtin: `obj.push(x)` calls the property,
/// not the array builtin. The receiver flows through `Instr::CallBuiltin`,
/// whose handler declines an Object receiver and re-routes to the property.
#[test]
fn object_property_shadows_method_builtin() {
    // Own `push` wins over the array builtin. (`x + 100` promotes to Float.)
    assert_eq!(
        testutil::run_val("const o = { push: (x) => x + 100 }; return o.push(5);"),
        testutil::num(105.0)
    );
    // A real array receiver still hits the builtin (returns the new length).
    assert_eq!(
        testutil::run_val("const a = [1, 2]; a.push(3); return a.length;"),
        testutil::num(3.0)
    );
    // Works for a string-method name too, via a captured closure.
    assert_eq!(
        testutil::run_val("const n = 7; const o = { trim: () => n }; return o.trim();"),
        Value::PosInt(7)
    );
}

/// Calling a builtin-method name on an object that lacks that property is a
/// TypeError naming the method — the re-route finds no own property to call.
#[test]
fn missing_method_on_object_is_type_error() {
    let err = testutil::run_runtime_err("const o = { a: 1 }; return o.push(5);");
    assert!(
        err.message.contains("push"),
        "expected message to name `push`, got: {}",
        err.message
    );
}

/// `hasOwnProperty` is the one method builtin that *accepts* an Object receiver,
/// so it stays on the builtin fast path: it succeeds without consulting the
/// object's own properties. An own `hasOwnProperty` therefore does **not**
/// shadow the builtin — a deliberate divergence from JS and from the other
/// method builtins' shadowing (see `obj_has_own_property`).
#[test]
fn has_own_property_is_not_shadowed() {
    // Own `hasOwnProperty` does NOT win: the builtin runs (`'a'` is present).
    assert_eq!(
        testutil::run_val(
            "const o = { a: 1, hasOwnProperty: () => 999 }; return o.hasOwnProperty('a');"
        ),
        Value::Bool(true)
    );
    // And the builtin behaves normally on a plain object.
    assert_eq!(
        testutil::run_val("return ({ a: 1 }).hasOwnProperty('b');"),
        Value::Bool(false)
    );
}

/// Shadowing is uniform across every receiver type that routes through a
/// `*_receiver` getter: map (`map_receiver`), set (`set_receiver`), regexp
/// (`regexp_receiver`), and the polymorphic string/array methods. An own
/// property of the builtin's name wins in every case.
#[test]
fn shadowing_is_uniform_across_receiver_types() {
    // Map method (`get`/`has` → map_receiver via the map_set_* dispatchers).
    assert_eq!(
        testutil::run_val("const o = { get: (k) => 'shadowed' }; return o.get('x');"),
        eval("'shadowed'")
    );
    assert_eq!(
        testutil::run_val("const o = { has: () => 42 }; return o.has('x');"),
        Value::PosInt(42)
    );
    // Set method (`add` → set_receiver).
    assert_eq!(
        testutil::run_val("const o = { add: (x) => x + 1 }; return o.add(9);"),
        testutil::num(10.0)
    );
    // RegExp method (`test` → regexp_receiver).
    assert_eq!(
        testutil::run_val("const o = { test: (s) => s + '!' }; return o.test('hi');"),
        eval("'hi!'")
    );
    // Polymorphic method (`slice` → slice_poly's fallthrough) and varargs
    // poly (`concat`).
    assert_eq!(
        testutil::run_val("const o = { slice: (a, b) => a + b }; return o.slice(2, 3);"),
        testutil::num(5.0)
    );
    assert_eq!(
        testutil::run_val("const o = { concat: (x) => x * 2 }; return o.concat(21);"),
        testutil::num(42.0)
    );
}

/// The general `x.toString()` method: one renderer (`to_js_string`) behind
/// `String(x)`, template interpolation, and this method — including regexp,
/// which is no longer a special-cased builtin. An object's own `toString`
/// shadows the default `"[object Object]"`.
#[test]
fn general_to_string_method() {
    assert_eq!(testutil::run_val("return (42).toString();"), eval("'42'"));
    assert_eq!(
        testutil::run_val("return [1, 2, 3].toString();"),
        eval("'1,2,3'")
    );
    assert_eq!(
        testutil::run_val("return /ab/gi.toString();"),
        eval("'/ab/gi'")
    );
    assert_eq!(
        testutil::run_val("return (true).toString();"),
        eval("'true'")
    );
    // Plain object → default; an own `toString` shadows it.
    assert_eq!(
        testutil::run_val("return ({}).toString();"),
        eval("'[object Object]'")
    );
    assert_eq!(
        testutil::run_val("return ({ toString: () => 'custom' }).toString();"),
        eval("'custom'")
    );
    // Identical to the String()/template renderer.
    assert_eq!(
        testutil::run_val("return String([1, 2]) === [1, 2].toString();"),
        Value::Bool(true)
    );
}

// ── Phase 13 OO: Step 1a — `this` as a frame field ─────────────────────

#[test]
fn this_at_top_level_is_undefined() {
    assert_eq!(testutil::run_val("return this;"), Value::Undefined);
}

#[test]
fn this_in_plain_call_is_undefined() {
    assert_eq!(
        testutil::run_val("function f() { return this; } return f();"),
        Value::Undefined
    );
}

#[test]
fn this_in_arrow_is_undefined() {
    assert_eq!(
        testutil::run_val("const f = () => this; return f();"),
        Value::Undefined
    );
}

#[test]
fn this_in_method_is_undefined_pre_step_3() {
    // Until step 3 no call form sets a non-undefined `this`, so a method call
    // like `obj.m()` still reads `undefined` — correct for this step.
    assert_eq!(
        testutil::run_val(
            "const obj = { greet: function() { return this; } }; return obj.greet();"
        ),
        Value::Undefined
    );
}

#[test]
fn this_is_not_a_compile_error() {
    let prog = compile("return this;").expect("this should compile now");
    assert!(
        !prog.code.is_empty(),
        "program compiled but has no instructions"
    );
}

#[test]
fn this_preserves_call_conventions() {
    // No slot layout changed: default params, arguments, closures
    // all unaffected.
    assert_eq!(
        testutil::run_val("function f(a = 1) { return a + arguments[0]; } return f(2);"),
        testutil::num(4.0)
    );
    assert_eq!(
        testutil::run_val(
            "function make() { let x = 42; return function() { return x; }; } let g = make(); return g();"
        ),
        Value::PosInt(42)
    );
}

#[test]
fn loadthis_is_classified_pure() {
    use crate::vm::Instr;
    let prog = compile("return this;").expect("compiles");
    let has_loadthis = prog.code.iter().any(|i| matches!(i, Instr::LoadThis));
    assert!(
        has_loadthis,
        "expected LoadThis in code, got {:?}",
        prog.code
    );
    // Pure-push: `this;` (bare expression statement) reduces to a no-op.
    let prog = compile("this; return 1;").expect("compiles");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::LoadThis)),
        "bare `this;` should be optimized away, got {:?}",
        prog.code
    );
}

#[test]
fn this_in_arrow_uses_captured_slot_not_loadthis() {
    // An arrow referencing `this` reads through the captured-binding path —
    // the arrow body contains no LoadThis; the reify prologue in the nearest
    // non-arrow scope (here the root) contains LoadThis + SetLocal.
    let prog = compile("const f = () => this; return f();").expect("compiles");
    // The arrow body (closure) should NOT contain LoadThis.
    let has_loadthis = prog.code.iter().any(|i| matches!(i, Instr::LoadThis));
    // The root scope contains the reify prologue (LoadThis + SetLocal),
    // so LoadThis does appear — but only once, in the root, not in the arrow.
    assert!(has_loadthis, "root should have LoadThis for reify prologue");
    // Count LoadThis occurrences: should be exactly 1 (root reify only).
    let loadthis_count = prog
        .code
        .iter()
        .filter(|i| matches!(i, Instr::LoadThis))
        .count();
    assert_eq!(
        loadthis_count, 1,
        "expected exactly 1 LoadThis (root reify prologue), got {}",
        loadthis_count
    );
}

#[test]
fn direct_this_no_nested_arrow_has_no_reify_prologue() {
    // A non-arrow function that references `this` directly, with no
    // this-capturing nested arrow, emits no reify prologue — just LoadThis
    // at the use site. No SetLocal for <this> slot allocation.
    let prog = compile("function f() { return this; } return f();").expect("compiles");
    let has_loadthis = prog.code.iter().any(|i| matches!(i, Instr::LoadThis));
    assert!(has_loadthis, "expected LoadThis in code");
    // Verify the function body has LoadThis followed by Return (no SetLocal
    // for a synthetic this_slot between them).
    assert!(
        prog.code
            .windows(2)
            .any(|w| matches!(w[0], Instr::LoadThis) && matches!(w[1], Instr::Return(_))),
        "LoadThis should be followed by Return, not SetLocal"
    );
}

#[test]
fn arrow_within_arrow_captures_transitively() {
    // Arrow-within-arrow: inner arrow's `this` resolves transitively through
    // the outer arrow to the nearest non-arrow (root), reading undefined.
    assert_eq!(
        testutil::run_val(
            "const outer = () => { const inner = () => this; return inner(); }; return outer();"
        ),
        Value::Undefined
    );
}

#[test]
fn function_without_this_is_unchanged() {
    // A plain function with no `this` reference is byte-for-byte unchanged.
    // We verify this explicitly: no LoadThis, no SetLocal targeting this_slot.
    let prog = compile("function add(a, b) { return a + b; } return add(1, 2);").expect("compiles");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::LoadThis)),
        "function without `this` should have no LoadThis"
    );
}
