//! Host-effect tests: `tools.*` (Invoke) and `raise` (Raise) —
//! behavioral (end-to-end) tests. Lowering/shape tests live in
//! `codegen_shape.rs`.

use crate::compiler::compile;
use crate::testutil::{eval, eval_str};
use crate::vm::{StepResult, VM, Value};

// ── effects (tools / raise) ───────────────────────────────────────

#[test]
fn awaited_tools_call_yields_pending_effect() {
    // End-to-end: an awaited `tools.*` call yields a `Pending` effect
    // carrying the method name and evaluated args; the host settles the
    // call's promise to resume.
    let prog = compile("return await tools.add(10, 3);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let id = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "add");
            assert_eq!(calls[0].args, vec![Value::PosInt(10), Value::PosInt(3)]);
            calls[0].promise
        }
        other => panic!("expected Pending, got {other:?}"),
    };
    // Host resolves the call; the re-executed await returns its value.
    vm.resolve_promise(id, Value::PosInt(13)).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::PosInt(13));
        }
        other => panic!("unexpected effect: {other:?}"),
    }
}

#[test]
fn pending_round_trips_under_single_stepping() {
    // A `Pending` yield in the middle of `step(1)` slices: the host
    // resolves the call and single-stepping continues to completion.
    let prog = compile("const x = await tools.f(1); return x + 1;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step(1).unwrap() {
            StepResult::OutOfFuel => {}
            StepResult::Pending { calls } => {
                for call in calls {
                    vm.resolve_promise(call.promise, Value::PosInt(41)).unwrap();
                }
            }
            StepResult::Done { value, .. } => {
                assert_eq!(value, Value::Float(42.0));
                break;
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}

#[test]
fn unawaited_tools_call_is_fire_and_forget() {
    // Without an await, the program runs to completion and the started call
    // is reported in `Done::unstarted` (host decides whether to run it).
    let prog = compile("tools.log(\"hi\"); return 1;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, unstarted } => {
            assert_eq!(value, Value::PosInt(1));
            assert_eq!(unstarted.len(), 1);
            assert_eq!(unstarted[0].name, "log");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn raise_yields_effect_and_resumes_as_expression() {
    // `raise(...)` is an expression: it yields a `Raise` effect, then the
    // host pushes the resumed value which the program returns.
    let prog = compile("return raise(\"pick_a_number\");").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Raise { condition, payload } => {
            assert_eq!(condition, "pick_a_number");
            assert!(payload.is_none(), "no payload for raise(\"name\")");
        }
        other => panic!("expected Raise, got {other:?}"),
    }
    // Resume via resume_raise (ip already advanced by step()).
    vm.resume_raise(Value::PosInt(42));
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::PosInt(42));
        }
        other => panic!("unexpected effect: {other:?}"),
    }
}

// ── host-seeded `input` binding ─────────────────────────────────────

