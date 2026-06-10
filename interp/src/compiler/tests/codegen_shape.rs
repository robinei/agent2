//! Optimizer / codegen-shape tests — assert on instruction streams,
//! constant folding, peephole rules, CFG simplification, const-propagation,
//! constant-function lowering. These tests pin the optimizer contract and
//! are immune to compiler churn at the behavioral level.

use super::*;
use crate::builtin::Builtin;
use crate::compiler::compile;
use crate::rc_str::RcStr;
use crate::testutil;
use crate::vm::{Instr, SlotKind, VM, Value};

// ── core optimizer: const-fold / peephole / simplify_cfg ─────────

#[test]
fn const_folds_literal_arithmetic() {
    // `1 + 2 * 3` is fully constant-folded to `7`; as a bare value statement
    // it is then dead (pure push + Pop), leaving just the root `Return(0)`.
    let prog = compile("1 + 2 * 3;").expect("compiles");
    assert_eq!(prog.code, vec![Instr::Return(0)]);
    assert_eq!(prog.spans.len(), prog.code.len());
    let vm = run_program(prog);
    assert!(vm.stack.is_empty());

    // When the folded value is actually used, the constant lands in input.
    // (VM arithmetic yields `Number`, so 7 is stored as `Number(7.0)`.)
    let prog = compile("input.x = 1 + 2 * 3;").expect("compiles");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::Add | Instr::Mul)),
        "arithmetic should be folded away: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), testutil::num(7.0));
}

#[test]
fn peephole_folds_not_into_branch() {
    // `if (!cond) body` should NOT contain a `Not` immediately before a
    // conditional jump — the peephole folds it into the opposite branch.
    let prog = compile("let c = true; if (!c) { input.x = 1; }").expect("compiles");
    let has_not = prog.code.iter().any(|i| matches!(i, Instr::Not));
    assert!(!has_not, "Not should be folded away: {:?}", prog.code);
    // And it still behaves correctly: `c` is true, so the body is skipped.
    let vm = run_program(prog);
    let o = &vm.objects[0];
    assert_eq!(o.get(&RcStr::from("x")), None, "body must not run");
}

#[test]
fn peephole_not_fold_preserves_semantics_when_taken() {
    // `!c` is true here, so the body runs.
    let prog = compile("let c = false; if (!c) { input.x = 1; }").expect("compiles");
    assert!(!prog.code.iter().any(|i| matches!(i, Instr::Not)));
    let vm = run_program(prog);
    let o = &vm.objects[0];
    assert_eq!(o.get(&RcStr::from("x")), Some(&Value::PosInt(1)));
}

#[test]
fn simplify_cfg_eliminates_dead_code_after_return() {
    // Code after an unconditional `return` (before any label) is unreachable
    // and must be dropped.
    let prog = compile("function f() { return 1; let x = 2; return x; } input.r = f();")
        .expect("compiles");
    // Exactly one `Return` instr should survive inside `f` for the live path
    // (plus the root frame's Return(0)). The dead `return x` and its setup
    // are gone, so there is no `PushPosInt(2)` in the stream.
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::PushPosInt(2))),
        "dead `let x = 2` should be eliminated: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    let o = &vm.objects[0];
    assert_eq!(o.get(&RcStr::from("r")), Some(&Value::PosInt(1)));
}

#[test]
fn simplify_cfg_preserves_loop_semantics() {
    // A loop with break/continue exercises jump threading + jump-to-next;
    // verify the observable result is unchanged.
    let prog = compile(
        "let sum = 0; \
             for (let i = 0; i < 10; i++) { \
               if (i === 3) { continue; } \
               if (i === 7) { break; } \
               sum = sum + i; \
             } \
             input.sum = sum;",
    )
    .expect("compiles");
    let vm = run_program(prog);
    // 0+1+2 + 4+5+6 = 18 (3 skipped, break at 7).
    let o = &vm.objects[0];
    assert_eq!(o.get(&RcStr::from("sum")), Some(&Value::Float(18.0)));
}

