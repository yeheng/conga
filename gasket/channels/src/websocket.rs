use std::sync::Arc;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Query, State},
    response::IntoResponse,
};
use dashmap::DashMap;
use futures_util::{sink::SinkExt, stream::StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::approval_router::ApprovalRouter;
use crate::events::ChannelType::WebSocket as WebSocketChannel;
use crate::events::{ChatEvent, InboundMessage, OutboundMessage};

type ConnectionId = String;
type UserId = String;

/// Authentication validator function type
type AuthValidator = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Maximum number of concurrent WebSocket connections
const MAX_CONNECTIONS: usize = 1000;

/// Channel size for per-connection message queue (backpressure)
const CONNECTION_CHANNEL_SIZE: usize = 100;

/// Query parameters for WebSocket connection
#[derive(Debug, serde::Deserialize)]
pub struct WebSocketQuery {
    /// Optional authentication token
    pub token: Option<String>,
    /// Optional user ID (defaults to connection ID if not provided)
    pub user_id: Option<String>,
}

/// Unified guard for connection cleanup on disconnect.
///
/// Uses ownership verification (compare-and-swap pattern) to prevent
/// a stale guard from destroying a newer connection for the same user.
/// This handles the race where:
///   1. User disconnects → guard drop spawns async cleanup
///   2. User reconnects → new connection registered
///   3. Old cleanup task runs → must NOT remove the new connection.
struct ConnectionGuard {
    manager: Arc<WebSocketManager>,
    connection_id: ConnectionId,
    user_id: UserId,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let manager = self.manager.clone();
        let connection_id = self.connection_id.clone();
        let user_id = self.user_id.clone();

        // Remove from connections map
        manager.connections.remove(&connection_id);

        // Only remove user mapping if it still points to OUR connection_id.
        // DashMap's remove_if allows atomic check-and-remove.
        let was_removed = manager
            .user_connections
            .remove_if(&user_id, |_, current_conn_id| {
                let is_ours = *current_conn_id == connection_id;
                if is_ours {
                    debug!(
                        "Connection guard cleaned up user {} and connection {}",
                        user_id, connection_id
                    );
                } else {
                    debug!(
                    "Connection guard skipped user {} cleanup: current connection is {}, not {}",
                    user_id, current_conn_id, connection_id
                );
                }
                is_ours
            })
            .is_some();

        // If we actually cleaned up this user's mapping, also clear remembered approvals
        // so that a fresh connection does not inherit stale decisions.
        if was_removed {
            if let Ok(router_guard) = manager.approval_router.read() {
                if let Some(ref router) = *router_guard {
                    let session_key = crate::events::SessionKey::new(
                        crate::events::ChannelType::WebSocket,
                        &user_id,
                    );
                    router.forget_session(&session_key);
                }
            }
        }
    }
}

/// Manages active WebSocket connections
pub struct WebSocketManager {
    /// Map of active connections
    /// Key: Connection ID (UUID)
    /// Value: Sender to the connection's write loop
    ///
    /// Uses DashMap for better concurrent performance - operations only lock
    /// the relevant shard, not the entire map.
    connections: DashMap<ConnectionId, mpsc::Sender<Message>>,

    /// Map of user IDs to connection IDs (for routing messages to specific users)
    user_connections: DashMap<UserId, ConnectionId>,

    /// Sender to the message bus for inbound messages
    inbound_tx: crate::middleware::InboundSender,

    /// Optional authentication token validator (can be set via set_auth_validator)
    auth_validator: std::sync::RwLock<Option<AuthValidator>>,

    /// Router for approval request/response pairing
    approval_router: std::sync::RwLock<Option<Arc<ApprovalRouter>>>,
}

impl WebSocketManager {
    pub fn new(inbound_tx: crate::middleware::InboundSender) -> Self {
        Self {
            connections: DashMap::new(),
            user_connections: DashMap::new(),
            inbound_tx,
            auth_validator: std::sync::RwLock::new(None),
            approval_router: std::sync::RwLock::new(None),
        }
    }

