use std::collections::HashMap;
use std::env;

use serde_json::{json, Value};

use pulldown_cmark::{html, Options, Parser};

use bsmcp_common::bookstack::{BookStackClient, ContentType, ExportFormat};
use crate::semantic::SemanticState;

const PROTOCOL_VERSION: &str = "2025-03-26";

pub async fn handle_request(request: &Value, client: &BookStackClient, semantic: Option<&SemanticState>) -> Option<Value> {
    let id = request.get("id");

    match request.get("jsonrpc").and_then(|v| v.as_str()) {
        Some("2.0") => {}
        _ => {
            return Some(json_rpc_error(id, -32600, "Invalid Request: missing or wrong jsonrpc version (must be \"2.0\")"));
        }
    }

    let method = request["method"].as_str().unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(json!({}));

    match method {
        "initialize" => {
            let instructions = build_instructions(client, semantic.is_some()).await;
            Some(json_rpc_result(id, json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "BookStack MCP",
                    "version": "0.3.0",
                },
                "instructions": instructions,
            })))
        }
        "notifications/initialized" => None,
        "tools/list" => Some(json_rpc_result(id, json!({ "tools": tool_definitions(semantic.is_some()) }))),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let result = execute_tool(name, &args, client, semantic).await;
            match result {
                Ok(text) => Some(json_rpc_result(id, json!({
                    "content": [{ "type": "text", "text": text }],
                }))),
                Err(e) => Some(json_rpc_result(id, json!({
                    "content": [{ "type": "text", "text": format!("Error: {e}") }],
                    "isError": true,
                }))),
            }
        }
        _ => Some(json_rpc_error(id, -32601, "Method not found")),
    }
}

fn json_rpc_result(id: Option<&Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.cloned().unwrap_or(Value::Null),
        "result": result,
    })
}

fn json_rpc_error(id: Option<&Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.cloned().unwrap_or(Value::Null),
        "error": { "code": code, "message": message },
    })
}

