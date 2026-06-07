//! Compiler — JS source → VM instructions (`vm::Instr`).
//!
//! Compiles a subset of JS into the stack VM in `vm.rs`. Parses with
//! `oxc_parser`, traverses the AST, lowers supported constructs, and emits
//! informative `Diagnostic`s for the rest. See `COMPILER_PLAN.md` for the full
//! design.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use indexmap::IndexMap;
use oxc_allocator::Allocator;
use oxc_ast::ast;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

use crate::builtin::Builtin;
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

// ── Analysis structures (Phase 3: functions / closures) ─────────────

/// Per-parameter analysis info.
#[derive(Debug, Clone)]
struct ParamInfo {
    name: String,
    has_default: bool,
}

/// Pre-computed analysis for one function scope (including the top-level
/// program). The analysis pass walks all nested functions, detects free
/// variables, and determines which slots must be `Boxed` because they are
/// both captured and reassigned.
#[derive(Debug)]
struct FuncScope {
    /// Unique id (index into the `ProgramAnalysis::scopes` vec).
    id: usize,
    /// Parent scope id (`usize::MAX` for the root program scope).
    parent: usize,
    /// Entry-point label for this function's body.
    label: u32,
    /// Parameters in order: (name, has_default).
    params: Vec<ParamInfo>,
    /// For a named function expression, the function's own name (visible
    /// inside the body for self-recursion).
    self_name: Option<String>,
    /// Whether this is a declaration (hoisted into the prologue).
    is_declaration: bool,
    /// All bindings declared in this scope, in declaration order.
    /// Each entry is (name, raw_slot_index, is_const). Raw slot indices
    /// are 0-based within own locals (excluding upvals). Includes
    /// duplicates for block-scoped names.
    names: IndexMap<String, SlotInfo>,
    /// Which own-local raw-slot indices are `const`.
    const_slots: HashSet<u32>,
    /// Which own-local raw-slot indices are reassigned in the body.
    reassigned: HashSet<u32>,
    /// Which own-local raw-slot indices are captured by nested functions.
    captured: HashSet<u32>,
    /// Nested function scope ids.
    children: Vec<usize>,
    /// Free variables: names referenced but not declared in this scope,
    /// mapped to the first reference span.
    free_vars: HashMap<String, u32>,
    /// After the bottom-up capture pass, the capture list: absolute slot
    /// indices (including upval slots) in the PARENT frame, in the order
    /// they become the closure's leading locals.
    captures: Vec<u32>,
    /// Number of leading upval slots (pre-installed by `CallDyn`).
    upval_count: u32,
    /// Total number of own-local slots (params + declared vars).
    own_local_count: u32,
    /// Final slot kinds for all locals (upvals first, then own locals).
    /// Computed after capture propagation.
    slot_kinds: Vec<SlotKind>,
}

/// Complete scope-analysis result for a compilation unit.
#[derive(Debug)]
struct ProgramAnalysis {
    scopes: Vec<FuncScope>,
    root: usize,
}

// ─────────────────────────────────────────────────────────────────────

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

    // Phase 3: run scope/capture analysis over all nested functions before
    // codegen, so we know which slots are `Boxed` and what the closure
    // capture lists are.
    compiler.analysis = Some(compiler.analyze_program(&ret.program));

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
#[derive(Copy, Clone, Debug)]
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
    /// COMPILER_PLAN).
    scopes: Vec<HashMap<String, SlotInfo>>,
    /// Loop-context stack for `break`/`continue` (innermost loop last).
    loops: Vec<LoopCtx>,
    diagnostics: Vec<Diagnostic>,
    /// Phase 3: scope/capture analysis pre-computed before codegen. `None`
    /// during the analysis pass itself; `Some` during codegen.
    analysis: Option<ProgramAnalysis>,
    /// Phase 3: which function scope we are currently codegen'ing. This is an
    /// index into `analysis.scopes`. Only set during codegen.
    current_scope: usize,
    /// Phase 3: per-scope cursor for matching children in AST order.
    /// `next_child[parent_id]` = index of the next child to match.
    next_child: Vec<usize>,
    /// Phase 3: monotonic local-slot allocator for the current function.
    /// Reset to 0 at each function scope entry. The analysis pre-computes
    /// slot *kinds* (Plain/Boxed); the counter ensures names→slots match
    /// the same order as the analysis walk.
    next_slot: u32,
    /// Phase 3: queue of matched declaration child IDs. When
    /// `hoist_function_decl_in_stmt` matches a child scope, it pushes it
    /// here; `compile_function_decl_body` removes it in FIFO order.
    decl_child_queue: Vec<usize>,
    /// Phase 3: cursor into `analysis.scopes[current_scope].names`.
    /// Advanced by `declare_lexical` as declarations are processed,
    /// ensuring shadowed names get their correct pre-computed slots.
    next_name_idx: usize,
}

