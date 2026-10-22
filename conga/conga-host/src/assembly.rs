//! Shared Host assembly for every transport (the gateway's WebSocket, the
//! desktop app's IPC, and the CLI REPL).
//!
//! ONE place wires: fail-loud session resume → system prompt + skills →
//! permission mode → approver (registry + cancel watch + transport emit) →
//! tool set (built-in + external + MCP + transport extras) → sub-agent
//! spawner → `Host`. Transports keep only their channel/emitter plumbing.
//! Before this module existed, the gateway and the desktop backend each
//! hand-copied this wiring and the copies drifted (the desktop `/clear`
//! stopped rotating the log; the desktop missed `policy.set_signal`) - and
//! the CLI kept a third hand-rolled copy that never got the sub-agent
//! spawner at all.

use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use conga::ToolDefinition;

use crate::approval::{self, ApprovalRegistry, RegisterOutcome};
use crate::permission::{Approver, Mode, PermissionPolicy};
use crate::subagent::HostSubagentSpawner;
use crate::subagent_types::SubagentEvent;
use crate::{ConfigLoader, HookStack, Host, HostConfig, SessionManager};

/// A session-API failure for transport mapping: `Config` (provider/env
/// setup broken — the user must fix `~/.conga` or env) vs `Session` (this
/// session's log refuses to load — corruption fails closed). Transports
/// render `to_string()`; the variants exist so future callers can act on
/// the class instead of parsing prose.
#[derive(Debug)]
pub enum AssemblyError {
    Config(String),
    Session(String),
}

impl std::fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssemblyError::Config(m) => write!(f, "Config error: {m}"),
            AssemblyError::Session(m) => write!(f, "Session error: {m}"),
        }
    }
}

/// Where an approval request goes when a tool needs human consent:
/// `(request_id, tool_name, args, preview)`. Transports forward it onto
/// their ordered event channel so a request can never overtake the
/// `tool_start` event of the call it belongs to. `preview` is the
/// human-readable diff for file-mutating tools (see [`crate::preview`]),
/// `None` for tools whose arguments already read well.
pub type ApprovalEmit =
    Arc<dyn Fn(String, String, serde_json::Value, Option<String>) + Send + Sync>;

/// Where sub-agent events go. Transports forward onto their ordered event
/// channel (the same one as approvals and main-agent stream events).
pub type SubagentEmit = Arc<dyn Fn(SubagentEvent) + Send + Sync>;

/// The tool set every host assembles: built-in first, then `prepend` (the
/// CLI's in-process ext tools outrank external ones), then external
/// (`CONGA_EXTERNAL_TOOLS`), then MCP (`~/.conga/mcp.json`), then `append`
/// (the desktop app's in-process extension tools). One gathering point for
/// initial load AND the CLI's `/reload-tools`, so a reload can never drift
/// from the initial set. `quiet` logs to tracing (transports); otherwise
/// the CLI's stderr banners print.
pub async fn gather_tools(
    prepend: Vec<ToolDefinition>,
    append: Vec<ToolDefinition>,
    quiet: bool,
) -> Vec<ToolDefinition> {
    let external = {
        let cmds = crate::commands_from_env();
        if cmds.is_empty() {
            Vec::new()
        } else {
            match crate::load_external_tools(&cmds).await {
                Ok(t) => {
                    if quiet {
                        tracing::info!("loaded {} external tool(s)", t.len());
                    } else {
                        eprintln!(
                            "(external tools: {} from {} command(s))",
                            t.len(),
                            cmds.len()
                        );
                    }
                    t
                }
                Err(e) => {
                    if quiet {
                        tracing::warn!("external tools load failed: {e}");
                    } else {
                        eprintln!("(external tools load failed: {e})");
                    }
                    Vec::new()
                }
            }
        }
    };
    let mcp_tools = crate::mcp::load_all_mcp().await;
    let built_in = crate::built_in_tools();
    let mut tools = built_in;
    tools.extend(prepend);
    tools.extend(external);
    tools.extend(mcp_tools);
    tools.extend(append);
    dedup_tool_names(tools, quiet)
}

