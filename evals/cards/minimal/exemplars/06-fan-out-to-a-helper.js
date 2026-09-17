tell("reading reports/ — twelve files, so I'll hand the summarising to a helper rather than do it in one long pass here.");
const names = (await tools.bash("ls reports/")).stdout.trim().split("\n");

// Twelve summaries is a lot of judgement and none of it needs my
// history, so it goes to a clean room. spawn() hands back a handle,
// and that handle is what ask() addresses. It only creates the agent —
// it sits idle until asked, so the ask is what actually sets it working,
// and awaiting it is what brings the answer back here.
const helper = spawn("You summarise files. One sentence each, concrete, no preamble.");

const summaries = [];
for (const name of names) {
    const file = await tools.read_file(`reports/${name}`);
    summaries.push(await ask(helper, `One sentence on what this is about:\n\n${file.content}`));
}

tell(names.map((n, i) => `${n}: ${summaries[i]}`).join("\n"));
done();
