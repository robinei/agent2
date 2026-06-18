use crate::builtin::Args;
use crate::builtin::array::{
    array_at, array_concat, array_includes, array_index_of, array_last_index_of,
    array_slice_builtin,
};
use crate::builtin::string::{
    str_at, str_concat, str_includes, str_index_of, str_last_index_of, str_slice,
};
use crate::vm::{ErrorKind, VM, VMError, Value};

// ── polymorphic handlers (dispatch on receiver: string vs array) ─────────────

/// `x.toString()` for any receiver — the general ToString method. Delegates to
/// the same `to_js_string` coercion used by `String(x)` and template
/// interpolation (one renderer behind both; this is why a separate
/// `RegExpToString` was removed — `to_js_string` already renders `/pat/flags`).
/// An object's own `toString` shadows the default (consistent with method
/// dispatch — re-routes via the `MethodOnObject` signal); a plain object
/// otherwise yields `"[object Object]"`. Nullish receivers never reach here:
/// `null.toString()` errors at the member access. (Radix is unsupported —
/// `(255).toString(16)` is an arity error, not a silent base-10 result.)
pub fn value_to_string(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let recv = args.get(vm, 0);
    if let Value::Object(p) = recv
        && vm
            .objects
            .get(*p as usize)
            .is_some_and(|o| o.map.contains_key("toString"))
    {
        return Err(vm.fail(ErrorKind::MethodOnObject, ""));
    }
    let s = vm.to_js_string(recv, 0);
    Ok(Value::String(s))
}

pub fn slice_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_slice(vm, args),
        Value::Array(_) => array_slice_builtin(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

pub fn includes_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_includes(vm, args),
        Value::Array(_) => array_includes(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

pub fn index_of_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_index_of(vm, args),
        Value::Array(_) => array_index_of(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

pub fn last_index_of_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_last_index_of(vm, args),
        Value::Array(_) => array_last_index_of(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

pub fn at_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_at(vm, args),
        Value::Array(_) => array_at(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}

pub fn concat_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_concat(vm, args),
        Value::Array(_) => array_concat(vm, args),
        recv => Err(vm.method_receiver_error(recv)),
    }
}
