use super::*;
use crate::alloc_counter;
use crate::builtin::Builtin;
use crate::vm::{StepResult, VM};

/// Run a compiled program to completion via `for_program`, returning the
/// finished VM so the heap/stack can be inspected.
fn run_program(prog: Program) -> VM {
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step().unwrap() {
            StepResult::Done => return vm,
            other => panic!("unexpected effect: {other:?}"),
        }
    }
}

#[test]
fn const_folds_literal_arithmetic() {
    // `1 + 2 * 3` is fully constant-folded to `7`; as a bare value statement
    // it is then dead (pure push + Pop), leaving just the root `Return(0)`.
    let prog = compile("1 + 2 * 3;").expect("compiles");
    assert_eq!(prog.code, vec![Instr::Return(0)]);
    assert_eq!(prog.spans.len(), prog.code.len());
    let vm = run_program(prog);
    assert!(vm.stack.is_empty());

    // When the folded value is actually used, the constant lands in state.
    // (VM arithmetic yields `Number`, so 7 is stored as `Number(7.0)`.)
    let prog = compile("state.x = 1 + 2 * 3;").expect("compiles");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::Add | Instr::Mul)),
        "arithmetic should be folded away: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), num(7.0));
}

#[test]
fn peephole_folds_not_into_branch() {
    // `if (!cond) body` should NOT contain a `Not` immediately before a
    // conditional jump — the peephole folds it into the opposite branch.
    let prog = compile("let c = true; if (!c) { state.x = 1; }").expect("compiles");
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
    let prog = compile("let c = false; if (!c) { state.x = 1; }").expect("compiles");
    assert!(!prog.code.iter().any(|i| matches!(i, Instr::Not)));
    let vm = run_program(prog);
    let o = &vm.objects[0];
    assert_eq!(o.get(&RcStr::from("x")), Some(&Value::PosInt(1)));
}

#[test]
fn simplify_cfg_eliminates_dead_code_after_return() {
    // Code after an unconditional `return` (before any label) is unreachable
    // and must be dropped.
    let prog = compile("function f() { return 1; let x = 2; return x; } state.r = f();")
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
             state.sum = sum;",
    )
    .expect("compiles");
    let vm = run_program(prog);
    // 0+1+2 + 4+5+6 = 18 (3 skipped, break at 7).
    let o = &vm.objects[0];
    assert_eq!(o.get(&RcStr::from("sum")), Some(&Value::Float(18.0)));
}

#[test]
fn peephole_double_negation_compiles_to_tobool() {
    // A *runtime* operand (`state.c`) isn't const-folded, so `!!` exercises
    // the `Not;Not → ToBool` peephole: no `Not`, one `ToBool`.
    let prog = compile("state.b = !!state.c;").expect("compiles");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::Not)),
        "!! should fold to ToBool, no Not left: {:?}",
        prog.code
    );
    assert!(prog.code.iter().any(|i| matches!(i, Instr::ToBool)));
    // `state.c` is undefined here → `!!undefined` is `false`.
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
    let prog = compile("const N = 5; state.x = N * 2;").expect("compiles");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::Mul | Instr::Local(_))),
        "N should be propagated and folded: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), num(10.0));
}

#[test]
fn const_propagation_chains() {
    // A const initializer that reads earlier consts folds transitively.
    let prog = compile("const a = 3; const b = a + 1; state.x = b;").expect("compiles");
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), num(4.0));
}

#[test]
fn const_propagation_respects_shadowing() {
    // Each `const` gets a unique slot, so propagation never confuses an inner
    // shadow with the outer binding.
    let prog =
        compile("const x = 1; { const x = 2; state.a = x; } state.b = x;").expect("compiles");
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "a"), Value::PosInt(2));
    assert_eq!(state_val(&vm, "b"), Value::PosInt(1));
}

#[test]
fn let_is_not_propagated() {
    // A reassigned `let` must read its slot, never a stale literal.
    let prog = compile("let y = 5; y = 6; state.x = y;").expect("compiles");
    assert!(
        prog.code.iter().any(|i| matches!(i, Instr::Local(_))),
        "reassigned let must load: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), Value::PosInt(6));
}

#[test]
fn const_captured_by_closure_is_correct() {
    // `k` is a literal const, eliminated and resolved to its value inside the
    // hoisted `f` (cross-function const resolution via `resolve_captures`).
    let prog = compile("const k = 7; function f() { return k; } state.x = f();").expect("compiles");
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), Value::PosInt(7));
}

// ── Phase A: constant branch folding ─────────────────────────────

#[test]
fn const_branch_folds_dead_arm() {
    // `if (FLAG)` on a const folds the branch; the dead arm is pruned.
    let prog = compile("const FLAG = false; if (FLAG) { state.x = 1; } state.y = 2;").expect("ok");
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
    let prog = compile("const FLAG = true; if (FLAG) { state.x = 1; }").expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::JFalse(_) | Instr::JTrue(_) | Instr::Jump(_))),
        "live constant branch should leave no jump: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), Value::PosInt(1));
}

// ── Phase C: effectively-const `let` ─────────────────────────────

#[test]
fn effectively_const_let_propagates() {
    // A `let` never reassigned and never captured is propagated like a const.
    let prog = compile("let N = 5; state.x = N * 2;").expect("compiles");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::Mul | Instr::Local(_))),
        "effectively-const let should fold: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), num(10.0));
}

#[test]
fn later_reassignment_defeats_propagation_everywhere() {
    // A reassignment anywhere makes the binding non-immutable, so even the
    // use *before* it loads the slot (sound without dataflow).
    let prog = compile("let N = 5; state.x = N; N = 9; state.y = N;").expect("ok");
    assert!(
        prog.code.iter().any(|i| matches!(i, Instr::Local(_))),
        "a reassigned let must load: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), Value::PosInt(5));
    assert_eq!(state_val(&vm, "y"), Value::PosInt(9));
}

// ── Phase D: captured-const seeding into closures ────────────────

#[test]
fn captured_const_propagates_into_closure() {
    // `k` is a literal const → eliminated; inside the arrow `k * 2` folds to
    // `20`, and `f` captures nothing (no `Mul`).
    let prog = compile("const k = 10; const f = () => k * 2; state.x = f();").expect("ok");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::Mul)),
        "captured const should fold inside the closure: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), num(20.0));
}

// ── Phase E: constant binding elimination (slot + capture) ───────

#[test]
fn literal_const_has_no_slot_or_store() {
    // A literal `const` is a compile-time binding: no `SetLocal` (no store)
    // and no `Local` (reads are literals).
    let prog = compile("const N = 5; state.x = N;").expect("compiles");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::SetLocal(_) | Instr::Local(_))),
        "literal const should occupy no slot: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), Value::PosInt(5));
}

#[test]
fn const_only_closure_demotes_to_fn() {
    // `f` references only the literal const `k`, which is eliminated — so `f`
    // captures nothing and is a bare `Fn` (no `MakeClosure`, no heap closure).
    let prog = compile("const k = 5; const f = () => k; state.x = f();").expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..))),
        "const-only closure should demote to Fn: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), Value::PosInt(5));
}

#[test]
fn const_only_closure_in_loop_demotes_to_fn() {
    // The `map` callback references only a literal const → no per-iteration
    // closure allocation (bare `Fn`, not `MakeClosure`).
    let prog = compile("const f = 2; state.r = [1, 2, 3].map(x => x * f);").expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..))),
        "callback over a const should not allocate a closure: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    match state_val(&vm, "r") {
        Value::Array(p) => {
            let arr = &vm.arrays[p as usize];
            assert_eq!(arr.len(), 3);
            assert_eq!(arr[2], num(6.0));
        }
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn non_literal_captured_const_keeps_its_store() {
    // A *non-literal* const (an object) still gets a slot and is captured by
    // value, so its store is kept and the closure is a real `MakeClosure`.
    let prog = compile("const o = { v: 5 }; const f = () => o.v; state.x = f();").expect("ok");
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
    assert_eq!(state_val(&vm, "x"), Value::PosInt(5));
}

#[test]
fn const_elimination_respects_shadowing() {
    // Inner literal const shadows the outer; each reference resolves to its
    // own value even though neither occupies a slot.
    let prog = compile("const x = 1; { const x = 2; state.a = x; } state.b = x;").expect("ok");
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "a"), Value::PosInt(2));
    assert_eq!(state_val(&vm, "b"), Value::PosInt(1));
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
            "const k = 3; const outer = () => { const inner = () => k * 10; return inner(); }; state.x = outer();",
        )
        .expect("ok");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::Mul)),
        "transitive const should fold: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), num(30.0));
}

// ── Phase F: constant functions ──────────────────────────────────

#[test]
fn mutual_recursion_allocates_no_closures() {
    // Today's wart: `a`/`b` capture each other → two heap closures. As const
    // functions they capture nothing — no `MakeClosure`.
    let prog = compile(
        "function isEven(n){ return n === 0 ? true : isOdd(n - 1); } \
             function isOdd(n){ return n === 0 ? false : isEven(n - 1); } \
             state.x = isEven(10);",
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
    assert_eq!(state_val(&vm, "x"), Value::Bool(true));
}

#[test]
fn self_recursion_is_static_with_no_slot() {
    // A const function recurses via its own `Fn` constant: static `Call`,
    // no `CallDyn`, no `MakeClosure`, no self-slot setup.
    let prog =
        compile("function fact(n){ return n <= 1 ? 1 : n * fact(n - 1); } state.x = fact(5);")
            .expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::CallDyn(_) | Instr::MakeClosure(..))),
        "self-recursion should be static: {:?}",
        prog.code
    );
    // `n * fact(...)` is arithmetic → `Number`, so 120 is `Number(120.0)`.
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "x"), num(120.0));
}

