const hits = (await tools.bash("grep -rn MARKER .")).stdout.split("\n").filter(Boolean);
return { question: "decide which of these still matter and deal with those", hits };
