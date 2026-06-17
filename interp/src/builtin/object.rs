use crate::builtin::Args;
use crate::vm::{ErrorKind, ObjData, RcStr, VM, VMError, Value};
use indexmap::IndexMap;
use thin_vec::ThinVec;

// ── object static implementations ────────────────────────────────────────────

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
    let addr = vm.objects.len() as u32;
    vm.objects.push(ObjData { proto: None, map });
    Ok(Value::Object(addr))
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
/// Deliberate divergence from JS / from the other method builtins' shadowing:
/// this is the one method builtin that *accepts* an `Object` receiver, so it
/// just succeeds — an object's own `hasOwnProperty` property does **not**
/// shadow it (in JS it would). We accept that to keep this builtin on the fast
/// path: overriding `hasOwnProperty` is never a useful pattern here, and the
/// alternative (a self-shadow `contains_key` on every call) would tax the
/// common case to honor one nobody wants. The generic "Object receiver ⇒
/// re-route" signal in the `call()` epilogue only fires on *error*, which this
/// builtin never raises for an Object, so nothing special is needed.
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

/// `Object.getPrototypeOf(obj)` → the prototype of `obj`, or `null` if none.
/// Only accepts `Value::Object`; non-Object args are a TypeError (no wrapper
/// coercion).
pub fn obj_get_proto_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let obj = vm
        .objects
        .get(obj_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    match obj.proto {
        Some(p) => Ok(Value::Object(p)),
        None => Ok(Value::Null),
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
    let new_proto = match args.get(vm, 1) {
        Value::Object(p) => Some(*p),
        Value::Null => None,
        _ => {
            return Err(vm.fail(ErrorKind::TypeError, "prototype must be an object or null"));
        }
    };
    // Cycle check: walk `new_proto`'s chain to see if it reaches `obj_ptr`.
    if let Some(proto_ptr) = new_proto {
        const MAX_PROTO_DEPTH: u32 = 100;
        let mut cur = Some(proto_ptr);
        for _ in 0..MAX_PROTO_DEPTH {
            match cur {
                Some(p) if p == obj_ptr => {
                    return Err(vm.fail(ErrorKind::TypeError, "cyclic prototype chain"));
                }
                Some(p) => {
                    let o = vm
                        .objects
                        .get(p as usize)
                        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
                    cur = o.proto;
                }
                None => break,
            }
        }
    }
    vm.objects[obj_ptr as usize].proto = new_proto;
    Ok(Value::Object(obj_ptr))
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{
        Value,
        builtin::Builtin,
        testutil::{self, run_instrs},
        vm::Instr,
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
}
