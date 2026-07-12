//! Gateway 命令实现

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use colored::Colorize;
use tracing::info;

use gasket_engine::config::{load_config, ModelRegistry};
use gasket_engine::cron::CronService;
use gasket_engine::providers::ProviderRegistry;
use gasket_engine::session::{AgentSession, ContextCompactor};
use gasket_engine::subagents::SimpleSpawner;
use gasket_engine::token_tracker::ModelPricing;
use gasket_engine::tools::ContextTool;
use gasket_engine::tools::{build_tool_registry, CronTool, Tool, ToolContext, ToolRegistryConfig};
use gasket_engine::tools::{MessageTool, ToolMetadata, ToolRegistry};
use gasket_engine::SubagentSpawner;
use gasket_types::events::{InboundMessage, OutboundMessage};
use gasket_types::SessionKey;

use super::registry::CliModelResolver;
use crate::provider::setup_vault;
use axum::response::IntoResponse;
use tower_http::cors::CorsLayer;

use super::command_host::CliCommandHost;
use crate::command::builtins::{clear, exit, help, model, new as builtin_new, sessions};
use crate::command::dispatcher::shared_help_snapshot;
use crate::command::DispatcherBuilder;
use crate::command::{CommandResult, RouteOutcome};

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
        inbound_tx,
        inbound_rx,
        outbound_tx,
        outbound_rx,
        store,
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
    let cron_path = gasket_engine::config::config_dir()
        .join("state")
        .join("cron.json");
    let cron_service = Arc::new(CronService::new(workspace.clone(), cron_path).await);

    let inbound_sender = gasket_channels::InboundSender::new(inbound_tx.clone());
    let providers = Arc::new(gasket_channels::ImProviders::from_config(
        &config.channels,
        inbound_sender.clone(),
    ));
    let inbound_tx_heartbeat = inbound_tx.clone();
    let inbound_tx_cron = inbound_tx;

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
        &store,
        &cron_service,
        approval_callback,
    )
    .await?;

    // Build the slash-command dispatcher for WebSocket clients.
    // Built-ins are registered here; user YAML commands are loaded from ~/.gasket/commands.
    let host = Arc::new(CliCommandHost::new(
        agent.clone(),
        Some(outbound_tx.clone()),
    ));
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
        Some(outbound_tx.clone()),
    );
    let dispatcher = Arc::new(
        dispatcher_builder
            .build()
            .await
            .context("failed to build slash-command dispatcher")?,
    );

    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    setup_http_server(&providers, &agent, &dispatcher, &mut tasks).await;
    setup_direct_pipeline(
        inbound_rx,
        outbound_rx,
        &providers,
        &agent,
        &dispatcher,
        &mut tasks,
    )
    .await;
    start_heartbeat_service(inbound_tx_heartbeat, &workspace, &mut tasks);
    cron_service.ensure_system_cron_jobs().await;
    start_cron_checker(
        inbound_tx_cron,
        outbound_tx.clone(),
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
    store: &Arc<gasket_storage::JsonStore>,
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
    // `embedding_recall` carries (searcher, indexer).
    #[cfg(feature = "embedding")]
    let (history_search, embedding_recall) = if let Some(ref emb_cfg) = config.embedding {
        match gasket_engine::session::history::builder::setup_embedding_recall(store, emb_cfg).await
        {
            Ok((searcher, indexer)) => {
                let params = gasket_engine::tools::HistorySearchParams {
                    searcher: searcher.clone(),
                    config: emb_cfg.recall.clone(),
                };
                (Some(params), Some((searcher, indexer)))
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
        store: store.clone(),
    });

    let worker_tools = build_tool_registry(ToolRegistryConfig {
        subagent_spawner: None,
        extra_tools: vec![],
        provider: Some(provider_info.provider.clone()),
        model: Some(provider_info.model.clone()),
        #[cfg(feature = "embedding")]
        history_search: None, // workers don't need to search history
        role: gasket_types::AgentRole::Worker,
        store: store.clone(),
    });
    let worker_tools = Arc::new(worker_tools);

    let spawn_budget = gasket_types::SpawnBudget::new(
        gasket_engine::config::get_config()
            .tools
            .spawn
            .max_concurrency,
    );

    let extra_tools = build_extra_tools(cron_service, &provider_info, &agent_config, store);

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
    let mut agent = if let Some((searcher, indexer)) = embedding_recall {
        AgentSession::with_store_and_embedding(
            provider_info.provider.clone(),
            workspace.clone(),
            agent_config.clone(),
            tools.clone(),
            store.clone(),
            gasket_engine::session::builder::EmbeddingContext { searcher, indexer },
        )
        .await
        .context("Failed to initialize agent (check workspace bootstrap files)")?
    } else {
        AgentSession::with_store(
            provider_info.provider.clone(),
            workspace.clone(),
            agent_config.clone(),
            tools.clone(),
            store.clone(),
        )
        .await
        .context("Failed to initialize agent (check workspace bootstrap files)")?
    };
    #[cfg(not(feature = "embedding"))]
    let mut agent = AgentSession::with_store(
        provider_info.provider.clone(),
        workspace.clone(),
        agent_config.clone(),
        tools.clone(),
        store.clone(),
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
    store: &Arc<gasket_storage::JsonStore>,
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

    let ctx_event_store: Arc<dyn gasket_storage::EventStoreTrait> = store.clone();
    let ctx_session_store: Arc<dyn gasket_storage::SessionStoreTrait> = store.clone();
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
    dispatcher: &Arc<crate::command::Dispatcher>,
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
    dispatcher: Arc<crate::command::Dispatcher>,
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
    dispatcher: Arc<crate::command::Dispatcher>,
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

/// Sets up the direct pipeline: inbound dispatch loop + outbound send loop.
/// Replaces the old broker-based pipeline (OutboundDispatcher + SessionManager).
async fn setup_direct_pipeline(
    mut inbound_rx: tokio::sync::mpsc::Receiver<InboundMessage>,
    mut outbound_rx: tokio::sync::mpsc::Receiver<OutboundMessage>,
    providers: &Arc<gasket_channels::ImProviders>,
    agent: &Arc<AgentSession>,
    dispatcher: &Arc<crate::command::Dispatcher>,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let providers_out = providers.clone();
    // Outbound loop: read from mpsc channel and send to providers
    tasks.push(tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            // WebSocket is a streaming channel: chunks must arrive in strict
            // order. Inline the send instead of spawning to preserve FIFO.
            if msg.channel == gasket_channels::ChannelType::WebSocket {
                if let Err(e) = providers_out.send(&msg).await {
                    tracing::error!(target: "gateway::outbound", "Outbound delivery failed: {}", e);
                }
                continue;
            }
            let providers = providers_out.clone();
            tokio::spawn(async move {
                if let Err(e) = providers.send(&msg).await {
                    tracing::error!(target: "gateway::outbound", "Outbound delivery failed: {}", e);
                }
            });
        }
        tracing::info!(target: "gateway::outbound", "Outbound loop shutting down");
    }));

    let providers_in = providers.clone();
    let agent = agent.clone();
    let dispatcher = dispatcher.clone();
    // Inbound loop: route via dispatcher (slash commands), then fall back to agent
    tasks.push(tokio::spawn(async move {
        while let Some(msg) = inbound_rx.recv().await {
            let session_key = msg.session_key();

            // Try slash-command routing first
            let route_outcome = dispatcher.route(&msg.content, &session_key).await;
            match route_outcome {
                RouteOutcome::Handled(CommandResult::Print(text)) => {
                    let outbound = OutboundMessage::new(msg.channel, msg.chat_id, text);
                    if let Err(e) = providers_in.send(&outbound).await {
                        tracing::error!(target: "gateway::inbound", "Send failed: {}", e);
                    }
                }
                RouteOutcome::Handled(CommandResult::Error(text)) => {
                    let outbound = OutboundMessage::new(msg.channel, msg.chat_id, text);
                    if let Err(e) = providers_in.send(&outbound).await {
                        tracing::error!(target: "gateway::inbound", "Send failed: {}", e);
                    }
                }
                RouteOutcome::Handled(CommandResult::Quit) => {}
                RouteOutcome::Rewrite { prompt, .. } => {
                    dispatch_agent_turn(
                        agent.clone(),
                        InboundMessage {
                            content: prompt,
                            ..msg
                        },
                        providers_in.clone(),
                    )
                    .await;
                }
                RouteOutcome::Passthrough(text) => {
                    dispatch_agent_turn(
                        agent.clone(),
                        InboundMessage {
                            content: text,
                            ..msg
                        },
                        providers_in.clone(),
                    )
                    .await;
                }
            }
        }
        tracing::info!(target: "gateway::inbound", "Inbound loop shutting down");
    }));
}

