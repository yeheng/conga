//! The coding-agent system prompt: static discipline text, project doc
//! injection, and a per-turn environment snapshot.
//!
//! Three layers, assembled in [`crate::assembly`] (static) and
//! [`Host::run_turn`](crate::Host::run_turn) (per-turn):
//!
//! 1. `CODING_AGENT_PROMPT` — tool discipline, verification duty, surgical
//!    changes. Constant, paid on every request.
//! 2. `append_project_doc` — the nearest `AGENTS.md`/`CLAUDE.md` found by
//!    walking up from the project dir (first hit wins, ≤ 16 KB, annotated
//!    when truncated). Static per working directory.
//! 3. `env_snapshot` — UTC date + `git status --porcelain` + `git diff
//!    --stat`, capped and 3s-timeout-guarded. Rebuilt every turn so the
//!    agent sees repo drift as it happens.

use std::path::Path;

/// Injection cap for the project doc. A runaway AGENTS.md must not eat the
/// context window wholesale.
const MAX_PROJECT_DOC_BYTES: usize = 16 * 1024;

/// Cap on `git status --porcelain` lines before summarizing the rest.
const MAX_STATUS_LINES: usize = 64;

/// Cap on `git diff --stat` lines.
const MAX_DIFFSTAT_LINES: usize = 40;

/// Guard on each git subprocess so a wedged git cannot stall a turn.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// The static discipline: what the agent IS, not where it runs (that part
/// is the environment snapshot).
pub const CODING_AGENT_PROMPT: &str = "\
You are conga, a software engineering agent working in a real repository.

Core rules:
- Solve the user's ACTUAL request. Do not widen scope: no drive-by refactors,
  no speculative abstractions, no error handling for impossible cases.
- Be surgical. Touch the minimum files and lines. Match existing style even
  when you disagree with it. Every changed line must trace to the request.
- Read before you write. Open the relevant code, tests, and configs first;
  reuse existing patterns and helpers instead of inventing parallel ones.
- Verify your work. After a code change, run the narrowest command that
  proves it: the specific test, a build of the touched crate, a smoke run of
  the changed path. Never claim success without evidence you observed.
- When a tool call fails, read the error. Fix the cause; do not retry the
  identical call, do not suppress the symptom, do not special-case inputs.
- Prefer `edit` with multiple hunks in one call over many single edits.
- `bash` is a persistent shell per session: cd, exported env vars, and
  activated virtualenvs survive across calls. Use `run_in_background` for
  long-running commands and poll the returned log file.
- Track multi-step work with the `todo` tool; toggle items as you finish.
- If requirements are ambiguous and the choice changes scope or behavior,
  ask. For reversible low-risk details, pick the reasonable default and say
  so in your answer.
- Report at the end: what changed, how it was verified, and any known gaps.
  Claims about files, commands, or test results must be things you actually
  observed — mark inferences as such.";

/// Apply a custom base prompt over an already-assembled static prompt.
/// The static prompt is `built-in + project doc + skills` (see
/// [`crate::assembly`]); a custom prompt replaces ONLY the built-in
/// prefix - the project doc and skills sections ride along untouched, so
/// harness context survives a persona swap.
///
/// `custom` blank or `None` -> the assembled prompt unchanged.
pub fn with_custom_base_prompt(assembled: &str, custom: Option<&str>) -> String {
    let Some(custom) = custom.map(str::trim).filter(|c| !c.is_empty()) else {
        return assembled.to_string();
    };
    // The earliest appended section wins: project doc when present, else
    // skills, else nothing (bare built-in prompt).
    let tail = TAIL_MARKERS.iter().filter_map(|m| assembled.find(m)).min();
    match tail {
        // Slice from just past the leading "\n\n" so the kept tail
        // starts with "## ..." and the format adds exactly one blank
        // line between custom and tail.
        Some(i) => format!("{custom}\n\n{}", &assembled[i + 2..]),
        None => custom.to_string(),
    }
}

