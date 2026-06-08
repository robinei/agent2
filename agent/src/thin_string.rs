//! `ThinString` — a UTF-8-guaranteeing string type backed by `ThinVec<u8>`.
//!
//! Serves as both string content (`HeapValue::String`) and object keys
//! (`FieldName`). Replaces the raw `ThinStr` (= `ThinVec<u8>`) alias everywhere.
//!
//! # Correctness invariant
//!
//! The inner bytes are always valid UTF-8. Construction is the validation
//! boundary: `from_utf8` checks once; `From<&str>` is trivially valid;
//! `from_utf8_unchecked` is for bytes provably originating from valid UTF-8
//! (e.g. re-wrapping the output of a `&str` operation). No code path validates
//! on *read* — `as_str()` is a zero-cost `unsafe` transmute.
//!
//! # Hash / Eq contract
//!
//! `Hash` and `Eq` delegate to `self.as_str()`, matching the `Borrow<str>`
//! impl. This is load-bearing: `IndexMap<ThinString, _>::get("k")` works only
//! when `ThinString` hashes identically to `&str`. Deriving on the inner
//! `ThinVec<u8>` would break key lookup (different hash algorithm for `[u8]`).

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

use thin_vec::ThinVec;

/// A UTF-8 string stored in a `ThinVec<u8>`. Guarantees valid UTF-8 at rest.
#[derive(Clone, Debug, Default)]
pub struct ThinString(ThinVec<u8>);

impl ThinString {
    /// An empty string.
    pub fn new() -> Self {
        Self(ThinVec::new())
    }

    /// Create with a pre-allocated capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self(ThinVec::with_capacity(cap))
    }

    /// Validate UTF-8 once at the construction boundary. This is the only
    /// validation site — reads go through `as_str()` which is zero-cost.
    pub fn from_utf8(v: ThinVec<u8>) -> Result<Self, std::str::Utf8Error> {
        std::str::from_utf8(&v)?;
        Ok(Self(v))
    }

    /// Build a `ThinString` from bytes known to be valid UTF-8. The caller
    /// guarantees correctness — misuse produces UB on any `as_str()` call that
    /// crosses a non-UTF-8 boundary. Restricted to provably-valid sources:
    /// bytes that originated from a `&str` or an existing `ThinString`.
    ///
    /// # Safety
    ///
    /// `v` must be valid UTF-8.
    pub unsafe fn from_utf8_unchecked(v: ThinVec<u8>) -> Self {
        Self(v)
    }

    /// View as `&str` — zero-cost, no revalidation. The inner bytes are always
    /// valid UTF-8 (enforced at construction).
    pub fn as_str(&self) -> &str {
        // Safety: the invariant guarantees valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }

    /// View as raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consume and return the inner `ThinVec<u8>`.
    pub fn into_bytes(self) -> ThinVec<u8> {
        self.0
    }

    /// Append a `&str` fragment, growing the buffer.
    pub fn push_str(&mut self, s: &str) {
        self.0.extend_from_slice(s.as_bytes());
    }
}

// ── trait impls ──────────────────────────────────────────────────────────────

impl Deref for ThinString {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for ThinString {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for ThinString {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for ThinString {
    fn from(s: &str) -> Self {
        Self(ThinVec::from(s.as_bytes()))
    }
}

impl From<String> for ThinString {
    fn from(s: String) -> Self {
        Self(ThinVec::from(s.into_bytes().as_slice()))
    }
}

impl<'a> From<&'a String> for ThinString {
    fn from(s: &'a String) -> Self {
        Self(ThinVec::from(s.as_bytes()))
    }
}

impl<'a> FromIterator<&'a str> for ThinString {
    fn from_iter<I: IntoIterator<Item = &'a str>>(iter: I) -> Self {
        let mut s = Self::new();
        for frag in iter {
            s.push_str(frag);
        }
        s
    }
}

impl PartialEq for ThinString {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for ThinString {}

impl PartialOrd for ThinString {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ThinString {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for ThinString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl fmt::Display for ThinString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}
