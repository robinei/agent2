use oxc_ast::ast;

use crate::vm::instr::TypeTag;
use crate::vm::{Instr, Value};

impl super::Compiler {
    pub(super) fn compile_binary(&mut self, bin: &ast::BinaryExpression) {
        use ast::BinaryOperator as Op;
        let span = bin.span.start;

        // `key in obj` lowers to ObjHas, which pops the (string) key then the
        // object. Evaluate left (key) then right (obj) to keep JS eval order,
        // then Dig(1) (formerly Swap) into [obj, key]; ToStr coerces the key as JS `in` does.
        if bin.operator == Op::In {
            self.compile_expr(&bin.left);
            self.emit(Instr::ToStr, span);
            self.compile_expr(&bin.right);
            self.emit(Instr::Dig(1), span);
            self.emit(Instr::ObjHas, span);
            return;
        }

        if bin.operator == Op::Instanceof {
            self.compile_instanceof(&bin.left, &bin.right, span);
            return;
        }

        // Evaluate operands left-to-right; the op pops rhs then lhs.
        self.compile_expr(&bin.left);
        self.compile_expr(&bin.right);
        let instr = match bin.operator {
            Op::Addition => Instr::Add,
            Op::Subtraction => Instr::Sub,
            Op::Multiplication => Instr::Mul,
            Op::Division => Instr::Div,
            Op::Remainder => Instr::Mod,
            Op::Exponential => Instr::Pow,
            Op::Equality => Instr::LooseEq,
            Op::Inequality => Instr::LooseNeq,
            Op::StrictEquality => Instr::Eq,
            Op::StrictInequality => Instr::Neq,
            Op::LessThan => Instr::Lt,
            Op::LessEqualThan => Instr::LtEq,
            Op::GreaterThan => Instr::Gt,
            Op::GreaterEqualThan => Instr::GtEq,
            Op::BitwiseAnd => Instr::BitAnd,
            Op::BitwiseOR => Instr::BitOr,
            Op::BitwiseXOR => Instr::BitXor,
            Op::ShiftLeft => Instr::BitLhs,
            Op::ShiftRight => Instr::BitRhs,
            Op::ShiftRightZeroFill => Instr::BitURhs,
            Op::In => unreachable!("`in` handled above"),
            Op::Instanceof => unreachable!("`instanceof` handled above"),
        };
        self.emit(instr, span);
    }

    /// `x instanceof RHS`. The compiler picks one of two lowerings by the RHS:
    /// - **Builtin type**: RHS is an *undeclared* identifier naming a known
    ///   builtin constructor (`Array`, `Object`, `Map`, `Set`, `RegExp`,
    ///   `Function`). Emit `TypeCheck(tag)` for a structural value-tag check.
    ///   A declared local shadowing the same name takes the user-callable path.
    /// - **User callable**: evaluate the RHS and emit `InstanceOf`, which walks
    ///   the prototype chain at runtime.
    pub(super) fn compile_instanceof(
        &mut self,
        lhs: &ast::Expression,
        rhs: &ast::Expression,
        span: u32,
    ) {
        self.compile_expr(lhs);
        // Check for builtin-type fast path: RHS is an undeclared identifier
        // naming a known builtin type.
        if let ast::Expression::Identifier(id) = rhs {
            let ref_span = id.span.start;
            if self.ref_slot(ref_span).is_none() && !crate::is_host_const(id.name.as_str()) {
                let tag = match id.name.as_str() {
                    "Array" => Some(TypeTag::Array),
                    "Object" => Some(TypeTag::Object),
                    "Map" => Some(TypeTag::Map),
                    "Set" => Some(TypeTag::Set),
                    "RegExp" => Some(TypeTag::RegExp),
                    "Function" => Some(TypeTag::Function),
                    _ => None,
                };
                if let Some(tag) = tag {
                    self.emit(Instr::TypeCheck(tag), span);
                    return;
                }
            }
        }
        // User-callable path: evaluate RHS and emit InstanceOf.
        self.compile_expr(rhs);
        self.emit(Instr::InstanceOf, span);
    }

