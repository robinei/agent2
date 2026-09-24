//! Structural tools (10_EDITING F2): `outline` and `parse_errors` backed by
//! tree-sitter — read-only, no disk mutation.
//!
//! Language is inferred from the file extension; an unsupported extension
//! returns an explicit error, never a silent empty list.
//!
//! `parse_errors` has a second form that checks candidate source before
//! writing: `parse_errors([null, source, lang])` — no disk touch.

use std::path::Path;

use serde_json::{Value, json};
use tree_sitter::{Node, Parser};

use super::registry::ToolDef;
use crate::report::clip;

// ── language detection ───────────────────────────────────────────────────────

/// Map a file extension to the language name used internally.
fn lang_from_path(path: &str) -> Result<&'static str, String> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "rs" => Ok("rust"),
        "js" | "mjs" | "cjs" => Ok("javascript"),
        "ts" | "tsx" | "mts" | "cts" => Ok("typescript"),
        "py" | "pyi" => Ok("python"),
        _ => Err(format!(
            "unsupported file extension .{ext} for structural tools \
             (supported: .rs, .js/.mjs/.cjs, .ts/.tsx/.mts/.cts, .py/.pyi)"
        )),
    }
}

fn language_for(lang: &str) -> Result<tree_sitter::Language, String> {
    match lang {
        "rust" => Ok(tree_sitter_rust::LANGUAGE.into()),
        "javascript" => Ok(tree_sitter_javascript::LANGUAGE.into()),
        "typescript" => Ok(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "python" => Ok(tree_sitter_python::LANGUAGE.into()),
        other => Err(format!("unknown language: {other}")),
    }
}

// ── outline ──────────────────────────────────────────────────────────────────

pub fn outline_def() -> ToolDef {
    ToolDef {
        name: "outline".into(),
        // What it *omits* is the load-bearing half of this description.
        // Observed live 2026-09-15: a program looking for
        // `#[allow(dead_code)]` attributes called `outline` on sixteen
        // files, which structurally cannot answer that — sixteen calls
        // that could only come back empty-handed. The old text ("list
        // its top-level definitions") was accurate and said nothing
        // about what is absent, which is what a reader needs in order to
        // pick a different tool.
        description: "The definitions in a source file, language inferred from the extension — Rust, JavaScript, TypeScript or Python. Nested ones are included: a method in an `impl` or `class`, a test in `#[cfg(test)] mod tests`, each with `parent` naming the scope that holds it. Read-only. It lists what a file *defines*, never what uses it."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "name": "path", "type": "string", "description": "absolute or cwd-relative path" }
            ],
            "minItems": 1,
            "maxItems": 1
        }),
        guidelines: vec![
            "**If you will read this yourself — not just compute on it — you must `history.peek(r)` in this same reply, or `history.keep(r)` to have it from here on.** Nothing else shows it to you: the result is a variable, and printing it back is replaced by the id of the row it repeats.".into(),
            "Nothing here tells you a definition is unused. A name can be reached without appearing anywhere as that name — `getattr(mod, \"f_\" + i)`, a table keyed by strings, a decorator registry — so neither this nor a search for the name can see it. Before deleting a definition, look for the *mechanisms*: read the callers, and grep for the prefix and for `getattr`/`globals`/registry calls. Then work out which names that mechanism can actually reach — a dispatch table is usually a list you can read, and \"a lookup exists\" is not the same answer as \"every name is reachable\".".into(),
            "`start_line` is an edit anchor, not just a fact: `Edit.replaceLines(text, start_line, end_line, …)` names one place exactly, where a string that looks distinctive often is not. A marker like `TODO(perf)` appears seven times in a small file; `parse_header` appears once, and outline says which lines it spans.".into(),
        ],
        example: Some("const { items } = await tools.outline(path);".into()),
        returns: Some(
            "{ items: Array<{ name: string; kind: \"function\" | \"class\" | \
             \"struct\" | \"enum\" | \"trait\" | \"impl\" | \"module\" | \"const\" | \"static\" | \
             \"type\" | \"macro\" | \"interface\" | \"field\" | \"variable\"; start_line: number; \
             end_line: number; signature?: string; attributes?: string[]; doc?: string; \
             parent?: string }>; id: number }"
                .into(),
        ),
        show_once: false,
        handler: Box::new(|args| {
            let path = args
                .get(0)
                .and_then(|v| v.as_str())
                .ok_or("outline(path) needs a string path")?;
            let lang = lang_from_path(path)?;
            let source = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            run_outline(&source, lang)
        }),
    }
}

fn run_outline(source: &str, lang: &str) -> Result<Value, String> {
    let mut parser = Parser::new();
    parser
        .set_language(&language_for(lang)?)
        .map_err(|e| format!("setting language: {e}"))?;

    let tree = parser.parse(source, None).ok_or("parse returned no tree")?;

    let root = tree.root_node();
    let entries = collect_definitions(root, source, lang);
    // **An object, like every other tool.** This returned the bare
    // array for a long time while its own `example` and `returns` both
    // promised `{ items }` — so a model that read its declaration and
    // wrote `outline.items` got `undefined`, and `.length` of that.
    // Across 96 kept runs that was 11 of the 30 traps recorded and
    // every single `.length of undefined` among them, on 21 calls: a
    // 52% trap rate for the one tool in the set that did not return an
    // object. The declaration was right about what the model wanted;
    // it was the value that was the outlier.
    Ok(json!({
        "items": serde_json::to_value(entries).unwrap_or(json!([]))
    }))
}

