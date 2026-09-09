//! The no-fence rule (phase 20 doc, Part A "The transport").
//!
//! The card states absolutely that a completion is only ever valid
//! JavaScript — no code fence, no surrounding prose. That is
//! self-enforcing (a completion that doesn't parse is already a
//! condition with a handler), but a model habitually wraps its answer
//! in a ```javascript fence anyway. [`extract`] tolerates exactly
//! that one habit, silently, and nothing else: it does not hunt for
//! prose, does not try to salvage a program buried in an explanation,
//! and does not advertise the leniency anywhere the model can see it.

/// Strip a single leading/trailing code fence if the **whole**
/// trimmed response is wrapped in one — never a fence appearing
/// mid-text, which is left alone per Step A1: "reserve the
/// parse-failure condition for genuine syntax errors."
///
/// Recognizes an optional language tag on the opening fence
/// (```javascript, ```js, or bare ```) and requires a matching
/// closing ``` as the last non-blank line, so a program that
/// legitimately contains a ``` in a string or comment is not
/// mis-stripped.
pub fn extract(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return raw.to_owned();
    };
    // The rest of the opening fence line is a language tag (or
    // nothing) — skip to the first newline.
    let Some(nl) = after_open.find('\n') else {
        return raw.to_owned();
    };
    let tag = after_open[..nl].trim();
    if !(tag.is_empty() || tag.eq_ignore_ascii_case("javascript") || tag.eq_ignore_ascii_case("js"))
    {
        return raw.to_owned();
    }
    let body = &after_open[nl + 1..];
    let Some(body) = body.strip_suffix("```") else {
        return raw.to_owned();
    };
    body.trim_end_matches('\n').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_fence_is_returned_unchanged() {
        assert_eq!(extract("const x = 1;"), "const x = 1;");
    }

    #[test]
    fn a_stray_javascript_fence_is_stripped_silently() {
        assert_eq!(extract("```javascript\nconst x = 1;\n```"), "const x = 1;");
    }

    #[test]
    fn a_bare_fence_with_no_language_tag_is_stripped() {
        assert_eq!(extract("```\nconst x = 1;\n```"), "const x = 1;");
    }

    #[test]
    fn js_tag_is_also_recognized() {
        assert_eq!(extract("```js\nconst x = 1;\n```"), "const x = 1;");
    }

    #[test]
    fn a_fence_appearing_mid_text_is_left_alone() {
        // Not wrapped end-to-end — this is a genuine syntax error to
        // report as a trap, not something to salvage.
        let raw = "const s = \"```\";\nsay(s);";
        assert_eq!(extract(raw), raw);
    }

    #[test]
    fn an_unmatched_opening_fence_is_left_alone() {
        let raw = "```javascript\nconst x = 1;";
        assert_eq!(extract(raw), raw);
    }

    #[test]
    fn a_fence_with_an_unrecognized_tag_is_left_alone() {
        let raw = "```python\nx = 1\n```";
        assert_eq!(extract(raw), raw);
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(
            extract("  \n```javascript\nconst x = 1;\n```\n  "),
            "const x = 1;"
        );
    }
}
