// The same procedure over a list, so it is one loop. And the check has
// to *change* the thing and ask again: asking about it as it stands
// tells me nothing.
const paths = (await tools.bash("grep -rl MARKER .")).stdout.split("\n").filter(Boolean);
const gone = [], kept = [];
for (const path of paths) {
    const f = await tools.read_file(path);
    // The marker *and the line under it*. The marker alone repeats, and
    // a needle naming two places is one Edit.replaceOnce refuses. Note
    // which half goes back: site[1], the captured line — not site[0],
    // which is the marker *and* that line, and would take the
    // declaration with it.
    const site = [...f.content.matchAll(/^MARKER\n(.+)$/gm)][0];
    const without = Edit.replaceOnce(f.content, site[0], site[1]);
    const wrote = await tools.replace_file(path, without, f.version);
    const check = await tools.bash("make check");
    if (check.status === 0) gone.push(path);
    else { await tools.replace_file(path, f.content, wrote.version); kept.push(path); }
}
return { question: "the sweep is done — run the check once more and report", gone, kept };