#[derive(serde::Serialize)]
struct OutlineEntry {
    name: String,
    kind: String,
    start_line: usize,
    end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    /// Attributes and decorators attached to this definition, verbatim
    /// and in source order: `#[test]`, `#[allow(dead_code)]`, `@cached`.
    ///
    /// Included because they are short and carry meaning the signature
    /// does not — whether a function is a test, a lint is suppressed, a
    /// field is serialised. A live run looked for `#[allow(dead_code)]`
    /// by calling `outline` on sixteen files and got nothing back,
    /// because an index that omits attributes cannot answer a question
    /// about attributes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attributes: Vec<String>,
    /// The **first line** of the doc comment, if any — a summary, not
    /// the comment.
    ///
    /// First line only on purpose: this stays an index. A file whose
    /// definitions each carry a paragraph would otherwise return most of
    /// itself, and a caller who wants the prose can read the lines the
    /// entry already points at.
    #[serde(skip_serializing_if = "Option::is_none")]
    doc: Option<String>,
    /// What encloses this definition, as a scope path —
    /// `tests`, `Compactor`, `tests::helpers` — absent at file level.
    ///
    /// **`name` stays bare so a search by name still finds it.** The
    /// live failure this field exists for was a run searching
    /// `items` for a test by name; qualifying `name` into
    /// `tests::renders_a_line` would have left that search failing for
    /// a second reason. The qualification goes beside the name, not
    /// into it, and it is what tells `mod tests`'s `fn compact` from
    /// the `fn compact` at module level — which are otherwise two
    /// entries with the same name and kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
}

/// Attributes/decorators and the doc summary attached to `node`, read
/// from the siblings immediately above it.
///
/// Tree-sitter puts these *before* the definition rather than inside it
/// (in Rust an `attribute_item`, in most languages a comment node), so
/// they are found by walking back from the definition until something
/// that is neither runs out.
fn leading_context(node: &Node, source: &str) -> (Vec<String>, Option<String>) {
    let mut attributes = Vec::new();
    let mut doc_lines: Vec<String> = Vec::new();
    let mut cur = node.prev_sibling();
    while let Some(sib) = cur {
        let text = sib.utf8_text(source.as_bytes()).unwrap_or("").trim();
        match sib.kind() {
            "attribute_item" | "decorator" => attributes.push(text.to_owned()),
            k if k.contains("comment") => {
                // Doc comments only — an ordinary `//` note above a
                // definition is about the code, not a summary of it.
                let stripped = text
                    .strip_prefix("///")
                    .or_else(|| text.strip_prefix("//!"))
                    .or_else(|| text.strip_prefix("/**"));
                if let Some(rest) = stripped {
                    doc_lines.push(rest.trim().to_owned());
                } else {
                    break;
                }
            }
            _ => break,
        }
        cur = sib.prev_sibling();
    }
    attributes.reverse();
    doc_lines.reverse();
    let doc = doc_lines
        .into_iter()
        .find(|l| !l.is_empty())
        .map(|l| crate::report::clip(&l, 160));
    (attributes, doc)
}

/// The kind a reader would guess, from the kind the grammar uses.
///
/// **Four languages, four names for a function.** `kind` was the
/// tree-sitter node kind — `function_item` in Rust,
/// `function_declaration` in JS and TS, `function_definition` in
/// Python — so a program filtering an outline had to know which
/// grammar produced it, and the word every model actually reaches for,
/// `"function"`, matched none of them. Across the kept corpus programs
/// compared `kind` against `"function"` 26 times in 17 runs and
/// against `"function_definition"` 17 times in 15: the wrong guess was
/// commoner than the right one.
///
/// And it fails silently. `items.filter(i => i.kind === "function")`
/// is an empty array, not an error, so a run reads it as "nothing here
/// is live" and carries on. One `sweep-8` run did exactly that and
/// replaced the whole of `helpers.py` with its docstring.
///
/// The distinctions the grammars make are kept — a Rust `struct` and
/// an `enum` do not both become "type" — only the spelling is made
/// language-independent, so one filter works on any file.
///
/// **A method is a function.** `method_definition` used to render as
/// `"method"`, which was unreachable while nothing descended into a
/// class and became the same bug the moment something did: a JS class
/// method would have been `"method"` where the Rust `fn` in an `impl`
/// and the Python `def` in a `class` are both `"function"`, so
/// `filter(i => i.kind === "function")` would silently skip one
/// language's methods and no other's. `parent` carries what
/// `"method"` used to say, and carries it in every language.
/// **A declaration without a body is the same concept as one with**,
/// and the grammars give it a different node kind: a trait's required
/// `fn sig(&self);` is a `function_signature_item` where its optional
/// `fn deflt(&self) {}` is a `function_item`. Reporting the second as
/// `"function"` and the first as `"function_signature_item"` would say
/// the part of a trait that *is* the interface is a different species
/// from the part that merely shows — and a program filtering for
/// `"function"` would get the defaults and miss the contract.
fn readable_kind(kind: &str) -> &str {
    match kind {
        "function_item"
        | "function_declaration"
        | "function_definition"
        | "method_definition"
        | "generator_function_declaration"
        // Bodiless: a required trait method or an `extern` block's
        // `fn foo();` in Rust, an interface member, an abstract method
        // or a `declare function` in TypeScript.
        | "function_signature_item"
        | "function_signature"
        | "method_signature"
        | "abstract_method_signature" => "function",
        "class_declaration" | "class_definition" | "abstract_class_declaration" => "class",
        "struct_item" => "struct",
        "enum_item" | "enum_declaration" => "enum",
        "trait_item" => "trait",
        "impl_item" => "impl",
        // A TypeScript `namespace`/`module` body holds definitions and
        // scopes their names, which is what `mod` means here.
        "mod_item" | "internal_module" => "module",
        "const_item" => "const",
        "static_item" => "static",
        // `type A;` in a trait and `type A = u32;` outside it are the
        // same declaration with and without an answer.
        "type_item" | "type_alias_declaration" | "associated_type" => "type",
        "macro_definition" => "macro",
        "interface_declaration" => "interface",
        // Named slots on a type rather than free bindings: an
        // interface's `a: number`, a class's `f = 1`. Not `"variable"` —
        // a variable is something you can read on its own.
        "property_signature" | "public_field_definition" | "field_definition" => "field",
        "lexical_declaration" | "variable_declaration" => "variable",
        // A grammar kind with no word yet keeps its own, so a new
        // language degrades to the old behaviour rather than to a lie.
        other => other,
    }
}

