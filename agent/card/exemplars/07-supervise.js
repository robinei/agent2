One helper per package: the suites take minutes, so they run at once, and their output stays out of my context.

```js
const names = (await tools.bash("ls packages")).stdout.split("\n").filter(Boolean);
const crew = names.map((n) => spawn(`Run the suite for ${n}. Say what failed, or that nothing did.`));
```

`ask` is the waiting: each settles when its helper answers, so the verdicts arrive without polling.

```js
const said = await Promise.all(crew.map((h, i) => ask(h, `Result for ${names[i]}?`)));
const bad = said.map((v, i) => [names[i], v]).filter(([, v]) => /fail/i.test(v));
tell(bad.length ? bad.map(([n, v]) => `${n}: ${v}`).join("\n") : `All ${names.length} pass.`);
finish();
```
