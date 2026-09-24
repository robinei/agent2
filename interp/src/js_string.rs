//! `JsString` — a thin, reference-counted, immutable UTF-16 string.
//!
//! One machine word: a `NonNull` pointing at a single heap block laid out as
//! `[ Header | units… ]`. Cloning bumps a non-atomic refcount; the units are
//! shared and never mutated (JS strings are immutable). It is the `Rc`-flavored
//! analogue of `arcstr::ArcStr`: the same thin, single-allocation,
//! single-indirection representation, but a `Cell` refcount instead of an
//! atomic — so it is `!Send`/`!Sync` (correct for the single-threaded VM) and
//! pays no atomic traffic on clone/drop.
//!
//! # The name
//!
//! It was `RcStr`, in `rc_str.rs`, until the payload changed. It no longer
//! holds a `str`, and this repo has just spent a phase paying for a name that
//! did not mean what it said.
//!
//! # Why hand-rolled
//!
//! `Rc<[u16]>` is a *fat* pointer (16 bytes), which would widen `Value` from
//! 16 to 24. `Rc<Vec<u16>>` is thin (8 bytes) but double-indirect and
//! double-allocating (`Rc → RcBox → Vec → buf`). `JsString` is thin (8 bytes)
//! *and* single-allocation / single-indirection: refcount, length, and units
//! live in one block reached through one pointer.
//!
//! # Why UTF-16 code units
//!
//! **Because a `usize` that means "UTF-8 byte" is checked by nothing.** The
//! payload was UTF-8 until 2026-09-24. Nothing forced the operations to
//! disagree about what a position was, and they all did anyway: `length` said
//! bytes, `padStart` said bytes for the target and characters for the pad,
//! `slice` said bytes and silently rounded (`"aéb".slice(0, 2)` was `"a"`),
//! `split("")` said characters, regex match indices said bytes, and
//! `fromCharCode` said code units and then dropped the ones it could not
//! encode (`String.fromCharCode(0xD83D, 0xDE00)` was `""`). Seven silent wrong
//! answers, none of them loud, none caught by a test. A code unit is the unit
//! every reference a maintainer or a model will consult already uses, which is
//! the only thing that keeps the drift from starting again. See
//! `docs/30_STRINGS.md`.
//!
//! The cost is stated plainly: ASCII text occupies twice the memory, and the
//! JSON boundary transcodes in both directions. The `is_ascii` header bit
//! keeps that crossing near the memcpy floor for the text this harness
//! actually moves.
//!
//! # Invariants
//!
//! - `self.0` always points at a live, properly-aligned [`Header`] immediately
//!   followed by exactly `header.len & !ASCII_BIT` `u16` code units.
//! - `ASCII_BIT` is set iff every one of those units is `< 0x80`.
//! - The units are never mutated after construction and no `&mut` to them is
//!   ever handed out, so sharing them across clones is sound.
//! - They are *not* required to be well-formed UTF-16: an unpaired surrogate
//!   is a value a JS program can legitimately produce and hold, which is the
//!   one capability this representation has that a UTF-8 one cannot.
//!
//! # Hash / Eq contract
//!
//! `Hash`, `Eq`, and `Ord` all delegate to `as_units()`, matching the
//! `Borrow<[u16]>` impl. This is load-bearing for
//! `IndexMap<JsString, _>::get(wide!("k"))`: the key must hash identically to
//! `&[u16]`. `Eq` additionally short-circuits on pointer identity, so
//! comparing two clones of the same allocation (or two interned strings) is
//! O(1).
//!
//! `Ord` moving from code-point order to code-unit order **is a fix**, not a
//! side effect: JS defines `<` on strings as code-unit comparison, so
//! `"Ｚ" < "\u{10000}"` now gives the answer the spec asks for, and
//! `localeCompare` follows it.

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::borrow::Borrow;
use std::cell::Cell;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ptr::{self, NonNull};
use std::slice;

/// Allocation prefix: a non-atomic strong count and the code-unit length with
/// an all-ASCII flag in its top bit. `repr(C)` so the trailing units sit at a
/// fixed, padding-free offset (`HEADER_SIZE`): both fields are `usize`, so the
/// struct size is a multiple of its align (8), and `[u16]` needs align 2,
/// which 8 satisfies and 16 is a multiple of.
#[repr(C)]
struct Header {
    count: Cell<usize>,
    len: usize,
}

