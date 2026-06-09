//! Optimizer — post-codegen passes over the *label-form* instruction stream.
//!
//! The compiler emits instructions with symbolic `Label` markers and label-id
//! jump targets. This module runs the optimization passes while targets are
//! still symbolic (so instructions can be freely deleted/rewritten), then
//! [`backpatch`] strips the labels and resolves every target to a real offset.
//!
//! [`finalize`] is the single entry point the compiler calls. It runs
//! [`optimize`] (a fixpoint over the passes below) followed by [`backpatch`]:
//!
//!   • [`invert_branches`] — `JFalse(L); Jump(M); Label(L)` → `JTrue(M)` (and
//!     the mirror): a conditional that branches *around* an unconditional jump
//!     is inverted so the jump folds away.
//!   • [`simplify_cfg`] — one reachability DFS that threads jumps, prunes
//!     unreachable code (including whole dead labeled blocks), and drops
//!     jump-to-next.
//!   • [`peephole`] — a local shift-reduce that cancels/fuses adjacent
//!     instructions and constant-folds `[const-push…, op]` windows.
//!
//! The passes mutually enable each other (e.g. peephole emptying a block to a
//! bare `Jump` exposes threading for cfg; cfg pruning exposes adjacencies for
//! peephole), so [`optimize`] iterates the trio to a fixpoint.

use crate::vm::{Instr, StackValue, StepResult, VM};

/// Compiler entry point: optimize the label-form code, then resolve labels.
pub(crate) fn finalize(
    code: Vec<Instr>,
    spans: Vec<u32>,
    next_label: u32,
) -> (Vec<Instr>, Vec<u32>) {
    let (code, spans) = optimize(code, spans, next_label);
    backpatch(code, spans, next_label)
}

/// First non-`Label` index at or after `i` (or `code.len()`). Labels are
/// zero-width markers, so this is where control "really" lands.
fn pe_next_real(code: &[Instr], mut i: usize) -> usize {
    while i < code.len() && matches!(code[i], Instr::Label(_)) {
        i += 1;
    }
    i
}

/// Follow a chain of unconditional jumps: if label `start` resolves to a
/// `Jump(M)`, the real destination is `M` (and so on). Bounded by the code
/// length and a self-loop guard so `while(true){}` can't spin here.
fn pe_thread(code: &[Instr], marker: &[u32], start: u32) -> u32 {
    let mut cur = start;
    for _ in 0..=code.len() {
        let m = marker[cur as usize];
        if m == u32::MAX {
            return cur; // label with no marker (shouldn't happen) — leave as-is
        }
        match code.get(pe_next_real(code, m as usize)) {
            Some(Instr::Jump(next)) if *next != cur => cur = *next,
            _ => return cur,
        }
    }
    cur
}

/// Whether `Label(l)` is the next real instruction at or after `from` (only
/// `Label` markers may sit between).
fn label_is_next(code: &[Instr], from: usize, l: u32) -> bool {
    let mut j = from;
    while let Some(Instr::Label(m)) = code.get(j) {
        if *m == l {
            return true;
        }
        j += 1;
    }
    false
}

/// Invert a conditional that branches around an unconditional jump:
///   `JFalse(L); Jump(M); …Label(L)` → `JTrue(M)`   (the dropped `Jump` is not a
///   jump target — nothing labels it — so removing it is safe)
///   `JTrue(L);  Jump(M); …Label(L)` → `JFalse(M)`
/// Requires `Label(L)` to be the *next real* instruction after the `Jump` (so the
/// false-arm body between them is empty); otherwise the inversion would change
/// where the not-taken arm lands. Commonly arises once peephole empties the
/// then-branch of an `if (c) {} else { … }`.
fn invert_branches(code: Vec<Instr>, spans: Vec<u32>) -> (Vec<Instr>, Vec<u32>) {
    let n = code.len();
    let mut out_code = Vec::with_capacity(n);
    let mut out_spans = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        if i + 1 < n {
            let inverted = match (&code[i], &code[i + 1]) {
                (Instr::JFalse(l), Instr::Jump(m)) => Some((*l, Instr::JTrue(*m))),
                (Instr::JTrue(l), Instr::Jump(m)) => Some((*l, Instr::JFalse(*m))),
                _ => None,
            };
            if let Some((l, inv)) = inverted {
                if label_is_next(&code, i + 2, l) {
                    out_code.push(inv);
                    out_spans.push(spans[i]);
                    i += 2; // consume the conditional and the now-folded Jump
                    continue;
                }
            }
        }
        out_code.push(code[i].clone());
        out_spans.push(spans[i]);
        i += 1;
    }
    (out_code, out_spans)
}

