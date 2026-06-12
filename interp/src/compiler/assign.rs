use oxc_ast::ast;
use oxc_span::GetSpan;

use crate::builtin::Builtin;
use crate::vm::{Instr, LocalIndex, RcStr, SetMode};

impl<'src> super::Compiler<'src> {
    /// Assignment is an expression: when `value_needed` is true, it leaves the
    /// assigned value on the stack. In void context (`value_needed == false`),
    /// the value is either consumed by `SetLocal` (for locals) or popped after
    /// `ObjSet`/`IndexSet`. Plain `=`, compound (`+=` …), and short-circuiting
    /// logical (`&&=`/`||=`/`??=`) assignment all share the [`LValue`] read/write
    /// lowering. Array/object destructuring targets are handled separately.
    pub(super) fn compile_assignment(&mut self, a: &ast::AssignmentExpression, value_needed: bool) {
        use ast::AssignmentOperator as Op;
        let span = a.span.start;

        // Destructuring assignment (`[a, b] = …`, `({a} = …)`). These leave the
        // RHS value as the expression result, so keep an extra copy.
        match &a.left {
            ast::AssignmentTarget::ArrayAssignmentTarget(_)
            | ast::AssignmentTarget::ObjectAssignmentTarget(_) => {
                if a.operator != Op::Assign {
                    self.error(span, "destructuring targets only allow plain `=`");
                    return;
                }
                self.compile_expr(&a.right);
                if value_needed {
                    self.emit(Instr::Pick(0), span); // one copy is the expression result
                }
                self.destructure_assign(&a.left, span);
                if !value_needed {
                    // destructure_assign consumers the source; no value left
                }
                return;
            }
            _ => {}
        }

        let lv = match self.lvalue_from_target(&a.left) {
            Some(lv) => lv,
            None => return,
        };

        match a.operator {
            Op::Assign => {
                self.lvalue_emit_addr(&lv, span);
                self.compile_expr(&a.right);
                if value_needed {
                    self.lvalue_emit_store(&lv, span);
                } else {
                    self.lvalue_emit_store_void(&lv, span);
                }
            }
            Op::LogicalAnd | Op::LogicalOr | Op::LogicalNullish => {
                self.compile_logical_assign(&lv, a.operator, &a.right, span, value_needed);
            }
            _ => {
                let op = match self.compound_binary_instr(a.operator, span) {
                    Some(op) => op,
                    None => return,
                };
                self.lvalue_emit_addr(&lv, span);
                self.lvalue_emit_load(&lv, span);
                self.compile_expr(&a.right);
                self.emit(op, span);
                if value_needed {
                    self.lvalue_emit_store(&lv, span);
                } else {
                    self.lvalue_emit_store_void(&lv, span);
                }
            }
        }
    }

    /// Map a compound assignment operator to its binary instruction. (`=` and
    /// the logical operators are handled by their own paths.)
    pub(super) fn compound_binary_instr(
        &mut self,
        op: ast::AssignmentOperator,
        _span: u32,
    ) -> Option<Instr> {
        use ast::AssignmentOperator as Op;
        Some(match op {
            Op::Addition => Instr::Add,
            Op::Subtraction => Instr::Sub,
            Op::Multiplication => Instr::Mul,
            Op::Division => Instr::Div,
            Op::Remainder => Instr::Mod,
            Op::Exponential => Instr::Pow,
            Op::ShiftLeft => Instr::BitLhs,
            Op::ShiftRight => Instr::BitRhs,
            Op::BitwiseOR => Instr::BitOr,
            Op::BitwiseXOR => Instr::BitXor,
            Op::BitwiseAnd => Instr::BitAnd,
            Op::ShiftRightZeroFill => Instr::BitURhs,
            Op::Assign | Op::LogicalAnd | Op::LogicalOr | Op::LogicalNullish => {
                unreachable!("handled by dedicated paths")
            }
        })
    }

