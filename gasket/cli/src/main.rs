//! gasket CLI

use anyhow::Result;
use clap::Parser;
#[cfg(feature = "telemetry")]
use opentelemetry::trace::TracerProvider;
#[cfg(feature = "telemetry")]
use opentelemetry_otlp::WithExportConfig;
use rustls::crypto::ring::default_provider;
#[cfg(feature = "telemetry")]
use tracing::info;
use tracing_subscriber::EnvFilter;

mod cli;
mod commands;
mod provider;

use cli::{
    AuthCommands, ChannelsCommands, Cli, Commands, CronCommands, EmbeddingCommands, ToolCommands,
    VaultCommands,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls CryptoProvider (required for rustls 0.23+)
    default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Parse CLI first so we can choose the right logging destination
    let cli = Cli::parse();

    let log_to_file =
        std::env::var("GASKET_LOG_FILE").is_ok_and(|v| !v.is_empty() && v != "false" && v != "0");

    // Initialize logging and OpenTelemetry
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,tantivy::directory::managed_directory=warn,html5ever=error")
    });

    // Try to initialize OpenTelemetry, fall back to plain logging if unavailable
    #[cfg(feature = "telemetry")]
    let otel_initialized = init_telemetry(env_filter.clone(), log_to_file);
    #[cfg(not(feature = "telemetry"))]
    let otel_initialized = false;

    if !otel_initialized {
        if log_to_file {
            init_file_logging(env_filter);
        } else {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_ansi(true)
                .init();
        }
    }

    match cli.command {
        Some(Commands::Onboard) => commands::cmd_onboard().await,
        Some(Commands::Status) => commands::cmd_status().await,
        Some(Commands::Agent(opts)) => commands::cmd_agent(opts).await,
        Some(Commands::Gateway) => commands::cmd_gateway().await,
        Some(Commands::Channels { command }) => match command {
            ChannelsCommands::Status => commands::cmd_channels_status().await,
        },
        Some(Commands::Auth { command }) => match command {
            AuthCommands::Copilot { pat, client_id } => {
                commands::cmd_auth_copilot(pat, client_id).await
            }
            AuthCommands::Status => commands::cmd_auth_status().await,
        },
        Some(Commands::Cron { command }) => match command {
            CronCommands::List => commands::cmd_cron_list().await,
            CronCommands::Add {
                name,
                cron,
                message,
            } => commands::cmd_cron_add(name, cron, message).await,
            CronCommands::Remove { id } => commands::cmd_cron_remove(id).await,
            CronCommands::Show { id } => commands::cmd_cron_show(id).await,
            CronCommands::Enable { id } => commands::cmd_cron_enable(id).await,
            CronCommands::Disable { id } => commands::cmd_cron_disable(id).await,
            CronCommands::Refresh => commands::cmd_cron_refresh().await,
        },
        Some(Commands::Stats) => commands::cmd_stats().await,
        Some(Commands::Vault { command }) => match command {
            VaultCommands::List => commands::cmd_vault_list().await,
            VaultCommands::Set {
                key,
                value,
                description,
            } => commands::cmd_vault_set(key, value, description).await,
            VaultCommands::Get { key } => commands::cmd_vault_get(key).await,
            VaultCommands::Delete { key, force } => commands::cmd_vault_delete(key, force).await,
            VaultCommands::Show { key, show_value } => {
                commands::cmd_vault_show(key, show_value).await
            }
            VaultCommands::Import { file, merge } => commands::cmd_vault_import(file, merge).await,
            VaultCommands::Export { file } => commands::cmd_vault_export(file).await,
            VaultCommands::Rekey => commands::cmd_vault_rekey().await,
        },
        Some(Commands::Tool { command }) => match command {
            ToolCommands::Execute { name, args } => commands::cmd_tool_execute(name, args).await,
        },
        Some(Commands::Embedding { command }) => match command {
            EmbeddingCommands::Rebuild { limit } => commands::cmd_embedding_rebuild(limit).await,
            EmbeddingCommands::Stats => commands::cmd_embedding_stats().await,
        },
        None => {
            // No command - show help
            println!("🐈 gasket v2.0.0 - A lightweight AI assistant\n");
            println!("Usage: gasket <COMMAND>\n");
            println!("Commands:");
            println!("  onboard   Initialize configuration");
            println!("  status    Show status");
            println!("  agent     Chat with the agent (REPL)");
            println!("  channels  Manage chat channels");
            println!("  gateway   Start the gateway");
            println!("  auth      Authentication commands");
            println!("  cron      Manage scheduled tasks");
            println!("  stats     Show session token usage and cost statistics");
            println!("  tool       Execute tools directly");
            println!("  vault      Manage vault secrets");
            println!("  embedding  Manage embedding index and history recall\n");
            println!("Run 'gasket --help' for more information.");
            Ok(())
        }
    }
}

/// Initialize OpenTelemetry tracing (optional).
///
/// Only initializes OpenTelemetry when explicitly configured via environment
/// variables. Defaults to no exporter (logging only).
///
/// Environment variables:
/// - `OTEL_EXPORTER_OTLP_ENDPOINT`: OTLP endpoint URL (e.g., http://localhost:4317)
/// - `OTEL_SDK_DISABLED=true`: Disable OpenTelemetry completely
/// Initialize file-based logging (avoids polluting the terminal).
fn init_file_logging(env_filter: EnvFilter) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let log_dir = dirs::home_dir()
        .map(|p| p.join(".gasket").join("logs"))
        .unwrap_or_else(|| std::path::PathBuf::from(".gasket/logs"));

    std::fs::create_dir_all(&log_dir).ok();

    let file_appender = tracing_appender::rolling::daily(log_dir, "gasket.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    // Leak the guard so the background thread lives for the process lifetime.
    // This is fine for a CLI binary.
    std::mem::forget(_guard);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(non_blocking))
        .init();
}

#[cfg(feature = "telemetry")]
fn init_telemetry(env_filter: EnvFilter, log_to_file: bool) -> bool {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // Check if OTEL is disabled
    if std::env::var("OTEL_SDK_DISABLED").is_ok_and(|v| v == "true") {
        return false;
    }

    // Only initialize if endpoint is explicitly configured
    let endpoint = match std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        Ok(e) => e,
        Err(_) => return false, // No endpoint configured, skip OpenTelemetry
    };

    // Try to create OTLP exporter
    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{}/v1/traces", endpoint))
        .build()
    {
        Ok(e) => e,
        Err(_) => return false,
    };

    // Create tracer provider
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();

    let tracer = provider.tracer("gasket");

    // Set global tracer provider
    opentelemetry::global::set_tracer_provider(provider);

    // Create tracing layer and initialize
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(otel_layer);

    if log_to_file {
        let log_dir = dirs::home_dir()
            .map(|p| p.join(".gasket").join("logs"))
            .unwrap_or_else(|| std::path::PathBuf::from(".gasket/logs"));
        std::fs::create_dir_all(&log_dir).ok();
        let file_appender = tracing_appender::rolling::daily(log_dir, "gasket.log");
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
        std::mem::forget(_guard);
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(non_blocking))
            .init();
    } else {
        registry.with(tracing_subscriber::fmt::layer()).init();
    }

    info!("OpenTelemetry tracing enabled: {}", endpoint);
    true
}
