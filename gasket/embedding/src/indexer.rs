//! Embedding indexer that builds and maintains the search index.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::index::MemoryIndex;
use crate::provider::EmbeddingProvider;
use crate::vector_store::{VectorRecord, VectorStore};
use gasket_types::{EventType, SessionEvent};

const MIN_CONTENT_LEN: usize = 5;
const BATCH_SIZE: usize = 16;
const FLUSH_INTERVAL_MS: u64 = 500;

/// Indexer that computes embeddings and maintains the in-memory search index.
pub struct EmbeddingIndexer {
    cancel: CancellationToken,
    handle: Option<JoinHandle<()>>,
}

impl EmbeddingIndexer {
    /// Spawn a background task that listens on the broadcast channel for events.
    pub fn start(
        provider: Arc<dyn EmbeddingProvider>,
        store: Arc<dyn VectorStore>,
        index: Arc<MemoryIndex>,
        mut rx: broadcast::Receiver<SessionEvent>,
    ) -> Result<Self> {
        let cancel = CancellationToken::new();
        let cancel_child = cancel.clone();

        let handle = tokio::spawn(async move {
            let mut buffer: Vec<SessionEvent> = Vec::with_capacity(BATCH_SIZE);
            let mut flush_interval =
                tokio::time::interval(std::time::Duration::from_millis(FLUSH_INTERVAL_MS));
            flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    biased;
                    _ = cancel_child.cancelled() => {
                        if !buffer.is_empty() {
                            if let Err(e) = Self::process_batch(
                                provider.as_ref(),
                                store.as_ref(),
                                &index,
                                std::mem::take(&mut buffer),
                            ).await {
                                tracing::warn!("embedding indexer flush on cancel error: {e}");
                            }
                        }
                        break;
                    }
                    result = rx.recv() => {
                        match result {
                            Ok(event) => {
                                buffer.push(event);
                                if buffer.len() >= BATCH_SIZE {
                                    if let Err(e) = Self::process_batch(
                                        provider.as_ref(),
                                        store.as_ref(),
                                        &index,
                                        std::mem::take(&mut buffer),
                                    ).await {
                                        tracing::warn!("embedding indexer batch process error: {e}");
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("embedding indexer lagged {n} events");
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                if !buffer.is_empty() {
                                    if let Err(e) = Self::process_batch(
                                        provider.as_ref(),
                                        store.as_ref(),
                                        &index,
                                        std::mem::take(&mut buffer),
                                    ).await {
                                        tracing::warn!("embedding indexer flush on close error: {e}");
                                    }
                                }
                                break;
                            }
                        }
                    }
                    _ = flush_interval.tick(), if !buffer.is_empty() => {
                        if let Err(e) = Self::process_batch(
                            provider.as_ref(),
                            store.as_ref(),
                            &index,
                            std::mem::take(&mut buffer),
                        ).await {
                            tracing::warn!("embedding indexer batch process error: {e}");
                        }
                    }
                }
            }
        });

        Ok(Self {
            cancel,
            handle: Some(handle),
        })
    }

    /// Shut down the background task and wait for it to finish.
    pub async fn shutdown(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        }
    }
}

impl Drop for EmbeddingIndexer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl EmbeddingIndexer {
    /// Cold-start: load stored embeddings and insert into the in-memory index.
    ///
    /// When `limit` is `Some(n)`, only the most recent `n` embeddings are loaded
    /// (keeps memory bounded). `None` means load everything.
    pub async fn rebuild_index(
        store: &dyn VectorStore,
        index: &MemoryIndex,
        limit: Option<usize>,
    ) -> Result<usize> {
        let embeddings = match limit {
            Some(n) => store.load_recent(n).await?,
            None => store.load_all().await?,
        };
        let total = embeddings.len();

        for stored in embeddings {
            index.insert(stored.event_id, stored.embedding)?;
        }

        tracing::info!("rebuild_index: loaded {total} embeddings into index (limit={limit:?})");
        Ok(total)
    }