/// Control-flow simplification over the label-form code: a single reachability
/// walk — no fixpoint — that threads jumps, prunes unreachable code, and drops
/// jump-to-next.
///
///   • **threads jumps**: a jump whose target resolves to an unconditional
///     `Jump(M)` is retargeted straight to `M` (chain-followed once via
///     [`pe_thread`], self-loop guarded);
///   • **prunes dead code**: a DFS from the program entry *and* every function/
///     closure entry (labels named by `Call`/`PushFn`/`MakeClosure`, which are
///     reached by call rather than by a control-flow edge) marks every
///     reachable instruction; the rest — including whole unreachable labeled
///     blocks — are dropped;
///   • **drops jump-to-next**: a jump whose own `Label(L)` is the next real
///     instruction targets where control falls through anyway — `Jump(L)` is
///     dropped, `JFalse/JTrue(L)` degenerates to `Pop(1)` (both arms reach the
///     next instr, but the condition is still consumed), `JNotNullish(L)` is
///     dropped (it *peeks*, so there's nothing to consume).
///
/// One DFS is both simpler and strictly stronger than the old iterate-to-
/// fixpoint rebuild: reachability prunes dead labeled blocks a linear "after an
/// unconditional transfer" scan would keep. Surviving `Label`s are stripped by
/// [`backpatch`]; `Call`/`PushFn`/`MakeClosure` operands are never threaded.
fn simplify_cfg(code: Vec<Instr>, spans: Vec<u32>, next_label: u32) -> (Vec<Instr>, Vec<u32>) {
    let n = code.len();
    if n == 0 {
        return (code, spans);
    }

    // Map each label id to the index of its `Label` marker.
    let mut marker = vec![u32::MAX; next_label as usize];
    for (i, instr) in code.iter().enumerate() {
        if let Instr::Label(id) = instr {
            marker[*id as usize] = i as u32;
        }
    }
    let marker_idx = |label: u32| -> Option<usize> {
        marker
            .get(label as usize)
            .copied()
            .filter(|&m| m != u32::MAX)
            .map(|m| m as usize)
    };

    // Reachability DFS. Roots: the program entry (0) plus every function/closure
    // entry point (reached by call, not by a control-flow successor edge).
    let mut visited = vec![false; n];
    let mut work: Vec<usize> = vec![0];
    for instr in &code {
        if let Instr::Call(l, _) | Instr::PushFn(l) | Instr::MakeClosure(l, _) = instr {
            if let Some(idx) = marker_idx(*l) {
                work.push(idx);
            }
        }
    }
    while let Some(i) = work.pop() {
        if i >= n || visited[i] {
            continue;
        }
        visited[i] = true;
        match &code[i] {
            // Unconditional transfer / terminator: no fall-through successor.
            Instr::Return(_) => {}
            Instr::Jump(l) => {
                if let Some(idx) = marker_idx(pe_thread(&code, &marker, *l)) {
                    work.push(idx);
                }
            }
            // Two-way branch: fall through AND take the (threaded) target.
            Instr::JFalse(l) | Instr::JTrue(l) | Instr::JNotNullish(l) => {
                work.push(i + 1);
                if let Some(idx) = marker_idx(pe_thread(&code, &marker, *l)) {
                    work.push(idx);
                }
            }
            // Everything else (incl. Label, Call, Invoke, Raise, CallBuiltin,
            // CallDyn) falls through to the next instruction.
            _ => work.push(i + 1),
        }
    }

    // Emit reachable instructions, threading jump operands.
    let mut kept: Vec<Instr> = Vec::with_capacity(n);
    let mut kept_spans: Vec<u32> = Vec::with_capacity(n);
    for i in 0..n {
        if !visited[i] {
            continue;
        }
        let instr = match &code[i] {
            Instr::Jump(l) => Instr::Jump(pe_thread(&code, &marker, *l)),
            Instr::JFalse(l) => Instr::JFalse(pe_thread(&code, &marker, *l)),
            Instr::JTrue(l) => Instr::JTrue(pe_thread(&code, &marker, *l)),
            Instr::JNotNullish(l) => Instr::JNotNullish(pe_thread(&code, &marker, *l)),
            other => other.clone(),
        };
        kept.push(instr);
        kept_spans.push(spans[i]);
    }

    // Drop / degenerate jump-to-next (see the doc comment for the per-kind rule).
    let mut out_code = Vec::with_capacity(kept.len());
    let mut out_spans = Vec::with_capacity(kept.len());
    for (idx, instr) in kept.iter().enumerate() {
        match instr {
            Instr::Jump(l) if label_is_next(&kept, idx + 1, *l) => continue,
            Instr::JNotNullish(l) if label_is_next(&kept, idx + 1, *l) => continue,
            Instr::JFalse(l) | Instr::JTrue(l) if label_is_next(&kept, idx + 1, *l) => {
                out_code.push(Instr::Pop(1));
                out_spans.push(kept_spans[idx]);
                continue;
            }
            _ => {}
        }
        out_code.push(instr.clone());
        out_spans.push(kept_spans[idx]);
    }
    (out_code, out_spans)
}

