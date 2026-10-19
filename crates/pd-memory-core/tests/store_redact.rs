//! DefaultRedactor tests (task-1, dev A).

use pd_memory_core::config::RedactionConfig;
use pd_memory_core::ports::Redactor;
use pd_memory_core::redact::DefaultRedactor;

fn redactor() -> DefaultRedactor {
    DefaultRedactor::new(RedactionConfig::default())
}

#[test]
fn redacts_emails_without_touching_chinese() {
    let out = redactor().redact("我的邮箱是 test.user+tag@example.com，请勿外传。");
    assert_eq!(out, "我的邮箱是 [REDACTED_EMAIL]，请勿外传。");
}

#[test]
fn redacts_cn_mobile_numbers_only() {
    let out = redactor().redact("电话 13812345678，订单号 12345678901 不是手机号。");
    assert!(out.contains("[REDACTED_PHONE]"));
    assert!(!out.contains("13812345678"));
    assert!(out.contains("12345678901"));
    assert!(out.starts_with("电话 "));
}

#[test]
fn redacts_api_keys_and_bearer_tokens() {
    let out = redactor()
        .redact("api_key=sk-abc123DEF456ghi789 header=Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig");
    assert!(!out.contains("sk-abc123DEF456ghi789"));
    assert!(out.contains("[REDACTED_API_KEY]"));
    assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"));
    assert!(out.contains("Bearer [REDACTED_TOKEN]"));
}

#[test]
fn supports_extra_patterns_and_empty_input() {
    let redactor = DefaultRedactor::new(RedactionConfig {
        enabled: true,
        extra_patterns: vec!["内部代号".to_string(), String::new()],
    });
    assert_eq!(redactor.redact("这是内部代号项目"), "这是[REDACTED]项目");
    assert_eq!(redactor.redact(""), "");
    assert_eq!(redactor.redact("没有敏感信息的中文"), "没有敏感信息的中文");
}

#[test]
fn disabled_redactor_is_identity() {
    let disabled = DefaultRedactor::new(RedactionConfig {
        enabled: false,
        extra_patterns: vec!["secret".to_string()],
    });
    let text = "a@b.com 13812345678 secret sk-abcdefghi";
    assert_eq!(disabled.redact(text), text);
}