/// One entry for a definition node, or `None` when it has no name.
fn entry_for(node: &Node, source: &str, lang: &str, parent: Option<&str>) -> Option<OutlineEntry> {
    if is_loop_binding(node) {
        return None;
    }
    let name = find_name(node, source)?;
    let (attributes, doc) = leading_context(node, source);
    Some(OutlineEntry {
        name,
        kind: readable_kind(node.kind()).to_string(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        signature: signature_for(node, source, lang),
        attributes,
        doc,
        parent: parent.map(str::to_owned),
    })
}

/// The `const x` in `for (const x of xs)`, which is a loop's own
/// bookkeeping and not something a reader outlines a file to find.
///
/// It only became reachable when `find_name` learned to look inside a
/// `variable_declarator`: before that a declaration at file level
/// produced no entry at all, so the loop head was excluded by the same
/// accident that excluded every `const`.
fn is_loop_binding(node: &Node) -> bool {
    matches!(node.kind(), "lexical_declaration" | "variable_declaration")
        && node
            .parent()
            .is_some_and(|p| p.kind().starts_with("for_") || p.kind() == "for_statement")
}

fn collect_definitions(node: Node, source: &str, lang: &str) -> Vec<OutlineEntry> {
    let mut entries = Vec::new();
    collect_definitions_impl(node, source, lang, None, &mut entries);
    entries
}

/// Walk the tree in source order, emitting one entry per definition and
/// **descending into the definitions that are containers** — a Rust
/// `mod`/`impl`/`trait`, a class in any of the four languages.
///
/// **It used to stop at the first definition on every path, so a file's
/// outline was its module level and nothing else.** A live run asked to
/// explain a Rust test called `outline` on `compaction.rs`, searched
/// `items` for the test by name and found nothing; its own reasoning
/// log worked out why — "the outline only lists top-level definitions,
/// so the `#[cfg(test)]` module's tests are not in it" — and it spent
/// two turns recovering. That file has eight module-level definitions
/// and around forty more inside `mod tests` and its `impl` blocks, so
/// the index was hiding the majority of what it indexes. In this
/// codebase the tests carry most of the documentation, which makes
/// `mod tests` the part most worth finding.
///
/// Containers, not everything: a `fn` nested inside another `fn` body
/// and a closure stay out, because an outline is a list of places you
/// can navigate to and name, not a parse dump. `#[cfg(test)]` gets no
/// special case — a module is descended into because it is a module,
/// and a rule that singled out one attribute would hide `mod parser`
/// for the same reason it used to hide `mod tests`.
fn collect_definitions_impl(
    node: Node,
    source: &str,
    lang: &str,
    parent: Option<&str>,
    entries: &mut Vec<OutlineEntry>,
) {
    for i in 0..node.named_child_count() {
        let Some(child) = node.named_child(i) else {
            continue;
        };
        if !is_definition_node(child.kind(), lang) {
            // Not a definition itself — a `decorated_definition`, a
            // `declaration_list`, an `export_statement`: keep looking
            // underneath it at the same scope.
            collect_definitions_impl(child, source, lang, parent, entries);
            continue;
        }
        let entry = entry_for(&child, source, lang, parent);
        let own_name = entry.as_ref().map(|e| e.name.clone());
        if let Some(entry) = entry {
            entries.push(entry);
        }
        if is_container_node(child.kind(), lang) {
            let inner = match (parent, own_name.as_deref()) {
                (Some(outer), Some(name)) => {
                    Some(format!("{outer}{}{name}", scope_separator(lang)))
                }
                (None, Some(name)) => Some(name.to_owned()),
                (outer, None) => outer.map(str::to_owned),
            };
            collect_definitions_impl(child, source, lang, inner.as_deref(), entries);
        }
    }
}

/// How the language writes a scope path, so `parent` reads the way the
/// file it came from does: `tests::helpers`, `Outer.Inner`.
fn scope_separator(lang: &str) -> &'static str {
    if lang == "rust" { "::" } else { "." }
}

