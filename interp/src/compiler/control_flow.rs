use oxc_ast::ast;

use crate::vm::{Instr, LocalIndex};

impl<'src> super::Compiler<'src> {
    pub(super) fn compile_if(&mut self, s: &ast::IfStatement) {
        let span = s.span.start;
        self.compile_expr(&s.test);
        match &s.alternate {
            Some(alt) => {
                let els = self.new_label();
                let end = self.new_label();
                self.emit(Instr::JFalse(els), span);
                self.compile_stmt(&s.consequent);
                self.emit(Instr::Jump(end), span);
                self.emit(Instr::Label(els), span);
                self.compile_stmt(alt);
                self.emit(Instr::Label(end), span);
            }
            None => {
                let end = self.new_label();
                self.emit(Instr::JFalse(end), span);
                self.compile_stmt(&s.consequent);
                self.emit(Instr::Label(end), span);
            }
        }
    }

    pub(super) fn compile_while(&mut self, s: &ast::WhileStatement) {
        let span = s.span.start;
        let top = self.new_label();
        let end = self.new_label();
        self.emit(Instr::Label(top), span);
        self.compile_expr(&s.test);
        self.emit(Instr::JFalse(end), span);
        self.loops.push(super::LoopCtx {
            break_label: end,
            continue_label: Some(top),
            floor: self.barriers.len(),
        });
        self.compile_stmt(&s.body);
        self.loops.pop();
        self.emit(Instr::Jump(top), span);
        self.emit(Instr::Label(end), span);
    }

    pub(super) fn compile_do_while(&mut self, s: &ast::DoWhileStatement) {
        let span = s.span.start;
        let top = self.new_label();
        let cont = self.new_label();
        let end = self.new_label();
        self.emit(Instr::Label(top), span);
        self.loops.push(super::LoopCtx {
            break_label: end,
            continue_label: Some(cont),
            floor: self.barriers.len(),
        });
        self.compile_stmt(&s.body);
        self.loops.pop();
        // `continue` lands here, at the loop test.
        self.emit(Instr::Label(cont), span);
        self.compile_expr(&s.test);
        self.emit(Instr::JTrue(top), span);
        self.emit(Instr::Label(end), span);
    }

    pub(super) fn compile_for(&mut self, s: &ast::ForStatement) {
        let span = s.span.start;
        // Captured head bindings need a fresh cell per iteration so closures
        // created in the body capture per-iteration copies. The init declaration
        // runs once (outside the loop), so its bindings are NOT handled by
        // `compile_var_decl`'s in-loop path; we re-box them explicitly below,
        // modeling the spec's CreatePerIterationEnvironment. (`var` heads are
        // function-scoped — never in `fresh_owns` — so they stay shared.)
        let mut head_fresh: Vec<u32> = Vec::new();
        match &s.init {
            Some(ast::ForStatementInit::VariableDeclaration(decl)) => {
                self.compile_var_decl(decl);
                for d in &decl.declarations {
                    if let ast::BindingPattern::BindingIdentifier(id) = &d.id {
                        if let Some(slot) = self.binding_slot(id.span.start) {
                            if self.slot_needs_fresh(slot) {
                                head_fresh.push(slot);
                            }
                        }
                    }
                }
            }
            Some(other) => {
                // An expression initializer (it inherits the `Expression`
                // variants): evaluate and discard.
                if let Some(expr) = other.as_expression() {
                    self.compile_expr(expr);
                    self.emit(Instr::Pop(1), span);
                }
            }
            None => {}
        }
        // Initial per-iteration environment: seed each head binding's first cell.
        for &slot in &head_fresh {
            self.emit(Instr::FreshCell(slot as LocalIndex), span);
        }
        let top = self.new_label();
        let cont = self.new_label();
        let end = self.new_label();
        self.emit(Instr::Label(top), span);
        if let Some(test) = &s.test {
            self.compile_expr(test);
            self.emit(Instr::JFalse(end), span);
        }
        self.loops.push(super::LoopCtx {
            break_label: end,
            continue_label: Some(cont),
            floor: self.barriers.len(),
        });
        self.compile_stmt(&s.body);
        self.loops.pop();
        // `continue` runs the update, then re-tests.
        self.emit(Instr::Label(cont), span);
        // Re-box BEFORE the update so the just-captured cell is never mutated:
        // the new cell copies the current value forward, the update mutates the
        // new cell, and the next body sees/captures it.
        for &slot in &head_fresh {
            self.emit(Instr::FreshCell(slot as LocalIndex), span);
        }
        if let Some(update) = &s.update {
            self.compile_expr(update);
            self.emit(Instr::Pop(1), span);
        }
        self.emit(Instr::Jump(top), span);
        self.emit(Instr::Label(end), span);
    }

