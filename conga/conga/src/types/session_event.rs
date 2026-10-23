//! `SessionEvent` — the append-only vocabulary of the session event log.
//!
//! User/Assistant/ToolResult wrap the corresponding [`AgentMessage`]
//! variants (serde-compatible with legacy message rows: migration is just
//! wrapping by discriminant). [`derive_messages`] projects the log back into
//! the model-visible message list.

use serde::{Deserialize, Serialize};

use crate::types::message::{AgentMessage, ContentBlock, ToolResultMessage, Usage};

/// The append-only vocabulary of the session event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    TurnStart,
    /// Wraps an [`AgentMessage::User`].
    User(AgentMessage),
    /// Wraps an [`AgentMessage::Assistant`], with the token accounting that
    /// traveled with it.
    Assistant {
        message: AgentMessage,
        usage: Option<Usage>,
    },
    /// Wraps an [`AgentMessage::ToolResult`].
    ToolResult(AgentMessage),
    TurnEnd {
        reason: TurnEndReason,
    },
    /// The user cleared the conversation. A FACT in the log, not a log
    /// rotation: [`derive_messages`] projects away everything up to and
    /// including the LAST `Cleared`, while the rows before it stay on disk
    /// (append-only is never violated). Keeping the session id stable means
    /// live connections, REST readers, and the session index all keep
    /// addressing the same chat — no ghost sessions after `/clear`.
    Cleared,
    /// A compaction checkpoint: `base` is the frozen post-compaction
    /// message view (pinned original task + notice + newest groups, as
    /// produced by the host's compaction policy), `dropped` counts the
    /// messages folded away. [`derive_messages`] restarts from the LAST
    /// `Compacted`. Appending this event — the log is never rewritten —
    /// pins the new prefix ON DISK, so every later turn replays the same
    /// bytes instead of re-deriving a per-request view. That pin is what
    /// keeps the provider prompt cache warm: a wire-only compaction view
    /// oscillates between compacted and full history and misses the cache
    /// every turn.
    Compacted {
        base: Vec<AgentMessage>,
        dropped: usize,
    },
}

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Aborted { cause: Option<CancelCause> },
    Error { message: String },
}

/// Who cancelled an in-flight turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CancelCause {
    User,
    Parent,
    Hook { reason: String },
}

impl SessionEvent {
    /// Message → event (only the three message-surface variants;
    /// TurnStart/TurnEnd are not constructed this way).
    pub fn from_message(msg: &AgentMessage, usage: Option<Usage>) -> Option<SessionEvent> {
        match msg {
            AgentMessage::User(_) => Some(SessionEvent::User(msg.clone())),
            AgentMessage::Assistant(_) => Some(SessionEvent::Assistant {
                message: msg.clone(),
                usage,
            }),
            AgentMessage::ToolResult(_) => Some(SessionEvent::ToolResult(msg.clone())),
            AgentMessage::Custom(_) => None,
        }
    }
}

/// Pure projection: event log → model-visible messages. TurnStart/TurnEnd/
/// Cleared produce no messages. Everything up to and including the LAST
/// `Cleared` OR `Compacted` is replaced by that checkpoint — `/clear`
/// restarts empty, a compaction checkpoint restarts from its frozen `base`
/// plus whatever was appended after it (the log itself stays append-only
/// either way). A torn tail left by a crash is projected as-is (the partial
/// facts are kept intact).
pub fn derive_messages(log: &[SessionEvent]) -> Vec<AgentMessage> {
    match log.iter().rposition(is_checkpoint) {
        Some(i) => match &log[i] {
            SessionEvent::Compacted { base, .. } => base
                .iter()
                .cloned()
                .chain(log[i + 1..].iter().filter_map(surface))
                .collect(),
            _ => log[i + 1..].iter().filter_map(surface).collect(),
        },
        None => log.iter().filter_map(surface).collect(),
    }
}

/// True at projection restart points: the LAST `Cleared` or `Compacted`
/// wins, whichever it is.
fn is_checkpoint(ev: &SessionEvent) -> bool {
    matches!(ev, SessionEvent::Cleared | SessionEvent::Compacted { .. })
}

/// Event → model-visible message, if it has one.
fn surface(ev: &SessionEvent) -> Option<AgentMessage> {
    match ev {
        SessionEvent::User(msg)
        | SessionEvent::Assistant { message: msg, .. }
        | SessionEvent::ToolResult(msg) => Some(msg.clone()),
        SessionEvent::TurnStart
        | SessionEvent::TurnEnd { .. }
        | SessionEvent::Cleared
        | SessionEvent::Compacted { .. } => None,
    }
}

