//! Allocation-count benchmarks — exact counts via `CountingAlloc`.
//! These tests are precise by design and must not be "simplified".

use super::*;
use crate::alloc_counter;
use crate::compiler::compile;
use crate::compiler::Program;
use crate::vm::{StepResult, VM, Value};

/// Run a compiled program and return the finished VM + allocation count.
fn run_counted(prog: Program) -> (VM, usize) {
    alloc_counter::reset();
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    loop {
        match vm.step().unwrap() {
            StepResult::Done { .. } => break,
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
    // 1. Empty program.
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
                 input.r = c();",
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
                 input.r = c();",
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
                 input.r = c(); \
                 input.s = s;",
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
                 input.s = s;",
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
                 input.r = c();",
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
             input.r = c(); \
             input.s = s;",
    )
    .expect("compiles");

    let (vm, allocs) = run_counted(prog);
    eprintln!("BASELINE makecounter_100_iter: {allocs} allocs");

    // Verify correctness.
    assert_eq!(input_val(&vm, "r"), crate::testutil::num(101.0));
    match input_val(&vm, "s") {
        Value::String(s) => assert_eq!(s.len(), 101),
        other => panic!("not a string: {other:?}"),
    }
}
