//! Compaction as a checked batch of `Compacted` events (phase 20 doc,
//! Part E).
//!
//! This module is the "the program is checked before it commits" half
//! of Part E: given a compaction handler's proposed edits
//! (`remove_history`/`rewrite_history` calls, parsed elsewhere into
//! [`CompactionOp`]), [`compact`] validates them against the **log** —
//! every id real and correctly labelled, no duplicate target, no
//! silently-emptied row, the result actually under the threshold — and
//! either returns the `EventPayload::Compacted` events a caller should
//! append, or a specific [`CompactionError`] the condition can re-fire
//! with. It does not decide *when* to compact ([`should_fire`] is a
//! pure headroom check) and it does not run the handler itself (that is
//! Part D/C's VM-stack territory) — it is the validator a handler's
//! output is checked against, usable and testable on its own.
//!
//! **Never drop an id — only content.** Unlike the deleted POC, which
//! operated on a private row vector (its own id space, mutated in
//! place), this module never rewrites the log at all: it only *proposes*
//! `EventPayload::Compacted { of, label, text }` events, which a caller
//! appends the ordinary way (`Tree::append`). The log stays
//! append-only, so "every id still present as at least a stub" is no
//! longer an invariant [`compact`] has to maintain by careful
//! construction — it is true *by the shape of the architecture itself*:
//! nothing this module can do removes a row, only shadow it. That is
//! the real advantage over regenerative summarization named in
//! `23_ONE_AGENT.md`: a summary blob is opaque and cannot be asserted
//! on; a proposed batch of `Compacted` events can be, before it ever
//! touches the log.
//!
//! **Not wired into `dispatch_calls` yet — deliberately, not by
//! oversight.** `machine.rs`'s `TOOL_REMOVE_HISTORY`/`TOOL_REWRITE_HISTORY`
//! arm rejects both calls with an explicit message ("compaction is not
//! wired into this session yet... leaves remove_history/rewrite_history
//! to a later pass") rather than guessing at this module's API while it
//! was still being rewritten in the same phase. That later pass has no
//! caller for this module's public surface yet, so it trips `dead_code`
//! under `-D warnings`; `#![allow(dead_code)]` here is the Pass B
//! judgment call this module falls under (23_ONE_AGENT.md Pass B: "its
//! caller is yet to be written"), not a blanket exemption — every item
//! below still has its own doc and its own tests.
#![allow(dead_code)]

use crate::document::{self, label_of};
use crate::tree::CompactedView;
use crate::types::*;

/// A compaction handler's request via `remove_history(id, label)` or
/// `rewrite_history(id, label, value)`.
///
/// `label` is the redundant checksum every id-bearing verb call
/// carries (Part B): it must match the target event's own
/// [`document::label_of`], or the whole batch is rejected. With a dense
/// integer keyspace every wrong id is a *valid* id, so a mistake would
/// otherwise silently compact the wrong row — the label turns that
/// into a rejected call instead.
#[derive(Clone, Debug, PartialEq)]
pub enum CompactionOp {
    /// Drop this event's content entirely, keeping only its id and
    /// label — the cheap, preferred operation (the card's guidance:
    /// "prefer removal and verbatim retention; rewrite only rows that
    /// genuinely need it").
    Remove { id: EventId, label: String },
    /// Replace this event's rendered content with a shorter summary,
    /// keeping its id and label. Costs output tokens proportional to
    /// `text`, unlike `Remove` — the more expensive operation, for the
    /// rows that actually need rephrasing rather than dropping.
    Rewrite {
        id: EventId,
        label: String,
        text: String,
    },
}

