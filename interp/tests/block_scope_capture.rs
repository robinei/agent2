//! **A closure captures the block it was written in.**
//!
//! Two sibling blocks that declare the same name are two bindings, and
//! a closure made in each must see its own. This dialect gets that
//! wrong: both closures see the *first* block's value.
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
//! **Loops are not affected**, which is why this survived: the classic
//! `for (let i …) fns.push(() => i)` case is correct, and so is
//! `for (const x of …)` with or without an inner binding. It is sibling
//! *blocks* sharing a name.

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
#[ignore = "known defect: sibling blocks sharing a name share a binding for closures"]
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
