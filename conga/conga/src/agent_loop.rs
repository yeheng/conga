//! The agent loop — single outer loop: LLM call → tool calls → repeat.

use std::future::Future;
use std::pin::Pin;

use futures_util::StreamExt;

use crate::error::{AgentError, ToolError};
use crate::types::context::{AgentContext, AgentLoopConfig, RetryPolicy, StreamChunk};
use crate::types::event::{AgentEvent, ContentDelta};
use crate::types::message::{
    AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall, ToolResultMessage,
    UserMessage,
};
use crate::types::session_event::SessionEvent;
use crate::types::tool::{RiskLevel, ToolCallCtx, ToolCallVerdict, ToolContext, ToolResult};

/// Run the agent loop to completion.
///
/// `emit` is called for every [`AgentEvent`] as it happens. Returns the full
/// list of messages produced (assistant turns + tool results).
///
/// Host usage:
/// ```ignore
/// run_agent_loop(prompts, context, config, |ev| match ev { ... }).await?;
/// ```
pub async fn run_agent_loop<E>(
    initial_prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    config: AgentLoopConfig,
    mut emit: E,
) -> Result<Vec<AgentMessage>, AgentError>
where
    E: FnMut(AgentEvent),
{
    let mut new_messages: Vec<AgentMessage> = Vec::new();

    // Seed context with the initial prompts. These are inputs, not assistant
    // output: no MessageStart/MessageEnd is emitted for them (an empty
    // AssistantMessage per user prompt would be a lie both frontends would
    // have to filter out).
    for msg in &initial_prompts {
        context.messages.push(msg.clone());
        new_messages.push(msg.clone());
    }
    emit(AgentEvent::AgentStart);
    tracing::info!(model = %config.model.id, session = %context.session_id, "agent loop start");

    let mut guard = crate::guard::RepeatGuard::new();

    for turn in 0..config.max_turns {
        emit(AgentEvent::TurnStart);
        tracing::info!("agent turn {} start", turn);

        // Mid-turn steering: user text queued while the previous turn was
        // running enters here, as a real User message (persisted like the
        // initial prompt) — the agent sees it before its next decision.
        if let Some(steer) = &config.steer {
            for text in steer.drain() {
                let user = AgentMessage::User(UserMessage {
                    content: vec![ContentBlock::text(text)],
                    timestamp: crate::now(),
                });
                persist_event(&config, &SessionEvent::User(user.clone()))?;
                context.messages.push(user.clone());
                new_messages.push(user);
            }
        }

        // 1. Call the LLM. (An abort signal is handled inside
        //    `stream_assistant_response`, before any provider request is made.)
        let assistant = stream_assistant_response(&context, &config, &mut emit).await?;
        let stop_reason = assistant.stop_reason.clone();
        // Crash-safety invariant: the assembled assistant message (with this
        // step's usage) hits the log BEFORE any tool in it executes, so a
        // crash mid-tool leaves an honest "assistant asked, tool never
        // answered" tail instead of a phantom.
        persist_event(
            &config,
            &SessionEvent::Assistant {
                message: AgentMessage::Assistant(assistant.clone()),
                usage: assistant.usage,
            },
        )?;
        context
            .messages
            .push(AgentMessage::Assistant(assistant.clone()));
        new_messages.push(AgentMessage::Assistant(assistant.clone()));

        // 2. Check termination.
        match stop_reason {
            StopReason::EndTurn | StopReason::Error(_) | StopReason::Aborted => {
                emit(AgentEvent::TurnEnd {
                    message: assistant,
                    tool_results: vec![],
                });
                break;
            }
            StopReason::MaxTokens => {
                // Output was truncated: fail every tool call in this turn.
                tracing::warn!("assistant output truncated (max_tokens); discarding tool calls");
                let error_results = fail_all_tool_calls(&assistant);
                for r in &error_results {
                    persist_event(
                        &config,
                        &SessionEvent::ToolResult(AgentMessage::ToolResult(r.clone())),
                    )?;
                    context.messages.push(AgentMessage::ToolResult(r.clone()));
                    new_messages.push(AgentMessage::ToolResult(r.clone()));
                }
                emit(AgentEvent::TurnEnd {
                    message: assistant,
                    tool_results: error_results,
                });
                continue;
            }
            StopReason::ToolUse => {} // fall through to execution
        }

        // 3. Execute tool calls (concurrently within the batch; results are
        //    recorded in declaration order).
        let tool_results =
            execute_tool_calls(&context, &assistant, &config, &mut guard, &mut emit).await?;
        for r in &tool_results {
            context.messages.push(AgentMessage::ToolResult(r.clone()));
            new_messages.push(AgentMessage::ToolResult(r.clone()));
        }

        emit(AgentEvent::TurnEnd {
            message: assistant,
            tool_results,
        });
    }

    tracing::info!("agent loop end");
    emit(AgentEvent::AgentEnd);
    Ok(new_messages)
}

/// Convenience: run the loop with a no-op emitter (for hosts that only want
/// the final message list).
pub async fn agent_loop(
    initial_prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
) -> Result<Vec<AgentMessage>, AgentError> {
    run_agent_loop(initial_prompts, context, config, |_| {}).await
}

fn is_aborted(config: &AgentLoopConfig) -> bool {
    config.signal.as_ref().is_some_and(|s| s.is_cancelled())
}

/// Hand one [`SessionEvent`] to the loop's `persist` callback (if installed).
/// A persist `Err` propagates: storage failures abort the run (fail loud),
/// never silently swallowed. `persist: None` is a no-op.
fn persist_event(config: &AgentLoopConfig, event: &SessionEvent) -> Result<(), AgentError> {
    match &config.persist {
        Some(persist) => persist(event),
        None => Ok(()),
    }
}

/// Emit `ToolExecutionEnd`, persist the completed tool result, and record it.
/// Every finished result flows through here — successes, tool errors, hook
/// blocks (a refusal is a fact), malformed args, unknown tools, over-limit
/// drops — so the on-disk order always matches execution order. Called only
/// after any `after_tool_call` rewriting, never before.
fn record_tool_result<E>(
    config: &AgentLoopConfig,
    emit: &mut E,
    results: &mut Vec<ToolResultMessage>,
    result: ToolResultMessage,
    is_error: bool,
) -> Result<(), AgentError>
where
    E: FnMut(AgentEvent),
{
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: result.tool_call_id.clone(),
        result: result.clone(),
        is_error,
    });
    persist_event(
        config,
        &SessionEvent::ToolResult(AgentMessage::ToolResult(result.clone())),
    )?;
    results.push(result);
    Ok(())
}

/// Build an error ToolResult for every tool call in `assistant` (used on
/// MaxTokens truncation, where calls may be partial).
fn fail_all_tool_calls(assistant: &AssistantMessage) -> Vec<ToolResultMessage> {
    assistant
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall { tool_call: tc } => Some(ToolResultMessage {
                tool_call_id: tc.id.clone(),
                tool_name: tc.function.name.clone(),
                content: vec![ContentBlock::Text {
                    text:
                        "Error: assistant output was truncated (max_tokens); tool call discarded."
                            .into(),
                }],
                is_error: true,
                timestamp: crate::now(),
            }),
            _ => None,
        })
        .collect()
}

/// Build an error [`ToolResultMessage`] for a tool call that failed before or
/// during execution (missing/malformed args, unknown tool, tool-internal
/// error). The agent loop feeds this back to the LLM so the model can retry,
/// rather than aborting the whole run.
fn error_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    message: impl Into<String>,
) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: tool_call_id.into(),
        tool_name: tool_name.into(),
        content: vec![ContentBlock::text(message)],
        is_error: true,
        timestamp: crate::now(),
    }
}

/// Execute every tool call in `assistant`, running before/after hooks (V0.1:
/// hooks are no-ops; wired in stage 3g).
/// One tool call of a batch, prepared by the sequential pre-pass.
///
/// The pre-pass resolves limits, arg parsing, hooks (approvers must be
/// asked in declaration order), and tool lookup; only calls that clear all
/// of that become [`Slot::Ready`] and fan out concurrently.
enum Slot {
    Ready {
        tc: ToolCall,
        execute: crate::types::tool::ToolFn,
        args: serde_json::Value,
        args_key: String,
    },
    /// Pre-execution rejection (limit / parse error / hook block / unknown
    /// tool) with the error tool_result to record for the model.
    Failed(ToolResultMessage),
}