/// True for instructions that push exactly one value with no side effect, so a
/// `<pure-push>; Pop(n)` (e.g. a bare `x;` / `5;` statement) reduces to
/// `Pop(n-1)` in [`peephole`] — the pushed value is the top, hence one of the
/// `n` discarded.
fn pe_is_pure_push(i: &Instr) -> bool {
    matches!(
        i,
        Instr::PushNull
            | Instr::PushUndefined
            | Instr::PushBool(_)
            | Instr::PushFloat(_)
            | Instr::PushPosInt(_)
            | Instr::PushNegInt(_)
            | Instr::PushFn(_)
            | Instr::PushObject(_)
            | Instr::PushBuiltin(_)
            | Instr::PushStr(_)
            | Instr::Local(_)
            | Instr::Dup
    )
}

/// True for a push of a compile-time *constant value* (a literal). Stricter than
/// [`pe_is_pure_push`]: excludes `Local`/`Dup` (not constants) and `PushPtr`/
/// `PushFn`/`PushBuiltin` (heap/code references, and `PushFn` still carries an
/// unresolved label id here). These are the only operands [`pe_try_constfold`]
/// will evaluate.
fn pe_is_const_push(i: &Instr) -> bool {
    matches!(
        i,
        Instr::PushNull
            | Instr::PushUndefined
            | Instr::PushBool(_)
            | Instr::PushFloat(_)
            | Instr::PushPosInt(_)
            | Instr::PushNegInt(_)
            | Instr::PushStr(_)
    )
}

/// Truthiness of a constant push, matching the VM's JS falsy set (`false`,
/// `0`/`-0`, `NaN`, `""`, `null`, `undefined`). `None` for non-constant pushes.
/// Drives constant branch folding (`PushBool(false); JFalse(L)` → `Jump(L)`).
fn pe_const_truthy(i: &Instr) -> Option<bool> {
    Some(match i {
        Instr::PushNull | Instr::PushUndefined => false,
        Instr::PushBool(b) => *b,
        Instr::PushPosInt(u) => *u != 0,
        Instr::PushNegInt(_) => true, // always negative ⇒ nonzero ⇒ truthy
        Instr::PushFloat(n) => *n != 0.0 && !n.is_nan(),
        Instr::PushStr(s) => !s.as_str().is_empty(),
        _ => return None,
    })
}

