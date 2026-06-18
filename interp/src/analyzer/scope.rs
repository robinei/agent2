use std::collections::{HashMap, HashSet};

use indexmap::{IndexMap, IndexSet};

use crate::vm::SlotKind;

use oxc_ast::ast;

use super::Analyzer;
use super::BlockScopes;
use super::const_fns::ConstValue;

// ── Analysis structures (Phase 3: functions / closures) ─────────────

/// Per-parameter analysis info.
#[derive(Debug, Clone)]
pub(crate) struct ParamInfo {
    pub(crate) name: String,
    pub(crate) has_default: bool,
    /// True for the synthetic `...rest` parameter (the last entry when present).
    pub(crate) is_rest: bool,
}

/// Pre-computed analysis for one function scope (including the top-level
/// program). The analysis pass walks all nested functions, detects free
/// variables, and determines which slots must be `Boxed` because they are
/// captured by a nested function (conservative: all captured slots are boxed).
#[derive(Debug)]
pub(crate) struct FuncScope {
    /// Unique id (index into the `ProgramAnalysis::scopes` vec).
    pub(crate) id: usize,
    /// Parent scope id (`usize::MAX` for the root program scope).
    pub(crate) parent: usize,
    /// Entry-point label for this function's body.
    pub(crate) label: u32,
    /// Span of the defining function/arrow node (`u32::MAX` for the root), used
    /// to build `scope_by_span` so codegen can find this scope by AST node.
    pub(crate) node_span: u32,
    /// End offset of the defining node's span (`u32::MAX` for the root).
    /// With `node_span`, the source range backing span-based function
    /// attribution in the debug table (see `crate::debuginfo`).
    pub(crate) node_end: u32,
    /// Parameters in order: (name, has_default).
    pub(crate) params: Vec<ParamInfo>,
    /// For a named function expression, the function's own name (visible
    /// inside the body for self-recursion).
    pub(crate) self_name: Option<String>,
    /// If this function scope is the initializer of `let`/`const NAME = <fn-expr>`,
    /// the binding NAME in the enclosing scope. Lets a non-capturing function
    /// *expression* bound to an immutable name become a constant function
    /// (Phase F), like a declaration — provided the binding is never reassigned
    /// (checked separately). `var` is excluded (hoisted-`undefined` semantics).
    /// `None` for declarations and non-simple-bound expressions.
    pub(crate) binding_name: Option<String>,
    /// Whether this is a declaration (hoisted into the prologue).
    pub(crate) is_declaration: bool,
    /// Distinct binding names declared in this scope → (own-slot, is_const).
    /// Own-slot indices are 0-based within own locals (excluding upvals).
    /// Only the *first* occurrence of a shadowed name is kept; per-reference
    /// resolution uses `local_refs`/`binding_spans` (keyed by span), so this
    /// table is consulted only by capture resolution.
    pub(super) names: IndexMap<String, SlotInfo>,
    /// Which own-local slot indices are captured by nested functions (→ Boxed,
    /// unless also `loop_declared`, in which case → per-iteration `fresh_owns`).
    pub(super) captured: HashSet<u32>,
    /// Own-local slot indices that are the target of an assignment/update within
    /// this scope (beyond their declaration). A binding that is NOT reassigned
    /// and NOT captured is immutable in fact — an "effectively-const" `let`/`var`
    /// the compiler may propagate like a `const`. (A binding mutated by a nested
    /// closure is necessarily in `captured`, so the `!reassigned && !captured`
    /// test catches it even though the write isn't recorded here.)
    pub(super) reassigned: HashSet<u32>,
    /// Own-local slot indices for `let`/`const` bindings declared lexically
    /// inside a loop (loop head or loop body). Combined with `captured` this
    /// yields `fresh_owns`.
    pub(super) loop_declared: HashSet<u32>,
    /// Own-local slot indices that are captured AND loop-declared: each loop
    /// iteration gives them a fresh cell (the compiler emits `FreshCell`), so
    /// they are allocated `Plain` (no eager cell) rather than `Boxed`. Derived
    /// in `resolve_captures`.
    pub(crate) fresh_owns: HashSet<u32>,
    /// Nested function scope ids.
    pub(crate) children: Vec<usize>,
    /// Free variables: names referenced but not declared in this scope. Drives
    /// bottom-up capture propagation. `IndexSet` for deterministic upval order.
    pub(super) free_vars: IndexSet<String>,
    /// Binding occurrences declared here: (binding span, own-slot). Finalized
    /// into `ProgramAnalysis::binding_slot` (own-slot → absolute) after capture
    /// resolution fixes `upval_count`.
    pub(super) binding_spans: Vec<(u32, u32, bool)>,
    /// Own-local slot → declared name, recorded at slot allocation. Unlike
    /// `names` this keeps shadowed re-declarations (each has its own slot).
    /// Debug info only (see `debug_slot_names`); params are filled from
    /// `params` there, so entries `< nparams` stay `None`.
    pub(super) own_slot_names: Vec<Option<String>>,
    /// Identifier references that resolved to an own local: (ref span, own-slot,
    /// is_const). Finalized into `ref_resolution`.
    pub(super) local_refs: Vec<(u32, u32, bool)>,
    /// Identifier references that were free here: (ref span, name). Finalized to
    /// an upval slot (if captured) or the self-reference slot, else dropped.
    pub(super) free_refs: Vec<(u32, String)>,
    /// Compile-time const bindings declared in this scope, by name (mirrors
    /// `names`, but for `const x = <literal>` bindings that occupy no slot).
    /// Consulted by `resolve_captures` so a nested function's reference resolves
    /// to the value instead of becoming a capture.
    pub(super) const_names: IndexMap<String, ConstValue>,
    /// References (by span) that resolved to a const binding *in this scope* —
    /// the compiler emits the literal. Finalized into `ProgramAnalysis::const_refs`.
    pub(super) const_refs: Vec<(u32, ConstValue)>,
    /// Free-variable names that resolved (up the scope chain) to an enclosing
    /// const — so the body's `free_refs` of that name become const refs rather
    /// than upvals. Filled by `resolve_captures`.
    pub(super) const_by_name: HashMap<String, ConstValue>,
    /// Own-local slots that turned out to be **constant functions** (Phase F):
    /// own-slot → `Fn { label, arity }`. A same-scope reference to such a slot
    /// emits the `Fn` literal instead of a `Local`; the binding store is skipped.
    /// Filled after the const-function fixpoint, before `resolve_captures`.
    pub(super) const_fn_slots: HashMap<u32, ConstValue>,
    /// Captured names → (upval slot index, is_const), filled by capture
    /// resolution. Used to finalize `free_refs`.
    pub(super) upval_by_name: HashMap<String, (u32, bool)>,
    /// The capture list: absolute slot indices in the PARENT frame, in upval
    /// order (each becomes one of this closure's leading locals).
    pub(crate) captures: Vec<u32>,
    /// Number of leading upval slots (pre-installed by `CallDyn`/`MakeClosure`).
    pub(crate) upval_count: u32,
    /// Total number of own-local slots (params + declared vars).
    pub(crate) own_local_count: u32,
    /// Whether the body references the special `arguments` array (an `arguments`
    /// identifier that does not resolve to a real binding). Drives eager
    /// materialization of the arguments array in the prologue, before the arg
    /// region is normalized to exactly `nparams`.
    pub(crate) uses_arguments: bool,
    /// Final slot kinds for own locals (params first, then declared vars). The
    /// compiler routes the declared kinds into `EnterFrame`'s `local_kinds` and
    /// boxes any captured params in place via `FreshCell`.
    pub(crate) slot_kinds: Vec<SlotKind>,
    /// Whether this function scope is an arrow function (no own `this`).
    pub(crate) is_arrow: bool,
    /// If this non-arrow scope reifies `this` for arrow capture, the own-local
    /// slot index that holds the reified `this` value. `None` otherwise.
    pub(crate) this_slot: Option<u32>,
    /// Whether a nested arrow scope propagated `<this>` into this scope's
    /// free_vars, requiring reification.  Set during Phase A propagation;
    /// cleared by the fixpoint reset.  A non-arrow scope with `<this>` in its
    /// own free_vars from a *direct* `ThisExpression` (not arrows) does NOT set
    /// this flag and does NOT reify — it just emits `LoadThis`.
    pub(crate) needs_this_reify: bool,
}

