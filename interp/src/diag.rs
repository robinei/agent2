//! Compile/run-time diagnostics shared by the analyzer and codegen.

use crate::span::Span;

/// Which pass produced a [`Diagnostic`]. Lets the conformance runner bucket
/// failures honestly (oxc parse errors vs. our own semantic rejections),
/// instead of folding every compile error into one "parse error" bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagKind {
    /// An error from the `oxc_parser` pass (syntax): "Unexpected token",
    /// "Cannot assign to this expression", etc.
    Parse,
    /// An error from our analyzer or codegen (semantics): "undeclared
    /// variable", "X is not supported", "assignment to constant", etc.
    Semantic,
}

/// A compile- or run-time diagnostic anchored at a source byte range.
/// `render` underlines the whole range when it's wider than one byte,
/// and falls back to a single caret when `span.start == span.end` (a
/// synthetic site with no expression of its own to underline).
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    /// Source byte range the diagnostic points at.
    pub span: Span,
    pub message: String,
    /// Which pass produced this diagnostic.
    pub kind: DiagKind,
}

/// 1-based `(line, col)` of a byte offset in `source`, with the offset of the
/// containing line's start (for caret rendering).
pub fn line_col(source: &str, span: u32) -> (usize, usize, usize) {
    let offset = (span as usize).min(source.len());
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
    (line, offset - line_start + 1, line_start)
}

impl Diagnostic {
    /// Render as `line:col: message` followed by the offending source line
    /// and an underline beneath it: a single caret for a zero-width span
    /// (`start == end`), or a run of `^` under the whole `[start, end)` when
    /// the span carries real width — clipped to the line, since an
    /// underline never wraps.
    pub fn render(&self, source: &str) -> String {
        let start = (self.span.start as usize).min(source.len());
        let end = (self.span.end as usize).min(source.len()).max(start);
        let (line, col, line_start) = line_col(source, self.span.start);
        let line_end = source[line_start..]
            .find('\n')
            .map(|p| line_start + p)
            .unwrap_or(source.len());
        let src_line = &source[line_start..line_end];
        let underline_end = end.min(line_end);
        let width = underline_end.saturating_sub(start).max(1);
        let caret = format!("{}{}", " ".repeat(start - line_start), "^".repeat(width));
        format!("{line}:{col}: {}\n{src_line}\n{caret}", self.message)
    }
}
