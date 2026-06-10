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

pub fn slice_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_slice(vm, args),
        Value::Array(_) => array_slice_builtin(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

pub fn includes_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_includes(vm, args),
        Value::Array(_) => array_includes(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

pub fn index_of_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_index_of(vm, args),
        Value::Array(_) => array_index_of(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

pub fn last_index_of_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_last_index_of(vm, args),
        Value::Array(_) => array_last_index_of(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

pub fn at_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_at(vm, args),
        Value::Array(_) => array_at(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}

pub fn concat_poly(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::String(_) => str_concat(vm, args),
        Value::Array(_) => array_concat(vm, args),
        _ => Err(vm.fail(ErrorKind::TypeError, "type error")),
    }
}
