"""A question with no work in it.

Every other task in this suite rewards more machinery, and every card
sentence added this week pushes that way: write the loop, run a
control, hand over rather than stop.  This one guards the other ditch.
The card's own words — "a question that needs no tools is a two-line
program that `tell()`s the answer" — and the failure it names is a
program that greps the filesystem for something nobody asked about, or
hands itself a to-do list to answer a question it could already answer.

So the check is about the *shape* of the run, not about files: one
program, no handover, no tool calls, and an answer that actually
engages with the question rather than offering to go and look.

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
    env.require(not env.score["silent"], "the run never said anything to anyone")
    env.require(
        env.score["tool_calls"] == 0,
        f"went looking for something: {env.score['tool_calls']} tool call(s) for a "
        "question that needed none",
    )
    env.require(
        env.score["handovers"] == 0,
        "handed itself a to-do list for a question it could already answer",
    )
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
