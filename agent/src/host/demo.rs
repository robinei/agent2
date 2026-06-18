//! The M0 demo (8_HARNESS milestones): a scripted-LLM session you can
//! actually run — user turn → program → tool fan-out → results →
//! completion report → final text — shared verbatim by the
//! `agent session` CLI and the end-to-end test.

use std::io;
use std::sync::mpsc::Sender;

use serde_json::json;

use super::{
    ScriptedLlm, Session, SessionCommand, SessionEvent, ToolDef, ToolRegistry, scripted_program,
    scripted_text,
};
use crate::types::Tree;

pub const DEMO_PROMPT: &str =
    "You are the M0 scripted demo agent. You solve tasks by writing programs.";

const DEMO_SOURCE: &str = r#"
const a = tools.echo("alpha");
const b = tools.echo("beta");
console.log("fanned out: two echo calls in flight");
return [await a, await b];
"#;

pub fn demo_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(ToolDef {
        name: "echo".into(),
        description: "Returns its argument unchanged.".into(),
        input_schema: json!({
            "type": "array",
            "items": [{ "description": "the value to echo" }]
        }),
        handler: Box::new(|args| Ok(args.get(0).cloned().unwrap_or(serde_json::Value::Null))),
    });
    registry
}

pub fn demo_script() -> ScriptedLlm {
    ScriptedLlm::new([
        scripted_program("demo-1", DEMO_SOURCE),
        scripted_text("Round trip complete: the fan-out returned alpha and beta."),
    ])
}

/// Build the demo session over `tree`, queue the user turn, and run it
/// to completion. Events stream to `events` as it goes.
pub fn run_demo(tree: Tree, events: Sender<SessionEvent>) -> io::Result<Session> {
    let session = Session::new(
        tree,
        DEMO_PROMPT,
        json!(null),
        demo_registry(),
        Box::new(demo_script()),
        events,
    )?;
    session.handle().send(SessionCommand::UserTurn(
        "Demonstrate a tool fan-out and report what came back.".into(),
    ));
    Ok(session.run())
}
