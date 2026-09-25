use crate::builtin::Args;
use crate::vm::instr::TypeTag;
use crate::vm::{ErrorKind, IntegrityLevel, JsString, ObjData, VM, VMError, Value};
use indexmap::IndexMap;
use thin_vec::ThinVec;

// ── object static implementations ────────────────────────────────────────────

/// `Object(x)` / `new Object(x)` — the constructor. Returns `x` if it is
/// already an object (Object/Array/Map/Set/RegExp/Closure/Builtin/Bound/
/// Promise), else coerces to a plain object. With no argument (or
/// `undefined`/`null`), returns `{}`. Boxed primitives (`new Number(5)`) are
/// deferred (Step 2b gives method compat without boxing); `Object(5)` in JS
/// boxes too, but here it returns `{}` since we have no wrapper — a known
/// divergence pinned in the ledger.
pub fn object_ctor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let arg = args.get(vm, 0);
    match arg {
        Value::Object(_)
        | Value::Array(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::RegExp(_)
        | Value::Closure { .. }
        | Value::Builtin(_)
        | Value::Bound(_)
        | Value::Promise(_) => Ok(arg.clone()),
        Value::Undefined | Value::Null => Ok(vm.alloc_object(IndexMap::new())),
        // Primitives: JS boxes (`Object(5)` → `new Number(5)`); we return
        // `{}` (no wrapper type — Step 2b keeps method compat without boxing).
        _ => Ok(vm.alloc_object(IndexMap::new())),
    }
}

/// `Error(msg, options)` / `new Error(msg, options)` and one of these per
/// error class — **the** constructor body for every entry point. `new
/// TypeError(…)`, a bare `TypeError(…)`, and a call through a value
/// (`const E = TypeError; E("x")`) all land here; there is no compiler
/// shortcut past it any more.
///
/// The `tag` is the class, and [`VM::alloc_error`] recovers it from the name
/// — so a constructor and a VM raise of the same kind produce the same
/// prototype, and there is no second place for the two to disagree.
///
/// Each class needs a registry row of its own, and a row needs its own
/// handler: that is the whole reason for the per-name wrappers below rather
/// than one function taking the name. Without them the global `TypeError`
/// has no constructor to resolve to, and `e instanceof TypeError` is a
/// `TypeError` about the right-hand side rather than an answer.
///
/// **The message coerces here, not in the compiler.** `new Error(123)` is
/// `"123"` because of the `to_js_string` below; that used to be a `ToStr` the
/// compiler emitted ahead of a dedicated instruction, and moving it here is
/// what let the instruction go.
fn error_construct(vm: &mut VM, args: Args, tag: TypeTag) -> Result<Value, VMError> {
    let message = message_arg(vm, &args, 0);
    let cause = options_cause(vm, &args, 1);
    let err = vm.alloc_error(JsString::from(tag.name()), message);
    set_own(vm, &err, "cause", cause);
    Ok(err)
}

/// The message argument at `i`, ToString-coerced.
///
/// Absent is `""`, not `"undefined"`: `new Error()` in JS has an empty
/// message, and a program that prints `` `${e}` `` would otherwise read
/// `"Error: undefined"` and go looking for the undefined thing.
fn message_arg(vm: &mut VM, args: &Args, i: usize) -> JsString {
    match args.get(vm, i) {
        Value::Undefined => JsString::from(""),
        other => vm.to_js_string(&other.clone(), 0),
    }
}

/// Give `err` an own property, or leave it alone when `value` is `None`.
///
/// `None` and `Some(Value::Undefined)` are deliberately different: the
/// absent case must not create the key at all, or `Object.keys(e)` and
/// `JSON.stringify(e)` would report a field the program never set.
fn set_own(vm: &mut VM, err: &Value, key: &str, value: Option<Value>) {
    if let (Value::Object(p), Some(value)) = (err, value) {
        vm.objects[*p as usize]
            .map
            .insert(JsString::from(key), value);
    }
}

