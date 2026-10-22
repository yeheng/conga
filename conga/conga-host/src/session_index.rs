//! Session full-text search: an FTS5 sidecar index over the on-disk event
//! logs. Lives in conga-host behind Cargo feature `session-index`; the
//! gateway REST route and the desktop Tauri command are the two consumers.
//!
//! One SQLite database at `<config_dir>/index.db`. Every text-bearing
//! SessionEvent becomes one row; a per-session high-water mark in `meta`
//! keeps reindexing incremental. Built lazily on demand — no background
//! thread, write path untouched.

use std::path::Path;

use conga::{AgentMessage, EventStorage, SessionEvent};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// Shared hit shape for both consumers (gateway REST JSON and the desktop
/// Tauri command serialize this identically). `name` comes from the
/// session's `meta.json` sidecar; `snippet` is the FTS5 snippet.
#[derive(Debug, serde::Serialize)]
pub struct SessionHit {
    pub session_id: String,
    pub name: Option<String>,
    pub snippet: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Stats {
    /// Sessions that had new rows inserted this run.
    pub sessions: usize,
    pub events_indexed: usize,
}

/// Open (creating if needed) the sidecar index and ensure the schema.
/// Returns a single-connection pool — each call owns its connections for
/// the whole run, mirroring the old one-`Connection`-per-call design.
pub async fn init_db(db_path: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        // Concurrent desktop searches open their own pools; a transient
        // SQLITE_BUSY waits instead of erroring out.
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    sqlx::raw_sql(
        "CREATE VIRTUAL TABLE IF NOT EXISTS events USING fts5(\
             session_id UNINDEXED, seq UNINDEXED, kind UNINDEXED, text);\
         CREATE TABLE IF NOT EXISTS meta(\
             key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
    )
    .execute(&pool)
    .await?;
    Ok(pool)
}

struct Row {
    seq: usize,
    kind: &'static str,
    text: String,
}

fn block_text(content: &[conga::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            conga::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Project one session's event log into indexable rows. `seq` is the event's
/// index in the log (0-based), so the high-water mark stays monotonic even
/// though marker events produce no row.
fn event_rows(events: &[SessionEvent]) -> Vec<Row> {
    events
        .iter()
        .enumerate()
        .filter_map(|(seq, ev)| {
            let (kind, msg): (&'static str, &AgentMessage) = match ev {
                SessionEvent::User(m) => ("user", m),
                SessionEvent::Assistant { message: m, .. } => ("assistant", m),
                SessionEvent::ToolResult(m) => ("tool_result", m),
                // Compacted produces no rows: its frozen base repeats
                // messages already indexed from their original events —
                // indexing it would duplicate every pinned/kept message.
                SessionEvent::TurnStart
                | SessionEvent::TurnEnd { .. }
                | SessionEvent::Cleared
                | SessionEvent::Compacted { .. } => return None,
            };
            let content = match msg {
                AgentMessage::User(u) => block_text(&u.content),
                AgentMessage::Assistant(a) => block_text(&a.content),
                AgentMessage::ToolResult(t) => block_text(&t.content),
                AgentMessage::Custom(_) => return None,
            };
            if content.is_empty() {
                return None;
            }
            Some(Row {
                seq,
                kind,
                text: content,
            })
        })
        .collect()
}

/// Incremental reindex: for every session dir under `store_root`, append
/// only events past the per-session high-water mark stored in `meta`.
///
/// Async via sqlx: the SQLite work rides sqlx's connection worker threads;
/// callers simply `.await` — no `spawn_blocking` needed.
pub async fn reindex(store_root: &Path, db_path: &Path) -> anyhow::Result<Stats> {
    let pool = init_db(db_path).await?;
    let storage = EventStorage::new(store_root);
    let mut stats = Stats::default();
    // Fresh install: no store root yet → empty index, not an error (search
    // just returns no hits). Any other read failure still fails loud.
    let entries = match std::fs::read_dir(store_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Stats::default()),
        Err(e) => return Err(e.into()),
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|id| conga::is_valid_session_id(id))
        .collect();
    ids.sort(); // deterministic order for tests and logging
    for id in ids {
        let events = storage.load_events_sync(&id)?;
        // No mark yet → -1, so a text event at seq 0 is indexed on first run.
        let last: i64 = sqlx::query_scalar("SELECT value FROM meta WHERE key = ?1")
            .bind(&id)
            .fetch_optional(&pool)
            .await?
            .unwrap_or(-1);
        let mut inserted = 0usize;
        let mut max_seq = last;
        // One transaction per session: the row inserts and the meta
        // high-water mark commit together or not at all, so a mid-reindex
        // failure can never leave rows a later run would duplicate.
        let mut tx = pool.begin().await?;
        for row in event_rows(&events) {
            if (row.seq as i64) <= last {
                continue;
            }
            sqlx::query("INSERT INTO events(session_id, seq, kind, text) VALUES (?1, ?2, ?3, ?4)")
                .bind(&id)
                .bind(row.seq as i64)
                .bind(row.kind)
                .bind(row.text)
                .execute(&mut *tx)
                .await?;
            max_seq = max_seq.max(row.seq as i64);
            inserted += 1;
        }
        if inserted > 0 {
            sqlx::query(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(&id)
            .bind(max_seq)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        if inserted > 0 {
            stats.sessions += 1;
            stats.events_indexed += inserted;
        }
    }
    Ok(stats)
}

/// FTS5 MATCH search over the index. The query is phrase-quoted (inner
/// double quotes doubled) so user input can never be parsed as FTS5
/// syntax. Rows map to the shared `SessionHit`; ordering is bm25 rank,
/// `name` is enriched from the session's meta.json sidecar.
/// Callers must guard the empty-string query themselves: the engine
/// returns Err for "" (FTS5 empty-phrase syntax error); the gateway route
/// and the desktop command reject blank `q` before calling.
///
/// Async via sqlx — same contract as [`reindex`].
pub async fn search(
    store_root: &Path,
    db_path: &Path,
    q: &str,
    limit: usize,
) -> anyhow::Result<Vec<SessionHit>> {
    let pool = init_db(db_path).await?;
    let phrase = format!("\"{}\"", q.replace('"', "\"\""));
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT session_id, snippet(events, 3, '', '', '…', 16), rank \
         FROM events WHERE events MATCH ?1 ORDER BY rank LIMIT ?2",
    )
    .bind(phrase)
    .bind(limit as i64)
    .fetch_all(&pool)
    .await?;
    let storage = EventStorage::new(store_root);
    let mut hits = Vec::with_capacity(rows.len());
    for (session_id, snippet) in rows {
        let name = storage.load_meta_sync(&session_id).and_then(|m| m.name);
        hits.push(SessionHit {
            session_id,
            name,
            snippet,
        });
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::types::message::{FunctionCall, ToolCall};
    use conga::types::session_event::TurnEndReason;
    use conga::SessionMeta;
    use conga::{AssistantMessage, ContentBlock, EventStorage, StopReason, ToolResultMessage};

    fn user_ev(t: &str) -> SessionEvent {
        SessionEvent::User(AgentMessage::user(t))
    }

    // ---- block_text ----

    #[test]
    fn block_text_of_no_blocks_is_empty() {
        assert_eq!(block_text(&[]), "");
    }

    #[test]
    fn block_text_joins_text_blocks_with_newlines() {
        let blocks = vec![ContentBlock::text("first"), ContentBlock::text("second")];
        assert_eq!(block_text(&blocks), "first\nsecond");
    }

    #[test]
    fn block_text_skips_non_text_blocks() {
        // Tool-call and thinking blocks carry no indexable prose.
        let blocks = vec![
            ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: "t1".into(),
                    function: FunctionCall {
                        name: "bash".into(),
                        arguments: "{}".into(),
                    },
                },
            },
            ContentBlock::text("kept"),
            ContentBlock::Thinking {
                thinking: "hmm".into(),
            },
        ];
        assert_eq!(block_text(&blocks), "kept");
    }

    #[test]
    fn block_text_of_only_non_text_blocks_is_empty() {
        let blocks = vec![ContentBlock::Thinking {
            thinking: "hmm".into(),
        }];
        assert_eq!(block_text(&blocks), "");
    }

    #[test]
    fn block_text_keeps_embedded_newlines_and_unicode() {
        let blocks = vec![
            ContentBlock::text("line1\nline2"),
            ContentBlock::text("中文🙂"),
        ];
        assert_eq!(block_text(&blocks), "line1\nline2\n中文🙂");
    }

    #[test]
    fn block_text_of_a_single_empty_text_block_is_empty() {
        assert_eq!(block_text(&[ContentBlock::text("")]), "");
    }

    #[tokio::test]
    async fn event_rows_extracts_text_and_skips_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let events = vec![
            SessionEvent::TurnStart,
            user_ev("find the flaky test"),
            SessionEvent::TurnEnd {
                reason: TurnEndReason::Completed,
            },
        ];
        store.append_events("s1", &events).await.unwrap();
        let rows = event_rows(&store.load_events("s1").await.unwrap());
        assert_eq!(rows.len(), 1, "markers produce no rows");
        assert_eq!(rows[0].seq, 1, "seq is the log index, not the row index");
        assert_eq!(rows[0].kind, "user");
        assert!(rows[0].text.contains("flaky"));
    }

    #[tokio::test]
    async fn init_db_creates_fts5_and_meta_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        let pool = init_db(&db).await.unwrap();
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('events', 'meta')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 2, "FTS5 table and meta table must both exist");
        assert!(init_db(&db).await.is_ok(), "second open is idempotent");
    }

    #[tokio::test]
    async fn reindex_is_incremental_across_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        store
            .append_events("s1", &[SessionEvent::TurnStart, user_ev("first message")])
            .await
            .unwrap();
        let first = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!((first.sessions, first.events_indexed), (1, 1));

        store
            .append_events(
                "s1",
                &[
                    user_ev("second message"),
                    SessionEvent::TurnEnd {
                        reason: TurnEndReason::Completed,
                    },
                ],
            )
            .await
            .unwrap();
        let second = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!(
            second.events_indexed, 1,
            "only the newly appended text event"
        );

        let pool = init_db(&db).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 2, "no duplicate rows");
        let mark: i64 = sqlx::query_scalar("SELECT value FROM meta WHERE key = 's1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mark, 2, "high-water mark is the max indexed seq (0-based)");
    }

    #[tokio::test]
    async fn reindex_on_empty_root_is_zero_stats() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            reindex(tmp.path(), &tmp.path().join("index.db"))
                .await
                .unwrap(),
            Stats::default()
        );
    }

    #[tokio::test]
    async fn reindex_on_missing_root_is_default_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("no-such-store");
        let stats = reindex(&root, &tmp.path().join("index.db")).await.unwrap();
        assert_eq!(stats, Stats::default(), "fresh install → empty index");
        assert!(!root.exists(), "missing store root must not be created");
    }

    #[tokio::test]
    async fn reindex_indexes_tool_result_events() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        store
            .append_events(
                "s1",
                &[SessionEvent::ToolResult(AgentMessage::ToolResult(
                    ToolResultMessage {
                        tool_call_id: "t1".into(),
                        tool_name: "bash".into(),
                        content: vec![ContentBlock::text("rg found nothing")],
                        is_error: false,
                        timestamp: 0,
                    },
                ))],
            )
            .await
            .unwrap();
        let stats = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!((stats.sessions, stats.events_indexed), (1, 1));
        let pool = init_db(&db).await.unwrap();
        let text: String = sqlx::query_scalar("SELECT text FROM events WHERE kind = 'tool_result'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(text, "rg found nothing");
    }

    #[tokio::test]
    async fn reindex_skips_tool_call_only_assistant_events() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        let a = AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: "t1".into(),
                    function: FunctionCall {
                        name: "bash".into(),
                        arguments: "{}".into(),
                    },
                },
            }],
            model: String::new(),
            stop_reason: StopReason::ToolUse,
            usage: None,
            timestamp: 0,
            stream_indices: Vec::new(),
        };
        store
            .append_events(
                "s1",
                &[SessionEvent::Assistant {
                    message: AgentMessage::Assistant(a),
                    usage: None,
                }],
            )
            .await
            .unwrap();
        let stats = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!(stats, Stats::default(), "no text content → no row");
        let pool = init_db(&db).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "indexed count unchanged");
    }

    #[tokio::test]
    async fn reindex_indexes_seq0_text_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        // No TurnStart: the session's only text event sits at seq 0.
        store
            .append_events("s1", &[user_ev("sole message")])
            .await
            .unwrap();
        let first = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!(
            (first.sessions, first.events_indexed),
            (1, 1),
            "seq-0 text is indexed on first run"
        );
        let second = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!(second, Stats::default(), "not re-inserted on second run");
    }

    #[tokio::test]
    async fn search_returns_snippet_names_phrase_quoting_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        store
            .append_events("s1", &[user_ev("the flaky test failed again")])
            .await
            .unwrap();
        store
            .append_events("s2", &[user_ev("NEAR(a b) is fts5 syntax")])
            .await
            .unwrap();
        store
            .append_events("s3", &[user_ev("needle one")])
            .await
            .unwrap();
        store
            .append_events("s4", &[user_ev("needle two")])
            .await
            .unwrap();
        store
            .append_events("s5", &[user_ev("needle three")])
            .await
            .unwrap();
        store
            .write_meta(
                "s1",
                &SessionMeta {
                    name: Some("flaky hunt".into()),
                },
            )
            .await
            .unwrap();
        reindex(tmp.path(), &db).await.unwrap();

        let hits = search(tmp.path(), &db, "flaky", 20).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert_eq!(
            hits[0].name.as_deref(),
            Some("flaky hunt"),
            "name enriched from meta.json"
        );
        assert!(hits[0].snippet.contains("flaky"));
        assert!(
            search(tmp.path(), &db, "zebra", 20)
                .await
                .unwrap()
                .is_empty(),
            "no hit is empty, not error"
        );
        assert!(
            search(tmp.path(), &db, "NEAR(a b", 20).await.is_ok(),
            "syntax-looking input never parsed as FTS5"
        );
        assert_eq!(
            search(tmp.path(), &db, "needle", 2).await.unwrap().len(),
            2,
            "limit respected"
        );
    }
}
