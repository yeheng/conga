/**
 * Wire event types mirrored from `conga/conga-host/src/wire.rs` (OutgoingEvent).
 *
 * This schema is owned by the Rust host; this file is a typed projection of
 * it. Contract (see the wire.rs header): add fields, never rename. The
 * golden fixture `test/fixtures/wire-events.ndjson` is byte-checked against
 * the Rust serializer by `conga-host`'s `wire_fixture_matches_sdk_contract`
 * test — any drift fails CI on the Rust side.
 *
 * Transport notes:
 * - `conga exec --json` emits: content, thinking, tool_start, tool_end,
 *   error, done (the last line of every turn; with usage summary fields on
 *   success).
 * - The gateway additionally emits: busy, queued, approval_request and the
 *   `subagent_*` family. They are typed here so one parser serves both
 *   transports.
 */

interface WireBase {
  type: string;
}

export interface ContentEvent extends WireBase {
  type: "content";
  content: string;
}

export interface ThinkingEvent extends WireBase {
  type: "thinking";
  content: string;
}

export interface ToolStartEvent extends WireBase {
  type: "tool_start";
  name: string;
  /** Raw JSON string of the tool arguments. */
  arguments: string;
  /** Stable id pairing tool_start with tool_end. */
  tool_call_id: string;
}

export interface ToolEndEvent extends WireBase {
  type: "tool_end";
  name: string;
  output: string;
  tool_call_id: string;
}

export interface ErrorEvent extends WireBase {
  type: "error";
  content: string;
  message: string;
}

export interface BusyEvent extends WireBase {
  type: "busy";
  content: string;
  message: string;
}

export interface QueuedEvent extends WireBase {
  type: "queued";
  message: string;
}

export interface ApprovalRequestEvent extends WireBase {
  type: "approval_request";
  /** Approval request id — reply over the gateway socket, not the wire. */
  id: string;
  tool_name: string;
  /** Truncated JSON of the args, for display. */
  description: string;
  /** Full JSON of the args. */
  arguments: string;
  /** Human-readable diff preview for edit/write. */
  preview?: string;
}

/** Usage summary fields, present on a successful turn's final `done`. */
export interface UsageSummary {
  usage_in: number;
  usage_out: number;
  usage_cache_read: number;
  usage_cache_write: number;
  elapsed_ms: number;
}

export interface DoneEvent extends WireBase, Partial<UsageSummary> {
  type: "done";
}

/** Subagent events (gateway only) — parsed but not modelled per-variant. */
export interface SubagentEvent extends WireBase {
  type: `subagent_${string}`;
  [key: string]: unknown;
}

export type WireEvent =
  | ContentEvent
  | ThinkingEvent
  | ToolStartEvent
  | ToolEndEvent
  | ErrorEvent
  | BusyEvent
  | QueuedEvent
  | ApprovalRequestEvent
  | DoneEvent
  | SubagentEvent;

const KNOWN_TYPES = new Set([
  "content",
  "thinking",
  "tool_start",
  "tool_end",
  "error",
  "busy",
  "queued",
  "approval_request",
  "done",
]);

/**
 * Parse one NDJSON line from `conga exec --json` (or a gateway frame).
 * Validation is deliberately thin: the Rust side owns field names (and a
 * fixture test pins them); here we only require a known `type` so callers
 * can switch on the union safely.
 */
export function parseWireEvent(line: string): WireEvent {
  const value: unknown = JSON.parse(line);
  if (typeof value !== "object" || value === null) {
    throw new Error(`wire event is not an object: ${line.slice(0, 120)}`);
  }
  const type = (value as { type?: unknown }).type;
  if (typeof type !== "string") {
    throw new Error(`wire event without "type": ${line.slice(0, 120)}`);
  }
  if (type.startsWith("subagent_")) {
    return value as SubagentEvent;
  }
  if (!KNOWN_TYPES.has(type)) {
    throw new Error(`unknown wire event type "${type}": ${line.slice(0, 120)}`);
  }
  return value as WireEvent;
}

/** True if this `done` carries the turn's usage summary. */
export function isDoneWithSummary(ev: WireEvent): ev is DoneEvent & UsageSummary {
  return ev.type === "done" && typeof (ev as UsageSummary).usage_in === "number";
}
