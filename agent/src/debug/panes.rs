//! Terminal-free data extraction for the debug panes (9_TUI Step 3):
//! disassembly window rows, stack rows, promise rows, value previews.
//! `ui.rs` only styles what these produce, so the pane content is
//! unit-testable without a terminal.

use interp::{PromiseState, VM, Value};

/// One row of the disassembly pane. Instruction text is synthesized in
/// parts (opcode / operands / source line) so the UI can color it without
/// any tokenization.
#[derive(Debug, PartialEq)]
pub enum AsmRow {
    /// A `── name ──` function header (block start).
    Header(String),
    Instr {
        ip: u32,
        op: String,
        /// The operand text inside the opcode's parentheses, if any.
        args: String,
        line: Option<usize>,
        /// The instruction at `vm.ip`.
        current: bool,
    },
}

/// A disassembly window of up to `height` rows centered on `vm.ip`, with
/// function-name headers wherever the owning function changes.
pub fn disasm_window(vm: &VM, height: usize) -> Vec<AsmRow> {
    if vm.code.is_empty() || height == 0 {
        return Vec::new();
    }
    let half = (height / 2) as i64;
    let start = (vm.ip as i64 - half).max(0) as usize;
    let end = (start + height).min(vm.code.len());
    let mut rows = Vec::with_capacity(end - start);
    let mut cur_fn = None;
    for ip in start as u32..end as u32 {
        if let Some((idx, f)) = vm.function_at(ip) {
            if cur_fn != Some(idx) {
                rows.push(AsmRow::Header(format!("── {} ──", f.name)));
                cur_fn = Some(idx);
            }
        }
        let debug = format!("{:?}", vm.code[ip as usize]);
        let (op, args) = match debug.split_once('(') {
            Some((op, rest)) => (
                op.to_string(),
                rest.strip_suffix(')').unwrap_or(rest).to_string(),
            ),
            None => (debug, String::new()),
        };
        let line = vm.spans.get(ip as usize).and_then(|&sp| {
            if vm.source.is_empty() {
                None
            } else {
                Some(interp::diag::line_col(&vm.source, sp).0)
            }
        });
        rows.push(AsmRow::Instr {
            ip,
            op,
            args,
            line,
            current: ip == vm.ip,
        });
    }
    rows
}

/// Opcode category for disasm coloring — purely synthesized from the
/// opcode name, no tokenization.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OpKind {
    /// Pushes a literal/constant.
    Push,
    /// Control flow: jumps, labels, return, frame entry.
    Control,
    /// Calls and host effects (tools, await, raise).
    Effect,
    Other,
}

pub fn op_kind(op: &str) -> OpKind {
    if op.starts_with("Push") {
        OpKind::Push
    } else if matches!(
        op,
        "Jump"
            | "JFalse"
            | "JTrue"
            | "JNotNullish"
            | "Return"
            | "EnterFrame"
            | "TryEnter"
            | "TryExit"
            | "Throw"
    ) {
        OpKind::Control
    } else if op.starts_with("Call") || matches!(op, "Invoke" | "Await" | "Raise" | "MakeClosure") {
        OpKind::Effect
    } else {
        OpKind::Other
    }
}

/// What a stack-pane row is, driving the region background: each frame
/// renders as a header + locals band, then a (lighter) temporaries band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StackRowKind {
    FrameHeader,
    Local,
    Temp,
}

/// One row of the stack pane.
#[derive(Debug, PartialEq)]
pub struct StackRow {
    pub text: String,
    pub kind: StackRowKind,
}

/// Stack rows, innermost frame first: per frame a header, then named
/// locals with value previews, then one row per expression temporary.
pub fn stack_rows(vm: &VM) -> Vec<StackRow> {
    let mut rows = Vec::new();
    for f in vm.frames().iter().rev() {
        rows.push(StackRow {
            text: format!("{}  fp={}", f.name(), f.fp),
            kind: StackRowKind::FrameHeader,
        });
        for (i, v) in f.locals.iter().enumerate() {
            let name = f
                .local_name(i)
                .map(str::to_string)
                .unwrap_or_else(|| format!("#{i}"));
            rows.push(StackRow {
                text: format!("  {name} = {}", preview(vm, v, 36)),
                kind: StackRowKind::Local,
            });
        }
        for v in f.temps {
            rows.push(StackRow {
                text: format!("  {}", preview(vm, v, 36)),
                kind: StackRowKind::Temp,
            });
        }
    }
    rows
}

/// Promise-pane rows: id, state, short value/waiter info, plus what the
/// runner still has in flight is rendered by the caller (it owns timers).
pub fn promise_rows(vm: &VM) -> Vec<String> {
    vm.promises
        .iter()
        .enumerate()
        .map(|(i, p)| match p {
            PromiseState::Pending { waiters } if waiters.is_empty() => {
                format!("#{i} pending")
            }
            PromiseState::Pending { waiters } => {
                format!("#{i} pending ({} waiting)", waiters.len())
            }
            PromiseState::Resolved(v) => format!("#{i} resolved {}", preview(vm, v, 28)),
            PromiseState::Rejected(v) => format!("#{i} rejected {}", preview(vm, v, 28)),
        })
        .collect()
}

