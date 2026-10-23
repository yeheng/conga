//! Offline end-to-end integration tests for the event-sourced host pipeline
//! (Host, ConfigLoader, SessionManager, PermissionPolicy, run_agent_loop +
//! persist callback, EventStorage) with a deterministic FakeStream: no
//! network, no LLM keys, CI-mandatory.
//!
//! The log under `<root>/<session>/events.jsonl` is the single source of
//! truth: `run_turn` derives history from it, the loop persists each
//! Assistant/ToolResult as it happens, and the host frames each turn with
//! TurnStart/User/TurnEnd. Legacy `messages.jsonl` migrates once, in full,
//! and is then deleted.
mod common;

use std::collections::VecDeque;
use std::sync::Arc;

use common::FakeStream;
use conga::types::message::{FunctionCall, ToolCall};
use conga::{
    derive_messages, AgentMessage, AssistantMessage, CancelCause, ContentBlock, EventStorage,
    SessionEvent, StopReason, StreamChunk, ToolResultMessage, TurnEndReason, UserMessage,
};
use conga_host::{
    ConfigLoader, EventPrinter, Host, HostConfig, Mode, PermissionPolicy, SessionManager,
    TurnSummary,
};
use parking_lot::Mutex;

fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
    let map: std::collections::HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
}

fn test_cfg(retry_off: bool) -> HostConfig {
    let mut pairs = vec![
        ("CONGA_LLM_BASE_URL", "https://api.test/v1"),
        ("CONGA_LLM_KEY", "sk-test"),
        ("CONGA_LLM_MODEL", "m"),
    ];
    if retry_off {
        pairs.push(("CONGA_RETRY_MAX", "0"));
    }
    ConfigLoader::load_with(&fake_env(&pairs)).unwrap()
}

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        content: vec![ContentBlock::text(text.to_string())],
        timestamp: conga::now(),
    })
}

fn full_auto_policy() -> PermissionPolicy {
    PermissionPolicy::new(Mode::FullAuto, Arc::new(|_, _| Box::pin(async { false })))
}

/// A `write` tool-call script chunk (args streamed as one delta, like the
/// FakeStream's other scripts).
fn write_call(id: &str, path: &str, content: &str) -> StreamChunk {
    StreamChunk::ToolCallDelta {
        index: None,
        id: id.into(),
        name: Some("write".into()),
        args_delta: serde_json::json!({ "path": path, "content": content }).to_string(),
    }
}

/// The reason of the log's only TurnEnd event.
fn turn_end_reason(events: &[SessionEvent]) -> TurnEndReason {
    let ends: Vec<&TurnEndReason> = events
        .iter()
        .filter_map(|ev| match ev {
            SessionEvent::TurnEnd { reason } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(ends.len(), 1, "expected exactly one TurnEnd, got {ends:?}");
    ends[0].clone()
}

/// The reason this plan exists: a tool side effect that already happened must
/// survive a turn whose second stream errors mid-flight. The log keeps every
/// fact (user, assistant, tool result) and ends with TurnEnd=Error — never a
/// silently adopted "fresh" session.
#[tokio::test]
async fn mid_turn_failure_preserves_side_effect() {
    let tmp = tempfile::tempdir().unwrap();
    // The `write` tool resolves within the process cwd, so the side-effect
    // target must live in a dir under it.
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = format!(
        "{}/side-effect.txt",
        work.path().file_name().unwrap().to_string_lossy()
    );

    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let fake = FakeStream::new(vec![
        vec![write_call("t1", &rel, "precious"), StreamChunk::Done],
        // Mid-stream error AFTER content was emitted: not retried (would
        // duplicate partial output), surfaced as stop_reason::Error.
        vec![
            StreamChunk::TextDelta("partial".into()),
            StreamChunk::Error("boom".into()),
        ],
    ]);
    let host = Host::new(
        test_cfg(true),
        session,
        Arc::new(full_auto_policy()),
        "sys".into(),
        conga_host::built_in_tools(),
    )
    .with_stream_fn(Arc::new(fake));

    let summary = host
        .run_turn("write the file please", |_| {})
        .await
        .expect("a mid-stream error surfaces as a message; it must not fail the turn");

    // The side effect happened — nothing may erase that fact.
    assert!(
        std::path::Path::new(&rel).exists(),
        "tool side effect must exist on disk after the failed turn"
    );
    assert!(
        matches!(summary.reason, TurnEndReason::Error { .. }),
        "expected TurnEnd::Error, got {:?}",
        summary.reason
    );

    // Reopen: the derived history keeps user + assistant + tool result.
    let sid = host.session().current_id().to_string();
    let reopen = SessionManager::with_root(tmp.path().to_path_buf());
    let events = reopen.open(&sid).await.unwrap();
    let msgs = derive_messages(&events);
    assert!(
        msgs.iter().any(|m| matches!(m, AgentMessage::User(_))),
        "user message missing from derived history"
    );
    assert!(
        msgs.iter().any(|m| matches!(m, AgentMessage::Assistant(_))),
        "assistant message missing from derived history"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(_))),
        "completed tool result missing from derived history"
    );
    assert!(matches!(
        turn_end_reason(&events),
        TurnEndReason::Error { .. }
    ));
}

