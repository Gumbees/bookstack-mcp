//! `/remember/v1/audit/read` — server-side log of every /remember write.
//!
//! Read-only. Always scoped to the calling user (token_id_hash); cannot read
//! another user's audit trail through this endpoint.

use serde_json::{json, Value};

use super::envelope::ErrorCode;
use super::{Context, Outcome};

pub async fn read(ctx: &Context) -> Outcome {
    let limit = ctx.body_count("limit", 50, 500) as i64;
    let offset = ctx.body.get("offset").and_then(|v| v.as_i64()).unwrap_or(0);
    let since = ctx.body.get("since_unix").and_then(|v| v.as_i64());

    match ctx
        .db
        .list_audit_entries(&ctx.token_id_hash, limit, offset, since)
        .await
    {
        Ok(entries) => {
            let json_entries: Vec<Value> = entries
                .iter()
                .map(|e| {
                    json!({
                        "id": e.id,
                        "ai_identity_ouid": e.ai_identity_ouid,
                        "user_id": e.user_id,
                        "resource": e.resource,
                        "action": e.action,
                        "target_page_id": e.target_page_id,
                        "target_key": e.target_key,
                        "success": e.success,
                        "error": e.error,
                        "trace_id": e.trace_id,
                        "occurred_at": e.occurred_at,
                    })
                })
                .collect();
            Outcome::ok(json!({
                "count": json_entries.len(),
                "entries": json_entries,
            }))
        }
        Err(e) => Outcome::error(ErrorCode::InternalError, e, None),
    }
}
