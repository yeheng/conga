//! Background indexing service — consumes `Topic::WikiChanged` events and
//! asynchronously updates the Tantivy search index.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gasket_broker::{BrokerError, Subscriber};
use gasket_storage::wiki::{PageIndex, PageStore};
use tracing::{debug, info, warn};

/// Trait for computing embeddings (injected from engine/embedding layer).
#[async_trait::async_trait]
pub trait WikiEmbeddingProvider: Send + Sync {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
}

/// Trait for upserting wiki page vectors (injected from engine/embedding layer).
#[async_trait::async_trait]
pub trait WikiVectorStore: Send + Sync {
    async fn upsert(&self, id: &str, vector: Vec<f32>, content: &str) -> anyhow::Result<()>;
    async fn search(
        &self,
        query: &[f32],
        top_k: usize,
        min_score: f32,
    ) -> anyhow::Result<Vec<WikiVectorHit>>;
    async fn delete(&self, id: &str) -> anyhow::Result<()>;
}

/// A single hit from wiki vector search.
#[derive(Debug, Clone)]
pub struct WikiVectorHit {
    pub id: String,
    pub score: f32,
}

/// Background actor that subscribes to `Topic::WikiChanged` and syncs
/// the Tantivy index and optionally the vector index.
pub struct WikiIndexingService {
    page_store: PageStore,
    page_index: Arc<PageIndex>,
    embedding_provider: Option<Arc<dyn WikiEmbeddingProvider>>,
    vector_store: Option<Arc<dyn WikiVectorStore>>,
}

impl WikiIndexingService {
    pub fn new(page_store: PageStore, page_index: Arc<PageIndex>) -> Self {
        Self {
            page_store,
            page_index,
            embedding_provider: None,
            vector_store: None,
        }
    }

    /// Attach semantic embedding capabilities.
    /// When set, wiki pages will be vectorized on every write.
    pub fn with_semantic(
        mut self,
        provider: Arc<dyn WikiEmbeddingProvider>,
        store: Arc<dyn WikiVectorStore>,
    ) -> Self {
        self.embedding_provider = Some(provider);
        self.vector_store = Some(store);
        self
    }

    /// Spawn the service as a background Tokio task.
    /// Consumes a broker `Subscriber` for WikiChanged events — no global singleton.
    pub fn spawn(self, mut sub: Subscriber) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!("WikiIndexingService started");

            // Guard to prevent concurrent rebuild tasks from piling up
            // when the consumer falls behind repeatedly.
            let is_rebuilding = Arc::new(AtomicBool::new(false));

            loop {
                match sub.recv().await {
                    Ok(envelope) => {
                        self.handle_envelope(&envelope.payload).await;
                    }
                    Err(BrokerError::ChannelClosed) => {
                        info!("WikiIndexingService: channel closed, shutting down");
                        break;
                    }
                    Err(BrokerError::Lagged(n)) => {
                        warn!(
                            "WikiIndexingService: lagged {} messages, scheduling async rebuild",
                            n
                        );
                        let index = self.page_index.clone();
                        let store = self.page_store.clone();
                        let guard = is_rebuilding.clone();
                        // Only spawn a rebuild if one isn't already running.
                        if !guard.load(Ordering::Relaxed) {
                            tokio::spawn(async move {
                                guard.store(true, Ordering::Relaxed);
                                if let Err(e) = index.rebuild(&store).await {
                                    warn!("WikiIndexingService: async rebuild failed: {}", e);
                                }
                                guard.store(false, Ordering::Relaxed);
                                info!("WikiIndexingService: async rebuild completed");
                            });
                        } else {
                            debug!("WikiIndexingService: rebuild already in progress, skipping");
                        }
                    }
                    Err(e) => {
                        warn!("WikiIndexingService: recv error: {}", e);
                    }
                }
            }
        })
    }

    async fn handle_envelope(&self, payload: &gasket_broker::BrokerPayload) {
        let path = match payload {
            gasket_broker::BrokerPayload::WikiChanged { path } => path,
            _ => return,
        };

        // Check if page still exists — if not, it was a deletion.
        match self.page_store.read(path).await {
            Ok(page) => {
                if let Err(e) = self.page_index.upsert(&page).await {
                    warn!("WikiIndexingService: failed to upsert {}: {}", path, e);
                } else {
                    debug!("WikiIndexingService: upserted {}", path);
                }

                // Compute embedding and upsert to vector store.
                if let (Some(provider), Some(vstore)) =
                    (&self.embedding_provider, &self.vector_store)
                {
                    // Use title + summary + full content for embedding.
                    let embed_text = match &page.summary {
                        Some(s) => format!("{} {}\n{}", page.title, s, page.content),
                        None => format!("{}\n{}", page.title, page.content),
                    };
                    match provider.embed(&embed_text).await {
                        Ok(vector) => {
                            if let Err(e) = vstore.upsert(path, vector, &embed_text).await {
                                warn!("WikiIndexingService: failed to vectorize {}: {}", path, e);
                            } else {
                                debug!("WikiIndexingService: vectorized {}", path);
                            }
                        }
                        Err(e) => {
                            warn!("WikiIndexingService: embedding failed for {}: {}", path, e);
                        }
                    }
                }
            }
            Err(_) => {
                // Page doesn't exist anymore — delete from index.
                if let Err(e) = self.page_index.delete(path).await {
                    warn!(
                        "WikiIndexingService: failed to delete {} from index: {}",
                        path, e
                    );
                } else {
                    debug!("WikiIndexingService: deleted {} from index", path);
                }
                // Remove vector if semantic indexing is enabled.
                if let Some(vstore) = &self.vector_store {
                    if let Err(e) = vstore.delete(path).await {
                        debug!(
                            "WikiIndexingService: failed to delete vector for {}: {}",
                            path, e
                        );
                    }
                }
            }
        }
    }
}
