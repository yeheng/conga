//! `AgentContext` / `AgentLoopConfig` — what the agent sees and how it runs.

use std::collections::HashMap;
use std::time::Duration;

use std::path::PathBuf;
use std::sync::Arc;

use crate::cancel::CancelSignal;
use crate::error::AgentError;
use crate::types::message::{AgentMessage, ModelId, StopReason};
use crate::types::session_event::SessionEvent;
use crate::types::tool::ToolDefinition;

/// Everything the agent sees for one run. Deliberately has **no plugin-shared
/// state field** — plugin-private state lives in files under `ToolContext.state_dir`.
#[derive(Clone)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<crate::types::message::AgentMessage>,
    pub tools: Vec<ToolDefinition>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub session_id: String,
}

impl std::fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentContext")
            .field("system_prompt", &self.system_prompt)
            .field("messages_len", &self.messages.len())
            .field("tools_len", &self.tools.len())
            .field("cwd", &self.cwd)
            .field("session_id", &self.session_id)
            .finish()
    }
}

/// How a single agent loop invocation is configured.
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: ModelSpec,
    /// Hard ceiling on outer-loop turns. Default 50.
    pub max_turns: usize,
    /// Hard ceiling on tool calls executed within a single turn. Default 20.
    pub max_tool_calls_per_turn: usize,
    /// Engine-level safety net for one tool call: a tool whose Future never
    /// resolves (wedged MCP server, deadlocked plugin) is cut off and
    /// reported as an error tool_result instead of hanging the whole loop.
    /// `None` (the default) imposes no engine limit — tools with their own
    /// timeouts (bash, fetch, MCP, external) keep governing themselves, and
    /// a tool may legitimately run longer than any fixed default. Hosts that
    /// want the net set it via `CONGA_TOOL_TIMEOUT_S`.
    pub tool_timeout: Option<Duration>,
    /// Cooperative abort: cancels the loop at the next safe point. Async
    /// waiters (SSE download, approval waits) are woken the instant
    /// [`CancelSignal::cancel`](crate::CancelSignal::cancel) fires - no polling.
    pub signal: Option<CancelSignal>,
    /// The LLM call entry point. Injected by the host so the loop is
    /// provider-agnostic and testable with a mock.
    pub stream_fn: Arc<dyn StreamFn>,
    /// Optional hook chain consulted around each tool call (block / modify /
    /// redact). `None` = no hooks (default — used by tests and `agent_loop`).
    pub hooks: Option<Arc<dyn crate::types::tool::HookChain>>,
    /// Retry policy for LLM calls that fail before any content is streamed
    /// (connection errors, non-2xx). Mid-stream failures are surfaced, not
    /// retried, to avoid duplicating partial output already emitted.
    pub retry: RetryPolicy,
    /// Optional persistence callback: called with each `SessionEvent` as it
    /// is produced, in crash-safe order (Assistant BEFORE any tool in it
    /// executes; each ToolResult after any hook rewriting). `None` = no
    /// persistence (bare `agent_loop` and tests). A persist `Err` aborts the
    /// run (fail loud - storage failures are never silently swallowed).
    #[allow(clippy::type_complexity)]
    pub persist: Option<Arc<dyn Fn(&SessionEvent) -> Result<(), AgentError> + Send + Sync>>,
    /// Mid-turn user input: transports push text onto this queue while the
    /// loop runs; the loop drains it at the top of each turn iteration and
    /// appends each item as a real `User` message (persisted like any
    /// other). `None` = no steering (bare `agent_loop` and tests).
    pub steer: Option<crate::steer::SteerQueue>,
    /// Optional transform applied to the message list before EVERY LLM
    /// call — the seam host compaction (or redaction, auditing) hooks
    /// into. Pure wire view: the loop's accumulator, returned messages,
    /// and persisted events always keep the full uncompacted history.
    /// `Err` fails the run loud (never silently swallowed).
    #[allow(clippy::type_complexity)]
    pub transform_context:
        Option<Arc<dyn Fn(&[AgentMessage]) -> Result<Vec<AgentMessage>, AgentError> + Send + Sync>>,
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopConfig")
            .field("model", &self.model.id)
            .field("max_turns", &self.max_turns)
            .field("max_tool_calls_per_turn", &self.max_tool_calls_per_turn)
            .finish_non_exhaustive()
    }
}

