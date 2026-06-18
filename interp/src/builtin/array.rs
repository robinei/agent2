use crate::builtin::Args;
use crate::vm::{ErrorKind, RcStr, VM, VMError, Value};
use smallvec::SmallVec;
use thin_vec::ThinVec;

// ── Array static implementations ─────────────────────────────────────────────

/// `Array.isArray(x)` → bool.
pub fn array_is_array(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    Ok(Value::Bool(matches!(args.get(vm, 0), Value::Array(_))))
}

/// `Array.from(items)` → new array. Copies from an existing array, or
/// creates an array of `length` undefineds from an array-like object.
pub fn array_from(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let items = args.get(vm, 0);
    match items {
        Value::Array(p) => {
            let arr = vm
                .arrays
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
            Ok(vm.alloc_array(arr.clone()))
        }
        Value::Object(p) => {
            let obj = vm
                .objects
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::ValueError, "value error"))?;
            let len = obj
                .map
                .get("length")
                .and_then(|v| v.to_number())
                .unwrap_or(0.0);
            let n = (len as usize).min(10_000_000);
            let mut out: ThinVec<Value> = ThinVec::with_capacity(n);
            for i in 0..n {
                let key = crate::rc_str::RcStr::from(i.to_string());
                let v = obj.map.get(&key).cloned().unwrap_or(Value::Undefined);
                out.push(v);
            }
            Ok(vm.alloc_array(out))
        }
        Value::Undefined | Value::Null => Ok(vm.alloc_array(ThinVec::new())),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

// ── array method implementations ─────────────────────────────────────────────

/// `arr.push(a, b, …)` → appends all arguments and returns the new length.
pub fn array_push(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let ip = vm.ip;
    // Clone the values to push first (immutable borrow of vm.stack), then
    // mutate the array.
    let to_push: SmallVec<[Value; 8]> = args.slice(vm)[1..].iter().cloned().collect();
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    for v in to_push {
        arr.push(v);
    }
    Ok(Value::int_from_f64(arr.len() as f64))
}

/// `arr.pop()` → removes and returns the last element, or `undefined` if empty.
pub fn array_pop(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    // JS: empty array → undefined, not an error.
    Ok(arr.pop().unwrap_or(Value::Undefined))
}

/// `arr.shift()` → removes and returns the first element, or `undefined` if empty.
pub fn array_shift(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    // JS: empty array → undefined, not an error.
    if arr.is_empty() {
        return Ok(Value::Undefined);
    }
    Ok(arr.remove(0))
}

/// `arr.unshift(a, b, …)` → prepends all arguments (preserving order) and
/// returns the new length.
pub fn array_unshift(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let ip = vm.ip;
    // Clone the values first (immutable borrow), then mutate.
    let to_insert: SmallVec<[Value; 8]> = args.slice(vm)[1..].iter().cloned().collect();
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    // Insert in reverse so order is preserved: unshift(1,2) → [1,2,...]
    for v in to_insert.into_iter().rev() {
        arr.insert(0, v);
    }
    Ok(Value::int_from_f64(arr.len() as f64))
}

/// `arr.join([sep])` → joins with sep (default ",").
pub fn array_join(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let sep = match args.get(vm, 1) {
        Value::Undefined => RcStr::from(","),
        v => vm.to_js_string(v, 0),
    };
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let mut joined = String::new();
    for (i, v) in arr.iter().enumerate() {
        if i > 0 {
            joined.push_str(sep.as_str());
        }
        // JS: null/undefined elements contribute the empty string.
        if !matches!(v, Value::Null | Value::Undefined) {
            joined.push_str(vm.to_js_string(v, 0).as_str());
        }
    }
    Ok(Value::String(RcStr::from(joined)))
}

/// `arr.reverse()` → reverses in-place, returns the receiver.
pub fn array_reverse(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    arr.reverse();
    Ok(Value::Array(arr_ptr))
}

/// `arr.flat([depth])` → flattens nested arrays to the given depth (default 1).
pub fn array_flat(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let depth = match args.get(vm, 1) {
        Value::Undefined => 1usize,
        v => v
            .to_number()
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))? as usize,
    };
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    let mut result: ThinVec<Value> = ThinVec::new();
    flatten_into(vm, arr, depth, &mut result)?;
    Ok(vm.alloc_array(result))
}