    /// Set the approval router for handling tool confirmation flows.
    pub fn set_approval_router(&self, router: Arc<ApprovalRouter>) {
        if let Ok(mut guard) = self.approval_router.write() {
            *guard = Some(router);
        }
    }

    /// Send an inbound message through the middleware pipeline.
    async fn send_inbound(&self, inbound: InboundMessage) -> Result<(), String> {
        self.inbound_tx
            .send(inbound)
            .await
            .map_err(|e| e.to_string())
    }

    /// Set an authentication validator function
    pub fn set_auth_validator<F>(&self, validator: F)
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        // Use std::sync::RwLock since this is a one-time setup operation
        if let Ok(mut guard) = self.auth_validator.write() {
            *guard = Some(Arc::new(validator));
        }
    }

    /// Get the number of active connections
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Handle a new WebSocket connection
    pub async fn handle_connection(
        ws: WebSocketUpgrade,
        State(manager): State<Arc<WebSocketManager>>,
        Query(query): Query<WebSocketQuery>,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |socket| handle_socket(socket, manager, query))
    }

    /// Send an outbound message to a specific user/connection
    pub async fn send(&self, msg: OutboundMessage) {
        // Try to find the connection by chat_id (which could be user_id or connection_id)
        let chat_id = msg.chat_id();
        let connection_id = if let Some(conn_id) = self.user_connections.get(chat_id) {
            // chat_id is a user_id, look up the connection
            conn_id.value().clone()
        } else if self.connections.contains_key(chat_id) {
            // chat_id is already a connection_id
            chat_id.to_string()
        } else {
            warn!(
                "No connection found for chat_id: {} (user_connections: {:?})",
                chat_id,
                self.user_connections
                    .iter()
                    .map(|e| e.key().clone())
                    .collect::<Vec<_>>()
            );
            return;
        };

        let sender = self
            .connections
            .get(&connection_id)
            .map(|entry| entry.value().clone());

        if let Some(sender) = sender {
            let text = Self::message_text(&msg);
            if text.is_empty() {
                warn!("WebSocketManager::send - empty message, skipping");
                return;
            }

            if let Err(e) = sender.send(Message::Text(text.into())).await {
                warn!(
                    "Failed to send message to connection {}: {}",
                    connection_id, e
                );
            }
        } else {
            warn!(
                "Connection {} not found for outbound message",
                connection_id
            );
        }
    }

    /// Broadcast an outbound message to all active WebSocket connections.
    pub async fn broadcast(&self, msg: &OutboundMessage) {
        let text = Self::message_text(msg);
        if text.is_empty() {
            warn!("WebSocketManager::broadcast - empty message, skipping");
            return;
        }

        let count = self.connections.len();
        if count == 0 {
            debug!("WebSocketManager::broadcast - no active connections");
            return;
        }

        let text_arc: std::sync::Arc<str> = text.into();
        let mut failed = 0usize;
        let senders: Vec<(ConnectionId, mpsc::Sender<Message>)> = self
            .connections
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();

        for (connection_id, sender) in senders {
            if let Err(e) = sender
                .send(Message::Text(text_arc.to_string().into()))
                .await
            {
                warn!("Failed to broadcast to connection {}: {}", connection_id, e);
                failed += 1;
            }
        }

        info!(
            "Broadcasted WebSocket message to {}/{} connections",
            count.saturating_sub(failed),
            count
        );
    }

    fn message_text(msg: &OutboundMessage) -> String {
        if let Some(ws_msg) = msg.ws_message() {
            ws_msg.to_json()
        } else if !msg.content().is_empty() {
            ChatEvent::text(msg.content()).to_json()
        } else {
            String::new()
        }
    }
}

