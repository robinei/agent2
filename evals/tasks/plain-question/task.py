"""A question with no work in it.

Every other task in this suite rewards more machinery, and every card
sentence added this week pushes that way: write the loop, run a
control, hand over rather than stop.  This one guards the other ditch.
The failure it names is a program that greps the filesystem for
something nobody asked about, or hands itself a to-do list to answer a
question it could already answer.

(This used to quote the card back — "a question that needs no tools is
a two-line program that `tell()`s the answer".  That sentence is not in
the card any more, and what replaced it says the opposite: a reply with
no code blocks is the right shape for answering a question.  The task
was still grading against the deleted rule.)

So the check is about the *shape* of the run, not about files: one
program, no handover, no tool calls, and an answer that actually
engages with the question rather than offering to go and look.

**And whether it spent a program saying it.**  `programs` counts
completions, not cells, so a run that answers in prose and one that
answers by running `tell("…"); finish("…")` score identically on every
gate below — and the second is the shape the card calls wrong ("a reply
with no code blocks in it rests the branch… the right shape for
answering a question").  `cells` is the number that tells them apart,
and it is graded rather than gated: a program that answers correctly is
not a failure, it is a round trip spent on nothing.  See
`evals/spoke_in_code.py` for the corpus-wide number and
`evals/cards/prose-answer/` for the arm that tries to move it.

There is no fixture directory: the sandbox starts empty, which is part
of the task — there is nothing here to read, and a program that goes
looking anyway has misread what was asked.
"""

PROMPT = (
    "Quick question, no need to look anything up: what's the practical difference "
    "between a mutex and a semaphore?"
)


def setup(env):
    pass


def check(env):
    # Graded before the gates: spoke, needed no tools, took one program.
    env.credit(
        sum(
            [
                not env.score["silent"],
                env.score["tool_calls"] == 0,
                env.score["programs"] == 1,
            ]
        ),
        3,
        "spoke / no tools / one program",
    )

    # **Did it answer, or run a program that answers?**  Not a gate —
    # both are correct answers and only one of them is wasteful.
    cells = len(env.score.get("program_lengths", []))
    env.credit(1 if cells == 0 else 0, 1, "answered in prose, ran nothing")

    env.require(not env.score["silent"], "the run never said anything to anyone")
    env.require(
        env.score["tool_calls"] == 0,
        f"went looking for something: {env.score['tool_calls']} tool call(s) for a "
        "question that needed none",
    )
    # Under automatic continuation (27.1) a second program *is* the
    # handover: the only way to stop is `finish()`, so "handed itself a
    # to-do list" and "took more than one program" became the same
    # claim, and the separate `handovers` count went with `next_program`.
    env.require(
        env.score["programs"] == 1,
        f"took {env.score['programs']} programs over a question with no work in it",
    )
    # Engaging with the question, not offering to: both concepts named,
    # and the distinction that matters (ownership / a count above one)
    # actually drawn.
    env.require(
        env.said("mutex") and env.said("semaphore"),
        "the answer never names both things it was asked to compare",
    )
    env.require(
        any(
            env.said(word)
            for word in ("owner", "ownership", "count", "counting", "permits", "one at a time")
        ),
        "named both and drew no distinction between them",
    )
