//! Web-UI LLM env settings, persisted at `<config_dir>/settings.json`.
//!
//! Precedence: the file OVERRIDES process env — an explicit UI choice beats
//! ambient `.env` config. [`Host::run_turn`](crate::Host::run_turn)
//! re-resolves the provider from this file EVERY turn, so a UI save reaches
//! the very next LLM call; sub-agent fast routing re-reads it at session
//! assembly. The API never returns the raw key — only a mask — and a PUT
//! with a blank `apiKey` means "keep the stored one".

use std::path::{Path, PathBuf};

use conga::ProviderConfig;

/// Cap for the custom system prompt (markdown). Generous vs. the 16 KB
/// project-doc cap: the prompt is the user's own and replaces the
/// built-in text, but a runaway paste must not eat the context window.
const MAX_CUSTOM_PROMPT_BYTES: usize = 64 * 1024;

/// Bounds for the user-configured context window (`maxTokens`). Below 1024
/// the window is unusably small; above 2M no current model qualifies.
pub const MIN_MAX_TOKENS: u64 = 1024;
pub const MAX_MAX_TOKENS: u64 = 2_000_000;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LlmGroup {
    pub base_url: String,
    /// Write-only over the API: GET responses carry a mask instead, PUT
    /// with an empty string keeps the stored key.
    pub api_key: String,
    pub model: String,
    /// `openai` (default) or `anthropic`.
    pub api: String,
}

/// The whole settings file.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EnvSettings {
    /// Main-agent LLM; `None` = use process env.
    pub llm: Option<LlmGroup>,
    /// Sub-agent fast model; `None` = main model / env prefix.
    pub fast_llm: Option<LlmGroup>,
    /// Custom base instructions (markdown): replaces the built-in
    /// `CODING_AGENT_PROMPT` prefix while project doc / skills / env
    /// snapshot stay appended. `None` or blank = built-in prompt.
    pub system_prompt: Option<String>,
    /// User-configured context window (tokens): overrides
    /// `CONGA_CONTEXT_WINDOW` for compaction and the context-stats
    /// percentage. `None` = env applies. See [`effective_max_tokens`].
    pub max_tokens: Option<u64>,
}

impl LlmGroup {
    /// Validate one group: non-empty http(s) base URL, non-empty model,
    /// api in {openai, anthropic}. `api_key` presence is checked at merge
    /// time (it may be inherited from the stored group).
    pub fn validate(&self) -> Result<(), String> {
        let base = self.base_url.trim();
        if base.is_empty() {
            return Err("base_url is required".into());
        }
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err(format!(
                "base_url must start with http:// or https://: {base}"
            ));
        }
        if self.model.trim().is_empty() {
            return Err("model is required".into());
        }
        match self.api.trim() {
            "" | "openai" | "anthropic" => Ok(()),
            other => Err(format!("api must be openai or anthropic, got: {other}")),
        }
    }

    /// Build a provider; egress proxies fall back to `CONGA_LLM_PROXY*`.
    pub fn to_provider(&self) -> Result<ProviderConfig, conga::ConfigError> {
        let api = match self.api.trim() {
            "anthropic" => conga::ProviderApi::Anthropic,
            _ => conga::ProviderApi::OpenAiCompat,
        };
        ProviderConfig::from_parts(
            api,
            self.base_url.trim().to_string(),
            self.api_key.clone(),
            self.model.trim().to_string(),
        )
    }
}

/// `~/.conga/settings.json`.
pub fn settings_path() -> PathBuf {
    conga::storage::config_dir().join("settings.json")
}

/// Load from an explicit path. Missing file → empty settings; a corrupt
/// file warns and degrades to empty (the process env still provides a
/// working provider — a hand-mangled file must not brick the app).
pub fn load_settings_at(path: &Path) -> EnvSettings {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<EnvSettings>(&bytes) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("{}: corrupt settings file ignored: {e}", path.display());
                EnvSettings::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => EnvSettings::default(),
        Err(e) => {
            tracing::warn!("{}: settings file unreadable, ignored: {e}", path.display());
            EnvSettings::default()
        }
    }
}

pub fn load_settings() -> EnvSettings {
    load_settings_at(&settings_path())
}

