//! Gate tests for incremental evaluation (25.2).
//!
//! The fragments here stand in for whatever a caller slices out of its own
//! source — the notebook transport's cells, a shell's input lines. Nothing in
//! this file knows about markdown, which is the point (D16).

use super::Repl;
use crate::vm::{Instr, SlotKind, StepResult, Value};

/// Lay fragments out in one buffer the way a caller must: same length
/// throughout, same newlines, only one fragment's text live at a time.
///
/// This is D2's shared parse buffer reduced to what a test needs. The blanking
/// is what keeps spans absolute, so the analysis tables accumulate instead of
/// colliding at zero.
struct Unit {
    /// Byte ranges of each fragment within the buffer.
    spans: Vec<(usize, usize)>,
    text: String,
}

impl Unit {
    /// Build a unit from fragments, separating them with a blank line so each
    /// starts on its own line.
    fn new(fragments: &[&str]) -> Self {
        let mut text = String::new();
        let mut spans = Vec::new();
        for frag in fragments {
            let start = text.len();
            text.push_str(frag);
            if !frag.ends_with('\n') {
                text.push('\n');
            }
            spans.push((start, text.len()));
            text.push('\n');
        }
        Self { spans, text }
    }

    /// The buffer with fragment `i` live and everything else blanked —
    /// newlines kept, so line and column are unchanged.
    fn buffer(&self, i: usize) -> String {
        let (start, end) = self.spans[i];
        self.blanked_except(start, end)
    }

    /// A buffer with nothing live, for closing a unit that has no final
    /// fragment of its own.
    fn empty(&self) -> String {
        self.blanked_except(0, 0)
    }

    fn blanked_except(&self, start: usize, end: usize) -> String {
        self.text
            .bytes()
            .enumerate()
            .map(|(i, b)| {
                if (start..end).contains(&i) || b == b'\n' {
                    b as char
                } else {
                    ' '
                }
            })
            .collect()
    }

    fn len(&self) -> usize {
        self.spans.len()
    }
}

/// Push every fragment, stepping each to its `Pause`, then close and run to
/// `Done`. Panics with rendered diagnostics on a compile failure.
fn run_all(fragments: &[&str]) -> Repl {
    let unit = Unit::new(fragments);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    for i in 0..unit.len() {
        let buf = unit.buffer(i);
        if let Err(errs) = repl.push(&buf) {
            let rendered: Vec<String> = errs.iter().map(|d| d.render(&buf)).collect();
            panic!("fragment {i} failed to compile:\n{}", rendered.join("\n"));
        }
        match repl.vm.step(u64::MAX).unwrap() {
            StepResult::Paused { .. } => {}
            other => panic!("fragment {i} did not pause: {other:?}"),
        }
    }
    repl.close(&unit.empty()).expect("close compiles");
    match repl.vm.step(u64::MAX).unwrap() {
        StepResult::Done { .. } => {}
        other => panic!("the unit did not finish: {other:?}"),
    }
    repl
}

/// Compile fragments until one fails, returning its rendered diagnostics.
fn errs_from(fragments: &[&str]) -> Vec<String> {
    let unit = Unit::new(fragments);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    for i in 0..unit.len() {
        let buf = unit.buffer(i);
        match repl.push(&buf) {
            Ok(()) => {
                repl.vm.step(u64::MAX).unwrap();
            }
            Err(errs) => return errs.iter().map(|d| d.render(&buf)).collect(),
        }
    }
    panic!("expected a compile failure, every fragment compiled");
}

fn console(repl: &Repl) -> Vec<String> {
    repl.vm.console_lines.clone()
}

// ── the shared scope ──────────────────────────────────────────────────

/// The gate's first case, and the reason the frame is never unwound between
/// fragments: a binding from one is live in the next.
#[test]
fn a_const_in_fragment_0_is_readable_in_fragment_1() {
    let repl = run_all(&["const greeting = \"hi\";", "console.log(greeting);"]);
    assert_eq!(console(&repl), vec!["hi"]);
}