    pub(super) fn compile_break(&mut self, s: &ast::BreakStatement) {
        if s.label.is_some() {
            self.error(s.span.start, "labeled `break` is not supported");
            return;
        }
        match self.loops.last() {
            Some(ctx) => {
                let kind = super::ExitKind::Jump {
                    target: ctx.break_label,
                    floor: ctx.floor,
                };
                self.emit_exit(kind, s.span.start);
            }
            None => self.error(s.span.start, "`break` outside a loop"),
        }
    }

    pub(super) fn compile_continue(&mut self, s: &ast::ContinueStatement) {
        if s.label.is_some() {
            self.error(s.span.start, "labeled `continue` is not supported");
            return;
        }
        // `continue` targets the innermost *loop* — break-only `switch` entries
        // (`continue_label: None`) are skipped, so `continue` inside a `switch`
        // escapes to the enclosing loop, as in JS.
        let target = self
            .loops
            .iter()
            .rev()
            .find_map(|ctx| ctx.continue_label.map(|l| (l, ctx.floor)));
        match target {
            Some((target, floor)) => {
                self.emit_exit(super::ExitKind::Jump { target, floor }, s.span.start);
            }
            None => self.error(s.span.start, "`continue` outside a loop"),
        }
    }

    /// Emit an early exit (`break`/`continue`), unwinding the barriers from
    /// the current emission point down to the destination's `floor`,
    /// innermost-out: a balancing `TryExit` per `try` entry, a `Pop` per
    /// crossed stack residue (switch discriminants; the pending thrown
    /// value beneath a `finally` unwind copy — discarding it is JS's
    /// completion-override). At the innermost crossed *finalizer* entry the
    /// exit instead jumps to that entry's stub for this destination
    /// ([`Compiler::stub_label`]); the stub (emitted by `compile_try` in
    /// the post-`try` compile context) runs the finally block and continues
    /// the exit from there, so outer finallys chain recursively.
    ///
    /// Soundness of the pop ordering: a residue is popped only after every
    /// handler entered above it has been `TryExit`ed; a handler entered
    /// while a residue is open snapshots a stack that includes the residue
    /// slots, so popping them any earlier would desynchronize the snapshot.
    pub(super) fn emit_exit(&mut self, kind: super::ExitKind, span: u32) {
        let floor = match kind {
            super::ExitKind::Jump { floor, .. } => floor,
            super::ExitKind::Return => 0,
        };
        let mut depth = self.barriers.len();
        while depth > floor {
            depth -= 1;
            match self.barriers[depth] {
                super::Barrier::Residue { slots } => {
                    // `return` never pops residues: frame teardown discards
                    // the whole operand stack (see [`ExitKind::Return`]).
                    if slots > 0 && !matches!(kind, super::ExitKind::Return) {
                        self.emit(Instr::Pop(slots), span);
                    }
                }
                super::Barrier::Try { has_finalizer, .. } => {
                    self.emit(Instr::TryExit, span);
                    if has_finalizer {
                        let stub = self.stub_label(depth, kind);
                        self.emit(Instr::Jump(stub), span);
                        return;
                    }
                }
            }
        }
        match kind {
            super::ExitKind::Jump { target, .. } => self.emit(Instr::Jump(target), span),
            super::ExitKind::Return => {
                let slot = self.return_spill.as_ref().expect("spill set up").slot;
                self.emit(Instr::Local(slot as LocalIndex), span);
                self.emit(Instr::Return(1), span);
            }
        }
    }

