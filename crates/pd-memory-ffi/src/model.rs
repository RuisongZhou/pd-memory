//! Model-provider construction for the FFI layer.
//!
//! The engine never builds its own model client ([`pd_memory_core::ports`]);
//! host adapters inject one. For the C ABI the only input is the JSON config, so
//! this module maps [`ModelConfig`] onto a provider:
//!
//! * no model configured -> [`UnavailableModelProvider`], so capture / list /
//!   get / overview / export / settings keep working offline (the hook-style and
//!   Android capture-only deployments);
//! * model configured + `http-model` feature -> [`pd_memory_llm::provider_from_config`];
//! * model configured without the feature -> a clear `unsupported` error instead
//!   of a silent no-op, because the caller explicitly asked for LLM behaviour.

use std::sync::Arc;

use pd_memory_core::config::ModelConfig;
use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::ports::{CompletionRequest, CompletionResponse, ModelProvider};

/// Provider used when the config does not configure a model.
///
/// Everything that does not need an LLM (capture, list, get, overview, export,
/// settings, state) works; model-dependent operations (retrieve, flush, dream)
/// fail with [`MemoryError::Model`] whose message tells the host how to fix the
/// configuration.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct UnavailableModelProvider;

impl ModelProvider for UnavailableModelProvider {
    fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        Err(MemoryError::Model(format!(
            "no model provider is configured (schema `{}`): set `model.model` (and `model.base_url` / \
             `model.api_key`) in the engine config JSON and build pd-memory-ffi with the `http-model` feature",
            request.schema_name
        )))
    }
}

/// True when the config explicitly asks for a remote model.
pub(crate) fn model_is_configured(config: &ModelConfig) -> bool {
    let model = config.model.trim();
    let base_url = config
        .base_url
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    !model.is_empty() || !base_url.is_empty()
}

/// Build the model provider for an engine created through the FFI.
pub(crate) fn build_model(config: &ModelConfig) -> Result<Arc<dyn ModelProvider>> {
    if !model_is_configured(config) {
        log::debug!("pd-memory-ffi: no model configured, using UnavailableModelProvider");
        return Ok(Arc::new(UnavailableModelProvider));
    }

    #[cfg(feature = "http-model")]
    {
        pd_memory_llm::provider_from_config(config)
    }

    #[cfg(not(feature = "http-model"))]
    {
        let _ = config;
        Err(MemoryError::Unsupported(
            "a model is configured but this pd-memory-ffi build does not include the HTTP model \
             providers; rebuild with `--features http-model` (Android: see android/README.md)"
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_model_config_uses_fallback_provider() {
        let config = ModelConfig::default();
        assert!(!model_is_configured(&config));
        let provider = build_model(&config).expect("fallback provider");
        let error = provider
            .complete(CompletionRequest {
                system: String::new(),
                user: String::new(),
                schema_name: "memory_classify".into(),
                timeout: std::time::Duration::from_secs(1),
                max_output_tokens: None,
                temperature: None,
            })
            .expect_err("fallback provider must not invent completions");
        assert_eq!(error.code(), "model");
        assert!(error
            .to_string()
            .contains("no model provider is configured"));
    }

    /// Without the `http-model` feature a configured model must fail loudly: the
    /// host asked for LLM behaviour that this build does not contain.
    #[cfg(not(feature = "http-model"))]
    #[test]
    fn configured_model_needs_http_model_feature() {
        let config = ModelConfig {
            model: "gpt-4o-mini".into(),
            base_url: Some("https://example.invalid/v1".into()),
            ..Default::default()
        };
        assert!(model_is_configured(&config));
        let error = match build_model(&config) {
            Ok(_) => panic!("without http-model a configured model is rejected"),
            Err(error) => error,
        };
        assert_eq!(error.code(), "unsupported");
        assert!(error.to_string().contains("http-model"));
    }
}
