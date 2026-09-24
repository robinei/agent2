//! The operations a JS string needs that `&[u16]` does not already have.
//!
//! **A position in this crate is a UTF-16 code unit, everywhere.** `&[u16]`
//! brings `len`, `is_empty`, `starts_with`, `ends_with`, slicing and `cmp`
//! for free — that is most of what the string builtins do. What it does not
//! bring is substring search, the JS whitespace set, case mapping, and
//! code-point decoding that tolerates an unpaired surrogate. Those live here,
//! once, rather than being re-decided per builtin: the bug this whole change
//! is treating was thirty operations each deciding locally what a position
//! meant.

/// Index of the first occurrence of `needle` in `hay` at or after `from`.
///
/// Naive, and deliberately so: the strings this runs on are a program's own
/// text, the alphabet is not adversarial, and `memchr` does not exist for
/// `u16`. A two-way search can be dropped in behind this signature the day a
/// measurement asks for one.
pub fn find(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return if from <= hay.len() { Some(from) } else { None };
    }
    if from >= hay.len() || needle.len() > hay.len() - from {
        return None;
    }
    let first = needle[0];
    for i in from..=(hay.len() - needle.len()) {
        if hay[i] == first && &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

/// Index of the last occurrence of `needle` starting at or before `limit`.
pub fn rfind(hay: &[u16], needle: &[u16], limit: usize) -> Option<usize> {
    let limit = limit.min(hay.len().saturating_sub(needle.len()));
    if needle.len() > hay.len() {
        return None;
    }
    (0..=limit)
        .rev()
        .find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Whether `u` is in JS's `StringWhiteSpace` — `WhiteSpace ∪ LineTerminator`.
///
/// **Not `char::is_whitespace`.** The two sets disagree in both directions and
/// `trim` is defined against this one: JS includes U+FEFF (ZWNBSP), which Rust
/// does not treat as whitespace, and Rust's set is code-point-wide while this
/// is a table the spec fixes. `test262`'s `trim/15.5.4.20-*` is eight files
/// that check exactly the boundary cases.
pub fn is_js_whitespace(u: u16) -> bool {
    matches!(
        u,
        0x0009 | 0x000A | 0x000B | 0x000C | 0x000D | 0x0020 | 0x00A0 | 0x1680 | 0x2000
            ..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

/// The half-open range left after trimming JS whitespace from both ends.
pub fn trim_range(units: &[u16]) -> (usize, usize) {
    let start = trim_start_index(units);
    if start == units.len() {
        return (start, start);
    }
    (start, trim_end_index(units))
}

/// The first index that is not JS whitespace (or `len`).
pub fn trim_start_index(units: &[u16]) -> usize {
    units
        .iter()
        .position(|&u| !is_js_whitespace(u))
        .unwrap_or(units.len())
}

/// One past the last index that is not JS whitespace (or `0`).
pub fn trim_end_index(units: &[u16]) -> usize {
    units
        .iter()
        .rposition(|&u| !is_js_whitespace(u))
        .map_or(0, |i| i + 1)
}

/// The code point at `i` and how many units it occupies.
///
/// An unpaired surrogate is returned as itself with a length of 1 — which is
/// what `codePointAt` is specified to do, and the reason this cannot be
/// `char::decode_utf16` with the errors thrown away.
pub fn code_point_at(units: &[u16], i: usize) -> Option<(u32, usize)> {
    let first = *units.get(i)? as u32;
    if (0xD800..0xDC00).contains(&first)
        && let Some(&low) = units.get(i + 1)
        && (0xDC00..0xE000).contains(&(low as u32))
    {
        return Some((
            0x10000 + ((first - 0xD800) << 10) + (low as u32 - 0xDC00),
            2,
        ));
    }
    Some((first, 1))
}

/// Walk `units` one code point at a time, yielding each as a unit slice.
///
/// This is what `for (const ch of s)`, `split("")` and the string spread all
/// mean by "a character": a code point, so a surrogate pair stays together and
/// a lone surrogate comes out whole rather than being replaced.
pub fn code_points(units: &[u16]) -> impl Iterator<Item = &[u16]> {
    let mut i = 0;
    std::iter::from_fn(move || {
        let (_, n) = code_point_at(units, i)?;
        let out = &units[i..i + n];
        i += n;
        Some(out)
    })
}

/// Map each code point through `f` and re-encode.
///
/// Case mapping is defined on code points, and both directions can change
/// length (`ß` → `SS`), so this cannot be a per-unit map. An unpaired
/// surrogate has no case and is copied through untouched.
fn map_code_points<I, F>(units: &[u16], f: F) -> Vec<u16>
where
    F: Fn(char) -> I,
    I: Iterator<Item = char>,
{
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while let Some((cp, n)) = code_point_at(units, i) {
        match char::from_u32(cp) {
            Some(c) => {
                for mapped in f(c) {
                    let mut buf = [0u16; 2];
                    out.extend_from_slice(mapped.encode_utf16(&mut buf));
                }
            }
            // A lone surrogate: `char::from_u32` refuses it and it has no
            // case, so it passes through as the unit it is.
            None => out.push(units[i]),
        }
        i += n;
    }
    out
}

/// `toLowerCase` over code units.
///
/// **Delegated to `str::to_lowercase` whenever the string is well-formed**,
/// because case mapping is not a per-code-point function. A final Greek Σ
/// lowercases to ς rather than σ, and deciding which needs the `Cased` and
/// `Case_Ignorable` properties of the *surrounding* characters — a table this
/// crate does not carry and `char::to_lowercase` does not consult.
/// test262's `toLowerCase/Final_Sigma_U180E` and `special_casing_conditional`
/// check exactly that, and both regressed when this was first written as a
/// per-code-point map.
///
/// The fallback is that map, taken only when an unpaired surrogate makes the
/// UTF-8 round trip lossy. A surrogate has no case, and a string containing a
/// hole has no Greek word boundary around it that the context rule could see
/// differently.
pub fn to_lowercase(units: &[u16]) -> Vec<u16> {
    if is_well_formed(units) {
        return from_str(&crate::js_string::units_to_utf8_lossy(units, false).to_lowercase());
    }
    map_code_points(units, char::to_lowercase)
}

/// `toUpperCase` over code units. See [`to_lowercase`] for why this is not a
/// per-code-point map — `ß` uppercases to `SS`, and the special-casing table
/// belongs to `str`.
pub fn to_uppercase(units: &[u16]) -> Vec<u16> {
    if is_well_formed(units) {
        return from_str(&crate::js_string::units_to_utf8_lossy(units, false).to_uppercase());
    }
    map_code_points(units, char::to_uppercase)
}

/// Whether `units` is well-formed UTF-16 — every surrogate paired.
pub fn is_well_formed(units: &[u16]) -> bool {
    let mut i = 0;
    while i < units.len() {
        let u = units[i] as u32;
        if (0xD800..0xE000).contains(&u) {
            let (_, n) = code_point_at(units, i).expect("i is in range");
            if n != 2 {
                return false;
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    true
}

/// `units` with each unpaired surrogate replaced by U+FFFD.
pub fn to_well_formed(units: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i] as u32;
        if (0xD800..0xE000).contains(&u) {
            let (_, n) = code_point_at(units, i).expect("i is in range");
            if n == 2 {
                out.extend_from_slice(&units[i..i + 2]);
                i += 2;
            } else {
                out.push(0xFFFD);
                i += 1;
            }
            continue;
        }
        out.push(units[i]);
        i += 1;
    }
    out
}

/// Widen a UTF-8 slice to owned code units.
pub fn from_str(s: &str) -> Vec<u16> {
    if s.is_ascii() {
        return s.as_bytes().iter().map(|&b| b as u16).collect();
    }
    s.encode_utf16().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        from_str(s)
    }

    #[test]
    fn find_and_rfind_count_units() {
        let hay = u("a😀b");
        assert_eq!(hay.len(), 4);
        // "b" is at unit 3, not byte 5 — the answer JS gives.
        assert_eq!(find(&hay, &u("b"), 0), Some(3));
        assert_eq!(find(&hay, &u("😀"), 0), Some(1));
        assert_eq!(find(&hay, &u("z"), 0), None);
        assert_eq!(find(&u("aaa"), &u("a"), 1), Some(1));
        assert_eq!(rfind(&u("aaa"), &u("a"), 3), Some(2));
        assert_eq!(rfind(&u("aaa"), &u("a"), 1), Some(1));
        assert_eq!(rfind(&u("abc"), &u("z"), 3), None);
        // An empty needle matches at the start position, as `str::find` does.
        assert_eq!(find(&u("abc"), &[], 2), Some(2));
        assert_eq!(find(&u("abc"), &[], 9), None);
    }

    #[test]
    fn code_points_keep_pairs_and_lone_surrogates() {
        let src = u("a😀b");
        let pair: Vec<&[u16]> = code_points(&src).collect();
        assert_eq!(pair.len(), 3);
        assert_eq!(pair[1].len(), 2);
        // A lone high surrogate is one code point, not an error and not U+FFFD.
        let lone = [0x61u16, 0xD800, 0x62];
        let got: Vec<&[u16]> = code_points(&lone).collect();
        assert_eq!(got.len(), 3);
        assert_eq!(got[1], &[0xD800]);
        assert_eq!(code_point_at(&lone, 1), Some((0xD800, 1)));
        assert_eq!(code_point_at(&u("😀"), 0), Some((0x1F600, 2)));
    }

    /// JS's whitespace set includes U+FEFF and Rust's `is_whitespace` does
    /// not; that disagreement is eight test262 failures on its own.
    #[test]
    fn js_whitespace_includes_zwnbsp() {
        assert!(is_js_whitespace(0xFEFF));
        assert!(!char::from_u32(0xFEFF).unwrap().is_whitespace());
        assert!(is_js_whitespace(0x00A0));
        assert!(is_js_whitespace(0x2028));
        assert!(!is_js_whitespace(0x200B)); // ZWSP is not whitespace in JS
        let s = u("\u{FEFF} hi \u{2029}");
        let (a, b) = trim_range(&s);
        assert_eq!(&s[a..b], &u("hi")[..]);
    }

    #[test]
    fn case_mapping_can_change_length() {
        assert_eq!(to_uppercase(&u("straße")), u("STRASSE"));
        assert_eq!(to_lowercase(&u("ÄÖÜ")), u("äöü"));
        // An unpaired surrogate has no case and survives the round trip.
        assert_eq!(to_lowercase(&[0xD800]), vec![0xD800]);
    }

    #[test]
    fn well_formed_checks_and_repairs() {
        assert!(is_well_formed(&u("a😀b")));
        assert!(!is_well_formed(&[0xD800]));
        assert!(!is_well_formed(&[0xDC00, 0x61]));
        assert_eq!(
            to_well_formed(&[0x61, 0xD800, 0x62]),
            vec![0x61, 0xFFFD, 0x62]
        );
        assert_eq!(to_well_formed(&u("😀")), u("😀"));
    }
}
