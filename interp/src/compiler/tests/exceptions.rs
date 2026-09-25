//! 6_LANGUAGE Part B — `try` / `catch` / `throw`.
//!
//! Covers: throw/catch of arbitrary values, the `new Error(...)` object
//! shape, catchable runtime errors (materialized `{ name, message }`),
//! await-rejection delivery, the uncatchable set (`raise`; fuel
//! exhaustion is a `StepResult`, invisible to programs),
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
    // `e` outside the catch block resolves via `PushName` and fails at
    // runtime with a `ReferenceError` — matching JS semantics.
    assert_eq!(
        run_err_kind(r#"try { throw 1; } catch (e) {} return e;"#),
        ErrorKind::ReferenceError
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
fn new_error_carries_its_cause() {
    // **This used to be a compile error.** The dedicated construction path
    // could only take a message, so `new Error("m", { cause: e })` — plain
    // JS, and the only reason anyone passes a second argument — was refused
    // outright. With that path gone the choice was to accept the bag and
    // drop it, which would leave `e.cause` undefined with nothing said, or
    // to honour it. It is honoured.
    assert_eq!(
        run_ret(r#"return new Error("m", { cause: 42 }).cause;"#),
        json!(42)
    );
    // The bare-call form is legal JS for an error class and reaches the
    // same constructor body, so it answers the same.
    assert_eq!(
        run_ret(r#"return TypeError("m", { cause: "why" }).cause;"#),
        json!("why")
    );
    // No bag, no key: `Object.keys` must not report a field the program
    // never set, and `JSON.stringify(e)` must not grow a `"cause": null`.
    assert_eq!(
        run_ret(r#"return Object.keys(new Error("m"));"#),
        json!(["name", "message"])
    );
    // A bag without a `cause` is the same: an options object is not itself
    // a cause.
    assert_eq!(
        run_ret(r#"return Object.keys(new Error("m", { other: 1 }));"#),
        json!(["name", "message"])
    );
    // Three arguments is past the row's bound, and still refused — the
    // bound now lives in the registry row, not in a check of its own.
    let errs = compile_errs(r#"throw Error("m", {}, 3);"#);
    assert!(
        errs.iter().any(|e| e.contains("`Error` expects 0 to 2")),
        "got: {errs:?}"
    );
}

#[test]
fn new_non_error_still_rejected() {
    // `new Foo()` now compiles (the undeclared identifier `Foo` resolves to
    // `PushName` at runtime). The runtime `PushName` raises `ReferenceError`.
    assert!(
        !compile_ok("const x = new Foo();").code.is_empty(),
        "expected to compile"
    );
    assert_eq!(
        run_err_kind("const x = new Foo();"),
        ErrorKind::ReferenceError
    );
}

// ── uncaught propagation ─────────────────────────────────────────────

#[test]
fn uncaught_throw_escalates_as_uncaught_exception() {
    let err = run_runtime_err(r#"throw new Error("boom");"#);
    assert_eq!(err.kind, ErrorKind::UncaughtException);
    assert!(matches!(err.resume, ResumeMode::NotResumable));
    assert!(
        err.message.contains("uncaught Error: boom"),
        "got: {}",
        err.message
    );
    // The thrown value itself survives structurally, not just a rendering.
    assert!(
        matches!(err.payload, Some(Value::Object(_))),
        "got: {:?}",
        err.payload
    );
}

#[test]
fn uncaught_throw_of_plain_value() {
    let err = run_runtime_err(r#"throw 42;"#);
    assert_eq!(err.kind, ErrorKind::UncaughtException);
    assert!(
        err.message.contains("uncaught exception"),
        "got: {}",
        err.message
    );
    assert_eq!(err.payload, Some(Value::PosInt(42)));
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
    assert!(
        msg.contains("cannot read property 'foo' on null"),
        "got: {msg}"
    );
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
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::String("after".into()));
        }
        other => panic!("unexpected effect: {other:?}"),
    }
}

#[test]
fn out_of_fuel_is_invisible_to_programs() {
    // Fuel exhaustion is a StepResult, not an error: running dry inside a
    // `try` never reaches the `catch`, and the next slice resumes exactly
    // where the last one left off.
    let prog = compile_ok(
        r#"
        let n = 0;
        try { for (let i = 0; i < 100; i++) { n += 1; } } catch (e) { return "caught"; }
        return n;
        "#,
    );
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let value = loop {
        match vm.step(7).unwrap() {
            StepResult::OutOfFuel => continue,
            StepResult::Done { value, .. } => break value,
            other => panic!("unexpected effect: {other:?}"),
        }
    };
    assert_eq!(
        vm.stack_value_to_json(&value, 0).unwrap(),
        serde_json::json!(100)
    );
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
    let calls = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => calls,
        other => panic!("expected Pending, got {other:?}"),
    };
    let reason = vm
        .json_to_stack_value(&json!({ "reason": "down" }), 0)
        .unwrap();
    vm.reject_promise(calls[0].promise, reason).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::String("down".into())),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn await_rejection_without_try_escalates_unchanged() {
    let prog = compile_ok(r#"return await tools.f();"#);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => calls,
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.reject_promise(calls[0].promise, Value::String("down".into()))
        .unwrap();
    let err = vm.step(u64::MAX).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ValueError);
    assert!(matches!(err.resume, ResumeMode::PushValueThenContinue));
    assert_eq!(err.message, "down", "got: {}", err.message);
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
    match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { .. } => {}
        other => panic!("expected Pending, got {other:?}"),
    }
    // The host converts the failed call into a program-visible exception.
    match vm.throw_value(Value::String("X".into())) {
        ThrowOutcome::Caught => {}
        ThrowOutcome::Uncaught(_) => panic!("expected the handler to catch"),
    }
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::String("caught:X".into())),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn host_throw_value_uncaught_reports_back() {
    let prog = compile_ok(r#"return await tools.f();"#);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
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

// ── finally (codegen duplication: normal + unwind paths) ─────────────

#[test]
fn finally_runs_on_normal_path() {
    assert_eq!(
        run_ret(r#"const a = []; try { a.push(1); } finally { a.push(2); } return a;"#),
        json!([1, 2])
    );
}

#[test]
fn finally_runs_after_catch() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            try { throw 1; } catch (e) { a.push("c"); } finally { a.push("f"); }
            return a;
            "#
        ),
        json!(["c", "f"])
    );
}

#[test]
fn finally_runs_on_normal_path_with_catch_not_taken() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            try { a.push("b"); } catch (e) { a.push("c"); } finally { a.push("f"); }
            return a;
            "#
        ),
        json!(["b", "f"])
    );
}

#[test]
fn finally_runs_then_rethrows_without_catch() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            try {
                try { throw "x"; } finally { a.push("f"); }
            } catch (e) { a.push(e); }
            return a;
            "#
        ),
        json!(["f", "x"])
    );
}

