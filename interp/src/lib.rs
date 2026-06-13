//! JS interpreter crate — compiler, VM, analyzer, optimizer, builtins.
//!
//! The public surface is what an embedder (like the `agent` crate) needs:
//! compilation, execution, and the value/instruction types it inspects.

pub mod analyzer;
pub mod builtin;
pub mod compiler;
pub mod debuginfo;
pub mod diag;
pub mod optimizer;
pub mod prelude;
pub mod rc_str;
pub mod vm;

#[cfg(test)]
pub(crate) mod testutil;

pub use compiler::{Program, compile};
pub use debuginfo::{DebugTable, FnDebug};
pub use diag::Diagnostic;
pub use rc_str::RcStr;
pub use vm::{
    ErrorKind, FrameView, Instr, InvokeCall, PromisePtr, PromiseState, ResumeMode, StepResult, VM,
    VMError, Value,
};

/// Host-seeded read-only consts available to every program by name:
/// `input` (the frame's caller-provided JSON, `objects[0]`) and
/// `attachments` (this run's authored content, `objects[1]`). They may
/// not be shadowed or reassigned, and the compiler maps each name to its
/// fixed object slot. Single source of truth for the compiler/analyzer
/// checks and the VM's slot reservation.
pub(crate) fn is_host_const(name: &str) -> bool {
    matches!(name, "input" | "attachments")
}

// ── test-only allocation counter ─────────────────────────────────────────────

#[cfg(test)]
mod alloc_counter {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        // Per-thread, not a global atomic: `cargo test` runs each test on its
        // own thread in parallel, so a shared counter would attribute other
        // tests' allocations to whichever test is measuring. `const` init keeps
        // the TLS access allocation-free (no lazy heap init), so `bump()` can't
        // re-enter the allocator.
        static COUNT: Cell<usize> = const { Cell::new(0) };
    }

    fn bump() {
        // `try_with`: a thread may still allocate while its TLS is being torn
        // down, where `with` would panic. Such allocations simply go uncounted.
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }

    pub struct CountingAlloc;

    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            bump();
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            bump();
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            bump();
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    /// Read the calling thread's allocation count.
    pub fn count() -> usize {
        COUNT.with(|c| c.get())
    }

    /// Reset the calling thread's counter to zero.
    pub fn reset() {
        COUNT.with(|c| c.set(0));
    }
}

#[cfg(test)]
#[global_allocator]
static A: alloc_counter::CountingAlloc = alloc_counter::CountingAlloc;
