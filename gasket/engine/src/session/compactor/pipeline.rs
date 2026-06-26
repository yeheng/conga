//! Compaction pipeline stages — module-level functions for testability.

use std::sync::Arc;

use anyhow::{bail, Result};
use tracing::{debug, info, warn};

use gasket_providers::{ChatMessage, ChatRequest, LlmProvider};
use gasket_storage::{count_tokens, EventStoreTrait, SessionStoreTrait};
use gasket_types::{SessionEvent, SessionKey};

use crate::vault::redact_secrets;

use super::CompactionListener;

/// Execute the full compaction pipeline: load → build context → summarize → persist.
#[allow(clippy::too_many_arguments)]
pub async fn run_compaction(
    event_store: &dyn EventStoreTrait,
    session_store: &dyn SessionStoreTrait,
    provider: &dyn LlmProvider,
    model: &str,
    summarization_prompt: &str,
    session_key: &SessionKey,
    vault_values: &[String],
    listeners: &[Arc<dyn CompactionListener>],
) -> Result<()> {
    // 1. Load target sequence
    let target_sequence = event_store
        .get_max_sequence(session_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get max sequence for {}: {}", session_key, e))?;

    // 2. Load existing summary
    let existing_summary = match session_store.load_summary(session_key).await {
        Ok(Some((content, _watermark))) => Some(content),
        Ok(None) => None,
        Err(e) => bail!("Failed to load summary for {}: {}", session_key, e),
    };

    // 3. Load events to compact
    let events = event_store
        .get_events_up_to_sequence(session_key, target_sequence)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load events for {}: {}", session_key, e))?;

    if events.is_empty() {
        debug!("No events to compact for {}", session_key);
        return Ok(());
    }

    // 4. Build context → summarize → persist
    let context_text = build_context_text(existing_summary.as_deref(), &events);
    debug!(
        "Compaction context for {}: {} tokens, {} events (up to seq {})",
        session_key,
        count_tokens(&context_text),
        events.len(),
        target_sequence
    );

    let summary_text = summarize_with_llm(provider, model, summarization_prompt, &context_text)
        .await?
        .trim()
        .to_string();

    if summary_text.is_empty() {
        bail!("LLM returned empty summary for {}", session_key);
    }

    persist_and_gc(
        session_store,
        event_store,
        session_key,
        &summary_text,
        vault_values,
        target_sequence,
        listeners,
    )
    .await?;

    Ok(())
}

/// Build the text context sent to the LLM for summarization.
///
/// Prepends the existing summary (if any) before the event list.
pub fn build_context_text(existing_summary: Option<&str>, events: &[SessionEvent]) -> String {
    let mut parts = Vec::with_capacity(events.len() + 1);

    if let Some(summary) = existing_summary {
        if !summary.is_empty() {
            parts.push(format!("Previous summary:\n{}", summary));
        }
    }

    for event in events {
        parts.push(format!("{}: {}", event.event_type, event.content));
    }

    parts.join("\n")
}

/// Call the LLM to generate a summary from the context text.
pub async fn summarize_with_llm(
    provider: &dyn LlmProvider,
    model: &str,
    summarization_prompt: &str,
    context_text: &str,
) -> Result<String> {
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage::system(summarization_prompt),
            ChatMessage::user(context_text.to_string()),
        ],
        tools: None,
        temperature: Some(0.3),
        max_tokens: Some(1024),
        thinking: None,
    };

    let response = provider
        .chat(request)
        .await
        .map_err(|e| anyhow::anyhow!("LLM summarization call failed: {}", e))?;

    Ok(response.content.unwrap_or_default())
}

/// Redact secrets, persist the summary, and garbage-collect old events.
#[allow(clippy::too_many_arguments)]
pub async fn persist_and_gc(
    session_store: &dyn SessionStoreTrait,
    event_store: &dyn EventStoreTrait,
    session_key: &SessionKey,
    summary_text: &str,
    vault_values: &[String],
    target_sequence: i64,
    listeners: &[Arc<dyn CompactionListener>],
) -> Result<()> {
    // Redact secrets
    let summary_to_persist = if vault_values.is_empty() {
        summary_text.to_string()
    } else {
        redact_secrets(summary_text, vault_values)
    };

    // Persist summary with new watermark
    session_store
        .save_summary(session_key, &summary_to_persist, target_sequence)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to save summary for {}: {}", session_key, e))?;

    // Fetch event IDs before deletion so listeners know what was removed.
    let deleted_ids = event_store
        .get_event_ids_up_to(session_key, target_sequence)
        .await
        .unwrap_or_default();

    // Garbage-collect old events (non-fatal on failure)
    match event_store
        .delete_events_upto(session_key, target_sequence)
        .await
    {
        Ok(deleted) => {
            // Notify listeners after successful deletion.
            if !deleted_ids.is_empty() {
                for listener in listeners {
                    listener.on_events_deleted(&deleted_ids);
                }
            }
            info!(
                "Compaction complete for {}: {} tokens summary, {} events GC'd (watermark={})",
                session_key,
                count_tokens(summary_text),
                deleted,
                target_sequence
            );
        }
        Err(e) => {
            warn!(
                "Compaction: summary saved but GC failed for {}: {}",
                session_key, e
            );
        }
    }

    Ok(())
}
