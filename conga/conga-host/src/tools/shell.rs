//! Persistent per-session shell: `cd`, exported env vars, and activated
//! virtualenvs survive across `bash` tool calls within a session.
//!
//! One `sh` child process per session id (process-global registry; `ToolContext`
//! is per-call and cannot carry handles). Commands are fed over stdin with a
//! sentinel protocol — the wrapper prints a marker + exit status when a
//! command finishes, so we know exactly where output ends. The stdout reader
//! lives IN the session (`BufReader<ChildStdout>`), so partial reads never
//! lose bytes between calls. A per-session [`tokio::sync::Mutex`] serializes
//! commands (a shell has one stdin).
//!
//! On timeout the shell is killed and evicted: a timed-out command may have
//! left the shell wedged (blocked read, unbalanced quote), so the next call
//! starts a fresh shell and says so in its output.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Cap on bytes buffered by [`collect`] while a command runs (spill and
/// truncation only happen later, at the bash-tool layer). Reuses the
/// shared output cap plus headroom for the cap/exit markers.
const SHELL_COLLECT_CAP: usize = crate::tools::MAX_OUTPUT_BYTES + 8 * 1024;

/// Per-run completion marker: `__CONGA_DONE_<uuid>__`. A fresh random nonce
/// per command means user output can never contain THIS run's sentinel
/// line — the fixed-marker collision (`echo '__CONGA_DONE__ 0'` truncating
/// output and forging an exit code) is structurally impossible.
fn new_sentinel() -> String {
    format!("__CONGA_DONE_{}__", uuid::Uuid::new_v4().simple())
}

/// One live shell and its I/O, owned by the registry entry.
struct ShellSession {
    /// Kept (never read) so `kill_on_drop` reaps the process tree when the
    /// session is evicted.
    #[allow(dead_code)]
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

impl ShellSession {
    fn spawn(cwd: &std::path::Path, env: &HashMap<String, String>) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.current_dir(cwd)
            .env_clear()
            // Don't leak conga's own config/secrets (e.g. CONGA_LLM_KEY).
            .envs(env.iter().filter(|(k, _)| !k.starts_with("CONGA_")))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }
}

type Shared = std::sync::Arc<tokio::sync::Mutex<ShellSession>>;

/// Registry of live shells, keyed by session id. An evicted entry is simply
/// respawned on the next call — a shell is cheap, correctness is not.
fn registry() -> &'static tokio::sync::Mutex<HashMap<String, Shared>> {
    static REG: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, Shared>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Combined stdout+stderr of one command run through the persistent shell.
pub struct ShellOutcome {
    pub output: String,
    /// Exit status parsed from the sentinel line. `None` when the run did
    /// not complete (timeout, shell death, spawn failure).
    pub exit_code: Option<i32>,
}

impl ShellOutcome {
    fn incomplete(output: String) -> Self {
        Self {
            output,
            exit_code: None,
        }
    }
}
#[allow(clippy::too_many_arguments)]
pub async fn run(
    session_id: &str,
    command: &str,
    timeout: Duration,
    cwd: &std::path::Path,
    env: &HashMap<String, String>,
    background_log_dir: Option<&std::path::Path>,
) -> ShellOutcome {
    // Background mode: redirect to a log file, return immediately after fork.
    // The completion marker carries a per-run nonce: only THIS run's exact
    // marker line terminates `collect`, so output echoing sentinel-shaped
    // text (e.g. `echo '__CONGA_DONE__ 0'`) passes through untouched.
    let sentinel = new_sentinel();
    let mut log_path: Option<std::path::PathBuf> = None;
    let wrapped: String = match background_log_dir {
        Some(dir) => {
            let _ = std::fs::create_dir_all(dir);
            let log = dir.join(format!("bg-{}.log", millis()));
            log_path = Some(log.clone());
            format!(
                "{{ {command} ; }} > '{}' 2>&1 & printf '%s 0\\n' {sentinel}\n",
                log.display()
            )
        }
        None => format!("{{ {command} ; }} 2>&1\nprintf \"%s $?\\n\" {sentinel}\n"),
    };

    // Fetch (or respawn) the session; serial commands per session.
    let session = match get_or_spawn(session_id, cwd, env).await {
        Some(s) => s,
        None => {
            return ShellOutcome::incomplete("[shell unavailable: could not spawn sh]".into());
        }
    };
    let mut guard = session.lock().await;

    if guard.stdin.write_all(wrapped.as_bytes()).await.is_err() {
        // Shell died under us: drop, respawn once, retry.
        drop(guard);
        evict(session_id).await;
        let session = match get_or_spawn(session_id, cwd, env).await {
            Some(s) => s,
            None => {
                return ShellOutcome::incomplete("[shell unavailable: could not spawn sh]".into());
            }
        };
        let mut guard = session.lock().await;
        if guard.stdin.write_all(wrapped.as_bytes()).await.is_err() {
            return ShellOutcome::incomplete(
                "[shell unavailable: write failed after respawn]".into(),
            );
        }
        return finish(
            collect(&mut guard, timeout, session_id, &sentinel).await,
            log_path,
        );
    }
    finish(
        collect(&mut guard, timeout, session_id, &sentinel).await,
        log_path,
    )
}

