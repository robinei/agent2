use std::collections::{HashMap, HashSet};

use crate::diag::{DiagKind, Diagnostic};
use crate::vm::{Instr, RcStr};

impl super::Compiler {
    pub(super) fn new() -> Self {
        super::Compiler {
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
    pub(super) fn new_label(&mut self) -> u32 {
        let id = self.next_label;
        self.next_label += 1;
        id
    }

    /// Append an instruction with its source span (byte offset).
    pub(super) fn emit(&mut self, instr: Instr, span: u32) {
        self.code.push(instr);
        self.spans.push(span);
    }

    /// Intern a string literal, returning a shared `RcStr`. Deduplicated by
    /// content — identical literals across the program share one allocation, so
    /// the embedded `PushStr` operands (and the values they push at runtime) are
    /// all clones of the same block.
    pub(super) fn intern_string(&mut self, s: &str) -> RcStr {
        if let Some(existing) = self.interned.get(s) {
            return existing.clone();
        }
        let rc = RcStr::from(s);
        self.interned.insert(rc.clone());
        rc
    }

    /// Record a semantic diagnostic; aborts the compile before a `Program` is
    /// produced.
    pub(super) fn error(&mut self, span: u32, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            message: message.into(),
            kind: DiagKind::Semantic,
        });
    }

    /// Record a parse diagnostic (from `oxc_parser`).
    pub(super) fn parse_error(&mut self, span: u32, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            message: message.into(),
            kind: DiagKind::Parse,
        });
    }
}
