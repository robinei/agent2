const [a, b] = await Promise.all([tools.read_file("a.log"), tools.read_file("b.log")]);
const ae = (a.content.match(/ERROR/g) || []).length;
const be = (b.content.match(/ERROR/g) || []).length;
tell(ae > be ? `a is failing: ${ae} errors vs ${be}.` : `b is failing: ${be} errors vs ${ae}.`);