/// Post-process one outcome: background runs name their log file so the
/// model knows where to poll.
fn finish(mut outcome: ShellOutcome, log_path: Option<std::path::PathBuf>) -> ShellOutcome {
    if let Some(log) = log_path {
        outcome
            .output
            .push_str(&format!("\n[background output -> {}]", log.display()));
    }
    outcome
}

/// Read output until this run's sentinel line; enforce the timeout. On
/// timeout or shell death the caller's registry entry is evicted
/// (kill_on_drop reaps the process tree once the Arc refs drop).
///
/// Buffered output is capped at [`SHELL_COLLECT_CAP`] during collection
/// itself: once the cap is hit, further lines are read (and discarded) until
/// the sentinel arrives — a runaway `cat huge.file` can no longer buffer
/// gigabytes in RAM while the spill/truncate layer only runs later. The cap
/// mirrors [`crate::tools::MAX_OUTPUT_BYTES`](crate::tools) but sits
/// deliberately above it: collect must also hold the truncation marker plus
/// whatever finish/post-processing appends.
async fn collect(
    guard: &mut tokio::sync::MutexGuard<'_, ShellSession>,
    timeout: Duration,
    session_id: &str,
    sentinel: &str,
) -> ShellOutcome {
    let mut out = String::new();
    let mut capped = false;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read_one = guard.stdout.read_until(b'\n', &mut buf);
        match tokio::time::timeout(timeout, read_one).await {
            Err(_) => {
                evict(session_id).await;
                return ShellOutcome::incomplete(format!(
                    "{out}\n[exit timeout] command exceeded {}s; shell session was reset",
                    timeout.as_secs()
                ));
            }
            Ok(Ok(0)) => {
                evict(session_id).await;
                return ShellOutcome::incomplete(format!("{out}\n[shell died; session reset]"));
            }
            Ok(Ok(_)) => {
                let line = String::from_utf8_lossy(&buf);
                if let Some(rest) = line.strip_prefix(sentinel) {
                    let code = rest.trim();
                    if capped {
                        out.push_str("\n[... output capped at ");
                        out.push_str(&SHELL_COLLECT_CAP.to_string());
                        out.push_str(" bytes; later output discarded ...]");
                    }
                    out.push_str(&format!("[exit {code}]"));
                    return ShellOutcome {
                        output: out,
                        exit_code: code.parse().ok(),
                    };
                }
                // Keep buffering while under the cap; past it, only the
                // sentinel line matters (still consumed above).
                if !capped {
                    if out.len() + line.len() > SHELL_COLLECT_CAP {
                        let room = SHELL_COLLECT_CAP.saturating_sub(out.len());
                        let mut cut = room;
                        while cut > 0 && !line.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        out.push_str(&line[..cut]);
                        capped = true;
                    } else {
                        out.push_str(&line);
                    }
                }
            }
            Ok(Err(e)) => {
                evict(session_id).await;
                return ShellOutcome::incomplete(format!(
                    "{out}\n[shell read error: {e}; session reset]"
                ));
            }
        }
    }
}

async fn get_or_spawn(
    session_id: &str,
    cwd: &std::path::Path,
    env: &HashMap<String, String>,
) -> Option<Shared> {
    let mut reg = registry().lock().await;
    if let Some(existing) = reg.get(session_id) {
        return Some(std::sync::Arc::clone(existing));
    }
    let session = ShellSession::spawn(cwd, env).ok()?;
    let shared = std::sync::Arc::new(tokio::sync::Mutex::new(session));
    reg.insert(session_id.to_string(), std::sync::Arc::clone(&shared));
    Some(shared)
}

