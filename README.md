# BookStack MCP Server

An MCP (Model Context Protocol) server that gives Claude full access to a [BookStack](https://www.bookstackapp.com/) instance. Built in Rust with tokio/axum for a single static binary with zero runtime dependencies.

## Features

- Full CRUD on all core BookStack resources (shelves, books, chapters, pages, attachments)
- Full-text search with BookStack query operators
- **OAuth 2.1 support** — use as a Claude Desktop custom connector without config files
- **Dynamic structure discovery** — AI automatically learns your BookStack hierarchy on connect
- Multi-user support via per-session BookStack API tokens
- SSE transport (MCP protocol version `2024-11-05`)
- Multi-arch Docker images (amd64 + arm64)
- ~10MB Docker image (Alpine + static Rust binary)

## Available Tools (49)

| Category | Tools |
|----------|-------|
| **Search** | `search_content` |
| **Shelves** | `list_shelves`, `get_shelf`, `create_shelf`, `update_shelf`, `delete_shelf` |
| **Books** | `list_books`, `get_book`, `create_book`, `update_book`, `delete_book` |
| **Chapters** | `list_chapters`, `get_chapter`, `create_chapter`, `update_chapter`, `delete_chapter` |
| **Pages** | `list_pages`, `get_page`, `create_page`, `update_page`, `delete_page` |
| **Attachments** | `list_attachments`, `get_attachment`, `create_attachment`, `update_attachment`, `delete_attachment` |
| **Exports** | `export_page`, `export_chapter`, `export_book` (markdown, plaintext, html) |
| **Comments** | `list_comments`, `get_comment`, `create_comment`, `update_comment`, `delete_comment` |
| **Recycle Bin** | `list_recycle_bin`, `restore_recycle_bin_item`, `destroy_recycle_bin_item` |
| **Users** | `list_users`, `get_user` |
| **Audit Log** | `list_audit_log` |
| **System** | `get_system_info` |
| **Images** | `list_images`, `get_image`, `update_image`, `delete_image` |
| **Permissions** | `get_content_permissions`, `update_content_permissions` |
| **Roles** | `list_roles`, `get_role` |

## Setup

### Prerequisites

- A BookStack instance with API access enabled
- A BookStack API token (created in your BookStack user profile under "API Tokens")

### Configuration

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `BSMCP_BOOKSTACK_URL` | Yes | - | Your BookStack instance URL |
| `BSMCP_HOST` | No | `0.0.0.0` | Bind address |
| `BSMCP_PORT` | No | `8080` | Bind port |
| `BSMCP_INSTANCE_NAME` | No | - | Instance name shown to AI (e.g. "Personal KB") |
| `BSMCP_INSTANCE_DESC` | No | - | Instance description shown to AI |
| `BSMCP_DB_PATH` | No | `/data/bookstack-mcp.db` | SQLite database path for OAuth token persistence |

```bash
cp .env.example .env
# Edit .env with your BookStack URL
```

### Run with Docker Compose

```bash
docker compose up -d
```

### Run from source

```bash
cargo run --release
```

## Authentication

### OAuth 2.1 (Claude Desktop Custom Connector)

The server implements OAuth 2.1 (authorization code + PKCE) with a browser-based login form:

1. Add a custom connector in Claude Desktop with URL: `https://your-host/mcp/sse`
2. When connecting, a login form opens in your browser with instructions
3. Enter your BookStack API **Token ID** and **Token Secret**

No config files needed — authentication happens entirely through the browser.

### Bearer Token (Claude Code / Direct)

For Claude Code or direct SSE connections, authenticate with a Bearer token:

```
Authorization: Bearer <token_id>:<token_secret>
```

The token ID and secret come from your BookStack API token. Each SSE connection creates an isolated session authenticated with that token's permissions.

## MCP Client Configuration

### Claude Desktop

Use the OAuth custom connector method above — no JSON config required.

### Claude Code

Add to your MCP server configuration:

```json
{
  "mcpServers": {
    "bookstack": {
      "url": "https://your-bookstack-mcp-host/mcp/sse",
      "headers": {
        "Authorization": "Bearer YOUR_TOKEN_ID:YOUR_TOKEN_SECRET"
      }
    }
  }
}
```

## API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/mcp/sse` | SSE connection (requires Bearer auth or OAuth) |
| `POST` | `/mcp/messages/?sessionId=<id>` | Send MCP JSON-RPC messages |
| `GET` | `/health` | Health check |
| `GET` | `/.well-known/oauth-authorization-server` | OAuth metadata (RFC 8414) |
| `GET` | `/.well-known/oauth-protected-resource` | Protected resource metadata (RFC 9728) |
| `GET` | `/authorize` | Login form for BookStack API token |
| `POST` | `/authorize` | Validate credentials and issue auth code |
| `POST` | `/token` | OAuth token exchange |
| `POST` | `/register` | Dynamic client registration (RFC 7591) |

## Search Operators

The `search_content` tool supports BookStack's search operators:

- `"exact phrase"` - Exact match
- `{type:page}` - Filter by type (page, chapter, book, shelf)
- `{in_name:term}` - Search within names only
- `{created_by:me}` - Filter by creator
- `[tag_name=value]` - Filter by tag

## License

MIT