/// A cooperative abort between tool executions persists the partial facts:
/// the completed ToolResult is in the log and TurnEnd{Aborted} is written.
#[tokio::test]
async fn aborted_turn_persists_partial_facts() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = format!(
        "{}/aborted.txt",
        work.path().file_name().unwrap().to_string_lossy()
    );

    let session = SessionManager::with_root(tmp.path().to_path_buf());
    // One tool-call script: after its ToolExecutionEnd fires we set the abort
    // signal, so the next provider request is skipped (StopReason::Aborted).
    let fake = FakeStream::new(vec![vec![
        write_call("t1", &rel, "done"),
        StreamChunk::Done,
    ]]);
    let host = Host::new(
        test_cfg(false),
        session,
        Arc::new(full_auto_policy()),
        "sys".into(),
        conga_host::built_in_tools(),
    )
    .with_stream_fn(Arc::new(fake));

    let signal = host.signal().clone();
    let summary = host
        .run_turn("go", move |ev| {
            if matches!(ev, conga::AgentEvent::ToolExecutionEnd { .. }) {
                signal.cancel();
            }
        })
        .await
        .expect("an aborted turn returns Ok with partial facts");

    assert!(
        matches!(summary.reason, TurnEndReason::Aborted { .. }),
        "expected TurnEnd::Aborted, got {:?}",
        summary.reason
    );
    assert!(
        std::path::Path::new(&rel).exists(),
        "the tool that completed before the abort must have run"
    );

    let sid = host.session().current_id().to_string();
    let events = EventStorage::new(tmp.path())
        .load_events(&sid)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, SessionEvent::ToolResult(_))),
        "completed tool result must be persisted"
    );
    assert!(matches!(
        turn_end_reason(&events),
        TurnEndReason::Aborted { .. }
    ));
}

/// A full successful turn: the event log projects to exactly the messages the
/// loop returned (which is what the legacy history+new_msgs concatenation
/// used to be), framed by TurnStart/User/TurnEnd{Completed}.
#[tokio::test]
async fn success_path_log_equals_legacy_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = format!(
        "{}/ok.txt",
        work.path().file_name().unwrap().to_string_lossy()
    );

    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let fake = FakeStream::new(vec![
        vec![write_call("t1", &rel, "data"), StreamChunk::Done],
        vec![
            StreamChunk::TextDelta("done".into()),
            StreamChunk::Usage {
                input: 10,
                output: 2,
                cache_read: 0,
                cache_write: 0,
            },
            StreamChunk::Done,
        ],
    ]);
    let host = Host::new(
        test_cfg(false),
        session,
        Arc::new(full_auto_policy()),
        "You are a helpful assistant.".into(),
        conga_host::built_in_tools(),
    )
    .with_stream_fn(Arc::new(fake));

    let mut buf: Vec<u8> = Vec::new();
    let summary: TurnSummary = host
        .run_turn("write it", |ev| {
            EventPrinter::new(&mut buf).on_event(&ev);
        })
        .await
        .expect("success turn");
    assert!(matches!(summary.reason, TurnEndReason::Completed));
    assert!(
        String::from_utf8_lossy(&buf).contains("done"),
        "printer must render the streamed text"
    );

    let sid = host.session().current_id().to_string();
    let events = EventStorage::new(tmp.path())
        .load_events(&sid)
        .await
        .unwrap();

    // Framing: TurnStart first, User second, TurnEnd{Completed} last.
    assert!(matches!(events.first(), Some(SessionEvent::TurnStart)));
    assert!(matches!(events.get(1), Some(SessionEvent::User(_))));
    assert!(matches!(events.last(), Some(SessionEvent::TurnEnd { .. })));

    // The projection equals the loop's returned messages — i.e. the legacy
    // "history + new_msgs" list the old API assembled in memory.
    assert_eq!(
        derive_messages(&events),
        summary.new_messages,
        "derived history must equal the loop's new messages"
    );
    // And it is the full legacy shape: user, assistant(tool call), tool
    // result, closing assistant.
    let kinds: Vec<&str> = summary
        .new_messages
        .iter()
        .map(|m| match m {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "toolresult",
            AgentMessage::Custom(_) => "custom",
        })
        .collect();
    assert_eq!(kinds, vec!["user", "assistant", "toolresult", "assistant"]);

    // Usage travels with the persisted assistant event (restart-safe budget).
    let last_usage = events.iter().rev().find_map(|ev| match ev {
        SessionEvent::Assistant { usage, .. } => *usage,
        _ => None,
    });
    assert_eq!(
        last_usage.map(|u| u.input_tokens),
        Some(10),
        "assistant usage must be persisted for token-aware compaction restarts"
    );
}

