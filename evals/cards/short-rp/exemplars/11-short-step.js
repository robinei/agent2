// One step: find them. What to do next depends on what is there, and
// this program cannot read what it fetches.
const hits = (await tools.bash("grep -rn '@deprecated' src/")).stdout.split("\n").filter(Boolean);
return { question: "for each of these, check whether anything still calls it and drop the ones nothing does", hits };