/// The `cause` of `new Error(msg, { cause })`, or `None` when there is no
/// options bag or it carries no `cause` key.
///
/// **Present because the argument used to be a compile error.** `new
/// Error("m", { cause: e })` is ordinary JS and the commonest reason anyone
/// passes a second argument; the compiler rejected it outright, which was at
/// least loud. Once the dedicated construction path went away there was
/// nothing left to reject it *with* — and an options bag accepted and
/// discarded would leave `e.cause` undefined with nothing said. So it is
/// honoured instead.
///
/// Absence is `None` rather than `undefined`: `new Error("m")` must not grow
/// a `cause` key, or `Object.keys(e)` and `JSON.stringify(e)` would report a
/// field the program never set.
fn options_cause(vm: &mut VM, args: &Args, i: usize) -> Option<Value> {
    let Value::Object(p) = args.get(vm, i) else {
        return None;
    };
    let p = *p;
    vm.objects
        .get(p as usize)?
        .map
        .get(&JsString::from("cause"))
        .cloned()
}

macro_rules! error_ctor {
    ($fn_name:ident, $tag:ident) => {
        pub fn $fn_name(vm: &mut VM, args: Args) -> Result<Value, VMError> {
            error_construct(vm, args, TypeTag::$tag)
        }
    };
}

error_ctor!(error_ctor, Error);
error_ctor!(type_error_ctor, TypeError);
error_ctor!(value_error_ctor, ValueError);
error_ctor!(range_error_ctor, RangeError);
error_ctor!(syntax_error_ctor, SyntaxError);
error_ctor!(reference_error_ctor, ReferenceError);
error_ctor!(eval_error_ctor, EvalError);
error_ctor!(uri_error_ctor, URIError);

/// `new AggregateError(errors, message, options)` — the one error class
/// whose first argument is not the message.
///
/// **Nothing in this runtime throws it.** `Promise.any`, its only producer in
/// JS, is a compile error here. It exists so a program's own
/// `throw new AggregateError(failures, "all attempts failed")` works, which
/// is why the `errors` argument is built for real rather than ignored: the
/// code that catches an `AggregateError` is precisely the code that reads
/// `e.errors`, so a class that accepted the iterable and dropped it would
/// fail exactly where it is used.
///
/// `errors` becomes a plain `Array` own property — materialized eagerly
/// through the same `iterable_elements` that `new Set(x)`/`new Map(x)` use,
/// so an argument those two accept works here too. A value none of them can
/// iterate is a `TypeError` naming what it got, not an empty `.errors`.
pub fn aggregate_error_ctor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let errors = args.get(vm, 0).clone();
    let Some(elements) = vm.iterable_elements(&errors)? else {
        let what = vm.describe_operand(&errors);
        return Err(vm.fail(
            ErrorKind::TypeError,
            format!("`AggregateError` needs an iterable of errors; got {what}").as_str(),
        ));
    };
    let message = message_arg(vm, &args, 1);
    let cause = options_cause(vm, &args, 2);
    let errors = vm.alloc_array(elements.into());
    let err = vm.alloc_error(JsString::from(TypeTag::AggregateError.name()), message);
    set_own(vm, &err, "errors", Some(errors));
    set_own(vm, &err, "cause", cause);
    Ok(err)
}

/// `new SuppressedError(error, suppressed, message, options)` — the error
/// raised when disposing a resource throws while another error is already
/// propagating, so the two must travel together.
///
/// **Nothing in this runtime throws it**: `using` and `DisposableStack`, its
/// only producers in JS, do not exist here. Declared so a program can throw
/// and catch its own, and shaped correctly because the whole point of the
/// class is the pair it carries — `.error` (the one that won) and
/// `.suppressed` (the one it displaced). Both are set unconditionally, even
/// when `undefined`, because a `SuppressedError` missing either half is not
/// one.
pub fn suppressed_error_ctor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let error = args.get(vm, 0).clone();
    let suppressed = args.get(vm, 1).clone();
    let message = message_arg(vm, &args, 2);
    let cause = options_cause(vm, &args, 3);
    let err = vm.alloc_error(JsString::from(TypeTag::SuppressedError.name()), message);
    set_own(vm, &err, "error", Some(error));
    set_own(vm, &err, "suppressed", Some(suppressed));
    set_own(vm, &err, "cause", cause);
    Ok(err)
}

