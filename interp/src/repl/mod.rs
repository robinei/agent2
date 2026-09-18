//! Incremental evaluation: one compilation, paused between fragments.
//!
//! [`Repl`] holds an `Analyzer`, its growing `ProgramAnalysis`, a `Compiler`
//! and a `VM` open across successive fragments of source. Each fragment is
//! analyzed into the accumulated tables, compiled into instructions appended
//! after the ones already there, and run — in a frame that is never unwound
//! between fragments, so every binding stays live and the next fragment simply
//! continues.
//!
//! `compile(src)` plus a fresh VM is the degenerate case: one fragment, then
//! done. Nothing about the one-shot path changes.
//!
//! **This is a REPL, not a notebook.** Nothing here knows about markdown,
//! fences, cells or replies; a fragment is a `&str`. An interactive shell, an
//! eval loop, or a debugger evaluating an expression against a live frame would
//! each want exactly this. The notebook transport is one caller.
//!
//! ## How a fragment becomes instructions
//!
//! A one-shot compile does six things a fragment must do differently.
//!
//! 1. **Spans are absolute.** The caller hands in a buffer as long as the whole
//!    unit with only this fragment's text live and everything else blanked, so
//!    `oxc`'s spans are already offsets into the unit. That is what lets the
//!    span-keyed analysis tables accumulate instead of colliding at zero.
//! 2. **The optimizer is skipped.** `optimizer::finalize` is `optimize` plus
//!    `backpatch`, and `optimize` deletes and reorders instructions to a
//!    fixpoint — which over an accumulated stream would move already-executed
//!    code out from under a live `ip` and invalidate every `Fn`/`Closure`
//!    address. Only `backpatch` runs, scoped to the appended range.
//! 3. **Labels persist.** Backpatch strips `Label` markers, so a function
//!    declared in one fragment and called from a later one would resolve
//!    against a marker that is already gone. [`Repl::label_addr`] keeps every
//!    resolved address for the life of the unit.
//! 4. **The prelude grows.** Helpers are appended past the buffer and recorded,
//!    so a second fragment using `.map()` after the first already did neither
//!    redeclares `__map` nor collides spans with it.
//! 5. **The root frame is pinned.** Every root declaration gets a real slot and
//!    a real store, and no root function is a const-fn. Both optimizations are
//!    sound only for a whole program: a later fragment can add the first read
//!    of a binding whose store was elided, or assign to a function whose slot
//!    was reclaimed, and by then neither is repairable.
//! 6. **`source` is the real text.** Diagnostics render against the unit's
//!    actual source, not the blanked parse buffer — a closure from fragment 0
//!    that throws during fragment 2 must not underline spaces.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::analyzer::incremental::{IncrementalAnalyzer, ROOT};
use crate::compiler::Compiler;
use crate::compiler::fragment::FragmentPrologue;
use crate::diag::Diagnostic;
use crate::span::Span;
use crate::vm::{CodeAddr, Instr, SlotKind, VM, VMError};

/// A live incremental evaluation.
pub struct Repl {
    analyzer: IncrementalAnalyzer,
    compiler: Compiler,
    /// The VM, with the shared root frame standing from the first fragment to
    /// the last. Public so a driver can `step` it, inspect it, settle promises
    /// and resume conditions exactly as it would any other VM.
    pub vm: VM,
    /// Label id -> resolved code address, for the life of the unit.
    /// `u32::MAX` means "not yet defined".
    label_addr: Vec<CodeAddr>,
    /// Canonical closure per const-fn address, persisted so a `PushFn` in a
    /// later fragment resolves to the same closure as an earlier one.
    const_fn_closures: HashMap<CodeAddr, u32>,
    /// The root's slot kinds as of the last fragment, to diff against.
    prev_slot_kinds: Vec<SlotKind>,
    /// Prelude helpers already compiled into the unit.
    emitted_helpers: HashSet<&'static str>,
    /// The unit's real source text: each fragment's live bytes unblanked into
    /// place, with the prelude appended past them.
    source: String,
    /// Where the next prelude chunk goes — past everything written so far, so
    /// helper spans never collide across fragments.
    prelude_base: usize,
    /// How many fragments have been compiled. Only the first builds the
    /// frame; the rest extend it.
    fragments: usize,
    /// Set once [`close`](Self::close) has emitted the run's `Return(0)`.
    closed: bool,
}