/// True for instructions whose result is already a `Bool`, so a following
/// `ToBool` is a no-op. Excludes `And`/`Or`, which return one of their operands
/// (not necessarily a bool) under JS semantics.
fn pe_produces_bool(i: &Instr) -> bool {
    matches!(
        i,
        Instr::Not
            | Instr::ToBool
            | Instr::PushBool(_)
            | Instr::Eq
            | Instr::Neq
            | Instr::LooseEq
            | Instr::LooseNeq
            | Instr::Lt
            | Instr::Gt
            | Instr::LtEq
            | Instr::GtEq
            | Instr::IsNull
            | Instr::IsBool
            | Instr::IsFloat
            | Instr::IsNum
            | Instr::IsStr
            | Instr::IsObj
            | Instr::ObjHas
            | Instr::ObjDelete
    )
}

/// Operand count of a pure, deterministic, constant-foldable instruction (1 for
/// unary, 2 for binary), or `None` if it isn't foldable. Excludes anything that
/// touches the heap, frame, or host (`Local`, `ObjGet`, `Invoke`, builtins, …)
/// and `And`/`Or` (rarely emitted; short-circuit lowers to jumps).
fn pe_fold_arity(op: &Instr) -> Option<usize> {
    Some(match op {
        Instr::Neg
        | Instr::Not
        | Instr::BitNot
        | Instr::ToNum
        | Instr::ToBool
        | Instr::ToStr
        | Instr::TypeOf
        | Instr::IsNull
        | Instr::IsBool
        | Instr::IsFloat
        | Instr::IsNum
        | Instr::IsStr
        | Instr::IsObj => 1,
        Instr::Add
        | Instr::Sub
        | Instr::Mul
        | Instr::Div
        | Instr::Mod
        | Instr::Pow
        | Instr::Eq
        | Instr::Neq
        | Instr::LooseEq
        | Instr::LooseNeq
        | Instr::Lt
        | Instr::Gt
        | Instr::LtEq
        | Instr::GtEq
        | Instr::BitAnd
        | Instr::BitOr
        | Instr::BitXor
        | Instr::BitLhs
        | Instr::BitRhs => 2,
        _ => return None,
    })
}

/// Convert a folded runtime value back into the push that reproduces it, or
/// `None` for values that aren't compile-time constants (heap pointers,
/// functions, builtins). A folded `Number` stays `PushFloat` — never re-
/// canonicalized to an int — so it matches what the op produced at runtime.
fn pe_value_to_push(v: &StackValue) -> Option<Instr> {
    Some(match v {
        StackValue::Null => Instr::PushNull,
        StackValue::Undefined => Instr::PushUndefined,
        StackValue::Bool(b) => Instr::PushBool(*b),
        StackValue::PosInt(u) => Instr::PushPosInt(*u),
        StackValue::NegInt(i) => Instr::PushNegInt(*i),
        StackValue::Number(n) => Instr::PushFloat(*n),
        StackValue::String(s) => Instr::PushStr(s.clone()),
        _ => return None,
    })
}

/// If the tail of `out` is `[<const-push> × arity, <foldable-op>]`, evaluate it
/// in a throwaway `VM` and return `(window_len, replacement_push)`. Running it
/// through the real VM gives perfect semantic fidelity (no re-implemented
/// coercions). Returns `None` if the op isn't foldable, the operands aren't all
/// constants (a `Label` among them blocks the fold, so it can't cross a jump
/// target), the evaluation errors (the runtime error is *preserved* rather than
/// turned into a compile-time fold), or the result isn't a representable
/// constant.
fn pe_try_constfold(out: &[Instr]) -> Option<(usize, Instr)> {
    let op = out.last()?;
    let arity = pe_fold_arity(op)?;
    if out.len() < arity + 1 {
        return None;
    }
    let operands = &out[out.len() - 1 - arity..out.len() - 1];
    if !operands.iter().all(pe_is_const_push) {
        return None;
    }
    let mut prog: Vec<Instr> = operands.to_vec();
    prog.push(op.clone());
    let mut vm = VM::new(prog);
    // A single `step()` runs the whole jumpless program to `Done` (ip past the
    // end). An effect or error means "don't fold" — preserve runtime behavior.
    match vm.step() {
        Ok(StepResult::Done) => {}
        _ => return None,
    }
    if vm.stack.len() != 1 {
        return None;
    }
    let push = pe_value_to_push(&vm.stack[0])?;
    Some((arity + 1, push))
}

