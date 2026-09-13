//! Append-only JSONL stores for sessions.
//!
//! Layout under `~/.conga/`:
//! ```text
//! sessions/
//!   {session_id}/
//!     events.jsonl     # append-only, one SessionEvent per line
//!     messages.jsonl   # legacy: append-only, one AgentMessage per line
//!     tool_state/{tool_name}/  # per-plugin private state
//! ```
//!
//! ## Format contract
//!
//! - **Single writer per session.** Appends open their own handle with
//!   `O_APPEND`, so a whole-line write is atomic against other writers, but
//!   concurrent writers can interleave batches (and a torn tail can only be
//!   produced by the writer that owns the file). Hosts must serialize appends
//!   to one session — the CLI and the gateway each run one loop per session.
//! - **Torn tails are crash artifacts, not data.** If the final line fails to
//!   parse (an append interrupted by crash/power loss), loading drops it and
//!   truncates the file in place: the interrupted turn was incomplete anyway,
//!   and this keeps later appends clean. A corrupt line in the **middle**
//!   fails the load with the file line number — that is real damage (bit rot,
//!   external edit), not a crash artifact.
//! - **Schema evolution is additive only.** New struct fields must carry
//!   `#[serde(default)]`; adding enum variants is a breaking change for
//!   readers built against the older file format.

use std::path::{Path, PathBuf};

use crate::error::AgentError;
use crate::types::message::AgentMessage;
use crate::types::session_event::SessionEvent;

/// The conga config/data root: `~/.conga/`.
///
/// Legacy fallback: pre-rename installs kept everything under `~/.gasket/`.
/// When `~/.conga` doesn't exist yet but `~/.gasket` does, keep using the
/// old root so existing sessions, `mcp.json`, and `app_config.json` stay
/// readable — adopting the new dir only on fresh machines.
pub fn config_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let new = home.join(".conga");
    let legacy = home.join(".gasket");
    if !new.exists() && legacy.exists() {
        legacy
    } else {
        new
    }
}

/// A session id must be a flat, safe identifier: non-empty, ASCII
/// alphanumeric + `-`/`_` only, at most 128 chars. Rejects `/`, `\`, `..` -
/// defends against path traversal when the id originates from untrusted input
/// (e.g. the gateway's `?user_id=` query param).
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn validate_session_id(id: &str) -> Result<(), AgentError> {
    if is_valid_session_id(id) {
        Ok(())
    } else {
        Err(AgentError::InvalidSessionId(id.to_string()))
    }
}

/// Append-only JSONL message store for sessions.
#[derive(Debug, Clone)]
pub struct JsonlStorage {
    base_dir: PathBuf,
}

impl JsonlStorage {
    /// Create a store rooted at `base_dir` (typically `~/.conga/sessions`).
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Default store at `<config_dir>/sessions`.
    pub fn default_root() -> Self {
        Self::new(config_dir().join("sessions"))
    }

    /// This store's root directory (used by the host to list sessions).
    pub fn base_dir_clone(&self) -> PathBuf {
        self.base_dir.clone()
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(session_id)
    }

    /// Path to a session's `messages.jsonl` (whether or not it exists yet).
    pub fn messages_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("messages.jsonl")
    }

    /// Append a single message to the session's JSONL log. Creates the session
    /// directory if missing.
    pub async fn append_message(
        &self,
        session_id: &str,
        msg: &AgentMessage,
    ) -> Result<(), AgentError> {
        validate_session_id(session_id)?;
        let path = self.messages_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        append_line(&mut file, msg).await
    }

    /// Append a batch of messages in order. Creates the session directory once
    /// and writes all lines to a single open file handle. Hosts call this after
    /// a run to persist the returned `Vec<AgentMessage>` transcript.
    pub async fn append_messages(
        &self,
        session_id: &str,
        msgs: &[AgentMessage],
    ) -> Result<(), AgentError> {
        if msgs.is_empty() {
            return Ok(());
        }
        validate_session_id(session_id)?;
        let path = self.messages_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        for msg in msgs {
            append_line(&mut file, msg).await?;
        }
        Ok(())
    }

    /// Load all messages for a session, in append order. Returns empty vec for
    /// a session that has never been written.
    ///
    /// Applies the torn-tail recovery policy (see the module docs): a final
    /// line that fails to parse is dropped and the file truncated in place;
    /// a corrupt line in the middle fails with its file line number.
    pub async fn load_messages(&self, session_id: &str) -> Result<Vec<AgentMessage>, AgentError> {
        validate_session_id(session_id)?;
        parse_transcript(&self.messages_path(session_id)).await
    }

    /// Load messages from an arbitrary JSONL file (used by tests/hosts that
    /// point at a specific file). Same recovery policy as
    /// [`load_messages`](Self::load_messages).
    pub async fn load_from_file(path: &Path) -> Result<Vec<AgentMessage>, AgentError> {
        parse_transcript(path).await
    }
}

