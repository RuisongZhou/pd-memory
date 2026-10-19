//! Retry policy shared by the HTTP model providers.
//!
//! The engine is blocking by design, so retries are simple thread sleeps with
//! exponential backoff (1s, 2s, 4s, ...). Only transient conditions are retried:
//! status codes `408`, `409`, `425`, `429`, `500`, `502`, `503`, `504`, plus transport-level
//! failures (connect/timeout/body-read errors). Everything else fails fast.

use std::time::Duration;

use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::ports::CompletionResponse;

/// Base backoff delay; attempt `n` waits `BASE_BACKOFF * 2^n`.
pub const BASE_BACKOFF_MS: u64 = 1_000;

/// Delay before retry number `attempt` (0-based): 1s, 2s, 4s, ...
pub fn backoff_delay(attempt: u32) -> Duration {
    Duration::from_millis(BASE_BACKOFF_MS.saturating_mul(1u64 << attempt.min(10)))
}

/// Whether an HTTP status code is worth retrying.
pub fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Sleep abstraction so tests can run retry loops without real delays.
pub trait Sleeper: Send + Sync {
    /// Block for `duration`.
    fn sleep(&self, duration: Duration);
}

/// Default [`Sleeper`] backed by [`std::thread::sleep`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemSleeper;

impl Sleeper for SystemSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Result of one provider attempt, classified for the retry loop.
pub(crate) enum AttemptOutcome<T = CompletionResponse> {
    Success(T),
    /// Transient failure: may be retried.
    Retryable(MemoryError),
    /// Permanent failure: retrying cannot help.
    Fatal(MemoryError),
}

/// Drive up to `1 + max_retries` attempts with exponential backoff.
pub(crate) fn run_with_retries<T>(
    max_retries: u32,
    sleeper: &dyn Sleeper,
    mut attempt: impl FnMut(u32) -> AttemptOutcome<T>,
) -> Result<T> {
    let mut retries_used = 0u32;
    loop {
        match attempt(retries_used) {
            AttemptOutcome::Success(response) => return Ok(response),
            AttemptOutcome::Fatal(error) => return Err(error),
            AttemptOutcome::Retryable(error) => {
                if retries_used >= max_retries {
                    return Err(error);
                }
                let delay = backoff_delay(retries_used);
                log::warn!(
                    "model request failed ({}); retry {}/{} in {:?}",
                    error,
                    retries_used + 1,
                    max_retries,
                    delay
                );
                sleeper.sleep(delay);
                retries_used += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSleeper {
        slept: Mutex<Vec<Duration>>,
    }

    impl Sleeper for RecordingSleeper {
        fn sleep(&self, duration: Duration) {
            self.slept.lock().unwrap().push(duration);
        }
    }

    fn response(text: &str) -> CompletionResponse {
        CompletionResponse {
            text: text.to_string(),
            usage: None,
        }
    }

    #[test]
    fn backoff_is_exponential_from_one_second() {
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
    }

    #[test]
    fn retryable_status_classification() {
        assert!(retryable_status(408));
        assert!(retryable_status(409));
        assert!(retryable_status(425));
        assert!(retryable_status(429));
        assert!(retryable_status(500));
        assert!(retryable_status(503));
        assert!(!retryable_status(501));
        assert!(!retryable_status(599));
        assert!(!retryable_status(200));
        assert!(!retryable_status(400));
        assert!(!retryable_status(401));
        assert!(!retryable_status(404));
        assert!(!retryable_status(422));
        assert!(!retryable_status(600));
    }

    #[test]
    fn retries_until_success_with_exponential_backoff() {
        let sleeper = RecordingSleeper::default();
        let mut attempts = 0u32;
        let result: CompletionResponse = run_with_retries(3, &sleeper, |_retry| {
            attempts += 1;
            if attempts < 3 {
                AttemptOutcome::Retryable(MemoryError::Model("HTTP 429: slow down".into()))
            } else {
                AttemptOutcome::Success(response("ok"))
            }
        })
        .expect("eventually succeeds");
        assert_eq!(result.text, "ok");
        assert_eq!(attempts, 3);
        assert_eq!(
            *sleeper.slept.lock().unwrap(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
    }

    #[test]
    fn fatal_errors_are_not_retried() {
        let sleeper = RecordingSleeper::default();
        let mut attempts = 0u32;
        let result: Result<CompletionResponse> = run_with_retries(3, &sleeper, |_retry| {
            attempts += 1;
            AttemptOutcome::Fatal(MemoryError::Model("HTTP 401: bad key".into()))
        });
        assert!(matches!(result, Err(MemoryError::Model(_))));
        assert_eq!(attempts, 1);
        assert!(sleeper.slept.lock().unwrap().is_empty());
    }

    #[test]
    fn retries_are_bounded_by_max_retries() {
        let sleeper = RecordingSleeper::default();
        let mut attempts = 0u32;
        let result: Result<CompletionResponse> = run_with_retries(2, &sleeper, |_retry| {
            attempts += 1;
            AttemptOutcome::Retryable(MemoryError::Model("HTTP 500: boom".into()))
        });
        assert!(result.is_err());
        assert_eq!(attempts, 3);
        assert_eq!(
            *sleeper.slept.lock().unwrap(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
    }

    #[test]
    fn zero_retries_means_single_attempt() {
        let sleeper = RecordingSleeper::default();
        let mut attempts = 0u32;
        let result: Result<CompletionResponse> = run_with_retries(0, &sleeper, |_retry| {
            attempts += 1;
            AttemptOutcome::Retryable(MemoryError::Model("HTTP 503: try later".into()))
        });
        assert!(result.is_err());
        assert_eq!(attempts, 1);
        assert!(sleeper.slept.lock().unwrap().is_empty());
    }
}
