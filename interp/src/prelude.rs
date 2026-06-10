//! Higher-order array-method prelude (Phase 4.0).
//!
//! The VM has no iterator/prototype machinery, so `arr.map(cb)` and friends are
//! lowered to a static `Call` of a small JS helper (`__map(a, f)`, …). The
//! helpers are **real JS source** compiled in the same unit as the user program
//! (appended once, see [`assemble`]), so their bindings hoist and their entry
//! labels resolve through the normal analyzer/backpatch path — no
//! hand-fabricated AST nodes with synthetic spans (COMPILER_PLAN §4.4). Each
//! helper is **self-contained** (it references only its own params and locals,
//! so it has no captures), which lets the call site use a static `Call` and the
//! VM skip upval installation. The user callback is invoked per element via
//! `CallDyn` (extra `(elem, i, arr)` args past the callback's arity are simply
//! cleaned up by `Return`, matching JS `cb(element, index, array)`).
//!
//! Only the helpers a program actually uses are appended (tree-shaking), so a
//! program that uses no higher-order methods compiles byte-for-byte unchanged.
//!
//! A **bare namespaced builtin** works as the callback (`arr.map(Math.sqrt)`):
//! the helper invokes it with `(element, index, array)` and builtins ignore
//! surplus arguments (flexible arity from `Builtin::meta`), just like user
//! functions/closures map params positionally and ignore the rest.

/// One higher-order method: the call-site method name and the JS source of the
/// helper(s) it lowers to. The helper **names** referenced by the compiler
/// (`compiler::Compiler::compile_method_call`) must match the `function`
/// names here; the per-method helper name + arity lives there.
struct Hof {
    /// The method name as written on the receiver (`arr.map` → `"map"`).
    method: &'static str,
    /// The helper function declaration(s) this method needs. `reduce` carries
    /// both the with-initial-value and no-initial-value forms.
    source: &'static str,
}

/// The higher-order methods recognized on array receivers, each paired with its
/// prelude helper source. All helpers take the receiver as the first parameter
/// (`a`) and the callback as the second (`f`); `reduce` adds the accumulator.
const HOFS: &[Hof] = &[
    Hof {
        method: "map",
        source: "function __map(a, f) {\n  const r = [];\n  for (let i = 0; i < a.length; i++) { r.push(f(a[i], i, a)); }\n  return r;\n}",
    },
    Hof {
        method: "filter",
        source: "function __filter(a, f) {\n  const r = [];\n  for (let i = 0; i < a.length; i++) { const v = a[i]; if (f(v, i, a)) { r.push(v); } }\n  return r;\n}",
    },
    Hof {
        method: "forEach",
        source: "function __forEach(a, f) {\n  for (let i = 0; i < a.length; i++) { f(a[i], i, a); }\n  return undefined;\n}",
    },
    Hof {
        method: "some",
        source: "function __some(a, f) {\n  for (let i = 0; i < a.length; i++) { if (f(a[i], i, a)) { return true; } }\n  return false;\n}",
    },
    Hof {
        method: "every",
        source: "function __every(a, f) {\n  for (let i = 0; i < a.length; i++) { if (!f(a[i], i, a)) { return false; } }\n  return true;\n}",
    },
    Hof {
        method: "find",
        source: "function __find(a, f) {\n  for (let i = 0; i < a.length; i++) { const v = a[i]; if (f(v, i, a)) { return v; } }\n  return undefined;\n}",
    },
    Hof {
        method: "findIndex",
        source: "function __findIndex(a, f) {\n  for (let i = 0; i < a.length; i++) { if (f(a[i], i, a)) { return i; } }\n  return -1;\n}",
    },
    Hof {
        // Two forms: `reduce(f, init)` -> `__reduce`; `reduce(f)` (no initial
        // value, seed with element 0) -> `__reduce1`. The call site picks by
        // argument count.
        method: "reduce",
        source: "function __reduce(a, f, acc) {\n  for (let i = 0; i < a.length; i++) { acc = f(acc, a[i], i, a); }\n  return acc;\n}\nfunction __reduce1(a, f) {\n  let acc = a[0];\n  for (let i = 1; i < a.length; i++) { acc = f(acc, a[i], i, a); }\n  return acc;\n}",
    },
    Hof {
        method: "flatMap",
        source: "function __flatMap(a, f) {\n  const r = [];\n  for (let i = 0; i < a.length; i++) { const v = f(a[i], i, a); for (let j = 0; j < v.length; j++) { r.push(v[j]); } }\n  return r;\n}",
    },
    Hof {
        method: "findLast",
        source: "function __findLast(a, f) {\n  for (let i = a.length - 1; i >= 0; i--) { const v = a[i]; if (f(v, i, a)) { return v; } }\n  return undefined;\n}",
    },
    Hof {
        method: "findLastIndex",
        source: "function __findLastIndex(a, f) {\n  for (let i = a.length - 1; i >= 0; i--) { if (f(a[i], i, a)) { return i; } }\n  return -1;\n}",
    },
    Hof {
        method: "sort",
        source: "function __sort(a, f) {\n  for (let i = 1; i < a.length; i++) {\n    const key = a[i];\n    let j = i - 1;\n    while (j >= 0 && f(a[j], key) > 0) { a[j + 1] = a[j]; j--; }\n    a[j + 1] = key;\n  }\n  return a;\n}\nfunction __sortDefault(a) {\n  for (let i = 1; i < a.length; i++) {\n    const key = a[i];\n    let j = i - 1;\n    while (j >= 0 && String(a[j]) > String(key)) { a[j + 1] = a[j]; j--; }\n    a[j + 1] = key;\n  }\n  return a;\n}",
    },
];

