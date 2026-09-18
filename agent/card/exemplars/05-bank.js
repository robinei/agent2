const r = await tools.bash("CHECK 2>&1");
const passing = r.stdout.split("\n").filter((l) => l.endsWith("ok"));
// Banked now: a return only arrives if this program survives.
history.append({ passing });
const f = await tools.read_file("PATH");
await tools.replace_file("PATH", Edit.replaceOnce(f.content, "OLD", "NEW"), f.version);
done();
