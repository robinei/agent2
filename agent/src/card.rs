//! The card — the system prompt (phase 20 doc, Step C4).
//!
//! Under code mode there is no separate dialect card and no
//! instruction header: this string *is* the system prompt (Step A1:
//! "the card goes in `system`"), the one place the model is told
//! anything, and the immutable cache prefix everything else is
//! appended after. One file, one golden test, versioned — a card
//! change is a behaviour change, same as any other.
//!
//! A spec, not a persona: no "you are a helpful assistant". The
//! register is "programs are written here; emit only valid
//! JavaScript; the whole response is parsed" — every sentence below
//! is held to that.
//!
//! Carries the verb set and nothing else structural — no schemas, the
//! signatures are the documentation — plus the guidance collected from
//! the parts that derived each line, cited inline below so a future
//! edit can trace a sentence back to its reasoning instead of
//! wondering why it's there.

/// The card, verbatim. Kept as one constant (not assembled from
/// fragments) so the golden test below is the whole prompt, not an
/// approximation of it.
/// The card's own text, and the exemplars that open `messages`.
///
/// Both are *files*, not string literals. A prompt is the one part of
/// this system a person tunes by reading it and changing a sentence,
/// and that must not require a Rust toolchain — nor should a prose
/// edit show up in review wrapped in a raw string literal. The default
/// set is embedded with `include_str!`, so a stock binary is
/// self-contained and the golden test still guards accidental edits;
/// `--card <dir>` replaces the whole set at startup, which is what
/// makes an ablation a command-line argument rather than a branch.
///
/// Editing the files cannot disturb a conversation already underway:
/// `Agent.system` and `Agent.exemplars` snapshot the whole prefix at an
/// agent's root (see `types::EventPayload`), so it belongs to the
/// conversation and not to whatever the files say today. Only `system`
/// did until 27, which meant this sentence was half true and the half
/// that was false was invisible — the prose came from the log and the
/// examples from the running process.
pub struct Card {
    pub text: String,
    pub exemplars: Vec<Exemplar>,
}

/// The card shipped in the binary: `agent/card/card.md` plus
/// `agent/card/exemplars/NN-name.{txt,js}`.
pub fn embedded() -> Card {
    macro_rules! exemplar {
        ($stem:literal) => {
            Exemplar {
                user: include_str!(concat!("../card/exemplars/", $stem, ".txt")).to_owned(),
                // **The rendered turn, not the source.** `.js` is what
                // was written; `.turn` is how the document hands it
                // back — carrying `【↓ history[N]】` above each block and
                // `【← history[N]】` beside each call. Shipping the
                // source made the examples the one place those never
                // appear, while every real turn the model reads is
                // covered in them.
                assistant: include_str!(concat!("../card/exemplars/", $stem, ".turn")).to_owned(),
            }
        };
    }
    Card {
        text: include_str!("../card/card.md").to_owned(),
        // **In the order they happened**, which is not the order they
        // were written in. They are one session now — a reply can act
        // on what the turn before it was shown — so `09-act` follows
        // `05-keep` because it fetches the row `05-keep` kept, and
        // `04-many` follows `02-continue` because it sweeps what that
        // one found. Shuffle them and the ids point at nothing.
        //
        // `exemplar_gen::SERIES` is the same order and a test pins the
        // two together.
        exemplars: vec![
            exemplar!("01-finish"),
            exemplar!("02-continue"),
            exemplar!("04-many"),
            exemplar!("03-ask"),
            exemplar!("05-keep"),
            exemplar!("09-act"),
            exemplar!("06-fork"),
            exemplar!("08-watch"),
            exemplar!("07-supervise"),
        ],
    }
}

/// Read a card from a directory laid out like `agent/card/`: `card.md`
/// and an `exemplars/` directory whose `.js` files are the assistant
/// turns, each paired with a `.txt` of the same stem for the user turn.
/// Exemplars are ordered by filename, which is why they are numbered.
///
/// A missing `exemplars/` is not an error — a card with none is a
/// legitimate thing to measure (see `25_JS_DIALECT.md` 25.6), and the
/// ablation that motivated `--card` needs exactly that.
pub fn load_from(dir: &std::path::Path) -> Result<Card, String> {
    let text = std::fs::read_to_string(dir.join("card.md"))
        .map_err(|e| format!("{}: {e}", dir.join("card.md").display()))?;
    let ex_dir = dir.join("exemplars");
    let mut stems: Vec<std::path::PathBuf> = match std::fs::read_dir(&ex_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "js"))
            .collect(),
        Err(_) => Vec::new(),
    };
    stems.sort();
    let mut exemplars = Vec::with_capacity(stems.len());
    for js in stems {
        let txt = js.with_extension("txt");
        let user = std::fs::read_to_string(&txt).map_err(|e| {
            format!(
                "{}: {e} (every exemplar needs its user turn)",
                txt.display()
            )
        })?;
        let assistant =
            std::fs::read_to_string(&js).map_err(|e| format!("{}: {e}", js.display()))?;
        exemplars.push(Exemplar { user, assistant });
    }
    Ok(Card { text, exemplars })
}

/// The card this process runs with. Set once, before any session opens;
/// [`embedded`] when nobody said otherwise.
static ACTIVE: std::sync::OnceLock<Card> = std::sync::OnceLock::new();

/// Install a card. Returns an error if a session has already read one,
/// because two different prompts in one process would make the log
/// ambiguous about which produced a given program.
pub fn set_active(card: Card) -> Result<(), String> {
    ACTIVE
        .set(card)
        .map_err(|_| "the card was already read; --card must come before the session opens".into())
}

pub fn active() -> &'static Card {
    ACTIVE.get_or_init(embedded)
}

/// Per-tool clip for the rendered description. It exists so one
/// verbose entry cannot dominate the cache-immutable prefix, not as a
/// budget to write up to — since 27.8 the signature carries the shapes
/// and the prose says only what a type cannot, so every shipped tool
/// fits comfortably inside this.
const DESCRIPTION_MAX_BYTES: usize = 400;

/// **The card teaches the glyphs the renderer emits.**
///
/// `card.md` is a file, so it cannot reference `BLOCK_ARROW` and
/// `ARROW` the way the compaction directive now does. This is the
/// substitute: rename a marker and the card stops describing what the
/// model will see, which is precisely the failure the forgery guard
/// and the console line cap were — a literal encoding another part's
/// format, with nothing watching it.
#[test]
fn the_card_teaches_the_markers_the_document_uses() {
    let card = &active().text;
    let down = crate::document::BLOCK_ARROW;
    let left = crate::document::ARROW.trim();
    let (open, close) = (crate::document::FENCE_OPEN, crate::document::FENCE_CLOSE);
    assert!(
        card.contains(&format!("{open}{down} history[")),
        "the card shows the block marker as it renders: {open}{down} history[N]{close}"
    );
    assert!(
        card.contains(&format!("{open}{left} history[")),
        "and the call annotation as it renders: {open}{left} history[N]{close}"
    );
    // **The fence is the rule now, so the fence is what must be
    // named.** It used to be the two arrows — and naming those was the
    // best available, because they were all the model had to go on.
    // A reply is cleaned of anything between the brackets before it is
    // kept, so the brackets are the thing to recognise and the thing
    // not to type.
    assert!(
        card.lines()
            .any(|l| l.contains(open) && l.contains(close) && l.contains("harness")),
        "one line names the fence and says whose it is"
    );
}

/// The shipped manifest, as the model reads it — the facts that were
/// wrong in it, held.
///
/// **One of them was this test's own.** It asserted `read_file`
/// "declares no field its handler cannot produce", which quietly made
/// the handler the authority. It is not: `machine.rs` injects `id` into
/// every object result before it is logged, so the model is handed
/// `{ content, version, id }` while the manifest said
/// `{ content, version }` — and the card tells it to use `f.id`. Six
/// return types denied the field the card was pointing at, behind a
/// green test, because the test was asking the wrong layer.
#[test]
fn the_shipped_manifest_tells_the_truth_about_itself() {
    let manifest = tool_manifest(&crate::host::tools::real_registry(), true);
    assert!(
        !manifest.contains("[truncated;"),
        "a clipped tool description drops its tail where the model reads it:\n{manifest}"
    );
    assert!(
        manifest.contains("30s timeout, 4MB per stream"),
        "`bash`'s hard limits are the part that must survive a clip: {manifest}"
    );
    assert!(
        manifest.contains("parse_errors(path: string | null"),
        "the parameter its own @example passes `null` to says so: {manifest}"
    );
    assert!(
        manifest.contains("diff?: string"),
        "`replace_file` returns a diff, and saying so is what stops the re-read: {manifest}"
    );
    assert!(
        manifest.contains("start_line: number"),
        "`outline`'s entries carry start_line/end_line, not `line`: {manifest}"
    );
    assert!(
        manifest.contains(
            "read_file(path: string, from?: number, to?: number): \
             Promise<{ content: string; version: string; id: number }>"
        ),
        "`read_file` declares every field the model is handed: {manifest}"
    );
}

/// **The worked examples compile.**
///
/// They are the most directly imitated thing the model is given — the
/// `outline` example taught `const { items }` against a tool that
/// returned a bare array, and the fifth one taught a copy the card
/// forbids in bold. Syntax is the cheapest of those failures to catch
/// and the only one a test can catch on its own, so it is caught here:
/// an exemplar teaching a construct this dialect does not have would
/// be a broken example shipped in every prompt.
///
/// The blocks of one exemplar are concatenated before compiling,
/// because that is how a reply's blocks see each other — the fourth
/// binds `hits` in its first block and reads it in its second.
#[test]
fn every_worked_example_compiles() {
    for ex in &active().exemplars {
        // Cleaned first, as `Notebook::push_text` cleans a reply: the
        // examples ship as rendered turns, so a block carries
        // `【← history[N]】` beside a call, which no compiler is ever
        // handed.
        let mut reply = ex.assistant.clone();
        crate::notebook::strip_annotations_for_test(&mut reply);
        let js: String = reply
            .split("```js")
            .skip(1)
            .filter_map(|rest| rest.split_once("```").map(|(code, _)| code.to_owned()))
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        assert!(
            !js.is_empty(),
            "an exemplar with no ```js block: {}",
            ex.user
        );
        if let Err(errs) = interp::compile(&js) {
            panic!(
                "a worked example does not compile — it is shipped in every prompt:\n{errs:?}\n{js}"
            );
        }
    }
}

/// **The dialect table is true of the interpreter it describes.**
///
/// The card names three places this language answers differently from
/// JS, each as a literal expression and a literal result. They are the
/// only claims in the card a reader is invited to rely on without
/// trying them, and nothing connected them to the interpreter — the
/// behaviours are pinned in `interp`'s own tests, but under their own
/// names, so a change there would leave the card asserting something
/// false in every prompt.
///
/// This runs the left column and checks the right. It reads the table
/// out of the card rather than restating it, so a row added to the
/// card is a row this checks, and a row this cannot parse is a row the
/// card has written in some shape a reader will not recognise either.
#[test]
fn the_dialect_table_says_what_the_interpreter_does() {
    let card = active().text.clone();
    let rows: Vec<(String, String)> = card
        .lines()
        .skip_while(|l| !l.starts_with("| you write "))
        .skip(1)
        .skip_while(|l| l.starts_with("|---"))
        .take_while(|l| l.starts_with('|'))
        .filter_map(|l| {
            let cells: Vec<&str> = l.trim_matches('|').split('|').collect();
            let un = |c: &str| c.trim().trim_matches('`').to_owned();
            Some((un(cells.first()?), un(cells.get(1)?)))
        })
        .collect();
    assert_eq!(rows.len(), 2, "the table's rows parsed: {rows:?}");

    for (expr, expected) in rows {
        // `e` is the card's own word for a caught error, and one row is
        // about exactly that. Binding it here is what makes the row
        // runnable without restating it.
        let src =
            format!("let e; try {{ null.x; }} catch (err) {{ e = err; }} return String({expr});");
        let prog = interp::compile(&src)
            .unwrap_or_else(|e| panic!("the table's `{expr}` does not compile: {e:?}"));
        let mut vm = interp::VM::for_program(prog, serde_json::Value::Null).unwrap();
        let got = match vm.step(u64::MAX).expect("the table's expression runs") {
            interp::StepResult::Done { value, .. } => vm
                .stack_value_to_json(&value, 0)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            other => panic!("`{expr}` did not finish: {other:?}"),
        };
        assert_eq!(
            got, expected,
            "the card says `{expr}` is `{expected}`; it is `{got}`"
        );
    }
}

