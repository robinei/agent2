`notebook/` — the card for `AGENT2_TRANSPORT=notebook` (phase 25).

**A second card, not a replacement.** `agent/card/` is the control arm and
must not move: an A/B whose baseline drifts measures nothing. This one is
the same card with exactly what the transport changes rewritten, and
nothing else touched — the API declarations, the three dialect
divergences, the "check by changing" and "a piece is not a call"
arguments are all verbatim, so a difference in the numbers is a
difference in the transport rather than in the prose around it.

What is different, and why (`docs/25_NOTEBOOK.md`):

- **The reply is markdown**, and ```js blocks in it run in source order.
  A block fenced any other way is quoted, not run (D3).
- **The blocks share one scope** — one compilation that pauses between
  them, so a `const` declared twice across two blocks is a redeclaration
  error (D12). The card says so, and exemplar 04 demonstrates it by
  binding in one block and using it in the next.
- **There is no `return`** (D5). What crosses to the next reply goes to
  `history.append`, which is what exemplars 02 and 05 now end on.
- **`done()` stops nothing** (D8): it decides that the branch rests once
  the reply ends, and the blocks after it still run. The card carries the
  `if (bail) { … done(); }` counter-example explicitly, because the
  natural reading is wrong and an exemplar written during this phase's
  own design got it wrong.
- **Prose says what you already know; `tell()` says what you just found
  out.** Prose is emitted while the completion streams, before any
  instruction runs, so a finding written above the block that checks it
  is a claim made before its evidence exists.

The five exemplars keep the job each one does in the shipped card —
finish a task and check it, hand on without stopping, offer a bounded
choice, do many at once, keep two rows — which is the part worth
preserving. `agent/src/card.rs`'s own exemplar tests explain what each is
for; they cover the built-in card, so the notebook forms are covered by
`the_notebook_cards_exemplars_split_into_cells_that_compile` instead.

Run it with `--card evals/cards/notebook` and `AGENT2_TRANSPORT=notebook`
together: the card describes a transport, so it measures nothing on its
own.
