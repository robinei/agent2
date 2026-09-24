The check wants five and the file says three — that is the whole of it. Fetching the row I kept, because this block has to compute with the bytes, not read them.

```js
const conf = await history.fetch(93);
const fixed = Edit.replaceOnce(conf.content, "3", "5");
await tools.replace_file("CONF.json", fixed, conf.version);
const check = await tools.bash("CHECK 2>&1");
if (check.status !== 0) return `set retries to 5, CHECK still fails:\n${check.stdout}`;
tell("retries was 3 where CHECK wants 5; CONF.json says 5 and CHECK passes.");
finish();
```
