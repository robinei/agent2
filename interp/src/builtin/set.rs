use crate::builtin::Args;
use crate::vm::{ErrorKind, MapKey, VM, VMError, Value};
use thin_vec::ThinVec;

/// `Set()` without `new` — throws (JS: `TypeError: Constructor Set requires
/// 'new'`). The `new Set(…)` path is handled by `Instr::New`'s
/// builtin-constructor arm (`VM::construct_builtin`).
pub fn set_ctor(vm: &mut VM, _args: Args) -> Result<Value, VMError> {
    Err(vm.fail(
        ErrorKind::TypeError,
        "cannot call `Set` as a function, use `new Set()`",
    ))
}

pub fn set_is_set(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    Ok(Value::Bool(matches!(args.get(vm, 0), Value::Set(_))))
}

pub fn set_add(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let set_ptr = args.set_receiver(vm)?;
    let value = args.get(vm, 1).clone();
    let ip = vm.ip;
    let set = vm
        .sets
        .get_mut(set_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::BadPointer, "bad set pointer"))?;
    set.insert(MapKey(value));
    Ok(Value::Set(set_ptr))
}

pub fn set_has(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let set_ptr = args.set_receiver(vm)?;
    let value = args.get(vm, 1).clone();
    let set = vm
        .sets
        .get(set_ptr as usize)
        .ok_or_else(|| vm.fail_invariant(ErrorKind::BadPointer, "bad set pointer"))?;
    Ok(Value::Bool(set.contains(&MapKey(value))))
}

pub fn set_delete(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let set_ptr = args.set_receiver(vm)?;
    let value = args.get(vm, 1).clone();
    let ip = vm.ip;
    let set = vm
        .sets
        .get_mut(set_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::BadPointer, "bad set pointer"))?;
    let removed = set.shift_remove(&MapKey(value));
    Ok(Value::Bool(removed))
}

pub fn set_clear(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let set_ptr = args.set_receiver(vm)?;
    let ip = vm.ip;
    let set = vm
        .sets
        .get_mut(set_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::BadPointer, "bad set pointer"))?;
    set.clear();
    Ok(Value::Undefined)
}

pub fn set_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let set_ptr = args.set_receiver(vm)?;
    let set = vm
        .sets
        .get(set_ptr as usize)
        .ok_or_else(|| vm.fail_invariant(ErrorKind::BadPointer, "bad set pointer"))?;
    let values: ThinVec<Value> = set.iter().map(|k| k.0.clone()).collect();
    Ok(vm.alloc_array(values))
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{Value, testutil};

    #[test]
    fn set_new_empty() {
        let v = testutil::eval("new Set()");
        assert!(matches!(v, Value::Set(_)));
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

    // ── new Set(iterable) — sweep-200 2026-09-17: only `Array` worked ──

    #[test]
    fn set_construct_from_string() {
        // `new Set("abc")` iterates characters, like real JS — not just
        // `new Set(Array)`.
        let out = testutil::run_ret("return new Set('abc').size;");
        assert_eq!(out, serde_json::json!(3));
    }

    #[test]
    fn set_construct_from_string_dedupes() {
        let out = testutil::run_ret("return new Set('aab').size;");
        assert_eq!(out, serde_json::json!(2));
    }

    #[test]
    fn set_construct_from_other_set() {
        let out = testutil::run_ret(
            "const a = new Set([1, 2, 3]); const b = new Set(a); return [b.size, b.has(2)];",
        );
        assert_eq!(out, serde_json::json!([3, true]));
    }

    #[test]
    fn set_construct_from_map_keys() {
        let out = testutil::run_ret(
            "const m = new Map([['a', 1], ['b', 2]]); \
             const s = new Set(m.keys()); return [s.size, s.has('a'), s.has('b')];",
        );
        assert_eq!(out, serde_json::json!([2, true, true]));
    }

    #[test]
    fn set_construct_from_map_yields_entries() {
        // Iterating a `Map` directly (not `.keys()`) yields `[key, value]`
        // entries, same as real JS.
        let out = testutil::run_ret(
            "const m = new Map([['a', 1]]); const s = new Set(m); \
             return [s.size, s.values()[0]];",
        );
        assert_eq!(out, serde_json::json!([1, ["a", 1]]));
    }

    #[test]
    fn set_construct_non_iterable_still_throws() {
        use crate::testutil::run_err_kind;
        use crate::vm::ErrorKind;
        // A plain number is not one of this dialect's iterables.
        assert_eq!(run_err_kind("return new Set(5);"), ErrorKind::TypeError);
    }

    #[test]
    fn set_construct_plain_object_still_throws() {
        use crate::testutil::run_err_kind;
        use crate::vm::ErrorKind;
        assert_eq!(
            run_err_kind("return new Set({a: 1});"),
            ErrorKind::TypeError
        );
    }

    // ── new Map(iterable) — same gap, same fix ─────────────────────────

    #[test]
    fn map_construct_from_other_map() {
        let out = testutil::run_ret(
            "const a = new Map([['a', 1], ['b', 2]]); const b = new Map(a); \
             return [b.size, b.get('a'), b.get('b')];",
        );
        assert_eq!(out, serde_json::json!([2, 1, 2]));
    }

    #[test]
    fn map_construct_from_set_of_pairs() {
        let out = testutil::run_ret(
            "const s = new Set([['a', 1], ['b', 2]]); const m = new Map(s); \
             return [m.get('a'), m.get('b')];",
        );
        assert_eq!(out, serde_json::json!([1, 2]));
    }

    #[test]
    fn map_construct_non_iterable_still_throws() {
        use crate::testutil::run_err_kind;
        use crate::vm::ErrorKind;
        assert_eq!(run_err_kind("return new Map(5);"), ErrorKind::TypeError);
    }
}
