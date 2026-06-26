//! Session configuration — AgentConfig with kernel conversion support.

use crate::kernel::KernelConfig;

/// Default model for agent
pub const DEFAULT_MODEL: &str = "gpt-4o";
/// Default maximum iterations for agent loop
pub const DEFAULT_MAX_ITERATIONS: u32 = 100;
/// Default temperature for generation
pub const DEFAULT_TEMPERATURE: f32 = 1.0;
/// Default maximum tokens for generation
pub const DEFAULT_MAX_TOKENS: u32 = 100_000;
/// Default memory window size
pub const DEFAULT_MEMORY_WINDOW: usize = 50;
/// Default maximum characters for tool result output
pub const DEFAULT_MAX_TOOL_RESULT_CHARS: usize = 16000;
/// Default maximum retries for transient provider errors
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// Default tool execution timeout in seconds (2 minutes)
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
/// Default subagent execution timeout in seconds (10 minutes)
pub const DEFAULT_SUBAGENT_TIMEOUT_SECS: u64 = 600;
/// Default session idle timeout in seconds (1 hour)
pub const DEFAULT_SESSION_IDLE_TIMEOUT_SECS: u64 = 3600;
/// Default wait timeout for subagent results in seconds (12 minutes)
pub const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 720;
/// Default cooldown after a failed compaction LLM call
pub const DEFAULT_COMPACTION_COOLDOWN_SECS: u64 = 60;
/// Default timeout for after-response hooks
pub const DEFAULT_AFTER_RESPONSE_HOOK_TIMEOUT_SECS: u64 = 30;
/// Default timeout for external shell hooks
pub const DEFAULT_EXTERNAL_HOOK_TIMEOUT_SECS: u64 = 2;
/// Default concurrency for parallel tool execution
pub const DEFAULT_TOOL_CONCURRENCY: usize = 5;

/// Configuration for the self-evolution hook.
#[derive(Clone, Debug)]
pub struct EvolutionConfig {
    /// Whether the evolution hook is enabled.
    pub enabled: bool,
    /// Number of messages to accumulate before triggering reflection.
    pub batch_messages: usize,
    /// Maximum number of concurrent evolution tasks (default: 3).
    pub concurrency: usize,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            batch_messages: 20,
            concurrency: 3,
        }
    }
}

/// Prompt templates — re-export from config layer (single source of truth).
pub use crate::config::app_config::PromptsConfig;

/// Agent loop configuration.
///
/// **Layer model (per Linus review):** these fields fall into three lifetimes.
/// They are kept flat here for backward compatibility with existing call
/// sites, but the comment groups indicate the lifetime each field belongs to.
/// New code should prefer `to_kernel_config()` and the session/infra accessors
/// over reaching into individual fields, so the layering stays meaningful.
///
/// 1. **Per-execution (kernel-layer)** — re-resolved on every LLM turn. These
///    are funneled into `KernelConfig` via [`AgentConfigExt::to_kernel_config`].
///    `tool_filter` is intentionally NOT here: it varies per inbound request
///    and is threaded as a parameter through `handle_inbound`, then set on
///    the runtime context's `KernelConfig.tool_filter`.
///
/// 2. **Per-session (session-layer)** — locked in when the session is built;
///    changing them mid-session has no effect.
///
/// 3. **Infrastructure (constructor-time)** — wired once at startup; treated
///    as immutable for the lifetime of the process.
#[derive(Clone)]
pub struct AgentConfig {
    // ── Layer 1: per-execution (kernel-layer) ─────────────────────────────
    pub model: String,
    pub max_iterations: u32,
    pub temperature: f32,
    pub max_tokens: u32,
    /// Maximum characters for tool result output (0 = unlimited)
    pub max_tool_result_chars: usize,
    /// Maximum retries for transient provider errors
    pub max_retries: u32,
    /// Enable thinking/reasoning mode for deep reasoning models
    pub thinking_enabled: bool,
    /// Tool execution timeout in seconds
    pub tool_timeout_secs: u64,
    /// External-tool execution timeout in seconds (fallback when manifest omits it)
    pub plugin_timeout_secs: u64,
    /// Maximum characters for WebSocket subagent summary (0 = unlimited).
    pub ws_summary_limit: usize,

    // ── Layer 2: per-session (session-layer) ──────────────────────────────
    pub memory_window: usize,
    /// Enable streaming mode for progressive output
    pub streaming: bool,
    /// Subagent execution timeout in seconds
    pub subagent_timeout_secs: u64,
    /// Session idle timeout in seconds
    pub session_idle_timeout_secs: u64,
    /// Cooldown after a failed compaction LLM call (default: 60s).
    pub compaction_cooldown_secs: u64,

    // ── Layer 3: infrastructure (constructor-time) ────────────────────────
    /// Timeout for after-response hooks in seconds (default: 30s).
    pub after_response_hook_timeout_secs: u64,
    /// Timeout for external shell hooks in seconds (default: 2s).
    pub external_hook_timeout_secs: u64,
    /// Prompt configuration for internal AI behaviors.
    pub prompts: PromptsConfig,
    /// Self-evolution configuration (auto-learning from conversations).
    pub evolution: Option<EvolutionConfig>,
    /// Optional path to an external stop-words file for keyword-based history recall.
    pub stop_words_path: Option<std::path::PathBuf>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            // Layer 1
            model: DEFAULT_MODEL.to_string(),
            max_iterations: DEFAULT_MAX_ITERATIONS,
            temperature: DEFAULT_TEMPERATURE,
            max_tokens: DEFAULT_MAX_TOKENS,
            max_tool_result_chars: DEFAULT_MAX_TOOL_RESULT_CHARS,
            max_retries: DEFAULT_MAX_RETRIES,
            thinking_enabled: false,
            tool_timeout_secs: DEFAULT_TOOL_TIMEOUT_SECS,
            plugin_timeout_secs: DEFAULT_TOOL_TIMEOUT_SECS,
            ws_summary_limit: 0,
            // Layer 2
            memory_window: DEFAULT_MEMORY_WINDOW,
            streaming: true,
            subagent_timeout_secs: DEFAULT_SUBAGENT_TIMEOUT_SECS,
            session_idle_timeout_secs: DEFAULT_SESSION_IDLE_TIMEOUT_SECS,
            compaction_cooldown_secs: DEFAULT_COMPACTION_COOLDOWN_SECS,
            // Layer 3
            after_response_hook_timeout_secs: DEFAULT_AFTER_RESPONSE_HOOK_TIMEOUT_SECS,
            external_hook_timeout_secs: DEFAULT_EXTERNAL_HOOK_TIMEOUT_SECS,
            prompts: PromptsConfig::default(),
            evolution: Some(EvolutionConfig::default()),
            stop_words_path: None,
        }
    }
}

/// Extension trait to convert AgentConfig → KernelConfig.
pub trait AgentConfigExt {
    fn to_kernel_config(&self) -> KernelConfig;
}

impl AgentConfigExt for AgentConfig {
    fn to_kernel_config(&self) -> KernelConfig {
        KernelConfig {
            model: self.model.clone(),
            max_iterations: self.max_iterations,
            max_retries: self.max_retries,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            max_tool_result_chars: self.max_tool_result_chars,
            thinking_enabled: self.thinking_enabled,
            tool_timeout_secs: self.tool_timeout_secs,
            plugin_timeout_secs: self.plugin_timeout_secs,
            ws_summary_limit: self.ws_summary_limit,
            // tool_filter is per-request data, not per-config.
            // `session::process_direct_streaming_with_channel` overwrites this
            // field on every turn with the inbound message's tool_filter.
            tool_filter: None,
        }
    }
}
