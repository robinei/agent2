//! Compile/run-time diagnostics shared by the analyzer and codegen.

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

/// A compile- or run-time diagnostic anchored at a source byte offset.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    /// Source byte offset the diagnostic points at.
    pub span: u32,
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
    /// Render as `line:col: message` followed by the offending source line and
    /// a caret under the offending column.
    pub fn render(&self, source: &str) -> String {
        let offset = (self.span as usize).min(source.len());
        let (line, col, line_start) = line_col(source, self.span);
        let line_end = source[line_start..]
            .find('\n')
            .map(|p| line_start + p)
            .unwrap_or(source.len());
        let src_line = &source[line_start..line_end];
        let caret = format!("{}^", " ".repeat(offset - line_start));
        format!("{line}:{col}: {}\n{src_line}\n{caret}", self.message)
    }
}
