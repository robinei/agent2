use super::*;
use crate::builtin::{Builtin, BuiltinKind};
use smallvec::SmallVec;

/// The missing-`await` hint, appended to property/index access errors when
/// the receiver is a promise — the misuse LLMs actually commit under this
/// dialect (`tools.f(x).field` instead of `(await tools.f(x)).field`).
fn await_hint(v: &Value) -> &'static str {
    match v {
        Value::Promise(_) => " (did you forget `await`?)?",
        _ => "",
    }
}

impl VM {
    // ── Step 2e: the canonical property read/write pair ────────────
    //
    /// The one property *read* ladder for every receiver type. Resolves in
    /// order:
    ///
    ///   1. Primary representation (Array int-index, Object own-map,
    ///      Map.get, String char-at, RegExp virtual props, Closure
    ///      virtual props, Builtin constructor virtual props)
    ///   2. User own-property bag (Object's map; Closure's inline `props`).
    ///      Array/Map/Set/Promise/RegExp are non-extensible for now (a user
    ///      write is a `TypeError`, pinned), so they carry no bag; a
    ///      side-table is the corpus-gated seam if that changes.
    ///   3. Type prototype chain (2a), with `method_for_receiver`
    ///      fallback (builtin prototypes have empty maps — methods are
    ///      virtual)
    ///   4. `Undefined`
    ///
    /// `key` is a `Value`: `String(field)` for named reads, an int for
    /// array/string indexing, or any value ToString'd for Object lookup.
    /// `null`/`undefined` receivers are a `TypeError`. This subsumes and
    /// **deletes**: the former `regexp_prop`, `resolve_computed_property`,
    /// `get_property_from_type`, and the inline resolve copies in
    /// `IndexGet`/`IndexSet`/`ObjHas`.
    pub(crate) fn get_property(&mut self, receiver: &Value, key: &Value) -> Result<Value, VMError> {
        // ── rung 0: primary representation ────────────────────────
        match receiver {
            Value::Null | Value::Undefined => {
                let name = self.to_js_string(key, 0);
                return Err(self.fail(
                    ErrorKind::TypeError,
                    format!(
                        "cannot read property '{}' on {}{}",
                        name.as_str(),
                        receiver.type_name(),
                        await_hint(receiver)
                    ),
                ));
            }
            Value::Upval(_) => return Err(self.fail(ErrorKind::ValueError, "value error")),
            Value::Promise(_) => {
                let name = self.to_js_string(key, 0);
                return Err(self.fail(
                    ErrorKind::TypeError,
                    format!(
                        "cannot read property '{}' on promise{}",
                        name.as_str(),
                        await_hint(receiver)
                    ),
                ));
            }
            _ => {}
        }

        // Integer-key fast path: arrays and strings (first, so
        // `arr[0]` never enters the named/ladder path).
        if let Some(idx) = key.as_i64() {
            if idx < 0 {
                return Err(self.fail(ErrorKind::ValueError, "value error"));
            }
            let idx = idx as usize;
            match receiver {
                Value::Array(p) => {
                    let arr = self.arrays.get(*p as usize).ok_or_else(|| {
                        self.fail_not_resumable(ErrorKind::TypeError, "bad array pointer")
                    })?;
                    return Ok(arr.get(idx).cloned().unwrap_or(Value::Undefined));
                }
                Value::String(s) => {
                    let s = s.as_str();
                    if idx >= s.len() {
                        return Ok(Value::Undefined);
                    }
                    if !s.is_char_boundary(idx) {
                        return Err(self.fail(ErrorKind::ValueError, "value error"));
                    }
                    let ch = s[idx..].chars().next().unwrap();
                    return Ok(Value::String(RcStr::from(ch.to_string())));
                }
                // Integer key on a non-array, non-string, non-Object:
                // TypeError (cannot index into <type>). Object falls
                // through to the named path (ToString the key).
                Value::Object(_) => {} // falls through
                _ => {
                    return Err(self.fail(
                        ErrorKind::TypeError,
                        format!(
                            "cannot index into {} with {}",
                            receiver.type_name(),
                            self.preview(key)
                        ),
                    ));
                }
            }
        }

        // Named-key path: coerce the key to a string.
        let field = self.to_js_string(key, 0);
        self.named_get_property(receiver, field.as_str())
    }

    /// The named half of [`get_property`] — virtual rungs, own-property
    /// bags, and proto-chain walk for a `&str` field. Called after the
    /// integer-key fast path above (which lives in `get_property` so
    /// array-index reads inline to exactly today's code).
    fn named_get_property(&mut self, receiver: &Value, field: &str) -> Result<Value, VMError> {
        match receiver {
            Value::Null | Value::Undefined | Value::Promise(_) | Value::Upval(_) => {
                unreachable!("handled before named_get_property")
            }

            // ── Object: own map → proto chain (the model) ────────
            Value::Object(p) => self.resolve_proto_chain(*p, field),

            // ── RegExp: virtual rungs (folded from `regexp_prop`) ─
            Value::RegExp(r) => {
                let v = match field {
                    "source" => Value::String(r.pattern.clone()),
                    "flags" => Value::String(r.flags.clone()),
                    "global" => Value::Bool(r.flags.contains('g')),
                    "ignoreCase" => Value::Bool(r.flags.contains('i')),
                    "multiline" => Value::Bool(r.flags.contains('m')),
                    "dotAll" => Value::Bool(r.flags.contains('s')),
                    "unicode" => Value::Bool(r.flags.contains('u')),
                    "sticky" => Value::Bool(r.flags.contains('y')),
                    "lastIndex" => Value::PosInt(r.last_index.get() as u64),
                    _ => Value::Undefined,
                };
                if !matches!(v, Value::Undefined) {
                    return Ok(v);
                }
                self.type_proto_lookup(crate::vm::instr::TypeTag::RegExp, field, receiver)
            }

            // ── Closure: virtual rungs → inline bag → Function proto ─
            Value::Closure { ptr, .. } => {
                match field {
                    "prototype" => {
                        return Ok(Value::Object(self.resolve_prototype(*ptr)?));
                    }
                    "name" | "length" => {
                        let c = self.closures.get(*ptr as usize).ok_or_else(|| {
                            self.fail_not_resumable(ErrorKind::TypeError, "bad closure pointer")
                        })?;
                        if field == "name" {
                            return Ok(Value::String(RcStr::from("")));
                        }
                        return Ok(Value::Float(c.arity as f64));
                    }
                    _ => {}
                }
                // Inline own-property bag (Step 2e).
                let c = self.closures.get(*ptr as usize).ok_or_else(|| {
                    self.fail_not_resumable(ErrorKind::TypeError, "bad closure pointer")
                })?;
                if let Some(ref bag) = c.props
                    && let Some(v) = bag.get(field)
                {
                    return Ok(v.clone());
                }
                self.type_proto_lookup(crate::vm::instr::TypeTag::Function, field, receiver)
            }

            // ── Builtin: virtual rungs (constructor) or Function proto ─
            Value::Builtin(b) => {
                if let Some(tag) = b.constructor_type_tag() {
                    match field {
                        "prototype" => {
                            return Ok(Value::Object(self.prototype_for(tag)?));
                        }
                        "name" => {
                            return Ok(Value::String(RcStr::from(b.meta().name)));
                        }
                        "length" => {
                            let meta = b.meta();
                            let n = match meta.kind {
                                crate::builtin::BuiltinKind::Method => {
                                    meta.min_args.saturating_sub(1)
                                }
                                crate::builtin::BuiltinKind::Namespace(_)
                                | crate::builtin::BuiltinKind::Constructor { .. } => meta.min_args,
                            };
                            return Ok(Value::Float(n as f64));
                        }
                        _ => {
                            if let Some(static_b) =
                                crate::builtin::Builtin::for_namespace(tag.name(), field)
                            {
                                return Ok(Value::Builtin(static_b));
                            }
                        }
                    }
                }
                self.type_proto_lookup(crate::vm::instr::TypeTag::Function, field, receiver)
            }

            // ── Array: `length` virtual rung → Array proto ───────
            Value::Array(_) => {
                if field == "length" {
                    let len = match receiver {
                        Value::Array(p) => {
                            self.arrays.get(*p as usize).map(|a| a.len()).unwrap_or(0)
                        }
                        _ => unreachable!(),
                    };
                    return Ok(Value::Float(len as f64));
                }
                self.type_proto_lookup(crate::vm::instr::TypeTag::Array, field, receiver)
            }

            // ── Map: `size` virtual rung → Map proto ────────────
            Value::Map(_) => {
                if field == "size" {
                    let sz = match receiver {
                        Value::Map(p) => self.maps.get(*p as usize).map(|m| m.len()).unwrap_or(0),
                        _ => unreachable!(),
                    };
                    return Ok(Value::Float(sz as f64));
                }
                self.type_proto_lookup(crate::vm::instr::TypeTag::Map, field, receiver)
            }

            // ── Set: `size` virtual rung → Set proto ────────────
            Value::Set(_) => {
                if field == "size" {
                    let sz = match receiver {
                        Value::Set(p) => self.sets.get(*p as usize).map(|s| s.len()).unwrap_or(0),
                        _ => unreachable!(),
                    };
                    return Ok(Value::Float(sz as f64));
                }
                self.type_proto_lookup(crate::vm::instr::TypeTag::Set, field, receiver)
            }

            // ── Bound: Function proto (non-extensible) ───────────
            Value::Bound(_) => {
                self.type_proto_lookup(crate::vm::instr::TypeTag::Function, field, receiver)
            }

            // ── String/Number/Boolean: type proto (no bags) ─────
            Value::String(_) => {
                self.type_proto_lookup(crate::vm::instr::TypeTag::String, field, receiver)
            }
            Value::Float(_) | Value::PosInt(_) | Value::NegInt(_) => {
                self.type_proto_lookup(crate::vm::instr::TypeTag::Number, field, receiver)
            }
            Value::Bool(_) => {
                self.type_proto_lookup(crate::vm::instr::TypeTag::Boolean, field, receiver)
            }
        }
    }

