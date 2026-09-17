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
//! **Wired, as of phase 27.** `machine.rs` fires
//! [`Cause::Compaction`] from `prompt_if_needed` when [`should_fire`]
//! says the document has outgrown its budget, collects the handler's
//! `remove_history`/`rewrite_history` calls into a batch, and commits
//! that batch through [`compact`] when the handler's program returns.
//! The `#![allow(dead_code)]` this module carried while its caller was
//! unwritten is gone with it.

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
    /// The op named something that is on the path but is not a row of
    /// the conversation — the agent's own root, a structural event.
    ///
    /// `label_of`'s fallback is the literal `"event"`, which means "no
    /// name for this kind of row", and those never render into the
    /// document at all. Compacting one is a no-op that would look like
    /// a success and free nothing, so it is a refusal instead: a live
    /// compaction program on 2026-09-16 opened with
    /// `remove_history(1, "system")`, aiming at the card, which is not
    /// a row and cannot be made smaller this way.
    ///
    /// A `Call` and a `Result` land here too, and that is the right
    /// answer rather than a gap in `label_of`: they have no line of
    /// their own anywhere, because they are rendered *inside* the
    /// completion report of the `Return` that closed their program.
    /// Removing that `return` takes the whole menu with it, which is
    /// what `compacting_a_return_removes_the_report_rendered_around_it`
    /// pins.
    NotARow(EventId),
    /// The batch, once applied, is still at or above the threshold —
    /// the real advantage of program-based compaction over
    /// regenerative summarization: this is checkable before commit,
    /// so a compactor that didn't free enough can be told so and
    /// asked again (the condition re-fires) rather than silently
    /// accepted.
    StillOverThreshold { size: usize, threshold: usize },
}

