# BookStack MCP Server

An MCP (Model Context Protocol) server that gives Claude full access to a [BookStack](https://www.bookstackapp.com/) instance. Built in Rust with tokio/axum for a single static binary with zero runtime dependencies.

## Features

- Full CRUD on all core BookStack resources (shelves, books, chapters, pages, attachments)
- Full-text search with BookStack query operators
- Multi-user support via per-session BookStack API tokens
- SSE transport (MCP protocol version `2024-11-05`)
- ~10MB Docker image (Alpine + static Rust binary)

## Available Tools (26)

| Category | Tools |
|----------|-------|
| **Search** | `search_content` |
| **Shelves** | `list_shelves`, `get_shelf`, `create_shelf`, `update_shelf`, `delete_shelf` |
| **Books** | `list_books`, `get_book`, `create_book`, `update_book`, `delete_book` |
| **Chapters** | `list_chapters`, `get_chapter`, `create_chapter`, `update_chapter`, `delete_chapter` |
| **Pages** | `list_pages`, `get_page`, `create_page`, `update_page`, `delete_page` |
| **Attachments** | `list_attachments`, `get_attachment`, `create_attachment`, `update_attachment`, `delete_attachment` |

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

Clients authenticate via the SSE connection using a Bearer token:

```
Authorization: Bearer <token_id>:<token_secret>
```

The token ID and secret come from your BookStack API token. Each SSE connection creates an isolated session authenticated with that token's permissions.

## MCP Client Configuration

### Claude Desktop / Claude Code

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
| `GET` | `/mcp/sse` | SSE connection (requires Bearer auth) |
| `POST` | `/mcp/messages/?sessionId=<id>` | Send MCP JSON-RPC messages |
| `GET` | `/health` | Health check |

## Search Operators

The `search_content` tool supports BookStack's search operators:

- `"exact phrase"` - Exact match
- `{type:page}` - Filter by type (page, chapter, book, shelf)
- `{in_name:term}` - Search within names only
- `{created_by:me}` - Filter by creator
- `[tag_name=value]` - Filter by tag

## License

MIT
