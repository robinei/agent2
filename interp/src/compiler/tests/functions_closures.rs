//! Function, closure, capture, `arguments`, and per-iteration cell tests.
//! Also includes some lang_basics/objects_arrays tests that were co-located.

use crate::compiler::compile;
use crate::testutil;
use crate::vm::{ErrorKind, StepResult, VM, Value};

#[test]
fn arguments_variadic_sum() {
    assert_eq!(
        testutil::run_ret(
            "function sum() { let t = 0; for (let i = 0; i < arguments.length; i++) { t += arguments[i]; } return t; } return sum(1, 2, 3, 4);"
        ),
        serde_json::json!(10)
    );
}

#[test]
fn arguments_beyond_declared_params() {
    assert_eq!(
        testutil::run_ret("function f(a) { return a + arguments.length; } return f(10, 20, 30);"),
        serde_json::json!(13)
    );
}

#[test]
fn calling_effectively_const_non_callable_errors_with_its_value() {
    // Regression: the dynamic-call path read the callee with a raw `Local`
    // load. For an effectively-const binding the initializer store is
    // dead-eliminated, so that slot is never written — the call saw
    // `undefined` instead of the propagated constant. The callee read must
    // materialize the constant, so the error names the real value's type.
    let err = testutil::run_runtime_err("let n = 5; return n(1, 2);");
    assert_eq!(err.kind, ErrorKind::TypeError);
    assert!(
        err.message.contains("cannot call a number"),
        "got: {}",
        err.message
    );
}

#[test]
fn calling_effectively_const_builtin_binding() {
    // The propagated-constant callee path must also work when the constant
    // IS callable (a builtin stored in a never-reassigned `let`).
    assert_eq!(
        testutil::run_ret("let f = Math.sqrt; return f(16);"),
        serde_json::json!(4)
    );
}

#[test]
fn missing_args_pad_to_undefined() {
    // Fewer args than declared params: the missing params are `undefined`
    // (JS-like — user-function arity is not strict; the VM normalizes).
    assert_eq!(
        testutil::run_ret("function f(a, b) { return [a, b === undefined]; } return f(1);"),
        serde_json::json!([1, true])
    );
}

#[test]
fn arguments_is_cached_per_frame() {
    assert_eq!(
        testutil::run_ret("function f() { return arguments === arguments; } return f(1, 2);"),
        serde_json::json!(true)
    );
}

#[test]
fn arguments_can_be_shadowed() {
    assert_eq!(
        testutil::run_val(
            "function f() { let arguments = 42; return arguments; } return f(1, 2, 3);"
        ),
        Value::PosInt(42)
    );
}

#[test]
fn arguments_in_arrow_is_own_frame() {
    assert_eq!(
        testutil::run_ret("let f = (a) => arguments.length; return f(1, 2, 3);"),
        serde_json::json!(3)
    );
}

#[test]
fn arguments_at_top_level_is_empty() {
    assert_eq!(
        testutil::run_ret("return arguments.length;"),
        serde_json::json!(0)
    );
}

#[test]
fn hof_bare_builtin_callback() {
    assert_eq!(
        testutil::run_val("return [4, 9, 16].map(Math.sqrt).reduce((s, x) => s + x, 0);"),
        testutil::num(9.0)
    );
}

#[test]
fn compound_assignment() {
    assert_eq!(
        testutil::run_val("let x = 5; x += 3; return x;"),
        testutil::num(8.0)
    );
    assert_eq!(
        testutil::run_val("let x = 5; x -= 2; return x;"),
        testutil::num(3.0)
    );
    assert_eq!(
        testutil::run_val("let x = 5; x *= 2; return x;"),
        testutil::num(10.0)
    );
    assert_eq!(
        testutil::run_val("let x = 2; x **= 3; return x;"),
        testutil::num(8.0)
    );
    assert_eq!(
        testutil::run_val("let x = 7; x %= 3; return x;"),
        testutil::num(1.0)
    );
    assert_eq!(
        testutil::run_val("let x = 1; x <<= 3; return x;"),
        testutil::num(8.0)
    );
    match testutil::run_val("let s = \"a\"; s += \"b\"; return s;") {
        Value::String(s) => assert_eq!(s.as_str(), "ab"),
        other => panic!("not a string: {other:?}"),
    }
    assert_eq!(
        testutil::run_val("let o = { a: 1 }; o.a += 4; return o.a;"),
        testutil::num(5.0)
    );
    assert_eq!(
        testutil::run_val("let arr = [1, 2]; arr[0] += 10; return arr[0];"),
        testutil::num(11.0)
    );
    assert_eq!(
        testutil::run_val("let x = 5; return (x += 5);"),
        testutil::num(10.0)
    );
}

