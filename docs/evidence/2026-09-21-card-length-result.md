# Result: a 31% shorter card changed nothing measurable

Preregistered in `2026-09-21-card-length-prereg.md` before the first
run. 12 runs of `sweep-8`, `deepseek-v4-flash` via opencode,
interleaved, `--jobs 1`. All 12 fingerprints matched their arm.

| | `long` (22,273 B) | `short` (15,255 B) |
|---|---|---|
| **passed** | 6/6 | 6/6 |
| **traps** | 2 | 2 |
| trap kinds | guard, program | program |
| **reasoning KB** (median) | 19.9 | 22.8 |
| programs | 3 | 2.5 |
| output tokens | 6,284 | 6,731 |
| stops | 0 | 0 |

Exact Mann-Whitney, two-sided, n=6/6:

- reasoning KB **p = 0.937**
- output tokens **p = 0.818**
- programs **p = 0.448**

## What that does and does not say

**The prediction held.** The prereg put 60% on "no detectable
difference on any metric", and that is what came back.

**The secondary hypothesis got no support, and not merely for want of
power.** The claim was that an imperative register transfers to the
output and cuts reasoning by 10–25%. The medians are 19.9 against 22.8
— they *crossed*, in the wrong direction, at p = 0.937. An
underpowered test of a real effect looks like a large gap with a
mediocre p-value; this looks like nothing. n=6 still cannot rule out a
modest effect, but it can say the data are not pointing at one.

**The three failure modes I predicted did not appear.** No trap
touched `history.remove`/`replace` timing, replacing a summarised row,
or batch design. All four traps across both arms are ordinary:

- `long-4` — `history.fetch(1)` on an id that is not a row.
- `long-6` — a `replace_file` version conflict. The CAS guard working.
- `short-3` — the model's own thrown assertion on an unexpected file
  body. That is the card's "assert a derivation" rule firing, which is
  the behaviour we want.
- `short-4` — `ReferenceError: helpers_fixed is not defined`, a
  variable expected to survive across replies.

`short-4` is the only one that could be read as a card weakening: the
short card's version of that paragraph dropped the clause contrasting
within-reply scope against across-reply scope (the within-reply half is
stated earlier in the card, so the rule survived; the contrast did
not). **At n=1 that is not evidence**, and it was not on the predicted
list. It is the thing to watch, not a finding.

## The one real effect, which is not behavioural

Per program, median across runs:

| | prompt tokens | uncached |
|---|---|---|
| `long` | 8,863 | 3,007 |
| `short` | 6,956 | 2,654 |

**−21.5% prompt tokens, −12% uncached.** The system prompt is the
immutable cache prefix, so most of what was cut was being paid at the
cached rate — which is why the uncached saving is half the headline
number.

## Conclusion

Keep the short card. It costs a fifth less per program, passes
identically, and traps identically. But it should be kept **because it
is cheaper and no worse**, not because shortening improved behaviour:
on this evidence it did not, and the register hypothesis that motivated
it is unsupported.

The open question the prereg named is still open and needs 15–20 per
arm to close. The cheaper thing to test first is **position**, not
length — see the tail-ordering note: the rehearsal line that won weakly
yesterday sat four lines from the bottom of the request tail, and the
penultimate slot in the whole context is currently spent on
`- A client is attached; an ask() may be answered promptly.` whenever
no work is under way.
