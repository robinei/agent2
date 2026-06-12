use oxc_ast::ast;
use oxc_span::GetSpan;

use crate::vm::{Instr, RcStr, SetMode};

impl<'src> super::Compiler<'src> {
    pub(super) fn compile_array(&mut self, arr: &ast::ArrayExpression) {
        let span = arr.span.start;
        // Fast path: no spread elements (byte-for-byte unchanged from before)
        let has_spread = arr
            .elements
            .iter()
            .any(|el| matches!(el, ast::ArrayExpressionElement::SpreadElement(_)));
        if !has_spread {
            let mut n = 0u32;
            for el in &arr.elements {
                match el.as_expression() {
                    Some(e) => {
                        self.compile_expr(e);
                        n += 1;
                    }
                    None => {
                        self.error(el.span().start, "array holes are not supported");
                        return;
                    }
                }
            }
            self.emit(Instr::ArrNew(n), span);
            return;
        }

        // Slow path: incremental building with spread elements.
        // Start with ArrNew for the leading static segment (possibly empty).
        let mut leading = 0u32;
        for el in &arr.elements {
            match el {
                ast::ArrayExpressionElement::SpreadElement(_) => break,
                _ => {
                    if let Some(e) = el.as_expression() {
                        self.compile_expr(e);
                        leading += 1;
                    } else {
                        self.error(el.span().start, "array holes are not supported");
                        return;
                    }
                }
            }
        }
        self.emit(Instr::ArrNew(leading), span);

        // Remaining elements: alternate spreads and single-element pushes.
        for el in &arr.elements[leading as usize..] {
            match el {
                ast::ArrayExpressionElement::SpreadElement(s) => {
                    self.compile_expr(&s.argument);
                    self.emit(Instr::ArrExtend, span);
                }
                _ => {
                    if let Some(e) = el.as_expression() {
                        self.compile_expr(e);
                        self.emit(Instr::ArrPush, span);
                    } else {
                        self.error(el.span().start, "array holes are not supported");
                        return;
                    }
                }
            }
        }
    }

    /// Validate a static (non-spread) object literal property and extract its
    /// field name. Reports a compile error and returns `None` for getters/
    /// setters, methods, and unsupported key forms (computed keys return
    /// `None` without error — the caller falls through to the IndexSet path).
    pub(super) fn static_property_name(&mut self, p: &ast::ObjectProperty) -> Option<RcStr> {
        if p.kind != ast::PropertyKind::Init {
            self.error(p.span.start, "getters/setters are not supported");
            return None;
        }
        // Method shorthand `{ run(x) { … } }` is not rejected — the value is a
        // FunctionExpression compiled via the normal function-expression path.
        // `this` inside the method body still errors with its existing message.
        if p.computed {
            return None;
        }
        match &p.key {
            ast::PropertyKey::StaticIdentifier(id) => Some(RcStr::from(id.name.as_str())),
            ast::PropertyKey::StringLiteral(s) => Some(RcStr::from(s.value.as_str())),
            ast::PropertyKey::NumericLiteral(num) => {
                Some(RcStr::from(super::number_key_to_string(num.value).as_str()))
            }
            _ => {
                self.error(p.key.span().start, "unsupported object key");
                None
            }
        }
    }

    pub(super) fn compile_object(&mut self, obj: &ast::ObjectExpression) {
        let span = obj.span.start;
        // Fast path: no spread, no computed keys (byte-for-byte unchanged)
        let needs_slow = obj.properties.iter().any(|prop| {
            matches!(prop, ast::ObjectPropertyKind::SpreadProperty(_))
                || matches!(prop, ast::ObjectPropertyKind::ObjectProperty(p) if p.computed)
        });
        if !needs_slow {
            let mut names: Vec<RcStr> = Vec::with_capacity(obj.properties.len());
            for prop in &obj.properties {
                let p = match prop {
                    ast::ObjectPropertyKind::ObjectProperty(p) => p,
                    ast::ObjectPropertyKind::SpreadProperty(_) => unreachable!(),
                };
                let Some(name) = self.static_property_name(p) else {
                    return;
                };
                self.compile_expr(&p.value);
                names.push(name);
            }
            self.emit(Instr::ObjNew(names.into()), span);
            return;
        }

        // Slow path: incremental building with spreads and/or computed keys.
        // Phase 1: emit ObjNew for the leading static segment (stop at first
        // spread or computed key).
        let leading_count = obj
            .properties
            .iter()
            .take_while(|prop| match prop {
                ast::ObjectPropertyKind::SpreadProperty(_) => false,
                ast::ObjectPropertyKind::ObjectProperty(p) => !p.computed,
            })
            .count();

        let mut leading_names: Vec<RcStr> = Vec::with_capacity(leading_count);
        for prop in &obj.properties[..leading_count] {
            let p = match prop {
                ast::ObjectPropertyKind::ObjectProperty(p) => p,
                ast::ObjectPropertyKind::SpreadProperty(_) => unreachable!(),
            };
            let Some(name) = self.static_property_name(p) else {
                return;
            };
            self.compile_expr(&p.value);
            leading_names.push(name);
        }
        self.emit(Instr::ObjNew(leading_names.into()), span);

        // Phase 2: remaining properties — spreads, computed keys, and static
        // fields after a spread/computed key.  For a static field we use
        // Pick(0) + ObjSet + Pop(1) to keep the object on the stack; for a
        // computed key we use Pick(0) + compile key + compile value +
        // IndexSet(New) + Pop(1) (IndexSet pops container, key, val).
        for prop in &obj.properties[leading_count..] {
            match prop {
                ast::ObjectPropertyKind::SpreadProperty(s) => {
                    self.compile_expr(&s.argument);
                    self.emit(Instr::ObjExtend, span);
                }
                ast::ObjectPropertyKind::ObjectProperty(p) => {
                    if p.computed {
                        self.emit(Instr::Pick(0), span);
                        self.compile_expr(
                            p.key
                                .as_expression()
                                .expect("computed key must have expression"),
                        );
                        self.compile_expr(&p.value);
                        self.emit(Instr::IndexSet(SetMode::New), span);
                        self.emit(Instr::Pop(1), span);
                    } else {
                        let Some(name) = self.static_property_name(p) else {
                            return;
                        };
                        self.emit(Instr::Pick(0), span);
                        self.compile_expr(&p.value);
                        self.emit(Instr::ObjSet(name, SetMode::New), span);
                        self.emit(Instr::Pop(1), span);
                    }
                }
            }
        }
    }

    pub(super) fn compile_template(&mut self, tl: &ast::TemplateLiteral) {
        let span = tl.span.start;
        // result = quasi0 + expr0 + quasi1 + expr1 + … . The accumulator starts
        // as a string (interned constant) and stays one, so every `Add` takes
        // the concat path and ToString-coerces each interpolated value, as JS does.
        let quasi_str = |q: &ast::TemplateElement| {
            q.value
                .cooked
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or_else(|| q.value.raw.as_str())
                .to_string()
        };
        let q0 = self.intern_string(quasi_str(&tl.quasis[0]).as_str());
        self.emit(Instr::PushStr(q0), span);
        for (i, expr) in tl.expressions.iter().enumerate() {
            self.compile_expr(expr);
            self.emit(Instr::Add, span);
            let qn = self.intern_string(quasi_str(&tl.quasis[i + 1]).as_str());
            self.emit(Instr::PushStr(qn), span);
            self.emit(Instr::Add, span);
        }
    }
}
