//! Generic handler shared by all collection resources (journal, collage,
//! opportunities, etc). Each resource is a small struct implementing
//! [`CollectionResource`]; the handler does the heavy lifting once.

use serde_json::{json, Value};

use bsmcp_common::settings::{GlobalSettings, UserSettings};

use super::envelope::{ErrorCode, RememberWarning};
use super::frontmatter;
use super::journal_archive;
use super::provision;
use super::section;
use super::{Context, Outcome};

/// Where a resource's pages live. Either a book (collage, shared_collage,
/// user_journal, legacy journal) or a chapter (Phase 6 journal — pages live
/// flat inside the per-identity Journal chapter, year-rollover sweep moves
/// stale entries into `Journal Archive - {YEAR}` chapters scoped within the
/// same Identity book).
#[derive(Clone, Copy, Debug)]
pub enum CollectionParent {
    Book(i64),
    Chapter(i64),
}

impl CollectionParent {
    /// Returns the parent's book id when book-parented, `None` otherwise.
    /// Used by code paths that only make sense for books — shelf-pin
    /// reattachment, the search `{in_book:N}` filter, list/find walks.
    pub fn book_id(&self) -> Option<i64> {
        match self {
            CollectionParent::Book(id) => Some(*id),
            CollectionParent::Chapter(_) => None,
        }
    }

    /// Find a page by exact-name match within this parent's scope. Books
    /// walk every page in the book; chapters scope strictly to pages
    /// inside that chapter.
    pub async fn find_page_by_name(
        &self,
        client: &bsmcp_common::bookstack::BookStackClient,
        name: &str,
    ) -> Result<Option<i64>, String> {
        let row = match self {
            CollectionParent::Book(id) => client.find_page_in_book(*id, name).await?,
            CollectionParent::Chapter(id) => client.find_page_in_chapter(*id, name).await?,
        };
        Ok(row.and_then(|p| p.get("id").and_then(|v| v.as_i64())))
    }