/// Top bit of `Header::len`: every unit in this string is `< 0x80`.
///
/// **A flag, not a third field.** A third `usize` would take the header from
/// 16 to 24 bytes and make every short string worse; a string of 2^63 code
/// units is not a thing. Construction already walks every unit, so setting it
/// is free, and it earns its keep at the JSON boundary — measured on 100 KB of
/// ASCII, the all-ASCII scan alone is 22.6 µs of the 52 µs cost of narrowing
/// back to UTF-8.
const ASCII_BIT: usize = 1 << (usize::BITS - 1);

const HEADER_SIZE: usize = std::mem::size_of::<Header>();

/// A thin, reference-counted, immutable UTF-16 string. See the module docs.
pub struct JsString(NonNull<Header>);

impl JsString {
    /// The empty string.
    pub fn new() -> Self {
        Self::from_units_with_ascii(&[], true)
    }

    /// Allocate a block for `len` code units with a starting refcount of 1.
    /// Returns the header pointer and a pointer to the (uninitialized) units,
    /// which the caller must fill completely before the value escapes.
    fn alloc_block(len: usize, ascii: bool) -> (NonNull<Header>, *mut u16) {
        let layout = Self::layout_for(len);
        // SAFETY: `layout` has non-zero size (`Header` alone is two words), so
        // `alloc` is a valid request; we convert a null return into the
        // standard allocation-failure path rather than dereferencing it.
        let raw = unsafe { alloc(layout) };
        if raw.is_null() {
            handle_alloc_error(layout);
        }
        let header = raw as *mut Header;
        // SAFETY: `raw` is freshly allocated for one `Header` plus `len` units
        // and is uninitialized. We write the header once; the units that
        // immediately follow are the caller's to fill.
        unsafe {
            ptr::write(
                header,
                Header {
                    count: Cell::new(1),
                    len: len | if ascii { ASCII_BIT } else { 0 },
                },
            );
            (
                NonNull::new_unchecked(header),
                raw.add(HEADER_SIZE) as *mut u16,
            )
        }
    }

    /// Copy `units` into a fresh block, scanning for the all-ASCII flag.
    pub fn from_units(units: &[u16]) -> Self {
        let ascii = units.iter().all(|&u| u < 0x80);
        Self::from_units_with_ascii(units, ascii)
    }

    /// `from_units` for a caller that already knows the answer to the scan.
    fn from_units_with_ascii(units: &[u16], ascii: bool) -> Self {
        debug_assert_eq!(ascii, units.iter().all(|&u| u < 0x80));
        let (header, data) = Self::alloc_block(units.len(), ascii);
        // SAFETY: `data` points at exactly `units.len()` uninitialized `u16`s
        // inside the block we just allocated, non-overlapping with `units`.
        unsafe { ptr::copy_nonoverlapping(units.as_ptr(), data, units.len()) };
        Self(header)
    }

    /// A one-code-point string. An astral code point becomes a surrogate pair.
    pub fn from_char(c: char) -> Self {
        let mut buf = [0u16; 2];
        Self::from_units(c.encode_utf16(&mut buf))
    }

    /// Layout of a block holding the header plus `len` trailing code units.
    ///
    /// The arithmetic is the standard library's rather than a comment's:
    /// `extend` answers the alignment and trailing-padding questions, and
    /// checks the multiplication that a hand-rolled `HEADER_SIZE + len * 2`
    /// would have to promise never overflows.
    fn layout_for(len: usize) -> Layout {
        let units = Layout::array::<u16>(len).expect("JsString: allocation size overflow");
        let (layout, offset) = Layout::new::<Header>()
            .extend(units)
            .expect("JsString: allocation size overflow");
        debug_assert_eq!(offset, HEADER_SIZE, "units must follow the header directly");
        layout.pad_to_align()
    }

    /// Borrow the header. The returned reference is tied to `&self`.
    fn header(&self) -> &Header {
        // SAFETY: `self.0` points at a live, initialized `Header` for as long
        // as `self` is alive (class invariant); the borrow is bounded by `&self`.
        unsafe { self.0.as_ref() }
    }

    /// Code-unit count, with the ASCII flag masked off.
    fn unit_len(&self) -> usize {
        self.header().len & !ASCII_BIT
    }