#[test]
fn for_program_seeds_input_object() {
    // `testutil::run_ret` uses `Null` seed; test manual `for_program` seeding.
    let prog = crate::testutil::compile_ok("return input.x + input.y;");
    let mut vm = VM::for_program(prog, serde_json::json!({"x": 10, "y": 20})).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::Float(30.0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn input_with_null_seed_is_empty_object() {
    // `Null` seed (or missing) yields an empty input object.
    let prog = compile("return Object.keys(input).length;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::Float(0.0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn for_program_with_seeds_both_input_and_attachments() {
    // The two host consts are bound from independent seeds and read
    // side by side; their fixed slots don't collide with nested values.
    let prog = crate::testutil::compile_ok(
        "return input.who + \" wrote \" + attachments.file + \" (\" + input.n + \")\";",
    );
    let mut vm = VM::for_program_with(
        prog,
        serde_json::json!({ "who": "me", "n": 3 }),
        serde_json::json!({ "file": "<body>" }),
    )
    .unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::String("me wrote <body> (3)".into()))
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn attachments_empty_when_unseeded() {
    // `for_program` (no attachments arg) and a Null seed both yield an
    // empty `attachments` object — `attachments.<name>` is undefined, not
    // a crash on the fixed slot.
    let prog = compile("return Object.keys(attachments).length;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::Float(0.0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn attachments_is_a_read_only_host_const() {
    // Same protections as `input`: cannot reassign or shadow.
    let reassign = compile("attachments = {};").unwrap_err();
    assert!(
        reassign
            .iter()
            .any(|d| d.message.contains("reassign") && d.message.contains("attachments")),
        "got: {reassign:?}"
    );
    let shadow = compile("const attachments = 1; return attachments;").unwrap_err();
    assert!(
        shadow
            .iter()
            .any(|d| d.message.contains("shadow") && d.message.contains("attachments")),
        "got: {shadow:?}"
    );
}

// ── Step 5: raise payload and resume_raise ───────────────────────

#[test]
fn raise_with_payload_roundtrip() {
    // `raise("name", expr)` passes the payload in StepResult::Raise.
    let prog = compile("raise(\"err\", 42);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Raise { condition, payload } => {
            assert_eq!(condition, "err");
            assert_eq!(payload, Some(Value::PosInt(42)));
        }
        other => panic!("expected Raise, got {other:?}"),
    }
}

#[test]
fn raise_no_payload_resume_raise() {
    // `raise("name")` → no payload, resume_raise feeds the result value.
    let prog = compile("return raise(\"question\");").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Raise { condition, payload } => {
            assert_eq!(condition, "question");
            assert!(payload.is_none());
        }
        other => panic!("expected Raise, got {other:?}"),
    }
    vm.resume_raise(Value::String("answer".into()));
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::String("answer".into()));
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn raise_too_many_args_is_compile_error() {
    let errs = crate::testutil::compile_errs("raise(\"x\", 1, 2);");
    assert!(!errs.is_empty(), "should be a compile error");
}

#[test]
fn raise_non_literal_name_is_compile_error() {
    let errs = crate::testutil::compile_errs("let n = \"x\"; raise(n);");
    assert!(!errs.is_empty(), "should be a compile error");
}

// ── phase 20 harness vocabulary: bare-global verbs + decision values
// (`docs/20_CODE_MODE.md` Step C1/D2) ─────────────────────────────
//
// `tell`/`ask`/`answer`/`spawn`/`fork`/`append_history`/`fetch_history` are
// a fixed, closed surface — bare-global, `Invoke`-based, exactly like
// `tools.*` above but without the namespace, since (unlike `tools.*`)
// this set never varies per agent. `resume`/`abandon` are pure
// decision-value constructors (Step D2): no `Invoke`, no promise —
// the same shape `TypeError(...)` already builds, just tagged
// `__decision` instead of `name`. `tell` is the one exception to
// "`Invoke`-based": it lowers to `Notify` instead (23_ONE_AGENT.md
// C0b), covered separately below rather than in this loop, because it
// never yields `Pending` even when awaited.

#[test]
fn awaited_harness_verb_calls_yield_pending_effect() {
    // One representative per verb: each is bare (no `tools.` prefix)
    // and produces the same `Invoke` effect `tools.*` does.
    for (call, expected_name, expected_args) in [
        (
            "ask(\"who\", \"q\")",
            "ask",
            vec![Value::String("who".into()), Value::String("q".into())],
        ),
        (
            "spawn(\"reviewer\")",
            "spawn",
            vec![Value::String("reviewer".into())],
        ),
        ("fork()", "fork", vec![]),
        (
            "append_history(1)",
            "append_history",
            vec![Value::PosInt(1)],
        ),
        ("fetch_history(7)", "fetch_history", vec![Value::PosInt(7)]),
        (
            "answer(1, 2)",
            "answer",
            vec![Value::PosInt(1), Value::PosInt(2)],
        ),
        (
            "remove_history(4, \"note\")",
            "remove_history",
            vec![Value::PosInt(4), Value::String("note".into())],
        ),
        (
            "rewrite_history(4, \"note\", \"shorter\")",
            "rewrite_history",
            vec![
                Value::PosInt(4),
                Value::String("note".into()),
                Value::String("shorter".into()),
            ],
        ),
        ("list_agents()", "list_agents", vec![]),
        ("done()", "done", vec![]),
    ] {
        let src = format!("return await {call};");
        let prog = compile(&src).unwrap_or_else(|e| panic!("{call} failed to compile: {e:?}"));
        let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
        match vm.step(u64::MAX).unwrap() {
            StepResult::Pending { calls } => {
                assert_eq!(calls.len(), 1, "{call}");
                assert_eq!(calls[0].name, expected_name, "{call}");
                assert_eq!(calls[0].args, expected_args, "{call}");
            }
            other => panic!("{call}: expected Pending, got {other:?}"),
        }
    }
}

#[test]
fn unawaited_harness_verb_call_is_fire_and_forget() {
    // Same fire-and-forget shape as an unawaited `tools.*` call.
    let prog = compile("tell(\"hi\"); return 1;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, unstarted } => {
            assert_eq!(value, Value::PosInt(1));
            assert_eq!(unstarted.len(), 1);
            assert_eq!(unstarted[0].name, "tell");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn tell_never_yields_pending_awaited_or_not() {
    // `tell()` lowers to `Notify` (23_ONE_AGENT.md C0b), which pushes
    // `undefined` rather than a promise — so unlike every other bare
    // verb, `await tell(...)` and a bare `tell(...)` are the identical
    // expression: `Await`'s own rule is "a non-promise passes through
    // unchanged," and there is no promise here to pass through
    // *unchanged from*. Neither form ever produces a `Pending` effect;
    // the call still reaches the outbox (the host still logs it and
    // delivers it), just never as something the program waited on.
    for src in ["return tell(\"hi\");", "return await tell(\"hi\");"] {
        let prog = compile(src).unwrap_or_else(|e| panic!("{src} failed to compile: {e:?}"));
        let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
        match vm.step(u64::MAX).unwrap() {
            StepResult::Done { value, unstarted } => {
                assert_eq!(value, Value::Undefined, "{src}");
                assert_eq!(unstarted.len(), 1, "{src}");
                assert_eq!(unstarted[0].name, "tell", "{src}");
            }
            other => {
                panic!("{src}: expected Done (no Pending — nothing to wait on), got {other:?}")
            }
        }
    }
}

#[test]
fn a_local_declaration_shadows_the_harness_verb() {
    // `compile_user_call` resolves a local binding before ever
    // reaching the bare-global dispatch these verbs live in, so a
    // program that declares its own `ask` calls that, not the
    // harness verb — no `Invoke` at all.
    assert_eq!(
        eval("(function ask(x) { return x + 1; })(5)"),
        Value::Float(6.0)
    );
    let prog = compile("function ask(x) { return x + 1; } return ask(5);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::Float(6.0)),
        other => panic!("expected Done (no Invoke — shadowed), got {other:?}"),
    }
}

#[test]
fn resume_builds_a_plain_decision_object_no_invoke() {
    assert_eq!(eval_str("resume(42).__decision"), "resume");
    assert_eq!(eval("resume(42).value"), Value::PosInt(42));
    // No host round-trip: running to completion never yields Pending.
    let prog = compile("return resume(42);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {}
        other => panic!("expected Done (a pure value, no effect), got {other:?}"),
    }
}

#[test]
fn resume_with_no_argument_has_an_undefined_value() {
    // `resume()` for a posted condition — "continue, nothing changed"
    // (Step D1) — still a well-formed decision object.
    assert_eq!(eval_str("resume().__decision"), "resume");
    assert_eq!(eval("resume().value"), Value::Undefined);
}

#[test]
fn abandon_builds_a_plain_decision_object_no_invoke() {
    assert_eq!(eval_str("abandon().__decision"), "abandon");
    let prog = compile("return abandon();").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {}
        other => panic!("expected Done (a pure value, no effect), got {other:?}"),
    }
}

#[test]
fn resume_takes_at_most_one_argument() {
    let errs = crate::testutil::compile_errs("resume(1, 2);");
    assert!(!errs.is_empty(), "should be a compile error");
}

#[test]
fn abandon_takes_no_arguments() {
    let errs = crate::testutil::compile_errs("abandon(1);");
    assert!(!errs.is_empty(), "should be a compile error");
}

#[test]
fn done_takes_no_arguments() {
    // Same fixed-arity shape as `abandon` above: `done()` is the only
    // thing that stops the loop (`agent/src/machine.rs`'s `TOOL_DONE`),
    // and no argument would mean anything here.
    let errs = crate::testutil::compile_errs("done(1);");
    assert!(!errs.is_empty(), "should be a compile error");
}

#[test]
fn resume_and_abandon_are_ordinary_values_not_reserved_words() {
    // Neither is a keyword — a program can still use the name as a
    // local binding (shadowing, same as any other bare-global verb).
    let prog =
        compile("function resume(x) { return x * 2; } return resume(21);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::Float(42.0)),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn spawn_yields_its_handle_without_a_source_level_await() {
    // `const h = spawn(...)` must evaluate to the handle, not a promise.
    // The card and its exemplars spell it without `await`, and a model
    // that copies them and passes `h` as an address must not be handing
    // over a promise -- two live traps came from exactly that. Creating
    // settles at dispatch, so the compiler emits the await itself.
    let prog = compile("const h = spawn(\"charter\"); return h;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let id = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => {
            assert_eq!(calls.len(), 1, "one spawn call");
            assert_eq!(calls[0].name.as_str(), "spawn");
            calls[0].promise
        }
        other => panic!("expected the spawn to yield, got {other:?}"),
    };
    vm.resolve_promise(id, Value::Float(7.0)).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::Float(7.0), "the handle itself, not a promise");
        }
        other => panic!("expected completion, got {other:?}"),
    }
}

