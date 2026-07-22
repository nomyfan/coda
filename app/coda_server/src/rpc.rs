//! JSON-RPC 2.0 envelope for the single WebSocket wire.
//!
//! This is the *only* hand-written protocol code on the server: it decodes a raw
//! inbound frame into a request / notification / structurally-invalid call, and
//! frames outgoing results, errors, and server pushes. It is domain-agnostic —
//! it deals in `serde_json::Value` at its seam, and the dispatcher
//! (`bin/server.rs`) deserializes `params` into the per-method type.
//!
//! The decode/encode asymmetry is intentional: decode must distinguish a
//! `-32700` parse error (the frame isn't JSON) from a `-32600` invalid request
//! (valid JSON that isn't a well-formed call), recovering the `id` when present;
//! encode cannot fail structurally.

use serde::Serialize;
use serde_json::{Value, json};

/// A JSON-RPC id echoed verbatim on the response. Per the spec an id is a string
/// or a number (we never mint them — the client owns id allocation); we keep it
/// as a `Value` so it round-trips exactly.
pub type RpcId = Value;

/// One classified inbound frame. `params` stays a `Value`; the dispatcher
/// deserializes it per method.
#[derive(Debug)]
pub enum Incoming {
    /// A call with an `id`; the dispatcher must answer with exactly one reply.
    Request {
        id: RpcId,
        method: String,
        params: Value,
    },
    /// A call without an `id`; run for effect, never answered.
    Notification { method: String, params: Value },
    /// Not a well-formed call. A `-32700` parse error (not JSON: `id` is `None`,
    /// answered with id `null`) or a `-32600` invalid request (JSON but not a
    /// well-formed `"2.0"` call: `id` echoed when it's a usable value, else
    /// `null`). Always answered, never dropped.
    Invalid { id: Option<RpcId>, error: RpcError },
}

/// A JSON-RPC error object. `data` is omitted from the wire when `None`.
#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// Standard JSON-RPC codes.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// App codes, in the JSON-RPC-reserved implementation-defined server-error block
// (`-32000..-32099`). Frozen: the wire carries only the number and the client
// mirrors this table (`protocol.ts` `RpcCode`).
pub const SESSION_BUSY: i32 = -32001;
pub const NOT_OWNER: i32 = -32002;
pub const SESSION_NOT_LIVE: i32 = -32003;
pub const MODEL_SWITCH_WHILE_RUNNING: i32 = -32004;
pub const MODEL_LOCKED: i32 = -32005;
pub const UNKNOWN_WORKSPACE: i32 = -32010;
pub const INVALID_SESSION_ID: i32 = -32011;
pub const INVALID_MODEL_SELECTION: i32 = -32012;
pub const SESSION_NOT_FOUND: i32 = -32013;
pub const OPEN_FAILED: i32 = -32020;
pub const DELETE_FAILED: i32 = -32021;
pub const RENAME_FAILED: i32 = -32022;
pub const ALLOW_PATTERN_FAILED: i32 = -32030;

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Carry a human-readable detail string in `data` (surfaced to the client).
    pub fn with_detail(code: i32, message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(Value::String(detail.into())),
        }
    }

    fn parse_error() -> Self {
        Self::new(PARSE_ERROR, "parse error")
    }

    fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("unknown method: {method}"))
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
}

/// A fully-built outgoing envelope the transport serializes verbatim.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct RpcOutgoing(Value);

/// How a frame's `id` member is classified, kept separate from an absent member
/// so a malformed id isn't silently downgraded to a notification.
enum ReqId {
    /// No `id` member at all — the one shape that makes a frame a notification.
    Absent,
    /// A spec-legal id (string, number, or the discouraged-but-allowed `null`),
    /// echoed verbatim on the reply.
    Present(RpcId),
    /// An `id` of an unsupported type (bool / array / object): a malformed
    /// request, not a notification. Can't be echoed, so its error id is `null`.
    Unsupported,
}

impl ReqId {
    /// The id to echo on an error for this frame: the present value (even
    /// `null`), or `None` for an absent or unusable id.
    fn echoable(&self) -> Option<RpcId> {
        match self {
            ReqId::Present(id) => Some(id.clone()),
            _ => None,
        }
    }
}

