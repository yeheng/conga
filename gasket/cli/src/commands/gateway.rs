//! Gateway 命令实现

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use colored::Colorize;
use tracing::info;

use gasket_engine::bus_adapter::EngineHandler;

use gasket_engine::config::{load_config, ModelRegistry};
use gasket_engine::cron::CronService;
use gasket_engine::providers::ProviderRegistry;
use gasket_engine::session::{AgentSession, ContextCompactor};
use gasket_engine::subagents::SimpleSpawner;
use gasket_engine::token_tracker::ModelPricing;
use gasket_engine::tools::ContextTool;
use gasket_engine::tools::{build_tool_registry, CronTool, Tool, ToolContext, ToolRegistryConfig};
use gasket_engine::tools::{MessageTool, ToolMetadata, ToolRegistry};
use gasket_engine::EventStore;
use gasket_engine::SqliteStore;
use gasket_engine::SubagentSpawner;

use crate::commands::broker_outbound::OutboundDispatcher;
use gasket_engine::broker::{BrokerPayload, SessionManager};
use gasket_types::SessionKey;

use super::registry::CliModelResolver;
use crate::provider::setup_vault;
use axum::response::IntoResponse;
use tower_http::cors::CorsLayer;

use super::command_host::CliCommandHost;
use super::dispatching_handler::DispatchingEngineHandler;
use gasket_command::builtins::{clear, exit, help, model, new as builtin_new, sessions};
use gasket_command::dispatcher::shared_help_snapshot;
use gasket_command::DispatcherBuilder;

/// Run the gateway command
pub async fn cmd_gateway() -> Result<()> {
    let config = load_config().await.context("Failed to load config")?;

    if let Err(errors) = config.validate() {
        print_validation_errors(&errors);
        return Ok(());
    }

    if config.channels.enabled_count() == 0 {
        print_no_channels_hint();
        return Ok(());
    }

    // ── Infrastructure initialization (Linus refactor: extracted to engine) ──
    let gasket_engine::bootstrap::EngineInfra {
        config,
        broker,
        sqlite_store,
    } = gasket_engine::bootstrap::init_engine_infra(
        gasket_engine::bootstrap::BrokerCapacity::gateway(),
    )
    .await
    .context("Failed to initialize engine infrastructure")?;

    let vault = setup_vault(&config)?;

    warn_disabled_features(&config.channels);

    println!("🐈 Starting gateway...\n");

    let workspace =
        gasket_engine::tools::resolve_exec_workspace(&config, std::path::Path::new("."));
    let cron_sqlite_store = SqliteStore::new()
        .await
        .expect("Failed to open SQLite store for cron persistence");
    let cron_service =
        Arc::new(CronService::new(workspace.clone(), cron_sqlite_store.cron_store()).await);

    let inbound_sender = gasket_channels::InboundSender::new(broker.clone());
    let providers = Arc::new(gasket_channels::ImProviders::from_config(
        &config.channels,
        inbound_sender.clone(),
    ));

    // Set up WebSocket approval callback if WebSocket is enabled
    let approval_callback = {
        let mut callback: Option<Arc<dyn gasket_types::ApprovalCallback>> = None;
        for provider in providers.iter() {
            #[cfg(feature = "websocket")]
            if let gasket_channels::ImProvider::WebSocket(ref adapter) = provider {
                let manager = adapter.manager().clone();
                let router = Arc::new(gasket_channels::ApprovalRouter::new());
                manager.set_approval_router(router.clone());
                callback = Some(Arc::new(gasket_channels::WebSocketApprovalCallback::new(
                    manager, router,
                )));
            }
        }
        callback
    };

    let (agent, tools, subagent_spawner) = setup_agent_pipeline(
        &config,
        vault,
        &workspace,
        &sqlite_store,
        &cron_service,
        approval_callback,
    )
    .await?;

    // Build the slash-command dispatcher for WebSocket clients.
    // Built-ins are registered here; user YAML commands are loaded from ~/.gasket/commands.
    let host = Arc::new(CliCommandHost::new(agent.clone(), Some(broker.clone())));
    let help_snap = shared_help_snapshot();
    let user_dir = dirs::home_dir().map(|h| h.join(".gasket/commands"));
    let mut dispatcher_builder = DispatcherBuilder::new()
        .host(host)
        .help_snapshot(help_snap.clone())
        .register_builtin(exit())
        .register_builtin(clear())
        .register_builtin(help(help_snap.clone()))
        .register_builtin(builtin_new())
        .register_builtin(sessions())
        .register_builtin(model());
    if let Some(p) = user_dir {
        dispatcher_builder = dispatcher_builder.user_dir(p);
    }
    // Register all tools (including plugins) as slash commands
    dispatcher_builder = super::plugin_commands::register_tool_commands(
        dispatcher_builder,
        tools.clone(),
        Some(subagent_spawner.clone()),
        Some(broker.clone()),
    );
    let dispatcher = Arc::new(
        dispatcher_builder
            .build()
            .await
            .context("failed to build slash-command dispatcher")?,
    );

    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    setup_http_server(&providers, &agent, &dispatcher, &mut tasks).await;
    setup_broker_pipeline(broker.clone(), &providers, &agent, &dispatcher, &mut tasks);
    start_heartbeat_service(broker.clone(), &workspace, &mut tasks);
    cron_service.ensure_system_cron_jobs().await;
    start_cron_checker(
        broker.clone(),
        &cron_service,
        tools,
        subagent_spawner,
        &mut tasks,
    );
    tasks.extend(providers.spawn_all(&inbound_sender));

    println!("\n🐈 Gateway running. Press Ctrl+C to stop.\n");
    tokio::signal::ctrl_c().await?;
    println!("\n🐈 Shutting down gracefully...");
    shutdown_tasks(tasks).await;

    Ok(())
}

