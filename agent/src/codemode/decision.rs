//! Reading a handler's returned value as a decision (phase 20 doc,
//! Part D "The decision is the handler's return value").
//!
//! `resume(value?)` and `abandon()` compile to a plain tagged object —
//! `{__decision: "resume", value}` / `{__decision: "abandon"}` — built
//! by `interp`'s compiler with no host round-trip at all (previous
//! commit). This module is the harness's side of that contract:
//! [`read`] takes whatever a handler's top-level completion actually
//! returned and says which decision, if any, it is.
//!
//! "Only a decision counts" (Step D2): a handler that returns nothing,
//! returns an ordinary value with no `__decision` tag, or falls off
//! the end, has decided nothing — [`read`] returns `None` uniformly
//! for all three, which is exactly the input a re-fired condition
//! needs ("no decision was made"), not a special case per shape.

use interp::{VM, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// `return resume(value)` — continue the raising program with
    /// `value` as what `raise()` evaluates to (or the result of the
    /// failed operation, for a trap). `Value::Null` for a bare
    /// `resume()` (Step D1: "`resume()` takes no value for a posted
    /// condition") — indistinguishable at this layer from an explicit
    /// `resume(null)`, which is fine: both mean "nothing to inject".
    Resume(serde_json::Value),
    /// `return abandon()` — discard the raising program; the mind is
    /// prompted for a replacement at that stack level next.
    Abandon,
}

/// Read a handler's completed program's return value as a [`Decision`],
/// or `None` if it is not one — the "no decision was made" case Step
/// D2 requires the condition to re-fire on.
pub fn read(vm: &VM, value: &Value) -> Option<Decision> {
    let json = vm.stack_value_to_json(value, 0).ok()?;
    let obj = json.as_object()?;
    match obj.get("__decision")?.as_str()? {
        "resume" => Some(Decision::Resume(
            obj.get("value").cloned().unwrap_or(serde_json::Value::Null),
        )),
        "abandon" => Some(Decision::Abandon),
        // An object that merely happens to have a `__decision` key
        // with some other value — not a decision this compiler ever
        // produces, so not one `read` will recognize either.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interp::compile;

    fn run_to_value(source: &str) -> (VM, Value) {
        let prog = compile(source).unwrap_or_else(|e| panic!("compile error: {e:?}"));
        let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
        match vm.step(u64::MAX).unwrap() {
            interp::StepResult::Done { value, .. } => (vm, value),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn resume_with_a_value_reads_back_as_resume() {
        let (vm, v) = run_to_value("return resume(42);");
        assert_eq!(read(&vm, &v), Some(Decision::Resume(serde_json::json!(42))));
    }

    #[test]
    fn resume_with_no_argument_reads_back_as_resume_null() {
        let (vm, v) = run_to_value("return resume();");
        assert_eq!(
            read(&vm, &v),
            Some(Decision::Resume(serde_json::Value::Null))
        );
    }

    #[test]
    fn abandon_reads_back_as_abandon() {
        let (vm, v) = run_to_value("return abandon();");
        assert_eq!(read(&vm, &v), Some(Decision::Abandon));
    }

    #[test]
    fn a_plain_value_is_not_a_decision() {
        for source in ["return 42;", "return 'resume';", "return { ok: true };"] {
            let (vm, v) = run_to_value(source);
            assert_eq!(read(&vm, &v), None, "source: {source}");
        }
    }

    #[test]
    fn falling_off_the_end_returns_undefined_which_is_not_a_decision() {
        let (vm, v) = run_to_value("1 + 1;");
        assert_eq!(read(&vm, &v), None);
    }

    #[test]
    fn an_object_with_an_unrecognized_decision_tag_is_not_a_decision() {
        // Nothing this compiler produces has a `__decision` other than
        // "resume"/"abandon", but a handler could construct one by
        // hand — still not recognized, matching "only a decision
        // counts", not "only a tagged-looking object counts".
        let (vm, v) = run_to_value("return { __decision: 'retry' };");
        assert_eq!(read(&vm, &v), None);
    }

    #[test]
    fn misuse_is_inert_not_wrong() {
        // Step D2: "`resume(v)` on its own line has no effect" —
        // calling it without `return` produces an ordinary, unused
        // object value; the *actual* completion value is whatever the
        // program separately returns (or falls off the end with).
        let (vm, v) = run_to_value("resume(42); 1 + 1;");
        assert_eq!(read(&vm, &v), None);
    }
}
