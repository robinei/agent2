const [readme, design] = await Promise.all([
  tools.read_file("README.md"),
  tools.read_file("DESIGN.md"),
]);
// Kept as rows of their own, because I will be thinking about these for
// the rest of the task and there is only one return.
history.append({ readme: readme.content, design: design.content });
return (await tools.bash("CHECK 2>&1")).stdout;