    /// The exit-stub label for destination `kind` on the finalizer entry at
    /// `barrier_idx`, allocating it on first request. All exits with the
    /// same destination through the same finalizer share one stub.
    pub(super) fn stub_label(&mut self, barrier_idx: usize, kind: super::ExitKind) -> u32 {
        let super::Barrier::Try { stubs, .. } = &self.barriers[barrier_idx] else {
            unreachable!("stub_label on a non-try barrier");
        };
        if let Some(&(_, label)) = stubs.iter().find(|&&(k, _)| k == kind) {
            return label;
        }
        let label = self.new_label();
        let super::Barrier::Try { stubs, .. } = &mut self.barriers[barrier_idx] else {
            unreachable!()
        };
        stubs.push((kind, label));
        label
    }

    /// Emit the exit for a `return` whose value is on top of the stack.
    /// Without a finalizer to cross: one balancing `TryExit` per open `try`
    /// entry (a frame must never leave handler entries behind), then
    /// `Return(1)` — the value rides the stack, byte-for-byte the pre-B2
    /// codegen. Crossing a finalizer: spill the value to the return slot
    /// and take the exit walk through the finally stubs.
    pub(super) fn emit_return(&mut self, span: u32) {
        let crosses_finalizer = self.barriers.iter().any(|b| {
            matches!(
                b,
                super::Barrier::Try {
                    has_finalizer: true,
                    ..
                }
            )
        });
        if !crosses_finalizer {
            let try_exits = self
                .barriers
                .iter()
                .filter(|b| matches!(b, super::Barrier::Try { .. }))
                .count();
            for _ in 0..try_exits {
                self.emit(Instr::TryExit, span);
            }
            self.emit(Instr::Return(1), span);
            return;
        }
        let spill = self.return_spill.as_mut().expect("spill set up");
        spill.used = true;
        let slot = spill.slot;
        self.emit(Instr::SetLocal(slot as LocalIndex), span);
        self.emit_exit(super::ExitKind::Return, span);
    }

    /// Post-body half of the spill-slot protocol (see [`ReturnSpill`]):
    /// materialize the slot by patching (or, for a root frame that skipped
    /// it, inserting) the `EnterFrame`, only if some `return` used it.
    pub(super) fn finalize_return_spill(&mut self, spill: super::ReturnSpill, span: u32) {
        if !spill.used {
            return;
        }
        match spill.enter_frame {
            Ok(i) => {
                let Instr::EnterFrame(nparams, build_args, kinds) = &self.code[i] else {
                    unreachable!("ReturnSpill::enter_frame must point at an EnterFrame");
                };
                let mut kinds: Vec<crate::vm::SlotKind> = kinds.iter().copied().collect();
                kinds.push(crate::vm::SlotKind::Plain);
                self.code[i] = Instr::EnterFrame(*nparams, *build_args, kinds.into());
            }
            Err(i) => {
                self.code.insert(
                    i,
                    Instr::EnterFrame(0, false, vec![crate::vm::SlotKind::Plain].into()),
                );
                self.spans.insert(i, span);
            }
        }
    }

    pub(super) fn compile_try(&mut self, s: &ast::TryStatement) {
        let span = s.span.start;
        match &s.finalizer {
            None => {
                let Some(handler) = &s.handler else {
                    // The parser requires `catch` or `finally`; unreachable,
                    // but guard anyway.
                    self.error(span, "`try` requires a `catch` or `finally` clause");
                    return;
                };
                self.compile_try_catch(&s.block, handler, span);
            }
            Some(fin) => {
                let fin_label = self.new_label();
                let end = self.new_label();
                self.emit(Instr::TryEnter(fin_label), span);
                self.barriers.push(super::Barrier::Try {
                    has_finalizer: true,
                    stubs: Vec::new(),
                });
                match &s.handler {
                    Some(handler) => self.compile_try_catch(&s.block, handler, span),
                    None => {
                        for stmt in &s.block.body {
                            self.compile_stmt(stmt);
                        }
                    }
                }
                let Some(super::Barrier::Try { stubs, .. }) = self.barriers.pop() else {
                    unreachable!("unbalanced barrier stack");
                };
                self.emit(Instr::TryExit, span);
                self.compile_finally_copy(fin, 0); // normal-path copy
                self.emit(Instr::Jump(end), span);
                self.emit(Instr::Label(fin_label), fin.span.start);
                self.compile_finally_copy(fin, 1); // unwind-path copy
                self.emit(Instr::Throw, fin.span.start); // rethrow
                for (kind, stub) in stubs {
                    self.emit(Instr::Label(stub), fin.span.start);
                    self.compile_finally_copy(fin, 0); // exit-path copy
                    self.emit_exit(kind, fin.span.start); // continue outward
                }
                self.emit(Instr::Label(end), span);
            }
        }
    }