async fn execute_tool(name: &str, args: &Value, client: &BookStackClient, semantic: Option<&SemanticState>) -> Result<String, String> {
    match name {
        // Semantic Search (conditional)
        "semantic_search" => {
            let sem = semantic.ok_or("Semantic search is not enabled")?;
            let query = arg_str(args, "query")?;
            let limit = arg_i64(args, "limit", 10).clamp(1, 50) as usize;
            let threshold = args.get("threshold").and_then(|v| v.as_f64()).unwrap_or(0.5) as f32;
            // Pass the user's client for permission filtering
            let result = sem.search(&query, limit, threshold, client).await?;
            format_json(&result)
        }
        "reembed" => {
            let sem = semantic.ok_or("Semantic search is not enabled")?;
            let scope = arg_str_default(args, "scope", "all");
            let result = sem.trigger_reembed(&scope).await?;
            format_json(&result)
        }
        "embedding_status" => {
            let sem = semantic.ok_or("Semantic search is not enabled")?;
            let result = sem.embedding_status().await?;
            format_json(&result)
        }

        // Search
        "search_content" => {
            let query = arg_str(args, "query")?;
            let page = arg_i64(args, "page", 1).max(1);
            let count = arg_count(args, 20);
            let result = client.search(&query, page, count).await?;
            Ok(format_search_results(&result))
        }

        // Shelves
        "list_shelves" => {
            let count = arg_count(args, 50);
            let offset = arg_offset(args);
            format_json(&client.list_shelves(count, offset).await?)
        }
        "get_shelf" => {
            let id = arg_i64_required(args, "shelf_id")?;
            format_json(&client.get_shelf(id).await?)
        }
        "create_shelf" => {
            let name = arg_str(args, "name")?;
            let desc = arg_str_default(args, "description", "");
            format_json(&client.create_shelf(&name, &desc).await?)
        }
        "update_shelf" => {
            let id = arg_i64_required(args, "shelf_id")?;
            let data = filter_string_update_fields(args, &["name", "description"]);
            format_json(&client.update_shelf(id, &data).await?)
        }
        "delete_shelf" => {
            let id = arg_i64_required(args, "shelf_id")?;
            client.delete_shelf(id).await?;
            Ok(format!("Shelf {id} deleted."))
        }

        // Books
        "list_books" => {
            let count = arg_count(args, 50);
            let offset = arg_offset(args);
            format_json(&client.list_books(count, offset).await?)
        }
        "get_book" => {
            let id = arg_i64_required(args, "book_id")?;
            format_json(&client.get_book(id).await?)
        }
        "create_book" => {
            let name = arg_str(args, "name")?;
            let desc = arg_str_default(args, "description", "");
            format_json(&client.create_book(&name, &desc).await?)
        }
        "update_book" => {
            let id = arg_i64_required(args, "book_id")?;
            let data = filter_string_update_fields(args, &["name", "description"]);
            format_json(&client.update_book(id, &data).await?)
        }
        "delete_book" => {
            let id = arg_i64_required(args, "book_id")?;
            client.delete_book(id).await?;
            Ok(format!("Book {id} deleted."))
        }

        // Chapters
        "list_chapters" => {
            let count = arg_count(args, 50);
            let offset = arg_offset(args);
            format_json(&client.list_chapters(count, offset).await?)
        }
        "get_chapter" => {
            let id = arg_i64_required(args, "chapter_id")?;
            format_json(&client.get_chapter(id).await?)
        }
        "create_chapter" => {
            let book_id = arg_i64_required(args, "book_id")?;
            let name = arg_str(args, "name")?;
            let desc = arg_str_default(args, "description", "");
            format_json(&client.create_chapter(book_id, &name, &desc).await?)
        }
        "update_chapter" => {
            let id = arg_i64_required(args, "chapter_id")?;
            let data = filter_string_update_fields(args, &["name", "description"]);
            format_json(&client.update_chapter(id, &data).await?)
        }
        "delete_chapter" => {
            let id = arg_i64_required(args, "chapter_id")?;
            client.delete_chapter(id).await?;
            Ok(format!("Chapter {id} deleted."))
        }

        // Pages
        "list_pages" => {
            let count = arg_count(args, 50);
            let offset = arg_offset(args);
            format_json(&client.list_pages(count, offset).await?)
        }
        "get_page" => {
            let id = arg_i64_required(args, "page_id")?;
            format_json(&client.get_page(id).await?)
        }
        "create_page" => {
            let mut data = json!({ "name": arg_str(args, "name")? });
            if let Some(v) = args.get("chapter_id").and_then(|v| v.as_i64()) {
                data["chapter_id"] = json!(v);
            } else if let Some(v) = args.get("book_id").and_then(|v| v.as_i64()) {
                data["book_id"] = json!(v);
            } else {
                return Err("Either book_id or chapter_id is required".to_string());
            }
            if let Some(md) = args.get("markdown").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                data["html"] = json!(markdown_to_html(md));
            } else if let Some(v) = args.get("html").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                data["html"] = json!(v);
            }
            format_json(&client.create_page(&data).await?)
        }
        "update_page" => {
            let id = arg_i64_required(args, "page_id")?;
            let mut data = json!({});
            if let Some(v) = args.get("name").and_then(|v| v.as_str()) {
                data["name"] = json!(v);
            }
            if let Some(md) = args.get("markdown").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                data["html"] = json!(markdown_to_html(md));
            } else if let Some(v) = args.get("html").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                data["html"] = json!(v);
            }
            format_json(&client.update_page(id, &data).await?)
        }
        "edit_page" => {
            let id = arg_i64_required(args, "page_id")?;
            let old_text = args.get("old_text").and_then(|v| v.as_str())
                .ok_or("old_text is required")?;
            let new_text = args.get("new_text").and_then(|v| v.as_str())
                .ok_or("new_text is required")?;
            let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);

            // Fetch current markdown content
            let md = client.export_page(id, ExportFormat::Markdown).await?;

            // Validate old_text exists
            let count = md.matches(old_text).count();
            if count == 0 {
                return Err(format!("old_text not found in page {id}"));
            }
            if count > 1 && !replace_all {
                return Err(format!("old_text found {count} times in page {id}. Use replace_all=true to replace all, or provide more context to make it unique."));
            }

            // Apply replacement
            let updated = if replace_all {
                md.replace(old_text, new_text)
            } else {
                md.replacen(old_text, new_text, 1)
            };

            let data = json!({ "html": markdown_to_html(&updated) });
            format_json(&client.update_page(id, &data).await?)
        }
        "delete_page" => {
            let id = arg_i64_required(args, "page_id")?;
            client.delete_page(id).await?;
            Ok(format!("Page {id} deleted."))
        }

        // Attachments
        "list_attachments" => {
            format_json(&client.list_attachments().await?)
        }
        "get_attachment" => {
            let id = arg_i64_required(args, "attachment_id")?;
            format_json(&client.get_attachment(id).await?)
        }
        "create_attachment" => {
            let mut data = json!({
                "name": arg_str(args, "name")?,
                "uploaded_to": arg_i64_required(args, "uploaded_to")?,
            });
            if let Some(v) = args.get("link").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                data["link"] = json!(v);
            }
            format_json(&client.create_attachment(&data).await?)
        }
        "update_attachment" => {
            let id = arg_i64_required(args, "attachment_id")?;
            let data = filter_string_update_fields(args, &["name", "link"]);
            format_json(&client.update_attachment(id, &data).await?)
        }
        "delete_attachment" => {
            let id = arg_i64_required(args, "attachment_id")?;
            client.delete_attachment(id).await?;
            Ok(format!("Attachment {id} deleted."))
        }

        // Exports
        "export_page" => {
            let id = arg_i64_required(args, "page_id")?;
            let fmt = ExportFormat::parse_str(&arg_str_default(args, "format", "markdown"))?;
            client.export_page(id, fmt).await
        }
        "export_chapter" => {
            let id = arg_i64_required(args, "chapter_id")?;
            let fmt = ExportFormat::parse_str(&arg_str_default(args, "format", "markdown"))?;
            client.export_chapter(id, fmt).await
        }
        "export_book" => {
            let id = arg_i64_required(args, "book_id")?;
            let fmt = ExportFormat::parse_str(&arg_str_default(args, "format", "markdown"))?;
            client.export_book(id, fmt).await
        }

        // Comments
        "list_comments" => {
            let mut query: Vec<(&str, &str)> = vec![];
            let page_id_str;
            if let Some(v) = args.get("page_id").and_then(|v| v.as_i64()) {
                page_id_str = v.to_string();
                query.push(("filter[page_id]", &page_id_str));
            }
            format_json(&client.list_comments(&query).await?)
        }
        "get_comment" => {
            let id = arg_i64_required(args, "comment_id")?;
            format_json(&client.get_comment(id).await?)
        }
        "create_comment" => {
            let mut data = json!({
                "page_id": arg_i64_required(args, "page_id")?,
            });
            if let Some(md) = args.get("markdown").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                data["html"] = json!(markdown_to_html(md));
            } else if let Some(v) = args.get("html").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                data["html"] = json!(v);
            }
            if let Some(v) = args.get("parent_id").and_then(|v| v.as_i64()) {
                data["parent_id"] = json!(v);
            }
            format_json(&client.create_comment(&data).await?)
        }
        "update_comment" => {
            let id = arg_i64_required(args, "comment_id")?;
            let mut data = json!({});
            if let Some(md) = args.get("markdown").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                data["html"] = json!(markdown_to_html(md));
            } else if let Some(v) = args.get("html").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                data["html"] = json!(v);
            }
            format_json(&client.update_comment(id, &data).await?)
        }
        "delete_comment" => {
            let id = arg_i64_required(args, "comment_id")?;
            client.delete_comment(id).await?;
            Ok(format!("Comment {id} deleted."))
        }

        // Recycle Bin
        "list_recycle_bin" => {
            let count = arg_count(args, 50);
            let offset = arg_offset(args);
            format_json(&client.list_recycle_bin(count, offset).await?)
        }
        "restore_recycle_bin_item" => {
            let id = arg_i64_required(args, "deletion_id")?;
            format_json(&client.restore_recycle_bin_item(id).await?)
        }
        "destroy_recycle_bin_item" => {
            let id = arg_i64_required(args, "deletion_id")?;
            client.destroy_recycle_bin_item(id).await?;
            Ok(format!("Recycle bin item {id} permanently deleted."))
        }

        // Users
        "list_users" => {
            let count = arg_count(args, 50);
            let offset = arg_offset(args);
            format_json(&client.list_users(count, offset).await?)
        }
        "get_user" => {
            let id = arg_i64_required(args, "user_id")?;
            format_json(&client.get_user(id).await?)
        }

        // Audit Log
        "list_audit_log" => {
            let count = arg_count(args, 50);
            let offset = arg_offset(args);
            format_json(&client.list_audit_log(count, offset).await?)
        }

        // System
        "get_system_info" => {
            format_json(&client.get_system_info().await?)
        }

        // Image Gallery
        "list_images" => {
            let count = arg_count(args, 50);
            let offset = arg_offset(args);
            let mut filter: Vec<(&str, &str)> = vec![];
            let type_str;
            if let Some(v) = args.get("type").and_then(|v| v.as_str()) {
                validate_enum(v, &["gallery", "drawio"], "type")?;
                type_str = v.to_string();
                filter.push(("filter[type]", &type_str));
            }
            let uploaded_to_str;
            if let Some(v) = args.get("uploaded_to").and_then(|v| v.as_i64()) {
                uploaded_to_str = v.to_string();
                filter.push(("filter[uploaded_to]", &uploaded_to_str));
            }
            format_json(&client.list_images(count, offset, &filter).await?)
        }
        "get_image" => {
            let id = arg_i64_required(args, "image_id")?;
            format_json(&client.get_image(id).await?)
        }
        "update_image" => {
            let id = arg_i64_required(args, "image_id")?;
            let data = filter_string_update_fields(args, &["name"]);
            format_json(&client.update_image(id, &data).await?)
        }
        "delete_image" => {
            let id = arg_i64_required(args, "image_id")?;
            client.delete_image(id).await?;
            Ok(format!("Image {id} deleted."))
        }

        // Content Permissions
        "get_content_permissions" => {
            let content_type = ContentType::parse_str(&arg_str(args, "content_type")?)?;
            let content_id = arg_i64_required(args, "content_id")?;
            format_json(&client.get_content_permissions(content_type, content_id).await?)
        }
        "update_content_permissions" => {
            let content_type = ContentType::parse_str(&arg_str(args, "content_type")?)?;
            let content_id = arg_i64_required(args, "content_id")?;
            let data = filter_update_fields(args, &["owner_id", "role_permissions", "fallback_permissions"]);
            format_json(&client.update_content_permissions(content_type, content_id, &data).await?)
        }

        // Roles
        "list_roles" => {
            let count = arg_count(args, 50);
            let offset = arg_offset(args);
            format_json(&client.list_roles(count, offset).await?)
        }
        "get_role" => {
            let id = arg_i64_required(args, "role_id")?;
            format_json(&client.get_role(id).await?)
        }

        _ => Err(format!("Unknown tool: {name}")),
    }
}