#[test]
fn spread_takes_a_set_and_a_string() {
    // `[...new Set(xs)]` is *the* JavaScript dedupe. It trapped live on
    // 2026-09-15 with "array spread source must be an array", costing a
    // recovery round trip in a real session.
    assert_eq!(eval_str("[...new Set([1, 2, 2, 3])].join(',')"), "1,2,3");
    assert_eq!(eval_str("[...'abc'].join('-')"), "a-b-c");
    assert_eq!(eval_str("[...new Set(['a']), ...['b']].join(',')"), "a,b");
}

/// **The promise combinators work rather than trapping.** `await`
/// inside `try`/`catch` is what the card teaches and what composes best
/// here — straight-line, and there is nothing to chain onto when the
/// whole program is top-level statements. But reaching for `.catch` is
/// ordinary JavaScript, a live run did it on 2026-09-17, and the trap
/// classified as a `gap`: ours, by our own table. A dialect that could
/// have answered and trapped instead spends the model's round trip to
/// make a point about style.
#[test]
fn promise_combinators_run() {
    // A plain value passes through `await`, so these exercise the
    // helpers' own control flow without needing a host to settle
    // anything — which is the part that could be wrong.
    assert_eq!(eval_str("String(await (7).then((v) => v + 1))"), "8");
    assert_eq!(
        eval_str("String(await (7).then((v) => v + 1, (e) => 0))"),
        "8"
    );
    assert_eq!(eval_str("String(await (7).catch((e) => 0))"), "7");
    assert_eq!(eval_str("String(await (7).finally(() => 1))"), "7");

    // A handler that *throws* is not routed to the next link, because
    // an async body that throws escapes rather than rejecting in this
    // VM — pinned by `an_async_body_that_throws_escapes_instead_of_
    // rejecting`, which predates these helpers and is not caused by
    // them. A rejected *call* does reach `.catch`, which is the case
    // that actually happens: `a_rejected_call_reaches_dot_catch`.
}