    /// List the most-recently-updated pages within this parent's scope,
    /// up to `limit`. Used by the read-without-key list path and search.
    pub async fn list_pages_by_updated(
        &self,
        client: &bsmcp_common::bookstack::BookStackClient,
        limit: usize,
    ) -> Result<Vec<Value>, String> {
        match self {
            CollectionParent::Book(id) => client.list_book_pages_by_updated(*id, limit).await,
            CollectionParent::Chapter(id) => client.list_chapter_pages_by_updated(*id, limit).await,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum KeyKind {
    /// YYYY-MM-DD — used by journals.
    Date,
    /// Slugified topic/name — used by collage.
    Slug,
}

/// Trait implemented by each collection resource.
pub trait CollectionResource: Send + Sync {
    fn name(&self) -> &'static str;

    /// UserSettings field name for the parent book — used by the
    /// settings_not_configured error so the AI gets a real field name in
    /// `error.fix` instead of the resource name.
    fn setting_field(&self) -> &'static str;

    fn parent(&self, settings: &UserSettings) -> Option<CollectionParent>;

    fn key_kind(&self) -> KeyKind;

    /// For resources organized by sub-chapters within a book (e.g., journals
    /// split into YYYY-MM chapters). Returns the chapter name a page with the
    /// given key should live in. Default: no sub-chapter.
    fn sub_chapter_for_key(&self, _key: &str) -> Option<String> {
        None
    }

    /// Convert the natural key into the BookStack page name. Default: the key itself.
    fn key_to_page_name(&self, key: &str) -> String {
        key.to_string()
    }

    /// Whether this resource permits writes. Some (e.g., user_journal) may be read-only
    /// from the AI's perspective in a future revision; for v1 all are writable.
    fn writable(&self) -> bool {
        true
    }

    /// When set, every successful write to this resource ensures the parent
    /// book lives on the named global shelf — books that have drifted off
    /// the shelf are reattached on each write. Currently only `user_journal`
    /// uses this; other collections aren't shelf-pinned.
    fn shelf_pin(&self, _globals: &GlobalSettings) -> Option<i64> {
        None
    }

    /// When set, the resource is the per-identity Journal: pages live in a
    /// Journal chapter inside the Identity book, with year-rollover sweep
    /// moving stale-year pages into `Journal Archive - {YEAR}` chapters.
    /// Returns the Identity book id so the sweep / archive lookup can scope
    /// chapter operations strictly within that book.
    ///
    /// Default: None (every other collection is book-parented, no archive
    /// rollover, no chapter scope).
    fn journal_archive_context(&self, _s: &UserSettings) -> Option<i64> {
        None
    }
}

pub async fn handle(
    resource: &dyn CollectionResource,
    action: &str,
    ctx: &Context,
) -> Outcome {
    let parent = match resource.parent(&ctx.settings) {
        Some(p) => p,
        None => {
            return Outcome::settings_not_configured(
                resource.setting_field(),
                format!(
                    "{} requires `{}` to be set. The collection has no parent book to read or write.",
                    resource.name(),
                    resource.setting_field()
                ),
            );
        }
    };

    match action {
        "read" => handle_read(resource, parent, ctx).await,
        "write" => {
            if !resource.writable() {
                return Outcome::error(
                    ErrorCode::InvalidArgument,
                    format!("{} is read-only", resource.name()),
                    None,
                );
            }
            handle_write(resource, parent, ctx).await
        }
        "append" => {
            if !resource.writable() {
                return Outcome::error(
                    ErrorCode::InvalidArgument,
                    format!("{} is read-only", resource.name()),
                    None,
                );
            }
            handle_append(resource, parent, ctx).await
        }
        "update_section" | "append_section" => {
            if !resource.writable() {
                return Outcome::error(
                    ErrorCode::InvalidArgument,
                    format!("{} is read-only", resource.name()),
                    None,
                );
            }
            handle_section_op(resource, parent, ctx, action == "append_section").await
        }
        "search" => handle_search(resource, parent, ctx).await,
        "delete" => {
            if !resource.writable() {
                return Outcome::error(
                    ErrorCode::InvalidArgument,
                    format!("{} is read-only", resource.name()),
                    None,
                );
            }
            handle_delete(resource, parent, ctx).await
        }
        _ => Outcome::error(
            ErrorCode::UnknownAction,
            format!("Unknown action {action} on {}", resource.name()),
            None,
        ),
    }
}

// --- READ ---

async fn handle_read(
    resource: &dyn CollectionResource,
    parent: CollectionParent,
    ctx: &Context,
) -> Outcome {
    // Specific page by id wins over key wins over list.
    if let Some(page_id) = ctx.body_i64("id") {
        return read_one_by_id(page_id, ctx).await;
    }
    if let Some(key) = ctx.body_str("key") {
        return read_one_by_key(resource, parent, &key, ctx).await;
    }
    list_pages(resource, parent, ctx).await
}

async fn read_one_by_id(page_id: i64, ctx: &Context) -> Outcome {
    match ctx.client.get_page(page_id).await {
        Ok(page) => {
            let markdown = page.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
            let body = frontmatter::strip(markdown).to_string();
            Outcome::ok_with_target(
                json!({
                    "id": page_id,
                    "name": page.get("name").cloned().unwrap_or(Value::Null),
                    "markdown": body,
                    "raw_markdown": markdown,
                    "book_id": page.get("book_id").cloned().unwrap_or(Value::Null),
                    "chapter_id": page.get("chapter_id").cloned().unwrap_or(Value::Null),
                    "updated_at": page.get("updated_at").cloned().unwrap_or(Value::Null),
                }),
                Some(page_id),
                None,
            )
        }
        Err(e) => Outcome::error(ErrorCode::NotFound, e, Some("id")),
    }
}

async fn read_one_by_key(
    resource: &dyn CollectionResource,
    parent: CollectionParent,
    key: &str,
    ctx: &Context,
) -> Outcome {
    let page_name = resource.key_to_page_name(key);

    // Per-identity journal: route past-year reads into the matching archive
    // chapter. The current journal chapter only ever holds the current
    // year's pages (year-rollover sweep moves stale entries on every write).
    if let (Some(identity_book_id), CollectionParent::Chapter(journal_chapter_id)) =
        (resource.journal_archive_context(&ctx.settings), parent)
    {
        match journal_archive::resolve_read_chapter_for_key(
            key,
            &ctx.settings,
            identity_book_id,
            journal_chapter_id,
            ctx,
        )
        .await
        {
            Ok(Some(chapter_id)) => {
                let resolved = CollectionParent::Chapter(chapter_id);
                return match find_page_by_name(resolved, &page_name, ctx).await {
                    Ok(Some(page_id)) => read_one_by_id(page_id, ctx).await,
                    Ok(None) => Outcome::error(
                        ErrorCode::NotFound,
                        format!("No {} page named {page_name:?}", resource.name()),
                        Some("key"),
                    ),
                    Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
                };
            }
            Ok(None) => {
                // No archive chapter for that year — page can't exist.
                return Outcome::error(
                    ErrorCode::NotFound,
                    format!(
                        "No {} archive chapter exists for the year in key {key:?}",
                        resource.name()
                    ),
                    Some("key"),
                );
            }
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        }
    }

    match find_page_by_name(parent, &page_name, ctx).await {
        Ok(Some(page_id)) => read_one_by_id(page_id, ctx).await,
        Ok(None) => Outcome::error(
            ErrorCode::NotFound,
            format!("No {} page named {page_name:?}", resource.name()),
            Some("key"),
        ),
        Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
    }
}

async fn list_pages(
    resource: &dyn CollectionResource,
    parent: CollectionParent,
    ctx: &Context,
) -> Outcome {
    let limit = ctx.body_count("limit", 25, 200);
    let offset = ctx.body.get("offset").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
    // Pull (limit + offset) most-recently-updated pages from the parent
    // (book or chapter, depending on the resource) and skip to the offset.
    // Goes through `CollectionParent::list_pages_by_updated` so we get
    // database `updated_at` ordering, not search-relevance.
    match parent.list_pages_by_updated(&ctx.client, limit + offset).await {
        Ok(rows) => {
            let pages: Vec<Value> = rows
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|p| json!({
                    "id": p.get("id").cloned().unwrap_or(Value::Null),
                    "name": p.get("name").cloned().unwrap_or(Value::Null),
                    "url": p.get("url").cloned().unwrap_or(Value::Null),
                    "updated_at": p.get("updated_at").cloned().unwrap_or(Value::Null),
                }))
                .collect();
            Outcome::ok(json!({
                "resource": resource.name(),
                "count": pages.len(),
                "pages": pages,
            }))
        }
        Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
    }
}

async fn find_page_by_name(
    parent: CollectionParent,
    name: &str,
    ctx: &Context,
) -> Result<Option<i64>, String> {
    // Goes through `get_book`/`get_chapter` + flatten — never `search` — so
    // the parent scope is honored. Exact-name match (case-insensitive) is
    // done client-side.
    parent.find_page_by_name(&ctx.client, name).await
}

/// Used only by `handle_search`, which prepends a positive keyword term —
/// so the filter is honored. Listing/lookup paths must NOT use this; go
/// through `CollectionParent::list_pages_by_updated` /
/// `CollectionParent::find_page_by_name` instead.
fn parent_filter(parent: CollectionParent) -> String {
    match parent {
        CollectionParent::Book(id) => format!("{{in_book:{id}}}"),
        CollectionParent::Chapter(id) => format!("{{in_chapter:{id}}}"),
    }
}

// --- WRITE ---

async fn handle_write(
    resource: &dyn CollectionResource,
    parent: CollectionParent,
    ctx: &Context,
) -> Outcome {
    // Self-healing shelf pin: if the resource is shelf-pinned (currently
    // user_journal) AND the parent is a book, reattach it to the configured
    // shelf on every write. Idempotent — `ensure_book_on_shelf` is a no-op
    // when the book is already there. Chapter-parented resources don't have
    // a shelf to pin themselves to (they live inside an Identity book that
    // sits on the Hive shelf, locked separately).
    let globals = ctx.db.get_global_settings().await.unwrap_or_default();
    if let (Some(shelf_id), Some(book_id)) = (resource.shelf_pin(&globals), parent.book_id()) {
        provision::ensure_book_on_shelf(&ctx.client, book_id, shelf_id).await;
    }

    // Year-rollover sweep for the per-identity journal. Runs before every
    // write so any stale-year pages are moved into their archive chapter
    // before the new entry lands. Idempotent and best-effort — failures
    // log but don't block the user's write.
    if let (Some(identity_book_id), CollectionParent::Chapter(journal_chapter_id)) =
        (resource.journal_archive_context(&ctx.settings), parent)
    {
        match journal_archive::year_rollover_sweep(journal_chapter_id, identity_book_id, ctx).await {
            Ok(0) => {}
            Ok(n) => eprintln!("year_rollover_sweep: archived {n} stale page(s) before {} write", resource.name()),
            Err(e) => eprintln!("year_rollover_sweep failed (non-fatal, write continues): {e}"),
        }
    }

    let body_text = match ctx.body_str("body") {
        Some(b) => b,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "body field is required for write",
                Some("body"),
            );
        }
    };

