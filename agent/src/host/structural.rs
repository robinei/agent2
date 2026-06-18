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
        description: "Read a source file and list its top-level definitions \
                      (functions, classes, modules, …). Language is inferred from \
                      the extension. Read-only — no mutation."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                { "type": "string", "description": "absolute or cwd-relative path" }
            ],
            "minItems": 1,
            "maxItems": 1
        }),
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
    Ok(serde_json::to_value(entries).unwrap_or(json!([])))
}

#[derive(serde::Serialize)]
struct OutlineEntry {
    name: String,
    kind: String,
    start_line: usize,
    end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

fn collect_definitions(node: Node, source: &str, lang: &str) -> Vec<OutlineEntry> {
    let mut entries = Vec::new();
    collect_definitions_impl(node, source, lang, &mut entries);
    entries
}

fn collect_definitions_impl(node: Node, source: &str, lang: &str, entries: &mut Vec<OutlineEntry>) {
    let is_def = is_definition_node(node.kind(), lang);
    let mut def_children: Vec<Node> = Vec::with_capacity(node.named_child_count());
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i)
            && child.is_named()
        {
            def_children.push(child);
            if !is_definition_node(child.kind(), lang) {
                collect_definitions_impl(child, source, lang, entries);
            }
        }
    }

    if is_def {
        let name = find_name(&node, source);
        if let Some(name) = name {
            let start = node.start_position();
            let end = node.end_position();
            let sig = signature_for(&node, source, lang);
            entries.push(OutlineEntry {
                name,
                kind: node.kind().to_string(),
                start_line: (start.row + 1),
                end_line: (end.row + 1),
                signature: sig,
            });
        }
    }

    // Process def children that were deferred.
    for child in def_children {
        if is_definition_node(child.kind(), lang) {
            let name = find_name(&child, source);
            if let Some(name) = name {
                let start = child.start_position();
                let end = child.end_position();
                let sig = signature_for(&child, source, lang);
                entries.push(OutlineEntry {
                    name,
                    kind: child.kind().to_string(),
                    start_line: (start.row + 1),
                    end_line: (end.row + 1),
                    signature: sig,
                });
            }
        }
    }
}

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
        ),
        "javascript" => matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "method_definition"
                | "lexical_declaration"
                | "variable_declaration"
        ),
        "typescript" => matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "method_definition"
                | "lexical_declaration"
                | "variable_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "abstract_class_declaration"
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
        if child.child_count() == 0 && child.kind() == "identifier" {
            return Some(child.utf8_text(source.as_bytes()).ok()?.to_string());
        }
        if child.child_count() == 0
            && (child.kind() == "property_identifier" || child.kind() == "type_identifier")
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
        description: "Verify syntax: `parse_errors([path])` reads a file and \
                      checks it; `parse_errors([null, source, lang])` checks \
                      candidate content the program computed **before writing** — \
                      no disk touch. Returns { ok, errors: [{ line, col, message }] }; \
                      a path with no structural parser (e.g. .html, .css) returns \
                      { ok: null, skipped } so a sweep over mixed files is safe. \
                      Supported languages: rust, javascript, typescript, python."
            .into(),
        input_schema: json!({
            "type": "array",
            "items": [
                {
                    "oneOf": [
                        { "type": "string", "description": "absolute or cwd-relative path" },
                        { "type": "null", "description": "null — use source+lang form" }
                    ]
                },
                { "type": "string", "description": "source text (required when first arg is null)" },
                { "type": "string", "description": "language name (required when first arg is null)" }
            ],
            "minItems": 1,
            "maxItems": 3
        }),

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

fn run_parse_errors(source: &str, lang: &str) -> Result<Value, String> {
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
        let arr = result.as_array().unwrap();
        assert!(!arr.is_empty(), "expected non-empty outline");
        let kinds: Vec<&str> = arr.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert!(
            kinds.contains(&"function_item"),
            "missing function in {arr:?}"
        );
        assert!(kinds.contains(&"struct_item"), "missing struct in {arr:?}");
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
