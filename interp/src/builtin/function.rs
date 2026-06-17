use std::rc::Rc;

use thin_vec::ThinVec;

use crate::builtin::Args;
use crate::vm::{BoundFn, ErrorKind, VM, VMError, Value};

// ── Function.prototype.bind ────────────────────────────────

/// `f.bind(thisArg, ...args)` — constructs a `Value::Bound` without invoking.
/// Re-binding (`g.bind(…)` where `g` is itself a `Bound`) flattens into a fresh
/// `BoundFn` over the innermost `callable`, concatenating `bound_args` and
/// keeping the *first* `this_val` (JS: re-binding `this` is a no-op).
pub fn function_bind(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let callable = args.get(vm, 0);
    let this_val = args.get(vm, 1).clone();

    let (inner_callable, inner_this, existing_bound_args) = match callable {
        Value::Closure { .. } | Value::Builtin(_) => (callable.clone(), this_val, ThinVec::new()),
        Value::Bound(b) => {
            // Re-binding: flatten — use the original target and first this,
            // concatenate bound args (the old ones first, then the new ones).
            let existing = b.bound_args.clone();
            (b.callable.clone(), b.this_val.clone(), existing)
        }
        _ => {
            vm.stack.truncate(args.base);
            return Err(vm.fail(ErrorKind::TypeError, "receiver is not callable"));
        }
    };

    let mut all_bound = existing_bound_args;
    for i in 2..args.argc {
        all_bound.push(args.get(vm, i).clone());
    }

    let bound = BoundFn {
        this_val: inner_this,
        bound_args: all_bound,
        callable: inner_callable,
    };
    Ok(Value::Bound(Rc::new(bound)))
}

// ── Function.prototype.call ────────────────────────────────

/// `f.call(thisArg, ...args)` — invokes `f` with `this = thisArg` immediately.
/// Raises a `MethodOnObject` signal so the `CallBuiltin` dispatch lane
/// re-enters `dispatch_call`: the handler does not itself call `dispatch_call`,
/// because the builtin epilogue would corrupt the stack before the callee runs.
pub fn function_call(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let callable = args.get(vm, 0);
    match callable {
        Value::Closure { .. } | Value::Builtin(_) | Value::Bound(_) => {}
        _ => {
            vm.stack.truncate(args.base);
            return Err(vm.fail(ErrorKind::TypeError, "receiver is not callable"));
        }
    }
    // Let the CallBuiltin dispatch lane handle the re-entry.
    Err(vm.fail(ErrorKind::MethodOnObject, ""))
}

// ── Function.prototype.apply ───────────────────────────────

/// `f.apply(thisArg, argsArray)` — invokes `f` with `this = thisArg` immediately,
/// expanding `argsArray` as the argument list. Same re-entry signal as `.call`.
pub fn function_apply(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let callable = args.get(vm, 0);
    match callable {
        Value::Closure { .. } | Value::Builtin(_) | Value::Bound(_) => {}
        _ => {
            vm.stack.truncate(args.base);
            return Err(vm.fail(ErrorKind::TypeError, "receiver is not callable"));
        }
    }
    // Let the CallBuiltin dispatch lane handle the re-entry.
    Err(vm.fail(ErrorKind::MethodOnObject, ""))
}
