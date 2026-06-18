use std::collections::{HashMap, HashSet};

use crate::vm::SlotKind;

use super::scope::{FuncScope, SlotInfo};

/// How an identifier reference resolves to a frame slot.
#[derive(Copy, Clone, Debug)]
pub(crate) struct RefSlot {
    pub(crate) slot: u32,
    /// Declared `const` (drives the const-reassignment error).
    pub(crate) is_const: bool,
    /// Immutable in fact: a `const`, or a `let`/`var` that is never reassigned
    /// and never captured. Gates constant propagation (a stronger, value-safe
    /// condition than `is_const`); does NOT relax the reassignment check.
    pub(crate) immutable: bool,
}

/// Complete scope-analysis result for a compilation unit.
#[derive(Debug)]
pub(crate) struct ProgramAnalysis {
    pub(crate) scopes: Vec<FuncScope>,
    pub(crate) root: usize,
    /// Binding occurrence span → absolute frame slot. Drives the `SetLocal`
    /// target at every declaration site.
    pub(crate) binding_slot: HashMap<u32, u32>,
    /// Identifier-reference span → resolved frame slot. Absent means the name is
    /// not a local (global/`input`/`undefined`/… or undeclared); codegen falls
    /// back to name-based resolution.
    pub(crate) ref_resolution: HashMap<u32, RefSlot>,
    /// Binding occurrence span → whether the binding is immutable in fact (see
    /// `RefSlot::immutable`). Drives whether a `const`/effectively-const
    /// initializer is recorded for propagation.
    pub(crate) binding_immutable: HashMap<u32, bool>,
    /// Binding occurrence span → whether the binding is captured by a nested
    /// function. A non-captured, fully-propagated binding has a dead store the
    /// compiler can drop (a captured one's slot is read by `MakeClosure`).
    pub(crate) binding_captured: HashMap<u32, bool>,
    /// Identifier-reference span → the compile-time constant it resolves to
    /// (a `const x = <literal>` binding, intra- or cross-function). The compiler
    /// emits the literal; these bindings have no slot and are never captured.
    pub(crate) const_refs: HashMap<u32, super::const_fns::ConstValue>,
    /// Scope ids of constant functions (Phase F): non-reassigned function
    /// declarations that capture nothing. The compiler skips their binding store
    /// (the slot is dead) — references/calls go through `const_refs`.
    pub(crate) const_fn_scopes: std::collections::HashSet<usize>,
    /// Function/arrow AST node span → its `scopes` index.
    pub(crate) scope_by_span: HashMap<u32, usize>,
}

