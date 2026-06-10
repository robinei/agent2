//! Function, closure, capture, `arguments`, and per-iteration cell tests.
//! Also includes some lang_basics/objects_arrays tests that were co-located.

use super::*;
use crate::compiler::compile;
use crate::testutil;
use crate::vm::{Instr, SlotKind, StepResult, VM, Value};

#[test]
fn arguments_variadic_sum() {
    let vm = testutil::run(
        "function sum() { let t = 0; for (let i = 0; i < arguments.length; i++) { t += arguments[i]; } return t; } input.r = sum(1, 2, 3, 4);",
    );
    assert_eq!(input_val(&vm, "r"), testutil::num(10.0));
}

#[test]
fn arguments_beyond_declared_params() {
    let vm = testutil::run("function f(a) { return a + arguments.length; } input.r = f(10, 20, 30);");
    assert_eq!(input_val(&vm, "r"), testutil::num(13.0));
}

#[test]
fn arguments_is_cached_per_frame() {
    let vm = testutil::run("function f() { return arguments === arguments; } input.r = f(1, 2);");
    assert_eq!(input_val(&vm, "r"), Value::Bool(true));
}

#[test]
fn arguments_can_be_shadowed() {
    let vm = testutil::run("function f() { let arguments = 42; return arguments; } input.r = f(1, 2, 3);");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(42));
}

#[test]
fn arguments_in_arrow_is_own_frame() {
    let vm = testutil::run("let f = (a) => arguments.length; input.r = f(1, 2, 3);");
    assert_eq!(input_val(&vm, "r"), testutil::num(3.0));
}

#[test]
fn arguments_at_top_level_is_empty() {
    let vm = testutil::run("input.r = arguments.length;");
    assert_eq!(input_val(&vm, "r"), testutil::num(0.0));
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
    assert_eq!(testutil::run_val("let x = 5; x += 3; return x;"), testutil::num(8.0));
    assert_eq!(testutil::run_val("let x = 5; x -= 2; return x;"), testutil::num(3.0));
    assert_eq!(testutil::run_val("let x = 5; x *= 2; return x;"), testutil::num(10.0));
    assert_eq!(testutil::run_val("let x = 2; x **= 3; return x;"), testutil::num(8.0));
    assert_eq!(testutil::run_val("let x = 7; x %= 3; return x;"), testutil::num(1.0));
    assert_eq!(testutil::run_val("let x = 1; x <<= 3; return x;"), testutil::num(8.0));
    match testutil::run_val("let s = \"a\"; s += \"b\"; return s;") {
        Value::String(s) => assert_eq!(s.as_str(), "ab"),
        other => panic!("not a string: {other:?}"),
    }
    let vm = testutil::run("input.o = { a: 1 }; input.o.a += 4; input.r = input.o.a;");
    assert_eq!(input_val(&vm, "r"), testutil::num(5.0));
    let vm = testutil::run("input.arr = [1, 2]; input.arr[0] += 10; input.r = input.arr[0];");
    assert_eq!(input_val(&vm, "r"), testutil::num(11.0));
    assert_eq!(testutil::run_val("let x = 5; return (x += 5);"), testutil::num(10.0));
}

#[test]
fn logical_assignment() {
    assert_eq!(testutil::run_val("let x = 0; x ||= 5; return x;"), Value::PosInt(5));
    assert_eq!(testutil::run_val("let x = 3; x ||= 5; return x;"), Value::PosInt(3));
    assert_eq!(testutil::run_val("let x = 3; x &&= 7; return x;"), Value::PosInt(7));
    assert_eq!(testutil::run_val("let x = 0; x &&= 7; return x;"), Value::PosInt(0));
    assert_eq!(testutil::run_val("let x = null; x ??= 9; return x;"), Value::PosInt(9));
    assert_eq!(testutil::run_val("let x = 0; x ??= 9; return x;"), Value::PosInt(0));
    let vm = testutil::run("input.hit = 0; let x = 3; x ||= (input.hit = 1); input.r = x;");
    assert_eq!(input_val(&vm, "hit"), Value::PosInt(0));
    assert_eq!(input_val(&vm, "r"), Value::PosInt(3));
    let vm = testutil::run("input.o = { a: null }; input.o.a ??= 5; input.r = input.o.a;");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(5));
    let vm = testutil::run("input.o = { a: 2 }; input.r = (input.o.a ??= 99);");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(2));
    let vm = testutil::run("input.arr = [7]; input.r = (input.arr[0] ||= 1);");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(7));
}

#[test]
fn increment_decrement() {
    let vm = testutil::run("let x = 5; input.a = x++; input.b = x;");
    assert_eq!(input_val(&vm, "a"), testutil::num(5.0));
    assert_eq!(input_val(&vm, "b"), testutil::num(6.0));
    assert_eq!(testutil::run_val("let x = 5; x--; return x;"), testutil::num(4.0));
    assert_eq!(testutil::run_val("let x = 5; return --x;"), testutil::num(4.0));
    assert_eq!(testutil::run_val("let x = \"5\"; x++; return x;"), testutil::num(6.0));
}

