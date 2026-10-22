//! LLM providers - OpenAI-compatible + Anthropic.

pub mod anthropic;
pub mod openai_compat;
pub mod sse;

pub use anthropic::AnthropicProvider;
pub use openai_compat::OpenAiCompat;

use crate::types::context::ProviderApi;

/// Connection + model config for an LLM provider, read from environment.
///
/// Recognized env vars (all optional unless noted):
/// - `CONGA_LLM_BASE_URL` - provider base URL (e.g. `https://api.deepseek.com/v1`)
/// - `CONGA_LLM_KEY`      - API key (auth bearer / x-api-key)
/// - `CONGA_LLM_MODEL`    - model id to call
/// - `CONGA_LLM_API`      - `openai` (default) or `anthropic` - wire protocol
/// - `CONGA_LLM_PROXY`    - proxy used for BOTH http and https (fallback)
/// - `CONGA_LLM_HTTP_PROXY`  - proxy for http requests (overrides CONGA_LLM_PROXY for http)
/// - `CONGA_LLM_HTTPS_PROXY` - proxy for https requests (overrides CONGA_LLM_PROXY for https)
///
/// Proxy precedence: per-scheme (`HTTP_PROXY`/`HTTPS_PROXY`) wins; `CONGA_LLM_PROXY`
/// fills in for whichever scheme has no explicit proxy.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Wire protocol the provider speaks. Determines which provider impl to
    /// build and how auth headers are sent.
    pub api: ProviderApi,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// A reqwest client with any configured proxies applied. Built once,
    /// shared by all calls.
    pub client: reqwest::Client,
}

/// Errors from building a [`ProviderConfig`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required env var: {0}")]
    Missing(&'static str),
    #[error("invalid proxy URL {var}={url}: {source}")]
    BadProxy {
        var: &'static str,
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("could not build HTTP client: {0}")]
    ClientBuild(reqwest::Error),
}

