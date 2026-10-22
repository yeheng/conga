//! In-process chat transport for the desktop app.
//!
//! Mirrors the gateway's per-connection session loop (conga-gateway ws.rs):
//! one `Host` per session, assembled from the same config/env knobs, with the
//! same cancellation and approval semantics. The only difference is the
//! transport — instead of WebSocket frames, turn events are emitted as Tauri
//! `chat-event` IPC events whose `event` payload is byte-for-byte the
//! gateway's wire JSON (host-owned schema, `conga_host::wire`), so the
//! frontend's message handling is shared unchanged between both transports.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dashmap::DashMap;
use log::{info, warn};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use conga::AgentEvent;
use conga_host::event_map::{event_to_ws, subagent_event_to_ws};
use conga_host::wire::OutgoingEvent;
use conga_host::SessionAssembly;

/// Tauri event channel carrying every chat event. Payload: [`ChatEventPayload`].
pub const CHAT_EVENT: &str = "chat-event";

/// IPC payload: the gateway-protocol event plus the session it belongs to, so
/// one broadcast channel can multiplex many sessions (WS had a socket per
/// session instead).
#[derive(Clone, serde::Serialize)]
struct ChatEventPayload {
    session_id: String,
    event: serde_json::Value,
}

/// Same internal queue as the gateway's `WireEvent`: everything the frontend
/// sees flows through ONE ordered channel with a single emitter task, so a
/// turn-boundary `done` can never overtake the last subagent event and an
enum WireEvent {
    Agent(AgentEvent),
    Subagent(conga_host::SubagentEvent),
    Approval {
        request_id: String,
        tool_name: String,
        args: serde_json::Value,
        preview: Option<String>,
    },
    /// Mid-turn user message accepted into the steer queue; the loop injects
    /// it as a User message before its next LLM call.
    Queued(String),
    /// Slash-command reply: bypasses run_turn through the same ordered
    /// channel. Always followed by `Done` so the frontend's turn-boundary
    /// handling fires (clears isReceiving, refreshes context).
    Reply(OutgoingEvent),
    Done,
    Error(String),
}

/// Per-session state: the Host (which owns the on-disk event log via its
/// SessionManager) plus the cross-command knobs (cancel, approvals). The
/// transcript itself is never mirrored in memory — the event log is the
/// single source of truth. `host`/`registry`/`cancel_tx` come from the
/// shared [`SessionAssembly`] — identical wiring to the gateway's WS
/// connection, so behavior cannot drift between transports.
struct ChatSession {
    host: conga_host::Host,
    /// Turn serialization: mirrors the gateway's one-turn-per-connection
    /// behavior (a second message while a turn runs gets a `busy` reply).
    /// Host::run_turn's own TurnInProgress rejection is the backstop.
    turn_active: AtomicBool,
    /// Unlocks pending approval waits on cancel. Fresh `subscribe()` receivers
    /// per approval avoid the cancel-latch poisoning (see approval.rs tests).
    cancel_tx: tokio::sync::watch::Sender<bool>,
    /// Shared with the approver closure baked into the Host's policy —
    /// `approval_response` must fill in decisions on THIS registry.
    registry: Arc<Mutex<conga_host::approval::ApprovalRegistry>>,
    wire_tx: UnboundedSender<WireEvent>,
    /// Cumulative provider-reported input tokens across turns (fed by
    /// `AfterProviderResponse` events in the emitter). Mirrors the gateway's
    /// `WsSession::usage_in`. Lock-free: the emitter task accumulates, the
    /// `get_context` command reads.
    usage_in: AtomicU64,
    /// Cumulative provider-reported output tokens across turns.
    usage_out: AtomicU64,
    /// Cumulative provider-reported prompt-cache read/write tokens across
    /// turns (0 while the provider reports no cache breakdown). Mirrors the
    /// gateway's `WsSession::cache_read/write`.
    cache_read: AtomicU64,
    cache_write: AtomicU64,
    /// Most recent provider-reported input-token count for this turn (current
    /// window occupancy). Drives the context-saturation percentage.
    last_input_tokens: AtomicU64,
    /// task just before `run_turn`, read by the emitter on `Done`. Wrapped in
    /// a Mutex so the setter (turn task) and reader (emitter task) synchronize
    /// without atomics on an `Instant`.
    turn_start: Mutex<Option<Instant>>,
}