impl Repl {
    /// A new unit with the host-seeded consts in place and no code.
    pub fn new(
        input: serde_json::Value,
        attachments: serde_json::Value,
    ) -> Result<Self, VMError> {
        Ok(Self {
            analyzer: IncrementalAnalyzer::new(),
            compiler: Compiler::new_incremental(),
            vm: VM::for_incremental(input, attachments)?,
            label_addr: Vec::new(),
            const_fn_closures: HashMap::new(),
            prev_slot_kinds: Vec::new(),
            emitted_helpers: HashSet::new(),
            source: String::new(),
            prelude_base: 0,
            fragments: 0,
            closed: false,
        })
    }

    /// Feed one fragment, appending its instructions after the ones already
    /// compiled. The VM is left standing at the first of them.
    ///
    /// `buffer` is as long as the whole unit with only this fragment's text
    /// live — see the module docs. Nothing is executed here; the caller steps
    /// the VM.
    pub fn push(&mut self, buffer: &str) -> Result<(), Vec<Diagnostic>> {
        self.compile_and_append(buffer, false)
    }

    /// End the unit: emit the ordinary root `Return(0)`, so the next `step`
    /// unwinds the frame and reports `Done` on exactly the path a one-shot
    /// program takes.
    ///
    /// `buffer` may be a final fragment or an empty unit-length buffer when
    /// there is nothing left to compile. A run is one run however many
    /// fragments it took, so this is the only terminator it ever has.
    pub fn close(&mut self, buffer: &str) -> Result<(), Vec<Diagnostic>> {
        self.compile_and_append(buffer, true)
    }

    /// The unit's source so far, as the diagnostics render against it.
    pub fn source(&self) -> &str {
        &self.source
    }

    fn compile_and_append(
        &mut self,
        buffer: &str,
        close: bool,
    ) -> Result<(), Vec<Diagnostic>> {
        assert!(!self.closed, "the unit is already closed");

        // ── the parse text: the caller's buffer, then any new prelude ──
        //
        // The prelude region grows monotonically past everything written so
        // far, so helpers appended for a later fragment never land on spans an
        // earlier one already used.
        let prelude = crate::prelude::assemble_new(buffer, &mut self.emitted_helpers);
        self.prelude_base = self.prelude_base.max(buffer.len());
        let mut full = String::with_capacity(self.prelude_base + prelude.len() + 1);
        full.push_str(buffer);
        // Pad to the prelude base with blanks that keep the line structure, the
        // same rule the caller's buffer follows.
        while full.len() < self.prelude_base {
            full.push(' ');
        }
        let prelude_at = full.len();
        full.push_str(&prelude);
        if !prelude.is_empty() {
            self.prelude_base = full.len();
        }

        self.remember_source(&full, prelude_at);

        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, &full, SourceType::mjs())
            .with_options(oxc_parser::ParseOptions {
                allow_return_outside_function: true,
                ..Default::default()
            })
            .parse();

        for err in &ret.errors {
            let span = err
                .labels
                .as_ref()
                .and_then(|labels| labels.first())
                .map(|l| {
                    let start = l.offset() as u32;
                    Span::new(start, start + l.len() as u32)
                })
                .unwrap_or_default();
            self.compiler.parse_error(span, err.message.to_string());
        }

        // ── analysis, accumulated and re-resolved ──
        let fragment = self.analyzer.feed(&ret.program);
        self.compiler.push_diagnostics(fragment.diagnostics);

