//! `RcStr` — a thin, reference-counted, immutable UTF-8 string.
//!
//! One machine word: a `NonNull` pointing at a single heap block laid out as
//! `[ Header | bytes… ]`. Cloning bumps a non-atomic refcount; the bytes are
//! shared and never mutated (JS strings are immutable). It is the `Rc`-flavored
//! analogue of `arcstr::ArcStr`: the same thin, single-allocation,
//! single-indirection representation, but a `Cell` refcount instead of an
//! atomic — so it is `!Send`/`!Sync` (correct for the single-threaded VM) and
//! pays no atomic traffic on clone/drop.
//!
//! # Why hand-rolled
//!
//! `Rc<str>` is a *fat* pointer (16 bytes), which would widen `Value` from
//! 16 to 24. `Rc<String>` is thin (8 bytes) but double-indirect and
//! double-allocating (`Rc → RcBox → String → buf`). `RcStr` is thin (8 bytes)
//! *and* single-allocation / single-indirection: refcount, length, and bytes
//! live in one block reached through one pointer.
//!
//! # Invariants
//!
//! - `self.0` always points at a live, properly-aligned [`Header`] immediately
//!   followed by exactly `header.len` bytes of valid UTF-8.
//! - Those bytes are never mutated after construction and no `&mut` to them is
//!   ever handed out, so sharing them across clones is sound.
//!
//! # Hash / Eq contract
//!
//! `Hash`, `Eq`, and `Ord` all delegate to `as_str()`, matching the
//! `Borrow<str>` impl. This is load-bearing for `IndexMap<RcStr, _>::get("k")`:
//! the key must hash identically to `&str`. `Eq` additionally short-circuits on
//! pointer identity, so comparing two clones of the same allocation (or two
//! interned strings) is O(1).

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::borrow::Borrow;
use std::cell::Cell;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::ptr::{self, NonNull};
use std::slice;

/// Allocation prefix: a non-atomic strong count and the byte length. `repr(C)`
/// so the trailing bytes sit at a fixed, padding-free offset (`HEADER_SIZE`):
/// both fields are `usize`, so the struct size is a multiple of its align and a
/// `u8` array (align 1) follows immediately.
#[repr(C)]
struct Header {
    count: Cell<usize>,
    len: usize,
}

const HEADER_SIZE: usize = std::mem::size_of::<Header>();
const HEADER_ALIGN: usize = std::mem::align_of::<Header>();

/// A thin, reference-counted, immutable UTF-8 string. See the module docs.
pub struct RcStr(NonNull<Header>);

impl RcStr {
    /// The empty string.
    pub fn new() -> Self {
        Self::from_bytes(b"")
    }

    /// Allocate a block for `bytes` (assumed valid UTF-8) and copy them in with
    /// a starting refcount of 1.
    fn from_bytes(bytes: &[u8]) -> Self {
        let len = bytes.len();
        let layout = Self::layout_for(len);
        // SAFETY: `layout` has non-zero size (`Header` alone is two words), so
        // `alloc` is a valid request; we convert a null return into the
        // standard allocation-failure path rather than dereferencing it.
        let raw = unsafe { alloc(layout) };
        if raw.is_null() {
            handle_alloc_error(layout);
        }
        let header = raw as *mut Header;
        // SAFETY: `raw` is freshly allocated for one `Header` plus `len` bytes
        // and is uninitialized. We write the header once, then copy exactly
        // `len` bytes into the region that immediately follows it — within the
        // allocation and non-overlapping with the source `&[u8]`.
        unsafe {
            ptr::write(
                header,
                Header {
                    count: Cell::new(1),
                    len,
                },
            );
            ptr::copy_nonoverlapping(bytes.as_ptr(), raw.add(HEADER_SIZE), len);
            Self(NonNull::new_unchecked(header))
        }
    }

    /// Layout of a block holding the header plus `len` trailing bytes.
    fn layout_for(len: usize) -> Layout {
        // `HEADER_SIZE + len` cannot overflow for any real string: `len` of a
        // live `&str`/`[u8]` is bounded by `isize::MAX`. `from_size_align`
        // rejects an over-large size, which `expect` turns into a clean panic.
        Layout::from_size_align(HEADER_SIZE + len, HEADER_ALIGN)
            .expect("RcStr: allocation size overflow")
    }

    /// Borrow the header. The returned reference is tied to `&self`.
    fn header(&self) -> &Header {
        // SAFETY: `self.0` points at a live, initialized `Header` for as long
        // as `self` is alive (class invariant); the borrow is bounded by `&self`.
        unsafe { self.0.as_ref() }
    }

    /// View as `&str` — zero-copy, no UTF-8 revalidation (the bytes were valid
    /// at construction and are immutable thereafter).
    pub fn as_str(&self) -> &str {
        let len = self.header().len;
        // SAFETY: `len` bytes of valid UTF-8 immediately follow the header
        // within the same allocation; the slice borrow is bounded by `&self`.
        unsafe {
            let data = (self.0.as_ptr() as *const u8).add(HEADER_SIZE);
            std::str::from_utf8_unchecked(slice::from_raw_parts(data, len))
        }
    }

    /// View as raw UTF-8 bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }

