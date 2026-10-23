//! Outbound proxy resolution — the ONE place that knows the proxy env
//! vars and the precedence between them.
//!
//! Three egress classes use a proxy, and each has its own knobs:
//!
//! | class | env vars | resolved by |
//! |---|---|---|
//! | LLM provider traffic | `CONGA_LLM_PROXY*` | [`LlmProxy`] |
//! | outbound tool traffic (`fetch`, `web_search`) | `CONGA_TOOL_PROXY` | [`tool_proxy_from_env_with`] |
//! | remote MCP traffic | the tool proxy, then the legacy LLM/system chain | [`mcp_proxy_from_env_with`] |
//!
//! # The implicit fourth source
//!
//! `reqwest` reads the standard `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`
//! environment variables on its own when a client is built WITHOUT an
//! explicit proxy; adding ANY explicit proxy (`ClientBuilder::proxy`)
//! switches that automatic behaviour off.
//!
//! So every client built here has a fourth, invisible proxy source: the
//! process environment. It is NOT part of the precedence chains above — it
//! is the fallback that applies when no explicit proxy is configured. Two
//! consequences the code must respect:
//!
//! * A client that must not use a proxy at all (e.g. `fetch` with a pinned
//!   DNS address — a proxy resolves the host itself and silently drops the
//!   pin) has to call `ClientBuilder::no_proxy()` explicitly.
//! * A client that wants the system proxy must not add an explicit proxy,
//!   or it silently loses the system one.

/// Proxy for LLM provider traffic (applies to every scheme).
pub const LLM_PROXY_VAR: &str = "CONGA_LLM_PROXY";
/// Proxy for LLM provider traffic over `http://`.
pub const LLM_HTTP_PROXY_VAR: &str = "CONGA_LLM_HTTP_PROXY";
/// Proxy for LLM provider traffic over `https://`.
pub const LLM_HTTPS_PROXY_VAR: &str = "CONGA_LLM_HTTPS_PROXY";
/// Proxy for outbound tool traffic (`fetch`, `web_search`) and remote MCP.
pub const TOOL_PROXY_VAR: &str = "CONGA_TOOL_PROXY";

/// The LLM client's proxy knobs.
///
/// Precedence: the per-scheme knob wins; [`generic`](Self::generic) fills
/// in for whichever scheme has no explicit proxy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmProxy {
    /// Proxy for `http://` destinations.
    pub http: Option<String>,
    /// Proxy for `https://` destinations.
    pub https: Option<String>,
    /// Proxy for every scheme.
    pub generic: Option<String>,
}

