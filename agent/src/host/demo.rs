//! The M0 demo (8_HARNESS milestones): a scripted-LLM session you can
//! actually run — user turn → program → tool fan-out → results →
//! completion report → final text — shared verbatim by the
//! `agent session` CLI and the end-to-end test.

use std::io;
use std::sync::mpsc::Sender;

use serde_json::json;

use super::{
    ScriptedLlm, Session, SessionCommand, SessionEvent, ToolDef, ToolRegistry, scripted_program,
};
use crate::types::Tree;

pub const DEMO_PROMPT: &str =
    "You are the M0 scripted demo agent. You solve tasks by writing programs.";

const DEMO_SOURCE: &str = r#"
const a = tools.echo("alpha");
const b = tools.echo("beta");
console.log("fanned out: two echo calls in flight");
history.append([await a, await b]);
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
        guidelines: Vec::new(),
        example: None,
        returns: None,
        handler: Box::new(|args| Ok(args.get(0).cloned().unwrap_or(serde_json::Value::Null))),
    });
    registry
}

/// One program, one round trip. `finish_program`'s own doc explains why
/// there is no second scripted turn to consume: a completion with
/// nothing left unaccounted for (no unseen post, nothing pending) does
/// not manufacture a reason to prompt again — a holdover from the old
/// tool-calling protocol, where a `tool_result` always needed a
/// follow-up completion by the chat API's own rules, would have shown
/// up here as a second, unconsumed `scripted_text` the demo never
/// actually reaches.
pub fn demo_script() -> ScriptedLlm {
    ScriptedLlm::new([scripted_program(DEMO_SOURCE)])
}

/// Build the demo session over `tree`, queue the user turn, and run it
/// to completion. Events stream to `events` as it goes.
pub fn run_demo(tree: Tree, events: Sender<SessionEvent>) -> io::Result<Session> {
    let session = Session::new(
        tree,
        DEMO_PROMPT,
        demo_registry(),
        Box::new(demo_script()),
        events,
    )?;
    session.handle().send(SessionCommand::UserTurn {
        branch: session.conversation_branch(),
        text: "Demonstrate a tool fan-out and report what came back.".into(),
        expects_reply: true,
    });
    Ok(session.run())
}