    /// Compile one copy of a `finally` block, `pending_slots` being the
    /// operand-stack slots of the pending completion beneath it (1 on the
    /// unwind path — the thrown value; 0 otherwise), pushed as a residue
    /// barrier so exits escaping the copy discard them. Function/arrow
    /// bodies inside `F` are emitted once per copy under the *same* entry
    /// label; every reference resolves to the last-emitted copy (label maps
    /// are last-wins) and the earlier, never-targeted copies are pruned as
    /// unreachable — so duplication is safe for closures too.
    pub(super) fn compile_finally_copy(&mut self, fin: &ast::BlockStatement, pending_slots: usize) {
        self.barriers.push(super::Barrier::Residue {
            slots: pending_slots,
        });
        for stmt in &fin.body {
            self.compile_stmt(stmt);
        }
        self.barriers.pop();
    }

    /// The `try { … } catch (e) { … }` core (no finalizer at this level).
    pub(super) fn compile_try_catch(
        &mut self,
        block: &ast::BlockStatement,
        handler: &ast::CatchClause,
        span: u32,
    ) {
        let catch_label = self.new_label();
        let end = self.new_label();
        self.emit(Instr::TryEnter(catch_label), span);
        self.barriers.push(super::Barrier::Try {
            has_finalizer: false,
            stubs: Vec::new(),
        });
        for stmt in &block.body {
            self.compile_stmt(stmt);
        }
        self.barriers.pop();
        self.emit(Instr::TryExit, span);
        self.emit(Instr::Jump(end), span);

        // Catch: the unwinder pushed the thrown value; bind or discard it.
        let hspan = handler.span.start;
        self.emit(Instr::Label(catch_label), hspan);
        match &handler.param {
            Some(param) => match &param.pattern {
                ast::BindingPattern::BindingIdentifier(id) => {
                    match self.binding_slot(id.span.start) {
                        Some(slot) => {
                            // A captured per-iteration binding gets a fresh
                            // cell, like any in-loop `let` declaration.
                            self.fresh_cell_if_needed(slot, hspan);
                            self.emit(Instr::SetLocal(slot as LocalIndex), hspan);
                        }
                        // No slot: an earlier diagnostic (e.g. shadowing
                        // `input`) — just discard the value.
                        None => self.emit(Instr::Pop(1), hspan),
                    }
                }
                pattern => {
                    // `catch ({ message })` — destructure the thrown value.
                    self.emit_pattern_fresh_cells(pattern, hspan);
                    self.destructure_binding(pattern, hspan);
                }
            },
            None => self.emit(Instr::Pop(1), hspan),
        }
        for stmt in &handler.body.body {
            self.compile_stmt(stmt);
        }
        self.emit(Instr::Label(end), span);
    }