/// First registration wins (the assembly order IS the priority: built-in →
/// prepend → external → MCP → append). Later same-name tools are dropped
/// with a warning instead of silently shadowing — the loop resolves tools
/// by first name match, so an unreported collision would route calls to the
/// wrong implementation.
fn dedup_tool_names(tools: Vec<ToolDefinition>, quiet: bool) -> Vec<ToolDefinition> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(tools.len());
    for t in tools {
        if seen.insert(t.name.clone()) {
            out.push(t);
        } else if quiet {
            tracing::warn!(
                tool = %t.name,
                "duplicate tool name dropped (first registration wins)"
            );
        } else {
            eprintln!(
                "(warning: duplicate tool name '{}' dropped; first registration wins)",
                t.name
            );
        }
    }
    out
}

/// A fully wired session: the transport drives `host.run_turn` per user
/// message, fills in approval decisions on `registry`, and cancels via the
/// Host's abort flag plus `cancel_tx`.
pub struct SessionAssembly {
    pub host: Host,
    /// In-flight approvals for this session. `approval_response` from the
    /// transport fills in decisions; the turn boundary calls
    /// `clear_pending`.
    pub registry: Arc<StdMutex<ApprovalRegistry>>,
    /// Cancel broadcast: sending `true` unlocks any approval still waiting
    /// on a decision (the Host's abort flag stops the agent loop itself).
    pub cancel_tx: tokio::sync::watch::Sender<bool>,
}

/// Lock the approval registry without letting a poisoned mutex panic.
///
/// Rust poisons a mutex when a thread panics while holding it, but
/// `ApprovalRegistry` is a plain map and every critical section here is a
/// few microseconds — a panic cannot have left it half-updated, so the data
/// behind a `PoisonError` is still valid.
///
/// Propagating the poison instead would mean one bad frame anywhere in the
/// session kills the whole connection on the *next* lock (`unwrap()`), i.e.
/// a remotely reachable DoS on the gateway. Recovering is strictly better:
/// the alternative is a dead session.
///
/// Note this is a `std` Mutex held across an `await`-free critical section
/// by design — these are map insert/lookup/clear, never I/O, so blocking a
/// runtime thread is not a concern and switching to an async mutex would
/// only add a cancellation hazard around `register`.
pub fn lock_registry(
    registry: &StdMutex<ApprovalRegistry>,
) -> std::sync::MutexGuard<'_, ApprovalRegistry> {
    registry.lock().unwrap_or_else(|e| e.into_inner())
}

/// Fail-loud resume of a session's event log (the config-independent step,
/// so tests can exercise corruption refusal without provider config).
/// Corruption is an `Err`, never adopt-and-restart over a damaged log.
pub async fn resume_session(
    store_root: &Path,
    session_id: &str,
) -> Result<SessionManager, AssemblyError> {
    let mgr = SessionManager::with_root(store_root.to_path_buf());
    match mgr.resume(session_id).await {
        Ok(history) => {
            if !history.is_empty() {
                tracing::info!(
                    "session {session_id}: resumed {} msgs (event log)",
                    history.len()
                );
            }
            Ok(mgr)
        }
        Err(e) => Err(AssemblyError::Session(e.to_string())),
    }
}

