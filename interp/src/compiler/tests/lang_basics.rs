//! Language basics: literals, operators, coercion, builtin intrinsics.

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
    assert_eq!(
        testutil::run_ret("let hit = 0; let r = false && (hit = 1); return { hit, r };"),
        serde_json::json!({"hit": 0, "r": false})
    );
    assert_eq!(
        testutil::run_ret("let hit = 0; let r = true || (hit = 1); return { hit, r };"),
        serde_json::json!({"hit": 0, "r": true})
    );
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
    assert_eq!(
        testutil::run_ret("let name = \"bob\"; return `hi ${name}, ${1 + 2}!`;"),
        serde_json::json!("hi bob, 3!")
    );
}

// ── intrinsics ───────────────────────────────────────────────────

#[test]
fn intrinsics_static() {
    assert_eq!(eval("Math.max(3, 7)"), testutil::num(7.0));
    assert_eq!(eval("Math.min(3, 7)"), testutil::num(3.0));
    assert_eq!(eval("Math.abs(-5)"), testutil::num(5.0));
    assert_eq!(eval("Math.floor(3.9)"), testutil::num(3.0));
    assert_eq!(eval("Math.pow(2, 5)"), testutil::num(32.0));
    assert_eq!(
        eval("Object.keys({ a: 1, b: 2 }).length"),
        testutil::num(2.0)
    );
    assert_eq!(eval("Object.values({ a: 5 })[0]"), Value::PosInt(5));
    assert_eq!(eval("JSON.parse(\"[1,2,3]\").length"), testutil::num(3.0));
    assert_eq!(eval_str("JSON.stringify([1,2])"), "[1,2]");
    assert_eq!(eval("Number.isInteger(4)"), Value::Bool(true));
    assert_eq!(eval("Array.isArray([1])"), Value::Bool(true));
    assert_eq!(eval("Array.isArray(5)"), Value::Bool(false));
    // Constants
    assert!(
        matches!(eval("Math.PI"), Value::Float(f) if (f - std::f64::consts::PI).abs() < 0.001)
    );
    assert_eq!(
        eval("Number.MAX_SAFE_INTEGER"),
        Value::PosInt(9007199254740991)
    );
    // Member read in non-call position works.
    let v = testutil::run_val("const x = Math.PI; return x * 2;");
    assert!(matches!(v, Value::Float(f) if (f - 2.0 * std::f64::consts::PI).abs() < 0.01));
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
    assert_eq!(eval("\"hello\".indexOf(\"l\")"), Value::PosInt(2));
    assert_eq!(eval_str("\"hello\".slice(1, 3)"), "el");
    assert_eq!(eval_str("\"  hi  \".trim()"), "hi");
    assert_eq!(eval_str("[\"a\", \"b\"].join(\"-\")"), "a-b");
    assert_eq!(eval_str("[1, 2].join()"), "1,2");
    assert_eq!(
        testutil::run_ret("let arr = [1]; arr.push(2); return arr.length;"),
        serde_json::json!(2)
    );
    assert_eq!(
        testutil::run_val("let arr = [1, 2, 3]; return arr.pop();"),
        Value::PosInt(3)
    );
}

// ── top-level return semantics ─────────────────────────────────────

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
    assert_eq!(
        testutil::run_ret("return [1, 2, 3];"),
        serde_json::json!([1, 2, 3])
    );
    assert_eq!(testutil::run_ret("return true;"), serde_json::json!(true));
    assert_eq!(testutil::run_ret("return null;"), serde_json::json!(null));
}
