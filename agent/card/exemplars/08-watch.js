Nothing here will tell me when it lands, so I look, wait, and look again. `ask` is the wait only for a helper I spawned; this is out in the world.

```js
let last = "";
for (let i = 1; i <= 20; i++) {
  const c = await tools.bash("CHECK 2>&1");
  if (c.status === 0) { tell(`settled on check ${i}`); finish(); break; }
  last = c.stdout.trim().split("\n").at(-1) ?? "";
  console.log(`${i}: ${last}`);
  await tools.wait_until(Date.now() + 15000);
}
if (last) return `still not settled after 20 checks over 5 minutes; last said: ${last}`;
```
