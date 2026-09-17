The card as a `.d.ts` — one sentence of mechanism, the declarations, and
only the JavaScript we lack that a model would actually reach for.

5.2KB against `short`'s 9.5KB, and no worked examples. The bet is that a
type declaration with a one-line doc comment is the most familiar
possible way to hand an API to a model trained on TypeScript, and that
familiarity is worth more than prose arguing for good judgement — the
argument being that pi is effective on a system prompt of roughly one
sentence per tool, and that we spend 14.7KB of reasoning a run against
its 3.5KB on the identical model at the identical effort.

What is deliberately *not* here: every paragraph of the long card about
handing over, proving a check can fail, not guessing, reading before
writing. Those are what this arm is testing the price of.

The dialect section lists only deltas with a measured trap behind them
(UTF-8 byte length, no cross-type comparison coercion, plain error
objects, no executor pattern). Everything else JavaScript has either
works or stops the program and says so, and listing it would be paying
prose for silence.

**Signatures track the code, not the intent.** Writing them down found
that the verbs were in three dispatch groups behind one uniform-looking
table, and that `raise` was never a promise despite every example in the
long card putting an `await` in front of it.

That is settled now: since the settle-at-dispatch change, **`ask` is the
only verb that returns a promise.** Everything else hands back the
value. `tools.*` are all async.

The failure here is one-sided, which is why the prose card got away with
saying nothing: awaiting a plain value is the identity, so declaring
`Promise` where there is none costs nothing, while omitting it where
there is one hands the model a promise it never unwraps. Stated in the
card so a reader can act on it.
