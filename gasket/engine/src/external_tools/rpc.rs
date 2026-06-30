//! JSON-RPC 2.0 message types and line-based codec for plugins.
//!
//! This module provides a minimal JSON-RPC 2.0 implementation for communication
//! with external plugins over stdio. Messages are serialized as single JSON
//! lines separated by `\n` for unambiguous parsing.
//!
//! # Protocol
//!
//! - Requests/Responses are delimited by newlines (`\n`)
//! - Maximum message size: 1 MiB
//! - Invalid JSON on stdout is silently discarded (logged at WARN level)
//! - Notifications (requests without `id`) are supported
//!
//! # Example
//!
//! ```rust
//! use gasket_engine::external_tools::rpc::{RpcMessage, RpcRequest};
//! use serde_json::json;
//!
//! let request = RpcRequest {
//!     jsonrpc: "2.0".to_string(),
//!     id: Some(json!(1)),
//!     method: "test/method".to_string(),
//!     params: Some(json!({"key": "value"})),
//! };
//!
//! let msg = RpcMessage::Request(request);
//! let encoded = gasket_engine::external_tools::rpc::encode(&msg);
//! assert!(encoded.ends_with('\n'));
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum size of a single JSON-RPC message (1 MiB).
///
/// Messages exceeding this size are rejected during decoding to prevent
/// memory exhaustion attacks from malicious scripts.
pub const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// JSON-RPC 2.0 message enum.
///
/// Represents either a request (or notification) from the client or a response
/// from the server. The untagged enum allows deserialization to try both variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcMessage {
    /// JSON-RPC request (including notifications which lack an `id` field)
    Request(RpcRequest),

    /// JSON-RPC response (success or error)
    Response(RpcResponse),
}

/// JSON-RPC 2.0 request.
///
/// Represents a method call from client to server. If `id` is `None`, this is
/// a notification (no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    /// JSON-RPC version (must be "2.0")
    pub jsonrpc: String,

    /// Request identifier (None for notifications)
    ///
    /// Notifications are requests that don't expect a response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,

    /// Method name to invoke
    pub method: String,

    /// Method parameters (optional, can be array or object)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 response.
///
/// Represents a success or error response from server to client. Exactly one
/// of `result` or `error` must be present (per JSON-RPC 2.0 spec).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    /// JSON-RPC version (must be "2.0")
    pub jsonrpc: String,

    /// Request identifier (must match the request's `id`)
    pub id: Value,

    /// Result value on success
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,

    /// Error details on failure
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// JSON-RPC 2.0 error object.
///
/// Structured error information for failed requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// Error code (number)
    ///
    /// Standard codes:
    /// - -32700: Parse error
    /// - -32600: Invalid request
    /// - -32601: Method not found
    /// - -32602: Invalid params
    /// - -32603: Internal error
    pub code: i32,

    /// Human-readable error message
    pub message: String,

    /// Additional error data (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Create a "method not found" error (-32601).
    ///
    /// Used when the requested RPC method is not available.
    pub fn method_not_found(method: impl Into<String>) -> Self {
        Self {
            code: -32601,
            message: format!("Method not found: {}", method.into()),
            data: None,
        }
    }

    /// Create a "permission denied" error (-32000).
    ///
    /// Used when the script lacks permission to call the requested method.
    pub fn permission_denied(method: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: format!("Permission denied: {}", method.into()),
            data: None,
        }
    }

    /// Create an "invalid params" error (-32602).
    ///
    /// Used when method parameters are invalid or cannot be processed.
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
            data: None,
        }
    }

    /// Create an "internal error" (-32603).
    ///
    /// Used for unexpected errors during request processing.
    pub fn internal_error(msg: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: msg.into(),
            data: None,
        }
    }
}

impl From<gasket_types::ToolError> for RpcError {
    fn from(err: gasket_types::ToolError) -> Self {
        match err {
            gasket_types::ToolError::InvalidArguments(msg) => RpcError::invalid_params(msg),
            gasket_types::ToolError::ExecutionError(msg) => RpcError::internal_error(msg),
            gasket_types::ToolError::PermissionDenied(msg) => RpcError::permission_denied(msg),
            gasket_types::ToolError::NotFound(msg) => RpcError::method_not_found(msg),
        }
    }
}

