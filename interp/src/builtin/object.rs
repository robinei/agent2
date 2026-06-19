use crate::builtin::Args;
use crate::vm::{ErrorKind, IntegrityLevel, ObjData, RcStr, VM, VMError, Value};
use indexmap::IndexMap;
use thin_vec::ThinVec;

// ── object static implementations ────────────────────────────────────────────

/// `Object(x)` / `new Object(x)` — the constructor. Returns `x` if it is
/// already an object (Object/Array/Map/Set/RegExp/Closure/Builtin/Bound/
/// Promise), else coerces to a plain object. With no argument (or
/// `undefined`/`null`), returns `{}`. Boxed primitives (`new Number(5)`) are
/// deferred (Step 2b gives method compat without boxing); `Object(5)` in JS
/// boxes too, but here it returns `{}` since we have no wrapper — a known
/// divergence pinned in the ledger.
pub fn object_ctor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arg = args.get(vm, 0);
    match arg {
        Value::Object(_)
        | Value::Array(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::RegExp(_)
        | Value::Closure { .. }
        | Value::Builtin(_)
        | Value::Bound(_)
        | Value::Promise(_) => Ok(arg.clone()),
        Value::Undefined | Value::Null => Ok(vm.alloc_object(IndexMap::new())),
        // Primitives: JS boxes (`Object(5)` → `new Number(5)`); we return
        // `{}` (no wrapper type — Step 2b keeps method compat without boxing).
        _ => Ok(vm.alloc_object(IndexMap::new())),
    }
}

/// `Object.keys(obj)` → array of strings.
pub fn obj_keys(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let keys: Vec<RcStr> = {
        let obj = vm
            .objects
            .get(obj_ptr as usize)
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        obj.map.keys().cloned().collect()
    };
    let arr: ThinVec<Value> = keys.into_iter().map(Value::String).collect();
    Ok(vm.alloc_array(arr))
}

/// `Object.values(obj)` → array of values.
pub fn obj_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let vals: ThinVec<Value> = {
        let obj = vm
            .objects
            .get(obj_ptr as usize)
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        obj.map.values().cloned().collect()
    };
    Ok(vm.alloc_array(vals))
}

