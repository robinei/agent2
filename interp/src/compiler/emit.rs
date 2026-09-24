use std::collections::{HashMap, HashSet};

use crate::diag::{DiagKind, Diagnostic};
use crate::span::Span;
use crate::vm::{Instr, JsString};

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
            pin_root: false,
            no_top_level_return: None,
            prologue_fn_decls: HashSet::new(),
        }
    }

    /// Allocate a fresh label id.
    pub(super) fn new_label(&mut self) -> u32 {
        let id = self.next_label;
        self.next_label += 1;
        id
    }

    /// Append an instruction with its source span (byte range).
    pub(super) fn emit(&mut self, instr: Instr, span: Span) {
        self.code.push(instr);
        self.spans.push(span);
    }

    /// Intern a string literal, returning a shared `JsString`. Deduplicated by
    /// content — identical literals across the program share one allocation, so
    /// the embedded `PushStr` operands (and the values they push at runtime) are
    /// all clones of the same block.
    pub(super) fn intern_string(&mut self, s: &str) -> JsString {
        self.intern_units(&crate::units::from_str(s))
    }

    /// [`intern_string`](Self::intern_string) for content that is already code
    /// units — which a source literal holding a lone surrogate has to be,
    /// since no `&str` can carry one. The set is keyed by `JsString`, which
    /// borrows as `[u16]`, so this is the primitive and the `&str` form widens
    /// into it.
    pub(super) fn intern_units(&mut self, units: &[u16]) -> JsString {
        if let Some(existing) = self.interned.get(units) {
            return existing.clone();
        }
        let rc = JsString::from_units(units);
        self.interned.insert(rc.clone());
        rc
    }

    /// Record a semantic diagnostic; aborts the compile before a `Program` is
    /// produced.
    pub(super) fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            message: message.into(),
            kind: DiagKind::Semantic,
        });
    }

    /// Record a parse diagnostic (from `oxc_parser`).
    pub(crate) fn parse_error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            message: message.into(),
            kind: DiagKind::Parse,
        });
    }
}
