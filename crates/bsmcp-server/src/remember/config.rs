//! `remember/config` — read/write the per-user `config_extras` slot and
//! manage the briefing's setup-nudge snooze.
//!
//! `read`  — returns the current `config_extras` map plus the
//!            `setup_nudge_dismissed_until` timestamp.
//! `write` — accepts `{config: {key: value, ...}}` (string values), merging
//!           into existing extras. Pass an explicit `null` for a key to
//!           delete it. Other UserSettings fields are not touched.
//! `dismiss_setup_nudge` — accepts `{days: int}`, stamps a future Unix
//!           timestamp on `settings_nudge_dismissed_until`. Mirrors the
//!           legacy `dismiss_setup_nudge` MCP tool.

use serde_json::{json, Value};

use super::envelope::ErrorCode;
use super::{Context, DispatchResult};

const MIN_DAYS: i64 = 1;
const MAX_DAYS: i64 = 365;

pub async fn read(ctx: &Context) -> DispatchResult {
    Ok(json!({
        "config": ctx.settings.config_extras,
        "setup_nudge_dismissed_until": ctx.settings.settings_nudge_dismissed_until,
    }))
}

pub async fn write(ctx: &Context) -> DispatchResult {
    let patch = ctx.body.get("config").or_else(|| ctx.body.get("patch"));
    let patch = match patch {
        Some(Value::Object(map)) => map.clone(),
        Some(_) => {
            return Err((
                ErrorCode::InvalidArgument,
                "`config` must be a JSON object of string values".to_string(),
            ));
        }
        None => return Err((
            ErrorCode::InvalidArgument,
            "Missing `config` (object of key/value pairs to merge into config_extras)".to_string(),
        )),
    };

    let mut settings = ctx
        .db
        .get_user_settings(&ctx.token_id_hash)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_user_settings failed: {e}")))?
        .unwrap_or_default();

    for (key, value) in patch {
        match value {
            Value::Null => {
                settings.config_extras.remove(&key);
            }
            Value::String(s) => {
                settings.config_extras.insert(key, s);
            }
            // Tolerate non-string scalars by stringifying. Reject objects /
            // arrays — config_extras is a flat K/V slot.
            Value::Bool(b) => {
                settings.config_extras.insert(key, b.to_string());
            }
            Value::Number(n) => {
                settings.config_extras.insert(key, n.to_string());
            }
            Value::Object(_) | Value::Array(_) => {
                return Err((
                    ErrorCode::InvalidArgument,
                    format!("config_extras values must be scalars (got object/array for key `{key}`)"),
                ));
            }
        }
    }

    ctx.db
        .save_user_settings(&ctx.token_id_hash, &settings)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("save_user_settings failed: {e}")))?;

    Ok(json!({ "config": settings.config_extras }))
}

pub async fn dismiss_setup_nudge(ctx: &Context) -> DispatchResult {
    let days = ctx.body_i64("days").ok_or_else(|| (
        ErrorCode::InvalidArgument,
        "Missing required argument: days".to_string(),
    ))?;
    let days = days.clamp(MIN_DAYS, MAX_DAYS);

    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let dismissed_until = now_unix + days * 86400;

    let mut settings = ctx
        .db
        .get_user_settings(&ctx.token_id_hash)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_user_settings failed: {e}")))?
        .unwrap_or_default();
    settings.settings_nudge_dismissed_until = Some(dismissed_until);
    ctx.db
        .save_user_settings(&ctx.token_id_hash, &settings)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("save_user_settings failed: {e}")))?;

    let until_human = chrono::DateTime::<chrono::Utc>::from_timestamp(dismissed_until, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| dismissed_until.to_string());

    Ok(json!({
        "dismissed_until": dismissed_until,
        "until_human": until_human,
        "days": days,
    }))
}