/// The 400-repair regression: an abort between two calls of one batch
/// leaves the assistant's second tool call unanswered in the log. The next
/// turn's provider request must carry a synthesized error result for it —
/// Crash-window regression: a turn that died mid-batch (here: a hand-built
/// log where the assistant made two write calls but only t1 got a result)
/// leaves the second tool call unanswered in the log. The next turn's
/// provider request must carry a synthesized error result for it -
/// OpenAI-compat APIs reject `tool_calls` without matching `tool` messages.
/// (Concurrent execution closed the live dangling path - an abort mid-batch
/// now lets dispatched calls finish - so the repair path is pinned by this
/// synthetic log, exactly what a crashed process leaves behind.)
#[tokio::test]
async fn next_turn_request_answers_every_tool_call() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = |name: &str| {
        format!(
            "{}/{name}",
            work.path().file_name().unwrap().to_string_lossy()
        )
    };

    let session = SessionManager::with_root(tmp.path().to_path_buf());
    // Turn 1, as a crashed process would have left it: both writes were
    // dispatched, only t1's result hit the log before the process died.
    let dangling_assistant = AgentMessage::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: "t1".into(),
                    function: FunctionCall {
                        name: "write".into(),
                        arguments: format!("{:?} {:?} {}", rel("a.txt"), "one", 1),
                    },
                },
            },
            ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: "t2".into(),
                    function: FunctionCall {
                        name: "write".into(),
                        arguments: format!("{:?} {:?} {}", rel("b.txt"), "two", 2),
                    },
                },
            },
        ],
        model: "m".into(),
        stop_reason: StopReason::ToolUse,
        usage: None,
        timestamp: conga::now(),
        stream_indices: vec![],
    });
    let t1_result = AgentMessage::ToolResult(ToolResultMessage {
        tool_call_id: "t1".into(),
        tool_name: "write".into(),
        content: vec![ContentBlock::text("ok".to_string())],
        is_error: false,
        timestamp: conga::now(),
    });
    for ev in [
        SessionEvent::TurnStart,
        SessionEvent::User(user_msg("write both")),
        SessionEvent::Assistant {
            message: dangling_assistant,
            usage: None,
        },
        SessionEvent::ToolResult(t1_result),
        SessionEvent::TurnEnd {
            reason: TurnEndReason::Aborted {
                cause: Some(CancelCause::User),
            },
        },
    ] {
        session.append_event(&ev).await.unwrap();
    }

    // Turn 2: plain text answer - its request is what we assert on.
    let fake = Arc::new(FakeStream::new(vec![vec![
        StreamChunk::TextDelta("ok".into()),
        StreamChunk::Done,
    ]]));
    let host = Host::new(
        test_cfg(false),
        session,
        Arc::new(full_auto_policy()),
        "sys".into(),
        conga_host::built_in_tools(),
    )
    .with_stream_fn(fake.clone());

    let summary = host.run_turn("continue", |_| {}).await.expect("turn");
    assert!(matches!(summary.reason, TurnEndReason::Completed));

    let requests = fake.seen();
    assert_eq!(requests.len(), 1, "one provider request for turn 2");

    // Every assistant tool_call in the request is answered by a tool
    // result - the OpenAI-compat 400 contract.
    let mut pending: Vec<String> = Vec::new();
    for msg in &requests[0] {
        match msg {
            AgentMessage::Assistant(a) => {
                for b in &a.content {
                    if let ContentBlock::ToolCall { tool_call: tc } = b {
                        pending.push(tc.id.clone());
                    }
                }
            }
            AgentMessage::ToolResult(tr) => {
                let i = pending
                    .iter()
                    .position(|id| id == &tr.tool_call_id)
                    .expect("tool result without a pending tool call");
                pending.remove(i);
            }
            _ => {}
        }
    }
    assert!(
        pending.is_empty(),
        "turn-2 request still has unanswered tool calls: {pending:?}"
    );
}

