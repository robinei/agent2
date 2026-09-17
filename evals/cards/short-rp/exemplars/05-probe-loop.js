// The same procedure over a list, so it is one loop — but only this
// piece of the task. What comes after depends on how it went.
const sites = (await tools.bash("grep -rn MARKER .")).stdout.split("\n").filter(Boolean);
const gone = [], kept = [];
for (const site of sites) {
    const f = await tools.read_file(path(site));
    const w = await tools.replace_file(path(site), Edit.replaceOnce(f.content, needleFor(site), ""), f.version);
    const ok = await tools.bash("set -o pipefail; cargo check 2>&1 | tail -5");
    if (ok.status === 0) gone.push(site);
    else { await tools.replace_file(path(site), f.content, w.version); kept.push(site); }
}
return { question: "the sweep is done — check the tree still builds and report", gone, kept };
