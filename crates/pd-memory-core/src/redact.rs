//! Default secret / PII redaction (dev A).
//!
//! Applied to captured messages before they are persisted and to traces before
//! they leave the process. Must be cheap, deterministic and safe for Chinese
//! text (no aggressive entity removal).
//!
//! Implemented without a regex engine: the scanners below are single-pass,
//! linear in the input size, and only match ASCII-shaped PII (e-mail addresses,
//! 11-digit CN mobile numbers, `sk-...` API keys, `Bearer ...` tokens). CJK
//! characters are never consumed by a pattern and are copied through verbatim.

use crate::config::RedactionConfig;
use crate::ports::Redactor;

/// Placeholder used for configured `extra_patterns` literals.
const EXTRA_PLACEHOLDER: &str = "[REDACTED]";
const EMAIL_PLACEHOLDER: &str = "[REDACTED_EMAIL]";
const PHONE_PLACEHOLDER: &str = "[REDACTED_PHONE]";
const API_KEY_PLACEHOLDER: &str = "[REDACTED_API_KEY]";
const BEARER_PLACEHOLDER: &str = "Bearer [REDACTED_TOKEN]";

/// Regex-free default redactor: emails, phone numbers, bearer tokens,
/// `sk-`-style API keys, and configured extra literal patterns.
#[derive(Debug, Clone)]
pub struct DefaultRedactor {
    pub config: RedactionConfig,
}

impl DefaultRedactor {
    pub fn new(config: RedactionConfig) -> Self {
        Self { config }
    }
}

impl Redactor for DefaultRedactor {
    fn redact(&self, text: &str) -> String {
        if !self.config.enabled || text.is_empty() {
            return text.to_string();
        }
        let mut redacted = redact_bearer_tokens(text);
        redacted = redact_api_keys(&redacted);
        redacted = redact_emails(&redacted);
        redacted = redact_cn_mobiles(&redacted);
        for pattern in &self.config.extra_patterns {
            if pattern.is_empty() {
                continue;
            }
            redacted = redacted.replace(pattern.as_str(), EXTRA_PLACEHOLDER);
        }
        redacted
    }
}

// ---------------------------------------------------------------------------
// ASCII helpers
// ---------------------------------------------------------------------------

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// True when the byte at `index` (if any) continues an ASCII word.
fn continues_word(bytes: &[u8], index: usize) -> bool {
    bytes.get(index).copied().map(is_word_byte).unwrap_or(false)
}

fn starts_with_ci(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    if index + needle.len() > bytes.len() {
        return false;
    }
    bytes[index..index + needle.len()]
        .iter()
        .zip(needle.iter())
        .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

// ---------------------------------------------------------------------------
// Bearer tokens
// ---------------------------------------------------------------------------

fn redact_bearer_tokens(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        if starts_with_ci(bytes, index, b"bearer")
            && !continues_word(bytes, index.wrapping_sub(1))
            && bytes
                .get(index + 6)
                .map(|byte| byte.is_ascii_whitespace())
                .unwrap_or(false)
        {
            let mut token_start = index + 6;
            while token_start < bytes.len() && bytes[token_start].is_ascii_whitespace() {
                token_start += 1;
            }
            let mut token_end = token_start;
            while token_end < bytes.len() && is_bearer_token_byte(bytes[token_end]) {
                token_end += 1;
            }
            // Avoid mangling short prose such as `Bearer authentication`.
            if token_end.saturating_sub(token_start) >= 8 {
                output.push_str(&text[copied..index]);
                output.push_str(BEARER_PLACEHOLDER);
                copied = token_end;
                index = token_end;
                continue;
            }
        }
        index += 1;
    }
    output.push_str(&text[copied..]);
    output
}

fn is_bearer_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
}

// ---------------------------------------------------------------------------
// `sk-...` API keys
// ---------------------------------------------------------------------------

