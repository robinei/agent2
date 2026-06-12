use std::collections::HashMap;

use oxc_ast::ast;
use oxc_span::GetSpan;

use crate::analyzer::frame_abs;
use crate::builtin::Builtin;
use crate::vm::{Instr, LocalIndex, SlotKind};

impl<'src> super::Compiler<'src> {
    /// Hoist function declarations in the current scope's prologue: emit each
    /// declaration's binding value (`Fn`/closure) into its slot. Recurses
    /// through blocks/conditionals/loops (function declarations hoist to the
    /// enclosing function), but not into nested functions.
    pub(super) fn hoist_function_decls(&mut self, stmts: &[ast::Statement]) {
        for stmt in stmts {
            self.hoist_function_decl_in_stmt(stmt);
        }
    }

    pub(super) fn hoist_function_decl_in_stmt(&mut self, stmt: &ast::Statement) {
        match stmt {
            ast::Statement::FunctionDeclaration(f) => {
                let Some(scope_id) = self.scope_for_node(f.span.start) else {
                    return;
                };
                let (label, captures) = {
                    let analysis = self.analysis.as_ref().expect("analysis present");
                    let child = &analysis.scopes[scope_id];
                    (child.label, child.captures.clone())
                };
                // A constant function (Phase F) has no live slot — its binding
                // store is dead (references/calls go through its `Fn` constant).
                if self.is_const_fn_scope(scope_id) {
                    return;
                }
                if let Some(id) = &f.id {
                    if let Some(slot) = self.binding_slot(id.span.start) {
                        let span = f.span.start;
                        if captures.is_empty() {
                            self.emit(Instr::PushFn(label), span);
                        } else {
                            self.emit(
                                Instr::MakeClosure(
                                    label,
                                    captures.iter().map(|&c| c as LocalIndex).collect(),
                                ),
                                span,
                            );
                        }
                        self.emit(Instr::SetLocal(slot as LocalIndex), span);
                    }
                }
            }
            ast::Statement::BlockStatement(b) => {
                for s in &b.body {
                    self.hoist_function_decl_in_stmt(s);
                }
            }
            ast::Statement::IfStatement(s) => {
                self.hoist_function_decl_in_stmt(&s.consequent);
                if let Some(alt) = &s.alternate {
                    self.hoist_function_decl_in_stmt(alt);
                }
            }
            ast::Statement::WhileStatement(s) => self.hoist_function_decl_in_stmt(&s.body),
            ast::Statement::DoWhileStatement(s) => self.hoist_function_decl_in_stmt(&s.body),
            ast::Statement::ForStatement(s) => self.hoist_function_decl_in_stmt(&s.body),
            ast::Statement::TryStatement(t) => {
                for s in &t.block.body {
                    self.hoist_function_decl_in_stmt(s);
                }
                if let Some(h) = &t.handler {
                    for s in &h.body.body {
                        self.hoist_function_decl_in_stmt(s);
                    }
                }
                if let Some(f) = &t.finalizer {
                    for s in &f.body {
                        self.hoist_function_decl_in_stmt(s);
                    }
                }
            }
            _ => {}
        }
    }

    /// Emit the body of a function declaration (its binding was already emitted
    /// in the prologue by `hoist_function_decls`).
    pub(super) fn compile_function_decl_body(&mut self, f: &ast::Function) {
        let Some(scope_id) = self.scope_for_node(f.span.start) else {
            self.error(
                f.span.start,
                "internal error: function declaration not found in analysis",
            );
            return;
        };
        if let Some(body) = &f.body {
            self.emit_function_def(scope_id, &body.statements, &f.params, f.span.start, false);
        }
    }

    /// Compile a function expression: emit the function value, then its body.
    pub(super) fn compile_function_expr(&mut self, func: &ast::Function, span: u32) {
        let Some(scope_id) = self.scope_for_node(func.span.start) else {
            self.error(
                span,
                "internal error: function expression not found in analysis",
            );
            return;
        };
        self.emit_closure_value(scope_id, span);
        if let Some(body) = &func.body {
            self.emit_function_def(scope_id, &body.statements, &func.params, span, false);
        }
    }

