//! Scope & capture analysis (Pass 1): resolves every binding and identifier
//! reference to a frame slot — keyed by source span — and computes closure
//! capture lists and slot boxing. Codegen consumes the resulting
//! `ProgramAnalysis` and keeps no scope state of its own. See `COMPILER_PLAN.md`.

use std::collections::{HashMap, HashSet};

use indexmap::{IndexMap, IndexSet};
use oxc_ast::ast;

use crate::diag::Diagnostic;
use crate::vm::SlotKind;

// ── Analysis structures (Phase 3: functions / closures) ─────────────

/// Per-parameter analysis info.
#[derive(Debug, Clone)]
pub(crate) struct ParamInfo {
    pub(crate) name: String,
    pub(crate) has_default: bool,
}

/// Pre-computed analysis for one function scope (including the top-level
/// program). The analysis pass walks all nested functions, detects free
/// variables, and determines which slots must be `Boxed` because they are
/// captured by a nested function (conservative: all captured slots are boxed).
#[derive(Debug)]
pub(crate) struct FuncScope {
    /// Unique id (index into the `ProgramAnalysis::scopes` vec).
    id: usize,
    /// Parent scope id (`usize::MAX` for the root program scope).
    pub(crate) parent: usize,
    /// Entry-point label for this function's body.
    pub(crate) label: u32,
    /// Span of the defining function/arrow node (`u32::MAX` for the root), used
    /// to build `scope_by_span` so codegen can find this scope by AST node.
    node_span: u32,
    /// Parameters in order: (name, has_default).
    pub(crate) params: Vec<ParamInfo>,
    /// For a named function expression, the function's own name (visible
    /// inside the body for self-recursion).
    pub(crate) self_name: Option<String>,
    /// Whether this is a declaration (hoisted into the prologue).
    pub(crate) is_declaration: bool,
    /// Distinct binding names declared in this scope → (own-slot, is_const).
    /// Own-slot indices are 0-based within own locals (excluding upvals).
    /// Only the *first* occurrence of a shadowed name is kept; per-reference
    /// resolution uses `local_refs`/`binding_spans` (keyed by span), so this
    /// table is consulted only by capture resolution.
    names: IndexMap<String, SlotInfo>,
    /// Which own-local slot indices are captured by nested functions (→ Boxed,
    /// unless also `loop_declared`, in which case → per-iteration `fresh_owns`).
    captured: HashSet<u32>,
    /// Own-local slot indices for `let`/`const` bindings declared lexically
    /// inside a loop (loop head or loop body). Combined with `captured` this
    /// yields `fresh_owns`.
    loop_declared: HashSet<u32>,
    /// Own-local slot indices that are captured AND loop-declared: each loop
    /// iteration gives them a fresh cell (the compiler emits `FreshCell`), so
    /// they are allocated `Plain` (no eager cell) rather than `Boxed`. Derived
    /// in `resolve_captures`.
    pub(crate) fresh_owns: HashSet<u32>,
    /// Nested function scope ids.
    pub(crate) children: Vec<usize>,
    /// Free variables: names referenced but not declared in this scope. Drives
    /// bottom-up capture propagation. `IndexSet` for deterministic upval order.
    free_vars: IndexSet<String>,
    /// Binding occurrences declared here: (binding span, own-slot). Finalized
    /// into `ProgramAnalysis::binding_slot` (own-slot → absolute) after capture
    /// resolution fixes `upval_count`.
    binding_spans: Vec<(u32, u32)>,
    /// Identifier references that resolved to an own local: (ref span, own-slot,
    /// is_const). Finalized into `ref_resolution`.
    local_refs: Vec<(u32, u32, bool)>,
    /// Identifier references that were free here: (ref span, name). Finalized to
    /// an upval slot (if captured) or the self-reference slot, else dropped.
    free_refs: Vec<(u32, String)>,
    /// Captured names → (upval slot index, is_const), filled by capture
    /// resolution. Used to finalize `free_refs`.
    upval_by_name: HashMap<String, (u32, bool)>,
    /// The capture list: absolute slot indices in the PARENT frame, in upval
    /// order (each becomes one of this closure's leading locals).
    pub(crate) captures: Vec<u32>,
    /// Number of leading upval slots (pre-installed by `CallDyn`/`MakeClosure`).
    pub(crate) upval_count: u32,
    /// Total number of own-local slots (params + declared vars).
    pub(crate) own_local_count: u32,
    /// Whether the body references the special `arguments` array (an `arguments`
    /// identifier that does not resolve to a real binding). Drives eager
    /// materialization of the arguments array in the prologue, before the arg
    /// region is normalized to exactly `nparams`.
    pub(crate) uses_arguments: bool,
    /// Final slot kinds for own locals (params first, then declared vars). The
    /// compiler routes the declared kinds into `EnterFrame`'s `local_kinds` and
    /// boxes any captured params in place via `FreshCell`.
    pub(crate) slot_kinds: Vec<SlotKind>,
}