#[test]
fn function_passed_as_value_is_fn_constant() {
    // A const function passed as a callback emits `PushFn` (no closure).
    let prog =
        compile("function dbl(x){ return x * 2; } state.r = [1, 2, 3].map(dbl);").expect("ok");
    assert!(prog.code.iter().any(|i| matches!(i, Instr::PushFn(_))));
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..)))
    );
    let vm = run_program(prog);
    match state_val(&vm, "r") {
        Value::Array(p) => {
            let a = &vm.arrays[p as usize];
            assert_eq!(a[2], num(6.0));
        }
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn function_capturing_real_var_stays_a_closure() {
    // `adder` captures `base` (a real `let`), so it must remain a real
    // closure — correctness preserved.
    let vm = run_vm(
        "function make() { let base = 100; \
               function adder(n) { return base + n; } \
               return adder(5); \
             } \
             state.x = make();",
    );
    assert_eq!(state_val(&vm, "x"), num(105.0));
}

#[test]
fn reassigned_function_is_not_a_constant() {
    // `f` is reassigned, so it isn't a const function (keeps a mutable slot);
    // compiling must succeed (no "assignment to constant") and run correctly.
    let prog = compile("function f() { return 1; } state.a = f(); f = 5; state.b = typeof f;")
        .expect("reassignable function binding");
    let vm = run_program(prog);
    assert_eq!(state_val(&vm, "a"), Value::PosInt(1));
    match state_val(&vm, "b") {
        Value::String(s) => assert_eq!(s.as_str(), "number"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn const_arrow_is_a_constant_function() {
    // A non-capturing arrow bound to a `const` is a constant function: no
    // closure, called/passed via its `Fn`.
    let prog = compile("const dbl = (x) => x * 2; state.r = [1, 2, 3].map(dbl); state.y = dbl(5);")
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
    assert_eq!(state_val(&vm, "y"), num(10.0));
    match state_val(&vm, "r") {
        Value::Array(p) => assert_eq!(vm.arrays[p as usize][2], num(6.0)),
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn const_named_fn_expr_self_recursion_is_static() {
    let prog = compile(
        "const fact = function f(n){ return n <= 1 ? 1 : n * f(n - 1); }; state.x = fact(4);",
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
    assert_eq!(state_val(&vm, "x"), num(24.0));
}

#[test]
fn capturing_const_arrow_stays_a_closure() {
    // A const arrow that captures a real `let` is still a real closure.
    let vm = run_vm(
        "function make() { let base = 100; const add = (n) => base + n; return add(5); } \
             state.x = make();",
    );
    assert_eq!(state_val(&vm, "x"), num(105.0));
}

#[test]
fn const_fn_reclaims_its_frame_slot() {
    // A constant function occupies no frame slot (caveat 2): the root frame
    // allocates only the real local `x`, not a slot for `f`. And renumbering
    // the survivor is correct.
    let prog = compile("function f() { return 1; } let x = 0; x = f(); state.x = x;").expect("ok");
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
    assert_eq!(state_val(&vm, "x"), Value::PosInt(1));
}

#[test]
fn const_fn_between_locals_renumbers_correctly() {
    // A const fn declared between two mutable locals must not corrupt their
    // slots when its slot is reclaimed.
    let vm = run_vm(
        "let a = 1; function f() { return 10; } let b = 2; \
             a = a + f(); b = b + f(); state.r = a * 100 + b;",
    );
    // a = 1 + 10 = 11; b = 2 + 10 = 12 → 11*100 + 12 = 1112.
    assert_eq!(state_val(&vm, "r"), num(1112.0));
}

#[test]
fn never_reassigned_let_function_is_constant() {
    // A `let` holding a non-capturing function, never reassigned, is just as
    // immutable as a `const` — so it's a constant function (no closure).
    let prog = compile("let dbl = (x) => x * 2; state.r = [1, 2, 3].map(dbl);").expect("ok");
    assert!(
        !prog
            .code
            .iter()
            .any(|i| matches!(i, Instr::MakeClosure(..))),
        "never-reassigned let function should be a Fn constant: {:?}",
        prog.code
    );
    let vm = run_program(prog);
    match state_val(&vm, "r") {
        Value::Array(p) => assert_eq!(vm.arrays[p as usize][2], num(6.0)),
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn reassigned_let_function_is_not_constant() {
    // Reassigning the binding defeats const-function treatment; still correct.
    let vm = run_vm("let f = () => 1; state.a = f(); f = () => 2; state.b = f();");
    assert_eq!(state_val(&vm, "a"), Value::PosInt(1));
    assert_eq!(state_val(&vm, "b"), Value::PosInt(2));
}

#[test]
fn var_function_is_not_a_constant() {
    // `var` is excluded (hoisted `undefined`): it stays an ordinary binding,
    // and still runs correctly.
    let vm = run_vm("var f = () => 7; state.x = f();");
    assert_eq!(state_val(&vm, "x"), Value::PosInt(7));
}

#[test]
fn for_program_seeds_state_at_heap0() {
    // The blessed `state` object always lives at objects[0], even when seeded.
    let prog = compile("1;").expect("compiles");
    let state = serde_json::json!({ "count": 7 });
    let vm = VM::for_program(prog, state).unwrap();
    let o = &vm.objects[0];
    assert_eq!(o.get(&RcStr::from("count")), Some(&Value::PosInt(7)));
}

#[test]
fn unsupported_statement_errors() {
    // An out-of-scope statement still produces a rendered diagnostic.
    let errs = compile("class C {}").expect_err("should not compile");
    assert_eq!(errs.len(), 1);
    // Renders as line:col with a caret.
    let rendered = errs[0].render("class C {}");
    assert!(rendered.starts_with("1:1: "), "got: {rendered}");
}

#[test]
fn syntax_error_is_reported() {
    // oxc's own syntax errors are surfaced as Diagnostics.
    let errs = compile("1 +* 2;").expect_err("syntax error");
    assert!(!errs.is_empty());
}

// ── Phase 1: expressions ───────────────────────────────────────────
//
// Most expression behavior is exercised end-to-end: compile a program that
// writes its result into `state.r`, run it, then read `objects[0]["r"]`. This
// routes every expression through the real VM and the `state`/`Ptr(0)`
// lowering at once.

/// Compile + run `src` to completion, returning the finished VM.
fn run_vm(src: &str) -> VM {
    match compile(src) {
        Ok(prog) => run_program(prog),
        Err(errs) => panic!("compile failed: {:?}", errs[0].render(src)),
    }
}

/// Read `state.<key>` (a slot of the objects[0] state object) from a finished VM.
fn state_val(vm: &VM, key: &str) -> Value {
    vm.objects[0]
        .get(&RcStr::from(key))
        .cloned()
        .unwrap_or_else(|| panic!("no state.{key}"))
}

/// Evaluate a single expression by assigning it to `state.r`, returning the
/// resulting `Value`.
fn eval(expr: &str) -> Value {
    let vm = run_vm(&format!("state.r = ({expr});"));
    state_val(&vm, "r")
}

/// Like `eval`, but resolves the result string to an owned `String`.
fn eval_str(expr: &str) -> String {
    let vm = run_vm(&format!("state.r = ({expr});"));
    match state_val(&vm, "r") {
        Value::String(s) => s.as_str().to_owned(),
        other => panic!("not a string: {other:?}"),
    }
}

fn num(v: f64) -> Value {
    Value::Float(v)
}

#[test]
fn literals() {
    assert_eq!(eval("42"), Value::PosInt(42));
    assert_eq!(eval("-7"), Value::NegInt(-7)); // folded literal
    assert_eq!(eval("3.5"), num(3.5));
    assert_eq!(eval("true"), Value::Bool(true));
    assert_eq!(eval("null"), Value::Null);
    assert_eq!(eval("undefined"), Value::Undefined);
    assert_eq!(eval_str("\"hi\""), "hi");
    assert!(matches!(eval("NaN"), Value::Float(n) if n.is_nan()));
    assert!(matches!(eval("Infinity"), Value::Float(n) if n.is_infinite()));
}

#[test]
fn arithmetic_and_operators() {
    assert_eq!(eval("1 + 2 * 3"), num(7.0));
    assert_eq!(eval("(1 + 2) * 3"), num(9.0));
    assert_eq!(eval("10 % 3"), num(1.0));
    assert_eq!(eval("2 ** 10"), num(1024.0));
    assert_eq!(eval("7 & 3"), num(3.0));
    assert_eq!(eval("1 << 4"), num(16.0));
    assert_eq!(eval("-5"), Value::NegInt(-5));
    assert_eq!(eval("+\"42\""), num(42.0)); // unary plus ToNumber
    assert_eq!(eval("!0"), Value::Bool(true));
    assert_eq!(eval("~0"), num(-1.0));
    assert_eq!(eval_str("\"a\" + \"b\""), "ab");
}

#[test]
fn comparisons_and_equality() {
    assert_eq!(eval("1 < 2"), Value::Bool(true));
    assert_eq!(eval("2 <= 2"), Value::Bool(true));
    assert_eq!(eval("3 === 3"), Value::Bool(true));
    assert_eq!(eval("3 !== 4"), Value::Bool(true));
    assert_eq!(eval("1 == \"1\""), Value::Bool(true)); // loose
    assert_eq!(eval("1 === \"1\""), Value::Bool(false)); // strict
    assert_eq!(eval("null == undefined"), Value::Bool(true));
}

#[test]
fn short_circuit_logical() {
    assert_eq!(eval("0 && 5"), Value::PosInt(0));
    assert_eq!(eval("3 && 5"), Value::PosInt(5));
    assert_eq!(eval("0 || 5"), Value::PosInt(5));
    assert_eq!(eval("3 || 5"), Value::PosInt(3));
    assert_eq!(eval("null ?? 5"), Value::PosInt(5));
    assert_eq!(eval("0 ?? 5"), Value::PosInt(0)); // 0 is not nullish
    assert_eq!(eval("undefined ?? 9"), Value::PosInt(9));
}

#[test]
fn short_circuit_does_not_evaluate_rhs() {
    // The RHS assignment must NOT run when the LHS short-circuits.
    let vm = run_vm("state.hit = 0; state.r = false && (state.hit = 1);");
    assert_eq!(state_val(&vm, "r"), Value::Bool(false));
    assert_eq!(state_val(&vm, "hit"), Value::PosInt(0));

    let vm = run_vm("state.hit = 0; state.r = true || (state.hit = 1);");
    assert_eq!(state_val(&vm, "r"), Value::Bool(true));
    assert_eq!(state_val(&vm, "hit"), Value::PosInt(0));
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
    let vm = run_vm("state.name = \"bob\"; state.r = `hi ${state.name}, ${1 + 2}!`;");
    match state_val(&vm, "r") {
        Value::String(s) => assert_eq!(s.as_bytes(), b"hi bob, 3!"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn arrays_and_objects() {
    // Array literal, length, index read.
    assert_eq!(eval("[10, 20, 30].length"), num(3.0));
    assert_eq!(eval("[10, 20, 30][1]"), Value::PosInt(20));
    assert_eq!(eval("[10, 20][5]"), Value::Undefined); // OOB read
    // Object literal + member read (static and computed).
    assert_eq!(eval("({ a: 1, b: 2 }).b"), Value::PosInt(2));
    assert_eq!(eval("({ a: 1, b: 2 })[\"a\"]"), Value::PosInt(1));
    assert_eq!(eval("({ a: 1 }).missing"), Value::Undefined);
    // Numeric key.
    assert_eq!(eval("({ 1: \"x\" })[1]"), eval("\"x\""));
}

#[test]
fn member_and_index_assignment() {
    // Static member assignment leaves the value and mutates the object.
    let vm = run_vm("state.obj = { a: 1 }; state.r = (state.obj.a = 9);");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(9));
    match state_val(&vm, "obj") {
        Value::Object(p) => {
            let o = &vm.objects[p as usize];
            assert_eq!(o.get(&RcStr::from("a")), Some(&Value::PosInt(9)))
        }
        other => panic!("{other:?}"),
    }
    // Index assignment into an array.
    let vm = run_vm("state.arr = [1, 2, 3]; state.arr[0] = 99; state.r = state.arr[0];");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(99));
    // Index assignment into an object (string key coercion).
    let vm = run_vm("state.o = {}; state.o[\"k\"] = 7; state.r = state.o.k;");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(7));
}

#[test]
fn optional_chaining() {
    // Missing base short-circuits to undefined; present base reads through.
    assert_eq!(eval("state.nope?.x"), Value::Undefined);
    let vm = run_vm("state.obj = { x: 7 }; state.r = state.obj?.x;");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(7));
    // A fully-optional chain short-circuits across links.
    assert_eq!(eval("state.nope?.a?.b"), Value::Undefined);
}

#[test]
fn optional_method_calls() {
    // Present receiver: the method runs normally (push returns new length).
    let vm = run_vm("state.arr = [1]; state.r = state.arr?.push(2);");
    assert_eq!(state_val(&vm, "r"), num(2.0));
    let vm = run_vm("state.arr = [1]; state.arr?.push(2); state.r = state.arr.length;");
    assert_eq!(state_val(&vm, "r"), num(2.0));

    // Nullish receiver: the whole call short-circuits to undefined.
    let vm = run_vm("state.r = state.nope?.push(2);");
    assert_eq!(state_val(&vm, "r"), Value::Undefined);

    // Short-circuit must NOT evaluate the arguments.
    let vm = run_vm("state.hit = 0; state.r = state.nope?.push(state.hit = 1);");
    assert_eq!(state_val(&vm, "r"), Value::Undefined);
    assert_eq!(state_val(&vm, "hit"), Value::PosInt(0));

    // String methods take the same optional path.
    let vm = run_vm("state.s = \"a,b,c\"; state.r = state.s?.split(\",\").length;");
    assert_eq!(state_val(&vm, "r"), num(3.0));
}

#[test]
fn first_class_builtin_refs() {
    // A namespaced builtin used as a value is a callable `Builtin`.
    assert_eq!(eval_str("typeof Math.sqrt"), "function");
}

#[test]
fn optional_invocation_calls() {
    // `?.()` on a real callable invokes it (via first-class builtin ref).
    assert_eq!(eval("Math.max?.(3, 7)"), num(7.0));
    assert_eq!(eval("Math.sqrt?.(9)"), num(3.0));

    // Stored builtin value, retrieved and optionally invoked.
    let vm = run_vm("state.f = Math.sqrt; state.r = state.f?.(16);");
    assert_eq!(state_val(&vm, "r"), num(4.0));

    // Nullish callee short-circuits to undefined.
    assert_eq!(eval("state.nope?.()"), Value::Undefined);

    // Short-circuit must NOT evaluate the arguments.
    let vm = run_vm("state.hit = 0; state.r = state.nope?.(state.hit = 1);");
    assert_eq!(state_val(&vm, "r"), Value::Undefined);
    assert_eq!(state_val(&vm, "hit"), Value::PosInt(0));

    // A present-but-non-callable callee is a runtime TypeError, like JS.
    let prog = compile("state.x = 5; state.x?.();").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let err = loop {
        match vm.step() {
            Ok(StepResult::Done) => panic!("expected a runtime error"),
            Ok(_) => continue,
            Err(e) => break e,
        }
    };
    assert!(matches!(err, crate::vm::VMError::TypeError), "got: {err:?}");
}

#[test]
fn optional_call_reclaims_static_builtin() {
    // A constant non-nullish callee makes the `?.` guard dead, so
    // `Math.max?.(…)` reclaims the static `CallBuiltin` — identical to
    // `Math.max(…)`, with no `JNotNullish`/`CallDyn`.
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
    // It still computes the right answer.
    assert_eq!(eval("Math.max?.(3, 7)"), num(7.0));
}

#[test]
fn in_and_delete() {
    let vm = run_vm("state.o = { a: 1 }; state.r = (\"a\" in state.o);");
    assert_eq!(state_val(&vm, "r"), Value::Bool(true));
    let vm = run_vm("state.o = { a: 1 }; state.r = (\"b\" in state.o);");
    assert_eq!(state_val(&vm, "r"), Value::Bool(false));
    // delete removes the key and returns whether it existed.
    let vm =
        run_vm("state.o = { a: 1 }; state.r = delete state.o.a; state.had = (\"a\" in state.o);");
    assert_eq!(state_val(&vm, "r"), Value::Bool(true));
    assert_eq!(state_val(&vm, "had"), Value::Bool(false));
}

#[test]
fn intrinsics_static() {
    assert_eq!(eval("Math.max(3, 7)"), num(7.0));
    assert_eq!(eval("Math.min(3, 7)"), num(3.0));
    assert_eq!(eval("Math.abs(-5)"), num(5.0));
    assert_eq!(eval("Math.floor(3.9)"), num(3.0));
    assert_eq!(eval("Math.pow(2, 5)"), num(32.0));
    assert_eq!(eval("Object.keys({ a: 1, b: 2 }).length"), num(2.0));
    assert_eq!(eval("Object.values({ a: 5 })[0]"), Value::PosInt(5));
    assert_eq!(eval("JSON.parse(\"[1,2,3]\").length"), num(3.0));
    assert_eq!(eval_str("JSON.stringify([1,2])"), "[1,2]");
    assert_eq!(eval("Number.isInteger(4)"), Value::Bool(true));
    assert_eq!(eval("Array.isArray([1])"), Value::Bool(true));
    assert_eq!(eval("Array.isArray(5)"), Value::Bool(false));
}

#[test]
fn intrinsics_global() {
    assert_eq!(eval_str("String(5)"), "5");
    assert_eq!(eval("Number(\"42\")"), num(42.0));
    assert_eq!(eval("Boolean(0)"), Value::Bool(false));
    assert_eq!(eval("Boolean(\"x\")"), Value::Bool(true));
}

#[test]
fn intrinsics_methods() {
    assert_eq!(eval("\"a,b,c\".split(\",\").length"), num(3.0));
    assert_eq!(eval("\"a,b,c\".split(\",\", 2).length"), num(2.0));
    assert_eq!(eval("\"hello\".includes(\"ell\")"), Value::Bool(true));
    assert_eq!(eval("\"hello\".startsWith(\"he\")"), Value::Bool(true));
    assert_eq!(eval("\"hello\".endsWith(\"lo\")"), Value::Bool(true));
    assert_eq!(eval("\"hello\".indexOf(\"l\")"), num(2.0));
    assert_eq!(eval_str("\"hello\".slice(1, 3)"), "el");
    assert_eq!(eval_str("\"  hi  \".trim()"), "hi");
    assert_eq!(eval_str("[\"a\", \"b\"].join(\"-\")"), "a-b");
    assert_eq!(eval_str("[1, 2].join()"), "1,2"); // default separator
    // Array mutators run and mutate the receiver.
    let vm = run_vm("state.arr = [1]; state.arr.push(2); state.r = state.arr.length;");
    assert_eq!(state_val(&vm, "r"), num(2.0));
    let vm = run_vm("state.arr = [1, 2, 3]; state.r = state.arr.pop();");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(3));
}

#[test]
fn diagnostics_for_unsupported() {
    // These all live in later phases / out of scope and must error cleanly.
    for src in [
        "x;",           // undeclared variable
        "x = 1;",       // assignment to undeclared variable
        "i++;",         // update of undeclared variable
        "tools;",       // bare `tools` is not a value
        "tools.send;",  // `tools.send` without a call
        "raise(x);",    // raise with a non-literal argument
        "raise();",     // raise with no argument
        "Math.tan(1);", // unsupported intrinsic
        "Math.pow(1);", // wrong arity (needs exactly 2)
        "f(...args);",  // spread arg
        "new Foo();",   // new
        "class C {}",   // class statement
    ] {
        assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
    }
}

#[test]
fn builtin_arity_is_enforced_from_meta() {
    // Wrong arities are rejected at compile time, with the accepted range
    // and the builtin name sourced from `Builtin::meta()`.
    for src in [
        "Math.pow(1);",              // needs exactly 2
        "Math.pow(1, 2, 3);",        // too many
        "Math.abs();",               // needs 1
        "\"x\".slice();",            // needs 1..2 args after receiver
        "\"x\".slice(1, 2, 3);",     // too many
        "[1].pop(2);",               // needs 0
        "Object.keys();",            // needs 1
        "Number.parseInt(1, 2, 3);", // needs 1..2
    ] {
        assert!(
            compile(src).is_err(),
            "expected `{src}` to fail arity check"
        );
    }

    // push() with no element is a no-op (returns length).
    assert!(compile("[1].push();").is_ok());

    // The diagnostic names the builtin and reports the receiver-free bounds.
    let errs = compile("\"x\".slice(1, 2, 3);").expect_err("too many args");
    let msg = &errs[0].message;
    assert!(msg.contains("`slice`"), "got: {msg}");
    assert!(msg.contains("1 to 2"), "got: {msg}");

    // Variadic `min`/`max` accept any count, including zero.
    assert_eq!(eval("Math.max()"), num(f64::NEG_INFINITY));
    assert_eq!(eval("Math.max(1, 2, 3, 4, 5)"), num(5.0));
}

#[test]
fn state_is_ptr_zero() {
    // Bare `state` is the objects[0] object pointer; the whole bag round-trips.
    let vm = run_vm("state.a = 1; state.r = JSON.stringify(state);");
    match state_val(&vm, "r") {
        // r was set last, so it appears in the serialized object too.
        Value::String(s) => {
            assert!(
                s.as_str().contains("\"a\":1"),
                "got {}",
                s.as_str().to_owned()
            )
        }
        other => panic!("{other:?}"),
    }
}

// ── Phase 4: effects (tools / raise) ────────────────────────────────

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
fn tools_call_yields_invoke_effect() {
    // End-to-end: a `tools.*` call yields an `Invoke` effect carrying the
    // method name and the evaluated args; the host pushes a result to resume.
    let prog = compile("state.r = tools.add(10, 3);").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Invoke { calls } => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "add");
            assert_eq!(calls[0].args, vec![Value::PosInt(10), Value::PosInt(3)]);
        }
        other => panic!("expected Invoke, got {other:?}"),
    }
    // Host resolves the call and pushes the result; the program stores it.
    vm.stack.push(Value::PosInt(13));
    loop {
        match vm.step().unwrap() {
            StepResult::Done => break,
            other => panic!("unexpected effect: {other:?}"),
        }
    }
    assert_eq!(state_val(&vm, "r"), Value::PosInt(13));
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

#[test]
fn raise_yields_effect_and_resumes_as_expression() {
    // `raise(...)` is an expression: it yields a `Raise` effect, then the
    // host pushes the resumed value which the program consumes.
    let prog = compile("state.r = raise(\"pick_a_number\");").expect("compiles");
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    match vm.step().unwrap() {
        StepResult::Raise { condition } => assert_eq!(condition, "pick_a_number"),
        other => panic!("expected Raise, got {other:?}"),
    }
    // Resume restart: advance past the Raise and push the resumed value.
    vm.ip += 1;
    vm.stack.push(Value::PosInt(42));
    loop {
        match vm.step().unwrap() {
            StepResult::Done => break,
            other => panic!("unexpected effect: {other:?}"),
        }
    }
    assert_eq!(state_val(&vm, "r"), Value::PosInt(42));
}

// ── Phase 2: statements / control flow ──────────────────────────────

#[test]
fn local_declarations_and_reassignment() {
    assert_eq!(eval_phase2("let x = 5; return x;"), Value::PosInt(5));
    assert_eq!(eval_phase2("const x = 7; return x;"), Value::PosInt(7));
    assert_eq!(eval_phase2("let x = 1; x = 2; return x;"), Value::PosInt(2));
    // Uninitialized local is `undefined`.
    assert_eq!(eval_phase2("let x; return x;"), Value::Undefined);
    // Multiple declarators in one statement.
    assert_eq!(eval_phase2("let a = 1, b = 2; return a + b;"), num(3.0));
}

#[test]
fn block_scoping() {
    // An inner block shadows; the outer binding is restored after.
    let vm = run_vm("let x = 1; { let x = 2; state.inner = x; } state.outer = x;");
    assert_eq!(state_val(&vm, "inner"), Value::PosInt(2));
    assert_eq!(state_val(&vm, "outer"), Value::PosInt(1));
}

#[test]
fn var_is_function_scoped_and_hoisted() {
    // `var` is visible (as undefined) before its declaration runs.
    let vm = run_vm("state.before = typeof x; var x = 5; state.after = x;");
    assert_eq!(eval_str_in(&vm, "before"), "undefined");
    assert_eq!(state_val(&vm, "after"), Value::PosInt(5));
    // A `var` in a block belongs to the function scope.
    assert_eq!(eval_phase2("{ var y = 9; } return y;"), Value::PosInt(9));
}

#[test]
fn if_else() {
    assert_eq!(
        eval_phase2("let r; if (1 > 0) r = 10; else r = 20; return r;"),
        Value::PosInt(10)
    );
    assert_eq!(
        eval_phase2("let r; if (0) r = 10; else r = 20; return r;"),
        Value::PosInt(20)
    );
    // Dangling-if with no else leaves the prior value.
    assert_eq!(
        eval_phase2("let r = 3; if (false) r = 9; return r;"),
        Value::PosInt(3)
    );
    // else-if chains.
    assert_eq!(
        eval_phase2(
            "let x = 2, r; if (x === 1) r = 1; else if (x === 2) r = 2; else r = 3; return r;"
        ),
        Value::PosInt(2)
    );
}

#[test]
fn while_loop() {
    assert_eq!(
        eval_phase2("let i = 0, s = 0; while (i < 5) { s += i; i += 1; } return s;"),
        num(10.0)
    );
}

#[test]
fn while_continue_retests() {
    // `continue` in a `while` jumps back to the test (no update clause), so a
    // manual increment before it avoids an infinite loop and `i === 3` skips.
    assert_eq!(
        eval_phase2(
            "let i = 0, s = 0; while (i < 5) { i++; if (i === 3) continue; s += i; } return s;"
        ),
        num(12.0)
    );
}

#[test]
fn for_with_expression_initializer() {
    // The `for` init may be a plain expression (no declaration); `i` is an
    // outer local that the loop mutates.
    assert_eq!(
        eval_phase2("let i, s = 0; for (i = 0; i < 4; i++) { s += i; } return s;"),
        num(6.0)
    );
}

#[test]
fn do_while_loop() {
    // Body always runs at least once, even with a false test.
    assert_eq!(
        eval_phase2("let n = 0; do { n += 1; } while (n < 3); return n;"),
        num(3.0)
    );
    assert_eq!(
        eval_phase2("let n = 0; do { n += 1; } while (false); return n;"),
        num(1.0)
    );
}

#[test]
fn for_loop() {
    assert_eq!(
        eval_phase2("let s = 0; for (let i = 0; i < 5; i++) { s += i; } return s;"),
        num(10.0)
    );
    // Empty clauses: `for (;;)` with an internal break.
    assert_eq!(
        eval_phase2("let i = 0; for (;;) { if (i >= 3) break; i++; } return i;"),
        num(3.0)
    );
}

#[test]
fn break_and_continue() {
    // break stops the loop early.
    assert_eq!(
        eval_phase2(
            "let s = 0; for (let i = 0; i < 10; i++) { if (i === 3) break; s += i; } return s;"
        ),
        num(3.0)
    );
    // continue skips the rest of the body (the for-update still runs).
    assert_eq!(
        eval_phase2(
            "let s = 0; for (let i = 0; i < 5; i++) { if (i % 2 === 0) continue; s += i; } return s;"
        ),
        num(4.0)
    );
    // break only exits the innermost loop.
    assert_eq!(
        eval_phase2(
            "let c = 0; for (let i = 0; i < 3; i++) { for (let j = 0; j < 3; j++) { if (j === 1) break; c++; } } return c;"
        ),
        num(3.0)
    );
}

#[test]
fn for_of_array() {
    // Sum the values of an array.
    assert_eq!(
        eval_phase2("let s = 0; for (const x of [1, 2, 3, 4]) { s += x; } return s;"),
        num(10.0)
    );
    // `let` binding, body without braces.
    assert_eq!(
        eval_phase2("let s = 0; for (let x of [10, 20]) s += x; return s;"),
        num(30.0)
    );
    // Empty array: body never runs (the literal is untouched).
    assert_eq!(
        eval_phase2("let s = 99; for (const x of []) s = 0; return s;"),
        Value::PosInt(99)
    );
}

#[test]
fn for_of_string_chars() {
    // for-of over a string yields its characters.
    assert_eq!(
        eval_str_phase2("let r = \"\"; for (const c of \"abc\") r = c + r; return r;"),
        "cba"
    );
}

#[test]
fn for_of_break_and_continue() {
    // break exits early.
    assert_eq!(
        eval_phase2(
            "let s = 0; for (const x of [1, 2, 3, 4]) { if (x === 3) break; s += x; } return s;"
        ),
        num(3.0)
    );
    // continue skips an element.
    assert_eq!(
        eval_phase2(
            "let s = 0; for (const x of [1, 2, 3, 4]) { if (x % 2 === 0) continue; s += x; } return s;"
        ),
        num(4.0)
    );
    // Nested for-of: break exits only the inner loop.
    assert_eq!(
        eval_phase2(
            "let c = 0; for (const i of [1, 2, 3]) { for (const j of [1, 2, 3]) { if (j === 2) break; c++; } } return c;"
        ),
        num(3.0)
    );
}

#[test]
fn for_in_object_keys() {
    // for-in yields the keys (insertion order) of an object.
    let vm = run_vm(
        "state.o = { a: 1, b: 2, c: 3 }; state.r = \"\"; for (const k in state.o) { state.r = state.r + k; }",
    );
    assert_eq!(eval_str_in(&vm, "r"), "abc");
    // Sum the values by indexing back into the object with each key.
    let vm = run_vm(
        "state.o = { a: 1, b: 2, c: 3 }; let s = 0; for (const k in state.o) { s += state.o[k]; } state.r = s;",
    );
    assert_eq!(state_val(&vm, "r"), num(6.0));
}

#[test]
fn for_in_over_state() {
    // for-in over the blessed `state` object enumerates its keys.
    let vm =
        run_vm("state.x = 1; state.y = 2; let n = 0; for (const k in state) n++; state.r = n;");
    assert_eq!(state_val(&vm, "r"), num(2.0));
}

#[test]
fn for_of_in_diagnostics() {
    // Unsupported head forms record a clean diagnostic.
    for src in [
        "for (const [a, b] of [[1, 2]]) {}", // destructuring binding
        "for (x of [1]) {}",                 // bare assignment target (undeclared)
    ] {
        assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
    }
}

#[test]
fn switch_basic_and_fallthrough() {
    // A matching case runs and `break` stops fall-through.
    assert_eq!(
        eval_phase2(
            "let r = 0; switch (2) { case 1: r = 1; break; case 2: r = 2; break; case 3: r = 3; break; } return r;"
        ),
        Value::PosInt(2)
    );
    // No break: execution falls through into the next case.
    assert_eq!(
        eval_phase2(
            "let r = 0; switch (1) { case 1: r += 1; case 2: r += 10; break; case 3: r += 100; } return r;"
        ),
        num(11.0)
    );
    // default runs when nothing matches.
    assert_eq!(
        eval_phase2("let r = 0; switch (9) { case 1: r = 1; break; default: r = 42; } return r;"),
        Value::PosInt(42)
    );
    // default in the middle, reached by fall-through from a later... actually
    // default is dispatched only when no case matches; here 1 matches.
    assert_eq!(
        eval_phase2(
            "let r = 0; switch (1) { default: r = 42; break; case 1: r = 7; break; } return r;"
        ),
        Value::PosInt(7)
    );
    // Strict (===) matching: a string discriminant does not match a number.
    assert_eq!(
        eval_phase2(
            "let r = 0; switch (\"1\") { case 1: r = 1; break; default: r = 2; } return r;"
        ),
        Value::PosInt(2)
    );
}

#[test]
fn switch_break_only_continue_escapes() {
    // `break` inside a switch breaks the switch, not the enclosing loop.
    assert_eq!(
        eval_phase2(
            "let s = 0; for (let i = 0; i < 3; i++) { switch (i) { case 1: break; default: s += i; } } return s;"
        ),
        num(2.0) // i=0 (default, +0) and i=2 (default, +2); i=1 breaks the switch
    );
    // `continue` inside a switch continues the enclosing loop.
    assert_eq!(
        eval_phase2(
            "let s = 0; for (let i = 0; i < 4; i++) { switch (i) { case 2: continue; default: break; } s += i; } return s;"
        ),
        num(4.0) // i=2 continues (skips s+=i); 0+1+3 = 4
    );
}

#[test]
fn switch_lexical_decls_share_block() {
    // A `let` in one case is visible (one block) but slot-distinct per name.
    assert_eq!(
        eval_phase2(
            "let r = 0; switch (1) { case 1: { let x = 5; r = x; break; } default: r = 0; } return r;"
        ),
        Value::PosInt(5)
    );
}

#[test]
fn switch_continue_outside_loop_errors() {
    // `continue` in a switch with no enclosing loop is an error.
    assert!(compile("switch (1) { case 1: continue; }").is_err());
}

// ── Phase 4.0: higher-order array methods (prelude) ─────────────────

#[test]
fn hof_map_filter() {
    // map applies the callback to each element.
    let vm = run_vm("state.r = [1, 2, 3].map(x => x * 2);");
    match state_val(&vm, "r") {
        Value::Array(p) => assert_eq!(vm.arrays[p as usize].len(), 3),
        other => panic!("{other:?}"),
    }
    // map result summed back via reduce.
    assert_eq!(
        eval_phase2("let a = [1, 2, 3].map(x => x * 2); return a[0] + a[1] + a[2];"),
        num(12.0)
    );
    // filter keeps matching elements.
    assert_eq!(
        eval_phase2("let a = [1, 2, 3, 4].filter(x => x % 2 === 0); return a.length;"),
        num(2.0)
    );
}

#[test]
fn hof_reduce_both_forms() {
    // reduce with an initial value.
    assert_eq!(
        eval_phase2("return [1, 2, 3, 4].reduce((s, x) => s + x, 0);"),
        num(10.0)
    );
    // reduce without an initial value (seeds from element 0).
    assert_eq!(
        eval_phase2("return [1, 2, 3, 4].reduce((s, x) => s + x);"),
        num(10.0)
    );
}

#[test]
fn hof_search_methods() {
    assert_eq!(
        eval_phase2("return [1, 2, 3].some(x => x === 2);"),
        Value::Bool(true)
    );
    assert_eq!(
        eval_phase2("return [1, 2, 3].every(x => x > 0);"),
        Value::Bool(true)
    );
    assert_eq!(
        eval_phase2("return [1, 2, 3].every(x => x > 1);"),
        Value::Bool(false)
    );
    // find returns the matching element (an untouched literal here).
    assert_eq!(
        eval_phase2("return [5, 6, 7].find(x => x > 5);"),
        Value::PosInt(6)
    );
    assert_eq!(
        eval_phase2("return [5, 6, 7].findIndex(x => x === 7);"),
        num(2.0)
    );
    // find with no match → undefined; findIndex with no match → -1.
    assert_eq!(
        eval_phase2("return [1, 2].find(x => x > 9);"),
        Value::Undefined
    );
    assert_eq!(
        eval_phase2("return [1, 2].findIndex(x => x > 9);"),
        Value::NegInt(-1)
    );
}

#[test]
fn hof_foreach_side_effects() {
    // forEach runs the callback for its effects and returns undefined.
    let vm = run_vm("state.sum = 0; [1, 2, 3].forEach(x => { state.sum += x; });");
    assert_eq!(state_val(&vm, "sum"), num(6.0));
}

#[test]
fn hof_callback_index_and_array_args() {
    // The callback receives (element, index, array).
    assert_eq!(
        eval_phase2("return [10, 20, 30].map((x, i) => x + i).reduce((s, x) => s + x, 0);"),
        num(63.0) // (10+0)+(20+1)+(30+2) = 63
    );
}

#[test]
fn hof_closure_callback_captures() {
    // A callback closing over an enclosing local works (CallDyn path).
    assert_eq!(
        eval_phase2("let k = 10; return [1, 2, 3].map(x => x + k).reduce((s, x) => s + x, 0);"),
        num(36.0) // (1+10)+(2+10)+(3+10) = 36
    );
}

#[test]
fn hof_chained_and_nested() {
    // Chained higher-order methods.
    assert_eq!(
        eval_phase2(
            "return [1, 2, 3, 4, 5].filter(x => x % 2 === 1).map(x => x * x).reduce((s, x) => s + x, 0);"
        ),
        num(35.0) // 1 + 9 + 25
    );
}

#[test]
fn hof_inside_user_function() {
    // A higher-order call inside a user function resolves the top-level
    // prelude helper from a nested scope. (Uses `run_vm` directly because
    // the function body has its own `return`.)
    let vm = run_vm(
        "function total(a) { return a.map(x => x + 1).reduce((s, x) => s + x, 0); } state.r = total([1, 2, 3]);",
    );
    assert_eq!(state_val(&vm, "r"), num(9.0)); // 2 + 3 + 4
}

#[test]
fn hof_arity_errors() {
    assert!(compile("[1].map();").is_err()); // needs a callback
    assert!(compile("[1].reduce();").is_err()); // needs 1 or 2 args
}

// ── Phase 4: `arguments` ────────────────────────────────────────────

#[test]
fn arguments_variadic_sum() {
    // A param-less function reads all of its args through `arguments`.
    let vm = run_vm(
        "function sum() { let t = 0; for (let i = 0; i < arguments.length; i++) { t += arguments[i]; } return t; } state.r = sum(1, 2, 3, 4);",
    );
    assert_eq!(state_val(&vm, "r"), num(10.0));
}

#[test]
fn arguments_beyond_declared_params() {
    // Arguments past the declared parameters are still visible.
    let vm = run_vm("function f(a) { return a + arguments.length; } state.r = f(10, 20, 30);");
    assert_eq!(state_val(&vm, "r"), num(13.0)); // 10 + 3
}

#[test]
fn arguments_is_cached_per_frame() {
    // Two references in the same frame yield the *same* array object
    // (reference-equal under `===`), which only holds if the per-frame
    // cache reuses one build instead of materializing a fresh array each
    // time.
    let vm = run_vm("function f() { return arguments === arguments; } state.r = f(1, 2);");
    assert_eq!(state_val(&vm, "r"), Value::Bool(true));
}

#[test]
fn arguments_can_be_shadowed() {
    // A real binding named `arguments` shadows the frame-args array.
    let vm = run_vm("function f() { let arguments = 42; return arguments; } state.r = f(1, 2, 3);");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(42));
}

#[test]
fn arguments_in_arrow_is_own_frame() {
    // Accepted divergence from JS (where an arrow inherits the enclosing
    // `arguments`): here an arrow's `arguments` is its own frame's args.
    let vm = run_vm("let f = (a) => arguments.length; state.r = f(1, 2, 3);");
    assert_eq!(state_val(&vm, "r"), num(3.0));
}

#[test]
fn arguments_at_top_level_is_empty() {
    // The root frame has no args, so top-level `arguments` is an empty array.
    let vm = run_vm("state.r = arguments.length;");
    assert_eq!(state_val(&vm, "r"), num(0.0));
}

#[test]
fn hof_bare_builtin_callback() {
    // A namespaced builtin passed directly as the callback works: the helper
    // invokes it with (element, index, array) and the builtin ignores the
    // surplus args (flexible arity from `Builtin::meta`).
    assert_eq!(
        eval_phase2("return [4, 9, 16].map(Math.sqrt).reduce((s, x) => s + x, 0);"),
        num(9.0) // 2 + 3 + 4
    );
}

#[test]
fn compound_assignment() {
    // Local targets.
    assert_eq!(eval_phase2("let x = 5; x += 3; return x;"), num(8.0));
    assert_eq!(eval_phase2("let x = 5; x -= 2; return x;"), num(3.0));
    assert_eq!(eval_phase2("let x = 5; x *= 2; return x;"), num(10.0));
    assert_eq!(eval_phase2("let x = 2; x **= 3; return x;"), num(8.0));
    assert_eq!(eval_phase2("let x = 7; x %= 3; return x;"), num(1.0));
    assert_eq!(eval_phase2("let x = 1; x <<= 3; return x;"), num(8.0));
    // String `+=` concatenates.
    assert_eq!(
        eval_str_phase2("let s = \"a\"; s += \"b\"; return s;"),
        "ab"
    );
    // Member target.
    let vm = run_vm("state.o = { a: 1 }; state.o.a += 4; state.r = state.o.a;");
    assert_eq!(state_val(&vm, "r"), num(5.0));
    // Index target (key evaluated once).
    let vm = run_vm("state.arr = [1, 2]; state.arr[0] += 10; state.r = state.arr[0];");
    assert_eq!(state_val(&vm, "r"), num(11.0));
    // Compound assignment is an expression yielding the new value.
    assert_eq!(eval_phase2("let x = 5; return (x += 5);"), num(10.0));
}

#[test]
fn logical_assignment() {
    assert_eq!(
        eval_phase2("let x = 0; x ||= 5; return x;"),
        Value::PosInt(5)
    );
    assert_eq!(
        eval_phase2("let x = 3; x ||= 5; return x;"),
        Value::PosInt(3)
    );
    assert_eq!(
        eval_phase2("let x = 3; x &&= 7; return x;"),
        Value::PosInt(7)
    );
    assert_eq!(
        eval_phase2("let x = 0; x &&= 7; return x;"),
        Value::PosInt(0)
    );
    assert_eq!(
        eval_phase2("let x = null; x ??= 9; return x;"),
        Value::PosInt(9)
    );
    assert_eq!(
        eval_phase2("let x = 0; x ??= 9; return x;"),
        Value::PosInt(0)
    );

    // Short-circuit must NOT evaluate the RHS (nor store).
    let vm = run_vm("state.hit = 0; let x = 3; x ||= (state.hit = 1); state.r = x;");
    assert_eq!(state_val(&vm, "hit"), Value::PosInt(0));
    assert_eq!(state_val(&vm, "r"), Value::PosInt(3));

    // Member target, store path.
    let vm = run_vm("state.o = { a: null }; state.o.a ??= 5; state.r = state.o.a;");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(5));
    // Member target, keep path (address values cleaned up).
    let vm = run_vm("state.o = { a: 2 }; state.r = (state.o.a ??= 99);");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(2));
    // Index target, keep path.
    let vm = run_vm("state.arr = [7]; state.r = (state.arr[0] ||= 1);");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(7));
}

#[test]
fn increment_decrement() {
    // Postfix returns the old value, prefix the new.
    let vm = run_vm("let x = 5; state.a = x++; state.b = x;");
    assert_eq!(state_val(&vm, "a"), num(5.0));
    assert_eq!(state_val(&vm, "b"), num(6.0));
    let vm = run_vm("let y = 5; state.a = ++y; state.b = y;");
    assert_eq!(state_val(&vm, "a"), num(6.0));
    assert_eq!(state_val(&vm, "b"), num(6.0));
    // Decrement.
    assert_eq!(eval_phase2("let x = 5; x--; return x;"), num(4.0));
    assert_eq!(eval_phase2("let x = 5; return --x;"), num(4.0));
    // `++` coerces like ToNumber (string "5" → 6, not "51").
    assert_eq!(eval_phase2("let x = \"5\"; x++; return x;"), num(6.0));
    // Member / index targets — postsets now preserves the exact old value.
    let vm = run_vm("state.o = { n: 1 }; state.r = state.o.n++; state.after = state.o.n;");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(1));
    assert_eq!(state_val(&vm, "after"), num(2.0));
    let vm = run_vm("state.arr = [10]; state.r = ++state.arr[0]; state.after = state.arr[0];");
    assert_eq!(state_val(&vm, "r"), num(11.0));
    assert_eq!(state_val(&vm, "after"), num(11.0));
}