#[tokio::test]
async fn legacy_messages_migrate_once_and_delete_legacy() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = "legacy1";
    let dir = tmp.path().join(sid);
    std::fs::create_dir_all(&dir).unwrap();

    // A legacy transcript: user + assistant rows.
    let legacy = vec![user_msg("hello"), user_msg("again")];
    let mut raw = String::new();
    for m in &legacy {
        raw.push_str(&serde_json::to_string(m).unwrap());
        raw.push('\n');
    }
    std::fs::write(dir.join("messages.jsonl"), raw).unwrap();

    let sm = SessionManager::with_root(tmp.path().to_path_buf());
    let events = sm.migrate(sid).await.unwrap();
    assert_eq!(
        derive_messages(&events),
        legacy,
        "migrated log must project to the legacy messages"
    );
    assert!(
        dir.join("events.jsonl").exists(),
        "events.jsonl must exist after migration"
    );
    assert!(
        !dir.join("events.jsonl.tmp").exists(),
        "the atomic-install staging file must not survive the migration"
    );
    assert!(
        !dir.join("messages.jsonl").exists(),
        "legacy messages.jsonl must be deleted after a successful migration (D1)"
    );

    // Second open: loads events.jsonl directly, no legacy to touch.
    let events2 = sm.migrate(sid).await.unwrap();
    assert_eq!(events, events2, "second open must be idempotent");
    assert!(!dir.join("messages.jsonl").exists());
}

/// Corruption fails closed: a mid-file bad row makes the open return
/// Err — the old adopt-and-start-fresh behavior is gone, and a failed
/// migration leaves the legacy file untouched.
#[tokio::test]
async fn corrupted_session_errors_instead_of_adopting() {
    let tmp = tempfile::tempdir().unwrap();

    // Corrupt events.jsonl (bad middle line): has_events → load_events → Err.
    let sid = "broken-events";
    let dir = tmp.path().join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    let good = serde_json::to_string(&SessionEvent::User(user_msg("a"))).unwrap();
    let mut raw = String::new();
    raw.push_str(&good);
    raw.push('\n');
    raw.push_str("{\"type\":\"???\"\n"); // torn/corrupt middle row
    raw.push_str(&good);
    raw.push('\n');
    std::fs::write(dir.join("events.jsonl"), raw).unwrap();
    let sm = SessionManager::with_root(tmp.path().to_path_buf());
    assert!(
        sm.open(sid).await.is_err(),
        "corrupt events.jsonl must fail closed, not adopt"
    );

    // Corrupt legacy messages.jsonl (bad middle line): migration input is
    // unreadable → Err, and the legacy file must be left untouched.
    let sid2 = "broken-legacy";
    let dir2 = tmp.path().join(sid2);
    std::fs::create_dir_all(&dir2).unwrap();
    let mut raw2 = String::new();
    raw2.push_str(&serde_json::to_string(&user_msg("a")).unwrap());
    raw2.push('\n');
    raw2.push_str("{\"role\":\"??\"\n");
    raw2.push_str(&serde_json::to_string(&user_msg("b")).unwrap());
    raw2.push('\n');
    std::fs::write(dir2.join("messages.jsonl"), raw2).unwrap();
    let sm2 = SessionManager::with_root(tmp.path().to_path_buf());
    assert!(
        sm2.migrate(sid2).await.is_err(),
        "corrupt legacy transcript must fail closed, not adopt"
    );
    assert!(
        dir2.join("messages.jsonl").exists(),
        "a failed migration must leave the legacy file untouched"
    );
    assert!(
        !dir2.join("events.jsonl").exists(),
        "no partial events.jsonl may be written from a corrupt source"
    );
}