    /// Current strong-reference count. Test/diagnostic use only.
    #[cfg(test)]
    fn strong_count(&self) -> usize {
        self.header().count.get()
    }
}

// ── lifecycle ────────────────────────────────────────────────────────────────

impl Clone for RcStr {
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

impl Drop for RcStr {
    fn drop(&mut self) {
        let count = self.header().count.get();
        if count > 1 {
            self.header().count.set(count - 1);
            return;
        }
        // Last owner. `Header` is trivially droppable (`Cell<usize>` + `usize`),
        // so there is nothing to run before freeing — just release the block.
        let layout = Self::layout_for(self.header().len);
        // SAFETY: `self.0` came from `alloc` with exactly this layout, the
        // refcount reached zero so no other `RcStr` aliases the block, and we
        // never touch `self.0` again after this.
        unsafe { dealloc(self.0.as_ptr() as *mut u8, layout) }
    }
}

// ── construction conveniences ────────────────────────────────────────────────

impl Default for RcStr {
    fn default() -> Self {
        Self::new()
    }
}

impl From<&str> for RcStr {
    fn from(s: &str) -> Self {
        Self::from_bytes(s.as_bytes())
    }
}

impl From<String> for RcStr {
    fn from(s: String) -> Self {
        Self::from_bytes(s.as_bytes())
    }
}

impl From<&String> for RcStr {
    fn from(s: &String) -> Self {
        Self::from_bytes(s.as_bytes())
    }
}

impl<'a> FromIterator<&'a str> for RcStr {
    fn from_iter<I: IntoIterator<Item = &'a str>>(iter: I) -> Self {
        // Immutable target: assemble in a growable buffer, then freeze once.
        let buf: String = iter.into_iter().collect();
        Self::from(buf)
    }
}

// ── string-like access ───────────────────────────────────────────────────────

impl Deref for RcStr {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for RcStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for RcStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

// ── comparison / hashing (delegate to `str`) ─────────────────────────────────

impl PartialEq for RcStr {
    fn eq(&self, other: &Self) -> bool {
        // Same allocation ⇒ equal, no byte compare. Makes equality of clones
        // and interned strings O(1); the fallback handles distinct allocations.
        ptr::eq(self.0.as_ptr(), other.0.as_ptr()) || self.as_str() == other.as_str()
    }
}

impl Eq for RcStr {}

impl PartialOrd for RcStr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RcStr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for RcStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialEq<str> for RcStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for RcStr {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

// ── formatting ───────────────────────────────────────────────────────────────

impl fmt::Display for RcStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

impl fmt::Debug for RcStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_counter;

    #[test]
    fn thin_pointer_size() {
        assert_eq!(std::mem::size_of::<RcStr>(), std::mem::size_of::<usize>());
        // Niche: the `NonNull` makes `Option<RcStr>` free.
        assert_eq!(
            std::mem::size_of::<Option<RcStr>>(),
            std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn roundtrip_and_unicode() {
        assert_eq!(RcStr::from("").as_str(), "");
        assert_eq!(RcStr::from("hello").as_str(), "hello");
        let s = RcStr::from("héllo→世界");
        assert_eq!(s.as_str(), "héllo→世界");
        assert_eq!(s.as_bytes(), "héllo→世界".as_bytes());
        assert_eq!(&*s, "héllo→世界"); // Deref
    }

    #[test]
    fn clone_shares_allocation_and_refcounts() {
        let a = RcStr::from("shared");
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
        let base = RcStr::from("allocate me once");
        alloc_counter::reset();
        let clones: Vec<RcStr> = (0..1000).map(|_| base.clone()).collect();
        // The Vec itself allocates; the RcStr clones must not.
        assert_eq!(base.strong_count(), 1001);
        drop(clones);
        assert_eq!(base.strong_count(), 1);
        // `base` still valid after all clones dropped — no double free / UAF.
        assert_eq!(base.as_str(), "allocate me once");
    }

    #[test]
    fn one_allocation_per_string() {
        alloc_counter::reset();
        let s = RcStr::from("single block");
        assert_eq!(alloc_counter::count(), 1);
        assert_eq!(s.as_str(), "single block");
    }

    #[test]
    fn eq_pointer_fast_path_and_value_path() {
        let a = RcStr::from("abc");
        let b = a.clone(); // same allocation → pointer fast path
        let c = RcStr::from("abc"); // distinct allocation → value compare
        let d = RcStr::from("abd");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_ne!(a, d);
        assert_eq!(a, "abc");
        assert!(a < d);
    }

    #[test]
    fn works_as_map_key_via_str_borrow() {
        use indexmap::IndexMap;
        let mut m: IndexMap<RcStr, u32> = IndexMap::new();
        m.insert(RcStr::from("one"), 1);
        m.insert(RcStr::from("two"), 2);
        // Lookup by &str relies on Borrow<str> + matching Hash/Eq.
        assert_eq!(m.get("one"), Some(&1));
        assert_eq!(m.get("two"), Some(&2));
        assert_eq!(m.get("three"), None);
    }

    #[test]
    fn from_iter_concatenates() {
        let s: RcStr = ["a", "bc", "def"].into_iter().collect();
        assert_eq!(s.as_str(), "abcdef");
    }
}