#[test]
fn peephole_double_negation_compiles_to_tobool() {
    // A *runtime* operand (`input.c`) isn't const-folded, so `!!` exercises
    // the `Not;Not → ToBool` peephole: no `Not`, one `ToBool`.
    let prog = compile("input.b = !!input.c;").expect("compiles");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::Not)),
        "!! should fold to ToBool, no Not left: {:?}",
        prog.code
    );
    assert!(prog.code.iter().any(|i| matches!(i, Instr::ToBool)));
    // `input.c` is undefined here → `!!undefined` is `false`.
    let vm = run_program(prog);
    let o = &vm.objects[0];
    assert_eq!(o.get(&RcStr::from("b")), Some(&Value::Bool(false)));
}

#[test]
fn peephole_drops_pure_value_statement() {
    // A bare `5;` expression statement (pure push + Pop) leaves nothing.
    let prog = compile("5;").expect("compiles");
    assert_eq!(prog.code, vec![Instr::Return(0)]);
}

#[test]
fn optimize_empty_if_body_collapses() {
    // `if (c) {}` — the then-body is empty, so after optimization the branch
    // degenerates to just consuming the condition (no jump survives).
    let prog = compile("let c = true; if (c) { }").expect("compiles");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::JFalse(_) | Instr::JTrue(_) | Instr::Jump(_))),
        "empty if should leave no branch: {:?}",
        prog.code
    );
    // Still executes cleanly.
    let _ = run_program(prog);
}

#[test]
fn const_propagation_folds_uses() {
    // A `const` bound to a literal is propagated to its uses, so `N * 2`
    // folds to `10` — no `Mul`, and no `Local` load of `N`.
    let prog = compile("const N = 5; input.x = N * 2;").expect("compiles");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::Mul | Instr::Local(_))),
        "N should be propagated and folded: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), testutil::num(10.0));
}

#[test]
fn const_propagation_chains() {
    // A const initializer that reads earlier consts folds transitively.
    let prog = compile("const a = 3; const b = a + 1; input.x = b;").expect("compiles");
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), testutil::num(4.0));
}

#[test]
fn const_propagation_respects_shadowing() {
    // Each `const` gets a unique slot, so propagation never confuses an inner
    // shadow with the outer binding.
    let prog =
        compile("const x = 1; { const x = 2; input.a = x; } input.b = x;").expect("compiles");
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "a"), Value::PosInt(2));
    assert_eq!(input_val(&vm, "b"), Value::PosInt(1));
}

#[test]
fn let_is_not_propagated() {
    // A reassigned `let` must read its slot, never a stale literal.
    let prog = compile("let y = 5; y = 6; input.x = y;").expect("compiles");
    assert!(
        prog.code.iter().any(|i| matches!(i, Instr::Local(_))),
        "reassigned let must load: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), Value::PosInt(6));
}

#[test]
fn const_captured_by_closure_is_correct() {
    // `k` is a literal const, eliminated and resolved to its value inside the
    // hoisted `f` (cross-function const resolution via `resolve_captures`).
    let prog = compile("const k = 7; function f() { return k; } input.x = f();").expect("compiles");
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), Value::PosInt(7));
}

// ── Phase A: constant branch folding ─────────────────────────────

#[test]
fn const_branch_folds_dead_arm() {
    // `if (FLAG)` on a const folds the branch; the dead arm is pruned.
    let prog = compile("const FLAG = false; if (FLAG) { input.x = 1; } input.y = 2;").expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::JFalse(_) | Instr::JTrue(_))),
        "branch on a constant should be folded: {:?}",
        prog.code
    );
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::PushPosInt(1))),
        "dead arm should be eliminated: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    let o = &vm.objects[0];
    assert_eq!(o.get(&RcStr::from("x")), None);
    assert_eq!(o.get(&RcStr::from("y")), Some(&Value::PosInt(2)));
}

#[test]
fn const_branch_keeps_live_arm() {
    // `if (true)` keeps the body and drops the branch entirely.
    let prog = compile("const FLAG = true; if (FLAG) { input.x = 1; }").expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::JFalse(_) | Instr::JTrue(_) | Instr::Jump(_))),
        "live constant branch should leave no jump: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), Value::PosInt(1));
}

