//! Debug info for the debugger TUI (9_TUI Step 1): function names, source
//! ranges, and frame-slot names — built by the compiler from the analyzer's
//! scope tables, carried on [`crate::compiler::Program`], and consumed by
//! the VM's introspection accessors (`VM::function_at`, `VM::frames`,
//! `VM::disasm`).
//!
//! Function identity is **span-based**: an instruction belongs to the
//! innermost function whose *source* span contains the instruction's span
//! (`spans[ip]`). The optimizer passes already maintain `spans` in lockstep
//! with `code`, so attribution survives every rewrite with no parallel
//! table and no range bookkeeping — it is per-instruction, not per-range.

/// Debug info for one function (or the root program).
#[derive(Debug, Clone)]
pub struct FnDebug {
    /// Best-effort name: the declaration/expression name (`function f`),
    /// the binding name (`const f = () => …`), or `"<anonymous>"`;
    /// `"<root>"` for the top-level program.
    pub name: String,
    /// Source byte range of the defining function node (root: the whole
    /// source, prelude included).
    pub span_start: u32,
    pub span_end: u32,
    /// Frame slot → declared name under the `[params | upvals | own
    /// locals | self?]` layout. `None` for synthetic slots (anonymous
    /// destructuring-param slots, the return spill).
    pub slot_names: Vec<Option<String>>,
}

/// Per-program debug table, indexed by analyzer scope id.
#[derive(Debug, Clone, Default)]
pub struct DebugTable {
    pub functions: Vec<FnDebug>,
    /// Index of the root program entry in `functions`.
    pub root: usize,
}

impl DebugTable {
    /// The function containing a source offset: the innermost function
    /// whose span contains it, else the root. `None` only for an empty
    /// table (hand-assembled programs compiled without debug info).
    pub fn function_at_span(&self, offset: u32) -> Option<usize> {
        if self.functions.is_empty() {
            return None;
        }
        // Function spans nest lexically, so the innermost containing
        // function is the one with the greatest start offset.
        let mut best = self.root;
        let mut best_start = 0u32;
        for (i, f) in self.functions.iter().enumerate() {
            if i != self.root
                && f.span_start <= offset
                && offset < f.span_end
                && f.span_start >= best_start
            {
                best = i;
                best_start = f.span_start;
            }
        }
        Some(best)
    }
}
