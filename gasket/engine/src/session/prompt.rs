//! Prompt loading utilities.
//!
//! Provides functions to load workspace bootstrap files and skills context
//! for injection into the system prompt. These are called directly by
//! `AgentSession` during initialization — no dynamic hook dispatch needed.

use std::path::Path;

use tokio::fs;
use tracing::{debug, info, warn};

use gasket_storage::count_tokens;

/// Bootstrap files loaded into the system prompt for the full (main agent) profile
///
/// Load order matters — later files appear closer to the conversation start,
/// giving them more influence on the model's behavior.
///
/// | File        | Purpose                                       |
/// |-------------|-----------------------------------------------|
/// | PROFILE.md  | Agent identity + user profile                  |
/// | SOUL.md     | Core behavioral rules & knowledge management   |
/// | MEMORY.md   | Session memory (auto-truncated, may not exist) |
/// | BOOTSTRAP.md| First-run onboarding ritual                    |
pub const BOOTSTRAP_FILES_FULL: &[&str] = &["PROFILE.md", "SOUL.md", "MEMORY.md", "BOOTSTRAP.md"];

/// Bootstrap files loaded for the minimal (subagent) profile — only core identity
pub const BOOTSTRAP_FILES_MINIMAL: &[&str] = &["PROFILE.md", "SOUL.md"];

/// Maximum tokens allowed per single bootstrap file before emitting a warning
const BOOTSTRAP_TOKEN_WARN_THRESHOLD: usize = 2000;

/// Hard token limit for MEMORY.md — content exceeding this is truncated.
/// Keeps the tail (most recent entries) and drops the head (oldest entries).
pub const MEMORY_TOKEN_HARD_LIMIT: usize = 2048;

/// Files subject to hard token truncation (not just a warning).
const TRUNCATABLE_FILES: &[&str] = &["MEMORY.md"];

/// Default identity prefix when none is configured.
const DEFAULT_IDENTITY_PREFIX: &str =
    "你是乐子🐈, 我的personal AI assistant.\nYour working directory: {workspace}.\n YOU can ONLY READ and WRITE under working directory.";

/// Load the system prompt from workspace bootstrap files.
///
/// Reads the specified files from the workspace directory and concatenates them.
/// Returns an identity header plus any loaded bootstrap file contents.
/// If no files are found, returns only the identity header.
///
/// # Errors
/// Returns an error if a bootstrap file **exists** but cannot be read.
pub async fn load_system_prompt(
    workspace: &Path,
    files: &[&str],
    identity_prefix: Option<&str>,
) -> Result<String, std::io::Error> {
    let mut parts = Vec::new();

    // Identity header
    let prefix = identity_prefix.unwrap_or(DEFAULT_IDENTITY_PREFIX);
    parts.push(prefix.replace("{workspace}", &workspace.display().to_string()));

    // Load bootstrap files
    let mut loaded_any = false;
    let mut total_tokens: usize = 0;
    for filename in files {
        let file_path = workspace.join(filename);
        if file_path.exists() {
            let raw_content = fs::read_to_string(&file_path).await?;
            if !raw_content.trim().is_empty() {
                let trimmed = raw_content.trim();
                let tokens = count_tokens(trimmed);

                // Hard truncation for memory-class files (e.g. MEMORY.md)
                let content =
                    if TRUNCATABLE_FILES.contains(filename) && tokens > MEMORY_TOKEN_HARD_LIMIT {
                        warn!(
                        "Bootstrap file {} has {} tokens (hard limit {}). Truncating tail-keep.",
                        filename, tokens, MEMORY_TOKEN_HARD_LIMIT
                    );
                        truncate_keep_tail(trimmed, MEMORY_TOKEN_HARD_LIMIT)
                    } else {
                        if tokens > BOOTSTRAP_TOKEN_WARN_THRESHOLD {
                            warn!(
                            "Bootstrap file {} has {} tokens (threshold {}). Consider trimming it.",
                            filename, tokens, BOOTSTRAP_TOKEN_WARN_THRESHOLD
                        );
                        }
                        trimmed.to_string()
                    };

                let final_tokens = count_tokens(&content);
                total_tokens += final_tokens;
                debug!(
                    "Loaded bootstrap file: {} ({} tokens{})",
                    filename,
                    final_tokens,
                    if final_tokens < tokens {
                        format!(", truncated from {}", tokens)
                    } else {
                        String::new()
                    }
                );
                parts.push(format!("## {}\n\n{}", filename, content));
                loaded_any = true;
            }
        }
    }

    info!(
        "System prompt: {} bootstrap files loaded ({} found), ~{} tokens total",
        files.len(),
        loaded_any,
        total_tokens
    );

    Ok(parts.join("\n\n"))
}

