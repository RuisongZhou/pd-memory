//! OpenAI-compatible `/chat/completions` provider.
//!
//! Works with the OpenAI API and any compatible gateway (Azure-style proxies
//! through `base_url`, Ollama/LM Studio/vLLM local endpoints, ...). Requests
//! use JSON mode (`response_format: {"type":"json_object"}`) because every
//! engine prompt expects a single JSON object back.

use std::sync::Arc;

use pd_memory_core::config::ModelConfig;
use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::ports::{CompletionRequest, CompletionResponse, ModelProvider};
use serde_json::json;

use crate::http_transport::{HttpRequest, HttpResponse, HttpTransport, UreqTransport};
use crate::retry::{run_with_retries, AttemptOutcome, Sleeper, SystemSleeper};
use crate::shared;

const PROVIDER_NAME: &str = "openai-chat";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const DEFAULT_TEMPERATURE: f32 = 0.2;

/// Blocking provider for OpenAI-compatible chat completion APIs.
///
/// Construction validates the model name and resolves the API key once. The
/// key is stored in memory only and is never logged or embedded in errors.
pub struct OpenAiChatProvider {
    config: ModelConfig,
    api_key: Option<String>,
    transport: Arc<dyn HttpTransport>,
    sleeper: Arc<dyn Sleeper>,
}