#[test]
fn a_let_in_fragment_0_is_readable_and_writable_in_fragment_1() {
    let repl = run_all(&["let n = 1;", "n = n + 41;", "console.log(n);"]);
    assert_eq!(console(&repl), vec!["42"]);
}

/// **The case root-frame pinning exists for.** `let x = 5` alone is
/// effectively const, so a whole-program compile folds the 5 into the
/// instruction stream and drops the store — leaving the slot holding
/// `Undefined`. A later fragment that reads it would then see nothing.
#[test]
fn a_folded_binding_still_has_its_value_in_a_later_fragment() {
    let repl = run_all(&["let x = 5;", "x = x + 1;", "console.log(x);"]);
    assert_eq!(console(&repl), vec!["6"]);
}

/// And the same thing stated about the emitted code: at the root, the store
/// survives however dead it looks from inside the fragment that wrote it.
#[test]
fn the_root_store_is_never_elided() {
    let unit = Unit::new(&["let x = 5;"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    assert!(
        repl.vm.code.iter().any(|i| matches!(i, Instr::SetLocal(0))),
        "the pinned root must store its declaration, not only fold its reads: {:?}",
        repl.vm.code
    );
}

// ── capture across fragments, and the promotion ───────────────────────

/// **The case that forced promotion.** A closure in fragment 1 captures a
/// variable fragment 0 declared and wrote. The slot was `Plain` while nothing
/// captured it, so the fragment's prologue promotes it with `FreshCell` — and
/// because the root is pinned, the slot really holds `5` for `FreshCell` to
/// carry into the new cell.
#[test]
fn a_closure_capturing_an_earlier_binding_sees_the_current_value() {
    let repl = run_all(&[
        "let x = 5;\nconsole.log(x);",
        "const f = () => x + 1;\nconsole.log(f());",
    ]);
    assert_eq!(console(&repl), vec!["5", "6"]);
}

/// The promotion is a *diff*, so it emits a `FreshCell` for exactly the slot
/// that flipped and nothing else.
#[test]
fn promotion_emits_one_fresh_cell_for_the_slot_that_flipped() {
    let unit = Unit::new(&["let x = 5;\nlet y = 6;", "const f = () => x;"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    let before = repl.vm.code.len();
    repl.push(&unit.buffer(1)).unwrap();
    let fresh: Vec<&Instr> = repl.vm.code[before..]
        .iter()
        .filter(|i| matches!(i, Instr::FreshCell(_)))
        .collect();
    assert_eq!(
        fresh,
        vec![&Instr::FreshCell(0)],
        "only `x` was captured, so only `x` is promoted"
    );
}

/// A promoted binding keeps its identity: writing through the closure is
/// visible to the outer name, which is what boxing is for.
#[test]
fn a_promoted_binding_is_shared_not_copied() {
    let repl = run_all(&[
        "let count = 0;",
        "const bump = () => { count = count + 1; };",
        "bump();\nbump();\nconsole.log(count);",
    ]);
    assert_eq!(console(&repl), vec!["2"]);
}

/// A slot a closure captures *within its own fragment* is allocated `Boxed`
/// by the prologue directly, so it needs no promotion at all — which is why
/// `ExtendFrame` and `FreshCell` never touch the same slot.
#[test]
fn a_slot_captured_in_its_own_fragment_is_boxed_without_a_fresh_cell() {
    let unit = Unit::new(&["let z = 1;\nconst g = () => z;"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    assert!(
        !repl
            .vm
            .code
            .iter()
            .any(|i| matches!(i, Instr::FreshCell(_))),
        "nothing pre-existing flipped, so nothing is promoted"
    );
    let Some(Instr::EnterFrame(_, _, kinds)) = repl.vm.code.first() else {
        panic!(
            "expected an EnterFrame prologue, got {:?}",
            repl.vm.code.first()
        );
    };
    assert_eq!(
        kinds[0],
        SlotKind::Boxed,
        "`z` is captured here, so it is boxed here"
    );
}

// ── functions across fragments ────────────────────────────────────────

/// A function declared in one fragment and called from a later one. The call
/// is a static `Call` against a label whose `Label` marker was stripped when
/// fragment 0 was backpatched, so it resolves only because the label table
/// outlives the fragment.
#[test]
fn a_function_declared_in_fragment_0_is_called_in_fragment_1() {
    let repl = run_all(&[
        "function double(n) { return n * 2; }",
        "console.log(double(21));",
    ]);
    assert_eq!(console(&repl), vec!["42"]);
}

/// And it resolves top-level names, which is the part that needs the root
/// scope to have kept growing rather than been rebuilt.
#[test]
fn a_function_from_an_earlier_fragment_resolves_a_top_level_name() {
    let repl = run_all(&[
        "let factor = 3;\nfunction scale(n) { return n * factor; }",
        "factor = 10;\nconsole.log(scale(4));",
    ]);
    assert_eq!(console(&repl), vec!["40"]);
}

/// A prelude helper pulled in by a later fragment is appended past everything
/// already written, and one pulled in twice is compiled once.
#[test]
fn a_prelude_helper_arrives_with_the_fragment_that_needs_it() {
    let repl = run_all(&[
        "let xs = [1, 2, 3];",
        "console.log(xs.map(n => n * 2).join(\",\"));",
        "console.log(xs.map(n => n + 1).join(\",\"));",
    ]);
    assert_eq!(console(&repl), vec!["2,4,6", "2,3,4"]);
}

// ── diagnostics ───────────────────────────────────────────────────────

/// An undeclared name is caught, and names the name.
///
/// **Not at compile time, here or on the one-shot path** — 25.2's gate asks
/// for a compile error, but this dialect cannot give one and does not today:
/// a reference absent from `ref_resolution` is exactly the *global* fallback,
/// which is how `console`, `Math`, `tools` and every harness verb resolve. The
/// compiler has no closed list to check a bare name against, so
/// `compile("console.log(mystery);")` succeeds one-shot too and the name is
/// reported at runtime. Incremental evaluation changes nothing about that; the
/// fragment still fails, still names the binding, and still leaves the
/// fragments before it standing.
#[test]
fn an_undeclared_name_is_reported_and_names_it() {
    let unit = Unit::new(&["let a = 1;", "console.log(mystery);"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    repl.push(&unit.buffer(1))
        .expect("it compiles, as it does one-shot");
    let err = repl.vm.step(u64::MAX).expect_err("but it does not run");
    assert_eq!(err.kind, crate::vm::ErrorKind::ReferenceError);
    assert!(
        err.message.contains("mystery"),
        "the error must name the undeclared binding: {err:?}"
    );
}

/// A redeclaration across fragments. `oxc` catches `let x; let x;` inside one
/// parse, but each fragment is parsed on its own, so the collision is only
/// visible to the accumulated scope — which is where this check lives.
#[test]
fn a_redeclaration_across_fragments_is_caught() {
    let errs = errs_from(&["let dup = 1;", "let dup = 2;"]);
    assert!(
        errs.iter()
            .any(|e| e.contains("dup") && e.contains("already declared")),
        "expected a redeclaration diagnostic: {errs:?}"
    );
}

/// But shadowing inside a nested block is ordinary JS and stays legal.
#[test]
fn a_nested_block_may_still_shadow_a_top_level_name() {
    let repl = run_all(&[
        "let v = 1;",
        "{ let v = 2; console.log(v); }\nconsole.log(v);",
    ]);
    assert_eq!(console(&repl), vec!["2", "1"]);
}

/// A fragment that does not compile leaves every fragment before it standing —
/// nothing is appended, so the VM never sees it.
#[test]
fn a_failed_fragment_leaves_the_earlier_ones_intact() {
    let unit = Unit::new(&["console.log(\"first\");", "this is not javascript"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    let code_len = repl.vm.code.len();
    assert!(repl.push(&unit.buffer(1)).is_err());
    assert_eq!(
        repl.vm.code.len(),
        code_len,
        "a failed fragment must append nothing"
    );
    assert_eq!(console(&repl), vec!["first"]);
}

// ── spans stay absolute ───────────────────────────────────────────────

/// The reason for D2's buffer: a call compiled in the *third* fragment carries
/// a span that slices the unit's own source back to that call's text. If cells
/// were compiled from their own substrings both would start at zero and the
/// span-keyed tables would collide.
#[test]
fn a_call_in_fragment_2_has_a_span_slicing_its_own_text() {
    let unit = Unit::new(&[
        "let a = 1;",
        "let b = 2;",
        "console.log(\"third fragment\");",
    ]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    repl.push(&unit.buffer(1)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    let before = repl.vm.code.len();
    repl.push(&unit.buffer(2)).unwrap();

    let idx = (before..repl.vm.code.len())
        .find(|&i| matches!(repl.vm.code[i], Instr::CallBuiltin(..)))
        .expect("the third fragment makes a call");
    let span = repl.vm.spans[idx];
    let sliced = &repl.source()[span.start as usize..span.end as usize];
    assert_eq!(sliced, "console.log(\"third fragment\")");
    // And it really is an offset into the unit, not a fragment-local one.
    assert!(
        span.start as usize >= unit.spans[2].0,
        "the span must be absolute, not rebased to the fragment"
    );
}

/// The unit's source is the real text, not the blanked parse buffer, so a
/// span compiled in an earlier fragment still reads correctly later.
#[test]
fn the_source_keeps_every_fragments_text() {
    let unit = Unit::new(&["let first = 1;", "let second = 2;"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    repl.push(&unit.buffer(1)).unwrap();
    assert!(repl.source().contains("let first = 1;"));
    assert!(repl.source().contains("let second = 2;"));
    assert_eq!(
        repl.source().len(),
        unit.text.len(),
        "the source keeps the unit's coordinates"
    );
}

// ── the prologue and the epilogue ─────────────────────────────────────

/// A fragment ends with `Pause`, not `Return(0)`: the frame stays standing, so
/// the next fragment finds the bindings it declared.
#[test]
fn a_fragment_ends_with_pause_and_the_unit_ends_with_return() {
    let unit = Unit::new(&["let kept = 7;"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    assert_eq!(repl.vm.code.last(), Some(&Instr::Pause));
    assert!(matches!(
        repl.vm.step(u64::MAX).unwrap(),
        StepResult::Paused { .. }
    ));
    // The frame is still standing, with the binding in it.
    assert_eq!(repl.vm.stack.len(), 1);
    assert_eq!(repl.vm.callstack.len(), 1);

    repl.close(&unit.empty()).unwrap();
    assert_eq!(repl.vm.code.last(), Some(&Instr::Return(0)));
    assert!(matches!(
        repl.vm.step(u64::MAX).unwrap(),
        StepResult::Done { .. }
    ));
}

/// `ip` is left at the append position, so appending the next fragment puts
/// its first instruction exactly where the VM is standing. Nothing repositions
/// anything.
#[test]
fn ip_lands_on_the_append_position() {
    let unit = Unit::new(&["let a = 1;", "let b = 2;"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    let append_at = repl.vm.code.len();
    assert_eq!(repl.vm.ip as usize, append_at);
    repl.push(&unit.buffer(1)).unwrap();
    assert_eq!(
        repl.vm.ip as usize, append_at,
        "the code vector grew in front of a VM already standing there"
    );
}

/// The first `ExtendFrame` edge case: a first fragment that declares nothing
/// emits no `EnterFrame` at all, so a later `ExtendFrame` extends from zero.
#[test]
fn a_first_fragment_with_no_locals_emits_no_enter_frame() {
    let unit = Unit::new(&["console.log(\"nothing declared\");", "let late = 1;"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    assert!(
        !repl
            .vm
            .code
            .iter()
            .any(|i| matches!(i, Instr::EnterFrame(..))),
        "a root frame with no locals has no prologue: {:?}",
        repl.vm.code
    );
    repl.vm.step(u64::MAX).unwrap();

    let before = repl.vm.code.len();
    repl.push(&unit.buffer(1)).unwrap();
    assert!(
        matches!(repl.vm.code[before], Instr::ExtendFrame(_)),
        "the later fragment extends from zero: {:?}",
        &repl.vm.code[before..]
    );
    assert!(matches!(
        repl.vm.step(u64::MAX).unwrap(),
        StepResult::Paused { .. }
    ));
    assert_eq!(repl.vm.stack.len(), 1, "the frame grew by one slot");
}

/// The second edge case: a fragment declaring no new locals elides
/// `ExtendFrame` rather than emitting an empty one.
#[test]
fn a_fragment_with_no_new_locals_emits_no_extend_frame() {
    let unit = Unit::new(&["let only = 1;", "console.log(only);"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    let before = repl.vm.code.len();
    repl.push(&unit.buffer(1)).unwrap();
    assert!(
        !repl.vm.code[before..]
            .iter()
            .any(|i| matches!(i, Instr::ExtendFrame(_))),
        "nothing new was declared, so there is nothing to extend by: {:?}",
        &repl.vm.code[before..]
    );
}

/// Slot indices are allocated in declaration order across the whole unit, and
/// each fragment's new ones sit above the ones already taken.
#[test]
fn slots_are_allocated_in_declaration_order_across_fragments() {
    let unit = Unit::new(&["let a = 1;\nlet b = 2;", "let c = 3;"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    let Some(Instr::EnterFrame(_, _, kinds)) = repl.vm.code.first() else {
        panic!("expected an EnterFrame");
    };
    assert_eq!(kinds.len(), 2, "two declarations, two slots");
    repl.vm.step(u64::MAX).unwrap();

    let before = repl.vm.code.len();
    repl.push(&unit.buffer(1)).unwrap();
    let Instr::ExtendFrame(kinds) = &repl.vm.code[before] else {
        panic!("expected an ExtendFrame, got {:?}", repl.vm.code[before]);
    };
    assert_eq!(kinds.len(), 1, "the fragment extends by its own one slot");
    repl.vm.step(u64::MAX).unwrap();
    assert_eq!(repl.vm.stack.len(), 3);
}

/// **The invariant that makes extending a push.** A fragment is a run of
/// complete statements, so between two of them the operand stack is balanced
/// and `sp` is exactly the top of the locals — which is what `ExtendFrame`
/// `debug_assert`s before it pushes. Fragments here end after deeply nested
/// expressions, whose temporaries must all be gone by the boundary; a
/// violation trips the assertion, since tests build in debug.
#[test]
fn the_frame_is_quiescent_at_every_prologue() {
    let repl = run_all(&[
        "let a = ((1 + 2) * (3 + 4)) - (5 * (6 - 7));",
        "let b = [a, a * 2, a * 3].map(n => n + 1).reduce((s, n) => s + n, 0);",
        "let c = { x: { y: [a, b] } }.x.y[1];",
        "console.log(c === b);",
    ]);
    assert_eq!(console(&repl), vec!["true"]);
    // Four fragments, four declarations, one slot each, nothing left over.
    assert_eq!(repl.vm.stack.len(), 0, "the run unwound at close");
}

/// A root function keeps a real slot and a real store under pinning, so
/// nothing a later fragment says about it can move it. (Whole-program, this
/// same source allocates *no* slot for `f` and renumbers `x` down to 0.)
#[test]
fn a_root_function_is_never_slot_eliminated() {
    let unit = Unit::new(&[
        "function f() { return 1; }\nlet x = 5;",
        "f = 2;\nconsole.log(x);",
    ]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    let Some(Instr::EnterFrame(_, _, kinds)) = repl.vm.code.first() else {
        panic!("expected an EnterFrame");
    };
    assert_eq!(
        kinds.len(),
        2,
        "`f` keeps its slot, so `x` keeps index 1 whatever comes later"
    );
    repl.vm.step(u64::MAX).unwrap();
    repl.push(&unit.buffer(1)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    assert_eq!(
        console(&repl),
        vec!["5"],
        "`x` is still where fragment 0 put it"
    );
}

// ── the frame stays usable ────────────────────────────────────────────

/// Control flow inside a fragment backpatches against the appended range, so
/// its jumps land where they should even though the code vector already holds
/// another fragment's resolved addresses.
#[test]
fn control_flow_in_a_later_fragment_jumps_correctly() {
    let repl = run_all(&[
        "let total = 0;",
        "for (let i = 0; i < 5; i++) { if (i % 2 === 0) { total = total + i; } }",
        "console.log(total);",
    ]);
    assert_eq!(console(&repl), vec!["6"]);
}

/// `try`/`catch` too, which emits a `TryEnter` carrying a label.
#[test]
fn a_try_in_a_later_fragment_catches() {
    let repl = run_all(&[
        "let caught = \"no\";",
        "try { throw new Error(\"boom\"); } catch (e) { caught = e.message; }",
        "console.log(caught);",
    ]);
    assert_eq!(console(&repl), vec!["boom"]);
}

/// Objects and arrays built in one fragment keep their identity in the next —
/// the heap is the VM's, and the VM never went away.
#[test]
fn heap_values_survive_the_boundary() {
    let repl = run_all(&[
        "const acc = [];",
        "acc.push(\"one\");",
        "acc.push(\"two\");",
        "console.log(acc.join(\"+\"));",
    ]);
    assert_eq!(console(&repl), vec!["one+two"]);
}

/// A unit closed with a final fragment rather than an empty buffer: the
/// epilogue is the ordinary root `Return(0)` either way.
#[test]
fn a_unit_can_close_on_its_last_fragment() {
    let unit = Unit::new(&["let v = 9;", "console.log(v);"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    repl.close(&unit.buffer(1)).unwrap();
    assert!(matches!(
        repl.vm.step(u64::MAX).unwrap(),
        StepResult::Done { .. }
    ));
    assert_eq!(console(&repl), vec!["9"]);
}

/// An empty unit is a run that does nothing and finishes.
#[test]
fn an_empty_unit_closes_cleanly() {
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.close("").unwrap();
    match repl.vm.step(u64::MAX).unwrap() {
        StepResult::Done { value, .. } => assert_eq!(value, Value::Undefined),
        other => panic!("expected Done, got {other:?}"),
    }
}

// ── the top-level `return` flag (25.3) ────────────────────────────────

/// Off by default: `interp` does not decide that a top-level `return` is
/// wrong, it only offers to enforce that a caller thinks so.
#[test]
fn a_top_level_return_is_allowed_unless_the_caller_rejects_it() {
    let unit = Unit::new(&["return 1;"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    assert!(repl.push(&unit.buffer(0)).is_ok());
}

/// And when the caller rejects it, the caller's own sentence is what comes
/// back — `interp` supplies no wording of its own.
#[test]
fn a_rejected_top_level_return_carries_the_callers_message() {
    let unit = Unit::new(&["let a = 1;", "return { a };"]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.reject_top_level_return("use `history.note` and `finish(text)` instead");
    repl.push(&unit.buffer(0)).unwrap();
    repl.vm.step(u64::MAX).unwrap();
    let errs = repl
        .push(&unit.buffer(1))
        .expect_err("the fragment is refused");
    let rendered: Vec<String> = errs.iter().map(|d| d.render(&unit.buffer(1))).collect();
    assert!(
        rendered
            .iter()
            .any(|e| e.contains("use `history.note` and `finish(text)` instead")),
        "{rendered:?}"
    );
}

/// The gate's second half: a `return` *inside a function* in a fragment is an
/// ordinary function return and is left alone.
#[test]
fn a_return_inside_a_function_in_a_fragment_is_left_alone() {
    let unit = Unit::new(&[
        "function pick(xs) { for (const x of xs) { if (x > 2) { return x; } } return -1; }",
        "const arrow = (n) => { if (n) { return \"yes\"; } return \"no\"; };",
        "console.log(pick([1, 2, 3]), arrow(1));",
    ]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.reject_top_level_return("use `history.note` and `finish(text)` instead");
    for i in 0..unit.len() {
        repl.push(&unit.buffer(i)).expect("a function may return");
        repl.vm.step(u64::MAX).unwrap();
    }
    assert_eq!(console(&repl), vec!["3 yes"]);
}

/// Top-level `this` capture is refused rather than silently misplacing a slot
/// on the fragment after it (see the diagnostic's own note).
#[test]
fn capturing_this_at_the_top_level_is_refused() {
    let errs = errs_from(&["const f = () => this;"]);
    assert!(
        errs.iter().any(|e| e.contains("`this` cannot be captured")),
        "{errs:?}"
    );
}

/// **The two cases that made phase 25's first design unsound**, end to
/// end rather than by inspecting instructions. Before root-frame
/// pinning, `immutable = is_const || (!reassigned && !captured)` let
/// constant propagation fold `5` into the stream and drop the store, so
/// a later fragment's `FreshCell` boxed `Undefined` and the closure
/// returned `NaN`. And `compact_const_fn_slots` renumbered survivors
/// when a later fragment demoted a const function, moving a binding an
/// earlier fragment had already written.
#[test]
fn the_unsound_cases_that_forced_root_pinning() {
    let repl = run_all(&[
        "let x = 5; console.log(x);",
        "const f = () => x + 1; console.log(f());",
    ]);
    assert_eq!(console(&repl), vec!["5", "6"], "NaN here is the old bug");

    // And the slot-renumbering half.
    let repl = run_all(&[
        "function f(){ return 1; }\nlet x = 5;",
        "f = 2;\nconsole.log(x);\nconsole.log(f);",
    ]);
    assert_eq!(console(&repl), vec!["5", "2"], "x must not have shifted");
}

// ── a function's own frame (regression) ───────────────────────────────

/// **A function declaration binds its name in the *enclosing* scope, and
/// nowhere else.** Its own frame declares no local for itself.
///
/// The incremental path once hoisted every root declaration into each nested
/// scope as well, so every function's frame carried a redundant slot and a
/// `ClosureNew` of itself — doubling the code and allocating a closure on
/// every call. The values still came out right, which is exactly why this
/// pins the *shape* rather than the behaviour.
#[test]
fn a_functions_own_frame_declares_no_local_for_itself() {
    let unit = Unit::new(&[
        "function outer() { return 1; }\nfunction two() { return 2; }\nlet r = outer() + two();",
    ]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();

    // Walk each function body — everything after the root's prologue —
    // and check no frame re-binds a name the root already owns.
    let entries: Vec<usize> = repl
        .vm
        .code
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, i)| matches!(i, Instr::EnterFrame(..)))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(entries.len(), 2, "two functions, two frames");
    for at in entries {
        let Instr::EnterFrame(_, _, kinds) = &repl.vm.code[at] else {
            unreachable!()
        };
        assert!(
            kinds.is_empty(),
            "a function whose body declares nothing needs no locals, got {kinds:?} at {at}"
        );
        assert!(
            !matches!(repl.vm.code[at + 1], Instr::ClosureNew(..)),
            "a function must not build a closure of itself in its own frame: {:?}",
            &repl.vm.code[at..at + 3]
        );
    }
}

/// The other half of the invariant: a function that *does* name itself keeps
/// its self-reference slot, and self-recursion works across fragments.
#[test]
fn a_self_recursive_function_keeps_its_self_slot() {
    let repl = run_all(&[
        "function fact(n) { return n <= 1 ? 1 : n * fact(n - 1); }",
        "console.log(fact(5));",
    ]);
    assert_eq!(console(&repl), vec!["120"]);
    assert!(
        repl.vm
            .code
            .iter()
            .any(|i| matches!(i, Instr::EnterFrame(1, _, kinds) if !kinds.is_empty())),
        "a self-recursive function allocates its self slot"
    );
}

/// And the saving is real: a unit of plain functions costs no more code than
/// their bodies need.
#[test]
fn functions_that_never_name_themselves_cost_no_prologue_closure() {
    let unit = Unit::new(&[
        "function a() { return 1; }\nfunction b() { return 2; }\nfunction c() { return 3; }\n",
    ]);
    let mut repl = Repl::new(serde_json::Value::Null, serde_json::Value::Null).unwrap();
    repl.push(&unit.buffer(0)).unwrap();
    // Three root bindings, three bodies. The only `ClosureNew`s are the
    // three that bind the names in the *root* frame.
    let closures = repl
        .vm
        .code
        .iter()
        .filter(|i| matches!(i, Instr::ClosureNew(..)))
        .count();
    assert_eq!(closures, 3, "one per binding, none inside a body");
}