#[test]
fn array_destructuring_declaration() {
    let vm = run_vm("let [a, b] = [10, 20]; state.a = a; state.b = b;");
    assert_eq!(state_val(&vm, "a"), Value::PosInt(10));
    assert_eq!(state_val(&vm, "b"), Value::PosInt(20));
    // Holes skip elements.
    assert_eq!(
        eval_phase2("let [, b] = [1, 2]; return b;"),
        Value::PosInt(2)
    );
    // Defaults apply only when the element is undefined.
    assert_eq!(eval_phase2("let [a = 5] = []; return a;"), Value::PosInt(5));
    assert_eq!(
        eval_phase2("let [a = 5] = [1]; return a;"),
        Value::PosInt(1)
    );
    // Nested.
    let vm = run_vm("let [[a], { b }] = [[1], { b: 2 }]; state.a = a; state.b = b;");
    assert_eq!(state_val(&vm, "a"), Value::PosInt(1));
    assert_eq!(state_val(&vm, "b"), Value::PosInt(2));
}

#[test]
fn object_destructuring_declaration() {
    let vm = run_vm("let { x, y } = { x: 1, y: 2 }; state.x = x; state.y = y;");
    assert_eq!(state_val(&vm, "x"), Value::PosInt(1));
    assert_eq!(state_val(&vm, "y"), Value::PosInt(2));
    // Renaming and defaults.
    assert_eq!(
        eval_phase2("let { a: aa } = { a: 7 }; return aa;"),
        Value::PosInt(7)
    );
    assert_eq!(
        eval_phase2("let { b = 3 } = {}; return b;"),
        Value::PosInt(3)
    );
    assert_eq!(
        eval_phase2("let { b = 3 } = { b: 9 }; return b;"),
        Value::PosInt(9)
    );
}

