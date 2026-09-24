use crate::builtin::Args;
use crate::js_string::keys;
use crate::vm::{ErrorKind, JsString, VM, VMError, Value};
use smallvec::SmallVec;
use thin_vec::ThinVec;

// ── Array static implementations ─────────────────────────────────────────────

/// `Array(...items)` / `Array(length)` — the constructor as a plain call.
/// `Array(n)` with a single numeric arg creates a length-`n` array of
/// `undefined` (JS's sparse-array-of-holes, modeled as undefined slots).
/// `Array(a, b, …)` creates `[a, b, …]`. `Array()` → `[]`. The `new` path
/// (`new Array(…)`) is identical to the plain call and is handled by
/// `Instr::New`'s builtin-constructor arm, which delegates here.
pub fn array_ctor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    // `Array(n)` with a single number → length-n array of undefined.
    if args.argc == 1
        && let Some(n) = args.get(vm, 0).to_number()
        && n >= 0.0
        && n.is_finite()
        && n <= 10_000_000.0
    {
        let len = n as usize;
        let arr: ThinVec<Value> = std::iter::repeat_n(Value::Undefined, len).collect();
        return Ok(vm.alloc_array(arr));
    }
    // Negative or non-finite: JS throws RangeError. We surface a
    // ValueError with a clear message (no RangeError kind yet).
    // Non-numeric single arg: fall through to `[arg]` (JS coerces to
    // number and throws on NaN, but the common case is a numeric length;
    // a non-numeric `Array(x)` is `Array(x)` → `[x]` in practice only
    // when x is not a number — matching `Array("foo")` → `["foo"]`).
    // `Array(a, b, …)` / `Array()` → `[...args]`.
    let items: ThinVec<Value> = (0..args.argc).map(|i| args.get(vm, i).clone()).collect();
    Ok(vm.alloc_array(items))
}

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
                .get(keys::LENGTH)
                .and_then(|v| v.to_number())
                .unwrap_or(0.0);
            let n = (len as usize).min(10_000_000);
            let mut out: ThinVec<Value> = ThinVec::with_capacity(n);
            for i in 0..n {
                let key = crate::js_string::JsString::from(i.to_string());
                let v = obj.map.get(&key).cloned().unwrap_or(Value::Undefined);
                out.push(v);
            }
            Ok(vm.alloc_array(out))
        }
        // **Everything `for … of` iterates, `Array.from` converts.**
        // It used to take an array or a `{length}` object and throw
        // "type error" at everything else, so the commonest use of all
        // — `Array.from(new Set(xs))` to dedupe — failed, while the
        // spread that means the same thing, `[...new Set(xs)]`,
        // worked. Two spellings of one operation disagreeing is a
        // dialect gap a reader can only find by falling into it.
        Value::Set(_) => super::set_values(vm, args),
        Value::Map(_) => super::map_entries(vm, args),
        Value::String(s) => {
            let chars: ThinVec<Value> = crate::units::code_points(s.as_units())
                .map(|cp| Value::String(JsString::from_units(cp)))
                .collect();
            Ok(vm.alloc_array(chars))
        }
        Value::Undefined | Value::Null => Ok(vm.alloc_array(ThinVec::new())),
        other => {
            let what = vm.describe_operand(&other.clone());
            Err(vm.fail(
                ErrorKind::TypeError,
                format!(
                    "Array.from needs something to iterate — an array, a string, a Set, a \
                     Map, or an object with a `length`. Got {what}."
                )
                .as_str(),
            ))
        }
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
        Value::Undefined => JsString::from(","),
        v => vm.to_js_string(v, 0),
    };
    let arr = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let mut joined: Vec<u16> = Vec::new();
    for (i, v) in arr.iter().enumerate() {
        if i > 0 {
            joined.extend_from_slice(sep.as_units());
        }
        // JS: null/undefined elements contribute the empty string.
        if !matches!(v, Value::Null | Value::Undefined) {
            joined.extend_from_slice(vm.to_js_string(v, 0).as_units());
        }
    }
    Ok(Value::String(JsString::from_units(&joined)))
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

/// `arr.keys()` / `arr.values()` / `arr.entries()` — the index view, the
/// element view, and the pairs.
///
/// **Arrays are the reason `[...Array(n).keys()]` exists**, which is one
/// of the two idioms for "the numbers 0 to n". The other,
/// `Array.from({length: n}, (_, i) => i)`, is above. `Map` and `Set`
/// have had these three since they were added; an array not having them
/// meant the range idiom a model reached for first threw, and the one it
/// fell back to returned nulls.
///
/// Eager arrays rather than lazy iterators, like the `Map`/`Set` ones
/// beside them: this dialect has no iterator protocol, and `for … of`
/// and spread both take an array.
pub fn array_keys(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let n = vm.arrays.get(arr_ptr as usize).map_or(0, |a| a.len());
    let keys: ThinVec<Value> = (0..n).map(|i| Value::PosInt(i as u64)).collect();
    Ok(vm.alloc_array(keys))
}

pub fn array_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let copy = vm.arrays.get(arr_ptr as usize).cloned().unwrap_or_default();
    Ok(vm.alloc_array(copy))
}