/// A log row reduced to what the model-visible projection needs.
///
/// Every consumer of the event log reduces its input to this first — the
/// full projection, the row-level counter, the index builder — so "what
/// the model sees" has exactly ONE implementation instead of one per
/// call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowView {
    /// Projects to one model-visible message (`User` / `Assistant` /
    /// `ToolResult`).
    Message,
    /// A compaction checkpoint whose frozen `base` holds this many
    /// messages. Everything before it stops being projected.
    Compacted { base_len: usize },
    /// A `/clear` marker: the conversation restarts empty.
    Cleared,
    /// Projects to nothing (`TurnStart` / `TurnEnd` / unknown variant).
    Inactive,
}

/// Reduce a parsed event to its projection contribution.
pub fn row_view(ev: &SessionEvent) -> RowView {
    match ev {
        SessionEvent::User(_) | SessionEvent::Assistant { .. } | SessionEvent::ToolResult(_) => {
            RowView::Message
        }
        SessionEvent::Cleared => RowView::Cleared,
        SessionEvent::Compacted { base, .. } => RowView::Compacted {
            base_len: base.len(),
        },
        SessionEvent::TurnStart | SessionEvent::TurnEnd { .. } => RowView::Inactive,
    }
}

/// Fold log rows (in log order) into the model-visible message count.
///
/// A checkpoint REPLACES the count so far: `Cleared` resets it to zero,
/// `Compacted` sets it to the size of the frozen base. This is exactly the
/// scope rule [`derive_messages`] applies, expressed on the reduced row so
/// a cheap row-level scan reuses it instead of re-deriving the rule.
pub fn visible_count_from_rows<I: IntoIterator<Item = RowView>>(rows: I) -> usize {
    let mut count = 0usize;
    for row in rows {
        match row {
            RowView::Message => count += 1,
            RowView::Compacted { base_len } => count = base_len,
            RowView::Cleared => count = 0,
            RowView::Inactive => {}
        }
    }
    count
}

/// The number of model-visible messages a log projects to.
///
/// Equal to `derive_messages(log).len()` by construction.
pub fn visible_message_count(log: &[SessionEvent]) -> usize {
    visible_count_from_rows(log.iter().map(row_view))
}

/// Reduce one raw JSONL row to its projection contribution, WITHOUT
/// materializing the row's payload.
///
/// `None` = the row is not a decodable event row (torn tail, not JSON) and
/// contributes nothing. An unknown variant decodes to
/// [`RowView::Inactive`] — the same "skip" the full projection gives it.
pub fn row_view_from_json(line: &str) -> Option<RowView> {
    let probe: RowProbe = serde_json::from_str(line).ok()?;
    Some(match probe.kind.as_str() {
        "user" | "assistant" | "tool_result" => RowView::Message,
        "cleared" => RowView::Cleared,
        "compacted" => RowView::Compacted {
            base_len: probe.base.map_or(0, |b| b.0),
        },
        _ => RowView::Inactive,
    })
}

/// Minimal probe of an event row: the variant discriminant plus, for a
/// compaction checkpoint, the LENGTH of its frozen base. The length is
/// counted without materializing the base's messages — a compacted base
/// can carry megabytes of tool output.
#[derive(serde::Deserialize)]
struct RowProbe {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    base: Option<BaseLen>,
}

/// The number of elements in a JSON array, counted without allocating
/// them.
struct BaseLen(usize);

impl<'de> serde::Deserialize<'de> for BaseLen {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BaseLenVisitor;

        impl<'de> serde::de::Visitor<'de> for BaseLenVisitor {
            type Value = BaseLen;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of messages")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<BaseLen, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut len = 0usize;
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    len += 1;
                }
                Ok(BaseLen(len))
            }
        }

        deserializer.deserialize_seq(BaseLenVisitor)
    }
}

/// Index (0-based) of the first event the model-visible view still
/// projects from — i.e. just past the last [`SessionEvent::Cleared`] or
/// [`SessionEvent::Compacted`], or 0 when neither appears. Hosts use this
/// to scope log-tail scans (e.g. the compaction budget's usage restore) to
/// the post-checkpoint slice, matching [`derive_messages`]: a usage row
/// from before a checkpoint describes history that no longer exists.
///
/// This is THE scope of the model-visible view. Anything that needs to
/// know "which part of the log is still in play" uses this function —
/// there is no second predicate.
pub fn view_scope_start(log: &[SessionEvent]) -> usize {
    log.iter().rposition(is_checkpoint).map_or(0, |i| i + 1)
}