#[test]
fn destructuring_assignment() {
    let vm = run_vm("let a, b; [a, b] = [3, 4]; state.a = a; state.b = b;");
    assert_eq!(state_val(&vm, "a"), Value::PosInt(3));
    assert_eq!(state_val(&vm, "b"), Value::PosInt(4));
    // Object destructuring assignment needs parens.
    let vm = run_vm("let x, y; ({ x, y } = { x: 5, y: 6 }); state.x = x; state.y = y;");
    assert_eq!(state_val(&vm, "x"), Value::PosInt(5));
    assert_eq!(state_val(&vm, "y"), Value::PosInt(6));
    // Renamed object target.
    let vm = run_vm("let z; ({ a: z } = { a: 8 }); state.z = z;");
    assert_eq!(state_val(&vm, "z"), Value::PosInt(8));
}

#[test]
fn let_without_init_resets_each_iteration() {
    // A bare `let x;` re-initializes to undefined on each loop entry, so a
    // value set only on the first iteration does not leak into the next.
    let vm = run_vm(
        "let last; for (let i = 0; i < 2; i++) { let x; if (i === 0) x = 5; last = x; } state.r = last;",
    );
    assert_eq!(state_val(&vm, "r"), Value::Undefined);
}

#[test]
fn phase2_diagnostics() {
    for src in [
        "const x = 1; x = 2;",              // const reassignment
        "const x = 1; x += 1;",             // const compound
        "const x = 1; x++;",                // const update
        "let state = 1;",                   // shadowing blessed `state`
        "y = 1;",                           // assignment to undeclared
        "break;",                           // break outside a loop
        "continue;",                        // continue outside a loop
        "let [a, ...rest] = [1, 2];",       // rest in destructuring
        "outer: while (true) break outer;", // labeled statements
    ] {
        assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
    }
    // Spot-check messages.
    let errs = compile("const x = 1; x = 2;").expect_err("const");
    assert!(
        errs[0].message.contains("constant"),
        "got: {}",
        errs[0].message
    );
    let errs = compile("let [a, ...rest] = [1, 2];").expect_err("rest");
    assert!(errs[0].message.contains("rest"), "got: {}", errs[0].message);
}

