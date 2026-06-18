use indexmap::IndexMap;
use oxc_ast::ast;

use super::Analyzer;
use super::const_fns::{ConstValue, literal_const_value};
use super::scope::{FuncScope, ParamInfo, SlotInfo};
use super::{BlockScopes, NameRes};

pub(super) struct BindingCtx<'a> {
    is_const: bool,
    is_var: bool,
    scope: &'a mut FuncScope,
    block_scopes: &'a mut BlockScopes,
    next_slot: &'a mut u32,
}

impl Analyzer {
    /// Pre-register function-scoped names (`var` bindings and function
    /// declarations) so forward references resolve, mirroring JS hoisting.
    /// Recurses through blocks/conditionals/loops but never into nested
    /// functions. Binding spans/slots are recorded here; the declaration site
    /// reuses the same slot.
    pub(super) fn analyze_hoist(
        &mut self,
        stmts: &[ast::Statement],
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        next_slot: &mut u32,
    ) {
        for stmt in stmts {
            self.analyze_hoist_stmt(stmt, scope, block_scopes, next_slot);
        }
    }

    pub(super) fn analyze_hoist_stmt(
        &mut self,
        stmt: &ast::Statement,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        next_slot: &mut u32,
    ) {
        match stmt {
            ast::Statement::VariableDeclaration(decl)
                if decl.kind == ast::VariableDeclarationKind::Var =>
            {
                for d in &decl.declarations {
                    self.hoist_var_pattern(&d.id, scope, block_scopes, next_slot);
                }
            }
            ast::Statement::FunctionDeclaration(f) => {
                if let Some(id) = &f.id {
                    self.analyze_register_name(
                        id.name.as_str(),
                        id.span.start,
                        &mut BindingCtx {
                            is_const: false,
                            is_var: true,
                            scope,
                            block_scopes,
                            next_slot,
                        },
                    );
                }
            }
            ast::Statement::BlockStatement(b) => {
                self.analyze_hoist(&b.body, scope, block_scopes, next_slot)
            }
            ast::Statement::IfStatement(s) => {
                self.analyze_hoist_stmt(&s.consequent, scope, block_scopes, next_slot);
                if let Some(alt) = &s.alternate {
                    self.analyze_hoist_stmt(alt, scope, block_scopes, next_slot);
                }
            }
            ast::Statement::WhileStatement(s) => {
                self.analyze_hoist_stmt(&s.body, scope, block_scopes, next_slot)
            }
            ast::Statement::DoWhileStatement(s) => {
                self.analyze_hoist_stmt(&s.body, scope, block_scopes, next_slot)
            }
            ast::Statement::ForStatement(s) => {
                if let Some(ast::ForStatementInit::VariableDeclaration(decl)) = &s.init
                    && decl.kind == ast::VariableDeclarationKind::Var
                {
                    for d in &decl.declarations {
                        self.hoist_var_pattern(&d.id, scope, block_scopes, next_slot);
                    }
                }
                self.analyze_hoist_stmt(&s.body, scope, block_scopes, next_slot);
            }
            ast::Statement::ForOfStatement(s) => {
                if let ast::ForStatementLeft::VariableDeclaration(decl) = &s.left
                    && decl.kind == ast::VariableDeclarationKind::Var
                {
                    for d in &decl.declarations {
                        self.hoist_var_pattern(&d.id, scope, block_scopes, next_slot);
                    }
                }
                self.analyze_hoist_stmt(&s.body, scope, block_scopes, next_slot);
            }
            ast::Statement::ForInStatement(s) => {
                if let ast::ForStatementLeft::VariableDeclaration(decl) = &s.left
                    && decl.kind == ast::VariableDeclarationKind::Var
                {
                    for d in &decl.declarations {
                        self.hoist_var_pattern(&d.id, scope, block_scopes, next_slot);
                    }
                }
                self.analyze_hoist_stmt(&s.body, scope, block_scopes, next_slot);
            }
            ast::Statement::SwitchStatement(s) => {
                // `var` declarations inside case clauses are function-scoped.
                for case in &s.cases {
                    for cs in &case.consequent {
                        self.analyze_hoist_stmt(cs, scope, block_scopes, next_slot);
                    }
                }
            }
            ast::Statement::TryStatement(t) => {
                self.analyze_hoist(&t.block.body, scope, block_scopes, next_slot);
                if let Some(h) = &t.handler {
                    self.analyze_hoist(&h.body.body, scope, block_scopes, next_slot);
                }
                if let Some(f) = &t.finalizer {
                    self.analyze_hoist(&f.body, scope, block_scopes, next_slot);
                }
            }
            _ => {}
        }
    }

