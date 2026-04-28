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
                return Outcome::settings_not_configured(
                    "hive_shelf_id",
                    "global hive_shelf_id is not set — admin must configure it before identities can be listed",
                );
            }
        },
        "user_journals" => match globals.user_journals_shelf_id {
            Some(id) => id,
            None => {
                return Outcome::settings_not_configured(
                    "user_journals_shelf_id",
                    "global user_journals_shelf_id is not set — admin must configure it before user journals can be listed",
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

    // Phase 5b: try the index first for the books-on-shelf list. Falls back
    // to the live BookStack `get_shelf` call on miss/error/empty so the
    // call stays correct before the worker's first walk and on
    // postgres-stub deployments.
    if let Ok(indexed_books) = ctx.index_db.list_indexed_books_by_shelf(shelf_id).await {
        if !indexed_books.is_empty() {
            // Resolve shelf metadata from the index too if we have it.
            let shelf_name = match ctx.index_db.get_indexed_shelf(shelf_id).await {
                Ok(Some(s)) => Value::String(s.name),
                _ => Value::Null,
            };
            let books: Vec<Value> = indexed_books
                .into_iter()
                .map(|b| {
                    json!({
                        "book_id": b.book_id,
                        "name": b.name,
                        "slug": b.slug,
                        "url": Value::Null, // index doesn't store url; UI can derive from slug
                    })
                })
                .collect();
            return Outcome::ok(json!({
                "kind": kind,
                "shelf_id": shelf_id,
                "shelf_name": shelf_name,
                "count": books.len(),
                "books": books,
            }));
        }
    }

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