/// A resolved local-variable binding: its frame slot plus whether it was
/// declared `const` (so writes can be rejected at compile time).
#[derive(Copy, Clone, Debug)]
pub(crate) struct SlotInfo {
    pub(crate) slot: u32,
    pub(crate) is_const: bool,
}

impl FuncScope {
    pub(crate) fn new(
        parent: usize,
        label: u32,
        node_span: u32,
        node_end: u32,
        params: Vec<ParamInfo>,
        self_name: Option<String>,
        is_declaration: bool,
    ) -> Self {
        FuncScope {
            id: 0,
            parent,
            label,
            node_span,
            node_end,
            params,
            self_name,
            binding_name: None,
            is_declaration,
            names: IndexMap::new(),
            captured: HashSet::new(),
            reassigned: HashSet::new(),
            loop_declared: HashSet::new(),
            fresh_owns: HashSet::new(),
            children: Vec::new(),
            free_vars: IndexSet::new(),
            binding_spans: Vec::new(),
            own_slot_names: Vec::new(),
            local_refs: Vec::new(),
            free_refs: Vec::new(),
            const_names: IndexMap::new(),
            const_refs: Vec::new(),
            const_by_name: HashMap::new(),
            const_fn_slots: HashMap::new(),
            upval_by_name: HashMap::new(),
            captures: Vec::new(),
            upval_count: 0,
            own_local_count: 0,
            uses_arguments: false,
            slot_kinds: Vec::new(),
            is_arrow: false,
            this_slot: None,
            needs_this_reify: false,
        }
    }

