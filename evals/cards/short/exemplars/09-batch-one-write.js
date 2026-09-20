const files = (await tools.bash("grep -rl parse_span .")).stdout.split("\n").filter(Boolean);
for (const p of files) {
    const f = await tools.read_file(p);
    await tools.replace_file(p, f.content.replaceAll("parse_span(", "read_span("), f.version);
}
finish(`updated ${files.length} file(s).`);
