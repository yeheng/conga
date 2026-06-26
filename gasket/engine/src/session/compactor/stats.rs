//! Context usage statistics and watermark inspection.

use anyhow::Result;
use gasket_storage::{count_tokens, EventStoreTrait, SessionStoreTrait};
use gasket_types::SessionKey;

/// Context usage statistics for a session.
#[derive(Debug, Clone)]
pub struct UsageStats {
    /// Configured token budget for the context window.
    pub token_budget: usize,
    /// Compaction threshold multiplier (e.g. 1.2 = 120%).
    pub compaction_threshold: f32,
    /// Token count at which auto-compaction triggers.
    pub threshold_tokens: usize,
    /// Estimated total tokens (summary + uncompacted events).
    pub current_tokens: usize,
    /// Current usage as a percentage of token_budget.
    pub usage_percent: f64,
    /// Tokens consumed by the existing summary.
    pub summary_tokens: usize,
    /// Number of events not yet covered by a summary.
    pub uncompacted_events: usize,
    /// Token count of uncompacted events.
    pub event_tokens: usize,
    /// Whether a compaction task is currently running.
    pub is_compressing: bool,
}

/// Watermark and sequence information for a session.
#[derive(Debug, Clone)]
pub struct WatermarkInfo {
    /// The covered_upto_sequence from the latest summary.
    pub watermark: i64,
    /// Maximum sequence number in the event store.
    pub max_sequence: i64,
    /// Number of events after the watermark.
    pub uncompacted_count: usize,
    /// Percentage of history that has been compacted.
    pub compacted_percent: f64,
}

/// Load the current summary and its watermark for a session.
///
/// Returns `(summary_text, covered_upto_sequence)`.
/// If no summary exists, returns `("", 0)`.
pub async fn load_summary_with_watermark(
    session_store: &dyn SessionStoreTrait,
    session_key: &SessionKey,
) -> (String, i64) {
    match session_store.load_summary(session_key).await {
        Ok(Some((content, watermark))) => (content, watermark),
        Ok(None) => (String::new(), 0),
        Err(e) => {
            tracing::debug!("Failed to load summary for {}: {}", session_key, e);
            (String::new(), 0)
        }
    }
}

/// Get context usage statistics for a session.
pub async fn get_usage_stats(
    event_store: &dyn EventStoreTrait,
    session_store: &dyn SessionStoreTrait,
    session_key: &SessionKey,
    token_budget: usize,
    compaction_threshold: f32,
    is_compressing: bool,
) -> Result<UsageStats> {
    let (summary_text, watermark) = load_summary_with_watermark(session_store, session_key).await;
    let summary_tokens = if summary_text.is_empty() {
        0
    } else {
        count_tokens(&summary_text)
    };

    let events = event_store
        .get_events_after_sequence(session_key, watermark)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load events: {}", e))?;

    let event_tokens: usize = events.iter().map(|e| count_tokens(&e.content)).sum();
    let total_tokens = summary_tokens + event_tokens;
    let threshold_tokens = (token_budget as f32 * compaction_threshold) as usize;
    let usage_percent = if token_budget > 0 {
        (total_tokens as f64 / token_budget as f64) * 100.0
    } else {
        0.0
    };

    Ok(UsageStats {
        token_budget,
        compaction_threshold,
        threshold_tokens,
        current_tokens: total_tokens,
        usage_percent,
        summary_tokens,
        uncompacted_events: events.len(),
        event_tokens,
        is_compressing,
    })
}

/// Get watermark and sequence information for a session.
pub async fn get_watermark_info(
    event_store: &dyn EventStoreTrait,
    session_store: &dyn SessionStoreTrait,
    session_key: &SessionKey,
) -> Result<WatermarkInfo> {
    let (_, watermark) = load_summary_with_watermark(session_store, session_key).await;

    let max_sequence = event_store
        .get_max_sequence(session_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get max sequence: {}", e))?;

    let uncompacted = event_store
        .get_events_after_sequence(session_key, watermark)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to count uncompacted events: {}", e))?
        .len();

    let compacted_percent = if max_sequence > 0 {
        (watermark as f64 / max_sequence as f64) * 100.0
    } else {
        0.0
    };

    Ok(WatermarkInfo {
        watermark,
        max_sequence,
        uncompacted_count: uncompacted,
        compacted_percent,
    })
}
