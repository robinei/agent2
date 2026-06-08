//! Compile/run-time diagnostics shared by the analyzer and codegen.

/// A compile- or run-time diagnostic anchored at a source byte offset.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    /// Source byte offset the diagnostic points at.
    pub span: u32,
    pub message: String,
}

impl Diagnostic {
    /// Render as `line:col: message` followed by the offending source line and
    /// a caret under the offending column.
    pub fn render(&self, source: &str) -> String {
        let offset = (self.span as usize).min(source.len());
        // Find the start of the line containing `offset`, and the 1-based line
        // number, by scanning newlines up to it.
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
        let col = offset - line_start + 1;
        let line_end = source[line_start..]
            .find('\n')
            .map(|p| line_start + p)
            .unwrap_or(source.len());
        let src_line = &source[line_start..line_end];
        let caret = format!("{}^", " ".repeat(offset - line_start));
        format!("{line}:{col}: {}\n{src_line}\n{caret}", self.message)
    }
}
