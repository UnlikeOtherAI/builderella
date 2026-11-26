//! JSON-RPC 2.0 message shapes and the MCP protocol revisions this server
//! speaks.

use serde_json::{Map, Value, json};

pub(crate) const PARSE_ERROR: i64 = -32700;
pub(crate) const INVALID_REQUEST: i64 = -32600;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const INTERNAL_ERROR: i64 = -32603;
/// Defined by revision 2026-07-28.
pub(crate) const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

pub(crate) const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub(crate) const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub(crate) const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
pub(crate) const META_SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";

/// Every protocol revision the server accepts, newest first.
///
/// `2026-07-28` is stateless: each request carries its version in `_meta`
/// and there is no handshake. The older revisions are negotiated with
/// `initialize`. Both styles are served on the same connection.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// A protocol revision, ordered by date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Revision {
    V2024_11_05,
    V2025_03_26,
    V2025_06_18,
    V2025_11_25,
    V2026_07_28,
}

impl Revision {
    /// The newest revision that uses the `initialize` handshake. Offered
    /// when a client asks for a version the server does not know.
    pub(crate) const LATEST_HANDSHAKE: Revision = Revision::V2025_11_25;

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "2024-11-05" => Self::V2024_11_05,
            "2025-03-26" => Self::V2025_03_26,
            "2025-06-18" => Self::V2025_06_18,
            "2025-11-25" => Self::V2025_11_25,
            "2026-07-28" => Self::V2026_07_28,
            _ => return None,
        })
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::V2024_11_05 => "2024-11-05",
            Self::V2025_03_26 => "2025-03-26",
            Self::V2025_06_18 => "2025-06-18",
            Self::V2025_11_25 => "2025-11-25",
            Self::V2026_07_28 => "2026-07-28",
        }
    }

    /// Version and capabilities travel with every request; no handshake.
    pub(crate) fn is_stateless(self) -> bool {
        self >= Self::V2026_07_28
    }

    /// Tool annotations exist from 2025-03-26.
    pub(crate) fn has_annotations(self) -> bool {
        self >= Self::V2025_03_26
    }

    /// `title`, `outputSchema` and `structuredContent` exist from 2025-06-18.
    pub(crate) fn has_structured_output(self) -> bool {
        self >= Self::V2025_06_18
    }

    /// Only 2025-03-26 requires receivers to accept JSON-RPC batches.
    pub(crate) fn has_batching(self) -> bool {
        self == Self::V2025_03_26
    }
}

/// A protocol-level failure, sent as a JSON-RPC error response.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, message)
    }

    pub(crate) fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub(crate) fn method_not_found(method: &str) -> Self {
        Self::new(
            METHOD_NOT_FOUND,
            format!("Method not found: {}", crate::sanitize::inline(method, 100)),
        )
    }

    pub(crate) fn unsupported_version(requested: &str) -> Self {
        Self {
            code: UNSUPPORTED_PROTOCOL_VERSION,
            message: "Unsupported protocol version".into(),
            data: Some(json!({
                "supported": SUPPORTED_PROTOCOL_VERSIONS,
                "requested": crate::sanitize::inline(requested, 100),
            })),
        }
    }
}

/// A success response.
pub(crate) fn result_message(id: &Value, result: Map<String, Value>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// An error response. `id` is omitted when the request's id could not be
/// read, which is how the MCP schema models that case (the member is
/// optional, never null).
pub(crate) fn error_message(id: Option<&Value>, error: &RpcError) -> Value {
    let mut body = Map::new();
    body.insert("code".into(), json!(error.code));
    body.insert("message".into(), json!(error.message));
    if let Some(data) = &error.data {
        body.insert("data".into(), data.clone());
    }
    let mut message = Map::new();
    message.insert("jsonrpc".into(), json!("2.0"));
    if let Some(id) = id {
        message.insert("id".into(), id.clone());
    }
    message.insert("error".into(), Value::Object(body));
    Value::Object(message)
}

/// Request ids are strings or integers; `null`, fractions and anything else
/// are not ids.
pub(crate) fn is_valid_id(id: &Value) -> bool {
    match id {
        Value::String(_) => true,
        Value::Number(number) => number.is_i64() || number.is_u64(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_round_trip_and_order_by_date() {
        for text in SUPPORTED_PROTOCOL_VERSIONS {
            assert_eq!(Revision::parse(text).unwrap().as_str(), text);
        }
        assert!(Revision::parse("2023-01-01").is_none());
        assert!(Revision::V2026_07_28 > Revision::V2025_11_25);
        assert!(!Revision::LATEST_HANDSHAKE.is_stateless());
        assert!(Revision::V2026_07_28.is_stateless());
        assert!(!Revision::V2024_11_05.has_annotations());
        assert!(!Revision::V2025_03_26.has_structured_output());
        assert!(Revision::V2025_03_26.has_batching());
        assert!(!Revision::V2025_06_18.has_batching());
    }

    #[test]
    fn an_unreadable_id_is_left_out_of_the_error() {
        let message = error_message(None, &RpcError::new(PARSE_ERROR, "Parse error"));
        assert!(message.get("id").is_none());
        assert_eq!(message["error"]["code"], PARSE_ERROR);
        let with_id = error_message(Some(&json!(7)), &RpcError::invalid_params("x"));
        assert_eq!(with_id["id"], 7);
    }

    #[test]
    fn only_strings_and_integers_are_ids() {
        assert!(is_valid_id(&json!("a")));
        assert!(is_valid_id(&json!(-3)));
        assert!(!is_valid_id(&json!(1.5)));
        assert!(!is_valid_id(&Value::Null));
        assert!(!is_valid_id(&json!([1])));
    }
}
