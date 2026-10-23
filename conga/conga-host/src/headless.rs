//! Headless assembly for embedded hosts (flow's `flow-harness-agent`).
//!
//! The contract this module exists to enforce — every property below is
//! structural, not "filtered at the call site":
//!
//! 1. **The settings file is unreachable.** `build_headless` injects the
//!    provider's own stream_fn, which flips `Host::stream_fn_overridden`;
//!    `run_turn` then uses `EnvSettings::default()` instead of reading
//!    `~/.conga/settings.json` (`lib.rs` per-turn settings load). A
//!    settings file anywhere on the machine cannot override the provider,
//!    replace the system prompt, or cap max_tokens on this path.
//! 2. **The key never travels through the environment.** The caller passes
//!    it as a parameter; env parsing substitutes a placeholder that is
//!    immediately overwritten. `headless_prepare` rejects a pre-existing
//!    `CONGA_LLM_KEY` so an env-borne key can never silently win.
//! 3. **No cwd discovery.** Neither `config_file::apply` (`.conga/
//!    config.toml` walk) nor `dotenvy` is called — a workdir's `.env` or
//!    config file cannot inject provider settings (e.g. a `CONGA_LLM_PROXY`
//!    hijacking model traffic).
//!
//! All non-sensitive configuration (base_url/model/api, proxies, tunables)
//! is parsed from env by the same `ConfigLoader` code the CLI uses — one
//! parser, no drift between the two assembly paths.

use std::path::Path;
use std::sync::Arc;

use crate::assembly::AssemblyError;
use crate::permission::{Approver, Mode};
use crate::subagent_types::SubagentEvent;
use crate::{ConfigLoader, Host, SessionManager};

/// Pin conga's state root and project dir, and refuse env-borne keys.
///
/// **Contract: call this from the main thread before any tokio runtime
/// exists** — it mutates the process environment (`CONGA_CONFIG_DIR`,
/// `CONGA_PROJECT_DIR`), which std requires to be race-free. The flow
/// runner's `main` is synchronous up to this point.
///
/// * `roots` — the scratch root this whole run owns (sessions, tool state,
///   spill, memory, skills, mcp all land under `roots/conga` or
///   `roots/sessions`); deleted by the caller when the run ends.
/// * `project_dir` — the agent's working root (AGENTS.md, hooks, skills).
pub fn headless_prepare(roots: &Path, project_dir: &Path) -> Result<(), AssemblyError> {
    if std::env::var_os("CONGA_LLM_KEY").is_some() {
        return Err(AssemblyError::Config(
            "CONGA_LLM_KEY must not be set in the environment: the headless path \
             takes the api key as an explicit parameter and never reads it from env"
                .into(),
        ));
    }
    let config_root = roots.join("conga");
    // Documented main-thread, pre-runtime contract (see above).
    std::env::set_var("CONGA_CONFIG_DIR", &config_root);
    std::env::set_var("CONGA_PROJECT_DIR", project_dir);
    // Fail loud if the platform ignored the pin (e.g. a future storage
    // change): a run whose state lands outside the scratch root would leak
    // into the user's home and be readable by later runs.
    if !conga::storage::config_dir().starts_with(roots) {
        return Err(AssemblyError::Config(format!(
            "conga config dir {:?} does not fall inside the pinned roots {:?}; \
             refusing to run with un-isolated state",
            conga::storage::config_dir(),
            roots
        )));
    }
    Ok(())
}

