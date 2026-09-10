//! Compaction as a checked rewrite of the rendered record (phase 20
//! doc, Part E).
//!
//! This module is the "the program is checked before it commits"
//! half of Part E: given a compaction handler's proposed edits
//! (`remove_history`/`rewrite_history` calls), [`compact`] applies
//! them and validates the result atomically — every id still present
//! as at least a stub, nothing silently emptied, the result actually
//! under the threshold — or rejects the whole batch with a specific
//! reason the condition can re-fire with. It does not decide *when*
//! to compact ([`should_fire`] is a pure headroom check) and it does
//! not run the handler itself (that is Part D/C's VM-stack territory,
//! not built yet) — it is the validator a handler's output is checked
//! against, usable and testable on its own.
//!
//! "Never drop an id — only content": [`compact`] never removes an
//! `(EntryId, Entry)` pair from the log, only replaces the `Entry` at
//! an existing id with an [`Entry::CompactedStub`]. Order and ids are
//! preserved exactly.

use super::document;
use super::entry::{Entry, EntryId};

/// A compaction handler's request via `remove_history(id, label)` or
/// `rewrite_history(id, label, value)`.
///
/// `label` is the redundant checksum every id-bearing verb call
/// carries (Part B): it must match the target entry's own
/// [`Entry::label`], or the whole batch is rejected. With a dense
/// integer keyspace every wrong id is a *valid* id, so a mistake would
/// otherwise silently compact the wrong row — the label turns that
/// into a rejected call instead.
#[derive(Clone, Debug, PartialEq)]
pub enum CompactionOp {
    /// Drop this entry's content entirely, keeping only its id and
    /// label — the cheap, preferred operation (the card's guidance:
    /// "prefer removal and verbatim retention; rewrite only rows that
    /// genuinely need it").
    Remove { id: EntryId, label: String },
    /// Replace this entry's content with a shorter summary, keeping
    /// its id and label. Costs output tokens proportional to `text`,
    /// unlike `Remove` — the more expensive operation, for the rows
    /// that actually need rephrasing rather than dropping.
    Rewrite {
        id: EntryId,
        label: String,
        text: String,
    },
}

impl CompactionOp {
    fn id(&self) -> EntryId {
        match self {
            CompactionOp::Remove { id, .. } => *id,
            CompactionOp::Rewrite { id, .. } => *id,
        }
    }

    fn label(&self) -> &str {
        match self {
            CompactionOp::Remove { label, .. } => label,
            CompactionOp::Rewrite { label, .. } => label,
        }
    }
}

/// The fixed marker a bare `remove_history` leaves in place of
/// content — never empty, so "no row silently emptied" holds by
/// construction rather than needing every caller to remember it.
pub const REMOVED_MARKER: &str = "(removed)";

#[derive(Clone, Debug, PartialEq)]
pub enum CompactionError {
    /// The op named an id that isn't in the log at all.
    UnknownId(EntryId),
    /// The op's label doesn't match the entry's own label — the
    /// checksum catching a wrong-id mistake before it does damage.
    LabelMismatch {
        id: EntryId,
        expected: String,
        given: String,
    },
    /// A `Rewrite` supplied empty text — indistinguishable from
    /// silently emptying the row, which Part E forbids outright.
    EmptyRewrite(EntryId),
    /// Two ops in the same batch target the same id — ambiguous
    /// (which one wins?) and always a mistake, since a handler run
    /// gets one shot at each row.
    DuplicateTarget(EntryId),
    /// The batch, once applied, is still at or above the threshold —
    /// the real advantage of program-based compaction over
    /// regenerative summarization: this is checkable before commit,
    /// so a compactor that didn't free enough can be told so and
    /// asked again (the condition re-fires) rather than silently
    /// accepted.
    StillOverThreshold { size: usize, threshold: usize },
}

