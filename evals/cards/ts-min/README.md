`ts`, plus the two smallest exemplars that can exist.

**Prediction, recorded before the run**: some minimal demonstration of
the format and the contract will beat none. Robin's, 2026-09-17.

The reasoning behind it, which is why this is a separate arm rather than
a hunch folded into `ts`: exemplars were doing two jobs, and only one is
unique to them.

*Teaching the API* a `.d.ts` does better — that is the whole bet of the
`ts` card, and TSDoc `@example` covers any residue inside the
declarations.

*Demonstrating the response format* only an assistant-role message can
do. `card.rs` has always said so: turn one has nothing else to imitate.
And it still bites — live programs on 2026-09-17 opened with "the
previous response ran off the end with a stray tag after done()" and
"the last program died on a comma inside prose".

So these two carry format and contract and nothing else: the whole reply
is bare JavaScript, one ends on `done()` because its task is finished,
the other ends on `return` because it is not, and what it returns is
what the next program sees. No comments, no judgement, no technique, and
nothing task-shaped enough to be copied — which is the failure the
careful exemplars produced this morning when a run reproduced one
verbatim into an unrelated repo.
