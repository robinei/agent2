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
fn ts_type(schema: &serde_json::Value) -> &'static str {
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

pub fn tool_manifest(registry: &crate::host::ToolRegistry) -> String {
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
    manifest.push_str("\n\ndeclare namespace tools {");
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
    manifest.push_str("}\n");
    manifest
}

/// The full system prompt for one agent: [`CARD`] plus [`tool_manifest`]
/// for its (possibly allowlist-narrowed) registry. Callers snapshot this
/// once, at the agent's root (`Agent.system` — see `types::EventPayload`),
/// never recompute it mid-conversation: the system prompt is the
/// immutable cache prefix, and a later card edit or registry change must
/// not alter an existing conversation's prompt out from under it.
pub fn full_card(registry: &crate::host::ToolRegistry) -> String {
    format!(
        "{}{}{}",
        active().text,
        tool_manifest(registry),
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
        let manifest = tool_manifest(&registry_with_tools());
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
        let m = tool_manifest(&registry);
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
        let m = tool_manifest(&registry_with_tools());
        assert!(m.contains("  /** No names, no return type. */"), "{m}");
    }

    /// A tool that names no parameters and declares no return type is
    /// still declared. A manifest that omits a live tool is worse than
    /// one that describes it thinly, and `argN: unknown` is honest.
    #[test]
    fn a_tool_without_names_or_a_return_type_still_renders() {
        let manifest = tool_manifest(&registry_with_tools());
        assert!(
            manifest.contains("function bare(arg0: Record<string, unknown>): Promise<unknown>;"),
            "{manifest}"
        );
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
        const EXPECTED_LEN: usize = 12235;
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
        // What the whole reply is, and that nothing may surround it.
        assert!(card().contains("no code fence"), "{}", card());
        assert!(
            card().contains("entire reply is a JavaScript program"),
            "{}",
            card()
        );
        // And the two halves of the ending, which 27.1 inverted: a
        // program finishing is not the task finishing.
        assert!(card().contains("done()"), "{}", card());
        assert!(
            card().contains("not trying to finish the task in one program"),
            "{}",
            card()
        );
    }

    #[test]
    fn the_exemplars_assistant_turn_is_valid_javascript() {
        // The one thing in this file that must actually compile: each
        // exemplar's assistant turn is exactly what a real completion
        // would need to parse (Step B1's own rule for an assistant
        // turn), so it is held to the same standard here.
        for ex in &exemplars() {
            interp::compile(&ex.assistant)
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
            run_against_stubs(&ex.assistant)
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
        /// The program returned something for the next one to read.
        /// `Value::Undefined` when it ran off the end.
        returned: bool,
    }

    /// Drive one program on a bare VM: every call answered by
    /// [`stub_result`], every `raise` resumed with a plausible answer.
    /// Deliberately not the real machine — this checks the program
    /// against its tools, and wants no conversation around it.
    fn run_against_stubs(src: &str) -> Result<Ending, String> {
        use interp::{StepResult, VM, Value};
        let program = interp::compile(src).map_err(|e| format!("{e:?}"))?;
        let mut vm = VM::for_program(program, serde_json::Value::Null)
            .map_err(|e| format!("could not start: {e:?}"))?;
        let mut done = false;
        loop {
            match vm.step(u64::MAX).map_err(|e| format!("{e:?}"))? {
                StepResult::Done { value, .. } => {
                    return Ok(Ending {
                        done,
                        returned: !matches!(value, Value::Undefined),
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
                // The settle-at-dispatch verbs — `done()` among them —
                // come back here instead of through the outbox: they
                // answer into the frame that called them, so the stub
                // pushes the value rather than settling a promise.
                StepResult::Settle { call } => {
                    if call.name == crate::machine::TOOL_DONE {
                        done = true;
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
            }
        }
    }

    /// **Every exemplar ends on purpose.** Since 27.1 a program that
    /// runs off the end does not stop — the next one is written — so
    /// falling off the end is no longer an ending at all, and an
    /// exemplar that did it would be teaching the accident the whole
    /// change exists to prevent. Each one either calls `done()`,
    /// because its task is finished, or returns the thing the next
    /// program continues from.
    ///
    /// Not both: `done()` rests the branch, so a value returned beside
    /// it is read by nobody, and writing one says the author expected
    /// something to come next.
    #[test]
    fn every_exemplar_ends_on_purpose() {
        for ex in &exemplars() {
            let ending = run_against_stubs(&ex.assistant)
                .unwrap_or_else(|e| panic!("exemplar for {:?} trapped: {e}", ex.user));
            assert!(
                ending.done || ending.returned,
                "exemplar for {:?} runs off the end — under automatic \
                 continuation that is not an ending",
                ex.user
            );
            assert!(
                !(ending.done && ending.returned),
                "exemplar for {:?} calls done() *and* returns a value — \
                 nothing will read the value",
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
                interp::compile(&ex.assistant).unwrap_or_else(|e| {
                    panic!(
                        "{} exemplar for {:?} does not parse: {e:?}",
                        dir.display(),
                        ex.user
                    )
                });
                let ends = ex.assistant.contains("done()")
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
    #[test]
    fn the_exemplars_demonstrate_the_endings_and_the_shapes() {
        let ex = exemplars();
        assert_eq!(ex.len(), 5, "five, and each earns its place");
        assert!(
            ex[0].assistant.contains("done()") && !ex[0].assistant.contains("return"),
            "the first ends a finished task: {}",
            ex[0].assistant
        );
        assert!(
            ex[1].assistant.contains("return") && !ex[1].assistant.contains("done()"),
            "the second hands on and does not stop: {}",
            ex[1].assistant
        );
        assert!(
            ex[2].assistant.contains("await ask(") && ex[2].assistant.contains("done()"),
            "the third asks mid-program and acts on the answer: {}",
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
        // invented names and all, into a repo that had none of them.
        for e in &ex {
            assert!(
                e.assistant.len() < 400,
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
}
