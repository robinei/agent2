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
- Plan ahead and write the plan *as* the program: think through the whole \
job and manifest as much of it as you can in one `run_program` — loops, \
conditionals, iteration over a worklist — not one small program per step. \
Each `run_program` is a round-trip through you (an LLM turn), so N \
independent steps cost one turn, not N; spend a fresh roundtrip only where \
a step truly needs a result you couldn't predict. Reading three files, \
editing each, and verifying is *one* program. Validation of what you just \
wrote (`parse_errors`, a `bash` build / tests) belongs in the program that \
wrote it, never a follow-up `run_program`.
- Writing more than a couple of files, or any large body? Delegate — don't \
pack every body into one call. Pin the interfaces (paths, signatures, \
shared types), spawn one `tools.agent` per file with those contracts, and \
`await` them together. Two payoffs: the bodies generate concurrently \
(not serially in your program text), and the work stays *visible* — each \
file is its own progressing frame. One `run_program` carrying many large \
attachments is the opposite: it emits silently while you write every body, \
then lands everything at once with no progress in between. Keep the \
orchestrator small; own the final cross-file check.
- Need a decision or missing input partway? `raise(name, payload)` and \
continue the *same* program with `resume(value)`; don't return a final \
answer just to start over with a fresh `run_program`. A `raise` keeps a \
long orchestration alive — restarting from the top discards its progress \
(variables, in-flight reads).
- A reply with *no* tool call ends your turn; its text is your final \
answer.

