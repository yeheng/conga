//! Channel middleware infrastructure
//!
//! Provides simple, non-generic middleware utilities for channel operations.
//! Removed the over-engineered generic middleware stack in favor of direct method calls.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::events::InboundMessage;

// ── ChannelError ──────────────────────────────────────────

/// Structured error type for channel operations.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// The channel is not connected or not started.
    #[error("Channel '{channel}' is not connected")]
    NotConnected { channel: String },

    /// Authentication with the channel service failed.
    #[error("Auth error for channel '{channel}': {message}")]
    AuthError { channel: String, message: String },

    /// The message could not be delivered.
    #[error("Delivery failed for channel '{channel}': {message}")]
    DeliveryFailed { channel: String, message: String },

    /// Rate limited by the channel service.
    #[error("Rate limited by channel '{channel}'")]
    RateLimited { channel: String },

    /// The message format is invalid for this channel.
    #[error("Invalid message format: {0}")]
    InvalidFormat(String),

    /// Other/unknown errors.
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl ChannelError {
    /// Get the channel name associated with this error (if any).
    pub fn channel(&self) -> Option<&str> {
        match self {
            Self::NotConnected { channel }
            | Self::AuthError { channel, .. }
            | Self::DeliveryFailed { channel, .. }
            | Self::RateLimited { channel } => Some(channel),
            _ => None,
        }
    }

    /// Whether this error is likely transient and retryable.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::DeliveryFailed { .. })
    }
}

// ── SimpleRateLimiter ─────────────────────────────────────

/// Simple token-bucket rate limiter for inbound messages per sender.
///
/// Allows at most `max_messages` per sender within any rolling `window` period.
pub struct SimpleRateLimiter {
    max_messages: u32,
    window: std::time::Duration,
    /// sender_id -> deque of timestamps
    timestamps: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl SimpleRateLimiter {
    /// Create a rate limiter allowing `max_messages` per sender per `window`.
    pub fn new(max_messages: u32, window: std::time::Duration) -> Self {
        Self {
            max_messages,
            window,
            timestamps: Mutex::new(HashMap::new()),
        }
    }

    /// Check if a message from the given sender is allowed.
    /// Returns true if allowed, false if rate limited.
    ///
    /// Only cleans the current sender's timestamp deque — O(1) regardless of
    /// total user count. Use [`Self::prune_stale`] from a background task to
    /// reclaim memory from idle senders.
    pub fn check(&self, sender_id: &str) -> bool {
        let mut map = self.timestamps.lock().unwrap();
        let now = Instant::now();

        let ts = map.entry(sender_id.to_string()).or_default();

        // Evict expired entries for this sender only
        while let Some(&front) = ts.front() {
            if now.duration_since(front) > self.window {
                ts.pop_front();
            } else {
                break;
            }
        }

        if ts.len() < self.max_messages as usize {
            ts.push_back(now);
            true
        } else {
            false
        }
    }

    /// Prune all senders whose timestamps have fully expired.
    ///
    /// Intended to be called periodically (e.g. every 5 minutes) from a
    /// background tokio task alongside other housekeeping work. This keeps
    /// the HashMap bounded without penalizing the hot-path `check()`.
    pub fn prune_stale(&self) {
        let mut map = self.timestamps.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, ts| ts.iter().any(|t| now.duration_since(*t) <= self.window));
    }

    /// Check and return appropriate result, logging if rate limited.
    pub fn check_and_log(&self, msg: &InboundMessage) -> bool {
        let allowed = self.check(&msg.sender_id);
        if !allowed {
            warn!(
                sender = %msg.sender_id,
                channel = %msg.channel,
                "Rate limit exceeded"
            );
        }
        allowed
    }
}

// ── SimpleAuthChecker ─────────────────────────────────────

/// Simple auth checker for sender allowlist.
pub struct SimpleAuthChecker {
    allowed_senders: std::collections::HashSet<String>,
}

impl SimpleAuthChecker {
    /// Create an auth checker with an allowlist of sender IDs.
    pub fn new(allowed_senders: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed_senders: allowed_senders.into_iter().collect(),
        }
    }

    /// Check if a sender is allowed.
    /// Returns true if allowed (or if allowlist is empty), false otherwise.
    pub fn is_allowed(&self, sender_id: &str) -> bool {
        if self.allowed_senders.is_empty() {
            return true;
        }
        self.allowed_senders.contains(sender_id)
    }

    /// Check and return appropriate result, logging if rejected.
    pub fn check_and_log(&self, msg: &InboundMessage) -> bool {
        if !self.allowed_senders.is_empty() && !self.allowed_senders.contains(&msg.sender_id) {
            warn!(
                sender = %msg.sender_id,
                channel = %msg.channel,
                "Message rejected: sender not in allowlist"
            );
            false
        } else {
            true
        }
    }
}

// ── Logging helpers ──────────────────────────────────────

/// Log an inbound message at debug level.
pub fn log_inbound(msg: &InboundMessage) {
    debug!(
        channel = %msg.channel,
        sender = %msg.sender_id,
        chat_id = %msg.chat_id,
        content_len = msg.content.len(),
        "Inbound message"
    );
}

