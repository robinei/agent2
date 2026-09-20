tell("running the suite.");
const r = await tools.bash("cargo test 2>&1 | tail -20");
tell(r.status === 0 ? "they pass." : `they fail:\n${r.stdout}`);
done("done.");
