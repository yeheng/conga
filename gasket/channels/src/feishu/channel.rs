//! Feishu (飞书) channel implementation
//!
//! Supports Feishu/Lark bot messaging via webhook and API

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, instrument};

use std::sync::Arc;

use crate::adapter::ImAdapter;
use crate::events::{ChannelType, InboundMessage, OutboundMessage};
use crate::middleware::InboundSender;

/// Feishu channel configuration
#[derive(Debug, Clone)]
pub struct FeishuConfig {
    /// App ID
    pub app_id: String,

    /// App Secret
    pub app_secret: String,

    /// Verification token for webhook validation
    pub verification_token: Option<String>,

    /// Encrypt key for event decryption
    pub encrypt_key: Option<String>,

    /// Allowed users/groups (empty = allow all)
    pub allow_from: Vec<String>,
}

/// Feishu channel.
///
/// Sends incoming messages through `InboundSender` which applies auth/rate-limit
/// checks before forwarding to the message bus.
#[derive(Clone)]
pub struct FeishuChannel {
    config: FeishuConfig,
    inbound_sender: InboundSender,
    client: Client,
    access_token: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl FeishuChannel {
    /// Create a new Feishu channel with an inbound message sender.
    pub fn new(config: FeishuConfig, inbound_sender: InboundSender) -> Self {
        Self {
            config,
            inbound_sender,
            client: Client::new(),
            access_token: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub fn from_config(cfg: &crate::config::FeishuConfig, inbound: InboundSender) -> Self {
        Self::new(
            FeishuConfig {
                app_id: cfg.app_id.clone(),
                app_secret: cfg.app_secret.clone(),
                verification_token: cfg.verification_token.clone(),
                encrypt_key: cfg.encrypt_key.clone(),
                allow_from: cfg.allow_from.clone(),
            },
            inbound,
        )
    }

    /// Build axum routes for Feishu webhooks.
    pub fn routes(&self) -> axum::Router {
        let cloned = self.clone();
        axum::Router::new().route(
            "/feishu/events",
            axum::routing::post(move |body: bytes::Bytes| async move {
                let json_value: serde_json::Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(e) => {
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            format!("Invalid JSON body: {}", e),
                        );
                    }
                };

                // URL verification challenge
                if let Some("url_verification") = json_value.get("type").and_then(|t| t.as_str()) {
                    let challenge: FeishuChallenge = match serde_json::from_value(json_value) {
                        Ok(c) => c,
                        Err(e) => {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                format!("Invalid challenge format: {}", e),
                            );
                        }
                    };
                    let response = FeishuChallengeResponse {
                        challenge: challenge.challenge,
                    };
                    return (
                        axum::http::StatusCode::OK,
                        serde_json::to_string(&response).unwrap_or_default(),
                    );
                }

                let event: FeishuEvent = match serde_json::from_value(json_value) {
                    Ok(e) => e,
                    Err(e) => {
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            format!("Invalid event format: {}", e),
                        );
                    }
                };

                match cloned.handle_webhook_event(event).await {
                    Ok(()) => (
                        axum::http::StatusCode::OK,
                        serde_json::json!({"code": 0}).to_string(),
                    ),
                    Err(e) => {
                        tracing::error!("Feishu event processing failed: {}", e);
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            serde_json::json!({"code": -1, "msg": e.to_string()}).to_string(),
                        )
                    }
                }
            }),
        )
    }

    /// Get tenant access token
    async fn get_access_token(&self) -> anyhow::Result<String> {
        {
            let guard = self.access_token.lock().await;
            if let Some(ref token) = *guard {
                return Ok(token.clone());
            }
        }

        #[derive(Serialize)]
        struct TokenRequest {
            app_id: String,
            app_secret: String,
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            code: i32,
            msg: String,
            tenant_access_token: Option<String>,
            #[allow(dead_code)]
            expire: Option<i64>,
        }

        let response = self
            .client
            .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
            .json(&TokenRequest {
                app_id: self.config.app_id.clone(),
                app_secret: self.config.app_secret.clone(),
            })
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await?;
            anyhow::bail!("Failed to get Feishu access token: {} - {}", status, body);
        }

        let token_response: TokenResponse = response.json().await?;

        if token_response.code != 0 {
            anyhow::bail!(
                "Feishu API error (code={}): {}",
                token_response.code,
                token_response.msg
            );
        }

        let token = token_response.tenant_access_token.ok_or_else(|| {
            anyhow::anyhow!("Feishu API returned code=0 but no tenant_access_token")
        })?;

        let mut guard = self.access_token.lock().await;
        *guard = Some(token);

        info!("Obtained Feishu tenant access token");
        Ok(guard.as_ref().unwrap().clone())
    }

    /// Handle incoming webhook event
    #[instrument(name = "channel.feishu.handle_webhook", skip_all)]
    pub async fn handle_webhook_event(&self, event: FeishuEvent) -> anyhow::Result<()> {
        // Verify token if configured
        if let Some(ref token) = self.config.verification_token {
            if event.token != *token {
                error!("Invalid verification token in Feishu event");
                return Ok(());
            }
        }

        match event.event_type.as_str() {
            "im.message.receive_v1" => {
                if let Some(message) = event.event.message {
                    self.handle_message_event(message, event.event.sender)
                        .await?;
                }
            }
            _ => {
                debug!("Ignoring Feishu event type: {}", event.event_type);
            }
        }

        Ok(())
    }

    /// Handle message receive event
    async fn handle_message_event(
        &self,
        message: FeishuMessage,
        sender: Option<FeishuSender>,
    ) -> anyhow::Result<()> {
        // Check allowlist
        if let Some(ref sender_info) = sender {
            let sender_id = &sender_info.sender_id.user_id;
            if !self.config.allow_from.is_empty() && !self.config.allow_from.contains(sender_id) {
                debug!(
                    "Ignoring message from unauthorized Feishu user: {}",
                    sender_id
                );
                return Ok(());
            }

            // Only handle text messages
            if message.message_type != "text" {
                debug!("Ignoring non-text Feishu message: {}", message.message_type);
                return Ok(());
            }

            // Parse message content (JSON string for text)
            let content = serde_json::from_str::<FeishuTextContent>(&message.content)
                .map(|c| c.text)
                .unwrap_or_else(|_| message.content.clone());

            debug!("Received Feishu message: {}", content);

            let metadata = serde_json::to_value(&message).ok();

            let inbound = InboundMessage {
                channel: ChannelType::Feishu,
                sender_id: sender_info.sender_id.user_id.clone(),
                chat_id: message.chat_id.clone(),
                content,
                media: None,
                metadata,
                timestamp: chrono::Utc::now(),
                trace_id: None,
            };

            self.inbound_sender.send(inbound).await?;
        }

        Ok(())
    }

    /// Determine receive_id_type based on ID prefix
    ///
    /// - `oc` prefix -> `chat_id`
    /// - `ou` prefix -> `open_id`
    fn get_receive_id_type(id: &str) -> &'static str {
        if id.starts_with("ou") {
            "open_id"
        } else {
            // Default to chat_id for "oc" prefix and others
            "chat_id"
        }
    }

    /// Send a text message to a chat
    #[instrument(name = "channel.feishu.send_text", skip(self, text), fields(chat_id = %chat_id))]
    pub async fn send_text(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
        let token = self.get_access_token().await?;

        #[derive(Serialize)]
        struct SendMessageRequest {
            receive_id: String,
            msg_type: String,
            content: String,
        }

        #[derive(Deserialize)]
        struct ApiResponse {
            code: i32,
            msg: String,
        }

        let content = serde_json::to_string(&serde_json::json!({ "text": text }))?;
        let receive_id_type = Self::get_receive_id_type(chat_id);

        let url = format!(
            "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type={}",
            receive_id_type
        );

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .json(&SendMessageRequest {
                receive_id: chat_id.to_string(),
                msg_type: "text".to_string(),
                content,
            })
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await?;
            anyhow::bail!("Failed to send Feishu message: {} - {}", status, body);
        }

        let api_resp: ApiResponse = response.json().await?;
        if api_resp.code != 0 {
            anyhow::bail!(
                "Feishu send message API error (code={}): {}",
                api_resp.code,
                api_resp.msg
            );
        }

        debug!("Sent Feishu message to chat: {}", chat_id);
        Ok(())
    }
}

