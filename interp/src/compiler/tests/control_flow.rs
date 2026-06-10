//! Control-flow tests: loops, switch, break/continue, if/else, for-of, for-in.

use super::*;
use crate::testutil;
use crate::vm::Value;

// ── Phase 2: statements / control flow ──────────────────────────────

#[test]
fn local_declarations_and_reassignment() {
    assert_eq!(testutil::run_val("let x = 5; return x;"), Value::PosInt(5));
    assert_eq!(
        testutil::run_val("const x = 7; return x;"),
        Value::PosInt(7)
    );
    assert_eq!(
        testutil::run_val("let x = 1; x = 2; return x;"),
        Value::PosInt(2)
    );
    assert_eq!(testutil::run_val("let x; return x;"), Value::Undefined);
    assert_eq!(
        testutil::run_val("let a = 1, b = 2; return a + b;"),
        testutil::num(3.0)
    );
}

#[test]
fn block_scoping() {
    assert_eq!(
        testutil::run_ret("let x = 1; let inner; { let x = 2; inner = x; } let outer = x; return { inner, outer };")
            .as_object().unwrap(),
        &serde_json::json!({"inner": 2, "outer": 1}).as_object().unwrap().clone()
    );
}

#[test]
fn var_is_function_scoped_and_hoisted() {
    assert_eq!(
        testutil::run_ret(
            "let before = typeof x; var x = 5; let after = x; return { before, after };"
        )
        .as_object()
        .unwrap(),
        &serde_json::json!({"before": "undefined", "after": 5})
            .as_object()
            .unwrap()
            .clone()
    );
    assert_eq!(
        testutil::run_val("{ var y = 9; } return y;"),
        Value::PosInt(9)
    );
}

#[test]
fn if_else() {
    assert_eq!(
        testutil::run_val("let r; if (1 > 0) r = 10; else r = 20; return r;"),
        Value::PosInt(10)
    );
    assert_eq!(
        testutil::run_val("let r; if (0) r = 10; else r = 20; return r;"),
        Value::PosInt(20)
    );
    assert_eq!(
        testutil::run_val("let r = 3; if (false) r = 9; return r;"),
        Value::PosInt(3)
    );
    assert_eq!(
        testutil::run_val(
            "let x = 2, r; if (x === 1) r = 1; else if (x === 2) r = 2; else r = 3; return r;"
        ),
        Value::PosInt(2)
    );
}

#[test]
fn while_loop() {
    assert_eq!(
        testutil::run_val("let i = 0, s = 0; while (i < 5) { s += i; i += 1; } return s;"),
        testutil::num(10.0)
    );
}

#[test]
fn while_continue_retests() {
    assert_eq!(
        testutil::run_val(
            "let i = 0, s = 0; while (i < 5) { i++; if (i === 3) continue; s += i; } return s;"
        ),
        testutil::num(12.0)
    );
}

#[test]
fn for_with_expression_initializer() {
    assert_eq!(
        testutil::run_val("let i, s = 0; for (i = 0; i < 4; i++) { s += i; } return s;"),
        testutil::num(6.0)
    );
}

#[test]
fn do_while_loop() {
    assert_eq!(
        testutil::run_val("let n = 0; do { n += 1; } while (n < 3); return n;"),
        testutil::num(3.0)
    );
    assert_eq!(
        testutil::run_val("let n = 0; do { n += 1; } while (false); return n;"),
        testutil::num(1.0)
    );
}

#[test]
fn for_loop() {
    assert_eq!(
        testutil::run_val("let s = 0; for (let i = 0; i < 5; i++) { s += i; } return s;"),
        testutil::num(10.0)
    );
    assert_eq!(
        testutil::run_val("let i = 0; for (;;) { if (i >= 3) break; i++; } return i;"),
        testutil::num(3.0)
    );
}

#[test]
fn break_and_continue() {
    assert_eq!(
        testutil::run_val(
            "let s = 0; for (let i = 0; i < 10; i++) { if (i === 3) break; s += i; } return s;"
        ),
        testutil::num(3.0)
    );
    assert_eq!(
        testutil::run_val(
            "let s = 0; for (let i = 0; i < 5; i++) { if (i % 2 === 0) continue; s += i; } return s;"
        ),
        testutil::num(4.0)
    );
    assert_eq!(
        testutil::run_val(
            "let c = 0; for (let i = 0; i < 3; i++) { for (let j = 0; j < 3; j++) { if (j === 1) break; c++; } } return c;"
        ),
        testutil::num(3.0)
    );
}

#[test]
fn for_of_array() {
    assert_eq!(
        testutil::run_val("let s = 0; for (const x of [1, 2, 3, 4]) { s += x; } return s;"),
        testutil::num(10.0)
    );
    assert_eq!(
        testutil::run_val("let s = 0; for (let x of [10, 20]) s += x; return s;"),
        testutil::num(30.0)
    );
    assert_eq!(
        testutil::run_val("let s = 99; for (const x of []) s = 0; return s;"),
        Value::PosInt(99)
    );
}

