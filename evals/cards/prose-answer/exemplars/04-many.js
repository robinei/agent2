Every call site at once: enumerate, then read them.

```js
const hits = (await tools.bash("grep -rl OLD .")).stdout.split("\n").filter(Boolean);
const fs = await Promise.all(hits.map((p) => tools.read_file(p)));
```

`hits` and `fs` are still bound below — one reply, one scope.

```js
for (const [i, p] of hits.entries()) {
  const text = Edit.replaceAll(fs[i].content, "OLD", "NEW");
  const w = await tools.replace_file(p, text, fs[i].version);
  console.log(`${p}\n${w.diff}`);
}
const c = await tools.bash("CHECK 2>&1");
if (c.status !== 0) return `${hits.length} changed, but CHECK fails:\n${c.stdout}`;
tell(`${hits.length} changed, CHECK passes.`);
finish();
```