async fn handle_socket(socket: WebSocket, manager: Arc<WebSocketManager>, query: WebSocketQuery) {
    let (mut sender, mut receiver) = socket.split();

    // Create a unique ID for this connection
    let connection_id = uuid::Uuid::new_v4().to_string();

    // Determine user ID (use provided or default to connection ID)
    let user_id = query.user_id.unwrap_or_else(|| connection_id.clone());

    // Authenticate if token is provided
    if let Some(token) = &query.token {
        // Use std::sync::RwLock since auth validation is a quick synchronous operation
        let validator = manager.auth_validator.read().unwrap();
        if let Some(ref validator_fn) = *validator {
            if !validator_fn(token) {
                warn!(
                    "Authentication failed for connection {}: invalid token",
                    connection_id
                );
                return;
            }
        }
    }

    debug!(
        "New WebSocket connection: {} (user: {})",
        connection_id, user_id
    );

    // Check connection limit (synchronous with DashMap)
    let current_connections = manager.connection_count();
    if current_connections >= MAX_CONNECTIONS {
        warn!(
            "Connection limit reached ({}/{}), rejecting new connection",
            current_connections, MAX_CONNECTIONS
        );
        return;
    }

    // Create a bounded channel for sending messages to this connection (backpressure)
    let (tx, mut rx) = mpsc::channel(CONNECTION_CHANNEL_SIZE);

    // Register the connection using DashMap (no explicit locking needed)
    // If user already has a connection, remove the old one (single connection per user)
    if let Some(old_conn_id) = manager.user_connections.get(&user_id) {
        let old_id = old_conn_id.value().clone();
        manager.connections.remove(&old_id);
        info!("Replaced old connection {} for user {}", old_id, user_id);
    }

    manager.connections.insert(connection_id.clone(), tx);
    manager
        .user_connections
        .insert(user_id.clone(), connection_id.clone());

    info!(
        "WebSocket connected: {} (user: {}), total connections: {}",
        connection_id,
        user_id,
        manager.connection_count()
    );

    // Single unified guard handles both connection and user mapping cleanup.
    // Uses ownership verification to avoid destroying newer connections.
    let _guard = ConnectionGuard {
        manager: manager.clone(),
        connection_id: connection_id.clone(),
        user_id: user_id.clone(),
    };

    // Spawn a task to forward messages from the channel to the WebSocket
    let mut send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(msg).await.is_err() {
                debug!("Send error, connection may be closed");
                break;
            }
        }
    });

    // Handle incoming messages
    let mut recv_task = {
        let manager = manager.clone();
        let connection_id = connection_id.clone();
        let user_id = user_id.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = receiver.next().await {
                match msg {
                    Message::Text(text) => {
                        debug!("incoming messages: {}", text);

                        // Try to parse as JSON first (modern wire format with trace_id)
                        let json_payload: Option<serde_json::Value> =
                            serde_json::from_str(&text).ok();

                        if let Some(ref json) = json_payload {
                            let msg_type = json.get("type").and_then(|t| t.as_str());

                            // Approval response
                            if msg_type == Some("approval_response") {
                                let router_guard = manager.approval_router.read().ok();
                                if let Some(Some(ref router)) = router_guard.as_deref() {
                                    if let Ok(response) = serde_json::from_value::<
                                        gasket_types::ToolApprovalResponse,
                                    >(
                                        json.clone()
                                    ) {
                                        debug!(
                                            "Routing approval_response for request {}",
                                            response.request_id
                                        );
                                        router.resolve(&response);
                                    }
                                }
                                continue;
                            }

                            // Structured message with trace_id
                            if msg_type == Some("message") {
                                let content = json
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(&text)
                                    .to_string();
                                let trace_id = json
                                    .get("trace_id")
                                    .and_then(|v| v.as_str())
                                    .map(String::from);

                                tracing::info!(
                                    target: "gasket::websocket",
                                    connection_id = %connection_id,
                                    user_id = %user_id,
                                    trace_id = ?trace_id,
                                    "WebSocket received message"
                                );

                                let inbound = InboundMessage {
                                    channel: WebSocketChannel,
                                    sender_id: user_id.clone(),
                                    chat_id: user_id.clone(),
                                    content,
                                    media: None,
                                    metadata: None,
                                    timestamp: chrono::Utc::now(),
                                    trace_id,
                                };

                                if let Err(e) = manager.send_inbound(inbound).await {
                                    error!("Failed to forward inbound message: {}", e);
                                    break;
                                }
                                continue;
                            }
                        }

                        // Fallback: plain text (legacy or unknown JSON)
                        let trace_id = json_payload
                            .as_ref()
                            .and_then(|j| j.get("trace_id"))
                            .and_then(|v| v.as_str())
                            .map(String::from);

                        tracing::info!(
                            target: "gasket::websocket",
                            connection_id = %connection_id,
                            user_id = %user_id,
                            trace_id = ?trace_id,
                            "WebSocket received plain-text message"
                        );

                        let inbound = InboundMessage {
                            channel: WebSocketChannel,
                            sender_id: user_id.clone(),
                            chat_id: user_id.clone(), // Use user_id for session persistence
                            content: text.to_string(),
                            media: None,
                            metadata: None,
                            timestamp: chrono::Utc::now(),
                            trace_id,
                        };

                        if let Err(e) = manager.send_inbound(inbound).await {
                            error!("Failed to forward inbound message: {}", e);
                            break;
                        }
                    }
                    Message::Close(_) => {
                        tracing::trace!("Received close frame from {}", connection_id);
                        break;
                    }
                    Message::Ping(_data) => {
                        // Handle ping for keepalive
                        tracing::trace!("Received ping from {}", connection_id);
                        // Note: axum handles pong automatically
                    }
                    _ => {}
                }
            }
        })
    };

    // Wait for either task to finish
    tokio::select! {
        _ = (&mut send_task) => {
            tracing::trace!("Send task finished for {}", connection_id);
        },
        _ = (&mut recv_task) => {
            tracing::trace!("Recv task finished for {}", connection_id);
        },
    }

    // Guard will clean up automatically
    debug!("WebSocket connection closed: {}", connection_id);
}