#[derive(Default)]
pub struct ChatState {
    sessions: DashMap<String, Arc<ChatSession>>,
    /// Serializes session construction: two concurrent first messages for the
    /// same session must not race the on-disk resume/migration.
    create_lock: tokio::sync::Mutex<()>,
    store_root: PathBuf,
}

impl ChatState {
    pub fn new() -> Self {
        Self {
            store_root: conga::JsonlStorage::default_root().base_dir_clone(),
            ..Default::default()
        }
    }

    async fn get_or_create(
        &self,
        app: &AppHandle,
        session_id: &str,
    ) -> Result<Arc<ChatSession>, String> {
        if let Some(s) = self.sessions.get(session_id) {
            return Ok(Arc::clone(&s));
        }
        let _guard = self.create_lock.lock().await;
        if let Some(s) = self.sessions.get(session_id) {
            return Ok(Arc::clone(&s));
        }
        let session = build_session(app, &self.store_root, session_id).await?;
        self.sessions
            .insert(session_id.to_string(), session.clone());
        Ok(session)
    }
}

/// Single emitter task per session: owns all `app.emit` calls for that
/// session, preserving cross-stream ordering exactly like the gateway's
/// single socket writer. Also accumulates provider-reported token usage
/// (from `AfterProviderResponse` events) into the session's atomic counters
/// and renders a usage summary on `Done`.
fn spawn_emitter(
    app: AppHandle,
    session_id: String,
    mut rx: UnboundedReceiver<WireEvent>,
    session: Arc<ChatSession>,
) {
    tauri::async_runtime::spawn(async move {
        // tool_call_id -> tool name, per turn (cleared on Done).
        let mut tool_names: HashMap<String, String> = HashMap::new();
        while let Some(ev) = rx.recv().await {
            let event: Option<serde_json::Value> = match ev {
                WireEvent::Agent(event) => {
                    if let AgentEvent::ToolExecutionStart {
                        tool_call_id,
                        tool_name,
                        ..
                    } = &event
                    {
                        tool_names.insert(tool_call_id.clone(), tool_name.clone());
                    }
                    // Accumulate provider-reported usage (mirrors the gateway's
                    // forwarder). `AfterProviderResponse` carries the token counts
                    // the provider returned for this call; they accumulate into the
                    // session counters so the `done` summary and the `get_context`
                    // command report real API spend.
                    if let AgentEvent::AfterProviderResponse { response, .. } = &event {
                        if let Some(u) = &response.usage {
                            session
                                .usage_in
                                .fetch_add(u.input_tokens, Ordering::Relaxed);
                            session
                                .usage_out
                                .fetch_add(u.output_tokens, Ordering::Relaxed);
                            session
                                .cache_read
                                .fetch_add(u.cache_read_tokens.unwrap_or(0), Ordering::Relaxed);
                            session
                                .cache_write
                                .fetch_add(u.cache_write_tokens.unwrap_or(0), Ordering::Relaxed);
                            session
                                .last_input_tokens
                                .store(u.input_tokens, Ordering::Relaxed);
                        }
                    }
                    event_to_ws(&event, &mut tool_names)
                        .and_then(|ev| serde_json::to_value(ev).ok())
                }
                WireEvent::Subagent(ev) => {
                    // Sub-agent provider usage counts toward the session's token
                    // totals (same as the gateway); it has no IPC message of its own.
                    if let conga_host::SubagentEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read,
                        cache_write,
                    } = ev
                    {
                        session.usage_in.fetch_add(input_tokens, Ordering::Relaxed);
                        session
                            .usage_out
                            .fetch_add(output_tokens, Ordering::Relaxed);
                        session.cache_read.fetch_add(cache_read, Ordering::Relaxed);
                        session
                            .cache_write
                            .fetch_add(cache_write, Ordering::Relaxed);
                        None
                    } else {
                        subagent_event_to_ws(&ev)
                    }
                }
                WireEvent::Approval {
                    request_id,
                    tool_name,
                    args,
                    preview,
                } => serde_json::to_value(OutgoingEvent::approval_request(
                    request_id, tool_name, &args, preview,
                ))
                .ok(),
                WireEvent::Queued(text) => serde_json::to_value(OutgoingEvent::queued(text)).ok(),
                WireEvent::Reply(ev) => serde_json::to_value(ev).ok(),
                WireEvent::Done => {
                    // Turn boundary: the tool-name cache is per-turn.
                    tool_names.clear();
                    // Emit a usage summary line: cumulative tokens + elapsed time.
                    // `turn_start` is set by the turn task just before run_turn.
                    let elapsed_ms = session
                        .turn_start
                        .lock()
                        .unwrap()
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    let ev = if elapsed_ms > 0 {
                        OutgoingEvent::done_with_summary(
                            session.usage_in.load(Ordering::Relaxed),
                            session.usage_out.load(Ordering::Relaxed),
                            session.cache_read.load(Ordering::Relaxed),
                            session.cache_write.load(Ordering::Relaxed),
                            elapsed_ms,
                        )
                    } else {
                        OutgoingEvent::done()
                    };
                    serde_json::to_value(ev).ok()
                }
                WireEvent::Error(msg) => serde_json::to_value(OutgoingEvent::error(msg)).ok(),
            };
            if let Some(event) = event {
                let payload = ChatEventPayload {
                    session_id: session_id.clone(),
                    event,
                };
                if let Err(e) = app.emit(CHAT_EVENT, payload) {
                    warn!("session {session_id}: emit failed: {e}");
                }
            }
        }
    });
}