/// Evaluate a self-contained sequence of constant pushes + pure foldable ops to
/// the single push that reproduces its result, or `None` if the sequence isn't
/// purely constant, doesn't reduce to exactly one value, or errors at runtime.
///
/// Used by the compiler for `const`-binding propagation: it evaluates a `const`
/// initializer's instructions to learn the bound value, then emits that literal
/// at each reference instead of a `Local` load (see `compiler::compile_identifier`).
pub(crate) fn const_eval(instrs: &[Instr]) -> Option<Instr> {
    if instrs.is_empty() {
        return None;
    }
    // Reject anything that touches the frame/heap/host or isn't a constant.
    if !instrs
        .iter()
        .all(|i| pe_is_const_push(i) || pe_fold_arity(i).is_some())
    {
        return None;
    }
    let mut vm = VM::new(instrs.to_vec());
    match vm.step() {
        Ok(StepResult::Done) => {}
        _ => return None, // multi-step effect or runtime error → not a constant
    }
    if vm.stack.len() != 1 {
        return None;
    }
    pe_value_to_push(&vm.stack[0])
}

/// The result of trying to combine two adjacent instructions in [`peephole`].
enum Reduction {
    /// Both instructions cancel; drop them.
    Cancel,
    /// Both fuse into one replacement instruction.
    Replace(Instr),
    /// No rule applies; leave them.
    Keep,
}

/// Try to combine adjacent `a; b` into a cheaper form. A `Label` is never an
/// operand of any rule, and `a`/`b` are adjacent in the output, so no rewrite
/// can cross a jump target. Patterns:
///
///   • `Swap; Swap` → ∅                    (involution)
///   • `Swap; Pop(1)` → `Nip(1)`           (drop the value below the top)
///   • `Swap; Pop(n≥2)` → `Pop(n)`         (swap is moot if both are popped)
///   • `Dup; SetLocal(x)` → `TeeLocal(x)`  (write without the extra copy/pop)
///   • `Not; Not` → `ToBool`               (double negation is boolean coercion)
///   • `Not; JFalse(L)` → `JTrue(L)`,  `Not; JTrue(L)` → `JFalse(L)`
///   • `ToBool; {JFalse|JTrue|Not}` → drop the `ToBool` (the consumer coerces)
///   • `<bool-producer>; ToBool` → drop the `ToBool` (already a bool; covers
///     `ToBool;ToBool`, `Eq;ToBool`, `Lt;ToBool`, `Not;ToBool`, …)
///   • `<const c>; JFalse(L)` → `Jump(L)` if `c` falsy, else ∅ (constant branch
///     folding; `JTrue` mirrors). The freed dead arm is then pruned by
///     `simplify_cfg` on the next fixpoint iteration.
///   • `Pop(a); Pop(b)` → `Pop(a+b)`
///   • `<pure-push>; Pop(n)` → `Pop(n-1)`  (∅ when n==1; the pushed value is
///     one of the discarded)
fn pe_reduce(a: &Instr, b: &Instr) -> Reduction {
    match (a, b) {
        (Instr::Swap, Instr::Swap) => Reduction::Cancel,
        (Instr::Swap, Instr::Pop(n)) if *n == 1 => Reduction::Replace(Instr::Nip(1)),
        (Instr::Swap, Instr::Pop(n)) => Reduction::Replace(Instr::Pop(*n)),
        (Instr::Dup, Instr::SetLocal(x)) => Reduction::Replace(Instr::TeeLocal(*x)),
        (Instr::Not, Instr::Not) => Reduction::Replace(Instr::ToBool),
        (Instr::Not, Instr::JFalse(l)) => Reduction::Replace(Instr::JTrue(*l)),
        (Instr::Not, Instr::JTrue(l)) => Reduction::Replace(Instr::JFalse(*l)),
        // A `ToBool` whose result is immediately re-coerced is redundant.
        (Instr::ToBool, Instr::JFalse(_) | Instr::JTrue(_) | Instr::Not) => {
            Reduction::Replace(b.clone())
        }
        // A `ToBool` right after anything that already yields a bool is a no-op.
        (p, Instr::ToBool) if pe_produces_bool(p) => Reduction::Replace(p.clone()),
        // Constant branch folding: a conditional jump on a known constant becomes
        // an unconditional jump (taken) or vanishes (not taken). Both consume the
        // condition, so the const push folds away with it.
        (p, Instr::JFalse(l)) => match pe_const_truthy(p) {
            Some(false) => Reduction::Replace(Instr::Jump(*l)),
            Some(true) => Reduction::Cancel,
            None => Reduction::Keep,
        },
        (p, Instr::JTrue(l)) => match pe_const_truthy(p) {
            Some(true) => Reduction::Replace(Instr::Jump(*l)),
            Some(false) => Reduction::Cancel,
            None => Reduction::Keep,
        },
        (Instr::Pop(x), Instr::Pop(y)) => Reduction::Replace(Instr::Pop(x + y)),
        (p, Instr::Pop(n)) if pe_is_pure_push(p) => match *n {
            // `Pop(0)` is a no-op; leave it (cancelling would lose the value).
            0 => Reduction::Keep,
            1 => Reduction::Cancel,
            _ => Reduction::Replace(Instr::Pop(n - 1)),
        },
        _ => Reduction::Keep,
    }
}

