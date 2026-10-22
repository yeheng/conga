//! `terminal` tool — run commands on a PTY, with run/read/send actions and a
//! per-session output ring buffer. Lives in conga-ext behind Cargo feature
//! `terminal`; the session registry is process-global within this crate.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use conga::{
    ContentBlock, ExtensionApi, RiskLevel, ToolCallCtx, ToolDefinition, ToolError, ToolResult,
};
use parking_lot::{Mutex, RwLock};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Rolling output buffer for one PTY session, capped at MAX_BYTES: pushing
/// past the cap evicts whole oldest chunks until back under it.
#[derive(Default)]
struct OutputRing {
    chunks: VecDeque<String>,
    bytes: usize,
}

impl OutputRing {
    const MAX_BYTES: usize = 64 * 1024;

    fn push_str(&mut self, s: &str) {
        let mut s = s.to_string();
        if s.len() > Self::MAX_BYTES {
            // Char-safe tail: never slice through a multi-byte char.
            let mut cut = Self::MAX_BYTES;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            s = s[cut..].to_string();
        }
        self.bytes += s.len();
        self.chunks.push_back(s);
        while self.bytes > Self::MAX_BYTES {
            let Some(front) = self.chunks.pop_front() else {
                break;
            };
            self.bytes -= front.len();
        }
    }

    /// Take everything buffered (empty string when nothing new).
    fn drain(&mut self) -> String {
        let out: String = self.chunks.drain(..).collect();
        self.bytes = 0;
        out
    }

    /// Whether nothing is buffered — the sweep's "fully read" precondition.
    fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

/// How many bytes of `bytes` can be decoded now. When a trailing multi-byte
/// char is split across reads (`error_len() == None`, at most 4 bytes), hold
/// it back for the next chunk; genuinely invalid tails are flushed in full
/// (the caller's lossy decode replaces them).
fn utf8_split(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) => {
            let valid = e.valid_up_to();
            if e.error_len().is_none() && bytes.len() - valid <= 4 {
                valid
            } else {
                bytes.len()
            }
        }
    }
}

struct PtySession {
    child: Box<dyn portable_pty::Child + Send>,
    /// Consumed by the `send` action; must be taken at spawn time because
    /// `take_writer` succeeds only once per master.
    writer: Box<dyn Write + Send>,
    ring: Arc<Mutex<OutputRing>>,
    /// Set by the pump thread (Release) after it hit EOF/error and flushed
    /// its final bytes. Together with a reaped child this is the only safe
    /// reap point for the session: exit alone doesn't prove the last output
    /// landed in the ring yet.
    reader_done: Arc<AtomicBool>,
}

/// Sessions keyed by `<tool session_id>/<name>`; same global-state pattern
/// as conga's `proxy.rs` override. Dead sessions are reaped on
/// `read` (exited + fully drained) and swept on the next `run` — see
/// [`reap_dead_sessions`].
static REGISTRY: LazyLock<RwLock<HashMap<String, Arc<Mutex<PtySession>>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Same registration shape as `search.rs::register`.
pub fn register(api: &mut dyn ExtensionApi) {
    // The registry is process-global, so its cleanup must ride the host's
    // process-global hook too (idempotent, once per process): session
    // delete / last-connection-close kills this session's PTYs instead of
    // waiting for a passive `read`/`run` sweep that may never come.
    static CLEANUP_HOOK: OnceLock<()> = OnceLock::new();
    if CLEANUP_HOOK.set(()).is_ok() {
        conga_host::session_cleanup::register_hook(Arc::new(evict_session_terminals));
    }
    api.register_tool(ToolDefinition {
        name: "terminal".into(),
        label: "Terminal".into(),
        description: "Run commands on a PTY. action: run (spawn), read (drain new \
                      output + exit status), send (write to stdin)."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["run", "read", "send"] },
                "command": { "type": "string", "description": "command (run) or input line (send)" },
                "session": { "type": "string", "description": "session name (default \"default\")" }
            },
            "required": ["action"]
        }),
        risk: RiskLevel::High,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    });
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, ToolError> {
    let action = ctx.args["action"]
        .as_str()
        .ok_or_else(|| ToolError::Message("action is required".into()))?;
    let session_name = ctx.args["session"].as_str().unwrap_or("default");
    let key = format!("{}/{}", ctx.ctx.session_id, session_name);
    match action {
        "run" => run(&ctx, &key),
        "read" => read(&key).await,
        "send" => send(&ctx, &key).await,
        other => Ok(ToolResult::error(format!("unknown action: {other}"))),
    }
}