    // Resolve key/id. id-targeted updates skip key resolution; otherwise we
    // derive a page name from key (auto-keying journals to today's date when
    // both id and key are absent).
    let id_arg = ctx.body_i64("id");
    let key_arg = ctx.body_str("key").or_else(|| match resource.key_kind() {
        KeyKind::Date => Some(frontmatter::today_iso_date()),
        _ => None,
    });

    if id_arg.is_none() && key_arg.is_none() {
        return Outcome::error(
            ErrorCode::InvalidArgument,
            "either id or key is required for write",
            Some("key"),
        );
    }

    let normalized_key = key_arg.as_deref().map(|k| match resource.key_kind() {
        KeyKind::Slug => frontmatter::slugify(k),
        KeyKind::Date => k.to_string(),
    });

    let page_name = normalized_key
        .as_deref()
        .map(|k| resource.key_to_page_name(k))
        .unwrap_or_default();

    // Find existing page (by id or by key lookup).
    let existing_id = if let Some(id) = id_arg {
        Some(id)
    } else if !page_name.is_empty() {
        match find_page_by_name(parent, &page_name, ctx).await {
            Ok(maybe) => maybe,
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        }
    } else {
        None
    };

    let frontmatter_block = frontmatter::build(
        &ctx.settings,
        &ctx.trace_id,
        resource.name(),
        normalized_key.as_deref(),
        existing_id,
    );
    let full_body = format!("{frontmatter_block}{body_text}");

    if let Some(id) = existing_id {
        // Update existing page.
        let payload = if page_name.is_empty() {
            json!({ "markdown": full_body })
        } else {
            json!({ "name": page_name, "markdown": full_body })
        };
        match ctx.client.update_page(id, &payload).await {
            Ok(updated) => Outcome::ok_with_target(
                build_write_response(&updated, "updated"),
                Some(id),
                normalized_key.clone(),
            ),
            Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
        }
    } else {
        // Create new page. For book-parented resources with sub-chapters
        // (journals), we need to find/create the sub-chapter first.
        let target = match resolve_create_target(resource, parent, normalized_key.as_deref(), ctx).await {
            Ok(t) => t,
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        };
        let mut payload = json!({ "name": page_name, "markdown": full_body });
        match target {
            CreateTarget::Book(id) => { payload["book_id"] = json!(id); }
            CreateTarget::Chapter(id) => { payload["chapter_id"] = json!(id); }
        }
        match ctx.client.create_page(&payload).await {
            Ok(created) => {
                let new_id = created.get("id").and_then(|v| v.as_i64());
                Outcome::ok_with_target(
                    build_write_response(&created, "created"),
                    new_id,
                    normalized_key.clone(),
                )
            }
            Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
        }
    }
}

