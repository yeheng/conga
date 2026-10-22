//! `list` tool — list directory entries, optionally recursive with a glob.
//!
//! Walking is `.gitignore`-aware (via the `ignore` crate) and always prunes
//! build/VCS/dependency trees ([`ALWAYS_IGNORE`]) so tool output stays bounded
//! even when those dirs aren't gitignored.

use std::sync::Arc;

use ignore::WalkBuilder;

use conga::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
use conga::ContentBlock;

/// Cap the number of entries returned so tool output stays bounded.
const MAX_ENTRIES: usize = 2000;

/// Directories always pruned, even when not in `.gitignore`: VCS metadata,
/// Rust/Cargo build output, JS dependency trees. Pruned at the directory level
/// so we never descend into them (`target/` alone can hold thousands of files).
const ALWAYS_IGNORE: &[&str] = &[".git", "target", "node_modules"];

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "list".into(),
        label: "List".into(),
        description: "List directory entries (gitignore-aware). Optional recursive + glob pattern."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory relative to cwd (default .)" },
                "recursive": { "type": "boolean", "description": "Recurse into subdirs (default false)" },
                "pattern": { "type": "string", "description": "Glob to filter, e.g. **/*.rs" }
            }
        }),
        risk: RiskLevel::Low,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, conga::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }
    let path = ctx.args["path"].as_str().unwrap_or(".");
    let recursive = ctx.args["recursive"].as_bool().unwrap_or(false);
    let pattern = ctx.args["pattern"].as_str();

    let base = match super::resolve_within_cwd(&ctx.ctx.cwd, path) {
        Ok(p) => p,
        Err(msg) => return Ok(ToolResult::error(msg)),
    };
    if !base.exists() {
        return Ok(ToolResult::error(format!("path not found: {}", path)));
    }

    let glob_pat = pattern.and_then(|p| glob::Pattern::new(p).ok());

    let mut builder = WalkBuilder::new(&base);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .filter_entry(|entry| {
            // Prune always-ignore dirs at the directory level (see ALWAYS_IGNORE).
            entry.file_type().is_none_or(|ft| {
                !ft.is_dir()
                    || entry
                        .file_name()
                        .to_str()
                        .map(|name| !ALWAYS_IGNORE.contains(&name))
                        .unwrap_or(true)
            })
        });
    if !recursive {
        builder.max_depth(Some(1));
    }

    let mut entries: Vec<String> = Vec::new();
    for entry in builder.build().filter_map(|e| e.ok()) {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        if ctx.aborted() {
            return Ok(ToolResult::error("aborted".to_string()));
        }
        let rel = entry
            .path()
            .strip_prefix(&base)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();
        if rel.is_empty() {
            continue;
        }
        if let Some(pat) = &glob_pat {
            if !pat.matches(&rel) {
                continue;
            }
        }
        let suffix = if entry.file_type().is_some_and(|t| t.is_dir()) {
            "/"
        } else {
            ""
        };
        entries.push(format!("{}{}", rel, suffix));
    }

    entries.sort();
    // Spill oversize listings at birth (freeze-at-birth contract; a
    // 2000-entry recursive listing can exceed the in-context cap).
    let text = super::spill_or_truncate(&ctx, &entries.join("\n"));
    Ok(ToolResult {
        content: vec![ContentBlock::text(text)],
        details: serde_json::json!({"count": entries.len()}),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::types::tool::ToolContext;
    use std::sync::atomic::AtomicBool;

    async fn run(args: serde_json::Value, cwd: &std::path::Path) -> ToolResult {
        let t = tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: cwd.to_path_buf(),
            },
        })
        .await
        .unwrap()
    }

    fn text_of(r: &ToolResult) -> String {
        match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn lists_top_level() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.rs"), "").await.unwrap();
        tokio::fs::create_dir(tmp.path().join("sub")).await.unwrap();
        let r = run(serde_json::json!({}), tmp.path()).await;
        let text = text_of(&r);
        assert!(text.contains("a.rs"));
        assert!(text.contains("sub/"));
    }

    #[tokio::test]
    async fn rejects_path_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(serde_json::json!({"path": "../"}), tmp.path()).await;
        assert!(r.is_error, "`..` escape must be rejected");
        let r = run(serde_json::json!({"path": "/etc"}), tmp.path()).await;
        assert!(r.is_error, "absolute path must be rejected");
    }

    #[tokio::test]
    async fn recursive_with_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.rs"), "").await.unwrap();
        tokio::fs::create_dir(tmp.path().join("d")).await.unwrap();
        tokio::fs::write(tmp.path().join("d/b.txt"), "")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"recursive": true, "pattern": "*.txt"}),
            tmp.path(),
        )
        .await;
        let text = text_of(&r);
        assert!(text.contains("b.txt"));
        assert!(!text.contains("a.rs"));
    }

    #[tokio::test]
    async fn prunes_target_dir() {
        // target/ must be pruned even with no .gitignore.
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("keep.rs"), "")
            .await
            .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("target/debug"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("target/debug/junk.o"), "")
            .await
            .unwrap();
        let r = run(serde_json::json!({"recursive": true}), tmp.path()).await;
        let text = text_of(&r);
        assert!(text.contains("keep.rs"));
        assert!(
            !text.contains("target"),
            "target/ leaked into output: {text}"
        );
    }

    #[tokio::test]
    async fn respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        // The `ignore` crate applies `.gitignore` only inside a git repo. An
        // empty `.git` marker is enough to establish the repo root.
        tokio::fs::create_dir(tmp.path().join(".git"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join(".gitignore"), "*.log\n")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("a.rs"), "").await.unwrap();
        tokio::fs::write(tmp.path().join("noisy.log"), "")
            .await
            .unwrap();
        let r = run(serde_json::json!({}), tmp.path()).await;
        let text = text_of(&r);
        assert!(text.contains("a.rs"));
        assert!(
            !text.contains("noisy.log"),
            "gitignored file leaked: {text}"
        );
    }

    #[test]
    fn tool_definition_exposes_optional_path_recursive_and_pattern() {
        let t = tool();
        assert_eq!(t.name, "list");
        assert_eq!(t.label, "List");
        assert_eq!(t.risk, conga::types::tool::RiskLevel::Low);
        assert!(t.parameters["properties"]["path"]["type"] == "string");
        assert!(t.parameters["properties"]["recursive"]["type"] == "boolean");
        assert!(t.parameters["properties"]["pattern"]["type"] == "string");
        assert!(
            t.parameters.get("required").is_none(),
            "every argument has a default"
        );
    }

    /// Without `recursive` the walk stops at depth 1: a file inside a
    /// subdirectory must not show up.
    #[tokio::test]
    async fn does_not_recurse_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.rs"), "").await.unwrap();
        tokio::fs::create_dir(tmp.path().join("sub")).await.unwrap();
        tokio::fs::write(tmp.path().join("sub/deep.rs"), "").await.unwrap();
        let r = run(serde_json::json!({}), tmp.path()).await;
        let text = text_of(&r);
        assert!(text.contains("a.rs"), "{text}");
        assert!(text.contains("sub/"), "{text}");
        assert!(
            !text.contains("deep.rs"),
            "a non-recursive listing must not descend: {text}"
        );
    }

    /// Error path: a path that does not exist is reported, not silently
    /// empty.
    #[tokio::test]
    async fn reports_path_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(serde_json::json!({"path": "nope"}), tmp.path()).await;
        assert!(r.is_error);
        assert!(text_of(&r).contains("path not found"), "{}", text_of(&r));
    }

    /// Boundary: a glob that matches nothing yields an empty listing (and
    /// no error).
    #[tokio::test]
    async fn lists_nothing_when_the_glob_matches_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.rs"), "").await.unwrap();
        let r = run(serde_json::json!({"pattern": "*.zzz"}), tmp.path()).await;
        assert!(!r.is_error);
        assert_eq!(text_of(&r), "");
        assert_eq!(r.details["count"], 0);
    }

    /// Boundary: an invalid glob pattern is dropped (no filtering) rather
    /// than failing the whole listing.
    #[tokio::test]
    async fn invalid_glob_pattern_falls_back_to_no_filtering() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.rs"), "").await.unwrap();
        let r = run(serde_json::json!({"pattern": "["}), tmp.path()).await;
        assert!(!r.is_error);
        assert!(text_of(&r).contains("a.rs"), "{}", text_of(&r));
    }

    /// Boundary: listing a FILE (not a directory) yields nothing — the
    /// root entry strips to an empty relative path.
    #[tokio::test]
    async fn lists_nothing_for_a_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.rs"), "").await.unwrap();
        let r = run(serde_json::json!({"path": "a.rs"}), tmp.path()).await;
        assert!(!r.is_error);
        assert_eq!(text_of(&r), "");
    }

    /// Boundary: the listing is capped so a huge tree cannot flood the
    /// context.
    #[tokio::test]
    async fn caps_the_number_of_entries() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..(MAX_ENTRIES + 25) {
            tokio::fs::write(tmp.path().join(format!("f{i}.rs")), "")
                .await
                .unwrap();
        }
        let r = run(serde_json::json!({"recursive": true}), tmp.path()).await;
        assert_eq!(
            r.details["count"], MAX_ENTRIES,
            "the listing must stop at the cap"
        );
    }
}
