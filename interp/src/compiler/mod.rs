//! Compiler — JS source → VM instructions (`vm::Instr`).
//!
//! Codegen (Pass 2): parses with `oxc_parser`, then lowers the AST to VM
//! instructions, resolving every binding/reference/function by span through the
//! [`crate::analyzer`] tables (Pass 1) — this pass keeps no scope state of its
//! own. Diagnostics live in [`crate::diag`]. See `COMPILER_PLAN.md` for the
//! full design.

use std::sync::Arc;

use std::collections::{HashMap, HashSet};

use oxc_allocator::Allocator;
use oxc_ast::ast;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

use crate::analyzer::{self, ConstValue, ProgramAnalysis, RefSlot, frame_abs};
use crate::builtin::Builtin;
use crate::diag::Diagnostic;
use crate::vm::RcStr;
use crate::vm::{Instr, LocalIndex, SetMode, SlotKind, Value};

/// A compiled program: the flat instruction stream, a parallel span table
/// (`spans[ip]` = source byte offset of the instruction at `ip`), the
/// source it was compiled from (for rendering runtime diagnostics), and
/// the debug table (function names, source ranges, slot names — 9_TUI).
#[derive(Debug)]
pub struct Program {
    pub code: Vec<Instr>,
    pub spans: Vec<u32>,
    pub source: Arc<str>,
    pub debug: crate::debuginfo::DebugTable,
}

