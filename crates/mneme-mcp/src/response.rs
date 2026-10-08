//! Exact MCP response encoding and the final process-boundary safety rails.
//!
//! Tool values are rendered once into compact text content, then the complete
//! JSON-RPC response is serialized once and those exact bytes are shared by the
//! stdio and HTTP transports. Oversize content is replaced with a small error;
//! JSON is never made invalid by byte truncation.

use std::io::Write;

use serde_json::{Value, json};

/// Maximum decoded UTF-8 bytes in the MCP text block produced for one tool
/// result. This is an emergency ceiling; the typed presentation packer will use
/// a smaller task/context budget before exposure and receipt issuance.
pub const MAX_TOOL_TEXT_BYTES: usize = 256 * 1024;

/// Maximum serialized JSON-RPC frame size. Stdio's trailing newline counts
/// against this ceiling, so the shared JSON body is limited to one byte less.
pub const MAX_JSONRPC_FRAME_BYTES: usize = 512 * 1024;

const STDIO_DELIMITER_BYTES: usize = 1;
const MAX_JSONRPC_BODY_BYTES: usize = MAX_JSONRPC_FRAME_BYTES - STDIO_DELIMITER_BYTES;
const RESPONSE_TOO_LARGE_CODE: i64 = -32001;

/// A JSON-RPC response paired with its one canonical compact serialization.
/// Transports must send [`Self::bytes`] directly rather than serializing
/// [`Self::value`] again.
#[derive(Debug)]
pub struct BoundedResponse {
    #[cfg(feature = "http")]
    value: Value,
    bytes: Vec<u8>,
}

impl BoundedResponse {
    /// Serialize `value` once. If it exceeds the frame ceiling, replace it with
    /// a compact JSON-RPC error carrying the original request id when that id
    /// itself fits inside a bounded response.
    pub fn new(value: Value) -> Self {
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        match serde_json::to_vec(&value) {
            Ok(bytes) if bytes.len() <= MAX_JSONRPC_BODY_BYTES => Self {
                #[cfg(feature = "http")]
                value,
                bytes,
            },
            Ok(_) => Self::replacement(
                id,
                RESPONSE_TOO_LARGE_CODE,
                &format!(
                    "response exceeds the {MAX_JSONRPC_FRAME_BYTES}-byte JSON-RPC frame limit"
                ),
            ),
            Err(_) => Self::replacement(id, -32603, "response could not be serialized as JSON"),
        }
    }

    fn replacement(id: Value, code: i64, message: &str) -> Self {
        let value = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        if let Ok(bytes) = serde_json::to_vec(&value)
            && bytes.len() <= MAX_JSONRPC_BODY_BYTES
        {
            return Self {
                #[cfg(feature = "http")]
                value,
                bytes,
            };
        }

        // A request id may itself be almost as large as the inbound request.
        // Keeping that id and keeping a hard response bound are then mutually
        // exclusive; fail closed on the byte invariant with JSON-RPC's null id.
        let value = json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {
                "code": RESPONSE_TOO_LARGE_CODE,
                "message": "response and request id exceed the JSON-RPC frame limit",
            },
        });
        let bytes =
            serde_json::to_vec(&value).expect("the fixed bounded-response fallback is valid JSON");
        debug_assert!(bytes.len() <= MAX_JSONRPC_BODY_BYTES);
        Self {
            #[cfg(feature = "http")]
            value,
            bytes,
        }
    }

    #[cfg(feature = "http")]
    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[cfg(feature = "http")]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Write the exact shared serialization as one newline-delimited stdio
    /// frame. The delimiter was already reserved by [`Self::new`].
    pub fn write_stdio(&self, out: &mut dyn Write) -> std::io::Result<()> {
        debug_assert!(self.bytes().len() + STDIO_DELIMITER_BYTES <= MAX_JSONRPC_FRAME_BYTES);
        out.write_all(self.bytes())?;
        out.write_all(b"\n")
    }
}

/// Wrap a successful tool value in one compact MCP text content block. The
/// limit measures the decoded `text` field, before JSON-RPC escaping.
pub fn tool_success(value: &Value) -> Value {
    match tool_text(value) {
        Ok(text) => tool_result(text, false),
        Err(ToolTextError::TooLarge { actual }) => tool_error(&format!(
            "tool result is {actual} decoded UTF-8 bytes; maximum is {MAX_TOOL_TEXT_BYTES}"
        )),
        Err(ToolTextError::Serialize) => {
            tool_error("tool result could not be serialized as compact JSON")
        }
    }
}