/// Dispatch a message turn through the agent and send the results to providers.
async fn dispatch_agent_turn(
    agent: Arc<AgentSession>,
    msg: InboundMessage,
    providers: Arc<gasket_channels::ImProviders>,
) {
    use gasket_engine::session::HandleOutcome;

    let session_key = msg.session_key();
    let providers_for_err = providers.clone();

    match agent.handle_inbound(&msg.content, &session_key, None).await {
        Ok(HandleOutcome::Consumed) => {}
        Ok(HandleOutcome::Replied { mut events, result }) => {
            // Forward streaming ChatEvents as outbound messages
            while let Some(event) = events.recv().await {
                let outbound = OutboundMessage::with_ws_message(
                    msg.channel.clone(),
                    msg.chat_id.clone(),
                    event,
                );
                if let Err(e) = providers.send(&outbound).await {
                    tracing::error!(target: "gateway::dispatch", "Send failed: {}", e);
                }
            }
            // Await the final result; ChatEvents already delivered the content
            let _ = result.await;
        }
        Err(e) => {
            tracing::error!(target: "gateway::dispatch", "Agent error: {}", e);
            let error_out = OutboundMessage::new(msg.channel, msg.chat_id, format!("Error: {}", e));
            let _ = providers_for_err.send(&error_out).await;
        }
    }
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

/// Start heartbeat service that periodically sends heartbeat tasks through inbound channel.
fn start_heartbeat_service(
    inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
    workspace: &Path,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let heartbeat = gasket_engine::heartbeat::HeartbeatService::new(workspace.to_path_buf());
    tasks.push(tokio::spawn(async move {
        heartbeat
            .run(|task_text| {
                let inbound_tx = inbound_tx.clone();
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
                    let _ = inbound_tx.send(inbound).await;
                }
            })
            .await;
    }));
}