// ── Phase C: effectively-const `let` ─────────────────────────────

#[test]
fn effectively_const_let_propagates() {
    // A `let` never reassigned and never captured is propagated like a const.
    let prog = compile("let N = 5; input.x = N * 2;").expect("compiles");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::Mul | Instr::Local(_))),
        "effectively-const let should fold: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), testutil::num(10.0));
}

#[test]
fn later_reassignment_defeats_propagation_everywhere() {
    // A reassignment anywhere makes the binding non-immutable, so even the
    // use *before* it loads the slot (sound without dataflow).
    let prog = compile("let N = 5; input.x = N; N = 9; input.y = N;").expect("ok");
    assert!(
        prog.code.iter().any(|i| matches!(i, Instr::Local(_))),
        "a reassigned let must load: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), Value::PosInt(5));
    assert_eq!(input_val(&vm, "y"), Value::PosInt(9));
}

// ── Phase D: captured-const seeding into closures ────────────────

#[test]
fn captured_const_propagates_into_closure() {
    // `k` is a literal const → eliminated; inside the arrow `k * 2` folds to
    // `20`, and `f` captures nothing (no `Mul`).
    let prog = compile("const k = 10; const f = () => k * 2; input.x = f();").expect("ok");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::Mul)),
        "captured const should fold inside the closure: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), testutil::num(20.0));
}

// ── Phase E: constant binding elimination (slot + capture) ───────

#[test]
fn literal_const_has_no_slot_or_store() {
    // A literal `const` is a compile-time binding: no `SetLocal` (no store)
    // and no `Local` (reads are literals).
    let prog = compile("const N = 5; input.x = N;").expect("compiles");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::SetLocal(_) | Instr::Local(_))),
        "literal const should occupy no slot: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), Value::PosInt(5));
}

#[test]
fn const_only_closure_demotes_to_fn() {
    // `f` references only the literal const `k`, which is eliminated — so `f`
    // captures nothing and is a bare `Fn` (no `MakeClosure`, no heap closure).
    let prog = compile("const k = 5; const f = () => k; input.x = f();").expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..))),
        "const-only closure should demote to Fn: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), Value::PosInt(5));
}

#[test]
fn const_only_closure_in_loop_demotes_to_fn() {
    // The `map` callback references only a literal const → no per-iteration
    // closure allocation (bare `Fn`, not `MakeClosure`).
    let prog = compile("const f = 2; input.r = [1, 2, 3].map(x => x * f);").expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..))),
        "callback over a const should not allocate a closure: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    match input_val(&vm, "r") {
        Value::Array(p) => {
            let arr = &vm.arrays[p as usize];
            assert_eq!(arr.len(), 3);
            assert_eq!(arr[2], testutil::num(6.0));
        }
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn non_literal_captured_const_keeps_its_store() {
    // A *non-literal* const (an object) still gets a slot and is captured by
    // value, so its store is kept and the closure is a real `MakeClosure`.
    let prog = compile("const o = { v: 5 }; const f = () => o.v; input.x = f();").expect("ok");
    assert!(
        prog.code.iter().any(|i| matches!(i, Instr::SetLocal(_))),
        "non-literal captured const must keep its store: {:?}",
        prog.code
    );
    assert!(
        prog.code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..)))
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), Value::PosInt(5));
}

#[test]
fn const_elimination_respects_shadowing() {
    // Inner literal const shadows the outer; each reference resolves to its
    // own value even though neither occupies a slot.
    let prog = compile("const x = 1; { const x = 2; input.a = x; } input.b = x;").expect("ok");
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "a"), Value::PosInt(2));
    assert_eq!(input_val(&vm, "b"), Value::PosInt(1));
}

#[test]
fn const_write_still_errors_when_eliminated() {
    // Reassigning an eliminated const is still a compile error.
    let errs = compile("const N = 5; N = 6;").expect_err("should reject");
    assert!(
        errs.iter().any(|d| d.message.contains("constant")),
        "expected an assignment-to-constant error: {errs:?}"
    );
}

