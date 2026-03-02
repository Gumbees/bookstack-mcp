# BookStack MCP Server

Rust MCP server that bridges Claude to a BookStack instance via SSE transport.

## Architecture

```
src/
  main.rs        - Axum server, routes, env config
  sse.rs         - SSE session management, multi-user auth, message routing
  mcp.rs         - MCP protocol handler, tool definitions, tool execution
  bookstack.rs   - BookStack REST API client (reqwest)
```

**Flow:** Client connects SSE with `Bearer <token_id>:<token_secret>` -> validates against BookStack -> creates session -> client sends JSON-RPC to `/mcp/messages/?sessionId=<id>` -> dispatches to tool -> responds via SSE event.

**Key patterns:**
- `mcp.rs` uses `block_in_place` + `block_on` to call async BookStack client from sync `handle_request`
- Tool definitions use helper fns: `tool()`, `paginated_schema()`, `id_schema()`, `name_desc_schema()`, `update_schema()`
- `bookstack.rs` has 4 HTTP methods (`get`, `post`, `put`, `delete`) that all follow the same pattern
- Sessions stored in `Arc<RwLock<HashMap<String, Session>>>` with 30s cleanup loop

## Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `BSMCP_BOOKSTACK_URL` | Yes | - | BookStack instance URL |
| `BSMCP_HOST` | No | `0.0.0.0` | Bind address |
| `BSMCP_PORT` | No | `8080` | Bind port |

## What's Implemented (26 tools)

- **search_content** - Full-text search with BookStack query operators
- **Shelves** - list, get, create, update, delete (5)
- **Books** - list, get, create, update, delete (5)
- **Chapters** - list, get, create, update, delete (5)
- **Pages** - list, get, create, update, delete (5)
- **Attachments** - list, get, create, update, delete (5) - link attachments only

## What's Missing (BookStack API endpoints not yet implemented)

### Priority 1 - High value for MCP usage
- **Page/Chapter/Book Export** - `GET /api/pages/{id}/export/{html,plaintext,markdown}` - Get clean exported content. Markdown export is especially useful since `get_page` returns raw HTML in the `html` field.
- **Comments CRUD** - `GET/POST/PUT/DELETE /api/comments` - Read and write comments on pages.
- **Recycle Bin** - `GET /api/recycle-bin`, `PUT /api/recycle-bin/{id}` (restore), `DELETE /api/recycle-bin/{id}` (permanent delete) - Recover deleted items.

### Priority 2 - Useful for admin/context
- **Users** - `GET /api/users`, `GET /api/users/{id}` - List and read users (read-only is fine).
- **Audit Log** - `GET /api/audit-log` - Activity history with filters.
- **System Info** - `GET /api/system` - Instance version and info.

### Priority 3 - Specialized
- **Image Gallery** - `GET/POST/PUT/DELETE /api/image-gallery` - List, read, update, delete images. Upload requires multipart form (not just JSON).
- **Content Permissions** - `GET/PUT /api/content-permissions/{type}/{id}` - Read/update permissions on content.
- **Roles** - `GET /api/roles`, `GET /api/roles/{id}` - List and read roles.

### Not planned
- **Imports** - ZIP file handling doesn't work well over MCP text protocol.
- **User/Role CRUD** - Creating/deleting users/roles is admin-level; read-only is sufficient.
- **PDF/ZIP export** - Binary formats can't be returned as MCP text content.

## Adding a New Tool

1. **bookstack.rs** - Add the API method(s) to `BookStackClient`
2. **mcp.rs** - Add match arm in `execute_tool()`, add tool def in `tool_definitions()`
3. Use existing helpers: `arg_str`, `arg_i64`, `arg_i64_required`, `arg_str_default`, `filter_update_fields`, `format_json`

For GET endpoints that need a raw text response (like export), add a `get_text()` method to `BookStackClient` that returns `String` instead of `Value`.

## Building

```bash
cargo build --release    # ~10MB optimized binary
docker compose build     # Alpine multi-stage, ~10MB image
```

## Branch Info

- `main` - production branch
- `master` - active development (should be merged to main when stable)

## Deployment

Docker Compose with Traefik reverse proxy. Domain: `bookstack-mcp.beesroadhouse.com`. TLS via Let's Encrypt.
