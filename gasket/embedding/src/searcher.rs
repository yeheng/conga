//! Semantic search over stored embeddings.

use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, info};

use crate::index::MemoryIndex;
use crate::provider::EmbeddingProvider;
use crate::vector_store::VectorStore;
use gasket_storage::EventStoreTrait;
use gasket_types::EventType;

/// Configuration for recall search behavior.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RecallConfig {
    pub top_k: usize,
    pub token_budget: usize,
    pub min_score: f32,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            top_k: 5,
            token_budget: 500,
            min_score: 0.3,
        }
    }
}

/// A single recall search result with full event content already loaded.
#[derive(Debug, Clone)]
pub struct RecallHit {
    pub event_id: String,
    pub session_key: String,
    /// `"user"`, `"assistant"`, or `"system"`.
    pub role: String,
    pub content: String,
    pub score: f32,
    pub created_at: String,
}

/// Searches embeddings for semantically similar past events.
///
/// Two-tier architecture:
/// - **Hot index** (memory): recent embeddings for fast queries.
/// - **Cold store** (LanceDB): full historical embeddings.
pub struct RecallSearcher {
    provider: Arc<dyn EmbeddingProvider>,
    index: Arc<MemoryIndex>,
    store: Arc<dyn VectorStore>,
    event_store: Arc<dyn EventStoreTrait>,
}

impl RecallSearcher {
    pub fn new(
        provider: Arc<dyn EmbeddingProvider>,
        index: Arc<MemoryIndex>,
        store: Arc<dyn VectorStore>,
        event_store: Arc<dyn EventStoreTrait>,
    ) -> Self {
        Self {
            provider,
            index,
            store,
            event_store,
        }
    }

    /// Access the underlying embedding provider (for reuse by other subsystems).
    pub fn provider(&self) -> &Arc<dyn EmbeddingProvider> {
        &self.provider
    }

    /// Access the underlying vector store (for reuse by other subsystems).
    pub fn store(&self) -> &Arc<dyn VectorStore> {
        &self.store
    }

    /// Search for similar events. Returns `(event_id, score)` pairs sorted
    /// by descending score.
    ///
    /// Use this when you only need IDs (e.g. for deletion or further
    /// post-processing). For typical recall use cases prefer
    /// [`RecallSearcher::recall`], which also returns the event content.
    pub async fn recall_ids(
        &self,
        query: &str,
        config: &RecallConfig,
    ) -> Result<Vec<(String, f32)>> {
        debug!("Recalling with query: {:?}, config: {:?}", query, config);
        let query_vec = self.provider.embed(query).await?;

        // ── Tier 1: hot index (memory) ──────────────────────────────
        let overfetch = config.top_k.saturating_mul(2).max(1);
        let hot_raw = self.index.search(&query_vec, overfetch);
        let mut hot_results: Vec<(String, f32)> = hot_raw
            .into_iter()
            .filter(|(_, score)| *score >= config.min_score)
            .collect();

        // ── Tier 2: cold store, filling the gap ─────────────────────
        let needed = config.top_k.saturating_sub(hot_results.len());
        if needed > 0 {
            let hot_ids: std::collections::HashSet<String> =
                hot_results.iter().map(|(id, _)| id.clone()).collect();
            let cold = self
                .store
                .search(&query_vec, needed, config.min_score, &hot_ids)
                .await?;
            if !cold.is_empty() {
                info!("[RecallSearcher] cold store returned {} hits", cold.len());
            }
            hot_results.extend(cold.into_iter().map(|r| (r.id, r.score)));
        }

        hot_results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        hot_results.truncate(config.top_k);
        Ok(hot_results)
    }

    /// Search for similar events and return full content + role + timestamp
    /// joined from the underlying [`EventStore`].
    ///
    /// IDs returned by the vector store but missing from the event store
    /// (e.g. compacted away) are silently dropped. The remaining hits are
    /// returned in descending score order.
    pub async fn recall(&self, query: &str, config: &RecallConfig) -> Result<Vec<RecallHit>> {
        let scored = self.recall_ids(query, config).await?;
        if scored.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<uuid::Uuid> = scored
            .iter()
            .filter_map(|(id, _)| uuid::Uuid::parse_str(id).ok())
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let events = self
            .event_store
            .get_events_by_ids_global(&ids)
            .await
            .map_err(|e| anyhow::anyhow!("failed to load recalled events: {e}"))?;

        let score_map: std::collections::HashMap<String, f32> = scored.into_iter().collect();
        let mut hits: Vec<RecallHit> = events
            .into_iter()
            .map(|e| {
                let event_id = e.id.to_string();
                let score = score_map.get(&event_id).copied().unwrap_or(0.0);
                RecallHit {
                    event_id,
                    session_key: e.session_key,
                    role: role_str(&e.event_type).to_string(),
                    content: e.content,
                    score,
                    created_at: e.created_at.to_rfc3339(),
                }
            })
            .collect();

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(hits)
    }
}