async fn evict(session_id: &str) {
    // Dropping the registry's Arc lets kill_on_drop reap the tree once the
    // in-flight caller's guard releases.
    registry().lock().await.remove(session_id);
}

/// Kill and forget the session's persistent shell (if any). Public: called
/// on session delete / last-connection close (`session_cleanup`) so a
/// long-running host doesn't accumulate idle shells for dead sessions. A
/// later call for the same id simply respawns.
pub async fn evict_session(session_id: &str) {
    evict(session_id).await;
}

/// Milliseconds since epoch, for background log file names.
fn millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run_in(session: &str, cmd: &str) -> String {
        run(
            session,
            cmd,
            Duration::from_secs(10),
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            None,
        )
        .await
        .output
    }

    #[tokio::test]
    async fn cwd_persists_across_calls() {
        let out = run_in("t1-cwd", "cd /tmp && pwd").await;
        assert!(out.contains("/tmp"), "{out}");
        let b = run_in("t1-cwd", "pwd").await;
        assert!(b.contains("/tmp"), "cwd must persist: {b}");
    }

    #[tokio::test]
    async fn env_export_persists() {
        run_in("t2-env", "export MYVAR=hello42").await;
        let b = run_in("t2-env", "printf %s $MYVAR").await;
        assert!(b.contains("hello42"), "env must persist: {b}");
    }

    #[tokio::test]
    async fn timeout_resets_session() {
        let out = run(
            "t3-timeout",
            "sleep 30",
            Duration::from_secs(1),
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            None,
        )
        .await
        .output;
        assert!(out.contains("shell session was reset"), "{out}");
        let b = run_in("t3-timeout", "echo alive").await;
        assert!(
            b.contains("alive"),
            "session must be usable after reset: {b}"
        );
    }

    #[tokio::test]
    async fn background_returns_and_logs() {
        let dir = std::env::temp_dir().join("conga-bg-test-unique");
        let _ = std::fs::remove_dir_all(&dir);
        let out = run(
            "t4-bg",
            "echo delayed-output",
            Duration::from_secs(5),
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            Some(&dir),
        )
        .await
        .output;
        assert!(out.contains("bg-"), "must reference the log name: {out}");
        assert!(out.contains("[exit 0]"), "{out}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let logged = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .any(|e| {
                std::fs::read_to_string(e.path())
                    .map(|c| c.contains("delayed-output"))
                    .unwrap_or(false)
            });
        assert!(logged, "background log must capture output");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exit_status_reported() {
        // `exit N` in the top-level command would terminate the persistent
        // shell itself; an inner sh reports the code and the shell survives.
        let out = run_in("t5-exit", "sh -c 'exit 3'").await;
        assert!(out.contains("[exit 3]"), "{out}");
    }

    #[tokio::test]
    async fn sentinel_like_output_is_not_truncated() {
        // Echoing the legacy fixed marker (and any sentinel-shaped line)
        // must pass through as plain output: no early truncation, no
        // forged exit code — only this run's random nonce terminates.
        let out = run_in("t8-sentinel", "echo '__CONGA_DONE__ 0'; echo survived").await;
        assert!(
            out.contains("__CONGA_DONE__ 0"),
            "marker text must survive: {out}"
        );
        assert!(
            out.contains("survived"),
            "output after a forged marker must not be cut: {out}"
        );
        assert!(out.contains("[exit 0]"), "real exit code must win: {out}");
        assert!(
            !out.contains("[exit 7]"),
            "forged code must be ignored: {out}"
        );
    }

    #[tokio::test]
    async fn mirror_of_bash_tool_parameters() {
        // Exact parameters the bash tool passes: tempdir cwd, full process
        // env, uuid session, default timeout. Isolates shell.rs from the
        // bash.rs glue.
        let tmp = tempfile::tempdir().unwrap();
        let session = format!("bash-mirror-{}", uuid::Uuid::new_v4());
        let out = run(
            &session,
            "echo hello",
            Duration::from_secs(120),
            tmp.path(),
            &std::env::vars().collect(),
            None,
        )
        .await
        .output;
        assert!(out.contains("hello"), "{out:?}");
        assert!(out.contains("[exit 0]"), "{out:?}");
    }

    #[tokio::test]
    async fn evict_session_removes_entry_and_respawns_clean() {
        run_in("t7-evict", "cd /").await;
        assert!(registry().lock().await.contains_key("t7-evict"));
        evict_session("t7-evict").await;
        assert!(
            !registry().lock().await.contains_key("t7-evict"),
            "evict must remove the registry entry"
        );
        // Fresh shell: the `cd /` must NOT have survived the eviction
        // (spawn cwd is back; on macOS /tmp may print as /private/tmp,
        // so assert the property that actually matters: not "/").
        let b = run_in("t7-evict", "pwd").await;
        assert!(b.contains("[exit 0]"), "respawn must work: {b}");
        assert_ne!(
            b.lines().next().unwrap().trim(),
            "/",
            "cwd must reset after eviction: {b}"
        );
    }

    #[tokio::test]
    async fn secrets_stripped_from_env() {
        let mut env = HashMap::new();
        env.insert("CONGA_LLM_KEY".to_string(), "sk-secret".to_string());
        env.insert("NORMAL_VAR".to_string(), "ok".to_string());
        let out = run(
            "t6-secrets",
            "printf %s \"$CONGA_LLM_KEY|$NORMAL_VAR\"",
            Duration::from_secs(5),
            std::path::Path::new("/tmp"),
            &env,
            None,
        )
        .await
        .output;
        assert!(out.contains("|ok"), "normal var must survive: {out}");
        assert!(
            !out.contains("sk-secret"),
            "CONGA_* must be stripped: {out}"
        );
    }

    /// A command emitting far past [`SHELL_COLLECT_CAP`] must not buffer it
    /// all: the collected output is capped, carries the cap marker, and the
    /// run still reports the real exit sentinel (0).
    #[tokio::test]
    async fn oversized_output_is_capped_with_marker_and_exit() {
        // ~3x the cap in 1KB lines; fast to emit, impossible to mistake.
        let lines = SHELL_COLLECT_CAP / 1024 * 3;
        let session = format!("t9-cap-{}", uuid::Uuid::new_v4());
        let outcome = run(
            &session,
            &format!("i=1; while [ $i -le {lines} ]; do printf 'x%.0s' $(seq 1 1024); printf '\\n'; i=$((i+1)); done"),
            Duration::from_secs(60),
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            None,
        )
        .await;
        assert!(
            outcome.output.contains("output capped at"),
            "cap marker must be present (output len {})",
            outcome.output.len()
        );
        // The buffer itself stayed at (or under) the cap plus markers.
        assert!(
            outcome.output.len() <= SHELL_COLLECT_CAP + 200,
            "collected output must be bounded, got {}",
            outcome.output.len()
        );
        // The shell survives: next call runs clean (bytes were consumed,
        // not abandoned mid-stream).
        let next = run_in(&session, "echo alive").await;
        assert!(next.contains("alive"), "shell must stay usable: {next}");
        assert!(next.contains("[exit 0]"), "{next}");
    }

    // ---- new_sentinel ----

    /// The completion marker carries a per-run nonce: two runs must never
    /// share one, or user output could forge an exit code.
    #[test]
    fn new_sentinel_is_unique_per_call() {
        let a = new_sentinel();
        let b = new_sentinel();
        assert_ne!(a, b, "every run needs its own marker");
        assert!(a.starts_with("__CONGA_DONE_"), "{a}");
        assert!(a.ends_with("__"), "{a}");
        assert!(!a.contains(' '), "the marker must be a single token: {a}");
    }

    // ---- ShellSession::spawn ----

    /// A spawned session is wired up: stdin accepts a command and stdout
    /// carries its output.
    #[tokio::test]
    async fn spawn_creates_a_usable_shell_session() {
        let mut session = ShellSession::spawn(std::path::Path::new("/tmp"), &HashMap::new())
            .expect("sh must be spawnable");
        session
            .stdin
            .write_all(b"echo spawned-ok\n")
            .await
            .expect("stdin must accept the command");
        let mut buf = Vec::new();
        session
            .stdout
            .read_until(b'\n', &mut buf)
            .await
            .expect("stdout must yield a line");
        assert_eq!(String::from_utf8_lossy(&buf).trim(), "spawned-ok");
    }

    /// Error path: a working directory that does not exist cannot host a
    /// shell.
    #[test]
    fn spawn_fails_for_a_missing_working_directory() {
        // `ShellSession` has no `Debug` impl, so `expect_err` is not
        // available — match on the result instead.
        match ShellSession::spawn(
            std::path::Path::new("/conga-no-such-dir-xyz"),
            &HashMap::new(),
        ) {
            Ok(_) => panic!("spawning into a missing dir must fail"),
            Err(e) => assert!(!e.to_string().is_empty()),
        }
    }

    // ---- ShellOutcome::incomplete / finish ----

    /// An incomplete run (timeout, shell death) has no exit code.
    #[test]
    fn incomplete_has_no_exit_code() {
        let outcome = ShellOutcome::incomplete("partial output".into());
        assert_eq!(outcome.output, "partial output");
        assert_eq!(outcome.exit_code, None);
    }

    /// A background run names its log file so the model knows where to poll.
    #[test]
    fn finish_appends_the_background_log_path() {
        let outcome = finish(
            ShellOutcome::incomplete("done".into()),
            Some(std::path::PathBuf::from("/tmp/bg-1.log")),
        );
        assert!(
            outcome
                .output
                .contains("[background output -> /tmp/bg-1.log]"),
            "{}",
            outcome.output
        );
        assert!(
            outcome.output.starts_with("done"),
            "the collected output must be kept: {}",
            outcome.output
        );
    }

    /// A foreground run is left exactly as collected.
    #[test]
    fn finish_without_a_log_path_leaves_the_output_untouched() {
        let outcome = finish(
            ShellOutcome {
                output: "[exit 0]".into(),
                exit_code: Some(0),
            },
            None,
        );
        assert_eq!(outcome.output, "[exit 0]");
        assert_eq!(outcome.exit_code, Some(0));
    }

    // ---- get_or_spawn / evict ----

    /// The registry hands out ONE session per id: a second call reuses the
    /// live shell instead of spawning another one.
    #[tokio::test]
    async fn get_or_spawn_reuses_the_existing_session() {
        let id = format!("t10-reuse-{}", uuid::Uuid::new_v4());
        let first = get_or_spawn(&id, std::path::Path::new("/tmp"), &HashMap::new())
            .await
            .expect("first spawn");
        let second = get_or_spawn(&id, std::path::Path::new("/tmp"), &HashMap::new())
            .await
            .expect("second spawn");
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "the same session id must reuse the same shell"
        );
        assert!(registry().lock().await.contains_key(&id));
        evict(&id).await;
    }

    /// Different ids get different shells.
    #[tokio::test]
    async fn get_or_spawn_creates_one_session_per_id() {
        let a_id = format!("t11-a-{}", uuid::Uuid::new_v4());
        let b_id = format!("t11-b-{}", uuid::Uuid::new_v4());
        let a = get_or_spawn(&a_id, std::path::Path::new("/tmp"), &HashMap::new())
            .await
            .expect("spawn a");
        let b = get_or_spawn(&b_id, std::path::Path::new("/tmp"), &HashMap::new())
            .await
            .expect("spawn b");
        assert!(
            !std::sync::Arc::ptr_eq(&a, &b),
            "different ids must not share a shell"
        );
        evict(&a_id).await;
        evict(&b_id).await;
    }

    /// `evict` drops the registry entry; a later call respawns from scratch.
    #[tokio::test]
    async fn evict_removes_the_registry_entry() {
        let id = format!("t12-evict-{}", uuid::Uuid::new_v4());
        get_or_spawn(&id, std::path::Path::new("/tmp"), &HashMap::new())
            .await
            .expect("spawn");
        assert!(registry().lock().await.contains_key(&id));
        evict(&id).await;
        assert!(
            !registry().lock().await.contains_key(&id),
            "evict must drop the registry entry"
        );
    }

    /// Evicting an unknown id is a no-op (no panic).
    #[tokio::test]
    async fn evict_of_an_unknown_session_is_a_no_op() {
        evict("t13-never-existed").await;
        assert!(!registry().lock().await.contains_key("t13-never-existed"));
    }

    // ---- millis ----

    /// The background log file name is derived from a millisecond
    /// timestamp: it must be a real timestamp and must not go backwards.
    #[test]
    fn millis_returns_a_non_decreasing_timestamp() {
        let first = millis();
        let second = millis();
        assert!(first > 0, "the clock must be past the epoch");
        assert!(second >= first, "time must not go backwards");
    }
}
