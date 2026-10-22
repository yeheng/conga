//! In-memory history compaction.
//!
//! Pure host policy: shrinks the working transcript before `run_agent_loop`.
//! Does not rewrite JSONL, no LLM summary. One algorithm ([`compact_by_count`]:
//! drop oldest whole atomic groups, prepend a notice); two triggers via
//! [`ContextBudget`]: token-aware (provider-reported `usage.input_tokens`
//! over `threshold_pct` of `window`) or a message-count fallback.

use conga::{AgentMessage, ContentBlock, UserMessage};

/// Old ToolResult text blocks are head-truncated to this many chars during
/// compaction (the newest exchange stays verbatim).
const TRUNCATED_TOOL_RESULT_CHARS: usize = 400;

/// Default cap on working history size (message count, not tokens).
pub const DEFAULT_MAX_MESSAGES: usize = 80;

/// Read `CONGA_COMPACT_MAX_MESSAGES` or [`DEFAULT_MAX_MESSAGES`].
/// `0` means "do not compact".
pub fn max_messages_from_env() -> usize {
    max_messages_from(&|k| std::env::var(k))
}

pub fn max_messages_from(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> usize {
    lookup("CONGA_COMPACT_MAX_MESSAGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_MESSAGES)
}

/// Partition `messages` into atomic `[start, end)` groups that must never be
/// split across a compaction boundary. An `Assistant` opens a group and
/// absorbs any immediately-following `ToolResult` messages into the same
/// group; every other message forms its own singleton group.
fn atomic_groups(messages: &[AgentMessage]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let start = i;
        if matches!(messages[i], AgentMessage::Assistant(_)) {
            i += 1;
            while i < messages.len() && matches!(messages[i], AgentMessage::ToolResult(_)) {
                i += 1;
            }
        } else {
            i += 1;
        }
        groups.push((start, i));
    }
    groups
}

/// Compaction result: the no-op path borrows the input instead of cloning
/// the whole history on every LLM call.
pub enum Compacted<'a> {
    /// Under budget: the input itself is the view; nothing was cloned.
    Borrowed(&'a [AgentMessage]),
    /// Compacted: a shorter, owned history.
    Owned(Vec<AgentMessage>),
}

impl<'a> Compacted<'a> {
    pub fn as_slice(&self) -> &[AgentMessage] {
        match self {
            Compacted::Borrowed(s) => s,
            Compacted::Owned(v) => v,
        }
    }

    /// Flatten to an owned `Vec` (the loop's `transform_context` seam
    /// returns a `Vec` by contract).
    pub fn into_owned(self) -> Vec<AgentMessage> {
        match self {
            Compacted::Borrowed(s) => s.to_vec(),
            Compacted::Owned(v) => v,
        }
    }
}

/// Core group-walking algorithm; `None` = nothing to drop (under budget,
/// `max_messages == 0`, or the walk kept everything).
fn compacted(messages: &[AgentMessage], max_messages: usize) -> Option<Vec<AgentMessage>> {
    if max_messages == 0 || messages.len() <= max_messages {
        return None;
    }

    let groups = atomic_groups(messages);
    let budget = max_messages.saturating_sub(1);

    // Walk groups from newest backwards, accumulating whole groups until the
    // running message count would exceed the budget. Always keep at least the
    // final group. The FIRST group is exempt from the budget: keep it even
    // when its size alone exceeds what's left.
    let mut kept_msg_count = 0;
    let mut first_kept_group = groups.len();
    for (idx, &(start, end)) in groups.iter().enumerate().rev() {
        let group_len = end - start;
        if idx == 0 {
            // Pin the original task; it rides outside the budget.
            kept_msg_count += group_len;
            first_kept_group = 0;
            break;
        }
        if idx < groups.len() - 1 && kept_msg_count + group_len > budget {
            break;
        }
        kept_msg_count += group_len;
        first_kept_group = idx;
    }

    let start = groups[first_kept_group].0;
    // The pinned first group (groups[0]) stays; only what lies between it
    // and `start` was actually dropped.
    let pinned_len = groups[0].1 - groups[0].0;
    let dropped = start.saturating_sub(pinned_len);
    if dropped == 0 {
        return None;
    }

    let mut out: Vec<AgentMessage> = Vec::with_capacity(kept_msg_count + 1);
    out.push(messages[0].clone()); // pinned original task
    out.push(AgentMessage::User(UserMessage {
        content: vec![ContentBlock::text(format!(
            "[compacted {dropped} earlier messages; original task kept]"
        ))],
        timestamp: conga::now(),
    }));
    // Truncate old tool results (everything before the final group) to a
    // head preview; the newest exchange stays verbatim.
    let newest_group_start = groups[groups.len() - 1].0;
    for (i, m) in messages[start..].iter().enumerate() {
        let abs = start + i;
        if abs < newest_group_start {
            out.push(truncate_tool_result(m));
        } else {
            out.push(m.clone());
        }
    }
    Some(out)
}

/// If `messages.len() > max_messages`, keep the newest groups whose total
/// message count fits in `max_messages - 1` (one slot reserved for the
/// summary notice) and prepend one user notice naming how many were dropped.
/// Groups are kept whole, so an `Assistant(tool_call)` is never separated from
/// its trailing `ToolResult`s.
///
/// Harness invariants on top of the plain drop-oldest walk:
/// - The FIRST group (the original task) is never dropped — an agent that
///   forgets why it started is worse than an agent with a shorter window.
/// - Before dropping anything, ToolResult text blocks OUTSIDE the newest
///   groups are truncated to a head preview: old tool output is usually
///   spent; the tail is what the model still reasons over.
///
/// Under budget (or `max_messages == 0`): clone unchanged.
pub fn compact_by_count(messages: &[AgentMessage], max_messages: usize) -> Vec<AgentMessage> {
    compacted(messages, max_messages).unwrap_or_else(|| messages.to_vec())
}

/// Borrowed-when-unchanged twin of [`compact_by_count`].
fn compact_by_count_view<'a>(messages: &'a [AgentMessage], max_messages: usize) -> Compacted<'a> {
    match compacted(messages, max_messages) {
        Some(v) => Compacted::Owned(v),
        None => Compacted::Borrowed(messages),
    }
}

