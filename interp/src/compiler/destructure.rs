use oxc_ast::ast;
use oxc_span::GetSpan;

use crate::builtin::Builtin;
use crate::vm::{Instr, LocalIndex, RcStr};

impl<'src> super::Compiler<'src> {
    /// Apply a destructuring/parameter default to the value on top of the stack:
    /// if it is `undefined`, replace it with the default expression's value;
    /// otherwise leave it. (JS applies defaults only for `undefined`, not
    /// `null`.) Leaves exactly one value either way.
    pub(super) fn emit_default(&mut self, default: &ast::Expression, span: u32) {
        let have = self.new_label();
        self.emit(Instr::Pick(0), span);
        self.emit(Instr::PushUndefined, span);
        self.emit(Instr::Eq, span);
        self.emit(Instr::JFalse(have), span); // not undefined → keep the value
        self.emit(Instr::Pop(1), span); // undefined → drop and use the default
        self.compile_expr(default);
        self.emit(Instr::Label(have), span);
    }

    /// With an object on top of the stack, read the property named by a pattern
    /// key, leaving its value on top (consuming the object copy). A static key
    /// uses the fast `ObjGet`; a computed key evaluates the expression and uses
    /// the polymorphic `IndexGet`.
    pub(super) fn emit_property_key_access(
        &mut self,
        key: &ast::PropertyKey,
        computed: bool,
        span: u32,
    ) {
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
            ast::PropertyKey::NumericLiteral(n) => super::number_key_to_string(n.value),
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
    pub(super) fn emit_property_key_string(
        &mut self,
        key: &ast::PropertyKey,
        computed: bool,
        span: u32,
    ) {
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
            ast::PropertyKey::NumericLiteral(n) => super::number_key_to_string(n.value),
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
    pub(super) fn emit_rest_excluded_key_access(&mut self, span: u32) {
        self.emit(Instr::Pick(0), span); //   [src, rest, src, key, key]
        self.emit(Instr::Pick(3), span); //   [src, rest, src, key, key, rest]
        self.emit(Instr::Dig(1), span); //    [src, rest, src, key, rest, key]
        self.emit(Instr::ObjDelete, span); // [src, rest, src, key, existed]
        self.emit(Instr::Pop(1), span); //    [src, rest, src, key]
        self.emit(Instr::IndexGet, span); //  [src, rest, value]
    }

    /// Destructure the source value already on top of the stack into a binding
    /// pattern, **consuming** that value. Used by declarations; every leaf is a
    /// binding identifier whose slot comes from analysis (`binding_slot`).
    pub(super) fn destructure_binding(&mut self, pat: &ast::BindingPattern, span: u32) {
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

    /// Emit `FreshCell` for every captured binding in a loop-head pattern, so
    /// in-loop closures capture per-iteration copies (the single-identifier
    /// form does the same inline in `compile_index_loop`).
    pub(super) fn emit_pattern_fresh_cells(&mut self, pat: &ast::BindingPattern, span: u32) {
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
}