#[test]
fn transitively_captured_const_folds() {
    // `k` captured through two closure levels still resolves to its value.
    let prog = compile(
            "const k = 3; const outer = () => { const inner = () => k * 10; return inner(); }; input.x = outer();",
        )
        .expect("ok");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::Mul)),
        "transitive const should fold: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), testutil::num(30.0));
}

// ── Phase F: constant functions ──────────────────────────────────

#[test]
fn mutual_recursion_allocates_no_closures() {
    let prog = compile(
        "function isEven(n){ return n === 0 ? true : isOdd(n - 1); } \
             function isOdd(n){ return n === 0 ? false : isEven(n - 1); } \
             input.x = isEven(10);",
    )
    .expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..))),
        "mutual recursion should allocate no closures: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), Value::Bool(true));
}

#[test]
fn self_recursion_is_static_with_no_slot() {
    let prog =
        compile("function fact(n){ return n <= 1 ? 1 : n * fact(n - 1); } input.x = fact(5);")
            .expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::CallDyn(_) | Instr::MakeClosure(..))),
        "self-recursion should be static: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), testutil::num(120.0));
}

#[test]
fn function_passed_as_value_is_fn_constant() {
    let prog =
        compile("function dbl(x){ return x * 2; } input.r = [1, 2, 3].map(dbl);").expect("ok");
    assert!(prog.code.iter().any(|i| matches!(i, Instr::PushFn(_))));
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..)))
    );
    let vm = run_program(prog);
    match input_val(&vm, "r") {
        Value::Array(p) => {
            let a = &vm.arrays[p as usize];
            assert_eq!(a[2], testutil::num(6.0));
        }
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn function_capturing_real_var_stays_a_closure() {
    let vm = testutil::run(
        "function make() { let base = 100; \
               function adder(n) { return base + n; } \
               return adder(5); \
             } \
             input.x = make();",
    );
    assert_eq!(input_val(&vm, "x"), testutil::num(105.0));
}

#[test]
fn reassigned_function_is_not_a_constant() {
    let prog = compile("function f() { return 1; } input.a = f(); f = 5; input.b = typeof f;")
        .expect("reassignable function binding");
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "a"), Value::PosInt(1));
    match input_val(&vm, "b") {
        Value::String(s) => assert_eq!(s.as_str(), "number"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn const_arrow_is_a_constant_function() {
    let prog = compile("const dbl = (x) => x * 2; input.r = [1, 2, 3].map(dbl); input.y = dbl(5);")
        .expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..))),
        "const arrow should be a Fn constant: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "y"), testutil::num(10.0));
    match input_val(&vm, "r") {
        Value::Array(p) => assert_eq!(vm.arrays[p as usize][2], testutil::num(6.0)),
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn const_named_fn_expr_self_recursion_is_static() {
    let prog = compile(
        "const fact = function f(n){ return n <= 1 ? 1 : n * f(n - 1); }; input.x = fact(4);",
    )
    .expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::CallDyn(_) | Instr::MakeClosure(..))),
        "named const fn-expr self-recursion should be static: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), testutil::num(24.0));
}

#[test]
fn capturing_const_arrow_stays_a_closure() {
    let vm = testutil::run(
        "function make() { let base = 100; const add = (n) => base + n; return add(5); } \
             input.x = make();",
    );
    assert_eq!(input_val(&vm, "x"), testutil::num(105.0));
}