// === Broadcast HTTP API ====================================================

/// Request body for POST /broadcast
#[derive(Debug, serde::Deserialize)]
pub struct BroadcastRequest {
    /// Plain text message (ignored if `ws_message` is present)
    pub content: Option<String>,
    /// Structured WebSocket message
    pub ws_message: Option<crate::events::WebSocketMessage>,
}

async fn handle_broadcast(
    State(manager): State<Arc<WebSocketManager>>,
    axum::extract::Json(req): axum::extract::Json<BroadcastRequest>,
) -> impl IntoResponse {
    let msg = if let Some(ws_msg) = req.ws_message {
        crate::events::OutboundMessage::broadcast_ws_message(
            crate::events::ChannelType::WebSocket,
            ws_msg,
        )
    } else {
        crate::events::OutboundMessage::broadcast(
            crate::events::ChannelType::WebSocket,
            req.content.unwrap_or_default(),
        )
    };
    manager.broadcast(&msg).await;
    let body = serde_json::json!({"status": "ok", "connections": manager.connection_count()});
    (axum::http::StatusCode::OK, body.to_string())
}

// === WebSocket Adapter =====================================================

use crate::adapter::ImAdapter;
use crate::middleware::InboundSender;
use async_trait::async_trait;

/// WebSocket adapter — delegates outbound sends to WebSocketManager.
///
/// Inbound messages are handled by the HTTP handler in gateway.rs, so
/// `start()` is a no-op.
#[derive(Clone)]
pub struct WebSocketAdapter {
    manager: Arc<WebSocketManager>,
}

impl WebSocketAdapter {
    pub fn new(manager: Arc<WebSocketManager>) -> Self {
        Self { manager }
    }

    pub fn from_config(_cfg: &crate::config::WebSocketConfig, inbound: InboundSender) -> Self {
        let manager = Arc::new(WebSocketManager::new(inbound));
        Self { manager }
    }

    /// Return the WebSocket upgrade route and broadcast endpoint.
    pub fn routes(&self) -> axum::Router {
        let manager = self.manager.clone();
        axum::Router::new()
            .route(
                "/ws",
                axum::routing::get(WebSocketManager::handle_connection),
            )
            .route("/broadcast", axum::routing::post(handle_broadcast))
            .with_state(manager)
    }

