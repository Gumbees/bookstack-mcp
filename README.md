# BookStack MCP Server

An MCP (Model Context Protocol) server that gives Claude full access to a [BookStack](https://www.bookstackapp.com/) instance. Built in Rust with tokio/axum as a Cargo workspace with pluggable database backends and optional semantic vector search.

## Features

- Full CRUD on all core BookStack resources (shelves, books, chapters, pages, attachments)
- Full-text search with BookStack query operators
- **Semantic vector search** — natural language search across all content via embeddings (optional)
- **Pluggable database** — SQLite for simple deployments, PostgreSQL + pgvector for production
- **Separate embedder** — background embedding service keeps ONNX/model weight out of the MCP server
- **Server-side markdown to HTML conversion** — send markdown, server converts before sending to BookStack
- **OAuth 2.1 support** — use as a Claude.ai or Claude Desktop custom connector without config files
- **Encrypted token storage** — OAuth tokens encrypted at rest with AES-256-GCM
- **Dual transport** — SSE (MCP 2024-11-05) and Streamable HTTP (MCP 2025-03-26)
- **Dynamic structure discovery** — AI automatically learns your BookStack hierarchy on connect
- **Auto-migration** — seamlessly migrate from SQLite to PostgreSQL on startup
- Multi-user support via per-session BookStack API tokens
- Multi-arch Docker images (amd64 + arm64)

## Architecture

```
crates/
  bsmcp-common/       Shared types, traits, config, chunking, vector utils
  bsmcp-db-sqlite/    SQLite backend (rusqlite, bundled)
  bsmcp-db-postgres/  PostgreSQL + pgvector backend (sqlx)
  bsmcp-server/       MCP server binary (axum, no ONNX dependency)
  bsmcp-embedder/     Embedder binary (fastembed + ONNX, job queue worker + HTTP /embed)

docker/
  Dockerfile.server       Lightweight server image (~35MB)
  Dockerfile.embedder     Embedder image with ONNX Runtime (~45MB)
  docker-compose.yml      PostgreSQL deployment (production)
  docker-compose.sqlite.yml  SQLite deployment (simple)
```

The MCP server handles all client-facing protocol, OAuth, and search. The embedder runs separately, polling a database-backed job queue to embed pages and serving a `/embed` HTTP endpoint for query-time embedding. This keeps the ONNX model out of the server process.

## Available Tools (49)

| Category | Tools |
|----------|-------|
| **Search** | `search_content` |
| **Semantic** | `semantic_search`, `reembed`, `embed_status` |
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

Semantic tools (`semantic_search`, `reembed`, `embed_status`) only appear when `BSMCP_SEMANTIC_SEARCH=true` and an embedder is running.

## Setup

### Prerequisites

- A BookStack instance with API access enabled
- A BookStack API token (created in your BookStack user profile under "API Tokens")
- Docker and Docker Compose (for container deployment)

### Quick Start (PostgreSQL — recommended)

```bash
cp .env.example .env
# Edit .env with your BookStack URL, encryption key, and database password

docker compose -f docker/docker-compose.yml up -d
```

This starts three containers:
- **bsmcp-postgres** — PostgreSQL 17 with pgvector extension
- **bsmcp-server** — MCP server (port 8080)
- **bsmcp-embedder** — Background embedding service

### Quick Start (SQLite — simple)

```bash
cp .env.example .env
# Edit .env with your BookStack URL and encryption key

docker compose -f docker/docker-compose.sqlite.yml up -d
```

This starts two containers sharing a SQLite database file.

### Run from source

```bash
# Server
cargo run --release -p bsmcp-server

# Embedder (separate terminal)
cargo run --release -p bsmcp-embedder
```

### Configuration

#### Server Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `BSMCP_BOOKSTACK_URL` | Yes | - | Your BookStack instance URL |
| `BSMCP_ENCRYPTION_KEY` | Yes | - | 32+ char key for AES-256-GCM token encryption |
| `BSMCP_DB_BACKEND` | No | `sqlite` | Database backend: `sqlite` or `postgres` |
| `BSMCP_DATABASE_URL` | If postgres | - | PostgreSQL connection string |
| `BSMCP_DB_PATH` | No | `/data/bookstack-mcp.db` | SQLite database path |
| `BSMCP_PUBLIC_DOMAIN` | No | - | Public domain for OAuth redirects (e.g. `mcp.example.com`) |
| `BSMCP_INTERNAL_DOMAIN` | No | - | Internal/Docker-network domain |
| `BSMCP_HOST` | No | `0.0.0.0` | Bind address |
| `BSMCP_PORT` | No | `8080` | Bind port |
| `BSMCP_INSTANCE_NAME` | No | - | Instance name shown to AI |
| `BSMCP_INSTANCE_DESC` | No | - | Instance description shown to AI |
| `BSMCP_SEMANTIC_SEARCH` | No | `false` | Enable semantic search tools |
| `BSMCP_EMBEDDER_URL` | No | `http://bsmcp-embedder:8081` | Embedder HTTP endpoint |
| `BSMCP_WEBHOOK_SECRET` | If semantic | - | BookStack webhook secret |
| `BSMCP_BACKUP_INTERVAL` | No | - | Hours between backups (0 = disabled) |
| `BSMCP_BACKUP_PATH` | No | `/data/backups` | Backup directory |

#### Embedder Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `BSMCP_EMBED_TOKEN_ID` | Yes | - | BookStack API token ID for crawling |
| `BSMCP_EMBED_TOKEN_SECRET` | Yes | - | BookStack API token secret |
| `BSMCP_EMBED_MODEL` | No | `BAAI/bge-large-en-v1.5` | Embedding model name |
| `BSMCP_MODEL_PATH` | No | `/data/models` | ONNX model cache directory |
| `BSMCP_EMBED_THREADS` | No | `0` (all cores) | Max embedding threads |
| `BSMCP_EMBED_BATCH_SIZE` | No | `32` | Chunks per embedding batch |
| `BSMCP_EMBED_DELAY_MS` | No | `50` | Delay between pages (API throttle) |
| `BSMCP_EMBED_POLL_INTERVAL` | No | `5` | Seconds between job queue polls |
| `BSMCP_EMBED_HOST` | No | `0.0.0.0` | Embedder listen address |
| `BSMCP_EMBED_PORT` | No | `8081` | Embedder listen port |

See `.env.example` for the full list with comments.

### Semantic Search Setup

1. Set `BSMCP_SEMANTIC_SEARCH=true` in your server env
2. Set `BSMCP_WEBHOOK_SECRET` to a random string
3. Create a BookStack API token with read access for the embedder (`BSMCP_EMBED_TOKEN_ID` / `BSMCP_EMBED_TOKEN_SECRET`)
4. Configure a webhook in BookStack: **Settings > Webhooks > Add Webhook**
   - URL: `https://your-mcp-host/webhooks/bookstack?secret=YOUR_WEBHOOK_SECRET`
   - Events: Page Create, Page Update, Page Delete
5. Use the `reembed` tool to trigger initial embedding of all pages
6. After initial embedding, page changes are automatically re-embedded via webhooks

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
| `GET` | `/mcp/sse` | SSE connection (MCP 2024-11-05) |
| `POST` | `/mcp/sse` | Streamable HTTP (MCP 2025-03-26) |
| `POST` | `/mcp/messages/?sessionId=<id>` | Send MCP JSON-RPC messages (SSE transport) |
| `GET` | `/health` | Health check |
| `POST` | `/webhooks/bookstack?secret=<s>` | BookStack webhook receiver (semantic search) |
| `GET` | `/.well-known/oauth-authorization-server` | OAuth metadata (RFC 8414) |
| `GET` | `/.well-known/oauth-protected-resource` | Protected resource metadata (RFC 9728) |
| `GET` | `/authorize` | Login form for BookStack API token |
| `POST` | `/authorize` | Validate credentials and issue auth code |
| `POST` | `/token` | OAuth token exchange |
| `POST` | `/register` | Dynamic client registration (RFC 7591) |

## Upgrading

### From v0.2.x to v0.3.0

v0.3.0 is a major architecture change: the monolithic server is split into a Cargo workspace with separate server and embedder binaries, pluggable database backends (SQLite or PostgreSQL), and a database-backed job queue.

#### What changed

- **Two container images** instead of one: `ghcr.io/gumbees/bsmcp-server` and `ghcr.io/gumbees/bsmcp-embedder`
- **Pluggable database**: SQLite (default, same as before) or PostgreSQL + pgvector
- **Separate embedder**: The ONNX model and embedding pipeline run in their own container, keeping the MCP server lightweight
- **Database-backed job queue**: Embedding jobs survive restarts; PostgreSQL supports concurrent embedders via `FOR UPDATE SKIP LOCKED`
- **Docker service names**: `postgres` renamed to `bsmcp-postgres`, `bookstack-mcp` renamed to `bsmcp-server`, volume `pgdata` renamed to `bsmcp-pgdata`
- **New env vars**: `BSMCP_DB_BACKEND`, `BSMCP_DATABASE_URL`, `BSMCP_EMBEDDER_URL`, `BSMCP_EMBED_TOKEN_ID`, `BSMCP_EMBED_TOKEN_SECRET`, plus embedder performance tuning vars

#### Upgrade path: staying on SQLite

If you're happy with SQLite, the upgrade is minimal:

1. Pull the new images
2. Replace your `docker-compose.yml` with `docker/docker-compose.sqlite.yml`
3. Add embedder env vars (`BSMCP_EMBED_TOKEN_ID`, `BSMCP_EMBED_TOKEN_SECRET`) — create a new BookStack API token with read access
4. Restart: `docker compose up -d`

Your existing SQLite database and embeddings are preserved. The server reads the same `bsmcp-data` volume.

#### Upgrade path: migrating to PostgreSQL

For better performance and concurrent embedding:

1. Pull the new images
2. Replace your `docker-compose.yml` with `docker/docker-compose.yml` (the PostgreSQL version)
3. Add new env vars: `BSMCP_DB_BACKEND=postgres`, `BSMCP_DATABASE_URL`, `BSMCP_DB_PASSWORD`, plus embedder vars
4. Start the stack: `docker compose up -d`

**Auto-migration:** If the server detects an existing SQLite database at `BSMCP_DB_PATH` when running with `BSMCP_DB_BACKEND=postgres`, it automatically migrates all data (access tokens, pages, chunks, embeddings, relationships, jobs) to PostgreSQL. After successful migration, the SQLite file is renamed to `.db.migrated` to prevent re-migration.

**Manual migration:** You can also migrate explicitly:

```bash
docker exec bsmcp-server bsmcp-server migrate \
  --from-sqlite /data/bookstack-mcp.db \
  --to-postgres postgres://bsmcp:yourpassword@bsmcp-postgres/bsmcp
```

**Migration details:**
- Encrypted access tokens are copied as-is (portable when `BSMCP_ENCRYPTION_KEY` matches)
- Chunk embeddings are converted from SQLite BLOB (LE f32 bytes) to pgvector `vector(1024)`
- Row counts are validated after migration
- All active OAuth sessions are preserved — connected clients don't need to re-authenticate

**Lightweight alternative:** If re-embedding is acceptable, you can skip migration entirely. Start fresh with PostgreSQL and use the `reembed` tool to re-embed all pages. Only `access_tokens` matter for session continuity.

#### Upgrade path: Docker volume migration

If you're coming from v0.2.x with a `pgdata` volume and want to keep your PostgreSQL data:

```bash
docker compose down
docker volume create bsmcp-pgdata
docker run --rm \
  -v pgdata:/source:ro \
  -v bsmcp-pgdata:/dest \
  alpine cp -a /source/. /dest/
docker compose up -d
# Once verified, remove old volume:
docker volume rm pgdata
```

### From v0.1.2 to v0.1.3

See the [v0.1.3 upgrade notes](https://github.com/gumbees/bookstack-mcp/releases/tag/v0.1.3) for details on:
- New required `BSMCP_ENCRYPTION_KEY` env var
- Renamed `BSMCP_PUBLIC_URL` to `BSMCP_PUBLIC_DOMAIN`
- Docker volume renamed `mcp-data` to `bsmcp-data`
- PKCE enforcement for OAuth

## Search Operators

The `search_content` tool supports BookStack's search operators:

- `"exact phrase"` - Exact match
- `{type:page}` - Filter by type (page, chapter, book, shelf)
- `{in_name:term}` - Search within names only
- `{created_by:me}` - Filter by creator
- `[tag_name=value]` - Filter by tag

## License

MIT