#[test]
fn for_of_string_chars() {
    match testutil::run_val("let r = \"\"; for (const c of \"abc\") r = c + r; return r;") {
        Value::String(s) => assert_eq!(s.as_str(), "cba"),
        other => panic!("not a string: {other:?}"),
    }
}

#[test]
fn for_of_break_and_continue() {
    assert_eq!(
        testutil::run_val(
            "let s = 0; for (const x of [1, 2, 3, 4]) { if (x === 3) break; s += x; } return s;"
        ),
        testutil::num(3.0)
    );
    assert_eq!(
        testutil::run_val(
            "let s = 0; for (const x of [1, 2, 3, 4]) { if (x % 2 === 0) continue; s += x; } return s;"
        ),
        testutil::num(4.0)
    );
    assert_eq!(
        testutil::run_val(
            "let c = 0; for (const i of [1, 2, 3]) { for (const j of [1, 2, 3]) { if (j === 2) break; c++; } } return c;"
        ),
        testutil::num(3.0)
    );
}

#[test]
fn for_in_object_keys() {
    assert_eq!(
        testutil::run_ret(
            "let o = { a: 1, b: 2, c: 3 }; let r = \"\"; for (const k in o) { r = r + k; } return r;"
        ),
        serde_json::json!("abc")
    );
    assert_eq!(
        testutil::run_ret(
            "let o = { a: 1, b: 2, c: 3 }; let s = 0; for (const k in o) { s += o[k]; } return s;"
        ),
        serde_json::json!(6)
    );
}

#[test]
fn for_in_over_input() {
    let vm = testutil::run(
        "input.x = 1; input.y = 2; let n = 0; for (const k in input) n++; input.r = n;",
    );
    assert_eq!(input_val(&vm, "r"), testutil::num(2.0));
}

#[test]
fn switch_basic_and_fallthrough() {
    assert_eq!(
        testutil::run_val(
            "let r = 0; switch (2) { case 1: r = 1; break; case 2: r = 2; break; case 3: r = 3; break; } return r;"
        ),
        Value::PosInt(2)
    );
    assert_eq!(
        testutil::run_val(
            "let r = 0; switch (1) { case 1: r += 1; case 2: r += 10; break; case 3: r += 100; } return r;"
        ),
        testutil::num(11.0)
    );
    assert_eq!(
        testutil::run_val(
            "let r = 0; switch (9) { case 1: r = 1; break; default: r = 42; } return r;"
        ),
        Value::PosInt(42)
    );
    assert_eq!(
        testutil::run_val(
            "let r = 0; switch (1) { default: r = 42; break; case 1: r = 7; break; } return r;"
        ),
        Value::PosInt(7)
    );
    assert_eq!(
        testutil::run_val(
            "let r = 0; switch (\"1\") { case 1: r = 1; break; default: r = 2; } return r;"
        ),
        Value::PosInt(2)
    );
}

#[test]
fn switch_break_only_continue_escapes() {
    assert_eq!(
        testutil::run_val(
            "let s = 0; for (let i = 0; i < 3; i++) { switch (i) { case 1: break; default: s += i; } } return s;"
        ),
        testutil::num(2.0)
    );
    assert_eq!(
        testutil::run_val(
            "let s = 0; for (let i = 0; i < 4; i++) { switch (i) { case 2: continue; default: break; } s += i; } return s;"
        ),
        testutil::num(4.0)
    );
}

#[test]
fn switch_lexical_decls_share_block() {
    assert_eq!(
        testutil::run_val(
            "let r = 0; switch (1) { case 1: { let x = 5; r = x; break; } default: r = 0; } return r;"
        ),
        Value::PosInt(5)
    );
}

#[test]
fn continue_in_do_while_retests_condition() {
    // `continue` inside `do-while` jumps to the condition, which re-tests.
    assert_eq!(
        testutil::run_ret(
            "let i = 0; let s = 0; do { i++; if (i < 3) continue; s++; } while (i < 5); return { i, s };"
        ),
        serde_json::json!({"i": 5, "s": 3})
    );
    // i=1,2: continue (skip s++). i=3,4,5: s++ → s=3.
}

#[test]
fn switch_break_only_continue_skips_to_enclosing_loop() {
    // `continue` inside a `switch` (no loop on the switch itself) must
    // skip to the innermost enclosing loop, not error.
    assert_eq!(
        testutil::run_ret(
            "let s = 0; for (let i = 1; i <= 3; i++) { switch (i) { case 1: s++; break; case 2: continue; default: s += 10; } } return s;"
        ),
        serde_json::json!(11)
    );
    // i=1: s=1, break. i=2: continue → skip increment. i=3: s=11.
}