pub fn flatten_into(
    vm: &VM,
    src: &[Value],
    depth: usize,
    out: &mut ThinVec<Value>,
) -> Result<(), VMError> {
    for v in src {
        if depth > 0
            && let Value::Array(p) = v
        {
            let nested = vm
                .arrays
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad array pointer"))?;
            flatten_into(vm, nested, depth - 1, out)?;
            continue;
        }
        out.push(v.clone());
    }
    Ok(())
}

/// `arr.fill(value[, start[, end]])` → fills in-place, returns the receiver.
pub fn array_fill(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let value = args.get(vm, 1).clone();
    // Extract start/end values before borrowing arr.
    let start_arg = args.get(vm, 2).clone();
    let end_arg = args.get(vm, 3).clone();
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    let len = arr.len() as i64;
    let to_idx = |v: &Value, default: i64| -> i64 {
        if matches!(v, Value::Undefined) {
            return default;
        }
        v.to_number()
            .map(|n| {
                let i = n as i64;
                if i < 0 { (i + len).max(0) } else { i.min(len) }
            })
            .unwrap_or(default)
    };
    let start = to_idx(&start_arg, 0).max(0) as usize;
    let end = to_idx(&end_arg, len).max(0) as usize;
    let end = end.min(arr.len());
    for i in start..end {
        arr[i] = value.clone();
    }
    Ok(Value::Array(arr_ptr))
}

