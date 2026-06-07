//! Compiler — JS source → VM instructions (`vm::Instr`).
//!
//! Compiles a subset of JS into the stack VM in `vm.rs`. Parses with
//! `oxc_parser`, traverses the AST, lowers supported constructs, and emits
//! informative `Diagnostic`s for the rest. See `COMPILER_PLAN.md` for the full
//! design; this file is the Phase 0 skeleton: parsing, the `Compiler` /
//! `Program` / `Diagnostic` types, a label allocator, the backpatch pass, the
//! span table, and codegen for literal + arithmetic expressions.

use std::sync::Arc;

use oxc_allocator::Allocator;
use oxc_ast::ast;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

use crate::vm::{Instr, StackValue};

/// A compiled program: the flat instruction stream, a parallel span table
/// (`spans[ip]` = source byte offset of the instruction at `ip`), and the
/// source it was compiled from (for rendering runtime diagnostics).
#[derive(Debug)]
pub struct Program {
    pub code: Vec<Instr>,
    pub spans: Vec<u32>,
    pub source: Arc<str>,
}

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

/// Compile JS source into a `Program`. Collects every diagnostic (oxc syntax
/// errors plus our own semantic errors) and returns them all if any exist,
/// rather than producing a partial program.
pub fn compile(source: &str) -> Result<Program, Vec<Diagnostic>> {
    let allocator = Allocator::default();
    let source_type = SourceType::default(); // JavaScript module
    let ret = Parser::new(&allocator, source, source_type).parse();

    let mut compiler = Compiler::new(source);

    // Convert oxc's own syntax errors into our Diagnostic shape.
    for err in &ret.errors {
        let span = err
            .labels
            .as_ref()
            .and_then(|labels| labels.first())
            .map(|l| l.offset() as u32)
            .unwrap_or(0);
        compiler.error(span, err.message.to_string());
    }

    compiler.compile_program(&ret.program);

    if !compiler.diagnostics.is_empty() {
        return Err(compiler.diagnostics);
    }

    let (code, spans) = backpatch(compiler.code, compiler.spans, compiler.next_label);
    Ok(Program {
        code,
        spans,
        source: Arc::from(source),
    })
}

/// Codegen state for one compilation unit.
struct Compiler<'src> {
    source: &'src str,
    /// Instructions with `Label` markers; addresses in `Jump`/`JFalse`/`Call`/
    /// `MakeClosure`/`Push(Fn)` are label ids until the backpatch pass.
    code: Vec<Instr>,
    /// `spans[i]` = source byte offset of `code[i]`; kept in lockstep.
    spans: Vec<u32>,
    /// Monotonic label-id allocator.
    next_label: u32,
    diagnostics: Vec<Diagnostic>,
}

impl<'src> Compiler<'src> {
    fn new(source: &'src str) -> Self {
        Compiler {
            source,
            code: Vec::new(),
            spans: Vec::new(),
            next_label: 0,
            diagnostics: Vec::new(),
        }
    }

    /// Allocate a fresh label id.
    fn new_label(&mut self) -> u32 {
        let id = self.next_label;
        self.next_label += 1;
        id
    }

    /// Append an instruction with its source span (byte offset).
    fn emit(&mut self, instr: Instr, span: u32) {
        self.code.push(instr);
        self.spans.push(span);
    }