/// Wrap a tool failure without allowing a backend error string to bypass the
/// decoded-content ceiling. Oversize errors are replaced, never truncated.
pub fn tool_error(message: &str) -> Value {
    let text = if message.len() <= MAX_TOOL_TEXT_BYTES {
        message.to_owned()
    } else {
        format!(
            "tool error text is {} decoded UTF-8 bytes; maximum is {MAX_TOOL_TEXT_BYTES}",
            message.len()
        )
    };
    tool_result(text, true)
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

enum ToolTextError {
    TooLarge { actual: usize },
    Serialize,
}

fn tool_text(value: &Value) -> Result<String, ToolTextError> {
    let text = match value.as_str() {
        Some(text) => text.to_owned(),
        None => serde_json::to_string(value).map_err(|_| ToolTextError::Serialize)?,
    };
    if text.len() > MAX_TOOL_TEXT_BYTES {
        return Err(ToolTextError::TooLarge { actual: text.len() });
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jsonrpc(id: Value, result: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    #[test]
    fn compact_tool_text_round_trips_escaping_controls_and_multibyte_utf8() {
        let original = json!({
            "quote": "a\"b",
            "slash": "c\\d",
            "controls": "line one\nline two\t\u{0000}",
            "unicode": "snowman ☃ / cat 😺",
            "nested": [1, 2, { "ok": true }],
        });
        let tool = tool_success(&original);
        assert_eq!(tool["isError"], false);
        let inner = tool.pointer("/content/0/text").unwrap().as_str().unwrap();
        assert!(!inner.contains("\n  "), "inner JSON must be compact");
        assert_eq!(serde_json::from_str::<Value>(inner).unwrap(), original);

        let response = BoundedResponse::new(jsonrpc(json!("escape-id"), tool));
        let outer: Value = serde_json::from_slice(response.bytes()).unwrap();
        let inner = outer
            .pointer("/result/content/0/text")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(inner).unwrap(), original);
    }

    #[test]
    fn oversize_tool_text_becomes_a_small_tool_error() {
        let tool = tool_success(&Value::String("x".repeat(MAX_TOOL_TEXT_BYTES + 1)));
        assert_eq!(tool["isError"], true);
        let text = tool.pointer("/content/0/text").unwrap().as_str().unwrap();
        assert!(text.contains("tool result is"));
        assert!(text.len() < 256);
        assert!(serde_json::to_vec(&tool).unwrap().len() < 1024);
    }

    #[test]
    fn oversize_frame_becomes_a_small_error_and_preserves_request_id() {
        let id = json!("request-42");
        let response = BoundedResponse::new(jsonrpc(
            id.clone(),
            json!({ "blob": "x".repeat(MAX_JSONRPC_FRAME_BYTES) }),
        ));
        assert!(response.bytes().len() < MAX_JSONRPC_FRAME_BYTES);
        let value: Value = serde_json::from_slice(response.bytes()).unwrap();
        assert_eq!(value["id"], id);
        assert_eq!(
            value.pointer("/error/code"),
            Some(&json!(RESPONSE_TOO_LARGE_CODE))
        );
        assert!(value.get("result").is_none());
    }

    #[test]
    fn stdio_reuses_the_encoded_body_and_counts_its_newline() {
        let response = BoundedResponse::new(jsonrpc(json!(7), json!({ "ok": true })));
        let expected = response.bytes().to_vec();
        let mut frame = Vec::new();
        response.write_stdio(&mut frame).unwrap();
        assert_eq!(&frame[..frame.len() - 1], expected.as_slice());
        assert_eq!(frame.last(), Some(&b'\n'));
        assert!(frame.len() <= MAX_JSONRPC_FRAME_BYTES);
        assert_eq!(frame.iter().filter(|byte| **byte == b'\n').count(), 1);
    }

    #[test]
    fn request_id_too_large_to_echo_still_cannot_break_the_frame_bound() {
        let response = BoundedResponse::new(jsonrpc(
            Value::String("i".repeat(MAX_JSONRPC_FRAME_BYTES)),
            json!({ "ok": true }),
        ));
        assert!(response.bytes().len() < MAX_JSONRPC_FRAME_BYTES);
        let value: Value = serde_json::from_slice(response.bytes()).unwrap();
        assert_eq!(value["id"], Value::Null);
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("request id")
        );
    }
}
