use oxc_ast::ast;
use oxc_span::GetSpan;

use crate::vm::{Instr, LocalIndex};

impl<'src> super::Compiler<'src> {
    /// The whole program is the root frame's body. Scope/capture analysis ran
    /// already (in `self.analysis`), so the prologue `EnterFrame` slot kinds and all
    /// binding/reference/function resolutions are precomputed and looked up by
    /// span. Function bodies are emitted inline (guarded by a jump-over) at
    /// their definition sites.
    pub(super) fn compile_program(&mut self, program: &ast::Program) {
        let analysis = self
            .analysis
            .as_ref()
            .expect("analysis must run before codegen");
        let root = &analysis.scopes[analysis.root];
        self.current_scope = analysis.root;

        // Prologue. The root frame has no params and no upvals, so `EnterFrame`
        // just allocates its locals (and eagerly builds top-level `arguments` if
        // referenced). Skip it entirely when there is nothing to set up.
        let enter_frame_at = self.code.len();
        let root_slot_count = root.slot_kinds.len() as u32;
        let emitted_enter_frame = !root.slot_kinds.is_empty() || root.uses_arguments;
        let root_this_slot = root.this_slot;
        if emitted_enter_frame {
            let kinds = root.slot_kinds.clone();
            let uses_arguments = root.uses_arguments;
            self.emit(
                Instr::EnterFrame(0, uses_arguments, kinds.into()),
                program.span.start,
            );
        }
        self.return_spill = Some(super::ReturnSpill {
            slot: root_slot_count,
            enter_frame: if emitted_enter_frame {
                Ok(enter_frame_at)
            } else {
                Err(enter_frame_at)
            },
            used: false,
        });

        // Reify `this` for arrow capture at top level: copy the root frame's
        // `this_val` (undefined) into a captured Boxed local so top-level arrows
        // can capture it through the standard upval path.
        if let Some(ts) = root_this_slot {
            self.emit(Instr::LoadThis, program.span.start);
            self.emit(Instr::SetLocal(ts as LocalIndex), program.span.start);
        }

        // Hoist function declarations into the prologue (emit their bindings).
        self.hoist_function_decls(&program.body);

        // Compile top-level body statements.
        for stmt in &program.body {
            self.compile_stmt(stmt);
        }

        // Root frame ends with Return(0) → StepResult::Done.
        self.emit(Instr::Return(0), program.span.end);
        if let Some(spill) = self.return_spill.take() {
            self.finalize_return_spill(spill, program.span.start);
        }
    }

    pub(super) fn compile_stmt(&mut self, stmt: &ast::Statement) {
        match stmt {
            // Every expression statement leaves one value, popped to keep the
            // stack-discipline invariant (one value per expression). For
            // assignments and updates targeting locals, we lower directly in a
            // "value-not-needed" mode, skipping the wasted Pick(0)/Pop pair (formerly Dup/Pop).
            ast::Statement::ExpressionStatement(es) => match &es.expression {
                ast::Expression::AssignmentExpression(a) => {
                    self.compile_assignment(a, false);
                }
                ast::Expression::UpdateExpression(u) => {
                    self.compile_update(u, false);
                }
                _ => {
                    self.compile_expr(&es.expression);
                    self.emit(Instr::Pop(1), es.span.start);
                }
            },
            ast::Statement::VariableDeclaration(decl) => self.compile_var_decl(decl),
            ast::Statement::BlockStatement(block) => {
                // Block scoping was resolved by analysis (slots are function-wide
                // and per-reference resolution is span-keyed), so a block is just
                // its statements — no per-block bookkeeping here.
                for s in &block.body {
                    self.compile_stmt(s);
                }
            }
            ast::Statement::EmptyStatement(_) => {}
            ast::Statement::IfStatement(s) => self.compile_if(s),
            ast::Statement::WhileStatement(s) => self.compile_while(s),
            ast::Statement::DoWhileStatement(s) => self.compile_do_while(s),
            ast::Statement::ForStatement(s) => self.compile_for(s),
            ast::Statement::BreakStatement(s) => self.compile_break(s),
            ast::Statement::ContinueStatement(s) => self.compile_continue(s),

            // ── Phase 3: functions / return ───────────────────────────
            ast::Statement::FunctionDeclaration(f) => {
                // The binding was already hoisted in the prologue by
                // `hoist_function_decls`. Now emit the function body.
                self.compile_function_decl_body(f);
            }
            ast::Statement::ReturnStatement(r) => {
                let analysis = self
                    .analysis
                    .as_ref()
                    .expect("analysis present during codegen");
                // Top-level `return` is allowed (8_HARNESS Step 0); the root
                // frame's Return pops it and yields Done { value }.
                let _is_top_level = analysis.scopes[self.current_scope].parent == usize::MAX;
                match &r.argument {
                    Some(expr) => self.compile_expr(expr),
                    None => self.emit(Instr::PushUndefined, r.span.start),
                }
                self.emit_return(r.span.start);
            }

            ast::Statement::ForOfStatement(s) => self.compile_for_of(s),
            ast::Statement::ForInStatement(s) => self.compile_for_in(s),

            ast::Statement::SwitchStatement(s) => self.compile_switch(s),

            // ── Phase 6B: exceptions ──────────────────────────────────
            ast::Statement::ThrowStatement(s) => {
                self.compile_expr(&s.argument);
                self.emit(Instr::Throw, s.span.start);
            }
            ast::Statement::TryStatement(s) => self.compile_try(s),

            // Phase 13 Step 7a — plain class (constructor + methods + fields).
            ast::Statement::ClassDeclaration(c) => self.compile_class_decl(c),

            // Out of scope — informative errors.
            ast::Statement::LabeledStatement(s) => {
                self.error(s.span.start, "labeled statements are not supported")
            }
            other => self.error(other.span().start, "unsupported statement"),
        }
    }