fn print_validation_errors(errors: &[gasket_engine::ConfigValidationError]) {
    println!("{}", "Configuration validation failed:".red());
    for error in errors {
        println!("  - {}", error);
    }
    println!("\nPlease fix the configuration and try again.");
}

fn print_no_channels_hint() {
    println!("{}", "⚠️  No channels configured".yellow());
    println!("Add a channel to your config:");
    println!("\n  channels:");
    println!("    telegram:");
    println!("      enabled: true");
    println!("      token: \"YOUR_BOT_TOKEN\"");
    println!("      allow_from: []");
}

/// Warn when a channel is enabled in config but its compile-time feature is disabled.
fn warn_disabled_features(channels: &gasket_types::channel_config::ChannelsConfig) {
    let checks: [(&str, bool, bool); 6] = [
        (
            "telegram",
            cfg!(feature = "telegram"),
            channels.telegram.as_ref().is_some_and(|c| c.enabled),
        ),
        (
            "discord",
            cfg!(feature = "discord"),
            channels.discord.as_ref().is_some_and(|c| c.enabled),
        ),
        (
            "slack",
            cfg!(feature = "slack"),
            channels.slack.as_ref().is_some_and(|c| c.enabled),
        ),
        (
            "feishu",
            cfg!(feature = "feishu"),
            channels.feishu.as_ref().is_some_and(|c| c.enabled),
        ),
        (
            "wechat",
            cfg!(feature = "wechat"),
            channels.wechat.as_ref().is_some_and(|c| c.enabled),
        ),
        (
            "websocket",
            cfg!(feature = "websocket"),
            channels.websocket.as_ref().is_some_and(|c| c.enabled),
        ),
    ];

    for (name, compiled, enabled) in &checks {
        if *enabled && !compiled {
            tracing::warn!(
                "Channel '{}' is enabled in config but was NOT compiled. \
                 Rebuild with: cargo run --features {} -- gateway",
                name,
                name
            );
        }
    }
}

