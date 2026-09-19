//! Incremental scope analysis: one growing `ProgramAnalysis`, fed fragments.
//!
//! The one-shot [`super::analyze`] walks a whole program and hands back a
//! finished `ProgramAnalysis`. This is the same walk with its state held open
//! between calls: the root `FuncScope`, the top-level block scopes, the slot
//! allocator and the label allocator all persist, so a name declared by one
//! fragment resolves in the next and each fragment allocates its own slots
//! above the ones already taken.
//!
//! **Pristine in, resolved out.** `resolve_const_functions` and
//! `resolve_captures` mutate scopes destructively — clearing and refilling
//! capture state, renumbering slots — so they cannot be run twice over the same
//! scopes. [`IncrementalAnalyzer`] therefore keeps the walk's output
//! *unresolved* and clones it for each re-resolution. That is what makes "the
//! analysis simply accumulates" true rather than approximately true.
//!
//! **The root is scope 0, not the last scope.** `resolve_captures` requires a
//! child's id to be lower than its parent's, and the one-shot builder satisfies
//! that by pushing the root last. Under accumulation the root cannot be last —
//! a later fragment's nested functions would land above it — so it is pinned at
//! index 0 instead. That satisfies the same invariant: a scope with no parent
//! needs nothing done before its children, and every genuine parent/child pair
//! still has the child lower.

use indexmap::IndexMap;
use oxc_ast::ast;

use crate::diag::Diagnostic;
use crate::span::Span;

use super::captures::{ProgramAnalysis, finalize_tables};
use super::const_fns::resolve_const_functions;
use super::scope::FuncScope;
use super::{Analyzer, BlockScopes, NameRes};

/// The root scope's index in an incremental unit. Fixed, because the root is
/// the one scope that outlives every fragment.
pub(crate) const ROOT: usize = 0;

/// Analysis state held open across fragments.
pub(crate) struct IncrementalAnalyzer {
    /// Every scope as the walk produced it — never capture-resolved, never
    /// compacted. `pristine[ROOT]` is the root, growing as fragments arrive;
    /// the rest are nested function scopes in creation order (children before
    /// parents, which capture resolution requires).
    pristine: Vec<FuncScope>,
    /// The top-level lexical block scopes, persisted so a `let` in one
    /// fragment is in scope for the next.
    block_scopes: BlockScopes,
    /// Next free own-slot in the root frame. Declaration order across the
    /// whole unit, which is what makes slot indices stable.
    next_slot: u32,
    /// Label allocator, shared with codegen the way the one-shot path shares
    /// it via `Analysis::next_label`.
    next_label: u32,
}

/// One fragment's analysis result.
pub(crate) struct FragmentAnalysis {
    pub(crate) program: ProgramAnalysis,
    pub(crate) next_label: u32,
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// Root own-slots `[first_new_slot, root_local_count)` — the ones this
    /// fragment declared, which its prologue must allocate with
    /// `Instr::ExtendFrame`.
    pub(crate) first_new_slot: u32,
}

impl IncrementalAnalyzer {
    pub(crate) fn new() -> Self {
        let mut analyzer = Analyzer {
            next_label: 0,
            diagnostics: Vec::new(),
            loop_depth: 0,
            current_super: None,
        };
        // The root's label, allocated first exactly as `analyze_top_level`
        // does, so label numbering matches the one-shot path.
        let label = analyzer.new_label();
        let mut root = FuncScope::new(usize::MAX, label, u32::MAX, u32::MAX, Vec::new(), None, false);
        root.id = ROOT;
        Self {
            pristine: vec![root],
            block_scopes: vec![IndexMap::new()],
            next_slot: 0,
            next_label: analyzer.next_label,
        }
    }

    /// Walk one fragment into the accumulated analysis and re-resolve.
    ///
    /// `program` is the parse of the shared buffer, in which only this
    /// fragment's text is live — so its top-level body holds exactly this
    /// fragment's statements, and every span it carries is already an offset
    /// into the whole unit (D2).
    pub(crate) fn feed(&mut self, program: &ast::Program) -> FragmentAnalysis {
        let mut analyzer = Analyzer {
            next_label: self.next_label,
            diagnostics: Vec::new(),
            loop_depth: 0,
            current_super: None,
        };

        let first_new_slot = self.next_slot;
        // Top-level bindings as they stood before this fragment, so a
        // redeclaration can be told from a fresh name. Only the outermost
        // block scope: a `let` inside a nested block legitimately shadows.
        let before: IndexMap<String, NameRes> = top_level_bindings(&self.block_scopes);

        // Take the root out to walk into it; `pristine` meanwhile receives
        // this fragment's nested function scopes, which `push_scope` numbers
        // from `pristine.len()` upward.
        let placeholder =
            FuncScope::new(usize::MAX, u32::MAX, u32::MAX, u32::MAX, Vec::new(), None, false);
        let mut root = std::mem::replace(&mut self.pristine[ROOT], placeholder);

        analyzer.analyze_hoist(
            &program.body,
            &mut root,
            &mut self.block_scopes,
            &mut self.next_slot,
        );
        analyzer.analyze_stmts(
            &program.body,
            &mut root,
            &mut self.block_scopes,
            &mut self.next_slot,
            &mut self.pristine,
        );

        root.own_local_count = self.next_slot;
        root.id = ROOT;
        self.pristine[ROOT] = root;
        // `push_scope` fixes a child's `parent` when the *parent* is pushed,
        // and the root never is — so its children are adopted here instead.
        let children = self.pristine[ROOT].children.clone();
        for child in children {
            self.pristine[child].parent = ROOT;
        }

        self.next_label = analyzer.next_label;
        let mut diagnostics = analyzer.diagnostics;
        diagnostics.extend(self.redeclarations(&before));

        // Re-resolve from the pristine copy. Cloning is what keeps this
        // idempotent: the passes below rewrite capture state in place and
        // could not be run a second time over their own output.
        let mut scopes = self.pristine.clone();
        let const_fns = resolve_const_functions(&mut scopes, Some(ROOT));
        let (
            binding_slot,
            ref_resolution,
            binding_immutable,
            binding_captured,
            const_refs,
            scope_by_span,
        ) = finalize_tables(&scopes, &const_fns);

        FragmentAnalysis {
            program: ProgramAnalysis {
                scopes,
                root: ROOT,
                binding_slot,
                ref_resolution,
                binding_immutable,
                binding_captured,
                const_refs,
                const_fn_scopes: const_fns,
                scope_by_span,
            },
            next_label: self.next_label,
            diagnostics,
            first_new_slot,
        }
    }

