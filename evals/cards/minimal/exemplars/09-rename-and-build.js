tell("finding every caller, updating each one, then building to check I didn't break anything.");
const hits = (await tools.bash("grep -rl 'parse_span(' --include='*.rs' . 2>/dev/null")).stdout.split("\n").map(s => s.trim()).filter(Boolean);

// I know what to do with each one, so I do it here. Nothing about this
// loop needs a reading: the edit is the same everywhere and the
// compiler will tell me whether it worked.
const changed = [];
for (const path of hits) {
    const f = await tools.read_file(path);
    const next = f.content.replaceAll("parse_span(", "read_span(");
    if (next === f.content) continue;
    await tools.replace_file(path, next, f.version);
    changed.push(path);
}

const build = await tools.bash("cargo build 2>&1 | tail -40");
if (build.status === 0) {
    tell(`done — ${changed.length} file(s) updated, and it builds.`);
    done();
} else {
    // Now it is a reading: which of these errors are mine, and what the
    // fix is, cannot be worked out by more string handling. Return what
    // I did and what broke, and the next program fixes it.
    return {
        question: "the rename is applied but the build fails — read these errors, decide which are from the rename, and fix them",
        changed,
        errors: build.stdout,
    };
}
