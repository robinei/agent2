use oxc_ast::ast;
use oxc_span::GetSpan;

use crate::builtin::Builtin;
use crate::vm::{Instr, LocalIndex, RcStr, SetMode};

/// One instance method to install on `C.prototype`.
struct Method<'a> {
    name: RcStr,
    func: &'a ast::Function<'a>,
    span: u32,
}

impl super::Compiler {
    /// `class C { … }` declaration (Step 7a — plain class, no `extends`). Direct
    /// codegen: emit the same bytecode the hand-written `function C(){…};
    /// C.prototype.m = …` form compiles to, then store `C` into its binding slot.
    pub(super) fn compile_class_decl(&mut self, class: &ast::Class) {
        // The class value (constructor closure, with methods installed on its
        // prototype) is left on the stack; store it into the name's slot.
        if !self.compile_class_value(class) {
            return;
        }
        if let Some(id) = &class.id
            && let Some(slot) = self.binding_slot(id.span.start)
        {
            self.emit(Instr::SetLocal(slot as LocalIndex), class.span.start);
            return;
        }
        // No binding (shouldn't happen for a declaration) — discard the value to
        // keep the stack balanced.
        self.emit(Instr::Pop(1), class.span.start);
    }

    /// `const X = class { … }` / `(class { … })` expression form: leaves the
    /// constructor closure value on the stack.
    pub(super) fn compile_class_expr(&mut self, class: &ast::Class, span: u32) {
        if !self.compile_class_value(class) {
            // Keep the stack balanced for the surrounding expression even on a
            // rejected class (the diagnostic already failed the compile).
            self.emit(Instr::PushUndefined, span);
        }
    }

