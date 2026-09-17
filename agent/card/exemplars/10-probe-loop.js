tell("taking the #[ignore] off each one in turn and running it, so the test runner decides rather than me.");
const paths = (await tools.bash("grep -rl '#\\[ignore\\]' --include='*.rs' . 2>/dev/null")).stdout
    .split("\n").map(s => s.trim()).filter(Boolean);

// The control, inline: if the suite is already red, a failure after
// un-ignoring something is not attributable to that test, and the whole
// loop below would be measuring nothing.
const base = await tools.bash("set -o pipefail; cargo test 2>&1 | tail -5");
if (base.status !== 0) {
    // Blocked is not done. Returning ends this program and starts the
    // next one with this in front of it — which is what gets past the
    // obstacle. Stopping here would leave the task undone.
    return {
        question: "the suite is already failing, so un-ignoring can't be attributed — read these, decide whether they're worth fixing first",
        failures: base.stdout,
    };
}

// I knew this procedure before I knew the list, so it is a loop, not a
// handover and not a program per test. And the check has to *change*
// the code and ask again: leaving the marker on and running the suite
// tells me nothing, because an ignored test is exactly the one that
// doesn't run.
const freed = [], kept = [];
for (const path of paths) {
    let file = await tools.read_file(path);
    // The marker plus the fn line under it. `#[ignore]` on its own
    // repeats within a file; the pair names one test — and it is text I
    // am holding, not a line number that goes stale the moment I edit.
    const sites = [...file.content.matchAll(/^[ \t]*#\[ignore\][ \t]*\n([ \t]*(?:pub )?(?:async )?fn (\w+))/gm)];

    for (const site of sites) {
        const name = site[2];
        // Fails loudly if that text does not pick out exactly one place,
        // so the edit either lands where I meant or does not happen.
        const stripped = Edit.replaceOnce(file.content, site[0], site[1]);
        const wrote = await tools.replace_file(path, stripped, file.version);

        // pipefail, or the status is `tail`'s and `tail` always succeeds.
        // And a 0 status is not yet an answer: a filter that matches
        // nothing exits 0 too, as does a run where the test was still
        // ignored. Make the runner say it ran one and it passed —
        // otherwise this check has no way to tell me no.
        const run = await tools.bash(`set -o pipefail; cargo test ${name} -- --exact 2>&1 | tail -5`);
        if (run.status === 0 && /1 passed/.test(run.stdout)) {
            freed.push(name);
            file = { content: stripped, version: wrote.version };
        } else {
            // put it back — the write handed me the version it produced,
            // so there is nothing to re-read
            const back = await tools.replace_file(path, file.content, wrote.version);
            file = { content: file.content, version: back.version };
            kept.push(name);
        }
    }
}

tell(freed.length
    ? `un-ignored ${freed.length}: ${freed.join(", ")}. ${kept.length} still fail and keep the marker.`
    : `none of the ${kept.length} pass yet — every marker stays.`);
done();