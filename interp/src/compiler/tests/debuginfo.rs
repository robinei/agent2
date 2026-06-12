//! 9_TUI Step 1 — the debug table (function names, source ranges, slot
//! names) and the VM's introspection accessors (`function_at`, `frames`,
//! `disasm`).

use crate::testutil::*;
use crate::vm::{StepResult, VM, Value};

#[test]
fn debug_table_function_and_slot_names() {
    let prog = compile_ok(
        r#"
        function add(a, b) { let sum = a + b; return sum; }
        const twice = (x) => x * 2;
        const fns = [function () { return 1; }];
        return add(1, 2) + twice(3) + fns.length;
        "#,
    );
    let names: Vec<&str> = prog
        .debug
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(names.contains(&"<root>"), "{names:?}");
    assert!(names.contains(&"add"), "{names:?}");
    assert!(names.contains(&"twice"), "{names:?}");
    assert!(names.contains(&"<anonymous>"), "{names:?}");

    let add = prog
        .debug
        .functions
        .iter()
        .find(|f| f.name == "add")
        .unwrap();
    // [params | upvals | own locals]: a, b, then sum.
    assert_eq!(add.slot_names[0].as_deref(), Some("a"));
    assert_eq!(add.slot_names[1].as_deref(), Some("b"));
    assert!(
        add.slot_names.iter().any(|n| n.as_deref() == Some("sum")),
        "{:?}",
        add.slot_names
    );
}

#[test]
fn frames_reports_names_and_locals_mid_call() {
    let prog = compile_ok(
        r#"
        function work(n) {
            let acc = n + 1;
            let spin = 0;
            for (let i = 0; i < 50; i++) { spin += 1; }
            return acc;
        }
        return work(41);
        "#,
    );
    let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    // Single-step until we observe `work`'s frame with `acc` initialized.
    let mut seen = false;
    loop {
        match vm.step(1).unwrap() {
            StepResult::OutOfFuel => {}
            StepResult::Done { value, .. } => {
                assert_eq!(value, Value::Float(42.0));
                break;
            }
            other => panic!("unexpected effect: {other:?}"),
        }
        let frames = vm.frames();
        if frames.len() == 2 && !seen {
            assert_eq!(frames[0].name(), "<root>");
            assert_eq!(frames[1].name(), "work");
            let top = &frames[1];
            if let Some(acc_slot) =
                (0..top.locals.len()).find(|&i| top.local_name(i) == Some("acc"))
            {
                if top.locals[acc_slot] == Value::Float(42.0) {
                    assert_eq!(top.local_name(0), Some("n"));
                    assert_eq!(top.locals[0], Value::PosInt(41));
                    seen = true;
                }
            }
        }
    }
    assert!(seen, "never observed `work`'s frame with `acc` set");
}

#[test]
fn disasm_headers_at_function_block_starts() {
    let prog = compile_ok(
        r#"
        function add(a, b) { return a + b; }
        return add(1, 2);
        "#,
    );
    let vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
    let asm = vm.disasm(0, vm.code.len() as u32);
    assert!(asm.contains("── add ──"), "{asm}");
    assert!(asm.contains("── <root> ──"), "{asm}");
    // Every instruction renders exactly one line; headers are extra.
    assert_eq!(
        asm.lines().filter(|l| !l.starts_with("──")).count(),
        vm.code.len(),
        "{asm}"
    );
}
