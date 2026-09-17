tell("reading both logs to see which one is actually failing.");
// "failing" is a judgement on the real text, and I have the text here —
// so the comparison happens here too, rather than being handed on.
const [a, b] = await Promise.all([
    tools.read_file("server-a.log"),
    tools.read_file("server-b.log"),
]);

const aErrors = (a.content.match(/ERROR/g) || []).length;
const bErrors = (b.content.match(/ERROR/g) || []).length;
tell(aErrors > bErrors
    ? `server-a is failing: ${aErrors} ERROR lines vs server-b's ${bErrors}.`
    : `server-b is failing: ${bErrors} ERROR lines vs server-a's ${aErrors}.`);
done();