    /// Compile an arrow function expression.
    pub(super) fn compile_arrow_expr(&mut self, arrow: &ast::ArrowFunctionExpression, span: u32) {
        let Some(scope_id) = self.scope_for_node(arrow.span.start) else {
            self.error(span, "internal error: arrow function not found in analysis");
            return;
        };
        self.emit_closure_value(scope_id, span);
        // An arrow with an expression body returns that expression directly.
        let is_expression_body = arrow.expression;
        self.emit_function_def(
            scope_id,
            &arrow.body.statements,
            &arrow.params,
            span,
            is_expression_body,
        );
    }

    /// If `init` is a function expression that is a constant function (Phase F),
    /// emit only its body (the jump-over guards it) — no value push, no store,
    /// since references resolve to its `Fn` constant — and return `true`.
    pub(super) fn emit_const_fn_expr_body(&mut self, init: &ast::Expression) -> bool {
        let span = match init {
            ast::Expression::ArrowFunctionExpression(a) => a.span.start,
            ast::Expression::FunctionExpression(f) => f.span.start,
            _ => return false,
        };
        let Some(scope_id) = self.scope_for_node(span) else {
            return false;
        };
        if !self.is_const_fn_scope(scope_id) {
            return false;
        }
        match init {
            ast::Expression::ArrowFunctionExpression(a) => {
                self.emit_function_def(scope_id, &a.body.statements, &a.params, span, a.expression);
            }
            ast::Expression::FunctionExpression(f) => {
                if let Some(body) = &f.body {
                    self.emit_function_def(scope_id, &body.statements, &f.params, span, false);
                }
            }
            _ => unreachable!(),
        }
        true
    }

    /// Push a function value: a bare `Fn` when it captures nothing, else a
    /// `MakeClosure` over its capture list.
    pub(super) fn emit_closure_value(&mut self, scope_id: usize, span: u32) {
        let (label, captures) = {
            let analysis = self.analysis.as_ref().expect("analysis present");
            let child = &analysis.scopes[scope_id];
            (child.label, child.captures.clone())
        };
        if captures.is_empty() {
            self.emit(Instr::PushFn(label), span);
        } else {
            self.emit(
                Instr::MakeClosure(label, captures.iter().map(|&c| c as LocalIndex).collect()),
                span,
            );
        }
    }