impl<'src> Compiler<'src> {
    fn new(source: &'src str) -> Self {
        Compiler {
            source,
            code: Vec::new(),
            spans: Vec::new(),
            next_label: 0,
            scopes: Vec::new(),
            loops: Vec::new(),
            diagnostics: Vec::new(),
            analysis: None,
            current_scope: 0,
            next_child: Vec::new(),
            next_slot: 0,
            decl_child_queue: Vec::new(),
            next_name_idx: 0,
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

    /// The whole program is the root frame's body. Phase 3: scope/capture
    /// analysis was already run (stored in `self.analysis`), so we know the
    /// exact `Vec<SlotKind>` for the prologue `Alloc` and which slots are
    /// const/reassigned/captured. After emitting the root body (including
    /// hoisted function declarations), we drain `pending_functions` and emit
    /// each function body.
    fn compile_program(&mut self, program: &ast::Program) {
        let analysis = self
            .analysis
            .as_ref()
            .expect("analysis must run before codegen");
        let root = &analysis.scopes[analysis.root];
        self.current_scope = analysis.root;
        self.next_slot = 0;

        // Initialize the next_child cursor for all scopes.
        self.next_child = vec![0usize; analysis.scopes.len()];

        // Prologue: allocate all root locals with the correct SlotKinds
        // (pre-computed by analysis).
        self.scopes.push(HashMap::new());
        self.next_name_idx = 0;

        if !root.slot_kinds.is_empty() {
            self.emit(Instr::Alloc(root.slot_kinds.clone()), program.span.start);
        }

        // Hoist `var` declarations and function declarations. These use
        // the monotonic `next_slot` counter which matches the analysis
        // order (params first, then declared vars).
        self.hoist_vars_in_stmts(&program.body);

        // Hoist function declarations into the prologue (emit bindings).
        self.hoist_function_decls(&program.body);

        // Compile top-level body statements.
        for stmt in &program.body {
            self.compile_stmt(stmt);
        }

        // Root frame ends with Return(0) → StepResult::Done.
        self.emit(Instr::Return(0), program.span.end);
    }

    // ── Phase 3: scope / capture analysis ───────────────────────────────

    /// Walk the entire AST and build the `ProgramAnalysis`: for each function
    /// scope (including the root), collect params, declared locals, nested
    /// functions, free variables, and reassignment info. Then run a bottom-up
    /// capture-propagation pass to determine slot kinds and capture lists.
    fn analyze_program(&mut self, program: &ast::Program) -> ProgramAnalysis {
        let mut scopes = Vec::new();
        let root = self.analyze_top_level(program, &mut scopes);
        self.resolve_captures(&mut scopes);
        ProgramAnalysis { scopes, root }
    }

    /// Build a `FuncScope` for the program root and walk its body.
    fn analyze_top_level(&mut self, program: &ast::Program, scopes: &mut Vec<FuncScope>) -> usize {
        let label = self.new_label();
        let mut scope = FuncScope {
            id: 0, // temporary; updated after children are pushed
            parent: usize::MAX,
            label,
            params: Vec::new(),
            self_name: None,
            is_declaration: false,
            names: IndexMap::new(),
            const_slots: HashSet::new(),
            reassigned: HashSet::new(),
            captured: HashSet::new(),
            children: Vec::new(),
            free_vars: HashMap::new(),
            captures: Vec::new(),
            upval_count: 0,
            own_local_count: 0,
            slot_kinds: Vec::new(),
        };

        // Block-scoped name tracking for the analysis walk.
        let mut block_scopes: Vec<IndexMap<String, u32>> = vec![IndexMap::new()];
        let mut next_slot = 0u32;

        // Use a temporary scopes vec for children; they'll be prepended to
        // the main scopes vec before the root is pushed.
        let mut child_scopes = Vec::new();
        self.analyze_stmts(
            &program.body,
            0, // parent id (placeholder)
            &mut scope,
            &mut block_scopes,
            &mut next_slot,
            &mut child_scopes,
        );

        scope.own_local_count = next_slot;
        scope.slot_kinds = vec![SlotKind::Plain; scope.own_local_count as usize];

        // Children are pushed first, then the root. The correct IDs and
        // parent references are already set by build_function_scope.
        for child in child_scopes {
            scopes.push(child);
        }

        let root_id = scopes.len();
        scope.id = root_id;
        // Fix up root's children parent references.
        for &child_id in &scope.children {
            scopes[child_id].parent = root_id;
        }
        scopes.push(scope);
        root_id
    }

    /// Analyze a list of statements within a function scope.
    fn analyze_stmts(
        &mut self,
        stmts: &[ast::Statement],
        func_id: usize,
        func_scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, u32>>,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        for stmt in stmts {
            self.analyze_stmt(stmt, func_id, func_scope, block_scopes, next_slot, scopes);
        }
    }

    fn analyze_stmt(
        &mut self,
        stmt: &ast::Statement,
        func_id: usize,
        func_scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, u32>>,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        match stmt {
            ast::Statement::VariableDeclaration(decl) => {
                let is_const = decl.kind == ast::VariableDeclarationKind::Const;
                let is_var = decl.kind == ast::VariableDeclarationKind::Var;
                for d in &decl.declarations {
                    self.analyze_declare_pattern(
                        &d.id,
                        is_const,
                        is_var,
                        func_scope,
                        block_scopes,
                        next_slot,
                    );
                    if let Some(init) = &d.init {
                        self.analyze_expr(init, func_id, func_scope, block_scopes, scopes);
                    }
                }
            }
            ast::Statement::FunctionDeclaration(f) => {
                // Hoisted function declaration — add as a child scope.
                let child = self.build_function_scope(
                    f, func_id, true, // is_declaration
                    scopes,
                );
                func_scope.children.push(child);
                // Register the binding name in the function scope.
                if let Some(id) = &f.id {
                    let name = id.name.as_str();
                    let slot = self.analyze_register_name(
                        name,
                        false, // functions are not const
                        true,  // hoisted like var
                        func_scope,
                        block_scopes,
                        next_slot,
                    );
                    // Also register in function-scope names for codegen lookup.
                    func_scope
                        .names
                        .entry(name.to_string())
                        .or_insert(SlotInfo {
                            slot,
                            is_const: false,
                        });
                }
            }
            ast::Statement::BlockStatement(block) => {
                block_scopes.push(IndexMap::new());
                self.analyze_stmts(
                    &block.body,
                    func_id,
                    func_scope,
                    block_scopes,
                    next_slot,
                    scopes,
                );
                block_scopes.pop();
            }
            ast::Statement::IfStatement(s) => {
                self.analyze_expr(&s.test, func_id, func_scope, block_scopes, scopes);
                self.analyze_stmt(
                    &s.consequent,
                    func_id,
                    func_scope,
                    block_scopes,
                    next_slot,
                    scopes,
                );
                if let Some(alt) = &s.alternate {
                    self.analyze_stmt(alt, func_id, func_scope, block_scopes, next_slot, scopes);
                }
            }
            ast::Statement::WhileStatement(s) => {
                self.analyze_expr(&s.test, func_id, func_scope, block_scopes, scopes);
                self.analyze_stmt(
                    &s.body,
                    func_id,
                    func_scope,
                    block_scopes,
                    next_slot,
                    scopes,
                );
            }
            ast::Statement::DoWhileStatement(s) => {
                self.analyze_stmt(
                    &s.body,
                    func_id,
                    func_scope,
                    block_scopes,
                    next_slot,
                    scopes,
                );
                self.analyze_expr(&s.test, func_id, func_scope, block_scopes, scopes);
            }
            ast::Statement::ForStatement(s) => {
                if let Some(init) = &s.init {
                    match init {
                        ast::ForStatementInit::VariableDeclaration(decl) => {
                            let is_const = decl.kind == ast::VariableDeclarationKind::Const;
                            let is_var = decl.kind == ast::VariableDeclarationKind::Var;
                            for d in &decl.declarations {
                                self.analyze_declare_pattern(
                                    &d.id,
                                    is_const,
                                    is_var,
                                    func_scope,
                                    block_scopes,
                                    next_slot,
                                );
                                if let Some(init_expr) = &d.init {
                                    self.analyze_expr(
                                        init_expr,
                                        func_id,
                                        func_scope,
                                        block_scopes,
                                        scopes,
                                    );
                                }
                            }
                        }
                        _ => {
                            // Expression init (inherit_variants! means the
                            // expression variants are flattened in). Use
                            // as_expression() to get the expression ref.
                            if let Some(expr) = init.as_expression() {
                                self.analyze_expr(expr, func_id, func_scope, block_scopes, scopes);
                            }
                        }
                    }
                }
                if let Some(test) = &s.test {
                    self.analyze_expr(test, func_id, func_scope, block_scopes, scopes);
                }
                if let Some(update) = &s.update {
                    self.analyze_expr(update, func_id, func_scope, block_scopes, scopes);
                }
                self.analyze_stmt(
                    &s.body,
                    func_id,
                    func_scope,
                    block_scopes,
                    next_slot,
                    scopes,
                );
            }
            ast::Statement::ExpressionStatement(es) => {
                self.analyze_expr(&es.expression, func_id, func_scope, block_scopes, scopes);
            }
            ast::Statement::ReturnStatement(r) => {
                if let Some(val) = &r.argument {
                    self.analyze_expr(val, func_id, func_scope, block_scopes, scopes);
                }
            }
            ast::Statement::BreakStatement(_)
            | ast::Statement::ContinueStatement(_)
            | ast::Statement::EmptyStatement(_) => {}
            // For constructs not yet supported, skip analysis (they'll error
            // during codegen). Nested functions are caught by the recursive
            // walk; other expressions are walked shallowly for free vars.
            _ => {
                // Walk any expressions inside the unsupported statement for
                // free-variable detection (e.g. `for..of` has a body).
                self.analyze_stmt_shallow(
                    stmt,
                    func_id,
                    func_scope,
                    block_scopes,
                    next_slot,
                    scopes,
                );
            }
        }
    }

    /// Shallow analysis of unsupported statements: walk the statement tree
    /// enough to find identifiers (so free variables are detected) but don't
    /// create new function scopes (those will error during codegen).
    fn analyze_stmt_shallow(
        &mut self,
        stmt: &ast::Statement,
        func_id: usize,
        func_scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, u32>>,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        match stmt {
            ast::Statement::ForOfStatement(s) => {
                self.analyze_stmt(
                    &s.body,
                    func_id,
                    func_scope,
                    block_scopes,
                    next_slot,
                    scopes,
                );
            }
            ast::Statement::ForInStatement(s) => {
                self.analyze_stmt(
                    &s.body,
                    func_id,
                    func_scope,
                    block_scopes,
                    next_slot,
                    scopes,
                );
            }
            ast::Statement::SwitchStatement(s) => {
                self.analyze_expr(&s.discriminant, func_id, func_scope, block_scopes, scopes);
                for case in &s.cases {
                    if let Some(test) = &case.test {
                        self.analyze_expr(test, func_id, func_scope, block_scopes, scopes);
                    }
                    for cs in &case.consequent {
                        self.analyze_stmt(cs, func_id, func_scope, block_scopes, next_slot, scopes);
                    }
                }
            }
            _ => {}
        }
    }

    /// Register a name from a declaration pattern in the analysis, returning the
    /// assigned slot index.
    fn analyze_declare_pattern(
        &mut self,
        pat: &ast::BindingPattern,
        is_const: bool,
        is_var: bool,
        func_scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, u32>>,
        next_slot: &mut u32,
    ) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                let name = id.name.as_str();
                self.analyze_register_name(
                    name,
                    is_const,
                    is_var,
                    func_scope,
                    block_scopes,
                    next_slot,
                );
            }
            ast::BindingPattern::AssignmentPattern(ap) => {
                self.analyze_declare_pattern(
                    &ap.left,
                    is_const,
                    is_var,
                    func_scope,
                    block_scopes,
                    next_slot,
                );
            }
            ast::BindingPattern::ArrayPattern(arr) => {
                for el in arr.elements.iter().flatten() {
                    self.analyze_declare_pattern(
                        el,
                        is_const,
                        is_var,
                        func_scope,
                        block_scopes,
                        next_slot,
                    );
                }
                if let Some(rest) = &arr.rest {
                    self.analyze_declare_pattern(
                        &rest.argument,
                        is_const,
                        is_var,
                        func_scope,
                        block_scopes,
                        next_slot,
                    );
                }
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.analyze_declare_pattern(
                        &prop.value,
                        is_const,
                        is_var,
                        func_scope,
                        block_scopes,
                        next_slot,
                    );
                }
                if let Some(rest) = &obj.rest {
                    self.analyze_declare_pattern(
                        &rest.argument,
                        is_const,
                        is_var,
                        func_scope,
                        block_scopes,
                        next_slot,
                    );
                }
            }
        }
    }

    /// Register a binding name and return its slot index. `var` names go to the
    /// function-scope slot table (block_scopes[0]); `let`/`const` go to the
    /// innermost block scope. A name already in the function scope (from a `var`)
    /// is reused; otherwise a fresh slot is allocated.
    fn analyze_register_name(
        &mut self,
        name: &str,
        is_const: bool,
        is_var: bool,
        func_scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, u32>>,
        next_slot: &mut u32,
    ) -> u32 {
        if name == "state" {
            // Shadowing `state` is an error during codegen; just skip.
            return 0;
        }
        if is_var {
            // `var` goes to the function scope (block_scopes[0]).
            if let Some(&slot) = block_scopes[0].get(name) {
                return slot;
            }
            let slot = *next_slot;
            *next_slot += 1;
            block_scopes[0].insert(name.to_string(), slot);
            func_scope
                .names
                .entry(name.to_string())
                .or_insert(SlotInfo {
                    slot,
                    is_const: false,
                });
            if is_const {
                func_scope.const_slots.insert(slot);
            }
            slot
        } else {
            // `let`/`const` go to the innermost block scope.
            let scope = block_scopes.last_mut().expect("at least one block scope");
            if scope.contains_key(name) {
                // Redeclaration in the same block — will be caught as an error
                // during codegen. Assign a fresh slot anyway.
                let slot = *next_slot;
                *next_slot += 1;
                scope.insert(name.to_string(), slot);
                func_scope
                    .names
                    .entry(name.to_string())
                    .or_insert(SlotInfo { slot, is_const });
                if is_const {
                    func_scope.const_slots.insert(slot);
                }
                return slot;
            }
            let slot = *next_slot;
            *next_slot += 1;
            scope.insert(name.to_string(), slot);
            func_scope
                .names
                .entry(name.to_string())
                .or_insert(SlotInfo { slot, is_const });
            if is_const {
                func_scope.const_slots.insert(slot);
            }
            slot
        }
    }

    /// Analyze an expression for free-variable detection and reassignment
    /// tracking.
    fn analyze_expr(
        &mut self,
        expr: &ast::Expression,
        func_id: usize,
        func_scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, u32>>,
        scopes: &mut Vec<FuncScope>,
    ) {
        match expr {
            ast::Expression::Identifier(id) => {
                let name = id.name.as_str();
                // Check if the name is a local in this function.
                if !self.analyze_resolve_name(name, block_scopes).is_some() {
                    // Free variable — record it.
                    func_scope
                        .free_vars
                        .entry(name.to_string())
                        .or_insert(id.span.start);
                }
            }
            ast::Expression::AssignmentExpression(a) => {
                // Mark the target as reassigned.
                self.analyze_assignment_target(&a.left, func_scope, block_scopes);
                self.analyze_expr(&a.right, func_id, func_scope, block_scopes, scopes);
            }
            ast::Expression::UpdateExpression(u) => {
                // UpdateExpression.argument is a SimpleAssignmentTarget.
                self.analyze_simple_assign_target(&u.argument, func_scope, block_scopes);
            }
            ast::Expression::BinaryExpression(b) => {
                self.analyze_expr(&b.left, func_id, func_scope, block_scopes, scopes);
                self.analyze_expr(&b.right, func_id, func_scope, block_scopes, scopes);
            }
            ast::Expression::UnaryExpression(u) => {
                self.analyze_expr(&u.argument, func_id, func_scope, block_scopes, scopes);
            }
            ast::Expression::LogicalExpression(l) => {
                self.analyze_expr(&l.left, func_id, func_scope, block_scopes, scopes);
                self.analyze_expr(&l.right, func_id, func_scope, block_scopes, scopes);
            }
            ast::Expression::ConditionalExpression(c) => {
                self.analyze_expr(&c.test, func_id, func_scope, block_scopes, scopes);
                self.analyze_expr(&c.consequent, func_id, func_scope, block_scopes, scopes);
                self.analyze_expr(&c.alternate, func_id, func_scope, block_scopes, scopes);
            }
            ast::Expression::CallExpression(c) => {
                self.analyze_expr(&c.callee, func_id, func_scope, block_scopes, scopes);
                for arg in &c.arguments {
                    if let Some(e) = arg.as_expression() {
                        self.analyze_expr(e, func_id, func_scope, block_scopes, scopes);
                    }
                }
            }
            ast::Expression::StaticMemberExpression(m) => {
                self.analyze_expr(&m.object, func_id, func_scope, block_scopes, scopes);
            }
            ast::Expression::ComputedMemberExpression(m) => {
                self.analyze_expr(&m.object, func_id, func_scope, block_scopes, scopes);
                self.analyze_expr(&m.expression, func_id, func_scope, block_scopes, scopes);
            }
            ast::Expression::ArrayExpression(arr) => {
                for el in &arr.elements {
                    if let Some(e) = el.as_expression() {
                        self.analyze_expr(e, func_id, func_scope, block_scopes, scopes);
                    }
                }
            }
            ast::Expression::ObjectExpression(obj) => {
                for prop in &obj.properties {
                    match prop {
                        ast::ObjectPropertyKind::ObjectProperty(p) => {
                            self.analyze_expr(&p.value, func_id, func_scope, block_scopes, scopes);
                        }
                        ast::ObjectPropertyKind::SpreadProperty(s) => {
                            self.analyze_expr(
                                &s.argument,
                                func_id,
                                func_scope,
                                block_scopes,
                                scopes,
                            );
                        }
                    }
                }
            }
            ast::Expression::TemplateLiteral(tl) => {
                for e in &tl.expressions {
                    self.analyze_expr(e, func_id, func_scope, block_scopes, scopes);
                }
            }
            ast::Expression::SequenceExpression(seq) => {
                for e in &seq.expressions {
                    self.analyze_expr(e, func_id, func_scope, block_scopes, scopes);
                }
            }
            ast::Expression::ParenthesizedExpression(p) => {
                self.analyze_expr(&p.expression, func_id, func_scope, block_scopes, scopes);
            }
            ast::Expression::ChainExpression(chain) => {
                // ChainExpression.expression is a ChainElement (not Expression).
                // Walk into it to find nested identifiers.
                self.analyze_chain_element(
                    &chain.expression,
                    func_id,
                    func_scope,
                    block_scopes,
                    scopes,
                );
            }
            ast::Expression::FunctionExpression(f) => {
                let child = self.build_function_scope(f, func_id, false, scopes);
                func_scope.children.push(child);
            }
            ast::Expression::ArrowFunctionExpression(a) => {
                let child = self.build_arrow_scope(a, func_id, scopes);
                func_scope.children.push(child);
            }
            // Literals and unsupported expressions: nothing to analyze.
            _ => {}
        }
    }

    /// Mark the target of an assignment as reassigned. For simple identifiers,
    /// looks up the slot and marks it. For member expressions, marks nothing
    /// (they don't affect slot boxing).
    fn analyze_assignment_target(
        &mut self,
        target: &ast::AssignmentTarget,
        func_scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, u32>>,
    ) {
        match target {
            ast::AssignmentTarget::AssignmentTargetIdentifier(id) => {
                let name = id.name.as_str();
                if let Some(slot) = self.analyze_resolve_name(name, block_scopes) {
                    func_scope.reassigned.insert(slot);
                }
            }
            ast::AssignmentTarget::StaticMemberExpression(_)
            | ast::AssignmentTarget::ComputedMemberExpression(_) => {}
            ast::AssignmentTarget::ArrayAssignmentTarget(arr) => {
                for el in arr.elements.iter().flatten() {
                    // Elements are AssignmentTargetMaybeDefault — use
                    // as_assignment_target() to get the inner target.
                    if let Some(t) = el.as_assignment_target() {
                        self.analyze_assignment_target(t, func_scope, block_scopes);
                    }
                }
                if let Some(rest) = &arr.rest {
                    self.analyze_assignment_target(&rest.target, func_scope, block_scopes);
                }
            }
            ast::AssignmentTarget::ObjectAssignmentTarget(obj) => {
                for prop in &obj.properties {
                    match prop {
                        ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                            // binding is an IdentifierReference — mark the slot.
                            let name = p.binding.name.as_str();
                            if let Some(slot) = self.analyze_resolve_name(name, block_scopes) {
                                func_scope.reassigned.insert(slot);
                            }
                        }
                        ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                            // binding is an AssignmentTargetMaybeDefault.
                            if let Some(t) = p.binding.as_assignment_target() {
                                self.analyze_assignment_target(t, func_scope, block_scopes);
                            }
                        }
                    }
                }
                if let Some(rest) = &obj.rest {
                    self.analyze_assignment_target(&rest.target, func_scope, block_scopes);
                }
            }
            _ => {}
        }
    }

    /// Like `analyze_assignment_target` but for `SimpleAssignmentTarget`
    /// (used by `UpdateExpression::argument`).
    fn analyze_simple_assign_target(
        &mut self,
        target: &ast::SimpleAssignmentTarget,
        func_scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, u32>>,
    ) {
        match target {
            ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                let name = id.name.as_str();
                if let Some(slot) = self.analyze_resolve_name(name, block_scopes) {
                    func_scope.reassigned.insert(slot);
                }
            }
            ast::SimpleAssignmentTarget::StaticMemberExpression(_)
            | ast::SimpleAssignmentTarget::ComputedMemberExpression(_) => {}
            _ => {}
        }
    }

    /// Walk into a `ChainElement` to find identifiers and nested expressions.
    fn analyze_chain_element(
        &mut self,
        el: &ast::ChainElement,
        func_id: usize,
        func_scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, u32>>,
        scopes: &mut Vec<FuncScope>,
    ) {
        match el {
            ast::ChainElement::CallExpression(c) => {
                // Walk the callee and args.
                self.analyze_expr(&c.callee, func_id, func_scope, block_scopes, scopes);
                for arg in &c.arguments {
                    if let Some(e) = arg.as_expression() {
                        self.analyze_expr(e, func_id, func_scope, block_scopes, scopes);
                    }
                }
            }
            // Other ChainElement variants (StaticMemberExpression,
            // ComputedMemberExpression, etc.) are fine to skip — the
            // contained identifiers will be caught when the chain is
            // lowered during codegen.
            _ => {}
        }
    }

    /// Resolve a name to its own-local slot index (0-based within the current
    /// function's own locals). Returns `None` if the name is not declared in
    /// this function (i.e., it's a free variable).
    fn analyze_resolve_name(
        &self,
        name: &str,
        block_scopes: &[IndexMap<String, u32>],
    ) -> Option<u32> {
        block_scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    /// Build a `FuncScope` for an `ast::Function` (used by both declarations and
    /// expressions). Walks the body recursively to collect nested scopes.
    fn build_function_scope(
        &mut self,
        func: &ast::Function,
        parent_id: usize,
        is_declaration: bool,
        scopes: &mut Vec<FuncScope>,
    ) -> usize {
        let label = self.new_label();

        // Collect parameters.
        let mut params: Vec<ParamInfo> = Vec::new();
        for param in &func.params.items {
            // oxc stores parameter defaults on the FormalParameter's
            // `initializer` field (inherited via `inherit_variants!`).
            let has_default = param.initializer.is_some();
            let (name, _pat_has_default) = self.analyze_param_info(&param.pattern);
            for n in &name {
                params.push(ParamInfo {
                    name: n.clone(),
                    has_default,
                });
            }
        }

        let self_name = func.id.as_ref().map(|id| id.name.as_str().to_string());

        // Build the scope but don't push it yet. Children will be pushed
        // first (by recursive calls), then this scope. After the walk we
        // compute the correct id.
        let mut scope = FuncScope {
            id: 0, // placeholder; fixed up below
            parent: parent_id,
            label,
            params,
            self_name,
            is_declaration,
            names: IndexMap::new(),
            const_slots: HashSet::new(),
            reassigned: HashSet::new(),
            captured: HashSet::new(),
            children: Vec::new(),
            free_vars: HashMap::new(),
            captures: Vec::new(),
            upval_count: 0,
            own_local_count: 0,
            slot_kinds: Vec::new(),
        };

        // Block-scoped name tracking.
        let mut block_scopes: Vec<IndexMap<String, u32>> = vec![IndexMap::new()];
        let mut next_slot = scope.params.len() as u32;

        for (i, p) in scope.params.iter().enumerate() {
            block_scopes[0].insert(p.name.clone(), i as u32);
            scope.names.insert(
                p.name.clone(),
                SlotInfo {
                    slot: i as u32,
                    is_const: false,
                },
            );
        }

        if let Some(body) = &func.body {
            self.analyze_stmts(
                &body.statements,
                0, // temporary parent id; will be fixed up
                &mut scope,
                &mut block_scopes,
                &mut next_slot,
                scopes,
            );
        }

        scope.own_local_count = next_slot;
        scope.slot_kinds = vec![SlotKind::Plain; next_slot as usize];

        // Push the scope AFTER children. The true id is scopes.len().
        let id = scopes.len();
        scope.id = id;
        // Fix up children's parent references to point to the correct id.
        for &child_id in &scope.children {
            scopes[child_id].parent = id;
        }
        scopes.push(scope);
        id
    }

    /// Build a `FuncScope` for an arrow function.
    fn build_arrow_scope(
        &mut self,
        arrow: &ast::ArrowFunctionExpression,
        parent_id: usize,
        scopes: &mut Vec<FuncScope>,
    ) -> usize {
        let label = self.new_label();

        let mut params: Vec<ParamInfo> = Vec::new();
        for param in &arrow.params.items {
            let has_default = param.initializer.is_some();
            let (name, _pat_has_default) = self.analyze_param_info(&param.pattern);
            for n in &name {
                params.push(ParamInfo {
                    name: n.clone(),
                    has_default,
                });
            }
        }

        let mut scope = FuncScope {
            id: 0,
            parent: parent_id,
            label,
            params,
            self_name: None,
            is_declaration: false,
            names: IndexMap::new(),
            const_slots: HashSet::new(),
            reassigned: HashSet::new(),
            captured: HashSet::new(),
            children: Vec::new(),
            free_vars: HashMap::new(),
            captures: Vec::new(),
            upval_count: 0,
            own_local_count: 0,
            slot_kinds: Vec::new(),
        };

        let mut block_scopes: Vec<IndexMap<String, u32>> = vec![IndexMap::new()];
        let mut next_slot = scope.params.len() as u32;

        for (i, p) in scope.params.iter().enumerate() {
            block_scopes[0].insert(p.name.clone(), i as u32);
            scope.names.insert(
                p.name.clone(),
                SlotInfo {
                    slot: i as u32,
                    is_const: false,
                },
            );
        }

        self.analyze_stmts(
            &arrow.body.statements,
            0,
            &mut scope,
            &mut block_scopes,
            &mut next_slot,
            scopes,
        );

        scope.own_local_count = next_slot;
        scope.slot_kinds = vec![SlotKind::Plain; next_slot as usize];

        let id = scopes.len();
        scope.id = id;
        for &child_id in &scope.children {
            scopes[child_id].parent = id;
        }
        scopes.push(scope);
        id
    }

    /// Extract binding names (recursively, for destructured params) and whether
    /// the parameter has a default.
    fn analyze_param_info(&self, pat: &ast::BindingPattern) -> (Vec<String>, bool) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                (vec![id.name.as_str().to_string()], false)
            }
            ast::BindingPattern::AssignmentPattern(ap) => {
                let (names, _) = self.analyze_param_info(&ap.left);
                (names, true)
            }
            ast::BindingPattern::ArrayPattern(arr) => {
                let mut names = Vec::new();
                for el in arr.elements.iter().flatten() {
                    let (n, _) = self.analyze_param_info(el);
                    names.extend(n);
                }
                if let Some(rest) = &arr.rest {
                    let (n, _) = self.analyze_param_info(&rest.argument);
                    names.extend(n);
                }
                (names, false)
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                let mut names = Vec::new();
                for prop in &obj.properties {
                    let (n, _) = self.analyze_param_info(&prop.value);
                    names.extend(n);
                }
                if let Some(rest) = &obj.rest {
                    let (n, _) = self.analyze_param_info(&rest.argument);
                    names.extend(n);
                }
                (names, false)
            }
        }
    }

    /// Bottom-up pass: for each scope, resolve its children's free variables
    /// against its own locals, populate capture lists, and determine which slots
    /// must be `Boxed` (captured AND reassigned).
    fn resolve_captures(&self, scopes: &mut Vec<FuncScope>) {
        // Process scopes in reverse order (children before parents).
        for i in (0..scopes.len()).rev() {
            // Gather the scope's children first.
            let children: Vec<usize> = scopes[i].children.clone();

            for &child_id in &children {
                // Clone free_vars to release the borrow on scopes.
                let free_vars: HashMap<String, u32> = scopes[child_id].free_vars.clone();
                // A named function's own name inside its body is a
                // self-reference, not a capture from the parent.
                let self_name: Option<String> = scopes[child_id].self_name.clone();

                for (fv_name, _fv_span) in &free_vars {
                    // Skip free variables that match the function's own
                    // name — they'll get a dedicated self-reference slot.
                    if self_name.as_ref() == Some(fv_name) {
                        continue;
                    }
                    // Copy the slot info to release the immutable borrow
                    // before the mutable borrows below.
                    let slot_info = scopes[i].names.get(fv_name).copied();
                    if let Some(info) = slot_info {
                        let parent_abs_slot = scopes[i].upval_count + info.slot;

                        let child_ref = &mut scopes[child_id];
                        if !child_ref.captures.contains(&parent_abs_slot) {
                            child_ref.captures.push(parent_abs_slot);
                        }

                        scopes[i].captured.insert(info.slot);
                    }
                }
            }
        }

        // Second pass (forward): compute slot kinds.
        // Any slot that is captured by a nested function must be Boxed
        // (conservative: we can't easily determine if the nested function
        // reassigns it, and boxing is always safe).
        for i in 0..scopes.len() {
            let own_count = scopes[i].own_local_count;
            let param_count = scopes[i].params.len() as u32;

            let upval_count = scopes[i].captures.len() as u32;
            scopes[i].upval_count = upval_count;

            let mut kinds: Vec<SlotKind> = vec![SlotKind::Plain; upval_count as usize];

            // Params: if captured, they get a Boxed copy slot.
            for p_idx in 0..param_count {
                if scopes[i].captured.contains(&p_idx) {
                    kinds.push(SlotKind::Boxed);
                } else {
                    kinds.push(SlotKind::Plain);
                }
            }

            // Declared locals (non-params): box if captured.
            for s_idx in param_count..own_count {
                if scopes[i].captured.contains(&s_idx) {
                    kinds.push(SlotKind::Boxed);
                } else {
                    kinds.push(SlotKind::Plain);
                }
            }

            scopes[i].slot_kinds = kinds;
        }
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

            // Later phases / out of scope — informative errors.
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
                            // declaration executes (e.g. per loop iteration).
                            // Outside a loop `Alloc` already zeroed the slot, so
                            // skip the redundant Push+SetLocal.
                            if !self.loops.is_empty() {
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

    // ── scope / binding helpers ──────────────────────────────────────────

    /// Declare a `let`/`const` binding in the current (innermost) scope, giving
    /// it a fresh function-wide slot. `state` is blessed and cannot be shadowed.
    /// Phase 3: slot indices are pre-computed by the analysis; this method
    /// looks up the pre-computed slot from the function scope.
    fn declare_lexical(&mut self, name: &str, span: u32, is_const: bool) -> u32 {
        if name == "state" {
            self.error(span, "cannot shadow the blessed `state` object");
        }
        // Walk the analysis's pre-computed names (in declaration order) to
        // find the next occurrence of this name. The analysis and codegen
        // walk in the same order, so a simple forward scan with a cursor
        // handles shadowed bindings correctly.
        let analysis = self.analysis.as_ref().expect("analysis present");
        let scope = &analysis.scopes[self.current_scope];
        let upvals = scope.upval_count;
        let names = &scope.names;
        let mut idx = self.next_name_idx;
        while idx < names.len() {
            let (n, info) = names.get_index(idx).unwrap();
            idx += 1;
            if n == name {
                self.next_name_idx = idx;
                let abs_slot = upvals + info.slot;
                self.next_slot = self.next_slot.max(abs_slot + 1);
                self.scopes
                    .last_mut()
                    .expect("a scope is always open during codegen")
                    .insert(
                        name.to_string(),
                        SlotInfo {
                            slot: abs_slot,
                            is_const: info.is_const,
                        },
                    );
                return abs_slot;
            }
        }
        // Not found in the remaining analysis entries — fallback (should be
        // rare; e.g., a name not tracked by the analysis).
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
                self.compile_user_call(id.name.as_str(), &argv, span)
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

    // ── Phase 3: function codegen ────────────────────────────────────

    /// Call to a user-defined function identified by a bare name. If the name
    /// resolves to a local binding, emit a static `Call` (when we know the
    /// label) or `CallDyn`. Otherwise fall through to the built-in global call
    /// path (`String`, `Number`, `Boolean`, `raise`).
    fn compile_user_call(&mut self, name: &str, argv: &[&ast::Expression], span: u32) {
        if let Some(info) = self.resolve_local(name) {
            // Try to resolve to a static `Call`. If the function was declared
            // in this scope and has NO captures, we can use a static Call.
            // Functions with captures must use CallDyn so the VM installs
            // the upvals as leading locals.
            let label = self.find_callee_label(name);
            match label {
                Some(l) => {
                    // Check whether this function has any captures.
                    let has_captures = self.function_has_captures(name);
                    if has_captures {
                        // Dynamic call via Local + CallDyn (installs upvals).
                        self.compile_args(argv);
                        self.emit(Instr::Local(info.slot), span);
                        self.emit(Instr::CallDyn(argv.len() as u32), span);
                    } else {
                        // Static call: push args and Call. Pad with Undefined if
                        // the caller passes fewer args than the function expects
                        // (for default parameters).
                        let expected_arity = self.function_arity(name);
                        self.compile_args(argv);
                        let argc = argv.len() as u32;
                        // Pad with Undefined for missing args.
                        for _ in argc..expected_arity {
                            self.emit(Instr::Push(StackValue::Undefined), span);
                        }
                        self.emit(Instr::Call(l, expected_arity), span);
                    }
                }
                None => {
                    // Dynamic call: push args, load callee, CallDyn.
                    self.compile_args(argv);
                    self.emit(Instr::Local(info.slot), span);
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

    /// Return the next child scope id for `parent_id` (advancing the cursor).
    fn next_child_scope(&mut self, parent_id: usize) -> Option<usize> {
        let analysis = self.analysis.as_ref().expect("analysis present");
        let scope = &analysis.scopes[parent_id];
        let idx = self.next_child[parent_id];
        if idx < scope.children.len() {
            let child_id = scope.children[idx];
            self.next_child[parent_id] = idx + 1;
            Some(child_id)
        } else {
            None
        }
    }

    /// Hoist function declarations in the current scope's prologue: walk the
    /// AST statements to find `function` declarations, register their names
    /// in scopes[0], and emit bindings. This must match the analysis walk
    /// order so that `match_child_scope`'s cursor is properly synced.
    fn hoist_function_decls(&mut self, stmts: &[ast::Statement]) {
        for stmt in stmts {
            self.hoist_function_decl_in_stmt(stmt);
        }
    }

    fn hoist_function_decl_in_stmt(&mut self, stmt: &ast::Statement) {
        match stmt {
            ast::Statement::FunctionDeclaration(f) => {
                let name = f.id.as_ref().map(|id| id.name.as_str());
                let child_id = self.match_child_scope(
                    self.current_scope,
                    name,
                    true, // is_declaration
                    f.span.start,
                );
                let Some(child_id) = child_id else {
                    return;
                };
                let (label, captures) = {
                    let analysis = self.analysis.as_ref().expect("analysis present");
                    let child = &analysis.scopes[child_id];
                    (child.label, child.captures.clone())
                };

                // Save the child_id so compile_function_decl_body can
                // find it without re-matching.
                self.decl_child_queue.push(child_id);

                // Register the name in scopes[0] (like var hoisting).
                // Use the pre-computed slot from analysis since function
                // declarations are unique per scope.
                if let Some(name_str) = name {
                    let slot = {
                        let analysis: &ProgramAnalysis =
                            self.analysis.as_ref().expect("analysis present");
                        let child_scope = &analysis.scopes[child_id];
                        let parent_id = child_scope.parent;
                        if parent_id != usize::MAX {
                            if let Some(info) = analysis.scopes[parent_id].names.get(name_str) {
                                analysis.scopes[parent_id].upval_count + info.slot
                            } else {
                                self.next_slot
                            }
                        } else {
                            self.next_slot
                        }
                    };
                    self.scopes[0].insert(
                        name_str.to_string(),
                        SlotInfo {
                            slot,
                            is_const: false,
                        },
                    );

                    // Emit the function value binding.
                    let span = f.span.start;
                    if captures.is_empty() {
                        self.emit(Instr::Push(StackValue::Fn(label)), span);
                    } else {
                        self.emit(Instr::MakeClosure(label, captures), span);
                    }
                    self.emit(Instr::SetLocal(slot), span);
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

    /// Emit the body of a function declaration. Called from `compile_stmt`
    /// when a `FunctionDeclaration` is encountered during the body walk.
    /// The child scope was already matched by `hoist_function_decl_in_stmt`
    /// and pushed to `decl_child_queue`.
    fn compile_function_decl_body(&mut self, f: &ast::Function) {
        let child_id = match self.decl_child_queue.first().copied() {
            Some(id) => {
                self.decl_child_queue.remove(0);
                id
            }
            None => {
                self.error(
                    f.span.start,
                    "internal error: no queued child scope for function declaration",
                );
                return;
            }
        };

        if let Some(body) = &f.body {
            self.emit_function_def(child_id, &body.statements, &f.params, f.span.start, false);
        }
    }

    /// Compile a function expression: emit the function value and its body.
    fn compile_function_expr(&mut self, func: &ast::Function, span: u32) {
        let func_name = func.id.as_ref().map(|id| id.name.as_str());
        let child_id = self.match_child_scope(
            self.current_scope,
            func_name,
            false, // !is_declaration
            span,
        );
        let Some(child_id) = child_id else {
            self.error(
                span,
                "internal error: function expression not found in analysis",
            );
            return;
        };

        // Clone the data we need from analysis so we can release the borrow
        // before calling self.emit().
        let (label, captures, is_empty_captures) = {
            let analysis = self.analysis.as_ref().expect("analysis present");
            let child = &analysis.scopes[child_id];
            (
                child.label,
                child.captures.clone(),
                child.captures.is_empty(),
            )
        };

        // Emit the function value on the expression stack.
        if is_empty_captures {
            self.emit(Instr::Push(StackValue::Fn(label)), span);
        } else {
            self.emit(Instr::MakeClosure(label, captures), span);
        }

        // Emit the body after the current expression.
        if let Some(body) = &func.body {
            self.emit_function_def(child_id, &body.statements, &func.params, span, false);
        }
    }

    /// Compile an arrow function expression.
    fn compile_arrow_expr(&mut self, arrow: &ast::ArrowFunctionExpression, span: u32) {
        let child_id = self.match_child_scope(
            self.current_scope,
            None,  // arrows are always anonymous
            false, // !is_declaration
            span,
        );
        let Some(child_id) = child_id else {
            self.error(span, "internal error: arrow function not found in analysis");
            return;
        };

        let (label, captures, is_empty_captures) = {
            let analysis = self.analysis.as_ref().expect("analysis present");
            let child = &analysis.scopes[child_id];
            (
                child.label,
                child.captures.clone(),
                child.captures.is_empty(),
            )
        };

        if is_empty_captures {
            self.emit(Instr::Push(StackValue::Fn(label)), span);
        } else {
            self.emit(Instr::MakeClosure(label, captures), span);
        }

        // Arrow expression bodies: the body is an expression, not a block.
        // Emit the expression and then Return(1); no implicit Undefined return.
        let is_expression_body = arrow.expression;
        self.emit_function_def(
            child_id,
            &arrow.body.statements,
            &arrow.params,
            span,
            is_expression_body,
        );
    }

    /// Match a child scope by name and declaration status, advancing the
    /// per-parent cursor. For anonymous functions, matches the next
    /// non-declaration child.
    fn match_child_scope(
        &mut self,
        parent_id: usize,
        name: Option<&str>,
        is_declaration: bool,
        error_span: u32,
    ) -> Option<usize> {
        let result;
        {
            let analysis = self.analysis.as_ref().expect("analysis present");
            let scope = &analysis.scopes[parent_id];
            let start = self.next_child[parent_id];

            let mut found = None;
            for i in start..scope.children.len() {
                let child_id = scope.children[i];
                let child = &analysis.scopes[child_id];
                let matches = child.is_declaration == is_declaration
                    && match (name, &child.self_name) {
                        (Some(n), Some(s)) => n == s.as_str(),
                        (None, None) => true,
                        _ => false,
                    };
                if matches {
                    self.next_child[parent_id] = i + 1;
                    found = Some(child_id);
                    break;
                }
            }
            result = found;
        }

        if result.is_none() {
            self.error(
                error_span,
                format!(
                    "internal error: unmatched child scope (name={name:?}, decl={is_declaration})"
                ),
            );
        }
        result
    }

    /// Emit a function body: label, prologue (Alloc, param copies/defaults,
    /// self-reference), body statements, implicit Return. Called after the
    /// function value has been pushed to the stack (or at the declaration site).
    /// `is_expression_body`: if true (arrow expression body), don't add an
    /// implicit `return undefined` at the end.
    fn emit_function_def(
        &mut self,
        scope_id: usize,
        body_stmts: &[ast::Statement],
        params: &ast::FormalParameters,
        span: u32,
        is_expression_body: bool,
    ) {
        // Clone all the analysis data we need so we can release the borrow
        // before calling self.emit().
        let (label, upvals, slot_kinds, params_info, self_name, captures) = {
            let analysis = self.analysis.as_ref().expect("analysis present");
            let scope = &analysis.scopes[scope_id];
            (
                scope.label,
                scope.upval_count,
                scope.slot_kinds.clone(),
                scope.params.clone(),
                scope.self_name.clone(),
                scope.captures.clone(),
            )
        };

        let prev_scope = self.current_scope;
        self.current_scope = scope_id;
        self.next_slot = 0;

        // Emit a Jump over the body for sequential execution, THEN the
        // entry Label. Call/CallDyn jump to the Label (body start);
        // sequential execution hits the Jump and skips the body.
        let after = self.new_label();
        self.emit(Instr::Jump(after), span);
        self.emit(Instr::Label(label), span);

        // Save and replace the scope stack — function bodies must not see
        // the enclosing scope's bindings.
        let saved_scopes = std::mem::take(&mut self.scopes);
        self.scopes.push(HashMap::new());
        self.next_name_idx = 0;

        // Seed scopes[0] with pre-computed names from the analysis.
        // Only insert the first occurrence of each name; duplicates
        // (from block-scoped shadows) are handled by declare_lexical's
        // cursor.
        {
            let analysis = self.analysis.as_ref().expect("analysis present");
            let scope = &analysis.scopes[scope_id];
            let mut seen = HashSet::new();
            for (name, info) in &scope.names {
                if seen.insert(name.clone()) {
                    self.scopes[0].insert(
                        name.clone(),
                        SlotInfo {
                            slot: upvals + info.slot,
                            is_const: info.is_const,
                        },
                    );
                }
            }
        }

        // Allocate all own locals. The analysis pre-computed the slot
        // kinds (Boxed/Plain) and the total count. The name→slot mapping
        // is built dynamically during the body walk, which matches the
        // analysis walk order.
        let own_kinds: Vec<SlotKind> = slot_kinds[upvals as usize..].to_vec();
        let own_len = own_kinds.len() as u32;
        self.next_slot = own_len;
        if !own_kinds.is_empty() {
            self.emit(Instr::Alloc(own_kinds), span);
        }

        // Register parameter names in the function scope.
        for (p_idx, p) in params_info.iter().enumerate() {
            let abs_slot = upvals + p_idx as u32;
            self.scopes[0].insert(
                p.name.clone(),
                SlotInfo {
                    slot: abs_slot,
                    is_const: false,
                },
            );
        }

        // Register captured variable names in the function scope. The
        // captures list contains parent absolute slot indices; lookup
        // the parent scope to find the corresponding names.
        {
            let analysis = self.analysis.as_ref().expect("analysis present");
            let parent_id = analysis.scopes[scope_id].parent;
            if parent_id != usize::MAX && !captures.is_empty() {
                let parent = &analysis.scopes[parent_id];
                for (cap_idx, &parent_slot) in captures.iter().enumerate() {
                    // Find the name in the parent scope that maps to this slot.
                    for (name, info) in &parent.names {
                        if info.slot + parent.upval_count == parent_slot {
                            self.scopes[0].insert(
                                name.clone(),
                                SlotInfo {
                                    slot: cap_idx as u32, // upval slot index
                                    is_const: false,
                                },
                            );
                            break;
                        }
                    }
                }
            }
        }

        // Register the self-name (for named function expressions) in this
        // function's scope, so it's visible inside the body for recursion.
        if let Some(ref sn) = self_name {
            // Self-name goes PAST all pre-computed own locals.
            let self_slot = upvals + own_len;
            self.scopes[0].insert(
                sn.clone(),
                SlotInfo {
                    slot: self_slot,
                    is_const: true,
                },
            );
        }

        // Copy captured/reassigned params from Arg to Boxed slots; apply
        // parameter defaults.
        for (p_idx, param_info) in params_info.iter().enumerate() {
            let own_idx = p_idx as u32;
            let abs_slot = upvals + own_idx;
            // Check if this param needs boxing by examining the slot kind.
            let slot_kind = slot_kinds.get(abs_slot as usize).copied();
            let needs_box = matches!(slot_kind, Some(SlotKind::Boxed));

            let pat = &params.items[p_idx].pattern;
            let default_expr = params.items[p_idx].initializer.as_ref().map(|v| &**v);
            self.emit_param_setup(
                pat,
                p_idx as u32,
                abs_slot,
                needs_box,
                param_info.has_default,
                default_expr,
                span,
            );
        }

        // Self-reference for named function expressions.
        if let Some(ref sn) = self_name {
            if let Some(info) = self.resolve_local(sn) {
                // Allocate an extra slot for the self-reference.
                self.emit(Instr::Alloc(vec![SlotKind::Plain]), span);
                // Self-reference is always a bare Fn (never a MakeClosure
                // capturing the current frame — that would capture the
                // wrong things for invocation).
                self.emit(Instr::Push(StackValue::Fn(label)), span);
                self.emit(Instr::SetLocal(info.slot), span);
            }
        }

        // Hoist inner function declarations (emit their bindings in this
        // function's prologue).
        self.hoist_function_decls(body_stmts);

        // Compile the body statements.
        if is_expression_body && body_stmts.len() == 1 {
            // Arrow expression body: the single statement is an expression
            // whose value is the return value. Compile the expression, then
            // Return(1). Don't Pop the value.
            if let ast::Statement::ExpressionStatement(es) = &body_stmts[0] {
                self.compile_expr(&es.expression);
                self.emit(Instr::Return(1), span);
            }
        } else {
            for stmt in body_stmts {
                self.compile_stmt(stmt);
            }
            // Implicit return at end of function.
            self.emit(Instr::Push(StackValue::Undefined), span);
            self.emit(Instr::Return(1), span);
        }

        // Label for the jump-over at the start.
        self.emit(Instr::Label(after), span);

        self.scopes.pop();
        // Restore the enclosing scope stack.
        self.scopes = saved_scopes;
        self.current_scope = prev_scope;
    }

    /// Emit prologue code for one parameter: optionally read Arg, check for
    /// undefined/default, and store to the appropriate slot. `has_default`
    /// comes from the FormalParameter's `initializer` field (oxc stores
    /// defaults there, not in the BindingPattern).
    fn emit_param_setup(
        &mut self,
        _pattern: &ast::BindingPattern,
        arg_idx: u32,
        abs_slot: u32,
        needs_box: bool,
        has_default: bool,
        default_expr: Option<&ast::Expression>,
        span: u32,
    ) {
        if has_default {
            // Parameter has a default: read Arg, check if undefined, apply
            // default if needed, then store to the slot.
            if let Some(default) = default_expr {
                let skip_default = self.new_label();
                self.emit(Instr::Arg(arg_idx), span);
                self.emit(Instr::Dup, span);
                self.emit(Instr::Push(StackValue::Undefined), span);
                self.emit(Instr::Eq, span);
                self.emit(Instr::JFalse(skip_default), span);
                // Arg is undefined: pop it, evaluate default.
                self.emit(Instr::Pop(1), span);
                self.compile_expr(default);
                self.emit(Instr::Label(skip_default), span);
                // Store to the local slot (the value — arg or default — is on top).
                self.emit(Instr::SetLocal(abs_slot), span);
            }
        } else if needs_box {
            // No default, but we need a Boxed copy: Arg → SetLocal.
            self.emit(Instr::Arg(arg_idx), span);
            self.emit(Instr::SetLocal(abs_slot), span);
        } else {
            // Plain non-captured param: still copy Arg to Local so the body
            // can use Local(slot) uniformly.
            self.emit(Instr::Arg(arg_idx), span);
            self.emit(Instr::SetLocal(abs_slot), span);
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
            count_decls_in_stmt(&s.consequent) + s.alternate.as_ref().map_or(0, count_decls_in_stmt)
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
            "tools.send(1);", // tools (Phase 4)
            "raise(\"x\");",  // raise (Phase 4)
            "Math.tan(1);",   // unsupported intrinsic
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
}
