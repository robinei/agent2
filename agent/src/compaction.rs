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
//! append — best effort, dropping any op it cannot apply
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


/// The fixed marker a bare `remove_history` leaves in place of
/// content — never empty, so "no row silently emptied" holds by
/// construction rather than needing every caller to remember it.
pub const REMOVED_MARKER: &str = "(removed)";

/// The `EventPayload::Compacted` events a caller should append for
/// `ops`, in `ops`' own order — **best effort**: an op naming something
/// it cannot compact is dropped and the rest apply. Applies nothing
/// itself; never takes a `&mut Tree`.
///
/// It used to be all-or-nothing, and to hand back a `CompactionError`
/// naming the first op it tripped over, which the caller posted to the
/// model. Both halves were wrong. Atomicity bought nothing — the log is
/// untouched either way, so there is no half-applied state to protect
/// against — and it cost everything: in the skipped-tests run of
/// 2026-09-17 each batch named one call among roughly twenty valid
/// removals, and the twenty died with the one, twice, so the session
/// compacted nothing at all while its document grew 15452 -> 18182
/// against a 20000 budget.
///
/// The message was the other half. A refusal the model has to act on is
/// a turn spent on bookkeeping rather than the task, and the one thing
/// it actually needs to know — whether the document got smaller — the
/// next request answers by being smaller.
///
/// There is no threshold check either. A round that frees something and
/// still leaves the document too large is progress, and nothing needs
/// to re-fire on the strength of it: `compaction_if_needed` already
/// asks the only question that matters — is this document over budget
/// *now* — each time a request is built. Refusing such a round threw
/// the progress away and then asked for it again anyway. What still
/// ends the loop is `COMPACTION_ATTEMPTS`: a round freeing *nothing*
/// appends no `Compacted` event, so the attempt counter is not reset
/// and the cap bites.
pub fn compact(tree: &Tree, spine: &Spine, ops: &[CompactionOp]) -> Vec<EventPayload> {
    let path = tree.path_events(spine.leaf_id);
    let mut seen = std::collections::HashSet::new();

    ops.iter()
        .filter(|op| {
            // Each op stands or falls on its own — and it is checked
            // *before* the one-op-per-row rule, so an op that was never
            // going to apply does not consume its row's slot and take a
            // good op on the same row down with it.
            if let CompactionOp::Rewrite { text, .. } = op
                && text.is_empty()
            {
                return false;
            }
            let Some(target) = path.iter().find(|e| e.id == op.id()) else {
                return false;
            };
            // "event" is `label_of`'s "no name for this kind of row":
            // the card, a worked example, an `Agent`, or a `Call`
            // rendered inside some `Return`'s report rather than as a
            // line of its own. None can be made smaller this way.
            let expected = label_of(&target.payload);
            if expected == "event" || expected != op.label() {
                return false;
            }
            seen.insert(op.id())
        })
        .cloned()
        .map(CompactionOp::into_payload)
        .collect()
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
    use crate::document::{Document, Transport};

    /// These tests are about *sizes and ops* — what compaction removes
    /// and when it fires — not about wire containers, so they render
    /// under one transport throughout and say so once here rather than
    /// at every call site. The one size question that *is* transport-
    /// sensitive has its own test,
    /// `a_programs_bytes_count_in_either_transport`, which builds both
    /// shapes by hand.
    fn render(tree: &Tree, spine: &Spine, budget: usize) -> Document {
        crate::document::render(tree, spine, budget, Transport::Program)
    }

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
        let events = compact(&tree, &spine, &ops);
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
    /// report, and goes when the report goes (the test above). So it is
    /// skipped. What this really pins is that skipping it costs nothing
    /// else in the batch: the `return` named beside it still applies.
    #[test]
    fn a_call_is_skipped_but_the_rest_of_the_batch_applies() {
        let (tree, spine, ret) = branch_with_a_report();
        let call = tree
            .path_events(spine.leaf_id)
            .iter()
            .find(|e| matches!(e.payload, EventPayload::Call(_)))
            .unwrap()
            .id;
        assert_ne!(call, ret);
        let payloads = compact(
            &tree,
            &spine,
            &[
                CompactionOp::Remove {
                    id: call,
                    label: "call".into(),
                },
                CompactionOp::Remove {
                    id: ret,
                    label: "return".into(),
                },
            ],
        );
        assert_eq!(payloads.len(), 1, "only the call is dropped: {payloads:?}");
        assert!(
            matches!(&payloads[0], EventPayload::Compacted { of, .. } if *of == ret),
            "{payloads:?}"
        );
    }

    /// Every way a single op can be bad, each paired with a good one:
    /// the bad op is dropped in silence and the good one still applies.
    /// Atomicity used to mean the good one died too — twenty of them at
    /// a time, in the run that prompted this.
    #[test]
    fn a_bad_op_is_skipped_and_never_takes_a_good_one_with_it() {
        let good = |note| CompactionOp::Remove {
            id: note,
            label: "note".into(),
        };
        let cases: Vec<(&str, fn(EventId) -> CompactionOp)> = vec![
            ("an id that is not on this path", |_| CompactionOp::Remove {
                id: EventId::new(9999),
                label: "note".into(),
            }),
            ("a real id under the wrong label", |note| {
                CompactionOp::Remove {
                    id: note,
                    label: "turn".into(),
                }
            }),
            ("a rewrite that would empty the row", |note| {
                CompactionOp::Rewrite {
                    id: note,
                    label: "note".into(),
                    text: String::new(),
                }
            }),
        ];
        for (what, bad) in cases {
            let (tree, spine, _program, note) = sample_branch();
            let payloads = compact(&tree, &spine, &[bad(note), good(note)]);
            assert_eq!(payloads.len(), 1, "{what}: {payloads:?}");
            assert!(
                matches!(&payloads[0], EventPayload::Compacted { of, .. } if *of == note),
                "{what}: {payloads:?}"
            );
        }
    }

    /// One op per row still holds — the second op on an id is dropped
    /// rather than overwriting the first, so "no row gets two
    /// operations" survives the move to best effort.
    #[test]
    fn the_second_op_on_one_id_is_dropped_and_the_first_stands() {
        let (tree, spine, _program, note) = sample_branch();
        let payloads = compact(
            &tree,
            &spine,
            &[
                CompactionOp::Remove {
                    id: note,
                    label: "note".into(),
                },
                CompactionOp::Rewrite {
                    id: note,
                    label: "note".into(),
                    text: "second op on the same id".into(),
                },
            ],
        );
        assert_eq!(payloads.len(), 1, "{payloads:?}");
        assert!(
            matches!(&payloads[0], EventPayload::Compacted { text, .. } if text.is_none()),
            "the first op wins, and it was a removal: {payloads:?}"
        );
    }

    #[test]
    fn a_valid_batch_returns_compacted_events_that_shadow_every_target() {
        let (tree, spine, _program, note) = sample_branch();
        let ops = [CompactionOp::Remove {
            id: note,
            label: "note".into(),
        }];
        let payloads = compact(&tree, &spine, &ops);
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
        let payloads = compact(&tree, &spine, &ops);
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
        let payloads = compact(&tree, &spine, &ops);
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
    /// Something structural is dropped, never quietly accepted.
    ///
    /// A live compaction program opened with
    /// `remove_history(1, "system")` — aiming at the card, which is not
    /// a row, renders nowhere, and could not be made smaller by
    /// compacting it. Applied, that op would have committed, freed
    /// nothing, and left the model believing it had worked.
    #[test]
    fn a_structural_event_is_not_a_row() {
        let (tree, spine, _, note) = sample_branch();
        let agent = tree
            .enclosing_agent(spine.leaf_id)
            .expect("the branch has an agent");
        let payloads = compact(
            &tree,
            &spine,
            &[
                CompactionOp::Remove {
                    id: agent,
                    label: "event".into(),
                },
                CompactionOp::Remove {
                    id: note,
                    label: "note".into(),
                },
            ],
        );
        assert_eq!(payloads.len(), 1, "the card is not compactable: {payloads:?}");
        assert!(
            matches!(&payloads[0], EventPayload::Compacted { of, .. } if *of == note),
            "{payloads:?}"
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
            transport: Transport::Program,
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
            transport: Transport::Program,
        };
        assert_eq!(rendered_size(&as_text), program.len());
        assert_eq!(rendered_size(&as_call), rendered_size(&as_text));
    }
}
