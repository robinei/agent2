mod analyzer;
mod builtin;
mod compiler;
mod diag;
mod prelude;
mod thin_string;
mod tree;
mod types;
mod vm;

pub use types::*;

fn main() {}

// ── test-only allocation counter ─────────────────────────────────────────────

#[cfg(test)]
mod alloc_counter {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNT: AtomicUsize = AtomicUsize::new(0);

    pub struct CountingAlloc;

    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            COUNT.fetch_add(1, Ordering::Relaxed);
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            COUNT.fetch_add(1, Ordering::Relaxed);
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(
            &self,
            ptr: *mut u8,
            layout: Layout,
            new_size: usize,
        ) -> *mut u8 {
            COUNT.fetch_add(1, Ordering::Relaxed);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    /// Read the current allocation count.
    pub fn count() -> usize {
        COUNT.load(Ordering::Relaxed)
    }

    /// Reset the counter to zero.
    pub fn reset() {
        COUNT.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
#[global_allocator]
static A: alloc_counter::CountingAlloc = alloc_counter::CountingAlloc;