pub fn array_entries(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let items = vm.arrays.get(arr_ptr as usize).cloned().unwrap_or_default();
    let mut out: ThinVec<Value> = ThinVec::with_capacity(items.len());
    for (i, v) in items.into_iter().enumerate() {
        let pair: ThinVec<Value> = vec![Value::PosInt(i as u64), v].into();
        out.push(vm.alloc_array(pair));
    }
    Ok(vm.alloc_array(out))
}

/// `arr.toReversed()` — `reverse()` on a copy, leaving the receiver
/// alone. The in-place pair are the older spelling and the trap: a
/// program that writes `const sorted = xs.reverse()` has also reversed
/// `xs`, which is rarely what it meant.
pub fn array_to_reversed(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arr_ptr = args.array_receiver(vm)?;
    let mut copy = vm.arrays.get(arr_ptr as usize).cloned().unwrap_or_default();
    copy.reverse();
    Ok(vm.alloc_array(copy))
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
            Value::String(s) => assert!(s.eq_str("1,2")),
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
            Value::String(s) => assert!(s.eq_str("1 - 2")),
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

    /// **The two spellings of one operation have to agree.**
    /// `[...new Set(xs)]` worked and `Array.from(new Set(xs))` threw
    /// "type error", so the commonest dedupe in the language failed
    /// depending on how it was written.
    #[test]
    fn array_from_converts_what_for_of_iterates() {
        assert_eq!(
            testutil::run_ret("return Array.from(new Set([1, 1, 2]));"),
            serde_json::json!([1, 2])
        );
        assert_eq!(
            testutil::run_ret("return Array.from('ab');"),
            serde_json::json!(["a", "b"])
        );
        assert_eq!(
            testutil::run_ret("return Array.from(new Map([['a', 1]]));"),
            serde_json::json!([["a", 1]])
        );
        assert_eq!(
            testutil::run_ret("return Array.from(null);"),
            serde_json::json!([])
        );
    }

    /// **The mapper used to be accepted and dropped.**
    /// `Array.from({length: n}, (_, i) => i)` is how a range is written,
    /// and it returned `[null, null, null]` — a wrong answer the program
    /// was never told about.
    #[test]
    fn array_from_applies_its_map_function() {
        assert_eq!(
            testutil::run_ret("return Array.from({length: 3}, (_, i) => i);"),
            serde_json::json!([0, 1, 2])
        );
        assert_eq!(
            testutil::run_ret("return Array.from(new Set(['a']), (s) => s + '!');"),
            serde_json::json!(["a!"])
        );
    }

    /// What it cannot convert, it says so about — `describe_operand`
    /// names the value rather than the old bare "type error".
    #[test]
    fn array_from_names_what_it_cannot_convert() {
        let err = testutil::run_runtime_err("Array.from(42);");
        assert_eq!(err.kind, crate::ErrorKind::TypeError);
        assert!(err.message.contains("Array.from needs"), "{}", err.message);
        assert!(
            err.message.contains("42"),
            "names the value: {}",
            err.message
        );
    }

    /// `[...Array(n).keys()]` is the other way to write a range, and
    /// arrays had none of the three views `Map` and `Set` have had all
    /// along.
    #[test]
    fn arrays_have_keys_values_and_entries() {
        assert_eq!(
            testutil::run_ret("return [...Array(3).keys()];"),
            serde_json::json!([0, 1, 2])
        );
        assert_eq!(
            testutil::run_ret("return ['a', 'b'].values();"),
            serde_json::json!(["a", "b"])
        );
        assert_eq!(
            testutil::run_ret("return [...['a'].entries()];"),
            serde_json::json!([[0, "a"]])
        );
    }

    /// The copying forms exist because `const s = xs.sort()` also sorts
    /// `xs`, which is rarely what it meant.
    #[test]
    fn to_sorted_and_to_reversed_leave_the_receiver_alone() {
        let out = testutil::run_ret(
            "const xs = [3, 1, 2]; const a = xs.toSorted((p, q) => p - q);              const b = xs.toReversed(); return [a, b, xs];",
        );
        assert_eq!(
            out,
            serde_json::json!([[1, 2, 3], [2, 1, 3], [3, 1, 2]]),
            "both copy; the original is untouched"
        );
        assert_eq!(
            testutil::run_ret("return ['b', 'a'].toSorted();"),
            serde_json::json!(["a", "b"])
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

    /// `const out = []; out[0] = x` is how anyone grows an array by
    /// index, and it used to be a loud error like any other
    /// out-of-bounds write. Appending at exactly `length` makes no
    /// hole, so the invariant that bound protects is untouched.
    #[test]
    fn writing_at_exactly_length_appends() {
        assert_eq!(
            testutil::run_ret("const a = []; a[0] = 7; a[1] = 8; return a;"),
            serde_json::json!([7, 8])
        );
    }

    #[test]
    fn writing_past_length_still_errors_and_says_how_to_grow() {
        let err = testutil::run_runtime_err("const a = []; a[3] = 1;");
        assert_eq!(err.kind, crate::ErrorKind::ValueError);
        assert!(err.message.contains("push()"), "{}", err.message);
    }
}