async fn setup_agent_pipeline(
    config: &gasket_engine::config::Config,
    vault: Option<Arc<gasket_engine::vault::VaultStore>>,
    workspace: &std::path::PathBuf,
    sqlite_store: &Arc<SqliteStore>,
    cron_service: &Arc<CronService>,
    approval_callback: Option<Arc<dyn gasket_types::ApprovalCallback>>,
) -> Result<(
    Arc<AgentSession>,
    Arc<ToolRegistry>,
    Arc<dyn SubagentSpawner>,
)> {
    let provider_info = crate::provider::find_provider(config, vault.as_deref())?;
    let mut agent_config = super::registry::build_agent_config(config);
    agent_config.model = provider_info.model.clone();

    if agent_config.thinking_enabled && !provider_info.supports_thinking {
        tracing::warn!(
            "Provider '{}' does not support thinking mode. Thinking disabled.",
            provider_info.provider_name
        );
        agent_config.thinking_enabled = false;
    }

    let model_registry = ModelRegistry::from_config(&config.agents);
    if !model_registry.is_empty() {
        info!(
            "Model switching enabled with {} model profiles: {}",
            model_registry.len(),
            model_registry.list_available_models().join(", ")
        );
    }

    // Initialize embedding recall if configured.
    //
    // `embedding_recall` carries (searcher, indexer, event_store_tx) so the
    // channel invariant is encoded in the type: either all three are present
    // or none are.
    #[cfg(feature = "embedding")]
    let (history_search, embedding_recall) = if let Some(ref emb_cfg) = config.embedding {
        let event_store = gasket_engine::EventStore::new(sqlite_store.pool());
        let tx = event_store.sender();
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
                (Some(params), Some((searcher, indexer, tx)))
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

    let orchestrator_tools = build_tool_registry(ToolRegistryConfig {
        subagent_spawner: None,
        extra_tools: vec![],
        provider: Some(provider_info.provider.clone()),
        model: Some(provider_info.model.clone()),
        #[cfg(feature = "embedding")]
        history_search: history_search.clone(),
        role: gasket_types::AgentRole::Orchestrator,
    });

    let worker_tools = build_tool_registry(ToolRegistryConfig {
        subagent_spawner: None,
        extra_tools: vec![],
        provider: Some(provider_info.provider.clone()),
        model: Some(provider_info.model.clone()),
        #[cfg(feature = "embedding")]
        history_search: None, // workers don't need to search history
        role: gasket_types::AgentRole::Worker,
    });
    let worker_tools = Arc::new(worker_tools);

    let spawn_budget = gasket_types::SpawnBudget::new(
        gasket_engine::config::get_config()
            .tools
            .spawn
            .max_concurrency,
    );

    let extra_tools = build_extra_tools(cron_service, &provider_info, &agent_config, sqlite_store);

    let mut tools = orchestrator_tools.clone();
    for (tool, metadata) in extra_tools {
        tools.register_with_metadata(tool, metadata);
    }
    let tools = if let Some(callback) = approval_callback {
        Arc::new(tools.with_approval_callback(callback))
    } else {
        Arc::new(tools)
    };

    let pricing = provider_info
        .pricing
        .map(|(input, output, currency)| ModelPricing::new(input, output, &currency));

    // 1. Create agent session first (without spawner) so we can extract pending_asks
    #[cfg(feature = "embedding")]
    let mut agent = if let Some((searcher, indexer, event_store_tx)) = embedding_recall {
        AgentSession::with_sqlite_store_and_embedding(
            provider_info.provider.clone(),
            workspace.clone(),
            agent_config.clone(),
            tools.clone(),
            sqlite_store.clone(),
            gasket_engine::session::builder::EmbeddingContext {
                searcher,
                indexer,
                event_store_tx,
            },
        )
        .await
        .context("Failed to initialize agent (check workspace bootstrap files)")?
    } else {
        AgentSession::with_sqlite_store(
            provider_info.provider.clone(),
            workspace.clone(),
            agent_config.clone(),
            tools.clone(),
            sqlite_store.clone(),
        )
        .await
        .context("Failed to initialize agent (check workspace bootstrap files)")?
    };
    #[cfg(not(feature = "embedding"))]
    let mut agent = AgentSession::with_sqlite_store(
        provider_info.provider.clone(),
        workspace.clone(),
        agent_config.clone(),
        tools.clone(),
        sqlite_store.clone(),
    )
    .await
    .context("Failed to initialize agent (check workspace bootstrap files)")?;

    // 2. Build spawner with the session's pending-ask registry so subagents can use ask_user
    let subagent_spawner: Arc<dyn SubagentSpawner> = Arc::new(
        SimpleSpawner::new(
            provider_info.provider.clone(),
            worker_tools,
            workspace.clone(),
            spawn_budget,
        )
        .with_pending_asks(agent.pending_asks())
        .with_thinking_enabled(agent_config.thinking_enabled)
        .with_model_resolver(Arc::new(CliModelResolver {
            provider_registry: {
                let mut r = ProviderRegistry::from_config(config);
                if let Some(ref v) = vault {
                    r.with_vault(v.clone());
                }
                r
            },
            model_registry,
        })),
    );

    agent = agent
        .with_pricing(pricing)
        .with_spawner(subagent_spawner.clone());
    let agent = Arc::new(agent);

    Ok((agent, tools, subagent_spawner))
}

fn build_extra_tools(
    cron_service: &Arc<CronService>,
    provider_info: &crate::provider::ProviderInfo,
    agent_config: &gasket_engine::session::AgentConfig,
    sqlite_store: &Arc<SqliteStore>,
) -> Vec<(Box<dyn Tool>, ToolMetadata)> {
    let mut ext = vec![(
        Box::new(MessageTool) as Box<dyn Tool>,
        ToolMetadata {
            display_name: "Send Message".to_string(),
            category: "communication".to_string(),
            tags: vec!["message".to_string(), "send".to_string()],
            requires_approval: false,
            is_mutating: false,
        },
    )];

    ext.push((
        Box::new(CronTool::new(cron_service.clone())) as Box<dyn Tool>,
        ToolMetadata {
            display_name: "Schedule Task".to_string(),
            category: "system".to_string(),
            tags: vec!["cron".to_string(), "schedule".to_string()],
            requires_approval: false,
            is_mutating: false,
        },
    ));

    let ctx_pool = sqlite_store.pool();
    let ctx_event_store = EventStore::new(ctx_pool.clone());
    let ctx_session_store = gasket_engine::SessionStore::new(ctx_pool);
    let mut ctx_compactor = ContextCompactor::new(
        provider_info.provider.clone(),
        ctx_event_store,
        ctx_session_store,
        provider_info.model.clone(),
        8000,
    );
    if let Some(ref prompt) = agent_config.prompts.summarization {
        ctx_compactor = ctx_compactor.with_summarization_prompt(prompt.clone());
    }
    ext.push((
        Box::new(ContextTool::new(Arc::new(ctx_compactor))) as Box<dyn Tool>,
        ToolMetadata {
            display_name: "Context Management".to_string(),
            category: "system".to_string(),
            tags: vec!["context".to_string(), "compression".to_string()],
            requires_approval: false,
            is_mutating: true,
        },
    ));

    ext
}

async fn setup_http_server(
    providers: &Arc<gasket_channels::ImProviders>,
    agent: &Arc<AgentSession>,
    dispatcher: &Arc<gasket_command::Dispatcher>,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    #[cfg(any(feature = "websocket", feature = "feishu"))]
    {
        let providers_for_http = providers.clone();
        let agent_for_http = agent.clone();
        let dispatcher_for_http = dispatcher.clone();
        tasks.push(tokio::spawn(async move {
            let mut app = axum::Router::new();
            for provider in providers_for_http.iter() {
                if let Some(router) = provider.routes() {
                    app = app.merge(router);
                }
            }
            app = add_context_routes(app, agent_for_http, dispatcher_for_http);
            app = app.layer(CorsLayer::permissive());

            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], 3000));
            tracing::info!("HTTP server listening on {}", addr);
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    if let Err(e) = axum::serve(listener, app).await {
                        tracing::error!("HTTP server error: {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to bind HTTP server port 3000: {}", e);
                }
            }
        }));
    }
}

