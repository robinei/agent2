//! Builtins — the JS standard-library surface (`arr.push`, `Math.max`,
//! `Object.keys`, `JSON.parse`, …) that is invoked with *call* syntax.
//!
//! # Adding a builtin
//!
//! 1. Add one row to the `builtins!` table below (variant name, kind, display
//!    name, min/max args counting the receiver, handler function).
//! 2. Write the handler `fn(vm: &mut VM, args: Args) -> Result<Value, VMError>`.
//! 3. Add tests covering: the normal case, absent-arg (undefined-default) case,
//!    and wrong-receiver-type TypeError.
//! 4. For polymorphic (string+array) methods, dispatch on the receiver in the
//!    handler and share the row.
//!
//! **Do not add a Builtin row for:** member-read constants (e.g. `Math.PI`) —
//! fold those in the compiler (`compile_static_member`). Callback-taking array
//! methods (`map`, `filter`, …) go in the prelude (`interp/src/prelude.rs`).
//!
//! The VM has no method/prototype objects, so these are recognized
//! structurally by the compiler and lowered to a call against a `Builtin` id
//! rather than to one dedicated instruction each. This keeps the instruction
//! set to true VM primitives and gives variadic/optional-argument builtins
//! (`Math.max`, `s.slice`) for free via a uniform calling convention.
//!
//! Calling convention (shared by `Instr::CallBuiltin` and a `Builtin` value
//! called through `CallDyn`): the `argc` arguments sit on the stack
//! left-to-right (arg 0 deepest, the last on top); for a method the receiver is
//! arg 0. The builtin pops exactly its `argc` arguments and pushes exactly one
//! result — assignment-style "leave a value" semantics, so every builtin call
//! is a well-formed expression.

use crate::vm::instr::{ArrayPtr, MapPtr, SetPtr, TypeTag};
use crate::vm::{ErrorKind, RcStr, VM, VMError, Value};

mod array;
mod console;
mod edit;
mod function;
mod json;
mod map;
mod math;
mod number;
mod object;
mod poly;
mod regexp;
mod set;
mod string;
mod typedarray;

use array::*;
use console::*;
use edit::*;
use function::*;
use json::*;
use map::*;
use math::*;
use number::*;
use object::*;
use poly::*;
use regexp::*;
use set::*;
use string::*;
use typedarray::*;

// Re-export the constructor handlers so `VM::construct_builtin` can reach
// them by bare name. The `builtins!` macro references them unqualified via
// the glob imports above; this `pub(crate) use` makes them reachable as
// `crate::builtin::array_ctor` etc. too.
pub(crate) use array::array_ctor;
pub(crate) use function::boolean_ctor;
pub(crate) use function::function_ctor;
pub(crate) use map::map_ctor;
pub(crate) use number::number_ctor;
pub(crate) use number::number_to_fixed;
pub(crate) use object::obj_create;
pub(crate) use object::obj_freeze;
pub(crate) use object::obj_is_extensible;
pub(crate) use object::obj_is_frozen;
pub(crate) use object::obj_is_sealed;
pub(crate) use object::obj_prevent_extensions;
pub(crate) use object::obj_seal;
pub(crate) use object::object_ctor;
pub(crate) use regexp::regexp_ctor;
pub(crate) use set::set_ctor;
pub(crate) use string::string_ctor;
// Typed array constructors + codec helpers used by dispatch.rs.
pub(crate) use typedarray::arraybuffer_ctor;
pub(crate) use typedarray::arraybuffer_slice;
pub(crate) use typedarray::bigint64array_ctor;
pub(crate) use typedarray::biguint64array_ctor;
pub(crate) use typedarray::dataview_ctor;
pub(crate) use typedarray::float32array_ctor;
pub(crate) use typedarray::float64array_ctor;
pub(crate) use typedarray::int8array_ctor;
pub(crate) use typedarray::int16array_ctor;
pub(crate) use typedarray::int32array_ctor;
pub(crate) use typedarray::ta_decode_bytes;
pub(crate) use typedarray::ta_encode_bytes;
pub(crate) use typedarray::uint8array_ctor;
pub(crate) use typedarray::uint8clamped_ctor;
pub(crate) use typedarray::uint16array_ctor;
pub(crate) use typedarray::uint32array_ctor;

// ── declarative builtin registry ─────────────────────────────────────────────

/// The kind of a builtin: a method on a receiver value, a static function
/// under a namespace (`Math.abs`, `JSON.parse`, …), or a constructor
/// (`Array`, `Map`, … — Step 2a Part 2). Constructors are callable
/// `Value::Builtin`s whose `type_tag` keys the prototype side table and
/// derives the owning-namespace name (`TypeTag::name`), so their static
/// methods (`Array.isArray`) and `.prototype` resolve as virtual rungs off
/// the constructor value.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BuiltinKind {
    Method,
    Namespace(&'static str),
    Constructor { type_tag: TypeTag },
}

/// Sentinel for variadic builtins: no upper bound on argument count.
const VARARG: u32 = u32::MAX;

/// Helper for `builtins!`: emit a name-check-and-return when `$cond` is
/// `true`; expand to nothing when `false`. Keeps `method_for_receiver` free
/// of dead `if false { … }` blocks.
macro_rules! method_check {
    (false, $lit:literal, $variant:ident, $var:ident) => {};
    (true, $lit:literal, $variant:ident, $var:ident) => {
        if $lit == $var {
            return Some(Builtin::$variant);
        }
    };
}