#[async_trait]
impl ImAdapter for FeishuChannel {
    fn name(&self) -> &str {
        "feishu"
    }

    async fn start(&self, _inbound: InboundSender) -> anyhow::Result<()> {
        info!("Starting Feishu channel");
        // Pre-fetch access token
        self.get_access_token().await?;
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> anyhow::Result<()> {
        self.send_text(msg.chat_id(), msg.content()).await
    }
}

pub type FeishuAdapter = FeishuChannel;

// Feishu API types

/// Feishu webhook event
#[derive(Debug, Deserialize)]
pub struct FeishuEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub token: String,
    pub event: FeishuEventData,
}

/// Feishu event data
#[derive(Debug, Deserialize)]
pub struct FeishuEventData {
    #[serde(rename = "type")]
    pub event_type: String,
    pub sender: Option<FeishuSender>,
    pub message: Option<FeishuMessage>,
}

/// Feishu message sender
#[derive(Debug, Deserialize)]
pub struct FeishuSender {
    pub sender_id: FeishuSenderId,
    pub sender_type: String,
}

/// Feishu sender ID
#[derive(Debug, Deserialize)]
pub struct FeishuSenderId {
    #[serde(rename = "open_id")]
    pub open_id: String,
    pub user_id: String,
    pub union_id: String,
}

/// Feishu message
#[derive(Debug, Deserialize, Serialize)]
pub struct FeishuMessage {
    pub message_id: String,
    #[serde(rename = "root_id")]
    pub root_message_id: Option<String>,
    #[serde(rename = "parent_id")]
    pub parent_message_id: Option<String>,
    pub create_time: String,
    pub chat_id: String,
    pub message_type: String,
    pub content: String,
    pub mentions: Option<Vec<FeishuMention>>,
}

