use super::*;
use crate::builtin::Builtin;
use crate::vm::RcStr;

fn s0(n: usize) -> Vec<u32> {
    vec![0u32; n]
}

// ── peephole: adjacent-pair rules ────────────────────────────────

#[test]
fn peephole_cancels_and_fuses() {
    // Dig(1);Dig(1);Dig(1);Dig(1) cancels fully.
    let (c, _) = peephole(
        vec![Instr::Dig(1), Instr::Dig(1), Instr::Dig(1), Instr::Dig(1)],
        s0(4),
    );
    assert!(c.is_empty(), "{c:?}");

    // Not;Not → ToBool; Pop fusion; pure-push;Pop vanish.
    assert_eq!(
        peephole(vec![Instr::Not, Instr::Not], s0(2)).0,
        vec![Instr::ToBool]
    );
    assert_eq!(
        peephole(vec![Instr::Pop(2), Instr::Pop(3)], s0(2)).0,
        vec![Instr::Pop(5)]
    );
    assert!(
        peephole(vec![Instr::PushPosInt(7), Instr::Pop(1)], s0(2))
            .0
            .is_empty()
    );

    // A Label blocks a rewrite (control could enter between).
    let code = vec![Instr::Dig(1), Instr::Label(0), Instr::Dig(1)];
    assert_eq!(peephole(code.clone(), s0(3)).0, code);

    // An effectful producer is not removed by the pure-push rule.
    let code = vec![Instr::CallBuiltin(Builtin::MathAbs, 1), Instr::Pop(1)];
    assert_eq!(peephole(code.clone(), s0(2)).0, code);
}

#[test]
fn peephole_constant_branch_folding() {
    // falsy const + JFalse → unconditional Jump
    assert_eq!(
        peephole(vec![Instr::PushBool(false), Instr::JFalse(0)], s0(2)).0,
        vec![Instr::Jump(0)]
    );
    // truthy const + JFalse → vanishes (falls through)
    assert!(
        peephole(vec![Instr::PushBool(true), Instr::JFalse(0)], s0(2))
            .0
            .is_empty()
    );
    // truthy const + JTrue → Jump; falsy + JTrue → vanishes
    assert_eq!(
        peephole(vec![Instr::PushPosInt(1), Instr::JTrue(0)], s0(2)).0,
        vec![Instr::Jump(0)]
    );
    assert!(
        peephole(vec![Instr::PushPosInt(0), Instr::JTrue(0)], s0(2))
            .0
            .is_empty()
    );
    // "" is falsy, "0" is truthy.
    assert_eq!(
        peephole(
            vec![Instr::PushStr(RcStr::from("")), Instr::JFalse(0)],
            s0(2)
        )
        .0,
        vec![Instr::Jump(0)]
    );
    // A non-constant condition is untouched.
    let code = vec![Instr::GetLocal(0), Instr::JFalse(0)];
    assert_eq!(peephole(code.clone(), s0(2)).0, code);
}

#[test]
fn peephole_dup_setlocal_to_tee() {
    let (c, _) = peephole(vec![Instr::Pick(0), Instr::SetLocal(3)], s0(2));
    assert_eq!(c, vec![Instr::TeeLocal(3)]);
}

#[test]
fn peephole_tobool_absorption() {
    for consumer in [Instr::JFalse(0), Instr::JTrue(0), Instr::Not] {
        let (c, _) = peephole(vec![Instr::ToBool, consumer.clone()], s0(2));
        assert_eq!(c, vec![consumer]);
    }
    assert_eq!(
        peephole(vec![Instr::Lt, Instr::ToBool], s0(2)).0,
        vec![Instr::Lt]
    );
    // !!!x : Not;Not;Not → ToBool;Not → Not.
    assert_eq!(
        peephole(vec![Instr::Not, Instr::Not, Instr::Not], s0(3)).0,
        vec![Instr::Not]
    );
    // ToBool must NOT be absorbed before JNotNullish (changes nullishness).
    let code = vec![Instr::ToBool, Instr::JNotNullish(0)];
    assert_eq!(peephole(code.clone(), s0(2)).0, code);
}

