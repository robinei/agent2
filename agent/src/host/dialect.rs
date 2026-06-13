//! System-prompt dialect card (8_HARNESS Step 6).
//!
//! The generated-where-possible preamble every frame's system message
//! starts with: how to act, the program contract, restart semantics,
//! the distilled dialect divergences, and the tool list rendered from
//! the registry schemas. Deliberately short — the condition report
//! carries the per-incident detail.

use super::registry::ToolRegistry;
use crate::report::clip;

/// Per-tool clip for the rendered input schema.
const SCHEMA_MAX_BYTES: usize = 200;

const CARD_HEAD: &str = "\
You act by writing JavaScript programs. The harness compiles and runs \
each one and replies with a report (result or condition) as the tool \
result.

## acting
- run_program(source): submit a complete program — this is how you do \
everything (compute, call tools, orchestrate).
- A reply with *no* tool call ends the task; its text is your final \
answer.

## program contract
- `input` is a read-only const holding this frame's JSON input.
- End with a top-level `return <value>`; the value must be JSON-able \
(no functions or promises). Without `return` the result is undefined.
- `console.log(...)` is your diagnostic trace: it is quoted back in \
every report and survives failures (the return value does not).
- No state persists between programs. Reuse prior work via artifacts: \
reports list every completed call as `[#id] ...`, and \
`await tools.tool_result(id)` re-fetches one instantly from the log. \
`tool_result(id)` returns what call `#id` returned *then* — a record, \
not a re-run. If the world may have changed since, make a fresh call \
instead.

## status-result discipline
- Tool results can be large; they live in variables and the log, not \
your context — keep them there.
- `return` and `console.log` only small, status-shaped values. \
Oversized returns are rejected.

## editing files
- Read into a variable → locate structurally (`grep -n`, an outline, \
or a brace/dedent scan) → compute the exact span → `replace_file` with \
the `version` from the read → **verify in the same program** (re-grep / \
re-read / run the build via `bash`) and only `raise` on surprise.
- Author new content freely; locate with the smallest reliable handle \
(a short anchor or a computed span), never by reproducing a large block.
- `create_file(path, content)` for new files; `replace_file(path, \
expected_version, content)` passes the `version` from `read_file` — a \
changed-file condition means re-read and re-apply.
- `extractBlock(text, headIndex) -> { start, end }` is a pure helper \
recipe: brace-balance for `{}` languages, dedent for Python — \
whole-function replacement is computed, not retyped.