    /// Record a diagnostic; aborts the compile before a `Program` is produced.
    fn error(&mut self, span: u32, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            message: message.into(),
        });
    }

    // ── codegen ──────────────────────────────────────────────────────

    /// The whole program is the root frame's body: lower each top-level
    /// statement, then `Return(0)` to pop the root frame (→ `StepResult::Done`)
    /// and stop execution falling into any appended function bodies.
    fn compile_program(&mut self, program: &ast::Program) {
        for stmt in &program.body {
            self.compile_stmt(stmt);
        }
        self.emit(Instr::Return(0), program.span.end);
    }

    fn compile_stmt(&mut self, stmt: &ast::Statement) {
        match stmt {
            // Every expression statement leaves one value, popped to keep the
            // stack-discipline invariant (one value per expression).
            ast::Statement::ExpressionStatement(es) => {
                self.compile_expr(&es.expression);
                self.emit(Instr::Pop(1), es.span.start);
            }
            other => self.error(other.span().start, "unsupported statement"),
        }
    }

    /// Every expression leaves exactly one value on the stack (the
    /// stack-discipline invariant). Unsupported nodes record a diagnostic and
    /// emit nothing — the diagnostics abort the compile before a `Program` is
    /// produced, so the missing value never matters.
    fn compile_expr(&mut self, expr: &ast::Expression) {
        match expr {
            // ── literals ──────────────────────────────────────────────
            ast::Expression::NumericLiteral(lit) => {
                self.emit(Instr::Push(number_literal_to_value(lit.value)), lit.span.start);
            }
            ast::Expression::StringLiteral(lit) => {
                self.emit(Instr::PushStr(lit.value.as_str().to_string()), lit.span.start);
            }
            ast::Expression::BooleanLiteral(lit) => {
                self.emit(Instr::Push(StackValue::Bool(lit.value)), lit.span.start);
            }
            ast::Expression::NullLiteral(lit) => {
                self.emit(Instr::Push(StackValue::Null), lit.span.start);
            }
            ast::Expression::TemplateLiteral(tl) => self.compile_template(tl),

            // ── identifiers ───────────────────────────────────────────
            ast::Expression::Identifier(id) => {
                self.compile_identifier(id.name.as_str(), id.span.start)
            }

            // ── composite literals ────────────────────────────────────
            ast::Expression::ArrayExpression(arr) => self.compile_array(arr),
            ast::Expression::ObjectExpression(obj) => self.compile_object(obj),

            // ── operators ─────────────────────────────────────────────
            ast::Expression::BinaryExpression(bin) => self.compile_binary(bin),
            ast::Expression::UnaryExpression(un) => self.compile_unary(un),
            ast::Expression::LogicalExpression(log) => self.compile_logical(log),
            ast::Expression::ConditionalExpression(cond) => self.compile_conditional(cond),
            ast::Expression::AssignmentExpression(a) => self.compile_assignment(a),
            ast::Expression::SequenceExpression(seq) => {
                // The comma operator: evaluate each, discard all but the last.
                let last = seq.expressions.len().saturating_sub(1);
                for (i, e) in seq.expressions.iter().enumerate() {
                    self.compile_expr(e);
                    if i != last {
                        self.emit(Instr::Pop(1), e.span().start);
                    }
                }
            }

            // ── member access / calls ─────────────────────────────────
            ast::Expression::StaticMemberExpression(m) => self.compile_static_member(m),
            ast::Expression::ComputedMemberExpression(m) => self.compile_computed_member(m),
            ast::Expression::CallExpression(c) => self.compile_call(c),
            ast::Expression::ChainExpression(chain) => self.compile_chain_element(&chain.expression),

            ast::Expression::ParenthesizedExpression(p) => self.compile_expr(&p.expression),

            // ── informative errors for out-of-scope nodes ─────────────
            ast::Expression::UpdateExpression(u) => {
                self.error(u.span.start, "`++`/`--` are not supported until Phase 2")
            }
            ast::Expression::FunctionExpression(f) => {
                self.error(f.span.start, "function expressions are not supported until Phase 3")
            }
            ast::Expression::ArrowFunctionExpression(f) => {
                self.error(f.span.start, "arrow functions are not supported until Phase 3")
            }
            ast::Expression::BigIntLiteral(b) => self.error(b.span.start, "BigInt is not supported"),
            ast::Expression::RegExpLiteral(r) => {
                self.error(r.span.start, "regular expressions are not supported")
            }
            ast::Expression::ThisExpression(t) => self.error(t.span.start, "`this` is not supported"),
            ast::Expression::NewExpression(n) => self.error(n.span.start, "`new` is not supported"),
            other => self.error(other.span().start, "unsupported expression"),
        }
    }

    /// A bare identifier resolves only to the blessed `state` object or the
    /// global literal-like names. Everything else is an undeclared variable —
    /// a compile error, so typos can't silently become persistent state. (Local
    /// variables arrive in Phase 2/3; namespace names like `Math`/`Object` are
    /// recognized structurally as call/member receivers, never as bare values.)
    fn compile_identifier(&mut self, name: &str, span: u32) {
        let value = match name {
            "state" => StackValue::Ptr(0),
            "undefined" => StackValue::Undefined,
            "NaN" => StackValue::Number(f64::NAN),
            "Infinity" => StackValue::Number(f64::INFINITY),
            _ => {
                self.error(span, format!("undeclared variable `{name}`"));
                return;
            }
        };
        self.emit(Instr::Push(value), span);
    }

    // ── operators ──────────────────────────────────────────────────────

    fn compile_binary(&mut self, bin: &ast::BinaryExpression) {
        use ast::BinaryOperator as Op;
        let span = bin.span.start;

        // `key in obj` lowers to ObjHas, which pops the (string) key then the
        // object. Evaluate left (key) then right (obj) to keep JS eval order,
        // then Swap into [obj, key]; ToStr coerces the key as JS `in` does.
        if bin.operator == Op::In {
            self.compile_expr(&bin.left);
            self.emit(Instr::ToStr, span);
            self.compile_expr(&bin.right);
            self.emit(Instr::Swap, span);
            self.emit(Instr::ObjHas, span);
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
            Op::ShiftRightZeroFill => {
                self.error(span, "unsigned right shift (`>>>`) is not supported");
                return;
            }
            Op::Instanceof => {
                self.error(span, "`instanceof` is not supported");
                return;
            }
            Op::In => unreachable!("`in` handled above"),
        };
        self.emit(instr, span);
    }

    fn compile_unary(&mut self, un: &ast::UnaryExpression) {
        use ast::UnaryOperator as Op;
        let span = un.span.start;
        match un.operator {
            Op::UnaryNegation => {
                // Fold `-<numeric literal>` to a canonical NegInt/Number at
                // compile time; otherwise `Neg` promotes to Number(-x).
                if let ast::Expression::NumericLiteral(lit) = &un.argument {
                    self.emit(Instr::Push(f64_to_value(-lit.value)), span);
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
                self.emit(Instr::Push(StackValue::Undefined), span);
            }
            Op::Delete => self.compile_delete(&un.argument, span),
        }
    }

    /// `delete obj.foo` / `delete obj[k]` lower to `ObjDelete` (which pops the
    /// string key then the object and pushes whether it existed). A non-property
    /// delete is an error.
    fn compile_delete(&mut self, arg: &ast::Expression, span: u32) {
        match arg {
            ast::Expression::StaticMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.emit(
                    Instr::PushStr(m.property.name.as_str().to_string()),
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

    fn compile_delete_chain(&mut self, el: &ast::ChainElement, span: u32) {
        match el {
            ast::ChainElement::StaticMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.emit(
                    Instr::PushStr(m.property.name.as_str().to_string()),
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
    fn compile_logical(&mut self, log: &ast::LogicalExpression) {
        use ast::LogicalOperator as Op;
        let span = log.span.start;
        self.compile_expr(&log.left);
        match log.operator {
            Op::And => {
                // truthy: drop lhs, eval rhs; falsy: keep lhs.
                let end = self.new_label();
                self.emit(Instr::Dup, span);
                self.emit(Instr::JFalse(end), span);
                self.emit(Instr::Pop(1), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Label(end), span);
            }
            Op::Or => {
                // truthy: keep lhs; falsy: drop lhs, eval rhs.
                let end = self.new_label();
                self.emit(Instr::Dup, span);
                self.emit(Instr::JTrue(end), span);
                self.emit(Instr::Pop(1), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Label(end), span);
            }
            Op::Coalesce => {
                // nullish (== null, which is true only for null/undefined): eval
                // rhs; otherwise keep lhs.
                let keep = self.new_label();
                let end = self.new_label();
                self.emit(Instr::Dup, span);
                self.emit(Instr::Push(StackValue::Null), span);
                self.emit(Instr::LooseEq, span);
                self.emit(Instr::JFalse(keep), span);
                self.emit(Instr::Pop(1), span);
                self.compile_expr(&log.right);
                self.emit(Instr::Jump(end), span);
                self.emit(Instr::Label(keep), span);
                self.emit(Instr::Label(end), span);
            }
        }
    }

    fn compile_conditional(&mut self, cond: &ast::ConditionalExpression) {
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

    // ── composite literals ──────────────────────────────────────────────

    fn compile_array(&mut self, arr: &ast::ArrayExpression) {
        let mut n = 0u32;
        for el in &arr.elements {
            match el.as_expression() {
                Some(e) => {
                    self.compile_expr(e);
                    n += 1;
                }
                None => {
                    self.error(
                        el.span().start,
                        "array holes and spread elements are not supported",
                    );
                    return;
                }
            }
        }
        self.emit(Instr::ArrNew(n), arr.span.start);
    }

    fn compile_object(&mut self, obj: &ast::ObjectExpression) {
        let mut names: Vec<String> = Vec::with_capacity(obj.properties.len());
        for prop in &obj.properties {
            let p = match prop {
                ast::ObjectPropertyKind::ObjectProperty(p) => p,
                ast::ObjectPropertyKind::SpreadProperty(s) => {
                    self.error(s.span.start, "object spread is not supported");
                    return;
                }
            };
            if p.kind != ast::PropertyKind::Init {
                self.error(p.span.start, "getters/setters are not supported");
                return;
            }
            if p.method {
                self.error(p.span.start, "object methods are not supported");
                return;
            }
            if p.computed {
                self.error(p.span.start, "computed object keys are not supported");
                return;
            }
            let name = match &p.key {
                ast::PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
                ast::PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
                ast::PropertyKey::NumericLiteral(num) => number_key_to_string(num.value),
                _ => {
                    self.error(p.key.span().start, "unsupported object key");
                    return;
                }
            };
            // Values are pushed in source order (field 0's value deepest), then
            // ObjNew consumes them against the parallel field-name list.
            self.compile_expr(&p.value);
            names.push(name);
        }
        self.emit(Instr::ObjNew(names), obj.span.start);
    }

    fn compile_template(&mut self, tl: &ast::TemplateLiteral) {
        let span = tl.span.start;
        // result = quasi0 + expr0 + quasi1 + expr1 + … . The accumulator starts
        // as a string (PushStr) and stays one, so every `Add` takes the concat
        // path and ToString-coerces each interpolated value, as JS does.
        let quasi_str = |q: &ast::TemplateElement| {
            q.value
                .cooked
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or_else(|| q.value.raw.as_str())
                .to_string()
        };
        self.emit(Instr::PushStr(quasi_str(&tl.quasis[0])), span);
        for (i, expr) in tl.expressions.iter().enumerate() {
            self.compile_expr(expr);
            self.emit(Instr::Add, span);
            self.emit(Instr::PushStr(quasi_str(&tl.quasis[i + 1])), span);
            self.emit(Instr::Add, span);
        }
    }

    // ── member access ────────────────────────────────────────────────────

    /// `obj.foo` (and `state.foo`, since `state` lowers to `Ptr(0)`). `.length`
    /// is the static intrinsic `ArrLength` (the accepted divergence: an object
    /// property literally named `length` reached via `.length`); anything else
    /// is `ObjGet`.
    fn compile_static_member(&mut self, m: &ast::StaticMemberExpression) {
        self.compile_expr(&m.object);
        if m.optional {
            let end = self.begin_optional(m.span.start);
            self.emit_static_access(m);
            self.emit(Instr::Label(end), m.span.start);
        } else {
            self.emit_static_access(m);
        }
    }

    fn emit_static_access(&mut self, m: &ast::StaticMemberExpression) {
        let name = m.property.name.as_str();
        let span = m.property.span.start;
        if name == "length" {
            self.emit(Instr::ArrLength, span);
        } else {
            self.emit(Instr::ObjGet(name.to_string()), span);
        }
    }

    /// `obj[expr]` — runtime-polymorphic computed access via `IndexGet`.
    fn compile_computed_member(&mut self, m: &ast::ComputedMemberExpression) {
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

    /// Optional-chaining (`?.`) prologue. With the object value already on the
    /// stack, short-circuit to `undefined` when it is nullish (`== null`, i.e.
    /// null or undefined); otherwise leave the object for the access that the
    /// caller emits next. Returns the `end` label to place after that access.
    ///
    /// Per-link: a fully-`?.` chain (`a?.b?.c`) short-circuits correctly because
    /// each link re-checks; mixing `?.` then a plain `.` on a nullish base
    /// (`a?.b.c`) is an accepted divergence (runtime TypeError, not `undefined`).
    fn begin_optional(&mut self, span: u32) -> u32 {
        let cont = self.new_label();
        let end = self.new_label();
        self.emit(Instr::Dup, span);
        self.emit(Instr::Push(StackValue::Null), span);
        self.emit(Instr::LooseEq, span);
        self.emit(Instr::JFalse(cont), span);
        self.emit(Instr::Pop(1), span);
        self.emit(Instr::Push(StackValue::Undefined), span);
        self.emit(Instr::Jump(end), span);
        self.emit(Instr::Label(cont), span);
        end
    }

    fn compile_chain_element(&mut self, el: &ast::ChainElement) {
        match el {
            ast::ChainElement::CallExpression(c) => self.compile_call(c),
            ast::ChainElement::StaticMemberExpression(m) => self.compile_static_member(m),
            ast::ChainElement::ComputedMemberExpression(m) => self.compile_computed_member(m),
            ast::ChainElement::PrivateFieldExpression(m) => {
                self.error(m.span.start, "private fields are not supported")
            }
            ast::ChainElement::TSNonNullExpression(e) => {
                self.error(e.span.start, "TypeScript non-null assertions are not supported")
            }
        }
    }

    // ── assignment ───────────────────────────────────────────────────────

    /// Assignment is an expression: it leaves the assigned value on the stack
    /// (the store instructions push it back), so lowering is just the operands
    /// in source order followed by the store — no shuffling, correct eval order.
    ///
    /// Phase 1 supports plain `=` to a member/index/`state` target. Compound
    /// (`+=`), logical (`??=`), and identifier targets (which need local
    /// declarations) arrive in Phase 2.
    fn compile_assignment(&mut self, a: &ast::AssignmentExpression) {
        use ast::AssignmentOperator as Op;
        let span = a.span.start;
        if a.operator != Op::Assign {
            self.error(
                span,
                "compound and logical assignment are not supported until Phase 2",
            );
            return;
        }
        match &a.left {
            ast::AssignmentTarget::StaticMemberExpression(m) => {
                let field = m.property.name.as_str().to_string();
                self.compile_expr(&m.object);
                self.compile_expr(&a.right);
                self.emit(Instr::ObjSet(field), span); // leaves the value
            }
            ast::AssignmentTarget::ComputedMemberExpression(m) => {
                self.compile_expr(&m.object);
                self.compile_expr(&m.expression);
                self.compile_expr(&a.right);
                self.emit(Instr::IndexSet, span); // leaves the value
            }
            ast::AssignmentTarget::AssignmentTargetIdentifier(id) => self.error(
                id.span.start,
                format!(
                    "cannot assign to `{}`: variables are not declarable until Phase 2 \
                     (and `state` cannot be rebound)",
                    id.name.as_str()
                ),
            ),
            other => self.error(other.span().start, "unsupported assignment target"),
        }
    }

    // ── calls / intrinsics ───────────────────────────────────────────────

    /// Calls are recognized structurally (the VM has no method objects): a
    /// `namespace.method(...)` static intrinsic, a `recv.method(...)` array/
    /// string method, or a global function like `String(x)`. Each lowers to a
    /// dedicated instruction; user functions, `tools.*`, and `raise` arrive in
    /// later phases.
    fn compile_call(&mut self, call: &ast::CallExpression) {
        let span = call.span.start;
        if call.optional {
            self.error(span, "optional calls (`?.()`) are not supported");
            return;
        }
        // Collect non-spread argument expressions (spread clashes with the VM's
        // strict arity).
        let mut argv: Vec<&ast::Expression> = Vec::with_capacity(call.arguments.len());
        for arg in &call.arguments {
            match arg.as_expression() {
                Some(e) => argv.push(e),
                None => {
                    self.error(arg.span().start, "spread arguments are not supported");
                    return;
                }
            }
        }

        match &call.callee {
            ast::Expression::StaticMemberExpression(m) => {
                if m.optional {
                    self.error(span, "optional calls (`?.()`) are not supported");
                    return;
                }
                let method = m.property.name.as_str();
                // A leading identifier matching a reserved namespace is a static
                // intrinsic; otherwise it is a method on the receiver value.
                if let ast::Expression::Identifier(obj) = &m.object {
                    match obj.name.as_str() {
                        "Math" => return self.compile_math_call(method, &argv, span),
                        "Object" => return self.compile_object_static_call(method, &argv, span),
                        "JSON" => return self.compile_json_call(method, &argv, span),
                        "Number" => return self.compile_number_static_call(method, &argv, span),
                        "Array" => return self.compile_array_static_call(method, &argv, span),
                        "tools" => {
                            self.error(span, "`tools.*` calls are not supported until Phase 4");
                            return;
                        }
                        _ => {}
                    }
                }
                self.compile_method_call(&m.object, method, &argv, span);
            }
            ast::Expression::ComputedMemberExpression(_) => {
                self.error(span, "computed method calls (`obj[expr](...)`) are not supported")
            }
            ast::Expression::Identifier(id) => self.compile_global_call(id.name.as_str(), &argv, span),
            other => self.error(other.span().start, "unsupported call target"),
        }
    }

    /// Compile all argument expressions left-to-right.
    fn compile_args(&mut self, argv: &[&ast::Expression]) {
        for &e in argv {
            self.compile_expr(e);
        }
    }

    /// Validate an exact arity, recording a diagnostic if it doesn't match.
    fn arity(&mut self, argv: &[&ast::Expression], want: usize, span: u32, name: &str) -> bool {
        if argv.len() == want {
            true
        } else {
            self.error(
                span,
                format!("`{name}` expects {want} argument(s), got {}", argv.len()),
            );
            false
        }
    }

    /// Validate an inclusive arity range.
    fn arity_range(
        &mut self,
        argv: &[&ast::Expression],
        min: usize,
        max: usize,
        span: u32,
        name: &str,
    ) -> bool {
        if (min..=max).contains(&argv.len()) {
            true
        } else {
            self.error(
                span,
                format!(
                    "`{name}` expects {min}..={max} argument(s), got {}",
                    argv.len()
                ),
            );
            false
        }
    }

    fn compile_math_call(&mut self, method: &str, argv: &[&ast::Expression], span: u32) {
        // Binary first (max/min/pow); the rest are unary.
        let binary = match method {
            "max" => Some(Instr::Max),
            "min" => Some(Instr::Min),
            "pow" => Some(Instr::Pow),
            _ => None,
        };
        if let Some(instr) = binary {
            if !self.arity(argv, 2, span, &format!("Math.{method}")) {
                return;
            }
            self.compile_args(argv);
            self.emit(instr, span);
            return;
        }
        let unary = match method {
            "abs" => Instr::Abs,
            "sqrt" => Instr::Sqrt,
            "floor" => Instr::Floor,
            "ceil" => Instr::Ceil,
            "round" => Instr::Round,
            "sign" => Instr::Sign,
            _ => {
                self.error(span, format!("unsupported `Math.{method}`"));
                return;
            }
        };
        if !self.arity(argv, 1, span, &format!("Math.{method}")) {
            return;
        }
        self.compile_args(argv);
        self.emit(unary, span);
    }

    fn compile_object_static_call(&mut self, method: &str, argv: &[&ast::Expression], span: u32) {
        let instr = match method {
            "keys" => Instr::ObjKeys,
            "values" => Instr::ObjValues,
            _ => {
                self.error(span, format!("unsupported `Object.{method}`"));
                return;
            }
        };
        if !self.arity(argv, 1, span, &format!("Object.{method}")) {
            return;
        }
        self.compile_args(argv);
        self.emit(instr, span);
    }

    fn compile_json_call(&mut self, method: &str, argv: &[&ast::Expression], span: u32) {
        let instr = match method {
            "parse" => Instr::StrToJson,
            "stringify" => Instr::StrFromJson,
            _ => {
                self.error(span, format!("unsupported `JSON.{method}`"));
                return;
            }
        };
        // JSON.stringify's indent argument is deferred (single-arg only).
        if !self.arity(argv, 1, span, &format!("JSON.{method}")) {
            return;
        }
        self.compile_args(argv);
        self.emit(instr, span);
    }

    fn compile_number_static_call(&mut self, method: &str, argv: &[&ast::Expression], span: u32) {
        match method {
            "isInteger" => {
                if !self.arity(argv, 1, span, "Number.isInteger") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::IsInt, span);
            }
            _ => self.error(span, format!("unsupported `Number.{method}`")),
        }
    }

    fn compile_array_static_call(&mut self, method: &str, argv: &[&ast::Expression], span: u32) {
        match method {
            "isArray" => {
                if !self.arity(argv, 1, span, "Array.isArray") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::IsArr, span);
            }
            _ => self.error(span, format!("unsupported `Array.{method}`")),
        }
    }

    /// Global function calls recognized structurally.
    fn compile_global_call(&mut self, name: &str, argv: &[&ast::Expression], span: u32) {
        match name {
            "String" => {
                if !self.arity(argv, 1, span, "String") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::ToStr, span);
            }
            "Number" => {
                if !self.arity(argv, 1, span, "Number") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::ToNum, span);
            }
            "Boolean" => {
                if !self.arity(argv, 1, span, "Boolean") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::ToBool, span);
            }
            "raise" => self.error(span, "`raise` is not supported until Phase 4"),
            _ => self.error(
                span,
                format!("call to undeclared function `{name}` (user functions are Phase 3)"),
            ),
        }
    }

    /// Array/string methods on a receiver value. Dispatch is purely syntactic
    /// (name + arity) and assumes the conventional receiver type; a mismatch is
    /// a runtime `TypeError`.
    fn compile_method_call(
        &mut self,
        recv: &ast::Expression,
        method: &str,
        argv: &[&ast::Expression],
        span: u32,
    ) {
        match method {
            // ── array methods ─────────────────────────────────────────
            "push" => {
                if !self.arity(argv, 1, span, "push") {
                    return;
                }
                self.compile_expr(recv);
                self.compile_args(argv);
                self.emit(Instr::ArrPush, span);
                // JS push returns the new length; the VM op yields nothing, so
                // synthesize a result (accepted divergence: `undefined`).
                self.emit(Instr::Push(StackValue::Undefined), span);
            }
            "unshift" => {
                if !self.arity(argv, 1, span, "unshift") {
                    return;
                }
                self.compile_expr(recv);
                self.compile_args(argv);
                self.emit(Instr::ArrUnshift, span);
                self.emit(Instr::Push(StackValue::Undefined), span);
            }
            "pop" => {
                if !self.arity(argv, 0, span, "pop") {
                    return;
                }
                self.compile_expr(recv);
                self.emit(Instr::ArrPop, span);
            }
            "shift" => {
                if !self.arity(argv, 0, span, "shift") {
                    return;
                }
                self.compile_expr(recv);
                self.emit(Instr::ArrShift, span);
            }
            "join" => {
                if !self.arity_range(argv, 0, 1, span, "join") {
                    return;
                }
                self.compile_expr(recv);
                if argv.len() == 1 {
                    self.compile_expr(argv[0]);
                } else {
                    self.emit(Instr::PushStr(",".to_string()), span); // JS default separator
                }
                self.emit(Instr::ArrJoin, span);
            }
            // ── string methods ────────────────────────────────────────
            "split" => self.compile_str_optarg(recv, argv, span, "split", Instr::StrSplit),
            "includes" => self.compile_str_optarg(recv, argv, span, "includes", Instr::StrIncludes),
            "indexOf" => self.compile_str_optarg(recv, argv, span, "indexOf", Instr::StrIndexOf),
            "lastIndexOf" => {
                self.compile_str_optarg(recv, argv, span, "lastIndexOf", Instr::StrLastIndexOf)
            }
            "startsWith" => {
                if !self.arity(argv, 1, span, "startsWith") {
                    return;
                }
                self.compile_expr(recv);
                self.compile_args(argv);
                self.emit(Instr::StrStartsWith, span);
            }
            "endsWith" => {
                if !self.arity(argv, 1, span, "endsWith") {
                    return;
                }
                self.compile_expr(recv);
                self.compile_args(argv);
                self.emit(Instr::StrEndsWith, span);
            }
            "slice" => {
                if !self.arity(argv, 2, span, "slice") {
                    return;
                }
                self.compile_expr(recv);
                self.compile_args(argv);
                self.emit(Instr::StrSlice, span);
            }
            "trim" => {
                if !self.arity(argv, 0, span, "trim") {
                    return;
                }
                self.compile_expr(recv);
                self.emit(Instr::StrTrim, span);
            }
            _ => self.error(span, format!("unsupported method `{method}`")),
        }
    }

    /// Shared lowering for string methods with one required arg and one optional
    /// arg (`split`/`includes`/`indexOf`/`lastIndexOf`). The instruction carries
    /// the optional-arg count (0 or 1), which is `argv.len() - 1`.
    fn compile_str_optarg(
        &mut self,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: u32,
        name: &str,
        make: fn(u32) -> Instr,
    ) {
        if !self.arity_range(argv, 1, 2, span, name) {
            return;
        }
        self.compile_expr(recv);
        self.compile_args(argv);
        self.emit(make((argv.len() - 1) as u32), span);
    }
}

/// Canonicalize a non-negative numeric literal: an integer in `u64` range
/// becomes a `PosInt`, otherwise a `Number`. Literals are non-negative; unary
/// minus is a separate operator folded via `f64_to_value`.
fn number_literal_to_value(value: f64) -> StackValue {
    if value.fract() == 0.0 && value >= 0.0 && value <= u64::MAX as f64 {
        StackValue::PosInt(value as u64)
    } else {
        StackValue::Number(value)
    }
}

/// Canonicalize an arbitrary (possibly negative) f64 into the VM's integer
/// variants when it is integral and in range, mirroring serde_json's split:
/// non-negative → `PosInt`, negative → `NegInt`, otherwise `Number`.
fn f64_to_value(value: f64) -> StackValue {
    if value.fract() == 0.0 && value >= 0.0 && value <= u64::MAX as f64 {
        StackValue::PosInt(value as u64)
    } else if value.fract() == 0.0 && value < 0.0 && value >= i64::MIN as f64 {
        StackValue::NegInt(value as i64)
    } else {
        StackValue::Number(value)
    }
}

/// Render a numeric object-literal key the way JS does (`{1: …}` → key "1",
/// `{1.5: …}` → "1.5"), so it matches the string form computed access produces.
fn number_key_to_string(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

/// Strip `Label` markers and rewrite every label-id address into a real code
/// offset, copying spans in lockstep so the table stays aligned with the
/// compacted code. Single linear pass after a first scan that records each
/// label's offset.
fn backpatch(code: Vec<Instr>, spans: Vec<u32>, next_label: u32) -> (Vec<Instr>, Vec<u32>) {
    // First scan: the offset of each label is the count of non-Label
    // instructions preceding it.
    let mut label_offset = vec![0u32; next_label as usize];
    let mut offset = 0u32;
    for instr in &code {
        match instr {
            Instr::Label(id) => label_offset[*id as usize] = offset,
            _ => offset += 1,
        }
    }

    // Second scan: drop Labels, rewrite addresses (which carry label ids until
    // now), and emit spans in lockstep.
    let mut out_code = Vec::with_capacity(code.len());
    let mut out_spans = Vec::with_capacity(spans.len());
    for (instr, span) in code.into_iter().zip(spans) {
        let rewritten = match instr {
            Instr::Label(_) => continue,
            Instr::Jump(l) => Instr::Jump(label_offset[l as usize]),
            Instr::JFalse(l) => Instr::JFalse(label_offset[l as usize]),
            Instr::JTrue(l) => Instr::JTrue(label_offset[l as usize]),
            Instr::Call(l, n) => Instr::Call(label_offset[l as usize], n),
            Instr::MakeClosure(l, caps) => Instr::MakeClosure(label_offset[l as usize], caps),
            Instr::Push(StackValue::Fn(l)) => Instr::Push(StackValue::Fn(label_offset[l as usize])),
            other => other,
        };
        out_code.push(rewritten);
        out_spans.push(span);
    }
    (out_code, out_spans)
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{StepResult, VM};

    /// Run a compiled program to completion via `for_program`, returning the
    /// finished VM so the heap/stack can be inspected.
    fn run_program(prog: Program) -> VM {
        let mut vm = VM::for_program(prog.code, serde_json::Value::Null).unwrap();
        loop {
            match vm.step().unwrap() {
                StepResult::Done => return vm,
                other => panic!("unexpected effect: {other:?}"),
            }
        }
    }

    #[test]
    fn roundtrip_literal_arithmetic() {
        // `1 + 2 * 3` lowers to: push the three literals, multiply 2*3, add,
        // then the expression-statement Pop and the top-level Return(0).
        let prog = compile("1 + 2 * 3;").expect("compiles");
        assert_eq!(
            prog.code,
            vec![
                Instr::Push(StackValue::PosInt(1)),
                Instr::Push(StackValue::PosInt(2)),
                Instr::Push(StackValue::PosInt(3)),
                Instr::Mul,
                Instr::Add,
                Instr::Pop(1),
                Instr::Return(0),
            ]
        );
        // spans stay in lockstep with code.
        assert_eq!(prog.spans.len(), prog.code.len());

        // And it executes cleanly through for_program (heap[0] = state), ending
        // with an empty stack after the expression value is popped.
        let vm = run_program(prog);
        assert!(vm.stack.is_empty());
    }

    #[test]
    fn for_program_seeds_state_at_heap0() {
        // The blessed `state` object always lives at heap[0], even when seeded.
        let prog = compile("1;").expect("compiles");
        let state = serde_json::json!({ "count": 7 });
        let vm = VM::for_program(prog.code, state).unwrap();
        match &vm.heap[0] {
            crate::vm::HeapValue::Object(o) => {
                assert_eq!(o.get("count"), Some(&StackValue::PosInt(7)));
            }
            other => panic!("expected state object at heap[0], got {other:?}"),
        }
    }

    #[test]
    fn unsupported_statement_errors() {
        // A bare declaration is not handled until Phase 2 → a diagnostic.
        let errs = compile("let x = 1;").expect_err("should not compile yet");
        assert_eq!(errs.len(), 1);
        // Renders as line:col with a caret.
        let rendered = errs[0].render("let x = 1;");
        assert!(rendered.starts_with("1:1: "), "got: {rendered}");
    }

    #[test]
    fn syntax_error_is_reported() {
        // oxc's own syntax errors are surfaced as Diagnostics.
        let errs = compile("1 +* 2;").expect_err("syntax error");
        assert!(!errs.is_empty());
    }

    // ── Phase 1: expressions ───────────────────────────────────────────
    //
    // Most expression behavior is exercised end-to-end: compile a program that
    // writes its result into `state.r`, run it, then read `heap[0]["r"]`. This
    // routes every expression through the real VM and the `state`/`Ptr(0)`
    // lowering at once.

    use crate::vm::HeapValue;

    /// Compile + run `src` to completion, returning the finished VM.
    fn run_vm(src: &str) -> VM {
        match compile(src) {
            Ok(prog) => run_program(prog),
            Err(errs) => panic!("compile failed: {:?}", errs[0].render(src)),
        }
    }

    /// Read `state.<key>` (a slot of the heap[0] object) from a finished VM.
    fn state_val(vm: &VM, key: &str) -> StackValue {
        match &vm.heap[0] {
            HeapValue::Object(o) => *o.get(key).unwrap_or_else(|| panic!("no state.{key}")),
            other => panic!("state is not an object: {other:?}"),
        }
    }

    /// Evaluate a single expression by assigning it to `state.r`, returning the
    /// resulting `StackValue`.
    fn eval(expr: &str) -> StackValue {
        let vm = run_vm(&format!("state.r = ({expr});"));
        state_val(&vm, "r")
    }

    /// Like `eval`, but resolves the result heap string to an owned `String`.
    fn eval_str(expr: &str) -> String {
        let vm = run_vm(&format!("state.r = ({expr});"));
        match state_val(&vm, "r") {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                HeapValue::String(s) => s.clone(),
                other => panic!("not a string: {other:?}"),
            },
            other => panic!("not a pointer: {other:?}"),
        }
    }

    fn num(v: f64) -> StackValue {
        StackValue::Number(v)
    }

    #[test]
    fn literals() {
        assert_eq!(eval("42"), StackValue::PosInt(42));
        assert_eq!(eval("-7"), StackValue::NegInt(-7)); // folded literal
        assert_eq!(eval("3.5"), num(3.5));
        assert_eq!(eval("true"), StackValue::Bool(true));
        assert_eq!(eval("null"), StackValue::Null);
        assert_eq!(eval("undefined"), StackValue::Undefined);
        assert_eq!(eval_str("\"hi\""), "hi");
        assert!(matches!(eval("NaN"), StackValue::Number(n) if n.is_nan()));
        assert!(matches!(eval("Infinity"), StackValue::Number(n) if n.is_infinite()));
    }

    #[test]
    fn arithmetic_and_operators() {
        assert_eq!(eval("1 + 2 * 3"), num(7.0));
        assert_eq!(eval("(1 + 2) * 3"), num(9.0));
        assert_eq!(eval("10 % 3"), num(1.0));
        assert_eq!(eval("2 ** 10"), num(1024.0));
        assert_eq!(eval("7 & 3"), num(3.0));
        assert_eq!(eval("1 << 4"), num(16.0));
        assert_eq!(eval("-5"), StackValue::NegInt(-5));
        assert_eq!(eval("+\"42\""), num(42.0)); // unary plus ToNumber
        assert_eq!(eval("!0"), StackValue::Bool(true));
        assert_eq!(eval("~0"), num(-1.0));
        assert_eq!(eval_str("\"a\" + \"b\""), "ab");
    }

    #[test]
    fn comparisons_and_equality() {
        assert_eq!(eval("1 < 2"), StackValue::Bool(true));
        assert_eq!(eval("2 <= 2"), StackValue::Bool(true));
        assert_eq!(eval("3 === 3"), StackValue::Bool(true));
        assert_eq!(eval("3 !== 4"), StackValue::Bool(true));
        assert_eq!(eval("1 == \"1\""), StackValue::Bool(true)); // loose
        assert_eq!(eval("1 === \"1\""), StackValue::Bool(false)); // strict
        assert_eq!(eval("null == undefined"), StackValue::Bool(true));
    }

    #[test]
    fn short_circuit_logical() {
        assert_eq!(eval("0 && 5"), StackValue::PosInt(0));
        assert_eq!(eval("3 && 5"), StackValue::PosInt(5));
        assert_eq!(eval("0 || 5"), StackValue::PosInt(5));
        assert_eq!(eval("3 || 5"), StackValue::PosInt(3));
        assert_eq!(eval("null ?? 5"), StackValue::PosInt(5));
        assert_eq!(eval("0 ?? 5"), StackValue::PosInt(0)); // 0 is not nullish
        assert_eq!(eval("undefined ?? 9"), StackValue::PosInt(9));
    }

    #[test]
    fn short_circuit_does_not_evaluate_rhs() {
        // The RHS assignment must NOT run when the LHS short-circuits.
        let vm = run_vm("state.hit = 0; state.r = false && (state.hit = 1);");
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(false));
        assert_eq!(state_val(&vm, "hit"), StackValue::PosInt(0));

        let vm = run_vm("state.hit = 0; state.r = true || (state.hit = 1);");
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(true));
        assert_eq!(state_val(&vm, "hit"), StackValue::PosInt(0));
    }

    #[test]
    fn ternary() {
        assert_eq!(eval("1 ? 10 : 20"), StackValue::PosInt(10));
        assert_eq!(eval("0 ? 10 : 20"), StackValue::PosInt(20));
    }

    #[test]
    fn typeof_op() {
        assert_eq!(eval_str("typeof 5"), "number");
        assert_eq!(eval_str("typeof \"x\""), "string");
        assert_eq!(eval_str("typeof true"), "boolean");
        assert_eq!(eval_str("typeof undefined"), "undefined");
        assert_eq!(eval_str("typeof null"), "object");
        assert_eq!(eval_str("typeof [1]"), "object");
    }

    #[test]
    fn template_literals() {
        let vm = run_vm("state.name = \"bob\"; state.r = `hi ${state.name}, ${1 + 2}!`;");
        match state_val(&vm, "r") {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                HeapValue::String(s) => assert_eq!(s, "hi bob, 3!"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn arrays_and_objects() {
        // Array literal, length, index read.
        assert_eq!(eval("[10, 20, 30].length"), num(3.0));
        assert_eq!(eval("[10, 20, 30][1]"), StackValue::PosInt(20));
        assert_eq!(eval("[10, 20][5]"), StackValue::Undefined); // OOB read
        // Object literal + member read (static and computed).
        assert_eq!(eval("({ a: 1, b: 2 }).b"), StackValue::PosInt(2));
        assert_eq!(eval("({ a: 1, b: 2 })[\"a\"]"), StackValue::PosInt(1));
        assert_eq!(eval("({ a: 1 }).missing"), StackValue::Undefined);
        // Numeric key.
        assert_eq!(eval("({ 1: \"x\" })[1]"), eval("\"x\""));
    }

    #[test]
    fn member_and_index_assignment() {
        // Static member assignment leaves the value and mutates the object.
        let vm = run_vm("state.obj = { a: 1 }; state.r = (state.obj.a = 9);");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(9));
        match state_val(&vm, "obj") {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                HeapValue::Object(o) => assert_eq!(o.get("a"), Some(&StackValue::PosInt(9))),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        // Index assignment into an array.
        let vm = run_vm("state.arr = [1, 2, 3]; state.arr[0] = 99; state.r = state.arr[0];");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(99));
        // Index assignment into an object (string key coercion).
        let vm = run_vm("state.o = {}; state.o[\"k\"] = 7; state.r = state.o.k;");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(7));
    }

    #[test]
    fn optional_chaining() {
        // Missing base short-circuits to undefined; present base reads through.
        assert_eq!(eval("state.nope?.x"), StackValue::Undefined);
        let vm = run_vm("state.obj = { x: 7 }; state.r = state.obj?.x;");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(7));
        // A fully-optional chain short-circuits across links.
        assert_eq!(eval("state.nope?.a?.b"), StackValue::Undefined);
    }

    #[test]
    fn in_and_delete() {
        let vm = run_vm("state.o = { a: 1 }; state.r = (\"a\" in state.o);");
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(true));
        let vm = run_vm("state.o = { a: 1 }; state.r = (\"b\" in state.o);");
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(false));
        // delete removes the key and returns whether it existed.
        let vm = run_vm("state.o = { a: 1 }; state.r = delete state.o.a; state.had = (\"a\" in state.o);");
        assert_eq!(state_val(&vm, "r"), StackValue::Bool(true));
        assert_eq!(state_val(&vm, "had"), StackValue::Bool(false));
    }

    #[test]
    fn intrinsics_static() {
        assert_eq!(eval("Math.max(3, 7)"), num(7.0));
        assert_eq!(eval("Math.min(3, 7)"), num(3.0));
        assert_eq!(eval("Math.abs(-5)"), num(5.0));
        assert_eq!(eval("Math.floor(3.9)"), num(3.0));
        assert_eq!(eval("Math.pow(2, 5)"), num(32.0));
        assert_eq!(eval("Object.keys({ a: 1, b: 2 }).length"), num(2.0));
        assert_eq!(eval("Object.values({ a: 5 })[0]"), StackValue::PosInt(5));
        assert_eq!(eval("JSON.parse(\"[1,2,3]\").length"), num(3.0));
        assert_eq!(eval_str("JSON.stringify([1,2])"), "[1,2]");
        assert_eq!(eval("Number.isInteger(4)"), StackValue::Bool(true));
        assert_eq!(eval("Array.isArray([1])"), StackValue::Bool(true));
        assert_eq!(eval("Array.isArray(5)"), StackValue::Bool(false));
    }

    #[test]
    fn intrinsics_global() {
        assert_eq!(eval_str("String(5)"), "5");
        assert_eq!(eval("Number(\"42\")"), num(42.0));
        assert_eq!(eval("Boolean(0)"), StackValue::Bool(false));
        assert_eq!(eval("Boolean(\"x\")"), StackValue::Bool(true));
    }

    #[test]
    fn intrinsics_methods() {
        assert_eq!(eval("\"a,b,c\".split(\",\").length"), num(3.0));
        assert_eq!(eval("\"a,b,c\".split(\",\", 2).length"), num(2.0));
        assert_eq!(eval("\"hello\".includes(\"ell\")"), StackValue::Bool(true));
        assert_eq!(eval("\"hello\".startsWith(\"he\")"), StackValue::Bool(true));
        assert_eq!(eval("\"hello\".endsWith(\"lo\")"), StackValue::Bool(true));
        assert_eq!(eval("\"hello\".indexOf(\"l\")"), num(2.0));
        assert_eq!(eval_str("\"hello\".slice(1, 3)"), "el");
        assert_eq!(eval_str("\"  hi  \".trim()"), "hi");
        assert_eq!(eval_str("[\"a\", \"b\"].join(\"-\")"), "a-b");
        assert_eq!(eval_str("[1, 2].join()"), "1,2"); // default separator
        // Array mutators run and mutate the receiver.
        let vm = run_vm("state.arr = [1]; state.arr.push(2); state.r = state.arr.length;");
        assert_eq!(state_val(&vm, "r"), num(2.0));
        let vm = run_vm("state.arr = [1, 2, 3]; state.r = state.arr.pop();");
        assert_eq!(state_val(&vm, "r"), StackValue::PosInt(3));
    }

    #[test]
    fn diagnostics_for_unsupported() {
        // These all live in later phases / out of scope and must error cleanly.
        for src in [
            "x;",                 // undeclared variable
            "x = 1;",             // identifier assignment (no declarations yet)
            "state.x += 1;",      // compound assignment (Phase 2)
            "i++;",               // update (Phase 2)
            "foo(1);",            // undeclared function (Phase 3)
            "tools.send(1);",     // tools (Phase 4)
            "raise(\"x\");",      // raise (Phase 4)
            "Math.tan(1);",       // unsupported intrinsic
            "[1, 2].zap();",      // unknown method
            "Math.max(1);",       // wrong arity
            "f(...args);",        // spread arg
            "new Foo();",         // new
            "class C {}",         // class statement
        ] {
            assert!(
                compile(src).is_err(),
                "expected `{src}` to fail to compile"
            );
        }
    }

    #[test]
    fn state_is_ptr_zero() {
        // Bare `state` is the heap[0] object pointer; the whole bag round-trips.
        let vm = run_vm("state.a = 1; state.r = JSON.stringify(state);");
        match state_val(&vm, "r") {
            StackValue::Ptr(p) => match &vm.heap[p as usize] {
                // r was set last, so it appears in the serialized object too.
                HeapValue::String(s) => assert!(s.contains("\"a\":1"), "got {s}"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }
}