// ── Phase 2 test helpers ────────────────────────────────────────────

/// Run a statement sequence ending in `return <expr>;`, rewritten as the
/// final expression assigned to `state.__ret`, and return that value. Lets
/// tests read the result of code that uses locals/control flow.
fn eval_phase2(src: &str) -> Value {
    let rewritten = src.replacen("return ", "state.__ret = ", 1);
    let vm = run_vm(&rewritten);
    state_val(&vm, "__ret")
}

/// Like [`eval_phase2`], but resolves the heap string result.
fn eval_str_phase2(src: &str) -> String {
    let rewritten = src.replacen("return ", "state.__ret = ", 1);
    let vm = run_vm(&rewritten);
    match state_val(&vm, "__ret") {
        Value::String(s) => s.as_str().to_owned(),
        other => panic!("not a string: {other:?}"),
    }
}

/// Read `state.<key>` as an owned string from a finished VM.
fn eval_str_in(vm: &VM, key: &str) -> String {
    match state_val(vm, key) {
        Value::String(s) => s.as_str().to_owned(),
        other => panic!("not a string: {other:?}"),
    }
}

// ── Phase 3: functions / closures ─────────────────────────────────

/// Run `src` and return `state.r`. Phase 3: programs can define and call
/// functions; we wrap the result in a well-known state slot.
fn eval_phase3(src: &str) -> Value {
    let vm = run_vm(src);
    state_val(&vm, "r")
}