    /// View as UTF-16 code units — zero-copy, no validation (there is nothing
    /// to validate: an unpaired surrogate is a legal value here).
    ///
    /// **This is the primitive, and call sites should read as it.**
    /// `s.as_units()[i]` says "this index is a code unit" at the point of use,
    /// which is the disease being treated.
    pub fn as_units(&self) -> &[u16] {
        let len = self.unit_len();
        // SAFETY: `len` code units immediately follow the header within the
        // same allocation, at offset `HEADER_SIZE`, which is a multiple of
        // `align_of::<u16>()`; the slice borrow is bounded by `&self`.
        unsafe {
            let data = (self.0.as_ptr() as *const u8).add(HEADER_SIZE) as *const u16;
            slice::from_raw_parts(data, len)
        }
    }

    /// Whether every code unit is `< 0x80` — read from the header, not scanned.
    pub fn is_ascii(&self) -> bool {
        self.header().len & ASCII_BIT != 0
    }

    /// Narrow to UTF-8, replacing each unpaired surrogate with U+FFFD.
    ///
    /// **Lossy by construction, and there is no fallible version worth
    /// having.** An unpaired surrogate is a value a program can legitimately
    /// produce (`"\uD800"`, a slice through an astral character) and has no
    /// UTF-8 form. Erroring here would make the JSON boundary fallible on data
    /// the program was entitled to make, turning a display problem into a
    /// crashed run. The trade is a loud failure for a quiet one, which is
    /// against the grain of `25_JS_DIALECT.md`; the mitigation is
    /// `isWellFormed`/`toWellFormed`, which let a program ask.
    pub fn to_utf8_lossy(&self) -> String {
        units_to_utf8_lossy(self.as_units(), self.is_ascii())
    }

    /// Whether this string's content equals a UTF-8 literal.
    ///
    /// **The one spelling of `s == "literal"` that survives the payload
    /// change.** `PartialEq<str>` could only exist while the payload *was*
    /// UTF-8. Comparing without allocating is still easy — it just is not an
    /// operator any more.
    pub fn eq_str(&self, other: &str) -> bool {
        let units = self.as_units();
        if self.is_ascii() {
            // A non-ASCII byte of `other` is ≥ 0x80 and every unit here is
            // < 0x80, so the byte-against-unit compare can only ever say
            // "different" — which is the right answer.
            return units.len() == other.len()
                && units
                    .iter()
                    .zip(other.as_bytes())
                    .all(|(&u, &b)| u == b as u16);
        }
        let mut it = units.iter().copied();
        for want in other.encode_utf16() {
            if it.next() != Some(want) {
                return false;
            }
        }
        it.next().is_none()
    }

    /// Current strong-reference count. Test/diagnostic use only.
    #[cfg(test)]
    fn strong_count(&self) -> usize {
        self.header().count.get()
    }
}

