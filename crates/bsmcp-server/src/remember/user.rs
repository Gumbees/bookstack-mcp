//! `remember/user` — read/write the per-user `UserSettings` row.
//!
//! Reads return the full settings struct serialized to JSON (no token hash —
//! it's a row key, not part of the struct). Writes accept a partial JSON
//! object and merge into existing settings before persisting; keys not
//! provided are preserved.

use serde_json::{json, Map, Value};

use bsmcp_common::settings::UserSettings;

use super::envelope::ErrorCode;
use super::{Context, DispatchResult};

pub async fn read(ctx: &Context) -> DispatchResult {
    Ok(json!({ "user": serialize_user(&ctx.settings) }))
}

pub async fn write(ctx: &Context) -> DispatchResult {
    let patch = ctx.body.get("patch").or_else(|| ctx.body.get("user"));
    let patch = match patch {
        Some(Value::Object(map)) => map.clone(),
        Some(_) => {
            return Err((
                ErrorCode::InvalidArgument,
                "`patch` must be a JSON object".to_string(),
            ));
        }
        None => return Err((
            ErrorCode::InvalidArgument,
            "Missing `patch` (object of fields to merge into UserSettings)".to_string(),
        )),
    };

    let mut current = serialize_user(&ctx.settings);
    let Value::Object(ref mut current_map) = current else {
        return Err((
            ErrorCode::InternalError,
            "UserSettings did not serialize to a JSON object".to_string(),
        ));
    };
    for (k, v) in patch {
        current_map.insert(k, v);
    }

    let merged: UserSettings = serde_json::from_value(Value::Object(current_map.clone()))
        .map_err(|e| (ErrorCode::InvalidArgument, format!("Patch produced invalid UserSettings: {e}")))?;

    ctx.db
        .save_user_settings(&ctx.token_id_hash, &merged)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("save_user_settings failed: {e}")))?;

    Ok(json!({ "user": serialize_user(&merged) }))
}

/// Serialize UserSettings to a JSON object, suitable for response bodies.
/// Returns an empty object on the (pathologically unreachable) serialize
/// failure rather than panicking.
fn serialize_user(s: &UserSettings) -> Value {
    serde_json::to_value(s).unwrap_or_else(|_| Value::Object(Map::new()))
}
