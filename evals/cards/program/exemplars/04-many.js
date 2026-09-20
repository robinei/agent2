const hits = (await tools.bash("grep -rl OLD .")).stdout.split("\n").filter(Boolean);
const fs = await Promise.all(hits.map((p) => tools.read_file(p)));
for (let i = 0; i < hits.length; i++) {
  const e = Edit.replaceCount(fs[i].content, "OLD", "NEW");
  console.log(`${hits[i]}: ${e.count}`);
  await tools.replace_file(hits[i], e.result, fs[i].version);
}
finish(`${hits.length} files`);
