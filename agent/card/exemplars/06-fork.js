Reading all three first: what I learn is what a helper would otherwise be told.

```js
const [a, b, c] = await Promise.all(
  ["svc-a", "svc-b", "svc-c"].map((s) => tools.read_file(`${s}.yaml`)),
);
const row = history.append({ a: a.content, b: b.content, c: c.content });
```

A `fork` has read what I have read, so I point at the row rather than write a brief — which could only guess at what matters.

```js
const [changed, shared] = await Promise.all([
  ask(fork(), `history.fetch(${row}) holds three configs. Which changed last night?`),
  ask(fork(), `history.fetch(${row}) holds three configs. Any shared pool limit?`),
]);
tell(`Changed: ${changed}. Shared: ${shared}.`);
finish();
```