/// Local-window peephole. Shift-reduce: push each incoming instruction, then
/// repeatedly reduce the top of the output until nothing applies — first the
/// adjacent-pair rules ([`pe_reduce`]), then constant folding of a
/// `[const-push…, op]` window ([`pe_try_constfold`]). Re-reducing after every
/// push means a reduction's *result* is re-examined against what's now beneath
/// it, so cascades collapse in any direction (`Swap;Swap;Swap;Swap`, nested
/// `(1+2)*3`, `Pop;Pop;Pop`).
///
/// Composed with the CFG passes under a fixpoint loop (see [`optimize`]).
fn peephole(code: Vec<Instr>, spans: Vec<u32>) -> (Vec<Instr>, Vec<u32>) {
    let mut out: Vec<Instr> = Vec::with_capacity(code.len());
    let mut out_spans: Vec<u32> = Vec::with_capacity(code.len());
    for (instr, span) in code.into_iter().zip(spans) {
        // `Pop(0)` is a no-op — drop it outright (and let its neighbours, now
        // adjacent, reduce). Safe even at a jump target: control just proceeds.
        if matches!(instr, Instr::Pop(0)) {
            continue;
        }
        out.push(instr);
        out_spans.push(span);
        loop {
            let n = out.len();
            // Adjacent-pair rules.
            if n >= 2 {
                match pe_reduce(&out[n - 2], &out[n - 1]) {
                    Reduction::Cancel => {
                        out.truncate(n - 2);
                        out_spans.truncate(n - 2);
                        continue;
                    }
                    Reduction::Replace(r) => {
                        let sp = out_spans[n - 1];
                        out.truncate(n - 2);
                        out_spans.truncate(n - 2);
                        out.push(r);
                        out_spans.push(sp);
                        continue;
                    }
                    Reduction::Keep => {}
                }
            }
            // Constant folding of a `[const…, op]` window.
            if let Some((k, push)) = pe_try_constfold(&out) {
                let sp = out_spans[out.len() - 1];
                let len = out.len();
                out.truncate(len - k);
                out_spans.truncate(len - k);
                out.push(push);
                out_spans.push(sp);
                continue;
            }
            break;
        }
    }
    (out, out_spans)
}