impl SessionAssembly {
    /// Assemble one session's `Host` exactly as every server transport
    /// does. `extra_tools` are appended after built-in + external + MCP
    /// (the desktop app adds its in-process extension tools there).
    /// `Err` class is [`AssemblyError`] (Config vs Session); transports
    /// render `to_string()` directly for the user.
    pub async fn build(
        store_root: &Path,
        session_id: &str,
        extra_tools: Vec<ToolDefinition>,
        approval_emit: ApprovalEmit,
        subagent_emit: SubagentEmit,
    ) -> Result<Self, AssemblyError> {
        let cfg = match ConfigLoader::load() {
            Ok(c) => c,
            Err(e) => return Err(AssemblyError::Config(e.to_string())),
        };
        let session_mgr = resume_session(store_root, session_id).await?;
        // Intentionally one env knob shared by every transport.
        let mode = std::env::var("CONGA_GATEWAY_MODE")
            .ok()
            .and_then(|s| Mode::parse(&s))
            .unwrap_or(Mode::AutoEdit);

        // Cancel 双通道：Host 的取消信号驱动 loop 中止，watch 解锁挂起的审批。
        let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);
        let registry = Arc::new(StdMutex::new(ApprovalRegistry::new()));
        let approver: Approver = {
            let registry = Arc::clone(&registry);
            let cancel_tx = cancel_tx.clone();
            let emit = approval_emit.clone();
            Arc::new(move |tool_name: &str, args: &serde_json::Value| {
                let registry = Arc::clone(&registry);
                let cancel_tx = cancel_tx.clone();
                let emit = Arc::clone(&emit);
                Box::pin(async move {
                    let outcome = { lock_registry(&registry).register(tool_name) };
                    let (request_id, rx) = match outcome {
                        RegisterOutcome::Remembered(v) => return v,
                        RegisterOutcome::Pending { request_id, rx } => (request_id, rx),
                    };
                    let preview =
                        crate::preview::approval_preview(tool_name, args, &crate::project_dir());
                    emit(
                        request_id.clone(),
                        tool_name.to_string(),
                        args.clone(),
                        preview,
                    );
                    let timeout_s = std::env::var("CONGA_APPROVAL_TIMEOUT_S")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(300u64);
                    // subscribe() 把当前值标记为已见：只有将来的 send 才命中
                    // changed()，本连接的第一次 cancel 不会毒化后续所有审批
                    // （见 approval.rs 的 wait_for_decision 测试）。
                    approval::wait_for_decision(
                        rx,
                        cancel_tx.subscribe(),
                        std::time::Duration::from_secs(timeout_s),
                    )
                    .await
                })
            })
        };
        let tools = gather_tools(Vec::new(), extra_tools, true).await;
        let host = assemble_host(
            cfg,
            session_mgr,
            mode,
            approver,
            Vec::new(),
            tools,
            subagent_emit,
        )
        .await;
        Ok(Self {
            host,
            registry,
            cancel_tx,
        })
    }

    /// The CLI REPL variant: default store root, `--resume=` handled here
    /// (adopted before assembly, same `last`/id semantics as `/resume`),
    /// caller-supplied approver (stdin), ext-feature hooks prepended before
    /// the permission policy, ext tools ranked right after built-ins, and a
    /// stderr sub-agent tap (the REPL has no event channel). Same wiring as
    /// [`build`] - one assembly, no third drifting copy. The caller still
    /// owns `install_ctrl_c` (the CLI hooks it straight after this returns).
    pub async fn build_cli(
        mode: Mode,
        approver: Approver,
        resume: Option<String>,
        extra_hooks: Vec<Arc<dyn conga::HookChain>>,
        ext_tools: Vec<ToolDefinition>,
    ) -> Result<Host, AssemblyError> {
        let cfg = match ConfigLoader::load() {
            Ok(c) => c,
            Err(e) => return Err(AssemblyError::Config(e.to_string())),
        };
        let session = SessionManager::new();
        if let Some(r) = resume {
            let res = if r == "last" {
                session.resume_last().await
            } else {
                session.resume(&r).await
            };
            match res {
                Ok(m) => eprintln!("(resumed {} with {} msgs)", session.current_id(), m.len()),
                Err(e) => eprintln!("(resume: {e})"),
            }
        }
        let tools = gather_tools(ext_tools, Vec::new(), false).await;
        // Sub-agent events carry task/content; they must never hit stderr
        // unconditionally — embedded hosts pipe stderr tails into persisted
        // outcomes (flow's harness runner keeps a 512B tail in its journal).
        // tracing keeps the REPL behavior when its subscriber is installed
        // and stays silent under the runner's warn filter.
        let tap: SubagentEmit = Arc::new(|ev: SubagentEvent| {
            tracing::debug!("[subagent] {ev:?}");
        });
        Ok(assemble_host(cfg, session, mode, approver, extra_hooks, tools, tap).await)
    }
}