/// `Object.keys(obj)` → array of own enumerable string keys. Step 2e: reads
/// the unified own-prop snapshot, so it also enumerates a **function's** user
/// **What it was handed, because it is nearly always `undefined`.**
/// `Object.entries(r.items)` on a result with no `items` used to say
/// `in \`entries\`: type error`, which names neither the value nor the
/// expression — the same silence `Array.from` was fixed of, and the
/// same cause: a field that is not there.
fn object_needs_an_object(vm: &mut VM, verb: &str, got: &Value) -> VMError {
    let what = vm.describe_operand(got);
    vm.fail(
        ErrorKind::TypeError,
        format!("Object.{verb} needs an object; got {what}").as_str(),
    )
}

/// props (excluding the virtual `name`/`length`/`prototype` rungs), not just
/// plain objects.
pub fn obj_keys(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let recv = args.get(vm, 0).clone();
    let props = match vm.own_enumerable_props(&recv) {
        Some(p) => p,
        None => return Err(object_needs_an_object(vm, "keys", &recv)),
    };
    let arr: ThinVec<Value> = props.into_iter().map(|(k, _)| Value::String(k)).collect();
    Ok(vm.alloc_array(arr))
}

/// `Object.values(obj)` → array of own enumerable values (Object map or a
/// function's `props` bag — see [`obj_keys`]).
pub fn obj_values(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let recv = args.get(vm, 0).clone();
    let props = match vm.own_enumerable_props(&recv) {
        Some(p) => p,
        None => return Err(object_needs_an_object(vm, "values", &recv)),
    };
    let vals: ThinVec<Value> = props.into_iter().map(|(_, v)| v).collect();
    Ok(vm.alloc_array(vals))
}

/// `Object.entries(obj)` → array of [key, value] pairs (Object map or a
/// function's `props` bag — see [`obj_keys`]).
pub fn obj_entries(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let recv = args.get(vm, 0).clone();
    let pairs = match vm.own_enumerable_props(&recv) {
        Some(p) => p,
        None => return Err(object_needs_an_object(vm, "entries", &recv)),
    };
    let mut result: ThinVec<Value> = ThinVec::with_capacity(pairs.len());
    for (k, v) in pairs {
        let pair: ThinVec<Value> = vec![Value::String(k), v].into();
        result.push(vm.alloc_array(pair));
    }
    Ok(vm.alloc_array(result))
}