/// Executes one batch of tool calls in three phases:
///
/// 1. **Sequential pre-pass** - per-call limit check, cooperative abort
///    check, arg parsing, `before_tool_call` hooks, and tool lookup. Hooks
///    run in declaration order so a human approver is asked about call #1
///    before call #2 exists as an approved fact.
/// 2. **Concurrent execution** - approved calls dispatch together via
///    `join_all` (I/O-bound tools overlap; `Start` events still emit in
///    declaration order). A mid-batch abort no longer skips already-
///    dispatched calls: they run to completion so every call gets a result.
/// 3. **Sequential post-pass** - `after_tool_call` hooks, repeat-guard
///    advisories, persistence, and `End` events, all in declaration order,
///    so the session log replays exactly as declared regardless of which
///    tool finished first.
async fn execute_tool_calls<E>(
    context: &AgentContext,
    assistant: &AssistantMessage,
    config: &AgentLoopConfig,
    guard: &mut crate::guard::RepeatGuard,
    emit: &mut E,
) -> Result<Vec<ToolResultMessage>, AgentError>
where
    E: FnMut(AgentEvent),
{
    let tool_calls: Vec<_> = assistant
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall { tool_call: tc } => Some(tc.clone()),
            _ => None,
        })
        .collect();

    // ---- Phase 1: sequential pre-pass (hooks must fire in order) ----
    let mut slots: Vec<Slot> = Vec::with_capacity(tool_calls.len());
    for (i, tc) in tool_calls.into_iter().enumerate() {
        if i >= config.max_tool_calls_per_turn {
            // Limit reached: report the dropped call as an error tool_result so
            // the model sees one result per call instead of a silent gap.
            let limit = config.max_tool_calls_per_turn;
            slots.push(Slot::Failed(error_tool_result(
                &tc.id,
                &tc.function.name,
                format!("tool call limit reached ({limit} per turn); call dropped"),
            )));
            continue;
        }
        // Cooperative abort before dispatching more calls in this batch.
        // Calls already dispatched still complete (Phase 2 runs them out).
        if is_aborted(config) {
            break;
        }

        // Parse the accumulated tool-call arguments. Empty -> `{}` (a tool may
        // take no params); malformed JSON -> feed the parse error back to the
        // LLM as a tool_result so it can retry, instead of silently degrading
        // to `Null` and crashing inside the tool.
        let mut args: serde_json::Value = match serde_json::from_str(&tc.function.arguments) {
            Ok(v) => v,
            Err(_) if tc.function.arguments.trim().is_empty() => {
                serde_json::Value::Object(Default::default())
            }
            Err(e) => {
                tracing::warn!(tool = %tc.function.name, error = %e, "malformed tool arguments");
                slots.push(Slot::Failed(error_tool_result(
                    &tc.id,
                    &tc.function.name,
                    format!(
                        "failed to parse tool arguments as JSON: {e}\nraw arguments: {:?}",
                        tc.function.arguments
                    ),
                )));
                continue;
            }
        };

        // before_tool_call hook: consult the hook chain if installed.
        // Risk is looked up from the tool definition (unknown tools default
        // to High - the safe default, matching the old host-side table).
        let risk = context
            .tools
            .iter()
            .find(|t| t.name == tc.function.name)
            .map(|t| t.risk)
            .unwrap_or(RiskLevel::High);
        let verdict = match &config.hooks {
            Some(h) => {
                h.before_tool_call(&tc.id, &tc.function.name, &args, risk)
                    .await
            }
            None => ToolCallVerdict::Allow,
        };
        match verdict {
            ToolCallVerdict::Block(reason) => {
                tracing::warn!(tool = %tc.function.name, "tool blocked by before_tool_call hook");
                slots.push(Slot::Failed(error_tool_result(
                    &tc.id,
                    &tc.function.name,
                    reason,
                )));
                continue;
            }
            ToolCallVerdict::Modify(new_args) => args = new_args,
            ToolCallVerdict::Allow => {}
        }

        // Locate the tool. Unknown tool -> error tool_result (the model may
        // have hallucinated a name); continue the run.
        let tool = match context.tools.iter().find(|t| t.name == tc.function.name) {
            Some(t) => t,
            None => {
                tracing::warn!(tool = %tc.function.name, "tool not found");
                slots.push(Slot::Failed(error_tool_result(
                    &tc.id,
                    &tc.function.name,
                    format!("tool not found: {}", tc.function.name),
                )));
                continue;
            }
        };

        let args_key = args.to_string();
        slots.push(Slot::Ready {
            tc,
            execute: tool.execute.clone(),
            args,
            args_key,
        });
    }

    // ---- Phase 2: concurrent dispatch (Start events stay in order) ----
    // `tool_timeout` is the engine-level safety net: a tool whose Future
    // never resolves (wedged MCP server, deadlocked plugin) is cut off and
    // reported as an error result instead of hanging the whole loop. Tools
    type ToolFuture = Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send>>;
    let mut futures: Vec<ToolFuture> = Vec::new();
    for slot in &slots {
        if let Slot::Ready {
            tc, execute, args, ..
        } = slot
        {
            emit(AgentEvent::ToolExecutionStart {
                tool_call_id: tc.id.clone(),
                tool_name: tc.function.name.clone(),
                args: args.clone(),
            });
            tracing::info!(tool = %tc.function.name, "tool execute");
            let fut = (execute.clone())(ToolCallCtx {
                tool_call_id: tc.id.clone(),
                args: args.clone(),
                signal: config.signal.as_ref().map(|s| s.flag()).unwrap_or_default(),
                ctx: ToolContext {
                    cwd: context.cwd.clone(),
                    env: context.env.clone(),
                    session_id: context.session_id.clone(),
                    // Invalid session id (would escape the state root): fall
                    // back to a scratch temp dir, never a path outside it.
                    state_dir: tool_state_dir(context, &tc.function.name)
                        .unwrap_or_else(std::env::temp_dir),
                },
            });
            futures.push(match config.tool_timeout {
                Some(limit) => Box::pin(async move {
                    match tokio::time::timeout(limit, fut).await {
                        Ok(r) => r,
                        Err(_) => Err(ToolError::Message(format!(
                            "tool execution timed out after {}s",
                            limit.as_secs()
                        ))),
                    }
                }),
                None => fut,
            });
        }
    }
    let raws = futures_util::future::join_all(futures).await;

    // ---- Phase 3: sequential post-pass (record in declaration order) ----
    let mut results = Vec::with_capacity(slots.len());
    let mut cursor = raws.into_iter();
    for slot in slots {
        match slot {
            Slot::Failed(result) => {
                record_tool_result(config, emit, &mut results, result, true)?;
            }
            Slot::Ready { tc, args_key, .. } => {
                // A tool-internal error becomes an error tool_result fed
                // back to the LLM; the run continues instead of aborting.
                let raw = match cursor.next().expect("join_all result per Ready slot") {
                    Ok(r) => r,
                    Err(e) => {
                        let msg = e.to_string();
                        tracing::warn!(tool = %tc.function.name, error = %msg, "tool execute error");
                        crate::types::tool::ToolResult::error(msg)
                    }
                };

                let mut result = ToolResultMessage {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.function.name.clone(),
                    content: raw.content,
                    is_error: raw.is_error,
                    timestamp: crate::now(),
                };

                // after_tool_call hook: chain may replace the result (redact, etc.).
                if let Some(h) = &config.hooks {
                    result = h.after_tool_call(&tc.id, &result);
                }

                if let Some(note) =
                    crate::guard::repeat_advisory(guard.observe(&tc.function.name, &args_key))
                {
                    match result.content.iter_mut().find_map(|b| match b {
                        crate::ContentBlock::Text { text } => Some(text),
                        _ => None,
                    }) {
                        Some(text) => {
                            text.push_str("\n\n[");
                            text.push_str(&note);
                            text.push(']');
                        }
                        None => result
                            .content
                            .push(crate::ContentBlock::text(format!("[{note}]"))),
                    }
                }

                let is_error = result.is_error;
                record_tool_result(config, emit, &mut results, result, is_error)?;

                tracing::info!(tool = %tc.function.name, is_error, "tool done");
            }
        }
    }

    Ok(results)
}

/// Default per-tool state directory:
/// `<config_dir>/sessions/<session_id>/tool_state/<tool_name>/`.
///
/// The session id is validated before interpolation: an id that could
/// escape the root (`../`, separators, empty) gets no state dir at all
/// rather than a directory outside it. Note this uses the global
/// [`crate::storage::config_dir`] — a host-injected custom storage root is
/// not threaded into `AgentContext`, so state stays under the default root.
fn tool_state_dir(context: &AgentContext, tool_name: &str) -> Option<std::path::PathBuf> {
    if !crate::storage::is_valid_session_id(&context.session_id) {
        tracing::warn!(session = %context.session_id, "invalid session id; tool state dir skipped");
        return None;
    }
    Some(
        crate::storage::config_dir()
            .join("sessions")
            .join(&context.session_id)
            .join("tool_state")
            .join(tool_name),
    )
}