/// Compaction is a TURN-BOUNDARY checkpoint, not a per-call wire view:
/// when the restored budget is over threshold at turn start, ONE
/// `Compacted` event is appended and the provider sees that view for the
/// whole turn. Mid-turn growth appends AFTER the checkpoint (prefix-stable
/// within the turn — the property that keeps the provider prompt cache
/// warm), and the pre-checkpoint rows stay on disk (append-only intact).
/// This replaces the old `transform_context` wire view, which re-compacted
/// before every LLM call and oscillated between compacted and full
/// history, missing the cache every turn.
#[tokio::test]
async fn compaction_checkpoints_at_turn_start_and_pins_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = format!(
        "{}/c.txt",
        work.path().file_name().unwrap().to_string_lossy()
    );
    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let sid = session.current_id().to_string();

    // Seed a long history: 30 single-user-message groups plus a final
    // Assistant carrying a usage row far over the (default) window — the
    // next turn's budget restore must see over-threshold and checkpoint.
    for i in 0..30 {
        session
            .append_event(&SessionEvent::User(user_msg(&format!("m{i}"))))
            .await
            .unwrap();
    }
    session
        .append_event(&SessionEvent::Assistant {
            message: AgentMessage::assistant_text("big turn"),
            usage: Some(conga::Usage {
                input_tokens: 1_000_000,
                output_tokens: 1,
                cache_read_tokens: None,
                cache_write_tokens: None,
            }),
        })
        .await
        .unwrap();

    let fake = Arc::new(FakeStream::new(vec![
        vec![write_call("t1", &rel, "one"), StreamChunk::Done],
        vec![StreamChunk::TextDelta("done".into()), StreamChunk::Done],
        // Turn 2: proves the checkpoint is replayed, not re-derived.
        vec![StreamChunk::TextDelta("done2".into()), StreamChunk::Done],
    ]));
    let stream_fn: Arc<dyn conga::StreamFn> = fake.clone();
    // Defaults: window 128k, threshold 80% → 1M input tokens is over.
    let budget = conga_host::ContextBudget::from_env_with(&fake_env(&[]));
    let host = Host::new(
        test_cfg(true),
        session,
        Arc::new(full_auto_policy()),
        "sys".into(),
        conga_host::built_in_tools(),
    )
    .with_stream_fn(stream_fn)
    .with_budget(budget);

    let summary = host.run_turn("go", |_| {}).await.unwrap();
    assert!(matches!(summary.reason, TurnEndReason::Completed));

    let seen = fake.seen();
    assert_eq!(seen.len(), 2, "two LLM calls in turn 1");
    // Turn-start checkpoint: 31 seeded messages compacted to the 50%
    // target (pinned m0 + notice + 14 newest = 16) + the current user.
    assert_eq!(
        seen[0].len(),
        17,
        "call 1 must be the compacted view + current user, got {}",
        seen[0].len()
    );
    // Mid-turn growth is pure append: call 2 = call 1 + assistant +
    // tool_result, byte-identical prefix (no per-call re-compaction).
    assert_eq!(seen[1].len(), seen[0].len() + 2);
    assert_eq!(&seen[1][..seen[0].len()], &seen[0][..]);

    host.run_turn("again", |_| {}).await.unwrap();
    let seen = fake.seen();
    assert_eq!(seen.len(), 3, "one LLM call in turn 2");
    // Turn 2 replays the SAME checkpoint bytes: its request prefixes with
    // turn 1's final request (appended assistant/tool_result/user only).
    // The pre-checkpoint 1M usage row is out of the post-checkpoint scan
    // scope, so occupancy resets to unknown → no re-compaction churn.
    assert_eq!(&seen[2][..seen[1].len()], &seen[1][..]);

    // The log keeps every pre-checkpoint row AND the checkpoint itself.
    let reopen = SessionManager::with_root(tmp.path().to_path_buf());
    let events = reopen.open(&sid).await.unwrap();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, SessionEvent::Compacted { dropped, .. } if *dropped > 0)),
        "a Compacted checkpoint must be persisted"
    );
    let assistants = events
        .iter()
        .filter(|ev| matches!(ev, SessionEvent::Assistant { .. }))
        .count();
    assert_eq!(
        assistants, 4,
        "seeded + 2 turn-1 + 1 turn-2 assistants, all kept uncompacted"
    );
    // derive_messages restarts from the checkpoint: the projected view is
    // the compacted base + post-checkpoint rows — never the full 31.
    // base(16) + user("go") + asst + result + asst + user("again") + asst.
    assert_eq!(derive_messages(&events).len(), 22);
}