/// Narrow code units to UTF-8, replacing each unpaired surrogate with U+FFFD.
///
/// **One function, so the loss is countable.** Every path out of the VM into a
/// UTF-8 sink — `Display`, the JSON boundary, diagnostics — goes through here
/// or through [`JsString::to_utf8_lossy`], which calls it.
pub fn units_to_utf8_lossy(units: &[u16], ascii: bool) -> String {
    if ascii {
        debug_assert!(units.iter().all(|&u| u < 0x80));
        // SAFETY: every unit is < 0x80, so the narrowed bytes are ASCII and
        // therefore valid UTF-8.
        return unsafe { String::from_utf8_unchecked(units.iter().map(|&u| u as u8).collect()) };
    }
    char::decode_utf16(units.iter().copied())
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

// ── lifecycle ────────────────────────────────────────────────────────────────

impl Clone for JsString {
    fn clone(&self) -> Self {
        let count = self.header().count.get();
        // Match `Rc`: refuse to wrap the count (a saturated count would let a
        // later run of drops free the block while clones still alias it). The
        // branch is perfectly predicted and practically never taken.
        if count > isize::MAX as usize {
            std::process::abort();
        }
        self.header().count.set(count + 1);
        Self(self.0)
    }
}

impl Drop for JsString {
    fn drop(&mut self) {
        let count = self.header().count.get();
        if count > 1 {
            self.header().count.set(count - 1);
            return;
        }
        // Last owner. `Header` is trivially droppable (`Cell<usize>` + `usize`),
        // so there is nothing to run before freeing — just release the block.
        let layout = Self::layout_for(self.unit_len());
        // SAFETY: `self.0` came from `alloc` with exactly this layout (the
        // ASCII flag is masked out of the length, as it is everywhere), the
        // refcount reached zero so no other `JsString` aliases the block, and we
        // never touch `self.0` again after this.
        unsafe { dealloc(self.0.as_ptr() as *mut u8, layout) }
    }
}

// ── construction conveniences ────────────────────────────────────────────────

impl Default for JsString {
    fn default() -> Self {
        Self::new()
    }
}

impl From<&str> for JsString {
    fn from(s: &str) -> Self {
        if s.is_ascii() {
            // Widening ASCII is `b as u16` per byte with no intermediate
            // buffer: measured at 2.7 µs per 100 KB against 86 µs for
            // `encode_utf16().collect()`, which is why the branch is here and
            // not left to the general path.
            let (header, data) = Self::alloc_block(s.len(), true);
            for (i, &b) in s.as_bytes().iter().enumerate() {
                // SAFETY: `i` is in `0..s.len()`, the exact count of units the
                // block was allocated for.
                unsafe { data.add(i).write(b as u16) };
            }
            return Self(header);
        }
        let units: Vec<u16> = s.encode_utf16().collect();
        Self::from_units_with_ascii(&units, false)
    }
}

impl From<String> for JsString {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl From<&String> for JsString {
    fn from(s: &String) -> Self {
        Self::from(s.as_str())
    }
}

impl From<&[u16]> for JsString {
    fn from(units: &[u16]) -> Self {
        Self::from_units(units)
    }
}

impl From<Vec<u16>> for JsString {
    fn from(units: Vec<u16>) -> Self {
        Self::from_units(&units)
    }
}

impl<'a> FromIterator<&'a str> for JsString {
    fn from_iter<I: IntoIterator<Item = &'a str>>(iter: I) -> Self {
        // Immutable target: assemble in a growable buffer, then freeze once.
        let buf: Vec<u16> = iter.into_iter().flat_map(str::encode_utf16).collect();
        Self::from_units(&buf)
    }
}

impl FromIterator<u16> for JsString {
    fn from_iter<I: IntoIterator<Item = u16>>(iter: I) -> Self {
        let buf: Vec<u16> = iter.into_iter().collect();
        Self::from_units(&buf)
    }
}

// ── comparison / hashing (delegate to `[u16]`) ───────────────────────────────

impl Borrow<[u16]> for JsString {
    fn borrow(&self) -> &[u16] {
        self.as_units()
    }
}

impl AsRef<[u16]> for JsString {
    fn as_ref(&self) -> &[u16] {
        self.as_units()
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        // Same allocation ⇒ equal, no unit compare. Makes equality of clones
        // and interned strings O(1); the fallback handles distinct allocations.
        ptr::eq(self.0.as_ptr(), other.0.as_ptr()) || self.as_units() == other.as_units()
    }
}

impl Eq for JsString {}

impl PartialOrd for JsString {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsString {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_units().cmp(other.as_units())
    }
}

impl Hash for JsString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_units().hash(state);
    }
}

// ── formatting ───────────────────────────────────────────────────────────────

impl fmt::Display for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_utf8_lossy())
    }
}

impl fmt::Debug for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_utf8_lossy().fmt(f)
    }
}

/// A name that can be compared against a `&'static str` literal.
///
/// **The builtin table is a list of literals, and two callers reach it with
/// two different types.** The compiler looks a method up with a `&str` from
/// the AST; the VM looks the same method up with the `JsString` operand of an
/// `ObjGet`. Neither should have to allocate to ask, and before the payload
/// change neither did, because `JsString` could hand out a `&str`. This is what
/// replaces that.
pub trait NameEq {
    fn name_eq(&self, lit: &str) -> bool;
}

impl NameEq for str {
    fn name_eq(&self, lit: &str) -> bool {
        self == lit
    }
}

impl NameEq for JsString {
    fn name_eq(&self, lit: &str) -> bool {
        self.eq_str(lit)
    }
}

// ── compile-time widened literals ────────────────────────────────────────────