#[test]
fn array_destructuring_declaration() {
    let vm = testutil::run("let [a, b] = [10, 20]; input.a = a; input.b = b;");
    assert_eq!(input_val(&vm, "a"), Value::PosInt(10));
    assert_eq!(input_val(&vm, "b"), Value::PosInt(20));
    assert_eq!(testutil::run_val("let [, b] = [1, 2]; return b;"), Value::PosInt(2));
    assert_eq!(testutil::run_val("let [a = 5] = []; return a;"), Value::PosInt(5));
    assert_eq!(testutil::run_val("let [a = 5] = [1]; return a;"), Value::PosInt(1));
}

#[test]
fn object_destructuring_declaration() {
    let vm = testutil::run("let { x, y } = { x: 1, y: 2 }; input.x = x; input.y = y;");
    assert_eq!(input_val(&vm, "x"), Value::PosInt(1));
    assert_eq!(input_val(&vm, "y"), Value::PosInt(2));
    assert_eq!(testutil::run_val("let { a: aa } = { a: 7 }; return aa;"), Value::PosInt(7));
    assert_eq!(testutil::run_val("let { b = 3 } = {}; return b;"), Value::PosInt(3));
    assert_eq!(testutil::run_val("let { b = 3 } = { b: 9 }; return b;"), Value::PosInt(9));
}

#[test]
fn destructuring_assignment() {
    let vm = testutil::run("let a, b; [a, b] = [3, 4]; input.a = a; input.b = b;");
    assert_eq!(input_val(&vm, "a"), Value::PosInt(3));
    assert_eq!(input_val(&vm, "b"), Value::PosInt(4));
}

#[test]
fn let_without_init_resets_each_iteration() {
    let vm = testutil::run(
        "let last; for (let i = 0; i < 2; i++) { let x; if (i === 0) x = 5; last = x; } input.r = last;",
    );
    assert_eq!(input_val(&vm, "r"), Value::Undefined);
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
        "let [a, ...rest] = [1, 2];",
        "outer: while (true) break outer;",
    ] {
        assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
    }
    let errs = compile("const x = 1; x = 2;").expect_err("const");
    assert!(errs[0].message.contains("constant"), "got: {}", errs[0].message);
    let errs = compile("let [a, ...rest] = [1, 2];").expect_err("rest");
    assert!(errs[0].message.contains("rest"), "got: {}", errs[0].message);
}

// ── Phase 3: functions / closures ─────────────────────────────────

#[test]
fn function_declaration_and_call() {
    assert_eq!(testutil::run_val("function add(a, b) { return a + b; } return add(3, 4);"), testutil::num(7.0));
}

#[test]
fn function_hoisting_forward_reference() {
    assert_eq!(testutil::run_val("return add(2, 3); function add(a, b) { return a + b; }"), testutil::num(5.0));
}

#[test]
fn function_return_without_value() {
    assert_eq!(testutil::run_val("function f() { return; } return f();"), Value::Undefined);
}

#[test]
fn function_implicit_return() {
    assert_eq!(testutil::run_val("function f() {} return f();"), Value::Undefined);
}

#[test]
fn parameter_defaults() {
    assert_eq!(testutil::run_val("function f(x = 5) { return x; } return f();"), Value::PosInt(5));
    assert_eq!(testutil::run_val("function f(x = 5) { return x; } return f(9);"), Value::PosInt(9));
}

#[test]
fn function_expression() {
    assert_eq!(testutil::run_val("let add = function(a, b) { return a + b; }; return add(5, 6);"), testutil::num(11.0));
}

#[test]
fn arrow_expression_body() {
    assert_eq!(testutil::run_val("let add = (a, b) => a + b; return add(3, 4);"), testutil::num(7.0));
}

#[test]
fn arrow_block_body() {
    assert_eq!(testutil::run_val("let f = (x) => { return x * 2; }; return f(7);"), testutil::num(14.0));
}

#[test]
fn recursion() {
    assert_eq!(testutil::run_val("function fact(n) { if (n <= 1) return 1; return n * fact(n - 1); } return fact(5);"), testutil::num(120.0));
}

#[test]
fn mutual_recursion() {
    assert_eq!(testutil::run_val("function isEven(n) { if (n === 0) return true; return isOdd(n - 1); } function isOdd(n) { if (n === 0) return false; return isEven(n - 1); } return isEven(4);"), Value::Bool(true));
}

#[test]
fn closure_captures_local() {
    let vm = testutil::run("function makeAdder(x) { return function(y) { return x + y; }; } input.add5 = makeAdder(5); input.r = input.add5(3);");
    assert_eq!(input_val(&vm, "r"), testutil::num(8.0));
}