/// Bottom-up capture resolution. Phase A propagates each scope's free variables
/// into its parent (children have lower ids than parents, so ascending order
/// visits children first). Phase B (descending: parents first) turns every
/// resolvable free variable into an upval — sourced from a parent local (which
/// it boxes) or a parent upval — and fixes `upval_count`. A final pass derives
/// each scope's own-local `slot_kinds`.
pub(crate) fn resolve_captures(scopes: &mut [FuncScope]) {
    let n = scopes.len();

    // Phase A: propagate free variables that the parent doesn't declare upward.
    for i in 0..n {
        let parent = scopes[i].parent;
        if parent == usize::MAX {
            continue;
        }
        let self_name = scopes[i].self_name.clone();
        let frees: Vec<String> = scopes[i].free_vars.iter().cloned().collect();
        for fv in frees {
            if self_name.as_deref() == Some(fv.as_str()) {
                continue;
            }
            // <this> boundary: a non-arrow child "declares" <this> — don't
            // propagate. An arrow child is transparent — propagate upward and
            // flag the parent for reification if it is non-arrow.
            if fv == "<this>" {
                if scopes[i].is_arrow {
                    if !scopes[parent].is_arrow {
                        scopes[parent].needs_this_reify = true;
                    }
                    scopes[parent].free_vars.insert(fv);
                }
                continue;
            }
            // Don't propagate a name the parent resolves: a slot (`names`) is
            // captured below; a const (`const_names`) resolves to a value here.
            if !scopes[parent].names.contains_key(&fv)
                && !scopes[parent].const_names.contains_key(&fv)
            {
                scopes[parent].free_vars.insert(fv);
            }
        }
    }

    // Reify `this` for non-arrow scopes whose arrow descendants reference
    // `<this>`.  Only scopes with `needs_this_reify` set (by Phase A from an
    // arrow child) are reified; a direct `this` in a non-arrow scope with no
    // capturing arrows does not need a slot and emits `LoadThis` directly.
    for i in (0..n).rev() {
        let s = &mut scopes[i];
        if s.is_arrow || !s.needs_this_reify {
            continue;
        }
        s.free_vars.shift_remove("<this>");
        s.this_slot = Some(s.own_local_count);
        s.names.insert(
            "<this>".to_string(),
            SlotInfo {
                slot: s.own_local_count,
                is_const: false,
            },
        );
        // Ensure slot_names is large enough for debug info.
        if s.own_slot_names.len() <= s.own_local_count as usize {
            s.own_slot_names
                .resize(s.own_local_count as usize + 1, None);
        }
        s.own_slot_names[s.own_local_count as usize] = Some("<this>".to_string());
        // Increment own_local_count so the slot is allocated and appears in
        // slot_kinds. Mark it captured (→ Boxed) so closures capture the cell.
        s.captured.insert(s.own_local_count);
        s.own_local_count += 1;
    }

    // Phase B: assign upvals and capture lists (parents before children).
    for i in (0..n).rev() {
        let parent = scopes[i].parent;
        if parent == usize::MAX {
            scopes[i].upval_count = 0;
            continue;
        }
        let self_name = scopes[i].self_name.clone();
        let frees: Vec<String> = scopes[i].free_vars.iter().cloned().collect();
        for fv in frees {
            if self_name.as_deref() == Some(fv.as_str()) {
                continue;
            }
            let parent_nparams = scopes[parent].params.len() as u32;
            // Const resolution (nearest-first): the parent's own const, or a const
            // the parent itself resolved to (transitive capture-of-a-const). Such a
            // free var becomes a const ref, never a capture.
            if let Some(value) = scopes[parent]
                .const_names
                .get(&fv)
                .or_else(|| scopes[parent].const_by_name.get(&fv))
                .cloned()
            {
                scopes[i].const_by_name.insert(fv, value);
                continue;
            }
            let (parent_abs, is_const) = if let Some(info) = scopes[parent].names.get(&fv).copied()
            {
                scopes[parent].captured.insert(info.slot);
                (
                    super::scope::frame_abs(info.slot, parent_nparams, scopes[parent].upval_count),
                    info.is_const,
                )
            } else if let Some(&(idx, is_const)) = scopes[parent].upval_by_name.get(&fv) {
                // Capturing one of the parent's own upvals: its absolute slot is
                // `parent_nparams + idx` under the [params | upvals | locals] layout.
                (parent_nparams + idx, is_const)
            } else {
                // Not declared in any ancestor: a global/`input`/undeclared
                // name — not an upval.
                continue;
            };
            let idx = scopes[i].captures.len() as u32;
            scopes[i].captures.push(parent_abs);
            scopes[i].upval_by_name.insert(fv, (idx, is_const));
        }
        scopes[i].upval_count = scopes[i].captures.len() as u32;
    }

    // Own-local slot kinds. A slot captured by some descendant is normally
    // `Boxed` (one eager cell, shared by reference). But a captured slot that is
    // also loop-declared (`let`/`const` in a loop head/body) gets a *fresh* cell
    // each iteration via `FreshCell`, so it needs no eager cell — it is allocated
    // `Plain` and recorded in `fresh_owns` for the compiler to drive `FreshCell`.
    for s in scopes.iter_mut() {
        let mut fresh = HashSet::new();
        s.slot_kinds = (0..s.own_local_count)
            .map(|slot| {
                if s.captured.contains(&slot) {
                    if s.loop_declared.contains(&slot) {
                        fresh.insert(slot);
                        SlotKind::Plain
                    } else {
                        SlotKind::Boxed
                    }
                } else {
                    SlotKind::Plain
                }
            })
            .collect();
        s.fresh_owns = fresh;
    }
}

