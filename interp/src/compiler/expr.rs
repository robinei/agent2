use oxc_ast::ast;
use oxc_span::GetSpan;

use crate::builtin::Builtin;
use crate::vm::{Instr, RcStr, Value};

impl super::Compiler {
    /// Every expression leaves exactly one value on the stack (the
    /// stack-discipline invariant). Unsupported nodes record a diagnostic and
    /// emit nothing — the diagnostics abort the compile before a `Program` is
    /// produced, so the missing value never matters.
    pub(super) fn compile_expr(&mut self, expr: &ast::Expression) {
        match expr {
            // ── literals ──────────────────────────────────────────────
            ast::Expression::NumericLiteral(lit) => match super::number_literal_to_value(lit.value)
            {
                Value::PosInt(v) => self.emit(Instr::PushPosInt(v), lit.span.start),
                Value::Float(v) => self.emit(Instr::PushFloat(v), lit.span.start),
                _ => unreachable!(),
            },
            ast::Expression::StringLiteral(lit) => {
                let s = self.intern_string(lit.value.as_str());
                self.emit(Instr::PushStr(s), lit.span.start);
            }
            ast::Expression::BooleanLiteral(lit) => {
                self.emit(Instr::PushBool(lit.value), lit.span.start);
            }
            ast::Expression::NullLiteral(lit) => {
                self.emit(Instr::PushNull, lit.span.start);
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
            ast::Expression::AssignmentExpression(a) => self.compile_assignment(a, true),
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
            ast::Expression::ChainExpression(chain) => {
                self.compile_chain_element(&chain.expression)
            }

            ast::Expression::ParenthesizedExpression(p) => self.compile_expr(&p.expression),

            ast::Expression::UpdateExpression(u) => self.compile_update(u, true),

            // ── Phase 7: await ────────────────────────────────────────
            // `await x` → evaluate x, `Await`. A non-promise value passes
            // through unchanged; a pending promise yields to the host with
            // ip parked on the Await (it re-executes after resolution).
            // The parser confines `await` to async bodies and the top level
            // (module mode), which Tier 2's suspension story relies on.
            ast::Expression::AwaitExpression(a) => {
                self.compile_expr(&a.argument);
                self.emit(Instr::Await, a.span.start);
            }

            // ── Phase 3: function expressions / arrows ────────────────
            ast::Expression::FunctionExpression(f) => {
                self.compile_function_expr(f, f.span.start);
            }
            ast::Expression::ArrowFunctionExpression(f) => {
                self.compile_arrow_expr(f, f.span.start);
            }

            // ── informative errors for out-of-scope nodes ─────────────
            ast::Expression::BigIntLiteral(b) => {
                self.error(b.span.start, "BigInt is not supported")
            }
            ast::Expression::RegExpLiteral(r) => {
                let span = r.span.start;
                let pattern = self.intern_string(r.regex.pattern.text.as_str());
                let flags_str = super::regexp_flags_to_str(r.regex.flags);
                let flags = self.intern_string(&flags_str);
                // Route through the `RegExp` constructor builtin — the same
                // path as `new RegExp(pattern, flags)` and `RegExp(...)`. One
                // canonical construction path (the `regexp_ctor` handler →
                // `alloc_regexp`), no dedicated instruction.
                self.emit(Instr::PushStr(pattern), span);
                self.emit(Instr::PushStr(flags), span);
                self.emit(Instr::CallBuiltin(Builtin::RegExpCtor, 2), span);
            }
            ast::Expression::ThisExpression(t) => {
                // A `this` inside an arrow resolves to the nearest non-arrow's
                // reified captured slot (the analyzer recorded it as a free var
                // resolved through the upval chain).  A direct `this` in a
                // non-arrow function has no slot resolution and falls through
                // to LoadThis.
                if let Some(r) = self.ref_slot(t.span.start) {
                    self.emit_slot_read(&r, t.span.start);
                } else {
                    self.emit(Instr::LoadThis, t.span.start);
                }
            }
            ast::Expression::NewExpression(n) => {
                if let ast::Expression::Identifier(id) = &n.callee {
                    if super::is_error_ctor(id.name.as_str()) {
                        return self.compile_error_ctor(id.name.as_str(), n);
                    }
                    // Step 2a Part 2: native constructors (`new Map()`,
                    // `new Set()`, `new RegExp()`, `new Array()`, …) route
                    // through the generic `new` path: `compile_expr(callee)`
                    // emits `PushBuiltin(constructor)`, then `New` +
                    // `NewReturn` — `Instr::New`'s builtin-constructor arm
                    // folds the type's native construction. The dedicated
                    // `compile_*_ctor` helpers are retired by this.
                    if Builtin::for_constructor(id.name.as_str()).is_some() {
                        return self.compile_new_call(n);
                    }
                }
                // Targeted message for the misuse LLMs actually type: there is
                // no executor pattern (7_ASYNC commitment 4) — every promise
                // comes from a tool call or (Tier 2) an async function call,
                // so all promises provably settle.
                if matches!(&n.callee, ast::Expression::Identifier(id) if id.name == "Promise") {
                    self.error(
                        n.span.start,
                        "`new Promise` is not supported: promises come only from `tools.*` calls \
                         and async functions (there is no executor pattern)",
                    )
                } else {
                    self.compile_new_call(n);
                }
            }
            ast::Expression::ClassExpression(c) => self.compile_class_expr(c, c.span.start),
            ast::Expression::Super(s) => self.error(
                s.span.start,
                "`super` is only valid as `super(...)` or `super.method(...)` \
                 inside a derived class",
            ),
            other => self.error(other.span().start, "unsupported expression"),
        }
    }

    /// `new Error(msg)` / `new TypeError(msg)` / …: build the `{ name,
    /// message }` error object. The message coerces with ToString at
    /// construction (`new Error(123)` → `"123"`, as in JS); absent → `""`.
    pub(super) fn compile_error_ctor(&mut self, name: &str, n: &ast::NewExpression) {
        let span = n.span.start;
        if n.arguments.len() > 1 {
            // JS's `{ cause }` options bag is out of scope; stay strict.
            self.error(
                span,
                format!("`new {name}` takes at most one (message) argument"),
            );
            return;
        }
        let name_str = self.intern_string(name);
        self.emit(Instr::PushStr(name_str), span);
        match n.arguments.first() {
            None => {
                let empty = self.intern_string("");
                self.emit(Instr::PushStr(empty), span);
            }
            Some(arg) => match arg.as_expression() {
                Some(msg) => {
                    self.compile_expr(msg);
                    self.emit(Instr::ToStr, span);
                }
                None => {
                    self.error(
                        span,
                        format!("spread arguments are not supported in `new {name}`"),
                    );
                    return;
                }
            },
        }
        self.emit(
            Instr::ObjNew(vec![RcStr::from("name"), RcStr::from("message")].into()),
            span,
        );
    }

    /// `new F(args)` for a user-defined function `F` (not a builtin ctor).
    /// MVP requires a statically resolvable callee (an Identifier); in-practice
    /// `new (expr)()` is rare and the dynamic form is a documented divergence.
    /// Compiles to: push callee, push args, `New(nargs)`, `NewReturn`.
    /// Step 2a Part 2: native constructors (`new Map()`, `new RegExp()`, …)
    /// also flow through here — `compile_expr(callee)` emits
    /// `PushBuiltin(constructor)`, and `Instr::New`'s builtin-constructor arm
    /// Step 2a Part 2: native constructors (`new Map()`, `new RegExp()`, …)
    /// also flow through here — `compile_expr(callee)` emits
    /// `PushBuiltin(constructor)`, and `Instr::New`'s builtin-constructor arm
    /// dispatches the native construction (`VM::construct_builtin`).
    fn compile_new_call(&mut self, n: &ast::NewExpression) {
        let span = n.span.start;
        // Push the callee (a Closure value)
        self.compile_expr(&n.callee);
        let nargs = n.arguments.len() as u32;
        // Push arguments left-to-right
        for arg in &n.arguments {
            match arg.as_expression() {
                Some(e) => self.compile_expr(e),
                None => {
                    self.error(span, "spread arguments are not supported with `new`");
                    return;
                }
            }
        }
        self.emit(Instr::New(nargs), span);
        self.emit(Instr::NewReturn, span);
    }

    /// A bare identifier resolves only to the host-seeded `input` object or the
    /// global literal-like names. Everything else is an undeclared variable —
    /// a compile error. (Local variables arrive in Phase 2/3; namespace names
    /// like `Math`/`Object` are recognized structurally as call/member
    /// receivers, never as bare values.)
    pub(super) fn compile_identifier(&mut self, name: &str, span: u32) {
        // A local/param/captured variable resolves to its frame slot (resolved
        // by analysis, keyed by this reference's span); `Local` dereferences a
        // boxed slot transparently.
        // An eliminated `const x = <literal>` binding (Phase E): no slot — the
        // reference is the literal itself (resolved intra- or cross-function by
        // analysis). Composes with const-folding like any other push.
        if let Some(value) = self.const_ref(span) {
            let push = self.const_value_push(&value);
            self.emit(push, span);
            return;
        }
        if let Some(r) = self.ref_slot(span) {
            self.emit_slot_read(&r, span);
            return;
        }
        // `arguments` (when not shadowed by a real binding above) is the current
        // frame's argument array — built and cached per frame by the VM.
        if name == "arguments" {
            self.emit(Instr::Arguments, span);
            return;
        }
        match name {
            // Host-seeded consts at fixed object slots (see `is_host_const`).
            "input" => self.emit(Instr::PushObject(0), span),
            "attachments" => self.emit(Instr::PushObject(1), span),
            "undefined" => self.emit(Instr::PushUndefined, span),
            "NaN" => self.emit(Instr::PushFloat(f64::NAN), span),
            "Infinity" => self.emit(Instr::PushFloat(f64::INFINITY), span),
            // Step 2a Part 2: bare constructor identifiers resolve to the real
            // `Value::Builtin` (callable, `typeof === "function"`). The
            // compiler's fast path (`Array(…)`, `new Array(…)`, `Array.isArray`)
            // still lowers to `CallBuiltin`/`New` directly; this is the
            // value/reflective path (`let f = Array`, `f === globalThis.Array`).
            _ if Builtin::for_constructor(name).is_some() => {
                let b = Builtin::for_constructor(name).unwrap();
                self.emit(Instr::PushBuiltin(b), span);
            }
            // Step 2a Part 2: bare namespace identifiers (`Math`, `JSON`)
            // resolve to the real frozen `Value::Object` (non-callable, `typeof
            // === "object"`). The fast path (`Math.max(…)`) still lowers to
            // `CallBuiltin`; this is the value path (`let m = Math`).
            "Math" => self.emit(Instr::PushGlobal(crate::vm::GlobalId::Math), span),
            "JSON" => self.emit(Instr::PushGlobal(crate::vm::GlobalId::JSON), span),
            _ => {
                let name_str = self.intern_string(name);
                self.emit(Instr::PushName(name_str), span);
            }
        }
    }
}