enum CreateTarget {
    Book(i64),
    Chapter(i64),
}

async fn resolve_create_target(
    resource: &dyn CollectionResource,
    parent: CollectionParent,
    key: Option<&str>,
    ctx: &Context,
) -> Result<CreateTarget, String> {
    match parent {
        CollectionParent::Chapter(id) => {
            // Chapter-parented resources land all writes directly in the
            // chapter; sub-chapter splitting doesn't apply.
            Ok(CreateTarget::Chapter(id))
        }
        CollectionParent::Book(book_id) => {
            // Book-parented: if the resource splits by sub-chapter (e.g.
            // user_journal's monthly chapters), find or create it.
            let sub_chapter_name = key.and_then(|k| resource.sub_chapter_for_key(k));
            if let Some(chapter_name) = sub_chapter_name {
                let chapter_id = find_or_create_chapter(book_id, &chapter_name, ctx).await?;
                Ok(CreateTarget::Chapter(chapter_id))
            } else {
                Ok(CreateTarget::Book(book_id))
            }
        }
    }
}

async fn find_or_create_chapter(
    book_id: i64,
    name: &str,
    ctx: &Context,
) -> Result<i64, String> {
    // Look up via `get_book` + chapter list — never `search` — so we always
    // see existing chapters and never create duplicates.
    if let Some(existing) = ctx.client.find_chapter_in_book(book_id, name).await? {
        if let Some(id) = existing.get("id").and_then(|v| v.as_i64()) {
            return Ok(id);
        }
    }
    // Create with a generated description (server requires non-empty).
    let description = format!("Auto-created by /remember for {name}");
    let created = ctx.client.create_chapter(book_id, name, &description).await?;
    created
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "create_chapter returned no id".to_string())
}

// --- APPEND (non-destructive write) ---
//
// Add `body` to the existing page at `key` (or create the page if missing).
// Optional `timestamp=true` prefixes the appended chunk with a local time
// marker (`## HH:MM TZ`) so multi-append-per-day journals produce a readable
// timeline. The original `written_at` frontmatter field is preserved if the
// page already exists; provenance for the append shows up as `last_appended_at`
// + `append_count` (parsed-and-incremented from the existing frontmatter).

