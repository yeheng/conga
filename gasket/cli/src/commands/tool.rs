//! Tool execution CLI — run a tool directly without going through the agent loop.

use anyhow::{Context, Result};
use gasket_engine::config::load_config;
use gasket_engine::tools::{build_tool_registry, ToolContext, ToolRegistryConfig};


/// Execute a tool directly via CLI.
///
/// Example: gasket tool execute evolution '{"threshold": 20}'
pub async fn cmd_tool_execute(name: String, args: String) -> Result<()> {
    let config = load_config().await.context("Failed to load config")?;
    let vault = crate::provider::setup_vault(&config)?;
    let provider_info = crate::provider::find_provider(&config, vault.as_deref())
        .context("No provider available")?;

    let sqlite_store = gasket_engine::SqliteStore::new()
        .await
        .expect("Failed to open SqliteStore");

    #[cfg(feature = "embedding")]
    let pool = sqlite_store.pool();

    gasket_engine::config::init_config(config.clone());
    gasket_storage::init_db(sqlite_store);

    // Initialize embedding recall if configured
    #[cfg(feature = "embedding")]
    let (history_search, _embedding_indexer) = if let Some(ref emb_cfg) = config.embedding {
        let event_store = gasket_engine::EventStore::new(pool.clone());
        match gasket_engine::session::history::builder::setup_embedding_recall(
            &event_store,
            emb_cfg,
        )
        .await
        {
            Ok((searcher, indexer)) => {
                let params = gasket_engine::tools::HistorySearchParams {
                    searcher: searcher.clone(),
                    config: emb_cfg.recall.clone(),
                };
                (Some(params), Some(indexer))
            }
            Err(e) => {
                tracing::warn!("Failed to initialize embedding recall: {}", e);
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    // (non-embedding builds skip semantic recall initialization)

    let tools = build_tool_registry(ToolRegistryConfig {
        subagent_spawner: None,
        extra_tools: vec![],
        provider: Some(provider_info.provider.clone()),
        model: Some(provider_info.model.clone()),
        #[cfg(feature = "embedding")]
        history_search,
        role: gasket_types::AgentRole::Orchestrator,
    });

    let args_json: serde_json::Value = serde_json::from_str(&args)
        .with_context(|| format!("Failed to parse tool arguments as JSON: {}", args))?;

    let ctx = ToolContext::default();

    match tools.execute(&name, args_json, &ctx).await {
        Ok(result) => {
            println!("✓ Tool '{}' executed successfully.\n", name);
            println!("{}", result);
            Ok(())
        }
        Err(e) => {
            anyhow::bail!("Tool '{}' failed: {}", name, e);
        }
    }
}
