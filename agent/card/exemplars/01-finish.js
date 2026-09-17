const r = await tools.bash("make check");
tell(r.status === 0 ? "green." : `red:\n${r.stdout}`);
done();
