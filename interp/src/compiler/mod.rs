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
use oxc_span::SourceType;

use crate::analyzer::{self, ProgramAnalysis};
use crate::diag::Diagnostic;
use crate::vm::RcStr;
use crate::vm::{Instr, Value};

mod analysis;
mod assign;
mod call;
mod control_flow;
mod destructure;
mod emit;
mod expr;
mod function;
mod literals;
mod member;
mod operators;
mod stmt;

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

// ── impl blocks live in the sub-modules above ────────────────────────
//
// Each sub-file contains `impl<'src> Compiler<'src> { … }` with the
// methods listed below.  They're organised by concern:
//
//  emit.rs        — new, new_label, emit, intern_string, error
//  analysis.rs    — binding_slot, scope_for_node, slot_needs_fresh, …
//  destructure.rs — destructure_binding, emit_default, emit_property_key_*, emit_pattern_fresh_cells
//  stmt.rs        — compile_program, compile_stmt, compile_var_decl
//  control_flow.rs— compile_if/while/for/break/continue, emit_exit,
//                   compile_try, compile_for_of/in, compile_switch
//  expr.rs        — compile_expr, compile_identifier, error_ctor, …
//  operators.rs   — compile_binary/unary, compile_delete, compile_logical
//  literals.rs    — compile_array, compile_object, compile_template
//  member.rs      — compile_static_member, compile_computed_member, …
//  assign.rs      — compile_assignment, compile_update, lvalue_*, …
//  call.rs        — compile_call, compile_method_call, compile_hof, …
//  function.rs    — hoist_function_decls, emit_function_def, …
// All method bodies were moved to the sub-module files listed above.

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