#[test]
fn function_declaration_and_call() {
    assert_eq!(
        eval_phase3("function add(a, b) { return a + b; } state.r = add(3, 4);"),
        num(7.0)
    );
}

#[test]
fn function_hoisting_forward_reference() {
    assert_eq!(
        eval_phase3("state.r = add(2, 3); function add(a, b) { return a + b; }"),
        num(5.0)
    );
}

#[test]
fn function_return_without_value() {
    assert_eq!(
        eval_phase3("function f() { return; } state.r = f();"),
        Value::Undefined
    );
}

#[test]
fn function_implicit_return() {
    assert_eq!(
        eval_phase3("function f() {} state.r = f();"),
        Value::Undefined
    );
}

#[test]
fn parameter_defaults() {
    // Default applied when called without an argument: the compiler
    // pads with Undefined, which triggers the default expression.
    assert_eq!(
        eval_phase3("function f(x = 5) { return x; } state.r = f();"),
        Value::PosInt(5)
    );
    assert_eq!(
        eval_phase3("function f(x = 5) { return x; } state.r = f(9);"),
        Value::PosInt(9)
    );
}

#[test]
fn function_expression() {
    assert_eq!(
        eval_phase3("let add = function(a, b) { return a + b; }; state.r = add(5, 6);"),
        num(11.0)
    );
}

