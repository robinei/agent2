Every call site at once: enumerate, then read them.

```js
const hits = (await tools.bash("grep -rl OLD .")).stdout.split("\n").filter(Boolean);
const fs = await Promise.all(hits.map((p) => tools.read_file(p)));
```

`hits` and `fs` are still bound below — one reply, one scope. One line each: which did what.

```js
for (const [i, p] of hits.entries()) {
  const w = await tools.replace_file(p, Edit.replaceAll(fs[i].content, "OLD", "NEW"), fs[i].version);
  console.log(p, w.diff ? "changed" : "NO CHANGE");
}
const c = await tools.bash("CHECK 2>&1");
if (c.status !== 0) return `${hits.length} changed, CHECK fails:\n${c.stdout}`;
tell(`${hits.length} changed, CHECK passes.`);
finish();
```
