One helper per package: each suite takes minutes, and its output belongs in their context rather than mine.

```js
const names = (await tools.bash("ls packages")).stdout.split("\n").filter(Boolean);
for (const n of names) {
  tell(spawn(`Run the suite for ${n}. Report failures with tell("parent", ...), naming it.`), "Begin.");
}
```

Ending the turn here would leave nobody watching, so the program waits instead.

```js
let busy;
do {
  await tools.wait_until(Date.now() + 15000);
  busy = list_agents().filter((r) => r.status === "running" || r.status === "thinking");
} while (busy.length);
history.append({ supervising: names });
```
