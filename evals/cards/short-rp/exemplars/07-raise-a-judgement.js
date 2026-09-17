const [a, b] = await Promise.all([tools.read_file("b1.txt"), tools.read_file("b2.txt")]);
if (a.content === b.content) tell("identical — either is fine.");
else tell(`keeping ${await raise("conflicting_backups", { a: a.content, b: b.content })}.`);