/// Classify one raw inbound frame. Never fails: a bad frame becomes an
/// `Invalid` the dispatcher answers. A frame is a *request* only when it is a
/// well-formed `"2.0"` call carrying a spec-legal `id`; a *notification* only
/// when it is a well-formed `"2.0"` call with **no** `id` member. Everything
/// else — non-JSON (`-32700`), a non-object, a wrong/absent `jsonrpc`, a
/// missing `method`, or an unsupported `id` type — is `Invalid` (`-32600`), so
/// a malformed frame can never slip through and drive a method.
pub fn decode(frame: &str) -> Incoming {
    let value: Value = match serde_json::from_str(frame) {
        Ok(value) => value,
        // Not JSON at all — `-32700`, no id to echo.
        Err(_) => {
            return Incoming::Invalid {
                id: None,
                error: RpcError::parse_error(),
            };
        }
    };

    let Value::Object(mut object) = value else {
        // Valid JSON but not an object: it can't be a call.
        return Incoming::Invalid {
            id: None,
            error: RpcError::invalid_request("request must be a JSON object"),
        };
    };

    // Classify the id up front so it can be echoed on any error below.
    let id = match object.get("id") {
        None => ReqId::Absent,
        Some(value @ (Value::String(_) | Value::Number(_) | Value::Null)) => {
            ReqId::Present(value.clone())
        }
        Some(_) => ReqId::Unsupported,
    };

    // The `jsonrpc` version must be exactly "2.0"; a missing or mismatched
    // version is not a valid call and must not execute a method.
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Incoming::Invalid {
            id: id.echoable(),
            error: RpcError::invalid_request(
                "missing or unsupported 'jsonrpc' version (expected \"2.0\")",
            ),
        };
    }

    let Some(method) = object
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Incoming::Invalid {
            id: id.echoable(),
            error: RpcError::invalid_request("missing or non-string 'method'"),
        };
    };

    let params = object.remove("params").unwrap_or(Value::Null);

    match id {
        ReqId::Absent => Incoming::Notification { method, params },
        ReqId::Present(id) => Incoming::Request { id, method, params },
        // An id member of an unsupported type makes this a malformed request,
        // not a notification — reject it rather than run the method unanswered.
        ReqId::Unsupported => Incoming::Invalid {
            id: None,
            error: RpcError::invalid_request("'id' must be a string, number, or null"),
        },
    }
}

/// Frame a successful result, echoing the request id verbatim.
impl<T> From<(RpcId, &T)> for RpcOutgoing
where
    T: Serialize + ?Sized,
{
    fn from((id, payload): (RpcId, &T)) -> Self {
        RpcOutgoing(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": to_value(payload),
        }))
    }
}

/// Frame a failure without an echoable request id (for example, a parse error).
impl From<RpcError> for RpcOutgoing {
    fn from(error: RpcError) -> Self {
        RpcOutgoing(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": error,
        }))
    }
}

/// Frame a failure for a valid request, echoing its id verbatim.
impl From<(RpcId, RpcError)> for RpcOutgoing {
    fn from((id, error): (RpcId, RpcError)) -> Self {
        RpcOutgoing(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": error,
        }))
    }
}

/// Frame a failure whose request id may not be recoverable.
impl From<(Option<RpcId>, RpcError)> for RpcOutgoing {
    fn from((id, error): (Option<RpcId>, RpcError)) -> Self {
        match id {
            Some(id) => (id, error).into(),
            None => error.into(),
        }
    }
}

/// Frame a server-initiated notification (no id, never answered).
pub fn notify(method: &str, params: &impl Serialize) -> RpcOutgoing {
    RpcOutgoing(json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": to_value(params),
    }))
}

