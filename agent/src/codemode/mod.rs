//! Phase 20 — code mode (`docs/20_CODE_MODE.md`).
//!
//! This module is additive: it runs **beside** the existing chat/
//! `run_program`-tool path, not through it (ground rules, phase 20
//! doc). Nothing here is wired into `machine.rs`'s step loop yet —
//! that is the deep integration the doc's Parts C/D require, and it
//! touches the compiler's call-resolution and the VM's suspension
//! machinery, both load-bearing for the existing system's conformance
//! and behaviour. This module builds the parts that are pure,
//! testable, and safe to land first: the document a completion
//! request is built from (Part A/B), and how a raw completion is
//! turned back into a program (the no-fence rule, Part A).

pub mod compaction;
pub mod decision;
pub mod document;
pub mod entry;
pub mod fence;
pub mod introspect;
pub mod stack;
pub mod transport;
pub mod verbs;
