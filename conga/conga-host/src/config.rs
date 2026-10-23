//! ConfigLoader: 从 env/.env 聚合 ProviderConfig + AgentTunables。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use conga::{
    AgentContext, AgentError, AgentLoopConfig, AgentMessage, AgentTunables, CancelSignal,
    HookChain, ProviderConfig, SessionEvent, StreamFn, ToolDefinition,
};

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub provider: ProviderConfig,
    pub tunables: AgentTunables,
}

pub struct ConfigLoader;

impl ConfigLoader {
    pub fn load() -> Result<HostConfig, crate::HostError> {
        let _ = dotenvy::dotenv(); // best-effort；env 已有的值优先
        Self::load_with(&|k: &str| std::env::var(k))
    }

    pub fn load_with(
        lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    ) -> Result<HostConfig, crate::HostError> {
        let provider = ProviderConfig::from_env_with(lookup)?;
        let tunables = AgentTunables::from_env_with(lookup);
        Ok(HostConfig { provider, tunables })
    }
}
/// The agent loop's persist callback — same shape as
/// `AgentLoopConfig::persist`, named so host signatures stay readable.
pub type PersistFn = Arc<dyn Fn(&SessionEvent) -> Result<(), AgentError> + Send + Sync>;

impl HostConfig {
    /// The provider's native `stream_fn`. `Host::new` fills its stream_fn slot
    /// from this; power users (or tests) can take it directly.
    pub fn provider_stream_fn(&self) -> Arc<dyn StreamFn> {
        match self.provider.api {
            conga::ProviderApi::OpenAiCompat => {
                Arc::new(conga::OpenAiCompat::from_config(&self.provider))
            }
            conga::ProviderApi::Anthropic => {
                Arc::new(conga::AnthropicProvider::from_config(&self.provider))
            }
        }
    }

    /// Assemble the `ModelSpec` + tunables into an `AgentLoopConfig`.
    /// `stream_fn` is injected explicitly: hosts pass their own field (the
    /// provider's, or a test fake). `max_turns` is caller-supplied (smoke
    /// tests cap it at 3; the REPL uses the tunables default). `signal` and
    /// `hooks` are host-specific.
    pub fn build_loop_config(
        &self,
        max_turns: usize,
        signal: Option<CancelSignal>,
        hooks: Option<Arc<dyn HookChain>>,
        stream_fn: Arc<dyn StreamFn>,
    ) -> AgentLoopConfig {
        AgentLoopConfig {
            model: conga::ModelSpec {
                id: self.provider.model.clone(),
                api: self.provider.api,
                max_tokens: self.tunables.max_tokens,
            },
            max_turns,
            max_tool_calls_per_turn: self.tunables.max_tool_calls_per_turn,
            tool_timeout: self.tunables.tool_timeout_secs.map(Duration::from_secs),
            signal,
            stream_fn,
            hooks,
            retry: self.tunables.retry.clone(),
            persist: None,
            steer: None,
            transform_context: None,
        }
    }

    /// Assemble an `AgentContext` for one run. History is cloned into the
    /// context (the loop consumes it by value); callers keep their own copy
    /// to extend with the run's new messages afterwards.
    pub fn build_context(
        &self,
        system_prompt: &str,
        history: &[AgentMessage],
        tools: Vec<ToolDefinition>,
        cwd: PathBuf,
        env: HashMap<String, String>,
        session_id: &str,
    ) -> AgentContext {
        AgentContext {
            system_prompt: system_prompt.to_string(),
            messages: history.to_vec(),
            tools,
            cwd,
            env,
            session_id: session_id.to_string(),
        }
    }

    /// The single canonical "build one turn" step, shared by every host
    /// (`Host::run_turn` for the CLI, the gateway's per-connection driver).
    /// Resets the abort signal, then builds the context + loop config from
    /// the provider/tunables. `inputs.history` is the log-derived working
    /// copy — already past any turn-start compaction checkpoint (`Host::
    /// run_turn` appends a `Compacted` event when over budget) — and the
    /// event log on disk remains the single source of truth. Returns owned
    /// values so the caller can run the loop
    /// however it likes - inline (CLI) or in a spawned task with
    /// event-channel forwarding (gateway). `persist`, when set, is handed to
    /// the loop so every Assistant/ToolResult lands on disk as it happens;
    /// the host frames the turn with its own TurnStart/User/TurnEnd writes.
    pub fn prepare_turn(
        &self,
        inputs: TurnInputs<'_>,
        signal: &CancelSignal,
        hooks: Arc<dyn HookChain>,
        stream_fn: Arc<dyn StreamFn>,
        max_turns: usize,
        persist: Option<PersistFn>,
    ) -> (AgentContext, AgentLoopConfig) {
        // A Ctrl-C from a previous turn must not leak into this one. reset()
        // also wakes any approval-wait still parked on the old cancel.
        signal.reset();
        let context = self.build_context(
            inputs.system_prompt,
            inputs.history,
            inputs.tools.to_vec(),
            inputs.cwd.to_path_buf(),
            std::env::vars().collect(),
            inputs.session_id,
        );
        let mut config =
            self.build_loop_config(max_turns, Some(signal.clone()), Some(hooks), stream_fn);
        config.persist = persist;
        (context, config)
    }
}