/// A definition whose body holds more definitions worth indexing.
///
/// Deliberately narrower than "has a block": a function body is not one
/// of these. See [`collect_definitions_impl`] for why.
fn is_container_node(kind: &str, lang: &str) -> bool {
    match lang {
        "rust" => matches!(kind, "mod_item" | "impl_item" | "trait_item"),
        "python" => matches!(kind, "class_definition"),
        "javascript" => matches!(kind, "class_declaration"),
        // An `interface` holds declarations and names them, which is
        // the whole of what it is; not descending into it left the one
        // construct in the language that is *only* a list of members
        // reported as a single line. A `namespace` body is a scope, so
        // its contents used to surface at file level with no `parent` —
        // which is the ambiguity `parent` was added to remove.
        "typescript" => matches!(
            kind,
            "class_declaration"
                | "abstract_class_declaration"
                | "interface_declaration"
                | "internal_module"
        ),
        _ => false,
    }
}

/// **A declaration a reader would look for, whether or not it has a
/// body.** The bodiless half was missing everywhere it exists, and it
/// is the half of a trait or an interface that is actually the
/// contract: `trait T { fn sig(&self); fn deflt(&self) {} type A; }`
/// listed `deflt` — because it has a `{}` and is therefore a
/// `function_item` — and omitted `sig` and `A`, so the optional part
/// showed and the required part did not. `extern "C" { fn foo(); }` was
/// empty for the same reason, and in TypeScript an `interface`'s
/// members, an `abstract` method and `declare function g();` were all
/// invisible.
///
/// Python is the one language with nothing to add here: a bodiless
/// `def` does not exist, `def m(self): ...` has a block, and a
/// `Protocol` method is an ordinary `function_definition`.
fn is_definition_node(kind: &str, lang: &str) -> bool {
    match lang {
        "rust" => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "impl_item"
                | "mod_item"
                | "const_item"
                | "static_item"
                | "type_item"
                | "macro_definition"
                // Bodiless: a trait's required methods, and everything
                // inside `extern "C" { … }`.
                | "function_signature_item"
                | "associated_type"
        ),
        "javascript" => matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "method_definition"
                | "field_definition"
                | "lexical_declaration"
                | "variable_declaration"
        ),
        "typescript" => matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "method_definition"
                | "field_definition"
                | "lexical_declaration"
                | "variable_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "abstract_class_declaration"
                | "internal_module"
                // Bodiless: interface members, `abstract` members, and
                // the `function` half of a `declare function g();`.
                | "method_signature"
                | "property_signature"
                | "abstract_method_signature"
                | "public_field_definition"
                | "function_signature"
        ),
        "python" => matches!(kind, "function_definition" | "class_definition"),
        _ => false,
    }
}

/// Find the "name" child field within a definition node.
fn find_name(node: &Node, source: &str) -> Option<String> {
    for i in 0..node.child_count() {
        let child = node.child(i)?;
        if node.field_name_for_child(i as u32) == Some("name") {
            return Some(child.utf8_text(source.as_bytes()).ok()?.to_string());
        }
    }
    // Fallback: look for an identifier child without a field name
    for i in 0..node.named_child_count() {
        let child = node.named_child(i)?;
        // **A `const` at file level named nothing, so it appeared
        // nowhere.** `lexical_declaration` has no `name` field — the
        // name is a level down, on its `variable_declarator` — and no
        // branch here descended, so `find_name` returned `None` and the
        // entry was dropped. `"variable"` was in the declared `kind`
        // union the whole time with nothing able to produce it, and an
        // outline of a module of exported constants came back empty.
        if child.kind() == "variable_declarator" {
            return find_name(&child, source);
        }
        if child.child_count() == 0 && child.kind() == "identifier" {
            return Some(child.utf8_text(source.as_bytes()).ok()?.to_string());
        }
        // `private_property_identifier` is here because a class field
        // is a definition now and `#count = 0` is one: without it the
        // private half of a class is the half that silently vanishes,
        // which is the failure this whole walk exists to end.
        if child.child_count() == 0
            && matches!(
                child.kind(),
                "property_identifier" | "type_identifier" | "private_property_identifier"
            )
        {
            return Some(child.utf8_text(source.as_bytes()).ok()?.to_string());
        }
    }
    None
}

/// Extract a one-line signature from a definition node.
fn signature_for(node: &Node, source: &str, lang: &str) -> Option<String> {
    match lang {
        "rust" => signature_first_line(node, source, &["{"], 120),
        "javascript" | "typescript" => signature_first_line(node, source, &["{"], 120),
        "python" => signature_first_line(node, source, &[":", "->"], 120),
        _ => None,
    }
}

fn signature_first_line(
    node: &Node,
    source: &str,
    stop_chars: &[&str],
    max_len: usize,
) -> Option<String> {
    let bytes = &source.as_bytes()[node.start_byte()..node.end_byte()];
    let end = bytes
        .iter()
        .position(|b| {
            let ch = *b as char;
            ch == '\n' || stop_chars.iter().any(|s| s.contains(ch))
        })
        .unwrap_or(bytes.len());
    let sig_bytes = &bytes[..end.min(max_len)];
    let sig = String::from_utf8_lossy(sig_bytes).trim().to_string();
    if sig.is_empty() { None } else { Some(sig) }
}

// ── parse_errors ─────────────────────────────────────────────────────────────