async fn handle_append(
    resource: &dyn CollectionResource,
    parent: CollectionParent,
    ctx: &Context,
) -> Outcome {
    let globals = ctx.db.get_global_settings().await.unwrap_or_default();
    if let (Some(shelf_id), Some(book_id)) = (resource.shelf_pin(&globals), parent.book_id()) {
        provision::ensure_book_on_shelf(&ctx.client, book_id, shelf_id).await;
    }
    // Year-rollover sweep — same as handle_write. Append paths can also
    // create pages, so the sweep needs to run here too.
    if let (Some(identity_book_id), CollectionParent::Chapter(journal_chapter_id)) =
        (resource.journal_archive_context(&ctx.settings), parent)
    {
        match journal_archive::year_rollover_sweep(journal_chapter_id, identity_book_id, ctx).await {
            Ok(0) => {}
            Ok(n) => eprintln!("year_rollover_sweep: archived {n} stale page(s) before {} append", resource.name()),
            Err(e) => eprintln!("year_rollover_sweep failed (non-fatal, append continues): {e}"),
        }
    }

    let body_text = match ctx.body_str("body") {
        Some(b) => b,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "body field is required for append",
                Some("body"),
            );
        }
    };
    let timestamp = ctx
        .body
        .get("timestamp")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let id_arg = ctx.body_i64("id");
    let key_arg = ctx.body_str("key").or_else(|| match resource.key_kind() {
        KeyKind::Date => Some(frontmatter::today_iso_date()),
        _ => None,
    });

    if id_arg.is_none() && key_arg.is_none() {
        return Outcome::error(
            ErrorCode::InvalidArgument,
            "either id or key is required for append",
            Some("key"),
        );
    }

    let normalized_key = key_arg.as_deref().map(|k| match resource.key_kind() {
        KeyKind::Slug => frontmatter::slugify(k),
        KeyKind::Date => k.to_string(),
    });
    let page_name = normalized_key
        .as_deref()
        .map(|k| resource.key_to_page_name(k))
        .unwrap_or_default();

    let existing_id = if let Some(id) = id_arg {
        Some(id)
    } else if !page_name.is_empty() {
        match find_page_by_name(parent, &page_name, ctx).await {
            Ok(maybe) => maybe,
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        }
    } else {
        None
    };

    // Build the append chunk body (with optional timestamp prefix). Trimming
    // the user's body lets the prefix line stand on its own and keeps the
    // separator newlines clean.
    let chunk = if timestamp {
        let stamp = local_time_marker(&ctx.settings);
        format!("## {stamp}\n\n{body}\n", body = body_text.trim())
    } else {
        format!("{}\n", body_text.trim())
    };

    if let Some(id) = existing_id {
        // Read existing page body, parse the prior frontmatter to preserve
        // `written_at` and increment `append_count`, then write back the
        // concatenated body with refreshed frontmatter.
        let page = match ctx.client.get_page(id).await {
            Ok(p) => p,
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        };
        let existing_md = page
            .get("markdown")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let prior_existing_body = frontmatter::strip(existing_md);
        let prior = parse_provenance(existing_md);

        let new_body = if prior_existing_body.trim().is_empty() {
            chunk
        } else {
            format!("{}\n\n{}", prior_existing_body.trim_end(), chunk)
        };
        let new_append_count = prior.append_count.unwrap_or(0) + 1;

        let frontmatter_block = build_append_frontmatter(
            &ctx.settings,
            &ctx.trace_id,
            resource.name(),
            normalized_key.as_deref(),
            Some(id),
            prior.written_at.as_deref(),
            new_append_count,
        );
        let full_body = format!("{frontmatter_block}{new_body}");
        let payload = if page_name.is_empty() {
            json!({ "markdown": full_body })
        } else {
            json!({ "name": page_name, "markdown": full_body })
        };
        match ctx.client.update_page(id, &payload).await {
            Ok(updated) => Outcome::ok_with_target(
                build_write_response(&updated, "appended"),
                Some(id),
                normalized_key.clone(),
            ),
            Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
        }
    } else {
        // No existing page — fall through to create-with-new-body. Reuses the
        // standard `write` create path (find/create sub-chapter etc.) since
        // append-into-nothing is identical to write-with-this-body.
        let target = match resolve_create_target(resource, parent, normalized_key.as_deref(), ctx).await {
            Ok(t) => t,
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        };
        let frontmatter_block = build_append_frontmatter(
            &ctx.settings,
            &ctx.trace_id,
            resource.name(),
            normalized_key.as_deref(),
            None,
            None,
            1,
        );
        let full_body = format!("{frontmatter_block}{chunk}");
        let mut payload = json!({ "name": page_name, "markdown": full_body });
        match target {
            CreateTarget::Book(id) => { payload["book_id"] = json!(id); }
            CreateTarget::Chapter(id) => { payload["chapter_id"] = json!(id); }
        }
        match ctx.client.create_page(&payload).await {
            Ok(created) => {
                let new_id = created.get("id").and_then(|v| v.as_i64());
                Outcome::ok_with_target(
                    build_write_response(&created, "created"),
                    new_id,
                    normalized_key.clone(),
                )
            }
            Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
        }
    }
}

// --- UPDATE_SECTION / APPEND_SECTION (section-aware writes) ---

async fn handle_section_op(
    resource: &dyn CollectionResource,
    parent: CollectionParent,
    ctx: &Context,
    is_append: bool,
) -> Outcome {
    let action_label = if is_append { "append_section" } else { "update_section" };
    let section_name = match ctx.body_str("section") {
        Some(s) => s,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                format!("section field is required for {action_label}"),
                Some("section"),
            );
        }
    };
    let body_text = match ctx.body_str("body") {
        Some(b) => b,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                format!("body field is required for {action_label}"),
                Some("body"),
            );
        }
    };

    let id_arg = ctx.body_i64("id");
    let key_arg = ctx.body_str("key").or_else(|| match resource.key_kind() {
        KeyKind::Date => Some(frontmatter::today_iso_date()),
        _ => None,
    });
    if id_arg.is_none() && key_arg.is_none() {
        return Outcome::error(
            ErrorCode::InvalidArgument,
            format!("either id or key is required for {action_label}"),
            Some("key"),
        );
    }
    let normalized_key = key_arg.as_deref().map(|k| match resource.key_kind() {
        KeyKind::Slug => frontmatter::slugify(k),
        KeyKind::Date => k.to_string(),
    });
    let page_name = normalized_key
        .as_deref()
        .map(|k| resource.key_to_page_name(k))
        .unwrap_or_default();

    let existing_id = if let Some(id) = id_arg {
        Some(id)
    } else if !page_name.is_empty() {
        match find_page_by_name(parent, &page_name, ctx).await {
            Ok(maybe) => maybe,
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        }
    } else {
        None
    };

    let (existing_body, prior) = if let Some(id) = existing_id {
        match ctx.client.get_page(id).await {
            Ok(page) => {
                let raw = page.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
                (frontmatter::strip(raw).to_string(), parse_provenance(raw))
            }
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        }
    } else {
        // No existing page — section op effectively creates the page with
        // a single section.
        (String::new(), Provenance::default())
    };

    let new_body = if is_append {
        section::append_to_section(&existing_body, &section_name, &body_text)
    } else {
        section::replace_section(&existing_body, &section_name, &body_text)
    };

    let frontmatter_block = build_section_frontmatter(
        &ctx.settings,
        &ctx.trace_id,
        resource.name(),
        normalized_key.as_deref(),
        existing_id,
        prior.written_at.as_deref(),
        prior.append_count,
        &section_name,
    );
    let full_body = format!("{frontmatter_block}{new_body}");

    if let Some(id) = existing_id {
        let payload = if page_name.is_empty() {
            json!({ "markdown": full_body })
        } else {
            json!({ "name": page_name, "markdown": full_body })
        };
        match ctx.client.update_page(id, &payload).await {
            Ok(updated) => Outcome::ok_with_target(
                build_write_response(&updated, action_label),
                Some(id),
                normalized_key.clone(),
            ),
            Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
        }
    } else {
        let target = match resolve_create_target(resource, parent, normalized_key.as_deref(), ctx).await {
            Ok(t) => t,
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        };
        let mut payload = json!({ "name": page_name, "markdown": full_body });
        match target {
            CreateTarget::Book(id) => { payload["book_id"] = json!(id); }
            CreateTarget::Chapter(id) => { payload["chapter_id"] = json!(id); }
        }
        match ctx.client.create_page(&payload).await {
            Ok(created) => {
                let new_id = created.get("id").and_then(|v| v.as_i64());
                Outcome::ok_with_target(
                    build_write_response(&created, "created"),
                    new_id,
                    normalized_key.clone(),
                )
            }
            Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
        }
    }
}

