//! Parsing the bare-global harness vocabulary's `Invoke` calls into
//! typed effects (phase 20 doc, Step C1).
//!
//! `interp`'s compiler (`interp/src/compiler/call.rs`) now compiles `say`,
//! `ask`, `answer`, `spawn`, `fork`, `append_history`, and `artifact`
//! to `Instr::Invoke(name, argc)` — the same effect `tools.foo(...)`
//! already produces, arity-agnostic at the compiler level exactly as
//! `tools.*` is. This module is the next layer down: turning one
//! `InvokeCall` (a name plus raw `Value` args) into a validated
//! [`HarnessEffect`], the same way `machine.rs`'s existing dispatch
//! turns a `tools.spawn(...)` call into a `Call::Spawn`. It does not
//! touch `machine.rs` or the event log — nothing here is wired into a
//! running session yet (see the module's own doc in `mod.rs`).
//!
//! A malformed call (wrong arity, wrong argument shape) is a parse
//! error the caller rejects in place — the same "reject_call" pattern
//! `machine.rs` already uses for a malformed `tools.*` call: the
//! program's own `try`/`catch` is the first line of defense
//! (6_LANGUAGE Part B), and only an uncaught one traps into a
//! condition.

use interp::{InvokeCall, VM};

use super::entry::EntryId;
use crate::types::EventId;

/// One resolved harness-verb call, ready for a (future) host
/// dispatcher to act on. Field shapes are this module's own concrete
/// choice filling gaps the plan doc leaves at the level of "the verb
/// exists" rather than "here is its exact arity" — documented per
/// verb below, easily revised once a real dispatcher exists to press
/// against.
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessEffect {
    /// `say(text)` or `say(to, text)` — a tell: appends and delivers,
    /// resolves nothing (Step C1). `to: None` means the implicit
    /// target `18_TARGETING` already defines for an omitted address —
    /// whoever is owed a reply, or the user on a root branch.
    Say { to: Option<String>, text: String },
    /// `ask(who, text)` → a value once answered. Always two
    /// arguments in this doc's own examples, unlike `say`'s two
    /// forms — kept that way here rather than inventing a `who`-less
    /// shorthand the doc never shows.
    Ask { who: Option<String>, text: String },
    /// `answer(question, label, value)` — discharges an inbound
    /// question. Three arguments, matching Step C1's own worked
    /// example (`answer(7, "ask from planner", {…})`) rather than the
    /// two-argument `answer(question, value)` that appears earlier in
    /// the same file before the label checksum was added — the
    /// worked example is the more specific, later statement, so it is
    /// the one implemented. `label` is checked the same way
    /// `compaction.rs`'s `CompactionOp` checks it: it must match the
    /// question entry's own label, or the call is rejected — not
    /// implemented at this layer (no log to check against yet), but
    /// the shape is carried through so a dispatcher can.
    Answer {
        question: EntryId,
        label: String,
        value: serde_json::Value,
    },
    /// `spawn(charter)` — one required argument, the new agent's
    /// charter. Step C1 does not describe a second argument (a tool
    /// allowlist, say) for the bare form the way `tools.agent` might
    /// have taken one; kept to exactly what the doc shows.
    Spawn { charter: String },
    /// `fork()` — no arguments; inherits the calling context's whole
    /// history (Step C3).
    Fork,
    /// `append_history(value)` — one argument, any JSON value. The
    /// card's own guidance (Step C4) is to append a *projection*, not
    /// a raw result, but that is a prompting concern, not something
    /// this parsing layer can or should enforce.
    AppendHistory { value: serde_json::Value },
    /// `artifact(id)` — one argument, a positive integer entry id.
    /// Single-argument, no label: Step C1 describes it as a plain
    /// fetch-by-id (DESIGN.md's `tools.tool_result`, renamed), and
    /// unlike `answer`/`remove_history`/`rewrite_history` its worked
    /// examples never show a second argument.
    Artifact { id: EntryId },
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerbError(pub String);

impl std::fmt::Display for VerbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn err(msg: impl Into<String>) -> VerbError {
    VerbError(msg.into())
}

/// A positive integer id from an untrusted JSON argument — never
/// `EventId::new` directly on unchecked input, which panics on zero
/// (`types.rs`). A malformed id is a parse error the call is rejected
/// for, not a crash.
fn entry_id_arg(v: &serde_json::Value, which: &str) -> Result<EntryId, VerbError> {
    let n = v
        .as_u64()
        .ok_or_else(|| err(format!("{which} must be a positive integer id")))?;
    if n == 0 {
        return Err(err(format!("{which} must be a positive integer id, got 0")));
    }
    Ok(EventId::new(n))
}

fn string_arg(v: &serde_json::Value, which: &str) -> Result<String, VerbError> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| err(format!("{which} must be a string")))
}

