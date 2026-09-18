const f = await tools.read_file("PATH");
// The file itself says nobody knows which value is right, and nothing
// else in the repo settles it — so it is not mine to guess.
const want = await ask("user", "QUESTION — A, or B?");
await tools.replace_file("PATH", Edit.replaceOnce(f.content, "OLD", want), f.version);
tell(`set to ${want}.`);
done();
