//! Language basics: literals, operators, coercion, builtin intrinsics.

use super::*;
use crate::testutil;
use crate::testutil::{eval, eval_str};
use crate::vm::Value;

// ── literals ─────────────────────────────────────────────────────

#[test]
fn literals() {
    assert_eq!(eval("42"), Value::PosInt(42));
    assert_eq!(eval("-7"), Value::NegInt(-7));
    assert_eq!(eval("3.5"), testutil::num(3.5));
    assert_eq!(eval("true"), Value::Bool(true));
    assert_eq!(eval("null"), Value::Null);
    assert_eq!(eval("undefined"), Value::Undefined);
    assert_eq!(eval_str("\"hi\""), "hi");
    assert!(matches!(eval("NaN"), Value::Float(n) if n.is_nan()));
    assert!(matches!(eval("Infinity"), Value::Float(n) if n.is_infinite()));
}

#[test]
fn arithmetic_and_operators() {
    assert_eq!(eval("1 + 2 * 3"), testutil::num(7.0));
    assert_eq!(eval("(1 + 2) * 3"), testutil::num(9.0));
    assert_eq!(eval("10 % 3"), testutil::num(1.0));
    assert_eq!(eval("2 ** 10"), testutil::num(1024.0));
    assert_eq!(eval("7 & 3"), testutil::num(3.0));
    assert_eq!(eval("1 << 4"), testutil::num(16.0));
    assert_eq!(eval("-5"), Value::NegInt(-5));
    assert_eq!(eval("+\"42\""), testutil::num(42.0));
    assert_eq!(eval("!0"), Value::Bool(true));
    assert_eq!(eval("~0"), testutil::num(-1.0));
    assert_eq!(eval_str("\"a\" + \"b\""), "ab");
}

#[test]
fn comparisons_and_equality() {
    assert_eq!(eval("1 < 2"), Value::Bool(true));
    assert_eq!(eval("2 <= 2"), Value::Bool(true));
    assert_eq!(eval("3 === 3"), Value::Bool(true));
    assert_eq!(eval("3 !== 4"), Value::Bool(true));
    assert_eq!(eval("1 == \"1\""), Value::Bool(true));
    assert_eq!(eval("1 === \"1\""), Value::Bool(false));
    assert_eq!(eval("null == undefined"), Value::Bool(true));
}

#[test]
fn short_circuit_logical() {
    assert_eq!(eval("0 && 5"), Value::PosInt(0));
    assert_eq!(eval("3 && 5"), Value::PosInt(5));
    assert_eq!(eval("0 || 5"), Value::PosInt(5));
    assert_eq!(eval("3 || 5"), Value::PosInt(3));
    assert_eq!(eval("null ?? 5"), Value::PosInt(5));
    assert_eq!(eval("0 ?? 5"), Value::PosInt(0));
    assert_eq!(eval("undefined ?? 9"), Value::PosInt(9));
}

#[test]
fn short_circuit_does_not_evaluate_rhs() {
    let vm = testutil::run("input.hit = 0; input.r = false && (input.hit = 1);");
    assert_eq!(input_val(&vm, "r"), Value::Bool(false));
    assert_eq!(input_val(&vm, "hit"), Value::PosInt(0));
    let vm = testutil::run("input.hit = 0; input.r = true || (input.hit = 1);");
    assert_eq!(input_val(&vm, "r"), Value::Bool(true));
    assert_eq!(input_val(&vm, "hit"), Value::PosInt(0));
}

#[test]
fn ternary() {
    assert_eq!(eval("1 ? 10 : 20"), Value::PosInt(10));
    assert_eq!(eval("0 ? 10 : 20"), Value::PosInt(20));
}

#[test]
fn typeof_op() {
    assert_eq!(eval_str("typeof 5"), "number");
    assert_eq!(eval_str("typeof \"x\""), "string");
    assert_eq!(eval_str("typeof true"), "boolean");
    assert_eq!(eval_str("typeof undefined"), "undefined");
    assert_eq!(eval_str("typeof null"), "object");
    assert_eq!(eval_str("typeof [1]"), "object");
}

#[test]
fn template_literals() {
    let vm = testutil::run("input.name = \"bob\"; input.r = `hi ${input.name}, ${1 + 2}!`;");
    match input_val(&vm, "r") {
        Value::String(s) => assert_eq!(s.as_bytes(), b"hi bob, 3!"),
        other => panic!("{other:?}"),
    }
}

// ── intrinsics ───────────────────────────────────────────────────

#[test]
fn intrinsics_static() {
    assert_eq!(eval("Math.max(3, 7)"), testutil::num(7.0));
    assert_eq!(eval("Math.min(3, 7)"), testutil::num(3.0));
    assert_eq!(eval("Math.abs(-5)"), testutil::num(5.0));
    assert_eq!(eval("Math.floor(3.9)"), testutil::num(3.0));
    assert_eq!(eval("Math.pow(2, 5)"), testutil::num(32.0));
    assert_eq!(eval("Object.keys({ a: 1, b: 2 }).length"), testutil::num(2.0));
    assert_eq!(eval("Object.values({ a: 5 })[0]"), Value::PosInt(5));
    assert_eq!(eval("JSON.parse(\"[1,2,3]\").length"), testutil::num(3.0));
    assert_eq!(eval_str("JSON.stringify([1,2])"), "[1,2]");
    assert_eq!(eval("Number.isInteger(4)"), Value::Bool(true));
    assert_eq!(eval("Array.isArray([1])"), Value::Bool(true));
    assert_eq!(eval("Array.isArray(5)"), Value::Bool(false));
}

#[test]
fn intrinsics_global() {
    assert_eq!(eval_str("String(5)"), "5");
    assert_eq!(eval("Number(\"42\")"), testutil::num(42.0));
    assert_eq!(eval("Boolean(0)"), Value::Bool(false));
    assert_eq!(eval("Boolean(\"x\")"), Value::Bool(true));
}

#[test]
fn intrinsics_methods() {
    assert_eq!(eval("\"a,b,c\".split(\",\").length"), testutil::num(3.0));
    assert_eq!(eval("\"a,b,c\".split(\",\", 2).length"), testutil::num(2.0));
    assert_eq!(eval("\"hello\".includes(\"ell\")"), Value::Bool(true));
    assert_eq!(eval("\"hello\".startsWith(\"he\")"), Value::Bool(true));
    assert_eq!(eval("\"hello\".endsWith(\"lo\")"), Value::Bool(true));
    assert_eq!(eval("\"hello\".indexOf(\"l\")"), testutil::num(2.0));
    assert_eq!(eval_str("\"hello\".slice(1, 3)"), "el");
    assert_eq!(eval_str("\"  hi  \".trim()"), "hi");
    assert_eq!(eval_str("[\"a\", \"b\"].join(\"-\")"), "a-b");
    assert_eq!(eval_str("[1, 2].join()"), "1,2");
    let vm = testutil::run("input.arr = [1]; input.arr.push(2); input.r = input.arr.length;");
    assert_eq!(input_val(&vm, "r"), testutil::num(2.0));
    let vm = testutil::run("input.arr = [1, 2, 3]; input.r = input.arr.pop();");
    assert_eq!(input_val(&vm, "r"), Value::PosInt(3));
}