/// Build the prelude source to append to `user_source`: the concatenated source
/// of every helper whose method the program uses. Returns an empty string when
/// the program uses no higher-order methods (so it compiles unchanged).
pub fn assemble(user_source: &str) -> String {
    let mut out = String::new();
    for hof in HOFS {
        if uses_method(user_source, hof.method) {
            out.push('\n');
            out.push_str(hof.source);
            out.push('\n');
        }
    }
    out
}

/// Whether `source` contains a method call `recv.<method>(…)`.
///
/// A real (non-computed) member call always contains the literal token
/// `.<method>` in source, with the next character not continuing an identifier
/// — so this is exact for supported syntax: there are **no false negatives**.
/// A false positive (matching `.mapKey` for `map` is prevented by the boundary
/// check; an unrelated `.find` while only `findIndex` is used would merely
/// append an unused helper) is harmless — it never changes behavior.
fn uses_method(source: &str, method: &str) -> bool {
    let needle = format!(".{method}");
    source.match_indices(&needle).any(|(i, _)| {
        let after = i + needle.len();
        source[after..]
            .chars()
            .next()
            // EOF, `(`, whitespace, `?` … all end the token; an identifier char
            // means this is a longer name (`.mapping`), not `method`.
            .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '$')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_used_methods_only() {
        // No HOF → empty prelude.
        assert!(assemble("let x = 1 + 2;").is_empty());
        // Used method → its helper appears; unused ones do not.
        let p = assemble("state.r = [1, 2].map(x => x * 2);");
        assert!(p.contains("function __map"));
        assert!(!p.contains("function __filter"));
    }

    #[test]
    fn find_vs_find_index_boundary() {
        // `findIndex` must not pull in `find`, and vice-versa.
        let p = assemble("state.r = a.findIndex(x => x);");
        assert!(p.contains("function __findIndex"));
        assert!(!p.contains("function __find("));
        let p = assemble("state.r = a.find(x => x);");
        assert!(p.contains("function __find("));
        assert!(!p.contains("function __findIndex"));
    }

    #[test]
    fn longer_identifier_does_not_trigger() {
        // `.mapping` is not a `map` call.
        assert!(assemble("state.r = obj.mapping;").is_empty());
    }

    #[test]
    fn reduce_pulls_both_forms() {
        let p = assemble("state.r = a.reduce((s, x) => s + x, 0);");
        assert!(p.contains("function __reduce("));
        assert!(p.contains("function __reduce1("));
    }

    // ── Step 4d: prelude additions ────────────────────────────────────

    #[test]
    fn sort_default_comparison() {
        // JS: [10, 9, 1].sort() → [1, 10, 9] (string comparison)
        assert_eq!(
            crate::testutil::run_ret("return [10,9,1].sort();"),
            serde_json::json!([1, 10, 9])
        );
    }

    #[test]
    fn sort_with_comparator() {
        // JS: [10, 9, 1].sort((x, y) => x - y) → [1, 9, 10]
        assert_eq!(
            crate::testutil::run_ret("return [10,9,1].sort((x, y) => x - y);"),
            serde_json::json!([1, 9, 10])
        );
    }

    #[test]
    fn sort_returns_mutated_array() {
        // Sort is in-place and returns the receiver.
        assert_eq!(
            crate::testutil::run_ret(
                "const a = [3,1,2]; const r = a.sort((x,y)=>x-y); return [r === a, a];"
            ),
            serde_json::json!([true, [1, 2, 3]])
        );
    }

    #[test]
    fn flat_map() {
        assert_eq!(
            crate::testutil::run_ret("return [[1],[2,3]].flatMap(x => x);"),
            serde_json::json!([1, 2, 3])
        );
    }

    #[test]
    fn find_last() {
        assert_eq!(
            crate::testutil::run_ret("return [1,2,3,2].findLast(x => x < 3);"),
            serde_json::json!(2)
        );
    }

    #[test]
    fn find_last_index() {
        assert_eq!(
            crate::testutil::run_ret("return [1,2,3,2].findLastIndex(x => x < 3);"),
            serde_json::json!(3)
        );
    }
}