/// Write one record as a single `line\n` buffer: one `write_all`, so a crash
/// can never leave a complete line dangling without its terminator — it leaves
/// either a full line or a truncated fragment, and a truncated final fragment
/// is what [`scan_jsonl`] repairs on the next load.
async fn append_line<T: serde::Serialize>(
    file: &mut tokio::fs::File,
    value: &T,
) -> Result<(), AgentError> {
    use tokio::io::AsyncWriteExt;
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    file.write_all(line.as_bytes()).await?;
    // `tokio::fs::File` buffers writes in userspace and completes
    // `write_all` as soon as the syscall is *submitted* to the blocking
    // pool. Awaiting `flush` waits for the write to actually land in the
    // kernel, so a caller that reads (or crashes) right after this
    // returns observes the line instead of racing the background write.
    file.flush().await?;
    Ok(())
}

/// Sync twin of [`append_line`]: one `write_all` of `line\n` on a
/// `std::fs` handle opened with `O_APPEND`. Same crash discipline — a torn
/// write leaves a fragment that [`scan_jsonl`] heals as a torn tail.
fn append_line_sync<T: serde::Serialize>(
    file: &mut std::fs::File,
    value: &T,
) -> Result<(), AgentError> {
    use std::io::Write;
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Parse a message transcript, applying the torn-tail recovery policy.
///
/// Thin wrapper over the generic [`scan_jsonl`] scanner; see that function
/// (and the module docs) for the exact semantics. The legacy `messages.jsonl`
/// behavior is frozen: `fail_closed_on_data` is off, so any unparseable last
/// line — whatever the error class — heals as a torn tail.
async fn parse_transcript(path: &Path) -> Result<Vec<AgentMessage>, AgentError> {
    scan_jsonl::<AgentMessage>(path, true, false).await
}

/// Scan a JSONL file into `T` rows, applying the torn-tail recovery policy.
///
/// A missing file is an empty log. Returns `Err(AgentError::Transcript)`
/// naming the file line for a corrupt line in the middle of the file (real
/// damage). If only the **last** line is invalid it is a torn tail (an append
/// interrupted by crash/power loss): the line is dropped and — when
/// `repair_tail` is set — the file is truncated at that line's start, so
/// loading succeeds with the preceding records and later appends stay clean.
///
/// When `fail_closed_on_data` is set, a deserialization **data** error
/// (`serde_json::error::Category::Data` — e.g. a complete row whose `"type"`
/// tag matches no known variant) fails the load regardless of position and
/// never truncates. A byte-truncated write can only produce a `Syntax`/`Eof`
/// error, so a `Data` error on the last line is version skew (a newer conga
/// wrote a row this reader does not know — by definition the most recent
/// line), and healing it away would silently destroy data.
async fn scan_jsonl<T: serde::de::DeserializeOwned>(
    path: &Path,
    repair_tail: bool,
    fail_closed_on_data: bool,
) -> Result<Vec<T>, AgentError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let (items, torn) = scan_jsonl_buffer(&bytes, path, fail_closed_on_data)?;
    if let Some(torn) = torn {
        warn_torn_tail(path, &torn);
        if repair_tail {
            repair_torn_tail(path, torn.start).await?;
        }
    }
    Ok(items)
}

/// Sync twin of [`scan_jsonl`] on `std::fs`, for engines that already run
/// on a blocking thread (the session-index reindex in `spawn_blocking`).
/// Identical policy — the parse core is shared, so the two cannot drift.
fn scan_jsonl_sync<T: serde::de::DeserializeOwned>(
    path: &Path,
    repair_tail: bool,
    fail_closed_on_data: bool,
) -> Result<Vec<T>, AgentError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let (items, torn) = scan_jsonl_buffer(&bytes, path, fail_closed_on_data)?;
    if let Some(torn) = torn {
        warn_torn_tail(path, &torn);
        if repair_tail {
            repair_torn_tail_sync(path, torn.start)?;
        }
    }
    Ok(items)
}

/// A torn-tail verdict from [`scan_jsonl_buffer`]: the bad line's number,
/// its start byte offset (the truncation point), and the parse error.
struct TornTail {
    line_no: usize,
    start: usize,
    err: serde_json::Error,
}

