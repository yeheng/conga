#!/usr/bin/env node
// Canned `conga exec --json` replacement for SDK tests. Never talks to an
// LLM — it replays the golden wire fixture with a mode-selected tail:
//   default      → streaming events + done_with_summary, exit 0
//   FAKE_MODE=error   → streaming events + done + error, exit 1
//   FAKE_MODE=garbage → streaming events + a malformed line (SDK must fail loud)
//   FAKE_MODE=hang    → one content line, then waits for SIGINT (exit 130)
//   FAKE_MODE=argv    → one content event echoing process argv (flag passthrough)
import { readFileSync } from "node:fs";

const fixture = readFileSync(
  new URL("./wire-events.ndjson", import.meta.url),
  "utf8",
).trimEnd()
  .split("\n");

const streaming = fixture.slice(0, 4); // content, thinking, tool_start, tool_end
const plainDone = fixture[8];
const summaryDone = fixture[9];

process.stderr.write("[exec] session fakesession0000000000000000000000000\n");

const mode = process.env.FAKE_MODE ?? "ok";
if (mode !== "argv") {
  for (const line of streaming) process.stdout.write(line + "\n");
}

if (mode === "argv") {
  const content = JSON.stringify(process.argv.slice(2));
  process.stdout.write(`{"type":"content","content":${JSON.stringify(content)}}\n`);
  process.stdout.write(summaryDone + "\n");
  process.exit(0);
} else if (mode === "hang") {
  process.on("SIGINT", () => process.exit(130));
  // Keep the event loop alive forever; only SIGINT ends us.
  setInterval(() => {}, 1 << 30);
} else if (mode === "error") {
  process.stdout.write(plainDone + "\n");
  process.stdout.write(
    '{"type":"error","content":"provider unreachable","message":"provider unreachable"}\n',
  );
  process.exit(1);
} else if (mode === "garbage") {
  process.stdout.write("this is not json\n");
  process.exit(0);
} else {
  process.stdout.write(summaryDone + "\n");
  process.exit(0);
}
