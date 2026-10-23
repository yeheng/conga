//! Runtime-configurable proxy for outbound tool HTTP traffic (`fetch`,
//! `web_search`). Precedence: in-process override (desktop app UI) >
//! `CONGA_TOOL_PROXY` env > none. Supported schemes: http, https, socks5,
//! socks5h (with optional `user:pass@` userinfo embedded in the URL).

use std::sync::RwLock;

/// In-process override, set by hosts with a UI (the desktop app). `None`
/// means "fall back to the env var".
static OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

const ALLOWED_SCHEMES: &[&str] = &["http", "https", "socks5", "socks5h"];

/// Set (or clear) the runtime override. The URL is validated eagerly so a
/// typo from the UI fails at save time, not at the next tool call. `None`,
/// empty, and blank strings all clear the override.
pub fn set_tool_proxy(url: Option<&str>) -> Result<(), String> {
    let normalized = url.map(str::trim).filter(|s| !s.is_empty());
    match normalized {
        None => {
            *OVERRIDE.write().unwrap() = None;
            Ok(())
        }
        Some(url) => {
            validate_tool_proxy(url)?;
            *OVERRIDE.write().unwrap() = Some(url.to_string());
            Ok(())
        }
    }
}

/// The currently active proxy URL (override > env), if any.
pub fn tool_proxy() -> Option<String> {
    resolve_with(&|k| std::env::var(k))
}

/// Same as [`tool_proxy`] with an injectable env lookup — used by tests to
/// avoid mutating process env (mirrors `ProviderConfig::from_env_with`).
fn resolve_with(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Option<String> {
    if let Some(o) = OVERRIDE.read().unwrap().clone() {
        return Some(o);
    }
    conga::proxy::tool_proxy_from_env_with(lookup)
}

/// Validate a proxy URL without installing it. Single source of truth for
/// what a valid tool proxy is — hosts (desktop UI) call this before saving.
pub fn validate_tool_proxy(url: &str) -> Result<(), String> {
    let scheme = url.split("://").next().unwrap_or("").to_ascii_lowercase();
    if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
        return Err(format!(
            "unsupported proxy scheme '{scheme}' in '{}' (allowed: http, https, socks5, socks5h)",
            redact(url)
        ));
    }
    reqwest::Proxy::all(url)
        .map(|_| ())
        .map_err(|e| format!("invalid proxy url '{}': {e}", redact(url)))
}

/// Strip `user:pass@` credentials from a URL for logging/error messages
/// (scheme and host kept): the userinfo before `@` becomes `***`.
fn redact(url: &str) -> String {
    let at = match url.rfind('@') {
        Some(at) => at,
        None => return url.to_string(),
    };
    let scheme_len = url.find("://").map(|i| i + 3).unwrap_or(0);
    format!("{}***{}", &url[..scheme_len], &url[at..])
}

/// Attach the active proxy (if any) to a client builder. An invalid URL can
/// only come from the env var here (the override is validated at set time);
/// it is warned and skipped — fail-open to a direct connection.
pub fn apply_tool_proxy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    apply_proxy_url(builder, &tool_proxy())
}

fn apply_proxy_url(
    mut builder: reqwest::ClientBuilder,
    url: &Option<String>,
) -> reqwest::ClientBuilder {
    if let Some(url) = url {
        match reqwest::Proxy::all(url) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(e) => tracing::warn!("ignoring invalid tool proxy '{}': {e}", redact(url)),
        }
    }
    builder
}

#[cfg(test)]
pub(crate) mod test_util {
    pub(crate) fn fake_env(
        pairs: &[(&str, &str)],
    ) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    /// Serializes tests that touch the global override. Shared across this
    /// crate's test modules (proxy.rs, tools/fetch.rs) so parallel test
    /// threads/tasks cannot observe each other's override. A tokio mutex so
    /// async tests can hold it across `.await` (sync tests use
    /// `blocking_lock`).
    pub static LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_util::fake_env;

    #[test]
    fn validation_accepts_supported_schemes() {
        for url in [
            "http://127.0.0.1:8080",
            "https://proxy.corp:8443",
            "socks5://127.0.0.1:1080",
            "socks5h://proxy.internal:1080",
            "SOCKS5://127.0.0.1:1080",
            "http://user:pass@proxy:8080",
        ] {
            assert!(validate_tool_proxy(url).is_ok(), "should accept {url}");
        }
    }

    #[test]
    fn validation_rejects_bad_input() {
        for url in ["", "  ", "ftp://proxy:21", "127.0.0.1:8080", "http://"] {
            assert!(validate_tool_proxy(url).is_err(), "should reject '{url}'");
        }
    }

