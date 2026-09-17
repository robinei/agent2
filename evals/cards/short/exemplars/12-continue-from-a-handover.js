// What I returned came back to me as text I have read — not as
// variables. `hits` was a name in the program before this one and went
// with its VM, so the three names are written out here, from the report
// above.
tell("checking each one for callers, then dropping the ones with none.");
const names = ["formatLegacyDate", "parseOldHeader", "toCamelDeep"];
const dropped = [], kept = [];
for (const name of names) {
    // grep exits 1 when it matches nothing, and nothing is the answer
    // that makes this one droppable.
    const callers = await tools.bash(`grep -rln "${name}" src/ test/ | grep -v src/util.js`);
    if (callers.status === 0) { kept.push(name); continue; }
    const f = await tools.read_file("src/util.js");
    const block = Edit.extractBlock(f.content, f.content.indexOf(`export function ${name}`));
    await tools.replace_file("src/util.js", Edit.replaceLines(f.content, block.start, block.end, ""), f.version);
    dropped.push(name);
}
tell(`dropped ${dropped.length}, kept ${kept.length} that are still called.`);
done();