// --- Arg helpers ---

fn arg_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("Missing required argument: {key}"))
}

fn arg_str_default(args: &Value, key: &str, default: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}

fn arg_i64(args: &Value, key: &str, default: i64) -> i64 {
    args.get(key).and_then(|v| v.as_i64()).unwrap_or(default)
}

fn arg_count(args: &Value, default: i64) -> i64 {
    arg_i64(args, "count", default).clamp(1, 500)
}

fn arg_offset(args: &Value) -> i64 {
    arg_i64(args, "offset", 0).max(0)
}

fn arg_i64_required(args: &Value, key: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("Missing required argument: {key}"))
}

fn validate_enum(value: &str, allowed: &[&str], name: &str) -> Result<(), String> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(format!("Invalid {name}: '{value}'. Must be one of: {}", allowed.join(", ")))
    }
}

fn filter_update_fields(args: &Value, fields: &[&str]) -> Value {
    let mut data = json!({});
    for &field in fields {
        if let Some(v) = args.get(field) {
            if !v.is_null() {
                data[field] = v.clone();
            }
        }
    }
    data
}

fn filter_string_update_fields(args: &Value, fields: &[&str]) -> Value {
    let mut data = json!({});
    for &field in fields {
        if let Some(v) = args.get(field) {
            if v.is_string() {
                data[field] = v.clone();
            }
        }
    }
    data
}