/// Retry policy for a single LLM call. Backoff is exponential:
/// `initial_delay_ms * 2^(attempt-1)`, capped at `max_delay_ms`. When
/// `jitter` is on, a bounded pseudo-random offset (± delay/4) is applied so
/// concurrent workers retried by the same clock don't thunder into the
/// provider in lockstep.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Max retries after the initial attempt (0 = no retry).
    pub max_retries: usize,
    /// Delay before the first retry, in milliseconds.
    pub initial_delay_ms: u64,
    /// Upper bound on backoff delay, in milliseconds.
    pub max_delay_ms: u64,
    /// Apply bounded jitter to every backoff (default true).
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_delay_ms: 500,
            max_delay_ms: 8_000,
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// No retries - for tests that want deterministic, fast failure.
    pub fn off() -> Self {
        Self {
            max_retries: 0,
            initial_delay_ms: 0,
            max_delay_ms: 0,
            jitter: false,
        }
    }
}

/// Loop tunables loadable from the environment (all optional, with defaults).
///
/// Recognized env vars:
/// - `CONGA_MAX_TURNS`        - outer-loop turn ceiling (default 50)
/// - `CONGA_MAX_TOOL_CALLS`   - per-turn tool-call ceiling (default 20)
/// - `CONGA_MAX_TOKENS`       - model output token cap (default 4096)
/// - `CONGA_RETRY_MAX`        - max LLM-call retries (default 2)
/// - `CONGA_RETRY_INITIAL_MS` - first retry backoff ms (default 500)
/// - `CONGA_TOOL_TIMEOUT_S`   - engine-level per-call tool timeout in
///   seconds (default: none)
///
/// Malformed values fall back to the default silently.
#[derive(Debug, Clone)]
pub struct AgentTunables {
    pub max_turns: usize,
    pub max_tool_calls_per_turn: usize,
    pub max_tokens: usize,
    pub retry: RetryPolicy,
    /// Engine-level tool timeout (see `AgentLoopConfig::tool_timeout`);
    pub tool_timeout_secs: Option<u64>,
}

impl AgentTunables {
    /// Read tunables from the process environment.
    pub fn from_env() -> Self {
        Self::from_env_with(&|k: &str| std::env::var(k))
    }

    /// Same as [`from_env`](Self::from_env) but with an injectable lookup -
    /// used by tests to avoid mutating process env.
    pub fn from_env_with(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Self {
        let default_retry = RetryPolicy::default();
        Self {
            max_turns: env_parse(lookup, "CONGA_MAX_TURNS", 50),
            max_tool_calls_per_turn: env_parse(lookup, "CONGA_MAX_TOOL_CALLS", 20),
            max_tokens: env_parse(lookup, "CONGA_MAX_TOKENS", 4096),
            tool_timeout_secs: lookup("CONGA_TOOL_TIMEOUT_S")
                .ok()
                .and_then(|s| s.parse().ok()),
            retry: RetryPolicy {
                max_retries: env_parse(lookup, "CONGA_RETRY_MAX", default_retry.max_retries),
                initial_delay_ms: env_parse(
                    lookup,
                    "CONGA_RETRY_INITIAL_MS",
                    default_retry.initial_delay_ms,
                ),
                max_delay_ms: env_parse(lookup, "CONGA_RETRY_MAX_MS", default_retry.max_delay_ms),
                jitter: default_retry.jitter,
            },
        }
    }
}

/// Parse `key` from the lookup as `T`, falling back to `default` on miss or
/// parse failure.
fn env_parse<T: std::str::FromStr>(
    lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    key: &str,
    default: T,
) -> T {
    lookup(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Specification of a model the host wants to talk to.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub id: ModelId,
    pub api: ProviderApi,
    pub max_tokens: usize,
}

/// Which wire protocol the provider speaks. Determines `convert_to_llm` shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderApi {
    /// OpenAI / DeepSeek / 智谱 / xAI / Groq / Ollama / vLLM / etc.
    OpenAiCompat,
    /// Anthropic native messages API.
    Anthropic,
}

/// A provider-agnostic stream chunk produced by `StreamFn`.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamChunk {
    TextDelta(String),
    ToolCallDelta {
        /// OpenAI-compat stream key for parallel tool calls
        /// (`tool_calls[].index`): first appearance opens a call,
        /// continuations repeat the index without id/name. `None` when the
        /// provider keys by id (Anthropic) or omits the field.
        index: Option<u32>,
        id: String,
        name: Option<String>,
        args_delta: String,
    },
    ThinkingDelta(String),
    Usage {
        input: u64,
        output: u64,
        /// Prompt tokens served from the provider cache; 0 = not reported.
        cache_read: u64,
        /// Prompt tokens written into the provider cache; 0 = not reported.
        cache_write: u64,
    },
    /// Provider-reported stop signal (OpenAI `finish_reason`, Anthropic
    /// `message_delta.stop_reason`), already mapped to the internal
    /// [`StopReason`]. Emitted at most once per stream, before `Done`.
    /// When present it overrides the loop's content-based stop guess, so a
    /// length-truncated response classifies as `MaxTokens` instead of a
    /// bogus tool call with malformed arguments.
    Stop(StopReason),
    Done,
    Error(String),
}