/// Stream one assistant response from the LLM, accumulating into an
/// [`AssistantMessage`] and emitting `MessageUpdate` for each delta.
async fn stream_assistant_response<E>(
    context: &AgentContext,
    config: &AgentLoopConfig,
    emit: &mut E,
) -> Result<AssistantMessage, AgentError>
where
    E: FnMut(AgentEvent),
{
    let max_retries = config.retry.max_retries;
    let mut attempt: usize = 0;
    // Compute the wire view ONCE per logical LLM call, before any provider
    // request: retries must see the identical view, so the transform runs
    // outside the retry loop. `Err` fails the run loud. `None` = the
    // accumulator itself is the view (zero-change default).
    let transformed: Option<Vec<AgentMessage>> = match &config.transform_context {
        Some(t) => Some(t(&context.messages)?),
        None => None,
    };
    let wire: &[AgentMessage] = transformed.as_deref().unwrap_or(&context.messages);
    loop {
        if is_aborted(config) {
            // A cancel arrived while the host was waiting (e.g. an approval
            // prompt): exit before burning a provider request. Mirrors the
            // in-stream abort path's event shape so hosts see the Aborted
            // message the same way.
            tracing::info!("provider request skipped: aborted");
            let mut msg = AssistantMessage::new(&config.model.id);
            msg.stop_reason = StopReason::Aborted;
            emit(AgentEvent::MessageEnd {
                message: msg.clone(),
            });
            emit(AgentEvent::AfterProviderResponse {
                model: config.model.id.clone(),
                response: msg.clone(),
            });
            return Ok(msg);
        }
        attempt += 1;
        emit(AgentEvent::BeforeProviderRequest {
            model: config.model.id.clone(),
        });
        tracing::debug!(model = %config.model.id, attempt, "provider request");

        match attempt_stream_once(wire, context, config, &mut *emit).await {
            StreamAttempt::Done(accumulated) => {
                tracing::debug!(stop_reason = ?accumulated.stop_reason, "provider response");
                emit(AgentEvent::MessageEnd {
                    message: accumulated.clone(),
                });
                emit(AgentEvent::AfterProviderResponse {
                    model: config.model.id.clone(),
                    response: accumulated.clone(),
                });
                return Ok(accumulated);
            }
            StreamAttempt::Errored {
                error,
                emitted_content,
            } => {
                // Only retry when nothing was emitted to the host yet (so the
                // retry is invisible) and the signal isn't already aborting.
                let can_retry = !emitted_content && attempt <= max_retries && !is_aborted(config);
                if can_retry {
                    // Rate-limit errors back off longer: hammering a 429
                    // provider on the fast schedule just deepens the limit.
                    // The status rides the provider's own error string
                    // ("HTTP 429 ...") - bounded to self-produced text.
                    let rate_limited = error.contains("429");
                    let delay = backoff_ms(attempt, &config.retry, rate_limited);
                    tracing::warn!(
                        attempt,
                        max_retries,
                        rate_limited,
                        delay_ms = delay,
                        error = %error,
                        "provider stream error, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    continue;
                }
                let mut msg = AssistantMessage::new(&config.model.id);
                msg.stop_reason = StopReason::Error(error);
                tracing::debug!(stop_reason = ?msg.stop_reason, "provider response (errored)");
                emit(AgentEvent::MessageEnd {
                    message: msg.clone(),
                });
                emit(AgentEvent::AfterProviderResponse {
                    model: config.model.id.clone(),
                    response: msg.clone(),
                });
                return Ok(msg);
            }
        }
    }
}

/// Outcome of one streaming attempt.
enum StreamAttempt {
    /// Stream completed (normally or via abort). Carries the accumulated message.
    Done(AssistantMessage),
    /// Stream errored. `emitted_content` tells the caller whether any content
    /// delta was already sent to the host - if so, retrying would duplicate it.
    Errored {
        error: String,
        emitted_content: bool,
    },
}

async fn attempt_stream_once<E>(
    wire: &[AgentMessage],
    context: &AgentContext,
    config: &AgentLoopConfig,
    emit: &mut E,
) -> StreamAttempt
where
    E: FnMut(AgentEvent),
{
    let mut stream: Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> =
        (config.stream_fn).stream(
            &config.model,
            wire,
            &context.system_prompt,
            &context.tools,
            config.signal.clone(),
        );

    let mut accumulated = AssistantMessage::new(&config.model.id);
    let mut usage = None;
    // Raw cache-token totals for this step. Kept separate from `usage`
    // (whose cache fields are Option) and normalized below: 0 total means
    // the provider never reported cache tokens → None ("absent = unknown",
    // matching old logs that predate these fields).
    let mut cache_read_total: u64 = 0;
    let mut cache_write_total: u64 = 0;
    let mut emitted_content = false;
    // Provider-reported stop reason, when the stream carried one
    // (StreamChunk::Stop). Overrides the content-based guess below.
    let mut provider_stop: Option<StopReason> = None;

    while let Some(chunk) = stream.next().await {
        // Cooperative abort: stop accumulating as soon as the signal is set.
        if is_aborted(config) {
            tracing::info!("provider stream aborted");
            accumulated.stop_reason = StopReason::Aborted;
        }
        match chunk {
            StreamChunk::TextDelta(t) => {
                emitted_content = true;
                accumulated.append_text(&t);
                emit(AgentEvent::MessageUpdate {
                    delta: ContentDelta::TextDelta(t),
                });
            }
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                args_delta,
            } => {
                emitted_content = true;
                accumulated.append_tool_call(index, id.clone(), name.clone(), args_delta.clone());
                emit(AgentEvent::MessageUpdate {
                    delta: ContentDelta::ToolCallDelta {
                        id,
                        name,
                        args_delta,
                    },
                });
            }
            StreamChunk::ThinkingDelta(t) => {
                emitted_content = true;
                accumulated.append_thinking(&t);
                emit(AgentEvent::MessageUpdate {
                    delta: ContentDelta::ThinkingDelta(t),
                });
            }
            StreamChunk::Usage {
                input,
                output,
                cache_read,
                cache_write,
            } => {
                // Merge, don't overwrite: Anthropic sends input tokens in
                // `message_start` and output tokens in `message_delta` as two
                // separate Usage chunks. Overwriting would zero input on the
                // second. Both OpenAI (one combined chunk) and Anthropic
                // (complementary partials) sum correctly. Cache counters sum
                // the same way.
                let u = usage.get_or_insert(crate::types::message::Usage::default());
                u.input_tokens += input;
                u.output_tokens += output;
                cache_read_total += cache_read;
                cache_write_total += cache_write;
            }
            StreamChunk::Stop(reason) => {
                // Provider-reported stop signal (OpenAI `finish_reason`,
                // Anthropic `message_delta.stop_reason`). Remembered and
                // applied after the loop: it wins over the content-based
                // guess below, so a length-truncated response that died
                // mid-tool-call classifies as MaxTokens instead of ToolUse
                // with malformed arguments. An explicit stop never counts
                // as "content emitted" on its own.
                provider_stop = Some(reason);
            }
            StreamChunk::Done => break,
            StreamChunk::Error(e) => {
                tracing::error!(error = %e, "provider stream error");
                return StreamAttempt::Errored {
                    error: e,
                    emitted_content,
                };
            }
        }
    }

    // The provider may end its stream without a Done chunk when the abort
    // signal stopped the download mid-flight; preserve the Aborted marker so
    // the persisted partial transcript stays honest. If the model emitted
    // tool calls the turn would otherwise continue; otherwise it ended.
    if accumulated.stop_reason != StopReason::Aborted {
        accumulated.stop_reason = if is_aborted(config) {
            StopReason::Aborted
        } else if let Some(reason) = provider_stop {
            reason
        } else if accumulated
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
        {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
    }

    if let Some(u) = usage.as_mut() {
        u.cache_read_tokens = (cache_read_total > 0).then_some(cache_read_total);
        u.cache_write_tokens = (cache_write_total > 0).then_some(cache_write_total);
    }
    accumulated.usage = usage;
    StreamAttempt::Done(accumulated)
}

/// Exponential backoff for retry `attempt` (1-based): `initial * 2^(attempt-1)`,
/// capped at `max`. Returns 0 when `initial` is 0 (no delay). Rate-limited
/// calls (HTTP 429) get a longer floor (`4 * initial`) so the fast schedule
/// doesn't deepen the limit. Jitter, when on, applies a bounded offset of
/// ± delay/4 derived from the wall clock (no rand dependency).
fn backoff_ms(attempt: usize, policy: &RetryPolicy, rate_limited: bool) -> u64 {
    if policy.initial_delay_ms == 0 {
        return 0;
    }
    let shift = attempt.saturating_sub(1).min(10);
    let base = policy.initial_delay_ms.saturating_mul(1u64 << shift);
    let mut delay = base.min(policy.max_delay_ms);
    if rate_limited {
        let floor = policy
            .initial_delay_ms
            .saturating_mul(4)
            .min(policy.max_delay_ms);
        delay = delay.max(floor);
    }
    if policy.jitter {
        let span = (delay / 4).max(1);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        let offset = nanos % (2 * span);
        delay = delay.saturating_add(span).saturating_sub(offset);
    }
    delay
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::context::{ModelSpec, ProviderApi};
    use crate::types::tool::ToolDefinition;
    use crate::ExtensionApi;
    use crate::StreamFn;
    use futures_util::stream;
    use std::sync::Arc;
    use std::sync::Mutex;

    use crate::types::session_event::SessionEvent;

    /// A mock StreamFn that replays a fixed chunk sequence.
    struct MockStream(Vec<StreamChunk>);
    impl StreamFn for MockStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[crate::types::tool::ToolDefinition],
            _signal: Option<crate::CancelSignal>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            Box::pin(stream::iter(self.0.clone()))
        }
    }

    fn test_config(chunks: Vec<StreamChunk>) -> AgentLoopConfig {
        AgentLoopConfig {
            model: ModelSpec {
                id: "test".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 1024,
            },
            max_turns: 5,
            max_tool_calls_per_turn: 5,
            tool_timeout: None,
            signal: None,
            stream_fn: std::sync::Arc::new(MockStream(chunks)),
            hooks: None,
            retry: crate::RetryPolicy::off(),
            steer: None,
            persist: None,
            transform_context: None,
        }
    }

    #[tokio::test]
    async fn loop_emits_text_and_ends() {
        // Model streams "Hello" then " world" and ends naturally.
        let config = test_config(vec![
            StreamChunk::TextDelta("Hello".into()),
            StreamChunk::TextDelta(" world".into()),
            StreamChunk::Usage {
                input: 3,
                output: 2,
                cache_read: 0,
                cache_write: 0,
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s1".into(),
        };

        let mut saw_start = false;
        let mut saw_end = false;
        let msgs = run_agent_loop(vec![], context, config, |ev| match ev {
            AgentEvent::AgentStart => saw_start = true,
            AgentEvent::AgentEnd => saw_end = true,
            _ => {}
        })
        .await
        .unwrap();

        assert!(saw_start && saw_end);
        // One assistant message with the full text.
        let any_assistant = msgs.iter().any(|m| {
            matches!(m, AgentMessage::Assistant(a) if a
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == "Hello world")))
        });
        assert!(any_assistant, "expected accumulated 'Hello world' text");
    }

    #[tokio::test]
    async fn loop_executes_tool_then_ends() {
        // A tool that echoes its args as text.
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo args".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        // Model: tool_call(echo, {"x":1}) -> then plain text "done".
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{\"x\":1}".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s2".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // Expect: Assistant(tool_call) + ToolResult(echo output).
        let has_tool_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name == "echo"
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "{\"x\":1}")))
        });
        assert!(has_tool_result, "expected echo tool result");
    }

    #[tokio::test]
    async fn loop_assembles_chunked_tool_call() {
        // Model streams ONE tool call across two deltas: the first carries
        // id+name, the second (continuation) carries only args with an empty
        // id - the OpenAI-compat streaming shape. Must assemble into ONE call,
        // not split into two.
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo args".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{\"x\":".into(),
            },
            StreamChunk::ToolCallDelta {
                index: None,
                id: String::new(),
                name: None,
                args_delta: "1}".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // Each turn replays the same chunked call, so the mock drives max_turns
        // turns. What matters is that the two deltas assemble into ONE call per
        // turn with full args - not a split into a named-no-args call plus an
        // empty-name-with-args call (the pre-fix behavior).
        let echo_results: Vec<_> = msgs
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolResult(tr) if tr.tool_name == "echo" => Some(tr.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !echo_results.is_empty(),
            "expected at least one echo tool result"
        );
        for tr in &echo_results {
            let text = match &tr.content[0] {
                ContentBlock::Text { text } => text.clone(),
                _ => panic!("expected text content"),
            };
            assert!(
                text.contains("\"x\":1"),
                "expected assembled args, got: {text}"
            );
        }
        // The bug would leak the args fragment into a split empty-name call.
        let split = msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name.is_empty()));
        assert!(
            !split,
            "chunked args leaked into a split empty-name tool call"
        );
    }

    #[tokio::test]
    async fn hung_tool_times_out_and_loop_continues() {
        // A tool whose Future never resolves must not wedge the loop: with
        // `tool_timeout` set, the engine cuts it off, records an error
        // tool_result (event + persist), and the run finishes normally.
        let hung = crate::types::tool::ToolDefinition {
            name: "hang".into(),
            label: "Hang".into(),
            description: "never resolves".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: ToolCallCtx| Box::pin(std::future::pending())),
        };
        let mut config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t-hang".into(),
                name: Some("hang".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        config.tool_timeout = Some(std::time::Duration::from_millis(50));

        let persisted = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let persist_sink = Arc::clone(&persisted);
        config.persist = Some(Arc::new(move |ev: &SessionEvent| {
            if matches!(ev, SessionEvent::ToolResult(_)) {
                persist_sink.lock().unwrap().push("tool_result");
            }
            Ok(())
        }));

        let mut saw_timeout_end = false;
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![hung],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s-hang".into(),
        };
        let msgs = run_agent_loop(vec![], context, config, |ev| {
            if let AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } = ev
            {
                if is_error
                    && result.tool_name == "hang"
                    && result.content.iter().any(
                        |b| matches!(b, ContentBlock::Text { text } if text.contains("timed out")),
                    )
                {
                    saw_timeout_end = true;
                }
            }
        })
        .await
        .unwrap();

        assert!(
            saw_timeout_end,
            "timeout must surface as an error ToolExecutionEnd"
        );
        assert!(
            msgs.iter().any(|m| matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "hang" && tr.is_error)),
            "the loop must record an error tool_result for the hung tool"
        );
        assert!(
            !persisted.lock().unwrap().is_empty(),
            "the timeout tool_result must be persisted"
        );
    }

    #[tokio::test]
    async fn loop_assembles_interleaved_indexed_tool_calls() {
        // OpenAI-compat parallel tool calls, fragments interleaved and keyed
        // by `index`. Both calls must execute with their own complete args;
        // the pre-fix accumulator appended B's fragment onto A's arguments.
        fn echo_tool(name: &'static str) -> ToolDefinition {
            ToolDefinition {
                name: name.into(),
                label: name.into(),
                description: "echo args".into(),
                parameters: serde_json::json!({"type": "object"}),
                risk: RiskLevel::Low,
                execute: std::sync::Arc::new(|c: ToolCallCtx| {
                    Box::pin(
                        async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                    )
                }),
            }
        }
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: Some(0),
                id: "t0".into(),
                name: Some("alpha".into()),
                args_delta: "{\"a\":".into(),
            },
            StreamChunk::ToolCallDelta {
                index: Some(1),
                id: "t1".into(),
                name: Some("beta".into()),
                args_delta: "{\"b\":".into(),
            },
            StreamChunk::ToolCallDelta {
                index: Some(0),
                id: String::new(),
                name: None,
                args_delta: "0}".into(),
            },
            StreamChunk::ToolCallDelta {
                index: Some(1),
                id: String::new(),
                name: None,
                args_delta: "1}".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo_tool("alpha"), echo_tool("beta")],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // The mock replays the same chunk list every turn; every execution
        // of alpha must carry {"a":0} and every beta {"b":1} - never the
        // cross-contaminated {"a":{"b":...}} of the pre-fix behavior.
        for m in &msgs {
            if let AgentMessage::ToolResult(tr) = m {
                let text = match &tr.content[0] {
                    ContentBlock::Text { text } => text.clone(),
                    _ => panic!("expected text content"),
                };
                match tr.tool_name.as_str() {
                    "alpha" => assert!(text.contains("\"a\":0"), "alpha args: {text}"),
                    "beta" => assert!(text.contains("\"b\":1"), "beta args: {text}"),
                    other => panic!("unexpected tool result: {other}"),
                }
            }
        }
        assert!(
            msgs.iter()
                .any(|m| matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name == "beta")),
            "both interleaved calls must execute"
        );
    }

    /// A mock StreamFn that records the message list it was called with.
    struct RecordingStream {
        chunks: Vec<StreamChunk>,
        seen: Arc<Mutex<Vec<Vec<AgentMessage>>>>,
    }
    impl StreamFn for RecordingStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[crate::types::tool::ToolDefinition],
            _signal: Option<crate::CancelSignal>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            self.seen.lock().unwrap().push(messages.to_vec());
            Box::pin(stream::iter(self.chunks.clone()))
        }
    }

    fn echo_tool() -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo args".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        }
    }

    // ---- direct tests for the loop's internal helpers -------------------

    /// An assistant message carrying the given tool calls
    /// `(id, name, raw arguments)`.
    fn assistant_with_calls(calls: &[(&str, &str, &str)]) -> AssistantMessage {
        let mut msg = AssistantMessage::new(&"test".to_string());
        for (id, name, args) in calls {
            msg.append_tool_call(
                None,
                id.to_string(),
                Some(name.to_string()),
                args.to_string(),
            );
        }
        msg
    }

    /// A context carrying the given tools.
    fn ctx_with_tools(tools: Vec<ToolDefinition>) -> AgentContext {
        AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools,
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        }
    }

    /// The text of a tool result's first content block.
    fn result_text(result: &ToolResultMessage) -> String {
        match result.content.first() {
            Some(ContentBlock::Text { text }) => text.clone(),
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    /// The text of an assistant message's first content block.
    fn result_text_assistant(msg: &AssistantMessage) -> String {
        match msg.content.first() {
            Some(ContentBlock::Text { text }) => text.clone(),
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    /// A stream fn that only counts how often it was polled.
    struct CountingStream {
        polls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl StreamFn for CountingStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system: &str,
            _tools: &[ToolDefinition],
            _signal: Option<crate::CancelSignal>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(futures_util::stream::iter(vec![]))
        }
    }

    // ---- is_aborted ------------------------------------------------------

    /// No signal at all means "not aborted".
    #[test]
    fn is_aborted_is_false_without_a_signal() {
        let config = test_config(vec![]);
        assert!(!is_aborted(&config));
    }

    /// The abort state follows the cancel signal: clear -> not aborted,
    /// cancelled -> aborted, reset -> not aborted again.
    #[test]
    fn is_aborted_follows_the_cancel_signal() {
        let mut config = test_config(vec![]);
        let sig = crate::CancelSignal::new();
        config.signal = Some(sig.clone());
        assert!(!is_aborted(&config));
        sig.cancel();
        assert!(is_aborted(&config));
        sig.reset();
        assert!(!is_aborted(&config));
    }

    // ---- persist_event ---------------------------------------------------

    /// Without a persist callback the event is dropped silently (no-op).
    #[test]
    fn persist_event_without_callback_is_a_noop() {
        let config = test_config(vec![]);
        assert!(persist_event(&config, &SessionEvent::TurnStart).is_ok());
    }

    /// The callback receives the event that was handed in.
    #[test]
    fn persist_event_forwards_the_event_to_the_callback() {
        let mut config = test_config(vec![]);
        let seen: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        config.persist = Some(Arc::new(move |ev: &SessionEvent| {
            sink.lock().unwrap().push(ev.clone());
            Ok(())
        }));

        persist_event(&config, &SessionEvent::TurnStart).unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(matches!(seen[0], SessionEvent::TurnStart));
    }

    /// A failing persist callback aborts the run (fail loud) - the error is
    /// propagated to the caller.
    #[test]
    fn persist_event_propagates_the_callback_error() {
        let mut config = test_config(vec![]);
        config.persist = Some(Arc::new(|_ev: &SessionEvent| {
            Err(AgentError::Io(std::io::Error::other("disk full")))
        }));

        let err = persist_event(&config, &SessionEvent::TurnStart).unwrap_err();
        assert!(matches!(err, AgentError::Io(_)), "{err:?}");
    }

    // ---- record_tool_result ----------------------------------------------

    /// A finished tool result is emitted, persisted and collected - in that
    /// order.
    #[test]
    fn record_tool_result_emits_persists_and_collects() {
        let mut config = test_config(vec![]);
        let persisted: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = persisted.clone();
        config.persist = Some(Arc::new(move |ev: &SessionEvent| {
            sink.lock().unwrap().push(ev.clone());
            Ok(())
        }));

        let result = ToolResultMessage {
            tool_call_id: "t1".into(),
            tool_name: "echo".into(),
            content: vec![ContentBlock::text("done")],
            is_error: false,
            timestamp: 7,
        };

        let mut events: Vec<AgentEvent> = Vec::new();
        let mut results: Vec<ToolResultMessage> = Vec::new();
        {
            let mut emit = |ev: AgentEvent| events.push(ev);
            record_tool_result(&config, &mut emit, &mut results, result.clone(), false).unwrap();
        }

        // Collected exactly once, unchanged.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], result);

        // Emitted as ToolExecutionEnd with the error flag.
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result: emitted,
                is_error,
            } => {
                assert_eq!(tool_call_id, "t1");
                assert_eq!(emitted, &result);
                assert!(!is_error);
            }
            other => panic!("expected ToolExecutionEnd, got {other:?}"),
        }

        // Persisted as a ToolResult session event.
        let persisted = persisted.lock().unwrap();
        assert_eq!(persisted.len(), 1);
        assert!(
            matches!(&persisted[0], SessionEvent::ToolResult(AgentMessage::ToolResult(tr))
                if tr.tool_call_id == "t1" && tr.tool_name == "echo"),
            "unexpected persisted event: {:?}",
            persisted[0]
        );
    }

    /// The `is_error` flag is passed through to the event, and a failing
    /// persist callback keeps the result OUT of the collected list (the run
    /// aborts before the fact is recorded).
    #[test]
    fn record_tool_result_flags_errors_and_drops_unpersisted_results() {
        let mut config = test_config(vec![]);
        config.persist = Some(Arc::new(|_ev: &SessionEvent| {
            Err(AgentError::Io(std::io::Error::other("disk full")))
        }));
        let result = ToolResultMessage {
            tool_call_id: "t1".into(),
            tool_name: "boom".into(),
            content: vec![ContentBlock::text("failed")],
            is_error: true,
            timestamp: 7,
        };

        let mut events: Vec<AgentEvent> = Vec::new();
        let mut results: Vec<ToolResultMessage> = Vec::new();
        let err = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            record_tool_result(&config, &mut emit, &mut results, result.clone(), true)
        }
        .unwrap_err();

        assert!(matches!(err, AgentError::Io(_)), "{err:?}");
        assert!(results.is_empty(), "an unpersisted result is not collected");
        // The event was still emitted before the persist failure.
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::ToolExecutionEnd { is_error: true, .. }
        ));
    }

    // ---- fail_all_tool_calls ---------------------------------------------

    /// Every tool call of a truncated assistant message gets an error
    /// result - none may be missed.
    #[test]
    fn fail_all_tool_calls_marks_every_call_as_failed() {
        let mut assistant = assistant_with_calls(&[("t1", "alpha", "{}"), ("t2", "beta", "{}")]);
        assistant.append_text("some prose");

        let results = fail_all_tool_calls(&assistant);

        assert_eq!(results.len(), 2, "every tool call must get a result");
        assert_eq!(results[0].tool_call_id, "t1");
        assert_eq!(results[0].tool_name, "alpha");
        assert_eq!(results[1].tool_call_id, "t2");
        assert_eq!(results[1].tool_name, "beta");
        for r in &results {
            assert!(r.is_error, "a discarded call must be flagged as an error");
            let text = result_text(r);
            assert!(text.contains("truncated"), "{text}");
        }
    }

    /// An assistant message without tool calls has nothing to discard.
    #[test]
    fn fail_all_tool_calls_without_tool_calls_is_empty() {
        let mut assistant = AssistantMessage::new(&"test".to_string());
        assistant.append_text("just prose");
        assert!(fail_all_tool_calls(&assistant).is_empty());
    }

    // ---- error_tool_result -----------------------------------------------

    /// The error result carries the call identity, the message and the error
    /// flag.
    #[test]
    fn error_tool_result_keeps_the_identity_and_flags_the_error() {
        let r = error_tool_result("call-1", "bash", "boom");
        assert!(r.is_error);
        assert_eq!(r.tool_call_id, "call-1");
        assert_eq!(r.tool_name, "bash");
        assert_eq!(r.content.len(), 1);
        assert_eq!(result_text(&r), "boom");
        assert!(r.timestamp > 0, "the result must be timestamped");
    }

    /// An empty message is still a valid error result (no panic, no default
    /// text invented).
    #[test]
    fn error_tool_result_accepts_an_empty_message() {
        let r = error_tool_result("", "", "");
        assert!(r.is_error);
        assert_eq!(r.tool_call_id, "");
        assert_eq!(r.tool_name, "");
        assert_eq!(result_text(&r), "");
    }

    // ---- execute_tool_calls ----------------------------------------------

    /// A batch without tool calls does nothing: no results, no events.
    #[tokio::test]
    async fn execute_tool_calls_without_tool_calls_returns_nothing() {
        let mut assistant = AssistantMessage::new(&"test".to_string());
        assistant.append_text("no tools here");
        let config = test_config(vec![]);
        let ctx = ctx_with_tools(vec![]);
        let mut guard = crate::guard::RepeatGuard::new();

        let mut events: Vec<AgentEvent> = Vec::new();
        let results = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            execute_tool_calls(&ctx, &assistant, &config, &mut guard, &mut emit)
                .await
                .unwrap()
        };

        assert!(results.is_empty());
        assert!(events.is_empty(), "an empty batch must emit no events");
    }

    /// A single tool call is executed and its result recorded.
    #[tokio::test]
    async fn execute_tool_calls_runs_a_single_tool_and_records_its_result() {
        let assistant = assistant_with_calls(&[("t1", "echo", "{\"x\":1}")]);
        let config = test_config(vec![]);
        let ctx = ctx_with_tools(vec![echo_tool()]);
        let mut guard = crate::guard::RepeatGuard::new();

        let mut events: Vec<AgentEvent> = Vec::new();
        let results = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            execute_tool_calls(&ctx, &assistant, &config, &mut guard, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_call_id, "t1");
        assert_eq!(results[0].tool_name, "echo");
        assert!(!results[0].is_error);
        assert_eq!(result_text(&results[0]), "{\"x\":1}");

        // Start and End events for the executed call.
        let starts = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolExecutionStart { .. }))
            .count();
        let ends = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
            .count();
        assert_eq!(starts, 1);
        assert_eq!(ends, 1);
    }

    /// Parallel calls are dispatched together and their results are recorded
    /// in DECLARATION order, no matter which one finishes first.
    #[tokio::test]
    async fn execute_tool_calls_runs_parallel_tools_in_declaration_order() {
        // The first tool yields a few times before answering, so the second
        // one is very likely to finish first.
        let slow = crate::types::tool::ToolDefinition {
            name: "slow".into(),
            label: "Slow".into(),
            description: "yields".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: ToolCallCtx| {
                Box::pin(async move {
                    for _ in 0..5 {
                        tokio::task::yield_now().await;
                    }
                    Ok(crate::types::tool::ToolResult::text("slow"))
                })
            }),
        };
        let fast = crate::types::tool::ToolDefinition {
            name: "fast".into(),
            label: "Fast".into(),
            description: "answers at once".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: ToolCallCtx| {
                Box::pin(async move { Ok(crate::types::tool::ToolResult::text("fast")) })
            }),
        };
        let assistant = assistant_with_calls(&[("t1", "slow", "{}"), ("t2", "fast", "{}")]);
        let config = test_config(vec![]);
        let ctx = ctx_with_tools(vec![slow, fast]);
        let mut guard = crate::guard::RepeatGuard::new();

        let mut events: Vec<AgentEvent> = Vec::new();
        let results = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            execute_tool_calls(&ctx, &assistant, &config, &mut guard, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_name, "slow");
        assert_eq!(results[1].tool_name, "fast");

        // The Start events follow the declaration order too.
        let started: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolExecutionStart { tool_name, .. } => Some(tool_name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(started, vec!["slow".to_string(), "fast".to_string()]);
    }

    /// A tool that returns `Err` becomes an error result - the batch does
    /// not abort.
    #[tokio::test]
    async fn execute_tool_calls_reports_a_tool_error_as_an_error_result() {
        let boom = crate::types::tool::ToolDefinition {
            name: "boom".into(),
            label: "Boom".into(),
            description: "always fails".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: ToolCallCtx| {
                Box::pin(async move { Err(crate::error::ToolError::Message("boom".into())) })
            }),
        };
        let assistant = assistant_with_calls(&[("t1", "boom", "{}")]);
        let config = test_config(vec![]);
        let ctx = ctx_with_tools(vec![boom]);
        let mut guard = crate::guard::RepeatGuard::new();

        let mut events: Vec<AgentEvent> = Vec::new();
        let results = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            execute_tool_calls(&ctx, &assistant, &config, &mut guard, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(result_text(&results[0]).contains("boom"));
        assert!(matches!(
            &events.last().unwrap(),
            AgentEvent::ToolExecutionEnd { is_error: true, .. }
        ));
    }

    /// An unknown tool is reported as an error result instead of crashing.
    #[tokio::test]
    async fn execute_tool_calls_reports_an_unknown_tool() {
        let assistant = assistant_with_calls(&[("t1", "ghost", "{}")]);
        let config = test_config(vec![]);
        let ctx = ctx_with_tools(vec![]);
        let mut guard = crate::guard::RepeatGuard::new();

        let mut events: Vec<AgentEvent> = Vec::new();
        let results = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            execute_tool_calls(&ctx, &assistant, &config, &mut guard, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(result_text(&results[0]).contains("tool not found: ghost"));
        // A rejected call is never dispatched.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolExecutionStart { .. })),
            "an unknown tool must not be dispatched"
        );
    }

    /// Malformed arguments are fed back to the model as an error result.
    #[tokio::test]
    async fn execute_tool_calls_reports_malformed_arguments() {
        let assistant = assistant_with_calls(&[("t1", "echo", "{\"x\":")]);
        let config = test_config(vec![]);
        let ctx = ctx_with_tools(vec![echo_tool()]);
        let mut guard = crate::guard::RepeatGuard::new();

        let mut events: Vec<AgentEvent> = Vec::new();
        let results = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            execute_tool_calls(&ctx, &assistant, &config, &mut guard, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(result_text(&results[0]).contains("failed to parse tool arguments"));
    }

    /// Empty arguments are treated as `{}` - a tool without parameters.
    #[tokio::test]
    async fn execute_tool_calls_treats_empty_arguments_as_an_empty_object() {
        let assistant = assistant_with_calls(&[("t1", "echo", "")]);
        let config = test_config(vec![]);
        let ctx = ctx_with_tools(vec![echo_tool()]);
        let mut guard = crate::guard::RepeatGuard::new();

        let mut events: Vec<AgentEvent> = Vec::new();
        let results = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            execute_tool_calls(&ctx, &assistant, &config, &mut guard, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(results.len(), 1);
        assert!(!results[0].is_error);
        assert_eq!(result_text(&results[0]), "{}");
    }

    /// Calls beyond the per-turn limit are reported, never dropped silently.
    #[tokio::test]
    async fn execute_tool_calls_reports_calls_over_the_limit() {
        let assistant = assistant_with_calls(&[("t1", "echo", "{}"), ("t2", "echo", "{}")]);
        let mut config = test_config(vec![]);
        config.max_tool_calls_per_turn = 1;
        let ctx = ctx_with_tools(vec![echo_tool()]);
        let mut guard = crate::guard::RepeatGuard::new();

        let mut events: Vec<AgentEvent> = Vec::new();
        let results = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            execute_tool_calls(&ctx, &assistant, &config, &mut guard, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(results.len(), 2, "the dropped call still gets a result");
        assert!(!results[0].is_error);
        assert!(results[1].is_error);
        assert!(result_text(&results[1]).contains("tool call limit"));
        assert_eq!(results[1].tool_call_id, "t2");
    }

    /// An already-cancelled signal stops the batch before any tool runs.
    #[tokio::test]
    async fn execute_tool_calls_skips_the_batch_when_already_aborted() {
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = ran.clone();
        let counted = crate::types::tool::ToolDefinition {
            name: "counted".into(),
            label: "Counted".into(),
            description: "counts executions".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(move |_c: ToolCallCtx| {
                let counter = counter.clone();
                Box::pin(async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(crate::types::tool::ToolResult::text("ran"))
                })
            }),
        };
        let assistant = assistant_with_calls(&[("t1", "counted", "{}")]);
        let mut config = test_config(vec![]);
        let sig = crate::CancelSignal::new();
        sig.cancel();
        config.signal = Some(sig);
        let ctx = ctx_with_tools(vec![counted]);
        let mut guard = crate::guard::RepeatGuard::new();

        let mut events: Vec<AgentEvent> = Vec::new();
        let results = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            execute_tool_calls(&ctx, &assistant, &config, &mut guard, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an aborted batch must not execute any tool"
        );
        assert!(results.is_empty());
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolExecutionStart { .. })),
            "an aborted batch must not dispatch a tool"
        );
    }

    // ---- stream_assistant_response ---------------------------------------

    /// An empty provider stream ends the turn with an empty message.
    #[tokio::test]
    async fn stream_assistant_response_with_an_empty_stream_ends_the_turn() {
        let config = test_config(vec![]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let msg = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            stream_assistant_response(&ctx, &config, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(msg.stop_reason, StopReason::EndTurn);
        assert!(msg.content.is_empty());
        assert_eq!(msg.model, "test");
        assert!(msg.usage.is_none());

        // The host is told the message ended and the response arrived.
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::MessageEnd { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::AfterProviderResponse { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::BeforeProviderRequest { .. })));
    }

    /// Text deltas are concatenated into ONE text block and streamed to the
    /// host as they arrive.
    #[tokio::test]
    async fn stream_assistant_response_concatenates_text_deltas() {
        let config = test_config(vec![
            StreamChunk::TextDelta("Hello".into()),
            StreamChunk::TextDelta(" world".into()),
            StreamChunk::Done,
        ]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let msg = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            stream_assistant_response(&ctx, &config, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(msg.content.len(), 1);
        assert_eq!(result_text_assistant(&msg), "Hello world");
        assert_eq!(msg.stop_reason, StopReason::EndTurn);

        let deltas = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::MessageUpdate {
                        delta: ContentDelta::TextDelta(_)
                    }
                )
            })
            .count();
        assert_eq!(deltas, 2, "one update per streamed delta");
    }

    /// A provider error surfaces as `StopReason::Error` (no retry with the
    /// default "retries off" policy).
    #[tokio::test]
    async fn stream_assistant_response_reports_a_provider_error() {
        let config = test_config(vec![StreamChunk::Error("provider boom".into())]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let msg = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            stream_assistant_response(&ctx, &config, &mut emit)
                .await
                .unwrap()
        };

        match &msg.stop_reason {
            StopReason::Error(e) => assert!(e.contains("provider boom"), "{e}"),
            other => panic!("expected StopReason::Error, got {other:?}"),
        }
        assert!(msg.content.is_empty());
    }

    /// A cancel that arrives before the provider request skips the request
    /// entirely and reports an aborted message.
    #[tokio::test]
    async fn stream_assistant_response_skips_the_provider_when_aborted() {
        let mut config = test_config(vec![]);
        let sig = crate::CancelSignal::new();
        sig.cancel();
        config.signal = Some(sig);
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        config.stream_fn = Arc::new(CountingStream {
            polls: polls.clone(),
        });
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let msg = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            stream_assistant_response(&ctx, &config, &mut emit)
                .await
                .unwrap()
        };

        assert_eq!(msg.stop_reason, StopReason::Aborted);
        assert_eq!(
            polls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an aborted call must not reach the provider"
        );
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::BeforeProviderRequest { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::MessageEnd { .. })));
    }

    // ---- attempt_stream_once ---------------------------------------------

    /// An empty stream yields an ended message without usage.
    #[tokio::test]
    async fn attempt_stream_once_with_an_empty_stream_yields_end_turn() {
        let config = test_config(vec![]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let outcome = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            attempt_stream_once(&[], &ctx, &config, &mut emit).await
        };

        match outcome {
            StreamAttempt::Done(msg) => {
                assert_eq!(msg.stop_reason, StopReason::EndTurn);
                assert!(msg.content.is_empty());
                assert!(msg.usage.is_none());
            }
            StreamAttempt::Errored { .. } => panic!("expected a completed stream"),
        }
    }

    /// Text deltas are concatenated into a single block.
    #[tokio::test]
    async fn attempt_stream_once_accumulates_text_deltas() {
        let config = test_config(vec![
            StreamChunk::TextDelta("Hello".into()),
            StreamChunk::TextDelta(" world".into()),
            StreamChunk::Done,
        ]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let outcome = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            attempt_stream_once(&[], &ctx, &config, &mut emit).await
        };

        match outcome {
            StreamAttempt::Done(msg) => {
                assert_eq!(msg.content.len(), 1);
                assert_eq!(result_text_assistant(&msg), "Hello world");
                assert_eq!(msg.stop_reason, StopReason::EndTurn);
            }
            StreamAttempt::Errored { .. } => panic!("expected a completed stream"),
        }
    }

    /// A stream error before any content is retryable (nothing was emitted
    /// to the host yet).
    #[tokio::test]
    async fn attempt_stream_once_reports_an_error_without_emitted_content() {
        let config = test_config(vec![StreamChunk::Error("boom".into())]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let outcome = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            attempt_stream_once(&[], &ctx, &config, &mut emit).await
        };

        match outcome {
            StreamAttempt::Errored {
                error,
                emitted_content,
            } => {
                assert_eq!(error, "boom");
                assert!(!emitted_content, "nothing was streamed yet");
            }
            StreamAttempt::Done(_) => panic!("expected a failed stream"),
        }
    }

    /// A stream error AFTER content was emitted is NOT retryable - the
    /// caller must not duplicate the partial output.
    #[tokio::test]
    async fn attempt_stream_once_reports_an_error_after_content() {
        let config = test_config(vec![
            StreamChunk::TextDelta("partial".into()),
            StreamChunk::Error("mid-stream boom".into()),
        ]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let outcome = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            attempt_stream_once(&[], &ctx, &config, &mut emit).await
        };

        match outcome {
            StreamAttempt::Errored {
                error,
                emitted_content,
            } => {
                assert_eq!(error, "mid-stream boom");
                assert!(emitted_content, "content was streamed before the error");
            }
            StreamAttempt::Done(_) => panic!("expected a failed stream"),
        }
    }

    /// The provider's stop signal wins over the content-based guess.
    #[tokio::test]
    async fn attempt_stream_once_provider_stop_overrides_the_content_guess() {
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{\"x\":".into(),
            },
            StreamChunk::Stop(StopReason::MaxTokens),
            StreamChunk::Done,
        ]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let outcome = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            attempt_stream_once(&[], &ctx, &config, &mut emit).await
        };

        match outcome {
            StreamAttempt::Done(msg) => {
                assert_eq!(
                    msg.stop_reason,
                    StopReason::MaxTokens,
                    "the provider stop must win over the ToolUse guess"
                );
            }
            StreamAttempt::Errored { .. } => panic!("expected a completed stream"),
        }
    }

    /// Without a provider stop, a tool call classifies the turn as ToolUse.
    #[tokio::test]
    async fn attempt_stream_once_guesses_tool_use_from_a_tool_call() {
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let outcome = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            attempt_stream_once(&[], &ctx, &config, &mut emit).await
        };

        match outcome {
            StreamAttempt::Done(msg) => assert_eq!(msg.stop_reason, StopReason::ToolUse),
            StreamAttempt::Errored { .. } => panic!("expected a completed stream"),
        }
    }

    /// Usage chunks are merged, cache counters included.
    #[tokio::test]
    async fn attempt_stream_once_merges_usage_chunks() {
        let config = test_config(vec![
            StreamChunk::Usage {
                input: 42,
                output: 0,
                cache_read: 100,
                cache_write: 50,
            },
            StreamChunk::Usage {
                input: 0,
                output: 7,
                cache_read: 0,
                cache_write: 0,
            },
            StreamChunk::Done,
        ]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let outcome = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            attempt_stream_once(&[], &ctx, &config, &mut emit).await
        };

        match outcome {
            StreamAttempt::Done(msg) => {
                let usage = msg.usage.expect("usage must be reported");
                assert_eq!(usage.input_tokens, 42);
                assert_eq!(usage.output_tokens, 7);
                assert_eq!(usage.cache_read_tokens, Some(100));
                assert_eq!(usage.cache_write_tokens, Some(50));
            }
            StreamAttempt::Errored { .. } => panic!("expected a completed stream"),
        }
    }

    /// Unreported cache tokens stay `None` (absent = unknown).
    #[tokio::test]
    async fn attempt_stream_once_leaves_unreported_cache_tokens_none() {
        let config = test_config(vec![
            StreamChunk::Usage {
                input: 42,
                output: 7,
                cache_read: 0,
                cache_write: 0,
            },
            StreamChunk::Done,
        ]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let outcome = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            attempt_stream_once(&[], &ctx, &config, &mut emit).await
        };

        match outcome {
            StreamAttempt::Done(msg) => {
                let usage = msg.usage.expect("usage must be reported");
                assert!(usage.cache_read_tokens.is_none());
                assert!(usage.cache_write_tokens.is_none());
            }
            StreamAttempt::Errored { .. } => panic!("expected a completed stream"),
        }
    }

    /// Thinking deltas are accumulated in their own block.
    #[tokio::test]
    async fn attempt_stream_once_accumulates_thinking_deltas() {
        let config = test_config(vec![
            StreamChunk::ThinkingDelta("let me think".into()),
            StreamChunk::ThinkingDelta(" harder".into()),
            StreamChunk::Done,
        ]);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let outcome = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            attempt_stream_once(&[], &ctx, &config, &mut emit).await
        };

        match outcome {
            StreamAttempt::Done(msg) => {
                assert_eq!(msg.content.len(), 1);
                match &msg.content[0] {
                    ContentBlock::Thinking { thinking } => {
                        assert_eq!(thinking, "let me think harder")
                    }
                    other => panic!("expected a thinking block, got {other:?}"),
                }
            }
            StreamAttempt::Errored { .. } => panic!("expected a completed stream"),
        }
    }

    /// A cancelled signal during the stream keeps the aborted marker, even
    /// though the partial content was still accumulated.
    #[tokio::test]
    async fn attempt_stream_once_marks_the_message_aborted_when_cancelled() {
        let mut config = test_config(vec![
            StreamChunk::TextDelta("partial".into()),
            StreamChunk::Done,
        ]);
        let sig = crate::CancelSignal::new();
        sig.cancel();
        config.signal = Some(sig);
        let ctx = ctx_with_tools(vec![]);

        let mut events: Vec<AgentEvent> = Vec::new();
        let outcome = {
            let mut emit = |ev: AgentEvent| events.push(ev);
            attempt_stream_once(&[], &ctx, &config, &mut emit).await
        };

        match outcome {
            StreamAttempt::Done(msg) => {
                assert_eq!(msg.stop_reason, StopReason::Aborted);
                assert_eq!(result_text_assistant(&msg), "partial");
            }
            StreamAttempt::Errored { .. } => panic!("expected a completed stream"),
        }
    }

    #[tokio::test]
    async fn transform_context_applies_before_each_llm_call() {
        // The seam must run before EVERY provider call, not just the first:
        // with a tool-calling mock driving max_turns turns, the wire view
        // stays compacted even as the accumulator grows turn over turn.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        config.max_turns = 3;
        config.stream_fn = Arc::new(RecordingStream {
            chunks: vec![
                StreamChunk::ToolCallDelta {
                    index: None,
                    id: "t1".into(),
                    name: Some("echo".into()),
                    args_delta: "{}".into(),
                },
                StreamChunk::Done,
            ],
            seen: Arc::clone(&seen),
        });
        let calls2 = Arc::clone(&calls);
        config.transform_context = Some(Arc::new(move |msgs: &[AgentMessage]| {
            calls2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Keep only the last two messages (any compaction-like policy).
            Ok(msgs.iter().rev().take(2).rev().cloned().collect())
        }));
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo_tool()],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        run_agent_loop(
            vec![AgentMessage::User(crate::types::message::UserMessage {
                content: vec![ContentBlock::text("go")],
                timestamp: 1,
            })],
            context,
            config,
            |_| {},
        )
        .await
        .unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "one wire view per LLM call");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "transform invoked once per LLM call"
        );
        // Accumulator grows 1 → 3 → 5 messages across turns; the wire view
        // must stay capped at 2 after the first call.
        assert_eq!(seen[0].len(), 1, "first call sees the seeded prompt");
        assert!(
            seen[1].len() <= 2,
            "second call compacted, got {}",
            seen[1].len()
        );
        assert!(
            seen[2].len() <= 2,
            "third call compacted, got {}",
            seen[2].len()
        );
    }

    #[tokio::test]
    async fn transform_context_error_fails_the_run_loud() {
        let mut config = test_config(vec![StreamChunk::Done]);
        config.transform_context = Some(Arc::new(|_msgs: &[AgentMessage]| {
            Err(AgentError::ContextTransform("budget exploded".into()))
        }));
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let err = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(err, AgentError::ContextTransform(ref m) if m.contains("budget exploded")),
            "expected ContextTransform error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn transform_context_never_touches_accumulator_or_persisted_events() {
        // The transformed list is a WIRE VIEW only: what the loop
        // accumulates, returns, and persists is always the full history.
        let mut config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        config.max_turns = 2;
        let persisted: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let p2 = Arc::clone(&persisted);
        config.persist = Some(Arc::new(move |ev: &SessionEvent| {
            p2.lock().unwrap().push(ev.clone());
            Ok(())
        }));
        config.transform_context = Some(Arc::new(|msgs: &[AgentMessage]| {
            Ok(msgs.iter().rev().take(1).rev().cloned().collect())
        }));
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo_tool()],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // Full accumulator: 2 × (assistant + tool result) = 4 messages,
        // none dropped despite the transform keeping only the last one.
        assert_eq!(msgs.len(), 4, "returned messages must be uncompacted");
        let persisted = persisted.lock().unwrap();
        let assistants = persisted
            .iter()
            .filter(|ev| matches!(ev, SessionEvent::Assistant { .. }))
            .count();
        assert_eq!(assistants, 2, "persisted assistant events uncompacted");
    }

    /// A `before_tool_call` handler that blocks the `bash` tool.
    struct BlockBash;
    impl crate::extension::BeforeToolCallHandler for BlockBash {
        fn call(
            &self,
            _id: &str,
            tool_name: &str,
            _args: &serde_json::Value,
            _risk: RiskLevel,
        ) -> ToolCallVerdict {
            if tool_name == "bash" {
                ToolCallVerdict::Block("blocked by policy".into())
            } else {
                ToolCallVerdict::Allow
            }
        }
    }

    /// An `after_tool_call` handler that redacts text content.
    struct Redactor;
    impl crate::extension::AfterToolCallHandler for Redactor {
        fn call(
            &self,
            _id: &str,
            _result: &crate::ToolResultMessage,
        ) -> Option<crate::ToolResultMessage> {
            Some(crate::ToolResultMessage {
                tool_call_id: "t1".into(),
                tool_name: "echo".into(),
                content: vec![ContentBlock::text("[REDACTED]")],
                is_error: false,
                timestamp: 0,
            })
        }
    }

    #[tokio::test]
    async fn before_hook_blocks_tool() {
        // Register a BlockBash handler, then have the model call `bash`.
        let mut api = crate::extension::ExtensionApiImpl::new();
        api.register_before_tool_call(Box::new(BlockBash));

        let echo = crate::ToolDefinition {
            name: "bash".into(),
            label: "Bash".into(),
            description: "shell".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: ToolCallCtx| {
                Box::pin(async move { Ok(crate::ToolResult::text("ran")) })
            }),
        };
        let config = AgentLoopConfig {
            model: ModelSpec {
                id: "test".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 1024,
            },
            max_turns: 5,
            max_tool_calls_per_turn: 5,
            tool_timeout: None,
            signal: None,
            stream_fn: std::sync::Arc::new(MockStream(vec![
                StreamChunk::ToolCallDelta {
                    index: None,
                    id: "t1".into(),
                    name: Some("bash".into()),
                    args_delta: "{\"command\":\"rm -rf /\"}".into(),
                },
                StreamChunk::Done,
            ])),
            hooks: Some(std::sync::Arc::new(api)),
            persist: None,
            transform_context: None,
            retry: crate::RetryPolicy::off(),
            steer: None,
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s3".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // The tool was NOT executed — instead we got an error result with the
        // block reason.
        let blocked = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "bash" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "blocked by policy")))
        });
        assert!(blocked, "expected bash to be blocked, not executed");
    }

    #[tokio::test]
    async fn after_hook_redacts_result() {
        let mut api = crate::extension::ExtensionApiImpl::new();
        api.register_after_tool_call(Box::new(Redactor));

        let echo = crate::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(async move { Ok(crate::ToolResult::text(c.args.to_string())) })
            }),
        };
        let config = AgentLoopConfig {
            model: ModelSpec {
                id: "test".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 1024,
            },
            max_turns: 5,
            max_tool_calls_per_turn: 5,
            tool_timeout: None,
            signal: None,
            stream_fn: std::sync::Arc::new(MockStream(vec![
                StreamChunk::ToolCallDelta {
                    index: None,
                    id: "t1".into(),
                    name: Some("echo".into()),
                    args_delta: "{\"secret\":1}".into(),
                },
                StreamChunk::Done,
            ])),
            hooks: Some(std::sync::Arc::new(api)),
            persist: None,
            transform_context: None,
            retry: crate::RetryPolicy::off(),
            steer: None,
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s4".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // Original secret output was replaced by [REDACTED].
        let redacted = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "[REDACTED]")))
        });
        let leaked = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("secret"))))
        });
        assert!(redacted, "expected redacted result");
        assert!(!leaked, "secret must not appear in output");
    }

    #[tokio::test]
    async fn loop_recovers_from_tool_error() {
        // A tool whose execute returns Err. The loop must feed an error
        // tool_result back to the LLM and continue, not abort the run.
        let boom = crate::types::tool::ToolDefinition {
            name: "boom".into(),
            label: "Boom".into(),
            description: "always fails".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: ToolCallCtx| {
                Box::pin(async move { Err(crate::error::ToolError::Message("boom".into())) })
            }),
        };
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("boom".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![boom],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let has_error_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "boom" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("boom"))))
        });
        assert!(
            has_error_result,
            "expected an error tool_result, not a crash"
        );
    }

    #[tokio::test]
    async fn loop_handles_malformed_tool_args() {
        // The model streams malformed argument JSON. The loop must report a
        // parse error as a tool_result and continue, not crash inside the tool.
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{\"command\":".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let has_error_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "echo" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("failed to parse tool arguments"))))
        });
        assert!(
            has_error_result,
            "expected a parse-error tool_result, not a crash"
        );
    }

    #[tokio::test]
    async fn loop_handles_unknown_tool() {
        // The model calls a tool that was never registered. The loop must
        // report "tool not found" as a tool_result and continue.
        let config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("ghost".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let has_error_result = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.tool_name == "ghost" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("tool not found"))))
        });
        assert!(
            has_error_result,
            "expected a not-found tool_result, not a crash"
        );
    }

    /// A mock that flips the abort signal on when streaming starts, then yields
    /// a text delta + Done. Exercises the in-stream abort check.
    struct FlipOnStream;
    impl StreamFn for FlipOnStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[crate::types::tool::ToolDefinition],
            signal: Option<crate::CancelSignal>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            if let Some(s) = signal {
                s.cancel();
            }
            Box::pin(stream::iter(vec![
                StreamChunk::TextDelta("partial".into()),
                StreamChunk::Done,
            ]))
        }
    }

    #[tokio::test]
    async fn loop_aborts_during_stream() {
        // The signal flips during streaming (the mock sets it when stream() is
        // called), so the top-of-turn guard doesn't fire first. The stream must
        // stop after the first chunk and carry StopReason::Aborted.
        let config = AgentLoopConfig {
            model: ModelSpec {
                id: "test".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 1024,
            },
            max_turns: 5,
            max_tool_calls_per_turn: 5,
            tool_timeout: None,
            signal: Some(crate::CancelSignal::new()),
            stream_fn: std::sync::Arc::new(FlipOnStream),
            hooks: None,
            persist: None,
            transform_context: None,
            retry: crate::RetryPolicy::off(),
            steer: None,
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let aborted = msgs.iter().any(
            |m| matches!(m, AgentMessage::Assistant(a) if a.stop_reason == StopReason::Aborted),
        );
        assert!(
            aborted,
            "expected an assistant message with stop_reason Aborted"
        );
    }

    #[tokio::test]
    async fn loop_aborts_mid_tool_batch() {
        // Two tool calls in one turn. The first (`set_abort`) cancels the
        // signal mid-batch. Under concurrent execution BOTH calls were
        // already dispatched, so both complete and record results in
        // declaration order - what the abort must guarantee is that no
        // further provider request happens afterwards.
        let set_abort = crate::types::tool::ToolDefinition {
            name: "set_abort".into(),
            label: "SetAbort".into(),
            description: "sets abort".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(async move {
                    c.signal.store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(crate::types::tool::ToolResult::text("flipped"))
                })
            }),
        };
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        // Script comes from AbortScriptStream below; test_config only sets
        // defaults.
        let mut config = test_config(vec![]);
        config.signal = Some(crate::CancelSignal::new());

        // The provider must be consulted exactly once: the abort (raised
        // during the tool batch) must stop the loop before a 2nd request.
        struct AbortScriptStream(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl StreamFn for AbortScriptStream {
            fn stream(
                &self,
                _model: &ModelSpec,
                _messages: &[AgentMessage],
                _system: &str,
                _tools: &[ToolDefinition],
                _signal: Option<crate::CancelSignal>,
            ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
                let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(n, 0, "provider must not be polled again after abort");
                Box::pin(futures_util::stream::iter(vec![
                    StreamChunk::ToolCallDelta {
                        index: None,
                        id: "t1".into(),
                        name: Some("set_abort".into()),
                        args_delta: "{}".into(),
                    },
                    StreamChunk::ToolCallDelta {
                        index: None,
                        id: "t2".into(),
                        name: Some("echo".into()),
                        args_delta: "{}".into(),
                    },
                    StreamChunk::Done,
                ]))
            }
        }
        let provider_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        config.stream_fn = std::sync::Arc::new(AbortScriptStream(provider_calls.clone()));

        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![set_abort, echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();
        assert_eq!(
            provider_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "abort must stop the loop before another provider request"
        );

        let ran_set_abort = msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name == "set_abort"));
        let ran_echo = msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(tr) if tr.tool_name == "echo"));
        assert!(
            ran_set_abort,
            "first tool should have executed before abort"
        );
        // Concurrent batches run already-dispatched calls to completion so
        // every declared call gets a result (no dangling tool_call).
        assert!(
            ran_echo,
            "dispatched second tool must complete even after mid-batch abort"
        );
        // The abort must stop the LOOP, not the batch: no further provider
        // request. run_agent_loop returned after the tool round, so the
        // single scripted provider call above was the last one - the
        // signal-carrying loop config is what enforced it.
    }

    #[tokio::test]
    async fn tool_batch_runs_concurrently_and_records_in_declaration_order() {
        // Three 150ms tools in one batch must overlap (total well under the
        // 450ms serial floor), and their results must land in the session
        // in declaration order despite intentionally staggered finish times.
        let slow = |name: &'static str, delay_ms: u64| crate::types::tool::ToolDefinition {
            name: name.into(),
            label: name.into(),
            description: name.into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(move |_| {
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    Ok(crate::types::tool::ToolResult::text(name))
                })
            }),
        };
        // first is slowest on purpose: a serial recorder would see it finish
        // last if it recorded at completion time instead of by slot order.
        let tools = vec![slow("first", 150), slow("second", 90), slow("third", 60)];

        let mut config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("first".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t2".into(),
                name: Some("second".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t3".into(),
                name: Some("third".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        // The mock replays the same script every call; stop after the batch.
        config.max_turns = 1;
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools,
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let started = std::time::Instant::now();
        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();
        let elapsed = started.elapsed();

        let order: Vec<&str> = msgs
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolResult(tr) => Some(tr.tool_name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(order, vec!["first", "second", "third"]);
        assert!(
            elapsed < std::time::Duration::from_millis(420),
            "three 150/90/60ms tools took {elapsed:?} - batch is not concurrent"
        );
    }

    /// A mock that errors `failures` times, then replays `success`. Shares a
    /// call counter across attempts (StreamFn is `&self`).
    struct FlakyStream {
        failures: usize,
        success: Vec<StreamChunk>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl StreamFn for FlakyStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[crate::types::tool::ToolDefinition],
            _signal: Option<crate::CancelSignal>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.failures {
                Box::pin(stream::iter(vec![StreamChunk::Error("transient".into())]))
            } else {
                Box::pin(stream::iter(self.success.clone()))
            }
        }
    }

    #[test]
    fn backoff_rate_limited_gets_longer_floor() {
        let policy = RetryPolicy {
            max_retries: 3,
            initial_delay_ms: 100,
            max_delay_ms: 8_000,
            jitter: false,
        };
        // Attempt 1: base 100ms; rate-limited floor 400ms wins.
        assert_eq!(backoff_ms(1, &policy, true), 400);
        // Attempt 1 without rate limiting stays on the fast schedule.
        assert_eq!(backoff_ms(1, &policy, false), 100);
        // Once the exponential curve passes the floor, both are equal.
        assert_eq!(backoff_ms(4, &policy, true), 800);
        assert_eq!(backoff_ms(4, &policy, false), 800);
    }

    #[test]
    fn backoff_jitter_stays_bounded() {
        let policy = RetryPolicy {
            max_retries: 5,
            initial_delay_ms: 100,
            max_delay_ms: 8_000,
            jitter: true,
        };
        for attempt in 1..=5 {
            let d = backoff_ms(attempt, &policy, false);
            let base = backoff_ms(
                attempt,
                &RetryPolicy {
                    jitter: false,
                    ..policy.clone()
                },
                false,
            );
            let span = (base / 4).max(1);
            assert!(d + span >= base, "jitter dipped below bound");
            assert!(d <= base + span, "jitter exceeded bound");
        }
    }

    #[tokio::test]
    async fn steer_queue_injects_user_message_next_turn() {
        // Turn 1: model streams text and ends; a steer message queued before
        // the run must appear as a User message in the LLM input and in the
        // persisted event log.
        let steer = crate::SteerQueue::new();
        steer.push("use plan B");
        let mut config = test_config(vec![]);
        config.steer = Some(steer);
        let seen = Arc::new(Mutex::new(Vec::<Vec<AgentMessage>>::new()));
        let capture_seen = seen.clone();
        config.stream_fn = Arc::new(SteerCaptureStream {
            inner: vec![StreamChunk::TextDelta("done".into()), StreamChunk::Done],
            seen: capture_seen,
        });
        let context = AgentContext {
            system_prompt: String::new(),
            messages: vec![],
            tools: vec![],
            cwd: std::path::PathBuf::from("/tmp"),
            env: std::collections::HashMap::new(),
            session_id: "t".into(),
        };
        let persisted = Arc::new(Mutex::new(Vec::<SessionEvent>::new()));
        let p = persisted.clone();
        config.persist = Some(Arc::new(move |ev: &SessionEvent| {
            p.lock().unwrap().push(ev.clone());
            Ok(())
        }));
        let _ = run_agent_loop(vec![], context, config, |_| {}).await;
        let calls = seen.lock().unwrap();
        let last = calls.last().expect("at least one LLM call");
        assert!(
            last.iter().any(|m| matches!(m, AgentMessage::User(u)
                if u.content.iter().any(|b| matches!(b, crate::ContentBlock::Text { text } if text == "use plan B")))),
            "steered text must reach the LLM input, got {last:?}"
        );
        let log = persisted.lock().unwrap();
        assert!(
            log.iter().any(|ev| matches!(ev, SessionEvent::User(m)
                if matches!(m, AgentMessage::User(u) if u.content.iter().any(|b| matches!(b, crate::ContentBlock::Text { text } if text == "use plan B"))))),
            "steered text must be persisted as a User event"
        );
    }

    /// Streams canned chunks once while recording the message list it was
    /// handed (the steer assertion inspects what the model would have seen).
    struct SteerCaptureStream {
        inner: Vec<StreamChunk>,
        seen: Arc<Mutex<Vec<Vec<AgentMessage>>>>,
    }

    impl StreamFn for SteerCaptureStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[crate::types::tool::ToolDefinition],
            _signal: Option<crate::CancelSignal>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            self.seen.lock().unwrap().push(messages.to_vec());
            Box::pin(stream::iter(self.inner.clone()))
        }
    }

    #[tokio::test]
    async fn loop_retries_transient_provider_error() {
        // First two attempts error before any content; third succeeds. With
        // max_retries=2 the run must recover and produce the success text.
        let mut config = test_config(vec![]);
        config.stream_fn = std::sync::Arc::new(FlakyStream {
            failures: 2,
            success: vec![StreamChunk::TextDelta("ok".into()), StreamChunk::Done],
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        config.retry = crate::RetryPolicy {
            max_retries: 2,
            initial_delay_ms: 1,
            max_delay_ms: 10,
            jitter: false,
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };
        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();
        let ok = msgs.iter().any(|m| {
            matches!(m, AgentMessage::Assistant(a)
                if a.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "ok")))
        });
        assert!(ok, "expected retry to recover and produce 'ok'");
    }

    #[tokio::test]
    async fn loop_does_not_retry_after_content_emitted() {
        // Stream emits text then errors mid-stream. Retry must NOT fire (would
        // duplicate emitted content); the error surfaces as stop_reason::Error.
        let mut config = test_config(vec![
            StreamChunk::TextDelta("partial".into()),
            StreamChunk::Error("mid-stream boom".into()),
        ]);
        config.retry = crate::RetryPolicy {
            max_retries: 3,
            initial_delay_ms: 1,
            max_delay_ms: 10,
            jitter: false,
        };
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };
        let mut text_deltas = 0u32;
        let msgs = run_agent_loop(vec![], context, config, |ev| {
            if matches!(
                ev,
                AgentEvent::MessageUpdate {
                    delta: ContentDelta::TextDelta(_)
                }
            ) {
                text_deltas += 1;
            }
        })
        .await
        .unwrap();
        let errored = msgs.iter().any(|m| {
            matches!(m, AgentMessage::Assistant(a)
                if matches!(a.stop_reason, StopReason::Error(_)))
        });
        assert_eq!(
            text_deltas, 1,
            "mid-stream error must not retry (would re-emit content)"
        );
        assert!(
            errored,
            "mid-stream error should surface as stop_reason::Error"
        );
    }

    #[tokio::test]
    async fn over_limit_tool_calls_reported_not_silent() {
        // max_tool_calls_per_turn = 1, but the model emits 2 calls. The second
        // must come back as an error tool_result (not a silent gap).
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        let mut config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t2".into(),
                name: Some("echo".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Done,
        ]);
        config.max_tool_calls_per_turn = 1;
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };
        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();
        // t2 must surface as an error tool_result mentioning the limit.
        let dropped = msgs.iter().any(|m| {
            matches!(m, AgentMessage::ToolResult(tr)
                if tr.is_error && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("tool call limit"))))
        });
        assert!(
            dropped,
            "over-limit call must be reported as an error, not dropped silently"
        );
    }

    #[tokio::test]
    async fn usage_merges_across_chunks() {
        // Two complementary Usage chunks (Anthropic shape: input then output).
        // Final usage must hold both, not just the last one.
        let config = test_config(vec![
            StreamChunk::Usage {
                input: 42,
                output: 0,
                cache_read: 100,
                cache_write: 50,
            },
            StreamChunk::Usage {
                input: 0,
                output: 7,
                cache_read: 0,
                cache_write: 0,
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };
        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();
        let merged = msgs.iter().any(|m| {
            matches!(m, AgentMessage::Assistant(a)
                if a.usage.as_ref().is_some_and(|u|
                    u.input_tokens == 42
                        && u.output_tokens == 7
                        && u.cache_read_tokens == Some(100)
                        && u.cache_write_tokens == Some(50)))
        });
        assert!(
            merged,
            "usage must merge input+output+cache fields across chunks"
        );
    }

    #[tokio::test]
    async fn usage_cache_zero_reports_none() {
        // Zero cache totals mean "provider didn't report cache stats";
        // the persisted Usage must keep None (absent = unknown) rather
        // than Some(0), matching old-log semantics.
        let config = test_config(vec![
            StreamChunk::Usage {
                input: 42,
                output: 7,
                cache_read: 0,
                cache_write: 0,
            },
            StreamChunk::Done,
        ]);
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };
        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();
        let none = msgs.iter().any(|m| {
            matches!(m, AgentMessage::Assistant(a)
                if a.usage.as_ref().is_some_and(|u|
                    u.cache_read_tokens.is_none() && u.cache_write_tokens.is_none()))
        });
        assert!(none, "unreported cache tokens must persist as None");
    }

    /// Test stream that must never be polled: the pre-set abort has to stop
    /// the loop before the provider is touched.
    struct PollCountingStream {
        polls: std::sync::atomic::AtomicUsize,
    }
    impl StreamFn for PollCountingStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system: &str,
            _tools: &[ToolDefinition],
            _signal: Option<crate::CancelSignal>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            let n = self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                n, 0,
                "provider stream must not be polled after a pre-set abort"
            );
            Box::pin(futures_util::stream::iter(vec![]))
        }
    }

    #[tokio::test]
    async fn pre_set_signal_aborts_before_provider_request() {
        let mut config = test_config(vec![]);
        let sig = crate::CancelSignal::new();
        sig.cancel();
        config.signal = Some(sig);
        config.stream_fn = Arc::new(PollCountingStream {
            polls: Default::default(),
        });
        let ctx = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: std::env::current_dir().unwrap(),
            env: std::collections::HashMap::new(),
            session_id: "test".into(),
        };
        let msgs = crate::agent_loop(vec![], ctx, config).await.unwrap();
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                AgentMessage::Assistant(a) if a.stop_reason == StopReason::Aborted
            )),
            "pre-set signal must produce an Aborted message"
        );
    }

    #[tokio::test]
    async fn persist_writes_assistant_before_tool_results() {
        // Scripted stream: text + tool_call + usage. The persist callback
        // must observe the assembled Assistant (with its tool_call block and
        // THIS step's usage) BEFORE any ToolResult - crash-safe ordering.
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo args".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|c: ToolCallCtx| {
                Box::pin(
                    async move { Ok(crate::types::tool::ToolResult::text(c.args.to_string())) },
                )
            }),
        };
        let mut config = test_config(vec![
            StreamChunk::TextDelta("checking".into()),
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{}".into(),
            },
            StreamChunk::Usage {
                input: 5,
                output: 3,
                cache_read: 0,
                cache_write: 0,
            },
            StreamChunk::Done,
        ]);
        let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        config.persist = Some(Arc::new(move |ev: &SessionEvent| {
            sink.lock().unwrap().push(ev.clone());
            Ok(())
        }));
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let recorded = events.lock().unwrap().clone();
        assert!(
            recorded.len() >= 2,
            "expected at least Assistant + ToolResult"
        );
        match &recorded[0] {
            SessionEvent::Assistant { message, usage } => {
                assert!(
                    matches!(message, AgentMessage::Assistant(a)
                        if a.content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }))),
                    "persisted Assistant must carry the tool_call block"
                );
                assert_eq!(
                    usage,
                    &Some(crate::types::message::Usage {
                        input_tokens: 5,
                        output_tokens: 3,
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                    }),
                    "persisted Assistant must carry this step's usage"
                );
            }
            other => panic!("expected Assistant first, got {other:?}"),
        }
        assert!(
            matches!(&recorded[1], SessionEvent::ToolResult(AgentMessage::ToolResult(tr))
                if tr.tool_name == "echo"),
            "ToolResult must be persisted after the Assistant"
        );
    }

    /// The persisted events reach the session log as JSON lines, in the
    /// crash-safe order: the assistant turn first, the tool result after it.
    #[tokio::test]
    async fn persisted_events_are_written_to_the_session_log_as_json() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");

        let mut config = test_config(vec![
            StreamChunk::TextDelta("checking".into()),
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("echo".into()),
                args_delta: "{\"x\":1}".into(),
            },
            StreamChunk::Usage {
                input: 5,
                output: 3,
                cache_read: 0,
                cache_write: 0,
            },
            StreamChunk::Done,
        ]);
        config.max_turns = 1;
        {
            let log = log.clone();
            config.persist = Some(Arc::new(move |ev: &SessionEvent| {
                let line = serde_json::to_string(ev).map_err(AgentError::Serde)?;
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log)
                    .map_err(AgentError::Io)?;
                use std::io::Write;
                writeln!(file, "{line}").map_err(AgentError::Io)?;
                Ok(())
            }));
        }

        let ctx = ctx_with_tools(vec![echo_tool()]);
        run_agent_loop(vec![], ctx, config, |_| {}).await.unwrap();

        let raw = std::fs::read_to_string(&log).expect("the session log must exist");
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("every log line is valid JSON"))
            .collect();
        assert_eq!(lines.len(), 2, "assistant + tool result: {lines:?}");

        // Line 1: the assembled assistant turn (with its usage).
        assert_eq!(lines[0]["type"], "assistant");
        assert_eq!(lines[0]["message"]["role"], "assistant");
        assert_eq!(lines[0]["usage"]["input_tokens"], 5);
        assert_eq!(lines[0]["usage"]["output_tokens"], 3);

        // Line 2: the tool result answering the call.
        assert_eq!(lines[1]["type"], "tool_result");
        assert_eq!(lines[1]["role"], "tool_result");
        assert_eq!(lines[1]["tool_call_id"], "t1");
        assert_eq!(lines[1]["tool_name"], "echo");
        assert_eq!(lines[1]["is_error"], false);
        assert_eq!(lines[1]["content"][0]["type"], "text");
        assert_eq!(lines[1]["content"][0]["text"], "{\"x\":1}");
    }

    #[tokio::test]
    async fn persist_error_aborts_run() {
        // The persist callback fails on its very first call: the run must
        // abort with that Err instead of silently continuing.
        let mut config = test_config(vec![StreamChunk::TextDelta("hi".into()), StreamChunk::Done]);
        config.persist = Some(Arc::new(|_ev: &SessionEvent| {
            Err(AgentError::Io(std::io::Error::other("disk full")))
        }));
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        let result = run_agent_loop(vec![], context, config, |_| {}).await;
        assert!(result.is_err(), "persist error must abort the run");
    }

    #[tokio::test]
    async fn blocked_tool_call_still_persists_result() {
        // A before_tool_call Block still produces an is_error ToolResult,
        // and that refusal is persisted like any other fact.
        let mut api = crate::extension::ExtensionApiImpl::new();
        api.register_before_tool_call(Box::new(BlockBash));
        let bash = crate::ToolDefinition {
            name: "bash".into(),
            label: "Bash".into(),
            description: "shell".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: ToolCallCtx| {
                Box::pin(async move { Ok(crate::ToolResult::text("ran")) })
            }),
        };
        let mut config = test_config(vec![
            StreamChunk::ToolCallDelta {
                index: None,
                id: "t1".into(),
                name: Some("bash".into()),
                args_delta: "{\"command\":\"rm -rf /\"}".into(),
            },
            StreamChunk::Done,
        ]);
        config.hooks = Some(std::sync::Arc::new(api));
        let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        config.persist = Some(Arc::new(move |ev: &SessionEvent| {
            sink.lock().unwrap().push(ev.clone());
            Ok(())
        }));
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![bash],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s".into(),
        };

        run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        let recorded = events.lock().unwrap().clone();
        assert!(
            recorded.len() >= 2,
            "expected Assistant + blocked ToolResult on disk"
        );
        assert!(
            matches!(&recorded[1], SessionEvent::ToolResult(AgentMessage::ToolResult(tr))
                if tr.tool_name == "bash" && tr.is_error
                && tr.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "blocked by policy"))),
            "a blocked call must still persist its is_error ToolResult"
        );
    }

    #[test]
    fn tool_state_dir_rejects_traversal_session_ids() {
        // A session id that could escape the state root (`../`, separators,
        // empty) must yield NO directory, never one outside the root.
        let mk = |id: &str| AgentContext {
            system_prompt: String::new(),
            messages: vec![],
            tools: vec![],
            cwd: ".".into(),
            env: Default::default(),
            session_id: id.into(),
        };
        for bad in ["../escape", "a/b", "..", "", "a\\b", "."] {
            assert!(
                tool_state_dir(&mk(bad), "t").is_none(),
                "session id {bad:?} must be rejected"
            );
        }
        // A valid id stays under <config_dir>/sessions/<sid>/tool_state.
        let dir = tool_state_dir(&mk("s-1_ok"), "t").unwrap();
        assert_eq!(
            dir,
            crate::storage::config_dir()
                .join("sessions")
                .join("s-1_ok")
                .join("tool_state")
                .join("t")
        );
    }

    /// A mock that replays a different chunk script per LLM call (turn 1
    /// truncates, turn 2 recovers), sharing a call counter (`StreamFn` is
    /// `&self`).
    struct ScriptedStream {
        scripts: Vec<Vec<StreamChunk>>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl StreamFn for ScriptedStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[AgentMessage],
            _system_prompt: &str,
            _tools: &[crate::types::tool::ToolDefinition],
            _signal: Option<crate::CancelSignal>,
        ) -> Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.scripts.get(n) {
                Some(script) => Box::pin(stream::iter(script.clone())),
                None => Box::pin(stream::iter(vec![StreamChunk::Done])),
            }
        }
    }

    #[tokio::test]
    async fn max_tokens_truncation_skips_tool_execution() {
        // OpenAI `finish_reason: "length"` shape: a tool call cut off
        // mid-arguments (malformed JSON) plus the Stop chunk. Without the
        // stop signal the loop would misclassify this as ToolUse and feed
        // the model a bogus malformed-args error; with it the turn ends as
        // MaxTokens and every partial call is discarded unread.
        let executed: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let exec_sink = executed.clone();
        let echo = crate::types::tool::ToolDefinition {
            name: "echo".into(),
            label: "Echo".into(),
            description: "echo args".into(),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(move |c: ToolCallCtx| {
                let sink = exec_sink.clone();
                Box::pin(async move {
                    sink.lock().push(c.args.to_string());
                    Ok(crate::types::tool::ToolResult::text("ran"))
                })
            }),
        };
        let mut config = test_config(vec![]);
        config.stream_fn = Arc::new(ScriptedStream {
            scripts: vec![
                vec![
                    StreamChunk::ToolCallDelta {
                        index: Some(0),
                        id: "t1".into(),
                        name: Some("echo".into()),
                        args_delta: "{\"x\":".into(), // truncated mid-JSON
                    },
                    StreamChunk::Stop(StopReason::MaxTokens),
                    StreamChunk::Done,
                ],
                vec![
                    StreamChunk::TextDelta("recovered".into()),
                    StreamChunk::Done,
                ],
            ],
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        config.max_turns = 2;
        let context = AgentContext {
            system_prompt: "sys".into(),
            messages: vec![],
            tools: vec![echo],
            cwd: ".".into(),
            env: Default::default(),
            session_id: "s-trunc".into(),
        };

        let msgs = run_agent_loop(vec![], context, config, |_| {})
            .await
            .unwrap();

        // The truncated assistant message classifies as MaxTokens...
        let truncated = msgs.iter().find_map(|m| match m {
            AgentMessage::Assistant(a) if a.stop_reason == StopReason::MaxTokens => Some(a.clone()),
            _ => None,
        });
        let truncated = truncated.expect("an assistant message with stop_reason MaxTokens");
        assert!(
            truncated
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { .. })),
            "fixture streams a partial tool call"
        );
        // ...and the tool NEVER executes (no malformed-args execution path).
        assert!(
            executed.lock().is_empty(),
            "truncated tool call must not execute"
        );
        // Instead every partial call gets an is_error discard result.
        let discarded: Vec<_> = msgs
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolResult(tr) if tr.tool_name == "echo" => Some(tr.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(discarded.len(), 1);
        assert!(discarded[0].is_error);
        assert!(
            discarded[0].content.iter().any(|b| matches!(b,
                ContentBlock::Text { text } if text.contains("truncated"))),
            "discard result explains the truncation"
        );
        // And the loop continues: the next turn's plain text ends the run.
        assert!(
            msgs.iter().any(|m| matches!(m, AgentMessage::Assistant(a) if a
                .content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "recovered")))),
            "loop recovers on the next turn"
        );
    }
}