/// How an identifier reference resolves to a frame slot.
#[derive(Copy, Clone, Debug)]
pub(crate) struct RefSlot {
    pub(crate) slot: u32,
    pub(crate) is_const: bool,
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
    /// not a local (global/`state`/`undefined`/… or undeclared); codegen falls
    /// back to name-based resolution.
    pub(crate) ref_resolution: HashMap<u32, RefSlot>,
    /// Function/arrow AST node span → its `scopes` index.
    pub(crate) scope_by_span: HashMap<u32, usize>,
}

impl FuncScope {
    fn new(
        parent: usize,
        label: u32,
        node_span: u32,
        params: Vec<ParamInfo>,
        self_name: Option<String>,
        is_declaration: bool,
    ) -> Self {
        FuncScope {
            id: 0,
            parent,
            label,
            node_span,
            params,
            self_name,
            is_declaration,
            names: IndexMap::new(),
            captured: HashSet::new(),
            loop_declared: HashSet::new(),
            fresh_owns: HashSet::new(),
            children: Vec::new(),
            free_vars: IndexSet::new(),
            binding_spans: Vec::new(),
            local_refs: Vec::new(),
            free_refs: Vec::new(),
            upval_by_name: HashMap::new(),
            captures: Vec::new(),
            upval_count: 0,
            own_local_count: 0,
            uses_arguments: false,
            slot_kinds: Vec::new(),
        }
    }
}

/// Absolute frame slot for an own-local index under the `[params | upvals |
/// locals]` layout: the `nparams` params keep slots `0..nparams` (they arrive in
/// place as the call's arguments), then the `K` upvals occupy `nparams..nparams+K`,
/// then the remaining own locals are shifted up by `K`. (`own == own_local_count`
/// yields the self-reference slot just past all own locals.)
pub(crate) fn frame_abs(own: u32, nparams: u32, upval_count: u32) -> u32 {
    if own < nparams {
        own
    } else {
        own + upval_count
    }
}