/// Assemble a session's Host via the SHARED assembly (conga_host::assembly)
/// — the exact same config load, fail-loud resume, skills, permission mode,
/// approver, tool set (built-in + external + MCP), and sub-agent wiring the
/// gateway uses per WS connection. The desktop adds its in-process extension
/// tools (web_search) as `extra_tools`. Do not invent new config here — the
/// desktop app reads the same `~/.conga` setup.
async fn build_session(
    app: &AppHandle,
    store_root: &std::path::Path,
    session_id: &str,
) -> Result<Arc<ChatSession>, String> {
    let (wire_tx, wire_rx) = tokio::sync::mpsc::unbounded_channel::<WireEvent>();

    let approval_emit: conga_host::ApprovalEmit = {
        let wire = wire_tx.clone();
        Arc::new(
            move |request_id: String,
                  tool_name: String,
                  args: serde_json::Value,
                  preview: Option<String>| {
                // Approval requests ride the same ordered channel as every other
                // wire event, so a request can never overtake the tool_start of
                // the call it belongs to.
                let _ = wire.send(WireEvent::Approval {
                    request_id,
                    tool_name,
                    args,
                    preview,
                });
            },
        )
    };
    let subagent_emit: conga_host::SubagentEmit = {
        let wire = wire_tx.clone();
        Arc::new(move |ev: conga_host::SubagentEvent| {
            let _ = wire.send(WireEvent::Subagent(ev));
        })
    };

    // Production extensions from conga-ext (web_search / rag_search, and
    // terminal) — the non-demo composition root. Their HTTP clients honor
    // the runtime tool proxy (conga::set_tool_proxy).
    let search_tools = {
        let mut api = conga::ExtensionApiImpl::new();
        conga_ext::prod_register(&mut api);
        api.tools
    };

    let SessionAssembly {
        host,
        registry,
        cancel_tx,
    } = SessionAssembly::build(
        store_root,
        session_id,
        search_tools,
        approval_emit,
        subagent_emit,
    )
    .await
    .map_err(|e| e.to_string())?;

    let session = Arc::new(ChatSession {
        host,
        turn_active: AtomicBool::new(false),
        cancel_tx,
        registry,
        wire_tx,
        usage_in: AtomicU64::new(0),
        usage_out: AtomicU64::new(0),
        cache_read: AtomicU64::new(0),
        cache_write: AtomicU64::new(0),
        last_input_tokens: AtomicU64::new(0),
        turn_start: Mutex::new(None),
    });
    // Spawn the emitter after the session Arc exists so it can hold a clone
    // for usage accumulation and the `done` summary line.
    spawn_emitter(
        app.clone(),
        session_id.to_string(),
        wire_rx,
        Arc::clone(&session),
    );
    Ok(session)
}