fn warn_torn_tail(path: &Path, torn: &TornTail) {
    tracing::warn!(
        path = %path.display(),
        line = torn.line_no,
        error = %torn.err,
        "dropping torn transcript tail"
    );
}

/// Pure scan of an in-memory JSONL buffer into rows plus an optional torn
/// tail. Shared core of [`scan_jsonl`] and [`scan_jsonl_sync`] — all the
/// line-walking and error-precedence policy lives here, the wrappers only
/// own file IO and the tail repair.
///
/// A parse failure is only a torn tail if no further line follows it
/// (i.e. it is the last non-blank line). Defer the verdict instead of
/// rescanning the remainder per line — one pass, O(n).
fn scan_jsonl_buffer<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    path: &Path,
    fail_closed_on_data: bool,
) -> Result<(Vec<T>, Option<TornTail>), AgentError> {
    let mut items = Vec::new();
    let mut line_start = 0usize;
    let mut line_no = 0usize;
    let mut pending: Option<TornTail> = None;
    for (idx, b) in bytes.iter().enumerate() {
        if *b != b'\n' {
            continue;
        }
        line_no += 1;
        let this_line_start = line_start;
        let line = bytes[this_line_start..idx].trim_ascii();
        line_start = idx + 1;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<T>(line) {
            Ok(m) => {
                if let Some(bad) = pending.take() {
                    // A good line after the bad one proves the bad line was
                    // mid-file damage, not a torn tail.
                    return Err(AgentError::Transcript(format!(
                        "invalid line {} in {}: {}",
                        bad.line_no,
                        path.display(),
                        bad.err
                    )));
                }
                items.push(m);
            }
            Err(e) => {
                if fail_closed_on_data && e.classify() == serde_json::error::Category::Data {
                    return Err(AgentError::Transcript(format!(
                        "invalid line {line_no} in {}: {e}",
                        path.display()
                    )));
                }
                if let Some(bad) = pending.take() {
                    // A second bad line proves the first was mid-file too.
                    return Err(AgentError::Transcript(format!(
                        "invalid line {} in {}: {}",
                        bad.line_no,
                        path.display(),
                        bad.err
                    )));
                }
                pending = Some(TornTail {
                    line_no,
                    start: this_line_start,
                    err: e,
                });
            }
        }
    }
    // Trailing fragment after the last newline (no terminator yet).
    let tail = bytes[line_start..].trim_ascii();
    if !tail.is_empty() {
        line_no += 1;
        match serde_json::from_slice::<T>(tail) {
            Ok(m) => {
                if let Some(bad) = pending.take() {
                    return Err(AgentError::Transcript(format!(
                        "invalid line {} in {}: {}",
                        bad.line_no,
                        path.display(),
                        bad.err
                    )));
                }
                items.push(m);
            }
            Err(e) => {
                // A torn fragment after a bad complete line proves the bad
                // line was mid-file (old semantics: error, not double-heal).
                if let Some(bad) = pending.take() {
                    return Err(AgentError::Transcript(format!(
                        "invalid line {} in {}: {}",
                        bad.line_no,
                        path.display(),
                        bad.err
                    )));
                }
                if fail_closed_on_data && e.classify() == serde_json::error::Category::Data {
                    return Err(AgentError::Transcript(format!(
                        "invalid line {line_no} in {}: {e}",
                        path.display()
                    )));
                }
                return Ok((
                    items,
                    Some(TornTail {
                        line_no,
                        start: line_start,
                        err: e,
                    }),
                ));
            }
        }
    }
    // A bad complete line with nothing but (optional) blank lines after it is
    // the torn tail: drop it, and truncate the file at its start so later
    // appends land after valid data.
    Ok((items, pending))
}

/// Truncate the transcript at `keep_until` (the byte offset where a torn line
/// starts), so later appends land after valid data and future loads never
/// re-hit the bad line.
async fn repair_torn_tail(path: &Path, keep_until: usize) -> Result<(), AgentError> {
    let file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    file.set_len(keep_until as u64).await?;
    Ok(())
}

/// Sync twin of [`repair_torn_tail`].
fn repair_torn_tail_sync(path: &Path, keep_until: usize) -> Result<(), AgentError> {
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(keep_until as u64)?;
    Ok(())
}

/// User-facing session metadata (the `meta.json` sidecar). Purely additive:
/// every field is optional so older readers ignore newer fields.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub name: Option<String>,
}

/// Append-only JSONL session event store: `events.jsonl`, one
/// [`SessionEvent`] per line, written with the same discipline (one
/// `O_APPEND` handle, single `write_all` of `line\n`) and read with the same
/// torn-tail policy as [`JsonlStorage`]. `messages.jsonl` in the same
/// session directory is the legacy format; [`EventStorage`] can read it as
/// the migration source and remove it once migrated.
#[derive(Debug, Clone)]
pub struct EventStorage {
    root: PathBuf,
}

