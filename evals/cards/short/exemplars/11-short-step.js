// One step: find them. What to do next depends on what is there, and
// this program cannot read what it fetches.
tell("finding the marked sites.");
const hits = (await tools.bash("grep -rn MARKER .")).stdout.split("\n").filter(Boolean);
return { question: "decide which of these still matter and deal with those", hits };