/// Records `(system, messages)` per provider call. The prefix-stability
/// guard asserts on exactly what the provider would cache — the message
/// list AND the system prompt — so both must be captured (FakeStream
/// records messages only).
struct PrefixCaptureStream {
    scripts: Mutex<VecDeque<Vec<StreamChunk>>>,
    seen: Mutex<Vec<(String, Vec<AgentMessage>)>>,
}

impl PrefixCaptureStream {
    fn new(scripts: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<(String, Vec<AgentMessage>)> {
        self.seen.lock().clone()
    }
}

impl conga::StreamFn for PrefixCaptureStream {
    fn stream(
        &self,
        _model: &conga::ModelSpec,
        messages: &[AgentMessage],
        system: &str,
        _tools: &[conga::ToolDefinition],
        _signal: Option<conga::CancelSignal>,
    ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = conga::StreamChunk> + Send>> {
        self.seen
            .lock()
            .push((system.to_string(), messages.to_vec()));
        let chunks =
            self.scripts.lock().pop_front().expect(
                "PrefixCaptureStream: script underflow — fewer scripts than stream() calls",
            );
        Box::pin(futures_util::stream::iter(chunks))
    }
}

/// THE guard: what the provider is asked to cache must be byte-stable
/// across consecutive requests. Four invariants, one test:
///
/// 1. The system prompt is IDENTICAL on every provider call of every
///    turn (static head — a per-turn env block in there would invalidate
///    the cache from the top on every turn).
/// 2. The system prompt carries NO dynamic env (no UTC date, no git
///    status, no `<environment>` tag).
/// 3. Every request's message list is a strict PREFIX-EXTENSION of the
///    previous one (within a turn and across turns): request N's exact
///    messages reappear, untouched, at the head of request N+1. That is
///    the byte-level precondition for prefix-cache hits.
/// 4. Send-what-you-persist: the volatile env rides INSIDE the persisted
///    user message (its tail), and the request history equals the log's
///    projection — replay and request are the same bytes.
#[tokio::test]
async fn consecutive_requests_are_prefix_stable_and_env_stays_out_of_system() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir_in(".").unwrap();
    let rel = format!(
        "{}/g.txt",
        work.path().file_name().unwrap().to_string_lossy()
    );
    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let sid = session.current_id().to_string();

    let provider = Arc::new(PrefixCaptureStream::new(vec![
        vec![write_call("t1", &rel, "one"), StreamChunk::Done],
        vec![StreamChunk::TextDelta("done".into()), StreamChunk::Done],
        vec![StreamChunk::TextDelta("done2".into()), StreamChunk::Done],
    ]));
    let host = Host::new(
        test_cfg(true),
        session,
        Arc::new(full_auto_policy()),
        "static system prompt".into(),
        conga_host::built_in_tools(),
    )
    .with_stream_fn(provider.clone() as Arc<dyn conga::StreamFn>);

    host.run_turn("task one", |_| {}).await.unwrap();
    host.run_turn("task two", |_| {}).await.unwrap();

    let seen = provider.seen();
    assert_eq!(seen.len(), 3, "two calls in turn 1, one in turn 2");

    // 1. Static head: identical system on every call. The per-turn seam
    //    (compose_turn_prompt) appends the skills/memory catalogs after the
    //    base — deterministic rescans, so the appended bytes are identical
    //    on every call; the base itself must still ride the head.
    for (system, _) in &seen {
        assert_eq!(system, &seen[0].0);
        assert!(system.starts_with("static system prompt"));
    }
    // 2. No dynamic env may leak into the cached prefix.
    assert!(!seen[0].0.contains("Date (UTC)"));
    assert!(!seen[0].0.contains("Git status"));
    assert!(!seen[0].0.contains("<environment>"));