/// The single-source-of-truth macro for every builtin. One row per builtin
/// declares its enum variant, kind, display name, argument bounds (counting the
/// receiver for methods), per-type validity booleans (array/string/map/set/
/// regexp/function/number/boolean), and handler function. The macro emits the
/// enum, `meta()`, `call()`, `for_method()`, `for_namespace()`, and
/// `method_for_receiver()` — no hand-written dispatch duplication.
///
/// The `number` and `boolean` columns (Step 2b) cover primitive method
/// resolution: `(5).toFixed` finds `NumberToFixed` via the `number` column,
/// with the receiver staying an unboxed primitive passed as arg 0 (no
/// boxing allocation — matching how engines treat primitive method calls).
macro_rules! builtins {
    (
        $(
            $variant:ident,
            $kind:expr,
            $name:literal,
            $min:expr,
            $max:expr,
            $handler:ident,
            $array:tt,
            $string:tt,
            $map:tt,
            $set:tt,
            $regexp:tt,
            $function:tt,
            $number:tt,
            $boolean:tt,
            $typedarray:tt,
            $dataview:tt;
        )*
    ) => {
        /// A builtin's identity. Used both as the static call target
        /// (`Instr::CallBuiltin(Builtin, argc)`, the compiler's fast path) and
        /// as a first-class value (`Value::Builtin(Builtin)`, for passing a
        /// builtin as a callback — invoked through `CallDyn`). The enum *is*
        /// the registry key: `Debug` prints the name and equality is trivial.
        #[derive(Copy, Clone, Debug, PartialEq, Eq)]
        pub enum Builtin {
            $(
                $variant,
            )*
        }

        /// Compile-time facts about a builtin: its display name and accepted
        /// argument count (inclusive, **counting the receiver** for methods).
        /// One source of truth the compiler reads for arity checks and error
        /// messages.
        pub struct BuiltinMeta {
            pub name: &'static str,
            pub min_args: u32,
            pub max_args: u32,
            pub kind: BuiltinKind,
        }

        impl Builtin {
            pub const fn meta(self) -> BuiltinMeta {
                match self {
                    $(
                        Builtin::$variant => BuiltinMeta {
                            name: $name,
                            min_args: $min,
                            max_args: $max,
                            kind: $kind,
                        },
                    )*
                }
            }

            /// Dispatch: run the builtin against `vm`, consuming `argc` stack
            /// arguments and pushing one result.
            ///
            /// Arguments are read in-place via `Args` — never moved, cloned, or
            /// collected. The epilogue truncates the stack and pushes the result
            /// on both the `Ok` and `Err` paths, preserving the pop-first
            /// invariant.
            pub fn call(self, vm: &mut VM, argc: u32) -> Result<(), VMError> {
                let n = argc as usize;
                if vm.stack.len() < n {
                    return Err(vm.fail(ErrorKind::StackUnderflow, "stack underflow"));
                }
                let base = vm.stack.len() - n;
                let args = Args { base, argc: n };
                let meta = self.meta();
                // Runtime arity: only require the receiver to be present for
                // methods; compile-time arity is strict (compiler lint).
                let min_runtime = match meta.kind {
                    BuiltinKind::Method => 1,
                    BuiltinKind::Namespace(_) | BuiltinKind::Constructor { .. } => 0,
                };
                if argc < min_runtime {
                    vm.stack.truncate(base);
                    return Err(vm.fail(
                        ErrorKind::BadArg,
                        format!(
                            "`{}` called with too few arguments ({argc})",
                            meta.name
                        ),
                    ));
                }
                let result = match self {
                    $(
                        Builtin::$variant => $handler(vm, args),
                    )*
                };
                // Epilogue: drop the args and push the result on success; on
                // error, drop the args and tag the message.
                match result {
                    Ok(val) => {
                        vm.stack.truncate(args.base);
                        vm.stack.push(val);
                        Ok(())
                    }
                    Err(mut e) => {
                        vm.stack.truncate(args.base);
                        e.message = format!("in `{}`: {}", meta.name, e.message);
                        Err(e)
                    }
                }
            }

            /// Look up a method builtin by name (for `recv.push(…)` style calls).
            pub fn for_method(name: &str) -> Option<Builtin> {
                $(
                    if matches!($kind, BuiltinKind::Method) && $name == name {
                        return Some(Builtin::$variant);
                    }
                )*
                None
            }

            /// Look up a namespaced builtin by namespace + method name (for
            /// `Math.abs(…)` style calls and `Math.sqrt` as a value).
            pub fn for_namespace(ns: &str, name: &str) -> Option<Builtin> {
                $(
                    if let BuiltinKind::Namespace(ns_val) = $kind {
                        if ns_val == ns && $name == name {
                            return Some(Builtin::$variant);
                        }
                    }
                )*
                None
            }

            /// Enumerate every `Namespace`-kind row as `(namespace, member,
            /// Builtin)`. The single source of truth for populating the real
            /// `Math`/`JSON`/… namespace objects (Step 2a Part 3): a new
            /// namespace method added to the `builtins!` table appears on the
            /// real object with no second edit, so the value path
            /// (`Math.max` read as a value) and the call path
            /// (`Math.max(…)` via `CallBuiltin`) cannot disagree.
            pub fn namespace_statics() -> impl Iterator<Item = (&'static str, &'static str, Builtin)> {
                [
                    $(
                        (if let BuiltinKind::Namespace(ns_val) = $kind {
                            Some((ns_val, $name, Builtin::$variant))
                        } else {
                            None
                        }),
                    )*
                ]
                .into_iter()
                .flatten()
            }

            /// Look up a constructor builtin by its JS name (`Array`, `Map`, …).
            /// Step 2a Part 2: constructors are callable `Value::Builtin`s, so
            /// the bare identifier `Array` and `new Map(…)` resolve through this.
            pub fn for_constructor(name: &str) -> Option<Builtin> {
                $(
                    if let BuiltinKind::Constructor { type_tag } = $kind {
                        if type_tag.name() == name {
                            return Some(Builtin::$variant);
                        }
                    }
                )*
                None
            }

            /// The `TypeTag` of a constructor builtin, or `None` for non-constructors.
            /// Used by `Instr::New` and the virtual-rung property reads
            /// (`.prototype`, static methods) to key the prototype side table.
            /// A direct per-variant match (each arm folds to a `const`
            /// `Option<TypeTag>`) — not `self.meta().kind`, so it never builds
            /// the full `BuiltinMeta` struct on the hot `new`/property-read path.
            pub const fn constructor_type_tag(self) -> Option<TypeTag> {
                match self {
                    $(
                        Builtin::$variant => match $kind {
                            BuiltinKind::Constructor { type_tag } => Some(type_tag),
                            _ => None,
                        },
                    )*
                }
            }

            /// The constructor `Builtin` for a `TypeTag` (`Array` → `ArrayCtor`),
            /// or `None` for a tag with no constructor row. The direct inverse
            /// of `constructor_type_tag` — an integer tag compare per
            /// constructor row (non-constructor rows fold away), so reflective
            /// reads like `[].constructor` recover the constructor value without
            /// going through `tag.name()` + a string scan of the registry.
            pub fn for_type_tag(tag: TypeTag) -> Option<Builtin> {
                $(
                    if let BuiltinKind::Constructor { type_tag } = $kind {
                        if type_tag as u8 == tag as u8 {
                            return Some(Builtin::$variant);
                        }
                    }
                )*
                None
            }

            /// Receiver-type-aware method-builtin lookup: given a receiver
            /// value and a method name, return the `Builtin` if the name is a
            /// valid method for that receiver *type*, or `None` otherwise.
            /// Generated from the `builtins!` rows — dispatches on receiver
            /// type first, then does a type-limited name search. Zero runtime
            /// iteration; zero dead `if false` blocks in source.
            pub fn method_for_receiver(recv: &Value, name: &str) -> Option<Builtin> {
                match recv {
                    Value::Array(_) => {
                        $(method_check!($array, $name, $variant, name);)*
                        None
                    }
                    Value::String(_) => {
                        $(method_check!($string, $name, $variant, name);)*
                        None
                    }
                    Value::Map(_) => {
                        $(method_check!($map, $name, $variant, name);)*
                        None
                    }
                    Value::Set(_) => {
                        $(method_check!($set, $name, $variant, name);)*
                        None
                    }
                    Value::RegExp(_) => {
                        $(method_check!($regexp, $name, $variant, name);)*
                        None
                    }
                    Value::Closure { .. } | Value::Builtin(_) | Value::Bound(_) => {
                        $(method_check!($function, $name, $variant, name);)*
                        None
                    }
                    Value::Float(_) | Value::PosInt(_) | Value::NegInt(_) => {
                        $(method_check!($number, $name, $variant, name);)*
                        None
                    }
                    Value::Bool(_) => {
                        $(method_check!($boolean, $name, $variant, name);)*
                        None
                    }
                    Value::TypedArray(_) => {
                        $(method_check!($typedarray, $name, $variant, name);)*
                        None
                    }
                    Value::DataView(_) => {
                        $(method_check!($dataview, $name, $variant, name);)*
                        None
                    }
                    Value::ArrayBuffer(_) => {
                        $(method_check!($dataview, $name, $variant, name);)*
                        None
                    }
                    _ => None,
                }
            }
        }
    };
}