/// Async variant of [`load_settings_at`] for callers already on the Tokio
/// runtime (`Host::run_turn` re-reads the file every turn): `tokio::fs`
/// keeps the read off the worker thread's synchronous path. Same
/// degrade-to-empty semantics as the sync loader.
pub async fn load_settings_at_async(path: &Path) -> EnvSettings {
    match tokio::fs::read(path).await {
        Ok(bytes) => match serde_json::from_slice::<EnvSettings>(&bytes) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("{}: corrupt settings file ignored: {e}", path.display());
                EnvSettings::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => EnvSettings::default(),
        Err(e) => {
            tracing::warn!("{}: settings file unreadable, ignored: {e}", path.display());
            EnvSettings::default()
        }
    }
}

pub async fn load_settings_async() -> EnvSettings {
    load_settings_at_async(&settings_path()).await
}

/// Validate then atomically persist (tmp + rename). A crash can never
/// leave a torn file shadowing an intact one.
pub fn save_settings_at(path: &Path, s: &EnvSettings) -> Result<(), String> {
    if let Some(g) = &s.llm {
        g.validate().map_err(|e| format!("llm: {e}"))?;
    }
    if let Some(g) = &s.fast_llm {
        g.validate().map_err(|e| format!("fastLlm: {e}"))?;
    }
    validate_max_tokens(s.max_tokens).map_err(|e| format!("maxTokens: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    // Unique tmp name (same directory → same mount, atomic rename): two
    // concurrent PUTs must never write through one shared `settings.json.tmp`.
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(format!(".{}.conga-tmp", uuid::Uuid::new_v4()));
    let tmp = std::path::PathBuf::from(tmp_os);
    let res = std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.map_err(|e| e.to_string())?;
    Ok(())
}

pub fn save_settings(s: &EnvSettings) -> Result<(), String> {
    save_settings_at(&settings_path(), s)
}

/// The provider the main agent should use this turn: the settings file's
/// `llm` group wins over the process env. `None` = keep env config.
pub fn effective_provider(s: &EnvSettings) -> Option<ProviderConfig> {
    let g = s.llm.as_ref()?;
    match g.to_provider() {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("settings llm group invalid, falling back to env: {e}");
            None
        }
    }
}

/// Same for the sub-agent fast model.
pub fn effective_fast_provider(s: &EnvSettings) -> Option<ProviderConfig> {
    let g = s.fast_llm.as_ref()?;
    match g.to_provider() {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("settings fastLlm group invalid, ignoring: {e}");
            None
        }
    }
}

/// The context window this process should assume: the settings file's
/// `maxTokens` wins over `CONGA_CONTEXT_WINDOW`, else the 128k default.
/// Stateless — reads the file on each call, so a UI save reaches the next
/// stats request without a restart (`run_turn` feeds the same settings
/// re-read into the compaction budget).
pub fn effective_max_tokens() -> u64 {
    resolve_max_tokens(load_settings().max_tokens, &|k| std::env::var(k))
}

/// Testable core of [`effective_max_tokens`]; the env comes from the
/// process in production. Missing / unparsable env falls to the 128k
/// default (same tolerance as `ContextBudget::from_env`).
fn resolve_max_tokens(
    settings: Option<u64>,
    lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
) -> u64 {
    settings.unwrap_or_else(|| {
        lookup("CONGA_CONTEXT_WINDOW")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(128_000)
    })
}

/// Bounds check for the stored `maxTokens` (error without prefix; callers
/// add the `maxTokens:` context). `None` (no override) is always valid.
fn validate_max_tokens(n: Option<u64>) -> Result<(), String> {
    if let Some(n) = n {
        if !(MIN_MAX_TOKENS..=MAX_MAX_TOKENS).contains(&n) {
            return Err(format!(
                "must be an integer between {MIN_MAX_TOKENS} and {MAX_MAX_TOKENS} (got {n})"
            ));
        }
    }
    Ok(())
}

/// Mask a stored key for display: `sk-…ab12`. Short keys mask entirely.
pub fn mask_key(key: &str) -> String {
    let k = key.trim();
    if k.is_empty() {
        return String::new();
    }
    if k.len() <= 8 {
        return "…".to_string();
    }
    format!("{}…{}", &k[..3], &k[k.len() - 4..])
}