/// Serialize a payload to a `Value`. Our payloads are plain derive-`Serialize`
/// structs with string keys, so this is infallible in practice; a failure means
/// a programming error in a payload type, not a runtime condition.
fn to_value<T: Serialize + ?Sized>(payload: &T) -> Value {
    serde_json::to_value(payload).expect("rpc payload serializes to a JSON value")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_object(outgoing: &RpcOutgoing) -> &serde_json::Map<String, Value> {
        outgoing.0.as_object().expect("outgoing is an object")
    }

    #[test]
    fn decode_request_recovers_id_method_params() {
        let frame = r#"{"jsonrpc":"2.0","id":7,"method":"open_session","params":{"a":1}}"#;
        match decode(frame) {
            Incoming::Request { id, method, params } => {
                assert_eq!(id, json!(7));
                assert_eq!(method, "open_session");
                assert_eq!(params, json!({"a":1}));
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn decode_notification_has_no_id() {
        let frame = r#"{"jsonrpc":"2.0","method":"task","params":{}}"#;
        assert!(matches!(decode(frame), Incoming::Notification { method, .. } if method == "task"));
    }

    #[test]
    fn decode_missing_params_defaults_to_null() {
        let frame = r#"{"jsonrpc":"2.0","id":1,"method":"list_workspaces"}"#;
        match decode(frame) {
            Incoming::Request { params, .. } => assert_eq!(params, Value::Null),
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn decode_non_json_is_parse_error_with_null_id() {
        match decode("not json{") {
            Incoming::Invalid { id, error } => {
                assert!(id.is_none());
                assert_eq!(error.code, PARSE_ERROR);
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn decode_non_object_json_is_invalid_request() {
        match decode("[1,2,3]") {
            Incoming::Invalid { id, error } => {
                assert!(id.is_none());
                assert_eq!(error.code, INVALID_REQUEST);
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn decode_object_without_method_is_invalid_request_with_recovered_id() {
        match decode(r#"{"jsonrpc":"2.0","id":"abc"}"#) {
            Incoming::Invalid { id, error } => {
                assert_eq!(id, Some(json!("abc")));
                assert_eq!(error.code, INVALID_REQUEST);
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn decode_missing_jsonrpc_is_invalid_request_and_never_runs_a_method() {
        // A frame that omits `jsonrpc` must not execute a method (here a
        // mutating one); it is answered with `-32600`, correlated to its id.
        match decode(r#"{"id":5,"method":"delete_session","params":{}}"#) {
            Incoming::Invalid { id, error } => {
                assert_eq!(id, Some(json!(5)));
                assert_eq!(error.code, INVALID_REQUEST);
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn decode_wrong_jsonrpc_version_is_invalid_request() {
        match decode(r#"{"jsonrpc":"1.0","id":1,"method":"list_workspaces"}"#) {
            Incoming::Invalid { id, error } => {
                assert_eq!(id, Some(json!(1)));
                assert_eq!(error.code, INVALID_REQUEST);
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn decode_null_id_is_a_request_not_a_notification() {
        // `null` is a discouraged but spec-legal request id: it must be answered
        // (with id `null`), not silently downgraded to a fire-and-forget call.
        match decode(r#"{"jsonrpc":"2.0","id":null,"method":"open_session","params":{}}"#) {
            Incoming::Request { id, method, .. } => {
                assert_eq!(id, Value::Null);
                assert_eq!(method, "open_session");
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn decode_unsupported_id_type_is_invalid_request_not_a_notification() {
        // An `id` of an unsupported type is a malformed request; it must be
        // rejected (`-32600`, id `null`), not run as an unanswered notification.
        match decode(r#"{"jsonrpc":"2.0","id":{"x":1},"method":"task","params":{}}"#) {
            Incoming::Invalid { id, error } => {
                assert!(id.is_none());
                assert_eq!(error.code, INVALID_REQUEST);
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn result_frame_echoes_id_and_carries_result() {
        let out: RpcOutgoing = (json!(3), &json!({"ok": true})).into();
        let obj = as_object(&out);
        assert_eq!(obj["jsonrpc"], json!("2.0"));
        assert_eq!(obj["id"], json!(3));
        assert_eq!(obj["result"], json!({"ok": true}));
        assert!(!obj.contains_key("error"));
    }

    #[test]
    fn error_frame_with_null_id_omits_absent_data() {
        let out: RpcOutgoing = RpcError::parse_error().into();
        let obj = as_object(&out);
        assert_eq!(obj["id"], Value::Null);
        let err = obj["error"].as_object().expect("error object");
        assert_eq!(err["code"], json!(PARSE_ERROR));
        assert!(!err.contains_key("data"), "absent data must be omitted");
    }

    #[test]
    fn error_frame_carries_detail_data() {
        let out: RpcOutgoing = (
            json!(9),
            RpcError::with_detail(ALLOW_PATTERN_FAILED, "allow pattern failed", "disk full"),
        )
            .into();
        let obj = as_object(&out);
        assert_eq!(obj["id"], json!(9));
        assert_eq!(obj["error"]["data"], json!("disk full"));
    }

    #[test]
    fn optional_error_id_uses_available_id() {
        let with_id: RpcOutgoing = (
            Some(json!(7)),
            RpcError::new(INVALID_REQUEST, "invalid request"),
        )
            .into();
        assert_eq!(as_object(&with_id)["id"], json!(7));

        let without_id: RpcOutgoing =
            (None, RpcError::new(INVALID_REQUEST, "invalid request")).into();
        assert_eq!(as_object(&without_id)["id"], Value::Null);
    }

    #[test]
    fn notify_frame_has_method_and_no_id() {
        let out = notify("event", &json!({"n": 1}));
        let obj = as_object(&out);
        assert_eq!(obj["method"], json!("event"));
        assert_eq!(obj["params"], json!({"n": 1}));
        assert!(!obj.contains_key("id"));
    }
}