fn run(ctx: &ToolCallCtx, key: &str) -> Result<ToolResult, ToolError> {
    let command = ctx.args["command"]
        .as_str()
        .ok_or_else(|| ToolError::Message("command is required for run".into()))?;

    // Replace any live session under this key: SIGHUP then reap, so children
    // don't linger as zombies. The registry write guard is dropped at the
    // statement end — a wedged session lock below can't freeze other sessions.
    let old = REGISTRY.write().remove(key);
    if let Some(old) = old {
        let mut s = old.lock();
        let _ = s.child.kill();
        let _ = s.child.wait();
    }

    // Sessions the model stopped reading after their child exited would
    // otherwise accumulate forever; sweep them before spawning the new one.
    reap_dead_sessions();

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            ..Default::default()
        })
        .map_err(|e| ToolError::Message(format!("openpty failed: {e}")))?;
    let mut cmd = CommandBuilder::new(if cfg!(target_os = "windows") {
        "cmd"
    } else {
        "sh"
    });
    if cfg!(target_os = "windows") {
        cmd.arg("/C");
    } else {
        cmd.arg("-c");
    }
    cmd.arg(command);
    cmd.cwd(&ctx.ctx.cwd);
    // Env is taken from ToolContext with CONGA_* filtered — same rule as core's
    // bash tool. CommandBuilder inherits the raw process env by default, which
    // still holds CONGA_* secrets, so clear it and re-inject explicitly.
    cmd.env_clear();
    for (k, v) in &ctx.ctx.env {
        if !k.starts_with("CONGA_") {
            cmd.env(k, v);
        }
    }
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| ToolError::Message(format!("spawn failed: {e}")))?;
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| ToolError::Message(format!("pty reader failed: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| ToolError::Message(format!("pty writer failed: {e}")))?;

    let ring = Arc::new(Mutex::new(OutputRing::default()));
    let pump_ring = Arc::clone(&ring);
    let reader_done = Arc::new(AtomicBool::new(false));
    let pump_done = Arc::clone(&reader_done);
    // Blocking reads live on a plain thread — portable-pty is sync IO. The
    // reader holds its own dup of the master fd, so dropping `pair` below is
    // safe; the thread exits on EOF when the child closes its end.
    std::thread::spawn(move || {
        let mut carry: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Carry residual bytes across reads so a multi-byte char
                    // split at a chunk boundary isn't turned into two U+FFFD.
                    carry.extend_from_slice(&buf[..n]);
                    let cut = utf8_split(&carry);
                    pump_ring
                        .lock()
                        .push_str(&String::from_utf8_lossy(&carry[..cut]));
                    carry.drain(..cut);
                }
            }
        }
        if !carry.is_empty() {
            pump_ring.lock().push_str(&String::from_utf8_lossy(&carry));
        }
        // Publish after the final flush so a reaper that observes `done`
        // knows no more bytes will ever land in the ring.
        pump_done.store(true, Ordering::Release);
    });

    REGISTRY.write().insert(
        key.to_string(),
        Arc::new(Mutex::new(PtySession {
            child,
            writer,
            ring,
            reader_done,
        })),
    );
    Ok(ToolResult {
        content: vec![ContentBlock::text(format!("session `{key}` started"))],
        details: serde_json::json!({"session": key}),
        is_error: false,
    })
}