/// Assemble one headless host: provider trio + tunables from env (same
/// parsing as the CLI), `api_key` from the parameter, sessions under
/// `roots/sessions`, headless-denying approver, no external/MCP tools
/// beyond what the environment already declares.
///
/// The returned host has its stream_fn injected, which is what makes the
/// settings file unreachable (see the module docs) — callers must not
/// "helpfully" re-load settings around it.
pub async fn build_headless(
    api_key: &str,
    roots: &Path,
    mode: Mode,
) -> Result<Host, AssemblyError> {
    let mut cfg = ConfigLoader::load_with(&|k: &str| {
        if k == "CONGA_LLM_KEY" {
            // Placeholder passes provider validation; overwritten below.
            // `headless_prepare` already refused a real env-borne key.
            Ok("headless-parameter-injected".to_string())
        } else {
            std::env::var(k)
        }
    })
    .map_err(|e| AssemblyError::Config(e.to_string()))?;
    cfg.provider.api_key = api_key.to_string();

    let session = SessionManager::with_root(roots.join("sessions"));
    // Nobody is watching: a consent prompt that blocks would hang the run,
    // so approval is a deny (surfaced to the model as a tool error). Modes
    // at or above auto-edit never reach it.
    let deny: Approver = Arc::new(|name: &str, _: &serde_json::Value| {
        let name = name.to_string();
        Box::pin(async move {
            tracing::warn!("headless approval for {name} denied (no human attached)");
            false
        })
    });
    let quiet_emit: crate::assembly::SubagentEmit = Arc::new(|_: SubagentEvent| {});
    let stream_fn = cfg.provider_stream_fn();
    let host = crate::assembly::assemble_headless_host(cfg, session, mode, deny, quiet_emit).await;
    Ok(host.with_stream_fn(stream_fn))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{EnvSettings, LlmGroup};

    /// 三个 headless 测试都做进程 env 手术；并行会互相践踏（restore 回
    /// 旧值可能覆盖另一个测试刚设置的值）。串行化本模块内的测试，并与
    /// 其它会读同一批 env 变量的测试共用 crate 级锁。
    use crate::ENV_LOCK;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("scratch dir")
    }

    fn env_snapshot(names: &[&str]) -> Vec<(String, Option<std::ffi::OsString>)> {
        names
            .iter()
            .map(|n| (n.to_string(), std::env::var_os(n)))
            .collect()
    }

    fn env_restore(snapshot: Vec<(String, Option<std::ffi::OsString>)>) {
        for (name, value) in snapshot {
            match value {
                Some(v) => std::env::set_var(&name, v),
                None => std::env::remove_var(&name),
            }
        }
    }

    #[tokio::test]
    async fn headless_refuses_env_borne_key() {
        let roots = scratch();
        let saved = env_snapshot(&["CONGA_LLM_KEY"]);
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("CONGA_LLM_KEY", "sk-env");
        let err = headless_prepare(roots.path(), roots.path()).unwrap_err();
        assert!(err.to_string().contains("CONGA_LLM_KEY"), "{err}");
        env_restore(saved);
    }

    #[tokio::test]
    async fn headless_pins_state_root_and_project_dir() {
        let roots = scratch();
        let project = scratch();
        let saved = env_snapshot(&["CONGA_CONFIG_DIR", "CONGA_PROJECT_DIR", "CONGA_LLM_KEY"]);
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CONGA_LLM_KEY");
        headless_prepare(roots.path(), project.path()).unwrap();
        assert_eq!(conga::storage::config_dir(), roots.path().join("conga"));
        assert_eq!(crate::project_dir(), project.path());
        env_restore(saved);
    }

    #[tokio::test]
    // The env lock is deliberately held across the await: the whole
    // point is that no other test mutates the process env meanwhile.
    #[allow(clippy::await_holding_lock)]
    async fn headless_with_takes_typed_config_and_restores_env_pins() {
        let roots = scratch();
        let saved = env_snapshot(&["CONGA_CONFIG_DIR", "CONGA_PROJECT_DIR", "CONGA_LLM_KEY"]);
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CONGA_LLM_KEY");
        let spec = HeadlessSpec {
            base_url: "https://inline.example/v1".into(),
            model: "inline-model".into(),
            api: "openai".into(),
            proxy: None,
            max_turns: Some(9),
            max_tool_calls_per_turn: None,
            max_tokens: Some(1234),
            tool_timeout_secs: None,
        };
        let session = build_headless_with(
            "sk-inline",
            &spec,
            roots.path(),
            roots.path(),
            Mode::FullAuto,
        )
        .await
        .expect("inline headless assembly");
        // 钉根在位（turn 进行中懒读的路径都落在 roots 内）
        assert_eq!(conga::storage::config_dir(), roots.path().join("conga"));
        assert_eq!(crate::project_dir(), roots.path());
        // 毒化 settings 不生效（stream_fn 注入缝与 env 变体同一条）
        let poisoned = EnvSettings {
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
        let (cfg, _) = session.host.resolve_turn_provider(&poisoned);
        assert_eq!(cfg.provider.model, "inline-model");
        assert_eq!(cfg.provider.base_url, "https://inline.example/v1");
        assert_eq!(cfg.provider.api_key, "sk-inline");
        assert_eq!(cfg.tunables.max_turns, 9);
        assert_eq!(cfg.tunables.max_tokens, 1234);
        drop(session);
        // pin 释放后环境恢复
        assert_ne!(conga::storage::config_dir(), roots.path().join("conga"));
        env_restore(saved);
    }

    #[tokio::test]
    // The env lock is deliberately held across the await: the whole
    // point is that no other test mutates the process env meanwhile.
    #[allow(clippy::await_holding_lock)]
    async fn headless_settings_file_cannot_reach_the_provider() {
        // The poison: a settings override that would win on the CLI path.
        let poisoned = EnvSettings {
            llm: Some(LlmGroup {
                base_url: "https://settings.example/v1".into(),
                api_key: "sk-settings".into(),
                model: "settings-model".into(),
                api: "openai".into(),
            }),
            fast_llm: None,
            system_prompt: Some("# hijacked\n".into()),
            max_tokens: Some(1),
        };
        let roots = scratch();
        let saved = env_snapshot(&[
            "CONGA_CONFIG_DIR",
            "CONGA_PROJECT_DIR",
            "CONGA_LLM_KEY",
            "CONGA_LLM_BASE_URL",
            "CONGA_LLM_MODEL",
            "CONGA_LLM_API",
        ]);
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CONGA_LLM_KEY");
        std::env::set_var("CONGA_LLM_BASE_URL", "https://env.example/v1");
        std::env::set_var("CONGA_LLM_MODEL", "env-model");
        std::env::remove_var("CONGA_LLM_API");
        headless_prepare(roots.path(), roots.path()).unwrap();
        let host = build_headless("sk-param", roots.path(), Mode::FullAuto)
            .await
            .expect("headless assembly");
        // With stream_fn injected, resolve_turn_provider short-circuits to
        // the assembled cfg — even when handed poisoned settings directly.
        let (cfg, _) = host.resolve_turn_provider(&poisoned);
        assert_eq!(cfg.provider.model, "env-model");
        assert_eq!(cfg.provider.base_url, "https://env.example/v1");
        assert_eq!(cfg.provider.api_key, "sk-param");
        env_restore(saved);
    }
}