        let root = &fragment.program.scopes[ROOT];
        // A top-level arrow that references `this` makes capture resolution
        // append a synthetic `<this>` slot past every declared local — at an
        // index the *next* fragment's first declaration would also claim, and
        // at a different index again on the fragment after that, because the
        // slot is re-derived each time rather than allocated once. Top-level
        // `this` is `undefined` in this dialect, so capturing it buys nothing;
        // refusing it is cheaper than making the slot stable.
        if root.this_slot.is_some() {
            self.compiler.push_diagnostics([Diagnostic {
                span: Span::point(ret.program.span.start),
                message: "`this` cannot be captured at the top level of an \
                          incremental evaluation"
                    .to_string(),
                kind: crate::diag::DiagKind::Semantic,
            }]);
        }

        let slot_kinds: Vec<SlotKind> = root.slot_kinds.to_vec();
        let uses_arguments = root.uses_arguments;
        let prologue = FragmentPrologue {
            new_kinds: slot_kinds
                .get(fragment.first_new_slot as usize..)
                .unwrap_or(&[])
                .to_vec(),
            promotions: self.promotions(&slot_kinds, fragment.first_new_slot),
            first_fragment: self.fragments == 0,
            uses_arguments,
            spill_slot: slot_kinds.len() as u32,
        };

        self.compiler
            .begin_fragment(fragment.program, fragment.next_label);
        self.compiler
            .compile_fragment(&ret.program, &prologue, close);

        let diagnostics = self.compiler.take_diagnostics();
        if !diagnostics.is_empty() {
            // Nothing is appended, so the VM is untouched and the fragments
            // that already ran still stand: a fragment that does not compile
            // costs itself and nothing before it.
            let (_, _) = self.compiler.take_fragment();
            return Err(diagnostics);
        }

        // ── backpatch the appended range, and append it ──
        let (code, spans) = self.compiler.take_fragment();
        let base = self.vm.code.len() as CodeAddr;
        let (code, spans) = self.backpatch(code, spans, base);

        let from = self.vm.code.len();
        self.vm.code.extend(code);
        self.vm.spans.extend(spans);
        self.vm
            .install_const_fn_closures(from, &mut self.const_fn_closures);
        self.vm.source = Arc::from(self.source.as_str());

