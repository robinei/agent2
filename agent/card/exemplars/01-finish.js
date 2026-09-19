Changing it, then running the check in the same reply — a change I have not checked is not a change I have made.

```js
const f = await tools.read_file("PATH");
const fixed = Edit.replaceOnce(f.content, "OLD", "NEW");
await tools.replace_file("PATH", fixed, f.version);
const check = await tools.bash("CHECK");
tell(check.status === 0 ? "done — CHECK passes." : `changed it, but CHECK fails:\n${check.stdout}`);
done();
```