/// `Object.fromEntries(entries)` → object from [key, value] pairs.
pub fn obj_from_entries(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    // **A `Map` is a list of entries, which is the whole point of it.**
    // `Object.fromEntries(m)` is the standard way to turn one back into
    // an object and this refused it. Found by the message written an
    // hour earlier: `Object.fromEntries needs an object; got a map` is
    // what a `sweep-40` run was told on 2026-09-20, and naming the
    // value is what made the gap visible — the old "type error" would
    // have hidden it again.
    let entries_value = match args.get(vm, 0) {
        Value::Map(_) => super::map_entries(vm, args)?,
        other => other.clone(),
    };
    let arr_ptr = match &entries_value {
        Value::Array(p) => *p,
        other => {
            let other = other.clone();
            return Err(object_needs_an_object(vm, "fromEntries", &other));
        }
    };
    let entries = vm
        .arrays
        .get(arr_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
    let mut map = IndexMap::<JsString, Value>::new();
    for entry in entries {
        let pair_ptr = match entry {
            Value::Array(p) => p,
            // **Which entry, and what it was.** `Object.fromEntries`
            // takes `[key, value]` pairs, and the commonest way to get
            // this wrong is to hand it a flat list — `[a, b]` rather
            // than `[[a, b]]`. "type error" leaves the reader to work
            // out that the fault is one element deep.
            other => {
                let other = other.clone();
                let what = vm.describe_operand(&other);
                return Err(vm.fail(
                    ErrorKind::TypeError,
                    format!("Object.fromEntries needs [key, value] pairs; one entry is {what}")
                        .as_str(),
                ));
            }
        };
        let pair = vm
            .arrays
            .get(*pair_ptr as usize)
            .ok_or_else(|| vm.fail(ErrorKind::TypeError, "type error"))?;
        if pair.len() < 2 {
            continue;
        }
        let key = vm.to_js_string(&pair[0], 0);
        map.insert(key, pair[1].clone());
    }
    // Step 2b: use `alloc_object` so the result chains to `Object.prototype`
    // (matching JS — `Object.fromEntries([])` is a plain object).
    Ok(vm.alloc_object(map))
}

/// `Object.assign(target, ...sources)` → copies properties from sources to target,
/// returns the target.
pub fn obj_assign(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let target_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        other => {
            let other = other.clone();
            return Err(object_needs_an_object(vm, "assign", &other));
        }
    };
    // Copy from all source objects (args 1..) into target.
    let ip = vm.ip;
    // Collect all source entries before mutating target (avoid borrow conflict).
    let mut entries: Vec<(JsString, Value)> = Vec::new();
    for i in 1..args.argc {
        let source_ptr = match args.get(vm, i) {
            Value::Object(p) => *p,
            _ => continue, // Non-object sources are silently skipped in JS.
        };
        let source = vm
            .objects
            .get(source_ptr as usize)
            .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer"))?;
        entries.extend(source.map.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    let target = vm
        .objects
        .get_mut(target_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer"))?;
    for (k, v) in entries {
        target.map.insert(k, v);
    }
    Ok(Value::Object(target_ptr))
}

/// `Object.hasOwn(obj, key)` → bool. Returns whether `obj` has its own
/// property `key`. Since there is no prototype chain in this dialect,
/// this is equivalent to `key in obj`.
pub fn obj_has_own(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let recv = args.get(vm, 0).clone();
    let key = vm.to_js_string(args.get(vm, 1), 0);
    match vm.own_prop_contains(&recv, &key) {
        Some(has) => Ok(Value::Bool(has)),
        None => Err(object_needs_an_object(vm, "hasOwn", &recv)),
    }
}

/// `obj.hasOwnProperty(key)` → bool. Instance version of `Object.hasOwn`.
///
/// Step 2c: shadowing is now uniform — an object's own `hasOwnProperty`
/// property shadows the builtin (matching JS). The former deliberate
/// divergence (non-shadowing) is retired; the unified `CallBuiltin`
/// Object-receiver path consults `resolve_method_for_object_receiver` for
/// every method name, not a whitelist.
pub fn obj_has_own_property(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let recv = args.get(vm, 0).clone();
    let key = vm.to_js_string(args.get(vm, 1), 0);
    match vm.own_prop_contains(&recv, &key) {
        Some(has) => Ok(Value::Bool(has)),
        None => Err(object_needs_an_object(vm, "hasOwn", &recv)),
    }
}

/// `Object.create(proto [, properties])` — creates a new object with
/// `proto` as its `[[Prototype]]` (Step 2b). `proto` must be an `Object` or
/// `null`. The optional second argument (property descriptors) is not
/// supported (descriptor tier is Step 4) — passing it is a `TypeError` so
/// the divergence is loud, not silent. The new object is extensible with
/// an empty own-property map, exactly like JS.
pub fn obj_create(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let proto = args.get(vm, 0);
    let proto_ptr = match proto {
        Value::Object(p) => Some(*p),
        Value::Null => None,
        _ => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "Object.create: prototype must be an object or null",
            ));
        }
    };
    // The optional second arg (property descriptors) is not supported.
    if !matches!(args.get(vm, 1), Value::Undefined) {
        return Err(vm.fail(
            ErrorKind::TypeError,
            "Object.create: property descriptors are not supported (Step 4 tier)",
        ));
    }
    let ptr = vm.objects.len() as u32;
    vm.objects.push(ObjData {
        proto: proto_ptr,
        map: IndexMap::new(),
        ..Default::default()
    });
    Ok(Value::Object(ptr))
}

