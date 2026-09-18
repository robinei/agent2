//! Source span type shared by the compiler and VM.
//!
//! Every VM instruction is tagged with one of these (`Program::spans` /
//! `Compiler::spans` / `VM::spans`), so anything downstream — a runtime
//! diagnostic, or a host logging a call — can point at the whole source
//! expression that produced an instruction, not just its first byte.
//!
//! Almost every site constructs a `Span` straight from an AST node's own
//! `oxc_span::Span` (which already carries both ends — nothing is computed,
//! only kept instead of discarded). Where an instruction is synthetic and
//! has no single source node of its own (a compiler-inserted prologue op,
//! say), `start == end` and the call site says so in a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    /// A span with an explicit `[start, end)` — for the rare site that
    /// computes its own end rather than borrowing an AST node's.
    pub fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }

    /// A zero-width span at a single byte offset, for instructions with no
    /// source expression of their own to underline.
    pub fn point(at: u32) -> Self {
        Span { start: at, end: at }
    }
}

impl From<oxc_span::Span> for Span {
    fn from(s: oxc_span::Span) -> Self {
        Span {
            start: s.start,
            end: s.end,
        }
    }
}
