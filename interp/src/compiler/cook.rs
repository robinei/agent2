//! Source literals that can hold a lone surrogate.
//!
//! **This is the capability the whole representation change exists for.**
//! A `JsString` can hold an unpaired surrogate; until this module, a *source
//! literal* could not produce one, so `"\uD800"` could not be written and the
//! change reduced to code-unit indexing — which the far cheaper
//! "UTF-8 with an `is_ascii` bit" option would have delivered for a fifth of
//! the work. `docs/30_STRINGS.md` calls that out as the sentence that decides
//! whether any of this was worth doing.
//!
//! # What this is not
//!
//! `30_STRINGS.md` planned a ≈250-line re-cooker reading the raw source slice
//! (`lit.span`) and handling `\xNN`, `\uXXXX`, `\u{…}`, surrogate pairs, line
//! continuations and legacy octal for itself. **That plan was wrong about
//! oxc**, and it is worth saying which half: the doc says oxc's `value` "is an
//! `Atom` backed by `&str`, which structurally cannot hold a lone surrogate
//! **and does not say so**". The first half is true; the second is not, as of
//! oxc 0.134.
//!
//! `StringLiteral` and `TemplateElement` each carry a `lone_surrogates: bool`,
//! and when it is set, `value` is a documented in-band encoding: each lone
//! surrogate appears as U+FFFD followed by its code unit as four hex digits,
//! and a genuine U+FFFD in the source appears as U+FFFD followed by `fffd`.
//!
//! Measured on oxc 0.134 before this was written:
//!
//! ```text
//!   "\uD800"          lone=true   units [FFFD, 'd', '8', '0', '0']
//!   "a\uD800b"        lone=true   units ['a', FFFD, 'd','8','0','0', 'b']
//!   "�"          lone=false  units [FFFD]
//!   "😀"    lone=false  units [D83D, DE00]   (already paired)
//!   `a\uD800b`        lone=true   cooked as above
//! ```
//!
//! So the whole job is to undo that encoding — about forty lines, with oxc
//! still doing every escape form it already did. Re-cooking from raw source
//! would have meant maintaining a second implementation of JS string escapes
//! beside the parser's, and being subtly wrong about one of them.
//!
//! The decode is uniform because the two cases collapse: the four hex digits
//! after U+FFFD are the code unit to emit *whatever* they say, and `fffd`
//! emits U+FFFD, which is exactly what the escaped-escape means.

use oxc_ast::ast;

use crate::vm::JsString;

/// The replacement character, which oxc uses as its in-band escape.
const ESCAPE: char = '\u{FFFD}';

/// Decode oxc's lone-surrogate encoding into code units.
///
/// Only called when the literal's `lone_surrogates` flag is set — for every
/// other literal the `&str` is the value and `JsString::from` is exact.
fn decode_lone_surrogates(value: &str) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != ESCAPE {
            let mut buf = [0u16; 2];
            out.extend_from_slice(c.encode_utf16(&mut buf));
            continue;
        }
        // Four hex digits follow. A malformed encoding is not reachable from
        // the parser, so a missing digit means oxc changed its contract — emit
        // the replacement character itself rather than silently dropping text.
        let mut unit: u32 = 0;
        let mut digits = 0;
        for _ in 0..4 {
            match chars.next().and_then(|d| d.to_digit(16)) {
                Some(d) => {
                    unit = unit * 16 + d;
                    digits += 1;
                }
                None => break,
            }
        }
        if digits == 4 {
            out.push(unit as u16);
        } else {
            out.push(ESCAPE as u16);
        }
    }
    out
}

/// The code units of a string literal, lone surrogates included.
pub(crate) fn string_literal_units(lit: &ast::StringLiteral) -> Vec<u16> {
    if lit.lone_surrogates {
        decode_lone_surrogates(lit.value.as_str())
    } else {
        crate::units::from_str(lit.value.as_str())
    }
}

/// [`string_literal_units`] as a `JsString`, for the sites that do not intern.
pub(crate) fn string_literal(lit: &ast::StringLiteral) -> JsString {
    if lit.lone_surrogates {
        JsString::from_units(&decode_lone_surrogates(lit.value.as_str()))
    } else {
        JsString::from(lit.value.as_str())
    }
}

/// The code units of one template quasi's *cooked* value.
///
/// A quasi with no cooked value is one whose escapes are invalid — legal only
/// in a tagged template, which this dialect does not have — so the raw text is
/// the only thing left to use, exactly as before.
pub(crate) fn template_element_units(q: &ast::TemplateElement) -> Vec<u16> {
    match q.value.cooked.as_ref() {
        Some(cooked) if q.lone_surrogates => decode_lone_surrogates(cooked.as_str()),
        Some(cooked) => crate::units::from_str(cooked.as_str()),
        None => crate::units::from_str(q.value.raw.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_documented_encoding() {
        // "\uD800" — the case the whole representation change is for.
        assert_eq!(decode_lone_surrogates("\u{FFFD}d800"), vec![0xD800]);
        assert_eq!(
            decode_lone_surrogates("a\u{FFFD}d800b"),
            vec![0x61, 0xD800, 0x62]
        );
        // A genuine U+FFFD inside a literal that also has a lone surrogate:
        // the escaped escape, which decodes to itself.
        assert_eq!(
            decode_lone_surrogates("\u{FFFD}fffd\u{FFFD}dc00"),
            vec![0xFFFD, 0xDC00]
        );
        // Uppercase hex is accepted, though oxc writes lowercase.
        assert_eq!(decode_lone_surrogates("\u{FFFD}D800"), vec![0xD800]);
        // Text with no escape at all passes through, astral chars included.
        assert_eq!(decode_lone_surrogates("a😀"), vec![0x61, 0xD83D, 0xDE00]);
        // A truncated encoding cannot come from the parser; if oxc's contract
        // ever changes, this loses nothing rather than eating the tail.
        assert_eq!(decode_lone_surrogates("\u{FFFD}d8"), vec![0xFFFD]);
    }
}