// ---- in-process variant (flow-executor inline execution) ----

/// RAII pin for the lazily-read global knobs conga consults mid-turn
/// (`CONGA_CONFIG_DIR`, `CONGA_PROJECT_DIR`): set on creation, restored on
/// drop — the pin must outlive the whole turn because storage paths are
/// resolved lazily inside the loop.
///
/// **Contract: the caller guarantees no other thread reads or writes these
/// names concurrently.** flow-executor satisfies this by processing tasks
/// serially and never touching `CONGA_*` outside the agent path.
pub struct ScopedEnvPin {
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

impl ScopedEnvPin {
    pub fn pin(vars: &[(&str, std::ffi::OsString)]) -> Self {
        let saved = vars
            .iter()
            .map(|(name, _)| (name.to_string(), std::env::var_os(name)))
            .collect::<Vec<_>>();
        for (name, value) in vars {
            std::env::set_var(name, value);
        }
        ScopedEnvPin { saved }
    }
}

impl Drop for ScopedEnvPin {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            match value {
                Some(v) => std::env::set_var(&name, v),
                None => std::env::remove_var(&name),
            }
        }
    }
}

/// Explicit, typed configuration for in-process hosts: the provider trio,
/// the proxy and the tunables arrive as values (no `CONGA_*` env
/// round-trip), the api key as a parameter — the same never-through-env
/// rule as [`build_headless`].
#[derive(Debug, Clone)]
pub struct HeadlessSpec {
    pub base_url: String,
    pub model: String,
    /// `openai` (OpenAI-compat) or `anthropic`.
    pub api: String,
    pub proxy: Option<String>,
    pub max_turns: Option<usize>,
    pub max_tool_calls_per_turn: Option<usize>,
    pub max_tokens: Option<usize>,
    pub tool_timeout_secs: Option<u64>,
}

