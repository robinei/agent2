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
    #[allow(dead_code)] // used from Phase 2+ (control flow / functions)
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

    fn compile_expr(&mut self, expr: &ast::Expression) {
        match expr {
            ast::Expression::NumericLiteral(lit) => {
                let value = number_literal_to_value(lit.value);
                self.emit(Instr::Push(value), lit.span.start);
            }
            ast::Expression::ParenthesizedExpression(p) => self.compile_expr(&p.expression),
            ast::Expression::BinaryExpression(bin) => self.compile_binary(bin),
            other => self.error(other.span().start, "unsupported expression"),
        }
    }

    fn compile_binary(&mut self, bin: &ast::BinaryExpression) {
        use ast::BinaryOperator as Op;
        // Evaluate operands left-to-right; the op pops rhs then lhs.
        self.compile_expr(&bin.left);
        self.compile_expr(&bin.right);
        let instr = match bin.operator {
            Op::Addition => Instr::Add,
            Op::Subtraction => Instr::Sub,
            Op::Multiplication => Instr::Mul,
            Op::Division => Instr::Div,
            Op::Remainder => Instr::Mod,
            _ => {
                self.error(bin.span.start, "unsupported binary operator");
                return;
            }
        };
        self.emit(instr, bin.span.start);
    }
}

/// Canonicalize a numeric literal: an integer value in `u64` range becomes a
/// `PosInt` (literals are non-negative; unary minus is a separate operator),
/// otherwise a `Number`. Mirrors the VM's canonical integer representation.
fn number_literal_to_value(value: f64) -> StackValue {
    if value.fract() == 0.0 && value >= 0.0 && value <= u64::MAX as f64 {
        StackValue::PosInt(value as u64)
    } else {
        StackValue::Number(value)
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
}