#[test]
fn arrow_expression_body() {
    // Arrow with expression body implicitly returns.
    assert_eq!(
        eval_phase3("let add = (a, b) => a + b; state.r = add(3, 4);"),
        num(7.0)
    );
}

#[test]
fn arrow_block_body() {
    assert_eq!(
        eval_phase3("let f = (x) => { return x * 2; }; state.r = f(7);"),
        num(14.0)
    );
}

#[test]
fn recursion() {
    assert_eq!(
        eval_phase3(
            "function fact(n) { if (n <= 1) return 1; return n * fact(n - 1); } state.r = fact(5);"
        ),
        num(120.0)
    );
}

#[test]
fn mutual_recursion() {
    assert_eq!(
        eval_phase3(
            "function isEven(n) { if (n === 0) return true; return isOdd(n - 1); } function isOdd(n) { if (n === 0) return false; return isEven(n - 1); } state.r = isEven(4);"
        ),
        Value::Bool(true)
    );
}

#[test]
fn closure_captures_local() {
    // Simple closure: inner function captures outer variable by value.
    let vm = run_vm(
        "function makeAdder(x) { return function(y) { return x + y; }; } state.add5 = makeAdder(5); state.r = state.add5(3);",
    );
    assert_eq!(state_val(&vm, "r"), num(8.0));
}

#[test]
fn closure_mutation_visible() {
    let vm = run_vm(
        "function makeCounter() { let count = 0; function inc() { count = count + 1; return count; } return inc; } state.c1 = makeCounter(); state.c1(); state.r = state.c1();",
    );
    assert_eq!(state_val(&vm, "r"), num(2.0));
}

// ── per-iteration capture: each loop iteration's closure gets its own cell ──

#[test]
fn for_let_head_var_captured_per_iteration() {
    // Closures created in different iterations must capture distinct copies
    // of the for-head `let` variable (classic [0,1,2], not [3,3,3]).
    let vm = run_vm(
        "let fns = []; \
             for (let i = 0; i < 3; i++) { fns.push(() => i); } \
             let a = fns[0], b = fns[1], c = fns[2]; \
             state.r = a() * 100 + b() * 10 + c();",
    );
    assert_eq!(state_val(&vm, "r"), num(12.0)); // 0,1,2
}

#[test]
fn for_body_declared_var_captured_per_iteration() {
    // A captured binding *declared in the body* also needs a fresh cell each
    // iteration, even though the for-head variable isn't captured here.
    let vm = run_vm(
        "let fns = []; \
             for (let i = 0; i < 3; i++) { let j = i * 2; fns.push(() => j); } \
             let a = fns[0], b = fns[1], c = fns[2]; \
             state.r = a() * 100 + b() * 10 + c();",
    );
    assert_eq!(state_val(&vm, "r"), num(24.0)); // 0,2,4
}

#[test]
fn for_of_loop_var_captured_per_iteration() {
    let vm = run_vm(
        "let fns = []; \
             for (const x of [10, 20, 30]) { fns.push(() => x); } \
             let a = fns[0], b = fns[1], c = fns[2]; \
             state.r = a() * 100 + b() * 10 + c();",
    );
    assert_eq!(state_val(&vm, "r"), num(1230.0)); // 10,20,30
}

#[test]
fn for_in_loop_var_captured_per_iteration() {
    let vm = run_vm(
        "let fns = []; let obj = { a: 1, b: 2 }; \
             for (const k in obj) { fns.push(() => k); } \
             let a = fns[0], b = fns[1]; \
             state.r = a() + b();",
    );
    assert_eq!(eval_str_in(&vm, "r"), "ab"); // keys 'a','b', not 'b','b'
}

#[test]
fn while_body_declared_var_captured_per_iteration() {
    let vm = run_vm(
        "let fns = []; let i = 0; \
             while (i < 3) { let j = i; fns.push(() => j); i = i + 1; } \
             let a = fns[0], b = fns[1], c = fns[2]; \
             state.r = a() * 100 + b() * 10 + c();",
    );
    assert_eq!(state_val(&vm, "r"), num(12.0)); // 0,1,2
}

#[test]
fn for_head_var_value_carries_forward() {
    // The fresh per-iteration cell is seeded with the previous iteration's
    // value, so the update (`i++`) and the running total stay correct even
    // with re-boxing.
    let vm = run_vm("let sum = 0; for (let i = 0; i < 5; i++) { sum = sum + i; } state.r = sum;");
    assert_eq!(state_val(&vm, "r"), num(10.0)); // 0+1+2+3+4
}

#[test]
fn captured_var_in_loop_is_shared_not_per_iteration() {
    // `var` is function-scoped: a single binding shared across iterations, so
    // all closures observe the final value (3), unlike `let`. Must NOT be
    // re-boxed per iteration.
    let vm = run_vm(
        "let fns = []; \
             for (var i = 0; i < 3; i++) { fns.push(() => i); } \
             let a = fns[0], b = fns[1], c = fns[2]; \
             state.r = a() * 100 + b() * 10 + c();",
    );
    assert_eq!(state_val(&vm, "r"), num(333.0)); // shared `var i` == 3
}

