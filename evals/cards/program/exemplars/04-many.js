const hits = (await tools.bash("grep -rl OLD .")).stdout.split("\n").filter(Boolean);
const fs = await Promise.all(hits.map((p) => tools.read_file(p)));
for (let i = 0; i < hits.length; i++) {
  const text = Edit.replaceAll(fs[i].content, "OLD", "NEW");
  console.log(`${hits[i]}: ${Edit.count(fs[i].content, "OLD")}`);
  await tools.replace_file(hits[i], text, fs[i].version);
}
tell(`${hits.length} files`);
finish();
