//! Shared envelope/meta/warning types for the /remember responses.

use serde_json::{json, Value};

use bsmcp_common::settings::UserSettings;

/// Discriminated error codes — clients can switch on these.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // SemanticUnavailable reserved for explicit "semantic required" calls
pub enum ErrorCode {
    SettingsNotConfigured,
    InvalidArgument,
    UnknownAction,
    BookStackError,
    NotFound,
    SemanticUnavailable,
    InternalError,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SettingsNotConfigured => "settings_not_configured",
            Self::InvalidArgument => "invalid_argument",
            Self::UnknownAction => "unknown_action",
            Self::BookStackError => "bookstack_error",
            Self::NotFound => "not_found",
            Self::SemanticUnavailable => "semantic_unavailable",
            Self::InternalError => "internal_error",
        }
    }
}

/// Soft warning attached to a successful response. Doesn't fail the call.
#[derive(Clone, Debug)]
pub struct RememberWarning {
    pub code: String,
    pub message: String,
}

impl RememberWarning {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}

pub fn build_meta(
    trace_id: &str,
    elapsed_ms: u64,
    settings: &UserSettings,
    warnings: Vec<RememberWarning>,
) -> Value {
    json!({
        "trace_id": trace_id,
        "elapsed_ms": elapsed_ms,
        "config": {
            "label": settings.label,
            "role": settings.role,
            "ai_identity_name": settings.ai_identity_name,
            "ai_identity_ouid": settings.ai_identity_ouid,
            "user_id": settings.user_id,
        },
        "warnings": warnings.iter().map(|w| json!({
            "code": w.code,
            "message": w.message,
        })).collect::<Vec<_>>(),
    })
}