/// The LLM call entry point, injected into `AgentLoopConfig`.
///
/// Returns a boxed async iterator of [`StreamChunk`]. Providers implement this;
/// tests inject a mock that yields a canned chunk sequence.
pub trait StreamFn: Send + Sync {
    fn stream(
        &self,
        model: &ModelSpec,
        messages: &[crate::types::message::AgentMessage],
        system_prompt: &str,
        tools: &[ToolDefinition],
        signal: Option<CancelSignal>,
    ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::fake_env;

    #[test]
    fn tunables_default_when_unset() {
        let t = AgentTunables::from_env_with(&fake_env(&[]));
        assert_eq!(t.max_turns, 50);
        assert_eq!(t.max_tool_calls_per_turn, 20);
        assert_eq!(t.max_tokens, 4096);
        assert_eq!(t.retry.max_retries, 2);
    }

    #[test]
    fn tunables_parse_values() {
        let t = AgentTunables::from_env_with(&fake_env(&[
            ("CONGA_MAX_TURNS", "5"),
            ("CONGA_MAX_TOOL_CALLS", "8"),
            ("CONGA_MAX_TOKENS", "123"),
            ("CONGA_RETRY_MAX", "3"),
            ("CONGA_RETRY_INITIAL_MS", "100"),
            ("CONGA_RETRY_MAX_MS", "2000"),
        ]));
        assert_eq!(t.max_turns, 5);
        assert_eq!(t.max_tool_calls_per_turn, 8);
        assert_eq!(t.max_tokens, 123);
        assert_eq!(t.retry.max_retries, 3);
        assert_eq!(t.retry.initial_delay_ms, 100);
        assert_eq!(t.retry.max_delay_ms, 2000);
    }

    #[test]
    fn tunables_malformed_falls_back() {
        let t = AgentTunables::from_env_with(&fake_env(&[("CONGA_MAX_TURNS", "not-a-number")]));
        assert_eq!(t.max_turns, 50);
    }

    // ── RetryPolicy ───────────────────────────────────────────────

    #[test]
    fn retry_policy_default_values() {
        let r = RetryPolicy::default();
        assert_eq!(r.max_retries, 2);
        assert_eq!(r.initial_delay_ms, 500);
        assert_eq!(r.max_delay_ms, 8_000);
        assert!(r.jitter);
    }

    #[test]
    fn retry_policy_off_disables_retries_and_jitter() {
        let r = RetryPolicy::off();
        assert_eq!(r.max_retries, 0);
        assert_eq!(r.initial_delay_ms, 0);
        assert_eq!(r.max_delay_ms, 0);
        assert!(!r.jitter);
    }

    #[test]
    fn retry_policy_off_never_retries() {
        // `off` is the deterministic-failure policy for tests: the retry
        // budget must be exactly zero, not merely "small".
        assert_eq!(RetryPolicy::off().max_retries, 0);
        assert_ne!(
            RetryPolicy::off().max_retries,
            RetryPolicy::default().max_retries
        );
    }

    // ── env_parse ─────────────────────────────────────────────────

    #[test]
    fn env_parse_reads_a_numeric_value() {
        let v = env_parse(&fake_env(&[("K", "42")]), "K", 0usize);
        assert_eq!(v, 42usize);
    }

    #[test]
    fn env_parse_accepts_zero() {
        // "0" is a legal value, not a missing one.
        let v = env_parse(&fake_env(&[("K", "0")]), "K", 7usize);
        assert_eq!(v, 0usize);
    }

    #[test]
    fn env_parse_falls_back_when_the_var_is_missing() {
        let v = env_parse(&fake_env(&[]), "K", 7usize);
        assert_eq!(v, 7usize);
    }

    #[test]
    fn env_parse_falls_back_on_a_malformed_value() {
        let v = env_parse(&fake_env(&[("K", "twelve")]), "K", 7usize);
        assert_eq!(v, 7usize);
    }

    #[test]
    fn env_parse_falls_back_on_a_negative_value_for_unsigned() {
        let v = env_parse(&fake_env(&[("K", "-1")]), "K", 7u64);
        assert_eq!(v, 7u64);
    }

    // ── AgentTunables::from_env ───────────────────────────────────

    #[test]
    fn tunables_from_env_agrees_with_the_injected_lookup() {
        // `from_env` must be a thin wrapper over the process lookup — same
        // values, whether or not the machine has the vars set.
        let direct = AgentTunables::from_env();
        let injected = AgentTunables::from_env_with(&|k: &str| std::env::var(k));
        assert_eq!(direct.max_turns, injected.max_turns);
        assert_eq!(
            direct.max_tool_calls_per_turn,
            injected.max_tool_calls_per_turn
        );
        assert_eq!(direct.max_tokens, injected.max_tokens);
        assert_eq!(direct.retry.max_retries, injected.retry.max_retries);
        assert_eq!(direct.tool_timeout_secs, injected.tool_timeout_secs);
    }

    #[test]
    fn tunables_tool_timeout_is_absent_by_default() {
        let t = AgentTunables::from_env_with(&fake_env(&[]));
        assert_eq!(t.tool_timeout_secs, None);
    }

    #[test]
    fn tunables_tool_timeout_parses_seconds() {
        let t = AgentTunables::from_env_with(&fake_env(&[("CONGA_TOOL_TIMEOUT_S", "30")]));
        assert_eq!(t.tool_timeout_secs, Some(30));
    }

    #[test]
    fn tunables_tool_timeout_ignores_a_malformed_value() {
        let t = AgentTunables::from_env_with(&fake_env(&[("CONGA_TOOL_TIMEOUT_S", "soon")]));
        assert_eq!(t.tool_timeout_secs, None);
    }

    /// Boundary: `0` parses as a REAL zero-second timeout, not as "disabled".
    /// `agent_loop` feeds that straight into `tokio::time::timeout`, so a
    /// zero-length deadline cuts off every tool call that does not complete
    /// synchronously ("tool execution timed out after 0s"). The way to
    /// disable the engine-level timeout is to leave the variable unset.
    #[test]
    fn tunables_tool_timeout_of_zero_parses_as_zero() {
        let t = AgentTunables::from_env_with(&fake_env(&[("CONGA_TOOL_TIMEOUT_S", "0")]));
        assert_eq!(t.tool_timeout_secs, Some(0));
    }

    // ── Debug formatting ──────────────────────────────────────────

    #[test]
    fn agent_context_debug_summarizes_without_dumping_content() {
        let ctx = AgentContext {
            system_prompt: "be brief".to_string(),
            messages: vec![crate::types::message::AgentMessage::user("hello there")],
            tools: Vec::new(),
            cwd: PathBuf::from("/tmp/work"),
            env: HashMap::from([("K".to_string(), "V".to_string())]),
            session_id: "s1".to_string(),
        };
        let out = format!("{ctx:?}");
        assert!(out.contains("AgentContext"), "{out}");
        assert!(out.contains("be brief"), "{out}");
        assert!(out.contains("messages_len"), "{out}");
        assert!(out.contains("1"), "{out}");
        assert!(out.contains("tools_len"), "{out}");
        assert!(out.contains("/tmp/work"), "{out}");
        assert!(out.contains("s1"), "{out}");
    }

    #[test]
    fn agent_loop_config_debug_summarizes_the_knobs() {
        let cfg = AgentLoopConfig {
            model: ModelSpec {
                id: "claude-sonnet".to_string(),
                api: ProviderApi::Anthropic,
                max_tokens: 4096,
            },
            max_turns: 7,
            max_tool_calls_per_turn: 3,
            tool_timeout: None,
            signal: None,
            stream_fn: std::sync::Arc::new(NullStream),
            hooks: None,
            retry: RetryPolicy::off(),
            persist: None,
            steer: None,
            transform_context: None,
        };
        let out = format!("{cfg:?}");
        assert!(out.contains("AgentLoopConfig"), "{out}");
        assert!(out.contains("claude-sonnet"), "{out}");
        assert!(out.contains("7"), "{out}");
        assert!(out.contains("3"), "{out}");
        // Non-exhaustive: the sensitive/derived fields stay out of the log.
        assert!(!out.contains("persist"), "{out}");
    }

    // ── StreamFn ──────────────────────────────────────────────────

    /// A `StreamFn` that yields a canned chunk sequence.
    struct NullStream;

    impl StreamFn for NullStream {
        fn stream(
            &self,
            _model: &ModelSpec,
            _messages: &[crate::types::message::AgentMessage],
            _system_prompt: &str,
            _tools: &[ToolDefinition],
            _signal: Option<CancelSignal>,
        ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = StreamChunk> + Send>> {
            let chunks = vec![StreamChunk::TextDelta("hi".to_string()), StreamChunk::Done];
            Box::pin(futures_util::stream::iter(chunks))
        }
    }

    #[tokio::test]
    async fn stream_fn_yields_the_provider_chunks() {
        use futures_util::StreamExt;
        let fn_impl = NullStream;
        let model = ModelSpec {
            id: "m".to_string(),
            api: ProviderApi::OpenAiCompat,
            max_tokens: 16,
        };
        let mut stream = fn_impl.stream(&model, &[], "", &[], None);
        assert_eq!(
            stream.next().await,
            Some(StreamChunk::TextDelta("hi".to_string()))
        );
        assert_eq!(stream.next().await, Some(StreamChunk::Done));
        assert_eq!(stream.next().await, None);
    }
}
