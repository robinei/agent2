Reading the failure once — after this reply I know what it said, and the bytes stay on the record either way.

```js
const check = await tools.bash("CHECK 2>&1");
history.peek(check, (r) => r.stdout);
```

The file it names I am working against from here, so that one stays. `content` alone: the row already carries the version.

```js
const conf = await tools.read_file("CONF.json");
history.keep(conf, (f) => f.content);
```

What I worked out gets rows of its own — nothing else holds it, and one each lets a later compaction drop the one I am done with.

```js
history.note({ why_check_fails: "retries is 3, CHECK wants 5" });
history.note({ who_writes_conf: "the deploy job" });
```
