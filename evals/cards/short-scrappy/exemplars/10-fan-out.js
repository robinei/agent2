const names = (await tools.bash("ls reports/")).stdout.split("\n").filter(Boolean);
const helper = spawn("You summarise files. One sentence each.");
const out = [];
for (const n of names) out.push(await ask(helper, (await tools.read_file(`reports/${n}`)).content));
tell(names.map((n, i) => `${n}: ${out[i]}`).join("\n"));
done();
