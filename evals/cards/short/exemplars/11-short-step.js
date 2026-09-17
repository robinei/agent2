// One step: find them. What to do next depends on what is there, and
// this program cannot read what it fetches.
tell("finding the skipped tests.");
const hits = (await tools.bash("grep -rn '@unittest.skip' .")).stdout.split("\n").filter(Boolean);
return { question: "un-skip each of these in turn and run it; keep the marker on the ones that still fail", hits };
