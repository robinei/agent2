const f = await tools.read_file("PATH");
// Not mine to guess, and a closed choice: the answer is one of these.
const pick = await choose("user", "QUESTION?", ["A", "B", "leave it"]);
if (pick === "leave it") {
  finish("left PATH alone.");
} else {
  await tools.replace_file("PATH", Edit.replaceOnce(f.content, "OLD", pick), f.version);
  finish(`PATH says ${pick} now.`);
}
