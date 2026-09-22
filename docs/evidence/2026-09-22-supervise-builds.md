# A task where waiting is expensive

Nine modules, one `check.sh` that sleeps six seconds each. Serial is
54 s of wall clock nothing can compress; fanned out it is ~6 s. Three
runs each, `deepseek-v4-flash` and `gpt-5.6-sol`.

| | local exec | strategy |
|---|---|---|
| flash | 64, 41, 64 s | awaits each check in turn |
| `gpt-5.6-sol` | 14, 44, 18 s | **`Promise.all` in 3 of 3** |

`sol` wrote the textbook shape unprompted:

```js
const results = await Promise.all(modules.map(async (module) => {
  const run = await tools.bash(`./check.sh ${JSON.stringify(module)} 2>&1`);
  return { module, status: run.status, output: run.stdout };
}));
```

flash never did, in any run. One flash run worked the cost out in
prose — *"the check sleeps 6s per module, so nine in sequence would
take 54s — that's why the sequential loop timed out"* — and then
reached for shell job control rather than the language it was writing
in.

**This is the code-mode advantage as arithmetic rather than style.** A
tool loop cannot write that program: it can call nine tools in
parallel if its API allows, but it cannot compute over the nine
results in the same turn, and it cannot decide the width of the
fan-out from a `find` it ran a line earlier.

## `spawn` is still at zero, and that is the finding

The task deliberately accepts `Promise.all` as a full answer — the
alternative was to require delegation and measure compliance with a
verb. Both models that fanned out used tools, not helpers.

So after a task built to make waiting expensive, `spawn`/`fork`/
`list_agents` remain unused. The honest reading: **parallelism alone
does not call for a second agent.** Delegation earns its place only
when the sub-work needs its own *context* — which is what
`delegate-notes` creates with a 4 KB window, and the one place
`spawn` has ever been used. Time pressure is answered by
`Promise.all`; context pressure is what needs another mind.

## The checker was wrong before the models were

Five of six runs failed on the first pass, four with an identical
complaint. The cause was mine: `wrongly` scanned **per message**
instead of per line, so a correct answer that lists every module and
its verdict —

    Ran ./check.sh on all 9 modules: 7 pass, 2 fail.
    - alpha: PASS
    - beta: FAIL (exit 1) — unresolved symbol 'frobnicate'

— put `alpha` and the word `fail` in one string and was rejected. The
docstring claimed sentence-level; the code did not do it. That reply
is now a fixture: **a real answer a checker got wrong is the best
fixture there is.**

A checker that greps too widely fails good runs as surely as one that
greps too narrowly passes bad ones, and only the second is the failure
`drive.py`'s fixture rule was written for.

## The driver change this needed

`--timeout` was one number for the whole suite, so a task built around
waiting looked like a task that hung. A task now declares `TIMEOUT`
and the flag still wins when raised above it.
