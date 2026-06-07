//! Compiler — JS source → VM instructions (`vm::Instr`).
//!
//! Compiles a subset of JS into the stack VM in `vm.rs`. Parses with
//! `oxc_parser`, traverses the AST, lowers supported constructs, and emits
//! informative `Diagnostic`s for the rest. See `COMPILER_PLAN.md` for the full
//! design; this file is the Phase 0 skeleton: parsing, the `Compiler` /
//! `Program` / `Diagnostic` types, a label allocator, the backpatch pass, the
//! span table, and codegen for literal + arithmetic expressions.

use std::collections::HashMap;
use std::sync::Arc;

use oxc_allocator::Allocator;
use oxc_ast::ast;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

use crate::builtin::Builtin;
use crate::vm::{Instr, SlotKind, StackValue};

/// A compiled program: the flat instruction stream, a parallel span table
/// (`spans[ip]` = source byte offset of the instruction at `ip`), and the
/// source it was compiled from (for rendering runtime diagnostics).
#[derive(Debug)]
pub struct Program {
    pub code: Vec<Instr>,
    pub spans: Vec<u32>,
    pub source: Arc<str>,
}

/// A compile- or run-time diagnostic anchored at a source byte offset.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    /// Source byte offset the diagnostic points at.
    pub span: u32,
    pub message: String,
}

impl Diagnostic {
    /// Render as `line:col: message` followed by the offending source line and
    /// a caret under the offending column.
    pub fn render(&self, source: &str) -> String {
        let offset = (self.span as usize).min(source.len());
        // Find the start of the line containing `offset`, and the 1-based line
        // number, by scanning newlines up to it.
        let mut line = 1;
        let mut line_start = 0;
        for (i, b) in source.bytes().enumerate() {
            if i >= offset {
                break;
            }
            if b == b'\n' {
                line += 1;
                line_start = i + 1;
            }
        }
        let col = offset - line_start + 1;
        let line_end = source[line_start..]
            .find('\n')
            .map(|p| line_start + p)
            .unwrap_or(source.len());
        let src_line = &source[line_start..line_end];
        let caret = format!("{}^", " ".repeat(offset - line_start));
        format!("{line}:{col}: {}\n{src_line}\n{caret}", self.message)
    }
}

/// Compile JS source into a `Program`. Collects every diagnostic (oxc syntax
/// errors plus our own semantic errors) and returns them all if any exist,
/// rather than producing a partial program.
pub fn compile(source: &str) -> Result<Program, Vec<Diagnostic>> {
    let allocator = Allocator::default();
    let source_type = SourceType::default(); // JavaScript module
    let ret = Parser::new(&allocator, source, source_type).parse();

    let mut compiler = Compiler::new(source);

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

    compiler.compile_program(&ret.program);

    if !compiler.diagnostics.is_empty() {
        return Err(compiler.diagnostics);
    }

    let (code, spans) = backpatch(compiler.code, compiler.spans, compiler.next_label);
    Ok(Program {
        code,
        spans,
        source: Arc::from(source),
    })
}

/// A resolved local-variable binding: its frame slot plus whether it was
/// declared `const` (so writes can be rejected at compile time).
#[derive(Copy, Clone)]
struct SlotInfo {
    slot: u32,
    is_const: bool,
}

/// One entry of the loop-context stack: where `break` and `continue` jump for
/// the innermost enclosing loop. Both carry a label id (resolved in backpatch).
struct LoopCtx {
    break_label: u32,
    continue_label: u32,
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
    /// Lexical scope stack (innermost last). `scopes[0]` is the function/program
    /// scope where `var`s and top-level bindings live; blocks and loop heads push
    /// their own scopes. Block scoping affects *visibility* only — slots are
    /// function-wide and never reused (see the stack-discipline invariants in
    /// COMPILER_PLAN). Phase 2 has no nested functions, so every slot is `Plain`.
    scopes: Vec<HashMap<String, SlotInfo>>,
    /// Monotonic local-slot allocator for the current function.
    next_slot: u32,
    /// Loop-context stack for `break`/`continue` (innermost loop last).
    loops: Vec<LoopCtx>,
    diagnostics: Vec<Diagnostic>,
}