/// A short single-line preview of a value, heap refs resolved through the
/// VM, truncated to ~`max` chars. Never fails: non-JSON values (promises,
/// functions) get a synthetic rendering.
pub fn preview(vm: &VM, v: &Value, max: usize) -> String {
    let s = match v {
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::PosInt(n) => n.to_string(),
        Value::NegInt(n) => n.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::String(s) => format!("{:?}", s.as_str()),
        // A boxed (captured) slot: dereference its cell.
        Value::Upval(c) => {
            return match vm.cells.get(*c as usize) {
                Some(inner) => format!("↑{}", preview(vm, inner, max)),
                None => "↑<bad cell>".to_string(),
            };
        }
        Value::Array(_) | Value::Object(_) => vm
            .stack_value_to_json(v, 0)
            .map(|j| j.to_string())
            .unwrap_or_else(|_| "<unrepresentable>".to_string()),
        Value::Fn(a) => format!("fn@{a}"),
        Value::Closure(p) => format!("closure#{p}"),
        Value::Builtin(b) => format!("{b:?}"),
        Value::Promise(p) => {
            let state = match vm.promises.get(*p as usize) {
                Some(PromiseState::Pending { .. }) => "pending",
                Some(PromiseState::Resolved(_)) => "resolved",
                Some(PromiseState::Rejected(_)) => "rejected",
                None => "?",
            };
            format!("Promise#{p}({state})")
        }
    };
    truncate(s, max)
}

fn truncate(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::runner::{RunState, Runner};

    fn paused_runner(src: &str) -> Runner {
        let prog = interp::compile(src).expect("compiles");
        Runner::new(prog, serde_json::Value::Null).expect("vm")
    }

    #[test]
    fn disasm_window_centers_one_current_row_with_headers() {
        let mut r = paused_runner(
            "function add(a, b) { return a + b; }\nlet t = add(1, 2);\nreturn t + add(3, 4);",
        );
        for _ in 0..4 {
            r.step_instr();
        }
        let rows = disasm_window(&r.vm, 9);
        assert!(rows.len() >= 5, "{rows:?}");
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, AsmRow::Instr { current: true, .. }))
                .count(),
            1,
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|r| matches!(r, AsmRow::Header(_))),
            "{rows:?}"
        );
        // Parts are synthesized: every instruction row has an opcode and a
        // source line, and opcodes categorize.
        let has_categorized_op = rows.iter().any(|r| {
            matches!(r, AsmRow::Instr { op, line: Some(_), .. } if op_kind(op) != OpKind::Other)
        });
        assert!(has_categorized_op, "{rows:?}");
    }

    #[test]
    fn stack_rows_show_named_locals_mid_call() {
        let mut r = paused_runner(
            "function work(n) { let acc = n + 1; let pad = acc * 2; return pad; }\nreturn work(41);",
        );
        // Step until `acc` is visible with its value in the top frame.
        for _ in 0..2000 {
            r.step_instr();
            if r.state != RunState::Paused {
                break;
            }
            let rows = stack_rows(&r.vm);
            if rows.first().map(|h| h.text.starts_with("work")) == Some(true)
                && rows.iter().any(|x| x.text.contains("acc = 42"))
            {
                assert!(rows.iter().any(|x| x.text.contains("n = 41")), "{rows:?}");
                assert!(
                    rows.iter().any(
                        |x| x.kind == StackRowKind::FrameHeader && x.text.starts_with("<root>")
                    ),
                    "{rows:?}"
                );
                return;
            }
        }
        panic!("never observed work's frame with acc set");
    }

    #[test]
    fn promise_rows_track_pending_then_resolved() {
        let mut r = paused_runner("const x = await tools.sleep(0); return x;");
        // Run until the sleep is parked (Waiting), then check the pane.
        r.state = RunState::Running;
        for _ in 0..100 {
            r.tick();
            if r.state == RunState::Waiting {
                break;
            }
        }
        assert!(
            promise_rows(&r.vm).iter().any(|x| x.contains("pending")),
            "{:?}",
            promise_rows(&r.vm)
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        r.poll_timers();
        assert!(
            promise_rows(&r.vm).iter().any(|x| x.contains("resolved")),
            "{:?}",
            promise_rows(&r.vm)
        );
    }

    #[test]
    fn preview_handles_heap_and_synthetic_values() {
        let mut r = paused_runner("const o = { a: [1, 2], s: 'x' }; return o;");
        r.state = RunState::Running;
        r.tick();
        match &r.state {
            RunState::Done { value } => assert!(value.contains("[1,2]"), "{value}"),
            other => panic!("{other:?}"),
        }
        let p = preview(&r.vm, &Value::Undefined, 20);
        assert_eq!(p, "undefined");
        let long = preview(&r.vm, &Value::String(interp::RcStr::from("abcdefghij")), 6);
        assert!(long.ends_with('…'), "{long}");
    }
}