// --- Frontmatter parsing + extended builders ---
//
// Phase 2's new actions need to carry a few extra provenance fields beyond
// what `frontmatter::build` emits — and to do that without rewriting the
// existing `write` flow, we parse selected fields out of the prior body
// here and re-stamp.

#[derive(Default, Debug)]
struct Provenance {
    written_at: Option<String>,
    append_count: Option<i64>,
}

fn parse_provenance(markdown: &str) -> Provenance {
    let trimmed = markdown.trim_start();
    if !trimmed.starts_with("---") {
        return Provenance::default();
    }
    let mut out = Provenance::default();
    let mut iter = trimmed.lines();
    iter.next(); // opening ---
    for line in iter {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            match key {
                "written_at" => out.written_at = Some(value.to_string()),
                "append_count" => out.append_count = value.parse().ok(),
                _ => {}
            }
        }
    }
    out
}

fn build_append_frontmatter(
    settings: &bsmcp_common::settings::UserSettings,
    trace_id: &str,
    resource: &str,
    key: Option<&str>,
    supersedes_page: Option<i64>,
    preserved_written_at: Option<&str>,
    new_append_count: i64,
) -> String {
    let mut out = String::from("---\n");
    if let Some(name) = &settings.ai_identity_name {
        out.push_str(&format!("written_by: {}\n", yaml_quote(name)));
    }
    if let Some(ouid) = &settings.ai_identity_ouid {
        out.push_str(&format!("ai_identity_ouid: {}\n", yaml_quote(ouid)));
    }
    if let Some(user_id) = &settings.user_id {
        out.push_str(&format!("user_id: {}\n", yaml_quote(user_id)));
    }
    let written_at = preserved_written_at
        .map(|s| s.to_string())
        .unwrap_or_else(frontmatter::now_iso_utc);
    out.push_str(&format!("written_at: {}\n", yaml_quote(&written_at)));
    out.push_str(&format!("last_appended_at: {}\n", yaml_quote(&frontmatter::now_iso_utc())));
    out.push_str(&format!("append_count: {new_append_count}\n"));
    out.push_str(&format!("trace_id: {}\n", yaml_quote(trace_id)));
    out.push_str(&format!("resource: {}\n", yaml_quote(resource)));
    if let Some(k) = key {
        out.push_str(&format!("key: {}\n", yaml_quote(k)));
    }
    if let Some(p) = supersedes_page {
        out.push_str(&format!("supersedes_page: {p}\n"));
    }
    out.push_str("---\n\n");
    out
}

#[allow(clippy::too_many_arguments)]
fn build_section_frontmatter(
    settings: &bsmcp_common::settings::UserSettings,
    trace_id: &str,
    resource: &str,
    key: Option<&str>,
    supersedes_page: Option<i64>,
    preserved_written_at: Option<&str>,
    preserved_append_count: Option<i64>,
    last_section: &str,
) -> String {
    let mut out = String::from("---\n");
    if let Some(name) = &settings.ai_identity_name {
        out.push_str(&format!("written_by: {}\n", yaml_quote(name)));
    }
    if let Some(ouid) = &settings.ai_identity_ouid {
        out.push_str(&format!("ai_identity_ouid: {}\n", yaml_quote(ouid)));
    }
    if let Some(user_id) = &settings.user_id {
        out.push_str(&format!("user_id: {}\n", yaml_quote(user_id)));
    }
    let written_at = preserved_written_at
        .map(|s| s.to_string())
        .unwrap_or_else(frontmatter::now_iso_utc);
    out.push_str(&format!("written_at: {}\n", yaml_quote(&written_at)));
    out.push_str(&format!(
        "last_section_update_at: {}\n",
        yaml_quote(&frontmatter::now_iso_utc())
    ));
    out.push_str(&format!("last_updated_section: {}\n", yaml_quote(last_section)));
    if let Some(c) = preserved_append_count {
        out.push_str(&format!("append_count: {c}\n"));
    }
    out.push_str(&format!("trace_id: {}\n", yaml_quote(trace_id)));
    out.push_str(&format!("resource: {}\n", yaml_quote(resource)));
    if let Some(k) = key {
        out.push_str(&format!("key: {}\n", yaml_quote(k)));
    }
    if let Some(p) = supersedes_page {
        out.push_str(&format!("supersedes_page: {p}\n"));
    }
    out.push_str("---\n\n");
    out
}

