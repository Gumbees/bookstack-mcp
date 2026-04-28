//! Auto-provision helpers for /settings.
//!
//! Given a request to "create if missing" some Hive structure, these helpers
//! call BookStack create endpoints with sensible defaults from the naming
//! module and gracefully surface permission errors as `ProvisionResult::Denied`
//! rather than crashing the settings save.

use bsmcp_common::bookstack::{BookStackClient, ContentType};
use bsmcp_common::db::IndexDb;
use serde_json::{json, Value};

use super::naming::NamedResource;

/// Outcome of one auto-provision attempt.
#[derive(Clone, Debug)]
pub enum ProvisionResult {
    /// Newly created during this call.
    Created { id: i64, name: String },
    /// Already existed; reused. Returned by the `find_or_create_*` helpers
    /// when a name-match lookup hits an existing book/chapter/page. Distinct
    /// from `Created` so the caller can distinguish "I made this" from "this
    /// was here already" — useful for dedup logging and migration plans.
    FoundExisting { id: i64, name: String },
    Denied { reason: String },
    Failed { reason: String },
}

impl ProvisionResult {
    pub fn id(&self) -> Option<i64> {
        match self {
            Self::Created { id, .. } | Self::FoundExisting { id, .. } => Some(*id),
            _ => None,
        }
    }

