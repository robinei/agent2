use oxc_ast::ast;

use crate::builtin::Builtin;
use crate::vm::Instr;

impl<'src> super::Compiler<'src> {
    /// `obj.foo` (and `state.foo`, since `state` lowers to `Ptr(0)`). `.length`
    /// lowers to `ArrLength` (str/arr intrinsic; on an object, the `length`
    /// property) and `.size` to `MapSetSize` (map/set intrinsic; on an object,
    /// the `size` property); anything else is `ObjGet`.
    pub(super) fn compile_static_member(&mut self, m: &ast::StaticMemberExpression) {
        // Namespace constants and first-class builtin refs: handle before
        // evaluating the object.
        if let ast::Expression::Identifier(obj) = &m.object {
            // Constants: fold to compile-time values.
            if let Some(val) =
                super::namespace_constant(obj.name.as_str(), m.property.name.as_str())
            {
                let span = m.span.start;
                match val {
                    super::ConstVal::Float(f) => self.emit(Instr::PushFloat(f), span),
                    super::ConstVal::PosInt(n) => self.emit(Instr::PushPosInt(n), span),
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

    pub(super) fn emit_static_access(&mut self, m: &ast::StaticMemberExpression) {
        let name = m.property.name.as_str();
        let span = m.property.span.start;
        if name == "length" {
            self.emit(Instr::ArrLength, span);
        } else if name == "size" {
            self.emit(Instr::MapSetSize, span);
        } else {
            self.emit(Instr::ObjGet(name.into()), span);
        }
    }

    /// `obj[expr]` — runtime-polymorphic computed access via `IndexGet`.
    pub(super) fn compile_computed_member(&mut self, m: &ast::ComputedMemberExpression) {
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
    pub(super) fn begin_optional(&mut self, span: u32) -> u32 {
        let cont = self.new_label();
        let end = self.new_label();
        self.emit(Instr::JNotNullish(cont), span);
        self.emit(Instr::PushUndefined, span);
        self.emit(Instr::Jump(end), span);
        self.emit(Instr::Label(cont), span);
        end
    }

    pub(super) fn compile_chain_element(&mut self, el: &ast::ChainElement) {
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
}
