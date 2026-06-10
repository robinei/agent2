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

use crate::vm::{ErrorKind, VM, VMError, Value};

mod array;
mod console;
mod json;
mod math;
mod number;
mod object;
mod poly;
mod string;

use array::*;
use console::*;
use json::*;
use math::*;
use number::*;
use object::*;
use poly::*;
use string::*;

// ── declarative builtin registry ─────────────────────────────────────────────

/// The kind of a builtin: either a method on a receiver value (string or array),
/// or a static function under a namespace (`Math.abs`, `JSON.parse`, …).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BuiltinKind {
    Method,
    Namespace(&'static str),
}

/// Sentinel for variadic builtins: no upper bound on argument count.
const VARARG: u32 = u32::MAX;

/// The single-source-of-truth macro for every builtin. One row per builtin
/// declares its enum variant, kind, display name, argument bounds (counting the
/// receiver for methods), and handler function. The macro emits the enum,
/// `meta()`, `call()`, `for_method()`, and `for_namespace()` — no hand-written
/// dispatch duplication.
macro_rules! builtins {
    (
        $(
            $variant:ident, $kind:expr, $name:literal, $min:expr, $max:expr, $handler:ident;
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
                // Runtime arity: only require the receiver to be present for
                // methods; compile-time arity is strict (compiler lint).
                let min_runtime = match self.meta().kind {
                    BuiltinKind::Method => 1,
                    BuiltinKind::Namespace(_) => 0,
                };
                if argc < min_runtime {
                    vm.stack.truncate(base);
                    return Err(vm.fail(
                        ErrorKind::BadArg,
                        format!(
                            "`{}` called with too few arguments ({argc})",
                            self.meta().name
                        ),
                    ));
                }
                let result = match self {
                    $(
                        Builtin::$variant => $handler(vm, args),
                    )*
                };
                // Epilogue: truncate args on both paths, push result on Ok.
                vm.stack.truncate(args.base);
                match result {
                    Ok(val) => {
                        vm.stack.push(val);
                        Ok(())
                    }
                    Err(mut e) => {
                        e.message = format!("in `{}`: {}", self.meta().name, e.message);
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
        }
    };
}

builtins! {
    // ── Array static ──
    ArrayIsArray, BuiltinKind::Namespace("Array"), "isArray", 1, 1, array_is_array;

    // ── array methods (Method, receiver + args) ──
    ArrayPush,    BuiltinKind::Method, "push",        1, VARARG, array_push;
    ArrayPop,     BuiltinKind::Method, "pop",         1, 1,      array_pop;
    ArrayShift,   BuiltinKind::Method, "shift",       1, 1,      array_shift;
    ArrayUnshift, BuiltinKind::Method, "unshift",     1, VARARG, array_unshift;
    ArrayJoin,    BuiltinKind::Method, "join",        1, 2,      array_join;
    ArrayReverse, BuiltinKind::Method, "reverse",     1, 1,      array_reverse;
    ArrayFlat,    BuiltinKind::Method, "flat",        1, 2,      array_flat;
    ArrayFill,    BuiltinKind::Method, "fill",        2, VARARG, array_fill;
    ArraySplice,  BuiltinKind::Method, "splice",      1, VARARG, array_splice;

    // ── console ──
    ConsoleLog,  BuiltinKind::Namespace("console"), "log",  0, VARARG, console_log;
    ConsoleWarn, BuiltinKind::Namespace("console"), "warn", 0, VARARG, console_warn;
    ConsoleError,BuiltinKind::Namespace("console"), "error",0, VARARG, console_error;
    ConsoleInfo, BuiltinKind::Namespace("console"), "info", 0, VARARG, console_info;

    // ── JSON static ──
    JSONParse,     BuiltinKind::Namespace("JSON"), "parse",     1, 1, json_parse;
    JSONStringify, BuiltinKind::Namespace("JSON"), "stringify", 1, 1, json_stringify;

    // ── Math ──
    MathAbs,   BuiltinKind::Namespace("Math"), "abs",   1, 1,      math_abs;
    MathSqrt,  BuiltinKind::Namespace("Math"), "sqrt",  1, 1,      math_sqrt;
    MathCeil,  BuiltinKind::Namespace("Math"), "ceil",  1, 1,      math_ceil;
    MathFloor, BuiltinKind::Namespace("Math"), "floor", 1, 1,      math_floor;
    MathRound, BuiltinKind::Namespace("Math"), "round", 1, 1,      math_round;
    MathSign,  BuiltinKind::Namespace("Math"), "sign",  1, 1,      math_sign;
    MathMin,   BuiltinKind::Namespace("Math"), "min",   0, VARARG, math_min;
    MathMax,   BuiltinKind::Namespace("Math"), "max",   0, VARARG, math_max;
    MathPow,   BuiltinKind::Namespace("Math"), "pow",   2, 2,      math_pow;

    // ── Number static ──
    NumberIsInteger,  BuiltinKind::Namespace("Number"), "isInteger",  1, 1, number_is_integer;
    NumberParseInt,   BuiltinKind::Namespace("Number"), "parseInt",   1, 2, number_parse_int;
    NumberParseFloat, BuiltinKind::Namespace("Number"), "parseFloat", 1, 1, number_parse_float;

    // ── Object static ──
    ObjKeys,   BuiltinKind::Namespace("Object"), "keys",   1, 1, obj_keys;
    ObjValues, BuiltinKind::Namespace("Object"), "values", 1, 1, obj_values;

    // ── string methods ──
    StrSplit,       BuiltinKind::Method, "split",       2, 3, str_split;
    StrIncludes,    BuiltinKind::Method, "includes",    2, 3, includes_poly;
    StrIndexOf,     BuiltinKind::Method, "indexOf",     2, 3, index_of_poly;
    StrLastIndexOf, BuiltinKind::Method, "lastIndexOf", 2, 3, last_index_of_poly;
    StrStartsWith,  BuiltinKind::Method, "startsWith",  2, 2, str_starts_with;
    StrEndsWith,    BuiltinKind::Method, "endsWith",    2, 2, str_ends_with;
    StrSlice,       BuiltinKind::Method, "slice",       2, 3, slice_poly;
    StrTrim,        BuiltinKind::Method, "trim",        1, 1, str_trim;
    StrReplace,      BuiltinKind::Method, "replace",      3, 3, str_replace;
    StrReplaceAll,   BuiltinKind::Method, "replaceAll",   3, 3, str_replace_all;
    StrToLowerCase,  BuiltinKind::Method, "toLowerCase",  1, 1, str_to_lower_case;
    StrToUpperCase,  BuiltinKind::Method, "toUpperCase",  1, 1, str_to_upper_case;
    StrPadStart,     BuiltinKind::Method, "padStart",     2, 3, str_pad_start;
    StrPadEnd,       BuiltinKind::Method, "padEnd",       2, 3, str_pad_end;
    StrRepeat,       BuiltinKind::Method, "repeat",       2, 2, str_repeat;
    StrTrimStart,    BuiltinKind::Method, "trimStart",    1, 1, str_trim_start;
    StrTrimEnd,      BuiltinKind::Method, "trimEnd",      1, 1, str_trim_end;
    StrCharAt,       BuiltinKind::Method, "charAt",       2, 2, str_char_at;
    StrAt,           BuiltinKind::Method, "at",           2, 2, at_poly;
    StrConcat,       BuiltinKind::Method, "concat",       1, VARARG, concat_poly;
}

// ── argument accessor ────────────────────────────────────────────────────────

/// Zero-cost argument handle: a short-lived borrow token that reads arguments
/// in-place on the stack. `Copy` so handlers can pass it by value.
///
/// An absent argument (index ≥ argc) yields `&Value::Undefined`, matching JS
/// semantics. Handlers apply JS-level defaults for optional args (e.g. `join`
/// separator → `","`, `slice` end → length) themselves.
#[derive(Clone, Copy)]
struct Args {
    /// Index of arg 0 in `vm.stack` (the deepest).
    base: usize,
    /// Number of arguments present.
    argc: usize,
}

impl Args {
    /// Arg `i`, or `&Value::Undefined` if absent. Zero-cost — no clone.
    fn get<'a>(&self, vm: &'a VM, i: usize) -> &'a Value {
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
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{self, run_instrs};
    use crate::vm::{Instr, StepResult};

    // ── first-class value tests ────────────────────────────────────────

    #[test]
    fn builtin_as_first_class_value_via_calldyn() {
        let out = run_instrs(vec![
            Instr::PushFloat(2.0),
            Instr::PushFloat(7.0),
            Instr::PushBuiltin(Builtin::MathMax),
            Instr::CallDyn(2),
        ]);
        assert_eq!(out, vec![Value::Float(7.0)]);
    }

    #[test]
    fn builtin_value_shape() {
        let mut vm = VM::new(vec![Instr::PushBuiltin(Builtin::MathMax), Instr::TypeOf]);
        while !matches!(vm.step().unwrap(), StepResult::Done { .. }) {}
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
            match vm.step() {
                Err(e) => break e,
                Ok(StepResult::Done { .. }) => panic!("expected error"),
                Ok(_) => {}
            }
        };
        assert_eq!(err.kind, ErrorKind::TypeError);
        // Stack has operands consumed.
        vm.resume_with(&err, Value::PosInt(99)).unwrap();
        loop {
            match vm.step().unwrap() {
                StepResult::Done { value } => {
                    assert_eq!(value, Value::PosInt(99));
                    break;
                }
                _ => {}
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
}