fn format_json(v: &Value) -> Result<String, String> {
    serde_json::to_string_pretty(v).map_err(|e| e.to_string())
}

fn format_search_results(data: &Value) -> String {
    let results = data.get("data").and_then(|v| v.as_array());
    let total = data.get("total").and_then(|v| v.as_i64()).unwrap_or(0);

    let Some(results) = results else {
        return "No results found.".into();
    };

    if results.is_empty() {
        return "No results found.".into();
    }

    let mut lines = vec![format!("Found {total} results:\n")];
    for item in results {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let id = item.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        lines.push(format!("- [{item_type}] {name} (id: {id})"));

        if let Some(preview) = item.get("preview_html") {
            let raw = if let Some(content) = preview.get("content").and_then(|v| v.as_str()) {
                content.to_string()
            } else if let Some(s) = preview.as_str() {
                s.to_string()
            } else {
                String::new()
            };
            if !raw.is_empty() {
                let clean = strip_html_tags(&raw);
                let truncated: String = clean.chars().take(200).collect();
                lines.push(format!("  Preview: {truncated}"));
            }
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

fn strip_html_tags(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
}

fn markdown_to_html(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

// --- Dynamic instructions (sent on initialize) ---

async fn build_instructions(client: &BookStackClient, semantic_enabled: bool) -> String {
    let instance_name = env::var("BSMCP_INSTANCE_NAME").unwrap_or_default();
    let instance_desc = env::var("BSMCP_INSTANCE_DESC").unwrap_or_default();

    let mut instructions = String::new();
    if !instance_name.is_empty() {
        instructions.push_str(&instance_name);
        if !instance_desc.is_empty() {
            instructions.push_str(&format!(" - {instance_desc}"));
        }
        instructions.push_str("\n\n");
    }
    instructions.push_str(
        "BookStack knowledge management server. Content is organized as: \
         Shelves > Books > Chapters > Pages. Use search_content to find content, \
         or navigate the hierarchy using the IDs below.\n\n\
         IMPORTANT: Before creating or updating any page, first retrieve an existing page \
         from the same book or chapter using get_page to identify the writing style, \
         formatting conventions, heading structure, and markdown patterns already in use. \
         Match the established style of the surrounding content.\n\n\
         Markdown content is automatically converted to HTML server-side. \
         You can send markdown via the 'markdown' parameter for pages and comments — \
         the server handles conversion reliably, avoiding JSON escaping issues with \
         complex markdown. Use 'html' only when you need precise HTML control.\n\n",
    );

    match build_structure(client).await {
        Some(structure) => {
            instructions.push_str("Current structure:\n\n");
            instructions.push_str(&structure);
        }
        None => {
            instructions
                .push_str("Use list_shelves and list_books to explore the structure.");
        }
    }

    if semantic_enabled {
        instructions.push_str(
            "\n\nSemantic vector search is available and should be your PRIMARY search method. \
             Prefer 'semantic_search' over 'search_content' for most queries — it finds \
             conceptually related content by meaning, not just keyword matches, and returns \
             richer context including a Markov blanket of related pages (linked_from, links_to, \
             co_linked, siblings). Only fall back to 'search_content' when you need exact \
             keyword/tag matches or when semantic_search returns no results. \
             Use 'reembed' to re-index content after bulk changes and 'embedding_status' \
             to check indexing progress.",
        );
    }

    instructions
}

async fn build_structure(client: &BookStackClient) -> Option<String> {
    let shelves = client.list_shelves(500, 0).await.ok()?;
    let shelf_list = shelves["data"].as_array()?;

    let shelf_futures: Vec<_> = shelf_list
        .iter()
        .filter_map(|s| s["id"].as_i64())
        .map(|id| client.get_shelf(id))
        .collect();
    let shelf_details = futures::future::join_all(shelf_futures).await;

    let chapters = client
        .list_chapters(500, 0)
        .await
        .ok()
        .and_then(|v| v["data"].as_array().cloned())
        .unwrap_or_default();

    let mut chapters_by_book: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    for ch in &chapters {
        if let (Some(book_id), Some(id), Some(name)) = (
            ch["book_id"].as_i64(),
            ch["id"].as_i64(),
            ch["name"].as_str(),
        ) {
            chapters_by_book
                .entry(book_id)
                .or_default()
                .push((id, name.to_string()));
        }
    }

    let mut output = String::new();
    for shelf in shelf_details.iter().flatten() {
        let name = shelf["name"].as_str().unwrap_or("?");
        let id = shelf["id"].as_i64().unwrap_or(0);
        output.push_str(&format!("Shelf: {name} (ID: {id})\n"));

        if let Some(books) = shelf["books"].as_array() {
            for book in books {
                let bname = book["name"].as_str().unwrap_or("?");
                let bid = book["id"].as_i64().unwrap_or(0);
                output.push_str(&format!("  Book: {bname} (ID: {bid})\n"));

                if let Some(chs) = chapters_by_book.get(&bid) {
                    for (cid, cname) in chs {
                        output.push_str(&format!("    Chapter: {cname} (ID: {cid})\n"));
                    }
                }
            }
        }
        output.push('\n');
    }

    if output.is_empty() {
        None
    } else {
        Some(output)
    }
}

// --- Tool definitions ---

pub fn tool_definitions(semantic_enabled: bool) -> Vec<Value> {
    let mut tools = vec![
        tool("search_content",
            "Search across all BookStack content (pages, chapters, books, shelves). Supports operators: {type:page}, [tag_name=value], {in_name:term}, {created_by:me}, exact match with quotes.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "page": { "type": "integer", "description": "Page number", "default": 1 },
                    "count": { "type": "integer", "description": "Results per page", "default": 20 }
                },
                "required": ["query"]
            })),

        // Shelves
        tool("list_shelves", "List all shelves. Shelves are the top-level organizational unit.",
            paginated_schema()),
        tool("get_shelf", "Get a shelf by ID, including its books.",
            id_schema("shelf_id")),
        tool("create_shelf", "Create a new shelf.",
            name_desc_schema()),
        tool("update_shelf", "Update a shelf.",
            update_schema("shelf_id", &["name", "description"])),
        tool("delete_shelf", "Delete a shelf. This does NOT delete the books inside it.",
            id_schema("shelf_id")),

        // Books
        tool("list_books", "List all books.", paginated_schema()),
        tool("get_book", "Get a book by ID, including its chapters and pages.",
            id_schema("book_id")),
        tool("create_book", "Create a new book.",
            name_desc_schema()),
        tool("update_book", "Update a book.",
            update_schema("book_id", &["name", "description"])),
        tool("delete_book", "Delete a book and all its chapters/pages.",
            id_schema("book_id")),

        // Chapters
        tool("list_chapters", "List all chapters across all books.", paginated_schema()),
        tool("get_chapter", "Get a chapter by ID, including its pages.",
            id_schema("chapter_id")),
        tool("create_chapter", "Create a new chapter within a book.", json!({
            "type": "object",
            "properties": {
                "book_id": { "type": "integer", "description": "Book ID to create chapter in" },
                "name": { "type": "string", "description": "Chapter name" },
                "description": { "type": "string", "description": "Chapter description", "default": "" }
            },
            "required": ["book_id", "name"]
        })),
        tool("update_chapter", "Update a chapter.",
            update_schema("chapter_id", &["name", "description"])),
        tool("delete_chapter", "Delete a chapter. Pages inside become book-level pages.",
            id_schema("chapter_id")),

        // Pages
        tool("list_pages", "List all pages across all books.", paginated_schema()),
        tool("get_page", "Get a page by ID, including full content.",
            id_schema("page_id")),
        tool("create_page", "Create a new page. Must provide either book_id or chapter_id. Provide content as markdown (preferred) or html. Markdown is converted to HTML server-side for reliable formatting.", json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Page name" },
                "book_id": { "type": "integer", "description": "Book ID (if not in a chapter)" },
                "chapter_id": { "type": "integer", "description": "Chapter ID (preferred over book_id)" },
                "markdown": { "type": "string", "description": "Page content in markdown (converted to HTML server-side)", "default": "" },
                "html": { "type": "string", "description": "Page content in HTML (use if you need precise HTML control)", "default": "" }
            },
            "required": ["name"]
        })),
        tool("update_page", "Update a page. Provide content as markdown (preferred) or html. Markdown is converted to HTML server-side.",
            update_schema("page_id", &["name", "markdown", "html"])),
        tool("edit_page", "Performs exact string replacements in a page's markdown content. Fetches the page, finds old_text, replaces with new_text, and saves. Fails if old_text is not found or is ambiguous (found multiple times without replace_all).", json!({
            "type": "object",
            "properties": {
                "page_id": { "type": "integer", "description": "The page_id" },
                "old_text": { "type": "string", "description": "The exact text to find and replace" },
                "new_text": { "type": "string", "description": "The replacement text" },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)", "default": false }
            },
            "required": ["page_id", "old_text", "new_text"]
        })),
        tool("delete_page", "Delete a page (moves to recycle bin).",
            id_schema("page_id")),

        // Attachments
        tool("list_attachments", "List all attachments.", json!({
            "type": "object", "properties": {}
        })),
        tool("get_attachment", "Get an attachment by ID, including its content or download link.",
            id_schema("attachment_id")),
        tool("create_attachment", "Create a link attachment on a page. uploaded_to is the page ID.", json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Attachment name" },
                "uploaded_to": { "type": "integer", "description": "Page ID to attach to" },
                "link": { "type": "string", "description": "URL for link attachment", "default": "" }
            },
            "required": ["name", "uploaded_to"]
        })),
        tool("update_attachment", "Update an attachment.",
            update_schema("attachment_id", &["name", "link"])),
        tool("delete_attachment", "Delete an attachment.",
            id_schema("attachment_id")),

        // Exports
        tool("export_page", "Export a page as markdown, plaintext, or html. Returns the raw exported content.", json!({
            "type": "object",
            "properties": {
                "page_id": { "type": "integer", "description": "Page ID to export" },
                "format": { "type": "string", "enum": ["markdown", "plaintext", "html"], "description": "Export format", "default": "markdown" }
            },
            "required": ["page_id"]
        })),
        tool("export_chapter", "Export a chapter as markdown, plaintext, or html. Returns all pages in the chapter.", json!({
            "type": "object",
            "properties": {
                "chapter_id": { "type": "integer", "description": "Chapter ID to export" },
                "format": { "type": "string", "enum": ["markdown", "plaintext", "html"], "description": "Export format", "default": "markdown" }
            },
            "required": ["chapter_id"]
        })),
        tool("export_book", "Export a book as markdown, plaintext, or html. Returns all chapters and pages.", json!({
            "type": "object",
            "properties": {
                "book_id": { "type": "integer", "description": "Book ID to export" },
                "format": { "type": "string", "enum": ["markdown", "plaintext", "html"], "description": "Export format", "default": "markdown" }
            },
            "required": ["book_id"]
        })),

        // Comments
        tool("list_comments", "List comments, optionally filtered by page.", json!({
            "type": "object",
            "properties": {
                "page_id": { "type": "integer", "description": "Filter comments by page ID" }
            }
        })),
        tool("get_comment", "Get a comment by ID.",
            id_schema("comment_id")),
        tool("create_comment", "Create a comment on a page. Provide content as markdown (preferred) or html.", json!({
            "type": "object",
            "properties": {
                "page_id": { "type": "integer", "description": "Page ID to comment on" },
                "markdown": { "type": "string", "description": "Comment content in markdown (converted to HTML server-side)" },
                "html": { "type": "string", "description": "Comment content in HTML" },
                "parent_id": { "type": "integer", "description": "Parent comment ID for replies" }
            },
            "required": ["page_id"]
        })),
        tool("update_comment", "Update a comment. Provide content as markdown (preferred) or html.", json!({
            "type": "object",
            "properties": {
                "comment_id": { "type": "integer", "description": "The comment_id" },
                "markdown": { "type": "string", "description": "New comment content in markdown (converted to HTML server-side)" },
                "html": { "type": "string", "description": "New comment content in HTML" }
            },
            "required": ["comment_id"]
        })),
        tool("delete_comment", "Delete a comment.",
            id_schema("comment_id")),

        // Recycle Bin
        tool("list_recycle_bin", "List items in the recycle bin.",
            paginated_schema()),
        tool("restore_recycle_bin_item", "Restore an item from the recycle bin.",
            id_schema("deletion_id")),
        tool("destroy_recycle_bin_item", "Permanently delete an item from the recycle bin. Cannot be undone.",
            id_schema("deletion_id")),

        // Users
        tool("list_users", "List all users.",
            paginated_schema()),
        tool("get_user", "Get a user by ID.",
            id_schema("user_id")),

        // Audit Log
        tool("list_audit_log", "List audit log entries showing recent activity.",
            paginated_schema()),

        // System
        tool("get_system_info", "Get BookStack instance information (version, etc.).", json!({
            "type": "object", "properties": {}
        })),

        // Image Gallery
        tool("list_images", "List images in the gallery.", json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer", "description": "Number of results", "default": 50 },
                "offset": { "type": "integer", "description": "Number to skip", "default": 0 },
                "type": { "type": "string", "enum": ["gallery", "drawio"], "description": "Filter by image type" },
                "uploaded_to": { "type": "integer", "description": "Filter by page ID the image was uploaded to" }
            }
        })),
        tool("get_image", "Get image details by ID. Returns metadata and URLs.",
            id_schema("image_id")),
        tool("update_image", "Update image metadata (name).", json!({
            "type": "object",
            "properties": {
                "image_id": { "type": "integer", "description": "The image_id" },
                "name": { "type": "string", "description": "New image name" }
            },
            "required": ["image_id"]
        })),
        tool("delete_image", "Delete an image from the gallery.",
            id_schema("image_id")),

        // Content Permissions
        tool("get_content_permissions", "Get permissions for a content item.", json!({
            "type": "object",
            "properties": {
                "content_type": { "type": "string", "enum": ["page", "chapter", "book", "shelf"], "description": "Content type" },
                "content_id": { "type": "integer", "description": "Content item ID" }
            },
            "required": ["content_type", "content_id"]
        })),
        tool("update_content_permissions", "Update permissions for a content item.", json!({
            "type": "object",
            "properties": {
                "content_type": { "type": "string", "enum": ["page", "chapter", "book", "shelf"], "description": "Content type" },
                "content_id": { "type": "integer", "description": "Content item ID" },
                "owner_id": { "type": "integer", "description": "New owner user ID" },
                "role_permissions": { "type": "array", "description": "Array of role permission objects" },
                "fallback_permissions": { "type": "object", "description": "Fallback permission settings" }
            },
            "required": ["content_type", "content_id"]
        })),

        // Roles
        tool("list_roles", "List all roles.",
            paginated_schema()),
        tool("get_role", "Get a role by ID, including its permissions.",
            id_schema("role_id")),
    ];

    if semantic_enabled {
        tools.push(tool("semantic_search",
            "Search content by meaning using vector embeddings. Finds conceptually related pages without exact keyword matches. Returns matching chunks with relevance scores and a Markov blanket of contextually related pages.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural language search query" },
                    "limit": { "type": "integer", "description": "Max number of page results to return", "default": 10 },
                    "threshold": { "type": "number", "description": "Minimum cosine similarity score (0.0-1.0)", "default": 0.5 }
                },
                "required": ["query"]
            })));
        tools.push(tool("reembed",
            "Trigger re-embedding of page content. Runs in the background. Use 'all' to re-embed everything, 'book:ID' for a specific book, or 'page:ID' for a single page.",
            json!({
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "description": "Scope: 'all', 'book:ID', or 'page:ID'", "default": "all" }
                }
            })));
        tools.push(tool("embedding_status",
            "Get the current status of the semantic search index, including total indexed pages, chunks, and latest embedding job progress.",
            json!({
                "type": "object",
                "properties": {}
            })));
    }

    tools
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

fn paginated_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "count": { "type": "integer", "description": "Number of results", "default": 50 },
            "offset": { "type": "integer", "description": "Number to skip", "default": 0 }
        }
    })
}

fn id_schema(id_name: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            id_name: { "type": "integer", "description": format!("The {id_name}") }
        },
        "required": [id_name]
    })
}

fn name_desc_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "description": "Name" },
            "description": { "type": "string", "description": "Description", "default": "" }
        },
        "required": ["name"]
    })
}

fn update_schema(id_name: &str, fields: &[&str]) -> Value {
    let mut props = json!({ id_name: { "type": "integer", "description": format!("The {id_name}") } });
    for &field in fields {
        props[field] = json!({ "type": "string", "description": format!("New {field}") });
    }
    json!({
        "type": "object",
        "properties": props,
        "required": [id_name]
    })
}