#[test]
fn exception_in_catch_still_runs_finally() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            try {
                try { throw 1; } catch (e) { throw 2; } finally { a.push("f"); }
            } catch (e) { a.push(e); }
            return a;
            "#
        ),
        json!(["f", 2])
    );
}

#[test]
fn uncaught_rethrow_after_finally() {
    let err = run_runtime_err(r#"try { throw "boom"; } finally {}"#);
    assert_eq!(err.kind, ErrorKind::UncaughtException);
    assert_eq!(err.payload, Some(Value::String("boom".into())));
}

#[test]
fn loop_with_break_inside_finally_is_fine() {
    // break targeting a loop declared INSIDE the finally block is legal.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            try {} finally {
                for (let i = 0; i < 5; i++) { if (i === 1) { break; } a.push(i); }
            }
            return a;
            "#
        ),
        json!([0])
    );
}

#[test]
fn closure_inside_finally_works_on_both_paths() {
    // A function body inside a duplicated finally block is emitted twice
    // under the same entry label; references resolve to one copy.
    assert_eq!(
        run_ret(
            r#"
            let r = 0;
            try { throw 1; } catch (e) {} finally { const g = (x) => x + 1; r = g(41); }
            return r;
            "#
        ),
        json!(42)
    );
}

#[test]
fn return_in_function_inside_finally_is_fine() {
    assert_eq!(
        run_ret(
            r#"
            let r;
            try {} finally { const f = () => { return 7; }; r = f(); }
            return r;
            "#
        ),
        json!(7)
    );
}

// ── finally: break/continue across and out of `finally` (Part B2) ────
//
// JS completion-value semantics: an early exit crossing a finalizer runs
// the finally block on its way out (via the exit stub), and a jump *from*
// a finally block overrides whatever completion was pending.

#[test]
fn break_crossing_finally_runs_block() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            while (true) { try { a.push("b"); break; } finally { a.push("f"); } }
            return a;
            "#
        ),
        json!(["b", "f"])
    );
}

#[test]
fn continue_crossing_finally_runs_block_each_iteration() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            for (let i = 0; i < 3; i++) {
                try { if (i === 1) { continue; } a.push(i); } finally { a.push("f"); }
            }
            return a;
            "#
        ),
        json!([0, "f", "f", 2, "f"])
    );
}

