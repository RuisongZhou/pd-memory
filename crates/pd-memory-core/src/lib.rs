//! `pd-memory-core` — file-native, auditable, project-scoped long-term memory.
//!
//! This crate is a Rust reimplementation of the PilotDeck / ClawXMemory memory
//! design (behavioral reference). The design invariants are:
//!
//! 1. **Markdown files are the system of record.** SQLite (or JSONL) is only a
//!    control plane for the L0 capture queue and pipeline state.
//! 2. **Writes are asynchronous/batched.** `capture_turn` only appends to the
//!    queue; an LLM-backed heartbeat pipeline later extracts memory notes.
//! 3. **Reads are model-guided.** The model gates recall, then selects files
//!    from the manifest for PilotDeck's four-section system context.
//! 4. **Consolidation is reversible.** Dream runs on a staged copy and commits
//!    with a snapshot that can be rolled back.
//! 5. **No implicit host coupling.** Every path, model, clock and telemetry
//!    dependency is injected through [`ports`].
//!
//! Module ownership (for parallel work):
//! - Lead: `lib`, `types`, `ports`, `config`, `error`
//! - dev A: `message`, `frontmatter`, `files`, `manifest`, `control_plane`, `redact`
//! - dev B: `extraction`, `indexing`, `recall`, `dream`, `scheduler`, `engine`

pub mod config;
pub mod error;
pub mod ports;
pub mod types;

// dev A — storage / capture primitives
pub mod control_plane;
pub mod files;
pub mod frontmatter;
pub mod manifest;
pub mod message;
pub mod redact;

// dev B — pipeline / engine
pub mod dream;
pub mod engine;
pub mod extraction;
pub mod indexing;
pub mod recall;
pub mod scheduler;

pub use config::{ApiType, MemoryConfig, ModelConfig};
pub use error::{MemoryError, Result};
pub use ports::{
    Clock, CompletionRequest, CompletionResponse, MemoryStore, ModelProvider, QueueStore, Redactor,
    StateStore, StoreLayout, StoreRoots, SystemClock, Telemetry,
};
pub use types::*;

/// Crate version, surfaced by the CLI and MCP `initialize` response.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