#[test]
fn captured_loop_var_allocates_plain_not_boxed() {
    // The optimization: a captured loop variable is allocated `Plain` (no
    // eager cell in the preamble) and re-boxed per iteration via FreshCell.
    // Here the only captured binding is the loop var `i`, so the prologue
    // `EnterFrame` must contain no `Boxed` slot, yet FreshCell is emitted.
    let prog = compile("let fns = []; for (let i = 0; i < 3; i++) { fns.push(() => i); }")
        .expect("compiles");
    let has_boxed = prog.code.iter().any(
            |i| matches!(i, Instr::EnterFrame(_, _, kinds) if kinds.iter().any(|k| *k == SlotKind::Boxed)),
        );
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
    // A loop variable not captured by any closure stays a Plain slot, so no
    // FreshCell is emitted (per-iteration freshness is unobservable).
    let prog = compile("let s = 0; for (let i = 0; i < 3; i++) { s = s + i; }").expect("compiles");
    assert!(
        !prog.code.iter().any(|i| matches!(i, Instr::FreshCell(_))),
        "uncaptured loop var should not emit FreshCell: {:?}",
        prog.code
    );
}

#[test]
fn function_decl_in_block_scope() {
    let vm = run_vm("state.r = foo(); { function foo() { return 9; } }");
    assert_eq!(state_val(&vm, "r"), Value::PosInt(9));
}

#[test]
fn return_must_be_inside_function() {
    let errs = compile("return 1;").expect_err("top-level return should error");
    assert!(
        errs[0].message.contains("return"),
        "got: {}",
        errs[0].message
    );
}

#[test]
fn capture_through_intermediate_function() {
    // `inner` captures `x` from `outer`; `middle` doesn't reference `x` but
    // must still forward the capture down. (Transitive capture — the old
    // immediate-parent-only resolver got this wrong.)
    let vm = run_vm(
        "function outer() { \
               let x = 10; \
               function middle() { \
                 function inner() { return x; } \
                 return inner(); \
               } \
               return middle(); \
             } \
             state.r = outer();",
    );
    assert_eq!(state_val(&vm, "r"), Value::PosInt(10));
}

#[test]
fn sibling_block_shadowing_uses_distinct_bindings() {
    // The same name `x` in two blocks (and an outer `x`) must resolve to
    // three distinct slots; references resolve per-occurrence by span.
    let vm = run_vm(
        "let x = 1; \
             { let x = 2; state.a = x; } \
             { let x = 3; state.b = x; } \
             state.c = x;",
    );
    assert_eq!(state_val(&vm, "a"), Value::PosInt(2));
    assert_eq!(state_val(&vm, "b"), Value::PosInt(3));
    assert_eq!(state_val(&vm, "c"), Value::PosInt(1));
}

#[test]
fn write_only_capture_is_detected() {
    // `setter` only *writes* the captured `v` (never reads it). Capturing
    // must still happen so `setter` and `getter` share one cell. (The old
    // resolver ignored assignment-target identifiers and missed this.)
    let vm = run_vm(
        "function make() { \
               let v = 0; \
               function setter(n) { v = n; } \
               function getter() { return v; } \
               setter(42); \
               return getter(); \
             } \
             state.r = make();",
    );
    assert_eq!(state_val(&vm, "r"), Value::PosInt(42));
}

// ── allocation baseline (compiled, realistic workload) ──────────

/// Run a compiled program and return the finished VM + allocation count.
fn run_counted(prog: Program) -> (VM, usize) {
    alloc_counter::reset();
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step().unwrap() {
            StepResult::Done => break,
            other => panic!("unexpected effect: {other:?}"),
        }
    }
    let count = alloc_counter::count();
    (vm, count)
}

/// Breakdown: measure each component of the makeCounter workload in
/// isolation to understand where allocations go.
#[test]
fn alloc_breakdown_makecounter() {
    // 1. Empty program (just state initialization).
    let (_, empty) = run_counted(compile("1;").expect("compiles"));
    eprintln!("  empty program: {empty}");

    // 2. makeCounter creation (function decl + one call, no loop).
    let (_, once) = run_counted(
        compile(
            "function makeCounter() { \
                   let count = 0; \
                   function inc() { count = count + 1; return count; } \
                   return inc; \
                 } \
                 let c = makeCounter(); \
                 state.r = c();",
        )
        .expect("compiles"),
    );
    eprintln!("  makeCounter + 1 call: {once}");

    // 3. Per-iteration cost: closure call + string concat in isolation.
    let (_, one_iter) = run_counted(
        compile(
            "function makeCounter() { \
                   let count = 0; \
                   function inc() { count = count + 1; return count; } \
                   return inc; \
                 } \
                 let c = makeCounter(); \
                 let s = 'x'; \
                 c(); \
                 s = s + 'x'; \
                 state.r = c();",
        )
        .expect("compiles"),
    );
    eprintln!("  + 1 iter (2 calls total): {one_iter}");
    eprintln!("  -> per-iter marginal: {}", one_iter.saturating_sub(once));

    // 3b. Just creating the closure + calling inc once vs twice.
    let (_, make_only) = run_counted(
        compile(
            "function makeCounter() { \
                   let count = 0; \
                   function inc() { count = count + 1; return count; } \
                   return inc; \
                 } \
                 let c = makeCounter();",
        )
        .expect("compiles"),
    );
    eprintln!("  makeCounter only (no inc call): {make_only}");

    let (_, inc_1) = run_counted(
        compile(
            "function makeCounter() { \
                   let count = 0; \
                   function inc() { count = count + 1; return count; } \
                   return inc; \
                 } \
                 let c = makeCounter(); \
                 c();",
        )
        .expect("compiles"),
    );
    eprintln!("  makeCounter + 1 inc call: {inc_1}");

    let (_, inc_2) = run_counted(
        compile(
            "function makeCounter() { \
                   let count = 0; \
                   function inc() { count = count + 1; return count; } \
                   return inc; \
                 } \
                 let c = makeCounter(); \
                 c(); c();",
        )
        .expect("compiles"),
    );
    eprintln!("  makeCounter + 2 inc calls: {inc_2}");
    eprintln!(
        "  -> marginal per inc call: {}",
        inc_2.saturating_sub(inc_1)
    );

    // 3c. Bare function call (no captures, no closure).
    let (_, bare_0) = run_counted(compile("function f() { return 1; }").expect("compiles"));
    eprintln!("  bare fn decl (no call): {bare_0}");

    let (_, bare_1) = run_counted(compile("function f() { return 1; } f();").expect("compiles"));
    eprintln!("  bare fn decl + 1 call: {bare_1}");

    let (_, bare_2) =
        run_counted(compile("function f() { return 1; } f(); f();").expect("compiles"));
    eprintln!("  bare fn decl + 2 calls: {bare_2}");
    eprintln!(
        "  -> marginal per bare call: {}",
        bare_2.saturating_sub(bare_1)
    );

    // 4. 100 iterations (full benchmark).
    let (_, full) = run_counted(
        compile(
            "function makeCounter() { \
                   let count = 0; \
                   function inc() { count = count + 1; return count; } \
                   return inc; \
                 } \
                 let c = makeCounter(); \
                 let s = 'x'; \
                 for (let i = 0; i < 100; i++) { \
                   c(); \
                   s = s + 'x'; \
                 } \
                 state.r = c(); \
                 state.s = s;",
        )
        .expect("compiles"),
    );
    eprintln!("  100 iter full: {full}");

    // 5. String concat only (no closures), 100 iterations.
    let (_, concat_only) = run_counted(
        compile(
            "let s = 'x'; \
                 for (let i = 0; i < 100; i++) { \
                   s = s + 'x'; \
                 } \
                 state.s = s;",
        )
        .expect("compiles"),
    );
    eprintln!("  100x concat only: {concat_only}");

    // 6. Closure calls only (no string concat), 100 iterations.
    let (_, closure_only) = run_counted(
        compile(
            "function makeCounter() { \
                   let count = 0; \
                   function inc() { count = count + 1; return count; } \
                   return inc; \
                 } \
                 let c = makeCounter(); \
                 for (let i = 0; i < 100; i++) { \
                   c(); \
                 } \
                 state.r = c();",
        )
        .expect("compiles"),
    );
    eprintln!("  100x closure only: {closure_only}");
}

/// Baseline: makeCounter closure called 100× in a loop. Exercises closures,
/// upval mutation, arithmetic, and string concat in compiled code.
#[test]
fn alloc_baseline_makecounter() {
    let prog = compile(
        "function makeCounter() { \
               let count = 0; \
               function inc() { count = count + 1; return count; } \
               return inc; \
             } \
             let c = makeCounter(); \
             let s = 'x'; \
             for (let i = 0; i < 100; i++) { \
               c(); \
               s = s + 'x'; \
             } \
             state.r = c(); \
             state.s = s;",
    )
    .expect("compiles");

    let (vm, allocs) = run_counted(prog);
    eprintln!("BASELINE makecounter_100_iter: {allocs} allocs");

    // Verify correctness.
    assert_eq!(state_val(&vm, "r"), num(101.0));
    match state_val(&vm, "s") {
        Value::String(s) => assert_eq!(s.len(), 101),
        other => panic!("not a string: {other:?}"),
    }
}