#[test]
fn break_crossing_two_nested_finallys_innermost_first() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            while (true) {
                try {
                    try { break; } finally { a.push("inner"); }
                } finally { a.push("outer"); }
            }
            return a;
            "#
        ),
        json!(["inner", "outer"])
    );
}

#[test]
fn throw_in_exit_path_finally_overrides_break() {
    // The finally on the break's way out throws: the exception wins (the
    // break is abandoned) and is catchable by the enclosing handler.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            while (true) {
                try {
                    try { break; } finally { throw "x"; }
                } catch (e) { a.push(e); break; }
            }
            return a;
            "#
        ),
        json!(["x"])
    );
}

#[test]
fn finally_break_swallows_pending_exception() {
    // A jump out of the unwind-path copy discards the pending thrown value.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            while (true) { try { throw "boom"; } finally { a.push("f"); break; } }
            a.push("after");
            return a;
            "#
        ),
        json!(["f", "after"])
    );
}

#[test]
fn finally_continue_overrides_pending_break() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            for (let i = 0; i < 3; i++) { a.push(i); try { break; } finally { continue; } }
            return a;
            "#
        ),
        json!([0, 1, 2])
    );
}

#[test]
fn finally_break_overrides_pending_continue() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            for (let i = 0; i < 3; i++) { a.push(i); try { continue; } finally { break; } }
            return a;
            "#
        ),
        json!([0])
    );
}

#[test]
fn switch_break_crossing_finally() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            switch (1) {
                case 1: try { a.push("b"); break; } finally { a.push("f"); }
                case 2: a.push("fell");
            }
            return a;
            "#
        ),
        json!(["b", "f"])
    );
}

#[test]
fn break_from_catch_crossing_finally() {
    // The exit initiates in the catch clause; the finalizer entry is still
    // open (it wraps the catch) and must run on the way out.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            while (true) {
                try { throw 1; } catch (e) { a.push("c"); break; } finally { a.push("f"); }
            }
            return a;
            "#
        ),
        json!(["c", "f"])
    );
}

#[test]
fn break_inside_try_within_unwind_finally() {
    // Pop-ordering soundness: the inner handler's snapshot includes the
    // pending thrown value beneath the unwind copy, so the break must
    // TryExit the inner entry *before* popping the pending value.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            while (true) {
                try { throw "t"; } finally {
                    try { a.push("inner"); break; } catch (e) { a.push("nope"); }
                }
            }
            a.push("after");
            return a;
            "#
        ),
        json!(["inner", "after"])
    );
}

#[test]
fn catch_inside_unwind_finally_keeps_pending_value_intact() {
    // An inner try/catch fully handled inside the unwind copy must leave
    // the pending thrown value untouched beneath it for the rethrow.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            let caught = null;
            try {
                try { throw "boom"; } finally {
                    try { throw "inner"; } catch (e) { a.push(e); }
                    a.push("f");
                }
            } catch (e) { caught = e; }
            return [a, caught];
            "#
        ),
        json!([["inner", "f"], "boom"])
    );
}

#[test]
fn closure_in_finally_with_three_copies() {
    // The finally block is emitted three times here (normal path, unwind
    // path, and the break's exit stub); the closure inside resolves to the
    // last-emitted body on every path (label maps are last-wins).
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            let caught = null;
            try {
                let i = 0;
                while (true) {
                    i = i + 1;
                    try {
                        if (i === 2) { throw "t"; }
                        if (i === 3) { break; }
                    } catch (e) {
                        if (i === 2) { throw e; }
                    } finally {
                        const g = (x) => x * 10 + i;
                        a.push(g(i));
                    }
                }
            } catch (e) { caught = e; }
            return [a, caught];
            "#
        ),
        json!([[11, 22], "t"])
    );
}

#[test]
fn for_of_break_crossing_finally_keeps_loop_state() {
    // for-of keeps [container, idx] on the operand stack across the body;
    // routing the break through the exit stub must land at the loop end
    // with exactly those slots intact.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            for (const x of [1, 2, 3]) {
                try { if (x === 2) { break; } a.push(x); } finally { a.push("f"); }
            }
            return a;
            "#
        ),
        json!([1, "f", "f"])
    );
}

#[test]
fn for_of_continue_crossing_finally_keeps_loop_state() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            for (const x of [1, 2, 3]) {
                try { if (x !== 2) { continue; } a.push(x); } finally { a.push("f"); }
            }
            return a;
            "#
        ),
        json!(["f", 2, "f", "f"])
    );
}