/// **The card does not contradict its own declarations about `await`.**
///
/// The sentence teaching `await` said "of everything above, only
/// `ask` is [async]" while the block above it declared `choose`
/// returning `Promise<string>`. No run paid for it — 27 `choose`
/// calls in the corpus, every one awaited, because the worked example
/// awaits it and the example wins. That is the third time in this
/// audit that an example covered for a rule that was wrong, which is
/// a reason to check the rule rather than to trust the example again.
#[test]
fn every_promise_above_the_tools_is_named_as_one() {
    let card = active().text.clone();
    let decls = card
        .split_once("## What you can call")
        .expect("the declaration block")
        .1
        .split_once("## Two places")
        .expect("and its end")
        .0;
    let promised: Vec<&str> = decls
        .lines()
        .filter(|l| l.contains("Promise<"))
        .filter_map(|l| l.split_once("declare function ").map(|(_, r)| r))
        .filter_map(|r| r.split_once('(').map(|(n, _)| n))
        .collect();
    assert_eq!(
        promised,
        vec!["ask", "choose"],
        "the async verbs above `tools.*`"
    );
    // Built from the declarations rather than written out, so adding a
    // third async verb fails here until the paragraph names it too.
    let named = promised
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(" and ");
    assert!(
        card.contains(&format!("only {named} are")),
        "the `await` paragraph does not name exactly {named}"
    );
}

/// **A closed value set renders as a union, not as `string`.**
///
/// Everything rendered as `string`, so a small fixed vocabulary
/// reached the reader as "any text". `outline`'s `kind` was exactly
/// that, and the guess models actually make — `"function"` — was not
/// one of its values; the filter it appears in returns an empty array
/// rather than an error, and one run read that emptiness as "nothing
/// here is live" and replaced a source file with its docstring.
#[test]
fn a_parameter_with_a_closed_value_set_names_the_values() {
    let manifest = tool_manifest(&crate::host::tools::real_registry(), true);
    assert!(
        manifest.contains(r#""rust" | "javascript" | "typescript" | "python""#),
        "parse_errors's `lang` still renders as `string`:\n{manifest}"
    );
    assert!(
        !manifest.contains("lang?: string"),
        "and not both ways:\n{manifest}"
    );
}

/// **The card's TypeScript parses.**
///
/// The declaration block is the largest single thing the model reads —
/// half the card — and nothing has ever checked that it is valid
/// TypeScript. A stray brace in it would reach every prompt as text
/// that looks authoritative and parses as nothing, and the reader most
/// likely to be confused by it is the one that treats declarations as
/// the contract.
///
/// The harness already carries a TypeScript parser for `parse_errors`,
/// so this costs one call.
#[test]
fn the_cards_declarations_are_valid_typescript() {
    let card = &active().text;
    let mut blocks = 0;
    for rest in card.split("```ts").skip(1) {
        let Some((code, _)) = rest.split_once("```") else {
            continue;
        };
        blocks += 1;
        let out =
            crate::host::structural::run_parse_errors(code, "typescript").expect("the parser runs");
        assert_eq!(
            out["ok"], true,
            "the card's TypeScript does not parse: {}",
            out["errors"]
        );
    }
    assert!(blocks >= 1, "the card should carry a ```ts block");
}

/// **Every tool's `@example` compiles.**
///
/// They are one line each and they are copied verbatim — the `outline`
/// one taught `const { items }` against a tool that returned a bare
/// array for months. That particular failure was semantic and a
/// compiler cannot see it; a typo in one is the cheap half, and it
/// ships in every prompt just the same.
///
/// The bodies use `path`, `body`, `candidate`, `lang`, `p`, `f`, `old`
/// and `new` as stand-ins, so they are declared before the example is
/// compiled — the same thing a reply does before using a name.
#[test]
fn every_tool_example_compiles() {
    let preamble = "const path = 'p'; const body = ''; const candidate = ''; \
                    const lang = 'rust'; const p = 'p'; const f = { content: '', version: '' }; \
                    const old = 'a'; const replacement = 'b';";
    for def in crate::host::tools::real_registry().iter() {
        let Some(example) = &def.example else {
            continue;
        };
        let src = format!("{preamble}\nasync function __ex() {{ {example} }}");
        if let Err(errs) = interp::compile(&src) {
            panic!(
                "`{}`'s @example does not compile:\n{errs:?}\n{src}",
                def.name
            );
        }
    }
}

/// Every shipped tool's description fits inside the clip above.
///
/// The doc there says they all "fit comfortably", and `bash` did not:
/// at 408 bytes it was cut mid-word, and what fell off the end was
/// `"s timeout, 4MB per stream"` — so the one hard operational fact in
/// it, that a command has 30 seconds and 4MB, was the part the model
/// never read. A claim about the shipped set is worth exactly as much
/// as the test that holds it, so here is the test.
///
/// The limits now lead the sentence as well. A clip takes the tail, so
/// what must survive one belongs at the front.
#[test]
fn every_tool_description_fits_the_clip() {
    for def in crate::host::tools::real_registry().iter() {
        let n = def.description.len();
        assert!(
            n <= DESCRIPTION_MAX_BYTES,
            "`{}`'s description is {n} bytes against a {DESCRIPTION_MAX_BYTES}-byte clip — \
             the tail would be cut off where the model reads it",
            def.name
        );
    }
}

/// This session's tools, as TypeScript declarations.
///
/// **The same format as everything else the model is told.** It used to
/// be `- tools.bash — <prose> args schema: {"type":"array",…}`, which
/// states an API in the one notation its reader is least practised at,
/// and buried the *result* shape in the middle of an English sentence
/// ("Resolves to { status, stdout, stderr, truncated? }"). A model that
/// has read a great deal of TypeScript should be handed a `.d.ts`.
///
/// Parameter names come from each schema item's `name`, optionality
/// from its position against `minItems`, and the return type from
/// `ToolDef::returns`. A tool that supplies neither still renders — as
/// `argN: unknown` and `Promise<unknown>` — because a manifest that
/// omits a live tool is worse than one that describes it thinly.
fn ts_type(schema: &serde_json::Value) -> String {
    // **A parameter with a closed value set says what they are.**
    // Everything here rendered as `string`, so a small fixed
    // vocabulary reached the reader as "any text" and had to be
    // recovered from the prose beside it — or guessed. `outline`'s
    // `kind` was exactly that, and the guess models actually make,
    // `"function"`, was not one of the values; the filter it appears
    // in returns an empty array rather than an error, and one run read
    // that emptiness as "nothing here is live" and emptied a file.
    // A union costs a few bytes and removes the guess.
    if let Some(values) = schema.get("enum").and_then(|e| e.as_array()) {
        let mut rendered: Vec<String> = values
            .iter()
            .filter_map(|v| v.as_str())
            .map(|v| format!("\"{v}\""))
            .collect();
        if !rendered.is_empty() {
            if schema.get("nullable").and_then(|n| n.as_bool()) == Some(true) {
                rendered.push("null".into());
            }
            return rendered.join(" | ");
        }
    }
    ts_type_plain(schema).to_owned()
}

fn ts_type_plain(schema: &serde_json::Value) -> &'static str {
    // **A parameter that takes `null` says so.** `parse_errors`'s own
    // `@example` passes `null` for its path — that is the documented
    // way to check unwritten content — while the signature above it
    // said `path: string`. A declaration contradicted by the example
    // printed under it teaches nothing about which to believe.
    if schema.get("nullable").and_then(|n| n.as_bool()) == Some(true) {
        return match schema.get("type").and_then(|t| t.as_str()) {
            Some("string") => "string | null",
            Some("integer") | Some("number") => "number | null",
            Some("boolean") => "boolean | null",
            _ => "unknown",
        };
    }
    match schema.get("type").and_then(|t| t.as_str()) {
        Some("string") => "string",
        Some("integer") | Some("number") => "number",
        Some("boolean") => "boolean",
        Some("array") => "unknown[]",
        Some("object") => "Record<string, unknown>",
        _ => "unknown",
    }
}

/// A tool's TSDoc, synthesised from its fields rather than stored as
/// one blob: the description is the summary, each guideline a bullet,
/// the example an `@example`. Kept apart in [`ToolDef`] so a rule can
/// be added to one tool without rewriting its prose, and so the
/// rendering — not the author — decides the shape.
///
/// Collapses to a single `/** … */` line when there is nothing but a
/// description, because most tools have nothing more to say and a
/// four-line comment around one sentence is noise.
fn doc_comment(def: &crate::host::ToolDef) -> String {
    let summary = crate::report::clip(&def.description, DESCRIPTION_MAX_BYTES);
    if def.guidelines.is_empty() && def.example.is_none() {
        return format!("\n  /** {summary} */\n");
    }
    let mut out = format!("\n  /**\n   * {summary}\n");
    if !def.guidelines.is_empty() {
        out.push_str("   *\n");
        for g in &def.guidelines {
            out.push_str(&format!("   * - {g}\n"));
        }
    }
    if let Some(example) = &def.example {
        out.push_str(&format!("   *\n   * @example {example}\n"));
    }
    out.push_str("   */\n");
    out
}

/// The tool declarations, framed to match the card they are appended
/// to — see the comment at the splice below for why `markdown` is read
/// off the card's own first bytes rather than passed down from a
/// transport.
pub fn tool_manifest(registry: &crate::host::ToolRegistry, markdown: bool) -> String {
    let mut manifest = String::new();
    let mut tools: Vec<_> = registry.iter().collect();
    if tools.is_empty() {
        return manifest;
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    // A namespace, because that is how they are *called*. Declared bare
    // they would read as globals beside `spawn` and `ask`, and the one
    // thing the `tools.` prefix reliably signals — that these are this
    // session's configured capabilities — would be missing from the
    // only place the model reads their names.
    // **A card is framed the way its own model writes.** Under the
    // notebook transport the reply is markdown with code in blocks, so
    // the card is too and the manifest is a fenced `ts` block under a
    // heading. Under the program transport the reply is bare
    // JavaScript and nothing else — the card says so in as many words —
    // so the card is a TypeScript document and the manifest is
    // declarations spliced onto the end of it, fenced by nothing.
    //
    // Which it is, is a fact about the card, so it is read off the
    // card: a TypeScript document opens `/**`, and a markdown one does
    // not. No transport parameter reaches here and none should — the
    // card directory is chosen by the caller, and a manifest framed
    // one way against a card written the other is a contradiction the
    // model has to resolve for us.
    if markdown {
        manifest.push_str("\n\n## This session\'s tools\n\n```ts\ndeclare namespace tools {");
    } else {
        manifest.push_str("\n\ndeclare namespace tools {");
    }
    for def in tools {
        let items = def
            .input_schema
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        let required = def
            .input_schema
            .get("minItems")
            .and_then(|n| n.as_u64())
            .unwrap_or(items.len() as u64) as usize;
        let params: Vec<String> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let name = item
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("arg{i}"));
                let opt = if i >= required { "?" } else { "" };
                format!("{name}{opt}: {}", ts_type(item))
            })
            .collect();
        let returns = def.returns.as_deref().unwrap_or("unknown");
        manifest.push_str(&doc_comment(def));
        manifest.push_str(&format!(
            "  function {}({}): Promise<{}>;\n",
            def.name,
            params.join(", "),
            returns,
        ));
    }
    manifest.push_str(if markdown { "}\n```\n" } else { "}\n" });
    manifest
}