impl std::fmt::Display for CompactionError {
    /// Written for the model, not for a log: each one says what was
    /// wrong *and* what to do about it, because every one of these is
    /// a retry rather than a failure — the log is untouched, the
    /// condition re-fires, and the next program gets another go.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactionError::UnknownId(id) => write!(
                f,
                "#{} is not a row on this conversation — only the rows shown above can be \
                 compacted, by the id each one carries",
                id.as_u64()
            ),
            CompactionError::LabelMismatch {
                id,
                expected,
                given,
            } => write!(
                f,
                "#{} is a `{expected}`, not a `{given}` — the label is checked against the \
                 row so a wrong id cannot compact the wrong thing. Nothing was changed.",
                id.as_u64()
            ),
            CompactionError::EmptyRewrite(id) => write!(
                f,
                "the rewrite for #{0} was empty — use `remove_history(#{0}, label)` to drop \
                 a row's content; a rewrite has to leave something readable behind",
                id.as_u64()
            ),
            CompactionError::DuplicateTarget(id) => write!(
                f,
                "#{} was named twice in one batch — each row gets one operation",
                id.as_u64()
            ),
            CompactionError::NotARow(id) => write!(
                f,
                "#{} is not a row of the conversation — it is structural, and the card and \
                 the worked examples above it are fixed. Only the numbered rows can be \
                 compacted. Nothing was changed.",
                id.as_u64()
            ),
            CompactionError::StillOverThreshold { size, threshold } => write!(
                f,
                "that batch would leave {size} bytes against a {threshold}-byte threshold, so \
                 it was not applied. Nothing changed — compact more of it and return again."
            ),
        }
    }
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
    let context = spine.context();

    let path = tree.path_events(leaf);
    let mut lookup = tree.compacted_lookup(leaf);
    for op in ops {
        let Some(target) = path.iter().find(|e| e.id == op.id()) else {
            return Err(CompactionError::UnknownId(op.id()));
        };
        let expected = label_of(&target.payload);
        if expected == "event" {
            return Err(CompactionError::NotARow(op.id()));
        }
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

    let doc = document::render_with_lookup(
        tree,
        agent,
        leaf,
        &context.system,
        &context.exemplars,
        budget,
        &lookup,
    );
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
    doc.messages
        .iter()
        .map(|m| {
            // Under `Transport::RunProgram` a program is not in
            // `content` — it is the `source` of the turn's tool call,
            // and it is usually the largest thing in the document. Only
            // counting `content` there would undercount every assistant
            // turn, so compaction would fire late in one transport and
            // on time in the other, which would quietly make a
            // comparison between them a comparison of when they
            // compacted.
            m.content.len()
                + m.tool_calls
                    .iter()
                    .flatten()
                    .map(|c| c.source.len())
                    .sum::<usize>()
        })
        .sum()
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
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
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

    /// A branch whose program made one call with a big result, so the
    /// completion report around its `Return` is the largest thing in
    /// the document — the shape every real run has.
    fn branch_with_a_report() -> (Tree, Spine, EventId) {
        let mut tree = Tree::new(None);
        let mut spine = tree
            .start_agent(None, None, "root", None, "CARD", Vec::new())
            .unwrap();
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
        tree.append(
            &mut spine,
            EventPayload::Message(Message::Turn {
                author: Author::Agent(agent),
                source: "await tools.bash('cargo check');".into(),
                thinking: None,
                usage: None,
            }),
        )
        .unwrap();
        let call = tree
            .append(
                &mut spine,
                EventPayload::Call(Call::Invoke {
                    name: "bash".into(),
                    args: serde_json::json!(["cargo check --all-targets 2>&1"]),
                    site: 0,
                }),
            )
            .unwrap();
        tree.append(
            &mut spine,
            EventPayload::Result {
                call,
                outcome: Outcome::Delivered(serde_json::json!({
                    "status": 0,
                    "stdout": "warning: unused\n".repeat(200),
                })),
            },
        )
        .unwrap();
        let ret = tree
            .append(
                &mut spine,
                EventPayload::Return {
                    value: serde_json::json!("checked"),
                },
            )
            .unwrap();
        (tree, spine, ret)
    }

    /// **The completion report is a row.** Until 27.3 this arm of
    /// `render_with_lookup` called `derive_report` unconditionally, so
    /// the return preview, the console and the artifact menu were the
    /// one part of a conversation compaction could not reach: the
    /// checksum accepted `remove_history(id, "return")`, the dry run
    /// came back the same size, and the batch was refused for freeing
    /// nothing. Two live compaction programs hit exactly that.
    #[test]
    fn compacting_a_return_removes_the_report_rendered_around_it() {
        let (tree, spine, ret) = branch_with_a_report();
        let before = rendered_size(&render(&tree, &spine, 4096));
        let ops = [CompactionOp::Remove {
            id: ret,
            label: "return".into(),
        }];
        let events = compact(&tree, &spine, &ops, 4096, before).unwrap();
        assert_eq!(events.len(), 1);

        // Apply it and render again — the report's own words are gone
        // and the document is materially smaller.
        let (mut tree, mut spine, _) = branch_with_a_report();
        for payload in events {
            tree.append(&mut spine, payload).unwrap();
        }
        let doc = render(&tree, &spine, 4096);
        let after = rendered_size(&doc);
        let text: String = doc.messages.iter().map(|m| m.content.clone()).collect();
        assert!(!text.contains("warning: unused"), "result replayed");
        assert!(!text.contains("cargo check --all-targets"), "menu kept");

        // What it freed is the whole report bar the stub that replaces
        // it — not "some bytes", which a rounding change could satisfy.
        // The report is small here *because of 27.2*: a menu row no
        // longer replays its result, so the 3 KB of stdout above was
        // already an `ok, N bytes`. On a real run the menu is the bulk.
        let (fresh, fresh_spine, fresh_ret) = branch_with_a_report();
        let report = crate::report::derive_report(&fresh, fresh_spine.leaf_id, fresh_ret, 4096);
        assert!(
            before - after >= report.len() - 64,
            "freed {} of a {}-byte report",
            before - after,
            report.len()
        );
    }

    /// A `Call` has no line of its own to remove — it is *inside* the
    /// report, and goes when the report goes (the test above). So the
    /// `"event"` fallback refusing it is the honest answer, not a gap:
    /// compacting one would look like a success and free nothing.
    #[test]
    fn a_call_is_not_a_row_because_the_report_around_it_is() {
        let (tree, spine, ret) = branch_with_a_report();
        let call = tree
            .path_events(spine.leaf_id)
            .iter()
            .find(|e| matches!(e.payload, EventPayload::Call(_)))
            .unwrap()
            .id;
        assert_ne!(call, ret);
        assert_eq!(
            compact(
                &tree,
                &spine,
                &[CompactionOp::Remove {
                    id: call,
                    label: "call".into(),
                }],
                4096,
                10_000,
            ),
            Err(CompactionError::NotARow(call))
        );
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
    /// Something structural is refused by name, not quietly accepted.
    ///
    /// A live compaction program opened with
    /// `remove_history(1, "system")` — aiming at the card, which is not
    /// a row, renders nowhere, and could not be made smaller by
    /// compacting it. Accepted, that op would have committed, freed
    /// nothing, and left the model believing it had worked.
    #[test]
    fn a_structural_event_is_not_a_row() {
        let (tree, spine, _, _) = sample_branch();
        let agent = tree
            .enclosing_agent(spine.leaf_id)
            .expect("the branch has an agent");
        let err = compact(
            &tree,
            &spine,
            &[CompactionOp::Remove {
                id: agent,
                label: "event".into(),
            }],
            64 * 1024,
            1,
        )
        .unwrap_err();
        assert!(matches!(err, CompactionError::NotARow(id) if id == agent));
        let said = err.to_string();
        assert!(said.contains("not a row"), "{said}");
        assert!(
            said.contains("fixed"),
            "says what cannot be touched: {said}"
        );
    }
    /// A program's bytes count wherever the transport happens to put
    /// them. Under `RunProgram` they are in the turn's tool call rather
    /// than its content, and a size that missed them would fire
    /// compaction late in that mode only — turning any comparison
    /// between the two transports into a comparison of when each one
    /// compacted.
    #[test]
    fn a_programs_bytes_count_in_either_transport() {
        use crate::document::{ChatMessage, ChatRole, Document, ToolCall};
        let program = "tell(\"x\");".repeat(20);
        let as_text = Document {
            messages: vec![ChatMessage {
                role: ChatRole::Assistant,
                content: program.clone(),
                tool_calls: None,
                tool_call_id: None,
            }],
            preamble: 0,
        };
        let as_call = Document {
            messages: vec![ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "c1".into(),
                    source: program.clone(),
                }]),
                tool_call_id: None,
            }],
            preamble: 0,
        };
        assert_eq!(rendered_size(&as_text), program.len());
        assert_eq!(rendered_size(&as_call), rendered_size(&as_text));
    }
}