    pub fn human(&self, resource: NamedResource) -> String {
        match self {
            Self::Created { id, name } => format!(
                "Created {} \"{name}\" (id={id})", resource.default_name()
            ),
            Self::FoundExisting { id, name } => format!(
                "Found existing {} \"{name}\" (id={id}); reused", resource.default_name()
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
///
/// Backed by [`find_or_create_book_on_shelf`] when a shelf is configured, so
/// the /settings UI's "Create if missing" buttons reuse an existing
/// Identity/Journal/Collage book instead of duplicating it. When no shelf
/// is given, falls back to bare create.
pub async fn create_book(
    client: &BookStackClient,
    index_db: &dyn IndexDb,
    resource: NamedResource,
    parent_shelf_id: Option<i64>,
) -> ProvisionResult {
    if let Some(shelf_id) = parent_shelf_id {
        return find_or_create_book_on_shelf(
            client,
            index_db,
            shelf_id,
            resource.default_name(),
            resource.default_description(),
        )
        .await;
    }
    // No shelf — bare create. See `create_named_book` for why we don't
    // dedup globally without a known shelf to scope the lookup to.
    let book = match client.create_book(resource.default_name(), resource.default_description()).await {
        Ok(v) => v,
        Err(e) => return classify_error(&e),
    };
    let book_id = match book.get("id").and_then(|i| i.as_i64()) {
        Some(id) => id,
        None => return ProvisionResult::Failed { reason: "create_book returned no id".to_string() },
    };
    ProvisionResult::Created {
        id: book_id,
        name: resource.default_name().to_string(),
    }
}

/// Lock both journal books to owner-only access. No-op for unset IDs.
///
/// Called on every settings save (UI, probe-accept, MCP) so journals are
/// always private — whether auto-created or selected from existing books.
pub async fn lock_journal_books_to_owner(
    client: &BookStackClient,
    ai_journal_book_id: Option<i64>,
    user_journal_book_id: Option<i64>,
) {
    if let Some(id) = ai_journal_book_id {
        lock_to_owner_only(client, ContentType::Book, id).await;
    }
    if let Some(id) = user_journal_book_id {
        lock_to_owner_only(client, ContentType::Book, id).await;
    }
}

/// Lock a piece of Hive content so only its owner (and admins via system
/// permission) can access it. Used for journals — content nobody else should
/// see, including other Hive users on the same instance.
///
/// Disables inheritance, clears all role permissions, and zeroes the fallback
/// (so non-owner non-admin users get no view/create/update/delete). Owners and
/// admins keep access via BookStack's built-in semantics.
///
/// Best-effort: BookStack returns 403 if the calling user can't manage
/// permissions on the item. Logged + swallowed so it never blocks the save.
pub async fn lock_to_owner_only(
    client: &BookStackClient,
    content_type: ContentType,
    content_id: i64,
) {
    let payload = json!({
        "role_permissions": [],
        "fallback_permissions": {
            "inheriting": false,
            "view": false,
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
            "Provision: failed to lock {label} {content_id} to owner-only (non-fatal): {e}"
        );
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

/// Ensure a book sits on a shelf. Idempotent — no-op if it's already there.
/// Best-effort: failures are logged and swallowed since shelf attachment is
/// always recoverable via the BookStack UI.
pub async fn ensure_book_on_shelf(
    client: &BookStackClient,
    book_id: i64,
    shelf_id: i64,
) {
    let shelf = match client.get_shelf(shelf_id).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Provision: ensure_book_on_shelf({book_id} → {shelf_id}) — get_shelf failed (non-fatal): {e}");
            return;
        }
    };
    let mut existing: Vec<i64> = shelf
        .get("books")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|b| b.get("id").and_then(|i| i.as_i64())).collect())
        .unwrap_or_default();
    if existing.contains(&book_id) {
        return;
    }
    existing.push(book_id);
    let payload = json!({ "books": existing });
    if let Err(e) = client.update_shelf(shelf_id, &payload).await {
        eprintln!("Provision: ensure_book_on_shelf({book_id} → {shelf_id}) — update_shelf failed (non-fatal): {e}");
    }
}

/// Create a book with a personalized name (used by per-user provisioning).
///
/// Backed by [`find_or_create_book_on_shelf`] when a shelf is configured, so
/// re-runs reuse an existing book of the same name instead of duplicating.
/// When no shelf is given, falls back to bare create (no global dedup —
/// callers without a configured shelf are typically in an early-setup state
/// the briefing's setup_nudge already prompts to fix).
pub async fn create_named_book(
    client: &BookStackClient,
    index_db: &dyn IndexDb,
    name: &str,
    description: &str,
    parent_shelf_id: Option<i64>,
) -> ProvisionResult {
    if let Some(shelf_id) = parent_shelf_id {
        return find_or_create_book_on_shelf(client, index_db, shelf_id, name, description).await;
    }
    // No shelf — bare create. Without a known shelf to scope the dedup
    // lookup to, listing every book on the instance to match by name is
    // expensive and racy. Defer that path to a future helper if the use
    // case shows up.
    let book = match client.create_book(name, description).await {
        Ok(v) => v,
        Err(e) => return classify_error(&e),
    };
    let book_id = match book.get("id").and_then(|i| i.as_i64()) {
        Some(id) => id,
        None => return ProvisionResult::Failed { reason: "create_book returned no id".to_string() },
    };
    ProvisionResult::Created { id: book_id, name: name.to_string() }
}

/// Create a page with an arbitrary name + body inside a book.
///
/// Wraps [`find_or_create_page`] with a book-root parent (no chapter) so
/// re-runs reuse an existing page with the same name instead of duplicating.
pub async fn create_named_page(
    client: &BookStackClient,
    index_db: &dyn IndexDb,
    name: &str,
    parent_book_id: i64,
    markdown: &str,
) -> ProvisionResult {
    find_or_create_page(client, index_db, Some(parent_book_id), None, name, markdown).await
}

/// Create a page inside a book or chapter, with the given markdown body.
///
/// Wraps [`find_or_create_page`] using the resource's default name. Reserved
/// for callers (identity / whoami auto-creation) that key off a `NamedResource`
/// rather than a free-form name.
#[allow(dead_code)] // reserved for future identity/whoami auto-creation paths
pub async fn create_page(
    client: &BookStackClient,
    index_db: &dyn IndexDb,
    resource: NamedResource,
    parent_book_id: Option<i64>,
    parent_chapter_id: Option<i64>,
    markdown: &str,
) -> ProvisionResult {
    if parent_book_id.is_none() && parent_chapter_id.is_none() {
        return ProvisionResult::Failed {
            reason: "create_page requires a book_id or chapter_id".to_string(),
        };
    }
    find_or_create_page(
        client,
        index_db,
        parent_book_id,
        parent_chapter_id,
        resource.default_name(),
        markdown,
    )
    .await
}

// --- find-or-create helpers ---
//
// Phase 1 of the identity book restructure (RFC: identity-book-restructure):
// every direct `client.create_*` callsite in the remember module funnels
// through one of these so re-provisioning never duplicates structure that
// already exists in BookStack. Match semantics: exact case-sensitive name
// match, no fuzzy match. Description is only used at create time (existing
// books/chapters/pages keep their existing description / content).
//
// Lookup scope is always narrowed by the parent (shelf, book, or chapter)
// to keep the lookups cheap and avoid cross-identity confusion (e.g., two
// identity books on the same shelf both named "Identity").

/// Find a book on a shelf by exact name match, or create it and attach.
/// Returns `FoundExisting` on hit, `Created` on miss, or a Denied/Failed
/// outcome when BookStack rejects the request.
///
/// Phase 5c: tries the local index first (no roundtrip), then falls back
/// to a live BookStack `get_shelf` walk when the index is empty/unavailable
/// (worker hasn't run, postgres-stub deployment).
pub async fn find_or_create_book_on_shelf(
    client: &BookStackClient,
    index_db: &dyn IndexDb,
    shelf_id: i64,
    name: &str,
    description: &str,
) -> ProvisionResult {
    // 1a. Try the index — cheap, no BookStack roundtrip. A hit is
    //     authoritative; a miss falls through to the live BookStack walk
    //     in case the index is briefly stale (worker reconciles
    //     asynchronously) or empty (fresh deployment, postgres stub).
    if let Ok(books) = index_db.list_indexed_books_by_shelf(shelf_id).await {
        for book in &books {
            if book.name == name {
                return ProvisionResult::FoundExisting {
                    id: book.book_id,
                    name: name.to_string(),
                };
            }
        }
    }

    // 1b. Fall back to the live BookStack walk.
    match client.get_shelf(shelf_id).await {
        Ok(shelf) => {
            if let Some(books) = shelf.get("books").and_then(|v| v.as_array()) {
                for book in books {
                    if book.get("name").and_then(|n| n.as_str()) == Some(name) {
                        if let Some(id) = book.get("id").and_then(|i| i.as_i64()) {
                            return ProvisionResult::FoundExisting {
                                id,
                                name: name.to_string(),
                            };
                        }
                    }
                }
            }
        }
        Err(e) => return classify_error(&e),
    }

    // 2. Not found — create and attach to the shelf.
    let book = match client.create_book(name, description).await {
        Ok(v) => v,
        Err(e) => return classify_error(&e),
    };
    let book_id = match book.get("id").and_then(|i| i.as_i64()) {
        Some(id) => id,
        None => return ProvisionResult::Failed { reason: "create_book returned no id".to_string() },
    };
    ensure_book_on_shelf(client, book_id, shelf_id).await;
    ProvisionResult::Created { id: book_id, name: name.to_string() }
}

/// Find a chapter inside a book by exact name match, or create it.
///
/// Used by `identity::create` to scaffold the `Agents`, `Subagent
/// Conversations`, and `Journal` chapters, and by `remember_journal` to
/// lazy-create `Journal Archive - {YEAR}` chapters during the year-rollover
/// sweep. Match semantics: exact case-sensitive name match against
/// `chapter.name`. Description is only used at create time.
pub async fn find_or_create_chapter(
    client: &BookStackClient,
    index_db: &dyn IndexDb,
    book_id: i64,
    name: &str,
    description: &str,
) -> ProvisionResult {
    // 1a. Try the index — cheap, no BookStack roundtrip.
    if let Ok(chapters) = index_db.list_indexed_chapters_by_book(book_id).await {
        for chapter in &chapters {
            if chapter.name == name {
                return ProvisionResult::FoundExisting {
                    id: chapter.chapter_id,
                    name: name.to_string(),
                };
            }
        }
    }

    // 1b. Fall back to the live BookStack walk.
    match client.get_book(book_id).await {
        Ok(book) => {
            if let Some(contents) = book.get("contents").and_then(|v| v.as_array()) {
                for item in contents {
                    if item.get("type").and_then(|t| t.as_str()) == Some("chapter")
                        && item.get("name").and_then(|n| n.as_str()) == Some(name)
                    {
                        if let Some(id) = item.get("id").and_then(|i| i.as_i64()) {
                            return ProvisionResult::FoundExisting {
                                id,
                                name: name.to_string(),
                            };
                        }
                    }
                }
            }
        }
        Err(e) => return classify_error(&e),
    }

    // 2. Not found — create.
    let chapter = match client.create_chapter(book_id, name, description).await {
        Ok(v) => v,
        Err(e) => return classify_error(&e),
    };
    let chapter_id = match chapter.get("id").and_then(|i| i.as_i64()) {
        Some(id) => id,
        None => return ProvisionResult::Failed { reason: "create_chapter returned no id".to_string() },
    };
    ProvisionResult::Created { id: chapter_id, name: name.to_string() }
}

/// Find a page by exact name match in either a chapter (preferred) or at a
/// book's root, or create it. Caller specifies the parent via `parent_chapter_id`
/// (places the page inside the chapter) or `parent_book_id` (places it loose
/// at the book root). At least one must be `Some`.
///
/// Lookup is scoped strictly to the specified parent — a chapter lookup will
/// not pick up loose pages at the book root, and a book-root lookup will not
/// pick up pages inside chapters. This is deliberate: if a page exists with
/// the same name in a different parent, the caller almost certainly does not
/// want to reuse it.
pub async fn find_or_create_page(
    client: &BookStackClient,
    index_db: &dyn IndexDb,
    parent_book_id: Option<i64>,
    parent_chapter_id: Option<i64>,
    name: &str,
    markdown: &str,
) -> ProvisionResult {
    if parent_book_id.is_none() && parent_chapter_id.is_none() {
        return ProvisionResult::Failed {
            reason: "find_or_create_page requires book_id or chapter_id".to_string(),
        };
    }

    // 1a. Try the index — cheap, no BookStack roundtrip. Lookup is scoped
    //     to the parent: chapter pages or book-root loose pages, never both.
    let indexed = if let Some(chapter_id) = parent_chapter_id {
        index_db.list_indexed_pages_by_chapter(chapter_id).await
    } else if let Some(book_id) = parent_book_id {
        index_db.list_indexed_pages_by_book_root(book_id).await
    } else {
        Ok(Vec::new())
    };
    if let Ok(pages) = indexed {
        for page in &pages {
            if page.name == name {
                return ProvisionResult::FoundExisting {
                    id: page.page_id,
                    name: name.to_string(),
                };
            }
        }
    }

    // 1b. Fall back to the live BookStack walk in the specified parent.
    let existing_id: Option<i64> = if let Some(chapter_id) = parent_chapter_id {
        match client.get_chapter(chapter_id).await {
            Ok(chapter) => find_named_page_in_array(&chapter, "pages", name),
            Err(e) => return classify_error(&e),
        }
    } else if let Some(book_id) = parent_book_id {
        match client.get_book(book_id).await {
            Ok(book) => find_loose_page_at_book_root(&book, name),
            Err(e) => return classify_error(&e),
        }
    } else {
        unreachable!("guarded by the early-return above");
    };

    if let Some(id) = existing_id {
        return ProvisionResult::FoundExisting { id, name: name.to_string() };
    }

    // 2. Not found — create.
    let mut payload = json!({
        "name": name,
        "markdown": markdown,
    });
    if let Some(id) = parent_chapter_id {
        payload["chapter_id"] = json!(id);
    } else if let Some(id) = parent_book_id {
        payload["book_id"] = json!(id);
    }
    match client.create_page(&payload).await {
        Ok(v) => match v.get("id").and_then(|i| i.as_i64()) {
            Some(id) => ProvisionResult::Created { id, name: name.to_string() },
            None => ProvisionResult::Failed { reason: "create_page returned no id".to_string() },
        },
        Err(e) => classify_error(&e),
    }
}

/// Walk a JSON object's named array and return the first item with a matching
/// `name` field. Used by the chapter-page lookup path (`get_chapter` returns
/// `{ pages: [...] }`).
fn find_named_page_in_array(container: &Value, array_field: &str, name: &str) -> Option<i64> {
    container
        .get(array_field)?
        .as_array()?
        .iter()
        .find(|p| p.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|p| p.get("id").and_then(|i| i.as_i64()))
}

/// Find a page that lives at the book root (no chapter), matched by exact
/// name. Pages inside chapters are intentionally excluded — the book-root
/// lookup is for loose pages only.
fn find_loose_page_at_book_root(book: &Value, name: &str) -> Option<i64> {
    book.get("contents")?
        .as_array()?
        .iter()
        .find(|item| {
            item.get("type").and_then(|t| t.as_str()) == Some("page")
                && item.get("name").and_then(|n| n.as_str()) == Some(name)
        })
        .and_then(|p| p.get("id").and_then(|i| i.as_i64()))
}