/// Apply a compaction batch to the rendered record and validate the
/// result, atomically: either every op is well-formed and the result
/// is under `threshold`, or nothing is applied and a specific
/// [`CompactionError`] says why — the report a re-fired condition
/// carries.
///
/// `size_of` measures a candidate record (typically
/// [`rendered_size`]); it is a parameter rather than hard-coded so a
/// caller can swap in a real token count later without this module
/// changing.
pub fn compact(
    log: &[(EntryId, Entry)],
    ops: &[CompactionOp],
    size_of: impl Fn(&[(EntryId, Entry)]) -> usize,
    threshold: usize,
) -> Result<Vec<(EntryId, Entry)>, CompactionError> {
    let mut seen = std::collections::HashSet::new();
    for op in ops {
        if !seen.insert(op.id()) {
            return Err(CompactionError::DuplicateTarget(op.id()));
        }
        if let CompactionOp::Rewrite { id, text, .. } = op
            && text.is_empty()
        {
            return Err(CompactionError::EmptyRewrite(*id));
        }
    }

    let mut result: Vec<(EntryId, Entry)> = log.to_vec();
    for op in ops {
        let target = result
            .iter_mut()
            .find(|(id, _)| *id == op.id())
            .ok_or(CompactionError::UnknownId(op.id()))?;
        let actual_label = target.1.label();
        if actual_label != op.label() {
            return Err(CompactionError::LabelMismatch {
                id: op.id(),
                expected: actual_label.to_owned(),
                given: op.label().to_owned(),
            });
        }
        target.1 = match op {
            CompactionOp::Remove { label, .. } => Entry::CompactedStub {
                label: label.clone(),
                text: REMOVED_MARKER.to_owned(),
            },
            CompactionOp::Rewrite { label, text, .. } => Entry::CompactedStub {
                label: label.clone(),
                text: text.clone(),
            },
        };
    }

    // "Every id still present as at least a stub" — guaranteed by
    // construction above (entries are replaced in place, never
    // removed from the vec), asserted here as a cheap, load-bearing
    // check against a future refactor breaking that invariant
    // silently.
    debug_assert_eq!(
        result.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        log.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        "compact() must preserve every id and their order"
    );

    let size = size_of(&result);
    if size >= threshold {
        return Err(CompactionError::StillOverThreshold { size, threshold });
    }
    Ok(result)
}

/// A byte-length proxy for the rendered record's size — the card plus
/// every turn's content, summed. Not a real token count (there is no
/// tokenizer in this crate), but a legitimate, monotone, testable
/// stand-in: it grows and shrinks exactly when the real token count
/// would, which is all [`should_fire`] and [`compact`]'s threshold
/// check need. Swap for a real count without changing either
/// function's shape.
pub fn rendered_size(card: &str, log: &[(EntryId, Entry)]) -> usize {
    match document::render(card, log) {
        Ok(doc) => doc.messages.iter().map(|m| m.content.len()).sum(),
        // A malformed log (Step B1b's invariant broken) has no
        // rendering to size — treat as unbounded so the caller's
        // threshold check fails loudly rather than silently passing.
        Err(_) => usize::MAX,
    }
}

