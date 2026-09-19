const hits = (await tools.bash("grep -rln OLD_HOST .")).stdout.split("\n").filter(Boolean);
return hits;
