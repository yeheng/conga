//! `mcp.json` config file: server list, per-server transport config.

use std::collections::HashMap;

use serde::Deserialize;

use super::McpError;

/// One MCP server entry from `mcp.json`.
///
/// Two mutually-exclusive transport modes:
/// - **stdio** (default): `command` + `args` + `env` — spawns a subprocess.
/// - **Streamable HTTP**: `url` + `headers` — POSTs JSON-RPC to a remote server.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// stdio: command to run (mutually exclusive with `url`).
    #[serde(default)]
    pub command: Option<String>,
    /// stdio: command arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio: extra environment variables for the subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Streamable HTTP: server URL (mutually exclusive with `command`).
    #[serde(default)]
    pub url: Option<String>,
    /// Streamable HTTP: extra HTTP headers (e.g. `Authorization: Bearer ...`).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

impl McpServerConfig {
    /// True if this entry configures a Streamable HTTP server.
    pub(crate) fn is_http(&self) -> bool {
        self.url.is_some()
    }
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerConfig>,
}

/// Read MCP config: `$CONGA_MCP_CONFIG` path, else `~/.conga/mcp.json`.
/// Missing file → empty vec. Bad JSON → error.
pub fn load_config() -> Result<Vec<(String, McpServerConfig)>, McpError> {
    let path = mcp_config_path();
    load_config_from(&path)
}

/// Same as [`load_config`] but from an explicit path (tests).
pub fn load_config_from(
    path: &std::path::Path,
) -> Result<Vec<(String, McpServerConfig)>, McpError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(McpError::Io(e)),
    };
    let cfg: ConfigFile = serde_json::from_str(&text)?;
    Ok(cfg.mcp_servers.into_iter().collect())
}

fn mcp_config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("CONGA_MCP_CONFIG") {
        return std::path::PathBuf::from(p);
    }
    conga::storage::config_dir().join("mcp.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.json");
        let cfg = load_config_from(&path).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn config_parses_mcp_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{
                "mcpServers": {
                    "github": {
                        "command": "npx",
                        "args": ["-y", "server-github"],
                        "env": { "TOKEN": "secret" }
                    },
                    "fs": {
                        "command": "npx",
                        "args": ["-y", "server-fs"]
                    }
                }
            }"#,
        )
        .unwrap();
        let cfg = load_config_from(&path).unwrap();
        assert_eq!(cfg.len(), 2);
        let github = cfg
            .iter()
            .find(|(n, _)| n == "github")
            .map(|(_, c)| c)
            .unwrap();
        assert_eq!(github.command.as_deref(), Some("npx"));
        assert_eq!(github.args, vec!["-y", "server-github"]);
        assert_eq!(github.env.get("TOKEN").unwrap(), "secret");
    }

    #[test]
    fn config_parses_http_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{
                "mcpServers": {
                    "remote": {
                        "url": "https://mcp.example.dev/mcp",
                        "headers": { "Authorization": "Bearer tok123" }
                    },
                    "local": {
                        "command": "npx",
                        "args": ["-y", "server-fs"]
                    }
                }
            }"#,
        )
        .unwrap();
        let cfg = load_config_from(&path).unwrap();
        assert_eq!(cfg.len(), 2);
        let remote = cfg
            .iter()
            .find(|(n, _)| n == "remote")
            .map(|(_, c)| c)
            .unwrap();
        assert!(remote.is_http());
        assert_eq!(remote.url.as_deref(), Some("https://mcp.example.dev/mcp"));
        assert_eq!(
            remote.headers.get("Authorization").unwrap(),
            "Bearer tok123"
        );
        assert!(remote.command.is_none());

        let local = cfg
            .iter()
            .find(|(n, _)| n == "local")
            .map(|(_, c)| c)
            .unwrap();
        assert!(!local.is_http());
        assert_eq!(local.command.as_deref(), Some("npx"));
        assert!(local.url.is_none());
    }
}
