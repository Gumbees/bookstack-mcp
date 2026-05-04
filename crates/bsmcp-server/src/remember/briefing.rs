//! `remember/briefing read` — thin wrapper that exposes the existing
//! briefing under the v1.0.0 namespace without duplicating logic.
//!
//! `crate::briefing::read` builds its own `{ok, data, meta}` envelope; the
//! dispatcher here re-wraps with the remember-namespace meta, so we extract
//! the inner `data` field.

use serde_json::json;

use crate::briefing;

use super::envelope::ErrorCode;
use super::{Context, DispatchResult};

pub async fn read(ctx: &Context) -> DispatchResult {
    let envelope = briefing::read(
        ctx.body.clone(),
        &ctx.token_id,
        &ctx.client,
        ctx.db.clone(),
        ctx.semantic.clone(),
    )
    .await;

    if envelope.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        return Err((
            ErrorCode::InternalError,
            envelope
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("briefing returned unexpected error envelope")
                .to_string(),
        ));
    }
    Ok(envelope.get("data").cloned().unwrap_or(json!({})))
}
