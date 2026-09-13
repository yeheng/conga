// Golden-fixture test: the SAME file the Rust side byte-checks
// (conga-host's `wire_fixture_matches_sdk_contract`). If this passes, the
// TS projection of the wire schema is in lockstep with the Rust
// serializer.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  isDoneWithSummary,
  parseWireEvent,
  type ApprovalRequestEvent,
  type ContentEvent,
  type DoneEvent,
  type ErrorEvent,
  type QueuedEvent,
  type BusyEvent,
  type ThinkingEvent,
  type ToolEndEvent,
  type ToolStartEvent,
  type WireEvent,
} from "../src/wire.js";

const fixture = readFileSync(
  new URL("./fixtures/wire-events.ndjson", import.meta.url),
  "utf8",
);
const lines = fixture.trimEnd().split("\n");

test("fixture covers every wire event constructor", () => {
  assert.equal(lines.length, 10, "fixture must carry one line per constructor (done twice)");
  const types = lines.map((l) => JSON.parse(l).type);
  assert.deepEqual(types, [
    "content",
    "thinking",
    "tool_start",
    "tool_end",
    "error",
    "busy",
    "queued",
    "approval_request",
    "done",
    "done",
  ]);
});

test("every fixture line parses into the union", () => {
  for (const line of lines) {
    const ev = parseWireEvent(line);
    assert.equal(typeof ev.type, "string");
  }
});

test("parser rejects malformed lines loud", () => {
  assert.throws(() => parseWireEvent("not json"));
  assert.throws(() => parseWireEvent('{"no_type":1}'));
  assert.throws(() => parseWireEvent('{"type":"mystery"}'));
  assert.throws(() => parseWireEvent('["array"]'));
});

test("exhaustive field check per event type", () => {
  const evs = lines.map(parseWireEvent);

  const content = evs[0] as ContentEvent;
  assert.equal(content.type, "content");
  assert.equal(content.content, "你好");

  const thinking = evs[1] as ThinkingEvent;
  assert.equal(thinking.type, "thinking");
  assert.equal(thinking.content, "checking the plan");

  const start = evs[2] as ToolStartEvent;
  assert.equal(start.type, "tool_start");
  assert.equal(start.name, "bash");
  assert.deepEqual(JSON.parse(start.arguments), { cmd: "ls" });
  assert.equal(start.tool_call_id, "tc1");

  const end = evs[3] as ToolEndEvent;
  assert.equal(end.type, "tool_end");
  assert.equal(end.output, "a.txt\nb.txt");
  assert.equal(end.tool_call_id, "tc1");

  const error = evs[4] as ErrorEvent;
  assert.equal(error.type, "error");
  assert.equal(error.message, "provider unreachable");

  assert.equal((evs[5] as BusyEvent).message, "a turn is already running");
  assert.equal((evs[6] as QueuedEvent).message, "mid-turn steer");

  const approval = evs[7] as ApprovalRequestEvent;
  assert.equal(approval.type, "approval_request");
  assert.equal(approval.id, "req1");
  assert.equal(approval.tool_name, "write");
  assert.match(approval.preview!, /^\+\+\+ b\/x\.txt/m);

  const plainDone = evs[8] as DoneEvent;
  assert.equal(plainDone.type, "done");
  assert.equal(isDoneWithSummary(plainDone), false, "plain done carries no usage");

  const summaryDone = evs[9] as DoneEvent;
  assert.equal(isDoneWithSummary(summaryDone), true);
  assert.equal((summaryDone as typeof summaryDone & { usage_in: number }).usage_in, 1234);
});

test("subagent_* types parse as the open-ended family", () => {
  const ev = parseWireEvent('{"type":"subagent_started","id":"s1","index":0}');
  assert.equal(ev.type, "subagent_started");
});

test("switch over the union is exhaustive", () => {
  for (const ev of lines.map(parseWireEvent)) {
    switch (ev.type) {
      case "content":
      case "thinking":
      case "tool_start":
      case "tool_end":
      case "error":
      case "busy":
      case "queued":
      case "approval_request":
      case "done":
        break;
      default:
        if (!ev.type.startsWith("subagent_")) {
          assert.fail(`unhandled wire type: ${ev.type}`);
        }
    }
  }
});