impl EventStorage {
    /// Create a store rooted at `root` (typically `~/.conga/sessions`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    /// Path to a session's `events.jsonl` (whether or not it exists yet).
    pub fn events_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("events.jsonl")
    }

    /// Path to a session's legacy `messages.jsonl` — the migration source.
    pub fn messages_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("messages.jsonl")
    }

    /// Whether the session has an event log on disk.
    pub fn has_events(&self, session_id: &str) -> bool {
        is_valid_session_id(session_id) && self.events_path(session_id).exists()
    }

    /// Append a single event to the session's JSONL log. Creates the session
    /// directory if missing.
    pub async fn append_event(
        &self,
        session_id: &str,
        ev: &SessionEvent,
    ) -> Result<(), AgentError> {
        validate_session_id(session_id)?;
        let path = self.events_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        append_line(&mut file, ev).await
    }

    /// Append a batch of events in order to a single open file handle.
    /// An empty batch is a no-op (no directory or file is created).
    pub async fn append_events(
        &self,
        session_id: &str,
        evs: &[SessionEvent],
    ) -> Result<(), AgentError> {
        if evs.is_empty() {
            return Ok(());
        }
        validate_session_id(session_id)?;
        let path = self.events_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        for ev in evs {
            append_line(&mut file, ev).await?;
        }
        Ok(())
    }

    /// Synchronous single-event append — same `O_APPEND` + single
    /// `write_all` discipline as [`append_event`](Self::append_event), on
    /// `std::fs`. For sync persist callbacks (the agent loop's persist
    /// closure) so they never have to bridge onto the async runtime.
    pub fn append_event_sync(&self, session_id: &str, ev: &SessionEvent) -> Result<(), AgentError> {
        validate_session_id(session_id)?;
        let mut file = open_append_sync(&self.events_path(session_id))?;
        append_line_sync(&mut file, ev)
    }

    /// Synchronous batch twin of [`append_events`](Self::append_events):
    /// rows stream onto the live log through one `O_APPEND` handle. An
    /// empty batch is a no-op (no directory or file is created).
    pub fn append_events_sync(
        &self,
        session_id: &str,
        evs: &[SessionEvent],
    ) -> Result<(), AgentError> {
        if evs.is_empty() {
            return Ok(());
        }
        validate_session_id(session_id)?;
        let mut file = open_append_sync(&self.events_path(session_id))?;
        for ev in evs {
            append_line_sync(&mut file, ev)?;
        }
        Ok(())
    }

    /// Path to a session's in-flight `events.jsonl.tmp` — the staging file
    /// for [`append_events_atomic`](Self::append_events_atomic). A leftover
    /// from a crashed write is invisible to
    /// [`has_events`](Self::has_events)/[`load_events`](Self::load_events),
    /// which only ever look at `events.jsonl`.
    fn events_tmp_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("events.jsonl.tmp")
    }

    /// Atomically install a full batch of events as the session's event log:
    /// the batch is written to `events.jsonl.tmp`, synced, then renamed onto
    /// `events.jsonl` (rename is atomic on POSIX). Unlike
    /// [`append_events`](Self::append_events) — which streams rows onto the
    /// live log and can leave a torn prefix if the process dies mid-batch —
    /// a crash here can never produce a partial `events.jsonl`: either the
    /// complete log exists or only the `.tmp` does, and the next
    /// `append_events_atomic` call replaces the stale `.tmp` wholesale.
    /// An empty batch is a no-op (no directory or file is created).
    pub async fn append_events_atomic(
        &self,
        session_id: &str,
        evs: &[SessionEvent],
    ) -> Result<(), AgentError> {
        if evs.is_empty() {
            return Ok(());
        }
        validate_session_id(session_id)?;
        let dest = self.events_path(session_id);
        let tmp = self.events_tmp_path(session_id);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(&tmp).await?;
        for ev in evs {
            append_line(&mut file, ev).await?;
        }
        // Flush the data to disk before the rename so the rename boundary
        // (not page cache luck) is what makes the log durable.
        file.sync_all().await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
    }

    /// Load all events for a session, in append order. Returns empty vec for
    /// a session that has never been written. Same torn-tail policy as
    /// [`JsonlStorage::load_messages`], plus fail-closed version-skew
    /// handling: a complete row this reader does not understand fails the
    /// load instead of being healed away as a torn tail.
    pub async fn load_events(&self, session_id: &str) -> Result<Vec<SessionEvent>, AgentError> {
        validate_session_id(session_id)?;
        scan_jsonl::<SessionEvent>(&self.events_path(session_id), true, true).await
    }

    /// Synchronous twin of [`load_events`](Self::load_events) on `std::fs`,
    /// for engines that already run on a blocking thread (the session-index
    /// reindex inside `spawn_blocking`). Identical torn-tail and
    /// fail-closed policy — the parse core is shared.
    pub fn load_events_sync(&self, session_id: &str) -> Result<Vec<SessionEvent>, AgentError> {
        validate_session_id(session_id)?;
        scan_jsonl_sync::<SessionEvent>(&self.events_path(session_id), true, true)
    }

    /// Load the legacy `messages.jsonl` for a session — the migration source.
    /// Returns empty vec when there is nothing to migrate. Legacy semantics
    /// are frozen: identical recovery policy to
    /// [`JsonlStorage::load_messages`].
    pub async fn load_messages(&self, session_id: &str) -> Result<Vec<AgentMessage>, AgentError> {
        validate_session_id(session_id)?;
        scan_jsonl::<AgentMessage>(&self.messages_path(session_id), true, false).await
    }

    /// Remove the session's legacy `messages.jsonl` after a successful
    /// migration. Other files in the session directory are untouched; a
    /// missing file is not an error.
    pub async fn delete_legacy(&self, session_id: &str) -> Result<(), AgentError> {
        validate_session_id(session_id)?;
        match tokio::fs::remove_file(self.messages_path(session_id)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Path to a session's `meta.json` sidecar (user-facing metadata such as
    /// the display name), next to `events.jsonl`.
    pub fn meta_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("meta.json")
    }

    /// Load the session's metadata sidecar. A missing or unreadable file is
    /// not an error — metadata is optional decoration, never transcript data.
    pub async fn load_meta(&self, session_id: &str) -> Option<SessionMeta> {
        if !is_valid_session_id(session_id) {
            return None;
        }
        let bytes = tokio::fs::read(self.meta_path(session_id)).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Synchronous twin of [`load_meta`](Self::load_meta).
    pub fn load_meta_sync(&self, session_id: &str) -> Option<SessionMeta> {
        if !is_valid_session_id(session_id) {
            return None;
        }
        let bytes = std::fs::read(self.meta_path(session_id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Persist session metadata (tmp + rename, the same crash discipline as
    /// [`append_events_atomic`](Self::append_events_atomic)). Creates the
    /// session directory if missing, so a session can be named before its
    /// first turn lands on disk.
    pub async fn write_meta(&self, session_id: &str, meta: &SessionMeta) -> Result<(), AgentError> {
        validate_session_id(session_id)?;
        let dest = self.meta_path(session_id);
        let tmp = self.session_dir(session_id).join("meta.json.tmp");
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut payload = serde_json::to_vec(meta)?;
        payload.push(b'\n');
        tokio::fs::write(&tmp, payload).await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
    }

    /// Remove the session directory wholesale (event log, legacy transcript,
    /// meta sidecar). Returns `Ok(false)` when the session never existed.
    pub async fn remove_session(&self, session_id: &str) -> Result<bool, AgentError> {
        validate_session_id(session_id)?;
        match tokio::fs::remove_dir_all(self.session_dir(session_id)).await {
            Ok(()) => {
                // Legacy layouts kept per-tool state (and spill files) at
                // `<config_dir>/tool_state/<session_id>/`, outside the session
                // dir; a deleted session must not leave those behind. Best
                // effort: their absence is not an error. Current state lives
                // inside the session dir and dies with the remove above.
                let legacy_tool_state = crate::storage::config_dir()
                    .join("tool_state")
                    .join(session_id);
                let _ = tokio::fs::remove_dir_all(&legacy_tool_state).await;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// Open `path` for `O_APPEND` creation, creating the parent directory only
/// when the open proves it missing. The sync append paths are the agent
/// loop's per-event persist callback — an unconditional `create_dir_all`
/// there walks the directory tree on every event; the NotFound fallback
/// pays for directory creation exactly once per session.
fn open_append_sync(path: &Path) -> Result<std::fs::File, AgentError> {
    let open = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    };
    match open() {
        Ok(file) => Ok(file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Ok(open()?)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::message::{ContentBlock, UserMessage};
    #[test]
    fn config_dir_prefers_new_root_and_falls_back_to_legacy() {
        // `config_dir` reads `dirs::home_dir`, which on unix follows $HOME.
        // Point it at a temp home and assert the adopt/fallback contract:
        // legacy `~/.gasket` alone → keep using it (data continuity);
        // once `~/.conga` exists → new root wins going forward.
        let tmp = tempfile::tempdir().unwrap();
        let saved = std::env::var("HOME").ok();
        // SAFETY: test-only env mutation; this test runs serially with
        // other storage tests that don't read HOME.
        std::env::set_var("HOME", tmp.path());

        std::fs::create_dir_all(tmp.path().join(".gasket")).unwrap();
        assert!(config_dir().ends_with(".gasket"));

        std::fs::create_dir_all(tmp.path().join(".conga")).unwrap();
        assert!(config_dir().ends_with(".conga"));

        match saved {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(text)],
            timestamp: 42,
        })
    }

    #[tokio::test]
    async fn append_then_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());

        store
            .append_message("s1", &user_msg("hello"))
            .await
            .unwrap();
        store
            .append_message("s1", &user_msg("world"))
            .await
            .unwrap();

        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 2);
        // Order preserved.
        assert!(
            matches!(&loaded[0], AgentMessage::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "hello"))
        );
        assert!(
            matches!(&loaded[1], AgentMessage::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "world"))
        );
    }

    #[tokio::test]
    async fn append_messages_batch_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let batch = vec![user_msg("a"), user_msg("b"), user_msg("c")];
        store.append_messages("s1", &batch).await.unwrap();

        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(
            store.messages_path("s1"),
            tmp.path().join("s1").join("messages.jsonl")
        );
    }

    #[tokio::test]
    async fn append_messages_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        store.append_messages("s1", &[]).await.unwrap();
        // No file created for an empty batch.
        assert!(!store.messages_path("s1").exists());
    }

    #[tokio::test]
    async fn load_missing_session_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let loaded = store.load_messages("never-existed").await.unwrap();
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn append_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        // Session dir does not exist yet.
        store.append_message("s1", &user_msg("x")).await.unwrap();
        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 1);
    }

    #[tokio::test]
    async fn rejects_path_traversal_session_id() {
        // A session id carrying path components must never reach the filesystem.
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        for bad in ["../evil", "nested/s1", "/etc", "..", "a\\b", ""] {
            assert!(
                store.append_message(bad, &user_msg("x")).await.is_err(),
                "{bad:?} should be rejected"
            );
            assert!(
                store.append_messages(bad, &[user_msg("x")]).await.is_err(),
                "{bad:?} should be rejected (batch)"
            );
            assert!(
                store.load_messages(bad).await.is_err(),
                "{bad:?} should be rejected (load)"
            );
        }
        // Nothing was written outside the store root.
        assert!(!tmp.path().join("../evil").exists());
    }

    #[tokio::test]
    async fn event_sync_append_round_trips_through_async_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());

        store
            .append_event_sync("s1", &SessionEvent::TurnStart)
            .unwrap();
        store
            .append_events_sync("s1", &[user_msg_event("a"), user_msg_event("b")])
            .unwrap();

        let loaded = store.load_events("s1").await.unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0], SessionEvent::TurnStart);
        // Sync append is visible to a fresh async reader in append order.
        assert!(matches!(&loaded[1], SessionEvent::User(m) if m == &user_msg("a")));
    }

    #[test]
    fn event_sync_append_rejects_bad_id_and_empty_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        assert!(store
            .append_event_sync("../evil", &SessionEvent::TurnStart)
            .is_err());
        // Empty batch is a no-op: no directory or file is created.
        store.append_events_sync("s1", &[]).unwrap();
        assert!(!store.events_path("s1").exists());
    }

    fn user_msg_event(text: &str) -> SessionEvent {
        SessionEvent::User(user_msg(text))
    }

    // ── Torn-tail recovery ────────────────────────────────────────

    fn raw_line(msg: &AgentMessage) -> String {
        serde_json::to_string(msg).unwrap()
    }

    fn write_raw(store: &JsonlStorage, session_id: &str, content: &str) {
        let path = store.messages_path(session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// A truncated final line (write interrupted by crash/power loss) must be
    /// dropped, the file repaired in place, and later appends must load clean.
    #[tokio::test]
    async fn torn_tail_is_dropped_and_file_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());

        let good = raw_line(&user_msg("ok"));
        let torn = &good[..good.len() / 2]; // mid-JSON truncation
        write_raw(&store, "s1", &format!("{good}\n{torn}"));

        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 1, "torn tail must be dropped, prefix kept");
        assert!(
            matches!(&loaded[0], AgentMessage::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "ok"))
        );

        // File on disk is repaired: only the good line (plus terminator) remains.
        let raw = std::fs::read_to_string(store.messages_path("s1")).unwrap();
        assert_eq!(
            raw,
            format!("{good}\n"),
            "file must be truncated at the torn line"
        );

        // Appending after the repair works and never re-hits the bad line.
        store
            .append_message("s1", &user_msg("after"))
            .await
            .unwrap();
        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(
            matches!(&loaded[1], AgentMessage::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "after"))
        );
    }

    /// A torn tail that still ends with a newline (crash after the `\n` of a
    /// garbage line) must be repaired the same way.
    #[tokio::test]
    async fn torn_tail_with_newline_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let good = raw_line(&user_msg("ok"));
        write_raw(&store, "s1", &format!("{good}\nNOT_JSON\n"));

        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 1);
        let raw = std::fs::read_to_string(store.messages_path("s1")).unwrap();
        assert_eq!(raw, format!("{good}\n"));
    }

    /// A file whose only line is torn (first write crashed) becomes an empty
    /// but usable session.
    #[tokio::test]
    async fn single_torn_line_becomes_empty_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let good = raw_line(&user_msg("ok"));
        write_raw(&store, "s1", &good[..good.len() / 2]);

        assert!(store.load_messages("s1").await.unwrap().is_empty());
        store
            .append_message("s1", &user_msg("first"))
            .await
            .unwrap();
        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 1);
    }

    /// A corrupt line in the middle is real damage, not a crash artifact: the
    /// load must fail loudly with the file line number.
    #[tokio::test]
    async fn mid_file_corruption_fails_with_line_number() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let good = raw_line(&user_msg("ok"));
        write_raw(&store, "s1", &format!("{good}\nNOT_JSON\n{good}\n"));

        let err = store.load_messages("s1").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("line 2"),
            "error must name the file line, got: {msg}"
        );
        assert!(
            msg.contains("messages.jsonl"),
            "error must name the file, got: {msg}"
        );
        // No repair for mid-file damage: the file is untouched.
        let raw = std::fs::read_to_string(store.messages_path("s1")).unwrap();
        assert_eq!(raw, format!("{good}\nNOT_JSON\n{good}\n"));
    }

    /// A trailing fragment that is *valid* JSON without a newline (crash
    /// between line and terminator under the old two-write scheme) parses
    /// fine — `load_from_file` shares the same policy.
    #[tokio::test]
    async fn load_from_file_shares_recovery_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let good = raw_line(&user_msg("ok"));
        write_raw(
            &store,
            "s1",
            &format!("{good}\n{}", &good[..good.len() / 2]),
        );

        let loaded = JsonlStorage::load_from_file(&store.messages_path("s1"))
            .await
            .unwrap();
        assert_eq!(loaded.len(), 1);
    }

    // ── EventStorage ──────────────────────────────────────────────

    use crate::types::session_event::{SessionEvent, TurnEndReason};

    fn sample_events() -> Vec<SessionEvent> {
        vec![
            SessionEvent::TurnStart,
            SessionEvent::User(user_msg("hello")),
            SessionEvent::TurnEnd {
                reason: TurnEndReason::Completed,
            },
        ]
    }

    fn write_raw_events(store: &EventStorage, session_id: &str, content: &str) {
        let path = store.events_path(session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn events_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let events = sample_events();

        store.append_events("s1", &events).await.unwrap();

        let loaded = store.load_events("s1").await.unwrap();
        assert_eq!(loaded, events);
        assert_eq!(
            store.events_path("s1"),
            tmp.path().join("s1").join("events.jsonl")
        );
    }

    #[tokio::test]
    async fn events_torn_tail_last_line_dropped_and_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());

        let good1 = serde_json::to_string(&SessionEvent::TurnStart).unwrap();
        let good2 = serde_json::to_string(&SessionEvent::User(user_msg("ok"))).unwrap();
        let torn = &good2[..good2.len() / 2]; // mid-JSON truncation
        write_raw_events(&store, "s1", &format!("{good1}\n{good2}\n{torn}"));

        let loaded = store.load_events("s1").await.unwrap();
        assert_eq!(loaded.len(), 2, "torn tail must be dropped, prefix kept");

        // File on disk is repaired: truncated at the torn line.
        let raw = std::fs::read_to_string(store.events_path("s1")).unwrap();
        assert_eq!(raw, format!("{good1}\n{good2}\n"));
    }

    #[tokio::test]
    async fn events_mid_file_corruption_errors_with_line_number() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let good = serde_json::to_string(&SessionEvent::TurnStart).unwrap();
        write_raw_events(&store, "s1", &format!("NOT_JSON\n{good}\n"));

        let err = store.load_events("s1").await.unwrap_err();
        assert!(matches!(err, AgentError::Transcript(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("line 1"),
            "error must name the file line, got: {msg}"
        );
        assert!(
            msg.contains("events.jsonl"),
            "error must name the file, got: {msg}"
        );
    }

    /// Fail closed on version skew: a syntactically complete row whose
    /// "type" tag matches no known variant — even as the LAST line — must
    /// fail the load and leave the file untouched, never be healed away as
    /// a torn tail. A newer conga wrote that row (it is by definition the
    /// most recent line); truncating it would silently destroy data.
    #[tokio::test]
    async fn events_unknown_variant_fails_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        write_raw_events(&store, "s1", "{\"type\":\"from_the_future\"}\n");

        let err = store.load_events("s1").await.unwrap_err();
        assert!(matches!(err, AgentError::Transcript(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("line 1"),
            "error must name the file line, got: {msg}"
        );
        // No destructive repair: the unknown row is still on disk.
        let raw = std::fs::read_to_string(store.events_path("s1")).unwrap();
        assert_eq!(raw, "{\"type\":\"from_the_future\"}\n");
    }

    /// The same unknown variant mid-file is real damage: fail loudly,
    /// file untouched.
    #[tokio::test]
    async fn events_unknown_variant_mid_file_fails_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let good = serde_json::to_string(&SessionEvent::TurnStart).unwrap();
        write_raw_events(
            &store,
            "s1",
            &format!("{{\"type\":\"from_the_future\"}}\n{good}\n"),
        );

        let err = store.load_events("s1").await.unwrap_err();
        assert!(matches!(err, AgentError::Transcript(_)));
        let raw = std::fs::read_to_string(store.events_path("s1")).unwrap();
        assert_eq!(raw, format!("{{\"type\":\"from_the_future\"}}\n{good}\n"));
    }

    // ── Atomic batch install ──────────────────────────────────────

    #[tokio::test]
    async fn append_events_atomic_empty_batch_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        store.append_events_atomic("s1", &[]).await.unwrap();
        assert!(!store.has_events("s1"));
        assert!(!store.events_path("s1").exists());
        assert!(!tmp.path().join("s1").exists());
    }

    #[tokio::test]
    async fn append_events_atomic_installs_full_batch_and_leaves_no_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let events = sample_events();

        store.append_events_atomic("s1", &events).await.unwrap();

        assert_eq!(store.load_events("s1").await.unwrap(), events);
        assert!(
            !tmp.path().join("s1").join("events.jsonl.tmp").exists(),
            "the staging file must be gone after the rename"
        );
    }

    /// Crash before the rename: only a (possibly torn) `.tmp` exists. It is
    /// invisible to `has_events`/`load_events`, and a retry replaces it
    /// wholesale instead of appending behind it.
    #[tokio::test]
    async fn leftover_tmp_from_crash_is_ignored_and_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let dir = tmp.path().join("s1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("events.jsonl.tmp"), "{\"type\":\"TurnSta").unwrap();

        assert!(!store.has_events("s1"));
        assert!(store.load_events("s1").await.unwrap().is_empty());

        // The retry after the crash.
        let events = sample_events();
        store.append_events_atomic("s1", &events).await.unwrap();
        assert_eq!(store.load_events("s1").await.unwrap(), events);
        assert!(!dir.join("events.jsonl.tmp").exists());
    }

    // ── Session meta sidecar & removal ────────────────────────

    #[tokio::test]
    async fn meta_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        assert!(store.load_meta("s1").await.is_none());

        store
            .write_meta(
                "s1",
                &SessionMeta {
                    name: Some("My chat".into()),
                },
            )
            .await
            .unwrap();
        let meta = store.load_meta("s1").await.unwrap();
        assert_eq!(meta.name.as_deref(), Some("My chat"));
        // No staging file left behind.
        assert!(!tmp.path().join("s1").join("meta.json.tmp").exists());
    }

    #[tokio::test]
    async fn write_meta_rejects_bad_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        assert!(store
            .write_meta("../evil", &SessionMeta::default())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn remove_session_deletes_dir_and_reports_existence() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        store
            .append_event("s1", &SessionEvent::TurnStart)
            .await
            .unwrap();
        store
            .write_meta(
                "s1",
                &SessionMeta {
                    name: Some("x".into()),
                },
            )
            .await
            .unwrap();

        assert!(store.remove_session("s1").await.unwrap());
        assert!(!tmp.path().join("s1").exists());
        // A second delete reports the absence, not an error.
        assert!(!store.remove_session("s1").await.unwrap());
        assert!(store.remove_session("../evil").await.is_err());
    }
}
