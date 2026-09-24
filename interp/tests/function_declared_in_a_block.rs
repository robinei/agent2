//! **A function declared in a block is assigned where it stands.**
//!
//! Annex B (B.3.3.1 / B.3.3.2) says a non-strict `function f` written
//! inside a block, `case` or `if` clause synthesizes a *var*-scoped `f`
//! in the enclosing function, and that binding receives the function
//! object when the declaration is **reached** — not at frame entry. The
//! compiler hoisted it into the prologue instead, alongside the ordinary
//! top-level declarations, so
//!
//! ```js
//! var order = [];
//! { function f() { return 'inner'; } }
//! order.push(typeof f);
//! order.push(f());
//! function f() { return 'outer'; }
//! ```
//!
//! stored `inner` then `outer` before the first statement ran, and the
//! block — which is what the spec says decides — never got a say.
//!
//! **What surfaced instead was nondeterminism.** Both declarations were
//! constant functions (Phase F), and `register_const_fns` walked a
//! `HashSet<usize>` to write them into the one name-keyed `const_names`
//! entry `f` can have. `RandomState` is seeded per process, so whichever
//! declaration it reached first claimed the name: on 2026-09-24 twelve
//! separate `cargo test` processes over the snippet above answered
//! `function,inner` five times and `function,outer` seven. Within one
//! process it never wavered. Two full test262 sweeps of the *same*
//! binary differed by 19 entries and 10 passes for the same reason.
//!
//! The fix is three rules that were missing, not a tie-break:
//!
//! * A name declared by more than one function in a scope is not a
//!   compile-time constant — it holds a different function at different
//!   points of the run, and `const_names` can only record one. Both
//!   declarations keep a real slot and a real store.
//! * Only declarations written directly in a function body are stored in
//!   its prologue. A block-level one is stored where it stands.
//! * A call by a bare name lowers to a static `Call` only when the name
//!   has exactly one declaration and that declaration was prologue-stored;
//!   otherwise the callee is read from the slot.
//!
//! **One shape is still wrong, and deliberately so.** A block-level
//! declaration that is the *only* one of its name is still a constant
//! function, so `if (false) { function g(){} } typeof g` answers
//! `"function"` where the spec says `"undefined"`: references resolve to
//! the `Fn` constant and never consult the slot the block would have
//! written. Taking block-level declarations out of Phase F fixes that and
//! 22 more test262 annex-B files — and breaks `try { return g();
//! function g(){…} }`, which is *correct* today only because the constant
//! carries it: the spec initializes the block-scoped `g` at block entry,
//! and this compiler has no block-entry instantiation to lean on. That
//! trade needs the two bindings annex B actually describes (a lexical one
//! per block, a var one per function), which is more than a nondeterminism
//! fix should reach for.
//!
//! **A single-process test cannot catch this class of bug**: the hash
//! seed is fixed for the life of the process, so one run is always
//! self-consistent and a stability check would have passed on either
//! answer. What a test can do is pin the answer the spec requires, which
//! is what the cases below do — a wrong-but-stable compiler fails them
//! just as loudly as a random one.

fn val(src: &str) -> serde_json::Value {
    let program = interp::compile(src).unwrap_or_else(|e| {
        panic!(
            "{src}\n{:?}",
            e.iter().map(|d| &d.message).collect::<Vec<_>>()
        )
    });
    let mut vm = interp::VM::for_program(program, serde_json::Value::Null).unwrap();
    match vm.step(u64::MAX).unwrap() {
        interp::StepResult::Done { value, .. } => {
            vm.stack_value_to_json(&value, 0).expect("value to JSON")
        }
        other => panic!("{src} did not finish: {other:?}"),
    }
}

/// The case that varied per process. The block runs after the prologue,
/// so at the call `f` is the block's function, not the hoisted one.
#[test]
fn a_block_declaration_outranks_the_hoisted_one_once_the_block_has_run() {
    let src = "var order = []; \
               { function f() { return 'inner'; } } \
               order.push(typeof f); \
               order.push(typeof f === 'function' ? f() : '<not-fn>'); \
               function f() { return 'outer'; } \
               return order.join(',');";
    assert_eq!(val(src), "function,inner");
}

/// …and before the block has run, the hoisted declaration is still the
/// answer. Reading `f` at both points is what distinguishes "assigned
/// where it stands" from "assigned first".
#[test]
fn before_the_block_runs_the_hoisted_declaration_is_still_the_answer() {
    let src = "var order = []; \
               order.push(f()); \
               { function f() { return 'inner'; } } \
               order.push(f()); \
               function f() { return 'outer'; } \
               return order.join(',');";
    assert_eq!(val(src), "outer,inner");
}

/// With no block in sight, duplicate declarations are settled by the
/// prologue, which stores them in source order — so the last one wins,
/// everywhere, including before either was written.
#[test]
fn the_last_of_several_declarations_is_the_one_the_name_holds() {
    assert_eq!(
        val("function f() { return 1; } function f() { return 2; } return f();"),
        2.0
    );
    assert_eq!(
        val("const a = f(); function f() { return 'first'; } \
             function f() { return 'second'; } return a;"),
        "second"
    );
}

/// A formal parameter of the same name cancels annex B outright:
/// B.3.3.1 gates the synthesized `var` *and* its update on
/// `parameterNames does not contain F`, so the parameter is untouched
/// on both sides of the block.
#[test]
fn a_parameter_of_the_same_name_is_not_overwritten() {
    let src = "var init, after; \
               (function (f) { \
                 init = f; \
                 switch (1) { case 1: function f() {} } \
                 after = f; \
               }(123)); \
               return init + ',' + after;";
    assert_eq!(val(src), "123,123");
}
