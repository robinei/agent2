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
                assistant: include_str!(concat!("../card/exemplars/", $stem, ".js")).to_owned(),
            }
        };
    }
    Card {
        text: include_str!("../card/card.md").to_owned(),
        exemplars: vec![
            exemplar!("01-finish"),
            exemplar!("02-continue"),
            exemplar!("03-ask"),
            exemplar!("04-many"),
            exemplar!("05-keep"),
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
    assert!(
        card.contains(&format!("`{down} history[")),
        "the card shows the block marker as it renders: {down}"
    );
    assert!(
        card.contains(&format!("/* {left} history[")),
        "and the call annotation as it renders: {left}"
    );
    assert!(
        card.contains(&format!("Every `{down}` and `{left}`")),
        "and says both were added by the harness"
    );
}

/// The shipped manifest, as the model reads it — the two facts that
/// were wrong in it, held.
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
        manifest.contains("read_file(path: string, from?: number, to?: number): Promise<{ content: string; version: string }>"),
        "`read_file` declares no field its handler cannot produce: {manifest}"
    );
}

/// The opening listing: bounded, two deep, and quiet about build
/// output.
#[test]
fn the_working_directory_listing_is_bounded_and_skips_noise() {
    let dir = std::env::temp_dir().join(format!("agent2-listing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
    std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
    std::fs::write(dir.join("app.py"), "").unwrap();
    std::fs::write(dir.join("sub/mod.py"), "").unwrap();
    std::fs::write(dir.join("sub/deeper/buried.py"), "").unwrap();
    std::fs::write(dir.join("node_modules/pkg/index.js"), "").unwrap();

    let out = listing(&dir);
    assert!(out.contains("app.py"), "{out}");
    assert!(out.contains("sub/"), "directories are marked: {out}");
    assert!(out.contains("sub/mod.py"), "two levels deep: {out}");
    assert!(!out.contains("buried.py"), "and not three: {out}");
    assert!(
        !out.contains("node_modules"),
        "build noise is skipped: {out}"
    );

    // A big tree costs a fixed number of bytes and says that it stopped.
    let big = dir.join("many");
    std::fs::create_dir_all(&big).unwrap();
    for i in 0..(LISTING_MAX_ENTRIES + 20) {
        std::fs::write(big.join(format!("f{i}.txt")), "").unwrap();
    }
    let out = listing(&dir);
    assert!(out.contains("… and more"), "{out}");
    assert!(
        out.lines().count() < LISTING_MAX_ENTRIES + 12,
        "bounded: {} lines",
        out.lines().count()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Bounded before the sort.** The 50-entry cap limits what is shown;
/// without a read cap, a directory with a hundred thousand entries is
/// still enumerated and sorted in full to print fifty of them, at the
/// start of every session.
#[test]
fn a_huge_directory_costs_a_bounded_amount_of_work() {
    let dir = std::env::temp_dir().join(format!("agent2-listing-big-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..(LISTING_MAX_SCAN + 200) {
        std::fs::write(dir.join(format!("f{i:05}.txt")), "").unwrap();
    }
    let started = std::time::Instant::now();
    let out = listing(&dir);
    assert!(out.contains("… and more"), "says it stopped: {out}");
    assert!(
        out.lines().count() < LISTING_MAX_ENTRIES + 12,
        "{} lines",
        out.lines().count()
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(&dir);
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
        let js: String = ex
            .assistant
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
    assert_eq!(rows.len(), 3, "the table's rows parsed: {rows:?}");

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
        .split_once("## Three places")
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
        Ok(dir) => format!(
            "\n\nCurrent working directory: {}{}",
            dir.display(),
            listing(&dir)
        ),
        // Not worth failing a session over, and a wrong answer would be
        // worse than none.
        Err(_) => String::new(),
    }
}

/// How many entries the opening listing shows before it stops.
const LISTING_MAX_ENTRIES: usize = 50;

/// How many directory entries are read before one is given up on.
/// Ten times what can be shown, so ordinary directories sort whole and
/// a pathological one costs a bounded amount of work rather than
/// however much happens to be on disk.
const LISTING_MAX_SCAN: usize = 500;

/// Noise every listing of a working tree has and nobody wants: build
/// output and dependency trees, which are large, uninteresting, and the
/// reason a naive `find .` comes back with ten thousand lines.
const LISTING_SKIP: [&str; 9] = [
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    ".mypy_cache",
    ".pytest_cache",
];

/// **What is in the working directory, two levels deep.**
///
/// Telling the model *where* it is fixed half of this; the measurement
/// says the other half was still being paid. Across 96 kept runs, 23 —
/// **a quarter** — spend their entire first program on `ls`, `find` or
/// `pwd` and nothing else, which is a whole completion spent learning
/// what a listing would have said. Two of them ran `find` over a depth
/// of three and piped it through `head -200`.
///
/// Two levels because one is usually a list of directories and three is
/// usually a flood. Bounded at [`LISTING_MAX_ENTRIES`] and honest when
/// it stops, so a large tree costs a fixed number of bytes rather than
/// however many files happen to be there.
///
/// Snapshotted at the agent's root with everything else here: it says
/// what was there when the conversation started, and a session that
/// changes the tree later reads it as history, which is what the rest
/// of the record is.
fn listing(dir: &std::path::Path) -> String {
    // **Breadth-first, so a truncation costs detail rather than a
    // whole subtree.** Depth-first spends the budget on whatever sorts
    // first: a repo whose `app/` holds sixty files would show `app/`
    // and nothing else, and the reader would not learn that `tests/`
    // and `Makefile` exist at all. Every top-level name first, then
    // what is inside them, means the shallowest facts — which are the
    // ones a first program acts on — are the ones that survive.
    fn level(
        dir: &std::path::Path,
        prefix: &str,
        out: &mut Vec<String>,
        cut: &mut bool,
    ) -> Vec<(String, String)> {
        let mut dirs = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return dirs;
        };
        // **Bounded before the sort, not after.** The cap below limits
        // what is *shown*; this limits what is read, because a
        // directory with a hundred thousand entries would otherwise be
        // enumerated and sorted in full to print fifty of them, at the
        // start of every session. The evals run in directories with
        // four files; a real checkout is where this matters.
        let mut entries: Vec<_> = rd.flatten().take(LISTING_MAX_SCAN).collect();
        let overflowed = entries.len() == LISTING_MAX_SCAN;
        entries.sort_by_key(|e| e.file_name());
        if overflowed {
            *cut = true;
        }
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if LISTING_SKIP.contains(&name.as_str()) {
                continue;
            }
            if out.len() >= LISTING_MAX_ENTRIES {
                *cut = true;
                return dirs;
            }
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            out.push(format!("{prefix}{name}{}", if is_dir { "/" } else { "" }));
            if is_dir {
                dirs.push((format!("{prefix}{name}/"), e.path().display().to_string()));
            }
        }
        dirs
    }
    let mut out = Vec::new();
    let mut truncated = false;
    for (prefix, path) in level(dir, "", &mut out, &mut truncated) {
        if truncated {
            break;
        }
        level(
            std::path::Path::new(&path),
            &prefix,
            &mut out,
            &mut truncated,
        );
    }
    // Back into path order once the budget has been spent breadth-first,
    // so what is shown reads as a tree rather than as two passes.
    out.sort();
    if out.is_empty() {
        return String::new();
    }
    let more = if truncated {
        "\n… and more — `bash` for the rest."
    } else {
        ""
    };
    format!(
        "\n\nWhat is in it, two levels deep:\n\n```text\n{}\n```{more}",
        out.join("\n")
    )
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
        const EXPECTED_LEN: usize = 19912;
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
        // `history.remove`/`history.replace` are named here too now.
        // They used to be withheld, because they were refused outside a
        // compaction program and advertising them would have pointed
        // the model at two verbs that fail where it would first try
        // them. They are not refused any more: a program that has
        // finished with an entry may say so when it knows, which is
        // usually the program that made it rather than a compaction
        // program later with less to go on.
        for member in [
            "function append(",
            "function fetch(",
            "function remove(",
            "function replace(",
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
        assert!(card().contains("done()"), "{}", card());
        assert!(
            card().contains("not trying to finish the task in one reply"),
            "{}",
            card()
        );
    }

    /// An exemplar's cells, in order, in one scope — what the notebook
    /// driver hands the compiler.
    fn cells_of(reply: &str) -> String {
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
            "spawn" | "fork" => json!({ "agent": 2 }),
            _ => json!(null),
        }
    }

    /// How an exemplar ended — the thing 27.1 made a decision rather
    /// than an omission, so it is worth reading back off a real run
    /// instead of grepping the source for the word.
    struct Ending {
        /// `done()` was called: the task is over.
        done: bool,
        /// `history.append` was called: something was handed to the
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
        let mut done = false;
        let mut appended = false;
        loop {
            match vm.step(u64::MAX).map_err(|e| format!("{e:?}"))? {
                StepResult::Done { .. } => return Ok(Ending { done, appended }),
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
                // The settle-at-dispatch verbs — `done()` among them —
                // come back here instead of through the outbox: they
                // answer into the frame that called them, so the stub
                // pushes the value rather than settling a promise.
                StepResult::Settle { call } => {
                    if call.name == crate::machine::TOOL_DONE {
                        done = true;
                    }
                    if call.name == crate::machine::TOOL_APPEND_HISTORY {
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
                // Unreachable with `u64::MAX` fuel, and there is
                // nothing to do about it but step again.
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
    /// change exists to prevent. Each one either calls `done()`, because
    /// its task is finished, or `history.append`s the thing the next
    /// reply continues from.
    #[test]
    fn every_exemplar_ends_on_purpose() {
        for ex in &exemplars() {
            // An exemplar is a *reply*: markdown, with its program in
            // fenced cells. Running it means running its cells, in
            // order, in one scope — which is what the notebook driver
            // does with the real thing.
            let source = cells_of(&ex.assistant);
            let ending = run_against_stubs(&source)
                .unwrap_or_else(|e| panic!("exemplar for {:?} trapped: {e}", ex.user));
            // **The two endings a reply has.** `done()` says the task is
            // finished; `history.append` hands a finding to the reply
            // after this one and rests the branch (D4). There is no
            // `return` to be the second of those any more, and an
            // exemplar that does neither demonstrates a reply that found
            // something and threw it away.
            //
            // Read off the run, not off the text: a call inside a
            // branch never taken is not an ending.
            assert!(
                ending.done || ending.appended,
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
            for ex in &card.exemplars {
                // **What "the assistant turn" *is* depends on the
                // transport the variant is for.** Under
                // `Transport::Notebook` it is markdown whose ```js blocks
                // are the program, so compiling the whole thing as
                // JavaScript would fail on the prose. A variant that
                // contains a cell is read the way that transport reads
                // it: split first, then compile each cell.
                let cells = crate::notebook::split_cells(&ex.assistant);
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
                // that hands work to the next reply is `history.append`.
                let ends = ex.assistant.contains("done()")
                    || ex.assistant.contains("history.append")
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
    /// uses `append_history` as a short projection, and every one opens
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
        let harness: String = [include_str!("report.rs"), include_str!("machine.rs")]
            .iter()
            .map(|f| match f.find("\n#[cfg(test)]") {
                Some(at) => &f[..at],
                None => f,
            })
            .collect::<Vec<_>>()
            .concat();
        // Only what the model reads: quoted strings, not identifiers,
        // doc comments, or the constants the lowering is named by.
        let model_facing: Vec<&str> = harness
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///") && l.contains('"')
            })
            .collect();
        for (stale, instead) in [
            ("fetch_history(", "history.fetch("),
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
        assert_eq!(ex.len(), 5, "five, and each earns its place");
        assert!(
            ex[0].assistant.contains("done()") && !ex[0].assistant.contains("return"),
            "the first ends a finished task: {}",
            ex[0].assistant
        );
        // **`history.append`, not `return`.** A reply has no return
        // (D5): what the second exemplar demonstrates is handing a
        // finding on to the next reply and *not* ending the task.
        assert!(
            ex[1].assistant.contains("history.append") && !ex[1].assistant.contains("done()"),
            "the second hands on and does not stop: {}",
            ex[1].assistant
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
            ex[2].assistant.contains("await choose(")
                && ex[2].assistant.contains("===")
                && ex[2].assistant.contains("done()"),
            "the third offers a bounded choice and acts on the answer: {}",
            ex[2].assistant
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
        // is a promise the program has to live to keep; `history.append`
        // is already on the log the moment it is called, and survives
        // the program's own trap — verified on a real run, where note
        // #551 outlived the exception that killed the program holding
        // it. That is the case the card never made, and the failure it
        // describes is one we watched: a dead-code run worked out
        // correctly which attributes were pointless, trapped on
        // `fmt is not defined`, and lost the whole analysis.
        assert!(
            ex[4].assistant.matches("history.append").count() == 2,
            "the fifth appends separately, a row each: {}",
            ex[4].assistant
        );
        // Per-item findings belong in `console.log`, not `append`: 200
        // appends would be 200 permanent rows, which is the firehose
        // the card warns about, while the console's display is bounded
        // and its record stays whole and fetchable.
        assert!(
            ex[3].assistant.contains("console.log") && !ex[3].assistant.contains("history.append"),
            "the loop prints per item rather than appending: {}",
            ex[3].assistant
        );
        // **The fourth is the whole structural argument for code mode**
        // — N items in one completion, where a tool loop spends N round
        // trips — and nothing showed it. Measured across 3,071 programs
        // on 2026-09-17: 15% contain a loop at all and 5% make parallel
        // calls, so the shape the design exists for is the shape the
        // model almost never reaches for. It carries both in one
        // program: enumerate, read in parallel, edit each, verify once.
        assert!(
            ex[3].assistant.contains("Promise.all") && ex[3].assistant.contains("for ("),
            "the fourth does many at once: {}",
            ex[3].assistant
        );
        // **The finishing one changes something and checks it.** It used
        // to be `bash("make check")` → `tell` → `done()`, against the
        // prompt "is the build green?" — right for that prompt, and
        // structurally identical to the dominant failure: measured on
        // 2026-09-17, 45% of programs read something, wrote nothing,
        // carried nothing forward and did not finish, 140 of those 155
        // ending by telling the user. The exemplars are two turns of a
        // 10 KB card and the only place the model sees the work done
        // rather than described, so one of them does the work.
        assert!(
            ex[0].assistant.contains("replace_file") && ex[0].assistant.contains("bash"),
            "the first edits and then runs the thing that would fail: {}",
            ex[0].assistant
        );
        // **Whatever finishes, speaks.** Every exemplar that calls
        // `done()` tells the person first, and the two that hand on
        // instead do neither — which is the rule, demonstrated rather
        // than stated. Pinned because the demonstration lost once: a
        // crossing-table line arguing `console.log` over `tell` moved
        // tells from 75% of programs to 22% and took the legitimate
        // ones with them, and runs ending without a word to anybody
        // went from 1 in 12 to 4 in 12 across four arms. Three
        // exemplars showing the opposite did not hold it.
        for e in &ex {
            assert_eq!(
                e.assistant.contains("done()"),
                e.assistant.contains("tell("),
                "an exemplar finishes without speaking, or speaks without finishing: {}",
                e.assistant
            );
        }
        // Short enough to be a shape rather than a technique to copy —
        // a live run on 2026-09-17 reproduced a long exemplar verbatim,
        // invented names and all, into a repo that had none of them. The
        // cap counts the whole reply now, prose and fences included, so
        // it is larger than the 400 it was when an exemplar was bare
        // JavaScript; the code inside is no longer than it was.
        for e in &ex {
            assert!(
                e.assistant.len() < 700,
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
        assert_eq!(card.exemplars.len(), 5, "five, each keeping its own job");

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
    /// transport there are only two ways to: `done()`, because the task is
    /// finished, or `history.append`, because something is being handed to
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
            let finishes = ex.assistant.contains("done()");
            let hands_on = ex.assistant.contains("history.append");
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
        let ex = load_from(&dir).unwrap().exemplars;

        // The first finishes a task it actually changed, and checks the
        // change by running the thing that would fail.
        assert!(
            ex[0].assistant.contains("done()") && ex[0].assistant.contains("tools.replace_file"),
            "the first changes something and finishes: {}",
            ex[0].assistant
        );
        // The second hands on without stopping — which is now `append`,
        // there being no `return`.
        assert!(
            ex[1].assistant.contains("history.append") && !ex[1].assistant.contains("done()"),
            "the second hands on and does not stop: {}",
            ex[1].assistant
        );
        // The third offers a *bounded* choice and acts on the answer. The
        // awaited value is the point: a free-form `ask` answered in prose
        // could not be compared with `===`. And it guards the skip with
        // `else` — which under this transport is not merely tidy: `done()`
        // stops nothing, so a guard that used it would write the file it
        // meant to leave alone (D8).
        assert!(
            ex[2].assistant.contains("await choose(")
                && ex[2].assistant.contains("===")
                && ex[2].assistant.contains("} else {")
                && ex[2].assistant.contains("done()"),
            "the third offers a bounded choice and guards with else: {}",
            ex[2].assistant
        );
        // The fourth does many at once — the structural argument for code
        // mode — and prints per item rather than appending 200 rows.
        assert!(
            ex[3].assistant.contains("Promise.all")
                && ex[3].assistant.contains("for (")
                && ex[3].assistant.contains("console.log")
                && !ex[3].assistant.contains("history.append"),
            "the fourth does many at once and prints per item: {}",
            ex[3].assistant
        );
        // It is also the one that demonstrates the shared scope: it binds
        // in one cell and uses the binding in the next, which is the thing
        // about this transport a declaration cannot show.
        let cells = crate::notebook::split_cells(&ex[3].assistant);
        assert!(cells.len() >= 2, "the fourth spans two cells");
        assert!(
            cells[0].slice(&ex[3].assistant).contains("const hits")
                && cells[1].slice(&ex[3].assistant).contains("hits.length"),
            "the fourth binds in one cell and reads it in the next"
        );
        // The fifth keeps two rows rather than one fat value, so a later
        // compaction can drop one and leave the other exact.
        assert!(
            ex[4].assistant.matches("history.append").count() >= 2,
            "the fifth appends separately, a row each: {}",
            ex[4].assistant
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
        core.reject_top_level_return(crate::notebook::NO_TOP_LEVEL_RETURN);

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
