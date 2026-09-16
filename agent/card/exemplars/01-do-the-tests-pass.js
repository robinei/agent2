tell("running the test suite, then I'll let you know what happened.");
const result = await tools.bash("cargo test 2>&1 | tail -20");
if (result.status === 0) {
    tell("tests pass.");
} else {
    tell(`tests failed:\n${result.stdout}`);
}