/// Headless variant of [`assemble_host`] for [`crate::headless`]: built-in
/// tool set (external/MCP load from the pinned env/config root — normally
/// empty there), quiet logging, no extra hooks. Session root, provider and
/// mode come from the headless caller.
pub(crate) async fn assemble_headless_host(
    cfg: HostConfig,
    session: SessionManager,
    mode: Mode,
    approver: Approver,
    subagent_emit: SubagentEmit,
) -> Host {
    let tools = gather_tools(Vec::new(), Vec::new(), true).await;
    assemble_host(cfg, session, mode, approver, Vec::new(), tools, subagent_emit).await
}

/// The one Host assembly shared by every caller of this module:
/// Swap a sub-agent loop config onto a fast provider (model id + stream_fn
/// together — a half-applied switch would be worse than none). Tunables
/// (max_tokens, thinking) stay the parent's.
fn apply_fast_provider(
    loop_config: &mut conga::AgentLoopConfig,
    fast: &conga::ProviderConfig,
    tunables: &conga::AgentTunables,
) {
    loop_config.model = conga::ModelSpec {
        id: fast.model.clone(),
        api: fast.api,
        max_tokens: tunables.max_tokens,
    };
    loop_config.stream_fn = match fast.api {
        conga::ProviderApi::OpenAiCompat => Arc::new(conga::OpenAiCompat::from_config(fast)),
        conga::ProviderApi::Anthropic => Arc::new(conga::AnthropicProvider::from_config(fast)),
    };
}

