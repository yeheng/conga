//! Embedding index CLI commands.

use anyhow::Result;

#[cfg(feature = "embedding")]
pub async fn cmd_embedding_rebuild(limit: Option<usize>) -> Result<()> {
    let config = gasket_engine::config::load_config().await?;
    let embedding_config = match config.embedding {
        Some(c) => c,
        None => {
            println!(
                "Embedding is not configured. Add [embedding] section to ~/.gasket/config.yaml"
            );
            return Ok(());
        }
    };

    let provider = embedding_config.provider.build()?;
    let dim = provider.dim();

    let vector_store = gasket_engine::embedding::vector_store::build_vector_store(
        &embedding_config.vector_store,
        dim,
    )
    .await?;

    let index = gasket_engine::embedding::MemoryIndex::new(dim);

    println!("Rebuilding embedding index...");
    println!("  Backend: {:?}", embedding_config.vector_store);
    println!("  Dimension: {}", dim);
    if let Some(limit) = limit {
        println!("  Limit: {} recent embeddings", limit);
    } else {
        println!("  Loading all embeddings");
    }

    let count = gasket_engine::embedding::EmbeddingIndexer::rebuild_index(
        vector_store.as_ref(),
        &index,
        limit,
    )
    .await?;

    println!("✅ Rebuilt index with {} embeddings", count);
    Ok(())
}

#[cfg(feature = "embedding")]
pub async fn cmd_embedding_stats() -> Result<()> {
    let config = gasket_engine::config::load_config().await?;
    let embedding_config = match config.embedding {
        Some(c) => c,
        None => {
            println!(
                "Embedding is not configured. Add [embedding] section to ~/.gasket/config.yaml"
            );
            return Ok(());
        }
    };

    let provider = embedding_config.provider.build()?;
    let dim = provider.dim();

    let vector_store = gasket_engine::embedding::vector_store::build_vector_store(
        &embedding_config.vector_store,
        dim,
    )
    .await?;

    let count = vector_store.count().await?;
    println!("Embedding Store Statistics:");
    println!("  Backend: {:?}", embedding_config.vector_store);
    println!("  Dimension: {}", dim);
    println!("  Total embeddings: {}", count);

    Ok(())
}

#[cfg(not(feature = "embedding"))]
pub async fn cmd_embedding_rebuild(_limit: Option<usize>) -> Result<()> {
    anyhow::bail!(
        "This build of gasket does not include embedding support. \
         Rebuild with --features embedding"
    )
}

#[cfg(not(feature = "embedding"))]
pub async fn cmd_embedding_stats() -> Result<()> {
    anyhow::bail!(
        "This build of gasket does not include embedding support. \
         Rebuild with --features embedding"
    )
}