#[cfg(any(feature = "websocket", feature = "feishu"))]
fn add_context_routes(
    mut app: axum::Router,
    agent: Arc<AgentSession>,
    dispatcher: Arc<gasket_command::Dispatcher>,
) -> axum::Router {
    let agent_for_context = agent.clone();
    let agent_for_compact = agent;
    app = app
        .route(
            "/api/sessions/{session_key}/context",
            axum::routing::get(
                move |axum::extract::Path(session_key): axum::extract::Path<String>| {
                    let agent = agent_for_context.clone();
                    async move { handle_context_get(agent, session_key).await }
                },
            ),
        )
        .route(
            "/api/sessions/{session_key}/context/compact",
            axum::routing::post(
                move |axum::extract::Path(session_key): axum::extract::Path<String>| {
                    let agent = agent_for_compact.clone();
                    async move { handle_context_compact(agent, session_key).await }
                },
            ),
        )
        .route(
            "/api/commands",
            axum::routing::get(move || {
                let dispatcher = dispatcher.clone();
                async move { handle_commands_list(dispatcher).await }
            }),
        );
    app
}

async fn handle_context_get(
    agent: Arc<AgentSession>,
    session_key: String,
) -> axum::response::Response {
    let key = match SessionKey::parse(&session_key) {
        Some(k) => k,
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "Invalid session key"})),
            )
                .into_response();
        }
    };
    match (
        agent.get_context_stats(&key).await,
        agent.get_watermark_info(&key).await,
    ) {
        (Some(stats), Some(watermark)) => {
            let body = serde_json::json!({
                "context_stats": {
                    "token_budget": stats.token_budget,
                    "compaction_threshold": stats.compaction_threshold,
                    "threshold_tokens": stats.threshold_tokens,
                    "current_tokens": stats.current_tokens,
                    "usage_percent": stats.usage_percent,
                    "is_compressing": stats.is_compressing,
                },
                "watermark_info": {
                    "watermark": watermark.watermark,
                    "max_sequence": watermark.max_sequence,
                    "uncompacted_count": watermark.uncompacted_count,
                    "compacted_percent": watermark.compacted_percent,
                }
            });
            (axum::http::StatusCode::OK, axum::Json(body)).into_response()
        }
        _ => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "Session not found or no compactor available"})),
        )
            .into_response(),
    }
}

