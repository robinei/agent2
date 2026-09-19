const [readme, design] = await Promise.all([
  tools.read_file("README.md"),
  tools.read_file("DESIGN.md"),
]);
// A row each, so a later compaction can drop one and keep the other.
history.append({ readme: readme.content });
history.append({ design: design.content });
return (await tools.bash("CHECK 2>&1")).stdout;
