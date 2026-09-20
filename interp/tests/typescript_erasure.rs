//! **Typed source runs, and the types are gone.**
//!
//! The dialect is JavaScript and still is. What changed is that the
//! *source* stopped being: the card's whole API is a block of
//! TypeScript `declare`s, and so is a great deal of the code the model
//! will be asked to work on. An annotation that would have been deleted
//! by `tsc` before the program ran should not be the reason a program
//! fails to parse.
//!
//! So `compile` parses as `mjs().with_typescript(true)` — a *module*,
//! because top-level `await` is the dialect's primary pattern and
//! `SourceType::ts()` is a script where `await` is an ordinary
//! identifier — and the analyzer and compiler erase what erases.
//!
//! **Every test here asserts a value, not that it compiled.** The first
//! cut of this change compiled `(x as number)` happily and then threw
//! `x is not defined` at run time: the compiler erased the cast but the
//! *analyzer* never descended into it, so the identifier inside was
//! never bound and fell through to a global lookup. Erasure that only
//! half happens is worse than no erasure, and only running it says
//! which one you have.

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

#[test]
fn annotations_and_generics_do_not_change_the_answer() {
    for (src, want) in [
        ("let x: number = 2; return x + 1;", 3),
        (
            "const f = (a: number, b?: number): number => a + 1; return f(2);",
            3,
        ),
        ("function g<T>(v: T): T { return v } return g(2) + 1;", 3),
        (
            "interface P { a: number } const p: P = { a: 3 }; return p.a;",
            3,
        ),
        ("type T = number; const c: T = 3; return c;", 3),
        (
            "let n: number = 0; for (let i: number = 0; i < 3; i++) { n += i } return n;",
            3,
        ),
        (
            "class C { x: number = 3; get(): number { return this.x } } return new C().get();",
            3,
        ),
    ] {
        assert_eq!(val(src), serde_json::json!(want), "{src}");
    }
}

/// The wrappers that are claims about a type rather than computations.
/// Each one names a local, which is the case that broke: the analyzer
/// has to see through them or the name inside resolves to nothing.
#[test]
fn a_cast_still_names_the_binding_inside_it() {
    for src in [
        "let x = 2; return (x as number) + 1;",
        "let x = 2; return (x satisfies number) + 1;",
        "let x = 2; return (<number>x) + 1;",
        "let x = 3; return x!;",
        "let x = 1; const o = { a: { b: 2 } }; return o?.a!.b + x;",
    ] {
        assert_eq!(val(src), serde_json::json!(3), "{src}");
    }
}

/// An erased annotation leaves the same instructions behind. Not a
/// nicety: the optimizer folds `let x = 2; x + 1` to a constant, and a
/// cast that blocked the fold would be a silent performance cliff on
/// every typed line.
#[test]
fn erasure_leaves_the_same_instructions() {
    let typed = interp::compile("let x: number = 2; return (x as number) + 1;").unwrap();
    let plain = interp::compile("let x = 2; return x + 1;").unwrap();
    assert_eq!(format!("{:?}", typed.code), format!("{:?}", plain.code));
}

/// `enum` and `namespace` are values at run time, not types. Erasing
/// them would drop a binding the program then reads, so they stay
/// refused — loudly, which is the point.
#[test]
fn constructs_that_emit_code_are_still_refused() {
    for src in [
        "enum E { A, B } return E.A;",
        "namespace N { export const a = 1; } return 1;",
    ] {
        assert!(
            interp::compile(src).is_err(),
            "{src} compiled and should not have"
        );
    }
}

/// **`Edit` and `history` are compile-time builtin namespaces**
/// (`BuiltinKind::Namespace`), resolved by name in `call.rs` — not
/// objects a program declares or the host installs. The card describes
/// them with `declare namespace`, which is why that shape had to stop
/// being an error: a model echoing the block it reads most often must
/// not break, and the echo must not shadow the real thing.
#[test]
fn the_builtin_namespaces_survive_their_own_declaration() {
    assert_eq!(
        val(r#"return Edit.count("aaa", "a");"#),
        serde_json::json!(3)
    );
    assert_eq!(
        val(
            r#"declare namespace Edit { function count(t: string, n: string): number }
               return Edit.count("aaa", "a");"#
        ),
        serde_json::json!(3),
        "the ambient declaration erases; the builtin is still there"
    );
    assert_eq!(
        val(r#"const s: string = "a b"; return Edit.replaceOnce(s, "b", "c") as string;"#),
        serde_json::json!("a c")
    );
    // `history` reaches the host rather than returning a value here, so
    // this asserts only that the same echo still compiles.
    assert!(
        interp::compile(
            r#"declare namespace history { function append(v: unknown): void }
               history.append({ a: 1 });"#
        )
        .is_ok()
    );
}
