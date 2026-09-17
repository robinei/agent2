// The same procedure over a list, so it is one loop. And the check has
// to *change* the thing and ask again: asking about it as it stands
// tells me nothing.
const f = await tools.read_file("src/util.js");
let content = f.content, version = f.version;

// The marker *and the line under it*. The marker alone repeats, and a
// needle naming two places is one Edit.replaceOnce refuses. Note which
// half goes back: site[1], the captured `export function` line — not
// site[0], which is the marker *and* that line, and would take the
// function's signature with it.
const sites = [...content.matchAll(/^\/\/ @deprecated[^\n]*\n(export function (\w+))/gm)];
const gone = [], kept = [];
for (const site of sites) {
    const without = Edit.replaceOnce(content, site[0], site[1]);
    const wrote = await tools.replace_file("src/util.js", without, version);
    const check = await tools.bash("npm test 2>&1 | tail -5");
    if (check.status === 0) {
        content = without;
        version = wrote.version;
        gone.push(site[2]);
    } else {
        version = (await tools.replace_file("src/util.js", content, wrote.version)).version;
        kept.push(site[2]);
    }
}
return { question: "the sweep is done — run the suite once more and report", gone, kept };
