//! Helpers shared by the HTTP providers: URL joining, header assembly,
//! timeout resolution and safe error-body summarisation.

use std::collections::BTreeMap;
use std::time::Duration;

use pd_memory_core::config::ModelConfig;
use pd_memory_core::error::MemoryError;
use pd_memory_core::ports::CompletionRequest;

/// Default OpenAI-compatible base URL (includes the `/v1` segment).
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Default Anthropic base URL (no `/v1` segment).
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// Anthropic API version header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Maximum number of characters of an error response body copied into an error.
pub const ERROR_BODY_EXCERPT_CHARS: usize = 600;

fn trim_end_slashes(value: &str) -> &str {
    value.trim_end_matches('/')
}

/// Resolve the OpenAI-compatible chat completions endpoint.
///
/// - `None`/empty -> `https://api.openai.com/v1/chat/completions`
/// - ends with `/chat/completions` -> used verbatim
/// - otherwise `<base>/chat/completions` (e.g. `http://localhost:11434/v1`
///   becomes `http://localhost:11434/v1/chat/completions`)
pub fn openai_endpoint(base_url: Option<&str>) -> String {
    let base = base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_OPENAI_BASE_URL);
    let base = trim_end_slashes(base);
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

/// Resolve the Anthropic messages endpoint.
///
/// - `None`/empty -> `https://api.anthropic.com/v1/messages`
/// - ends with `/v1/messages` -> used verbatim
/// - ends with `/v1` -> `<base>/messages`
/// - otherwise `<base>/v1/messages`
pub fn anthropic_endpoint(base_url: Option<&str>) -> String {
    let base = base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_ANTHROPIC_BASE_URL);
    let base = trim_end_slashes(base);
    if base.ends_with("/v1/messages") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// Merge default headers with user-configured headers (case-insensitive on the
/// name; user values win) while preserving deterministic ordering.
pub fn merged_headers(
    config: &ModelConfig,
    defaults: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut merged: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (name, value) in defaults {
        merged.insert(name.to_ascii_lowercase(), (name, value));
    }
    for (name, value) in &config.headers {
        let lowered = name.to_ascii_lowercase();
        merged.insert(lowered, (name.clone(), value.clone()));
    }
    merged.into_values().collect()
}

/// Timeout for one attempt: the request timeout when set, otherwise the
/// provider-wide configured timeout. Always at least one millisecond so ureq
/// never receives a zero timeout by accident.
pub fn request_timeout(request: &CompletionRequest, config: &ModelConfig) -> Duration {
    let timeout = if request.timeout.is_zero() {
        Duration::from_millis(config.timeout_ms.max(1))
    } else {
        request.timeout
    };
    timeout.max(Duration::from_millis(1))
}

/// Truncate an error response body to a small, log-safe excerpt.
pub fn body_excerpt(body: &str) -> String {
    let text = body.trim();
    if text.is_empty() {
        return "<empty body>".to_string();
    }
    let mut excerpt: String = text.chars().take(ERROR_BODY_EXCERPT_CHARS).collect();
    if text.chars().count() > ERROR_BODY_EXCERPT_CHARS {
        excerpt.push_str("...");
    }
    excerpt
}

/// Replace any occurrence of a secret with `[REDACTED]` so provider error
/// messages can never leak an API key, even if a proxy echoes request headers.
pub fn redact_secret(text: &str, secret: Option<&str>) -> String {
    let Some(secret) = secret.map(str::trim).filter(|value| !value.is_empty()) else {
        return text.to_string();
    };
    text.replace(secret, "[REDACTED]")
}

/// Build the [`MemoryError::Model`] used for a non-2xx provider response.
pub fn http_status_error(
    provider: &str,
    status: u16,
    body: &str,
    api_key: Option<&str>,
) -> MemoryError {
    let excerpt = redact_secret(&body_excerpt(body), api_key);
    MemoryError::Model(format!(
        "{provider} request failed with HTTP {status}: {excerpt}"
    ))
}

/// Map a transport-level failure to [`MemoryError::Model`]. Transport errors
/// never contain header values, but the secret is still scrubbed defensively.
pub fn transport_error(
    provider: &str,
    error: impl std::fmt::Display,
    api_key: Option<&str>,
) -> MemoryError {
    MemoryError::Model(redact_secret(
        &format!("{provider} transport error: {error}"),
        api_key,
    ))
}

