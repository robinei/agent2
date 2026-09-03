//! Phase 7: async/await end-to-end tests. Tier 1 — promises, the outbox
//! protocol (`StepResult::Pending`), `Promise.all` fan-out, rejection
//! escalation, and the promise-misuse diagnostics. Tier 2 — stackless
//! suspension of async function frames: interleaved chains, scheduler
//! determinism, try/catch across suspension, rejection propagation,
//! promise adoption, and deadlock detection. Instruction-level protocol
//! tests live in `vm/tests.rs` (effects section).

use crate::testutil::*;
use crate::vm::{ErrorKind, InvokeCall, ResumeMode, StepResult, VM, Value};

// ── fan-out ────────────────────────────────────────────────────────

#[test]
fn fanout_via_map_single_yield() {
    // The canonical Tier 1 pattern: start N calls via map, await them all.
    // ONE Pending yield carries all N calls (fan-out composes across the
    // map's control flow — there are no adjacent Invoke instructions here).
    let src = r#"
        const urls = ["a", "b", "c"];
        const ps = urls.map(u => tools.fetch(u));
        return await Promise.all(ps);
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => calls,
        other => panic!("expected Pending, got {other:?}"),
    };
    assert_eq!(calls.len(), 3, "one yield carries all three calls");
    for call in &calls {
        assert_eq!(call.name, "fetch");
    }
    // Resolve in REVERSE order: resolution order must not matter.
    for call in calls.iter().rev() {
        let arg = match &call.args[0] {
            Value::String(s) => s.as_str().to_owned(),
            other => panic!("expected string arg, got {other:?}"),
        };
        vm.resolve_promise(call.promise, Value::String(format!("got:{arg}").into()))
            .unwrap();
    }
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, unstarted } => {
            assert!(unstarted.is_empty());
            assert_eq!(
                vm.stack_value_to_json(&value, 0).unwrap(),
                serde_json::json!(["got:a", "got:b", "got:c"])
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn serial_awaits_in_loop() {
    // An await inside a loop body yields once per iteration (each call is
    // started only after the previous one resolved).
    let mut yields = 0;
    let result = run_ret_with_tools(
        "let total = 0;
         for (const x of [1, 2, 3]) { total = total + await tools.get(x); }
         return total;",
        |name, args| {
            assert_eq!(name, "get");
            yields += 1;
            Value::Float(args[0].as_f64().unwrap() * 10.0)
        },
    );
    assert_eq!(result, serde_json::json!(60));
    assert_eq!(yields, 3);
}

// ── await semantics ────────────────────────────────────────────────

#[test]
fn top_level_await_and_return_coexist() {
    // Module source type gives top-level await; allow_return_outside_function
    // keeps top-level return. Awaiting a non-promise passes it through.
    assert_eq!(run_ret("return await 5;"), serde_json::json!(5));
    assert_eq!(
        run_ret("const x = await \"s\"; return x;"),
        serde_json::json!("s")
    );
}

#[test]
fn await_same_promise_twice() {
    // A settled promise stays settled: a second await sees the same value
    // without another host round-trip.
    let result = run_ret_with_tools(
        "const p = tools.f();
         const a = await p;
         const b = await p;
         return [a, b];",
        |_, _| Value::PosInt(7),
    );
    assert_eq!(result, serde_json::json!([7, 7]));
}

#[test]
fn rejected_await_escalates_and_is_resumable() {
    let prog = compile_ok("return await tools.f();");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let id = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => calls[0].promise,
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.reject_promise(id, Value::String("tool exploded".into()))
        .unwrap();
    let err = vm.step(u64::MAX).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ValueError);
    assert!(
        err.message.contains("rejected") && err.message.contains("tool exploded"),
        "got: {}",
        err.message
    );
    // Phase 3 path: the host substitutes a value and execution continues.
    assert!(matches!(err.resume, ResumeMode::PushValueThenContinue));
    vm.resume_with(&err, Value::PosInt(0)).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::PosInt(0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

// ── async functions ────────────────────────────────────────────────

#[test]
fn async_function_declaration_round_trips() {
    // The async body runs synchronously until its first pending await,
    // suspends (Tier 2), and hands the caller a promise; the outer `await`
    // delivers the eventual return value.
    let result = run_ret_with_tools(
        "async function get(u) { return await tools.fetch(u); }
         return await get(\"x\");",
        |_, args| Value::String(format!("got:{}", str_arg(&args[0])).into()),
    );
    assert_eq!(result, serde_json::json!("got:x"));
}

#[test]
fn async_arrow_and_expression_accepted() {
    let result = run_ret_with_tools(
        "const f = async (u) => await tools.g(u);
         const h = async function (u) { return await tools.g(u); };
         return [await f(1), await h(2)];",
        |_, args| Value::Float(args[0].as_f64().unwrap() + 100.0),
    );
    assert_eq!(result, serde_json::json!([101, 102]));
}

// ── fire-and-forget / program end ──────────────────────────────────

#[test]
fn unawaited_calls_reported_in_done() {
    // A started-but-never-awaited call still reaches the host (in
    // Done::unstarted), preserving fire-and-forget effects like logging.
    let prog = compile_ok("tools.audit(\"step1\"); tools.audit(\"step2\"); return 1;");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, unstarted } => {
            assert_eq!(value, Value::PosInt(1));
            let args: Vec<&str> = unstarted.iter().map(|c| str_arg(&c.args[0])).collect();
            assert_eq!(args, ["step1", "step2"]);
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn mixed_awaited_and_unawaited() {
    // Calls already delivered via Pending do NOT reappear in Done::unstarted;
    // calls started after the last yield do.
    let prog = compile_ok(
        "const a = await tools.f();
         tools.fire();
         return a;",
    );
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let id = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => {
            assert_eq!(calls.len(), 1);
            calls[0].promise
        }
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.resolve_promise(id, Value::PosInt(5)).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, unstarted } => {
            assert_eq!(value, Value::PosInt(5));
            assert_eq!(unstarted.len(), 1);
            assert_eq!(unstarted[0].name, "fire");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

// ── promise value semantics ────────────────────────────────────────

#[test]
fn promise_typeof_and_identity() {
    let result = run_ret(
        "const p = tools.f(1);
         const q = p;
         const r = tools.f(2);
         return [typeof p, p === q, p === r];",
    );
    assert_eq!(result, serde_json::json!(["object", true, false]));
}

#[test]
fn promise_has_no_json_form() {
    // Returning a promise reaches the persistence boundary -> error with the
    // missing-await hint (host-side conversion).
    let prog = compile_ok("return tools.f();");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let value = match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => value,
        other => panic!("expected Done, got {other:?}"),
    };
    let err = vm.stack_value_to_json(&value, 0).unwrap_err();
    assert!(
        err.message.contains("did you forget `await`?"),
        "got: {}",
        err.message
    );
    // Program-side JSON.stringify hits the same wall.
    let err = run_runtime_err("return JSON.stringify(tools.f());");
    assert!(
        err.message.contains("did you forget `await`?"),
        "got: {}",
        err.message
    );
}

#[test]
fn property_access_on_promise_hints_await() {
    let err = run_runtime_err("const p = tools.f(); return p.result;");
    assert_eq!(err.kind, ErrorKind::TypeError);
    assert!(
        err.message.contains("did you forget `await`?"),
        "got: {}",
        err.message
    );
    let err = run_runtime_err("const p = tools.f(); return p[0];");
    assert!(
        err.message.contains("did you forget `await`?"),
        "got: {}",
        err.message
    );
}

// ── diagnostics (compile-time) ─────────────────────────────────────

#[test]
fn unsupported_promise_statics_are_compile_errors() {
    for (snippet, expect) in [
        ("Promise.race([]);", "await the promises"),
        ("Promise.any([]);", "await the promises"),
        ("Promise.resolve(1);", "plain values"),
        ("Promise.reject(1);", "plain values"),
    ] {
        let errs = compile_errs(snippet);
        assert!(
            errs[0].contains(expect),
            "for `{snippet}` expected `{expect}`, got: {}",
            errs[0]
        );
    }
}

#[test]
fn new_promise_is_a_targeted_compile_error() {
    let errs = compile_errs("const p = new Promise((res, rej) => res(1));");
    assert!(errs[0].contains("no executor pattern"), "got: {}", errs[0]);
}

#[test]
fn for_await_is_a_compile_error() {
    let errs = compile_errs("for await (const x of xs) { }");
    assert!(errs[0].contains("for await"), "got: {}", errs[0]);
}

#[test]
fn await_in_sync_function_is_a_parse_error() {
    // The parser confines `await` to async bodies and the top level — the
    // invariant Tier 2's single-frame suspension relies on.
    let errs = compile_errs("function f(p) { return await p; }");
    assert!(!errs.is_empty());
}

// ── Tier 2: interleaved async functions ────────────────────────────

#[test]
fn canonical_chain_maximal_batching() {
    // The pattern Tier 2 exists for: per-item chains with a data dependency
    // (`g` needs `f`'s result) still batch maximally — the first yield
    // carries ALL `f` calls, the second ALL `g` calls. In Tier 1 these
    // chains ran correctly but serially (2N yields instead of 2).
    let src = r#"
        const items = [1, 2, 3];
        return await Promise.all(items.map(async (it) => tools.g(await tools.f(it))));
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["f", "f", "f"], "first yield carries all f calls");
    for c in &calls {
        let x = c.args[0].as_f64().unwrap();
        vm.resolve_promise(c.promise, Value::Float(x * 10.0))
            .unwrap();
    }
    let calls = expect_pending(&mut vm);
    let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["g", "g", "g"], "second yield carries all g calls");
    let args: Vec<f64> = calls.iter().map(|c| c.args[0].as_f64().unwrap()).collect();
    assert_eq!(args, [10.0, 20.0, 30.0], "g sees f's results");
    for c in &calls {
        let x = c.args[0].as_f64().unwrap();
        vm.resolve_promise(c.promise, Value::Float(x + 1.0))
            .unwrap();
    }
    assert_eq!(expect_done_json(&mut vm), serde_json::json!([11, 21, 31]));
}

#[test]
fn caller_continues_after_async_call_suspends() {
    // A pending await suspends exactly the async frame: the caller receives
    // a promise and keeps running (the fire call lands in the same yield).
    let src = r#"
        async function slow() { return await tools.f(); }
        const p = slow();
        tools.fire("after");
        return await p;
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["f", "fire"],
        "root kept executing past the suspension"
    );
    vm.resolve_promise(calls[0].promise, Value::PosInt(9))
        .unwrap();
    vm.resolve_promise(calls[1].promise, Value::Null).unwrap();
    assert_eq!(expect_done_json(&mut vm), serde_json::json!(9));
}

#[test]
fn interleaving_follows_host_resolution_order() {
    // Scheduling is a deterministic FIFO over the host's resolution order
    // (commitment 6): resolving b before a resumes b's continuation first.
    let src = r#"
        const log = [];
        async function chain(name) {
            log.push(name + ":start");
            await tools.t(name);
            log.push(name + ":resumed");
            return name;
        }
        const a = chain("a");
        const b = chain("b");
        await Promise.all([a, b]);
        return log;
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    assert_eq!(calls.len(), 2);
    vm.resolve_promise(calls[1].promise, Value::Null).unwrap(); // b first
    vm.resolve_promise(calls[0].promise, Value::Null).unwrap();
    assert_eq!(
        expect_done_json(&mut vm),
        serde_json::json!(["a:start", "b:start", "b:resumed", "a:resumed"])
    );
}

#[test]
fn concurrent_calls_to_same_async_fn_do_not_share_state() {
    let src = r#"
        async function acc(x) {
            const local = x * 10;
            const r = await tools.f(x);
            return local + r;
        }
        const a = acc(1);
        const b = acc(2);
        return [await a, await b];
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    // Resolve in reverse order: each suspended frame keeps its own locals.
    for c in calls.iter().rev() {
        let x = c.args[0].as_f64().unwrap();
        vm.resolve_promise(c.promise, Value::Float(x)).unwrap();
    }
    assert_eq!(expect_done_json(&mut vm), serde_json::json!([11, 22]));
}

#[test]
fn await_with_live_mid_expression_state() {
    // Suspension snapshots expression temporaries exactly: awaits as binary
    // operands, in a ternary arm, and in a call argument list, with partial
    // sums live across each suspension.
    let result = run_ret_with_tools(
        r#"
        async function two(x) {
            return (await tools.a(x)) + (x > 1 ? await tools.b(x) : 100) + Math.max(1, await tools.c(x));
        }
        return await Promise.all([two(1), two(2)]);
        "#,
        |name, args| {
            let x = args[0].as_f64().unwrap();
            match name {
                "a" => Value::Float(x * 10.0),
                "b" => Value::Float(x * 100.0),
                "c" => Value::Float(x),
                other => panic!("unexpected tool {other}"),
            }
        },
    );
    assert_eq!(result, serde_json::json!([111, 222]));
}

#[test]
fn await_in_loop_head_resumes_repeatedly() {
    // Suspend → resume → re-suspend cycles of one logical frame, with the
    // await in the loop condition.
    let mut served = 0;
    let result = run_ret_with_tools(
        r#"
        async function count() {
            let n = 0;
            while ((await tools.next()) > 0) { n = n + 1; }
            return n;
        }
        return await count();
        "#,
        |_, _| {
            served += 1;
            Value::Float(if served <= 3 { 5.0 } else { 0.0 })
        },
    );
    assert_eq!(result, serde_json::json!(3));
    assert_eq!(served, 4);
}

#[test]
fn async_fn_without_suspension_returns_plain_value() {
    // Accepted divergence: completing without ever suspending returns the
    // plain value, not a wrapped promise — observationally invisible since
    // `await` passes non-promises through and is the only promise consumer.
    let result = run_ret(
        r#"
        async function id(x) { return x; }
        const v = id(5);
        return [typeof v, v, await id(6)];
        "#,
    );
    assert_eq!(result, serde_json::json!(["number", 5, 6]));
}

#[test]
fn returned_promise_is_adopted() {
    // An async function returning a promise (after suspending) chains it,
    // as in JS: `wrap`'s await sees inner's eventual value, never a promise.
    // Exercises both adoption paths — the root's Await chain-follow and the
    // scheduler redirect of a continuation woken with a promise payload.
    let result = run_ret_with_tools(
        r#"
        async function inner() { return await tools.f(); }
        async function outer() { await tools.g(); return inner(); }
        async function wrap() { return "v:" + (await outer()); }
        return await wrap();
        "#,
        |name, _| match name {
            "g" => Value::Null,
            "f" => Value::PosInt(42),
            other => panic!("unexpected tool {other}"),
        },
    );
    assert_eq!(result, serde_json::json!("v:42"));
}

// ── Tier 2: rejection and try/catch across suspension ──────────────

#[test]
fn rejection_propagates_through_awaiting_chain() {
    // A rejection cascades through suspended frames with no handlers (each
    // rejects its own promise) until a handler — here the root's — takes it.
    let src = r#"
        async function inner() { return await tools.f(); }
        async function outer() { return await inner(); }
        const p = outer();
        try { return await p; } catch (e) { return "caught:" + e; }
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    vm.reject_promise(calls[0].promise, Value::String("boom".into()))
        .unwrap();
    assert_eq!(expect_done_json(&mut vm), serde_json::json!("caught:boom"));
}

#[test]
fn catch_across_await_handles_rejection_on_resume() {
    // The suspending frame's handler entry is saved into the continuation
    // and re-based on resume: `try { await p } catch` works across a
    // suspension, with the rejection delivered on resume.
    let src = r#"
        async function safe() {
            try { return await tools.f(); } catch (e) { return "caught:" + e; }
        }
        return await safe();
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    vm.reject_promise(calls[0].promise, Value::String("bad".into()))
        .unwrap();
    assert_eq!(expect_done_json(&mut vm), serde_json::json!("caught:bad"));
}

#[test]
fn nested_try_rejection_hits_inner_handler() {
    // Both nested handlers span the await and are saved/re-based; the
    // rejection dispatches to the innermost.
    let src = r#"
        async function g() {
            try {
                try { return await tools.f(); } catch (e) { return "inner:" + e; }
            } catch (e2) { return "outer:" + e2; }
        }
        return await g();
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    vm.reject_promise(calls[0].promise, Value::String("boom".into()))
        .unwrap();
    assert_eq!(expect_done_json(&mut vm), serde_json::json!("inner:boom"));
}

#[test]
fn suspended_handler_does_not_leak_into_root() {
    // A frame that suspends inside a `try` carries its handler entry away
    // with it: a top-level throw afterwards must NOT be caught by it.
    let src = r#"
        async function g() { try { return await tools.f(); } catch (e) { return "swallowed"; } }
        g();
        throw "escapes";
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let err = vm.step(u64::MAX).unwrap_err();
    assert_eq!(err.kind, ErrorKind::UncaughtException);
    assert!(err.message.contains("escapes"), "got: {}", err.message);
}

#[test]
fn throw_before_first_suspension_propagates_synchronously() {
    // Accepted divergence: until its first suspension an async body runs on
    // the caller's stack, so an early throw reaches the caller's `try`
    // directly (JS would reject the promise instead).
    let result = run_ret(
        r#"
        async function boom() { throw "sync"; }
        try { boom(); } catch (e) { return "caught:" + e; }
        return "uncaught";
        "#,
    );
    assert_eq!(result, serde_json::json!("caught:sync"));
}

#[test]
fn throw_after_suspension_rejects_promise() {
    // After the first suspension the frame belongs to a strand: an uncaught
    // throw rejects the call's promise (JS semantics) instead of unwinding
    // into the parked root — even though the root has a `try` active.
    let src = r#"
        async function boom() { await tools.f(); throw "late"; }
        const p = boom();
        try { return await p; } catch (e) { return "caught:" + e; }
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    vm.resolve_promise(calls[0].promise, Value::Null).unwrap();
    assert_eq!(expect_done_json(&mut vm), serde_json::json!("caught:late"));
}

#[test]
fn vm_error_in_strand_rejects_promise() {
    // A catchable VM error (TypeError) inside a resumed strand with no
    // strand handler rejects the strand's promise as a { name, message }
    // error object — the root's handler wall holds.
    let src = r#"
        async function bad() { await tools.f(); return null.x; }
        const p = bad();
        try { return await p; } catch (e) { return "name:" + e.name; }
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    vm.resolve_promise(calls[0].promise, Value::Null).unwrap();
    assert_eq!(
        expect_done_json(&mut vm),
        serde_json::json!("name:TypeError")
    );
}

#[test]
fn promise_all_settled_mixed_outcomes() {
    // JS result shape: one { status, value/reason } entry per element, in
    // input order; a rejection becomes an entry instead of propagating; a
    // non-promise element settles fulfilled. The helper never rejects.
    let src = r#"
        const ps = [tools.a(), tools.b(), 7];
        return await Promise.allSettled(ps);
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    assert_eq!(calls.len(), 2);
    vm.resolve_promise(calls[0].promise, Value::PosInt(1))
        .unwrap();
    vm.reject_promise(calls[1].promise, Value::String("boom".into()))
        .unwrap();
    assert_eq!(
        expect_done_json(&mut vm),
        serde_json::json!([
            { "status": "fulfilled", "value": 1 },
            { "status": "rejected", "reason": "boom" },
            { "status": "fulfilled", "value": 7 },
        ])
    );
}

#[test]
fn promise_all_settled_with_async_fn_chain() {
    // allSettled over async-call promises: a strand that throws after
    // suspending lands as a rejected entry (its promise rejection is the
    // helper's catch), alongside a fulfilled one.
    let src = r#"
        async function ok() { return (await tools.f()) + 1; }
        async function bad() { await tools.f(); throw "late"; }
        return await Promise.allSettled([ok(), bad()]);
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    for c in &calls {
        vm.resolve_promise(c.promise, Value::PosInt(10)).unwrap();
    }
    assert_eq!(
        expect_done_json(&mut vm),
        serde_json::json!([
            { "status": "fulfilled", "value": 11 },
            { "status": "rejected", "reason": "late" },
        ])
    );
}

// ── Tier 2: deadlock and cycles ────────────────────────────────────

#[test]
fn circular_await_is_deadlock_error() {
    // The only deadlock source in this dialect (commitment 4): an async
    // call awaiting its own promise. Detected when the root blocks with
    // nothing ready, nothing in the outbox, and nothing in flight; the
    // error names the await chain.
    let src = r#"
        const shared = { p: null };
        async function f(s) { await tools.x(); return await s.p; }
        shared.p = f(shared);
        return await shared.p;
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    vm.resolve_promise(calls[0].promise, Value::Null).unwrap();
    let err = vm.step(u64::MAX).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Deadlock);
    assert!(matches!(err.resume, ResumeMode::NotResumable));
    assert!(
        err.message.contains("circular await"),
        "got: {}",
        err.message
    );
    assert!(err.message.contains("which awaits"), "got: {}", err.message);
    assert!(err.message.contains("suspended at"), "got: {}", err.message);
}

#[test]
fn chaining_cycle_is_type_error() {
    // A promise resolved with itself (async fn returning its own promise)
    // is the JS "chaining cycle" TypeError, not an infinite loop.
    let src = r#"
        const s = { p: null };
        async function f() { await tools.x(); return s.p; }
        s.p = f();
        return await s.p;
    "#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    vm.resolve_promise(calls[0].promise, Value::Null).unwrap();
    let err = vm.step(u64::MAX).unwrap_err();
    assert_eq!(err.kind, ErrorKind::TypeError);
    assert!(
        err.message.contains("chaining cycle"),
        "got: {}",
        err.message
    );
}

// ── call sites ─────────────────────────────────────────────────────

/// Every `InvokeCall` carries `site` — the source byte offset of its
/// `Invoke` instruction — so the harness can annotate a program's source
/// per call site from the log alone, with no live VM (17_BRANCHES A2).
/// Asserted through a *nested* call (inside an arrow inside `map`) to pin
/// that the site is the call's own instruction, not the top-level one.
#[test]
fn invoke_call_carries_its_source_site() {
    let src = r#"const outer = tools.first("x");
const inner = ["a"].map(u => tools.second(u));
return [await outer, await Promise.all(inner)];
"#;
    let prog = compile_ok(src);
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let calls = expect_pending(&mut vm);
    assert_eq!(calls.len(), 2);

    // Each site points at its own `tools.<name>(` in the source.
    for call in &calls {
        let at = &src[call.site as usize..];
        assert!(
            at.starts_with(&format!("tools.{}(", call.name)),
            "site for `{}` landed at {:?}",
            call.name,
            &at[..at.len().min(30)]
        );
    }
    // The nested call's site is on line 2, the top-level one's on line 1 —
    // distinct positions, not one shared program-level offset.
    let first = calls.iter().find(|c| c.name == "first").unwrap().site;
    let second = calls.iter().find(|c| c.name == "second").unwrap().site;
    assert_ne!(first, second);
    assert_eq!(crate::diag::line_col(src, first).0, 1);
    assert_eq!(crate::diag::line_col(src, second).0, 2);
}

// ── helpers ────────────────────────────────────────────────────────

fn expect_pending(vm: &mut VM) -> Vec<InvokeCall> {
    match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => calls,
        other => panic!("expected Pending, got {other:?}"),
    }
}

fn expect_done_json(vm: &mut VM) -> serde_json::Value {
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => vm.stack_value_to_json(&value, 0).unwrap(),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn str_arg(v: &Value) -> &str {
    match v {
        Value::String(s) => s.as_str(),
        other => panic!("expected string arg, got {other:?}"),
    }
}
