//! Shared session-store API: list / messages / rename / delete / search.
//!
//! The gateway's axum handlers and the desktop app's Tauri commands are
//! thin transports over these functions — one validation rule, one DTO
//! shape, one fail-loud policy. Transport-specific concerns (HTTP status
//! codes, active-connection checks, IPC errors) stay in the transports.

use std::path::Path;

use conga::{AgentMessage, EventStorage, SessionMeta};

use crate::session::SessionManager;

/// One listed session (id, message count, display name, mtime in ms).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionListItem {
    pub id: String,
    pub msg_count: usize,
    pub name: Option<String>,
    /// Milliseconds since UNIX epoch; 0 when the file has no mtime.
    pub mtime_ms: u64,
}

/// A session-API failure, mapped by transports: `BadRequest` → HTTP 400,
/// `NotFound` → 404, `Internal` → 500 (fail loud, never silently adopted).
#[derive(Debug)]
pub enum SessionApiError {
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

impl std::fmt::Display for SessionApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionApiError::BadRequest(m) => write!(f, "{m}"),
            SessionApiError::NotFound(m) => write!(f, "{m}"),
            SessionApiError::Internal(m) => write!(f, "{m}"),
        }
    }
}

/// List all sessions on disk, newest first. Does NOT depend on live
/// connections — reads the JSONL store directly, so it also serves sessions
/// created by the CLI or other devices.
pub async fn list_sessions(store_root: &Path) -> Result<Vec<SessionListItem>, SessionApiError> {
    let mgr = SessionManager::with_root(store_root.to_path_buf());
    let mut sessions = mgr
        .list()
        .await
        .map_err(|e| SessionApiError::Internal(e.to_string()))?;
    // Newest first.
    sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    Ok(sessions
        .into_iter()
        .map(|s| SessionListItem {
            id: s.id,
            msg_count: s.msg_count,
            name: s.name,
            mtime_ms: s
                .mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        })
        .collect())
}

/// Backend-truth transcript for a session: `derive_messages` over the
/// on-disk event log, migrating a legacy `messages.jsonl` once. `Ok(None)`
/// means the session has no on-disk data (a local-only chat). A corrupt log
/// is `Internal` (fail loud, never silently adopt).
pub async fn session_messages(
    store_root: &Path,
    session_id: &str,
) -> Result<Option<Vec<AgentMessage>>, SessionApiError> {
    let storage = EventStorage::new(store_root.to_path_buf());
    if !storage.has_events(session_id) && !storage.messages_path(session_id).exists() {
        return Ok(None);
    }
    let mgr = SessionManager::with_root(store_root.to_path_buf());
    let events = mgr
        .open_or_migrate(session_id)
        .await
        .map_err(|e| SessionApiError::Internal(e.to_string()))?;
    Ok(Some(conga::derive_messages(&events)))
}

/// Session-wide prompt-cache accounting, folded from the same on-disk
/// event log (usage rows on Assistant events). `Ok(None)` = no on-disk
/// data (same contract as [`session_messages`]); read-only observability —
/// the hit rate is a fact about the log, recomputed on demand.
pub async fn session_cache_stats(
    store_root: &Path,
    session_id: &str,
) -> Result<Option<conga::CacheStats>, SessionApiError> {
    let storage = EventStorage::new(store_root.to_path_buf());
    if !storage.has_events(session_id) && !storage.messages_path(session_id).exists() {
        return Ok(None);
    }
    let mgr = SessionManager::with_root(store_root.to_path_buf());
    let events = mgr
        .open_or_migrate(session_id)
        .await
        .map_err(|e| SessionApiError::Internal(e.to_string()))?;
    Ok(Some(conga::cache_stats(&events)))
}

/// Persist the session's display name in its `meta.json` sidecar. Creates
/// the session directory if needed, so a chat can be named before its first
/// turn lands on disk. Bad id/name → `BadRequest`.
pub async fn rename_session(
    store_root: &Path,
    session_id: &str,
    name: &str,
) -> Result<(), SessionApiError> {
    if !conga::is_valid_session_id(session_id) {
        return Err(SessionApiError::BadRequest("invalid session id".into()));
    }
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(SessionApiError::BadRequest(
            "name must be 1..=200 chars".into(),
        ));
    }
    EventStorage::new(store_root.to_path_buf())
        .write_meta(
            session_id,
            &SessionMeta {
                name: Some(name.to_string()),
            },
        )
        .await
        .map_err(|e| SessionApiError::Internal(e.to_string()))
}

