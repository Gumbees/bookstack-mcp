//! Envelope/meta types for `/remember/v1/{resource}/{action}` responses.
//!
//! v1.0.0 reintroduces the `/remember/v1/...` namespace alongside the
//! `/briefing/v1/read` surface. The shape and helpers are identical to the
//! briefing's envelope — re-exported here so handlers in this module never
//! have to reach into `crate::briefing::envelope`.

pub use crate::briefing::envelope::{build_meta, TIMEZONE_REFRESH_SECS};

use serde_json::{json, Value};

/// Discriminated error codes — clients can switch on these.
#[derive(Clone, Copy, Debug)]
pub enum ErrorCode {
    InvalidArgument,
    UnknownAction,
    UnknownResource,
    InternalError,
    /// The action is well-formed and the user is authenticated, but
    /// settings on this instance opt out of the operation. Most common
    /// cause: `journaling_enabled = false` on a secondary MCP wired
    /// alongside a primary; the AI is being told "don't journal here."
    Forbidden,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::UnknownAction => "unknown_action",
            Self::UnknownResource => "unknown_resource",
            Self::InternalError => "internal_error",
            Self::Forbidden => "forbidden",
        }
    }
}

/// Build the standard error envelope. `meta` is whatever `build_meta` would
/// return for the call — failures still carry the time/setup context so the
/// AI can react without a follow-up briefing call.
pub fn error_envelope(code: ErrorCode, message: impl Into<String>, meta: Value) -> Value {
    json!({
        "ok": false,
        "error": {
            "code": code.as_str(),
            "message": message.into(),
        },
        "meta": meta,
    })
}

/// Build the standard success envelope.
pub fn ok_envelope(data: Value, meta: Value) -> Value {
    json!({
        "ok": true,
        "data": data,
        "meta": meta,
    })
}
