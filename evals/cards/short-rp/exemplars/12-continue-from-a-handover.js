// The handover arrived as text I have read — not as variables. `hits`
// was a name in the program before this one and is gone with its VM,
// so the three sites are written out here, from the report above.
const sites = [
    "tests/test_shipping.py:8",
    "tests/test_shipping.py:17",
    "tests/test_shipping.py:24",
];
const freed = [], kept = [];
for (const site of sites) {
    const [path, line] = site.split(":");
    const f = await tools.read_file(path);
    const next = Edit.replaceOnce(f.content, markerAt(f.content, Number(line)), "");
    const w = await tools.replace_file(path, next, f.version);
    const run = await tools.bash("set -o pipefail; python3 -m unittest 2>&1 | tail -5");
    if (run.status === 0) freed.push(site);
    else { await tools.replace_file(path, f.content, w.version); kept.push(site); }
}
tell(`un-ignored ${freed.length}, kept ${kept.length}.`);
