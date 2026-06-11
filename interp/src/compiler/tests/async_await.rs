//! Phase 7 Tier 1: async/await end-to-end tests — promises, the outbox
//! protocol (`StepResult::Pending`), `Promise.all` fan-out, rejection
//! escalation, and the promise-misuse diagnostics. Instruction-level
//! protocol tests live in `vm/tests.rs` (effects section).

use crate::testutil::*;
use crate::vm::{ErrorKind, ResumeMode, StepResult, VM, Value};

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
    let calls = match vm.step().unwrap() {
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
    match vm.step().unwrap() {
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
    let id = match vm.step().unwrap() {
        StepResult::Pending { calls } => calls[0].promise,
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.reject_promise(id, Value::String("tool exploded".into()))
        .unwrap();
    let err = vm.step().unwrap_err();
    assert_eq!(err.kind, ErrorKind::ValueError);
    assert!(
        err.message.contains("rejected") && err.message.contains("tool exploded"),
        "got: {}",
        err.message
    );
    // Phase 3 path: the host substitutes a value and execution continues.
    assert!(matches!(err.resume, ResumeMode::PushValueThenContinue));
    vm.resume_with(&err, Value::PosInt(0)).unwrap();
    match vm.step().unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::PosInt(0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

// ── async functions (Tier 1: accepted, run synchronously) ─────────

#[test]
fn async_function_declaration_blocks_inline() {
    // Tier 1: the async body runs synchronously on the caller's stack; the
    // outer `await` then passes the plain return value through.
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
    match vm.step().unwrap() {
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
    let id = match vm.step().unwrap() {
        StepResult::Pending { calls } => {
            assert_eq!(calls.len(), 1);
            calls[0].promise
        }
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.resolve_promise(id, Value::PosInt(5)).unwrap();
    match vm.step().unwrap() {
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
    let value = match vm.step().unwrap() {
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
        ("Promise.allSettled([]);", "not supported yet"),
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

// ── helpers ────────────────────────────────────────────────────────

fn str_arg(v: &Value) -> &str {
    match v {
        Value::String(s) => s.as_str(),
        other => panic!("expected string arg, got {other:?}"),
    }
}
