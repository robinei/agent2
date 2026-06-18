//! Scope & capture analysis (Pass 1): resolves every binding and identifier
//! reference to a frame slot — keyed by source span — and computes closure
//! capture lists and slot boxing. Codegen consumes the resulting
//! `ProgramAnalysis` and keeps no scope state of its own. See `COMPILER_PLAN.md`.

pub(crate) mod captures;
pub(crate) mod const_fns;
pub(crate) mod scope;
pub(crate) mod walk;

pub(crate) use captures::{ProgramAnalysis, RefSlot};
pub(crate) use const_fns::ConstValue;
#[allow(unused_imports)]
pub(crate) use scope::{FuncScope, ParamInfo, SlotInfo};

use indexmap::IndexMap;
use oxc_ast::ast;

use crate::diag::Diagnostic;

use captures::finalize_tables;
use const_fns::resolve_const_functions;
pub(crate) use scope::frame_abs;

/// How a name resolves within a function's lexical (block) scopes. A `Slot` is an
/// ordinary frame local (param / `let` / `var`); a `Const` is a compile-time
/// constant binding that never reaches the frame.
#[derive(Clone, Debug)]
pub(crate) enum NameRes {
    Slot { slot: u32, is_const: bool },
    Const(ConstValue),
}

/// The lexical block-scope stack threaded through analysis: innermost scope last.
pub(crate) type BlockScopes = Vec<IndexMap<String, NameRes>>;

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
        current_super: None,
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
pub(super) struct Analyzer {
    pub(super) next_label: u32,
    pub(super) diagnostics: Vec<Diagnostic>,
    /// Lexical loop-nesting depth at the current walk position (reset to 0 when
    /// entering a nested function body). A `let`/`const` declared while this is
    /// `> 0` is a per-iteration binding: if also captured, it gets a fresh cell
    /// each iteration rather than one eager cell, so its slot kind is `Plain`.
    pub(super) loop_depth: u32,
    /// The superclass *identifier name* of the `extends` clause for the class
    /// whose method/constructor body is currently being analyzed (`None`
    /// outside a derived class). A `super` reference (Step 7b) resolves to this
    /// name, so the existing capture machinery threads the parent constructor in
    /// as an upval. Lexically scoped like `this`: inherited by nested arrows,
    /// cleared on entering a nested non-arrow function, re-set per nested class.
    pub(super) current_super: Option<String>,
}

impl Analyzer {
    /// Allocate a fresh label id.
    pub(super) fn new_label(&mut self) -> u32 {
        let id = self.next_label;
        self.next_label += 1;
        id
    }

    /// Record a semantic diagnostic from the analyzer.
    pub(super) fn error(&mut self, span: u32, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            message: message.into(),
            kind: super::diag::DiagKind::Semantic,
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
    pub(super) fn analyze_program(&mut self, program: &ast::Program) -> ProgramAnalysis {
        let mut scopes = Vec::new();
        let root = self.analyze_top_level(program, &mut scopes);
        // Phase F: identify constant functions (a fixpoint over capture
        // resolution) and leave `scopes` capture-resolved with them registered,
        // so their references resolve to `Fn` values rather than captures —
        // breaking self/mutual-recursion captures.
        let const_fns = resolve_const_functions(&mut scopes);
        let (
            binding_slot,
            ref_resolution,
            binding_immutable,
            binding_captured,
            const_refs,
            scope_by_span,
        ) = finalize_tables(&scopes, &const_fns);
        ProgramAnalysis {
            scopes,
            root,
            binding_slot,
            ref_resolution,
            binding_immutable,
            binding_captured,
            const_refs,
            const_fn_scopes: const_fns,
            scope_by_span,
        }
    }

    /// Build the root `FuncScope` and walk the top-level body.
    pub(super) fn analyze_top_level(
        &mut self,
        program: &ast::Program,
        scopes: &mut Vec<FuncScope>,
    ) -> usize {
        let label = self.new_label();
        let mut scope = FuncScope::new(
            usize::MAX,
            label,
            u32::MAX,
            u32::MAX,
            Vec::new(),
            None,
            false,
        );
        let mut block_scopes: BlockScopes = vec![IndexMap::new()];
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
    pub(super) fn push_scope(&self, mut scope: FuncScope, scopes: &mut Vec<FuncScope>) -> usize {
        let id = scopes.len();
        scope.id = id;
        for &child in &scope.children {
            scopes[child].parent = id;
        }
        scopes.push(scope);
        id
    }
}
