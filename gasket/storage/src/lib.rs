//! SQLite-backed storage and history processing for gasket.
//!
//! This crate provides:
//! - **Persistence:** Sessions, conversation messages, summaries, cron jobs
//! - **History:** Token-budget-aware history truncation and multi-dimensional retrieval

mod cron_store;
mod event_store;
pub mod fs;
pub mod global;
mod kv_store;
mod maintenance_store;
mod migrations;
pub mod session_store;

// ── Merged from gasket-history ──
pub mod processor;
pub mod query;

use std::path::PathBuf;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use tracing::debug;

pub use cron_store::CronStore;
pub use event_store::{EventFilter, EventStore, EventStoreTrait, StoreError};
pub use global::{get_db, init_db};
pub use kv_store::KvStore;
pub use maintenance_store::MaintenanceStore;
pub use session_store::SessionStore;

// ── History re-exports ──
pub use processor::{count_tokens, process_history, HistoryConfig, ProcessedHistory};
pub use query::{
    HistoryQuery, HistoryQueryBuilder, HistoryResult, QueryOrder, ResultMeta, TimeRange,
};

// Re-export sqlx types for consumers that need direct pool access
pub use sqlx::sqlite::SqliteRow;
pub use sqlx::{query as sql_query, query_as, Row, SqlitePool};

pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gasket")
}

/// SQLite-backed store — thin connection-pool manager.
///
/// Holds a single `sqlx::SqlitePool` and delegates all business logic to
/// dedicated repositories: [`SessionStore`], [`CronStore`], [`KvStore`],
/// [`MaintenanceStore`].
///
/// **Not** used for explicit long-term memory — that lives in Markdown files.
#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Create a new `SqliteStore` with the default database path
    pub async fn new() -> anyhow::Result<Self> {
        let path = config_dir().join("gasket.db");
        Self::with_path(path).await
    }

    /// Create a new `SqliteStore` with a custom database path.
    pub async fn with_path(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        let store = Self { pool };
        store.init_db().await?;
        store.health_check().await?;
        debug!("Opened SqliteStore at {:?}", path);
        Ok(store)
    }

    /// Get a clone of the underlying SQLite pool.
    pub fn pool(&self) -> SqlitePool {
        self.pool.clone()
    }

    /// Create a `SqliteStore` from an existing pool (no migrations).
    ///
    /// Intended for test setups and internal use where the caller already
    /// has a configured pool. Does NOT run migrations — the caller is
    /// responsible for ensuring the schema exists.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Convenience accessor — builds a [`SessionStore`] backed by the same pool.
    pub fn session_store(&self) -> SessionStore {
        SessionStore::new(self.pool.clone())
    }

    /// Convenience accessor — builds a [`CronStore`] backed by the same pool.
    pub fn cron_store(&self) -> CronStore {
        CronStore::new(self.pool.clone())
    }

    /// Convenience accessor — builds a [`KvStore`] backed by the same pool.
    pub fn kv_store(&self) -> KvStore {
        KvStore::new(self.pool.clone())
    }

    /// Convenience accessor — builds a [`MaintenanceStore`] backed by the same pool.
    pub fn maintenance_store(&self) -> MaintenanceStore {
        MaintenanceStore::new(self.pool.clone())
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&self.pool)
            .await?;
        if integrity != "ok" {
            anyhow::bail!("SQLite integrity check failed: {}", integrity);
        }

        sqlx::query(
            "INSERT OR REPLACE INTO cron_state (job_id, last_run_at, next_run_at) VALUES ('__health_check__', NULL, NULL)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("DELETE FROM cron_state WHERE job_id = '__health_check__'")
            .execute(&self.pool)
            .await?;

        debug!("SQLite health check passed");
        Ok(())
    }

    async fn init_db(&self) -> anyhow::Result<()> {
        migrations::run_all(&self.pool).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) async fn temp_store() -> SqliteStore {
        let path =
            std::env::temp_dir().join(format!("gasket_sqlite_test_{}.db", uuid::Uuid::new_v4()));
        SqliteStore::with_path(path).await.unwrap()
    }

    // ── Session Summary tests ──

    #[tokio::test]
    async fn test_sqlite_session_summary_save_and_load() {
        let store = temp_store().await;
        let session = store.session_store();
        let key = gasket_types::SessionKey::new(gasket_types::ChannelType::Cli, "test:123");

        assert!(session.load_summary(&key).await.unwrap().is_none());

        session
            .save_summary(&key, "This is a summary of the conversation.", 42)
            .await
            .unwrap();

        let summary = session.load_summary(&key).await.unwrap();
        assert_eq!(
            summary,
            Some(("This is a summary of the conversation.".to_string(), 42))
        );
    }

    #[tokio::test]
    async fn test_sqlite_session_summary_upsert() {
        let store = temp_store().await;
        let session = store.session_store();
        let key = gasket_types::SessionKey::new(gasket_types::ChannelType::Cli, "key1");

        session.save_summary(&key, "Summary v1", 10).await.unwrap();
        session.save_summary(&key, "Summary v2", 20).await.unwrap();

        let summary = session.load_summary(&key).await.unwrap();
        assert_eq!(summary, Some(("Summary v2".to_string(), 20)));
    }

    #[tokio::test]
    async fn test_sqlite_session_summary_delete() {
        let store = temp_store().await;
        let session = store.session_store();
        let key = gasket_types::SessionKey::new(gasket_types::ChannelType::Cli, "key1");

        session.save_summary(&key, "Summary", 5).await.unwrap();
        assert!(session.delete_summary(&key).await.unwrap());
        assert!(!session.delete_summary(&key).await.unwrap());
        assert!(session.load_summary(&key).await.unwrap().is_none());
    }

    // ── Generic KV tests ──

    #[tokio::test]
    async fn test_kv_roundtrip() {
        let store = temp_store().await;
        let kv = store.kv_store();

        assert!(kv.read("test_key").await.unwrap().is_none());
        kv.write("test_key", "test_value").await.unwrap();
        assert_eq!(
            kv.read("test_key").await.unwrap(),
            Some("test_value".to_string())
        );
    }

    #[tokio::test]
    async fn test_kv_overwrite() {
        let store = temp_store().await;
        let kv = store.kv_store();

        kv.write("key1", "v1").await.unwrap();
        kv.write("key1", "v2").await.unwrap();
        assert_eq!(kv.read("key1").await.unwrap(), Some("v2".to_string()));
    }

    #[tokio::test]
    async fn test_kv_delete() {
        let store = temp_store().await;
        let kv = store.kv_store();

        kv.write("del_key", "val").await.unwrap();
        assert!(kv.delete("del_key").await.unwrap());
        assert!(!kv.delete("del_key").await.unwrap());
        assert!(kv.read("del_key").await.unwrap().is_none());
    }
}
