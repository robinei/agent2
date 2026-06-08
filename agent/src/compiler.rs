//! Compiler — JS source → VM instructions (`vm::Instr`).
//!
//! Codegen (Pass 2): parses with `oxc_parser`, then lowers the AST to VM
//! instructions, resolving every binding/reference/function by span through the
//! [`crate::analyzer`] tables (Pass 1) — this pass keeps no scope state of its
//! own. Diagnostics live in [`crate::diag`]. See `COMPILER_PLAN.md` for the
//! full design.

use std::sync::Arc;

use oxc_allocator::Allocator;
use oxc_ast::ast;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

use crate::analyzer::{self, ProgramAnalysis, RefSlot, frame_abs};
use crate::builtin::Builtin;
use crate::diag::Diagnostic;
use crate::vm::{Instr, SetMode, SlotKind, StackValue};

/// A compiled program: the flat instruction stream, a parallel span table
/// (`spans[ip]` = source byte offset of the instruction at `ip`), and the
/// source it was compiled from (for rendering runtime diagnostics).
#[derive(Debug)]
pub struct Program {
    pub code: Vec<Instr>,
    pub spans: Vec<u32>,
    pub source: Arc<str>,
}

/// Compile JS source into a `Program`. Collects every diagnostic (oxc syntax
/// errors plus our own semantic errors) and returns them all if any exist,
/// rather than producing a partial program.
pub fn compile(source: &str) -> Result<Program, Vec<Diagnostic>> {
    let allocator = Allocator::default();
    let source_type = SourceType::default(); // JavaScript module

    // Append only the higher-order-method helpers (`__map`, …) the program
    // actually uses. They are real JS compiled in the same unit (appended, so
    // user spans are unchanged), hoisted like any top-level function and
    // referenced by label from the call sites. A program using no such methods
    // gets an empty prelude and compiles byte-for-byte unchanged.
    let prelude = crate::prelude::assemble(source);
    let full_source = if prelude.is_empty() {
        source.to_string()
    } else {
        format!("{source}\n{prelude}")
    };
    let ret = Parser::new(&allocator, &full_source, source_type).parse();

    let mut compiler = Compiler::new(&full_source);

    // Convert oxc's own syntax errors into our Diagnostic shape.
    for err in &ret.errors {
        let span = err
            .labels
            .as_ref()
            .and_then(|labels| labels.first())
            .map(|l| l.offset() as u32)
            .unwrap_or(0);
        compiler.error(span, err.message.to_string());
    }

    // Pass 1: scope/capture analysis. It resolves every binding/reference to a
    // frame slot (keyed by span) and computes closure captures, returning the
    // tables codegen consults plus the next free label id (codegen continues
    // the same numbering) and any semantic diagnostics.
    let result = analyzer::analyze(&ret.program);
    compiler.diagnostics.extend(result.diagnostics);
    compiler.next_label = result.next_label;
    compiler.analysis = Some(result.program);

    compiler.compile_program(&ret.program);

    if !compiler.diagnostics.is_empty() {
        return Err(compiler.diagnostics);
    }

    let (code, spans) = backpatch(compiler.code, compiler.spans, compiler.next_label);
    Ok(Program {
        code,
        spans,
        // The full source (user code + any appended prelude) so runtime
        // diagnostics render against the same offsets the spans were taken from.
        source: Arc::from(full_source.as_str()),
    })
}

/// One entry of the break/continue-context stack. `break` targets the innermost
/// entry's `break_label`; `continue` targets the innermost entry that has a
/// `continue_label`. A `switch` pushes a **break-only** entry (`continue_label:
/// None`) so `break` resolves to the switch end while `continue` skips past it
/// to the enclosing loop. Labels are resolved in backpatch.
struct LoopCtx {
    break_label: u32,
    continue_label: Option<u32>,
}

/// An assignment/update target resolved to its storage shape, so that `=`,
/// compound (`+=`), logical (`??=`), and `++`/`--` can share one read/write
/// lowering. `'r` is the borrow of the AST nodes, `'a` the arena they live in.
enum LValue<'r, 'a> {
    /// A frame-local variable slot.
    Local(u32),
    /// `obj.field` — the object expression plus the static field name.
    Member(&'r ast::Expression<'a>, String),
    /// `obj[key]` — the object expression plus the computed key expression.
    Index(&'r ast::Expression<'a>, &'r ast::Expression<'a>),
}

/// Codegen state for one compilation unit.
struct Compiler<'src> {
    source: &'src str,
    /// Instructions with `Label` markers; addresses in `Jump`/`JFalse`/`Call`/
    /// `MakeClosure`/`Push(Fn)` are label ids until the backpatch pass.
    code: Vec<Instr>,
    /// `spans[i]` = source byte offset of `code[i]`; kept in lockstep.
    spans: Vec<u32>,
    /// Monotonic label-id allocator.
    next_label: u32,
    /// Loop-context stack for `break`/`continue` (innermost loop last).
    loops: Vec<LoopCtx>,
    diagnostics: Vec<Diagnostic>,
    /// Scope/capture analysis pre-computed before codegen. `None` during the
    /// analysis pass itself; `Some` during codegen. Codegen resolves every
    /// binding/reference/function via its span-keyed tables — it keeps no scope
    /// stack of its own.
    analysis: Option<ProgramAnalysis>,
    /// Which function scope we are currently codegen'ing (index into
    /// `analysis.scopes`). Consulted only for static-call resolution of
    /// directly-named callees (`find_callee_label`/`function_arity`/…).
    current_scope: usize,
}

impl<'src> Compiler<'src> {
    fn new(source: &'src str) -> Self {
        Compiler {
            source,
            code: Vec::new(),
            spans: Vec::new(),
            next_label: 0,
            loops: Vec::new(),
            diagnostics: Vec::new(),
            analysis: None,
            current_scope: 0,
        }
    }

    /// Allocate a fresh label id.
    fn new_label(&mut self) -> u32 {
        let id = self.next_label;
        self.next_label += 1;
        id
    }

    /// Append an instruction with its source span (byte offset).
    fn emit(&mut self, instr: Instr, span: u32) {
        self.code.push(instr);
        self.spans.push(span);
    }

