The shipped card plus **one exemplar that runs nothing**.

`card.md` says it already:

> **A reply with no code blocks in it rests the branch.** … That is the
> right shape for answering a question.

and the five shipped exemplars say the opposite by example — every one
of them opens with a ```js block. `evals/spoke_in_code.py` is what that
costs: across 320 kept logs, 25% of replies that ran a program had that
program make no tool call at all, and in the one conversation in the
corpus it was 5 of 6.

So `06-answer` is a question whose answer is already known, answered in
prose, with nothing run. Everything else is byte-for-byte the shipped
card, so an arm against the default isolates the exemplar.

The other arm on the same question is `AGENT2_REPLY_SHAPE_TAIL=1`,
which says it in the request's **tail** instead — see
`machine.rs`'s `REPLY_SHAPE_TAIL`. The two are worth running apart:
the card's last line sits ~11 KB from the end of a short request and
~28 KB from the end of a long one, and the tail is the end.
