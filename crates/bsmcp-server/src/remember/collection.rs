//! Generic handler shared by all collection resources (journal, collage,
//! opportunities, etc). Each resource is a small struct implementing
//! [`CollectionResource`]; the handler does the heavy lifting once.

use serde_json::{json, Value};

use bsmcp_common::settings::UserSettings;

use super::envelope::{ErrorCode, RememberWarning};
use super::frontmatter;
use super::{Context, Outcome};

/// Book ID where a resource's pages live. Pages may be distributed across
/// sub-chapters per `sub_chapter_for_key` (e.g., journals split into YYYY-MM
/// monthly chapters).
pub type CollectionParent = i64;

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
}

pub async fn handle(
    resource: &dyn CollectionResource,
    action: &str,
    ctx: &Context,
) -> Outcome {
    let parent = match resource.parent(&ctx.settings) {
        Some(p) => p,
        None => {
            return Outcome::error(
                ErrorCode::SettingsNotConfigured,
                format!("{} parent not configured in settings", resource.name()),
                Some(resource.name()),
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
    let limit = ctx.body_count("limit", 25, 200) as i64;
    let offset = ctx.body.get("offset").and_then(|v| v.as_i64()).unwrap_or(0);
    let filter = parent_filter(parent);
    let query = format!("{{type:page}} {filter}");
    match ctx.client.search(&query, 1, limit + offset).await {
        Ok(resp) => {
            let data = resp
                .get("data")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let pages: Vec<Value> = data
                .into_iter()
                .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("page"))
                .skip(offset as usize)
                .take(limit as usize)
                .map(|p| json!({
                    "id": p.get("id").cloned().unwrap_or(Value::Null),
                    "name": p.get("name").cloned().unwrap_or(Value::Null),
                    "preview": p.get("preview_html").cloned().unwrap_or(Value::Null),
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

fn parent_filter(book_id: CollectionParent) -> String {
    format!("{{in_book:{book_id}}}")
}

async fn find_page_by_name(
    parent: CollectionParent,
    name: &str,
    ctx: &Context,
) -> Result<Option<i64>, String> {
    let filter = parent_filter(parent);
    // BookStack's name filter does substring match; we verify exact name client-side.
    let query = format!("{{type:page}} {{name:{name}}} {filter}");
    let resp = ctx.client.search(&query, 1, 25).await?;
    let data = resp.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for item in data {
        if item.get("type").and_then(|t| t.as_str()) != Some("page") {
            continue;
        }
        let item_name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if item_name.eq_ignore_ascii_case(name) {
            if let Some(id) = item.get("id").and_then(|i| i.as_i64()) {
                return Ok(Some(id));
            }
        }
    }
    Ok(None)
}

// --- WRITE ---

async fn handle_write(
    resource: &dyn CollectionResource,
    parent: CollectionParent,
    ctx: &Context,
) -> Outcome {
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
    book_id: CollectionParent,
    key: Option<&str>,
    ctx: &Context,
) -> Result<CreateTarget, String> {
    // If the resource splits by sub-chapter, find or create it.
    let sub_chapter_name = key.and_then(|k| resource.sub_chapter_for_key(k));
    if let Some(chapter_name) = sub_chapter_name {
        let chapter_id = find_or_create_chapter(book_id, &chapter_name, ctx).await?;
        Ok(CreateTarget::Chapter(chapter_id))
    } else {
        Ok(CreateTarget::Book(book_id))
    }
}

async fn find_or_create_chapter(
    book_id: i64,
    name: &str,
    ctx: &Context,
) -> Result<i64, String> {
    let query = format!("{{type:chapter}} {{in_book:{book_id}}} {{name:{name}}}");
    let resp = ctx.client.search(&query, 1, 25).await?;
    let data = resp.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for item in data {
        if item.get("type").and_then(|t| t.as_str()) != Some("chapter") {
            continue;
        }
        let item_name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if item_name.eq_ignore_ascii_case(name) {
            if let Some(id) = item.get("id").and_then(|i| i.as_i64()) {
                return Ok(id);
            }
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

    // Always run BookStack keyword search scoped to the parent.
    let filter = parent_filter(parent);
    let kw_query = format!("{query} {{type:page}} {filter}");
    let keyword_hits = match ctx.client.search(&kw_query, 1, limit as i64).await {
        Ok(v) => v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default(),
        Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
    };

    // Optionally augment with semantic — filter by parent post-hoc since the
    // semantic backend doesn't accept book/chapter filters yet.
    let mut warnings = Vec::new();
    let semantic_hits: Vec<Value> = if let Some(sem) = &ctx.semantic {
        match sem.search(&query, limit * 4, 0.45, true, false, &ctx.client).await {
            Ok(v) => {
                let results = v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
                results.into_iter().filter(|hit| matches_parent(hit, parent)).take(limit).collect()
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

fn matches_parent(hit: &Value, book_id: CollectionParent) -> bool {
    hit.get("book_id").and_then(|v| v.as_i64()) == Some(book_id)
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
        fn parent(&self, s: &UserSettings) -> Option<CollectionParent> {
            s.ai_hive_journal_book_id        }
        fn key_kind(&self) -> KeyKind { KeyKind::Date }
        fn sub_chapter_for_key(&self, key: &str) -> Option<String> {
            // YYYY-MM-DD → YYYY-MM monthly chapter
            if key.len() >= 7 { Some(key[..7].to_string()) } else { None }
        }
    }

    pub struct Collage;
    impl CollectionResource for Collage {
        fn name(&self) -> &'static str { "collage" }
        fn parent(&self, s: &UserSettings) -> Option<CollectionParent> {
            s.ai_collage_book_id        }
        fn key_kind(&self) -> KeyKind { KeyKind::Slug }
    }

    pub struct SharedCollage;
    impl CollectionResource for SharedCollage {
        fn name(&self) -> &'static str { "shared_collage" }
        fn parent(&self, s: &UserSettings) -> Option<CollectionParent> {
            s.ai_shared_collage_book_id        }
        fn key_kind(&self) -> KeyKind { KeyKind::Slug }
    }

    pub struct UserJournal;
    impl CollectionResource for UserJournal {
        fn name(&self) -> &'static str { "user_journal" }
        fn parent(&self, s: &UserSettings) -> Option<CollectionParent> {
            s.user_journal_book_id        }
        fn key_kind(&self) -> KeyKind { KeyKind::Date }
        fn sub_chapter_for_key(&self, key: &str) -> Option<String> {
            if key.len() >= 7 { Some(key[..7].to_string()) } else { None }
        }
    }
}