/// Index (0-based) of the first event that still belongs to the
/// conversation for BILLING purposes — i.e. just past the last
/// [`SessionEvent::Cleared`], or 0 when none appears.
///
/// Deliberately different from [`view_scope_start`]: tokens billed before
/// a compaction checkpoint are still facts about this conversation, even
/// though the checkpoint stops them from being projected into the model's
/// view. Only `/clear` starts a new conversation.
pub fn billing_scope_start(log: &[SessionEvent]) -> usize {
    log.iter()
        .rposition(is_conversation_reset)
        .map_or(0, |i| i + 1)
}

/// True at conversation resets: `/clear` starts a NEW conversation, a
/// compaction checkpoint does not (same conversation, shorter window).
fn is_conversation_reset(ev: &SessionEvent) -> bool {
    matches!(ev, SessionEvent::Cleared)
}

/// Session-wide prompt-cache accounting, folded from the persisted usage
/// rows. Scope matches the conversation: `/clear` restarts the count,
/// `Compacted` does not — tokens billed before a checkpoint are still
/// facts about this conversation. Derived on demand, never stored: the
/// same "state as a fold" as [`derive_messages`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheStats {
    /// Sum of reported `input_tokens` (providers that fold cache reads
    /// into input — OpenAI-compat — are already counted there).
    pub input_tokens: u64,
    /// Sum of tokens served from the provider's prompt cache.
    pub cache_read_tokens: u64,
    /// Sum of tokens written into the provider's cache.
    pub cache_write_tokens: u64,
    /// Assistant rows that carried usage at all.
    pub calls: u64,
    /// Assistant rows that reported cache fields (old logs/providers may
    /// omit them; the distinction keeps the hit rate honest).
    pub cache_reporting_calls: u64,
}

impl CacheStats {
    /// `cache_read / input` over the session; `None` before any usage.
    pub fn hit_rate(&self) -> Option<f64> {
        (self.input_tokens > 0).then(|| self.cache_read_tokens as f64 / self.input_tokens as f64)
    }
}

/// Fold the log's usage rows into [`CacheStats`] (scoped past the last
/// `Cleared`, like the conversation itself — see [`billing_scope_start`]).
pub fn cache_stats(log: &[SessionEvent]) -> CacheStats {
    let from = billing_scope_start(log);
    log[from..].iter().fold(CacheStats::default(), |mut s, ev| {
        if let SessionEvent::Assistant { usage: Some(u), .. } = ev {
            s.input_tokens += u.input_tokens;
            s.cache_read_tokens += u.cache_read_tokens.unwrap_or(0);
            s.cache_write_tokens += u.cache_write_tokens.unwrap_or(0);
            s.calls += 1;
            if u.cache_read_tokens.is_some() || u.cache_write_tokens.is_some() {
                s.cache_reporting_calls += 1;
            }
        }
        s
    })
}

/// Synthesize error `ToolResult`s for tool calls whose turn ended before a
/// result was recorded — a cooperative abort between calls in a batch, a
/// crash mid-execution, or a stream error after partial tool calls. The log
/// keeps those partial facts intact (that is the point); the provider
/// protocol does not tolerate them: OpenAI-compat and Anthropic both reject
/// an assistant `tool_calls` message that is not followed by one result per
/// `tool_call_id` (HTTP 400). Runs on the in-memory working copy only, right
/// after [`derive_messages`]; the on-disk log is untouched.
pub fn repair_unanswered_tool_calls(messages: &mut Vec<AgentMessage>) {
    let mut i = 0;
    while i < messages.len() {
        // Tool calls of this assistant message, in call order.
        let calls: Vec<(String, String)> = match &messages[i] {
            AgentMessage::Assistant(a) => a
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall { tool_call: tc } => {
                        Some((tc.id.clone(), tc.function.name.clone()))
                    }
                    _ => None,
                })
                .collect(),
            _ => {
                i += 1;
                continue;
            }
        };
        if calls.is_empty() {
            i += 1;
            continue;
        }
        // Consume the run of tool results that answers this assistant.
        let mut answered: std::collections::HashSet<String> = Default::default();
        let mut j = i + 1;
        while let Some(AgentMessage::ToolResult(tr)) = messages.get(j) {
            answered.insert(tr.tool_call_id.clone());
            j += 1;
        }
        let missing: Vec<(String, String)> = calls
            .into_iter()
            .filter(|(id, _)| !answered.contains(id))
            .collect();
        if missing.is_empty() {
            i = j;
            continue;
        }
        let synthesized: Vec<AgentMessage> = missing
            .into_iter()
            .map(|(id, name)| {
                AgentMessage::ToolResult(ToolResultMessage {
                    tool_call_id: id,
                    tool_name: name,
                    content: vec![ContentBlock::text(
                        "Error: no result was recorded for this tool call (the turn ended \
                         before it completed); treat it as failed.",
                    )],
                    is_error: true,
                    timestamp: crate::now(),
                })
            })
            .collect();
        let inserted = synthesized.len();
        messages.splice(j..j, synthesized);
        i = j + inserted;
    }
}

