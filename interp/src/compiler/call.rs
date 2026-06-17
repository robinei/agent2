use oxc_ast::ast;
use oxc_span::GetSpan;

use crate::analyzer::ConstValue;
use crate::builtin::Builtin;
use crate::vm::Instr;

impl<'src> super::Compiler<'src> {
    /// Calls are recognized structurally (the VM has no method objects): a
    /// `namespace.method(...)` static intrinsic, a `recv.method(...)` array/
    /// string method, or a global function like `String(x)`. Each lowers to a
    /// dedicated instruction; user functions, `tools.*`, and `raise` arrive in
    /// later phases.
    pub(super) fn compile_call(&mut self, call: &ast::CallExpression) {
        let span = call.span.start;
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
            if let ast::Expression::StaticMemberExpression(m) = &call.callee {
                if let ast::Expression::Identifier(obj) = &m.object {
                    if Builtin::for_namespace(obj.name.as_str(), m.property.name.as_str()).is_some()
                    {
                        return self.compile_namespace_call(
                            obj.name.as_str(),
                            m.property.name.as_str(),
                            &argv,
                            span,
                        );
                    }
                }
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
            ast::Expression::StaticMemberExpression(m) => {
                let method = m.property.name.as_str();
                // A leading identifier matching a reserved namespace is a static
                // intrinsic; otherwise it is a method on the receiver value.
                if let ast::Expression::Identifier(obj) = &m.object {
                    match obj.name.as_str() {
                        "Math" | "Object" | "JSON" | "Number" | "Array" | "String" | "Map"
                        | "Set" | "console" | "Edit" => {
                            return self.compile_namespace_call(
                                obj.name.as_str(),
                                method,
                                &argv,
                                span,
                            );
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
            other => self.error(other.span().start, "unsupported call target"),
        }
    }

    /// Compile a call with spread arguments: lower to callee expression +
    /// array of args + [`CallSpread`].
    pub(super) fn compile_call_spread(&mut self, call: &ast::CallExpression, span: u32) {
        // `tools.foo(...args)` can't take the value path: `tools` is only
        // valid structurally as an `Invoke` receiver (compiling it as an
        // expression would give a misleading undeclared-variable error), and
        // `Invoke` has a static arg count. Reject with a targeted error.
        if let ast::Expression::StaticMemberExpression(m) = &call.callee {
            if let ast::Expression::Identifier(obj) = &m.object {
                if obj.name.as_str() == "tools" {
                    self.error(span, "spread arguments are not supported on tool calls");
                    return;
                }
            }
        }

        // Detect a method callee so we emit ObjPeek/ObjPeekDyn (keeping the
        // receiver) instead of ObjGet/IndexGet (consuming it), and thread
        // has_this=true to CallSpread.  Skip namespaces (Math.max) — those
        // are builtins accessed via ObjGet.
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
                    self.emit(Instr::ObjPeek(m.property.name.as_str().into()), span);
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
    pub(super) fn compile_call_args_array(
        &mut self,
        args: &oxc_allocator::Vec<'_, ast::Argument>,
        span: u32,
    ) {
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
        span: u32,
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
        span: u32,
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
        span: u32,
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
        span: u32,
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
    pub(super) fn compile_global_call(&mut self, name: &str, argv: &[&ast::Expression], span: u32) {
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
                            other.span().start,
                            "`raise` condition name must be a string literal",
                        );
                        return;
                    }
                };
                if argv.len() >= 2 {
                    // Payload: compile the expression, emit Raise with argc=1.
                    self.compile_expr(&argv[1]);
                    self.emit(Instr::Raise(name, 1), span);
                } else {
                    self.emit(Instr::Raise(name, 0), span);
                }
            }
            _ => self.error(
                span,
                format!("call to undeclared function `{name}` (user functions are Phase 3)"),
            ),
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
        span: u32,
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
            // an `Object` receiver reroutes (`MethodOnObject`) to a same-named
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

    /// Compile a method call where the method name is not a known builtin.
    /// Lowers `recv.method(args)` to: evaluate recv, get property `method`,
    /// evaluate args, then CallDyn.
    pub(super) fn compile_dynamic_method_call(
        &mut self,
        recv: &ast::Expression,
        method: &str,
        argv: &[&ast::Expression],
        span: u32,
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
        span: u32,
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

    /// `arr.reduce(cb[, init])`. The two JS forms map to two helpers: with an
    /// initial value → `__reduce(a, f, acc)`; without → `__reduce1(a, f)`
    /// (seeded from element 0).
    pub(super) fn compile_reduce(
        &mut self,
        recv: &ast::Expression,
        argv: &[&ast::Expression],
        span: u32,
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
        span: u32,
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
        span: u32,
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
        span: u32,
    ) {
        // A constant function (Phase F): no slot — call its label statically.
        // Pad missing args to the declared arity (as the slotted path does).
        if let Some(ConstValue::Fn { label, arity }) = self.const_ref(callee_span) {
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
