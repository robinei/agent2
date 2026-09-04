// Demo program for `agent debug` (9_TUI Step 2): exercises calls,
// stub-tool awaits (echo/wait_until/fail), console output, and a raise.

function fib(n) {
  if (n < 2) {
    return n;
  }
  return fib(n - 1) + fib(n - 2);
}

async function fetchSum(a, b) {
  const x = await tools.echo(a);
  const y = await tools.echo(b);
  return x + y;
}

console.log("fib(10) =", fib(10));

const sum = await fetchSum(4, 5);
console.log("echo sum:", sum);

console.log("sleeping 800ms...");
await tools.wait_until(input.now + 800);
console.log("awake");

let caught = "";
try {
  await tools.fail("intentional failure");
} catch (e) {
  caught = e;
  console.log("caught:", e);
}

raise("demo_condition", { reason: "press r to resume", caught: caught });

console.log("resumed after condition");
return "done";
