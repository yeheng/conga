//! `SteerQueue` — mid-turn user input, injected at the next safe point.
//!
//! The loop's only steering primitive used to be the cancel signal: a user
//! watching the agent go wrong could kill the turn but not redirect it. A
//! `SteerQueue` is the other half: transports push user text onto it while a
//! turn runs; the loop drains it at the top of each turn iteration (before
//! the next LLM call) and appends each item as a real `User` message —
//! persisted through the same `persist` callback as everything else, so a
//! steered conversation survives restarts via `derive_messages`.

use std::collections::VecDeque;
use std::sync::Arc;

/// Shared, cloneable queue of pending user messages.
///
/// Clone hands out another handle to the SAME queue (Arc inside), which is
/// how `AgentLoopConfig` (Clone) can carry it.
#[derive(Clone, Default)]
pub struct SteerQueue(Arc<std::sync::Mutex<VecDeque<String>>>);

impl SteerQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one user message for injection at the next safe point.
    pub fn push(&self, msg: impl Into<String>) {
        self.0.lock().unwrap().push_back(msg.into());
    }

    /// Take everything queued (oldest first). Called by the loop at each
    /// turn boundary.
    pub fn drain(&self) -> Vec<String> {
        let mut q = self.0.lock().unwrap();
        q.drain(..).collect()
    }
}

impl std::fmt::Debug for SteerQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.0.lock().unwrap().len();
        f.debug_struct("SteerQueue").field("pending", &len).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_drain_fifo() {
        let q = SteerQueue::new();
        q.push("first");
        q.push("second");
        assert_eq!(q.drain(), vec!["first", "second"]);
        assert!(q.drain().is_empty());
    }

    #[test]
    fn clone_shares_the_queue() {
        let q = SteerQueue::new();
        let handle = q.clone();
        handle.push("via clone");
        assert_eq!(q.drain(), vec!["via clone"]);
    }

    /// The Debug output reports how many messages are still pending.
    #[test]
    fn debug_fmt_reports_pending_count() {
        let q = SteerQueue::new();
        let empty = format!("{q:?}");
        assert!(empty.contains("SteerQueue"), "{empty}");
        assert!(empty.contains("pending: 0"), "{empty}");

        q.push("first");
        q.push("second");
        let pending = format!("{q:?}");
        assert!(pending.contains("pending: 2"), "{pending}");

        q.drain();
        assert!(format!("{q:?}").contains("pending: 0"));
    }
}