impl<'src> Compiler<'src> {
    fn new(source: &'src str) -> Self {
        Compiler {
            source,
            code: Vec::new(),
            spans: Vec::new(),
            next_label: 0,
            scopes: Vec::new(),
            next_slot: 0,
            loops: Vec::new(),
            diagnostics: Vec::new(),
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

    /// The whole program is the root frame's body. First reserve all of the
    /// frame's locals in one prologue `Alloc` (the stack-discipline invariant:
    /// no temporaries above the locals when `Alloc` runs), with `var`s hoisted
    /// so forward references resolve; then lower each top-level statement; then
    /// `Return(0)` to pop the root frame (→ `StepResult::Done`) and stop
    /// execution falling into any appended function bodies.
    fn compile_program(&mut self, program: &ast::Program) {
        // The program is the sole function scope (Phase 2 has no nested
        // functions). Reserve its locals up front: count every binding (an
        // over-estimate is harmless — extra slots are unused `Undefined`s), emit
        // one `Alloc`, then hoist `var` names so their slots precede the
        // lexical ones and references anywhere in the body resolve.
        self.scopes.push(HashMap::new());
        let slot_count = count_decls_in_stmts(&program.body);
        if slot_count > 0 {
            self.emit(
                Instr::Alloc(vec![SlotKind::Plain; slot_count as usize]),
                program.span.start,
            );
        }
        self.hoist_vars_in_stmts(&program.body);

        for stmt in &program.body {
            self.compile_stmt(stmt);
        }
        self.emit(Instr::Return(0), program.span.end);
    }

    fn compile_stmt(&mut self, stmt: &ast::Statement) {
        match stmt {
            // Every expression statement leaves one value, popped to keep the
            // stack-discipline invariant (one value per expression).
            ast::Statement::ExpressionStatement(es) => {
                self.compile_expr(&es.expression);
                self.emit(Instr::Pop(1), es.span.start);
            }
            ast::Statement::VariableDeclaration(decl) => self.compile_var_decl(decl),
            ast::Statement::BlockStatement(block) => {
                // A block is a fresh lexical scope; slots are function-wide so
                // there is no per-block `Alloc` (only name visibility changes).
                self.scopes.push(HashMap::new());
                for s in &block.body {
                    self.compile_stmt(s);
                }
                self.scopes.pop();
            }
            ast::Statement::EmptyStatement(_) => {}
            ast::Statement::IfStatement(s) => self.compile_if(s),
            ast::Statement::WhileStatement(s) => self.compile_while(s),
            ast::Statement::DoWhileStatement(s) => self.compile_do_while(s),
            ast::Statement::ForStatement(s) => self.compile_for(s),
            ast::Statement::BreakStatement(s) => self.compile_break(s),
            ast::Statement::ContinueStatement(s) => self.compile_continue(s),

            // Later phases / out of scope — informative errors.
            ast::Statement::FunctionDeclaration(f) => self.error(
                f.span.start,
                "function declarations are not supported until Phase 3",
            ),
            ast::Statement::ReturnStatement(r) => self.error(
                r.span.start,
                "`return` outside a function is not supported until Phase 3",
            ),
            ast::Statement::ForOfStatement(s) => {
                self.error(s.span.start, "`for...of` is not supported until Phase 4")
            }
            ast::Statement::ForInStatement(s) => {
                self.error(s.span.start, "`for...in` is not supported until Phase 4")
            }
            ast::Statement::SwitchStatement(s) => {
                self.error(s.span.start, "`switch` is not supported until Phase 4")
            }
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

    /// `let`/`const`/`var` declarations. `var` slots were hoisted in the
    /// prologue (so only the initializer runs here); `let`/`const` slots are
    /// allocated as the declaration is reached. A declaration is a statement, so
    /// nothing is left on the stack.
    fn compile_var_decl(&mut self, decl: &ast::VariableDeclaration) {
        use ast::VariableDeclarationKind as Kind;
        let is_const = decl.kind == Kind::Const;
        let is_var = decl.kind == Kind::Var;
        if matches!(decl.kind, Kind::Using | Kind::AwaitUsing) {
            self.error(decl.span.start, "`using` declarations are not supported");
            return;
        }
        for d in &decl.declarations {
            // Bind names first so a destructuring pattern's slots exist before
            // its extraction code runs (and so `let x = x` resolves `x` to the
            // new binding, matching JS scoping — TDZ aside).
            self.declare_pattern(&d.id, is_const, is_var);
            match &d.id {
                ast::BindingPattern::BindingIdentifier(id) => {
                    let slot = self.resolve_local(id.name.as_str()).map(|i| i.slot);
                    match (&d.init, slot) {
                        (Some(init), Some(slot)) => {
                            self.compile_expr(init);
                            self.emit(Instr::SetLocal(slot), d.span.start);
                        }
                        (None, Some(slot)) if !is_var => {
                            // `let x;` re-initializes to `undefined` each time the
                            // declaration executes (e.g. per loop iteration); a
                            // bare `var x;` is a no-op (already hoisted).
                            self.emit(Instr::Push(StackValue::Undefined), d.span.start);
                            self.emit(Instr::SetLocal(slot), d.span.start);
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

    /// Declare every binding identifier introduced by a pattern. `let`/`const`
    /// get fresh slots in the current scope here; `var` names were already
    /// hoisted into the function scope, so they are left untouched.
    fn declare_pattern(&mut self, pat: &ast::BindingPattern, is_const: bool, is_var: bool) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                if !is_var {
                    self.declare_lexical(id.name.as_str(), id.span.start, is_const);
                }
            }
            ast::BindingPattern::AssignmentPattern(ap) => {
                self.declare_pattern(&ap.left, is_const, is_var)
            }
            ast::BindingPattern::ArrayPattern(arr) => {
                for el in arr.elements.iter().flatten() {
                    self.declare_pattern(el, is_const, is_var);
                }
                if let Some(rest) = &arr.rest {
                    self.declare_pattern(&rest.argument, is_const, is_var);
                }
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.declare_pattern(&prop.value, is_const, is_var);
                }
                if let Some(rest) = &obj.rest {
                    self.declare_pattern(&rest.argument, is_const, is_var);
                }
            }
        }
    }

    /// Destructure the source value already on top of the stack into a binding
    /// pattern, **consuming** that value. Used by declarations; every leaf is a
    /// (already-declared) binding identifier, so leaves lower to `SetLocal`.
    fn destructure_binding(&mut self, pat: &ast::BindingPattern, span: u32) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                match self.resolve_local(id.name.as_str()) {
                    Some(info) => self.emit(Instr::SetLocal(info.slot), span),
                    None => {
                        // Declared moments ago; a miss means an earlier error.
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
                    self.error(arr.span.start, "rest elements in destructuring are not supported");
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
                    self.error(obj.span.start, "rest elements in destructuring are not supported");
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

    // ── scope / binding helpers ──────────────────────────────────────────

    /// Declare a `let`/`const` binding in the current (innermost) scope, giving
    /// it a fresh function-wide slot. `state` is blessed and cannot be shadowed.
    fn declare_lexical(&mut self, name: &str, span: u32, is_const: bool) -> u32 {
        if name == "state" {
            self.error(span, "cannot shadow the blessed `state` object");
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        self.scopes
            .last_mut()
            .expect("a scope is always open during codegen")
            .insert(name.to_string(), SlotInfo { slot, is_const });
        slot
    }

    /// Hoist every `var` binding into the function scope (`scopes[0]`), assigning
    /// each distinct name one slot. Recurses through nested blocks/conditionals/
    /// loops but not into nested functions (there are none in Phase 2). The
    /// initializer assignment itself is emitted later, at the declaration site.
    fn hoist_vars_in_stmts(&mut self, stmts: &[ast::Statement]) {
        for stmt in stmts {
            self.hoist_vars_in_stmt(stmt);
        }
    }

    fn hoist_vars_in_stmt(&mut self, stmt: &ast::Statement) {
        match stmt {
            ast::Statement::VariableDeclaration(decl) => {
                if decl.kind == ast::VariableDeclarationKind::Var {
                    for d in &decl.declarations {
                        self.hoist_var_pattern(&d.id);
                    }
                }
            }
            ast::Statement::BlockStatement(b) => self.hoist_vars_in_stmts(&b.body),
            ast::Statement::IfStatement(s) => {
                self.hoist_vars_in_stmt(&s.consequent);
                if let Some(alt) = &s.alternate {
                    self.hoist_vars_in_stmt(alt);
                }
            }
            ast::Statement::WhileStatement(s) => self.hoist_vars_in_stmt(&s.body),
            ast::Statement::DoWhileStatement(s) => self.hoist_vars_in_stmt(&s.body),
            ast::Statement::ForStatement(s) => {
                if let Some(ast::ForStatementInit::VariableDeclaration(decl)) = &s.init {
                    if decl.kind == ast::VariableDeclarationKind::Var {
                        for d in &decl.declarations {
                            self.hoist_var_pattern(&d.id);
                        }
                    }
                }
                self.hoist_vars_in_stmt(&s.body);
            }
            _ => {}
        }
    }

    fn hoist_var_pattern(&mut self, pat: &ast::BindingPattern) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                let name = id.name.as_str();
                if name == "state" {
                    self.error(id.span.start, "cannot shadow the blessed `state` object");
                    return;
                }
                // A `var` name maps to a single slot even if redeclared.
                if self.scopes[0].contains_key(name) {
                    return;
                }
                let slot = self.next_slot;
                self.next_slot += 1;
                self.scopes[0].insert(
                    name.to_string(),
                    SlotInfo {
                        slot,
                        is_const: false,
                    },
                );
            }
            ast::BindingPattern::AssignmentPattern(ap) => self.hoist_var_pattern(&ap.left),
            ast::BindingPattern::ArrayPattern(arr) => {
                for el in arr.elements.iter().flatten() {
                    self.hoist_var_pattern(el);
                }
                if let Some(rest) = &arr.rest {
                    self.hoist_var_pattern(&rest.argument);
                }
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.hoist_var_pattern(&prop.value);
                }
                if let Some(rest) = &obj.rest {
                    self.hoist_var_pattern(&rest.argument);
                }
            }
        }
    }

    /// Resolve a name to its local binding by searching scopes innermost-first.
    fn resolve_local(&self, name: &str) -> Option<SlotInfo> {
        self.scopes.iter().rev().find_map(|s| s.get(name).copied())
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
            continue_label: top,
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
            continue_label: cont,
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
        // The loop head (`for (let i …)`) is its own scope.
        self.scopes.push(HashMap::new());
        match &s.init {
            Some(ast::ForStatementInit::VariableDeclaration(decl)) => self.compile_var_decl(decl),
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
            continue_label: cont,
        });
        self.compile_stmt(&s.body);
        self.loops.pop();
        // `continue` runs the update, then re-tests.
        self.emit(Instr::Label(cont), span);
        if let Some(update) = &s.update {
            self.compile_expr(update);
            self.emit(Instr::Pop(1), span);
        }
        self.emit(Instr::Jump(top), span);
        self.emit(Instr::Label(end), span);
        self.scopes.pop();
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
        match self.loops.last() {
            Some(ctx) => {
                let target = ctx.continue_label;
                self.emit(Instr::Jump(target), s.span.start);
            }
            None => self.error(s.span.start, "`continue` outside a loop"),
        }
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
            ast::Expression::AssignmentExpression(a) => self.compile_assignment(a),
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

            ast::Expression::UpdateExpression(u) => self.compile_update(u),

            // ── informative errors for out-of-scope nodes ─────────────
            ast::Expression::FunctionExpression(f) => self.error(
                f.span.start,
                "function expressions are not supported until Phase 3",
            ),
            ast::Expression::ArrowFunctionExpression(f) => self.error(
                f.span.start,
                "arrow functions are not supported until Phase 3",
            ),
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
        // A local variable resolves to its frame slot; `Local` dereferences a
        // boxed slot transparently.
        if let Some(info) = self.resolve_local(name) {
            self.emit(Instr::Local(info.slot), span);
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

    /// Assignment is an expression: it leaves the assigned value on the stack.
    /// Plain `=`, compound (`+=` …), and short-circuiting logical (`&&=`/`||=`/
    /// `??=`) assignment all share the [`LValue`] read/write lowering. Array/
    /// object destructuring targets are handled separately.
    fn compile_assignment(&mut self, a: &ast::AssignmentExpression) {
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
                self.emit(Instr::Dup, span); // one copy is the expression result
                self.destructure_assign(&a.left, span);
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
                self.lvalue_emit_store(&lv, span);
            }
            Op::LogicalAnd | Op::LogicalOr | Op::LogicalNullish => {
                self.compile_logical_assign(&lv, a.operator, &a.right, span);
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
                self.lvalue_emit_store(&lv, span);
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
    /// evaluated once. Leaves the resulting value (old on short-circuit, else v).
    fn compile_logical_assign(
        &mut self,
        lv: &LValue<'_, '_>,
        op: ast::AssignmentOperator,
        rhs: &ast::Expression,
        span: u32,
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
        self.lvalue_emit_store(lv, span);
        self.emit(Instr::Jump(end), span);
        // Keep path: old is on top, above any address values — drop those.
        self.emit(Instr::Label(keep), span);
        self.emit_drop_below_top(depth, span);
        self.emit(Instr::Label(end), span);
    }

    /// `++x` / `x++` / `--x` / `x--`. Numeric (forces `ToNumber` via `Sub`): the
    /// new value is `old − p` where `p = -1` for `++` and `+1` for `--`. Prefix
    /// leaves the new value; postfix recovers and leaves the old value as
    /// `new + p` (exact for the integers loop counters use).
    fn compile_update(&mut self, u: &ast::UpdateExpression) {
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
        self.lvalue_emit_addr(&lv, span);
        self.lvalue_emit_load(&lv, span);
        self.emit(Instr::Push(p), span);
        self.emit(Instr::Sub, span);
        self.lvalue_emit_store(&lv, span); // leaves the new value
        if !u.prefix {
            // Postfix: recover the old value (new + p).
            self.emit(Instr::Push(p), span);
            self.emit(Instr::Add, span);
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
            ast::AssignmentTarget::StaticMemberExpression(m) => {
                Some(LValue::Member(&m.object, m.property.name.as_str().to_string()))
            }
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
            ast::SimpleAssignmentTarget::StaticMemberExpression(m) => {
                Some(LValue::Member(&m.object, m.property.name.as_str().to_string()))
            }
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
    fn lvalue_for_identifier<'r, 'a>(
        &mut self,
        name: &str,
        span: u32,
    ) -> Option<LValue<'r, 'a>> {
        match self.resolve_local(name) {
            Some(info) => {
                if info.is_const {
                    self.error(span, format!("assignment to constant `{name}`"));
                }
                Some(LValue::Local(info.slot))
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
    /// leave it on the stack (assignment is an expression).
    fn lvalue_emit_store(&mut self, lv: &LValue<'_, '_>, span: u32) {
        match lv {
            LValue::Local(slot) => {
                self.emit(Instr::Dup, span); // keep a copy as the result
                self.emit(Instr::SetLocal(*slot), span);
            }
            LValue::Member(_, field) => self.emit(Instr::ObjSet(field.clone()), span),
            LValue::Index(..) => self.emit(Instr::IndexSet, span),
        }
    }

    /// Remove `n` values sitting directly below the top of the stack, leaving the
    /// top in place. (`Swap`+`Pop` peels one at a time.)
    fn emit_drop_below_top(&mut self, n: usize, span: u32) {
        for _ in 0..n {
            self.emit(Instr::Swap, span);
            self.emit(Instr::Pop(1), span);
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
                    self.error(arr.span.start, "rest elements in destructuring are not supported");
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
                    self.error(obj.span.start, "rest elements in destructuring are not supported");
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
                            self.assign_to_identifier(p.binding.name.as_str(), p.binding.span.start, span);
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
        match self.resolve_local(name) {
            Some(info) => {
                if info.is_const {
                    self.error(id_span, format!("assignment to constant `{name}`"));
                }
                self.emit(Instr::SetLocal(info.slot), span);
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
                            self.error(span, "`tools.*` calls are not supported until Phase 4");
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
                self.compile_global_call(id.name.as_str(), &argv, span)
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
            "raise" => self.error(span, "`raise` is not supported until Phase 4"),
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
            _ => {
                self.error(span, format!("unsupported method `{method}`"));
                return;
            }
        };
        // The receiver is arg 0 and counts toward arity; bounds come from
        // `meta()`. The variadic-default cases (e.g. `join` with no separator)
        // are handled by the builtin itself based on the received `argc`.
        self.compile_builtin_call(builtin, Some(recv), argv, span, optional);
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

/// Count an upper bound on the local slots a function body needs: one per
/// binding identifier in every `var`/`let`/`const` (and `for`-init)
/// declaration, recursing through nested blocks/conditionals/loops but not into
/// nested functions. Over-counting is harmless — the prologue `Alloc` just
/// reserves a few unused `Undefined` slots — while codegen assigns the actual
/// (deduplicated, ≤ this) slots, so every slot index stays in range.
fn count_decls_in_stmts(stmts: &[ast::Statement]) -> u32 {
    stmts.iter().map(count_decls_in_stmt).sum()
}

fn count_decls_in_stmt(stmt: &ast::Statement) -> u32 {
    match stmt {
        ast::Statement::VariableDeclaration(decl) => {
            decl.declarations.iter().map(|d| count_pattern(&d.id)).sum()
        }
        ast::Statement::BlockStatement(b) => count_decls_in_stmts(&b.body),
        ast::Statement::IfStatement(s) => {
            count_decls_in_stmt(&s.consequent)
                + s.alternate.as_ref().map_or(0, count_decls_in_stmt)
        }
        ast::Statement::WhileStatement(s) => count_decls_in_stmt(&s.body),
        ast::Statement::DoWhileStatement(s) => count_decls_in_stmt(&s.body),
        ast::Statement::ForStatement(s) => {
            let init = match &s.init {
                Some(ast::ForStatementInit::VariableDeclaration(decl)) => {
                    decl.declarations.iter().map(|d| count_pattern(&d.id)).sum()
                }
                _ => 0,
            };
            init + count_decls_in_stmt(&s.body)
        }
        _ => 0,
    }
}

/// Count the binding identifiers a pattern introduces (see [`count_decls_in_stmts`]).
fn count_pattern(pat: &ast::BindingPattern) -> u32 {
    match pat {
        ast::BindingPattern::BindingIdentifier(_) => 1,
        ast::BindingPattern::AssignmentPattern(ap) => count_pattern(&ap.left),
        ast::BindingPattern::ArrayPattern(arr) => {
            arr.elements
                .iter()
                .flatten()
                .map(count_pattern)
                .sum::<u32>()
                + arr.rest.as_ref().map_or(0, |r| count_pattern(&r.argument))
        }
        ast::BindingPattern::ObjectPattern(obj) => {
            obj.properties
                .iter()
                .map(|p| count_pattern(&p.value))
                .sum::<u32>()
                + obj.rest.as_ref().map_or(0, |r| count_pattern(&r.argument))
        }
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
            "x;",             // undeclared variable
            "x = 1;",         // assignment to undeclared variable
            "i++;",           // update of undeclared variable
            "foo(1);",        // undeclared function (Phase 3)
            "tools.send(1);", // tools (Phase 4)
            "raise(\"x\");",  // raise (Phase 4)
            "Math.tan(1);",   // unsupported intrinsic
            "[1, 2].zap();",  // unknown method
            "Math.pow(1);",   // wrong arity (needs exactly 2)
            "f(...args);",    // spread arg
            "new Foo();",     // new
            "class C {}",     // class statement
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

    // ── Phase 2: statements / control flow ──────────────────────────────

    #[test]
    fn local_declarations_and_reassignment() {
        assert_eq!(eval_phase2("let x = 5; return x;"), StackValue::PosInt(5));
        assert_eq!(eval_phase2("const x = 7; return x;"), StackValue::PosInt(7));
        assert_eq!(eval_phase2("let x = 1; x = 2; return x;"), StackValue::PosInt(2));
        // Uninitialized local is `undefined`.
        assert_eq!(eval_phase2("let x; return x;"), StackValue::Undefined);
        // Multiple declarators in one statement.
        assert_eq!(eval_phase2("let a = 1, b = 2; return a + b;"), num(3.0));
    }

    #[test]
    fn block_scoping() {
        // An inner block shadows; the outer binding is restored after.
        let vm = run_vm(
            "let x = 1; { let x = 2; state.inner = x; } state.outer = x;",
        );
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
        assert_eq!(eval_phase2("let r; if (1 > 0) r = 10; else r = 20; return r;"), StackValue::PosInt(10));
        assert_eq!(eval_phase2("let r; if (0) r = 10; else r = 20; return r;"), StackValue::PosInt(20));
        // Dangling-if with no else leaves the prior value.
        assert_eq!(eval_phase2("let r = 3; if (false) r = 9; return r;"), StackValue::PosInt(3));
        // else-if chains.
        assert_eq!(
            eval_phase2("let x = 2, r; if (x === 1) r = 1; else if (x === 2) r = 2; else r = 3; return r;"),
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
            eval_phase2("let s = 0; for (let i = 0; i < 10; i++) { if (i === 3) break; s += i; } return s;"),
            num(3.0)
        );
        // continue skips the rest of the body (the for-update still runs).
        assert_eq!(
            eval_phase2("let s = 0; for (let i = 0; i < 5; i++) { if (i % 2 === 0) continue; s += i; } return s;"),
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
        assert_eq!(eval_phase2("let x = 0; x ||= 5; return x;"), StackValue::PosInt(5));
        assert_eq!(eval_phase2("let x = 3; x ||= 5; return x;"), StackValue::PosInt(3));
        assert_eq!(eval_phase2("let x = 3; x &&= 7; return x;"), StackValue::PosInt(7));
        assert_eq!(eval_phase2("let x = 0; x &&= 7; return x;"), StackValue::PosInt(0));
        assert_eq!(eval_phase2("let x = null; x ??= 9; return x;"), StackValue::PosInt(9));
        assert_eq!(eval_phase2("let x = 0; x ??= 9; return x;"), StackValue::PosInt(0));

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
        // Member / index targets.
        let vm = run_vm("state.o = { n: 1 }; state.r = state.o.n++; state.after = state.o.n;");
        assert_eq!(state_val(&vm, "r"), num(1.0));
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
        assert_eq!(eval_phase2("let [, b] = [1, 2]; return b;"), StackValue::PosInt(2));
        // Defaults apply only when the element is undefined.
        assert_eq!(eval_phase2("let [a = 5] = []; return a;"), StackValue::PosInt(5));
        assert_eq!(eval_phase2("let [a = 5] = [1]; return a;"), StackValue::PosInt(1));
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
        assert_eq!(eval_phase2("let { a: aa } = { a: 7 }; return aa;"), StackValue::PosInt(7));
        assert_eq!(eval_phase2("let { b = 3 } = {}; return b;"), StackValue::PosInt(3));
        assert_eq!(eval_phase2("let { b = 3 } = { b: 9 }; return b;"), StackValue::PosInt(9));
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
            "const x = 1; x = 2;",               // const reassignment
            "const x = 1; x += 1;",              // const compound
            "const x = 1; x++;",                 // const update
            "let state = 1;",                    // shadowing blessed `state`
            "y = 1;",                            // assignment to undeclared
            "break;",                            // break outside a loop
            "continue;",                         // continue outside a loop
            "let [a, ...rest] = [1, 2];",        // rest in destructuring
            "outer: while (true) break outer;",  // labeled statements
        ] {
            assert!(compile(src).is_err(), "expected `{src}` to fail to compile");
        }
        // Spot-check messages.
        let errs = compile("const x = 1; x = 2;").expect_err("const");
        assert!(errs[0].message.contains("constant"), "got: {}", errs[0].message);
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
}
