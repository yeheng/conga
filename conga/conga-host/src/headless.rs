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
    use std::sync::Mutex;

    /// 三个 headless 测试都做进程 env 手术；并行会互相践踏（restore 回
    /// 旧值可能覆盖另一个测试刚设置的值）。串行化本模块内的测试。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