    /// Top-level names this fragment declared a *second* time.
    ///
    /// Each fragment is parsed on its own, so `oxc` — which catches
    /// `let x; let x;` within one parse as a syntax error — cannot see a
    /// collision that spans fragments. The walk quietly allocates a fresh slot
    /// and shadows instead, which would silently strand the old binding, so the
    /// check belongs here: a name whose outermost-scope entry now points
    /// somewhere new was declared twice.
    fn redeclarations(&self, before: &IndexMap<String, NameRes>) -> Vec<Diagnostic> {
        let after = top_level_bindings(&self.block_scopes);
        let mut out = Vec::new();
        for (name, old) in before {
            let Some(new) = after.get(name) else {
                continue;
            };
            if same_binding(old, new) {
                continue;
            }
            // Report at the offending declaration: the binding occurrence
            // that took the new slot, when there is one.
            let span = match new {
                NameRes::Slot { slot, .. } => self.pristine[ROOT]
                    .binding_spans
                    .iter()
                    .find(|(_, own, _)| own == slot)
                    .map(|(span, _, _)| *span)
                    .unwrap_or(0),
                NameRes::Const(_) => 0,
            };
            out.push(Diagnostic {
                span: Span::point(span),
                // **Say which scope, because that is the surprise.**
                // This check only ever fires across fragments — a
                // collision inside one parse is caught as a syntax
                // error before it gets here — so the name being
                // redeclared is always one that something *earlier*
                // bound, in a scope the new code did not open and
                // cannot see the top of. A bare "already declared"
                // sends a reader looking for the other declaration in
                // the fragment in front of them, where it is not.
                //
                // Live, across 96 kept runs of the agent that embeds
                // this crate, six programs died here — on `f`, `r`,
                // `refs`, `files`, `names` and `t`, all of them the
                // second `const` of a name a previous fragment had
                // already taken.
                //
                // **And it is not a naming collision.** Looked at
                // again on 2026-09-20 with the failing line beside the
                // binding it clashed with: four of four were the same
                // statement written twice —
                // `const f = await tools.read_file("helpers.py");`
                // above and below. The caller is not running out of
                // names, it is redoing work, and the binding it wants
                // is still sitting there. So the remedy the message
                // leads with is *use it*; renaming would keep the
                // second read and hide the waste, which is also why
                // this diagnostic is worth having rather than quietly
                // shadowing.
                message: format!(
                    "`{name}` is already declared — this code shares one scope with what \
                     ran before it, so `{name}` is still bound to whatever the earlier code \
                     gave it. If that is the value you want, use it; it does not need \
                     fetching twice. If you want a different one, give it a different name."
                ),
                kind: crate::diag::DiagKind::Semantic,
            });
        }
        out
    }
}

/// The outermost block scope's bindings, by name. Only the outermost: a `let`
/// inside a nested block legitimately shadows a top-level name, and is not a
/// redeclaration.
fn top_level_bindings(block_scopes: &BlockScopes) -> IndexMap<String, NameRes> {
    block_scopes
        .first()
        .map(|top| {
            top.iter()
                .map(|(name, res)| (name.clone(), res.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether two resolutions of one name are the same binding.
///
/// Every `let`/`const` gets a unique slot — never reused, even when shadowing —
/// so a changed slot is a redeclaration. A literal `const` occupies no slot at
/// all, so it is compared by value instead; redeclaring one to the *identical*
/// literal therefore goes unreported, which is the one case where nothing
/// observable changes anyway.
fn same_binding(a: &NameRes, b: &NameRes) -> bool {
    match (a, b) {
        (NameRes::Slot { slot: x, .. }, NameRes::Slot { slot: y, .. }) => x == y,
        (NameRes::Const(x), NameRes::Const(y)) => x == y,
        _ => false,
    }
}
