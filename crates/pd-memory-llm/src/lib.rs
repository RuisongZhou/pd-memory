//! HTTP model providers for `pd-memory`.
//!
//! Blocking, no async runtime, no globals: adapters implement
//! [`pd_memory_core::ports::ModelProvider`] and are constructed explicitly by
//! host adapters (CLI / MCP / hooks / Android JNI).
//!
//! - [`OpenAiChatProvider`] — OpenAI-compatible `/chat/completions` (JSON mode)
//! - [`AnthropicProvider`] — Anthropic `/v1/messages`
//! - [`MockProvider`] — deterministic, offline, for tests and mock-mode E2E
//!
//! The engine sends JSON-mode prompts and validates the response itself, so
//! providers only return text plus optional token usage. Failures are mapped to
//! [`pd_memory_core::error::MemoryError::Model`] with the HTTP status and a
//! bounded body excerpt; API keys are never logged or embedded in errors.

mod anthropic;
mod http_transport;
mod mock;
mod openai;
mod retry;
mod shared;

#[cfg(test)]
mod test_support;

use std::sync::Arc;

pub use anthropic::AnthropicProvider;
pub use http_transport::{HttpRequest, HttpResponse, HttpTransport, TransportError, UreqTransport};
pub use mock::MockProvider;
pub use openai::OpenAiChatProvider;
pub use retry::{backoff_delay, retryable_status, Sleeper, SystemSleeper};

use pd_memory_core::config::{ApiType, ModelConfig};
use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::ports::ModelProvider;

/// Environment variable that switches every host adapter into offline mock mode.
pub const PROVIDER_ENV: &str = "PD_MEMORY_PROVIDER";

/// Whether `PD_MEMORY_PROVIDER=mock` is set in this process.
pub fn mock_selected_from_env() -> bool {
    std::env::var(PROVIDER_ENV)
        .map(|value| value.trim().eq_ignore_ascii_case("mock"))
        .unwrap_or(false)
}

/// Build a provider from configuration.
///
/// Dispatch rules:
/// - `PD_MEMORY_PROVIDER=mock` selects [`MockProvider`] regardless of API type
///   (offline tests / WorkBuddy mock E2E);
/// - [`ApiType::OpenaiChat`] -> [`OpenAiChatProvider`];
/// - [`ApiType::Anthropic`] -> [`AnthropicProvider`];
/// - `OpenaiResponses` / `Google` -> [`MemoryError::Unsupported`].
pub fn provider_from_config(config: &ModelConfig) -> Result<Arc<dyn ModelProvider>> {
    if mock_selected_from_env() {
        log::info!("PD_MEMORY_PROVIDER=mock: using deterministic MockProvider");
        return Ok(Arc::new(MockProvider::new()));
    }
    match config.api_type {
        ApiType::OpenaiChat => Ok(Arc::new(OpenAiChatProvider::new(config.clone())?)),
        ApiType::Anthropic => Ok(Arc::new(AnthropicProvider::new(config.clone())?)),
        other => Err(MemoryError::Unsupported(format!(
            "model api_type {other:?} is not implemented; supported: openai-chat, anthropic"
        ))),
    }
}

/// Validate that the configured API type is implemented by this build.
pub fn api_type_supported(api_type: ApiType) -> bool {
    matches!(api_type, ApiType::OpenaiChat | ApiType::Anthropic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn model_config(api_type: ApiType) -> ModelConfig {
        ModelConfig {
            api_type,
            model: "test-model".into(),
            api_key: Some("sk-unit-test".into()),
            ..Default::default()
        }
    }

    #[test]
    fn supported_api_types_match_dispatch() {
        assert!(api_type_supported(ApiType::OpenaiChat));
        assert!(api_type_supported(ApiType::Anthropic));
        assert!(!api_type_supported(ApiType::OpenaiResponses));
        assert!(!api_type_supported(ApiType::Google));
    }

    #[test]
    fn provider_from_config_dispatches_by_api_type() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(PROVIDER_ENV);
        assert!(provider_from_config(&model_config(ApiType::OpenaiChat)).is_ok());
        assert!(provider_from_config(&model_config(ApiType::Anthropic)).is_ok());
    }

    #[test]
    fn unsupported_api_types_are_rejected() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(PROVIDER_ENV);
        for api_type in [ApiType::OpenaiResponses, ApiType::Google] {
            let error = provider_from_config(&model_config(api_type))
                .err()
                .expect("unsupported api type must fail");
            assert!(matches!(error, MemoryError::Unsupported(_)), "{error}");
        }
    }

    #[test]
    fn mock_env_var_overrides_configured_provider() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(PROVIDER_ENV, "mock");
        let provider = provider_from_config(&model_config(ApiType::Google)).expect("mock selected");
        let response = provider
            .complete(pd_memory_core::ports::CompletionRequest {
                system: "system".into(),
                user: "{\"query\":\"hello\"}".into(),
                schema_name: "memory_gate".into(),
                timeout: std::time::Duration::from_secs(1),
                max_output_tokens: None,
                temperature: None,
            })
            .expect("mock completes");
        assert!(response.text.contains("\"route\""));
        std::env::remove_var(PROVIDER_ENV);
    }
}