    /// Source range of the defining node, for the debug table.
    pub(crate) fn node_range(&self) -> (u32, u32) {
        (self.node_span, self.node_end)
    }

    /// Best-effort function name for the debug table: the declaration /
    /// named-expression name, else the binding name (`const f = () => …`),
    /// else `"<anonymous>"`.
    pub(crate) fn debug_name(&self) -> String {
        self.self_name
            .as_ref()
            .or(self.binding_name.as_ref())
            .cloned()
            .unwrap_or_else(|| "<anonymous>".to_string())
    }

    /// Frame-slot → name table for the debug table (9_TUI Step 1), under
    /// the absolute `[params | upvals | own locals | self?]` layout.
    /// `include_self` adds the self-reference slot's name; the caller
    /// passes `false` for constant functions, whose self slot is never
    /// allocated (they refer to themselves by their `Fn` constant).
    pub(crate) fn debug_slot_names(&self, include_self: bool) -> Vec<Option<String>> {
        let nparams = self.params.len() as u32;
        let with_self = include_self && self.self_name.is_some();
        let total = self.own_local_count + self.upval_count + with_self as u32;
        let mut v: Vec<Option<String>> = vec![None; total as usize];
        for (i, p) in self.params.iter().enumerate() {
            if !p.name.is_empty() {
                v[i] = Some(p.name.clone());
            }
        }
        for (name, &(uidx, _)) in &self.upval_by_name {
            v[(nparams + uidx) as usize] = Some(name.clone());
        }
        for (own, name) in self.own_slot_names.iter().enumerate() {
            if let Some(n) = name {
                v[frame_abs(own as u32, nparams, self.upval_count) as usize] = Some(n.clone());
            }
        }
        if with_self {
            v[(self.own_local_count + self.upval_count) as usize] = self.self_name.clone();
        }
        v
    }

