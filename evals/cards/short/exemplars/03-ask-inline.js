const f = await tools.read_file("ops/config.json");
const cfg = JSON.parse(f.content);
const want = await ask("user", `retries is ${cfg.retries} — what should it be?`);
cfg.retries = Number(want);
await tools.replace_file("ops/config.json", JSON.stringify(cfg, null, 2), f.version);
tell(`set retries to ${want}.`);
done();
