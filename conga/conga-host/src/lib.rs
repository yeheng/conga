//! conga-host - 可复用的 host 层（配置/session/权限/事件渲染/压缩/外部工具）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use conga::{
    derive_messages, repair_unanswered_tool_calls, AgentError, AgentEvent, AgentMessage,
    CancelCause, CancelSignal, ContentBlock, SessionEvent, StopReason, StreamFn, ToolDefinition,
    TurnEndReason, UserMessage,
};

pub mod approval;
pub mod assembly;
pub mod compact;
pub mod config;
pub mod event_map;
pub mod evolve;
pub mod external_tool;
#[cfg(feature = "ext-js")]
pub mod ext_js;
pub mod hooks;
pub mod mcp;
pub mod memory;
pub mod permission;
pub mod preview;
pub mod printer;
pub mod process_hooks;
pub mod prompt;
pub mod proxy;
pub mod session;
pub mod session_api;
pub mod session_cleanup;
#[cfg(feature = "session-index")]
pub mod session_index;
pub mod settings;
pub mod skills;
pub mod subagent;
pub mod subagent_types;
pub mod tools;
pub mod wire;

pub use assembly::{gather_tools, resume_session, ApprovalEmit, SessionAssembly, SubagentEmit};
pub use compact::{
    compact_by_count, max_messages_from_env, Compacted, ContextBudget, DEFAULT_MAX_MESSAGES,
};
pub use config::{ConfigLoader, HostConfig, TurnInputs};
pub use conga::RiskLevel;
pub use external_tool::{
    commands_from_env, load_all as load_external_tools, ExternalCommand, ExternalToolBridge,
};
pub use hooks::HookStack;
pub use mcp::{load_all_mcp, McpBridge, McpError, McpServerConfig};
pub use memory::append_memory;
pub use permission::{Mode, PermissionPolicy};
pub use printer::EventPrinter;
pub use process_hooks::ProcessHookChain;
pub use prompt::{append_project_doc, env_snapshot, CODING_AGENT_PROMPT};
pub use proxy::{apply_tool_proxy, set_tool_proxy, tool_proxy, validate_tool_proxy};
pub use session::{SessionInfo, SessionManager};
#[cfg(feature = "session-index")]
pub use session_api::search_sessions;
pub use session_api::{
    delete_session, list_sessions, rename_session, session_cache_stats, session_messages,
    SessionApiError, SessionListItem,
};
pub use session_cleanup::{cleanup_session_resources, register_hook, run_hooks};
pub use skills::append_skills;
pub use subagent::HostSubagentSpawner;
pub use subagent_types::{
    NoopSubagentSpawner, SubagentEvent, SubagentResult, SubagentSpawn, SubagentSpawner,
};
pub use tools::built_in_tools;

/// Project directory: the sandbox root for tool paths and the base for
/// `<dir>/.conga/skills` project skills. `CONGA_PROJECT_DIR` overrides the
/// process cwd — servers (gateway, desktop app) don't run inside the project
/// they serve, so they need an explicit knob; the CLI leaves it unset.
pub fn project_dir() -> PathBuf {
    project_dir_with(std::env::var("CONGA_PROJECT_DIR").ok().as_deref())
}