/// The GET view: same shape as [`EnvSettings`] but the raw `api_key` is
/// replaced by `apiKeySet` + `apiKeyHint` - the secret never crosses the
/// API (the gateway listens on 0.0.0.0; CORS is open). `systemPrompt` is
/// not a secret and round-trips verbatim; `maxTokens` is a number or null.
pub fn settings_to_masked_json(s: &EnvSettings) -> serde_json::Value {
    fn group(g: &Option<LlmGroup>) -> serde_json::Value {
        match g {
            None => serde_json::Value::Null,
            Some(g) => serde_json::json!({
                "baseUrl": g.base_url,
                "model": g.model,
                "api": if g.api.trim().is_empty() { "openai" } else { g.api.trim() },
                "apiKeySet": !g.api_key.trim().is_empty(),
                "apiKeyHint": mask_key(&g.api_key),
            }),
        }
    }
    serde_json::json!({
        "llm": group(&s.llm),
        "fastLlm": group(&s.fast_llm),
        "systemPrompt": s.system_prompt.as_deref().unwrap_or(""),
        "maxTokens": s.max_tokens,
    })
}

/// Full PUT flow: parse the payload, merge (blank `apiKey` = keep stored),
/// validate, persist, and return the new masked view. One code path for
/// the gateway REST route and the desktop Tauri command.
pub fn put_settings(payload: &serde_json::Value) -> Result<serde_json::Value, String> {
    let incoming: EnvSettings = serde_json::from_value(payload.clone())
        .map_err(|e| format!("invalid settings payload: {e}"))?;
    // `maxTokens` needs presence info beyond the parsed struct: an explicit
    // JSON null CLEARS the override (env applies again) while an absent key
    // keeps the stored one — serde cannot tell the two apart in Option<u64>.
    let merged = merge_put(
        &load_settings(),
        incoming,
        payload.get("maxTokens").is_some(),
    )?;
    save_settings(&merged)?;
    Ok(settings_to_masked_json(&merged))
}

