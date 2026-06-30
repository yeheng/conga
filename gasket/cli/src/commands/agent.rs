//! Agent 命令实现

use std::io::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result};
use colored::Colorize;
use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};
use tracing::{info, Level};

use gasket_channels::SessionKey;
use crate::command::builtins::{clear, exit, help, model, new as builtin_new, sessions};
use crate::command::dispatcher::shared_help_snapshot;
use crate::command::{CommandCompleter, CommandResult, DispatcherBuilder, RouteOutcome};
use gasket_engine::config::ModelRegistry;
use gasket_engine::providers::ProviderRegistry;
use gasket_engine::session::{AgentResponse, AgentSession};
use gasket_engine::subagents::SimpleSpawner;
use gasket_engine::token_tracker::ModelPricing;
use gasket_engine::ModelResolver;
use gasket_types::events::ChatEvent;

use super::command_host::CliCommandHost;
use super::registry::CliModelResolver;
use crate::cli::AgentOptions;
use crate::provider::setup_vault;

/// Run the agent command
pub async fn cmd_agent(opts: AgentOptions) -> Result<()> {
    // Enable debug logging if requested
    if opts.logs {
        tracing_subscriber::fmt()
            .with_env_filter(Level::DEBUG.to_string())
            .try_init()
            .ok();
    }

    // ── Infrastructure initialization (Linus refactor: extracted to engine) ──
    let gasket_engine::bootstrap::EngineInfra {
        config,
        broker,
        store,
    } = gasket_engine::bootstrap::init_engine_infra(
        gasket_engine::bootstrap::BrokerCapacity::agent_repl(),
    )
    .await
    .context("Failed to initialize engine infrastructure")?;

    let workspace =
        gasket_engine::tools::resolve_exec_workspace(&config, std::path::Path::new("."));

    // Check for vault placeholders and unlock if needed (JIT setup)
    let vault = setup_vault(&config)?;

    // Find a provider
    let provider_info = crate::provider::find_provider(&config, vault.as_deref())?;

    // Create agent config
    let mut agent_config = super::registry::build_agent_config(&config);
    agent_config.model = provider_info.model.clone();

    // Handle thinking mode
    if opts.thinking || agent_config.thinking_enabled {
        if provider_info.supports_thinking {
            agent_config.thinking_enabled = true;
        } else {
            // Warn if thinking is requested but not supported
            println!(
                "{} Provider '{}' does not support thinking mode. Thinking disabled.",
                "⚠️".yellow(),
                provider_info.provider_name
            );
            agent_config.thinking_enabled = false;
        }
    }

    // Handle streaming mode
    if opts.no_stream {
        agent_config.streaming = false;
    }

    // Build model registry and provider registry for switch_model tool
    let model_registry = Arc::new(ModelRegistry::from_config(&config.agents));
    let mut provider_registry = ProviderRegistry::from_config(&config);
    if let Some(ref v) = vault {
        provider_registry.with_vault(v.clone());
    }
    let _provider_registry = Arc::new(provider_registry);

    // Log available models if any are configured
    if !model_registry.is_empty() {
        info!(
            "Model switching enabled with {} model profiles: {}",
            model_registry.len(),
            model_registry.list_available_models().join(", ")
        );
    }

    // Initialize embedding recall if configured.
    //
    // `embedding_recall` carries (searcher, indexer) as a single bundle.
    #[cfg(feature = "embedding")]
    let (history_search, embedding_recall) = if let Some(ref emb_cfg) = config.embedding {
        let store_for_emb = store.clone();
        match gasket_engine::session::history::builder::setup_embedding_recall(
            &store_for_emb,
            emb_cfg,
        )
        .await
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

    // Build Orchestrator (main agent) tool registry — includes spawn tools.
    let orchestrator_tools =
        gasket_engine::tools::build_tool_registry(gasket_engine::tools::ToolRegistryConfig {
            subagent_spawner: None,
            extra_tools: vec![],
            provider: Some(provider_info.provider.clone()),
            model: Some(provider_info.model.clone()),
            #[cfg(feature = "embedding")]
            history_search: history_search.clone(),
            role: gasket_types::AgentRole::Orchestrator,
            store: store.clone(),
        });
    let tools = Arc::new(orchestrator_tools);

    // Build Worker (subagent) tool registry — excludes spawn tools.
    let worker_tools =
        gasket_engine::tools::build_tool_registry(gasket_engine::tools::ToolRegistryConfig {
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

    // Build SpawnBudget from config.
    let spawn_budget = gasket_types::SpawnBudget::new(
        gasket_engine::config::get_config()
            .tools
            .spawn
            .max_concurrency,
    );

    // Create model resolver for subagent spawner to support model_id switching.
    let mut resolver_registry = ProviderRegistry::from_config(&config);
    if let Some(ref v) = vault {
        resolver_registry.with_vault(v.clone());
    }
    let model_resolver: Arc<dyn ModelResolver> = Arc::new(CliModelResolver {
        provider_registry: resolver_registry,
        model_registry: ModelRegistry::from_config(&config.agents),
    });

    // Convert pricing info to ModelPricing
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
            gasket_engine::session::builder::EmbeddingContext {
                searcher,
                indexer,
            },
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
    let subagent_spawner: Arc<dyn gasket_engine::SubagentSpawner> = Arc::new(
        SimpleSpawner::new(
            provider_info.provider.clone(),
            worker_tools,
            workspace.clone(),
            spawn_budget,
        )
        .with_pending_asks(agent.pending_asks())
        .with_thinking_enabled(agent_config.thinking_enabled)
        .with_model_resolver(model_resolver),
    );
    let subagent_spawner_for_commands = subagent_spawner.clone();

    // 3. Wire spawner and pricing into agent
    agent = agent.with_pricing(pricing).with_spawner(subagent_spawner);

    // Wrap once. Existing &self method calls pass through via Arc::Deref;
    // CliCommandHost needs an owned Arc clone for shared ownership.
    let agent = Arc::new(agent);

    let render_md = !opts.no_markdown;
    let use_streaming = !opts.no_stream;

    match opts.message {
        Some(msg) => {
            // Single message mode
            info!("Processing message: {}", msg);
            let session_key = SessionKey::new(gasket_channels::ChannelType::Cli, "direct");
            use gasket_engine::session::HandleOutcome;
            let outcome = agent.handle_inbound(&msg, &session_key, None).await?;
            match outcome {
                HandleOutcome::Consumed => {
                    if use_streaming {
                        println!("(answered)");
                    }
                }
                HandleOutcome::Replied {
                    events: mut event_rx,
                    result: result_handle,
                } => {
                    if use_streaming {
                        let forward_handle = tokio::spawn(async move {
                            while let Some(event) = event_rx.recv().await {
                                match event {
                                    ChatEvent::Content { content } => print!("{}", content),
                                    ChatEvent::Thinking { content } => {
                                        eprint!("{}", content.dimmed().italic());
                                        std::io::stderr().flush().ok();
                                    }
                                    ChatEvent::Done => println!(),
                                    _ => {}
                                }
                            }
                        });
                        let (result, _) = tokio::join!(result_handle, forward_handle);
                        if let Err(e) = result {
                            return Err(anyhow::anyhow!("Task join error: {}", e));
                        }
                    } else {
                        // Drain events in non-streaming mode; only the final result matters.
                        tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
                        let resp = result_handle
                            .await
                            .map_err(|e| anyhow::anyhow!("Task join error: {}", e))??;
                        print_response_with_reasoning(&resp, render_md);
                    }
                }
            }
        }
        None => {
            // Interactive mode
            println!("🐈 gasket interactive mode. Type '/help' for commands, '/exit' to quit.\n");

            let interactive_session =
                SessionKey::new(gasket_channels::ChannelType::Cli, "interactive");

            // Build the slash-command dispatcher: built-ins, user YAML files,
            // help snapshot. The CLI is the only path through this dispatcher;
            // bot channels (Telegram, Discord, Slack) keep their existing
            // passthrough behavior — they never see this code.
            let host = Arc::new(CliCommandHost::new(agent.clone(), Some(broker.clone())));
            let help_snap = shared_help_snapshot();
            let user_dir = dirs::home_dir().map(|h| h.join(".gasket/commands"));

            let mut builder = DispatcherBuilder::new()
                .host(host)
                .help_snapshot(help_snap.clone())
                .register_builtin(exit())
                .register_builtin(clear())
                .register_builtin(help(help_snap.clone()))
                .register_builtin(builtin_new())
                .register_builtin(sessions())
                .register_builtin(model());
            if let Some(p) = user_dir {
                builder = builder.user_dir(p);
            }
            // Register all tools (including plugins) as slash commands
            builder = super::plugin_commands::register_tool_commands(
                builder,
                agent.tools(),
                Some(subagent_spawner_for_commands.clone()),
                Some(broker.clone()),
            );
            let dispatcher = builder
                .build()
                .await
                .context("failed to build slash-command dispatcher")?;

            let completer = CommandCompleter::from_dispatcher(&dispatcher);
            let mut line_editor = Reedline::create().with_completer(Box::new(completer));
            let prompt =
                DefaultPrompt::new(DefaultPromptSegment::Empty, DefaultPromptSegment::Empty);

            loop {
                match line_editor.read_line(&prompt) {
                    Ok(Signal::Success(line)) => {
                        let line = line.trim();

                        if line.is_empty() {
                            continue;
                        }

                        // Backward-compat: bare-word exit/quit terminators that
                        // existed before the dispatcher landed. Slash forms go
                        // through dispatcher's /exit alias chain instead.
                        if matches!(line, "exit" | "quit" | ":q") {
                            println!("Goodbye! 🐈");
                            break;
                        }

                        match dispatcher.route(line, &interactive_session).await {
                            RouteOutcome::Handled(CommandResult::Quit) => {
                                println!("Goodbye! 🐈");
                                break;
                            }
                            RouteOutcome::Handled(CommandResult::Print(s)) => {
                                println!("{}", s);
                                continue;
                            }
                            RouteOutcome::Handled(CommandResult::Error(s)) => {
                                eprintln!("{}", s.red());
                                continue;
                            }
                            RouteOutcome::Rewrite {
                                prompt: rewritten,
                                tool_filter,
                            } => {
                                run_llm_input(
                                    &agent,
                                    &interactive_session,
                                    &rewritten,
                                    tool_filter,
                                    use_streaming,
                                    render_md,
                                )
                                .await;
                                continue;
                            }
                            RouteOutcome::Passthrough(text) => {
                                run_llm_input(
                                    &agent,
                                    &interactive_session,
                                    &text,
                                    None,
                                    use_streaming,
                                    render_md,
                                )
                                .await;
                                continue;
                            }
                        }
                    }
                    Ok(Signal::CtrlC) | Ok(Signal::CtrlD) => {
                        println!("\nGoodbye! 🐈");
                        break;
                    }
                    Err(e) => {
                        println!("Error: {}", e);
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Send a line of text (passthrough or rewritten) to the LLM and render
/// the response. Errors are printed inline; this function never propagates
/// them to keep the REPL responsive.
async fn run_llm_input(
    agent: &AgentSession,
    session_key: &SessionKey,
    text: &str,
    tool_filter: Option<Vec<String>>,
    use_streaming: bool,
    render_md: bool,
) {
    println!();
    use gasket_engine::session::HandleOutcome;
    let outcome = agent.handle_inbound(text, session_key, tool_filter).await;

    match outcome {
        Ok(HandleOutcome::Consumed) => {
            println!();
        }
        Ok(HandleOutcome::Replied {
            events: mut event_rx,
            result: result_handle,
        }) => {
            if use_streaming {
                let forward_handle = tokio::spawn(async move {
                    while let Some(event) = event_rx.recv().await {
                        match event {
                            ChatEvent::Content { content } => {
                                print!("{}", content);
                                std::io::stdout().flush().ok();
                            }
                            ChatEvent::Thinking { content } => {
                                eprint!("{}", content.dimmed().italic());
                                std::io::stderr().flush().ok();
                            }
                            ChatEvent::Done => {}
                            _ => {}
                        }
                    }
                });

                let (result, _) = tokio::join!(result_handle, forward_handle);
                if result.is_ok() {
                    println!("\n");
                } else if let Err(e) = result {
                    println!("\n{} {}\n", "Error:".red(), e);
                }
            } else {
                // Drain events in non-streaming mode; only the final result matters.
                tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
                match result_handle.await {
                    Ok(Ok(resp)) => {
                        print_response_with_reasoning(&resp, render_md);
                        println!();
                    }
                    Ok(Err(e)) => println!("\n{} {}\n", "Error:".red(), e),
                    Err(e) => println!("\n{} {}\n", "Error:".red(), e),
                }
            }
        }
        Err(e) => {
            println!("\n{} {}\n", "Error:".red(), e);
        }
    }
}

/// Print response with optional Markdown rendering
fn print_response(response: &str, render_md: bool) {
    #[cfg(feature = "markdown")]
    if render_md {
        use termimad::MadSkin;
        let skin = MadSkin::default();
        skin.print_text(response);
        return;
    }

    // Fallback to plain text
    println!("{}", response);
}

/// Print reasoning content in a styled block
fn print_reasoning_block(reasoning: &str) {
    // Print a header with dimmed color and box drawing
    println!(
        "{}",
        "┌─ Thinking ─────────────────────────────────".dimmed()
    );

    // Print reasoning content with dimmed/italic style
    // Split by lines to handle multi-line reasoning
    for line in reasoning.lines() {
        println!("│ {}", line.dimmed().italic());
    }

    // Print footer
    println!(
        "{}",
        "└─────────────────────────────────────────────".dimmed()
    );
}

/// Print response with optional reasoning content and Markdown rendering
fn print_response_with_reasoning(response: &AgentResponse, render_md: bool) {
    // Print reasoning content first (if present) with special styling
    if let Some(ref reasoning) = response.reasoning_content {
        if !reasoning.is_empty() {
            print_reasoning_block(reasoning);
            println!(); // Add blank line between reasoning and main response
        }
    }

    // Print main response content
    print_response(&response.content, render_md);
}
