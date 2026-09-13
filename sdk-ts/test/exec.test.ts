// End-to-end tests of the exec wrapper against a canned conga binary
// (test/fixtures/fake-conga.mjs) — no LLM, no real harness needed.
import assert from "node:assert/strict";
import { test } from "node:test";

import { exec, runExec, type DoneEvent, type ErrorEvent } from "../src/index.js";

const FAKE = new URL("./fixtures/fake-conga.mjs", import.meta.url).pathname;

test("happy path: runExec resolves with all events and exit code 0", async () => {
  const result = await runExec({ task: "say hi", congaBin: FAKE });
  assert.equal(result.exitCode, 0);
  assert.equal(result.events.length, 5);
  const types = result.events.map((e) => e.type);
  assert.deepEqual(types, ["content", "thinking", "tool_start", "tool_end", "done"]);
  const done = result.events[4] as DoneEvent;
  assert.equal(done.usage_in, 1234);
  assert.equal(done.elapsed_ms, 4321);
});

test("stderr chatter is forwarded to process.stderr by default", async () => {
  const chunks: string[] = [];
  const original = process.stderr.write.bind(process.stderr);
  process.stderr.write = ((c: string) => {
    chunks.push(c);
    return true;
  }) as typeof process.stderr.write;
  try {
    await runExec({ task: "x", congaBin: FAKE });
  } finally {
    process.stderr.write = original;
  }
  assert.ok(chunks.join("").includes("[exec] session"));
});

test("stderr lines reach an attached listener instead of the console", async () => {
  const lines: string[] = [];
  const run = exec({ task: "x", congaBin: FAKE });
  run.on("stderr", (line) => lines.push(line));
  const result = await run.wait();
  assert.equal(result.exitCode, 0);
  assert.ok(lines.some((l) => l.includes("[exec] session")));
});

test("async iteration yields the same stream as wait().events", async () => {
  const run = exec({ task: "x", congaBin: FAKE });
  const seen: string[] = [];
  for await (const ev of run) {
    seen.push(ev.type);
    if (ev.type === "tool_end") break; // early exit must not hang the process
  }
  await run.wait();
  assert.deepEqual(seen, ["content", "thinking", "tool_start", "tool_end"]);
});

test("turn error: exit code 1 and the error event is the last one", async () => {
  const result = await runExec({ task: "x", congaBin: FAKE, env: { FAKE_MODE: "error" } });
  assert.equal(result.exitCode, 1);
  const last = result.events.at(-1) as ErrorEvent;
  assert.equal(last.type, "error");
  assert.equal(last.message, "provider unreachable");
  assert.equal(result.events.at(-2)?.type, "done", "done precedes the error line");
});

test("malformed NDJSON fails loud and rejects wait()", async () => {
  await assert.rejects(
    () => runExec({ task: "x", congaBin: FAKE, env: { FAKE_MODE: "garbage" } }),
    /not json|wire event/,
  );
});

test("extJs scripts are passed through as repeatable --ext-js=", async () => {
  const result = await runExec({
    task: "x",
    congaBin: FAKE,
    env: { FAKE_MODE: "argv" },
    extJs: ["tools/a.js", "tools/b.js"],
  });
  assert.equal(result.exitCode, 0);
  const content = JSON.parse((result.events[0] as { content: string }).content);
  assert.deepEqual(content, ["exec", "--json", "--ext-js=tools/a.js", "--ext-js=tools/b.js", "x"]);
});

test("cancel() aborts a running turn with exit code 130", async () => {
  const run = exec({ task: "x", congaBin: FAKE, env: { FAKE_MODE: "hang" } });
  const result = run.wait();
  // Wait until the child is up, then abort.
  await new Promise((r) => setTimeout(r, 150));
  run.cancel();
  const outcome = await result;
  // The fake handles SIGINT and exits with conga's abort code.
  assert.equal(outcome.exitCode, 130);
  assert.equal(outcome.signal, null);
});

test("missing binary rejects with a clear error", async () => {
  await assert.rejects(
    () => runExec({ task: "x", congaBin: "/nonexistent/conga-binary" }),
    (err: Error) => /ENOENT|spawn/i.test(err.message),
  );
});