    /// Emit the constructor closure value with its prototype methods installed,
    /// leaving the value on the stack. Returns `false` (after recording a
    /// diagnostic) if the class uses an unsupported feature, in which case
    /// nothing is left on the stack.
    fn compile_class_value(&mut self, class: &ast::Class) -> bool {
        let span = class.span.start;
        // `extends <ident>` (Step 7b): the superclass must be a plain identifier
        // resolvable to a constructor value (the MVP rejects an expression
        // superclass, matching the static-callee restriction on `new`).
        let super_class: Option<&ast::Expression> = match &class.super_class {
            Some(ast::Expression::Identifier(_)) => class.super_class.as_ref(),
            Some(other) => {
                self.error(
                    other.span().start,
                    "`extends` requires a class/constructor name (an expression \
                     superclass is not supported)",
                );
                return false;
            }
            None => None,
        };
        let is_derived = super_class.is_some();
        if !class.decorators.is_empty() {
            self.error(span, "class decorators are not supported");
            return false;
        }

        // Partition the class body into the (optional) constructor, instance
        // fields, and instance methods. Rejected element kinds (static members,
        // getters/setters, computed/private keys, static blocks, …) error here.
        let mut ctor: Option<&ast::Function> = None;
        let mut fields: Vec<(RcStr, Option<&ast::Expression>)> = Vec::new();
        let mut methods: Vec<Method> = Vec::new();
        for el in &class.body.body {
            match el {
                ast::ClassElement::MethodDefinition(m) => {
                    if m.r#static {
                        self.error(m.span.start, "`static` class members are not supported");
                        return false;
                    }
                    match m.kind {
                        ast::MethodDefinitionKind::Constructor => ctor = Some(&m.value),
                        ast::MethodDefinitionKind::Method => {
                            let Some(name) = self.class_key_name(&m.key, m.computed) else {
                                return false;
                            };
                            methods.push(Method {
                                name,
                                func: &m.value,
                                span: m.value.span.start,
                            });
                        }
                        ast::MethodDefinitionKind::Get | ast::MethodDefinitionKind::Set => {
                            self.error(
                                m.span.start,
                                "getters/setters are not supported (use a plain method)",
                            );
                            return false;
                        }
                    }
                }
                ast::ClassElement::PropertyDefinition(p) => {
                    if p.r#static {
                        self.error(p.span.start, "`static` class fields are not supported");
                        return false;
                    }
                    let Some(name) = self.class_key_name(&p.key, p.computed) else {
                        return false;
                    };
                    fields.push((name, p.value.as_ref()));
                }
                other => {
                    self.error(
                        other.span().start,
                        "unsupported class element (static blocks, accessors, and index \
                         signatures are not supported)",
                    );
                    return false;
                }
            }
        }

        // A derived class needs an explicit `constructor` that calls `super(...)`
        // (the MVP does not synthesize a default forwarding constructor — `super`
        // binding requires a real constructor scope to capture the parent).
        if is_derived && ctor.is_none() {
            self.error(
                span,
                "a `class` with `extends` must declare a `constructor` that calls \
                 `super(...)` (a default constructor is not synthesized)",
            );
            return false;
        }

        // ── the constructor becomes `C` ──────────────────────────────────────
        // The constructor scope is the explicit `constructor` method's scope, or
        // (for a default constructor) a synthetic scope the analyzer keyed by the
        // class node's span. Field initializers are prepended to its body — or,
        // for a derived class, emitted right after `super(...)` returns
        // (`defer_fields_after_super`), so they observe parent-set values.
        let ctor_span = ctor.map_or(span, |f| f.span.start);
        let Some(ctor_scope) = self.scope_for_node(ctor_span) else {
            self.error(
                span,
                "internal error: class constructor not found in analysis",
            );
            return false;
        };
        self.emit_closure_value(ctor_scope, span);
        match ctor {
            Some(func) => {
                let body = func.body.as_ref().map(|b| &b.statements[..]).unwrap_or(&[]);
                self.emit_function_def(
                    ctor_scope,
                    body,
                    Some(&func.params),
                    &fields,
                    span,
                    false,
                    is_derived,
                );
            }
            None => {
                self.emit_function_def(ctor_scope, &[], None, &fields, span, false, false);
            }
        }

        // ── install instance methods on `C.prototype` ────────────────────────
        if !methods.is_empty() {
            self.emit(Instr::Pick(0), span); // dup C
            self.emit(Instr::ObjGet(RcStr::from("prototype")), span); // C.prototype (lazy-alloc)
            for m in &methods {
                let Some(m_scope) = self.scope_for_node(m.span) else {
                    self.error(m.span, "internal error: class method not found in analysis");
                    return false;
                };
                self.emit(Instr::Pick(0), m.span); // dup prototype
                self.emit_closure_value(m_scope, m.span);
                let body = m
                    .func
                    .body
                    .as_ref()
                    .map(|b| &b.statements[..])
                    .unwrap_or(&[]);
                self.emit_function_def(
                    m_scope,
                    body,
                    Some(&m.func.params),
                    &[],
                    m.span,
                    false,
                    false,
                );
                // ObjSet leaves the value; discard it, keep the prototype.
                self.emit(Instr::ObjSet(m.name.clone(), SetMode::New), m.span);
                self.emit(Instr::Pop(1), m.span);
            }
            self.emit(Instr::Pop(1), span); // drop the prototype, leaving C
        }

        // ── `extends`: link `C.prototype`'s [[Prototype]] to `Parent.prototype`
        // so instances inherit parent methods via the Step-2 chain walk. Reuses
        // the Step-8 `Object.setPrototypeOf` primitive (which rejects cycles).
        if let Some(sc) = super_class {
            self.emit(Instr::Pick(0), span); // dup C
            self.emit(Instr::ObjGet(RcStr::from("prototype")), span); // C.prototype
            self.compile_expr(sc); // Parent
            self.emit(Instr::ObjGet(RcStr::from("prototype")), span); // Parent.prototype
            // setPrototypeOf(C.prototype, Parent.prototype) → returns C.prototype.
            self.emit(Instr::CallBuiltin(Builtin::ObjSetProtoOf, 2), span);
            self.emit(Instr::Pop(1), span); // drop the returned C.prototype, leaving C
        }
        true
    }

    /// Extract a class member's property name. Computed (`[expr]`) and private
    /// (`#x`) keys are rejected in the MVP.
    fn class_key_name(&mut self, key: &ast::PropertyKey, computed: bool) -> Option<RcStr> {
        if computed {
            self.error(
                key.span().start,
                "computed class member names (`[expr]`) are not supported",
            );
            return None;
        }
        match key {
            ast::PropertyKey::StaticIdentifier(id) => Some(RcStr::from(id.name.as_str())),
            ast::PropertyKey::StringLiteral(s) => Some(RcStr::from(s.value.as_str())),
            ast::PropertyKey::NumericLiteral(n) => {
                Some(RcStr::from(super::number_key_to_string(n.value).as_str()))
            }
            ast::PropertyKey::PrivateIdentifier(_) => {
                self.error(
                    key.span().start,
                    "private class members (`#name`) are not supported",
                );
                None
            }
            _ => {
                self.error(key.span().start, "unsupported class member name");
                None
            }
        }
    }
}