/// Opening text of the sections `append_project_doc` / `append_skills`
/// append after the built-in head.
const TAIL_MARKERS: [&str; 2] = ["\n\n## Project instructions (", "\n\n## Skills\n"];

/// Plan-mode directive, appended to the PERSISTED user message tail while
/// [`crate::permission::Mode::Plan`] is active (see `Host::run_turn`).
/// Rides the tail — like the env snapshot — so the system prompt stays
/// byte-stable across mode switches and the provider cache prefix survives.
pub const PLAN_MODE_DIRECTIVE: &str = "\
<agent-mode>plan</agent-mode>
Plan mode is ON: mutating tools (write / edit / bash / fetch / subagents) are blocked. Explore the repository with read-only tools, then present a concrete implementation plan — files to touch, approach, risks, and how to verify. Do not attempt to make changes.";

/// Append the nearest project doc (`AGENTS.md`, then `CLAUDE.md`) found at
/// or above `cwd`. First hit wins; nothing found → `base` unchanged.
pub fn append_project_doc(base: &str, cwd: &Path) -> String {
    match find_project_doc(cwd) {
        None => base.to_string(),
        Some((path, content)) => {
            let (body, note) = if content.len() > MAX_PROJECT_DOC_BYTES {
                let mut cut = MAX_PROJECT_DOC_BYTES;
                while cut > 0 && !content.is_char_boundary(cut) {
                    cut -= 1;
                }
                (&content[..cut], "\n[... truncated at 16 KB ...]")
            } else {
                (content.as_str(), "")
            };
            format!(
                "{base}\n\n## Project instructions ({})\n{}{}",
                path.display(),
                body,
                note
            )
        }
    }
}

