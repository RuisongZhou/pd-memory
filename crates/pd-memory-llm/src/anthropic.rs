//! Anthropic `/v1/messages` provider.
//!
//! Uses the Messages API with `x-api-key` + `anthropic-version` headers. JSON
//! mode is expressed through the prompt (the engine prompts end with an
//! explicit JSON-shape instruction), so no `response_format` field is sent.

use std::sync::Arc;

use pd_memory_core::config::ModelConfig;
use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::ports::{CompletionRequest, CompletionResponse, ModelProvider};
use serde_json::json;

use crate::http_transport::{HttpRequest, HttpTransport, UreqTransport};
use crate::retry::{run_with_retries, AttemptOutcome, Sleeper, SystemSleeper};
use crate::shared;

const PROVIDER_NAME: &str = "anthropic";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const DEFAULT_TEMPERATURE: f32 = 0.2;

/// Blocking provider for the Anthropic Messages API.
///
/// Construction requires an API key (literal `api_key` or via `api_key_env`);
/// the key is held in memory only and never logged or embedded in errors.
pub struct AnthropicProvider {
    config: ModelConfig,
    api_key: String,
    transport: Arc<dyn HttpTransport>,
    sleeper: Arc<dyn Sleeper>,
}

impl AnthropicProvider {
    /// Build a provider with the default (ureq/rustls) transport.
    pub fn new(config: ModelConfig) -> Result<Self> {
        Self::with_parts(config, Arc::new(UreqTransport), Arc::new(SystemSleeper))
    }

    /// Build a provider with injected transport and sleeper (tests, embedding).
    pub(crate) fn with_parts(
        config: ModelConfig,
        transport: Arc<dyn HttpTransport>,
        sleeper: Arc<dyn Sleeper>,
    ) -> Result<Self> {
        let model = config.model.trim();
        if model.is_empty() {
            return Err(MemoryError::InvalidConfig(
                "model.model must be set for the anthropic provider".into(),
            ));
        }
        let api_key = config.resolve_api_key().ok_or_else(|| {
            MemoryError::InvalidConfig(
                "anthropic provider requires model.api_key or model.api_key_env".into(),
            )
        })?;
        Ok(Self {
            api_key,
            config,
            transport,
            sleeper,
        })
    }

    /// Resolved messages endpoint.
    pub fn endpoint(&self) -> String {
        shared::anthropic_endpoint(self.config.base_url.as_deref())
    }

    /// Build the HTTP request for one completion. Pure and unit-tested.
    pub(crate) fn build_http_request(&self, request: &CompletionRequest) -> HttpRequest {
        let max_tokens = request
            .max_output_tokens
            .or(self.config.max_output_tokens)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let temperature = request.temperature.unwrap_or(DEFAULT_TEMPERATURE);
        let body = json!({
            "model": self.config.model,
            "max_tokens": max_tokens,
            "system": request.system,
            "messages": [
                {"role": "user", "content": request.user},
            ],
            "temperature": temperature,
        });
        let defaults: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (
                "User-Agent".to_string(),
                format!("pd-memory/{}", pd_memory_core::VERSION),
            ),
            ("Accept".to_string(), "application/json".to_string()),
            ("x-api-key".to_string(), self.api_key.clone()),
            (
                "anthropic-version".to_string(),
                shared::ANTHROPIC_VERSION.to_string(),
            ),
        ];
        HttpRequest {
            url: self.endpoint(),
            headers: shared::merged_headers(&self.config, defaults),
            body,
            timeout: shared::request_timeout(request, &self.config),
        }
    }

    /// Parse a success response body (first `text` content block). Pure.
    pub(crate) fn parse_response(body: &str) -> Result<CompletionResponse> {
        let value: serde_json::Value = serde_json::from_str(body).map_err(|error| {
            shared::invalid_response(PROVIDER_NAME, format!("body is not JSON: {error}"))
        })?;
        let content = value
            .get("content")
            .and_then(serde_json::Value::as_array)
            .and_then(|blocks| {
                blocks.iter().find_map(|block| {
                    if block.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                        block
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    } else {
                        None
                    }
                })
            })
            .ok_or_else(|| {
                shared::invalid_response(PROVIDER_NAME, "missing content[].text block")
            })?;
        if content.trim().is_empty() {
            return Err(shared::invalid_response(
                PROVIDER_NAME,
                "content[].text is empty",
            ));
        }
        Ok(CompletionResponse {
            text: content,
            usage: shared::usage_from_value(value.get("usage")),
        })
    }

    fn attempt(&self, request: &CompletionRequest) -> AttemptOutcome {
        let http_request = self.build_http_request(request);
        let api_key = Some(self.api_key.as_str());
        match self.transport.post_json(&http_request) {
            Ok(response) => {
                if (200..300).contains(&response.status) {
                    match Self::parse_response(&response.body) {
                        Ok(parsed) => AttemptOutcome::Success(parsed),
                        Err(error) => AttemptOutcome::Fatal(error),
                    }
                } else {
                    let error = shared::http_status_error(
                        PROVIDER_NAME,
                        response.status,
                        &response.body,
                        api_key,
                    );
                    if crate::retry::retryable_status(response.status) {
                        AttemptOutcome::Retryable(error)
                    } else {
                        AttemptOutcome::Fatal(error)
                    }
                }
            }
            Err(error) => {
                AttemptOutcome::Retryable(shared::transport_error(PROVIDER_NAME, error, api_key))
            }
        }
    }
}

