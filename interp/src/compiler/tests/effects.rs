//! Host-effect tests: `tools.*` (Invoke) and `raise` (Raise) —
//! behavioral (end-to-end) tests. Lowering/shape tests live in
//! `codegen_shape.rs`.

use crate::compiler::compile;
use crate::testutil::{eval, eval_str};
use crate::vm::{Instr, StepResult, VM, Value};

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
// The bare-global verbs are a fixed, closed surface, exactly like
// `tools.*` above but without the namespace, since (unlike `tools.*`)
// this set never varies per agent. They lower three ways, and which
// one a verb gets says what kind of call it is:
//
//   `Invoke`  — `ask`, and nothing else: a promise, because the
//               value comes from someone else and may take minutes.
//   `Notify`  — `tell`: settles at dispatch and has no value, so it
//               pushes `undefined` (23_ONE_AGENT.md C0b). Covered
//               separately below, because it never yields `Pending`
//               even when awaited.
//   `Settle`  — everything else: settles into the standing frame and
//               hands back a value, with no promise and no `Await`.
//
// `resume`/`abandon` are pure decision-value constructors (Step D2):
// no call at all — the same shape `TypeError(...)` already builds,
// just tagged `__decision` instead of `name`.

#[test]
fn awaited_harness_verb_calls_yield_pending_effect() {
    // `ask` is the last bare verb with a promise, and the only one
    // that should have had one all along: a genuine round trip to
    // someone else, which may take minutes and may fail
    // asynchronously. Everything else in the vocabulary now settles
    // into the calling frame.
    let src = "return await ask(\"who\", \"q\");";
    let prog = compile(src).expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Pending { calls } => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "ask");
            assert_eq!(
                calls[0].args,
                vec![Value::String("who".into()), Value::String("q".into())]
            );
        }
        other => panic!("expected Pending, got {other:?}"),
    }
}

#[test]
fn settle_at_dispatch_verbs_yield_settle_with_or_without_await() {
    // Every verb the harness answers into the calling frame. Each is
    // checked **both** spellings: the card and its exemplars write
    // `await` in front of several of these and a model that copies them
    // must keep working, while a model that leaves it off must get the
    // value and not a promise. `Await` passing a non-promise straight
    // through is what makes the two identical.
    //
    // `done` used to be on this list. It is not answered into anything
    // now — it halts, so there is no frame left to answer into.
    for (call, expected_name, expected_args) in [
        (
            "spawn(\"reviewer\")",
            "spawn",
            vec![Value::String("reviewer".into())],
        ),
        ("fork()", "fork", vec![]),
        ("list_agents()", "list_agents", vec![]),
        ("fetch_history(7)", "fetch_history", vec![Value::PosInt(7)]),
        (
            "append_history(1)",
            "append_history",
            vec![Value::PosInt(1)],
        ),
        (
            "answer(1, 2, 3)",
            "answer",
            vec![Value::PosInt(1), Value::PosInt(2), Value::PosInt(3)],
        ),
        (
            "remove_history(4, \"note\")",
            "remove_history",
            vec![Value::PosInt(4), Value::String("note".into())],
        ),
        (
            "replace_history(4, \"note\", \"shorter\")",
            "replace_history",
            vec![
                Value::PosInt(4),
                Value::String("note".into()),
                Value::String("shorter".into()),
            ],
        ),
    ] {
        for src in [format!("return {call};"), format!("return await {call};")] {
            let prog = compile(&src).unwrap_or_else(|e| panic!("{src} failed to compile: {e:?}"));
            let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
            match vm.step(u64::MAX).unwrap() {
                StepResult::Settle { call: settle } => {
                    assert_eq!(settle.name, expected_name, "{src}");
                    assert_eq!(settle.args, expected_args, "{src}");
                }
                other => panic!("{src}: expected Settle, got {other:?}"),
            }
            vm.push_settled(Value::Float(7.0)).expect("outstanding");
            match vm.step(u64::MAX).unwrap() {
                StepResult::Done { value, .. } => {
                    assert_eq!(value, Value::Float(7.0), "{src}: the value itself");
                }
                other => panic!("{src}: expected completion, got {other:?}"),
            }
        }
    }
}

