# A fork inherits the role, and the first message is what overrides it

`spawn` takes a charter. `fork` cannot — it inherits a context instead
of being given one. The question this answers: does that context carry
the *role* along with the knowledge, and if so, what undoes it?

The harness already tells a fork that it is a fork. Rendered at the end
of its first document (`report::render_fork`):

```
`[2]` user told you: <the parent's last instruction>

[harness] fork of branch #1 at #2 — questions before this line are
being handled there; do not redo its work unless asked.
```

Two signals that pull against each other. The inherited post is
rendered **"user told you"** — the person's words addressed to *it* —
and the line below says do not redo the work those words asked for.
Nothing says what it is now for.

## Three arms, three runs each

`deepseek-v4-flash`. Fixture: two small Python files with the same
retry loop written out three times. In every arm the fork's first and
only message carries the same eleven words of work; the arms differ
only in what the *parent* context holds, and in arm C by one sentence
prepended to that message.

> *Factor the retry loop into one helper and update all three call sites.*

| | parent context | delegated | did it itself |
|---|---|---|---|
| **A** | no standing role | **0 / 3** | 3 / 3 |
| **B** | *"you never edit files yourself, you always delegate"* | **3 / 3** | 0 / 3 |
| **C** | same as B, plus the sentence below | **0 / 3** | 3 / 3 |

Arm C's sentence, the only difference from B:

> *You are no longer the manager — you are a worker I forked off to do
> this one job yourself, and the manager is still talking to me on the
> other branch.*

## What the forks said

The separation is clean, but the model's own words are the finding.

**Arm B**, inheriting the role and acting on it:

> *"Delegating the edit, **as agreed** — I don't touch files."*
>
> *"**Standing order holds**: a helper makes the edit."*
>
> *"Right — from here on I keep my hands off the files. This goes to a
> helper."*

"As agreed" and "standing order" are agreements the fork was never
party to, treated as binding on itself — and acted on, by writing a
charter for an editor rather than doing eleven words of work.

**Arm C**, same parent, told its role:

> *"I'm the worker on this job, so I do the edit myself — **the
> no-editing rule was Robin's instruction to the manager, not to me**."*
>
> *"**Role noted**, and worth saying plainly since the branch I came
> from recorded the opposite rule …"*

Both name the inherited rule explicitly and set it aside. That is the
same rule arm B obeyed without noticing it had been addressed to
someone else.

**Arm A** shows the inheritance is real rather than a default: with no
role in the parent, the same model on the same task just does it
(*"Taking it."*), or reads the files and asks a design question. Never
a helper.

## What went into the card

`fork()`'s doc comment, beside the `name?` parameter (`483377f`):

> **Its first message is its charter.** There is no charter slot: a
> fork arrives holding everything you hold, and is told only that it is
> a fork and that the branch it came from is handling what went before.
> What it is *now* — whether it does this itself or hands it on, and
> how far it may reach — is whatever you say first. Say it in the same
> breath as the work, because it has your context and will otherwise
> carry on being you.

## In passing

Arm B's fork wrote its helper's charter with a role in it, unprompted:

> *"You are a one-shot editor for a small Python repo (cwd). Do the
> work yourself — do not spawn or fork anyone."*

That is the `spawn` half of the same convention, arrived at without
being told, and the reason `spawn` needed no `role` argument beyond
its charter (`10626b3`).

## Caveats

Three runs per arm, one model. `gpt-5.6-luna` was to be the
cross-model check and its endpoint returned `503 Upstream request
failed` throughout; `deepseek-v4-flash` on the same key was fine, so
this is one model's behaviour, not two.

B3's parent phase was cut short by the driver's timeout, so its fork
began from an interrupted program and a stray open `ask`. Its first
move was still to hand the editing to a helper, but it is the least
clean of the nine.

The classification is the fork's **first move** — delegate or do. Both
are legitimate answers to the prompt in the abstract; what the arms
measure is which one the inherited context makes it pick.

## How it was run

```
agent session --real --headless --turn '<manager turn>'      LOG
agent session --headless --fork <leaf> --name untold         LOG
agent session --real --headless --resume <fork> --turn '…'   LOG
agent transcript LOG <fork>
```

The third line did not work before `dcd0ea9`: `--turn` addressed
`conversation_branch()` regardless of `--resume`, and a fork of the
conversation branch belongs to the same agent, so every turn landed on
the original and was reported there. There was no way to speak to a
fork from the CLI, which is why this had not been looked at.

Two further traps, both hit here first:

- **Scoring must not open a session.** `session --list-branches` builds
  a `Session`, which reconciles, which can spend a completion and
  append to the log being measured. The first run of this had
  scripted-LLM events written into it that way. `agent transcript` and
  reading the `.jsonl` are the read-only routes.
- **`--stdin` closes on EOF before the turn completes.** `echo … |
  agent session --stdin` prints `--- quiet` and exits having written no
  program. `--turn` is the flag for one scripted turn.
