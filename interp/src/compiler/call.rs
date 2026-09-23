use oxc_ast::ast;
use oxc_span::GetSpan;

use crate::analyzer::ConstValue;
use crate::builtin::Builtin;
use crate::span::Span;
use crate::vm::Instr;

impl super::Compiler {
    /// Calls are recognized structurally (the VM has no method objects): a
    /// `namespace.method(...)` static intrinsic, a `recv.method(...)` array/
    /// string method, or a global function like `String(x)`. Each lowers to a
    /// dedicated instruction; user functions, `tools.*`, and `raise` arrive in
    /// later phases.
    pub(super) fn compile_call(&mut self, call: &ast::CallExpression) {
        let span = call.span.into();
        // Check for spread arguments — if present, use `CallSpread` path.
        let has_spread = call
            .arguments
            .iter()
            .any(|arg| matches!(arg, ast::Argument::SpreadElement(_)));

        if has_spread {
            return self.compile_call_spread(call, span);
        }

        // Fast path: no spread — existing argument-collection logic.
        let mut argv: Vec<&ast::Expression> = Vec::with_capacity(call.arguments.len());
        for arg in &call.arguments {
            match arg.as_expression() {
                Some(e) => argv.push(e),
                None => unreachable!(),
            }
        }

        // `call.optional` is the `?.()` token *on the callee value* (`f?.()`,
        // `state.fn?.()`, `Math.max?.(…)`) — distinct from `obj?.method()`
        // (handled below as an optional member).
        if call.optional {
            // Static-call reclaim: a constant, non-nullish callee makes the `?.`
            // guard provably dead — `Math.max?.(a, b)` is identical to
            // `Math.max(a, b)`. Emit the static `CallBuiltin` and skip the
            // guard/`CallDyn`. (First-class builtin refs are the only constant
            // callables today; named function refs join them in Phase 3.)
            if let ast::Expression::StaticMemberExpression(m) = &call.callee
                && let ast::Expression::Identifier(obj) = &m.object
                && Builtin::for_namespace(obj.name.as_str(), m.property.name.as_str()).is_some()
            {
                return self.compile_namespace_call(
                    obj.name.as_str(),
                    m.property.name.as_str(),
                    &argv,
                    span,
                );
            }
            // Otherwise the callee is a genuine runtime value: evaluate it,
            // short-circuit to undefined when nullish (args skipped), else
            // dynamically invoke it.
            self.compile_expr(&call.callee);
            let end = self.begin_optional(span);
            self.compile_args(&argv);
            self.emit(Instr::CallDyn(argv.len() as u32, false), span);
            self.emit(Instr::Label(end), span);
            return;
        }

        match &call.callee {
            // `super(args)` (Step 7b): invoke the parent constructor with the
            // current instance as `this` — not the `new` path (no fresh object).
            ast::Expression::Super(s) => {
                self.compile_super_call(s.span.into(), &argv, span);
            }
            ast::Expression::StaticMemberExpression(m) => {
                let method = m.property.name.as_str();
                // `super.m(args)` (Step 7b): resolve `m` on the *parent prototype*
                // (so an override on `C` is skipped) and call it with `this`.
                if let ast::Expression::Super(s) = &m.object {
                    self.compile_super_method_call(s.span.into(), method, &argv, span);
                    return;
                }
                // A leading identifier matching a reserved namespace is a static
                // intrinsic; otherwise it is a method on the receiver value.
                if let ast::Expression::Identifier(obj) = &m.object {
                    // **`Array.from(x, f)` is a higher-order call
                    // wearing a namespace's clothes.** The builtin
                    // behind the one-argument form cannot invoke a
                    // closure, so the two-argument form accepted `f`
                    // and silently dropped it:
                    // `Array.from({length: 3}, (_, i) => i)` — the
                    // standard way to write a range — returned
                    // `[null, null, null]` and the program carried on
                    // with it. A wrong answer nobody is told about is
                    // worse than the gap it papers over.
                    //
                    // Lowered like any other HOF, with the source as
                    // the receiver: `__arrayFrom(x, f)`, whose body
                    // calls the one-argument builtin to convert and
                    // then maps. Checked here rather than in
                    // `compile_method_call`, because the namespace arm
                    // below claims the call first.
                    if obj.name.as_str() == "Array" && method == "from" && argv.len() == 2 {
                        return self.emit_prelude_call(
                            "__arrayFrom",
                            argv[0],
                            &argv[1..],
                            span,
                            false,
                        );
                    }
                    match obj.name.as_str() {
                        "Math" | "Object" | "JSON" | "Number" | "Array" | "String" | "Map"
                        | "Set" | "console" | "Edit" | "ArrayBuffer" | "Date" => {
                            return self.compile_namespace_call(
                                obj.name.as_str(),
                                method,
                                &argv,
                                span,
                            );
                        }
                        "history" => {
                            // `history.remove(a, b)` → the settle verb
                            // `remove_history`. Recognized structurally,
                            // exactly as `tools` is below: the four
                            // verbs are one family and read as one, and
                            // the bare `*_history` names stay valid
                            // because they are what the host dispatches
                            // on.
                            let verb = match method {
                                "append" => "append_history",
                                "fetch" => "fetch_history",
                                "remove" => "remove_history",
                                "replace" => "replace_history",
                                "slice" => "slice_history",
                                // Through the helper, so the projection
                                // may be a lambda: a settle verb takes
                                // values, and a function pushed onto
                                // that stack is not one.
                                "keep" | "peek" => {
                                    let Some(label) = self.find_root_callee_label("__historyShow")
                                    else {
                                        return self.error(
                                            span,
                                            "`history.keep`/`history.peek` need their prelude \
                                             helper, which was not compiled".to_owned(),
                                        );
                                    };
                                    let k = self.intern_string(method);
                                    self.emit(Instr::PushStr(k), span);
                                    self.compile_args(&argv);
                                    self.emit(Instr::Call(label, 1 + argv.len() as u32), span);
                                    return;
                                }
                                _ => {
                                    let known =
                                        "append, fetch, keep, peek, remove, replace and slice";
                                    return self.error(
                                        span,
                                        format!("`history.{method}` is not a thing — history has {known}"),
                                    );
                                }
                            };
                            self.compile_args(&argv);
                            self.emit(Instr::Settle(verb.into(), argv.len() as u32), span);
                            return;
                        }
                        "tools" => {
                            // `tools.foo(a, b)` → `Invoke("foo", 2)`. Recognized
                            // structurally; `tools` is valid only as the receiver
                            // of such a call (bare `tools` and `tools.foo` without
                            // a call are undeclared-identifier errors elsewhere).
                            self.compile_args(&argv);
                            self.emit(Instr::Invoke(method.into(), argv.len() as u32), span);
                            return;
                        }
                        "Promise" => {
                            return self.compile_promise_call(method, &argv, span);
                        }
                        _ => {}
                    }
                }
                self.compile_method_call(&m.object, method, &argv, span, m.optional);
            }
            ast::Expression::ComputedMemberExpression(m) => {
                // recv[k](args) → ObjPeekDyn + CallDyn(has_this=true)
                self.compile_expr(&m.object);
                self.compile_expr(&m.expression);
                self.emit(Instr::ObjPeekDyn, span);
                self.compile_args(&argv);
                self.emit(Instr::CallDyn(argv.len() as u32, true), span);
            }
            ast::Expression::Identifier(id) => {
                self.compile_user_call(id.name.as_str(), id.span.start, &argv, span)
            }
            other => {
                // General expression callee: evaluate it, push args, CallDyn.
                self.compile_expr(other);
                self.compile_args(&argv);
                self.emit(Instr::CallDyn(argv.len() as u32, false), span);
            }
        }
    }