/// Testable core of [`project_dir`]; the override comes from env in
/// production.
fn project_dir_with(override_dir: Option<&str>) -> PathBuf {
    match override_dir.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("config error: {0}")]
    Config(#[from] conga::ConfigError),
    #[error("session error: {0}")]
    Session(String),
    #[error("agent error: {0}")]
    Agent(#[from] conga::AgentError),
}

/// Assemble config/session/policy/loop into one driver. Hosts hold one
/// `Host` and call [`run_turn`](Host::run_turn) per user message; CLI,
/// smoke tests, and future frontends (a2a, channels) reuse the same path.
///
pub struct Host {
    cfg: HostConfig,
    session: SessionManager,
    policy: Arc<PermissionPolicy>,
    hooks: Arc<dyn conga::HookChain>,
    stream_fn: Arc<dyn StreamFn>,
    /// Set by [`with_stream_fn`](Self::with_stream_fn): an explicitly
    /// injected provider (tests, custom wiring) is exempt from the
    /// per-turn settings override in [`resolve_turn_provider`].
    stream_fn_overridden: bool,
    signal: CancelSignal,
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    /// Wired subagent spawner (set by [`with_spawner`](Self::with_spawner)).
    /// Extraction for [`evolve`](Self::evolve) runs on it; `None` on hosts
    /// that never wired one.
    spawner: Option<Arc<dyn crate::subagent_types::SubagentSpawner>>,
    cwd: PathBuf,
    max_turns: usize,
    /// Compaction knobs. The token count itself is NOT kept here: every turn
    /// re-derives history from the log and restores the last persisted
    /// assistant usage from its tail, so token-aware compaction survives
    /// restarts by construction.
    budget: ContextBudget,
    /// Mid-turn user input queue: transports push here while a turn runs;
    /// `run_turn` hands it to the loop, which injects each item as a real
    /// User message at the next safe point.
    steer: conga::SteerQueue,
    /// Turn serialization slot: run_turn acquires it on entry and releases
    /// it on completion or drop. The event log's format contract assumes a
    /// single writer per session, and one Host drives one session — so a
    /// second concurrent turn is rejected outright instead of interleaving
    /// two event streams into one log.
    turn_in_flight: AtomicBool,
}

impl Host {
    /// Build a host from a loaded config. `stream_fn` starts as the
    /// provider's own; tests override it with [`with_stream_fn`](Self::with_stream_fn).
    ///
    /// `policy` is the shared instance: it is the default hook chain AND what
    /// [`policy`](Self::policy) exposes, so hosts that compose extra gates on
    /// top (via [`with_hooks`](Self::with_hooks)) still mutate the live policy
    /// through the accessor (e.g. the REPL's `/mode`).
    pub fn new(
        cfg: HostConfig,
        session: SessionManager,
        policy: Arc<PermissionPolicy>,
        system_prompt: String,
        tools: Vec<ToolDefinition>,
    ) -> Self {
        let hooks: Arc<dyn conga::HookChain> = Arc::new(HookStack::new(vec![policy.clone()]));
        // Tool sandbox + project skills follow the project dir (env
        // override for servers); unset = process cwd, so the CLI is
        // unchanged. `.` fallback keeps tools resolving to the same place.
        let cwd = project_dir();
        Self {
            cwd,
            signal: CancelSignal::new(),
            stream_fn: cfg.provider_stream_fn(),
            stream_fn_overridden: false,
            max_turns: cfg.tunables.max_turns,
            budget: ContextBudget::from_env(),
            steer: conga::SteerQueue::new(),
            cfg,
            turn_in_flight: AtomicBool::new(false),
            session,
            policy,
            hooks,
            system_prompt,
            tools,
            spawner: None,
        }
    }

    /// The shared mid-turn input queue. Transports clone this handle and
    /// push user text while a turn runs; the loop injects it as User
    /// messages before the next LLM call.
    pub fn steer(&self) -> conga::SteerQueue {
        self.steer.clone()
    }

    /// Replace the provider stream_fn with a fake (tests) or custom one.
    /// An explicitly injected provider is exempt from the per-turn
    /// settings-file override (see [`resolve_turn_provider`]).
    pub fn with_stream_fn(mut self, stream_fn: Arc<dyn StreamFn>) -> Self {
        self.stream_fn = stream_fn;
        self.stream_fn_overridden = true;
        self
    }

    /// Resolve this turn's config + stream_fn: the web UI's settings file
    /// (`~/.conga/settings.json`, see [`crate::settings`]) OVERRIDES the
    /// env-derived config, re-read every turn so a UI save reaches the
    /// very next LLM call. Hosts with an injected stream_fn keep theirs.
    pub fn resolve_turn_provider(
        &self,
        settings: &crate::settings::EnvSettings,
    ) -> (HostConfig, Arc<dyn StreamFn>) {
        if self.stream_fn_overridden {
            return (self.cfg.clone(), self.stream_fn.clone());
        }
        match crate::settings::effective_provider(settings) {
            Some(pc) => {
                let cfg = HostConfig {
                    provider: pc,
                    tunables: self.cfg.tunables.clone(),
                };
                let stream_fn = cfg.provider_stream_fn();
                (cfg, stream_fn)
            }
            None => (self.cfg.clone(), self.stream_fn.clone()),
        }
    }

    /// Override the compaction budget (tests inject knobs without touching
    /// the process environment).
    pub fn with_budget(mut self, budget: ContextBudget) -> Self {
        self.budget = budget;
        self
    }

    /// Override the per-turn turn ceiling (default: the tunables value).
    pub fn with_max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }

    /// Replace the hook chain (default: the permission policy alone). Hosts
    /// that stack extension gates before the policy pass the composed stack.
    pub fn with_hooks(mut self, hooks: Arc<dyn conga::HookChain>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Wire a subagent spawner into this host's `spawn_subagents` tool,
    /// replacing the spawner-less default from [`built_in_tools`]. The
    /// spawner lives in the tool's execute closure, so it is per-Host:
    /// concurrent hosts (one per gateway connection) never see each
    /// other's spawner. Hosts that excluded the tool from their list keep
    /// it excluded — sub-agent tool sets filter `spawn_subagents` out, so
    /// nesting stays disabled. The spawner is also stored on the Host
    /// itself: [`evolve`](Self::evolve) extraction runs on it. Without
    /// this, the tool reports subagents as unavailable and `evolve`
    /// fails; CLI/gateway pass a `HostSubagentSpawner` built from the
    /// host's config. The `evolve` tool gets its live handle
    /// (session + policy + spawner) here too.
    pub fn with_spawner(
        mut self,
        spawner: Arc<dyn crate::subagent_types::SubagentSpawner>,
    ) -> Self {
        self.spawner = Some(Arc::clone(&spawner));
        self.rewire_spawner_tools();
        self
    }

    /// Re-apply the live spawner wiring to the CURRENT tool list.
    /// [`built_in_tools`](crate::tools::built_in_tools) registers both
    /// `spawn_subagents` and `evolve` spawner-less, so the wiring must be
    /// re-applied every time the list is replaced — not just from
    /// [`with_spawner`](Self::with_spawner), but also after
    /// [`set_tools`](Self::set_tools): `/reload-tools` rebuilds the list
    /// from the spawner-less defaults, and without this re-application
    /// both tools answer "unavailable" until restart. Returns the names
    /// actually swapped, so tests can pin the wiring by name without
    /// executing any tool.
    fn rewire_spawner_tools(&mut self) -> Vec<&'static str> {
        let mut swapped = Vec::new();
        let Some(spawner) = self.spawner.clone() else {
            return swapped;
        };
        if let Some(t) = self.tools.iter_mut().find(|t| t.name == "spawn_subagents") {
            *t = tools::subagent::tool(Some(Arc::clone(&spawner)));
            swapped.push("spawn_subagents");
        }
        // evolve needs the same spawner plus this host's policy/session —
        // both already on the Host, so wire the handle here too.
        if let Some(t) = self.tools.iter_mut().find(|t| t.name == "evolve") {
            *t = tools::evolve_tool::tool(Some(tools::evolve_tool::EvolveHandle {
                session: self.session.clone(),
                policy: Arc::clone(&self.policy),
                spawner: Some(spawner),
            }));
            swapped.push("evolve");
        }
        swapped
    }

    /// The wired subagent spawner (extraction runs on it). `None` on hosts
    /// that never called [`with_spawner`](Self::with_spawner) (evolve
    /// reports unavailable).
    pub fn spawner(&self) -> Option<&Arc<dyn crate::subagent_types::SubagentSpawner>> {
        self.spawner.as_ref()
    }

    /// Self-evolution: distill a session into approved memory/skills.
    /// `None` = the current session. See `docs/evolve.md`.
    pub async fn evolve(
        &self,
        session_id: Option<&str>,
    ) -> Result<evolve::EvolveOutcome, AgentError> {
        let config = conga::storage::config_dir();
        evolve::run_evolve(
            &self.session,
            &self.policy,
            self.spawner.as_ref(),
            session_id,
            &config.join("memory"),
            &config.join("skills"),
            &self.cwd,
        )
        .await
    }

    /// Replace the tool list (used by `/reload-tools`). The fresh list is
    /// rebuilt from spawner-less defaults, so the live wiring is re-applied
    /// right after the swap (`rewire_spawner_tools`) — a reload must not
    /// strip the `spawn_subagents`/`evolve` handles until restart.
    pub fn set_tools(&mut self, tools: Vec<ToolDefinition>) {
        self.tools = tools;
        self.rewire_spawner_tools();
    }

    pub fn session(&self) -> &SessionManager {
        &self.session
    }

    /// Clear the conversation — the unified `/clear` semantics for every
    /// transport (CLI, gateway, desktop): append a `SessionEvent::Cleared`
    /// fact to the CURRENT session's log. The session id does NOT rotate;
    /// [`derive_messages`](conga::derive_messages) projects away the
    /// pre-clear prefix on the next turn, while the log on disk stays
    /// append-only (the pre-clear rows remain searchable/history). Fail
    /// loud: when the marker cannot be persisted the caller must tell the
    /// user the clear did NOT take.
    pub async fn clear_session(&self) -> Result<(), AgentError> {
        self.session.mark_cleared().await
    }

    pub fn session_mut(&mut self) -> &mut SessionManager {
        &mut self.session
    }

    pub fn policy(&self) -> &PermissionPolicy {
        &self.policy
    }

    /// The shared cancel signal. `install_ctrl_c` wants a clone; readers
    /// poll [`is_cancelled`](conga::CancelSignal::is_cancelled) cheaply,
    /// async waiters use [`cancelled`](conga::CancelSignal::cancelled).
    pub fn signal(&self) -> &CancelSignal {
        &self.signal
    }

    /// One user message through the whole event-sourced pipeline.
    ///
    /// The log is the only truth: history is *derived* from
    /// `events.jsonl` (never carried by the caller), the turn is framed by
    /// `TurnStart`/`User`/`TurnEnd` writes, and the agent loop persists every
    /// Assistant/ToolResult as it happens via the injected persist closure —
    /// so a crash or failed turn keeps every side effect that already
    /// happened, instead of rolling back to a pre-turn transcript.
    /// Flow: persist(TurnStart) → restore budget from the log tail →
    /// history = derive_messages(load_events) → repair dangling tool calls →
    /// [over budget: persist ONE `Compacted` checkpoint, restart history
    /// from it] → persist(User, with the per-turn environment snapshot
    /// embedded — send-what-you-persist keeps the provider prompt-cache
    /// prefix byte-stable) → prepare_turn → run_agent_loop →
    /// persist(TurnEnd{reason}) → [`TurnSummary`].
    ///
    /// `on_event` receives every [`AgentEvent`](conga::AgentEvent)
    /// as it happens (streaming text, tool calls, usage, errors).
    pub async fn run_turn<E>(
        &self,
        user_msg: &str,
        mut on_event: E,
    ) -> Result<TurnSummary, AgentError>
    where
        E: FnMut(AgentEvent) + Send,
    {
        // Turns are serialized per Host (single-writer event log). The
        // guard is a Drop release: a dropped/cancelled turn still frees the
        // host, so a cancelled connection cannot deadlock the next one.
        if self.turn_in_flight.swap(true, Ordering::AcqRel) {
            return Err(AgentError::TurnInProgress);
        }
        let _turn_guard = TurnGuard(&self.turn_in_flight);

        let sid = self.session.current_id();
        // Open (migrating a legacy transcript once, if any), fail closed on
        // corruption, then frame the turn.
        let events = self.session.open_or_migrate(&sid).await?;
        self.session.append_event(&SessionEvent::TurnStart).await?;

        // Restore the token budget from the log tail (the last persisted
        // assistant usage) — scoped to the post-checkpoint slice (past the
        // last Cleared/Compacted): a cleared conversation is empty and a
        // compacted one is only as big as its checkpoint, so a
        // pre-checkpoint usage snapshot would over-estimate and trigger
        // compaction against history that no longer exists. No usage after
        // the checkpoint (e.g. the turn right after one) simply means
        // occupancy is unknown-yet-small; the next provider report sets it
        // straight — that is the hysteresis.
        let live = conga::live_range_start(&events);
        let mut budget = self.budget.clone();
        if let Some(input_tokens) = events[live..].iter().rev().find_map(|ev| match ev {
            SessionEvent::Assistant { usage, .. } => usage.as_ref().map(|u| u.input_tokens),
            _ => None,
        }) {
            budget.record_input_tokens(input_tokens);
        }
        let mut history = derive_messages(&events);
        // The log keeps partial facts by design (abort/crash mid-batch);
        // the provider protocol does not tolerate them. Synthesize error
        // results for tool calls that never got one before feeding the loop.
        repair_unanswered_tool_calls(&mut history);

        // Per-turn settings: re-read the file every turn so a UI save
        // reaches the very next LLM call - both the provider AND the
        // custom base prompt (a half-applied switch would be worse than
        // none). Injected stream_fn hosts keep their wired prompt.
        let settings = if self.stream_fn_overridden {
            crate::settings::EnvSettings::default()
        } else {
            crate::settings::load_settings_async().await
        };
        // A saved `maxTokens` overrides the env-derived context window on
        // the very next turn — the budget is per-turn state cloned above,
        // so this never needs a restart. Absent = keep the env window.
        // Applied BEFORE the compaction decision so the override reaches
        // this turn's checkpoint, not the next one's.
        if let Some(n) = settings.max_tokens {
            budget.set_window(n);
        }

        // Over budget: append ONE `Compacted` checkpoint and restart the
        // projection from it. The compacted view is persisted — an append,
        // never a rewrite — so every later turn replays the same prefix
        // bytes. This replaces the old per-request `transform_context`
        // wire view, which oscillated between the compacted view and the
        // full history (compact one turn, borrow back the next) and missed
        // the provider prompt cache on every one of those turns.
        if let Compacted::Owned(view) = budget.compact(&history) {
            // view = [pinned task, notice, kept...]; history = [pinned,
            // dropped..., kept...] → dropped = history.len() - view.len() + 1
            // (the notice occupies one slot).
            let dropped = history.len() - view.len() + 1;
            self.session
                .append_event(&SessionEvent::Compacted {
                    base: view.clone(),
                    dropped,
                })
                .await?;
            history = view;
        }

        // Per-turn environment block: git status / diffstat drift as the
        // session progresses. Built fresh each turn (async git subprocess
        // calls are capped and timeout-guarded inside `env_snapshot`) and
        // embedded in the PERSISTED user message — the request tail is
        // exactly what the log replays. The system prompt stays
        // byte-stable across turns, so the provider's cached prefix
        // (tools + system + history) survives; volatile env rides in the
        // request tail instead of poisoning the prefix from the top.
        let snapshot = crate::prompt::env_snapshot(&self.cwd).await;
        let mut user_content = if snapshot.is_empty() {
            user_msg.to_string()
        } else {
            format!("{user_msg}\n\n<environment>\n{snapshot}\n</environment>")
        };
        // Plan-mode directive rides the same persisted tail (volatile mode
        // state must not touch the byte-stable system prompt / cache
        // prefix). The policy gate below stays the law; this is the hint.
        if self.policy.mode() == Mode::Plan {
            user_content = format!("{user_content}\n\n{}", crate::prompt::PLAN_MODE_DIRECTIVE);
        }
        let user = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(user_content)],
            timestamp: conga::now(),
        });
        // Appended AFTER any Compacted checkpoint so the projection reads
        // [checkpoint base, user, ...] — the current task always survives
        // compaction.
        self.session
            .append_event(&SessionEvent::User(user.clone()))
            .await?;

        let base_prompt = compose_turn_prompt(
            &self.system_prompt,
            settings.system_prompt.as_deref(),
            &self.cwd,
        );
        // Per-turn LLM settings: the web UI persists env overrides to
        // ~/.conga/settings.json; re-resolve the provider EVERY turn so a
        // UI save reaches the very next LLM call (model id + stream_fn
        // together - a half-applied switch would be worse than none).
        let (turn_cfg, turn_stream_fn) = self.resolve_turn_provider(&settings);
        let (context, mut config) = turn_cfg.prepare_turn(
            TurnInputs {
                system_prompt: &base_prompt,
                history: &history,
                tools: &self.tools,
                cwd: &self.cwd,
                session_id: &sid,
            },
            self.signal(),
            self.hooks.clone(),
            turn_stream_fn,
            self.max_turns,
            Some(self.session.persist_fn()),
        );
        config.steer = Some(self.steer.clone());

        let outcome = conga::run_agent_loop(vec![user], context, config, |ev| {
            on_event(ev);
        })
        .await;

        // Ok + no signal = Completed; Ok + signal = Aborted; Err = Error.
        // A surfaced provider error (stop_reason::Error on the closing
        // assistant) counts as an errored turn.
        let reason = match &outcome {
            Err(e) => TurnEndReason::Error {
                message: e.to_string(),
            },
            Ok(msgs) => {
                if self.signal().is_cancelled() {
                    TurnEndReason::Aborted {
                        cause: Some(CancelCause::User),
                    }
                } else if let Some(AgentMessage::Assistant(a)) = msgs
                    .iter()
                    .rev()
                    .find(|m| matches!(m, AgentMessage::Assistant(_)))
                {
                    match &a.stop_reason {
                        StopReason::Error(message) => TurnEndReason::Error {
                            message: message.clone(),
                        },
                        _ => TurnEndReason::Completed,
                    }
                } else {
                    TurnEndReason::Completed
                }
            }
        };
        // The TurnEnd marker is best-effort: appending it must never shadow
        // the loop's own outcome. If the write fails (disk full, permission,
        // …) we log and carry on, returning the loop's result/error as-is —
        // derive_messages tolerates a missing trailing TurnEnd, so the next
        // open still reconstructs a coherent history.
        if let Err(e) = self
            .session
            .append_event(&SessionEvent::TurnEnd {
                reason: reason.clone(),
            })
            .await
        {
            tracing::warn!(error = %e, "failed to append TurnEnd marker; loop outcome preserved");
        }

        outcome.map(|new_messages| TurnSummary {
            reason,
            new_messages,
        })
    }
}

