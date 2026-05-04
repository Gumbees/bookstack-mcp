//! `remember/directory read` — serve the in-memory `DirectoryService`
//! snapshot verbatim under the standard remember envelope.
//!
//! There's no `write` action: the directory is derived state, rebuilt from
//! IndexDb on webhook events. This handler is a thin shim around
//! `DirectoryService::current()`.

use serde_json::{json, Value};

use super::envelope::ErrorCode;
use super::{Context, DispatchResult};

pub async fn read(ctx: &Context) -> DispatchResult {
    let svc = ctx.directory.as_ref().ok_or_else(|| {
        (
            ErrorCode::InternalError,
            "Directory service not wired into remember dispatcher".to_string(),
        )
    })?;
    let snapshot = svc.current().await;
    let snapshot_json = serde_json::to_value(&*snapshot).unwrap_or(Value::Null);
    Ok(json!({ "directory": snapshot_json }))
}
