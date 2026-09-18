const f = await tools.read_file("PATH");
const fixed = Edit.replaceOnce(f.content, "OLD", "NEW");
await tools.replace_file("PATH", fixed, f.version);
const check = await tools.bash("CHECK");
tell(check.status === 0 ? "done — CHECK passes." : `changed it, but CHECK fails:\n${check.stdout}`);
done();