#[test]
fn logical_assignment() {
    assert_eq!(
        testutil::run_val("let x = 0; x ||= 5; return x;"),
        Value::PosInt(5)
    );
    assert_eq!(
        testutil::run_val("let x = 3; x ||= 5; return x;"),
        Value::PosInt(3)
    );
    assert_eq!(
        testutil::run_val("let x = 3; x &&= 7; return x;"),
        Value::PosInt(7)
    );
    assert_eq!(
        testutil::run_val("let x = 0; x &&= 7; return x;"),
        Value::PosInt(0)
    );
    assert_eq!(
        testutil::run_val("let x = null; x ??= 9; return x;"),
        Value::PosInt(9)
    );
    assert_eq!(
        testutil::run_val("let x = 0; x ??= 9; return x;"),
        Value::PosInt(0)
    );
    // Short-circuit: RHS must not evaluate when LHS is truthy.
    assert_eq!(
        testutil::run_ret("let hit = 0; let x = 3; x ||= (hit = 1); return { hit, x };"),
        serde_json::json!({"hit": 0, "x": 3})
    );
    assert_eq!(
        testutil::run_val("let o = { a: null }; o.a ??= 5; return o.a;"),
        Value::PosInt(5)
    );
    assert_eq!(
        testutil::run_val("let o = { a: 2 }; return (o.a ??= 99);"),
        Value::PosInt(2)
    );
    assert_eq!(
        testutil::run_val("let arr = [7]; return (arr[0] ||= 1);"),
        Value::PosInt(7)
    );
}

#[test]
fn increment_decrement() {
    assert_eq!(
        testutil::run_ret("let x = 5; let a = x++; let b = x; return { a, b };"),
        serde_json::json!({"a": 5, "b": 6})
    );
    assert_eq!(
        testutil::run_val("let x = 5; x--; return x;"),
        testutil::num(4.0)
    );
    assert_eq!(
        testutil::run_val("let x = 5; return --x;"),
        testutil::num(4.0)
    );
    assert_eq!(
        testutil::run_val("let x = \"5\"; x++; return x;"),
        testutil::num(6.0)
    );
}

#[test]
fn array_destructuring_declaration() {
    assert_eq!(
        testutil::run_ret("let [a, b] = [10, 20]; return { a, b };"),
        serde_json::json!({"a": 10, "b": 20})
    );
    assert_eq!(
        testutil::run_val("let [, b] = [1, 2]; return b;"),
        Value::PosInt(2)
    );
    assert_eq!(
        testutil::run_val("let [a = 5] = []; return a;"),
        Value::PosInt(5)
    );
    assert_eq!(
        testutil::run_val("let [a = 5] = [1]; return a;"),
        Value::PosInt(1)
    );
}

#[test]
fn object_destructuring_declaration() {
    assert_eq!(
        testutil::run_ret("let { x, y } = { x: 1, y: 2 }; return { x, y };"),
        serde_json::json!({"x": 1, "y": 2})
    );
    assert_eq!(
        testutil::run_val("let { a: aa } = { a: 7 }; return aa;"),
        Value::PosInt(7)
    );
    assert_eq!(
        testutil::run_val("let { b = 3 } = {}; return b;"),
        Value::PosInt(3)
    );
    assert_eq!(
        testutil::run_val("let { b = 3 } = { b: 9 }; return b;"),
        Value::PosInt(9)
    );
}

#[test]
fn destructuring_assignment() {
    assert_eq!(
        testutil::run_ret("let a, b; [a, b] = [3, 4]; return { a, b };"),
        serde_json::json!({"a": 3, "b": 4})
    );
}

#[test]
fn let_without_init_resets_each_iteration() {
    // `let x` without init in a loop body resets to undefined each iteration.
    assert_eq!(
        testutil::run_val(
            "let last; for (let i = 0; i < 2; i++) { let x; if (i === 0) x = 5; last = x; } return last;"
        ),
        Value::Undefined
    );
}

#[test]
fn phase2_diagnostics() {
    for src in [
        "const x = 1; x = 2;",
        "const x = 1; x += 1;",
        "const x = 1; x++;",
        "let input = 1;",
        "y = 1;",
        "break;",
        "continue;",
        "outer: while (true) break outer;",
    ] {
        assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
    }
    let errs = compile("const x = 1; x = 2;").expect_err("const");
    assert!(
        errs[0].message.contains("constant"),
        "got: {}",
        errs[0].message
    );
}

// ── Phase 3: functions / closures ─────────────────────────────────

#[test]
fn function_declaration_and_call() {
    assert_eq!(
        testutil::run_val("function add(a, b) { return a + b; } return add(3, 4);"),
        testutil::num(7.0)
    );
}

#[test]
fn function_hoisting_forward_reference() {
    assert_eq!(
        testutil::run_val("return add(2, 3); function add(a, b) { return a + b; }"),
        testutil::num(5.0)
    );
}