impl LlmProxy {
    /// Resolve from an injectable env lookup.
    pub fn from_env_with(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Self {
        Self {
            http: lookup(LLM_HTTP_PROXY_VAR).ok(),
            https: lookup(LLM_HTTPS_PROXY_VAR).ok(),
            generic: lookup(LLM_PROXY_VAR).ok(),
        }
    }

    /// Resolve from the process environment.
    pub fn from_env() -> Self {
        Self::from_env_with(&|k: &str| std::env::var(k))
    }

    /// The prefixed variant (e.g. `CONGA_FAST_LLM_*`): the prefixed knobs
    /// win over the generic `CONGA_LLM_*` ones.
    pub fn from_prefixed_env_with(
        prefix: &str,
        lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    ) -> Self {
        let prefixed = |suffix: &str| lookup(&format!("{prefix}_{suffix}")).ok();
        Self {
            http: prefixed("HTTP_PROXY").or_else(|| lookup(LLM_HTTP_PROXY_VAR).ok()),
            https: prefixed("HTTPS_PROXY").or_else(|| lookup(LLM_HTTPS_PROXY_VAR).ok()),
            generic: prefixed("PROXY").or_else(|| lookup(LLM_PROXY_VAR).ok()),
        }
    }

    /// The proxy URL for one destination scheme: the scheme-specific knob,
    /// else the generic one.
    pub fn for_scheme(&self, https: bool) -> Option<&str> {
        let specific = if https {
            self.https.as_deref()
        } else {
            self.http.as_deref()
        };
        specific.or(self.generic.as_deref())
    }
}

/// The tool proxy URL from the environment.
///
/// Trimmed; a blank value means "no proxy" (the same tolerance the
/// runtime override applies).
pub fn tool_proxy_from_env_with(
    lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
) -> Option<String> {
    lookup(TOOL_PROXY_VAR)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The proxy for remote MCP traffic.
///
/// The tool proxy comes first (the host resolves it — it may come from a
/// runtime override), then the legacy LLM/system env chain for backward
/// compatibility. Direct connection when none is set.
pub fn mcp_proxy_from_env_with(
    lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    tool_proxy: Option<&str>,
) -> Option<String> {
    if let Some(p) = tool_proxy {
        return Some(p.to_string());
    }
    [LLM_PROXY_VAR, "HTTPS_PROXY", "https_proxy"]
        .iter()
        .find_map(|k| lookup(k).ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    #[test]
    fn llm_proxy_prefers_the_per_scheme_knob() {
        let p = LlmProxy::from_env_with(&env(&[
            (LLM_PROXY_VAR, "http://generic:1"),
            (LLM_HTTP_PROXY_VAR, "http://http:2"),
        ]));
        assert_eq!(p.for_scheme(false), Some("http://http:2"));
        assert_eq!(p.for_scheme(true), Some("http://generic:1"));
    }

    #[test]
    fn llm_proxy_without_knobs_has_no_proxy() {
        let p = LlmProxy::from_env_with(&env(&[]));
        assert_eq!(p.for_scheme(false), None);
        assert_eq!(p.for_scheme(true), None);
    }

    #[test]
    fn prefixed_llm_proxy_wins_over_the_generic_one() {
        let p = LlmProxy::from_prefixed_env_with(
            "CONGA_FAST_LLM",
            &env(&[
                ("CONGA_FAST_LLM_PROXY", "http://fast:1"),
                (LLM_PROXY_VAR, "http://main:2"),
            ]),
        );
        assert_eq!(p.generic.as_deref(), Some("http://fast:1"));
    }

    #[test]
    fn prefixed_llm_proxy_falls_back_to_the_generic_knobs() {
        let p = LlmProxy::from_prefixed_env_with(
            "CONGA_FAST_LLM",
            &env(&[
                (LLM_PROXY_VAR, "http://main:2"),
                (LLM_HTTPS_PROXY_VAR, "http://main-https:3"),
            ]),
        );
        assert_eq!(p.generic.as_deref(), Some("http://main:2"));
        assert_eq!(p.https.as_deref(), Some("http://main-https:3"));
    }

    #[test]
    fn tool_proxy_is_trimmed_and_a_blank_value_means_none() {
        assert_eq!(
            tool_proxy_from_env_with(&env(&[(TOOL_PROXY_VAR, "  http://p:1  ")])),
            Some("http://p:1".to_string())
        );
        assert_eq!(
            tool_proxy_from_env_with(&env(&[(TOOL_PROXY_VAR, "   ")])),
            None
        );
        assert_eq!(tool_proxy_from_env_with(&env(&[])), None);
    }

    #[test]
    fn mcp_proxy_prefers_the_tool_proxy() {
        assert_eq!(
            mcp_proxy_from_env_with(
                &env(&[(LLM_PROXY_VAR, "http://llm:1")]),
                Some("http://tool:2")
            ),
            Some("http://tool:2".to_string())
        );
    }

    #[test]
    fn mcp_proxy_falls_back_to_the_llm_then_the_system_chain() {
        assert_eq!(
            mcp_proxy_from_env_with(&env(&[(LLM_PROXY_VAR, "http://llm:1")]), None),
            Some("http://llm:1".to_string())
        );
        assert_eq!(
            mcp_proxy_from_env_with(&env(&[("HTTPS_PROXY", "http://sys:2")]), None),
            Some("http://sys:2".to_string())
        );
        assert_eq!(
            mcp_proxy_from_env_with(&env(&[("https_proxy", "http://sys:3")]), None),
            Some("http://sys:3".to_string())
        );
        assert_eq!(mcp_proxy_from_env_with(&env(&[]), None), None);
    }
}
