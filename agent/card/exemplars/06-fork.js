Reading all three first: what I learn is what a brief would say.

```js
const cfgs = await Promise.all(["a", "b", "c"].map((s) => tools.read_file(`svc-${s}.yaml`)));
const rows = cfgs.map((f) => f.id).join(", ");
```

A fork has read what I read, so I point at the rows those reads already are — a copy would be the same bytes twice. Their logs are long; the answers are a line.

```js
const [rel, pool] = await Promise.all([
  ask(fork(), `Configs: history.fetch of ${rows}. In deploy.log, which shipped last night?`),
  ask(fork(), `Configs: history.fetch of ${rows}. In pool.log, which limit do they share?`),
]);
tell(`Shipped: ${rel}. Shared: ${pool}.`);
finish();
```