    /// Walk the builtin prototype chain for `tag`, then fall back to
    /// `method_for_receiver` (the unified method-resolution gate).
    fn type_proto_lookup(
        &mut self,
        tag: crate::vm::instr::TypeTag,
        field: &str,
        receiver: &Value,
    ) -> Result<Value, VMError> {
        let proto = self.prototype_for(tag)?;
        let val = self.resolve_proto_chain(proto, field)?;
        if !matches!(val, Value::Undefined) {
            return Ok(val);
        }
        Ok(Builtin::method_for_receiver(receiver, field)
            .map(Value::Builtin)
            .unwrap_or(Value::Undefined))
    }

    // ── set_property: the canonical write ladder ────────────────────
    //
    /// The one property *write* ladder. Resolves the destination through
    /// the matching rungs and performs the write:
    ///
    ///   1. Primary representation in-place (Array int-index, Object
    ///      own-map, RegExp `lastIndex` cell)
    ///   2. Integrity gate (Step 2d: extensible/seal/frozen)
    ///   3. User own-prop bag (Object's map; Closure inline bag;
    ///      side-table bags)
    ///   4. Reject (primitives, non-extensible natives)
    ///
    /// `mode` controls the return value: `New` leaves the assigned value,
    /// `Old` reads and leaves the previous value.
    pub(crate) fn set_property(
        &mut self,
        receiver: &Value,
        key: &Value,
        val: Value,
        mode: SetMode,
    ) -> Result<Value, VMError> {
        // Integer-key fast path for arrays.
        if let Some(idx) = key.as_i64() {
            if idx < 0 {
                return Err(self.fail(ErrorKind::ValueError, "value error"));
            }
            let idx = idx as usize;
            if let Value::Array(p) = receiver {
                let ip = self.ip;
                let arr = match self.arrays.get_mut(*p as usize) {
                    Some(a) => a,
                    _ => {
                        return Err(VMError::fail_at(
                            ip,
                            ErrorKind::TypeError,
                            "bad array pointer",
                        ));
                    }
                };
                if idx >= arr.len() {
                    let len = arr.len();
                    return Err(self.fail(
                        ErrorKind::ValueError,
                        format!("cannot write array index {idx}: out of bounds (length {len})"),
                    ));
                }
                let old = std::mem::replace(&mut arr[idx], val);
                return Ok(match mode {
                    SetMode::New => arr[idx].clone(),
                    SetMode::Old => old,
                });
            }
        }

        let field = self.to_js_string(key, 0);
        self.named_set_property(receiver, field.as_str(), val, mode)
    }

    /// Named half of [`set_property`].
    fn named_set_property(
        &mut self,
        receiver: &Value,
        field: &str,
        val: Value,
        mode: SetMode,
    ) -> Result<Value, VMError> {
        match receiver {
            // ── Object: own map, with integrity gate ─────────────
            Value::Object(p) => {
                let obj_ptr = *p;
                let (is_new, integrity) = {
                    let obj = match self.objects.get(obj_ptr as usize) {
                        Some(o) => o,
                        _ => {
                            return Err(
                                self.fail_not_resumable(ErrorKind::TypeError, "bad object pointer")
                            );
                        }
                    };
                    (!obj.map.contains_key(field), obj.integrity)
                };
                if integrity == IntegrityLevel::Frozen {
                    return Err(self.fail(
                        ErrorKind::TypeError,
                        "cannot set a property of a frozen object",
                    ));
                }
                if is_new
                    && matches!(
                        integrity,
                        IntegrityLevel::NonExtensible | IntegrityLevel::Sealed
                    )
                {
                    return Err(self.fail(
                        ErrorKind::TypeError,
                        "cannot add a property to a non-extensible object",
                    ));
                }
                let ip = self.ip;
                let obj = match self.objects.get_mut(obj_ptr as usize) {
                    Some(o) => o,
                    _ => {
                        return Err(VMError::fail_at(
                            ip,
                            ErrorKind::TypeError,
                            "bad object pointer",
                        ));
                    }
                };
                let result = match mode {
                    SetMode::Old => {
                        let old = obj.map.get(field).cloned().unwrap_or(Value::Undefined);
                        if let Some(slot) = obj.map.get_mut(field) {
                            *slot = val;
                        } else {
                            obj.map.insert(RcStr::from(field), val);
                        }
                        old
                    }
                    SetMode::New => {
                        let result = val.clone();
                        if let Some(slot) = obj.map.get_mut(field) {
                            *slot = val;
                        } else {
                            obj.map.insert(RcStr::from(field), val);
                        }
                        result
                    }
                };
                Ok(result)
            }

            // ── RegExp: lastIndex is writable ────────────────────
            Value::RegExp(r) => {
                if field == "lastIndex" {
                    let n = val.to_number().unwrap_or(0.0);
                    let n = if n.is_finite() && n >= 0.0 {
                        n as usize
                    } else {
                        0
                    };
                    let old = Value::PosInt(r.last_index.get() as u64);
                    r.last_index.set(n);
                    return Ok(match mode {
                        SetMode::New => val,
                        SetMode::Old => old,
                    });
                }
                // Other RegExp properties are read-only; silently accept.
                Ok(match mode {
                    SetMode::New => val,
                    SetMode::Old => val,
                })
            }

            // ── Closure: inline bag (extensible) ────────────────
            Value::Closure { ptr, .. } => {
                let ip = self.ip;
                let c = match self.closures.get_mut(*ptr as usize) {
                    Some(c) => c,
                    _ => {
                        return Err(VMError::fail_at(
                            ip,
                            ErrorKind::TypeError,
                            "bad closure pointer",
                        ));
                    }
                };
                let bag = c.props.get_or_insert_with(|| Box::new(IndexMap::new()));
                let result = match mode {
                    SetMode::Old => {
                        let old = bag.get(field).cloned().unwrap_or(Value::Undefined);
                        if let Some(slot) = bag.get_mut(field) {
                            *slot = val;
                        } else {
                            bag.insert(RcStr::from(field), val);
                        }
                        old
                    }
                    SetMode::New => {
                        let result = val.clone();
                        if let Some(slot) = bag.get_mut(field) {
                            *slot = val;
                        } else {
                            bag.insert(RcStr::from(field), val);
                        }
                        result
                    }
                };
                Ok(result)
            }

            // ── Builtin/Bound/Array/Map/Set/Promise/primitives: ──
            //     non-extensible (TypeError) for now. Side-table
            //     bags for Array/Map/Set/RegExp are deferred
            //     (corpus-gated).
            Value::Builtin(_) => Err(self.fail(
                ErrorKind::TypeError,
                "cannot set a property of a builtin function",
            )),
            Value::Bound(_) => Err(self.fail(
                ErrorKind::TypeError,
                "cannot set a property of a bound function",
            )),
            Value::Array(_) => Err(self.fail(
                ErrorKind::TypeError,
                "cannot set a named property on an array (non-extensible)",
            )),
            Value::Map(_) => Err(self.fail(
                ErrorKind::TypeError,
                "cannot set a named property on a Map (non-extensible)",
            )),
            Value::Set(_) => Err(self.fail(
                ErrorKind::TypeError,
                "cannot set a named property on a Set (non-extensible)",
            )),
            Value::Promise(_) => Err(self.fail(
                ErrorKind::TypeError,
                "cannot set a property on a promise (non-extensible)",
            )),
            Value::String(_)
            | Value::Float(_)
            | Value::PosInt(_)
            | Value::NegInt(_)
            | Value::Bool(_)
            | Value::Null
            | Value::Undefined
            | Value::Upval(_) => Err(self.fail(
                ErrorKind::TypeError,
                format!("cannot set property on {}", receiver.type_name()),
            )),
        }
    }

    /// Resolve a method name on an `Object` receiver via the unified
    /// own-properties → proto-chain walk. When a `CallBuiltin` instruction
    /// lands on an `Object` receiver, this helper checks whether the object
    /// (or its proto chain) has a shadowing property of the same name —
    /// if so, that property is dispatched instead of the builtin (Step 2c:
    /// uniform shadowing for every method name, including `hasOwnProperty`).
    /// The former `MethodOnObject` error-signal + `reroute_method_to_object`
    /// pair are retired; this is the fast-path shadow check that keeps
    /// `CallBuiltin` the common case while sharing one resolution body
    /// (`resolve_proto_chain`) with `get_property`.
    pub(crate) fn resolve_method_for_object_receiver(
        &mut self,
        recv: &Value,
        name: &str,
    ) -> Result<Option<Value>, VMError> {
        let obj_ptr = match recv {
            Value::Object(p) => *p,
            _ => return Ok(None),
        };
        let val = self.resolve_proto_chain(obj_ptr, name)?;
        Ok(if matches!(val, Value::Undefined) {
            None
        } else {
            Some(val)
        })
    }