builtins! {
    // ── Array static ──
    ArrayIsArray, BuiltinKind::Namespace("Array"), "isArray", 1, 1, array_is_array, false, false, false, false, false, false, false, false, false, false;
    ArrayFrom,    BuiltinKind::Namespace("Array"), "from",    1, 2, array_from,    false, false, false, false, false, false, false, false, false, false;

    // ── array methods ──
    ArrayPush,    BuiltinKind::Method, "push",        1, VARARG, array_push,    true,  false, false, false, false, false, false, false, false, false;
    ArrayPop,     BuiltinKind::Method, "pop",         1, 1,      array_pop,     true,  false, false, false, false, false, false, false, false, false;
    ArrayShift,   BuiltinKind::Method, "shift",       1, 1,      array_shift,   true,  false, false, false, false, false, false, false, false, false;
    ArrayUnshift, BuiltinKind::Method, "unshift",     1, VARARG, array_unshift, true,  false, false, false, false, false, false, false, false, false;
    ArrayJoin,    BuiltinKind::Method, "join",        1, 2,      array_join,    true,  false, false, false, false, false, false, false, false, false;
    ArrayReverse, BuiltinKind::Method, "reverse",     1, 1,      array_reverse, true,  false, false, false, false, false, false, false, false, false;
    ArrayFlat,    BuiltinKind::Method, "flat",        1, 2,      array_flat,    true,  false, false, false, false, false, false, false, false, false;
    ArrayFill,    BuiltinKind::Method, "fill",        2, VARARG, array_fill,    true,  false, false, false, false, false, false, false, false, false;
    ArraySplice,  BuiltinKind::Method, "splice",      1, VARARG, array_splice,  true,  false, false, false, false, false, false, false, false, false;

    // ── console ──
    ConsoleLog,  BuiltinKind::Namespace("console"), "log",  0, VARARG, console_log,  false, false, false, false, false, false, false, false, false, false;
    ConsoleWarn, BuiltinKind::Namespace("console"), "warn", 0, VARARG, console_warn, false, false, false, false, false, false, false, false, false, false;
    ConsoleError,BuiltinKind::Namespace("console"), "error",0, VARARG, console_error,false, false, false, false, false, false, false, false, false, false;
    ConsoleInfo, BuiltinKind::Namespace("console"), "info", 0, VARARG, console_info, false, false, false, false, false, false, false, false, false, false;
    ConsoleAssert,BuiltinKind::Namespace("console"), "assert", 1, VARARG, console_assert, false, false, false, false, false, false, false, false, false, false;

    // ── JSON static ──
    JSONParse,     BuiltinKind::Namespace("JSON"), "parse",     1, 1, json_parse,     false, false, false, false, false, false, false, false, false, false;
    JSONStringify, BuiltinKind::Namespace("JSON"), "stringify", 1, 3, json_stringify, false, false, false, false, false, false, false, false, false, false;

    // ── Math ──
    MathAbs,   BuiltinKind::Namespace("Math"), "abs",   1, 1,      math_abs,   false, false, false, false, false, false, false, false, false, false;
    MathSqrt,  BuiltinKind::Namespace("Math"), "sqrt",  1, 1,      math_sqrt,  false, false, false, false, false, false, false, false, false, false;
    MathCeil,  BuiltinKind::Namespace("Math"), "ceil",  1, 1,      math_ceil,  false, false, false, false, false, false, false, false, false, false;
    MathFloor, BuiltinKind::Namespace("Math"), "floor", 1, 1,      math_floor, false, false, false, false, false, false, false, false, false, false;
    MathRound, BuiltinKind::Namespace("Math"), "round", 1, 1,      math_round, false, false, false, false, false, false, false, false, false, false;
    MathSign,  BuiltinKind::Namespace("Math"), "sign",  1, 1,      math_sign,  false, false, false, false, false, false, false, false, false, false;
    MathMin,   BuiltinKind::Namespace("Math"), "min",   0, VARARG, math_min,   false, false, false, false, false, false, false, false, false, false;
    MathMax,   BuiltinKind::Namespace("Math"), "max",   0, VARARG, math_max,   false, false, false, false, false, false, false, false, false, false;
    MathPow,   BuiltinKind::Namespace("Math"), "pow",   2, 2,      math_pow,   false, false, false, false, false, false, false, false, false, false;
    MathTrunc, BuiltinKind::Namespace("Math"), "trunc", 1, 1,      math_trunc, false, false, false, false, false, false, false, false, false, false;
    MathCbrt,  BuiltinKind::Namespace("Math"), "cbrt",  1, 1,      math_cbrt,  false, false, false, false, false, false, false, false, false, false;
    MathExp,   BuiltinKind::Namespace("Math"), "exp",   1, 1,      math_exp,   false, false, false, false, false, false, false, false, false, false;
    MathLog,   BuiltinKind::Namespace("Math"), "log",   1, 1,      math_log,   false, false, false, false, false, false, false, false, false, false;
    MathLog2,  BuiltinKind::Namespace("Math"), "log2",  1, 1,      math_log2,  false, false, false, false, false, false, false, false, false, false;
    MathLog10, BuiltinKind::Namespace("Math"), "log10", 1, 1,      math_log10, false, false, false, false, false, false, false, false, false, false;
    MathSin,   BuiltinKind::Namespace("Math"), "sin",   1, 1,      math_sin,   false, false, false, false, false, false, false, false, false, false;
    MathCos,   BuiltinKind::Namespace("Math"), "cos",   1, 1,      math_cos,   false, false, false, false, false, false, false, false, false, false;
    MathTan,   BuiltinKind::Namespace("Math"), "tan",   1, 1,      math_tan,   false, false, false, false, false, false, false, false, false, false;
    MathAsin,  BuiltinKind::Namespace("Math"), "asin",  1, 1,      math_asin,  false, false, false, false, false, false, false, false, false, false;
    MathAcos,  BuiltinKind::Namespace("Math"), "acos",  1, 1,      math_acos,  false, false, false, false, false, false, false, false, false, false;
    MathAtan,  BuiltinKind::Namespace("Math"), "atan",  1, 1,      math_atan,  false, false, false, false, false, false, false, false, false, false;
    MathAtan2, BuiltinKind::Namespace("Math"), "atan2", 2, 2,      math_atan2, false, false, false, false, false, false, false, false, false, false;
    MathHypot, BuiltinKind::Namespace("Math"), "hypot", 0, VARARG, math_hypot, false, false, false, false, false, false, false, false, false, false;

    // ── Number static ──
    NumberIsInteger,  BuiltinKind::Namespace("Number"), "isInteger",  1, 1, number_is_integer,  false, false, false, false, false, false, false, false, false, false;
    NumberIsFinite,   BuiltinKind::Namespace("Number"), "isFinite",   1, 1, number_is_finite,   false, false, false, false, false, false, false, false, false, false;
    NumberIsNaN,      BuiltinKind::Namespace("Number"), "isNaN",      1, 1, number_is_nan,      false, false, false, false, false, false, false, false, false, false;
    NumberParseInt,   BuiltinKind::Namespace("Number"), "parseInt",   1, 2, number_parse_int,   false, false, false, false, false, false, false, false, false, false;
    NumberParseFloat, BuiltinKind::Namespace("Number"), "parseFloat", 1, 1, number_parse_float, false, false, false, false, false, false, false, false, false, false;

    // ── Number.prototype methods (Step 2b — primitive method resolution
    //     without boxing; the receiver stays an unboxed number value) ──
    NumberToFixed, BuiltinKind::Method, "toFixed", 1, 2, number_to_fixed, false, false, false, false, false, false, true, false, false, false;

    // ── Object static ──
    ObjKeys,        BuiltinKind::Namespace("Object"), "keys",        1, 1,      obj_keys,         false, false, false, false, false, false, false, false, false, false;
    ObjValues,      BuiltinKind::Namespace("Object"), "values",      1, 1,      obj_values,       false, false, false, false, false, false, false, false, false, false;
    ObjEntries,     BuiltinKind::Namespace("Object"), "entries",     1, 1,      obj_entries,      false, false, false, false, false, false, false, false, false, false;
    ObjFromEntries, BuiltinKind::Namespace("Object"), "fromEntries", 1, 1,      obj_from_entries, false, false, false, false, false, false, false, false, false, false;
    ObjAssign,      BuiltinKind::Namespace("Object"), "assign",      1, VARARG, obj_assign,       false, false, false, false, false, false, false, false, false, false;
    ObjHasOwn,      BuiltinKind::Namespace("Object"), "hasOwn",      2, 2,      obj_has_own,      false, false, false, false, false, false, false, false, false, false;
    ObjGetProtoOf,  BuiltinKind::Namespace("Object"), "getPrototypeOf", 1, 1,   obj_get_proto_of, false, false, false, false, false, false, false, false, false, false;
    ObjSetProtoOf,  BuiltinKind::Namespace("Object"), "setPrototypeOf", 2, 2,   obj_set_proto_of, false, false, false, false, false, false, false, false, false, false;
    ObjCreate,      BuiltinKind::Namespace("Object"), "create",          1, 2,   obj_create,       false, false, false, false, false, false, false, false, false, false;
    ObjFreeze,            BuiltinKind::Namespace("Object"), "freeze",            1, 1, obj_freeze,             false, false, false, false, false, false, false, false, false, false;
    ObjIsFrozen,          BuiltinKind::Namespace("Object"), "isFrozen",          1, 1, obj_is_frozen,          false, false, false, false, false, false, false, false, false, false;
    ObjSeal,              BuiltinKind::Namespace("Object"), "seal",              1, 1, obj_seal,               false, false, false, false, false, false, false, false, false, false;
    ObjIsSealed,          BuiltinKind::Namespace("Object"), "isSealed",          1, 1, obj_is_sealed,          false, false, false, false, false, false, false, false, false, false;
    ObjPreventExtensions, BuiltinKind::Namespace("Object"), "preventExtensions", 1, 1, obj_prevent_extensions, false, false, false, false, false, false, false, false, false, false;
    ObjIsExtensible,      BuiltinKind::Namespace("Object"), "isExtensible",      1, 1, obj_is_extensible,      false, false, false, false, false, false, false, false, false, false;

    // ── object methods ──
    ObjHasOwnProperty, BuiltinKind::Method, "hasOwnProperty", 2, 2, obj_has_own_property, false, false, false, false, false, false, false, false, false, false;

    // ── string methods ──
    StrSplit,       BuiltinKind::Method, "split",       2, 3, str_split,       false, true,  false, false, false, false, false, false, false, false;
    StrIncludes,    BuiltinKind::Method, "includes",    2, 3, includes_poly,   true,  true,  false, false, false, false, false, false, false, false;
    StrIndexOf,     BuiltinKind::Method, "indexOf",     2, 3, index_of_poly,  true,  true,  false, false, false, false, false, false, false, false;
    StrLastIndexOf, BuiltinKind::Method, "lastIndexOf", 2, 3, last_index_of_poly, true, true, false, false, false, false, false, false, false, false;
    StrStartsWith,  BuiltinKind::Method, "startsWith",  2, 2, str_starts_with, false, true,  false, false, false, false, false, false, false, false;
    StrEndsWith,    BuiltinKind::Method, "endsWith",    2, 2, str_ends_with,   false, true,  false, false, false, false, false, false, false, false;
    StrSlice,       BuiltinKind::Method, "slice",       2, 3, slice_poly,      true,  true,  false, false, false, false, false, false, false, false;
    StrSubstring,   BuiltinKind::Method, "substring",   2, 3, str_substring,   false, true,  false, false, false, false, false, false, false, false;
    StrTrim,        BuiltinKind::Method, "trim",        1, 1, str_trim,        false, true,  false, false, false, false, false, false, false, false;
    StrReplace,      BuiltinKind::Method, "__replaceStr",    3, 3, str_replace,        false, true, false, false, false, false, false, false, false, false;
    StrReplaceAll,   BuiltinKind::Method, "__replaceAllStr", 3, 3, str_replace_all,    false, true, false, false, false, false, false, false, false, false;
    StrToLowerCase,  BuiltinKind::Method, "toLowerCase",  1, 1, str_to_lower_case,  false, true, false, false, false, false, false, false, false, false;
    StrToUpperCase,  BuiltinKind::Method, "toUpperCase",  1, 1, str_to_upper_case,  false, true, false, false, false, false, false, false, false, false;
    StrPadStart,     BuiltinKind::Method, "padStart",     2, 3, str_pad_start,     false, true, false, false, false, false, false, false, false, false;
    StrPadEnd,       BuiltinKind::Method, "padEnd",       2, 3, str_pad_end,       false, true, false, false, false, false, false, false, false, false;
    StrRepeat,       BuiltinKind::Method, "repeat",       2, 2, str_repeat,        false, true, false, false, false, false, false, false, false, false;
    StrTrimStart,    BuiltinKind::Method, "trimStart",    1, 1, str_trim_start,    false, true, false, false, false, false, false, false, false, false;
    StrTrimEnd,      BuiltinKind::Method, "trimEnd",      1, 1, str_trim_end,      false, true, false, false, false, false, false, false, false, false;
    StrCharAt,       BuiltinKind::Method, "charAt",       2, 2, str_char_at,       false, true, false, false, false, false, false, false, false, false;
    StrAt,           BuiltinKind::Method, "at",           2, 2, at_poly,           true,  true, false, false, false, false, false, false, false, false;
    StrConcat,       BuiltinKind::Method, "concat",       1, VARARG, concat_poly,  true,  true, false, false, false, false, false, false, false, false;

    // ── RegExp methods ──
    RegExpTest,     BuiltinKind::Method, "test",     2, 2, regexp_test, false, false, false, false, true, false, false, false, false, false;
    RegExpExec,     BuiltinKind::Method, "exec",     2, 2, regexp_exec, false, false, false, false, true, false, false, false, false, false;

    // ── universal methods ──
    // `toString`: 1 arg (receiver) for most types; 2 (receiver + radix) for
    // numbers (Step 2b: `(255).toString(16)` → "ff"). Extra args are ignored
    // at runtime for non-number receivers.
    ToString, BuiltinKind::Method, "toString", 1, 2, value_to_string, true, true, true, true, true, true, true, true, false, false;

    // ── Function methods ──
    FunctionBind,  BuiltinKind::Method, "bind",  1, VARARG, function_bind, false, false, false, false, false, true, false, false, false, false;

    // ── string methods that accept RegExp ──
    StrMatch,    BuiltinKind::Method, "match",    2, 2, str_match,     false, true, false, false, false, false, false, false, false, false;
    StrMatchAll, BuiltinKind::Method, "matchAll", 2, 2, str_match_all, false, true, false, false, false, false, false, false, false, false;
    StrSearch,   BuiltinKind::Method, "search",   2, 2, str_search,    false, true, false, false, false, false, false, false, false, false;

    // ── String static ──
    StrFromCharCode,  BuiltinKind::Namespace("String"), "fromCharCode",  0, VARARG, str_from_char_code,  false, false, false, false, false, false, false, false, false, false;
    StrFromCodePoint, BuiltinKind::Namespace("String"), "fromCodePoint", 0, VARARG, str_from_code_point, false, false, false, false, false, false, false, false, false, false;

    // ── Edit static ──
    EditReplaceOnce,       BuiltinKind::Namespace("Edit"), "replaceOnce",    3, 3, edit_replace_once,       false, false, false, false, false, false, false, false, false, false;
    EditReplaceCount,      BuiltinKind::Namespace("Edit"), "replaceCount",   3, 3, edit_replace_count,      false, false, false, false, false, false, false, false, false, false;
    EditCount,             BuiltinKind::Namespace("Edit"), "count",          2, 2, edit_count,             false, false, false, false, false, false, false, false, false, false;
    EditExtractBlock,      BuiltinKind::Namespace("Edit"), "extractBlock",   2, 2, edit_extract_block,      false, false, false, false, false, false, false, false, false, false;
    EditExtractByIndent,   BuiltinKind::Namespace("Edit"), "extractByIndent",    2, 2, edit_extract_by_indent,   false, false, false, false, false, false, false, false, false, false;
    EditExtractEnclosing,  BuiltinKind::Namespace("Edit"), "extractEnclosing",   4, 4, edit_extract_enclosing,  false, false, false, false, false, false, false, false, false, false;
    EditReplaceLines,      BuiltinKind::Namespace("Edit"), "replaceLines",   4, 4, edit_replace_lines,      false, false, false, false, false, false, false, false, false, false;
    EditInsertAt,          BuiltinKind::Namespace("Edit"), "insertAt",       3, 3, edit_insert_at,          false, false, false, false, false, false, false, false, false, false;
    EditApplyEdits,        BuiltinKind::Namespace("Edit"), "applyEdits",     2, 2, edit_apply_edits,        false, false, false, false, false, false, false, false, false, false;

    // ── Map static ──
    MapIsMap, BuiltinKind::Namespace("Map"), "isMap", 1, 1, map_is_map, false, false, false, false, false, false, false, false, false, false;

    // ── Map methods ──
    MapGet,    BuiltinKind::Method, "get",    2, 2, map_get, false, false, true, false, false, false, false, false, false, false;
    MapSet,    BuiltinKind::Method, "set",    3, 3, map_set, false, false, true, false, false, false, false, false, false, false;

    // ── Set static ──
    SetIsSet, BuiltinKind::Namespace("Set"), "isSet", 1, 1, set_is_set, false, false, false, false, false, false, false, false, false, false;

    // ── Set methods ──
    SetAdd,    BuiltinKind::Method, "add",    2, 2, set_add, false, false, false, true, false, false, false, false, false, false;

    // ── Map/Set shared methods ──
    MapSetHas,     BuiltinKind::Method, "has",     2, 2, map_set_has,     false, false, true, true, false, false, false, false, false, false;
    MapSetDelete,  BuiltinKind::Method, "delete",  2, 2, map_set_delete,  false, false, true, true, false, false, false, false, false, false;
    MapSetClear,   BuiltinKind::Method, "clear",   1, 1, map_set_clear,   false, false, true, true, false, false, false, false, false, false;
    MapSetKeys,    BuiltinKind::Method, "keys",    1, 1, map_set_keys,    false, false, true, true, false, false, false, false, false, false;
    MapSetValues,  BuiltinKind::Method, "values",  1, 1, map_set_values,  false, false, true, true, false, false, false, false, false, false;
    MapSetEntries, BuiltinKind::Method, "entries", 1, 1, map_set_entries, false, false, true, true, false, false, false, false, false, false;

    // ── Constructors (Step 2a Part 2) ──
    // Callable `Value::Builtin`s keyed by `BuiltinKind::Constructor { type_tag }`.
    // The handler is the plain-call behavior (`Array(3)`, `Number("5")`, …);
    // `Instr::New` dispatches the `new` path directly (via `construct_builtin`).
    // `Map`/`Set` require `new` — their handler throws. `Function` is a
    // constructor value for reflection (`Map instanceof Function`,
    // `Object.getPrototypeOf(Array) === Function.prototype`) but neither
    // `new Function(body)` nor `Function(body)` is supported — both throw
    // (a documented divergence; function expressions are the alternative).
    ArrayCtor,   BuiltinKind::Constructor { type_tag: TypeTag::Array },   "Array",   0, VARARG, array_ctor,   false, false, false, false, false, false, false, false, false, false;
    ObjectCtor,  BuiltinKind::Constructor { type_tag: TypeTag::Object },  "Object",  0, 1,      object_ctor,  false, false, false, false, false, false, false, false, false, false;
    MapCtor,     BuiltinKind::Constructor { type_tag: TypeTag::Map },     "Map",     0, 1,      map_ctor,     false, false, false, false, false, false, false, false, false, false;
    SetCtor,     BuiltinKind::Constructor { type_tag: TypeTag::Set },     "Set",     0, 1,      set_ctor,     false, false, false, false, false, false, false, false, false, false;
    RegExpCtor,  BuiltinKind::Constructor { type_tag: TypeTag::RegExp },  "RegExp",  1, 2,      regexp_ctor,  false, false, false, false, false, false, false, false, false, false;
    NumberCtor,  BuiltinKind::Constructor { type_tag: TypeTag::Number },  "Number",  1, 1,      number_ctor,  false, false, false, false, false, false, false, false, false, false;
    StringCtor,  BuiltinKind::Constructor { type_tag: TypeTag::String },  "String",  1, 1,      string_ctor,  false, false, false, false, false, false, false, false, false, false;
    BooleanCtor, BuiltinKind::Constructor { type_tag: TypeTag::Boolean }, "Boolean", 1, 1,      boolean_ctor, false, false, false, false, false, false, false, false, false, false;
    FunctionCtor,BuiltinKind::Constructor { type_tag: TypeTag::Function },"Function",0, VARARG, function_ctor, false, false, false, false, false, false, false, false, false, false;

    // ── ArrayBuffer ──
    ArrayBufferCtor,    BuiltinKind::Constructor { type_tag: TypeTag::ArrayBuffer },        "ArrayBuffer",        1, 1,      arraybuffer_ctor,      false, false, false, false, false, false, false, false, false, false;
    ArrayBufferIsView,  BuiltinKind::Namespace("ArrayBuffer"), "isView",                    1, 1,      arraybuffer_is_view,   false, false, false, false, false, false, false, false, false, false;
    ArrayBufferSlice,   BuiltinKind::Method, "slice",                                       2, 3,      arraybuffer_slice,     false, false, false, false, false, false, false, false, false, true;

    // ── Typed array constructors ──
    Int8ArrayCtor,         BuiltinKind::Constructor { type_tag: TypeTag::Int8Array },         "Int8Array",         0, VARARG, int8array_ctor,        false, false, false, false, false, false, false, false, false, false;
    Uint8ArrayCtor,        BuiltinKind::Constructor { type_tag: TypeTag::Uint8Array },        "Uint8Array",        0, VARARG, uint8array_ctor,       false, false, false, false, false, false, false, false, false, false;
    Uint8ClampedArrayCtor, BuiltinKind::Constructor { type_tag: TypeTag::Uint8ClampedArray }, "Uint8ClampedArray", 0, VARARG, uint8clamped_ctor,     false, false, false, false, false, false, false, false, false, false;
    Int16ArrayCtor,        BuiltinKind::Constructor { type_tag: TypeTag::Int16Array },        "Int16Array",        0, VARARG, int16array_ctor,       false, false, false, false, false, false, false, false, false, false;
    Uint16ArrayCtor,       BuiltinKind::Constructor { type_tag: TypeTag::Uint16Array },       "Uint16Array",       0, VARARG, uint16array_ctor,      false, false, false, false, false, false, false, false, false, false;
    Int32ArrayCtor,        BuiltinKind::Constructor { type_tag: TypeTag::Int32Array },        "Int32Array",        0, VARARG, int32array_ctor,       false, false, false, false, false, false, false, false, false, false;
    Uint32ArrayCtor,       BuiltinKind::Constructor { type_tag: TypeTag::Uint32Array },       "Uint32Array",       0, VARARG, uint32array_ctor,      false, false, false, false, false, false, false, false, false, false;
    Float32ArrayCtor,      BuiltinKind::Constructor { type_tag: TypeTag::Float32Array },      "Float32Array",      0, VARARG, float32array_ctor,     false, false, false, false, false, false, false, false, false, false;
    Float64ArrayCtor,      BuiltinKind::Constructor { type_tag: TypeTag::Float64Array },      "Float64Array",      0, VARARG, float64array_ctor,     false, false, false, false, false, false, false, false, false, false;
    BigInt64ArrayCtor,     BuiltinKind::Constructor { type_tag: TypeTag::BigInt64Array },     "BigInt64Array",     0, VARARG, bigint64array_ctor,    false, false, false, false, false, false, false, false, false, false;
    BigUint64ArrayCtor,    BuiltinKind::Constructor { type_tag: TypeTag::BigUint64Array },    "BigUint64Array",    0, VARARG, biguint64array_ctor,   false, false, false, false, false, false, false, false, false, false;

    // ── DataView (Step 5) ──
    DataViewCtor,      BuiltinKind::Constructor { type_tag: TypeTag::DataView }, "DataView", 1, 3, dataview_ctor, false, false, false, false, false, false, false, false, false, false;
    DvGetInt8,         BuiltinKind::Method, "getInt8",     2, 2, dv_get_int8,    false, false, false, false, false, false, false, false, false, true;
    DvGetUint8,        BuiltinKind::Method, "getUint8",    2, 2, dv_get_uint8,   false, false, false, false, false, false, false, false, false, true;
    DvGetInt16,        BuiltinKind::Method, "getInt16",    2, 3, dv_get_int16,   false, false, false, false, false, false, false, false, false, true;
    DvGetUint16,       BuiltinKind::Method, "getUint16",   2, 3, dv_get_uint16,  false, false, false, false, false, false, false, false, false, true;
    DvGetInt32,        BuiltinKind::Method, "getInt32",    2, 3, dv_get_int32,   false, false, false, false, false, false, false, false, false, true;
    DvGetUint32,       BuiltinKind::Method, "getUint32",   2, 3, dv_get_uint32,  false, false, false, false, false, false, false, false, false, true;
    DvGetFloat32,      BuiltinKind::Method, "getFloat32",  2, 3, dv_get_float32, false, false, false, false, false, false, false, false, false, true;
    DvGetFloat64,      BuiltinKind::Method, "getFloat64",  2, 3, dv_get_float64, false, false, false, false, false, false, false, false, false, true;
    DvGetBigInt64,     BuiltinKind::Method, "getBigInt64", 2, 3, dv_get_bigint64,  false, false, false, false, false, false, false, false, false, true;
    DvGetBigUint64,    BuiltinKind::Method, "getBigUint64",2, 3, dv_get_biguint64, false, false, false, false, false, false, false, false, false, true;
    DvSetInt8,         BuiltinKind::Method, "setInt8",     3, 3, dv_set_int8,    false, false, false, false, false, false, false, false, false, true;
    DvSetUint8,        BuiltinKind::Method, "setUint8",    3, 3, dv_set_uint8,   false, false, false, false, false, false, false, false, false, true;
    DvSetInt16,        BuiltinKind::Method, "setInt16",    3, 4, dv_set_int16,   false, false, false, false, false, false, false, false, false, true;
    DvSetUint16,       BuiltinKind::Method, "setUint16",   3, 4, dv_set_uint16,  false, false, false, false, false, false, false, false, false, true;
    DvSetInt32,        BuiltinKind::Method, "setInt32",    3, 4, dv_set_int32,   false, false, false, false, false, false, false, false, false, true;
    DvSetUint32,       BuiltinKind::Method, "setUint32",   3, 4, dv_set_uint32,  false, false, false, false, false, false, false, false, false, true;
    DvSetFloat32,      BuiltinKind::Method, "setFloat32",  3, 4, dv_set_float32, false, false, false, false, false, false, false, false, false, true;
    DvSetFloat64,      BuiltinKind::Method, "setFloat64",  3, 4, dv_set_float64, false, false, false, false, false, false, false, false, false, true;
    DvSetBigInt64,     BuiltinKind::Method, "setBigInt64", 3, 4, dv_set_bigint64,  false, false, false, false, false, false, false, false, false, true;
    DvSetBigUint64,    BuiltinKind::Method, "setBigUint64",3, 4, dv_set_biguint64, false, false, false, false, false, false, false, false, false, true;

    // ── Typed array prototype methods (Step 4) ──
    // All have typedarray: true (column 8). The static path compiles
    // to the first-matched builtin; call_builtin_or_shadow re-dispatches
    // to the receiver-specific handler at runtime.
    TaSubarray,    BuiltinKind::Method, "subarray",    2, 3,      ta_subarray,    false, false, false, false, false, false, false, false, true, false;
    TaSet,         BuiltinKind::Method, "set",         2, 3,      ta_set,         false, false, false, false, false, false, false, false, true, false;
    TaCopyWithin,  BuiltinKind::Method, "copyWithin",  2, 4,      ta_copywithin,  false, false, false, false, false, false, false, false, true, false;
    TaSlice,       BuiltinKind::Method, "slice",       2, 3,      ta_slice_ta,    false, false, false, false, false, false, false, false, true, false;
    TaAt,          BuiltinKind::Method, "at",          2, 2,      ta_at,          false, false, false, false, false, false, false, false, true, false;
    TaIncludes,    BuiltinKind::Method, "includes",    2, 3,      ta_includes,    false, false, false, false, false, false, false, false, true, false;
    TaIndexOf,     BuiltinKind::Method, "indexOf",     2, 3,      ta_index_of,    false, false, false, false, false, false, false, false, true, false;
    TaLastIndexOf, BuiltinKind::Method, "lastIndexOf", 2, 3,      ta_last_index_of, false, false, false, false, false, false, false, false, true, false;
    TaJoin,        BuiltinKind::Method, "join",        1, 2,      ta_join,        false, false, false, false, false, false, false, false, true, false;
    TaFill,        BuiltinKind::Method, "fill",        2, VARARG, ta_fill,        false, false, false, false, false, false, false, false, true, false;
    TaReverse,     BuiltinKind::Method, "reverse",     1, 1,      ta_reverse,     false, false, false, false, false, false, false, false, true, false;
    TaSort,        BuiltinKind::Method, "sort",        1, 2,      ta_sort,        false, false, false, false, false, false, false, false, true, false;
    TaKeys,        BuiltinKind::Method, "keys",        1, 1,      ta_keys,        false, false, false, false, false, false, false, false, true, false;
    TaValues,      BuiltinKind::Method, "values",      1, 1,      ta_values,      false, false, false, false, false, false, false, false, true, false;
    TaEntries,     BuiltinKind::Method, "entries",     1, 1,      ta_entries,     false, false, false, false, false, false, false, false, true, false;
}

