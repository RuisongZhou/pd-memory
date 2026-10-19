//! Error types shared by every `pd-memory` crate.

use std::path::PathBuf;

/// Convenient result alias used across the SDK.
pub type Result<T, E = MemoryError> = std::result::Result<T, E>;

/// The single error enum of the memory SDK.
///
/// Variants are intentionally coarse so that FFI/MCP layers can map them to a
/// small set of stable error codes (`invalid_config`, `io`, `model`, ...).
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid on-disk data at {path}: {message}")]
    Corrupt { path: PathBuf, message: String },

    #[error("model provider error: {0}")]
    Model(String),

    #[error("model output could not be parsed ({schema}): {message}")]
    ModelOutput { schema: String, message: String },

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("path is outside the memory root: {0}")]
    PathEscape(String),

    #[error("memory entry not found: {0}")]
    NotFound(String),

    #[error("operation is not supported in this build: {0}")]
    Unsupported(String),

    #[error("store is locked by another writer: {0}")]
    Locked(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("{0}")]
    Other(String),
}

impl MemoryError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        MemoryError::Io {
            path: path.into(),
            source,
        }
    }

    /// Stable machine-readable code for FFI / MCP error payloads.
    pub fn code(&self) -> &'static str {
        match self {
            MemoryError::InvalidConfig(_) => "invalid_config",
            MemoryError::Io { .. } => "io",
            MemoryError::Corrupt { .. } => "corrupt_data",
            MemoryError::Model(_) => "model",
            MemoryError::ModelOutput { .. } => "model_output",
            MemoryError::Json(_) => "json",
            MemoryError::PathEscape(_) => "path_escape",
            MemoryError::NotFound(_) => "not_found",
            MemoryError::Unsupported(_) => "unsupported",
            MemoryError::Locked(_) => "locked",
            MemoryError::InvalidInput(_) => "invalid_input",
            MemoryError::Other(_) => "other",
        }
    }
}
