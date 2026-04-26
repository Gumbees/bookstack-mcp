//! Auto-provision helpers for /settings.
//!
//! Given a request to "create if missing" some Hive structure, these helpers
//! call BookStack create endpoints with sensible defaults from the naming
//! module and gracefully surface permission errors as `ProvisionResult::Denied`
//! rather than crashing the settings save.

use bsmcp_common::bookstack::{BookStackClient, ContentType};
use serde_json::json;

use super::naming::NamedResource;

/// Outcome of one auto-provision attempt.
#[derive(Clone, Debug)]
pub enum ProvisionResult {
    Created { id: i64, name: String },
    Denied { reason: String },
    Failed { reason: String },
}

impl ProvisionResult {
    pub fn id(&self) -> Option<i64> {
        if let Self::Created { id, .. } = self { Some(*id) } else { None }
    }

    pub fn human(&self, resource: NamedResource) -> String {
        match self {
            Self::Created { id, name } => format!(
                "Created {} \"{name}\" (id={id})", resource.default_name()
            ),
            Self::Denied { reason } => format!(
                "Cannot create {}: permission denied. {reason}", resource.default_name()
            ),
            Self::Failed { reason } => format!(
                "Failed to create {}: {reason}", resource.default_name()
            ),
        }
    }
}

fn classify_error(err: &str) -> ProvisionResult {
    let lower = err.to_lowercase();
    if lower.contains("403") || lower.contains("forbidden") || lower.contains("permission") {
        ProvisionResult::Denied { reason: err.to_string() }
    } else {
        ProvisionResult::Failed { reason: err.to_string() }
    }
}

/// Create a shelf with the given resource's defaults.
pub async fn create_shelf(
    client: &BookStackClient,
    resource: NamedResource,
) -> ProvisionResult {
    match client.create_shelf(resource.default_name(), resource.default_description()).await {
        Ok(v) => match v.get("id").and_then(|i| i.as_i64()) {
            Some(id) => ProvisionResult::Created {
                id,
                name: resource.default_name().to_string(),
            },
            None => ProvisionResult::Failed { reason: "create_shelf returned no id".to_string() },
        },
        Err(e) => classify_error(&e),
    }
}

/// Create a book and (optionally) attach it to a shelf.
pub async fn create_book(
    client: &BookStackClient,
    resource: NamedResource,
    parent_shelf_id: Option<i64>,
) -> ProvisionResult {
    let book = match client.create_book(resource.default_name(), resource.default_description()).await {
        Ok(v) => v,
        Err(e) => return classify_error(&e),
    };
    let book_id = match book.get("id").and_then(|i| i.as_i64()) {
        Some(id) => id,
        None => return ProvisionResult::Failed { reason: "create_book returned no id".to_string() },
    };

    if let Some(shelf_id) = parent_shelf_id {
        // Append book to shelf — fetch existing books, then update the shelf with the new list.
        if let Ok(shelf) = client.get_shelf(shelf_id).await {
            let mut existing: Vec<i64> = shelf
                .get("books")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|b| b.get("id").and_then(|i| i.as_i64())).collect())
                .unwrap_or_default();
            if !existing.contains(&book_id) {
                existing.push(book_id);
            }
            // Best-effort — don't fail the provision if the shelf update fails.
            let payload = json!({ "books": existing });
            if let Err(e) = client.update_shelf(shelf_id, &payload).await {
                eprintln!("Provision: created book {book_id} but couldn't attach to shelf {shelf_id}: {e}");
            }
        }
    }

    ProvisionResult::Created {
        id: book_id,
        name: resource.default_name().to_string(),
    }
}

/// Lock a piece of Hive content (shelf/book/chapter/page) so only the Admin
/// role plus the page owner can edit it; everyone else gets read-only.
///
/// BookStack default for newly-created content is to inherit permissions from
/// its parent. This helper opts the content out of inheritance and writes an
/// explicit role-permission entry for Admin (full access) plus a fallback that
/// permits view-only for everyone else. Page owners always retain edit access
/// regardless of role permissions — that's BookStack's built-in behaviour.
///
/// Best-effort: BookStack returns 403 if the calling user can't manage
/// permissions on the item. Logged + swallowed so it never blocks the parent
/// save flow.
pub async fn lock_to_admin_only(
    client: &BookStackClient,
    content_type: ContentType,
    content_id: i64,
    admin_role_id: i64,
) {
    let payload = json!({
        "role_permissions": [
            {
                "role_id": admin_role_id,
                "view": true,
                "create": true,
                "update": true,
                "delete": true,
            }
        ],
        "fallback_permissions": {
            "inheriting": false,
            "view": true,
            "create": false,
            "update": false,
            "delete": false,
        }
    });
    let label = match content_type {
        ContentType::Page => "page",
        ContentType::Chapter => "chapter",
        ContentType::Book => "book",
        ContentType::Shelf => "shelf",
    };
    if let Err(e) = client
        .update_content_permissions(content_type, content_id, &payload)
        .await
    {
        eprintln!(
            "Provision: failed to lock {label} {content_id} to admin-only (non-fatal): {e}"
        );
    }
}

/// Create a page inside a book or chapter, with the given markdown body.
#[allow(dead_code)] // reserved for future identity/whoami auto-creation paths
pub async fn create_page(
    client: &BookStackClient,
    resource: NamedResource,
    parent_book_id: Option<i64>,
    parent_chapter_id: Option<i64>,
    markdown: &str,
) -> ProvisionResult {
    let mut payload = json!({
        "name": resource.default_name(),
        "markdown": markdown,
    });
    if let Some(id) = parent_chapter_id {
        payload["chapter_id"] = json!(id);
    } else if let Some(id) = parent_book_id {
        payload["book_id"] = json!(id);
    } else {
        return ProvisionResult::Failed {
            reason: "create_page requires a book_id or chapter_id".to_string(),
        };
    }
    match client.create_page(&payload).await {
        Ok(v) => match v.get("id").and_then(|i| i.as_i64()) {
            Some(id) => ProvisionResult::Created {
                id,
                name: resource.default_name().to_string(),
            },
            None => ProvisionResult::Failed { reason: "create_page returned no id".to_string() },
        },
        Err(e) => classify_error(&e),
    }
}