/// Bottom-up capture resolution. Phase A propagates each scope's free variables
/// into its parent (children have lower ids than parents, so ascending order
/// visits children first). Phase B (descending: parents first) turns every
/// resolvable free variable into an upval — sourced from a parent local (which
/// it boxes) or a parent upval — and fixes `upval_count`. A final pass derives
/// each scope's own-local `slot_kinds`.
fn resolve_captures(scopes: &mut [FuncScope]) {
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
            if !scopes[parent].names.contains_key(&fv) {
                scopes[parent].free_vars.insert(fv);
            }
        }
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
            let (parent_abs, is_const) = if let Some(info) = scopes[parent].names.get(&fv).copied()
            {
                scopes[parent].captured.insert(info.slot);
                (
                    frame_abs(info.slot, parent_nparams, scopes[parent].upval_count),
                    info.is_const,
                )
            } else if let Some(&(idx, is_const)) = scopes[parent].upval_by_name.get(&fv) {
                // Capturing one of the parent's own upvals: its absolute slot is
                // `parent_nparams + idx` under the [params | upvals | locals] layout.
                (parent_nparams + idx, is_const)
            } else {
                // Not declared in any ancestor: a global/`state`/undeclared
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
fn finalize_tables(
    scopes: &[FuncScope],
) -> (
    HashMap<u32, u32>,
    HashMap<u32, RefSlot>,
    HashMap<u32, usize>,
) {
    let mut binding_slot = HashMap::new();
    let mut ref_resolution = HashMap::new();
    let mut scope_by_span = HashMap::new();
    for s in scopes {
        if s.node_span != u32::MAX {
            scope_by_span.insert(s.node_span, s.id);
        }
        let nparams = s.params.len() as u32;
        for &(span, own) in &s.binding_spans {
            binding_slot.insert(span, frame_abs(own, nparams, s.upval_count));
        }
        for &(span, own, is_const) in &s.local_refs {
            ref_resolution.insert(
                span,
                RefSlot {
                    slot: frame_abs(own, nparams, s.upval_count),
                    is_const,
                },
            );
        }
        for (span, name) in &s.free_refs {
            if s.self_name.as_deref() == Some(name.as_str()) {
                // Self-reference: the dedicated slot past all own locals.
                ref_resolution.insert(
                    *span,
                    RefSlot {
                        slot: frame_abs(s.own_local_count, nparams, s.upval_count),
                        is_const: true,
                    },
                );
            } else if let Some(&(idx, is_const)) = s.upval_by_name.get(name) {
                // The body's own upvals occupy slots [nparams, nparams + K).
                ref_resolution.insert(
                    *span,
                    RefSlot {
                        slot: nparams + idx,
                        is_const,
                    },
                );
            }
            // Otherwise a global/`state`/undeclared name: leave absent so codegen
            // falls back to name-based resolution.
        }
    }
    (binding_slot, ref_resolution, scope_by_span)
}
/// A resolved local-variable binding: its frame slot plus whether it was
/// declared `const` (so writes can be rejected at compile time).
#[derive(Copy, Clone, Debug)]
struct SlotInfo {
    slot: u32,
    is_const: bool,
}

/// Output of the analysis pass: the resolved `ProgramAnalysis`, the next free
/// label id (codegen continues the same allocation), and semantic diagnostics.
pub(crate) struct Analysis {
    pub(crate) program: ProgramAnalysis,
    pub(crate) next_label: u32,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

/// Run scope/capture analysis over the whole program.
pub(crate) fn analyze(program: &ast::Program) -> Analysis {
    let mut analyzer = Analyzer {
        next_label: 0,
        diagnostics: Vec::new(),
        loop_depth: 0,
    };
    let program = analyzer.analyze_program(program);
    Analysis {
        program,
        next_label: analyzer.next_label,
        diagnostics: analyzer.diagnostics,
    }
}

/// Analysis-pass state: a label-id allocator (numbering is shared with codegen,
/// handed off via `Analysis::next_label`) and collected diagnostics.
struct Analyzer {
    next_label: u32,
    diagnostics: Vec<Diagnostic>,
    /// Lexical loop-nesting depth at the current walk position (reset to 0 when
    /// entering a nested function body). A `let`/`const` declared while this is
    /// `> 0` is a per-iteration binding: if also captured, it gets a fresh cell
    /// each iteration rather than one eager cell, so its slot kind is `Plain`.
    loop_depth: u32,
}

impl Analyzer {
    /// Allocate a fresh label id.
    fn new_label(&mut self) -> u32 {
        let id = self.next_label;
        self.next_label += 1;
        id
    }

    /// Record a diagnostic.
    fn error(&mut self, span: u32, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            message: message.into(),
        });
    }

    // ── Phase 3: scope / capture analysis ───────────────────────────────

    /// Walk the whole AST once, building a `FuncScope` per function (and the
    /// root). The walk records every binding occurrence and identifier
    /// reference, keyed by span; a reference that doesn't resolve to a local
    /// here is a free variable. A second pass propagates free variables up the
    /// scope tree, turns the resolvable ones into captures (boxing the captured
    /// owner slot), and fixes each scope's `upval_count`. A final pass converts
    /// the recorded own-slots into absolute frame slots, producing the
    /// span-keyed `binding_slot` / `ref_resolution` / `scope_by_span` tables
    /// that codegen consults — codegen keeps no scope state of its own.
    fn analyze_program(&mut self, program: &ast::Program) -> ProgramAnalysis {
        let mut scopes = Vec::new();
        let root = self.analyze_top_level(program, &mut scopes);
        resolve_captures(&mut scopes);
        let (binding_slot, ref_resolution, scope_by_span) = finalize_tables(&scopes);
        ProgramAnalysis {
            scopes,
            root,
            binding_slot,
            ref_resolution,
            scope_by_span,
        }
    }

    /// Build the root `FuncScope` and walk the top-level body.
    fn analyze_top_level(&mut self, program: &ast::Program, scopes: &mut Vec<FuncScope>) -> usize {
        let label = self.new_label();
        let mut scope = FuncScope::new(usize::MAX, label, u32::MAX, Vec::new(), None, false);
        let mut block_scopes: Vec<IndexMap<String, (u32, bool)>> = vec![IndexMap::new()];
        let mut next_slot = 0u32;

        self.analyze_hoist(&program.body, &mut scope, &mut block_scopes, &mut next_slot);
        self.analyze_stmts(
            &program.body,
            &mut scope,
            &mut block_scopes,
            &mut next_slot,
            scopes,
        );

        scope.own_local_count = next_slot;
        self.push_scope(scope, scopes)
    }

    /// Push a finished scope: assign its final id and fix up its children's
    /// `parent` pointers (children were pushed first, with a placeholder parent).
    fn push_scope(&self, mut scope: FuncScope, scopes: &mut Vec<FuncScope>) -> usize {
        let id = scopes.len();
        scope.id = id;
        for &child in &scope.children {
            scopes[child].parent = id;
        }
        scopes.push(scope);
        id
    }

    /// Pre-register function-scoped names (`var` bindings and function
    /// declarations) so forward references resolve, mirroring JS hoisting.
    /// Recurses through blocks/conditionals/loops but never into nested
    /// functions. Binding spans/slots are recorded here; the declaration site
    /// reuses the same slot.
    fn analyze_hoist(
        &mut self,
        stmts: &[ast::Statement],
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        next_slot: &mut u32,
    ) {
        for stmt in stmts {
            self.analyze_hoist_stmt(stmt, scope, block_scopes, next_slot);
        }
    }

    fn analyze_hoist_stmt(
        &mut self,
        stmt: &ast::Statement,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        next_slot: &mut u32,
    ) {
        match stmt {
            ast::Statement::VariableDeclaration(decl)
                if decl.kind == ast::VariableDeclarationKind::Var =>
            {
                for d in &decl.declarations {
                    self.hoist_var_pattern(&d.id, scope, block_scopes, next_slot);
                }
            }
            ast::Statement::FunctionDeclaration(f) => {
                if let Some(id) = &f.id {
                    self.analyze_register_name(
                        id.name.as_str(),
                        id.span.start,
                        false,
                        true,
                        scope,
                        block_scopes,
                        next_slot,
                    );
                }
            }
            ast::Statement::BlockStatement(b) => {
                self.analyze_hoist(&b.body, scope, block_scopes, next_slot)
            }
            ast::Statement::IfStatement(s) => {
                self.analyze_hoist_stmt(&s.consequent, scope, block_scopes, next_slot);
                if let Some(alt) = &s.alternate {
                    self.analyze_hoist_stmt(alt, scope, block_scopes, next_slot);
                }
            }
            ast::Statement::WhileStatement(s) => {
                self.analyze_hoist_stmt(&s.body, scope, block_scopes, next_slot)
            }
            ast::Statement::DoWhileStatement(s) => {
                self.analyze_hoist_stmt(&s.body, scope, block_scopes, next_slot)
            }
            ast::Statement::ForStatement(s) => {
                if let Some(ast::ForStatementInit::VariableDeclaration(decl)) = &s.init {
                    if decl.kind == ast::VariableDeclarationKind::Var {
                        for d in &decl.declarations {
                            self.hoist_var_pattern(&d.id, scope, block_scopes, next_slot);
                        }
                    }
                }
                self.analyze_hoist_stmt(&s.body, scope, block_scopes, next_slot);
            }
            ast::Statement::ForOfStatement(s) => {
                if let ast::ForStatementLeft::VariableDeclaration(decl) = &s.left {
                    if decl.kind == ast::VariableDeclarationKind::Var {
                        for d in &decl.declarations {
                            self.hoist_var_pattern(&d.id, scope, block_scopes, next_slot);
                        }
                    }
                }
                self.analyze_hoist_stmt(&s.body, scope, block_scopes, next_slot);
            }
            ast::Statement::ForInStatement(s) => {
                if let ast::ForStatementLeft::VariableDeclaration(decl) = &s.left {
                    if decl.kind == ast::VariableDeclarationKind::Var {
                        for d in &decl.declarations {
                            self.hoist_var_pattern(&d.id, scope, block_scopes, next_slot);
                        }
                    }
                }
                self.analyze_hoist_stmt(&s.body, scope, block_scopes, next_slot);
            }
            ast::Statement::SwitchStatement(s) => {
                // `var` declarations inside case clauses are function-scoped.
                for case in &s.cases {
                    for cs in &case.consequent {
                        self.analyze_hoist_stmt(cs, scope, block_scopes, next_slot);
                    }
                }
            }
            _ => {}
        }
    }

    /// Register every binding identifier in a `var` pattern (function-scoped).
    fn hoist_var_pattern(
        &mut self,
        pat: &ast::BindingPattern,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        next_slot: &mut u32,
    ) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                self.analyze_register_name(
                    id.name.as_str(),
                    id.span.start,
                    false,
                    true,
                    scope,
                    block_scopes,
                    next_slot,
                );
            }
            ast::BindingPattern::AssignmentPattern(ap) => {
                self.hoist_var_pattern(&ap.left, scope, block_scopes, next_slot)
            }
            ast::BindingPattern::ArrayPattern(arr) => {
                for el in arr.elements.iter().flatten() {
                    self.hoist_var_pattern(el, scope, block_scopes, next_slot);
                }
                if let Some(rest) = &arr.rest {
                    self.hoist_var_pattern(&rest.argument, scope, block_scopes, next_slot);
                }
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.hoist_var_pattern(&prop.value, scope, block_scopes, next_slot);
                }
                if let Some(rest) = &obj.rest {
                    self.hoist_var_pattern(&rest.argument, scope, block_scopes, next_slot);
                }
            }
        }
    }

    fn analyze_stmts(
        &mut self,
        stmts: &[ast::Statement],
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        for stmt in stmts {
            self.analyze_stmt(stmt, scope, block_scopes, next_slot, scopes);
        }
    }

    fn analyze_stmt(
        &mut self,
        stmt: &ast::Statement,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        match stmt {
            ast::Statement::VariableDeclaration(decl) => {
                self.analyze_var_decl(decl, scope, block_scopes, next_slot, scopes);
            }
            ast::Statement::FunctionDeclaration(f) => {
                // Name already hoisted; build the function's scope.
                let child = self.build_function_scope(f, true, scopes);
                scope.children.push(child);
            }
            ast::Statement::BlockStatement(block) => {
                block_scopes.push(IndexMap::new());
                self.analyze_stmts(&block.body, scope, block_scopes, next_slot, scopes);
                block_scopes.pop();
            }
            ast::Statement::IfStatement(s) => {
                self.analyze_expr(&s.test, scope, block_scopes, scopes);
                self.analyze_stmt(&s.consequent, scope, block_scopes, next_slot, scopes);
                if let Some(alt) = &s.alternate {
                    self.analyze_stmt(alt, scope, block_scopes, next_slot, scopes);
                }
            }
            ast::Statement::WhileStatement(s) => {
                self.analyze_expr(&s.test, scope, block_scopes, scopes);
                self.loop_depth += 1;
                self.analyze_stmt(&s.body, scope, block_scopes, next_slot, scopes);
                self.loop_depth -= 1;
            }
            ast::Statement::DoWhileStatement(s) => {
                self.loop_depth += 1;
                self.analyze_stmt(&s.body, scope, block_scopes, next_slot, scopes);
                self.loop_depth -= 1;
                self.analyze_expr(&s.test, scope, block_scopes, scopes);
            }
            ast::Statement::ForStatement(s) => {
                // The head declaration and body are all per-iteration: a `let`
                // declared in the head (`for (let i …)`) is loop-declared too.
                self.loop_depth += 1;
                if let Some(init) = &s.init {
                    match init {
                        ast::ForStatementInit::VariableDeclaration(decl) => {
                            self.analyze_var_decl(decl, scope, block_scopes, next_slot, scopes);
                        }
                        _ => {
                            if let Some(expr) = init.as_expression() {
                                self.analyze_expr(expr, scope, block_scopes, scopes);
                            }
                        }
                    }
                }
                if let Some(test) = &s.test {
                    self.analyze_expr(test, scope, block_scopes, scopes);
                }
                if let Some(update) = &s.update {
                    self.analyze_expr(update, scope, block_scopes, scopes);
                }
                self.analyze_stmt(&s.body, scope, block_scopes, next_slot, scopes);
                self.loop_depth -= 1;
            }
            ast::Statement::ExpressionStatement(es) => {
                self.analyze_expr(&es.expression, scope, block_scopes, scopes);
            }
            ast::Statement::ReturnStatement(r) => {
                if let Some(val) = &r.argument {
                    self.analyze_expr(val, scope, block_scopes, scopes);
                }
            }
            ast::Statement::BreakStatement(_)
            | ast::Statement::ContinueStatement(_)
            | ast::Statement::EmptyStatement(_) => {}
            // for-of / for-in: walk the iterable/object RHS, declare the loop
            // binding (let/const -> block slot; var was hoisted), then the body.
            // A fresh block scope wraps the head + body so the loop binding does
            // not leak past the loop.
            ast::Statement::ForOfStatement(s) => {
                self.analyze_expr(&s.right, scope, block_scopes, scopes);
                block_scopes.push(IndexMap::new());
                self.loop_depth += 1;
                self.analyze_for_head(&s.left, scope, block_scopes, next_slot, scopes);
                self.analyze_stmt(&s.body, scope, block_scopes, next_slot, scopes);
                self.loop_depth -= 1;
                block_scopes.pop();
            }
            ast::Statement::ForInStatement(s) => {
                self.analyze_expr(&s.right, scope, block_scopes, scopes);
                block_scopes.push(IndexMap::new());
                self.loop_depth += 1;
                self.analyze_for_head(&s.left, scope, block_scopes, next_slot, scopes);
                self.analyze_stmt(&s.body, scope, block_scopes, next_slot, scopes);
                self.loop_depth -= 1;
                block_scopes.pop();
            }
            // `switch`: the whole body shares **one** lexical block (a `let` in
            // one `case` is visible in later cases), so push a single block scope
            // around all the case tests and consequents. `break`/`continue`
            // targeting is handled in codegen (break-only context for the switch).
            ast::Statement::SwitchStatement(s) => {
                self.analyze_expr(&s.discriminant, scope, block_scopes, scopes);
                block_scopes.push(IndexMap::new());
                for case in &s.cases {
                    if let Some(test) = &case.test {
                        self.analyze_expr(test, scope, block_scopes, scopes);
                    }
                    for cs in &case.consequent {
                        self.analyze_stmt(cs, scope, block_scopes, next_slot, scopes);
                    }
                }
                block_scopes.pop();
            }
            _ => {}
        }
    }

    /// Declare the loop binding of a `for-of`/`for-in` head. Only the
    /// `let`/`const`/`var x` declaration form is resolved here (the binding
    /// gets a slot exactly like a normal declaration; `var` was already
    /// hoisted). The bare-assignment-target form (`for (x of …)`) is left
    /// unresolved — codegen rejects it.
    fn analyze_for_head(
        &mut self,
        left: &ast::ForStatementLeft,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        if let ast::ForStatementLeft::VariableDeclaration(decl) = left {
            self.analyze_var_decl(decl, scope, block_scopes, next_slot, scopes);
        }
    }

    fn analyze_var_decl(
        &mut self,
        decl: &ast::VariableDeclaration,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        let is_const = decl.kind == ast::VariableDeclarationKind::Const;
        let is_var = decl.kind == ast::VariableDeclarationKind::Var;
        for d in &decl.declarations {
            // `var` names were hoisted; `let`/`const` register here.
            self.analyze_declare_pattern(
                &d.id,
                is_const,
                is_var,
                scope,
                block_scopes,
                next_slot,
                scopes,
            );
            if let Some(init) = &d.init {
                self.analyze_expr(init, scope, block_scopes, scopes);
            }
        }
    }

    /// Register `let`/`const` binding names (skipped for already-hoisted `var`s)
    /// and analyze any pattern default expressions for free variables.
    fn analyze_declare_pattern(
        &mut self,
        pat: &ast::BindingPattern,
        is_const: bool,
        is_var: bool,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                if !is_var {
                    self.analyze_register_name(
                        id.name.as_str(),
                        id.span.start,
                        is_const,
                        false,
                        scope,
                        block_scopes,
                        next_slot,
                    );
                }
            }
            ast::BindingPattern::AssignmentPattern(ap) => {
                self.analyze_declare_pattern(
                    &ap.left,
                    is_const,
                    is_var,
                    scope,
                    block_scopes,
                    next_slot,
                    scopes,
                );
                self.analyze_expr(&ap.right, scope, block_scopes, scopes);
            }
            ast::BindingPattern::ArrayPattern(arr) => {
                for el in arr.elements.iter().flatten() {
                    self.analyze_declare_pattern(
                        el,
                        is_const,
                        is_var,
                        scope,
                        block_scopes,
                        next_slot,
                        scopes,
                    );
                }
                if let Some(rest) = &arr.rest {
                    self.analyze_declare_pattern(
                        &rest.argument,
                        is_const,
                        is_var,
                        scope,
                        block_scopes,
                        next_slot,
                        scopes,
                    );
                }
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    if prop.computed {
                        if let Some(expr) = prop.key.as_expression() {
                            self.analyze_expr(expr, scope, block_scopes, scopes);
                        }
                    }
                    self.analyze_declare_pattern(
                        &prop.value,
                        is_const,
                        is_var,
                        scope,
                        block_scopes,
                        next_slot,
                        scopes,
                    );
                }
                if let Some(rest) = &obj.rest {
                    self.analyze_declare_pattern(
                        &rest.argument,
                        is_const,
                        is_var,
                        scope,
                        block_scopes,
                        next_slot,
                        scopes,
                    );
                }
            }
        }
    }

    /// Register a binding name, recording its span→slot mapping. `var` names
    /// live in the function scope (`block_scopes[0]`) and reuse an existing
    /// slot; `let`/`const` get a fresh slot in the innermost block.
    fn analyze_register_name(
        &mut self,
        name: &str,
        span: u32,
        is_const: bool,
        is_var: bool,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        next_slot: &mut u32,
    ) -> u32 {
        if name == "state" {
            self.error(span, "cannot shadow the blessed `state` object");
            return 0;
        }
        let slot = if is_var {
            if let Some(&(slot, _)) = block_scopes[0].get(name) {
                slot
            } else {
                let slot = *next_slot;
                *next_slot += 1;
                block_scopes[0].insert(name.to_string(), (slot, false));
                scope.names.entry(name.to_string()).or_insert(SlotInfo {
                    slot,
                    is_const: false,
                });
                slot
            }
        } else {
            let slot = *next_slot;
            *next_slot += 1;
            block_scopes
                .last_mut()
                .expect("a block scope is always open")
                .insert(name.to_string(), (slot, is_const));
            scope
                .names
                .entry(name.to_string())
                .or_insert(SlotInfo { slot, is_const });
            // A `let`/`const` declared inside a loop is a per-iteration binding;
            // record it so a captured one becomes `fresh_owns` (Plain + per-iter
            // FreshCell) rather than an eagerly-boxed shared cell.
            if self.loop_depth > 0 {
                scope.loop_declared.insert(slot);
            }
            slot
        };
        scope.binding_spans.push((span, slot));
        slot
    }

    fn analyze_expr(
        &mut self,
        expr: &ast::Expression,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        scopes: &mut Vec<FuncScope>,
    ) {
        match expr {
            ast::Expression::Identifier(id) => {
                self.analyze_ref(id.name.as_str(), id.span.start, scope, block_scopes);
            }
            ast::Expression::AssignmentExpression(a) => {
                self.analyze_assign_target(&a.left, scope, block_scopes, scopes);
                self.analyze_expr(&a.right, scope, block_scopes, scopes);
            }
            ast::Expression::UpdateExpression(u) => {
                self.analyze_simple_target(&u.argument, scope, block_scopes, scopes);
            }
            ast::Expression::BinaryExpression(b) => {
                self.analyze_expr(&b.left, scope, block_scopes, scopes);
                self.analyze_expr(&b.right, scope, block_scopes, scopes);
            }
            ast::Expression::UnaryExpression(u) => {
                self.analyze_expr(&u.argument, scope, block_scopes, scopes);
            }
            ast::Expression::LogicalExpression(l) => {
                self.analyze_expr(&l.left, scope, block_scopes, scopes);
                self.analyze_expr(&l.right, scope, block_scopes, scopes);
            }
            ast::Expression::ConditionalExpression(c) => {
                self.analyze_expr(&c.test, scope, block_scopes, scopes);
                self.analyze_expr(&c.consequent, scope, block_scopes, scopes);
                self.analyze_expr(&c.alternate, scope, block_scopes, scopes);
            }
            ast::Expression::CallExpression(c) => {
                self.analyze_expr(&c.callee, scope, block_scopes, scopes);
                for arg in &c.arguments {
                    if let Some(e) = arg.as_expression() {
                        self.analyze_expr(e, scope, block_scopes, scopes);
                    }
                }
            }
            ast::Expression::StaticMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
            }
            ast::Expression::ComputedMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
                self.analyze_expr(&m.expression, scope, block_scopes, scopes);
            }
            ast::Expression::ArrayExpression(arr) => {
                for el in &arr.elements {
                    if let Some(e) = el.as_expression() {
                        self.analyze_expr(e, scope, block_scopes, scopes);
                    }
                }
            }
            ast::Expression::ObjectExpression(obj) => {
                for prop in &obj.properties {
                    match prop {
                        ast::ObjectPropertyKind::ObjectProperty(p) => {
                            if p.computed {
                                if let Some(e) = p.key.as_expression() {
                                    self.analyze_expr(e, scope, block_scopes, scopes);
                                }
                            }
                            self.analyze_expr(&p.value, scope, block_scopes, scopes);
                        }
                        ast::ObjectPropertyKind::SpreadProperty(s) => {
                            self.analyze_expr(&s.argument, scope, block_scopes, scopes);
                        }
                    }
                }
            }
            ast::Expression::TemplateLiteral(tl) => {
                for e in &tl.expressions {
                    self.analyze_expr(e, scope, block_scopes, scopes);
                }
            }
            ast::Expression::SequenceExpression(seq) => {
                for e in &seq.expressions {
                    self.analyze_expr(e, scope, block_scopes, scopes);
                }
            }
            ast::Expression::ParenthesizedExpression(p) => {
                self.analyze_expr(&p.expression, scope, block_scopes, scopes);
            }
            ast::Expression::ChainExpression(chain) => {
                self.analyze_chain_element(&chain.expression, scope, block_scopes, scopes);
            }
            ast::Expression::FunctionExpression(f) => {
                let child = self.build_function_scope(f, false, scopes);
                scope.children.push(child);
            }
            ast::Expression::ArrowFunctionExpression(a) => {
                let child = self.build_arrow_scope(a, scopes);
                scope.children.push(child);
            }
            _ => {}
        }
    }

    /// Record an identifier reference: to an own local (resolved now) or as a
    /// free variable (resolved to an upval/self/global during finalization).
    fn analyze_ref(
        &mut self,
        name: &str,
        span: u32,
        scope: &mut FuncScope,
        block_scopes: &[IndexMap<String, (u32, bool)>],
    ) {
        if let Some((slot, is_const)) = self.analyze_resolve_name(name, block_scopes) {
            scope.local_refs.push((span, slot, is_const));
        } else {
            // An unshadowed `arguments` reference uses the frame's argument array
            // (the compiler emits `Arguments`, not a `Local`); flag the scope so
            // the prologue materializes that array before normalizing the args.
            if name == "arguments" {
                scope.uses_arguments = true;
            }
            scope.free_refs.push((span, name.to_string()));
            scope.free_vars.insert(name.to_string());
        }
    }

    /// Walk an assignment target, recording references (the written identifier,
    /// plus any member-object / computed-key / default expressions).
    fn analyze_assign_target(
        &mut self,
        target: &ast::AssignmentTarget,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        scopes: &mut Vec<FuncScope>,
    ) {
        match target {
            ast::AssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.analyze_ref(id.name.as_str(), id.span.start, scope, block_scopes);
            }
            ast::AssignmentTarget::StaticMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
            }
            ast::AssignmentTarget::ComputedMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
                self.analyze_expr(&m.expression, scope, block_scopes, scopes);
            }
            ast::AssignmentTarget::ArrayAssignmentTarget(arr) => {
                for el in arr.elements.iter().flatten() {
                    self.analyze_assign_maybe_default(el, scope, block_scopes, scopes);
                }
                if let Some(rest) = &arr.rest {
                    self.analyze_assign_target(&rest.target, scope, block_scopes, scopes);
                }
            }
            ast::AssignmentTarget::ObjectAssignmentTarget(obj) => {
                for prop in &obj.properties {
                    match prop {
                        ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                            self.analyze_ref(
                                p.binding.name.as_str(),
                                p.binding.span.start,
                                scope,
                                block_scopes,
                            );
                            if let Some(init) = &p.init {
                                self.analyze_expr(init, scope, block_scopes, scopes);
                            }
                        }
                        ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                            if p.computed {
                                if let Some(e) = p.name.as_expression() {
                                    self.analyze_expr(e, scope, block_scopes, scopes);
                                }
                            }
                            self.analyze_assign_maybe_default(
                                &p.binding,
                                scope,
                                block_scopes,
                                scopes,
                            );
                        }
                    }
                }
                if let Some(rest) = &obj.rest {
                    self.analyze_assign_target(&rest.target, scope, block_scopes, scopes);
                }
            }
            _ => {}
        }
    }

    fn analyze_assign_maybe_default(
        &mut self,
        m: &ast::AssignmentTargetMaybeDefault,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        scopes: &mut Vec<FuncScope>,
    ) {
        match m {
            ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(wd) => {
                self.analyze_expr(&wd.init, scope, block_scopes, scopes);
                self.analyze_assign_target(&wd.binding, scope, block_scopes, scopes);
            }
            other => {
                if let Some(t) = other.as_assignment_target() {
                    self.analyze_assign_target(t, scope, block_scopes, scopes);
                }
            }
        }
    }

    /// Like `analyze_assign_target`, for the `SimpleAssignmentTarget` of `++`/`--`.
    fn analyze_simple_target(
        &mut self,
        target: &ast::SimpleAssignmentTarget,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        scopes: &mut Vec<FuncScope>,
    ) {
        match target {
            ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.analyze_ref(id.name.as_str(), id.span.start, scope, block_scopes);
            }
            ast::SimpleAssignmentTarget::StaticMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
            }
            ast::SimpleAssignmentTarget::ComputedMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
                self.analyze_expr(&m.expression, scope, block_scopes, scopes);
            }
            _ => {}
        }
    }

    /// Walk a `ChainElement` (optional chaining) for references.
    fn analyze_chain_element(
        &mut self,
        el: &ast::ChainElement,
        scope: &mut FuncScope,
        block_scopes: &mut Vec<IndexMap<String, (u32, bool)>>,
        scopes: &mut Vec<FuncScope>,
    ) {
        match el {
            ast::ChainElement::CallExpression(c) => {
                self.analyze_expr(&c.callee, scope, block_scopes, scopes);
                for arg in &c.arguments {
                    if let Some(e) = arg.as_expression() {
                        self.analyze_expr(e, scope, block_scopes, scopes);
                    }
                }
            }
            ast::ChainElement::StaticMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
            }
            ast::ChainElement::ComputedMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
                self.analyze_expr(&m.expression, scope, block_scopes, scopes);
            }
            _ => {}
        }
    }

    /// Resolve a name against the current function's block scopes (innermost
    /// first), returning its own-slot and const-ness, or `None` if not local.
    fn analyze_resolve_name(
        &self,
        name: &str,
        block_scopes: &[IndexMap<String, (u32, bool)>],
    ) -> Option<(u32, bool)> {
        block_scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    /// Build a `FuncScope` for a function declaration or expression.
    fn build_function_scope(
        &mut self,
        func: &ast::Function,
        is_declaration: bool,
        scopes: &mut Vec<FuncScope>,
    ) -> usize {
        let label = self.new_label();
        let params = self.collect_params(&func.params);
        let self_name = func.id.as_ref().map(|id| id.name.as_str().to_string());
        let mut scope = FuncScope::new(
            usize::MAX,
            label,
            func.span.start,
            params,
            self_name,
            is_declaration,
        );
        let body = func.body.as_ref().map(|b| &b.statements[..]).unwrap_or(&[]);
        self.analyze_function_body(&mut scope, &func.params, body, scopes);
        self.push_scope(scope, scopes)
    }

    /// Build a `FuncScope` for an arrow function.
    fn build_arrow_scope(
        &mut self,
        arrow: &ast::ArrowFunctionExpression,
        scopes: &mut Vec<FuncScope>,
    ) -> usize {
        let label = self.new_label();
        let params = self.collect_params(&arrow.params);
        let mut scope = FuncScope::new(usize::MAX, label, arrow.span.start, params, None, false);
        self.analyze_function_body(&mut scope, &arrow.params, &arrow.body.statements, scopes);
        self.push_scope(scope, scopes)
    }

    /// Shared body of `build_function_scope` / `build_arrow_scope`: seed the
    /// param slots, analyze param defaults, hoist, then walk the body.
    fn analyze_function_body(
        &mut self,
        scope: &mut FuncScope,
        params: &ast::FormalParameters,
        body: &[ast::Statement],
        scopes: &mut Vec<FuncScope>,
    ) {
        let mut block_scopes: Vec<IndexMap<String, (u32, bool)>> = vec![IndexMap::new()];
        let mut next_slot = scope.params.len() as u32;
        for (i, p) in scope.params.iter().enumerate() {
            block_scopes[0].insert(p.name.clone(), (i as u32, false));
            scope.names.insert(
                p.name.clone(),
                SlotInfo {
                    slot: i as u32,
                    is_const: false,
                },
            );
        }
        // Param default expressions (`function f(a, b = a)`) — params are now in
        // scope, so a default may reference an earlier one.
        for p in &params.items {
            if let Some(init) = &p.initializer {
                self.analyze_expr(init, scope, &mut block_scopes, scopes);
            }
        }
        self.analyze_hoist(body, scope, &mut block_scopes, &mut next_slot);
        // A nested function is a fresh frame: its bindings are not per-iteration
        // with respect to any loop enclosing the *definition*. Reset loop depth
        // for the body walk and restore it afterwards.
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        self.analyze_stmts(body, scope, &mut block_scopes, &mut next_slot, scopes);
        self.loop_depth = saved_loop_depth;
        scope.own_local_count = next_slot;
    }

    /// Flatten formal parameters into ordered `ParamInfo` (one per binding name).
    fn collect_params(&self, params: &ast::FormalParameters) -> Vec<ParamInfo> {
        let mut out = Vec::new();
        for param in &params.items {
            let has_default = param.initializer.is_some();
            let (names, _) = self.analyze_param_info(&param.pattern);
            for n in names {
                out.push(ParamInfo {
                    name: n,
                    has_default,
                });
            }
        }
        out
    }

    /// Extract binding names (recursively) from a parameter pattern.
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
                    names.extend(self.analyze_param_info(el).0);
                }
                if let Some(rest) = &arr.rest {
                    names.extend(self.analyze_param_info(&rest.argument).0);
                }
                (names, false)
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                let mut names = Vec::new();
                for prop in &obj.properties {
                    names.extend(self.analyze_param_info(&prop.value).0);
                }
                if let Some(rest) = &obj.rest {
                    names.extend(self.analyze_param_info(&rest.argument).0);
                }
                (names, false)
            }
        }
    }
}