impl CompactionOp {
    fn id(&self) -> EventId {
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

    /// The `EventPayload::Compacted` this op becomes once it has
    /// cleared validation — the thing a caller actually appends.
    fn into_payload(self) -> EventPayload {
        match self {
            CompactionOp::Remove { id, label } => EventPayload::Compacted {
                of: id,
                label,
                text: None,
            },
            CompactionOp::Rewrite { id, label, text } => EventPayload::Compacted {
                of: id,
                label,
                text: Some(text),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CompactionError {
    /// The op named an id that isn't on this branch's path at all.
    UnknownId(EventId),
    /// The op's label doesn't match the target event's own kind — the
    /// checksum catching a wrong-id mistake before it does damage.
    LabelMismatch {
        id: EventId,
        expected: String,
        given: String,
    },
    /// A `Rewrite` supplied empty text — indistinguishable from
    /// silently emptying the row, which Part E forbids outright.
    EmptyRewrite(EventId),
    /// Two ops in the same batch target the same id — ambiguous
    /// (which one wins?) and always a mistake, since a handler run
    /// gets one shot at each row.
    DuplicateTarget(EventId),
    /// The batch, once applied, is still at or above the threshold —
    /// the real advantage of program-based compaction over
    /// regenerative summarization: this is checkable before commit,
    /// so a compactor that didn't free enough can be told so and
    /// asked again (the condition re-fires) rather than silently
    /// accepted.
    StillOverThreshold { size: usize, threshold: usize },
}

/// The fixed marker a bare `remove_history` leaves in place of
/// content — never empty, so "no row silently emptied" holds by
/// construction rather than needing every caller to remember it.
pub const REMOVED_MARKER: &str = "(removed)";

/// Validate a compaction batch against `spine`'s path and, if it clears
/// every check, return the `EventPayload::Compacted` events a caller
/// should append (in `ops`' own order) — or a specific
/// [`CompactionError`] the condition can re-fire with. Applies nothing:
/// this is a pure check plus a dry-run render, never a `&mut Tree`.
///
/// The dry-run is real, not approximate: it renders the *actual*
/// document `document::render` would produce with this batch's ops
/// folded into the current compaction lookup, via the same low-level
/// fold (`render_with_lookup`) `render` itself calls — not a
/// byte-counting proxy that could disagree with it. `spine` is the same
/// handle `document::render` now takes: its `system` supplies the card
/// text for the dry-run, so a compaction check renders under exactly
/// the prompt a real request would.
pub fn compact(
    tree: &Tree,
    spine: &Spine,
    ops: &[CompactionOp],
    budget: usize,
    threshold: usize,
) -> Result<Vec<EventPayload>, CompactionError> {
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

    let leaf = spine.leaf_id;
    let agent = tree
        .enclosing_agent(leaf)
        .expect("a spine's leaf always has an enclosing Agent");
    let card = &spine.context().system;

    let path = tree.path_events(leaf);
    let mut lookup = tree.compacted_lookup(leaf);
    for op in ops {
        let Some(target) = path.iter().find(|e| e.id == op.id()) else {
            return Err(CompactionError::UnknownId(op.id()));
        };
        let expected = label_of(&target.payload);
        if expected != op.label() {
            return Err(CompactionError::LabelMismatch {
                id: op.id(),
                expected: expected.to_owned(),
                given: op.label().to_owned(),
            });
        }
        let text = match op {
            CompactionOp::Remove { .. } => None,
            CompactionOp::Rewrite { text, .. } => Some(text.clone()),
        };
        lookup.insert(
            op.id(),
            CompactedView {
                label: op.label().to_owned(),
                text,
            },
        );
    }

    let doc = document::render_with_lookup(tree, agent, leaf, card, budget, &lookup);
    let size = rendered_size(&doc);
    if size >= threshold {
        return Err(CompactionError::StillOverThreshold { size, threshold });
    }

    Ok(ops
        .iter()
        .cloned()
        .map(CompactionOp::into_payload)
        .collect())
}

/// Total content bytes across a rendered [`document::Document`] — a
/// byte-length proxy for what the model attends to, not a real token
/// count (there is no tokenizer in this crate), but a legitimate,
/// monotone, testable stand-in: it grows and shrinks exactly when the
/// real token count would, which is all [`should_fire`] and [`compact`]'s
/// threshold check need. Swap for a real count without changing either
/// function's shape.
pub fn rendered_size(doc: &document::Document) -> usize {
    doc.messages.iter().map(|m| m.content.len()).sum()
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
    use crate::document::render;

    /// A small real branch: a user post, a completed program, and a
    /// `Note` — three ids to target, and a card/budget combination that
    /// makes the size checks exercisable.
    fn sample_branch() -> (Tree, Spine, EventId, EventId) {
        let mut tree = Tree::new(None);
        let mut spine = tree.start_agent(None, None, "root", None, "CARD").unwrap();
        let agent = spine.leaf_id;
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Post {
                from: Author::User,
                origin: Origin::Direct {
                    text: "go".into(),
                    input: serde_json::Value::Null,
                    expects_reply: true,
                },
            }),
        )
        .unwrap();
        let program = tree
            .append(
                &mut spine,
                EventPayload::Message(Message::Turn {
                    author: Author::Agent(agent),
                    source: "1;".into(),
                    thinking: None,
                    usage: None,
                }),
            )
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Return {
                value: serde_json::json!(1),
            },
        )
        .unwrap();
        let note = tree
            .append(
                &mut spine,
                EventPayload::Note {
                    text: "a long note that takes up a lot of space in the record".into(),
                },
            )
            .unwrap();
        (tree, spine, program, note)
    }

    #[test]
    fn unknown_id_is_rejected() {
        let (tree, spine, _program, _note) = sample_branch();
        let ops = [CompactionOp::Remove {
            id: EventId::new(9999),
            label: "note".into(),
        }];
        assert_eq!(
            compact(&tree, &spine, &ops, 64 * 1024, 10_000),
            Err(CompactionError::UnknownId(EventId::new(9999)))
        );
    }

    #[test]
    fn wrong_label_is_rejected_even_for_a_real_id() {
        let (tree, spine, _program, note) = sample_branch();
        let ops = [CompactionOp::Remove {
            id: note,
            label: "turn".into(),
        }];
        assert_eq!(
            compact(&tree, &spine, &ops, 64 * 1024, 10_000),
            Err(CompactionError::LabelMismatch {
                id: note,
                expected: "note".into(),
                given: "turn".into(),
            })
        );
    }

    #[test]
    fn empty_rewrite_is_rejected_as_silently_emptying_a_row() {
        let (tree, spine, _program, note) = sample_branch();
        let ops = [CompactionOp::Rewrite {
            id: note,
            label: "note".into(),
            text: "".into(),
        }];
        assert_eq!(
            compact(&tree, &spine, &ops, 64 * 1024, 10_000),
            Err(CompactionError::EmptyRewrite(note))
        );
    }

    #[test]
    fn duplicate_target_in_one_batch_is_rejected() {
        let (tree, spine, _program, note) = sample_branch();
        let ops = [
            CompactionOp::Remove {
                id: note,
                label: "note".into(),
            },
            CompactionOp::Rewrite {
                id: note,
                label: "note".into(),
                text: "second op on the same id".into(),
            },
        ];
        assert_eq!(
            compact(&tree, &spine, &ops, 64 * 1024, 10_000),
            Err(CompactionError::DuplicateTarget(note))
        );
    }

    #[test]
    fn a_batch_that_does_not_free_enough_is_rejected() {
        let (tree, spine, _program, note) = sample_branch();
        let before = rendered_size(&render(&tree, &spine, 64 * 1024));
        // Threshold impossible to clear even after compacting
        // everything compactable.
        let ops = [CompactionOp::Remove {
            id: note,
            label: "note".into(),
        }];
        let result = compact(&tree, &spine, &ops, 64 * 1024, 1);
        assert!(matches!(
            result,
            Err(CompactionError::StillOverThreshold { .. })
        ));
        if let Err(CompactionError::StillOverThreshold { size, .. }) = result {
            assert!(
                size < before,
                "compaction did shrink things, just not enough: {size} vs {before}"
            );
        }
    }

    #[test]
    fn a_valid_batch_returns_compacted_events_that_shadow_every_target() {
        let (tree, spine, _program, note) = sample_branch();
        let ops = [CompactionOp::Remove {
            id: note,
            label: "note".into(),
        }];
        let payloads = compact(&tree, &spine, &ops, 64 * 1024, 100_000).unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(
            payloads[0],
            EventPayload::Compacted {
                of: note,
                label: "note".into(),
                text: None,
            }
        );
    }

    #[test]
    fn rewrite_carries_its_text_into_the_compacted_event() {
        let (tree, spine, _program, note) = sample_branch();
        let ops = [CompactionOp::Rewrite {
            id: note,
            label: "note".into(),
            text: "note was long".into(),
        }];
        let payloads = compact(&tree, &spine, &ops, 64 * 1024, 100_000).unwrap();
        assert_eq!(
            payloads[0],
            EventPayload::Compacted {
                of: note,
                label: "note".into(),
                text: Some("note was long".into()),
            }
        );
    }

    /// **Never drop an id — only content.** Once a caller appends the
    /// `Compacted` event `compact` proposed, the target id still
    /// resolves — as a stub, not a hole — and the branch keeps
    /// rendering.
    #[test]
    fn appending_the_proposed_event_leaves_the_id_resolvable_as_a_stub() {
        let (mut tree, mut spine, _program, note) = sample_branch();
        let ops = [CompactionOp::Remove {
            id: note,
            label: "note".into(),
        }];
        let payloads = compact(&tree, &spine, &ops, 64 * 1024, 100_000).unwrap();
        for payload in payloads {
            tree.append(&mut spine, payload).unwrap();
        }

        let doc = render(&tree, &spine, 64 * 1024);
        let rendered: String = doc
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let marker = format!("[{}]", note.as_u64());
        assert!(rendered.contains(&marker), "the id must still round-trip");
        assert!(rendered.contains(REMOVED_MARKER));
        assert!(
            !rendered.contains("a long note that takes up a lot of space"),
            "the content is gone, only the id and label remain"
        );
    }

    #[test]
    fn should_fire_respects_headroom_not_the_hard_limit() {
        // 20% headroom: fires at 80% of budget, not at 100%.
        assert!(!should_fire(799, 1000, 0.2));
        assert!(should_fire(800, 1000, 0.2));
        assert!(should_fire(1000, 1000, 0.2));
    }
}
