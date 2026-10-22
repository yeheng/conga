//! WebSocket upgrade handler and the per-connection session loop.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use dashmap::mapref::entry::Entry;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use conga::AgentEvent;

use conga_host::assembly::lock_registry;
use conga_host::event_map::event_to_ws;
use conga_host::wire::OutgoingEvent;

use crate::state::{AppState, WsSession};
use crate::wire::{ApprovalResponse, IncomingMessage};

/// Everything written to the socket flows through ONE ordered channel and a
/// single writer task. A single writer guarantees cross-stream ordering:
/// without it, the turn-boundary `done` could overtake the last subagent
/// event (the frontend skips `done` while subagents are active → stuck UI),
/// and approval requests could overtake the tool_start they belong to.
enum WireEvent {
    Agent(conga::AgentEvent),
    Subagent(conga_host::SubagentEvent),
    Approval {
        request_id: String,
        tool_name: String,
        args: serde_json::Value,
        preview: Option<String>,
    },
    /// Mid-turn user message accepted into the steer queue; rendered as a
    /// queued user bubble. The loop injects it before its next LLM call.
    Queued(String),
    /// Slash-command reply that bypasses `run_turn` (goes through the same
    /// ordered channel as everything else — a single writer means exactly
    /// that, no direct-sender shortcuts). Always followed by `Done` so the
    /// frontend's turn-boundary handling fires.
    Reply(OutgoingEvent),
    Done,
    Error(String),
}

pub(crate) async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // `user_id` is untrusted client input. Never use it as a filesystem path
    // component: a malicious `?user_id=../../etc` would otherwise write the
    // session JSONL outside the store root. Validate; fall back to a fresh
    // server-generated UUID when missing or unsafe.
    let session_id = params
        .get("user_id")
        .filter(|s| conga::is_valid_session_id(s))
        .cloned()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // One live connection per session id: a second client reusing the id
    // would clobber the session map entry (the first connection's cleanup
    // would then remove/kill the second's resources, e.g. its persistent
    // shell). Reject instead — the entry is removed on disconnect, so a
    // prompt reconnect still works. This check is the cheap early reject
    // for the sequential case; the authoritative atomic claim (DashMap
    // `entry`) happens in `handle_ws`, closing the simultaneous-upgrade
    // race this check alone cannot.
    if state.sessions.contains_key(&session_id) {
        warn!("ws upgrade rejected: session {session_id} already connected");
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({
                "error": format!("session already connected: {session_id}")
            })),
        )
            .into_response();
    }
    info!("ws upgrade: session={session_id}");
    ws.on_upgrade(move |socket| handle_ws(socket, state, session_id))
}

