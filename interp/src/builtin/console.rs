use crate::builtin::Args;
use crate::vm::{VM, VMError, Value};

// ── console implementations ──────────────────────────────────────────────────

/// Maximum number of lines in the console buffer.
const CONSOLE_CAP: usize = 256;
/// Maximum bytes per line; longer lines are truncated with a trailing `…`.
const CONSOLE_LINE_CAP: usize = 4096;

pub fn console_log(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    console_write(vm, args, "")
}

pub fn console_warn(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    console_write(vm, args, "[warn] ")
}

pub fn console_error(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    console_write(vm, args, "[error] ")
}

pub fn console_info(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    console_write(vm, args, "")
}

pub fn console_assert(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    if args.argc == 0 {
        return Ok(Value::Undefined);
    }
    let cond = args.get(vm, 0);
    if cond.is_truthy() {
        return Ok(Value::Undefined);
    }
    let msg_args = Args {
        base: args.base + 1,
        argc: args.argc.saturating_sub(1),
    };
    console_write(vm, msg_args, "Assertion failed: ")
}

fn console_write(vm: &mut VM, args: Args, prefix: &str) -> Result<Value, VMError> {
    let mut line = String::new();
    for i in 0..args.argc {
        if i > 0 {
            line.push(' ');
        }
        let s = match args.get(vm, i) {
            Value::String(s) => s.as_str().to_owned(),
            other => match vm.stack_value_to_json(other, 2) {
                Ok(serde_json::Value::String(s)) => s,
                Ok(j) => serde_json::to_string(&j).unwrap_or_default(),
                Err(_) => "[unserializable]".to_string(),
            },
        };
        line.push_str(&s);
    }
    // Truncate long lines.
    if line.len() > CONSOLE_LINE_CAP {
        line.truncate(CONSOLE_LINE_CAP - 3);
        line.push('…');
    }
    let full = if prefix.is_empty() {
        line
    } else {
        format!("{prefix}{line}")
    };
    // Ring-buffer logic.
    if vm.console_lines.len() >= CONSOLE_CAP {
        let dropped = vm.console_lines.len() - CONSOLE_CAP + 1;
        vm.console_lines.drain(0..dropped);
        vm.console_lines
            .push(format!("[… {dropped} lines dropped]"));
    }
    vm.console_lines.push(full);
    Ok(Value::Undefined)
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{
        VM, Value,
        builtin::Builtin,
        testutil::{self, run_instrs},
        vm::Instr,
    };

    // ── Step 4a: console tests ────────────────────────────────────────

    #[test]
    fn console_log_formats_args() {
        let out = run_instrs(vec![
            Instr::PushStr("a".into()),
            Instr::PushPosInt(1),
            Instr::PushStr("[2]".into()),
            Instr::CallBuiltin(Builtin::ConsoleLog, 3),
        ]);
        // Returns undefined.
        assert_eq!(out, vec![Value::Undefined]);
    }

    #[test]
    fn console_buffer_readable_after_run() {
        let prog =
            testutil::compile_ok("console.log('hello', 42); console.warn('oops'); return 1;");
        let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
        loop {
            match vm.step(u64::MAX).unwrap() {
                crate::vm::StepResult::Done { .. } => break,
                _ => {}
            }
        }
        // console_lines should have two entries.
        let lines = &vm.console_lines;
        assert_eq!(lines.len(), 2, "got: {lines:?}");
        assert!(
            lines[0].contains("hello") && lines[0].contains("42"),
            "line 0: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("[warn]") && lines[1].contains("oops"),
            "line 1: {}",
            lines[1]
        );
    }

    #[test]
    fn console_assert_passes_silently() {
        let out = testutil::run_ret("console.assert(true, 'nope'); return 1;");
        assert_eq!(out, serde_json::json!(1));
    }

    #[test]
    fn console_assert_fails_writes_error() {
        let prog = testutil::compile_ok(
            "console.assert(false, 'bad', 42); return 1;",
        );
        let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
        loop {
            match vm.step(u64::MAX).unwrap() {
                crate::vm::StepResult::Done { .. } => break,
                _ => {}
            }
        }
        assert_eq!(vm.console_lines.len(), 1);
        assert!(
            vm.console_lines[0].contains("Assertion failed")
                && vm.console_lines[0].contains("bad")
                && vm.console_lines[0].contains("42"),
            "line: {}",
            vm.console_lines[0]
        );
    }
}
