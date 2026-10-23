//! `config.toml`: file-based base config layered under the environment.
//!
//! Layering, lowest → highest:
//! 1. `config.toml` (this module) — `./.conga/config.toml` or `~/.conga/config.toml`
//! 2. `.env` (dotenvy — loaded here FIRST, so it wins over the file)
//! 3. real process env (always wins: we only `set_var` what is unset)
//! 4. `~/.conga/settings.json` (web UI, re-read every turn — separate layer)
//!
//! Discovery: `CONGA_CONFIG` (fail-loud when the file is missing) →
//! `./.conga/config.toml` → `~/.conga/config.toml`; first existing file wins.
//! No file anywhere → no-op (env-only, backward compatible).
//!
//! Sections mirror the `CONGA_*` env vars documented in `.env.example`;
//! see `config.example.toml` for a full annotated template. Unknown keys
//! are a parse error (typo protection). Empty string values are treated
//! as "unset" so templates can ship placeholder blanks.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Errors from loading `config.toml`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigFileError {
    #[error("CONGA_CONFIG is set but the file does not exist: {0}")]
    ExplicitMissing(PathBuf),
    #[error("cannot read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config.toml {path} (unknown key or bad value): {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// The whole `config.toml`. Every section is optional; every key optional.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigFile {
    pub llm: Option<LlmSection>,
    pub fast_llm: Option<LlmSection>,
    pub proxy: Option<ProxySection>,
    pub tunables: Option<TunablesSection>,
    pub gateway: Option<GatewaySection>,
    pub compact: Option<CompactSection>,
    pub tools: Option<ToolsSection>,
    pub search: Option<SearchSection>,
    pub rag: Option<RagSection>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmSection {
    pub base_url: Option<String>,
    pub key: Option<String>,
    pub model: Option<String>,
    /// `openai` (default) or `anthropic`.
    pub api: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxySection {
    /// LLM egress proxy for both http and https (`CONGA_LLM_PROXY`).
    pub llm: Option<String>,
    pub llm_http: Option<String>,
    pub llm_https: Option<String>,
    /// Tool traffic proxy for fetch / web_search (`CONGA_TOOL_PROXY`).
    pub tool: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TunablesSection {
    pub max_turns: Option<u64>,
    pub max_tool_calls: Option<u64>,
    pub max_tokens: Option<u64>,
    pub retry_max: Option<u64>,
    pub retry_initial_ms: Option<u64>,
    pub retry_max_ms: Option<u64>,
    pub tool_timeout_s: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewaySection {
    pub port: Option<u64>,
    pub host: Option<String>,
    /// suggest | auto-edit | full-auto | plan.
    pub mode: Option<String>,
    pub approval_timeout_s: Option<u64>,
    pub token: Option<String>,
    /// Comma-separated extra CORS origins.
    pub cors_origins: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactSection {
    pub context_window: Option<u64>,
    pub threshold_pct: Option<u64>,
    pub target_pct: Option<u64>,
    pub max_messages: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsSection {
    /// Comma-separated external command whitelist.
    pub external_tools: Option<String>,
    /// MCP config file path (default `~/.conga/mcp.json`).
    pub mcp_config: Option<String>,
    pub mcp_call_timeout_s: Option<u64>,
    /// Disable the fetch tool's SSRF guard (trusted LANs only).
    pub fetch_allow_private_net: Option<bool>,
    /// Run bash tool commands in a filesystem sandbox.
    pub sandbox: Option<bool>,
    /// Project dir: tool sandbox root + project skills base.
    pub project_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchSection {
    pub provider: Option<String>,
    pub brave_api_key: Option<String>,
    pub tavily_api_key: Option<String>,
    pub serper_api_key: Option<String>,
    pub serpapi_api_key: Option<String>,
    pub exa_api_key: Option<String>,
    pub firecrawl_api_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RagSection {
    /// conga-rag config file (overrides `./rag.toml` discovery).
    pub config: Option<String>,
    pub embed_base_url: Option<String>,
    pub embed_key: Option<String>,
    pub embed_model: Option<String>,
    pub embed_batch: Option<u64>,
    pub store_path: Option<String>,
}

impl ConfigFile {
    /// Flatten to `(env var name, value)` pairs, skipping unset/empty keys.
    pub fn env_pairs(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(s) = &self.llm {
            s.push(&mut out, "CONGA_LLM");
        }
        if let Some(s) = &self.fast_llm {
            s.push(&mut out, "CONGA_FAST_LLM");
        }
        if let Some(s) = &self.proxy {
            s.push(&mut out);
        }
        if let Some(s) = &self.tunables {
            s.push(&mut out);
        }
        if let Some(s) = &self.gateway {
            s.push(&mut out);
        }
        if let Some(s) = &self.compact {
            s.push(&mut out);
        }
        if let Some(s) = &self.tools {
            s.push(&mut out);
        }
        if let Some(s) = &self.search {
            s.push(&mut out);
        }
        if let Some(s) = &self.rag {
            s.push(&mut out);
        }
        out
    }
}

/// String env values with an env-name prefix (`CONGA_LLM` + `_BASE_URL`).
impl LlmSection {
    fn push(&self, out: &mut Vec<(String, String)>, prefix: &str) {
        push_str(out, &format!("{prefix}_BASE_URL"), &self.base_url);
        push_str(out, &format!("{prefix}_KEY"), &self.key);
        push_str(out, &format!("{prefix}_MODEL"), &self.model);
        push_str(out, &format!("{prefix}_API"), &self.api);
    }
}

impl ProxySection {
    fn push(&self, out: &mut Vec<(String, String)>) {
        push_str(out, "CONGA_LLM_PROXY", &self.llm);
        push_str(out, "CONGA_LLM_HTTP_PROXY", &self.llm_http);
        push_str(out, "CONGA_LLM_HTTPS_PROXY", &self.llm_https);
        push_str(out, "CONGA_TOOL_PROXY", &self.tool);
    }
}

impl TunablesSection {
    fn push(&self, out: &mut Vec<(String, String)>) {
        push_num(out, "CONGA_MAX_TURNS", &self.max_turns);
        push_num(out, "CONGA_MAX_TOOL_CALLS", &self.max_tool_calls);
        push_num(out, "CONGA_MAX_TOKENS", &self.max_tokens);
        push_num(out, "CONGA_RETRY_MAX", &self.retry_max);
        push_num(out, "CONGA_RETRY_INITIAL_MS", &self.retry_initial_ms);
        push_num(out, "CONGA_RETRY_MAX_MS", &self.retry_max_ms);
        push_num(out, "CONGA_TOOL_TIMEOUT_S", &self.tool_timeout_s);
    }
}

impl GatewaySection {
    fn push(&self, out: &mut Vec<(String, String)>) {
        push_num(out, "CONGA_GATEWAY_PORT", &self.port);
        push_str(out, "CONGA_GATEWAY_HOST", &self.host);
        push_str(out, "CONGA_GATEWAY_MODE", &self.mode);
        push_num(out, "CONGA_APPROVAL_TIMEOUT_S", &self.approval_timeout_s);
        push_str(out, "CONGA_GATEWAY_TOKEN", &self.token);
        push_str(out, "CONGA_GATEWAY_CORS_ORIGINS", &self.cors_origins);
    }
}

impl CompactSection {
    fn push(&self, out: &mut Vec<(String, String)>) {
        push_num(out, "CONGA_CONTEXT_WINDOW", &self.context_window);
        push_num(out, "CONGA_COMPACT_THRESHOLD_PCT", &self.threshold_pct);
        push_num(out, "CONGA_COMPACT_TARGET_PCT", &self.target_pct);
        push_num(out, "CONGA_COMPACT_MAX_MESSAGES", &self.max_messages);
    }
}

impl ToolsSection {
    fn push(&self, out: &mut Vec<(String, String)>) {
        push_str(out, "CONGA_EXTERNAL_TOOLS", &self.external_tools);
        push_str(out, "CONGA_MCP_CONFIG", &self.mcp_config);
        push_num(out, "CONGA_MCP_CALL_TIMEOUT_S", &self.mcp_call_timeout_s);
        push_bool(
            out,
            "CONGA_FETCH_ALLOW_PRIVATE_NET",
            &self.fetch_allow_private_net,
        );
        push_bool(out, "CONGA_SANDBOX", &self.sandbox);
        push_str(out, "CONGA_PROJECT_DIR", &self.project_dir);
    }
}

impl SearchSection {
    fn push(&self, out: &mut Vec<(String, String)>) {
        push_str(out, "CONGA_SEARCH_PROVIDER", &self.provider);
        push_str(out, "CONGA_BRAVE_API_KEY", &self.brave_api_key);
        push_str(out, "CONGA_TAVILY_API_KEY", &self.tavily_api_key);
        push_str(out, "CONGA_SERPER_API_KEY", &self.serper_api_key);
        push_str(out, "CONGA_SERPAPI_API_KEY", &self.serpapi_api_key);
        push_str(out, "CONGA_EXA_API_KEY", &self.exa_api_key);
        push_str(out, "CONGA_FIRECRAWL_API_KEY", &self.firecrawl_api_key);
    }
}

impl RagSection {
    fn push(&self, out: &mut Vec<(String, String)>) {
        push_str(out, "CONGA_RAG_CONFIG", &self.config);
        push_str(out, "CONGA_RAG_EMBED_BASE_URL", &self.embed_base_url);
        push_str(out, "CONGA_RAG_EMBED_KEY", &self.embed_key);
        push_str(out, "CONGA_RAG_EMBED_MODEL", &self.embed_model);
        push_num(out, "CONGA_RAG_EMBED_BATCH", &self.embed_batch);
        push_str(out, "CONGA_RAG_STORE_PATH", &self.store_path);
    }
}

fn push_str(out: &mut Vec<(String, String)>, name: &str, value: &Option<String>) {
    // Empty string = unset (templates may ship placeholder blanks).
    if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
        out.push((name.to_string(), v.to_string()));
    }
}

fn push_num(out: &mut Vec<(String, String)>, name: &str, value: &Option<u64>) {
    if let Some(v) = value {
        out.push((name.to_string(), v.to_string()));
    }
}

/// Bools serialize as "1"/"0" — every parse site checks `== "1"`
/// (fetch also accepts "true", so "1" is the safe common form).
fn push_bool(out: &mut Vec<(String, String)>, name: &str, value: &Option<bool>) {
    if let Some(v) = value {
        out.push((name.to_string(), if *v { "1" } else { "0" }.to_string()));
    }
}

/// Load `.conga/config.toml` and apply it as the base config layer.
///
/// Runs dotenv first (`.env` must win over the file), then sets each config
/// value as an env var ONLY when that var is not already set (env overrides
/// the file). Call this once at the top of `main`, before any `CONGA_*`
/// env read. `Ok(None)` = no config file found (env-only startup).
pub fn apply() -> Result<Option<PathBuf>, ConfigFileError> {
    let _ = dotenvy::dotenv(); // .env fills first → it overrides config.toml
    let lookup = |k: &str| std::env::var(k);
    let path = match discover(
        &lookup,
        Path::new(".conga/config.toml"),
        dirs::home_dir().as_deref(),
    )? {
        Some(p) => p,
        None => return Ok(None),
    };
    let raw = std::fs::read_to_string(&path).map_err(|e| ConfigFileError::Read {
        path: path.clone(),
        source: e,
    })?;
    let cfg: ConfigFile = toml::from_str(&raw).map_err(|e| ConfigFileError::Parse {
        path: path.clone(),
        source: e,
    })?;
    overlay(&cfg.env_pairs(), &lookup, &|k, v| std::env::set_var(k, v));
    Ok(Some(path))
}

/// `CONGA_CONFIG` (fail-loud when missing) → `project` → `home/.conga/config.toml`.
fn discover(
    env: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    project: &Path,
    home: Option<&Path>,
) -> Result<Option<PathBuf>, ConfigFileError> {
    if let Ok(p) = env("CONGA_CONFIG") {
        let p = PathBuf::from(&p);
        if p.exists() {
            return Ok(Some(p));
        }
        return Err(ConfigFileError::ExplicitMissing(p));
    }
    if project.exists() {
        return Ok(Some(project.to_path_buf()));
    }
    Ok(home
        .map(|h| h.join(".conga/config.toml"))
        .filter(|p| p.exists()))
}

/// Set each pair only when the env var is unset — env wins.
fn overlay(
    pairs: &[(String, String)],
    get: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    set: &dyn Fn(&str, &str),
) {
    for (k, v) in pairs {
        if get(k).is_err() {
            set(k, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn no_env(_: &str) -> Result<String, std::env::VarError> {
        Err(std::env::VarError::NotPresent)
    }

    fn fake_env(
        pairs: &[(&str, &str)],
    ) -> impl Fn(&str) -> Result<String, std::env::VarError> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    const FULL: &str = r#"
[llm]
base_url = "https://api.x.com/v1"
key = "sk-1"
model = "m1"
api = "anthropic"

[fast_llm]
base_url = "https://fast.x.com/v1"
key = "sk-2"
model = "m2"

[proxy]
llm = "http://p:8080"
tool = "socks5://127.0.0.1:1080"

[tunables]
max_turns = 7
max_tool_calls = 8
max_tokens = 123
retry_max = 3
retry_initial_ms = 100
retry_max_ms = 2000
tool_timeout_s = 30

[gateway]
port = 3000
host = "0.0.0.0"
mode = "plan"
approval_timeout_s = 60
token = "t"
cors_origins = "https://a.example"

[compact]
context_window = 64000
threshold_pct = 75
target_pct = 40
max_messages = 20

[tools]
external_tools = "rg,jq"
mcp_config = "/tmp/mcp.json"
mcp_call_timeout_s = 90
fetch_allow_private_net = true
sandbox = true
project_dir = "/tmp/proj"

[search]
provider = "brave"
brave_api_key = "bk"
tavily_api_key = "tk"
serper_api_key = "sp"
serpapi_api_key = "spp"
exa_api_key = "ex"
firecrawl_api_key = "fc"

[rag]
config = "/tmp/rag.toml"
embed_base_url = "https://e.x.com/v1"
embed_key = "ek"
embed_model = "emb-1"
embed_batch = 8
store_path = "/tmp/index.db"
"#;

    fn names(cfg: &ConfigFile) -> Vec<String> {
        cfg.env_pairs().into_iter().map(|(k, _)| k).collect()
    }

    #[test]
    fn parses_and_maps_every_section() {
        let cfg: ConfigFile = toml::from_str(FULL).unwrap();
        let pairs = cfg.env_pairs();
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("missing {k}"))
        };
        assert_eq!(get("CONGA_LLM_BASE_URL"), "https://api.x.com/v1");
        assert_eq!(get("CONGA_LLM_API"), "anthropic");
        assert_eq!(get("CONGA_FAST_LLM_MODEL"), "m2");
        assert_eq!(get("CONGA_LLM_PROXY"), "http://p:8080");
        assert_eq!(get("CONGA_TOOL_PROXY"), "socks5://127.0.0.1:1080");
        assert_eq!(get("CONGA_MAX_TURNS"), "7");
        assert_eq!(get("CONGA_TOOL_TIMEOUT_S"), "30");
        assert_eq!(get("CONGA_GATEWAY_PORT"), "3000");
        assert_eq!(get("CONGA_GATEWAY_MODE"), "plan");
        assert_eq!(get("CONGA_APPROVAL_TIMEOUT_S"), "60");
        assert_eq!(get("CONGA_CONTEXT_WINDOW"), "64000");
        assert_eq!(get("CONGA_COMPACT_MAX_MESSAGES"), "20");
        assert_eq!(get("CONGA_EXTERNAL_TOOLS"), "rg,jq");
        assert_eq!(get("CONGA_FETCH_ALLOW_PRIVATE_NET"), "1");
        assert_eq!(get("CONGA_SANDBOX"), "1");
        assert_eq!(get("CONGA_PROJECT_DIR"), "/tmp/proj");
        assert_eq!(get("CONGA_SEARCH_PROVIDER"), "brave");
        assert_eq!(get("CONGA_FIRECRAWL_API_KEY"), "fc");
        assert_eq!(get("CONGA_RAG_CONFIG"), "/tmp/rag.toml");
        assert_eq!(get("CONGA_RAG_EMBED_BATCH"), "8");
        // Complete coverage: 4+3+2+7+6+4+6+7+6 = 45 pairs (fast_llm omits api).
        assert_eq!(pairs.len(), 45, "all keys mapped: {:?}", names(&cfg));
    }

    #[test]
    fn empty_file_is_noop() {
        let cfg: ConfigFile = toml::from_str("").unwrap();
        assert!(cfg.env_pairs().is_empty());
    }

    #[test]
    fn empty_string_values_are_skipped() {
        let cfg: ConfigFile = toml::from_str(
            r#"
[llm]
base_url = "https://api.x.com/v1"
key = ""
"#,
        )
        .unwrap();
        let pairs = cfg.env_pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "CONGA_LLM_BASE_URL");
    }

    #[test]
    fn false_bool_serializes_as_zero() {
        let cfg: ConfigFile =
            toml::from_str("[tools]\nsandbox = false\nfetch_allow_private_net = true\n").unwrap();
        let pairs = cfg.env_pairs();
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).unwrap().1.clone();
        assert_eq!(get("CONGA_SANDBOX"), "0");
        assert_eq!(get("CONGA_FETCH_ALLOW_PRIVATE_NET"), "1");
    }

    #[test]
    fn unknown_key_fails_loud() {
        let e = toml::from_str::<ConfigFile>("[llm]\nbase_urL = \"typo\"\n").unwrap_err();
        assert!(e.to_string().contains("base_urL"), "must name the key: {e}");
        let e = toml::from_str::<ConfigFile>("[not_a_section]\nx = 1\n").unwrap_err();
        assert!(
            e.to_string().contains("not_a_section"),
            "must name the section: {e}"
        );
    }

    #[test]
    fn overlay_env_wins_and_unset_gets_set() {
        let pairs = vec![
            ("CONGA_LLM_MODEL".to_string(), "file-model".to_string()),
            ("CONGA_MAX_TURNS".to_string(), "9".to_string()),
        ];
        let set = std::cell::RefCell::new(Vec::new());
        overlay(
            &pairs,
            &fake_env(&[("CONGA_LLM_MODEL", "env-model")]),
            &|k, v| set.borrow_mut().push((k.to_string(), v.to_string())),
        );
        // Env-set var untouched; unset var applied from the file.
        assert_eq!(
            set.into_inner(),
            vec![("CONGA_MAX_TURNS".to_string(), "9".to_string())]
        );
    }

    #[test]
    fn discover_order_explicit_project_home() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join(".conga/config.toml");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, "").unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".conga")).unwrap();
        std::fs::write(home.join(".conga/config.toml"), "").unwrap();

        // 1. CONGA_CONFIG wins when the file exists.
        let explicit = tmp.path().join("explicit.toml");
        std::fs::write(&explicit, "").unwrap();
        let found = discover(
            &fake_env(&[("CONGA_CONFIG", explicit.to_str().unwrap())]),
            &project,
            Some(&home),
        )
        .unwrap();
        assert_eq!(found.as_deref(), Some(explicit.as_path()));

        // 2. Project-level beats home.
        let found = discover(&no_env, &project, Some(&home)).unwrap();
        assert_eq!(found.as_deref(), Some(project.as_path()));

        // 3. Home fallback when no project file.
        let found = discover(&no_env, &tmp.path().join("no-such"), Some(&home)).unwrap();
        assert_eq!(
            found.as_deref(),
            Some(home.join(".conga/config.toml").as_path())
        );

        // 4. Nothing anywhere → None.
        let empty_home = tmp.path().join("empty-home");
        std::fs::create_dir_all(&empty_home).unwrap();
        assert!(
            discover(&no_env, &tmp.path().join("no-such"), Some(&empty_home))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn discover_fails_loud_when_explicit_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-config.toml");
        let err = discover(
            &fake_env(&[("CONGA_CONFIG", missing.to_str().unwrap())]),
            Path::new("no-such"),
            None,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("CONGA_CONFIG"), "must name the var: {msg}");
        assert!(
            msg.contains("no-such-config.toml"),
            "must name the path: {msg}"
        );
    }

    #[test]
    fn shipped_example_file_parses() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../config.example.toml"
        ))
        .expect("config.example.toml must exist next to the workspace");
        let cfg: ConfigFile =
            toml::from_str(&raw).expect("example must parse (unknown key = drift)");
        // Sanity: the example actually configures the required LLM trio.
        let names = cfg.env_pairs();
        for k in ["CONGA_LLM_BASE_URL", "CONGA_LLM_KEY", "CONGA_LLM_MODEL"] {
            assert!(names.iter().any(|(n, _)| n == k), "missing {k}");
        }
    }

    // ---- push_str / push_num / push_bool -------------------------------

    /// `push_str` skips a missing value and an EMPTY string (templates may
    /// ship placeholder blanks), but keeps any other string - including
    /// whitespace-only ones.
    #[test]
    fn push_str_skips_missing_and_empty_values() {
        let mut out = Vec::new();
        push_str(&mut out, "CONGA_MISSING", &None);
        push_str(&mut out, "CONGA_EMPTY", &Some(String::new()));
        push_str(&mut out, "CONGA_BLANK", &Some("   ".to_string()));
        push_str(&mut out, "CONGA_SET", &Some("value".to_string()));
        assert_eq!(
            out,
            vec![
                ("CONGA_BLANK".to_string(), "   ".to_string()),
                ("CONGA_SET".to_string(), "value".to_string()),
            ]
        );
    }

    /// `push_num` renders the number as its decimal string; zero is a real
    /// value (not "unset") and is kept.
    #[test]
    fn push_num_renders_zero_and_max() {
        let mut out = Vec::new();
        push_num(&mut out, "CONGA_NONE", &None);
        push_num(&mut out, "CONGA_ZERO", &Some(0));
        push_num(&mut out, "CONGA_MAX", &Some(u64::MAX));
        assert_eq!(
            out,
            vec![
                ("CONGA_ZERO".to_string(), "0".to_string()),
                ("CONGA_MAX".to_string(), u64::MAX.to_string()),
            ]
        );
    }

    /// `push_bool` renders `true` as "1" and `false` as "0" - the form every
    /// parse site checks.
    #[test]
    fn push_bool_renders_one_and_zero() {
        let mut out = Vec::new();
        push_bool(&mut out, "CONGA_NONE", &None);
        push_bool(&mut out, "CONGA_TRUE", &Some(true));
        push_bool(&mut out, "CONGA_FALSE", &Some(false));
        assert_eq!(
            out,
            vec![
                ("CONGA_TRUE".to_string(), "1".to_string()),
                ("CONGA_FALSE".to_string(), "0".to_string()),
            ]
        );
    }

    /// A duplicate key inside one TOML table is a parse error, not a
    /// silent "last one wins".
    #[test]
    fn duplicate_key_in_one_table_fails_loud() {
        let e = toml::from_str::<ConfigFile>(
            "[llm]\nbase_url = \"https://a\"\nbase_url = \"https://b\"\n",
        )
        .unwrap_err();
        assert!(
            e.to_string().contains("base_url"),
            "must name the duplicated key: {e}"
        );
    }

    /// Every section pushes its own env names - checked directly on the
    /// section helpers, not only through `env_pairs`.
    #[test]
    fn every_section_pushes_its_own_env_names() {
        let mut out = Vec::new();
        LlmSection {
            base_url: Some("u".into()),
            key: Some("k".into()),
            model: Some("m".into()),
            api: Some("anthropic".into()),
        }
        .push(&mut out, "CONGA_LLM");
        assert_eq!(
            out,
            vec![
                ("CONGA_LLM_BASE_URL".to_string(), "u".to_string()),
                ("CONGA_LLM_KEY".to_string(), "k".to_string()),
                ("CONGA_LLM_MODEL".to_string(), "m".to_string()),
                ("CONGA_LLM_API".to_string(), "anthropic".to_string()),
            ]
        );

        let mut out = Vec::new();
        ProxySection {
            llm: Some("http://p".into()),
            llm_http: None,
            llm_https: None,
            tool: Some("socks5://t".into()),
        }
        .push(&mut out);
        assert_eq!(
            out,
            vec![
                ("CONGA_LLM_PROXY".to_string(), "http://p".to_string()),
                ("CONGA_TOOL_PROXY".to_string(), "socks5://t".to_string()),
            ]
        );

        let mut out = Vec::new();
        TunablesSection {
            max_turns: Some(1),
            max_tool_calls: None,
            max_tokens: Some(2),
            retry_max: None,
            retry_initial_ms: None,
            retry_max_ms: None,
            tool_timeout_s: None,
        }
        .push(&mut out);
        assert_eq!(
            out,
            vec![
                ("CONGA_MAX_TURNS".to_string(), "1".to_string()),
                ("CONGA_MAX_TOKENS".to_string(), "2".to_string()),
            ]
        );

        let mut out = Vec::new();
        GatewaySection {
            port: Some(3000),
            host: Some("0.0.0.0".into()),
            mode: Some("plan".into()),
            approval_timeout_s: None,
            token: None,
            cors_origins: None,
        }
        .push(&mut out);
        assert_eq!(
            out,
            vec![
                ("CONGA_GATEWAY_PORT".to_string(), "3000".to_string()),
                ("CONGA_GATEWAY_HOST".to_string(), "0.0.0.0".to_string()),
                ("CONGA_GATEWAY_MODE".to_string(), "plan".to_string()),
            ]
        );

        let mut out = Vec::new();
        CompactSection {
            context_window: Some(64000),
            threshold_pct: None,
            target_pct: None,
            max_messages: None,
        }
        .push(&mut out);
        assert_eq!(
            out,
            vec![("CONGA_CONTEXT_WINDOW".to_string(), "64000".to_string())]
        );

        let mut out = Vec::new();
        ToolsSection {
            external_tools: None,
            mcp_config: None,
            mcp_call_timeout_s: Some(90),
            fetch_allow_private_net: Some(false),
            sandbox: Some(true),
            project_dir: None,
        }
        .push(&mut out);
        assert_eq!(
            out,
            vec![
                ("CONGA_MCP_CALL_TIMEOUT_S".to_string(), "90".to_string()),
                ("CONGA_FETCH_ALLOW_PRIVATE_NET".to_string(), "0".to_string()),
                ("CONGA_SANDBOX".to_string(), "1".to_string()),
            ]
        );

        let mut out = Vec::new();
        SearchSection {
            provider: Some("brave".into()),
            brave_api_key: Some("bk".into()),
            tavily_api_key: None,
            serper_api_key: None,
            serpapi_api_key: None,
            exa_api_key: None,
            firecrawl_api_key: None,
        }
        .push(&mut out);
        assert_eq!(
            out,
            vec![
                ("CONGA_SEARCH_PROVIDER".to_string(), "brave".to_string()),
                ("CONGA_BRAVE_API_KEY".to_string(), "bk".to_string()),
            ]
        );

        let mut out = Vec::new();
        RagSection {
            config: None,
            embed_base_url: None,
            embed_key: None,
            embed_model: None,
            embed_batch: Some(8),
            store_path: None,
        }
        .push(&mut out);
        assert_eq!(
            out,
            vec![("CONGA_RAG_EMBED_BATCH".to_string(), "8".to_string())]
        );
    }

    // ---- apply() ---------------------------------------------------------

    /// Env vars the child probe reports back to the parent test.
    const PROBE_ENV_KEYS: &[&str] = &[
        "CONGA_PROJECT_DIR",
        "CONGA_MAX_TURNS",
        "CONGA_SANDBOX",
        "CONGA_LLM_MODEL",
    ];

    /// What the child process reported about its `apply()` call.
    struct ApplyReport {
        /// `ok:<path>`, `ok:none` or `err:<message>`.
        outcome: String,
        /// The env vars the child saw after `apply()` ran.
        env: Vec<(String, String)>,
    }

    impl ApplyReport {
        fn env_of(&self, key: &str) -> Option<&str> {
            self.env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        }
    }

    /// `apply()` mutates the *process* environment and reads the user's home
    /// directory, so exercising it in-process would leak state into the rest
    /// of the test binary. The probe therefore runs in a hermetic child
    /// process (fresh HOME, fresh working directory, cleared environment)
    /// and prints a machine-readable report the parent asserts on.
    #[test]
    #[ignore = "helper spawned as a child process by the apply_* tests"]
    fn apply_probe() {
        // The parent marks the child through the environment; without the
        // marker this test does nothing.
        if std::env::var("CONGA_TEST_APPLY_PROBE").is_err() {
            return;
        }
        let outcome = match apply() {
            Ok(Some(path)) => format!("ok:{}", path.display()),
            Ok(None) => "ok:none".to_string(),
            Err(e) => format!("err:{e}"),
        };
        println!("APPLY_OUTCOME={outcome}");
        for key in PROBE_ENV_KEYS {
            let value = std::env::var(key).unwrap_or_else(|_| "<unset>".to_string());
            println!("APPLY_ENV {key}={value}");
        }
    }

    /// Run `apply()` in a hermetic child process and collect its report.
    fn run_apply_in_child(home: &Path, cwd: &Path, env: &[(&str, &str)]) -> ApplyReport {
        let exe = std::env::current_exe().expect("test binary path");
        let mut cmd = std::process::Command::new(exe);
        cmd.args([
            "--ignored",
            "--exact",
            "config_file::tests::apply_probe",
            "--nocapture",
            "--test-threads=1",
        ])
        // A cleared environment: no inherited CONGA_* vars, no HOME leak.
        .env_clear()
        .env("HOME", home)
        .env("CONGA_TEST_APPLY_PROBE", "1")
        .current_dir(cwd);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("spawn apply probe");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "apply probe failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            out.status
        );
        let mut env_out = Vec::new();
        for line in stdout.lines() {
            if let Some(idx) = line.find("APPLY_ENV ") {
                let rest = &line[idx + "APPLY_ENV ".len()..];
                if let Some((k, v)) = rest.split_once('=') {
                    env_out.push((k.to_string(), v.to_string()));
                }
            }
        }
        // The outcome may span several lines (a TOML parse error prints a
        // snippet), so it runs until the first env marker.
        let outcome = match stdout.find("APPLY_OUTCOME=") {
            Some(idx) => {
                let rest = &stdout[idx + "APPLY_OUTCOME=".len()..];
                let end = rest.find("\nAPPLY_ENV ").unwrap_or(rest.len());
                rest[..end].trim().to_string()
            }
            None => {
                panic!("apply probe must print APPLY_OUTCOME\nstdout:\n{stdout}\nstderr:\n{stderr}")
            }
        };
        ApplyReport {
            outcome,
            env: env_out,
        }
    }

    /// Write a config file and return its path.
    fn write_config(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Happy path: the config file is applied to the process environment.
    #[test]
    fn apply_sets_config_values_into_the_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let cfg = write_config(
            tmp.path(),
            "config.toml",
            "[tools]\nproject_dir = \"/tmp/proj\"\nsandbox = true\n\n[tunables]\nmax_turns = 7\n",
        );

        let report = run_apply_in_child(&home, &cwd, &[("CONGA_CONFIG", cfg.to_str().unwrap())]);
        assert_eq!(
            report.outcome,
            format!("ok:{}", cfg.display()),
            "apply must report the file it applied"
        );
        assert_eq!(report.env_of("CONGA_PROJECT_DIR"), Some("/tmp/proj"));
        assert_eq!(report.env_of("CONGA_MAX_TURNS"), Some("7"));
        assert_eq!(report.env_of("CONGA_SANDBOX"), Some("1"));
    }

    /// The real process environment wins over the file: an already-set var
    /// is never overwritten.
    #[test]
    fn apply_keeps_an_already_set_env_value() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let cfg = write_config(
            tmp.path(),
            "config.toml",
            "[tools]\nproject_dir = \"/tmp/from-file\"\n",
        );

        let report = run_apply_in_child(
            &home,
            &cwd,
            &[
                ("CONGA_CONFIG", cfg.to_str().unwrap()),
                ("CONGA_PROJECT_DIR", "/tmp/from-env"),
            ],
        );
        assert!(report.outcome.starts_with("ok:"), "{}", report.outcome);
        assert_eq!(
            report.env_of("CONGA_PROJECT_DIR"),
            Some("/tmp/from-env"),
            "an already-set env var must not be overwritten by the file"
        );
    }

    /// A missing `CONGA_CONFIG` file fails loud - it never falls back to the
    /// project/home discovery.
    #[test]
    fn apply_fails_loud_when_the_explicit_config_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        // A project-level file exists but must NOT be used as a fallback.
        write_config(
            &cwd,
            ".conga/config.toml",
            "[tools]\nproject_dir = \"/p\"\n",
        );
        let missing = tmp.path().join("no-such-config.toml");

        let report =
            run_apply_in_child(&home, &cwd, &[("CONGA_CONFIG", missing.to_str().unwrap())]);
        assert!(report.outcome.starts_with("err:"), "{}", report.outcome);
        assert!(
            report.outcome.contains("CONGA_CONFIG"),
            "must name the var: {}",
            report.outcome
        );
        assert!(
            report.outcome.contains("no-such-config.toml"),
            "must name the path: {}",
            report.outcome
        );
    }

    /// A malformed config file is a parse error naming the file.
    #[test]
    fn apply_fails_loud_on_an_invalid_config_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let cfg = write_config(tmp.path(), "config.toml", "[llm]\nbase_urL = \"typo\"\n");

        let report = run_apply_in_child(&home, &cwd, &[("CONGA_CONFIG", cfg.to_str().unwrap())]);
        assert!(report.outcome.starts_with("err:"), "{}", report.outcome);
        assert!(
            report.outcome.contains("invalid config.toml"),
            "must name the failure: {}",
            report.outcome
        );
        assert!(
            report.outcome.contains("base_urL"),
            "must name the offending key: {}",
            report.outcome
        );
    }

    /// No config file anywhere: `apply()` is a no-op that reports "nothing
    /// found" instead of failing.
    #[test]
    fn apply_is_a_noop_without_any_config_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let report = run_apply_in_child(&home, &cwd, &[]);
        assert_eq!(report.outcome, "ok:none");
        assert_eq!(report.env_of("CONGA_PROJECT_DIR"), Some("<unset>"));
    }

    /// Discovery order: explicit file > project file > home file.
    #[test]
    fn apply_prefers_explicit_then_project_then_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(home.join(".conga")).unwrap();
        std::fs::create_dir_all(cwd.join(".conga")).unwrap();
        let explicit = write_config(
            tmp.path(),
            "explicit.toml",
            "[tools]\nproject_dir = \"/explicit\"\n",
        );
        write_config(
            &cwd,
            ".conga/config.toml",
            "[tools]\nproject_dir = \"/project\"\n",
        );
        write_config(
            &home,
            ".conga/config.toml",
            "[tools]\nproject_dir = \"/home\"\n",
        );

        // 1. explicit wins
        let report =
            run_apply_in_child(&home, &cwd, &[("CONGA_CONFIG", explicit.to_str().unwrap())]);
        assert_eq!(report.outcome, format!("ok:{}", explicit.display()));
        assert_eq!(report.env_of("CONGA_PROJECT_DIR"), Some("/explicit"));

        // 2. project beats home
        let report = run_apply_in_child(&home, &cwd, &[]);
        assert!(
            report.outcome.ends_with(".conga/config.toml"),
            "project file must win: {}",
            report.outcome
        );
        assert_eq!(report.env_of("CONGA_PROJECT_DIR"), Some("/project"));

        // 3. home is the last fallback
        let empty_cwd = tmp.path().join("empty-cwd");
        std::fs::create_dir_all(&empty_cwd).unwrap();
        let report = run_apply_in_child(&home, &empty_cwd, &[]);
        assert!(
            report.outcome.ends_with(".conga/config.toml"),
            "home file must be the fallback: {}",
            report.outcome
        );
        assert_eq!(report.env_of("CONGA_PROJECT_DIR"), Some("/home"));
    }

    /// An empty config file changes nothing (no env var is invented).
    #[test]
    fn apply_of_an_empty_file_sets_no_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let cfg = write_config(tmp.path(), "config.toml", "");

        let report = run_apply_in_child(&home, &cwd, &[("CONGA_CONFIG", cfg.to_str().unwrap())]);
        assert_eq!(report.outcome, format!("ok:{}", cfg.display()));
        for key in PROBE_ENV_KEYS {
            assert_eq!(
                report.env_of(key),
                Some("<unset>"),
                "an empty config must not set {key}"
            );
        }
    }
}
