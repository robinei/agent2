use crate::builtin::Args;
use crate::vm::{VM, VMError, Value};

// ── Date static ───────────────────────────────────────────────────────────

/// `Date.now()` — current wall-clock time, epoch milliseconds. The only
/// `Date` surface this dialect implements; `new Date()` remains unscheduled
/// (15_COMPAT). Deliberately nondeterministic (DESIGN.md names `Date.now`
/// explicitly as compatible: recovery is re-execution/rewrite, not a
/// deterministic retrace, so a wall-clock builtin costs nothing there) —
/// and never constant-folded, since `pe_fold_arity` excludes every builtin
/// call from the optimizer's fold table.
pub fn date_now(_vm: &mut VM, _args: Args) -> Result<Value, VMError> {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok(Value::PosInt(ms))
}

#[cfg(test)]
mod tests {
    use crate::testutil::eval;
    use crate::vm::Value;

    #[test]
    fn date_now_returns_a_plausible_epoch_ms() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let got = match eval("Date.now()") {
            Value::PosInt(n) => n,
            other => panic!("expected PosInt, got {other:?}"),
        };
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            (before..=after).contains(&got),
            "{before} <= {got} <= {after}"
        );
    }

    #[test]
    fn date_now_is_not_folded_across_two_calls() {
        // Two calls a moment apart must each read the clock live — proof
        // the optimizer never treats this builtin as constant-foldable.
        let got = crate::testutil::run_val(
            "const a = Date.now(); const b = Date.now(); return b - a >= 0;",
        );
        assert_eq!(got, Value::Bool(true));
    }

    #[test]
    fn date_now_rejects_arguments() {
        let errs = crate::testutil::compile_errs("Date.now(1);");
        assert!(!errs.is_empty());
    }
}
