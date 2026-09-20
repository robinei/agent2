Card variants, for arms the default card is measured against.

The **default** card — no `--card` flag — is the shipped one,
`agent/card/`: the `.d.ts` plus two exemplars. Everything here exists to
be compared with it.

- `notebook/` — the markdown card for `AGENT2_TRANSPORT=notebook`:
  prose at the top level, the API in one `ts` block, the tools appended
  in a fence of their own. The shipped card stays a TypeScript document
  because its own first line says the reply is JavaScript and nothing
  else; which framing the generated manifest takes is read off the
  card's first bytes (`card.rs`'s `full_card`).

- `ts/` — the shipped card with the two exemplars removed. The live
  ablation: exemplars were measured at six runs in fifteen once, and
  that number should be re-taken whenever the card changes shape.

Deleted on 2026-09-20, recoverable from git: `short` (it taught `stop(reason)` and `finish(text)`, a vocabulary the harness stopped speaking, and its own entry here had already said it was not runnable as a baseline). Deleted on 2026-09-17: `mid`, `minimal`,
`sketch`, `no-exemplars`, `short-noex`, `short-rp`, `short-scrappy`.
Every one was an arm against a card that no longer exists — most of
them still teaching `next_program`, a verb deleted in phase 27 — so a
run of any of them would measure a system we do not have. Their results
are in `docs/27_ONE_HISTORY.md`, which is the part worth keeping.
