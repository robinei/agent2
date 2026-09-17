tell("looking at what's here, then I'll read the parts that actually say what this is.");

// Which files answer this is a judgement, and I cannot make it: I would
// have to read them to know, and I cannot read anything this program
// fetches. So gather the cheap thing — the names — and return those.
// Whoever writes next has read this list and can pick the two or three
// worth opening, instead of me globbing *.md and hoping. No `done()`:
// the task is not finished, so returning is exactly right.
const layout = (await tools.bash("ls -1A")).stdout.split("\n").map(s => s.trim()).filter(Boolean);
const docs = (await tools.bash("find . -maxdepth 2 -iname '*.md' -o -maxdepth 2 -iname 'Cargo.toml' -o -maxdepth 2 -iname 'package.json' 2>/dev/null | head -40")).stdout;

return {
    question: "what is this project? — pick the few files that would actually say, read those, and answer",
    layout,
    candidates: docs.split("\n").map(s => s.trim()).filter(Boolean),
};
