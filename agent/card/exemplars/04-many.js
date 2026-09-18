const hits = (await tools.bash("grep -rl OLD .")).stdout.split("\n").filter(Boolean);
const fs = await Promise.all(hits.map((p) => tools.read_file(p)));
for (let i = 0; i < hits.length; i++) {
  const f = fs[i];
  await tools.replace_file(hits[i], Edit.replaceCount(f.content, "OLD", "NEW").result, f.version);
}
tell(`${hits.length} files; CHECK ${(await tools.bash("CHECK")).status}`);
done();