    // ── declarations ─────────────────────────────────────────────────────

    /// `let`/`const`/`var` declarations. Every binding's slot was assigned by
    /// analysis and is looked up by span; a declaration is a statement, so
    /// nothing is left on the stack.
    pub(super) fn compile_var_decl(&mut self, decl: &ast::VariableDeclaration) {
        use ast::VariableDeclarationKind as Kind;
        let is_var = decl.kind == Kind::Var;
        if matches!(decl.kind, Kind::Using | Kind::AwaitUsing) {
            self.error(decl.span.start, "`using` declarations are not supported");
            return;
        }
        for d in &decl.declarations {
            // `const NAME = <non-capturing fn-expr>` is a constant function
            // (Phase F): emit only the body — no value push, no store — since
            // references resolve to its `Fn` constant.
            if let ast::BindingPattern::BindingIdentifier(_) = &d.id {
                if let Some(init) = &d.init {
                    if self.emit_const_fn_expr_body(init) {
                        continue;
                    }
                }
            }
            match &d.id {
                ast::BindingPattern::BindingIdentifier(id) => {
                    let slot = self.binding_slot(id.span.start);
                    match (&d.init, slot) {
                        (Some(init), Some(slot)) => {
                            let init_start = self.code.len();
                            self.compile_expr(init);
                            let init_end = self.code.len();
                            // Record an immutable binding (a `const`, or an
                            // effectively-const `let`/`var` — never reassigned and
                            // never captured) bound to a compile-time constant, so
                            // references emit the literal directly (and then fold).
                            // (Mutable, captured-and-mutable, non-constant, and
                            // destructured initializers aren't recorded.)
                            let recorded = self.binding_immutable(id.span.start)
                                && match crate::optimizer::const_eval(
                                    &self.code[init_start..init_end],
                                ) {
                                    Some(push) => {
                                        self.const_env.insert(slot, push);
                                        true
                                    }
                                    None => false,
                                };
                            // Dead-store elimination: if every read is propagated
                            // (recorded) and the slot is not captured (so
                            // `MakeClosure` never reads it), the initializer store
                            // is dead. `const_eval` succeeding guarantees the init
                            // is pure const-pushes/ops (no labels/effects), so we
                            // can drop the emitted init wholesale.
                            if recorded && !self.binding_captured(id.span.start) {
                                self.code.truncate(init_start);
                                self.spans.truncate(init_start);
                            } else {
                                // Inside a loop, a captured `let`/`const` binding
                                // gets a fresh cell each iteration so in-loop
                                // closures capture per-iteration copies. The new
                                // cell's seed value is irrelevant here — this
                                // SetLocal overwrites it with the initializer.
                                self.fresh_cell_if_needed(slot, d.span.start);
                                self.emit(Instr::SetLocal(slot as LocalIndex), d.span.start);
                            }
                        }
                        (None, Some(slot)) if !is_var => {
                            // `let x;` re-initializes to `undefined` each time the
                            // declaration executes (e.g. per loop iteration).
                            // Outside a loop `EnterFrame` already zeroed the slot,
                            // so skip the redundant Push+SetLocal.
                            if !self.loops.is_empty() {
                                self.fresh_cell_if_needed(slot, d.span.start);
                                self.emit(Instr::PushUndefined, d.span.start);
                                self.emit(Instr::SetLocal(slot as LocalIndex), d.span.start);
                            }
                        }
                        _ => {}
                    }
                }
                pattern => match &d.init {
                    Some(init) => {
                        // Evaluate the source once, then destructure it (the
                        // helper consumes the source value).
                        self.compile_expr(init);
                        self.destructure_binding(pattern, d.span.start);
                    }
                    None => self.error(
                        d.span.start,
                        "destructuring declaration requires an initializer",
                    ),
                },
            }
        }
    }
}
