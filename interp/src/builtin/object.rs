use crate::builtin::Args;
use crate::vm::{ErrorKind, VM, VMError, Value};
use thin_vec::ThinVec;

// ── object static implementations ────────────────────────────────────────────

/// `Object.keys(obj)` → array of strings.
pub fn obj_keys(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let keys: ThinVec<Value> = vm
        .objects
        .get(obj_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?
        .keys()
        .map(|k| Value::String(k.clone()))
        .collect();
    Ok(vm.alloc_array(keys))
}

/// `Object.values(obj)` → array of values.
pub fn obj_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => return Err(vm.fail(ErrorKind::TypeError, "type error")),
    };
    let vals: ThinVec<Value> = vm
        .objects
        .get(obj_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?
        .values()
        .cloned()
        .collect();
    Ok(vm.alloc_array(vals))
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{Value, builtin::Builtin, testutil::run_instrs, vm::Instr};

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
}