/// Head-truncate a message's ToolResult text blocks. Other message kinds
/// pass through untouched.
fn truncate_tool_result(m: &AgentMessage) -> AgentMessage {
    match m {
        AgentMessage::ToolResult(tr) => {
            let mut tr = tr.clone();
            for b in tr.content.iter_mut() {
                if let ContentBlock::Text { text } = b {
                    if text.chars().count() > TRUNCATED_TOOL_RESULT_CHARS {
                        let head: String = text.chars().take(TRUNCATED_TOOL_RESULT_CHARS).collect();
                        *text = format!("{head}\n[... output truncated by compaction ...]");
                    }
                }
            }
            AgentMessage::ToolResult(tr)
        }
        other => other.clone(),
    }
}

/// parse failure. Mirrors the helper in `conga/src/types/context.rs` so
/// `compact.rs` stays self-contained.
fn env_parse<T: std::str::FromStr>(
    lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    key: &str,
    default: T,
) -> T {
    lookup(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Token-aware compaction trigger with hysteresis.
///
/// Uses the provider-reported `usage.input_tokens` (fed in via
/// [`record_input_tokens`](Self::record_input_tokens)) to decide when the
/// working transcript has grown past `threshold_pct` of the context `window`.
/// When over threshold, [`compact`](Self::compact) retains `target_pct`% of
/// the current messages — a proportional reduction. Core ships no tokenizer,
/// so it does not pretend to model per-message token cost; it reuses
/// [`compact_by_count`]'s greedy group-walking algorithm for both paths.
///
/// When no usage has been recorded (`last_input_tokens == 0`), compaction
/// falls back to a fixed message-count cap (`fallback_max_messages`).
#[derive(Clone)]
pub struct ContextBudget {
    /// Model context window (`CONGA_CONTEXT_WINDOW`, default 128_000).
    window: u64,
    /// Compaction trigger as a percentage of `window`
    /// (`CONGA_COMPACT_THRESHOLD_PCT`, default 80).
    threshold_pct: u8,
    /// Post-compaction retention as a percentage of the current message count
    /// (`CONGA_COMPACT_TARGET_PCT`, default 50).
    target_pct: u8,
    /// Message-count fallback when no usage is available
    /// (`CONGA_COMPACT_MAX_MESSAGES`, default 80).
    fallback_max_messages: usize,
    /// Most recent `usage.input_tokens` reported by the provider.
    last_input_tokens: u64,
}

impl ContextBudget {
    /// Read all knobs from the process environment.
    pub fn from_env() -> Self {
        Self::from_env_with(&|k| std::env::var(k))
    }

    /// Same as [`from_env`](Self::from_env) but with an injectable lookup -
    /// used by tests to avoid mutating process env.
    pub fn from_env_with(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Self {
        Self {
            window: env_parse(lookup, "CONGA_CONTEXT_WINDOW", 128_000),
            threshold_pct: env_parse(lookup, "CONGA_COMPACT_THRESHOLD_PCT", 80),
            target_pct: env_parse(lookup, "CONGA_COMPACT_TARGET_PCT", 50),
            fallback_max_messages: max_messages_from(lookup),
            last_input_tokens: 0,
        }
    }

    /// Feed the provider-reported input-token count for this turn.
    pub fn record_input_tokens(&mut self, n: u64) {
        self.last_input_tokens = n;
    }

    /// Override the context window (settings.json `maxTokens` beats the
    /// env-derived value). `run_turn` re-reads the settings file every
    /// turn and calls this, so a dialog save applies to the very next
    /// turn without a restart.
    pub fn set_window(&mut self, n: u64) {
        self.window = n;
    }

    /// The configured context window (env/settings-resolved).
    pub fn window(&self) -> u64 {
        self.window
    }
    /// Current input-token occupancy (most recent provider report).
    pub fn current_tokens(&self) -> u64 {
        self.last_input_tokens
    }

    /// True when `last_input_tokens` exceeds `threshold_pct` of `window`.
    pub fn needs_compaction(&self) -> bool {
        self.last_input_tokens > self.window * self.threshold_pct as u64 / 100
    }
    /// Compact `messages` according to the budget. The unchanged (no-op)
    /// paths borrow the input — [`Compacted::Borrowed`] — instead of
    /// cloning the whole history on every LLM call.
    ///
    /// - No usage recorded: fall back to [`compact_by_count`] with
    ///   `fallback_max_messages`.
    /// - Under threshold: return unchanged (borrowed).
    /// - Over threshold: [`compact_by_count`] retaining `target_pct`% of the
    ///   current message count. One algorithm, two triggers.
    pub fn compact<'a>(&self, messages: &'a [AgentMessage]) -> Compacted<'a> {
        if self.last_input_tokens == 0 {
            return compact_by_count_view(messages, self.fallback_max_messages);
        }
        if !self.needs_compaction() {
            return Compacted::Borrowed(messages);
        }
        // Token pressure triggered. Core has no tokenizer, so a proportional
        // message-count reduction is the honest target — it reuses the same
        // greedy group-walker as the count-based path instead of a second
        // compaction strategy that would pretend to know per-group token cost.
        let target = (messages.len() * self.target_pct as usize / 100).max(1);
        compact_by_count_view(messages, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::types::message::{FunctionCall, ToolCall};
    use conga::{AssistantMessage, StopReason, ToolResultMessage};

    fn user(s: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(s)],
            timestamp: 1,
        })
    }

    fn assistant(s: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::text(s)],
            model: "m".into(),
            stop_reason: StopReason::EndTurn,
            usage: None,
            timestamp: 1,
            stream_indices: Vec::new(),
        })
    }

    /// Assistant carrying a single tool call with the given id.
    fn assistant_with_tool(id: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: id.into(),
                    function: FunctionCall {
                        name: "f".into(),
                        arguments: "{}".into(),
                    },
                },
            }],
            model: "m".into(),
            stop_reason: StopReason::ToolUse,
            usage: None,
            timestamp: 1,
            stream_indices: Vec::new(),
        })
    }

    /// ToolResult answering `tool_call_id`.
    fn result(id: &str) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "f".into(),
            content: vec![ContentBlock::text("ok")],
            is_error: false,
            timestamp: 1,
        })
    }

    /// ToolResult whose single text block carries `text`.
    fn result_with_text(id: &str, text: &str) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "f".into(),
            content: vec![ContentBlock::text(text)],
            is_error: false,
            timestamp: 1,
        })
    }

    /// The text of a message's first text block (test helper).
    fn text_of(m: &AgentMessage) -> String {
        match m {
            AgentMessage::User(u) => first_text(&u.content),
            AgentMessage::Assistant(a) => first_text(&a.content),
            AgentMessage::ToolResult(t) => first_text(&t.content),
            AgentMessage::Custom(_) => String::new(),
        }
    }

    fn first_text(blocks: &[ContentBlock]) -> String {
        blocks
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Injectable env lookup — never touches the process environment.
    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    #[test]
    fn under_budget_unchanged() {
        let msgs = vec![user("a"), assistant("b")];
        let out = compact_by_count(&msgs, 10);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], AgentMessage::User(_)));
    }

    #[test]
    fn exactly_at_limit_unchanged() {
        let msgs: Vec<_> = (0..5).map(|i| user(&format!("{i}"))).collect();
        let out = compact_by_count(&msgs, 5);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn over_budget_shrinks_and_notices() {
        let msgs: Vec<_> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        let out = compact_by_count(&msgs, 4);
        // 1 pinned task + 1 notice + 3 kept = 5
        assert_eq!(out.len(), 5);
        match &out[0] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "m0"),
                _ => panic!(),
            },
            _ => panic!("expected pinned original task"),
        }
        match &out[1] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => {
                    assert!(text.contains("compacted 6 earlier messages"));
                }
                _ => panic!(),
            },
            _ => panic!("expected summary user message"),
        }
        // Tail preserved: m7, m8, m9
        match &out[4] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "m9"),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn max_zero_is_noop() {
        let msgs = vec![user("a"), user("b"), user("c")];
        assert_eq!(compact_by_count(&msgs, 0).len(), 3);
    }

    #[test]
    fn max_one_keeps_last_group_plus_notice() {
        // max=1 leaves zero budget for real messages, but the last atomic
        // group is always kept (never dropped entirely), so the result is the
        // summary notice followed by the final message.
        let msgs = vec![user("a"), user("b")];
        let out = compact_by_count(&msgs, 1);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], AgentMessage::User(_)));
        match &out[1] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "b"),
                _ => panic!(),
            },
            _ => panic!("expected last message preserved"),
        }
    }

    #[test]
    fn env_default() {
        assert_eq!(
            max_messages_from(&|_| Err(std::env::VarError::NotPresent)),
            DEFAULT_MAX_MESSAGES
        );
        assert_eq!(
            max_messages_from(&|k| {
                if k == "CONGA_COMPACT_MAX_MESSAGES" {
                    Ok("12".into())
                } else {
                    Err(std::env::VarError::NotPresent)
                }
            }),
            12
        );
    }

    #[test]
    fn never_splits_tool_call_from_result() {
        // [user, asst(tc=t1), result(t1), user, asst(tc=t2), result(t2), user]
        let msgs = vec![
            user("u1"),
            assistant_with_tool("t1"),
            result("t1"),
            user("u2"),
            assistant_with_tool("t2"),
            result("t2"),
            user("u3"),
        ];
        let out = compact_by_count(&msgs, 4);

        // Collect tool_call ids and result ids from the kept tail (skip the
        // leading summary notice).
        let mut call_ids: Vec<String> = Vec::new();
        let mut result_ids: Vec<String> = Vec::new();
        for m in out.iter().skip(1) {
            match m {
                AgentMessage::Assistant(a) => {
                    for b in &a.content {
                        if let ContentBlock::ToolCall { tool_call } = b {
                            call_ids.push(tool_call.id.clone());
                        }
                    }
                }
                AgentMessage::ToolResult(r) => result_ids.push(r.tool_call_id.clone()),
                _ => {}
            }
        }
        // Every tool call has a matching result and vice versa: no orphans.
        for id in &call_ids {
            assert!(
                result_ids.contains(id),
                "orphan tool_call {id} has no result"
            );
        }
        for id in &result_ids {
            assert!(call_ids.contains(id), "orphan tool_result {id} has no call");
        }
    }

    #[test]
    fn needs_compaction_uses_real_tokens() {
        let mut budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 80,
            last_input_tokens: 0,
        };
        budget.record_input_tokens(70_000);
        assert!(!budget.needs_compaction());
        budget.record_input_tokens(85_000);
        assert!(budget.needs_compaction());
    }

    #[test]
    fn compact_drops_groups_under_target() {
        let msgs: Vec<_> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        let budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 80,
            last_input_tokens: 100_000,
        };
        let out = budget.compact(&msgs).into_owned();
        // Token-triggered → retain target_pct% (50%) of 10 messages as the
        // count budget = 5; compact_by_count keeps 4 newest (m6..m9) under
        // the notice slot, plus the pinned original task (m0) = 6 total,
        // dropping 5 (m1..m5).
        assert_eq!(out.len(), 6);
        match &out[0] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => assert!(text.contains("m0"), "original task pinned"),
                _ => panic!(),
            },
            _ => panic!("expected pinned original task"),
        }
        match &out[1] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => assert!(text.contains("compacted 5")),
                _ => panic!(),
            },
            _ => panic!("expected summary user message"),
        }
    }

    #[test]
    fn compact_falls_back_when_no_usage() {
        let msgs: Vec<_> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        let budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 4,
            last_input_tokens: 0,
        };
        let out = budget.compact(&msgs).into_owned();
        // fallback: compact_by_count(msgs, 4) = 1 pinned task + 1 notice
        // + 3 kept = 5
        assert_eq!(out.len(), 5);
        assert!(matches!(&out[0], AgentMessage::User(u)
            if matches!(&u.content[0], ContentBlock::Text { text } if text.contains("m0"))));
    }

    /// `set_window` is the settings.json `maxTokens` override: an
    /// under-budget occupancy against the env window becomes over-budget
    /// once a smaller user window is applied (run_turn calls this every
    /// turn after re-reading the settings file).
    #[test]
    fn set_window_overrides_env_derived_threshold() {
        let mut budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 80,
            last_input_tokens: 50_000,
        };
        assert_eq!(budget.window(), 100_000);
        assert!(!budget.needs_compaction(), "50k of 100k is under 80%");
        budget.set_window(50_000);
        assert_eq!(budget.window(), 50_000);
        assert!(
            budget.needs_compaction(),
            "50k occupancy against a 50k window is over the 80% trigger"
        );
    }
    #[test]
    fn compact_under_threshold_is_noop() {
        let msgs: Vec<_> = (0..5).map(|i| user(&format!("m{i}"))).collect();
        let budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 80,
            last_input_tokens: 50_000,
        };
        let out = budget.compact(&msgs);
        // Under threshold the view BORROWS the input: same slice, zero
        // clones — not a fresh copy of the whole history.
        match out {
            Compacted::Borrowed(slice) => {
                assert!(std::ptr::eq(slice.as_ptr(), msgs.as_ptr()));
                assert_eq!(slice.len(), 5);
            }
            Compacted::Owned(v) => panic!("under-threshold compact must borrow, got owned {v:?}"),
        }
        // And the flattened view still reads as the original history.
        let flat = budget.compact(&msgs).into_owned();
        assert_eq!(flat.len(), 5);
        match &flat[0] {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "m0"),
                _ => panic!(),
            },
            _ => panic!("expected original first message"),
        }
    }

    /// Same borrowing contract on the count-fallback trigger (no recorded
    /// usage): under the fallback cap there is nothing to drop.
    #[test]
    fn compact_fallback_under_cap_borrows() {
        let msgs: Vec<_> = (0..3).map(|i| user(&format!("m{i}"))).collect();
        let budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 80,
            last_input_tokens: 0,
        };
        match budget.compact(&msgs) {
            Compacted::Borrowed(slice) => {
                assert!(std::ptr::eq(slice.as_ptr(), msgs.as_ptr()))
            }
            Compacted::Owned(v) => panic!("must borrow under the cap, got owned {v:?}"),
        }
    }

    #[test]
    fn compact_never_splits_tool_pair() {
        // [user, asst(tc=t1), result(t1), user, asst(tc=t2), result(t2), user]
        let msgs = vec![
            user("u1"),
            assistant_with_tool("t1"),
            result("t1"),
            user("u2"),
            assistant_with_tool("t2"),
            result("t2"),
            user("u3"),
        ];
        let budget = ContextBudget {
            window: 100_000,
            threshold_pct: 80,
            target_pct: 50,
            fallback_max_messages: 80,
            last_input_tokens: 100_000,
        };
        let out = budget.compact(&msgs).into_owned();

        // Collect tool_call ids and result ids from the kept tail (skip the
        // leading summary notice).
        let mut call_ids: Vec<String> = Vec::new();
        let mut result_ids: Vec<String> = Vec::new();
        for m in out.iter().skip(1) {
            match m {
                AgentMessage::Assistant(a) => {
                    for b in &a.content {
                        if let ContentBlock::ToolCall { tool_call } = b {
                            call_ids.push(tool_call.id.clone());
                        }
                    }
                }
                AgentMessage::ToolResult(r) => result_ids.push(r.tool_call_id.clone()),
                _ => {}
            }
        }
        // Every tool call has a matching result and vice versa: no orphans.
        for id in &call_ids {
            assert!(
                result_ids.contains(id),
                "orphan tool_call {id} has no result"
            );
        }
        for id in &result_ids {
            assert!(call_ids.contains(id), "orphan tool_result {id} has no call");
        }
    }

    // ── env parsing (injected lookups only) ──

    /// `max_messages_from_env` reads the process environment; the only
    /// hermetic assertion is that it agrees with what that environment says.
    #[test]
    fn max_messages_from_env_reads_the_process_environment() {
        let expected = std::env::var("CONGA_COMPACT_MAX_MESSAGES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_MESSAGES);
        assert_eq!(max_messages_from_env(), expected);
    }

    #[test]
    fn max_messages_from_falls_back_on_unusable_values() {
        // Unset → default.
        assert_eq!(
            max_messages_from(&fake_env(&[])),
            DEFAULT_MAX_MESSAGES,
            "unset must fall back to the default"
        );
        // Empty / non-numeric / negative / whitespace-padded → default.
        for raw in ["", "abc", "-1", " 12 ", "12.5", "0x10"] {
            assert_eq!(
                max_messages_from(&fake_env(&[("CONGA_COMPACT_MAX_MESSAGES", raw)])),
                DEFAULT_MAX_MESSAGES,
                "value {raw:?} must fall back to the default"
            );
        }
        // "0" is a legal value: it means "never compact".
        assert_eq!(
            max_messages_from(&fake_env(&[("CONGA_COMPACT_MAX_MESSAGES", "0")])),
            0
        );
        // A huge value is accepted verbatim.
        assert_eq!(
            max_messages_from(&fake_env(&[("CONGA_COMPACT_MAX_MESSAGES", "100000")])),
            100_000
        );
    }

    #[test]
    fn env_parse_reads_the_value_or_returns_the_default() {
        assert_eq!(env_parse(&fake_env(&[]), "K", 7u32), 7);
        assert_eq!(env_parse(&fake_env(&[("K", "9")]), "K", 7u32), 9);
        assert_eq!(env_parse(&fake_env(&[("K", "nope")]), "K", 7u32), 7);
        // A different key is not read.
        assert_eq!(env_parse(&fake_env(&[("OTHER", "9")]), "K", 7u32), 7);
        // Generic over the parsed type.
        assert_eq!(
            env_parse(&fake_env(&[("K", "s")]), "K", "d".to_string()),
            "s".to_string()
        );
    }

    // ── atomic grouping ──

    #[test]
    fn atomic_groups_keeps_an_assistant_with_its_tool_results() {
        let msgs = vec![
            user("u1"),
            assistant_with_tool("t1"),
            result("t1"),
            result("t2"),
            user("u2"),
            assistant("a2"),
        ];
        assert_eq!(atomic_groups(&msgs), vec![(0, 1), (1, 4), (4, 5), (5, 6)]);
    }

    #[test]
    fn atomic_groups_on_empty_input_has_no_groups() {
        assert!(atomic_groups(&[]).is_empty());
    }

    #[test]
    fn atomic_groups_treats_a_leading_tool_result_as_its_own_group() {
        let msgs = vec![result("t1"), user("u")];
        assert_eq!(atomic_groups(&msgs), vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn atomic_groups_splits_two_assistants_without_tool_results() {
        let msgs = vec![assistant("a1"), assistant("a2")];
        assert_eq!(atomic_groups(&msgs), vec![(0, 1), (1, 2)]);
    }

    // ── the compaction walk ──

    #[test]
    fn compacted_returns_none_when_nothing_can_be_dropped() {
        let msgs = vec![user("a"), user("b")];
        // Under budget.
        assert!(compacted(&msgs, 5).is_none());
        // Exactly at the limit.
        assert!(compacted(&msgs, 2).is_none());
        // max == 0 disables compaction.
        assert!(compacted(&msgs, 0).is_none());
        // Over budget, but only the pinned first group would be dropped.
        assert!(compacted(&msgs, 1).is_none());
    }

    #[test]
    fn compacted_drops_the_oldest_groups_and_prepends_a_notice() {
        let msgs: Vec<_> = (0..6).map(|i| user(&format!("m{i}"))).collect();
        let out = compacted(&msgs, 3).expect("must compact");
        // pinned task + notice + the two newest messages.
        assert_eq!(out.len(), 4);
        assert_eq!(text_of(&out[0]), "m0", "original task is pinned");
        assert!(
            text_of(&out[1]).contains("compacted 3 earlier messages"),
            "notice: {}",
            text_of(&out[1])
        );
        assert_eq!(text_of(&out[2]), "m4");
        assert_eq!(text_of(&out[3]), "m5");
    }

    #[test]
    fn compacted_keeps_the_whole_last_atomic_group() {
        // [user, asst(tc), result, user] — the trailing user is its own group.
        let msgs = vec![
            user("u1"),
            assistant_with_tool("t1"),
            result("t1"),
            user("u2"),
        ];
        let out = compacted(&msgs, 2).expect("must compact");
        // pinned + notice + the last group.
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[2], AgentMessage::User(_)));
        assert_eq!(text_of(&out[2]), "u2");
    }

    /// KNOWN BUG (reported, not fixed): the "pinned original task" is pushed
    /// as `messages[0]` only — the REST of the pinned group is dropped. When
    /// the transcript starts with an `Assistant` message, the pinned group is
    /// `[assistant, tool_result, ...]`, so the tool result disappears and
    /// the assistant is left with an ORPHAN tool call. The notice also
    /// under-counts the dropped messages (`dropped` subtracts the pinned
    /// group's length, but only its first message is actually kept).
    ///
    /// This test pins the CURRENT behaviour so the bug stays visible; it
    /// must be updated when the bug is fixed.
    #[test]
    fn pinned_first_group_keeps_only_its_first_message_known_bug() {
        let msgs = vec![
            assistant_with_tool("t1"),
            result("t1"),
            user("u2"),
            user("u3"),
            user("u4"),
        ];
        let out = compact_by_count(&msgs, 3);
        // The pinned assistant (with its tool call) survives...
        match &out[0] {
            AgentMessage::Assistant(a) => assert!(a
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { .. }))),
            other => panic!("expected the pinned assistant, got {other:?}"),
        }
        // ...but its tool result is gone → an orphan tool call.
        let result_ids: Vec<String> = out
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolResult(r) => Some(r.tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert!(
            result_ids.is_empty(),
            "the pinned group's tool result was dropped: {result_ids:?}"
        );
        // Two messages were dropped (the tool result and u2), but the notice
        // counts only one.
        assert!(
            text_of(&out[1]).contains("compacted 1 earlier messages"),
            "notice under-counts: {}",
            text_of(&out[1])
        );
    }

    #[test]
    fn compaction_truncates_old_tool_results_but_keeps_the_newest_exchange() {
        let long = "z".repeat(1_000);
        let msgs = vec![
            user("u1"),
            assistant_with_tool("t1"),
            result_with_text("t1", "old"),
            user("u2"),
            assistant_with_tool("t2"),
            result_with_text("t2", &long),
            user("u3"),
            assistant_with_tool("t3"),
            result_with_text("t3", "newest"),
        ];
        let out = compact_by_count(&msgs, 7);
        // pinned + notice + messages[3..]
        assert_eq!(out.len(), 8);
        // The old tool result (index 5) is truncated to a head preview.
        let truncated = text_of(&out[4]);
        assert!(
            truncated.contains("output truncated by compaction"),
            "old tool result must be truncated: {truncated}"
        );
        assert!(truncated.starts_with(&"z".repeat(TRUNCATED_TOOL_RESULT_CHARS)));
        // The newest exchange stays verbatim.
        assert_eq!(text_of(&out[7]), "newest");
    }

    // ── the borrowed / owned view ──

    #[test]
    fn as_slice_exposes_the_borrowed_and_the_owned_messages() {
        let msgs = vec![user("a"), user("b")];
        let borrowed = Compacted::Borrowed(&msgs);
        assert_eq!(borrowed.as_slice().len(), 2);
        assert!(std::ptr::eq(borrowed.as_slice().as_ptr(), msgs.as_ptr()));

        let owned = Compacted::Owned(vec![user("x"), user("y"), user("z")]);
        assert_eq!(owned.as_slice().len(), 3);
        assert_eq!(text_of(&owned.as_slice()[2]), "z");
    }

    #[test]
    fn compact_by_count_view_borrows_when_unchanged_and_owns_after_compaction() {
        let msgs: Vec<_> = (0..6).map(|i| user(&format!("m{i}"))).collect();
        match compact_by_count_view(&msgs, 10) {
            Compacted::Borrowed(s) => {
                assert!(std::ptr::eq(s.as_ptr(), msgs.as_ptr()));
                assert_eq!(s.len(), 6);
            }
            Compacted::Owned(v) => panic!("under budget must borrow, got owned {v:?}"),
        }
        match compact_by_count_view(&msgs, 3) {
            Compacted::Owned(v) => assert_eq!(v.len(), 4),
            Compacted::Borrowed(s) => panic!("over budget must own, got borrowed {s:?}"),
        }
    }

    // ── tool-result truncation ──

    #[test]
    fn truncate_tool_result_shortens_long_text_and_marks_it() {
        let long = "x".repeat(TRUNCATED_TOOL_RESULT_CHARS + 50);
        let out = truncate_tool_result(&result_with_text("t1", &long));
        let text = text_of(&out);
        assert!(
            text.starts_with(&"x".repeat(TRUNCATED_TOOL_RESULT_CHARS)),
            "head must be preserved"
        );
        assert!(
            text.contains("output truncated by compaction"),
            "truncation must be marked: {text}"
        );
    }

    #[test]
    fn truncate_tool_result_keeps_text_at_or_below_the_limit_untouched() {
        let exact = "y".repeat(TRUNCATED_TOOL_RESULT_CHARS);
        let text = text_of(&truncate_tool_result(&result_with_text("t1", &exact)));
        assert_eq!(text, exact, "exactly at the limit must not be truncated");

        let short = "ok";
        let text = text_of(&truncate_tool_result(&result_with_text("t1", short)));
        assert_eq!(text, short);

        let empty = text_of(&truncate_tool_result(&result_with_text("t1", "")));
        assert_eq!(empty, "");
    }

    #[test]
    fn truncate_tool_result_passes_other_messages_through() {
        let out = truncate_tool_result(&user("hello"));
        assert!(matches!(&out, AgentMessage::User(_)));
        assert_eq!(text_of(&out), "hello");
    }

    // ── budget knobs ──

    #[test]
    fn from_env_with_uses_the_documented_defaults() {
        let b = ContextBudget::from_env_with(&fake_env(&[]));
        assert_eq!(b.window(), 128_000);
        assert_eq!(b.threshold_pct, 80);
        assert_eq!(b.target_pct, 50);
        assert_eq!(b.fallback_max_messages, DEFAULT_MAX_MESSAGES);
        assert_eq!(b.current_tokens(), 0);
    }

    #[test]
    fn from_env_with_reads_every_knob() {
        let b = ContextBudget::from_env_with(&fake_env(&[
            ("CONGA_CONTEXT_WINDOW", "1000"),
            ("CONGA_COMPACT_THRESHOLD_PCT", "90"),
            ("CONGA_COMPACT_TARGET_PCT", "25"),
            ("CONGA_COMPACT_MAX_MESSAGES", "12"),
        ]));
        assert_eq!(b.window(), 1000);
        assert_eq!(b.threshold_pct, 90);
        assert_eq!(b.target_pct, 25);
        assert_eq!(b.fallback_max_messages, 12);
    }

    #[test]
    fn from_env_with_ignores_unusable_values() {
        let b = ContextBudget::from_env_with(&fake_env(&[
            ("CONGA_CONTEXT_WINDOW", "not-a-number"),
            ("CONGA_COMPACT_THRESHOLD_PCT", ""),
            ("CONGA_COMPACT_TARGET_PCT", "-5"),
            ("CONGA_COMPACT_MAX_MESSAGES", "abc"),
        ]));
        assert_eq!(b.window(), 128_000);
        assert_eq!(b.threshold_pct, 80);
        assert_eq!(b.target_pct, 50);
        assert_eq!(b.fallback_max_messages, DEFAULT_MAX_MESSAGES);
    }

    #[test]
    fn from_env_agrees_with_the_process_environment() {
        let b = ContextBudget::from_env();
        let expected_window = std::env::var("CONGA_CONTEXT_WINDOW")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(128_000);
        assert_eq!(b.window(), expected_window);
    }

    #[test]
    fn current_tokens_reports_the_last_recorded_value() {
        let mut b = ContextBudget::from_env_with(&fake_env(&[]));
        assert_eq!(b.current_tokens(), 0, "nothing recorded yet");
        b.record_input_tokens(1_234);
        assert_eq!(b.current_tokens(), 1_234);
        // A later report replaces the previous one.
        b.record_input_tokens(0);
        assert_eq!(b.current_tokens(), 0);
    }
}