    /// Return a reference to the inner [`WebSocketManager`].
    pub fn manager(&self) -> &Arc<WebSocketManager> {
        &self.manager
    }
}

#[async_trait]
impl ImAdapter for WebSocketAdapter {
    fn name(&self) -> &str {
        "websocket"
    }

    async fn start(&self, _inbound: InboundSender) -> anyhow::Result<()> {
        // Inbound is handled by the axum WebSocket handler.
        Ok(())
    }

    async fn send(&self, msg: &crate::events::OutboundMessage) -> anyhow::Result<()> {
        if msg.is_broadcast() {
            self.manager.broadcast(msg).await;
        } else {
            self.manager.send(msg.clone()).await;
        }
        Ok(())
    }
}

/// CLI adapter — no-op for outbound messages.
#[derive(Clone, Copy)]
pub struct CliAdapter;

#[async_trait]
impl ImAdapter for CliAdapter {
    fn name(&self) -> &str {
        "cli"
    }

    async fn start(&self, _inbound: InboundSender) -> anyhow::Result<()> {
        Ok(())
    }

    async fn send(&self, _msg: &crate::events::OutboundMessage) -> anyhow::Result<()> {
        Ok(())
    }
}

// ── WebSocket Approval Callback ────────────────────────────────────────────

use gasket_types::{ApprovalCallback, SessionKey, ToolApprovalRequest};

/// WebSocket-driven implementation of [`ApprovalCallback`].
///
/// Sends `ChatEvent::ApprovalRequest` to the user over WebSocket and blocks
/// until the user replies with `approval_response` or a timeout expires.
#[derive(Clone)]
pub struct WebSocketApprovalCallback {
    manager: Arc<WebSocketManager>,
    router: Arc<ApprovalRouter>,
    timeout: std::time::Duration,
}

impl WebSocketApprovalCallback {
    /// Create a new callback with the given manager and router.
    pub fn new(manager: Arc<WebSocketManager>, router: Arc<ApprovalRouter>) -> Self {
        Self {
            manager,
            router,
            timeout: std::time::Duration::from_secs(300),
        }
    }

    /// Set a custom timeout for waiting on user approval.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl ApprovalCallback for WebSocketApprovalCallback {
    async fn request_approval(
        &self,
        session_key: &SessionKey,
        request: ToolApprovalRequest,
    ) -> Result<bool, String> {
        // Check if we have a remembered decision for this session + tool
        if let Some(remembered) = self.router.is_remembered(session_key, &request.tool_name) {
            if remembered {
                debug!(
                    "Auto-approving {} for session {} (remembered)",
                    request.tool_name, session_key
                );
                return Ok(true);
            } else {
                debug!(
                    "Auto-denying {} for session {} (remembered)",
                    request.tool_name, session_key
                );
                return Ok(false);
            }
        }

        let id = uuid::Uuid::parse_str(&request.id)
            .map_err(|e| format!("Invalid approval request id: {}", e))?;

        let rx = self.router.register(id);

        let ws_msg = OutboundMessage::with_ws_message(
            WebSocketChannel,
            session_key.chat_id.clone(),
            gasket_types::events::ChatEvent::approval_request(
                &request.id,
                &request.tool_name,
                &request.description,
                &request.arguments,
            ),
        );
        self.manager.send(ws_msg).await;

        let response = self.router.wait_for_response(id, self.timeout, rx).await?;

        // If the user asked us to remember this decision, store it
        if response.remember {
            self.router
                .remember(session_key, &request.tool_name, response.approved);
        }

        Ok(response.approved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_websocket_query_parsing() {
        // Test that WebSocketQuery can be deserialized
        let query = WebSocketQuery {
            token: Some("test-token".to_string()),
            user_id: Some("user-123".to_string()),
        };
        assert_eq!(query.token, Some("test-token".to_string()));
        assert_eq!(query.user_id, Some("user-123".to_string()));
    }
}
