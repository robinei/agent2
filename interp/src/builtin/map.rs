use crate::builtin::Args;
use crate::vm::{ErrorKind, MapKey, VM, VMError, Value};
use thin_vec::ThinVec;

/// `Map()` without `new` — throws (JS: `TypeError: Constructor Map requires
/// 'new'`). The `new Map(…)` path is handled by `Instr::New`'s
/// builtin-constructor arm (`VM::construct_builtin`).
pub fn map_ctor(vm: &mut VM, _args: Args) -> Result<Value, VMError> {
    Err(vm.fail(
        ErrorKind::TypeError,
        "cannot call `Map` as a function, use `new Map()`",
    ))
}

pub fn map_is_map(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    Ok(Value::Bool(matches!(args.get(vm, 0), Value::Map(_))))
}

pub fn map_get(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = args.map_receiver(vm)?;
    let key = args.get(vm, 1).clone();
    let map = vm
        .maps
        .get(map_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(map.get(&MapKey(key)).cloned().unwrap_or(Value::Undefined))
}

pub fn map_set(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = args.map_receiver(vm)?;
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
    let map_ptr = args.map_receiver(vm)?;
    let key = args.get(vm, 1).clone();
    let map = vm
        .maps
        .get(map_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    Ok(Value::Bool(map.contains_key(&MapKey(key))))
}

pub fn map_delete(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = args.map_receiver(vm)?;
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
    let map_ptr = args.map_receiver(vm)?;
    let ip = vm.ip;
    let map = vm
        .maps
        .get_mut(map_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::ValueError, "value error"))?;
    map.clear();
    Ok(Value::Undefined)
}

pub fn map_keys(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = args.map_receiver(vm)?;
    let map = vm
        .maps
        .get(map_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    let keys: ThinVec<Value> = map.keys().map(|k| k.0.clone()).collect();
    Ok(vm.alloc_array(keys))
}

pub fn map_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = args.map_receiver(vm)?;
    let map = vm
        .maps
        .get(map_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
    let values: ThinVec<Value> = map.values().cloned().collect();
    Ok(vm.alloc_array(values))
}

pub fn map_entries(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let map_ptr = args.map_receiver(vm)?;
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
        recv => Err(vm.method_receiver_error(recv, "a Map or a Set")),
    }
}

pub fn map_set_delete(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Map(_) => map_delete(vm, args),
        Value::Set(_) => super::set_delete(vm, args),
        recv => Err(vm.method_receiver_error(recv, "a Map or a Set")),
    }
}

pub fn map_set_clear(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Map(_) => map_clear(vm, args),
        Value::Set(_) => super::set_clear(vm, args),
        recv => Err(vm.method_receiver_error(recv, "a Map or a Set")),
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
        recv => Err(vm.method_receiver_error(recv, "a Map or a Set")),
    }
}

pub fn map_set_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Map(_) => map_values(vm, args),
        Value::Set(_) => super::set_values(vm, args),
        recv => Err(vm.method_receiver_error(recv, "a Map or a Set")),
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
        recv => Err(vm.method_receiver_error(recv, "a Map or a Set")),
    }
}

/// The `for (const x of …)` source, normalised. `for-of` lowers to an
/// index loop over a container, which an array and a string already
/// are; a `Map` iterates as `[key, value]` pairs and a `Set` as its
/// values, so those become the array the loop can actually index.
/// Anything else is passed through untouched, so the loop's own
/// `GetLength` still raises the type error for it.
///
/// Observed live 2026-09-16: a program grouped work with
/// `new Map()` — the obvious way to write it — and then
/// `for (const [path, lines] of byFile)` trapped with "cannot read
/// .length of a map". It cost three of that run's four programs, all
/// spent recovering from a dialect gap rather than on the task.
pub fn iter_source(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Map(_) => map_entries(vm, args),
        Value::Set(_) => super::set_values(vm, args),
        // **A string iterates by character, like everything else that
        // walks one.** `for-of` lowers to an index loop — `c[i]` while
        // `i < c.length` — and strings here index and measure in UTF-8
        // *bytes*, so `for (const ch of s)` stepped one byte at a time
        // and trapped on the second byte of the first non-ASCII
        // character in `s`. Not an edge: this repository's own sources
        // are full of em dashes, and `for…of` is the idiom a program
        // reaches for once told that `.length` counts bytes.
        //
        // Seen live on 2026-09-24: a model walking `report.rs` to
        // brace-match a function body was cut off mid-character, and
        // the only reason it recovered cheaply is that it gave up on
        // scanning and used `outline` instead.
        //
        // `[...s]` and `s.split("")` already both answered 3 for
        // `"a—b"` — this is the third spelling agreeing with them
        // rather than a new rule. It costs the same array those two
        // allocate.
        Value::String(s) => {
            let chars: ThinVec<Value> = s
                .as_str()
                .chars()
                .map(|c| Value::String(c.to_string().into()))
                .collect();
            Ok(vm.alloc_array(chars))
        }
        other => Ok(other.clone()),
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{Value, testutil};

    #[test]
    fn map_new_empty() {
        let v = testutil::eval("new Map()");
        assert!(matches!(v, Value::Map(_)));
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

    /// Live 2026-09-16: `for (const [path, lines] of byFile)` over a
    /// `Map` trapped with "cannot read .length of a map", and the
    /// recovery attempt `[...byFile]` trapped too. A `Map` is the
    /// obvious way to group work by key, so both now iterate.
    #[test]
    fn for_of_over_a_map_yields_key_value_pairs() {
        let out = testutil::eval_str(
            "(() => { const m = new Map(); m.set('a', 1); m.set('b', 2);              const seen = []; for (const [k, v] of m) seen.push(k + v);              return seen.join(','); })()",
        );
        assert_eq!(out, "a1,b2");
    }

    #[test]
    fn for_of_over_a_set_yields_its_values() {
        let out = testutil::eval_str(
            "(() => { const s = new Set(['a', 'b', 'a']); const seen = [];              for (const v of s) seen.push(v); return seen.join(','); })()",
        );
        assert_eq!(out, "a,b");
    }

    #[test]
    fn spreading_a_map_gives_its_entries() {
        let out = testutil::eval_str(
            "(() => [...new Map([['a', 1]])].map(p => p[0] + p[1]).join(','))()",
        );
        assert_eq!(out, "a1");
    }

    #[test]
    fn for_of_over_a_plain_value_still_fails() {
        // The normalising step passes non-iterables through untouched,
        // so the loop's own `GetLength` raises, as it always has.
        let err = testutil::run_runtime_err("for (const x of 42) {}");
        assert_eq!(err.kind, crate::ErrorKind::TypeError);
    }
}
