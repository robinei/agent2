tell("comparing the two backups now.");
// If they genuinely differ, that's not a fact more reading resolves;
// it's a judgement call, and I can't invent a reason to prefer one
// over the other from the files alone. If so: raise it — not guess,
// and not stop here having only noticed the problem.
const [a, b] = await Promise.all([
    tools.read_file("backup-1.txt"),
    tools.read_file("backup-2.txt"),
]);

if (a.content === b.content) {
    tell("identical — no real conflict, either is fine.");
} else {
    const keep = await raise("conflicting_backups", { a: a.content, b: b.content });
    tell(`keeping ${keep}.`);
}
done();