/// Conservative YAML scalar quoting — same logic as `frontmatter::build`.
fn yaml_quote(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars().any(|c| matches!(c, ':' | '#' | '\'' | '"' | '\n' | '{' | '}' | '[' | ']' | ','))
        || matches!(s, "true" | "false" | "null" | "yes" | "no" | "~");
    if needs_quote {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

/// Format the user's local time as `HH:MM TZ` for the optional timestamp
/// prefix on `append`. Falls back to the configured timezone, then UTC.
fn local_time_marker(settings: &bsmcp_common::settings::UserSettings) -> String {
    use chrono::Utc;
    let now_utc = Utc::now();
    if let Some(tz_name) = settings.timezone.as_deref() {
        if let Ok(tz) = tz_name.parse::<chrono_tz::Tz>() {
            let local = now_utc.with_timezone(&tz);
            return format!("{}", local.format("%H:%M %Z"));
        }
    }
    format!("{} UTC", now_utc.format("%H:%M"))
}

fn build_write_response(page: &Value, action: &str) -> Value {
    json!({
        "action": action,
        "id": page.get("id").cloned().unwrap_or(Value::Null),
        "name": page.get("name").cloned().unwrap_or(Value::Null),
        "url": page.get("url").cloned().unwrap_or(Value::Null),
        "updated_at": page.get("updated_at").cloned().unwrap_or(Value::Null),
    })
}

// --- SEARCH ---

async fn handle_search(
    resource: &dyn CollectionResource,
    parent: CollectionParent,
    ctx: &Context,
) -> Outcome {
    let query = match ctx.body_str("query") {
        Some(q) => q,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "query field is required for search",
                Some("query"),
            );
        }
    };

    let limit = ctx.body_count("limit", 10, 50);
    let include_archives = ctx
        .body
        .get("include_archives")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Build the keyword filter. Default scope is the configured parent;
    // for the per-identity journal with `include_archives=true`, walk the
    // current Journal chapter PLUS every `Journal Archive - *` chapter
    // inside the same Identity book.
    let mut search_parents: Vec<CollectionParent> = vec![parent];
    if include_archives {
        if let Some(identity_book_id) = resource.journal_archive_context(&ctx.settings) {
            for archive_id in
                journal_archive::list_archive_chapter_ids(identity_book_id, ctx).await
            {
                search_parents.push(CollectionParent::Chapter(archive_id));
            }
        }
    }

    // Aggregate keyword hits from each scope. BookStack search doesn't
    // accept multiple `{in_chapter:X}` filters on one query, so we issue
    // one call per scope and merge.
    let mut keyword_hits: Vec<Value> = Vec::new();
    for p in &search_parents {
        let kw_query = format!("{query} {{type:page}} {}", parent_filter(*p));
        let hits = match ctx.client.search(&kw_query, 1, limit as i64).await {
            Ok(v) => v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default(),
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        };
        keyword_hits.extend(hits);
    }
    keyword_hits.truncate(limit);

    // Optionally augment with semantic — filter by parent post-hoc since the
    // semantic backend doesn't accept book/chapter filters yet.
    let mut warnings = Vec::new();
    let semantic_hits: Vec<Value> = if let Some(sem) = &ctx.semantic {
        let user_roles = sem
            .resolve_user_roles(&ctx.token_id_hash, ctx.settings.bookstack_user_id, &ctx.client)
            .await;
        match sem
            .search(
                &query,
                limit * 4,
                0.45,
                true,
                false,
                &ctx.client,
                None,
                user_roles.as_deref(),
            )
            .await
        {
            Ok(v) => {
                let results = v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
                results
                    .into_iter()
                    .filter(|hit| search_parents.iter().any(|p| matches_parent(hit, *p)))
                    .take(limit)
                    .collect()
            }
            Err(e) => {
                warnings.push(RememberWarning::new(
                    "semantic_unavailable",
                    format!("Semantic search failed: {e} — keyword results only"),
                ));
                Vec::new()
            }
        }
    } else {
        warnings.push(RememberWarning::new(
            "semantic_disabled",
            "Server has BSMCP_SEMANTIC_SEARCH=false — keyword results only",
        ));
        Vec::new()
    };

    let mut outcome = Outcome::ok(json!({
        "resource": resource.name(),
        "query": query,
        "keyword_hits": keyword_hits,
        "semantic_hits": semantic_hits,
    }));
    for w in warnings {
        outcome = outcome.with_warning(w);
    }
    outcome
}

fn matches_parent(hit: &Value, parent: CollectionParent) -> bool {
    match parent {
        CollectionParent::Book(id) => {
            hit.get("book_id").and_then(|v| v.as_i64()) == Some(id)
        }
        CollectionParent::Chapter(id) => {
            hit.get("chapter_id").and_then(|v| v.as_i64()) == Some(id)
        }
    }
}

// --- DELETE (soft) ---

