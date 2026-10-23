//! SessionManager: wraps [`EventStorage`], adding "current session/list/
//! latest" semantics plus the one-shot legacy `messages.jsonl` migration.
//!
//! The event log (`events.jsonl`) is the single source of truth. Legacy
//! `messages.jsonl` transcripts migrate once — wrapped row-by-row into the
//! event log, deleted only after the full write succeeded (D1: not kept) —
//! and any corruption fails closed with `Err` instead of being adopted.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use conga::{derive_messages, AgentError, AgentMessage, EventStorage, JsonlStorage, SessionEvent};

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub mtime: SystemTime,
    pub msg_count: usize,
    /// Display name from the session's `meta.json` sidecar, if any.
    pub name: Option<String>,
}

/// Cursor semantics: `new()` generates the initial `current_id`; only
/// `new` / `resume` / `clear` change it; events always append to the
/// current id. A fresh manager that was never resumed writes a brand-new
/// session — callers that want an existing session MUST `resume` first.
#[derive(Clone)]
pub struct SessionManager {
    root: PathBuf,
    storage: EventStorage,
    /// Current-session cursor. Interior-mutable on purpose: rotating to a
    /// fresh id (`clear`) is a cursor move, not a host mutation — transports
    /// that hold the `Host` behind an `Arc` (the desktop app) must be able
    /// to `/clear` without `&mut` access.
    current_id: Arc<parking_lot::Mutex<String>>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Count model-visible messages in one transcript's raw contents.
///
/// The event-log branch reuses the CORE projection rules
/// (`conga::visible_count_from_rows`): a `/clear` marker resets the count
/// to zero, a `Compacted` checkpoint sets it to the size of its frozen
/// base — exactly what the model sees (`conga::derive_messages`). The
/// row-level probe (`conga::row_view_from_json`) keeps the count cheap on
/// large logs (a session holds megabytes of tool output per row). A legacy
/// `messages.jsonl` (pre-migration) holds one message per non-empty line. A
/// torn/unparseable event row is not a message and is skipped.
fn count_messages(raw: &str, is_events: bool) -> usize {
    if !is_events {
        // A pre-migration messages.jsonl holds one message per non-empty line.
        return raw.lines().filter(|l| !l.trim().is_empty()).count();
    }
    conga::visible_count_from_rows(raw.lines().filter_map(conga::row_view_from_json))
}

/// `msg_count` cache keys stored in the session's `meta.json` sidecar.
/// Additive — core's `SessionMeta` reader ignores unknown fields, so the
/// display name written by rename flows keeps working. `msg_count` is the
/// model-visible message count; `msg_count_bytes` is the `events.jsonl`
/// size it was computed at (the freshness check: appends grow the file);
/// `msg_count_rule` names the counting rule that produced it.
const META_MSG_COUNT: &str = "msg_count";
const META_MSG_COUNT_BYTES: &str = "msg_count_bytes";
const META_MSG_COUNT_RULE: &str = "msg_count_rule";

/// The counting rule this build implements. Bump it whenever the rule
/// changes: a cache written by the previous rule would otherwise keep
/// serving its (now wrong) number until the log happens to grow.
const MSG_COUNT_RULE: &str = "v2-compacted-aware";

/// Read the cached `(msg_count, events_bytes)` pair from a `meta.json`.
/// `None` when the sidecar is missing, unreadable, predates the cache, or
/// was written by a different counting rule.
async fn read_msg_count_cache(meta_path: &std::path::Path) -> Option<(usize, u64)> {
    let raw = tokio::fs::read(meta_path).await.ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    if v.get(META_MSG_COUNT_RULE)?.as_str()? != MSG_COUNT_RULE {
        return None;
    }
    Some((
        v.get(META_MSG_COUNT)?.as_u64()? as usize,
        v.get(META_MSG_COUNT_BYTES)?.as_u64()?,
    ))
}

/// Merge the cache pair into `meta.json`, preserving every other field
/// (e.g. the display name). Atomic (tmp + rename), best effort — a failed
/// write never fails `list`.
async fn write_msg_count_cache(meta_path: &std::path::Path, count: usize, events_len: u64) {
    let write = async {
        let mut v: serde_json::Value = match tokio::fs::read(meta_path).await {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|_| serde_json::json!({})),
            Err(_) => serde_json::json!({}),
        };
        if let Some(map) = v.as_object_mut() {
            map.insert(META_MSG_COUNT.into(), count.into());
            map.insert(META_MSG_COUNT_BYTES.into(), events_len.into());
            map.insert(META_MSG_COUNT_RULE.into(), MSG_COUNT_RULE.into());
        }
        // Unique tmp name: concurrent `list` self-heals of the same session
        // must not write through one shared `meta.json.tmp`.
        let mut tmp_os = meta_path.as_os_str().to_os_string();
        tmp_os.push(format!(".{}.conga-tmp", uuid::Uuid::new_v4()));
        let tmp = std::path::PathBuf::from(tmp_os);
        tokio::fs::write(&tmp, v.to_string()).await?;
        tokio::fs::rename(&tmp, meta_path).await?;
        Ok::<(), std::io::Error>(())
    };
    let _ = write.await;
}