    /// Short-circuiting logical assignment: `x &&= v` ≡ `x && (x = v)`,
    /// `x ||= v` ≡ `x || (x = v)`, `x ??= v` ≡ `x ?? (x = v)`. The RHS — and the
    /// store — run only on the non-short-circuit path; the lvalue's address is
    /// evaluated once. When `value_needed` is true, leaves the resulting value
    /// (old on short-circuit, else v); in void context discards it.
    pub(super) fn compile_logical_assign(
        &mut self,
        lv: &super::LValue<'_, '_>,
        op: ast::AssignmentOperator,
        rhs: &ast::Expression,
        span: u32,
        value_needed: bool,
    ) {
        use ast::AssignmentOperator as Op;
        let keep = self.new_label();
        let end = self.new_label();
        let depth = self.lvalue_addr_depth(lv);
        self.lvalue_emit_addr(lv, span);
        self.lvalue_emit_load(lv, span); // [addr…, old]
        match op {
            // `JNotNullish` pops `old` on the nullish fall-through, so the
            // store path starts with a clean stack and needs no explicit Pop.
            Op::LogicalNullish => self.emit(Instr::JNotNullish(keep), span),
            Op::LogicalAnd => {
                // For `&&=`, need a copy of `old` to test truthiness without
                // consuming it (the keep path needs it). In void context we
                // can just peek (JFalse pops, but we'd lose old). We always
                // Pick(0) (formerly Dup) since the keep path or store path consumes `old`.
                self.emit(Instr::Pick(0), span);
                self.emit(Instr::JFalse(keep), span); // falsy → keep old
                self.emit(Instr::Pop(1), span); // store path: discard old
            }
            Op::LogicalOr => {
                self.emit(Instr::Pick(0), span);
                self.emit(Instr::JTrue(keep), span); // truthy → keep old
                self.emit(Instr::Pop(1), span); // store path: discard old
            }
            _ => unreachable!("only logical operators reach here"),
        }
        // Store path: evaluate the RHS, store it.
        self.compile_expr(rhs);
        if value_needed {
            self.lvalue_emit_store(lv, span);
        } else {
            self.lvalue_emit_store_void(lv, span);
        }
        self.emit(Instr::Jump(end), span);
        // Keep path: old is on top, above any address values — drop those.
        self.emit(Instr::Label(keep), span);
        if value_needed {
            self.emit_drop_below_top(depth, span);
        } else {
            // Void: discard EVERYTHING (old + address operands).
            self.emit(Instr::Pop(1 + depth), span);
        }
        self.emit(Instr::Label(end), span);
    }

    /// `++x` / `x++` / `--x` / `x--`. Numeric (forces `ToNumber` via `Sub`): the
    /// new value is `old − p` where `p = -1` for `++` and `+1` for `--`. Prefix
    /// leaves the new value; postfix leaves the old value. For locals, `IncLocal`
    /// handles prefix/postfix in one instruction. For non-locals, `ObjSet`/
    /// `IndexSet` in `SetMode::Old` preserves the exact old value.
    pub(super) fn compile_update(&mut self, u: &ast::UpdateExpression, value_needed: bool) {
        let span = u.span.start;
        let lv = match self.lvalue_from_simple_target(&u.argument) {
            Some(lv) => lv,
            None => return,
        };
        // `p`: ++ subtracts -1 (i.e. adds 1); -- subtracts +1.
        let p = match u.operator {
            ast::UpdateOperator::Increment => -1.0,
            ast::UpdateOperator::Decrement => 1.0,
        };

        // Fast path for local variables: use `IncLocal` (1 instruction) when
        // the value is needed, or load-sub-store when void.
        if let super::LValue::Local(slot) = &lv {
            if value_needed {
                let mode = if u.prefix {
                    crate::vm::UpdateMode::Prefix
                } else {
                    crate::vm::UpdateMode::Postfix
                };
                self.emit(Instr::IncLocal(*slot as u16, p, mode), span);
            } else {
                // Void: `IncLocal` (in-place update) then drop its result. The
                // update mode is irrelevant since the pushed value is popped.
                self.emit(
                    Instr::IncLocal(*slot as u16, p, crate::vm::UpdateMode::Postfix),
                    span,
                );
                self.emit(Instr::Pop(1), span);
            }
            return;
        }

        // Non-local targets (member/index): load-sub-store path.
        self.lvalue_emit_addr(&lv, span);
        self.lvalue_emit_load(&lv, span);
        self.emit(Instr::PushFloat(p), span);
        self.emit(Instr::Sub, span);
        if value_needed {
            let mode = if u.prefix { SetMode::New } else { SetMode::Old };
            match &lv {
                super::LValue::Member(_, field) => {
                    self.emit(Instr::ObjSet(RcStr::from(field.as_str()), mode), span);
                }
                super::LValue::Index(..) => {
                    self.emit(Instr::IndexSet(mode), span);
                }
                super::LValue::Local(_) => unreachable!("handled above"),
            }
        } else {
            self.lvalue_emit_store_void(&lv, span);
        }
    }