    /// Process a single event: filter, dedup, embed, persist.
    pub async fn process_event(
        provider: &dyn EmbeddingProvider,
        store: &dyn VectorStore,
        index: &MemoryIndex,
        event: SessionEvent,
    ) -> Result<()> {
        Self::process_batch(provider, store, index, vec![event]).await
    }

    /// Process a batch of events: filter, dedup, batch embed, bulk upsert.
    pub async fn process_batch(
        provider: &dyn EmbeddingProvider,
        store: &dyn VectorStore,
        index: &MemoryIndex,
        events: Vec<SessionEvent>,
    ) -> Result<()> {
        // Step 1: Filter event types and short content, dedup within batch.
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();

        for event in events {
            let event_type_str = match &event.event_type {
                EventType::UserMessage => "user_message",
                EventType::AssistantMessage => "assistant_message",
                _ => continue,
            };
            if event.content.len() < MIN_CONTENT_LEN {
                continue;
            }
            let event_id = event.id.to_string();
            if !seen.insert(event_id.clone()) {
                continue;
            }
            candidates.push((event, event_type_str));
        }

        if candidates.is_empty() {
            return Ok(());
        }

        // Step 2: Skip events already persisted.
        let mut to_embed = Vec::new();
        for (event, event_type_str) in candidates {
            let event_id = event.id.to_string();
            if store.exists(&event_id).await? {
                continue;
            }
            to_embed.push((event, event_type_str));
        }

        if to_embed.is_empty() {
            return Ok(());
        }

        // Step 3: Batch compute embeddings.
        let texts: Vec<&str> = to_embed.iter().map(|(e, _)| e.content.as_str()).collect();
        let embeddings = provider.embed_batch(&texts).await?;

        // Step 4: Build records and bulk upsert.
        let mut records = Vec::with_capacity(to_embed.len());
        let mut ids_and_embeddings = Vec::with_capacity(to_embed.len());

        for (idx, (event, event_type_str)) in to_embed.iter().enumerate() {
            let embedding = embeddings[idx].clone();
            let content_hash = xxhash_rust::xxh3::xxh3_64(event.content.as_bytes()).to_string();
            records.push(VectorRecord {
                id: event.id.to_string(),
                vector: embedding.clone(),
                session_key: event.session_key.clone(),
                event_type: event_type_str.to_string(),
                content_hash,
            });
            ids_and_embeddings.push((event.id.to_string(), embedding));
        }

        store.upsert(records).await?;

        // Step 5: Update in-memory index.
        for (id, embedding) in ids_and_embeddings {
            index.insert(id, embedding)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::vector_store::VectorStore;
    use chrono::Utc;
    use uuid::Uuid;

    /// Build a LanceDB-backed vector store in a fresh temp dir.
    ///
    /// The `TempDir` is returned alongside the store and must outlive the test.
    async fn test_store() -> (Arc<dyn VectorStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::store::VectorStoreConfig::LanceDB {
            path: dir.path().to_string_lossy().to_string(),
            table: "event_embeddings".to_string(),
        };
        let store = crate::store::build_vector_store(&config, 3).await.unwrap();
        (store, dir)
    }

    fn make_event(event_type: EventType, content: &str) -> SessionEvent {
        SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test-session".to_string(),
            event_type,
            content: content.to_string(),
            metadata: gasket_types::EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        }
    }

    #[tokio::test]
    async fn test_rebuild_index() {
        let (store, _vector_dir) = test_store().await;
        let index = MemoryIndex::new(3);

        store
            .upsert(vec![
                VectorRecord {
                    id: "evt-1".to_string(),
                    vector: vec![1.0, 0.0, 0.0],
                    session_key: "sess-a".to_string(),
                    event_type: "user_message".to_string(),
                    content_hash: "h1".to_string(),
                },
                VectorRecord {
                    id: "evt-2".to_string(),
                    vector: vec![0.0, 1.0, 0.0],
                    session_key: "sess-a".to_string(),
                    event_type: "assistant_message".to_string(),
                    content_hash: "h2".to_string(),
                },
                VectorRecord {
                    id: "evt-3".to_string(),
                    vector: vec![0.0, 0.0, 1.0],
                    session_key: "sess-a".to_string(),
                    event_type: "user_message".to_string(),
                    content_hash: "h3".to_string(),
                },
            ])
            .await
            .unwrap();

        let count = EmbeddingIndexer::rebuild_index(store.as_ref(), &index, None)
            .await
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(index.len(), 3);

        let results = index.search(&[1.0, 0.0, 0.0], 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "evt-1");
        assert!(results[0].1 > 0.99);
    }

    #[tokio::test]
    async fn test_process_event_user_message() {
        let provider = MockProvider::new(3);
        let (store, _vector_dir) = test_store().await;
        let index = MemoryIndex::new(3);

        let event = make_event(EventType::UserMessage, "Hello, this is a test message");

        EmbeddingIndexer::process_event(&provider, store.as_ref(), &index, event)
            .await
            .unwrap();

        assert_eq!(index.len(), 1);
    }

    #[tokio::test]
    async fn test_process_event_skips_tool_call() {
        let provider = MockProvider::new(3);
        let (store, _vector_dir) = test_store().await;
        let index = MemoryIndex::new(3);

        let event = make_event(
            EventType::ToolCall {
                tool_name: "search".to_string(),
                arguments: serde_json::json!({}),
            },
            "tool call content here",
        );

        EmbeddingIndexer::process_event(&provider, store.as_ref(), &index, event)
            .await
            .unwrap();

        assert_eq!(index.len(), 0, "tool calls should be skipped");
    }

    #[tokio::test]
    async fn test_process_event_skips_short_content() {
        let provider = MockProvider::new(3);
        let (store, _vector_dir) = test_store().await;
        let index = MemoryIndex::new(3);

        let event = make_event(EventType::UserMessage, "Hi");

        EmbeddingIndexer::process_event(&provider, store.as_ref(), &index, event)
            .await
            .unwrap();

        assert_eq!(index.len(), 0, "short content should be skipped");
    }

    #[tokio::test]
    async fn test_process_event_dedup() {
        let provider = MockProvider::new(3);
        let (store, _vector_dir) = test_store().await;
        let index = MemoryIndex::new(3);

        let event = make_event(EventType::UserMessage, "Hello, this is a test message");
        let event_id = event.id.to_string();

        EmbeddingIndexer::process_event(&provider, store.as_ref(), &index, event)
            .await
            .unwrap();
        assert_eq!(index.len(), 1);

        let event2 = SessionEvent {
            id: uuid::Uuid::parse_str(&event_id).unwrap(),
            ..make_event(EventType::UserMessage, "Different content but same ID")
        };

        EmbeddingIndexer::process_event(&provider, store.as_ref(), &index, event2)
            .await
            .unwrap();

        assert_eq!(index.len(), 1);
    }

    #[tokio::test]
    async fn test_start_and_shutdown() {
        let provider = Arc::new(MockProvider::new(3));
        let (store, _vector_dir) = test_store().await;
        let index = Arc::new(MemoryIndex::new(3));
        let (_tx, rx) = broadcast::channel::<SessionEvent>(16);

        let mut indexer =
            EmbeddingIndexer::start(provider, store, index, rx).expect("start indexer");

        indexer.shutdown().await;
        assert!(indexer.handle.is_none());
    }

    #[tokio::test]
    async fn test_start_processes_events() {
        let provider = Arc::new(MockProvider::new(3));
        let (store, _vector_dir) = test_store().await;
        let index = Arc::new(MemoryIndex::new(3));
        let (tx, rx) = broadcast::channel::<SessionEvent>(16);

        let mut indexer =
            EmbeddingIndexer::start(provider, store, index.clone(), rx).expect("start indexer");

        let event = make_event(EventType::UserMessage, "Hello from broadcast channel");
        tx.send(event).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert_eq!(
            index.len(),
            1,
            "event should be processed by background task"
        );

        indexer.shutdown().await;
    }
}
