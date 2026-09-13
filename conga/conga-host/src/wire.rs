//! Wire DTO for the frontend's streaming chat protocol.
//!
//! This schema is owned by the host (single definition) and shared by every
//! transport: the gateway serializes it onto a WebSocket, the Tauri desktop
//! backend emits the same JSON as the payload of its `chat-event` IPC event.
//! Field-for-field compatibility with the frontend's `processWebSocketMessage`
//! is the contract - add fields, never rename.

use serde::Serialize;

/// One server -> client event. Optional fields are omitted from the JSON when
/// absent, matching the original gateway wire shape exactly.
#[derive(Serialize)]
pub struct OutgoingEvent {
    #[serde(rename = "type")]
    event_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// Stable id pairing a `tool_start` with a `tool_end` (only on those).
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    /// Human-readable diff preview for `approval_request` (edit/write).
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Cumulative input tokens for the whole session (only on `done`).
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_in: Option<u64>,
    /// Cumulative output tokens for the whole session (only on `done`).
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_out: Option<u64>,
    /// Cumulative prompt-cache-read tokens for the whole session (only on
    /// `done`; 0 when the provider reports no cache breakdown).
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_cache_read: Option<u64>,
    /// Cumulative prompt-cache-write tokens for the whole session (only on
    /// `done`; 0 when the provider reports no cache breakdown).
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_cache_write: Option<u64>,
    /// Wall-clock duration of this turn in milliseconds (only on `done`).
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
}

impl OutgoingEvent {
    fn base(event_type: &'static str) -> Self {
        Self {
            event_type,
            id: None,
            tool_call_id: None,
            tool_name: None,
            description: None,
            content: None,
            name: None,
            arguments: None,
            preview: None,
            output: None,
            message: None,
            usage_in: None,
            usage_out: None,
            usage_cache_read: None,
            usage_cache_write: None,
            elapsed_ms: None,
        }
    }

    pub fn content(s: String) -> Self {
        let mut ev = Self::base("content");
        ev.content = Some(s);
        ev
    }
    pub fn thinking(s: String) -> Self {
        let mut ev = Self::base("thinking");
        ev.content = Some(s);
        ev
    }
    pub fn tool_start(name: String, args: String, tool_call_id: String) -> Self {
        let mut ev = Self::base("tool_start");
        ev.name = Some(name);
        ev.arguments = Some(args);
        ev.tool_call_id = Some(tool_call_id);
        ev
    }
    pub fn tool_end(name: String, output: String, tool_call_id: String) -> Self {
        let mut ev = Self::base("tool_end");
        ev.name = Some(name);
        ev.output = Some(output);
        ev.tool_call_id = Some(tool_call_id);
        ev
    }
    pub fn error(msg: String) -> Self {
        let mut ev = Self::base("error");
        ev.content = Some(msg.clone());
        ev.message = Some(msg);
        ev
    }
    pub fn done() -> Self {
        Self::base("done")
    }
    /// Turn-boundary `done` carrying a usage summary. The frontend renders
    /// one line: elapsed time + cumulative input/output (and cache, when
    /// the provider reports it) tokens.
    pub fn done_with_summary(
        usage_in: u64,
        usage_out: u64,
        cache_read: u64,
        cache_write: u64,
        elapsed_ms: u64,
    ) -> Self {
        let mut ev = Self::base("done");
        ev.usage_in = Some(usage_in);
        ev.usage_out = Some(usage_out);
        ev.usage_cache_read = Some(cache_read);
        ev.usage_cache_write = Some(cache_write);
        ev.elapsed_ms = Some(elapsed_ms);
        ev
    }
    /// Reply to a message received while a turn is already running. Kept
    /// distinct from `error` so the frontend can show a toast without
    /// clearing the in-flight conversation state.
    pub fn busy(msg: String) -> Self {
        let mut ev = Self::base("busy");
        ev.content = Some(msg.clone());
        ev.message = Some(msg);
        ev
    }
    /// Acknowledgment for a mid-turn user message that was queued for
    /// steering. The frontend renders the text as a pending user bubble.
    pub fn queued(text: String) -> Self {
        let mut ev = Self::base("queued");
        ev.message = Some(text);
        ev
    }
    pub fn approval_request(
        request_id: String,
        tool_name: String,
        args: &serde_json::Value,
        preview: Option<String>,
    ) -> Self {
        // description 给前端展示；arguments 保留原始参数。截断防超长。
        let desc = serde_json::to_string(args).unwrap_or_default();
        let desc = if desc.chars().count() > 300 {
            format!("{}...", desc.chars().take(300).collect::<String>())
        } else {
            desc
        };
        let mut ev = Self::base("approval_request");
        ev.id = Some(request_id);
        ev.tool_name = Some(tool_name);
        ev.description = Some(desc);
        ev.arguments = Some(args.to_string());
        ev.preview = preview;
        ev
    }
}