## program contract
- `input` is a read-only const holding this frame's JSON input.
- `attachments` is a read-only const holding the name→string map you pass \
as run_program's second argument. **Any file body or blob longer than a \
few lines MUST be passed in `attachments` and read as `attachments.<name>` \
— never inlined into `source`.** Inlined content bloats the program, has \
to be JS-escaped (backtick/`${` hazards that silently corrupt it), and \
buries the logic; an attachment is inert string data. Inline a literal \
only for a line or two. So to write two files, the run_program call is:
```
{ source: `await tools.create_file(\"/x/app.js\", attachments.app);
           await tools.create_file(\"/x/page.html\", attachments.page);
           return \"wrote 2 files\";`,
  attachments: { app: \"<the whole app.js body>\",
                 page: \"<the whole page.html body>\" } }
```
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

## answers and results
- Tool results can be large; they live in variables and the log, not \
your context — keep them there, work on them in the program, and reuse \
them by id.
- To bring content into your *reasoning*, `return` it: returns are \
budgeted (~64 KB by default; a caller may raise a subagent's via \
`agent(task, {budget})`), delivered into your context up to that budget, \
with the full value always fetchable by id. Reading three files to \
summarize them is one program that returns the summary — never \
`bash sed`/`tool_result` content into `/tmp` to read it back in slices.
- To digest many large files, spawn one `agent` per file — each *returns \
its summary*. If your answer is a large *product* (a verbatim file, a \
full report), `create_file` it and report the path; don't try to shrink \
it into the return.

## editing files
- Read into a variable → locate structurally (`grep -n`, an outline, \
or a brace/dedent scan) → compute the exact span → `replace_file` with \
the `version` from the read → **verify in the same program** (re-grep / \
re-read / run the build via `bash`) and only `raise` on surprise.
- Author new content freely; locate with the smallest reliable handle \
(a short anchor or a computed span), never by reproducing a large block.
- `create_file(path, content)` for new files; `replace_file(path, \
expected_version, content)` passes the `version` from `read_file` — a \
changed-file condition means re-read and re-apply. Pass a non-trivial \
`content` from `attachments.<name>`, never a long inline literal.
- `Edit.*` pure string helpers (namespace like `Math`). Each errors on \
ambiguity (0/N matches, out-of-range, overlapping) as a catchable runtime \
error — use try/catch when expected. Signatures:
  `Edit.replaceOnce(text, old, new)` → string — replace old (string or \
RegExp) iff it matches exactly once in text; errors with the actual count.
  `Edit.replaceCount(text, old, new)` → { result, count } — replace every \
occurrence; returns modified string and match count.
  `Edit.count(text, needle)` → number — count non-overlapping matches of \
needle (string or RegExp) in text.
  `Edit.extractBlock(text, headIndex)` → { start, end } — find nearest \
`{` at/after headIndex, balance braces, return span (exclusive end). \
Errors no-brace or unbalanced.
  `Edit.extractByIndent(text, lineIndex)` → { start, end } — from \
0-indexed lineIndex, collect lines with greater indent until a non-blank \
dedent. Blank lines included. Errors out-of-range.
  `Edit.extractEnclosing(text, index, open, close)` → { start, end } — \
innermost pair of single-char delimiters enclosing byte index. Errors if none.
  `Edit.replaceLines(text, start, end, newText)` → string — replace \
1-indexed inclusive line range. Errors on invalid range.
  `Edit.insertAt(text, lineNo, newText)` → string — insert before \
1-indexed lineNo; lineNo = last-line+1 appends. Errors out-of-range.
  `Edit.applyEdits(text, edits)` → string — atomic multi-replace. edits \
is `[{ old, new }]`; each old must match exactly once, spans must be \
disjoint, applied right-to-left. Errors naming the offender.
- Tools: `outline(path)` lists definitions via tree-sitter; \
`parse_errors(path)` / `parse_errors(null, source, lang)` verifies syntax \
**before writing** — the loop: read → transform → `parse_errors` → \
`replace_file` → optional `bash` build/test verify.

## tools
Call tools **positionally**, each argument separate: \
`create_file(path, content)`, `bash(cmd)` — never bundle the arguments \
into one array (`create_file([path, content])` is wrong). A tool's \
`array` input schema describes its positional argument *list*, not a \
single array parameter. Every call returns a promise; `await` it. Calls \
started before awaiting run in parallel (`Promise.all` works).
- tools.tool_result(id) — re-fetch artifact [#id] from the log \
(instant, free).
- tools.agent({ prompt, input, budget? }) — delegate a subtask to a \
fresh subagent; resolves to its JSON result. It sees only what you pass \
it. `budget` (bytes) caps the answer it delivers into your context \
(default ~64 KB); raise it when you want a large result back.
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
- RegExp: `/pattern/flags`, `.test()`, `.exec()` (a `/g` regex is \
stateful: `.exec()` advances `.lastIndex`, so the `while ((m = \
re.exec(s)) !== null)` loop terminates). For all matches with capture \
groups in one call, prefer `str.matchAll(/…/g)` → array of match \
objects. Named groups `(?<name>…)` show up on a match's `.groups`. \
`.replace()` / `.replaceAll()` take a string replacement (`$1`..`$n`, \
`$<name>`, `$&` tokens) or a function replacer \
(`(match, ...groups, offset, str) => …`).
- Strings are UTF-8 bytes: `.length` and all indices count bytes, not \
UTF-16 units.
- `<` `>` `<=` `>=` never coerce across types; objects/arrays never \
coerce to primitives (`[5] == 5` is false, `[] + 1` is an error).
- Calls are strict-arity only for builtins (`Math.max`, `s.split`, etc.); \
user functions may omit trailing arguments. Writing past an array's end \
errors (use `push`). Bitwise ops are 64-bit.";

/// Worked programs — the shapes the prose describes, shown concretely.
/// A raw string so the JS (quotes, backticks, `${}`) needs no escaping;
/// indentation is literal, so the code sits at the source's left margin.
const CARD_EXAMPLES: &str = r####"

## examples
Good programs orchestrate and self-check in *one* run.

Write files (bodies in `attachments`) and verify them in the SAME program —
never split the check into a second run_program:
```
await tools.bash("mkdir -p /app");
for (const [path, body] of [["/app/game.js", attachments.game],
                            ["/app/index.html", attachments.html]]) {
  await tools.create_file(path, body);
}
const checks = await Promise.all([
  tools.parse_errors("/app/game.js"),
  tools.parse_errors("/app/index.html"),  // .html soft-skips → { ok: null }
]);
const bad = checks.find(c => c.ok === false);
if (bad) raise("syntax_error", bad);   // stop and hand it to me, don't guess
return "wrote + verified 2 files";
```

Edit an existing file — read the version, transform, write it back, re-check:
```
const f = await tools.read_file("/app/util.js");
const next = Edit.replaceOnce(f.content, "const MAX = 10;", "const MAX = 100;");
await tools.replace_file("/app/util.js", f.version, next);  // version from the read
const c = await tools.parse_errors("/app/util.js");
if (c.ok === false) raise("syntax_error", c);
return "bumped MAX to 100";
```

Many files — author ONE detailed plan (it's a big string, so an attachment),
hand the *same* plan to every subagent, and have each build just its slice.
Shared spec → the independently-written files compose into a coherent whole:
```
// attachments.plan = the full architecture: every file's role, the exact
// shared interfaces/signatures/names, conventions. Authored once.
const files = ["/app/game.js", "/app/render.js", "/app/input.js"];
await Promise.all(files.map(file =>
  tools.agent({
    prompt: `Implement only ${file}, exactly to the plan. Honor the shared ` +
            `interfaces and names verbatim so it composes with its siblings. ` +
            `create_file it, then parse_errors it.`,
    input: { plan: attachments.plan, file },   // every agent sees the whole plan
  })));
const c = await tools.bash("cd /app && node --check game.js render.js input.js");
return c.status === 0 ? "all slices built against one shared plan" : c.stderr;
```
"####;

/// Render the full card: static head, the registry's tools (sorted,
/// one line each), static tail, worked examples.
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
    card.push_str(CARD_EXAMPLES);
    card
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::registry::ToolDef;
    use serde_json::json;

    /// Bound on the static (non-tool-list) card text. The card carries the
    /// always-true contract (acting strategy, editing recipe, dialect
    /// divergences) plus worked examples; per-incident detail belongs in
    /// the reports. Roomy on purpose — concrete examples earn their bytes
    /// (they shift first-try behavior where prose alone did not).
    const CARD_STATIC_MAX_BYTES: usize = 30_720;

    fn registry_with_tools() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(ToolDef {
            name: "fetch_page".into(),
            description: "Fetch a URL and return its body text.".into(),
            input_schema: json!({ "type": "array", "items": [{ "type": "string" }] }),
            handler: Box::new(|_| Ok(json!(null))),
        });
        registry.register(ToolDef {
            name: "send_email".into(),
            description: "Send an email.".into(),
            input_schema: json!({ "type": "array" }),
            handler: Box::new(|_| Ok(json!(null))),
        });
        registry
    }

    #[test]
    fn card_stays_short() {
        let static_bytes = CARD_HEAD.len() + CARD_TAIL.len() + CARD_EXAMPLES.len();
        assert!(
            static_bytes <= CARD_STATIC_MAX_BYTES,
            "static card text ({static_bytes} bytes) grew past \
             {CARD_STATIC_MAX_BYTES} — keep it bounded; move detail into the reports"
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
            "Plan ahead and write the plan",
            "one `tools.agent` per file",
            "keeps a long orchestration alive",
            "`input` is a read-only const",
            "`attachments` is a read-only const",
            "attachments: { app:",
            "top-level `return <value>`",
            // Worked examples (CARD_EXAMPLES):
            "## examples",
            "verify them in the SAME program",
            "Edit.replaceOnce(f.content",
            "every agent sees the whole plan",
            "console.log",
            "tools.tool_result(id)",
            "Call tools **positionally**",
            "not a single array parameter",
            "tools.agent({ prompt, input, budget? })",
            "raise(name, payload)",
            "resume(value)",
            "No `this` or `class`",
            "`new Map()`",
            "`Map` and `Set`",
            "$<name>",
            "UTF-8 bytes",
            // Step 5 additions:
            "editing files",
            "extractBlock",
            "create_file(path, content)",
            "replace_file(path,",
            "verify in the same program",
            "answers and results",
            "Tool results can be large",
            "returns are budgeted",
            "returned *then*",
            "Before repeating a call",
            "if it wrote, sent, or deleted, it already happened",
        ] {
            assert!(card.contains(needle), "card missing: {needle}");
        }
    }
}
