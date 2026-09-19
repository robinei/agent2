Every call site at once rather than one per reply: enumerate them, then read them all in parallel.

```js
const hits = (await tools.bash("grep -rl OLD .")).stdout.split("\n").filter(Boolean);
const fs = await Promise.all(hits.map((p) => tools.read_file(p)));
```

`hits` and `fs` are still bound in the block below — the blocks of one reply share a scope.

```js
for (let i = 0; i < hits.length; i++) {
  const e = Edit.replaceCount(fs[i].content, "OLD", "NEW");
  console.log(`${hits[i]}: ${e.count}`);
  await tools.replace_file(hits[i], e.result, fs[i].version);
}
tell(`${hits.length} files`);
done();
```
