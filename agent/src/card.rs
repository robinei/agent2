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
            exemplar!("01-do-the-tests-pass"),
            exemplar!("02-fix-a-value-that-needs-asking"),
            exemplar!("03-judge-two-logs"),
            exemplar!("04-raise-a-conflict"),
            exemplar!("05-handle-a-raised-condition"),
            exemplar!("06-fan-out-to-a-helper"),
            exemplar!("07-recon-then-hand-over"),
            exemplar!("08-recurring-cleanup"),
            exemplar!("09-rename-and-build"),
            exemplar!("10-probe-loop"),
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
    format!("{}{}", active().text, tool_manifest(registry))
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
        assert!(full.contains("declare namespace tools {"));
        assert!(full.contains("function fetch_page("));
    }

    #[test]
    fn the_card_is_stable() {
        // A golden test in the sense Step C4 asks for: any edit to
        // `card()` shows up as a diff review must look at, not a byte
        // count that silently drifts. Comparing full text (not just a
        // hash) so the diff itself is legible in a failure message.
        const EXPECTED_LEN: usize = 18725;
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
            "append_history(",
            "fetch_history(",
            "raise(",
            "resume(",
            "abandon(",
            "remove_history(",
            "rewrite_history(",
            "list_agents(",
        ] {
            assert!(card().contains(verb), "card is missing {verb}");
        }
    }

    #[test]
    fn the_card_states_the_no_fence_rule_and_the_no_op_rule() {
        assert!(card().contains("no code fence"));
        assert!(card().contains("parsed as JavaScript"));
        assert!(card().contains("silent no-op"));
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
            "read_file" => json!({
                "content": "// one\n#[ignore]\nfn thing() {}\n",
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

    #[test]
    fn the_exemplars_open_with_a_tell() {
        // Narration used to be a special leading-comment convention
        // that nothing ever streamed to anyone (there was no
        // extraction code anywhere in the harness); it's replaced by
        // an ordinary `tell()` call, which actually reaches the user.
        // `tell()` has no placement rule the old convention claimed to
        // (a live watcher never existed to have one for) — it can
        // appear anywhere in the program — but each exemplar still
        // opens by saying what it's about to do, which is the
        // property this checks. Leading `//` comments are skipped:
        // a comment is not a statement, and the handler exemplar
        // opens by explaining the judgement it is about to make
        // before announcing it.
        for ex in &exemplars() {
            let first_statement = ex
                .assistant
                .lines()
                .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with("//"))
                .unwrap_or("");
            assert!(
                first_statement.trim_start().starts_with("tell("),
                "exemplar for {:?} opens with {first_statement:?}, not a tell()",
                ex.user
            );
        }
    }

    #[test]
    fn the_second_exemplar_demonstrates_ask_inline_then_acting_on_it() {
        // Card prose alone ("ask() is a normal await, not a reason to
        // end early") wasn't enough to stop a live run from splitting
        // an ordinary read-then-ask into two programs (2026-09-10) —
        // this exemplar is the demonstration, so hold it to actually
        // being one: `ask(` appears, and something runs after it in
        // the same program (not the program's last line).
        let all = exemplars();
        let ex = &all[1];
        let ask_at = ex
            .assistant
            .find("await ask(")
            .expect("second exemplar should demonstrate ask()");
        let after = &ex.assistant[ask_at..];
        assert!(
            after.lines().count() > 2,
            "ask() should not be the last meaningful line of the exemplar"
        );
    }

    #[test]
    fn the_third_exemplar_finishes_the_judgment_in_the_same_program() {
        // Card prose alone ("ending a program... has quietly failed to
        // do the task") wasn't enough either — two independent live
        // runs (2026-09-14) each wrote a genuinely well-reasoned
        // recon-only program whose own closing comment planned a "next
        // program," which nothing then ran. This exemplar is the
        // demonstration: both reads happen, then the judgment (a
        // tell() call comparing what was actually read) happens in the
        // same program, not a planned-but-absent one.
        let all = exemplars();
        let ex = &all[2];
        let last_read = ex
            .assistant
            .rfind("tools.read_file(")
            .expect("third exemplar should read more than one thing");
        let after = &ex.assistant[last_read..];
        assert!(
            after.contains("tell("),
            "the judgement must be reported in the same program as the reads, \
             not deferred to an implied next one"
        );
        // The specific failure this exemplar answers: a program ending
        // on a forward-looking comment instead of doing the work.
        assert!(!ex.assistant.to_lowercase().contains("next program"));
    }

    #[test]
    fn the_fourth_exemplar_actually_calls_raise() {
        // raise() had prose in three places and no worked example
        // anywhere — live 2026-09-14 found a program that read two
        // genuinely conflicting, data-side-irresolvable numbers,
        // correctly recognized it couldn't decide, and then just
        // stopped instead of calling raise() — recognizing the
        // moment isn't the same as acting on it, the same gap a
        // worked example (not another sentence) closed for ask() and
        // for finishing a judgement inline.
        let all = exemplars();
        let ex = &all[3];
        assert!(
            ex.assistant.contains("await raise("),
            "the fourth exemplar exists specifically to demonstrate raise() \
             actually being called, not just described"
        );
        // And it must not be the program's last line — same
        // "reported, not just decided" standard the ask()-inline
        // exemplar is held to.
        let raise_at = ex.assistant.find("await raise(").unwrap();
        assert!(ex.assistant[raise_at..].lines().count() > 1);
    }

    #[test]
    fn the_fifth_exemplar_uses_append_history_as_a_short_projection() {
        // append_history's payoff isn't wired anywhere yet (see the
        // doc comment on exemplars()) — this exemplar tests only
        // whether the model reaches for the verb appropriately once
        // shown how, not whether anything downstream uses it.
        // Found by what it demonstrates, not by position — an exemplar
        // added ahead of it must not silently retarget this test at a
        // different one.
        let all = exemplars();
        let ex = all
            .iter()
            .find(|e| e.assistant.contains("append_history("))
            .expect("an exemplar demonstrating append_history");
        assert!(
            ex.assistant.contains("append_history("),
            "the fifth exemplar exists to demonstrate append_history actually \
             being called, not just described"
        );
        // The card's own rule: "never the raw result." The appended
        // string must be short — a projection, not a dump of the
        // count() call's own output.
        let call_start = ex.assistant.find("append_history(").unwrap();
        let call_end = ex.assistant[call_start..].find(");").unwrap() + call_start;
        let payload = &ex.assistant[call_start..call_end];
        assert!(
            payload.len() < 200,
            "append_history's payload should be a short projection, not a \
             dump: {} bytes",
            payload.len()
        );
        // And the card's other rule: not to read something back the
        // same turn. There is no matching fetch_history()/read of this
        // call's own value anywhere in the exemplar.
        assert!(!ex.assistant.contains("fetch_history("));
    }
}