    /// Compile a call with spread arguments: lower to callee expression +
    /// array of args + [`CallSpread`].
    pub(super) fn compile_call_spread(&mut self, call: &ast::CallExpression, span: Span) {
        // `tools.foo(...args)` can't take the value path: `tools` is only
        // valid structurally as an `Invoke` receiver (compiling it as an
        // expression would give a misleading undeclared-variable error), and
        // `Invoke` has a static arg count. Reject with a targeted error.
        if let ast::Expression::StaticMemberExpression(m) = &call.callee
            && let ast::Expression::Identifier(obj) = &m.object
            && obj.name.as_str() == "tools"
        {
            self.error(span, "spread arguments are not supported on tool calls");
            return;
        }

        // `super(...args)` / `super.m(...args)` (Step 7b) with a spread: same
        // `has_this` layout as the non-spread forms, but the args become an array
        // for `CallSpread`. Receiver is always `this`.
        if let ast::Expression::Super(s) = &call.callee {
            self.emit(Instr::LoadThis, span);
            self.emit_super_class_ref(s.span.into());
            self.compile_call_args_array(&call.arguments, span);
            self.emit(Instr::CallSpread(true), span);
            return;
        }
        if let ast::Expression::StaticMemberExpression(m) = &call.callee
            && let ast::Expression::Super(s) = &m.object
        {
            self.emit(Instr::LoadThis, span);
            self.emit_super_class_ref(s.span.into());
            self.emit(Instr::ObjGet(crate::vm::RcStr::from("prototype")), span);
            self.emit(Instr::ObjGet(m.property.name.as_str().into()), span);
            self.compile_call_args_array(&call.arguments, span);
            self.emit(Instr::CallSpread(true), span);
            return;
        }

        // `f.call(thisArg, ...args)` (and the degenerate `f.apply(...)`) with a
        // spread among the args: forward to the `has_this` dispatch — thisArg is
        // arg 0, the rest become the (spread) args array. See
        // `compile_invoke_forward` for the non-spread case.
        if let ast::Expression::StaticMemberExpression(m) = &call.callee
            && matches!(m.property.name.as_str(), "call" | "apply")
        {
            self.compile_expr(&m.object); // [f]
            let end = if call.optional {
                Some(self.begin_optional(span))
            } else {
                None
            };
            match call.arguments.first().and_then(|a| a.as_expression()) {
                Some(t) => self.compile_expr(t),
                None => self.emit(Instr::PushUndefined, span),
            }
            self.emit(Instr::Dig(1), span); // [thisArg, f]
            let rest = 1.min(call.arguments.len());
            self.compile_call_args_array(&call.arguments[rest..], span);
            self.emit(Instr::CallSpread(true), span);
            if let Some(end) = end {
                self.emit(Instr::Label(end), span);
            }
            return;
        }

        // Detect a method callee so we emit ObjPeek/ObjPeekDyn (keeping the
        // receiver) instead of ObjGet/IndexGet (consuming it), and thread
        // has_this=true to CallSpread.  Skip namespaces (Math.max) — those
        // are builtins accessed via ObjGet.  For known method builtins, emit
        // PushBuiltin instead of ObjPeek so the builtin value (not a property
        // read) lands on the stack for dispatch_call's Builtin arm.
        let has_this = match &call.callee {
            ast::Expression::StaticMemberExpression(m) => {
                if let ast::Expression::Identifier(obj) = &m.object {
                    !matches!(
                        obj.name.as_str(),
                        "Math"
                            | "Object"
                            | "JSON"
                            | "Number"
                            | "Array"
                            | "String"
                            | "Map"
                            | "Set"
                            | "console"
                            | "Edit"
                            | "Date"
                    )
                } else {
                    true
                }
            }
            ast::Expression::ComputedMemberExpression(_) => true,
            _ => false,
        };
        if has_this {
            // Emit receiver + property as a method access, keeping the receiver.
            match &call.callee {
                ast::Expression::StaticMemberExpression(m) => {
                    self.compile_expr(&m.object);
                    let method = m.property.name.as_str();
                    if let Some(builtin) = Builtin::for_method(method) {
                        self.emit(Instr::PushBuiltin(builtin), span);
                    } else {
                        self.emit(Instr::ObjPeek(method.into()), span);
                    }
                }
                ast::Expression::ComputedMemberExpression(m) => {
                    self.compile_expr(&m.object);
                    self.compile_expr(&m.expression);
                    self.emit(Instr::ObjPeekDyn, span);
                }
                _ => unreachable!(),
            }
        } else {
            // Compile callee as a value expression (produces the callable on stack).
            self.compile_expr(&call.callee);
        }

        if call.optional {
            let end = self.begin_optional(span);
            self.compile_call_args_array(&call.arguments, span);
            self.emit(Instr::CallSpread(has_this), span);
            self.emit(Instr::Label(end), span);
        } else {
            self.compile_call_args_array(&call.arguments, span);
            self.emit(Instr::CallSpread(has_this), span);
        }
    }