impl ModelProvider for AnthropicProvider {
    fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        run_with_retries(self.config.max_retries, self.sleeper.as_ref(), |_attempt| {
            self.attempt(&request)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeTransport, NoopSleeper, RetryRecordingSleeper};
    use std::time::Duration;

    fn config() -> ModelConfig {
        ModelConfig {
            api_type: pd_memory_core::config::ApiType::Anthropic,
            model: "claude-3-5-sonnet-latest".into(),
            base_url: Some("https://api.example.com".into()),
            api_key: Some("sk-ant-test".into()),
            max_retries: 2,
            ..Default::default()
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "You are a memory extractor.".into(),
            user: "remember: I prefer Chinese replies".into(),
            schema_name: "memory_create_feedback".into(),
            timeout: Duration::from_secs(30),
            max_output_tokens: Some(1024),
            temperature: None,
        }
    }

    fn provider(transport: Arc<FakeTransport>) -> AnthropicProvider {
        AnthropicProvider::with_parts(config(), transport, Arc::new(NoopSleeper))
            .expect("provider builds")
    }

    #[test]
    fn request_body_and_headers_match_anthropic_contract() {
        let transport = Arc::new(FakeTransport::default());
        let provider = provider(transport);
        let http = provider.build_http_request(&request());
        assert_eq!(http.url, "https://api.example.com/v1/messages");
        assert_eq!(http.body["model"], "claude-3-5-sonnet-latest");
        assert_eq!(http.body["max_tokens"], 1024);
        assert_eq!(http.body["system"], "You are a memory extractor.");
        assert_eq!(http.body["messages"][0]["role"], "user");
        assert_eq!(
            http.body["messages"][0]["content"],
            "remember: I prefer Chinese replies"
        );
        assert!((http.body["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        let header = |name: &str| {
            http.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("x-api-key"), Some("sk-ant-test"));
        assert_eq!(header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(header("content-type"), Some("application/json"));
        assert!(!http.body.to_string().contains("sk-ant-test"));
    }

    #[test]
    fn parses_text_content_block_with_usage() {
        let response = AnthropicProvider::parse_response(
            r#"{"content":[{"type":"text","text":"{\"ok\":true}"}],
                "usage":{"input_tokens":5,"output_tokens":9}}"#,
        )
        .expect("parses");
        assert_eq!(response.text, "{\"ok\":true}");
        assert_eq!(response.usage.unwrap().output_tokens, Some(9));
    }

    #[test]
    fn malformed_response_is_rejected() {
        assert!(matches!(
            AnthropicProvider::parse_response(r#"{"content":[]}"#),
            Err(MemoryError::Model(_))
        ));
        assert!(matches!(
            AnthropicProvider::parse_response("{oops"),
            Err(MemoryError::Model(_))
        ));
    }

    #[test]
    fn requires_api_key() {
        let mut config = config();
        config.api_key = None;
        config.api_key_env = None;
        let result = AnthropicProvider::with_parts(
            config,
            Arc::new(FakeTransport::default()),
            Arc::new(NoopSleeper),
        );
        assert!(matches!(result, Err(MemoryError::InvalidConfig(_))));
    }

    #[test]
    fn retryable_statuses_retry_exactly_max_retries_times() {
        let transport = Arc::new(FakeTransport::default());
        for _ in 0..3 {
            transport.push_http(429, r#"{"error":{"type":"rate_limit"}}"#);
        }
        let sleeper = Arc::new(RetryRecordingSleeper::default());
        let provider = AnthropicProvider::with_parts(config(), transport.clone(), sleeper.clone())
            .expect("builds");
        let error = provider.complete(request()).unwrap_err();
        assert!(format!("{error}").contains("429"));
        assert_eq!(transport.request_count(), 3);
        assert_eq!(
            sleeper.delays(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
    }

    #[test]
    fn non_retryable_status_fails_fast() {
        let transport = Arc::new(FakeTransport::default());
        transport.push_http(400, r#"{"error":{"message":"bad request"}}"#);
        let provider = provider(transport.clone());
        assert!(provider.complete(request()).is_err());
        assert_eq!(transport.request_count(), 1);
    }

    #[test]
    fn api_key_is_redacted_even_if_a_proxy_echoes_it() {
        let transport = Arc::new(FakeTransport::default());
        transport.push_http(401, r#"{"error":"bad key sk-ant-test"}"#);
        let provider = provider(transport);
        let message = format!("{}", provider.complete(request()).unwrap_err());
        assert!(!message.contains("sk-ant-test"), "leaked: {message}");
        assert!(message.contains("[REDACTED]"));
    }
}
