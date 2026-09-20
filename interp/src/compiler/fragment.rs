//! Codegen for one fragment of an incremental unit.
//!
//! [`super::Compiler::compile_fragment`] is `compile_program` with the two
//! ends changed and the middle identical:
//!
//! - **The prologue grows a live frame instead of building one.** The first
//!   fragment emits `EnterFrame` as any program does; every later one emits
//!   `ExtendFrame` carrying only its own new slots, plus a `FreshCell` for each
//!   pre-existing slot a closure in *this* fragment just captured.
//! - **The epilogue is `Pause`, not `Return(0)`.** A root `Return(0)` ends the
//!   run, unwinding the frame and taking every top-level binding with it —
//!   exactly wrong between two fragments that share a scope. The run's real
//!   `Return(0)` is emitted once, by the closing fragment.
//!
//! Everything between is `compile_program`'s own path: hoist the fragment's
//! function declarations, then compile its statements.

use oxc_ast::ast;

use crate::span::Span;
use crate::vm::{Instr, LocalIndex, SlotKind};

/// What a fragment's prologue must set up, computed by the caller from the
/// re-resolved analysis.
pub(crate) struct FragmentPrologue {
    /// Slot kinds for the root locals this fragment declares, in declaration
    /// order. Empty when the fragment declares nothing, in which case no
    /// prologue instruction is emitted at all rather than an empty one.
    pub(crate) new_kinds: Vec<SlotKind>,
    /// Pre-existing root slots that flipped `Plain -> Boxed` because a closure
    /// in this fragment captured them. Each gets a `FreshCell`, which reads the
    /// slot's current value, allocates a cell seeded with it and stores the
    /// `Upval` back — a value-preserving promotion.
    ///
    /// Disjoint from `new_kinds`: a *new* slot captured in its own fragment is
    /// allocated `Boxed` by the prologue directly, so order does not matter.
    pub(crate) promotions: Vec<u32>,
    /// Whether this is the unit's **first** fragment. `EnterFrame` builds a
    /// frame and belongs to the first fragment alone; every later one extends
    /// the frame that is already standing.
    ///
    /// So a first fragment declaring nothing emits no prologue at all, and the
    /// next fragment's `ExtendFrame` simply extends from zero — the frame is
    /// quiescent with `local_count` 0, which is a perfectly good thing to push
    /// onto. Deciding this by fragment index rather than by "has an
    /// `EnterFrame` been emitted yet" keeps `EnterFrame`'s arg-region
    /// normalization — which truncates the stack to `fp + nparams` — off every
    /// instruction stream where a live frame could already hold locals.
    pub(crate) first_fragment: bool,
    /// Whether the root frame needs an eager `arguments` array. Only
    /// `EnterFrame` carries the flag, so it is honoured on the first fragment;
    /// afterwards `Instr::Arguments` builds the root's array lazily from an
    /// `arg_count` of zero, which is the same empty array.
    pub(crate) uses_arguments: bool,
    /// The frame slot a `return` crossing a `finally` would spill through:
    /// one past every root local this fragment leaves allocated.
    pub(crate) spill_slot: u32,
}

impl super::Compiler {
    /// A compiler that will be fed fragments rather than a whole program: the
    /// root frame is **pinned** (see `Compiler::pin_root`), and `code`/`spans`
    /// are emptied between fragments by [`take_fragment`](Self::take_fragment)
    /// so each one is backpatched and appended on its own.
    pub(crate) fn new_incremental() -> Self {
        let mut c = Self::new();
        c.pin_root = true;
        c
    }

    /// Make a top-level `return` a compile error carrying `message`.
    pub(crate) fn set_no_top_level_return(&mut self, message: Option<String>) {
        self.no_top_level_return = message;
    }

    /// Install the fragment's re-resolved analysis and continue the shared
    /// label numbering.
    pub(crate) fn begin_fragment(
        &mut self,
        analysis: crate::analyzer::ProgramAnalysis,
        next_label: u32,
    ) {
        self.analysis = Some(analysis);
        self.next_label = next_label;
    }

    /// Take this fragment's label-form code and spans, leaving the compiler
    /// empty for the next one. Everything else — the label counter, the
    /// interned strings, the constant environment — persists, which is what
    /// makes it one compilation rather than a series of them.
    pub(crate) fn take_fragment(&mut self) -> (Vec<Instr>, Vec<Span>) {
        (
            std::mem::take(&mut self.code),
            std::mem::take(&mut self.spans),
        )
    }

    pub(crate) fn take_diagnostics(&mut self) -> Vec<crate::diag::Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    pub(crate) fn push_diagnostics(
        &mut self,
        diags: impl IntoIterator<Item = crate::diag::Diagnostic>,
    ) {
        self.diagnostics.extend(diags);
    }

    pub(crate) fn label_ceiling(&self) -> u32 {
        self.next_label
    }

    /// Compile one fragment into `self.code`/`self.spans`, which the caller has
    /// emptied: what lands there is this fragment alone, in label form, ready
    /// to be backpatched against a base address and appended.
    ///
    /// `close` emits the run's `Return(0)` terminator instead of `Pause`. That
    /// is the ordinary program epilogue — a reply is one run, however many
    /// fragments it took — so the frame unwinds and `step` reports `Done` on
    /// exactly the path a one-shot program takes.
    pub(crate) fn compile_fragment(
        &mut self,
        program: &ast::Program,
        prologue: &FragmentPrologue,
        close: bool,
    ) {
        let analysis = self
            .analysis
            .as_ref()
            .expect("analysis must run before codegen");
        self.current_scope = analysis.root;

        // Synthetic prologue instructions: no source expression of their own,
        // so a point span at the fragment's start rather than an invented
        // range (the convention `span.rs` names).
        let prologue_span = Span::point(program.span.start);
        let prologue_at = self.code.len();
        let emitted_prologue =
            !prologue.new_kinds.is_empty() || (prologue.first_fragment && prologue.uses_arguments);

        if emitted_prologue {
            let kinds = prologue.new_kinds.clone();
            let instr = if prologue.first_fragment {
                // The first fragment builds the frame the ordinary way. The
                // root has no params and no upvals, so this only allocates
                // locals.
                Instr::EnterFrame(0, prologue.uses_arguments, kinds.into())
            } else {
                Instr::ExtendFrame(kinds.into())
            };
            self.emit(instr, prologue_span);
        }

        self.return_spill = Some(super::ReturnSpill {
            slot: prologue.spill_slot,
            enter_frame: if emitted_prologue {
                Ok(prologue_at)
            } else {
                Err(prologue_at)
            },
            prologue: if prologue.first_fragment {
                super::PrologueKind::Enter
            } else {
                super::PrologueKind::Extend
            },
            used: false,
        });

        // Promotions come after the frame has grown, so a `FreshCell` can
        // never name a slot that does not exist yet.
        for &slot in &prologue.promotions {
            self.emit(Instr::FreshCell(slot as LocalIndex), prologue_span);
        }

        // Hoist this fragment's function declarations, then compile its
        // statements — `compile_program`'s own path, unchanged.
        self.hoist_function_decls(&program.body);
        for stmt in &program.body {
            self.compile_stmt(stmt);
        }

        let end_span = Span::point(program.span.end);
        if close {
            self.emit(Instr::Return(0), end_span);
        } else {
            self.emit(Instr::Pause, end_span);
        }
        if let Some(spill) = self.return_spill.take() {
            self.finalize_return_spill(spill, prologue_span);
        }
    }
}