#[test]
fn peephole_push_pop_n_and_pop0() {
    assert_eq!(
        peephole(vec![Instr::PushPosInt(1), Instr::Pop(3)], s0(2)).0,
        vec![Instr::Pop(2)]
    );
    // Two pushes + Pop(2) cancel entirely (cascade).
    assert!(
        peephole(
            vec![Instr::PushPosInt(1), Instr::PushPosInt(2), Instr::Pop(2)],
            s0(3)
        )
        .0
        .is_empty()
    );
    // Pop(0) is dropped outright, exposing neighbours.
    assert!(peephole(vec![Instr::Pop(0)], s0(1)).0.is_empty());
    assert!(
        peephole(
            vec![Instr::PushPosInt(1), Instr::Pop(0), Instr::Pop(1)],
            s0(3)
        )
        .0
        .is_empty()
    );
}

#[test]
fn peephole_swap_pop() {
    assert_eq!(
        peephole(vec![Instr::Dig(1), Instr::Pop(1)], s0(2)).0,
        vec![Instr::Nip(1)]
    );
    assert_eq!(
        peephole(vec![Instr::Dig(1), Instr::Pop(2)], s0(2)).0,
        vec![Instr::Pop(2)]
    );
}

// ── peephole: constant folding ───────────────────────────────────

#[test]
fn constfold_arithmetic() {
    // 2 * 3 → 6.0 (VM arithmetic yields Number).
    let (c, _) = peephole(
        vec![Instr::PushPosInt(2), Instr::PushPosInt(3), Instr::Mul],
        s0(3),
    );
    assert_eq!(c, vec![Instr::PushFloat(6.0)]);
    // Nested: 1 + 2 * 3 → 7.0.
    let (c, _) = peephole(
        vec![
            Instr::PushPosInt(1),
            Instr::PushPosInt(2),
            Instr::PushPosInt(3),
            Instr::Mul,
            Instr::Add,
        ],
        s0(5),
    );
    assert_eq!(c, vec![Instr::PushFloat(7.0)]);
}

#[test]
fn constfold_string_and_compare_and_unary() {
    // String concat.
    let (c, _) = peephole(
        vec![
            Instr::PushStr(RcStr::from("a")),
            Instr::PushStr(RcStr::from("b")),
            Instr::Add,
        ],
        s0(3),
    );
    assert_eq!(c, vec![Instr::PushStr(RcStr::from("ab"))]);
    // Comparison → Bool.
    let (c, _) = peephole(
        vec![Instr::PushPosInt(1), Instr::PushPosInt(2), Instr::Lt],
        s0(3),
    );
    assert_eq!(c, vec![Instr::PushBool(true)]);
    // Unary Neg.
    let (c, _) = peephole(vec![Instr::PushPosInt(5), Instr::Neg], s0(2));
    assert_eq!(c, vec![Instr::PushFloat(-5.0)]);
}

#[test]
fn constfold_preserves_runtime_errors() {
    // A shift count out of range errors at runtime — do NOT fold it.
    let code = vec![Instr::PushPosInt(1), Instr::PushPosInt(99), Instr::BitLhs];
    assert_eq!(peephole(code.clone(), s0(3)).0, code);
}

#[test]
fn constfold_skips_non_constant_operands() {
    // A `Local` operand is not a compile-time constant.
    let code = vec![Instr::GetLocal(0), Instr::PushPosInt(1), Instr::Add];
    assert_eq!(peephole(code.clone(), s0(3)).0, code);
}

// ── simplify_cfg ─────────────────────────────────────────────────

#[test]
fn cfg_prunes_unreachable_block_and_threads() {
    // `Jump(1)` skips an orphan block (Label 0 never targeted); the dead
    // block is pruned and the now jump-to-next `Jump(1)` dropped.
    let code = vec![
        Instr::Jump(1),
        Instr::Label(0),
        Instr::PushPosInt(999),
        Instr::Label(1),
        Instr::Return(0),
    ];
    let (out, out_spans) = simplify_cfg(code, s0(5), 2);
    assert_eq!(out, vec![Instr::Label(1), Instr::Return(0)]);
    assert_eq!(out_spans.len(), out.len());
}