    #[test]
    fn override_beats_env_and_blank_env_is_none() {
        let _g = test_util::LOCK.blocking_lock();
        set_tool_proxy(Some("socks5://override:1080")).unwrap();
        assert_eq!(
            resolve_with(&fake_env(&[(
                conga::proxy::TOOL_PROXY_VAR,
                "http://env:8080"
            )])),
            Some("socks5://override:1080".to_string())
        );

        set_tool_proxy(None).unwrap();
        assert_eq!(
            resolve_with(&fake_env(&[(
                conga::proxy::TOOL_PROXY_VAR,
                "http://env:8080"
            )])),
            Some("http://env:8080".to_string())
        );
        // env unset and no override -> none
        assert_eq!(resolve_with(&fake_env(&[])), None);
        // blank env value treated as unset
        assert_eq!(
            resolve_with(&fake_env(&[(conga::proxy::TOOL_PROXY_VAR, "  ")])),
            None
        );
    }

    #[test]
    fn set_rejects_invalid_and_keeps_previous() {
        let _g = test_util::LOCK.blocking_lock();
        set_tool_proxy(Some("http://good:8080")).unwrap();
        assert!(set_tool_proxy(Some("garbage")).is_err());
        assert_eq!(
            resolve_with(&fake_env(&[])),
            Some("http://good:8080".to_string())
        );
        // blank input clears
        set_tool_proxy(Some("   ")).unwrap();
        assert_eq!(resolve_with(&fake_env(&[])), None);
    }

    #[test]
    fn apply_builds_client_with_socks5_and_fails_open_on_invalid() {
        let _g = test_util::LOCK.blocking_lock();
        set_tool_proxy(Some("socks5://127.0.0.1:1080")).unwrap();
        apply_tool_proxy(reqwest::Client::builder())
            .build()
            .unwrap();
        set_tool_proxy(None).unwrap();
        // invalid URL (as if from a bad env value) must not break client construction
        apply_proxy_url(reqwest::Client::builder(), &Some("ftp://bad".to_string()))
            .build()
            .unwrap();
    }

    #[test]
    fn redact_keeps_scheme_host_and_hides_credentials() {
        assert_eq!(
            redact("http://user:pass@proxy:8080"),
            "http://***@proxy:8080"
        );
        assert_eq!(redact("socks5://127.0.0.1:1080"), "socks5://127.0.0.1:1080");
    }

    // ── `tool_proxy` ──────────────────────────────────────────────

    /// Restore an env var to the value it had before the test.
    fn restore_env(key: &str, saved: Option<String>) {
        match saved {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn tool_proxy_returns_the_override_when_it_is_set() {
        let _g = test_util::LOCK.blocking_lock();
        set_tool_proxy(Some("socks5://override:1080")).unwrap();
        let resolved = tool_proxy();
        set_tool_proxy(None).unwrap();
        assert_eq!(resolved, Some("socks5://override:1080".to_string()));
    }

    #[test]
    fn tool_proxy_falls_back_to_the_env_var() {
        let _g = test_util::LOCK.blocking_lock();
        set_tool_proxy(None).unwrap();
        let saved = std::env::var(conga::proxy::TOOL_PROXY_VAR).ok();
        std::env::set_var(conga::proxy::TOOL_PROXY_VAR, "http://env-proxy:8080");
        let resolved = tool_proxy();
        restore_env(conga::proxy::TOOL_PROXY_VAR, saved);
        assert_eq!(resolved, Some("http://env-proxy:8080".to_string()));
    }

    #[test]
    fn tool_proxy_trims_the_env_var_and_ignores_a_blank_one() {
        let _g = test_util::LOCK.blocking_lock();
        set_tool_proxy(None).unwrap();
        let saved = std::env::var(conga::proxy::TOOL_PROXY_VAR).ok();

        std::env::set_var(conga::proxy::TOOL_PROXY_VAR, "  http://env-proxy:8080  ");
        let trimmed = tool_proxy();
        assert_eq!(trimmed, Some("http://env-proxy:8080".to_string()));

        std::env::set_var(conga::proxy::TOOL_PROXY_VAR, "   ");
        let blank = tool_proxy();
        restore_env(conga::proxy::TOOL_PROXY_VAR, saved);
        assert_eq!(blank, None, "a blank env value means no proxy");
    }

    #[test]
    fn tool_proxy_is_none_without_override_and_without_env() {
        let _g = test_util::LOCK.blocking_lock();
        set_tool_proxy(None).unwrap();
        let saved = std::env::var(conga::proxy::TOOL_PROXY_VAR).ok();
        std::env::remove_var(conga::proxy::TOOL_PROXY_VAR);
        let resolved = tool_proxy();
        restore_env(conga::proxy::TOOL_PROXY_VAR, saved);
        assert_eq!(resolved, None);
    }
}