        self.fragments += 1;
        self.prev_slot_kinds = slot_kinds;
        self.closed = close;
        Ok(())
    }

    /// Pre-existing root slots that flipped `Plain -> Boxed`, as a diff against
    /// the previous fragment's kinds.
    ///
    /// **A diff, not a rule.** Capture resolution recomputes `slot_kinds` over
    /// the accumulated scopes, so comparing the result against the last one
    /// yields exactly the slots that flipped — there is no "is this the first
    /// capture of an earlier name" logic to write.
    ///
    /// Only slots below `first_new_slot` are considered: a slot this fragment
    /// declared was allocated with its final kind by the prologue and needs no
    /// promotion.
    ///
    /// **Why it is sound**, now that the root frame is pinned: a flip can only
    /// be caused by a new closure in the fragment being compiled, because a
    /// reference from any earlier closure would itself have been a capture and
    /// the slot would have been `Boxed` from the start. So every instruction
    /// compiled against the `Plain` representation is straight-line code in a
    /// fragment that has already run, and — this is the part pinning buys — the
    /// slot really does hold the value, because the store was never elided.
    /// `FreshCell` moves that value into a cell; it could not have conjured one.
    fn promotions(&self, now: &[SlotKind], first_new_slot: u32) -> Vec<u32> {
        let ceiling = (first_new_slot as usize).min(self.prev_slot_kinds.len());
        (0..ceiling)
            .filter(|&i| {
                self.prev_slot_kinds[i] == SlotKind::Plain && now[i] == SlotKind::Boxed
            })
            .map(|i| i as u32)
            .collect()
    }

    /// Resolve this fragment's labels against `base` and rewrite its label-id
    /// operands, recording every address in [`Self::label_addr`].
    ///
    /// Scoped to the fragment by construction: the frozen prefix is already
    /// label-free with resolved addresses, and a pass that could not tell a
    /// resolved address from a label id would corrupt it.
    fn backpatch(
        &mut self,
        code: Vec<Instr>,
        spans: Vec<Span>,
        base: CodeAddr,
    ) -> (Vec<Instr>, Vec<Span>) {
        if self.label_addr.len() < self.compiler.label_ceiling() as usize {
            self.label_addr
                .resize(self.compiler.label_ceiling() as usize, CodeAddr::MAX);
        }
        // First scan: a label's address is `base` plus the count of non-Label
        // instructions before it in this fragment.
        let mut offset = base;
        for instr in &code {
            match instr {
                Instr::Label(id) => self.label_addr[*id as usize] = offset,
                _ => offset += 1,
            }
        }

        // Second scan: drop the labels and rewrite the addresses. A label
        // defined by an *earlier* fragment resolves from the same table — which
        // is how a function declared in one fragment is called from the next.
        let at = |table: &[CodeAddr], l: u32| -> CodeAddr {
            let a = table[l as usize];
            debug_assert_ne!(a, CodeAddr::MAX, "label {l} was referenced but never defined");
            a
        };
        let mut out_code = Vec::with_capacity(code.len());
        let mut out_spans = Vec::with_capacity(spans.len());
        for (instr, span) in code.into_iter().zip(spans) {
            let rewritten = match instr {
                Instr::Label(_) => continue,
                Instr::Jump(l) => Instr::Jump(at(&self.label_addr, l)),
                Instr::JFalse(l) => Instr::JFalse(at(&self.label_addr, l)),
                Instr::JTrue(l) => Instr::JTrue(at(&self.label_addr, l)),
                Instr::JNotNullish(l) => Instr::JNotNullish(at(&self.label_addr, l)),
                Instr::TryEnter(l) => Instr::TryEnter(at(&self.label_addr, l)),
                Instr::Call(l, n) => Instr::Call(at(&self.label_addr, l), n),
                Instr::ClosureNew(l, arity, caps) => {
                    Instr::ClosureNew(at(&self.label_addr, l), arity, caps)
                }
                Instr::PushFn(l, ptr, arity) => {
                    Instr::PushFn(at(&self.label_addr, l), ptr, arity)
                }
                other => other,
            };
            out_code.push(rewritten);
            out_spans.push(span);
        }
        (out_code, out_spans)
    }

    /// Unblank this fragment's live bytes into the unit's real source.
    ///
    /// The parse buffer is blanked outside the live fragment, but a diagnostic
    /// rendered later — a closure from fragment 0 throwing during fragment 2 —
    /// must underline the text that was really there. Every non-blank byte of
    /// each buffer is therefore copied into place as it arrives, leaving a
    /// source in which every span that was ever compiled reads correctly.
    fn remember_source(&mut self, full: &str, prelude_at: usize) {
        if self.source.len() < full.len() {
            let pad = full.len() - self.source.len();
            self.source.extend(std::iter::repeat_n(' ', pad));
        }
        // SAFETY-free: both are ASCII-blank outside the live regions, and the
        // live regions are whole UTF-8 substrings on line boundaries, so the
        // result stays valid UTF-8. Rebuilt rather than mutated in place to
        // keep that guarantee checkable by the compiler.
        let mut merged: Vec<u8> = self.source.clone().into_bytes();
        for (i, b) in full.bytes().enumerate() {
            // The prelude is written verbatim; the fragment contributes only
            // its live bytes, everything else being the blank fill.
            if b != b' ' || i >= prelude_at {
                merged[i] = b;
            }
        }
        self.source = String::from_utf8(merged)
            .expect("the unit's source stays UTF-8: fills are ASCII and fragments are aligned");
    }
}

#[cfg(test)]
mod tests;
