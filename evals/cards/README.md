Card variants, for arms the default card is measured against.

The **default** card — no `--card` flag — is the shipped one,
`agent/card/`: the `.d.ts` plus five exemplars. Everything here exists
to be compared with it.

- `prose-answer/` — the shipped card plus one exemplar that runs
  nothing. See its own README; it is the only variant here currently
  runnable as an arm.

- `program/` and `ts/` — **both predate "one transport: the notebook"
  and are not runnable as baselines.** Each opens by telling the model
  its entire reply is a JavaScript program with no prose and no fences,
  which this harness no longer parses: a run of either would produce a
  reply with no cells in it and measure nothing about the card. Their
  verbs were mechanically updated on 2026-09-20 (`stop(reason)` and
  `finish(text)` are gone from the dialect, and the gate below compiles
  every variant's exemplars), so they *look* current and are not.
  Re-base them on the notebook transport before quoting a number from
  either, or delete them.

  `ts/` is worth re-basing rather than dropping: it is not "the shipped
  card minus exemplars" as this file used to claim, but an independent
  minimal arm with its own argument — override priors only where the
  dialect would silently betray them, and otherwise say nothing. That
  question is still open.

Deleted on 2026-09-20, recoverable from git: `short`, for the same
reason — it taught `append_history`, `return value` and a vocabulary
the harness stopped speaking, and its own entry here already said it
was not runnable as a baseline.

Deleted on 2026-09-17: `mid`, `minimal`, `sketch`, `no-exemplars`,
`short-noex`, `short-rp`, `short-scrappy`. Every one was an arm against
a card that no longer exists — most of them still teaching
`next_program`, a verb deleted in phase 27 — so a run of any of them
would measure a system we do not have. Their results are in
`docs/27_ONE_HISTORY.md`, which is the part worth keeping.
