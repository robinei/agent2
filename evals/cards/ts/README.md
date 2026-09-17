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

**Signatures track the code, not the intent**, and writing them down
found that the verbs are in three groups rather than one:

- `spawn`, `fork`, `done`, `remove_history`, `rewrite_history` — the
  compiler emits the `Await` itself, so the expression is already the
  value. Never a promise, today or after.
- `raise` — `Instr::Raise`, and the host pushes the resumed value
  straight onto the stack. Also never a promise, though the long card
  has always written `await raise(...)`.
- `ask`, `answer`, `append_history`, `fetch_history`, `list_agents` —
  plain `Invoke`, no `Await`. Genuinely promises **today**; all but
  `ask` stop being so the moment the settle-at-dispatch change lands,
  and this file changes in that same commit or the card lies.

Awaiting a plain value is the identity here, so the failure is
one-sided: a declaration that says `Promise` when it is not costs
nothing, and one that omits `Promise` when it is would hand the model a
promise it never unwraps. The prose card, which says nothing either way
and relies on every example writing `await`, has been getting this for
free.
