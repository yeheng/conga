//! MessageHandler wrapper that routes slash commands through the Dispatcher
//! before falling back to the LLM pipeline.
//!
//! This bridges `crate::command::Dispatcher` with `gasket-broker::MessageHandler`,
//! so WebSocket (and other streaming channels) get the same slash-command
//! behavior as the CLI REPL.

use std::sync::Arc;

use async_trait::async_trait;
use gasket_engine::broker::session::MessageHandler;
use gasket_engine::bus_adapter::EngineHandler;
use gasket_types::events::{ChannelType, ChatEvent, OutboundMessage};
use gasket_types::SessionKey;

use crate::command::{CommandResult, Dispatcher, RouteOutcome};

/// Wraps an [`EngineHandler`] with a [`Dispatcher`] so that slash commands
/// are intercepted before they reach the LLM.
pub struct DispatchingEngineHandler {
    engine: EngineHandler,
    dispatcher: Arc<Dispatcher>,
}

impl DispatchingEngineHandler {
    pub fn new(engine: EngineHandler, dispatcher: Arc<Dispatcher>) -> Self {
        Self { engine, dispatcher }
    }
}

#[async_trait]
impl MessageHandler for DispatchingEngineHandler {
    async fn try_intercept(
        &self,
        session_key: &SessionKey,
        msg: gasket_types::events::InboundMessage,
    ) -> Result<(), gasket_types::events::InboundMessage> {
        self.engine.try_intercept(session_key, msg).await
    }

    async fn handle_message(
        &self,
        session_key: &SessionKey,
        message: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let outcome = self.dispatcher.route(message, session_key).await;
        tracing::debug!(
            session_key = %session_key,
            message = message,
            outcome = ?outcome,
            "Dispatcher routing result"
        );
        match outcome {
            RouteOutcome::Handled(CommandResult::Print(s)) => Ok(s),
            RouteOutcome::Handled(CommandResult::Error(s)) => Ok(s),
            RouteOutcome::Handled(CommandResult::Quit) => {
                Ok("Quit is not supported in this channel.".to_string())
            }
            RouteOutcome::Rewrite {
                prompt,
                tool_filter,
            } => {
                self.engine
                    .handle_message(session_key, &prompt)
                    .await
                    .map(|mut text| {
                        // Append tool-filter hint for debugging
                        if tool_filter.is_some() {
                            text.push_str("\n\n[tool filter applied]");
                        }
                        text
                    })
            }
            RouteOutcome::Passthrough(text) => {
                if message.trim_start().starts_with('/') {
                    tracing::warn!(
                        session_key = %session_key,
                        message = message,
                        "Message looks like a slash command but was parsed as passthrough; \
                         this usually means the message content was malformed"
                    );
                    return Ok(format!(
                        "Unknown command format: {:?}. Please send commands as plain text.",
                        message
                    ));
                }
                self.engine.handle_message(session_key, &text).await
            }
        }
    }

    async fn handle_streaming_message(
        &self,
        message: &str,
        session_key: &SessionKey,
        _tool_filter: Option<Vec<String>>,
    ) -> Result<
        (
            tokio::sync::mpsc::Receiver<ChatEvent>,
            tokio::sync::oneshot::Receiver<
                Result<OutboundMessage, Box<dyn std::error::Error + Send + Sync>>,
            >,
        ),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let session_key_owned = session_key.clone();

        let outcome = self.dispatcher.route(message, session_key).await;
        tracing::debug!(
            session_key = %session_key,
            message = message,
            outcome = ?outcome,
            "Dispatcher routing result (streaming)"
        );
        match outcome {
            RouteOutcome::Handled(CommandResult::Print(text)) => {
                let (chat_tx, chat_rx) = tokio::sync::mpsc::channel(2);
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();

                // Strip ANSI escape sequences for WebSocket clients
                let display_text = if text.starts_with('\x1B') {
                    "Screen cleared.".to_string()
                } else {
                    text
                };

                tokio::spawn(async move {
                    let _ = chat_tx.send(ChatEvent::text(&display_text)).await;
                    let _ = chat_tx.send(ChatEvent::done()).await;

                    let outbound = OutboundMessage::new(
                        ChannelType::WebSocket,
                        session_key_owned.to_string(),
                        display_text,
                    );
                    let _ = result_tx.send(Ok(outbound));
                });

                Ok((chat_rx, result_rx))
            }
            RouteOutcome::Handled(CommandResult::Error(text)) => {
                let (chat_tx, chat_rx) = tokio::sync::mpsc::channel(2);
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();

                tokio::spawn(async move {
                    let _ = chat_tx.send(ChatEvent::error(&text)).await;
                    let _ = chat_tx.send(ChatEvent::done()).await;

                    let outbound = OutboundMessage::new(
                        ChannelType::WebSocket,
                        session_key_owned.to_string(),
                        text,
                    );
                    let _ = result_tx.send(Ok(outbound));
                });

                Ok((chat_rx, result_rx))
            }
            RouteOutcome::Handled(CommandResult::Quit) => {
                let (chat_tx, chat_rx) = tokio::sync::mpsc::channel(2);
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();

                let msg = "Quit is not supported in this channel.".to_string();
                tokio::spawn(async move {
                    let _ = chat_tx.send(ChatEvent::text(&msg)).await;
                    let _ = chat_tx.send(ChatEvent::done()).await;

                    let outbound = OutboundMessage::new(
                        ChannelType::WebSocket,
                        session_key_owned.to_string(),
                        msg,
                    );
                    let _ = result_tx.send(Ok(outbound));
                });

                Ok((chat_rx, result_rx))
            }
            RouteOutcome::Rewrite {
                prompt,
                tool_filter,
            } => {
                self.engine
                    .handle_streaming_message(&prompt, session_key, tool_filter)
                    .await
            }
            RouteOutcome::Passthrough(text) => {
                if message.trim_start().starts_with('/') {
                    tracing::warn!(
                        session_key = %session_key,
                        message = message,
                        "Message looks like a slash command but was parsed as passthrough; \
                         this usually means the message content was malformed"
                    );
                    let (chat_tx, chat_rx) = tokio::sync::mpsc::channel(2);
                    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                    let msg = format!(
                        "Unknown command format: {:?}. Please send commands as plain text.",
                        message
                    );
                    tokio::spawn(async move {
                        let _ = chat_tx.send(ChatEvent::error(&msg)).await;
                        let _ = chat_tx.send(ChatEvent::done()).await;
                        let outbound = OutboundMessage::new(
                            ChannelType::WebSocket,
                            session_key_owned.to_string(),
                            msg,
                        );
                        let _ = result_tx.send(Ok(outbound));
                    });
                    return Ok((chat_rx, result_rx));
                }
                self.engine
                    .handle_streaming_message(&text, session_key, None)
                    .await
            }
        }
    }
}
