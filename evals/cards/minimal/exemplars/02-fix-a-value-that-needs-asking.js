tell("reading the current retry setting, then I'll check with you before changing it.");
// I can't tell what "off" means by guessing, and the file won't say
// what it should be either — a wrong guess is worse than asking, so
// resolve that here, in this same program, rather than reporting what
// is there now and leaving.
const cfg = await tools.read_file("ops/config.json");
const parsed = JSON.parse(cfg.content);

const target = await ask("user", `retries is currently ${parsed.retries} — what should it be?`);

parsed.retries = Number(target);
await tools.write_file("ops/config.json", JSON.stringify(parsed, null, 2));
tell(`set retries to ${target} in ops/config.json.`);
done();