/// Load the skills context from the workspace.
///
/// Scans for skill definitions and returns a formatted string for prompt injection,
/// or `None` if no skills are found.
pub async fn load_skills_context(workspace: &Path) -> Option<String> {
    let ctx = super::load_skills(workspace).await?;
    if ctx.is_empty() {
        None
    } else {
        Some(format!("# Skills\n\n{}", ctx))
    }
}

/// Truncate content to fit within `max_tokens`, keeping the **tail** (most recent
/// entries) and dropping lines from the head. Prepends a system warning so the
/// agent knows it must clean up.
///
/// Uses a two-stage strategy:
/// 1. Line-level tail-keep (fast, preserves semantic boundaries).
/// 2. Character-level binary-search fallback (handles single lines longer than
///    the budget, e.g. a 50 000-character MEMORY.md with no newlines).
pub fn truncate_keep_tail(content: &str, max_tokens: usize) -> String {
    let warning = "[SYSTEM WARNING: This file was truncated because it exceeded the token limit. \
        Oldest entries were removed. Use 'read_file' to view the full file on disk, \
        and 'edit_file' to prune, summarize, or move details to separate files in memory/.]";
    let warning_tokens = count_tokens(warning) + 10; // margin for newlines
    let budget = max_tokens.saturating_sub(warning_tokens);

    let mut current_tokens = 0;
    let mut split_byte_index = content.len();

    // rmatch_indices guarantees UTF-8 safe byte indices because '\n' is an ASCII byte
    // and can never appear inside a multi-byte UTF-8 character continuation byte.
    for (idx, _) in content.rmatch_indices('\n') {
        let line = &content[idx..split_byte_index];
        let line_tokens = count_tokens(line);

        if current_tokens + line_tokens > budget {
            break;
        }
        current_tokens += line_tokens;
        split_byte_index = idx;
    }

    let tail = if split_byte_index == content.len() {
        // No newlines at all — treat the entire content as one block.
        if count_tokens(content) <= budget {
            return content.to_string();
        }
        truncate_tail_by_chars(content, budget)
    } else {
        let tail = &content[split_byte_index..];
        // Fallback: if a single line exceeds the budget, truncate character-by-character.
        truncate_tail_by_chars(tail, budget)
    };

    let mut result = String::with_capacity(warning.len() + tail.len() + 2);
    result.push_str(warning);
    result.push_str("\n\n");
    result.push_str(tail);
    result
}

/// Binary-search the smallest byte index such that `tail[split..]` fits within
/// `budget` tokens. Guarantees UTF-8 safety via `floor_char_boundary`.
fn truncate_tail_by_chars(tail: &str, budget: usize) -> &str {
    let tail_tokens = count_tokens(tail);
    if tail_tokens <= budget {
        return tail;
    }

    let mut lo = 0usize;
    let mut hi = tail.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let mid = tail.floor_char_boundary(mid);
        let candidate = &tail[mid..];
        if count_tokens(candidate) > budget {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    &tail[tail.floor_char_boundary(lo)..]
}