/// Walk up from `cwd` (at most 8 levels) looking for AGENTS.md or CLAUDE.md
/// in that order per directory. Returns the read file on the first hit.
fn find_project_doc(cwd: &Path) -> Option<(std::path::PathBuf, String)> {
    let mut dir = Some(cwd.to_path_buf());
    for _ in 0..8 {
        let d = dir?;
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let candidate = d.join(name);
            if candidate.is_file() {
                if let Ok(content) = std::fs::read_to_string(&candidate) {
                    if !content.trim().is_empty() {
                        return Some((candidate, content));
                    }
                }
            }
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// The per-turn environment block: UTC date, platform, git status, and diff
/// stat. Empty string when the directory is not a git repository (caller
/// omits the block entirely) — non-git projects skip git entirely.
pub async fn env_snapshot(cwd: &Path) -> String {
    let date = utc_date();
    let status = git_output(
        cwd,
        &["status", "--porcelain"],
        MAX_STATUS_LINES,
        "status lines omitted",
    )
    .await;
    if status.is_none() {
        // Not a git repo (or git missing): date-only snapshot.
        return format!("Date (UTC): {date}\nPlatform: {}", std::env::consts::OS);
    }
    let status = status.unwrap_or_default();
    let diffstat = git_output(
        cwd,
        &["diff", "--stat"],
        MAX_DIFFSTAT_LINES,
        "diffstat lines omitted",
    )
    .await
    .unwrap_or_default();
    let untracked_note = if status.lines().any(|l| l.starts_with("??")) {
        "\nUntracked files exist; inspect them before assuming a clean tree."
    } else {
        ""
    };
    format!(
        "Date (UTC): {date}\nPlatform: {}\n\nGit status:\n{}\nGit diff (uncommitted, vs index):\n{}{}",
        std::env::consts::OS,
        status,
        diffstat,
        untracked_note
    )
}

/// Run `git <args>` capped and timeout-guarded. `None` when the directory is
/// not a git repository (or git is unavailable at all). An async subprocess
/// with a timeout future: a wedged git is killed on drop (`kill_on_drop`)
/// instead of stalling — or thread-spawning for — the async turn.
async fn git_output(
    cwd: &Path,
    args: &[&str],
    max_lines: usize,
    omit_note: &str,
) -> Option<String> {
    let out = tokio::time::timeout(
        GIT_TIMEOUT,
        tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let out = match out {
        Ok(Ok(o)) => o,
        Ok(Err(_)) => return None, // no git binary
        Err(_) => return None,     // wedged: treat as not-a-repo
    };
    if !out.status.success() {
        // `git status` in a non-repo exits nonzero: treat as not-a-repo.
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Some(String::new());
    }
    if lines.len() > max_lines {
        let mut capped: Vec<String> = lines[..max_lines].iter().map(|s| s.to_string()).collect();
        capped.push(format!("[... {} {omit_note} ...]", lines.len() - max_lines));
        Some(capped.join("\n"))
    } else {
        Some(text.trim_end().to_string())
    }
}

/// UTC `YYYY-MM-DD` from the system clock (Howard Hinnant's civil-from-days;
/// no chrono dependency).
fn utc_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let days = now.as_secs() / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 → (year, month, day). See "chrono-compatible"
/// civil_from_days reference implementation.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_doc_found_and_injected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Rules\nBe terse.").unwrap();
        let out = append_project_doc("BASE", tmp.path());
        assert!(out.starts_with("BASE"));
        assert!(out.contains("## Project instructions"));
        assert!(out.contains("Be terse."));
    }

    #[test]
    fn claudemd_is_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Claude rules").unwrap();
        let out = append_project_doc("BASE", tmp.path());
        assert!(out.contains("Claude rules"));
    }

    #[test]
    fn agents_wins_over_claudemd_in_same_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "agents-wins").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude-loses").unwrap();
        let out = append_project_doc("BASE", tmp.path());
        assert!(out.contains("agents-wins"));
        assert!(!out.contains("claude-loses"));
    }

    #[test]
    fn parent_directory_doc_found() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "from-parent").unwrap();
        let out = append_project_doc("BASE", &sub);
        assert!(out.contains("from-parent"));
    }

    #[test]
    fn no_doc_leaves_base() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(append_project_doc("BASE", tmp.path()), "BASE");
    }

    #[test]
    fn oversized_doc_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_PROJECT_DOC_BYTES + 5_000);
        std::fs::write(tmp.path().join("AGENTS.md"), big).unwrap();
        let out = append_project_doc("BASE", tmp.path());
        assert!(out.contains("[... truncated at 16 KB ...]"));
        assert!(out.len() < MAX_PROJECT_DOC_BYTES + 2_000);
    }

    #[tokio::test]
    async fn snapshot_in_git_repo_has_status() {
        // This test runs inside the conga workspace (a git repo).
        let cwd = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let snap = env_snapshot(cwd).await;
        assert!(snap.contains("Date (UTC):"));
        assert!(snap.contains("Platform:"));
        assert!(snap.contains("Git status:"));
    }

    #[tokio::test]
    async fn snapshot_in_plain_dir_is_date_only() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = env_snapshot(tmp.path()).await;
        assert!(snap.contains("Date (UTC):"));
        assert!(!snap.contains("Git status:"), "non-git dir: {snap}");
    }

    #[test]
    fn find_project_doc_returns_nothing_without_a_doc() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_project_doc(tmp.path()).is_none());
    }

    #[test]
    fn find_project_doc_returns_the_path_and_the_content() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("AGENTS.md");
        std::fs::write(&file, "# Rules\nBe terse.").unwrap();
        let (path, content) = find_project_doc(tmp.path()).unwrap();
        assert_eq!(path, file);
        assert!(content.contains("Be terse."));
    }

    #[test]
    fn find_project_doc_prefers_agents_md_over_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "agents-wins").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude-loses").unwrap();
        let (path, content) = find_project_doc(tmp.path()).unwrap();
        assert_eq!(path.file_name().unwrap(), "AGENTS.md");
        assert!(content.contains("agents-wins"));
    }

    #[test]
    fn find_project_doc_skips_a_blank_file_and_keeps_walking_up() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        // A whitespace-only doc is not a doc: the search continues.
        std::fs::write(sub.join("AGENTS.md"), "   \n\t\n").unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "from-parent").unwrap();
        let (path, content) = find_project_doc(&sub).unwrap();
        assert_eq!(path, tmp.path().join("AGENTS.md"));
        assert!(content.contains("from-parent"));
    }

    #[test]
    fn find_project_doc_stops_after_eight_levels() {
        // The walk-up is capped at 8 directories (the start dir + 7
        // ancestors): a doc that sits 8 levels above the start is NOT found.
        let tmp = tempfile::tempdir().unwrap();
        let mut deep = tmp.path().to_path_buf();
        for _ in 0..8 {
            deep = deep.join("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "too-far").unwrap();
        assert!(
            find_project_doc(&deep).is_none(),
            "the 8-level cap must hold"
        );
        // One level less is still found.
        let mut near = tmp.path().to_path_buf();
        for _ in 0..7 {
            near = near.join("d");
        }
        std::fs::create_dir_all(&near).unwrap();
        assert!(find_project_doc(&near).is_some());
    }

    #[tokio::test]
    async fn git_output_returns_none_outside_a_git_repository() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            git_output(tmp.path(), &["status", "--porcelain"], 64, "omitted")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn git_output_returns_none_for_a_failing_command() {
        let cwd = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // Inside the repo, but the ref does not exist -> nonzero exit.
        assert!(git_output(
            cwd,
            &["rev-parse", "--verify", "no-such-ref"],
            64,
            "omitted"
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn git_output_returns_an_empty_string_for_empty_output() {
        let cwd = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = git_output(cwd, &["log", "--oneline", "-n", "0"], 10, "omitted")
            .await
            .unwrap();
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn git_output_caps_the_line_count_and_appends_the_note() {
        let cwd = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = git_output(
            cwd,
            &["log", "--oneline", "-n", "5"],
            2,
            "log lines omitted",
        )
        .await
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "2 kept lines + the omission note: {out}");
        assert!(lines[2].contains("[... 3 log lines omitted ...]"), "{out}");
    }

    #[tokio::test]
    async fn git_output_keeps_short_output_verbatim() {
        let cwd = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = git_output(cwd, &["rev-parse", "--is-inside-work-tree"], 64, "omitted")
            .await
            .unwrap();
        assert_eq!(out, "true");
    }

    #[test]
    fn civil_from_days_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn utc_date_shape() {
        let d = utc_date();
        assert_eq!(d.len(), 10, "{d}");
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
    }

    #[test]
    fn custom_prompt_swaps_head_keeps_tail() {
        let assembled = format!(
            "{}\n\n## Project instructions (/repo/AGENTS.md)\nBe terse.\n\n## Skills\n\n- name: x",
            CODING_AGENT_PROMPT
        );
        let out = with_custom_base_prompt(&assembled, Some("You are a pirate."));
        assert!(
            out.starts_with("You are a pirate.\n\n## Project instructions"),
            "{out}"
        );
        assert!(!out.contains("software engineering agent"), "{out}");
        assert!(out.contains("Be terse."), "{out}");
        assert!(out.ends_with("- name: x"), "{out}");
    }

    #[test]
    fn custom_prompt_skills_only_tail() {
        // No project doc: skills section is still preserved.
        let assembled = format!("{CODING_AGENT_PROMPT}\n\n## Skills\n\n- name: x");
        let out = with_custom_base_prompt(&assembled, Some("  custom  "));
        assert_eq!(out, "custom\n\n## Skills\n\n- name: x");
    }

    #[test]
    fn custom_prompt_blank_or_none_keeps_built_in() {
        let assembled = format!("{CODING_AGENT_PROMPT}\n\n## Skills\n\n- name: x");
        assert_eq!(with_custom_base_prompt(&assembled, None), assembled);
        assert_eq!(with_custom_base_prompt(&assembled, Some("  ")), assembled);
        // bare prompt with no appended sections: custom replaces it whole
        assert_eq!(
            with_custom_base_prompt(CODING_AGENT_PROMPT, Some("plain")),
            "plain"
        );
    }
}