/// `Object.entries(obj)` → array of [key, value] pairs.
pub fn obj_entries(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let pairs: Vec<(RcStr, Value)> = {
        let obj = vm
            .objects
            .get(obj_ptr as usize)
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        obj.map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    let mut result: ThinVec<Value> = ThinVec::with_capacity(pairs.len());
    for (k, v) in pairs {
        let pair: ThinVec<Value> = vec![Value::String(k), v].into();
        result.push(vm.alloc_array(pair));
    }
    Ok(vm.alloc_array(result))
}

/// `Object.fromEntries(entries)` → object from [key, value] pairs.
pub fn obj_from_entries(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = match args.get(vm, 0) {
        Value::Array(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let entries = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let mut map = IndexMap::<RcStr, Value>::new();
    for entry in entries {
        let pair_ptr = match entry {
            Value::Array(p) => p,
            _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
        };
        let pair = vm
            .arrays
            .get(*pair_ptr as usize)
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        if pair.len() < 2 {
            continue;
        }
        let key = vm.to_js_string(&pair[0], 0);
        map.insert(key, pair[1].clone());
    }
    // Step 2b: use `alloc_object` so the result chains to `Object.prototype`
    // (matching JS — `Object.fromEntries([])` is a plain object).
    Ok(vm.alloc_object(map))
}

/// `Object.assign(target, ...sources)` → copies properties from sources to target,
/// returns the target.
pub fn obj_assign(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let target_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    // Copy from all source objects (args 1..) into target.
    let ip = vm.ip;
    // Collect all source entries before mutating target (avoid borrow conflict).
    let mut entries: Vec<(RcStr, Value)> = Vec::new();
    for i in 1..args.argc {
        let source_ptr = match args.get(vm, i) {
            Value::Object(p) => *p,
            _ => continue, // Non-object sources are silently skipped in JS.
        };
        let source = vm
            .objects
            .get(source_ptr as usize)
            .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer"))?;
        entries.extend(source.map.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    let target = vm
        .objects
        .get_mut(target_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer"))?;
    for (k, v) in entries {
        target.map.insert(k, v);
    }
    Ok(Value::Object(target_ptr))
}

/// `Object.hasOwn(obj, key)` → bool. Returns whether `obj` has its own
/// property `key`. Since there is no prototype chain in this dialect,
/// this is equivalent to `key in obj`.
pub fn obj_has_own(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let key = vm.to_js_string(args.get(vm, 1), 0);
    let obj = vm
        .objects
        .get(obj_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(Value::Bool(obj.map.contains_key(key.as_str())))
}

/// `obj.hasOwnProperty(key)` → bool. Instance version of `Object.hasOwn`.
///
/// Step 2c: shadowing is now uniform — an object's own `hasOwnProperty`
/// property shadows the builtin (matching JS). The former deliberate
/// divergence (non-shadowing) is retired; the unified `CallBuiltin`
/// Object-receiver path consults `resolve_method_for_object_receiver` for
/// every method name, not a whitelist.
pub fn obj_has_own_property(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let key = vm.to_js_string(args.get(vm, 1), 0);
    let obj = vm
        .objects
        .get(obj_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(Value::Bool(obj.map.contains_key(key.as_str())))
}

/// `Object.create(proto [, properties])` — creates a new object with
/// `proto` as its `[[Prototype]]` (Step 2b). `proto` must be an `Object` or
/// `null`. The optional second argument (property descriptors) is not
/// supported (descriptor tier is Step 4) — passing it is a `TypeError` so
/// the divergence is loud, not silent. The new object is extensible with
/// an empty own-property map, exactly like JS.
pub fn obj_create(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let proto = args.get(vm, 0);
    let proto_ptr = match proto {
        Value::Object(p) => Some(*p),
        Value::Null => None,
        _ => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "Object.create: prototype must be an object or null",
            ));
        }
    };
    // The optional second arg (property descriptors) is not supported.
    if !matches!(args.get(vm, 1), Value::Undefined) {
        return Err(vm.fail(
            ErrorKind::TypeError,
            "Object.create: property descriptors are not supported (Step 4 tier)",
        ));
    }
    let ptr = vm.objects.len() as u32;
    vm.objects.push(ObjData {
        proto: proto_ptr,
        map: IndexMap::new(),
        ..Default::default()
    });
    Ok(Value::Object(ptr))
}

/// `Object.getPrototypeOf(x)` → the `[[Prototype]]` of `x` as a value, or
/// `null` if `x` has no prototype (`Object.create(null)`). Step 2b: accepts
/// any value — primitives return their wrapper type's prototype
/// (`Object.getPrototypeOf(5) === Number.prototype`), structural types
/// return their type prototype (`Object.getPrototypeOf([]) ===
/// Array.prototype`), constructors return `Function.prototype`
/// (`Object.getPrototypeOf(Array) === Function.prototype`). `null`/
/// `undefined` are a `TypeError` (no wrapper coercion — JS throws too).
pub fn obj_get_proto_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let val = args.get(vm, 0).clone();
    match vm.value_proto(&val)? {
        Some(p) => Ok(Value::Object(p)),
        None => match &val {
            Value::Null | Value::Undefined => Err(vm.fail(
                ErrorKind::TypeError,
                "Object.getPrototypeOf: cannot convert primitive to object (null/undefined)",
            )),
            // Upval is an internal marker that should never reach here.
            _ => Ok(Value::Null),
        },
    }
}

/// `Object.setPrototypeOf(obj, proto)` → sets `obj`'s prototype and returns
/// `obj`. `proto` must be an `Object` or `null`. A cyclic set (where `proto`'s
/// own chain already reaches `obj`) is rejected with a `TypeError`, matching JS.
pub fn obj_set_proto_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let proto = args.get(vm, 1).clone();
    vm.set_object_proto(obj_ptr, proto)?;
    Ok(Value::Object(obj_ptr))
}

// ── Step 2d: Object.freeze / seal / preventExtensions ───────────────────────

/// `Object.freeze(obj)` — freezes the object (no add, delete, or modify) and
/// returns it. Shallow — only the object itself, not nested children.
/// MVP: receiver must be `Value::Object`; non-Object (array/map/set) is
/// deferred (documented divergence).
pub fn obj_freeze(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "Object.freeze: receiver must be an Object (arrays/maps/sets deferred)",
            ));
        }
    };
    let ip = vm.ip;
    let obj = vm
        .objects
        .get_mut(obj_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer"))?;
    obj.integrity = IntegrityLevel::Frozen;
    Ok(Value::Object(obj_ptr))
}