pub fn parse_errors_def() -> ToolDef {
    ToolDef {
        name: "parse_errors".into(),
        description: "Check syntax. With a path, reads and checks that file; with `source` and `lang` instead (`\"rust\"`, `\"javascript\"`, `\"typescript\"`, `\"python\"`), checks content you computed **before writing it**."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                {
                    "name": "path",
                    "type": "string",
                    "nullable": true,
                    "description": "path, or null to check `source` instead"
                },
                { "name": "source", "type": "string", "description": "content to check, unwritten" },
                {
                    "name": "lang",
                    "type": "string",
                    "enum": ["rust", "javascript", "typescript", "python"],
                    "description": "language name, with `source`"
                }
            ],
            "minItems": 1,
            "maxItems": 3
        }),

        guidelines: vec![
            "Check content *before* writing it: pass `source` and `lang` with no path, and nothing touches disk.".into(),
        ],
        example: Some("const { ok } = await tools.parse_errors(null, candidate, lang);".into()),
        returns: Some(
            "{ ok: boolean; errors: Array<{ line: number; message: string }>; id: number }".into(),
        ),
        show_once: true,
        handler: Box::new(|args| {
            let first = args.get(0);
            if first.is_none() || first == Some(&Value::Null) {
                // Form: [null, source, lang]
                let source = args
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or("parse_errors([null, source, lang]) needs source as arg 2")?;
                let lang = args
                    .get(2)
                    .and_then(|v| v.as_str())
                    .ok_or("parse_errors([null, source, lang]) needs lang as arg 3")?;
                run_parse_errors(source, lang)
            } else {
                // Form: [path]
                let path = first
                    .and_then(|v| v.as_str())
                    .ok_or("parse_errors(path) needs a string path")?;
                let lang = match lang_from_path(path) {
                    Ok(lang) => lang,
                    // `parse_errors` is "verify what I just wrote", and a
                    // validation sweep routinely includes non-code files
                    // (HTML, CSS, JSON, …). Soft-skip those instead of
                    // rejecting the whole program — `outline` stays strict
                    // (you asked for structure of a file we can't parse),
                    // but a sweep should degrade gracefully, not detonate.
                    Err(_) => {
                        let ext = Path::new(path)
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("");
                        return Ok(json!({
                            "ok": null,
                            "skipped": format!(
                                "no structural parser for .{ext}; syntax not checked \
                                 (supported: rust, javascript, typescript, python)"
                            )
                        }));
                    }
                };
                let source = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
                run_parse_errors(&source, lang)
            }
        }),
    }
}

pub(crate) fn run_parse_errors(source: &str, lang: &str) -> Result<Value, String> {
    let mut parser = Parser::new();
    parser
        .set_language(&language_for(lang)?)
        .map_err(|e| format!("setting language: {e}"))?;

    let tree = parser.parse(source, None).ok_or("parse returned no tree")?;

    let root = tree.root_node();
    let mut errors: Vec<ParseError> = Vec::new();
    collect_errors(root, source, &mut errors);

    Ok(json!({
        "ok": errors.is_empty(),
        "errors": errors,
    }))
}

#[derive(serde::Serialize)]
struct ParseError {
    line: usize,
    col: usize,
    message: String,
}

