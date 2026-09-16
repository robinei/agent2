tell("clearing anything in /tmp/build-cache older than a day, and I'll flag it if tonight's count looks unusual.");
const found = await tools.bash("find /tmp/build-cache -type f -mtime +1 -printf '.' | wc -c");
const count = Number(found.stdout.trim()) || 0;

if (count === 0) {
    tell("nothing older than a day — /tmp/build-cache is already clean.");
} else {
    await tools.bash("find /tmp/build-cache -type f -mtime +1 -delete");
    tell(`removed ${count} stale file(s).`);
    if (count > 200) {
        // a short projection for whoever runs this next — not read
        // back by me, I'm done; this run's own count is already in
        // the variable I just used
        append_history(`/tmp/build-cache had ${count} stale files tonight — well above the usual handful; worth checking what's writing there if it keeps climbing.`);
    }
}