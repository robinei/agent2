Reading all three first: what I learn is what a brief would say.

```js
const [a, b, c] = await Promise.all(["a", "b", "c"].map((s) => tools.read_file(`svc-${s}.yaml`)));
const row = history.append({ "svc-a.yaml": a.content, "svc-b.yaml": b.content, "svc-c.yaml": c.content });
```

A fork has read what I read, so I point at the row. Their logs are long; the answers are a line.

```js
const [rel, pool] = await Promise.all([
  ask(fork(), `history.fetch(${row}) has the configs. In deploy.log, which shipped last night?`),
  ask(fork(), `history.fetch(${row}) has the configs. In pool.log, which limit do they share?`),
]);
tell(`Shipped: ${rel}. Shared: ${pool}.`);
finish();
```
