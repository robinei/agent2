const names = (await tools.bash("ls -1A")).stdout.split("\n").filter(Boolean);
return { question: "which of these say what the project is? read those and answer", names };