/// Chaining composes because each helper is an `async function`, so it
/// returns a promise like the real thing.
#[test]
fn promise_combinators_chain() {
    compile("p.then(f).catch(g).finally(h);").expect("chains");
    compile("const v = await p.catch(g);").expect("awaitable");
}

/// The arities JS has, and a clear refusal past them.
#[test]
fn then_takes_one_or_two_handlers() {
    compile("p.then();").expect_err("no handler is a mistake");
    compile("p.then(f, g, h);").expect_err("three is a mistake");
}

/// **A rejected tool call reaches `.catch`** — the case that actually
/// happens, and the one the combinators exist for. The plain-value
/// tests above exercise the helpers' control flow; this drives a real
/// host rejection through the whole path.
#[test]
fn a_rejected_call_reaches_dot_catch() {
    let prog = compile("return await tools.fetch(\"x\").catch((e) => `handled: ${e}`);")
        .expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let id = match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => calls[0].promise,
        other => panic!("expected Pending, got {other:?}"),
    };
    vm.reject_promise(id, Value::String("host is down".into()))
        .unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::String("handled: host is down".into()));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// **A known gap, pinned rather than papered over.** An `async`
/// function whose body throws does not reject its promise in this VM —
/// the throw escapes synchronously to whoever called it, even when the
/// caller stores the promise and awaits it later inside a `try`. It
/// predates the combinators and is not caused by them (the third case
/// here uses no combinator at all), but it is why `p.then(f)` does not
/// route a throw from `f` into a following `.catch`.
///
/// Recorded as a test so the day it is fixed, this fails and says so.
#[test]
fn an_async_body_that_throws_escapes_instead_of_rejecting() {
    let src = "function t() { throw new Error(\"x\"); }\n\
               async function b() { await 1; return t(); }\n\
               const p = b();\n\
               try { await p; } catch (e) { return \"caught\"; }\n\
               return \"no throw\";";
    let prog = compile(src).expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let err = vm
        .step(u64::MAX)
        .expect_err("escapes rather than rejecting");
    assert_eq!(err.kind, crate::vm::ErrorKind::UncaughtException, "{err:?}");
}
