# BookStack MCP Server

Rust MCP server that bridges Claude to a BookStack instance via SSE transport.

## Architecture

```
src/
  main.rs        - Axum server, routes, env config
  sse.rs         - SSE session management, multi-user auth, message routing
  mcp.rs         - MCP protocol handler, tool definitions, tool execution
  bookstack.rs   - BookStack REST API client (reqwest)
  oauth.rs       - OAuth 2.1, login form, token exchange
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
| `BSMCP_INSTANCE_NAME` | No | - | Instance name shown to AI (e.g. "Nate's Personal KB") |
| `BSMCP_INSTANCE_DESC` | No | - | Instance description shown to AI (e.g. "Personal knowledge base for home and projects") |

## Implemented Tools (49)

- **search_content** - Full-text search with BookStack query operators
- **Shelves** - list, get, create, update, delete (5)
- **Books** - list, get, create, update, delete (5)
- **Chapters** - list, get, create, update, delete (5)
- **Pages** - list, get, create, update, delete (5)
- **Attachments** - list, get, create, update, delete (5) - link attachments only
- **Exports** - export_page, export_chapter, export_book (3) - markdown, plaintext, or html
- **Comments** - list, get, create, update, delete (5)
- **Recycle Bin** - list, restore, destroy (3)
- **Users** - list, get (2) - read-only
- **Audit Log** - list (1)
- **System** - get_system_info (1)
- **Image Gallery** - list, get, update, delete (4) - no upload (requires multipart)
- **Content Permissions** - get, update (2)
- **Roles** - list, get (2) - read-only

## Not Implemented

- **Imports** - ZIP file handling doesn't work well over MCP text protocol.
- **User/Role CRUD** - Creating/deleting users/roles is admin-level; read-only is sufficient.
- **PDF/ZIP export** - Binary formats can't be returned as MCP text content.
- **Image upload** - Requires multipart form data, not JSON.

## Adding a New Tool

1. **bookstack.rs** - Add the API method(s) to `BookStackClient`
2. **mcp.rs** - Add match arm in `execute_tool()`, add tool def in `tool_definitions()`
3. Use existing helpers: `arg_str`, `arg_i64`, `arg_i64_required`, `arg_str_default`, `filter_update_fields`, `format_json`

For GET endpoints that need a raw text response (like export), add a `get_text()` method to `BookStackClient` that returns `String` instead of `Value`.

## OAuth / Claude Desktop Custom Connector

The server implements OAuth 2.1 (authorization code + PKCE) with a browser-based login form for BookStack API token authentication.

**How to configure in Claude Desktop:**
1. Add custom connector with URL: `https://bookstack-mcp.beesroadhouse.com/mcp/sse`
2. For Client ID / Client Secret, enter any value (e.g. "unused") — real auth happens in the browser
3. When connecting, a login form opens — enter your BookStack API Token ID and Secret

**OAuth endpoints:**
- `GET /.well-known/oauth-authorization-server` — RFC 8414 metadata (MCP 2025-03-26)
- `GET /.well-known/oauth-protected-resource` — RFC 9728 metadata (MCP 2025-06-18)
- `GET /authorize` — Serves login form for API token entry (with instructions + link to BookStack)
- `POST /authorize` — Validates token against BookStack, issues auth code
- `POST /token` — Token exchange (retrieves stored credentials, issues access token)

**Two auth flows:**
1. **Form-based (primary):** Claude opens /authorize → user enters BookStack API token in browser form → server validates via API → stores credentials with auth code → redirects → code exchange issues access token. Token endpoint auth method = "none".
2. **Legacy client credentials:** Client sends BookStack token_id as client_id and token_secret as client_secret in the /token request. Still works for backward compatibility.

**Also supported:** Legacy `Bearer token_id:token_secret` format on SSE/messages endpoints (Claude Code direct connection).

**Architecture:** OAuth types live in `oauth.rs`. Auth codes store BookStack credentials from the form. Auth codes and access tokens stored in `AppState` (in-memory, cleaned up every 30s). Auth codes expire in 5 minutes, access tokens in 24 hours.

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