/// `Object.isFrozen(obj)` → bool. Returns whether the object is frozen.
/// MVP: non-Object receivers return `false` (deferred).
pub fn obj_is_frozen(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Object(p) => {
            let obj = vm
                .objects
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad object pointer"))?;
            Ok(Value::Bool(obj.integrity == IntegrityLevel::Frozen))
        }
        _ => Ok(Value::Bool(false)),
    }
}

/// `Object.seal(obj)` — seals the object (no add, no delete; modify allowed)
/// and returns it. Shallow.
/// MVP: receiver must be `Value::Object`; non-Object is deferred.
pub fn obj_seal(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "Object.seal: receiver must be an Object (arrays/maps/sets deferred)",
            ));
        }
    };
    let ip = vm.ip;
    let obj = vm
        .objects
        .get_mut(obj_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer"))?;
    // seal is a strict upgrade: from Extensible or NonExtensible → Sealed.
    // Never downgrade (a Frozen object stays Frozen).
    if matches!(
        obj.integrity,
        IntegrityLevel::Extensible | IntegrityLevel::NonExtensible
    ) {
        obj.integrity = IntegrityLevel::Sealed;
    }
    Ok(Value::Object(obj_ptr))
}

/// `Object.isSealed(obj)` → bool. Returns whether the object is sealed.
/// An object is sealed if it is at least Sealed (or Frozen).
/// MVP: non-Object receivers return `false`.
pub fn obj_is_sealed(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Object(p) => {
            let obj = vm
                .objects
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad object pointer"))?;
            Ok(Value::Bool(matches!(
                obj.integrity,
                IntegrityLevel::Sealed | IntegrityLevel::Frozen
            )))
        }
        _ => Ok(Value::Bool(false)),
    }
}

/// `Object.preventExtensions(obj)` — prevents new properties from being
/// added (modify + delete still allowed) and returns the object.
/// If already NonExtensible/Sealed/Frozen, a no-op (but the level doesn't
/// downgrade). Shallow.
/// MVP: receiver must be `Value::Object`; non-Object is deferred.
pub fn obj_prevent_extensions(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "Object.preventExtensions: receiver must be an Object (arrays/maps/sets deferred)",
            ));
        }
    };
    let ip = vm.ip;
    let obj = vm
        .objects
        .get_mut(obj_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer"))?;
    // Only upgrade; never downgrade (e.g. a sealed object stays sealed).
    if obj.integrity == IntegrityLevel::Extensible {
        obj.integrity = IntegrityLevel::NonExtensible;
    }
    Ok(Value::Object(obj_ptr))
}