#[test]
fn function_return_without_value() {
    assert_eq!(
        testutil::run_val("function f() { return; } return f();"),
        Value::Undefined
    );
}

#[test]
fn function_implicit_return() {
    assert_eq!(
        testutil::run_val("function f() {} return f();"),
        Value::Undefined
    );
}

#[test]
fn parameter_defaults() {
    assert_eq!(
        testutil::run_val("function f(x = 5) { return x; } return f();"),
        Value::PosInt(5)
    );
    assert_eq!(
        testutil::run_val("function f(x = 5) { return x; } return f(9);"),
        Value::PosInt(9)
    );
}

#[test]
fn function_expression() {
    assert_eq!(
        testutil::run_val("let add = function(a, b) { return a + b; }; return add(5, 6);"),
        testutil::num(11.0)
    );
}

#[test]
fn arrow_expression_body() {
    assert_eq!(
        testutil::run_val("let add = (a, b) => a + b; return add(3, 4);"),
        testutil::num(7.0)
    );
}

#[test]
fn arrow_block_body() {
    assert_eq!(
        testutil::run_val("let f = (x) => { return x * 2; }; return f(7);"),
        testutil::num(14.0)
    );
}

#[test]
fn recursion() {
    assert_eq!(
        testutil::run_val(
            "function fact(n) { if (n <= 1) return 1; return n * fact(n - 1); } return fact(5);"
        ),
        testutil::num(120.0)
    );
}

#[test]
fn mutual_recursion() {
    assert_eq!(
        testutil::run_val(
            "function isEven(n) { if (n === 0) return true; return isOdd(n - 1); } function isOdd(n) { if (n === 0) return false; return isEven(n - 1); } return isEven(4);"
        ),
        Value::Bool(true)
    );
}

#[test]
fn closure_captures_local() {
    assert_eq!(
        testutil::run_ret(
            "function makeAdder(x) { return function(y) { return x + y; }; } let add5 = makeAdder(5); return add5(3);"
        ),
        serde_json::json!(8)
    );
}

#[test]
fn closure_mutation_visible() {
    assert_eq!(
        testutil::run_ret(
            "function makeCounter() { let count = 0; function inc() { count = count + 1; return count; } return inc; } let c = makeCounter(); c(); return c();"
        ),
        serde_json::json!(2)
    );
}

// ── per-iteration capture ────────────────────────────────────────

#[test]
fn for_let_head_var_captured_per_iteration() {
    assert_eq!(
        testutil::run_ret(
            "let fns = []; for (let i = 0; i < 3; i++) { fns.push(() => i); } let a = fns[0], b = fns[1], c = fns[2]; return a() * 100 + b() * 10 + c();"
        ),
        serde_json::json!(12)
    );
}

#[test]
fn for_body_declared_var_captured_per_iteration() {
    assert_eq!(
        testutil::run_ret(
            "let fns = []; for (let i = 0; i < 3; i++) { let j = i * 2; fns.push(() => j); } let a = fns[0], b = fns[1], c = fns[2]; return a() * 100 + b() * 10 + c();"
        ),
        serde_json::json!(24)
    );
}

#[test]
fn for_of_loop_var_captured_per_iteration() {
    assert_eq!(
        testutil::run_ret(
            "let fns = []; for (const x of [10, 20, 30]) { fns.push(() => x); } let a = fns[0], b = fns[1], c = fns[2]; return a() * 100 + b() * 10 + c();"
        ),
        serde_json::json!(1230)
    );
}

#[test]
fn for_in_loop_var_captured_per_iteration() {
    assert_eq!(
        testutil::run_ret(
            "let fns = []; let obj = { a: 1, b: 2 }; for (const k in obj) { fns.push(() => k); } let a = fns[0], b = fns[1]; return a() + b();"
        ),
        serde_json::json!("ab")
    );
}

#[test]
fn while_body_declared_var_captured_per_iteration() {
    assert_eq!(
        testutil::run_ret(
            "let fns = []; let i = 0; while (i < 3) { let j = i; fns.push(() => j); i = i + 1; } let a = fns[0], b = fns[1], c = fns[2]; return a() * 100 + b() * 10 + c();"
        ),
        serde_json::json!(12)
    );
}

#[test]
fn for_head_var_value_carries_forward() {
    assert_eq!(
        testutil::run_ret(
            "let sum = 0; for (let i = 0; i < 5; i++) { sum = sum + i; } return sum;"
        ),
        serde_json::json!(10)
    );
}

#[test]
fn captured_var_in_loop_is_shared_not_per_iteration() {
    assert_eq!(
        testutil::run_ret(
            "let fns = []; for (var i = 0; i < 3; i++) { fns.push(() => i); } let a = fns[0], b = fns[1], c = fns[2]; return a() * 100 + b() * 10 + c();"
        ),
        serde_json::json!(333)
    );
}

