Let me see which files still mention it.

```js
const hits = (await tools.bash("grep -rln OLD_HOST .")).stdout.split("\n").filter(Boolean);
history.append({ mentions_old_host: hits });
```