/// `None`/absent stays `None`; anything else must be a string. Used
/// for the omittable address arguments (`say`'s `to`, `ask`'s `who`).
fn optional_string_arg(
    v: Option<&serde_json::Value>,
    which: &str,
) -> Result<Option<String>, VerbError> {
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(other) => string_arg(other, which).map(Some),
    }
}

/// Convert every argument of `call` to JSON up front — the uniform
/// representation every parser below works from, matching
/// `machine.rs`'s existing `value_json` bridge for the same VM
/// `Value` type.
fn args_as_json(vm: &VM, call: &InvokeCall) -> Vec<serde_json::Value> {
    call.args
        .iter()
        .map(|v| {
            vm.stack_value_to_json(v, 0)
                .unwrap_or_else(|_| serde_json::Value::String(format!("{v:?}")))
        })
        .collect()
}

/// Parse one `InvokeCall` produced by the bare harness vocabulary into
/// a [`HarnessEffect`]. `Err` for a call whose `name` is not one of
/// the seven verbs, or whose arguments don't match the chosen shape.
pub fn parse_effect(vm: &VM, call: &InvokeCall) -> Result<HarnessEffect, VerbError> {
    let args = args_as_json(vm, call);
    match call.name.as_str() {
        "say" => match args.as_slice() {
            [text] => Ok(HarnessEffect::Say {
                to: None,
                text: string_arg(text, "say's text")?,
            }),
            [to, text] => Ok(HarnessEffect::Say {
                to: optional_string_arg(Some(to), "say's `to`")?,
                text: string_arg(text, "say's text")?,
            }),
            _ => Err(err("say(text) or say(to, text)")),
        },
        "ask" => match args.as_slice() {
            [who, text] => Ok(HarnessEffect::Ask {
                who: optional_string_arg(Some(who), "ask's `who`")?,
                text: string_arg(text, "ask's text")?,
            }),
            _ => Err(err("ask(who, text)")),
        },
        "answer" => match args.as_slice() {
            [question, label, value] => Ok(HarnessEffect::Answer {
                question: entry_id_arg(question, "answer's question id")?,
                label: string_arg(label, "answer's label")?,
                value: value.clone(),
            }),
            _ => Err(err("answer(question, label, value)")),
        },
        "spawn" => match args.as_slice() {
            [charter] => Ok(HarnessEffect::Spawn {
                charter: string_arg(charter, "spawn's charter")?,
            }),
            _ => Err(err("spawn(charter)")),
        },
        "fork" => match args.as_slice() {
            [] => Ok(HarnessEffect::Fork),
            _ => Err(err("fork() takes no arguments")),
        },
        "append_history" => match args.as_slice() {
            [value] => Ok(HarnessEffect::AppendHistory {
                value: value.clone(),
            }),
            _ => Err(err("append_history(value)")),
        },
        "artifact" => match args.as_slice() {
            [id] => Ok(HarnessEffect::Artifact {
                id: entry_id_arg(id, "artifact's id")?,
            }),
            _ => Err(err("artifact(id)")),
        },
        other => Err(err(format!("not a harness verb: `{other}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interp::compile;

    /// Compile+run `source` to its first effect and hand back the
    /// `(vm, call)` pair `parse_effect` takes — the harness verb under
    /// test is always the sole, awaited call.
    fn first_call(source: &str) -> (VM, InvokeCall) {
        let prog = compile(source).unwrap_or_else(|e| panic!("compile error: {e:?}"));
        let mut vm = VM::for_program(prog, serde_json::Value::Null).unwrap();
        match vm.step(u64::MAX).unwrap() {
            interp::StepResult::Pending { mut calls } => {
                assert_eq!(calls.len(), 1, "expected exactly one call");
                (vm, calls.remove(0))
            }
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn say_with_one_argument_has_no_explicit_target() {
        let (vm, call) = first_call("return await say('hi');");
        assert_eq!(
            parse_effect(&vm, &call).unwrap(),
            HarnessEffect::Say {
                to: None,
                text: "hi".into(),
            }
        );
    }

    #[test]
    fn say_with_two_arguments_names_the_target() {
        let (vm, call) = first_call("return await say('robin', 'hi');");
        assert_eq!(
            parse_effect(&vm, &call).unwrap(),
            HarnessEffect::Say {
                to: Some("robin".into()),
                text: "hi".into(),
            }
        );
    }

    #[test]
    fn say_with_wrong_arity_is_rejected() {
        let (vm, call) = first_call("return await say();");
        assert!(parse_effect(&vm, &call).is_err());
    }

    #[test]
    fn ask_requires_exactly_two_arguments() {
        let (vm, call) = first_call("return await ask('robin', 'q?');");
        assert_eq!(
            parse_effect(&vm, &call).unwrap(),
            HarnessEffect::Ask {
                who: Some("robin".into()),
                text: "q?".into(),
            }
        );
        let (vm, call) = first_call("return await ask('q?');");
        assert!(parse_effect(&vm, &call).is_err());
    }

    #[test]
    fn ask_with_null_who_is_the_implicit_target() {
        let (vm, call) = first_call("return await ask(null, 'q?');");
        assert_eq!(
            parse_effect(&vm, &call).unwrap(),
            HarnessEffect::Ask {
                who: None,
                text: "q?".into(),
            }
        );
    }

    #[test]
    fn answer_takes_id_label_and_value() {
        let (vm, call) = first_call("return await answer(7, 'ask from planner', 42);");
        assert_eq!(
            parse_effect(&vm, &call).unwrap(),
            HarnessEffect::Answer {
                question: EventId::new(7),
                label: "ask from planner".into(),
                value: serde_json::json!(42),
            }
        );
    }

    #[test]
    fn answer_with_a_zero_id_is_rejected_not_a_panic() {
        // `EventId::new(0)` panics — the parser must catch this before
        // ever constructing one.
        let (vm, call) = first_call("return await answer(0, 'x', 1);");
        assert!(parse_effect(&vm, &call).is_err());
    }

    #[test]
    fn answer_with_only_two_arguments_is_rejected() {
        // The label checksum is required, not optional — a two-arg
        // `answer(question, value)` no longer matches.
        let (vm, call) = first_call("return await answer(7, 42);");
        assert!(parse_effect(&vm, &call).is_err());
    }

    #[test]
    fn spawn_takes_a_charter_string() {
        let (vm, call) = first_call("return await spawn('reviewer');");
        assert_eq!(
            parse_effect(&vm, &call).unwrap(),
            HarnessEffect::Spawn {
                charter: "reviewer".into(),
            }
        );
    }

    #[test]
    fn fork_takes_no_arguments() {
        let (vm, call) = first_call("return await fork();");
        assert_eq!(parse_effect(&vm, &call).unwrap(), HarnessEffect::Fork);
        let (vm, call) = first_call("return await fork(1);");
        assert!(parse_effect(&vm, &call).is_err());
    }

    #[test]
    fn append_history_takes_any_one_json_value() {
        let (vm, call) = first_call("return await append_history({ rows: 4 });");
        assert_eq!(
            parse_effect(&vm, &call).unwrap(),
            HarnessEffect::AppendHistory {
                value: serde_json::json!({ "rows": 4 }),
            }
        );
    }

    #[test]
    fn artifact_takes_a_positive_integer_id() {
        let (vm, call) = first_call("return await artifact(51);");
        assert_eq!(
            parse_effect(&vm, &call).unwrap(),
            HarnessEffect::Artifact {
                id: EventId::new(51),
            }
        );
    }

    #[test]
    fn artifact_with_a_zero_id_is_rejected_not_a_panic() {
        let (vm, call) = first_call("return await artifact(0);");
        assert!(parse_effect(&vm, &call).is_err());
    }

    #[test]
    fn artifact_with_a_non_numeric_id_is_rejected() {
        let (vm, call) = first_call("return await artifact('nope');");
        assert!(parse_effect(&vm, &call).is_err());
    }

    #[test]
    fn a_non_harness_call_name_is_rejected() {
        // `tools.*` calls go through this parser's caller only for
        // the bare vocabulary; a defensive check all the same.
        let (vm, call) = first_call("return await tools.read_file('x');");
        assert!(parse_effect(&vm, &call).is_err());
    }
}
