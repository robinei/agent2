use std::collections::HashSet;

use indexmap::{IndexMap, IndexSet};
use oxc_ast::ast;

use super::scope::FuncScope;

/// A compile-time constant value bound by a `const x = <literal>` declaration.
/// Such bindings are *not* runtime variables: they occupy no frame slot, are
/// never captured, and every reference resolves to this value (the compiler
/// emits the corresponding push). Numbers are kept as `f64` and lowered through
/// the compiler's usual `f64_to_value`, so propagation matches what the literal
/// would have compiled to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ConstValue {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    /// A non-capturing, non-reassigned function: its value is a fixed code
    /// address (`Fn(label)`), so the binding is a compile-time constant — no
    /// slot, references emit `PushFn(label)`, calls are static `Call(label)`.
    /// `arity` is the declared parameter count (for static-call arg padding);
    /// `js_length` is JS `Function.prototype.length` (params before the first
    /// default/rest), baked into the `PushFn` for `fn.length` (Step 6).
    Fn {
        label: u32,
        arity: u32,
        js_length: u16,
    },
}

/// Recognize an initializer that is a compile-time constant *literal* (the
/// scope of constant-binding elimination, v1): literals and a unary minus on a
/// numeric literal. Returns the value, or `None` for anything that needs runtime
/// evaluation (a non-literal const still gets a slot and ordinary propagation).
pub(crate) fn literal_const_value(expr: &ast::Expression) -> Option<ConstValue> {
    match expr {
        ast::Expression::NumericLiteral(n) => Some(ConstValue::Num(n.value)),
        ast::Expression::StringLiteral(s) => Some(ConstValue::Str(s.value.to_string())),
        ast::Expression::BooleanLiteral(b) => Some(ConstValue::Bool(b.value)),
        ast::Expression::NullLiteral(_) => Some(ConstValue::Null),
        ast::Expression::UnaryExpression(u) if u.operator == ast::UnaryOperator::UnaryNegation => {
            match &u.argument {
                ast::Expression::NumericLiteral(n) => Some(ConstValue::Num(-n.value)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Phase F — identify **constant functions** and leave `scopes` fully capture-
/// resolved with them registered. A constant function is a function *declaration*
/// that is never reassigned and captures nothing once constant functions resolve
/// to their `Fn(label)` value (so mutually/self-recursive functions, whose only
/// "captures" are each other, qualify).
///
/// This is a greatest-fixpoint *over capture resolution itself* — which is what
/// makes it handle transitive captures correctly (a function that forwards a
/// captured variable down to a nested closure really does capture it). Start by
/// optimistically assuming every non-reassigned function declaration is constant;
/// register them (so references resolve to values, not captures); run
/// `resolve_captures`; **demote** any that still ended up with a real capture;
/// repeat until stable. Monotone (demote-only) ⇒ converges. The final iteration
/// leaves `scopes` with the correct captures/upvals for `finalize_tables`.
pub(crate) fn resolve_const_functions(scopes: &mut [FuncScope]) -> HashSet<usize> {
    // `resolve_captures` mutates `free_vars` (propagation) and the derived
    // capture fields; snapshot the inputs so each iteration starts clean.
    let direct_free: Vec<IndexSet<String>> = scopes.iter().map(|s| s.free_vars.clone()).collect();
    let direct_consts: Vec<IndexMap<String, ConstValue>> =
        scopes.iter().map(|s| s.const_names.clone()).collect();
    // Snapshot own_local_count — the reify pass in resolve_captures increments
    // it for the synthetic <this> slot; each fixpoint iteration must start from
    // the original count.
    let mut base_own_count: Vec<u32> = scopes.iter().map(|s| s.own_local_count).collect();

    let mut const_fns: HashSet<usize> = HashSet::new();
    for (id, s) in scopes.iter().enumerate() {
        if s.parent == usize::MAX {
            continue;
        }
        // The external binding: a declaration's own name, or a function
        // expression's `const NAME = …` binding. Either must be non-reassigned.
        let Some(name) = const_fn_binding_name(s) else {
            continue;
        };
        if let Some(info) = scopes[s.parent].names.get(name)
            && !scopes[s.parent].reassigned.contains(&info.slot)
        {
            const_fns.insert(id);
        }
    }

    loop {
        for (i, s) in scopes.iter_mut().enumerate() {
            s.free_vars = direct_free[i].clone();
            s.const_names = direct_consts[i].clone();
            s.const_fn_slots.clear();
            s.captured.clear();
            s.captures.clear();
            s.upval_by_name.clear();
            s.const_by_name.clear();
            s.upval_count = 0;
            s.slot_kinds.clear();
            s.fresh_owns.clear();
            s.own_local_count = base_own_count[i];
            s.this_slot = None;
            s.needs_this_reify = false;
            s.names.shift_remove("<this>");
        }
        register_const_fns(scopes, &const_fns);
        super::captures::resolve_captures(scopes);
        let demoted: Vec<usize> = const_fns
            .iter()
            .copied()
            .filter(|&f| !scopes[f].captures.is_empty())
            .collect();
        if demoted.is_empty() {
            break;
        }
        for f in demoted {
            const_fns.remove(&f);
        }
    }

    // Reclaim the dead const-function slots, then re-resolve captures against the
    // compacted slot numbers (the fixpoint above left `const_fn_slots` populated).
    compact_const_fn_slots(scopes);
    for (i, s) in scopes.iter_mut().enumerate() {
        s.free_vars = direct_free[i].clone();
        s.const_names = direct_consts[i].clone();
        s.captured.clear();
        s.captures.clear();
        s.upval_by_name.clear();
        s.const_by_name.clear();
        s.upval_count = 0;
        s.slot_kinds.clear();
        s.fresh_owns.clear();
        // After compact, own_local_count may have shrunk; use the new
        // (compacted) value as the base for the final resolve_captures.
        base_own_count[i] = s.own_local_count;
        s.this_slot = None;
        s.needs_this_reify = false;
        s.names.shift_remove("<this>");
    }
    register_const_fns(scopes, &const_fns);
    super::captures::resolve_captures(scopes);
    const_fns
}

/// The external binding name of a constant-function candidate: a declaration's
/// own name, or a function expression's `const NAME = …` binding. (A named
/// function expression's `self_name` is its *internal* recursion name, distinct
/// from its external `const` binding.)
fn const_fn_binding_name(s: &FuncScope) -> Option<&String> {
    if s.is_declaration {
        s.self_name.as_ref()
    } else {
        s.binding_name.as_ref()
    }
}

/// Reclaim the (now dead) frame slots of constant functions: each scope's
/// const-function binding slots are removed and the surviving own-locals are
/// renumbered down, so a constant function is truly zero-cost (no reserved
/// slot). Same-scope references to a const function are rewritten to its `Fn`
/// constant here; cross-scope ones already resolved via `const_names`. Run after
/// the fixpoint, before the final `resolve_captures` (which recomputes captures
/// against the compacted slot numbers).
fn compact_const_fn_slots(scopes: &mut [FuncScope]) {
    for s in scopes.iter_mut() {
        if s.const_fn_slots.is_empty() {
            continue;
        }
        let mut dead: Vec<u32> = s.const_fn_slots.keys().copied().collect();
        dead.sort_unstable();
        // Surviving own-slot → compacted index (shifted down past dead slots).
        let remap = |slot: u32| -> Option<u32> {
            if dead.binary_search(&slot).is_ok() {
                None
            } else {
                Some(slot - dead.iter().filter(|&&d| d < slot).count() as u32)
            }
        };

        // Same-scope refs to a const function become its `Fn` constant; survivors
        // keep their (renumbered) slot.
        let mut kept = Vec::with_capacity(s.local_refs.len());
        for (span, slot, is_const) in std::mem::take(&mut s.local_refs) {
            if let Some(val) = s.const_fn_slots.get(&slot) {
                s.const_refs.push((span, val.clone()));
            } else {
                kept.push((
                    span,
                    remap(slot).expect("non-const-fn slot survives"),
                    is_const,
                ));
            }
        }
        s.local_refs = kept;

        s.names.retain(|_, info| match remap(info.slot) {
            Some(n) => {
                info.slot = n;
                true
            }
            None => false,
        });
        s.binding_spans = std::mem::take(&mut s.binding_spans)
            .into_iter()
            .filter_map(|(span, slot, is_const)| remap(slot).map(|n| (span, n, is_const)))
            .collect();
        let mut slot_names: Vec<Option<String>> = Vec::with_capacity(s.own_slot_names.len());
        for (slot, name) in std::mem::take(&mut s.own_slot_names)
            .into_iter()
            .enumerate()
        {
            if let Some(n) = remap(slot as u32) {
                let n = n as usize;
                if slot_names.len() <= n {
                    slot_names.resize(n + 1, None);
                }
                slot_names[n] = name;
            }
        }
        s.own_slot_names = slot_names;
        s.reassigned = s.reassigned.iter().filter_map(|&sl| remap(sl)).collect();
        s.loop_declared = s.loop_declared.iter().filter_map(|&sl| remap(sl)).collect();
        s.own_local_count -= dead.len() as u32;
        s.this_slot = s.this_slot.and_then(remap);
        s.const_fn_slots.clear();
    }
}

/// Register each constant function as an `Fn` constant in its enclosing scope:
/// by name in `const_names` (so `resolve_captures` resolves references to it as
/// a value, never a capture) and by slot in `const_fn_slots` (so a same-scope
/// reference emits the literal). Run before `resolve_captures`.
pub(crate) fn register_const_fns(scopes: &mut [FuncScope], const_fns: &HashSet<usize>) {
    for &sf in const_fns {
        let parent = scopes[sf].parent;
        let Some(name) = const_fn_binding_name(&scopes[sf]).cloned() else {
            continue;
        };
        let val = ConstValue::Fn {
            label: scopes[sf].label,
            arity: scopes[sf].declared_arity(),
            js_length: scopes[sf].js_length(),
        };
        let slot = scopes[parent].names.get(&name).map(|i| i.slot);
        scopes[parent]
            .const_names
            .entry(name)
            .or_insert(val.clone());
        if let Some(slot) = slot {
            scopes[parent].const_fn_slots.insert(slot, val);
        }
    }
}