    /// Snapshot of a receiver's own enumerable string-keyed properties as
    /// (key, value) pairs in insertion order — the one source the reflection
    /// builtins (`Object.keys`/`values`/`entries`) read. An `Object` reads its
    /// `map`; a function reads its `Closure.props` bag (Step 2e), whose virtual
    /// rungs (`name`/`length`/`prototype`) are *not* stored there and so are
    /// correctly excluded from enumeration. A function with no user props yields
    /// an empty list. Returns `None` for any other receiver — the builtins turn
    /// that into the same `TypeError` they already raise for non-objects.
    /// (Collections/primitives are non-extensible, so they have no user bag.)
    pub(crate) fn own_enumerable_props(&self, value: &Value) -> Option<Vec<(RcStr, Value)>> {
        match value {
            Value::Object(p) => self
                .objects
                .get(*p as usize)
                .map(|o| o.map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            Value::Closure { ptr, .. } => Some(
                self.closures
                    .get(*ptr as usize)
                    .and_then(|c| c.props.as_deref())
                    .map(|bag| bag.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default(),
            ),
            _ => None,
        }
    }

    /// Whether a receiver has its own property `key` — the unified backing for
    /// `Object.hasOwn` and `obj.hasOwnProperty`. Mirrors `own_enumerable_props`'s
    /// receiver handling (Object `map` or function `props` bag); `None` for a
    /// receiver the reflection builtins reject.
    pub(crate) fn own_prop_contains(&self, value: &Value, key: &str) -> Option<bool> {
        match value {
            Value::Object(p) => self
                .objects
                .get(*p as usize)
                .map(|o| o.map.contains_key(key)),
            Value::Closure { ptr, .. } => Some(
                self.closures
                    .get(*ptr as usize)
                    .and_then(|c| c.props.as_deref())
                    .is_some_and(|bag| bag.contains_key(key)),
            ),
            _ => None,
        }
    }

    /// Dispatch a `CallBuiltin`-shaped call that an `Object` receiver may
    /// shadow. The top `argc` stack values are the builtin's args (arg 0 =
    /// receiver, deepest). If the receiver is an `Object` whose own/proto
    /// chain carries a property of the builtin's name, that property shadows
    /// the builtin and is dispatched with the Object as `this` (Step 2c:
    /// uniform shadowing, replacing the retired `MethodOnObject` reroute);
    /// otherwise the builtin runs normally. The Object check is the *only*
    /// thing on the fast path — the cheap `matches!` gates the receiver clone
    /// and the proto walk, so a structural receiver (`arr.push`, `"x".at`)
    /// pays one branch and nothing else. On the builtin path `ip` is advanced
    /// here; the shadow path defers `ip` to `dispatch_call`. Shared by the
    /// `Instr::CallBuiltin` arm and `dispatch_call`'s `Builtin` arm.
    pub(crate) fn call_builtin_or_shadow(&mut self, b: Builtin, argc: u32) -> Result<(), VMError> {
        if argc > 0 {
            let base = self.stack.len() - argc as usize;
            if matches!(self.stack[base], Value::Object(_)) {
                let recv = self.stack[base].clone();
                if let Some(callable) =
                    self.resolve_method_for_object_receiver(&recv, b.meta().name)?
                {
                    let recv = std::mem::replace(&mut self.stack[base], Value::Undefined);
                    return self.dispatch_call(callable, recv, argc - 1, 1);
                }
            }
        }
        b.call(self, argc)?;
        self.ip += 1;
        Ok(())
    }
}

impl VM {
    /// JS `Function.prototype.length` for any callable value (Step 6):
    /// `Closure` → the declared param count before the first default/rest
    /// (stored on the `Closure` heap entry); `Builtin` → `min_args` minus 1
    /// for `Method`-kind (the receiver isn't a declared param) or `min_args`
    /// for `Namespace` (approximate — a documented divergence); `Bound` →
    /// `max(0, target.length - bound_args.len())`. Non-callable → `None`.
    fn callable_length(&self, val: &Value) -> Option<u16> {
        match val {
            Value::Closure { ptr, .. } => self.closures.get(*ptr as usize).map(|c| c.arity),
            Value::Builtin(b) => {
                let meta = b.meta();
                let n = match meta.kind {
                    BuiltinKind::Method => meta.min_args.saturating_sub(1),
                    BuiltinKind::Namespace(_) | BuiltinKind::Constructor { .. } => meta.min_args,
                };
                Some(n as u16)
            }
            Value::Bound(b) => {
                let target = self.callable_length(&b.callable).unwrap_or(0);
                Some(target.saturating_sub(b.bound_args.len() as u16))
            }
            _ => None,
        }
    }

    pub(crate) fn dispatch(&mut self, fuel: &mut u64) -> Result<StepResult, VMError> {
        // ── macros for repetitive instruction shapes ─────────────────

        /// Pop one operand, coerce ToNumber (JS), apply f64→f64, push Number.
        /// A non-coercible operand (array/object/function) is a TypeError; an
        /// `undefined` or unparseable string coerces to NaN and propagates.
        macro_rules! unary_num {
            ($op:expr) => {{
                let val = self.pop()?;
                match val.to_number() {
                    Some(n) => {
                        self.stack.push(Value::Float($op(n)));
                        self.ip += 1;
                    }
                    None => {
                        let msg = format!(
                            "cannot coerce {} ({}) to number",
                            val.type_name(),
                            self.preview(&val)
                        );
                        return Err(self.fail(ErrorKind::TypeError, msg));
                    }
                }
            }};
        }

        /// Pop rhs then lhs, coerce both ToNumber (JS), apply f64→f64→f64, push
        /// Number. Strings/booleans/null coerce; arrays/objects/functions are a
        /// TypeError; undefined/unparseable strings become NaN.
        macro_rules! binary_num {
            ($op:expr) => {{
                let rhs = self.pop()?;
                let lhs = self.pop()?;
                match (lhs.to_number(), rhs.to_number()) {
                    (Some(a), Some(b)) => {
                        self.stack.push(Value::Float($op(a, b)));
                        self.ip += 1;
                    }
                    _ => {
                        let msg = format!(
                            "cannot coerce {} ({}) and {} ({}) to number",
                            lhs.type_name(),
                            self.preview(&lhs),
                            rhs.type_name(),
                            self.preview(&rhs)
                        );
                        return Err(self.fail(ErrorKind::TypeError, msg));
                    }
                }
            }};
        }

        /// Pop rhs then lhs (integer-valued: Int, or integer-valued Number),
        /// apply i64→i64→i64, push Number.
        macro_rules! binary_int {
            ($op:expr) => {{
                let rhs = self.pop()?;
                let lhs = self.pop()?;
                match (lhs.as_i64(), rhs.as_i64()) {
                    (Some(a), Some(b)) => {
                        self.stack.push(Value::Float($op(a, b) as f64));
                        self.ip += 1;
                    }
                    _ => {
                        let msg = format!(
                            "expected integer operands, got {} ({}) and {} ({})",
                            lhs.type_name(),
                            self.preview(&lhs),
                            rhs.type_name(),
                            self.preview(&rhs)
                        );
                        return Err(self.fail(ErrorKind::TypeError, msg));
                    }
                }
            }};
        }

        /// Pop rhs then lhs, compare with self.compare(), push Bool.
        macro_rules! cmp_op {
            ($cmp:tt $expected:ident) => {{
                let rhs = self.pop()?;
                let lhs = self.pop()?;
                let result = lhs
                    .compare(&rhs)
                    .map(|ord| ord $cmp std::cmp::Ordering::$expected)
                    .unwrap_or(false);
                self.stack.push(Value::Bool(result));
                self.ip += 1;
            }};
        }

        // ── main dispatch loop ───────────────────────────────────────

        loop {
            if self.ip as usize >= self.code.len() {
                return Ok(StepResult::Done {
                    value: Value::Undefined,
                    unstarted: std::mem::take(&mut self.outbox),
                });
            }
            if *fuel == 0 {
                return Ok(StepResult::OutOfFuel);
            }
            *fuel -= 1;
            match &self.code[self.ip as usize] {
                // ── stack manipulation ───────────────────────────
                Instr::PushUndefined => {
                    self.stack.push(Value::Undefined);
                    self.ip += 1;
                }
                Instr::PushNull => {
                    self.stack.push(Value::Null);
                    self.ip += 1;
                }
                Instr::PushBool(b) => {
                    self.stack.push(Value::Bool(*b));
                    self.ip += 1;
                }
                Instr::PushPosInt(u) => {
                    self.stack.push(Value::PosInt(*u));
                    self.ip += 1;
                }
                Instr::PushNegInt(i) => {
                    self.stack.push(Value::NegInt(*i));
                    self.ip += 1;
                }
                Instr::PushFloat(f) => {
                    self.stack.push(Value::Float(*f));
                    self.ip += 1;
                }
                Instr::PushStr(s) => {
                    self.stack.push(Value::String(s.clone()));
                    self.ip += 1;
                }
                Instr::PushArray(h) => {
                    self.stack.push(Value::Array(*h));
                    self.ip += 1;
                }
                Instr::PushObject(h) => {
                    self.stack.push(Value::Object(*h));
                    self.ip += 1;
                }
                Instr::PushFn(addr, ptr, _) => {
                    // Const-fn canonical push (Step 2e): a const-fn declaration
                    // is single-identity (one function object), so every
                    // value-reference resolves to the *same* pre-allocated
                    // canonical closure (`ptr` baked at load by
                    // `for_program_with`). This keeps `F === F`, a shared
                    // `.prototype`, and `new F() instanceof F` correct. Genuine
                    // per-evaluation function values (expressions, non-const
                    // decls) use `ClosureNew` instead, which allocates fresh.
                    self.stack.push(Value::Closure {
                        addr: *addr,
                        ptr: *ptr,
                    });
                    self.ip += 1;
                }
                Instr::PushBuiltin(b) => {
                    self.stack.push(Value::Builtin(*b));
                    self.ip += 1;
                }
                Instr::PushGlobal(g) => {
                    let obj = self.namespace_for(*g)?;
                    self.stack.push(Value::Object(obj));
                    self.ip += 1;
                }

                Instr::Pop(n) => {
                    // Only expression temporaries may be popped, never locals
                    // or args belonging to the current/caller frame.
                    if self.stack.len() < self.frame_floor() + *n {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    self.stack.truncate(self.stack.len() - n);
                    self.ip += 1;
                }

                Instr::Pick(n) => {
                    let n = *n;
                    let len = self.stack.len();
                    // The picked value sits at len-1-n; it (and everything above)
                    // must be a temporary, not a local/arg.
                    if len < self.frame_floor() + n + 1 {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    let val = self.stack[len - 1 - n].clone();
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::Dig(n) => {
                    let n = *n;
                    let len = self.stack.len();
                    if len < self.frame_floor() + n + 1 {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    // Fast paths for the common small-n cases, matching the
                    // performance of the former Dup/Swap/Rot dispatch.
                    match n {
                        0 => {} // no-op
                        1 => self.stack.swap(len - 1, len - 2),
                        2 => {
                            self.stack.swap(len - 3, len - 2);
                            self.stack.swap(len - 2, len - 1);
                        }
                        _ => {
                            let val = self.stack.remove(len - 1 - n);
                            self.stack.push(val);
                        }
                    }
                    self.ip += 1;
                }

                Instr::Nip(n) => {
                    let n = *n;
                    let len = self.stack.len();
                    if len < self.frame_floor() + n + 1 {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    // Remove n values directly below the top, leaving the top
                    // in place. Nip(1) ≡ Dig(1); Pop; Nip(n) is the inverse of
                    // Dig(n): where Dig moves element len-1-n to the top,
                    // Nip drops it.
                    let start = len - 1 - n;
                    self.stack.drain(start..start + n);
                    self.ip += 1;
                }

                // ── control flow ─────────────────────────────────
                Instr::Call(addr, nargs) => {
                    // Bare-address call: the callee is in the instruction, nothing
                    // sits below the args (reclaim_below = 0).
                    self.call_function(*addr, *nargs, u32::MAX, Value::Undefined, 0)?
                }

                Instr::CallDyn(nargs, has_this) => {
                    let nargs = *nargs;
                    let has_this = *has_this;
                    // Callee-below-args: stack is `[recv?, callee, args…]`. Read
                    // the callee (and receiver) *in place* — no arg shift — and
                    // leave their slots as `Undefined` placeholders that `Return`
                    // reclaims via `reclaim_below`.
                    let args_start = self.stack.len() - nargs as usize;
                    let callable =
                        std::mem::replace(&mut self.stack[args_start - 1], Value::Undefined);
                    let (this_val, below) = if has_this {
                        // 3b method-call path (`recv.m(args)`, `recv[k](args)`):
                        // both the receiver and the callee sit below `fp`, so
                        // `reclaim_below = 2`. Exercised by the method-`this`
                        // tests in `compiler/tests/objects_arrays.rs`.
                        let recv =
                            std::mem::replace(&mut self.stack[args_start - 2], Value::Undefined);
                        (recv, 2)
                    } else {
                        (Value::Undefined, 1)
                    };
                    self.dispatch_call(callable, this_val, nargs, below)?;
                }

                Instr::CallBuiltin(b, argc) => {
                    self.call_builtin_or_shadow(*b, *argc)?;
                }

                Instr::CallSpread(has_this) => {
                    let has_this = *has_this;
                    // `[recv?, callee, argsArray]`. Read callee/recv *in place*
                    // (no shift) — placeholders stay under the spread args for
                    // `Return` to reclaim via `reclaim_below`.
                    let callee_idx = self.stack.len() - 2;
                    let callable = std::mem::replace(&mut self.stack[callee_idx], Value::Undefined);
                    let (this_val, below) = if has_this {
                        let recv_idx = self.stack.len() - 3;
                        (
                            std::mem::replace(&mut self.stack[recv_idx], Value::Undefined),
                            2,
                        )
                    } else {
                        (Value::Undefined, 1)
                    };
                    let nargs = match self.pop()? {
                        Value::Array(arr_ptr) => {
                            let ip = self.ip;
                            let elements: ThinVec<Value> = self
                                .arrays
                                .get(arr_ptr as usize)
                                .ok_or_else(|| {
                                    VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer")
                                })?
                                .clone();
                            let n = elements.len() as u32;
                            for val in elements {
                                self.stack.push(val);
                            }
                            n
                        }
                        // Nullish args → no args. Lets `f.apply(t, null)` /
                        // `f.apply(t)` mean "call with no args" (JS-faithful for
                        // `.apply`); also makes `f(...null)` lenient, consistent
                        // with the VM's other nullish-spread divergences.
                        Value::Null | Value::Undefined => 0,
                        _ => {
                            return Err(self.fail(
                                ErrorKind::TypeError,
                                "spread call arguments must be an array",
                            ));
                        }
                    };
                    self.dispatch_call(callable, this_val, nargs, below)?;
                }

                Instr::New(nargs) => {
                    let nargs = *nargs;
                    let args_start = self.stack.len() - nargs as usize;
                    let callable =
                        std::mem::replace(&mut self.stack[args_start - 1], Value::Undefined);
                    // Native constructor (Step 2a Part 2): `new Map()`,
                    // `new Set()`, `new RegExp()`, `new Array()`, … — fold
                    // the type's native construction directly. The builtin's
                    // `type_tag` keys the prototype side table (so the
                    // constructed value's `[[Prototype]]` is correct); no
                    // user-code frame is pushed, so the `NewReturn` that
                    // follows this `New` in the code stream is dead.
                    //
                    // `construct_builtin` is a pure value-producer (Step 2a
                    // Part 3 item D): it consumes the callee placeholder +
                    // args and pushes the result, but does not touch `ip`.
                    // No `Vec::remove` mid-stack (item E): it truncates to
                    // below the callee and pushes, giving `[...caller,
                    // result]` in one O(1) truncate + push. We own the
                    // single `ip += 2` that steps past both `New` and the
                    // dead `NewReturn`.
                    if let Value::Builtin(b) = &callable
                        && b.constructor_type_tag().is_some()
                    {
                        self.construct_builtin(*b, nargs)?;
                        self.ip += 2;
                        continue;
                    }
                    let (_addr, ptr) = match &callable {
                        Value::Closure { addr, ptr } => (*addr, *ptr),
                        _ => {
                            let msg = format!(
                                "cannot call a {} as a function with `new`",
                                callable.type_name()
                            );
                            self.stack.truncate(args_start - 1);
                            return Err(self.fail(ErrorKind::TypeError, msg));
                        }
                    };
                    let proto_ptr = self
                        .closures
                        .get(ptr as usize)
                        .and_then(|c| c.prototype)
                        .unwrap_or_else(|| {
                            let new_map = IndexMap::new();
                            let proto_ptr = self.objects.len() as ObjectPtr;
                            self.objects.push(ObjData {
                                proto: None,
                                map: new_map,
                                ..Default::default()
                            });
                            self.closures[ptr as usize].prototype = Some(proto_ptr);
                            proto_ptr
                        });
                    let new_obj = self.objects.len() as ObjectPtr;
                    self.objects.push(ObjData {
                        proto: Some(proto_ptr),
                        map: IndexMap::new(),
                        ..Default::default()
                    });
                    let new_obj_val = Value::Object(new_obj);
                    self.callstack.last_mut().unwrap().new_obj = Some(new_obj);
                    self.dispatch_call(callable, new_obj_val, nargs, 1)?;
                }

                Instr::NewReturn => {
                    // JS: `new` yields the constructor's return value iff it is an
                    // *object* — arrays and functions included, i.e. anything
                    // non-primitive; otherwise it yields the freshly-allocated
                    // instance. (`Upval` is an internal cell marker, never a
                    // user-visible return value.)
                    let returned_object = matches!(
                        self.stack.last(),
                        Some(
                            Value::Object(_)
                                | Value::Array(_)
                                | Value::Map(_)
                                | Value::Set(_)
                                | Value::RegExp(_)
                                | Value::Closure { .. }
                                | Value::Builtin(_)
                                | Value::Promise(_)
                        )
                    );
                    if !returned_object {
                        // Non-object return: discard it and use the allocated
                        // instance stored on the caller frame.
                        let new_obj = self
                            .callstack
                            .last_mut()
                            .and_then(|f| f.new_obj.take())
                            .ok_or_else(|| {
                                self.fail_not_resumable(
                                    ErrorKind::BadReturn,
                                    "NewReturn: no new_obj on caller frame",
                                )
                            })?;
                        self.stack.pop();
                        self.stack.push(Value::Object(new_obj));
                    }
                    if let Some(f) = self.callstack.last_mut() {
                        f.new_obj = None;
                    }
                    self.ip += 1;
                }

                Instr::ClosureNew(addr, arity, captures) => {
                    let addr = self.validate_func_addr(*addr)?;
                    let mut upvals: SmallVec<[Value; 8]> = SmallVec::new();
                    for slot in captures.iter() {
                        if (*slot as u32) >= self.cur_local_count {
                            return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                        }
                        upvals.push(self.stack[(self.fp + *slot as u32) as usize].clone());
                    }
                    let closure =
                        self.alloc_closure(addr, ThinVec::from(upvals.as_slice()), *arity);
                    self.stack.push(closure);
                    self.ip += 1;
                }

                Instr::Return(nrets) => {
                    let frame = self
                        .callstack
                        .pop()
                        .ok_or_else(|| self.fail(ErrorKind::BadReturn, "bad return"))?;
                    // `fp` points at the frame base (arg 0 / local 0). The
                    // return value(s) replace the whole call group: the frame
                    // *plus* the `reclaim_below` dead slots (callee/receiver) the
                    // caller left just under `fp` (read in place, not shifted).
                    let keep_below = (self.fp - frame.reclaim_below) as usize;
                    let n = *nrets;
                    if self.stack.len() < keep_below + n {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    if let Completion::ResolvePromise(pid) = frame.completion {
                        // A scheduler-resumed async frame has no caller below:
                        // resolve its promise with the return value (waking
                        // waiters) and fall through to the scheduler.
                        let value = if n == 0 {
                            Value::Undefined
                        } else {
                            self.stack[self.stack.len() - n].clone()
                        };
                        self.stack.truncate(keep_below);
                        self.fp = frame.prev_fp;
                        self.cur_local_count = self.callstack.last().map_or(0, |f| f.local_count);
                        self.settle_and_wake(pid, PromiseState::Resolved(value))?;
                        self.schedule()?;
                        continue;
                    }
                    let ret_start = self.stack.len() - n;
                    // Move (don't clone) each return value down to the frame
                    // base; the source region is truncated away immediately, so
                    // cloning would just bump-then-drop a refcount for string/
                    // heap returns. Ascending order is safe: dest indices
                    // (`keep_below..`) are <= source indices (`ret_start..`), so
                    // a source slot is never read after being overwritten.
                    for i in 0..n {
                        let v = std::mem::replace(&mut self.stack[ret_start + i], Value::Undefined);
                        self.stack[keep_below + i] = v;
                    }
                    self.stack.truncate(keep_below + n);
                    self.ip = frame.return_addr;
                    self.fp = frame.prev_fp;
                    if self.callstack.is_empty() {
                        // Root frame returned — capture the top-level value
                        // (peek, so the stack is still inspectable after Done).
                        // Tool calls started but never awaited are reported as
                        // `unstarted`; the host decides whether to run them.
                        let value = self.stack.last().cloned().unwrap_or(Value::Undefined);
                        return Ok(StepResult::Done {
                            value,
                            unstarted: std::mem::take(&mut self.outbox),
                        });
                    }
                    // Refresh the local-count cache from the restored caller
                    // frame (returns are far rarer than local accesses).
                    self.cur_local_count = self.callstack.last().map_or(0, |f| f.local_count);
                }

                Instr::Jump(addr) => {
                    self.ip = self.validate_jump_addr(*addr)?;
                }

                Instr::JFalse(addr) => {
                    let addr = self.validate_jump_addr(*addr)?;
                    let val = self.pop()?;
                    if !val.is_truthy() {
                        self.ip = addr;
                    } else {
                        self.ip += 1;
                    }
                }

                Instr::JTrue(addr) => {
                    let addr = self.validate_jump_addr(*addr)?;
                    let val = self.pop()?;
                    if val.is_truthy() {
                        self.ip = addr;
                    } else {
                        self.ip += 1;
                    }
                }

                Instr::JNotNullish(addr) => {
                    let addr = self.validate_jump_addr(*addr)?;
                    // Taken: leave the value for the branch that proceeds with
                    // it. Fall-through: pop it — the emitter short-circuits
                    // past the value (see the instruction doc).
                    let val = self.peek()?;
                    if !matches!(val, Value::Null | Value::Undefined) {
                        self.ip = addr;
                    } else {
                        self.stack.pop();
                        self.ip += 1;
                    }
                }

                Instr::Label(_) => {
                    // Eliminated in a pre-pass; no-op at runtime.
                    self.ip += 1;
                }

                // ── frame access ────────────────────────────────
                Instr::EnterFrame(nparams, build_args, local_kinds) => {
                    let nparams = *nparams;
                    let build_args = *build_args;
                    // Collect slot kinds into a stack-allocated SmallVec (zero
                    // alloc for the typical ≤2 locals) instead of cloning the
                    // ThinVec from self.code.
                    let local_kinds: SmallVec<[SlotKind; 32]> =
                        local_kinds.iter().copied().collect();
                    let frame = self
                        .callstack
                        .last()
                        .ok_or_else(|| self.fail(ErrorKind::BadArg, "bad argument"))?;
                    let argc = frame.arg_count;
                    // The args arrived as the leading locals at [fp, fp + argc).
                    // 1. Materialize the `arguments` array (from the actual args)
                    //    BEFORE normalizing, if the body uses it.
                    if build_args {
                        let base = self.fp as usize;
                        if base + argc as usize > self.stack.len() {
                            return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                        }
                        let args: ThinVec<Value> = self.stack[base..base + argc as usize]
                            .iter()
                            .cloned()
                            .collect();
                        let arr = self.alloc_array(args);
                        if let Value::Array(p) = arr {
                            self.callstack.last_mut().unwrap().arguments_cache = Some(p);
                        }
                    }
                    // 2. Normalize the arg region to exactly `nparams` slots:
                    //    drop surplus args, or pad missing params with Undefined.
                    let want = self.fp as usize + nparams as usize;
                    if self.stack.len() > want {
                        self.stack.truncate(want);
                    } else {
                        self.stack.resize(want, Value::Undefined);
                    }
                    // 3. Install the closure's captured environment as the upval
                    //    locals, now landing at [fp + nparams, fp + nparams + K).
                    //    Read from `closures[ptr]` only when the callee has upvals
                    //    (a non-capturing function has empty upvals; the sentinel 0
                    //    is used for bare-addressed calls that have no heap entry).
                    let ptr = self
                        .callstack
                        .last()
                        .map_or(u32::MAX, |f| f.pending_closure);
                    let mut k: u32 = 0;
                    if ptr != u32::MAX
                        && let Some(closure) = self.closures.get(ptr as usize)
                    {
                        for uv in &closure.upvals {
                            self.stack.push(uv.clone());
                        }
                        k = closure.upvals.len() as u32;
                    }
                    // 4. Allocate the declared (non-param) own locals + self-ref
                    // slot (Boxed → fresh cell + Upval).
                    for kind in &local_kinds {
                        let slot = match kind {
                            SlotKind::Plain => Value::Undefined,
                            SlotKind::Boxed => {
                                let idx = self.cells.len() as CellIndex;
                                self.cells.push(Value::Undefined);
                                Value::Upval(idx)
                            }
                        };
                        self.stack.push(slot);
                    }
                    let total = nparams as u32 + k + local_kinds.len() as u32;
                    self.callstack.last_mut().unwrap().local_count = total;
                    self.cur_local_count = total;
                    self.ip += 1;
                }

                Instr::Arguments => {
                    let frame = self
                        .callstack
                        .last()
                        .ok_or_else(|| self.fail(ErrorKind::BadArg, "bad argument"))?;
                    // Reuse the cached array when this frame already built one
                    // (functions that use `arguments` build it eagerly in the
                    // prologue's EnterFrame; the lazy path here serves the root
                    // frame, which has no args).
                    if let Some(ptr) = frame.arguments_cache {
                        self.stack.push(Value::Array(ptr));
                        self.ip += 1;
                        continue;
                    }
                    // Build it from the frame's args (arg 0 at fp). Copy them out
                    // before touching the heap.
                    let argc = frame.arg_count;
                    let base = self.fp as usize;
                    if base + argc as usize > self.stack.len() {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    let args: ThinVec<Value> = self.stack[base..base + argc as usize]
                        .iter()
                        .cloned()
                        .collect();
                    let arr = self.alloc_array(args);
                    let ptr = match arr {
                        Value::Array(p) => p,
                        _ => unreachable!("alloc_array returns an Array"),
                    };
                    self.callstack.last_mut().unwrap().arguments_cache = Some(ptr);
                    self.stack.push(arr);
                    self.ip += 1;
                }

                Instr::GetLocal(local) => {
                    if (*local as u32) >= self.cur_local_count {
                        return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                    }
                    // A Boxed slot holds an Upval marker; dereference it so the
                    // value — never the marker — reaches the expression stack.
                    let val = match &self.stack[(self.fp + *local as u32) as usize] {
                        Value::Upval(c) => self
                            .cells
                            .get(*c as usize)
                            .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                            .clone(),
                        other => other.clone(),
                    };
                    self.stack.push(val);
                    self.ip += 1;
                }

                Instr::LoadThis => {
                    let this_val = self.callstack.last().unwrap().this_val.clone();
                    self.stack.push(this_val);
                    self.ip += 1;
                }

                Instr::SetLocal(local) => {
                    let local = *local;
                    if (local as u32) >= self.cur_local_count {
                        return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                    }
                    let val = self.pop()?;
                    let slot = (self.fp + local as u32) as usize;
                    // Write through a Boxed slot to its shared cell; a Plain slot
                    // is overwritten in place.
                    match self.stack[slot] {
                        Value::Upval(c) => {
                            let ip = self.ip;
                            *self.cells.get_mut(c as usize).ok_or_else(|| {
                                VMError::fail_at(ip, ErrorKind::ValueError, "bad cell pointer")
                            })? = val;
                        }
                        _ => self.stack[slot] = val,
                    }
                    self.ip += 1;
                }

                Instr::TeeLocal(local) => {
                    if (*local as u32) >= self.cur_local_count {
                        return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                    }
                    let val = self.peek()?.clone();
                    let slot = (self.fp + *local as u32) as usize;
                    // Like SetLocal but peeks: the value stays on the stack
                    // (assignment is an expression) while still writing to the
                    // local. Replaces Pick(0); SetLocal (formerly Dup; SetLocal).
                    match self.stack[slot] {
                        Value::Upval(c) => {
                            let ip = self.ip;
                            *self.cells.get_mut(c as usize).ok_or_else(|| {
                                VMError::fail_at(ip, ErrorKind::ValueError, "bad cell pointer")
                            })? = val;
                        }
                        _ => self.stack[slot] = val,
                    }
                    self.ip += 1;
                }

                Instr::FreshCell(local) => {
                    if (*local as u32) >= self.cur_local_count {
                        return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                    }
                    let slot = (self.fp + *local as u32) as usize;
                    // Read the current value, dereferencing an existing Upval.
                    let val = match &self.stack[slot] {
                        Value::Upval(c) => self
                            .cells
                            .get(*c as usize)
                            .ok_or_else(|| {
                                self.fail_not_resumable(ErrorKind::ValueError, "bad cell pointer")
                            })?
                            .clone(),
                        other => other.clone(),
                    };
                    // Allocate a fresh cell seeded with that value and point the
                    // slot at it, so subsequent captures see a per-iteration cell.
                    let idx = self.cells.len() as CellIndex;
                    self.cells.push(val);
                    self.stack[slot] = Value::Upval(idx);
                    self.ip += 1;
                }

                Instr::IncLocal(local, p, mode) => {
                    if *local as u32 >= self.cur_local_count {
                        return Err(self.fail(ErrorKind::BadLocal, "bad local"));
                    }
                    // Read current value (dereferencing boxed slots).
                    let old = match &self.stack[(self.fp + *local as u32) as usize] {
                        Value::Upval(c) => self
                            .cells
                            .get(*c as usize)
                            .ok_or_else(|| {
                                self.fail_not_resumable(ErrorKind::ValueError, "bad cell pointer")
                            })?
                            .clone(),
                        other => other.clone(),
                    };
                    // NotResumable: reads local by peek (no stack pop), so operand not consumed.
                    let old_num = old.to_number().ok_or_else(|| {
                        self.fail_not_resumable(ErrorKind::TypeError, "type error")
                    })?;
                    // Compute new value: subtract p (p = -1 for ++, p = 1 for --).
                    let new_num = old_num - *p;
                    let new_val = Value::Float(new_num);
                    // Store the new value.
                    let slot = (self.fp + *local as u32) as usize;
                    match self.stack[slot] {
                        Value::Upval(c) => {
                            let ip = self.ip;
                            *self.cells.get_mut(c as usize).ok_or_else(|| {
                                VMError::fail_at(ip, ErrorKind::ValueError, "bad cell pointer")
                            })? = new_val;
                        }
                        _ => self.stack[slot] = new_val,
                    }
                    // Push the appropriate result: old for postfix, new for prefix.
                    let result = match mode {
                        UpdateMode::Prefix => Value::Float(new_num),
                        UpdateMode::Postfix => Value::Float(old_num),
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                // ── type queries ────────────────────────────────
                Instr::TypeOf => {
                    let val = self.pop()?;
                    let tag = match val {
                        Value::Undefined => "undefined",
                        Value::Null => "object",
                        Value::Bool(_) => "boolean",
                        Value::Float(_) | Value::PosInt(_) | Value::NegInt(_) => "number",
                        Value::String(_) => "string",
                        Value::Closure { .. } | Value::Builtin(_) | Value::Bound(_) => "function",
                        Value::Array(_)
                        | Value::Object(_)
                        | Value::Promise(_)
                        | Value::RegExp(_)
                        | Value::Map(_)
                        | Value::Set(_) => "object",
                        Value::Upval(_) => {
                            return Err(self.fail(ErrorKind::ValueError, "value error"));
                        }
                    };
                    self.push_str_value(tag);
                    self.ip += 1;
                }

                // ── type predicates ─────────────────────────────
                Instr::IsNull => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(val, Value::Null)));
                    self.ip += 1;
                }
                Instr::IsBool => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(val, Value::Bool(_))));
                    self.ip += 1;
                }
                Instr::IsFloat => {
                    // True only for a Number with a fractional part (an Int is
                    // never a float). Use IsNum to test "is any number".
                    let val = self.pop()?;
                    let is_float = matches!(val, Value::Float(n) if !float_is_int(n));
                    self.stack.push(Value::Bool(is_float));
                    self.ip += 1;
                }
                Instr::IsNum => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(
                        val,
                        Value::Float(_) | Value::PosInt(_) | Value::NegInt(_)
                    )));
                    self.ip += 1;
                }
                Instr::IsStr => {
                    let val = self.pop()?;
                    let is_str = matches!(val, Value::String(_));
                    self.stack.push(Value::Bool(is_str));
                    self.ip += 1;
                }
                Instr::IsObj => {
                    let val = self.pop()?;
                    let is_obj = matches!(val, Value::Object(_) | Value::RegExp(_));
                    self.stack.push(Value::Bool(is_obj));
                    self.ip += 1;
                }
                Instr::IsMap => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(val, Value::Map(_))));
                    self.ip += 1;
                }
                Instr::IsSet => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(val, Value::Set(_))));
                    self.ip += 1;
                }

                // ── instanceof ────────────────────────────────
                // Step 2b: the structural `TypeTag` fast path
                // (`Instr::TypeCheck`) is folded into this walk — the
                // compiler evaluates the RHS to a real constructor value
                // and `instanceof` walks the `[[Prototype]]` chain via
                // `value_proto` for all receiver types (Object/Array/Map/
                // Set/RegExp/Closure/Builtin/Bound), with no special-case
                // instruction.
                Instr::InstanceOf => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    let result = self.instanceof(lhs, rhs)?;
                    self.stack.push(Value::Bool(result));
                    self.ip += 1;
                }

                // ── unary operators ─────────────────────────────
                Instr::Neg => unary_num!(|n: f64| -n),

                Instr::Not => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(!val.is_truthy()));
                    self.ip += 1;
                }

                Instr::BitNot => {
                    let val = self.pop()?;
                    match val.as_i64() {
                        Some(i) => {
                            self.stack.push(Value::Float(!i as f64));
                            self.ip += 1;
                        }
                        None => return Err(self.fail(ErrorKind::TypeError, "type error")),
                    }
                }

                // ── binary operators ────────────────────────────
                Instr::Add => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    // JS `+`: if either operand is a string, concatenate (ToString
                    // both); otherwise add numerically (ToNumber both). An
                    // array/object/function in the numeric path is a TypeError
                    // (we do not ToPrimitive it — see the note on `loose_equal`).
                    let result = if lhs.is_string() || rhs.is_string() {
                        // Pre-size the buffer when both operands are strings (the
                        // common concat path), saving incremental growth.
                        let cap = match (lhs.str_byte_len(), rhs.str_byte_len()) {
                            (Some(a), Some(b)) => a + b,
                            _ => 0,
                        };
                        let mut s = String::with_capacity(cap);
                        self.write_js_string(&lhs, 0, &mut s);
                        self.write_js_string(&rhs, 0, &mut s);
                        Value::String(RcStr::from(s))
                    } else {
                        match (lhs.to_number(), rhs.to_number()) {
                            (Some(a), Some(b)) => Value::Float(a + b),
                            _ => {
                                let msg = format!(
                                    "cannot add {} ({}) and {} ({})",
                                    lhs.type_name(),
                                    self.preview(&lhs),
                                    rhs.type_name(),
                                    self.preview(&rhs)
                                );
                                return Err(self.fail(ErrorKind::TypeError, msg));
                            }
                        }
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::Sub => binary_num!(|a: f64, b: f64| a - b),
                Instr::Mul => binary_num!(|a: f64, b: f64| a * b),
                // JS `/`: never throws — a zero divisor yields ±Infinity (or NaN
                // for 0/0), which f64 division produces directly.
                Instr::Div => binary_num!(|a: f64, b: f64| a / b),
                // JS `%`: float remainder with the dividend's sign; `x % 0` is
                // NaN. Rust's f64 `%` matches this exactly.
                Instr::Mod => binary_num!(|a: f64, b: f64| a % b),
                Instr::Pow => binary_num!(|a: f64, b: f64| a.powf(b)),

                Instr::Eq => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(Value::Bool(lhs.strict_equal(&rhs)));
                    self.ip += 1;
                }
                Instr::Neq => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(Value::Bool(!lhs.strict_equal(&rhs)));
                    self.ip += 1;
                }
                Instr::LooseEq => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(Value::Bool(lhs.loose_equal(&rhs)));
                    self.ip += 1;
                }
                Instr::LooseNeq => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(Value::Bool(!lhs.loose_equal(&rhs)));
                    self.ip += 1;
                }

                Instr::Lt => cmp_op!(== Less),
                Instr::Gt => cmp_op!(== Greater),
                Instr::LtEq => cmp_op!(!= Greater),
                Instr::GtEq => cmp_op!(!= Less),

                Instr::And => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(if lhs.is_truthy() { rhs } else { lhs });
                    self.ip += 1;
                }
                Instr::Or => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(if lhs.is_truthy() { lhs } else { rhs });
                    self.ip += 1;
                }

                Instr::BitAnd => binary_int!(|a: i64, b: i64| a & b),
                Instr::BitOr => binary_int!(|a: i64, b: i64| a | b),
                Instr::BitXor => binary_int!(|a: i64, b: i64| a ^ b),
                // Shift count must be in [0, 63]; anything else would overflow
                // (a panic in debug builds) so reject it as a ValueError.
                Instr::BitLhs => {
                    let b = self.pop_int()?;
                    let a = self.pop_int()?;
                    if !(0..64).contains(&b) {
                        return Err(self.fail(ErrorKind::ValueError, "value error"));
                    }
                    self.stack.push(Value::Float((a << b) as f64));
                    self.ip += 1;
                }
                Instr::BitRhs => {
                    let b = self.pop_int()?;
                    let a = self.pop_int()?;
                    if !(0..64).contains(&b) {
                        return Err(self.fail(ErrorKind::ValueError, "value error"));
                    }
                    self.stack.push(Value::Float((a >> b) as f64));
                    self.ip += 1;
                }
                Instr::BitURhs => {
                    let b = self.pop_int()?;
                    let a = self.pop_int()?;
                    if !(0..64).contains(&b) {
                        return Err(self.fail(ErrorKind::ValueError, "value error"));
                    }
                    self.stack
                        .push(Value::Float(((a as u64) >> (b as u32)) as f64));
                    self.ip += 1;
                }

                // ── object operations ───────────────────────────
                Instr::ObjNew(fields) => {
                    let n = fields.len();
                    if n > self.stack.len() {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    let split = self.stack.len() - n;
                    let vals: SmallVec<[Value; 16]> = self.stack.drain(split..).collect();
                    let mut obj = IndexMap::new();
                    // Left-to-right: field 0's value is the deepest (first
                    // pushed), so values line up with fields in order. `field`
                    // clones are refcount bumps on the interned `RcStr` key.
                    for (field, val) in fields.iter().zip(vals) {
                        obj.insert(field.clone(), val);
                    }
                    let obj_ptr = self.alloc_object(obj);
                    self.stack.push(obj_ptr);
                    self.ip += 1;
                }

                Instr::ObjGet(field) => {
                    // Stack-shape adapter: receiver (peeked) → pop, push val.
                    let key = Value::String(field.clone());
                    let recv = match self.stack.last() {
                        Some(r) => r.clone(),
                        None => {
                            return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                        }
                    };
                    let result = self.get_property(&recv, &key);
                    self.stack.pop();
                    self.stack.push(result?);
                    self.ip += 1;
                }

                // ObjGet minus the pop: reads the property but keeps the
                // receiver below it. Same resolution (own→proto chain) as
                // ObjGet, shared helper. obj -> obj, any
                Instr::ObjPeek(field) => {
                    let key = Value::String(field.clone());
                    let recv = match self.stack.last() {
                        Some(r) => r.clone(),
                        None => {
                            return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                        }
                    };
                    let val = self.get_property(&recv, &key)?;
                    self.stack.push(val);
                    self.ip += 1;
                }

                // ObjGetDyn minus the obj-pop: `Pick(0); IndexGet` fused.
                // obj, key -> obj, value
                Instr::ObjPeekDyn => {
                    let key = self.pop()?;
                    let container = self.stack.last().cloned().unwrap_or(Value::Undefined);
                    let val = self.get_property(&container, &key)?;
                    self.stack.push(val);
                    self.ip += 1;
                }

                // Method/value read (Step 6): resolve a named property on the
                // receiver. Step 2e: now a thin adapter over `get_property`.
                Instr::GetMethodOrProp(field) => {
                    let key = Value::String(field.clone());
                    let recv = match self.stack.last() {
                        Some(r) => r.clone(),
                        None => {
                            return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                        }
                    };
                    let result = self.get_property(&recv, &key)?;
                    self.stack.pop();
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::ObjSet(field, mode) => {
                    // Stack-shape adapter for named set. obj, val -> result.
                    let key = Value::String(field.clone());
                    let mode = *mode;
                    let val = self.pop()?;
                    let recv = match self.stack.last() {
                        Some(r) => r.clone(),
                        None => {
                            return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                        }
                    };
                    let result = self.set_property(&recv, &key, val, mode);
                    // Pop-first: consume the receiver regardless of outcome.
                    self.stack.pop();
                    self.stack.push(result?);
                    self.ip += 1;
                }

                // Runtime-polymorphic computed read.
                Instr::IndexGet => {
                    let key = self.pop()?;
                    let container = self.pop()?;
                    let val = self.get_property(&container, &key)?;
                    self.stack.push(val);
                    self.ip += 1;
                }

                // Runtime-polymorphic computed write.
                Instr::IndexSet(mode) => {
                    let mode = *mode;
                    let val = self.pop()?;
                    let key = self.pop()?;
                    let container = self.pop()?;
                    let result = self.set_property(&container, &key, val, mode)?;
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::ObjHas => {
                    // `key in obj` — obj, str -> bool.
                    let field = self.pop_string()?;
                    let recv = self
                        .stack
                        .pop()
                        .ok_or_else(|| self.fail(ErrorKind::StackUnderflow, "stack underflow"))?;
                    let key = Value::String(field);
                    // has_property: read that asks presence, not value.
                    let val = self.get_property(&recv, &key)?;
                    self.stack
                        .push(Value::Bool(!matches!(val, Value::Undefined)));
                    self.ip += 1;
                }

                Instr::ObjDelete => {
                    // `delete obj[key]` — obj, str -> bool.
                    let field = self.pop_string()?;
                    let recv = self
                        .stack
                        .pop()
                        .ok_or_else(|| self.fail(ErrorKind::StackUnderflow, "stack underflow"))?;
                    let existed = match &recv {
                        Value::Object(p) => {
                            let obj_ptr = *p;
                            let integrity = self
                                .objects
                                .get(obj_ptr as usize)
                                .map_or(IntegrityLevel::Extensible, |o| o.integrity);
                            if matches!(integrity, IntegrityLevel::Frozen | IntegrityLevel::Sealed)
                            {
                                return Err(self.fail(
                                    ErrorKind::TypeError,
                                    "cannot delete a property of a frozen or sealed object",
                                ));
                            }
                            let ip = self.ip;
                            self.objects
                                .get_mut(obj_ptr as usize)
                                .ok_or_else(|| {
                                    VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer")
                                })?
                                .map
                                .shift_remove(field.as_str())
                                .is_some()
                        }
                        Value::Closure { ptr, .. } => {
                            let ip = self.ip;
                            let c = match self.closures.get_mut(*ptr as usize) {
                                Some(c) => c,
                                _ => {
                                    return Err(VMError::fail_at(
                                        ip,
                                        ErrorKind::TypeError,
                                        "bad closure pointer",
                                    ));
                                }
                            };
                            if let Some(ref mut bag) = c.props {
                                bag.shift_remove(field.as_str()).is_some()
                            } else {
                                false
                            }
                        }
                        // Non-extensible types: delete returns false (JS).
                        _ => false,
                    };
                    self.stack.push(Value::Bool(existed));
                    self.ip += 1;
                }

                Instr::ObjExtend => {
                    // `Object.assign`-style: copy own props of src into obj.
                    // obj, src -> obj. Only Object receivers; null/undefined
                    // src is a no-op.
                    let src = self.pop()?;
                    let recv = self.pop()?;
                    let obj_ptr = match &recv {
                        Value::Object(p) => *p,
                        _ => {
                            return Err(self.fail(
                                ErrorKind::TypeError,
                                "object spread target must be an object",
                            ));
                        }
                    };
                    let integrity = self
                        .objects
                        .get(obj_ptr as usize)
                        .map_or(IntegrityLevel::Extensible, |o| o.integrity);
                    if integrity != IntegrityLevel::Extensible {
                        return Err(self.fail(
                            ErrorKind::TypeError,
                            "cannot extend a non-extensible object",
                        ));
                    }
                    let ip = self.ip;
                    match src {
                        Value::Null | Value::Undefined => {}
                        Value::Object(src_ptr) => {
                            let entries: SmallVec<[(RcStr, Value); 8]> = self
                                .objects
                                .get(src_ptr as usize)
                                .ok_or_else(|| {
                                    VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer")
                                })?
                                .map
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect();
                            let obj = self.objects.get_mut(obj_ptr as usize).ok_or_else(|| {
                                VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer")
                            })?;
                            obj.map.extend(entries);
                        }
                        _ => {
                            return Err(self.fail(
                                ErrorKind::TypeError,
                                "object spread source must be an object (or null/undefined)",
                            ));
                        }
                    }
                    self.stack.push(recv);
                    self.ip += 1;
                }

                // ── array operations ────────────────────────────
                Instr::ArrNew(n) => {
                    let n = *n as usize;
                    if n > self.stack.len() {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    let split = self.stack.len() - n;
                    // Left-to-right: first pushed becomes element 0.
                    let vals: ThinVec<Value> = self.stack.drain(split..).collect();
                    let arr_ptr = self.alloc_array(vals);
                    self.stack.push(arr_ptr);
                    self.ip += 1;
                }

                Instr::ArrExtend => {
                    let src = self.pop()?;
                    let arr_ptr = match self.pop()? {
                        Value::Array(p) => p,
                        _ => {
                            return Err(self.fail(
                                ErrorKind::TypeError,
                                "array spread target must be an array",
                            ));
                        }
                    };
                    let ip = self.ip;
                    match src {
                        Value::Array(src_ptr) => {
                            let src_elts: ThinVec<Value> = self
                                .arrays
                                .get(src_ptr as usize)
                                .ok_or_else(|| {
                                    VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer")
                                })?
                                .clone();
                            let arr = self.arrays.get_mut(arr_ptr as usize).ok_or_else(|| {
                                VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer")
                            })?;
                            arr.extend(src_elts);
                        }
                        _ => {
                            return Err(self.fail(
                                ErrorKind::TypeError,
                                "array spread source must be an array",
                            ));
                        }
                    }
                    self.stack.push(Value::Array(arr_ptr));
                    self.ip += 1;
                }

                Instr::ArrPush => {
                    let val = self.pop()?;
                    let arr_ptr = match self.pop()? {
                        Value::Array(p) => p,
                        _ => {
                            return Err(self
                                .fail(ErrorKind::TypeError, "array push target must be an array"));
                        }
                    };
                    let ip = self.ip;
                    let arr = self.arrays.get_mut(arr_ptr as usize).ok_or_else(|| {
                        VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer")
                    })?;
                    arr.push(val);
                    self.stack.push(Value::Array(arr_ptr));
                    self.ip += 1;
                }

                Instr::GetLength => {
                    let val = self.pop()?;
                    // `.length`: intrinsic byte/element count for strings and
                    // arrays; on an *object* a plain property read (JS — e.g. a
                    // RegExp match result stores its own `length`), `undefined`
                    // when absent. For a callable (Closure/Builtin/Bound), JS
                    // `fn.length` — the declared param count before the first
                    // default/rest (Step 6). Anything else (incl. map/set) is a
                    // `TypeError` — for-of lowering relies on that (it iterates
                    // only array/string, via `idx < ArrLength`).
                    let result = match val {
                        Value::String(s) => Value::Float(s.len() as f64),
                        Value::Array(p) => Value::Float(
                            self.arrays
                                .get(p as usize)
                                .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                                .len() as f64,
                        ),
                        Value::Object(p) => self
                            .objects
                            .get(p as usize)
                            .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                            .map
                            .get("length")
                            .cloned()
                            .unwrap_or(Value::Undefined),
                        Value::Closure { ptr, .. } => {
                            let arity = self
                                .closures
                                .get(ptr as usize)
                                .map(|c| c.arity)
                                .unwrap_or(0);
                            Value::Float(arity as f64)
                        }
                        Value::Builtin(b) => {
                            let meta = b.meta();
                            let n = match meta.kind {
                                crate::builtin::BuiltinKind::Method => {
                                    meta.min_args.saturating_sub(1)
                                }
                                crate::builtin::BuiltinKind::Namespace(_)
                                | crate::builtin::BuiltinKind::Constructor { .. } => meta.min_args,
                            };
                            Value::Float(n as f64)
                        }
                        Value::Bound(b) => {
                            let target = self.callable_length(&b.callable).unwrap_or(0);
                            let n = target.saturating_sub(b.bound_args.len() as u16);
                            Value::Float(n as f64)
                        }
                        _ => return Err(self.fail(ErrorKind::TypeError, "type error")),
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::GetSize => {
                    let val = self.pop()?;
                    // `.size`: intrinsic entry count for maps and sets; on an
                    // *object* the `size` property (`undefined` when absent).
                    let result = match val {
                        Value::Map(p) => Value::Float(
                            self.maps
                                .get(p as usize)
                                .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                                .len() as f64,
                        ),
                        Value::Set(p) => Value::Float(
                            self.sets
                                .get(p as usize)
                                .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                                .len() as f64,
                        ),
                        Value::Object(p) => self
                            .objects
                            .get(p as usize)
                            .ok_or_else(|| self.fail(ErrorKind::ValueError, "value error"))?
                            .map
                            .get("size")
                            .cloned()
                            .unwrap_or(Value::Undefined),
                        _ => return Err(self.fail(ErrorKind::TypeError, "type error")),
                    };
                    self.stack.push(result);
                    self.ip += 1;
                }

                Instr::ToStr => {
                    let val = self.pop()?;
                    let s = self.to_js_string(&val, 0);
                    self.stack.push(Value::String(s));
                    self.ip += 1;
                }

                Instr::ToNum => {
                    // ToNumber, matching the arithmetic operators' coercion: an
                    // array/object/function has no numeric form (TypeError).
                    let val = self.pop()?;
                    match val.to_number() {
                        Some(num) => self.stack.push(Value::Float(num)),
                        None => return Err(self.fail(ErrorKind::TypeError, "type error")),
                    }
                    self.ip += 1;
                }

                Instr::ToBool => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(val.is_truthy()));
                    self.ip += 1;
                }

                // ── external effects ───────────────────────────
                Instr::Invoke(name, nargs) => {
                    // Start (don't perform) the tool call: allocate a Pending
                    // promise, record the call in the outbox, push the promise,
                    // and continue executing. The host sees the accumulated
                    // outbox only when an `Await` blocks on a pending promise,
                    // so fan-out composes across arbitrary control flow.
                    let name = name.as_str().to_owned();
                    let n = *nargs as usize;
                    if n > self.stack.len() {
                        return Err(self.fail(ErrorKind::StackUnderflow, "stack underflow"));
                    }
                    let args = self.stack.split_off(self.stack.len() - n);
                    let id = self.alloc_promise();
                    self.outbox.push(InvokeCall {
                        promise: id,
                        name,
                        args,
                    });
                    self.stack.push(Value::Promise(id));
                    self.ip += 1;
                }

                Instr::Await => {
                    // Re-executing instruction: the operand is PEEKED while
                    // pending so the same Await can run again after the host
                    // settles the promise (`StepResult::Pending` leaves ip
                    // unchanged). A non-promise passes through unchanged.
                    let top = self.peek()?;
                    let id = match top {
                        Value::Promise(id) => *id,
                        _ => {
                            // `await x` on a plain value: the value IS the
                            // result; leave it in place.
                            self.ip += 1;
                            continue;
                        }
                    };
                    let state = self.promises.get(id as usize).ok_or_else(|| {
                        self.fail_not_resumable(ErrorKind::ValueError, "bad promise pointer")
                    })?;
                    match state {
                        PromiseState::Resolved(v) => {
                            // A promise resolved with a promise adopts it (JS:
                            // an async function returning a promise chains —
                            // a promise never resolves to a promise). Follow
                            // the chain and re-await the innermost promise in
                            // place; a cycle is the JS "chaining cycle" error.
                            if let Value::Promise(inner) = v {
                                let mut seen = vec![id];
                                let mut cur = *inner;
                                loop {
                                    if seen.contains(&cur) {
                                        self.stack.pop();
                                        return Err(self.fail(
                                            ErrorKind::TypeError,
                                            "chaining cycle detected: promise resolves to itself",
                                        ));
                                    }
                                    seen.push(cur);
                                    match self.promises.get(cur as usize) {
                                        Some(PromiseState::Resolved(Value::Promise(j))) => {
                                            cur = *j;
                                        }
                                        _ => break,
                                    }
                                }
                                *self.stack.last_mut().unwrap() = Value::Promise(cur);
                                continue; // re-execute the Await on the adoptee
                            }
                            let v = v.clone();
                            self.stack.pop();
                            self.stack.push(v);
                            self.ip += 1;
                        }
                        PromiseState::Rejected(errval) => {
                            // Inside a `try`, the rejection value itself is
                            // what `catch` receives (JS semantics: the reason
                            // passes through raw, not wrapped in an error
                            // object).
                            if self.reachable_handler() {
                                let errval = errval.clone();
                                self.stack.pop(); // the promise operand
                                self.unwind_to_handler(errval);
                                continue;
                            }
                            // Unhandled inside a resumed strand: reject the
                            // strand's own promise (propagation through an
                            // awaiting chain, as in JS) — never the host.
                            if self.in_strand() {
                                let errval = errval.clone();
                                self.stack.pop();
                                self.reject_strand(errval)?;
                                continue;
                            }
                            // Escalate via the Phase 3 path: pop the operand
                            // first (pop-first invariant), then fail resumably —
                            // the host may substitute a value for the rejection
                            // (`PushValueThenContinue`).
                            let msg = format!(
                                "awaited promise rejected with {} ({})",
                                errval.type_name(),
                                self.preview(errval)
                            );
                            self.stack.pop();
                            return Err(self.fail(ErrorKind::ValueError, msg));
                        }
                        PromiseState::Pending { .. } => {
                            // Below top level this Await is inside an async
                            // function's own frame (the parser confines
                            // `await` there): suspend exactly that frame —
                            // Tier 2's single-frame snapshot.
                            if self.callstack.len() > 1 {
                                self.suspend_current_frame(id)?;
                                continue;
                            }
                            // Top level: the root strand parks in place. Run
                            // ready continuations above the parked region
                            // first; the root's Await re-executes when the
                            // queue drains.
                            if !self.ready.is_empty() {
                                self.root_ip = self.ip;
                                self.schedule()?;
                                continue;
                            }
                            // Nothing ready: yield to the host — everything
                            // started since the last yield, ip unchanged
                            // (this Await re-executes on the next step),
                            // the promise stays on the stack. With nothing
                            // in flight either, no settlement can ever
                            // arrive: deadlock.
                            if !self.outbox.is_empty() || self.inflight > 0 {
                                let calls = std::mem::take(&mut self.outbox);
                                self.inflight += calls.len();
                                return Ok(StepResult::Pending { calls });
                            }
                            return Err(self.deadlock_error(id));
                        }
                    }
                }

                Instr::Raise(condition, argc) => {
                    let argc = *argc;
                    // Instruction contract: argc is 0 or 1 (the compiler
                    // enforces this at the source level; hand-assembled code
                    // violating it would leave stray values on the stack).
                    if argc > 1 {
                        return Err(self.fail(
                            ErrorKind::BadArg,
                            format!("Raise supports at most one payload, got {argc}"),
                        ));
                    }
                    let payload = if argc > 0 {
                        Some(self.stack.pop().ok_or_else(|| {
                            self.fail(ErrorKind::StackUnderflow, "stack underflow")
                        })?)
                    } else {
                        None
                    };
                    // Advance ip past the Raise before returning: the
                    // conceptual stack effect is (payload?) -> result,
                    // so the host resumes by pushing a result value and
                    // calling step() again (or using VM::resume_raise).
                    self.ip += 1;
                    return Ok(StepResult::Raise {
                        condition: condition.as_str().to_owned(),
                        payload,
                    });
                }

                // ── exceptions (6_LANGUAGE Part B) ──────────────
                Instr::TryEnter(addr) => {
                    let addr = self.validate_jump_addr(*addr)?;
                    self.handlers.push(HandlerEntry {
                        catch_ip: addr,
                        stack_len: self.stack.len(),
                        callstack_len: self.callstack.len(),
                        fp: self.fp,
                    });
                    self.ip += 1;
                }

                Instr::TryExit => {
                    // An unmatched TryExit is a compiler bug (NotResumable).
                    if self.handlers.pop().is_none() {
                        return Err(
                            self.fail(ErrorKind::BadArg, "TryExit without an active handler")
                        );
                    }
                    self.ip += 1;
                }

                Instr::Throw => {
                    let value = self.pop()?;
                    if self.reachable_handler() {
                        self.unwind_to_handler(value);
                        continue;
                    }
                    // A throw escaping a resumed strand rejects its promise
                    // (7_ASYNC Tier 2) — it must not unwind into the parked
                    // root strand's handlers, which belong to code that
                    // isn't executing.
                    if self.in_strand() {
                        self.reject_strand(value)?;
                        continue;
                    }
                    // NotResumable (via `fail`'s kind classification):
                    // the operand was consumed, but a `throw` has no
                    // result slot — pushing a replacement value would
                    // corrupt the statement-level stack. The thrown
                    // value rides along in `payload` so the host gets
                    // the program's own error structurally, not just a
                    // rendering.
                    let msg = self.uncaught_message(&value);
                    let mut err = self.fail(ErrorKind::UncaughtException, msg);
                    err.payload = Some(value);
                    return Err(err);
                }
            }
        }
    }
}