// ── argument accessor ────────────────────────────────────────────────────────

/// Zero-cost argument handle: a short-lived borrow token that reads arguments
/// in-place on the stack. `Copy` so handlers can pass it by value.
///
/// An absent argument (index ≥ argc) yields `&Value::Undefined`, matching JS
/// semantics. Handlers apply JS-level defaults for optional args (e.g. `join`
/// separator → `","`, `slice` end → length) themselves.
#[derive(Clone, Copy)]
pub(crate) struct Args {
    /// Index of arg 0 in `vm.stack` (the deepest).
    pub(crate) base: usize,
    /// Number of arguments present.
    pub(crate) argc: usize,
}

impl Args {
    /// Arg `i`, or `&Value::Undefined` if absent. Zero-cost — no clone.
    pub(crate) fn get<'a>(&self, vm: &'a VM, i: usize) -> &'a Value {
        if i < self.argc {
            &vm.stack[self.base + i]
        } else {
            &Value::Undefined
        }
    }
    /// All args (arg 0 = receiver, deepest) as a read-only slice.
    #[allow(dead_code)]
    fn slice<'a>(&self, vm: &'a VM) -> &'a [Value] {
        &vm.stack[self.base..self.base + self.argc]
    }

    // ── method-receiver extraction ──────────────────────────────────────────
    //
    // Each method builtin validates its receiver (arg 0) through one of these.
    // A non-matching receiver routes through `VM::method_receiver_error`,
    // which raises a `TypeError`. Object receivers are intercepted before
    // the builtin handler is called (at the `CallBuiltin`/`dispatch_call`
    // sites) to resolve the method via the unified own-properties →
    // proto-chain walk — so the signal is produced *at* the
    // receiver check, never inferred downstream.

    /// Receiver as an array pointer (else the method-receiver error).
    fn array_receiver(&self, vm: &VM) -> Result<ArrayPtr, VMError> {
        match self.get(vm, 0) {
            Value::Array(p) => Ok(*p),
            recv => Err(vm.method_receiver_error(recv)),
        }
    }

    /// Receiver as a map pointer (else the method-receiver error).
    fn map_receiver(&self, vm: &VM) -> Result<MapPtr, VMError> {
        match self.get(vm, 0) {
            Value::Map(p) => Ok(*p),
            recv => Err(vm.method_receiver_error(recv)),
        }
    }

    /// Receiver as a set pointer (else the method-receiver error).
    fn set_receiver(&self, vm: &VM) -> Result<SetPtr, VMError> {
        match self.get(vm, 0) {
            Value::Set(p) => Ok(*p),
            recv => Err(vm.method_receiver_error(recv)),
        }
    }

    /// Receiver as a borrowed string slice (else the method-receiver error).
    fn str_receiver<'a>(&self, vm: &'a VM) -> Result<&'a str, VMError> {
        match self.get(vm, 0) {
            Value::String(s) => Ok(s.as_str()),
            recv => Err(vm.method_receiver_error(recv)),
        }
    }

    /// Receiver as an owned `RcStr` (a refcount bump; for handlers that retain
    /// the string past a borrow of the VM). Else the method-receiver error.
    fn string_receiver(&self, vm: &VM) -> Result<RcStr, VMError> {
        match self.get(vm, 0) {
            Value::String(s) => Ok(s.clone()),
            recv => Err(vm.method_receiver_error(recv)),
        }
    }

    /// Receiver as a borrowed compiled regexp (else the method-receiver error).
    fn regexp_receiver<'a>(&self, vm: &'a VM) -> Result<&'a crate::vm::RcRegExp, VMError> {
        match self.get(vm, 0) {
            Value::RegExp(rx) => Ok(rx),
            recv => Err(vm.method_receiver_error(recv)),
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{self, run_instrs};
    use crate::vm::{Instr, StepResult, VM};

    // ── first-class value tests ────────────────────────────────────────

    #[test]
    fn builtin_as_first_class_value_via_calldyn() {
        let out = run_instrs(vec![
            Instr::PushBuiltin(Builtin::MathMax),
            Instr::PushFloat(2.0),
            Instr::PushFloat(7.0),
            Instr::CallDyn(2, false),
        ]);
        assert_eq!(out, vec![Value::Float(7.0)]);
    }

    #[test]
    fn builtin_value_shape() {
        let mut vm = VM::new(vec![Instr::PushBuiltin(Builtin::MathMax), Instr::TypeOf]);
        while !matches!(vm.step(u64::MAX).unwrap(), StepResult::Done { .. }) {}
        match vm.stack.last() {
            Some(Value::String(s)) => assert_eq!(s.as_str(), "function"),
            other => panic!("{other:?}"),
        }
    }

    // ── Step 1: new Args-based tests ───────────────────────────────────

    #[test]
    fn optional_arg_defaults_via_undefined_rule() {
        // [1,2,3].join() → "1,2,3"
        assert_eq!(
            testutil::run_ret("return [1,2,3].join();"),
            serde_json::json!("1,2,3")
        );
        // "a,b".split(",") → ["a","b"]
        assert_eq!(
            testutil::run_ret("return 'a,b'.split(',');"),
            serde_json::json!(["a", "b"])
        );
        // "abc".slice(1) → "bc"
        assert_eq!(
            testutil::run_ret("return 'abc'.slice(1);"),
            serde_json::json!("bc")
        );
    }

    #[test]
    fn resumability_builtin_failure_consumes_operands() {
        // A wrong-receiver call (number.pop()) → TypeError. After failure,
        // operands should be consumed.
        let mut vm = VM::for_program(
            testutil::compile_ok("return (42).pop();"),
            serde_json::Value::Null,
        )
        .unwrap();
        let err = loop {
            match vm.step(u64::MAX) {
                Err(e) => break e,
                Ok(StepResult::Done { .. }) => panic!("expected error"),
                Ok(_) => {}
            }
        };
        assert_eq!(err.kind, ErrorKind::TypeError);
        // Stack has operands consumed.
        vm.resume_with(&err, Value::PosInt(99)).unwrap();
        loop {
            if let StepResult::Done { value, .. } = vm.step(u64::MAX).unwrap() {
                assert_eq!(value, Value::PosInt(99));
                break;
            }
        }
    }

    // ── Step 2: lookup tests ───────────────────────────────────────────

    #[test]
    fn for_method_lookup() {
        assert_eq!(Builtin::for_method("push"), Some(Builtin::ArrayPush));
        assert_eq!(Builtin::for_method("trim"), Some(Builtin::StrTrim));
        assert_eq!(Builtin::for_method("abs"), None); // namespace, not method
        assert_eq!(Builtin::for_method("nope"), None);
    }

    #[test]
    fn for_namespace_lookup() {
        assert_eq!(
            Builtin::for_namespace("Math", "abs"),
            Some(Builtin::MathAbs)
        );
        assert_eq!(
            Builtin::for_namespace("Object", "keys"),
            Some(Builtin::ObjKeys)
        );
        assert_eq!(Builtin::for_namespace("Math", "push"), None);
        assert_eq!(Builtin::for_namespace("Foo", "bar"), None);
    }

    // ── method shadow tests ────────────────────────────────────────────

    #[test]
    fn object_own_property_takes_precedence_over_builtin_method() {
        // An object with its own `push` function should call that
        // function, not Array.push.
        let out = testutil::run_ret(
            r#"
            const obj = { push(x) { return x + 1; } };
            return obj.push(5);
            "#,
        );
        assert_eq!(out, serde_json::json!(6));
    }

    #[test]
    fn object_without_property_falls_back_to_builtin() {
        // An object without a `push` property still gets the builtin
        // error (since builtins validate their receiver type).
        let prog = testutil::compile_ok(
            r#"
            const obj = { x: 1 };
            return obj.push(5);
            "#,
        );
        let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
        let err = loop {
            match vm.step(u64::MAX) {
                Err(e) => break e,
                Ok(StepResult::Done { .. }) => panic!("expected error"),
                Ok(_) => {}
            }
        };
        assert_eq!(err.kind, crate::vm::ErrorKind::TypeError);
    }

    #[test]
    fn array_builtin_method_still_works() {
        // Array.push should still work normally for arrays.
        let out = testutil::run_ret("const a = [1,2]; a.push(3); return a;");
        assert_eq!(out, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn string_builtin_method_still_works() {
        // String.trim should still work normally for strings.
        let out = testutil::run_ret("return '  hi  '.trim();");
        assert_eq!(out, serde_json::json!("hi"));
    }

    #[test]
    fn object_shadows_string_method() {
        // Object with its own `trim` property should shadow String.trim.
        let out = testutil::run_ret(
            r#"
            const obj = { trim() { return 42; } };
            return obj.trim();
            "#,
        );
        assert_eq!(out, serde_json::json!(42));
    }
}
