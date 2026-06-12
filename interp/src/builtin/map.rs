use crate::builtin::Args;
use crate::vm::{ErrorKind, MapKey, VM, VMError, Value};
use thin_vec::ThinVec;

pub fn map_is_map(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    Ok(Value::Bool(matches!(args.get(vm, 0), Value::Map(_))))
}

pub fn map_get(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = match args.get(vm, 0) {
        Value::Map(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let key = args.get(vm, 1).clone();
    let map = vm
        .maps
        .get(map_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(map.get(&MapKey(key)).cloned().unwrap_or(Value::Undefined))
}

pub fn map_set(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = match args.get(vm, 0) {
        Value::Map(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let key = args.get(vm, 1).clone();
    let value = args.get(vm, 2).clone();
    let ip = vm.ip;
    let map = vm
        .maps
        .get_mut(map_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::ValueError, "value error"))?;
    map.insert(MapKey(key), value);
    Ok(Value::Map(map_ptr))
}

pub fn map_has(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = match args.get(vm, 0) {
        Value::Map(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let key = args.get(vm, 1).clone();
    let map = vm
        .maps
        .get(map_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(Value::Bool(map.contains_key(&MapKey(key))))
}

pub fn map_delete(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = match args.get(vm, 0) {
        Value::Map(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let key = args.get(vm, 1).clone();
    let ip = vm.ip;
    let map = vm
        .maps
        .get_mut(map_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::ValueError, "value error"))?;
    let removed = map.shift_remove(&MapKey(key)).is_some();
    Ok(Value::Bool(removed))
}

pub fn map_clear(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = match args.get(vm, 0) {
        Value::Map(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let ip = vm.ip;
    let map = vm
        .maps
        .get_mut(map_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::ValueError, "value error"))?;
    map.clear();
    Ok(Value::Undefined)
}

pub fn map_keys(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = match args.get(vm, 0) {
        Value::Map(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let map = vm
        .maps
        .get(map_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    let keys: ThinVec<Value> = map.keys().map(|k| k.0.clone()).collect();
    Ok(vm.alloc_array(keys))
}

pub fn map_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = match args.get(vm, 0) {
        Value::Map(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let map = vm
        .maps
        .get(map_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    let values: ThinVec<Value> = map.values().cloned().collect();
    Ok(vm.alloc_array(values))
}

pub fn map_entries(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = match args.get(vm, 0) {
        Value::Map(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let pairs: Vec<(Value, Value)> = {
        let map = vm
            .maps
            .get(map_ptr as usize)
            .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
        map.iter().map(|(k, v)| (k.0.clone(), v.clone())).collect()
    };
    let mut result: ThinVec<Value> = ThinVec::with_capacity(pairs.len());
    for (k, v) in pairs {
        let pair: ThinVec<Value> = vec![k, v].into();
        result.push(vm.alloc_array(pair));
    }
    Ok(vm.alloc_array(result))
}

// ── Map/Set shared polymorphic handlers ────────────────────────────────────

pub fn map_set_has(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Map(_) => map_has(vm, args),
        Value::Set(_) => super::set_has(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

pub fn map_set_delete(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Map(_) => map_delete(vm, args),
        Value::Set(_) => super::set_delete(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

pub fn map_set_clear(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Map(_) => map_clear(vm, args),
        Value::Set(_) => super::set_clear(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

pub fn map_set_keys(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Map(_) => map_keys(vm, args),
        Value::Set(p) => {
            // Set.keys() returns the same as values()
            let set = vm
                .sets
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
            let values: ThinVec<Value> = set.iter().map(|k| k.0.clone()).collect();
            Ok(vm.alloc_array(values))
        }
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

pub fn map_set_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Map(_) => map_values(vm, args),
        Value::Set(_) => super::set_values(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

pub fn map_set_entries(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Map(_) => map_entries(vm, args),
        Value::Set(p) => {
            // Set.entries() returns [value, value] pairs
            let set = vm
                .sets
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
            let values: ThinVec<Value> = set.iter().map(|k| k.0.clone()).collect();
            let mut result: ThinVec<Value> = ThinVec::with_capacity(values.len());
            for v in values {
                let pair: ThinVec<Value> = vec![v.clone(), v].into();
                result.push(vm.alloc_array(pair));
            }
            Ok(vm.alloc_array(result))
        }
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
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

    #[test]
    fn map_new_empty() {
        let out = run_instrs(vec![Instr::PushUndefined, Instr::MapNew]);
        assert!(matches!(&out[0], Value::Map(_)));
    }

    #[test]
    fn map_set_and_get() {
        let out = testutil::run_ret(
            "const m = new Map(); m.set('a', 1); return [m.get('a'), m.get('b')];",
        );
        assert_eq!(out, serde_json::json!([1, null]));
    }

    #[test]
    fn map_has_and_delete() {
        let out = testutil::run_ret(
            "const m = new Map(); m.set('a', 1); const had = m.has('a'); m.delete('a'); return [had, m.has('a')];",
        );
        assert_eq!(out, serde_json::json!([true, false]));
    }

    #[test]
    fn map_update_existing_key() {
        let out = testutil::run_ret(
            "const m = new Map(); m.set('a', 1); m.set('a', 2); return m.get('a');",
        );
        assert_eq!(out, serde_json::json!(2));
    }

    #[test]
    fn map_clear() {
        let out = testutil::run_ret(
            "const m = new Map([['a', 1], ['b', 2]]); m.clear(); return m.has('a');",
        );
        assert_eq!(out, serde_json::json!(false));
    }

    #[test]
    fn map_keys_values_entries() {
        let out = testutil::run_ret(
            "const m = new Map([['a', 1], ['b', 2]]); return [m.keys(), m.values(), m.entries()];",
        );
        assert_eq!(
            out,
            serde_json::json!([["a", "b"], [1, 2], [["a", 1], ["b", 2]]])
        );
    }

    #[test]
    fn map_constructor_from_pairs() {
        let out = testutil::run_ret(
            "const m = new Map([['x', 10], ['y', 20]]); return [m.get('x'), m.get('y')];",
        );
        assert_eq!(out, serde_json::json!([10, 20]));
    }

    #[test]
    fn map_is_map() {
        let out = testutil::run_ret("const m = new Map(); return [Map.isMap(m), Map.isMap({})];");
        assert_eq!(out, serde_json::json!([true, false]));
    }
}
