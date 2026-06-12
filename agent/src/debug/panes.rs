//! Terminal-free data extraction for the debug panes (9_TUI Step 3):
//! disassembly window rows, stack rows, promise rows, value previews.
//! `ui.rs` only styles what these produce, so the pane content is
//! unit-testable without a terminal.

use interp::{PromiseState, VM, Value};

/// One row of the disassembly pane.
#[derive(Debug, PartialEq)]
pub struct AsmRow {
    pub text: String,
    /// The instruction at `vm.ip`.
    pub current: bool,
    /// A `── name ──` function header (block start), not an instruction.
    pub header: bool,
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
                rows.push(AsmRow {
                    text: format!("── {} ──", f.name),
                    current: false,
                    header: true,
                });
                cur_fn = Some(idx);
            }
        }
        rows.push(AsmRow {
            text: vm.disasm_line(ip),
            current: ip == vm.ip,
            header: false,
        });
    }
    rows
}

/// One row of the stack pane.
#[derive(Debug, PartialEq)]
pub struct StackRow {
    pub text: String,
    /// A frame header (`name  fp=N`), not a slot.
    pub header: bool,
}

/// Stack rows, innermost frame first: per frame a header, then named
/// locals with value previews, then the temp range.
pub fn stack_rows(vm: &VM) -> Vec<StackRow> {
    let mut rows = Vec::new();
    for f in vm.frames().iter().rev() {
        rows.push(StackRow {
            text: format!("{}  fp={}", f.name(), f.fp),
            header: true,
        });
        for (i, v) in f.locals.iter().enumerate() {
            let name = f
                .local_name(i)
                .map(str::to_string)
                .unwrap_or_else(|| format!("#{i}"));
            rows.push(StackRow {
                text: format!("  {name} = {}", preview(vm, v, 36)),
                header: false,
            });
        }
        if !f.temps.is_empty() {
            let temps: Vec<String> = f.temps.iter().map(|v| preview(vm, v, 18)).collect();
            rows.push(StackRow {
                text: format!("  ~ [{}]", temps.join(", ")),
                header: false,
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
        assert_eq!(rows.iter().filter(|r| r.current).count(), 1, "{rows:?}");
        assert!(rows.iter().any(|r| r.header), "{rows:?}");
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
                    rows.iter()
                        .any(|x| x.header && x.text.starts_with("<root>")),
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