/// Encode a JSON-RPC message as a newline-terminated JSON string.
///
/// # Arguments
///
/// * `msg` - The message to encode
///
/// # Returns
///
/// A JSON string serialized from the message with a trailing `\n` character.
///
/// # Example
///
/// ```rust
/// use gasket_engine::external_tools::rpc::{RpcMessage, RpcRequest};
/// use serde_json::json;
///
/// let request = RpcRequest {
///     jsonrpc: "2.0".to_string(),
///     id: Some(json!(1)),
///     method: "test".to_string(),
///     params: None,
/// };
/// let msg = RpcMessage::Request(request);
/// let encoded = gasket_engine::external_tools::rpc::encode(&msg);
/// assert!(encoded.ends_with('\n'));
/// ```
pub fn encode(msg: &RpcMessage) -> String {
    let json = serde_json::to_string(msg).expect("failed to serialize RpcMessage");
    format!("{}\n", json)
}

/// Decode a JSON line into an RPC message.
///
/// # Arguments
///
/// * `line` - A single line from stdout (without the newline character)
///
/// # Returns
///
/// - `Some(RpcMessage)` - Successfully decoded message
/// - `None` - Input was empty, whitespace-only, too large, or invalid JSON
///
/// # Behavior
///
/// - Empty lines and whitespace-only lines return `None` (not an error)
/// - Lines exceeding `MAX_MESSAGE_SIZE` return `None` (logged at WARN)
/// - Invalid JSON returns `None` (logged at WARN with prefix `[script stdout non-JSON]`)
///
/// # Example
///
/// ```rust
/// use gasket_engine::external_tools::rpc::decode;
///
/// // Valid JSON-RPC request
/// let line = r#"{"jsonrpc":"2.0","id":1,"method":"test","params":{}}"#;
/// let msg = decode(line);
/// assert!(msg.is_some());
///
/// // Empty line
/// let line = "   \n";
/// let msg = decode(line);
/// assert!(msg.is_none());
/// ```
pub fn decode(line: &str) -> Option<Result<RpcMessage, String>> {
    // Skip empty and whitespace-only lines
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Enforce size limit
    if line.len() > MAX_MESSAGE_SIZE {
        return Some(Err(format!(
            "[script stdout oversized] {} bytes (exceeds {})",
            line.len(),
            MAX_MESSAGE_SIZE
        )));
    }

    // Attempt to parse JSON
    match serde_json::from_str::<RpcMessage>(trimmed) {
        Ok(msg) => Some(Ok(msg)),
        Err(e) => Some(Err(format!(
            "[script stdout non-JSON] {} - input: {:.100}",
            e, trimmed
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_encode_request() {
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(42)),
            method: "test/method".to_string(),
            params: Some(json!({"arg": "value"})),
        };
        let msg = RpcMessage::Request(request);
        let encoded = encode(&msg);

        // Verify newline termination
        assert!(encoded.ends_with('\n'));

        // Parse and verify fields
        let parsed: Value = serde_json::from_str(&encoded.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["method"], "test/method");
        assert_eq!(parsed["params"]["arg"], "value");
    }

    #[test]
    fn test_decode_request() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"test","params":{"key":"value"}}"#;
        let msg = decode(line)
            .expect("decode should return Some")
            .expect("failed to decode valid request");

        match msg {
            RpcMessage::Request(req) => {
                assert_eq!(req.jsonrpc, "2.0");
                assert_eq!(req.id, Some(json!(1)));
                assert_eq!(req.method, "test");
                assert_eq!(req.params, Some(json!({"key": "value"})));
            }
            _ => panic!("expected Request"),
        }
    }

    #[test]
    fn test_decode_response_with_result() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"status":"ok"}}"#;
        let msg = decode(line)
            .expect("decode should return Some")
            .expect("failed to decode valid response");

        match msg {
            RpcMessage::Response(res) => {
                assert_eq!(res.jsonrpc, "2.0");
                assert_eq!(res.id, json!(1));
                assert_eq!(res.result, Some(json!({"status": "ok"})));
                assert!(res.error.is_none());
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn test_decode_response_with_error() {
        let line =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let msg = decode(line)
            .expect("decode should return Some")
            .expect("failed to decode valid error response");

        match msg {
            RpcMessage::Response(res) => {
                assert_eq!(res.jsonrpc, "2.0");
                assert_eq!(res.id, json!(1));
                assert!(res.result.is_none());
                assert_eq!(
                    res.error,
                    Some(RpcError {
                        code: -32601,
                        message: "Method not found".to_string(),
                        data: None
                    })
                );
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn test_decode_invalid_json_returns_err() {
        let garbage = "{this is not valid json";
        let msg = decode(garbage);
        assert!(
            matches!(msg, Some(Err(_))),
            "invalid JSON should return Some(Err)"
        );
    }

    #[test]
    fn test_decode_plain_text_returns_err() {
        let text = "hello world this is just plain text";
        let msg = decode(text);
        assert!(
            matches!(msg, Some(Err(_))),
            "plain text should return Some(Err)"
        );
    }

    #[test]
    fn test_roundtrip_request() {
        let original = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(999)),
            method: "roundtrip/test".to_string(),
            params: Some(json!([1, 2, 3])),
        };

        let msg = RpcMessage::Request(original.clone());
        let encoded = encode(&msg);
        let decoded = decode(&encoded.trim())
            .expect("decode should return Some")
            .expect("roundtrip failed");

        match decoded {
            RpcMessage::Request(req) => {
                assert_eq!(req.jsonrpc, original.jsonrpc);
                assert_eq!(req.id, original.id);
                assert_eq!(req.method, original.method);
                assert_eq!(req.params, original.params);
            }
            _ => panic!("expected Request after roundtrip"),
        }
    }

    #[test]
    fn test_error_constructors() {
        let err1 = RpcError::method_not_found("unknown_method");
        assert_eq!(err1.code, -32601);
        assert_eq!(err1.message, "Method not found: unknown_method");
        assert!(err1.data.is_none());

        let err2 = RpcError::permission_denied("restricted/method");
        assert_eq!(err2.code, -32000);
        assert_eq!(err2.message, "Permission denied: restricted/method");
        assert!(err2.data.is_none());

        let err3 = RpcError::invalid_params("missing required field");
        assert_eq!(err3.code, -32602);
        assert_eq!(err3.message, "missing required field");
        assert!(err3.data.is_none());

        let err4 = RpcError::internal_error("database connection failed");
        assert_eq!(err4.code, -32603);
        assert_eq!(err4.message, "database connection failed");
        assert!(err4.data.is_none());
    }

    #[test]
    fn test_message_size_limit() {
        // Create a line exceeding 1 MiB
        let mut oversized = String::from(r#"{"jsonrpc":"2.0","method":"test","params":"#);
        oversized.push_str(&"x".repeat(MAX_MESSAGE_SIZE + 1000));
        oversized.push_str(r#""}"#);

        let msg = decode(&oversized);
        assert!(
            matches!(msg, Some(Err(_))),
            "oversized message should return Some(Err)"
        );
    }

    #[test]
    fn test_decode_empty_line() {
        assert!(decode("").is_none(), "empty string should return None");
        assert!(decode("   ").is_none(), "whitespace should return None");
        assert!(
            decode("\t\n\r").is_none(),
            "newline chars should return None"
        );
    }

    #[test]
    fn test_encode_notification() {
        // Notification: request without id
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: "notify".to_string(),
            params: Some(json!({"event": "update"})),
        };
        let msg = RpcMessage::Request(request);
        let encoded = encode(&msg);

        // Verify newline termination
        assert!(encoded.ends_with('\n'));

        // Parse and verify id field is absent
        let parsed: Value = serde_json::from_str(&encoded.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert!(
            parsed.get("id").is_none(),
            "notifications should not have id field"
        );
        assert_eq!(parsed["method"], "notify");
    }
}