async fn handle_delete(
    resource: &dyn CollectionResource,
    parent: CollectionParent,
    ctx: &Context,
) -> Outcome {
    let id = if let Some(id) = ctx.body_i64("id") {
        id
    } else if let Some(key) = ctx.body_str("key") {
        let page_name = resource.key_to_page_name(&key);
        match find_page_by_name(parent, &page_name, ctx).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return Outcome::error(
                    ErrorCode::NotFound,
                    format!("No {} page named {page_name:?}", resource.name()),
                    Some("key"),
                );
            }
            Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
        }
    } else {
        return Outcome::error(
            ErrorCode::InvalidArgument,
            "id or key is required for delete",
            None,
        );
    };

    // Fetch current page so we can preserve the body and rename.
    let page = match ctx.client.get_page(id).await {
        Ok(p) => p,
        Err(e) => return Outcome::error(ErrorCode::NotFound, e, Some("id")),
    };
    let current_name = page.get("name").and_then(|v| v.as_str()).unwrap_or("page").to_string();
    let current_md = page.get("markdown").and_then(|v| v.as_str()).unwrap_or("").to_string();

    // Build a tombstone: prepend new frontmatter that records the soft delete.
    let body_only = frontmatter::strip(&current_md).to_string();
    let mut tombstone_fm = frontmatter::build(
        &ctx.settings,
        &ctx.trace_id,
        resource.name(),
        ctx.body_str("key").as_deref(),
        Some(id),
    );
    let reason = ctx.body_str("reason").unwrap_or_else(|| "soft-delete via /remember".to_string());
    tombstone_fm = tombstone_fm.replace(
        "---\n\n",
        &format!("deleted: true\ndelete_reason: {}\n---\n\n", yaml_inline(&reason)),
    );

    let new_name = if current_name.starts_with("[archived] ") {
        current_name.clone()
    } else {
        format!("[archived] {current_name}")
    };

    let payload = json!({
        "name": new_name,
        "markdown": format!("{tombstone_fm}{body_only}"),
    });
    match ctx.client.update_page(id, &payload).await {
        Ok(_) => Outcome::ok_with_target(
            json!({
                "action": "soft_deleted",
                "id": id,
                "previous_name": current_name,
                "archived_name": new_name,
            }),
            Some(id),
            ctx.body_str("key"),
        ),
        Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
    }
}

fn yaml_inline(s: &str) -> String {
    if s.contains('\n') || s.contains('"') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

// --- The 4 collection resource impls ---

pub mod resources {
    use super::*;

    pub struct Journal;
    impl CollectionResource for Journal {
        fn name(&self) -> &'static str { "journal" }
        fn setting_field(&self) -> &'static str { "ai_identity_journal_chapter_id" }
        fn parent(&self, s: &UserSettings) -> Option<CollectionParent> {
            // Phase 6: chapter-parented. Pages live flat inside the
            // current-year Journal chapter; year-rollover sweep moves stale
            // entries into 'Journal Archive - {YEAR}' chapters.
            s.ai_identity_journal_chapter_id.map(CollectionParent::Chapter)
        }
        fn key_kind(&self) -> KeyKind { KeyKind::Date }
        fn journal_archive_context(&self, s: &UserSettings) -> Option<i64> {
            // The archive rollover and read-by-key dispatch need the parent
            // Identity book id to scope chapter lookup. Returns None when
            // the identity book isn't configured — the journal action then
            // surfaces a settings_not_configured error before writing.
            s.ai_identity_book_id
        }
    }

    pub struct Collage;
    impl CollectionResource for Collage {
        fn name(&self) -> &'static str { "collage" }
        fn setting_field(&self) -> &'static str { "ai_collage_book_id" }
        fn parent(&self, s: &UserSettings) -> Option<CollectionParent> {
            s.ai_collage_book_id.map(CollectionParent::Book)
        }
        fn key_kind(&self) -> KeyKind { KeyKind::Slug }
    }

    pub struct SharedCollage;
    impl CollectionResource for SharedCollage {
        fn name(&self) -> &'static str { "shared_collage" }
        fn setting_field(&self) -> &'static str { "ai_shared_collage_book_id" }
        fn parent(&self, s: &UserSettings) -> Option<CollectionParent> {
            s.ai_shared_collage_book_id.map(CollectionParent::Book)
        }
        fn key_kind(&self) -> KeyKind { KeyKind::Slug }
    }

    pub struct UserJournal;
    impl CollectionResource for UserJournal {
        fn name(&self) -> &'static str { "user_journal" }
        fn setting_field(&self) -> &'static str { "user_journal_book_id" }
        fn parent(&self, s: &UserSettings) -> Option<CollectionParent> {
            s.user_journal_book_id.map(CollectionParent::Book)
        }
        fn key_kind(&self) -> KeyKind { KeyKind::Date }
        fn sub_chapter_for_key(&self, key: &str) -> Option<String> {
            if key.len() >= 7 { Some(key[..7].to_string()) } else { None }
        }
        fn shelf_pin(&self, globals: &GlobalSettings) -> Option<i64> {
            // Force every user-journal write to reattach the book to the
            // global User Journals shelf — self-healing if the book drifts off.
            globals.user_journals_shelf_id
        }
    }
}
