use crate::analyzer::{ConstValue, RefSlot};
use crate::vm::{Instr, LocalIndex, Value};

impl super::Compiler {
    /// Absolute frame slot for a binding occurrence (keyed by its span). `None`
    /// only when the binding was rejected during analysis (e.g. shadowing
    /// `state`), in which case a diagnostic was already recorded.
    pub(super) fn binding_slot(&self, span: u32) -> Option<u32> {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .binding_slot
            .get(&span)
            .copied()
    }

    /// Whether the binding declared at `span` is immutable in fact (a `const`,
    /// or an un-reassigned, un-captured `let`/`var`) — so its constant
    /// initializer may be recorded for propagation. Defaults to `false`.
    pub(super) fn binding_immutable(&self, span: u32) -> bool {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .binding_immutable
            .get(&span)
            .copied()
            .unwrap_or(false)
    }

    /// Whether the binding declared at `span` is captured by a nested function.
    /// A non-captured binding's slot is read only by its own frame, so once every
    /// read is propagated its store is dead and can be dropped.
    pub(super) fn binding_captured(&self, span: u32) -> bool {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .binding_captured
            .get(&span)
            .copied()
            .unwrap_or(false)
    }

    /// The compile-time constant an identifier reference at `span` resolves to
    /// (a `const x = <literal>` binding — eliminated, no slot), if any.
    pub(super) fn const_ref(&self, span: u32) -> Option<ConstValue> {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .const_refs
            .get(&span)
            .cloned()
    }

    /// Whether the function scope `scope_id` is a constant function (Phase F):
    /// non-capturing and non-reassigned, so its binding store is dead.
    pub(super) fn is_const_fn_scope(&self, scope_id: usize) -> bool {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .const_fn_scopes
            .contains(&scope_id)
    }

    /// The push instruction that materializes a [`ConstValue`]. Numbers go
    /// through `f64_to_value` so the result matches the original literal exactly.
    pub(super) fn const_value_push(&mut self, v: &ConstValue) -> Instr {
        match v {
            ConstValue::Null => Instr::PushNull,
            ConstValue::Bool(b) => Instr::PushBool(*b),
            ConstValue::Str(s) => Instr::PushStr(self.intern_string(s)),
            ConstValue::Num(n) => match super::f64_to_value(*n) {
                Value::PosInt(u) => Instr::PushPosInt(u),
                Value::NegInt(i) => Instr::PushNegInt(i),
                Value::Float(f) => Instr::PushFloat(f),
                _ => unreachable!("f64_to_value yields an int or float"),
            },
            // A constant function (Phase F): its value is its code address,
            // with the canonical closure ptr patched during `for_program_with`.
            ConstValue::Fn {
                label, js_length, ..
            } => Instr::PushFn(*label, 0, *js_length),
        }
    }

    /// Resolution of an identifier *reference* (keyed by its span), or `None`
    /// when the name is not a local (a global / `state` / `undefined` / …, or
    /// undeclared) — the caller then falls back to name-based handling.
    pub(super) fn ref_slot(&self, span: u32) -> Option<RefSlot> {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .ref_resolution
            .get(&span)
            .copied()
    }

    /// Emit the read of a slot-resolved identifier reference.
    ///
    /// Constant propagation: an immutable binding (a `const`, or an
    /// effectively-const `let`/`var`) bound to a compile-time constant is
    /// read as the literal directly (which then composes with const-folding),
    /// rather than a `Local` load. This matters for correctness, not just
    /// speed: when every read is propagated, the binding's initializer store
    /// is dead-eliminated, so the slot is never written — a `Local` load
    /// would read an uninitialized (undefined) slot. Every consumer of a
    /// `RefSlot` read must go through here.
    pub(super) fn emit_slot_read(&mut self, r: &RefSlot, span: u32) {
        if r.immutable
            && let Some(push) = self.const_env.get(&r.slot)
        {
            self.emit(push.clone(), span);
            return;
        }
        self.emit(Instr::GetLocal(r.slot as LocalIndex), span);
    }

    /// The `scopes` index of the function/arrow defined at `span`.
    pub(super) fn scope_for_node(&self, span: u32) -> Option<usize> {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .scope_by_span
            .get(&span)
            .copied()
    }

    /// Whether the given absolute frame slot is a per-iteration "fresh" slot in
    /// the current scope — a captured (`let`/`const`) binding declared inside a
    /// loop, which gets a new cell each iteration via `FreshCell`. The analyzer
    /// records these in `fresh_owns` (own-slot indices) and allocates them
    /// `Plain` (no eager cell). `fresh_owns` is own-local-indexed (it excludes
    /// the leading upvals) while emitted slots are absolute (`upval_count + own`),
    /// so subtract the upval count first.
    pub(super) fn slot_needs_fresh(&self, slot: u32) -> bool {
        let scope = &self.analysis.as_ref().expect("analysis present").scopes[self.current_scope];
        match slot.checked_sub(scope.upval_count) {
            Some(own) => scope.fresh_owns.contains(&own),
            None => false,
        }
    }

    /// Emit `FreshCell(slot)` for a body declaration of a per-iteration binding.
    /// Gated on `self.loops` so the for-statement head (compiled in the `init`
    /// clause before the loop context is pushed) is excluded — `compile_for`
    /// re-boxes the head explicitly. Captured `var` bindings are function-scoped,
    /// never in `fresh_owns`, so they are correctly left shared.
    pub(super) fn fresh_cell_if_needed(&mut self, slot: u32, span: u32) {
        if !self.loops.is_empty() && self.slot_needs_fresh(slot) {
            self.emit(Instr::FreshCell(slot as LocalIndex), span);
        }
    }
}