#[test]
fn nested_try_finally_inside_exit_path_finally() {
    // The exit-stub copy of F contains its own try/finally; the inner
    // lowering (and its own copies) nest inside the stub.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            while (true) {
                try { break; } finally {
                    try { a.push("x"); } finally { a.push("y"); }
                    a.push("z");
                }
            }
            return a;
            "#
        ),
        json!(["x", "y", "z"])
    );
}

#[test]
fn continue_crossing_switch_then_finally() {
    // continue from inside a switch inside a try/finally: pops the
    // discriminant residue, then detours through the finally stub.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            for (let i = 0; i < 3; i++) {
                try {
                    switch (i) { case 1: continue; }
                    a.push(i);
                } finally { a.push("f"); }
            }
            return a;
            "#
        ),
        json!([0, "f", "f", 2, "f"])
    );
}

#[test]
fn continue_crossing_finally_then_switch() {
    // The reverse nesting: try/finally inside a switch case. The stub's
    // onward transfer pops the discriminant after the finally has run.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            for (let i = 0; i < 3; i++) {
                switch (i) {
                    case 1: try { continue; } finally { a.push("f"); }
                }
                a.push(i);
            }
            return a;
            "#
        ),
        json!([0, "f", 2])
    );
}

// ── finally: return across and inside `finally` (Part B2 Step 2) ─────
//
// A `return` crossing a finalizer spills its value to the reserved frame
// slot and detours through the finally stubs; a `return` from inside a
// finally block overrides whatever completion was pending.

#[test]
fn return_value_evaluated_before_finally_runs() {
    // The classic: the return value is captured before the finally mutates
    // the variable.
    assert_eq!(
        run_ret(r#"function f() { let x = 1; try { return x; } finally { x = 2; } } return f();"#),
        json!(1)
    );
}

#[test]
fn return_crossing_two_finallys_innermost_first() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            function f() {
                try {
                    try { return "v"; } finally { a.push("inner"); }
                } finally { a.push("outer"); }
            }
            const r = f();
            return [a, r];
            "#
        ),
        json!([["inner", "outer"], "v"])
    );
}

#[test]
fn finally_return_overrides_pending_return() {
    assert_eq!(
        run_ret(r#"function f() { try { return 1; } finally { return 2; } } return f();"#),
        json!(2)
    );
}

#[test]
fn finally_return_swallows_pending_exception() {
    assert_eq!(
        run_ret(r#"function f() { try { throw "boom"; } finally { return 2; } } return f();"#),
        json!(2)
    );
}

#[test]
fn finally_return_overrides_pending_break() {
    assert_eq!(
        run_ret(
            r#"
            function f() {
                while (true) { try { break; } finally { return 1; } }
                return 2;
            }
            return f();
            "#
        ),
        json!(1)
    );
}

#[test]
fn throw_in_finally_overrides_pending_return() {
    assert_eq!(
        run_ret(
            r#"
            function f() { try { return 1; } finally { throw "e"; } }
            let c = null;
            try { f(); } catch (e) { c = e; }
            return c;
            "#
        ),
        json!("e")
    );
}

#[test]
fn return_inside_normal_path_finally() {
    assert_eq!(
        run_ret(r#"function f() { try {} finally { return 7; } } return f();"#),
        json!(7)
    );
}

#[test]
fn failed_return_inside_unwind_finally_keeps_pending_value() {
    // Spill soundness: evaluating the return value throws *before* the
    // spill/exit, inside the unwind copy — the pending thrown value beneath
    // must survive intact for the rethrow.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            function g() { throw "g"; }
            function f() {
                try { throw "boom"; } finally {
                    try { return g(); } catch (e) { a.push(e); }
                    a.push("f");
                }
            }
            let c = null;
            try { f(); } catch (e) { c = e; }
            return [a, c];
            "#
        ),
        json!([["g", "f"], "boom"])
    );
}

#[test]
fn return_inside_finally_crossing_another_finally() {
    // The override return spills over the pending one (shared slot,
    // last-wins) and detours through the inner finalizer's stub.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            function f() {
                try { return 1; } finally {
                    a.push("f1");
                    try { return 2; } finally { a.push("f2"); }
                }
            }
            return [f(), a];
            "#
        ),
        json!([2, ["f1", "f2"]])
    );
}

#[test]
fn return_crossing_finally_inside_for_of() {
    // The for-of's stack-resident [container, idx] state is dead on this
    // path; frame teardown discards it along with any pending residues.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            function f() {
                for (const x of [1, 2, 3]) {
                    try { if (x === 2) { return x * 10; } } finally { a.push("f"); }
                }
                return 0;
            }
            const r = f();
            return [a, r];
            "#
        ),
        json!([["f", "f"], 20])
    );
}