/// Convert the per-scope, span-keyed records (own-slot relative) into the
/// absolute-slot tables codegen consults. Runs after `resolve_captures` has
/// fixed every `upval_count`.
#[allow(clippy::type_complexity)]
pub(crate) fn finalize_tables(
    scopes: &[FuncScope],
    const_fns: &HashSet<usize>,
) -> (
    HashMap<u32, u32>,
    HashMap<u32, RefSlot>,
    HashMap<u32, bool>,
    HashMap<u32, bool>,
    HashMap<u32, super::const_fns::ConstValue>,
    HashMap<u32, usize>,
) {
    let mut binding_slot = HashMap::new();
    let mut ref_resolution = HashMap::new();
    let mut binding_immutable = HashMap::new();
    let mut binding_captured = HashMap::new();
    let mut const_refs = HashMap::new();
    let mut scope_by_span = HashMap::new();
    for s in scopes {
        if s.node_span != u32::MAX {
            scope_by_span.insert(s.node_span, s.id);
        }
        let nparams = s.params.len() as u32;
        // A binding/own-local is immutable in fact when it is `const`, or it is
        // never reassigned in its scope AND never captured by a nested function
        // (a closure-mutated binding is necessarily captured, so this catches it).
        let own_immutable = |own: u32, is_const: bool| {
            is_const || (!s.reassigned.contains(&own) && !s.captured.contains(&own))
        };
        for &(span, own, is_const) in &s.binding_spans {
            binding_slot.insert(span, super::scope::frame_abs(own, nparams, s.upval_count));
            binding_immutable.insert(span, own_immutable(own, is_const));
            binding_captured.insert(span, s.captured.contains(&own));
        }
        for &(span, own, is_const) in &s.local_refs {
            // A same-scope reference to a constant function (Phase F) emits the
            // `Fn` literal — no slot read.
            if let Some(value) = s.const_fn_slots.get(&own) {
                const_refs.insert(span, value.clone());
                continue;
            }
            ref_resolution.insert(
                span,
                RefSlot {
                    slot: super::scope::frame_abs(own, nparams, s.upval_count),
                    is_const,
                    immutable: own_immutable(own, is_const),
                },
            );
        }
        // Const references resolved within this scope.
        for (span, value) in &s.const_refs {
            const_refs.insert(*span, value.clone());
        }
        for (span, name) in &s.free_refs {
            if s.self_name.as_deref() == Some(name.as_str()) {
                // Self-reference. A constant function refers to *itself* by its own
                // `Fn(label)` constant (static self-recursion, no self-slot);
                // otherwise the dedicated self-slot past all own locals.
                if const_fns.contains(&s.id) {
                    const_refs.insert(
                        *span,
                        super::const_fns::ConstValue::Fn {
                            label: s.label,
                            arity: s.declared_arity(),
                            js_length: s.js_length(),
                        },
                    );
                } else {
                    ref_resolution.insert(
                        *span,
                        RefSlot {
                            slot: super::scope::frame_abs(
                                s.own_local_count,
                                nparams,
                                s.upval_count,
                            ),
                            is_const: true,
                            immutable: true,
                        },
                    );
                }
            } else if let Some(value) = s.const_by_name.get(name) {
                // A free var that resolved (up the chain) to an enclosing const:
                // emit the literal — no upval, no capture.
                const_refs.insert(*span, value.clone());
            } else if let Some(&(idx, is_const)) = s.upval_by_name.get(name) {
                // The body's own upvals occupy slots [nparams, nparams + K). An
                // upval's immutability follows the captured binding's const-ness
                // (effectively-const lets are not propagated across capture).
                ref_resolution.insert(
                    *span,
                    RefSlot {
                        slot: nparams + idx,
                        is_const,
                        immutable: is_const,
                    },
                );
            }
            // Otherwise a global/`input`/undeclared name: leave absent so codegen
            // falls back to name-based resolution.
        }
    }
    (
        binding_slot,
        ref_resolution,
        binding_immutable,
        binding_captured,
        const_refs,
        scope_by_span,
    )
}
