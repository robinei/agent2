use crate::builtin::Args;
use crate::vm::{VM, VMError, Value};

// ── console implementations ──────────────────────────────────────────────────

/// Maximum number of lines in the console buffer.
///
/// **Lines, and it means it.** Until 2026-09-19 the buffer held one
/// entry per `console.log` *call*, so this capped calls: a single
/// `console.log(file)` counted as one against 256 and a real
/// `sweep-200` entry held 323 newlines in 4,094 bytes. Every bound
/// downstream inherited the lie — the report's line count could be off
/// by a factor of a hundred. Now one line is one entry, so the cap
/// here, the report's tail, and `history.fetch`'s array all mean
/// lines.
///
/// Raised with the split, because a file dump that used one slot now
/// uses hundreds and must not evict the run's own earlier output.
/// Matches `report::CONSOLE_MAX_LINES`, the next bound out, so the two
/// no longer disagree about what they are counting.
const CONSOLE_CAP: usize = 2_000;
/// Maximum bytes per line; longer lines are truncated with a trailing `…`.
///
/// **The same number as `report::CONSOLE_SECTION_MAX_BYTES`, and they
/// touch.** A capped line is exactly this many bytes, and the report's
/// newest-first walk charges a line its length *plus its newline* — so
/// a capped line overshot the section budget by one and the walk
/// stopped before taking anything. `console.log` of any file, outline
/// or grep dump over 4KB rendered "The last 0 of N lines" above an
/// empty fence: the whole point of the call, withheld, announced as a
/// clip of nothing. The report now keeps one line whatever its size;
/// `a_capped_line_fits_the_report_section_exactly` pins the equality
/// so that the two bounds cannot drift apart unnoticed.
pub(crate) const CONSOLE_LINE_CAP: usize = 4096;

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
    let full = if prefix.is_empty() {
        line
    } else {
        format!("{prefix}{line}")
    };
    // **One line per entry.** A call's output is split here rather than
    // stored whole, so `console_lines` is what its name says and every
    // bound over it counts the same thing the reader does. A trailing
    // newline ends the last line; it does not start an empty one.
    let text = full.strip_suffix('\n').unwrap_or(&full);
    for line in text.split('\n') {
        let mut line = line.to_owned();
        if line.len() > CONSOLE_LINE_CAP {
            line.truncate(CONSOLE_LINE_CAP - 3);
            line.push('…');
        }
        // Ring-buffer logic.
        if vm.console_lines.len() >= CONSOLE_CAP {
            let dropped = vm.console_lines.len() - CONSOLE_CAP + 1;
            vm.console_lines.drain(0..dropped);
            vm.console_lines
                .push(format!("[… {dropped} lines dropped]"));
        }
        vm.console_lines.push(line);
    }
    Ok(Value::Undefined)
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod line_split_tests {
    use crate::testutil;

    fn console(src: &str) -> Vec<String> {
        testutil::run_console(src)
    }

    /// **One line per entry.** A call that prints four lines makes four
    /// entries, so the cap here, the report's tail and
    /// `history.fetch`'s array all count what the reader counts.
    #[test]
    fn one_call_printing_many_lines_stores_many_lines() {
        assert_eq!(
            console(r#"console.log("a\nb\nc\nd");"#),
            vec!["a", "b", "c", "d"]
        );
    }

    /// A trailing newline ends the last line rather than starting an
    /// empty one — `ls` output would otherwise gain a blank entry, and
    /// that blank was the stray line under `### it printed`.
    #[test]
    fn a_trailing_newline_does_not_add_an_empty_line() {
        assert_eq!(console(r#"console.log("a\nb\n");"#), vec!["a", "b"]);
    }

    /// Blank lines *inside* the output are the program's own and stay.
    #[test]
    fn a_blank_line_inside_the_output_survives() {
        assert_eq!(console(r#"console.log("a\n\nb");"#), vec!["a", "", "b"]);
    }

    /// What the per-line cap actually produces, in bytes — the next
    /// bound out (`report::CONSOLE_SECTION_MAX_BYTES`) is the same
    /// number, so whether a capped line fits inside it or blows it by
    /// one is the difference between the reader seeing the line and
    /// seeing an empty fence.
    #[test]
    fn a_capped_line_fits_the_report_section_exactly() {
        let out = console(&format!(r#"console.log("{}");"#, "z".repeat(9000)));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].len(),
            crate::builtin::console::CONSOLE_LINE_CAP,
            "capped length"
        );
    }

    /// Each call still starts its own line, so two calls never merge.
    #[test]
    fn separate_calls_stay_separate_lines() {
        assert_eq!(
            console(r#"console.log("a"); console.log("b");"#),
            vec!["a", "b"]
        );
    }
}

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
            if let crate::vm::StepResult::Done { .. } = vm.step(u64::MAX).unwrap() {
                break;
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
        let prog = testutil::compile_ok("console.assert(false, 'bad', 42); return 1;");
        let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
        loop {
            if let crate::vm::StepResult::Done { .. } = vm.step(u64::MAX).unwrap() {
                break;
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