/// Compile JS source into a `Program`. Collects every diagnostic (oxc syntax
/// errors plus our own semantic errors) and returns them all if any exist,
/// rather than producing a partial program.
pub fn compile(source: &str) -> Result<Program, Vec<Diagnostic>> {
    let allocator = Allocator::default();
    // Module mode, so top-level `await` parses (the primary pattern: the
    // program is the main task — 7_ASYNC). Top-level `return` (8_HARNESS
    // Step 0) is preserved via `allow_return_outside_function`. Other
    // module-vs-script differences (e.g. `with`) are already rejected
    // explicitly by the compiler.
    let source_type = SourceType::mjs();

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
    let ret = Parser::new(&allocator, &full_source, source_type)
        .with_options(oxc_parser::ParseOptions {
            allow_return_outside_function: true,
            ..Default::default()
        })
        .parse();

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

    // Optimize the label-form code and resolve labels to offsets. See
    // `optimizer.rs` for the passes (CFG simplification, peephole, const-fold,
    // branch inversion — iterated to a fixpoint) and backpatch.
    let (code, spans) =
        crate::optimizer::finalize(compiler.code, compiler.spans, compiler.next_label);

    // Debug table (9_TUI Step 1): one entry per analyzer scope, indexed by
    // scope id. Built purely from the analysis tables — instruction →
    // function attribution at runtime goes through spans, so nothing here
    // depends on (or constrains) the optimizer's code motion.
    let debug = {
        let analysis = compiler.analysis.as_ref().expect("analysis present");
        let functions = analysis
            .scopes
            .iter()
            .enumerate()
            .map(|(id, s)| {
                let is_root = id == analysis.root;
                let (span_start, span_end) = if is_root {
                    (0, full_source.len() as u32)
                } else {
                    s.node_range()
                };
                crate::debuginfo::FnDebug {
                    name: if is_root {
                        "<root>".to_string()
                    } else {
                        s.debug_name()
                    },
                    span_start,
                    span_end,
                    slot_names: s.debug_slot_names(!analysis.const_fn_scopes.contains(&id)),
                }
            })
            .collect();
        crate::debuginfo::DebugTable {
            functions,
            root: analysis.root,
        }
    };

    Ok(Program {
        code,
        spans,
        // The full source (user code + any appended prelude) so runtime
        // diagnostics render against the same offsets the spans were taken from.
        source: Arc::from(full_source.as_str()),
        debug,
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
    /// `barriers.len()` when this context was pushed. A `break`/`continue`
    /// targeting this context unwinds every barrier above this mark: one
    /// `TryExit` per `try` entry (keeping the VM's handler stack balanced),
    /// a `Pop` per crossed stack residue, and a detour through the exit
    /// stub of each crossed `finally` (see [`Compiler::emit_exit`]).
    floor: usize,
}

/// The ultimate destination of an early exit that may cross `try` blocks
/// and `finally` boundaries. Doubles as the identity of a `finally` exit
/// stub: all exits with the same destination share one stub per crossed
/// finalizer (6_LANGUAGE Part B2).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExitKind {
    /// `break`/`continue`: jump to `target`, unwinding down to the loop
    /// context's barrier `floor`.
    Jump { target: u32, floor: usize },
    /// `return`: unwind every barrier; the value waits in the function's
    /// [`ReturnSpill`] slot, loaded by the final `Return(1)`. Residues are
    /// never popped on this path — frame teardown discards the whole
    /// operand stack, and overwriting the shared spill slot is exactly how
    /// a `return` from a finally overrides a pending one.
    Return,
}

/// Per-function bookkeeping for the return spill slot (see
/// [`Compiler::return_spill`]). The slot sits just past every
/// analyzer-allocated slot (`[params | upvals | own locals | self?]`) and
/// is only materialized — by patching the already-emitted `EnterFrame`
/// post-body — when some `return` actually crossed a finalizer; functions
/// without one compile byte-for-byte as before.
struct ReturnSpill {
    /// Absolute frame slot index.
    slot: u32,
    /// `Ok(i)`: `code[i]` is this frame's `EnterFrame`, patch its kinds.
    /// `Err(i)`: no `EnterFrame` was emitted (a root frame with no locals);
    /// insert one at `i` if the slot is used (safe pre-backpatch: labels
    /// are positional markers, jumps carry label ids).
    enter_frame: Result<usize, usize>,
    used: bool,
}

/// One entry of the compile-time barrier stack: everything an early exit
/// (`break`/`continue`, and in Part B2 Step 2 `return`) must unwind on its
/// way out, in nesting order (innermost last).
enum Barrier {
    /// One runtime `TryEnter` handler entry — the exit emits a balancing
    /// `TryExit`. If it is a `finally` wrapper, the exit then jumps to this
    /// entry's stub for its destination (requested here during body
    /// compilation, emitted by `compile_try` after the unwind copy), which
    /// runs the finally block and continues the exit from there.
    Try {
        has_finalizer: bool,
        stubs: Vec<(ExitKind, u32)>,
    },
    /// `slots` operand-stack slots that sit beneath the code compiled while
    /// this barrier is open and are owned by an enclosing construct: a
    /// `switch` discriminant, or the pending thrown value beneath a
    /// `finally` unwind copy. A `break`/`continue` jumping past this
    /// barrier pops them (its target label expects them gone — and popping
    /// a pending exception is exactly JS's "finally's jump overrides the
    /// pending completion"). A `return` never pops residues: frame teardown
    /// discards the whole operand stack, and popping under live inner
    /// handlers would desynchronize their stack snapshots.
    Residue { slots: usize },
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
    /// Exit barriers open at the current emission point within the current
    /// function body (innermost last; saved/reset per function): `try`
    /// entries and stack residues. Drives everything `break`/`continue`/
    /// `return` must emit when jumping out — balancing `TryExit`s, residue
    /// `Pop`s, and `finally` exit-stub detours (see [`Barrier`]).
    barriers: Vec<Barrier>,
    /// The current function's return spill slot (saved/reset per function;
    /// `None` only during analysis). A `return` crossing a `finally`
    /// boundary parks its value here while the finally copies run — the
    /// operand stack cannot carry it: pending residues beneath it could
    /// not be discarded without popping under live handler snapshots, and
    /// there is no pop-under instruction (nor is one wanted).
    return_spill: Option<ReturnSpill>,
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
    /// Interned string literals: identical contents share one `RcStr`
    /// allocation, which each `PushStr` then clones (a refcount bump). Stored as
    /// a set keyed by the string itself (via `RcStr: Borrow<str>`).
    interned: HashSet<RcStr>,
    /// Constant-propagation environment for the current function frame: a slot
    /// holding a `const` bound to a compile-time constant maps to the literal
    /// push that reproduces it, so references emit the literal instead of a
    /// `Local` load. Sound with no dataflow because a `const` is write-once
    /// (reassignment is rejected) and every `let`/`const` gets a unique slot
    /// (no reuse, even when shadowing), so an entry never goes stale. Saved and
    /// reset per function body in `emit_function_def` (slot numbers are
    /// frame-relative, so a callee's slots must not see the caller's constants).
    const_env: HashMap<u32, Instr>,
}

impl<'src> Compiler<'src> {
    fn new(source: &'src str) -> Self {
        Compiler {
            source,
            code: Vec::new(),
            spans: Vec::new(),
            next_label: 0,
            loops: Vec::new(),
            barriers: Vec::new(),
            return_spill: None,
            diagnostics: Vec::new(),
            analysis: None,
            current_scope: 0,
            interned: HashSet::new(),
            const_env: HashMap::new(),
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

    /// Intern a string literal, returning a shared `RcStr`. Deduplicated by
    /// content — identical literals across the program share one allocation, so
    /// the embedded `PushStr` operands (and the values they push at runtime) are
    /// all clones of the same block.
    fn intern_string(&mut self, s: &str) -> RcStr {
        if let Some(existing) = self.interned.get(s) {
            return existing.clone();
        }
        let rc = RcStr::from(s);
        self.interned.insert(rc.clone());
        rc
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
        let enter_frame_at = self.code.len();
        let root_slot_count = root.slot_kinds.len() as u32;
        let emitted_enter_frame = !root.slot_kinds.is_empty() || root.uses_arguments;
        if emitted_enter_frame {
            let kinds = root.slot_kinds.clone();
            let uses_arguments = root.uses_arguments;
            self.emit(
                Instr::EnterFrame(0, uses_arguments, kinds.into()),
                program.span.start,
            );
        }
        self.return_spill = Some(ReturnSpill {
            slot: root_slot_count,
            enter_frame: if emitted_enter_frame {
                Ok(enter_frame_at)
            } else {
                Err(enter_frame_at)
            },
            used: false,
        });

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

    fn compile_stmt(&mut self, stmt: &ast::Statement) {
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

            // Out of scope — informative errors.
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

    /// Destructure the source value already on top of the stack into a binding
    /// pattern, **consuming** that value. Used by declarations; every leaf is a
    /// binding identifier whose slot comes from analysis (`binding_slot`).
    fn destructure_binding(&mut self, pat: &ast::BindingPattern, span: u32) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                match self.binding_slot(id.span.start) {
                    Some(slot) => self.emit(Instr::SetLocal(slot as LocalIndex), span),
                    None => {
                        // No slot (an earlier error, e.g. shadowing `input`).
                        self.emit(Instr::Pop(1), span);
                    }
                }
            }
            ast::BindingPattern::AssignmentPattern(ap) => {
                self.emit_default(&ap.right, span);
                self.destructure_binding(&ap.left, span);
            }
            ast::BindingPattern::ArrayPattern(arr) => {
                for (i, el) in arr.elements.iter().enumerate() {
                    if let Some(el) = el {
                        self.emit(Instr::Pick(0), span);
                        self.emit(Instr::PushPosInt(i as u64), span);
                        self.emit(Instr::IndexGet, span);
                        self.destructure_binding(el, span);
                    }
                }
                if let Some(rest) = &arr.rest {
                    // `arr.slice(elements.len())` gives remaining elements.
                    self.emit(Instr::Pick(0), span);
                    self.emit(Instr::PushPosInt(arr.elements.len() as u64), span);
                    self.emit(Instr::CallBuiltin(Builtin::StrSlice, 2), span);
                    self.destructure_binding(&rest.argument, span);
                }
                self.emit(Instr::Pop(1), span); // drop the source
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                if let Some(rest) = &obj.rest {
                    // Rest: shallow-copy the source up front, then delete each
                    // matched key from the copy as it is extracted — so every
                    // key (computed ones included) evaluates exactly once.
                    self.emit(Instr::ObjNew(Vec::new().into()), span);
                    self.emit(Instr::Pick(1), span);
                    self.emit(Instr::ObjExtend, span); // [src, rest]
                    for prop in &obj.properties {
                        self.emit(Instr::Pick(1), span); // [src, rest, src]
                        self.emit_property_key_string(&prop.key, prop.computed, span);
                        self.emit_rest_excluded_key_access(span); // [src, rest, val]
                        self.destructure_binding(&prop.value, span);
                    }
                    self.destructure_binding(&rest.argument, span); // [src]
                } else {
                    for prop in &obj.properties {
                        self.emit(Instr::Pick(0), span);
                        self.emit_property_key_access(&prop.key, prop.computed, span);
                        self.destructure_binding(&prop.value, span);
                    }
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
        self.emit(Instr::ObjGet(RcStr::from(name.as_str())), span);
    }

    /// Push a pattern property's key as a string *value* (used to exclude
    /// matched keys from an object-rest copy). A static key pushes the literal;
    /// a computed key evaluates the expression and coerces with `ToStr`.
    fn emit_property_key_string(&mut self, key: &ast::PropertyKey, computed: bool, span: u32) {
        if computed {
            if let Some(expr) = key.as_expression() {
                self.compile_expr(expr);
                self.emit(Instr::ToStr, span);
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
                    self.emit(Instr::ToStr, span);
                    return;
                }
                self.error(key.span().start, "unsupported destructuring key");
                return;
            }
        };
        self.emit(Instr::PushStr(RcStr::from(name.as_str())), span);
    }

    /// Object-rest plumbing: with `[src, rest, src, key]` on the stack (key a
    /// string), delete `key` from the `rest` copy and read `src[key]`, leaving
    /// `[src, rest, value]`. The key is consumed by both uses via one `Pick`,
    /// so its expression never re-evaluates.
    fn emit_rest_excluded_key_access(&mut self, span: u32) {
        self.emit(Instr::Pick(0), span); //   [src, rest, src, key, key]
        self.emit(Instr::Pick(3), span); //   [src, rest, src, key, key, rest]
        self.emit(Instr::Dig(1), span); //    [src, rest, src, key, rest, key]
        self.emit(Instr::ObjDelete, span); // [src, rest, src, key, existed]
        self.emit(Instr::Pop(1), span); //    [src, rest, src, key]
        self.emit(Instr::IndexGet, span); //  [src, rest, value]
    }

    /// Apply a destructuring/parameter default to the value on top of the stack:
    /// if it is `undefined`, replace it with the default expression's value;
    /// otherwise leave it. (JS applies defaults only for `undefined`, not
    /// `null`.) Leaves exactly one value either way.
    fn emit_default(&mut self, default: &ast::Expression, span: u32) {
        let have = self.new_label();
        self.emit(Instr::Pick(0), span);
        self.emit(Instr::PushUndefined, span);
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

    /// Whether the binding declared at `span` is immutable in fact (a `const`,
    /// or an un-reassigned, un-captured `let`/`var`) — so its constant
    /// initializer may be recorded for propagation. Defaults to `false`.
    fn binding_immutable(&self, span: u32) -> bool {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .binding_immutable
            .get(&span)
            .copied()
            .unwrap_or(false)
    }

    /// Whether the binding declared at `span` is captured by a nested function.
    /// A non-captured binding's slot is read only by its own frame, so once every
    /// read is propagated its store is dead and can be dropped.
    fn binding_captured(&self, span: u32) -> bool {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .binding_captured
            .get(&span)
            .copied()
            .unwrap_or(false)
    }

    /// The compile-time constant an identifier reference at `span` resolves to
    /// (a `const x = <literal>` binding — eliminated, no slot), if any.
    fn const_ref(&self, span: u32) -> Option<ConstValue> {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .const_refs
            .get(&span)
            .cloned()
    }

    /// Whether the function scope `scope_id` is a constant function (Phase F):
    /// non-capturing and non-reassigned, so its binding store is dead.
    fn is_const_fn_scope(&self, scope_id: usize) -> bool {
        self.analysis
            .as_ref()
            .expect("analysis present")
            .const_fn_scopes
            .contains(&scope_id)
    }

    /// The push instruction that materializes a [`ConstValue`]. Numbers go
    /// through `f64_to_value` so the result matches the original literal exactly.
    fn const_value_push(&mut self, v: &ConstValue) -> Instr {
        match v {
            ConstValue::Null => Instr::PushNull,
            ConstValue::Undefined => Instr::PushUndefined,
            ConstValue::Bool(b) => Instr::PushBool(*b),
            ConstValue::Str(s) => Instr::PushStr(self.intern_string(s)),
            ConstValue::Num(n) => match f64_to_value(*n) {
                Value::PosInt(u) => Instr::PushPosInt(u),
                Value::NegInt(i) => Instr::PushNegInt(i),
                Value::Float(f) => Instr::PushFloat(f),
                _ => unreachable!("f64_to_value yields an int or float"),
            },
            // A constant function (Phase F): its value is its code address.
            ConstValue::Fn { label, .. } => Instr::PushFn(*label),
        }
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

    /// Emit the read of a slot-resolved identifier reference.
    ///
    /// Constant propagation: an immutable binding (a `const`, or an
    /// effectively-const `let`/`var`) bound to a compile-time constant is
    /// read as the literal directly (which then composes with const-folding),
    /// rather than a `Local` load. This matters for correctness, not just
    /// speed: when every read is propagated, the binding's initializer store
    /// is dead-eliminated, so the slot is never written — a `Local` load
    /// would read an uninitialized (undefined) slot. Every consumer of a
    /// `RefSlot` read must go through here.
    fn emit_slot_read(&mut self, r: &RefSlot, span: u32) {
        if r.immutable {
            if let Some(push) = self.const_env.get(&r.slot) {
                self.emit(push.clone(), span);
                return;
            }
        }
        self.emit(Instr::Local(r.slot as LocalIndex), span);
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
            self.emit(Instr::FreshCell(slot as LocalIndex), span);
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
            floor: self.barriers.len(),
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
        self.loops.push(LoopCtx {
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

    fn compile_break(&mut self, s: &ast::BreakStatement) {
        if s.label.is_some() {
            self.error(s.span.start, "labeled `break` is not supported");
            return;
        }
        match self.loops.last() {
            Some(ctx) => {
                let kind = ExitKind::Jump {
                    target: ctx.break_label,
                    floor: ctx.floor,
                };
                self.emit_exit(kind, s.span.start);
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
        let target = self
            .loops
            .iter()
            .rev()
            .find_map(|ctx| ctx.continue_label.map(|l| (l, ctx.floor)));
        match target {
            Some((target, floor)) => {
                self.emit_exit(ExitKind::Jump { target, floor }, s.span.start);
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
    fn emit_exit(&mut self, kind: ExitKind, span: u32) {
        let floor = match kind {
            ExitKind::Jump { floor, .. } => floor,
            ExitKind::Return => 0,
        };
        let mut depth = self.barriers.len();
        while depth > floor {
            depth -= 1;
            match self.barriers[depth] {
                Barrier::Residue { slots } => {
                    // `return` never pops residues: frame teardown discards
                    // the whole operand stack (see [`ExitKind::Return`]).
                    if slots > 0 && !matches!(kind, ExitKind::Return) {
                        self.emit(Instr::Pop(slots), span);
                    }
                }
                Barrier::Try { has_finalizer, .. } => {
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
            ExitKind::Jump { target, .. } => self.emit(Instr::Jump(target), span),
            ExitKind::Return => {
                let slot = self.return_spill.as_ref().expect("spill set up").slot;
                self.emit(Instr::Local(slot as LocalIndex), span);
                self.emit(Instr::Return(1), span);
            }
        }
    }

    /// The exit-stub label for destination `kind` on the finalizer entry at
    /// `barrier_idx`, allocating it on first request. All exits with the
    /// same destination through the same finalizer share one stub.
    fn stub_label(&mut self, barrier_idx: usize, kind: ExitKind) -> u32 {
        let Barrier::Try { stubs, .. } = &self.barriers[barrier_idx] else {
            unreachable!("stub_label on a non-try barrier");
        };
        if let Some(&(_, label)) = stubs.iter().find(|&&(k, _)| k == kind) {
            return label;
        }
        let label = self.new_label();
        let Barrier::Try { stubs, .. } = &mut self.barriers[barrier_idx] else {
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
    fn emit_return(&mut self, span: u32) {
        let crosses_finalizer = self.barriers.iter().any(|b| {
            matches!(
                b,
                Barrier::Try {
                    has_finalizer: true,
                    ..
                }
            )
        });
        if !crosses_finalizer {
            let try_exits = self
                .barriers
                .iter()
                .filter(|b| matches!(b, Barrier::Try { .. }))
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
        self.emit_exit(ExitKind::Return, span);
    }

    /// Post-body half of the spill-slot protocol (see [`ReturnSpill`]):
    /// materialize the slot by patching (or, for a root frame that skipped
    /// it, inserting) the `EnterFrame`, only if some `return` used it.
    fn finalize_return_spill(&mut self, spill: ReturnSpill, span: u32) {
        if !spill.used {
            return;
        }
        match spill.enter_frame {
            Ok(i) => {
                let Instr::EnterFrame(nparams, build_args, kinds) = &self.code[i] else {
                    unreachable!("ReturnSpill::enter_frame must point at an EnterFrame");
                };
                let mut kinds: Vec<SlotKind> = kinds.iter().copied().collect();
                kinds.push(SlotKind::Plain);
                self.code[i] = Instr::EnterFrame(*nparams, *build_args, kinds.into());
            }
            Err(i) => {
                self.code
                    .insert(i, Instr::EnterFrame(0, false, vec![SlotKind::Plain].into()));
                self.spans.insert(i, span);
            }
        }
    }

    /// `try { … } catch (e) { … }` (6_LANGUAGE Part B). Lowers to a handler
    /// window:
    ///
    /// ```text
    /// TryEnter(catch)
    ///   …try body…
    /// TryExit
    /// Jump(end)
    /// Label(catch)      ← unwinder lands here with the thrown value pushed
    ///   <bind or Pop the catch binding>
    ///   …catch body…
    /// Label(end)
    /// ```
    ///
    /// The unwinder pops the handler entry *before* jumping, so a throw
    /// inside `catch` propagates outward. `break`/`continue`/`return`
    /// leaving the try block emit their own `TryExit`s (tracked via
    /// `barriers`). `raise()` is not catchable by design.
    ///
    /// A `finally` clause wraps the whole thing in an *outer* handler —
    /// `try B catch C finally F` ≡ `try { try B catch C } finally F` — so an
    /// exception thrown from `C` still runs `F`. `F` is compiled once per
    /// way of leaving the protected region (the Part B2 codegen
    /// duplication): the normal path, the unwind path (above the pending
    /// thrown value, ending in a rethrow `Throw`), and one *exit stub* per
    /// distinct `break`/`continue` destination that crossed this finalizer
    /// (requested via [`Compiler::emit_exit`] during body compilation):
    ///
    /// ```text
    /// TryEnter(fin)
    ///   <try/catch as above, or the bare try body>
    /// TryExit
    /// F                 ← normal-path copy
    /// Jump(end)
    /// Label(fin)        ← unwinder lands here, thrown value pushed
    /// F                 ← unwind-path copy (runs above the thrown value)
    /// Throw             ← rethrow
    /// Label(stub_k)     ← exit sites jump here after their TryExits
    /// F                 ← exit-path copy
    /// <continue the exit: Pop residues / TryExit / outer stub / Jump>
    /// …one stub per destination…
    /// Label(end)
    /// ```
    ///
    /// Each copy statically knows what completion is pending, which is what
    /// makes JS's completion-value override semantics straight-line code: a
    /// jump out of a copy simply never reaches the copy's trailing
    /// epilogue (rethrow / onward transfer) — and on the unwind path it
    /// pops the pending thrown value when it crosses the copy's residue
    /// barrier, swallowing the exception exactly as JS does. A `throw`
    /// inside `F` needs nothing at all: the unwinder truncates to the outer
    /// handler's snapshot, which predates the pending value.
    ///
    /// Stub requests accrue only during body compilation: exits inside the
    /// `F` copies route to *outer* entries (this one is already popped), so
    /// draining the requests after the unwind copy sees the complete set.
    fn compile_try(&mut self, s: &ast::TryStatement) {
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
                self.barriers.push(Barrier::Try {
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
                let Some(Barrier::Try { stubs, .. }) = self.barriers.pop() else {
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
    fn compile_finally_copy(&mut self, fin: &ast::BlockStatement, pending_slots: usize) {
        self.barriers.push(Barrier::Residue {
            slots: pending_slots,
        });
        for stmt in &fin.body {
            self.compile_stmt(stmt);
        }
        self.barriers.pop();
    }

    /// The `try { … } catch (e) { … }` core (no finalizer at this level).
    fn compile_try_catch(
        &mut self,
        block: &ast::BlockStatement,
        handler: &ast::CatchClause,
        span: u32,
    ) {
        let catch_label = self.new_label();
        let end = self.new_label();
        self.emit(Instr::TryEnter(catch_label), span);
        self.barriers.push(Barrier::Try {
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
    fn compile_for_of(&mut self, s: &ast::ForOfStatement) {
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
    fn compile_for_in(&mut self, s: &ast::ForInStatement) {
        let span = s.span.start;
        let Some(pat) = self.for_loop_binding_pattern(&s.left, span) else {
            return;
        };
        self.compile_expr(&s.right);
        self.emit(Instr::CallBuiltin(Builtin::ObjKeys, 1), span);
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
    fn compile_index_loop(&mut self, pat: &ast::BindingPattern, body: &ast::Statement, span: u32) {
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
        self.loops.push(LoopCtx {
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
    fn for_loop_binding_pattern<'b>(
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

    /// Emit `FreshCell` for every captured binding in a loop-head pattern, so
    /// in-loop closures capture per-iteration copies (the single-identifier
    /// form does the same inline in `compile_index_loop`).
    fn emit_pattern_fresh_cells(&mut self, pat: &ast::BindingPattern, span: u32) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                if let Some(slot) = self.binding_slot(id.span.start) {
                    if self.slot_needs_fresh(slot) {
                        self.emit(Instr::FreshCell(slot as LocalIndex), span);
                    }
                }
            }
            ast::BindingPattern::AssignmentPattern(ap) => {
                self.emit_pattern_fresh_cells(&ap.left, span);
            }
            ast::BindingPattern::ArrayPattern(arr) => {
                for el in arr.elements.iter().flatten() {
                    self.emit_pattern_fresh_cells(el, span);
                }
                if let Some(rest) = &arr.rest {
                    self.emit_pattern_fresh_cells(&rest.argument, span);
                }
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.emit_pattern_fresh_cells(&prop.value, span);
                }
                if let Some(rest) = &obj.rest {
                    self.emit_pattern_fresh_cells(&rest.argument, span);
                }
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
        // The discriminant stays on the stack across the bodies: a residue
        // barrier makes a `continue` (which targets the enclosing loop, past
        // this switch) pop it. The switch's own `break` lands at `end`, where
        // the discriminant is expected (and popped below) — its loop context
        // sits above the residue, so `break` never crosses it.
        self.barriers.push(Barrier::Residue { slots: 1 });
        self.loops.push(LoopCtx {
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

    /// Every expression leaves exactly one value on the stack (the
    /// stack-discipline invariant). Unsupported nodes record a diagnostic and
    /// emit nothing — the diagnostics abort the compile before a `Program` is
    /// produced, so the missing value never matters.
    fn compile_expr(&mut self, expr: &ast::Expression) {
        match expr {
            // ── literals ──────────────────────────────────────────────
            ast::Expression::NumericLiteral(lit) => match number_literal_to_value(lit.value) {
                Value::PosInt(v) => self.emit(Instr::PushPosInt(v), lit.span.start),
                Value::Float(v) => self.emit(Instr::PushFloat(v), lit.span.start),
                _ => unreachable!(),
            },
            ast::Expression::StringLiteral(lit) => {
                let s = self.intern_string(lit.value.as_str());
                self.emit(Instr::PushStr(s), lit.span.start);
            }
            ast::Expression::BooleanLiteral(lit) => {
                self.emit(Instr::PushBool(lit.value), lit.span.start);
            }
            ast::Expression::NullLiteral(lit) => {
                self.emit(Instr::PushNull, lit.span.start);
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

            // ── Phase 7: await ────────────────────────────────────────
            // `await x` → evaluate x, `Await`. A non-promise value passes
            // through unchanged; a pending promise yields to the host with
            // ip parked on the Await (it re-executes after resolution).
            // The parser confines `await` to async bodies and the top level
            // (module mode), which Tier 2's suspension story relies on.
            ast::Expression::AwaitExpression(a) => {
                self.compile_expr(&a.argument);
                self.emit(Instr::Await, a.span.start);
            }

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
                let span = r.span.start;
                let pattern = self.intern_string(r.regex.pattern.text.as_str());
                let flags_str = regexp_flags_to_str(r.regex.flags);
                let flags = self.intern_string(&flags_str);
                self.emit(Instr::PushStr(pattern), span);
                self.emit(Instr::PushStr(flags), span);
                self.emit(Instr::RegExpNew, span);
            }
            ast::Expression::ThisExpression(t) => {
                self.error(t.span.start, "`this` is not supported")
            }
            ast::Expression::NewExpression(n) => {
                if let ast::Expression::Identifier(id) = &n.callee {
                    if is_error_ctor(id.name.as_str()) {
                        return self.compile_error_ctor(id.name.as_str(), n);
                    }
                    if id.name == "RegExp" {
                        return self.compile_regexp_ctor(n);
                    }
                    if id.name == "Map" {
                        return self.compile_map_ctor(n);
                    }
                    if id.name == "Set" {
                        return self.compile_set_ctor(n);
                    }
                }
                // Targeted message for the misuse LLMs actually type: there is
                // no executor pattern (7_ASYNC commitment 4) — every promise
                // comes from a tool call or (Tier 2) an async function call,
                // so all promises provably settle.
                if matches!(&n.callee, ast::Expression::Identifier(id) if id.name == "Promise") {
                    self.error(
                        n.span.start,
                        "`new Promise` is not supported: promises come only from `tools.*` calls \
                         and async functions (there is no executor pattern)",
                    )
                } else {
                    self.error(n.span.start, "`new` is not supported")
                }
            }
            other => self.error(other.span().start, "unsupported expression"),
        }
    }

    /// `new Error(msg)` / `new TypeError(msg)` / …: build the `{ name,
    /// message }` error object. The message coerces with ToString at
    /// construction (`new Error(123)` → `"123"`, as in JS); absent → `""`.
    fn compile_error_ctor(&mut self, name: &str, n: &ast::NewExpression) {
        let span = n.span.start;
        if n.arguments.len() > 1 {
            // JS's `{ cause }` options bag is out of scope; stay strict.
            self.error(
                span,
                format!("`new {name}` takes at most one (message) argument"),
            );
            return;
        }
        let name_str = self.intern_string(name);
        self.emit(Instr::PushStr(name_str), span);
        match n.arguments.first() {
            None => {
                let empty = self.intern_string("");
                self.emit(Instr::PushStr(empty), span);
            }
            Some(arg) => match arg.as_expression() {
                Some(msg) => {
                    self.compile_expr(msg);
                    self.emit(Instr::ToStr, span);
                }
                None => {
                    self.error(
                        span,
                        format!("spread arguments are not supported in `new {name}`"),
                    );
                    return;
                }
            },
        }
        self.emit(
            Instr::ObjNew(vec![RcStr::from("name"), RcStr::from("message")].into()),
            span,
        );
    }

    /// `new RegExp(pattern[, flags])` — compile pattern and flags, emit
    /// `RegExpNew`. Pattern coerces to string; flags default to `""`.
    fn compile_regexp_ctor(&mut self, n: &ast::NewExpression) {
        let span = n.span.start;
        if n.arguments.len() > 2 {
            self.error(span, "`new RegExp` takes at most two arguments");
            return;
        }
        // First argument: pattern (required, coerced to string).
        match n.arguments.first() {
            None => {
                let empty = self.intern_string("");
                self.emit(Instr::PushStr(empty), span);
            }
            Some(arg) => match arg.as_expression() {
                Some(expr) => {
                    self.compile_expr(expr);
                    self.emit(Instr::ToStr, span);
                }
                None => {
                    self.error(span, "spread arguments are not supported in `new RegExp`");
                    return;
                }
            },
        }
        // Second argument: flags (optional, coerced to string, default "").
        if n.arguments.len() >= 2 {
            match n.arguments[1].as_expression() {
                Some(expr) => {
                    self.compile_expr(expr);
                    self.emit(Instr::ToStr, span);
                }
                None => {
                    self.error(span, "spread arguments are not supported in `new RegExp`");
                    return;
                }
            }
        } else {
            let empty = self.intern_string("");
            self.emit(Instr::PushStr(empty), span);
        }
        self.emit(Instr::RegExpNew, span);
    }

    /// `new Map([entries])`: if an argument is given it must be an array of
    /// [key, value] pairs. No argument → empty Map.
    fn compile_map_ctor(&mut self, n: &ast::NewExpression) {
        let span = n.span.start;
        if n.arguments.len() > 1 {
            self.error(span, "`new Map` takes at most one (iterable) argument");
            return;
        }
        if let Some(arg) = n.arguments.first() {
            match arg.as_expression() {
                Some(expr) => {
                    self.compile_expr(expr);
                }
                None => {
                    self.error(span, "spread arguments are not supported in `new Map`");
                    return;
                }
            }
        } else {
            self.emit(Instr::PushUndefined, span);
        }
        self.emit(Instr::MapNew, span);
    }

    /// `new Set([iterable])`: if an argument is given it must be an array of
    /// values. No argument → empty Set.
    fn compile_set_ctor(&mut self, n: &ast::NewExpression) {
        let span = n.span.start;
        if n.arguments.len() > 1 {
            self.error(span, "`new Set` takes at most one (iterable) argument");
            return;
        }
        if let Some(arg) = n.arguments.first() {
            match arg.as_expression() {
                Some(expr) => {
                    self.compile_expr(expr);
                }
                None => {
                    self.error(span, "spread arguments are not supported in `new Set`");
                    return;
                }
            }
        } else {
            self.emit(Instr::PushUndefined, span);
        }
        self.emit(Instr::SetNew, span);
    }

    /// A bare identifier resolves only to the host-seeded `input` object or the
    /// global literal-like names. Everything else is an undeclared variable —
    /// a compile error. (Local variables arrive in Phase 2/3; namespace names
    /// like `Math`/`Object` are recognized structurally as call/member
    /// receivers, never as bare values.)
    fn compile_identifier(&mut self, name: &str, span: u32) {
        // A local/param/captured variable resolves to its frame slot (resolved
        // by analysis, keyed by this reference's span); `Local` dereferences a
        // boxed slot transparently.
        // An eliminated `const x = <literal>` binding (Phase E): no slot — the
        // reference is the literal itself (resolved intra- or cross-function by
        // analysis). Composes with const-folding like any other push.
        if let Some(value) = self.const_ref(span) {
            let push = self.const_value_push(&value);
            self.emit(push, span);
            return;
        }
        if let Some(r) = self.ref_slot(span) {
            self.emit_slot_read(&r, span);
            return;
        }
        // `arguments` (when not shadowed by a real binding above) is the current
        // frame's argument array — built and cached per frame by the VM.
        if name == "arguments" {
            self.emit(Instr::Arguments, span);
            return;
        }
        match name {
            "input" => self.emit(Instr::PushObject(0), span),
            "undefined" => self.emit(Instr::PushUndefined, span),
            "NaN" => self.emit(Instr::PushFloat(f64::NAN), span),
            "Infinity" => self.emit(Instr::PushFloat(f64::INFINITY), span),
            _ => {
                self.error(span, format!("undeclared variable `{name}`"));
            }
        }
    }

    // ── operators ──────────────────────────────────────────────────────

    fn compile_binary(&mut self, bin: &ast::BinaryExpression) {
        use ast::BinaryOperator as Op;
        let span = bin.span.start;

        // `key in obj` lowers to ObjHas, which pops the (string) key then the
        // object. Evaluate left (key) then right (obj) to keep JS eval order,
        // then Dig(1) (formerly Swap) into [obj, key]; ToStr coerces the key as JS `in` does.
        if bin.operator == Op::In {
            self.compile_expr(&bin.left);
            self.emit(Instr::ToStr, span);
            self.compile_expr(&bin.right);
            self.emit(Instr::Dig(1), span);
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
                    match f64_to_value(-lit.value) {
                        Value::PosInt(v) => self.emit(Instr::PushPosInt(v), span),
                        Value::NegInt(v) => self.emit(Instr::PushNegInt(v), span),
                        Value::Float(v) => self.emit(Instr::PushFloat(v), span),
                        _ => unreachable!(),
                    }
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
                self.emit(Instr::PushUndefined, span);
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
                    Instr::PushStr(m.property.name.as_str().into()),
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
                    Instr::PushStr(m.property.name.as_str().into()),
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
                self.emit(Instr::Pick(0), span);
                self.emit(Instr::JFalse(end), span);
                self.emit(Instr::Pop(1), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Label(end), span);
            }
            Op::Or => {
                // truthy: keep lhs; falsy: drop lhs, eval rhs.
                let end = self.new_label();
                self.emit(Instr::Pick(0), span);
                self.emit(Instr::JTrue(end), span);
                self.emit(Instr::Pop(1), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Label(end), span);
            }
            Op::Coalesce => {
                // not nullish: keep lhs (the taken jump leaves it); nullish:
                // the fall-through pops the lhs, then evaluate rhs.
                let end = self.new_label();
                self.emit(Instr::JNotNullish(end), span);
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
        let span = arr.span.start;
        // Fast path: no spread elements (byte-for-byte unchanged from before)
        let has_spread = arr
            .elements
            .iter()
            .any(|el| matches!(el, ast::ArrayExpressionElement::SpreadElement(_)));
        if !has_spread {
            let mut n = 0u32;
            for el in &arr.elements {
                match el.as_expression() {
                    Some(e) => {
                        self.compile_expr(e);
                        n += 1;
                    }
                    None => {
                        self.error(el.span().start, "array holes are not supported");
                        return;
                    }
                }
            }
            self.emit(Instr::ArrNew(n), span);
            return;
        }

        // Slow path: incremental building with spread elements.
        // Start with ArrNew for the leading static segment (possibly empty).
        let mut leading = 0u32;
        for el in &arr.elements {
            match el {
                ast::ArrayExpressionElement::SpreadElement(_) => break,
                _ => {
                    if let Some(e) = el.as_expression() {
                        self.compile_expr(e);
                        leading += 1;
                    } else {
                        self.error(el.span().start, "array holes are not supported");
                        return;
                    }
                }
            }
        }
        self.emit(Instr::ArrNew(leading), span);

        // Remaining elements: alternate spreads and single-element pushes.
        for el in &arr.elements[leading as usize..] {
            match el {
                ast::ArrayExpressionElement::SpreadElement(s) => {
                    self.compile_expr(&s.argument);
                    self.emit(Instr::ArrExtend, span);
                }
                _ => {
                    if let Some(e) = el.as_expression() {
                        self.compile_expr(e);
                        self.emit(Instr::ArrPush, span);
                    } else {
                        self.error(el.span().start, "array holes are not supported");
                        return;
                    }
                }
            }
        }
    }

    /// Validate a static (non-spread) object literal property and extract its
    /// field name. Reports a compile error and returns `None` for getters/
    /// setters, methods, and unsupported key forms (computed keys return
    /// `None` without error — the caller falls through to the IndexSet path).
    fn static_property_name(&mut self, p: &ast::ObjectProperty) -> Option<RcStr> {
        if p.kind != ast::PropertyKind::Init {
            self.error(p.span.start, "getters/setters are not supported");
            return None;
        }
        // Method shorthand `{ run(x) { … } }` is not rejected — the value is a
        // FunctionExpression compiled via the normal function-expression path.
        // `this` inside the method body still errors with its existing message.
        if p.computed {
            return None;
        }
        match &p.key {
            ast::PropertyKey::StaticIdentifier(id) => Some(RcStr::from(id.name.as_str())),
            ast::PropertyKey::StringLiteral(s) => Some(RcStr::from(s.value.as_str())),
            ast::PropertyKey::NumericLiteral(num) => {
                Some(RcStr::from(number_key_to_string(num.value).as_str()))
            }
            _ => {
                self.error(p.key.span().start, "unsupported object key");
                None
            }
        }
    }

    fn compile_object(&mut self, obj: &ast::ObjectExpression) {
        let span = obj.span.start;
        // Fast path: no spread, no computed keys (byte-for-byte unchanged)
        let needs_slow = obj.properties.iter().any(|prop| {
            matches!(prop, ast::ObjectPropertyKind::SpreadProperty(_))
                || matches!(prop, ast::ObjectPropertyKind::ObjectProperty(p) if p.computed)
        });
        if !needs_slow {
            let mut names: Vec<RcStr> = Vec::with_capacity(obj.properties.len());
            for prop in &obj.properties {
                let p = match prop {
                    ast::ObjectPropertyKind::ObjectProperty(p) => p,
                    ast::ObjectPropertyKind::SpreadProperty(_) => unreachable!(),
                };
                let Some(name) = self.static_property_name(p) else {
                    return;
                };
                self.compile_expr(&p.value);
                names.push(name);
            }
            self.emit(Instr::ObjNew(names.into()), span);
            return;
        }

        // Slow path: incremental building with spreads and/or computed keys.
        // Phase 1: emit ObjNew for the leading static segment (stop at first
        // spread or computed key).
        let leading_count = obj
            .properties
            .iter()
            .take_while(|prop| match prop {
                ast::ObjectPropertyKind::SpreadProperty(_) => false,
                ast::ObjectPropertyKind::ObjectProperty(p) => !p.computed,
            })
            .count();

        let mut leading_names: Vec<RcStr> = Vec::with_capacity(leading_count);
        for prop in &obj.properties[..leading_count] {
            let p = match prop {
                ast::ObjectPropertyKind::ObjectProperty(p) => p,
                ast::ObjectPropertyKind::SpreadProperty(_) => unreachable!(),
            };
            let Some(name) = self.static_property_name(p) else {
                return;
            };
            self.compile_expr(&p.value);
            leading_names.push(name);
        }
        self.emit(Instr::ObjNew(leading_names.into()), span);

        // Phase 2: remaining properties — spreads, computed keys, and static
        // fields after a spread/computed key.  For a static field we use
        // Pick(0) + ObjSet + Pop(1) to keep the object on the stack; for a
        // computed key we use Pick(0) + compile key + compile value +
        // IndexSet(New) + Pop(1) (IndexSet pops container, key, val).
        for prop in &obj.properties[leading_count..] {
            match prop {
                ast::ObjectPropertyKind::SpreadProperty(s) => {
                    self.compile_expr(&s.argument);
                    self.emit(Instr::ObjExtend, span);
                }
                ast::ObjectPropertyKind::ObjectProperty(p) => {
                    if p.computed {
                        self.emit(Instr::Pick(0), span);
                        self.compile_expr(
                            p.key
                                .as_expression()
                                .expect("computed key must have expression"),
                        );
                        self.compile_expr(&p.value);
                        self.emit(Instr::IndexSet(SetMode::New), span);
                        self.emit(Instr::Pop(1), span);
                    } else {
                        let Some(name) = self.static_property_name(p) else {
                            return;
                        };
                        self.emit(Instr::Pick(0), span);
                        self.compile_expr(&p.value);
                        self.emit(Instr::ObjSet(name, SetMode::New), span);
                        self.emit(Instr::Pop(1), span);
                    }
                }
            }
        }
    }

    fn compile_template(&mut self, tl: &ast::TemplateLiteral) {
        let span = tl.span.start;
        // result = quasi0 + expr0 + quasi1 + expr1 + … . The accumulator starts
        // as a string (interned constant) and stays one, so every `Add` takes
        // the concat path and ToString-coerces each interpolated value, as JS does.
        let quasi_str = |q: &ast::TemplateElement| {
            q.value
                .cooked
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or_else(|| q.value.raw.as_str())
                .to_string()
        };
        let q0 = self.intern_string(quasi_str(&tl.quasis[0]).as_str());
        self.emit(Instr::PushStr(q0), span);
        for (i, expr) in tl.expressions.iter().enumerate() {
            self.compile_expr(expr);
            self.emit(Instr::Add, span);
            let qn = self.intern_string(quasi_str(&tl.quasis[i + 1]).as_str());
            self.emit(Instr::PushStr(qn), span);
            self.emit(Instr::Add, span);
        }
    }

    // ── member access ────────────────────────────────────────────────────

    /// `obj.foo` (and `state.foo`, since `state` lowers to `Ptr(0)`). `.length`
    /// is the static intrinsic `ArrLength` (the accepted divergence: an object
    /// property literally named `length` reached via `.length`); anything else
    /// is `ObjGet`.
    fn compile_static_member(&mut self, m: &ast::StaticMemberExpression) {
        // Namespace constants and first-class builtin refs: handle before
        // evaluating the object.
        if let ast::Expression::Identifier(obj) = &m.object {
            // Constants: fold to compile-time values.
            if let Some(val) = namespace_constant(obj.name.as_str(), m.property.name.as_str()) {
                let span = m.span.start;
                match val {
                    ConstVal::Float(f) => self.emit(Instr::PushFloat(f), span),
                    ConstVal::PosInt(n) => self.emit(Instr::PushPosInt(n), span),
                }
                return;
            }
            // First-class reference to a namespaced builtin used as a *value* (e.g.
            // `Math.sqrt` passed as a callback): push the `Builtin`.
            if let Some(builtin) =
                Builtin::for_namespace(obj.name.as_str(), m.property.name.as_str())
            {
                self.emit(Instr::PushBuiltin(builtin), m.span.start);
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
        if name == "length" || name == "size" {
            self.emit(Instr::ArrLength, span);
        } else {
            self.emit(Instr::ObjGet(name.into()), span);
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
    /// `JNotNullish` keeps the value on the not-nullish (taken) path with no
    /// `Pick(0)` (formerly `Dup`) and pops it on the nullish fall-through, so
    /// the whole guard is one branch plus the short-circuit tail.
    ///
    /// Per-link: a fully-`?.` chain (`a?.b?.c`) short-circuits correctly because
    /// each link re-checks; mixing `?.` then a plain `.` on a nullish base
    /// (`a?.b.c`) is an accepted divergence (runtime TypeError, not `undefined`).
    fn begin_optional(&mut self, span: u32) -> u32 {
        let cont = self.new_label();
        let end = self.new_label();
        self.emit(Instr::JNotNullish(cont), span);
        self.emit(Instr::PushUndefined, span);
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
                    self.emit(Instr::Pick(0), span); // one copy is the expression result
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
            // `JNotNullish` pops `old` on the nullish fall-through, so the
            // store path starts with a clean stack and needs no explicit Pop.
            Op::LogicalNullish => self.emit(Instr::JNotNullish(keep), span),
            Op::LogicalAnd => {
                // For `&&=`, need a copy of `old` to test truthiness without
                // consuming it (the keep path needs it). In void context we
                // can just peek (JFalse pops, but we'd lose old). We always
                // Pick(0) (formerly Dup) since the keep path or store path consumes `old`.
                self.emit(Instr::Pick(0), span);
                self.emit(Instr::JFalse(keep), span); // falsy → keep old
                self.emit(Instr::Pop(1), span); // store path: discard old
            }
            Op::LogicalOr => {
                self.emit(Instr::Pick(0), span);
                self.emit(Instr::JTrue(keep), span); // truthy → keep old
                self.emit(Instr::Pop(1), span); // store path: discard old
            }
            _ => unreachable!("only logical operators reach here"),
        }
        // Store path: evaluate the RHS, store it.
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
            ast::UpdateOperator::Increment => -1.0,
            ast::UpdateOperator::Decrement => 1.0,
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
                self.emit(Instr::IncLocal(*slot as u16, p, mode), span);
            } else {
                // Void: `IncLocal` (in-place update) then drop its result. The
                // update mode is irrelevant since the pushed value is popped.
                self.emit(
                    Instr::IncLocal(*slot as u16, p, crate::vm::UpdateMode::Postfix),
                    span,
                );
                self.emit(Instr::Pop(1), span);
            }
            return;
        }

        // Non-local targets (member/index): load-sub-store path.
        self.lvalue_emit_addr(&lv, span);
        self.lvalue_emit_load(&lv, span);
        self.emit(Instr::PushFloat(p), span);
        self.emit(Instr::Sub, span);
        if value_needed {
            let mode = if u.prefix { SetMode::New } else { SetMode::Old };
            match &lv {
                LValue::Member(_, field) => {
                    self.emit(Instr::ObjSet(RcStr::from(field.as_str()), mode), span);
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
        // An eliminated `const` (Phase E) has no slot; a write to it is still a
        // constant reassignment error.
        if self.const_ref(span).is_some() {
            self.error(span, format!("assignment to constant `{name}`"));
            return None;
        }
        match self.ref_slot(span) {
            Some(r) => {
                if r.is_const {
                    self.error(span, format!("assignment to constant `{name}`"));
                }
                Some(LValue::Local(r.slot))
            }
            None if name == "input" => {
                self.error(span, "cannot reassign `input` (it is a host-seeded const)");
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
            LValue::Local(slot) => self.emit(Instr::Local(*slot as LocalIndex), span),
            LValue::Member(_, field) => {
                self.emit(Instr::Pick(0), span); // copy the object
                self.emit(Instr::ObjGet(RcStr::from(field.as_str())), span);
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
    /// `TeeLocal` (the one-instruction equivalent of `Pick(0); SetLocal`, formerly `Dup; SetLocal`).
    fn lvalue_emit_store(&mut self, lv: &LValue<'_, '_>, span: u32) {
        match lv {
            LValue::Local(slot) => {
                self.emit(Instr::TeeLocal(*slot as LocalIndex), span);
            }
            LValue::Member(_, field) => self.emit(
                Instr::ObjSet(RcStr::from(field.as_str()), SetMode::New),
                span,
            ),
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
                self.emit(Instr::SetLocal(*slot as LocalIndex), span);
            }
            LValue::Member(_, field) => {
                self.emit(
                    Instr::ObjSet(RcStr::from(field.as_str()), SetMode::New),
                    span,
                );
                self.emit(Instr::Pop(1), span);
            }
            LValue::Index(..) => {
                self.emit(Instr::IndexSet(SetMode::New), span);
                self.emit(Instr::Pop(1), span);
            }
        }
    }

    /// Remove `n` values sitting directly below the top of the stack, leaving the
    /// top in place. Uses `Nip(n)` (one instruction) rather than `Dig(1)`+`Pop` (formerly `Swap`+`Pop`)
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
                for (i, el) in arr.elements.iter().enumerate() {
                    if let Some(el) = el {
                        self.emit(Instr::Pick(0), span);
                        self.emit(Instr::PushPosInt(i as u64), span);
                        self.emit(Instr::IndexGet, span);
                        self.assign_maybe_default(el, span);
                    }
                }
                if let Some(rest) = &arr.rest {
                    self.emit(Instr::Pick(0), span);
                    self.emit(Instr::PushPosInt(arr.elements.len() as u64), span);
                    self.emit(Instr::CallBuiltin(Builtin::StrSlice, 2), span);
                    self.assign_target_leaf(&rest.target, span);
                }
                self.emit(Instr::Pop(1), span);
            }
            ast::AssignmentTarget::ObjectAssignmentTarget(obj) => {
                if let Some(rest) = &obj.rest {
                    // Same copy-minus-keys lowering as the declaration form
                    // (see `destructure_binding`).
                    self.emit(Instr::ObjNew(Vec::new().into()), span);
                    self.emit(Instr::Pick(1), span);
                    self.emit(Instr::ObjExtend, span); // [src, rest]
                    for prop in &obj.properties {
                        self.emit(Instr::Pick(1), span); // [src, rest, src]
                        match prop {
                            ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(
                                p,
                            ) => {
                                self.emit(Instr::PushStr(p.binding.name.as_str().into()), span);
                                self.emit_rest_excluded_key_access(span);
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
                                self.emit_property_key_string(&p.name, p.computed, span);
                                self.emit_rest_excluded_key_access(span);
                                self.assign_maybe_default(&p.binding, span);
                            }
                        }
                    }
                    self.assign_target_leaf(&rest.target, span); // [src]
                } else {
                    for prop in &obj.properties {
                        match prop {
                            ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(
                                p,
                            ) => {
                                // Shorthand `{a}` / `{a = d}`: the key and the target
                                // are the same identifier.
                                self.emit(Instr::Pick(0), span);
                                self.emit(Instr::ObjGet(p.binding.name.as_str().into()), span);
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
                                self.emit(Instr::Pick(0), span);
                                self.emit_property_key_access(&p.name, p.computed, span);
                                self.assign_maybe_default(&p.binding, span);
                            }
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
        // An eliminated `const` (Phase E) has no slot; reassigning it is an error.
        if self.const_ref(id_span).is_some() {
            self.error(id_span, format!("assignment to constant `{name}`"));
            self.emit(Instr::Pop(1), span);
            return;
        }
        match self.ref_slot(id_span) {
            Some(r) => {
                if r.is_const {
                    self.error(id_span, format!("assignment to constant `{name}`"));
                }
                self.emit(Instr::SetLocal(r.slot as LocalIndex), span);
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
        // Check for spread arguments — if present, use `CallSpread` path.
        let has_spread = call
            .arguments
            .iter()
            .any(|arg| matches!(arg, ast::Argument::SpreadElement(_)));

        if has_spread {
            return self.compile_call_spread(call, span);
        }

        // Fast path: no spread — existing argument-collection logic.
        let mut argv: Vec<&ast::Expression> = Vec::with_capacity(call.arguments.len());
        for arg in &call.arguments {
            match arg.as_expression() {
                Some(e) => argv.push(e),
                None => unreachable!(),
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
                    if Builtin::for_namespace(obj.name.as_str(), m.property.name.as_str()).is_some()
                    {
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
                        "Math" | "Object" | "JSON" | "Number" | "Array" | "Map" | "Set" | "console" => {
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
                            self.compile_args(&argv);
                            self.emit(Instr::Invoke(method.into(), argv.len() as u32), span);
                            return;
                        }
                        "Promise" => {
                            return self.compile_promise_call(method, &argv, span);
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

    /// Compile a call with spread arguments: lower to callee expression +
    /// array of args + [`CallSpread`].
    fn compile_call_spread(&mut self, call: &ast::CallExpression, span: u32) {
        // `tools.foo(...args)` can't take the value path: `tools` is only
        // valid structurally as an `Invoke` receiver (compiling it as an
        // expression would give a misleading undeclared-variable error), and
        // `Invoke` has a static arg count. Reject with a targeted error.
        if let ast::Expression::StaticMemberExpression(m) = &call.callee {
            if let ast::Expression::Identifier(obj) = &m.object {
                if obj.name.as_str() == "tools" {
                    self.error(span, "spread arguments are not supported on tool calls");
                    return;
                }
            }
        }

        // Compile callee as a value expression (produces the callable on stack).
        self.compile_expr(&call.callee);

        if call.optional {
            // optional call `f?.(...args)`: short-circuit to undefined when
            // nullish, else compile args and dispatch.
            let end = self.begin_optional(span);
            self.compile_call_args_array(&call.arguments, span);
            self.emit(Instr::Dig(1), span);
            self.emit(Instr::CallSpread, span);
            self.emit(Instr::Label(end), span);
        } else {
            self.compile_call_args_array(&call.arguments, span);
            self.emit(Instr::Dig(1), span);
            self.emit(Instr::CallSpread, span);
        }
    }

    /// Compile call arguments into an array on the stack.  Supports spread
    /// elements: leading static args + `ArrNew`, then `ArrExtend` for each
    /// spread and `ArrPush` for each trailing static argument.
    fn compile_call_args_array(&mut self, args: &oxc_allocator::Vec<'_, ast::Argument>, span: u32) {
        // Count leading non-spread arguments.
        let leading_count = args
            .iter()
            .take_while(|a| !matches!(a, ast::Argument::SpreadElement(_)))
            .count();

        // Emit values for leading static segment.
        for arg in args.iter().take(leading_count) {
            if let Some(e) = arg.as_expression() {
                self.compile_expr(e);
            }
        }
        self.emit(Instr::ArrNew(leading_count as u32), span);

        // Remaining: alternates spreads and single-element pushes.
        for arg in args.iter().skip(leading_count) {
            match arg {
                ast::Argument::SpreadElement(s) => {
                    self.compile_expr(&s.argument);
                    self.emit(Instr::ArrExtend, span);
                }
                _ => {
                    if let Some(e) = arg.as_expression() {
                        self.compile_expr(e);
                        self.emit(Instr::ArrPush, span);
                    }
                }
            }
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
        // Optional method call: guard on the receiver before the args/call.
        // `JNotNullish` (via `begin_optional`) keeps the receiver on the
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
    /// looking the receiver-less builtin up in the declarative table.
    fn compile_namespace_call(
        &mut self,
        ns: &str,
        method: &str,
        argv: &[&ast::Expression],
        span: u32,
    ) {
        match Builtin::for_namespace(ns, method) {
            Some(builtin) => self.compile_builtin_call(builtin, None, argv, span, false),
            None => self.error(span, format!("unsupported `{ns}.{method}`")),
        }
    }

    /// `Promise.*` statics. `Promise.all(xs)` and `Promise.allSettled(xs)`
    /// are supported (Phase 7), lowering to the prelude helpers `__all(xs)`
    /// / `__allSettled(xs)` — serial awaits over already-started promises,
    /// which is full fan-out concurrency because every tool call in `xs` is
    /// already in flight. The rest are rejected with diagnostics that say
    /// what to do instead.
    fn compile_promise_call(&mut self, method: &str, argv: &[&ast::Expression], span: u32) {
        match method {
            "all" | "allSettled" => {
                if argv.len() != 1 {
                    self.error(
                        span,
                        format!("`Promise.{method}` expects 1 argument, got {}", argv.len()),
                    );
                    return;
                }
                let helper = if method == "all" {
                    "__all"
                } else {
                    "__allSettled"
                };
                self.emit_prelude_call(helper, argv[0], &[], span, false);
            }
            // Wait-any needs VM support — deferred until evidence demands it.
            "race" | "any" => self.error(
                span,
                format!(
                    "`Promise.{method}` is not supported (await the promises you need directly)"
                ),
            ),
            // Pointless wrappers in this dialect: `await` passes plain values
            // through, and rejection is the error path, not a value.
            "resolve" | "reject" => self.error(
                span,
                format!(
                    "`Promise.{method}` is not supported (`await` accepts plain values directly)"
                ),
            ),
            _ => self.error(span, format!("unsupported `Promise.{method}`")),
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
            "parseInt" => {
                // Delegate to Number.parseInt
                self.compile_builtin_call(Builtin::NumberParseInt, None, argv, span, false);
            }
            "parseFloat" => {
                // Delegate to Number.parseFloat
                self.compile_builtin_call(Builtin::NumberParseFloat, None, argv, span, false);
            }
            "isNaN" => {
                // Bare isNaN coerces to number first (unlike Number.isNaN):
                // isNaN(x) ≡ Number.isNaN(Number(x))
                if !self.arity(argv, 1, span, "isNaN") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::ToNum, span);
                // NaN !== NaN is the canonical check.
                self.emit(Instr::Pick(0), span);
                self.emit(Instr::Neq, span); // [v !== v] = true only for NaN
            }
            "isFinite" => {
                // Bare isFinite coerces to number first (unlike Number.isFinite).
                if !self.arity(argv, 1, span, "isFinite") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::ToNum, span);
                // After ToNum: coerce to Number.isFinite.
                self.emit(Instr::CallBuiltin(Builtin::NumberIsFinite, 1), span);
            }
            "raise" => {
                // `raise("name")` → `Raise(name, 0)`, no payload.
                // `raise("name", expr)` → `Raise(name, 1)`, payload = expr.
                // The condition name must be a string literal. More than one
                // extra arg is a compile error.
                if argv.is_empty() || argv.len() > 2 {
                    self.error(
                        span,
                        "`raise` takes 1 or 2 arguments: raise(\"name\") or raise(\"name\", payload)",
                    );
                    return;
                }
                let name = match &argv[0] {
                    ast::Expression::StringLiteral(lit) => lit.value.as_str().into(),
                    other => {
                        self.error(
                            other.span().start,
                            "`raise` condition name must be a string literal",
                        );
                        return;
                    }
                };
                if argv.len() >= 2 {
                    // Payload: compile the expression, emit Raise with argc=1.
                    self.compile_expr(&argv[1]);
                    self.emit(Instr::Raise(name, 1), span);
                } else {
                    self.emit(Instr::Raise(name, 0), span);
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
        // ── higher-order array methods (prelude helpers) ──────────
        match method {
            "map" => return self.compile_hof(recv, argv, span, optional, "__map", 1),
            "filter" => return self.compile_hof(recv, argv, span, optional, "__filter", 1),
            "forEach" => return self.compile_hof(recv, argv, span, optional, "__forEach", 1),
            "some" => return self.compile_hof(recv, argv, span, optional, "__some", 1),
            "every" => return self.compile_hof(recv, argv, span, optional, "__every", 1),
            "find" => return self.compile_hof(recv, argv, span, optional, "__find", 1),
            "findIndex" => return self.compile_hof(recv, argv, span, optional, "__findIndex", 1),
            "reduce" => return self.compile_reduce(recv, argv, span, optional),
            "flatMap" => return self.compile_hof(recv, argv, span, optional, "__flatMap", 1),
            "findLast" => return self.compile_hof(recv, argv, span, optional, "__findLast", 1),
            "findLastIndex" => {
                return self.compile_hof(recv, argv, span, optional, "__findLastIndex", 1);
            }
            "sort" => return self.compile_sort(recv, argv, span, optional),
            _ => {}
        }
        if let Some(builtin) = Builtin::for_method(method) {
            self.compile_builtin_call(builtin, Some(recv), argv, span, optional);
            return;
        }
        // Not a known builtin method — treat as property access
        // followed by dynamic call (e.g. `state.add5(3)` where
        // add5 is a function stored in state).
        self.compile_dynamic_method_call(recv, method, argv, span, optional);
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
            self.emit(Instr::ObjGet(method.into()), span);
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
            self.emit(Instr::ObjGet(method.into()), span);
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

    /// `arr.sort([compareFn])`. With a comparator → `__sort(a, f)`;
    /// without → `__sortDefault(a)` (the JS default string comparison).
    fn compile_sort(
        &mut self,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: u32,
        optional: bool,
    ) {
        match argv.len() {
            1 => self.emit_prelude_call("__sort", recv, argv, span, optional),
            0 => self.emit_prelude_call("__sortDefault", recv, argv, span, optional),
            n => self.error(span, format!("`sort` expects 0 or 1 argument(s), got {n}")),
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
        // A constant function (Phase F): no slot — call its label statically.
        // Pad missing args to the declared arity (as the slotted path does).
        if let Some(ConstValue::Fn { label, arity }) = self.const_ref(callee_span) {
            self.compile_args(argv);
            let passed = argv.len() as u32;
            for _ in passed..arity {
                self.emit(Instr::PushUndefined, span);
            }
            self.emit(Instr::Call(label, passed.max(arity)), span);
            return;
        }
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
                        self.emit(Instr::PushUndefined, span);
                    }
                    self.emit(Instr::Call(l, passed.max(expected_arity)), span);
                }
                _ => {
                    // Dynamic call: push args, load callee, CallDyn (installs
                    // upvals for captured/closure callees). The callee read
                    // goes through `emit_slot_read`: an effectively-const
                    // binding's slot may be dead-eliminated, so a raw `Local`
                    // load here would read undefined (and miscall) instead of
                    // the propagated constant.
                    self.compile_args(argv);
                    self.emit_slot_read(&r, span);
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
                return child.declared_arity();
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
                // A constant function (Phase F) has no live slot — its binding
                // store is dead (references/calls go through its `Fn` constant).
                if self.is_const_fn_scope(scope_id) {
                    return;
                }
                if let Some(id) = &f.id {
                    if let Some(slot) = self.binding_slot(id.span.start) {
                        let span = f.span.start;
                        if captures.is_empty() {
                            self.emit(Instr::PushFn(label), span);
                        } else {
                            self.emit(
                                Instr::MakeClosure(
                                    label,
                                    captures.iter().map(|&c| c as LocalIndex).collect(),
                                ),
                                span,
                            );
                        }
                        self.emit(Instr::SetLocal(slot as LocalIndex), span);
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
            ast::Statement::TryStatement(t) => {
                for s in &t.block.body {
                    self.hoist_function_decl_in_stmt(s);
                }
                if let Some(h) = &t.handler {
                    for s in &h.body.body {
                        self.hoist_function_decl_in_stmt(s);
                    }
                }
                if let Some(f) = &t.finalizer {
                    for s in &f.body {
                        self.hoist_function_decl_in_stmt(s);
                    }
                }
            }
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

    /// If `init` is a function expression that is a constant function (Phase F),
    /// emit only its body (the jump-over guards it) — no value push, no store,
    /// since references resolve to its `Fn` constant — and return `true`.
    fn emit_const_fn_expr_body(&mut self, init: &ast::Expression) -> bool {
        let span = match init {
            ast::Expression::ArrowFunctionExpression(a) => a.span.start,
            ast::Expression::FunctionExpression(f) => f.span.start,
            _ => return false,
        };
        let Some(scope_id) = self.scope_for_node(span) else {
            return false;
        };
        if !self.is_const_fn_scope(scope_id) {
            return false;
        }
        match init {
            ast::Expression::ArrowFunctionExpression(a) => {
                self.emit_function_def(scope_id, &a.body.statements, &a.params, span, a.expression);
            }
            ast::Expression::FunctionExpression(f) => {
                if let Some(body) = &f.body {
                    self.emit_function_def(scope_id, &body.statements, &f.params, span, false);
                }
            }
            _ => unreachable!(),
        }
        true
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
            self.emit(Instr::PushFn(label), span);
        } else {
            self.emit(
                Instr::MakeClosure(label, captures.iter().map(|&c| c as LocalIndex).collect()),
                span,
            );
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
            captures,
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
                scope.captures.clone(),
            )
        };
        let nparams = params_info.len() as u32;
        let has_rest = params_info.last().map(|p| p.is_rest).unwrap_or(false);
        let nregular = if has_rest { nparams - 1 } else { nparams };

        let prev_scope = self.current_scope;
        self.current_scope = scope_id;
        // Barriers don't cross function boundaries: a `return` in this body
        // must pop only THIS body's handlers, never the enclosing function's
        // (those belong to a different frame), and a body defined inside a
        // `finally` block is a fresh frame with no pending completion. The
        // spill slot is per-frame for the same reason.
        let prev_barriers = std::mem::take(&mut self.barriers);
        let prev_return_spill = self.return_spill.take();
        // Slot numbers are frame-relative, so the callee gets its own const env.
        // Phase D: seed it with constants captured *by value* — a `const`/
        // effectively-const capture is an immutable snapshot, so the upval holds a
        // fixed value. `captures[i]` is the parent slot; it installs at the child's
        // upval slot `nparams + i`, so a reference to that upval propagates the
        // literal inside the closure body.
        let prev_const_env = std::mem::take(&mut self.const_env);
        let mut child_const_env: HashMap<u32, Instr> = HashMap::new();
        for (i, &parent_slot) in captures.iter().enumerate() {
            if let Some(push) = prev_const_env.get(&parent_slot) {
                child_const_env.insert(nparams + i as u32, push.clone());
            }
        }
        self.const_env = child_const_env;

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
        // The self-reference slot — except for a constant function, which refers
        // to itself by its `Fn` constant, so the slot would be dead.
        if self_name.is_some() && !self.is_const_fn_scope(scope_id) {
            local_kinds.push(SlotKind::Plain);
        }
        // The return spill slot would land just past everything; the
        // `EnterFrame` emitted here is patched post-body to allocate it,
        // only if some `return` crossing a `finally` actually used it.
        let enter_frame_at = self.code.len();
        self.return_spill = Some(ReturnSpill {
            slot: nparams + upval_count + local_kinds.len() as u32,
            enter_frame: Ok(enter_frame_at),
            used: false,
        });
        self.emit(
            Instr::EnterFrame(nparams as u16, uses_arguments, local_kinds.into()),
            span,
        );

        // Per-parameter prologue: apply defaults (the arg is already in the slot)
        // and box captured params in place. Plain params with no default need no
        // code — their value is already in the local slot.  Skip the rest param
        // (if any) — it is handled separately below.
        for (p_idx, param_info) in params_info.iter().enumerate() {
            if param_info.is_rest {
                continue;
            }
            let slot = p_idx as u32; // params occupy slots 0..nparams
            let item = &params.items[p_idx];
            if matches!(&item.pattern, ast::BindingPattern::BindingIdentifier(_)) {
                let needs_box = matches!(slot_kinds.get(p_idx).copied(), Some(SlotKind::Boxed));
                let default_expr = item.initializer.as_ref().map(|v| &**v);
                self.emit_param_setup(slot, needs_box, param_info.has_default, default_expr, span);
            } else {
                // Destructuring param: the argument sits in an anonymous slot
                // (never captured — its name is not a legal identifier). Load
                // it, apply the whole-pattern default, and run the normal
                // pattern lowering into the leaf bindings (own locals; captured
                // ones got their cells from `EnterFrame`, and `SetLocal`
                // writes through cells).
                let pat_span = item.span.start;
                self.emit(Instr::Local(slot as LocalIndex), pat_span);
                if let Some(default) = &item.initializer {
                    self.emit_default(default, pat_span);
                }
                self.destructure_binding(&item.pattern, pat_span);
            }
        }

        // Rest parameter: build the rest array from `arguments.slice(nregular)`.
        // `EnterFrame` eagerly built the arguments cache (see uses_arguments above)
        // from ALL caller args before truncating to nparams, so `arguments` always
        // holds the full argument list.  `arguments.slice(nregular)` gives the
        // surplus elements that become the rest array.
        if has_rest {
            let rest_slot = nregular as u32;
            self.emit(Instr::Arguments, span);
            self.emit(Instr::PushPosInt(nregular as u64), span);
            self.emit(Instr::CallBuiltin(Builtin::StrSlice, 2), span);
            let rest_pat = &params
                .rest
                .as_ref()
                .expect("has_rest implies rest")
                .rest
                .argument;
            if matches!(rest_pat, ast::BindingPattern::BindingIdentifier(_)) {
                let needs_box = matches!(
                    slot_kinds.get(rest_slot as usize).copied(),
                    Some(SlotKind::Boxed)
                );
                self.emit(Instr::SetLocal(rest_slot as LocalIndex), span);
                if needs_box {
                    self.emit(Instr::FreshCell(rest_slot as LocalIndex), span);
                }
            } else {
                // Pattern rest (`...[a, b]`): destructure the freshly built
                // array directly; the anonymous rest slot stays undefined.
                self.destructure_binding(rest_pat, rest_pat.span().start);
            }
        }

        // Self-reference (named function expression / recursive declaration): the
        // slot was allocated by EnterFrame above; fill it with the bare `Fn`.
        // A constant function (Phase F) refers to itself by its `Fn` constant
        // (static self-recursion), so the self-slot is dead — skip the setup.
        if self_name.is_some() && !self.is_const_fn_scope(scope_id) {
            let self_slot = frame_abs(own_local_count, nparams, upval_count);
            self.emit(Instr::PushFn(label), span);
            self.emit(Instr::SetLocal(self_slot as LocalIndex), span);
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
            self.emit(Instr::PushUndefined, span);
            self.emit(Instr::Return(1), span);
        }

        self.emit(Instr::Label(after), span);
        if let Some(spill) = self.return_spill.take() {
            self.finalize_return_spill(spill, span);
        }
        self.current_scope = prev_scope;
        self.const_env = prev_const_env;
        self.barriers = prev_barriers;
        self.return_spill = prev_return_spill;
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
                self.emit(Instr::Local(slot as LocalIndex), span);
                self.emit(Instr::PushUndefined, span);
                self.emit(Instr::Eq, span);
                self.emit(Instr::JFalse(skip_default), span);
                self.compile_expr(default);
                self.emit(Instr::SetLocal(slot as LocalIndex), span);
                self.emit(Instr::Label(skip_default), span);
            }
        }
        if needs_box {
            // Promote the plain arg value in the slot to a shared cell.
            self.emit(Instr::FreshCell(slot as LocalIndex), span);
        }
    }
}

/// Represents a compile-time constant value (used for namespace member reads).
enum ConstVal {
    Float(f64),
    PosInt(u64),
}

/// The standard error constructor names recognized by `new` (6B decision 3):
/// each builds a plain `{ name, message }` object — there are no error
/// classes, prototypes, or `instanceof`.
fn is_error_ctor(name: &str) -> bool {
    matches!(
        name,
        "Error" | "TypeError" | "RangeError" | "SyntaxError" | "ReferenceError" | "EvalError"
    )
}

/// Build a flags string (e.g. `"gi"`) from an oxc [`RegExpFlags`] bitmask.
fn regexp_flags_to_str(flags: ast::RegExpFlags) -> String {
    let mut s = String::with_capacity(4);
    if flags.contains(ast::RegExpFlags::G) {
        s.push('g');
    }
    if flags.contains(ast::RegExpFlags::I) {
        s.push('i');
    }
    if flags.contains(ast::RegExpFlags::M) {
        s.push('m');
    }
    if flags.contains(ast::RegExpFlags::S) {
        s.push('s');
    }
    if flags.contains(ast::RegExpFlags::U) {
        s.push('u');
    }
    if flags.contains(ast::RegExpFlags::Y) {
        s.push('y');
    }
    if flags.contains(ast::RegExpFlags::D) {
        s.push('d');
    }
    if flags.contains(ast::RegExpFlags::V) {
        s.push('v');
    }
    s
}

/// Map a namespace + member name to a compile-time constant, if any.
fn namespace_constant(ns: &str, member: &str) -> Option<ConstVal> {
    match (ns, member) {
        ("Math", "PI") => Some(ConstVal::Float(std::f64::consts::PI)),
        ("Math", "E") => Some(ConstVal::Float(std::f64::consts::E)),
        ("Number", "MAX_SAFE_INTEGER") => Some(ConstVal::PosInt(9007199254740991)),
        ("Number", "EPSILON") => Some(ConstVal::Float(f64::EPSILON)),
        _ => None,
    }
}

/// Canonicalize a non-negative numeric literal: an integer in `u64` range
/// becomes a `PosInt`, otherwise a `Number`. Literals are non-negative; unary
/// minus is a separate operator folded via `f64_to_value`.
fn number_literal_to_value(value: f64) -> Value {
    if value.fract() == 0.0 && value >= 0.0 && value <= u64::MAX as f64 {
        Value::PosInt(value as u64)
    } else {
        Value::Float(value)
    }
}

/// Canonicalize an arbitrary (possibly negative) f64 into the VM's integer
/// variants when it is integral and in range, mirroring serde_json's split:
/// non-negative → `PosInt`, negative → `NegInt`, otherwise `Number`.
fn f64_to_value(value: f64) -> Value {
    if value.fract() == 0.0 && value >= 0.0 && value <= u64::MAX as f64 {
        Value::PosInt(value as u64)
    } else if value.fract() == 0.0 && value < 0.0 && value >= i64::MIN as f64 {
        Value::NegInt(value as i64)
    } else {
        Value::Float(value)
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

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