/// The one Host assembly shared by every caller of this module:
/// system prompt -> hook stack (`extra_hooks` first, policy last) -> signal
/// wiring -> sub-agent spawner. Sub-agents get the built-in set minus
/// `spawn_subagents` (nesting disabled; shared MCP/external servers are not
/// built for N parallel loops); the SAME composed hook stack (extra gates,
/// then policy) gates every call the sub-agents do get.
async fn assemble_host(
    cfg: HostConfig,
    session: SessionManager,
    mode: Mode,
    approver: Approver,
    extra_hooks: Vec<Arc<dyn conga::HookChain>>,
    tools: Vec<ToolDefinition>,
    subagent_emit: SubagentEmit,
) -> Host {
    let cwd = crate::project_dir();
    let system_prompt = crate::append_project_doc(crate::CODING_AGENT_PROMPT, &cwd);
    let policy = Arc::new(PermissionPolicy::new(mode, approver));

    // Host hooks: extra gates first (e.g. the CLI's ext permission gate),
    // the permission policy last so its verdicts see post-gate calls.
    // The SAME composed stack gates sub-agents: a sub-agent used to get
    // the policy alone, letting it bypass every extra hook the host
    // installed.
    let mut hook_stack = HookStack::new(Vec::new());
    for h in extra_hooks {
        hook_stack.push(h);
    }
    // Process-out PreToolUse hooks (Claude-compatible protocol) sit between
    // in-process gates and the policy: they are repo-shippable extra gates,
    // and the policy stays last so a failed/timed-out hook failing open can
    // never bypass the built-in floor gate. Loaded per assembly (same
    // lifecycle as the base system prompt); None when no hooks.json exists anywhere.
    if let Some(process_chain) = crate::process_hooks::ProcessHookChain::discover(&cwd) {
        tracing::info!(
            hooks = process_chain.len(),
            "process hooks installed (PreToolUse)"
        );
        hook_stack.push(process_chain);
    }
    hook_stack.push(policy.clone());
    let spawner_hooks: Arc<dyn conga::HookChain> = Arc::new(hook_stack);

    let subagent_tools: Vec<_> = crate::built_in_tools()
        .iter()
        .filter(|t| t.name != "spawn_subagents")
        .cloned()
        .collect();
    let host = Host::new(cfg.clone(), session, policy.clone(), system_prompt, tools)
        .with_hooks(Arc::clone(&spawner_hooks));
    // The approver may wait on a client that never answers; give it the
    // Host's cancel signal so cancel unwinds the wait. (The desktop
    // backend used to miss this line - it lived only in the gateway's
    // copy of this wiring.)
    policy.set_signal(host.signal().clone());
    let spawner_signal = host.signal().clone();
    let spawner_stream_fn = cfg.provider_stream_fn();
    // Fast-model routing, precedence: the web UI's settings file
    // (`fastLlm` group) wins, then a complete `CONGA_FAST_LLM_*` env set.
    // Same tunables otherwise; a partial env set fails loud at startup —
    // a typo must not silently keep the main model.
    let mut loop_config = cfg.build_loop_config(
        cfg.tunables.max_turns,
        Some(spawner_signal.clone()),
        None,
        spawner_stream_fn,
    );
    let settings_fast = crate::settings::effective_fast_provider(&crate::settings::load_settings());
    match settings_fast {
        Some(fast) => apply_fast_provider(&mut loop_config, &fast, &cfg.tunables),
        None => {
            match conga::ProviderConfig::from_env_prefixed("CONGA_FAST_LLM", &|k: &str| {
                std::env::var(k)
            }) {
                Ok(Some(fast)) => apply_fast_provider(&mut loop_config, &fast, &cfg.tunables),
                Ok(None) => {}
                Err(e) => {
                    eprintln!("(warning: CONGA_FAST_LLM_* incomplete, sub-agents keep the main model: {e})");
                }
            }
        }
    }
    // Sub-agent runs persist under the parent session's `sub/` directory:
    // crash-recoverable, and the parent can read a sub-agent's transcript
    // from disk (`read` allows absolute paths under ~/.conga).
    let sub_log_root = conga::JsonlStorage::default_root()
        .base_dir_clone()
        .join(host.session().current_id());
    let spawner = Arc::new(
        HostSubagentSpawner::new(
            "You are a focused sub-agent. Complete your assigned task concisely.".into(),
            subagent_tools,
            spawner_hooks,
            spawner_signal,
            crate::project_dir(),
            loop_config,
        )
        .with_ws_emit(subagent_emit)
        .with_sub_log_root(sub_log_root.join("sub")),
    );
    host.with_spawner(spawner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mid-file row this reader does not understand (a `Data` error, not
    /// a torn tail) must refuse the session — never a silent adopt.
    /// (Ported from the gateway's ws.rs so the contract lives with the
    /// implementation.)
    #[tokio::test]
    async fn resume_session_fails_loud_on_corrupt_log() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "corrupt-sess";
        let dir = tmp.path().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let good = serde_json::to_string(&conga::SessionEvent::TurnStart).unwrap();
        let body = format!("{good}\n{{\"type\":\"from_the_future\"}}\n{good}\n");
        std::fs::write(dir.join("events.jsonl"), body).unwrap();

        let err = resume_session(tmp.path(), id)
            .await
            .err()
            .expect("corrupt log must error");
        let msg = err.to_string();
        assert!(
            msg.contains("from_the_future") || msg.contains("invalid"),
            "{msg}"
        );
    }

    fn named_tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            label: name.into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            risk: conga::RiskLevel::Low,
            execute: std::sync::Arc::new(|_ctx| {
                Box::pin(async { Ok(conga::ToolResult::error("stub")) })
            }),
        }
    }

    /// A panic while holding the registry must not disable the session.
    /// `lock_registry` recovers the guard instead of propagating the poison;
    /// the old `lock().unwrap()` would have panicked the whole transport task
    /// on its next approval, which on the gateway is remotely reachable.
    ///
    /// The spawned thread panics on purpose, so the harness prints one panic
    /// message — that is the point of the test, not a failure.
    #[test]
    fn lock_registry_recovers_from_a_poisoned_mutex() {
        let registry = Arc::new(StdMutex::new(ApprovalRegistry::new()));
        let r = Arc::clone(&registry);
        let joined = std::thread::spawn(move || {
            let _guard = r.lock().unwrap();
            panic!("poison the registry on purpose");
        })
        .join();
        assert!(joined.is_err(), "precondition: the thread must panic");
        assert!(
            registry.is_poisoned(),
            "precondition: the mutex must be poisoned"
        );

        let mut guard = lock_registry(&registry);
        // Still functional: a new approval can be registered and answered.
        match guard.register("bash") {
            RegisterOutcome::Pending { request_id, .. } => {
                guard.respond(&request_id, true, false);
            }
            other => panic!("expected a pending approval, got {other:?}"),
        }
        guard.clear_pending();
    }

    #[test]
    fn duplicate_tool_names_first_wins() {
        let tools = vec![
            named_tool("bash"),
            named_tool("web_search"),
            named_tool("bash"), // duplicate: dropped
        ];
        let out = dedup_tool_names(tools, true);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "bash");
        assert_eq!(out[1].name, "web_search");
    }

    // ── AssemblyError ───────────────────────────────────────────

    #[test]
    fn assembly_error_display_names_the_class() {
        assert_eq!(
            AssemblyError::Config("boom".into()).to_string(),
            "Config error: boom"
        );
        assert_eq!(
            AssemblyError::Session("boom".into()).to_string(),
            "Session error: boom"
        );
    }

    // ── gather_tools ────────────────────────────────────────────

    #[tokio::test]
    async fn gather_tools_orders_prepend_before_append_and_dedups() {
        let tools = gather_tools(
            vec![named_tool("ext_pre")],
            vec![named_tool("ext_post")],
            true,
        )
        .await;
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"bash"), "built-ins are always present: {names:?}");
        let i_pre = names.iter().position(|n| *n == "ext_pre").unwrap();
        let i_post = names.iter().position(|n| *n == "ext_post").unwrap();
        assert!(i_pre < i_post, "prepend outranks append: {names:?}");
        // Every name is unique: the first registration wins.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate names: {names:?}");
    }

    #[tokio::test]
    async fn gather_tools_keeps_the_built_in_when_a_prepended_tool_shadows_it() {
        let tools = gather_tools(vec![named_tool("bash")], vec![], true).await;
        let bash_count = tools.iter().filter(|t| t.name == "bash").count();
        assert_eq!(bash_count, 1, "the built-in registration must win");
    }

    // ── build / build_cli ───────────────────────────────────────

    #[tokio::test]
    async fn build_fails_loud_when_the_provider_config_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let config_missing = ConfigLoader::load().is_err();
        let emit: ApprovalEmit = Arc::new(|_id, _name, _args, _preview| {});
        let sub: SubagentEmit = Arc::new(|_ev| {});
        match SessionAssembly::build(tmp.path(), "sess-1", vec![], emit, sub).await {
            Err(AssemblyError::Config(msg)) => {
                assert!(config_missing, "config was loadable but the build failed");
                assert!(msg.contains("CONGA_LLM"), "{msg}");
            }
            Err(AssemblyError::Session(msg)) => panic!("unexpected session error: {msg}"),
            Ok(assembly) => {
                assert!(!config_missing, "a missing config must not assemble");
                assert!(!assembly.host.session().current_id().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn build_cli_fails_loud_when_the_provider_config_is_missing() {
        let config_missing = ConfigLoader::load().is_err();
        let approver: Approver = Arc::new(|_n, _a| Box::pin(async { true }));
        match SessionAssembly::build_cli(Mode::FullAuto, approver, None, Vec::new(), Vec::new())
            .await
        {
            Err(AssemblyError::Config(msg)) => {
                assert!(config_missing, "config was loadable but the build failed");
                assert!(msg.contains("CONGA_LLM"), "{msg}");
            }
            Err(AssemblyError::Session(msg)) => panic!("unexpected session error: {msg}"),
            Ok(host) => {
                assert!(!config_missing, "a missing config must not assemble");
                assert!(!host.session().current_id().is_empty());
            }
        }
    }

    // ── the shared assembly ─────────────────────────────────────

    fn test_host_config() -> HostConfig {
        HostConfig {
            provider: conga::ProviderConfig::from_parts(
                conga::ProviderApi::OpenAiCompat,
                "https://api.x.com/v1".into(),
                "sk-test".into(),
                "main-model".into(),
            )
            .unwrap(),
            tunables: conga::AgentTunables::from_env_with(&|_k| Err(std::env::VarError::NotPresent)),
        }
    }

    #[test]
    fn apply_fast_provider_swaps_the_model_and_keeps_the_tunables() {
        let cfg = test_host_config();
        let mut lc = cfg.build_loop_config(3, None, None, cfg.provider_stream_fn());
        let fast = conga::ProviderConfig::from_parts(
            conga::ProviderApi::Anthropic,
            "https://fast.x/v1".into(),
            "sk-fast".into(),
            "fast-model".into(),
        )
        .unwrap();
        apply_fast_provider(&mut lc, &fast, &cfg.tunables);
        assert_eq!(lc.model.id, "fast-model");
        assert_eq!(lc.model.api, conga::ProviderApi::Anthropic);
        // The parent's tunables stay: only the model+provider are swapped.
        assert_eq!(lc.model.max_tokens, cfg.tunables.max_tokens);
        assert_eq!(lc.max_turns, 3);
    }

    #[tokio::test]
    async fn assemble_host_wires_the_policy_the_tools_and_the_spawner() {
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::with_root(tmp.path().to_path_buf());
        let approver: Approver = Arc::new(|_n, _a| Box::pin(async { true }));
        let host = assemble_host(
            test_host_config(),
            session,
            Mode::FullAuto,
            approver,
            Vec::new(),
            vec![named_tool("only_tool")],
            Arc::new(|_ev: SubagentEvent| {}),
        )
        .await;
        assert_eq!(host.policy().mode(), Mode::FullAuto);
        assert!(host.spawner().is_some(), "the spawner must be wired");
        assert_eq!(host.tools.len(), 1);
        assert_eq!(host.tools[0].name, "only_tool");
    }

    /// The headless variant wires the SAME policy and spawner, but its tool
    /// set comes from the built-ins alone — no caller-supplied extras.
    #[tokio::test]
    async fn assemble_headless_host_wires_the_policy_and_the_builtin_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::with_root(tmp.path().to_path_buf());
        let approver: Approver = Arc::new(|_n, _a| Box::pin(async { true }));
        let host = assemble_headless_host(
            test_host_config(),
            session,
            Mode::Plan,
            approver,
            Arc::new(|_ev: SubagentEvent| {}),
        )
        .await;

        assert_eq!(host.policy().mode(), Mode::Plan);
        assert!(host.spawner().is_some(), "the spawner must be wired");

        // Every built-in tool is present, and nothing else is.
        let names: Vec<&str> = host.tools.iter().map(|t| t.name.as_str()).collect();
        for want in crate::built_in_tools() {
            assert!(
                names.contains(&want.name.as_str()),
                "missing built-in tool {}",
                want.name
            );
        }
        assert!(
            !names.contains(&"only_tool"),
            "the headless variant must not add caller-supplied tools"
        );
    }
}
