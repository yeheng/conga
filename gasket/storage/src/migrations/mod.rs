//! Database schema migrations.
//!
//! All schema changes are applied idempotently.
//! Base tables and indexes use `CREATE ... IF NOT EXISTS`.
//! Incremental column additions use `ALTER TABLE`; duplicate-column and
//! missing-table errors are silently ignored so these can run on any state.

use sqlx::SqlitePool;

/// Base schema: tables and indexes. Every statement is idempotent.
const BASE_SCHEMA: &[&str] = &[
    // sessions_v2
    r#"CREATE TABLE IF NOT EXISTS sessions_v2 (
        key             TEXT PRIMARY KEY,
        channel         TEXT NOT NULL DEFAULT '',
        chat_id         TEXT NOT NULL DEFAULT '',
        created_at      TEXT NOT NULL,
        updated_at      TEXT NOT NULL,
        last_consolidated_event TEXT,
        total_events    INTEGER NOT NULL DEFAULT 0,
        total_tokens    INTEGER NOT NULL DEFAULT 0
    )"#,
    // session_events
    r#"CREATE TABLE IF NOT EXISTS session_events (
        id              TEXT PRIMARY KEY,
        session_key     TEXT NOT NULL,
        channel         TEXT NOT NULL DEFAULT '',
        chat_id         TEXT NOT NULL DEFAULT '',
        event_type      TEXT NOT NULL,
        content         TEXT NOT NULL,
        tools_used      TEXT DEFAULT '[]',
        token_usage     TEXT,
        token_len       INTEGER NOT NULL DEFAULT 0,
        event_data      TEXT,
        extra           TEXT DEFAULT '{}',
        created_at      TEXT NOT NULL,
        sequence        INTEGER NOT NULL DEFAULT 0,
        FOREIGN KEY (session_key) REFERENCES sessions_v2(key) ON DELETE CASCADE
    )"#,
    // session_summaries
    r#"CREATE TABLE IF NOT EXISTS session_summaries (
        session_key            TEXT PRIMARY KEY,
        content                TEXT NOT NULL,
        covered_upto_sequence  INTEGER NOT NULL DEFAULT 0,
        created_at             TEXT NOT NULL,
        compaction_in_progress INTEGER NOT NULL DEFAULT 0,
        compaction_started_at  TEXT
    )"#,
    // session_checkpoints
    r#"CREATE TABLE IF NOT EXISTS session_checkpoints (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        session_key     TEXT NOT NULL,
        target_sequence INTEGER NOT NULL,
        summary         TEXT NOT NULL,
        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
        UNIQUE(session_key, target_sequence)
    )"#,
    // session_embeddings
    r#"CREATE TABLE IF NOT EXISTS session_embeddings (
        message_id  TEXT PRIMARY KEY,
        session_key TEXT NOT NULL,
        embedding   BLOB NOT NULL,
        created_at  TEXT NOT NULL DEFAULT (datetime('now')),
        FOREIGN KEY (session_key) REFERENCES sessions_v2(key) ON DELETE CASCADE
    )"#,
    // cron_state
    r#"CREATE TABLE IF NOT EXISTS cron_state (
        job_id      TEXT PRIMARY KEY,
        last_run_at TEXT,
        next_run_at TEXT
    )"#,
    // maintenance_state
    r#"CREATE TABLE IF NOT EXISTS maintenance_state (
        task_name       TEXT NOT NULL,
        target_id       TEXT NOT NULL,
        last_watermark  INTEGER NOT NULL DEFAULT 0,
        updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
        PRIMARY KEY (task_name, target_id)
    )"#,
    // kv_store
    r#"CREATE TABLE IF NOT EXISTS kv_store (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    )"#,
    // indexes
    "CREATE INDEX IF NOT EXISTS idx_sessions_channel ON sessions_v2(channel)",
    "CREATE INDEX IF NOT EXISTS idx_sessions_chat_id ON sessions_v2(chat_id)",
    "CREATE INDEX IF NOT EXISTS idx_sessions_v2_channel_chat ON sessions_v2(channel, chat_id)",
    "CREATE INDEX IF NOT EXISTS idx_events_channel_chat ON session_events(channel, chat_id)",
    "CREATE INDEX IF NOT EXISTS idx_events_created ON session_events(created_at)",
    "CREATE INDEX IF NOT EXISTS idx_events_type ON session_events(event_type)",
    "CREATE INDEX IF NOT EXISTS idx_events_session_type_created ON session_events(session_key, event_type, created_at DESC)",
    "CREATE INDEX IF NOT EXISTS idx_events_channel_chat_sequence ON session_events(channel, chat_id, sequence)",
    "CREATE INDEX IF NOT EXISTS idx_events_session_sequence ON session_events(session_key, sequence)",
    "CREATE INDEX IF NOT EXISTS idx_session_checkpoints_key_seq ON session_checkpoints(session_key, target_sequence)",
    "CREATE INDEX IF NOT EXISTS idx_session_embeddings_session_key ON session_embeddings(session_key)",
    "CREATE INDEX IF NOT EXISTS idx_cron_state_next_run ON cron_state(next_run_at)",
];

/// Incremental schema changes (column additions). Errors for missing tables or
/// duplicate columns are silently ignored so these can run on any database state.
const INCREMENTAL_SCHEMA: &[&str] = &[
    "ALTER TABLE session_summaries ADD COLUMN covered_upto_sequence INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE session_events ADD COLUMN sequence INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE session_summaries ADD COLUMN compaction_in_progress INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE session_summaries ADD COLUMN compaction_started_at TEXT",
];

/// Run all migrations on an existing pool.
pub async fn run_all(pool: &SqlitePool) -> anyhow::Result<()> {
    for sql in BASE_SCHEMA {
        sqlx::query(sql).execute(pool).await?;
    }
    for sql in INCREMENTAL_SCHEMA {
        if let Err(e) = sqlx::query(sql).execute(pool).await {
            if let Some(db_err) = e.as_database_error() {
                let msg = db_err.message().to_lowercase();
                if msg.contains("duplicate column name") || msg.contains("no such table") {
                    continue;
                }
            }
            return Err(e.into());
        }
    }
    Ok(())
}
