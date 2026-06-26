//! Response finalization — decoupled from AgentSession to reduce bloat.
//!
//! `ResponseFinalizer` owns all post-processing logic:
//! - Persisting assistant events
//! - Triggering context compaction
//! - Executing AfterResponse hooks
//! - Calculating and logging token costs

use std::sync::Arc;

use tracing::{info, warn};

use crate::hooks::{HookPoint, HookRegistry, MutableContext, ToolCallInfo};
use crate::kernel::ExecutionResult;
use crate::session::compactor::ContextCompactor;
use crate::session::{AgentResponse, FinalizeContext};
use crate::token_tracker::ModelPricing;
use crate::vault::redact_secrets;
use gasket_storage::EventStore;
use gasket_types::{EventMetadata, EventType, SessionEvent};

/// Post-processor that finalizes an execution result into an `AgentResponse`.
///
/// Held by `AgentSession` so that the heavy finalization logic lives outside
/// the session module proper.
#[derive(Clone)]
pub struct ResponseFinalizer {
    hooks: Arc<HookRegistry>,
    event_store: EventStore,
    compactor: Option<Arc<ContextCompactor>>,
    pricing: Option<ModelPricing>,
    max_tokens: u32,
    after_response_hook_timeout_secs: u64,
}

impl ResponseFinalizer {
    pub fn new(
        hooks: Arc<HookRegistry>,
        event_store: EventStore,
        compactor: Option<Arc<ContextCompactor>>,
        pricing: Option<ModelPricing>,
        max_tokens: u32,
        after_response_hook_timeout_secs: u64,
    ) -> Self {
        Self {
            hooks,
            event_store,
            compactor,
            pricing,
            max_tokens,
            after_response_hook_timeout_secs,
        }
    }

    /// Finalize an `ExecutionResult` into a full `AgentResponse`.
    pub(crate) async fn finalize(
        &self,
        result: ExecutionResult,
        ctx: &FinalizeContext,
        model: &str,
    ) -> AgentResponse {
        let vault_values = &ctx.local_vault_values;

        save_assistant_event(&self.event_store, &result, ctx, vault_values).await;
        trigger_compaction(self.compactor.as_ref(), ctx, vault_values);
        execute_after_response_hooks(
            &self.hooks,
            &result,
            ctx,
            self.after_response_hook_timeout_secs,
        )
        .await;

        let cost = calculate_cost(&result.token_usage, self.pricing.as_ref());
        log_token_stats(&result.token_usage, cost, self.max_tokens);

        AgentResponse {
            content: result.content,
            reasoning_content: result.reasoning_content,
            tools_used: result.tools_used,
            model: Some(model.to_string()),
            token_usage: result.token_usage,
            cost,
        }
    }
}

/// Save the assistant's response as a session event.
async fn save_assistant_event(
    event_store: &EventStore,
    result: &ExecutionResult,
    ctx: &FinalizeContext,
    vault_values: &[String],
) {
    let history_content = redact_secrets(&result.content, vault_values);
    let mut metadata = EventMetadata {
        tools_used: result.tools_used.clone(),
        ..Default::default()
    };
    if let Some(ref reasoning) = result.reasoning_content {
        metadata.extra.insert(
            "reasoning_content".to_string(),
            serde_json::Value::String(reasoning.clone()),
        );
    }
    let assistant_event = SessionEvent {
        id: uuid::Uuid::now_v7(),
        session_key: ctx.session_key_str.to_string(),
        event_type: EventType::AssistantMessage,
        content: history_content,
        metadata,
        created_at: chrono::Utc::now(),
        sequence: 0,
    };
    if let Err(e) = event_store.append_event(&assistant_event).await {
        warn!("Failed to persist assistant event: {}", e);
    }
}

/// Trigger non-blocking context compaction if token budget is exceeded.
fn trigger_compaction(
    compactor: Option<&Arc<ContextCompactor>>,
    ctx: &FinalizeContext,
    vault_values: &[String],
) {
    if ctx.estimated_tokens > 0 {
        if let Some(comp) = compactor {
            comp.try_compact(&ctx.session_key, ctx.estimated_tokens, vault_values);
        }
    }
}

/// Execute AfterResponse hooks with the result context.
async fn execute_after_response_hooks(
    hooks: &HookRegistry,
    result: &ExecutionResult,
    ctx: &FinalizeContext,
    hook_timeout_secs: u64,
) {
    // ExecutionResult.tools_used carries tool names only (no call IDs).
    // Placeholder id — no downstream code reads ToolCallInfo.id in production.
    let tools_used: Vec<ToolCallInfo> = result
        .tools_used
        .iter()
        .map(|name| ToolCallInfo {
            id: String::new(),
            name: name.clone(),
            arguments: None,
        })
        .collect();

    let token_usage_for_hooks =
        result
            .token_usage
            .as_ref()
            .map(|usage| crate::token_tracker::TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                total_tokens: usage.total_tokens,
            });

    let mut hook_ctx = MutableContext {
        session_key: &ctx.session_key_str,
        messages: &mut vec![],
        user_input: Some(&ctx.content),
        response: Some(&result.content),
        tool_calls: Some(&tools_used),
        token_usage: token_usage_for_hooks.as_ref(),
        vault_values: Vec::new(),
    };

    let hook_timeout = std::time::Duration::from_secs(hook_timeout_secs);
    match tokio::time::timeout(
        hook_timeout,
        hooks.execute(HookPoint::AfterResponse, &mut hook_ctx),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            warn!("AfterResponse hook failed (ignored): {}", e);
        }
        Err(_) => {
            warn!(
                "AfterResponse hook timed out after {}s — skipping",
                hook_timeout_secs
            );
        }
    }
}

/// Calculate the cost of the response based on token usage.
pub(crate) fn calculate_cost(
    token_usage: &Option<gasket_types::TokenUsage>,
    pricing: Option<&ModelPricing>,
) -> f64 {
    match (token_usage, pricing) {
        (Some(usage), Some(p)) => p.calculate_cost(usage.input_tokens, usage.output_tokens),
        _ => 0.0,
    }
}

/// Log token usage statistics.
pub(crate) fn log_token_stats(
    usage: &Option<gasket_types::TokenUsage>,
    cost: f64,
    max_tokens: u32,
) {
    if let Some(u) = usage {
        let remaining = max_tokens.saturating_sub(u.total_tokens as u32);
        info!(
            "[Token] Used: {} / Max: {} | Remaining: {} | Input: {} | Output: {} | Cost: ${:.4}",
            u.total_tokens, max_tokens, remaining, u.input_tokens, u.output_tokens, cost
        );
    }
}