fn collect_errors(node: Node, source: &str, errors: &mut Vec<ParseError>) {
    if node.is_error() || node.is_missing() {
        let pos = node.start_position();
        let msg = if node.is_missing() {
            let kind = node.kind();
            format!("missing {kind}")
        } else {
            let text = node.utf8_text(source.as_bytes()).ok();
            let preview = text.map(|t| clip(t, 60)).unwrap_or_default();
            if preview.is_empty() {
                "syntax error".to_string()
            } else {
                format!("syntax error near `{preview}`")
            }
        };
        errors.push(ParseError {
            line: (pos.row + 1),
            col: (pos.column + 1),
            message: msg,
        });
        return; // Don't recurse into error nodes — their children are also errors.
    }

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_errors(child, source, errors);
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn call_handler(def: &ToolDef, args: Value) -> Result<Value, String> {
        (def.handler)(args)
    }

    fn temp_path_with_ext(ext: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(format!("test.{ext}"));
        (dir, path)
    }

    #[test]
    fn outline_rust_lists_definitions() {
        let (_dir, path) = temp_path_with_ext("rs");
        std::fs::write(&path, "fn hello() {}\nstruct Point {}\n").unwrap();
        let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
        let arr = result["items"].as_array().unwrap();
        assert!(!arr.is_empty(), "expected non-empty outline");
        let kinds: Vec<&str> = arr.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"function"), "missing function in {arr:?}");
        assert!(kinds.contains(&"struct"), "missing struct in {arr:?}");
    }

    /// **One filter works on any file.** `kind` used to be the
    /// tree-sitter node kind, so the same concept had four names and
    /// the word every model reaches for matched none of them: across
    /// the kept corpus, programs compared `kind` against `"function"`
    /// 26 times and against `"function_definition"` 17, so the wrong
    /// guess was the commoner one. It fails silently — an empty
    /// `filter` is not an error — and one `sweep-8` run read that
    /// emptiness as "nothing here is live" and replaced the whole of
    /// `helpers.py` with its docstring.
    #[test]
    fn a_function_is_called_a_function_in_every_language() {
        for (ext, src) in [
            ("rs", "fn hello() {}\n"),
            ("py", "def hello():\n    pass\n"),
            ("js", "function hello() {}\n"),
            ("ts", "function hello(): void {}\n"),
        ] {
            let (_dir, path) = temp_path_with_ext(ext);
            std::fs::write(&path, src).unwrap();
            let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
            let arr = result["items"].as_array().unwrap();
            let kinds: Vec<&str> = arr.iter().map(|e| e["kind"].as_str().unwrap()).collect();
            assert!(kinds.contains(&"function"), "{ext}: {arr:?}");
        }

        // The distinctions the grammars make are kept — a struct and an
        // enum do not both become "type".
        let (_dir, path) = temp_path_with_ext("rs");
        std::fs::write(&path, "struct P {}\nenum E { A }\ntrait T {}\n").unwrap();
        let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
        let kinds: Vec<&str> = result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        for want in ["struct", "enum", "trait"] {
            assert!(kinds.contains(&want), "missing {want} in {kinds:?}");
        }

        // Which words the declaration may use at all is
        // `the_declared_kinds_are_exactly_the_kinds_produced`.
        assert!(
            !outline_def().returns.unwrap().contains("\"method\""),
            "a method is a `function` here, in every language — see readable_kind"
        );
    }

    /// **The declared vocabulary and the produced one are the same
    /// set, checked in both directions.**
    ///
    /// A word the declaration names that nothing emits is worse than no
    /// word: `"method"` sat in this union unreachable for as long as
    /// nothing descended into a class, so a program could filter on it
    /// forever and get an empty array rather than an error — the
    /// failure mode that emptied `helpers.py`. The other direction
    /// catches a grammar kind falling through `readable_kind`'s
    /// `other => other` arm, which ships the model a raw tree-sitter
    /// name it was never told about.
    #[test]
    fn the_declared_kinds_are_exactly_the_kinds_produced() {
        let returns = outline_def().returns.unwrap();
        let union = {
            let after = returns.split_once("kind: ").expect("a `kind:` field").1;
            after.split_once(';').expect("the field ends").0.to_owned()
        };
        let declared: std::collections::BTreeSet<String> = union
            .split('|')
            .map(|w| w.trim().trim_matches('"').to_owned())
            .collect();

        // Every kind this can report, from one file per language that
        // uses each construct once.
        let mut produced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (ext, src) in [
            (
                "rs",
                "fn f() {}\nstruct S;\nenum E { A }\ntrait T { fn s(&self); type A; }\n\
                 impl S {}\nmod m {}\nconst C: u32 = 1;\nstatic X: u32 = 1;\n\
                 type Y = u32;\nmacro_rules! mac { () => {} }\n",
            ),
            (
                "ts",
                "interface I { a: number; m(): void }\nclass C { f = 1 }\n\
                 namespace N { const v = 1; }\n",
            ),
        ] {
            let (_dir, path) = temp_path_with_ext(ext);
            std::fs::write(&path, src).unwrap();
            let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
            for e in result["items"].as_array().unwrap() {
                produced.insert(e["kind"].as_str().unwrap().to_owned());
            }
        }

        assert_eq!(
            declared, produced,
            "the declared kinds and the produced kinds have drifted apart"
        );
    }

    #[test]
    fn outline_carries_attributes_and_a_doc_summary() {
        // The question try8 asked and could not get answered: which
        // definitions carry `#[allow(dead_code)]`. An index that omits
        // attributes cannot answer a question about attributes, and the
        // run spent sixteen calls finding that out.
        let (_dir, path) = temp_path_with_ext("rs");
        std::fs::write(
            &path,
            "/// Does the thing, briefly.\n\
             /// More detail nobody needs in an index.\n\
             #[allow(dead_code)]\n\
             #[inline]\n\
             pub fn probe(x: u32) -> u32 { x }\n\
             \n\
             // an ordinary note, not a summary\n\
             pub struct Plain;\n",
        )
        .unwrap();
        let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
        let arr = result["items"].as_array().unwrap();

        let probe = arr.iter().find(|e| e["name"] == "probe").expect("probe");
        let attrs: Vec<&str> = probe["attributes"]
            .as_array()
            .expect("attributes")
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(attrs, vec!["#[allow(dead_code)]", "#[inline]"]);
        assert_eq!(
            probe["doc"], "Does the thing, briefly.",
            "the first line only -- this stays an index"
        );

        // A plain `//` note is about the code, not a summary of it.
        let plain = arr.iter().find(|e| e["name"] == "Plain").expect("Plain");
        assert!(plain.get("doc").is_none(), "{plain:?}");
        assert!(plain.get("attributes").is_none(), "{plain:?}");
    }

    /// **The tests are in the outline.** A live run asked to explain a
    /// Rust test called `outline("agent/src/compaction.rs")`, searched
    /// `items` for the test by name, found nothing and burned two turns
    /// working out why: "the outline only lists top-level definitions,
    /// so the `#[cfg(test)]` module's tests are not in it". In this
    /// codebase the tests carry most of the documentation, so an index
    /// blind to them hides the majority of what is worth finding.
    ///
    /// `name` stays bare — the search that failed was by name, and
    /// qualifying it would have left that search failing for a second
    /// reason. `parent` is what tells the two `compact`s apart.
    #[test]
    fn a_test_inside_cfg_test_mod_tests_is_in_the_outline() {
        let (_dir, path) = temp_path_with_ext("rs");
        std::fs::write(
            &path,
            "pub fn compact() {}\n\
             \n\
             #[cfg(test)]\n\
             mod tests {\n\
             \x20   use super::*;\n\
             \x20   #[test]\n\
             \x20   fn renders_a_line() {}\n\
             \x20   fn compact() {}\n\
             \x20   mod inner {\n\
             \x20       fn deep() {}\n\
             \x20   }\n\
             }\n",
        )
        .unwrap();
        let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
        let arr = result["items"].as_array().unwrap();

        let by_name = |n: &str| -> Vec<&Value> { arr.iter().filter(|e| e["name"] == n).collect() };

        let rendered = by_name("renders_a_line");
        assert_eq!(rendered.len(), 1, "{arr:#?}");
        assert_eq!(rendered[0]["parent"], "tests");
        assert_eq!(rendered[0]["kind"], "function");
        // The attribute that made it a test comes with it, which is how
        // a caller tells a test from a helper beside it.
        assert_eq!(rendered[0]["attributes"][0], "#[test]");

        // Same name at two scopes, told apart by `parent` and by nothing
        // else: this is the case the field exists for.
        let compacts = by_name("compact");
        assert_eq!(compacts.len(), 2, "{arr:#?}");
        assert!(compacts.iter().any(|e| e.get("parent").is_none()));
        assert!(compacts.iter().any(|e| e["parent"] == "tests"));

        // Nesting composes, in the notation the language writes.
        let deep = by_name("deep");
        assert_eq!(deep.len(), 1, "{arr:#?}");
        assert_eq!(deep[0]["parent"], "tests::inner");
    }

    /// **A method is a function, and `parent` says whose.** Rust's
    /// `impl`, Python's `class` and JS's `class` all hide their bodies
    /// from an outline that stops at the first definition on a path;
    /// once it descends, the three had better agree on what they call
    /// what they found. `method_definition` rendering as `"method"`
    /// would have meant `filter(i => i.kind === "function")` silently
    /// skipping JS methods and no others — the same silent-empty
    /// failure `readable_kind` was written to end.
    #[test]
    fn a_method_is_a_function_with_a_parent_in_every_language() {
        for (ext, src, parent) in [
            (
                "rs",
                "struct S;\nimpl S {\n    fn probe(&self) {}\n}\n",
                "S",
            ),
            ("py", "class S:\n    def probe(self):\n        pass\n", "S"),
            ("js", "class S {\n    probe() {}\n}\n", "S"),
            ("ts", "class S {\n    probe(): void {}\n}\n", "S"),
        ] {
            let (_dir, path) = temp_path_with_ext(ext);
            std::fs::write(&path, src).unwrap();
            let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
            let arr = result["items"].as_array().unwrap();
            let probe = arr
                .iter()
                .find(|e| e["name"] == "probe")
                .unwrap_or_else(|| panic!("{ext}: no `probe` in {arr:#?}"));
            assert_eq!(probe["kind"], "function", "{ext}: {probe:?}");
            assert_eq!(probe["parent"], parent, "{ext}: {probe:?}");
        }
    }

    /// **Containers, not everything.** An outline is a list of places
    /// you can navigate to and name; a helper defined inside a function
    /// body is not one, and listing it would turn the index back into a
    /// parse dump — which is the cost that makes descending into
    /// modules affordable at all.
    #[test]
    fn a_definition_inside_a_function_body_stays_out() {
        for (ext, src) in [
            ("rs", "fn outer() {\n    fn helper() {}\n}\n"),
            ("py", "def outer():\n    def helper():\n        pass\n"),
            ("js", "function outer() {\n    function helper() {}\n}\n"),
        ] {
            let (_dir, path) = temp_path_with_ext(ext);
            std::fs::write(&path, src).unwrap();
            let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
            let names: Vec<&str> = result["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, vec!["outer"], "{ext}");
        }
    }

    /// **The half of a trait that is the contract was the half that
    /// was missing.** Probing
    /// `trait T { fn sig(&self); fn deflt(&self) {} type A; const C: u32 = 1; }`
    /// gave `T`, `deflt` and `C` — and not `sig`, not `A`. A method
    /// showed *because it had a body*: `fn deflt(&self) {}` is a
    /// `function_item` where `fn sig(&self);` is a
    /// `function_signature_item`, and only the first was a definition.
    /// So a trait's optional part was listed and its required part,
    /// which is the part anyone outlining the file wants, was not.
    ///
    /// `extern "C" { fn foo(); }` was empty for the same reason.
    #[test]
    fn a_declaration_without_a_body_is_still_a_declaration() {
        let (_dir, path) = temp_path_with_ext("rs");
        std::fs::write(
            &path,
            "trait T {\n\
             \x20   fn sig(&self);\n\
             \x20   fn deflt(&self) {}\n\
             \x20   type A;\n\
             \x20   const C: u32 = 1;\n\
             }\n\
             unsafe extern \"C\" {\n\
             \x20   fn foo();\n\
             }\n",
        )
        .unwrap();
        let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
        let arr = result["items"].as_array().unwrap();
        let find = |n: &str| {
            arr.iter()
                .find(|e| e["name"] == n)
                .unwrap_or_else(|| panic!("no `{n}` in {arr:#?}"))
        };

        // The required method and the default are the same species.
        assert_eq!(find("sig")["kind"], "function");
        assert_eq!(find("sig")["parent"], "T");
        assert_eq!(find("deflt")["kind"], find("sig")["kind"]);
        // `type A;` and `type A = u32;` likewise.
        assert_eq!(find("A")["kind"], "type");
        assert_eq!(find("A")["parent"], "T");
        assert_eq!(find("C")["kind"], "const");
        // An `extern` block is not a scope, so its contents sit where
        // the block does.
        assert_eq!(find("foo")["kind"], "function");
        assert!(find("foo").get("parent").is_none(), "{:?}", find("foo"));
    }

    /// The same hole in TypeScript, which has four shapes of it: an
    /// `interface`'s members, an `abstract` method, a class field and
    /// `declare function`. An `interface` is the one construct in the
    /// language that is *only* a list of members, and it was reported
    /// as a single line.
    #[test]
    fn typescript_declarations_without_bodies_are_listed() {
        let (_dir, path) = temp_path_with_ext("ts");
        std::fs::write(
            &path,
            "interface I {\n\
             \x20   a: number;\n\
             \x20   m(): void;\n\
             }\n\
             abstract class C {\n\
             \x20   abstract am(): void;\n\
             \x20   f: number = 1;\n\
             }\n\
             declare function g(): void;\n\
             namespace N {\n\
             \x20   export function h() {}\n\
             }\n",
        )
        .unwrap();
        let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
        let arr = result["items"].as_array().unwrap();
        let find = |n: &str| {
            arr.iter()
                .find(|e| e["name"] == n)
                .unwrap_or_else(|| panic!("no `{n}` in {arr:#?}"))
        };

        assert_eq!(find("I")["kind"], "interface");
        assert_eq!(find("m")["kind"], "function");
        assert_eq!(find("m")["parent"], "I");
        assert_eq!(find("a")["kind"], "field");
        assert_eq!(find("a")["parent"], "I");
        assert_eq!(find("am")["kind"], "function");
        assert_eq!(find("am")["parent"], "C");
        assert_eq!(find("f")["kind"], "field");
        assert_eq!(find("g")["kind"], "function");
        // A namespace is a scope, so what it holds says so — these used
        // to surface at file level with no `parent` at all, which is
        // the ambiguity `parent` exists to remove.
        assert_eq!(find("N")["kind"], "module");
        assert_eq!(find("h")["parent"], "N");
    }

    /// **`"variable"` was declared and unreachable**, which the
    /// kind-parity test is what found. `lexical_declaration` carries no
    /// `name` field — the name is a level down on its
    /// `variable_declarator` — so `find_name` returned `None` and the
    /// entry was dropped: a JavaScript module of exported constants
    /// outlined to nothing at all.
    #[test]
    fn a_const_at_file_level_is_in_the_outline() {
        for ext in ["js", "ts"] {
            let (_dir, path) = temp_path_with_ext(ext);
            std::fs::write(
                &path,
                "const LIMIT = 10;\nlet cursor = 0;\nfor (const row of rows) { use(row); }\n",
            )
            .unwrap();
            let result = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap();
            let arr = result["items"].as_array().unwrap();
            let names: Vec<&str> = arr.iter().map(|e| e["name"].as_str().unwrap()).collect();
            assert!(names.contains(&"LIMIT"), "{ext}: {arr:#?}");
            assert!(names.contains(&"cursor"), "{ext}: {arr:#?}");
            // A loop's own binding is bookkeeping, not a definition.
            assert!(!names.contains(&"row"), "{ext}: {arr:#?}");
        }
    }

    #[test]
    fn outline_unsupported_extension_errors() {
        let (_dir, path) = temp_path_with_ext("xyz");
        std::fs::write(&path, "content").unwrap();
        let err = call_handler(&outline_def(), json!([path.to_str().unwrap()])).unwrap_err();
        assert!(err.contains("unsupported file extension"), "got: {err}");
    }

    #[test]
    fn parse_errors_ok_for_valid_source() {
        let (_dir, path) = temp_path_with_ext("js");
        std::fs::write(&path, "let x = 1;\nlet y = 2;\n").unwrap();
        let result = call_handler(&parse_errors_def(), json!([path.to_str().unwrap()])).unwrap();
        assert_eq!(result["ok"], json!(true));
        assert!(result["errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn parse_errors_soft_skips_unsupported_extension() {
        // A validation sweep over mixed files must not detonate on a
        // non-code file: `parse_errors` soft-skips (vs. `outline`, strict).
        let (_dir, path) = temp_path_with_ext("html");
        std::fs::write(&path, "<!DOCTYPE html><html></html>").unwrap();
        let result = call_handler(&parse_errors_def(), json!([path.to_str().unwrap()])).unwrap();
        assert_eq!(result["ok"], json!(null));
        assert!(
            result["skipped"].as_str().unwrap().contains(".html"),
            "got: {result}"
        );
    }

    #[test]
    fn parse_errors_detects_broken_brace() {
        let (_dir, path) = temp_path_with_ext("js");
        std::fs::write(&path, "let x = 1;\nlet y = {\n").unwrap();
        let result = call_handler(&parse_errors_def(), json!([path.to_str().unwrap()])).unwrap();
        assert_eq!(result["ok"], json!(false));
        assert!(!result["errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn parse_errors_source_form_no_disk_touch() {
        let result = run_parse_errors("fn foo() {}\nstruct Bar {}\n", "rust").unwrap();
        assert_eq!(result["ok"], json!(true));

        let result = run_parse_errors("fn broken {", "rust").unwrap();
        assert_eq!(result["ok"], json!(false));
        assert!(!result["errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn parse_errors_reports_positions() {
        let errors = run_parse_errors("fn valid() {}\nfn broken {", "rust").unwrap();
        assert_eq!(errors["ok"], json!(false));
        let list = errors["errors"].as_array().unwrap();
        let has_line2 = list.iter().any(|e| e["line"].as_u64().unwrap() >= 2);
        assert!(has_line2, "expected error on line 2+, got {list:?}");
    }

    #[test]
    fn parse_errors_unknown_language_errors() {
        let err = run_parse_errors("code", "latin").unwrap_err();
        assert!(err.contains("unknown language"), "got: {err}");
    }
}