    // ── lvalue infrastructure ────────────────────────────────────────────

    /// Resolve a (non-destructuring) assignment target to an [`LValue`], or emit
    /// an error and return `None`. A `const`/`state` write is rejected here.
    pub(super) fn lvalue_from_target<'r, 'a>(
        &mut self,
        target: &'r ast::AssignmentTarget<'a>,
    ) -> Option<super::LValue<'r, 'a>> {
        match target {
            ast::AssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.lvalue_for_identifier(id.name.as_str(), id.span.start)
            }
            ast::AssignmentTarget::StaticMemberExpression(m) => Some(super::LValue::Member(
                &m.object,
                m.property.name.as_str().to_string(),
            )),
            ast::AssignmentTarget::ComputedMemberExpression(m) => {
                Some(super::LValue::Index(&m.object, &m.expression))
            }
            other => {
                self.error(other.span().start, "unsupported assignment target");
                None
            }
        }
    }

    /// Like [`lvalue_from_target`], but for the `SimpleAssignmentTarget` of an
    /// update expression (`++`/`--`).
    pub(super) fn lvalue_from_simple_target<'r, 'a>(
        &mut self,
        target: &'r ast::SimpleAssignmentTarget<'a>,
    ) -> Option<super::LValue<'r, 'a>> {
        match target {
            ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.lvalue_for_identifier(id.name.as_str(), id.span.start)
            }
            ast::SimpleAssignmentTarget::StaticMemberExpression(m) => Some(super::LValue::Member(
                &m.object,
                m.property.name.as_str().to_string(),
            )),
            ast::SimpleAssignmentTarget::ComputedMemberExpression(m) => {
                Some(super::LValue::Index(&m.object, &m.expression))
            }
            other => {
                self.error(other.span().start, "unsupported assignment target");
                None
            }
        }
    }

    /// Resolve an identifier write target: a local slot, or an error for
    /// `const`/`state`/undeclared names.
    pub(super) fn lvalue_for_identifier<'r, 'a>(
        &mut self,
        name: &str,
        span: u32,
    ) -> Option<super::LValue<'r, 'a>> {
        // An eliminated `const` (Phase E) has no slot; a write to it is still a
        // constant reassignment error.
        if self.const_ref(span).is_some() {
            self.error(span, format!("assignment to constant `{name}`"));
            return None;
        }
        match self.ref_slot(span) {
            Some(r) => {
                if r.is_const {
                    self.error(span, format!("assignment to constant `{name}`"));
                }
                Some(super::LValue::Local(r.slot))
            }
            None if name == "input" => {
                self.error(span, "cannot reassign `input` (it is a host-seeded const)");
                None
            }
            None => {
                self.error(span, format!("assignment to undeclared variable `{name}`"));
                None
            }
        }
    }

    /// Number of address values an lvalue pushes before its value (`Local` 0,
    /// `Member` 1, `Index` 2). Used to clean up after a short-circuit.
    pub(super) fn lvalue_addr_depth(&self, lv: &super::LValue<'_, '_>) -> usize {
        match lv {
            super::LValue::Local(_) => 0,
            super::LValue::Member(..) => 1,
            super::LValue::Index(..) => 2,
        }
    }

    /// Push the lvalue's address operands (the object, and key for an index) in
    /// JS evaluation order. A local has no address.
    pub(super) fn lvalue_emit_addr(&mut self, lv: &super::LValue<'_, '_>, _span: u32) {
        match lv {
            super::LValue::Local(_) => {}
            super::LValue::Member(obj, _) => self.compile_expr(obj),
            super::LValue::Index(obj, key) => {
                self.compile_expr(obj);
                self.compile_expr(key);
            }
        }
    }

    /// With the address already on the stack, push the lvalue's current value
    /// **without** consuming the address (so a store can follow). Uses `Pick` to
    /// copy the buried object/key for the read.
    pub(super) fn lvalue_emit_load(&mut self, lv: &super::LValue<'_, '_>, span: u32) {
        match lv {
            super::LValue::Local(slot) => self.emit(Instr::Local(*slot as LocalIndex), span),
            super::LValue::Member(_, field) => {
                self.emit(Instr::Pick(0), span); // copy the object
                self.emit(Instr::ObjGet(RcStr::from(field.as_str())), span);
            }
            super::LValue::Index(..) => {
                self.emit(Instr::Pick(1), span); // copy the object
                self.emit(Instr::Pick(1), span); // copy the key
                self.emit(Instr::IndexGet, span);
            }
        }
    }

    /// With `[address…, value]` on the stack, store `value` into the lvalue and
    /// leave it on the stack (assignment is an expression). For locals, uses
    /// `TeeLocal` (the one-instruction equivalent of `Pick(0); SetLocal`, formerly `Dup; SetLocal`).
    pub(super) fn lvalue_emit_store(&mut self, lv: &super::LValue<'_, '_>, span: u32) {
        match lv {
            super::LValue::Local(slot) => {
                self.emit(Instr::TeeLocal(*slot as LocalIndex), span);
            }
            super::LValue::Member(_, field) => self.emit(
                Instr::ObjSet(RcStr::from(field.as_str()), SetMode::New),
                span,
            ),
            super::LValue::Index(..) => self.emit(Instr::IndexSet(SetMode::New), span),
        }
    }

    /// Like [`lvalue_emit_store`], but for void context (the caller does NOT
    /// need the resulting value). For locals, uses plain `SetLocal` (consumes
    /// the value, pushing nothing). For non-locals, `ObjSet`/`IndexSet` always
    /// leave the value — emit a `Pop(1)` to discard it.
    pub(super) fn lvalue_emit_store_void(&mut self, lv: &super::LValue<'_, '_>, span: u32) {
        match lv {
            super::LValue::Local(slot) => {
                self.emit(Instr::SetLocal(*slot as LocalIndex), span);
            }
            super::LValue::Member(_, field) => {
                self.emit(
                    Instr::ObjSet(RcStr::from(field.as_str()), SetMode::New),
                    span,
                );
                self.emit(Instr::Pop(1), span);
            }
            super::LValue::Index(..) => {
                self.emit(Instr::IndexSet(SetMode::New), span);
                self.emit(Instr::Pop(1), span);
            }
        }
    }

    /// Remove `n` values sitting directly below the top of the stack, leaving the
    /// top in place. Uses `Nip(n)` (one instruction) rather than `Dig(1)`+`Pop` (formerly `Swap`+`Pop`)
    /// pairs.
    pub(super) fn emit_drop_below_top(&mut self, n: usize, span: u32) {
        if n > 0 {
            self.emit(Instr::Nip(n), span);
        }
    }

    // ── destructuring assignment ─────────────────────────────────────────

    /// Destructure the source value on top of the stack into an assignment
    /// pattern, **consuming** it. Leaves are existing assignment targets; Phase 2
    /// supports identifier leaves (member/index leaves and rest are errors).
    pub(super) fn destructure_assign(&mut self, target: &ast::AssignmentTarget, span: u32) {
        match target {
            ast::AssignmentTarget::ArrayAssignmentTarget(arr) => {
                for (i, el) in arr.elements.iter().enumerate() {
                    if let Some(el) = el {
                        self.emit(Instr::Pick(0), span);
                        self.emit(Instr::PushPosInt(i as u64), span);
                        self.emit(Instr::IndexGet, span);
                        self.assign_maybe_default(el, span);
                    }
                }
                if let Some(rest) = &arr.rest {
                    self.emit(Instr::Pick(0), span);
                    self.emit(Instr::PushPosInt(arr.elements.len() as u64), span);
                    self.emit(Instr::CallBuiltin(Builtin::StrSlice, 2), span);
                    self.assign_target_leaf(&rest.target, span);
                }
                self.emit(Instr::Pop(1), span);
            }
            ast::AssignmentTarget::ObjectAssignmentTarget(obj) => {
                if let Some(rest) = &obj.rest {
                    // Same copy-minus-keys lowering as the declaration form
                    // (see `destructure_binding`).
                    self.emit(Instr::ObjNew(Vec::new().into()), span);
                    self.emit(Instr::Pick(1), span);
                    self.emit(Instr::ObjExtend, span); // [src, rest]
                    for prop in &obj.properties {
                        self.emit(Instr::Pick(1), span); // [src, rest, src]
                        match prop {
                            ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(
                                p,
                            ) => {
                                self.emit(Instr::PushStr(p.binding.name.as_str().into()), span);
                                self.emit_rest_excluded_key_access(span);
                                if let Some(default) = &p.init {
                                    self.emit_default(default, span);
                                }
                                self.assign_to_identifier(
                                    p.binding.name.as_str(),
                                    p.binding.span.start,
                                    span,
                                );
                            }
                            ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                                self.emit_property_key_string(&p.name, p.computed, span);
                                self.emit_rest_excluded_key_access(span);
                                self.assign_maybe_default(&p.binding, span);
                            }
                        }
                    }
                    self.assign_target_leaf(&rest.target, span); // [src]
                } else {
                    for prop in &obj.properties {
                        match prop {
                            ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(
                                p,
                            ) => {
                                // Shorthand `{a}` / `{a = d}`: the key and the target
                                // are the same identifier.
                                self.emit(Instr::Pick(0), span);
                                self.emit(Instr::ObjGet(p.binding.name.as_str().into()), span);
                                if let Some(default) = &p.init {
                                    self.emit_default(default, span);
                                }
                                self.assign_to_identifier(
                                    p.binding.name.as_str(),
                                    p.binding.span.start,
                                    span,
                                );
                            }
                            ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                                self.emit(Instr::Pick(0), span);
                                self.emit_property_key_access(&p.name, p.computed, span);
                                self.assign_maybe_default(&p.binding, span);
                            }
                        }
                    }
                }
                self.emit(Instr::Pop(1), span);
            }
            other => self.error(other.span().start, "unsupported destructuring target"),
        }
    }

    /// Destructure-assign a single element, applying its default (if any) to the
    /// value already on top of the stack.
    pub(super) fn assign_maybe_default(
        &mut self,
        m: &ast::AssignmentTargetMaybeDefault,
        span: u32,
    ) {
        match m {
            ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(wd) => {
                self.emit_default(&wd.init, span);
                self.assign_target_leaf(&wd.binding, span);
            }
            other => {
                // Inherits the `AssignmentTarget` variants.
                if let Some(t) = other.as_assignment_target() {
                    self.assign_target_leaf(t, span);
                } else {
                    self.error(other.span().start, "unsupported destructuring target");
                }
            }
        }
    }

    /// Store the value on top of the stack into a destructuring leaf, consuming
    /// it. Identifier leaves lower to `SetLocal`; nested patterns recurse;
    /// member/index leaves are not supported in Phase 2.
    pub(super) fn assign_target_leaf(&mut self, target: &ast::AssignmentTarget, span: u32) {
        match target {
            ast::AssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.assign_to_identifier(id.name.as_str(), id.span.start, span)
            }
            ast::AssignmentTarget::ArrayAssignmentTarget(_)
            | ast::AssignmentTarget::ObjectAssignmentTarget(_) => {
                self.destructure_assign(target, span)
            }
            other => self.error(
                other.span().start,
                "only variable targets are supported inside destructuring assignment",
            ),
        }
    }

    /// Store the value on top of the stack into a named local, consuming it.
    pub(super) fn assign_to_identifier(&mut self, name: &str, id_span: u32, span: u32) {
        // An eliminated `const` (Phase E) has no slot; reassigning it is an error.
        if self.const_ref(id_span).is_some() {
            self.error(id_span, format!("assignment to constant `{name}`"));
            self.emit(Instr::Pop(1), span);
            return;
        }
        match self.ref_slot(id_span) {
            Some(r) => {
                if r.is_const {
                    self.error(id_span, format!("assignment to constant `{name}`"));
                }
                self.emit(Instr::SetLocal(r.slot as LocalIndex), span);
            }
            None => {
                self.error(
                    id_span,
                    format!("assignment to undeclared variable `{name}`"),
                );
                self.emit(Instr::Pop(1), span);
            }
        }
    }
}