    pub(super) fn compile_unary(&mut self, un: &ast::UnaryExpression) {
        use ast::UnaryOperator as Op;
        let span = un.span.start;
        match un.operator {
            Op::UnaryNegation => {
                // Fold `-<numeric literal>` to a canonical NegInt/Number at
                // compile time; otherwise `Neg` promotes to Number(-x).
                if let ast::Expression::NumericLiteral(lit) = &un.argument {
                    match super::f64_to_value(-lit.value) {
                        Value::PosInt(v) => self.emit(Instr::PushPosInt(v), span),
                        Value::NegInt(v) => self.emit(Instr::PushNegInt(v), span),
                        Value::Float(v) => self.emit(Instr::PushFloat(v), span),
                        _ => unreachable!(),
                    }
                } else {
                    self.compile_expr(&un.argument);
                    self.emit(Instr::Neg, span);
                }
            }
            Op::UnaryPlus => {
                self.compile_expr(&un.argument);
                self.emit(Instr::ToNum, span);
            }
            Op::LogicalNot => {
                self.compile_expr(&un.argument);
                self.emit(Instr::Not, span);
            }
            Op::BitwiseNot => {
                self.compile_expr(&un.argument);
                self.emit(Instr::BitNot, span);
            }
            Op::Typeof => {
                self.compile_expr(&un.argument);
                self.emit(Instr::TypeOf, span);
            }
            Op::Void => {
                self.compile_expr(&un.argument);
                self.emit(Instr::Pop(1), span);
                self.emit(Instr::PushUndefined, span);
            }
            Op::Delete => self.compile_delete(&un.argument, span),
        }
    }

    /// `delete obj.foo` / `delete obj[k]` lower to `ObjDelete` (which pops the
    /// string key then the object and pushes whether it existed). A non-property
    /// delete is an error.
    pub(super) fn compile_delete(&mut self, arg: &ast::Expression, span: u32) {
        match arg {
            ast::Expression::StaticMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.emit(
                    Instr::PushStr(m.property.name.as_str().into()),
                    m.property.span.start,
                );
                self.emit(Instr::ObjDelete, span);
            }
            ast::Expression::ComputedMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.compile_expr(&m.expression);
                self.emit(Instr::ToStr, span); // coerce the key to a string
                self.emit(Instr::ObjDelete, span);
            }
            ast::Expression::ChainExpression(c) => {
                // `delete a?.b` — compile the chained member, then delete.
                self.compile_delete_chain(&c.expression, span)
            }
            _ => self.error(span, "`delete` is only supported on object properties"),
        }
    }

    pub(super) fn compile_delete_chain(&mut self, el: &ast::ChainElement, span: u32) {
        match el {
            ast::ChainElement::StaticMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.emit(
                    Instr::PushStr(m.property.name.as_str().into()),
                    m.property.span.start,
                );
                self.emit(Instr::ObjDelete, span);
            }
            ast::ChainElement::ComputedMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.compile_expr(&m.expression);
                self.emit(Instr::ToStr, span);
                self.emit(Instr::ObjDelete, span);
            }
            _ => self.error(span, "`delete` is only supported on object properties"),
        }
    }

    /// Short-circuit `&&` / `||` / `??`, branch-compiled (NOT the `And`/`Or`
    /// instructions, which evaluate both operands and so cannot short-circuit).
    pub(super) fn compile_logical(&mut self, log: &ast::LogicalExpression) {
        use ast::LogicalOperator as Op;
        let span = log.span.start;
        self.compile_expr(&log.left);
        match log.operator {
            Op::And => {
                // truthy: drop lhs, eval rhs; falsy: keep lhs.
                let end = self.new_label();
                self.emit(Instr::Pick(0), span);
                self.emit(Instr::JFalse(end), span);
                self.emit(Instr::Pop(1), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Label(end), span);
            }
            Op::Or => {
                // truthy: keep lhs; falsy: drop lhs, eval rhs.
                let end = self.new_label();
                self.emit(Instr::Pick(0), span);
                self.emit(Instr::JTrue(end), span);
                self.emit(Instr::Pop(1), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Label(end), span);
            }
            Op::Coalesce => {
                // not nullish: keep lhs (the taken jump leaves it); nullish:
                // the fall-through pops the lhs, then evaluate rhs.
                let end = self.new_label();
                self.emit(Instr::JNotNullish(end), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Label(end), span);
            }
        }
    }

    pub(super) fn compile_conditional(&mut self, cond: &ast::ConditionalExpression) {
        let span = cond.span.start;
        let els = self.new_label();
        let end = self.new_label();
        self.compile_expr(&cond.test);
        self.emit(Instr::JFalse(els), span);
        self.compile_expr(&cond.consequent);
        self.emit(Instr::Jump(end), span);
        self.emit(Instr::Label(els), span);
        self.compile_expr(&cond.alternate);
        self.emit(Instr::Label(end), span);
    }
}
