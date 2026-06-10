use crate::builtin::Args;
use crate::vm::{ErrorKind, RcStr, VM, VMError, Value};

// ── JSON static implementations ──────────────────────────────────────────────

/// `JSON.parse(s)` → any.
pub fn json_parse(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let s = vm.string_from(args.get(vm, 0))?;
    let json: serde_json::Value =
        serde_json::from_str(&s).map_err(|_| vm.fail(ErrorKind::ValueError, "value error"))?;
    vm.json_to_stack_value(&json, 0)
}

/// `JSON.stringify(x)` → str.
pub fn json_stringify(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let json = vm.stack_value_to_json(args.get(vm, 0), 0)?;
    let s =
        serde_json::to_string(&json).map_err(|_| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(Value::String(RcStr::from(s)))
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{Value, builtin::Builtin, testutil::run_instrs, vm::Instr};

    // ── JSON.parse / JSON.stringify ────────────────────────────────────

    #[test]
    fn call_builtin_json_parse() {
        let out = run_instrs(vec![
            Instr::PushStr("42".into()),
            Instr::CallBuiltin(Builtin::JSONParse, 1),
        ]);
        assert_eq!(out, vec![Value::PosInt(42)]);
    }

    #[test]
    fn call_builtin_json_stringify() {
        let out = run_instrs(vec![
            Instr::PushFloat(3.5),
            Instr::CallBuiltin(Builtin::JSONStringify, 1),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "3.5"),
            other => panic!("expected string, got {other:?}"),
        }
    }
}