    /// `for (let x of iter) body` — iterate the values of an array/string. The
    /// VM has no iterator protocol, so this lowers to an index counter: the
    /// iterable and the index are kept on the stack as `[iter, idx]` for the
    /// whole loop, and each step binds the loop variable to `iter[idx]`. A
    /// non-array/string iterable is a runtime `TypeError` (from `ArrLength`).
    pub(super) fn compile_for_of(&mut self, s: &ast::ForOfStatement) {
        let span = s.span.start;
        // `for await (… of …)` consumes async iterables, which this dialect
        // has no source of (tool calls return plain promises; arrays are the
        // only iterable). Await the elements in the body instead.
        if s.r#await {
            self.error(
                span,
                "`for await` is not supported (await each element in the loop body instead)",
            );
            return;
        }
        let Some(pat) = self.for_loop_binding_pattern(&s.left, span) else {
            return;
        };
        // Push the iterable; `compile_index_loop` adds the counter and consumes
        // both at the end.
        self.compile_expr(&s.right);
        self.compile_index_loop(pat, &s.body, span);
    }

    /// `for (let k in obj) body` — iterate the keys of an object (insertion
    /// order). Lowers to `Object.keys(obj)` (an array of string keys) followed
    /// by the same index loop as `for-of`, binding the loop variable to each
    /// key. Over `state` this enumerates the blessed object's keys.
    pub(super) fn compile_for_in(&mut self, s: &ast::ForInStatement) {
        let span = s.span.start;
        let Some(pat) = self.for_loop_binding_pattern(&s.left, span) else {
            return;
        };
        self.compile_expr(&s.right);
        self.emit(
            Instr::CallBuiltin(crate::builtin::Builtin::ObjKeys, 1),
            span,
        );
        self.compile_index_loop(pat, &s.body, span);
    }

    /// Shared iteration scaffold for `for-of`/`for-in`. Expects the container to
    /// iterate already on the stack top. Pushes an index counter, then on each
    /// step binds the loop pattern to `container[idx]` and runs `body`;
    /// `break`/`continue` resolve through the loop-context stack. The container
    /// and counter (`[container, idx]`) are maintained on the stack at constant
    /// depth across the loop top, the `continue` target, and the exit — so
    /// `break` (→ end) and `continue` (→ increment) both land where exactly
    /// those two values are present, and the final `Pop(2)` cleans them up.
    pub(super) fn compile_index_loop(
        &mut self,
        pat: &ast::BindingPattern,
        body: &ast::Statement,
        span: u32,
    ) {
        self.emit(Instr::PushPosInt(0), span); // [cont, idx]
        let top = self.new_label();
        let cont = self.new_label();
        let end = self.new_label();
        self.emit(Instr::Label(top), span);
        // idx < length(container) ?
        self.emit(Instr::Pick(0), span); // [cont, idx, idx]
        self.emit(Instr::Pick(2), span); // [cont, idx, idx, cont]
        self.emit(Instr::ArrLength, span); // [cont, idx, idx, len]
        self.emit(Instr::Lt, span); // [cont, idx, idx<len]
        self.emit(Instr::JFalse(end), span); // [cont, idx]
        // Bind loop var = container[idx].
        self.emit(Instr::Pick(1), span); // [cont, idx, cont]
        self.emit(Instr::Pick(1), span); // [cont, idx, cont, idx]
        self.emit(Instr::IndexGet, span); // [cont, idx, elem]
        // A captured loop variable gets a fresh cell each iteration so in-loop
        // closures capture per-iteration copies; the SetLocal then binds the
        // element into that fresh cell. (`var` heads stay shared.)
        if let ast::BindingPattern::BindingIdentifier(id) = pat {
            let slot = self
                .binding_slot(id.span.start)
                .expect("checked in for_loop_binding_pattern");
            if self.slot_needs_fresh(slot) {
                self.emit(Instr::FreshCell(slot as LocalIndex), span);
            }
            self.emit(Instr::SetLocal(slot as LocalIndex), span); // [cont, idx]
        } else {
            // Destructuring head: fresh-cell the captured pattern slots, then
            // destructure the element (consumes it, restoring [cont, idx]).
            self.emit_pattern_fresh_cells(pat, span);
            self.destructure_binding(pat, span);
        }
        self.loops.push(super::LoopCtx {
            break_label: end,
            continue_label: Some(cont),
            floor: self.barriers.len(),
        });
        self.compile_stmt(body);
        self.loops.pop();
        // `continue` lands here, at the increment.
        self.emit(Instr::Label(cont), span);
        self.emit(Instr::PushPosInt(1), span); // [cont, idx, 1]
        self.emit(Instr::Add, span); // [cont, idx+1]
        self.emit(Instr::Jump(top), span);
        self.emit(Instr::Label(end), span);
        self.emit(Instr::Pop(2), span); // drop [cont, idx]
    }

    /// Resolve the binding of a `for-of`/`for-in` head to its pattern — a
    /// single identifier or a destructuring pattern, in the `let`/`const`/`var`
    /// declaration form. Multiple declarators and the bare-assignment-target
    /// form (`for (x of …)`) record a diagnostic and return `None`.
    pub(super) fn for_loop_binding_pattern<'b>(
        &mut self,
        left: &'b ast::ForStatementLeft<'b>,
        span: u32,
    ) -> Option<&'b ast::BindingPattern<'b>> {
        let decl = match left {
            ast::ForStatementLeft::VariableDeclaration(decl) => decl,
            _ => {
                self.error(
                    span,
                    "for-of/for-in requires a `let`/`const`/`var` loop binding",
                );
                return None;
            }
        };
        if decl.declarations.len() != 1 {
            self.error(span, "for-of/for-in needs exactly one loop variable");
            return None;
        }
        let pat = &decl.declarations[0].id;
        if let ast::BindingPattern::BindingIdentifier(id) = pat {
            // An identifier without a slot is an earlier diagnostic (e.g.
            // shadowing `input`) — bail like any other malformed head.
            self.binding_slot(id.span.start)?;
        }
        Some(pat)
    }

    /// `switch (disc) { case a: … default: … }`. The discriminant value is kept
    /// on the stack across the whole construct (`[disc]`); each `case` test is
    /// compared against a duplicate of it with strict `===` (`Eq`). On a match
    /// we jump to that case's body; bodies are emitted in source order so
    /// fall-through is just running into the next one. `default` is dispatched
    /// to when no `case` matches (it may sit anywhere among the bodies).
    /// `break` jumps to the switch end (via a break-only loop-context entry);
    /// `continue` is not bound here and escapes to any enclosing loop.
    pub(super) fn compile_switch(&mut self, s: &ast::SwitchStatement) {
        let span = s.span.start;
        self.compile_expr(&s.discriminant); // [disc]
        let end = self.new_label();
        // One body label per case (including `default`).
        let case_labels: Vec<u32> = s.cases.iter().map(|_| self.new_label()).collect();
        let mut default_idx: Option<usize> = None;

        // Dispatch: test each `case` in source order; record `default` for last.
        for (i, case) in s.cases.iter().enumerate() {
            match &case.test {
                Some(test) => {
                    self.emit(Instr::Pick(0), span); // dup disc -> [disc, disc]
                    self.compile_expr(test); // [disc, disc, test]
                    self.emit(Instr::Eq, span); // [disc, disc===test]
                    self.emit(Instr::JTrue(case_labels[i]), span); // [disc]
                }
                None => default_idx = Some(i),
            }
        }
        // No case matched → default body (if any), else the end.
        match default_idx {
            Some(i) => self.emit(Instr::Jump(case_labels[i]), span),
            None => self.emit(Instr::Jump(end), span),
        }

        // Bodies in source order; consecutive bodies fall through. `break` → end.
        // The discriminant stays on the stack across the bodies: a residue
        // barrier makes a `continue` (which targets the enclosing loop, past
        // this switch) pop it. The switch's own `break` lands at `end`, where
        // the discriminant is expected (and popped below) — its loop context
        // sits above the residue, so `break` never crosses it.
        self.barriers.push(super::Barrier::Residue { slots: 1 });
        self.loops.push(super::LoopCtx {
            break_label: end,
            continue_label: None,
            floor: self.barriers.len(),
        });
        for (i, case) in s.cases.iter().enumerate() {
            self.emit(Instr::Label(case_labels[i]), span);
            for stmt in &case.consequent {
                self.compile_stmt(stmt);
            }
        }
        self.loops.pop();
        self.barriers.pop();
        self.emit(Instr::Label(end), span);
        self.emit(Instr::Pop(1), span); // drop disc
    }
}