/// Borrowed inputs for one agent turn. Grouped so
/// [`HostConfig::prepare_turn`] stays readable instead of taking six
/// positional args. `history` is the projection of the session event log
/// (optionally compacted in memory) — callers no longer own a growing
/// transcript themselves.
pub struct TurnInputs<'a> {
    pub system_prompt: &'a str,
    pub history: &'a [AgentMessage],
    pub tools: &'a [ToolDefinition],
    pub cwd: &'a Path,
    pub session_id: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    fn test_cfg(pairs: &[(&str, &str)]) -> HostConfig {
        let mut base = vec![
            ("CONGA_LLM_BASE_URL", "https://api.x.com/v1"),
            ("CONGA_LLM_KEY", "sk-test"),
            ("CONGA_LLM_MODEL", "m"),
        ];
        base.extend_from_slice(pairs);
        ConfigLoader::load_with(&fake_env(&base)).unwrap()
    }

    #[test]
    fn loads_provider_and_tunables() {
        let cfg = test_cfg(&[("CONGA_MAX_TURNS", "7"), ("CONGA_LLM_API", "anthropic")]);
        assert_eq!(cfg.provider.model, "m");
        assert_eq!(cfg.provider.api, conga::ProviderApi::Anthropic);
        assert_eq!(cfg.tunables.max_turns, 7);
    }

    #[test]
    fn missing_llm_config_errors() {
        let r = ConfigLoader::load_with(&fake_env(&[]));
        assert!(r.is_err());
    }

    #[test]
    fn build_loop_config_wires_provider_and_tunables() {
        let cfg = test_cfg(&[("CONGA_MAX_TURNS", "7"), ("CONGA_LLM_API", "anthropic")]);
        // max_turns is the caller's arg (3), NOT the tunables value (7).
        let lc = cfg.build_loop_config(3, None, None, cfg.provider_stream_fn());
        assert_eq!(lc.model.id, "m");
        assert_eq!(lc.model.api, conga::ProviderApi::Anthropic);
        assert_eq!(lc.max_turns, 3);
        assert_eq!(lc.retry.max_retries, 2);
    }

    #[test]
    fn provider_stream_fn_matches_api() {
        let cfg = test_cfg(&[]);
        // Only the type matters here: OpenAI-compat config must yield a
        // stream_fn that does not panic on construction.
        let _ = cfg.provider_stream_fn();
    }

    #[test]
    fn load_reads_the_process_env_and_fails_loud_without_a_provider() {
        // `load()` reads the real process env (no .env file in the repo),
        // so the LLM knobs are absent -> a ConfigError, never a silent
        // default provider.
        match ConfigLoader::load() {
            Ok(cfg) => {
                // A developer machine with CONGA_LLM_* exported still works.
                assert!(!cfg.provider.model.is_empty());
                assert!(!cfg.provider.base_url.is_empty());
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("CONGA_LLM"),
                    "the missing knob must be named: {msg}"
                );
            }
        }
    }

    #[test]
    fn prepare_turn_resets_the_signal_and_wires_the_context() {
        let cfg = test_cfg(&[]);
        let signal = CancelSignal::new();
        signal.cancel();
        assert!(
            signal.is_cancelled(),
            "precondition: the signal is cancelled"
        );

        let history = vec![AgentMessage::User(conga::UserMessage {
            content: vec![conga::ContentBlock::text("hi".to_string())],
            timestamp: 1,
        })];
        let tools = vec![crate::tools::built_in_tools().into_iter().next().unwrap()];
        let (context, config) = cfg.prepare_turn(
            TurnInputs {
                system_prompt: "sys",
                history: &history,
                tools: &tools,
                cwd: Path::new("/tmp"),
                session_id: "s1",
            },
            &signal,
            Arc::new(crate::HookStack::new(Vec::new())),
            cfg.provider_stream_fn(),
            5,
            None,
        );

        // A Ctrl-C from the previous turn must not leak into this one.
        assert!(!signal.is_cancelled(), "prepare_turn must reset the signal");
        assert_eq!(context.system_prompt, "sys");
        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.tools.len(), 1);
        assert_eq!(context.cwd, PathBuf::from("/tmp"));
        assert_eq!(context.session_id, "s1");
        assert!(!context.env.is_empty(), "the env snapshot is captured");
        // The loop config carries the caller's knobs.
        assert_eq!(config.max_turns, 5);
        assert!(config.signal.is_some());
        assert!(config.hooks.is_some());
        assert!(config.persist.is_none());
    }

    #[test]
    fn prepare_turn_installs_the_persist_callback() {
        let cfg = test_cfg(&[]);
        let signal = CancelSignal::new();
        let persisted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&persisted);
        let persist: PersistFn = Arc::new(move |_ev: &SessionEvent| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let (_, config) = cfg.prepare_turn(
            TurnInputs {
                system_prompt: "sys",
                history: &[],
                tools: &[],
                cwd: Path::new("/tmp"),
                session_id: "s1",
            },
            &signal,
            Arc::new(crate::HookStack::new(Vec::new())),
            cfg.provider_stream_fn(),
            1,
            Some(persist),
        );
        assert!(config.persist.is_some());
        let persist = config.persist.unwrap();
        persist(&SessionEvent::TurnStart).unwrap();
        assert_eq!(persisted.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn build_context_maps_fields() {
        let cfg = test_cfg(&[]);
        let history = vec![AgentMessage::User(conga::UserMessage {
            content: vec![conga::ContentBlock::text("hi".to_string())],
            timestamp: 1,
        })];
        let ctx = cfg.build_context(
            "sys",
            &history,
            vec![],
            PathBuf::from("/tmp"),
            HashMap::from([("K".to_string(), "V".to_string())]),
            "s1",
        );
        assert_eq!(ctx.system_prompt, "sys");
        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.cwd, PathBuf::from("/tmp"));
        assert_eq!(ctx.env.get("K").map(String::as_str), Some("V"));
        assert_eq!(ctx.session_id, "s1");
    }
}