/// Build the [`MemoryError::Model`] used when a 2xx response body is malformed.
pub fn invalid_response(provider: &str, detail: impl std::fmt::Display) -> MemoryError {
    MemoryError::Model(format!("{provider} returned an invalid response: {detail}"))
}

/// Extract text from an OpenAI-style `content` value: either a plain string or
/// an array of content parts (`[{"type":"text","text":"..."}]`).
pub fn content_value_to_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(parts) => {
            let mut collected = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
                    collected.push_str(text);
                }
            }
            if collected.is_empty() {
                None
            } else {
                Some(collected)
            }
        }
        _ => None,
    }
}

/// Lenient usage extraction: `input_tokens`/`prompt_tokens` and
/// `output_tokens`/`completion_tokens` depending on provider naming.
pub fn usage_from_value(
    usage: Option<&serde_json::Value>,
) -> Option<pd_memory_core::ports::TokenUsage> {
    let usage = usage?;
    let input = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(serde_json::Value::as_u64);
    let output = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(serde_json::Value::as_u64);
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(pd_memory_core::ports::TokenUsage {
        input_tokens: input,
        output_tokens: output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_timeout(timeout: Duration) -> CompletionRequest {
        CompletionRequest {
            system: "system".into(),
            user: "user".into(),
            schema_name: "memory_classify".into(),
            timeout,
            max_output_tokens: None,
            temperature: None,
        }
    }

    #[test]
    fn openai_endpoint_defaults_and_joins() {
        assert_eq!(
            openai_endpoint(None),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            openai_endpoint(Some("http://localhost:11434/v1/")),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            openai_endpoint(Some("https://example.com/chat/completions")),
            "https://example.com/chat/completions"
        );
    }

    #[test]
    fn anthropic_endpoint_defaults_and_joins() {
        assert_eq!(
            anthropic_endpoint(None),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_endpoint(Some("https://proxy.example.com/v1")),
            "https://proxy.example.com/v1/messages"
        );
        assert_eq!(
            anthropic_endpoint(Some("https://proxy.example.com/v1/messages")),
            "https://proxy.example.com/v1/messages"
        );
    }

    #[test]
    fn user_headers_override_defaults_case_insensitively() {
        let mut config = ModelConfig::default();
        config
            .headers
            .insert("Authorization".into(), "Bearer custom".into());
        config.headers.insert("x-extra".into(), "1".into());
        let headers = merged_headers(
            &config,
            [("authorization".to_string(), "Bearer default".to_string())],
        );
        assert_eq!(
            headers,
            vec![
                ("Authorization".to_string(), "Bearer custom".to_string()),
                ("x-extra".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn body_excerpt_is_bounded_and_marks_truncation() {
        let body = "x".repeat(ERROR_BODY_EXCERPT_CHARS + 10);
        let excerpt = body_excerpt(&body);
        assert!(excerpt.ends_with("..."));
        assert_eq!(excerpt.chars().count(), ERROR_BODY_EXCERPT_CHARS + 3);
        assert_eq!(body_excerpt("   "), "<empty body>");
    }

    #[test]
    fn secrets_are_redacted_from_error_text() {
        let text = "failed with key sk-secret-123";
        assert_eq!(
            redact_secret(text, Some("sk-secret-123")),
            "failed with key [REDACTED]"
        );
        assert_eq!(redact_secret(text, None), text);
    }

    #[test]
    fn timeout_falls_back_to_provider_config() {
        let mut config = ModelConfig::default();
        config.timeout_ms = 2500;
        assert_eq!(
            request_timeout(&request_with_timeout(Duration::ZERO), &config),
            Duration::from_millis(2500)
        );
        assert_eq!(
            request_timeout(&request_with_timeout(Duration::from_secs(9)), &config),
            Duration::from_secs(9)
        );
    }

    #[test]
    fn content_value_handles_strings_and_text_parts() {
        assert_eq!(
            content_value_to_text(&serde_json::json!("hello")),
            Some("hello".into())
        );
        assert_eq!(
            content_value_to_text(&serde_json::json!([
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b"}
            ])),
            Some("ab".into())
        );
        assert_eq!(content_value_to_text(&serde_json::json!(7)), None);
    }
}
