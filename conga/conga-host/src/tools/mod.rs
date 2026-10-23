//! Built-in tools: read / write / edit / bash / list / grep / fetch / spawn_subagents / evolve.

pub mod bash;
pub mod edit;
pub mod evolve_tool;
pub mod fetch;
pub mod grep;
pub mod list;
pub mod read;
pub mod sandbox;
pub mod shell;
pub mod subagent;
pub mod todo;
pub mod write;

use std::path::{Component, Path, PathBuf};

use conga::types::tool::ToolDefinition;
/// The built-in tools, ready to drop into `AgentContext.tools`.
/// `spawn_subagents` and `evolve` are built WITHOUT live state here; hosts
/// wire their own via `Host::with_spawner`, which replaces both tools in
/// their list.
pub fn built_in_tools() -> Vec<ToolDefinition> {
    vec![
        read::tool(),
        write::tool(),
        edit::tool(),
        bash::tool(),
        list::tool(),
        grep::tool(),
        fetch::tool(),
        todo::tool(),
        subagent::tool(None),
        evolve_tool::tool(None),
    ]
}

/// Shared cap for tool textual output (bash stdout/stderr, fetch body).
pub(crate) const MAX_OUTPUT_BYTES: usize = 200_000;

/// Char-safe truncation to [`MAX_OUTPUT_BYTES`] with an indicator. Never
/// slices through a multi-byte UTF-8 char (a plain `String::truncate` panics
/// when the byte limit lands mid-char).
pub(crate) fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = s[..cut].to_string();
    out.push_str("\n...(truncated)");
    out
}

/// Spill threshold-sharing wrapper around [`truncate_output`]: content over
/// [`MAX_OUTPUT_BYTES`] is written whole to `<state_dir>/spill/` and replaced
/// in-context by a head preview + file path (the full output is preserved
/// on disk at that path). Falls back to plain truncation if the disk write fails —
/// a spill problem must never fail the tool.
pub(crate) fn spill_or_truncate(ctx: &conga::types::tool::ToolCallCtx, s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    // u64 hashes to 16 hex digits; keep a fixed 12-char name by taking the
    // leading 12 (an ASCII slice of a hex string is always char-safe).
    let name = format!("{}.txt", &format!("{:016x}", h.finish())[..12]);
    let dir = ctx.ctx.state_dir.join("spill");
    let path = dir.join(&name);
    match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, s)) {
        Ok(()) => {
            let head: String = s.chars().take(4000).collect();
            format!(
                "[output too large for context ({} bytes); full output saved to {}; head preview follows]\n{}\n[...preview ends — full output on disk at the path above]",
                s.len(),
                path.display(),
                head
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "spill write failed; falling back to truncation");
            truncate_output(s)
        }
    }
}

/// Resolve `requested` against `cwd`, rejecting any `..` or absolute component
/// that would escape `cwd`, and re-checking the symlink-resolved target so a
/// symlink inside `cwd` can't point outside it (lexical `..`-checking alone
/// doesn't catch that).
pub(crate) fn resolve_within_cwd(cwd: &Path, requested: &str) -> Result<PathBuf, String> {
    let cwd_canon = cwd
        .canonicalize()
        .map_err(|e| format!("cwd not accessible: {e}"))?;
    let mut resolved = cwd_canon.clone();
    for comp in Path::new(requested).components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(c) => resolved.push(c),
            Component::ParentDir => {
                // Only allow `..` while we're still strictly inside cwd.
                if resolved == cwd_canon || !resolved.starts_with(&cwd_canon) {
                    return Err(format!("path escapes working directory: {requested}"));
                }
                resolved.pop();
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("absolute paths are not allowed: {requested}"));
            }
        }
    }

    // The target (or, if it doesn't exist yet, its nearest existing ancestor)
    // may be reached through a symlink that leads outside cwd. Canonicalize
    // that ancestor and re-check, then re-append the not-yet-existing tail.
    let (existing, tail) = nearest_existing_ancestor(&resolved);
    let existing_canon = existing
        .canonicalize()
        .map_err(|e| format!("path not accessible: {e}"))?;
    if !existing_canon.starts_with(&cwd_canon) {
        return Err(format!("path escapes working directory: {requested}"));
    }
    // `.join` on an empty `tail` still appends a stray trailing separator
    // (turning a file path into a dir-looking one), so skip it when empty.
    if tail.as_os_str().is_empty() {
        Ok(existing_canon)
    } else {
        Ok(existing_canon.join(tail))
    }
}

