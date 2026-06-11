//! 6_LANGUAGE Part B — `try` / `catch` / `throw`.
//!
//! Covers: throw/catch of arbitrary values, the `new Error(...)` object
//! shape, catchable runtime errors (materialized `{ name, message }`),
//! await-rejection delivery, the uncatchable set (`raise`, `OutOfFuel`),
//! handler-stack balance across `break`/`continue`/`return`, and the
//! `vm.throw_value` host API.

use crate::testutil::*;
use crate::vm::{ErrorKind, ResumeMode, StepResult, ThrowOutcome, VM, Value};
use serde_json::json;

// ── basic throw / catch ──────────────────────────────────────────────

#[test]
fn throw_string_caught() {
    assert_eq!(
        run_ret(r#"try { throw "boom"; } catch (e) { return e; } return "no";"#),
        json!("boom")
    );
}

#[test]
fn throw_object_caught() {
    assert_eq!(
        run_ret(r#"try { throw { code: 7 }; } catch (e) { return e.code; }"#),
        json!(7)
    );
}

#[test]
fn try_completes_without_throw() {
    assert_eq!(
        run_ret(r#"let r = 1; try { r = 2; } catch (e) { r = 3; } return r;"#),
        json!(2)
    );
}

#[test]
fn catch_without_binding() {
    assert_eq!(
        run_ret(r#"let r = 0; try { throw 1; } catch { r = 2; } return r;"#),
        json!(2)
    );
}

#[test]
fn catch_destructures_thrown_value() {
    assert_eq!(
        run_ret(r#"try { throw new Error("boom"); } catch ({ message }) { return message; }"#),
        json!("boom")
    );
}

#[test]
fn catch_binding_is_block_scoped() {
    let errs = compile_errs(r#"try { throw 1; } catch (e) {} return e;"#);
    assert!(
        errs.iter().any(|e| e.contains("undeclared variable `e`")),
        "got: {errs:?}"
    );
}

#[test]
fn throw_unwinds_called_frames() {
    assert_eq!(
        run_ret(
            r#"
            function g() { throw "deep"; }
            function f() { g(); return "no"; }
            try { f(); } catch (e) { return e; }
            "#
        ),
        json!("deep")
    );
}

#[test]
fn nested_try_rethrow_reaches_outer() {
    assert_eq!(
        run_ret(
            r#"
            try {
                try { throw "inner"; } catch (e) { throw e + "+re"; }
            } catch (e) { return e; }
            "#
        ),
        json!("inner+re")
    );
}

#[test]
fn throw_mid_expression_discards_temporaries() {
    // The error fires mid-expression (inside `1 + …`); the unwinder must
    // truncate the partial operand stack back to the TryEnter snapshot.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            try { a.push(1 + ({}).x.y); } catch (e) { a.push("c"); }
            a.push("end");
            return a;
            "#
        ),
        json!(["c", "end"])
    );
}

#[test]
fn try_inside_for_of_keeps_loop_state() {
    // for-of keeps [container, idx] on the stack across the body; the
    // handler snapshot sits above them, so catching must not disturb them.
    assert_eq!(
        run_ret(
            r#"
            const out = [];
            for (const x of [1, 2, 3]) {
                try {
                    if (x === 2) { throw "skip"; }
                    out.push(x * 10);
                } catch (e) { out.push(e); }
            }
            return out;
            "#
        ),
        json!([10, "skip", 30])
    );
}

// ── new Error(...) ───────────────────────────────────────────────────

#[test]
fn new_error_builds_name_message_object() {
    assert_eq!(
        run_ret(r#"const e = new Error("msg"); return [e.name, e.message];"#),
        json!(["Error", "msg"])
    );
}

#[test]
fn new_error_subclass_names() {
    assert_eq!(
        run_ret(
            r#"try { throw new TypeError("bad"); } catch (e) { return e.name + ": " + e.message; }"#
        ),
        json!("TypeError: bad")
    );
    assert_eq!(
        run_ret(r#"return new RangeError("r").name;"#),
        json!("RangeError")
    );
}

#[test]
fn new_error_no_args_empty_message() {
    assert_eq!(run_ret(r#"return new Error().message;"#), json!(""));
}

#[test]
fn new_error_coerces_message_to_string() {
    assert_eq!(run_ret(r#"return new Error(123).message;"#), json!("123"));
}

#[test]
fn new_error_rejects_extra_args() {
    let errs = compile_errs(r#"throw new Error("m", { cause: 1 });"#);
    assert!(
        errs.iter().any(|e| e.contains("at most one")),
        "got: {errs:?}"
    );
}

#[test]
fn new_non_error_still_rejected() {
    let errs = compile_errs(r#"const x = new Foo();"#);
    assert!(
        errs.iter().any(|e| e.contains("`new` is not supported")),
        "got: {errs:?}"
    );
}

// ── uncaught propagation ─────────────────────────────────────────────

#[test]
fn uncaught_throw_escalates_as_value_error() {
    let err = run_runtime_err(r#"throw new Error("boom");"#);
    assert_eq!(err.kind, ErrorKind::ValueError);
    assert!(matches!(err.resume, ResumeMode::NotResumable));
    assert!(
        err.message.contains("uncaught Error: boom"),
        "got: {}",
        err.message
    );
}

#[test]
fn uncaught_throw_of_plain_value() {
    let err = run_runtime_err(r#"throw 42;"#);
    assert_eq!(err.kind, ErrorKind::ValueError);
    assert!(
        err.message.contains("uncaught exception"),
        "got: {}",
        err.message
    );
}

// ── catchable runtime errors ─────────────────────────────────────────

#[test]
fn runtime_type_error_caught_with_name() {
    assert_eq!(
        run_ret(r#"try { return null.foo; } catch (e) { return e.name; }"#),
        json!("TypeError")
    );
}

#[test]
fn caught_error_message_carries_rendered_diagnostic() {
    let msg = run_ret(r#"try { null.foo; } catch (e) { return e.message; }"#);
    let msg = msg.as_str().unwrap();
    // The rendered diagnostic includes the message, line:col, and the
    // offending source line.
    assert!(msg.contains("cannot read property on null"), "got: {msg}");
    assert!(msg.contains("1:"), "got: {msg}");
    assert!(msg.contains("null.foo"), "got: {msg}");
}

#[test]
fn json_parse_fallback_idiom() {
    // The strongest trained idiom this feature exists for.
    assert_eq!(
        run_ret(r#"try { return JSON.parse("{bad"); } catch (e) { return "fallback"; }"#),
        json!("fallback")
    );
}

#[test]
fn coercion_type_error_caught() {
    assert_eq!(
        run_ret(r#"try { return [] - 1; } catch (e) { return e.name; }"#),
        json!("TypeError")
    );
}

#[test]
fn not_resumable_error_is_not_catchable() {
    // A NotResumable error must escalate even inside `try`. `++` on a
    // non-numeric local errors via IncLocal's peek (NotResumable).
    let err = run_runtime_err(r#"let x = {}; try { x++; } catch (e) { return "caught"; }"#);
    assert!(matches!(err.resume, ResumeMode::NotResumable));
}

// ── the uncatchable set ──────────────────────────────────────────────

#[test]
fn raise_is_not_catchable() {
    let (mut vm, effect) = run_to_effect(
        r#"
        try { raise("halt"); } catch (e) { return "caught"; }
        return "after";
        "#,
    );
    match effect {
        StepResult::Raise { condition, .. } => assert_eq!(condition, "halt"),
        other => panic!("expected Raise, got {other:?}"),
    }
    // Resuming continues past the raise — the catch never runs.
    vm.resume_raise(Value::Null);
    loop {
        match vm.step().unwrap() {
            StepResult::Done { value, .. } => {
                assert_eq!(value, Value::String("after".into()));
                break;
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}

#[test]
fn out_of_fuel_is_not_catchable() {
    let prog = compile_ok(r#"try { while (true) {} } catch (e) { return "caught"; }"#);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    vm.fuel = 1000;
    let err = loop {
        match vm.step() {
            Err(e) => break e,
            Ok(StepResult::Done { .. }) => panic!("the kill switch was trapped"),
            Ok(other) => panic!("unexpected effect: {other:?}"),
        }
    };
    assert_eq!(err.kind, ErrorKind::OutOfFuel);
}

// ── handler-stack balance across jumps ───────────────────────────────

#[test]
fn break_out_of_try_pops_handler() {
    // If `break` left the handler active, the error after the loop would
    // unwind back into the (dead) catch instead of escalating.
    let err = run_runtime_err(
        r#"
        while (true) { try { break; } catch (e) {} }
        return null.foo;
        "#,
    );
    assert_eq!(err.kind, ErrorKind::TypeError);
}

#[test]
fn continue_out_of_try_pops_handler() {
    let err = run_runtime_err(
        r#"
        for (let i = 0; i < 3; i++) { try { continue; } catch (e) {} }
        return null.foo;
        "#,
    );
    assert_eq!(err.kind, ErrorKind::TypeError);
}

#[test]
fn continue_through_switch_inside_try() {
    // `continue` resolves past the break-only switch context to the loop,
    // crossing one `try` boundary — exactly one TryExit must be emitted.
    let err = run_runtime_err(
        r#"
        for (let i = 0; i < 2; i++) {
            try {
                switch (i) { default: continue; }
            } catch (e) {}
        }
        return null.foo;
        "#,
    );
    assert_eq!(err.kind, ErrorKind::TypeError);
}

#[test]
fn return_out_of_try_pops_handler() {
    let err = run_runtime_err(
        r#"
        function f() { try { return 1; } catch (e) { return 2; } }
        f();
        return null.foo;
        "#,
    );
    assert_eq!(err.kind, ErrorKind::TypeError);
}

#[test]
fn break_inside_try_stays_caught_when_error_precedes() {
    // Sanity mirror of the above: a throw before the break is still caught.
    assert_eq!(
        run_ret(
            r#"
            let r = "none";
            while (true) {
                try { throw "in"; } catch (e) { r = e; }
                break;
            }
            return r;
            "#
        ),
        json!("in")
    );
}

// ── async integration ────────────────────────────────────────────────

#[test]
fn await_rejection_delivers_raw_reason_to_catch() {
    let prog = compile_ok(
        r#"
        try { await tools.f(); return "ok"; }
        catch (e) { return e.reason; }
        "#,
    );
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = match vm.step().unwrap() {
        StepResult::Pending { calls } => calls,
        other => panic!("expected Pending, got {other:?}"),
    };
    let reason = vm
        .json_to_stack_value(&json!({ "reason": "down" }), 0)
        .unwrap();
    vm.reject_promise(calls[0].promise, reason).unwrap();
    match vm.step().unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::String("down".into())),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn await_rejection_without_try_escalates_unchanged() {
    let prog = compile_ok(r#"return await tools.f();"#);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = match vm.step().unwrap() {
        StepResult::Pending { calls } => calls,
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.reject_promise(calls[0].promise, Value::String("down".into()))
        .unwrap();
    let err = vm.step().unwrap_err();
    assert_eq!(err.kind, ErrorKind::ValueError);
    assert!(matches!(err.resume, ResumeMode::PushValueThenContinue));
    assert!(err.message.contains("rejected"), "got: {}", err.message);
}

#[test]
fn host_throw_value_caught_by_program() {
    let prog = compile_ok(
        r#"
        try { await tools.f(); } catch (e) { return "caught:" + e; }
        return "done";
        "#,
    );
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Pending { .. } => {}
        other => panic!("expected Pending, got {other:?}"),
    }
    // The host converts the failed call into a program-visible exception.
    match vm.throw_value(Value::String("X".into())) {
        ThrowOutcome::Caught => {}
        ThrowOutcome::Uncaught(_) => panic!("expected the handler to catch"),
    }
    match vm.step().unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::String("caught:X".into())),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn host_throw_value_uncaught_reports_back() {
    let prog = compile_ok(r#"return await tools.f();"#);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Pending { .. } => {}
        other => panic!("expected Pending, got {other:?}"),
    }
    match vm.throw_value(Value::String("X".into())) {
        ThrowOutcome::Uncaught(v) => assert_eq!(v, Value::String("X".into())),
        ThrowOutcome::Caught => panic!("no handler should be active"),
    }
}

// ── scoping / hoisting interactions ──────────────────────────────────

#[test]
fn var_hoists_out_of_try_and_catch() {
    assert_eq!(
        run_ret(
            r#"
            try { var x = 1; throw "t"; } catch (e) { var y = 2; }
            return [x, y];
            "#
        ),
        json!([1, 2])
    );
}

#[test]
fn function_declared_inside_try_is_hoisted() {
    assert_eq!(
        run_ret(
            r#"
            try { return g(); function g() { return "fn"; } }
            catch (e) { return "c"; }
            "#
        ),
        json!("fn")
    );
}

#[test]
fn closures_capture_per_iteration_catch_binding() {
    // The catch binding is a per-iteration `let`-like binding: closures
    // created in different iterations must see distinct values.
    assert_eq!(
        run_ret(
            r#"
            const fs = [];
            for (let i = 0; i < 2; i++) {
                try { throw i; } catch (e) { fs.push(() => e); }
            }
            const a = fs[0];
            const b = fs[1];
            return [a(), b()];
            "#
        ),
        json!([0, 1])
    );
}

// ── rejections that stay rejected ────────────────────────────────────

#[test]
fn finally_is_a_compile_error() {
    let errs = compile_errs(r#"try { f(); } catch (e) {} finally { g(); }"#);
    assert!(
        errs.iter()
            .any(|e| e.contains("`finally` is not supported")),
        "got: {errs:?}"
    );
    let errs = compile_errs(r#"try { f(); } finally { g(); }"#);
    assert!(
        errs.iter()
            .any(|e| e.contains("`finally` is not supported")),
        "got: {errs:?}"
    );
}