impl ProviderConfig {
    /// Read config from process environment.
    ///
    /// Requires `CONGA_LLM_BASE_URL`, `CONGA_LLM_KEY`, `CONGA_LLM_MODEL`.
    /// Proxies are optional.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with(&|k: &str| std::env::var(k))
    }

    /// Build a provider from explicit parts, applying the env proxy knobs
    /// (`CONGA_LLM_PROXY*`) for egress — the same precedence as
    /// [`from_env`](Self::from_env). Used by the host's settings file (the
    /// web UI persists LLM env overrides to `~/.conga/settings.json`).
    pub fn from_parts(
        api: ProviderApi,
        base_url: String,
        api_key: String,
        model: String,
    ) -> Result<Self, ConfigError> {
        let generic_proxy = std::env::var("CONGA_LLM_PROXY").ok();
        let http_proxy = std::env::var("CONGA_LLM_HTTP_PROXY").ok();
        let https_proxy = std::env::var("CONGA_LLM_HTTPS_PROXY").ok();
        let client = build_client(&http_proxy, &https_proxy, &generic_proxy)?;
        Ok(Self {
            api,
            base_url,
            api_key,
            model,
            client,
        })
    }

    /// [`from_parts`](Self::from_parts) with an explicit egress proxy — for
    /// embedded hosts that carry config as typed values and never touch the
    /// process environment (flow's in-executor headless assembly).
    pub fn from_parts_with_proxy(
        api: ProviderApi,
        base_url: String,
        api_key: String,
        model: String,
        generic_proxy: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let proxy = generic_proxy
            .filter(|p| !p.trim().is_empty())
            .map(str::to_string);
        let client = build_client(&None, &None, &proxy)?;
        Ok(Self {
            api,
            base_url,
            api_key,
            model,
            client,
        })
    }

    /// Read a prefixed provider config, e.g. prefix `CONGA_FAST_LLM` reads
    /// `CONGA_FAST_LLM_BASE_URL` / `_KEY` / `_MODEL` / `_API`. Used for the
    /// sub-agent "fast model" override; proxies fall back to the main
    /// `CONGA_LLM_PROXY*` knobs (a routing override rarely changes egress).
    /// `Ok(None)` when the prefix has NO vars set at all; a PARTIAL set
    /// (some vars present, a required one missing) is an `Err` so typos
    /// fail loud instead of silently falling back to the main model.
    pub fn from_env_prefixed(
        prefix: &str,
        lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    ) -> Result<Option<Self>, ConfigError> {
        let var = |suffix: &str| lookup(&format!("{prefix}_{suffix}")).ok();
        if var("BASE_URL").is_none()
            && var("KEY").is_none()
            && var("MODEL").is_none()
            && var("API").is_none()
        {
            return Ok(None);
        }
        let base_url = var("BASE_URL").ok_or(ConfigError::Missing("CONGA_FAST_LLM_BASE_URL"))?;
        let api_key = var("KEY").ok_or(ConfigError::Missing("CONGA_FAST_LLM_KEY"))?;
        let model = var("MODEL").ok_or(ConfigError::Missing("CONGA_FAST_LLM_MODEL"))?;
        let api = match var("API").as_deref() {
            Some("anthropic") => ProviderApi::Anthropic,
            _ => ProviderApi::OpenAiCompat,
        };
        // Proxy fallback: prefixed vars first, then the main LLM proxy knobs.
        let generic_proxy = var("PROXY").or_else(|| lookup("CONGA_LLM_PROXY").ok());
        let http_proxy = var("HTTP_PROXY").or_else(|| lookup("CONGA_LLM_HTTP_PROXY").ok());
        let https_proxy = var("HTTPS_PROXY").or_else(|| lookup("CONGA_LLM_HTTPS_PROXY").ok());
        let client = build_client(&http_proxy, &https_proxy, &generic_proxy)?;
        Ok(Some(Self {
            api,
            base_url,
            api_key,
            model,
            client,
        }))
    }

    /// Same as [`from_env`](Self::from_env) but with an injectable lookup -
    /// used by tests to avoid mutating process env.
    pub fn from_env_with(
        lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    ) -> Result<Self, ConfigError> {
        let base_url =
            lookup("CONGA_LLM_BASE_URL").map_err(|_| ConfigError::Missing("CONGA_LLM_BASE_URL"))?;
        let api_key = lookup("CONGA_LLM_KEY").map_err(|_| ConfigError::Missing("CONGA_LLM_KEY"))?;
        let model =
            lookup("CONGA_LLM_MODEL").map_err(|_| ConfigError::Missing("CONGA_LLM_MODEL"))?;
        let api = match lookup("CONGA_LLM_API").ok().as_deref() {
            Some("anthropic") => ProviderApi::Anthropic,
            _ => ProviderApi::OpenAiCompat,
        };

        let generic_proxy = lookup("CONGA_LLM_PROXY").ok();
        let http_proxy = lookup("CONGA_LLM_HTTP_PROXY").ok();
        let https_proxy = lookup("CONGA_LLM_HTTPS_PROXY").ok();

        let client = build_client(&http_proxy, &https_proxy, &generic_proxy)?;

        Ok(Self {
            api,
            base_url,
            api_key,
            model,
            client,
        })
    }
}

/// Build a reqwest client applying proxy precedence:
/// http scheme -> `http_proxy` else `generic`; https scheme -> `https_proxy`
/// else `generic`.
fn build_client(
    http_proxy: &Option<String>,
    https_proxy: &Option<String>,
    generic_proxy: &Option<String>,
) -> Result<reqwest::Client, ConfigError> {
    let mut builder = reqwest::Client::builder();

    let http_url = http_proxy.as_ref().or(generic_proxy.as_ref());
    if let Some(url) = http_url {
        let proxy = reqwest::Proxy::http(url).map_err(|e| ConfigError::BadProxy {
            var: "CONGA_LLM_HTTP_PROXY/CONGA_LLM_PROXY",
            url: url.clone(),
            source: e,
        })?;
        builder = builder.proxy(proxy);
    }

    let https_url = https_proxy.as_ref().or(generic_proxy.as_ref());
    if let Some(url) = https_url {
        let proxy = reqwest::Proxy::https(url).map_err(|e| ConfigError::BadProxy {
            var: "CONGA_LLM_HTTPS_PROXY/CONGA_LLM_PROXY",
            url: url.clone(),
            source: e,
        })?;
        builder = builder.proxy(proxy);
    }

    builder.build().map_err(ConfigError::ClientBuild)
}

use crate::types::message::ContentBlock;