// ── Context-occupancy payload (shared by the gateway REST route and the
// desktop `get_context` command — one JSON shape, one window knob) ────────

/// Context occupancy for the frontend. `last_input_tokens` is the current
/// window occupancy (the most recent provider-reported input-token count)
/// and drives the saturation percentage against `max_tokens` — callers
/// pass the resolved window (settings.json `maxTokens` >
/// `CONGA_CONTEXT_WINDOW` > 128k; see
/// [`crate::settings::effective_max_tokens`]). `cumulative_in`/`cumulative_out`
/// are the real accumulated API spend across the session, with
/// `cache_read_tokens`/`cache_write_tokens` as its cache breakdown. The
/// percentage is a display heuristic; the token counts themselves are real
/// API usage.
pub fn context_stats(
    last_input_tokens: u64,
    usage_in: u64,
    usage_out: u64,
    cache_read: u64,
    cache_write: u64,
    max_tokens: u64,
) -> serde_json::Value {
    let usage_percent = if max_tokens > 0 {
        (last_input_tokens as f64 / max_tokens as f64) * 100.0
    } else {
        0.0
    };
    serde_json::json!({
        "current_tokens": last_input_tokens,
        "usage_percent": usage_percent,
        "is_compressing": false,
        "cumulative_in": usage_in,
        "cumulative_out": usage_out,
        "cache_read_tokens": cache_read,
        "cache_write_tokens": cache_write,
        "max_tokens": max_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_stats_scenarios() {
        // 1. Zero usage -> zero tokens, zero percent against the default window.
        let stats = context_stats(0, 0, 0, 0, 0, 128_000);
        assert_eq!(stats["current_tokens"], 0);
        assert_eq!(stats["usage_percent"], 0.0);
        assert_eq!(stats["is_compressing"], false);
        assert_eq!(stats["cumulative_in"], 0);
        assert_eq!(stats["cumulative_out"], 0);
        assert_eq!(stats["cache_read_tokens"], 0);
        assert_eq!(stats["cache_write_tokens"], 0);
        assert_eq!(stats["max_tokens"], 128_000);

        // 2. Occupancy uses `last_input_tokens` (NOT cumulative in+out).
        // 64k current against a 128k window = 50%; cache + max pass
        // through verbatim (callers resolve the window from env/settings).
        let stats = context_stats(64_000, 100_000, 50_000, 30_000, 10_000, 128_000);
        assert_eq!(stats["current_tokens"], 64_000);
        assert_eq!(stats["usage_percent"], 50.0);
        assert_eq!(stats["cumulative_in"], 100_000);
        assert_eq!(stats["cumulative_out"], 50_000);
        assert_eq!(stats["cache_read_tokens"], 30_000);
        assert_eq!(stats["cache_write_tokens"], 10_000);
        assert_eq!(stats["max_tokens"], 128_000);

        // 3. An env-derived window (CONGA_CONTEXT_WINDOW=50000, what
        // effective_max_tokens passes through when settings are silent).
        let stats = context_stats(25_000, 999, 999, 0, 0, 50_000);
        assert_eq!(stats["current_tokens"], 25_000);
        assert_eq!(stats["usage_percent"], 50.0);
        assert_eq!(stats["max_tokens"], 50_000);

        // 4. Zero window is "no percentage", not a division by zero.
        let stats = context_stats(1_000, 1_000, 1_000, 0, 0, 0);
        assert_eq!(stats["current_tokens"], 1_000);
        assert_eq!(stats["usage_percent"], 0.0);
    }

    #[test]
    fn done_with_summary_carries_cache_fields() {
        let v =
            serde_json::to_value(OutgoingEvent::done_with_summary(10, 20, 30, 40, 1_000)).unwrap();
        assert_eq!(v["type"], "done");
        assert_eq!(v["usage_in"], 10);
        assert_eq!(v["usage_out"], 20);
        assert_eq!(v["usage_cache_read"], 30);
        assert_eq!(v["usage_cache_write"], 40);
        assert_eq!(v["elapsed_ms"], 1_000);
        // A plain done carries no summary fields at all.
        let v = serde_json::to_value(OutgoingEvent::done()).unwrap();
        assert!(v.get("usage_in").is_none() && v.get("usage_cache_read").is_none());
    }

    /// Golden NDJSON fixture shared with the TypeScript SDK
    /// (`sdk-ts/test/fixtures/wire-events.ndjson`): one line per wire
    /// event, covering every OutgoingEvent constructor. The SDK's tests
    /// parse the same file, so this test is the schema contract - any
    /// serialization change must regenerate the fixture deliberately:
    /// `CONGA_REGEN_FIXTURE=1 cargo test -p conga-host wire_fixture`
    ///
    /// Comparison is per-line PARSED equality: key order inside a JSON
    /// object is not part of the contract (serde_json's map ordering varies
    /// with `preserve_order` feature unification across the workspace).
    #[test]
    fn wire_fixture_matches_sdk_contract() {
        // Keys are pre-sorted in the source string so the embedded
        // arguments/description strings serialize identically whether or not
        // serde_json's `preserve_order` feature is unified on (BTreeMap sorts;
        // IndexMap preserves the alphabetical insertion order).
        let args: serde_json::Value =
            serde_json::from_str(r#"{"content":"hi","path":"notes/x.txt"}"#).unwrap();
        let events = vec![
            OutgoingEvent::content("你好".into()),
            OutgoingEvent::thinking("checking the plan".into()),
            OutgoingEvent::tool_start("bash".into(), r#"{"cmd":"ls"}"#.into(), "tc1".into()),
            OutgoingEvent::tool_end("bash".into(), "a.txt\nb.txt".into(), "tc1".into()),
            OutgoingEvent::error("provider unreachable".into()),
            OutgoingEvent::busy("a turn is already running".into()),
            OutgoingEvent::queued("mid-turn steer".into()),
            OutgoingEvent::approval_request(
                "req1".into(),
                "write".into(),
                &args,
                Some("--- a/x.txt\n+++ b/x.txt\n+hi".into()),
            ),
            OutgoingEvent::done(),
            OutgoingEvent::done_with_summary(1_234, 567, 89, 12, 4_321),
        ];
        let lines: Vec<String> = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../sdk-ts/test/fixtures/wire-events.ndjson");
        if std::env::var("CONGA_REGEN_FIXTURE").is_ok() {
            std::fs::create_dir_all(fixture.parent().unwrap()).unwrap();
            let mut buf = lines.join("\n");
            buf.push('\n');
            std::fs::write(&fixture, buf).unwrap();
            return;
        }
        let on_disk = std::fs::read_to_string(&fixture).unwrap_or_else(|e| {
            panic!(
                "sdk wire fixture missing ({e}); run CONGA_REGEN_FIXTURE=1 cargo test -p conga-host wire_fixture"
            )
        });
        let on_disk: Vec<serde_json::Value> = on_disk
            .lines()
            .map(|l| {
                serde_json::from_str(l)
                    .unwrap_or_else(|e| panic!("fixture line is not valid JSON ({e}): {l}"))
            })
            .collect();
        let fresh: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).expect("freshly built line parses"))
            .collect();
        assert_eq!(
            on_disk.len(),
            fresh.len(),
            "fixture line count drifted from the wire schema"
        );
        for (i, (got, want)) in on_disk.iter().zip(fresh.iter()).enumerate() {
            assert_eq!(got, want, "wire fixture drift at line {}", i + 1);
        }
    }
}
