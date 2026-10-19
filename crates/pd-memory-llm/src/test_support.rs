//! In-crate test doubles (never compiled into the library).
//!
//! Kept in the crate (rather than only in `#[cfg(test)]` modules) so the
//! OpenAI and Anthropic provider test modules can share one fake transport.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use crate::http_transport::{HttpRequest, HttpResponse, HttpTransport, TransportError};
use crate::retry::Sleeper;

/// Queued reply for [`FakeTransport`].
#[derive(Debug, Clone)]
pub(crate) enum FakeReply {
    Http { status: u16, body: String },
    Transport(String),
}

/// Deterministic [`HttpTransport`] serving queued replies in FIFO order.
#[derive(Default)]
pub(crate) struct FakeTransport {
    replies: Mutex<VecDeque<FakeReply>>,
    pub requests: Mutex<Vec<HttpRequest>>,
}

impl FakeTransport {
    pub(crate) fn push_http(&self, status: u16, body: impl Into<String>) {
        self.replies.lock().unwrap().push_back(FakeReply::Http {
            status,
            body: body.into(),
        });
    }

    pub(crate) fn push_ok(&self, body: impl Into<String>) {
        self.push_http(200, body);
    }

    pub(crate) fn push_transport_error(&self, message: impl Into<String>) {
        self.replies
            .lock()
            .unwrap()
            .push_back(FakeReply::Transport(message.into()));
    }

    pub(crate) fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl HttpTransport for FakeTransport {
    fn post_json(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.requests.lock().unwrap().push(request.clone());
        match self.replies.lock().unwrap().pop_front() {
            Some(FakeReply::Http { status, body }) => Ok(HttpResponse { status, body }),
            Some(FakeReply::Transport(message)) => Err(TransportError(message)),
            None => Err(TransportError(
                "fake transport: no queued reply for request".into(),
            )),
        }
    }
}

/// Sleeper that never blocks.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NoopSleeper;

impl Sleeper for NoopSleeper {
    fn sleep(&self, _duration: Duration) {}
}

/// Sleeper that records requested delays (for retry-policy assertions).
#[derive(Debug, Default)]
pub(crate) struct RetryRecordingSleeper {
    delays: Mutex<Vec<Duration>>,
}

impl RetryRecordingSleeper {
    pub(crate) fn delays(&self) -> Vec<Duration> {
        self.delays.lock().unwrap().clone()
    }
}

impl Sleeper for RetryRecordingSleeper {
    fn sleep(&self, duration: Duration) {
        self.delays.lock().unwrap().push(duration);
    }
}