/// `arr.splice(start[, deleteCount[, ...items]])` → mutates in-place, returns
/// the removed elements.
pub fn array_splice(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    // Extract all args before mutable borrow.
    let start_val = args.get(vm, 1).clone();
    let del_val = if args.argc >= 3 {
        Some(args.get(vm, 2).clone())
    } else {
        None
    };
    let to_insert: SmallVec<[Value; 8]> = (3..args.argc).map(|i| args.get(vm, i).clone()).collect();
    let ip = vm.ip;
    let arr = vm
        .arrays
        .get_mut(arr_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
    let len = arr.len() as i64;
    let to_idx = |v: &Value| -> i64 {
        if matches!(v, Value::Undefined) {
            return 0;
        }
        v.to_number()
            .map(|n| {
                let i = n as i64;
                if i < 0 { (i + len).max(0) } else { i.min(len) }
            })
            .unwrap_or(0)
    };
    let start = to_idx(&start_val);
    let del_count = match &del_val {
        Some(v) if !matches!(v, Value::Undefined) => {
            let n = v.to_number().unwrap_or(0.0);
            (n as i64).max(0).min(len - start) as usize
        }
        _ => (len - start).max(0) as usize,
    };
    let start = start as usize;
    let removed: ThinVec<Value> = arr.drain(start..start + del_count).collect();
    let _ = arr;
    // Insert new items at start position.
    if !to_insert.is_empty() {
        let arr = vm
            .arrays
            .get_mut(arr_ptr as usize)
            .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad array pointer"))?;
        for v in to_insert.into_iter().rev() {
            arr.insert(start, v);
        }
    }
    Ok(vm.alloc_array(removed))
}

/// `arr.slice(start[, end])` → new array, subset of the original.
pub fn array_slice_builtin(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let len = arr.len() as i64;
    let to_idx = |v: &Value, default: i64| -> i64 {
        if matches!(v, Value::Undefined) {
            return default;
        }
        v.to_number()
            .map(|n| {
                let i = n as i64;
                if i < 0 { (i + len).max(0) } else { i.min(len) }
            })
            .unwrap_or(default)
    };
    let start = to_idx(args.get(vm, 1), 0).max(0) as usize;
    let end = to_idx(args.get(vm, 2), len).max(0) as usize;
    let end = end.min(arr.len());
    if start >= end {
        return Ok(vm.alloc_array(ThinVec::new()));
    }
    let subset: ThinVec<Value> = arr[start..end].iter().cloned().collect();
    Ok(vm.alloc_array(subset))
}

pub fn array_includes(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let needle = args.get(vm, 1);
    let start = args.get(vm, 2).to_number().unwrap_or(0.0) as usize;
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    for v in &arr[start.min(arr.len())..] {
        if v == needle {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

pub fn array_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let needle = args.get(vm, 1);
    let start = args.get(vm, 2).to_number().unwrap_or(0.0) as i64;
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let from = start.max(0) as usize;
    for (i, v) in arr.iter().enumerate().skip(from) {
        if v == needle {
            return Ok(Value::int_from_f64(i as f64));
        }
    }
    Ok(Value::NegInt(-1))
}

pub fn array_last_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let needle = args.get(vm, 1);
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    // Default start is last index + needle length (JS behavior)
    let start = match args.get(vm, 2) {
        Value::Undefined => arr.len() as i64,
        v => v.to_number().unwrap_or(arr.len() as f64) as i64,
    };
    let end = (start + 1).min(arr.len() as i64).max(0) as usize;
    for (i, v) in arr.iter().enumerate().take(end).rev() {
        if v == needle {
            return Ok(Value::int_from_f64(i as f64));
        }
    }
    Ok(Value::NegInt(-1))
}

pub fn array_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let idx = args.get(vm, 1).to_number().unwrap_or(0.0);
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let len = arr.len() as i64;
    let i = if idx < 0.0 {
        idx as i64 + len
    } else {
        idx as i64
    };
    if i < 0 || i as usize >= arr.len() {
        return Ok(Value::Undefined);
    }
    Ok(arr[i as usize].clone())
}

pub fn array_concat(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let mut result: ThinVec<Value> = arr.clone();
    for i in 1..args.argc {
        match args.get(vm, i) {
            Value::Array(p) => {
                let other = vm
                    .arrays
                    .get(*p as usize)
                    .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
                result.extend(other.iter().cloned());
            }
            v => result.push(v.clone()),
        }
    }
    Ok(vm.alloc_array(result))
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

    // ── ArrayPush ──────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_push_returns_length() {
        let out = run_instrs(vec![
            Instr::PushFloat(10.0),
            Instr::ArrNew(1),
            Instr::Pick(0),
            Instr::PushFloat(20.0),
            Instr::CallBuiltin(Builtin::ArrayPush, 2),
        ]);
        assert_eq!(out.last(), Some(&Value::PosInt(2)));
    }

    // ── ArrayPop ───────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_pop_returns_last() {
        let out = run_instrs(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(2.0),
            Instr::ArrNew(2),
            Instr::CallBuiltin(Builtin::ArrayPop, 1),
        ]);
        assert_eq!(out, vec![Value::Float(2.0)]);
    }

    #[test]
    fn call_builtin_array_pop_empty_returns_undefined() {
        let out = run_instrs(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayPop, 1),
        ]);
        assert_eq!(out, vec![Value::Undefined]);
    }

    // ── ArrayShift ─────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_shift_returns_first() {
        let out = run_instrs(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(2.0),
            Instr::ArrNew(2),
            Instr::CallBuiltin(Builtin::ArrayShift, 1),
        ]);
        assert_eq!(out, vec![Value::Float(1.0)]);
    }

    #[test]
    fn call_builtin_array_shift_empty_returns_undefined() {
        let out = run_instrs(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayShift, 1),
        ]);
        assert_eq!(out, vec![Value::Undefined]);
    }

    // ── ArrayUnshift ───────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_unshift_returns_length() {
        let out = run_instrs(vec![
            Instr::PushFloat(2.0),
            Instr::ArrNew(1),
            Instr::Pick(0),
            Instr::PushFloat(1.0),
            Instr::CallBuiltin(Builtin::ArrayUnshift, 2),
        ]);
        assert_eq!(out.last(), Some(&Value::PosInt(2)));
    }

    // ── ArrayJoin ──────────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_join_default_sep() {
        let out = run_instrs(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(2.0),
            Instr::ArrNew(2),
            Instr::CallBuiltin(Builtin::ArrayJoin, 1),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "1,2"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn call_builtin_array_join_custom_sep() {
        let out = run_instrs(vec![
            Instr::PushFloat(1.0),
            Instr::PushFloat(2.0),
            Instr::ArrNew(2),
            Instr::PushStr(" - ".into()),
            Instr::CallBuiltin(Builtin::ArrayJoin, 2),
        ]);
        match &out[0] {
            Value::String(s) => assert_eq!(s.as_str(), "1 - 2"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    // ── Array.isArray ──────────────────────────────────────────────────

    #[test]
    fn call_builtin_array_is_array() {
        let out = run_instrs(vec![
            Instr::ArrNew(0),
            Instr::CallBuiltin(Builtin::ArrayIsArray, 1),
        ]);
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn call_builtin_array_is_array_false() {
        let out = run_instrs(vec![
            Instr::PushFloat(1.0),
            Instr::CallBuiltin(Builtin::ArrayIsArray, 1),
        ]);
        assert_eq!(out, vec![Value::Bool(false)]);
    }

    // ── Step 3: JS contract fixes ─────────────────────────────────────

    #[test]
    fn js_pop_shift_empty_returns_undefined() {
        // JS: [].pop() === undefined, [].shift() === undefined
        assert_eq!(
            testutil::run_ret("return [[].pop(), [].shift()];"),
            serde_json::json!([null, null])
        );
    }

    // ── Step 4c: array method tests ───────────────────────────────────

    #[test]
    fn array_reverse() {
        assert_eq!(
            testutil::run_ret("return [1,2,3].reverse();"),
            serde_json::json!([3, 2, 1])
        );
    }

    #[test]
    fn array_flat() {
        // Default depth 1.
        assert_eq!(
            testutil::run_ret("return [1,[2,[3]]].flat();"),
            serde_json::json!([1, 2, [3]])
        );
        // Depth 2.
        assert_eq!(
            testutil::run_ret("return [1,[2,[3]]].flat(2);"),
            serde_json::json!([1, 2, 3])
        );
    }

    #[test]
    fn array_fill() {
        assert_eq!(
            testutil::run_ret("return [1,2,3].fill(0, 1);"),
            serde_json::json!([1, 0, 0])
        );
    }

    #[test]
    fn array_splice() {
        // splice(1, 2, 9) — delete 2 at index 1, insert 9.
        assert_eq!(
            testutil::run_ret("const a=[1,2,3,4]; const r=a.splice(1,2,9); return [a,r];",),
            serde_json::json!([[1, 9, 4], [2, 3]])
        );
    }

    #[test]
    fn array_polymorphic_methods() {
        // slice
        assert_eq!(
            testutil::run_ret("return [1,2,3].slice(1);"),
            serde_json::json!([2, 3])
        );
        // indexOf
        assert_eq!(
            testutil::run_ret("return [1,2,3].indexOf(2);"),
            serde_json::json!(1)
        );
        // lastIndexOf
        assert_eq!(
            testutil::run_ret("return [1,2,1].lastIndexOf(1);"),
            serde_json::json!(2)
        );
        // includes
        assert_eq!(
            testutil::run_ret("return [1,2,3].includes(3);"),
            serde_json::json!(true)
        );
        // at
        assert_eq!(
            testutil::run_ret("return [1,2,3].at(-1);"),
            serde_json::json!(3)
        );
        // concat
        assert_eq!(
            testutil::run_ret("return [1,2].concat(3, [4]);"),
            serde_json::json!([1, 2, 3, 4])
        );
    }

    #[test]
    fn array_from_existing_array() {
        assert_eq!(
            testutil::run_ret("return Array.from([1, 2, 3]);"),
            serde_json::json!([1, 2, 3])
        );
    }

    #[test]
    fn array_from_array_like() {
        assert_eq!(
            testutil::run_ret("return Array.from({length: 3, '0': 'a', '1': 'b', '2': 'c'});"),
            serde_json::json!(["a", "b", "c"])
        );
    }

    #[test]
    fn array_from_empty_length() {
        assert_eq!(
            testutil::run_ret("return Array.from({length: 3});"),
            serde_json::json!([null, null, null])
        );
    }

    #[test]
    fn array_from_empty() {
        assert_eq!(
            testutil::run_ret("return Array.from([]);"),
            serde_json::json!([])
        );
    }
}
