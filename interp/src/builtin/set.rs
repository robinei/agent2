use crate::builtin::Args;
use crate::vm::{ErrorKind, MapKey, VM, VMError, Value};
use thin_vec::ThinVec;

pub fn set_is_set(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    Ok(Value::Bool(matches!(args.get(vm, 0), Value::Set(_))))
}

pub fn set_add(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let set_ptr = match args.get(vm, 0) {
        Value::Set(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let value = args.get(vm, 1).clone();
    let ip = vm.ip;
    let set = vm
        .sets
        .get_mut(set_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::ValueError, "value error"))?;
    set.insert(MapKey(value));
    Ok(Value::Set(set_ptr))
}

pub fn set_has(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let set_ptr = match args.get(vm, 0) {
        Value::Set(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let value = args.get(vm, 1).clone();
    let set = vm
        .sets
        .get(set_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(Value::Bool(set.contains(&MapKey(value))))
}

pub fn set_delete(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let set_ptr = match args.get(vm, 0) {
        Value::Set(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let value = args.get(vm, 1).clone();
    let ip = vm.ip;
    let set = vm
        .sets
        .get_mut(set_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::ValueError, "value error"))?;
    let removed = set.shift_remove(&MapKey(value));
    Ok(Value::Bool(removed))
}

pub fn set_clear(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let set_ptr = match args.get(vm, 0) {
        Value::Set(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let ip = vm.ip;
    let set = vm
        .sets
        .get_mut(set_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::ValueError, "value error"))?;
    set.clear();
    Ok(Value::Undefined)
}

pub fn set_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let set_ptr = match args.get(vm, 0) {
        Value::Set(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let set = vm
        .sets
        .get(set_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    let values: ThinVec<Value> = set.iter().map(|k| k.0.clone()).collect();
    Ok(vm.alloc_array(values))
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
    fn set_new_empty() {
        let out = run_instrs(vec![Instr::PushUndefined, Instr::SetNew]);
        assert!(matches!(&out[0], Value::Set(_)));
    }

    #[test]
    fn set_add_and_has() {
        let out = testutil::run_ret("const s = new Set(); s.add(1); return [s.has(1), s.has(2)];");
        assert_eq!(out, serde_json::json!([true, false]));
    }

    #[test]
    fn set_deduplication() {
        let out =
            testutil::run_ret("const s = new Set(); s.add(1); s.add(1); return s.values().length;");
        assert_eq!(out, serde_json::json!(1));
    }

    #[test]
    fn set_delete() {
        let out = testutil::run_ret(
            "const s = new Set([1, 2]); s.delete(1); return [s.has(1), s.has(2)];",
        );
        assert_eq!(out, serde_json::json!([false, true]));
    }

    #[test]
    fn set_clear() {
        let out = testutil::run_ret("const s = new Set([1, 2, 3]); s.clear(); return s.has(1);");
        assert_eq!(out, serde_json::json!(false));
    }

    #[test]
    fn set_values() {
        let out = testutil::run_ret("const s = new Set([1, 2, 3]); return s.values();");
        assert_eq!(out, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn set_is_set() {
        let out =
            testutil::run_ret("const s = new Set(); return [Set.isSet(s), Set.isSet([1,2])];");
        assert_eq!(out, serde_json::json!([true, false]));
    }

    #[test]
    fn set_size() {
        let out = testutil::run_ret("const s = new Set([1, 2, 3]); return s.size;");
        assert_eq!(out, serde_json::json!(3));
    }

    #[test]
    fn map_size() {
        let out = testutil::run_ret("const m = new Map([['a', 1], ['b', 2]]); return m.size;");
        assert_eq!(out, serde_json::json!(2));
    }
}