/// `Object.getPrototypeOf(x)` → the `[[Prototype]]` of `x` as a value, or
/// `null` if `x` has no prototype (`Object.create(null)`). Step 2b: accepts
/// any value — primitives return their wrapper type's prototype
/// (`Object.getPrototypeOf(5) === Number.prototype`), structural types
/// return their type prototype (`Object.getPrototypeOf([]) ===
/// Array.prototype`), constructors return `Function.prototype`
/// (`Object.getPrototypeOf(Array) === Function.prototype`). `null`/
/// `undefined` are a `TypeError` (no wrapper coercion — JS throws too).
pub fn obj_get_proto_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let val = args.get(vm, 0).clone();
    match vm.value_proto(&val)? {
        Some(p) => Ok(Value::Object(p)),
        None => match &val {
            Value::Null | Value::Undefined => Err(vm.fail(
                ErrorKind::TypeError,
                "Object.getPrototypeOf: cannot convert primitive to object (null/undefined)",
            )),
            // Upval is an internal marker that should never reach here.
            _ => Ok(Value::Null),
        },
    }
}

/// `Object.setPrototypeOf(obj, proto)` → sets `obj`'s prototype and returns
/// `obj`. `proto` must be an `Object` or `null`. A cyclic set (where `proto`'s
/// own chain already reaches `obj`) is rejected with a `TypeError`, matching JS.
pub fn obj_set_proto_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        other => {
            let other = other.clone();
            return Err(object_needs_an_object(vm, "setPrototypeOf", &other));
        }
    };
    let proto = args.get(vm, 1).clone();
    vm.set_object_proto(obj_ptr, proto)?;
    Ok(Value::Object(obj_ptr))
}

// ── Step 2d: Object.freeze / seal / preventExtensions ───────────────────────

/// `Object.freeze(obj)` — freezes the object (no add, delete, or modify) and
/// returns it. Shallow — only the object itself, not nested children.
/// MVP: receiver must be `Value::Object`; non-Object (array/map/set) is
/// deferred (documented divergence).
pub fn obj_freeze(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "Object.freeze: receiver must be an Object (arrays/maps/sets deferred)",
            ));
        }
    };
    let ip = vm.ip;
    let obj = vm
        .objects
        .get_mut(obj_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer"))?;
    obj.integrity = IntegrityLevel::Frozen;
    Ok(Value::Object(obj_ptr))
}

/// `Object.isFrozen(obj)` → bool. Returns whether the object is frozen.
/// MVP: non-Object receivers return `false` (deferred).
pub fn obj_is_frozen(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Object(p) => {
            let obj = vm
                .objects
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad object pointer"))?;
            Ok(Value::Bool(obj.integrity == IntegrityLevel::Frozen))
        }
        _ => Ok(Value::Bool(false)),
    }
}

/// `Object.seal(obj)` — seals the object (no add, no delete; modify allowed)
/// and returns it. Shallow.
/// MVP: receiver must be `Value::Object`; non-Object is deferred.
pub fn obj_seal(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "Object.seal: receiver must be an Object (arrays/maps/sets deferred)",
            ));
        }
    };
    let ip = vm.ip;
    let obj = vm
        .objects
        .get_mut(obj_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer"))?;
    // seal is a strict upgrade: from Extensible or NonExtensible → Sealed.
    // Never downgrade (a Frozen object stays Frozen).
    if matches!(
        obj.integrity,
        IntegrityLevel::Extensible | IntegrityLevel::NonExtensible
    ) {
        obj.integrity = IntegrityLevel::Sealed;
    }
    Ok(Value::Object(obj_ptr))
}

/// `Object.isSealed(obj)` → bool. Returns whether the object is sealed.
/// An object is sealed if it is at least Sealed (or Frozen).
/// MVP: non-Object receivers return `false`.
pub fn obj_is_sealed(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Object(p) => {
            let obj = vm
                .objects
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad object pointer"))?;
            Ok(Value::Bool(matches!(
                obj.integrity,
                IntegrityLevel::Sealed | IntegrityLevel::Frozen
            )))
        }
        _ => Ok(Value::Bool(false)),
    }
}

/// `Object.preventExtensions(obj)` — prevents new properties from being
/// added (modify + delete still allowed) and returns the object.
/// If already NonExtensible/Sealed/Frozen, a no-op (but the level doesn't
/// downgrade). Shallow.
/// MVP: receiver must be `Value::Object`; non-Object is deferred.
pub fn obj_prevent_extensions(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let obj_ptr = match args.get(vm, 0) {
        Value::Object(p) => *p,
        _ => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                "Object.preventExtensions: receiver must be an Object (arrays/maps/sets deferred)",
            ));
        }
    };
    let ip = vm.ip;
    let obj = vm
        .objects
        .get_mut(obj_ptr as usize)
        .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad object pointer"))?;
    // Only upgrade; never downgrade (e.g. a sealed object stays sealed).
    if obj.integrity == IntegrityLevel::Extensible {
        obj.integrity = IntegrityLevel::NonExtensible;
    }
    Ok(Value::Object(obj_ptr))
}