/// Merge a PUT payload into the current settings. A `None` group clears
/// it (back to env); a present group replaces it, except a blank
/// `api_key` inherits the stored key (which must then exist). The custom
/// prompt is stored trimmed; blank clears it (built-in prompt back).
/// `maxTokens` mirrors that merge shape: present number overrides
/// (validated), explicit null clears, absent key keeps the stored value.
fn merge_put(
    current: &EnvSettings,
    incoming: EnvSettings,
    max_tokens_in_payload: bool,
) -> Result<EnvSettings, String> {
    let merge_group = |name: &str,
                       new: Option<LlmGroup>,
                       old: &Option<LlmGroup>|
     -> Result<Option<LlmGroup>, String> {
        let Some(mut g) = new else {
            return Ok(None); // cleared: env config applies again
        };
        if g.api_key.trim().is_empty() {
            let stored = old
                .as_ref()
                .map(|o| o.api_key.clone())
                .filter(|k| !k.trim().is_empty());
            g.api_key = stored
                .ok_or_else(|| format!("{name}: apiKey is required (no stored key to keep)"))?;
        }
        g.validate().map_err(|e| format!("{name}: {e}"))?;
        Ok(Some(g))
    };
    let system_prompt = match incoming.system_prompt {
        None => current.system_prompt.clone(), // absent key: keep stored
        Some(p) => {
            let trimmed = p.trim();
            if trimmed.is_empty() {
                None // blank clears: built-in prompt applies again
            } else {
                if trimmed.len() > MAX_CUSTOM_PROMPT_BYTES {
                    return Err(format!(
                        "systemPrompt: over {MAX_CUSTOM_PROMPT_BYTES} bytes (got {})",
                        trimmed.len()
                    ));
                }
                Some(trimmed.to_string())
            }
        }
    };
    let max_tokens = if max_tokens_in_payload {
        match incoming.max_tokens {
            Some(n) => {
                validate_max_tokens(Some(n)).map_err(|e| format!("maxTokens: {e}"))?;
                Some(n)
            }
            None => None, // explicit null clears: env window applies again
        }
    } else {
        current.max_tokens // absent key: keep stored
    };
    Ok(EnvSettings {
        llm: merge_group("llm", incoming.llm, &current.llm)?,
        fast_llm: merge_group("fastLlm", incoming.fast_llm, &current.fast_llm)?,
        system_prompt,
        max_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(base: &str, key: &str, model: &str, api: &str) -> LlmGroup {
        LlmGroup {
            base_url: base.into(),
            api_key: key.into(),
            model: model.into(),
            api: api.into(),
        }
    }

    fn env_ok(v: &'static str) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        move |_| Ok(v.to_string())
    }

    fn env_missing() -> impl Fn(&str) -> Result<String, std::env::VarError> {
        move |_| Err(std::env::VarError::NotPresent)
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let s = EnvSettings {
            llm: Some(group("https://a.x/v1", "sk-1", "m1", "openai")),
            fast_llm: Some(group("https://b.x/v1", "sk-2", "m2", "anthropic")),
            system_prompt: None,
            max_tokens: Some(200_000),
        };
        save_settings_at(&path, &s).unwrap();
        assert_eq!(load_settings_at(&path), s);
    }

    #[tokio::test]
    async fn async_roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let s = EnvSettings {
            llm: Some(group("https://a.x/v1", "sk-1", "m1", "openai")),
            fast_llm: None,
            system_prompt: None,
            max_tokens: None,
        };
        save_settings_at(&path, &s).unwrap();
        assert_eq!(load_settings_at_async(&path).await, s);
    }

    #[tokio::test]
    async fn async_missing_and_corrupt_files_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_settings_at_async(&dir.path().join("nope.json")).await,
            EnvSettings::default()
        );
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load_settings_at_async(&path).await, EnvSettings::default());
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_settings_at(&dir.path().join("nope.json")),
            EnvSettings::default()
        );
    }

    #[test]
    fn corrupt_file_degrades_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load_settings_at(&path), EnvSettings::default());
    }

    #[test]
    fn masked_view_never_leaks_the_key() {
        let s = EnvSettings {
            llm: Some(group("https://a.x/v1", "sk-secret-key-1234", "m1", "")),
            fast_llm: None,
            system_prompt: None,
            max_tokens: None,
        };
        let v = settings_to_masked_json(&s).to_string();
        assert!(!v.contains("sk-secret-key-1234"), "raw key leaked: {v}");
        assert!(v.contains("apiKeySet"));
        assert!(v.contains("sk-…1234"));
        assert!(
            v.contains("\"api\":\"openai\""),
            "blank api defaults to openai"
        );
    }

    #[test]
    fn mask_shapes() {
        assert_eq!(mask_key(""), "");
        assert_eq!(mask_key("short"), "…");
        assert_eq!(mask_key("sk-abcdefgh"), "sk-…efgh");
    }

    #[test]
    fn validate_rejects_bad_groups() {
        assert!(group("", "k", "m", "").validate().is_err());
        assert!(group("ftp://x", "k", "m", "").validate().is_err());
        assert!(group("https://x", "k", "", "").validate().is_err());
        assert!(group("https://x", "k", "m", "bogus").validate().is_err());
        assert!(group("https://x", "k", "m", "anthropic").validate().is_ok());
        assert!(group("https://x", "k", "m", "").validate().is_ok());
    }

    #[test]
    fn effective_provider_builds_from_group() {
        let s = EnvSettings {
            llm: Some(group("https://a.x/v1", "sk-1", "m1", "anthropic")),
            fast_llm: None,
            system_prompt: None,
            max_tokens: None,
        };
        let p = effective_provider(&s).unwrap();
        assert_eq!(p.base_url, "https://a.x/v1");
        assert_eq!(p.model, "m1");
        assert_eq!(p.api, conga::ProviderApi::Anthropic);
        assert!(effective_fast_provider(&s).is_none());
    }

    #[test]
    fn merge_blank_key_inherits_stored() {
        let current = EnvSettings {
            llm: Some(group("https://old.x", "sk-stored", "old-model", "")),
            fast_llm: None,
            system_prompt: None,
            max_tokens: None,
        };
        let incoming = EnvSettings {
            llm: Some(group("https://new.x", "", "new-model", "")),
            fast_llm: None,
            system_prompt: None,
            max_tokens: None,
        };
        let merged = merge_put(&current, incoming, false).unwrap();
        let g = merged.llm.unwrap();
        assert_eq!(g.base_url, "https://new.x");
        assert_eq!(g.model, "new-model");
        assert_eq!(
            g.api_key, "sk-stored",
            "blank key must inherit the stored one"
        );
    }

    #[test]
    fn merge_blank_key_without_stored_errors() {
        let incoming = EnvSettings {
            llm: Some(group("https://new.x", "", "m", "")),
            fast_llm: None,
            system_prompt: None,
            max_tokens: None,
        };
        let err = merge_put(&EnvSettings::default(), incoming, false).unwrap_err();
        assert!(err.contains("apiKey is required"), "{err}");
    }

    #[test]
    fn merge_none_group_clears() {
        let current = EnvSettings {
            llm: Some(group("https://old.x", "sk", "m", "")),
            fast_llm: Some(group("https://f.x", "sk", "m", "")),
            system_prompt: None,
            max_tokens: None,
        };
        let merged = merge_put(&current, EnvSettings::default(), false).unwrap();
        assert!(merged.llm.is_none() && merged.fast_llm.is_none());
    }

    #[test]
    fn merge_validates_and_isolates_groups() {
        let current = EnvSettings::default();
        // bad scheme on fast group must not pass, and the error names it
        let incoming = EnvSettings {
            llm: None,
            fast_llm: Some(group("notaurl", "sk", "m", "")),
            system_prompt: None,
            max_tokens: None,
        };
        let err = merge_put(&current, incoming, false).unwrap_err();
        assert!(err.starts_with("fastLlm:"), "{err}");
    }

    #[test]
    fn put_settings_full_flow_via_temp_path() {
        // put_settings uses the real settings_path(); exercise the merge +
        // validation + masked-view logic through it would touch ~/.conga.
        // The merge/save/view pieces are covered above; here we only check
        // payload parsing errors surface as strings.
        let err = put_settings(&serde_json::json!({"llm": 42})).unwrap_err();
        assert!(err.contains("invalid settings payload"), "{err}");
    }

    #[test]
    fn merge_prompt_trims_and_stores() {
        let incoming = EnvSettings {
            system_prompt: Some("  You are a pirate.  ".into()),
            ..EnvSettings::default()
        };
        let merged = merge_put(&EnvSettings::default(), incoming, false).unwrap();
        assert_eq!(merged.system_prompt.as_deref(), Some("You are a pirate."));
    }

    #[test]
    fn merge_prompt_blank_clears_absent_keeps() {
        let current = EnvSettings {
            system_prompt: Some("stored".into()),
            ..EnvSettings::default()
        };
        // blank string clears
        let merged = merge_put(
            &current,
            EnvSettings {
                system_prompt: Some("   ".into()),
                ..EnvSettings::default()
            },
            false,
        )
        .unwrap();
        assert!(merged.system_prompt.is_none());
        // absent key keeps the stored one
        let merged = merge_put(
            &current,
            EnvSettings {
                system_prompt: None,
                ..EnvSettings::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(merged.system_prompt.as_deref(), Some("stored"));
    }

    #[test]
    fn merge_prompt_rejects_oversized() {
        let err = merge_put(
            &EnvSettings::default(),
            EnvSettings {
                system_prompt: Some("x".repeat(MAX_CUSTOM_PROMPT_BYTES + 1)),
                ..EnvSettings::default()
            },
            false,
        )
        .unwrap_err();
        assert!(err.starts_with("systemPrompt:"), "{err}");
    }

    #[test]
    fn masked_view_carries_prompt_verbatim() {
        let s = EnvSettings {
            system_prompt: Some("# custom\n\n- keep **markdown**".into()),
            ..EnvSettings::default()
        };
        let v = settings_to_masked_json(&s).to_string();
        assert!(v.contains("# custom"), "{v}");
        assert!(v.contains("\"systemPrompt\":\""), "{v}");
        // absent -> empty string (not null) so the frontend stays simple
        let empty = settings_to_masked_json(&EnvSettings::default()).to_string();
        assert!(empty.contains("\"systemPrompt\":\"\""), "{empty}");
    }

    #[test]
    fn max_tokens_round_trips_and_masks() {
        // Some(n) serializes as a number, None as null — both survive a
        // save/load cycle and the masked view exposes them verbatim.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let s = EnvSettings {
            max_tokens: Some(200_000),
            ..EnvSettings::default()
        };
        save_settings_at(&path, &s).unwrap();
        assert_eq!(load_settings_at(&path), s);
        assert_eq!(settings_to_masked_json(&s)["maxTokens"], 200_000);
        assert_eq!(
            settings_to_masked_json(&EnvSettings::default())["maxTokens"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn merge_max_tokens_overrides_clears_and_keeps() {
        let current = EnvSettings {
            max_tokens: Some(200_000),
            ..EnvSettings::default()
        };
        // absent key keeps the stored value (an older client's PUT must
        // not silently wipe the user's window)
        let merged = merge_put(&current, EnvSettings::default(), false).unwrap();
        assert_eq!(merged.max_tokens, Some(200_000));
        // present number overrides
        let merged = merge_put(
            &current,
            EnvSettings {
                max_tokens: Some(64_000),
                ..EnvSettings::default()
            },
            true,
        )
        .unwrap();
        assert_eq!(merged.max_tokens, Some(64_000));
        // explicit null clears: the env window applies again
        let merged = merge_put(&current, EnvSettings::default(), true).unwrap();
        assert_eq!(merged.max_tokens, None);
    }

    #[test]
    fn merge_max_tokens_validates_bounds() {
        let incoming = |n: u64| EnvSettings {
            max_tokens: Some(n),
            ..EnvSettings::default()
        };
        for bad in [0u64, 1023, 2_000_001] {
            let err = merge_put(&EnvSettings::default(), incoming(bad), true).unwrap_err();
            assert!(err.starts_with("maxTokens:"), "n={bad}: {err}");
        }
        for good in [1024u64, 128_000, 2_000_000] {
            assert!(
                merge_put(&EnvSettings::default(), incoming(good), true).is_ok(),
                "n={good} must be accepted"
            );
        }
        // the save path validates too (a hand-edited file must not persist)
        let err =
            save_settings_at(Path::new("/nonexistent/settings.json"), &incoming(0)).unwrap_err();
        assert!(err.starts_with("maxTokens:"), "{err}");
    }

    #[test]
    fn effective_max_tokens_precedence() {
        // settings.json value > CONGA_CONTEXT_WINDOW > 128k default.
        // Injectable lookup: no process-env mutation, safe under parallel
        // tests.
        assert_eq!(resolve_max_tokens(Some(200_000), &env_missing()), 200_000);
        assert_eq!(resolve_max_tokens(None, &env_ok("50000")), 50_000);
        assert_eq!(resolve_max_tokens(None, &env_missing()), 128_000);
        assert_eq!(resolve_max_tokens(None, &env_ok("not-a-number")), 128_000);
    }

    // ── LlmGroup::to_provider ───────────────────────────────────

    #[test]
    fn to_provider_maps_api_and_trims_fields() {
        let p = group("  https://a.x/v1  ", "sk-1", "  m1  ", "")
            .to_provider()
            .unwrap();
        assert_eq!(p.base_url, "https://a.x/v1");
        assert_eq!(p.model, "m1");
        assert_eq!(p.api_key, "sk-1");
        assert_eq!(p.api, conga::ProviderApi::OpenAiCompat);

        let p = group("https://a.x/v1", "sk-2", "m2", "anthropic")
            .to_provider()
            .unwrap();
        assert_eq!(p.api, conga::ProviderApi::Anthropic);
    }

    #[test]
    fn to_provider_falls_back_to_openai_for_an_unknown_api() {
        // NOTE: `to_provider` does NOT reject an unknown api string - it
        // maps everything that is not "anthropic" onto the OpenAI-compat
        // provider. Only `LlmGroup::validate` rejects it, and validate()
        // runs on the save/merge path.
        let p = group("https://a.x/v1", "sk-1", "m1", "bogus")
            .to_provider()
            .unwrap();
        assert_eq!(p.api, conga::ProviderApi::OpenAiCompat);
    }

    #[test]
    fn to_provider_does_not_validate_the_group_fields() {
        // NOTE: `to_provider` never runs `validate()`: an empty base URL or
        // model still yields a provider. `effective_provider`'s
        // "invalid group -> fall back to env" branch is therefore
        // unreachable for a hand-edited settings file.
        let p = group("", "k", "", "").to_provider().unwrap();
        assert_eq!(p.base_url, "");
        assert_eq!(p.model, "");
    }

    // ── settings_path / loaders / saver ──────────────────────────

    #[test]
    fn settings_path_is_settings_json_in_the_config_dir() {
        // `config_dir()` is read lazily and may be pinned to an arbitrary
        // scratch path (`CONGA_CONFIG_DIR`, used by the headless host), so
        // hold the env lock and assert the CONTRACT — settings.json lives IN
        // the config dir — instead of the directory's name.
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let p = settings_path();
        assert_eq!(p.file_name().unwrap(), "settings.json");
        assert_eq!(p.parent().unwrap(), conga::storage::config_dir());
    }

    #[test]
    fn load_settings_reads_the_config_path() {
        // The real config dir may or may not hold a settings.json; the
        // loader must agree with an explicit read of the very same path.
        assert_eq!(load_settings(), load_settings_at(&settings_path()));
    }

    #[tokio::test]
    async fn load_settings_async_reads_the_config_path() {
        assert_eq!(
            load_settings_async().await,
            load_settings_at_async(&settings_path()).await
        );
    }

    #[test]
    fn save_settings_rejects_invalid_groups_before_touching_disk() {
        // The real settings path must NOT be written by a failing save:
        // validation runs before any file I/O.
        let bad_llm = EnvSettings {
            llm: Some(group("not-a-url", "k", "m", "")),
            ..EnvSettings::default()
        };
        let err = save_settings(&bad_llm).unwrap_err();
        assert!(err.starts_with("llm:"), "{err}");

        let bad_fast = EnvSettings {
            fast_llm: Some(group("not-a-url", "k", "m", "")),
            ..EnvSettings::default()
        };
        let err = save_settings(&bad_fast).unwrap_err();
        assert!(err.starts_with("fastLlm:"), "{err}");

        let bad_tokens = EnvSettings {
            max_tokens: Some(10),
            ..EnvSettings::default()
        };
        let err = save_settings(&bad_tokens).unwrap_err();
        assert!(err.starts_with("maxTokens:"), "{err}");
    }

    #[test]
    fn save_settings_at_writes_atomically_without_leaving_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let s = EnvSettings {
            llm: Some(group("https://a.x/v1", "sk-1", "m1", "openai")),
            ..EnvSettings::default()
        };
        save_settings_at(&path, &s).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"baseUrl\""), "{raw}");
        assert_eq!(load_settings_at(&path), s);
        // Only the target file remains: the tmp+rename left no residue.
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "settings.json")
            .collect();
        assert!(leftovers.is_empty(), "leftover files: {leftovers:?}");
    }

    #[test]
    fn save_settings_at_reports_an_unwritable_target_without_residue() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let err = save_settings_at(&blocker.join("settings.json"), &EnvSettings::default())
            .unwrap_err();
        assert!(!err.is_empty());
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["not-a-dir".to_string()], "{names:?}");
    }

    // ── validate_max_tokens / effective_max_tokens ───────────────

    #[test]
    fn validate_max_tokens_accepts_the_bounds_and_rejects_outside() {
        assert!(validate_max_tokens(None).is_ok(), "None = no override");
        assert!(validate_max_tokens(Some(MIN_MAX_TOKENS)).is_ok());
        assert!(validate_max_tokens(Some(MAX_MAX_TOKENS)).is_ok());
        for bad in [0u64, 1, MIN_MAX_TOKENS - 1, MAX_MAX_TOKENS + 1, u64::MAX] {
            let err = validate_max_tokens(Some(bad)).unwrap_err();
            assert!(err.contains("between 1024 and 2000000"), "n={bad}: {err}");
            assert!(err.contains(&bad.to_string()), "n={bad}: {err}");
        }
    }

    #[test]
    fn effective_max_tokens_reads_the_settings_file_then_the_env() {
        // settings.json wins over CONGA_CONTEXT_WINDOW; both are absent in
        // a plain checkout, so the 128k default applies.
        let expected = load_settings().max_tokens.unwrap_or_else(|| {
            std::env::var("CONGA_CONTEXT_WINDOW")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(128_000)
        });
        assert_eq!(effective_max_tokens(), expected);
    }
}
