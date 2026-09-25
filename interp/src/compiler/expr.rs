use oxc_ast::ast;
use oxc_span::GetSpan;

use crate::builtin::Builtin;
use crate::span::Span;
use crate::vm::{Instr, Value};

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
                Value::PosInt(v) => self.emit(Instr::PushPosInt(v), lit.span.into()),
                Value::Float(v) => self.emit(Instr::PushFloat(v), lit.span.into()),
                _ => unreachable!(),
            },
            ast::Expression::StringLiteral(lit) => {
                // Not `lit.value.as_str()`: a literal holding a lone surrogate
                // reaches us as oxc's in-band encoding. See `cook.rs`.
                let s = self.intern_units(&super::cook::string_literal_units(lit));
                self.emit(Instr::PushStr(s), lit.span.into());
            }
            ast::Expression::BooleanLiteral(lit) => {
                self.emit(Instr::PushBool(lit.value), lit.span.into());
            }
            ast::Expression::NullLiteral(lit) => {
                self.emit(Instr::PushNull, lit.span.into());
            }
            ast::Expression::TemplateLiteral(tl) => self.compile_template(tl),

            // ── identifiers ───────────────────────────────────────────
            ast::Expression::Identifier(id) => {
                self.compile_identifier(id.name.as_str(), id.span.into())
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
                        self.emit(Instr::Pop(1), e.span().into());
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
                self.emit(Instr::Await, a.span.into());
            }

            // ── Phase 3: function expressions / arrows ────────────────
            ast::Expression::FunctionExpression(f) => {
                self.compile_function_expr(f, f.span.into());
            }
            ast::Expression::ArrowFunctionExpression(f) => {
                self.compile_arrow_expr(f, f.span.into());
            }

            // ── informative errors for out-of-scope nodes ─────────────
            ast::Expression::BigIntLiteral(b) => {
                self.error(b.span.into(), "BigInt is not supported")
            }
            ast::Expression::RegExpLiteral(r) => {
                let span = r.span.into();
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
                    self.emit_slot_read(&r, t.span.into());
                } else {
                    self.emit(Instr::LoadThis, t.span.into());
                }
            }
            ast::Expression::NewExpression(n) => {
                if let ast::Expression::Identifier(id) = &n.callee {
                    // Step 2a Part 2: native constructors (`new Map()`,
                    // `new Set()`, `new RegExp()`, `new Array()`, …) route
                    // through the generic `new` path: `compile_expr(callee)`
                    // emits `PushBuiltin(constructor)`, then `New` +
                    // `NewReturn` — `Instr::New`'s builtin-constructor arm
                    // folds the type's native construction. The dedicated
                    // `compile_*_ctor` helpers are retired by this, the error
                    // classes included: they have registry rows like every
                    // other constructor, so there is nothing left for a
                    // special case to add.
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
                        n.span.into(),
                        "`new Promise` is not supported: promises come only from `tools.*` calls \
                         and async functions (there is no executor pattern)",
                    )
                } else {
                    self.compile_new_call(n);
                }
            }
            ast::Expression::ClassExpression(c) => self.compile_class_expr(c, c.span.into()),
            ast::Expression::Super(s) => self.error(
                s.span.into(),
                "`super` is only valid as `super(...)` or `super.method(...)` \
                 inside a derived class",
            ),
            // ── TypeScript that erases to its operand ────────────────
            // `e as T`, `e satisfies T`, `<T>e` and `e!` are all claims
            // about a type and none of them is a computation: what runs
            // is the expression inside. Erasing them here is what
            // `tsc` does, and the alternative — refusing them — makes a
            // paste from a typed codebase fail on a line that would
            // have behaved identically with the annotation deleted.
            ast::Expression::TSAsExpression(e) => self.compile_expr(&e.expression),
            ast::Expression::TSSatisfiesExpression(e) => self.compile_expr(&e.expression),
            ast::Expression::TSTypeAssertion(e) => self.compile_expr(&e.expression),
            ast::Expression::TSNonNullExpression(e) => self.compile_expr(&e.expression),
            ast::Expression::TSInstantiationExpression(e) => self.compile_expr(&e.expression),
            // **The one a model actually reaches for.** A live run on
            // 2026-09-20 wrote ``tools.bash(String.raw`…`)`` to keep
            // backslashes literal in a shell command, which is a good
            // instinct and a tagged template, which this does not
            // compile. "unsupported expression" over a heredoc names
            // neither the construct nor the way round it.
            ast::Expression::TaggedTemplateExpression(e) => self.error(
                e.span.into(),
                "a tagged template is not supported. `String.raw` is the usual reason to \
                 want one: write the text as an ordinary string with each backslash \
                 doubled, or build it with `+` so there is no escape to keep.",
            ),
            other => self.error(other.span().into(), "unsupported expression"),
        }
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
        let span = n.span.into();
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
    pub(super) fn compile_identifier(&mut self, name: &str, span: Span) {
        // A local/param/captured variable resolves to its frame slot (resolved
        // by analysis, keyed by this reference's start offset — `key` below);
        // `Local` dereferences a boxed slot transparently. `span` (the whole
        // identifier's range) is only for the instructions this emits.
        let key = span.start;
        // An eliminated `const x = <literal>` binding (Phase E): no slot — the
        // reference is the literal itself (resolved intra- or cross-function by
        // analysis). Composes with const-folding like any other push.
        if let Some(value) = self.const_ref(key) {
            let push = self.const_value_push(&value);
            self.emit(push, span);
            return;
        }
        if let Some(r) = self.ref_slot(key) {
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