/// The full system prompt for one agent: [`CARD`] plus [`tool_manifest`]
/// for its (possibly allowlist-narrowed) registry. Callers snapshot this
/// once, at the agent's root (`Agent.system` — see `types::EventPayload`),
/// never recompute it mid-conversation: the system prompt is the
/// immutable cache prefix, and a later card edit or registry change must
/// not alter an existing conversation's prompt out from under it.
pub fn full_card(registry: &crate::host::ToolRegistry) -> String {
    let card = active();
    let text = &card.text;
    let markdown = !text.starts_with("/**");
    format!(
        "{}{}{}",
        text,
        tool_manifest(registry, markdown),
        working_directory()
    )
}

/// The tool names a rendered system prompt declares, read back out of
/// its own `declare namespace tools { … }` block.
///
/// Read back rather than taken from the registry on purpose: the check
/// this feeds runs over a *document* — a log rendered by `agent capture`
/// or `agent document`, possibly written by another process under
/// another card — where the registry that produced it is long gone and
/// the manifest is the only surviving statement of what the model was
/// told it had.
pub fn tools_declared_in(system: &str) -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    let Some(body) = system.split_once("declare namespace tools {") else {
        return names;
    };
    for line in body.1.lines() {
        let trimmed = line.trim_start();
        if line.starts_with('}') || line.starts_with("```") {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("function ")
            && let Some(name) = rest.split('(').next()
            && !name.is_empty()
        {
            names.insert(name.to_owned());
        }
    }
    names
}

/// Every `tools.X(` a worked example calls but the system prompt beside
/// it does not declare.
///
/// **A document that contradicts itself is one the model stops to
/// litigate.** Three eval documents were built here from scripted
/// sessions, whose registry holds exactly one tool, so each shipped a
/// manifest declaring only `tools.echo` in front of worked examples
/// calling `tools.bash` and `tools.read_file`. The model spent
/// thousands of reasoning tokens on the contradiction — "the tools
/// available in this session are only `tools.echo`! ... So bash/read_file
/// may not exist" — and the arm was not measuring what it was built to
/// measure. Nothing said so; the mismatch is visible only by reading
/// the rendered prompt, which is exactly the thing nobody reads until
/// something has already gone wrong.
///
/// One direction only. The converse — every declared tool is
/// demonstrated — is false by design and should stay false: there are
/// more tools than exemplars, and an exemplar exists to teach a shape,
/// not to tour the registry.
pub fn undeclared_example_tools(system: &str, worked_examples: &[&str]) -> Vec<String> {
    let declared = tools_declared_in(system);
    let mut missing: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for text in worked_examples {
        for (i, _) in text.match_indices("tools.") {
            let name: String = text[i + "tools.".len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            // `tools.X` with no call after it is prose about the
            // namespace, not a call this has to account for.
            if name.is_empty() || !text[i + "tools.".len() + name.len()..].starts_with('(') {
                continue;
            }
            if !declared.contains(&name) {
                missing.insert(name);
            }
        }
    }
    missing.into_iter().collect()
}

/// Where the program will run, stated once.
///
/// **Because we never told it.** The tool descriptions say "absolute or
/// cwd-relative path" and nothing says what the cwd *is*, so the first
/// program of nearly every run is spent working that out — `ls -a`,
/// `pwd`, `cat package.json` — and one run on 2026-09-17 ran `find . ~
/// -maxdepth 5`, searching the home directory, because it genuinely did
/// not know where it was. A tool loop pays two calls inside one turn
/// for that orientation; here it costs a whole program, which is a
/// whole completion.
///
/// `pi`'s system prompt ends with exactly this line, which is how the
/// omission was noticed at all.
///
/// Snapshotted with the rest of the prompt at the agent's root, like
/// `system` and `exemplars`: a session that changes directory later
/// must not silently rewrite what an existing conversation was told.
fn working_directory() -> String {
    match std::env::current_dir() {
        Ok(dir) => format!("\n\nCurrent working directory: {}", dir.display()),
        // Not worth failing a session over, and a wrong answer would be
        // worse than none.
        Err(_) => String::new(),
    }
}

/// A worked exemplar: a real user/assistant pair opening `messages`,
/// never part of the card — its whole point is to demonstrate an
/// *assistant* turn (Step B1), which only a message in that role can
/// do. Turn one has nothing else to imitate (the model's own prior
/// programs are its few-shot evidence, and there are none yet), so
/// this is the restoring force against a timid first program —
/// cheap insurance, not a remedy applied after the fact.
///
/// Why each of the ten exists, and what live failure it answers, is in
/// the file it lives in: `agent/card/exemplars/NN-name.js` opens with
/// the comment that used to sit on this constant.
pub use crate::types::Exemplar;

/// The exemplars of the active card.
pub fn seed_exemplars() -> &'static [Exemplar] {
    &active().exemplars
}

#[cfg(test)]
mod tests {

    use super::*;

    /// Tests hold the *embedded* card to its contract — the shipped
    /// default, not whatever `--card` a run installed.
    fn card() -> String {
        embedded().text
    }

    fn exemplars() -> Vec<Exemplar> {
        embedded().exemplars
    }

    /// The worked example that demonstrates `needle`, **found by what
    /// it does rather than by where it sits**.
    ///
    /// These assertions used to index — `ex[5]` was the fork one —
    /// which tied every claim to a position in `embedded()`. The
    /// examples are one session now and ship in the order they
    /// happened, so a step inserted in the middle would silently move
    /// every assertion onto a different example and go on passing.
    fn with(needle: &str) -> String {
        exemplars()
            .into_iter()
            .find(|e| e.assistant.contains(needle))
            .unwrap_or_else(|| panic!("no worked example calls {needle}"))
            .assistant
    }

    use crate::host::{ToolDef, ToolRegistry};
    use serde_json::json;

