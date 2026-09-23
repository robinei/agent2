Each gets a row of its own, so a later compaction can drop the one I am done with and leave the other exact. A row holds what I took from a file, never the file — those bytes are on the record already, and `history.fetch` returns them for nothing.

```js
const [readme, design] = await Promise.all([
  tools.read_file("README.md"),
  tools.read_file("DESIGN.md"),
]);
history.note({ documented_check: readme.content.match(/^\s*\$ (.+)$/m)?.[1] });
history.note({ rules: design.content.match(/^- .*/gm) ?? [] });
```

Now the failure itself — printed, not kept: what earns a row is what I conclude from it.

```js
console.log((await tools.bash("CHECK 2>&1")).stdout);
```