/// Read-path policy: relative paths resolve within cwd (see
/// [`resolve_within_cwd`]); absolute paths are allowed only under conga's
/// own config dir (`~/.conga` — spill files and tool state live there).
/// Anything else absolute is rejected: the cwd sandbox stays intact.
pub(crate) fn resolve_read_path(cwd: &Path, requested: &str) -> Result<PathBuf, String> {
    resolve_read_path_in(cwd, &conga::storage::config_dir(), requested)
}

/// Testable core: `allowed_root` is injected (production uses the config dir).
pub(crate) fn resolve_read_path_in(
    cwd: &Path,
    allowed_root: &Path,
    requested: &str,
) -> Result<PathBuf, String> {
    let p = Path::new(requested);
    if p.is_absolute() {
        let root_canon = allowed_root
            .canonicalize()
            .map_err(|e| format!("allowed root not accessible: {e}"))?;
        let canon = p
            .canonicalize()
            .map_err(|e| format!("path not accessible: {e}"))?;
        if canon.starts_with(&root_canon) {
            return Ok(canon);
        }
        return Err(format!(
            "absolute paths outside the conga config directory are not allowed: {requested}"
        ));
    }
    resolve_within_cwd(cwd, requested)
}

/// Walk `path` up to the nearest ancestor that exists on disk. Returns that
/// ancestor plus the (necessarily non-existent, so symlink-free) remainder.
fn nearest_existing_ancestor(path: &Path) -> (PathBuf, PathBuf) {
    let mut current = path.to_path_buf();
    let mut names = Vec::new();
    while !current.exists() {
        let Some(name) = current.file_name() else {
            break; // reached the root without finding an existing component
        };
        names.push(name.to_os_string());
        current.pop();
    }
    // `names` was collected leaf-to-root; rebuild root-to-leaf. Pushing onto
    // an empty PathBuf via `.join("")` would add a stray trailing separator,
    // so accumulate with `push` instead.
    let mut tail = PathBuf::new();
    for name in names.into_iter().rev() {
        tail.push(name);
    }
    (current, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_lexical_escape() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_within_cwd(tmp.path(), "../../etc/passwd").is_err());
        assert!(resolve_within_cwd(tmp.path(), "/etc/passwd").is_err());
    }

    #[test]
    fn allows_path_within_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "x").unwrap();
        let resolved = resolve_within_cwd(tmp.path(), "f.txt").unwrap();
        assert_eq!(resolved, tmp.path().canonicalize().unwrap().join("f.txt"));
    }

    #[test]
    fn allows_new_file_in_existing_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let resolved = resolve_within_cwd(tmp.path(), "sub/new.txt").unwrap();
        assert_eq!(
            resolved,
            tmp.path().canonicalize().unwrap().join("sub/new.txt")
        );
    }
    #[test]
    fn truncate_output_is_char_safe_at_multibyte_boundary() {
        // 3-byte chars: make the byte limit land inside the final char.
        let n = MAX_OUTPUT_BYTES / 3 + 10;
        let s = "你".repeat(n);
        assert!(s.len() > MAX_OUTPUT_BYTES);
        let out = truncate_output(&s);
        assert!(out.ends_with("...(truncated)"));
        assert!(out.len() < s.len());
        // The retained prefix is valid UTF-8 by construction; re-encode check.
        let trimmed = out.trim_end_matches("\n...(truncated)");
        assert_eq!(trimmed.chars().count() * 3, trimmed.len());
    }

    #[test]
    fn truncate_output_noop_under_limit() {
        assert_eq!(truncate_output("small"), "small");
    }

    /// Boundary: exactly at the cap nothing is truncated, and nothing is
    /// spilled to disk.
    #[test]
    fn output_at_exactly_the_limit_is_left_alone() {
        let exact = "x".repeat(MAX_OUTPUT_BYTES);
        let out = truncate_output(&exact);
        assert_eq!(out.len(), MAX_OUTPUT_BYTES);
        assert!(!out.ends_with("...(truncated)"));

        let tmp = tempfile::tempdir().unwrap();
        let ctx = spill_ctx(tmp.path());
        let out = spill_or_truncate(&ctx, &exact);
        assert_eq!(out.len(), MAX_OUTPUT_BYTES);
        assert!(
            !tmp.path().join("spill").exists(),
            "nothing may be spilled at the limit"
        );
    }

    /// Boundary: one byte over the cap is truncated with the indicator.
    #[test]
    fn output_one_byte_over_the_limit_is_truncated() {
        let over = "x".repeat(MAX_OUTPUT_BYTES + 1);
        let out = truncate_output(&over);
        assert!(
            out.ends_with("\n...(truncated)"),
            "{}",
            &out[out.len() - 20..]
        );
        assert_eq!(out.len(), MAX_OUTPUT_BYTES + "\n...(truncated)".len());
    }

    fn spill_ctx(state_dir: &Path) -> conga::types::tool::ToolCallCtx {
        conga::types::tool::ToolCallCtx {
            tool_call_id: "t".into(),
            args: serde_json::json!({}),
            signal: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "s".into(),
                state_dir: state_dir.to_path_buf(),
            },
        }
    }

    #[test]
    fn spill_writes_full_output_and_returns_stub() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = spill_ctx(tmp.path());
        let big = "x".repeat(MAX_OUTPUT_BYTES + 1000);
        let out = spill_or_truncate(&ctx, &big);
        assert!(out.len() < big.len());
        assert!(out.contains("full output saved to"), "{out}");
        // The file the stub points at holds the complete original output.
        let line = out.lines().find(|l| l.contains("saved to")).unwrap();
        let path = line
            .split("saved to ")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .trim();
        let on_disk = std::fs::read_to_string(path).unwrap();
        assert_eq!(on_disk.len(), big.len());
    }

    #[test]
    fn spill_small_output_passthrough() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = spill_ctx(tmp.path());
        let out = spill_or_truncate(&ctx, "small");
        assert_eq!(out, "small");
    }

    #[test]
    fn spill_write_failure_falls_back_to_truncation() {
        // state_dir is a regular file, so create_dir_all(<file>/spill) fails;
        // the tool must still get usable (truncated) output, never an error.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("occupied");
        std::fs::write(&file, "x").unwrap();
        let ctx = spill_ctx(&file);
        let big = "y".repeat(MAX_OUTPUT_BYTES + 100);
        let out = spill_or_truncate(&ctx, &big);
        assert!(out.ends_with("...(truncated)"), "{out}");
    }

    #[test]
    fn read_path_allows_absolute_inside_allowed_root() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("spilled.txt");
        std::fs::write(&file, "spilled").unwrap();
        let resolved =
            resolve_read_path_in(cwd.path(), root.path(), file.to_str().unwrap()).unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn read_path_rejects_absolute_outside_allowed_root() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("secret.txt");
        std::fs::write(&file, "s3cr3t").unwrap();
        let err =
            resolve_read_path_in(cwd.path(), root.path(), file.to_str().unwrap()).unwrap_err();
        assert!(err.contains("outside the conga config directory"), "{err}");
    }

    #[test]
    fn read_path_relative_still_sandboxed_to_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        // Relative escape is rejected regardless of the allowed root.
        assert!(resolve_read_path_in(cwd.path(), root.path(), "../escape").is_err());
        // A legitimate relative path still resolves inside cwd.
        std::fs::write(cwd.path().join("f.txt"), "x").unwrap();
        let resolved = resolve_read_path_in(cwd.path(), root.path(), "f.txt").unwrap();
        assert_eq!(resolved, cwd.path().canonicalize().unwrap().join("f.txt"));
    }

    /// The built-in set is the host's default tool list: every entry must be
    /// fully wired (schema + executable handler), and the list must not
    /// silently grow or shrink.
    #[test]
    fn built_in_tools_returns_the_expected_tool_set() {
        let names: Vec<String> = built_in_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "read",
                "write",
                "edit",
                "bash",
                "list",
                "grep",
                "fetch",
                "todo",
                "spawn_subagents",
                "evolve",
            ]
        );
    }

    #[test]
    fn built_in_tools_are_all_fully_specified() {
        let tools = built_in_tools();
        assert_eq!(tools.len(), 10);
        for t in &tools {
            assert!(!t.name.is_empty(), "tool without a name");
            assert!(!t.label.is_empty(), "{} has no label", t.name);
            assert!(!t.description.is_empty(), "{} has no description", t.name);
            assert!(
                t.parameters.is_object(),
                "{} has a non-object parameter schema",
                t.name
            );
        }
    }

    /// An existing path is its own nearest existing ancestor: the returned
    /// tail must be empty (no phantom components).
    #[test]
    fn nearest_existing_ancestor_returns_existing_path_with_empty_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let (ancestor, tail) = nearest_existing_ancestor(&file);
        assert_eq!(ancestor, file);
        assert!(
            tail.as_os_str().is_empty(),
            "tail must be empty for an existing path: {tail:?}"
        );
    }

    /// A path that does not exist yet resolves to its closest existing
    /// ancestor plus the still-missing remainder (root-to-leaf order).
    #[test]
    fn nearest_existing_ancestor_walks_up_and_keeps_missing_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a/b/c/new.txt");
        let (ancestor, tail) = nearest_existing_ancestor(&deep);
        assert_eq!(ancestor, tmp.path());
        assert_eq!(tail, PathBuf::from("a").join("b").join("c").join("new.txt"));
    }

    /// Walking up must stop at the filesystem root instead of looping
    /// forever when nothing on the path exists.
    #[cfg(unix)]
    #[test]
    fn nearest_existing_ancestor_stops_at_the_filesystem_root() {
        let missing = PathBuf::from("/conga-nonexistent-root-xyz/child.txt");
        let (ancestor, tail) = nearest_existing_ancestor(&missing);
        assert_eq!(ancestor, PathBuf::from("/"));
        assert_eq!(
            tail,
            PathBuf::from("conga-nonexistent-root-xyz").join("child.txt")
        );
    }

    /// The production read-path policy (not just its testable core):
    /// relative paths resolve inside cwd.
    #[test]
    fn resolve_read_path_resolves_relative_path_inside_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("f.txt"), "x").unwrap();
        let resolved = resolve_read_path(cwd.path(), "f.txt").unwrap();
        assert_eq!(resolved, cwd.path().canonicalize().unwrap().join("f.txt"));
    }

    #[test]
    fn resolve_read_path_rejects_relative_escape() {
        let cwd = tempfile::tempdir().unwrap();
        let err = resolve_read_path(cwd.path(), "../outside.txt").unwrap_err();
        assert!(err.contains("escapes working directory"), "{err}");
    }

    /// Absolute paths are only allowed under conga's own config dir; every
    /// other absolute path is refused.
    #[test]
    fn resolve_read_path_rejects_absolute_path_outside_config_dir() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("secret.txt");
        std::fs::write(&file, "s3cr3t").unwrap();
        let err = resolve_read_path(cwd.path(), file.to_str().unwrap()).unwrap_err();
        let root = conga::storage::config_dir();
        if root.exists() {
            assert!(err.contains("outside the conga config directory"), "{err}");
        } else {
            // No config dir on this machine: the allowed root cannot be
            // canonicalized, and the guard still refuses (fail closed).
            assert!(err.contains("allowed root not accessible"), "{err}");
        }
    }

    /// Boundary: an empty request is not a path — it must resolve to the
    /// cwd itself rather than erroring or escaping.
    #[test]
    fn resolve_read_path_empty_request_resolves_to_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let resolved = resolve_read_path(cwd.path(), "").unwrap();
        assert_eq!(resolved, cwd.path().canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        // A symlink inside cwd pointing outside it must not let a request
        // through, even though the lexical (no `..`) check alone would allow it.
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("escape")).unwrap();

        let r = resolve_within_cwd(tmp.path(), "escape/secret.txt");
        assert!(r.is_err(), "symlink escape must be rejected, got {r:?}");
    }
}
