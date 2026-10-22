//! `CancelSignal` - cooperative cancellation that is cheap to poll AND
//! wakes async waiters the instant it fires.
//!
//! Historically the abort signal was a bare `Arc<AtomicBool>`: cheap sync
//! checks, but async code had to poll it on a timer (the old 50ms
//! `sleep`-and-recheck loops in the SSE download and the approval wait) -
//! burning scheduler wakeups and adding up to 50ms of cancel latency.
//! `CancelSignal` pairs the same atomic flag (so sync checks stay a plain
//! load, and tools keep speaking `Arc<AtomicBool>` via [`flag`](Self::flag))
//! with a `tokio::sync::watch` channel so [`cancelled`](Self::cancelled)
//! resolves with zero polling the moment [`cancel`](Self::cancel) is called.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::watch;

/// Async-aware cooperative cancel token.
///
/// Clone shares the same underlying flag (like `Arc<AtomicBool>` did). All
/// setters go through [`cancel`](Self::cancel) / [`reset`](Self::reset) so
/// every async waiter is woken; the raw-flag escape hatch
/// ([`flag`](Self::flag)) exists only for tool-facing compatibility reads.
#[derive(Clone)]
pub struct CancelSignal {
    inner: Arc<Inner>,
}

struct Inner {
    /// The single source of truth, shared with `ToolCallCtx` consumers.
    flag: Arc<AtomicBool>,
    /// `cancel()`/`reset()` bump this to wake every `cancelled()` waiter.
    tx: watch::Sender<bool>,
}

impl Default for CancelSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CancelSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelSignal")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancelSignal {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                flag: Arc::new(AtomicBool::new(false)),
                tx,
            }),
        }
    }

    /// The raw flag handle for interfaces that still speak
    /// `Arc<AtomicBool>` (`ToolCallCtx.signal`). Reads are cheap; storing
    /// into it bypasses waiter notification - prefer [`cancel`](Self::cancel).
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.inner.flag)
    }

    /// Sync check (plain atomic load).
    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::Relaxed)
    }

    /// Set the flag and wake every async waiter immediately.
    pub fn cancel(&self) {
        self.inner.flag.store(true, Ordering::Relaxed);
        let _ = self.inner.tx.send(true);
    }

    /// Clear the flag (re-arm for the next turn) and wake parked waiters so
    /// stale [`cancelled`](Self::cancelled) futures observe the reset
    /// instead of sleeping until the next cancel.
    pub fn reset(&self) {
        self.inner.flag.store(false, Ordering::Relaxed);
        let _ = self.inner.tx.send(false);
    }

    /// Resolve as soon as the signal is - or becomes - cancelled.
    ///
    /// Race-free by construction: the watch receiver is subscribed BEFORE
    /// the flag is checked, so a `cancel()` that fires between the check and
    /// the `changed().await` still bumps the watch version and wakes us.
    /// A `reset()` wake is re-checked and re-waited, never mistaken for a
    /// cancel.
    pub async fn cancelled(&self) {
        loop {
            let mut rx = self.inner.tx.subscribe();
            if self.is_cancelled() {
                return;
            }
            if rx.changed().await.is_err() {
                // Sender dropped (only possible if every CancelSignal clone
                // is gone, including ours) - nothing left to wait for.
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cancel wakes a pending `cancelled()` within milliseconds - no poll
    /// interval, no timeout escape hatch.
    #[tokio::test]
    async fn cancel_wakes_pending_waiter_immediately() {
        let sig = CancelSignal::new();
        let waiter = sig.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });
        // Give the waiter a chance to park on the watch channel.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let started = std::time::Instant::now();
        sig.cancel();
        task.await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "cancel must wake the waiter immediately, took {:?}",
            started.elapsed()
        );
    }

    /// An already-cancelled signal resolves `cancelled()` without parking.
    #[tokio::test]
    async fn pre_cancelled_resolves_immediately() {
        let sig = CancelSignal::new();
        sig.cancel();
        let started = std::time::Instant::now();
        sig.cancelled().await;
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    /// reset() re-arms the flag and does NOT satisfy cancelled(); a waiter
    /// woken by the reset keeps waiting for a real cancel.
    #[tokio::test]
    async fn reset_rearms_without_cancelling() {
        let sig = CancelSignal::new();
        sig.cancel();
        sig.reset();
        assert!(!sig.is_cancelled());

        let waiter = sig.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(!task.is_finished(), "reset must not count as a cancel");

        sig.cancel();
        task.await.unwrap();
    }

    /// The raw flag handed to tools is the same cell the CancelSignal reads:
    /// a store through `flag()` is visible to `is_cancelled()` (legacy
    /// tool-side aborts keep working for sync checks).
    #[tokio::test]
    async fn raw_flag_shares_state_with_signal() {
        let sig = CancelSignal::new();
        let flag = sig.flag();
        assert!(!sig.is_cancelled());
        flag.store(true, Ordering::Relaxed);
        assert!(sig.is_cancelled());
        sig.reset();
        assert!(!flag.load(Ordering::Relaxed));
    }

    /// Clones share one signal: cancelling through one clone wakes a waiter
    /// holding another.
    #[tokio::test]
    async fn clones_share_the_signal() {
        let a = CancelSignal::new();
        let b = a.clone();
        let waiter = b.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        a.cancel();
        assert!(b.is_cancelled());
        task.await.unwrap();
    }

    /// `Default` hands out the same fresh, not-cancelled signal as `new()`.
    #[test]
    fn default_signal_starts_uncancelled() {
        let sig = CancelSignal::default();
        assert!(!sig.is_cancelled());
        // The shared raw flag starts clear too - tools must not see an abort.
        assert!(!sig.flag().load(Ordering::Relaxed));
    }

    /// A defaulted signal is fully usable: it can be cancelled and reset.
    #[test]
    fn default_signal_supports_cancel_and_reset() {
        let sig = CancelSignal::default();
        sig.cancel();
        assert!(sig.is_cancelled());
        sig.reset();
        assert!(!sig.is_cancelled());
    }

    /// The Debug output names the type and the current cancel state.
    #[test]
    fn debug_fmt_reports_cancel_state() {
        let sig = CancelSignal::new();
        let before = format!("{sig:?}");
        assert!(before.contains("CancelSignal"), "{before}");
        assert!(before.contains("cancelled: false"), "{before}");
        sig.cancel();
        let after = format!("{sig:?}");
        assert!(after.contains("cancelled: true"), "{after}");
    }
}