async fn read(key: &str) -> Result<ToolResult, ToolError> {
    let Some(sess) = REGISTRY.read().get(key).cloned() else {
        return Ok(ToolResult {
            content: vec![ContentBlock::text("no active session")],
            details: serde_json::json!({"exited": true}),
            is_error: false,
        });
    };
    // Lock scope ends before any await; child poll is non-blocking. The ring
    // itself bounds output at 64KiB — core's spill_or_truncate is pub(crate)
    // and not exported, so ext returns the drained text directly (consistent
    // with ext's search/web tools, which don't spill either).
    let (mut text, exit_code, reapable) = {
        let mut s = sess.lock();
        let out = s.ring.lock().drain();
        // try_wait: Ok(Some(status)) = exited, Ok(None) = still running.
        let status = s
            .child
            .try_wait()
            .map_err(|e| ToolError::Message(format!("wait failed: {e}")))?;
        let exit_code = status.map(|st| st.exit_code() as i32);
        // Reap only when the child is gone AND the pump thread has flushed
        // its last byte (`reader_done`): exit alone doesn't prove the final
        // output landed in the ring yet. The drained text is returned in
        // this same call, so nothing is lost by removing the session.
        let reapable = exit_code.is_some() && s.reader_done.load(Ordering::Acquire);
        (out, exit_code, reapable)
    };
    if reapable {
        remove_if_current(key, &sess);
    }
    if let Some(code) = exit_code {
        text.push_str(&format!("\n[exited code {code}]"));
    }
    Ok(ToolResult {
        content: vec![ContentBlock::text(text.trim())],
        details: serde_json::json!({
            "exited": exit_code.is_some(),
            "exit_code": exit_code,
        }),
        is_error: false,
    })
}
/// Remove `key` from the registry only if it still maps to `sess` — a
/// concurrent `run` under the same key must keep its fresh session.
fn remove_if_current(key: &str, sess: &Arc<Mutex<PtySession>>) {
    let mut reg = REGISTRY.write();
    if reg.get(key).is_some_and(|v| Arc::ptr_eq(v, sess)) {
        reg.remove(key);
    }
}

/// Sweep sessions whose child has exited and whose pump thread finished
/// (final output already landed in a ring nobody will read again). Called
/// from `run` before spawning a replacement — without it, sessions the
/// model stops reading after exit accumulate forever in the process-global
/// registry (child handles, 64KiB rings, reader-thread Arcs).
///
/// `try_lock` per session: a busy session (concurrent read/send) is simply
/// skipped — the next sweep gets it; a wedged session lock can never block
/// the sweep or the registry.
fn reap_dead_sessions() {
    let snapshot: Vec<(String, Arc<Mutex<PtySession>>)> = REGISTRY
        .read()
        .iter()
        .map(|(k, v)| (k.clone(), Arc::clone(v)))
        .collect();
    for (key, sess) in snapshot {
        let dead = match sess.try_lock() {
            Some(mut s) => {
                s.child.try_wait().is_ok_and(|st| st.is_some())
                    && s.reader_done.load(Ordering::Acquire)
                    // Undrained output must stay readable by a later
                    // `read` — only fully-read sessions may be culled.
                    && s.ring.lock().is_empty()
            }
            None => false,
        };
        if dead {
            remove_if_current(&key, &sess);
        }
    }
}

/// Kill and remove EVERY PTY session belonging to `session_id` (registry
/// keys are `<session_id>/<name>`; ids cannot contain `/`, so the prefix
/// match is exact). Public: hosts call it (directly or via
/// `conga_host::session_cleanup`) when the session is deleted or its last
/// connection closes - dead children and their 64KiB rings must not linger
/// in a long-running host process.
pub fn evict_session_terminals(session_id: &str) {
    let prefix = format!("{session_id}/");
    let victims: Vec<Arc<Mutex<PtySession>>> = {
        let mut reg = REGISTRY.write();
        let keys: Vec<String> = reg
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        keys.iter().filter_map(|k| reg.remove(k)).collect()
    };
    for v in victims {
        let mut s = v.lock();
        let _ = s.child.kill();
        let _ = s.child.wait();
    }
}