/// `Object.isExtensible(obj)` → bool. Returns whether the object can have
/// new properties added. Extensible only — NonExtensible/Sealed/Frozen all
/// return false.
/// MVP: non-Object receivers return `false`.
pub fn obj_is_extensible(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    match args.get(vm, 0) {
        Value::Object(p) => {
            let obj = vm
                .objects
                .get(*p as usize)
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad object pointer"))?;
            Ok(Value::Bool(obj.integrity == IntegrityLevel::Extensible))
        }
        _ => Ok(Value::Bool(false)),
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod named_value_tests {
    use crate::testutil;

    fn msg(src: &str) -> String {
        testutil::run_ret(&format!(
            "try {{ {src} }} catch (e) {{ return e.message; }}"
        ))
        .as_str()
        .unwrap_or_default()
        .to_owned()
    }

    /// **Every reachable `Object.*` refusal names the value.** Four of
    /// them said only "type error", which is the message this codebase
    /// has twice found a real bug behind: `Object.fromEntries needs an
    /// object; got a map` is what made the `Map` gap visible, an hour
    /// after that message was written.
    #[test]
    fn an_object_builtin_says_what_it_got() {
        let m = msg("Object.assign(undefined, {});");
        assert!(m.contains("Object.assign needs an object"), "got: {m}");
        assert!(m.contains("undefined"), "and names it: {m}");

        let m = msg("Object.hasOwn(undefined, \"x\");");
        assert!(m.contains("Object.hasOwn needs an object"), "got: {m}");

        // A flat list where pairs were wanted — the commonest way to
        // get `fromEntries` wrong, and the fault is one element deep.
        let m = msg("Object.fromEntries([1, 2]);");
        assert!(
            m.contains("[key, value] pairs") && m.contains("one entry is"),
            "got: {m}"
        );
    }
}

#[cfg(test)]
mod tests {
    /// **A `Map` is a list of entries.** `Object.fromEntries(m)` is the
    /// standard way to turn one back into an object, and this refused
    /// it — found because the message written an hour earlier named the
    /// value: `Object.fromEntries needs an object; got a map`, on a
    /// live `sweep-40` run. The old "type error" would have hidden it
    /// again.
    #[test]
    fn from_entries_takes_a_map() {
        assert_eq!(
            crate::testutil::run_ret(
                "const m = new Map([['a', 1], ['b', 2]]); return Object.fromEntries(m);"
            ),
            serde_json::json!({"a": 1, "b": 2})
        );
        // And the round trip both ways.
        assert_eq!(
            crate::testutil::run_ret("return Object.fromEntries(new Map(Object.entries({x: 7})));"),
            serde_json::json!({"x": 7})
        );
    }

    /// **What it was handed, because it is nearly always `undefined`.**
    /// `Object.entries(r.items)` on a result with no `items` said
    /// `in `entries`: type error`, naming neither the value nor the
    /// wanted shape — the same silence `Array.from` was fixed of, and
    /// the same cause: a field that is not there.
    #[test]
    fn object_verbs_name_what_they_were_handed() {
        for (src, want) in [
            ("Object.keys(null)", "got null"),
            ("Object.values(undefined)", "got undefined"),
            ("Object.entries(3)", "got a number (3)"),
            ("Object.fromEntries(5)", "got a number (5)"),
        ] {
            let err = crate::testutil::run_runtime_err(&format!("{src};"));
            assert_eq!(err.kind, crate::ErrorKind::TypeError, "{src}");
            assert!(
                err.message.contains("needs an object"),
                "{src}: {}",
                err.message
            );
            assert!(err.message.contains(want), "{src}: {}", err.message);
        }
    }

    use crate::{
        Value,
        builtin::Builtin,
        testutil::{self, run_instrs, run_runtime_err},
        vm::{ErrorKind, Instr},
    };

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

    // ── Step 4h: Object.entries / Object.fromEntries / Object.assign ──

    #[test]
    fn obj_entries_end_to_end() {
        // entries returns array of [key, value] pairs; iterate manually.
        assert_eq!(
            testutil::run_ret(
                "const e = Object.entries({a:1,b:2}); return [e[0][0], e[0][1], e[1][0], e[1][1]];"
            ),
            serde_json::json!(["a", 1, "b", 2])
        );
    }

    #[test]
    fn obj_from_entries() {
        assert_eq!(
            testutil::run_ret("return Object.fromEntries([['a', 1]]);"),
            serde_json::json!({"a": 1})
        );
    }

    #[test]
    fn obj_from_entries_round_trip() {
        assert_eq!(
            testutil::run_ret("const o = {a:1,b:2}; return Object.fromEntries(Object.entries(o));"),
            serde_json::json!({"a": 1, "b": 2})
        );
    }

    #[test]
    fn obj_assign_returns_target() {
        assert_eq!(
            testutil::run_ret("const t = {}; const r = Object.assign(t, {a:1}); return r === t;"),
            serde_json::json!(true)
        );
    }

    #[test]
    fn obj_assign_later_sources_win() {
        assert_eq!(
            testutil::run_ret("return Object.assign({a:1}, {a:2, b:3});"),
            serde_json::json!({"a": 2, "b": 3})
        );
    }

    #[test]
    fn obj_has_own() {
        assert_eq!(
            testutil::run_ret("return Object.hasOwn({a: 1}, 'a');"),
            serde_json::json!(true)
        );
        assert_eq!(
            testutil::run_ret("return Object.hasOwn({a: 1}, 'b');"),
            serde_json::json!(false)
        );
        assert_eq!(
            testutil::run_ret("return Object.hasOwn({}, 'toString');"),
            serde_json::json!(false)
        );
    }

    #[test]
    fn obj_has_own_property() {
        assert_eq!(
            testutil::run_ret("return ({a: 1}).hasOwnProperty('a');"),
            serde_json::json!(true)
        );
        assert_eq!(
            testutil::run_ret("return ({a: 1}).hasOwnProperty('b');"),
            serde_json::json!(false)
        );
    }

    // ── Step 2d: Object.freeze / seal / preventExtensions ──────────────

    #[test]
    fn freeze_blocks_writes() {
        let err = run_runtime_err("const o = {a:1}; Object.freeze(o); o.a = 2; return o;");
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn freeze_returns_object() {
        assert_eq!(
            testutil::run_ret("const o = {}; return Object.freeze(o) === o;"),
            serde_json::json!(true)
        );
    }

    #[test]
    fn is_frozen_after_freeze() {
        assert_eq!(
            testutil::run_ret(
                "const o = {}; return [Object.isFrozen(o), Object.freeze(o), Object.isFrozen(o)];"
            ),
            serde_json::json!([false, {}, true])
        );
    }

    #[test]
    fn freeze_blocks_delete() {
        let err = run_runtime_err("const o = {a:1}; Object.freeze(o); delete o.a;");
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn freeze_blocks_new_keys() {
        let err = run_runtime_err("const o = {}; Object.freeze(o); o.x = 1;");
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn seal_blocks_add_and_delete_but_allows_modify() {
        assert_eq!(
            testutil::run_ret("const o = {a:1}; Object.seal(o); o.a = 2; return o.a;"),
            serde_json::json!(2)
        );
        let err = run_runtime_err("const o = {a:1}; Object.seal(o); o.x = 1;");
        assert_eq!(err.kind, ErrorKind::TypeError);
        let err = run_runtime_err("const o = {a:1}; Object.seal(o); delete o.a;");
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn is_sealed_after_seal() {
        assert_eq!(
            testutil::run_ret(
                "const o = {}; return [Object.isSealed(o), Object.seal(o), Object.isSealed(o)];"
            ),
            serde_json::json!([false, {}, true])
        );
    }

    #[test]
    fn prevent_extensions_blocks_new_keys_but_allows_modify_and_delete() {
        assert_eq!(
            testutil::run_ret("const o = {a:1}; Object.preventExtensions(o); o.a = 2; return o.a;"),
            serde_json::json!(2)
        );
        let err = run_runtime_err("const o = {a:1}; Object.preventExtensions(o); o.x = 1;");
        assert_eq!(err.kind, ErrorKind::TypeError);
        assert_eq!(
            testutil::run_ret(
                "const o = {a:1}; Object.preventExtensions(o); delete o.a; return o.a === undefined;"
            ),
            serde_json::json!(true)
        );
    }

    #[test]
    fn is_extensible_tracks_prevent_extensions() {
        assert_eq!(
            testutil::run_ret(
                "const o = {}; const before = Object.isExtensible(o); Object.preventExtensions(o); return [before, Object.isExtensible(o)];"
            ),
            serde_json::json!([true, false])
        );
    }

    #[test]
    fn freeze_is_shallow() {
        // freeze is shallow: a nested object stays mutable.
        assert_eq!(
            testutil::run_ret("const o = {a: {b: 1}}; Object.freeze(o); o.a.b = 2; return o.a.b;"),
            serde_json::json!(2)
        );
    }

    #[test]
    fn frozen_object_serializes_as_plain_data() {
        // A frozen ordinary object still serializes to JSON — the level is
        // dropped, per the invariant boundary. (Builtin prototypes are
        // rejected by `kind`, not by `integrity`.)
        assert_eq!(
            testutil::run_ret("const o = {a:1}; Object.freeze(o); return o;"),
            serde_json::json!({ "a": 1 })
        );
    }

    #[test]
    fn freeze_seal_prevent_extensions_progression() {
        // Each step is strictly stronger; `seal` subsumes `preventExtensions`.
        assert_eq!(
            testutil::run_ret(
                "const o = {}; Object.preventExtensions(o); Object.seal(o); return [Object.isExtensible(o), Object.isSealed(o), Object.isFrozen(o)];"
            ),
            serde_json::json!([false, true, false])
        );
        assert_eq!(
            testutil::run_ret(
                "const o = {}; Object.seal(o); Object.freeze(o); return Object.isFrozen(o);"
            ),
            serde_json::json!(true)
        );
    }

    #[test]
    fn seal_after_freeze_is_noop() {
        assert_eq!(
            testutil::run_ret(
                "const o = {}; Object.freeze(o); Object.seal(o); return Object.isFrozen(o);"
            ),
            serde_json::json!(true)
        );
    }

    #[test]
    fn prevent_extensions_after_seal_is_noop() {
        // preventExtensions on a sealed object does not downgrade.
        assert_eq!(
            testutil::run_ret(
                "const o = {}; Object.seal(o); Object.preventExtensions(o); return Object.isSealed(o);"
            ),
            serde_json::json!(true)
        );
    }

    #[test]
    fn freeze_on_non_object_is_deferred() {
        let err = run_runtime_err("Object.freeze([]);");
        assert_eq!(err.kind, ErrorKind::TypeError);
        let err = run_runtime_err("Object.freeze(new Map());");
        assert_eq!(err.kind, ErrorKind::TypeError);
        let err = run_runtime_err("Object.freeze(new Set());");
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn is_frozen_on_non_object_is_false() {
        assert_eq!(
            testutil::run_ret("return Object.isFrozen([]);"),
            serde_json::json!(false)
        );
        assert_eq!(
            testutil::run_ret("return Object.isFrozen(5);"),
            serde_json::json!(false)
        );
    }

    #[test]
    fn is_extensible_on_non_object_is_false() {
        assert_eq!(
            testutil::run_ret("return Object.isExtensible([]);"),
            serde_json::json!(false)
        );
    }

    #[test]
    fn freeze_on_object_prototype_is_noop() {
        // Builtin prototypes are already Frozen (Step 2a); freeze on a
        // Frozen `BuiltinPrototype` object is a no-op. The integrity field
        // is the *same* one — user freeze writes the same `ObjData.integrity`
        // that the 2a builtin-prototype freeze used.
        assert_eq!(
            testutil::run_ret("return Object.freeze(Object.prototype) === Object.prototype;"),
            serde_json::json!(true)
        );
    }
}