#[cfg(test)]
mod tests {
    use crate::types::message::AgentMessage;
    use crate::types::session_event::{
        billing_scope_start, cache_stats, derive_messages, row_view, row_view_from_json,
        view_scope_start, visible_count_from_rows, visible_message_count, CancelCause, RowView,
        SessionEvent, TurnEndReason, Usage,
    };

    #[test]
    fn derive_projects_only_surface_events() {
        let log = vec![
            SessionEvent::TurnStart,
            SessionEvent::User(AgentMessage::user("hi")),
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("hello"),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                }),
            },
            SessionEvent::TurnEnd {
                reason: TurnEndReason::Completed,
            },
        ];
        assert_eq!(derive_messages(&log).len(), 2); // user + assistant
    }

    #[test]
    fn derive_tolerates_missing_turn_end() {
        // torn-tail 崩溃遗留
        let log = vec![
            SessionEvent::TurnStart,
            SessionEvent::User(AgentMessage::user("hi")),
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("partial"),
                usage: None,
            },
        ];
        assert_eq!(derive_messages(&log).len(), 2);
    }

    #[test]
    fn derive_truncates_at_cleared() {
        // /clear: everything up to and INCLUDING the marker is gone from the
        // model's view; rows after it live on.
        let log = vec![
            SessionEvent::User(AgentMessage::user("old question")),
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("old answer"),
                usage: None,
            },
            SessionEvent::Cleared,
            SessionEvent::User(AgentMessage::user("fresh start")),
        ];
        let msgs = derive_messages(&log);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0], AgentMessage::User(u)
            if matches!(&u.content[0], crate::ContentBlock::Text { text } if text == "fresh start")));
    }

    #[test]
    fn derive_last_cleared_wins() {
        // Two clears: only the LAST marker matters.
        let log = vec![
            SessionEvent::Cleared,
            SessionEvent::User(AgentMessage::user("between clears")),
            SessionEvent::Cleared,
        ];
        assert!(derive_messages(&log).is_empty());
    }

    #[test]
    fn derive_cleared_only_log_is_empty() {
        assert!(derive_messages(&[SessionEvent::Cleared]).is_empty());
    }

    #[test]
    fn view_scope_start_matches_derive() {
        let log = vec![
            SessionEvent::User(AgentMessage::user("a")),
            SessionEvent::Cleared,
            SessionEvent::User(AgentMessage::user("b")),
        ];
        assert_eq!(view_scope_start(&log), 2);
        assert_eq!(view_scope_start(&log[..1]), 0, "never cleared -> 0");
    }

    #[test]
    fn derive_restarts_from_last_compacted() {
        // Epoch compaction: the checkpoint's frozen base replaces the
        // pre-checkpoint rows; everything appended after it survives.
        let log = vec![
            SessionEvent::User(AgentMessage::user("old1")),
            SessionEvent::User(AgentMessage::user("old2")),
            SessionEvent::Compacted {
                base: vec![
                    AgentMessage::user("pinned task"),
                    AgentMessage::user("[compacted 1 earlier messages]"),
                ],
                dropped: 1,
            },
            SessionEvent::User(AgentMessage::user("after checkpoint")),
        ];
        let msgs = derive_messages(&log);
        assert_eq!(msgs.len(), 3);
        assert!(matches!(&msgs[0], AgentMessage::User(u)
            if matches!(&u.content[0], crate::ContentBlock::Text { text } if text == "pinned task")));
        assert!(matches!(&msgs[2], AgentMessage::User(u)
            if matches!(&u.content[0], crate::ContentBlock::Text { text } if text == "after checkpoint")));
    }

    #[test]
    fn last_checkpoint_wins_between_cleared_and_compacted() {
        // Cleared AFTER a Compacted: the clear wins (empty projection).
        let log = vec![
            SessionEvent::Compacted {
                base: vec![AgentMessage::user("base")],
                dropped: 3,
            },
            SessionEvent::Cleared,
            SessionEvent::User(AgentMessage::user("fresh")),
        ];
        assert_eq!(derive_messages(&log).len(), 1);

        // Compacted AFTER a Cleared: the checkpoint wins (base + tail).
        let log = vec![
            SessionEvent::Cleared,
            SessionEvent::User(AgentMessage::user("pre-checkpoint")),
            SessionEvent::Compacted {
                base: vec![AgentMessage::user("base")],
                dropped: 1,
            },
        ];
        assert_eq!(derive_messages(&log).len(), 1);
    }

    #[test]
    fn view_scope_start_recognizes_compacted() {
        let log = vec![
            SessionEvent::User(AgentMessage::user("a")),
            SessionEvent::Compacted {
                base: vec![],
                dropped: 1,
            },
            SessionEvent::User(AgentMessage::user("b")),
        ];
        assert_eq!(view_scope_start(&log), 2);
    }

    // ---- scope functions --------------------------------------------------

    #[test]
    fn billing_scope_start_ignores_compacted_checkpoints() {
        // Billing scope = the CONVERSATION, not the view: a compaction
        // checkpoint shortens the model's window but does not start a new
        // conversation, so the tokens billed before it still count.
        let log = vec![
            SessionEvent::User(AgentMessage::user("a")),
            SessionEvent::Compacted {
                base: vec![],
                dropped: 1,
            },
            SessionEvent::User(AgentMessage::user("b")),
        ];
        assert_eq!(billing_scope_start(&log), 0, "compaction is not a reset");
        assert_eq!(
            view_scope_start(&log),
            2,
            "the view restarts at the checkpoint"
        );
    }

    #[test]
    fn billing_scope_start_stops_at_the_last_cleared() {
        let log = vec![
            SessionEvent::User(AgentMessage::user("a")),
            SessionEvent::Cleared,
            SessionEvent::User(AgentMessage::user("b")),
        ];
        assert_eq!(billing_scope_start(&log), 2);
    }

    #[test]
    fn billing_scope_start_of_a_log_without_cleared_is_zero() {
        let log = vec![SessionEvent::User(AgentMessage::user("a"))];
        assert_eq!(billing_scope_start(&log), 0);
    }

    // ---- visible count ----------------------------------------------------

    #[test]
    fn visible_message_count_matches_the_projection() {
        let log = vec![
            SessionEvent::TurnStart,
            SessionEvent::User(AgentMessage::user("a")),
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("b"),
                usage: None,
            },
            SessionEvent::ToolResult(tool_result("t1")),
            SessionEvent::TurnEnd {
                reason: TurnEndReason::Completed,
            },
        ];
        assert_eq!(visible_message_count(&log), derive_messages(&log).len());
        assert_eq!(visible_message_count(&log), 3);
    }

    #[test]
    fn visible_message_count_restarts_at_a_compacted_checkpoint() {
        // The checkpoint's frozen base REPLACES everything before it.
        let log = vec![
            SessionEvent::User(AgentMessage::user("old1")),
            SessionEvent::User(AgentMessage::user("old2")),
            SessionEvent::Compacted {
                base: vec![AgentMessage::user("pinned task")],
                dropped: 1,
            },
            SessionEvent::User(AgentMessage::user("new")),
        ];
        assert_eq!(visible_message_count(&log), derive_messages(&log).len());
        assert_eq!(
            visible_message_count(&log),
            2,
            "base + one appended message"
        );
    }

    #[test]
    fn visible_message_count_resets_at_a_cleared_marker() {
        let log = vec![
            SessionEvent::User(AgentMessage::user("old")),
            SessionEvent::Cleared,
            SessionEvent::User(AgentMessage::user("new")),
        ];
        assert_eq!(visible_message_count(&log), derive_messages(&log).len());
        assert_eq!(visible_message_count(&log), 1);
    }

    #[test]
    fn visible_count_from_rows_applies_the_checkpoint_rules() {
        let rows = vec![
            RowView::Message,
            RowView::Message,
            RowView::Inactive,
            RowView::Compacted { base_len: 5 },
            RowView::Message,
            RowView::Cleared,
            RowView::Message,
        ];
        assert_eq!(visible_count_from_rows(rows), 1);
    }

    #[test]
    fn row_view_reduces_an_event_to_its_contribution() {
        assert_eq!(
            row_view(&SessionEvent::User(AgentMessage::user("a"))),
            RowView::Message
        );
        assert_eq!(
            row_view(&SessionEvent::TurnStart),
            RowView::Inactive,
            "markers project to nothing"
        );
        assert_eq!(row_view(&SessionEvent::Cleared), RowView::Cleared);
        assert_eq!(
            row_view(&SessionEvent::Compacted {
                base: vec![AgentMessage::user("a"), AgentMessage::user("b")],
                dropped: 3,
            }),
            RowView::Compacted { base_len: 2 }
        );
    }

    // ---- row-level probe --------------------------------------------------

    #[test]
    fn row_view_from_json_reads_the_discriminant() {
        assert_eq!(
            row_view_from_json(r#"{"type":"user"}"#),
            Some(RowView::Message)
        );
        assert_eq!(
            row_view_from_json(r#"{"type":"assistant"}"#),
            Some(RowView::Message)
        );
        assert_eq!(
            row_view_from_json(r#"{"type":"tool_result"}"#),
            Some(RowView::Message)
        );
        assert_eq!(
            row_view_from_json(r#"{"type":"cleared"}"#),
            Some(RowView::Cleared)
        );
        assert_eq!(
            row_view_from_json(r#"{"type":"turn_start"}"#),
            Some(RowView::Inactive)
        );
    }

    #[test]
    fn row_view_from_json_counts_a_compacted_base() {
        let row = r#"{"type":"compacted","base":[{"a":1},{"b":2},{"c":3}],"dropped":7}"#;
        assert_eq!(
            row_view_from_json(row),
            Some(RowView::Compacted { base_len: 3 })
        );
    }

    #[test]
    fn row_view_from_json_ignores_undecodable_rows() {
        assert_eq!(row_view_from_json(r#"{"type":"user""#), None, "torn tail");
        assert_eq!(row_view_from_json("not json at all"), None);
        assert_eq!(row_view_from_json(r#"{"no_type":true}"#), None);
    }

    #[test]
    fn row_view_from_json_treats_an_unknown_variant_as_inactive() {
        assert_eq!(
            row_view_from_json(r#"{"type":"future_variant"}"#),
            Some(RowView::Inactive)
        );
    }

    #[test]
    fn the_row_probe_and_the_projection_agree_on_a_real_log() {
        // The row-level probe is a cheap path to the SAME answer the full
        // projection gives. If these two ever drift, the session list and
        // the model disagree about the conversation.
        let log = vec![
            SessionEvent::TurnStart,
            SessionEvent::User(AgentMessage::user("a")),
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("b"),
                usage: None,
            },
            SessionEvent::TurnEnd {
                reason: TurnEndReason::Completed,
            },
            SessionEvent::Compacted {
                base: vec![AgentMessage::user("pinned")],
                dropped: 2,
            },
            SessionEvent::User(AgentMessage::user("c")),
            SessionEvent::Cleared,
            SessionEvent::User(AgentMessage::user("d")),
        ];
        let rows: Vec<RowView> = log
            .iter()
            .filter_map(|ev| {
                serde_json::to_string(ev)
                    .ok()
                    .and_then(|line| row_view_from_json(&line))
            })
            .collect();
        assert_eq!(
            visible_count_from_rows(rows),
            visible_message_count(&log),
            "the row probe and the full projection must agree"
        );
        assert_eq!(visible_message_count(&log), derive_messages(&log).len());
    }

    #[test]
    fn cache_stats_folds_usage_and_scopes_from_clear() {
        let usage = |input: u64, read: Option<u64>| Usage {
            input_tokens: input,
            output_tokens: 1,
            cache_read_tokens: read,
            cache_write_tokens: None,
        };
        let log = vec![
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("pre-clear"),
                usage: Some(usage(1_000, Some(900))),
            },
            SessionEvent::Cleared,
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("a"),
                usage: Some(usage(100, Some(80))),
            },
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("no-cache-fields"),
                usage: Some(usage(50, None)),
            },
            SessionEvent::Assistant {
                message: AgentMessage::assistant_text("no-usage"),
                usage: None,
            },
        ];
        let s = cache_stats(&log);
        assert_eq!(s.input_tokens, 150);
        assert_eq!(s.cache_read_tokens, 80);
        assert_eq!(s.calls, 2);
        assert_eq!(s.cache_reporting_calls, 1);
        assert!((s.hit_rate().unwrap() - 80.0 / 150.0).abs() < 1e-12);
        assert_eq!(cache_stats(&[]).hit_rate(), None);
    }

    #[test]
    fn compacted_event_serde_roundtrip() {
        let ev = SessionEvent::Compacted {
            base: vec![AgentMessage::user("task")],
            dropped: 7,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"compacted""#), "{s}");
        let back: SessionEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn serde_tag_shape_is_snake_case() {
        let ev = SessionEvent::TurnEnd {
            reason: TurnEndReason::Aborted {
                cause: Some(CancelCause::User),
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"turn_end""#));
        assert!(s.contains(r#""kind":"aborted""#));
    }

    #[test]
    fn unknown_type_discriminant_fails_to_parse() {
        // fail closed
        assert!(serde_json::from_str::<SessionEvent>(r#"{"type":"wat","data":1}"#).is_err());
    }

    /// Assistant carrying two tool calls (the dangling-call shape an
    /// aborted batch leaves in the log).
    fn assistant_with_calls(ids: &[&str]) -> AgentMessage {
        use crate::types::message::{
            AssistantMessage, ContentBlock, FunctionCall, StopReason, ToolCall,
        };
        let mut a = AssistantMessage::new(&"m".into());
        a.stop_reason = StopReason::ToolUse;
        for id in ids {
            a.content.push(ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: (*id).to_string(),
                    function: FunctionCall {
                        name: "write".to_string(),
                        arguments: "{}".to_string(),
                    },
                },
            });
        }
        AgentMessage::Assistant(a)
    }

    fn tool_result(id: &str) -> AgentMessage {
        use crate::types::message::{ContentBlock, ToolResultMessage};
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: id.to_string(),
            tool_name: "write".to_string(),
            content: vec![ContentBlock::text("ok")],
            is_error: false,
            timestamp: 0,
        })
    }

    #[test]
    fn repair_synthesizes_missing_tool_results() {
        use super::repair_unanswered_tool_calls;
        // Abort between two calls in one batch: t1 answered, t2 dangling.
        let mut msgs = vec![
            AgentMessage::user("go"),
            assistant_with_calls(&["t1", "t2"]),
            tool_result("t1"),
            AgentMessage::user("again"),
        ];
        repair_unanswered_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), 5, "one synthesized result inserted");
        let synth = match &msgs[3] {
            AgentMessage::ToolResult(tr) if tr.tool_call_id == "t2" => tr,
            other => panic!("expected synthesized t2 result, got {other:?}"),
        };
        assert!(synth.is_error);
        assert_eq!(synth.tool_name, "write");
        // Inserted after the real result, before the next user message.
        assert!(matches!(&msgs[2], AgentMessage::ToolResult(tr) if tr.tool_call_id == "t1"));
        assert!(matches!(&msgs[4], AgentMessage::User(_)));
    }

    #[test]
    fn repair_noop_when_all_answered() {
        use super::repair_unanswered_tool_calls;
        let mut msgs = vec![
            AgentMessage::user("go"),
            assistant_with_calls(&["t1", "t2"]),
            tool_result("t1"),
            tool_result("t2"),
        ];
        let before = msgs.clone();
        repair_unanswered_tool_calls(&mut msgs);
        assert_eq!(msgs, before);
    }

    #[test]
    fn repair_handles_dangling_tail() {
        use super::repair_unanswered_tool_calls;
        // Crash mid-execution: the assistant with tool calls is the last
        // message in the log.
        let mut msgs = vec![
            AgentMessage::user("go"),
            assistant_with_calls(&["t1", "t2"]),
            tool_result("t1"),
        ];
        repair_unanswered_tool_calls(&mut msgs);
        assert!(
            matches!(&msgs[3], AgentMessage::ToolResult(tr) if tr.tool_call_id == "t2" && tr.is_error)
        );
    }

    // ── SessionEvent::from_message ────────────────────────────────

    #[test]
    fn from_message_maps_a_user_message() {
        let msg = AgentMessage::user("hi");
        let ev = SessionEvent::from_message(&msg, None).expect("user maps to an event");
        assert_eq!(ev, SessionEvent::User(msg.clone()));
    }

    #[test]
    fn from_message_maps_an_assistant_message_with_its_usage() {
        let msg = AgentMessage::assistant_text("hello");
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: Some(3),
            cache_write_tokens: None,
        };
        let ev = SessionEvent::from_message(&msg, Some(usage)).expect("assistant maps to an event");
        match ev {
            SessionEvent::Assistant {
                message,
                usage: got,
            } => {
                assert_eq!(message, msg);
                assert_eq!(got, Some(usage));
            }
            other => panic!("expected an assistant event, got {other:?}"),
        }
    }

    #[test]
    fn from_message_maps_an_assistant_message_without_usage() {
        let msg = AgentMessage::assistant_text("hello");
        let ev = SessionEvent::from_message(&msg, None).expect("assistant maps to an event");
        match ev {
            SessionEvent::Assistant { message, usage } => {
                assert_eq!(message, msg);
                assert_eq!(usage, None);
            }
            other => panic!("expected an assistant event, got {other:?}"),
        }
    }

    #[test]
    fn from_message_maps_a_tool_result_message() {
        let msg = tool_result("t1");
        let ev = SessionEvent::from_message(&msg, None).expect("tool result maps to an event");
        assert_eq!(ev, SessionEvent::ToolResult(msg));
    }

    #[test]
    fn from_message_drops_custom_messages() {
        // Custom messages are plugin-private: they never reach the log.
        let msg = AgentMessage::Custom(crate::types::message::CustomMessage {
            custom_type: "todo.list".to_string(),
            content: serde_json::json!({}),
            timestamp: 0,
        });
        assert!(SessionEvent::from_message(&msg, None).is_none());
    }

    #[test]
    fn from_message_ignores_the_usage_for_non_assistant_messages() {
        // Usage belongs to the assistant row only; a user row carries none.
        let msg = AgentMessage::user("hi");
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: None,
            cache_write_tokens: None,
        };
        let ev = SessionEvent::from_message(&msg, Some(usage)).expect("user maps to an event");
        assert!(matches!(ev, SessionEvent::User(_)));
    }

    // ── is_checkpoint ─────────────────────────────────────────────

    #[test]
    fn is_checkpoint_is_true_for_cleared_and_compacted() {
        assert!(super::is_checkpoint(&SessionEvent::Cleared));
        assert!(super::is_checkpoint(&SessionEvent::Compacted {
            base: vec![],
            dropped: 0,
        }));
    }

    #[test]
    fn is_checkpoint_is_false_for_every_other_event() {
        assert!(!super::is_checkpoint(&SessionEvent::TurnStart));
        assert!(!super::is_checkpoint(&SessionEvent::TurnEnd {
            reason: TurnEndReason::Completed
        }));
        assert!(!super::is_checkpoint(&SessionEvent::User(
            AgentMessage::user("hi")
        )));
        assert!(!super::is_checkpoint(&SessionEvent::Assistant {
            message: AgentMessage::assistant_text("hi"),
            usage: None,
        }));
        assert!(!super::is_checkpoint(&SessionEvent::ToolResult(
            tool_result("t1")
        )));
    }

    // ── surface ───────────────────────────────────────────────────

    #[test]
    fn surface_yields_the_message_of_the_message_events() {
        let user = AgentMessage::user("hi");
        let assistant = AgentMessage::assistant_text("yo");
        let result = tool_result("t1");
        assert_eq!(
            super::surface(&SessionEvent::User(user.clone())),
            Some(user)
        );
        assert_eq!(
            super::surface(&SessionEvent::Assistant {
                message: assistant.clone(),
                usage: None,
            }),
            Some(assistant)
        );
        assert_eq!(
            super::surface(&SessionEvent::ToolResult(result.clone())),
            Some(result)
        );
    }

    #[test]
    fn surface_yields_nothing_for_the_control_events() {
        assert_eq!(super::surface(&SessionEvent::TurnStart), None);
        assert_eq!(
            super::surface(&SessionEvent::TurnEnd {
                reason: TurnEndReason::Completed
            }),
            None
        );
        assert_eq!(super::surface(&SessionEvent::Cleared), None);
        assert_eq!(
            super::surface(&SessionEvent::Compacted {
                base: vec![AgentMessage::user("pinned")],
                dropped: 3,
            }),
            None
        );
    }

    #[test]
    fn surface_keeps_the_compacted_base_out_of_the_message_view() {
        // The checkpoint's frozen base is a projection input, not a message
        // row of its own: it must not leak into the derived message list.
        let ev = SessionEvent::Compacted {
            base: vec![AgentMessage::user("pinned")],
            dropped: 1,
        };
        assert!(super::surface(&ev).is_none());
    }
}