#[test]
fn a_settle_is_not_re_issued_while_it_waits() {
    // The host need not answer in the same dispatch pass — `spawn`
    // takes a round trip through the real harness — so a host that
    // ticks the VM meanwhile must get "blocked, nothing new" rather
    // than the same call a second time.
    let prog = compile("return spawn(\"c\");").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    assert!(matches!(
        vm.step(u64::MAX).unwrap(),
        StepResult::Settle { .. }
    ));
    for _ in 0..3 {
        match vm.step(u64::MAX).unwrap() {
            StepResult::Pending { calls } => assert!(calls.is_empty(), "nothing new to hand over"),
            other => panic!("expected an empty Pending while waiting, got {other:?}"),
        }
    }
    vm.push_settled(Value::Float(2.0)).expect("outstanding");
    match vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::Float(2.0)),
        other => panic!("expected completion, got {other:?}"),
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
fn the_endings_each_take_exactly_one_argument() {
    // **The pairing is the arity.** `done(text)` and `stop(reason)` both
    // halt, and both are the last thing anyone hears from this reply —
    // so neither may be silent. Requiring the argument is what makes
    // "whatever finishes, speaks" a property of the language rather
    // than a habit the card has to keep teaching: a bare `done()` is
    // the accident that used to end a run without a word to anybody.
    for verb in ["done", "stop"] {
        assert!(
            !crate::testutil::compile_errs(&format!("{verb}();")).is_empty(),
            "{verb}() should not compile — it has nothing to say"
        );
        assert!(
            !crate::testutil::compile_errs(&format!("{verb}(\"a\", \"b\");")).is_empty(),
            "{verb}(a, b) should not compile"
        );
        compile(&format!("{verb}(\"the one thing it says\");"))
            .unwrap_or_else(|e| panic!("{verb}(text) should compile: {e:?}"));
    }
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
    // settles at dispatch, so the value comes straight back onto the
    // frame's stack.
    let prog = compile("const h = spawn(\"charter\"); return h;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        StepResult::Settle { call } => assert_eq!(call.name.as_str(), "spawn"),
        other => panic!("expected the spawn to yield, got {other:?}"),
    }
    vm.push_settled(Value::Float(7.0)).unwrap();
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

    // A handler that *throws* is routed to the next link, because the
    // helper is an async function and an async body that throws rejects
    // its promise (`an_async_body_that_throws_rejects_its_promise`).
    // These two assertions were dropped while that was still broken;
    // they are the reason `.then(f).catch(g)` is worth having at all.
    assert_eq!(
        eval_str("await (1).then(() => { throw new Error(\"boom\"); }).catch((e) => e.message)"),
        "boom"
    );
    assert_eq!(
        eval_str(
            "await (1).then(() => { throw new Error(\"boom\"); }, (e) => \"unused\")\
             .catch((e) => e.message)"
        ),
        "boom"
    );
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

/// **The gap this used to pin is closed.** An `async` function whose
/// body throws rejects its promise, so a caller that holds the promise
/// and awaits it later inside a `try` catches the error, instead of the
/// throw escaping synchronously to whoever made the call.
///
/// What changed: the call's promise is allocated by the prologue
/// (`AsyncEnter`) rather than at the first suspension, so a body that
/// throws before it ever suspends still has a promise to reject — and
/// this body suspends nowhere, since `await 1` on a non-promise passes
/// straight through. It is also why a throw from inside `p.then(f)` now
/// reaches a following `.catch` (`promise_combinators_run`).
#[test]
fn an_async_body_that_throws_rejects_its_promise() {
    let src = "function t() { throw new Error(\"x\"); }\n\
               async function b() { await 1; return t(); }\n\
               const p = b();\n\
               try { await p; } catch (e) { return \"caught\"; }\n\
               return \"no throw\";";
    let prog = compile(src).expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).expect("rejects rather than escaping") {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::String("caught".into()));
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

// ── settling in a plain frame (`Instr::Settle`) ───────────────────
//
// The three properties the `Invoke` + compiler-emitted-`Await`
// lowering could not have. Each is about the *frame*: it never leaves,
// so nothing has to be invented to represent it while it is away.

/// **A settle-at-dispatch verb inside a plain arrow does not suspend.**
/// This is the shape that forced the old design's hand:
/// `names.map(n => spawn(n))` put an `await` — one the source never
/// wrote — inside a function nobody declared `async`, so the arrow's
/// frame suspended and `suspend_current_frame` had to mint a promise
/// to hand back in its place. With `Settle` the arrow just returns the
/// value, and there is no continuation, no strand, and no promise
/// anywhere in the VM.
#[test]
fn a_settle_in_a_plain_arrow_makes_no_strand() {
    let src = "const made = [\"a\", \"b\"].map(n => spawn(n));\n\
               return made;";
    let prog = compile(src).expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let mut handles = 0.0;
    loop {
        match vm.step(u64::MAX).expect("no trap") {
            StepResult::Settle { call } => {
                assert_eq!(call.name.as_str(), "spawn");
                handles += 1.0;
                vm.push_settled(Value::Float(handles)).expect("outstanding");
            }
            StepResult::Done { value, .. } => {
                let rows = vm.stack_value_to_json(&value, 0).expect("json");
                assert_eq!(rows, serde_json::json!([1, 2]), "the handles themselves");
                break;
            }
            other => panic!("expected only settles and completion, got {other:?}"),
        }
    }
    assert_eq!(handles, 2.0, "one settle per element");
    // The point of the exercise: nothing was suspended and nothing was
    // promised. A single stray promise here would mean the arrow's
    // frame had left, which is the whole thing this replaces.
    assert!(vm.promise_count() == 0, "no promise was ever allocated");
    assert!(vm.continuation_count() == 0, "no frame was ever suspended");
}

/// **A throw after a settle-at-dispatch call, in a sync function, is
/// the caller's.** The old lowering suspended the frame at the call,
/// which meant everything after it belonged to a promise — so this
/// `throw` would have rejected a promise the `try` below never held.
/// A function nobody declared `async` must keep throwing to its caller.
#[test]
fn a_throw_after_a_settle_belongs_to_the_caller() {
    let src = "function make(n) { const h = spawn(n); throw \"after \" + h; }\n\
               try { make(\"a\"); } catch (e) { return e; }\n\
               return \"not caught\";";
    let prog = compile(src).expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).expect("no trap") {
        StepResult::Settle { call } => assert_eq!(call.name.as_str(), "spawn"),
        other => panic!("expected a settle, got {other:?}"),
    }
    vm.push_settled(Value::String("h1".into()))
        .expect("outstanding");
    match vm.step(u64::MAX).expect("no trap") {
        StepResult::Done { value, .. } => {
            assert_eq!(
                value,
                Value::String("after h1".into()),
                "the caller caught it"
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

/// **A failed settle throws at the call site**, so an ordinary
/// `try`/`catch` around the call — in a plain function — sees it. A
/// rejected promise could not do this: the rejection belonged to
/// whoever awaited it, and a sync caller never did.
#[test]
fn a_failed_settle_throws_where_it_was_called() {
    let src = "function make() { try { return spawn(1); } catch (e) { return \"caught \" + e; } }\n\
               return make();";
    let prog = compile(src).expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    assert!(matches!(
        vm.step(u64::MAX).unwrap(),
        StepResult::Settle { .. }
    ));
    let outcome = vm
        .settle_throw(Value::String("needs a charter".into()))
        .expect("outstanding");
    assert!(
        matches!(outcome, crate::vm::ThrowOutcome::Caught),
        "the try saw it"
    );
    match vm.step(u64::MAX).expect("no trap") {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::String("caught needs a charter".into()));
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

/// `Promise.all` over settle-at-dispatch calls still works, because
/// there are no promises in it to combine: they are plain values, and
/// `Promise.all` passes a non-thenable through. The card, the
/// exemplars and several harness tests spell it exactly this way.
#[test]
fn promise_all_over_settles_is_the_values() {
    let src = "return await Promise.all([\"a\", \"b\"].map(n => spawn(n)));";
    let prog = compile(src).expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let mut n = 0.0;
    loop {
        match vm.step(u64::MAX).expect("no trap") {
            StepResult::Settle { .. } => {
                n += 1.0;
                vm.push_settled(Value::Float(n)).expect("outstanding");
            }
            StepResult::Done { value, .. } => {
                let rows = vm.stack_value_to_json(&value, 0).expect("json");
                assert_eq!(rows, serde_json::json!([1, 2]));
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// **Every name in the exported vocabulary lowers to a host call.** The
/// list is what `agent`'s own gate iterates to check the harness answers
/// them; if a name drifted out of the compiler's arms it would lower to
/// an ordinary call to an undefined global, and the harness gate would
/// then be checking nothing.
#[test]
fn every_harness_verb_lowers_to_a_host_call() {
    for verb in crate::HARNESS_VERBS {
        // Enough arguments for the arity-checked ones; the rest ignore
        // the extras, and none of this runs past the first call.
        let src = match *verb {
            "done" | "stop" => format!("{verb}(\"x\");"),
            "fork" => format!("{verb}();"),
            _ => format!("{verb}(1, 2, 3);"),
        };
        let prog = compile(&src).unwrap_or_else(|e| panic!("{verb} failed to compile: {e:?}"));
        // **Two of them are not host calls, and the difference is the
        // point.** `done` and `stop` halt the VM where they stand, so
        // they lower to their own instructions rather than to something
        // the host answers: there is no answer, and nothing after them
        // to give one to. Everything else is a call.
        let emitted = match *verb {
            "done" => prog.code.iter().any(|i| matches!(i, Instr::Done)),
            "stop" => prog.code.iter().any(|i| matches!(i, Instr::Stop)),
            _ => prog.code.iter().any(|i| {
                matches!(i, Instr::Invoke(n, _) | Instr::Notify(n, _) | Instr::Settle(n, _)
                    if n.as_str() == *verb)
            }),
        };
        assert!(emitted, "`{verb}` does not lower to a host call");
    }
}

/// **A settle that fails with nowhere to catch it traps the program.**
/// The host answers a `Settle` between steps, so there is no `step()`
/// result for `settle_throw` to fail — it records the value and the
/// next `step` raises exactly the error an uncaught `throw` raises,
/// payload and all. Without this the VM would carry on past the call
/// with a hole where its value should have been.
#[test]
fn an_uncaught_settle_failure_traps_on_the_next_step() {
    let prog = compile("const h = spawn(1); return h;").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    assert!(matches!(
        vm.step(u64::MAX).unwrap(),
        StepResult::Settle { .. }
    ));
    let outcome = vm
        .settle_throw(Value::String("needs a charter".into()))
        .expect("outstanding");
    assert!(
        matches!(outcome, crate::vm::ThrowOutcome::Uncaught(_)),
        "nothing could catch it"
    );
    let err = vm.step(u64::MAX).expect_err("the program traps");
    assert_eq!(err.kind, crate::vm::ErrorKind::UncaughtException);
    assert_eq!(
        err.payload,
        Some(Value::String("needs a charter".into())),
        "the harness's own message reaches the report structurally"
    );
}

/// A settle inside an **async** call rejects that call's promise, the
/// same as any other throw there: the failure belongs to whoever awaits
/// the call, not to the parked code underneath it.
#[test]
fn a_settle_failure_inside_an_async_call_rejects_its_promise() {
    let src = "async function make() { return spawn(1); }\n\
               try { return await make(); } catch (e) { return \"caught \" + e; }";
    let prog = compile(src).expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    assert!(matches!(
        vm.step(u64::MAX).unwrap(),
        StepResult::Settle { .. }
    ));
    vm.settle_throw(Value::String("no charter".into()))
        .expect("outstanding");
    match vm.step(u64::MAX).expect("no trap") {
        StepResult::Done { value, .. } => {
            assert_eq!(value, Value::String("caught no charter".into()));
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

/// **`[...m]` and `Array.from(m)` take the same things.**
///
/// `String.match` with a non-global pattern answers an object
/// carrying `0`, `index`, `input` and `length` — JavaScript's own
/// match result is an array wearing those extra properties, which a
/// `ThinVec` cannot be. `Array.from` already accepted it and spread
/// did not, which is the gap this codebase already closed once for
/// `Array.from(new Set())`: two spellings of one operation that
/// disagree can only be found by falling into them. A `sweep-8` reply
/// on 2026-09-20 wrote the losing one twice.
#[test]
fn a_match_result_spreads_like_the_array_it_is_in_javascript() {
    let out = crate::testutil::run_ret(r#"return [..."hello".match(/l(l)/)];"#);
    assert_eq!(out, serde_json::json!(["ll", "l"]));

    // The extra properties the real thing carries are still there.
    let out = crate::testutil::run_ret(r#"return "hello".match(/l(l)/).index;"#);
    assert_eq!(out, serde_json::json!(2));

    // And the two spellings agree, which is the whole point.
    let out = crate::testutil::run_ret(
        r#"const m = "hello".match(/l(l)/); return JSON.stringify([...m]) === JSON.stringify(Array.from(m));"#,
    );
    assert_eq!(out, serde_json::json!(true));
}

/// **A spread that fails says what it got, and how you got there.**
///
/// The message named what a spread source may be and not what this
/// one was. The way a program arrives here is almost always
/// `[...s.match(re)]` where the match found nothing — `String.match`
/// answers `null`, not an empty array — and that happened twice in
/// one `sweep-8` reply on 2026-09-20.
#[test]
fn spreading_a_failed_match_says_so() {
    let msg = crate::testutil::run_ret(
        r#"try { return [..."abc".match(/zzz/)]; } catch (e) { return e.message; }"#,
    );
    let msg = msg.as_str().unwrap();
    assert!(msg.contains("Got null"), "names the value: {msg}");
    assert!(
        msg.contains("`String.match` that found nothing"),
        "and how a program gets here: {msg}"
    );

    // A wrong-but-not-null source names itself and skips the hint.
    let msg =
        crate::testutil::run_ret(r#"try { return [...42]; } catch (e) { return e.message; }"#);
    let msg = msg.as_str().unwrap();
    assert!(msg.contains("Got"), "{msg}");
    assert!(!msg.contains("String.match"), "no irrelevant hint: {msg}");

    // And a match that found something still spreads.
    let out = crate::testutil::run_ret(r#"return [..."abc".match(/b/)].length;"#);
    assert_eq!(out, serde_json::json!(1));
}