    /// Emit a function body: jump-over guard, entry label, prologue
    /// (`EnterFrame`, param defaults / captured-param boxing, self-reference),
    /// body statements, implicit `Return`. Called after the function value has
    /// been pushed (expressions)
    /// or at the declaration site. `is_expression_body` (arrow `=> expr`)
    /// suppresses the trailing implicit `return undefined`. All slots/kinds come
    /// from analysis; there is no codegen-side scope state to set up.
    #[allow(clippy::too_many_lines)]
    pub(super) fn emit_function_def(
        &mut self,
        scope_id: usize,
        body_stmts: &[ast::Statement],
        params: &ast::FormalParameters,
        span: u32,
        is_expression_body: bool,
    ) {
        let (
            label,
            slot_kinds,
            params_info,
            self_name,
            upval_count,
            own_local_count,
            uses_arguments,
            captures,
        ) = {
            let analysis = self.analysis.as_ref().expect("analysis present");
            let scope = &analysis.scopes[scope_id];
            (
                scope.label,
                scope.slot_kinds.clone(),
                scope.params.clone(),
                scope.self_name.clone(),
                scope.upval_count,
                scope.own_local_count,
                scope.uses_arguments,
                scope.captures.clone(),
            )
        };
        let nparams = params_info.len() as u32;
        let has_rest = params_info.last().map(|p| p.is_rest).unwrap_or(false);
        let nregular = if has_rest { nparams - 1 } else { nparams };

        let prev_scope = self.current_scope;
        self.current_scope = scope_id;
        // Barriers don't cross function boundaries: a `return` in this body
        // must pop only THIS body's handlers, never the enclosing function's
        // (those belong to a different frame), and a body defined inside a
        // `finally` block is a fresh frame with no pending completion. The
        // spill slot is per-frame for the same reason.
        let prev_barriers = std::mem::take(&mut self.barriers);
        let prev_return_spill = self.return_spill.take();
        // Slot numbers are frame-relative, so the callee gets its own const env.
        // Phase D: seed it with constants captured *by value* — a `const`/
        // effectively-const capture is an immutable snapshot, so the upval holds a
        // fixed value. `captures[i]` is the parent slot; it installs at the child's
        // upval slot `nparams + i`, so a reference to that upval propagates the
        // literal inside the closure body.
        let prev_const_env = std::mem::take(&mut self.const_env);
        let mut child_const_env: HashMap<u32, Instr> = HashMap::new();
        for (i, &parent_slot) in captures.iter().enumerate() {
            if let Some(push) = prev_const_env.get(&parent_slot) {
                child_const_env.insert(nparams + i as u32, push.clone());
            }
        }
        self.const_env = child_const_env;

        // Jump over the body for sequential execution; `Call`/`CallDyn` enter at
        // the label below.
        let after = self.new_label();
        self.emit(Instr::Jump(after), span);
        self.emit(Instr::Label(label), span);

        // Prologue frame setup in one instruction: the args are already in place
        // as the leading locals [0, nparams) (so no per-param copy), upvals get
        // installed at [nparams, nparams+K), and the declared (non-param) own
        // locals — plus the self-reference slot, if any — are allocated from
        // their kinds. `slot_kinds[..nparams]` are the params (handled below);
        // `slot_kinds[nparams..]` are the declared locals.
        let mut local_kinds: Vec<SlotKind> = slot_kinds[nparams as usize..].to_vec();
        // The self-reference slot — except for a constant function, which refers
        // to itself by its `Fn` constant, so the slot would be dead.
        if self_name.is_some() && !self.is_const_fn_scope(scope_id) {
            local_kinds.push(SlotKind::Plain);
        }
        // The return spill slot would land just past everything; the
        // `EnterFrame` emitted here is patched post-body to allocate it,
        // only if some `return` crossing a `finally` actually used it.
        let enter_frame_at = self.code.len();
        self.return_spill = Some(super::ReturnSpill {
            slot: nparams + upval_count + local_kinds.len() as u32,
            enter_frame: Ok(enter_frame_at),
            used: false,
        });
        self.emit(
            Instr::EnterFrame(nparams as u16, uses_arguments, local_kinds.into()),
            span,
        );

        // Per-parameter prologue: apply defaults (the arg is already in the slot)
        // and box captured params in place. Plain params with no default need no
        // code — their value is already in the local slot.  Skip the rest param
        // (if any) — it is handled separately below.
        for (p_idx, param_info) in params_info.iter().enumerate() {
            if param_info.is_rest {
                continue;
            }
            let slot = p_idx as u32; // params occupy slots 0..nparams
            let item = &params.items[p_idx];
            if matches!(&item.pattern, ast::BindingPattern::BindingIdentifier(_)) {
                let needs_box = matches!(slot_kinds.get(p_idx).copied(), Some(SlotKind::Boxed));
                let default_expr = item.initializer.as_ref().map(|v| &**v);
                self.emit_param_setup(slot, needs_box, param_info.has_default, default_expr, span);
            } else {
                // Destructuring param: the argument sits in an anonymous slot
                // (never captured — its name is not a legal identifier). Load
                // it, apply the whole-pattern default, and run the normal
                // pattern lowering into the leaf bindings (own locals; captured
                // ones got their cells from `EnterFrame`, and `SetLocal`
                // writes through cells).
                let pat_span = item.span.start;
                self.emit(Instr::Local(slot as LocalIndex), pat_span);
                if let Some(default) = &item.initializer {
                    self.emit_default(default, pat_span);
                }
                self.destructure_binding(&item.pattern, pat_span);
            }
        }

        // Rest parameter: build the rest array from `arguments.slice(nregular)`.
        // `EnterFrame` eagerly built the arguments cache (see uses_arguments above)
        // from ALL caller args before truncating to nparams, so `arguments` always
        // holds the full argument list.  `arguments.slice(nregular)` gives the
        // surplus elements that become the rest array.
        if has_rest {
            let rest_slot = nregular as u32;
            self.emit(Instr::Arguments, span);
            self.emit(Instr::PushPosInt(nregular as u64), span);
            self.emit(Instr::CallBuiltin(Builtin::StrSlice, 2), span);
            let rest_pat = &params
                .rest
                .as_ref()
                .expect("has_rest implies rest")
                .rest
                .argument;
            if matches!(rest_pat, ast::BindingPattern::BindingIdentifier(_)) {
                let needs_box = matches!(
                    slot_kinds.get(rest_slot as usize).copied(),
                    Some(SlotKind::Boxed)
                );
                self.emit(Instr::SetLocal(rest_slot as LocalIndex), span);
                if needs_box {
                    self.emit(Instr::FreshCell(rest_slot as LocalIndex), span);
                }
            } else {
                // Pattern rest (`...[a, b]`): destructure the freshly built
                // array directly; the anonymous rest slot stays undefined.
                self.destructure_binding(rest_pat, rest_pat.span().start);
            }
        }

        // Self-reference (named function expression / recursive declaration): the
        // slot was allocated by EnterFrame above; fill it with the bare `Fn`.
        // A constant function (Phase F) refers to itself by its `Fn` constant
        // (static self-recursion), so the self-slot is dead — skip the setup.
        if self_name.is_some() && !self.is_const_fn_scope(scope_id) {
            let self_slot = frame_abs(own_local_count, nparams, upval_count);
            self.emit(Instr::PushFn(label), span);
            self.emit(Instr::SetLocal(self_slot as LocalIndex), span);
        }

        // Inner function declarations: emit their bindings in this prologue.
        self.hoist_function_decls(body_stmts);

        if is_expression_body && body_stmts.len() == 1 {
            if let ast::Statement::ExpressionStatement(es) = &body_stmts[0] {
                self.compile_expr(&es.expression);
                self.emit(Instr::Return(1), span);
            }
        } else {
            for stmt in body_stmts {
                self.compile_stmt(stmt);
            }
            self.emit(Instr::PushUndefined, span);
            self.emit(Instr::Return(1), span);
        }

        self.emit(Instr::Label(after), span);
        if let Some(spill) = self.return_spill.take() {
            self.finalize_return_spill(spill, span);
        }
        self.current_scope = prev_scope;
        self.const_env = prev_const_env;
        self.barriers = prev_barriers;
        self.return_spill = prev_return_spill;
    }

