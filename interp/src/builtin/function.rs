use std::rc::Rc;

use thin_vec::ThinVec;

use crate::builtin::Args;
use crate::vm::{BoundFn, ErrorKind, VM, VMError, Value};

// ── Boolean constructor ─────────────────────────────────────────────────────

/// `Boolean(x)` / `new Boolean(x)` — the constructor as a plain call.
/// `Boolean()` → `false`; with one arg, ToBoolean. The `new` path would
/// box (`new Boolean(false)` → a Boolean wrapper object whose `typeof` is
/// `"object"`); here it is a documented divergence — we have no boxed
/// primitives, so `new Boolean(false)` returns the primitive `false`
/// (Step 2b keeps method compat without boxing).
pub fn boolean_ctor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    if args.argc == 0 {
        return Ok(Value::Bool(false));
    }
    let v = args.get(vm, 0).clone();
    Ok(Value::Bool(v.is_truthy()))
}

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

// `.call`/`.apply` are not builtins: the compiler lowers them directly to the
// `has_this` dispatch (`CallDyn`/`CallSpread`) — see `compile_invoke_forward`.