    /// Record a diagnostic; aborts the compile before a `Program` is produced.
    fn error(&mut self, span: u32, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            message: message.into(),
        });
    }

    // ── codegen ──────────────────────────────────────────────────────

    /// The whole program is the root frame's body. Scope/capture analysis ran
    /// already (in `self.analysis`), so the prologue `EnterFrame` slot kinds and all
    /// binding/reference/function resolutions are precomputed and looked up by
    /// span. Function bodies are emitted inline (guarded by a jump-over) at
    /// their definition sites.
    fn compile_program(&mut self, program: &ast::Program) {
        let analysis = self
            .analysis
            .as_ref()
            .expect("analysis must run before codegen");
        let root = &analysis.scopes[analysis.root];
        self.current_scope = analysis.root;

        // Prologue. The root frame has no params and no upvals, so `EnterFrame`
        // just allocates its locals (and eagerly builds top-level `arguments` if
        // referenced). Skip it entirely when there is nothing to set up.
        if !root.slot_kinds.is_empty() || root.uses_arguments {
            let kinds = root.slot_kinds.clone();
            let uses_arguments = root.uses_arguments;
            self.emit(Instr::EnterFrame(0, uses_arguments, kinds), program.span.start);
        }

        // Hoist function declarations into the prologue (emit their bindings).
        self.hoist_function_decls(&program.body);

        // Compile top-level body statements.
        for stmt in &program.body {
            self.compile_stmt(stmt);
        }

        // Root frame ends with Return(0) → StepResult::Done.
        self.emit(Instr::Return(0), program.span.end);
    }

    fn compile_stmt(&mut self, stmt: &ast::Statement) {
        match stmt {
            // Every expression statement leaves one value, popped to keep the
            // stack-discipline invariant (one value per expression). For
            // assignments and updates targeting locals, we lower directly in a
            // "value-not-needed" mode, skipping the wasted Dup/Pop pair.
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
                if analysis.scopes[self.current_scope].parent == usize::MAX {
                    self.error(r.span.start, "`return` outside a function");
                    return;
                }
                match &r.argument {
                    Some(expr) => {
                        self.compile_expr(expr);
                        self.emit(Instr::Return(1), r.span.start);
                    }
                    None => {
                        self.emit(Instr::Push(StackValue::Undefined), r.span.start);
                        self.emit(Instr::Return(1), r.span.start);
                    }
                }
            }

            ast::Statement::ForOfStatement(s) => self.compile_for_of(s),
            ast::Statement::ForInStatement(s) => self.compile_for_in(s),

            ast::Statement::SwitchStatement(s) => self.compile_switch(s),

            // Later phases / out of scope — informative errors.
            ast::Statement::ThrowStatement(s) => {
                self.error(s.span.start, "`throw` is not supported (use `raise`)")
            }
            ast::Statement::TryStatement(s) => {
                self.error(s.span.start, "`try`/`catch` is not supported (use `raise`)")
            }
            ast::Statement::ClassDeclaration(s) => {
                self.error(s.span.start, "`class` is not supported")
            }
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
    fn compile_var_decl(&mut self, decl: &ast::VariableDeclaration) {
        use ast::VariableDeclarationKind as Kind;
        let is_var = decl.kind == Kind::Var;
        if matches!(decl.kind, Kind::Using | Kind::AwaitUsing) {
            self.error(decl.span.start, "`using` declarations are not supported");
            return;
        }
        for d in &decl.declarations {
            match &d.id {
                ast::BindingPattern::BindingIdentifier(id) => {
                    let slot = self.binding_slot(id.span.start);
                    match (&d.init, slot) {
                        (Some(init), Some(slot)) => {
                            self.compile_expr(init);
                            // Inside a loop, a captured `let`/`const` binding gets
                            // a fresh cell each iteration so in-loop closures
                            // capture per-iteration copies. The new cell's seed
                            // value is irrelevant here — this SetLocal overwrites
                            // it with the initializer.
                            self.fresh_cell_if_needed(slot, d.span.start);
                            self.emit(Instr::SetLocal(slot), d.span.start);
                        }
                        (None, Some(slot)) if !is_var => {
                            // `let x;` re-initializes to `undefined` each time the
                            // declaration executes (e.g. per loop iteration).
                            // Outside a loop `EnterFrame` already zeroed the slot,
                            // so skip the redundant Push+SetLocal.
                            if !self.loops.is_empty() {
                                self.fresh_cell_if_needed(slot, d.span.start);
                                self.emit(Instr::Push(StackValue::Undefined), d.span.start);
                                self.emit(Instr::SetLocal(slot), d.span.start);
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

    /// Destructure the source value already on top of the stack into a binding
    /// pattern, **consuming** that value. Used by declarations; every leaf is a
    /// binding identifier whose slot comes from analysis (`binding_slot`).
    fn destructure_binding(&mut self, pat: &ast::BindingPattern, span: u32) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                match self.binding_slot(id.span.start) {
                    Some(slot) => self.emit(Instr::SetLocal(slot), span),
                    None => {
                        // No slot (an earlier error, e.g. shadowing `state`).
                        self.emit(Instr::Pop(1), span);
                    }
                }
            }
            ast::BindingPattern::AssignmentPattern(ap) => {
                self.emit_default(&ap.right, span);
                self.destructure_binding(&ap.left, span);
            }
            ast::BindingPattern::ArrayPattern(arr) => {
                if arr.rest.is_some() {
                    self.error(
                        arr.span.start,
                        "rest elements in destructuring are not supported",
                    );
                }
                for (i, el) in arr.elements.iter().enumerate() {
                    if let Some(el) = el {
                        self.emit(Instr::Dup, span);
                        self.emit(Instr::Push(StackValue::PosInt(i as u64)), span);
                        self.emit(Instr::IndexGet, span);
                        self.destructure_binding(el, span);
                    }
                }
                self.emit(Instr::Pop(1), span); // drop the source
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                if obj.rest.is_some() {
                    self.error(
                        obj.span.start,
                        "rest elements in destructuring are not supported",
                    );
                }
                for prop in &obj.properties {
                    self.emit(Instr::Dup, span);
                    self.emit_property_key_access(&prop.key, prop.computed, span);
                    self.destructure_binding(&prop.value, span);
                }
                self.emit(Instr::Pop(1), span); // drop the source
            }
        }
    }

    /// With an object on top of the stack, read the property named by a pattern
    /// key, leaving its value on top (consuming the object copy). A static key
    /// uses the fast `ObjGet`; a computed key evaluates the expression and uses
    /// the polymorphic `IndexGet`.
    fn emit_property_key_access(&mut self, key: &ast::PropertyKey, computed: bool, span: u32) {
        if computed {
            if let Some(expr) = key.as_expression() {
                self.compile_expr(expr);
                self.emit(Instr::IndexGet, span);
                return;
            }
        }
        let name = match key {
            ast::PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            ast::PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
            ast::PropertyKey::NumericLiteral(n) => number_key_to_string(n.value),
            _ => {
                if let Some(expr) = key.as_expression() {
                    self.compile_expr(expr);
                    self.emit(Instr::IndexGet, span);
                    return;
                }
                self.error(key.span().start, "unsupported destructuring key");
                return;
            }
        };
        self.emit(Instr::ObjGet(name), span);
    }

    /// Apply a destructuring/parameter default to the value on top of the stack:
    /// if it is `undefined`, replace it with the default expression's value;
    /// otherwise leave it. (JS applies defaults only for `undefined`, not
    /// `null`.) Leaves exactly one value either way.
    fn emit_default(&mut self, default: &ast::Expression, span: u32) {
        let have = self.new_label();
        self.emit(Instr::Dup, span);
        self.emit(Instr::Push(StackValue::Undefined), span);
        self.emit(Instr::Eq, span);
        self.emit(Instr::JFalse(have), span); // not undefined → keep the value
        self.emit(Instr::Pop(1), span); // undefined → drop and use the default
        self.compile_expr(default);
        self.emit(Instr::Label(have), span);
    }

    // ── analysis lookups ─────────────────────────────────────────────────

    /// Absolute frame slot for a binding occurrence (keyed by its span). `None`
    /// only when the binding was rejected during analysis (e.g. shadowing
    /// `state`), in which case a diagnostic was already recorded.
    fn binding_slot(&self, span: u32) -> Option<u32> {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .binding_slot
            .get(&span)
            .copied()
    }

    /// Resolution of an identifier *reference* (keyed by its span), or `None`
    /// when the name is not a local (a global / `state` / `undefined` / …, or
    /// undeclared) — the caller then falls back to name-based handling.
    fn ref_slot(&self, span: u32) -> Option<RefSlot> {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .ref_resolution
            .get(&span)
            .copied()
    }

    /// The `scopes` index of the function/arrow defined at `span`.
    fn scope_for_node(&self, span: u32) -> Option<usize> {
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
    fn slot_needs_fresh(&self, slot: u32) -> bool {
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
    fn fresh_cell_if_needed(&mut self, slot: u32, span: u32) {
        if !self.loops.is_empty() && self.slot_needs_fresh(slot) {
            self.emit(Instr::FreshCell(slot), span);
        }
    }

    // ── control flow ─────────────────────────────────────────────────────

    fn compile_if(&mut self, s: &ast::IfStatement) {
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

    fn compile_while(&mut self, s: &ast::WhileStatement) {
        let span = s.span.start;
        let top = self.new_label();
        let end = self.new_label();
        self.emit(Instr::Label(top), span);
        self.compile_expr(&s.test);
        self.emit(Instr::JFalse(end), span);
        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: Some(top),
        });
        self.compile_stmt(&s.body);
        self.loops.pop();
        self.emit(Instr::Jump(top), span);
        self.emit(Instr::Label(end), span);
    }

    fn compile_do_while(&mut self, s: &ast::DoWhileStatement) {
        let span = s.span.start;
        let top = self.new_label();
        let cont = self.new_label();
        let end = self.new_label();
        self.emit(Instr::Label(top), span);
        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: Some(cont),
        });
        self.compile_stmt(&s.body);
        self.loops.pop();
        // `continue` lands here, at the loop test.
        self.emit(Instr::Label(cont), span);
        self.compile_expr(&s.test);
        self.emit(Instr::JTrue(top), span);
        self.emit(Instr::Label(end), span);
    }

    fn compile_for(&mut self, s: &ast::ForStatement) {
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
            self.emit(Instr::FreshCell(slot), span);
        }
        let top = self.new_label();
        let cont = self.new_label();
        let end = self.new_label();
        self.emit(Instr::Label(top), span);
        if let Some(test) = &s.test {
            self.compile_expr(test);
            self.emit(Instr::JFalse(end), span);
        }
        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: Some(cont),
        });
        self.compile_stmt(&s.body);
        self.loops.pop();
        // `continue` runs the update, then re-tests.
        self.emit(Instr::Label(cont), span);
        // Re-box BEFORE the update so the just-captured cell is never mutated:
        // the new cell copies the current value forward, the update mutates the
        // new cell, and the next body sees/captures it.
        for &slot in &head_fresh {
            self.emit(Instr::FreshCell(slot), span);
        }
        if let Some(update) = &s.update {
            self.compile_expr(update);
            self.emit(Instr::Pop(1), span);
        }
        self.emit(Instr::Jump(top), span);
        self.emit(Instr::Label(end), span);
    }

    fn compile_break(&mut self, s: &ast::BreakStatement) {
        if s.label.is_some() {
            self.error(s.span.start, "labeled `break` is not supported");
            return;
        }
        match self.loops.last() {
            Some(ctx) => {
                let target = ctx.break_label;
                self.emit(Instr::Jump(target), s.span.start);
            }
            None => self.error(s.span.start, "`break` outside a loop"),
        }
    }

    fn compile_continue(&mut self, s: &ast::ContinueStatement) {
        if s.label.is_some() {
            self.error(s.span.start, "labeled `continue` is not supported");
            return;
        }
        // `continue` targets the innermost *loop* — break-only `switch` entries
        // (`continue_label: None`) are skipped, so `continue` inside a `switch`
        // escapes to the enclosing loop, as in JS.
        match self.loops.iter().rev().find_map(|ctx| ctx.continue_label) {
            Some(target) => self.emit(Instr::Jump(target), s.span.start),
            None => self.error(s.span.start, "`continue` outside a loop"),
        }
    }

    /// `for (let x of iter) body` — iterate the values of an array/string. The
    /// VM has no iterator protocol, so this lowers to an index counter: the
    /// iterable and the index are kept on the stack as `[iter, idx]` for the
    /// whole loop, and each step binds the loop variable to `iter[idx]`. A
    /// non-array/string iterable is a runtime `TypeError` (from `ArrLength`).
    fn compile_for_of(&mut self, s: &ast::ForOfStatement) {
        let span = s.span.start;
        let Some(slot) = self.for_loop_binding_slot(&s.left, span) else {
            return;
        };
        // Push the iterable; `compile_index_loop` adds the counter and consumes
        // both at the end.
        self.compile_expr(&s.right);
        self.compile_index_loop(slot, &s.body, span);
    }

    /// `for (let k in obj) body` — iterate the keys of an object (insertion
    /// order). Lowers to `Object.keys(obj)` (an array of string keys) followed
    /// by the same index loop as `for-of`, binding the loop variable to each
    /// key. Over `state` this enumerates the blessed object's keys.
    fn compile_for_in(&mut self, s: &ast::ForInStatement) {
        let span = s.span.start;
        let Some(slot) = self.for_loop_binding_slot(&s.left, span) else {
            return;
        };
        self.compile_expr(&s.right);
        self.emit(Instr::CallBuiltin(Builtin::ObjKeys, 1), span);
        self.compile_index_loop(slot, &s.body, span);
    }

    /// Shared iteration scaffold for `for-of`/`for-in`. Expects the container to
    /// iterate already on the stack top. Pushes an index counter, then on each
    /// step binds `slot` to `container[idx]` and runs `body`; `break`/`continue`
    /// resolve through the loop-context stack. The container and counter
    /// (`[container, idx]`) are maintained on the stack at constant depth across
    /// the loop top, the `continue` target, and the exit — so `break` (→ end)
    /// and `continue` (→ increment) both land where exactly those two values
    /// are present, and the final `Pop(2)` cleans them up.
    fn compile_index_loop(&mut self, slot: u32, body: &ast::Statement, span: u32) {
        self.emit(Instr::Push(StackValue::PosInt(0)), span); // [cont, idx]
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
        if self.slot_needs_fresh(slot) {
            self.emit(Instr::FreshCell(slot), span);
        }
        self.emit(Instr::SetLocal(slot), span); // [cont, idx]
        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: Some(cont),
        });
        self.compile_stmt(body);
        self.loops.pop();
        // `continue` lands here, at the increment.
        self.emit(Instr::Label(cont), span);
        self.emit(Instr::Push(StackValue::PosInt(1)), span); // [cont, idx, 1]
        self.emit(Instr::Add, span); // [cont, idx+1]
        self.emit(Instr::Jump(top), span);
        self.emit(Instr::Label(end), span);
        self.emit(Instr::Pop(2), span); // drop [cont, idx]
    }

    /// Resolve the loop variable of a `for-of`/`for-in` head to its frame slot.
    /// Only the `let`/`const`/`var x` single-identifier form is supported;
    /// destructuring, multiple declarators, and the bare-assignment-target form
    /// (`for (x of …)`) record a diagnostic and return `None`.
    fn for_loop_binding_slot(&mut self, left: &ast::ForStatementLeft, span: u32) -> Option<u32> {
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
        match &decl.declarations[0].id {
            ast::BindingPattern::BindingIdentifier(id) => self.binding_slot(id.span.start),
            _ => {
                self.error(
                    span,
                    "destructuring in a for-of/for-in binding is not supported",
                );
                None
            }
        }
    }

    /// `switch (disc) { case a: … default: … }`. The discriminant value is kept
    /// on the stack across the whole construct (`[disc]`); each `case` test is
    /// compared against a duplicate of it with strict `===` (`Eq`). On a match
    /// we jump to that case's body; bodies are emitted in source order so
    /// fall-through is just running into the next one. `default` is dispatched
    /// to when no `case` matches (it may sit anywhere among the bodies).
    /// `break` jumps to the switch end (via a break-only loop-context entry);
    /// `continue` is not bound here and escapes to any enclosing loop.
    fn compile_switch(&mut self, s: &ast::SwitchStatement) {
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
        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: None,
        });
        for (i, case) in s.cases.iter().enumerate() {
            self.emit(Instr::Label(case_labels[i]), span);
            for stmt in &case.consequent {
                self.compile_stmt(stmt);
            }
        }
        self.loops.pop();
        self.emit(Instr::Label(end), span);
        self.emit(Instr::Pop(1), span); // drop disc
    }

    /// Every expression leaves exactly one value on the stack (the
    /// stack-discipline invariant). Unsupported nodes record a diagnostic and
    /// emit nothing — the diagnostics abort the compile before a `Program` is
    /// produced, so the missing value never matters.
    fn compile_expr(&mut self, expr: &ast::Expression) {
        match expr {
            // ── literals ──────────────────────────────────────────────
            ast::Expression::NumericLiteral(lit) => {
                self.emit(
                    Instr::Push(number_literal_to_value(lit.value)),
                    lit.span.start,
                );
            }
            ast::Expression::StringLiteral(lit) => {
                self.emit(
                    Instr::PushStr(lit.value.as_str().to_string()),
                    lit.span.start,
                );
            }
            ast::Expression::BooleanLiteral(lit) => {
                self.emit(Instr::Push(StackValue::Bool(lit.value)), lit.span.start);
            }
            ast::Expression::NullLiteral(lit) => {
                self.emit(Instr::Push(StackValue::Null), lit.span.start);
            }
            ast::Expression::TemplateLiteral(tl) => self.compile_template(tl),

            // ── identifiers ───────────────────────────────────────────
            ast::Expression::Identifier(id) => {
                self.compile_identifier(id.name.as_str(), id.span.start)
            }

            // ── composite literals ────────────────────────────────────
            ast::Expression::ArrayExpression(arr) => self.compile_array(arr),
            ast::Expression::ObjectExpression(obj) => self.compile_object(obj),

            // ── operators ─────────────────────────────────────────────
            ast::Expression::BinaryExpression(bin) => self.compile_binary(bin),
            ast::Expression::UnaryExpression(un) => self.compile_unary(un),
            ast::Expression::LogicalExpression(log) => self.compile_logical(log),
            ast::Expression::ConditionalExpression(cond) => self.compile_conditional(cond),
            ast::Expression::AssignmentExpression(a) => self.compile_assignment(a, true),
            ast::Expression::SequenceExpression(seq) => {
                // The comma operator: evaluate each, discard all but the last.
                let last = seq.expressions.len().saturating_sub(1);
                for (i, e) in seq.expressions.iter().enumerate() {
                    self.compile_expr(e);
                    if i != last {
                        self.emit(Instr::Pop(1), e.span().start);
                    }
                }
            }

            // ── member access / calls ─────────────────────────────────
            ast::Expression::StaticMemberExpression(m) => self.compile_static_member(m),
            ast::Expression::ComputedMemberExpression(m) => self.compile_computed_member(m),
            ast::Expression::CallExpression(c) => self.compile_call(c),
            ast::Expression::ChainExpression(chain) => {
                self.compile_chain_element(&chain.expression)
            }

            ast::Expression::ParenthesizedExpression(p) => self.compile_expr(&p.expression),

            ast::Expression::UpdateExpression(u) => self.compile_update(u, true),

            // ── Phase 3: function expressions / arrows ────────────────
            ast::Expression::FunctionExpression(f) => {
                self.compile_function_expr(f, f.span.start);
            }
            ast::Expression::ArrowFunctionExpression(f) => {
                self.compile_arrow_expr(f, f.span.start);
            }

            // ── informative errors for out-of-scope nodes ─────────────
            ast::Expression::BigIntLiteral(b) => {
                self.error(b.span.start, "BigInt is not supported")
            }
            ast::Expression::RegExpLiteral(r) => {
                self.error(r.span.start, "regular expressions are not supported")
            }
            ast::Expression::ThisExpression(t) => {
                self.error(t.span.start, "`this` is not supported")
            }
            ast::Expression::NewExpression(n) => self.error(n.span.start, "`new` is not supported"),
            other => self.error(other.span().start, "unsupported expression"),
        }
    }

    /// A bare identifier resolves only to the blessed `state` object or the
    /// global literal-like names. Everything else is an undeclared variable —
    /// a compile error, so typos can't silently become persistent state. (Local
    /// variables arrive in Phase 2/3; namespace names like `Math`/`Object` are
    /// recognized structurally as call/member receivers, never as bare values.)
    fn compile_identifier(&mut self, name: &str, span: u32) {
        // A local/param/captured variable resolves to its frame slot (resolved
        // by analysis, keyed by this reference's span); `Local` dereferences a
        // boxed slot transparently.
        if let Some(r) = self.ref_slot(span) {
            self.emit(Instr::Local(r.slot), span);
            return;
        }
        // `arguments` (when not shadowed by a real binding above) is the current
        // frame's argument array — built and cached per frame by the VM.
        if name == "arguments" {
            self.emit(Instr::Arguments, span);
            return;
        }
        let value = match name {
            "state" => StackValue::Ptr(0),
            "undefined" => StackValue::Undefined,
            "NaN" => StackValue::Number(f64::NAN),
            "Infinity" => StackValue::Number(f64::INFINITY),
            _ => {
                self.error(span, format!("undeclared variable `{name}`"));
                return;
            }
        };
        self.emit(Instr::Push(value), span);
    }

    // ── operators ──────────────────────────────────────────────────────

    fn compile_binary(&mut self, bin: &ast::BinaryExpression) {
        use ast::BinaryOperator as Op;
        let span = bin.span.start;

        // `key in obj` lowers to ObjHas, which pops the (string) key then the
        // object. Evaluate left (key) then right (obj) to keep JS eval order,
        // then Swap into [obj, key]; ToStr coerces the key as JS `in` does.
        if bin.operator == Op::In {
            self.compile_expr(&bin.left);
            self.emit(Instr::ToStr, span);
            self.compile_expr(&bin.right);
            self.emit(Instr::Swap, span);
            self.emit(Instr::ObjHas, span);
            return;
        }

        // Evaluate operands left-to-right; the op pops rhs then lhs.
        self.compile_expr(&bin.left);
        self.compile_expr(&bin.right);
        let instr = match bin.operator {
            Op::Addition => Instr::Add,
            Op::Subtraction => Instr::Sub,
            Op::Multiplication => Instr::Mul,
            Op::Division => Instr::Div,
            Op::Remainder => Instr::Mod,
            Op::Exponential => Instr::Pow,
            Op::Equality => Instr::LooseEq,
            Op::Inequality => Instr::LooseNeq,
            Op::StrictEquality => Instr::Eq,
            Op::StrictInequality => Instr::Neq,
            Op::LessThan => Instr::Lt,
            Op::LessEqualThan => Instr::LtEq,
            Op::GreaterThan => Instr::Gt,
            Op::GreaterEqualThan => Instr::GtEq,
            Op::BitwiseAnd => Instr::BitAnd,
            Op::BitwiseOR => Instr::BitOr,
            Op::BitwiseXOR => Instr::BitXor,
            Op::ShiftLeft => Instr::BitLhs,
            Op::ShiftRight => Instr::BitRhs,
            Op::ShiftRightZeroFill => {
                self.error(span, "unsigned right shift (`>>>`) is not supported");
                return;
            }
            Op::Instanceof => {
                self.error(span, "`instanceof` is not supported");
                return;
            }
            Op::In => unreachable!("`in` handled above"),
        };
        self.emit(instr, span);
    }

    fn compile_unary(&mut self, un: &ast::UnaryExpression) {
        use ast::UnaryOperator as Op;
        let span = un.span.start;
        match un.operator {
            Op::UnaryNegation => {
                // Fold `-<numeric literal>` to a canonical NegInt/Number at
                // compile time; otherwise `Neg` promotes to Number(-x).
                if let ast::Expression::NumericLiteral(lit) = &un.argument {
                    self.emit(Instr::Push(f64_to_value(-lit.value)), span);
                } else {
                    self.compile_expr(&un.argument);
                    self.emit(Instr::Neg, span);
                }
            }
            Op::UnaryPlus => {
                self.compile_expr(&un.argument);
                self.emit(Instr::ToNum, span);
            }
            Op::LogicalNot => {
                self.compile_expr(&un.argument);
                self.emit(Instr::Not, span);
            }
            Op::BitwiseNot => {
                self.compile_expr(&un.argument);
                self.emit(Instr::BitNot, span);
            }
            Op::Typeof => {
                self.compile_expr(&un.argument);
                self.emit(Instr::TypeOf, span);
            }
            Op::Void => {
                self.compile_expr(&un.argument);
                self.emit(Instr::Pop(1), span);
                self.emit(Instr::Push(StackValue::Undefined), span);
            }
            Op::Delete => self.compile_delete(&un.argument, span),
        }
    }

    /// `delete obj.foo` / `delete obj[k]` lower to `ObjDelete` (which pops the
    /// string key then the object and pushes whether it existed). A non-property
    /// delete is an error.
    fn compile_delete(&mut self, arg: &ast::Expression, span: u32) {
        match arg {
            ast::Expression::StaticMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.emit(
                    Instr::PushStr(m.property.name.as_str().to_string()),
                    m.property.span.start,
                );
                self.emit(Instr::ObjDelete, span);
            }
            ast::Expression::ComputedMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.compile_expr(&m.expression);
                self.emit(Instr::ToStr, span); // coerce the key to a string
                self.emit(Instr::ObjDelete, span);
            }
            ast::Expression::ChainExpression(c) => {
                // `delete a?.b` — compile the chained member, then delete.
                self.compile_delete_chain(&c.expression, span)
            }
            _ => self.error(span, "`delete` is only supported on object properties"),
        }
    }

    fn compile_delete_chain(&mut self, el: &ast::ChainElement, span: u32) {
        match el {
            ast::ChainElement::StaticMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.emit(
                    Instr::PushStr(m.property.name.as_str().to_string()),
                    m.property.span.start,
                );
                self.emit(Instr::ObjDelete, span);
            }
            ast::ChainElement::ComputedMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.compile_expr(&m.expression);
                self.emit(Instr::ToStr, span);
                self.emit(Instr::ObjDelete, span);
            }
            _ => self.error(span, "`delete` is only supported on object properties"),
        }
    }

    /// Short-circuit `&&` / `||` / `??`, branch-compiled (NOT the `And`/`Or`
    /// instructions, which evaluate both operands and so cannot short-circuit).
    fn compile_logical(&mut self, log: &ast::LogicalExpression) {
        use ast::LogicalOperator as Op;
        let span = log.span.start;
        self.compile_expr(&log.left);
        match log.operator {
            Op::And => {
                // truthy: drop lhs, eval rhs; falsy: keep lhs.
                let end = self.new_label();
                self.emit(Instr::Dup, span);
                self.emit(Instr::JFalse(end), span);
                self.emit(Instr::Pop(1), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Label(end), span);
            }
            Op::Or => {
                // truthy: keep lhs; falsy: drop lhs, eval rhs.
                let end = self.new_label();
                self.emit(Instr::Dup, span);
                self.emit(Instr::JTrue(end), span);
                self.emit(Instr::Pop(1), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Label(end), span);
            }
            Op::Coalesce => {
                // not nullish: keep lhs (the peeking jump leaves it); nullish:
                // drop lhs and evaluate rhs.
                let end = self.new_label();
                self.emit(Instr::JNotNullish(end), span);
                self.emit(Instr::Pop(1), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Label(end), span);
            }
        }
    }

    fn compile_conditional(&mut self, cond: &ast::ConditionalExpression) {
        let span = cond.span.start;
        let els = self.new_label();
        let end = self.new_label();
        self.compile_expr(&cond.test);
        self.emit(Instr::JFalse(els), span);
        self.compile_expr(&cond.consequent);
        self.emit(Instr::Jump(end), span);
        self.emit(Instr::Label(els), span);
        self.compile_expr(&cond.alternate);
        self.emit(Instr::Label(end), span);
    }

    // ── composite literals ──────────────────────────────────────────────

    fn compile_array(&mut self, arr: &ast::ArrayExpression) {
        let mut n = 0u32;
        for el in &arr.elements {
            match el.as_expression() {
                Some(e) => {
                    self.compile_expr(e);
                    n += 1;
                }
                None => {
                    self.error(
                        el.span().start,
                        "array holes and spread elements are not supported",
                    );
                    return;
                }
            }
        }
        self.emit(Instr::ArrNew(n), arr.span.start);
    }

    fn compile_object(&mut self, obj: &ast::ObjectExpression) {
        let mut names: Vec<String> = Vec::with_capacity(obj.properties.len());
        for prop in &obj.properties {
            let p = match prop {
                ast::ObjectPropertyKind::ObjectProperty(p) => p,
                ast::ObjectPropertyKind::SpreadProperty(s) => {
                    self.error(s.span.start, "object spread is not supported");
                    return;
                }
            };
            if p.kind != ast::PropertyKind::Init {
                self.error(p.span.start, "getters/setters are not supported");
                return;
            }
            if p.method {
                self.error(p.span.start, "object methods are not supported");
                return;
            }
            if p.computed {
                self.error(p.span.start, "computed object keys are not supported");
                return;
            }
            let name = match &p.key {
                ast::PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
                ast::PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
                ast::PropertyKey::NumericLiteral(num) => number_key_to_string(num.value),
                _ => {
                    self.error(p.key.span().start, "unsupported object key");
                    return;
                }
            };
            // Values are pushed in source order (field 0's value deepest), then
            // ObjNew consumes them against the parallel field-name list.
            self.compile_expr(&p.value);
            names.push(name);
        }
        self.emit(Instr::ObjNew(names), obj.span.start);
    }

    fn compile_template(&mut self, tl: &ast::TemplateLiteral) {
        let span = tl.span.start;
        // result = quasi0 + expr0 + quasi1 + expr1 + … . The accumulator starts
        // as a string (PushStr) and stays one, so every `Add` takes the concat
        // path and ToString-coerces each interpolated value, as JS does.
        let quasi_str = |q: &ast::TemplateElement| {
            q.value
                .cooked
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or_else(|| q.value.raw.as_str())
                .to_string()
        };
        self.emit(Instr::PushStr(quasi_str(&tl.quasis[0])), span);
        for (i, expr) in tl.expressions.iter().enumerate() {
            self.compile_expr(expr);
            self.emit(Instr::Add, span);
            self.emit(Instr::PushStr(quasi_str(&tl.quasis[i + 1])), span);
            self.emit(Instr::Add, span);
        }
    }

    // ── member access ────────────────────────────────────────────────────

    /// `obj.foo` (and `state.foo`, since `state` lowers to `Ptr(0)`). `.length`
    /// is the static intrinsic `ArrLength` (the accepted divergence: an object
    /// property literally named `length` reached via `.length`); anything else
    /// is `ObjGet`.
    fn compile_static_member(&mut self, m: &ast::StaticMemberExpression) {
        // First-class reference to a namespaced builtin used as a *value* (e.g.
        // `Math.sqrt` passed as a callback or invoked via `?.()`): push the
        // `Builtin`. `?.` here is a no-op — a namespace is never nullish.
        if let ast::Expression::Identifier(obj) = &m.object {
            if let Some(builtin) = namespace_builtin(obj.name.as_str(), m.property.name.as_str()) {
                self.emit(Instr::Push(StackValue::Builtin(builtin)), m.span.start);
                return;
            }
        }
        self.compile_expr(&m.object);
        if m.optional {
            let end = self.begin_optional(m.span.start);
            self.emit_static_access(m);
            self.emit(Instr::Label(end), m.span.start);
        } else {
            self.emit_static_access(m);
        }
    }

    fn emit_static_access(&mut self, m: &ast::StaticMemberExpression) {
        let name = m.property.name.as_str();
        let span = m.property.span.start;
        if name == "length" {
            self.emit(Instr::ArrLength, span);
        } else {
            self.emit(Instr::ObjGet(name.to_string()), span);
        }
    }

    /// `obj[expr]` — runtime-polymorphic computed access via `IndexGet`.
    fn compile_computed_member(&mut self, m: &ast::ComputedMemberExpression) {
        self.compile_expr(&m.object);
        if m.optional {
            let end = self.begin_optional(m.span.start);
            self.compile_expr(&m.expression);
            self.emit(Instr::IndexGet, m.span.start);
            self.emit(Instr::Label(end), m.span.start);
        } else {
            self.compile_expr(&m.expression);
            self.emit(Instr::IndexGet, m.span.start);
        }
    }

    /// Optional-chaining (`?.`) prologue. With the guarded value already on the
    /// stack, short-circuit to `undefined` when it is nullish (null or
    /// undefined); otherwise leave the value for the access/call that the caller
    /// emits next. Returns the `end` label to place after that access.
    ///
    /// The peeking `JNotNullish` keeps the value on the not-nullish path with no
    /// `Dup`, so the whole guard is one branch plus the short-circuit tail.
    ///
    /// Per-link: a fully-`?.` chain (`a?.b?.c`) short-circuits correctly because
    /// each link re-checks; mixing `?.` then a plain `.` on a nullish base
    /// (`a?.b.c`) is an accepted divergence (runtime TypeError, not `undefined`).
    fn begin_optional(&mut self, span: u32) -> u32 {
        let cont = self.new_label();
        let end = self.new_label();
        self.emit(Instr::JNotNullish(cont), span);
        self.emit(Instr::Pop(1), span);
        self.emit(Instr::Push(StackValue::Undefined), span);
        self.emit(Instr::Jump(end), span);
        self.emit(Instr::Label(cont), span);
        end
    }

    fn compile_chain_element(&mut self, el: &ast::ChainElement) {
        match el {
            ast::ChainElement::CallExpression(c) => self.compile_call(c),
            ast::ChainElement::StaticMemberExpression(m) => self.compile_static_member(m),
            ast::ChainElement::ComputedMemberExpression(m) => self.compile_computed_member(m),
            ast::ChainElement::PrivateFieldExpression(m) => {
                self.error(m.span.start, "private fields are not supported")
            }
            ast::ChainElement::TSNonNullExpression(e) => self.error(
                e.span.start,
                "TypeScript non-null assertions are not supported",
            ),
        }
    }

    // ── assignment ───────────────────────────────────────────────────────

    /// Assignment is an expression: when `value_needed` is true, it leaves the
    /// assigned value on the stack. In void context (`value_needed == false`),
    /// the value is either consumed by `SetLocal` (for locals) or popped after
    /// `ObjSet`/`IndexSet`. Plain `=`, compound (`+=` …), and short-circuiting
    /// logical (`&&=`/`||=`/`??=`) assignment all share the [`LValue`] read/write
    /// lowering. Array/object destructuring targets are handled separately.
    fn compile_assignment(&mut self, a: &ast::AssignmentExpression, value_needed: bool) {
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
                    self.emit(Instr::Dup, span); // one copy is the expression result
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
    fn compound_binary_instr(&mut self, op: ast::AssignmentOperator, span: u32) -> Option<Instr> {
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
            Op::ShiftRightZeroFill => {
                self.error(span, "unsigned right shift (`>>>=`) is not supported");
                return None;
            }
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
    fn compile_logical_assign(
        &mut self,
        lv: &LValue<'_, '_>,
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
            Op::LogicalNullish => self.emit(Instr::JNotNullish(keep), span),
            Op::LogicalAnd => {
                // For `&&=`, need a copy of `old` to test truthiness without
                // consuming it (the keep path needs it). In void context we
                // can just peek (JFalse pops, but we'd lose old). We always
                // Dup since the keep path or store path consumes `old`.
                self.emit(Instr::Dup, span);
                self.emit(Instr::JFalse(keep), span); // falsy → keep old
            }
            Op::LogicalOr => {
                self.emit(Instr::Dup, span);
                self.emit(Instr::JTrue(keep), span); // truthy → keep old
            }
            _ => unreachable!("only logical operators reach here"),
        }
        // Store path: discard old, evaluate the RHS, store it.
        self.emit(Instr::Pop(1), span);
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
    fn compile_update(&mut self, u: &ast::UpdateExpression, value_needed: bool) {
        let span = u.span.start;
        let lv = match self.lvalue_from_simple_target(&u.argument) {
            Some(lv) => lv,
            None => return,
        };
        // `p`: ++ subtracts -1 (i.e. adds 1); -- subtracts +1.
        let p = match u.operator {
            ast::UpdateOperator::Increment => StackValue::NegInt(-1),
            ast::UpdateOperator::Decrement => StackValue::PosInt(1),
        };

        // Fast path for local variables: use `IncLocal` (1 instruction) when
        // the value is needed, or load-sub-store when void.
        if let LValue::Local(slot) = &lv {
            if value_needed {
                let mode = if u.prefix {
                    crate::vm::UpdateMode::Prefix
                } else {
                    crate::vm::UpdateMode::Postfix
                };
                self.emit(Instr::IncLocal(*slot, p, mode), span);
            } else {
                // Void: load, subtract, plain SetLocal (no Dup, no postfix
                // recovery). The value is consumed by SetLocal.
                self.emit(Instr::Local(*slot), span);
                self.emit(Instr::Push(p), span);
                self.emit(Instr::Sub, span);
                self.emit(Instr::SetLocal(*slot), span);
            }
            return;
        }

        // Non-local targets (member/index): load-sub-store path.
        self.lvalue_emit_addr(&lv, span);
        self.lvalue_emit_load(&lv, span);
        self.emit(Instr::Push(p), span);
        self.emit(Instr::Sub, span);
        if value_needed {
            let mode = if u.prefix { SetMode::New } else { SetMode::Old };
            match &lv {
                LValue::Member(_, field) => {
                    self.emit(Instr::ObjSet(field.clone(), mode), span);
                }
                LValue::Index(..) => {
                    self.emit(Instr::IndexSet(mode), span);
                }
                LValue::Local(_) => unreachable!("handled above"),
            }
        } else {
            self.lvalue_emit_store_void(&lv, span);
        }
    }

    // ── lvalue infrastructure ────────────────────────────────────────────

    /// Resolve a (non-destructuring) assignment target to an [`LValue`], or emit
    /// an error and return `None`. A `const`/`state` write is rejected here.
    fn lvalue_from_target<'r, 'a>(
        &mut self,
        target: &'r ast::AssignmentTarget<'a>,
    ) -> Option<LValue<'r, 'a>> {
        match target {
            ast::AssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.lvalue_for_identifier(id.name.as_str(), id.span.start)
            }
            ast::AssignmentTarget::StaticMemberExpression(m) => Some(LValue::Member(
                &m.object,
                m.property.name.as_str().to_string(),
            )),
            ast::AssignmentTarget::ComputedMemberExpression(m) => {
                Some(LValue::Index(&m.object, &m.expression))
            }
            other => {
                self.error(other.span().start, "unsupported assignment target");
                None
            }
        }
    }

    /// Like [`lvalue_from_target`], but for the `SimpleAssignmentTarget` of an
    /// update expression (`++`/`--`).
    fn lvalue_from_simple_target<'r, 'a>(
        &mut self,
        target: &'r ast::SimpleAssignmentTarget<'a>,
    ) -> Option<LValue<'r, 'a>> {
        match target {
            ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.lvalue_for_identifier(id.name.as_str(), id.span.start)
            }
            ast::SimpleAssignmentTarget::StaticMemberExpression(m) => Some(LValue::Member(
                &m.object,
                m.property.name.as_str().to_string(),
            )),
            ast::SimpleAssignmentTarget::ComputedMemberExpression(m) => {
                Some(LValue::Index(&m.object, &m.expression))
            }
            other => {
                self.error(other.span().start, "unsupported assignment target");
                None
            }
        }
    }

    /// Resolve an identifier write target: a local slot, or an error for
    /// `const`/`state`/undeclared names.
    fn lvalue_for_identifier<'r, 'a>(&mut self, name: &str, span: u32) -> Option<LValue<'r, 'a>> {
        match self.ref_slot(span) {
            Some(r) => {
                if r.is_const {
                    self.error(span, format!("assignment to constant `{name}`"));
                }
                Some(LValue::Local(r.slot))
            }
            None if name == "state" => {
                self.error(span, "cannot reassign the blessed `state` object");
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
    fn lvalue_addr_depth(&self, lv: &LValue<'_, '_>) -> usize {
        match lv {
            LValue::Local(_) => 0,
            LValue::Member(..) => 1,
            LValue::Index(..) => 2,
        }
    }

    /// Push the lvalue's address operands (the object, and key for an index) in
    /// JS evaluation order. A local has no address.
    fn lvalue_emit_addr(&mut self, lv: &LValue<'_, '_>, _span: u32) {
        match lv {
            LValue::Local(_) => {}
            LValue::Member(obj, _) => self.compile_expr(obj),
            LValue::Index(obj, key) => {
                self.compile_expr(obj);
                self.compile_expr(key);
            }
        }
    }

    /// With the address already on the stack, push the lvalue's current value
    /// **without** consuming the address (so a store can follow). Uses `Pick` to
    /// copy the buried object/key for the read.
    fn lvalue_emit_load(&mut self, lv: &LValue<'_, '_>, span: u32) {
        match lv {
            LValue::Local(slot) => self.emit(Instr::Local(*slot), span),
            LValue::Member(_, field) => {
                self.emit(Instr::Dup, span); // copy the object
                self.emit(Instr::ObjGet(field.clone()), span);
            }
            LValue::Index(..) => {
                self.emit(Instr::Pick(1), span); // copy the object
                self.emit(Instr::Pick(1), span); // copy the key
                self.emit(Instr::IndexGet, span);
            }
        }
    }

    /// With `[address…, value]` on the stack, store `value` into the lvalue and
    /// leave it on the stack (assignment is an expression). For locals, uses
    /// `TeeLocal` (the one-instruction equivalent of `Dup; SetLocal`).
    fn lvalue_emit_store(&mut self, lv: &LValue<'_, '_>, span: u32) {
        match lv {
            LValue::Local(slot) => {
                self.emit(Instr::TeeLocal(*slot), span);
            }
            LValue::Member(_, field) => self.emit(Instr::ObjSet(field.clone(), SetMode::New), span),
            LValue::Index(..) => self.emit(Instr::IndexSet(SetMode::New), span),
        }
    }

    /// Like [`lvalue_emit_store`], but for void context (the caller does NOT
    /// need the resulting value). For locals, uses plain `SetLocal` (consumes
    /// the value, pushing nothing). For non-locals, `ObjSet`/`IndexSet` always
    /// leave the value — emit a `Pop(1)` to discard it.
    fn lvalue_emit_store_void(&mut self, lv: &LValue<'_, '_>, span: u32) {
        match lv {
            LValue::Local(slot) => {
                self.emit(Instr::SetLocal(*slot), span);
            }
            LValue::Member(_, field) => {
                self.emit(Instr::ObjSet(field.clone(), SetMode::New), span);
                self.emit(Instr::Pop(1), span);
            }
            LValue::Index(..) => {
                self.emit(Instr::IndexSet(SetMode::New), span);
                self.emit(Instr::Pop(1), span);
            }
        }
    }

    /// Remove `n` values sitting directly below the top of the stack, leaving the
    /// top in place. Uses `Nip(n)` (one instruction) rather than `Swap`+`Pop`
    /// pairs.
    fn emit_drop_below_top(&mut self, n: usize, span: u32) {
        if n > 0 {
            self.emit(Instr::Nip(n), span);
        }
    }

    // ── destructuring assignment ─────────────────────────────────────────

    /// Destructure the source value on top of the stack into an assignment
    /// pattern, **consuming** it. Leaves are existing assignment targets; Phase 2
    /// supports identifier leaves (member/index leaves and rest are errors).
    fn destructure_assign(&mut self, target: &ast::AssignmentTarget, span: u32) {
        match target {
            ast::AssignmentTarget::ArrayAssignmentTarget(arr) => {
                if arr.rest.is_some() {
                    self.error(
                        arr.span.start,
                        "rest elements in destructuring are not supported",
                    );
                }
                for (i, el) in arr.elements.iter().enumerate() {
                    if let Some(el) = el {
                        self.emit(Instr::Dup, span);
                        self.emit(Instr::Push(StackValue::PosInt(i as u64)), span);
                        self.emit(Instr::IndexGet, span);
                        self.assign_maybe_default(el, span);
                    }
                }
                self.emit(Instr::Pop(1), span);
            }
            ast::AssignmentTarget::ObjectAssignmentTarget(obj) => {
                if obj.rest.is_some() {
                    self.error(
                        obj.span.start,
                        "rest elements in destructuring are not supported",
                    );
                }
                for prop in &obj.properties {
                    match prop {
                        ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                            // Shorthand `{a}` / `{a = d}`: the key and the target
                            // are the same identifier.
                            self.emit(Instr::Dup, span);
                            self.emit(Instr::ObjGet(p.binding.name.as_str().to_string()), span);
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
                            self.emit(Instr::Dup, span);
                            self.emit_property_key_access(&p.name, p.computed, span);
                            self.assign_maybe_default(&p.binding, span);
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
    fn assign_maybe_default(&mut self, m: &ast::AssignmentTargetMaybeDefault, span: u32) {
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
    fn assign_target_leaf(&mut self, target: &ast::AssignmentTarget, span: u32) {
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
    fn assign_to_identifier(&mut self, name: &str, id_span: u32, span: u32) {
        match self.ref_slot(id_span) {
            Some(r) => {
                if r.is_const {
                    self.error(id_span, format!("assignment to constant `{name}`"));
                }
                self.emit(Instr::SetLocal(r.slot), span);
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

    // ── calls / intrinsics ───────────────────────────────────────────────

    /// Calls are recognized structurally (the VM has no method objects): a
    /// `namespace.method(...)` static intrinsic, a `recv.method(...)` array/
    /// string method, or a global function like `String(x)`. Each lowers to a
    /// dedicated instruction; user functions, `tools.*`, and `raise` arrive in
    /// later phases.
    fn compile_call(&mut self, call: &ast::CallExpression) {
        let span = call.span.start;
        // Collect non-spread argument expressions (spread clashes with the VM's
        // strict arity).
        let mut argv: Vec<&ast::Expression> = Vec::with_capacity(call.arguments.len());
        for arg in &call.arguments {
            match arg.as_expression() {
                Some(e) => argv.push(e),
                None => {
                    self.error(arg.span().start, "spread arguments are not supported");
                    return;
                }
            }
        }

        // `call.optional` is the `?.()` token *on the callee value* (`f?.()`,
        // `state.fn?.()`, `Math.max?.(…)`) — distinct from `obj?.method()`
        // (handled below as an optional member).
        if call.optional {
            // Static-call reclaim: a constant, non-nullish callee makes the `?.`
            // guard provably dead — `Math.max?.(a, b)` is identical to
            // `Math.max(a, b)`. Emit the static `CallBuiltin` and skip the
            // guard/`CallDyn`. (First-class builtin refs are the only constant
            // callables today; named function refs join them in Phase 3.)
            if let ast::Expression::StaticMemberExpression(m) = &call.callee {
                if let ast::Expression::Identifier(obj) = &m.object {
                    if namespace_builtin(obj.name.as_str(), m.property.name.as_str()).is_some() {
                        return self.compile_namespace_call(
                            obj.name.as_str(),
                            m.property.name.as_str(),
                            &argv,
                            span,
                        );
                    }
                }
            }
            // Otherwise the callee is a genuine runtime value: evaluate it,
            // short-circuit to undefined when nullish (args skipped), else
            // dynamically invoke it.
            self.compile_expr(&call.callee);
            let end = self.begin_optional(span);
            self.compile_args(&argv);
            if !argv.is_empty() {
                // The callee sits below its args; bring it back to the top where
                // `CallDyn` expects it.
                self.emit(Instr::Dig(argv.len()), span);
            }
            self.emit(Instr::CallDyn(argv.len() as u32), span);
            self.emit(Instr::Label(end), span);
            return;
        }

        match &call.callee {
            ast::Expression::StaticMemberExpression(m) => {
                let method = m.property.name.as_str();
                // A leading identifier matching a reserved namespace is a static
                // intrinsic; otherwise it is a method on the receiver value.
                if let ast::Expression::Identifier(obj) = &m.object {
                    match obj.name.as_str() {
                        "Math" | "Object" | "JSON" | "Number" | "Array" => {
                            return self.compile_namespace_call(
                                obj.name.as_str(),
                                method,
                                &argv,
                                span,
                            );
                        }
                        "tools" => {
                            // `tools.foo(a, b)` → `Invoke("foo", 2)`. Recognized
                            // structurally; `tools` is valid only as the receiver
                            // of such a call (bare `tools` and `tools.foo` without
                            // a call are undeclared-identifier errors elsewhere).
                            // An optional member (`tools?.foo()`) is meaningless —
                            // `tools` always exists — so it lowers identically.
                            self.compile_args(&argv);
                            self.emit(Instr::Invoke(method.to_string(), argv.len() as u32), span);
                            return;
                        }
                        _ => {}
                    }
                }
                self.compile_method_call(&m.object, method, &argv, span, m.optional);
            }
            ast::Expression::ComputedMemberExpression(_) => self.error(
                span,
                "computed method calls (`obj[expr](...)`) are not supported",
            ),
            ast::Expression::Identifier(id) => {
                self.compile_user_call(id.name.as_str(), id.span.start, &argv, span)
            }
            other => self.error(other.span().start, "unsupported call target"),
        }
    }

    /// Compile all argument expressions left-to-right.
    fn compile_args(&mut self, argv: &[&ast::Expression]) {
        for &e in argv {
            self.compile_expr(e);
        }
    }

    /// Validate an exact arity, recording a diagnostic if it doesn't match.
    fn arity(&mut self, argv: &[&ast::Expression], want: usize, span: u32, name: &str) -> bool {
        if argv.len() == want {
            true
        } else {
            self.error(
                span,
                format!("`{name}` expects {want} argument(s), got {}", argv.len()),
            );
            false
        }
    }

    /// Compile a call to `builtin`: evaluate the receiver (if any) and the
    /// arguments, then emit `CallBuiltin` — but only after validating the
    /// argument count against `Builtin::meta()`, which is the single source of
    /// truth for both the accepted arity and the builtin's display name.
    ///
    /// `recv` is the method receiver (`None` for free/static builtins); it is
    /// arg 0 and counts toward `meta()`'s bounds. On an arity mismatch a
    /// diagnostic is recorded and nothing is emitted.
    ///
    /// `optional` lowers `recv?.method(args)`: when the receiver is nullish the
    /// whole call short-circuits to `undefined` and the arguments are **not**
    /// evaluated (the guard sits between the receiver and the arguments). Only
    /// meaningful with a receiver.
    fn compile_builtin_call(
        &mut self,
        builtin: Builtin,
        recv: Option<&ast::Expression>,
        argv: &[&ast::Expression],
        span: u32,
        optional: bool,
    ) {
        let base = recv.is_some() as u32; // the receiver occupies one arity slot
        let argc = base + argv.len() as u32;
        let meta = builtin.meta();
        if argc < meta.min_args || argc > meta.max_args {
            // Report the bounds without the implicit receiver, so the message
            // matches how the call is written in source.
            let lo = meta.min_args.saturating_sub(base);
            let want = if meta.max_args == u32::MAX {
                format!("at least {lo}")
            } else {
                let hi = meta.max_args - base;
                if lo == hi {
                    format!("{lo}")
                } else {
                    format!("{lo} to {hi}")
                }
            };
            self.error(
                span,
                format!(
                    "`{}` expects {want} argument(s), got {}",
                    meta.name,
                    argv.len()
                ),
            );
            return;
        }
        // Optional method call: guard on the receiver before the args/call. The
        // peeking `JNotNullish` (via `begin_optional`) keeps the receiver on the
        // not-nullish path for the call to consume.
        let end = match (recv, optional) {
            (Some(recv), true) => {
                self.compile_expr(recv);
                Some(self.begin_optional(span))
            }
            (Some(recv), false) => {
                self.compile_expr(recv);
                None
            }
            (None, _) => None,
        };
        self.compile_args(argv);
        self.emit(Instr::CallBuiltin(builtin, argc), span);
        if let Some(end) = end {
            self.emit(Instr::Label(end), span);
        }
    }

    /// Compile a namespaced static call (`Math.max(…)`, `JSON.parse(…)`, …) by
    /// looking the receiver-less builtin up in [`namespace_builtin`] — the same
    /// map that backs first-class references like `Math.sqrt` used as a value.
    fn compile_namespace_call(
        &mut self,
        ns: &str,
        method: &str,
        argv: &[&ast::Expression],
        span: u32,
    ) {
        match namespace_builtin(ns, method) {
            Some(builtin) => self.compile_builtin_call(builtin, None, argv, span, false),
            None => self.error(span, format!("unsupported `{ns}.{method}`")),
        }
    }

    /// Global function calls recognized structurally.
    fn compile_global_call(&mut self, name: &str, argv: &[&ast::Expression], span: u32) {
        match name {
            "String" => {
                if !self.arity(argv, 1, span, "String") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::ToStr, span);
            }
            "Number" => {
                if !self.arity(argv, 1, span, "Number") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::ToNum, span);
            }
            "Boolean" => {
                if !self.arity(argv, 1, span, "Boolean") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::ToBool, span);
            }
            "raise" => {
                // `raise("...")` → `Raise(String)`. The `Raise` instruction
                // carries a compile-time string, so the argument must be a
                // string literal. `raise(...)` is an expression: it leaves one
                // value (the host pushes the resumed value back on the stack),
                // satisfying the one-value-per-expression invariant.
                if !self.arity(argv, 1, span, "raise") {
                    return;
                }
                match argv[0] {
                    ast::Expression::StringLiteral(lit) => {
                        self.emit(Instr::Raise(lit.value.as_str().to_string()), span);
                    }
                    other => self.error(
                        other.span().start,
                        "`raise` requires a string-literal argument",
                    ),
                }
            }
            _ => self.error(
                span,
                format!("call to undeclared function `{name}` (user functions are Phase 3)"),
            ),
        }
    }

    /// Array/string methods on a receiver value. Dispatch is purely syntactic
    /// (name + arity) and assumes the conventional receiver type; a mismatch is
    /// a runtime `TypeError`. `optional` is the `recv?.method(...)` case: a
    /// nullish receiver short-circuits the call to `undefined`.
    fn compile_method_call(
        &mut self,
        recv: &ast::Expression,
        method: &str,
        argv: &[&ast::Expression],
        span: u32,
        optional: bool,
    ) {
        let builtin = match method {
            // ── array methods ─────────────────────────────────────────
            "push" => Builtin::ArrayPush,
            "unshift" => Builtin::ArrayUnshift,
            "pop" => Builtin::ArrayPop,
            "shift" => Builtin::ArrayShift,
            "join" => Builtin::ArrayJoin,
            // ── string methods ────────────────────────────────────────
            "split" => Builtin::StrSplit,
            "includes" => Builtin::StrIncludes,
            "indexOf" => Builtin::StrIndexOf,
            "lastIndexOf" => Builtin::StrLastIndexOf,
            "startsWith" => Builtin::StrStartsWith,
            "endsWith" => Builtin::StrEndsWith,
            "slice" => Builtin::StrSlice,
            "trim" => Builtin::StrTrim,
            // ── higher-order array methods (prelude helpers) ──────────
            "map" => return self.compile_hof(recv, argv, span, optional, "__map", 1),
            "filter" => return self.compile_hof(recv, argv, span, optional, "__filter", 1),
            "forEach" => return self.compile_hof(recv, argv, span, optional, "__forEach", 1),
            "some" => return self.compile_hof(recv, argv, span, optional, "__some", 1),
            "every" => return self.compile_hof(recv, argv, span, optional, "__every", 1),
            "find" => return self.compile_hof(recv, argv, span, optional, "__find", 1),
            "findIndex" => return self.compile_hof(recv, argv, span, optional, "__findIndex", 1),
            "reduce" => return self.compile_reduce(recv, argv, span, optional),
            _ => {
                // Not a known builtin method — treat as property access
                // followed by dynamic call (e.g. `state.add5(3)` where
                // add5 is a function stored in state).
                self.compile_dynamic_method_call(recv, method, argv, span, optional);
                return;
            }
        };
        // The receiver is arg 0 and counts toward arity; bounds come from
        // `meta()`. The variadic-default cases (e.g. `join` with no separator)
        // are handled by the builtin itself based on the received `argc`.
        self.compile_builtin_call(builtin, Some(recv), argv, span, optional);
    }

    /// Compile a method call where the method name is not a known builtin.
    /// Lowers `recv.method(args)` to: evaluate recv, get property `method`,
    /// evaluate args, then CallDyn.
    fn compile_dynamic_method_call(
        &mut self,
        recv: &ast::Expression,
        method: &str,
        argv: &[&ast::Expression],
        span: u32,
        optional: bool,
    ) {
        // Evaluate the receiver.
        self.compile_expr(recv);

        if optional {
            // Optional call: guard on the receiver before reading the property.
            let end = self.begin_optional(span);
            // Get the property from the non-nullish receiver.
            self.emit(Instr::ObjGet(method.to_string()), span);
            // Evaluate args.
            self.compile_args(argv);
            let argc = argv.len();
            if argc > 0 {
                self.emit(Instr::Dig(argc), span);
            }
            self.emit(Instr::CallDyn(argc as u32), span);
            self.emit(Instr::Label(end), span);
        } else {
            // Get the property (consumes receiver, pushes property value).
            self.emit(Instr::ObjGet(method.to_string()), span);
            // Evaluate args.
            self.compile_args(argv);
            let argc = argv.len();
            if argc > 0 {
                // The callee sits below the args; Dig brings it to the top
                // where CallDyn expects it.
                self.emit(Instr::Dig(argc), span);
            }
            self.emit(Instr::CallDyn(argc as u32), span);
        }
    }

    // ── Phase 4.0: higher-order array methods (prelude) ──────────────

    /// Lower a higher-order array method (`arr.map(cb)`, `arr.filter(cb)`, …) to
    /// a static `Call` of its prelude helper. `helper` is the helper's function
    /// name (`"__map"`); `want_cb` is the number of callback arguments the call
    /// site must supply (the receiver is added implicitly as the helper's first
    /// parameter). The helper is self-contained (no captures), so a static
    /// `Call` is always valid.
    fn compile_hof(
        &mut self,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: u32,
        optional: bool,
        helper: &str,
        want_cb: usize,
    ) {
        if argv.len() != want_cb {
            self.error(
                span,
                format!(
                    "`{}` expects {want_cb} argument(s), got {}",
                    &helper[2..],
                    argv.len()
                ),
            );
            return;
        }
        self.emit_prelude_call(helper, recv, argv, span, optional);
    }

    /// `arr.reduce(cb[, init])`. The two JS forms map to two helpers: with an
    /// initial value → `__reduce(a, f, acc)`; without → `__reduce1(a, f)`
    /// (seeded from element 0).
    fn compile_reduce(
        &mut self,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: u32,
        optional: bool,
    ) {
        match argv.len() {
            2 => self.emit_prelude_call("__reduce", recv, argv, span, optional),
            1 => self.emit_prelude_call("__reduce1", recv, argv, span, optional),
            n => self.error(
                span,
                format!("`reduce` expects 1 or 2 argument(s), got {n}"),
            ),
        }
    }

    /// Emit a static call to a prelude helper: evaluate the receiver (the
    /// helper's first parameter), then the remaining args, then
    /// `Call(helper_label, 1 + argv.len())`. `optional` (`arr?.map(cb)`) guards
    /// the receiver — a nullish receiver short-circuits to `undefined`, skipping
    /// the args and the call.
    fn emit_prelude_call(
        &mut self,
        helper: &str,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: u32,
        optional: bool,
    ) {
        let Some(label) = self.find_root_callee_label(helper) else {
            // The prelude assembler appends a helper whenever its method appears
            // in source, so a missing label is an internal inconsistency.
            self.error(
                span,
                format!("internal error: prelude helper `{helper}` is unavailable"),
            );
            return;
        };
        let arity = 1 + argv.len() as u32; // receiver + callback (+ init)
        self.compile_expr(recv);
        let end = if optional {
            Some(self.begin_optional(span))
        } else {
            None
        };
        self.compile_args(argv);
        self.emit(Instr::Call(label, arity), span);
        if let Some(end) = end {
            self.emit(Instr::Label(end), span);
        }
    }

    /// Find the entry label of a top-level (root-scope) function declaration by
    /// name. Used to resolve prelude helpers, which are always declared at the
    /// top level regardless of where the call site is.
    fn find_root_callee_label(&self, name: &str) -> Option<u32> {
        let analysis = self.analysis.as_ref().expect("analysis present");
        let root = &analysis.scopes[analysis.root];
        for &child_id in &root.children {
            let child = &analysis.scopes[child_id];
            if child.is_declaration && child.self_name.as_deref() == Some(name) {
                return Some(child.label);
            }
        }
        None
    }

    // ── Phase 3: function codegen ────────────────────────────────────

    /// Call to a user-defined function identified by a bare name. If the name
    /// resolves to a local binding, emit a static `Call` (when we know the
    /// label) or `CallDyn`. Otherwise fall through to the built-in global call
    /// path (`String`, `Number`, `Boolean`, `raise`).
    fn compile_user_call(
        &mut self,
        name: &str,
        callee_span: u32,
        argv: &[&ast::Expression],
        span: u32,
    ) {
        if let Some(r) = self.ref_slot(callee_span) {
            // Try to resolve to a static `Call`. If the function was declared
            // in this scope and has NO captures, we can use a static Call.
            // Functions with captures must use CallDyn so the VM installs
            // the upvals as leading locals.
            match self.find_callee_label(name) {
                Some(l) if !self.function_has_captures(name) => {
                    // Static call: push args and Call. Pad with Undefined when
                    // the caller passes fewer args than the function declares, so
                    // `arg_count` reaches the declared arity. When the caller
                    // passes *more* args than declared params, pass the larger
                    // count so the surplus stays reachable via `arguments` (the
                    // prologue `EnterFrame` then normalizes the slots to nparams).
                    let expected_arity = self.function_arity(name);
                    self.compile_args(argv);
                    let passed = argv.len() as u32;
                    for _ in passed..expected_arity {
                        self.emit(Instr::Push(StackValue::Undefined), span);
                    }
                    self.emit(Instr::Call(l, passed.max(expected_arity)), span);
                }
                _ => {
                    // Dynamic call: push args, load callee, CallDyn (installs
                    // upvals for captured/closure callees).
                    self.compile_args(argv);
                    self.emit(Instr::Local(r.slot), span);
                    self.emit(Instr::CallDyn(argv.len() as u32), span);
                }
            }
            return;
        }

        // Not a local — try global/built-in.
        self.compile_global_call(name, argv, span)
    }

    /// Check whether a named function in the current scope has captures.
    fn function_has_captures(&self, name: &str) -> bool {
        let analysis = self.analysis.as_ref().expect("analysis present");
        let scope = &analysis.scopes[self.current_scope];
        for &child_id in &scope.children {
            let child = &analysis.scopes[child_id];
            if child.is_declaration && child.self_name.as_deref() == Some(name) {
                return !child.captures.is_empty();
            }
        }
        false
    }

    /// Find the entry label of a function named `name` declared in the current
    /// scope. Returns `None` if not statically known.
    fn find_callee_label(&self, name: &str) -> Option<u32> {
        let analysis = self.analysis.as_ref().expect("analysis present");
        let scope = &analysis.scopes[self.current_scope];
        for &child_id in &scope.children {
            let child = &analysis.scopes[child_id];
            if child.is_declaration && child.self_name.as_deref() == Some(name) {
                return Some(child.label);
            }
        }
        None
    }

    /// Get the declared parameter count of a function named `name` in the
    /// current scope. Returns 0 if not found.
    fn function_arity(&self, name: &str) -> u32 {
        let analysis = self.analysis.as_ref().expect("analysis present");
        let scope = &analysis.scopes[self.current_scope];
        for &child_id in &scope.children {
            let child = &analysis.scopes[child_id];
            if child.is_declaration && child.self_name.as_deref() == Some(name) {
                return child.params.len() as u32;
            }
        }
        0
    }

    /// Hoist function declarations in the current scope's prologue: emit each
    /// declaration's binding value (`Fn`/closure) into its slot. Recurses
    /// through blocks/conditionals/loops (function declarations hoist to the
    /// enclosing function), but not into nested functions.
    fn hoist_function_decls(&mut self, stmts: &[ast::Statement]) {
        for stmt in stmts {
            self.hoist_function_decl_in_stmt(stmt);
        }
    }

    fn hoist_function_decl_in_stmt(&mut self, stmt: &ast::Statement) {
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
                if let Some(id) = &f.id {
                    if let Some(slot) = self.binding_slot(id.span.start) {
                        let span = f.span.start;
                        if captures.is_empty() {
                            self.emit(Instr::Push(StackValue::Fn(label)), span);
                        } else {
                            self.emit(Instr::MakeClosure(label, captures), span);
                        }
                        self.emit(Instr::SetLocal(slot), span);
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
            _ => {}
        }
    }

    /// Emit the body of a function declaration (its binding was already emitted
    /// in the prologue by `hoist_function_decls`).
    fn compile_function_decl_body(&mut self, f: &ast::Function) {
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
    fn compile_function_expr(&mut self, func: &ast::Function, span: u32) {
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
    fn compile_arrow_expr(&mut self, arrow: &ast::ArrowFunctionExpression, span: u32) {
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

    /// Push a function value: a bare `Fn` when it captures nothing, else a
    /// `MakeClosure` over its capture list.
    fn emit_closure_value(&mut self, scope_id: usize, span: u32) {
        let (label, captures) = {
            let analysis = self.analysis.as_ref().expect("analysis present");
            let child = &analysis.scopes[scope_id];
            (child.label, child.captures.clone())
        };
        if captures.is_empty() {
            self.emit(Instr::Push(StackValue::Fn(label)), span);
        } else {
            self.emit(Instr::MakeClosure(label, captures), span);
        }
    }

    /// Emit a function body: jump-over guard, entry label, prologue
    /// (`EnterFrame`, param defaults / captured-param boxing, self-reference),
    /// body statements, implicit `Return`. Called after the function value has
    /// been pushed (expressions)
    /// or at the declaration site. `is_expression_body` (arrow `=> expr`)
    /// suppresses the trailing implicit `return undefined`. All slots/kinds come
    /// from analysis; there is no codegen-side scope state to set up.
    fn emit_function_def(
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
            )
        };
        let nparams = params_info.len() as u32;

        let prev_scope = self.current_scope;
        self.current_scope = scope_id;

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
        if self_name.is_some() {
            local_kinds.push(SlotKind::Plain);
        }
        self.emit(
            Instr::EnterFrame(nparams, uses_arguments, local_kinds),
            span,
        );

        // Per-parameter prologue: apply defaults (the arg is already in the slot)
        // and box captured params in place. Plain params with no default need no
        // code — their value is already in the local slot.
        for (p_idx, param_info) in params_info.iter().enumerate() {
            let slot = p_idx as u32; // params occupy slots 0..nparams
            let needs_box = matches!(slot_kinds.get(p_idx).copied(), Some(SlotKind::Boxed));
            let default_expr = params.items[p_idx].initializer.as_ref().map(|v| &**v);
            self.emit_param_setup(slot, needs_box, param_info.has_default, default_expr, span);
        }

        // Self-reference (named function expression / recursive declaration): the
        // slot was allocated by EnterFrame above; fill it with the bare `Fn`.
        if self_name.is_some() {
            let self_slot = frame_abs(own_local_count, nparams, upval_count);
            self.emit(Instr::Push(StackValue::Fn(label)), span);
            self.emit(Instr::SetLocal(self_slot), span);
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
            self.emit(Instr::Push(StackValue::Undefined), span);
            self.emit(Instr::Return(1), span);
        }

        self.emit(Instr::Label(after), span);
        self.current_scope = prev_scope;
    }

    /// Emit per-parameter prologue code. The argument value is already in the
    /// param's local `slot` (placed in-frame by `EnterFrame`), so:
    ///   - with a default: if the slot is `undefined`, replace it with the
    ///     default expression's value;
    ///   - if captured (`needs_box`): box the slot in place with `FreshCell`
    ///     (Plain value → fresh cell), so closures capture it by reference.
    /// A plain param with no default needs no code at all.
    fn emit_param_setup(
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
                self.emit(Instr::Local(slot), span);
                self.emit(Instr::Push(StackValue::Undefined), span);
                self.emit(Instr::Eq, span);
                self.emit(Instr::JFalse(skip_default), span);
                self.compile_expr(default);
                self.emit(Instr::SetLocal(slot), span);
                self.emit(Instr::Label(skip_default), span);
            }
        }
        if needs_box {
            // Promote the plain arg value in the slot to a shared cell.
            self.emit(Instr::FreshCell(slot), span);
        }
    }
}

/// Map a reserved namespace + method to its receiver-less `Builtin`, if any.
/// Single source of truth for both static calls (`Math.max(…)`) and first-class
/// references (`Math.sqrt` used as a value / callback). Method builtins that
/// need a receiver (`push`, `slice`, …) are intentionally absent — they are not
/// first-class without binding.
fn namespace_builtin(ns: &str, method: &str) -> Option<Builtin> {
    Some(match (ns, method) {
        ("Math", "max") => Builtin::MathMax,
        ("Math", "min") => Builtin::MathMin,
        ("Math", "pow") => Builtin::MathPow,
        ("Math", "abs") => Builtin::MathAbs,
        ("Math", "sqrt") => Builtin::MathSqrt,
        ("Math", "floor") => Builtin::MathFloor,
        ("Math", "ceil") => Builtin::MathCeil,
        ("Math", "round") => Builtin::MathRound,
        ("Math", "sign") => Builtin::MathSign,
        ("Object", "keys") => Builtin::ObjKeys,
        ("Object", "values") => Builtin::ObjValues,
        ("JSON", "parse") => Builtin::JSONParse,
        ("JSON", "stringify") => Builtin::JSONStringify,
        ("Number", "isInteger") => Builtin::NumberIsInteger,
        ("Number", "parseInt") => Builtin::NumberParseInt,
        ("Number", "parseFloat") => Builtin::NumberParseFloat,
        ("Array", "isArray") => Builtin::ArrayIsArray,
        _ => return None,
    })
}

/// Canonicalize a non-negative numeric literal: an integer in `u64` range
/// becomes a `PosInt`, otherwise a `Number`. Literals are non-negative; unary
/// minus is a separate operator folded via `f64_to_value`.
fn number_literal_to_value(value: f64) -> StackValue {
    if value.fract() == 0.0 && value >= 0.0 && value <= u64::MAX as f64 {
        StackValue::PosInt(value as u64)
    } else {
        StackValue::Number(value)
    }
}

/// Canonicalize an arbitrary (possibly negative) f64 into the VM's integer
/// variants when it is integral and in range, mirroring serde_json's split:
/// non-negative → `PosInt`, negative → `NegInt`, otherwise `Number`.
fn f64_to_value(value: f64) -> StackValue {
    if value.fract() == 0.0 && value >= 0.0 && value <= u64::MAX as f64 {
        StackValue::PosInt(value as u64)
    } else if value.fract() == 0.0 && value < 0.0 && value >= i64::MIN as f64 {
        StackValue::NegInt(value as i64)
    } else {
        StackValue::Number(value)
    }
}

/// Render a numeric object-literal key the way JS does (`{1: …}` → key "1",
/// `{1.5: …}` → "1.5"), so it matches the string form computed access produces.
fn number_key_to_string(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

/// Strip `Label` markers and rewrite every label-id address into a real code
/// offset, copying spans in lockstep so the table stays aligned with the
/// compacted code. Single linear pass after a first scan that records each
/// label's offset.
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
            Instr::Push(StackValue::Fn(l)) => Instr::Push(StackValue::Fn(label_offset[l as usize])),
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
    use crate::vm::{StepResult, VM};

    /// Run a compiled program to completion via `for_program`, returning the
    /// finished VM so the heap/stack can be inspected.
    fn run_program(prog: Program) -> VM {
        let mut vm = VM::for_program(prog.code, serde_json::Value::Null).unwrap();
        loop {
            match vm.step().unwrap() {
                StepResult::Done => return vm,
                other => panic!("unexpected effect: {other:?}"),
            }
        }
    }

    #[test]
    fn roundtrip_literal_arithmetic() {
        // `1 + 2 * 3` lowers to: push the three literals, multiply 2*3, add,
        // then the expression-statement Pop and the top-level Return(0).
        let prog = compile("1 + 2 * 3;").expect("compiles");
        assert_eq!(
            prog.code,
            vec![
                Instr::Push(StackValue::PosInt(1)),
                Instr::Push(StackValue::PosInt(2)),
                Instr::Push(StackValue::PosInt(3)),
                Instr::Mul,
                Instr::Add,
                Instr::Pop(1),
                Instr::Return(0),
            ]
        );
        // spans stay in lockstep with code.
        assert_eq!(prog.spans.len(), prog.code.len());

        // And it executes cleanly through for_program (heap[0] = state), ending
        // with an empty stack after the expression value is popped.
        let vm = run_program(prog);
        assert!(vm.stack.is_empty());
    }

    #[test]
    fn for_program_seeds_state_at_heap0() {
        // The blessed `state` object always lives at heap[0], even when seeded.
        let prog = compile("1;").expect("compiles");
        let state = serde_json::json!({ "count": 7 });
        let vm = VM::for_program(prog.code, state).unwrap();
        match &vm.heap[0] {
            crate::vm::HeapValue::Object(o) => {
                assert_eq!(o.get("count"), Some(&StackValue::PosInt(7)));
            }
            other => panic!("expected state object at heap[0], got {other:?}"),
        }
    }

    #[test]
    fn unsupported_statement_errors() {
        // An out-of-scope statement still produces a rendered diagnostic.
        let errs = compile("class C {}").expect_err("should not compile");
        assert_eq!(errs.len(), 1);
        // Renders as line:col with a caret.
        let rendered = errs[0].render("class C {}");
        assert!(rendered.starts_with("1:1: "), "got: {rendered}");
    }

    #[test]
    fn syntax_error_is_reported() {
        // oxc's own syntax errors are surfaced as Diagnostics.
        let errs = compile("1 +* 2;").expect_err("syntax error");
        assert!(!errs.is_empty());
    }

    // ── Phase 1: expressions ───────────────────────────────────────────
    //
    // Most expression behavior is exercised end-to-end: compile a program that
    // writes its result into `state.r`, run it, then read `heap[0]["r"]`. This
    // routes every expression through the real VM and the `state`/`Ptr(0)`
    // lowering at once.

    use crate::vm::HeapValue;

    /// Compile + run `src` to completion, returning the finished VM.
    fn run_vm(src: &str) -> VM {
        match compile(src) {
            Ok(prog) => run_program(prog),
            Err(errs) => panic!("compile failed: {:?}", errs[0].render(src)),
        }
    }

    /// Read `state.<key>` (a slot of the heap[0] object) from a finished VM.
    fn state_val(vm: &VM, key: &str) -> StackValue {
        match &vm.heap[0] {
            HeapValue::Object(o) => *o.get(key).unwrap_or_else(|| panic!("no state.{key}")),
            other => panic!("state is not an object: {other:?}"),
        }
    }

    /// Evaluate a single expression by assigning it to `state.r`, returning the
    /// resulting `StackValue`.
    fn eval(expr: &str) -> StackValue {
        let vm = run_vm(&format!("state.r = ({expr});"));
        state_val(&vm, "r")
    }

    /// Like `eval`, but resolves the result heap string to an owned `String`.
    fn eval_str(expr: &str) -> String {
        let vm = run_vm(&format!("state.r = ({expr});"));
        match state_val(&vm, "r") {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                HeapValue::String(s) => s.clone(),
                other => panic!("not a string: {other:?}"),
            },
            other => panic!("not a pointer: {other:?}"),
        }
    }

    fn num(v: f64) -> StackValue {
        StackValue::Number(v)
    }

    #[test]
    fn literals() {
        assert_eq!(eval("42"), StackValue::PosInt(42));
        assert_eq!(eval("-7"), StackValue::NegInt(-7)); // folded literal
        assert_eq!(eval("3.5"), num(3.5));
        assert_eq!(eval("true"), StackValue::Bool(true));
        assert_eq!(eval("null"), StackValue::Null);
        assert_eq!(eval("undefined"), StackValue::Undefined);
        assert_eq!(eval_str("\"hi\""), "hi");
        assert!(matches!(eval("NaN"), StackValue::Number(n) if n.is_nan()));
        assert!(matches!(eval("Infinity"), StackValue::Number(n) if n.is_infinite()));
    }

    #[test]
    fn arithmetic_and_operators() {
        assert_eq!(eval("1 + 2 * 3"), num(7.0));
        assert_eq!(eval("(1 + 2) * 3"), num(9.0));
        assert_eq!(eval("10 % 3"), num(1.0));
        assert_eq!(eval("2 ** 10"), num(1024.0));
        assert_eq!(eval("7 & 3"), num(3.0));
        assert_eq!(eval("1 << 4"), num(16.0));
        assert_eq!(eval("-5"), StackValue::NegInt(-5));
        assert_eq!(eval("+\"42\""), num(42.0)); // unary plus ToNumber
        assert_eq!(eval("!0"), StackValue::Bool(true));
        assert_eq!(eval("~0"), num(-1.0));
        assert_eq!(eval_str("\"a\" + \"b\""), "ab");
    }

    #[test]
    fn comparisons_and_equality() {
        assert_eq!(eval("1 < 2"), StackValue::Bool(true));
        assert_eq!(eval("2 <= 2"), StackValue::Bool(true));
        assert_eq!(eval("3 === 3"), StackValue::Bool(true));
        assert_eq!(eval("3 !== 4"), StackValue::Bool(true));
        assert_eq!(eval("1 == \"1\""), StackValue::Bool(true)); // loose
        assert_eq!(eval("1 === \"1\""), StackValue::Bool(false)); // strict
        assert_eq!(eval("null == undefined"), StackValue::Bool(true));
    }

    #[test]
    fn short_circuit_logical() {
        assert_eq!(eval("0 && 5"), StackValue::PosInt(0));
        assert_eq!(eval("3 && 5"), StackValue::PosInt(5));
        assert_eq!(eval("0 || 5"), StackValue::PosInt(5));
        assert_eq!(eval("3 || 5"), StackValue::PosInt(3));
        assert_eq!(eval("null ?? 5"), StackValue::PosInt(5));
        assert_eq!(eval("0 ?? 5"), StackValue::PosInt(0)); // 0 is not nullish
        assert_eq!(eval("undefined ?? 9"), StackValue::PosInt(9));
    }

    #[test]
    fn short_circuit_does_not_evaluate_rhs() {
        // The RHS assignment must NOT run when the LHS short-circuits.
        let vm = run_vm("state.hit = 0; state.r = false && (state.hit = 1);");
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(false));
        assert_eq!(state_val(&vm, "hit"), StackValue::PosInt(0));

        let vm = run_vm("state.hit = 0; state.r = true || (state.hit = 1);");
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(true));
        assert_eq!(state_val(&vm, "hit"), StackValue::PosInt(0));
    }

    #[test]
    fn ternary() {
        assert_eq!(eval("1 ? 10 : 20"), StackValue::PosInt(10));
        assert_eq!(eval("0 ? 10 : 20"), StackValue::PosInt(20));
    }

    #[test]
    fn typeof_op() {
        assert_eq!(eval_str("typeof 5"), "number");
        assert_eq!(eval_str("typeof \"x\""), "string");
        assert_eq!(eval_str("typeof true"), "boolean");
        assert_eq!(eval_str("typeof undefined"), "undefined");
        assert_eq!(eval_str("typeof null"), "object");
        assert_eq!(eval_str("typeof [1]"), "object");
    }

    #[test]
    fn template_literals() {
        let vm = run_vm("state.name = \"bob\"; state.r = `hi ${state.name}, ${1 + 2}!`;");
        match state_val(&vm, "r") {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                HeapValue::String(s) => assert_eq!(s, "hi bob, 3!"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn arrays_and_objects() {
        // Array literal, length, index read.
        assert_eq!(eval("[10, 20, 30].length"), num(3.0));
        assert_eq!(eval("[10, 20, 30][1]"), StackValue::PosInt(20));
        assert_eq!(eval("[10, 20][5]"), StackValue::Undefined); // OOB read
        // Object literal + member read (static and computed).
        assert_eq!(eval("({ a: 1, b: 2 }).b"), StackValue::PosInt(2));
        assert_eq!(eval("({ a: 1, b: 2 })[\"a\"]"), StackValue::PosInt(1));
        assert_eq!(eval("({ a: 1 }).missing"), StackValue::Undefined);
        // Numeric key.
        assert_eq!(eval("({ 1: \"x\" })[1]"), eval("\"x\""));
    }

    #[test]
    fn member_and_index_assignment() {
        // Static member assignment leaves the value and mutates the object.
        let vm = run_vm("state.obj = { a: 1 }; state.r = (state.obj.a = 9);");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(9));
        match state_val(&vm, "obj") {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                HeapValue::Object(o) => assert_eq!(o.get("a"), Some(&StackValue::PosInt(9))),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        // Index assignment into an array.
        let vm = run_vm("state.arr = [1, 2, 3]; state.arr[0] = 99; state.r = state.arr[0];");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(99));
        // Index assignment into an object (string key coercion).
        let vm = run_vm("state.o = {}; state.o[\"k\"] = 7; state.r = state.o.k;");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(7));
    }

    #[test]
    fn optional_chaining() {
        // Missing base short-circuits to undefined; present base reads through.
        assert_eq!(eval("state.nope?.x"), StackValue::Undefined);
        let vm = run_vm("state.obj = { x: 7 }; state.r = state.obj?.x;");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(7));
        // A fully-optional chain short-circuits across links.
        assert_eq!(eval("state.nope?.a?.b"), StackValue::Undefined);
    }

    #[test]
    fn optional_method_calls() {
        // Present receiver: the method runs normally (push returns new length).
        let vm = run_vm("state.arr = [1]; state.r = state.arr?.push(2);");
        assert_eq!(state_val(&vm, "r"), num(2.0));
        let vm = run_vm("state.arr = [1]; state.arr?.push(2); state.r = state.arr.length;");
        assert_eq!(state_val(&vm, "r"), num(2.0));

        // Nullish receiver: the whole call short-circuits to undefined.
        let vm = run_vm("state.r = state.nope?.push(2);");
        assert_eq!(state_val(&vm, "r"), StackValue::Undefined);

        // Short-circuit must NOT evaluate the arguments.
        let vm = run_vm("state.hit = 0; state.r = state.nope?.push(state.hit = 1);");
        assert_eq!(state_val(&vm, "r"), StackValue::Undefined);
        assert_eq!(state_val(&vm, "hit"), StackValue::PosInt(0));

        // String methods take the same optional path.
        let vm = run_vm("state.s = \"a,b,c\"; state.r = state.s?.split(\",\").length;");
        assert_eq!(state_val(&vm, "r"), num(3.0));
    }

    #[test]
    fn first_class_builtin_refs() {
        // A namespaced builtin used as a value is a callable `Builtin`.
        assert_eq!(eval_str("typeof Math.sqrt"), "function");
    }

    #[test]
    fn optional_invocation_calls() {
        // `?.()` on a real callable invokes it (via first-class builtin ref).
        assert_eq!(eval("Math.max?.(3, 7)"), num(7.0));
        assert_eq!(eval("Math.sqrt?.(9)"), num(3.0));

        // Stored builtin value, retrieved and optionally invoked.
        let vm = run_vm("state.f = Math.sqrt; state.r = state.f?.(16);");
        assert_eq!(state_val(&vm, "r"), num(4.0));

        // Nullish callee short-circuits to undefined.
        assert_eq!(eval("state.nope?.()"), StackValue::Undefined);

        // Short-circuit must NOT evaluate the arguments.
        let vm = run_vm("state.hit = 0; state.r = state.nope?.(state.hit = 1);");
        assert_eq!(state_val(&vm, "r"), StackValue::Undefined);
        assert_eq!(state_val(&vm, "hit"), StackValue::PosInt(0));

        // A present-but-non-callable callee is a runtime TypeError, like JS.
        let prog = compile("state.x = 5; state.x?.();").expect("compiles");
        let mut vm = VM::for_program(prog.code, serde_json::Value::Null).unwrap();
        let err = loop {
            match vm.step() {
                Ok(StepResult::Done) => panic!("expected a runtime error"),
                Ok(_) => continue,
                Err(e) => break e,
            }
        };
        assert!(matches!(err, crate::vm::VMError::TypeError), "got: {err:?}");
    }

    #[test]
    fn optional_call_reclaims_static_builtin() {
        // A constant non-nullish callee makes the `?.` guard dead, so
        // `Math.max?.(…)` reclaims the static `CallBuiltin` — identical to
        // `Math.max(…)`, with no `JNotNullish`/`CallDyn`.
        let prog = compile("Math.max?.(3, 7);").expect("compiles");
        assert!(
            prog.code
                .iter()
                .any(|i| matches!(i, Instr::CallBuiltin(Builtin::MathMax, 2))),
            "expected CallBuiltin(MathMax, 2), got {:?}",
            prog.code
        );
        assert!(
            !prog
                .code
                .iter()
                .any(|i| matches!(i, Instr::CallDyn(_) | Instr::JNotNullish(_))),
            "guard/CallDyn should have been reclaimed: {:?}",
            prog.code
        );
        // It still computes the right answer.
        assert_eq!(eval("Math.max?.(3, 7)"), num(7.0));
    }

    #[test]
    fn in_and_delete() {
        let vm = run_vm("state.o = { a: 1 }; state.r = (\"a\" in state.o);");
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(true));
        let vm = run_vm("state.o = { a: 1 }; state.r = (\"b\" in state.o);");
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(false));
        // delete removes the key and returns whether it existed.
        let vm = run_vm(
            "state.o = { a: 1 }; state.r = delete state.o.a; state.had = (\"a\" in state.o);",
        );
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(true));
        assert_eq!(state_val(&vm, "had"), StackValue::Bool(false));
    }

    #[test]
    fn intrinsics_static() {
        assert_eq!(eval("Math.max(3, 7)"), num(7.0));
        assert_eq!(eval("Math.min(3, 7)"), num(3.0));
        assert_eq!(eval("Math.abs(-5)"), num(5.0));
        assert_eq!(eval("Math.floor(3.9)"), num(3.0));
        assert_eq!(eval("Math.pow(2, 5)"), num(32.0));
        assert_eq!(eval("Object.keys({ a: 1, b: 2 }).length"), num(2.0));
        assert_eq!(eval("Object.values({ a: 5 })[0]"), StackValue::PosInt(5));
        assert_eq!(eval("JSON.parse(\"[1,2,3]\").length"), num(3.0));
        assert_eq!(eval_str("JSON.stringify([1,2])"), "[1,2]");
        assert_eq!(eval("Number.isInteger(4)"), StackValue::Bool(true));
        assert_eq!(eval("Array.isArray([1])"), StackValue::Bool(true));
        assert_eq!(eval("Array.isArray(5)"), StackValue::Bool(false));
    }

    #[test]
    fn intrinsics_global() {
        assert_eq!(eval_str("String(5)"), "5");
        assert_eq!(eval("Number(\"42\")"), num(42.0));
        assert_eq!(eval("Boolean(0)"), StackValue::Bool(false));
        assert_eq!(eval("Boolean(\"x\")"), StackValue::Bool(true));
    }

    #[test]
    fn intrinsics_methods() {
        assert_eq!(eval("\"a,b,c\".split(\",\").length"), num(3.0));
        assert_eq!(eval("\"a,b,c\".split(\",\", 2).length"), num(2.0));
        assert_eq!(eval("\"hello\".includes(\"ell\")"), StackValue::Bool(true));
        assert_eq!(eval("\"hello\".startsWith(\"he\")"), StackValue::Bool(true));
        assert_eq!(eval("\"hello\".endsWith(\"lo\")"), StackValue::Bool(true));
        assert_eq!(eval("\"hello\".indexOf(\"l\")"), num(2.0));
        assert_eq!(eval_str("\"hello\".slice(1, 3)"), "el");
        assert_eq!(eval_str("\"  hi  \".trim()"), "hi");
        assert_eq!(eval_str("[\"a\", \"b\"].join(\"-\")"), "a-b");
        assert_eq!(eval_str("[1, 2].join()"), "1,2"); // default separator
        // Array mutators run and mutate the receiver.
        let vm = run_vm("state.arr = [1]; state.arr.push(2); state.r = state.arr.length;");
        assert_eq!(state_val(&vm, "r"), num(2.0));
        let vm = run_vm("state.arr = [1, 2, 3]; state.r = state.arr.pop();");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(3));
    }

    #[test]
    fn diagnostics_for_unsupported() {
        // These all live in later phases / out of scope and must error cleanly.
        for src in [
            "x;",           // undeclared variable
            "x = 1;",       // assignment to undeclared variable
            "i++;",         // update of undeclared variable
            "tools;",       // bare `tools` is not a value
            "tools.send;",  // `tools.send` without a call
            "raise(x);",    // raise with a non-literal argument
            "raise();",     // raise with no argument
            "Math.tan(1);", // unsupported intrinsic
            "Math.pow(1);", // wrong arity (needs exactly 2)
            "f(...args);",  // spread arg
            "new Foo();",   // new
            "class C {}",   // class statement
        ] {
            assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
        }
    }

    #[test]
    fn builtin_arity_is_enforced_from_meta() {
        // Wrong arities are rejected at compile time, with the accepted range
        // and the builtin name sourced from `Builtin::meta()`.
        for src in [
            "Math.pow(1);",              // needs exactly 2
            "Math.pow(1, 2, 3);",        // too many
            "Math.abs();",               // needs 1
            "\"x\".slice();",            // needs 1..2 args after receiver
            "\"x\".slice(1, 2, 3);",     // too many
            "[1].push();",               // needs 1
            "[1].pop(2);",               // needs 0
            "Object.keys();",            // needs 1
            "Number.parseInt(1, 2, 3);", // needs 1..2
        ] {
            assert!(
                compile(src).is_err(),
                "expected `{src}` to fail arity check"
            );
        }

        // The diagnostic names the builtin and reports the receiver-free bounds.
        let errs = compile("\"x\".slice(1, 2, 3);").expect_err("too many args");
        let msg = &errs[0].message;
        assert!(msg.contains("`slice`"), "got: {msg}");
        assert!(msg.contains("1 to 2"), "got: {msg}");

        // Variadic `min`/`max` accept any count, including zero.
        assert_eq!(eval("Math.max()"), num(f64::NEG_INFINITY));
        assert_eq!(eval("Math.max(1, 2, 3, 4, 5)"), num(5.0));
    }

    #[test]
    fn state_is_ptr_zero() {
        // Bare `state` is the heap[0] object pointer; the whole bag round-trips.
        let vm = run_vm("state.a = 1; state.r = JSON.stringify(state);");
        match state_val(&vm, "r") {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                // r was set last, so it appears in the serialized object too.
                HeapValue::String(s) => assert!(s.contains("\"a\":1"), "got {s}"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    // ── Phase 4: effects (tools / raise) ────────────────────────────────

    #[test]
    fn tools_call_lowers_to_invoke() {
        // `tools.foo(a, b)` lowers to args-then-`Invoke("foo", 2)`.
        let prog = compile("tools.notify(1, 2);").expect("compiles");
        assert!(
            prog.code.contains(&Instr::Invoke("notify".to_string(), 2)),
            "expected Invoke in {:?}",
            prog.code
        );
    }

    #[test]
    fn tools_call_yields_invoke_effect() {
        // End-to-end: a `tools.*` call yields an `Invoke` effect carrying the
        // method name and the evaluated args; the host pushes a result to resume.
        let prog = compile("state.r = tools.add(10, 3);").expect("compiles");
        let mut vm = VM::for_program(prog.code, serde_json::Value::Null).unwrap();
        match vm.step().unwrap() {
            StepResult::Invoke { calls } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "add");
                assert_eq!(
                    calls[0].args,
                    vec![StackValue::PosInt(10), StackValue::PosInt(3)]
                );
            }
            other => panic!("expected Invoke, got {other:?}"),
        }
        // Host resolves the call and pushes the result; the program stores it.
        vm.stack.push(StackValue::PosInt(13));
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                other => panic!("unexpected effect: {other:?}"),
            }
        }
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(13));
    }

    #[test]
    fn tools_call_with_no_args() {
        let prog = compile("tools.tick();").expect("compiles");
        assert!(prog.code.contains(&Instr::Invoke("tick".to_string(), 0)));
    }

    #[test]
    fn raise_lowers_to_raise_instr() {
        let prog = compile("raise(\"need_input\");").expect("compiles");
        assert!(
            prog.code.contains(&Instr::Raise("need_input".to_string())),
            "expected Raise in {:?}",
            prog.code
        );
    }

    #[test]
    fn raise_yields_effect_and_resumes_as_expression() {
        // `raise(...)` is an expression: it yields a `Raise` effect, then the
        // host pushes the resumed value which the program consumes.
        let prog = compile("state.r = raise(\"pick_a_number\");").expect("compiles");
        let mut vm = VM::for_program(prog.code, serde_json::Value::Null).unwrap();
        match vm.step().unwrap() {
            StepResult::Raise { condition } => assert_eq!(condition, "pick_a_number"),
            other => panic!("expected Raise, got {other:?}"),
        }
        // Resume restart: advance past the Raise and push the resumed value.
        vm.ip += 1;
        vm.stack.push(StackValue::PosInt(42));
        loop {
            match vm.step().unwrap() {
                StepResult::Done => break,
                other => panic!("unexpected effect: {other:?}"),
            }
        }
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(42));
    }

    // ── Phase 2: statements / control flow ──────────────────────────────

    #[test]
    fn local_declarations_and_reassignment() {
        assert_eq!(eval_phase2("let x = 5; return x;"), StackValue::PosInt(5));
        assert_eq!(eval_phase2("const x = 7; return x;"), StackValue::PosInt(7));
        assert_eq!(
            eval_phase2("let x = 1; x = 2; return x;"),
            StackValue::PosInt(2)
        );
        // Uninitialized local is `undefined`.
        assert_eq!(eval_phase2("let x; return x;"), StackValue::Undefined);
        // Multiple declarators in one statement.
        assert_eq!(eval_phase2("let a = 1, b = 2; return a + b;"), num(3.0));
    }

    #[test]
    fn block_scoping() {
        // An inner block shadows; the outer binding is restored after.
        let vm = run_vm("let x = 1; { let x = 2; state.inner = x; } state.outer = x;");
        assert_eq!(state_val(&vm, "inner"), StackValue::PosInt(2));
        assert_eq!(state_val(&vm, "outer"), StackValue::PosInt(1));
    }

    #[test]
    fn var_is_function_scoped_and_hoisted() {
        // `var` is visible (as undefined) before its declaration runs.
        let vm = run_vm("state.before = typeof x; var x = 5; state.after = x;");
        assert_eq!(eval_str_in(&vm, "before"), "undefined");
        assert_eq!(state_val(&vm, "after"), StackValue::PosInt(5));
        // A `var` in a block belongs to the function scope.
        assert_eq!(
            eval_phase2("{ var y = 9; } return y;"),
            StackValue::PosInt(9)
        );
    }

    #[test]
    fn if_else() {
        assert_eq!(
            eval_phase2("let r; if (1 > 0) r = 10; else r = 20; return r;"),
            StackValue::PosInt(10)
        );
        assert_eq!(
            eval_phase2("let r; if (0) r = 10; else r = 20; return r;"),
            StackValue::PosInt(20)
        );
        // Dangling-if with no else leaves the prior value.
        assert_eq!(
            eval_phase2("let r = 3; if (false) r = 9; return r;"),
            StackValue::PosInt(3)
        );
        // else-if chains.
        assert_eq!(
            eval_phase2(
                "let x = 2, r; if (x === 1) r = 1; else if (x === 2) r = 2; else r = 3; return r;"
            ),
            StackValue::PosInt(2)
        );
    }

    #[test]
    fn while_loop() {
        assert_eq!(
            eval_phase2("let i = 0, s = 0; while (i < 5) { s += i; i += 1; } return s;"),
            num(10.0)
        );
    }

    #[test]
    fn while_continue_retests() {
        // `continue` in a `while` jumps back to the test (no update clause), so a
        // manual increment before it avoids an infinite loop and `i === 3` skips.
        assert_eq!(
            eval_phase2(
                "let i = 0, s = 0; while (i < 5) { i++; if (i === 3) continue; s += i; } return s;"
            ),
            num(12.0)
        );
    }

    #[test]
    fn for_with_expression_initializer() {
        // The `for` init may be a plain expression (no declaration); `i` is an
        // outer local that the loop mutates.
        assert_eq!(
            eval_phase2("let i, s = 0; for (i = 0; i < 4; i++) { s += i; } return s;"),
            num(6.0)
        );
    }

    #[test]
    fn do_while_loop() {
        // Body always runs at least once, even with a false test.
        assert_eq!(
            eval_phase2("let n = 0; do { n += 1; } while (n < 3); return n;"),
            num(3.0)
        );
        assert_eq!(
            eval_phase2("let n = 0; do { n += 1; } while (false); return n;"),
            num(1.0)
        );
    }

    #[test]
    fn for_loop() {
        assert_eq!(
            eval_phase2("let s = 0; for (let i = 0; i < 5; i++) { s += i; } return s;"),
            num(10.0)
        );
        // Empty clauses: `for (;;)` with an internal break.
        assert_eq!(
            eval_phase2("let i = 0; for (;;) { if (i >= 3) break; i++; } return i;"),
            num(3.0)
        );
    }

    #[test]
    fn break_and_continue() {
        // break stops the loop early.
        assert_eq!(
            eval_phase2(
                "let s = 0; for (let i = 0; i < 10; i++) { if (i === 3) break; s += i; } return s;"
            ),
            num(3.0)
        );
        // continue skips the rest of the body (the for-update still runs).
        assert_eq!(
            eval_phase2(
                "let s = 0; for (let i = 0; i < 5; i++) { if (i % 2 === 0) continue; s += i; } return s;"
            ),
            num(4.0)
        );
        // break only exits the innermost loop.
        assert_eq!(
            eval_phase2(
                "let c = 0; for (let i = 0; i < 3; i++) { for (let j = 0; j < 3; j++) { if (j === 1) break; c++; } } return c;"
            ),
            num(3.0)
        );
    }

    #[test]
    fn for_of_array() {
        // Sum the values of an array.
        assert_eq!(
            eval_phase2("let s = 0; for (const x of [1, 2, 3, 4]) { s += x; } return s;"),
            num(10.0)
        );
        // `let` binding, body without braces.
        assert_eq!(
            eval_phase2("let s = 0; for (let x of [10, 20]) s += x; return s;"),
            num(30.0)
        );
        // Empty array: body never runs (the literal is untouched).
        assert_eq!(
            eval_phase2("let s = 99; for (const x of []) s = 0; return s;"),
            StackValue::PosInt(99)
        );
    }

    #[test]
    fn for_of_string_chars() {
        // for-of over a string yields its characters.
        assert_eq!(
            eval_str_phase2("let r = \"\"; for (const c of \"abc\") r = c + r; return r;"),
            "cba"
        );
    }

    #[test]
    fn for_of_break_and_continue() {
        // break exits early.
        assert_eq!(
            eval_phase2(
                "let s = 0; for (const x of [1, 2, 3, 4]) { if (x === 3) break; s += x; } return s;"
            ),
            num(3.0)
        );
        // continue skips an element.
        assert_eq!(
            eval_phase2(
                "let s = 0; for (const x of [1, 2, 3, 4]) { if (x % 2 === 0) continue; s += x; } return s;"
            ),
            num(4.0)
        );
        // Nested for-of: break exits only the inner loop.
        assert_eq!(
            eval_phase2(
                "let c = 0; for (const i of [1, 2, 3]) { for (const j of [1, 2, 3]) { if (j === 2) break; c++; } } return c;"
            ),
            num(3.0)
        );
    }

    #[test]
    fn for_in_object_keys() {
        // for-in yields the keys (insertion order) of an object.
        let vm = run_vm(
            "state.o = { a: 1, b: 2, c: 3 }; state.r = \"\"; for (const k in state.o) { state.r = state.r + k; }",
        );
        assert_eq!(eval_str_in(&vm, "r"), "abc");
        // Sum the values by indexing back into the object with each key.
        let vm = run_vm(
            "state.o = { a: 1, b: 2, c: 3 }; let s = 0; for (const k in state.o) { s += state.o[k]; } state.r = s;",
        );
        assert_eq!(state_val(&vm, "r"), num(6.0));
    }

    #[test]
    fn for_in_over_state() {
        // for-in over the blessed `state` object enumerates its keys.
        let vm =
            run_vm("state.x = 1; state.y = 2; let n = 0; for (const k in state) n++; state.r = n;");
        assert_eq!(state_val(&vm, "r"), num(2.0));
    }

    #[test]
    fn for_of_in_diagnostics() {
        // Unsupported head forms record a clean diagnostic.
        for src in [
            "for (const [a, b] of [[1, 2]]) {}", // destructuring binding
            "for (x of [1]) {}",                 // bare assignment target (undeclared)
        ] {
            assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
        }
    }

    #[test]
    fn switch_basic_and_fallthrough() {
        // A matching case runs and `break` stops fall-through.
        assert_eq!(
            eval_phase2(
                "let r = 0; switch (2) { case 1: r = 1; break; case 2: r = 2; break; case 3: r = 3; break; } return r;"
            ),
            StackValue::PosInt(2)
        );
        // No break: execution falls through into the next case.
        assert_eq!(
            eval_phase2(
                "let r = 0; switch (1) { case 1: r += 1; case 2: r += 10; break; case 3: r += 100; } return r;"
            ),
            num(11.0)
        );
        // default runs when nothing matches.
        assert_eq!(
            eval_phase2(
                "let r = 0; switch (9) { case 1: r = 1; break; default: r = 42; } return r;"
            ),
            StackValue::PosInt(42)
        );
        // default in the middle, reached by fall-through from a later... actually
        // default is dispatched only when no case matches; here 1 matches.
        assert_eq!(
            eval_phase2(
                "let r = 0; switch (1) { default: r = 42; break; case 1: r = 7; break; } return r;"
            ),
            StackValue::PosInt(7)
        );
        // Strict (===) matching: a string discriminant does not match a number.
        assert_eq!(
            eval_phase2(
                "let r = 0; switch (\"1\") { case 1: r = 1; break; default: r = 2; } return r;"
            ),
            StackValue::PosInt(2)
        );
    }

    #[test]
    fn switch_break_only_continue_escapes() {
        // `break` inside a switch breaks the switch, not the enclosing loop.
        assert_eq!(
            eval_phase2(
                "let s = 0; for (let i = 0; i < 3; i++) { switch (i) { case 1: break; default: s += i; } } return s;"
            ),
            num(2.0) // i=0 (default, +0) and i=2 (default, +2); i=1 breaks the switch
        );
        // `continue` inside a switch continues the enclosing loop.
        assert_eq!(
            eval_phase2(
                "let s = 0; for (let i = 0; i < 4; i++) { switch (i) { case 2: continue; default: break; } s += i; } return s;"
            ),
            num(4.0) // i=2 continues (skips s+=i); 0+1+3 = 4
        );
    }

    #[test]
    fn switch_lexical_decls_share_block() {
        // A `let` in one case is visible (one block) but slot-distinct per name.
        assert_eq!(
            eval_phase2(
                "let r = 0; switch (1) { case 1: { let x = 5; r = x; break; } default: r = 0; } return r;"
            ),
            StackValue::PosInt(5)
        );
    }

    #[test]
    fn switch_continue_outside_loop_errors() {
        // `continue` in a switch with no enclosing loop is an error.
        assert!(compile("switch (1) { case 1: continue; }").is_err());
    }

    // ── Phase 4.0: higher-order array methods (prelude) ─────────────────

    #[test]
    fn hof_map_filter() {
        // map applies the callback to each element.
        let vm = run_vm("state.r = [1, 2, 3].map(x => x * 2);");
        match state_val(&vm, "r") {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                HeapValue::Array(a) => assert_eq!(a.len(), 3),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        // map result summed back via reduce.
        assert_eq!(
            eval_phase2("let a = [1, 2, 3].map(x => x * 2); return a[0] + a[1] + a[2];"),
            num(12.0)
        );
        // filter keeps matching elements.
        assert_eq!(
            eval_phase2("let a = [1, 2, 3, 4].filter(x => x % 2 === 0); return a.length;"),
            num(2.0)
        );
    }

    #[test]
    fn hof_reduce_both_forms() {
        // reduce with an initial value.
        assert_eq!(
            eval_phase2("return [1, 2, 3, 4].reduce((s, x) => s + x, 0);"),
            num(10.0)
        );
        // reduce without an initial value (seeds from element 0).
        assert_eq!(
            eval_phase2("return [1, 2, 3, 4].reduce((s, x) => s + x);"),
            num(10.0)
        );
    }

    #[test]
    fn hof_search_methods() {
        assert_eq!(
            eval_phase2("return [1, 2, 3].some(x => x === 2);"),
            StackValue::Bool(true)
        );
        assert_eq!(
            eval_phase2("return [1, 2, 3].every(x => x > 0);"),
            StackValue::Bool(true)
        );
        assert_eq!(
            eval_phase2("return [1, 2, 3].every(x => x > 1);"),
            StackValue::Bool(false)
        );
        // find returns the matching element (an untouched literal here).
        assert_eq!(
            eval_phase2("return [5, 6, 7].find(x => x > 5);"),
            StackValue::PosInt(6)
        );
        assert_eq!(
            eval_phase2("return [5, 6, 7].findIndex(x => x === 7);"),
            num(2.0)
        );
        // find with no match → undefined; findIndex with no match → -1.
        assert_eq!(
            eval_phase2("return [1, 2].find(x => x > 9);"),
            StackValue::Undefined
        );
        assert_eq!(
            eval_phase2("return [1, 2].findIndex(x => x > 9);"),
            StackValue::NegInt(-1)
        );
    }

    #[test]
    fn hof_foreach_side_effects() {
        // forEach runs the callback for its effects and returns undefined.
        let vm = run_vm("state.sum = 0; [1, 2, 3].forEach(x => { state.sum += x; });");
        assert_eq!(state_val(&vm, "sum"), num(6.0));
    }

    #[test]
    fn hof_callback_index_and_array_args() {
        // The callback receives (element, index, array).
        assert_eq!(
            eval_phase2("return [10, 20, 30].map((x, i) => x + i).reduce((s, x) => s + x, 0);"),
            num(63.0) // (10+0)+(20+1)+(30+2) = 63
        );
    }

    #[test]
    fn hof_closure_callback_captures() {
        // A callback closing over an enclosing local works (CallDyn path).
        assert_eq!(
            eval_phase2("let k = 10; return [1, 2, 3].map(x => x + k).reduce((s, x) => s + x, 0);"),
            num(36.0) // (1+10)+(2+10)+(3+10) = 36
        );
    }

    #[test]
    fn hof_chained_and_nested() {
        // Chained higher-order methods.
        assert_eq!(
            eval_phase2(
                "return [1, 2, 3, 4, 5].filter(x => x % 2 === 1).map(x => x * x).reduce((s, x) => s + x, 0);"
            ),
            num(35.0) // 1 + 9 + 25
        );
    }

    #[test]
    fn hof_inside_user_function() {
        // A higher-order call inside a user function resolves the top-level
        // prelude helper from a nested scope. (Uses `run_vm` directly because
        // the function body has its own `return`.)
        let vm = run_vm(
            "function total(a) { return a.map(x => x + 1).reduce((s, x) => s + x, 0); } state.r = total([1, 2, 3]);",
        );
        assert_eq!(state_val(&vm, "r"), num(9.0)); // 2 + 3 + 4
    }

    #[test]
    fn hof_arity_errors() {
        assert!(compile("[1].map();").is_err()); // needs a callback
        assert!(compile("[1].reduce();").is_err()); // needs 1 or 2 args
    }

    // ── Phase 4: `arguments` ────────────────────────────────────────────

    #[test]
    fn arguments_variadic_sum() {
        // A param-less function reads all of its args through `arguments`.
        let vm = run_vm(
            "function sum() { let t = 0; for (let i = 0; i < arguments.length; i++) { t += arguments[i]; } return t; } state.r = sum(1, 2, 3, 4);",
        );
        assert_eq!(state_val(&vm, "r"), num(10.0));
    }

    #[test]
    fn arguments_beyond_declared_params() {
        // Arguments past the declared parameters are still visible.
        let vm = run_vm("function f(a) { return a + arguments.length; } state.r = f(10, 20, 30);");
        assert_eq!(state_val(&vm, "r"), num(13.0)); // 10 + 3
    }

    #[test]
    fn arguments_is_cached_per_frame() {
        // Two references in the same frame yield the *same* array object
        // (reference-equal under `===`), which only holds if the per-frame
        // cache reuses one build instead of materializing a fresh array each
        // time.
        let vm = run_vm("function f() { return arguments === arguments; } state.r = f(1, 2);");
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(true));
    }

    #[test]
    fn arguments_can_be_shadowed() {
        // A real binding named `arguments` shadows the frame-args array.
        let vm =
            run_vm("function f() { let arguments = 42; return arguments; } state.r = f(1, 2, 3);");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(42));
    }

    #[test]
    fn arguments_in_arrow_is_own_frame() {
        // Accepted divergence from JS (where an arrow inherits the enclosing
        // `arguments`): here an arrow's `arguments` is its own frame's args.
        let vm = run_vm("let f = (a) => arguments.length; state.r = f(1, 2, 3);");
        assert_eq!(state_val(&vm, "r"), num(3.0));
    }

    #[test]
    fn arguments_at_top_level_is_empty() {
        // The root frame has no args, so top-level `arguments` is an empty array.
        let vm = run_vm("state.r = arguments.length;");
        assert_eq!(state_val(&vm, "r"), num(0.0));
    }

    #[test]
    fn hof_bare_builtin_callback() {
        // A namespaced builtin passed directly as the callback works: the helper
        // invokes it with (element, index, array) and the builtin ignores the
        // surplus args (flexible arity from `Builtin::meta`).
        assert_eq!(
            eval_phase2("return [4, 9, 16].map(Math.sqrt).reduce((s, x) => s + x, 0);"),
            num(9.0) // 2 + 3 + 4
        );
    }

    #[test]
    fn compound_assignment() {
        // Local targets.
        assert_eq!(eval_phase2("let x = 5; x += 3; return x;"), num(8.0));
        assert_eq!(eval_phase2("let x = 5; x -= 2; return x;"), num(3.0));
        assert_eq!(eval_phase2("let x = 5; x *= 2; return x;"), num(10.0));
        assert_eq!(eval_phase2("let x = 2; x **= 3; return x;"), num(8.0));
        assert_eq!(eval_phase2("let x = 7; x %= 3; return x;"), num(1.0));
        assert_eq!(eval_phase2("let x = 1; x <<= 3; return x;"), num(8.0));
        // String `+=` concatenates.
        assert_eq!(
            eval_str_phase2("let s = \"a\"; s += \"b\"; return s;"),
            "ab"
        );
        // Member target.
        let vm = run_vm("state.o = { a: 1 }; state.o.a += 4; state.r = state.o.a;");
        assert_eq!(state_val(&vm, "r"), num(5.0));
        // Index target (key evaluated once).
        let vm = run_vm("state.arr = [1, 2]; state.arr[0] += 10; state.r = state.arr[0];");
        assert_eq!(state_val(&vm, "r"), num(11.0));
        // Compound assignment is an expression yielding the new value.
        assert_eq!(eval_phase2("let x = 5; return (x += 5);"), num(10.0));
    }

    #[test]
    fn logical_assignment() {
        assert_eq!(
            eval_phase2("let x = 0; x ||= 5; return x;"),
            StackValue::PosInt(5)
        );
        assert_eq!(
            eval_phase2("let x = 3; x ||= 5; return x;"),
            StackValue::PosInt(3)
        );
        assert_eq!(
            eval_phase2("let x = 3; x &&= 7; return x;"),
            StackValue::PosInt(7)
        );
        assert_eq!(
            eval_phase2("let x = 0; x &&= 7; return x;"),
            StackValue::PosInt(0)
        );
        assert_eq!(
            eval_phase2("let x = null; x ??= 9; return x;"),
            StackValue::PosInt(9)
        );
        assert_eq!(
            eval_phase2("let x = 0; x ??= 9; return x;"),
            StackValue::PosInt(0)
        );

        // Short-circuit must NOT evaluate the RHS (nor store).
        let vm = run_vm("state.hit = 0; let x = 3; x ||= (state.hit = 1); state.r = x;");
        assert_eq!(state_val(&vm, "hit"), StackValue::PosInt(0));
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(3));

        // Member target, store path.
        let vm = run_vm("state.o = { a: null }; state.o.a ??= 5; state.r = state.o.a;");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(5));
        // Member target, keep path (address values cleaned up).
        let vm = run_vm("state.o = { a: 2 }; state.r = (state.o.a ??= 99);");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(2));
        // Index target, keep path.
        let vm = run_vm("state.arr = [7]; state.r = (state.arr[0] ||= 1);");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(7));
    }

    #[test]
    fn increment_decrement() {
        // Postfix returns the old value, prefix the new.
        let vm = run_vm("let x = 5; state.a = x++; state.b = x;");
        assert_eq!(state_val(&vm, "a"), num(5.0));
        assert_eq!(state_val(&vm, "b"), num(6.0));
        let vm = run_vm("let y = 5; state.a = ++y; state.b = y;");
        assert_eq!(state_val(&vm, "a"), num(6.0));
        assert_eq!(state_val(&vm, "b"), num(6.0));
        // Decrement.
        assert_eq!(eval_phase2("let x = 5; x--; return x;"), num(4.0));
        assert_eq!(eval_phase2("let x = 5; return --x;"), num(4.0));
        // `++` coerces like ToNumber (string "5" → 6, not "51").
        assert_eq!(eval_phase2("let x = \"5\"; x++; return x;"), num(6.0));
        // Member / index targets — postsets now preserves the exact old value.
        let vm = run_vm("state.o = { n: 1 }; state.r = state.o.n++; state.after = state.o.n;");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(1));
        assert_eq!(state_val(&vm, "after"), num(2.0));
        let vm = run_vm("state.arr = [10]; state.r = ++state.arr[0]; state.after = state.arr[0];");
        assert_eq!(state_val(&vm, "r"), num(11.0));
        assert_eq!(state_val(&vm, "after"), num(11.0));
    }

    #[test]
    fn array_destructuring_declaration() {
        let vm = run_vm("let [a, b] = [10, 20]; state.a = a; state.b = b;");
        assert_eq!(state_val(&vm, "a"), StackValue::PosInt(10));
        assert_eq!(state_val(&vm, "b"), StackValue::PosInt(20));
        // Holes skip elements.
        assert_eq!(
            eval_phase2("let [, b] = [1, 2]; return b;"),
            StackValue::PosInt(2)
        );
        // Defaults apply only when the element is undefined.
        assert_eq!(
            eval_phase2("let [a = 5] = []; return a;"),
            StackValue::PosInt(5)
        );
        assert_eq!(
            eval_phase2("let [a = 5] = [1]; return a;"),
            StackValue::PosInt(1)
        );
        // Nested.
        let vm = run_vm("let [[a], { b }] = [[1], { b: 2 }]; state.a = a; state.b = b;");
        assert_eq!(state_val(&vm, "a"), StackValue::PosInt(1));
        assert_eq!(state_val(&vm, "b"), StackValue::PosInt(2));
    }

    #[test]
    fn object_destructuring_declaration() {
        let vm = run_vm("let { x, y } = { x: 1, y: 2 }; state.x = x; state.y = y;");
        assert_eq!(state_val(&vm, "x"), StackValue::PosInt(1));
        assert_eq!(state_val(&vm, "y"), StackValue::PosInt(2));
        // Renaming and defaults.
        assert_eq!(
            eval_phase2("let { a: aa } = { a: 7 }; return aa;"),
            StackValue::PosInt(7)
        );
        assert_eq!(
            eval_phase2("let { b = 3 } = {}; return b;"),
            StackValue::PosInt(3)
        );
        assert_eq!(
            eval_phase2("let { b = 3 } = { b: 9 }; return b;"),
            StackValue::PosInt(9)
        );
    }

    #[test]
    fn destructuring_assignment() {
        let vm = run_vm("let a, b; [a, b] = [3, 4]; state.a = a; state.b = b;");
        assert_eq!(state_val(&vm, "a"), StackValue::PosInt(3));
        assert_eq!(state_val(&vm, "b"), StackValue::PosInt(4));
        // Object destructuring assignment needs parens.
        let vm = run_vm("let x, y; ({ x, y } = { x: 5, y: 6 }); state.x = x; state.y = y;");
        assert_eq!(state_val(&vm, "x"), StackValue::PosInt(5));
        assert_eq!(state_val(&vm, "y"), StackValue::PosInt(6));
        // Renamed object target.
        let vm = run_vm("let z; ({ a: z } = { a: 8 }); state.z = z;");
        assert_eq!(state_val(&vm, "z"), StackValue::PosInt(8));
    }

    #[test]
    fn let_without_init_resets_each_iteration() {
        // A bare `let x;` re-initializes to undefined on each loop entry, so a
        // value set only on the first iteration does not leak into the next.
        let vm = run_vm(
            "let last; for (let i = 0; i < 2; i++) { let x; if (i === 0) x = 5; last = x; } state.r = last;",
        );
        assert_eq!(state_val(&vm, "r"), StackValue::Undefined);
    }

    #[test]
    fn phase2_diagnostics() {
        for src in [
            "const x = 1; x = 2;",              // const reassignment
            "const x = 1; x += 1;",             // const compound
            "const x = 1; x++;",                // const update
            "let state = 1;",                   // shadowing blessed `state`
            "y = 1;",                           // assignment to undeclared
            "break;",                           // break outside a loop
            "continue;",                        // continue outside a loop
            "let [a, ...rest] = [1, 2];",       // rest in destructuring
            "outer: while (true) break outer;", // labeled statements
        ] {
            assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
        }
        // Spot-check messages.
        let errs = compile("const x = 1; x = 2;").expect_err("const");
        assert!(
            errs[0].message.contains("constant"),
            "got: {}",
            errs[0].message
        );
        let errs = compile("let [a, ...rest] = [1, 2];").expect_err("rest");
        assert!(errs[0].message.contains("rest"), "got: {}", errs[0].message);
    }

    // ── Phase 2 test helpers ────────────────────────────────────────────

    /// Run a statement sequence ending in `return <expr>;`, rewritten as the
    /// final expression assigned to `state.__ret`, and return that value. Lets
    /// tests read the result of code that uses locals/control flow.
    fn eval_phase2(src: &str) -> StackValue {
        let rewritten = src.replacen("return ", "state.__ret = ", 1);
        let vm = run_vm(&rewritten);
        state_val(&vm, "__ret")
    }

    /// Like [`eval_phase2`], but resolves the heap string result.
    fn eval_str_phase2(src: &str) -> String {
        let rewritten = src.replacen("return ", "state.__ret = ", 1);
        let vm = run_vm(&rewritten);
        match state_val(&vm, "__ret") {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                HeapValue::String(s) => s.clone(),
                other => panic!("not a string: {other:?}"),
            },
            other => panic!("not a pointer: {other:?}"),
        }
    }

    /// Read `state.<key>` as an owned string from a finished VM.
    fn eval_str_in(vm: &VM, key: &str) -> String {
        match state_val(vm, key) {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                HeapValue::String(s) => s.clone(),
                other => panic!("not a string: {other:?}"),
            },
            other => panic!("not a pointer: {other:?}"),
        }
    }

    // ── Phase 3: functions / closures ─────────────────────────────────

    /// Run `src` and return `state.r`. Phase 3: programs can define and call
    /// functions; we wrap the result in a well-known state slot.
    fn eval_phase3(src: &str) -> StackValue {
        let vm = run_vm(src);
        state_val(&vm, "r")
    }

    #[test]
    fn function_declaration_and_call() {
        assert_eq!(
            eval_phase3("function add(a, b) { return a + b; } state.r = add(3, 4);"),
            num(7.0)
        );
    }

    #[test]
    fn function_hoisting_forward_reference() {
        assert_eq!(
            eval_phase3("state.r = add(2, 3); function add(a, b) { return a + b; }"),
            num(5.0)
        );
    }

    #[test]
    fn function_return_without_value() {
        assert_eq!(
            eval_phase3("function f() { return; } state.r = f();"),
            StackValue::Undefined
        );
    }

    #[test]
    fn function_implicit_return() {
        assert_eq!(
            eval_phase3("function f() {} state.r = f();"),
            StackValue::Undefined
        );
    }

    #[test]
    fn parameter_defaults() {
        // Default applied when called without an argument: the compiler
        // pads with Undefined, which triggers the default expression.
        assert_eq!(
            eval_phase3("function f(x = 5) { return x; } state.r = f();"),
            StackValue::PosInt(5)
        );
        assert_eq!(
            eval_phase3("function f(x = 5) { return x; } state.r = f(9);"),
            StackValue::PosInt(9)
        );
    }

    #[test]
    fn function_expression() {
        assert_eq!(
            eval_phase3("let add = function(a, b) { return a + b; }; state.r = add(5, 6);"),
            num(11.0)
        );
    }

    #[test]
    fn arrow_expression_body() {
        // Arrow with expression body implicitly returns.
        assert_eq!(
            eval_phase3("let add = (a, b) => a + b; state.r = add(3, 4);"),
            num(7.0)
        );
    }

    #[test]
    fn arrow_block_body() {
        assert_eq!(
            eval_phase3("let f = (x) => { return x * 2; }; state.r = f(7);"),
            num(14.0)
        );
    }

    #[test]
    fn recursion() {
        assert_eq!(
            eval_phase3(
                "function fact(n) { if (n <= 1) return 1; return n * fact(n - 1); } state.r = fact(5);"
            ),
            num(120.0)
        );
    }

    #[test]
    fn mutual_recursion() {
        assert_eq!(
            eval_phase3(
                "function isEven(n) { if (n === 0) return true; return isOdd(n - 1); } function isOdd(n) { if (n === 0) return false; return isEven(n - 1); } state.r = isEven(4);"
            ),
            StackValue::Bool(true)
        );
    }

    #[test]
    fn closure_captures_local() {
        // Simple closure: inner function captures outer variable by value.
        let vm = run_vm(
            "function makeAdder(x) { return function(y) { return x + y; }; } state.add5 = makeAdder(5); state.r = state.add5(3);",
        );
        assert_eq!(state_val(&vm, "r"), num(8.0));
    }

    #[test]
    fn closure_mutation_visible() {
        let vm = run_vm(
            "function makeCounter() { let count = 0; function inc() { count = count + 1; return count; } return inc; } state.c1 = makeCounter(); state.c1(); state.r = state.c1();",
        );
        assert_eq!(state_val(&vm, "r"), num(2.0));
    }

    // ── per-iteration capture: each loop iteration's closure gets its own cell ──

    #[test]
    fn for_let_head_var_captured_per_iteration() {
        // Closures created in different iterations must capture distinct copies
        // of the for-head `let` variable (classic [0,1,2], not [3,3,3]).
        let vm = run_vm(
            "let fns = []; \
             for (let i = 0; i < 3; i++) { fns.push(() => i); } \
             let a = fns[0], b = fns[1], c = fns[2]; \
             state.r = a() * 100 + b() * 10 + c();",
        );
        assert_eq!(state_val(&vm, "r"), num(12.0)); // 0,1,2
    }

    #[test]
    fn for_body_declared_var_captured_per_iteration() {
        // A captured binding *declared in the body* also needs a fresh cell each
        // iteration, even though the for-head variable isn't captured here.
        let vm = run_vm(
            "let fns = []; \
             for (let i = 0; i < 3; i++) { let j = i * 2; fns.push(() => j); } \
             let a = fns[0], b = fns[1], c = fns[2]; \
             state.r = a() * 100 + b() * 10 + c();",
        );
        assert_eq!(state_val(&vm, "r"), num(24.0)); // 0,2,4
    }

    #[test]
    fn for_of_loop_var_captured_per_iteration() {
        let vm = run_vm(
            "let fns = []; \
             for (const x of [10, 20, 30]) { fns.push(() => x); } \
             let a = fns[0], b = fns[1], c = fns[2]; \
             state.r = a() * 100 + b() * 10 + c();",
        );
        assert_eq!(state_val(&vm, "r"), num(1230.0)); // 10,20,30
    }

    #[test]
    fn for_in_loop_var_captured_per_iteration() {
        let vm = run_vm(
            "let fns = []; let obj = { a: 1, b: 2 }; \
             for (const k in obj) { fns.push(() => k); } \
             let a = fns[0], b = fns[1]; \
             state.r = a() + b();",
        );
        assert_eq!(eval_str_in(&vm, "r"), "ab"); // keys 'a','b', not 'b','b'
    }

    #[test]
    fn while_body_declared_var_captured_per_iteration() {
        let vm = run_vm(
            "let fns = []; let i = 0; \
             while (i < 3) { let j = i; fns.push(() => j); i = i + 1; } \
             let a = fns[0], b = fns[1], c = fns[2]; \
             state.r = a() * 100 + b() * 10 + c();",
        );
        assert_eq!(state_val(&vm, "r"), num(12.0)); // 0,1,2
    }

    #[test]
    fn for_head_var_value_carries_forward() {
        // The fresh per-iteration cell is seeded with the previous iteration's
        // value, so the update (`i++`) and the running total stay correct even
        // with re-boxing.
        let vm =
            run_vm("let sum = 0; for (let i = 0; i < 5; i++) { sum = sum + i; } state.r = sum;");
        assert_eq!(state_val(&vm, "r"), num(10.0)); // 0+1+2+3+4
    }

    #[test]
    fn captured_var_in_loop_is_shared_not_per_iteration() {
        // `var` is function-scoped: a single binding shared across iterations, so
        // all closures observe the final value (3), unlike `let`. Must NOT be
        // re-boxed per iteration.
        let vm = run_vm(
            "let fns = []; \
             for (var i = 0; i < 3; i++) { fns.push(() => i); } \
             let a = fns[0], b = fns[1], c = fns[2]; \
             state.r = a() * 100 + b() * 10 + c();",
        );
        assert_eq!(state_val(&vm, "r"), num(333.0)); // shared `var i` == 3
    }

    #[test]
    fn captured_loop_var_allocates_plain_not_boxed() {
        // The optimization: a captured loop variable is allocated `Plain` (no
        // eager cell in the preamble) and re-boxed per iteration via FreshCell.
        // Here the only captured binding is the loop var `i`, so the prologue
        // `EnterFrame` must contain no `Boxed` slot, yet FreshCell is emitted.
        let prog = compile("let fns = []; for (let i = 0; i < 3; i++) { fns.push(() => i); }")
            .expect("compiles");
        let has_boxed = prog.code.iter().any(
            |i| matches!(i, Instr::EnterFrame(_, _, kinds) if kinds.iter().any(|k| *k == SlotKind::Boxed)),
        );
        assert!(
            !has_boxed,
            "captured loop var should be Plain-allocated: {:?}",
            prog.code
        );
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::FreshCell(_))),
            "captured loop var should still be re-boxed per iteration: {:?}",
            prog.code
        );
    }

    #[test]
    fn plain_loop_var_emits_no_fresh_cell() {
        // A loop variable not captured by any closure stays a Plain slot, so no
        // FreshCell is emitted (per-iteration freshness is unobservable).
        let prog =
            compile("let s = 0; for (let i = 0; i < 3; i++) { s = s + i; }").expect("compiles");
        assert!(
            !prog.code.iter().any(|i| matches!(i, Instr::FreshCell(_))),
            "uncaptured loop var should not emit FreshCell: {:?}",
            prog.code
        );
    }

    #[test]
    fn function_decl_in_block_scope() {
        let vm = run_vm("state.r = foo(); { function foo() { return 9; } }");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(9));
    }

    #[test]
    fn return_must_be_inside_function() {
        let errs = compile("return 1;").expect_err("top-level return should error");
        assert!(
            errs[0].message.contains("return"),
            "got: {}",
            errs[0].message
        );
    }

    #[test]
    fn capture_through_intermediate_function() {
        // `inner` captures `x` from `outer`; `middle` doesn't reference `x` but
        // must still forward the capture down. (Transitive capture — the old
        // immediate-parent-only resolver got this wrong.)
        let vm = run_vm(
            "function outer() { \
               let x = 10; \
               function middle() { \
                 function inner() { return x; } \
                 return inner(); \
               } \
               return middle(); \
             } \
             state.r = outer();",
        );
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(10));
    }

    #[test]
    fn sibling_block_shadowing_uses_distinct_bindings() {
        // The same name `x` in two blocks (and an outer `x`) must resolve to
        // three distinct slots; references resolve per-occurrence by span.
        let vm = run_vm(
            "let x = 1; \
             { let x = 2; state.a = x; } \
             { let x = 3; state.b = x; } \
             state.c = x;",
        );
        assert_eq!(state_val(&vm, "a"), StackValue::PosInt(2));
        assert_eq!(state_val(&vm, "b"), StackValue::PosInt(3));
        assert_eq!(state_val(&vm, "c"), StackValue::PosInt(1));
    }

    #[test]
    fn write_only_capture_is_detected() {
        // `setter` only *writes* the captured `v` (never reads it). Capturing
        // must still happen so `setter` and `getter` share one cell. (The old
        // resolver ignored assignment-target identifiers and missed this.)
        let vm = run_vm(
            "function make() { \
               let v = 0; \
               function setter(n) { v = n; } \
               function getter() { return v; } \
               setter(42); \
               return getter(); \
             } \
             state.r = make();",
        );
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(42));
    }
}