    /// Caller-facing arity: declared params minus the trailing rest param (if
    /// any). Callers must not pad an Undefined for the rest slot — the
    /// prologue builds it from the surplus arguments.
    pub(crate) fn declared_arity(&self) -> u32 {
        match self.params.last() {
            Some(p) if p.is_rest => (self.params.len() - 1) as u32,
            _ => self.params.len() as u32,
        }
    }

    /// JS `Function.prototype.length`: the number of parameters **before the
    /// first one with a default value or the rest parameter** — counting stops
    /// at the first default/rest, and the rest param itself is never counted.
    /// Stored per-`Closure` (Step 6) so `fn.length` reads it off the value.
    pub(crate) fn js_length(&self) -> u16 {
        let mut n = 0u16;
        for p in &self.params {
            if p.has_default || p.is_rest {
                break;
            }
            n += 1;
        }
        n
    }
}

/// Absolute frame slot for an own-local index under the `[params | upvals |
/// locals]` layout: the `nparams` params keep slots `0..nparams` (they arrive in
/// place as the call's arguments), then the `K` upvals occupy `nparams..nparams+K`,
/// then the remaining own locals are shifted up by `K`. (`own == own_local_count`
/// yields the self-reference slot just past all own locals.)
pub(crate) fn frame_abs(own: u32, nparams: u32, upval_count: u32) -> u32 {
    if own < nparams {
        own
    } else {
        own + upval_count
    }
}

// ── Analyzer scope-building methods ────────────────────────────────

impl Analyzer {
    /// Build a `FuncScope` for a function declaration or expression.
    ///
    /// `inherit_super`: a class method keeps the enclosing class's `super`
    /// context (`true`); an ordinary nested function is a `super` boundary and
    /// clears it for its body (`false`), matching JS (a plain function has no
    /// `super`, an arrow inherits it lexically — see `build_arrow_scope`).
    pub(super) fn build_function_scope(
        &mut self,
        func: &ast::Function,
        is_declaration: bool,
        inherit_super: bool,
        scopes: &mut Vec<FuncScope>,
    ) -> usize {
        let label = self.new_label();
        let params = self.collect_params(&func.params);
        let self_name = func.id.as_ref().map(|id| id.name.as_str().to_string());
        let mut scope = FuncScope::new(
            usize::MAX,
            label,
            func.span.start,
            func.span.end,
            params,
            self_name,
            is_declaration,
        );
        if func.params.rest.is_some() {
            scope.uses_arguments = true;
        }
        let body = func.body.as_ref().map(|b| &b.statements[..]).unwrap_or(&[]);
        let saved_super = self.current_super.clone();
        if !inherit_super {
            self.current_super = None;
        }
        self.analyze_function_body(&mut scope, Some(&func.params), body, &[], scopes);
        self.current_super = saved_super;
        self.push_scope(scope, scopes)
    }

    /// Build a `FuncScope` for an arrow function.
    pub(super) fn build_arrow_scope(
        &mut self,
        arrow: &ast::ArrowFunctionExpression,
        scopes: &mut Vec<FuncScope>,
    ) -> usize {
        let label = self.new_label();
        let params = self.collect_params(&arrow.params);
        let mut scope = FuncScope::new(
            usize::MAX,
            label,
            arrow.span.start,
            arrow.span.end,
            params,
            None,
            false,
        );
        scope.is_arrow = true;
        if arrow.params.rest.is_some() {
            scope.uses_arguments = true;
        }
        self.analyze_function_body(
            &mut scope,
            Some(&arrow.params),
            &arrow.body.statements,
            &[],
            scopes,
        );
        self.push_scope(scope, scopes)
    }

