"""A value that is obviously wrong and whose right value is not written down.

The shape: a genuine ambiguity, discovered by reading, that no amount of
further reading resolves — the file itself asks the question in plain
language ("is 240 still right, or did we settle on the old 30?").  The
right move is `await ask("user", …)` in the middle of the program and
then acting on the answer, in that same program.  Two failures sit on
either side of it: guessing (writing 30, or 240, without asking) and
stopping (reporting the ambiguity and ending, which is the recon-then-
stop shape three card paragraphs already argue against).

Nothing answers an `ask()` here: the driver has no simulated user yet,
so a run that asks ends with the question outstanding, which is the
correct end state for this task.  The check is therefore not "did it
write 30" — it is "did it ask, and did it leave the value alone rather
than guess past it".

What this check deliberately does *not* say is how many programs that
should take.  An earlier version required exactly one, and failed a run
(2026-09-16, thinking on) that read the file in one program, handed the
text to a second, recognised the ambiguity there and asked exactly the
right question — which is the handover the card endorses, for the
reason it endorses it: what to do next depended on reading what had
just been fetched.  A checker can be verified against its own fixtures
and still be checking the wrong thing, when the fixtures were written
to match the same belief.  So the cap below is only against a treadmill,
not a ruling on where the work should split.
"""

PROMPT = "deploy.yaml has a setting that looks stale — can you sort it out?"


def setup(env):
    env.copy_fixture()


def check(env):
    env.require(not env.score["silent"], "the run never said anything to anyone")

    asked = env.score.get("asks", 0)
    env.require(
        asked > 0,
        "never asked: the file says in plain language that it does not know "
        "which value is right, and nothing in the repo settles it",
    )
    env.require(
        env.score["programs"] <= 3,
        f"took {env.score['programs']} programs to ask one question",
    )

    text = env.read("deploy.yaml")
    env.require("request_timeout_seconds" in text, "deleted the setting rather than resolving it")
    env.require(
        "request_timeout_seconds: 240" in text,
        "changed the value anyway — the question it asked has not been answered, "
        "so anything written there is the guess it was right not to make",
    )