/// Wrap legacy messages into session events.
///
/// A legacy row the event vocabulary cannot express — a plugin-private
/// `Custom` message — is SKIPPED with a warning instead of failing the
/// whole transcript: such a message is not model-visible anyway (the LLM
/// boundary filters it), and refusing to read the session because of it
/// would make an otherwise healthy conversation unreadable.
fn legacy_to_events(legacy: &[AgentMessage], session_id: &str) -> Vec<SessionEvent> {
    legacy
        .iter()
        .filter_map(|msg| {
            let ev = SessionEvent::from_message(msg, None);
            if ev.is_none() {
                tracing::warn!(
                    session = %session_id,
                    "legacy row cannot be represented as a session event; skipped"
                );
            }
            ev
        })
        .collect()
}

impl SessionManager {
    pub fn new() -> Self {
        Self::with_root(JsonlStorage::default_root().base_dir_clone())
    }

    /// 测试用：指定 root。
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root: root.clone(),
            storage: EventStorage::new(root),
            current_id: Arc::new(parking_lot::Mutex::new(uuid::Uuid::new_v4().to_string())),
        }
    }

    pub fn current_id(&self) -> String {
        self.current_id.lock().clone()
    }

    /// Open a session READ-ONLY: `events.jsonl` exists → load; otherwise a
    /// legacy `messages.jsonl` is converted IN MEMORY (nothing is written,
    /// nothing is deleted).
    ///
    /// This is what read endpoints use. Opening a session must never
    /// MIGRATE the user's disk — a GET that migrates (and deletes the
    /// legacy file) is a write disguised as a read.
    ///
    /// The one disk write a read can still do is the documented torn-tail
    /// self-heal inside the event-log reader: an incomplete LAST line (a
    /// crash artifact) is dropped and the file is truncated at its start.
    /// A mid-file corruption is never healed — it fails closed.
    ///
    /// Middle-row corruption → `Err` (fail closed, never adopt).
    ///
    /// Does not change the current-session cursor; [`resume`](Self::resume)
    /// adopts the id on top of this.
    pub async fn open(&self, session_id: &str) -> Result<Vec<SessionEvent>, AgentError> {
        if self.storage.has_events(session_id) {
            return self.storage.load_events(session_id).await;
        }
        let legacy = self.storage.load_messages(session_id).await?;
        Ok(legacy_to_events(&legacy, session_id))
    }

    /// Open a session, migrating a legacy transcript ONCE if needed.
    ///
    /// The legacy rows are installed atomically (tmp + rename) into
    /// `events.jsonl`, and the legacy file is deleted only after the
    /// install succeeded. This is the WRITE path — use it where the
    /// session is about to be written (a turn), not where it is only read.
    ///
    /// Crash windows:
    ///  * before the rename — only the `.tmp` exists, `has_events` stays
    ///    false, and the next migrate re-migrates from the untouched legacy
    ///    (idempotent; the stale `.tmp` is replaced wholesale).
    ///  * after the rename, before `delete_legacy` — events.jsonl is
    ///    complete and `has_events` short-circuits to it, so the leftover
    ///    legacy file is inert and is deliberately left in place: D1's
    ///    "delete after success" is satisfied at the rename boundary, and
    ///    a second opportunistic cleanup pass is not worth the code.
    ///
    /// Does not change the current-session cursor; [`resume`](Self::resume)
    /// adopts the id on top of this.
    pub async fn migrate(&self, session_id: &str) -> Result<Vec<SessionEvent>, AgentError> {
        if self.storage.has_events(session_id) {
            return self.storage.load_events(session_id).await;
        }
        let legacy = self.storage.load_messages(session_id).await?;
        if legacy.is_empty() {
            // Neither an event log nor legacy content: fresh session.
            return Ok(Vec::new());
        }
        let events = legacy_to_events(&legacy, session_id);
        if events.is_empty() {
            // Nothing representable: leave the legacy file alone rather
            // than installing an empty log and deleting the original.
            return Ok(Vec::new());
        }
        // Atomic install (tmp + rename): a crash can never leave a torn
        // events.jsonl shadowing the intact legacy file.
        self.storage
            .append_events_atomic(session_id, &events)
            .await?;
        self.storage.delete_legacy(session_id).await?;
        Ok(events)
    }

    /// Append one event to the current session's log.
    pub async fn append_event(&self, ev: &SessionEvent) -> Result<(), AgentError> {
        let sid = self.current_id.lock().clone();
        self.storage.append_event(&sid, ev).await
    }

    /// The agent loop's sync `persist` callback, backed by the store's
    /// synchronous `std::fs` append — no runtime bridging, no
    /// thread-spawn-per-event, and safe to call from any thread (the
    /// `Handle::block_on` nested-runtime panic risk is gone by
    /// construction). Lines are small and events per turn are few, so the
    /// brief blocking write is noise next to an LLM round-trip.
    #[allow(clippy::type_complexity)]
    pub fn persist_fn(&self) -> Arc<dyn Fn(&SessionEvent) -> Result<(), AgentError> + Send + Sync> {
        let storage = self.storage.clone();
        let sid = self.current_id.lock().clone();
        Arc::new(move |ev| storage.append_event_sync(&sid, ev))
    }

    /// Load (migrating if needed) a session and adopt it as the current one.
    /// Returns the derived model-visible history. Corruption fails closed.
    /// Load (migrating if needed) a session and adopt it as the current one.
    /// Returns the derived model-visible history. Corruption fails closed.
    /// `&self`: the cursor is interior-mutable, so hosts can resume through
    /// a shared `&SessionManager` (no `&mut` gymnastics).
    pub async fn resume(&self, id: &str) -> Result<Vec<AgentMessage>, crate::HostError> {
        let events = self
            .open(id)
            .await
            .map_err(|e| crate::HostError::Session(e.to_string()))?;
        *self.current_id.lock() = id.to_string();
        Ok(derive_messages(&events))
    }

    pub async fn resume_last(&self) -> Result<Vec<AgentMessage>, crate::HostError> {
        let id = self
            .list()
            .await?
            .into_iter()
            .max_by_key(|s| s.mtime)
            .map(|s| s.id)
            .ok_or_else(|| crate::HostError::Session("no prior session".into()))?;
        self.resume(&id).await
    }

    pub async fn list(&self) -> Result<Vec<SessionInfo>, crate::HostError> {
        let root = self.root.clone();
        let mut out = Vec::new();
        // Fresh install: sessions dir doesn't exist yet -> no sessions.
        let mut rd = match tokio::fs::read_dir(&root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(crate::HostError::Session(e.to_string())),
        };
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| crate::HostError::Session(e.to_string()))?
        {
            if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            // mtime comes from the transcript file, NOT the session dir:
            // appending updates the file's mtime but leaves the dir's
            // untouched, so dir mtime would freeze at first write. Prefer
            // events.jsonl; an unmigrated legacy session falls back to
            // messages.jsonl.
            let is_events = self.storage.has_events(&id);
            let path = if is_events {
                self.storage.events_path(&id)
            } else {
                self.storage.messages_path(&id)
            };
            let (mtime, events_len) = match tokio::fs::metadata(&path).await {
                Ok(m) => (m.modified().ok().unwrap_or(SystemTime::UNIX_EPOCH), m.len()),
                Err(_) => (SystemTime::UNIX_EPOCH, 0),
            };
            // Count model-visible messages, not raw event lines: the event
            // log carries TurnStart/TurnEnd marker rows that
            // derive_messages projects away. A legacy messages.jsonl
            // (pre-migration) still holds one message per non-empty line.
            // Event sessions consult the msg_count cache in the meta.json
            // sidecar first; only a missing/stale cache pays the full read.
            let msg_count = if is_events {
                self.cached_or_count_messages(&id, &path, events_len).await
            } else {
                match tokio::fs::read_to_string(&path).await {
                    Ok(s) => count_messages(&s, false),
                    Err(_) => 0,
                }
            };
            out.push(SessionInfo {
                id: id.clone(),
                mtime,
                msg_count,
                name: self.storage.load_meta(&id).await.and_then(|m| m.name),
            });
        }
        Ok(out)
    }

    /// `list` fast path: reuse the `msg_count` cached in the session's
    /// `meta.json` sidecar while `events.jsonl`'s size still matches the
    /// size recorded with it (`msg_count_bytes`); a missing or stale cache
    /// (events were appended since) falls back to one full read for THIS
    /// session only and refreshes the cache. Best effort: a failed
    /// cache write just costs the next `list` a full read.
    async fn cached_or_count_messages(
        &self,
        id: &str,
        events_path: &std::path::Path,
        events_len: u64,
    ) -> usize {
        if let Some((count, cached_len)) = read_msg_count_cache(&self.storage.meta_path(id)).await {
            if cached_len == events_len {
                return count;
            }
        }
        let count = match tokio::fs::read_to_string(events_path).await {
            Ok(s) => count_messages(&s, true),
            Err(_) => 0,
        };
        write_msg_count_cache(&self.storage.meta_path(id), count, events_len).await;
        count
    }

    /// Mark the conversation cleared — the unified `/clear` semantics for
    /// every transport: append a [`SessionEvent::Cleared`] fact to the
    /// CURRENT session's log. The session id does NOT rotate (live
    /// connections, REST readers, and the FTS index keep addressing the same
    /// chat); [`derive_messages`](conga::derive_messages) projects away the
    /// pre-clear prefix; the log on disk stays append-only. Fail loud: a
    /// failed write returns `Err` so the caller can tell the user the clear
    /// did NOT take (a silent failure would resurrect the old history on
    /// the next turn).
    pub async fn mark_cleared(&self) -> Result<(), AgentError> {
        self.append_event(&SessionEvent::Cleared).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::{ContentBlock, UserMessage};

    fn user_msg(t: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(t)],
            timestamp: 1,
        })
    }

    fn user_event(t: &str) -> SessionEvent {
        SessionEvent::User(user_msg(t))
    }

    fn assistant_event(t: &str) -> SessionEvent {
        SessionEvent::Assistant {
            message: AgentMessage::assistant_text(t),
            usage: None,
        }
    }

    #[tokio::test]
    async fn resume_loads_and_sets_current() {
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = "fixed-id".to_string();
        *sm.current_id.lock() = id.clone();
        sm.append_event(&user_event("a")).await.unwrap();
        sm.append_event(&user_event("b")).await.unwrap();

        let sm2 = SessionManager::with_root(tmp.path().to_path_buf());
        let msgs = sm2.resume(&id).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(sm2.current_id(), id);
    }

    #[tokio::test]
    async fn resume_last_picks_most_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let a = SessionManager::with_root(tmp.path().to_path_buf());
        *a.current_id.lock() = "old".into();
        a.append_event(&user_event("old")).await.unwrap();
        // 让 new 的 mtime 晚于 old
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = SessionManager::with_root(tmp.path().to_path_buf());
        *b.current_id.lock() = "new".into();
        b.append_event(&user_event("new")).await.unwrap();

        let pick = SessionManager::with_root(tmp.path().to_path_buf());
        let msgs = pick.resume_last().await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(pick.current_id(), "new");
    }

    #[tokio::test]
    async fn resume_last_uses_latest_message_mtime() {
        // Regression: dir mtime freezes at first write, so a session that gets
        // a *second* append after another session was created must still win.
        let tmp = tempfile::tempdir().unwrap();
        let a = SessionManager::with_root(tmp.path().to_path_buf());
        *a.current_id.lock() = "a".into();
        a.append_event(&user_event("a1")).await.unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = SessionManager::with_root(tmp.path().to_path_buf());
        *b.current_id.lock() = "b".into();
        b.append_event(&user_event("b1")).await.unwrap();

        // a receives a later event -> a is the most recently active session.
        std::thread::sleep(std::time::Duration::from_millis(20));
        a.append_event(&user_event("a2")).await.unwrap();

        let pick = SessionManager::with_root(tmp.path().to_path_buf());
        let msgs = pick.resume_last().await.unwrap();
        assert_eq!(pick.current_id(), "a");
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn list_on_missing_root_is_empty() {
        // Fresh install: sessions dir doesn't exist yet.
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().join("nope"));
        assert!(sm.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_counts_messages_not_turn_markers() {
        // A minimal turn writes 4 event lines (TurnStart, User, Assistant,
        // TurnEnd) but only 2 are model-visible messages. `msg_count` must
        // report messages, not raw event lines (TurnStart/TurnEnd contribute
        // nothing to derive_messages).
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = sm.current_id().to_string();
        sm.append_event(&SessionEvent::TurnStart).await.unwrap();
        sm.append_event(&user_event("hi")).await.unwrap();
        sm.append_event(&assistant_event("hello")).await.unwrap();
        sm.append_event(&SessionEvent::TurnEnd {
            reason: conga::TurnEndReason::Completed,
        })
        .await
        .unwrap();

        let info = sm
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .expect("session should be listed");
        assert_eq!(
            info.msg_count, 2,
            "msg_count must count messages (User/Assistant/ToolResult), not event lines"
        );
    }

    #[tokio::test]
    async fn clear_marks_the_log_and_survives_restart() {
        // /clear is a FACT in the log, not a rotation: the id stays, derive
        // truncates, a fresh process sees the same cleared view, and the
        // pre-clear rows are still on disk (append-only).
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = sm.current_id().to_string();
        sm.append_event(&user_event("before clear")).await.unwrap();
        sm.mark_cleared().await.unwrap();
        sm.append_event(&user_event("after clear")).await.unwrap();

        // Same id (no ghost sessions).
        assert_eq!(sm.current_id(), id);

        // A fresh process resume derives only the post-clear prefix.
        let sm2 = SessionManager::with_root(tmp.path().to_path_buf());
        let msgs = sm2.resume(&id).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0], AgentMessage::User(u)
            if matches!(&u.content[0], conga::ContentBlock::Text { text } if text == "after clear")));

        // The log on disk kept the pre-clear rows (append-only intact).
        let events = sm2.open(&id).await.unwrap();
        assert!(events.contains(&SessionEvent::Cleared));
        assert!(events.iter().any(|ev| matches!(ev,
            SessionEvent::User(m) if matches!(m, AgentMessage::User(u)
                if matches!(&u.content[0], conga::ContentBlock::Text { text } if text == "before clear")))));

        // list() mirrors derive: 1 model-visible message after the clear.
        let info = sm2
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .unwrap();
        assert_eq!(info.msg_count, 1);
    }

    #[tokio::test]
    async fn persist_fn_writes_events_outside_async_ctx() {
        // The loop's sync persist callback must land events in the log —
        // including when called from a plain (non-async) caller.
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        *sm.current_id.lock() = "persisted".into();
        let persist = sm.persist_fn();
        persist(&user_event("via-callback")).unwrap();

        let events = sm.open("persisted").await.unwrap();
        assert_eq!(events, vec![user_event("via-callback")]);
    }

    fn write_legacy(dir: &std::path::Path, msgs: &[AgentMessage]) {
        let mut raw = String::new();
        for m in msgs {
            raw.push_str(&serde_json::to_string(m).unwrap());
            raw.push('\n');
        }
        std::fs::write(dir.join("messages.jsonl"), raw).unwrap();
    }

    /// A read endpoint must not write: opening a legacy session converts
    /// it IN MEMORY and leaves the disk exactly as it was.
    #[tokio::test]
    async fn open_does_not_touch_the_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "legacy-readonly";
        let dir = tmp.path().join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = vec![user_msg("hello"), user_msg("again")];
        write_legacy(&dir, &legacy);

        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let events = sm.open(sid).await.unwrap();
        assert_eq!(derive_messages(&events), legacy);
        assert!(
            dir.join("messages.jsonl").exists(),
            "a read must not delete the legacy file"
        );
        assert!(
            !dir.join("events.jsonl").exists(),
            "a read must not install a migrated log"
        );
    }

    /// The write path installs the legacy transcript and removes it.
    #[tokio::test]
    async fn migrate_installs_the_legacy_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "legacy-migrate";
        let dir = tmp.path().join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = vec![user_msg("hello"), user_msg("again")];
        write_legacy(&dir, &legacy);

        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let events = sm.migrate(sid).await.unwrap();
        assert_eq!(derive_messages(&events), legacy);
        assert!(dir.join("events.jsonl").exists());
        assert!(!dir.join("messages.jsonl").exists());
    }

    /// A legacy row the event vocabulary cannot express (a plugin-private
    /// `Custom` message) must not make the whole session unreadable: it is
    /// skipped, the rest of the transcript is served.
    #[tokio::test]
    async fn open_skips_a_legacy_row_the_event_vocabulary_cannot_express() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "legacy-custom";
        let dir = tmp.path().join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        let custom = AgentMessage::Custom(conga::types::message::CustomMessage {
            custom_type: "todo.list".into(),
            content: serde_json::json!({"items": []}),
            timestamp: 1,
        });
        write_legacy(&dir, &[user_msg("before"), custom, user_msg("after")]);

        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let events = sm.open(sid).await.unwrap();
        let projected = derive_messages(&events);
        assert_eq!(projected.len(), 2, "the two user rows survive");
        assert_eq!(text_of_first(&projected[0]), "before");
        assert_eq!(text_of_first(&projected[1]), "after");
    }

    /// The same tolerant handling on the write path: the representable
    /// rows are installed, the legacy file goes away.
    #[tokio::test]
    async fn migrate_skips_a_legacy_row_the_event_vocabulary_cannot_express() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "legacy-custom-migrate";
        let dir = tmp.path().join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        let custom = AgentMessage::Custom(conga::types::message::CustomMessage {
            custom_type: "todo.list".into(),
            content: serde_json::json!({"items": []}),
            timestamp: 1,
        });
        write_legacy(&dir, &[user_msg("before"), custom, user_msg("after")]);

        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let events = sm.migrate(sid).await.unwrap();
        assert_eq!(derive_messages(&events).len(), 2);
        assert!(dir.join("events.jsonl").exists());
        assert!(!dir.join("messages.jsonl").exists());
    }

    fn text_of_first(msg: &AgentMessage) -> String {
        match msg {
            AgentMessage::User(u) => u
                .content
                .iter()
                .filter_map(|b| match b {
                    conga::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    /// Crash window before the rename: `events.jsonl.tmp` exists (possibly
    /// torn) but `events.jsonl` never materialized. `has_events` must be
    /// false, the legacy file must still be intact, and the next open must
    /// re-migrate the full transcript from scratch.
    #[tokio::test]
    async fn migration_crash_before_rename_re_migrates_from_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "crashed-migration";
        let dir = tmp.path().join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = vec![user_msg("hello"), user_msg("again")];
        write_legacy(&dir, &legacy);
        // The torn staging file a kill -9 between write and rename leaves.
        std::fs::write(dir.join("events.jsonl.tmp"), "{\"type\":\"TurnSta").unwrap();

        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let events = sm.migrate(sid).await.unwrap();
        assert_eq!(
            derive_messages(&events),
            legacy,
            "the retry must migrate the full legacy transcript"
        );
        assert!(dir.join("events.jsonl").exists());
        assert!(
            !dir.join("events.jsonl.tmp").exists(),
            "the retry must replace the stale staging file wholesale"
        );
        assert!(
            !dir.join("messages.jsonl").exists(),
            "legacy removed only after the successful atomic install"
        );
    }

    /// Crash window after the rename but before `delete_legacy`:
    /// `events.jsonl` is complete, so the open short-circuits to it and the
    /// leftover legacy file is inert and left in place (documented choice —
    /// the rename boundary is the success point for D1).
    #[tokio::test]
    async fn migration_crash_after_rename_opens_events_and_leaves_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "crashed-cleanup";
        let dir = tmp.path().join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        // The already-complete migrated log plus an orphaned legacy file.
        let events = vec![user_event("migrated-a"), user_event("migrated-b")];
        let mut raw = String::new();
        for ev in &events {
            raw.push_str(&serde_json::to_string(ev).unwrap());
            raw.push('\n');
        }
        std::fs::write(dir.join("events.jsonl"), raw).unwrap();
        write_legacy(&dir, &[user_msg("old-a"), user_msg("old-b")]);

        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let loaded = sm.migrate(sid).await.unwrap();
        assert_eq!(
            loaded, events,
            "must load the complete events.jsonl, never consult the legacy file"
        );
        assert!(
            dir.join("messages.jsonl").exists(),
            "the leftover legacy is inert cleanup, deliberately not deleted here"
        );
    }

    /// `list` caches msg_count in the meta.json sidecar; a later append
    /// changes events.jsonl's size, the stale cache is detected, and the
    /// full read self-heals the count for that session.
    #[tokio::test]
    async fn list_after_append_recaches_and_self_heals() {
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = "cache-count".to_string();
        *sm.current_id.lock() = id.clone();
        sm.append_event(&user_event("a")).await.unwrap();
        sm.append_event(&assistant_event("b")).await.unwrap();

        let find =
            |info: Vec<SessionInfo>| info.into_iter().find(|i| i.id == "cache-count").unwrap();
        assert_eq!(find(sm.list().await.unwrap()).msg_count, 2);

        // The first list wrote the cache into the sidecar.
        let meta_path = tmp.path().join(&id).join("meta.json");
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        assert_eq!(meta[META_MSG_COUNT], 2, "cache must land in meta.json");
        let bytes = std::fs::metadata(tmp.path().join(&id).join("events.jsonl"))
            .unwrap()
            .len();
        assert_eq!(meta[META_MSG_COUNT_BYTES].as_u64(), Some(bytes));

        // Append: the recorded size no longer matches the file — list must
        // fall back to the full read and report the NEW count.
        sm.append_event(&user_event("c")).await.unwrap();
        assert_eq!(find(sm.list().await.unwrap()).msg_count, 3);

        // And the refreshed cache is written back (new size recorded).
        let meta2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        let bytes2 = std::fs::metadata(tmp.path().join(&id).join("events.jsonl"))
            .unwrap()
            .len();
        assert_eq!(meta2[META_MSG_COUNT], 3);
        assert_eq!(meta2[META_MSG_COUNT_BYTES].as_u64(), Some(bytes2));
    }

    /// While events.jsonl is unchanged, list serves the cached count (no
    /// full read): a tampered-but-size-matched cache value is what comes
    /// back. The next append self-heals.
    #[tokio::test]
    async fn list_serves_cached_count_while_events_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = "cache-hit".to_string();
        *sm.current_id.lock() = id.clone();
        sm.append_event(&user_event("a")).await.unwrap();
        sm.append_event(&user_event("b")).await.unwrap();
        let find = |info: Vec<SessionInfo>| info.into_iter().find(|i| i.id == "cache-hit").unwrap();
        assert_eq!(find(sm.list().await.unwrap()).msg_count, 2); // seeds cache

        // Tamper ONLY the count; keep the recorded size — the cache is
        // fresh by the bytes check, so this value must be served as-is.
        let meta_path = tmp.path().join(&id).join("meta.json");
        let mut meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta[META_MSG_COUNT] = 99.into();
        std::fs::write(&meta_path, meta.to_string()).unwrap();
        assert_eq!(
            find(sm.list().await.unwrap()).msg_count,
            99,
            "fresh cache must be served without a full read"
        );

        // Appending invalidates the tampered cache; the count self-heals.
        sm.append_event(&user_event("c")).await.unwrap();
        assert_eq!(find(sm.list().await.unwrap()).msg_count, 3);
    }

    // ---- Default ----

    #[test]
    fn default_manager_starts_on_its_own_uuid_session() {
        let sm = SessionManager::default();
        let id = sm.current_id();
        assert!(
            uuid::Uuid::parse_str(&id).is_ok(),
            "a fresh manager must own a uuid session id, got: {id}"
        );
        let other = SessionManager::default();
        assert_ne!(
            id,
            other.current_id(),
            "two default managers must not share a session id"
        );
    }

    // ---- count_messages ----

    #[test]
    fn count_messages_of_an_empty_log_is_zero() {
        assert_eq!(count_messages("", false), 0);
        assert_eq!(count_messages("", true), 0);
    }

    #[test]
    fn count_messages_counts_every_non_empty_line_in_legacy_mode() {
        // A pre-migration messages.jsonl holds one message per non-empty line.
        let raw = "{\"a\":1}\n\n{\"b\":2}\n   \n{\"c\":3}\n";
        assert_eq!(count_messages(raw, false), 3);
    }

    #[test]
    fn count_messages_counts_only_model_visible_event_rows() {
        // TurnStart/TurnEnd are markers: they are not messages.
        let raw = concat!(
            "{\"type\":\"turn_start\"}\n",
            "{\"type\":\"user\"}\n",
            "{\"type\":\"assistant\"}\n",
            "{\"type\":\"tool_result\"}\n",
            "{\"type\":\"turn_end\"}\n",
        );
        assert_eq!(count_messages(raw, true), 3);
    }

    #[test]
    fn count_messages_counts_nothing_before_the_last_cleared_marker() {
        // The pre-clear prefix is history on disk, not conversation.
        let raw = concat!(
            "{\"type\":\"user\"}\n",
            "{\"type\":\"assistant\"}\n",
            "{\"type\":\"cleared\"}\n",
            "{\"type\":\"user\"}\n",
        );
        assert_eq!(count_messages(raw, true), 1);
    }

    #[test]
    fn count_messages_counts_a_compacted_checkpoint_as_its_base() {
        // A compaction checkpoint REPLACES the pre-checkpoint history with
        // its frozen base: the count must follow the checkpoint, not add
        // the folded-away messages on top of it.
        let events = vec![
            SessionEvent::User(user_msg("old1")),
            SessionEvent::User(user_msg("old2")),
            SessionEvent::Compacted {
                base: vec![user_msg("pinned task")],
                dropped: 1,
            },
            SessionEvent::User(user_msg("new")),
        ];
        let raw: String = events
            .iter()
            .map(|e| format!("{}\n", serde_json::to_string(e).unwrap()))
            .collect();
        assert_eq!(count_messages(&raw, true), 2, "base + one appended message");
        assert_eq!(
            count_messages(&raw, true),
            conga::derive_messages(&events).len(),
            "the counter and the model-visible view must agree"
        );
    }

    #[test]
    fn count_messages_follows_the_last_checkpoint() {
        // Two checkpoints in one log: the LAST one wins, exactly like the
        // model-visible projection.
        let events = vec![
            SessionEvent::User(user_msg("a")),
            SessionEvent::Compacted {
                base: vec![user_msg("pinned")],
                dropped: 0,
            },
            SessionEvent::User(user_msg("b")),
            SessionEvent::Compacted {
                base: vec![user_msg("pinned2"), user_msg("notice")],
                dropped: 2,
            },
            SessionEvent::User(user_msg("c")),
        ];
        let raw: String = events
            .iter()
            .map(|e| format!("{}\n", serde_json::to_string(e).unwrap()))
            .collect();
        assert_eq!(count_messages(&raw, true), 3);
        assert_eq!(
            count_messages(&raw, true),
            conga::derive_messages(&events).len()
        );
    }

    #[test]
    fn count_messages_skips_torn_unknown_and_typeless_rows() {
        let raw = concat!(
            "{\"type\":\"user\"}\n",
            "{\"type\":\"user\"\n", // torn tail
            "not json at all\n",
            "{\"type\":\"future_variant\"}\n", // unknown discriminant
            "{\"no_type\":true}\n",            // no discriminant at all
            "{\"type\":\"assistant\"}\n",
        );
        assert_eq!(count_messages(raw, true), 2);
    }

    // ---- read_msg_count_cache ----

    #[tokio::test]
    async fn read_msg_count_cache_returns_none_for_a_missing_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_msg_count_cache(&tmp.path().join("meta.json"))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn read_msg_count_cache_returns_none_for_unparsable_json() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("meta.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(read_msg_count_cache(&p).await.is_none());
    }

    #[tokio::test]
    async fn read_msg_count_cache_returns_none_when_a_field_is_missing_or_mistyped() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("meta.json");
        // msg_count_bytes missing
        std::fs::write(&p, r#"{"msg_count": 3}"#).unwrap();
        assert!(read_msg_count_cache(&p).await.is_none());
        // msg_count is not a number
        std::fs::write(&p, r#"{"msg_count": "three", "msg_count_bytes": 10}"#).unwrap();
        assert!(read_msg_count_cache(&p).await.is_none());
        // msg_count_bytes is not a number
        std::fs::write(&p, r#"{"msg_count": 3, "msg_count_bytes": "ten"}"#).unwrap();
        assert!(read_msg_count_cache(&p).await.is_none());
    }

    #[tokio::test]
    async fn read_msg_count_cache_returns_the_pair_for_a_valid_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("meta.json");
        std::fs::write(
            &p,
            format!(
                r#"{{"name":"x","msg_count":7,"msg_count_bytes":123,"msg_count_rule":"{MSG_COUNT_RULE}"}}"#
            ),
        )
        .unwrap();
        assert_eq!(read_msg_count_cache(&p).await, Some((7, 123)));
    }

    #[tokio::test]
    async fn read_msg_count_cache_rejects_a_cache_written_by_an_older_rule() {
        // A cache written before the counting rule changed (it ignored
        // compaction checkpoints) would keep serving its wrong number.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("meta.json");
        std::fs::write(&p, r#"{"msg_count":7,"msg_count_bytes":123}"#).unwrap();
        assert!(read_msg_count_cache(&p).await.is_none());
    }

    // ---- write_msg_count_cache ----

    #[tokio::test]
    async fn write_msg_count_cache_creates_the_sidecar_with_the_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("meta.json");
        write_msg_count_cache(&p, 4, 55).await;
        let raw = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v[META_MSG_COUNT], 4);
        assert_eq!(v[META_MSG_COUNT_BYTES], 55);
        assert_eq!(v[META_MSG_COUNT_RULE], MSG_COUNT_RULE);
    }

    #[tokio::test]
    async fn write_msg_count_cache_preserves_unrelated_fields() {
        // The display name written by the rename flow must survive the merge.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("meta.json");
        std::fs::write(&p, r#"{"name":"Release notes"}"#).unwrap();
        write_msg_count_cache(&p, 2, 20).await;
        let raw = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["name"], "Release notes");
        assert_eq!(v[META_MSG_COUNT], 2);
        assert_eq!(v[META_MSG_COUNT_BYTES], 20);
    }

    #[tokio::test]
    async fn write_msg_count_cache_overwrites_a_previous_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("meta.json");
        write_msg_count_cache(&p, 1, 10).await;
        write_msg_count_cache(&p, 9, 90).await;
        let raw = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v[META_MSG_COUNT], 9);
        assert_eq!(v[META_MSG_COUNT_BYTES], 90);
    }

    #[tokio::test]
    async fn write_msg_count_cache_replaces_a_corrupt_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("meta.json");
        std::fs::write(&p, "not json at all").unwrap();
        write_msg_count_cache(&p, 1, 2).await;
        let raw = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v[META_MSG_COUNT], 1);
        assert_eq!(v[META_MSG_COUNT_BYTES], 2);
    }

    #[tokio::test]
    async fn write_msg_count_cache_is_best_effort_on_an_unwritable_path() {
        // Missing parent directory: the write fails silently (best effort)
        // and nothing is created.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("missing-dir").join("meta.json");
        write_msg_count_cache(&p, 1, 2).await;
        assert!(
            !p.exists(),
            "a failed cache write must not create the sidecar"
        );
    }

    // ---- cached_or_count_messages ----

    #[tokio::test]
    async fn cached_or_count_messages_serves_a_fresh_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = "cached-hit";
        *sm.current_id.lock() = id.to_string();
        sm.append_event(&user_event("a")).await.unwrap();
        let path = sm.storage.events_path(id);
        let len = std::fs::metadata(&path).unwrap().len();
        // Seed a cache with a deliberately wrong count but the right size:
        // a fresh cache is served without a full read.
        write_msg_count_cache(&sm.storage.meta_path(id), 99, len).await;
        assert_eq!(sm.cached_or_count_messages(id, &path, len).await, 99);
    }

    #[tokio::test]
    async fn cached_or_count_messages_recounts_and_refreshes_a_stale_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = "cached-stale";
        *sm.current_id.lock() = id.to_string();
        sm.append_event(&user_event("a")).await.unwrap();
        sm.append_event(&assistant_event("b")).await.unwrap();
        let path = sm.storage.events_path(id);
        let len = std::fs::metadata(&path).unwrap().len();
        // A cache recorded for a different (older) file size is stale.
        write_msg_count_cache(&sm.storage.meta_path(id), 99, len - 1).await;
        assert_eq!(
            sm.cached_or_count_messages(id, &path, len).await,
            2,
            "a stale cache must fall back to a full read"
        );
        // The refreshed cache now records the real count and the real size.
        assert_eq!(
            read_msg_count_cache(&sm.storage.meta_path(id)).await,
            Some((2, len))
        );
    }

    #[tokio::test]
    async fn cached_or_count_messages_counts_and_caches_when_no_cache_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = "cached-missing";
        *sm.current_id.lock() = id.to_string();
        sm.append_event(&user_event("a")).await.unwrap();
        let path = sm.storage.events_path(id);
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(sm.cached_or_count_messages(id, &path, len).await, 1);
        assert_eq!(
            read_msg_count_cache(&sm.storage.meta_path(id)).await,
            Some((1, len)),
            "the counted value must be cached for the next list"
        );
    }

    #[tokio::test]
    async fn cached_or_count_messages_returns_zero_for_an_unreadable_log() {
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = "cached-unreadable";
        let path = sm.storage.events_path(id);
        // No events.jsonl on disk: the count is 0 (and the cache write is a
        // best-effort no-op because the session dir does not exist).
        assert_eq!(sm.cached_or_count_messages(id, &path, 0).await, 0);
    }
}