/// Run the optimization passes to a fixpoint. Each pass is internally single-
/// shot (cfg is one reachability DFS, peephole is one shift-reduce), but they
/// mutually enable each other, so the *trio* is iterated until the code stops
/// changing. Instruction count is monotonically non-increasing (every transform
/// removes/fuses or is an idempotent operand rewrite), so this terminates well
/// before the safety cap.
fn optimize(mut code: Vec<Instr>, mut spans: Vec<u32>, next_label: u32) -> (Vec<Instr>, Vec<u32>) {
    for _ in 0..32 {
        let prev = code.clone();
        let (c, s) = invert_branches(code, spans);
        let (c, s) = simplify_cfg(c, s, next_label);
        let (c, s) = peephole(c, s);
        code = c;
        spans = s;
        if code == prev {
            break;
        }
    }
    (code, spans)
}

/// Strip `Label` markers and rewrite every label-id address into a real code
/// offset, copying spans in lockstep so the table stays aligned with the
/// compacted code. A first scan records each label's offset; a second emits the
/// rewritten stream.
fn backpatch(code: Vec<Instr>, spans: Vec<u32>, next_label: u32) -> (Vec<Instr>, Vec<u32>) {
    // First scan: the offset of each label is the count of non-Label
    // instructions preceding it.
    let mut label_offset = vec![0u32; next_label as usize];
    let mut offset = 0u32;
    for instr in &code {
        match instr {
            Instr::Label(id) => label_offset[*id as usize] = offset,
            _ => offset += 1,
        }
    }

    // Second scan: drop Labels, rewrite addresses (which carry label ids until
    // now), and emit spans in lockstep.
    let mut out_code = Vec::with_capacity(code.len());
    let mut out_spans = Vec::with_capacity(spans.len());
    for (instr, span) in code.into_iter().zip(spans) {
        let rewritten = match instr {
            Instr::Label(_) => continue,
            Instr::Jump(l) => Instr::Jump(label_offset[l as usize]),
            Instr::JFalse(l) => Instr::JFalse(label_offset[l as usize]),
            Instr::JTrue(l) => Instr::JTrue(label_offset[l as usize]),
            Instr::JNotNullish(l) => Instr::JNotNullish(label_offset[l as usize]),
            Instr::Call(l, n) => Instr::Call(label_offset[l as usize], n),
            Instr::MakeClosure(l, caps) => Instr::MakeClosure(label_offset[l as usize], caps),
            Instr::PushFn(l) => Instr::PushFn(label_offset[l as usize]),
            other => other,
        };
        out_code.push(rewritten);
        out_spans.push(span);
    }
    (out_code, out_spans)
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::Builtin;
    use crate::vm::RcStr;

    fn s0(n: usize) -> Vec<u32> {
        vec![0u32; n]
    }

    // ── peephole: adjacent-pair rules ────────────────────────────────

    #[test]
    fn peephole_cancels_and_fuses() {
        // Swap;Swap;Swap;Swap cancels fully.
        let (c, _) = peephole(
            vec![Instr::Swap, Instr::Swap, Instr::Swap, Instr::Swap],
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
        let code = vec![Instr::Swap, Instr::Label(0), Instr::Swap];
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
        let code = vec![Instr::Local(0), Instr::JFalse(0)];
        assert_eq!(peephole(code.clone(), s0(2)).0, code);
    }

    #[test]
    fn peephole_dup_setlocal_to_tee() {
        let (c, _) = peephole(vec![Instr::Dup, Instr::SetLocal(3)], s0(2));
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
            peephole(vec![Instr::Swap, Instr::Pop(1)], s0(2)).0,
            vec![Instr::Nip(1)]
        );
        assert_eq!(
            peephole(vec![Instr::Swap, Instr::Pop(2)], s0(2)).0,
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
        let code = vec![Instr::Local(0), Instr::PushPosInt(1), Instr::Add];
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
}
