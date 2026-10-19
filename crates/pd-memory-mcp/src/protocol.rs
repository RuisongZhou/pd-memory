//! JSON-RPC 2.0 envelope helpers and MCP protocol constants.
//!
//! MCP's stdio transport is newline-delimited JSON-RPC 2.0: one complete JSON
//! object per line, nothing else on stdout.

use serde_json::{json, Value};

/// JSON-RPC version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// Protocol versions this server understands, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2024-11-05"];

/// Protocol version returned when the client requests an unknown version.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// Negotiate the initialize protocol version.
///
/// MCP clients (including WorkBuddy/CodeBuddy, which send `2025-11-25`) may
/// request versions newer than this server knows. Our JSON-RPC surface is
/// version-agnostic and the tools list is stable, so any well-formed
/// `YYYY-MM-DD` date is echoed back for forward compatibility. Anything else
/// falls back to [`DEFAULT_PROTOCOL_VERSION`].
pub fn negotiate_protocol_version(requested: Option<&str>) -> String {
    let requested = requested.map(str::trim).unwrap_or_default();
    if is_version_date(requested) {
        requested.to_string()
    } else {
        DEFAULT_PROTOCOL_VERSION.to_string()
    }
}

/// Whether `value` looks like an MCP protocol version (`YYYY-MM-DD`).
pub fn is_version_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    let digits_at = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    digits_at(0..4) && bytes[4] == b'-' && digits_at(5..7) && bytes[7] == b'-' && digits_at(8..10)
}

/// JSON-RPC error code: invalid JSON was received.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC error code: the JSON is not a valid request object.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC error code: method does not exist.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC error code: invalid method parameters.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC error code: internal server error.
pub const INTERNAL_ERROR: i64 = -32603;

/// Build a JSON-RPC success response.
pub fn success(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    })
}

/// Build a JSON-RPC error response.
pub fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_envelope_shape() {
        let value = success(json!(7), json!({"ok": true}));
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["result"]["ok"], true);
    }

    #[test]
    fn error_envelope_shape() {
        let value = error(Value::Null, METHOD_NOT_FOUND, "method not found");
        assert_eq!(value["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(value["error"]["message"], "method not found");
        assert_eq!(value["id"], Value::Null);
    }

    #[test]
    fn supported_versions_include_current_and_legacy() {
        assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&"2025-06-18"));
        assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&"2024-11-05"));
        assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&DEFAULT_PROTOCOL_VERSION));
    }

    #[test]
    fn protocol_negotiation_echoes_well_formed_versions() {
        assert_eq!(negotiate_protocol_version(Some("2024-11-05")), "2024-11-05");
        assert_eq!(negotiate_protocol_version(Some("2025-06-18")), "2025-06-18");
        // WorkBuddy/CodeBuddy sends a newer date; echo it for compatibility.
        assert_eq!(negotiate_protocol_version(Some("2025-11-25")), "2025-11-25");
        assert_eq!(
            negotiate_protocol_version(Some("garbage")),
            DEFAULT_PROTOCOL_VERSION
        );
        assert_eq!(negotiate_protocol_version(None), DEFAULT_PROTOCOL_VERSION);
        assert_eq!(
            negotiate_protocol_version(Some("2025-6-1")),
            DEFAULT_PROTOCOL_VERSION
        );
    }
}