impl HeadlessSpec {
    fn provider_config(&self, api_key: &str) -> Result<conga::ProviderConfig, AssemblyError> {
        let api = match self.api.trim() {
            "" | "openai" => conga::ProviderApi::OpenAiCompat,
            "anthropic" => conga::ProviderApi::Anthropic,
            other => {
                return Err(AssemblyError::Config(format!(
                    "unknown api {other:?} (expected openai | anthropic)"
                )))
            }
        };
        conga::ProviderConfig::from_parts_with_proxy(
            api,
            self.base_url.trim().to_string(),
            api_key.to_string(),
            self.model.trim().to_string(),
            self.proxy.as_deref(),
        )
        .map_err(|e| AssemblyError::Config(e.to_string()))
    }

    fn tunables(&self) -> conga::AgentTunables {
        conga::AgentTunables {
            max_turns: self.max_turns.unwrap_or(50),
            max_tool_calls_per_turn: self.max_tool_calls_per_turn.unwrap_or(20),
            max_tokens: self.max_tokens.unwrap_or(4096),
            retry: Default::default(),
            tool_timeout_secs: self.tool_timeout_secs,
        }
    }
}

/// An assembled headless host plus the env pin that keeps its state root and
/// project dir resolved for the whole turn. Deref to drive `run_turn`.
pub struct HeadlessHost {
    pub host: Host,
    _pin: ScopedEnvPin,
}

impl std::ops::Deref for HeadlessHost {
    type Target = Host;
    fn deref(&self) -> &Host {
        &self.host
    }
}

/// In-process variant of [`build_headless`]: provider + tunables arrive as
/// typed values (no env reads), state root and project dir are pinned for the
/// caller's scope, sessions live under `roots/sessions`. Same guarantees:
/// settings file unreachable (stream_fn injected), no dotenvy/config-file
/// discovery, env-borne `CONGA_LLM_KEY` refused.
pub async fn build_headless_with(
    api_key: &str,
    spec: &HeadlessSpec,
    roots: &Path,
    project_dir: &Path,
    mode: Mode,
) -> Result<HeadlessHost, AssemblyError> {
    if std::env::var_os("CONGA_LLM_KEY").is_some() {
        return Err(AssemblyError::Config(
            "CONGA_LLM_KEY must not be set in the environment: the headless path \
             takes the api key as an explicit parameter"
                .into(),
        ));
    }
    let provider = spec.provider_config(api_key)?;
    if provider.base_url.is_empty() || provider.model.is_empty() {
        return Err(AssemblyError::Config(
            "headless spec requires a non-empty base_url and model".into(),
        ));
    }
    let cfg = crate::HostConfig {
        tunables: spec.tunables(),
        provider,
    };
    let pin = ScopedEnvPin::pin(&[
        ("CONGA_CONFIG_DIR", roots.join("conga").into_os_string()),
        (
            "CONGA_PROJECT_DIR",
            project_dir.to_path_buf().into_os_string(),
        ),
    ]);
    if !conga::storage::config_dir().starts_with(roots) {
        return Err(AssemblyError::Config(format!(
            "conga config dir {:?} does not fall inside the pinned roots {:?}",
            conga::storage::config_dir(),
            roots
        )));
    }
    let session = SessionManager::with_root(roots.join("sessions"));
    let deny: Approver = Arc::new(|name: &str, _: &serde_json::Value| {
        let name = name.to_string();
        Box::pin(async move {
            tracing::warn!("headless approval for {name} denied (no human attached)");
            false
        })
    });
    let quiet_emit: crate::assembly::SubagentEmit =
        Arc::new(|_: crate::subagent_types::SubagentEvent| {});
    let stream_fn = cfg.provider_stream_fn();
    let host = crate::assembly::assemble_headless_host(cfg, session, mode, deny, quiet_emit).await;
    Ok(HeadlessHost {
        host: host.with_stream_fn(stream_fn),
        _pin: pin,
    })
}
