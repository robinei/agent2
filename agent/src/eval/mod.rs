//! The acceptance harness (`23_ONE_AGENT.md`, Pass C).
//!
//! Formerly `codemode/{tasks,harness}.rs`, where it drove the POC's
//! standalone `runner::run` loop. It survives the POC because it is the
//! only instrument that answers the question this project actually
//! asks — what a real model does with a real card — and no scripted LLM
//! can stand in for that. Pass C re-points it at the real `Session`
//! over `SessionCommand`/`SessionEvent`; the tasks and their check
//! functions carry over unchanged.
//!
//! The discipline the checks hold to, and must keep holding: **a check
//! gates on the safety or correctness property, never on which verb
//! fired.** Verb choice is the observational variable — gating on it
//! would make the harness confirm its own card rather than measure it.
//!
//! Never part of `cargo test`: it talks to a live model over the
//! network. It is reached through `agent eval`.

pub mod harness;
pub mod tasks;
