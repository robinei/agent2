//! **A closure captures the block it was written in.**
//!
//! Two sibling blocks that declare the same name are two bindings, and
//! a closure made in each must see its own. This dialect used to get
//! that wrong: both closures saw the *first* block's value.
//!
//! Found by reading a live run, not by a test. On 2026-09-24 a model
//! working out what a Rust test did wrote one program that peeked two
//! windows of the same file —
//!
//! ```js
//! if (testAt.length)    { const [s, e] = win(testAt[0], 4, 60);     history.peek(6, (r) => …slice(s - 1, e)…); }
//! if (compactAt.length) { const [s, e] = win(compactAt[0], 6, 130); history.peek(6, (r) => …slice(s - 1, e)…); }
//! ```
//!
//! — and both rows came back carrying the *first* window. Nothing
//! failed; the second answer was silently the first. A harness bug was
//! suspected first (the dispatcher falls back to the last shown value
//! when a projection yields nothing), and literal values and lambdas
//! over other fields both behaved, which is what narrowed it to here.
//!
//! **A single loop was not affected**, which is why this survived: the
//! classic `for (let i …) fns.push(() => i)` case was already correct,
//! and so was `for (const x of …)` with or without an inner binding —
//! one loop declares its name once.
//!
//! The cause was not the loop machinery at all. Capture resolution runs
//! after the walk (it must: a closure may name a binding declared later
//! in its own block) and looked a free name up in the enclosing
//! function's name table, which keeps **one entry per name per
//! function**. Two declarations of `s`, two slots, one table entry — so
//! every closure in either block captured the first. Sibling blocks were
//! just the shape that made it visible; a block shadowing a parameter and
//! two `for (let i …)` loops in a row failed the same way (see
//! `any_two_declarations_of_one_name_are_two_bindings`).

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

/// The shapes that already work, so a fix for the one below is not
/// free to break them.
#[test]
fn a_loop_body_is_a_fresh_binding_each_time() {
    for (src, want) in [
        (
            "const f = []; for (const x of [1, 2, 3]) f.push(() => x); \
             return f.map((g) => g()).join(',');",
            "1,2,3",
        ),
        (
            "const f = []; for (let i = 0; i < 3; i++) f.push(() => i); \
             return f.map((g) => g()).join(',');",
            "0,1,2",
        ),
        (
            "const f = []; for (const x of [1, 2]) { const y = x * 10; f.push(() => y); } \
             return f.map((g) => g()).join(',');",
            "10,20",
        ),
    ] {
        assert_eq!(val(src), serde_json::json!(want), "{src}");
    }
}

/// **The defect.** Two sibling blocks, one name, two closures — both
/// read the first block's binding.
#[test]
fn sibling_blocks_declaring_one_name_are_two_bindings() {
    for keyword in ["const", "let"] {
        let src = format!(
            "const f = []; {{ {keyword} s = 1; f.push(() => s); }} \
             {{ {keyword} s = 4; f.push(() => s); }} return f.map((g) => g()).join(',');"
        );
        assert_eq!(
            val(&src),
            serde_json::json!("1,4"),
            "{keyword}: each closure must see its own block's binding"
        );
    }
}

/// The same defect, in the other shapes that put two declarations of one
/// name in one function scope. Each of these returned the *first*
/// declaration's value before capture resolution became lexical; none of
/// them is a sibling `{ … }` block, which is why the one above did not
/// cover them.
#[test]
fn any_two_declarations_of_one_name_are_two_bindings() {
    for (src, want) in [
        // Two loops in a row. The head of a `for` had no block of its own,
        // so both `i`s landed in the enclosing one: this gave `0,1,2,2` —
        // and the `2,2` is the first loop's cell read after it finished,
        // which is how a shared binding fails loudly rather than quietly.
        (
            "const f = []; for (let i = 0; i < 2; i++) f.push(() => i); \
             for (let i = 10; i < 12; i++) f.push(() => i); \
             return f.map((g) => g()).join(',');",
            "0,1,10,11",
        ),
        (
            "const f = []; for (const x of [1, 2]) f.push(() => x); \
             for (const x of [8, 9]) f.push(() => x); \
             return f.map((g) => g()).join(',');",
            "1,2,8,9",
        ),
        // A block shadowing a parameter.
        (
            "const mk = (a) => { { let a = 5; return () => a; } }; return `${mk(1)()}`;",
            "5",
        ),
        // Two catch clauses binding the same name.
        (
            "const f = []; try { throw 1; } catch (e) { f.push(() => e); } \
             try { throw 2; } catch (e) { f.push(() => e); } \
             return f.map((g) => g()).join(',');",
            "1,2",
        ),
        // `if` branches are blocks too.
        (
            "const f = []; if (true) { let s = 1; f.push(() => s); } \
             if (true) { let s = 4; f.push(() => s); } \
             return f.map((g) => g()).join(',');",
            "1,4",
        ),
        // Two levels down: the inner arrow's name is free in the outer one,
        // so it is resolved a second time, at the *outer* arrow's block.
        (
            "const f = []; { let s = 1; f.push(() => (() => s)()); } \
             { let s = 4; f.push(() => (() => s)()); } \
             return f.map((g) => g()).join(',');",
            "1,4",
        ),
    ] {
        assert_eq!(val(src), serde_json::json!(want), "{src}");
    }
}

/// A closure may name a binding declared *later* in a block it sits in —
/// which is why capture resolution runs after the walk rather than off a
/// snapshot of the scopes open where the closure was written.
#[test]
fn a_closure_still_sees_a_binding_declared_after_it() {
    assert_eq!(
        val("const a = () => b(); const b = () => 7; return a();"),
        serde_json::json!(7)
    );
    // And it is the *visible* one it sees, not the first of the name.
    assert_eq!(
        val("{ let s = 1; } { const g = () => s; let s = 2; return g(); }"),
        serde_json::json!(2)
    );
}

/// A `let` in a `for` head does not outlive the loop. It used to: the head
/// declared into the enclosing block, so `i` was still readable after the
/// loop — and readable as a *number*, so nothing complained.
#[test]
fn a_for_head_binding_does_not_outlive_the_loop() {
    assert_eq!(
        val("for (let i = 0; i < 3; i++) {} return typeof i;"),
        serde_json::json!("undefined")
    );
}
