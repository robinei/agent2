The card as a single `.d.ts`: a comment block saying how this works and
what is expected, the declarations, and only the JavaScript we lack that
a model would actually reach for.

5.7KB against `short`'s 9.5KB, and no worked examples.

**The bet.** pi runs on priors — roughly a sentence per tool and nothing
else — and is effective. We spend 14.7KB of reasoning a run against its
3.5KB on the identical model at the identical `reasoning_effort: high`.
That gap is not configuration; it is 8.7KB of card arguing for
deliberation and ten exemplars demonstrating careful, commented
programs. This asks what happens if we override priors only where the
dialect would silently betray them, and otherwise say nothing.

**Why the "missing from JavaScript" list is three items.** Almost
everything we refuse — `for await`, `BigInt`, private fields,
`Promise.race`, getters/setters, labelled break — refuses *loudly*, with
the fix in the message. A card that lists those is paying prose for
something the error already says at the moment it matters. What earns a
line is only where the program runs and gives a wrong answer: UTF-8
`.length`, non-coercing relational operators, and `e instanceof Error`.
One sentence covers the rest.

**What is deliberately absent**: every paragraph of the long card about
handing over, proving a check can fail, not guessing, batching the
experiment. Two of those are measured to matter — asking, and asking
once for everything a check will answer at once. They are gone here on
purpose: this arm prices them.

Measured before it is allowed to become the shipped card.