    // 3. Prefix-extension: request K+1 starts with request K's exact
    // messages (structurally equal ⇒ identical cached bytes).
    let is_prefix = |shorter: &[AgentMessage], longer: &[AgentMessage]| {
        longer.len() >= shorter.len() && &longer[..shorter.len()] == shorter
    };
    assert!(
        is_prefix(&seen[0].1, &seen[1].1),
        "mid-turn growth must be append-only: {:?} vs {:?}",
        seen[0].1.len(),
        seen[1].1.len()
    );
    assert!(
        is_prefix(&seen[1].1, &seen[2].1),
        "turn-2 request must prefix-extend turn-1's final request"
    );

    // 4. Send-what-you-persist: env rides in the persisted user tail, and
    // the request history IS the log projection.
    let user_text = |m: &AgentMessage| match m {
        AgentMessage::User(u) => match &u.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        },
        _ => panic!("expected user message"),
    };
    let first_user = user_text(&seen[0].1[0]);
    assert!(
        first_user.starts_with("task one\n\n<environment>"),
        "env must be the tail of the persisted user message, got: {first_user:.60}…"
    );
    let reopen = SessionManager::with_root(tmp.path().to_path_buf());
    let events = reopen.open(&sid).await.unwrap();
    let projected = derive_messages(&events);
    // The turn-2 request equals the final projection minus its TRAILING
    // assistant — that message is persisted by this very provider call
    // (after streaming), so it cannot have been in the request.
    assert_eq!(
        &seen[2].1,
        &projected[..projected.len() - 1],
        "the request history must equal the log projection"
    );
}

/// The unified `/clear`: `clear_session` appends a `Cleared` fact to the
/// SAME session's log (no id rotation → no ghost sessions). The next turn's
/// derived history starts empty, a fresh process sees the same cleared
/// view, and the pre-clear rows stay on disk (append-only intact).
#[tokio::test]
async fn clear_session_marks_log_and_next_turn_starts_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let session = SessionManager::with_root(tmp.path().to_path_buf());
    let sid = session.current_id().to_string();
    let fake = FakeStream::new(vec![
        // Turn 1: a plain text answer.
        vec![
            StreamChunk::TextDelta("old answer".into()),
            StreamChunk::Done,
        ],
        // Turn 2 (post-clear): a fresh answer.
        vec![
            StreamChunk::TextDelta("fresh answer".into()),
            StreamChunk::Done,
        ],
    ]);
    let host = Host::new(
        test_cfg(true),
        session,
        Arc::new(full_auto_policy()),
        "sys".into(),
        vec![],
    )
    .with_stream_fn(Arc::new(fake));

    host.run_turn("old question", |_| {}).await.unwrap();

    // /clear: a fact in the log, same session id.
    host.clear_session().await.unwrap();
    assert_eq!(host.session().current_id(), sid);

    host.run_turn("fresh question", |_| {}).await.unwrap();

    // Derived history: exactly the post-clear turn (user + assistant).
    let reopen = SessionManager::with_root(tmp.path().to_path_buf());
    let events = reopen.open(&sid).await.unwrap();
    assert!(events.contains(&SessionEvent::Cleared));
    let msgs = derive_messages(&events);
    assert_eq!(msgs.len(), 2, "only the post-clear turn, got {msgs:?}");
    let texts: Vec<String> = msgs
        .iter()
        .filter_map(|m| match m {
            AgentMessage::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            },
            AgentMessage::Assistant(a) => match &a.content[0] {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().any(|t| t.starts_with("fresh question")),
        "user text leads the (env-suffixed) message: {texts:?}"
    );
    assert!(texts.iter().any(|t| t == "fresh answer"), "{texts:?}");
    assert!(
        !texts
            .iter()
            .any(|t| t.starts_with("old question") || t == "old answer"),
        "pre-clear content must not leak into the derived view: {texts:?}"
    );

    // Append-only intact: the pre-clear rows are still on disk.
    let raw = std::fs::read_to_string(tmp.path().join(&sid).join("events.jsonl")).unwrap();
    assert!(raw.contains("old question"), "log keeps pre-clear rows");
}