#[test]
fn top_level_return_crossing_finally_without_locals() {
    // Root frame with no declared locals: the spill slot is materialized by
    // inserting the prologue `EnterFrame` that was otherwise skipped.
    assert_eq!(run_ret(r#"try { return 42; } finally {}"#), json!(42));
}

#[test]
fn top_level_return_crossing_finally_with_locals() {
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            try { a.push(1); return a; } finally { a.push(2); }
            "#
        ),
        json!([1, 2])
    );
}

#[test]
fn return_crossing_finally_in_closure_with_upvals() {
    // The spill slot lands past the upval slots; the spilled value is the
    // captured cell's value at return time, not after the finally mutates
    // it.
    assert_eq!(
        run_ret(
            r#"
            function outer() {
                let v = 5;
                const inner = () => { try { return v; } finally { v = 6; } };
                const r = inner();
                return [r, v];
            }
            return outer();
            "#
        ),
        json!([5, 6])
    );
}

#[test]
fn return_with_arguments_and_spill_slot() {
    // `uses_arguments` frames build the args cache in EnterFrame; the
    // patched-in spill slot must not disturb that.
    assert_eq!(
        run_ret(r#"function f(p) { try { return arguments[0] + p; } finally {} } return f(20);"#),
        json!(40)
    );
}

#[test]
fn break_escaping_normal_path_finally() {
    // A break out of the finally block on the normal path: nothing is
    // pending; the block's trailing code is simply skipped.
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            while (true) { try { a.push("b"); } finally { a.push("f"); break; } }
            a.push("after");
            return a;
            "#
        ),
        json!(["b", "f", "after"])
    );
}

#[test]
fn break_crossing_plain_catch_still_works() {
    // The restriction is finally-specific: crossing a catch-only try stays
    // legal (covered more fully in the handler-balance tests above).
    assert_eq!(
        run_ret(
            r#"
            const a = [];
            while (true) { try { a.push(1); break; } catch (e) {} }
            return a;
            "#
        ),
        json!([1])
    );
}

/// **An async IIFE is not awaited, and nothing said so.**
///
/// `(async () => { … })();` at the top level builds a promise nobody
/// holds: the block reaches its end while the `await`s inside are
/// still parked, and the reply is logged with its calls in flight. A
/// run on 2026-09-20 spent two replies working that out from "issued;
/// no result recorded; may have happened" and failed for taking five
/// programs to ask one question. One run in 300 — kept because the
/// shape is syntactic and the contract is not about frequency. The card's rule for the whole class
/// is that a difference "stops the block and says what to write
/// instead"; this was the case that silently did not.
#[test]
fn an_unawaited_async_iife_is_refused_with_the_way_round_it() {
    for src in [
        "(async () => { await f(); })();",
        "(async function () { await f(); })();",
    ] {
        let errs = crate::compile(src).expect_err("refused");
        let msg = format!("{errs:?}");
        assert!(msg.contains("never awaited"), "names it: {msg}");
        assert!(
            msg.contains("at the top level"),
            "and says what to write instead: {msg}"
        );
    }

    // Awaited, it is an ordinary expression and compiles.
    crate::compile("await (async () => { return 1; })();").expect("awaited is fine");
    // So is one whose value is kept.
    crate::compile("const p = (async () => { return 1; })();").expect("bound is fine");
    // A plain IIFE is not async and has nothing to wait for.
    crate::compile("(() => { return 1; })();").expect("sync IIFE is fine");
    // And an un-awaited *call* stays legal — that is how `tell` works.
    crate::compile("tell(\"hi\");").expect("an unawaited call is the documented way");
}

// ── errors are Errors ────────────────────────────────────────────────
//
// Added 2026-09-25. Before it, `catch (e) { if (e instanceof Error) … }`
// took the wrong branch for *every* error a program could catch, and
// `` `${e}` `` logged `[object Object]` — so a model that wrote the
// defensive JavaScript it had written everywhere else got a silent miss
// and an unreadable log line. Each test below fails on the old dialect.

