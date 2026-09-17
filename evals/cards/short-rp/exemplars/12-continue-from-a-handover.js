// What the last program returned is a row with an id, so I fetch it
// rather than re-typing it out of the report — fetching costs nothing
// and adds nothing, and a name I transcribe by hand is a name I can
// get wrong.
const prev = await fetch_history(18);
const dropped = [], kept = [];
for (const hit of prev.hits) {
    const path = hit.split(":")[0];
    const check = await tools.bash(`make check ${path}`);
    if (check.status === 0) dropped.push(path);
    else kept.push(path);
}
done();