#[test]
fn const_fn_reclaims_its_frame_slot() {
    let prog = compile("function f() { return 1; } let x = 0; x = f(); input.x = x;").expect("ok");
    let local_count = prog.code.iter().find_map(|i| match i {
        Instr::EnterFrame(_, _, kinds) => Some(kinds.len()),
        _ => None,
    });
    assert_eq!(
        local_count,
        Some(1),
        "only `x` should occupy a slot, not `f`: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(input_val(&vm, "x"), Value::PosInt(1));
}

#[test]
fn const_fn_between_locals_renumbers_correctly() {
    let vm = testutil::run(
        "let a = 1; function f() { return 10; } let b = 2; \
             a = a + f(); b = b + f(); input.r = a * 100 + b;",
    );
    assert_eq!(input_val(&vm, "r"), testutil::num(1112.0));
}

#[test]
fn never_reassigned_let_function_is_constant() {
    let prog = compile("let dbl = (x) => x * 2; input.r = [1, 2, 3].map(dbl);").expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..))),
        "never-reassigned let function should be a Fn constant: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    match input_val(&vm, "r") {
        Value::Array(p) => assert_eq!(vm.arrays[p as usize][2], testutil::num(6.0)),
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn reassigned_let_function_is_not_constant() {
    let vm = testutil::run("let f = () => 1; input.a = f(); f = () => 2; input.b = f();");
    assert_eq!(input_val(&vm, "a"), Value::PosInt(1));
    assert_eq!(input_val(&vm, "b"), Value::PosInt(2));
}

#[test]
fn var_function_is_not_a_constant() {
    let vm = testutil::run("var f = () => 7; input.x = f();");
    assert_eq!(input_val(&vm, "x"), Value::PosInt(7));
}

#[test]
fn for_program_seeds_input_at_heap0() {
    // The host-seeded `input` object always lives at objects[0].
    let prog = compile("1;").expect("compiles");
    let state = serde_json::json!({ "count": 7 });
    let vm = VM::for_program(prog, state).unwrap();
    let o = &vm.objects[0];
    assert_eq!(o.get(&RcStr::from("count")), Some(&Value::PosInt(7)));
}

// ── effects lowering ──────────────────────────────────────────────

#[test]
fn tools_call_lowers_to_invoke() {
    // `tools.foo(a, b)` lowers to args-then-`Invoke("foo", 2)`.
    let prog = compile("tools.notify(1, 2);").expect("compiles");
    assert!(
        prog.code.contains(&Instr::Invoke("notify".into(), 2)),
        "expected Invoke in {:?}",
        prog.code
    );
}

#[test]
fn tools_call_with_no_args() {
    let prog = compile("tools.tick();").expect("compiles");
    assert!(prog.code.contains(&Instr::Invoke("tick".into(), 0)));
}

#[test]
fn raise_lowers_to_raise_instr() {
    let prog = compile("raise(\"need_input\");").expect("compiles");
    assert!(
        prog.code.contains(&Instr::Raise("need_input".into())),
        "expected Raise in {:?}",
        prog.code
    );
}

// ── optional-call lowering ────────────────────────────────────────

#[test]
fn optional_call_reclaims_static_builtin() {
    let prog = compile("Math.max?.(3, 7);").expect("compiles");
    assert!(
        prog.code
            .iter()
            .any(|i| matches!(i, Instr::CallBuiltin(Builtin::MathMax, 2))),
        "expected CallBuiltin(MathMax, 2), got {:?}",
        prog.code
    );
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::CallDyn(_) | Instr::JNotNullish(_))),
        "guard/CallDyn should have been reclaimed: {:?}",
        prog.code
    );
}

// ── closure / cell shape ──────────────────────────────────────────

#[test]
fn captured_loop_var_allocates_plain_not_boxed() {
    let prog = compile("let fns = []; for (let i = 0; i < 3; i++) { fns.push(() => i); }")
        .expect("compiles");
    let has_boxed = prog.code.iter().any(|i| matches!(i, Instr::EnterFrame(_, _, kinds) if kinds.iter().any(|k| *k == SlotKind::Boxed)));
    assert!(
        !has_boxed,
        "captured loop var should be Plain-allocated: {:?}",
        prog.code
    );
    assert!(
        prog.code.iter().any(|i| matches!(i, Instr::FreshCell(_))),
        "captured loop var should still be re-boxed per iteration: {:?}",
        prog.code
    );
}

#[test]
fn plain_loop_var_emits_no_fresh_cell() {
    let prog = compile("let s = 0; for (let i = 0; i < 3; i++) { s = s + i; }").expect("compiles");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::FreshCell(_))),
        "uncaptured loop var should not emit FreshCell: {:?}",
        prog.code
    );
}