#[test]
fn function_decl_in_block_scope() {
    assert_eq!(
        testutil::run_val("let r = foo(); { function foo() { return 9; } } return r;"),
        Value::PosInt(9)
    );
}

#[test]
fn top_level_return_value() {
    let prog = compile("return 1;").expect("top-level return compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Done { value } => assert_eq!(value, Value::PosInt(1)),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn capture_through_intermediate_function() {
    assert_eq!(
        testutil::run_val(
            "function outer() { let x = 10; function middle() { function inner() { return x; } return inner(); } return middle(); } return outer();"
        ),
        Value::PosInt(10)
    );
}

#[test]
fn sibling_block_shadowing_uses_distinct_bindings() {
    assert_eq!(
        testutil::run_ret(
            "let x = 1; let a; { let x = 2; a = x; } let b; { let x = 3; b = x; } let c = x; return { a, b, c };"
        ),
        serde_json::json!({"a": 2, "b": 3, "c": 1})
    );
}

// ── rest parameters (A5) ────────────────────────────────────────────

#[test]
fn rest_param_basic() {
    assert_eq!(
        testutil::run_val("function f(...args) { return args.length; } return f(1, 2, 3);"),
        testutil::num(3.0)
    );
    assert_eq!(
        testutil::run_val("function f(...args) { return args[0]; } return f(10, 20);"),
        Value::PosInt(10)
    );
}

#[test]
fn rest_param_with_regular_params() {
    assert_eq!(
        testutil::run_val(
            "function f(a, b, ...rest) { return rest.length; } return f(1, 2, 3, 4, 5);"
        ),
        testutil::num(3.0)
    );
    assert_eq!(
        testutil::run_val("function f(a, b, ...rest) { return rest[0]; } return f(1, 2, 3);"),
        Value::PosInt(3)
    );
    assert_eq!(
        testutil::run_val("function f(a, b, ...rest) { return rest[1]; } return f(1, 2, 3, 4);"),
        Value::PosInt(4)
    );
}

#[test]
fn rest_param_no_surplus_args() {
    // The caller must not pad an Undefined for the rest slot: `arguments`
    // reflects only what was actually passed. Cover both the static-call
    // path and the CallDyn path.
    assert_eq!(
        testutil::run_val("function f(...rest) { return arguments.length; } return f();"),
        testutil::num(0.0)
    );
    assert_eq!(
        testutil::run_val("function f(...rest) { return rest.length; } return f();"),
        testutil::num(0.0)
    );
    assert_eq!(
        testutil::run_val(
            "function call(fn) { return fn(); } function f(...rest) { return arguments.length; } return call(f);"
        ),
        testutil::num(0.0)
    );
}

#[test]
fn rest_param_with_arguments() {
    assert_eq!(
        testutil::run_val("function f(...rest) { return arguments.length; } return f(1, 2, 3);"),
        testutil::num(3.0)
    );
}

// ── array destructuring rest (A5) ──────────────────────────────────

#[test]
fn array_destructuring_rest_declaration() {
    assert_eq!(
        testutil::run_val("let [a, ...rest] = [1, 2, 3, 4]; return a;"),
        Value::PosInt(1)
    );
    assert_eq!(
        testutil::run_val("let [a, ...rest] = [1, 2, 3, 4]; return rest.length;"),
        testutil::num(3.0)
    );
    assert_eq!(
        testutil::run_val("let [a, ...rest] = [1, 2, 3, 4]; return rest[0];"),
        Value::PosInt(2)
    );
}

#[test]
fn array_destructuring_rest_empty() {
    assert_eq!(
        testutil::run_val("let [a, ...rest] = [1]; return a;"),
        Value::PosInt(1)
    );
    assert_eq!(
        testutil::run_val("let [a, ...rest] = [1]; return rest.length;"),
        testutil::num(0.0)
    );
}

#[test]
fn array_destructuring_rest_only() {
    assert_eq!(
        testutil::run_val("let [...rest] = [1, 2, 3]; return rest.length;"),
        testutil::num(3.0)
    );
    assert_eq!(
        testutil::run_val("let [...rest] = [1, 2, 3]; return rest[0];"),
        Value::PosInt(1)
    );
}

#[test]
fn array_destructuring_rest_assignment() {
    assert_eq!(
        testutil::run_ret("let a, rest; [a, ...rest] = [10, 20, 30]; return { a, rest };"),
        serde_json::json!({"a": 10, "rest": [20, 30]})
    );
}

#[test]
fn write_only_capture_is_detected() {
    assert_eq!(
        testutil::run_val(
            "function make() { let v = 0; function setter(n) { v = n; } function getter() { return v; } setter(42); return getter(); } return make();"
        ),
        Value::PosInt(42)
    );
}
