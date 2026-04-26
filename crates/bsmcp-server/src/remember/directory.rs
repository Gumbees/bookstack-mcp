//! `/remember/v1/directory/read` — discover globally-shared resources.
//!
//! Body:
//!   - `kind` (required): `"identities"` | `"user_journals"`
//!
//! Returns the books on the relevant global shelf, with the calling user's
//! BookStack permissions enforced naturally by the API.

use serde_json::{json, Value};

use super::envelope::ErrorCode;
use super::{Context, Outcome};

pub async fn read(ctx: &Context) -> Outcome {
    let kind = match ctx.body_str("kind") {
        Some(k) => k,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "kind field is required (\"identities\" or \"user_journals\")",
                Some("kind"),
            );
        }
    };

    let globals = match ctx.db.get_global_settings().await {
        Ok(g) => g,
        Err(e) => return Outcome::error(ErrorCode::InternalError, e, None),
    };

    let shelf_id = match kind.as_str() {
        "identities" => match globals.hive_shelf_id {
            Some(id) => id,
            None => {
                return Outcome::error(
                    ErrorCode::SettingsNotConfigured,
                    "global hive_shelf_id is not set",
                    Some("hive_shelf_id"),
                );
            }
        },
        "user_journals" => match globals.user_journals_shelf_id {
            Some(id) => id,
            None => {
                return Outcome::error(
                    ErrorCode::SettingsNotConfigured,
                    "global user_journals_shelf_id is not set",
                    Some("user_journals_shelf_id"),
                );
            }
        },
        other => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                format!("kind must be \"identities\" or \"user_journals\", got {other:?}"),
                Some("kind"),
            );
        }
    };

    let shelf = match ctx.client.get_shelf(shelf_id).await {
        Ok(s) => s,
        Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
    };

    let books: Vec<Value> = shelf
        .get("books")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(|b| {
            json!({
                "book_id": b.get("id").cloned().unwrap_or(Value::Null),
                "name": b.get("name").cloned().unwrap_or(Value::Null),
                "slug": b.get("slug").cloned().unwrap_or(Value::Null),
                "url": b.get("url").cloned().unwrap_or(Value::Null),
            })
        }).collect())
        .unwrap_or_default();

    Outcome::ok(json!({
        "kind": kind,
        "shelf_id": shelf_id,
        "shelf_name": shelf.get("name").cloned().unwrap_or(Value::Null),
        "count": books.len(),
        "books": books,
    }))
}