async fn handle_ws(socket: WebSocket, state: Arc<AppState>, session_id: String) {
    let (ws_tx, mut ws_rx) = socket.split();
    let session = Arc::new(Mutex::new(WsSession {
        sender: ws_tx,
        usage_in: 0,
        usage_out: 0,
        cache_read: 0,
        cache_write: 0,
        last_input_tokens: 0,
        turn_start: None,
    }));

    // ── Atomic session claim ────────────────────────────────
    // Register under the session id in one atomic step (check + insert via
    // DashMap `entry`). The `ws_handler` pre-check cannot close the race
    // between two simultaneous upgrades of the same id — both handshakes
    // would pass it before either registers. The loser here closes its
    // socket immediately, without touching the winner's resources.
    match state.sessions.entry(session_id.clone()) {
        Entry::Occupied(_) => {
            warn!("ws session {session_id}: duplicate connection lost the race; closing");
            let mut s = session.lock().await;
            let _ = s.sender.send(Message::Close(None)).await;
            return;
        }
        Entry::Vacant(e) => {
            e.insert(session.clone());
        }
    }

    // ── Single ordered wire channel ──────────────────────────
    // All outbound events (main agent stream, subagent events, approval
    // requests, turn-boundary done/error) queue here; one writer task owns
    // the socket, preserving order across streams.
    let (wire_tx, mut wire_rx) = tokio::sync::mpsc::unbounded_channel::<WireEvent>();
    let wire_session = session.clone();
    tokio::spawn(async move {
        // tool_call_id → tool name, per turn (cleared on Done).
        let mut tool_names: HashMap<String, String> = HashMap::new();
        while let Some(ev) = wire_rx.recv().await {
            let payload: Option<String> = match ev {
                WireEvent::Agent(event) => {
                    if let AgentEvent::ToolExecutionStart {
                        tool_call_id,
                        tool_name,
                        ..
                    } = &event
                    {
                        tool_names.insert(tool_call_id.clone(), tool_name.clone());
                    }
                    // Accumulate provider-reported usage for the context API.
                    // Unreported cache breakdown (None) contributes 0.
                    if let AgentEvent::AfterProviderResponse { response, .. } = &event {
                        if let Some(u) = &response.usage {
                            let mut s = wire_session.lock().await;
                            s.usage_in += u.input_tokens;
                            s.usage_out += u.output_tokens;
                            s.cache_read += u.cache_read_tokens.unwrap_or(0);
                            s.cache_write += u.cache_write_tokens.unwrap_or(0);
                            s.last_input_tokens = u.input_tokens;
                        }
                    }
                    event_to_ws(&event, &mut tool_names)
                        .map(|ev| serde_json::to_string(&ev).unwrap_or_default())
                }
                WireEvent::Subagent(ev) => {
                    if let conga_host::SubagentEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read,
                        cache_write,
                    } = ev
                    {
                        // Sub-agent provider usage counts toward the session's
                        // token totals; it has no WS message of its own. (The
                        // parent's compaction budget is NOT touched: sub-agent
                        // messages never enter the main history.)
                        let mut s = wire_session.lock().await;
                        s.usage_in += input_tokens;
                        s.usage_out += output_tokens;
                        s.cache_read += cache_read;
                        s.cache_write += cache_write;
                        None
                    } else {
                        conga_host::event_map::subagent_event_to_ws(&ev)
                            .map(|v| serde_json::to_string(&v).unwrap_or_default())
                    }
                }
                WireEvent::Approval {
                    request_id,
                    tool_name,
                    args,
                    preview,
                } => {
                    let ev = OutgoingEvent::approval_request(request_id, tool_name, &args, preview);
                    Some(serde_json::to_string(&ev).unwrap_or_default())
                }
                WireEvent::Queued(text) => {
                    let ev = OutgoingEvent::queued(text);
                    Some(serde_json::to_string(&ev).unwrap_or_default())
                }
                WireEvent::Reply(ev) => Some(serde_json::to_string(&ev).unwrap_or_default()),
                WireEvent::Done => {
                    // Turn boundary: the tool-name cache is per-turn.
                    tool_names.clear();
                    // Emit a usage summary line: cumulative tokens +
                    // elapsed time. `turn_start` is set by the main loop
                    // just before run_turn; None only if Done arrives
                    // without a preceding turn (shouldn't happen, but
                    // degrade to a plain done instead of crashing).
                    let s = wire_session.lock().await;
                    let elapsed_ms = s
                        .turn_start
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    let ev = if elapsed_ms > 0 {
                        OutgoingEvent::done_with_summary(
                            s.usage_in,
                            s.usage_out,
                            s.cache_read,
                            s.cache_write,
                            elapsed_ms,
                        )
                    } else {
                        OutgoingEvent::done()
                    };
                    drop(s); // release lock before send_json below
                    Some(serde_json::to_string(&ev).unwrap_or_default())
                }
                WireEvent::Error(msg) => {
                    let ev = OutgoingEvent::error(msg);
                    Some(serde_json::to_string(&ev).unwrap_or_default())
                }
            };
            if let Some(payload) = payload {
                let mut s = wire_session.lock().await;
                let _ = s
                    .sender
                    .send(axum::extract::ws::Message::Text(payload.into()))
                    .await;
            }
        }
    });

    // ── Assemble the session's Host ──────────────────────
    // One shared wiring for every transport (conga_host::assembly): config
    // load, fail-loud log resume (corruption refuses the connection —
    // never adopt-and-restart), skills, permission mode + approver, tool
    // set, sub-agent spawner. The gateway owns only transport plumbing:
    // this ordered channel and the message loop below.
    let approval_emit: conga_host::ApprovalEmit = {
        let wire = wire_tx.clone();
        Arc::new(
            move |request_id: String,
                  tool_name: String,
                  args: serde_json::Value,
                  preview: Option<String>| {
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
    let assembly = match conga_host::SessionAssembly::build(
        &state.store_root,
        &session_id,
        Vec::new(),
        approval_emit,
        subagent_emit,
    )
    .await
    {
        Ok(a) => a,
        Err(e) => {
            error!("session {session_id}: {e}");
            let err = OutgoingEvent::error(e.to_string());
            let mut s = session.lock().await;
            send_json(&mut s.sender, &err).await;
            let _ = s.sender.send(Message::Close(None)).await;
            state.sessions.remove(&session_id);
            return;
        }
    };
    let conga_host::SessionAssembly {
        host,
        registry,
        cancel_tx,
    } = assembly;
    // Cancel sets the Host's shared abort flag; run_turn reads it at safe points.
    let signal = host.signal().clone();

    // ── Main event loop ─────────────────────────────────────
    loop {
        let msg = match ws_rx.next().await {
            Some(Ok(Message::Text(t))) => t.to_string(),
            Some(Ok(Message::Close(_))) | None => {
                info!("session {session_id}: ws closed");
                break;
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = session.lock().await.sender.send(Message::Pong(data)).await;
                continue;
            }
            Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => continue,
            Some(Err(e)) => {
                warn!("session {session_id}: ws error: {e}");
                break;
            }
        };

        let incoming: IncomingMessage = match serde_json::from_str(&msg) {
            Ok(m) => m,
            Err(e) => {
                warn!("session {session_id}: bad JSON: {e}");
                continue;
            }
        };

        match incoming.msg_type.as_str() {
            "message" => {
                let user_text = match incoming.content {
                    Some(t) if !t.trim().is_empty() => t,
                    _ => continue,
                };
                info!(
                    "session {session_id}: message (trace {:?})",
                    incoming.trace_id
                );

                // Slash commands are handled server-side; anything else goes
                // to the LLM. Keep this list in sync with `/api/commands`.
                if let Some(cmd) = user_text.strip_prefix('/') {
                    let mut parts = cmd.split_whitespace();
                    let reply = match parts.next() {
                        Some("clear") => {
                            // Unified /clear: append a Cleared fact to THIS
                            // session's log — the id does NOT rotate, so the
                            // connection, REST readers, and the FTS index
                            // keep addressing the same chat (no ghost
                            // sessions). derive_messages truncates on the
                            // next turn. Reset the display counters too.
                            match host.clear_session().await {
                                Ok(()) => {
                                    let mut s = session.lock().await;
                                    s.usage_in = 0;
                                    s.usage_out = 0;
                                    s.cache_read = 0;
                                    s.cache_write = 0;
                                    s.last_input_tokens = 0;
                                    Some(OutgoingEvent::content("(session cleared)".to_string()))
                                }
                                Err(e) => Some(OutgoingEvent::error(format!("clear failed: {e}"))),
                            }
                        }
                        Some("help") => Some(OutgoingEvent::content(
                            "commands: /clear  /help".to_string(),
                        )),
                        Some(other) => {
                            Some(OutgoingEvent::error(format!("unknown command /{other}")))
                        }
                        None => None,
                    };
                    if let Some(ev) = reply {
                        // Slash commands are not turns: clear turn_start so the
                        // done event renders without a stale elapsed/usage
                        // summary from a previous turn.
                        session.lock().await.turn_start = None;
                        // Reply + done ride the ordered channel like every
                        // other outbound event — single writer, no shortcuts.
                        let _ = wire_tx.send(WireEvent::Reply(ev));
                        let _ = wire_tx.send(WireEvent::Done);
                    }
                    continue;
                }

                // Record turn start for the done-summary line. Set before
                // run_turn begins so the elapsed time covers the whole turn.
                session.lock().await.turn_start = Some(std::time::Instant::now());

                // The event log is the source of truth: run_turn derives
                // (and compacts) history from it internally.

                // ── Run the turn inline, multiplexing cancel/approval ──
                // run_turn drives the agent loop inline; the sync on_event
                // closure forwards events to the connection-wide wire channel
                // (whose single writer task owns the socket and ordering).
                // This is the same run_turn the CLI uses. On close/error we
                // break immediately: dropping `turn` is cancel-safe (the
                // event log already holds every fact the turn produced), and
                // it stops us re-polling an exhausted ws_rx (a Stream
                // contract violation).
                //
                // Turn serialization: one connection = one Host = one turn
                // at a time. The turn future is created, pinned, and polled
                // to completion inside this match arm — this loop cannot
                // reach a second run_turn until the current one resolves,
                // and Host itself rejects any concurrent run_turn
                // (`TurnInProgress`) as a backstop.
                let turn_wire = wire_tx.clone();
                let mut closing = false;
                let turn_outcome: Option<Result<conga_host::TurnSummary, conga::AgentError>> = {
                    let turn = host.run_turn(&user_text, {
                        let wire = turn_wire.clone();
                        move |ev| {
                            let _ = wire.send(WireEvent::Agent(ev));
                        }
                    });
                    tokio::pin!(turn);

                    let mut outcome = None;
                    loop {
                        tokio::select! {
                            res = &mut turn => {
                                outcome = Some(res);
                                break;
                            }
                            msg = ws_rx.next() => {
                                match msg {
                                    Some(Ok(Message::Text(t))) => {
                                        if let Ok(incoming) =
                                            serde_json::from_str::<IncomingMessage>(&t)
                                        {
                                            match incoming.msg_type.as_str() {
                                                "cancel" => {
                                                    info!("session {session_id}: cancel during turn");
                                                    signal.cancel();
                                                    let _ = cancel_tx.send(true);
                                                }
                                                "approval_response" => {
                                                    match serde_json::from_str::<
                                                        ApprovalResponse,
                                                    >(&t)
                                                    {
                                                        Ok(resp) => {
                                                            lock_registry(&registry).respond(
                                                                &resp.request_id,
                                                                resp.approved,
                                                                resp.remember,
                                                            );
                                                        }
                                                        // Not silently droppable: a malformed
                                                        // response leaves the tool call parked
                                                        // on an approval that never arrives.
                                                        Err(e) => warn!(
                                                            "session {session_id}: malformed \
                                                             approval_response ignored: {e}"
                                                        ),
                                                    }
                                                }
                                                "message" => {
                                                    // A message during a turn is
                                                    // STEERED, not rejected: it
                                                    // enters the loop as a real
                                                    // User message before the
                                                    // next LLM call. Ack so the
                                                    // user sees it queued.
                                                    if let Some(text) = incoming
                                                        .content
                                                        .clone()
                                                        .filter(|t| !t.trim().is_empty())
                                                    {
                                                        host.steer().push(text.clone());
                                                        let _ = turn_wire.send(
                                                            WireEvent::Queued(text),
                                                        );
                                                    }
                                                }
                                                other => warn!(
                                                    "session {session_id}: unknown msg type \
                                                     during turn: {other}"
                                                ),
                                            }
                                        } else {
                                            warn!(
                                                "session {session_id}: unparseable text frame \
                                                 ignored"
                                            );
                                        }
                                    }
                                    Some(Ok(Message::Ping(data))) => {
                                        let _ = session.lock().await.sender.send(Message::Pong(data)).await;
                                    }
                                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => {}
                                    Some(Ok(Message::Close(_))) | None => {
                                        info!("session {session_id}: ws closed during turn");
                                        signal.cancel();
                                        let _ = cancel_tx.send(true);
                                        closing = true;
                                        break;
                                    }
                                    Some(Err(e)) => {
                                        warn!("session {session_id}: ws error during turn: {e}");
                                        signal.cancel();
                                        let _ = cancel_tx.send(true);
                                        closing = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    outcome
                }; // turn dropped

                // Turn boundary: clear in-flight approvals regardless of outcome.
                lock_registry(&registry).clear_pending();

                if !closing {
                    // done/error are queued AFTER every event the turn emitted
                    // (all subagent events were queued before spawn returned),
                    // so the frontend sees a complete picture before the
                    // turn-boundary markers.
                    let _ = wire_tx.send(WireEvent::Done);
                    match turn_outcome {
                        // The log already holds everything the turn produced
                        // (persisted event-by-event); no in-memory history to
                        // rewire.
                        Some(Ok(_summary)) => {}
                        Some(Err(e)) => {
                            let _ = wire_tx.send(WireEvent::Error(format!("{e}")));
                            warn!("session {session_id}: agent error: {e}");
                        }
                        None => {}
                    }
                }

                if closing {
                    break;
                }
            }
            "cancel" => {
                // 回合外 cancel：置 signal + 解锁任何残留审批等待。
                signal.cancel();
                let _ = cancel_tx.send(true);
                info!("session {session_id}: cancel outside turn");
            }
            "approval_response" => {
                // 迟到的审批响应（回合已结束，registry 已 clear）：respond 对未知
                // request_id 是 no-op。畸形帧必须记日志——静默丢弃会让"审批卡住"
                // 这类问题无法诊断。
                match serde_json::from_str::<ApprovalResponse>(&msg) {
                    Ok(resp) => {
                        lock_registry(&registry).respond(
                            &resp.request_id,
                            resp.approved,
                            resp.remember,
                        );
                    }
                    Err(e) => {
                        warn!("session {session_id}: malformed approval_response ignored: {e}")
                    }
                }
            }
            other => {
                warn!("session {session_id}: unknown msg type: {other}");
            }
        }
    }

    info!("session {session_id}: ended");
    state.sessions.remove(&session_id);
    // Last connection gone: the session's process-global tool state (its
    // persistent shell; extension PTYs via cleanup hooks) must die with it,
    // not linger for the lifetime of the gateway. A reconnecting client
    // transparently gets a fresh shell on next use.
    conga_host::cleanup_session_resources(&session_id).await;
}

// ── WS send helper ─────────────────────────────────────────────

async fn send_json(sender: &mut SplitSink<WebSocket, Message>, event: &OutgoingEvent) {
    let text = serde_json::to_string(event).unwrap_or_default();
    if let Err(e) = sender.send(Message::Text(text.into())).await {
        warn!("send failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use dashmap::DashMap;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    /// The server side of a real WebSocket handshake plus a connected
    /// client. The client must stay alive for the lifetime of the socket.
    async fn ws_pair() -> (
        futures_util::stream::SplitSink<WebSocket, Message>,
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().route(
            "/ws",
            get(move |ws: axum::extract::ws::WebSocketUpgrade| {
                let tx = tx.clone();
                async move {
                    ws.on_upgrade(move |socket| async move {
                        let _ = tx.send(socket);
                        futures_util::future::pending::<()>().await;
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let (client, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("the handshake succeeds");
        let socket = rx.recv().await.expect("the server hands over its socket");
        let (sink, _stream) = socket.split();
        (sink, client)
    }

    /// The next JSON event the client receives.
    async fn next_json(
        client: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> serde_json::Value {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
            .await
            .expect("no event arrived within 5s")
            .expect("the stream ended")
            .expect("a websocket error");
        match msg {
            ClientMessage::Text(t) => serde_json::from_str(&t).expect("a JSON event"),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_json_writes_the_event_as_a_json_text_frame() {
        let (mut sink, mut client) = ws_pair().await;

        send_json(&mut sink, &OutgoingEvent::content("hello".into())).await;

        let v = next_json(&mut client).await;
        assert_eq!(v["type"], "content");
        assert_eq!(v["content"], "hello");
    }

    #[tokio::test]
    async fn send_json_serializes_the_done_event_without_a_summary() {
        let (mut sink, mut client) = ws_pair().await;

        send_json(&mut sink, &OutgoingEvent::done()).await;

        let v = next_json(&mut client).await;
        assert_eq!(v["type"], "done");
        assert!(v.get("usage_in").is_none(), "{v}");
    }

    /// A gateway server plus a connected client.
    async fn gateway_client(
        session_id: &str,
    ) -> (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Arc<AppState>,
    ) {
        // Provider env so `SessionAssembly::build` succeeds headless (no
        // network happens during assembly). Process-global; no other
        // gateway test reads these.
        std::env::set_var("CONGA_LLM_BASE_URL", "http://127.0.0.1:9");
        std::env::set_var("CONGA_LLM_KEY", "test-key");
        std::env::set_var("CONGA_LLM_MODEL", "test-model");

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState {
            sessions: DashMap::new(),
            store_root: tmp.path().to_path_buf(),
            index_db: tmp.path().join("index.db"),
            auth_token: Arc::new("t".to_string()),
        });
        let app = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let (client, _resp) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws?user_id={session_id}"))
                .await
                .expect("the handshake succeeds");
        (client, state)
    }

    #[tokio::test]
    async fn handle_ws_answers_a_slash_command_without_calling_the_model() {
        let (mut client, state) = gateway_client("slash-sess").await;

        client
            .send(ClientMessage::Text(
                r#"{"type":"message","content":"/help"}"#.into(),
            ))
            .await
            .unwrap();

        let first = next_json(&mut client).await;
        assert_eq!(first["type"], "content", "{first}");
        assert!(
            first["content"].as_str().unwrap().contains("/clear"),
            "{first}"
        );
        // The slash reply is followed by the turn-boundary marker.
        let second = next_json(&mut client).await;
        assert_eq!(second["type"], "done", "{second}");
        // The connection claims the session id.
        assert!(state.sessions.contains_key("slash-sess"));
    }

    #[tokio::test]
    async fn handle_ws_ignores_a_malformed_json_frame() {
        let (mut client, _state) = gateway_client("bad-json-sess").await;

        client
            .send(ClientMessage::Text("this is not json".into()))
            .await
            .unwrap();
        // Nothing may come back for a bad frame ...
        let quiet =
            tokio::time::timeout(std::time::Duration::from_millis(200), client.next()).await;
        assert!(
            quiet.is_err(),
            "a malformed frame must not produce a reply: {quiet:?}"
        );
        // ... and the session must still work afterwards.
        client
            .send(ClientMessage::Text(
                r#"{"type":"message","content":"/help"}"#.into(),
            ))
            .await
            .unwrap();
        let v = next_json(&mut client).await;
        assert_eq!(v["type"], "content", "{v}");
    }

    /// Two SIMULTANEOUS upgrades of one session id race past the handler's
    /// cheap pre-check; the atomic DashMap `entry` claim in `handle_ws`
    /// must still admit exactly one. Guards both the removed-insert
    /// regression (map stays empty → nothing registers) and the check-then-
    /// act race (map grows to two → state clobbered).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_same_id_upgrades_register_exactly_one() {
        // Provider env so `SessionAssembly::build` succeeds headless (no
        // network happens during assembly). Process-global; no other
        // gateway test reads these.
        std::env::set_var("CONGA_LLM_BASE_URL", "http://127.0.0.1:9");
        std::env::set_var("CONGA_LLM_KEY", "test-key");
        std::env::set_var("CONGA_LLM_MODEL", "test-model");

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState {
            sessions: DashMap::new(),
            store_root: tmp.path().to_path_buf(),
            index_db: tmp.path().join("index.db"),
            auth_token: Arc::new("t".to_string()),
        });
        let app = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("ws://{addr}/ws?user_id=race-sess");
        let (a, b) = tokio::join!(
            tokio_tungstenite::connect_async(url.clone()),
            tokio_tungstenite::connect_async(url),
        );
        // Both handshakes may legitimately succeed (the pre-check cannot
        // close the window); the entry claim decides the winner after.
        let mut streams = Vec::new();
        if let Ok((s, _)) = a {
            streams.push(s);
        }
        if let Ok((s, _)) = b {
            streams.push(s);
        }
        assert!(!streams.is_empty(), "at least one upgrade must succeed");

        // Exactly one registration — never zero (claim exists), never two
        // (claim is atomic). Registration precedes assembly, so this cannot
        // flap on host-build timing.
        wait_until(&state, 10, |n| n == 1, "exactly one session registered").await;

        // Sequential duplicate while one is live: 409 at the handler.
        let third =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws?user_id=race-sess")).await;
        match third {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
            }
            other => panic!("sequential duplicate upgrade must 409, got {other:?}"),
        }

        // Disconnect releases the claim: a reconnect works.
        drop(streams);
        wait_until(&state, 10, |n| n == 0, "entry released on disconnect").await;
    }

    async fn wait_until(
        state: &Arc<AppState>,
        secs: u64,
        pred: impl Fn(usize) -> bool,
        what: &str,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let n = state.sessions.len();
            if pred(n) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for: {what} (sessions.len() == {n})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