#[test]
fn closure_mutation_visible() {
    let vm = testutil::run("function makeCounter() { let count = 0; function inc() { count = count + 1; return count; } return inc; } input.c1 = makeCounter(); input.c1(); input.r = input.c1();");
    assert_eq!(input_val(&vm, "r"), testutil::num(2.0));
}

// ── per-iteration capture ────────────────────────────────────────

#[test]
fn for_let_head_var_captured_per_iteration() {
    let vm = testutil::run("let fns = []; for (let i = 0; i < 3; i++) { fns.push(() => i); } let a = fns[0], b = fns[1], c = fns[2]; input.r = a() * 100 + b() * 10 + c();");
    assert_eq!(input_val(&vm, "r"), testutil::num(12.0));
}

#[test]
fn for_body_declared_var_captured_per_iteration() {
    let vm = testutil::run("let fns = []; for (let i = 0; i < 3; i++) { let j = i * 2; fns.push(() => j); } let a = fns[0], b = fns[1], c = fns[2]; input.r = a() * 100 + b() * 10 + c();");
    assert_eq!(input_val(&vm, "r"), testutil::num(24.0));
}

#[test]
fn for_of_loop_var_captured_per_iteration() {
    let vm = testutil::run("let fns = []; for (const x of [10, 20, 30]) { fns.push(() => x); } let a = fns[0], b = fns[1], c = fns[2]; input.r = a() * 100 + b() * 10 + c();");
    assert_eq!(input_val(&vm, "r"), testutil::num(1230.0));
}

#[test]
fn for_in_loop_var_captured_per_iteration() {
    let vm = testutil::run("let fns = []; let obj = { a: 1, b: 2 }; for (const k in obj) { fns.push(() => k); } let a = fns[0], b = fns[1]; input.r = a() + b();");
    match input_val(&vm, "r") { Value::String(s) => assert_eq!(s.as_str(), "ab"), other => panic!("not a string: {other:?}") }
}

#[test]
fn while_body_declared_var_captured_per_iteration() {
    let vm = testutil::run("let fns = []; let i = 0; while (i < 3) { let j = i; fns.push(() => j); i = i + 1; } let a = fns[0], b = fns[1], c = fns[2]; input.r = a() * 100 + b() * 10 + c();");
    assert_eq!(input_val(&vm, "r"), testutil::num(12.0));
}

#[test]
fn for_head_var_value_carries_forward() {
    let vm = testutil::run("let sum = 0; for (let i = 0; i < 5; i++) { sum = sum + i; } input.r = sum;");
    assert_eq!(input_val(&vm, "r"), testutil::num(10.0));
}

#[test]
fn captured_var_in_loop_is_shared_not_per_iteration() {
    let vm = testutil::run("let fns = []; for (var i = 0; i < 3; i++) { fns.push(() => i); } let a = fns[0], b = fns[1], c = fns[2]; input.r = a() * 100 + b() * 10 + c();");
    assert_eq!(input_val(&vm, "r"), testutil::num(333.0));
}

#[test]
fn captured_loop_var_allocates_plain_not_boxed() {
    let prog = compile("let fns = []; for (let i = 0; i < 3; i++) { fns.push(() => i); }").expect("compiles");
    let has_boxed = prog.code.iter().any(|i| matches!(i, Instr::EnterFrame(_, _, kinds) if kinds.iter().any(|k| *k == SlotKind::Boxed)));
    assert!(!has_boxed, "captured loop var should be Plain-allocated: {:?}", prog.code);
    assert!(prog.code.iter().any(|i| matches!(i, Instr::FreshCell(_))), "captured loop var should still be re-boxed per iteration: {:?}", prog.code);
}

#[test]
fn plain_loop_var_emits_no_fresh_cell() {
    let prog = compile("let s = 0; for (let i = 0; i < 3; i++) { s = s + i; }").expect("compiles");
    assert!(!prog.code.iter().any(|i| matches!(i, Instr::FreshCell(_))), "uncaptured loop var should not emit FreshCell: {:?}", prog.code);
}

#[test]
fn function_decl_in_block_scope() {
    let vm = testutil::run("input.r = foo(); { function foo() { return 9; } }");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(9));
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
    let vm = testutil::run("function outer() { let x = 10; function middle() { function inner() { return x; } return inner(); } return middle(); } input.r = outer();");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(10));
}

#[test]
fn sibling_block_shadowing_uses_distinct_bindings() {
    let vm = testutil::run("let x = 1; { let x = 2; input.a = x; } { let x = 3; input.b = x; } input.c = x;");
    assert_eq!(input_val(&vm, "a"), Value::PosInt(2));
    assert_eq!(input_val(&vm, "b"), Value::PosInt(3));
    assert_eq!(input_val(&vm, "c"), Value::PosInt(1));
}

#[test]
fn write_only_capture_is_detected() {
    let vm = testutil::run("function make() { let v = 0; function setter(n) { v = n; } function getter() { return v; } setter(42); return getter(); } input.r = make();");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(42));
}