/// Concatenate a message's `Text` content blocks into one plain string
/// (assistant content / tool results). Shared by both provider wire formats.
pub(crate) fn collect_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::fake_env;

    #[test]
    fn parses_required_vars() {
        let cfg = ProviderConfig::from_env_with(&fake_env(&[
            ("CONGA_LLM_BASE_URL", "https://api.x.com/v1"),
            ("CONGA_LLM_KEY", "sk-test"),
            ("CONGA_LLM_MODEL", "gpt-4o-mini"),
        ]))
        .unwrap();
        assert_eq!(cfg.base_url, "https://api.x.com/v1");
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.model, "gpt-4o-mini");
        assert_eq!(cfg.api, ProviderApi::OpenAiCompat); // default when CONGA_LLM_API unset
    }

    #[test]
    fn prefixed_loader_none_when_unset() {
        let r = ProviderConfig::from_env_prefixed("CONGA_FAST_LLM", &fake_env(&[])).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn prefixed_loader_full_set() {
        let r = ProviderConfig::from_env_prefixed(
            "CONGA_FAST_LLM",
            &fake_env(&[
                ("CONGA_FAST_LLM_BASE_URL", "https://fast.x.com/v1"),
                ("CONGA_FAST_LLM_KEY", "sk-fast"),
                ("CONGA_FAST_LLM_MODEL", "fast-model"),
                ("CONGA_FAST_LLM_API", "anthropic"),
            ]),
        )
        .unwrap();
        let cfg = r.expect("full set must load");
        assert_eq!(cfg.base_url, "https://fast.x.com/v1");
        assert_eq!(cfg.model, "fast-model");
        assert_eq!(cfg.api, ProviderApi::Anthropic);
    }

    #[test]
    fn prefixed_loader_partial_set_fails_loud() {
        // MODEL missing but BASE_URL present: a typo must be an error, not a
        // silent fallback to the main model.
        let r = ProviderConfig::from_env_prefixed(
            "CONGA_FAST_LLM",
            &fake_env(&[("CONGA_FAST_LLM_BASE_URL", "https://fast.x.com/v1")]),
        );
        assert!(r.is_err(), "partial set must fail: {r:?}");
    }

    #[test]
    fn api_selects_anthropic() {
        let cfg = ProviderConfig::from_env_with(&fake_env(&[
            ("CONGA_LLM_BASE_URL", "https://api.anthropic.com/v1"),
            ("CONGA_LLM_KEY", "sk-test"),
            ("CONGA_LLM_MODEL", "claude-sonnet"),
            ("CONGA_LLM_API", "anthropic"),
        ]))
        .unwrap();
        assert_eq!(cfg.api, ProviderApi::Anthropic);
    }

    #[test]
    fn missing_required_var_errors() {
        let r = ProviderConfig::from_env_with(&fake_env(&[
            ("CONGA_LLM_KEY", "sk-test"),
            ("CONGA_LLM_MODEL", "m"),
        ]));
        assert!(matches!(r, Err(ConfigError::Missing("CONGA_LLM_BASE_URL"))));
    }

    #[test]
    fn generic_proxy_applies_to_both_schemes() {
        // CONGA_LLM_PROXY alone -> client builds (http + https both proxied).
        let cfg = ProviderConfig::from_env_with(&fake_env(&[
            ("CONGA_LLM_BASE_URL", "https://api.x.com/v1"),
            ("CONGA_LLM_KEY", "k"),
            ("CONGA_LLM_MODEL", "m"),
            ("CONGA_LLM_PROXY", "http://localhost:8080"),
        ]))
        .unwrap();
        // No direct way to inspect proxies on a Client; building without error
        // and having a usable client is the contract.
        let _ = cfg.client;
    }

    #[test]
    fn https_proxy_overrides_generic_for_https() {
        // Both generic and https set - both accepted (https wins for https).
        let cfg = ProviderConfig::from_env_with(&fake_env(&[
            ("CONGA_LLM_BASE_URL", "https://api.x.com/v1"),
            ("CONGA_LLM_KEY", "k"),
            ("CONGA_LLM_MODEL", "m"),
            ("CONGA_LLM_PROXY", "http://generic:8080"),
            ("CONGA_LLM_HTTPS_PROXY", "http://specific:9090"),
        ]))
        .unwrap();
        let _ = cfg.client;
    }

    #[test]
    fn bad_proxy_url_errors() {
        let r = ProviderConfig::from_env_with(&fake_env(&[
            ("CONGA_LLM_BASE_URL", "https://api.x.com/v1"),
            ("CONGA_LLM_KEY", "k"),
            ("CONGA_LLM_MODEL", "m"),
            ("CONGA_LLM_PROXY", "not a url %"),
        ]));
        assert!(matches!(r, Err(ConfigError::BadProxy { .. })));
    }

    // ── ProviderConfig::from_env ──────────────────────────────────

    #[test]
    fn from_env_agrees_with_the_injected_process_lookup() {
        // `from_env` must be exactly `from_env_with(&std::env::var)` — no
        // hidden extra knobs, no divergence between the two entry points.
        let direct = ProviderConfig::from_env();
        let injected = ProviderConfig::from_env_with(&|k: &str| std::env::var(k));
        match (direct, injected) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a.api, b.api);
                assert_eq!(a.base_url, b.base_url);
                assert_eq!(a.api_key, b.api_key);
                assert_eq!(a.model, b.model);
            }
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
            _ => panic!("from_env and from_env_with must agree"),
        }
    }

    #[test]
    fn from_env_error_names_the_missing_variable() {
        // Whatever the machine's env holds, the failure mode must name the
        // variable that is missing — never a bare "invalid config".
        if let Err(e) = ProviderConfig::from_env() {
            let msg = e.to_string();
            assert!(
                msg.contains("CONGA_LLM_BASE_URL")
                    || msg.contains("CONGA_LLM_KEY")
                    || msg.contains("CONGA_LLM_MODEL"),
                "error must name the missing variable, got: {msg}"
            );
        }
    }

    // ── ProviderConfig::from_parts ────────────────────────────────

    #[test]
    fn from_parts_keeps_the_explicit_values() {
        let cfg = ProviderConfig::from_parts(
            ProviderApi::Anthropic,
            "https://api.anthropic.com/v1".to_string(),
            "sk-test".to_string(),
            "claude-sonnet".to_string(),
        )
        .expect("explicit parts must build");
        assert_eq!(cfg.api, ProviderApi::Anthropic);
        assert_eq!(cfg.base_url, "https://api.anthropic.com/v1");
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.model, "claude-sonnet");
    }

    #[test]
    fn from_parts_keeps_the_openai_api_selector() {
        let cfg = ProviderConfig::from_parts(
            ProviderApi::OpenAiCompat,
            "http://127.0.0.1:8080/v1".to_string(),
            "k".to_string(),
            "m".to_string(),
        )
        .expect("explicit parts must build");
        assert_eq!(cfg.api, ProviderApi::OpenAiCompat);
        assert_eq!(cfg.base_url, "http://127.0.0.1:8080/v1");
    }

    #[test]
    fn from_parts_accepts_an_empty_base_url() {
        // No validation happens at this layer: the host owns the value and
        // the failure (if any) surfaces on the first request.
        let cfg = ProviderConfig::from_parts(
            ProviderApi::OpenAiCompat,
            String::new(),
            String::new(),
            String::new(),
        )
        .expect("empty values must not panic");
        assert_eq!(cfg.base_url, "");
        assert_eq!(cfg.api_key, "");
        assert_eq!(cfg.model, "");
    }

    // ── build_client ──────────────────────────────────────────────

    #[test]
    fn build_client_without_any_proxy() {
        let client = build_client(&None, &None, &None).expect("no proxy must build");
        let _ = client;
    }

    #[test]
    fn build_client_with_a_generic_proxy() {
        let generic = Some("http://127.0.0.1:7890".to_string());
        let client = build_client(&None, &None, &generic).expect("generic proxy must build");
        let _ = client;
    }

    #[test]
    fn build_client_with_scheme_specific_proxies() {
        let http = Some("http://127.0.0.1:7890".to_string());
        let https = Some("http://127.0.0.1:7891".to_string());
        let client = build_client(&http, &https, &None).expect("scheme proxies must build");
        let _ = client;
    }

    #[test]
    fn build_client_with_only_an_http_proxy() {
        let http = Some("http://127.0.0.1:7890".to_string());
        let client = build_client(&http, &None, &None).expect("http-only proxy must build");
        let _ = client;
    }

    /// A port no ambient proxy uses, so the assertions below cannot be
    /// satisfied by the machine's own `HTTP_PROXY`/`HTTPS_PROXY`.
    const PROXY_MARKER_PORT: u16 = 18899;

    #[test]
    fn from_parts_with_proxy_installs_the_given_proxy() {
        let cfg = ProviderConfig::from_parts_with_proxy(
            ProviderApi::OpenAiCompat,
            "https://api.x.com/v1".into(),
            "sk-test".into(),
            "main-model".into(),
            Some(&format!("http://127.0.0.1:{PROXY_MARKER_PORT}")),
        )
        .expect("a valid proxy must build");
        assert_eq!(cfg.base_url, "https://api.x.com/v1");
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.model, "main-model");
        let dbg = format!("{:?}", cfg.client);
        assert!(
            dbg.contains("18899"),
            "the proxy must reach the client: {dbg}"
        );
    }

    #[test]
    fn from_parts_with_proxy_without_a_proxy_leaves_it_unproxied() {
        let cfg = ProviderConfig::from_parts_with_proxy(
            ProviderApi::Anthropic,
            "https://api.x.com".into(),
            "sk-test".into(),
            "m".into(),
            None,
        )
        .expect("no proxy must build");
        assert_eq!(cfg.api, ProviderApi::Anthropic);
        let dbg = format!("{:?}", cfg.client);
        assert!(
            !dbg.contains("18899"),
            "no proxy must be installed: {dbg}"
        );
    }

    #[test]
    fn from_parts_with_proxy_treats_a_blank_proxy_as_absent() {
        let cfg = ProviderConfig::from_parts_with_proxy(
            ProviderApi::OpenAiCompat,
            "https://api.x.com/v1".into(),
            "sk-test".into(),
            "m".into(),
            Some("   "),
        )
        .expect("a blank proxy must build");
        let dbg = format!("{:?}", cfg.client);
        assert!(
            !dbg.contains("18899"),
            "a blank proxy must not be installed: {dbg}"
        );
    }

    #[test]
    fn from_parts_with_proxy_rejects_an_unparsable_proxy() {
        let err = ProviderConfig::from_parts_with_proxy(
            ProviderApi::OpenAiCompat,
            "https://api.x.com/v1".into(),
            "sk-test".into(),
            "m".into(),
            Some("not a url"),
        )
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::BadProxy { .. }),
            "an unparsable proxy must fail loudly, got: {err}"
        );
    }

    #[test]
    fn build_client_with_only_an_https_proxy() {
        let https = Some("http://127.0.0.1:7891".to_string());
        let client = build_client(&None, &https, &None).expect("https-only proxy must build");
        let _ = client;
    }

    #[test]
    fn build_client_rejects_a_malformed_http_proxy() {
        let http = Some("not a url %".to_string());
        let err = build_client(&http, &None, &None).expect_err("malformed proxy must fail");
        match err {
            ConfigError::BadProxy { var, url, .. } => {
                assert!(var.contains("HTTP_PROXY"), "var must name the knob: {var}");
                assert_eq!(url, "not a url %");
            }
            other => panic!("expected BadProxy, got {other:?}"),
        }
    }

    #[test]
    fn build_client_rejects_a_malformed_https_proxy() {
        let https = Some("not a url %".to_string());
        let err = build_client(&None, &https, &None).expect_err("malformed proxy must fail");
        match err {
            ConfigError::BadProxy { var, url, .. } => {
                assert!(var.contains("HTTPS_PROXY"), "var must name the knob: {var}");
                assert_eq!(url, "not a url %");
            }
            other => panic!("expected BadProxy, got {other:?}"),
        }
    }

    #[test]
    fn build_client_error_message_names_the_offending_url() {
        let generic = Some("not a url %".to_string());
        let err = build_client(&None, &None, &generic).expect_err("malformed proxy must fail");
        let msg = err.to_string();
        assert!(msg.contains("not a url %"), "{msg}");
        assert!(msg.contains("CONGA_LLM_PROXY"), "{msg}");
    }

    // ── collect_text ──────────────────────────────────────────────

    #[test]
    fn collect_text_on_an_empty_block_list() {
        assert_eq!(collect_text(&[]), "");
    }

    #[test]
    fn collect_text_joins_the_text_blocks_without_a_separator() {
        let blocks = vec![
            ContentBlock::text("a"),
            ContentBlock::text("b"),
            ContentBlock::text("c"),
        ];
        assert_eq!(collect_text(&blocks), "abc");
    }

    #[test]
    fn collect_text_skips_thinking_and_tool_call_blocks() {
        use crate::types::message::{FunctionCall, ToolCall};
        let blocks = vec![
            ContentBlock::text("visible"),
            ContentBlock::Thinking {
                thinking: "hidden".to_string(),
            },
            ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: "t1".to_string(),
                    function: FunctionCall {
                        name: "bash".to_string(),
                        arguments: "{}".to_string(),
                    },
                },
            },
            ContentBlock::text(" tail"),
        ];
        assert_eq!(collect_text(&blocks), "visible tail");
    }

    #[test]
    fn collect_text_keeps_whitespace_and_newlines_verbatim() {
        let blocks = vec![ContentBlock::text("  spaced  "), ContentBlock::text("\n")];
        assert_eq!(collect_text(&blocks), "  spaced  \n");
    }

    #[test]
    fn collect_text_with_an_empty_text_block() {
        assert_eq!(collect_text(&[ContentBlock::text("")]), "");
    }
}
