//! Deterministic test doubles for `pd-memory` (shared by all crates).
//!
//! [`ScriptedModelProvider`] returns queued responses matched by
//! `schema_name`, with a deterministic fallback that behaves like a minimal
//! extraction model. This lets pipeline tests run without network access.

use std::collections::VecDeque;
use std::sync::Mutex;

use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::ports::{CompletionRequest, CompletionResponse, ModelProvider};

/// A [`ModelProvider`] that returns scripted responses in FIFO order.
#[derive(Default)]
pub struct ScriptedModelProvider {
    responses: Mutex<VecDeque<(Option<String>, String)>>,
    pub log: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedModelProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a response; `schema` filters which request consumes it (`None` = any).
    pub fn push(&self, schema: Option<&str>, response: impl Into<String>) {
        self.responses
            .lock()
            .unwrap()
            .push_back((schema.map(str::to_string), response.into()));
    }

    pub fn request_count(&self) -> usize {
        self.log.lock().unwrap().len()
    }
}

impl ModelProvider for ScriptedModelProvider {
    fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        self.log.lock().unwrap().push(request.clone());
        let mut responses = self.responses.lock().unwrap();
        if let Some(index) = responses.iter().position(|(schema, _)| {
            schema
                .as_deref()
                .map_or(true, |value| value == request.schema_name)
        }) {
            let (_, text) = responses.remove(index).unwrap();
            return Ok(CompletionResponse { text, usage: None });
        }
        Err(MemoryError::Model(format!(
            "no scripted response for schema {}",
            request.schema_name
        )))
    }
}

/// Build a temporary memory data root for tests.
pub fn temp_data_root() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}