/// Feishu message mention
#[derive(Debug, Deserialize, Serialize)]
pub struct FeishuMention {
    pub key: String,
    pub id: FeishuMentionId,
    pub name: String,
    #[serde(rename = "type")]
    pub mention_type: String,
}

/// Feishu mention ID
#[derive(Debug, Deserialize, Serialize)]
pub struct FeishuMentionId {
    pub open_id: String,
    pub user_id: String,
}

/// Feishu text content
#[derive(Debug, Deserialize)]
pub struct FeishuTextContent {
    pub text: String,
}

/// Feishu webhook challenge response
#[derive(Debug, Deserialize)]
pub struct FeishuChallenge {
    pub challenge: String,
    pub token: String,
    #[serde(rename = "type")]
    pub challenge_type: String,
}

/// Feishu challenge response
#[derive(Debug, Serialize)]
pub struct FeishuChallengeResponse {
    pub challenge: String,
}

/// Stateless send: obtain a tenant access token and send a text message in one shot.
///
/// This avoids the need to keep a `FeishuChannel` instance alive just for sending.
pub async fn send_text_stateless(
    app_id: &str,
    app_secret: &str,
    chat_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let client = Client::new();

    // 1. Get tenant access token
    #[derive(Serialize)]
    struct TokenRequest {
        app_id: String,
        app_secret: String,
    }
    #[derive(Deserialize)]
    struct TokenResponse {
        code: i32,
        msg: String,
        tenant_access_token: Option<String>,
    }
    let resp = client
        .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
        .json(&TokenRequest {
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
        })
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Failed to get Feishu access token: {}", resp.status());
    }
    let token_resp: TokenResponse = resp.json().await?;
    if token_resp.code != 0 {
        anyhow::bail!(
            "Feishu token API error (code={}): {}",
            token_resp.code,
            token_resp.msg
        );
    }
    let token = token_resp
        .tenant_access_token
        .ok_or_else(|| anyhow::anyhow!("No tenant_access_token returned"))?;

    // 2. Send the message
    #[derive(Serialize)]
    struct SendReq {
        receive_id: String,
        msg_type: String,
        content: String,
    }
    #[derive(Deserialize)]
    struct ApiResp {
        code: i32,
        msg: String,
    }

    let receive_id_type = if chat_id.starts_with("ou") {
        "open_id"
    } else {
        "chat_id"
    };
    let url = format!(
        "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type={}",
        receive_id_type
    );
    let content = serde_json::to_string(&serde_json::json!({ "text": text }))?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&SendReq {
            receive_id: chat_id.to_string(),
            msg_type: "text".to_string(),
            content,
        })
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await?;
        anyhow::bail!("Feishu send failed: {} - {}", status, body);
    }
    let api_resp: ApiResp = resp.json().await?;
    if api_resp.code != 0 {
        anyhow::bail!(
            "Feishu send API error (code={}): {}",
            api_resp.code,
            api_resp.msg
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_sender() -> InboundSender {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        InboundSender::new(tx)
    }

    #[test]
    fn test_feishu_config_creation() {
        let config = FeishuConfig {
            app_id: "cli_test123".to_string(),
            app_secret: "secret123".to_string(),
            verification_token: Some("token123".to_string()),
            encrypt_key: None,
            allow_from: vec![],
        };

        assert_eq!(config.app_id, "cli_test123");
        assert_eq!(config.app_secret, "secret123");
    }

    #[test]
    fn test_feishu_channel_creation() {
        let config = FeishuConfig {
            app_id: "cli_test".to_string(),
            app_secret: "secret".to_string(),
            verification_token: None,
            encrypt_key: None,
            allow_from: vec![],
        };

        let channel = FeishuChannel::new(config, create_test_sender());

        assert_eq!(channel.name(), "feishu");
    }

    #[test]
    fn test_parse_feishu_text_content() {
        let json = r#"{"text":"Hello from Feishu!"}"#;
        let content: FeishuTextContent = serde_json::from_str(json).unwrap();
        assert_eq!(content.text, "Hello from Feishu!");
    }

    #[test]
    fn test_feishu_challenge_response() {
        let challenge = FeishuChallenge {
            challenge: "test_challenge".to_string(),
            token: "token123".to_string(),
            challenge_type: "url_verification".to_string(),
        };

        let response = FeishuChallengeResponse {
            challenge: challenge.challenge,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("test_challenge"));
    }

    #[test]
    fn test_get_receive_id_type() {
        // oc prefix should return chat_id
        assert_eq!(FeishuChannel::get_receive_id_type("oc_xxx"), "chat_id");
        assert_eq!(
            FeishuChannel::get_receive_id_type("oc_1234567890abcdef"),
            "chat_id"
        );

        // ou prefix should return open_id
        assert_eq!(FeishuChannel::get_receive_id_type("ou_xxx"), "open_id");
        assert_eq!(
            FeishuChannel::get_receive_id_type("ou_1234567890abcdef"),
            "open_id"
        );

        // other prefixes default to chat_id
        assert_eq!(FeishuChannel::get_receive_id_type("xxx"), "chat_id");
        assert_eq!(FeishuChannel::get_receive_id_type("cli_xxx"), "chat_id");
    }
}