    /// Build the constructor and method `FuncScope`s for a class and attach them
    /// as children of the enclosing `scope` (so capture/parent resolution treats
    /// them like any nested function). Instance fields are gathered here and their
    /// initializers analyzed inside the constructor scope. The class *name* (if
    /// any) is registered by the caller; this only builds the function scopes.
    pub(super) fn build_class_scopes(
        &mut self,
        class: &ast::Class,
        scope: &mut FuncScope,
        block_scopes: &mut BlockScopes,
        scopes: &mut Vec<FuncScope>,
    ) {
        // `extends <ident>` (Step 7b): analyze the superclass reference in *this*
        // (enclosing) scope — codegen reads it here to link `C.prototype`'s proto
        // — and set the `super` context so the members capture the parent
        // constructor. A non-identifier superclass is left for codegen to reject.
        let super_name = match &class.super_class {
            Some(sc) => {
                self.analyze_expr(sc, scope, block_scopes, scopes);
                match sc {
                    ast::Expression::Identifier(id) => Some(id.name.as_str().to_string()),
                    _ => None,
                }
            }
            None => None,
        };
        let saved_super = self.current_super.take();
        self.current_super = super_name;

        // Gather the explicit constructor (if any) and the instance-field
        // initializer expressions, in declaration order.
        let mut ctor: Option<&ast::Function> = None;
        let mut field_inits: Vec<&ast::Expression> = Vec::new();
        for el in &class.body.body {
            match el {
                ast::ClassElement::MethodDefinition(m)
                    if m.kind == ast::MethodDefinitionKind::Constructor =>
                {
                    ctor = Some(&m.value);
                }
                ast::ClassElement::PropertyDefinition(p) if !p.r#static => {
                    if let Some(init) = &p.value {
                        field_inits.push(init);
                    }
                }
                _ => {}
            }
        }
        // The constructor (explicit, or synthetic with just the field inits).
        let ctor_scope = self.build_constructor_scope(class, ctor, &field_inits, scopes);
        scope.children.push(ctor_scope);
        // Each non-constructor method is an ordinary (non-arrow) function scope,
        // but keeps the class's `super` context (`inherit_super = true`).
        for el in &class.body.body {
            if let ast::ClassElement::MethodDefinition(m) = el
                && m.kind != ast::MethodDefinitionKind::Constructor
            {
                let child = self.build_function_scope(&m.value, false, true, scopes);
                scope.children.push(child);
            }
        }

        self.current_super = saved_super;
    }

    /// Build the constructor `FuncScope` for a class. With an explicit
    /// `constructor` method, it is that method's function scope; otherwise a
    /// synthetic zero-param scope keyed by the class node's span (so codegen can
    /// find it via `scope_for_node(class.span)`). Instance-field initializers are
    /// analyzed *in the constructor scope* (they run as a prologue with `this`
    /// bound), so a field initializer's `this`/captures resolve there — including
    /// reify-on-capture when a field's nested arrow references `this`.
    pub(super) fn build_constructor_scope(
        &mut self,
        class: &ast::Class,
        ctor: Option<&ast::Function>,
        field_inits: &[&ast::Expression],
        scopes: &mut Vec<FuncScope>,
    ) -> usize {
        let label = self.new_label();
        match ctor {
            Some(func) => {
                let params = self.collect_params(&func.params);
                let mut scope = FuncScope::new(
                    usize::MAX,
                    label,
                    func.span.start,
                    func.span.end,
                    params,
                    None,
                    false,
                );
                if func.params.rest.is_some() {
                    scope.uses_arguments = true;
                }
                let body = func.body.as_ref().map(|b| &b.statements[..]).unwrap_or(&[]);
                self.analyze_function_body(
                    &mut scope,
                    Some(&func.params),
                    body,
                    field_inits,
                    scopes,
                );
                self.push_scope(scope, scopes)
            }
            None => {
                // Default constructor: no params, no body — just the field inits.
                let mut scope = FuncScope::new(
                    usize::MAX,
                    label,
                    class.span.start,
                    class.span.end,
                    Vec::new(),
                    None,
                    false,
                );
                self.analyze_function_body(&mut scope, None, &[], field_inits, scopes);
                self.push_scope(scope, scopes)
            }
        }
    }
}