    /// Compile call arguments into an array on the stack.  Supports spread
    /// elements: leading static args + `ArrNew`, then `ArrExtend` for each
    /// spread and `ArrPush` for each trailing static argument.
    pub(super) fn compile_call_args_array(&mut self, args: &[ast::Argument<'_>], span: Span) {
        // Count leading non-spread arguments.
        let leading_count = args
            .iter()
            .take_while(|a| !matches!(a, ast::Argument::SpreadElement(_)))
            .count();

        // Emit values for leading static segment.
        for arg in args.iter().take(leading_count) {
            if let Some(e) = arg.as_expression() {
                self.compile_expr(e);
            }
        }
        self.emit(Instr::ArrNew(leading_count as u32), span);

        // Remaining: alternates spreads and single-element pushes.
        for arg in args.iter().skip(leading_count) {
            match arg {
                ast::Argument::SpreadElement(s) => {
                    self.compile_expr(&s.argument);
                    self.emit(Instr::ArrExtend, span);
                }
                _ => {
                    if let Some(e) = arg.as_expression() {
                        self.compile_expr(e);
                        self.emit(Instr::ArrPush, span);
                    }
                }
            }
        }
    }

    /// Compile all argument expressions left-to-right.
    pub(super) fn compile_args(&mut self, argv: &[&ast::Expression]) {
        for &e in argv {
            self.compile_expr(e);
        }
    }

    /// Validate an exact arity, recording a diagnostic if it doesn't match.
    pub(super) fn arity(
        &mut self,
        argv: &[&ast::Expression],
        want: usize,
        span: Span,
        name: &str,
    ) -> bool {
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

    /// Compile a call to `builtin`: evaluate the receiver (if any) and the
    /// arguments, then emit `CallBuiltin` — but only after validating the
    /// argument count against `Builtin::meta()`, which is the single source of
    /// truth for both the accepted arity and the builtin's display name.
    ///
    /// `recv` is the method receiver (`None` for free/static builtins); it is
    /// arg 0 and counts toward `meta()`'s bounds. On an arity mismatch a
    /// diagnostic is recorded and nothing is emitted.
    ///
    /// `optional` lowers `recv?.method(args)`: when the receiver is nullish the
    /// whole call short-circuits to `undefined` and the arguments are **not**
    /// evaluated (the guard sits between the receiver and the arguments). Only
    /// meaningful with a receiver.
    pub(super) fn compile_builtin_call(
        &mut self,
        builtin: Builtin,
        recv: Option<&ast::Expression>,
        argv: &[&ast::Expression],
        span: Span,
        optional: bool,
    ) {
        let base = recv.is_some() as u32; // the receiver occupies one arity slot
        let argc = base + argv.len() as u32;
        // Compile-time arity lint — only for receiver-less builtins (namespace /
        // global, e.g. `Math.max`). A *method* call's receiver type isn't known
        // here: it may be an `Object` with a same-named own/proto property that
        // shadows the builtin (with its own arity), so method arity is left to
        // runtime — the handler runs for a matching receiver (leniently, as the
        // runtime already does for `split`/`indexOf`…) and an `Object` receiver
        // reroutes. So a method call never hard-errors on arity here, and its
        // `meta()` is never consulted.
        if recv.is_none() {
            let meta = builtin.meta();
            if argc < meta.min_args || argc > meta.max_args {
                // Report the bounds without the implicit receiver, so the message
                // matches how the call is written in source.
                let lo = meta.min_args.saturating_sub(base);
                let want = if meta.max_args == u32::MAX {
                    format!("at least {lo}")
                } else {
                    let hi = meta.max_args - base;
                    if lo == hi {
                        format!("{lo}")
                    } else {
                        format!("{lo} to {hi}")
                    }
                };
                self.error(
                    span,
                    format!(
                        "`{}` expects {want} argument(s), got {}",
                        meta.name,
                        argv.len()
                    ),
                );
                return;
            }
        }
        // Optional method call: guard on the receiver before the args/call.
        // `JNotNullish` (via `begin_optional`) keeps the receiver on the
        // not-nullish path for the call to consume.
        let end = match (recv, optional) {
            (Some(recv), true) => {
                self.compile_expr(recv);
                Some(self.begin_optional(span))
            }
            (Some(recv), false) => {
                self.compile_expr(recv);
                None
            }
            (None, _) => None,
        };
        self.compile_args(argv);
        self.emit(Instr::CallBuiltin(builtin, argc), span);
        if let Some(end) = end {
            self.emit(Instr::Label(end), span);
        }
    }

    /// Compile a namespaced static call (`Math.max(…)`, `JSON.parse(…)`, …) by
    /// looking the receiver-less builtin up in the declarative table.
    pub(super) fn compile_namespace_call(
        &mut self,
        ns: &str,
        method: &str,
        argv: &[&ast::Expression],
        span: Span,
    ) {
        match Builtin::for_namespace(ns, method) {
            Some(builtin) => self.compile_builtin_call(builtin, None, argv, span, false),
            None => self.error(span, format!("unsupported `{ns}.{method}`")),
        }
    }

    /// `Promise.*` statics. `Promise.all(xs)` and `Promise.allSettled(xs)`
    /// are supported (Phase 7), lowering to the prelude helpers `__all(xs)`
    /// / `__allSettled(xs)` — serial awaits over already-started promises,
    /// which is full fan-out concurrency because every tool call in `xs` is
    /// already in flight. The rest are rejected with diagnostics that say
    /// what to do instead.
    pub(super) fn compile_promise_call(
        &mut self,
        method: &str,
        argv: &[&ast::Expression],
        span: Span,
    ) {
        match method {
            "all" | "allSettled" => {
                if argv.len() != 1 {
                    self.error(
                        span,
                        format!("`Promise.{method}` expects 1 argument, got {}", argv.len()),
                    );
                    return;
                }
                let helper = if method == "all" {
                    "__all"
                } else {
                    "__allSettled"
                };
                self.emit_prelude_call(helper, argv[0], &[], span, false);
            }
            // Wait-any needs VM support — deferred until evidence demands it.
            "race" | "any" => self.error(
                span,
                format!(
                    "`Promise.{method}` is not supported (await the promises you need directly)"
                ),
            ),
            // Pointless wrappers in this dialect: `await` passes plain values
            // through, and rejection is the error path, not a value.
            "resolve" | "reject" => self.error(
                span,
                format!(
                    "`Promise.{method}` is not supported (`await` accepts plain values directly)"
                ),
            ),
            _ => self.error(span, format!("unsupported `Promise.{method}`")),
        }
    }

    /// Global function calls recognized structurally.
    pub(super) fn compile_global_call(
        &mut self,
        name: &str,
        argv: &[&ast::Expression],
        span: Span,
    ) {
        match name {
            // Step 2a Part 2: constructor names called as plain functions.
            // `String(x)`/`Number(x)`/`Boolean(x)` keep their dedicated
            // fast-path instructions (`ToStr`/`ToNum`/`ToBool`) — more
            // efficient than routing through `CallBuiltin` and behaviorally
            // identical. `Array(...)`/`Object(...)` route through the
            // constructor builtin (folding the native construction). `Map()`
            // /`Set()` without `new` throw at runtime (the handler raises
            // `TypeError`), matching JS.
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
            "Array" | "Object" | "Map" | "Set" | "RegExp" | "Function" => {
                if let Some(b) = Builtin::for_constructor(name) {
                    self.compile_builtin_call(b, None, argv, span, false);
                } else {
                    self.error(span, format!("unsupported `{name}`"));
                }
            }
            // Step 2a Part 2: `Math()` / `JSON()` — the namespace is a
            // non-callable object. Push it and dispatch via `CallDyn`, which
            // raises a `TypeError` at runtime ("cannot call a object as a
            // function"), matching JS. A compile-time error would be less
            // faithful — JS throws at runtime.
            "Math" => {
                self.emit(Instr::PushGlobal(crate::vm::GlobalId::Math), span);
                self.compile_args(argv);
                self.emit(Instr::CallDyn(argv.len() as u32, false), span);
            }
            "JSON" => {
                self.emit(Instr::PushGlobal(crate::vm::GlobalId::JSON), span);
                self.compile_args(argv);
                self.emit(Instr::CallDyn(argv.len() as u32, false), span);
            }
            "parseInt" => {
                // Delegate to Number.parseInt
                self.compile_builtin_call(Builtin::NumberParseInt, None, argv, span, false);
            }
            "parseFloat" => {
                // Delegate to Number.parseFloat
                self.compile_builtin_call(Builtin::NumberParseFloat, None, argv, span, false);
            }
            "isNaN" => {
                // Bare isNaN coerces to number first (unlike Number.isNaN):
                // isNaN(x) ≡ Number.isNaN(Number(x))
                if !self.arity(argv, 1, span, "isNaN") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::ToNum, span);
                // NaN !== NaN is the canonical check.
                self.emit(Instr::Pick(0), span);
                self.emit(Instr::Neq, span); // [v !== v] = true only for NaN
            }
            "isFinite" => {
                // Bare isFinite coerces to number first (unlike Number.isFinite).
                if !self.arity(argv, 1, span, "isFinite") {
                    return;
                }
                self.compile_args(argv);
                self.emit(Instr::ToNum, span);
                // After ToNum: coerce to Number.isFinite.
                self.emit(Instr::CallBuiltin(Builtin::NumberIsFinite, 1), span);
            }
            // **The two endings.** Both halt where they stand and
            // neither can be caught: an effect never enters the handler
            // search, so a `try` the program wrote for something else
            // cannot swallow the one decision it most needs to survive.
            // They differ only in what the harness does next — rest the
            // branch, or put the reason in front of the next reply.
            "finish" => {
                // **A flag, not an ending.** It says the task is
                // finished, so the branch should not be prompted again
                // — and that is all it says. What stops the program is
                // `return`; what reaches the person is `tell`. The verb
                // used to do all three at once, and its own doc needed
                // a paragraph to explain that it also halted.
                if !argv.is_empty() {
                    return self.error(
                        span,
                        "`finish()` takes nothing — it says the task is finished, and that \
                         is all. Say what you concluded with `tell(text)`; `return` is what \
                         ends the program.",
                    );
                }
                self.emit(Instr::Finish, span);
                self.emit(Instr::PushUndefined, span);
            }
            "raise" => {
                // `raise("name")` → `Raise(name, 0)`, no payload.
                // `raise("name", expr)` → `Raise(name, 1)`, payload = expr.
                // The condition name must be a string literal. More than one
                // extra arg is a compile error.
                if argv.is_empty() || argv.len() > 2 {
                    self.error(
                        span,
                        "`raise` takes 1 or 2 arguments: raise(\"name\") or raise(\"name\", payload)",
                    );
                    return;
                }
                let name = match &argv[0] {
                    ast::Expression::StringLiteral(lit) => lit.value.as_str().into(),
                    other => {
                        self.error(
                            other.span().into(),
                            "`raise` condition name must be a string literal",
                        );
                        return;
                    }
                };
                if argv.len() >= 2 {
                    // Payload: compile the expression, emit Raise with argc=1.
                    self.compile_expr(argv[1]);
                    self.emit(Instr::Raise(name, 1), span);
                } else {
                    self.emit(Instr::Raise(name, 0), span);
                }
            }
            "tell" => {
                // `tell()` is the one bare verb that settles at dispatch
                // — a local append+deliver, never real IO (23_ONE_AGENT.md
                // C0b) — so it gets its own instruction rather than the
                // shared `Invoke` bucket below: `Notify` pushes the call's
                // result (`undefined`) immediately instead of a promise,
                // which is also what keeps an unawaited `tell()` from
                // ever being mistaken for a value someone is still owed
                // (`machine.rs`'s Rule C exists for exactly that
                // question, and a `tell` should never be able to raise
                // it). `ask` stays on the `Invoke` path below: it is the
                // one call in this vocabulary with a value actually
                // coming back, which `expects_reply` already names.
                self.compile_args(argv);
                self.emit(Instr::Notify(name.into(), argv.len() as u32), span);
            }
            "spawn" | "fork" | "list_agents" | "fetch_history" | "answer" | "append_history"
            | "keep_history" | "peek_history"
            | "remove_history" | "replace_history" | "slice_history" => {
                // **The settle-at-dispatch verbs.** None of these leaves
                // the frame that called it: the host answers each from
                // the log or the tree it already has — `fetch_history`
                // reads a row, `answer`/`append_history` append one,
                // `remove_history`/`rewrite_history` add an edit to the
                // batch the running compaction handler is building,
                // `finish` sets the flag that stops the loop, and
                // `spawn`/`fork`/`list_agents` root a child, branch a
                // context, and read the subtree back. So they lower to
                // `Settle`, which yields the call and takes the value
                // straight back onto the stack.
                //
                // "Settles at dispatch" is about the **frame**, not the
                // clock. The last three cost the harness a round trip
                // through its own loop — creating a child needs the
                // registry and a card, and a branch's live status is
                // session state no single runner can see — but the
                // frame that called them is still standing when the
                // answer lands, which is the only property the lowering
                // depends on.
                //
                // They used to lower to `Invoke` + an `Await` emitted
                // *here* rather than written in the source, so that
                // `const h = spawn(...)` would be the handle and not a
                // promise. That worked, and cost more than it looked:
                // an `Await` the source never wrote put `await` inside
                // plain arrows (`names.map(n => spawn(n))`), which
                // suspended a frame nobody declared `async` and forced
                // `suspend_current_frame` to mint a promise for it.
                // `Settle` buys the same spelling with none of that —
                // and, because a failure now arrives as a throw at the
                // call site instead of a rejection on a promise nobody
                // holds, an unawaited mistake (a wrong compaction
                // label, an `answer` to a question this branch does not
                // owe) stops the program where it happened rather than
                // looking to the model exactly like a call that worked.
                //
                // A source-level `await` in front of any of them still
                // works and still means the same thing: `Await` passes
                // a non-promise straight through, and the card and its
                // exemplars spell several of these with one.
                //
                // `finish` takes no arguments — the only fixed arity
                // here. Nothing else would mean anything, so a wrong
                // count is a clear mistake worth a compile error rather
                // than silently ignored args (same shape as `abandon`).
                // `ask` is deliberately NOT in this set: it is the one
                // call in this vocabulary with a value genuinely coming
                // back from somewhere else, which may take minutes and
                // may fail asynchronously. Its promise is honest.
                self.compile_args(argv);
                self.emit(Instr::Settle(name.into(), argv.len() as u32), span);
            }
            "ask" | "choose" => {
                // The closed, harness-defined vocabulary (phase 20 doc,
                // `docs/20_CODE_MODE.md` Step C1) — a fixed global
                // surface, identical for every agent, known to this
                // compiler exactly the way `raise` already is above.
                // `tools.*` (the "tools" arm in `compile_call`) stays
                // the surface for a specific agent's *configured*
                // capabilities, which this compiler has no static view
                // of; these never vary per agent, so they get the same
                // bare-call treatment `tools.foo(...)` gives its own
                // names — `Invoke`, arity-agnostic here too, left to
                // the host to accept or refuse at runtime.
                //
                // `ask` and `choose` are the ones the settle-at-dispatch
                // arm above does **not** take, and the only bare verbs
                // left with a promise. They are a genuine round trip to
                // someone else, they may take minutes, and they may fail
                // asynchronously: the promise is the honest shape for
                // them, and the `await` in front of them in the card is
                // describing something real.
                //
                // `choose(who, question, options)` differs from `ask`
                // only in what it promises about the value: one of the
                // offered strings, or — when the person answers outside
                // the set — a rejection, which `Await` escalates as a
                // resumable error so the next program decides with their
                // actual words in view and `resume(...)` stands in for
                // the call. That is the A/B/C/Other shape, and it needed
                // no new mechanism: a rejected await was already a
                // resumable condition.
                self.compile_args(argv);
                self.emit(Instr::Invoke(name.into(), argv.len() as u32), span);
            }
            "resume" => {
                // `resume(value?)` — a pure decision value (Step D2),
                // not a host round-trip: no `Invoke`, no promise. A
                // handler's `return resume(v)` continues the raising
                // program with `v`; the harness reads the tag off the
                // returned object rather than this ever being executed
                // as a call, which is why it is a plain object
                // construction — the same shape `TypeError(...)` below
                // already builds.
                if argv.len() > 1 {
                    self.error(span, "`resume` takes at most one argument");
                    return;
                }
                let tag = self.intern_string("resume");
                self.emit(Instr::PushStr(tag), span);
                match argv.first() {
                    Some(v) => self.compile_expr(v),
                    None => self.emit(Instr::PushUndefined, span),
                }
                self.emit(
                    Instr::ObjNew(
                        vec![
                            crate::vm::RcStr::from("__decision"),
                            crate::vm::RcStr::from("value"),
                        ]
                        .into(),
                    ),
                    span,
                );
            }
            "abandon" => {
                // The other decision constructor (Step D2): discards
                // the raising program instead of continuing it. Takes
                // no arguments — unlike the other six harness verbs,
                // this is a fixed-shape constructor, so a wrong arg
                // count is a clear mistake worth a compile error, the
                // same call the error-ctor arm below makes for its own
                // arity.
                if !argv.is_empty() {
                    self.error(span, "`abandon` takes no arguments");
                    return;
                }
                let tag = self.intern_string("abandon");
                self.emit(Instr::PushStr(tag), span);
                self.emit(
                    Instr::ObjNew(vec![crate::vm::RcStr::from("__decision")].into()),
                    span,
                );
            }
            name if super::is_error_ctor(name) => {
                // `TypeError("msg")` / `Error("msg")` → create error object
                // (same logic as `compile_error_ctor` for `new`).
                if argv.len() > 1 {
                    self.error(
                        span,
                        format!("`{name}` takes at most one (message) argument"),
                    );
                    return;
                }
                let name_str = self.intern_string(name);
                self.emit(Instr::PushStr(name_str), span);
                match argv.first() {
                    None => {
                        let empty = self.intern_string("");
                        self.emit(Instr::PushStr(empty), span);
                    }
                    Some(msg) => {
                        self.compile_expr(msg);
                        self.emit(Instr::ToStr, span);
                    }
                }
                self.emit(
                    Instr::ObjNew(
                        vec![
                            crate::vm::RcStr::from("name"),
                            crate::vm::RcStr::from("message"),
                        ]
                        .into(),
                    ),
                    span,
                );
            }
            _ => {
                // Unknown global — compile as a dynamic call resolved at runtime.
                // `PushName` resolves the name via the builtin registry / hardcoded
                // globals: a name that maps to a value is then invoked by `CallDyn`
                // (raising `TypeError` if it isn't callable), while a genuinely
                // undeclared name raises `ReferenceError` at `PushName` itself —
                // before `CallDyn` runs — exactly as JS does for `undeclared()`.
                let name_str = self.intern_string(name);
                self.emit(Instr::PushName(name_str), span);
                self.compile_args(argv);
                self.emit(Instr::CallDyn(argv.len() as u32, false), span);
            }
        }
    }

    /// Array/string methods on a receiver value. Dispatch is purely syntactic
    /// (name + arity) and assumes the conventional receiver type; a mismatch is
    /// a runtime `TypeError`. `optional` is the `recv?.method(...)` case: a
    /// nullish receiver short-circuits the call to `undefined`.
    pub(super) fn compile_method_call(
        &mut self,
        recv: &ast::Expression,
        method: &str,
        argv: &[&ast::Expression],
        span: Span,
        optional: bool,
    ) {
        // ── higher-order array methods (prelude helpers) ──────────
        match method {
            "map" => return self.compile_hof(recv, argv, span, optional, "__map", 1),
            "filter" => return self.compile_hof(recv, argv, span, optional, "__filter", 1),
            "forEach" => return self.compile_hof(recv, argv, span, optional, "__forEach", 1),
            "some" => return self.compile_hof(recv, argv, span, optional, "__some", 1),
            "every" => return self.compile_hof(recv, argv, span, optional, "__every", 1),
            "find" => return self.compile_hof(recv, argv, span, optional, "__find", 1),
            "findIndex" => return self.compile_hof(recv, argv, span, optional, "__findIndex", 1),
            "reduce" => return self.compile_reduce(recv, argv, span, optional),
            "flatMap" => return self.compile_hof(recv, argv, span, optional, "__flatMap", 1),
            "findLast" => return self.compile_hof(recv, argv, span, optional, "__findLast", 1),
            "findLastIndex" => {
                return self.compile_hof(recv, argv, span, optional, "__findLastIndex", 1);
            }
            "sort" => return self.compile_sort(recv, argv, span, optional),
            "toSorted" => {
                return match argv.len() {
                    1 => self.emit_prelude_call("__toSorted", recv, argv, span, optional),
                    0 => self.emit_prelude_call("__toSortedDefault", recv, argv, span, optional),
                    n => self.error(
                        span,
                        format!("`toSorted` expects 0 or 1 argument(s), got {n}"),
                    ),
                };
            }
            // The promise combinators. `await` inside `try`/`catch` is
            // what the card teaches and what composes best here, but
            // reaching for `.catch` is ordinary JavaScript and trapping
            // on it spends a round trip to make a point about style —
            // the prelude helpers await and then call the handler, so
            // `p.then(f).catch(g)` chains as it reads.
            "then" => return self.compile_promise_then(recv, argv, span, optional),
            "catch" => return self.compile_hof(recv, argv, span, optional, "__pcatch", 1),
            "finally" => return self.compile_hof(recv, argv, span, optional, "__pfinally", 1),
            // `f.call`/`f.apply` are invocation forwarders, not builtins: lower
            // them to the existing `has_this` dispatch (`dispatch_call(f, this =
            // thisArg, args)`). See `compile_invoke_forward`.
            "call" => return self.compile_invoke_forward(recv, argv, span, optional, false),
            "apply" => return self.compile_invoke_forward(recv, argv, span, optional, true),
            // `replace`/`replaceAll` are prelude helpers so a *function*
            // replacer can be invoked from JS; they fall back to the
            // `__replaceStr`/`__replaceAllStr` builtins for string replacers.
            "replace" => return self.compile_hof(recv, argv, span, optional, "__replace", 2),
            "replaceAll" => {
                return self.compile_hof(recv, argv, span, optional, "__replaceAll", 2);
            }
            _ => {}
        }
        if let Some(builtin) = Builtin::for_method(method) {
            // Always emit the builtin call, regardless of arity. The runtime
            // decides by receiver *type* (unknown here): a matching receiver runs
            // the builtin (its own lenient/error behavior — the proper builtin
            // arity error for a structural receiver, not a property-read error);
            // an `Object` receiver resolves to a same-named
            // own/proto property, which carries its own arity. So a prototype
            // method sharing a builtin's name but not its arity still works
            // without diverting structural-receiver calls to the dynamic path.
            self.compile_builtin_call(builtin, Some(recv), argv, span, optional);
            return;
        }
        // Not a known builtin method — treat as property access
        // followed by dynamic call (e.g. `state.add5(3)` where
        // add5 is a function stored in state).
        self.compile_dynamic_method_call(recv, method, argv, span, optional);
    }

    /// `f.call(thisArg, ...args)` / `f.apply(thisArg, argsArray)` — JS function
    /// invocation forwarders. Rather than builtins that re-enter dispatch, these
    /// lower to the existing `has_this` dispatch: arrange `[thisArg, f, args…]`
    /// and emit `CallDyn`/`CallSpread(has_this=true)`, i.e.
    /// `dispatch_call(f, this = thisArg, args)`. `dispatch_call` then handles
    /// every callee kind (a `Closure` gets `thisArg` in its frame, a `Builtin`
    /// gets it as arg 0, a `Bound` overrides it with its own `this`).
    ///
    /// Divergence: a plain object cannot shadow `call`/`apply` with its own
    /// method — the receiver is always treated as the function being invoked.
    fn compile_invoke_forward(
        &mut self,
        recv: &ast::Expression, // the function being invoked
        argv: &[&ast::Expression],
        span: Span,
        optional: bool,
        spread: bool,
    ) {
        self.compile_expr(recv); // [f]
        let end = if optional {
            Some(self.begin_optional(span))
        } else {
            None
        };
        // thisArg = argv[0] (or undefined), pushed then swapped *below* the
        // callee to match the `has_this` layout `[thisArg, f, …]`.
        match argv.first() {
            Some(t) => self.compile_expr(t),
            None => self.emit(Instr::PushUndefined, span),
        }
        self.emit(Instr::Dig(1), span); // [thisArg, f]
        if spread {
            // `.apply`: argv[1] is the args array (absent → empty). A nullish
            // value yields no args; a non-array, non-nullish value is a runtime
            // TypeError (both handled by `CallSpread`).
            match argv.get(1) {
                Some(arr) => self.compile_expr(arr),
                None => self.emit(Instr::ArrNew(0), span),
            }
            self.emit(Instr::CallSpread(true), span);
        } else {
            // `.call`: argv[1..] are the call args.
            let rest = argv.get(1..).unwrap_or(&[]);
            for &a in rest {
                self.compile_expr(a);
            }
            self.emit(Instr::CallDyn(rest.len() as u32, true), span);
        }
        if let Some(end) = end {
            self.emit(Instr::Label(end), span);
        }
    }

    /// `super(args)` in a derived constructor (Step 7b): dispatch the parent
    /// constructor with `this` (the instance being built) as the receiver, so
    /// its `this.x = …` writes onto the same instance. Arranged as the `has_this`
    /// call layout `[this, Parent, args…]` + `CallDyn(has_this=true)` — *not* the
    /// `New` path (no fresh instance is allocated). The parent value is the
    /// captured superclass binding (resolved at this `super` node's span).
    pub(super) fn compile_super_call(
        &mut self,
        super_span: Span,
        argv: &[&ast::Expression],
        span: Span,
    ) {
        self.emit(Instr::LoadThis, span); // receiver = the instance
        self.emit_super_class_ref(super_span); // callee = parent constructor
        self.compile_args(argv);
        self.emit(Instr::CallDyn(argv.len() as u32, true), span);
    }

    /// `super.m(args)` (Step 7b): resolve `m` on the **parent prototype** (so a
    /// `C` override is bypassed), then call it with `this` bound to the instance.
    /// Layout `[this, Parent.prototype.m, args…]` + `CallDyn(has_this=true)`.
    pub(super) fn compile_super_method_call(
        &mut self,
        super_span: Span,
        method: &str,
        argv: &[&ast::Expression],
        span: Span,
    ) {
        self.emit(Instr::LoadThis, span); // receiver = the instance
        self.emit_super_class_ref(super_span); // parent constructor
        self.emit(Instr::ObjGet(crate::vm::RcStr::from("prototype")), span); // Parent.prototype
        self.emit(Instr::ObjGet(method.into()), span); // Parent.prototype.m (chain walk)
        self.compile_args(argv);
        self.emit(Instr::CallDyn(argv.len() as u32, true), span);
    }

    /// Read the captured superclass (parent constructor) value at a `super` use
    /// site. The analyzer registered this `super` node's span as a reference to
    /// the `extends` identifier, so it resolves like any captured binding.
    pub(super) fn emit_super_class_ref(&mut self, super_span: Span) {
        // `const_ref`/`ref_slot` are keyed by the `super` node's start offset
        // (like any captured binding); `super_span` itself is only for the
        // instructions this emits.
        let key = super_span.start;
        if let Some(value) = self.const_ref(key) {
            let push = self.const_value_push(&value);
            self.emit(push, super_span);
        } else if let Some(r) = self.ref_slot(key) {
            self.emit_slot_read(&r, super_span);
        } else {
            self.error(
                super_span,
                "`super` is only valid inside a derived class constructor or method",
            );
        }
    }

    /// Compile a method call where the method name is not a known builtin.
    /// Lowers `recv.method(args)` to: evaluate recv, get property `method`,
    /// evaluate args, then CallDyn.
    pub(super) fn compile_dynamic_method_call(
        &mut self,
        recv: &ast::Expression,
        method: &str,
        argv: &[&ast::Expression],
        span: Span,
        optional: bool,
    ) {
        // Evaluate the receiver.
        self.compile_expr(recv);

        if optional {
            // Optional call: guard on the receiver before reading the property.
            let end = self.begin_optional(span);
            // Peek the property: keep recv below for has_this binding.
            self.emit(Instr::ObjPeek(method.into()), span);
            // Evaluate args.
            self.compile_args(argv);
            self.emit(Instr::CallDyn(argv.len() as u32, true), span);
            self.emit(Instr::Label(end), span);
        } else {
            // Peek the property: keep recv below for has_this binding.
            self.emit(Instr::ObjPeek(method.into()), span);
            // Evaluate args.
            self.compile_args(argv);
            self.emit(Instr::CallDyn(argv.len() as u32, true), span);
        }
    }

    // ── Phase 4.0: higher-order array methods (prelude) ──────────────

    /// Lower a higher-order array method (`arr.map(cb)`, `arr.filter(cb)`, …) to
    /// a static `Call` of its prelude helper. `helper` is the helper's function
    /// name (`"__map"`); `want_cb` is the number of callback arguments the call
    /// site must supply (the receiver is added implicitly as the helper's first
    /// parameter). The helper is self-contained (no captures), so a static
    /// `Call` is always valid.
    pub(super) fn compile_hof(
        &mut self,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: Span,
        optional: bool,
        helper: &str,
        want_cb: usize,
    ) {
        if argv.len() != want_cb {
            self.error(
                span,
                format!(
                    "`{}` expects {want_cb} argument(s), got {}",
                    &helper[2..],
                    argv.len()
                ),
            );
            return;
        }
        self.emit_prelude_call(helper, recv, argv, span, optional);
    }

    /// `p.then(onValue)` / `p.then(onValue, onError)` — JS's two forms,
    /// both lowering to `__pthen(p, f, g)`. The one-argument form
    /// passes two arguments and lets `g` arrive `undefined`, which the
    /// helper tests for, rather than needing a second helper the way
    /// `sort`/`sortDefault` and `reduce`/`reduce1` do: those differ in
    /// *behaviour* without their second argument, this one does not.
    pub(super) fn compile_promise_then(
        &mut self,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: Span,
        optional: bool,
    ) {
        if argv.is_empty() || argv.len() > 2 {
            self.error(
                span,
                format!("`then` expects 1 or 2 arguments, got {}", argv.len()),
            );
            return;
        }
        self.emit_prelude_call("__pthen", recv, argv, span, optional);
    }

    /// `arr.reduce(cb[, init])`. The two JS forms map to two helpers: with an
    /// initial value → `__reduce(a, f, acc)`; without → `__reduce1(a, f)`
    /// (seeded from element 0).
    pub(super) fn compile_reduce(
        &mut self,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: Span,
        optional: bool,
    ) {
        match argv.len() {
            2 => self.emit_prelude_call("__reduce", recv, argv, span, optional),
            1 => self.emit_prelude_call("__reduce1", recv, argv, span, optional),
            n => self.error(
                span,
                format!("`reduce` expects 1 or 2 argument(s), got {n}"),
            ),
        }
    }

    /// `arr.sort([compareFn])`. With a comparator → `__sort(a, f)`;
    /// without → `__sortDefault(a)` (the JS default string comparison).
    pub(super) fn compile_sort(
        &mut self,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: Span,
        optional: bool,
    ) {
        match argv.len() {
            1 => self.emit_prelude_call("__sort", recv, argv, span, optional),
            0 => self.emit_prelude_call("__sortDefault", recv, argv, span, optional),
            n => self.error(span, format!("`sort` expects 0 or 1 argument(s), got {n}")),
        }
    }

    /// Emit a static call to a prelude helper: evaluate the receiver (the
    /// helper's first parameter), then the remaining args, then
    /// `Call(helper_label, 1 + argv.len())`. `optional` (`arr?.map(cb)`) guards
    /// the receiver — a nullish receiver short-circuits to `undefined`, skipping
    /// the args and the call.
    pub(super) fn emit_prelude_call(
        &mut self,
        helper: &str,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: Span,
        optional: bool,
    ) {
        let Some(label) = self.find_root_callee_label(helper) else {
            // The prelude assembler appends a helper whenever its method appears
            // in source, so a missing label is an internal inconsistency.
            self.error(
                span,
                format!("internal error: prelude helper `{helper}` is unavailable"),
            );
            return;
        };
        let arity = 1 + argv.len() as u32; // receiver + callback (+ init)
        self.compile_expr(recv);
        let end = if optional {
            Some(self.begin_optional(span))
        } else {
            None
        };
        self.compile_args(argv);
        self.emit(Instr::Call(label, arity), span);
        if let Some(end) = end {
            self.emit(Instr::Label(end), span);
        }
    }

    /// Find the entry label of a top-level (root-scope) function declaration by
    /// name. Used to resolve prelude helpers, which are always declared at the
    /// top level regardless of where the call site is.
    pub(super) fn find_root_callee_label(&self, name: &str) -> Option<u32> {
        let analysis = self.analysis.as_ref().expect("analysis present");
        let root = &analysis.scopes[analysis.root];
        for &child_id in &root.children {
            let child = &analysis.scopes[child_id];
            if child.is_declaration && child.self_name.as_deref() == Some(name) {
                return Some(child.label);
            }
        }
        None
    }

    /// Call to a user-defined function identified by a bare name. If the name
    /// resolves to a local binding, emit a static `Call` (when we know the
    /// label) or `CallDyn`. Otherwise fall through to the built-in global call
    /// path (`String`, `Number`, `Boolean`, `raise`).
    pub(super) fn compile_user_call(
        &mut self,
        name: &str,
        callee_span: u32,
        argv: &[&ast::Expression],
        span: Span,
    ) {
        // A constant function (Phase F): no slot — call its label statically.
        // Pad missing args to the declared arity (as the slotted path does).
        if let Some(ConstValue::Fn { label, arity, .. }) = self.const_ref(callee_span) {
            self.compile_args(argv);
            let passed = argv.len() as u32;
            for _ in passed..arity {
                self.emit(Instr::PushUndefined, span);
            }
            self.emit(Instr::Call(label, passed.max(arity)), span);
            return;
        }
        if let Some(r) = self.ref_slot(callee_span) {
            // Try to resolve to a static `Call`. If the function was declared
            // in this scope and has NO captures, we can use a static Call.
            // Functions with captures must use CallDyn so the VM installs
            // the upvals as leading locals.
            match self.find_callee_label(name) {
                Some(l) if !self.function_has_captures(name) => {
                    // Static call: push args and Call. Pad with Undefined when
                    // the caller passes fewer args than the function declares, so
                    // `arg_count` reaches the declared arity. When the caller
                    // passes *more* args than declared params, pass the larger
                    // count so the surplus stays reachable via `arguments` (the
                    // prologue `EnterFrame` then normalizes the slots to nparams).
                    let expected_arity = self.function_arity(name);
                    self.compile_args(argv);
                    let passed = argv.len() as u32;
                    for _ in passed..expected_arity {
                        self.emit(Instr::PushUndefined, span);
                    }
                    self.emit(Instr::Call(l, passed.max(expected_arity)), span);
                }
                _ => {
                    // Dynamic call: load callee below args, then CallDyn
                    // (installs upvals for captured/closure callees). The
                    // callee read goes through `emit_slot_read`: an
                    // effectively-const binding's slot may be dead-eliminated,
                    // so a raw `Local` load here would read undefined (and
                    // miscall) instead of the propagated constant.
                    self.emit_slot_read(&r, span);
                    self.compile_args(argv);
                    self.emit(Instr::CallDyn(argv.len() as u32, false), span);
                }
            }
            return;
        }

        // Not a local — try global/built-in.
        self.compile_global_call(name, argv, span)
    }

    /// Check whether a named function in the current scope has captures.
    pub(super) fn function_has_captures(&self, name: &str) -> bool {
        let analysis = self.analysis.as_ref().expect("analysis present");
        let scope = &analysis.scopes[self.current_scope];
        for &child_id in &scope.children {
            let child = &analysis.scopes[child_id];
            if child.is_declaration && child.self_name.as_deref() == Some(name) {
                return !child.captures.is_empty();
            }
        }
        false
    }

    /// Find the entry label of a function named `name` declared in the current
    /// scope. Returns `None` if not statically known.
    pub(super) fn find_callee_label(&self, name: &str) -> Option<u32> {
        let analysis = self.analysis.as_ref().expect("analysis present");
        let scope = &analysis.scopes[self.current_scope];
        for &child_id in &scope.children {
            let child = &analysis.scopes[child_id];
            if child.is_declaration && child.self_name.as_deref() == Some(name) {
                return Some(child.label);
            }
        }
        None
    }

    /// Get the declared parameter count of a function named `name` in the
    /// current scope. Returns 0 if not found.
    pub(super) fn function_arity(&self, name: &str) -> u32 {
        let analysis = self.analysis.as_ref().expect("analysis present");
        let scope = &analysis.scopes[self.current_scope];
        for &child_id in &scope.children {
            let child = &analysis.scopes[child_id];
            if child.is_declaration && child.self_name.as_deref() == Some(name) {
                return child.declared_arity();
            }
        }
        0
    }
}
