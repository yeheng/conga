//! `write` tool — write content to a file (creating parent dirs).

use std::sync::Arc;

use conga::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
use conga::ContentBlock;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "write".into(),
        label: "Write".into(),
        description: "Write text content to a file, creating parent directories.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        }),
        risk: RiskLevel::Medium,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, conga::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }
    let path = ctx.args["path"]
        .as_str()
        .ok_or_else(|| conga::error::ToolError::Message("path is required".into()))?;
    let content = ctx.args["content"]
        .as_str()
        .ok_or_else(|| conga::error::ToolError::Message("content is required".into()))?;

    let full = match super::resolve_within_cwd(&ctx.ctx.cwd, path) {
        Ok(p) => p,
        Err(msg) => return Ok(ToolResult::error(msg)),
    };
    if let Some(parent) = full.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Atomic write via temp file + rename — a crash mid-write must not leave
    // a half-written file behind (same policy as `edit`). The tmp name adds a
    // unique uuid between the target name and the suffix (suffix, not
    // `with_extension`, so extensions survive): concurrent writers to the
    // same target must never share a tmp file. A hard-crash orphan is inert
    // (never read, uniquely named); it is not worth a directory scan per
    // write to sweep. Same directory as the target, so `rename` stays
    // atomic within one filesystem.
    let mut tmp_os = full.clone().into_os_string();
    tmp_os.push(format!(".{}.conga-tmp", uuid::Uuid::new_v4()));
    let tmp = std::path::PathBuf::from(tmp_os);
    let outcome = async {
        tokio::fs::write(&tmp, content).await?;
        tokio::fs::rename(&tmp, &full).await
    }
    .await;
    if outcome.is_err() {
        // Best effort: never leave our own tmp behind on a failed write.
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    outcome?;

    Ok(ToolResult {
        content: vec![ContentBlock::text(format!(
            "wrote {} bytes to {}",
            content.len(),
            path
        ))],
        details: serde_json::json!({"path": path, "bytes": content.len()}),
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

    /// Same as [`run`], but keeps the `Err` so the error path can be
    /// asserted on.
    async fn run_raw(
        args: serde_json::Value,
        cwd: &std::path::Path,
    ) -> Result<ToolResult, conga::error::ToolError> {
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
    }

    #[tokio::test]
    async fn writes_file_and_creates_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tool();
        let r = (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args: serde_json::json!({"path": "sub/f.txt", "content": "hello"}),
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: tmp.path().to_path_buf(),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: tmp.path().to_path_buf(),
            },
        })
        .await
        .unwrap();

        assert!(!r.is_error);
        let written = tokio::fs::read_to_string(tmp.path().join("sub/f.txt"))
            .await
            .unwrap();
        assert_eq!(written, "hello");
    }

    /// Atomicity proof: after a successful write there is no leftover tmp
    /// file from THIS write, and a pre-existing tmp file from an earlier
    /// crashed attempt can never be resurrected — with unique tmp names the
    /// orphan is inert (never read, never renamed in).
    #[tokio::test]
    async fn atomic_write_leaves_no_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("sub/f.txt");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        // Simulate a crashed previous attempt: orphaned tmp next to target.
        std::fs::write(tmp.path().join("sub/f.txt.conga-tmp"), "STALE").unwrap();

        let t = tool();
        let r = (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args: serde_json::json!({"path": "sub/f.txt", "content": "fresh"}),
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: tmp.path().to_path_buf(),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: tmp.path().to_path_buf(),
            },
        })
        .await
        .unwrap();
        assert!(!r.is_error);
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            "fresh",
            "stale tmp content must not survive"
        );
        // Exactly the target and the (untouched) stale orphan remain: this
        // write's own uniquely-named tmp must not survive the rename.
        let mut names: Vec<String> = std::fs::read_dir(tmp.path().join("sub"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["f.txt".to_string(), "f.txt.conga-tmp".to_string()]
        );
    }

    /// Concurrent writers to the same target must not share a tmp file: both
    /// writes succeed, the final content is exactly one writer's (never an
    /// interleave), and no tmp file is left behind.
    #[tokio::test]
    async fn concurrent_writes_never_share_a_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = |content: &'static str| {
            let t = tool();
            let cwd = tmp.path().to_path_buf();
            async move {
                (t.execute)(ToolCallCtx {
                    tool_call_id: "x".into(),
                    args: serde_json::json!({"path": "sub/f.txt", "content": content}),
                    signal: Arc::new(AtomicBool::new(false)),
                    ctx: ToolContext {
                        cwd: cwd.clone(),
                        env: Default::default(),
                        session_id: "s".into(),
                        state_dir: cwd,
                    },
                })
                .await
                .unwrap()
            }
        };
        let a = "A".repeat(64 * 1024);
        let b = "B".repeat(64 * 1024);
        let (ra, rb) = tokio::join!(exec(a.leak()), exec(b.leak()));
        assert!(!ra.is_error, "{:?}", ra.content);
        assert!(!rb.is_error, "{:?}", rb.content);

        let final_content = tokio::fs::read_to_string(tmp.path().join("sub/f.txt"))
            .await
            .unwrap();
        let pure_a = final_content.chars().all(|c| c == 'A');
        let pure_b = final_content.chars().all(|c| c == 'B');
        assert!(
            pure_a || pure_b,
            "concurrent writes must not interleave content"
        );
        let leftovers: Vec<String> = std::fs::read_dir(tmp.path().join("sub"))
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().into_owned();
                (n != "f.txt").then_some(n)
            })
            .collect();
        assert!(leftovers.is_empty(), "tmp files leaked: {leftovers:?}");
    }

    // ---- tool definition / argument handling ----

    #[test]
    fn tool_definition_exposes_path_and_content() {
        let t = tool();
        assert_eq!(t.name, "write");
        assert_eq!(t.label, "Write");
        assert_eq!(t.risk, conga::types::tool::RiskLevel::Medium);
        assert!(t.parameters["properties"]["path"]["type"] == "string");
        assert!(t.parameters["properties"]["content"]["type"] == "string");
        assert_eq!(
            t.parameters["required"],
            serde_json::json!(["path", "content"])
        );
    }

    /// Error path: both arguments are mandatory.
    #[tokio::test]
    async fn requires_path_and_content() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_raw(serde_json::json!({"content": "x"}), tmp.path())
            .await
            .expect_err("a missing path must fail");
        assert!(err.to_string().contains("path is required"), "{err}");

        let err = run_raw(serde_json::json!({"path": "f.txt"}), tmp.path())
            .await
            .expect_err("a missing content must fail");
        assert!(err.to_string().contains("content is required"), "{err}");
    }

    /// Error path: a path outside the cwd is refused — the sandbox holds.
    #[tokio::test]
    async fn rejects_a_path_outside_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(
            serde_json::json!({"path": "../escape.txt", "content": "x"}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
        let text = match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(text.contains("escapes working directory"), "{text}");
    }

    /// Error path: an aborted call must not write anything.
    #[tokio::test]
    async fn aborts_on_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tool();
        let r = (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args: serde_json::json!({"path": "f.txt", "content": "nope"}),
            signal: Arc::new(AtomicBool::new(true)),
            ctx: ToolContext {
                cwd: tmp.path().to_path_buf(),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: tmp.path().to_path_buf(),
            },
        })
        .await
        .unwrap();
        assert!(r.is_error);
        assert!(!tmp.path().join("f.txt").exists(), "nothing may be written");
    }

    /// Boundary: empty content is a valid write — it produces an empty
    /// file, not an error.
    #[tokio::test]
    async fn writes_an_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(
            serde_json::json!({"path": "empty.txt", "content": ""}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(r.details["bytes"], 0);
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("empty.txt"))
                .await
                .unwrap(),
            ""
        );
    }

    /// The reported byte count is bytes, not characters, and the content
    /// round-trips byte for byte.
    #[tokio::test]
    async fn reports_the_byte_count_for_unicode_content() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "héllo 你好";
        let r = run(
            serde_json::json!({"path": "u.txt", "content": content}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(r.details["bytes"], content.len());
        assert_ne!(
            content.len(),
            content.chars().count(),
            "the fixture must actually be multibyte"
        );
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("u.txt"))
                .await
                .unwrap(),
            content
        );
    }

    /// Writing over an existing file replaces it wholesale.
    #[tokio::test]
    async fn overwrites_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "old content")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "content": "new"}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "new"
        );
    }
}
