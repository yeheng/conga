//! Tool execution CLI — run a tool directly without going through the agent loop.

use std::sync::Arc;

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

    let store = Arc::new(gasket_engine::JsonStore::new(gasket_engine::config::config_dir()));

    gasket_engine::config::init_config(config.clone());

    // Initialize embedding recall if configured
    #[cfg(feature = "embedding")]
    let (history_search, _embedding_indexer) = if let Some(ref emb_cfg) = config.embedding {
        match gasket_engine::session::history::builder::setup_embedding_recall(
            &store,
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
        store: store.clone(),
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