/// Per-turn prompt composition: custom base prompt (settings) first, then
/// the skills and memory catalogs. Both catalogs are deterministic rescans
/// — identical files produce identical bytes, so the provider cache prefix
/// survives every turn; only a real library change (evolve run, manual
/// edit) legitimately busts it. Kept as a named fn so the seam is testable
/// without a full Host.
pub(crate) fn compose_turn_prompt(base: &str, custom: Option<&str>, cwd: &Path) -> String {
    let p = crate::prompt::with_custom_base_prompt(base, custom);
    crate::append_memory(&crate::append_skills(&p, cwd))
}

/// Releases the per-Host turn slot when the turn future completes *or* is
/// dropped (ws close, cancellation): run_turn is cancel-safe, so a dropped
/// turn must not leave the host permanently busy.
struct TurnGuard<'a>(&'a AtomicBool);

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// What one `run_turn` produced. The event log remains the source of truth;
/// `new_messages` is the loop's returned slice for UI/stats convenience.
#[derive(Debug)]
pub struct TurnSummary {
    pub reason: TurnEndReason,
    pub new_messages: Vec<AgentMessage>,
}

/// Install a SIGINT handler that cancels `signal` (cooperative abort).
/// Every press is honored; `Host::run_turn` resets the signal at the start
/// of each turn (see [`HostConfig::prepare_turn`]).
pub fn install_ctrl_c(signal: CancelSignal) {
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break; // handler could not be installed; give up quietly
            }
            signal.cancel();
        }
    });
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

    fn test_cfg() -> HostConfig {
        ConfigLoader::load_with(&fake_env(&[
            ("CONGA_LLM_BASE_URL", "https://api.x.com/v1"),
            ("CONGA_LLM_KEY", "sk-test"),
            ("CONGA_LLM_MODEL", "m"),
        ]))
        .unwrap()
    }

    #[test]
    fn compose_turn_prompt_appends_skill_catalog_after_custom() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let skills = cwd.join(".conga").join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(
            skills.join("demo.md"),
            "---\nname: demo\ndescription: does things\n---\nBody\n",
        )
        .unwrap();
        let out = compose_turn_prompt("BUILT-IN", Some("custom persona"), cwd);
        assert!(out.starts_with("custom persona"));
        assert!(out.contains("## Skills"));
        assert!(out.contains("demo"));
        // Memory catalog from the real config dir must not break composition
        // and must ride last.
        if let Some(idx_mem) = out.find("## Memory") {
            let idx_skills = out.find("## Skills").unwrap();
            assert!(idx_mem > idx_skills);
        }
    }

    #[test]
    fn project_dir_env_override_beats_process_cwd() {
        assert_eq!(
            project_dir_with(Some("/some/project")),
            PathBuf::from("/some/project")
        );
        // Blank/empty override must not win over the cwd fallback.
        assert_eq!(
            project_dir_with(Some("   ")),
            std::env::current_dir().unwrap()
        );
        assert_eq!(project_dir_with(None), std::env::current_dir().unwrap());
    }

    #[test]
    fn host_construction_and_accessors() {
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::with_root(tmp.path().to_path_buf());
        let mut host = Host::new(
            test_cfg(),
            session,
            Arc::new(PermissionPolicy::new(
                Mode::FullAuto,
                Arc::new(|_, _| Box::pin(async { false })),
            )),
            "sys".into(),
            vec![],
        );
        assert!(!host.session().current_id().is_empty());
        assert!(!host.signal().is_cancelled());

        host.set_tools(vec![]); // compile check: setter exists
        let _ = host
            .with_stream_fn(Arc::new(FakeProvider))
            .with_max_turns(1);
    }

    /// `/reload-tools` rebuilds the tool list from spawner-less defaults;
    /// the live wiring must be re-applied on every swap or
    /// `spawn_subagents` and `evolve` answer "unavailable" until restart.
    /// Asserts on the swapped NAMES only — executing the tools would hit
    /// real config_dir roots.
    #[test]
    fn spawner_tool_wiring_reapplied_on_every_list_swap() {
        use crate::subagent_types::NoopSubagentSpawner;
        let tmp = tempfile::tempdir().unwrap();
        let mut host = Host::new(
            test_cfg(),
            SessionManager::with_root(tmp.path().to_path_buf()),
            Arc::new(PermissionPolicy::new(
                Mode::FullAuto,
                Arc::new(|_, _| Box::pin(async { false })),
            )),
            "sys".into(),
            crate::tools::built_in_tools(),
        );
        // No spawner wired: nothing to rewire even with both names present.
        assert!(host.rewire_spawner_tools().is_empty());

        // Wired spawner + both names in the list: both swap (and the
        // helper is idempotent on re-runs).
        let mut host = host.with_spawner(Arc::new(NoopSubagentSpawner));
        assert_eq!(host.rewire_spawner_tools(), ["spawn_subagents", "evolve"]);
        assert_eq!(host.rewire_spawner_tools(), ["spawn_subagents", "evolve"]);

        // A list without `evolve` (sub-agent tool sets filter it out):
        // only spawn_subagents rewires; the exclusion is preserved.
        host.set_tools(
            crate::tools::built_in_tools()
                .into_iter()
                .filter(|t| t.name != "evolve")
                .collect(),
        );
        assert_eq!(host.rewire_spawner_tools(), ["spawn_subagents"]);
    }

    /// The settings file OVERRIDES the env-derived provider for hosts that
    /// did NOT inject their own stream_fn; injected providers are exempt
    /// (a fake would otherwise be swapped for a real one mid-test).
    #[test]
    fn resolve_turn_provider_settings_override_and_exemption() {
        use crate::settings::{EnvSettings, LlmGroup};
        let make_host = |overridden: bool| {
            let tmp = tempfile::tempdir().unwrap();
            let host = Host::new(
                test_cfg(),
                SessionManager::with_root(tmp.path().to_path_buf()),
                Arc::new(PermissionPolicy::new(
                    Mode::FullAuto,
                    Arc::new(|_, _| Box::pin(async { true })),
                )),
                "sys".into(),
                vec![],
            );
            if overridden {
                host.with_stream_fn(Arc::new(FakeProvider))
            } else {
                host
            }
        };
        let settings = EnvSettings {
            llm: Some(LlmGroup {
                base_url: "https://settings.example/v1".into(),
                api_key: "sk-settings".into(),
                model: "settings-model".into(),
                api: "openai".into(),
            }),
            fast_llm: None,
            system_prompt: None,
            max_tokens: None,
        };
        // No injection → settings win (provider + stream_fn swapped).
        let (cfg, _) = make_host(false).resolve_turn_provider(&settings);
        assert_eq!(cfg.provider.base_url, "https://settings.example/v1");
        assert_eq!(cfg.provider.model, "settings-model");

        // Injected provider → settings ignored, config unchanged.
        let plain = make_host(true);
        let base_model = plain.cfg.provider.model.clone();
        let (cfg, _) = plain.resolve_turn_provider(&settings);
        assert_eq!(cfg.provider.model, base_model);

        // Empty settings → env config kept either way.
        let (cfg, _) = make_host(false).resolve_turn_provider(&EnvSettings::default());
        assert_eq!(cfg.provider.model, base_model);
    }

    #[test]
    fn host_error_from_agent_error() {
        let e: HostError = conga::AgentError::ToolNotFound("x".into()).into();
        assert!(e.to_string().contains("x"));
    }

    /// Minimal StreamFn for construction tests (never called).
    struct FakeProvider;
    impl StreamFn for FakeProvider {
        fn stream(
            &self,
            _model: &conga::ModelSpec,
            _messages: &[AgentMessage],
            _system: &str,
            _tools: &[ToolDefinition],
            _signal: Option<conga::CancelSignal>,
        ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = conga::StreamChunk> + Send>>
        {
            unreachable!("not called in construction tests")
        }
    }

    /// StreamFn that never yields: keeps a turn in flight on demand.
    struct PendingProvider;
    impl StreamFn for PendingProvider {
        fn stream(
            &self,
            _model: &conga::ModelSpec,
            _messages: &[AgentMessage],
            _system: &str,
            _tools: &[ToolDefinition],
            _signal: Option<conga::CancelSignal>,
        ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = conga::StreamChunk> + Send>>
        {
            Box::pin(futures_util::stream::pending())
        }
    }

    fn test_host() -> Host {
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::with_root(tmp.path().to_path_buf());
        Host::new(
            test_cfg(),
            session,
            Arc::new(PermissionPolicy::new(
                Mode::FullAuto,
                Arc::new(|_, _| Box::pin(async { true })),
            )),
            "sys".into(),
            vec![],
        )
        .with_stream_fn(Arc::new(PendingProvider))
    }

    /// StreamFn that emits one text delta then a stream error. Because content
    /// was already emitted, the loop does not retry and returns an assistant
    /// with `stop_reason::Error` (outcome == `Ok`).
    struct ErroringProvider;
    impl StreamFn for ErroringProvider {
        fn stream(
            &self,
            _: &conga::ModelSpec,
            _: &[AgentMessage],
            _: &str,
            _: &[ToolDefinition],
            _: Option<conga::CancelSignal>,
        ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = conga::StreamChunk> + Send>>
        {
            Box::pin(futures_util::stream::iter([
                conga::StreamChunk::TextDelta("partial".into()),
                conga::StreamChunk::Error("provider-boom".into()),
            ]))
        }
    }

    #[tokio::test]
    async fn run_turn_preserves_loop_outcome_when_turnend_append_fails() {
        // The agent loop emits its own AgentEvent::TurnEnd AFTER the
        // (errored) assistant is persisted but BEFORE run_turn appends the
        // SessionEvent::TurnEnd marker. Replacing the log with a directory
        // at that point makes ONLY that final append fail. The loop's outcome
        // (the provider error, surfaced as TurnEndReason::Error) must still
        // be returned — the storage error must not shadow it. Pre-fix, the
        // `.await?` on the TurnEnd append returned the storage error instead.
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::with_root(tmp.path().to_path_buf());
        let events_path = tmp.path().join(session.current_id()).join("events.jsonl");
        let host = Host::new(
            test_cfg(),
            session,
            Arc::new(PermissionPolicy::new(
                Mode::FullAuto,
                Arc::new(|_, _| Box::pin(async { true })),
            )),
            "sys".into(),
            vec![],
        )
        .with_stream_fn(Arc::new(ErroringProvider));

        let summary = host
            .run_turn("hi", move |ev| {
                if matches!(ev, AgentEvent::TurnEnd { .. }) {
                    // Only the trailing SessionEvent::TurnEnd append should
                    // fail; the assistant was already persisted above this.
                    let _ = std::fs::remove_file(&events_path);
                    let _ = std::fs::create_dir(&events_path);
                }
            })
            .await
            .expect("loop outcome must be returned, not the storage error");

        assert_eq!(
            summary.reason,
            TurnEndReason::Error {
                message: "provider-boom".into(),
            }
        );
    }

    #[tokio::test]
    async fn run_turn_is_serialized_per_host() {
        let host = test_host();
        // Poll the first turn once: the guard is acquired synchronously at
        // entry, before any await.
        let mut first = Box::pin(host.run_turn("hello", |_ev| {}));
        assert!(futures_util::poll!(first.as_mut()).is_pending());

        // A second concurrent turn on the same Host is rejected outright.
        match host.run_turn("again", |_ev| {}).await {
            Err(e) => assert!(e.to_string().contains("already running"), "{e}"),
            Ok(_) => panic!("second concurrent turn must be rejected"),
        }

        // Dropping the in-flight turn releases the slot (Drop guard): a
        // fresh turn starts — it just never finishes on the pending
        // provider, so the timeout elapsing is the pass condition.
        drop(first);
        let mut third = Box::pin(host.run_turn("third", |_ev| {}));
        let raced =
            tokio::time::timeout(std::time::Duration::from_millis(200), third.as_mut()).await;
        assert!(
            raced.is_err(),
            "third turn should be in flight, not rejected"
        );
    }

    /// Plan mode's directive must ride the PERSISTED user message tail —
    /// same channel as the env snapshot — so the system prompt stays
    /// byte-stable for the provider cache and the log replays exactly what
    /// the model saw. RED marker: "<agent-mode>plan</agent-mode>".
    #[tokio::test]
    async fn plan_mode_directive_rides_persisted_user_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::with_root(tmp.path().to_path_buf());
        let events_path = tmp.path().join(session.current_id()).join("events.jsonl");
        let host = Host::new(
            test_cfg(),
            session,
            Arc::new(PermissionPolicy::new(
                Mode::Plan,
                Arc::new(|_, _| Box::pin(async { true })),
            )),
            "sys".into(),
            vec![],
        )
        .with_stream_fn(Arc::new(ErroringProvider));

        let _ = host.run_turn("do it", |_ev| {}).await;

        let log = std::fs::read_to_string(&events_path).unwrap();
        assert!(
            log.contains("<agent-mode>plan</agent-mode>"),
            "persisted user event must carry the plan directive, log: {log}"
        );
    }

    /// The directive is plan-only: every other mode persists the user
    /// message without it.
    #[tokio::test]
    async fn non_plan_modes_carry_no_plan_directive() {
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::with_root(tmp.path().to_path_buf());
        let events_path = tmp.path().join(session.current_id()).join("events.jsonl");
        let host = Host::new(
            test_cfg(),
            session,
            Arc::new(PermissionPolicy::new(
                Mode::FullAuto,
                Arc::new(|_, _| Box::pin(async { true })),
            )),
            "sys".into(),
            vec![],
        )
        .with_stream_fn(Arc::new(ErroringProvider));

        let _ = host.run_turn("do it", |_ev| {}).await;

        let log = std::fs::read_to_string(&events_path).unwrap();
        assert!(
            !log.contains("<agent-mode>"),
            "non-plan mode must not carry the directive, log: {log}"
        );
    }
}