## tools
Call as `tools.<name>(args...)`. Every call returns a promise; `await` \
it. Calls started before awaiting run in parallel (`Promise.all` works).
- tools.tool_result(id) — re-fetch artifact [#id] from the log \
(instant, free).
- tools.agent({ prompt, input }) — delegate a subtask to a fresh \
subagent; resolves to its JSON result. It sees only what you pass it.
- Before repeating a call shown in the menu, read the call — if it \
wrote, sent, or deleted, it already happened; reuse its result with \
`tool_result(id)` instead of re-running. Pure reads are free to repeat.";

const CARD_TAIL: &str = "\
## conditions and restarts
- raise(name, payload) suspends the program and sends the payload to \
you as a condition report; it is not catchable in the program — \
conditions are addressed to you. Use it when you need a decision or \
missing information mid-program.
- A runtime error suspends the same way, with a diagnostic.
- You answer with a restart: resume(value) — execution continues with \
`value` as the result of the failed operation (or of the raise \
expression) — or run_program(source) — a rewrite in a fresh VM, with \
all prior artifacts still fetchable by id.

## dialect (JavaScript, with differences)
- No `this` or `class`. Use plain functions, closures, object literals, \
and factory functions.
- `new Error(msg)`, `new RegExp(...)`, `new Map()`, and `new Set()` are \
the allowed `new` forms. Absent: BigInt, labeled statements, \
getters/setters.
- try/catch/finally work as in JS for `throw` and runtime errors; \
`raise` is never catchable.
- Promises exist only as tool-call results: no `new Promise`, no \
`.then`/`.catch` — use `await`, `Promise.all`, `Promise.allSettled`. \
`async function`s are supported.
- `Map` and `Set` are available with standard methods: get/set/has/delete/ \
clear/keys/values/entries/forEach, plus `.size`.
- RegExp: `/pattern/flags`, `.test()`, `.exec()`. String `.replace()` / \
`.replaceAll()` support `$1`..`$9` group references.
- Strings are UTF-8 bytes: `.length` and all indices count bytes, not \
UTF-16 units.
- `<` `>` `<=` `>=` never coerce across types; objects/arrays never \
coerce to primitives (`[5] == 5` is false, `[] + 1` is an error).
- Calls are strict-arity only for builtins (`Math.max`, `s.split`, etc.); \
user functions may omit trailing arguments. Writing past an array's end \
errors (use `push`). Bitwise ops are 64-bit.";

/// Render the full card: static head, the registry's tools (sorted,
/// one line each), static tail.
pub fn dialect_card(registry: &ToolRegistry) -> String {
    let mut card = String::from(CARD_HEAD);
    let mut tools: Vec<_> = registry.iter().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    for def in tools {
        card.push_str(&format!(
            "\n- tools.{} — {} args schema: {}",
            def.name,
            def.description,
            clip(&def.input_schema.to_string(), SCHEMA_MAX_BYTES),
        ));
    }
    card.push_str("\n\n");
    card.push_str(CARD_TAIL);
    card
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::registry::ToolDef;
    use serde_json::json;

    /// Bound on the static (non-tool-list) card text — keep it short.
    const CARD_STATIC_MAX_BYTES: usize = 6144;

    fn registry_with_tools() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(ToolDef {
            name: "fetch_page".into(),
            description: "Fetch a URL and return its body text.".into(),
            input_schema: json!({ "type": "array", "items": [{ "type": "string" }] }),
            output_schema: json!({ "type": "string" }),
            handler: Box::new(|_| Ok(json!(null))),
        });
        registry.register(ToolDef {
            name: "send_email".into(),
            description: "Send an email.".into(),
            input_schema: json!({ "type": "array" }),
            output_schema: json!({}),
            handler: Box::new(|_| Ok(json!(null))),
        });
        registry
    }

    #[test]
    fn card_stays_short() {
        assert!(
            CARD_HEAD.len() + CARD_TAIL.len() <= CARD_STATIC_MAX_BYTES,
            "static card text grew past {CARD_STATIC_MAX_BYTES} bytes — \
             the card must stay short; move detail into the reports"
        );
    }

    #[test]
    fn tool_list_is_generated_from_schemas() {
        let card = dialect_card(&registry_with_tools());
        // One line per tool, description + input schema consumed.
        let fetch_line = card
            .lines()
            .find(|l| l.starts_with("- tools.fetch_page"))
            .expect("a fetch_page line");
        assert!(fetch_line.contains("Fetch a URL and return its body text."));
        assert!(fetch_line.contains(r#"{"type":"array","items":[{"type":"string"}]}"#));
        let email_line = card
            .lines()
            .find(|l| l.starts_with("- tools.send_email"))
            .expect("a send_email line");
        assert!(email_line.contains("Send an email."));
        // No effectful flag rendering — the flag is gone (10_EDITING Step 6).
        assert!(!email_line.contains("effectful"));
        assert!(!fetch_line.contains("effectful"));
    }

    #[test]
    fn card_covers_the_contract() {
        let card = dialect_card(&ToolRegistry::new());
        for needle in [
            "run_program(source)",
            "`input` is a read-only const",
            "top-level `return <value>`",
            "console.log",
            "tools.tool_result(id)",
            "tools.agent({ prompt, input })",
            "raise(name, payload)",
            "resume(value)",
            "No `this` or `class`",
            "`new Map()`",
            "`Map` and `Set`",
            "group references",
            "UTF-8 bytes",
            // Step 5 additions:
            "editing files",
            "extractBlock",
            "create_file(path, content)",
            "replace_file(path,",
            "verify in the same program",
            "status-result discipline",
            "Tool results can be large",
            "status-shaped",
            "returned *then*",
            "Before repeating a call",
            "if it wrote, sent, or deleted, it already happened",
        ] {
            assert!(card.contains(needle), "card missing: {needle}");
        }
    }
}