/// Log an outbound message at debug level.
pub fn log_outbound(channel: &str, chat_id: &str, content_len: usize) {
    debug!(
        channel = channel,
        chat_id = chat_id,
        content_len = content_len,
        "Outbound message"
    );
}

// ── InboundSender ───────────────────────────────────────

use std::sync::Arc;

/// A wrapper that applies auth and rate-limit checks before forwarding
/// messages to the inbound mpsc channel.
///
/// This ensures that **all** channels — including webhook-driven ones — go
/// through the same middleware pipeline (auth + rate-limit) before reaching
/// the engine's inbound dispatch loop.
#[derive(Clone)]
pub struct InboundSender {
    tx: mpsc::Sender<InboundMessage>,
    rate_limiter: Option<Arc<SimpleRateLimiter>>,
    auth_checker: Option<Arc<SimpleAuthChecker>>,
}

impl InboundSender {
    /// Create a new `InboundSender` backed by an mpsc channel.
    pub fn new(tx: mpsc::Sender<InboundMessage>) -> Self {
        Self {
            tx,
            rate_limiter: None,
            auth_checker: None,
        }
    }

    /// Attach a rate limiter.
    pub fn with_rate_limiter(mut self, rl: Arc<SimpleRateLimiter>) -> Self {
        self.rate_limiter = Some(rl);
        self
    }

    /// Attach an auth checker.
    pub fn with_auth_checker(mut self, ac: Arc<SimpleAuthChecker>) -> Self {
        self.auth_checker = Some(ac);
        self
    }

    /// Send a message through the middleware pipeline.
    ///
    /// Returns `Ok(())` even when the message is silently dropped by auth/rate-limit
    /// checks — the caller should not retry rejected messages.
    pub async fn send(&self, msg: InboundMessage) -> anyhow::Result<()> {
        log_inbound(&msg);

        tracing::info!(
            target: "gasket::middleware",
            sender_id = %msg.sender_id,
            chat_id = %msg.chat_id,
            trace_id = ?msg.trace_id,
            "Middleware forwarding inbound message"
        );

        if let Some(ref auth) = self.auth_checker {
            if !auth.check_and_log(&msg) {
                return Ok(()); // silently drop
            }
        }

        if let Some(ref rl) = self.rate_limiter {
            if !rl.check_and_log(&msg) {
                return Ok(()); // silently drop
            }
        }

        self.tx
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("inbound channel closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ChannelType;
    use chrono::Utc;

    fn make_inbound(sender: &str) -> InboundMessage {
        InboundMessage {
            channel: ChannelType::Cli,
            sender_id: sender.to_string(),
            chat_id: "chat1".to_string(),
            content: "hello".to_string(),
            media: None,
            metadata: None,
            timestamp: Utc::now(),
            trace_id: None,
        }
    }

    #[test]
    fn test_channel_error_retryable() {
        let err = ChannelError::RateLimited {
            channel: "telegram".to_string(),
        };
        assert!(err.is_retryable());
        assert_eq!(err.channel(), Some("telegram"));

        let err = ChannelError::NotConnected {
            channel: "discord".to_string(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_channel_error_display() {
        let err = ChannelError::AuthError {
            channel: "slack".to_string(),
            message: "invalid token".to_string(),
        };
        assert!(err.to_string().contains("slack"));
        assert!(err.to_string().contains("invalid token"));
    }

    #[test]
    fn test_simple_rate_limiter() {
        let rl = SimpleRateLimiter::new(2, std::time::Duration::from_secs(60));

        // First two should pass
        assert!(rl.check("user1"));
        assert!(rl.check("user1"));

        // Third should be rate limited
        assert!(!rl.check("user1"));

        // Different sender should still pass
        assert!(rl.check("user2"));
    }

    #[test]
    fn test_simple_auth_checker() {
        let auth = SimpleAuthChecker::new(vec!["user1".to_string(), "user2".to_string()]);

        assert!(auth.is_allowed("user1"));
        assert!(auth.is_allowed("user2"));
        assert!(!auth.is_allowed("unknown"));
    }

    #[test]
    fn test_simple_auth_checker_empty_allows_all() {
        let auth = SimpleAuthChecker::new(Vec::<String>::new());

        assert!(auth.is_allowed("anyone"));
        assert!(auth.is_allowed("user1"));
    }

    #[test]
    fn test_rate_limiter_cleans_up_stale_senders() {
        let rl = SimpleRateLimiter::new(2, std::time::Duration::from_millis(50));

        // Use up quota for user1
        assert!(rl.check("user1"));
        assert!(rl.check("user1"));

        // Wait for timestamps to expire
        std::thread::sleep(std::time::Duration::from_millis(60));

        // A check for user2 does NOT prune user1 (O(1) hot path)
        assert!(rl.check("user2"));
        {
            let map = rl.timestamps.lock().unwrap();
            assert!(map.contains_key("user1")); // still present
        }

        // Explicit prune removes stale senders
        rl.prune_stale();
        let map = rl.timestamps.lock().unwrap();
        assert!(!map.contains_key("user1"));
        assert!(map.contains_key("user2"));
    }

    #[test]
    fn test_log_inbound() {
        let msg = make_inbound("user1");
        log_inbound(&msg);
        // Should not panic
    }
}
