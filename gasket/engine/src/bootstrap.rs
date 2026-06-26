//! Bootstrap helpers for embedding the engine in a host process.
//!
//! Per Linus review: the CLI was copy-pasting 6× the same six-line
//! "create broker → open store → init globals" dance. Extracted here so the
//! call site shrinks to one line, and any future host (GUI, daemon, RPC)
//! gets the same initialization order for free.
//!
//! The helper deliberately stops short of unlocking the vault: vault unlock
//! is interactive (CLI prompts vs WebSocket handshake vs OAuth flow) and
//! belongs in the host crate, not in a generic bootstrap.

use std::sync::Arc;

use anyhow::Result;
use gasket_broker::MemoryBroker;
use gasket_storage::JsonStore;

use crate::config::{app_config::Config, init_config, load_config};

/// Shared infrastructure handed to every CLI command (and other engine hosts).
///
/// Owns:
/// - the parsed config (already pushed into the global slot via `init_config`);
/// - a topic broker;
/// - a `JsonStore` wrapped in an `Arc`, the sole storage backend.
pub struct EngineInfra {
    pub config: Config,
    pub broker: Arc<MemoryBroker>,
    pub store: Arc<JsonStore>,
}

impl EngineInfra {
    /// Build a fresh `Arc<dyn EventStoreTrait>` backed by the shared store.
    pub fn event_store(&self) -> Arc<dyn gasket_storage::EventStoreTrait> {
        self.store.clone()
    }

    /// Build a fresh `Arc<dyn SessionStoreTrait>` backed by the shared store.
    pub fn session_store(&self) -> Arc<dyn gasket_storage::SessionStoreTrait> {
        self.store.clone()
    }
}

/// Channel-capacity hint passed to [`init_engine_infra`].
///
/// Different hosts want different capacities — the CLI agent uses small
/// buffers (it's interactive and serial-per-session), the gateway uses larger
/// ones (it fans messages out across many remote channels).
#[derive(Debug, Clone, Copy)]
pub struct BrokerCapacity {
    pub p2p: usize,
    pub broadcast: usize,
}

impl BrokerCapacity {
    /// Capacity profile used by `cmd_agent` — small buffers, interactive REPL.
    pub const fn agent_repl() -> Self {
        Self {
            p2p: 256,
            broadcast: 64,
        }
    }

    /// Capacity profile used by `cmd_gateway` — sized for multi-channel fan-out.
    pub const fn gateway() -> Self {
        Self {
            p2p: 1024,
            broadcast: 256,
        }
    }
}

/// Initialize the shared infrastructure: load config → init globals →
/// create broker → open JsonStore → wrap in `Arc`. Returns [`EngineInfra`].
///
/// Fails fast if config cannot be loaded. The caller is expected to handle
/// vault unlock and provider resolution on top of the returned handle.
pub async fn init_engine_infra(capacity: BrokerCapacity) -> Result<EngineInfra> {
    let config = load_config().await?;
    init_config(config.clone());

    let broker = Arc::new(MemoryBroker::new(capacity.p2p, capacity.broadcast));

    let store = Arc::new(JsonStore::new(gasket_storage::config_dir()));

    Ok(EngineInfra {
        config,
        broker,
        store,
    })
}