/// `Object.isExtensible(obj)` → bool. Returns whether the object can have
/// new properties added. Extensible only — NonExtensible/Sealed/Frozen all
/// return false.
/// MVP: non-Object receivers return `false`.
pub fn obj_is_extensible(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Object(p) => {
            let obj = vm
                .objects
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad object pointer"))?;
            Ok(Value::Bool(obj.integrity == IntegrityLevel::Extensible))
        }
        _ => Ok(Value::Bool(false)),
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{
        Value,
        builtin::Builtin,
        testutil::{self, run_instrs, run_runtime_err},
        vm::{ErrorKind, Instr},
    };

    // ── Object.keys / Object.values ────────────────────────────────────

    #[test]
    fn call_builtin_obj_keys() {
        // ObjNew with 2 field names pops 2 values. Push them first.
        let out = run_instrs(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(2.0),
            Instr::ObjNew(vec!["a".into(), "b".into()].into()),
            Instr::CallBuiltin(Builtin::ObjKeys, 1),
        ]);
        // result is an array
        match &out[0] {
            Value::Array(_) => {}
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_obj_values() {
        let out = run_instrs(vec![
            Instr::PushFloat(5.0),
            Instr::ObjNew(vec!["x".into()].into()),
            Instr::CallBuiltin(Builtin::ObjValues, 1),
        ]);
        match &out[0] {
            Value::Array(_) => {}
            other => panic!("expected Array, got {other:?}"),
        }
    }

    // ── Step 4h: Object.entries / Object.fromEntries / Object.assign ──

    #[test]
    fn obj_entries_end_to_end() {
        // entries returns array of [key, value] pairs; iterate manually.
        assert_eq!(
            testutil::run_ret(
                "const e = Object.entries({a:1,b:2}); return [e[0][0], e[0][1], e[1][0], e[1][1]];"
            ),
            serde_json::json!(["a", 1, "b", 2])
        );
    }

    #[test]
    fn obj_from_entries() {
        assert_eq!(
            testutil::run_ret("return Object.fromEntries([['a', 1]]);"),
            serde_json::json!({"a": 1})
        );
    }

    #[test]
    fn obj_from_entries_round_trip() {
        assert_eq!(
            testutil::run_ret("const o = {a:1,b:2}; return Object.fromEntries(Object.entries(o));"),
            serde_json::json!({"a": 1, "b": 2})
        );
    }

    #[test]
    fn obj_assign_returns_target() {
        assert_eq!(
            testutil::run_ret("const t = {}; const r = Object.assign(t, {a:1}); return r === t;"),
            serde_json::json!(true)
        );
    }

    #[test]
    fn obj_assign_later_sources_win() {
        assert_eq!(
            testutil::run_ret("return Object.assign({a:1}, {a:2, b:3});"),
            serde_json::json!({"a": 2, "b": 3})
        );
    }

    #[test]
    fn obj_has_own() {
        assert_eq!(
            testutil::run_ret("return Object.hasOwn({a: 1}, 'a');"),
            serde_json::json!(true)
        );
        assert_eq!(
            testutil::run_ret("return Object.hasOwn({a: 1}, 'b');"),
            serde_json::json!(false)
        );
        assert_eq!(
            testutil::run_ret("return Object.hasOwn({}, 'toString');"),
            serde_json::json!(false)
        );
    }

    #[test]
    fn obj_has_own_property() {
        assert_eq!(
            testutil::run_ret("return ({a: 1}).hasOwnProperty('a');"),
            serde_json::json!(true)
        );
        assert_eq!(
            testutil::run_ret("return ({a: 1}).hasOwnProperty('b');"),
            serde_json::json!(false)
        );
    }

    // ── Step 2d: Object.freeze / seal / preventExtensions ──────────────

    #[test]
    fn freeze_blocks_writes() {
        let err = run_runtime_err("const o = {a:1}; Object.freeze(o); o.a = 2; return o;");
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn freeze_returns_object() {
        assert_eq!(
            testutil::run_ret("const o = {}; return Object.freeze(o) === o;"),
            serde_json::json!(true)
        );
    }

    #[test]
    fn is_frozen_after_freeze() {
        assert_eq!(
            testutil::run_ret(
                "const o = {}; return [Object.isFrozen(o), Object.freeze(o), Object.isFrozen(o)];"
            ),
            serde_json::json!([false, {}, true])
        );
    }

    #[test]
    fn freeze_blocks_delete() {
        let err = run_runtime_err("const o = {a:1}; Object.freeze(o); delete o.a;");
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn freeze_blocks_new_keys() {
        let err = run_runtime_err("const o = {}; Object.freeze(o); o.x = 1;");
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn seal_blocks_add_and_delete_but_allows_modify() {
        assert_eq!(
            testutil::run_ret("const o = {a:1}; Object.seal(o); o.a = 2; return o.a;"),
            serde_json::json!(2)
        );
        let err = run_runtime_err("const o = {a:1}; Object.seal(o); o.x = 1;");
        assert_eq!(err.kind, ErrorKind::TypeError);
        let err = run_runtime_err("const o = {a:1}; Object.seal(o); delete o.a;");
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn is_sealed_after_seal() {
        assert_eq!(
            testutil::run_ret(
                "const o = {}; return [Object.isSealed(o), Object.seal(o), Object.isSealed(o)];"
            ),
            serde_json::json!([false, {}, true])
        );
    }

    #[test]
    fn prevent_extensions_blocks_new_keys_but_allows_modify_and_delete() {
        assert_eq!(
            testutil::run_ret("const o = {a:1}; Object.preventExtensions(o); o.a = 2; return o.a;"),
            serde_json::json!(2)
        );
        let err = run_runtime_err("const o = {a:1}; Object.preventExtensions(o); o.x = 1;");
        assert_eq!(err.kind, ErrorKind::TypeError);
        assert_eq!(
            testutil::run_ret(
                "const o = {a:1}; Object.preventExtensions(o); delete o.a; return o.a === undefined;"
            ),
            serde_json::json!(true)
        );
    }

    #[test]
    fn is_extensible_tracks_prevent_extensions() {
        assert_eq!(
            testutil::run_ret(
                "const o = {}; const before = Object.isExtensible(o); Object.preventExtensions(o); return [before, Object.isExtensible(o)];"
            ),
            serde_json::json!([true, false])
        );
    }

    #[test]
    fn freeze_is_shallow() {
        // freeze is shallow: a nested object stays mutable.
        assert_eq!(
            testutil::run_ret("const o = {a: {b: 1}}; Object.freeze(o); o.a.b = 2; return o.a.b;"),
            serde_json::json!(2)
        );
    }

    #[test]
    fn frozen_object_serializes_as_plain_data() {
        // A frozen ordinary object still serializes to JSON — the level is
        // dropped, per the invariant boundary. (Builtin prototypes are
        // rejected by `kind`, not by `integrity`.)
        assert_eq!(
            testutil::run_ret("const o = {a:1}; Object.freeze(o); return o;"),
            serde_json::json!({ "a": 1 })
        );
    }

    #[test]
    fn freeze_seal_prevent_extensions_progression() {
        // Each step is strictly stronger; `seal` subsumes `preventExtensions`.
        assert_eq!(
            testutil::run_ret(
                "const o = {}; Object.preventExtensions(o); Object.seal(o); return [Object.isExtensible(o), Object.isSealed(o), Object.isFrozen(o)];"
            ),
            serde_json::json!([false, true, false])
        );
        assert_eq!(
            testutil::run_ret(
                "const o = {}; Object.seal(o); Object.freeze(o); return Object.isFrozen(o);"
            ),
            serde_json::json!(true)
        );
    }

    #[test]
    fn seal_after_freeze_is_noop() {
        assert_eq!(
            testutil::run_ret(
                "const o = {}; Object.freeze(o); Object.seal(o); return Object.isFrozen(o);"
            ),
            serde_json::json!(true)
        );
    }

    #[test]
    fn prevent_extensions_after_seal_is_noop() {
        // preventExtensions on a sealed object does not downgrade.
        assert_eq!(
            testutil::run_ret(
                "const o = {}; Object.seal(o); Object.preventExtensions(o); return Object.isSealed(o);"
            ),
            serde_json::json!(true)
        );
    }

    #[test]
    fn freeze_on_non_object_is_deferred() {
        let err = run_runtime_err("Object.freeze([]);");
        assert_eq!(err.kind, ErrorKind::TypeError);
        let err = run_runtime_err("Object.freeze(new Map());");
        assert_eq!(err.kind, ErrorKind::TypeError);
        let err = run_runtime_err("Object.freeze(new Set());");
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn is_frozen_on_non_object_is_false() {
        assert_eq!(
            testutil::run_ret("return Object.isFrozen([]);"),
            serde_json::json!(false)
        );
        assert_eq!(
            testutil::run_ret("return Object.isFrozen(5);"),
            serde_json::json!(false)
        );
    }

    #[test]
    fn is_extensible_on_non_object_is_false() {
        assert_eq!(
            testutil::run_ret("return Object.isExtensible([]);"),
            serde_json::json!(false)
        );
    }

    #[test]
    fn freeze_on_object_prototype_is_noop() {
        // Builtin prototypes are already Frozen (Step 2a); freeze on a
        // Frozen `BuiltinPrototype` object is a no-op. The integrity field
        // is the *same* one — user freeze writes the same `ObjData.integrity`
        // that the 2a builtin-prototype freeze used.
        assert_eq!(
            testutil::run_ret("return Object.freeze(Object.prototype) === Object.prototype;"),
            serde_json::json!(true)
        );
    }
}