impl OpenAiChatProvider {
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
                "model.model must be set for the openai-chat provider".into(),
            ));
        }
        let api_key = config.resolve_api_key();
        if api_key.is_none() {
            log::debug!(
                "openai-chat provider has no api key; sending unauthenticated requests \
                 (fine for local OpenAI-compatible servers)"
            );
        }
        Ok(Self {
            api_key,
            config,
            transport,
            sleeper,
        })
    }

    /// Resolved chat completions endpoint.
    pub fn endpoint(&self) -> String {
        shared::openai_endpoint(self.config.base_url.as_deref())
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
            "messages": [
                {"role": "system", "content": request.system},
                {"role": "user", "content": request.user},
            ],
            "temperature": temperature,
            "max_tokens": max_tokens,
            "response_format": {"type": "json_object"},
        });

        let mut defaults: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (
                "User-Agent".to_string(),
                format!("pd-memory/{}", pd_memory_core::VERSION),
            ),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        if let Some(key) = &self.api_key {
            defaults.push(("Authorization".to_string(), format!("Bearer {key}")));
        }
        let headers = shared::merged_headers(&self.config, defaults);
        HttpRequest {
            url: self.endpoint(),
            headers,
            body,
            timeout: shared::request_timeout(request, &self.config),
        }
    }

    /// Parse a success response body. Pure and unit-tested.
    pub(crate) fn parse_response(body: &str) -> Result<CompletionResponse> {
        let value: serde_json::Value = serde_json::from_str(body).map_err(|error| {
            shared::invalid_response(PROVIDER_NAME, format!("body is not JSON: {error}"))
        })?;
        let content = value
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(shared::content_value_to_text)
            .ok_or_else(|| {
                shared::invalid_response(PROVIDER_NAME, "missing choices[0].message.content")
            })?;
        if content.trim().is_empty() {
            return Err(shared::invalid_response(
                PROVIDER_NAME,
                "choices[0].message.content is empty",
            ));
        }
        Ok(CompletionResponse {
            text: content,
            usage: shared::usage_from_value(value.get("usage")),
        })
    }

    fn attempt(&self, http_request: &HttpRequest) -> AttemptOutcome<HttpResponse> {
        let api_key = self.api_key.as_deref();
        match self.transport.post_json(http_request) {
            Ok(response) => {
                if (200..300).contains(&response.status) {
                    AttemptOutcome::Success(response)
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

impl ModelProvider for OpenAiChatProvider {
    fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let mut http_request = self.build_http_request(&request);
        let response = run_with_retries(self.config.max_retries, self.sleeper.as_ref(), |_| {
            self.attempt(&http_request)
        })
        .or_else(|_| {
            http_request
                .body
                .as_object_mut()
                .map(|body| body.remove("response_format"));
            run_with_retries(self.config.max_retries, self.sleeper.as_ref(), |_| {
                self.attempt(&http_request)
            })
        })?;
        Self::parse_response(&response.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeTransport, NoopSleeper, RetryRecordingSleeper};
    use std::time::Duration;

    fn config(api_key: Option<&str>) -> ModelConfig {
        ModelConfig {
            model: "gpt-4o-mini".into(),
            base_url: Some("http://localhost:9999/v1".into()),
            api_key: api_key.map(|value| value.to_string()),
            max_retries: 2,
            ..Default::default()
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "You are a memory classifier.".into(),
            user: "focus: remember my timezone".into(),
            schema_name: "memory_classify".into(),
            timeout: Duration::from_millis(500),
            max_output_tokens: Some(512),
            temperature: Some(0.1),
        }
    }

    fn provider(transport: Arc<FakeTransport>) -> OpenAiChatProvider {
        OpenAiChatProvider::with_parts(config(Some("sk-test")), transport, Arc::new(NoopSleeper))
            .expect("provider builds")
    }

    #[test]
    fn request_body_has_required_fields() {
        let transport = Arc::new(FakeTransport::default());
        let provider = provider(transport);
        let http = provider.build_http_request(&request());
        assert_eq!(http.url, "http://localhost:9999/v1/chat/completions");
        assert_eq!(http.timeout, Duration::from_millis(500));
        let body = &http.body;
        assert_eq!(body["model"], "gpt-4o-mini");
        assert!((body["temperature"].as_f64().unwrap() - 0.1).abs() < 1e-6);
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["response_format"]["type"], "json_object");
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are a memory classifier.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "focus: remember my timezone");
        let auth = http
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str());
        assert_eq!(auth, Some("Bearer sk-test"));
        // The key must never appear in the body.
        assert!(!body.to_string().contains("sk-test"));
    }

    #[test]
    fn request_without_key_has_no_authorization_header() {
        let transport = Arc::new(FakeTransport::default());
        let provider =
            OpenAiChatProvider::with_parts(config(None), transport, Arc::new(NoopSleeper))
                .expect("provider builds");
        let http = provider.build_http_request(&request());
        assert!(!http
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization")));
    }

    #[test]
    fn successful_response_is_parsed_with_usage() {
        let response = OpenAiChatProvider::parse_response(
            r#"{"choices":[{"message":{"role":"assistant","content":"{\"should_store\":true}"}}],
                "usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
        )
        .expect("parses");
        assert_eq!(response.text, "{\"should_store\":true}");
        let usage = response.usage.expect("usage");
        assert_eq!(usage.input_tokens, Some(11));
        assert_eq!(usage.output_tokens, Some(7));
    }

    #[test]
    fn malformed_response_is_rejected() {
        let error = OpenAiChatProvider::parse_response(r#"{"choices":[]}"#).unwrap_err();
        assert!(matches!(error, MemoryError::Model(_)));
        let error = OpenAiChatProvider::parse_response("not json").unwrap_err();
        assert!(matches!(error, MemoryError::Model(_)));
    }

    #[test]
    fn retryable_status_is_retried_then_succeeds() {
        let transport = Arc::new(FakeTransport::default());
        transport.push_http(429, r#"{"error":"rate limited"}"#);
        transport.push_ok(r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#);
        let provider = OpenAiChatProvider::with_parts(
            config(Some("sk-test")),
            transport.clone(),
            Arc::new(NoopSleeper),
        )
        .expect("provider builds");
        let response = provider
            .complete(request())
            .expect("second attempt succeeds");
        assert_eq!(response.text, "{\"ok\":true}");
        assert_eq!(transport.request_count(), 2);
    }

    #[test]
    fn retries_are_bounded_and_error_mentions_status_without_key() {
        let transport = Arc::new(FakeTransport::default());
        for _ in 0..6 {
            transport.push_http(503, r#"{"error":"overloaded"}"#);
        }
        let sleeper = Arc::new(RetryRecordingSleeper::default());
        let provider = OpenAiChatProvider::with_parts(
            config(Some("sk-secret")),
            transport.clone(),
            sleeper.clone(),
        )
        .expect("provider builds");
        let error = provider.complete(request()).unwrap_err();
        let message = format!("{error}");
        assert!(message.contains("503"), "message was: {message}");
        assert!(!message.contains("sk-secret"), "key leaked: {message}");
        assert_eq!(transport.request_count(), 6); // three JSON-mode attempts + three fallback attempts
        assert_eq!(
            sleeper.delays(),
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(1),
                Duration::from_secs(2)
            ]
        );
    }

    #[test]
    fn auth_errors_only_retry_without_json_mode() {
        let transport = Arc::new(FakeTransport::default());
        transport.push_http(401, r#"{"error":{"message":"invalid api key"}}"#);
        transport.push_http(401, r#"{"error":{"message":"invalid api key"}}"#);
        let provider = provider(transport.clone());
        let error = provider.complete(request()).unwrap_err();
        assert!(matches!(error, MemoryError::Model(_)));
        assert_eq!(transport.request_count(), 2);
        let requests = transport.requests.lock().unwrap();
        assert!(requests[0].body.get("response_format").is_some());
        assert!(requests[1].body.get("response_format").is_none());
    }

    #[test]
    fn unsupported_json_mode_retries_once_without_response_format() {
        let transport = Arc::new(FakeTransport::default());
        transport.push_http(400, "response_format unsupported");
        transport.push_ok(r#"{"choices":[{"message":{"content":"{}"}}]}"#);
        let provider = provider(transport.clone());
        assert_eq!(provider.complete(request()).unwrap().text, "{}");
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].body.get("response_format").is_some());
        assert!(requests[1].body.get("response_format").is_none());
        assert_eq!(requests[0].body["messages"], requests[1].body["messages"]);
    }

    #[test]
    fn malformed_success_body_does_not_trigger_a_fallback_request() {
        let transport = Arc::new(FakeTransport::default());
        transport.push_ok("not JSON");
        let provider = provider(transport.clone());
        assert!(provider.complete(request()).is_err());
        assert_eq!(transport.request_count(), 1);
    }

    #[test]
    fn transport_errors_are_retried() {
        let transport = Arc::new(FakeTransport::default());
        transport.push_transport_error("connection refused");
        transport.push_ok(r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#);
        let provider = provider(transport.clone());
        let response = provider
            .complete(request())
            .expect("retried then succeeded");
        assert_eq!(response.text, "{\"ok\":true}");
        assert_eq!(transport.request_count(), 2);
    }

    #[test]
    fn missing_model_is_invalid_config() {
        let mut config = config(None);
        config.model.clear();
        let result = OpenAiChatProvider::with_parts(
            config,
            Arc::new(FakeTransport::default()),
            Arc::new(NoopSleeper),
        );
        assert!(matches!(result, Err(MemoryError::InvalidConfig(_))));
    }

    #[test]
    fn api_key_env_resolution() {
        // Guarded by a unique variable so parallel tests do not interfere.
        std::env::set_var("PD_MEMORY_TEST_OPENAI_KEY", "sk-from-env");
        let mut config = config(None);
        config.api_key_env = Some("PD_MEMORY_TEST_OPENAI_KEY".into());
        let transport = Arc::new(FakeTransport::default());
        let provider = OpenAiChatProvider::with_parts(config, transport, Arc::new(NoopSleeper))
            .expect("builds");
        let http = provider.build_http_request(&request());
        assert!(http
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                && value == "Bearer sk-from-env"));
        std::env::remove_var("PD_MEMORY_TEST_OPENAI_KEY");
    }

    #[test]
    fn injected_transport_receives_requests() {
        let transport = Arc::new(FakeTransport::default());
        transport.push_ok(r#"{"choices":[{"message":{"content":"{}"}}]}"#);
        let provider = provider(transport.clone());
        provider.complete(request()).expect("completes");
        let recorded = transport.requests.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].url, "http://localhost:9999/v1/chat/completions");
    }
}
