All the call sites at once, not one per reply: enumerate, then read in parallel.

```js
const hits = (await tools.bash("grep -rl OLD .")).stdout.split("\n").filter(Boolean);
const fs = await Promise.all(hits.map((p) => tools.read_file(p)));
```

`hits` and `fs` are still bound below — one reply, one scope.

```js
for (let i = 0; i < hits.length; i++) {
  const e = Edit.replaceCount(fs[i].content, "OLD", "NEW");
  const w = await tools.replace_file(hits[i], e.result, fs[i].version);
  console.log(`${hits[i]} ${e.count}\n${w.diff}`);
}
const c = await tools.bash("CHECK 2>&1");
if (c.status !== 0) stop(`${hits.length} changed, but CHECK fails:\n${c.stdout}`);
finish(`${hits.length} changed, CHECK passes.`);
```