#[test]
fn cfg_collapses_chain_of_empty_blocks() {
    // A chain of empty jump-only blocks threads through in one shot: the
    // skipped blocks are pruned, the trailing jump-to-next dropped, and the
    // zero-width labels stripped by backpatch — leaving just the target.
    let code = vec![
        Instr::Jump(0),
        Instr::Label(0),
        Instr::Jump(1),
        Instr::Label(1),
        Instr::Jump(2),
        Instr::Label(2),
        Instr::Return(0),
    ];
    let (out, _) = finalize(code, s0(7), 3);
    assert_eq!(out, vec![Instr::Return(0)]);
}

#[test]
fn cfg_keeps_call_referenced_block() {
    let code = vec![
        Instr::PushFn(0),
        Instr::Pop(1),
        Instr::Return(0),
        Instr::Label(0),
        Instr::PushPosInt(42),
        Instr::Return(1),
    ];
    let (out, _) = simplify_cfg(code, s0(6), 1);
    assert!(out.iter().any(|i| matches!(i, Instr::PushPosInt(42))));
}

#[test]
fn cfg_conditional_jump_to_next_becomes_pop() {
    // `JFalse(0); Label(0)` — both arms reach the next instr → just Pop(1).
    let code = vec![Instr::JFalse(0), Instr::Label(0), Instr::Return(0)];
    let (out, _) = simplify_cfg(code, s0(3), 1);
    assert_eq!(out, vec![Instr::Pop(1), Instr::Label(0), Instr::Return(0)]);
}

// ── invert_branches ──────────────────────────────────────────────

#[test]
fn invert_branch_around_jump() {
    // JFalse(0); Jump(1); Label(0) → JTrue(1); Label(0)
    let code = vec![
        Instr::JFalse(0),
        Instr::Jump(1),
        Instr::Label(0),
        Instr::Label(1),
    ];
    let (out, _) = invert_branches(code, s0(4));
    assert_eq!(out, vec![Instr::JTrue(1), Instr::Label(0), Instr::Label(1)]);

    // JTrue mirror.
    let code = vec![Instr::JTrue(0), Instr::Jump(1), Instr::Label(0)];
    let (out, _) = invert_branches(code, s0(3));
    assert_eq!(out, vec![Instr::JFalse(1), Instr::Label(0)]);

    // NOT inverted when a real instr sits between the Jump and Label(0).
    let code = vec![
        Instr::JFalse(0),
        Instr::Jump(1),
        Instr::PushNull,
        Instr::Label(0),
    ];
    assert_eq!(invert_branches(code.clone(), s0(4)).0, code);
}

// ── optimize: fixpoint interaction ───────────────────────────────

#[test]
fn optimize_threads_through_block_emptied_by_peephole() {
    // A block reachable by `Jump 0` is only `<pure-push>; Pop(1); Jump B`.
    // simplify_cfg can't thread `Jump 0` at first; peephole empties the
    // block; the fixpoint then threads/collapses. End state: no stale jumps,
    // dead value gone, B's body intact.
    let code = vec![
        Instr::Jump(0),       // 0
        Instr::Label(2),      // 1
        Instr::Return(0),     // 2
        Instr::Label(0),      // 3: block A
        Instr::PushPosInt(9), // 4
        Instr::Pop(1),        // 5
        Instr::Jump(1),       // 6 → B
        Instr::Label(1),      // 7: block B
        Instr::Return(1),     // 8
    ];
    let (out, _) = optimize(code, s0(9), 3);
    assert!(!out.iter().any(|i| matches!(i, Instr::PushPosInt(9))));
    assert!(out.iter().any(|i| matches!(i, Instr::Return(1))));
    let jumps = out.iter().filter(|i| matches!(i, Instr::Jump(_))).count();
    assert!(jumps <= 1, "{out:?}");
}
