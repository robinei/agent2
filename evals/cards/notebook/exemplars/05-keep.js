Both documents are what I will keep thinking with, so each gets a row of its own — a later compaction can drop one and leave the other exact.

```js
const [readme, design] = await Promise.all([
  tools.read_file("README.md"),
  tools.read_file("DESIGN.md"),
]);
history.append({ readme: readme.content });
history.append({ design: design.content });
```

Now the failure itself.

```js
history.append({ check: (await tools.bash("CHECK 2>&1")).stdout });
```
