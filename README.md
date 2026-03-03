# BookStack MCP Server

An MCP (Model Context Protocol) server that gives Claude full access to a [BookStack](https://www.bookstackapp.com/) instance. Built in Rust with tokio/axum for a single static binary with zero runtime dependencies.

## Features

- Full CRUD on all core BookStack resources (shelves, books, chapters, pages, attachments)
- Full-text search with BookStack query operators
- **Server-side markdown→HTML conversion** — send markdown content, server converts to HTML before sending to BookStack (avoids JSON escaping issues with complex markdown)
- **OAuth 2.1 support** — use as a Claude.ai or Claude Desktop custom connector without config files
- **Encrypted token storage** — OAuth tokens encrypted at rest with AES-256-GCM
- **Dual transport** — SSE (MCP 2024-11-05) and Streamable HTTP (MCP 2025-03-26)
- **Dynamic structure discovery** — AI automatically learns your BookStack hierarchy on connect
- Multi-user support via per-session BookStack API tokens
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
| `BSMCP_ENCRYPTION_KEY` | Yes | - | 32+ char key for AES-256-GCM encryption of OAuth tokens at rest |
| `BSMCP_HOST` | No | `0.0.0.0` | Bind address |
| `BSMCP_PORT` | No | `8080` | Bind port |
| `BSMCP_INSTANCE_NAME` | No | - | Instance name shown to AI (e.g. "Personal KB") |
| `BSMCP_INSTANCE_DESC` | No | - | Instance description shown to AI |
| `BSMCP_PUBLIC_DOMAIN` | No | - | Public domain this server is reachable at (e.g. `mcp.example.com`). Derives `https://{domain}` for OAuth redirects |
| `BSMCP_INTERNAL_DOMAIN` | No | - | Internal/Docker-network domain (e.g. `bookstack-mcp`). Derives `http://{domain}` for host verification |
| `BSMCP_DB_PATH` | No | `/data/bookstack-mcp.db` | SQLite database path for OAuth token persistence |
| `BSMCP_BACKUP_INTERVAL` | No | - | Hours between SQLite backups (integer). If unset, backups disabled |
| `BSMCP_BACKUP_PATH` | No | `/data/backups` | Directory for backup files |

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

## Connecting

The MCP endpoint URL is:

```
https://your-host/mcp/sse
```

> **Important:** Use the full path including `/mcp/sse` — not just the base domain.

### Claude.ai (Custom Connector)

1. Go to **Settings > Integrations > Add custom MCP** in Claude.ai
2. Enter the MCP endpoint URL: `https://your-host/mcp/sse`
3. A login form opens in your browser — enter your BookStack API **Token ID** and **Token Secret**
4. Once authorized, BookStack tools appear automatically in your conversations

### Claude Desktop (Custom Connector)

1. Add a custom connector with URL: `https://your-host/mcp/sse`
2. When connecting, a login form opens in your browser with instructions
3. Enter your BookStack API **Token ID** and **Token Secret**

No config files needed — authentication happens entirely through the browser via OAuth 2.1.

### Claude Code (Direct Bearer Token)

Add to your MCP server configuration:

```json
{
  "mcpServers": {
    "bookstack": {
      "url": "https://your-host/mcp/sse",
      "headers": {
        "Authorization": "Bearer YOUR_TOKEN_ID:YOUR_TOKEN_SECRET"
      }
    }
  }
}
```

The token ID and secret come from your BookStack API token (created under **My Account > Access & Security > API Tokens**).

## API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/mcp/sse` | SSE connection (MCP 2024-11-05, requires Bearer auth or OAuth) |
| `POST` | `/mcp/sse` | Streamable HTTP (MCP 2025-03-26, requires Bearer auth or OAuth) |
| `POST` | `/mcp/messages/?sessionId=<id>` | Send MCP JSON-RPC messages (SSE transport) |
| `GET` | `/health` | Health check |
| `GET` | `/.well-known/oauth-authorization-server` | OAuth metadata (RFC 8414) |
| `GET` | `/.well-known/oauth-protected-resource` | Protected resource metadata (RFC 9728) |
| `GET` | `/authorize` | Login form for BookStack API token |
| `POST` | `/authorize` | Validate credentials and issue auth code |
| `POST` | `/token` | OAuth token exchange |
| `POST` | `/register` | Dynamic client registration (RFC 7591) |

## Upgrading to v0.1.3

v0.1.3 introduces encrypted token storage, PKCE enforcement, and server-side markdown conversion. There are several breaking changes that require action before upgrading.

### 1. New required env var: `BSMCP_ENCRYPTION_KEY`

OAuth tokens are now encrypted at rest with AES-256-GCM. You must set a 32+ character encryption key before starting the new version. Without it, the server will refuse to start.

```bash
# Add to your .env
BSMCP_ENCRYPTION_KEY=your-secret-key-at-least-32-characters-long
```

Generate a key: `openssl rand -base64 48`

Existing plaintext tokens in the database are automatically encrypted on first access (transparent migration). If you change the encryption key later, all previously stored OAuth tokens become invalid and users must re-authenticate.

### 2. Renamed env var: `BSMCP_PUBLIC_URL` → `BSMCP_PUBLIC_DOMAIN`

The `BSMCP_PUBLIC_URL` variable (which took a full URL like `https://mcp.example.com`) has been replaced by `BSMCP_PUBLIC_DOMAIN` (just the domain — the server prepends `https://`).

```bash
# Old (no longer recognized)
BSMCP_PUBLIC_URL=https://mcp.example.com

# New
BSMCP_PUBLIC_DOMAIN=mcp.example.com
```

If you had `BSMCP_PUBLIC_URL` set, **remove it** and add `BSMCP_PUBLIC_DOMAIN` instead. The old variable is silently ignored.

You can also now set `BSMCP_INTERNAL_DOMAIN` for Docker-network host matching (derives `http://{domain}`).

### 3. Docker volume renamed: `mcp-data` → `bsmcp-data`

The docker-compose volume was renamed. If you pull the new compose file and run `docker compose up`, Docker will create a new empty volume and your existing SQLite database (OAuth tokens) will be orphaned in the old volume.

**To preserve your data:**

```bash
# Stop the running container
docker compose down

# Copy data from old volume to new
docker volume create bsmcp-data
docker run --rm \
  -v mcp-data:/source:ro \
  -v bsmcp-data:/dest \
  alpine cp -a /source/. /dest/

# Start with new compose
docker compose up -d

# Once verified working, remove old volume
docker volume rm mcp-data
```

### 4. PKCE now required for OAuth

The OAuth authorization endpoint now enforces PKCE (S256). All current Claude clients (Claude.ai, Claude Desktop, Claude Code) support PKCE, so this should be transparent. Custom OAuth clients that don't send `code_challenge` will receive a `400 invalid_request` error.

## Search Operators

The `search_content` tool supports BookStack's search operators:

- `"exact phrase"` - Exact match
- `{type:page}` - Filter by type (page, chapter, book, shelf)
- `{in_name:term}` - Search within names only
- `{created_by:me}` - Filter by creator
- `[tag_name=value]` - Filter by tag

## License

MIT