async fn send(ctx: &ToolCallCtx, key: &str) -> Result<ToolResult, ToolError> {
    let input = ctx.args["command"]
        .as_str()
        .ok_or_else(|| ToolError::Message("command is required for send".into()))?;
    let Some(sess) = REGISTRY.read().get(key).cloned() else {
        return Ok(ToolResult::error(format!("no session `{key}`")));
    };
    // Blocking stdin writes run on a std thread, not a tokio worker; the
    // session lock lives entirely inside the closure, never across an await.
    let input = input.to_string();
    let res = tokio::task::spawn_blocking(move || {
        let mut s = sess.lock();
        // PTY stdin is line-oriented: always terminate with a newline.
        s.writer
            .write_all(input.as_bytes())
            .and_then(|_| s.writer.write_all(b"\n"))
            .and_then(|_| s.writer.flush())
    })
    .await
    .map_err(|e| ToolError::Message(format!("stdin write failed: {e}")))?;
    res.map_err(|e| ToolError::Message(format!("stdin write failed: {e}")))?;
    Ok(ToolResult {
        content: vec![ContentBlock::text("sent")],
        details: serde_json::json!({"session": key}),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_returns_and_clears() {
        let mut r = OutputRing::default();
        r.push_str("hello\n");
        r.push_str("world\n");
        assert_eq!(r.drain(), "hello\nworld\n");
        assert_eq!(r.drain(), "", "second drain is empty");
    }

    #[test]
    fn cap_evicts_oldest_first() {
        let mut r = OutputRing::default();
        // 2 chunks of half the cap (plus 1) -> over cap after the second push.
        r.push_str(&"a".repeat(OutputRing::MAX_BYTES / 2));
        r.push_str(&"b".repeat(OutputRing::MAX_BYTES / 2 + 1));
        let out = r.drain();
        assert!(out.starts_with('b'), "oldest chunk evicted first");
        assert!(out.len() <= OutputRing::MAX_BYTES);
    }

    #[test]
    fn oversized_single_chunk_is_truncated_to_cap() {
        let mut r = OutputRing::default();
        r.push_str(&"x".repeat(OutputRing::MAX_BYTES * 2));
        assert!(r.drain().len() <= OutputRing::MAX_BYTES);
    }

    #[test]
    fn oversized_chunk_keeps_tail_on_char_boundary() {
        let mut r = OutputRing::default();
        // MAX_BYTES - 1 x's: cut lands mid-é, so the walk-back loop must run.
        r.push_str(&format!("{}é", "x".repeat(OutputRing::MAX_BYTES - 1)));
        let out = r.drain();
        assert!(
            out.ends_with('é'),
            "tail must keep the trailing multi-byte char"
        );
        assert!(out.len() <= OutputRing::MAX_BYTES);
    }

    // ── Tool-level integration (mirrors search.rs's proxy wiring test) ──

    use conga::{ContentBlock, ExtensionApiImpl, ToolCallCtx, ToolContext, ToolResult};
    use std::sync::Arc;

    fn registered_tool() -> conga::ToolDefinition {
        let mut api = ExtensionApiImpl::new();
        super::register(&mut api);
        assert_eq!(api.tools.len(), 1);
        assert_eq!(api.tools[0].name, "terminal");
        api.tools.remove(0)
    }

    async fn exec_with_env(
        args: serde_json::Value,
        cwd: &std::path::Path,
        session: &str,
        env: std::collections::HashMap<String, String>,
    ) -> ToolResult {
        let t = registered_tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env,
                session_id: session.into(),
                state_dir: cwd.to_path_buf(),
            },
        })
        .await
        .unwrap()
    }

    async fn exec(args: serde_json::Value, cwd: &std::path::Path, session: &str) -> ToolResult {
        exec_with_env(args, cwd, session, std::env::vars().collect()).await
    }

    fn text(r: &ToolResult) -> String {
        match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        }
    }

    /// Integration tests share the process-global REGISTRY, and every `run`
    /// sweeps ALL dead sessions in it — one test's sweep would cull
    /// another's fixture mid-test (e.g. the "still present" precondition in
    /// run_sweeps_dead_sessions). Serialize every test that calls `run`;
    /// tokio's Mutex because the guard is held across awaits.
    static REGISTRY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn run_then_read_returns_output_and_exit() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        // unique session key so parallel tests never share a registry slot
        let s = format!("run-read-{}", std::process::id());
        let r = exec(
            serde_json::json!({"action": "run", "command": "echo hello"}),
            tmp.path(),
            &s,
        )
        .await;
        assert!(!r.is_error, "spawn failed");
        // Poll read until the child exits and output shows up (pump thread is async).
        let mut got = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = exec(serde_json::json!({"action": "read"}), tmp.path(), &s).await;
            got = text(&r);
            if got.contains("hello") && got.contains("[exited") {
                break;
            }
        }
        assert!(got.contains("hello"), "got: {got}");
        assert!(got.contains("[exited code 0]"), "got: {got}");
    }

    #[test]
    fn utf8_split_holds_back_incomplete_trailing_char() {
        // "中" = E4 B8 AD: a char split across two reads must be held back.
        assert_eq!(utf8_split(&[0xE4]), 0);
        assert_eq!(utf8_split(&[b'a', 0xE4, 0xB8]), 1);
        assert_eq!(utf8_split(&[0xE4, 0xB8, 0xAD]), 3);
    }

    #[test]
    fn utf8_split_flushes_genuinely_invalid_bytes() {
        // 0xFF can never start a UTF-8 sequence: no point holding it back.
        assert_eq!(utf8_split(&[0xFF, 0xFE]), 2);
        assert_eq!(utf8_split(&[b'a', 0xFF]), 2);
    }

    #[tokio::test]
    async fn run_scrubs_conga_env_and_passes_others_through() {
        let _registry = REGISTRY_LOCK.lock().await;
        std::env::set_var("CONGA_SENTINEL", "leak-me-12345");
        let tmp = tempfile::tempdir().unwrap();
        let s = format!("env-scrub-{}", std::process::id());
        // GASK_TEST_OK lives only in the ToolContext env, not the process env:
        // it must pass through. CONGA_SENTINEL is in both and must be scrubbed.
        let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
        env.insert("GASK_TEST_OK".into(), "keep-me".into());
        let r = exec_with_env(
            serde_json::json!({
                "action": "run",
                "command": "echo sentinel=${CONGA_SENTINEL:-unset} ok=$GASK_TEST_OK"
            }),
            tmp.path(),
            &s,
            env,
        )
        .await;
        assert!(!r.is_error, "spawn failed");
        let mut got = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = exec(serde_json::json!({"action": "read"}), tmp.path(), &s).await;
            got = text(&r);
            if got.contains("sentinel=") && got.contains("[exited") {
                break;
            }
        }
        assert!(got.contains("unset"), "CONGA_SENTINEL leaked: {got}");
        assert!(
            !got.contains("leak-me-12345"),
            "CONGA_SENTINEL leaked: {got}"
        );
        assert!(got.contains("keep-me"), "non-CONGA env dropped: {got}");
    }

    #[tokio::test]
    async fn run_then_read_preserves_multibyte_output() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let s = format!("utf8-{}", std::process::id());
        let r = exec(
            serde_json::json!({"action": "run", "command": "echo 中文测试"}),
            tmp.path(),
            &s,
        )
        .await;
        assert!(!r.is_error);
        let mut got = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = exec(serde_json::json!({"action": "read"}), tmp.path(), &s).await;
            got = text(&r);
            if got.contains("中文测试") && got.contains("[exited") {
                break;
            }
        }
        assert!(got.contains("中文测试"), "got: {got}");
    }

    #[tokio::test]
    async fn read_with_no_session_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let r = exec(
            serde_json::json!({"action": "read"}),
            tmp.path(),
            "never-spawned",
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(text(&r), "no active session");
    }

    #[tokio::test]
    async fn send_writes_to_stdin_of_running_child() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let s = format!("send-{}", std::process::id());
        // `read` a line then echo it back — proves stdin round-trip through the PTY.
        let r = exec(
            serde_json::json!({"action": "run", "command": "read line; echo got:$line"}),
            tmp.path(),
            &s,
        )
        .await;
        assert!(!r.is_error);
        let r = exec(
            serde_json::json!({"action": "send", "command": "ping"}),
            tmp.path(),
            &s,
        )
        .await;
        assert!(!r.is_error, "send failed");
        let mut got = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = exec(serde_json::json!({"action": "read"}), tmp.path(), &s).await;
            got = text(&r);
            if got.contains("got:ping") {
                break;
            }
        }
        assert!(got.contains("got:ping"), "got: {got}");
    }

    /// True once `key`'s child has exited AND its pump thread finished —
    /// the preconditions the reaper acts on — without draining the ring.
    fn session_is_dead(key: &str) -> bool {
        let reg = REGISTRY.read();
        let Some(sess) = reg.get(key) else {
            return true; // already reaped
        };
        let mut s = sess.lock();
        s.child.try_wait().is_ok_and(|st| st.is_some())
            && s.reader_done.load(std::sync::atomic::Ordering::Acquire)
    }

    #[tokio::test]
    async fn read_reaps_exited_and_fully_drained_session() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let s = format!("reap-read-{}", std::process::id());
        let r = exec(
            serde_json::json!({"action": "run", "command": "echo done"}),
            tmp.path(),
            &s,
        )
        .await;
        assert!(!r.is_error);
        // Poll read until the session is dead AND the reaper had its turn:
        // the first exited read may still race the pump thread's final
        // flush; a later one observes reader_done and removes the entry.
        let mut got = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = exec(serde_json::json!({"action": "read"}), tmp.path(), &s).await;
            got = text(&r);
            if got.contains("done")
                && got.contains("[exited code 0]")
                && !REGISTRY.read().contains_key(&format!("{s}/default"))
            {
                break;
            }
        }
        assert!(got.contains("done"), "got: {got}");
        assert!(
            !REGISTRY.read().contains_key(&format!("{s}/default")),
            "exited+drained session must leave the registry"
        );
        // A read after the reap is the honest "no active session" answer.
        let r = exec(serde_json::json!({"action": "read"}), tmp.path(), &s).await;
        assert_eq!(text(&r), "no active session");
    }

    #[tokio::test]
    async fn evict_session_terminals_removes_only_that_session() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let a = format!("evict-a-{}", std::process::id());
        let b = format!("evict-b-{}", std::process::id());
        let r = exec(
            serde_json::json!({"action": "run", "command": "read x; echo $x"}),
            tmp.path(),
            &a,
        )
        .await;
        assert!(!r.is_error);
        let r = exec(
            serde_json::json!({"action": "run", "command": "read x; echo $x"}),
            tmp.path(),
            &b,
        )
        .await;
        assert!(!r.is_error);
        assert!(REGISTRY.read().contains_key(&format!("{a}/default")));

        evict_session_terminals(&a);

        assert!(
            !REGISTRY.read().contains_key(&format!("{a}/default")),
            "evicted session's PTY must be removed"
        );
        assert!(
            REGISTRY.read().contains_key(&format!("{b}/default")),
            "other sessions must be untouched"
        );
    }

    #[tokio::test]
    async fn register_wires_ptys_into_host_session_cleanup() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let s = format!("hook-cleanup-{}", std::process::id());
        let r = exec(
            serde_json::json!({"action": "run", "command": "read x; echo $x"}),
            tmp.path(),
            &s,
        )
        .await;
        assert!(!r.is_error);
        assert!(REGISTRY.read().contains_key(&format!("{s}/default")));

        // The hook `register()` installed must kill this session's PTY when
        // the host runs session cleanup (delete / last-connection-close).
        conga_host::session_cleanup::run_hooks(&s);
        assert!(
            !REGISTRY.read().contains_key(&format!("{s}/default")),
            "session cleanup must evict the session's PTYs"
        );
    }

    #[tokio::test]
    async fn run_sweeps_dead_sessions_the_model_stopped_reading() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let a = format!("reap-sweep-a-{}", std::process::id());
        let r = exec(
            serde_json::json!({"action": "run", "command": "true"}),
            tmp.path(),
            &a,
        )
        .await;
        assert!(!r.is_error);
        // the session is cullable without dropping unread bytes.
        for _ in 0..50 {
            if session_is_dead(&format!("{a}/default")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            session_is_dead(&format!("{a}/default")),
            "test precondition: session must die first"
        );
        assert!(REGISTRY.read().contains_key(&format!("{a}/default")));

        // Any new run sweeps the registry before spawning.
        let b = format!("reap-sweep-b-{}", std::process::id());
        let r = exec(
            serde_json::json!({"action": "run", "command": "echo live"}),
            tmp.path(),
            &b,
        )
        .await;
        assert!(!r.is_error);
        assert!(
            !REGISTRY.read().contains_key(&format!("{a}/default")),
            "dead unread session must be swept by run"
        );
        assert!(REGISTRY.read().contains_key(&format!("{b}/default")));
    }

    // ── Direct tests of the session primitives ──────────────────

    /// A tool context for the direct (non-tool-dispatch) calls.
    fn ctx_for(session: &str, cwd: &std::path::Path, args: serde_json::Value) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env: std::env::vars().collect(),
                session_id: session.into(),
                state_dir: cwd.to_path_buf(),
            },
        }
    }

    #[test]
    fn ring_is_empty_until_something_is_pushed() {
        let mut r = OutputRing::default();
        assert!(r.is_empty(), "a fresh ring holds nothing");
        r.push_str("x");
        assert!(!r.is_empty(), "a pushed chunk makes it non-empty");
        assert_eq!(r.drain(), "x");
        assert!(r.is_empty(), "draining empties the ring again");
    }

    #[tokio::test]
    async fn run_spawns_a_pty_session_and_registers_it() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let session = format!("direct-run-{}", std::process::id());
        let key = format!("{session}/default");
        let ctx = ctx_for(
            &session,
            tmp.path(),
            serde_json::json!({"command": "echo hi"}),
        );

        let res = run(&ctx, &key).expect("run spawns a session");

        assert!(!res.is_error);
        assert!(text(&res).contains(&key), "got: {}", text(&res));
        assert_eq!(res.details["session"], key.as_str());
        assert!(
            REGISTRY.read().contains_key(&key),
            "the spawned session is registered"
        );
        evict_session_terminals(&session);
    }

    #[tokio::test]
    async fn run_without_a_command_is_rejected() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let session = format!("direct-run-err-{}", std::process::id());
        let key = format!("{session}/default");
        let ctx = ctx_for(&session, tmp.path(), serde_json::json!({}));

        let err = match run(&ctx, &key) {
            Ok(_) => panic!("run without a command must fail"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("command is required for run"),
            "{err}"
        );
        assert!(
            !REGISTRY.read().contains_key(&key),
            "a rejected run must not register a session"
        );
    }

    #[tokio::test]
    async fn remove_if_current_only_removes_the_matching_session() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let a = format!("direct-reap-a-{}", std::process::id());
        let b = format!("direct-reap-b-{}", std::process::id());
        // Both children wait for stdin: they stay alive for the whole test.
        for session in [&a, &b] {
            let ctx = ctx_for(
                session,
                tmp.path(),
                serde_json::json!({"command": "read x; echo $x"}),
            );
            run(&ctx, &format!("{session}/default")).expect("run spawns");
        }
        let sess_a = REGISTRY
            .read()
            .get(&format!("{a}/default"))
            .cloned()
            .unwrap();
        let sess_b = REGISTRY
            .read()
            .get(&format!("{b}/default"))
            .cloned()
            .unwrap();

        // A stale session handle must not evict the current one.
        remove_if_current(&format!("{a}/default"), &sess_b);
        assert!(
            REGISTRY.read().contains_key(&format!("{a}/default")),
            "a mismatched session must not be removed"
        );

        remove_if_current(&format!("{a}/default"), &sess_a);
        assert!(
            !REGISTRY.read().contains_key(&format!("{a}/default")),
            "the matching session must be removed"
        );
        assert!(REGISTRY.read().contains_key(&format!("{b}/default")));

        evict_session_terminals(&a);
        evict_session_terminals(&b);
    }

    #[tokio::test]
    async fn reap_dead_sessions_culls_only_dead_and_fully_read_sessions() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();

        // (1) dead child, no output at all -> cullable.
        let dead = format!("direct-reap-dead-{}", std::process::id());
        let ctx = ctx_for(&dead, tmp.path(), serde_json::json!({"command": "true"}));
        run(&ctx, &format!("{dead}/default")).expect("run spawns");
        for _ in 0..50 {
            if session_is_dead(&format!("{dead}/default")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            session_is_dead(&format!("{dead}/default")),
            "test precondition: the child must have exited"
        );

        // (2) dead child with unread output -> must survive (the bytes are
        // still readable through `read`).
        let unread = format!("direct-reap-unread-{}", std::process::id());
        let ctx = ctx_for(
            &unread,
            tmp.path(),
            serde_json::json!({"command": "echo buffered"}),
        );
        run(&ctx, &format!("{unread}/default")).expect("run spawns");
        for _ in 0..50 {
            if session_is_dead(&format!("{unread}/default")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(session_is_dead(&format!("{unread}/default")));

        reap_dead_sessions();

        assert!(
            !REGISTRY.read().contains_key(&format!("{dead}/default")),
            "a dead, fully-read session must be culled"
        );
        assert!(
            REGISTRY.read().contains_key(&format!("{unread}/default")),
            "a session with unread output must survive the sweep"
        );

        evict_session_terminals(&dead);
        evict_session_terminals(&unread);
    }

    #[tokio::test]
    async fn send_writes_to_the_stdin_of_a_running_session() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let session = format!("direct-send-{}", std::process::id());
        let key = format!("{session}/default");
        let ctx = ctx_for(
            &session,
            tmp.path(),
            serde_json::json!({"command": "read line; echo got:$line"}),
        );
        run(&ctx, &key).expect("run spawns");

        let send_ctx = ctx_for(&session, tmp.path(), serde_json::json!({"command": "ping"}));
        let res = send(&send_ctx, &key).await.expect("send writes");
        assert!(!res.is_error);
        assert_eq!(text(&res), "sent");
        assert_eq!(res.details["session"], key.as_str());

        let mut got = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            got = text(&read(&key).await.expect("read"));
            if got.contains("got:ping") {
                break;
            }
        }
        assert!(got.contains("got:ping"), "got: {got}");

        evict_session_terminals(&session);
    }

    #[tokio::test]
    async fn send_without_a_command_or_session_fails() {
        let _registry = REGISTRY_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let session = format!("direct-send-err-{}", std::process::id());
        let key = format!("{session}/default");

        let no_cmd = ctx_for(&session, tmp.path(), serde_json::json!({}));
        let err = match send(&no_cmd, &key).await {
            Ok(_) => panic!("send without a command must fail"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("command is required for send"),
            "{err}"
        );

        // No such session: an honest tool-level error, not a panic.
        let ctx = ctx_for(&session, tmp.path(), serde_json::json!({"command": "x"}));
        let res = send(&ctx, &key).await.expect("send returns a tool error");
        assert!(res.is_error);
        assert!(text(&res).contains("no session"), "got: {}", text(&res));
    }
}
