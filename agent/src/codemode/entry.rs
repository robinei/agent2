//! History entries — the rendered record (phase 20 doc, Part B).
//!
//! An entry is one row of the **rendered record**: what a root
//! program did, plus the automatic and voluntary things that enter
//! history under DESIGN.md's rule C (phase 20 Step B2). It is
//! deliberately not the whole event log — a handler program's own
//! completions are logged (so `raise()`-placement can be read back
//! later) but never become entries here, which is what keeps a
//! forty-raise program's rendered footprint at exactly the entries its
//! *root* program and appends produced (Step B2's test).
//!
//! There is no `code()` liveness question and no JS-literal escaping
//! here (Step B1): an entry is a plain Rust value, not JS source
//! embedded in JS source. Only [`Entry::Program`]'s `source` field is
//! ever asked to parse as JavaScript.

use crate::types::EventId;

/// An entry's identity in the rendered record. Reused from the event
/// log's id type (not a fresh newtype) so this module composes
/// without translation once entries are backed by real log events —
/// see the module doc's note on what is and is not wired up yet.
pub type EntryId = EventId;

/// How a root or handler program ended, carried on [`Entry::Program`]
/// so the record can distinguish "did this and it worked" from "tried
/// this and it blew up" (Step B1: "Program entries carry status, not
/// just source").
#[derive(Clone, Debug, PartialEq)]
pub enum ProgramOutcome {
    Completed,
    /// A runtime trap. `line` is 1-based, matching the source the
    /// model wrote — the report must be able to say "line 30", not a
    /// raw byte offset.
    Trapped {
        line: u32,
        message: String,
    },
    /// Discarded by a handler's `abandon()` decision (Part D2).
    // Not yet constructed anywhere: no code drives a program to this
    // outcome until Part D's handler stack lands.
    #[allow(dead_code)]
    Abandoned,
}

/// A shell command a program ran, as an effects-row entry (Step B2).
/// Only the fact and its identity render — never the output, which
/// stays fetchable as `#{output}` (`artifact(output)`, Step C1).
#[derive(Clone, Debug, PartialEq)]
pub struct RanCommand {
    pub cmd: String,
    pub exit: i32,
    /// The call id whose `Result` holds the command's stdout/stderr —
    /// never inlined here (Step B2: "facts and identities, never
    /// content — not even truncated").
    pub output: EntryId,
}

/// One row of the rendered record.
///
/// Deliberately **not** `#[non_exhaustive]`: this enum is the complete
/// vocabulary Step B2 specifies for what may enter history
/// automatically (`Message`, `Effects`, `Note`) plus the one thing a
/// root program's own turn is (`Program`) and compaction's stub
/// (`CompactedStub`, Part E). Adding a new automatic-entry kind is a
/// design decision this file's doc should record, not a quiet enum
/// variant.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// An inbound message nobody's program was awaiting — a user
    /// utterance or an unsolicited post from another agent (rule C).
    /// Enters automatically; the harness never invents one.
    Message { from: String, text: String },

    /// A root program's own turn: bare source, no entry header (Step
    /// B1) — the assistant-turn content is exactly `source` and
    /// nothing else. `outcome` does **not** render as part of the
    /// assistant turn; it renders into the *following* user turn
    /// (Step B1: "Status and effects belong to the following user
    /// turn, not to the assistant turn").
    Program {
        source: String,
        outcome: ProgramOutcome,
    },

    /// The automatic, one-per-program record of what a program did to
    /// the world (Step B2). `of` names the [`Entry::Program`] this
    /// reports on. Reads aggregate (`read`); writes, commands and
    /// spawns are named because they are durable changes someone may
    /// need to review or undo.
    Effects {
        of: EntryId,
        wrote: Vec<String>,
        ran: Vec<RanCommand>,
        read: usize,
        spawned: Vec<String>,
    },

    /// Something a mind chose to remember (`append_history`, Step
    /// C1) — the one voluntary channel into a program's own future.
    /// `from` names the program that appended it, when known (a root
    /// program's own append) — `None` for a note surfacing from a
    /// handler's decision, which has no single program to attribute
    /// it to at the rendered-record level.
    Note { from: Option<EntryId>, text: String },

    /// What compaction (Part E) leaves behind: the id and a
    /// human-labelled one-liner, never the original content. "Never
    /// drop an id — only content."
    CompactedStub { label: String, text: String },
}

impl Entry {
    /// A short label for this entry, used by compaction and by
    /// id-bearing verb calls as the redundant checksum Part B
    /// specifies (`remove_history(id, label)` must match). Kept here,
    /// next to the entry it describes, rather than recomputed at every
    /// call site.
    pub fn label(&self) -> &str {
        match self {
            Entry::Message { from, .. } => from,
            Entry::Program { .. } => "program",
            Entry::Effects { .. } => "effects",
            Entry::Note { .. } => "note",
            Entry::CompactedStub { label, .. } => label,
        }
    }
}
