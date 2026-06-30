//! JSON file-backed storage and history processing for gasket.
//!
//! This crate provides:
//! - **Persistence:** Sessions, conversation messages, summaries, cron jobs
//!   (all via [`JsonStore`] under `~/.gasket`)
//! - **History:** Token-budget-aware history truncation and multi-dimensional retrieval

pub mod fs;
mod json_state;
mod json_store;
mod store_trait;

// ── Merged from gasket-history ──
pub mod processor;

use std::path::PathBuf;

pub use json_state::CronStateFile;
pub use json_store::JsonStore;
pub use store_trait::{EventStoreTrait, SessionStoreTrait, StoreError};

// ── History re-exports ──
pub use processor::{count_tokens, process_history, HistoryConfig, ProcessedHistory};

pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gasket")
}