/// Delete a session's on-disk data wholesale (event log + meta sidecar +
/// per-tool state) and kill its process-global tool state (persistent
/// shell, extension PTYs via cleanup hooks). `Ok(false)` = never existed
/// (`NotFound` is the caller's choice of framing; this returns the fact).
/// Transports that track live connections refuse the delete themselves
/// BEFORE calling this.
pub async fn delete_session(store_root: &Path, session_id: &str) -> Result<bool, SessionApiError> {
    if !conga::is_valid_session_id(session_id) {
        return Err(SessionApiError::BadRequest("invalid session id".into()));
    }
    let removed = EventStorage::new(store_root.to_path_buf())
        .remove_session(session_id)
        .await
        .map_err(|e| SessionApiError::Internal(e.to_string()))?;
    if removed {
        // The session is gone; its shell/PTYs must not linger in the
        // host process. Only on actual removal - a failed delete leaves
        // the session (and its state) intact for a retry.
        crate::session_cleanup::cleanup_session_resources(session_id).await;
    }
    Ok(removed)
}

/// Full-text search across all session event logs (FTS5 sidecar index).
/// Async (sqlx) — callers just `.await`. Runs the incremental high-water
/// reindex check first, then the query — per-call reindex is an incremental
/// stat check, not a full rebuild. Empty/blank q → BadRequest.
#[cfg(feature = "session-index")]
pub async fn search_sessions(
    store_root: &Path,
    index_db: &Path,
    q: &str,
    limit: usize,
) -> Result<Vec<crate::session_index::SessionHit>, SessionApiError> {
    let q = q.trim();
    if q.is_empty() {
        return Err(SessionApiError::BadRequest("q must be non-empty".into()));
    }
    crate::session_index::reindex(store_root, index_db)
        .await
        .map_err(|e| SessionApiError::Internal(e.to_string()))?;
    crate::session_index::search(store_root, index_db, q, limit)
        .await
        .map_err(|e| SessionApiError::Internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::SessionEvent;

    fn user_event(text: &str) -> SessionEvent {
        use conga::{AgentMessage, ContentBlock, UserMessage};
        SessionEvent::User(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(text)],
            timestamp: 1,
        }))
    }

    /// An assistant event carrying provider usage (the rows `cache_stats`
    /// folds).
    fn assistant_event_with_usage(
        input_tokens: u64,
        cache_read_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
    ) -> SessionEvent {
        use conga::{AgentMessage, Usage};
        SessionEvent::Assistant {
            message: AgentMessage::assistant_text("ok"),
            usage: Some(Usage {
                input_tokens,
                output_tokens: 1,
                cache_read_tokens,
                cache_write_tokens,
            }),
        }
    }

    #[tokio::test]
    async fn messages_none_for_unknown_session_some_after_write() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(session_messages(tmp.path(), "ghost")
            .await
            .unwrap()
            .is_none());
        EventStorage::new(tmp.path().to_path_buf())
            .append_event("sess-1", &user_event("hello"))
            .await
            .unwrap();
        let msgs = session_messages(tmp.path(), "sess-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn rename_validates_then_persists() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            rename_session(tmp.path(), "../evil", "ok").await,
            Err(SessionApiError::BadRequest(_))
        ));
        assert!(matches!(
            rename_session(tmp.path(), "sess-1", "  ").await,
            Err(SessionApiError::BadRequest(_))
        ));
        rename_session(tmp.path(), "sess-1", " Release notes ")
            .await
            .unwrap();
        let items = list_sessions(tmp.path()).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name.as_deref(), Some("Release notes"));
    }

    #[tokio::test]
    async fn delete_reports_existence() {
        let tmp = tempfile::tempdir().unwrap();
        EventStorage::new(tmp.path().to_path_buf())
            .append_event("sess-1", &user_event("x"))
            .await
            .unwrap();
        assert!(delete_session(tmp.path(), "sess-1").await.unwrap());
        assert!(!delete_session(tmp.path(), "sess-1").await.unwrap());
        assert!(!EventStorage::new(tmp.path().to_path_buf()).has_events("sess-1"));
    }

    #[tokio::test]
    async fn delete_runs_cleanup_hooks_for_removed_session() {
        static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        crate::session_cleanup::register_hook(std::sync::Arc::new(|sid| {
            SEEN.lock().unwrap().push(sid.to_string())
        }));
        let tmp = tempfile::tempdir().unwrap();
        EventStorage::new(tmp.path().to_path_buf())
            .append_event("sess-hook", &user_event("x"))
            .await
            .unwrap();
        delete_session(tmp.path(), "sess-hook").await.unwrap();
        assert!(
            SEEN.lock().unwrap().contains(&"sess-hook".to_string()),
            "delete must run cleanup hooks for the removed session"
        );
    }

    // ---- SessionApiError::fmt ----

    #[test]
    fn session_api_error_displays_only_its_message() {
        assert_eq!(
            SessionApiError::BadRequest("invalid session id".into()).to_string(),
            "invalid session id"
        );
        assert_eq!(
            SessionApiError::NotFound("no such session".into()).to_string(),
            "no such session"
        );
        assert_eq!(
            SessionApiError::Internal("disk on fire".into()).to_string(),
            "disk on fire"
        );
        // The variant name must not leak into the message: transports map the
        // variant to a status code, the message goes to the user.
        let rendered = SessionApiError::BadRequest("invalid session id".into()).to_string();
        assert!(!rendered.contains("BadRequest"), "{rendered}");
        assert!(!rendered.contains("SessionApiError"), "{rendered}");
    }

    #[test]
    fn session_api_error_displays_an_empty_message() {
        assert_eq!(SessionApiError::Internal(String::new()).to_string(), "");
    }

    // ---- session_cache_stats ----

    #[tokio::test]
    async fn cache_stats_are_none_for_a_session_without_on_disk_data() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            session_cache_stats(tmp.path(), "ghost")
                .await
                .unwrap()
                .is_none(),
            "no on-disk data means no stats (not zeroed stats)"
        );
    }

    #[tokio::test]
    async fn cache_stats_fold_the_usage_rows_of_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path().to_path_buf());
        store
            .append_event(
                "sess-1",
                &assistant_event_with_usage(100, Some(40), Some(10)),
            )
            .await
            .unwrap();
        store
            .append_event("sess-1", &assistant_event_with_usage(50, None, None))
            .await
            .unwrap();
        let stats = session_cache_stats(tmp.path(), "sess-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.input_tokens, 150);
        assert_eq!(stats.cache_read_tokens, 40);
        assert_eq!(stats.cache_write_tokens, 10);
        assert_eq!(stats.calls, 2);
        assert_eq!(
            stats.cache_reporting_calls, 1,
            "only the row that reported cache fields counts"
        );
        assert_eq!(stats.hit_rate(), Some(40.0 / 150.0));
    }

    #[tokio::test]
    async fn cache_stats_of_a_session_without_usage_are_zero() {
        let tmp = tempfile::tempdir().unwrap();
        EventStorage::new(tmp.path().to_path_buf())
            .append_event("sess-1", &user_event("hello"))
            .await
            .unwrap();
        let stats = session_cache_stats(tmp.path(), "sess-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.calls, 0);
        assert_eq!(stats.input_tokens, 0);
        assert_eq!(stats.hit_rate(), None, "no usage → no hit rate");
    }

    #[tokio::test]
    async fn cache_stats_ignore_rows_before_a_cleared_marker() {
        // The scope matches the conversation: a `/clear` restarts the count.
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path().to_path_buf());
        store
            .append_event(
                "sess-1",
                &assistant_event_with_usage(100, Some(40), Some(10)),
            )
            .await
            .unwrap();
        store
            .append_event("sess-1", &SessionEvent::Cleared)
            .await
            .unwrap();
        store
            .append_event("sess-1", &assistant_event_with_usage(10, None, None))
            .await
            .unwrap();
        let stats = session_cache_stats(tmp.path(), "sess-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.input_tokens, 10);
        assert_eq!(stats.calls, 1);
        assert_eq!(stats.cache_read_tokens, 0);
    }

    #[tokio::test]
    async fn cache_stats_of_a_corrupt_log_are_an_internal_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-1");
        std::fs::create_dir_all(&dir).unwrap();
        // Mid-file damage (not a torn tail) must fail loud.
        let good = serde_json::to_string(&user_event("ok")).unwrap();
        std::fs::write(
            dir.join("events.jsonl"),
            format!("{good}\nNOT_JSON\n{good}\n"),
        )
        .unwrap();
        let err = session_cache_stats(tmp.path(), "sess-1").await.unwrap_err();
        assert!(
            matches!(err, SessionApiError::Internal(_)),
            "a corrupt log must surface as Internal, got {err:?}"
        );
    }

    // ---- search_sessions ----

    #[cfg(feature = "session-index")]
    #[tokio::test]
    async fn search_sessions_rejects_a_blank_query() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        assert!(
            matches!(
                search_sessions(tmp.path(), &db, "   ", 10).await,
                Err(SessionApiError::BadRequest(_))
            ),
            "a blank query must be rejected before touching the index"
        );
        assert!(matches!(
            search_sessions(tmp.path(), &db, "", 10).await,
            Err(SessionApiError::BadRequest(_))
        ));
    }

    #[cfg(feature = "session-index")]
    #[tokio::test]
    async fn search_sessions_finds_the_matching_sessions() {
        let store_root = tempfile::tempdir().unwrap();
        let db_root = tempfile::tempdir().unwrap();
        let db = db_root.path().join("index.db");
        EventStorage::new(store_root.path().to_path_buf())
            .append_event("sess-1", &user_event("the flaky test failed"))
            .await
            .unwrap();
        EventStorage::new(store_root.path().to_path_buf())
            .append_event("sess-2", &user_event("unrelated chatter"))
            .await
            .unwrap();
        let hits = search_sessions(store_root.path(), &db, "flaky", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sess-1");
        assert!(hits[0].snippet.contains("flaky"));
    }
}