    /// Register every binding identifier in a `var` pattern (function-scoped).
    pub(super) fn hoist_var_pattern(
        &mut self,
        pat: &ast::BindingPattern,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        next_slot: &mut u32,
    ) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                self.analyze_register_name(
                    id.name.as_str(),
                    id.span.start,
                    &mut BindingCtx {
                        is_const: false,
                        is_var: true,
                        scope,
                        block_scopes,
                        next_slot,
                    },
                );
            }
            ast::BindingPattern::AssignmentPattern(ap) => {
                self.hoist_var_pattern(&ap.left, scope, block_scopes, next_slot)
            }
            ast::BindingPattern::ArrayPattern(arr) => {
                for el in arr.elements.iter().flatten() {
                    self.hoist_var_pattern(el, scope, block_scopes, next_slot);
                }
                if let Some(rest) = &arr.rest {
                    self.hoist_var_pattern(&rest.argument, scope, block_scopes, next_slot);
                }
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.hoist_var_pattern(&prop.value, scope, block_scopes, next_slot);
                }
                if let Some(rest) = &obj.rest {
                    self.hoist_var_pattern(&rest.argument, scope, block_scopes, next_slot);
                }
            }
        }
    }

    pub(super) fn analyze_stmts(
        &mut self,
        stmts: &[ast::Statement],
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        for stmt in stmts {
            self.analyze_stmt(stmt, scope, block_scopes, next_slot, scopes);
        }
    }

    pub(super) fn analyze_stmt(
        &mut self,
        stmt: &ast::Statement,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        match stmt {
            ast::Statement::VariableDeclaration(decl) => {
                self.analyze_var_decl(decl, scope, block_scopes, next_slot, scopes);
            }
            ast::Statement::FunctionDeclaration(f) => {
                // Name already hoisted; build the function's scope.
                let child = self.build_function_scope(f, true, false, scopes);
                scope.children.push(child);
            }
            ast::Statement::ClassDeclaration(c) => {
                // A class declaration binds its name (block-scoped, like `let`)
                // in the enclosing scope, then builds its constructor/method
                // scopes.
                if let Some(id) = &c.id {
                    self.analyze_register_name(
                        id.name.as_str(),
                        id.span.start,
                        &mut BindingCtx {
                            is_const: false,
                            is_var: false,
                            scope,
                            block_scopes,
                            next_slot,
                        },
                    );
                }
                self.build_class_scopes(c, scope, block_scopes, scopes);
            }
            ast::Statement::BlockStatement(block) => {
                block_scopes.push(IndexMap::new());
                self.analyze_stmts(&block.body, scope, block_scopes, next_slot, scopes);
                block_scopes.pop();
            }
            ast::Statement::IfStatement(s) => {
                self.analyze_expr(&s.test, scope, block_scopes, scopes);
                self.analyze_stmt(&s.consequent, scope, block_scopes, next_slot, scopes);
                if let Some(alt) = &s.alternate {
                    self.analyze_stmt(alt, scope, block_scopes, next_slot, scopes);
                }
            }
            ast::Statement::WhileStatement(s) => {
                self.analyze_expr(&s.test, scope, block_scopes, scopes);
                self.loop_depth += 1;
                self.analyze_stmt(&s.body, scope, block_scopes, next_slot, scopes);
                self.loop_depth -= 1;
            }
            ast::Statement::DoWhileStatement(s) => {
                self.loop_depth += 1;
                self.analyze_stmt(&s.body, scope, block_scopes, next_slot, scopes);
                self.loop_depth -= 1;
                self.analyze_expr(&s.test, scope, block_scopes, scopes);
            }
            ast::Statement::ForStatement(s) => {
                // The head declaration and body are all per-iteration: a `let`
                // declared in the head (`for (let i …)`) is loop-declared too.
                self.loop_depth += 1;
                if let Some(init) = &s.init {
                    match init {
                        ast::ForStatementInit::VariableDeclaration(decl) => {
                            self.analyze_var_decl(decl, scope, block_scopes, next_slot, scopes);
                        }
                        _ => {
                            if let Some(expr) = init.as_expression() {
                                self.analyze_expr(expr, scope, block_scopes, scopes);
                            }
                        }
                    }
                }
                if let Some(test) = &s.test {
                    self.analyze_expr(test, scope, block_scopes, scopes);
                }
                if let Some(update) = &s.update {
                    self.analyze_expr(update, scope, block_scopes, scopes);
                }
                self.analyze_stmt(&s.body, scope, block_scopes, next_slot, scopes);
                self.loop_depth -= 1;
            }
            ast::Statement::ExpressionStatement(es) => {
                self.analyze_expr(&es.expression, scope, block_scopes, scopes);
            }
            ast::Statement::ReturnStatement(r) => {
                if let Some(val) = &r.argument {
                    self.analyze_expr(val, scope, block_scopes, scopes);
                }
            }
            ast::Statement::BreakStatement(_)
            | ast::Statement::ContinueStatement(_)
            | ast::Statement::EmptyStatement(_) => {}
            // for-of / for-in: walk the iterable/object RHS, declare the loop
            // binding (let/const -> block slot; var was hoisted), then the body.
            // A fresh block scope wraps the head + body so the loop binding does
            // not leak past the loop.
            ast::Statement::ForOfStatement(s) => {
                self.analyze_expr(&s.right, scope, block_scopes, scopes);
                block_scopes.push(IndexMap::new());
                self.loop_depth += 1;
                self.analyze_for_head(&s.left, scope, block_scopes, next_slot, scopes);
                self.analyze_stmt(&s.body, scope, block_scopes, next_slot, scopes);
                self.loop_depth -= 1;
                block_scopes.pop();
            }
            ast::Statement::ForInStatement(s) => {
                self.analyze_expr(&s.right, scope, block_scopes, scopes);
                block_scopes.push(IndexMap::new());
                self.loop_depth += 1;
                self.analyze_for_head(&s.left, scope, block_scopes, next_slot, scopes);
                self.analyze_stmt(&s.body, scope, block_scopes, next_slot, scopes);
                self.loop_depth -= 1;
                block_scopes.pop();
            }
            // `switch`: the whole body shares **one** lexical block (a `let` in
            // one `case` is visible in later cases), so push a single block scope
            // around all the case tests and consequents. `break`/`continue`
            // targeting is handled in codegen (break-only context for the switch).
            ast::Statement::SwitchStatement(s) => {
                self.analyze_expr(&s.discriminant, scope, block_scopes, scopes);
                block_scopes.push(IndexMap::new());
                for case in &s.cases {
                    if let Some(test) = &case.test {
                        self.analyze_expr(test, scope, block_scopes, scopes);
                    }
                    for cs in &case.consequent {
                        self.analyze_stmt(cs, scope, block_scopes, next_slot, scopes);
                    }
                }
                block_scopes.pop();
            }
            ast::Statement::ThrowStatement(t) => {
                self.analyze_expr(&t.argument, scope, block_scopes, scopes);
            }
            // `try`/`catch`: the try block, the catch body, and the finalizer
            // are each their own lexical block. The catch binding (if any) is
            // block-scoped to the catch body and declared like a `let` (a
            // destructuring pattern declares its leaves the same way).
            ast::Statement::TryStatement(t) => {
                block_scopes.push(IndexMap::new());
                self.analyze_stmts(&t.block.body, scope, block_scopes, next_slot, scopes);
                block_scopes.pop();
                if let Some(h) = &t.handler {
                    block_scopes.push(IndexMap::new());
                    if let Some(param) = &h.param {
                        self.analyze_declare_pattern(
                            &param.pattern,
                            &mut BindingCtx {
                                is_const: false,
                                is_var: false,
                                scope,
                                block_scopes,
                                next_slot,
                            },
                            scopes,
                        );
                    }
                    self.analyze_stmts(&h.body.body, scope, block_scopes, next_slot, scopes);
                    block_scopes.pop();
                }
                // A finalizer is a compile error in codegen, but walk it so
                // diagnostics aggregate sensibly.
                if let Some(f) = &t.finalizer {
                    block_scopes.push(IndexMap::new());
                    self.analyze_stmts(&f.body, scope, block_scopes, next_slot, scopes);
                    block_scopes.pop();
                }
            }
            _ => {}
        }
    }

    /// Declare the loop binding of a `for-of`/`for-in` head. Only the
    /// `let`/`const`/`var x` declaration form is resolved here (the binding
    /// gets a slot exactly like a normal declaration; `var` was already
    /// hoisted). The bare-assignment-target form (`for (x of …)`) is left
    /// unresolved — codegen rejects it.
    pub(super) fn analyze_for_head(
        &mut self,
        left: &ast::ForStatementLeft,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        if let ast::ForStatementLeft::VariableDeclaration(decl) = left {
            self.analyze_var_decl(decl, scope, block_scopes, next_slot, scopes);
        }
    }

    pub(super) fn analyze_var_decl(
        &mut self,
        decl: &ast::VariableDeclaration,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        next_slot: &mut u32,
        scopes: &mut Vec<FuncScope>,
    ) {
        let is_const = decl.kind == ast::VariableDeclarationKind::Const;
        let is_var = decl.kind == ast::VariableDeclarationKind::Var;
        for d in &decl.declarations {
            // Constant-binding elimination: `const x = <literal>` is a compile-time
            // binding — it occupies no slot and is never captured; references
            // resolve to the value. The literal initializer has no refs/effects,
            // so it is not analyzed. (host-seeded consts may not be shadowed.)
            if is_const
                && let ast::BindingPattern::BindingIdentifier(id) = &d.id
                && !crate::is_host_const(&id.name)
                && let Some(value) = d.init.as_ref().and_then(literal_const_value)
            {
                self.analyze_register_const(id.name.as_str(), value, scope, block_scopes);
                continue;
            }
            // `var` names were hoisted; `let`/`const` register here.
            self.analyze_declare_pattern(
                &d.id,
                &mut BindingCtx {
                    is_const,
                    is_var,
                    scope,
                    block_scopes,
                    next_slot,
                },
                scopes,
            );
            if let Some(init) = &d.init {
                self.analyze_expr(init, scope, block_scopes, scopes);
                // `let`/`const NAME = <fn-expr>`: link the function-expression
                // scope to its binding, so a non-capturing one bound to an
                // immutable (never-reassigned) name becomes a constant function
                // (Phase F), like a declaration. `var` is excluded — its hoisted-
                // `undefined` value means a pre-assignment reference isn't the
                // function. Reassignment is checked later (in the fixpoint).
                if !is_var && let ast::BindingPattern::BindingIdentifier(id) = &d.id {
                    let fn_span = match init {
                        ast::Expression::ArrowFunctionExpression(a) => Some(a.span.start),
                        ast::Expression::FunctionExpression(f) => Some(f.span.start),
                        _ => None,
                    };
                    if let Some(fn_span) = fn_span
                        && let Some(s) = scopes.iter_mut().find(|s| s.node_span == fn_span)
                    {
                        s.binding_name = Some(id.name.to_string());
                    }
                }
            }
        }
    }

    /// Register a `const x = <literal>` as a compile-time binding: visible by name
    /// (for resolution + shadowing) in the current block scope and in this
    /// function's `const_names` (for cross-function resolution), but with no slot.
    pub(super) fn analyze_register_const(
        &mut self,
        name: &str,
        value: ConstValue,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
    ) {
        block_scopes
            .last_mut()
            .expect("a block scope is always open")
            .insert(name.to_string(), NameRes::Const(value.clone()));
        scope.const_names.entry(name.to_string()).or_insert(value);
    }

    /// Register `let`/`const` binding names (skipped for already-hoisted `var`s)
    /// and analyze any pattern default expressions for free variables.
    pub(super) fn analyze_declare_pattern(
        &mut self,
        pat: &ast::BindingPattern,
        ctx: &mut BindingCtx<'_>,
        scopes: &mut Vec<FuncScope>,
    ) {
        match pat {
            ast::BindingPattern::BindingIdentifier(id) => {
                if !ctx.is_var {
                    self.analyze_register_name(id.name.as_str(), id.span.start, ctx);
                }
            }
            ast::BindingPattern::AssignmentPattern(ap) => {
                self.analyze_declare_pattern(&ap.left, ctx, scopes);
                self.analyze_expr(&ap.right, ctx.scope, ctx.block_scopes, scopes);
            }
            ast::BindingPattern::ArrayPattern(arr) => {
                for el in arr.elements.iter().flatten() {
                    self.analyze_declare_pattern(el, ctx, scopes);
                }
                if let Some(rest) = &arr.rest {
                    self.analyze_declare_pattern(&rest.argument, ctx, scopes);
                }
            }
            ast::BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    if prop.computed
                        && let Some(expr) = prop.key.as_expression()
                    {
                        self.analyze_expr(expr, ctx.scope, ctx.block_scopes, scopes);
                    }
                    self.analyze_declare_pattern(&prop.value, ctx, scopes);
                }
                if let Some(rest) = &obj.rest {
                    self.analyze_declare_pattern(&rest.argument, ctx, scopes);
                }
            }
        }
    }

    /// Register a binding name, recording its span→slot mapping. `var` names
    /// live in the function scope (`block_scopes[0]`) and reuse an existing
    /// slot; `let`/`const` get a fresh slot in the innermost block.
    pub(super) fn analyze_register_name(
        &mut self,
        name: &str,
        span: u32,
        ctx: &mut BindingCtx<'_>,
    ) -> u32 {
        if crate::is_host_const(name) {
            self.error(
                span,
                format!("cannot shadow the host-seeded `{name}` object"),
            );
            return 0;
        }
        let slot = if ctx.is_var {
            if let Some(NameRes::Slot { slot, .. }) = ctx.block_scopes[0].get(name) {
                *slot
            } else {
                let slot = *ctx.next_slot;
                *ctx.next_slot += 1;
                ctx.block_scopes[0].insert(
                    name.to_string(),
                    NameRes::Slot {
                        slot,
                        is_const: false,
                    },
                );
                ctx.scope.names.entry(name.to_string()).or_insert(SlotInfo {
                    slot,
                    is_const: false,
                });
                slot
            }
        } else {
            let slot = *ctx.next_slot;
            *ctx.next_slot += 1;
            ctx.block_scopes
                .last_mut()
                .expect("a block scope is always open")
                .insert(
                    name.to_string(),
                    NameRes::Slot {
                        slot,
                        is_const: ctx.is_const,
                    },
                );
            ctx.scope.names.entry(name.to_string()).or_insert(SlotInfo {
                slot,
                is_const: ctx.is_const,
            });
            // A `let`/`const` declared inside a loop is a per-iteration binding;
            // record it so a captured one becomes `fresh_owns` (Plain + per-iter
            // FreshCell) rather than an eagerly-boxed shared cell.
            if self.loop_depth > 0 {
                ctx.scope.loop_declared.insert(slot);
            }
            slot
        };
        // Record the slot's name for the debug table (keep the first name
        // when `var` re-declarations reuse a slot).
        if ctx.scope.own_slot_names.len() <= slot as usize {
            ctx.scope.own_slot_names.resize(slot as usize + 1, None);
        }
        ctx.scope.own_slot_names[slot as usize].get_or_insert_with(|| name.to_string());
        ctx.scope.binding_spans.push((span, slot, ctx.is_const));
        slot
    }

    pub(super) fn analyze_expr(
        &mut self,
        expr: &ast::Expression,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        scopes: &mut Vec<FuncScope>,
    ) {
        match expr {
            ast::Expression::Identifier(id) => {
                self.analyze_ref(id.name.as_str(), id.span.start, scope, block_scopes);
            }
            ast::Expression::AssignmentExpression(a) => {
                self.analyze_assign_target(&a.left, scope, block_scopes, scopes);
                self.analyze_expr(&a.right, scope, block_scopes, scopes);
            }
            ast::Expression::UpdateExpression(u) => {
                self.analyze_simple_target(&u.argument, scope, block_scopes, scopes);
            }
            ast::Expression::BinaryExpression(b) => {
                self.analyze_expr(&b.left, scope, block_scopes, scopes);
                self.analyze_expr(&b.right, scope, block_scopes, scopes);
            }
            ast::Expression::UnaryExpression(u) => {
                self.analyze_expr(&u.argument, scope, block_scopes, scopes);
            }
            ast::Expression::LogicalExpression(l) => {
                self.analyze_expr(&l.left, scope, block_scopes, scopes);
                self.analyze_expr(&l.right, scope, block_scopes, scopes);
            }
            ast::Expression::ConditionalExpression(c) => {
                self.analyze_expr(&c.test, scope, block_scopes, scopes);
                self.analyze_expr(&c.consequent, scope, block_scopes, scopes);
                self.analyze_expr(&c.alternate, scope, block_scopes, scopes);
            }
            ast::Expression::CallExpression(c) => {
                self.analyze_expr(&c.callee, scope, block_scopes, scopes);
                for arg in &c.arguments {
                    match arg {
                        ast::Argument::SpreadElement(s) => {
                            self.analyze_expr(&s.argument, scope, block_scopes, scopes);
                        }
                        _ => {
                            if let Some(e) = arg.as_expression() {
                                self.analyze_expr(e, scope, block_scopes, scopes);
                            }
                        }
                    }
                }
            }
            ast::Expression::StaticMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
            }
            ast::Expression::ComputedMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
                self.analyze_expr(&m.expression, scope, block_scopes, scopes);
            }
            ast::Expression::ArrayExpression(arr) => {
                for el in &arr.elements {
                    match el {
                        ast::ArrayExpressionElement::SpreadElement(s) => {
                            self.analyze_expr(&s.argument, scope, block_scopes, scopes);
                        }
                        _ => {
                            if let Some(e) = el.as_expression() {
                                self.analyze_expr(e, scope, block_scopes, scopes);
                            }
                        }
                    }
                }
            }
            ast::Expression::ObjectExpression(obj) => {
                for prop in &obj.properties {
                    match prop {
                        ast::ObjectPropertyKind::ObjectProperty(p) => {
                            if p.computed
                                && let Some(e) = p.key.as_expression()
                            {
                                self.analyze_expr(e, scope, block_scopes, scopes);
                            }
                            self.analyze_expr(&p.value, scope, block_scopes, scopes);
                        }
                        ast::ObjectPropertyKind::SpreadProperty(s) => {
                            self.analyze_expr(&s.argument, scope, block_scopes, scopes);
                        }
                    }
                }
            }
            ast::Expression::TemplateLiteral(tl) => {
                for e in &tl.expressions {
                    self.analyze_expr(e, scope, block_scopes, scopes);
                }
            }
            ast::Expression::SequenceExpression(seq) => {
                for e in &seq.expressions {
                    self.analyze_expr(e, scope, block_scopes, scopes);
                }
            }
            ast::Expression::ParenthesizedExpression(p) => {
                self.analyze_expr(&p.expression, scope, block_scopes, scopes);
            }
            ast::Expression::AwaitExpression(a) => {
                self.analyze_expr(&a.argument, scope, block_scopes, scopes);
            }
            ast::Expression::ChainExpression(chain) => {
                self.analyze_chain_element(&chain.expression, scope, block_scopes, scopes);
            }
            ast::Expression::FunctionExpression(f) => {
                let child = self.build_function_scope(f, false, false, scopes);
                scope.children.push(child);
            }
            ast::Expression::ArrowFunctionExpression(a) => {
                let child = self.build_arrow_scope(a, scopes);
                scope.children.push(child);
            }
            ast::Expression::ClassExpression(c) => {
                // A class expression binds no name in the enclosing scope; just
                // build its constructor/method scopes.
                self.build_class_scopes(c, scope, block_scopes, scopes);
            }
            ast::Expression::NewExpression(n) => {
                // Visit the callee (Step 4b: user functions can now appear in
                // `new` expressions) and all arguments so their references
                // resolve.
                self.analyze_expr(&n.callee, scope, block_scopes, scopes);
                for arg in &n.arguments {
                    match arg {
                        ast::Argument::SpreadElement(s) => {
                            self.analyze_expr(&s.argument, scope, block_scopes, scopes);
                        }
                        _ => {
                            if let Some(e) = arg.as_expression() {
                                self.analyze_expr(e, scope, block_scopes, scopes);
                            }
                        }
                    }
                }
            }
            ast::Expression::ThisExpression(t) => {
                scope.free_refs.push((t.span.start, "<this>".to_string()));
                scope.free_vars.insert("<this>".to_string());
            }
            ast::Expression::Super(s) => {
                // `super` (as `super(...)` callee or `super.m` member object).
                // Resolve to the enclosing derived class's superclass binding so
                // the parent constructor is captured as an upval and read at the
                // `super` use site (keyed by this node's span). Outside a derived
                // class it is a no-op here; codegen reports the error.
                if let Some(name) = self.current_super.clone() {
                    self.analyze_ref(&name, s.span.start, scope, block_scopes);
                }
            }
            _ => {}
        }
    }

    /// Record an identifier reference: to an own local (resolved now) or as a
    /// free variable (resolved to an upval/self/global during finalization).
    pub(super) fn analyze_ref(
        &mut self,
        name: &str,
        span: u32,
        scope: &mut FuncScope,
        block_scopes: &BlockScopes,
    ) {
        match self.analyze_resolve_name(name, block_scopes) {
            Some(NameRes::Slot { slot, is_const }) => {
                scope.local_refs.push((span, slot, is_const));
                return;
            }
            // A compile-time const binding: this reference resolves to the value
            // (the compiler emits the literal); it never becomes a free var, so
            // const bindings are never captured.
            Some(NameRes::Const(value)) => {
                scope.const_refs.push((span, value));
                return;
            }
            None => {}
        }
        {
            // An unshadowed `arguments` reference uses the frame's argument array
            // (the compiler emits `Arguments`, not a `Local`); flag the scope so
            // the prologue materializes that array before normalizing the args.
            if name == "arguments" {
                scope.uses_arguments = true;
            }
            scope.free_refs.push((span, name.to_string()));
            scope.free_vars.insert(name.to_string());
        }
    }

    /// Walk an assignment target, recording references (the written identifier,
    /// plus any member-object / computed-key / default expressions).
    pub(super) fn analyze_assign_target(
        &mut self,
        target: &ast::AssignmentTarget,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        scopes: &mut Vec<FuncScope>,
    ) {
        match target {
            ast::AssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.analyze_ref(id.name.as_str(), id.span.start, scope, block_scopes);
                // Record the write so the binding isn't treated as immutable. A
                // write to a const resolves to no slot (the compiler rejects it).
                if let Some(NameRes::Slot { slot, .. }) =
                    self.analyze_resolve_name(id.name.as_str(), block_scopes)
                {
                    scope.reassigned.insert(slot);
                }
            }
            ast::AssignmentTarget::StaticMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
            }
            ast::AssignmentTarget::ComputedMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
                self.analyze_expr(&m.expression, scope, block_scopes, scopes);
            }
            ast::AssignmentTarget::ArrayAssignmentTarget(arr) => {
                for el in arr.elements.iter().flatten() {
                    self.analyze_assign_maybe_default(el, scope, block_scopes, scopes);
                }
                if let Some(rest) = &arr.rest {
                    self.analyze_assign_target(&rest.target, scope, block_scopes, scopes);
                }
            }
            ast::AssignmentTarget::ObjectAssignmentTarget(obj) => {
                for prop in &obj.properties {
                    match prop {
                        ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                            self.analyze_ref(
                                p.binding.name.as_str(),
                                p.binding.span.start,
                                scope,
                                block_scopes,
                            );
                            if let Some(init) = &p.init {
                                self.analyze_expr(init, scope, block_scopes, scopes);
                            }
                        }
                        ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                            if p.computed
                                && let Some(e) = p.name.as_expression()
                            {
                                self.analyze_expr(e, scope, block_scopes, scopes);
                            }
                            self.analyze_assign_maybe_default(
                                &p.binding,
                                scope,
                                block_scopes,
                                scopes,
                            );
                        }
                    }
                }
                if let Some(rest) = &obj.rest {
                    self.analyze_assign_target(&rest.target, scope, block_scopes, scopes);
                }
            }
            _ => {}
        }
    }

    pub(super) fn analyze_assign_maybe_default(
        &mut self,
        m: &ast::AssignmentTargetMaybeDefault,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        scopes: &mut Vec<FuncScope>,
    ) {
        match m {
            ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(wd) => {
                self.analyze_expr(&wd.init, scope, block_scopes, scopes);
                self.analyze_assign_target(&wd.binding, scope, block_scopes, scopes);
            }
            other => {
                if let Some(t) = other.as_assignment_target() {
                    self.analyze_assign_target(t, scope, block_scopes, scopes);
                }
            }
        }
    }

    /// Like `analyze_assign_target`, for the `SimpleAssignmentTarget` of `++`/`--`.
    pub(super) fn analyze_simple_target(
        &mut self,
        target: &ast::SimpleAssignmentTarget,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        scopes: &mut Vec<FuncScope>,
    ) {
        match target {
            ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                self.analyze_ref(id.name.as_str(), id.span.start, scope, block_scopes);
                // Record the `++`/`--` write so the binding isn't immutable.
                if let Some(NameRes::Slot { slot, .. }) =
                    self.analyze_resolve_name(id.name.as_str(), block_scopes)
                {
                    scope.reassigned.insert(slot);
                }
            }
            ast::SimpleAssignmentTarget::StaticMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
            }
            ast::SimpleAssignmentTarget::ComputedMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
                self.analyze_expr(&m.expression, scope, block_scopes, scopes);
            }
            _ => {}
        }
    }

    /// Walk a `ChainElement` (optional chaining) for references.
    pub(super) fn analyze_chain_element(
        &mut self,
        el: &ast::ChainElement,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        scopes: &mut Vec<FuncScope>,
    ) {
        match el {
            ast::ChainElement::CallExpression(c) => {
                self.analyze_expr(&c.callee, scope, block_scopes, scopes);
                for arg in &c.arguments {
                    match arg {
                        ast::Argument::SpreadElement(s) => {
                            self.analyze_expr(&s.argument, scope, block_scopes, scopes);
                        }
                        _ => {
                            if let Some(e) = arg.as_expression() {
                                self.analyze_expr(e, scope, block_scopes, scopes);
                            }
                        }
                    }
                }
            }
            ast::ChainElement::StaticMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
            }
            ast::ChainElement::ComputedMemberExpression(m) => {
                self.analyze_expr(&m.object, scope, block_scopes, scopes);
                self.analyze_expr(&m.expression, scope, block_scopes, scopes);
            }
            _ => {}
        }
    }

    /// Resolve a name against the current function's block scopes (innermost
    /// first): a frame slot, a compile-time const, or `None` if not local.
    pub(super) fn analyze_resolve_name(
        &self,
        name: &str,
        block_scopes: &BlockScopes,
    ) -> Option<NameRes> {
        block_scopes.iter().rev().find_map(|s| s.get(name).cloned())
    }

    /// Shared body of `build_function_scope` / `build_arrow_scope`: seed the
    /// param slots, analyze param defaults, hoist, then walk the body.
    pub(super) fn analyze_function_body(
        &mut self,
        scope: &mut FuncScope,
        params: Option<&ast::FormalParameters>,
        body: &[ast::Statement],
        field_inits: &[&ast::Expression],
        scopes: &mut Vec<FuncScope>,
    ) {
        let mut block_scopes: BlockScopes = vec![IndexMap::new()];
        let mut next_slot = scope.params.len() as u32;
        // A nested function is a fresh frame: its bindings are not per-iteration
        // with respect to any loop enclosing the *definition*. Reset loop depth
        // for the whole body walk (including pattern-param bindings, which must
        // not be marked loop-declared) and restore it afterwards.
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        for (i, p) in scope.params.iter().enumerate() {
            if p.name.is_empty() {
                // Destructuring param: anonymous slot, bindings declared below.
                continue;
            }
            block_scopes[0].insert(
                p.name.clone(),
                NameRes::Slot {
                    slot: i as u32,
                    is_const: false,
                },
            );
            scope.names.insert(
                p.name.clone(),
                SlotInfo {
                    slot: i as u32,
                    is_const: false,
                },
            );
        }
        // Param patterns (skipped entirely for a default constructor, which has
        // no params node).
        if let Some(params) = params {
            // Destructuring params: each pattern's bindings are ordinary own
            // locals (the compiler's prologue destructures the anonymous param
            // slot into them). This also analyzes inner pattern defaults
            // (`{a = 1}`).
            for p in &params.items {
                if !matches!(&p.pattern, ast::BindingPattern::BindingIdentifier(_)) {
                    self.analyze_declare_pattern(
                        &p.pattern,
                        &mut BindingCtx {
                            is_const: false,
                            is_var: false,
                            scope: &mut *scope,
                            block_scopes: &mut block_scopes,
                            next_slot: &mut next_slot,
                        },
                        scopes,
                    );
                }
            }
            if let Some(rest) = &params.rest
                && !matches!(
                    &rest.rest.argument,
                    ast::BindingPattern::BindingIdentifier(_)
                )
            {
                self.analyze_declare_pattern(
                    &rest.rest.argument,
                    &mut BindingCtx {
                        is_const: false,
                        is_var: false,
                        scope: &mut *scope,
                        block_scopes: &mut block_scopes,
                        next_slot: &mut next_slot,
                    },
                    scopes,
                );
            }
            // Param default expressions (`function f(a, b = a)`) — params are now
            // in scope, so a default may reference an earlier one.
            for p in &params.items {
                if let Some(init) = &p.initializer {
                    self.analyze_expr(init, scope, &mut block_scopes, scopes);
                }
            }
        }
        self.analyze_hoist(body, scope, &mut block_scopes, &mut next_slot);
        self.analyze_stmts(body, scope, &mut block_scopes, &mut next_slot, scopes);
        // Instance-field initializers (class only): analyzed in the constructor
        // scope, after the body's bindings, so a field's `this`/captures resolve
        // here. They declare no locals of their own.
        for init in field_inits {
            self.analyze_expr(init, scope, &mut block_scopes, scopes);
        }
        self.loop_depth = saved_loop_depth;
        scope.own_local_count = next_slot;
    }

    /// Build ordered `ParamInfo`, one slot per declared parameter (so the
    /// caller's positional argument layout matches the callee's param slots).
    /// A destructuring pattern gets an anonymous slot (empty name — not a legal
    /// identifier, so it can never be referenced or captured); its bindings are
    /// declared as ordinary own locals by `analyze_function_body` and filled by
    /// the compiler's prologue destructuring.
    pub(super) fn collect_params(&self, params: &ast::FormalParameters) -> Vec<ParamInfo> {
        fn param_name(pat: &ast::BindingPattern) -> String {
            match pat {
                ast::BindingPattern::BindingIdentifier(id) => id.name.as_str().to_string(),
                _ => String::new(),
            }
        }
        let mut out = Vec::new();
        for param in &params.items {
            out.push(ParamInfo {
                name: param_name(&param.pattern),
                has_default: param.initializer.is_some(),
                is_rest: false,
            });
        }
        if let Some(rest) = &params.rest {
            out.push(ParamInfo {
                name: param_name(&rest.rest.argument),
                has_default: false,
                is_rest: true,
            });
        }
        out
    }
}
