//! Embedding-based semantic search for gasket history recall.

pub mod index;
pub mod indexer;
pub mod provider;
pub mod rig_adapter;
pub mod searcher;
pub mod store;

pub mod lance_store;

/// Backwards-compatible alias — the `vector_store` symbols moved into `store`
/// during the `gasket-embedding` consolidation. Existing callers that wrote
/// `gasket_embedding::vector_store::VectorStore` keep compiling.
pub mod vector_store {
    pub use crate::store::{
        build_vector_store, SearchResult, StoredEmbedding, VectorRecord, VectorStore,
        VectorStoreConfig,
    };
}

pub use index::MemoryIndex;
pub use indexer::EmbeddingIndexer;
pub use provider::{EmbeddingProvider, ProviderConfig};
pub use searcher::{RecallConfig, RecallHit, RecallSearcher};
pub use store::{
    build_vector_store, SearchResult, StoredEmbedding, VectorRecord, VectorStore, VectorStoreConfig,
};