/// Widen an ASCII literal to a `[u16; N]` at compile time.
///
/// **The `wide!` macro below is the only intended caller.** `N` comes from
/// `$s.len()`, which equals the code-unit count exactly when the literal is
/// ASCII — so the length assert and the per-byte assert together make a
/// non-ASCII literal a *compile* error (this runs in const position) rather
/// than a lookup that silently misses at runtime.
pub const fn ascii_wide<const N: usize>(s: &str) -> [u16; N] {
    let b = s.as_bytes();
    assert!(b.len() == N, "wide!: literal is not ASCII");
    let mut out = [0u16; N];
    let mut i = 0;
    while i < N {
        assert!(b[i] < 0x80, "wide!: literal is not ASCII");
        out[i] = b[i] as u16;
        i += 1;
    }
    out
}

/// A `&'static [u16]` for an ASCII string literal, built at compile time.
///
/// The map keys in this crate are `JsString`; a lookup by literal has to present
/// the same shape the key hashes as. `wide!("length")` is that shape, with no
/// allocation and no runtime transcode.
#[macro_export]
macro_rules! wide {
    ($s:literal) => {{
        const W: [u16; $s.len()] = $crate::js_string::ascii_wide($s);
        &W as &'static [u16]
    }};
}

/// A chain of literal comparisons, written as a `match`.
///
/// **A property name stopped being a `str` and so stopped being matchable.** A
/// `match` on `&str` already lowers to a length-dispatched chain of `memcmp`s,
/// so this expands to the same class of machine code; what it buys is that the
/// source still reads as a table of names, and that the comparison is
/// `eq_str`, which is defined for whatever the payload happens to be.
#[macro_export]
macro_rules! match_wide {
    ($field:expr => { $($lit:literal => $arm:expr,)* _ => $default:expr $(,)? }) => {{
        let __f = $field;
        $(if $crate::js_string::JsString::eq_str(__f, $lit) { $arm } else)* { $default }
    }};
}