/// Start a turn for `content` on the given session. Returns immediately; the
/// turn's events stream back via `chat-event`. A message while a turn is
/// running is answered with a `busy` event (never silently dropped).
#[tauri::command]
pub async fn send_message(
    app: AppHandle,
    state: State<'_, ChatState>,
    session_id: String,
    content: String,
    trace_id: Option<String>,
) -> Result<(), String> {
    if !conga::is_valid_session_id(&session_id) {
        return Err("invalid session id".into());
    }
    if content.trim().is_empty() {
        return Ok(());
    }
    info!("session {session_id}: message (trace {trace_id:?})");
    let session = match state.get_or_create(&app, &session_id).await {
        Ok(s) => s,
        Err(e) => {
            // Mirror the gateway's config/session error frame: without it the UI
            // would hang in "sending" with no banner.
            let payload = ChatEventPayload {
                session_id: session_id.clone(),
                event: serde_json::to_value(OutgoingEvent::error(e.clone())).unwrap_or_default(),
            };
            let _ = app.emit(CHAT_EVENT, payload);
            return Err(e);
        }
    };

    if session.turn_active.swap(true, Ordering::AcqRel) {
        // Mid-turn message: steer it into the running loop (a real User message
        // before its next LLM call) and acknowledge — not rejected.
        session.host.steer().push(content.clone());
        let _ = session.wire_tx.send(WireEvent::Queued(content.clone()));
        return Ok(());
    }

    // ── Slash commands (mirrors the gateway's WS loop) ─────────
    // /clear and /help are handled server-side; everything else goes to
    // the LLM. Keep this list in sync with the ChatInput.vue completer.
    if let Some(cmd) = content.strip_prefix('/') {
        // The swap above claimed the turn slot, but slash commands are not
        // turns and never spawn the task whose Drop guard would release it.
        // Free it here or /clear leaves the session permanently "busy":
        // every later message is answered with Busy and nothing ever runs.
        session.turn_active.store(false, Ordering::Release);
        let mut parts = cmd.split_whitespace();
        let reply = match parts.next() {
            Some("clear") => {
                // SAME semantics as the gateway's /clear (and the CLI's): append a
                // `SessionEvent::Cleared` fact to this session's log. The id does
                // NOT rotate, so this IPC session, REST-style readers, and the FTS
                // index keep addressing the same chat. derive_messages truncates on
                // the next turn; a failed write is reported instead of silently
                // resurrecting the old history.
                match session.host.clear_session().await {
                    Ok(()) => {
                        session.usage_in.store(0, Ordering::Relaxed);
                        session.usage_out.store(0, Ordering::Relaxed);
                        session.cache_read.store(0, Ordering::Relaxed);
                        session.cache_write.store(0, Ordering::Relaxed);
                        session.last_input_tokens.store(0, Ordering::Relaxed);
                        OutgoingEvent::content("(session cleared)".to_string())
                    }
                    Err(e) => OutgoingEvent::error(format!("clear failed: {e}")),
                }
            }
            Some("help") => OutgoingEvent::content("commands: /clear  /help".to_string()),
            Some(other) => OutgoingEvent::error(format!("unknown command /{other}")),
            None => return Ok(()), // just "/" with nothing after
        };
        // Slash commands are not turns: clear turn_start so the `done`
        // event renders without a stale elapsed/usage summary.
        *session.turn_start.lock().unwrap() = None;
        let _ = session.wire_tx.send(WireEvent::Reply(reply));
        let _ = session.wire_tx.send(WireEvent::Done);
        return Ok(());
    }

    tauri::async_runtime::spawn(async move {
        // Drop guard: a failed/aborted turn must still free the slot, or the
        // session would stay "busy" forever.
        struct TurnGuard<'a>(&'a AtomicBool);
        impl Drop for TurnGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _guard = TurnGuard(&session.turn_active);
        // Record turn start for the done-summary line. Set before run_turn
        // begins so the elapsed time covers the whole turn.
        *session.turn_start.lock().unwrap() = Some(Instant::now());

        let wire = session.wire_tx.clone();
        let outcome = session
            .host
            .run_turn(&content, move |ev| {
                let _ = wire.send(WireEvent::Agent(ev));
            })
            .await;

        // Turn boundary: clear in-flight approvals regardless of outcome.
        session.registry.lock().unwrap().clear_pending();

        // done/error are queued AFTER every event the turn emitted, so the
        // frontend sees a complete picture before the turn-boundary markers.
        let _ = session.wire_tx.send(WireEvent::Done);
        if let Err(e) = outcome {
            warn!("session {session_id}: agent error: {e}");
            let _ = session.wire_tx.send(WireEvent::Error(format!("{e}")));
        }
    });
    Ok(())
}