    fn registry_with_tools() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(ToolDef {
            name: "fetch_page".into(),
            description: "Fetch a URL and return its body text.".into(),
            input_schema: json!({
                "type": "array",
                "items": [
                    { "name": "url", "type": "string" },
                    { "name": "timeoutMs", "type": "integer" }
                ],
                "minItems": 1
            }),
            guidelines: Vec::new(),
            example: None,
            returns: Some("{ body: string }".into()),
            handler: Box::new(|_| Ok(json!(null))),
            show_once: false,
        });
        // A tool that supplies neither names nor a return type, to pin
        // that the manifest still renders it rather than dropping it.
        registry.register(ToolDef {
            name: "bare".into(),
            description: "No names, no return type.".into(),
            input_schema: json!({ "type": "array", "items": [{ "type": "object" }] }),
            guidelines: Vec::new(),
            example: None,
            returns: None,
            handler: Box::new(|_| Ok(json!(null))),
            show_once: false,
        });
        registry
    }

    /// `dialect.rs`'s own test, ported: the manifest is generated from
    /// the registry's schemas, not hand-maintained prose — and since
    /// 27.8 it is generated as TypeScript, the notation its reader is
    /// most practised at, rather than as JSON Schema embedded in an
    /// English sentence.
    #[test]
    fn tool_manifest_is_generated_from_schemas() {
        let manifest = tool_manifest(&registry_with_tools(), false);
        assert!(
            manifest.contains(
                "function fetch_page(url: string, timeoutMs?: number): \
                 Promise<{ body: string }>;"
            ),
            "{manifest}"
        );
        // The description becomes the doc comment, where a reader of
        // declarations looks for it.
        assert!(
            manifest.contains("/** Fetch a URL and return its body text. */"),
            "{manifest}"
        );
        // Optionality is read off `minItems`, not guessed.
        assert!(manifest.contains("timeoutMs?:"), "{manifest}");
    }

    /// **The doc comment is synthesised from fields, not stored.** A
    /// behavioural rule belongs on the declaration it constrains — read
    /// where it applies rather than remembered from an essay — and
    /// keeping it in its own field means a rule can be added to one
    /// tool without rewriting that tool's prose.
    #[test]
    fn a_tools_doc_comment_is_built_from_its_parts() {
        let mut registry = ToolRegistry::new();
        registry.register(ToolDef {
            name: "dig".into(),
            description: "Dig a hole.".into(),
            input_schema: json!({ "type": "array", "items": [{ "name": "depth", "type": "integer" }] }),
            guidelines: vec!["Mind the cables.".into(), "Backfill when done.".into()],
            example: Some("await tools.dig(2);".into()),
            returns: Some("{ depth: number }".into()),
            handler: Box::new(|_| Ok(json!(null))),
            show_once: false,
        });
        let m = tool_manifest(&registry, false);
        assert!(m.contains("   * Dig a hole."), "{m}");
        assert!(m.contains("   * - Mind the cables."), "{m}");
        assert!(m.contains("   * - Backfill when done."), "{m}");
        assert!(m.contains("   * @example await tools.dig(2);"), "{m}");
        assert!(
            m.contains("  function dig(depth: number): Promise<{ depth: number }>;"),
            "{m}"
        );
    }

    /// With nothing but a description it collapses to one line, because
    /// most tools have nothing more to say and four lines of comment
    /// around one sentence is noise.
    #[test]
    fn a_tool_with_only_a_description_gets_a_one_line_comment() {
        let m = tool_manifest(&registry_with_tools(), false);
        assert!(m.contains("  /** No names, no return type. */"), "{m}");
    }

    /// A tool that names no parameters and declares no return type is
    /// still declared. A manifest that omits a live tool is worse than
    /// one that describes it thinly, and `argN: unknown` is honest.
    #[test]
    fn a_tool_without_names_or_a_return_type_still_renders() {
        let manifest = tool_manifest(&registry_with_tools(), false);
        assert!(
            manifest.contains("function bare(arg0: Record<string, unknown>): Promise<unknown>;"),
            "{manifest}"
        );
    }

    /// **A card is framed the way its own model writes**, and the
    /// manifest follows the card rather than a flag of its own. The
    /// shipped card is a TypeScript document — "no prose, no code
    /// fence" is its first line — so its tools are bare declarations
    /// spliced onto the end. A markdown card (the notebook transport's,
    /// where the reply is markdown with code in blocks) gets a heading
    /// and a `ts` fence instead, because a bare `declare namespace`
    /// dropped on the end of a markdown document is the one place the
    /// prompt would contradict the format it is asking for.
    #[test]
    fn the_manifest_is_framed_like_the_card_it_follows() {
        let bare = tool_manifest(&registry_with_tools(), false);
        assert!(bare.starts_with("\n\ndeclare namespace tools {"), "{bare}");
        assert!(!bare.contains("```"), "{bare}");

        let fenced = tool_manifest(&registry_with_tools(), true);
        assert!(fenced.contains("## This session\'s tools"), "{fenced}");
        assert!(
            fenced.contains("```ts\ndeclare namespace tools {"),
            "{fenced}"
        );
        assert!(fenced.trim_end().ends_with("```"), "{fenced}");

        // Both carry the same declarations: the framing is the only
        // difference, so a card cannot lose a tool by being markdown.
        assert_eq!(
            bare.matches("  function ").count(),
            fenced.matches("  function ").count()
        );
    }

    /// The shipped card is markdown; the frozen program-transport card
    /// under `evals/cards/program/` is a TypeScript document, and stays
    /// one — its own first line says the reply is JavaScript and nothing
    /// else. Which framing the generated tool manifest takes is read off
    /// the card's first bytes, so this reads the files rather than
    /// trusting the rule.
    #[test]
    fn the_shipped_card_is_markdown_and_the_frozen_one_is_not() {
        let shipped = include_str!("../card/card.md");
        assert!(!shipped.starts_with("/**"), "the shipped card is markdown");
        assert!(
            shipped.contains("\n```ts\n"),
            "its declarations live in a fence"
        );

        let frozen = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../evals/cards/program/card.md"),
        );
        // Only asserted when the eval cards are present — the binary
        // ships without them.
        if let Ok(frozen) = frozen {
            assert!(
                frozen.starts_with("/**"),
                "the frozen program card is a TypeScript document"
            );
        }
    }

    #[test]
    fn a_markdown_card_balances_its_fences() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cards = [
            dir.join("card/card.md"),
            dir.join("../evals/cards/notebook/card.md"),
        ];
        for path in cards {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue; // the eval cards do not ship with the binary
            };
            if text.starts_with("/**") {
                continue; // a TypeScript document, not markdown
            }
            let opens = text.lines().filter(|l| l.starts_with("```")).count();
            assert_eq!(
                opens % 2,
                0,
                "{} has an unclosed fence: {} lines start with ```",
                path.display(),
                opens
            );
        }
    }

    #[test]
    fn full_card_appends_the_manifest_after_the_card() {
        let full = full_card(&registry_with_tools());
        assert!(full.starts_with(&card()));
        // And says where the program will run — the thing every first
        // program was otherwise spending itself discovering.
        assert!(full.contains("Current working directory: "), "{full}");
        assert!(full.contains("declare namespace tools {"));
        assert!(full.contains("function fetch_page("));
    }

    #[test]
    fn the_card_is_stable() {
        // A golden test in the sense Step C4 asks for: any edit to
        // `card()` shows up as a diff review must look at, not a byte
        // count that silently drifts. Comparing full text (not just a
        // hash) so the diff itself is legible in a failure message.
        const EXPECTED_LEN: usize = 24110;
        assert_eq!(
            card().len(),
            EXPECTED_LEN,
            "card() changed length ({} -> {}) — a deliberate edit should \
             update EXPECTED_LEN in this test, not silently pass",
            EXPECTED_LEN,
            card().len()
        );
    }

    #[test]
    fn the_card_never_says_you_are_a_helpful_assistant() {
        // "A spec, not a persona" (Step C4), checked directly rather
        // than only asserted in a doc comment.
        // Specific persona-establishing phrases, not the bare "you are
        // a" substring — which false-positives on the card's own
        // legitimate "you are *already* holding it in a variable".
        let lower = card().to_lowercase();
        for phrase in ["you are a helpful", "helpful assistant", "i am an ai"] {
            assert!(
                !lower.contains(phrase),
                "found persona language: {phrase:?}"
            );
        }
    }

    /// **The card cannot name an `Edit` verb the runtime does not
    /// have, nor miss one it does.**
    ///
    /// Nothing checked this, and the card is edited by hand. Renaming
    /// `Edit.replaceCount` to `replaceAll` meant touching this card and
    /// three eval card variants; a miss would have left an arm calling
    /// a verb that does not exist — a whole run's worth of garbage,
    /// visible only in a live session, and attributed to the model.
    ///
    /// Both directions, because a verb the card never names is a verb
    /// the model never writes: it is dead weight in the binary and a
    /// capability nobody has.
    #[test]
    fn the_card_declares_exactly_the_edit_verbs_that_exist() {
        use std::collections::BTreeSet;
        let registered: BTreeSet<&str> = interp::builtin::Builtin::namespace_statics()
            .filter(|(ns, _, _)| *ns == "Edit")
            .map(|(_, name, _)| name)
            .collect();

        let edit_verbs = |text: &str| -> BTreeSet<String> {
            let start = text
                .find("declare namespace Edit {")
                .expect("the card declares the Edit namespace");
            let block = &text[start..];
            let end = block.find("\n}").expect("the namespace block closes");
            block[..end]
                .lines()
                .filter_map(|l| l.trim().strip_prefix("function "))
                .filter_map(|l| l.split('(').next())
                .map(str::to_owned)
                .collect()
        };
        let registered: BTreeSet<String> = registered.iter().map(|s| (*s).to_owned()).collect();

        // The shipping card, and every eval arm — an arm is a *copy* of
        // this file, so it is the one that gets forgotten, and an arm
        // calling a verb that does not exist spoils a whole run while
        // looking like the model's fault.
        for (what, text) in [
            ("the card", card()),
            (
                "evals/cards/program",
                include_str!("../../evals/cards/program/card.md").to_owned(),
            ),
            (
                "evals/cards/prose-answer",
                include_str!("../../evals/cards/prose-answer/card.md").to_owned(),
            ),
            (
                "evals/cards/ts",
                include_str!("../../evals/cards/ts/card.md").to_owned(),
            ),
            // The `run_program` arm. A copy like the others, and the
            // one most likely to rot: it is edited only when the
            // transport is being measured, which is rarely.
            (
                "evals/cards/run-program",
                include_str!("../../evals/cards/run-program/card.md").to_owned(),
            ),
        ] {
            assert_eq!(
                edit_verbs(&text),
                registered,
                "{what} and the `Edit` builtins disagree"
            );
        }
    }

    /// **The call card may not describe the other transport.**
    ///
    /// It is made by transforming the notebook card, and the first time
    /// that was done twelve paragraphs survived the transform: `return`
    /// "ends the reply, not just the block it sits in ... not the
    /// blocks below", prose "emitted before the block beneath it runs",
    /// "from inside a block", "stops the block". The batching argument
    /// — the one sentence that says a program may do many things — was
    /// dropped entirely. Five runs failed identically before anyone
    /// read the card, and the failure looked like the transport's
    /// fault.
    ///
    /// `Edit.extractBlock` and `extractByIndent` are about text, not
    /// cells, and are allowed their word.
    #[test]
    fn the_call_card_says_nothing_about_blocks_or_cells() {
        let card = include_str!("../../evals/cards/run-program/card.md");
        // **Whole words.** `blocked on` and `extractBlock` both hold
        // the substring and neither is about a cell; matching loosely
        // makes the guard cry wolf and get deleted.
        let allowed = ["brace-delimited block", "indented block", "A fenced block"];
        let offenders: Vec<&str> = card
            .lines()
            .filter(|l| {
                let words: Vec<String> = l
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .map(|w| w.to_ascii_lowercase())
                    .collect();
                let stale = words
                    .iter()
                    .any(|w| matches!(w.as_str(), "block" | "blocks" | "cell" | "cells"))
                    || l.contains("this reply")
                    || l.contains("next reply");
                stale && !allowed.iter().any(|a| l.contains(a))
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "the call card still describes the notebook: {offenders:#?}"
        );
        // And it must carry the argument that a call is not one step,
        // which is what stops the model reverting to short hops.
        assert!(
            card.contains("One call carries a whole program, not one step."),
            "the batching argument is missing from the call card"
        );
    }

    /// **The card cannot under-document `list_agents`.**
    ///
    /// It declared five fields and `serve_agents` returns eight. The
    /// three it omitted were `parent`, `last_answer` and — the one that
    /// matters — **`open`**, the count of questions a child is blocked
    /// on. A supervisor polling `status` alone sees a stalled child as
    /// merely quiet, which is a plausible reason the verb sat unused in
    /// 520 logs: what makes it worth calling was not written down.
    ///
    /// Both directions, because a field named here and not returned is
    /// a program reading `undefined` and believing it.
    #[test]
    fn the_card_declares_the_fields_list_agents_returns() {
        // The shape `host::mod`'s `serve_agents` builds, in its order.
        let returned = [
            "agent",
            "branch",
            "name",
            "charter",
            "parent",
            "status",
            "open",
            "last_answer",
        ];
        let card = card();
        // The return type, not the whole declaration: `opts?: { under?:
        // number; deep?: boolean }` holds semicolons of its own, and
        // slicing to the first one truncates before any field appears.
        let start = card
            .find("declare function list_agents(")
            .expect("the card declares list_agents");
        let decl = &card[start..];
        let body = decl
            .split_once("Array<{")
            .expect("a return type")
            .1
            .split_once("}>")
            .expect("its end")
            .0;
        let declared: std::collections::BTreeSet<&str> = body
            .split(';')
            .filter_map(|f| f.split_once(':'))
            .map(|(name, _)| name.trim())
            .filter(|n| !n.is_empty())
            .collect();
        let expected: std::collections::BTreeSet<&str> = returned.into_iter().collect();
        assert_eq!(
            declared, expected,
            "the card's `list_agents` row and what `serve_agents` builds disagree"
        );
        // And the status words a supervisor branches on.
        // **The statuses that exist.** `Runner::status`'s
        // `unreachable!("returned above")` is a panic message, not a
        // status, and scraping string literals out of that function put
        // it in the card as one.
        for word in [
            "running",
            "thinking",
            "suspended",
            "idle",
            "dormant",
            "queued",
        ] {
            assert!(
                card.contains(&format!("`\"{word}\"`")),
                "the card does not name the status `{word}`"
            );
        }
        assert!(
            !card.contains("returned above"),
            "`returned above` is a panic message in an unreachable arm, not a status"
        );
    }

    /// **The Node paragraph was measured on a broken document, and is
    /// gone.**
    ///
    /// It was added at 48% -> 0%, p<0.00001, on three captures built
    /// with the scripted LLM to save money. Those captures declare a
    /// manifest of exactly one tool:
    ///
    /// ```ts
    /// declare namespace tools { function echo(arg0: unknown): Promise<unknown>; }
    /// ```
    ///
    /// beside worked examples calling `tools.bash` and
    /// `tools.read_file`. The model said so itself, in reasoning I only
    /// read afterwards: *"the tools available in this session are only
    /// `tools.echo`! ... So bash/read_file may not exist."* It reached
    /// for `node:fs` because it had been told it had no file tool — not
    /// because the card was silent about Node.
    ///
    /// Re-run on captures taken from real sessions, with real
    /// manifests: **0/90 without the paragraph, 1/90 with it**, p=1.0.
    /// The problem it fixed does not exist. And the paragraph costs
    /// something: drafting in the reasoning stream ran 19% with it
    /// against 8% without, p=0.047, in the same direction on all three
    /// tasks. Naming a thing at length appears to put it in mind.
    ///
    /// A null would not have been reason enough to remove it — a card
    /// may hold what reasoning says belongs there. Evidence of harm is.
    ///
    /// Pinned as an absence because it is a plausible thing to re-add:
    /// the reasoning for it ("say what does not exist") is sound, and
    /// only a valid document shows that nobody asks.
    #[test]
    fn the_card_does_not_name_node() {
        let card = card();
        for word in ["node:fs", "readFileSync", "require`"] {
            assert!(
                !card.contains(word),
                "the card names {word} again — see this test's comment before keeping it"
            );
        }
    }

    /// **No worked example prints a result's payload.**
    ///
    /// `.content`, `.stdout`, `.stderr` — a result's own bytes. **Not
    /// `.diff`**, and that distinction is the whole correction to why
    /// this test exists.
    ///
    /// It was written believing the one `console.log` in any exemplar
    /// violated the card's ban: `04-many` logged
    /// `console.log(`${p}\n${w.diff}`)`, and a rule contradicted by the
    /// example beside it loses (p=0.0057). But a `diff` is what the
    /// tool computed about a write, not the file — compliant by the
    /// definition on this very line — so there was no contradiction,
    /// and replacing it changed nothing: `dumps` 9/60 against 9/60,
    /// p=1.0000, identical to the sample.
    ///
    /// The exemplar was still worth changing, on other grounds. What it
    /// logs now guards a documented failure: `replace_file` reports
    /// "nothing changed" by *omitting* `diff`, and a `sweep-40` run
    /// wrote bytes identical to disk and told the person it had deleted
    /// 24 helpers that were all still there. `NO CHANGE` in that trace
    /// is that run.
    ///
    /// **So the residual is not reachable by the document.** Three
    /// changes have now missed it — a closing consequence, a flat ban
    /// naming the syntax, and the exemplar — while 9 of 60 first
    /// replies go on printing a whole file. The report advisory is the
    /// remaining lever and it fires a turn too late to test here.
    #[test]
    fn no_exemplar_prints_a_results_payload() {
        for ex in &exemplars() {
            for (i, _) in ex.assistant.match_indices("console.log(") {
                let call = &ex.assistant[i..];
                let call = &call[..call.find('\n').unwrap_or(call.len())];
                for field in [".content", ".stdout", ".stderr"] {
                    assert!(
                        !call.contains(field),
                        "an exemplar prints {field}, which the card forbids: {call}"
                    );
                }
            }
        }
    }

    /// **The index of channels is load-bearing in a way the
    /// declarations below it are not.**
    ///
    /// "What crosses from this reply to the next" lists every way a
    /// value reaches the next reply. Measured 2026-09-24 against a
    /// frozen document (`lab/readverb`), n=20 per arm, deleting one
    /// thing at a time.
    ///
    /// **The baseline is the card of 2026-09-23, not this one.** A
    /// capture replays the card snapshotted in the log's `Agent` event,
    /// so it is the card that run was actually sent — two commits back
    /// from here, before the `console.log` rewrite and before
    /// `history.remove` left. The arms are all derived from that one
    /// baseline by deletion, so the comparisons below hold; the
    /// absolute rates are that card's, and calling them "as shipped"
    /// is how a stale capture quietly becomes a claim about code it
    /// never saw.
    ///
    /// ```text
    ///                           shows   notes  prints
    ///   baseline (2026-09-23)    90%      0%     45%
    ///   − this paragraph         75%     30%     60%
    ///   − the 05-keep exemplar   65%      5%     70%
    ///   − both                   45%     15%     65%
    /// ```
    ///
    /// **Removing both halves `keep`/`peek` adoption** — 90% to 45%,
    /// Fisher p=0.0057, and the median reply in that arm calls
    /// neither. Neither change alone is significant, so the pair earns
    /// its place together and the credit does not divide.
    ///
    /// **The attribution in the commit that added this paragraph is
    /// wrong.** It claimed the paragraph drove `keep`/`peek` adoption;
    /// alone that is p=0.41. What it does on its own is stop the
    /// copying — `history.note` at 0/20 against 6/20, p=0.0202 — which
    /// is the 52 KB-into-one-row failure the live runs were loudest
    /// about. Right paragraph, wrong reason, and the reason is what
    /// the next person would have edited against.
    ///
    /// **The `console.log` block does nothing, pooled.** On *this*
    /// task, reverting it alone moved `shows` 95% -> 82% (p=0.0434) and
    /// `prints` 28% -> 47% (p=0.0588), and it was called the largest
    /// card effect of the week on that basis. Run against three more
    /// task shapes — a rename-and-check, a search, a read-and-explain —
    /// it is null at n=150 per arm: dumps 18% against 15% (p=0.53),
    /// prints 43% against 47% (p=0.49). Its sign flips: on the edit
    /// task the paragraph *raises* dumping, 7% to 30% (p=0.042).
    ///
    /// **So every number above is this prompt's, not the card's.** A
    /// held-still document removes path variance, which is what makes
    /// its p-values mean anything, and replaces it with one task's
    /// idiosyncrasy, which nothing inside the document can reveal. The
    /// baselines alone should have been the warning: `shows` is 95%
    /// here and 20% on a search, `drafts` 5% here and 80% on an edit. Two attempts to improve it both
    /// failed: leading `note` with its distinguishing jobs was null,
    /// and replacing "Loop over two hundred items here, not above" with
    /// a *justification* — that a bounded tail is cheaper than rows —
    /// took `prints` from 15% to 40% — but at n=20 and p=0.155, which
    /// is not a result, and "a prohibition beats an explanation" is not
    /// a law this measured. The prediction it makes was tested and
    /// failed: replacing the paragraph's closing consequence with a
    /// flat ban naming the syntax (`console.log(f.content)`) moved
    /// content-dumping 9/60 to 11/60, p=0.81.
    ///
    /// **What the residual looks like.** On the current card 9 of 60
    /// first replies still print a whole file, 8 more print only counts
    /// and branches, which is the channel's job. Two different wordings
    /// have failed to shift the 9. The paragraph as a whole is worth
    /// its bytes and the sentences inside it are, so far, not
    /// separately measurable — so the next thing to try is not a third
    /// wording but a mechanism: the report already tells a program when
    /// it copied a row's bytes (`copied_note`), and nothing tells it
    /// when it printed them.
    #[test]
    fn the_index_of_channels_names_every_way_a_value_crosses() {
        let card = card();
        let (_, index) = card
            .split_once("## What crosses from this reply to the next")
            .expect("the section itself");
        let index = &index[..index.find("## ").unwrap_or(index.len())];
        for verb in [
            "history.note(v)",
            "history.keep(r)",
            "history.peek(r)",
            "console.log(x)",
            "tell(text)",
        ] {
            assert!(
                index.contains(verb),
                "the index does not list {verb}; a channel the model \
                 cannot find in this list it reaches for by habit"
            );
        }
    }

    /// **Every history verb the card declares is demonstrated by a
    /// worked example, and no example uses one the card does not
    /// declare.**
    ///
    /// The two halves fail differently and both have. `05-keep` was
    /// named for a verb it never called: it read two files and wrote
    /// their contents with `history.note` while the card, three
    /// paragraphs up, said `keep` was how you read. A live
    /// `deepseek-v4-flash` copied 11.7 KB into a row rather than keep
    /// it, and the advisory that fires on exactly that was in the
    /// prompt and ignored. The example beat the rule, which is what it
    /// has done every time it has been measured here.
    ///
    /// The other half is the cheaper failure: an example calling a verb
    /// that has been renamed or withdrawn teaches a name that does not
    /// resolve. `append` was renamed to `note`, and `remove` left the
    /// card when `peek` took its job.
    #[test]
    fn every_history_verb_is_demonstrated_and_every_demonstration_is_declared() {
        let card = card();
        let declared: std::collections::BTreeSet<String> = card
            .lines()
            .filter_map(|l| l.trim().strip_prefix("function "))
            .filter_map(|l| l.split('(').next())
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .collect();
        // The `history` namespace's own verbs: the ones an exemplar
        // reaches through `history.`.
        let used: std::collections::BTreeSet<String> = exemplars()
            .iter()
            .flat_map(|ex| {
                ex.assistant
                    .match_indices("history.")
                    .map(|(i, _)| {
                        ex.assistant[i + "history.".len()..]
                            .chars()
                            .take_while(char::is_ascii_alphanumeric)
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .collect();

        let history_verbs: std::collections::BTreeSet<String> = ["note", "fetch", "keep", "peek"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        for verb in &history_verbs {
            assert!(
                declared.contains(verb),
                "the card stopped declaring history.{verb};                  if that is deliberate, take it out of this list too"
            );
            assert!(
                used.contains(verb),
                "the card declares history.{verb} and no worked example uses it —                  the example is what the model copies"
            );
        }
        for verb in &used {
            assert!(
                declared.contains(verb),
                "an exemplar calls history.{verb}, which the card does not declare"
            );
        }
    }

    /// **Every `tools.X(` a worked example calls is declared in the
    /// manifest that ships with it.**
    ///
    /// The same shape as
    /// [`every_history_verb_is_demonstrated_and_every_demonstration_is_declared`],
    /// one namespace over, and it fails the same way: an example that
    /// calls a tool the reader has not been given teaches a name that
    /// does not resolve — except that here the reader can *see* it does
    /// not resolve, and says so. Three eval documents built from
    /// scripted sessions declared only `tools.echo` while their
    /// examples called `tools.bash` and `tools.read_file`, and the
    /// model spent thousands of reasoning tokens deciding which half of
    /// its own prompt to believe.
    ///
    /// This half holds the *shipped* pairing — the embedded card's
    /// exemplars against the real registry's manifest — which is the
    /// one a renamed or withdrawn tool would break. The scripted
    /// pairing cannot be caught from here, because no test builds it:
    /// it is built by `agent capture` over a `--headless` log, and
    /// [`crate::card::undeclared_example_tools`] is called there.
    #[test]
    fn every_tool_a_worked_example_calls_is_declared() {
        let system = format!(
            "{}{}",
            card(),
            tool_manifest(&crate::host::tools::real_registry(), true)
        );
        let exemplars = exemplars();
        let assistants: Vec<&str> = exemplars.iter().map(|ex| ex.assistant.as_str()).collect();
        let missing = undeclared_example_tools(&system, &assistants);
        assert!(
            missing.is_empty(),
            "a worked example calls {missing:?}, which the manifest shipping with it \
             does not declare — the example is what the model copies, and this one \
             names a tool it will be told it does not have"
        );

        // The detector has to be able to see a call at all: a check
        // that silently matches nothing passes forever.
        let seen = {
            let declared = tools_declared_in(&system);
            assert!(
                declared.contains("bash") && declared.contains("read_file"),
                "the manifest parser found {declared:?}"
            );
            undeclared_example_tools(&card(), &assistants)
        };
        assert!(
            seen.contains(&"bash".to_string()),
            "with the manifest removed the same examples must come back unsatisfied, \
             got {seen:?}"
        );
    }

    #[test]
    fn the_card_names_every_bare_verb() {
        for verb in [
            "tell(",
            "ask(",
            "answer(",
            "spawn(",
            "fork(",
            "namespace history",
            "raise(",
            "resume(",
            "abandon(",
            "list_agents(",
        ] {
            assert!(card().contains(verb), "card is missing {verb}");
        }
        // **`replace` is named; `remove` is not, and the line between
        // them is who can know.** Three of the four runs that rewrote
        // outside compaction were *correcting* a row they had found to
        // be wrong ("superseded — that row's computation was broken").
        // Nothing later can discover that: a compaction handler reads
        // the row, not the world it described. So `replace` stays.
        //
        // `remove`'s case outside compaction was "how a reply stops
        // paying for a file it appended in order to read" — and since
        // `peek`, that decision is made *before* the fact and costs
        // nothing, where removing costs a rewrite of every turn after
        // the row. A verb whose whole job another verb now does
        // earlier and cheaper is one the card can stop advertising;
        // the compaction directive introduces it in full where it is
        // actually the job.
        //
        // What they carry instead is where they are cheap. Changing
        // what a row shows makes every turn after it new text: measured
        // across the kept runs, a rewrite in a conversation under 5k
        // tokens costs nothing (88% cached either way), and one past 5k
        // cuts the cached share to 47.5% and multiplies uncached tokens
        // by 4.4. Recent rows are nearly free; the oldest row in a long
        // conversation is the whole conversation.
        // They used to be withheld, because they were refused outside a
        // compaction program and advertising them would have pointed
        // the model at two verbs that fail where it would first try
        // them. They are not refused any more: a program that has
        // finished with an entry may say so when it knows, which is
        // usually the program that made it rather than a compaction
        // program later with less to go on.
        for member in [
            "function note(",
            "function fetch(",
            "function replace(",
            "function keep(",
            "function peek(",
        ] {
            assert!(
                card().contains(member),
                "the history namespace is missing {member}"
            );
        }
    }

    #[test]
    fn the_card_states_the_response_rule_and_the_ending_rule() {
        // What a reply is, and which of its blocks run.
        assert!(
            card().contains("Your reply is **markdown**, and the code blocks in it run."),
            "{}",
            card()
        );
        assert!(card().contains("is quoted, not run"), "{}", card());
        // And the two halves of the ending, which 27.1 inverted: a
        // program finishing is not the task finishing.
        assert!(card().contains("finish("), "{}", card());
        assert!(
            card().contains("not trying to finish the task in one reply"),
            "{}",
            card()
        );
    }

    /// An exemplar's cells, in order, in one scope — what the notebook
    /// driver hands the compiler.
    /// The program inside a worked example's reply, **cleaned the way
    /// the harness cleans one**.
    ///
    /// The examples ship as rendered turns, so their blocks carry
    /// `【← history[N]】` beside a call — which is not JavaScript and is
    /// not meant to be. A reply is stripped of everything between the
    /// fences before it is logged or compiled
    /// (`Notebook::push_text`), so anything asking whether an example
    /// compiles has to strip it too. Doing otherwise would test a
    /// string no compiler is ever handed.
    fn cells_of(reply: &str) -> String {
        let mut reply = reply.to_owned();
        crate::notebook::strip_annotations_for_test(&mut reply);
        let reply = &reply;
        crate::notebook::split_cells(reply)
            .iter()
            .map(|c| c.slice(reply))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_exemplars_cells_are_valid_javascript() {
        // The one thing in this file that must actually compile. An
        // exemplar's assistant turn is a *reply* — markdown, with the
        // program in fenced cells — so what is held to the compiler is
        // what the notebook driver would hand it: the cells, in order,
        // in one scope.
        for ex in &exemplars() {
            let cells = crate::notebook::split_cells(&ex.assistant);
            assert!(!cells.is_empty(), "an exemplar with no cell: {}", ex.user);
            interp::compile(&cells_of(&ex.assistant))
                .unwrap_or_else(|e| panic!("seed exemplar does not parse: {e:?}"));
        }
    }

    /// Parsing is not enough. Four exemplars have shipped with bugs a
    /// parse could never catch — a `.output` field the `bash` tool
    /// does not return, arguments in the wrong order, a version used
    /// after the program's own write moved it on — and each one taught
    /// the model the bug, because an exemplar outranks the manifest
    /// that says otherwise. So every exemplar is *run* here, against
    /// stub tools shaped like the real registry's results, and must
    /// reach the end without trapping.
    ///
    /// The stubs answer the shape, not the content: a trap is a real
    /// defect in the exemplar, but a passing run says only that the
    /// program is well-formed against the tools it calls — never that
    /// its judgement is right.
    #[test]
    fn the_exemplars_run_to_completion_against_stub_tools() {
        for ex in &exemplars() {
            run_against_stubs(&cells_of(&ex.assistant))
                .unwrap_or_else(|e| panic!("exemplar for {:?} trapped: {e}", ex.user));
        }
    }

    /// One stub result per verb the exemplars call, in the shape the
    /// real registry documents (`host::tools::real_registry`): `bash`
    /// resolves to `{ status, stdout, stderr }`, a read to
    /// `{ content, version }`, a write to a fresh `{ version }`.
    fn stub_result(name: &str, args: &[interp::Value]) -> serde_json::Value {
        let path = match args.first() {
            Some(interp::Value::String(s)) => s.to_string(),
            _ => String::new(),
        };
        match name {
            // A grep-shaped listing: `path:lineno:text`, which also
            // reads as a plain file list for the exemplars that want one.
            "bash" => json!({
                "status": 0,
                "stdout": "src/lib.rs:2:#[ignore]\nsrc/other.rs:9:#[ignore]\n",
                "stderr": "",
            }),
            "read_file" if path.ends_with(".json") => {
                json!({ "content": "{\"retries\": 3}", "version": "v1" })
            }
            // Carries the placeholder tokens the exemplars edit against
            // (`OLD`) as well as a realistic-looking marker, so an
            // exemplar can demonstrate `Edit.replaceOnce` without
            // putting a real-looking literal on the card — a live run on
            // 2026-09-17 copied an exemplar's literals verbatim into a
            // repo that had none of them.
            "read_file" => json!({
                "content": "// one\nOLD\n#[ignore]\nfn thing() {}\n",
                "version": "v1",
            }),
            "replace_file" | "write_file" | "create_file" => json!({ "version": "v2" }),
            "ask" => json!("4"),
            // A fetched row answers in the shape the row's own call
            // did — `09-act` fetches the read it was shown and edits
            // `content`. Without this the stub hands back null and the
            // example traps on a property of nothing, which says
            // something about the stub and nothing about the example.
            "fetch_history" => json!({ "content": "{ \"retries\": 3 }\n", "version": "v1" }),
            "spawn" | "fork" => json!({ "agent": 2 }),
            // **Idle, so a supervision loop ends.** The seventh
            // exemplar waits while helpers work; against a roster that
            // never settles it would run until the test harness gave
            // up, which is a hang rather than a failure and reads as
            // neither.
            "list_agents" => json!([{
                "agent": 2, "branch": 2, "name": null, "charter": "",
                "parent": 1, "status": "idle", "open": 0, "last_answer": null,
            }]),
            _ => json!(null),
        }
    }

    /// How an exemplar ended — the thing 27.1 made a decision rather
    /// than an omission, so it is worth reading back off a real run
    /// instead of grepping the source for the word.
    struct Ending {
        /// `finish(text)` was called: the task is over.
        finished: bool,
        /// `history.note` was called: something was handed to the
        /// reply after this one. A reply has no `return` (D5), so this
        /// is the other way one can end on purpose.
        appended: bool,
    }

    /// Drive one program on a bare VM: every call answered by
    /// [`stub_result`], every `raise` resumed with a plausible answer.
    /// Deliberately not the real machine — this checks the program
    /// against its tools, and wants no conversation around it.
    fn run_against_stubs(src: &str) -> Result<Ending, String> {
        use interp::{StepResult, VM};
        let program = interp::compile(src).map_err(|e| format!("{e:?}"))?;
        let mut vm = VM::for_program(program, serde_json::Value::Null)
            .map_err(|e| format!("could not start: {e:?}"))?;
        // `finish()` sets a flag and lets the program run on, so the
        // fact is read off the VM when the program ends rather than
        // from a `StepResult` — which is the whole point of it being a
        // flag.
        let mut appended = false;
        loop {
            match vm.step(u64::MAX).map_err(|e| format!("{e:?}"))? {
                StepResult::Done { .. } => {
                    return Ok(Ending {
                        finished: vm.finished,
                        appended,
                    });
                }
                StepResult::Pending { calls } => {
                    for call in calls {
                        let result = stub_result(&call.name, &call.args);
                        let value = vm
                            .json_to_stack_value(&result, 0)
                            .map_err(|e| format!("{e:?}"))?;
                        vm.resolve_promise(call.promise, value)
                            .map_err(|e| format!("{e:?}"))?;
                    }
                }
                // The settle-at-dispatch verbs come back here instead of
                // through the outbox: they answer into the frame that
                // called them, so the stub pushes the value rather than
                // settling a promise.
                StepResult::Settle { call } => {
                    if call.name == crate::machine::TOOL_NOTE_HISTORY {
                        appended = true;
                    }
                    let result = stub_result(&call.name, &call.args);
                    let value = vm
                        .json_to_stack_value(&result, 0)
                        .map_err(|e| format!("{e:?}"))?;
                    vm.push_settled(value).map_err(|e| format!("{e:?}"))?;
                }
                StepResult::Raise { .. } => {
                    let value = vm
                        .json_to_stack_value(&json!("backup-2.txt"), 0)
                        .map_err(|e| format!("{e:?}"))?;
                    vm.resume_raise(value);
                }
                StepResult::OutOfFuel => {}
                // An exemplar is one whole program compiled one-shot, so
                // no `Instr::Pause` is ever in its stream.
                StepResult::Paused { .. } => {
                    return Err("an exemplar paused: it was not compiled one-shot".into());
                }
            }
        }
    }

    /// **Every exemplar ends on purpose.** Since 27.1 a program that
    /// runs off the end does not stop — the next one is written — so
    /// falling off the end is no longer an ending at all, and an
    /// exemplar that did it would be teaching the accident the whole
    /// change exists to prevent. Each one either calls `finish(text)`, because
    /// its task is finished, or `history.note`s the thing the next
    /// reply continues from.
    #[test]
    fn every_exemplar_ends_on_purpose() {
        for ex in &exemplars() {
            // An exemplar is a *reply*: markdown, with its program in
            // fenced cells. Running it means running its cells, in
            // order, in one scope — which is what the notebook driver
            // does with the real thing.
            let source = cells_of(&ex.assistant);
            // **An exemplar with no cells has already ended.** A reply
            // that runs nothing rests the branch (D4, and card.md's own
            // "a reply with no code blocks in it rests the branch");
            // there is no end to run off. The rule below is about a
            // *program* falling through, and applying it to a reply
            // with no program in it is what made every exemplar open
            // with a fence.
            if source.trim().is_empty() {
                continue;
            }
            let ending = run_against_stubs(&source)
                .unwrap_or_else(|e| panic!("exemplar for {:?} trapped: {e}", ex.user));
            // **The two endings a reply has.** `finish(text)` says the task is
            // finished; `history.note` hands a finding to the reply
            // after this one and rests the branch (D4). There is no
            // `return` to be the second of those any more, and an
            // exemplar that does neither demonstrates a reply that found
            // something and threw it away.
            //
            // Read off the run, not off the text: a call inside a
            // branch never taken is not an ending.
            assert!(
                ending.finished || ending.appended,
                "exemplar for {:?} neither finishes nor hands anything on",
                ex.user
            );
        }
    }

    /// **The eval card variants end on purpose too.** They are inputs
    /// to measurements, and a broken one does not fail loudly — it
    /// produces a *worse number*, which is indistinguishable from a
    /// real finding until someone reads the logs. Two exemplars have
    /// already shipped teaching a field the tools do not return; that
    /// cost a day of attributing the result to the card's prose.
    ///
    /// The check is textual and deliberately coarse — does an ending
    /// appear in the source at all — because these are not compiled
    /// against a real machine here and a stricter reading would start
    /// asserting things about control flow that only a run can settle.
    /// It catches the one shape that is now simply wrong: an exemplar
    /// with no ending anywhere in it.
    ///
    /// Only the ending is checked, not runnability: `sketch` exists to
    /// test the opposite hypothesis — exemplars as *shape*, with
    /// `path(site)` and `needleFor(site)` deliberately undefined — so
    /// running these against stubs would be asserting the very thing
    /// that variant is there to question. The shipped exemplars are
    /// held to both (`every_exemplar_ends_on_purpose`); these to the
    /// one that is about the card's claims rather than its code.
    ///
    /// Skipped silently when `evals/` is not beside the crate (a
    /// published tarball, a sparse checkout): its absence is not a
    /// defect in the agent.
    #[test]
    fn every_eval_card_variant_ends_on_purpose() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("evals/cards");
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };
        let mut checked = 0;
        for entry in entries.filter_map(|e| e.ok()) {
            let dir = entry.path();
            if !dir.join("card.md").is_file() {
                continue;
            }
            let card = load_from(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
            // **Whether "no fences" means prose is a fact about the
            // card, not about one exemplar.** A bare-JS variant's
            // assistant turns are programs whole; a markdown one's are
            // replies whose ```js blocks are the program — and in a
            // markdown card an exemplar with no block is a reply that
            // runs nothing, which is a shape worth demonstrating
            // (card.md: "a reply with no code blocks in it rests the
            // branch"). Read off the variant's other exemplars, so a
            // cell-less one is not compiled as JavaScript and told its
            // em-dash is an invalid character.
            let markdown = card
                .exemplars
                .iter()
                .any(|ex| !crate::notebook::split_cells(&ex.assistant).is_empty());
            for ex in &card.exemplars {
                // **What "the assistant turn" *is* depends on the
                // transport the variant is for.** Under
                // `Transport::Notebook` it is markdown whose ```js blocks
                // are the program, so compiling the whole thing as
                // JavaScript would fail on the prose. A variant that
                // contains a cell is read the way that transport reads
                // it: split first, then compile each cell.
                let cells = crate::notebook::split_cells(&ex.assistant);
                if cells.is_empty() && markdown {
                    // A reply that runs nothing. Nothing to compile,
                    // and nothing to end — it has already ended.
                    continue;
                }
                if cells.is_empty() {
                    interp::compile(&ex.assistant).unwrap_or_else(|e| {
                        panic!(
                            "{} exemplar for {:?} does not parse: {e:?}",
                            dir.display(),
                            ex.user
                        )
                    });
                } else {
                    for (i, cell) in cells.iter().enumerate() {
                        let src = cell.slice(&ex.assistant);
                        interp::compile(src).unwrap_or_else(|e| {
                            panic!(
                                "{} exemplar for {:?} cell {i} does not parse: {e:?}",
                                dir.display(),
                                ex.user
                            )
                        });
                    }
                }
                // And what counts as an *ending* depends on it too. A
                // notebook cell cannot `return` at all (D5), so the verb
                // that hands work to the next reply is `history.note`.
                // A variant's exemplar with no cells is a rest, same
                // as the shipped card's — see
                // `every_exemplar_ends_on_purpose`.
                let ends = cells.is_empty()
                    || ex.assistant.contains("finish(")
                    || ex.assistant.contains("stop(")
                    || ex.assistant.contains("history.note")
                    || ex
                        .assistant
                        .lines()
                        .any(|l| l.trim_start().starts_with("return"));
                assert!(
                    ends,
                    "{} exemplar for {:?} has no ending in it at all — \
                     under automatic continuation, running off the end \
                     is not one",
                    dir.display(),
                    ex.user
                );
            }
            checked += 1;
        }
        assert!(
            checked >= 2,
            "found only {checked} card variants under {root:?}"
        );
    }

    /// **Five positional exemplar tests lived here** — the second
    /// demonstrates `ask` inline, the fourth calls `raise`, the fifth
    /// uses `note_history` as a short projection, and every one opens
    /// with a `tell`. They pinned a ten-exemplar card whose job was to
    /// teach the API by demonstration.
    ///
    /// The card teaches the API by *declaration* now, and the two
    /// exemplars that remain exist for the one thing a declaration
    /// cannot show: what a finished assistant turn looks like. Asserting
    /// that the fourth one calls `raise` is meaningless when there is no
    /// fourth one, and adding exemplars back to satisfy the test would
    /// be the test choosing the design.
    ///
    /// What replaced them is below and is stricter about the thing that
    /// now matters: both exemplars parse, both run to completion against
    /// stub tools, and both end on purpose.
    /// **One name per thing, across the card and everything the harness
    /// prints.** The model is taught a vocabulary by the card and then
    /// reads the harness's own text every turn; where the two disagree
    /// it has to hold two names for one idea. Two had drifted apart by
    /// 2026-09-18: the report advertised `fetch_history(id)` (37 times
    /// in a single session) while the card taught the `history.*`
    /// namespace that commit 235f823 made the surface, and the request
    /// tail still called history rows "artifacts" — the name from the
    /// artifact/menu machinery that phase deleted.
    #[test]
    fn the_harness_and_the_card_use_one_vocabulary() {
        // Only the shipping half of each file: a `#[cfg(test)]` module
        // builds scripted programs whose text is input to the harness,
        // not output from it.
        //
        // **Every such module, not the file up to the first one.**
        // This truncated at the first `#[cfg(test)]`, and `machine.rs`
        // has one at line 487 of 11,324 — a `const TEST_BUDGET`, not
        // the test module. So the guard read 4% of the file it names
        // and called the other 96% checked. Both leaks found on
        // 2026-09-24 sat in that blind 96%: `keep_history` in the
        // keep/peek refusal and `note_history` in the one below it.
        //
        // Not every `#[cfg(test)]` opens a block — a `const` is one
        // line ending in `;`, and skipping to the next `}` past it
        // would swallow thousands of shipping lines, which is the same
        // bug wearing the other hat. So: an item ending in `;` is
        // skipped alone, and an item that opens a brace is skipped to
        // the `}` **at the attribute's own indentation**.
        //
        // Indentation, not the first column, because a `#[cfg(test)]`
        // inside an `impl` block closes at that block's indent — and
        // matching on column 0 instead walks past it to the end of the
        // `impl`, taking every shipping method with it. That is not
        // hypothetical: adding one `#[cfg(test)]` constructor to
        // `Runner` on 2026-09-24 hid the keep/peek refusal from this
        // scan, and the reach assertion below is what said so.
        let shipping = |f: &str| -> String {
            let mut out = String::new();
            let mut lines = f.lines().peekable();
            while let Some(line) = lines.next() {
                if line.trim_start() != "#[cfg(test)]" {
                    out.push_str(line);
                    out.push('\n');
                    continue;
                }
                let indent = &line[..line.len() - line.trim_start().len()];
                let closes = format!("{indent}}}");
                let Some(item) = lines.next() else { break };
                if !item.contains('{') && item.trim_end().ends_with(';') {
                    continue;
                }
                for rest in lines.by_ref() {
                    if rest == closes {
                        break;
                    }
                }
            }
            out
        };
        let harness: String = [include_str!("report.rs"), include_str!("machine.rs")]
            .iter()
            .map(|f| shipping(f))
            .collect::<Vec<_>>()
            .concat();
        // **The scan proves it reached the far side of the file.**
        // A guard that silently reads nothing passes forever; this one
        // did, for months. These two strings ship from past the line
        // the old scan stopped at, and are what it should have caught.
        for deep in ["takes one row", "needs the value to carry over"] {
            assert!(
                harness.contains(deep),
                "the scan stopped short of {deep:?} — it is reading less \
                 of the harness than it claims, which is how the leaks \
                 it is meant to catch got through"
            );
        }
        // Only what the model reads: quoted strings, not identifiers,
        // doc comments, or the constants the lowering is named by.
        let model_facing: Vec<&str> = harness
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///") && l.contains('"')
            })
            .collect();
        // **Every verb whose two spellings differ, not just the one
        // that was caught.** This list held `fetch_history` alone for
        // months while `keep_history` leaked in the refusal a live run
        // was handed on 2026-09-24 — told to fix `keep_history(result)`
        // for a call it had spelled `history.keep(…)`. A guard that
        // covers one of six is why the other five were invisible.
        for (stale, instead) in [
            ("fetch_history(", "history.fetch("),
            ("note_history(", "history.note("),
            ("keep_history(", "history.keep("),
            ("peek_history(", "history.peek("),
            ("remove_history(", "history.remove("),
            ("replace_history(", "history.replace("),
            ("artifacts on this branch", "rows on this branch"),
        ] {
            let offenders: Vec<&&str> = model_facing
                .iter()
                .filter(|l| l.contains(stale) && !l.contains("TOOL_"))
                .collect();
            assert!(
                offenders.is_empty(),
                "the model is shown {stale:?} but taught {instead:?}: {offenders:?}"
            );
        }
    }

    #[test]
    fn the_exemplars_demonstrate_the_endings_and_the_shapes() {
        let ex = exemplars();
        assert_eq!(ex.len(), 9, "nine, and each earns its place");

        assert!(
            with("Edit.replaceOnce").contains("finish()")
                && with("Edit.replaceOnce").contains("tell("),
            "the first ends a finished task, and says the answer on its way out: {}",
            with("Edit.replaceOnce")
        );
        // **`history.note`, not `finish()`.** What the second
        // exemplar demonstrates is handing a finding on to the next
        // reply and *not* ending the task.
        assert!(
            with("mentions_old_host").contains("history.note")
                && !with("mentions_old_host").contains("finish("),
            "the second hands on and does not stop: {}",
            with("mentions_old_host")
        );
        // **`choose`, not `ask`.** The awaited value is the whole point
        // of the third exemplar: a free-form `ask` answered in prose
        // cannot be compared with `===` or written into a file, and an
        // exemplar that does so teaches the one mistake this pair of
        // verbs exists to prevent. `choose` promises one of the offered
        // strings, so the `await`, the `===` and the edit are all
        // honest — and the case where the person answers outside the
        // set does not appear here because the program does not handle
        // it: it arrives as a resumable condition, and the program
        // written *then* is the one that judges their words.
        assert!(
            with("await choose(").contains("await choose(")
                && with("await choose(").contains("===")
                && with("await choose(").contains("finish("),
            "the third offers a bounded choice and acts on the answer: {}",
            with("await choose(")
        );
        // **The fifth appends twice, and that is the point.** A return
        // is one row however much is packed into it, so a later
        // compaction takes all of it or rewrites all of it; two appends
        // are two rows, and the one finished with can go while the
        // other stays exact. Two weaker arguments were tried and
        // dropped: that a program "only gets one return" (it can return
        // an object holding everything) and that a return "only reaches
        // the next program" (every past return still renders). What is
        // left is granularity, and landing before the program ends. A
        // `return`
        // is a promise the program has to live to keep; `history.note`
        // is already on the log the moment it is called, and survives
        // the program's own trap — verified on a real run, where note
        // #551 outlived the exception that killed the program holding
        // it. That is the case the card never made, and the failure it
        // describes is one we watched: a dead-code run worked out
        // correctly which attributes were pointless, trapped on
        // `fmt is not defined`, and lost the whole analysis.
        assert!(
            with("why_check_fails").matches("history.note").count() == 2,
            "the fifth appends separately, a row each: {}",
            with("why_check_fails")
        );
        // Per-item findings belong in `console.log`, not `append`: 200
        // appends would be 200 permanent rows, which is the firehose
        // the card warns about, while the console's display is bounded
        // and its record stays whole and fetchable.
        // **The sixth forks and points; the seventh spawns and waits.**
        //
        // Both were written wrong first. The sixth keyed its row `{a,
        // b, c}` while the card two inches above says to use the name
        // the thing already has — and an exemplar beats a rule, so it
        // would have taught the opposite. The seventh waited by polling
        // `list_agents` until nothing was `running`, which a freshly
        // told helper answers `queued` at best and, before that status
        // existed, `idle`: it decided the work was done before it had
        // begun. `ask` is the wait, and it cannot race.
        // **And the rows it points at are the reads themselves.** It
        // used to copy all three files into a note and point the forks
        // at that, which is the same bytes twice — a result carries the
        // id of the row it landed on, so `f.id` is the row, and a live
        // `deepseek-v4-flash` run on 2026-09-23 reproduced the copy
        // exactly, keys and all, for two 26 KB files.
        assert!(
            with("fork()").contains("fork()")
                && with("fork()").contains("f.id")
                && with("fork()").contains("history.fetch")
                && !with("fork()").contains("history.note"),
            "the sixth forks and points at the rows the reads already are: {}",
            with("fork()")
        );
        assert!(
            with("spawn(").contains("spawn(")
                && with("spawn(").contains("ask(")
                && !with("spawn(").contains("list_agents"),
            "the seventh waits with ask, not by polling a roster: {}",
            with("spawn(")
        );
        assert!(
            with("Edit.replaceAll").contains("console.log")
                && !with("Edit.replaceAll").contains("history.note"),
            "the loop prints per item rather than appending: {}",
            with("Edit.replaceAll")
        );
        // **The fourth is the whole structural argument for code mode**
        // — N items in one completion, where a tool loop spends N round
        // trips — and nothing showed it. Measured across 3,071 programs
        // on 2026-09-17: 15% contain a loop at all and 5% make parallel
        // calls, so the shape the design exists for is the shape the
        // model almost never reaches for. It carries both in one
        // program: enumerate, read in parallel, edit each, verify once.
        assert!(
            with("Edit.replaceAll").contains("Promise.all")
                && with("Edit.replaceAll").contains("for ("),
            "the fourth does many at once: {}",
            with("Edit.replaceAll")
        );
        // **The finishing one changes something and checks it.** It used
        // to be `bash("make check")` → `tell` → `finish(text)`, against the
        // prompt "is the build green?" — right for that prompt, and
        // structurally identical to the dominant failure: measured on
        // 2026-09-17, 45% of programs read something, wrote nothing,
        // carried nothing forward and did not finish, 140 of those 155
        // ending by telling the user. The exemplars are two turns of a
        // 10 KB card and the only place the model sees the work done
        // rather than described, so one of them does the work.
        assert!(
            with("Edit.replaceOnce").contains("replace_file")
                && with("Edit.replaceOnce").contains("bash"),
            "the first edits and then runs the thing that would fail: {}",
            with("Edit.replaceOnce")
        );
        // **Whatever finishes, speaks.** A reply that rests having told
        // nobody anything is a run that ended without a word (measured
        // at 1 in 12 runs, and 4 in 12 once a crossing-table line
        // talked the models out of `tell`). The verb carried the words
        // itself for a while, which made the pairing its arity; now it
        // carries nothing and the pairing is back to being a habit the
        // exemplars teach — so every exemplar that finishes says
        // something first, and the harness refuses a silent rest.
        for e in &ex {
            if e.assistant.contains("finish()") {
                assert!(
                    e.assistant.contains("tell("),
                    "an exemplar that finishes says the answer on its way out: {}",
                    e.assistant
                );
            }
        }
        // **And a failed check is shown, not just the good path.** It
        // is the commonest thing a reply has to say, and the exemplars
        // are the only place the model sees what to do about it:
        // `return` the reason, rather than a `finish()` claiming the
        // change landed. Two of the five demonstrate it, on the branch
        // right before the `tell` they would otherwise have reached.
        let returning = ex
            .iter()
            .filter(|e| e.assistant.contains("if (") && e.assistant.contains("return "))
            .count();
        assert!(
            returning >= 2,
            "the exemplars return on a failed check in {returning} of {} — \
             the finishing shape is the only one demonstrated",
            ex.len()
        );
        // **The eighth polls; the seventh must not.** They look alike
        // and are opposites. A helper you spawned settles its own `ask`,
        // so polling a roster for it is a race — the seventh waited on
        // `list_agents` until nothing was `running`, which a freshly
        // told helper answers `queued` at best, and decided the work was
        // done before it had begun (6254690). State out in the world
        // announces nothing, so looking again *is* the only way to know,
        // and `wait_until` is what makes the looking bounded.
        //
        // That removal left `wait_until` with no worked example at all,
        // and across every run logged to 2026-09-24 nothing has ever
        // called it. An exemplar beats a rule every time the two have
        // been measured here (p=0.0057), so a verb with no exemplar is
        // a verb the model does not have.
        assert!(
            with("tools.wait_until(").contains("tools.wait_until(")
                && with("tools.wait_until(").contains("for (")
                && !with("tools.wait_until(").contains("list_agents")
                && !with("tools.wait_until(").contains("spawn("),
            "the eighth polls the world on a bounded loop, and is not about helpers: {}",
            with("tools.wait_until(")
        );
        assert!(
            with("tools.wait_until(").contains("return "),
            "and says so when it runs out of looks rather than ending quiet: {}",
            with("tools.wait_until(")
        );

        // Short enough to be a shape rather than a technique to copy —
        // a live run on 2026-09-17 reproduced a long exemplar verbatim,
        // invented names and all, into a repo that had none of them. The
        // cap counts the whole reply now, prose and fences included, so
        // it is larger than the 400 it was when an exemplar was bare
        // JavaScript; the code inside is no longer than it was.
        //
        // It moved to 740 for an afternoon, when `stop(reason)` arrived
        // and the longest exemplar gained the branch where the check
        // fails. That was the wrong fix for the right instinct: the
        // exemplar was long because its loop was written out by hand,
        // and `hits.entries()` — which the dialect has always had, and
        // which a live model reached for unprompted — says the same
        // thing shorter. A measured guard is not the thing to move when
        // the code under it can be better instead.
        // **900, because the turn now carries the harness's own
        // annotations.** The guard is about the *program* — an example
        // long enough to copy is one the model copies instead of
        // writing — and what ships is the rendered turn, which adds a
        // `【↓ history[N]】` above every block and prose segment and a
        // `【← history[N]】` beside every logged call. Across the nine
        // that is ~90 bytes an example of text the model did not
        // write and cannot make shorter. The bound on the part that
        // *is* the lesson has not moved.
        for e in &ex {
            assert!(
                e.assistant.len() < 900,
                "an exemplar long enough to copy: {} bytes",
                e.assistant.len()
            );
        }
    }

    #[test]
    fn the_exemplars_assistant_turn_has_no_entry_header_and_no_fence() {
        // Step B1: an assistant turn is bare source, nothing else —
        // each exemplar must model that, not just the card's prose
        // about it.
        for ex in &exemplars() {
            assert!(!ex.assistant.starts_with("```"));
            assert!(!ex.assistant.starts_with('['));
        }
    }

    /// **The notebook card's exemplars are notebooks.** They are not under
    /// `the_exemplars_*` above, which cover the built-in card — the control
    /// arm for 25.8, which must not move — so the job those tests do is
    /// done here for the variant instead.
    ///
    /// Three things, and the first is the one a `Transport::Program` reader
    /// would miss: an exemplar whose fences are wrong, or tagged something
    /// other than ```js, splits into **no cells at all**. It would still
    /// parse as prose, still look right in a diff, and teach the model a
    /// reply that does nothing — and a broken eval input does not fail
    /// loudly, it produces a worse number, which is indistinguishable from
    /// a real finding until someone reads the logs.
    #[test]
    fn the_notebook_cards_exemplars_split_into_cells_that_compile() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("evals/cards/notebook");
        // Skipped silently when `evals/` is not beside the crate (a
        // published tarball, a sparse checkout), like its neighbour above.
        if !dir.join("card.md").is_file() {
            return;
        }
        let card = load_from(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        assert_eq!(card.exemplars.len(), 7, "seven, each keeping its own job");

        for ex in &card.exemplars {
            let cells = crate::notebook::split_cells(&ex.assistant);
            assert!(
                !cells.is_empty(),
                "exemplar for {:?} has no executable cell — it would run \
                 nothing and rest the branch",
                ex.user
            );
            for (i, cell) in cells.iter().enumerate() {
                let src = cell.slice(&ex.assistant);
                interp::compile(src).unwrap_or_else(|e| {
                    panic!(
                        "exemplar for {:?} cell {i} does not compile: {e:?}",
                        ex.user
                    )
                });
            }
        }
    }

    /// And they run — as one paused compilation, the way the transport runs
    /// them, not as N separate programs.
    ///
    /// This is the check that catches what a per-cell compile cannot: a
    /// second cell naming something the first never bound, or redeclaring
    /// something it did. Each cell compiles alone either way; only feeding
    /// them to one `Repl` in order can tell.
    #[test]
    fn the_notebook_cards_exemplars_run_as_one_paused_compilation() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("evals/cards/notebook");
        if !dir.join("card.md").is_file() {
            return;
        }
        let card = load_from(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        for ex in &card.exemplars {
            run_notebook_against_stubs(&ex.assistant)
                .unwrap_or_else(|e| panic!("notebook exemplar for {:?}: {e}", ex.user));
        }
    }

    /// **Every notebook exemplar ends on purpose too**, and under this
    /// transport there are only two ways to: `finish(text)`, because the task is
    /// finished, or `history.note`, because something is being handed to
    /// the next reply. `return` is not one — a cell cannot (D5) — so the
    /// third option the shipped exemplars have is simply gone.
    #[test]
    fn every_notebook_exemplar_ends_on_purpose() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("evals/cards/notebook");
        if !dir.join("card.md").is_file() {
            return;
        }
        let card = load_from(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        for ex in &card.exemplars {
            let finishes = ex.assistant.contains("finish(");
            let hands_on = ex.assistant.contains("history.note");
            assert!(
                finishes || hands_on,
                "notebook exemplar for {:?} neither finishes nor hands on",
                ex.user
            );
            assert!(
                !ex.assistant
                    .lines()
                    .any(|l| l.trim_start().starts_with("return ")),
                "notebook exemplar for {:?} has a top-level `return`, which \
                 a cell cannot do",
                ex.user
            );
        }
    }

    /// The jobs the five exemplars do, kept from the shipped card — the
    /// reasoning in `the_exemplars_demonstrate_the_endings_and_the_shapes`
    /// is the part worth preserving, and porting a form without its purpose
    /// would lose it.
    #[test]
    fn the_notebook_exemplars_keep_the_jobs_they_had() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("evals/cards/notebook");
        if !dir.join("card.md").is_file() {
            return;
        }
        let _ex = load_from(&dir).unwrap().exemplars;

        // The first finishes a task it actually changed, and checks the
        // change by running the thing that would fail.
        assert!(
            with("Edit.replaceOnce").contains("finish(")
                && with("Edit.replaceOnce").contains("tools.replace_file"),
            "the first changes something and finishes: {}",
            with("Edit.replaceOnce")
        );
        // The second hands on without stopping — which is now `append`,
        // there being no `return`.
        assert!(
            with("mentions_old_host").contains("history.note")
                && !with("mentions_old_host").contains("finish("),
            "the second hands on and does not stop: {}",
            with("mentions_old_host")
        );
        // The third offers a *bounded* choice and acts on the answer. The
        // awaited value is the point: a free-form `ask` answered in prose
        // could not be compared with `===`. And it guards the skip with
        // `else` — which under this transport is not merely tidy: `finish(text)`
        // stops nothing, so a guard that used it would write the file it
        // meant to leave alone (D8).
        assert!(
            with("await choose(").contains("await choose(")
                && with("await choose(").contains("===")
                && with("await choose(").contains("} else {")
                && with("await choose(").contains("finish("),
            "the third offers a bounded choice and guards with else: {}",
            with("await choose(")
        );
        // The fourth does many at once — the structural argument for code
        // mode — and prints per item rather than appending 200 rows.
        assert!(
            with("Edit.replaceAll").contains("Promise.all")
                && with("Edit.replaceAll").contains("for (")
                && with("Edit.replaceAll").contains("console.log")
                && !with("Edit.replaceAll").contains("history.note"),
            "the fourth does many at once and prints per item: {}",
            with("Edit.replaceAll")
        );
        // It is also the one that demonstrates the shared scope: it binds
        // in one cell and uses the binding in the next, which is the thing
        // about this transport a declaration cannot show.
        let cells = crate::notebook::split_cells(&with("Edit.replaceAll"));
        assert!(cells.len() >= 2, "the fourth spans two cells");
        assert!(
            cells[0]
                .slice(&with("Edit.replaceAll"))
                .contains("const hits")
                && cells[1]
                    .slice(&with("Edit.replaceAll"))
                    .contains("hits.length"),
            "the fourth binds in one cell and reads it in the next"
        );
        // The fifth keeps two rows rather than one fat value, so a later
        // compaction can drop one and leave the other exact.
        assert!(
            with("why_check_fails").matches("history.note").count() >= 2,
            "the fifth appends separately, a row each: {}",
            with("why_check_fails")
        );
    }

    /// Drive a whole notebook reply the way the transport does: one
    /// `ReplCore` fed each cell in turn, every call answered by
    /// [`stub_result`], every `raise` resumed. A cell ends at a `Pause`;
    /// the reply ends with the run's own `Return(0)`.
    fn run_notebook_against_stubs(markdown: &str) -> Result<(), String> {
        use interp::{StepResult, VM};
        let cells = crate::notebook::split_cells(markdown);
        let mut buffer = crate::notebook::ParseBuffer::new(markdown);
        let mut vm = VM::for_incremental(serde_json::Value::Null, serde_json::Value::Null)
            .map_err(|e| format!("{e:?}"))?;
        let mut core = interp::ReplCore::new();

        let mut fed = 0usize;
        loop {
            match vm.step(u64::MAX).map_err(|e| format!("{e:?}"))? {
                StepResult::Done { .. } => return Ok(()),
                StepResult::Paused { .. } => {
                    if fed < cells.len() {
                        let live = buffer.focus(cells[fed]);
                        core.push(&mut vm, live).map_err(|d| {
                            d.iter()
                                .map(|x| x.render(live))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })?;
                        fed += 1;
                    } else {
                        let blank = buffer.clear();
                        core.close(&mut vm, blank).map_err(|d| format!("{d:?}"))?;
                    }
                }
                StepResult::Pending { calls } => {
                    for call in calls {
                        let result = stub_result(&call.name, &call.args);
                        let value = vm
                            .json_to_stack_value(&result, 0)
                            .map_err(|e| format!("{e:?}"))?;
                        vm.resolve_promise(call.promise, value)
                            .map_err(|e| format!("{e:?}"))?;
                    }
                }
                StepResult::Settle { call } => {
                    let result = stub_result(&call.name, &call.args);
                    let value = vm
                        .json_to_stack_value(&result, 0)
                        .map_err(|e| format!("{e:?}"))?;
                    vm.push_settled(value).map_err(|e| format!("{e:?}"))?;
                }
                StepResult::Raise { .. } => {
                    let value = vm
                        .json_to_stack_value(&json!("backup-2.txt"), 0)
                        .map_err(|e| format!("{e:?}"))?;
                    vm.resume_raise(value);
                }
                StepResult::OutOfFuel => {}
            }
        }
    }
}