/// The property names this crate looks up by literal.
///
/// **Named, not spelled inline, because the type of a map key changed.** Every
/// one of these is an `IndexMap<JsString, _>::get` against a borrowed literal;
/// routing them through one module meant the shape that a literal key takes
/// was decided in one place rather than at eleven call sites.
pub mod keys {
    pub const LENGTH: &[u16] = wide!("length");
    pub const SIZE: &[u16] = wide!("size");
    pub const NAME: &[u16] = wide!("name");
    pub const MESSAGE: &[u16] = wide!("message");
    pub const OLD: &[u16] = wide!("old");
    pub const NEW: &[u16] = wide!("new");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_counter;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn thin_pointer_size() {
        assert_eq!(
            std::mem::size_of::<JsString>(),
            std::mem::size_of::<usize>()
        );
        // Niche: the `NonNull` makes `Option<JsString>` free.
        assert_eq!(
            std::mem::size_of::<Option<JsString>>(),
            std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn roundtrip_and_unicode() {
        assert_eq!(JsString::from("").as_units(), &[] as &[u16]);
        assert_eq!(JsString::from("hello").as_units(), &units("hello")[..]);
        let s = JsString::from("héllo→世界");
        assert_eq!(s.as_units(), &units("héllo→世界")[..]);
        assert_eq!(s.to_utf8_lossy(), "héllo→世界");
        assert!(!s.is_ascii());
        assert!(JsString::from("hello").is_ascii());
    }

    /// The capability the whole representation exists for: a string no UTF-8
    /// form can hold, held, compared and counted correctly.
    #[test]
    fn lone_surrogate_survives() {
        let s = JsString::from_units(&[0xD800]);
        assert_eq!(s.as_units().len(), 1);
        assert_eq!(s.as_units()[0], 0xD800);
        assert!(!s.is_ascii());
        // Only on the way *out* is it lossy.
        assert_eq!(s.to_utf8_lossy(), "\u{FFFD}");
        assert_eq!(JsString::from_units(&[0xD800]), s);
        assert_ne!(JsString::from_units(&[0xD801]), s);
    }

    #[test]
    fn astral_is_two_units() {
        let s = JsString::from("😀");
        assert_eq!(s.as_units(), &[0xD83D, 0xDE00]);
        assert_eq!(JsString::from_char('😀').as_units(), s.as_units());
        assert_eq!(s.to_utf8_lossy(), "😀");
    }

    #[test]
    fn ascii_bit_tracks_the_content() {
        assert!(JsString::new().is_ascii());
        assert!(JsString::from("plain").is_ascii());
        assert!(!JsString::from("é").is_ascii());
        assert!(!JsString::from_units(&[0x7F, 0x80]).is_ascii());
        assert!(JsString::from_units(&[0x7F, 0x00]).is_ascii());
    }

    #[test]
    fn eq_str_on_both_paths() {
        assert!(JsString::from("length").eq_str("length"));
        assert!(!JsString::from("length").eq_str("lengthy"));
        assert!(!JsString::from("lengthy").eq_str("length"));
        // ASCII receiver against a non-ASCII literal: never equal, never a panic.
        assert!(!JsString::from("ab").eq_str("é"));
        // Non-ASCII receiver takes the general path.
        assert!(JsString::from("héllo").eq_str("héllo"));
        assert!(!JsString::from("héllo").eq_str("hello"));
        assert!(JsString::from("😀").eq_str("😀"));
        assert!(!JsString::from_units(&[0xD83D]).eq_str("😀"));
    }

    #[test]
    fn clone_shares_allocation_and_refcounts() {
        let a = JsString::from("shared");
        assert_eq!(a.strong_count(), 1);
        let b = a.clone();
        assert_eq!(a.strong_count(), 2);
        // Same backing block.
        assert!(ptr::eq(a.0.as_ptr(), b.0.as_ptr()));
        assert_eq!(a, b);
        drop(b);
        assert_eq!(a.strong_count(), 1);
    }

    #[test]
    fn clone_allocates_nothing_drop_frees() {
        let base = JsString::from("allocate me once");
        alloc_counter::reset();
        let clones: Vec<JsString> = (0..1000).map(|_| base.clone()).collect();
        // The Vec itself allocates; the JsString clones must not.
        assert_eq!(base.strong_count(), 1001);
        drop(clones);
        assert_eq!(base.strong_count(), 1);
        // `base` still valid after all clones dropped — no double free / UAF.
        assert_eq!(base.to_utf8_lossy(), "allocate me once");
    }

    #[test]
    fn one_allocation_per_string() {
        alloc_counter::reset();
        let s = JsString::from("single block");
        assert_eq!(alloc_counter::count(), 1);
        assert_eq!(s.to_utf8_lossy(), "single block");
    }

    #[test]
    fn eq_pointer_fast_path_and_value_path() {
        let a = JsString::from("abc");
        let b = a.clone(); // same allocation → pointer fast path
        let c = JsString::from("abc"); // distinct allocation → value compare
        let d = JsString::from("abd");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_ne!(a, d);
        assert!(a.eq_str("abc"));
        assert!(a < d);
    }

    /// JS defines `<` on strings as code-unit comparison, and the old
    /// code-point `str::cmp` disagreed with it for exactly this pair.
    #[test]
    fn ord_is_code_unit_order() {
        let ff3a = JsString::from("\u{FF3A}"); // one unit, 0xFF3A
        let astral = JsString::from("\u{10000}"); // two units, 0xD800 0xDC00
        assert!(astral < ff3a);
        assert_eq!(astral.cmp(&ff3a), Ordering::Less);
    }

    #[test]
    fn works_as_map_key_via_units_borrow() {
        use indexmap::IndexMap;
        let mut m: IndexMap<JsString, u32> = IndexMap::new();
        m.insert(JsString::from("one"), 1);
        m.insert(JsString::from("two"), 2);
        // Lookup by &[u16] relies on Borrow<[u16]> + matching Hash/Eq: a
        // `wide!` literal and an `JsString` must hash the same.
        assert_eq!(m.get(wide!("one")), Some(&1));
        assert_eq!(m.get(wide!("two")), Some(&2));
        assert_eq!(m.get(wide!("three")), None);
        assert_eq!(m.get(&units("one")[..]), Some(&1));
    }

    #[test]
    fn from_iter_concatenates() {
        let s: JsString = ["a", "bc", "def"].into_iter().collect();
        assert_eq!(s.to_utf8_lossy(), "abcdef");
    }

    #[test]
    fn wide_literal_matches_encode_utf16() {
        assert_eq!(wide!("length"), &[108u16, 101, 110, 103, 116, 104][..]);
        assert_eq!(wide!(""), &[] as &[u16]);
        assert_eq!(keys::LENGTH, &units("length")[..]);
        assert_eq!(keys::SIZE, &units("size")[..]);
        assert_eq!(keys::NAME, &units("name")[..]);
        assert_eq!(keys::MESSAGE, &units("message")[..]);
        assert_eq!(keys::OLD, &units("old")[..]);
        assert_eq!(keys::NEW, &units("new")[..]);
    }
}