    /// Emit per-parameter prologue code. The argument value is already in the
    /// param's local `slot` (placed in-frame by `EnterFrame`), so:
    ///   - with a default: if the slot is `undefined`, replace it with the
    ///     default expression's value;
    ///   - if captured (`needs_box`): box the slot in place with `FreshCell`
    ///     (Plain value → fresh cell), so closures capture it by reference.
    /// A plain param with no default needs no code at all.
    pub(super) fn emit_param_setup(
        &mut self,
        slot: u32,
        needs_box: bool,
        has_default: bool,
        default_expr: Option<&ast::Expression>,
        span: u32,
    ) {
        if has_default {
            if let Some(default) = default_expr {
                // if Local(slot) === undefined { slot = default }
                let skip_default = self.new_label();
                self.emit(Instr::Local(slot as LocalIndex), span);
                self.emit(Instr::PushUndefined, span);
                self.emit(Instr::Eq, span);
                self.emit(Instr::JFalse(skip_default), span);
                self.compile_expr(default);
                self.emit(Instr::SetLocal(slot as LocalIndex), span);
                self.emit(Instr::Label(skip_default), span);
            }
        }
        if needs_box {
            // Promote the plain arg value in the slot to a shared cell.
            self.emit(Instr::FreshCell(slot as LocalIndex), span);
        }
    }
}
