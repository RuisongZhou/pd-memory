//! Minimal blocking HTTP transport abstraction.
//!
//! Providers depend on [`HttpTransport`] rather than `ureq` directly so that
//! unit tests can inject a fake transport and never touch the network. The
//! production implementation ([`UreqTransport`]) is a thin, blocking wrapper
//! around `ureq 3` with rustls.

use std::fmt;
use std::time::Duration;

use serde_json::Value;

/// A JSON POST request handed to a [`HttpTransport`].
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// Absolute request URL.
    pub url: String,
    /// Header name/value pairs. Values may contain credentials and must never
    /// be logged or copied into error messages.
    pub headers: Vec<(String, String)>,
    /// JSON request body.
    pub body: Value,
    /// End-to-end timeout for this attempt.
    pub timeout: Duration,
}

/// A raw HTTP response (status + UTF-8 body). HTTP error statuses are returned
/// as `Ok` so the provider owns retry classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body decoded as UTF-8 (lossy).
    pub body: String,
}

/// Transport-level failure (connect, TLS, timeout, body read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError(pub String);

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransportError {}

/// Blocking JSON-over-HTTP transport. Implementations must be thread-safe.
pub trait HttpTransport: Send + Sync {
    /// POST `request.body` as JSON and return the raw response.
    fn post_json(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// Default transport: blocking `ureq 3` over rustls.
///
/// HTTP error statuses are *not* turned into transport errors: the provider
/// inspects [`HttpResponse::status`] itself so that retryable statuses can be
/// retried and classified error bodies can be summarised.
#[derive(Debug, Clone, Copy, Default)]
pub struct UreqTransport;

impl HttpTransport for UreqTransport {
    fn post_json(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(request.timeout))
            .build()
            .into();
        let mut builder = agent.post(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let mut response = builder
            .send_json(&request.body)
            .map_err(|error| TransportError(format!("http request failed: {error}")))?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().map_err(|error| {
            TransportError(format!("http response body could not be read: {error}"))
        })?;
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_error_display_is_stable() {
        let error = TransportError("connection refused".into());
        assert_eq!(error.to_string(), "connection refused");
    }

    #[test]
    fn http_request_is_debug_printable_without_panicking() {
        let request = HttpRequest {
            url: "http://localhost/v1/chat/completions".into(),
            headers: vec![("authorization".into(), "Bearer secret".into())],
            body: serde_json::json!({"model": "m"}),
            timeout: Duration::from_millis(10),
        };
        assert!(format!("{request:?}").contains("/chat/completions"));
    }
}