#[test]
fn every_error_a_program_can_catch_is_instanceof_error() {
    // Three sources, one prototype. The VM's own raise:
    assert_eq!(
        run_ret("try { null.y; } catch (e) { return e instanceof Error; }"),
        json!(true)
    );
    // …including the non-JS kinds this dialect raises (`ValueError`):
    assert_eq!(
        run_ret(r#"try { JSON.parse("{"); } catch (e) { return e instanceof Error; }"#),
        json!(true)
    );
    // A constructed error, with `new` and without:
    assert_eq!(
        run_ret(r#"return new Error("x") instanceof Error;"#),
        json!(true)
    );
    assert_eq!(
        run_ret(r#"return TypeError("x") instanceof Error;"#),
        json!(true)
    );
    assert_eq!(
        run_ret(r#"return new RangeError("x") instanceof Error;"#),
        json!(true)
    );
    // And one that was thrown and caught, which is the shape that matters.
    assert_eq!(
        run_ret(r#"try { throw new TypeError("x"); } catch (e) { return e instanceof Error; }"#),
        json!(true)
    );
    // An error is still an object: the chain reaches `Object.prototype`.
    assert_eq!(
        run_ret(r#"return new Error("x") instanceof Object;"#),
        json!(true)
    );
}

#[test]
fn an_error_stringifies_as_name_colon_message() {
    // `Error.prototype.toString`'s three cases, through both coercions a
    // program actually writes.
    assert_eq!(
        run_ret(r#"return `${new TypeError("bad input")}`;"#),
        json!("TypeError: bad input")
    );
    assert_eq!(
        run_ret(r#"return "" + new TypeError("bad input");"#),
        json!("TypeError: bad input")
    );
    assert_eq!(run_ret(r#"return String(new Error(""));"#), json!("Error"));
    assert_eq!(run_ret(r#"return `${new Error()}`;"#), json!("Error"));
    // A VM-raised error carries its rendered diagnostic as the message, so
    // the coercion must at least lead with the kind and the cause.
    let s = run_ret("try { null.y; } catch (e) { return `${e}`; }");
    let s = s.as_str().expect("a string").to_owned();
    assert!(
        s.starts_with("TypeError: ") && s.contains("cannot read property 'y' on null"),
        "got {s}"
    );
}

#[test]
fn an_ordinary_object_that_looks_like_an_error_is_not_one() {
    // **The rule duck-typing would break.** An error is recognized by its
    // prototype, never by having a `name` and a `message` — a tool result
    // with those two fields is a record, and a program that formats it
    // must still see `[object Object]`, not a sentence.
    assert_eq!(
        run_ret(r#"return `${{ name: "x", message: "y" }}`;"#),
        json!("[object Object]")
    );
    assert_eq!(
        run_ret(r#"return ({ name: "x", message: "y" }) instanceof Error;"#),
        json!(false)
    );
    // Nor is a bare object, which has no fields to be mistaken for either.
    assert_eq!(run_ret("return `${{}}`;"), json!("[object Object]"));
    assert_eq!(run_ret("return ({}) instanceof Error;"), json!(false));
}

#[test]
fn an_error_is_still_a_two_field_object() {
    // The proto link is the only thing added: the fields stay own and
    // enumerable, so `Object.keys`, `JSON.stringify` and the `e.name`
    // branch the card recommends all behave as before.
    assert_eq!(
        run_ret(r#"return Object.keys(new TypeError("x"));"#),
        json!(["name", "message"])
    );
    assert_eq!(
        run_ret(r#"return JSON.stringify(new TypeError("x"));"#),
        json!(r#"{"name":"TypeError","message":"x"}"#)
    );
    assert_eq!(
        run_ret("try { null.y; } catch (e) { return e.name; }"),
        json!("TypeError")
    );
    // The prototype itself stays empty — its methods are virtual rungs, so
    // nothing new shows up in a `for-in` over an error.
    assert_eq!(run_ret("return Object.keys(Error.prototype);"), json!([]));
}

#[test]
fn error_is_a_constructor_not_the_function_constructor() {
    // `Error` used to resolve to `Builtin::FunctionCtor`, which is why
    // `instanceof` walked to `Function.prototype` and missed.
    assert_eq!(run_ret("return typeof Error;"), json!("function"));
    assert_eq!(
        run_ret(r#"return new Error("x").constructor === Error;"#),
        json!(true)
    );
    assert_eq!(
        run_ret(r#"return new Error("x").constructor.name;"#),
        json!("Error")
    );
    // A type error's immediate prototype is `TypeError.prototype`, which
    // chains to `Error.prototype` — the rung that carries the hierarchy.
    assert_eq!(
        run_ret(
            "try { null.y; } catch (e) { return Object.getPrototypeOf(e) === TypeError.prototype; }"
        ),
        json!(true)
    );
    assert_eq!(
        run_ret("return Object.getPrototypeOf(TypeError.prototype) === Error.prototype;"),
        json!(true)
    );
    // Called through a value (not the compiler's literal-name fast path),
    // `Error` still builds an error.
    assert_eq!(
        run_ret(r#"const E = Error; return E("x") instanceof Error;"#),
        json!(true)
    );
    assert_eq!(
        run_ret(r#"const E = Error; return `${E("x")}`;"#),
        json!("Error: x")
    );
}

// ── the error classes ────────────────────────────────────────────────
//
// Added 2026-09-25, the day after every error became an `Error`. That
// change gave them all *one* prototype, so `e instanceof TypeError` was
// false even of a type error: a program that wanted to handle a bad
// value differently from a bad type had nothing to branch on but
// `e.name`. Each test below fails with the single shared prototype.

#[test]
fn an_error_is_an_instance_of_its_own_class_and_of_error() {
    // A VM raise, and the class it names:
    assert_eq!(
        run_ret(
            "try { null.y; } catch (e) { return [e instanceof TypeError, e instanceof Error]; }"
        ),
        json!([true, true])
    );
    // `ValueError` is this dialect's own kind, and the second commonest
    // one the VM raises — `JSON.parse` of a truncated document is one.
    assert_eq!(
        run_ret(
            r#"try { JSON.parse("{"); } catch (e) { return [e.name, e instanceof ValueError, e instanceof Error]; }"#
        ),
        json!(["ValueError", true, true])
    );
    // A `ReferenceError` from an undeclared name, the third resumable kind.
    assert_eq!(
        run_ret(
            "try { nope(); } catch (e) { return [e instanceof ReferenceError, e instanceof Error]; }"
        ),
        json!([true, true])
    );
    // Constructed, with `new` and without, and through a value. All three
    // reach the same registry row now — there is no compiler shortcut left
    // for the first two to take.
    assert_eq!(
        run_ret(
            r#"return [new RangeError("x") instanceof RangeError, new RangeError("x") instanceof Error];"#
        ),
        json!([true, true])
    );
    assert_eq!(
        run_ret(
            r#"return [SyntaxError("x") instanceof SyntaxError, EvalError("x") instanceof EvalError];"#
        ),
        json!([true, true])
    );
    assert_eq!(
        run_ret(r#"const E = ValueError; return [E("x") instanceof ValueError, `${E("x")}`];"#),
        json!([true, "ValueError: x"])
    );
    // And thrown-then-caught, which is the shape that actually matters.
    assert_eq!(
        run_ret(
            r#"try { throw new RangeError("x"); } catch (e) { return e instanceof RangeError; }"#
        ),
        json!(true)
    );
}

#[test]
fn the_classes_are_siblings_not_cousins() {
    // **The assertion the whole change is for.** One shared prototype
    // could only answer every `instanceof` the same way; if these were
    // true, branching per class would take the first arm every time and
    // a program handling `RangeError` would swallow type errors with it.
    assert_eq!(
        run_ret(r#"return new TypeError("x") instanceof RangeError;"#),
        json!(false)
    );
    assert_eq!(
        run_ret(r#"return new ValueError("x") instanceof TypeError;"#),
        json!(false)
    );
    assert_eq!(
        run_ret("try { null.y; } catch (e) { return e instanceof ValueError; }"),
        json!(false)
    );
    // The base class is not an instance of its subclasses either — the
    // chain runs one way.
    assert_eq!(
        run_ret(r#"return new Error("x") instanceof TypeError;"#),
        json!(false)
    );
}

#[test]
fn the_classes_nothing_raises_are_still_real_classes() {
    // `URIError`, `AggregateError` and `SuppressedError` have no producer
    // in this runtime — no `encodeURI`, `Promise.any` is a compile error,
    // and there is no `using`. They are declared for the program's own
    // `throw`, so the thing to check is that a thrown one behaves like any
    // other error rather than dying as an undeclared name.
    assert_eq!(
        run_ret(
            r#"try { throw new URIError("bad escape"); }
               catch (e) { return [e.name, e.message, e instanceof URIError, e instanceof Error]; }"#
        ),
        json!(["URIError", "bad escape", true, true])
    );
    assert_eq!(
        run_ret(r#"return `${new URIError("bad escape")}`;"#),
        json!("URIError: bad escape")
    );
    // Siblings, like the rest: a program branching on one must not catch
    // the other.
    assert_eq!(
        run_ret(r#"return new URIError("x") instanceof TypeError;"#),
        json!(false)
    );
}

#[test]
fn an_aggregate_error_carries_its_errors_as_an_array() {
    // **The reason it is not faked.** Code that catches an
    // `AggregateError` is code that reads `e.errors`; a class that took
    // the iterable and dropped it would fail precisely where it is used.
    assert_eq!(
        run_ret(
            r#"const e = new AggregateError([new TypeError("a"), new RangeError("b")], "all failed");
               return [e.name, e.message, e.errors.length, e.errors[1].name];"#
        ),
        json!(["AggregateError", "all failed", 2, "RangeError"])
    );
    // `.errors` is a real array, not the argument passed through: a Set
    // works, as it does for `new Set(x)`/`new Map(x)`, and comes back
    // indexable.
    assert_eq!(
        run_ret(
            r#"const e = new AggregateError(new Set(["x", "y"]));
               return [Array.isArray(e.errors), e.errors[0], e.message];"#
        ),
        json!([true, "x", ""])
    );
    // A value nothing can iterate names what it got rather than yielding
    // an empty `.errors` that reads as "no failures".
    assert_eq!(
        run_err_kind(r#"throw new AggregateError(7, "m");"#),
        ErrorKind::TypeError
    );
}

#[test]
fn a_suppressed_error_carries_both_halves() {
    // The class exists for the pair: `.error` won, `.suppressed` was
    // displaced by it. Either one missing and it is not a SuppressedError.
    assert_eq!(
        run_ret(
            r#"const e = new SuppressedError(new TypeError("dispose"), new RangeError("body"), "m");
               return [e.name, e.message, e.error.name, e.suppressed.name];"#
        ),
        json!(["SuppressedError", "m", "TypeError", "RangeError"])
    );
    // Both keys exist even with no arguments — an absent half is
    // `undefined`, not a missing property.
    assert_eq!(
        run_ret(r#"return Object.keys(new SuppressedError());"#),
        json!(["name", "message", "error", "suppressed"])
    );
}

#[test]
fn value_error_is_a_class_though_it_is_not_a_js_name() {
    // A deliberate dialect addition. `ValueError` is what this VM raises
    // for "right type, impossible value", and it is the second commonest
    // kind a program can catch — so leaving it classless would have made
    // the likeliest error the one `instanceof` could not answer for.
    assert_eq!(run_ret("return typeof ValueError;"), json!("function"));
    assert_eq!(
        run_ret(r#"return [new ValueError("x").name, `${new ValueError("x")}`];"#),
        json!(["ValueError", "ValueError: x"])
    );
    assert_eq!(
        run_ret(r#"return new ValueError("x").constructor === ValueError;"#),
        json!(true)
    );
}

#[test]
fn a_subclass_changes_nothing_a_program_already_reads() {
    // `e.name` is the idiom in use (82 reads across the reply corpus,
    // against two `instanceof`s). The classes are added *beside* it —
    // every one of these answered the same way before the change.
    assert_eq!(
        run_ret("try { null.y; } catch (e) { return e.name; }"),
        json!("TypeError")
    );
    assert_eq!(
        run_ret(r#"return Object.keys(new RangeError("x"));"#),
        json!(["name", "message"])
    );
    assert_eq!(
        run_ret(r#"return JSON.stringify(new ValueError("x"));"#),
        json!(r#"{"name":"ValueError","message":"x"}"#)
    );
    assert_eq!(
        run_ret(r#"return `${new TypeError("m")}`;"#),
        json!("TypeError: m")
    );
    // The shape is still not what makes an error: a record with the same
    // two fields is a record.
    assert_eq!(
        run_ret(
            r#"return [({ name: "TypeError", message: "y" }) instanceof TypeError, `${{ name: "TypeError", message: "y" }}`];"#
        ),
        json!([false, "[object Object]"])
    );
}

#[test]
fn every_class_prototype_is_empty() {
    // The prototypes carry no own properties — their methods are virtual
    // rungs from the builtin registry — so six new prototypes add nothing
    // to a `for-in` or an `Object.keys` anywhere.
    for name in [
        "Error",
        "TypeError",
        "ValueError",
        "RangeError",
        "SyntaxError",
        "ReferenceError",
        "EvalError",
    ] {
        assert_eq!(
            run_ret(&format!("return Object.keys({name}.prototype);")),
            json!([]),
            "{name}.prototype has own keys"
        );
        // …and each one is a rung below `Error.prototype`, which is a rung
        // below `Object.prototype`. That two-rung chain is the hierarchy.
        let expected = if name == "Error" { "Object" } else { "Error" };
        assert_eq!(
            run_ret(&format!(
                "return Object.getPrototypeOf({name}.prototype) === {expected}.prototype;"
            )),
            json!(true),
            "{name}.prototype does not chain to {expected}.prototype"
        );
        assert_eq!(
            run_ret(&format!(r#"return new {name}("x") instanceof Object;"#)),
            json!(true),
            "a {name} is not an Object"
        );
    }
}