/// Cooperative abort: cancel the Host's shared cancel signal (read at safe
/// points by the agent loop) and unlock any pending approval wait.
#[tauri::command]
pub fn cancel_turn(state: State<'_, ChatState>, session_id: String) -> Result<(), String> {
    if let Some(session) = state.sessions.get(&session_id) {
        session.host.signal().cancel();
        let _ = session.cancel_tx.send(true);
        info!("session {session_id}: cancel");
    }
    // No session yet (cancel before the first turn): nothing to abort.
    Ok(())
}

/// Fill in an approval decision; `remember` caches it by tool name. Unknown
/// or late request ids are ignored silently, same as the gateway.
#[tauri::command]
pub fn approval_response(
    state: State<'_, ChatState>,
    session_id: String,
    request_id: String,
    approved: bool,
    remember: bool,
) -> Result<(), String> {
    if let Some(session) = state.sessions.get(&session_id) {
        session
            .registry
            .lock()
            .unwrap()
            .respond(&request_id, approved, remember);
    }
    Ok(())
}

/// Context occupancy for the desktop app. Mirrors the gateway's
/// `GET /api/sessions/:id/context` endpoint — both build the payload with
/// the SAME `conga_host::wire::context_stats` (one JSON shape, one window
/// knob: settings.json `maxTokens` > `CONGA_CONTEXT_WINDOW` > 128k):
/// reads the session's accumulated usage counters and computes a
/// saturation percentage. Payload shape: `{ context_stats }`.
#[tauri::command]
pub fn get_context(
    state: State<'_, ChatState>,
    session_id: String,
) -> Result<serde_json::Value, String> {
    let max_tokens = conga_host::settings::effective_max_tokens();
    let (last_input_tokens, usage_in, usage_out, cache_read, cache_write) =
        match state.sessions.get(&session_id) {
            Some(session) => (
                session.last_input_tokens.load(Ordering::Relaxed),
                session.usage_in.load(Ordering::Relaxed),
                session.usage_out.load(Ordering::Relaxed),
                session.cache_read.load(Ordering::Relaxed),
                session.cache_write.load(Ordering::Relaxed),
            ),
            None => (0u64, 0u64, 0u64, 0u64, 0u64),
        };
    let stats = conga_host::wire::context_stats(
        last_input_tokens,
        usage_in,
        usage_out,
        cache_read,
        cache_write,
        max_tokens,
    );
    Ok(serde_json::json!({ "context_stats": stats }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The desktop app stores its sessions in the same place the gateway
    /// does — the shared `~/.conga/sessions` root — so the two transports
    /// see one set of sessions.
    #[test]
    fn chat_state_roots_at_the_conga_sessions_dir() {
        // `$HOME` is process-global: hold the lock so no other test can move
        // the config root between the two reads below.
        let _guard = crate::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let expected = conga::JsonlStorage::default_root().base_dir_clone();
        let state = ChatState::new();
        assert_eq!(state.store_root, expected);
        assert!(
            state.store_root.ends_with("sessions"),
            "unexpected store root: {}",
            state.store_root.display()
        );
    }
}