fn role_str(event_type: &EventType) -> &'static str {
    match event_type {
        EventType::UserMessage => "user",
        EventType::AssistantMessage => "assistant",
        _ => "system",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::vector_store::VectorStore;
    use chrono::Utc;
    use gasket_types::{EventMetadata, SessionEvent};
    use uuid::Uuid;

    /// Build a JsonStore-backed event store in a fresh temp dir.
    ///
    /// The `TempDir` is returned alongside the store and must outlive the test.
    fn make_event_store() -> (
        Arc<dyn EventStoreTrait>,
        Arc<gasket_storage::JsonStore>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(gasket_storage::JsonStore::new(dir.path().to_path_buf()));
        (store.clone() as Arc<dyn EventStoreTrait>, store, dir)
    }

    /// Build a LanceDB-backed vector store in a fresh temp dir.
    ///
    /// The `TempDir` is returned alongside the store and must outlive the test.
    async fn make_store(dim: usize) -> (Arc<dyn VectorStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::store::VectorStoreConfig::LanceDB {
            path: dir.path().to_string_lossy().to_string(),
            table: "event_embeddings".to_string(),
        };
        let store = crate::store::build_vector_store(&config, dim)
            .await
            .unwrap();
        (store, dir)
    }

    fn make_event(content: &str, ty: EventType) -> SessionEvent {
        SessionEvent {
            id: Uuid::now_v7(),
            session_key: "sess".to_string(),
            event_type: ty,
            content: content.to_string(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        }
    }

    #[test]
    fn test_config_defaults() {
        let config = RecallConfig::default();
        assert_eq!(config.top_k, 5);
        assert_eq!(config.token_budget, 500);
        assert!((config.min_score - 0.3f32).abs() < f32::EPSILON);
    }

    #[test]
    fn test_config_deserialize_defaults() {
        let json = "{}";
        let config: RecallConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.top_k, 5);
        assert_eq!(config.token_budget, 500);
        assert!((config.min_score - 0.3f32).abs() < f32::EPSILON);
    }

    #[test]
    fn test_config_deserialize_custom() {
        let json = r#"{"top_k": 10, "token_budget": 1000, "min_score": 0.5}"#;
        let config: RecallConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.top_k, 10);
        assert_eq!(config.token_budget, 1000);
        assert!((config.min_score - 0.5f32).abs() < f32::EPSILON);
    }

    #[test]
    fn test_config_serialize_roundtrip() {
        let config = RecallConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: RecallConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.top_k, config.top_k);
        assert_eq!(parsed.token_budget, config.token_budget);
        assert!((parsed.min_score - config.min_score).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_recall_returns_hits_with_content() {
        let (event_store, store_ref, _dir) = make_event_store();
        let (store, _vector_dir) = make_store(3).await;
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(MockProvider::new(3));
        let index = Arc::new(MemoryIndex::new(3));

        let e1 = make_event("hello rust", EventType::UserMessage);
        let e2 = make_event("python rocks", EventType::AssistantMessage);
        store_ref.append(&e1).await.unwrap();
        store_ref.append(&e2).await.unwrap();

        index
            .insert(e1.id.to_string(), vec![1.0, 0.0, 0.0])
            .unwrap();
        index
            .insert(e2.id.to_string(), vec![0.0, 1.0, 0.0])
            .unwrap();
        use crate::vector_store::VectorRecord;
        store
            .upsert(vec![
                VectorRecord {
                    id: e1.id.to_string(),
                    vector: vec![1.0, 0.0, 0.0],
                    session_key: e1.session_key.clone(),
                    event_type: "user_message".into(),
                    content_hash: "h1".into(),
                },
                VectorRecord {
                    id: e2.id.to_string(),
                    vector: vec![0.0, 1.0, 0.0],
                    session_key: e2.session_key.clone(),
                    event_type: "assistant_message".into(),
                    content_hash: "h2".into(),
                },
            ])
            .await
            .unwrap();

        let searcher = RecallSearcher::new(provider, index, store, event_store);

        let config = RecallConfig {
            min_score: 0.0,
            ..Default::default()
        };
        let hits = searcher.recall("anything", &config).await.unwrap();
        assert!(!hits.is_empty());
        // Both hits should have the populated content from the event store.
        for hit in &hits {
            assert!(hit.content == "hello rust" || hit.content == "python rocks");
            assert!(hit.role == "user" || hit.role == "assistant");
            assert!(!hit.created_at.is_empty());
        }
    }

    #[tokio::test]
    async fn test_recall_filters_by_min_score() {
        let (event_store, store_ref, _dir) = make_event_store();
        let (store, _vector_dir) = make_store(3).await;
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(MockProvider::new(3));
        let index = Arc::new(MemoryIndex::new(3));

        let e1 = make_event("anything", EventType::UserMessage);
        store_ref.append(&e1).await.unwrap();
        index
            .insert(e1.id.to_string(), vec![1.0, 0.0, 0.0])
            .unwrap();

        let searcher = RecallSearcher::new(provider, index, store, event_store);

        // MockProvider returns zero vectors → similarity is 0.0,
        // which fails default min_score=0.3.
        let hits = searcher
            .recall("anything", &RecallConfig::default())
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn test_recall_respects_top_k() {
        let (event_store, store_ref, _dir) = make_event_store();
        let (store, _vector_dir) = make_store(3).await;
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(MockProvider::new(3));
        let index = Arc::new(MemoryIndex::new(3));

        for i in 0..20 {
            let e = make_event(&format!("msg-{i}"), EventType::UserMessage);
            store_ref.append(&e).await.unwrap();
            index.insert(e.id.to_string(), vec![1.0, 0.0, 0.0]).unwrap();
        }

        let searcher = RecallSearcher::new(provider, index, store, event_store);
        let config = RecallConfig {
            top_k: 3,
            min_score: 0.0,
            ..Default::default()
        };
        let hits = searcher.recall("anything", &config).await.unwrap();
        assert!(hits.len() <= 3);
    }
}