/// Fires at a headroom threshold, not at overflow (Part E): the
/// compaction handler is itself an inference whose prompt contains
/// the history that already doesn't fit, so there must be room left
/// for the handler's own prompt and program. `headroom_fraction` is
/// the fraction of `budget` reserved for that — e.g. `0.2` fires once
/// 80% of budget is used, leaving 20% for the handler.
pub fn should_fire(current_size: usize, budget: usize, headroom_fraction: f64) -> bool {
    let usable = (budget as f64 * (1.0 - headroom_fraction)) as usize;
    current_size >= usable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codemode::entry::ProgramOutcome;
    use crate::types::EventId;

    fn id(n: u64) -> EntryId {
        EventId::new(n)
    }

    fn sample_log() -> Vec<(EntryId, Entry)> {
        vec![
            (
                id(1),
                Entry::Message {
                    from: "robin".into(),
                    text: "go".into(),
                },
            ),
            (
                id(2),
                Entry::Program {
                    source: "1;".into(),
                    outcome: ProgramOutcome::Completed,
                },
            ),
            (
                id(3),
                Entry::Effects {
                    of: id(2),
                    wrote: vec!["big_file.rs".into()],
                    ran: vec![],
                    read: 0,
                    spawned: vec![],
                },
            ),
            (
                id(4),
                Entry::Note {
                    from: Some(id(2)),
                    text: "a long note that takes up a lot of space in the record".into(),
                },
            ),
        ]
    }

    #[test]
    fn remove_replaces_content_with_the_fixed_marker() {
        let log = sample_log();
        let ops = [CompactionOp::Remove {
            id: id(4),
            label: "note".into(),
        }];
        let after = compact(&log, &ops, |l| rendered_size("CARD", l), 10_000).unwrap();
        assert_eq!(
            after[3].1,
            Entry::CompactedStub {
                label: "note".into(),
                text: REMOVED_MARKER.into(),
            }
        );
    }

    #[test]
    fn rewrite_replaces_content_with_the_supplied_summary() {
        let log = sample_log();
        let ops = [CompactionOp::Rewrite {
            id: id(4),
            label: "note".into(),
            text: "note was long".into(),
        }];
        let after = compact(&log, &ops, |l| rendered_size("CARD", l), 10_000).unwrap();
        assert_eq!(
            after[3].1,
            Entry::CompactedStub {
                label: "note".into(),
                text: "note was long".into(),
            }
        );
    }

    #[test]
    fn every_id_survives_at_least_as_a_stub() {
        let log = sample_log();
        let ops = [
            CompactionOp::Remove {
                id: id(3),
                label: "effects".into(),
            },
            CompactionOp::Remove {
                id: id(4),
                label: "note".into(),
            },
        ];
        let after = compact(&log, &ops, |l| rendered_size("CARD", l), 10_000).unwrap();
        let before_ids: Vec<_> = log.iter().map(|(id, _)| *id).collect();
        let after_ids: Vec<_> = after.iter().map(|(id, _)| *id).collect();
        assert_eq!(before_ids, after_ids);
    }

    #[test]
    fn unknown_id_is_rejected() {
        let log = sample_log();
        let ops = [CompactionOp::Remove {
            id: id(99),
            label: "note".into(),
        }];
        assert_eq!(
            compact(&log, &ops, |l| rendered_size("CARD", l), 10_000),
            Err(CompactionError::UnknownId(id(99)))
        );
    }

    #[test]
    fn wrong_label_is_rejected_even_for_a_real_id() {
        // The checksum catching a plausible mistake: id 4 is real,
        // but its label is "note", not "effects" — a handler that
        // mixed up which id was which gets told, rather than silently
        // compacting the wrong row.
        let log = sample_log();
        let ops = [CompactionOp::Remove {
            id: id(4),
            label: "effects".into(),
        }];
        assert_eq!(
            compact(&log, &ops, |l| rendered_size("CARD", l), 10_000),
            Err(CompactionError::LabelMismatch {
                id: id(4),
                expected: "note".into(),
                given: "effects".into(),
            })
        );
    }

    #[test]
    fn empty_rewrite_is_rejected_as_silently_emptying_a_row() {
        let log = sample_log();
        let ops = [CompactionOp::Rewrite {
            id: id(4),
            label: "note".into(),
            text: "".into(),
        }];
        assert_eq!(
            compact(&log, &ops, |l| rendered_size("CARD", l), 10_000),
            Err(CompactionError::EmptyRewrite(id(4)))
        );
    }

    #[test]
    fn duplicate_target_in_one_batch_is_rejected() {
        let log = sample_log();
        let ops = [
            CompactionOp::Remove {
                id: id(4),
                label: "note".into(),
            },
            CompactionOp::Rewrite {
                id: id(4),
                label: "note".into(),
                text: "second op on the same id".into(),
            },
        ];
        assert_eq!(
            compact(&log, &ops, |l| rendered_size("CARD", l), 10_000),
            Err(CompactionError::DuplicateTarget(id(4)))
        );
    }

    #[test]
    fn a_batch_that_does_not_free_enough_is_rejected_and_nothing_applies() {
        let log = sample_log();
        let before_size = rendered_size("CARD", &log);
        // Threshold impossible to clear even after compacting
        // everything compactable — the batch must be rejected as a
        // whole, not partially applied.
        let ops = [CompactionOp::Remove {
            id: id(4),
            label: "note".into(),
        }];
        let result = compact(&log, &ops, |l| rendered_size("CARD", l), 1);
        assert!(matches!(
            result,
            Err(CompactionError::StillOverThreshold { .. })
        ));
        // The rejection carries the size, and it must be smaller than
        // before (compaction *did* shrink things) even though it
        // wasn't enough — the report a re-fired condition needs.
        if let Err(CompactionError::StillOverThreshold { size, .. }) = result {
            assert!(size < before_size);
        }
    }

    #[test]
    fn should_fire_respects_headroom_not_the_hard_limit() {
        // 20% headroom: fires at 80% of budget, not at 100%.
        assert!(!should_fire(799, 1000, 0.2));
        assert!(should_fire(800, 1000, 0.2));
        assert!(should_fire(1000, 1000, 0.2));
    }

    #[test]
    fn rendered_size_grows_with_content() {
        let small = vec![(
            id(1),
            Entry::Message {
                from: "a".into(),
                text: "hi".into(),
            },
        )];
        let bigger = {
            let mut v = small.clone();
            v.push((
                id(2),
                Entry::Message {
                    from: "a".into(),
                    text: "a much longer message with a lot more content in it".into(),
                },
            ));
            v
        };
        assert!(rendered_size("CARD", &small) < rendered_size("CARD", &bigger));
    }
}
