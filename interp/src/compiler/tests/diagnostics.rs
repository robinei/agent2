//! Compile-error tests — assert on diagnostic messages for unsupported
//! syntax, type errors, arity violations, and undeclared variables.

use crate::compiler::compile;
use crate::testutil;
use crate::testutil::eval;

#[test]
fn unsupported_statement_errors() {
    let errs = compile("class C {}").expect_err("should not compile");
    assert_eq!(errs.len(), 1);
    let rendered = errs[0].render("class C {}");
    assert!(rendered.starts_with("1:1: "), "got: {rendered}");
}

#[test]
fn syntax_error_is_reported() {
    let errs = compile("1 +* 2;").expect_err("syntax error");
    assert!(!errs.is_empty());
}

#[test]
fn diagnostics_for_unsupported() {
    for src in [
        "x;",
        "x = 1;",
        "i++;",
        "tools;",
        "tools.send;",
        "raise(x);",
        "raise();",
        "raise(\"name\", 1, 2);", // at most one payload
        "Math.random();",
        "Math.pow(1);",
        "f(...args);",
        "new Foo();",
        "class C {}",
    ] {
        assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
    }
}

#[test]
fn builtin_arity_is_enforced_from_meta() {
    for src in [
        "Math.pow(1);",
        "Math.pow(1, 2, 3);",
        "Math.abs();",
        "\"x\".slice();",
        "\"x\".slice(1, 2, 3);",
        "[1].pop(2);",
        "Object.keys();",
        "Number.parseInt(1, 2, 3);",
    ] {
        assert!(
            compile(src).is_err(),
            "expected `{src}` to fail arity check"
        );
    }
    assert!(compile("[1].push();").is_ok());
    let errs = compile("\"x\".slice(1, 2, 3);").expect_err("too many args");
    let msg = &errs[0].message;
    assert!(msg.contains("`slice`"), "got: {msg}");
    assert!(msg.contains("1 to 2"), "got: {msg}");
    assert_eq!(eval("Math.max()"), testutil::num(f64::NEG_INFINITY));
    assert_eq!(eval("Math.max(1, 2, 3, 4, 5)"), testutil::num(5.0));
}

#[test]
fn for_of_in_diagnostics() {
    // The bare-assignment-target head form stays rejected (destructuring
    // *declaration* heads compile — see control_flow.rs).
    for src in [
        "for (x of [1]) {}",
        "let a, b; for ([a, b] of [[1, 2]]) {}",
        "for (const x of [1], y = 2) {}",
    ] {
        assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
    }
}

#[test]
fn destructuring_params_compile() {
    // Previously rejected (and before that, a compiler panic) — now supported.
    // Behavioral coverage lives in functions_closures.rs.
    for src in [
        "function f({a, b}) { return a; } return f({a: 1});",
        "function f([a]) { return a; } return f([1]);",
        "function f({a} = {a: 5}) { return a; } return f();",
        "const f = ({a}) => a; return f({a: 7});",
        "function f(...[a, b]) { return a; } return f(1, 2);",
    ] {
        assert!(compile(src).is_ok(), "expected `{src}` to compile");
    }
}

#[test]
fn switch_continue_outside_loop_errors() {
    assert!(compile("switch (1) { case 1: continue; }").is_err());
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
fn unsigned_right_shift_is_supported() {
    // `>>>` and `>>>=` are supported (64-bit zero-fill right shift).
    let v = testutil::run_ret("return -1 >>> 0;");
    // -1 as u64 is 0xFFFFFFFFFFFFFFFF, shifted 0 = same bits → large positive number
    assert!(v.is_number());
    let v = testutil::run_ret("let x = -8; x >>>= 2; return x;");
    // -8 = 0xFFFFFFFFFFFFFFF8 as u64 >> 2 = 0x3FFFFFFFFFFFFFFE
    assert!(v.is_number());
}
