//! Vector store trait + LanceDB configuration.
//!
//! LanceDB is the sole vector backend. The SQLite brute-force backend was
//! removed during the SQLite-removal migration — LanceDB compiles whenever
//! this crate is present (i.e. when the engine `embedding` feature is on),
//! and is absent entirely when `embedding` is off.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// An embedding record loaded from the vector store.
pub struct StoredEmbedding {
    pub event_id: String,
    pub session_key: String,
    pub embedding: Vec<f32>,
    pub event_type: String,
    pub created_at: String,
}

/// A single embedding record to be upserted into a vector store.
pub struct VectorRecord {
    pub id: String,
    pub vector: Vec<f32>,
    pub session_key: String,
    pub event_type: String,
    pub content_hash: String,
}

/// Result from a vector similarity search.
pub struct SearchResult {
    pub id: String,
    pub score: f32,
}

/// Backend-agnostic vector storage interface.
///
/// Every implementation must be `Send + Sync` so it can be shared across
/// async tasks via `Arc<dyn VectorStore>`.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Upsert a batch of records. Idempotent — duplicate IDs are updated.
    async fn upsert(&self, records: Vec<VectorRecord>) -> Result<()>;

    /// Approximate nearest-neighbor search.
    ///
    /// Returns up to `top_k` results with score >= `min_score`, sorted by
    /// descending similarity. IDs in `exclude` are skipped.
    async fn search(
        &self,
        query: &[f32],
        top_k: usize,
        min_score: f32,
        exclude: &std::collections::HashSet<String>,
    ) -> Result<Vec<SearchResult>>;

    /// Delete records by ID. Returns the number of records removed.
    async fn delete(&self, ids: &[String]) -> Result<u64>;

    /// Check whether a record with the given ID exists.
    async fn exists(&self, id: &str) -> Result<bool>;

    /// Total number of stored records.
    async fn count(&self) -> Result<i64>;

    /// Return the embedding dimension this store was created with.
    fn dim(&self) -> usize;

    /// Load all stored embeddings (for full index rebuild).
    async fn load_all(&self) -> Result<Vec<StoredEmbedding>>;

    /// Load the most recent `limit` embeddings, ordered by created_at DESC.
    async fn load_recent(&self, limit: usize) -> Result<Vec<StoredEmbedding>>;
}

/// Configuration for the vector store backend. LanceDB is the sole backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum VectorStoreConfig {
    /// LanceDB embedded vector database with persistent ANN index.
    LanceDB {
        /// Path to the LanceDB database directory, e.g. "~/.gasket/vectors".
        #[serde(default = "default_path")]
        path: String,
        /// Table name inside the database. Defaults to "event_embeddings".
        #[serde(default = "default_table_name")]
        table: String,
    },
}

fn default_path() -> String {
    "~/.gasket/vectors".to_string()
}

fn default_table_name() -> String {
    "event_embeddings".to_string()
}

impl Default for VectorStoreConfig {
    fn default() -> Self {
        Self::LanceDB {
            path: default_path(),
            table: default_table_name(),
        }
    }
}

/// Build a `VectorStore` from configuration. LanceDB opens/creates the
/// database at the configured path.
pub async fn build_vector_store(
    config: &VectorStoreConfig,
    dim: usize,
) -> Result<Arc<dyn VectorStore>> {
    match config {
        VectorStoreConfig::LanceDB { path, table } => {
            let store = crate::lance_store::LanceVectorStore::open(path, table, dim).await?;
            Ok(Arc::new(store))
        }
    }
}