/// Start cron checker that polls for due jobs every 60 seconds.
/// Supports direct tool execution (bypassing LLM) for zero-token system tasks.
fn start_cron_checker(
    inbound_tx: tokio::sync::mpsc::Sender<InboundMessage>,
    outbound_tx: tokio::sync::mpsc::Sender<OutboundMessage>,
    cron_service: &Arc<CronService>,
    tools: Arc<ToolRegistry>,
    spawner: Arc<dyn SubagentSpawner>,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let cron_svc = cron_service.clone();
    tasks.push(tokio::spawn(async move {
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

                // Check if this is a direct tool execution job (bypassing LLM)
                if let Some(ref tool_name) = job.tool {
                    // Direct tool execution path - ZERO LLM tokens consumed
                    tracing::info!(
                        "Executing cron job '{}' directly via tool '{}' (bypassing LLM)",
                        job.name,
                        tool_name
                    );

                    // Build ToolContext with direct outbound channel
                    let ctx = ToolContext::default()
                        .outbound_tx(outbound_tx.clone())
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
                            let _ = outbound_tx.send(out_msg).await;
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
                            let _ = outbound_tx.send(out_msg).await;
                        }
                    }
                } else if is_broadcast {
                    // Broadcast path: send the message directly to all connected clients
                    let out_msg =
                        gasket_channels::OutboundMessage::broadcast(channel, job.message.clone());
                    let _ = outbound_tx.send(out_msg).await;
                } else {
                    // Traditional LLM-based path — forward to inbound channel
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
                    let _ = inbound_tx.send(inbound).await;
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