async fn handle_context_compact(
    agent: Arc<AgentSession>,
    session_key: String,
) -> axum::response::Response {
    let key = match SessionKey::parse(&session_key) {
        Some(k) => k,
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "Invalid session key"})),
            )
                .into_response();
        }
    };
    match agent.force_compact_and_wait(&key, &[]).await {
        Ok(()) => {
            match (
                agent.get_context_stats(&key).await,
                agent.get_watermark_info(&key).await,
            ) {
                (Some(stats), Some(watermark)) => {
                    let body = serde_json::json!({
                        "status": "compaction_completed",
                        "context_stats": {
                            "token_budget": stats.token_budget,
                            "compaction_threshold": stats.compaction_threshold,
                            "threshold_tokens": stats.threshold_tokens,
                            "current_tokens": stats.current_tokens,
                            "usage_percent": stats.usage_percent,
                            "is_compressing": stats.is_compressing,
                        },
                        "watermark_info": {
                            "watermark": watermark.watermark,
                            "max_sequence": watermark.max_sequence,
                            "uncompacted_count": watermark.uncompacted_count,
                            "compacted_percent": watermark.compacted_percent,
                        }
                    });
                    (axum::http::StatusCode::OK, axum::Json(body)).into_response()
                }
                _ => (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({ "status": "compaction_completed" })),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn handle_commands_list(
    dispatcher: Arc<gasket_command::Dispatcher>,
) -> axum::response::Response {
    let commands: Vec<serde_json::Value> = dispatcher
        .list_commands()
        .into_iter()
        .filter(|cmd| cmd.name != "exit")
        .map(|cmd| {
            serde_json::json!({
                "name": cmd.name,
                "description": cmd.description,
                "aliases": cmd.aliases,
            })
        })
        .collect();
    (axum::http::StatusCode::OK, axum::Json(commands)).into_response()
}

fn setup_broker_pipeline(
    broker: Arc<gasket_engine::broker::MemoryBroker>,
    providers: &Arc<gasket_channels::ImProviders>,
    agent: &Arc<AgentSession>,
    dispatcher: &Arc<gasket_command::Dispatcher>,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let outbound_dispatcher = OutboundDispatcher::new(broker.clone(), providers.clone());
    tasks.push(tokio::spawn(outbound_dispatcher.run()));

    let engine_handler = EngineHandler::new(agent.clone());
    let handler = Arc::new(DispatchingEngineHandler::new(
        engine_handler,
        dispatcher.clone(),
    ));
    let session_mgr = SessionManager::new(broker, handler, std::time::Duration::from_secs(3600));
    tasks.push(tokio::spawn(session_mgr.run()));
}

async fn shutdown_tasks(tasks: Vec<tokio::task::JoinHandle<()>>) {
    for task in &tasks {
        task.abort();
    }
    use tokio::time::{timeout, Duration};
    for task in tasks {
        let _ = timeout(Duration::from_millis(500), task).await;
    }
}

/// Start heartbeat service that periodically sends heartbeat tasks through the bus.
fn start_heartbeat_service(
    broker: Arc<gasket_engine::broker::MemoryBroker>,
    workspace: &Path,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let heartbeat = gasket_engine::heartbeat::HeartbeatService::new(workspace.to_path_buf());
    tasks.push(tokio::spawn(async move {
        heartbeat
            .run(|task_text| {
                let broker = broker.clone();
                async move {
                    let inbound = gasket_channels::InboundMessage {
                        channel: gasket_channels::ChannelType::Cli,
                        sender_id: "heartbeat".to_string(),
                        chat_id: "heartbeat".to_string(),
                        content: task_text,
                        media: None,
                        metadata: None,
                        timestamp: chrono::Utc::now(),
                        trace_id: None,
                    };
                    let envelope = gasket_engine::broker::Envelope::new(
                        gasket_engine::broker::Topic::Inbound,
                        BrokerPayload::Inbound(inbound),
                    );
                    let _ = broker.publish(envelope).await;
                }
            })
            .await;
    }));
}

/// Start cron checker that polls for due jobs every 60 seconds.
/// Supports direct tool execution (bypassing LLM) for zero-token system tasks.
fn start_cron_checker(
    broker: Arc<gasket_engine::broker::MemoryBroker>,
    cron_service: &Arc<CronService>,
    tools: Arc<ToolRegistry>,
    spawner: Arc<dyn SubagentSpawner>,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let cron_svc = cron_service.clone();
    tasks.push(tokio::spawn(async move {
        let broker = broker;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let due = cron_svc.get_due_jobs();
            for job in due {
                tracing::info!("Cron job due: {} ({})", job.name, job.id);

                let channel = job
                    .channel
                    .as_deref()
                    .and_then(|c| serde_json::from_value(serde_json::json!(c)).ok())
                    .unwrap_or(gasket_channels::ChannelType::Cli);
                let chat_id = job.chat_id.clone().unwrap_or_else(|| "cron".to_string());
                let is_broadcast = chat_id == "*";

                // Check if this is a direct tool execution job (bypass LLM)
                if let Some(ref tool_name) = job.tool {
                    // Direct tool execution path - ZERO LLM tokens consumed
                    tracing::info!(
                        "Executing cron job '{}' directly via tool '{}' (bypassing LLM)",
                        job.name,
                        tool_name
                    );

                    // Build ToolContext with broker-based outbound
                    let ctx = ToolContext::default()
                        .outbound_tx({
                            // Create a temporary mpsc channel for tool output,
                            // then forward to broker. This preserves the
                            // ToolContext API while using broker underneath.
                            let (tx, mut rx) =
                                tokio::sync::mpsc::channel::<gasket_channels::OutboundMessage>(16);
                            let broker2 = broker.clone();
                            tokio::spawn(async move {
                                while let Some(msg) = rx.recv().await {
                                    let envelope = gasket_engine::broker::Envelope::new(
                                        gasket_engine::broker::Topic::Outbound,
                                        BrokerPayload::Outbound(msg),
                                    );
                                    let _ = broker2.publish(envelope).await;
                                }
                            });
                            tx
                        })
                        .spawner(spawner.clone());

                    let args = job.tool_args.clone().unwrap_or(serde_json::json!({}));

                    // Execute tool directly
                    match tools.execute(tool_name, args, &ctx).await {
                        Ok(result) => {
                            tracing::info!("Cron job '{}' completed successfully.", job.name);
                            tracing::info!("{}", result);
                            // Send result to output channel
                            let out_msg = if is_broadcast {
                                gasket_channels::OutboundMessage::broadcast(channel, result)
                            } else {
                                gasket_channels::OutboundMessage::new(channel, &chat_id, result)
                            };
                            let envelope = gasket_engine::broker::Envelope::new(
                                gasket_engine::broker::Topic::Outbound,
                                BrokerPayload::Outbound(out_msg),
                            );
                            let _ = broker.publish(envelope).await;
                        }
                        Err(e) => {
                            tracing::error!("Cron job '{}' failed: {}", job.name, e);
                            // Send error to output channel
                            let error_msg = format!("Cron job error: {}", e);
                            let out_msg = if is_broadcast {
                                gasket_channels::OutboundMessage::broadcast(channel, error_msg)
                            } else {
                                gasket_channels::OutboundMessage::new(channel, &chat_id, error_msg)
                            };
                            let envelope = gasket_engine::broker::Envelope::new(
                                gasket_engine::broker::Topic::Outbound,
                                BrokerPayload::Outbound(out_msg),
                            );
                            let _ = broker.publish(envelope).await;
                        }
                    }
                } else if is_broadcast {
                    // Broadcast path: send the message directly to all connected clients
                    let out_msg =
                        gasket_channels::OutboundMessage::broadcast(channel, job.message.clone());
                    let envelope = gasket_engine::broker::Envelope::new(
                        gasket_engine::broker::Topic::Outbound,
                        BrokerPayload::Outbound(out_msg),
                    );
                    let _ = broker.publish(envelope).await;
                } else {
                    // Traditional LLM-based path
                    let inbound = gasket_channels::InboundMessage {
                        channel,
                        sender_id: "cron".to_string(),
                        chat_id,
                        content: job.message.clone(),
                        media: None,
                        metadata: None,
                        timestamp: chrono::Utc::now(),
                        trace_id: None,
                    };
                    let envelope = gasket_engine::broker::Envelope::new(
                        gasket_engine::broker::Topic::Inbound,
                        BrokerPayload::Inbound(inbound),
                    );
                    let _ = broker.publish(envelope).await;
                }

                // Advance job tick and persist state to database
                // This ensures state survives restarts and missed ticks are handled
                match cron_svc.advance_job_tick(&job.id).await {
                    Ok((last_run, next_run)) => {
                        tracing::debug!(
                            "Advanced job {} tick: last_run={}, next_run={}",
                            job.id,
                            last_run,
                            next_run
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            "Failed to advance job {} tick: {}. Job may run again on next check.",
                            job.id,
                            e
                        );
                    }
                }
            }
        }
    }));
}