fn redact_api_keys(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        if starts_with_ci(bytes, index, b"sk-") && !continues_word(bytes, index.wrapping_sub(1)) {
            let token_start = index + 3;
            let mut token_end = token_start;
            while token_end < bytes.len()
                && (bytes[token_end].is_ascii_alphanumeric()
                    || matches!(bytes[token_end], b'_' | b'-'))
            {
                token_end += 1;
            }
            if token_end.saturating_sub(token_start) >= 6 {
                output.push_str(&text[copied..index]);
                output.push_str(API_KEY_PLACEHOLDER);
                copied = token_end;
                index = token_end;
                continue;
            }
        }
        index += 1;
    }
    output.push_str(&text[copied..]);
    output
}

// ---------------------------------------------------------------------------
// E-mail addresses
// ---------------------------------------------------------------------------

fn redact_emails(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'@' {
            index += 1;
            continue;
        }
        // Expand the local part to the left.
        let mut start = index;
        while start > copied && is_email_local_byte(bytes[start - 1]) {
            start -= 1;
        }
        if start == index {
            index += 1;
            continue;
        }
        let local = &text[start..index];
        if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
            index += 1;
            continue;
        }
        // Expand the domain to the right.
        let mut end = index + 1;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'.' | b'-'))
        {
            end += 1;
        }
        let domain = &text[index + 1..end];
        if !is_valid_domain(domain) {
            index += 1;
            continue;
        }
        output.push_str(&text[copied..start]);
        output.push_str(EMAIL_PLACEHOLDER);
        copied = end;
        index = end;
    }
    output.push_str(&text[copied..]);
    output
}

fn is_email_local_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_valid_domain(domain: &str) -> bool {
    if domain.is_empty() || !domain.contains('.') {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.iter().any(|label| label.is_empty()) {
        return false;
    }
    let Some(tld) = labels.last() else {
        return false;
    };
    tld.len() >= 2 && tld.chars().all(|character| character.is_ascii_alphabetic())
}

// ---------------------------------------------------------------------------
// CN mobile numbers (11 digits, 1[3-9]xxxxxxxxx)
// ---------------------------------------------------------------------------

fn redact_cn_mobiles(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    while index + 11 <= bytes.len() {
        if bytes[index] == b'1'
            && !continues_word(bytes, index.wrapping_sub(1))
            && matches!(bytes[index + 1], b'3'..=b'9')
            && (index..index + 11).all(|offset| bytes[offset].is_ascii_digit())
            && !continues_word(bytes, index + 11)
        {
            output.push_str(&text[copied..index]);
            output.push_str(PHONE_PLACEHOLDER);
            copied = index + 11;
            index += 11;
            continue;
        }
        index += 1;
    }
    output.push_str(&text[copied..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> DefaultRedactor {
        DefaultRedactor::new(RedactionConfig::default())
    }

    #[test]
    fn redacts_email_and_phone() {
        let text = "联系我 test.user+tag@example.com 或 13812345678，谢谢。";
        let out = redactor().redact(text);
        assert_eq!(out, "联系我 [REDACTED_EMAIL] 或 [REDACTED_PHONE]，谢谢。");
    }

    #[test]
    fn redacts_api_keys_and_bearer_tokens() {
        let text = "key=sk-abc123DEF456 token: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig";
        let out = redactor().redact(text);
        assert!(!out.contains("sk-abc123DEF456"));
        assert!(out.contains("[REDACTED_API_KEY]"));
        assert!(out.contains(BEARER_PLACEHOLDER));
    }

    #[test]
    fn keeps_chinese_text_and_short_numbers() {
        let text = "这是一个中文测试，订单号 12345678901 不应被当作手机号，但 13912345678 应该。";
        let out = redactor().redact(text);
        // `12345678901` starts with `12`, not a CN mobile prefix.
        assert!(out.contains("12345678901"));
        assert!(out.contains(PHONE_PLACEHOLDER));
        assert!(out.starts_with("这是一个中文测试"));
    }

    #[test]
    fn supports_extra_patterns_and_disable() {
        let mut config = RedactionConfig::default();
        config.extra_patterns = vec!["内部代号".to_string()];
        let redactor = DefaultRedactor::new(config);
        assert_eq!(redactor.redact("这是内部代号项目"), "这是[REDACTED]项目");
        let disabled = DefaultRedactor::new(RedactionConfig {
            enabled: false,
            extra_patterns: Vec::new(),
        });
        assert_eq!(
            disabled.redact("a@b.com 13812345678"),
            "a@b.com 13812345678"
        );
    }
}
